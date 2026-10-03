//! `NodeService` — what answers on the socket, gRPC and HTTP/JSON alike.

use std::pin::Pin;

use axum::response::IntoResponse;
use tonic::{Request, Response, Status};

use crate::proto::v1::node_service_server::{NodeService, NodeServiceServer};
use crate::proto::v1::{
    ApiRoot, Capacity, Event, GetApiRootRequest, GetCapacityRequest, GetHealthRequest,
    GetNodeInfoRequest, Health, ListProvidersRequest, ListProvidersResponse, NodeInfo,
    WatchEventsRequest,
};
use crate::{node, providers};

/// The service. Stateless: every answer is computed from the engine's
/// declarations and this host's probes at call time.
#[derive(Clone, Default)]
pub struct NodeApi;

/// The RPC the contract has and this server does not serve yet answers with
/// the step that brings it — a refusal a client can read, never a default body
/// that looks like an answer.
fn not_yet(rpc: &str) -> Status {
    Status::unimplemented(format!(
        "{rpc} is not served yet: it lands with ADR-0040 P5"
    ))
}

/// Runs a host probe off the runtime's workers: every `NodeService` answer
/// reads files, cgroups and provider probes, which block.
async fn blocking<T: Send + 'static>(
    what: &'static str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Status::internal(format!("{what} panicked: {e}")))
}

/// `GetNodeInfo`, the one function both encodings call.
pub async fn node_info() -> Result<NodeInfo, Status> {
    blocking("node info", || {
        node::node_info(&providers::measured_reports())
    })
    .await
}

/// `GetHealth`, the one function both encodings call.
pub async fn health() -> Result<Health, Status> {
    blocking("health", || node::health(&providers::measured_reports())).await
}

/// `GetCapacity`, the one function both encodings call.
pub async fn capacity() -> Result<Capacity, Status> {
    blocking("capacity", node::capacity).await
}

/// `ListProviders`, the one function both encodings call (ADR-0050 D5).
/// `kind` empty = all; a kind the catalog does not have is `INVALID_ARGUMENT`
/// (the same refusal `provider ls --kind ceph` gives), not an empty list.
pub async fn list_providers(req: ListProvidersRequest) -> Result<ListProvidersResponse, Status> {
    let kind = match req.kind.as_str() {
        "" => None,
        k @ ("compute" | "network" | "storage" | "image" | "gateway") => Some(k.to_string()),
        other => {
            return Err(Status::invalid_argument(format!(
                "unknown provider kind '{other}': compute, network, storage, image or gateway"
            )))
        }
    };
    // The reports probe the host (binaries on the PATH, /dev/kvm, the holder):
    // blocking work, off the runtime's workers.
    let reports = tokio::task::spawn_blocking(providers::measured_reports)
        .await
        .map_err(|e| Status::internal(format!("provider probe panicked: {e}")))?;
    let this = match &kind {
        Some(k) => format!("/v1/providers?kind={k}"),
        None => "/v1/providers".to_string(),
    };
    Ok(ListProvidersResponse {
        links: vec![node::link("self", &this), node::link("root", "/v1")],
        providers: reports
            .iter()
            .filter(|r| kind.as_deref().is_none_or(|k| r.kind.as_str() == k))
            .map(providers::provider_info)
            .collect(),
    })
}

#[tonic::async_trait]
impl NodeService for NodeApi {
    async fn get_api_root(
        &self,
        _req: Request<GetApiRootRequest>,
    ) -> Result<Response<ApiRoot>, Status> {
        Ok(Response::new(node::api_root()))
    }

    async fn get_node_info(
        &self,
        _req: Request<GetNodeInfoRequest>,
    ) -> Result<Response<NodeInfo>, Status> {
        Ok(Response::new(node_info().await?))
    }

    async fn get_health(
        &self,
        _req: Request<GetHealthRequest>,
    ) -> Result<Response<Health>, Status> {
        Ok(Response::new(health().await?))
    }

    async fn get_capacity(
        &self,
        _req: Request<GetCapacityRequest>,
    ) -> Result<Response<Capacity>, Status> {
        Ok(Response::new(capacity().await?))
    }

    async fn list_providers(
        &self,
        req: Request<ListProvidersRequest>,
    ) -> Result<Response<ListProvidersResponse>, Status> {
        Ok(Response::new(list_providers(req.into_inner()).await?))
    }

    type WatchEventsStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<Event, Status>> + Send + 'static>>;

    async fn watch_events(
        &self,
        _req: Request<WatchEventsRequest>,
    ) -> Result<Response<Self::WatchEventsStream>, Status> {
        Err(not_yet("WatchEvents"))
    }
}

/// The router both transports share. The contract's REST routes are not
/// listed here: they are resolved against the table `build.rs` generates from
/// the `google.api.http` annotations ([`crate::transcode`]), in the fallback —
/// a template such as `{name}:start` is not something an axum route spells.
/// The gRPC service is merged in (tonic's routes are an axum router
/// underneath). Exposed for the in-process tests, which drive it with
/// `tower::ServiceExt::oneshot`.
pub fn router() -> axum::Router {
    use tonic::server::NamedService;
    // The gRPC service is mounted the way tonic's own `Routes` mounts it — one
    // wildcard route under the service name. Not through `Routes::into_axum_router`,
    // whose fallback answers EVERY unknown path with HTTP 200 + `grpc-status: 12`:
    // measured in the battery, `GET /v1/node` came back 200 with an empty body,
    // which a REST client reads as "served, nothing there". The fallback below
    // keeps that answer for gRPC callers and gives HTTP callers a 404.
    axum::Router::new()
        .route("/openapi.json", axum::routing::get(openapi_json))
        .route("/docs", axum::routing::get(crate::docs::swagger))
        .route("/redoc", axum::routing::get(crate::docs::redoc))
        .route("/docs/assets/:name", axum::routing::get(crate::docs::asset))
        .route_service(
            &format!("/{}/*rest", NodeServiceServer::<NodeApi>::NAME),
            NodeServiceServer::new(NodeApi),
        )
        .fallback(fallback)
        .method_not_allowed_fallback(|req: axum::extract::Request| async move {
            let path = req.uri().path().to_string();
            method_not_allowed(req.method().as_str(), &path, &["GET"])
        })
}

/// The largest request body the REST encoding reads.
const MAX_BODY: usize = 1024 * 1024;

/// A request for one of the contract's REST routes: bound, dispatched to the
/// service method the gRPC encoding calls, answered as JSON. A route of a
/// service this engine does not serve yet is 501 (DX-6001) — in the contract,
/// not here yet — which is a different answer from a path the contract does
/// not have (404).
async fn rest(
    route: &'static crate::transcode::Route,
    vars: Vec<(&'static str, String)>,
    req: axum::extract::Request,
) -> axum::response::Response {
    use crate::transcode;
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let answer = async {
        let body = axum::body::to_bytes(req.into_body(), MAX_BODY)
            .await
            .map_err(|_| {
                Status::invalid_argument(format!(
                    "{}: the request body is larger than {MAX_BODY} bytes",
                    route.rpc
                ))
            })?;
        let input = transcode::bind(route, &vars, query.as_deref(), &body)?;
        match route.service {
            "NodeService" => transcode::dispatch_node_service(&NodeApi, route.rpc, input).await,
            _ => Err(transcode::not_served(route)),
        }
    };
    match answer.await {
        Ok(body) => with_link_header(body),
        Err(status) => problem(status, &path),
    }
}

/// Everything the router has no route for. One of the contract's REST routes
/// is served ([`rest`]); a contract path on another method is 405 with the
/// methods the contract maps; and an unknown path gets: a gRPC caller (content-type `application/grpc…`)
/// the wire-level UNIMPLEMENTED tonic would give; anyone else a 404 carrying a
/// problem document (DX-4001, `application/problem+json`) — never a 200 with
/// nothing in it.
async fn fallback(req: axum::extract::Request) -> axum::response::Response {
    use crate::transcode::Resolved;
    match crate::transcode::resolve(req.method().as_str(), req.uri().path()) {
        Resolved::Route(route, vars) => return rest(route, vars, req).await,
        Resolved::OtherMethods(allow) => {
            return method_not_allowed(req.method().as_str(), req.uri().path(), &allow)
        }
        Resolved::Unknown => {}
    }
    let is_grpc = req
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/grpc"));
    if is_grpc {
        return hyper::Response::builder()
            .status(hyper::StatusCode::OK)
            .header("grpc-status", "12")
            .header(hyper::header::CONTENT_TYPE, "application/grpc")
            .body(axum::body::Body::empty())
            .expect("static response");
    }
    let path = req.uri().path().to_string();
    problem(
        Status::not_found(format!("route {} {path} on the node API", req.method())),
        &path,
    )
}

/// A message's JSON with the RFC 8288 `Link` header mirroring its own `links`
/// (ADR-0042 D2) — read from the body being sent, so the header and the body
/// cannot disagree.
fn with_link_header(body: serde_json::Value) -> axum::response::Response {
    let header = link_header(&body);
    let mut res = (hyper::StatusCode::OK, axum::Json(body)).into_response();
    if let Some(v) = header.and_then(|h| hyper::header::HeaderValue::from_str(&h).ok()) {
        res.headers_mut().insert(hyper::header::LINK, v);
    }
    res
}

/// `</v1/node>; rel="self", </v1>; rel="root"` from a message's `links`;
/// `None` when it has none.
pub fn link_header(body: &serde_json::Value) -> Option<String> {
    let links = body.get("links")?.as_array()?;
    let parts: Vec<String> = links
        .iter()
        .filter_map(|l| {
            let href = l.get("href")?.as_str()?;
            let rel = l.get("rel")?.as_str()?;
            Some(format!("<{href}>; rel=\"{rel}\""))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// The published OpenAPI document — the one the contract gate generates from
/// `proto/` and keeps committed (`docs/api/openapi.yaml`), embedded at build
/// time and served as JSON (ADR-0042 D3). Never written by hand here.
pub const OPENAPI_YAML: &str = include_str!("../../../../docs/api/openapi.yaml");

/// `OPENAPI_YAML` as JSON, converted once.
pub fn openapi_document() -> &'static serde_json::Value {
    static DOC: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    DOC.get_or_init(|| {
        serde_yaml::from_str(OPENAPI_YAML)
            .expect("docs/api/openapi.yaml is the generated contract and parses")
    })
}

async fn openapi_json() -> axum::response::Response {
    (hyper::StatusCode::OK, axum::Json(openapi_document())).into_response()
}

/// A path this socket serves, on a method it does not: 405 with `Allow` and a
/// problem document (DX-4001: no handler for this method and path). `allow` is
/// what the contract maps for the path, or `GET` for the documentation routes.
fn method_not_allowed(method: &str, path: &str, allow: &[&str]) -> axum::response::Response {
    let allow = allow.join(", ");
    let mut res = problem_as(
        Status::not_found(format!(
            "route {method} {path} on the node API (this path answers {allow})"
        )),
        path,
        Some(hyper::StatusCode::METHOD_NOT_ALLOWED),
    );
    if let Ok(v) = hyper::header::HeaderValue::from_str(&allow) {
        res.headers_mut().insert(hyper::header::ALLOW, v);
    }
    res
}

/// A failure as the engine's error, with its dictionary number, and the HTTP
/// status the REST encoding answers with. `UNIMPLEMENTED` is 501 — the
/// operation is in the contract and this version does not serve it, and a 503
/// would tell a client to retry (its class's status); everything else follows
/// the class.
pub fn engine_error(status: &Status) -> (delonix_model::Error, hyper::StatusCode) {
    use delonix_model::Error;
    let msg = status.message().to_string();
    match status.code() {
        tonic::Code::InvalidArgument | tonic::Code::FailedPrecondition => {
            (Error::Invalid(msg), hyper::StatusCode::BAD_REQUEST)
        }
        tonic::Code::NotFound => (
            Error::coded(4001, Error::NotFound(msg)),
            hyper::StatusCode::NOT_FOUND,
        ),
        tonic::Code::Unimplemented => (
            Error::coded(6001, Error::Unavailable(msg)),
            hyper::StatusCode::NOT_IMPLEMENTED,
        ),
        tonic::Code::PermissionDenied => {
            (Error::PermissionDenied(msg), hyper::StatusCode::FORBIDDEN)
        }
        _ => (
            Error::coded(
                9005,
                Error::Runtime {
                    context: "node-api",
                    message: msg,
                },
            ),
            hyper::StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// A failure as the RFC 9457 problem document the OpenAPI declares for every
/// error (ADR-0042 D2): the engine's own `codes::problem` — the same document
/// the CLI's errors map to — with `instance` the request path and
/// `grpc_status` the code the gRPC encoding carries for the same failure.
fn problem(status: Status, instance: &str) -> axum::response::Response {
    problem_as(status, instance, None)
}

/// [`problem`] with an HTTP status other than the failure's own (405 for a
/// method the path does not answer).
fn problem_as(
    status: Status,
    instance: &str,
    http: Option<hyper::StatusCode>,
) -> axum::response::Response {
    let (err, own) = engine_error(&status);
    let http = http.unwrap_or(own);
    let mut doc = delonix_model::codes::problem(&err, Some(instance));
    doc["status"] = http.as_u16().into();
    doc["grpc_status"] = (status.code() as i32).into();
    let mut res = (http, axum::Json(doc)).into_response();
    res.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/problem+json"),
    );
    res
}
