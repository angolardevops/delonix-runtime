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

use delonix_model::{Error, Result};

/// WHERE a proxy instance lives. Two can run at once, because a route's backend is
/// only reachable from one netns: containers and Cloud Hypervisor VMs are on the SDN
/// (the holder netns), a libvirt VM sits on `virbr0` in the host netns, which the
/// holder does not see (ADR-0046, D2). Same binary, same config format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Where {
    Holder,
    Host,
}

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
    /// Host names to publish in the operator host's `/etc/hosts` (`hosts: [host]`,
    /// ADR-0046). The proxy itself never reads this — it rides in the manual config
    /// so `rebuild` can sync the file from one place, and so the reconciler can tell
    /// which document asked for which name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub published_hosts: Vec<PublishedHost>,
    /// Addresses documents hold from an `IPPool` (`spec.pool`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claims: Vec<PoolClaim>,
    /// Ownership records, one per document (see [`Stamp`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stamps: Vec<Stamp>,
    /// Address the listeners bind to. `None` = every address, which is what the
    /// holder instance wants (the slirp forward decides who reaches it). The host
    /// instance sets `127.0.0.1`: it is a real socket on the host, and exposing a
    /// route to the LAN must be a decision, not a side effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
}

/// Who owns one document's routes: the stack that applied it, and what it applied.
///
/// The proxy config is COLLECTIVE, so a route has nowhere else to carry the
/// `delonix.io/stack` label and last-applied record every other Kind keeps on its own
/// resource. Without them the Kind cannot be owned, and a resource that cannot be owned
/// is invisible to `--prune` and `destroy`: the proxy, the hosts block and the leases of
/// a destroyed stack stayed behind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stamp {
    pub source: String,
    /// `HTTPRoute` or `Ingress`, as written — the plan names the Kind the user wrote.
    pub kind: String,
    pub stack: String,
    pub last_applied: String,
}

/// One host name a route publishes to the SDN, with the document that asked for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedHost {
    pub host: String,
    pub source: String,
    /// Address the name points at: the route's reserved address, or loopback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
}

/// An `IPPool` address a document holds, recorded so the reconciler can compare the
/// pool it DECLARES with the one it actually got.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolClaim {
    pub source: String,
    pub pool: String,
    pub addr: String,
}

/// Cert + key in PEM, ready to load into rustls (Phase 4 resolves them).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsMaterial {
    #[serde(rename = "certPem")]
    pub cert_pem: String,
    #[serde(rename = "keyPem")]
    pub key_pem: String,
    /// Which `tls.mode` produced this material (`selfSigned` / `secretRef`).
    ///
    /// **Recorded because a cert does not say where it came from**, and without
    /// it the reconciler's `actual` had nothing to report but «TLS is on» — so
    /// it copied the DESIRED mode instead, and switching `selfSigned` →
    /// `secretRef` on a route that already had TLS was invisible to the plan.
    /// A field that reports the desired value is not an observation.
    ///
    /// `#[serde(default)]` because configs written before this exist on disk;
    /// they read back as an empty mode, which compares as «unknown» and settles
    /// on the next apply.
    #[serde(default)]
    pub mode: String,
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
        mode: "selfSigned".into(),
    })
}

/// A listening port of the proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listener {
    pub port: u16,
    #[serde(default)]
    pub tls: bool,
    /// Host address this listener is reachable on (a reserved `IPPool` address). `None`
    /// = the default: loopback through the slirp for the holder instance, `bind` for the
    /// host one. Inside the holder netns the proxy still binds every address; the
    /// address only decides where the slirp forward is published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// The documents that asked for this port. Listeners are the UNION of every
    /// document's, so without this a document could neither be told apart from its
    /// neighbours (the drift check) nor removed on its own (`--prune`/`destroy`).
    /// Empty in a config written before it existed, which is read as «unknown».
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
}

/// A resolved route: matches by `host` (empty = any) + `path` prefix, and
/// forwards to `backend` (`ip:port`, already resolved from the container record).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    #[serde(default)]
    pub host: String,
    pub path: String,
    pub backend: String,
    /// Which `kind: HTTPRoute` document produced this route.
    ///
    /// **Provenance is what makes the Kind convergeable at all.** Every
    /// HTTPRoute document is merged by `resolve_config` into ONE proxy config,
    /// and without this field nothing recorded which document contributed which
    /// route — so a plan had nothing to compare a single document against, and
    /// `stack plan --fields` said exactly that as the Kind's reason for not
    /// converging.
    ///
    /// `#[serde(default)]` so a `manual.json` written before this keeps loading;
    /// an empty source means "unknown", which the reconciler reads as "not
    /// mine", never as "everyone's".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
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
        .filter(|r| (r.host.is_empty() || r.host == host) && path_prefix_matches(path, &r.path))
        .max_by_key(|r| (usize::from(!r.host.is_empty()), r.path.len()))
}

/// Kubernetes Gateway API `PathPrefix` semantics: `/foo` matches `/foo` and
/// `/foo/bar`, but NOT `/foobar`.
///
/// BUG FOUND: this used to be a raw `path.starts_with(prefix)` with no
/// segment-boundary check — `valid_path_prefix`/AGENTS.md explicitly align
/// `kind: HTTPRoute` to the k8s Gateway API's PathPrefix match, but the
/// runtime matcher didn't actually enforce it. Two routes `/api` (internal
/// backend) and `/` (public backend) on the same host: a request for
/// `/api-docs` (meant for the public backend) matched `/api` too, and — via
/// the longest-prefix tie-break — got silently routed to the more specific,
/// wrong (potentially internal) backend.
fn path_prefix_matches(path: &str, prefix: &str) -> bool {
    if !path.starts_with(prefix) {
        return false;
    }
    prefix.ends_with('/') || path.len() == prefix.len() || path.as_bytes()[prefix.len()] == b'/'
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
        .expect("static response is valid")
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
            &format!("{}\n", super::po::t("delonix: no route for this host/path")),
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
                &format!("{}\n", super::po::t("delonix: invalid request")),
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
            &format!(
                "{}\n",
                super::po::tf(
                    "delonix: backend {backend} unreachable: {err}",
                    &[("backend", &backend.to_string()), ("err", &e.to_string())],
                )
            ),
        )),
        Err(_elapsed) => Ok(text_response(
            StatusCode::GATEWAY_TIMEOUT,
            &format!(
                "{}\n",
                super::po::tf(
                    "delonix: backend {backend} did not respond in {secs}s",
                    &[
                        ("backend", &backend.to_string()),
                        ("secs", &BACKEND_TIMEOUT.as_secs().to_string()),
                    ],
                )
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
                        Ok(Err(e)) => eprintln!(
                            "{}",
                            super::po::tf(
                                "ingress-proxy: TLS handshake failed: {e}",
                                &[("e", &e.to_string())]
                            )
                        ),
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
        .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("invalid TLS cert (PEM)"))))?;
    if certs.is_empty() {
        return Err(Error::Invalid(
            super::po::t("empty TLS cert (no CERTIFICATE in the PEM)").into(),
        ));
    }
    let key = rustls_pemfile::private_key(&mut tls.key_pem.as_bytes())
        .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("invalid TLS key (PEM)"))))?
        .ok_or_else(|| {
            Error::Invalid(super::po::t("missing TLS key (no PRIVATE KEY in the PEM)").into())
        })?;
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
                                    "{}",
                                    super::po::t(
                                        "ingress-proxy: poisoned routes lock — reload ignored"
                                    )
                                );
                                continue;
                            }
                        }
                        eprintln!(
                            "{}",
                            super::po::tf(
                                "ingress-proxy: routes reloaded ({n} route(s))",
                                &[("n", &n.to_string())],
                            )
                        );
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
                    return Err(Error::Invalid(super::po::tf(
                        "listener :{port} requests TLS but the config has no TLS material (cert/key)",
                        &[("port", &l.port.to_string())],
                    )));
                }
            }
        } else {
            None
        };
        // The host instance binds the listener's reserved address (or `bind`); the
        // holder instance binds everything inside its own netns, where a host address
        // does not exist.
        let ip: std::net::IpAddr = match &cfg.bind {
            // An address that does not parse is a config error, not «loopback»: falling
            // back silently would bind somewhere the operator never asked for and
            // report the route as served.
            Some(default) => {
                let raw = l.addr.as_deref().unwrap_or(default);
                raw.parse().map_err(|_| {
                    Error::Invalid(super::po::tf(
                        "listener :{port}: '{addr}' is not an IP address",
                        &[("port", &l.port.to_string()), ("addr", raw)],
                    ))
                })?
            }
            None => std::net::IpAddr::from([0, 0, 0, 0]),
        };
        let addr = SocketAddr::new(ip, l.port);
        let listener = TcpListener::bind(addr).await.map_err(|e| Error::Runtime {
            context: "ingress-proxy bind",
            message: format!("{addr}: {e}"),
        })?;
        eprintln!(
            "{}",
            super::po::tf(
                "ingress-proxy: listening on {addr} ({scheme}, {n} route(s))",
                &[
                    ("addr", &addr.to_string()),
                    ("scheme", if l.tls { "https" } else { "http" }),
                    ("n", &cfg.routes.len().to_string()),
                ],
            )
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
            super::po::t("ingress-proxy: no HTTP listener to serve").into(),
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
            "{}: {e}",
            super::po::tf(
                "ingress-proxy: could not read the config {path}",
                &[("path", &config_path.display().to_string())],
            )
        ))
    })?;
    let cfg: ProxyConfig = serde_json::from_slice(&bytes).map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::t("ingress-proxy: invalid config")
        ))
    })?;
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
fn proxy_dir(w: Where) -> std::path::PathBuf {
    crate::cmd::util::state_root().join(match w {
        Where::Holder => "httproute",
        Where::Host => "httproute-host",
    })
}
/// Canonical path of the `ProxyConfig` (the proxy re-reads it on SIGHUP).
pub fn config_path(w: Where) -> std::path::PathBuf {
    proxy_dir(w).join("config.json")
}
fn pid_path(w: Where) -> std::path::PathBuf {
    proxy_dir(w).join("proxy.pid")
}
fn log_path(w: Where) -> std::path::PathBuf {
    proxy_dir(w).join("proxy.log")
}
/// HTTP port of the auto-routes (`--expose`). **Non-privileged** — in rootless the
/// slirp refuses to publish ports <1024. Reached with `Host: <fqdn>` on `:8080`.
const AUTO_HTTP_PORT: u16 = 8080;

/// The MANUAL part of the config (routes/listeners/TLS from `kind: HTTPRoute`).
fn manual_path(w: Where) -> std::path::PathBuf {
    proxy_dir(w).join("manual.json")
}
/// The AUTO-REGISTERED routes of containers (`container run --expose`).
fn auto_path() -> std::path::PathBuf {
    proxy_dir(Where::Holder).join("auto.json")
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

pub(crate) fn read_manual_config(w: Where) -> Option<ProxyConfig> {
    read_manual(w)
}

/// The config the RUNNING proxy is serving, or `None` if none is running.
///
/// Distinct from [`read_manual_config`] on purpose: that one is the manual
/// SOURCE (what `kind: HTTPRoute` asked for), this is the composed result the
/// proxy actually bound its sockets from. Only the second answers «do the live
/// listeners match what is being asked for», which is what decides whether a
/// converge can SIGHUP or has to restart.
///
/// Gated on the proxy being alive: a leftover `config.json` from a proxy that
/// died would otherwise read as live listeners, and a converge would restart a
/// proxy that does not exist to serve a port nobody is listening on.
pub(crate) fn live_config(w: Where) -> Option<ProxyConfig> {
    running_pid(w)?;
    serde_json::from_slice(&std::fs::read(config_path(w)).ok()?).ok()
}

/// Do the listeners in `new` differ from the ones the running proxy was started with —
/// by port, TLS or address? Listeners are bound once, at startup, so any of the three
/// needs a restart; comparing only ports let a moved address go unnoticed.
pub(crate) fn listeners_changed(w: Where, new: &[Listener]) -> bool {
    if running_pid(w).is_none() {
        return false;
    }
    let sig = |v: &[Listener]| -> Vec<(u16, bool, Option<String>)> {
        let mut x: Vec<_> = v.iter().map(|l| (l.port, l.tls, l.addr.clone())).collect();
        x.sort();
        x
    };
    match read_manual(w) {
        Some(old) => sig(&old.listeners) != sig(new),
        None => false,
    }
}

fn read_manual(w: Where) -> Option<ProxyConfig> {
    serde_json::from_slice(&std::fs::read(manual_path(w)).ok()?).ok()
}
fn read_auto() -> Vec<AutoRoute> {
    std::fs::read(auto_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Read-modify-write of `auto.json` under an **exclusive flock**, followed
/// by `rebuild()` (compose + write `config.json` + SIGHUP) STILL under that
/// SAME lock — two `container run --expose` in parallel must not lose a
/// route (lost update), NOR must the live proxy config end up reflecting a
/// stale snapshot. `f` receives the current list and returns the new one.
/// Returns `true` if it changed.
///
/// BUG FOUND: `rebuild()` used to be called by the CALLER, AFTER this
/// function had already released the lock. Two concurrent `--expose`
/// registrations could interleave so the LAST writer of `config.json` (the
/// file the proxy actually serves, reloaded via SIGHUP) reflected an
/// EARLIER, incomplete snapshot of `auto.json` — a route that was
/// genuinely added successfully to `auto.json` silently never made it into
/// the live proxy, with no error and no further trigger to recompose.
/// Folding the whole compose+write+reload into this same critical section
/// makes the last writer to finish always see (and publish) the final
/// `auto.json`.
fn with_auto_locked(f: impl FnOnce(&mut Vec<AutoRoute>)) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    let w = Where::Holder;
    std::fs::create_dir_all(proxy_dir(w)).map_err(|e| Error::Runtime {
        context: "httproute dir",
        message: e.to_string(),
    })?;
    // A dedicated lock file (the flock is on the fd; the content stays in auto.json).
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(proxy_dir(w).join("auto.lock"))
        .map_err(|e| Error::Runtime {
            context: "auto.lock",
            message: e.to_string(),
        })?;
    // SAFETY: flock(LOCK_EX) on the lock's fd; released on close (end of scope).
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(Error::Runtime {
            context: "flock auto",
            message: super::po::t("could not acquire the lock").into(),
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
        context: "write auto",
        message: e.to_string(),
    })?;
    // Still holding `lock` here — the recompose+publish happens before it's
    // dropped at the end of this scope.
    rebuild(Where::Holder)?;
    Ok(true)
}

/// **Composes the final config** from the MANUAL part (HTTPRoute) + the
/// AUTO-REGISTERED routes, and ensures the proxy is serving (or stops it if it all
/// went empty). It is the single point that `httproute apply` and auto-registration
/// call — neither source erases the other.
fn rebuild(w: Where) -> Result<()> {
    let manual = read_manual(w);
    // Only the holder instance has auto-registered routes (`container run --expose`
    // targets containers on the SDN).
    let auto = if w == Where::Holder {
        read_auto()
    } else {
        Vec::new()
    };

    // The names a document opted into publish in the host's `/etc/hosts`. Synced
    // here — the single point every source of routes goes through — and BEFORE the
    // "nothing left, stop" exit below, so removing the last route also removes its
    // names. Adding a name is loud when it cannot be written (a name that silently
    // does not resolve is the manual step this feature exists to remove); REMOVING
    // one only warns, because refusing a `rm` for lack of root would leave the route
    // in place.
    // Both instances contribute to the ONE block of the host's `/etc/hosts`, so the
    // names are read from both sources, never just the one being rebuilt.
    let published: Vec<(String, String)> = [Where::Holder, Where::Host]
        .iter()
        .filter_map(|x| {
            if *x == w {
                manual.clone()
            } else {
                read_manual(*x)
            }
        })
        .flat_map(|m| {
            m.published_hosts
                .into_iter()
                .map(|d| (d.host, d.addr.unwrap_or_else(|| "127.0.0.1".to_string())))
        })
        .collect();
    if let Err(e) = super::hosts_file::sync(&published) {
        if published.is_empty() {
            eprintln!("warning: {e}");
        } else {
            return Err(e);
        }
    }

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
            addr: None,
            sources: Vec::new(),
        });
    }
    for a in &auto {
        routes.push(Route {
            host: a.fqdn(),
            path: "/".into(),
            backend: format!("{}:{}", a.ip, a.port),
            // An auto-registered route comes from a `container run --expose`,
            // not from any document — left empty so the reconciler never
            // mistakes it for a manifest's, which is what would let a `plan`
            // propose removing a route nobody declared.
            source: String::new(),
        });
    }

    if listeners.is_empty() || routes.is_empty() {
        // Nothing declared (neither manual nor auto) → the proxy has no reason to exist.
        return stop(w);
    }
    ensure_running(
        &ProxyConfig {
            listeners,
            routes,
            tls,
            published_hosts: Vec::new(),
            claims: manual
                .as_ref()
                .map(|m| m.claims.clone())
                .unwrap_or_default(),
            stamps: Vec::new(),
            // The host instance is a real socket on the host: with no `bind` recorded it
            // would listen on every address, so the default is loopback, never «all».
            bind: manual
                .as_ref()
                .and_then(|m| m.bind.clone())
                .or_else(|| (w == Where::Host).then(|| "127.0.0.1".to_string())),
        },
        w,
    )
}

/// Writes the MANUAL part (from `httproute apply`) and recomposes the final config.
pub fn set_manual(cfg: &ProxyConfig, w: Where) -> Result<()> {
    // The ownership records belong to the DOCUMENTS, not to one composition of them:
    // the config is rebuilt from the manifest on every apply, and a stamp written by the
    // last one must survive it for every document that is still here.
    let mut cfg = cfg.clone();
    if let Some(old) = read_manual(w) {
        for st in old.stamps {
            let still_here = cfg.routes.iter().any(|r| r.source == st.source);
            if still_here && !cfg.stamps.iter().any(|x| x.source == st.source) {
                cfg.stamps.push(st);
            }
        }
    }
    write_manual(w, &cfg)?;
    rebuild(w)
}

fn write_manual(w: Where, cfg: &ProxyConfig) -> Result<()> {
    std::fs::create_dir_all(proxy_dir(w)).map_err(|e| Error::Runtime {
        context: "httproute dir",
        message: e.to_string(),
    })?;
    let json = serde_json::to_vec_pretty(cfg).map_err(|e| Error::Runtime {
        context: "serialize manual",
        message: e.to_string(),
    })?;
    // Atomic: a reader (the other instance's `rebuild`, a plan) must never see half a
    // file and conclude the routes are gone.
    delonix_state::write_atomic(&manual_path(w), &json).map_err(|e| Error::Runtime {
        context: "write manual",
        message: e.to_string(),
    })
}

/// Edits the MANUAL part in place WITHOUT recomposing or signalling the proxy — for
/// records that do not change what is served (the ownership stamp). `f` returns whether
/// it changed anything; nothing is written otherwise. `false` when there is no manual
/// part to edit.
pub(crate) fn update_manual(w: Where, f: impl FnOnce(&mut ProxyConfig) -> bool) -> Result<bool> {
    let Some(mut cfg) = read_manual(w) else {
        return Ok(false);
    };
    if !f(&mut cfg) {
        return Ok(false);
    }
    write_manual(w, &cfg)?;
    Ok(true)
}

/// Removes the MANUAL part (on `httproute rm`) and recomposes — the
/// auto-registered routes of `--expose` containers SURVIVE (the proxy only stops if
/// nothing else remains). Returns `true` if there were manual routes.
pub fn clear_manual(w: Where) -> Result<bool> {
    let had = manual_path(w).exists();
    let _ = std::fs::remove_file(manual_path(w));
    rebuild(w)?;
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
    // `with_auto_locked` already recomposes+publishes the config itself,
    // still under the same flock as the `auto.json` write — see its doc
    // comment for the race this closes.
    with_auto_locked(|auto| {
        auto.retain(|a| a.name != name); // replaces a previous entry of the same name
        auto.push(entry.clone());
    })?;
    Ok(())
}

/// **Removes** a container's auto-registration (on `container rm`/stop) and recomposes.
/// Best-effort — if the container was not registered, does nothing.
pub fn auto_deregister(name: &str) {
    let _ = with_auto_locked(|auto| auto.retain(|a| a.name != name));
}

/// Whether a live process's `/proc/<pid>/cmdline` is an `ingress-proxy` started
/// by **THIS state root**. PURE, so the recycled-pid cases are testable without
/// spawning a proxy.
///
/// **The root half is ACH-017, and it was measured.** The old test asked only
/// whether the blob contained `ingress-proxy`, and every `DELONIX_ROOT` on this
/// uid runs a proxy whose argv says exactly that — so a pid recycled onto
/// ANOTHER root's proxy passed as ours, and the pid is what `SIGHUP`/`SIGTERM`
/// go to. Reproduced 2026-09-09 with two isolated roots: `net httproute apply`
/// from root A returned `rc=0` printing "proxy #2470290 reloaded (SIGHUP)" —
/// B's proxy — while A's own port answered nothing, and `net httproute rm` from
/// A then killed B's proxy and took B's `:18081` down with it.
///
/// **The token is the `--config` path, not the environment.** `spawn_proxy`
/// does not pin `DELONIX_ROOT` on the child (it inherits the caller's
/// environment, and the machine's default root exports nothing), so the environ
/// proof that identifies the netns pin is unavailable here — the same reason it
/// is unavailable for `slirp4netns`, and the same answer: the ownership token is
/// the path WE choose in the argv. `config_path(Where::Holder)` is `state_root()/httproute/
/// config.json`, it is per-root, and `spawn_proxy` has passed it since the
/// commit that first spawned a proxy at all (478e09aa) — so there is no
/// in-place-upgrade trap of a proxy this cannot name.
///
/// `ingress-proxy` also stops being a substring of the whole blob and becomes an
/// argv element of its own: a path or an image name that merely spells it must
/// not let a process pose as the proxy. Same rule, and same reason, as the
/// `argv[0]`-only test for `slirp4netns` in `delonix_sdn::infra`.
fn proxy_argv_is_ours(cmdline: &[u8], cfg: &std::path::Path) -> bool {
    let argv: Vec<String> = cmdline
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if !argv.iter().any(|a| a == "ingress-proxy") {
        return false;
    }
    // We emit the separated form; the `--config=<path>` spelling is equally
    // valid and costs one line to accept.
    argv.windows(2)
        .any(|w| w[0] == "--config" && std::path::Path::new(&w[1]) == cfg)
        || argv
            .iter()
            .filter_map(|a| a.strip_prefix("--config="))
            .any(|v| std::path::Path::new(v) == cfg)
}

/// The proxy's PID if it is ALIVE **and really ours** — an `ingress-proxy`
/// serving THIS root's config ([`proxy_argv_is_ours`]) — else `None` (and cleans
/// up an orphan pidfile).
///
/// The identity guard is essential: without it, a PID recycled by the kernel
/// would make `SIGHUP`/`SIGTERM` hit an unrelated process (SIGHUP default =
/// terminate). Its first version asked only whether the process was *a* proxy,
/// which is a different question from whether it is *ours* — see
/// [`proxy_argv_is_ours`] for what that cost, measured.
fn running_pid(w: Where) -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(pid_path(w))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let is_ours = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| proxy_argv_is_ours(&c, &config_path(w)))
        .unwrap_or(false);
    if is_ours {
        Some(pid)
    } else {
        // Dead, recycled by something else, or ANOTHER root's proxy: in all
        // three the file is a lie about this root's proxy, and removing it is
        // the correct cleanup. Signalling the pid would not be.
        let _ = std::fs::remove_file(pid_path(w));
        None
    }
}

/// Writes the config and **ensures the proxy is serving**: if already alive, reloads
/// hot (SIGHUP); otherwise, starts it in the holder's netns and publishes the ports.
/// Idempotent — it is what `stack apply`/auto-registration always call.
pub fn ensure_running(cfg: &ProxyConfig, w: Where) -> Result<()> {
    std::fs::create_dir_all(proxy_dir(w)).map_err(|e| Error::Runtime {
        context: "httproute dir",
        message: e.to_string(),
    })?;
    // Captures the CURRENT listeners BEFORE overwriting the config (else `prev`
    // would already be the new one).
    let prev_ports = prev_listener_ports(w);
    let json = serde_json::to_vec_pretty(cfg).map_err(|e| Error::Runtime {
        context: "serialize config",
        message: e.to_string(),
    })?;
    std::fs::write(config_path(w), &json).map_err(|e| Error::Runtime {
        context: "write config",
        message: e.to_string(),
    })?;

    // BUG FOUND: the running_pid(w)-check → spawn_proxy(w) decision used to
    // have NO lock at all. When no proxy exists yet, two concurrent callers
    // (e.g. an `httproute apply` racing a `container run --expose`) both
    // observe `running_pid(w) == None` and both spawn a proxy; both try to
    // bind the same listener port(s) in the holder netns — one wins, the
    // other crashes at bind, and whichever spawn writes the pidfile LAST
    // wins that race independently of which process actually stayed alive.
    // If the crashed one wrote last, `running_pid(w)` later finds it dead
    // and cleans the pidfile while the SURVIVING proxy is left with no
    // pidfile recorded — an orphan that SIGHUP/SIGTERM can no longer reach.
    // Serializing the whole check-then-spawn decision under a dedicated
    // lock (separate from `auto.lock`, which only guards `auto.json`)
    // closes the race: only one caller ever gets to spawn.
    let _spawn_lock = FileLock::acquire(&proxy_dir(w).join("spawn.lock"));
    if let Some(pid) = running_pid(w) {
        // Alive → reload the routes hot (SIGHUP). The ports are already published.
        // WARNING: SIGHUP only reloads ROUTES; changing entrypoints/TLS requires a
        // restart (`httproute rm` + apply). We detect the change of the port set so
        // as not to lie that the new listener is serving.
        if let Some(prev) = prev_ports {
            let now: std::collections::BTreeSet<u16> =
                cfg.listeners.iter().map(|l| l.port).collect();
            if prev != now {
                eprintln!(
                    "{}",
                    super::po::tf(
                        "httproute: WARNING — listener change ({prev} → {now}) has NO hot effect; SIGHUP only reloads routes. Run `httproute rm` + apply to rebind the ports.",
                        &[("prev", &format!("{prev:?}")), ("now", &format!("{now:?}"))],
                    )
                );
            }
        }
        // SAFETY: SIGHUP to a pid confirmed alive AND confirmed to be THIS
        // root's proxy (`proxy_argv_is_ours`).
        unsafe { libc::kill(pid, libc::SIGHUP) };
        eprintln!(
            "{}",
            super::po::tf(
                "httproute: proxy #{pid} reloaded (SIGHUP)",
                &[("pid", &pid.to_string())],
            )
        );
        return Ok(());
    }
    spawn_proxy(w)?;
    if w == Where::Holder {
        publish_listeners(cfg)?;
    }
    Ok(())
}

/// Exclusive file lock (`flock`) — same minimal idiom `with_auto_locked`
/// already uses inline, factored out here so `ensure_running` can guard a
/// DIFFERENT critical section (the spawn decision) under its own lock file.
struct FileLock(std::fs::File);

impl FileLock {
    fn acquire(path: &std::path::Path) -> Option<FileLock> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .ok()?;
        // SAFETY: valid, open fd; LOCK_EX blocks until the lock is ours.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return None;
        }
        Some(FileLock(f))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: fd still open (we own the File until here).
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// The listener ports of the config CURRENTLY in effect (before we overwrite it) —
/// to detect a change of listeners on re-apply.
fn prev_listener_ports(w: Where) -> Option<std::collections::BTreeSet<u16>> {
    let bytes = std::fs::read(config_path(w)).ok()?;
    let cfg: ProxyConfig = serde_json::from_slice(&bytes).ok()?;
    Some(cfg.listeners.iter().map(|l| l.port).collect())
}

/// Starts the proxy INSIDE the holder's netns (via `infra_join_argv`), detached
/// (setsid, stdio to a log), and writes the pidfile.
fn spawn_proxy(w: Where) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // The holder instance lives in the holder's netns and needs it up; the host
    // instance is an ordinary process in the host netns and needs nothing.
    let join: Vec<String> = match w {
        Where::Holder => {
            delonix_sdn::infra::ensure_up()?;
            delonix_sdn::infra::infra_join_argv().ok_or_else(|| Error::Runtime {
                context: "holder",
                message: super::po::t("ingress holder is down").into(),
            })?
        }
        Where::Host => Vec::new(),
    };
    let self_exe = std::env::current_exe().map_err(|e| Error::Runtime {
        context: "current_exe",
        message: e.to_string(),
    })?;
    let cfg_path = config_path(w);

    // argv = nsenter … -- <delonix> ingress-proxy --config <config>
    let mut argv: Vec<String> = join;
    argv.push(self_exe.to_string_lossy().into_owned());
    argv.push("ingress-proxy".into());
    argv.push("--config".into());
    argv.push(cfg_path.to_string_lossy().into_owned());

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(w))
        .map_err(|e| Error::Runtime {
            context: "open proxy log",
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
    std::fs::write(pid_path(w), child.id().to_string()).map_err(|e| Error::Runtime {
        context: "write pidfile",
        message: e.to_string(),
    })?;
    // Confirm it really started (did not die right at bind): give it a moment and
    // check /proc. If it fell, point to the log — do not declare 'serving' and lie.
    std::thread::sleep(std::time::Duration::from_millis(300));
    if !std::path::Path::new(&format!("/proc/{}", child.id())).exists() {
        return Err(Error::Runtime {
            context: "ingress-proxy",
            message: super::po::tf(
                "the proxy crashed right at startup (port taken?) — see {log}",
                &[("log", &log_path(w).display().to_string())],
            ),
        });
    }
    eprintln!(
        "{}",
        super::po::tf(
            "httproute: proxy started (#{pid}) in the {where} netns",
            &[
                ("pid", &child.id().to_string()),
                (
                    "where",
                    if w == Where::Holder {
                        "holder's"
                    } else {
                        "host"
                    }
                ),
            ],
        )
    );
    Ok(())
}

/// Publishes the inbound ports on the host (slirp `add_hostfwd`): the proxy listens
/// on `0.0.0.0:<port>` in the holder's netns and catches the traffic delivered to
/// `SLIRP_IP`. (No DNAT — the holder has no `input` chain filtering local deliveries.)
fn publish_listeners(cfg: &ProxyConfig) -> Result<()> {
    let sock = delonix_sdn::infra::slirp_sock_path();
    for l in &cfg.listeners {
        let p = l.port.to_string();
        // Best-effort/idempotent: if the port ALREADY has a hostfwd (a previous proxy
        // that crashed without teardown), the slirp refuses with 'already exists' — not
        // fatal, the desired state (port published) is already there. Only warns on other errors.
        // No per-listener host address: an HTTPRoute listener has no `-p`-style spec,
        // so it keeps the `DELONIX_PUBLISH_ADDR`/`127.0.0.1` fallback of `publish_bind_addr`.
        if let Err(e) = delonix_sdn::slirp_add_hostfwd(&sock, &p, &p, "tcp", l.addr.as_deref()) {
            let msg = e.to_string();
            // «already in use on the host» is somebody ELSE's socket — the slirp's own
            // «already exists» is our earlier publish. Reading both as «kept» reported a
            // route as served while another process answered on its port.
            let ours = !msg.contains("in use") && {
                let m = msg.to_lowercase();
                m.contains("already") || m.contains("exist")
            };
            if ours {
                eprintln!(
                    "httproute: {}",
                    super::po::tf(
                        "port :{p} already published — kept",
                        &[("p", &p.to_string())]
                    )
                );
            } else {
                // Nothing is serving what was asked for: do not leave a proxy running
                // for it, and do not call it applied.
                let _ = stop_keeping_sources(Where::Holder);
                return Err(Error::Runtime {
                    context: "httproute publish",
                    message: super::po::tf(
                        "cannot publish :{p}: {err}",
                        &[("p", &p.to_string()), ("err", &msg)],
                    ),
                });
            }
        }
    }
    Ok(())
}

/// **Stops the proxy and unpublishes the ports** (teardown of `httproute rm`). Reads
/// the ports from the config before deleting it. Best-effort/idempotent.
pub fn stop(w: Where) -> Result<()> {
    // Unpublishes the known ports (from the config, if it still exists). Only the
    // holder instance publishes through the slirp; the host one is a plain socket
    // that dies with the process.
    if let (Where::Holder, Ok(bytes)) = (w, std::fs::read(config_path(w))) {
        if let Ok(cfg) = serde_json::from_slice::<ProxyConfig>(&bytes) {
            let sock = delonix_sdn::infra::slirp_sock_path();
            for l in &cfg.listeners {
                let _ = delonix_sdn::infra::slirp_remove_hostfwd(&sock, &l.port.to_string());
            }
        }
    }
    if let Some(pid) = running_pid(w) {
        // SAFETY: SIGTERM to a pid confirmed alive and confirmed to be THIS
        // root's proxy (`proxy_argv_is_ours`).
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let _ = std::fs::remove_file(pid_path(w));
    let _ = std::fs::remove_file(config_path(w));
    // Full teardown: also the sources (manual + auto), else a subsequent start would
    // raise phantom routes again.
    let _ = std::fs::remove_file(manual_path(w));
    if w == Where::Holder {
        let _ = std::fs::remove_file(auto_path());
    }
    Ok(())
}

/// **Stops the proxy WITHOUT touching the sources**, so the next `rebuild`
/// brings it back with the same routes on the new listeners.
///
/// Distinct from [`stop`] on purpose, and the difference is not cosmetic: `stop`
/// is a teardown and deletes `manual.json` AND `auto.json`. Using it to rebind a
/// port would silently take down every route auto-registered by a
/// `container run --expose` — services that have nothing to do with the
/// document being changed, and which only come back when each container is
/// restarted.
///
/// Unpublishes the live ports here because the caller is about to bind a
/// different set: leaving the old `hostfwd` in place would keep a port answering
/// on the host with nothing behind it.
pub(crate) fn stop_keeping_sources(w: Where) -> Result<()> {
    if let (Where::Holder, Some(cfg)) = (w, live_config(w)) {
        let sock = delonix_sdn::infra::slirp_sock_path();
        for l in &cfg.listeners {
            let _ = delonix_sdn::infra::slirp_remove_hostfwd(&sock, &l.port.to_string());
        }
    }
    if let Some(pid) = running_pid(w) {
        // SAFETY: SIGTERM to a pid confirmed alive and confirmed to be THIS
        // root's proxy (`proxy_argv_is_ours`).
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let _ = std::fs::remove_file(pid_path(w));
    let _ = std::fs::remove_file(config_path(w));
    Ok(())
}

/// Is the proxy running? (for `httproute ls`/describe).
pub fn is_running(w: Where) -> bool {
    running_pid(w).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_from_before_provenance_still_parses() {
        // Written by an earlier build: no `sources` on the listener, no `stamps`.
        let old = r#"{"listeners":[{"port":80}],"routes":[]}"#;
        let c: ProxyConfig = serde_json::from_str(old).unwrap();
        assert!(c.listeners[0].sources.is_empty());
        assert!(c.stamps.is_empty());
    }

    /// ACH-017: the proxy's pidfile has to prove the ROOT, not just the shape.
    ///
    /// **Reproduced 2026-09-09**, two isolated roots (`DELONIX_ROOT` +
    /// `DELONIX_NET_RUNTIME_DIR`) on one uid, each with its own L7 proxy, with
    /// root A's `proxy.pid` made to name root B's live proxy — the state a pid
    /// wrap-around leaves behind. Both halves fired:
    ///
    /// * READ — `net httproute apply` from A returned `rc=0` and printed
    ///   "proxy #2470290 reloaded (SIGHUP)" and "proxy serving", while A's own
    ///   `:18080` answered nothing at all. The SIGHUP went to B's proxy, A's
    ///   proxy was never respawned, and the command said it was serving.
    /// * KILL — `net httproute rm` from A then SIGTERMed B's proxy: B's
    ///   `:18081` went from `200` to no response. One root's teardown took the
    ///   other root's L7 down.
    ///
    /// The argv could not have separated them on `ingress-proxy` alone — that
    /// string is in every root's proxy. What separates them is the `--config`
    /// path, which is derived from the root.
    mod tests_proxy_ownership_proof {
        use std::path::Path;

        /// A real `/proc/<pid>/cmdline`: NUL-separated, trailing NUL.
        fn cmdline(parts: &[&str]) -> Vec<u8> {
            let mut v = Vec::new();
            for p in parts {
                v.extend_from_slice(p.as_bytes());
                v.push(0);
            }
            v
        }

        /// The argv `spawn_proxy` actually writes, measured on the reproduction.
        fn proxy_of(root: &str) -> Vec<u8> {
            cmdline(&[
                "/tmp/tgt/debug/delonix",
                "ingress-proxy",
                "--config",
                &format!("{root}/httproute/config.json"),
            ])
        }

        fn ours() -> std::path::PathBuf {
            std::path::PathBuf::from("/tmp/dxp/a/httproute/config.json")
        }

        #[test]
        fn our_own_proxy_is_recognized() {
            assert!(super::super::proxy_argv_is_ours(
                &proxy_of("/tmp/dxp/a"),
                &ours()
            ));
        }

        /// THE finding. Same binary, same subcommand, another root.
        #[test]
        fn another_roots_proxy_does_not_pass_as_ours() {
            assert!(
                !super::super::proxy_argv_is_ours(&proxy_of("/tmp/dxp/b"), &ours()),
                "root B's proxy must not pass as ours — this is the process that \
                 took the SIGTERM"
            );
        }

        /// The `nsenter` wrapper form, in case it ever stops `exec`ing the proxy:
        /// the two facts are still there as their own arguments, so the answer
        /// must not change.
        #[test]
        fn the_nsenter_wrapper_form_answers_the_same() {
            let wrapped = cmdline(&[
                "nsenter",
                "-t",
                "4242",
                "-U",
                "-m",
                "-n",
                "--preserve-credentials",
                "--",
                "/tmp/tgt/debug/delonix",
                "ingress-proxy",
                "--config",
                "/tmp/dxp/a/httproute/config.json",
            ]);
            assert!(super::super::proxy_argv_is_ours(&wrapped, &ours()));
        }

        /// `--config=<path>` is as valid as the separated form we emit.
        #[test]
        fn the_joined_config_spelling_is_accepted() {
            let joined = cmdline(&[
                "delonix",
                "ingress-proxy",
                "--config=/tmp/dxp/a/httproute/config.json",
            ]);
            assert!(super::super::proxy_argv_is_ours(&joined, &ours()));
        }

        /// `ingress-proxy` as a SUBSTRING of the blob is not the proxy. The old
        /// test searched the whole `cmdline` for it, so any process whose path
        /// or argument merely spelled it answered yes.
        #[test]
        fn a_process_that_only_spells_the_name_is_not_the_proxy() {
            let impostor = cmdline(&[
                "/usr/bin/tail",
                "-f",
                "/tmp/dxp/a/httproute/ingress-proxy.log",
            ]);
            assert!(!super::super::proxy_argv_is_ours(&impostor, &ours()));
        }

        /// A recycled pid on an unrelated process — the case the guard was
        /// written for, and which must keep being refused.
        #[test]
        fn a_recycled_pid_on_a_stranger_is_not_ours() {
            let stranger = cmdline(&["/usr/lib/firefox/firefox", "-contentproc"]);
            assert!(!super::super::proxy_argv_is_ours(&stranger, &ours()));
            assert!(!super::super::proxy_argv_is_ours(&[], &ours()));
        }

        /// No `--config` at all is no token, and no token is no proof — the same
        /// rule the slirp's ownership check follows.
        #[test]
        fn a_proxy_without_a_config_proves_nothing() {
            let bare = cmdline(&["delonix", "ingress-proxy"]);
            assert!(!super::super::proxy_argv_is_ours(&bare, &ours()));
        }

        /// A prefix is not a path: `/tmp/dxp/ab` must not answer for
        /// `/tmp/dxp/a`, which is what a plain `starts_with` would have done.
        #[test]
        fn a_sibling_root_with_a_prefix_name_is_not_ours() {
            assert!(!super::super::proxy_argv_is_ours(
                &proxy_of("/tmp/dxp/ab"),
                &ours()
            ));
            assert_ne!(
                Path::new("/tmp/dxp/ab/httproute/config.json"),
                ours().as_path()
            );
        }
    }

    fn r(host: &str, path: &str, backend: &str) -> Route {
        Route {
            host: host.to_string(),
            path: path.to_string(),
            backend: backend.to_string(),
            source: String::new(),
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
    fn pick_route_respeita_fronteira_de_segmento() {
        // BUG regression guard: `/api-docs` used to match the `/api` route
        // (raw string-prefix match, no segment boundary) instead of falling
        // through to `/` — exactly the k8s Gateway API PathPrefix semantics
        // AGENTS.md/`valid_path_prefix` claim but the runtime matcher didn't
        // enforce.
        let routes = vec![
            r("loja.ex", "/", "public:80"),
            r("loja.ex", "/api", "internal:80"),
        ];
        assert_eq!(
            pick_route(&routes, "loja.ex", "/api-docs").unwrap().backend,
            "public:80",
            "/api-docs não é um sub-caminho de /api"
        );
        assert_eq!(
            pick_route(&routes, "loja.ex", "/api").unwrap().backend,
            "internal:80",
            "/api exacto continua a casar"
        );
        assert_eq!(
            pick_route(&routes, "loja.ex", "/api/v1").unwrap().backend,
            "internal:80",
            "/api/v1 é um sub-caminho real de /api"
        );
    }

    #[test]
    fn path_prefix_matches_casos_directos() {
        assert!(super::path_prefix_matches("/api", "/api"));
        assert!(super::path_prefix_matches("/api/v1", "/api"));
        assert!(!super::path_prefix_matches("/api-docs", "/api"));
        assert!(super::path_prefix_matches("/anything", "/")); // root matches all
        assert!(super::path_prefix_matches("/api/v1", "/api/"));
        assert!(!super::path_prefix_matches("/apiX", "/api/"));
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
                addr: None,
                sources: Vec::new(),
            }],
            routes: vec![r("loja.ex", "/", "10.0.0.2:8080")],
            tls: Some(TlsMaterial {
                cert_pem: "C".into(),
                key_pem: "K".into(),
                mode: "secretRef".into(),
            }),
            published_hosts: Vec::new(),
            claims: Vec::new(),
            stamps: Vec::new(),
            bind: None,
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
                    addr: None,
                    sources: Vec::new(),
                },
                Listener {
                    port: 443,
                    tls: true,
                    addr: None,
                    sources: Vec::new(),
                },
            ],
            routes: vec![r("loja.ex", "/", "10.0.0.2:8080")],
            tls: None,
            published_hosts: Vec::new(),
            claims: Vec::new(),
            stamps: Vec::new(),
            bind: None,
        };
        let js = serde_json::to_string(&cfg).unwrap();
        let back: ProxyConfig = serde_json::from_str(&js).unwrap();
        assert_eq!(back.listeners.len(), 2);
        assert_eq!(back.routes[0].backend, "10.0.0.2:8080");
    }
}
