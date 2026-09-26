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
use crate::providers;

/// The service. Stateless: every answer is computed from the engine's
/// declarations and this host's probes at call time.
#[derive(Clone, Default)]
pub struct NodeApi;

/// The RPCs the contract has and this server does not serve yet answer with
/// the step that brings them — a refusal a client can read, never a default
/// body that looks like an answer.
fn not_yet(rpc: &str) -> Status {
    Status::unimplemented(format!(
        "{rpc} is not served yet: it lands with ADR-0042 step C (health/info/capacity) and \
         ADR-0040 P5 (WatchEvents); ListProviders is what this socket serves today"
    ))
}

/// `ListProviders`, the one function both encodings call (ADR-0050 D5).
/// `kind` empty = all; a kind the catalog does not have is `INVALID_ARGUMENT`
/// (the same refusal `provider ls --kind ceph` gives), not an empty list.
pub async fn list_providers(req: ListProvidersRequest) -> Result<ListProvidersResponse, Status> {
    let kind = match req.kind.as_str() {
        "" => None,
        k @ ("compute" | "network" | "storage" | "image") => Some(k.to_string()),
        other => {
            return Err(Status::invalid_argument(format!(
                "unknown provider kind '{other}': compute, network, storage or image"
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
        Err(not_yet("GetNodeInfo"))
    }

    async fn get_health(
        &self,
        _req: Request<GetHealthRequest>,
    ) -> Result<Response<Health>, Status> {
        Err(not_yet("GetHealth"))
    }

    async fn get_capacity(
        &self,
        _req: Request<GetCapacityRequest>,
    ) -> Result<Response<Capacity>, Status> {
        Err(not_yet("GetCapacity"))
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
