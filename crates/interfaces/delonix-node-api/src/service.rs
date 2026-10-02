//! `NodeService` — what answers on the socket, gRPC and HTTP/JSON alike.

use std::collections::HashMap;
use std::pin::Pin;

use axum::extract::Query;
use axum::response::IntoResponse;
use tonic::{Request, Response, Status};

use crate::proto::v1::node_service_server::{NodeService, NodeServiceServer};
use crate::proto::v1::{
    Capacity, Event, GetCapacityRequest, GetHealthRequest, GetNodeInfoRequest, Health,
    ListProvidersRequest, ListProvidersResponse, NodeInfo, WatchEventsRequest,
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
    Ok(ListProvidersResponse {
        providers: reports
            .iter()
            .filter(|r| kind.as_deref().is_none_or(|k| r.kind.as_str() == k))
            .map(providers::provider_info)
            .collect(),
    })
}

#[tonic::async_trait]
impl NodeService for NodeApi {
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

/// The router both transports share: the JSON routes first, the gRPC service
/// merged in (tonic's routes are an axum router underneath). Exposed for the
/// in-process tests, which drive it with `tower::ServiceExt::oneshot`.
pub fn router() -> axum::Router {
    use tonic::server::NamedService;
    // The gRPC service is mounted the way tonic's own `Routes` mounts it — one
    // wildcard route under the service name. Not through `Routes::into_axum_router`,
    // whose fallback answers EVERY unknown path with HTTP 200 + `grpc-status: 12`:
    // measured in the battery, `GET /v1/node` came back 200 with an empty body,
    // which a REST client reads as "served, nothing there". The fallback below
    // keeps that answer for gRPC callers and gives HTTP callers a 404.
    axum::Router::new()
        .route("/v1/providers", axum::routing::get(http_list_providers))
        .route(
            "/v1/node",
            axum::routing::get(|| json("/v1/node", node_info())),
        )
        .route(
            "/v1/node/health",
            axum::routing::get(|| json("/v1/node/health", health())),
        )
        .route(
            "/v1/node/capacity",
            axum::routing::get(|| json("/v1/node/capacity", capacity())),
        )
        .route("/openapi.json", axum::routing::get(openapi_json))
        .route("/docs", axum::routing::get(crate::docs::swagger))
        .route("/redoc", axum::routing::get(crate::docs::redoc))
        .route("/docs/assets/:name", axum::routing::get(crate::docs::asset))
        .route_service(
            &format!("/{}/*rest", NodeServiceServer::<NodeApi>::NAME),
            NodeServiceServer::new(NodeApi),
        )
        .fallback(fallback)
        .method_not_allowed_fallback(method_not_allowed)
}

/// What an unknown path gets: a gRPC caller (content-type `application/grpc…`)
/// the wire-level UNIMPLEMENTED tonic would give; anyone else a 404 carrying a
/// problem document (DX-4001, `application/problem+json`) — never a 200 with
/// nothing in it.
async fn fallback(req: axum::extract::Request) -> axum::response::Response {
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

/// One RPC's answer as its `google.api.http` JSON: the message, or the error
/// as an RFC 9457 problem document.
async fn json<T: serde::Serialize>(
    path: &str,
    answer: impl std::future::Future<Output = Result<T, Status>>,
) -> axum::response::Response {
    match answer.await {
        Ok(msg) => (hyper::StatusCode::OK, axum::Json(msg)).into_response(),
        Err(status) => problem(status, path),
    }
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
/// problem document (DX-4001: no handler for this method and path). Every REST
/// route of this socket is `GET` today — a route that gains another method
/// changes this, and `the_rest_routes_are_get_only` says so.
async fn method_not_allowed(req: axum::extract::Request) -> axum::response::Response {
    let path = req.uri().path().to_string();
    let mut res = problem_as(
        Status::not_found(format!(
            "route {} {path} on the node API (this path answers GET)",
            req.method()
        )),
        &path,
        Some(hyper::StatusCode::METHOD_NOT_ALLOWED),
    );
    res.headers_mut().insert(
        hyper::header::ALLOW,
        hyper::header::HeaderValue::from_static("GET"),
    );
    res
}

/// `GET /v1/providers[?kind=]` — the `google.api.http` mapping of
/// `ListProviders`, written by hand (one route today; see the crate docs).
async fn http_list_providers(Query(q): Query<HashMap<String, String>>) -> axum::response::Response {
    let req = ListProvidersRequest {
        kind: q.get("kind").cloned().unwrap_or_default(),
    };
    match list_providers(req).await {
        Ok(resp) => (hyper::StatusCode::OK, axum::Json(resp)).into_response(),
        Err(status) => problem(status, "/v1/providers"),
    }
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
