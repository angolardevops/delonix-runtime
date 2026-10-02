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
        .route("/v1/node", axum::routing::get(|| json(node_info())))
        .route("/v1/node/health", axum::routing::get(|| json(health())))
        .route("/v1/node/capacity", axum::routing::get(|| json(capacity())))
        .route("/openapi.json", axum::routing::get(openapi_json))
        .route("/docs", axum::routing::get(crate::docs::swagger))
        .route("/redoc", axum::routing::get(crate::docs::redoc))
        .route("/docs/assets/:name", axum::routing::get(crate::docs::asset))
        .route_service(
            &format!("/{}/*rest", NodeServiceServer::<NodeApi>::NAME),
            NodeServiceServer::new(NodeApi),
        )
        .fallback(fallback)
}

/// What an unknown path gets: a gRPC caller (content-type `application/grpc…`)
/// the wire-level UNIMPLEMENTED tonic would give; anyone else a 404 carrying a
/// `google.rpc.Status` with code 5 (NOT_FOUND) — never a 200 with nothing in it.
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
    rpc_error(Status::not_found(format!(
        "{} {} is not a route of this socket; the served paths are those of the \
         published OpenAPI that have a handler here (see `delonix serve node-api --help`)",
        req.method(),
        req.uri().path()
    )))
}

/// One RPC's answer as its `google.api.http` JSON: the message, or the error
/// as `google.rpc.Status`.
async fn json<T: serde::Serialize>(
    answer: impl std::future::Future<Output = Result<T, Status>>,
) -> axum::response::Response {
    match answer.await {
        Ok(msg) => (hyper::StatusCode::OK, axum::Json(msg)).into_response(),
        Err(status) => rpc_error(status),
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

/// `GET /v1/providers[?kind=]` — the `google.api.http` mapping of
/// `ListProviders`, written by hand (one route today; see the crate docs).
async fn http_list_providers(Query(q): Query<HashMap<String, String>>) -> axum::response::Response {
    let req = ListProvidersRequest {
        kind: q.get("kind").cloned().unwrap_or_default(),
    };
    match list_providers(req).await {
        Ok(resp) => (hyper::StatusCode::OK, axum::Json(resp)).into_response(),
        Err(status) => rpc_error(status),
    }
}

/// A gRPC status as the `google.rpc.Status` JSON the OpenAPI declares for every
/// error (`code` is the gRPC code number, as the mapping specifies), with the
/// HTTP status the gRPC-Gateway convention gives that code.
fn rpc_error(status: Status) -> axum::response::Response {
    let http = match status.code() {
        tonic::Code::InvalidArgument => hyper::StatusCode::BAD_REQUEST,
        tonic::Code::NotFound => hyper::StatusCode::NOT_FOUND,
        tonic::Code::Unimplemented => hyper::StatusCode::NOT_IMPLEMENTED,
        tonic::Code::FailedPrecondition => hyper::StatusCode::BAD_REQUEST,
        tonic::Code::PermissionDenied => hyper::StatusCode::FORBIDDEN,
        _ => hyper::StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = serde_json::json!({
        "code": status.code() as i32,
        "message": status.message(),
        "details": [],
    });
    (http, axum::Json(body)).into_response()
}
