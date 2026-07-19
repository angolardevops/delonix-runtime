//! `delonix ingress-proxy` — o reverse-proxy L7/HTTP embutido que serve os
//! `kind: HTTPRoute` (ver `cmd/httproute.rs`). Subcomando OCULTO: não é para o
//! utilizador o correr à mão — a Fase 4 lança-o DENTRO do netns do holder (onde
//! alcança os backends por IP) e publica as portas de entrada no host.
//!
//! **Fase 2 (este ficheiro):** o núcleo do proxy — servidor `hyper` (http1),
//! roteamento por `Host` + prefixo de path para `backend.ip:porta`, encaminhamento
//! com streaming de corpo (sem bufferizar). TLS (Fase 3) e o ciclo de vida/spawn
//! (Fase 4) vêm a seguir; um `listener` com `tls: true` é saltado com aviso aqui.
//!
//! A config é um JSON simples escrito pela Fase 4 (`ProxyConfig`) — rotas já
//! resolvidas para `ip:porta` (o proxy não fala com nenhum store).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use delonix_runtime_core::{Error, Result};

/// A config de runtime do proxy (escrita pela Fase 4, lida por `run`). Rotas já
/// resolvidas — o proxy não conhece containers nem stores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub listeners: Vec<Listener>,
    pub routes: Vec<Route>,
    /// Material TLS já resolvido pela Fase 4 (self-signed gerado OU cert/chave do
    /// `kind: Secret`). Presente ⇒ os listeners `tls: true` terminam TLS com ele.
    #[serde(default)]
    pub tls: Option<TlsMaterial>,
}

/// Cert + chave em PEM, prontos a carregar no rustls (a Fase 4 resolve-os).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsMaterial {
    #[serde(rename = "certPem")]
    pub cert_pem: String,
    #[serde(rename = "keyPem")]
    pub key_pem: String,
}

/// Gera um par cert+chave **self-signed** (PEM) para os `hosts` dados (SANs).
/// Usado pela Fase 4 quando `tls.mode: selfSigned`. Sem hosts → `localhost`.
pub fn self_signed_pem(hosts: &[String]) -> Result<TlsMaterial> {
    let sans: Vec<String> = if hosts.is_empty() {
        vec!["localhost".into()]
    } else {
        hosts.to_vec()
    };
    let ck = rcgen::generate_simple_self_signed(sans).map_err(|e| Error::Runtime {
        context: "self-signed cert",
        message: e.to_string(),
    })?;
    Ok(TlsMaterial {
        cert_pem: ck.cert.pem(),
        key_pem: ck.key_pair.serialize_pem(),
    })
}

/// Uma porta de escuta do proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listener {
    pub port: u16,
    #[serde(default)]
    pub tls: bool,
}

/// Uma rota resolvida: casa por `host` (vazio = qualquer) + prefixo `path`, e
/// encaminha para `backend` (`ip:porta`, já resolvido do record do container).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    #[serde(default)]
    pub host: String,
    pub path: String,
    pub backend: String,
}

/// O corpo de resposta unificado (proxiado OU gerado localmente p/ 404/502).
type RespBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// Tabela de rotas partilhada e **trocável a quente**: o `SIGHUP` relê a config e
/// substitui o `Arc` interno (os listeners continuam de pé). Cada pedido lê um
/// snapshot (`clone` do Arc) sob um read-lock curtíssimo — o auto-registo de
/// containers (rota nova sem reiniciar o proxy) assenta nisto.
type SharedRoutes = Arc<std::sync::RwLock<Arc<Vec<Route>>>>;

/// Teto de tempo para o backend responder (senão 504) — evita que um backend
/// pendurado prenda a ligação/task para sempre (slowloris do lado do backend).
const BACKEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Teto para o cliente enviar os headers completos — corta o slowloris clássico.
const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Teto para o handshake TLS completar — corta o slowloris de handshake.
const TLS_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Remove os headers **hop-by-hop** (RFC 7230 §6.1) de um `HeaderMap`, incluindo
/// os tokens listados no próprio `Connection:`. Um proxy NÃO os pode reencaminhar:
/// pertencem a UMA ligação (a nossa com o cliente / a nossa com o backend), não à
/// mensagem — deixá-los passar corrompe o enquadramento (o hyper reenquadra o
/// corpo por cima de um `Transfer-Encoding` do cliente → risco de smuggling) e
/// vaza `Connection: close`/`Keep-Alive` para o outro lado.
fn strip_hop_by_hop(headers: &mut hyper::HeaderMap) {
    use hyper::header::{HeaderName, CONNECTION};
    // Os nomes listados no(s) header(s) `Connection` são eles próprios hop-by-hop.
    let mut listed: Vec<HeaderName> = Vec::new();
    for v in headers.get_all(CONNECTION) {
        if let Ok(s) = v.to_str() {
            for tok in s.split(',') {
                if let Ok(name) = HeaderName::from_bytes(tok.trim().as_bytes()) {
                    listed.push(name);
                }
            }
        }
    }
    const HOP: [&str; 8] = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];
    for h in HOP {
        headers.remove(h);
    }
    headers.remove("proxy-connection"); // não-standard mas comum
    for h in listed {
        headers.remove(&h);
    }
}

/// Escolhe a melhor rota para um `(host, path)`: primeiro as rotas com `host`
/// específico (sobre as de qualquer-host), depois o prefixo de path mais LONGO
/// (o mais específico ganha). `None` = nenhuma casa.
fn pick_route<'a>(routes: &'a [Route], host: &str, path: &str) -> Option<&'a Route> {
    routes
        .iter()
        .filter(|r| (r.host.is_empty() || r.host == host) && path.starts_with(&r.path))
        .max_by_key(|r| (usize::from(!r.host.is_empty()), r.path.len()))
}

/// O `Host` do pedido, sem a porta (`loja.exemplo.ao:80` → `loja.exemplo.ao`).
fn req_host(req: &Request<Incoming>) -> String {
    req.headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().host())
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Uma resposta de erro simples (404/502) com corpo de texto.
fn text_response(code: StatusCode, msg: &str) -> Response<RespBody> {
    let body = Full::new(Bytes::from(msg.to_string()))
        .map_err(|e: Infallible| match e {})
        .boxed();
    Response::builder()
        .status(code)
        .body(body)
        .expect("resposta estática válida")
}

/// Trata um pedido: casa a rota e encaminha (streaming) para o backend, ou
/// devolve 404 (sem rota) / 502 (backend inacessível).
async fn handle(
    req: Request<Incoming>,
    routes: SharedRoutes,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
) -> std::result::Result<Response<RespBody>, Infallible> {
    let host = req_host(&req);
    let path = req.uri().path().to_string();
    // Snapshot das rotas (o SIGHUP pode trocá-las a qualquer momento).
    let snapshot = routes
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|p| p.into_inner().clone());
    let Some(route) = pick_route(&snapshot, &host, &path) else {
        return Ok(text_response(
            StatusCode::NOT_FOUND,
            "delonix: sem rota para este host/path\n",
        ));
    };
    let backend = route.backend.clone();

    // Reconstrói o pedido para o backend: mesma method/headers/corpo, URI absoluta
    // `http://<backend><path?query>`. O corpo (`Incoming`) é reencaminhado sem
    // bufferizar (streaming) — hyper transfere-o à medida que chega.
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let uri = format!("http://{backend}{pq}");
    let (parts, mut headers, body) = {
        let (p, b) = req.into_parts();
        (p.method, p.headers, b)
    };
    // Remove os hop-by-hop ANTES de encaminhar (o Host end-to-end mantém-se — o
    // backend pode precisar dele para virtual-hosting).
    strip_hop_by_hop(&mut headers);
    let mut out = Request::builder().method(parts).uri(&uri);
    if let Some(h) = out.headers_mut() {
        *h = headers;
    }
    let out_req = match out.body(body) {
        Ok(r) => r,
        Err(_) => {
            return Ok(text_response(
                StatusCode::BAD_GATEWAY,
                "delonix: pedido inválido\n",
            ))
        }
    };

    // Teto de tempo: um backend pendurado não pode prender a ligação para sempre.
    match tokio::time::timeout(BACKEND_TIMEOUT, client.request(out_req)).await {
        Ok(Ok(resp)) => {
            // A resposta do backend flui de volta em streaming; também lhe tiramos
            // os hop-by-hop antes de a devolver ao cliente.
            let (mut rparts, body) = resp.into_parts();
            strip_hop_by_hop(&mut rparts.headers);
            let body = body
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                .boxed();
            Ok(Response::from_parts(rparts, body))
        }
        Ok(Err(e)) => Ok(text_response(
            StatusCode::BAD_GATEWAY,
            &format!("delonix: backend {backend} inacessível: {e}\n"),
        )),
        Err(_elapsed) => Ok(text_response(
            StatusCode::GATEWAY_TIMEOUT,
            &format!(
                "delonix: backend {backend} não respondeu em {}s\n",
                BACKEND_TIMEOUT.as_secs()
            ),
        )),
    }
}

/// Serve UMA ligação já estabelecida (TCP ou TLS): `io` é qualquer IO que o hyper
/// saiba ler/escrever. Genérico para não duplicar o caminho TLS e o plano.
async fn serve_io<I>(
    io: I,
    routes: SharedRoutes,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
) where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + 'static,
{
    let svc = service_fn(move |req| handle(req, routes.clone(), client.clone()));
    // NOTA: WebSocket/`Connection: Upgrade` ainda NÃO é tunelado — o cliente
    // hyper-util legacy não estabelece a ligação trocada, e nós removemos o header
    // `Upgrade` (hop-by-hop) no encaminhamento. Tunelar upgrades (hyper::upgrade::on
    // nos dois lados + cópia bidireccional) é um follow-up; hoje só HTTP
    // request/response é proxiado.
    //
    // `header_read_timeout` corta o slowloris clássico (headers a conta-gotas) — o
    // `timer` é obrigatório para o hyper o poder aplicar.
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .serve_connection(io, svc)
        .await;
}

/// Aceita ligações numa porta e serve cada uma. Com `tls` presente, faz o
/// handshake TLS antes de servir (termina TLS no proxy); senão, HTTP simples.
async fn accept_loop(
    listener: TcpListener,
    routes: SharedRoutes,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
    tls: Option<tokio_rustls::TlsAcceptor>,
) {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => {
                // Um erro persistente (EMFILE/ENFILE sob esgotamento de fds) num
                // `continue` nu gira um busy-loop a queimar CPU — pausa curta.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                continue;
            }
        };
        let routes = routes.clone();
        let client = client.clone();
        let tls = tls.clone();
        tokio::task::spawn(async move {
            match tls {
                // Timeout no handshake: um cliente que abre TCP e nunca completa o
                // ClientHello não pode segurar a task para sempre (slowloris TLS).
                Some(acceptor) => {
                    match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
                    {
                        Ok(Ok(tls_stream)) => {
                            serve_io(TokioIo::new(tls_stream), routes, client).await
                        }
                        Ok(Err(e)) => eprintln!("ingress-proxy: handshake TLS falhou: {e}"),
                        Err(_) => { /* handshake não completou a tempo — descarta */ }
                    }
                }
                None => serve_io(TokioIo::new(stream), routes, client).await,
            }
        });
    }
}

/// Constrói o `ServerConfig` do rustls a partir do PEM (cert-chain + chave). O
/// provider criptográfico (`ring`) tem de estar instalado (ver `run`).
///
/// **Limitação v1 (SNI):** um ÚNICO cert serve todos os hosts (`with_single_cert`).
/// Para vários hosts com certs distintos era preciso um `ResolvesServerCert` por
/// SNI — follow-up. Hoje o self-signed cobre todos os hosts do HTTPRoute num só
/// cert (multi-SAN), e o modo BYO assume um cert que sirva todos os hosts.
fn build_server_config(tls: &TlsMaterial) -> Result<Arc<tokio_rustls::rustls::ServerConfig>> {
    use tokio_rustls::rustls::ServerConfig;
    let certs: Vec<_> = rustls_pemfile::certs(&mut tls.cert_pem.as_bytes())
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Invalid(format!("cert TLS inválido (PEM): {e}")))?;
    if certs.is_empty() {
        return Err(Error::Invalid(
            "cert TLS vazio (nenhum CERTIFICATE no PEM)".into(),
        ));
    }
    let key = rustls_pemfile::private_key(&mut tls.key_pem.as_bytes())
        .map_err(|e| Error::Invalid(format!("chave TLS inválida (PEM): {e}")))?
        .ok_or_else(|| Error::Invalid("chave TLS ausente (nenhuma PRIVATE KEY no PEM)".into()))?;
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| Error::Invalid(format!("cert/chave TLS incompatíveis: {e}")))?;
    Ok(Arc::new(cfg))
}

/// Núcleo assíncrono: bind de cada listener + servir, com a tabela de rotas
/// trocável a quente por `SIGHUP` (relê `config_path`). Os listeners e o material
/// TLS ficam FIXOS no arranque (mudá-los exige reiniciar); só as ROTAS recarregam
/// — é o que o auto-registo de containers precisa.
async fn serve(cfg: ProxyConfig, config_path: std::path::PathBuf) -> Result<()> {
    let client: Client<_, Incoming> = Client::builder(TokioExecutor::new())
        .build(hyper_util::client::legacy::connect::HttpConnector::new());

    // Um só ServerConfig TLS partilhado por todos os listeners TLS (se houver
    // material). Construído UMA vez — o rustls guarda-o num Arc.
    let tls_acceptor: Option<tokio_rustls::TlsAcceptor> = match &cfg.tls {
        Some(mat) => Some(tokio_rustls::TlsAcceptor::from(build_server_config(mat)?)),
        None => None,
    };

    // Tabela de rotas partilhada e trocável a quente.
    let routes: SharedRoutes = Arc::new(std::sync::RwLock::new(Arc::new(cfg.routes.clone())));

    // SIGHUP → relê a config e substitui SÓ as rotas (listeners/TLS ficam).
    {
        let routes = routes.clone();
        let path = config_path.clone();
        tokio::spawn(async move {
            let Ok(mut hup) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            else {
                return;
            };
            while hup.recv().await.is_some() {
                match std::fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<ProxyConfig>(&b).ok())
                {
                    Some(newcfg) => {
                        let n = newcfg.routes.len();
                        match routes.write() {
                            // Larga o write-lock ANTES do eprintln (o I/O de stderr
                            // não pode bloquear os handlers que fazem `read()`).
                            Ok(mut g) => *g = Arc::new(newcfg.routes),
                            Err(_) => {
                                eprintln!(
                                    "ingress-proxy: lock de rotas envenenado — reload ignorado"
                                );
                                continue;
                            }
                        }
                        eprintln!("ingress-proxy: rotas recarregadas ({n} rota(s))");
                    }
                    None => {
                        eprintln!("ingress-proxy: SIGHUP mas a config não releu — rotas mantidas")
                    }
                }
            }
        });
    }

    let mut handles = Vec::new();
    for l in &cfg.listeners {
        let acceptor = if l.tls {
            match &tls_acceptor {
                Some(a) => Some(a.clone()),
                None => {
                    return Err(Error::Invalid(format!(
                        "listener :{} pede TLS mas a config não tem material TLS (cert/chave)",
                        l.port
                    )));
                }
            }
        } else {
            None
        };
        let addr = SocketAddr::from(([0, 0, 0, 0], l.port));
        let listener = TcpListener::bind(addr).await.map_err(|e| Error::Runtime {
            context: "ingress-proxy bind",
            message: format!("{addr}: {e}"),
        })?;
        eprintln!(
            "ingress-proxy: a escutar em {addr} ({}, {} rota(s))",
            if l.tls { "https" } else { "http" },
            cfg.routes.len()
        );
        handles.push(tokio::spawn(accept_loop(
            listener,
            routes.clone(),
            client.clone(),
            acceptor,
        )));
    }
    if handles.is_empty() {
        return Err(Error::Invalid(
            "ingress-proxy: nenhum listener HTTP para servir".into(),
        ));
    }
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

/// Ponto de entrada do subcomando `delonix ingress-proxy --config <ficheiro>`.
/// Lê a `ProxyConfig` (JSON) e corre o servidor até morrer (bloqueia).
pub fn run(config_path: &Path) -> Result<()> {
    let bytes = std::fs::read(config_path).map_err(|e| {
        Error::Invalid(format!(
            "ingress-proxy: não li a config {}: {e}",
            config_path.display()
        ))
    })?;
    let cfg: ProxyConfig = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Invalid(format!("ingress-proxy: config inválida: {e}")))?;
    // Instala o provider criptográfico do rustls (ring) — o `ServerConfig::builder`
    // usa o default do processo; sem isto, entra em pânico. Idempotente (ignora se
    // já instalado por outra parte do processo).
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Runtime {
            context: "ingress-proxy runtime",
            message: e.to_string(),
        })?;
    rt.block_on(serve(cfg, config_path.to_path_buf()))
}

// ============================================================================
// Ciclo de vida (host-side): arrancar/recarregar/parar o proxy no netns do holder.
// O proxy é infra persistente (como o slirp/holder), lançado só quando há um
// HTTPRoute — respeita o 'daemonless' (não corre sem carga declarada).
// ============================================================================

/// Pasta de estado do proxy (`<root>/httproute/`). No mesmo sistema de ficheiros
/// que o holder vê (a mount-ns do holder é cópia da do host) — o proxy lá dentro
/// lê a MESMA config que aqui escrevemos.
fn proxy_dir() -> std::path::PathBuf {
    crate::cmd::util::state_root().join("httproute")
}
/// Caminho canónico da `ProxyConfig` (o proxy relê-o no SIGHUP).
pub fn config_path() -> std::path::PathBuf {
    proxy_dir().join("config.json")
}
fn pid_path() -> std::path::PathBuf {
    proxy_dir().join("proxy.pid")
}
fn log_path() -> std::path::PathBuf {
    proxy_dir().join("proxy.log")
}
/// Porta HTTP das auto-rotas (`--expose`). **Não-privilegiada** — em rootless o
/// slirp recusa publicar portas <1024. Alcança-se com `Host: <fqdn>` em `:8080`.
const AUTO_HTTP_PORT: u16 = 8080;

/// A parte MANUAL da config (rotas/listeners/TLS dos `kind: HTTPRoute`).
fn manual_path() -> std::path::PathBuf {
    proxy_dir().join("manual.json")
}
/// As rotas AUTO-REGISTADAS de containers (`container run --expose`).
fn auto_path() -> std::path::PathBuf {
    proxy_dir().join("auto.json")
}

/// Uma rota auto-registada de um container HTTP: o FQDN interno
/// `<nome>.<namespace>.delonix.internal` → `<ip>:<porta>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoRoute {
    pub name: String,
    pub namespace: String,
    pub ip: String,
    pub port: u16,
}

impl AutoRoute {
    /// O FQDN interno deste container (o `Host` que o casa no proxy + o nome DNS).
    pub fn fqdn(&self) -> String {
        format!("{}.{}.delonix.internal", self.name, self.namespace)
    }
}

fn read_manual() -> Option<ProxyConfig> {
    serde_json::from_slice(&std::fs::read(manual_path()).ok()?).ok()
}
fn read_auto() -> Vec<AutoRoute> {
    std::fs::read(auto_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Read-modify-write da `auto.json` sob **flock exclusivo** — dois
/// `container run --expose` em paralelo não podem perder uma rota (lost update).
/// `f` recebe a lista actual e devolve a nova. Devolve `true` se mudou.
fn with_auto_locked(f: impl FnOnce(&mut Vec<AutoRoute>)) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    std::fs::create_dir_all(proxy_dir()).map_err(|e| Error::Runtime {
        context: "httproute dir",
        message: e.to_string(),
    })?;
    // Um ficheiro de lock dedicado (o flock é no fd; o conteúdo fica na auto.json).
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(proxy_dir().join("auto.lock"))
        .map_err(|e| Error::Runtime {
            context: "auto.lock",
            message: e.to_string(),
        })?;
    // SAFETY: flock(LOCK_EX) no fd do lock; libertado ao fechar (fim do escopo).
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(Error::Runtime {
            context: "flock auto",
            message: "não obtive o lock".into(),
        });
    }
    let mut auto = read_auto();
    let before = auto.clone();
    f(&mut auto);
    if auto == before {
        return Ok(false);
    }
    std::fs::write(
        auto_path(),
        serde_json::to_vec_pretty(&auto).unwrap_or_default(),
    )
    .map_err(|e| Error::Runtime {
        context: "escrever auto",
        message: e.to_string(),
    })?;
    Ok(true)
}

/// **Compõe a config final** a partir da parte MANUAL (HTTPRoute) + as rotas
/// AUTO-REGISTADAS, e garante o proxy a servir (ou pára-o se ficou tudo vazio).
/// É o ponto único que o `httproute apply` e o auto-registo chamam — nenhuma
/// fonte apaga a outra.
fn rebuild() -> Result<()> {
    let manual = read_manual();
    let auto = read_auto();

    let mut listeners: Vec<Listener> = manual
        .as_ref()
        .map(|m| m.listeners.clone())
        .unwrap_or_default();
    let mut routes: Vec<Route> = manual
        .as_ref()
        .map(|m| m.routes.clone())
        .unwrap_or_default();
    let tls = manual.as_ref().and_then(|m| m.tls.clone());

    // As auto-rotas servem-se em HTTP na porta AUTO_HTTP_PORT (FQDN interno). NÃO
    // :80 — em rootless o slirp não publica portas privilegiadas (add_hostfwd
    // recusa <1024). Garante o listener se houver alguma auto-rota.
    if !auto.is_empty() && !listeners.iter().any(|l| l.port == AUTO_HTTP_PORT) {
        listeners.push(Listener {
            port: AUTO_HTTP_PORT,
            tls: false,
        });
    }
    for a in &auto {
        routes.push(Route {
            host: a.fqdn(),
            path: "/".into(),
            backend: format!("{}:{}", a.ip, a.port),
        });
    }

    if listeners.is_empty() || routes.is_empty() {
        // Nada declarado (nem manual nem auto) → o proxy não tem razão de existir.
        return stop();
    }
    ensure_running(&ProxyConfig {
        listeners,
        routes,
        tls,
    })
}

/// Escreve a parte MANUAL (do `httproute apply`) e recompõe a config final.
pub fn set_manual(cfg: &ProxyConfig) -> Result<()> {
    std::fs::create_dir_all(proxy_dir()).map_err(|e| Error::Runtime {
        context: "httproute dir",
        message: e.to_string(),
    })?;
    std::fs::write(
        manual_path(),
        serde_json::to_vec_pretty(cfg).unwrap_or_default(),
    )
    .map_err(|e| Error::Runtime {
        context: "escrever manual",
        message: e.to_string(),
    })?;
    rebuild()
}

/// Remove a parte MANUAL (no `httproute rm`) e recompõe — as rotas
/// auto-registadas de containers `--expose` SOBREVIVEM (o proxy só pára se nada
/// mais restar). Devolve `true` se havia rotas manuais.
pub fn clear_manual() -> Result<bool> {
    let had = manual_path().exists();
    let _ = std::fs::remove_file(manual_path());
    rebuild()?;
    Ok(had)
}

/// **Auto-regista** um container HTTP no proxy (`container run --expose`): junta/
/// actualiza a sua `AutoRoute` e recompõe a config (SIGHUP a quente). Idempotente.
pub fn auto_register(name: &str, namespace: &str, ip: &str, port: u16) -> Result<()> {
    let entry = AutoRoute {
        name: name.to_string(),
        namespace: namespace.to_string(),
        ip: ip.to_string(),
        port,
    };
    with_auto_locked(|auto| {
        auto.retain(|a| a.name != name); // substitui uma entrada anterior do mesmo nome
        auto.push(entry.clone());
    })?;
    rebuild()
}

/// **Remove** o auto-registo de um container (no `container rm`/stop) e recompõe.
/// Best-effort — se o container não estava registado, não faz nada.
pub fn auto_deregister(name: &str) {
    // `Ok(true)` = removeu algo → recompõe; `Ok(false)`/`Err` = não estava
    // registado (ou o lock falhou) — nada a recompor.
    if let Ok(true) = with_auto_locked(|auto| auto.retain(|a| a.name != name)) {
        let _ = rebuild();
    }
}

/// PID do proxy se estiver VIVO **e for mesmo o nosso** (o `/proc/<pid>/cmdline`
/// contém `ingress-proxy`), senão `None` (e limpa um pidfile órfão). A guarda de
/// identidade é essencial: sem ela, um PID reciclado pelo kernel faria o
/// `SIGHUP`/`SIGTERM` atingir um processo alheio (SIGHUP default = terminar).
fn running_pid() -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(pid_path())
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let is_ours = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| String::from_utf8_lossy(&c).contains("ingress-proxy"))
        .unwrap_or(false);
    if is_ours {
        Some(pid)
    } else {
        let _ = std::fs::remove_file(pid_path()); // morto OU PID reciclado — órfão
        None
    }
}

/// Escreve a config e **garante o proxy a servir**: se já está vivo, recarrega a
/// quente (SIGHUP); senão, arranca-o no netns do holder e publica as portas.
/// Idempotente — é o que o `stack apply`/auto-registo chamam sempre.
pub fn ensure_running(cfg: &ProxyConfig) -> Result<()> {
    std::fs::create_dir_all(proxy_dir()).map_err(|e| Error::Runtime {
        context: "httproute dir",
        message: e.to_string(),
    })?;
    // Captura os listeners ACTUAIS ANTES de sobrescrever a config (senão o prev
    // seria já o novo).
    let prev_ports = prev_listener_ports();
    let json = serde_json::to_vec_pretty(cfg).map_err(|e| Error::Runtime {
        context: "serialize config",
        message: e.to_string(),
    })?;
    std::fs::write(config_path(), &json).map_err(|e| Error::Runtime {
        context: "escrever config",
        message: e.to_string(),
    })?;

    if let Some(pid) = running_pid() {
        // Vivo → recarrega as rotas a quente (SIGHUP). As portas já estão publicadas.
        // AVISO: o SIGHUP só recarrega ROTAS; mudar entrypoints/TLS exige reiniciar
        // (`httproute rm` + apply). Detetamos a mudança do conjunto de portas para
        // não mentir que o novo listener está a servir.
        if let Some(prev) = prev_ports {
            let now: std::collections::BTreeSet<u16> =
                cfg.listeners.iter().map(|l| l.port).collect();
            if prev != now {
                eprintln!(
                    "httproute: AVISO — mudança de listeners ({prev:?} → {now:?}) NÃO tem efeito a quente; \
                     o SIGHUP só recarrega rotas. Faz `httproute rm` + apply para religar as portas."
                );
            }
        }
        // SAFETY: SIGHUP a um pid que confirmámos vivo E nosso (guarda de cmdline).
        unsafe { libc::kill(pid, libc::SIGHUP) };
        eprintln!("httproute: proxy #{pid} recarregado (SIGHUP)");
        return Ok(());
    }
    spawn_proxy()?;
    publish_listeners(cfg)?;
    Ok(())
}

/// As portas dos listeners da config ACTUALMENTE em vigor (antes de a
/// sobrescrevermos) — para detetar uma mudança de listeners no re-apply.
fn prev_listener_ports() -> Option<std::collections::BTreeSet<u16>> {
    let bytes = std::fs::read(config_path()).ok()?;
    let cfg: ProxyConfig = serde_json::from_slice(&bytes).ok()?;
    Some(cfg.listeners.iter().map(|l| l.port).collect())
}

/// Arranca o proxy DENTRO do netns do holder (via `infra_join_argv`), detached
/// (setsid, stdio para um log), e grava o pidfile.
fn spawn_proxy() -> Result<()> {
    use std::os::unix::process::CommandExt;
    // Garante o holder de pé (o proxy vive no netns dele).
    delonix_net::infra::ensure_up()?;
    let join = delonix_net::infra::infra_join_argv().ok_or_else(|| Error::Runtime {
        context: "holder",
        message: "ingress holder em baixo".into(),
    })?;
    let self_exe = std::env::current_exe().map_err(|e| Error::Runtime {
        context: "current_exe",
        message: e.to_string(),
    })?;
    let cfg_path = config_path();

    // argv = nsenter … -- <delonix> ingress-proxy --config <config>
    let mut argv: Vec<String> = join;
    argv.push(self_exe.to_string_lossy().into_owned());
    argv.push("ingress-proxy".into());
    argv.push("--config".into());
    argv.push(cfg_path.to_string_lossy().into_owned());

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .map_err(|e| Error::Runtime {
            context: "abrir log do proxy",
            message: e.to_string(),
        })?;
    let log2 = log.try_clone().map_err(|e| Error::Runtime {
        context: "clone log",
        message: e.to_string(),
    })?;

    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log2));
    // SAFETY: setsid no filho (pós-fork, pré-exec) destaca-o da sessão/terminal
    // do CLI para sobreviver à saída deste. O nsenter faz EXEC (não fork) do
    // proxy, por isso este PID torna-se o do proxy — signalável do host.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| Error::Runtime {
        context: "spawn ingress-proxy",
        message: format!("{}: {e}", argv.join(" ")),
    })?;
    std::fs::write(pid_path(), child.id().to_string()).map_err(|e| Error::Runtime {
        context: "escrever pidfile",
        message: e.to_string(),
    })?;
    // Confirma que arrancou de facto (não morreu logo no bind): dá-lhe um instante
    // e verifica o /proc. Se caiu, aponta o log — não declarar 'a servir' a mentir.
    std::thread::sleep(std::time::Duration::from_millis(300));
    if !std::path::Path::new(&format!("/proc/{}", child.id())).exists() {
        return Err(Error::Runtime {
            context: "ingress-proxy",
            message: format!(
                "o proxy caiu logo ao arrancar (porta ocupada?) — ver {}",
                log_path().display()
            ),
        });
    }
    eprintln!(
        "httproute: proxy arrancado (#{}) no netns do holder",
        child.id()
    );
    Ok(())
}

/// Publica as portas de entrada no host (slirp `add_hostfwd`): o proxy escuta em
/// `0.0.0.0:<porta>` no netns do holder e apanha o tráfego entregue em `SLIRP_IP`.
/// (Sem DNAT — o holder não tem `input` chain a filtrar entregas locais.)
fn publish_listeners(cfg: &ProxyConfig) -> Result<()> {
    let sock = delonix_net::infra::slirp_sock_path();
    for l in &cfg.listeners {
        let p = l.port.to_string();
        // Best-effort/idempotente: se a porta JÁ tem hostfwd (proxy anterior que
        // crashou sem teardown), o slirp recusa 'already exists' — não é fatal, o
        // estado desejado (porta publicada) já está. Só avisa noutros erros.
        if let Err(e) = delonix_net::slirp_add_hostfwd(&sock, &p, &p, "tcp") {
            let msg = e.to_string();
            if msg.contains("already") || msg.to_lowercase().contains("exist") {
                eprintln!("httproute: porta :{p} já publicada — mantida");
            } else {
                eprintln!("httproute: aviso ao publicar :{p}: {e}");
            }
        }
    }
    Ok(())
}

/// **Pára o proxy e despublica as portas** (teardown do `httproute rm`). Lê os
/// portos da config antes de a apagar. Best-effort/idempotente.
pub fn stop() -> Result<()> {
    // Despublica as portas conhecidas (da config, se ainda existir).
    if let Ok(bytes) = std::fs::read(config_path()) {
        if let Ok(cfg) = serde_json::from_slice::<ProxyConfig>(&bytes) {
            let sock = delonix_net::infra::slirp_sock_path();
            for l in &cfg.listeners {
                let _ = delonix_net::infra::slirp_remove_hostfwd(&sock, &l.port.to_string());
            }
        }
    }
    if let Some(pid) = running_pid() {
        // SAFETY: SIGTERM a um pid confirmado vivo e nosso.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let _ = std::fs::remove_file(pid_path());
    let _ = std::fs::remove_file(config_path());
    // Teardown total: também as fontes (manual + auto), senão um arranque seguinte
    // reergueria rotas fantasma.
    let _ = std::fs::remove_file(manual_path());
    let _ = std::fs::remove_file(auto_path());
    Ok(())
}

/// Está o proxy a correr? (para o `httproute ls`/describe).
pub fn is_running() -> bool {
    running_pid().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(host: &str, path: &str, backend: &str) -> Route {
        Route {
            host: host.to_string(),
            path: path.to_string(),
            backend: backend.to_string(),
        }
    }

    #[test]
    fn pick_route_prefere_host_especifico_e_prefixo_mais_longo() {
        let routes = vec![
            r("", "/", "10.0.0.1:80"),           // qualquer host, raiz
            r("loja.ex", "/", "10.0.0.2:80"),    // host específico, raiz
            r("loja.ex", "/api", "10.0.0.3:80"), // host específico, /api (mais longo)
        ];
        // /api na loja → a rota /api (prefixo mais longo)
        assert_eq!(
            pick_route(&routes, "loja.ex", "/api/x").unwrap().backend,
            "10.0.0.3:80"
        );
        // / na loja → a rota host-específica, não a de qualquer-host
        assert_eq!(
            pick_route(&routes, "loja.ex", "/home").unwrap().backend,
            "10.0.0.2:80"
        );
        // outro host → só casa a de qualquer-host
        assert_eq!(
            pick_route(&routes, "outro.ex", "/api").unwrap().backend,
            "10.0.0.1:80"
        );
    }

    #[test]
    fn pick_route_sem_correspondencia() {
        let routes = vec![r("loja.ex", "/", "10.0.0.2:80")];
        assert!(pick_route(&routes, "outro.ex", "/").is_none());
    }

    #[test]
    fn self_signed_gera_pem_valido() {
        let mat = self_signed_pem(&["loja.exemplo.ao".into()]).unwrap();
        assert!(mat.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(mat.key_pem.contains("PRIVATE KEY"));
        // E o rustls consegue construir um ServerConfig com ele.
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        assert!(build_server_config(&mat).is_ok());
    }

    #[test]
    fn config_com_tls_roundtrip_json() {
        let cfg = ProxyConfig {
            listeners: vec![Listener {
                port: 443,
                tls: true,
            }],
            routes: vec![r("loja.ex", "/", "10.0.0.2:8080")],
            tls: Some(TlsMaterial {
                cert_pem: "C".into(),
                key_pem: "K".into(),
            }),
        };
        let js = serde_json::to_string(&cfg).unwrap();
        let back: ProxyConfig = serde_json::from_str(&js).unwrap();
        assert_eq!(back.tls.unwrap().cert_pem, "C");
    }

    #[test]
    fn config_roundtrip_json() {
        let cfg = ProxyConfig {
            listeners: vec![
                Listener {
                    port: 80,
                    tls: false,
                },
                Listener {
                    port: 443,
                    tls: true,
                },
            ],
            routes: vec![r("loja.ex", "/", "10.0.0.2:8080")],
            tls: None,
        };
        let js = serde_json::to_string(&cfg).unwrap();
        let back: ProxyConfig = serde_json::from_str(&js).unwrap();
        assert_eq!(back.listeners.len(), 2);
        assert_eq!(back.routes[0].backend, "10.0.0.2:8080");
    }
}
