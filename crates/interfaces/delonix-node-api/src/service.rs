//! `NodeService` — what answers on the socket, gRPC and HTTP/JSON alike.

use std::pin::Pin;

use axum::response::IntoResponse;
use tonic::{Request, Response, Status};

use crate::proto::v1::network_service_server::{NetworkService, NetworkServiceServer};
use crate::proto::v1::node_service_server::{NodeService, NodeServiceServer};
use crate::proto::v1::operation_service_server::{OperationService, OperationServiceServer};
use crate::proto::v1::virtual_machine_service_server::{
    VirtualMachineService, VirtualMachineServiceServer,
};
use crate::proto::v1::volume_service_server::{VolumeService, VolumeServiceServer};
use crate::proto::v1::{
    ApiRoot, Capacity, Event, GetApiRootRequest, GetCapacityRequest, GetHealthRequest,
    GetNodeInfoRequest, Health, ListProvidersRequest, ListProvidersResponse, NodeInfo,
    WatchEventsRequest,
};
use crate::proto::v1::{
    CancelOperationRequest, GetOperationRequest, ListOperationsRequest, ListOperationsResponse,
    WatchOperationRequest,
};
use crate::proto::v1::{
    ConnectContainerRequest, Container, CreateNetworkRequest, DeleteNetworkRequest,
    DisconnectContainerRequest, GetNetworkRequest, ListNetworksRequest, ListNetworksResponse,
    Network, Operation,
};
use crate::proto::v1::{
    ConsoleRequest, ConsoleResponse, CreateSnapshotRequest, CreateVirtualMachineRequest,
    DeleteSnapshotRequest, DeleteVirtualMachineRequest, GetVirtualMachineRequest,
    ListSnapshotsRequest, ListSnapshotsResponse, ListVirtualMachinesRequest,
    ListVirtualMachinesResponse, PauseVirtualMachineRequest, RestoreSnapshotRequest,
    ResumeVirtualMachineRequest, StartVirtualMachineRequest, StopVirtualMachineRequest,
    VirtualMachine,
};
use crate::proto::v1::{
    CreateVolumeRequest, DeleteVolumeRequest, GetVolumeRequest, ListVolumesRequest,
    ListVolumesResponse, Volume,
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

/// A served service's RPC that is not served yet — the same answer the REST
/// encoding gives for a whole service that is not ([`crate::transcode::not_served`]).
fn unserved(service: &str, rpc: &str) -> Status {
    Status::unimplemented(format!(
        "{service}.{rpc} is in the contract and this engine does not serve it yet"
    ))
}

#[tonic::async_trait]
impl NetworkService for NodeApi {
    async fn get_network(
        &self,
        req: Request<GetNetworkRequest>,
    ) -> Result<Response<Network>, Status> {
        let req = req.into_inner();
        blocking("network", move || {
            crate::networks::get_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn list_networks(
        &self,
        req: Request<ListNetworksRequest>,
    ) -> Result<Response<ListNetworksResponse>, Status> {
        let req = req.into_inner();
        blocking("networks", move || {
            crate::networks::list_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn create_network(
        &self,
        req: Request<CreateNetworkRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        blocking("network create", move || {
            crate::network_ops::create_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn delete_network(
        &self,
        req: Request<DeleteNetworkRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        blocking("network delete", move || {
            crate::network_ops::delete_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn connect_container(
        &self,
        _req: Request<ConnectContainerRequest>,
    ) -> Result<Response<Container>, Status> {
        Err(unserved("NetworkService", "ConnectContainer"))
    }

    async fn disconnect_container(
        &self,
        _req: Request<DisconnectContainerRequest>,
    ) -> Result<Response<Container>, Status> {
        Err(unserved("NetworkService", "DisconnectContainer"))
    }
}

#[tonic::async_trait]
impl VolumeService for NodeApi {
    async fn get_volume(&self, req: Request<GetVolumeRequest>) -> Result<Response<Volume>, Status> {
        let req = req.into_inner();
        blocking("volume", move || {
            crate::volumes::get_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn list_volumes(
        &self,
        req: Request<ListVolumesRequest>,
    ) -> Result<Response<ListVolumesResponse>, Status> {
        let req = req.into_inner();
        blocking("volumes", move || {
            crate::volumes::list_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn create_volume(
        &self,
        _req: Request<CreateVolumeRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(unserved("VolumeService", "CreateVolume"))
    }

    async fn delete_volume(
        &self,
        _req: Request<DeleteVolumeRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(unserved("VolumeService", "DeleteVolume"))
    }
}

#[tonic::async_trait]
impl VirtualMachineService for NodeApi {
    async fn get_virtual_machine(
        &self,
        req: Request<GetVirtualMachineRequest>,
    ) -> Result<Response<VirtualMachine>, Status> {
        let req = req.into_inner();
        blocking("virtual machine", move || {
            crate::vms::get_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn list_virtual_machines(
        &self,
        req: Request<ListVirtualMachinesRequest>,
    ) -> Result<Response<ListVirtualMachinesResponse>, Status> {
        let req = req.into_inner();
        blocking("virtual machines", move || {
            crate::vms::list_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn create_virtual_machine(
        &self,
        req: Request<CreateVirtualMachineRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        blocking("virtual machine create", move || {
            crate::vm_ops::create_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn delete_virtual_machine(
        &self,
        req: Request<DeleteVirtualMachineRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        blocking("virtual machine delete", move || {
            crate::vm_ops::delete_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn start_virtual_machine(
        &self,
        req: Request<StartVirtualMachineRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        blocking("virtual machine start", move || {
            crate::vm_ops::start_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn stop_virtual_machine(
        &self,
        req: Request<StopVirtualMachineRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        blocking("virtual machine stop", move || {
            crate::vm_ops::stop_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn pause_virtual_machine(
        &self,
        req: Request<PauseVirtualMachineRequest>,
    ) -> Result<Response<VirtualMachine>, Status> {
        let req = req.into_inner();
        blocking("virtual machine pause", move || {
            crate::vm_ops::pause_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn resume_virtual_machine(
        &self,
        req: Request<ResumeVirtualMachineRequest>,
    ) -> Result<Response<VirtualMachine>, Status> {
        let req = req.into_inner();
        blocking("virtual machine resume", move || {
            crate::vm_ops::resume_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn create_snapshot(
        &self,
        req: Request<CreateSnapshotRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        blocking("virtual machine snapshot create", move || {
            crate::vm_ops::create_snapshot_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn list_snapshots(
        &self,
        req: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        let req = req.into_inner();
        blocking("virtual machine snapshots", move || {
            crate::vm_ops::list_snapshots_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn restore_snapshot(
        &self,
        req: Request<RestoreSnapshotRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        blocking("virtual machine snapshot restore", move || {
            crate::vm_ops::restore_snapshot_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    async fn delete_snapshot(
        &self,
        req: Request<DeleteSnapshotRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        blocking("virtual machine snapshot delete", move || {
            crate::vm_ops::delete_snapshot_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    type ConsoleStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ConsoleResponse, Status>> + Send + 'static>>;

    async fn console(
        &self,
        _req: Request<tonic::Streaming<ConsoleRequest>>,
    ) -> Result<Response<Self::ConsoleStream>, Status> {
        // A serial console is a live byte pipe into a running guest — the
        // same family as `ContainerService.Exec`, which this socket does not
        // serve at all yet. Named so a caller does not read a generic
        // "service not found": the method exists in the contract, this build
        // has nowhere to plumb it to.
        Err(unserved("VirtualMachineService", "Console"))
    }
}

#[tonic::async_trait]
impl OperationService for NodeApi {
    async fn get_operation(
        &self,
        req: Request<GetOperationRequest>,
    ) -> Result<Response<Operation>, Status> {
        let id = req.into_inner().id;
        blocking("operation", move || {
            crate::operations::get_in(&node::state_root(), &id)
        })
        .await?
        .map(Response::new)
    }

    async fn list_operations(
        &self,
        req: Request<ListOperationsRequest>,
    ) -> Result<Response<ListOperationsResponse>, Status> {
        let req = req.into_inner();
        blocking("operations", move || {
            crate::operations::list_in(&node::state_root(), &req)
        })
        .await?
        .map(Response::new)
    }

    type WatchOperationStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<Operation, Status>> + Send + 'static>>;

    async fn watch_operation(
        &self,
        _req: Request<WatchOperationRequest>,
    ) -> Result<Response<Self::WatchOperationStream>, Status> {
        Err(unserved("OperationService", "WatchOperation"))
    }

    async fn cancel_operation(
        &self,
        _req: Request<CancelOperationRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(unserved("OperationService", "CancelOperation"))
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
        .route_service(
            &format!("/{}/*rest", NetworkServiceServer::<NodeApi>::NAME),
            NetworkServiceServer::new(NodeApi),
        )
        .route_service(
            &format!("/{}/*rest", VolumeServiceServer::<NodeApi>::NAME),
            VolumeServiceServer::new(NodeApi),
        )
        .route_service(
            &format!("/{}/*rest", OperationServiceServer::<NodeApi>::NAME),
            OperationServiceServer::new(NodeApi),
        )
        .route_service(
            &format!("/{}/*rest", VirtualMachineServiceServer::<NodeApi>::NAME),
            VirtualMachineServiceServer::new(NodeApi),
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
    let if_none_match = req
        .headers()
        .get(hyper::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // Read before the body is taken: a borrow of the request held across an
    // await would make this future not `Send`.
    let (idempotency_key, if_match) = {
        let header = |name: &str| {
            req.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        (header("idempotency-key"), header("if-match"))
    };
    let answer = async {
        let body = axum::body::to_bytes(req.into_body(), MAX_BODY)
            .await
            .map_err(|_| {
                Status::invalid_argument(format!(
                    "{}: the request body is larger than {MAX_BODY} bytes",
                    route.rpc
                ))
            })?;
        let mut input = transcode::bind(route, &vars, query.as_deref(), &body)?;
        if MUTATIONS.contains(&route.rpc) {
            from_header(&mut input, "request_id", "Idempotency-Key", idempotency_key)?;
            from_header(
                &mut input,
                "etag",
                "If-Match",
                if_match.map(|v| v.trim_start_matches("W/").trim_matches('"').to_string()),
            )?;
        }
        match route.service {
            "NodeService" => transcode::dispatch_node_service(&NodeApi, route.rpc, input).await,
            "NetworkService" => {
                transcode::dispatch_network_service(&NodeApi, route.rpc, input).await
            }
            "VolumeService" => transcode::dispatch_volume_service(&NodeApi, route.rpc, input).await,
            "OperationService" => {
                transcode::dispatch_operation_service(&NodeApi, route.rpc, input).await
            }
            "VirtualMachineService" => {
                transcode::dispatch_virtual_machine_service(&NodeApi, route.rpc, input).await
            }
            _ => Err(transcode::not_served(route)),
        }
    };
    match answer.await {
        Ok(body) if MUTATIONS.contains(&route.rpc) => acknowledged(with_link_header_of(body)),
        Ok(body) => with_etag(with_link_header_of(body), if_none_match.as_deref()),
        Err(status) => problem(status, &path),
    }
}

/// The RPCs that answer an `Operation` and read the two request headers of
/// ADR-0042 D2: `Idempotency-Key` (the request's `request_id`) and `If-Match`
/// (its `etag`). A header on any other route is not read, and `If-Match` on a
/// request with no `etag` field is refused.
const MUTATIONS: &[&str] = &[
    "CreateNetwork",
    "DeleteNetwork",
    "CreateVirtualMachine",
    "DeleteVirtualMachine",
    "StartVirtualMachine",
    "StopVirtualMachine",
    "CreateSnapshot",
    "RestoreSnapshot",
    "DeleteSnapshot",
];

/// Puts a header's value in the request field it stands for. The same value
/// in both places is fine; two different ones are refused — which of them the
/// server obeyed would be a guess.
fn from_header(
    input: &mut serde_json::Value,
    field: &str,
    header: &str,
    value: Option<String>,
) -> Result<(), Status> {
    let (Some(value), Some(msg)) = (value, input.as_object_mut()) else {
        return Ok(());
    };
    if header == "If-Match" && value == "*" {
        return Ok(());
    }
    match msg.get(field).and_then(|v| v.as_str()) {
        Some(sent) if !sent.is_empty() && sent != value => Err(Status::invalid_argument(format!(
            "the {header} header ('{value}') and the request's {field} ('{sent}') disagree"
        ))),
        _ => {
            msg.insert(field.to_string(), value.into());
            Ok(())
        }
    }
}

/// The answer to a mutation (ADR-0042 D2): the `Operation`, with `Location`
/// naming it. `202 Accepted` while it runs; `200` once it has ended — the
/// body says how.
fn acknowledged(
    (body, mut res): (serde_json::Value, axum::response::Response),
) -> axum::response::Response {
    let running = matches!(
        body.get("state").and_then(|s| s.as_str()),
        Some("OPERATION_STATE_PENDING" | "OPERATION_STATE_RUNNING")
    );
    if running {
        *res.status_mut() = hyper::StatusCode::ACCEPTED;
    }
    if let Some(v) = body
        .get("id")
        .and_then(|i| i.as_str())
        .and_then(|id| hyper::header::HeaderValue::from_str(&format!("/v1/operations/{id}")).ok())
    {
        res.headers_mut().insert(hyper::header::LOCATION, v);
    }
    res
}

/// ADR-0042 D2, concurrency: a resource's `meta.etag` is its `ETag` header,
/// and a `GET` whose `If-None-Match` names that version is `304 Not Modified`
/// with no body. Only the REST routes come through here, and of those only
/// `GET` answers a resource — a mutation answers an `Operation`, which has no
/// `meta`.
fn with_etag(
    (body, mut res): (serde_json::Value, axum::response::Response),
    if_none_match: Option<&str>,
) -> axum::response::Response {
    let Some(etag) = body
        .pointer("/meta/etag")
        .and_then(|v| v.as_str())
        .filter(|e| !e.is_empty())
    else {
        return res;
    };
    let quoted = format!("\"{etag}\"");
    let Ok(value) = hyper::header::HeaderValue::from_str(&quoted) else {
        return res;
    };
    let unchanged = if_none_match.is_some_and(|sent| {
        sent.split(',')
            .map(|t| t.trim().trim_start_matches("W/"))
            .any(|t| t == "*" || t == quoted)
    });
    if unchanged {
        res = hyper::StatusCode::NOT_MODIFIED.into_response();
    }
    res.headers_mut().insert(hyper::header::ETAG, value);
    res
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
        no_route(format!("route {} {path} on the node API", req.method())),
        &path,
    )
}

/// A message's JSON with the RFC 8288 `Link` header mirroring its own `links`
/// (ADR-0042 D2) — read from the body being sent, so the header and the body
/// cannot disagree.
fn with_link_header_of(body: serde_json::Value) -> (serde_json::Value, axum::response::Response) {
    let header = link_header(&body);
    let mut res = (hyper::StatusCode::OK, axum::Json(&body)).into_response();
    if let Some(v) = header.and_then(|h| hyper::header::HeaderValue::from_str(&h).ok()) {
        res.headers_mut().insert(hyper::header::LINK, v);
    }
    (body, res)
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
        no_route(format!(
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

/// No handler for this method and path: `NOT_FOUND` carrying DX-4001, which
/// tells it apart from a resource that does not exist (DX-4000).
fn no_route(msg: String) -> Status {
    let mut status = Status::not_found(msg);
    status.metadata_mut().insert(
        crate::network_ops::DX_METADATA,
        tonic::metadata::MetadataValue::from_static("4001"),
    );
    status
}

/// A failure as the engine's error, with its dictionary number, and the HTTP
/// status the REST encoding answers with. `UNIMPLEMENTED` is 501 — the
/// operation is in the contract and this version does not serve it, and a 503
/// would tell a client to retry (its class's status); everything else follows
/// the class.
pub fn engine_error(status: &Status) -> (delonix_model::Error, hyper::StatusCode) {
    use delonix_model::Error;
    let msg = status.message().to_string();
    // A stale `etag` (`If-Match`): 412, never a silent overwrite.
    if status
        .metadata()
        .contains_key(crate::network_ops::STALE_ETAG_METADATA)
    {
        return (Error::Conflict(msg), hyper::StatusCode::PRECONDITION_FAILED);
    }
    // A failure that came from the engine carries its dictionary number: the
    // document says that code, and the status is its class's.
    if let Some(number) = status
        .metadata()
        .get(crate::network_ops::DX_METADATA)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u16>().ok())
    {
        let inner = match number / 1000 {
            1 | 2 => Error::Invalid(msg),
            3 => Error::NotRunning(msg),
            4 => Error::NotFound(msg),
            5 => Error::Conflict(msg),
            6 => Error::Unavailable(msg),
            7 => Error::PermissionDenied(msg),
            8 => Error::Timeout(msg),
            _ => Error::Runtime {
                context: "node-api",
                message: msg,
            },
        };
        let err = Error::coded(number, inner);
        let http = hyper::StatusCode::from_u16(err.class().http_status())
            .unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR);
        return (err, http);
    }
    match status.code() {
        tonic::Code::AlreadyExists => (Error::Conflict(msg), hyper::StatusCode::CONFLICT),
        tonic::Code::InvalidArgument | tonic::Code::FailedPrecondition => {
            (Error::Invalid(msg), hyper::StatusCode::BAD_REQUEST)
        }
        // A resource that is not there. A PATH that is not there carries
        // DX-4001 in its metadata ([`no_route`]) and was answered above.
        tonic::Code::NotFound => (Error::NotFound(msg), hyper::StatusCode::NOT_FOUND),
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
