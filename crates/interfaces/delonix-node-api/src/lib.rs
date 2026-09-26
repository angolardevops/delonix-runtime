//! **The node API of the Delonix Runtime** — `delonix.node.v1` served as gRPC and
//! HTTP/JSON on one local unix socket (ADR-0040 D4, ADR-0042 D1).
//!
//! What is served today, and it is said here so nobody reads the socket as the
//! whole contract: `NodeService.ListProviders` (ADR-0050 D5) — the same
//! `ProviderInfo` per provider that `delonix provider ls -o json` prints, built
//! from the same declarations and the same host probes. The other `NodeService`
//! RPCs answer `UNIMPLEMENTED` with the ADR step that brings them; every other
//! service of the contract is not registered on this socket at all.
//!
//! Both encodings come from the same `proto/` files: the gRPC stubs and the
//! proto3 JSON (`pbjson`, proto field names — what the published OpenAPI
//! declares). The HTTP route for a `google.api.http` annotation is written by
//! hand for now (`GET /v1/providers`, the only one); a generic transcoder over
//! the annotations is the step after this one, recorded in ADR-0042.
//!
//! `delonix serve node-api` runs the `delonix-node-api` binary, which calls
//! [`serve_blocking`]. The socket is `0600` and every accepted connection is
//! checked with `SO_PEERCRED` against this process's uid — the same discipline
//! as the CRI, the management API and the holder's control socket.

use delonix_model::Error;

/// The generated contract: prost messages, tonic server and client, and the
/// proto3 JSON `Serialize`/`Deserialize` of every message.
pub mod proto {
    pub mod v1 {
        tonic::include_proto!("delonix.node.v1");
        include!(concat!(env!("OUT_DIR"), "/delonix.node.v1.serde.rs"));
    }
}

pub mod providers;
mod service;

pub use service::{list_providers, router, NodeApi};

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
