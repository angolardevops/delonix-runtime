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
//! streams the engine's event log ([`events`]); every service of the
//! contract this socket does not implement at all (`ContainerService`,
//! `PodService`) is not registered either.
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
//! `VolumeService` ([`volumes`] reads, [`volume_ops`] writes): `CreateVolume`
//! of a local volume (quota, alert, labels) and `DeleteVolume` of a local
//! volume or a share's record, on the same ledger; a remote share, a share,
//! provisioning, a pool volume and `destroy_remote` answer `UNIMPLEMENTED`
//! with the CLI command that does them. `OperationService.WatchOperation`
//! streams an operation until it ends (gRPC only: the REST encoding does not
//! carry streams), and `CancelOperation` answers `FAILED_PRECONDITION`
//! (DX-5002) — no served mutation has a cancellation point, and an operation
//! is never reported cancelled for work that ran.
//!
//! `VirtualMachineService` ([`vms`] reads, [`vm_ops`] writes), full: create/
//! get/list/delete, start/stop (the same `Operation` ledger as networks),
//! the synchronous pause/resume, and the snapshot lifecycle. `Console` (a
//! live byte pipe into the guest) is `UNIMPLEMENTED`, the same family as
//! `ContainerService.Exec`, which this socket does not serve at all yet.
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

pub mod activation;
pub mod container_exec;
pub mod container_launch;
pub mod container_logs;
pub mod container_ops;
pub mod container_update;
pub mod containers;
pub mod docs;
pub mod events;
pub mod image_ops;
pub mod images;
pub mod network_ops;
pub mod networks;
pub mod pods;
pub mod stacks;
pub mod node;
pub mod operations;
pub mod page;
pub mod providers;
pub mod selector;
mod service;
pub mod transcode;
pub mod vm_ops;
pub mod vms;
pub mod volume_ops;
pub mod volumes;

pub use service::{
    capacity, health, link_header, list_providers, node_info, openapi_document, router, NodeApi,
    OPENAPI_YAML,
};

/// The node API's composition root: the ports this process registers before it
/// serves anything, once.
///
/// Today two: the node's VM network ([`delonix_sdn::vm_network::HostVmNetwork`])
/// and the cgroup leaf provider ([`delonix_linux::HostProcessLeaves`]), the same
/// registrations the CLI's `main` makes. Without it a
/// `CreateVirtualMachine` that names a network attachment on Cloud Hypervisor
/// reached the backend and failed with «no VM network provider is registered
/// in this process» (plan 68 item N3): `delonix-vm` seeds its own backends, but
/// the network needs a state root, which only the composition root knows.
/// Idempotent; the first registration in a process wins.
pub fn compose() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        delonix_vm::set_network(Box::new(delonix_sdn::vm_network::HostVmNetwork {
            state_root: node::state_root(),
        }));
        // The cgroup leaf a Cloud Hypervisor VMM starts in, with the VM's
        // ceilings — the same registration the CLI's `main` makes. Without it a
        // `CreateVirtualMachine` on Cloud Hypervisor is refused (DX-6515).
        delonix_vm::set_process_leaves(Box::new(delonix_linux::HostProcessLeaves));
    });
}

/// Where the server listens.
#[derive(Debug, Clone)]
pub enum Listen {
    /// A socket this process binds: a path or `unix:///path`.
    Path(String),
    /// A listening socket the service manager passed (`LISTEN_FDS`), already
    /// bound, with its mode set by the `.socket` unit ([`activation`]).
    Inherited(std::os::fd::RawFd),
}

/// Serves the node API on a unix socket, blocking the calling thread. `addr` is a
/// path or `unix:///path`. Same pattern as `delonix_mgmt::serve_blocking`.
pub fn serve_blocking(addr: &str) -> Result<(), Error> {
    serve(Listen::Path(addr.to_string()), None)
}

/// Serves the node API, blocking the calling thread, until `idle` (if given)
/// finds it idle — then returns `Ok`, which a socket-activated service reports
/// as a clean exit (ADR-0040 P5). With `idle` `None`, or a zero timeout, it
/// serves until killed.
pub fn serve(listen: Listen, idle: Option<activation::IdleExit>) -> Result<(), Error> {
    compose();
    delonix_node::alloc_tuning::limit_malloc_arenas();
    // An inherited descriptor is checked and taken before any thread exists.
    let inherited = match &listen {
        Listen::Inherited(fd) => {
            activation::check_listening_unix_stream(*fd)?;
            activation::set_cloexec(*fd)?;
            use std::os::fd::FromRawFd;
            // SAFETY: `fd` was passed by the service manager for this process
            // alone (`LISTEN_PID`), was just verified to be a listening unix
            // socket, and nothing else in this process owns it.
            let l = unsafe { std::os::unix::net::UnixListener::from_raw_fd(*fd) };
            l.set_nonblocking(true).map_err(|e| Error::Runtime {
                context: "inherited socket",
                message: e.to_string(),
            })?;
            Some(l)
        }
        Listen::Path(_) => None,
    };
    let idle = idle.filter(|i| !i.timeout.is_zero());
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
        let uds = match (listen, inherited) {
            (_, Some(l)) => {
                let uds = tokio::net::UnixListener::from_std(l).map_err(|e| Error::Runtime {
                    context: "inherited socket",
                    message: e.to_string(),
                })?;
                tracing::info!(
                    socket = ?uds.local_addr().ok().and_then(|a| a.as_pathname().map(|p| p.display().to_string())),
                    idle_timeout = ?idle.as_ref().map(|i| i.timeout),
                    "delonix-node-api (node API) serving a socket-activated listener"
                );
                uds
            }
            (Listen::Path(addr), None) => {
                let path = addr.strip_prefix("unix://").unwrap_or(&addr).to_string();
                let _ = std::fs::remove_file(&path); // an old socket of a previous run
                let uds = tokio::net::UnixListener::bind(&path).map_err(|e| Error::Runtime {
                    context: "bind",
                    message: e.to_string(),
                })?;
                // 0600 on the file AND `SO_PEERCRED` per connection (below): the
                // file mode depends on the ambient umask at bind time, the
                // credential check does not.
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                tracing::info!(socket = %path, "delonix-node-api (node API) listening");
                uds
            }
            (Listen::Inherited(_), None) => unreachable!("an inherited listener is taken above"),
        };
        serve_listener(uds, router(), idle).await
    })
}

/// Serves the router over a `UnixListener`: accept loop + hyper-util's `auto`
/// connection builder, which speaks HTTP/1.1 for the JSON routes and HTTP/2
/// (prior knowledge, no TLS on a local socket) for gRPC on the same listener.
///
/// With `idle`, returns `Ok` once no connection has been open for its timeout
/// and no operation of this process is `RUNNING` ([`activation`]). Every
/// accepted connection, open or closed, restarts the clock.
pub async fn serve_listener(
    uds: tokio::net::UnixListener,
    app: axum::Router,
    idle: Option<activation::IdleExit>,
) -> Result<(), Error> {
    use delonix_node::peer_cred::peer_uid;
    use hyper::body::Incoming;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tower::Service;
    // SAFETY: geteuid() has no preconditions.
    let own_uid = unsafe { libc::geteuid() };
    let mut make = app.into_make_service();
    let open = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(Mutex::new(std::time::Instant::now()));
    // How often the idle condition is looked at: a fraction of the timeout,
    // never busier than 50 ms nor lazier than 1 s.
    let tick = idle.as_ref().map(|i| {
        (i.timeout / 4).clamp(
            std::time::Duration::from_millis(50),
            std::time::Duration::from_secs(1),
        )
    });
    loop {
        let accepted = match (&idle, tick) {
            (Some(i), Some(tick)) => {
                tokio::select! {
                    r = uds.accept() => r,
                    _ = tokio::time::sleep(tick) => {
                        let quiet = last.lock().map(|t| t.elapsed()).unwrap_or_default();
                        if open.load(Ordering::SeqCst) == 0
                            && quiet >= i.timeout
                            && !operations::running_owned_by(&i.root, std::process::id() as i32)
                        {
                            tracing::info!(idle = ?quiet, "delonix-node-api idle, exiting");
                            return Ok(());
                        }
                        continue;
                    }
                }
            }
            _ => uds.accept().await,
        };
        // Transient accept errors (EMFILE, ECONNABORTED) are retried, never
        // fatal — see the management API for the incident that taught it.
        let (socket, _) = match accepted {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, "accept() failed, retrying");
                continue;
            }
        };
        if let Ok(mut t) = last.lock() {
            *t = std::time::Instant::now();
        }
        if peer_uid(&socket) != Some(own_uid) {
            continue;
        }
        let tower_service = match make.call(&socket).await {
            Ok(svc) => svc,
            Err(never) => match never {},
        };
        open.fetch_add(1, Ordering::SeqCst);
        let (open, last) = (open.clone(), last.clone());
        tokio::spawn(async move {
            let io = TokioIo::new(socket);
            let hyper_service = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                tower_service.clone().call(req)
            });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, hyper_service)
                .await;
            // The clock restarts when the LAST connection closes, not when it
            // opened: a long stream ending is activity.
            if let Ok(mut t) = last.lock() {
                *t = std::time::Instant::now();
            }
            open.fetch_sub(1, Ordering::SeqCst);
        });
    }
}
