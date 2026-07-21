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
fn parse_reference(input: &str) -> (String, String, String) {
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

/// The target architecture in OCI vocabulary (`amd64`, `arm64`, ...).
fn target_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        other => other,
    }
}

struct Client {
    http: reqwest::blocking::Client,
    host: String,
    repo: String,
    token: Option<String>,
    /// Credentials (`delonix login`), if any, for private registries.
    creds: Option<(String, String)>,
}

impl Client {
    fn send_once(&self, url: &str, accept: &str) -> reqwest::Result<reqwest::blocking::Response> {
        let mut req = self.http.get(url).header(reqwest::header::ACCEPT, accept);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        req.send()
    }

    /// GET with Bearer authentication; on 401, obtains a token and retries (once).
    fn fetch(&mut self, url: &str, accept: &str) -> Result<reqwest::blocking::Response> {
        let resp = self.send_once(url, accept).map_err(reg_err)?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let www = resp
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            self.token = Some(self.get_token(&www, None)?);
            let resp = self.send_once(url, accept).map_err(reg_err)?;
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
            Err(Error::NotFound(format!(
                "image {}:{}",
                self.repo,
                url.rsplit('/').next().unwrap_or("")
            )))
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

    fn blob(&mut self, digest: &str) -> Result<Vec<u8>> {
        self.blob_with_progress(digest, None)
    }

    /// Downloads a blob in STREAMING, calling `progress(bytes_read, total)` as
    /// it advances — the total comes from `Content-Length` (may be missing in
    /// chunked responses, hence the `Option`). Reading in chunks instead of `.bytes()`
    /// (which loads everything before returning) is what enables a progress
    /// bar: a VM artifact is hundreds of MB and without this the `pull` looks
    /// hung. The engine crate only REPORTS the bytes; the drawing is the bin's job.
    fn blob_with_progress(
        &mut self,
        digest: &str,
        progress: Option<&dyn Fn(u64, Option<u64>)>,
    ) -> Result<Vec<u8>> {
        use std::io::Read;
        let url = format!(
            "{}://{}/v2/{}/blobs/{}",
            scheme_for(&self.host),
            self.host,
            self.repo,
            digest
        );
        let mut resp = self.fetch(&url, "*/*")?;
        let total = resp.content_length();
        let mut buf: Vec<u8> = Vec::with_capacity(total.unwrap_or(0) as usize);
        let mut chunk = [0u8; 65536];
        let mut done: u64 = 0;
        loop {
            let n = resp
                .read(&mut chunk)
                .map_err(|e| Error::Registry(format!("blob read: {e}")))?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            done += n as u64;
            if let Some(p) = progress {
                p(done, total);
            }
        }
        Ok(buf)
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
const DOCKER_MANIFEST_MEDIA_TYPE: &str = "application/vnd.docker.distribution.manifest.v2+json";

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
pub fn registry_client(store: &ImageStore, reference: &str) -> Result<RegistryClient> {
    let (host, repo, refr) = parse_reference(reference);
    let http = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        // 600s (was 120s) — aligned with push_to_registry/push_oci_artifact:
        // blobs of large images (e.g. kindest/node, several hundred MB)
        // do not fit in a 120s deadline; `reqwest` cuts the body read
        // halfway, reported as "error decoding response body" (it is not a
        // parsing error — it is an interrupted stream read).
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(reg_err)?;
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
    let (host, repo, refr) = parse_reference(reference);
    let http = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        // 600s — see the same comment in `registry_client`.
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(reg_err)?;
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

    let manifest_bytes = if content_type.contains("index") || content_type.contains("manifest.list")
    {
        // Multi-arch index (`oci_spec::image::ImageIndex`) — picks the
        // linux/<arch> entry (or the first one, lacking a match).
        let index: ImageIndex = serde_json::from_slice(&body)?;
        let arch = target_arch();
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
        r.bytes().map_err(reg_err)?.to_vec()
    } else {
        body
    };

    // Image manifest (`oci_spec::image::ImageManifest`) — OCI/Docker v2 schema.
    let manifest: ImageManifest = serde_json::from_slice(&manifest_bytes)?;

    // 2) config blob (= image id)
    let config_digest_str = manifest.config().digest().to_string();
    let config_bytes = c.blob(&config_digest_str)?;
    if sha256_hex(&config_bytes) != manifest.config().digest().digest() {
        return Err(Error::Registry("config digest mismatch".into()));
    }
    let config_digest = store.cas().write(&config_bytes)?;

    // 3) layers (ignores "foreign"/Windows layers)
    let real_layers: Vec<&Descriptor> = manifest
        .layers()
        .iter()
        .filter(|l| !l.media_type().to_string().contains("foreign"))
        .collect();
    let total = real_layers.len();
    let mut layers = Vec::with_capacity(total);
    for (i, l) in real_layers.iter().enumerate() {
        let ldigest = l.digest().to_string();
        tracing::debug!(
            index = i + 1,
            total,
            digest = %&ldigest[..ldigest.len().min(19)],
            "pulling layer {}/{}",
            i + 1,
            total
        );
        let data = c.blob(&ldigest)?;
        let dg = store.cas().write(&data)?;
        if dg != ldigest {
            return Err(Error::Registry(format!("corrupted layer: {ldigest}")));
        }
        layers.push(dg);
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
    let (host, repo, refr) = parse_reference(target);
    let http = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(reg_err)?;
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
        .build()
        .map_err(oci_err)?;
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    c.push_manifest(&refr, &manifest_bytes, MediaType::ImageManifest.as_ref())?;

    let digest = format!("sha256:{}", sha256_hex(&manifest_bytes));
    tracing::info!(host = %host, repo = %repo, reference = %refr, digest = %digest, "pushed: {host}/{repo}:{refr}");
    Ok(digest)
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
    let (host, repo, refr) = parse_reference(source);
    let http = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(Duration::from_secs(600))
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

    let accept = "application/vnd.oci.image.manifest.v1+json";
    let url = c.manifest_url(&refr);
    let manifest_bytes = c.fetch(&url, accept)?.bytes().map_err(reg_err)?.to_vec();
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
    // announced digest without detection. See CLAUDE.md.
    let got = format!("sha256:{}", sha256_hex(&data));
    if got != layer_digest {
        return Err(Error::Registry(format!(
            "artifact corrupted or tampered: expected digest {layer_digest}, got {got}"
        )));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::{
        layer_media_type, parse_reference, pull_from_registry_with_creds, pull_oci_artifact,
        push_oci_artifact, sha256_hex, with_prefix,
    };

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
    fn serve_anon_registry() -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let blobs: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>> =
            Arc::new(Mutex::new(Default::default()));
        let manifests: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>> =
            Arc::new(Mutex::new(Default::default()));
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
        (port, handle)
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    #[test]
    fn push_e_pull_oci_artifact_round_trip() {
        let (port, _handle) = serve_anon_registry();
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

    /// Security-audit finding: `pull_oci_artifact` must reject a blob whose
    /// real content does not match the digest declared in the manifest — simulates a
    /// compromised/tampered registry that serves different bytes under the same digest.
    #[test]
    fn pull_oci_artifact_recusa_blob_adulterado() {
        let (port, _handle) = serve_anon_registry();
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
}
