//! `delonix ingress-proxy` — the embedded L7/HTTP reverse-proxy that serves the
//! `kind: HTTPRoute` (see `cmd/httproute.rs`). HIDDEN subcommand: it is not for the
//! user to run by hand — Phase 4 launches it INSIDE the holder's netns (where it
//! reaches the backends by IP) and publishes the inbound ports on the host.
//!
//! **Phase 2 (this file):** the proxy core — `hyper` server (http1),
//! routing by `Host` + path prefix to `backend.ip:port`, forwarding with
//! body streaming (no buffering). TLS (Phase 3) and the lifecycle/spawn
//! (Phase 4) come next; a `listener` with `tls: true` is skipped with a warning here.
//!
//! The config is a plain JSON written by Phase 4 (`ProxyConfig`) — routes already
//! resolved to `ip:port` (the proxy talks to no store).

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

/// The proxy's runtime config (written by Phase 4, read by `run`). Routes already
/// resolved — the proxy knows no containers or stores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub listeners: Vec<Listener>,
    pub routes: Vec<Route>,
    /// TLS material already resolved by Phase 4 (generated self-signed OR cert/key
    /// from a `kind: Secret`). Present ⇒ the `tls: true` listeners terminate TLS with it.
    #[serde(default)]
    pub tls: Option<TlsMaterial>,
}

/// Cert + key in PEM, ready to load into rustls (Phase 4 resolves them).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsMaterial {
    #[serde(rename = "certPem")]
    pub cert_pem: String,
    #[serde(rename = "keyPem")]
    pub key_pem: String,
}

/// Generates a **self-signed** cert+key pair (PEM) for the given `hosts` (SANs).
/// Used by Phase 4 when `tls.mode: selfSigned`. No hosts → `localhost`.
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

/// A listening port of the proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listener {
    pub port: u16,
    #[serde(default)]
    pub tls: bool,
}

/// A resolved route: matches by `host` (empty = any) + `path` prefix, and
/// forwards to `backend` (`ip:port`, already resolved from the container record).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    #[serde(default)]
    pub host: String,
    pub path: String,
    pub backend: String,
}

/// The unified response body (proxied OR generated locally for 404/502).
type RespBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// Shared and **hot-swappable** route table: `SIGHUP` re-reads the config and
/// replaces the inner `Arc` (the listeners stay up). Each request reads a
/// snapshot (`clone` of the Arc) under a very short read-lock — container
/// auto-registration (a new route without restarting the proxy) rests on this.
type SharedRoutes = Arc<std::sync::RwLock<Arc<Vec<Route>>>>;

/// Time ceiling for the backend to respond (else 504) — keeps a hung backend
/// from holding the connection/task forever (backend-side slowloris).
const BACKEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Ceiling for the client to send the full headers — cuts the classic slowloris.
const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Ceiling for the TLS handshake to complete — cuts the handshake slowloris.
const TLS_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Removes the **hop-by-hop** headers (RFC 7230 §6.1) from a `HeaderMap`, including
/// the tokens listed in the `Connection:` header itself. A proxy MUST NOT forward
/// them: they belong to ONE connection (ours with the client / ours with the
/// backend), not to the message — letting them through corrupts framing (hyper
/// reframes the body over a client `Transfer-Encoding` → smuggling risk) and
/// leaks `Connection: close`/`Keep-Alive` to the other side.
fn strip_hop_by_hop(headers: &mut hyper::HeaderMap) {
    use hyper::header::{HeaderName, CONNECTION};
    // The names listed in the `Connection` header(s) are themselves hop-by-hop.
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
    headers.remove("proxy-connection"); // non-standard but common
    for h in listed {
        headers.remove(&h);
    }
}

/// Picks the best route for a `(host, path)`: first the routes with a specific
/// `host` (over the any-host ones), then the LONGEST path prefix (the most
/// specific wins). `None` = none matches.
fn pick_route<'a>(routes: &'a [Route], host: &str, path: &str) -> Option<&'a Route> {
    routes
        .iter()
        .filter(|r| (r.host.is_empty() || r.host == host) && path.starts_with(&r.path))
        .max_by_key(|r| (usize::from(!r.host.is_empty()), r.path.len()))
}

/// The request's `Host`, without the port (`loja.exemplo.ao:80` → `loja.exemplo.ao`).
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

/// A simple error response (404/502) with a text body.
fn text_response(code: StatusCode, msg: &str) -> Response<RespBody> {
    let body = Full::new(Bytes::from(msg.to_string()))
        .map_err(|e: Infallible| match e {})
        .boxed();
    Response::builder()
        .status(code)
        .body(body)
        .expect("resposta estática válida")
}

/// Handles a request: matches the route and forwards (streaming) to the backend, or
/// returns 404 (no route) / 502 (backend unreachable).
async fn handle(
    req: Request<Incoming>,
    routes: SharedRoutes,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
) -> std::result::Result<Response<RespBody>, Infallible> {
    let host = req_host(&req);
    let path = req.uri().path().to_string();
    // Snapshot of the routes (SIGHUP may swap them at any moment).
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

    // Rebuilds the request to the backend: same method/headers/body, absolute URI
    // `http://<backend><path?query>`. The body (`Incoming`) is forwarded without
    // buffering (streaming) — hyper transfers it as it arrives.
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
    // Removes the hop-by-hop headers BEFORE forwarding (the Host stays end-to-end —
    // the backend may need it for virtual-hosting).
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

    // Time ceiling: a hung backend must not hold the connection forever.
    match tokio::time::timeout(BACKEND_TIMEOUT, client.request(out_req)).await {
        Ok(Ok(resp)) => {
            // The backend's response flows back streaming; we also strip its
            // hop-by-hop headers before returning it to the client.
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

/// Serves ONE already-established connection (TCP or TLS): `io` is any IO that hyper
/// can read/write. Generic so as not to duplicate the TLS and plain paths.
async fn serve_io<I>(
    io: I,
    routes: SharedRoutes,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
) where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + 'static,
{
    let svc = service_fn(move |req| handle(req, routes.clone(), client.clone()));
    // NOTE: WebSocket/`Connection: Upgrade` is NOT tunneled yet — the legacy
    // hyper-util client does not establish the switched connection, and we remove the
    // `Upgrade` header (hop-by-hop) in forwarding. Tunneling upgrades (hyper::upgrade::on
    // on both sides + bidirectional copy) is a follow-up; today only HTTP
    // request/response is proxied.
    //
    // `header_read_timeout` cuts the classic slowloris (headers dripped out) — the
    // `timer` is mandatory for hyper to be able to apply it.
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .serve_connection(io, svc)
        .await;
}

/// Accepts connections on a port and serves each one. With `tls` present, does the
/// TLS handshake before serving (terminates TLS at the proxy); otherwise, plain HTTP.
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
                // A persistent error (EMFILE/ENFILE under fd exhaustion) on a
                // bare `continue` spins a busy-loop burning CPU — short pause.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                continue;
            }
        };
        let routes = routes.clone();
        let client = client.clone();
        let tls = tls.clone();
        tokio::task::spawn(async move {
            match tls {
                // Handshake timeout: a client that opens TCP and never completes the
                // ClientHello must not hold the task forever (TLS slowloris).
                Some(acceptor) => {
                    match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
                    {
                        Ok(Ok(tls_stream)) => {
                            serve_io(TokioIo::new(tls_stream), routes, client).await
                        }
                        Ok(Err(e)) => eprintln!("ingress-proxy: handshake TLS falhou: {e}"),
                        Err(_) => { /* handshake did not complete in time — discard */ }
                    }
                }
                None => serve_io(TokioIo::new(stream), routes, client).await,
            }
        });
    }
}

/// Builds rustls's `ServerConfig` from the PEM (cert-chain + key). The
/// cryptographic provider (`ring`) must be installed (see `run`).
///
/// **v1 limitation (SNI):** a SINGLE cert serves all hosts (`with_single_cert`).
/// For several hosts with distinct certs a `ResolvesServerCert` per SNI would be
/// needed — follow-up. Today the self-signed covers all the HTTPRoute's hosts in one
/// cert (multi-SAN), and the BYO mode assumes a cert that serves all the hosts.
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
        .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("mismatched TLS cert/key"))))?;
    Ok(Arc::new(cfg))
}

/// Async core: bind each listener + serve, with the route table hot-swappable via
/// `SIGHUP` (re-reads `config_path`). The listeners and TLS material stay FIXED at
/// startup (changing them requires a restart); only the ROUTES reload — which is
/// what container auto-registration needs.
async fn serve(cfg: ProxyConfig, config_path: std::path::PathBuf) -> Result<()> {
    let client: Client<_, Incoming> = Client::builder(TokioExecutor::new())
        .build(hyper_util::client::legacy::connect::HttpConnector::new());

    // A single TLS ServerConfig shared by all TLS listeners (if there is
    // material). Built ONCE — rustls keeps it in an Arc.
    let tls_acceptor: Option<tokio_rustls::TlsAcceptor> = match &cfg.tls {
        Some(mat) => Some(tokio_rustls::TlsAcceptor::from(build_server_config(mat)?)),
        None => None,
    };

    // Shared and hot-swappable route table.
    let routes: SharedRoutes = Arc::new(std::sync::RwLock::new(Arc::new(cfg.routes.clone())));

    // SIGHUP → re-reads the config and replaces ONLY the routes (listeners/TLS stay).
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
                            // Drop the write-lock BEFORE the eprintln (stderr I/O
                            // must not block the handlers that do `read()`).
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
                        eprintln!(
                            "ingress-proxy: {}",
                            super::po::t("SIGHUP but the config would not re-parse — routes kept")
                        )
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

/// Entry point of the `delonix ingress-proxy --config <file>` subcommand.
/// Reads the `ProxyConfig` (JSON) and runs the server until it dies (blocks).
pub fn run(config_path: &Path) -> Result<()> {
    let bytes = std::fs::read(config_path).map_err(|e| {
        Error::Invalid(format!(
            "ingress-proxy: não li a config {}: {e}",
            config_path.display()
        ))
    })?;
    let cfg: ProxyConfig = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Invalid(format!("ingress-proxy: config inválida: {e}")))?;
    // Installs rustls's cryptographic provider (ring) — the `ServerConfig::builder`
    // uses the process default; without this, it panics. Idempotent (ignores if
    // already installed by another part of the process).
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
// Lifecycle (host-side): start/reload/stop the proxy in the holder's netns.
// The proxy is persistent infra (like the slirp/holder), launched only when there
// is an HTTPRoute — it respects 'daemonless' (does not run without declared load).
// ============================================================================

/// The proxy's state folder (`<root>/httproute/`). On the same filesystem the
/// holder sees (the holder's mount-ns is a copy of the host's) — the proxy in
/// there reads the SAME config we write out here.
fn proxy_dir() -> std::path::PathBuf {
    crate::cmd::util::state_root().join("httproute")
}
/// Canonical path of the `ProxyConfig` (the proxy re-reads it on SIGHUP).
pub fn config_path() -> std::path::PathBuf {
    proxy_dir().join("config.json")
}
fn pid_path() -> std::path::PathBuf {
    proxy_dir().join("proxy.pid")
}
fn log_path() -> std::path::PathBuf {
    proxy_dir().join("proxy.log")
}
/// HTTP port of the auto-routes (`--expose`). **Non-privileged** — in rootless the
/// slirp refuses to publish ports <1024. Reached with `Host: <fqdn>` on `:8080`.
const AUTO_HTTP_PORT: u16 = 8080;

/// The MANUAL part of the config (routes/listeners/TLS from `kind: HTTPRoute`).
fn manual_path() -> std::path::PathBuf {
    proxy_dir().join("manual.json")
}
/// The AUTO-REGISTERED routes of containers (`container run --expose`).
fn auto_path() -> std::path::PathBuf {
    proxy_dir().join("auto.json")
}

/// An auto-registered route of an HTTP container: the internal FQDN
/// `<name>.<namespace>.delonix.internal` → `<ip>:<port>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoRoute {
    pub name: String,
    pub namespace: String,
    pub ip: String,
    pub port: u16,
}

impl AutoRoute {
    /// This container's internal FQDN (the `Host` that matches it in the proxy + the DNS name).
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

/// Read-modify-write of `auto.json` under an **exclusive flock** — two
/// `container run --expose` in parallel must not lose a route (lost update).
/// `f` receives the current list and returns the new one. Returns `true` if it changed.
fn with_auto_locked(f: impl FnOnce(&mut Vec<AutoRoute>)) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    std::fs::create_dir_all(proxy_dir()).map_err(|e| Error::Runtime {
        context: "httproute dir",
        message: e.to_string(),
    })?;
    // A dedicated lock file (the flock is on the fd; the content stays in auto.json).
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(proxy_dir().join("auto.lock"))
        .map_err(|e| Error::Runtime {
            context: "auto.lock",
            message: e.to_string(),
        })?;
    // SAFETY: flock(LOCK_EX) on the lock's fd; released on close (end of scope).
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

/// **Composes the final config** from the MANUAL part (HTTPRoute) + the
/// AUTO-REGISTERED routes, and ensures the proxy is serving (or stops it if it all
/// went empty). It is the single point that `httproute apply` and auto-registration
/// call — neither source erases the other.
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

    // The auto-routes are served over HTTP on the AUTO_HTTP_PORT port (internal
    // FQDN). NOT :80 — in rootless the slirp does not publish privileged ports
    // (add_hostfwd refuses <1024). Ensures the listener if there is any auto-route.
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
        // Nothing declared (neither manual nor auto) → the proxy has no reason to exist.
        return stop();
    }
    ensure_running(&ProxyConfig {
        listeners,
        routes,
        tls,
    })
}

/// Writes the MANUAL part (from `httproute apply`) and recomposes the final config.
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

/// Removes the MANUAL part (on `httproute rm`) and recomposes — the
/// auto-registered routes of `--expose` containers SURVIVE (the proxy only stops if
/// nothing else remains). Returns `true` if there were manual routes.
pub fn clear_manual() -> Result<bool> {
    let had = manual_path().exists();
    let _ = std::fs::remove_file(manual_path());
    rebuild()?;
    Ok(had)
}

/// **Auto-registers** an HTTP container in the proxy (`container run --expose`):
/// adds/updates its `AutoRoute` and recomposes the config (hot SIGHUP). Idempotent.
pub fn auto_register(name: &str, namespace: &str, ip: &str, port: u16) -> Result<()> {
    let entry = AutoRoute {
        name: name.to_string(),
        namespace: namespace.to_string(),
        ip: ip.to_string(),
        port,
    };
    with_auto_locked(|auto| {
        auto.retain(|a| a.name != name); // replaces a previous entry of the same name
        auto.push(entry.clone());
    })?;
    rebuild()
}

/// **Removes** a container's auto-registration (on `container rm`/stop) and recomposes.
/// Best-effort — if the container was not registered, does nothing.
pub fn auto_deregister(name: &str) {
    // `Ok(true)` = removed something → recompose; `Ok(false)`/`Err` = was not
    // registered (or the lock failed) — nothing to recompose.
    if let Ok(true) = with_auto_locked(|auto| auto.retain(|a| a.name != name)) {
        let _ = rebuild();
    }
}

/// The proxy's PID if it is ALIVE **and really ours** (the `/proc/<pid>/cmdline`
/// contains `ingress-proxy`), else `None` (and cleans up an orphan pidfile). The
/// identity guard is essential: without it, a PID recycled by the kernel would make
/// `SIGHUP`/`SIGTERM` hit an unrelated process (SIGHUP default = terminate).
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
        let _ = std::fs::remove_file(pid_path()); // dead OR recycled PID — orphan
        None
    }
}

/// Writes the config and **ensures the proxy is serving**: if already alive, reloads
/// hot (SIGHUP); otherwise, starts it in the holder's netns and publishes the ports.
/// Idempotent — it is what `stack apply`/auto-registration always call.
pub fn ensure_running(cfg: &ProxyConfig) -> Result<()> {
    std::fs::create_dir_all(proxy_dir()).map_err(|e| Error::Runtime {
        context: "httproute dir",
        message: e.to_string(),
    })?;
    // Captures the CURRENT listeners BEFORE overwriting the config (else `prev`
    // would already be the new one).
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
        // Alive → reload the routes hot (SIGHUP). The ports are already published.
        // WARNING: SIGHUP only reloads ROUTES; changing entrypoints/TLS requires a
        // restart (`httproute rm` + apply). We detect the change of the port set so
        // as not to lie that the new listener is serving.
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
        // SAFETY: SIGHUP to a pid we confirmed alive AND ours (cmdline guard).
        unsafe { libc::kill(pid, libc::SIGHUP) };
        eprintln!("httproute: proxy #{pid} recarregado (SIGHUP)");
        return Ok(());
    }
    spawn_proxy()?;
    publish_listeners(cfg)?;
    Ok(())
}

/// The listener ports of the config CURRENTLY in effect (before we overwrite it) —
/// to detect a change of listeners on re-apply.
fn prev_listener_ports() -> Option<std::collections::BTreeSet<u16>> {
    let bytes = std::fs::read(config_path()).ok()?;
    let cfg: ProxyConfig = serde_json::from_slice(&bytes).ok()?;
    Some(cfg.listeners.iter().map(|l| l.port).collect())
}

/// Starts the proxy INSIDE the holder's netns (via `infra_join_argv`), detached
/// (setsid, stdio to a log), and writes the pidfile.
fn spawn_proxy() -> Result<()> {
    use std::os::unix::process::CommandExt;
    // Ensures the holder is up (the proxy lives in its netns).
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
    // SAFETY: setsid in the child (post-fork, pre-exec) detaches it from the CLI's
    // session/terminal so it survives this process's exit. nsenter does EXEC (not
    // fork) of the proxy, so this PID becomes the proxy's — signalable from the host.
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
    // Confirm it really started (did not die right at bind): give it a moment and
    // check /proc. If it fell, point to the log — do not declare 'serving' and lie.
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

/// Publishes the inbound ports on the host (slirp `add_hostfwd`): the proxy listens
/// on `0.0.0.0:<port>` in the holder's netns and catches the traffic delivered to
/// `SLIRP_IP`. (No DNAT — the holder has no `input` chain filtering local deliveries.)
fn publish_listeners(cfg: &ProxyConfig) -> Result<()> {
    let sock = delonix_net::infra::slirp_sock_path();
    for l in &cfg.listeners {
        let p = l.port.to_string();
        // Best-effort/idempotent: if the port ALREADY has a hostfwd (a previous proxy
        // that crashed without teardown), the slirp refuses with 'already exists' — not
        // fatal, the desired state (port published) is already there. Only warns on other errors.
        if let Err(e) = delonix_net::slirp_add_hostfwd(&sock, &p, &p, "tcp") {
            let msg = e.to_string();
            if msg.contains("already") || msg.to_lowercase().contains("exist") {
                eprintln!(
                    "httproute: {}",
                    super::po::tf(
                        "port :{p} already published — kept",
                        &[("p", &p.to_string())]
                    )
                );
            } else {
                eprintln!(
                    "httproute: {}",
                    super::po::tf(
                        "warning while publishing :{p}: {err}",
                        &[("p", &p.to_string()), ("err", &e.to_string())]
                    )
                );
            }
        }
    }
    Ok(())
}

/// **Stops the proxy and unpublishes the ports** (teardown of `httproute rm`). Reads
/// the ports from the config before deleting it. Best-effort/idempotent.
pub fn stop() -> Result<()> {
    // Unpublishes the known ports (from the config, if it still exists).
    if let Ok(bytes) = std::fs::read(config_path()) {
        if let Ok(cfg) = serde_json::from_slice::<ProxyConfig>(&bytes) {
            let sock = delonix_net::infra::slirp_sock_path();
            for l in &cfg.listeners {
                let _ = delonix_net::infra::slirp_remove_hostfwd(&sock, &l.port.to_string());
            }
        }
    }
    if let Some(pid) = running_pid() {
        // SAFETY: SIGTERM to a pid confirmed alive and ours.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let _ = std::fs::remove_file(pid_path());
    let _ = std::fs::remove_file(config_path());
    // Full teardown: also the sources (manual + auto), else a subsequent start would
    // raise phantom routes again.
    let _ = std::fs::remove_file(manual_path());
    let _ = std::fs::remove_file(auto_path());
    Ok(())
}

/// Is the proxy running? (for `httproute ls`/describe).
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
            r("", "/", "10.0.0.1:80"),           // any host, root
            r("loja.ex", "/", "10.0.0.2:80"),    // specific host, root
            r("loja.ex", "/api", "10.0.0.3:80"), // specific host, /api (longer)
        ];
        // /api on loja → the /api route (longest prefix)
        assert_eq!(
            pick_route(&routes, "loja.ex", "/api/x").unwrap().backend,
            "10.0.0.3:80"
        );
        // / on loja → the host-specific route, not the any-host one
        assert_eq!(
            pick_route(&routes, "loja.ex", "/home").unwrap().backend,
            "10.0.0.2:80"
        );
        // another host → only the any-host one matches
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
        // And rustls can build a ServerConfig from it.
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
