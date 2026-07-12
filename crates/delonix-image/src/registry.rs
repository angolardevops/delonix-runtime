//! Pull a partir de um registo OCI (Docker Registry HTTP API V2).
//!
//! Suporta o Docker Hub por omissão (com token anónimo) e qualquer registo
//! público que use o protocolo V2 (ghcr.io, quay.io, registry.k8s.io, ...).
//! O fluxo: resolve a referência → manifesto (escolhe a plataforma se for um
//! índice multi-arch) → blob de config → blobs de layers → guarda no CAS, tal
//! como o `load_docker_archive`.

use crate::cas::{sha256_hex, strip};
use crate::image::{now_unix, Image, ImageConfig, ImageStore};
use delonix_core::{Error, Result};
use serde::Deserialize;
use std::time::Duration;

/// Tipos de média aceites ao pedir um manifesto (índice OU manifesto de imagem).
const ACCEPT_MANIFEST: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";

#[derive(Deserialize)]
struct Index {
    manifests: Vec<IndexEntry>,
}
#[derive(Deserialize)]
struct IndexEntry {
    digest: String,
    #[serde(default)]
    platform: Option<Platform>,
}
#[derive(Deserialize)]
struct Platform {
    architecture: String,
    os: String,
}
#[derive(Deserialize)]
struct Manifest {
    config: Descriptor,
    layers: Vec<Descriptor>,
}
#[derive(Deserialize)]
struct Descriptor {
    digest: String,
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
}
#[derive(Deserialize)]
struct RawConfig {
    config: Option<RawInner>,
}
#[derive(Deserialize)]
struct RawInner {
    #[serde(rename = "Cmd")]
    cmd: Option<Vec<String>>,
    #[serde(rename = "Entrypoint")]
    entrypoint: Option<Vec<String>>,
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
    #[serde(rename = "User")]
    user: Option<String>,
    #[serde(rename = "WorkingDir")]
    working_dir: Option<String>,
}

fn reg_err(e: reqwest::Error) -> Error {
    Error::Registry(e.to_string())
}

/// Separa a referência em (host da API, repositório, tag/digest), aplicando as
/// regras do Docker: registo por omissão `registry-1.docker.io`, imagens
/// oficiais sob `library/`.
fn parse_reference(input: &str) -> (String, String, String) {
    // tag (`:`) ou digest (`@`) — o `:` tem de estar DEPOIS da última `/`.
    let (name, reference) = if let Some(idx) = input.find('@') {
        (&input[..idx], input[idx + 1..].to_string())
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
    // `docker.io`/`index.docker.io` → o host real da API V2.
    if host == "docker.io" || host == "index.docker.io" {
        host = "registry-1.docker.io".to_string();
    }
    // Docker Hub: imagem oficial de um só componente → prefixo `library/`.
    if host == "registry-1.docker.io" && !repo.contains('/') {
        repo = format!("library/{repo}");
    }
    (host, repo, reference)
}

/// Extrai `chave="valor"` de um cabeçalho `WWW-Authenticate`.
fn extract(header: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let start = header.find(&pat)? + pat.len();
    let rest = &header[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// O esquema HTTP para um registo: `http` para registos locais/inseguros
/// (`localhost`, `127.0.0.1`, `[::1]`), `https` para todos os outros — a mesma
/// regra do Docker/containerd para registos inseguros por omissão.
fn scheme_for(host: &str) -> &'static str {
    let h = host.split(':').next().unwrap_or(host);
    if h == "localhost" || h == "127.0.0.1" || h == "::1" || h == "[::1]" {
        "http"
    } else {
        "https"
    }
}

/// A arquitectura-alvo no vocabulário OCI (`amd64`, `arm64`, ...).
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
    /// Credenciais (`delonix login`), se existirem, para registos privados.
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

    /// GET com autenticação Bearer; em 401, obtém um token e repete (uma vez).
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
            Err(Error::NotFound(format!("image {}:{}", self.repo, url.rsplit('/').next().unwrap_or(""))))
        } else {
            Err(Error::Registry(format!("HTTP {status} em {url}")))
        }
    }

    /// Pede um token ao serviço de autenticação indicado no 401. Com
    /// `force_scope`, pede esse âmbito (ex.: `…:pull,push` para o `push`) em vez
    /// do indicado pelo servidor — o servidor concede-o se as credenciais o
    /// permitirem.
    fn get_token(&self, www: &str, force_scope: Option<&str>) -> Result<String> {
        let realm = extract(www, "realm")
            .ok_or_else(|| Error::Registry("autenticação sem `realm`".into()))?;
        let scope = match force_scope {
            Some(s) => s.to_string(),
            None => extract(www, "scope")
                .unwrap_or_else(|| format!("repository:{}:pull", self.repo)),
        };
        let mut url = format!("{realm}?scope={scope}");
        if let Some(service) = extract(www, "service") {
            url.push_str(&format!("&service={service}"));
        }
        let mut req = self.http.get(&url);
        // Registo privado: autentica o pedido de token com Basic (user:password).
        if let Some((u, p)) = &self.creds {
            req = req.basic_auth(u, Some(p));
        }
        let resp = req.send().map_err(reg_err)?;
        if !resp.status().is_success() {
            return Err(Error::Registry(format!("falha a obter token: HTTP {}", resp.status())));
        }
        let v: serde_json::Value = resp.json().map_err(reg_err)?;
        v.get("token")
            .or_else(|| v.get("access_token"))
            .and_then(|t| t.as_str())
            .map(String::from)
            .ok_or_else(|| Error::Registry("resposta de autenticação sem token".into()))
    }

    fn manifest_url(&self, reference: &str) -> String {
        format!("{}://{}/v2/{}/manifests/{}", scheme_for(&self.host), self.host, self.repo, reference)
    }

    fn blob(&mut self, digest: &str) -> Result<Vec<u8>> {
        let url = format!("{}://{}/v2/{}/blobs/{}", scheme_for(&self.host), self.host, self.repo, digest);
        let resp = self.fetch(&url, "*/*")?;
        Ok(resp.bytes().map_err(reg_err)?.to_vec())
    }

    // ---- push (escrita): blobs + manifesto ----------------------------------

    /// Executa um pedido de escrita; em 401, obtém um token com âmbito
    /// `pull,push` e repete (uma vez). O `build` é chamado a cada tentativa (o
    /// corpo é reconstruído), por isso é seguro repetir.
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

    /// `true` se o blob já existe no registo (evita reenviá-lo — dedup remota).
    fn blob_exists(&mut self, digest: &str) -> Result<bool> {
        let url = format!("{}://{}/v2/{}/blobs/{}", scheme_for(&self.host), self.host, self.repo, digest);
        let resp = self.write_req(&|http| http.head(&url))?;
        Ok(resp.status().is_success())
    }

    /// Envia um blob (config ou layer) por upload monolítico: `POST` para abrir
    /// a sessão, depois `PUT …?digest=<sha256>` com o conteúdo.
    fn push_blob(&mut self, digest: &str, data: &[u8]) -> Result<()> {
        if self.blob_exists(digest)? {
            return Ok(());
        }
        let start = format!("{}://{}/v2/{}/blobs/uploads/", scheme_for(&self.host), self.host, self.repo);
        let resp = self.write_req(&|http| http.post(&start))?;
        if resp.status() != reqwest::StatusCode::ACCEPTED {
            return Err(Error::Registry(format!(
                "abertura de upload: HTTP {} (faça `delonix login {}`?)",
                resp.status(),
                self.host
            )));
        }
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::Registry("upload sem cabeçalho Location".into()))?
            .to_string();
        // Location pode vir absoluto ou relativo ao host.
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
            return Err(Error::Registry(format!("PUT de blob {digest}: HTTP {status}")));
        }
        Ok(())
    }

    /// Publica o manifesto sob a tag/digest dado.
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
            return Err(Error::Registry(format!("PUT de manifesto: HTTP {status} {detail}")));
        }
        Ok(())
    }
}

/// Garante o prefixo `sha256:` num digest.
fn with_prefix(digest: &str) -> String {
    if digest.starts_with("sha256:") {
        digest.to_string()
    } else {
        format!("sha256:{digest}")
    }
}

/// O mediaType de um layer pelo seu *magic number* (gzip/zstd/tar simples).
fn layer_media_type(data: &[u8]) -> &'static str {
    if data.starts_with(&[0x1f, 0x8b]) {
        "application/vnd.docker.image.rootfs.diff.tar.gzip"
    } else if data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        "application/vnd.oci.image.layer.v1.tar+zstd"
    } else {
        "application/vnd.oci.image.layer.v1.tar"
    }
}

/// Cliente de registo reutilizável (fachada pública) — usado pela verificação
/// de assinaturas (B8) para buscar manifestos e blobs com a mesma auth do pull.
pub struct RegistryClient {
    inner: Client,
    reference: String,
}

/// Constrói um [`RegistryClient`] para `reference` (reutiliza credenciais e auth).
pub fn registry_client(store: &ImageStore, reference: &str) -> Result<RegistryClient> {
    let (host, repo, refr) = parse_reference(reference);
    let http = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(reg_err)?;
    let creds = crate::auth::lookup(store.root(), &host);
    Ok(RegistryClient {
        inner: Client { http, host, repo, token: None, creds },
        reference: refr,
    })
}

impl RegistryClient {
    /// A tag/digest com que o cliente foi criado.
    pub fn reference(&self) -> String {
        self.reference.clone()
    }
    /// Bytes crus de um manifesto (pela tag ou digest).
    pub fn get_manifest(&mut self, refr: &str) -> Result<Vec<u8>> {
        let url = self.inner.manifest_url(refr);
        let resp = self.inner.fetch(&url, ACCEPT_MANIFEST)?;
        Ok(resp.bytes().map_err(reg_err)?.to_vec())
    }
    /// Bytes crus de um blob (pelo digest).
    pub fn get_blob(&mut self, digest: &str) -> Result<Vec<u8>> {
        self.inner.blob(digest)
    }
}

/// GET simples que devolve o corpo em bytes — usado para sincronizar feeds
/// (ex.: o feed de CVE do `scan --update`).
pub fn http_get(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(reg_err)?;
    let resp = client.get(url).send().map_err(reg_err)?;
    if !resp.status().is_success() {
        return Err(Error::Registry(format!("HTTP {} em {url}", resp.status())));
    }
    Ok(resp.bytes().map_err(reg_err)?.to_vec())
}

/// GET com Bearer opcional; devolve `(status_http, corpo)`. Mesmo transporte do
/// [`http_post_json`] (aceita self-signed só com `DELONIX_API_INSECURE=1`). Usado pelo
/// CLI para ler recursos da plataforma (ex.: `delonix stack pull` → /v2/studio/designs).
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

/// POST de um corpo JSON com um Bearer opcional; devolve `(status_http, corpo)`.
/// Usado pelo TRANSPORTE HTTP do CLI (`DELONIX_HOST=https://…` → `/v2/cli`): o CLI
/// envia o seu argv à API, que corre o comando na plataforma. Aceita certificados
/// self-signed só com `DELONIX_API_INSECURE=1` (a Console self-host é self-signed;
/// um Cloud com TLS válido não precisa).
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

/// Como [`http_post_json`], mas STREAMING: entrega os bytes da resposta em chunks
/// ao `on_bytes` à medida que chegam (não espera o fim). Devolve o status HTTP.
/// Usado pelo transporte HTTP do CLI para comandos de streaming (`logs -f`, …).
/// Sem timeout (esses comandos correm indefinidamente).
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

/// Descarrega `reference` de um registo OCI para o armazém local.
pub fn pull_from_registry(store: &ImageStore, reference: &str) -> Result<Image> {
    let (host, repo, refr) = parse_reference(reference);
    let http = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(reg_err)?;
    let creds = crate::auth::lookup(store.root(), &host);
    let mut c = Client { http, host: host.clone(), repo: repo.clone(), token: None, creds };

    eprintln!("a puxar {repo}:{refr} de {host}...");

    // 1) manifesto (pode ser um índice multi-arch)
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
        let index: Index = serde_json::from_slice(&body)?;
        let arch = target_arch();
        let pick = index
            .manifests
            .iter()
            .find(|m| {
                m.platform
                    .as_ref()
                    .map(|p| p.os == "linux" && p.architecture == arch)
                    .unwrap_or(false)
            })
            .or_else(|| index.manifests.first())
            .ok_or_else(|| Error::Registry("índice de manifestos vazio".into()))?;
        eprintln!("plataforma escolhida: linux/{arch}");
        let purl = c.manifest_url(&pick.digest);
        let r = c.fetch(&purl, ACCEPT_MANIFEST)?;
        r.bytes().map_err(reg_err)?.to_vec()
    } else {
        body
    };

    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;

    // 2) blob de config (= id da imagem)
    let config_bytes = c.blob(&manifest.config.digest)?;
    if sha256_hex(&config_bytes) != strip(&manifest.config.digest) {
        return Err(Error::Registry("digest do config não confere".into()));
    }
    let config_digest = store.cas().write(&config_bytes)?;

    // 3) layers (ignora layers "foreign"/Windows)
    let real_layers: Vec<&Descriptor> = manifest
        .layers
        .iter()
        .filter(|l| !l.media_type.as_deref().unwrap_or("").contains("foreign"))
        .collect();
    let total = real_layers.len();
    let mut layers = Vec::with_capacity(total);
    for (i, l) in real_layers.iter().enumerate() {
        eprintln!("layer {}/{}  {}", i + 1, total, &l.digest[..l.digest.len().min(19)]);
        let data = c.blob(&l.digest)?;
        let dg = store.cas().write(&data)?;
        if dg != l.digest {
            return Err(Error::Registry(format!("layer corrompido: {}", l.digest)));
        }
        layers.push(dg);
    }

    // 4) monta e guarda
    let raw: RawConfig = serde_json::from_slice(&config_bytes)?;
    let inner = raw.config.unwrap_or(RawInner { cmd: None, entrypoint: None, env: None, user: None, working_dir: None });
    let repo_tags = store.merged_tags(&config_digest, reference);
    let image = Image {
        id: config_digest,
        repo_tags,
        layers,
        config: ImageConfig {
            cmd: inner.cmd.unwrap_or_default(),
            entrypoint: inner.entrypoint.unwrap_or_default(),
            env: inner.env.unwrap_or_default(),
            user: inner.user.unwrap_or_default(),
            working_dir: inner.working_dir.unwrap_or_default(),
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

/// Publica uma imagem local num registo OCI (Docker Registry HTTP API V2).
///
/// Reconstrói um manifesto schema-2 a partir dos blobs do CAS (config = `id`,
/// layers = `layers`), envia os que faltam (`POST`+`PUT` monolítico, com dedup
/// remota por `HEAD`) e publica o manifesto sob a tag de destino. O `push`
/// precisa de credenciais (`delonix login <host>`) para registos autenticados.
/// Constrói o **manifesto Docker schema-2** de uma imagem local (config +
/// descritores dos layers, com o mediaType detetado pelo magic number de cada
/// blob). Devolve `(bytes, digest)`. Usado pelo servidor OCI do register interno
/// para servir `docker pull` sem re-empacotar nada.
pub fn build_manifest(store: &ImageStore, image: &Image) -> Result<(Vec<u8>, String)> {
    let config_data = store.cas().read(&image.id)?;
    let config_desc = serde_json::json!({
        "mediaType": "application/vnd.docker.container.image.v1+json",
        "size": config_data.len(),
        "digest": with_prefix(&image.id),
    });
    let mut layer_descs = Vec::with_capacity(image.layers.len());
    for dg in &image.layers {
        let data = store.cas().read(dg)?;
        layer_descs.push(serde_json::json!({
            "mediaType": layer_media_type(&data),
            "size": data.len(),
            "digest": with_prefix(dg),
        }));
    }
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": config_desc,
        "layers": layer_descs,
    });
    let bytes = serde_json::to_vec(&manifest)?;
    let digest = format!("sha256:{}", crate::cas::sha256_hex(&bytes));
    Ok((bytes, digest))
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
    let mut c = Client { http, host: host.clone(), repo: repo.clone(), token: None, creds };

    eprintln!("a publicar {repo}:{refr} em {host}...");

    // 1) descritor do config + envio do blob de config.
    let config_data = store.cas().read(&image.id)?;
    let config_desc = serde_json::json!({
        "mediaType": "application/vnd.docker.container.image.v1+json",
        "size": config_data.len(),
        "digest": with_prefix(&image.id),
    });
    c.push_blob(&with_prefix(&image.id), &config_data)?;

    // 2) descritores + envio dos layers (os que faltarem no registo).
    let total = image.layers.len();
    let mut layer_descs = Vec::with_capacity(total);
    for (i, dg) in image.layers.iter().enumerate() {
        let data = store.cas().read(dg)?;
        eprintln!("layer {}/{}  {}", i + 1, total, &dg[..dg.len().min(19)]);
        c.push_blob(&with_prefix(dg), &data)?;
        layer_descs.push(serde_json::json!({
            "mediaType": layer_media_type(&data),
            "size": data.len(),
            "digest": with_prefix(dg),
        }));
    }

    // 3) manifesto schema-2 + publicação sob a tag.
    let media_type = "application/vnd.docker.distribution.manifest.v2+json";
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": media_type,
        "config": config_desc,
        "layers": layer_descs,
    });
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    c.push_manifest(&refr, &manifest_bytes, media_type)?;

    let digest = format!("sha256:{}", sha256_hex(&manifest_bytes));
    eprintln!("publicado: {host}/{repo}:{refr}  ({digest})");
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::{layer_media_type, parse_reference, with_prefix};

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
}
