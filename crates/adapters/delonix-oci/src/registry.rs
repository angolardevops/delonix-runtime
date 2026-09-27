//! Pull from an OCI registry (Docker Registry HTTP API V2).
//!
//! Supports Docker Hub by default (with an anonymous token) and any public
//! registry that uses the V2 protocol (ghcr.io, quay.io, registry.k8s.io, ...).
//! The flow: resolves the reference → manifest (picks the platform if it is a
//! multi-arch index) → config blob → layer blobs → stores in the CAS, just
//! like `load_docker_archive`.

use crate::cas::sha256_hex;
use crate::image::{now_unix, Image, ImageConfig, ImageStore};
use crate::{Error, Result};
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

/// Ceiling for a manifest or index body. Real ones are a few KiB; 4 MiB is
/// generous, and the point is that it is FINITE.
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
/// Ceiling for a non-registry feed fetched by [`http_get`] (the CVE feed).
const MAX_FEED_BYTES: u64 = 256 * 1024 * 1024;

/// Reads a response body up to `max` bytes and REFUSES anything longer.
///
/// `Response::bytes()` buffers whatever the server sends: a hostile or MITM'd
/// registry answering a manifest request with a multi-GiB chunked body made
/// the process allocate until OOM — before `verify_manifest_digest` ever ran.
/// Blobs already had this cap (`blob_with_progress_capped`); manifests did not.
fn read_capped(resp: reqwest::blocking::Response, max: u64, what: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    if resp.content_length().is_some_and(|n| n > max) {
        return Err(Error::Registry(format!(
            "{what} larger than the {max}-byte limit (Content-Length) — refusing"
        )));
    }
    let mut buf = Vec::new();
    resp.take(max + 1)
        .read_to_end(&mut buf)
        .map_err(|e| Error::Registry(format!("reading {what}: {e}")))?;
    if buf.len() as u64 > max {
        return Err(Error::Registry(format!(
            "{what} larger than the {max}-byte limit — refusing"
        )));
    }
    Ok(buf)
}

fn reg_err(e: reqwest::Error) -> Error {
    Error::Registry(e.to_string())
}

/// The same error, plus the one hint that turns it into an action.
///
/// A transport failure against `https://<host>` where the host was NOT declared
/// insecure is, in practice, almost always a registry that speaks plain HTTP —
/// and the bare `error sending request for url (https://…)` sends the reader
/// looking for a network problem that is not there. Measured on a production
/// cluster (2026-08-19): images could not be pulled from its own registry, and
/// the message named neither the cause nor the knob.
///
/// The hint is only added when it can be true: loopback and already-declared
/// hosts get the message unchanged, so it never invites an operator to open up
/// a registry that had nothing to do with the failure.
fn reg_err_with_hint(e: reqwest::Error, host: &str) -> Error {
    let declared = matches_insecure(host, &insecure_registries());
    let loopback = scheme_for(host) == "http";
    if declared || loopback || !e.is_connect() && !e.is_request() {
        return reg_err(e);
    }
    Error::Registry(format!(
        "{e}\n\nhint: {host} was contacted over HTTPS. If it serves plain HTTP \
         (a private registry on a trusted network often does), declare it: \
         DELONIX_INSECURE_REGISTRIES={host}"
    ))
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
/// A read the registry refuses to authorise (401/403).
fn is_denied(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN
}

fn extract(header: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let start = header.find(&pat)? + pat.len();
    let rest = &header[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Registries declared as plain-HTTP by the operator, via
/// `DELONIX_INSECURE_REGISTRIES` (comma-separated, `host` or `host:port`).
///
/// Same knob as `--insecure-registry` (Docker) and `certs.d` (containerd), and
/// it exists for the same reason: a private registry on a trusted network
/// frequently serves HTTP, and a client that cannot be told so simply cannot
/// pull from it. Measured 2026-08-19 on a production cluster — not a single
/// image could be pulled from its OWN registry, because
/// the registry serves HTTP and this client insisted on HTTPS; the API
/// answered `registry error: error sending request` and the image store stayed
/// empty. A knob nobody can reach is not a security boundary, it is a wall.
///
/// Explicit opt-in per host, never a range and never a "detect the LAN" rule:
/// silently downgrading a connection because an address looks private is how a
/// credential ends up on the wire in a place the operator never inspected.
fn insecure_registries() -> Vec<String> {
    std::env::var("DELONIX_INSECURE_REGISTRIES")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|e| e.trim().to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The host part of a registry authority, with the port removed.
///
/// Not a `split(':')`, and the difference is a bug that predates this function:
/// an IPv6 authority is bracketed (`[::1]:5000`), so splitting on the first
/// colon yields `"["` and the loopback comparison below could NEVER match a
/// bracketed address with a port. The literal `"[::1]"` was in the list of
/// loopback hosts and was unreachable — a check that reads as covered and is
/// not. Caught by the test `loopback_is_http_without_any_declaration`.
fn bare_host(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return match rest.find(']') {
            Some(i) => &rest[..i], // `[::1]:5000` -> `::1`
            None => authority,
        };
    }
    authority.split(':').next().unwrap_or(authority)
}

/// Does `host` (as written in the image reference, so possibly `host:port`)
/// match one of the declared entries?
///
/// An entry WITH a port matches only that port; an entry WITHOUT one matches
/// the host on any port. That asymmetry is deliberate and is what Docker does:
/// `registry.lan:5000` is a promise about one endpoint, `registry.lan` is a
/// promise about a machine.
///
/// Both sides are lowered here, and not only at the parsing of the environment
/// variable: this function is also the one the tests call, and a comparison
/// that is case-sensitive on one side only is a knob that looks set and is not.
fn matches_insecure(host: &str, entries: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    let bare = bare_host(&host);
    entries.iter().any(|raw| {
        let e = raw.to_ascii_lowercase();
        e == host || (!e.contains(':') && e == bare)
    })
}

/// The HTTP scheme for a registry: `http` for the loopback family and for
/// registries the operator declared insecure, `https` for all others — the
/// same rule as Docker/containerd.
///
/// Pure over its inputs so the decision can be tested without touching the
/// process environment: `scheme_for` is the thin wrapper that reads it.
fn scheme_for_with(host: &str, insecure: &[String]) -> &'static str {
    let h = bare_host(host);
    if h == "localhost" || h == "127.0.0.1" || h == "::1" {
        return "http";
    }
    if matches_insecure(host, insecure) {
        return "http";
    }
    "https"
}

fn scheme_for(host: &str) -> &'static str {
    scheme_for_with(host, &insecure_registries())
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
        // `_with_hint` here and in `write_req` only: these are the first two
        // contacts with the registry, and a plain-HTTP registry fails right
        // there — the transport gives up before there is any state to report.
        let resp = self
            .send_once(url, accept, from)
            .map_err(|e| reg_err_with_hint(e, &self.host))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let www = resp
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            self.token = Some(match self.request_token(&www, None)? {
                Ok(token) => token,
                // A read the registry will not authorise: it answers this way for
                // a repository that does not exist as well as for one these
                // credentials cannot see (ghcr: 403 on the token), and it will
                // not say which. The same answer as a 404 on a private one.
                Err(status) if is_denied(status) => return Err(self.not_visible()),
                Err(status) => {
                    return Err(Error::Registry(format!(
                        "failed to obtain token: HTTP {status}"
                    )))
                }
            });
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
                Err(Error::NotVisible(format!(
                    "repository {} — it does not exist, or it is private and these \
                     credentials cannot see it (`delonix image login <registry>`)",
                    self.repo
                )))
            } else {
                Err(Error::NoSuchImage(format!(
                    "{}:{}",
                    self.repo,
                    url.rsplit('/').next().unwrap_or("")
                )))
            }
        } else if is_denied(status) {
            // Reached only with the token already obtained: Docker Hub answers
            // 401 here for a repository that does not exist.
            Err(self.not_visible())
        } else {
            Err(Error::Registry(format!("HTTP {status} at {url}")))
        }
    }

    /// The honest answer to a read the registry refuses to authorise.
    fn not_visible(&self) -> Error {
        Error::NotVisible(format!(
            "image {} — it does not exist, or it is private and these credentials \
             cannot see it (`delonix image login <registry>`)",
            self.repo
        ))
    }

    /// Requests a token from the authentication service indicated in the 401. With
    /// `force_scope`, requests that scope (e.g. `…:pull,push` for the `push`) instead
    /// of the one indicated by the server — the server grants it if the credentials
    /// allow it.
    fn get_token(&self, www: &str, force_scope: Option<&str>) -> Result<String> {
        self.request_token(www, force_scope)?
            .map_err(|status| Error::Registry(format!("failed to obtain token: HTTP {status}")))
    }

    /// [`Self::get_token`], with a refused token request kept as its HTTP status
    /// so a read can tell «not allowed» from a broken registry.
    fn request_token(
        &self,
        www: &str,
        force_scope: Option<&str>,
    ) -> Result<std::result::Result<String, reqwest::StatusCode>> {
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
            return Ok(Err(resp.status()));
        }
        let v: serde_json::Value = resp.json().map_err(reg_err)?;
        v.get("token")
            .or_else(|| v.get("access_token"))
            .and_then(|t| t.as_str())
            .map(|t| Ok(t.to_string()))
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
        let mut sink = MemSink {
            buf: Vec::new(),
            max: max_bytes,
        };
        self.fetch_blob_into(digest, progress, max_bytes, &mut sink)?;
        Ok(sink.buf)
    }

    /// Downloads a blob straight into the CAS: streamed to a scratch file in
    /// the store, hashed on the way through, and renamed into place only once
    /// the content hashes to `digest`. Nothing is buffered whole — the RSS of
    /// a layer in flight is one read chunk, not the layer — and the write to
    /// disk overlaps the download instead of starting after the last byte.
    ///
    /// A digest mismatch is [`Error::DigestMismatch`] and nothing is left in
    /// the store; the scratch file is removed on every failure.
    fn blob_into_cas(
        &mut self,
        cas: &crate::cas::Cas,
        digest: &str,
        progress: Option<&dyn Fn(u64, Option<u64>)>,
    ) -> Result<()> {
        let tmp = cas.tmp_path();
        let result = (|| {
            let mut sink = crate::cas::StreamingBlob::create(&tmp)?;
            self.fetch_blob_into(digest, progress, Self::MAX_BLOB_BYTES, &mut sink)?;
            let got = sink.finish()?;
            let want = crate::cas::strip(digest);
            if got != want {
                return Err(Error::DigestMismatch(format!(
                    "blob {digest}: content hashes to sha256:{got}"
                )));
            }
            cas.adopt(&tmp, want)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// The download loop shared by [`Self::blob_with_progress_capped`] (into
    /// memory) and [`Self::blob_into_cas`] (into a file): resume by `Range`,
    /// the three ways a server may not honour it, the size cap, and the
    /// backoff all live here once.
    fn fetch_blob_into<S: BlobSink>(
        &mut self,
        digest: &str,
        progress: Option<&dyn Fn(u64, Option<u64>)>,
        max_bytes: u64,
        sink: &mut S,
    ) -> Result<()> {
        use std::io::Read;
        let url = format!(
            "{}://{}/v2/{}/blobs/{}",
            scheme_for(&self.host),
            self.host,
            self.repo,
            digest
        );
        // The sink is kept ACROSS attempts: this is what "resume" means here —
        // a retry continues from what it already holds (memory or file).
        // Size of the WHOLE blob. Not `content_length()` on a resumed request:
        // a 206's Content-Length is the length of the FRAGMENT, so taking it
        // would make the progress bar restart against a shrinking total. On a
        // resume the whole size comes from the `/<total>` of Content-Range.
        let mut total: Option<u64> = None;
        let mut last_err = String::new();

        for attempt in 1..=Self::BLOB_ATTEMPTS {
            let from = (sink.len() > 0).then_some(sink.len());
            if attempt > 1 {
                // Backoff, and a line saying what is happening: without it a
                // resumed pull on a slow link is indistinguishable from a hang,
                // which is the complaint that started this.
                std::thread::sleep(Duration::from_secs(1 << (attempt - 2).min(3)));
                tracing::warn!(
                    have = sink.len(),
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
                    if from.is_none() || e.is_not_found() {
                        return Err(e);
                    }
                    last_err = delonix_model::Error::from(e).to_string();
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
                total = resp.content_length();
                sink.reset(total)?;
            }

            let mut chunk = [0u8; 65536];
            let mut broke = false;
            loop {
                let n = match resp.read(&mut chunk) {
                    // Whatever the sink holds stays: the next attempt continues
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
                if sink.len() + n as u64 > max_bytes {
                    return Err(Error::Registry(format!(
                        "blob {digest} exceeds the {max_bytes}-byte limit — aborted"
                    )));
                }
                sink.append(&chunk[..n])?;
                if let Some(p) = progress {
                    p(sink.len(), total);
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
                if sink.len() < t {
                    last_err = format!("connection closed at {} of {t} bytes", sink.len());
                    continue;
                }
            }
            return Ok(());
        }

        Err(Error::Registry(format!(
            "blob {digest}: gave up after {} attempts with {} of {} bytes — last error: {last_err}",
            Self::BLOB_ATTEMPTS,
            sink.len(),
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
        self.write_req_try(&|http| Ok(build(http)))
    }

    /// [`Self::write_req`] with a builder that may fail — the one that opens
    /// a blob's FILE for the body, once per attempt, instead of holding the
    /// blob in memory to resend it.
    fn write_req_try(
        &mut self,
        build: &dyn Fn(&reqwest::blocking::Client) -> Result<reqwest::blocking::RequestBuilder>,
    ) -> Result<reqwest::blocking::Response> {
        self.write_req_raw(build)?
            .map_err(|e| reg_err_with_hint(e, &self.host))
    }

    /// [`Self::write_req_try`], with a TRANSPORT failure (connection refused
    /// or reset, timeout, body cut) kept as the raw `reqwest::Error` in the
    /// inner `Result` — the upload retry needs to tell it apart from a
    /// registry that answered. The outer `Result` carries what is not
    /// transport: the body could not be built, or no token was granted.
    fn write_req_raw(
        &mut self,
        build: &dyn Fn(&reqwest::blocking::Client) -> Result<reqwest::blocking::RequestBuilder>,
    ) -> Result<std::result::Result<reqwest::blocking::Response, reqwest::Error>> {
        let send = |http: &reqwest::blocking::Client, token: &Option<String>| -> Result<_> {
            let mut req = build(http)?;
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            Ok(req.send())
        };
        let resp = match send(&self.http, &self.token)? {
            Ok(r) => r,
            Err(e) => return Ok(Err(e)),
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let www = resp
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let scope = format!("repository:{}:pull,push", self.repo);
            self.token = Some(self.get_token(&www, Some(&scope))?);
            return send(&self.http, &self.token);
        }
        Ok(Ok(resp))
    }

    /// Sends a small blob held in memory (a config, a signature).
    fn push_blob(&mut self, digest: &str, data: &[u8]) -> Result<()> {
        self.push_blob_with(digest, &|| Ok(reqwest::blocking::Body::from(data.to_vec())))
    }

    /// Sends a blob STRAIGHT FROM ITS FILE: the body is read from disk as it
    /// goes out, and the file is reopened if a 401 forces a second attempt.
    /// The whole blob used to be in memory three times over (the read, a
    /// `to_vec`, and a `clone` per attempt) — ~6 GiB for a 2 GiB VM image.
    fn push_blob_file(
        &mut self,
        digest: &str,
        path: &std::path::Path,
        size: u64,
        meter: Option<&MeterSlot>,
    ) -> Result<()> {
        self.push_blob_with(digest, &|| {
            let body = MeteredFile {
                file: std::fs::File::open(path)?,
                pos: 0,
                meter: meter.cloned(),
            };
            Ok(reqwest::blocking::Body::sized(body, size))
        })?;
        // Done, including a blob the registry already had (HEAD) or whose
        // answer was lost: the bar must not stop short of it.
        if let Some(m) = meter {
            m.set(size);
        }
        Ok(())
    }

    /// How many times one blob's upload is tried. Same budget as the pull's
    /// `BLOB_ATTEMPTS`: enough to ride out a link that drops a connection every
    /// few minutes, not so many that a broken transfer takes all afternoon to
    /// say so.
    const PUSH_ATTEMPTS: u32 = 5;

    /// Uploads a blob, retrying what a retry can fix. The push had no retry at
    /// all: a connection that dropped at 90% of a 2 GiB image meant running
    /// the command again and sending all of it.
    ///
    /// Every attempt starts with a `HEAD`: a blob the registry already holds
    /// is done — including one whose `PUT` was accepted but whose answer was
    /// lost on the way back, which must not be sent twice. A transport failure
    /// and a 5xx/408/429 are retried with backoff, on a NEW session (a failed
    /// `PUT` usually invalidates its upload URL); any other answer (400 for a
    /// digest mismatch, 403) fails at once, since sending the same bytes again
    /// cannot change it. `body` is called once per attempt.
    fn push_blob_with(
        &mut self,
        digest: &str,
        body: &dyn Fn() -> Result<reqwest::blocking::Body>,
    ) -> Result<()> {
        let mut last = String::new();
        for attempt in 1..=Self::PUSH_ATTEMPTS {
            if attempt > 1 {
                std::thread::sleep(Duration::from_secs(1 << (attempt - 2).min(3)));
                tracing::warn!(attempt, "retrying the upload of {digest} after: {last}");
            }
            let outcome = match self.blob_present(digest) {
                Ok(true) => return Ok(()),
                Ok(false) => self.upload_once(digest, body),
                Err(f) => Err(f),
            };
            match outcome {
                Ok(()) => return Ok(()),
                Err(UploadFailure::Retry(why)) => last = why,
                Err(UploadFailure::Fatal(e)) => return Err(e),
            }
        }
        Err(Error::Registry(format!(
            "blob {digest}: upload gave up after {} attempts — last error: {last}",
            Self::PUSH_ATTEMPTS
        )))
    }

    /// `HEAD` of a blob for the upload loop: `true` if the registry has it,
    /// a transport failure or a 5xx/408/429 as retryable.
    fn blob_present(&mut self, digest: &str) -> std::result::Result<bool, UploadFailure> {
        let url = format!(
            "{}://{}/v2/{}/blobs/{}",
            scheme_for(&self.host),
            self.host,
            self.repo,
            digest
        );
        match self.write_req_raw(&|http| Ok(http.head(&url))) {
            Err(e) => Err(UploadFailure::Fatal(e)),
            Ok(Err(e)) => Err(UploadFailure::Retry(format!("blob HEAD: {e}"))),
            Ok(Ok(r)) if retryable_status(r.status()) => Err(UploadFailure::Retry(format!(
                "blob HEAD: HTTP {}",
                r.status()
            ))),
            Ok(Ok(r)) => Ok(r.status().is_success()),
        }
    }

    /// One monolithic upload: `POST` to open the session, then
    /// `PUT …?digest=<sha256>` with the body `body` produces.
    fn upload_once(
        &mut self,
        digest: &str,
        body: &dyn Fn() -> Result<reqwest::blocking::Body>,
    ) -> std::result::Result<(), UploadFailure> {
        let start = format!(
            "{}://{}/v2/{}/blobs/uploads/",
            scheme_for(&self.host),
            self.host,
            self.repo
        );
        let resp = match self.write_req_raw(&|http| Ok(http.post(&start))) {
            Err(e) => return Err(UploadFailure::Fatal(e)),
            Ok(Err(e)) => {
                return Err(UploadFailure::Retry(format!("upload start: {e}")));
            }
            Ok(Ok(r)) => r,
        };
        if resp.status() != reqwest::StatusCode::ACCEPTED {
            let status = resp.status();
            let msg = format!(
                "upload start: HTTP {status} (run `delonix login {}`?)",
                self.host
            );
            return Err(if retryable_status(status) {
                UploadFailure::Retry(msg)
            } else {
                UploadFailure::Fatal(Error::Registry(msg))
            });
        }
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                UploadFailure::Fatal(Error::Registry("upload without Location header".into()))
            })?
            .to_string();
        // Location may come absolute or relative to the host.
        let base = if location.starts_with("http") {
            location
        } else {
            format!("{}://{}{}", scheme_for(&self.host), self.host, location)
        };
        let sep = if base.contains('?') { '&' } else { '?' };
        let put_url = format!("{base}{sep}digest={digest}");
        let resp = match self.write_req_raw(&|http| {
            Ok(http
                .put(&put_url)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(body()?))
        }) {
            Err(e) => return Err(UploadFailure::Fatal(e)),
            Ok(Err(e)) => return Err(UploadFailure::Retry(format!("blob PUT {digest}: {e}"))),
            Ok(Ok(r)) => r,
        };
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else if retryable_status(status) {
            Err(UploadFailure::Retry(format!(
                "blob PUT {digest}: HTTP {status}"
            )))
        } else {
            Err(UploadFailure::Fatal(Error::Registry(format!(
                "blob PUT {digest}: HTTP {status}"
            ))))
        }
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
            let detail = read_capped(resp, 64 * 1024, "error body")
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
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

/// Progress of an upload: `(bytes sent, bytes to send)`, called as the bodies
/// go out. Shared across threads, since layers upload in parallel.
pub type PushProgress = std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>;

/// The aggregate progress of an upload of several blobs. Each blob owns a
/// slot holding its OWN position, set (not added to) as its body is read —
/// so a retried upload, which reads the file from zero again, moves its slot
/// back instead of counting the same bytes twice.
struct PushMeter {
    sent: Vec<std::sync::atomic::AtomicU64>,
    total: u64,
    report: PushProgress,
}

impl PushMeter {
    fn new(slots: usize, total: u64, report: PushProgress) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            sent: (0..slots)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
            total,
            report,
        })
    }
}

/// One blob's place in a [`PushMeter`].
#[derive(Clone)]
struct MeterSlot {
    meter: std::sync::Arc<PushMeter>,
    slot: usize,
}

impl MeterSlot {
    fn set(&self, pos: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.meter.sent[self.slot].store(pos, Relaxed);
        let done: u64 = self.meter.sent.iter().map(|a| a.load(Relaxed)).sum();
        // Not clamped to the total: a retry resets its slot, so the sum can
        // never pass it — and a clamp would hide exactly the double count a
        // missing reset would produce.
        (self.meter.report)(done, self.meter.total);
    }
}

/// A blob's file as the body of its upload, counting what has gone out.
struct MeteredFile {
    file: std::fs::File,
    pos: u64,
    meter: Option<MeterSlot>,
}

impl std::io::Read for MeteredFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read(buf)?;
        self.pos += n as u64;
        if let Some(m) = &self.meter {
            m.set(self.pos);
        }
        Ok(n)
    }
}

/// What one blob-upload attempt ended in, when it did not succeed.
enum UploadFailure {
    /// Transport failure, or a 5xx/408/429 — another attempt may succeed.
    Retry(String),
    /// Anything a retry cannot change.
    Fatal(Error),
}

/// Registry answers worth another attempt: server errors, and the two that
/// mean "not now" (request timeout, too many requests).
fn retryable_status(s: reqwest::StatusCode) -> bool {
    s.is_server_error()
        || s == reqwest::StatusCode::REQUEST_TIMEOUT
        || s == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// Where a downloaded blob goes: `len` is what a resume continues from,
/// `reset` starts over (the server ignored the `Range`), `append` takes the
/// next chunk.
trait BlobSink {
    fn len(&self) -> u64;
    fn reset(&mut self, size_hint: Option<u64>) -> Result<()>;
    fn append(&mut self, bytes: &[u8]) -> Result<()>;
}

/// The whole blob in memory — for the callers that need the bytes
/// (manifests' neighbours, signatures, VM artifacts until they stream).
struct MemSink {
    buf: Vec<u8>,
    max: u64,
}

impl BlobSink for MemSink {
    fn len(&self) -> u64 {
        self.buf.len() as u64
    }
    fn reset(&mut self, size_hint: Option<u64>) -> Result<()> {
        self.buf.clear();
        // Capped: the hint is the registry's UNTRUSTED Content-Length.
        if let Some(t) = size_hint {
            self.buf.reserve(t.min(self.max) as usize);
        }
        Ok(())
    }
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(bytes);
        Ok(())
    }
}

impl BlobSink for crate::cas::StreamingBlob {
    fn len(&self) -> u64 {
        crate::cas::StreamingBlob::len(self)
    }
    fn reset(&mut self, _size_hint: Option<u64>) -> Result<()> {
        crate::cas::StreamingBlob::reset(self)
    }
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        crate::cas::StreamingBlob::append(self, bytes)
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
        read_capped(resp, MAX_MANIFEST_BYTES, "manifest")
    }
    /// Raw bytes of a blob (by digest).
    pub fn get_blob(&mut self, digest: &str) -> Result<Vec<u8>> {
        self.inner.blob(digest)
    }
    /// Publishes a single-blob artifact under `tag` in THIS client's
    /// repository, with `layer_annotations` on its layer descriptor — see
    /// [`push_oci_artifact_with_layer_annotations`], which does the same on a
    /// client of its own.
    pub fn push_signature(
        &mut self,
        tag: &str,
        layer_media_type: &str,
        data: &[u8],
        layer_annotations: &BTreeMap<String, String>,
    ) -> Result<String> {
        push_layer_annotated(
            &mut self.inner,
            tag,
            layer_media_type,
            data,
            layer_annotations,
        )
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
    read_capped(resp, MAX_FEED_BYTES, "feed")
}

/// A local image, or the image pulled from its registry when there is none.
///
/// `announce` is called with the reference right before a pull starts, so the
/// caller can say so in its own words; nothing is announced for a local hit.
/// `creds: None` reads the credential vault (`image login`); `Some` uses the
/// caller's pair instead, which is how a manifest names its own credential.
pub fn resolve_or_pull(
    store: &ImageStore,
    reference: &str,
    creds: Option<(String, String)>,
    announce: &dyn Fn(&str),
) -> Result<Image> {
    if let Ok(img) = store.resolve(reference) {
        return Ok(img);
    }
    announce(reference);
    pull_from_registry_with_creds(store, reference, creds)
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
            return Err(Error::DigestMismatch(format!(
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
    let body = read_capped(resp, MAX_MANIFEST_BYTES, "manifest")?;
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
        let sub = read_capped(r, MAX_MANIFEST_BYTES, "sub-manifest")?;
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
    //
    // The config is fetched ALONGSIDE the layers, not before them: it is a
    // few KiB, but fetching it first put a full round-trip (and, on a loaded
    // disk, a multi-second write — measured 2.5s for 5.6 KiB) in front of
    // every layer download.
    let config_digest = manifest.config().digest().to_string();
    let need_config = !store.cas().has(&config_digest);

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

    if !missing.is_empty() || need_config {
        // LAYERS IN PARALLEL, and this is the difference between a pull that
        // saturates a link and one that does not. Measured on this host, same
        // origin, same total bytes: one connection 0.46 MiB/s, four in parallel
        // 1.45 MiB/s aggregate — 3.2x. The ceiling is PER CONNECTION, so a
        // sequential `for` over the layers leaves most of the link idle. This
        // was a plain sequential loop.
        //
        // The cap is small on purpose: a registry throttles per-client, and
        // more sockets past the point the link saturates buys nothing while
        // making a 429 more likely. Memory is no longer what bounds it: each
        // layer streams to a scratch file in the CAS (`blob_into_cas`), so a
        // worker holds one read chunk, not the layer.
        let workers = missing.len().min(4);
        let done_bytes = std::sync::atomic::AtomicU64::new(0);
        let done_layers = std::sync::atomic::AtomicUsize::new(0);
        let next = std::sync::Mutex::new(missing.clone().into_iter());
        let errors = std::sync::Mutex::new(Vec::<String>::new());
        let config_result = std::sync::Mutex::new(None::<Result<()>>);

        std::thread::scope(|scope| {
            if need_config {
                let mut cc = c.clone();
                let (config_digest, config_result) = (&config_digest, &config_result);
                let store = &store;
                scope.spawn(move || {
                    let res = match cc.blob_into_cas(store.cas(), config_digest, None) {
                        Err(Error::DigestMismatch(_)) => {
                            Err(Error::DigestMismatch("config digest mismatch".into()))
                        }
                        other => other,
                    };
                    *config_result.lock().unwrap() = Some(res);
                });
            }
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
                        cw.blob_into_cas(store.cas(), &dg, Some(&adapter))
                    } else {
                        cw.blob_into_cas(store.cas(), &dg, None)
                    };
                    match res {
                        Ok(()) => {
                            done_layers.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        Err(Error::DigestMismatch(_)) => errors
                            .lock()
                            .unwrap()
                            .push(format!("corrupted layer: {dg}")),
                        Err(e) => errors
                            .lock()
                            .unwrap()
                            .push(format!("layer {dg}: {}", delonix_model::Error::from(e))),
                    }
                });
            }
        });

        if let Some(Err(e)) = config_result.into_inner().unwrap() {
            return Err(e);
        }
        // Every failure, not just the first: a pull that dies on three layers
        // and names one sends the reader looking at the wrong thing.
        let errs = errors.into_inner().unwrap();
        if !errs.is_empty() {
            return Err(Error::Registry(errs.join("; ")));
        }
    }

    let config_bytes = store.cas().read(&config_digest)?;

    // 4) assemble and store — read the runtime config (Cmd/Env/Entrypoint/User/WorkingDir)
    // from the OCI config blob (`oci_spec::image::ImageConfiguration`).
    let oci_config: ImageConfiguration = serde_json::from_slice(&config_bytes)?;
    let inner = oci_config.config().clone().unwrap_or_default();
    let repo_tags = store.merged_tags(&config_digest, reference);
    let image = crate::image::Image {
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
    // Size from the file's metadata and the media type from its first bytes:
    // reading every layer whole just to learn those two facts made a push (and
    // `write_oci_archive`) read each layer from disk twice.
    let cas = store.cas();
    let config_desc = descriptor(
        DOCKER_CONFIG_MEDIA_TYPE,
        cas.size(&image.id)? as usize,
        &image.id,
    )?;
    let mut layer_descs = Vec::with_capacity(image.layers.len());
    for dg in &image.layers {
        let head = cas.head(dg, 4)?;
        layer_descs.push(descriptor(
            layer_media_type(&head),
            cas.size(dg)? as usize,
            dg,
        )?);
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
    push_to_registry_with_progress(store, source, target, None)
}

/// Layers uploaded at once. The same cap as the pull, for the same reason: a
/// registry throttles per client, and past the point the link saturates more
/// sockets only make a 429 likelier.
const PUSH_WORKERS: usize = 4;

/// [`push_to_registry`], reporting `(bytes sent, bytes to send)` for the
/// layers as they go out.
pub fn push_to_registry_with_progress(
    store: &ImageStore,
    source: &str,
    target: &str,
    progress: Option<PushProgress>,
) -> Result<String> {
    let image = store.resolve(source)?;
    let (host, repo, refr) = parse_reference(target);
    // A layer is a transfer of unknown size, like a VM artifact: a fixed
    // whole-request ceiling (it was 300s here) failed every layer that could
    // not move in five minutes, however healthy the link. See `transfer_client`.
    let http = transfer_client()?;
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

    // 2) the layers (those missing from the registry), IN PARALLEL: they went
    // one after another, so an image of N layers took the sum of their times
    // on a link whose ceiling is per connection — the pull already measured
    // that four connections move 3.2x what one does. Each straight from its
    // CAS file, never the whole layer in memory.
    let sizes = image
        .layers
        .iter()
        .map(|dg| store.cas().size(dg))
        .collect::<Result<Vec<u64>>>()?;
    let meter = progress.map(|cb| PushMeter::new(sizes.len(), sizes.iter().sum(), cb));
    let workers = image.layers.len().min(PUSH_WORKERS);
    let next = std::sync::Mutex::new(image.layers.iter().enumerate());
    let errors = std::sync::Mutex::new(Vec::<String>::new());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            // A clone per worker, as in the pull: the connection pool is
            // shared, and a clone starts with the token already obtained.
            let mut cw = c.clone();
            let (next, errors, sizes, meter) = (&next, &errors, &sizes, &meter);
            let store = &store;
            scope.spawn(move || loop {
                let Some((i, dg)) = next.lock().unwrap().next() else {
                    return;
                };
                tracing::debug!(
                    index = i + 1,
                    total = sizes.len(),
                    digest = %&dg[..dg.len().min(19)],
                    "pushing layer {}/{}",
                    i + 1,
                    sizes.len()
                );
                let slot = meter.as_ref().map(|m| MeterSlot {
                    meter: m.clone(),
                    slot: i,
                });
                if let Err(e) = cw.push_blob_file(
                    &with_prefix(dg),
                    &store.cas().path(dg),
                    sizes[i],
                    slot.as_ref(),
                ) {
                    errors
                        .lock()
                        .unwrap()
                        .push(format!("layer {dg}: {}", delonix_model::Error::from(e)));
                }
            });
        }
    });
    // Every failure, not just the first — the same rule as the pull's.
    let errs = errors.into_inner().unwrap();
    if !errs.is_empty() {
        return Err(Error::Registry(errs.join("; ")));
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
    push_artifact(
        root,
        target,
        layer_media_type,
        ArtifactLayer::Bytes(data),
        annotations,
    )
}

/// Like [`push_oci_artifact_with_annotations`], but the blob is a FILE and
/// goes out straight from disk: hashed in 1 MiB chunks and streamed as the
/// body, never held in memory. What `vm push` uses — a VM image is GiBs.
pub fn push_oci_artifact_file(
    root: &std::path::Path,
    target: &str,
    layer_media_type: &str,
    path: &std::path::Path,
    annotations: &BTreeMap<String, String>,
    progress: Option<PushProgress>,
) -> Result<String> {
    push_artifact(
        root,
        target,
        layer_media_type,
        ArtifactLayer::File(path, progress),
        annotations,
    )
}

/// Where the single layer of an artifact comes from.
enum ArtifactLayer<'a> {
    Bytes(&'a [u8]),
    File(&'a std::path::Path, Option<PushProgress>),
}

fn push_artifact(
    root: &std::path::Path,
    target: &str,
    layer_media_type: &str,
    layer: ArtifactLayer<'_>,
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

    let (layer_digest, layer_size) = match &layer {
        ArtifactLayer::Bytes(data) => (with_prefix(&sha256_hex(data)), data.len() as u64),
        ArtifactLayer::File(path, _) => (
            with_prefix(&crate::cas::sha256_file(path)?),
            std::fs::metadata(path)?.len(),
        ),
    };
    tracing::debug!(
        digest = %&layer_digest[..19.min(layer_digest.len())],
        bytes = layer_size,
        "pushing blob"
    );
    match layer {
        ArtifactLayer::Bytes(data) => c.push_blob(&layer_digest, data)?,
        ArtifactLayer::File(path, progress) => {
            let slot = progress.map(|cb| MeterSlot {
                meter: PushMeter::new(1, layer_size, cb),
                slot: 0,
            });
            c.push_blob_file(&layer_digest, path, layer_size, slot.as_ref())?
        }
    }

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
            layer_size as usize,
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

/// Like [`push_oci_artifact_with_annotations`], but the annotations land on
/// the LAYER descriptor rather than the manifest — what `image sign` needs
/// for a cosign-compatible signature artifact: `verify_signature` (`sign.rs`)
/// reads the signature from `sig_manifest.layers[].annotations`, the OCI
/// place a descriptor's own metadata lives, never the manifest's top-level
/// `annotations`. A dedicated function rather than overloading the existing
/// one: [`push_oci_artifact_with_annotations`] already has real callers (VM
/// image metadata) that expect manifest-level annotations, and changing what
/// its one parameter means for a second, unrelated caller is how two
/// call sites end up disagreeing about the same field.
pub fn push_oci_artifact_with_layer_annotations(
    root: &std::path::Path,
    target: &str,
    layer_media_type: &str,
    data: &[u8],
    layer_annotations: &BTreeMap<String, String>,
) -> Result<String> {
    let (host, repo, refr) = parse_reference(target);
    let http = transfer_client()?;
    let creds = crate::auth::lookup(root, &host);
    let mut c = Client {
        http,
        host,
        repo,
        token: None,
        creds,
    };
    push_layer_annotated(&mut c, &refr, layer_media_type, data, layer_annotations)
}

/// The body of [`push_oci_artifact_with_layer_annotations`], on a client the
/// caller already has — `image sign` reuses the one that read the manifest
/// it is signing, instead of a second client with its own TLS handshake and
/// token flow for the same repository.
fn push_layer_annotated(
    c: &mut Client,
    refr: &str,
    layer_media_type: &str,
    data: &[u8],
    layer_annotations: &BTreeMap<String, String>,
) -> Result<String> {
    let (host, repo) = (c.host.clone(), c.repo.clone());
    tracing::info!(repo = %repo, reference = %refr, host = %host, "pushing signature artifact {repo}:{refr} to {host}");

    let config_digest = with_prefix(&sha256_hex(EMPTY_CONFIG_BYTES));
    c.push_blob(&config_digest, EMPTY_CONFIG_BYTES)?;

    let layer_digest = with_prefix(&sha256_hex(data));
    c.push_blob(&layer_digest, data)?;

    let layer_descriptor = DescriptorBuilder::default()
        .media_type(MediaType::from(layer_media_type))
        .size(data.len() as u64)
        .digest(Digest::from_str(&layer_digest).map_err(oci_err)?)
        .annotations(
            layer_annotations
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<std::collections::HashMap<String, String>>(),
        )
        .build()
        .map_err(oci_err)?;

    let manifest = ImageManifestBuilder::default()
        .schema_version(2u32)
        .media_type(MediaType::ImageManifest)
        .artifact_type(MediaType::from(layer_media_type))
        .config(descriptor(
            EMPTY_CONFIG_MEDIA_TYPE,
            EMPTY_CONFIG_BYTES.len(),
            &config_digest,
        )?)
        .layers(vec![layer_descriptor])
        .build()
        .map_err(oci_err)?;
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    c.push_manifest(refr, &manifest_bytes, MediaType::ImageManifest.as_ref())?;

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
    let bytes = read_capped(
        c.fetch(&url, "application/vnd.oci.image.manifest.v1+json")?,
        MAX_MANIFEST_BYTES,
        "manifest",
    )?;
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
    let manifest_bytes = read_capped(c.fetch(&url, accept)?, MAX_MANIFEST_BYTES, "manifest")?;
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
        return Err(Error::DigestMismatch(format!(
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

/// What [`pull_oci_artifact_to_file`] left on disk.
#[derive(Debug)]
pub struct PulledArtifact {
    /// `sha256:<hex>` of the blob — verified against the manifest.
    pub digest: String,
    /// Size in bytes.
    pub size: u64,
    /// The manifest annotations (see [`pull_oci_artifact_with_meta`]).
    pub annotations: BTreeMap<String, String>,
    /// `false` when `dest` already held exactly this blob and nothing was
    /// downloaded.
    pub downloaded: bool,
}

/// Pulls a single-blob artifact (a VM image) STRAIGHT TO `dest`, never
/// holding it in memory. [`pull_oci_artifact_with_meta`] buffered the whole
/// blob (GiBs for an appliance), hashed it twice, and wrote it in place, so a
/// crash mid-write left a truncated qcow2 under the final name.
///
/// - The blob streams into `<dest>.<digest12>.download`, hashed on the way
///   through, and is renamed onto `dest` only once it matches the manifest.
///   The digest is in the partial's name so a partial of an EARLIER version
///   of the tag is never stitched onto this one.
/// - A partial left by an earlier process is RESUMED: its prefix is hashed
///   again and the download continues by `Range` — on a slow link, an
///   interrupted pull no longer starts over in the next process.
/// - When `dest` already holds this exact blob (same size and digest),
///   nothing is downloaded.
/// - A digest mismatch deletes the partial (bad bytes are never resumed); a
///   transport failure keeps it for the next attempt.
pub fn pull_oci_artifact_to_file(
    root: &std::path::Path,
    source: &str,
    dest: &std::path::Path,
    progress: Option<&dyn Fn(u64, Option<u64>)>,
) -> Result<PulledArtifact> {
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
    let manifest_bytes = read_capped(c.fetch(&url, accept)?, MAX_MANIFEST_BYTES, "manifest")?;
    // Same pin check as the in-memory path: a compromised registry must not be
    // able to swap the whole manifest under a `@sha256:` reference.
    verify_manifest_digest(&refr, &manifest_bytes)?;
    let manifest: ImageManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| Error::Registry(format!("invalid artifact manifest: {e}")))?;
    let layer = manifest
        .layers()
        .first()
        .ok_or_else(|| Error::Registry("artifact manifest has no layers".into()))?;
    let layer_digest = layer.digest().to_string();
    let hex = crate::cas::strip(&layer_digest).to_string();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Registry(format!(
            "artifact layer is not a sha256 blob: {layer_digest}"
        )));
    }
    let size = layer.size();
    // Read after the manifest checks, like the in-memory path; they are only
    // returned once the blob itself has been verified.
    let annotations: BTreeMap<String, String> = manifest
        .annotations()
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect();

    if std::fs::metadata(dest)
        .map(|m| m.len() == size)
        .unwrap_or(false)
        && crate::cas::sha256_file(dest)? == hex
    {
        return Ok(PulledArtifact {
            digest: layer_digest,
            size,
            annotations,
            downloaded: false,
        });
    }

    let dir = dest.parent().unwrap_or(std::path::Path::new("."));
    let fname = dest
        .file_name()
        .ok_or_else(|| Error::Registry(format!("not a file path: {}", dest.display())))?
        .to_string_lossy()
        .into_owned();
    let partial = dir.join(format!("{fname}.{}.download", &hex[..12]));
    // Partials of other versions of this same file: never resumable into
    // this blob, and a GiB each.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with(&format!("{fname}."))
                && n.ends_with(".download")
                && e.path() != partial
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    let mut sink = match crate::cas::StreamingBlob::resume(&partial) {
        Ok(mut s) => {
            if s.len() > size {
                s.reset()?;
            } else if !s.is_empty() {
                tracing::info!(
                    have = s.len(),
                    size,
                    "resuming {source} from a partial download"
                );
            }
            s
        }
        Err(_) => crate::cas::StreamingBlob::create(&partial)?,
    };
    // A partial that is already whole (the process died between the last
    // byte and the rename) needs no request at all.
    if sink.len() < size || size == 0 {
        c.fetch_blob_into(&layer_digest, progress, Client::MAX_BLOB_BYTES, &mut sink)?;
    }
    let got = sink.finish()?;
    if got != hex {
        let _ = std::fs::remove_file(&partial);
        return Err(Error::DigestMismatch(format!(
            "artifact corrupted or tampered: expected digest {layer_digest}, got sha256:{got}"
        )));
    }
    std::fs::rename(&partial, dest)?;
    Ok(PulledArtifact {
        digest: layer_digest,
        size,
        annotations,
        downloaded: true,
    })
}

/// Minimal mock of an ANONYMOUS OCI registry (no 401 challenge — like a
/// public `ghcr.io` or a local registry without auth): stores blobs/manifests
/// in memory and serves them back. Enough for a real round-trip of
/// `push_oci_artifact`→`pull_oci_artifact` without depending on the network.
/// Also returns a counter of `GET .../blobs/...` requests served — used by
/// `pull_from_registry_with_creds_salta_blobs_ja_no_cas` to prove a 2nd
/// pull of the same reference does not touch the network for content
/// already in the local CAS.
///
/// `pub(crate)` and OUTSIDE `mod tests`: `sign.rs`'s own tests need the exact
/// same mock for a sign→verify round trip, and a second copy of an 80-line
/// fake registry would be exactly the kind of divergence this repo's own
/// "generator and reader share one formula" rule exists to prevent — the two
/// mocks would drift on whatever detail the day one of them gets fixed.
#[cfg(test)]
pub(crate) fn serve_anon_registry() -> (
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

            let write_resp =
                |s: &mut std::net::TcpStream, status: &str, headers: &str, body: &[u8]| {
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

#[cfg(test)]
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    /// Serves one response with `body_len` bytes, chunked (no Content-Length),
    /// so only the streaming cap can stop it.
    fn serve_chunked(body_len: usize) -> String {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = l.accept() {
                let mut buf = [0u8; 1024];
                let _ = c.read(&mut buf);
                let _ = c.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
                let chunk = vec![b'a'; 4096];
                let mut sent = 0;
                while sent < body_len {
                    let n = chunk.len().min(body_len - sent);
                    if c.write_all(format!("{n:x}\r\n").as_bytes()).is_err()
                        || c.write_all(&chunk[..n]).is_err()
                        || c.write_all(b"\r\n").is_err()
                    {
                        return;
                    }
                    sent += n;
                }
                let _ = c.write_all(b"0\r\n\r\n");
            }
        });
        format!("http://{addr}/")
    }

    #[test]
    fn read_capped_refuses_a_body_over_the_limit() {
        let url = serve_chunked(50_000);
        let resp = reqwest::blocking::get(&url).unwrap();
        let err = super::read_capped(resp, 10_000, "manifest").unwrap_err();
        assert!(format!("{err}").contains("limit"), "{err}");
    }

    #[test]
    fn read_capped_accepts_a_body_within_the_limit() {
        let url = serve_chunked(5_000);
        let resp = reqwest::blocking::get(&url).unwrap();
        assert_eq!(
            super::read_capped(resp, 10_000, "manifest").unwrap().len(),
            5_000
        );
    }

    use super::{
        layer_media_type, matches_insecure, parse_content_range, parse_reference,
        pull_from_registry_with_creds, pull_oci_artifact, push_oci_artifact, scheme_for_with,
        serve_anon_registry, sha256_hex, with_prefix, Client,
    };

    fn insecure(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|e| e.to_string()).collect()
    }

    #[test]
    fn loopback_is_http_without_any_declaration() {
        for h in ["localhost:5000", "127.0.0.1:5000", "[::1]:5000"] {
            assert_eq!(scheme_for_with(h, &[]), "http", "{h}");
        }
    }

    /// The default has to stay HTTPS: this knob widens what an operator CAN
    /// allow, it does not change what happens when nobody said anything.
    #[test]
    fn everything_else_is_https_by_default() {
        for h in ["registry.lan:5000", "192.168.1.11:5000", "ghcr.io"] {
            assert_eq!(scheme_for_with(h, &[]), "https", "{h}");
        }
    }

    #[test]
    fn a_declared_registry_is_http() {
        let list = insecure(&["192.168.1.11:5000"]);
        assert_eq!(scheme_for_with("192.168.1.11:5000", &list), "http");
    }

    /// An entry WITH a port is a promise about one endpoint — the same host on
    /// another port keeps the default. Getting this wrong would silently
    /// downgrade a neighbouring registry that was never declared.
    #[test]
    fn a_port_in_the_entry_binds_the_promise_to_that_port() {
        let list = insecure(&["192.168.1.11:5000"]);
        assert_eq!(scheme_for_with("192.168.1.11:5001", &list), "https");
        assert_eq!(scheme_for_with("192.168.1.12:5000", &list), "https");
    }

    /// An entry WITHOUT a port is a promise about the machine.
    #[test]
    fn an_entry_without_a_port_matches_any_port() {
        let list = insecure(&["registry.lan"]);
        assert_eq!(scheme_for_with("registry.lan:5000", &list), "http");
        assert_eq!(scheme_for_with("registry.lan:5001", &list), "http");
        assert_eq!(scheme_for_with("other.lan:5000", &list), "https");
    }

    /// Hosts are case-insensitive; a list written in mixed case must not be a
    /// knob that looks set and is not.
    #[test]
    fn matching_ignores_case() {
        assert!(matches_insecure(
            "Registry.LAN:5000",
            &insecure(&["registry.lan"])
        ));
        assert!(matches_insecure(
            "registry.lan:5000",
            &insecure(&["Registry.LAN:5000"])
        ));
    }

    /// A suffix is not a match: `evil-registry.lan` must not be let in by a
    /// declaration of `registry.lan`.
    #[test]
    fn a_suffix_is_not_a_match() {
        let list = insecure(&["registry.lan"]);
        assert!(!matches_insecure("evil-registry.lan:5000", &list));
        assert_eq!(scheme_for_with("evil-registry.lan:5000", &list), "https");
    }

    #[test]
    fn blank_entries_are_ignored_not_matched() {
        // `DELONIX_INSECURE_REGISTRIES=""` and `"a,,b"` are the shapes a shell
        // produces by accident; neither may turn into "match everything".
        let list = insecure(&["", "registry.lan", ""]);
        assert!(!matches_insecure("ghcr.io", &list));
        assert!(matches_insecure("registry.lan", &list));
    }

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

    fn pull_with_token_answer(answer: &'static str) -> delonix_model::Error {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = serve_one(tx, answer);
        let port = rx.recv().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "delonix-oci-denied-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = crate::ImageStore::open(&tmp).unwrap();
        let err =
            pull_from_registry_with_creds(&store, &format!("127.0.0.1:{port}/repo:tag"), None)
                .expect_err("the token was not granted");
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&tmp);
        err.into()
    }

    /// A registry refusing the token for a read (ghcr answers 403 for a repository
    /// that does not exist) is «no such image, or not visible to you» — the same
    /// answer as a 404 — and not a registry failure that exits 1.
    #[test]
    fn a_refused_read_token_is_not_found() {
        let err = pull_with_token_answer(
            "HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
        assert!(err.is_not_found(), "{err:?}");
    }

    /// A registry that is actually broken keeps saying so.
    #[test]
    fn a_failing_token_endpoint_stays_a_registry_error() {
        let err = pull_with_token_answer(
            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
        assert!(err.number() == 9401, "{err:?}");
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
            "delonix-oci-pull-creds-test-{}-{}",
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

    #[test]
    fn push_e_pull_oci_artifact_round_trip() {
        let (port, _blob_gets, _handle) = serve_anon_registry();
        let tmp = std::env::temp_dir().join(format!(
            "delonix-oci-artifact-test-{}-{}",
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
            "delonix-oci-cas-skip-test-{}-{}",
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

    fn scratch(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "delonix-oci-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// The manifest of a push is now built from each blob's SIZE (metadata)
    /// and its first four bytes, instead of reading every layer whole a second
    /// time. This proves the descriptors did not change: the three media types
    /// are still sniffed correctly, the sizes are the real ones, and the image
    /// survives a push → pull round trip byte for byte.
    #[test]
    fn push_manifest_from_metadata_round_trips() {
        let (port, _gets, _handle) = serve_anon_registry();
        let src = scratch("push-meta-src");
        let store = crate::ImageStore::open(&src).unwrap();
        let config =
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#;
        let id = store.cas().write(config).unwrap();
        let blobs: [&[u8]; 3] = [
            &[0x1f, 0x8b, 0x08, 0x00, 1, 2, 3],
            &[0x28, 0xb5, 0x2f, 0xfd, 9, 9],
            b"plain-tar-bytes",
        ];
        let layers: Vec<String> = blobs
            .iter()
            .map(|b| store.cas().write(b).unwrap())
            .collect();
        let image = crate::image::Image {
            id: id.clone(),
            repo_tags: vec!["local/meta:t".into()],
            layers: layers.clone(),
            config: crate::image::ImageConfig::default(),
            created_unix: 0,
        };
        store.save(&image).unwrap();

        let (bytes, _) = crate::registry::build_manifest(&store, &image).unwrap();
        let m: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(m["config"]["size"], config.len());
        let want = [
            "application/vnd.docker.image.rootfs.diff.tar.gzip",
            "application/vnd.oci.image.layer.v1.tar+zstd",
            "application/vnd.oci.image.layer.v1.tar",
        ];
        for (i, l) in m["layers"].as_array().unwrap().iter().enumerate() {
            assert_eq!(l["mediaType"], want[i], "layer {i}");
            assert_eq!(l["size"], blobs[i].len(), "layer {i}");
            assert_eq!(l["digest"], layers[i], "layer {i}");
        }

        let target = format!("127.0.0.1:{port}/meta:t");
        crate::registry::push_to_registry(&store, "local/meta:t", &target).expect("push");
        let dst = scratch("push-meta-dst");
        let store2 = crate::ImageStore::open(&dst).unwrap();
        let pulled = pull_from_registry_with_creds(&store2, &target, None).expect("pull");
        assert_eq!(pulled.id, id);
        assert_eq!(pulled.layers, layers);
        for (i, dg) in layers.iter().enumerate() {
            assert_eq!(store2.cas().read(dg).unwrap(), blobs[i]);
        }
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    /// The config is now fetched on its own thread alongside the layers. Its
    /// digest check must have come along: a registry serving a config whose
    /// bytes do not hash to the manifest's digest is refused, and nothing is
    /// recorded as an image.
    #[test]
    fn a_tampered_config_is_refused_on_the_parallel_path() {
        let (port, _gets, _handle) = serve_anon_registry();
        let mut c = test_client(&format!("127.0.0.1:{port}"), "tamper");
        let real =
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#;
        let config_digest = format!("sha256:{}", sha256_hex(real));
        // The registry stores OTHER bytes under the real config's digest.
        c.push_blob(&config_digest, b"{\"evil\":true}").unwrap();
        let layer = b"layer-bytes".to_vec();
        let layer_digest = format!("sha256:{}", sha256_hex(&layer));
        c.push_blob(&layer_digest, &layer).unwrap();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "size": real.len(), "digest": config_digest},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar",
                        "size": layer.len(), "digest": layer_digest}],
        });
        c.push_manifest(
            "t",
            &serde_json::to_vec(&manifest).unwrap(),
            "application/vnd.oci.image.manifest.v1+json",
        )
        .unwrap();

        let tmp = scratch("tamper");
        let store = crate::ImageStore::open(&tmp).unwrap();
        let err =
            pull_from_registry_with_creds(&store, &format!("127.0.0.1:{port}/tamper:t"), None)
                .expect_err("a config that does not match its digest must be refused");
        assert!(err.to_string().contains("config digest mismatch"), "{err}");
        assert!(!store.cas().has(&config_digest));
        assert!(store.list().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Gate for the fixed 300s whole-request ceiling on container-image
    /// pushes: every layer that could not move in five minutes failed, however
    /// healthy the link. The push must use the transfer client, like the VM
    /// artifact path already did.
    #[test]
    fn container_push_uses_the_transfer_client() {
        let src = include_str!("registry.rs");
        // The client is built by the variant with progress; the plain entry
        // point only delegates to it.
        let start = src.find("pub fn push_to_registry_with_progress(").unwrap();
        let body = &src[start..start + src[start..].find("\n}\n").unwrap()];
        assert!(
            body.contains("transfer_client()"),
            "the container push must use transfer_client"
        );
        assert!(
            !body.contains(".timeout("),
            "the container push must not set its own ceiling"
        );
    }

    fn artifact_fixture(
        tag: &str,
        payload: &[u8],
    ) -> (
        u16,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        String,
        std::path::PathBuf,
    ) {
        let (port, gets, _h) = serve_anon_registry();
        let dir = scratch(tag);
        let target = format!("127.0.0.1:{port}/vm:{tag}");
        push_oci_artifact(
            &dir,
            &target,
            "application/vnd.delonix.vmimage.v1.qcow2",
            payload,
        )
        .unwrap();
        (port, gets, target, dir)
    }

    fn leftovers(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".download"))
            .collect()
    }

    /// A VM image pulled to a file: whole and verified, and a second pull of
    /// the same blob to the same path downloads nothing at all.
    #[test]
    fn a_vm_artifact_streams_to_its_file_and_is_not_pulled_twice() {
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
        let (_p, gets, target, dir) = artifact_fixture("to-file", &payload);
        let dest = dir.join("img.qcow2");

        let a = crate::registry::pull_oci_artifact_to_file(&dir, &target, &dest, None).unwrap();
        assert!(a.downloaded);
        assert_eq!(a.size, payload.len() as u64);
        assert_eq!(a.digest, format!("sha256:{}", sha256_hex(&payload)));
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        assert!(leftovers(&dir).is_empty());

        let before = gets.load(std::sync::atomic::Ordering::SeqCst);
        let b = crate::registry::pull_oci_artifact_to_file(&dir, &target, &dest, None).unwrap();
        assert!(!b.downloaded, "the file already held this exact blob");
        assert_eq!(gets.load(std::sync::atomic::Ordering::SeqCst), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A partial left WHOLE by a process that died before the rename is
    /// finished without a single request; one of another version of the tag
    /// is removed; a corrupt one is started over and still ends up correct.
    #[test]
    fn a_partial_from_an_earlier_process_is_resumed_or_discarded() {
        let payload: Vec<u8> = (0..150_000u32).map(|i| (i % 241) as u8).collect();
        let hex = sha256_hex(&payload);
        let (_p, gets, target, dir) = artifact_fixture("resume-file", &payload);
        let dest = dir.join("img.qcow2");
        let partial = dir.join(format!("img.qcow2.{}.download", &hex[..12]));
        let stale = dir.join("img.qcow2.aaaaaaaaaaaa.download");

        std::fs::write(&partial, &payload).unwrap();
        std::fs::write(&stale, b"an older version").unwrap();
        let before = gets.load(std::sync::atomic::Ordering::SeqCst);
        crate::registry::pull_oci_artifact_to_file(&dir, &target, &dest, None).unwrap();
        assert_eq!(gets.load(std::sync::atomic::Ordering::SeqCst), before);
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));

        std::fs::remove_file(&dest).unwrap();
        std::fs::write(&partial, vec![0xEEu8; 1000]).unwrap();
        crate::registry::pull_oci_artifact_to_file(&dir, &target, &dest, None).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        assert!(leftovers(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bytes that do not hash to the manifest's digest: refused, nothing under
    /// the final name, and no partial kept to be "resumed" later.
    #[test]
    fn a_tampered_vm_artifact_leaves_nothing_on_disk() {
        let (port, _gets, _h) = serve_anon_registry();
        let mut c = test_client(&format!("127.0.0.1:{port}"), "vm");
        let promised = b"the image the publisher signed".to_vec();
        let digest = format!("sha256:{}", sha256_hex(&promised));
        c.push_blob(&digest, b"what a compromised registry serves")
            .unwrap();
        c.push_blob(&format!("sha256:{}", sha256_hex(b"{}")), b"{}")
            .unwrap();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.empty.v1+json",
                       "size": 2, "digest": format!("sha256:{}", sha256_hex(b"{}"))},
            "layers": [{"mediaType": "application/vnd.delonix.vmimage.v1.qcow2",
                        "size": promised.len(), "digest": digest}],
        });
        c.push_manifest(
            "bad",
            &serde_json::to_vec(&manifest).unwrap(),
            "application/vnd.oci.image.manifest.v1+json",
        )
        .unwrap();
        let dir = scratch("vm-tamper");
        let dest = dir.join("img.qcow2");
        let err = crate::registry::pull_oci_artifact_to_file(
            &dir,
            &format!("127.0.0.1:{port}/vm:bad"),
            &dest,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, crate::Error::DigestMismatch(_)), "{err}");
        assert!(!dest.exists());
        assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A VM image pushed straight from its file arrives whole, and the
    /// manifest describes the file's real size.
    #[test]
    fn a_vm_artifact_pushed_from_its_file_round_trips() {
        let (port, _gets, _h) = serve_anon_registry();
        let dir = scratch("push-file");
        let src = dir.join("img.qcow2");
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 239) as u8).collect();
        std::fs::write(&src, &payload).unwrap();
        let target = format!("127.0.0.1:{port}/vm:from-file");

        crate::registry::push_oci_artifact_file(
            &dir,
            &target,
            "application/vnd.delonix.vmimage.v1.qcow2",
            &src,
            &std::collections::BTreeMap::new(),
            None,
        )
        .expect("push from file");
        let dest = dir.join("back.qcow2");
        let pulled =
            crate::registry::pull_oci_artifact_to_file(&dir, &target, &dest, None).unwrap();
        assert_eq!(pulled.size, payload.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A registry that answers the first blob PUT with 401 makes the push
    /// fetch a token and send the PUT again. The body is a FILE now, not a
    /// buffer to clone — the second attempt must reopen it and send the whole
    /// blob, with the token.
    #[test]
    fn a_401_on_the_blob_put_resends_the_whole_file() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(
            String,
            Option<String>,
            Vec<u8>,
        )>::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            let mut puts = 0;
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let hend = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = crate::registry::find_subslice(&buf, b"\r\n\r\n") {
                        break Some(i);
                    }
                };
                let Some(hend) = hend else { continue };
                let head = String::from_utf8_lossy(&buf[..hend]).to_string();
                let first = head.lines().next().unwrap_or_default().to_string();
                let lower = head.to_lowercase();
                let len: usize = lower
                    .lines()
                    .find(|l| l.starts_with("content-length:"))
                    .and_then(|l| l[15..].trim().parse().ok())
                    .unwrap_or(0);
                let auth = lower
                    .lines()
                    .find(|l| l.starts_with("authorization:"))
                    .map(|l| l[14..].trim().to_string());
                let mut body = buf[hend + 4..].to_vec();
                while body.len() < len {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                let (status, extra, resp_body): (&str, String, &[u8]) = if first
                    .starts_with("HEAD ")
                {
                    ("404 Not Found", String::new(), b"")
                } else if first.starts_with("POST ") {
                    (
                        "202 Accepted",
                        "location: /v2/r/blobs/uploads/u1\r\n".into(),
                        b"",
                    )
                } else if first.starts_with("GET /token") {
                    ("200 OK", String::new(), br#"{"token":"tok"}"#)
                } else if first.starts_with("PUT ") {
                    puts += 1;
                    log.lock()
                        .unwrap()
                        .push((first.clone(), auth.clone(), body.clone()));
                    if puts == 1 {
                        (
                            "401 Unauthorized",
                            format!("www-authenticate: Bearer realm=\"http://127.0.0.1:{port}/token\"\r\n"),
                            b"",
                        )
                    } else {
                        ("201 Created", String::new(), b"")
                    }
                } else {
                    ("404 Not Found", String::new(), b"")
                };
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 {status}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n",
                        resp_body.len()
                    )
                    .as_bytes(),
                );
                let _ = s.write_all(resp_body);
            }
        });

        let dir = scratch("put-401");
        let path = dir.join("blob");
        let payload: Vec<u8> = (0..500_000u32).map(|i| (i % 211) as u8).collect();
        std::fs::write(&path, &payload).unwrap();
        let digest = format!("sha256:{}", sha256_hex(&payload));
        let mut c = test_client(&format!("127.0.0.1:{port}"), "r");
        c.push_blob_file(&digest, &path, payload.len() as u64, None)
            .expect("the push must retry the PUT after the 401");

        let puts = seen.lock().unwrap().clone();
        assert_eq!(puts.len(), 2, "one refused PUT, one retry");
        let (_, auth, body) = &puts[1];
        assert_eq!(auth.as_deref(), Some("bearer tok"));
        assert_eq!(body, &payload, "the retry must carry the whole file again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the scripted registry does with the n-th blob PUT.
    #[derive(Clone, Copy)]
    enum PutAct {
        /// Answer with this status (201 stores the blob).
        Status(u16),
        /// Read the body, then close without answering: a dropped connection.
        Drop,
        /// Store the blob, then close without answering: the answer is lost.
        StoreThenDrop,
    }

    /// A registry whose blob PUTs follow `script`, in order (the last entry
    /// repeats). HEAD answers 200 once a blob is stored. Returns the port and
    /// the number of PUTs received.
    fn serve_scripted_push(
        script: Vec<PutAct>,
    ) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let puts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = puts.clone();
        std::thread::spawn(move || {
            let mut stored = false;
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let hend = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = crate::registry::find_subslice(&buf, b"\r\n\r\n") {
                        break Some(i);
                    }
                };
                let Some(hend) = hend else { continue };
                let head = String::from_utf8_lossy(&buf[..hend]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find(|l| l.starts_with("content-length:"))
                    .and_then(|l| l[15..].trim().parse().ok())
                    .unwrap_or(0);
                let mut body_len = buf.len() - hend - 4;
                while body_len < len {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body_len += n;
                }
                let reply = |s: &mut std::net::TcpStream, status: &str, extra: &str| {
                    let _ = s.write_all(
                        format!("HTTP/1.1 {status}\r\n{extra}content-length: 0\r\nconnection: close\r\n\r\n")
                            .as_bytes(),
                    );
                };
                if head.starts_with("head ") {
                    reply(&mut s, if stored { "200 OK" } else { "404 Not Found" }, "");
                } else if head.starts_with("post ") {
                    reply(
                        &mut s,
                        "202 Accepted",
                        "location: /v2/r/blobs/uploads/u\r\n",
                    );
                } else if head.starts_with("put ") {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    match script[n.min(script.len() - 1)] {
                        PutAct::Status(code) => {
                            if code == 201 {
                                stored = true;
                            }
                            reply(&mut s, &format!("{code} X"), "");
                        }
                        PutAct::Drop => {}
                        PutAct::StoreThenDrop => stored = true,
                    }
                } else {
                    reply(&mut s, "404 Not Found", "");
                }
            }
        });
        (port, puts)
    }

    fn push_with_script(script: Vec<PutAct>) -> (crate::Result<()>, usize) {
        let (res, puts, _) = push_with_script_metered(script);
        (res, puts)
    }

    /// [`push_with_script`], also returning every `(sent, total)` the upload
    /// reported.
    fn push_with_script_metered(
        script: Vec<PutAct>,
    ) -> (crate::Result<()>, usize, Vec<(u64, u64)>) {
        let reports = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = reports.clone();
        let meter = Some(crate::registry::MeterSlot {
            meter: crate::registry::PushMeter::new(
                1,
                200_000,
                std::sync::Arc::new(move |d, t| sink.lock().unwrap().push((d, t))),
            ),
            slot: 0,
        });
        let (port, puts) = serve_scripted_push(script);
        let dir = scratch("push-retry");
        let path = dir.join("blob");
        let payload = vec![5u8; 200_000];
        std::fs::write(&path, &payload).unwrap();
        let digest = format!("sha256:{}", sha256_hex(&payload));
        let mut c = test_client(&format!("127.0.0.1:{port}"), "r");
        let res = c.push_blob_file(&digest, &path, payload.len() as u64, meter.as_ref());
        let _ = std::fs::remove_dir_all(&dir);
        let seen = reports.lock().unwrap().clone();
        (res, puts.load(std::sync::atomic::Ordering::SeqCst), seen)
    }

    /// The push now reports progress, and a retried upload reads its file
    /// from zero again. Its slot must move back rather than add: the bar
    /// never passes the total, and ends exactly on it.
    #[test]
    fn push_progress_survives_a_retry_without_double_counting() {
        let (res, puts, reports) =
            push_with_script_metered(vec![PutAct::Status(503), PutAct::Status(201)]);
        res.expect("the retry succeeds");
        assert_eq!(puts, 2);
        assert!(!reports.is_empty(), "nothing was reported");
        assert!(
            reports.iter().all(|&(d, t)| d <= t),
            "progress passed the total: {:?}",
            reports.iter().max()
        );
        assert_eq!(reports.last(), Some(&(200_000, 200_000)));
    }

    /// The push had no retry: a dropped connection or a registry hiccup
    /// meant sending the whole blob again by hand. A 503 and a dropped
    /// connection are retried on a new session.
    #[test]
    fn a_push_retries_a_503_and_a_dropped_connection() {
        let (res, puts) =
            push_with_script(vec![PutAct::Status(503), PutAct::Drop, PutAct::Status(201)]);
        res.expect("two transient failures must not fail the push");
        assert_eq!(puts, 3);
    }

    /// A PUT that landed but whose answer was lost is NOT sent again: the
    /// next attempt's HEAD finds the blob.
    #[test]
    fn a_push_whose_answer_was_lost_is_not_sent_twice() {
        let (res, puts) = push_with_script(vec![PutAct::StoreThenDrop]);
        res.expect("the blob is in the registry");
        assert_eq!(
            puts, 1,
            "the blob was already there — a second PUT would resend it all"
        );
    }

    /// A 400 (digest mismatch) cannot be fixed by sending the same bytes
    /// again: one PUT, and the error.
    #[test]
    fn a_push_does_not_retry_a_400() {
        let (res, puts) = push_with_script(vec![PutAct::Status(400)]);
        assert!(res.unwrap_err().to_string().contains("400"));
        assert_eq!(puts, 1);
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
            "delonix-oci-artifact-tamper-test-{}-{}",
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
            "delonix-oci-manifest-pin-test-{}-{}",
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
            "delonix-oci-manifest-pin-ok-test-{}-{}",
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

    /// Everything in the store's blob directory that is not a finished blob:
    /// a streamed download must never leave a scratch file behind.
    fn scratch_files(cas: &crate::cas::Cas) -> Vec<String> {
        let dir = cas.path("sha256:x").parent().unwrap().to_path_buf();
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect()
    }

    /// Streaming into the CAS keeps the three resume behaviours of the
    /// in-memory path — including the file being TRUNCATED when the server
    /// ignores the range or answers from the wrong offset (appending there
    /// would duplicate the prefix on disk).
    #[test]
    fn a_streamed_blob_resumes_or_restarts_into_the_cas() {
        for (mode, cut) in [
            (Resume::Honour, 12_345),
            (Resume::Ignore, 9_000),
            (Resume::WrongOffset, 7_000),
        ] {
            let payload: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
            let digest = format!("sha256:{}", sha256_hex(&payload));
            let (port, _) = serve_flaky_blob(payload.clone(), cut, mode);
            let mut c = test_client(&format!("127.0.0.1:{port}"), "stream");
            let dir = scratch("stream-cas");
            let cas = crate::cas::Cas::open(&dir).unwrap();

            c.blob_into_cas(&cas, &digest, None)
                .expect("the cut download must end up whole in the CAS");
            assert_eq!(cas.read(&digest).unwrap(), payload);
            assert!(scratch_files(&cas).is_empty(), "{:?}", scratch_files(&cas));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Content that does not hash to its digest is refused and leaves NOTHING
    /// in the store — neither under the digest's name nor as a scratch file.
    #[test]
    fn a_streamed_blob_that_does_not_match_its_digest_leaves_nothing() {
        let payload = b"what the registry actually sent".to_vec();
        let (port, _) = serve_flaky_blob(payload.clone(), 5, Resume::Honour);
        let mut c = test_client(&format!("127.0.0.1:{port}"), "tamper-layer");
        let dir = scratch("stream-tamper");
        let cas = crate::cas::Cas::open(&dir).unwrap();
        let claimed = format!("sha256:{}", sha256_hex(b"what the manifest promised"));

        let err = c.blob_into_cas(&cas, &claimed, None).unwrap_err();
        assert!(matches!(err, crate::Error::DigestMismatch(_)), "{err}");
        assert!(!cas.has(&claimed));
        assert!(scratch_files(&cas).is_empty(), "{:?}", scratch_files(&cas));
        let _ = std::fs::remove_dir_all(&dir);
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
            "delonix-oci-progress-test-{}-{}",
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
