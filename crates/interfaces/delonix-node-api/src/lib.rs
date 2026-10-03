//! **The node API of the Delonix Runtime** — `delonix.node.v1` served as gRPC and
//! HTTP/JSON on one local unix socket (ADR-0040 D4, ADR-0042 D1).
//!
//! What is served today, and it is said here so nobody reads the socket as the
//! whole contract: `NodeService.ListProviders` (ADR-0050 D5) — the same
//! `ProviderInfo` per provider that `delonix provider ls -o json` prints — the
//! entry point `GetApiRoot` (`GET /v1`: a link to every resource served; each
//! resource carries its own `links`, mirrored in an RFC 8288 `Link` header) and
//! `GetNodeInfo`, `GetHealth` and `GetCapacity` (ADR-0042 step C), computed from
//! the same functions `delonix system info` reads. `GET /openapi.json` serves
//! the published document, and `GET /docs` (Swagger UI) and `GET /redoc`
//! render it from UI files embedded in the binary ([`docs`]). `WatchEvents`
//! answers `UNIMPLEMENTED` with the step that brings it; every other service of
//! the contract is not registered on this socket at all.
//!
//! Errors: gRPC carries `google.rpc.Status`; the REST encoding answers every
//! error with an RFC 9457 `application/problem+json` document built by the
//! engine's `codes::problem` — the same `DX-` codes the CLI exits with — which
//! is what the published OpenAPI declares (ADR-0042 D2).
//!
//! Step E, first wave: `NetworkService.GetNetwork` and `ListNetworks`
//! ([`networks`]) — the resource's `etag` is the REST `ETag` (a matching
//! `If-None-Match` is 304), a list takes `label_selector` ([`selector`]) and
//! pages by name, with the next page as a link.
//!
//! Step E, the first mutations: `CreateNetwork` and `DeleteNetwork`
//! ([`network_ops`]) answer an `Operation` persisted under the state root
//! before the work starts ([`operations`]), read back with
//! `OperationService.GetOperation` and `ListOperations`. `request_id` (REST:
//! `Idempotency-Key`) makes a retry the first answer; `etag` (REST:
//! `If-Match`) makes a stale delete 412.
//!
//! The REST routes are not written by hand: `build.rs` generates the route
//! table and the dispatchers from the `google.api.http` annotations
//! ([`transcode`]), for every RPC of the contract. A route of a service this
//! engine does not serve yet answers 501 (DX-6001), a path the contract does
//! not have 404 (DX-4001).
//!
//! Both encodings come from the same `proto/` files: the gRPC stubs and the
//! proto3 JSON (`pbjson`, proto field names — what the published OpenAPI
//! declares). The HTTP route for a `google.api.http` annotation is written by
//! hand for now (the four GET routes of `NodeService`); a generic transcoder
//! over the annotations is a later slice of ADR-0042 step C.
//!
//! `delonix serve node-api` runs the `delonix-node-api` binary, which calls
//! [`serve_blocking`]. The socket is `0600` and every accepted connection is
//! checked with `SO_PEERCRED` against this process's uid — the same discipline
//! as the CRI, the management API and the holder's control socket.

// A `tonic::Status` is large by nature and it is the error every service
// method returns; the CRI crate silences this lint for the same reason.
#![allow(clippy::result_large_err)]

use delonix_model::Error;

/// The generated contract: prost messages, tonic server and client, and the
/// proto3 JSON `Serialize`/`Deserialize` of every message.
pub mod proto {
    pub mod v1 {
        tonic::include_proto!("delonix.node.v1");
        include!(concat!(env!("OUT_DIR"), "/delonix.node.v1.serde.rs"));
    }
}

pub mod docs;
pub mod network_ops;
pub mod networks;
pub mod node;
pub mod operations;
pub mod providers;
pub mod selector;
mod service;
pub mod transcode;

pub use service::{
    capacity, health, link_header, list_providers, node_info, openapi_document, router, NodeApi,
    OPENAPI_YAML,
};

/// Serves the node API on a unix socket, blocking the calling thread. `addr` is a
/// path or `unix:///path`. Same pattern as `delonix_mgmt::serve_blocking`.
pub fn serve_blocking(addr: &str) -> Result<(), Error> {
    let path = addr.strip_prefix("unix://").unwrap_or(addr).to_string();
    delonix_node::alloc_tuning::limit_malloc_arenas();
    let rt = tokio::runtime::Builder::new_multi_thread()
        // A local control-plane API: a couple of workers, the same reasoning as
        // the CRI and the management API.
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| Error::Runtime {
            context: "tokio",
            message: e.to_string(),
        })?;
    rt.block_on(async move {
        let _ = std::fs::remove_file(&path); // an old socket of a previous run
        let uds = tokio::net::UnixListener::bind(&path).map_err(|e| Error::Runtime {
            context: "bind",
            message: e.to_string(),
        })?;
        // 0600 on the file AND `SO_PEERCRED` per connection (below): the file
        // mode depends on the ambient umask at bind time, the credential check
        // does not.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        tracing::info!(socket = %path, "delonix-node-api (node API) listening");
        serve_over_uds(uds, router()).await
    })
}

/// Serves the router over a `UnixListener`: accept loop + hyper-util's `auto`
/// connection builder, which speaks HTTP/1.1 for the JSON routes and HTTP/2
/// (prior knowledge, no TLS on a local socket) for gRPC on the same listener.
async fn serve_over_uds(uds: tokio::net::UnixListener, app: axum::Router) -> Result<(), Error> {
    use delonix_node::peer_cred::peer_uid;
    use hyper::body::Incoming;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tower::Service;
    // SAFETY: geteuid() has no preconditions.
    let own_uid = unsafe { libc::geteuid() };
    let mut make = app.into_make_service();
    loop {
        // Transient accept errors (EMFILE, ECONNABORTED) are retried, never
        // fatal — see the management API for the incident that taught it.
        let (socket, _) = match uds.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, "accept() failed, retrying");
                continue;
            }
        };
        if peer_uid(&socket) != Some(own_uid) {
            continue;
        }
        let tower_service = match make.call(&socket).await {
            Ok(svc) => svc,
            Err(never) => match never {},
        };
        tokio::spawn(async move {
            let io = TokioIo::new(socket);
            let hyper_service = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                tower_service.clone().call(req)
            });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, hyper_service)
                .await;
        });
    }
}
