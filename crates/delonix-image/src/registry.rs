//! Pull from an OCI registry (Docker Registry HTTP API V2).
//!
//! Supports Docker Hub by default (with an anonymous token) and any public
//! registry that uses the V2 protocol (ghcr.io, quay.io, registry.k8s.io, ...).
//! The flow: resolves the reference → manifest (picks the platform if it is a
//! multi-arch index) → config blob → layer blobs → stores in the CAS, just
//! like `load_docker_archive`.

use crate::cas::sha256_hex;
use crate::image::{now_unix, Image, ImageConfig, ImageStore};
use delonix_runtime_core::{Error, Result};
// Canonical OCI types (crate `oci-spec`, feature `image`) — replace the hand-rolled
// structs of the OCI/distribution schema that used to be here (C3-IMG).
use oci_spec::image::{
    Descriptor, DescriptorBuilder, Digest, ImageConfiguration, ImageIndex, ImageManifest,
    ImageManifestBuilder, MediaType,
};
use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

/// Converts an `oci-spec` error (construction/validation of OCI types) into an
/// [`Error::Registry`], so as not to leak the external crate's error type.
fn oci_err(e: impl std::fmt::Display) -> Error {
    Error::Registry(format!("oci-spec: {e}"))
}

/// Media types accepted when requesting a manifest (index OR image manifest).
const ACCEPT_MANIFEST: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";

fn reg_err(e: reqwest::Error) -> Error {
    Error::Registry(e.to_string())
}

/// Splits the reference into (API host, repository, tag/digest), applying
/// Docker's rules: default registry `registry-1.docker.io`, official
/// images under `library/`.
/// Splits an image reference into `(registry_host, repository, reference)` — where
/// `reference` is the digest if present (it rules resolution), otherwise the tag.
/// Pure and total (never panics, never errors — malformed input just yields the
/// best-effort split). `pub` so the robustness (proptest) test and the criterion
/// bench can reach it; a workspace-internal crate with no external consumers, so
/// widening this is free.
pub fn parse_reference(input: &str) -> (String, String, String) {
    // tag (`:`) or digest (`@`) — the `:` must be AFTER the last `/`.
    let (name, reference) = if let Some(idx) = input.find('@') {
        // `repo:tag@digest` (combined format, valid in Docker/OCI — the digest
        // rules the resolution, the tag is only informative) — cut the tag BEFORE the
        // `@`, otherwise `name` keeps the tag inside it (`repo:tag`) and the
        // manifest URL comes out malformed. Found when testing `kindest/node:vX@sha256:…`.
        let before = &input[..idx];
        let last_slash = before.rfind('/').map(|i| i + 1).unwrap_or(0);
        let name = match before[last_slash..].find(':') {
            Some(colon) => &before[..last_slash + colon],
            None => before,
        };
        (name, input[idx + 1..].to_string())
    } else {
        let last_slash = input.rfind('/').map(|i| i + 1).unwrap_or(0);
        match input[last_slash..].find(':') {
            Some(colon) => {
                let abs = last_slash + colon;
                (&input[..abs], input[abs + 1..].to_string())
            }
            None => (input, "latest".to_string()),
        }
    };

    let mut host = "registry-1.docker.io".to_string();
    let mut repo = name.to_string();
    if let Some(slash) = name.find('/') {
        let first = &name[..slash];
        if first.contains('.') || first.contains(':') || first == "localhost" {
            host = first.to_string();
            repo = name[slash + 1..].to_string();
        }
    }
    // `docker.io`/`index.docker.io` → the real V2 API host.
    if host == "docker.io" || host == "index.docker.io" {
        host = "registry-1.docker.io".to_string();
    }
    // Docker Hub: single-component official image → `library/` prefix.
    if host == "registry-1.docker.io" && !repo.contains('/') {
        repo = format!("library/{repo}");
    }
    (host, repo, reference)
}

/// Extracts `key="value"` from a `WWW-Authenticate` header.
fn extract(header: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let start = header.find(&pat)? + pat.len();
    let rest = &header[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The HTTP scheme for a registry: `http` for local/insecure registries
/// (`localhost`, `127.0.0.1`, `[::1]`), `https` for all others — the same
/// rule as Docker/containerd for insecure registries by default.
fn scheme_for(host: &str) -> &'static str {
    let h = host.split(':').next().unwrap_or(host);
    if h == "localhost" || h == "127.0.0.1" || h == "::1" || h == "[::1]" {
        "http"
    } else {
        "https"
    }
}

/// Start offset and whole size declared by a `Content-Range` response header
/// (`bytes <start>-<end>/<total>`, RFC 9110); the size is `None` for the `*`
/// form, which a server may send when it will resume but will not commit to a
/// total.
///
/// Pure, and separate, because it is the check that makes stitching two ranges
/// safe to attempt at all: appending the body of a 206 that starts somewhere
/// OTHER than where we stopped produces a blob that is corrupt in a way only
/// the final digest would catch — after the whole download has been paid for.
fn parse_content_range(v: &str) -> Option<(u64, Option<u64>)> {
    let (range, total) = v
        .trim()
        .strip_prefix("bytes")?
        .trim_start()
        .split_once('/')?;
    let start = range.split('-').next()?.trim().parse().ok()?;
    let total = total.trim();
    Some((start, (total != "*").then(|| total.parse().ok()).flatten()))
}

/// The target architecture in OCI vocabulary (`amd64`, `arm64`, ...).
fn target_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        other => other,
    }
}

#[derive(Clone)]
struct Client {
    http: reqwest::blocking::Client,
    host: String,
    repo: String,
    token: Option<String>,
    /// Credentials (`delonix login`), if any, for private registries.
    creds: Option<(String, String)>,
}

impl Client {
    fn send_once(
        &self,
        url: &str,
        accept: &str,
        from: Option<u64>,
    ) -> reqwest::Result<reqwest::blocking::Response> {
        let mut req = self.http.get(url).header(reqwest::header::ACCEPT, accept);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(off) = from {
            req = req.header(reqwest::header::RANGE, format!("bytes={off}-"));
        }
        req.send()
    }

    /// GET with Bearer authentication; on 401, obtains a token and retries (once).
    fn fetch(&mut self, url: &str, accept: &str) -> Result<reqwest::blocking::Response> {
        self.fetch_range(url, accept, None)
    }

    /// [`Self::fetch`] with an optional `Range: bytes=<from>-` — the resume of
    /// a partial blob (see [`Self::blob_with_progress_capped`]). Going through
    /// the same 401→token→retry path matters here and is not incidental: a
    /// registry token is short-lived (ghcr's is minutes), so on a slow link the
    /// token that opened the transfer can well be expired by the time an
    /// interrupted download is resumed.
    fn fetch_range(
        &mut self,
        url: &str,
        accept: &str,
        from: Option<u64>,
    ) -> Result<reqwest::blocking::Response> {
        let resp = self.send_once(url, accept, from).map_err(reg_err)?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let www = resp
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            self.token = Some(self.get_token(&www, None)?);
            let resp = self.send_once(url, accept, from).map_err(reg_err)?;
            return self.check(resp, url);
        }
        self.check(resp, url)
    }

    fn check(
        &self,
        resp: reqwest::blocking::Response,
        url: &str,
    ) -> Result<reqwest::blocking::Response> {
        let status = resp.status();
        if status.is_success() {
            Ok(resp)
        } else if status == reqwest::StatusCode::NOT_FOUND {
            // The tag-listing endpoint ends in `/tags/list`, so the old code —
            // which took the last path segment as the tag — reported
            // `no such image <repo>:list`, naming a tag that does not exist and
            // never did. Say what was actually not found.
            //
            // And say the other half: a registry answers 404 for a repository
            // that exists but is PRIVATE to these credentials, because telling
            // the two apart would leak which private repositories exist. A
            // reader who has just pushed there needs to know that "not found"
            // may mean "not visible to you".
            if url.ends_with("/tags/list") {
                Err(Error::NotFound(format!(
                    "repository {} — it does not exist, or it is private and these \
                     credentials cannot see it (`delonix image login <registry>`)",
                    self.repo
                )))
            } else {
                Err(Error::NotFound(format!(
                    "image {}:{}",
                    self.repo,
                    url.rsplit('/').next().unwrap_or("")
                )))
            }
        } else {
            Err(Error::Registry(format!("HTTP {status} at {url}")))
        }
    }

    /// Requests a token from the authentication service indicated in the 401. With
    /// `force_scope`, requests that scope (e.g. `…:pull,push` for the `push`) instead
    /// of the one indicated by the server — the server grants it if the credentials
    /// allow it.
    fn get_token(&self, www: &str, force_scope: Option<&str>) -> Result<String> {
        let realm = extract(www, "realm")
            .ok_or_else(|| Error::Registry("authentication without `realm`".into()))?;
        let scope = match force_scope {
            Some(s) => s.to_string(),
            None => {
                extract(www, "scope").unwrap_or_else(|| format!("repository:{}:pull", self.repo))
            }
        };
        let mut url = format!("{realm}?scope={scope}");
        if let Some(service) = extract(www, "service") {
            url.push_str(&format!("&service={service}"));
        }
        let mut req = self.http.get(&url);
        // Private registry: authenticate the token request with Basic (user:password).
        if let Some((u, p)) = &self.creds {
            req = req.basic_auth(u, Some(p));
        }
        let resp = req.send().map_err(reg_err)?;
        if !resp.status().is_success() {
            return Err(Error::Registry(format!(
                "failed to obtain token: HTTP {}",
                resp.status()
            )));
        }
        let v: serde_json::Value = resp.json().map_err(reg_err)?;
        v.get("token")
            .or_else(|| v.get("access_token"))
            .and_then(|t| t.as_str())
            .map(String::from)
            .ok_or_else(|| Error::Registry("authentication response without token".into()))
    }

    fn manifest_url(&self, reference: &str) -> String {
        format!(
            "{}://{}/v2/{}/manifests/{}",
            scheme_for(&self.host),
            self.host,
            self.repo,
            reference
        )
    }

    fn tags_url(&self) -> String {
        format!(
            "{}://{}/v2/{}/tags/list",
            scheme_for(&self.host),
            self.host,
            self.repo
        )
    }

    /// The repo's tags, per the Registry v2 `GET /v2/<name>/tags/list`
    /// endpoint (`{"name": ..., "tags": [...]}`). Reuses `fetch`'s normal
    /// 401→token→retry flow, so this works against ghcr.io/Docker Hub/any
    /// other v2 registry the same way pull/push already do. Single request,
    /// no `Link`-header pagination — fine for the handful of tags a golden
    /// VM image repo realistically has; a repo with hundreds of tags would
    /// only see the registry's first page.
    fn list_tags(&mut self) -> Result<Vec<String>> {
        let url = self.tags_url();
        let resp = self.fetch(&url, "application/json")?;
        let v: serde_json::Value = resp.json().map_err(reg_err)?;
        Ok(v.get("tags")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn blob(&mut self, digest: &str) -> Result<Vec<u8>> {
        self.blob_with_progress(digest, None)
    }

    /// Hard ceiling on a single blob (container layer or VM artifact). Golden
    /// VM images run to the low single-digit GiB (AGENTS.md: "layers de
    /// várias centenas de MB a multiple GB" for `kindest/node`); this is
    /// generous headroom above that, not a tight budget.
    const MAX_BLOB_BYTES: u64 = 8 * 1024 * 1024 * 1024;

    fn blob_with_progress(
        &mut self,
        digest: &str,
        progress: Option<&dyn Fn(u64, Option<u64>)>,
    ) -> Result<Vec<u8>> {
        self.blob_with_progress_capped(digest, progress, Self::MAX_BLOB_BYTES)
    }

    /// Downloads a blob in STREAMING, calling `progress(bytes_read, total)` as
    /// it advances — the total comes from `Content-Length` (may be missing in
    /// chunked responses, hence the `Option`). Reading in chunks instead of `.bytes()`
    /// (which loads everything before returning) is what enables a progress
    /// bar: a VM artifact is hundreds of MB and without this the `pull` looks
    /// hung. The engine crate only REPORTS the bytes; the drawing is the bin's job.
    ///
    /// `max_bytes` is a parameter (not always [`Self::MAX_BLOB_BYTES`]) so
    /// tests can exercise the over-limit abort path with a tiny fake cap
    /// instead of actually streaming gigabytes of data.
    ///
    /// BUG FOUND: `Vec::with_capacity(total.unwrap_or(0))` used to trust the
    /// registry's raw, UNTRUSTED `Content-Length` outright — a hostile or
    /// MITM'd registry returning a huge value (e.g. near u64::MAX) makes the
    /// allocator attempt a giant reservation, which ABORTS the whole process
    /// (not a recoverable error). Independently, nothing capped the actual
    /// read loop either, so a server that just kept streaming (with or
    /// without a lying Content-Length — a chunked response may not even
    /// send one) grew `buf` until the machine OOMed. Both are fixed here:
    /// the up-front reservation is capped regardless of the claimed total,
    /// and the loop aborts once the ACTUAL bytes read exceed the limit.
    /// How many times a single blob may be (re)opened. Same budget as
    /// `stream_download`'s, for the same reason: enough to ride out a link that
    /// drops a connection every few minutes, not so many that a genuinely
    /// broken transfer takes all afternoon to say so.
    const BLOB_ATTEMPTS: u32 = 5;

    fn blob_with_progress_capped(
        &mut self,
        digest: &str,
        progress: Option<&dyn Fn(u64, Option<u64>)>,
        max_bytes: u64,
    ) -> Result<Vec<u8>> {
        use std::io::Read;
        let url = format!(
            "{}://{}/v2/{}/blobs/{}",
            scheme_for(&self.host),
            self.host,
            self.repo,
            digest
        );
        // Kept ACROSS attempts: this is what "resume" means here. A blob is
        // buffered whole in memory (a property of this function since it was
        // written), so what a retry continues from is this vector, not a file.
        let mut buf: Vec<u8> = Vec::new();
        // Size of the WHOLE blob. Not `content_length()` on a resumed request:
        // a 206's Content-Length is the length of the FRAGMENT, so taking it
        // would make the progress bar restart against a shrinking total. On a
        // resume the whole size comes from the `/<total>` of Content-Range.
        let mut total: Option<u64> = None;
        let mut last_err = String::new();

        for attempt in 1..=Self::BLOB_ATTEMPTS {
            let from = (!buf.is_empty()).then_some(buf.len() as u64);
            if attempt > 1 {
                // Backoff, and a line saying what is happening: without it a
                // resumed pull on a slow link is indistinguishable from a hang,
                // which is the complaint that started this.
                std::thread::sleep(Duration::from_secs(1 << (attempt - 2).min(3)));
                tracing::warn!(
                    have = buf.len(),
                    attempt,
                    "resuming blob {digest} after: {last_err}"
                );
            }
            let mut resp = match self.fetch_range(&url, "*/*", from) {
                Ok(r) => r,
                // Retry a failed OPEN only while resuming. Getting here with
                // bytes in hand means the URL and the token were good moments
                // ago, so this is transport. On the FIRST request the same
                // error is far more likely to be a 403/404/no-such-repo, and
                // retrying those just delays an answer the caller already has.
                Err(e) => {
                    if from.is_none() || matches!(e, Error::NotFound(_)) {
                        return Err(e);
                    }
                    last_err = e.to_string();
                    continue;
                }
            };

            // Did the server actually honour the range? Three ways it may not,
            // and only the first is a resume: 206 at the offset asked for; 206
            // at a DIFFERENT offset (it answered another question — appending
            // would silently corrupt the blob, caught only by the digest at the
            // very end, after the whole download); or 200, meaning it ignored
            // the header and is sending the whole thing again.
            let mut restart = true;
            if let Some(off) = from {
                if resp.status().as_u16() == 206 {
                    let cr = resp
                        .headers()
                        .get(reqwest::header::CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_content_range);
                    if let Some((start, whole)) = cr {
                        if start == off {
                            restart = false;
                            if whole.is_some() {
                                total = whole;
                            }
                        }
                    }
                }
                if restart {
                    tracing::warn!(
                        "registry ignored the Range request for {digest} — restarting from zero"
                    );
                }
            }
            if restart {
                buf.clear();
                total = resp.content_length();
                if let Some(t) = total {
                    buf.reserve(t.min(max_bytes) as usize);
                }
            }

            let mut chunk = [0u8; 65536];
            let mut broke = false;
            loop {
                let n = match resp.read(&mut chunk) {
                    // Whatever is in `buf` stays: the next attempt continues
                    // from there instead of throwing away minutes of transfer.
                    Err(e) => {
                        last_err = format!("blob read: {e}");
                        broke = true;
                        break;
                    }
                    Ok(n) => n,
                };
                if n == 0 {
                    break;
                }
                if buf.len() as u64 + n as u64 > max_bytes {
                    return Err(Error::Registry(format!(
                        "blob {digest} exceeds the {max_bytes}-byte limit — aborted"
                    )));
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(p) = progress {
                    p(buf.len() as u64, total);
                }
            }
            if broke {
                continue;
            }
            // A clean EOF short of the announced size is a cut connection that
            // did not bother to error — resume it too. Without this the blob
            // came back truncated and only the caller's digest check noticed,
            // reporting corruption for what was really a dropped transfer.
            if let Some(t) = total {
                if (buf.len() as u64) < t {
                    last_err = format!("connection closed at {} of {t} bytes", buf.len());
                    continue;
                }
            }
            return Ok(buf);
        }

        Err(Error::Registry(format!(
            "blob {digest}: gave up after {} attempts with {} of {} bytes — last error: {last_err}",
            Self::BLOB_ATTEMPTS,
            buf.len(),
            total.map(|t| t.to_string()).unwrap_or_else(|| "?".into()),
        )))
    }

    // ---- push (write): blobs + manifest -------------------------------------

    /// Executes a write request; on 401, obtains a token with scope
    /// `pull,push` and retries (once). `build` is called on each attempt (the
    /// body is rebuilt), so it is safe to retry.
    fn write_req(
        &mut self,
        build: &dyn Fn(&reqwest::blocking::Client) -> reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::Response> {
        let send = |http: &reqwest::blocking::Client, token: &Option<String>| {
            let mut req = build(http);
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            req.send()
        };
        let resp = send(&self.http, &self.token).map_err(reg_err)?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let www = resp
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let scope = format!("repository:{}:pull,push", self.repo);
            self.token = Some(self.get_token(&www, Some(&scope))?);
            let resp = send(&self.http, &self.token).map_err(reg_err)?;
            return Ok(resp);
        }
        Ok(resp)
    }

    /// `true` if the blob already exists in the registry (avoids resending it — remote dedup).
    fn blob_exists(&mut self, digest: &str) -> Result<bool> {
        let url = format!(
            "{}://{}/v2/{}/blobs/{}",
            scheme_for(&self.host),
            self.host,
            self.repo,
            digest
        );
        let resp = self.write_req(&|http| http.head(&url))?;
        Ok(resp.status().is_success())
    }

    /// Sends a blob (config or layer) via a monolithic upload: `POST` to open
    /// the session, then `PUT …?digest=<sha256>` with the content.
    fn push_blob(&mut self, digest: &str, data: &[u8]) -> Result<()> {
        if self.blob_exists(digest)? {
            return Ok(());
        }
        let start = format!(
            "{}://{}/v2/{}/blobs/uploads/",
            scheme_for(&self.host),
            self.host,
            self.repo
        );
        let resp = self.write_req(&|http| http.post(&start))?;
        if resp.status() != reqwest::StatusCode::ACCEPTED {
            return Err(Error::Registry(format!(
                "upload start: HTTP {} (run `delonix login {}`?)",
                resp.status(),
                self.host
            )));
        }
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::Registry("upload without Location header".into()))?
            .to_string();
        // Location may come absolute or relative to the host.
        let base = if location.starts_with("http") {
            location
        } else {
            format!("{}://{}{}", scheme_for(&self.host), self.host, location)
        };
        let sep = if base.contains('?') { '&' } else { '?' };
        let put_url = format!("{base}{sep}digest={digest}");
        let body = data.to_vec();
        let resp = self.write_req(&|http| {
            http.put(&put_url)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(body.clone())
        })?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(Error::Registry(format!("blob PUT {digest}: HTTP {status}")));
        }
        Ok(())
    }

    /// Publishes the manifest under the given tag/digest.
    fn push_manifest(&mut self, reference: &str, body: &[u8], media_type: &str) -> Result<()> {
        let url = self.manifest_url(reference);
        let payload = body.to_vec();
        let resp = self.write_req(&|http| {
            http.put(&url)
                .header(reqwest::header::CONTENT_TYPE, media_type)
                .body(payload.clone())
        })?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().unwrap_or_default();
            let detail = detail.chars().take(200).collect::<String>();
            return Err(Error::Registry(format!(
                "manifest PUT: HTTP {status} {detail}"
            )));
        }
        Ok(())
    }
}

/// Ensures the `sha256:` prefix on a digest.
fn with_prefix(digest: &str) -> String {
    if digest.starts_with("sha256:") {
        digest.to_string()
    } else {
        format!("sha256:{digest}")
    }
}

/// Docker schema-2 media types (kept to match byte-for-byte what
/// `docker`/registries expect; in `oci_spec` they become `MediaType::Other(...)`).
const DOCKER_CONFIG_MEDIA_TYPE: &str = "application/vnd.docker.container.image.v1+json";
pub(crate) const DOCKER_MANIFEST_MEDIA_TYPE: &str =
    "application/vnd.docker.distribution.manifest.v2+json";

/// Builds an OCI [`Descriptor`] (`oci_spec`) from a mediaType, size
/// and digest (with or without the `sha256:` prefix). Centralises the digest
/// validation (`Digest::from_str`) and the construction via the builder.
fn descriptor(media_type: &str, size: usize, digest: &str) -> Result<Descriptor> {
    DescriptorBuilder::default()
        .media_type(media_type)
        .size(size as u64)
        .digest(Digest::from_str(&with_prefix(digest)).map_err(oci_err)?)
        .build()
        .map_err(oci_err)
}

/// The mediaType of a layer by its *magic number* (gzip/zstd/plain tar).
fn layer_media_type(data: &[u8]) -> &'static str {
    if data.starts_with(&[0x1f, 0x8b]) {
        "application/vnd.docker.image.rootfs.diff.tar.gzip"
    } else if data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        "application/vnd.oci.image.layer.v1.tar+zstd"
    } else {
        "application/vnd.oci.image.layer.v1.tar"
    }
}

/// Reusable registry client (public facade) — used by signature
/// verification (B8) to fetch manifests and blobs with the same auth as the pull.
pub struct RegistryClient {
    inner: Client,
    reference: String,
}

/// Builds a [`RegistryClient`] for `reference` (reuses credentials and auth).
/// HTTP client for transfers whose SIZE IS NOT KNOWN IN ADVANCE — image
/// layers and VM artifacts.
///
/// `reqwest`'s `timeout()` bounds the WHOLE request, body included, so a fixed
/// value is really a bandwidth requirement in disguise: at 600s, anything that
/// cannot move in ten minutes fails no matter how healthy the connection is.
/// Measured here, publishing VM images over a ~1.3 MB/s link: 646 MiB
/// succeeded, and 1.06, 1.22 and 1.45 GiB all failed — the artifact was fine,
/// the clock ran out.
///
/// What we want to catch is a connection that never opens, not one that is
/// merely long. `reqwest`'s BLOCKING builder has no `read_timeout` (that is
/// async-only in this version), so the pair available is a short
/// `connect_timeout` plus a total ceiling generous enough that it only ever
/// fires on something genuinely stuck: four hours covers the 8 GiB blob cap
/// even on a link slower than the one measured above.
fn transfer_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(4 * 60 * 60))
        .build()
        .map_err(reg_err)
}

pub fn registry_client(store: &ImageStore, reference: &str) -> Result<RegistryClient> {
    let (host, repo, refr) = parse_reference(reference);
    let http = transfer_client()?;
    let creds = crate::auth::lookup(store.root(), &host);
    Ok(RegistryClient {
        inner: Client {
            http,
            host,
            repo,
            token: None,
            creds,
        },
        reference: refr,
    })
}

impl RegistryClient {
    /// The tag/digest with which the client was created.
    pub fn reference(&self) -> String {
        self.reference.clone()
    }
    /// All tags of this client's repository (ignores the tag/digest it was
    /// built with — only `host`/`repo` matter here). See `Client::list_tags`.
    pub fn list_tags(&mut self) -> Result<Vec<String>> {
        self.inner.list_tags()
    }
    /// Raw bytes of a manifest (by tag or digest).
    pub fn get_manifest(&mut self, refr: &str) -> Result<Vec<u8>> {
        let url = self.inner.manifest_url(refr);
        let resp = self.inner.fetch(&url, ACCEPT_MANIFEST)?;
        Ok(resp.bytes().map_err(reg_err)?.to_vec())
    }
    /// Raw bytes of a blob (by digest).
    pub fn get_blob(&mut self, digest: &str) -> Result<Vec<u8>> {
        self.inner.blob(digest)
    }
}

/// Simple GET that returns the body as bytes — used to sync feeds
/// (e.g. the CVE feed of `scan --update`).
pub fn http_get(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(reg_err)?;
    let resp = client.get(url).send().map_err(reg_err)?;
    if !resp.status().is_success() {
        return Err(Error::Registry(format!("HTTP {} at {url}", resp.status())));
    }
    Ok(resp.bytes().map_err(reg_err)?.to_vec())
}

/// GET with optional Bearer; returns `(http_status, body)`. Same transport as
/// [`http_post_json`] (accepts self-signed only with `DELONIX_API_INSECURE=1`). Used by the
/// CLI to read platform resources (e.g. `delonix stack pull` → /v2/studio/designs).
pub fn http_get_auth(url: &str, token: Option<&str>) -> Result<(u16, Vec<u8>)> {
    let insecure = std::env::var("DELONIX_API_INSECURE").ok().as_deref() == Some("1");
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(Duration::from_secs(60))
        .danger_accept_invalid_certs(insecure)
        .build()
        .map_err(reg_err)?;
    let mut req = client.get(url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().map_err(reg_err)?;
    let status = resp.status().as_u16();
    Ok((status, resp.bytes().map_err(reg_err)?.to_vec()))
}

/// POST of a JSON body with an optional Bearer; returns `(http_status, body)`.
/// Used by the CLI's HTTP TRANSPORT (`DELONIX_HOST=https://…` → `/v2/cli`): the CLI
/// sends its argv to the API, which runs the command on the platform. Accepts
/// self-signed certificates only with `DELONIX_API_INSECURE=1` (the self-hosted Console is self-signed;
/// a Cloud with valid TLS does not need it).
pub fn http_post_json(url: &str, body: &str, token: Option<&str>) -> Result<(u16, Vec<u8>)> {
    let insecure = std::env::var("DELONIX_API_INSECURE").ok().as_deref() == Some("1");
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(Duration::from_secs(600))
        .danger_accept_invalid_certs(insecure)
        .build()
        .map_err(reg_err)?;
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body.to_string());
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().map_err(reg_err)?;
    let status = resp.status().as_u16();
    Ok((status, resp.bytes().map_err(reg_err)?.to_vec()))
}

/// Like [`http_post_json`], but STREAMING: delivers the response bytes in chunks
/// to `on_bytes` as they arrive (does not wait for the end). Returns the HTTP status.
/// Used by the CLI's HTTP transport for streaming commands (`logs -f`, …).
/// No timeout (those commands run indefinitely).
pub fn http_post_stream(
    url: &str,
    body: &str,
    token: Option<&str>,
    mut on_bytes: impl FnMut(&[u8]),
) -> Result<u16> {
    use std::io::Read;
    let insecure = std::env::var("DELONIX_API_INSECURE").ok().as_deref() == Some("1");
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(None)
        .danger_accept_invalid_certs(insecure)
        .build()
        .map_err(reg_err)?;
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body.to_string());
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let mut resp = req.send().map_err(reg_err)?;
    let status = resp.status().as_u16();
    let mut buf = [0u8; 8192];
    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| Error::Registry(format!("stream: {e}")))?;
        if n == 0 {
            break;
        }
        on_bytes(&buf[..n]);
    }
    Ok(status)
}

/// Downloads `reference` from an OCI registry into the local store. Credentials
/// (if any) come from the local `delonix login` (`<root>/auth.json`).
pub fn pull_from_registry(store: &ImageStore, reference: &str) -> Result<Image> {
    pull_from_registry_with_creds(store, reference, None)
}

/// Like [`pull_from_registry`], but with explicit credentials
/// (`creds_override = Some((user, password))`), used INSTEAD of the
/// local `delonix login` — for callers that already receive credentials from
/// another source (e.g. the CRI, which receives `AuthConfig` from the kubelet from the
/// Pod's `imagePullSecrets` — it cannot rely only on the node's local
/// `auth.json`, which may not even have that tenant's credentials). `None` keeps the
/// old behaviour (local lookup).
pub fn pull_from_registry_with_creds(
    store: &ImageStore,
    reference: &str,
    creds_override: Option<(String, String)>,
) -> Result<Image> {
    pull_from_registry_with_creds_platform(store, reference, creds_override, None)
}

/// Like [`pull_from_registry_with_creds`], but with an explicit `--platform`
/// architecture (`requested_arch`, OCI vocabulary — `amd64`/`arm64`/...):
/// `None` keeps today's behavior (host arch, via `target_arch()`); `Some(arch)`
/// picks that arch's entry from a multi-arch manifest index instead, and
/// stamps it into the resulting `Image.config.architecture` — the only way a
/// later caller can tell a locally-tagged image was pulled for a non-host arch
/// (see [`crate::image::ImageConfig::architecture`]).
pub fn pull_from_registry_with_creds_platform(
    store: &ImageStore,
    reference: &str,
    creds_override: Option<(String, String)>,
    requested_arch: Option<&str>,
) -> Result<Image> {
    pull_from_registry_with_creds_full(store, reference, creds_override, requested_arch, None)
}

/// `(layer_index_1_based, layer_total, bytes_done, bytes_total)`.
/// `+ Sync` because the layers are pulled in PARALLEL and every worker reports
/// through this callback. A non-`Sync` closure would be a data race, and the
/// compiler is the right place to catch it — the alternative (serialising the
/// callback behind a mutex inside the pull) would hide a contended lock in the
/// hot path for no benefit: a progress callback that cannot be called from two
/// threads has no business in a parallel pull.
pub type PullProgressCb<'a> = &'a (dyn Fn(usize, usize, u64, Option<u64>) + Sync);

/// When `reference` names a content digest (`sha256:...`), verifies the fetched
/// manifest bytes hash to EXACTLY that digest. A digest-pinned pull
/// (`repo@sha256:...`) is the whole point of pinning: even a compromised or
/// MITM'd registry must not be able to substitute the content. The blobs are
/// already checked against the digests the manifest declares — but if the
/// manifest ITSELF is not checked against the pinned reference, the registry
/// can serve a completely different, internally-consistent manifest (pointing
/// at the attacker's blobs) and the pin becomes decorative. The chain of trust
/// for a digest pull rests entirely on this check. A tag reference (`:latest`)
/// has no digest to verify, so it is a no-op there — TLS is the only integrity
/// for tags, same as `docker pull`.
fn verify_manifest_digest(reference: &str, manifest_bytes: &[u8]) -> Result<()> {
    if let Some(want) = reference.strip_prefix("sha256:") {
        let got = sha256_hex(manifest_bytes);
        if !got.eq_ignore_ascii_case(want) {
            return Err(Error::Registry(format!(
                "manifest digest mismatch: reference pins sha256:{want} but the registry \
                 served a manifest hashing to sha256:{got} — refusing (possible compromised \
                 registry or MITM)"
            )));
        }
    }
    Ok(())
}

/// Like [`pull_from_registry_with_creds_platform`], with an optional per-layer
/// download progress callback — the multi-layer sibling of
/// `pull_oci_artifact_with_progress` (single-blob VM artifacts). BUG FOUND
/// live: `delonix image pull <ref>` gave no feedback at all for a large image
/// (multiple, sometimes hundreds-of-MB, layers) beyond one log line at the
/// very start — looked hung, unlike `docker pull`'s familiar per-layer bars.
/// The engine crate only REPORTS bytes; the bin draws (same split
/// `blob_with_progress`'s own doc comment establishes).
pub fn pull_from_registry_with_creds_full(
    store: &ImageStore,
    reference: &str,
    creds_override: Option<(String, String)>,
    requested_arch: Option<&str>,
    progress: Option<PullProgressCb>,
) -> Result<Image> {
    let (host, repo, refr) = parse_reference(reference);
    let http = transfer_client()?;
    let creds = creds_override.or_else(|| crate::auth::lookup(store.root(), &host));
    let mut c = Client {
        http,
        host: host.clone(),
        repo: repo.clone(),
        token: None,
        creds,
    };

    tracing::info!(repo = %repo, reference = %refr, host = %host, "pulling {repo}:{refr} from {host}");

    // 1) manifest (may be a multi-arch index)
    let murl = c.manifest_url(&refr);
    let resp = c.fetch(&murl, ACCEPT_MANIFEST)?;
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.bytes().map_err(reg_err)?.to_vec();
    // A digest pin (`@sha256:...`) covers whatever the registry returned for it —
    // the index (multi-arch) OR a single manifest. Verify it here, before we act
    // on any of it (before picking a platform, before fetching any blob).
    verify_manifest_digest(&refr, &body)?;

    let manifest_bytes = if content_type.contains("index") || content_type.contains("manifest.list")
    {
        // Multi-arch index (`oci_spec::image::ImageIndex`) — picks the
        // linux/<arch> entry (or the first one, lacking a match).
        let index: ImageIndex = serde_json::from_slice(&body)?;
        let arch = requested_arch.unwrap_or_else(|| target_arch());
        let pick = index
            .manifests()
            .iter()
            .find(|m| {
                m.platform()
                    .as_ref()
                    .map(|p| p.os().to_string() == "linux" && p.architecture().to_string() == arch)
                    .unwrap_or(false)
            })
            .or_else(|| index.manifests().first())
            .ok_or_else(|| Error::Registry("empty manifest index".into()))?;
        tracing::info!(arch = %arch, "platform selected: linux/{arch}");
        let purl = c.manifest_url(pick.digest().as_ref());
        let r = c.fetch(&purl, ACCEPT_MANIFEST)?;
        let sub = r.bytes().map_err(reg_err)?.to_vec();
        // The picked sub-manifest is addressed by the index's own digest for that
        // platform — verify the bytes hash to it, or a registry that passed the
        // index check could still swap the per-arch manifest underneath us.
        verify_manifest_digest(pick.digest().as_ref(), &sub)?;
        sub
    } else {
        body
    };

    // Image manifest (`oci_spec::image::ImageManifest`) — OCI/Docker v2 schema.
    let manifest: ImageManifest = serde_json::from_slice(&manifest_bytes)?;

    // 2) config blob (= image id). CAS-first: content-addressed means "already
    // have this exact digest" IS the proof of correctness (the path itself is
    // the hash) — a re-pull of an already-fully-local image (or one that
    // shares a base layer with something already pulled) used to hit the
    // network for every single blob regardless, which is why pre-baking
    // images into the golden VM never actually saved a `kubeadm init` a
    // single byte of download: nothing ever checked `Cas::has` first. See
    // AGENTS.md ("cluster kubeadm" section) for the real-world symptom this
    // fixes (a kubeadm rate-limiter timeout while every core image
    // re-downloaded on every VM boot).
    let config_digest_str = manifest.config().digest().to_string();
    if !store.cas().has(&config_digest_str) {
        let config_bytes = c.blob(&config_digest_str)?;
        if sha256_hex(&config_bytes) != manifest.config().digest().digest() {
            return Err(Error::Registry("config digest mismatch".into()));
        }
        store.cas().write(&config_bytes)?;
    }
    let config_digest = config_digest_str;
    let config_bytes = store.cas().read(&config_digest)?;

    // 3) layers (ignores "foreign"/Windows layers) — same CAS-first check.
    let real_layers: Vec<&Descriptor> = manifest
        .layers()
        .iter()
        .filter(|l| !l.media_type().to_string().contains("foreign"))
        .collect();
    let total = real_layers.len();
    let layers: Vec<String> = real_layers.iter().map(|l| l.digest().to_string()).collect();

    // WHAT IS MISSING, in one pass, before downloading anything. `Cas::has` is
    // the check that makes a re-pull of a shared base layer free — it existed
    // and went uncalled once, and a `kubeadm init` re-downloaded every core
    // image on every VM because of it.
    let missing: Vec<String> = layers
        .iter()
        .filter(|d| !store.cas().has(d))
        .cloned()
        .collect();

    if !missing.is_empty() {
        // LAYERS IN PARALLEL, and this is the difference between a pull that
        // saturates a link and one that does not. Measured on this host, same
        // origin, same total bytes: one connection 0.46 MiB/s, four in parallel
        // 1.45 MiB/s aggregate — 3.2x. The ceiling is PER CONNECTION, so a
        // sequential `for` over the layers leaves most of the link idle. This
        // was a plain sequential loop.
        //
        // The cap is small on purpose: a registry throttles per-client, and
        // more sockets past the point the link saturates buys nothing while
        // making a 429 more likely. It also bounds memory — each in-flight
        // layer is buffered whole (a pre-existing property of `blob`, not
        // changed here), so N in flight is N layers of RAM.
        let workers = missing.len().min(4);
        let done_bytes = std::sync::atomic::AtomicU64::new(0);
        let done_layers = std::sync::atomic::AtomicUsize::new(0);
        let next = std::sync::Mutex::new(missing.clone().into_iter());
        let errors = std::sync::Mutex::new(Vec::<String>::new());

        std::thread::scope(|scope| {
            for _ in 0..workers {
                // A clone per worker: `blob` takes `&mut self` only to renew an
                // expired token, and a clone starts with the one already
                // obtained. `reqwest::blocking::Client` shares its connection
                // pool across clones, so this is not N pools.
                let mut cw = c.clone();
                let (next, errors, done_bytes, done_layers) =
                    (&next, &errors, &done_bytes, &done_layers);
                let store = &store;
                scope.spawn(move || loop {
                    let Some(dg) = next.lock().unwrap().next() else {
                        return;
                    };
                    let res = if let Some(cb) = progress {
                        // Progress is AGGREGATE across workers: with several
                        // layers in flight there is no single "layer i of n" to
                        // report, and per-layer bytes would make the bar jump
                        // backwards. `done` is every byte pulled so far.
                        //
                        // BUG FOUND: `blob_with_progress` reports the RUNNING
                        // TOTAL for its blob, and this adapter was adding that
                        // running total into the aggregate on every tick, as if
                        // it were a delta. The reported bytes grew with the
                        // SQUARE of the layer size — a 100 MiB layer in 64 KiB
                        // chunks announced tens of GiB pulled. `pull_oci_artifact`
                        // reads the same callback correctly (it compares `done`
                        // against the total), so the two consumers of one
                        // callback disagreed about what its argument meant.
                        // `Cell` and not an atomic: this closure is built per
                        // blob and only ever called by the worker that owns it.
                        let seen = std::cell::Cell::new(0u64);
                        let adapter = |done: u64, _blob_total: Option<u64>| {
                            let chunk = done.saturating_sub(seen.get());
                            seen.set(done);
                            let acc = done_bytes
                                .fetch_add(chunk, std::sync::atomic::Ordering::Relaxed)
                                + chunk;
                            let li = done_layers.load(std::sync::atomic::Ordering::Relaxed) + 1;
                            cb(li.min(total), total, acc, None);
                        };
                        cw.blob_with_progress(&dg, Some(&adapter))
                    } else {
                        cw.blob(&dg)
                    };
                    match res.and_then(|data| store.cas().write(&data)) {
                        Ok(written) if written == dg => {
                            done_layers.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        Ok(_) => errors
                            .lock()
                            .unwrap()
                            .push(format!("corrupted layer: {dg}")),
                        Err(e) => errors.lock().unwrap().push(format!("layer {dg}: {e}")),
                    }
                });
            }
        });

        // Every failure, not just the first: a pull that dies on three layers
        // and names one sends the reader looking at the wrong thing.
        let errs = errors.into_inner().unwrap();
        if !errs.is_empty() {
            return Err(Error::Registry(errs.join("; ")));
        }
    }

    // 4) assemble and store — read the runtime config (Cmd/Env/Entrypoint/User/WorkingDir)
    // from the OCI config blob (`oci_spec::image::ImageConfiguration`).
    let oci_config: ImageConfiguration = serde_json::from_slice(&config_bytes)?;
    let inner = oci_config.config().clone().unwrap_or_default();
    let repo_tags = store.merged_tags(&config_digest, reference);
    let image = Image {
        id: config_digest,
        repo_tags,
        layers,
        config: ImageConfig {
            cmd: inner.cmd().clone().unwrap_or_default(),
            entrypoint: inner.entrypoint().clone().unwrap_or_default(),
            env: inner.env().clone().unwrap_or_default(),
            user: inner.user().clone().unwrap_or_default(),
            working_dir: inner.working_dir().clone().unwrap_or_default(),
            cpus: None,
            memory: None,
            security: Vec::new(),
            healthcheck: None,
            architecture: requested_arch
                .map(str::to_string)
                .unwrap_or_else(|| target_arch().to_string()),
        },
        created_unix: now_unix(),
    };
    store.enforce_tag_uniqueness(&image)?;
    store.save(&image)?;
    Ok(image)
}

/// Publishes a local image to an OCI registry (Docker Registry HTTP API V2).
///
/// Rebuilds a schema-2 manifest from the CAS blobs (config = `id`,
/// layers = `layers`), sends the missing ones (`POST`+monolithic `PUT`, with remote
/// dedup by `HEAD`) and publishes the manifest under the target tag. The `push`
/// needs credentials (`delonix login <host>`) for authenticated registries.
/// Builds the **Docker schema-2 manifest** of a local image (config +
/// layer descriptors, with the mediaType detected by the magic number of each
/// blob). Returns `(bytes, digest)`. Used by the internal registry's OCI server
/// to serve `docker pull` without re-packing anything.
pub fn build_manifest(store: &ImageStore, image: &Image) -> Result<(Vec<u8>, String)> {
    let manifest = docker_manifest(store, image)?;
    let bytes = serde_json::to_vec(&manifest)?;
    let digest = format!("sha256:{}", crate::cas::sha256_hex(&bytes));
    Ok((bytes, digest))
}

/// Builds the Docker schema-2 [`ImageManifest`] of a local image (config +
/// layer descriptors, mediaType detected by magic number). Shared by
/// [`build_manifest`] (serving) and [`push_to_registry`] (publishing).
fn docker_manifest(store: &ImageStore, image: &Image) -> Result<ImageManifest> {
    let config_data = store.cas().read(&image.id)?;
    let config_desc = descriptor(DOCKER_CONFIG_MEDIA_TYPE, config_data.len(), &image.id)?;
    let mut layer_descs = Vec::with_capacity(image.layers.len());
    for dg in &image.layers {
        let data = store.cas().read(dg)?;
        layer_descs.push(descriptor(layer_media_type(&data), data.len(), dg)?);
    }
    ImageManifestBuilder::default()
        .schema_version(2u32)
        .media_type(DOCKER_MANIFEST_MEDIA_TYPE)
        .config(config_desc)
        .layers(layer_descs)
        .build()
        .map_err(oci_err)
}

pub fn push_to_registry(store: &ImageStore, source: &str, target: &str) -> Result<String> {
    let image = store.resolve(source)?;
    let (host, repo, refr) = parse_reference(target);
    let http = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(reg_err)?;
    let creds = crate::auth::lookup(store.root(), &host);
    let mut c = Client {
        http,
        host: host.clone(),
        repo: repo.clone(),
        token: None,
        creds,
    };

    tracing::info!(repo = %repo, reference = %refr, host = %host, "pushing {repo}:{refr} to {host}");

    // 1) send the config blob.
    let config_data = store.cas().read(&image.id)?;
    c.push_blob(&with_prefix(&image.id), &config_data)?;

    // 2) send the layers (those missing from the registry).
    let total = image.layers.len();
    for (i, dg) in image.layers.iter().enumerate() {
        let data = store.cas().read(dg)?;
        tracing::debug!(
            index = i + 1,
            total,
            digest = %&dg[..dg.len().min(19)],
            "pushing layer {}/{}",
            i + 1,
            total
        );
        c.push_blob(&with_prefix(dg), &data)?;
    }

    // 3) Docker schema-2 manifest (`oci_spec::image::ImageManifest`) + publication
    // under the tag. Same construction shared by `build_manifest`.
    let manifest = docker_manifest(store, &image)?;
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    c.push_manifest(&refr, &manifest_bytes, DOCKER_MANIFEST_MEDIA_TYPE)?;

    let digest = format!("sha256:{}", sha256_hex(&manifest_bytes));
    tracing::info!(host = %host, repo = %repo, reference = %refr, digest = %digest, "pushed: {host}/{repo}:{refr}");
    Ok(digest)
}

/// Media type of the empty config of an OCI 1.1 artifact (ORAS/Helm convention
/// for artifacts that are not container images).
const EMPTY_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.empty.v1+json";
const EMPTY_CONFIG_BYTES: &[u8] = b"{}";

/// Publishes `data` as a **single-blob** OCI 1.1 artifact (empty config + 1
/// layer) — used for VM images (qcow2), which are not container
/// images (those use [`push_to_registry`], with Docker layers/config). It only
/// generalises the manifest: it reuses the same [`Client`] (auth/upload) already
/// tested. `root` is only used for `crate::auth::lookup` (credentials from
/// `delonix login`) — with no `ImageStore`/CAS involved, it is a loose blob.
pub fn push_oci_artifact(
    root: &std::path::Path,
    target: &str,
    layer_media_type: &str,
    data: &[u8],
) -> Result<String> {
    push_oci_artifact_with_annotations(root, target, layer_media_type, data, &BTreeMap::new())
}

/// Like [`push_oci_artifact`], but records `annotations` on the manifest, so
/// the pull side can recover metadata that the blob itself does not carry.
///
/// A single-blob artifact is just a qcow2: everything the store knows about a
/// VM image (which distro, whether the guest runs cloud-init, the recommended
/// vCPU/memory) lived only in the local `.json`, so a `vm pull` produced an
/// image with those fields blank. Harmless for a cloud image; NOT harmless for
/// an appliance, where "does this guest run cloud-init" decides whether
/// `vm create` attaches a seed the guest cannot read.
pub fn push_oci_artifact_with_annotations(
    root: &std::path::Path,
    target: &str,
    layer_media_type: &str,
    data: &[u8],
    annotations: &BTreeMap<String, String>,
) -> Result<String> {
    let (host, repo, refr) = parse_reference(target);
    let http = transfer_client()?;
    let creds = crate::auth::lookup(root, &host);
    let mut c = Client {
        http,
        host: host.clone(),
        repo: repo.clone(),
        token: None,
        creds,
    };

    tracing::info!(repo = %repo, reference = %refr, host = %host, "pushing artifact {repo}:{refr} to {host}");

    let config_digest = with_prefix(&sha256_hex(EMPTY_CONFIG_BYTES));
    c.push_blob(&config_digest, EMPTY_CONFIG_BYTES)?;

    let layer_digest = with_prefix(&sha256_hex(data));
    tracing::debug!(
        digest = %&layer_digest[..19.min(layer_digest.len())],
        bytes = data.len(),
        "pushing blob"
    );
    c.push_blob(&layer_digest, data)?;

    // OCI 1.1 artifact manifest (`oci_spec::image::ImageManifest` with
    // `artifactType` + empty config `EmptyJSON`), ORAS/Helm standard.
    let manifest = ImageManifestBuilder::default()
        .schema_version(2u32)
        .media_type(MediaType::ImageManifest)
        .artifact_type(MediaType::from(layer_media_type))
        .config(descriptor(
            EMPTY_CONFIG_MEDIA_TYPE,
            EMPTY_CONFIG_BYTES.len(),
            &config_digest,
        )?)
        .layers(vec![descriptor(
            layer_media_type,
            data.len(),
            &layer_digest,
        )?])
        .annotations(
            annotations
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<std::collections::HashMap<String, String>>(),
        )
        .build()
        .map_err(oci_err)?;
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    c.push_manifest(&refr, &manifest_bytes, MediaType::ImageManifest.as_ref())?;

    let digest = format!("sha256:{}", sha256_hex(&manifest_bytes));
    tracing::info!(host = %host, repo = %repo, reference = %refr, digest = %digest, "pushed: {host}/{repo}:{refr}");
    Ok(digest)
}

/// Tags of `source`'s repository (host/repo part; any tag on `source` itself
/// is ignored). Same lightweight shape as [`push_oci_artifact`]/
/// [`pull_oci_artifact`] — no [`crate::image::ImageStore`] needed, `root` is
/// only used for `crate::auth::lookup`.
pub fn list_remote_tags(root: &std::path::Path, source: &str) -> Result<Vec<String>> {
    let (host, repo, _refr) = parse_reference(source);
    let http = transfer_client()?;
    let creds = crate::auth::lookup(root, &host);
    let mut c = Client {
        http,
        host,
        repo,
        token: None,
        creds,
    };
    c.list_tags()
}

/// What a remote artifact says about itself WITHOUT downloading it: the size
/// and digest of its single layer, plus whatever
/// [`push_oci_artifact_with_annotations`] stamped on the manifest.
///
/// Enough for `image vm ls-remote` to show what a tag actually is — distro,
/// size, whether it runs cloud-init — instead of a bare list of names that
/// tells the reader nothing about which one to pull.
#[derive(Debug, Clone)]
pub struct RemoteArtifact {
    pub tag: String,
    pub digest: String,
    pub size: u64,
    pub annotations: BTreeMap<String, String>,
}

/// Reads one tag's manifest (a single GET, no blob transfer).
pub fn describe_remote_artifact(
    root: &std::path::Path,
    source: &str,
    tag: &str,
) -> Result<RemoteArtifact> {
    let (host, repo, _) = parse_reference(source);
    let http = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(reg_err)?;
    let creds = crate::auth::lookup(root, &host);
    let mut c = Client {
        http,
        host,
        repo,
        token: None,
        creds,
    };
    let url = c.manifest_url(tag);
    let bytes = c
        .fetch(&url, "application/vnd.oci.image.manifest.v1+json")?
        .bytes()
        .map_err(reg_err)?
        .to_vec();
    let manifest: ImageManifest = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Registry(format!("invalid artifact manifest: {e}")))?;
    let layer = manifest
        .layers()
        .first()
        .ok_or_else(|| Error::Registry("artifact manifest has no layers".into()))?;
    Ok(RemoteArtifact {
        tag: tag.to_string(),
        digest: layer.digest().to_string(),
        size: layer.size() as u64,
        annotations: manifest
            .annotations()
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect(),
    })
}

/// Pull of an artifact published by [`push_oci_artifact`] — resolves the
/// manifest and returns the bytes of the (single) layer.
pub fn pull_oci_artifact(root: &std::path::Path, source: &str) -> Result<Vec<u8>> {
    pull_oci_artifact_with_progress(root, source, None)
}

/// Like [`pull_oci_artifact`], but with a progress callback for the blob
/// download (`(bytes_read, total)`), for a progress bar in the caller.
pub fn pull_oci_artifact_with_progress(
    root: &std::path::Path,
    source: &str,
    progress: Option<&dyn Fn(u64, Option<u64>)>,
) -> Result<Vec<u8>> {
    pull_oci_artifact_with_meta(root, source, progress).map(|(data, _)| data)
}

/// Like [`pull_oci_artifact_with_progress`], but also returns the manifest
/// annotations written by [`push_oci_artifact_with_annotations`] — the only
/// channel through which a single-blob artifact can carry anything beyond the
/// blob. Empty for an artifact published without them.
pub fn pull_oci_artifact_with_meta(
    root: &std::path::Path,
    source: &str,
    progress: Option<&dyn Fn(u64, Option<u64>)>,
) -> Result<(Vec<u8>, BTreeMap<String, String>)> {
    let (host, repo, refr) = parse_reference(source);
    let http = transfer_client()?;
    let creds = crate::auth::lookup(root, &host);
    let mut c = Client {
        http,
        host,
        repo,
        token: None,
        creds,
    };

    let accept = "application/vnd.oci.image.manifest.v1+json";
    let url = c.manifest_url(&refr);
    let manifest_bytes = c.fetch(&url, accept)?.bytes().map_err(reg_err)?.to_vec();
    // A digest-pinned artifact pull (a golden VM image referenced by
    // `@sha256:...`) must verify the manifest against the pin too — otherwise a
    // compromised registry substitutes the whole manifest and the single-blob
    // check below only proves it is self-consistent with the attacker's bytes.
    verify_manifest_digest(&refr, &manifest_bytes)?;
    let manifest: ImageManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| Error::Registry(format!("invalid artifact manifest: {e}")))?;
    let layer = manifest
        .layers()
        .first()
        .ok_or_else(|| Error::Registry("artifact manifest has no layers".into()))?;
    let layer_digest = layer.digest().to_string();
    let data = c.blob_with_progress(&layer_digest, progress)?;

    // Security-audit finding: the old path (`pull_from_registry_with_creds`)
    // already verifies each blob against the expected digest before accepting it — this path
    // (single-blob artifacts, e.g. VM images) had been left without that verification,
    // which let a compromised registry/content-MITM serve bytes different from the
    // announced digest without detection. See AGENTS.md.
    let got = format!("sha256:{}", sha256_hex(&data));
    if got != layer_digest {
        return Err(Error::Registry(format!(
            "artifact corrupted or tampered: expected digest {layer_digest}, got {got}"
        )));
    }
    // Read AFTER the digest check, so annotations from a manifest that failed
    // verification never reach the caller.
    let annotations: BTreeMap<String, String> = manifest
        .annotations()
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect();
    Ok((data, annotations))
}

#[cfg(test)]
mod tests {
    use super::{
        layer_media_type, parse_content_range, parse_reference, pull_from_registry_with_creds,
        pull_oci_artifact, push_oci_artifact, sha256_hex, with_prefix, Client,
    };

    fn test_client(host: &str, repo: &str) -> Client {
        Client {
            http: reqwest::blocking::Client::new(),
            host: host.to_string(),
            repo: repo.to_string(),
            token: None,
            creds: None,
        }
    }

    #[test]
    fn tags_url_is_the_v2_tags_list_endpoint() {
        let c = test_client("ghcr.io", "angolardevops/delonix-vm-k8s");
        assert_eq!(
            c.tags_url(),
            "https://ghcr.io/v2/angolardevops/delonix-vm-k8s/tags/list"
        );
    }

    #[test]
    fn with_prefix_is_idempotent() {
        assert_eq!(with_prefix("abc"), "sha256:abc");
        assert_eq!(with_prefix("sha256:abc"), "sha256:abc");
    }

    #[test]
    fn detects_layer_compression() {
        assert!(layer_media_type(&[0x1f, 0x8b, 0x08]).contains("gzip"));
        assert!(layer_media_type(&[0x28, 0xb5, 0x2f, 0xfd]).contains("zstd"));
        assert!(layer_media_type(b"ustar  ").ends_with(".tar"));
    }

    #[test]
    fn parses_docker_hub_official() {
        let (h, r, t) = parse_reference("nginx");
        assert_eq!(h, "registry-1.docker.io");
        assert_eq!(r, "library/nginx");
        assert_eq!(t, "latest");
    }

    #[test]
    fn parses_user_repo_and_tag() {
        let (h, r, t) = parse_reference("bitnami/redis:7.2");
        assert_eq!(h, "registry-1.docker.io");
        assert_eq!(r, "bitnami/redis");
        assert_eq!(t, "7.2");
    }

    #[test]
    fn parses_other_registry_with_port() {
        let (h, r, t) = parse_reference("ghcr.io/owner/app:v1");
        assert_eq!(h, "ghcr.io");
        assert_eq!(r, "owner/app");
        assert_eq!(t, "v1");
    }

    #[test]
    fn parses_digest() {
        let (_, r, t) = parse_reference("alpine@sha256:abc123");
        assert_eq!(r, "library/alpine");
        assert_eq!(t, "sha256:abc123");
    }

    /// Found when testing `kindest/node:v1.34.0@sha256:...` (the base image of
    /// `kind`) — the `@` branch of `parse_reference` did not cut the tag before the
    /// `@`, leaving `name` (and thus `repo`) with the tag inside it
    /// (`kindest/node:v1.34.0`), which produced a malformed manifest
    /// URL. `repo:tag@digest` is a valid reference in Docker/OCI —
    /// the digest rules the resolution, the tag is only informative.
    #[test]
    fn parses_repo_tag_and_digest_combined() {
        let (h, r, t) =
            parse_reference("kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac1");
        assert_eq!(h, "registry-1.docker.io");
        assert_eq!(r, "kindest/node");
        assert_eq!(t, "sha256:7416a61b42b1662ca6ca89f02028ac1");
    }

    #[test]
    fn parses_repo_tag_and_digest_combined_com_registo_explicito() {
        let (h, r, t) = parse_reference("ghcr.io/owner/app:v1@sha256:deadbeef");
        assert_eq!(h, "ghcr.io");
        assert_eq!(r, "owner/app");
        assert_eq!(t, "sha256:deadbeef");
    }

    // ---- Robustness ("fuzz on stable" via proptest) ----------------------
    // `parse_reference` parses UNTRUSTED input (an image ref from a manifest,
    // a CLI arg, a compose file). It has already had real bugs (the combined
    // `repo:tag@digest` form). It is total by contract; this proves it against
    // arbitrary and structured input instead of the handful of hand-picked cases.

    proptest::proptest! {
        // Arbitrary bytes-as-string: must never panic, always return owned parts.
        #[test]
        fn parse_reference_never_panics_on_arbitrary_input(s in ".*") {
            let _ = parse_reference(&s);
        }

        // Structured refs built from realistic fragments (registry/repo/tag/digest,
        // plus the separators and empties that break naive splitters) — same
        // no-panic guarantee, over inputs shaped like the real thing. When a
        // non-empty digest is present it must win the reference slot (the
        // resolution-authoritative half — the invariant a `repo:tag@digest` relies on).
        #[test]
        fn parse_reference_digest_wins_over_tag(
            host in proptest::option::of("[a-z0-9.-]{1,30}"),
            repo in "[a-z0-9][a-z0-9/_.-]{0,40}",
            tag in proptest::option::of("[A-Za-z0-9_.-]{1,20}"),
            hex in "[0-9a-f]{64}",
        ) {
            let mut r = String::new();
            if let Some(h) = host { r.push_str(&h); r.push('/'); }
            r.push_str(&repo);
            if let Some(t) = tag { r.push(':'); r.push_str(&t); }
            r.push_str("@sha256:");
            r.push_str(&hex);
            let (_h, _repo, reference) = parse_reference(&r);
            proptest::prop_assert_eq!(reference, format!("sha256:{hex}"));
        }
    }

    /// Minimal HTTP server (one connection, one canonical response) — enough
    /// to simulate an OCI registry that requires a token and capture the
    /// `Authorization` header the client sent when requesting that token.
    fn serve_one(
        port_tx: std::sync::mpsc::Sender<u16>,
        resp_after_401: &'static str,
    ) -> std::thread::JoinHandle<Option<String>> {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        port_tx.send(port).unwrap();
        std::thread::spawn(move || {
            // 1st connection: manifest request → 401 + WWW-Authenticate pointing
            // to the token endpoint on THIS SAME server.
            let (mut s1, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = s1.read(&mut buf);
            let www = format!(
                "Bearer realm=\"http://127.0.0.1:{port}/token\",service=\"test\",scope=\"repository:x:pull\""
            );
            let body401 = format!(
                "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: {www}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            let _ = s1.write_all(body401.as_bytes());
            drop(s1);

            // 2nd connection: TOKEN request → this is where we capture the Authorization
            // (Basic) that `pull_from_registry_with_creds` generated from the
            // credentials (override or local lookup).
            let (mut s2, _) = listener.accept().unwrap();
            let mut buf2 = [0u8; 4096];
            let n = s2.read(&mut buf2).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf2[..n]).to_string();
            let auth_header = req
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .map(|l| l.trim().to_string());
            let _ = s2.write_all(resp_after_401.as_bytes());
            drop(s2);
            auth_header
        })
    }

    #[test]
    fn pull_com_creds_override_usa_essas_credenciais_no_token_request() {
        let (tx, rx) = std::sync::mpsc::channel();
        // response to the token request: 401 again (we do not need to complete the
        // pull — only to observe the Authorization sent in the token request).
        let handle = serve_one(
            tx,
            "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
        let port = rx.recv().unwrap();

        let tmp = std::env::temp_dir().join(format!(
            "delonix-image-pull-creds-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = crate::ImageStore::open(&tmp).unwrap();
        // WITHOUT a local `delonix login` (auth.json does not exist) — if the precedence
        // were wrong (override ignored, only local lookup), the captured
        // Authorization would be None (no creds at all).
        let reference = format!("127.0.0.1:{port}/repo:tag");
        let _ = pull_from_registry_with_creds(
            &store,
            &reference,
            Some(("cri-user".to_string(), "cri-pass".to_string())),
        ); // an error is expected (2nd 401) — we only care about the captured Authorization.

        let captured = handle.join().unwrap();
        let auth = captured.expect("o cliente devia ter pedido um token (com Authorization Basic)");
        // "Basic " + base64("cri-user:cri-pass")
        let expected_b64 = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(b"cri-user:cri-pass")
        };
        assert!(
            auth.to_ascii_lowercase()
                .contains(&format!("basic {}", expected_b64.to_lowercase())),
            "Authorization capturado não usa as credenciais do override: {auth:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Minimal mock of an ANONYMOUS OCI registry (no 401 challenge — like a
    /// public `ghcr.io` or a local registry without auth): stores blobs/manifests
    /// in memory and serves them back. Enough for a real round-trip of
    /// `push_oci_artifact`→`pull_oci_artifact` without depending on the network.
    /// Also returns a counter of `GET .../blobs/...` requests served — used by
    /// `pull_from_registry_with_creds_salta_blobs_ja_no_cas` to prove a 2nd
    /// pull of the same reference does not touch the network for content
    /// already in the local CAS.
    fn serve_anon_registry() -> (
        u16,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let blobs: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>> =
            Arc::new(Mutex::new(Default::default()));
        let manifests: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>> =
            Arc::new(Mutex::new(Default::default()));
        let blob_gets = Arc::new(AtomicUsize::new(0));
        let blob_gets_thread = blob_gets.clone();
        let handle = std::thread::spawn(move || {
            listener.set_nonblocking(false).unwrap();
            loop {
                let (mut s, _) = match listener.accept() {
                    Ok(x) => x,
                    Err(_) => return,
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                // read headers (up to \r\n\r\n), then the body by Content-Length.
                let header_end = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = find_subslice(&buf, b"\r\n\r\n") {
                        break Some(i);
                    }
                    if buf.len() > 1_000_000 {
                        break None;
                    }
                };
                let Some(hend) = header_end else { continue };
                let head = String::from_utf8_lossy(&buf[..hend]).to_string();
                let mut lines = head.lines();
                let first = lines.next().unwrap_or_default();
                let mut parts = first.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let content_length: usize = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                    .and_then(|l| l.split(':').nth(1))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let mut body = buf[hend + 4..].to_vec();
                while body.len() < content_length {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }

                let write_resp = |s: &mut std::net::TcpStream,
                                  status: &str,
                                  headers: &str,
                                  body: &[u8]| {
                    let head = format!(
                        "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(body);
                };

                if method == "POST" && path.contains("/blobs/uploads/") {
                    write_resp(
                        &mut s,
                        "202 Accepted",
                        &format!("location: {path}upload-1\r\n"),
                        b"",
                    );
                } else if method == "PUT" && path.contains("/blobs/uploads/") {
                    let digest = path.split("digest=").nth(1).unwrap_or("").to_string();
                    blobs.lock().unwrap().insert(digest, body);
                    write_resp(&mut s, "201 Created", "", b"");
                } else if method == "HEAD" && path.contains("/blobs/") {
                    let digest = path.rsplit('/').next().unwrap_or("").to_string();
                    if blobs.lock().unwrap().contains_key(&digest) {
                        write_resp(&mut s, "200 OK", "", b"");
                    } else {
                        write_resp(&mut s, "404 Not Found", "", b"");
                    }
                } else if method == "GET" && path.contains("/blobs/") {
                    blob_gets_thread.fetch_add(1, Ordering::SeqCst);
                    let digest = path.rsplit('/').next().unwrap_or("").to_string();
                    match blobs.lock().unwrap().get(&digest) {
                        Some(data) => write_resp(&mut s, "200 OK", "", data),
                        None => write_resp(&mut s, "404 Not Found", "", b""),
                    }
                } else if method == "PUT" && path.contains("/manifests/") {
                    let refr = path.rsplit('/').next().unwrap_or("").to_string();
                    manifests.lock().unwrap().insert(refr, body);
                    write_resp(&mut s, "201 Created", "", b"");
                } else if method == "GET" && path.contains("/manifests/") {
                    let refr = path.rsplit('/').next().unwrap_or("").to_string();
                    match manifests.lock().unwrap().get(&refr) {
                        Some(data) => write_resp(
                            &mut s,
                            "200 OK",
                            "content-type: application/vnd.oci.image.manifest.v1+json\r\n",
                            data,
                        ),
                        None => write_resp(&mut s, "404 Not Found", "", b""),
                    }
                } else {
                    write_resp(&mut s, "404 Not Found", "", b"");
                }
            }
        });
        (port, blob_gets, handle)
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    #[test]
    fn push_e_pull_oci_artifact_round_trip() {
        let (port, _blob_gets, _handle) = serve_anon_registry();
        let tmp = std::env::temp_dir().join(format!(
            "delonix-image-artifact-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let target = format!("127.0.0.1:{port}/vm-images:golden");
        let payload = b"qcow2-conteudo-fingido-para-o-teste".to_vec();
        let digest = push_oci_artifact(
            &tmp,
            &target,
            "application/vnd.delonix.vmimage.v1.qcow2",
            &payload,
        )
        .expect("push devia ter sucesso contra o mock");
        assert!(digest.starts_with("sha256:"));

        let pulled =
            pull_oci_artifact(&tmp, &target).expect("pull devia ter sucesso contra o mock");
        assert_eq!(pulled, payload);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// BUG FIXED, found live (host kaeso-sys-01): `pull_from_registry_with_creds`
    /// used to fetch every blob from the network unconditionally, even when
    /// the exact content was already in the local CAS — a golden VM
    /// pre-seeded with kubeadm's images would still redownload everything on
    /// every real `kubeadm init` (the actual cause of a real rate-limiter
    /// timeout crash). Now it checks `Cas::has` before each blob GET.
    #[test]
    fn pull_from_registry_with_creds_salta_blobs_ja_no_cas() {
        let (port, blob_gets, _handle) = serve_anon_registry();
        let tmp = std::env::temp_dir().join(format!(
            "delonix-image-cas-skip-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        // `pull_from_registry_with_creds` parses the config blob as a REAL
        // `oci_spec::image::ImageConfiguration` (requires `architecture`/`os`)
        // — unlike `push_oci_artifact`'s single-blob artifacts (empty `{}`
        // config), so the manifest is built by hand here via the crate's own
        // `Client::push_blob`/`push_manifest` (same ones `push_oci_artifact`
        // uses internally) instead of reusing that higher-level function.
        let mut c = test_client(&format!("127.0.0.1:{port}"), "cas-skip");
        let config_bytes =
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#
                .to_vec();
        let config_digest = format!("sha256:{}", sha256_hex(&config_bytes));
        c.push_blob(&config_digest, &config_bytes).unwrap();

        let layer_bytes = b"conteudo-de-layer-fingido-para-o-teste".to_vec();
        let layer_digest = format!("sha256:{}", sha256_hex(&layer_bytes));
        c.push_blob(&layer_digest, &layer_bytes).unwrap();

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "size": config_bytes.len(),
                "digest": config_digest,
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "size": layer_bytes.len(),
                "digest": layer_digest,
            }],
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        c.push_manifest(
            "tag",
            &manifest_bytes,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .unwrap();

        let target = format!("127.0.0.1:{port}/cas-skip:tag");
        let store = crate::ImageStore::open(&tmp).unwrap();
        let img1 = pull_from_registry_with_creds(&store, &target, None)
            .expect("1º pull devia ter sucesso");
        let gets_after_first = blob_gets.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            gets_after_first > 0,
            "o 1º pull tinha de ter pedido pelo menos 1 blob (config + layer)"
        );

        let img2 = pull_from_registry_with_creds(&store, &target, None).expect(
            "2º pull (MESMO store) devia ter sucesso mesmo sem tocar na rede para os blobs",
        );
        let gets_after_second = blob_gets.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            gets_after_first, gets_after_second,
            "o 2º pull não devia ter pedido NENHUM blob novo — já estavam no CAS local"
        );
        assert_eq!(img1.id, img2.id);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Security-audit finding: `blob_with_progress` used to trust the registry's raw
    /// `Content-Length` header for `Vec::with_capacity` (a huge/lying value could abort the
    /// process via allocator failure) AND had no independent cap on the actual bytes read (a
    /// server that just keeps streaming, regardless of what it claimed, could OOM the host).
    /// `blob_with_progress_capped` closes both: the reservation is capped by `max_bytes`, and
    /// the read loop aborts as soon as ACTUAL bytes read exceed it — proven here with a tiny
    /// fake `max_bytes` instead of streaming gigabytes of real data.
    #[test]
    fn blob_with_progress_capped_aborta_acima_do_limite() {
        let (port, _blob_gets, _handle) = serve_anon_registry();
        let mut c = test_client(&format!("127.0.0.1:{port}"), "blob-cap");

        let big_blob = vec![b'x'; 1024];
        let digest = format!("sha256:{}", sha256_hex(&big_blob));
        c.push_blob(&digest, &big_blob).unwrap();

        let err = c
            .blob_with_progress_capped(&digest, None, 100)
            .expect_err("devia recusar um blob maior do que o limite");
        let msg = err.to_string();
        assert!(msg.contains(&digest), "{msg}");
        assert!(msg.contains("100"), "{msg}");
    }

    /// Sanity check: a blob within the limit still round-trips normally.
    #[test]
    fn blob_with_progress_capped_aceita_dentro_do_limite() {
        let (port, _blob_gets, _handle) = serve_anon_registry();
        let mut c = test_client(&format!("127.0.0.1:{port}"), "blob-cap-ok");

        let small_blob = b"conteudo-pequeno".to_vec();
        let digest = format!("sha256:{}", sha256_hex(&small_blob));
        c.push_blob(&digest, &small_blob).unwrap();

        let got = c
            .blob_with_progress_capped(&digest, None, small_blob.len() as u64)
            .expect("devia aceitar um blob exactamente no limite");
        assert_eq!(got, small_blob);
    }

    /// Security-audit finding: `pull_oci_artifact` must reject a blob whose
    /// real content does not match the digest declared in the manifest — simulates a
    /// compromised/tampered registry that serves different bytes under the same digest.
    #[test]
    fn pull_oci_artifact_recusa_blob_adulterado() {
        let (port, _blob_gets, _handle) = serve_anon_registry();
        let tmp = std::env::temp_dir().join(format!(
            "delonix-image-artifact-tamper-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let target = format!("127.0.0.1:{port}/vm-images:golden");
        let payload = b"conteudo-original-legitimo".to_vec();
        // `push_oci_artifact` returns the MANIFEST's digest, not the layer/blob's — what
        // we need to tamper with is the blob (the same `layer_digest` the pull will fetch).
        let layer_digest = format!("sha256:{}", sha256_hex(&payload));
        push_oci_artifact(
            &tmp,
            &target,
            "application/vnd.delonix.vmimage.v1.qcow2",
            &payload,
        )
        .unwrap();

        // Simulates direct tampering in the registry's storage: replaces the bytes
        // stored under the SAME digest (the manifest still points to `layer_digest`,
        // but the real content changed) — which a normal `push_blob` would never do (dedup
        // by HEAD), but a compromised registry/tampered backend could.
        let http = reqwest::blocking::Client::new();
        let put_url = format!(
            "http://127.0.0.1:{port}/v2/vm-images/blobs/uploads/tamper?digest={layer_digest}"
        );
        let resp = http
            .put(&put_url)
            .body(b"conteudo-adulterado-pelo-atacante".to_vec())
            .send()
            .unwrap();
        assert!(resp.status().is_success());

        let err =
            pull_oci_artifact(&tmp, &target).expect_err("pull devia recusar o blob adulterado");
        assert!(format!("{err}").contains("tampered") || format!("{err}").contains("digest"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Security-audit finding (ALTO): a digest-pinned pull (`repo@sha256:...`) must
    /// verify the MANIFEST against the pinned digest, not only the blobs against the
    /// manifest. Simulates a compromised/MITM registry that answers a request for
    /// `sha256:<X>` with a completely different (but internally consistent) manifest —
    /// the pull must refuse before touching a single blob, or digest-pinning is
    /// decorative. Before the fix, `verify_manifest_digest` did not exist and this
    /// substituted manifest would have been accepted and its blobs pulled.
    #[test]
    fn pull_por_digest_recusa_manifesto_substituido() {
        let (port, blob_gets, _handle) = serve_anon_registry();
        let tmp = std::env::temp_dir().join(format!(
            "delonix-image-manifest-pin-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        // A well-formed manifest the attacker WOULD serve (points at their own blobs).
        let mut c = test_client(&format!("127.0.0.1:{port}"), "pin");
        let config_bytes =
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#
                .to_vec();
        let config_digest = format!("sha256:{}", sha256_hex(&config_bytes));
        c.push_blob(&config_digest, &config_bytes).unwrap();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "size": config_bytes.len(),
                "digest": config_digest,
            },
            "layers": [],
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let real_digest = format!("sha256:{}", sha256_hex(&manifest_bytes));

        // The victim pins a DIFFERENT digest (the legit image they meant to pull).
        // The registry stores the attacker's manifest UNDER that pinned digest key
        // — exactly what a compromised registry/backend does.
        let pinned = format!("sha256:{}", "a".repeat(64));
        assert_ne!(pinned, real_digest);
        c.push_manifest(
            &pinned,
            &manifest_bytes,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .unwrap();

        let target = format!("127.0.0.1:{port}/pin@{pinned}");
        let store = crate::ImageStore::open(&tmp).unwrap();
        let err = pull_from_registry_with_creds(&store, &target, None)
            .expect_err("pull por digest devia recusar um manifesto que não corresponde ao pin");
        let msg = format!("{err}");
        assert!(
            msg.contains("manifest digest mismatch"),
            "erro inesperado: {msg}"
        );
        // Must refuse BEFORE fetching any blob.
        assert_eq!(
            blob_gets.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "não devia ter pedido nenhum blob antes de validar o manifesto"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The positive half: pulling by the CORRECT digest passes the new manifest
    /// verification (the check must not reject a legitimate digest pull).
    #[test]
    fn pull_por_digest_correto_passa_a_verificacao_do_manifesto() {
        let (port, _blob_gets, _handle) = serve_anon_registry();
        let tmp = std::env::temp_dir().join(format!(
            "delonix-image-manifest-pin-ok-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let mut c = test_client(&format!("127.0.0.1:{port}"), "pinok");
        let config_bytes =
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#
                .to_vec();
        let config_digest = format!("sha256:{}", sha256_hex(&config_bytes));
        c.push_blob(&config_digest, &config_bytes).unwrap();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "size": config_bytes.len(),
                "digest": config_digest,
            },
            "layers": [],
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let real_digest = format!("sha256:{}", sha256_hex(&manifest_bytes));
        c.push_manifest(
            &real_digest,
            &manifest_bytes,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .unwrap();

        let target = format!("127.0.0.1:{port}/pinok@{real_digest}");
        let store = crate::ImageStore::open(&tmp).unwrap();
        pull_from_registry_with_creds(&store, &target, None)
            .expect("pull pelo digest correcto devia passar a verificação do manifesto");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Deterministic round-trip of the MANIFEST through the `oci_spec::image` types
    /// (C3-IMG): starts from FIXED manifest bytes (Docker schema-2, real 64-hex
    /// digests), parses with `ImageManifest`, confirms the structure
    /// (config digest, layer order/digests/mediaType) and re-serialises —
    /// the re-serialisation must be IDEMPOTENT (stable digest) and the re-parse must
    /// yield an equal `ImageManifest`. No network: proves that the migration to
    /// `oci-spec` preserves the schema on the pull/push path.
    #[test]
    fn manifesto_round_trip_via_oci_spec_preserva_estrutura_e_digest() {
        use oci_spec::image::ImageManifest;

        // Canonical Docker schema-2 manifest (config + 2 layers, base→top order).
        const MANIFEST: &str = r#"{
  "schemaVersion": 2,
  "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
  "config": {
    "mediaType": "application/vnd.docker.container.image.v1+json",
    "size": 1470,
    "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  },
  "layers": [
    {
      "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
      "size": 3336911,
      "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111"
    },
    {
      "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
      "size": 145,
      "digest": "sha256:2222222222222222222222222222222222222222222222222222222222222222"
    }
  ]
}"#;

        // 1) parse.
        let m: ImageManifest = serde_json::from_str(MANIFEST).expect("parse do manifesto");
        assert_eq!(m.schema_version(), 2);
        assert_eq!(
            m.config().digest().to_string(),
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            m.config().media_type().to_string(),
            "application/vnd.docker.container.image.v1+json"
        );
        // layer order preserved (base=0 → top).
        let layer_digests: Vec<String> =
            m.layers().iter().map(|l| l.digest().to_string()).collect();
        assert_eq!(
            layer_digests,
            vec![
                "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .to_string(),
                "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                    .to_string(),
            ]
        );
        assert!(m
            .layers()
            .iter()
            .all(|l| l.media_type().to_string().ends_with(".tar.gzip")));

        // 2) idempotent re-serialisation (stable digest).
        let bytes1 = serde_json::to_vec(&m).expect("serialize 1");
        let m2: ImageManifest = serde_json::from_slice(&bytes1).expect("re-parse");
        let bytes2 = serde_json::to_vec(&m2).expect("serialize 2");
        assert_eq!(
            sha256_hex(&bytes1),
            sha256_hex(&bytes2),
            "a re-serialização do manifesto tem de ser byte-idêntica (digest estável)"
        );
        // 3) the re-parse is structurally equal (PartialEq of ImageManifest).
        assert_eq!(
            m, m2,
            "round-trip do manifesto tem de preservar a estrutura"
        );
    }

    // ---- resume of an interrupted blob --------------------------------------

    #[test]
    fn parse_content_range_le_o_inicio_e_o_total() {
        assert_eq!(
            parse_content_range("bytes 100-199/200"),
            Some((100, Some(200)))
        );
        // A server that resumes but will not commit to a total.
        assert_eq!(parse_content_range("bytes 100-199/*"), Some((100, None)));
        assert_eq!(parse_content_range("  bytes  0-9/10 "), Some((0, Some(10))));
        // Anything that is not a byte range must NOT be read as one: an
        // unparseable header is the case where the offset cannot be confirmed,
        // and appending there is what corrupts a blob.
        assert_eq!(parse_content_range("items 1-2/3"), None);
        assert_eq!(parse_content_range("bytes */200"), None);
        assert_eq!(parse_content_range(""), None);
    }

    /// How the fake registry answers the SECOND request for a blob.
    #[derive(Clone, Copy, PartialEq)]
    enum Resume {
        /// 206 from exactly the requested offset (a well-behaved registry).
        Honour,
        /// 200 with the whole body — the range header was ignored.
        Ignore,
        /// 206 announcing an offset that is NOT the one asked for. Appending
        /// this would corrupt the blob, so it must restart from zero.
        WrongOffset,
    }

    /// A blob endpoint that DROPS the first connection halfway through the
    /// body, and serves the remainder on the next request according to `mode`.
    /// This is the failure measured live on a slow link: not a timeout, a
    /// connection that dies mid-transfer.
    fn serve_flaky_blob(
        payload: Vec<u8>,
        cut_at: usize,
        mode: Resume,
    ) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match s.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => head.push(byte[0]),
                    }
                }
                let head = String::from_utf8_lossy(&head).to_lowercase();
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let total = payload.len();
                let from: usize = head
                    .lines()
                    .find(|l| l.starts_with("range:"))
                    .and_then(|l| l.split('=').nth(1))
                    .and_then(|v| v.trim().trim_end_matches('-').parse().ok())
                    .unwrap_or(0);

                if n == 0 {
                    // Announce the full length, then hang up early.
                    let _ = s.write_all(
                        format!("HTTP/1.1 200 OK\r\ncontent-length: {total}\r\n\r\n").as_bytes(),
                    );
                    let _ = s.write_all(&payload[..cut_at]);
                    let _ = s.flush();
                    continue; // drops the stream: EOF before the body ended
                }
                match mode {
                    Resume::Honour => {
                        let _ = s.write_all(
                            format!(
                                "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {from}-{}/{total}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                total - 1,
                                total - from
                            )
                            .as_bytes(),
                        );
                        let _ = s.write_all(&payload[from..]);
                    }
                    Resume::Ignore => {
                        let _ = s.write_all(
                            format!("HTTP/1.1 200 OK\r\ncontent-length: {total}\r\nconnection: close\r\n\r\n").as_bytes(),
                        );
                        let _ = s.write_all(&payload);
                    }
                    Resume::WrongOffset => {
                        // Claims to start at 0 while sending everything: a
                        // client that appended blindly would end up with the
                        // prefix twice.
                        let _ = s.write_all(
                            format!(
                                "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 0-{}/{total}\r\ncontent-length: {total}\r\nconnection: close\r\n\r\n",
                                total - 1
                            )
                            .as_bytes(),
                        );
                        let _ = s.write_all(&payload);
                    }
                }
                let _ = s.flush();
            }
        });
        (port, requests)
    }

    /// The bug this closes, measured live: `vm pull` of a 276 MiB image died
    /// after 8m19s with `blob read: request or response body error`, and the
    /// next attempt started again from byte zero — so on a link slower than the
    /// transfer, the image could never finish downloading, no matter how many
    /// times it was retried.
    #[test]
    fn blob_retoma_uma_ligacao_cortada_a_meio() {
        let payload: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let (port, requests) = serve_flaky_blob(payload.clone(), 12_345, Resume::Honour);
        let mut c = test_client(&format!("127.0.0.1:{port}"), "resume");

        let got = c
            .blob_with_progress_capped("sha256:whatever", None, 1 << 20)
            .expect("uma ligação cortada a meio tem de ser retomada, não abandonada");

        assert_eq!(got, payload, "os bytes costurados têm de bater com o blob");
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "devia ter bastado UM pedido de retomada — não um download inteiro de novo"
        );
    }

    /// A registry that ignores `Range` must not produce a corrupt blob: the
    /// only safe reading of a 200 is "it is sending the whole thing again".
    #[test]
    fn blob_recomeca_quando_o_servidor_ignora_o_range() {
        let payload: Vec<u8> = (0..30_000u32).map(|i| (i % 97) as u8).collect();
        let (port, _) = serve_flaky_blob(payload.clone(), 9_000, Resume::Ignore);
        let mut c = test_client(&format!("127.0.0.1:{port}"), "ignore-range");

        let got = c.blob_with_progress_capped("sha256:whatever", None, 1 << 20);
        assert_eq!(got.unwrap(), payload);
    }

    /// A 206 that starts somewhere OTHER than where we stopped answers a
    /// different question. Appending it would duplicate the prefix — corruption
    /// that only the caller's digest check would catch, at the end of the whole
    /// download. It has to restart instead.
    #[test]
    fn blob_recomeca_quando_o_206_vem_do_offset_errado() {
        let payload: Vec<u8> = (0..30_000u32).map(|i| (i % 131) as u8).collect();
        let (port, _) = serve_flaky_blob(payload.clone(), 7_000, Resume::WrongOffset);
        let mut c = test_client(&format!("127.0.0.1:{port}"), "wrong-offset");

        let got = c
            .blob_with_progress_capped("sha256:whatever", None, 1 << 20)
            .expect("devia recomeçar do zero, não colar no sítio errado");
        assert_eq!(
            got, payload,
            "colar um 206 do offset errado duplicaria o prefixo"
        );
    }

    /// The progress callback reports the RUNNING TOTAL for the blob, and both
    /// consumers must read it that way. The parallel pull's adapter used to add
    /// it in as if it were a per-chunk delta, so the bytes it announced grew
    /// with the square of the layer size.
    #[test]
    fn o_progresso_de_um_blob_e_acumulado_e_nunca_passa_do_total() {
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let (port, _) = serve_flaky_blob(payload.clone(), 20_000, Resume::Honour);
        let mut c = test_client(&format!("127.0.0.1:{port}"), "progresso");

        let ticks = std::cell::RefCell::new(Vec::<u64>::new());
        let seen_total = std::cell::Cell::new(None);
        let cb = |done: u64, total: Option<u64>| {
            ticks.borrow_mut().push(done);
            if total.is_some() {
                seen_total.set(total);
            }
        };
        let got = c
            .blob_with_progress_capped("sha256:whatever", Some(&cb), 1 << 20)
            .unwrap();

        assert_eq!(got.len(), payload.len());
        let ticks = ticks.into_inner();
        assert!(ticks.windows(2).all(|w| w[1] >= w[0]), "{ticks:?}");
        assert_eq!(ticks.last().copied(), Some(payload.len() as u64));
        assert_eq!(
            seen_total.get(),
            Some(payload.len() as u64),
            "o total tem de ser o do BLOB INTEIRO, não o do fragmento de um 206"
        );
    }

    /// The other half of the same contract, on the consumer side: the parallel
    /// pull's aggregate must not announce more bytes than were transferred. The
    /// layer here is deliberately several 64 KiB read-chunks long — with a
    /// single-chunk layer the old adapter (which added the running total in as
    /// if it were a delta) gave the right answer by accident.
    #[test]
    fn o_progresso_agregado_do_pull_nao_inventa_bytes() {
        let (port, _blob_gets, _handle) = serve_anon_registry();
        let tmp = std::env::temp_dir().join(format!(
            "delonix-image-progress-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let mut c = test_client(&format!("127.0.0.1:{port}"), "progresso-agregado");
        let config_bytes =
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#
                .to_vec();
        let config_digest = format!("sha256:{}", sha256_hex(&config_bytes));
        c.push_blob(&config_digest, &config_bytes).unwrap();

        let layer_bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let layer_digest = format!("sha256:{}", sha256_hex(&layer_bytes));
        c.push_blob(&layer_digest, &layer_bytes).unwrap();

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "size": config_bytes.len(),
                "digest": config_digest,
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "size": layer_bytes.len(),
                "digest": layer_digest,
            }],
        });
        c.push_manifest(
            "tag",
            &serde_json::to_vec(&manifest).unwrap(),
            "application/vnd.oci.image.manifest.v1+json",
        )
        .unwrap();

        let store = crate::ImageStore::open(&tmp).unwrap();
        let peak = std::sync::atomic::AtomicU64::new(0);
        let cb = |_l: usize, _lt: usize, done: u64, _t: Option<u64>| {
            peak.fetch_max(done, std::sync::atomic::Ordering::Relaxed);
        };
        super::pull_from_registry_with_creds_full(
            &store,
            &format!("127.0.0.1:{port}/progresso-agregado:tag"),
            None,
            None,
            Some(&cb),
        )
        .expect("o pull devia ter sucesso");

        let real = layer_bytes.len() as u64;
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::Relaxed),
            real,
            "o agregado tem de ser os bytes REALMENTE transferidos (a layer), \
             não a soma dos totais parciais"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
