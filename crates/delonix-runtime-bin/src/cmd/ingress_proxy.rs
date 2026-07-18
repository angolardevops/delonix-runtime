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

/// Teto de tempo para o backend responder (senão 504) — evita que um backend
/// pendurado prenda a ligação/task para sempre (slowloris do lado do backend).
const BACKEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
        "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer",
        "transfer-encoding", "upgrade",
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
    Response::builder().status(code).body(body).expect("resposta estática válida")
}

/// Trata um pedido: casa a rota e encaminha (streaming) para o backend, ou
/// devolve 404 (sem rota) / 502 (backend inacessível).
async fn handle(
    req: Request<Incoming>,
    cfg: Arc<ProxyConfig>,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
) -> std::result::Result<Response<RespBody>, Infallible> {
    let host = req_host(&req);
    let path = req.uri().path().to_string();
    let Some(route) = pick_route(&cfg.routes, &host, &path) else {
        return Ok(text_response(StatusCode::NOT_FOUND, "delonix: sem rota para este host/path\n"));
    };
    let backend = route.backend.clone();

    // Reconstrói o pedido para o backend: mesma method/headers/corpo, URI absoluta
    // `http://<backend><path?query>`. O corpo (`Incoming`) é reencaminhado sem
    // bufferizar (streaming) — hyper transfere-o à medida que chega.
    let pq = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
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
        Err(_) => return Ok(text_response(StatusCode::BAD_GATEWAY, "delonix: pedido inválido\n")),
    };

    // Teto de tempo: um backend pendurado não pode prender a ligação para sempre.
    match tokio::time::timeout(BACKEND_TIMEOUT, client.request(out_req)).await {
        Ok(Ok(resp)) => {
            // A resposta do backend flui de volta em streaming; também lhe tiramos
            // os hop-by-hop antes de a devolver ao cliente.
            let (mut rparts, body) = resp.into_parts();
            strip_hop_by_hop(&mut rparts.headers);
            let body = body.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>).boxed();
            Ok(Response::from_parts(rparts, body))
        }
        Ok(Err(e)) => Ok(text_response(
            StatusCode::BAD_GATEWAY,
            &format!("delonix: backend {backend} inacessível: {e}\n"),
        )),
        Err(_elapsed) => Ok(text_response(
            StatusCode::GATEWAY_TIMEOUT,
            &format!("delonix: backend {backend} não respondeu em {}s\n", BACKEND_TIMEOUT.as_secs()),
        )),
    }
}

/// Aceita ligações numa porta e serve cada uma (http1) com o handler de rota.
async fn accept_loop(
    listener: TcpListener,
    cfg: Arc<ProxyConfig>,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
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
        let cfg = cfg.clone();
        let client = client.clone();
        tokio::task::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| handle(req, cfg.clone(), client.clone()));
            // NOTA: WebSocket/`Connection: Upgrade` ainda NÃO é tunelado — o cliente
            // hyper-util legacy não estabelece a ligação trocada, e nós removemos o
            // header `Upgrade` (hop-by-hop) no encaminhamento. Tunelar upgrades
            // (hyper::upgrade::on nos dois lados + cópia bidireccional) é um
            // follow-up; hoje só HTTP request/response é proxiado.
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, svc).await;
        });
    }
}

/// Núcleo assíncrono: bind de cada listener (HTTP; TLS fica p/ a Fase 3) + servir.
async fn serve(cfg: ProxyConfig) -> Result<()> {
    let cfg = Arc::new(cfg);
    let client: Client<_, Incoming> =
        Client::builder(TokioExecutor::new()).build(hyper_util::client::legacy::connect::HttpConnector::new());

    let mut handles = Vec::new();
    for l in &cfg.listeners {
        if l.tls {
            eprintln!("ingress-proxy: listener :{} pede TLS — ainda não suportado (Fase 3), a saltar", l.port);
            continue;
        }
        let addr = SocketAddr::from(([0, 0, 0, 0], l.port));
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| Error::Runtime { context: "ingress-proxy bind", message: format!("{addr}: {e}") })?;
        eprintln!("ingress-proxy: a escutar em {addr} ({} rota(s))", cfg.routes.len());
        handles.push(tokio::spawn(accept_loop(listener, cfg.clone(), client.clone())));
    }
    if handles.is_empty() {
        return Err(Error::Invalid("ingress-proxy: nenhum listener HTTP para servir".into()));
    }
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

/// Ponto de entrada do subcomando `delonix ingress-proxy --config <ficheiro>`.
/// Lê a `ProxyConfig` (JSON) e corre o servidor até morrer (bloqueia).
pub fn run(config_path: &Path) -> Result<()> {
    let bytes = std::fs::read(config_path)
        .map_err(|e| Error::Invalid(format!("ingress-proxy: não li a config {}: {e}", config_path.display())))?;
    let cfg: ProxyConfig = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Invalid(format!("ingress-proxy: config inválida: {e}")))?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Runtime { context: "ingress-proxy runtime", message: e.to_string() })?;
    rt.block_on(serve(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(host: &str, path: &str, backend: &str) -> Route {
        Route { host: host.to_string(), path: path.to_string(), backend: backend.to_string() }
    }

    #[test]
    fn pick_route_prefere_host_especifico_e_prefixo_mais_longo() {
        let routes = vec![
            r("", "/", "10.0.0.1:80"),           // qualquer host, raiz
            r("loja.ex", "/", "10.0.0.2:80"),    // host específico, raiz
            r("loja.ex", "/api", "10.0.0.3:80"), // host específico, /api (mais longo)
        ];
        // /api na loja → a rota /api (prefixo mais longo)
        assert_eq!(pick_route(&routes, "loja.ex", "/api/x").unwrap().backend, "10.0.0.3:80");
        // / na loja → a rota host-específica, não a de qualquer-host
        assert_eq!(pick_route(&routes, "loja.ex", "/home").unwrap().backend, "10.0.0.2:80");
        // outro host → só casa a de qualquer-host
        assert_eq!(pick_route(&routes, "outro.ex", "/api").unwrap().backend, "10.0.0.1:80");
    }

    #[test]
    fn pick_route_sem_correspondencia() {
        let routes = vec![r("loja.ex", "/", "10.0.0.2:80")];
        assert!(pick_route(&routes, "outro.ex", "/").is_none());
    }

    #[test]
    fn config_roundtrip_json() {
        let cfg = ProxyConfig {
            listeners: vec![Listener { port: 80, tls: false }, Listener { port: 443, tls: true }],
            routes: vec![r("loja.ex", "/", "10.0.0.2:8080")],
        };
        let js = serde_json::to_string(&cfg).unwrap();
        let back: ProxyConfig = serde_json::from_str(&js).unwrap();
        assert_eq!(back.listeners.len(), 2);
        assert_eq!(back.routes[0].backend, "10.0.0.2:8080");
    }
}
