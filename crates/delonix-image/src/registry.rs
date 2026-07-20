//! Pull a partir de um registo OCI (Docker Registry HTTP API V2).
//!
//! Suporta o Docker Hub por omissão (com token anónimo) e qualquer registo
//! público que use o protocolo V2 (ghcr.io, quay.io, registry.k8s.io, ...).
//! O fluxo: resolve a referência → manifesto (escolhe a plataforma se for um
//! índice multi-arch) → blob de config → blobs de layers → guarda no CAS, tal
//! como o `load_docker_archive`.

use crate::cas::sha256_hex;
use crate::image::{now_unix, Image, ImageConfig, ImageStore};
use delonix_runtime_core::{Error, Result};
// Tipos OCI canónicos (crate `oci-spec`, feature `image`) — substituem os structs
// hand-rolled do schema OCI/distribution que existiam aqui (C3-IMG).
use oci_spec::image::{
    Descriptor, DescriptorBuilder, Digest, ImageConfiguration, ImageIndex, ImageManifest,
    ImageManifestBuilder, MediaType,
};
use std::str::FromStr;
use std::time::Duration;

/// Converte um erro do `oci-spec` (construção/validação de tipos OCI) num
/// [`Error::Registry`], para não vazar o tipo de erro da crate externa.
fn oci_err(e: impl std::fmt::Display) -> Error {
    Error::Registry(format!("oci-spec: {e}"))
}

/// Tipos de média aceites ao pedir um manifesto (índice OU manifesto de imagem).
const ACCEPT_MANIFEST: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";

fn reg_err(e: reqwest::Error) -> Error {
    Error::Registry(e.to_string())
}

/// Separa a referência em (host da API, repositório, tag/digest), aplicando as
/// regras do Docker: registo por omissão `registry-1.docker.io`, imagens
/// oficiais sob `library/`.
fn parse_reference(input: &str) -> (String, String, String) {
    // tag (`:`) ou digest (`@`) — o `:` tem de estar DEPOIS da última `/`.
    let (name, reference) = if let Some(idx) = input.find('@') {
        // `repo:tag@digest` (formato combinado, válido em Docker/OCI — o digest
        // manda na resolução, a tag é só informativa) — cortar a tag ANTES do
        // `@`, senão `name` fica com a tag lá dentro (`repo:tag`) e a URL do
        // manifesto sai malformada. Achado ao testar `kindest/node:vX@sha256:…`.
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
            Err(Error::NotFound(format!(
                "image {}:{}",
                self.repo,
                url.rsplit('/').next().unwrap_or("")
            )))
        } else {
            Err(Error::Registry(format!("HTTP {status} at {url}")))
        }
    }

    /// Pede um token ao serviço de autenticação indicado no 401. Com
    /// `force_scope`, pede esse âmbito (ex.: `…:pull,push` para o `push`) em vez
    /// do indicado pelo servidor — o servidor concede-o se as credenciais o
    /// permitirem.
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
        // Registo privado: autentica o pedido de token com Basic (user:password).
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

    /// Descarrega um blob em STREAMING, chamando `progress(bytes_lidos, total)` à
    /// medida que avança — o total vem do `Content-Length` (pode faltar em
    /// respostas chunked, daí o `Option`). Ler em pedaços em vez de `.bytes()`
    /// (que carrega tudo antes de devolver) é o que permite uma barra de
    /// progresso: um artefacto VM tem centenas de MB e sem isto o `pull` parece
    /// pendurado. O crate de motor só REPORTA os bytes; quem desenha é o bin.
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

    /// Envia um blob (config ou layer) por upload monolítico: `POST` para abrir
    /// a sessão, depois `PUT …?digest=<sha256>` com o conteúdo.
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
            return Err(Error::Registry(format!("blob PUT {digest}: HTTP {status}")));
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
            return Err(Error::Registry(format!(
                "manifest PUT: HTTP {status} {detail}"
            )));
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

/// Media types Docker schema-2 (mantidos para bater byte-a-byte com o que o
/// `docker`/registos esperam; no `oci_spec` viram `MediaType::Other(...)`).
const DOCKER_CONFIG_MEDIA_TYPE: &str = "application/vnd.docker.container.image.v1+json";
const DOCKER_MANIFEST_MEDIA_TYPE: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// Constrói um [`Descriptor`] OCI (`oci_spec`) a partir de um mediaType, tamanho
/// e digest (com ou sem prefixo `sha256:`). Centraliza a validação do digest
/// (`Digest::from_str`) e a construção via builder.
fn descriptor(media_type: &str, size: usize, digest: &str) -> Result<Descriptor> {
    DescriptorBuilder::default()
        .media_type(media_type)
        .size(size as u64)
        .digest(Digest::from_str(&with_prefix(digest)).map_err(oci_err)?)
        .build()
        .map_err(oci_err)
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
        // 600s (era 120s) — alinhado com push_to_registry/push_oci_artifact:
        // blobs de imagens grandes (ex. kindest/node, várias centenas de MB)
        // não cabem num deadline de 120s; `reqwest` corta a leitura do corpo
        // a meio, reportado como "error decoding response body" (não é um
        // erro de parsing — é uma leitura de stream interrompida).
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
        return Err(Error::Registry(format!("HTTP {} at {url}", resp.status())));
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

/// Descarrega `reference` de um registo OCI para o armazém local. Credenciais
/// (se existirem) vêm do `delonix login` local (`<root>/auth.json`).
pub fn pull_from_registry(store: &ImageStore, reference: &str) -> Result<Image> {
    pull_from_registry_with_creds(store, reference, None)
}

/// Como [`pull_from_registry`], mas com credenciais explícitas
/// (`creds_override = Some((user, password))`), usadas EM VEZ do
/// `delonix login` local — para chamadores que já recebem credenciais de
/// outra fonte (ex.: o CRI, que recebe `AuthConfig` do kubelet a partir dos
/// `imagePullSecrets` do Pod — não pode confiar só no `auth.json` local do
/// nó, que pode nem ter as credenciais daquele tenant). `None` mantém o
/// comportamento antigo (lookup local).
pub fn pull_from_registry_with_creds(
    store: &ImageStore,
    reference: &str,
    creds_override: Option<(String, String)>,
) -> Result<Image> {
    let (host, repo, refr) = parse_reference(reference);
    let http = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        // 600s — ver o mesmo comentário em `registry_client`.
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
        // Índice multi-arch (`oci_spec::image::ImageIndex`) — escolhe a entrada
        // linux/<arch> (ou a primeira, em falta de match).
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

    // Manifesto de imagem (`oci_spec::image::ImageManifest`) — schema OCI/Docker v2.
    let manifest: ImageManifest = serde_json::from_slice(&manifest_bytes)?;

    // 2) blob de config (= id da imagem)
    let config_digest_str = manifest.config().digest().to_string();
    let config_bytes = c.blob(&config_digest_str)?;
    if sha256_hex(&config_bytes) != manifest.config().digest().digest() {
        return Err(Error::Registry("config digest mismatch".into()));
    }
    let config_digest = store.cas().write(&config_bytes)?;

    // 3) layers (ignora layers "foreign"/Windows)
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

    // 4) monta e guarda — lê a config de execução (Cmd/Env/Entrypoint/User/WorkingDir)
    // do blob de config OCI (`oci_spec::image::ImageConfiguration`).
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
    let manifest = docker_manifest(store, image)?;
    let bytes = serde_json::to_vec(&manifest)?;
    let digest = format!("sha256:{}", crate::cas::sha256_hex(&bytes));
    Ok((bytes, digest))
}

/// Constrói o [`ImageManifest`] Docker schema-2 de uma imagem local (config +
/// descritores dos layers, mediaType detetado por magic number). Partilhado por
/// [`build_manifest`] (servir) e [`push_to_registry`] (publicar).
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

    // 1) envio do blob de config.
    let config_data = store.cas().read(&image.id)?;
    c.push_blob(&with_prefix(&image.id), &config_data)?;

    // 2) envio dos layers (os que faltarem no registo).
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

    // 3) manifesto Docker schema-2 (`oci_spec::image::ImageManifest`) + publicação
    // sob a tag. Mesma construção partilhada por `build_manifest`.
    let manifest = docker_manifest(store, &image)?;
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    c.push_manifest(&refr, &manifest_bytes, DOCKER_MANIFEST_MEDIA_TYPE)?;

    let digest = format!("sha256:{}", sha256_hex(&manifest_bytes));
    tracing::info!(host = %host, repo = %repo, reference = %refr, digest = %digest, "pushed: {host}/{repo}:{refr}");
    Ok(digest)
}

/// Media type do config vazio de um artefacto OCI 1.1 (convenção ORAS/Helm
/// para artefactos que não são imagens de container).
const EMPTY_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.empty.v1+json";
const EMPTY_CONFIG_BYTES: &[u8] = b"{}";

/// Publica `data` como artefacto OCI 1.1 de **blob único** (config vazio + 1
/// layer) — usado para imagens de VM (qcow2), que não são imagens de
/// container (essas usam [`push_to_registry`], com layers/config Docker). Só
/// generaliza o manifesto: reaproveita o mesmo [`Client`] (auth/upload) já
/// testado. `root` só é usado para `crate::auth::lookup` (credenciais de
/// `delonix login`) — sem `ImageStore`/CAS envolvido, é um blob solto.
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

    // Manifesto de artefacto OCI 1.1 (`oci_spec::image::ImageManifest` com
    // `artifactType` + config vazio `EmptyJSON`), padrão ORAS/Helm.
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

/// Pull de um artefacto publicado por [`push_oci_artifact`] — resolve o
/// manifesto e devolve os bytes do (único) layer.
pub fn pull_oci_artifact(root: &std::path::Path, source: &str) -> Result<Vec<u8>> {
    pull_oci_artifact_with_progress(root, source, None)
}

/// Como [`pull_oci_artifact`], mas com um callback de progresso do download do
/// blob (`(bytes_lidos, total)`), para uma barra de progresso no chamador.
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

    // Achado de auditoria de segurança: o caminho antigo (`pull_from_registry_with_creds`)
    // já verifica cada blob contra o digest esperado antes de o aceitar — este caminho
    // (artefactos de blob único, ex. imagens VM) tinha ficado sem essa verificação, o
    // que deixava um registo comprometido/MITM-ao-conteúdo servir bytes diferentes do
    // digest anunciado sem detecção. Ver CLAUDE.md.
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

    /// Achado ao testar `kindest/node:v1.34.0@sha256:...` (imagem base do
    /// `kind`) — o ramo `@` de `parse_reference` não cortava a tag antes do
    /// `@`, deixando `name` (e por isso `repo`) com a tag lá dentro
    /// (`kindest/node:v1.34.0`), o que produzia uma URL de manifesto
    /// malformada. `repo:tag@digest` é uma referência válida em Docker/OCI —
    /// o digest manda na resolução, a tag é só informativa.
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

    /// Servidor HTTP mínimo (uma ligação, uma resposta canónica) — o suficiente
    /// para simular um registo OCI que exige token e capturar o header
    /// `Authorization` que o cliente enviou ao pedir esse token.
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
            // 1.ª ligação: pedido do manifesto → 401 + WWW-Authenticate a apontar
            // para o endpoint de token NESTE MESMO servidor.
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

            // 2.ª ligação: pedido do TOKEN → é aqui que capturamos o Authorization
            // (Basic) que o `pull_from_registry_with_creds` gerou a partir das
            // credenciais (override ou lookup local).
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
        // resposta ao pedido de token: 401 de novo (não precisamos de completar o
        // pull — só de observar o Authorization enviado no pedido de token).
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
        // SEM `delonix login` local (auth.json não existe) — se a precedência
        // estivesse errada (override ignorado, só lookup local), o Authorization
        // capturado seria None (sem creds nenhumas).
        let reference = format!("127.0.0.1:{port}/repo:tag");
        let _ = pull_from_registry_with_creds(
            &store,
            &reference,
            Some(("cri-user".to_string(), "cri-pass".to_string())),
        ); // espera-se erro (2.º 401) — só nos interessa o Authorization capturado.

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

    /// Mock mínimo de um registo OCI ANÓNIMO (sem desafio 401 — como um
    /// `ghcr.io` público ou um registo local sem auth): guarda blobs/manifestos
    /// em memória e serve-os de volta. O suficiente para um round-trip real de
    /// `push_oci_artifact`→`pull_oci_artifact` sem depender de rede.
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
                // lê cabeçalhos (até \r\n\r\n), depois o corpo pelo Content-Length.
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

    /// Achado de auditoria de segurança: `pull_oci_artifact` tem de recusar um blob cujo
    /// conteúdo real não bate certo com o digest declarado no manifesto — simula um
    /// registo comprometido/adulterado que serve bytes diferentes sob o mesmo digest.
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
        // `push_oci_artifact` devolve o digest do MANIFESTO, não do layer/blob — o que
        // precisamos adulterar é o blob (o mesmo `layer_digest` que o pull vai buscar).
        let layer_digest = format!("sha256:{}", sha256_hex(&payload));
        push_oci_artifact(
            &tmp,
            &target,
            "application/vnd.delonix.vmimage.v1.qcow2",
            &payload,
        )
        .unwrap();

        // Simula adulteração directa no armazenamento do registo: substitui os bytes
        // guardados sob o MESMO digest (o manifesto continua a apontar para `layer_digest`,
        // mas o conteúdo real mudou) — o que um `push_blob` normal nunca faria (dedup
        // por HEAD), mas um registo comprometido/backend adulterado poderia.
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

    /// Round-trip determinístico do MANIFESTO pelos tipos `oci_spec::image`
    /// (C3-IMG): parte-se de bytes de manifesto FIXOS (Docker schema-2, digests
    /// reais de 64 hex), parseia-se com `ImageManifest`, confirma-se a estrutura
    /// (digest do config, ordem/digests/mediaType dos layers) e re-serializa-se —
    /// a re-serialização tem de ser IDEMPOTENTE (digest estável) e o re-parse tem
    /// de dar um `ImageManifest` igual. Sem rede: prova que a migração para
    /// `oci-spec` preserva o schema no caminho de pull/push.
    #[test]
    fn manifesto_round_trip_via_oci_spec_preserva_estrutura_e_digest() {
        use oci_spec::image::ImageManifest;

        // Manifesto Docker schema-2 canónico (config + 2 layers, ordem base→topo).
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
        // ordem dos layers preservada (base=0 → topo).
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

        // 2) re-serialização idempotente (digest estável).
        let bytes1 = serde_json::to_vec(&m).expect("serialize 1");
        let m2: ImageManifest = serde_json::from_slice(&bytes1).expect("re-parse");
        let bytes2 = serde_json::to_vec(&m2).expect("serialize 2");
        assert_eq!(
            sha256_hex(&bytes1),
            sha256_hex(&bytes2),
            "a re-serialização do manifesto tem de ser byte-idêntica (digest estável)"
        );
        // 3) o re-parse é estruturalmente igual (PartialEq de ImageManifest).
        assert_eq!(
            m, m2,
            "round-trip do manifesto tem de preservar a estrutura"
        );
    }
}
