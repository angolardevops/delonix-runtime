//! `VirtualMachineService` through the real transports: gRPC over the unix
//! socket with the generated client, and HTTP/JSON through the same router —
//! the server `delonix serve node-api` runs, not a test double (same
//! discipline as `grpc_list_providers.rs`).
//!
//! **Deliberately never creates a VM here.** Every request this process
//! serves reads `node::state_root()` — this HOST's real state root, shared
//! with whatever else runs `delonix` on it — and these tests run in the same
//! binary as `grpc_list_providers.rs`'s, which may run concurrently. Every
//! case below is chosen to be refused or answered `NotFound` BEFORE anything
//! would be written, so none of them can touch real state no matter which
//! root the process resolves. The create/get/list/delete/start/stop/
//! pause/resume/snapshot LOGIC itself is exercised against an isolated
//! `tempdir()` and a fake backend in `vms.rs`/`vm_ops.rs`'s own unit tests;
//! what only this file can prove is that the wire — proto codegen, the
//! custom-verb REST templates (`:start`/`:stop`/`:pause`/`:resume`), and the
//! nested snapshot paths — is actually reachable.

use delonix_node_api::proto::v1::virtual_machine_service_client::VirtualMachineServiceClient;
use delonix_node_api::proto::v1::{
    CreateSnapshotRequest, CreateVirtualMachineRequest, DeleteSnapshotRequest,
    DeleteVirtualMachineRequest, GetVirtualMachineRequest, ListSnapshotsRequest,
    ListVirtualMachinesRequest, PauseVirtualMachineRequest, RestoreSnapshotRequest,
    ResumeVirtualMachineRequest, StartVirtualMachineRequest, StopVirtualMachineRequest,
    VirtualMachineSpec,
};

/// A SHORT socket path, same reasoning as `grpc_list_providers.rs`'s.
fn short_sock() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir_in("/tmp").expect("a directory under /tmp");
    let sock = dir.path().join("x.sock").to_str().unwrap().to_owned();
    (dir, sock)
}

async fn wait_for(path: &str) {
    for _ in 0..200 {
        if std::path::Path::new(path).exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the server never created {path}");
}

/// A name no test in this workspace would ever create for real, so a NotFound
/// here can only mean the request reached the real handler and found nothing
/// — never a false negative from a VM another test happened to leave behind.
const NOWHERE: &str = "vmaas-audit-wire-test-does-not-exist";

#[tokio::test]
async fn virtual_machine_service_answers_over_grpc_without_touching_any_state() {
    let (_sock_dir, sock) = short_sock();
    let s = sock.clone();
    std::thread::spawn(move || {
        let _ = delonix_node_api::serve_blocking(&format!("unix://{s}"));
    });
    wait_for(&sock).await;

    let s2 = sock.clone();
    let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_| {
            let p = s2.clone();
            async move {
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(p).await?,
                ))
            }
        }))
        .await
        .expect("connect to the server's unix socket");
    let mut vms = VirtualMachineServiceClient::new(channel);

    // Get/list: safe reads, never write.
    let missing = vms
        .get_virtual_machine(GetVirtualMachineRequest {
            name: NOWHERE.into(),
            namespace: String::new(),
        })
        .await
        .expect_err("no such virtual machine");
    assert_eq!(missing.code(), tonic::Code::NotFound);

    let listed = vms
        .list_virtual_machines(ListVirtualMachinesRequest {
            namespace: "vmaas-audit-wire-test-namespace".into(),
            ..Default::default()
        })
        .await
        .expect("ListVirtualMachines")
        .into_inner();
    assert!(listed.virtual_machines.is_empty());

    // Create: refused for an invalid spec before anything is acknowledged —
    // true whatever this process's real state root holds.
    let bad_create = vms
        .create_virtual_machine(CreateVirtualMachineRequest {
            name: NOWHERE.into(),
            spec: Some(VirtualMachineSpec::default()), // image is empty
            ..Default::default()
        })
        .await
        .expect_err("spec.image is required");
    assert_eq!(bad_create.code(), tonic::Code::InvalidArgument);

    // Delete/start/stop of a name that does not exist: NotFound, before any
    // operation is written.
    let del = vms
        .delete_virtual_machine(DeleteVirtualMachineRequest {
            name: NOWHERE.into(),
            ..Default::default()
        })
        .await
        .expect_err("no such virtual machine");
    assert_eq!(del.code(), tonic::Code::NotFound);
    let start = vms
        .start_virtual_machine(StartVirtualMachineRequest {
            name: NOWHERE.into(),
            ..Default::default()
        })
        .await
        .expect_err("no such virtual machine");
    assert_eq!(start.code(), tonic::Code::NotFound);
    let stop = vms
        .stop_virtual_machine(StopVirtualMachineRequest {
            name: NOWHERE.into(),
            ..Default::default()
        })
        .await
        .expect_err("no such virtual machine");
    assert_eq!(stop.code(), tonic::Code::NotFound);

    // Pause/resume: synchronous (answer `VirtualMachine`, not `Operation`) —
    // still NotFound for a name that does not exist, never a default value.
    let pause = vms
        .pause_virtual_machine(PauseVirtualMachineRequest {
            name: NOWHERE.into(),
            ..Default::default()
        })
        .await
        .expect_err("no such virtual machine");
    assert_eq!(pause.code(), tonic::Code::NotFound);
    let resume = vms
        .resume_virtual_machine(ResumeVirtualMachineRequest {
            name: NOWHERE.into(),
            ..Default::default()
        })
        .await
        .expect_err("no such virtual machine");
    assert_eq!(resume.code(), tonic::Code::NotFound);

    // Snapshot lifecycle: every verb refuses on the OWNING VM before the
    // snapshot name is even considered.
    let snap_list = vms
        .list_snapshots(ListSnapshotsRequest {
            virtual_machine: NOWHERE.into(),
            namespace: String::new(),
        })
        .await
        .expect_err("no such virtual machine");
    assert_eq!(snap_list.code(), tonic::Code::NotFound);
    let snap_create = vms
        .create_snapshot(CreateSnapshotRequest {
            virtual_machine: NOWHERE.into(),
            snapshot: "s1".into(),
            ..Default::default()
        })
        .await
        .expect_err("no such virtual machine");
    assert_eq!(snap_create.code(), tonic::Code::NotFound);
    let snap_restore = vms
        .restore_snapshot(RestoreSnapshotRequest {
            virtual_machine: NOWHERE.into(),
            snapshot: "s1".into(),
            ..Default::default()
        })
        .await
        .expect_err("no such virtual machine");
    assert_eq!(snap_restore.code(), tonic::Code::NotFound);
    let snap_delete = vms
        .delete_snapshot(DeleteSnapshotRequest {
            virtual_machine: NOWHERE.into(),
            snapshot: "s1".into(),
            ..Default::default()
        })
        .await
        .expect_err("no such virtual machine");
    assert_eq!(snap_delete.code(), tonic::Code::NotFound);
}

/// The same refusals, over the REST/JSON encoding — proving the custom-verb
/// path templates (`:start`/`:stop`/`:pause`/`:resume`) and the nested
/// snapshot paths actually resolve to the right RPC, not just that the gRPC
/// trait compiles.
#[tokio::test]
async fn virtual_machine_service_routes_resolve_over_rest() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let app = delonix_node_api::router();
    let call = |method: &'static str, path: String, body: &'static str| {
        let app = app.clone();
        async move {
            let res = app
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(path)
                        .body(axum::body::Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = res.status().as_u16();
            let body = res.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        }
    };

    let (status, v) = call(
        "GET",
        format!("/v1/namespaces/default/virtualmachines/{NOWHERE}"),
        "",
    )
    .await;
    assert_eq!(status, 404, "{v}");
    assert_eq!(v["dx"], "DX-4000", "{v}");

    let (status, v) = call(
        "GET",
        "/v1/namespaces/vmaas-audit-wire-test-namespace/virtualmachines".into(),
        "",
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert!(v["virtual_machines"]
        .as_array()
        .is_none_or(|a| a.is_empty()));

    let (status, v) = call(
        "POST",
        "/v1/namespaces/default/virtualmachines".into(),
        r#"{"name":"vmaas-audit-wire-test"}"#,
    )
    .await;
    assert_eq!(status, 400, "a create with no spec.image: {v}");
    assert_eq!(v["grpc_status"], 3, "{v}");

    for (method, path, body) in [
        (
            "POST",
            format!("/v1/namespaces/default/virtualmachines/{NOWHERE}:start"),
            "{}",
        ),
        (
            "POST",
            format!("/v1/namespaces/default/virtualmachines/{NOWHERE}:stop"),
            "{}",
        ),
        (
            "POST",
            format!("/v1/namespaces/default/virtualmachines/{NOWHERE}:pause"),
            "{}",
        ),
        (
            "POST",
            format!("/v1/namespaces/default/virtualmachines/{NOWHERE}:resume"),
            "{}",
        ),
        (
            "DELETE",
            format!("/v1/namespaces/default/virtualmachines/{NOWHERE}"),
            "",
        ),
        (
            "GET",
            format!("/v1/namespaces/default/virtualmachines/{NOWHERE}/snapshots"),
            "",
        ),
        (
            "POST",
            format!("/v1/namespaces/default/virtualmachines/{NOWHERE}/snapshots"),
            r#"{"snapshot":"s1"}"#,
        ),
        (
            "POST",
            format!("/v1/namespaces/default/virtualmachines/{NOWHERE}/snapshots/s1:restore"),
            "{}",
        ),
        (
            "DELETE",
            format!("/v1/namespaces/default/virtualmachines/{NOWHERE}/snapshots/s1"),
            "",
        ),
    ] {
        let (status, v) = call(method, path.clone(), body).await;
        assert_eq!(status, 404, "{method} {path}: {v}");
        assert_eq!(v["dx"], "DX-4000", "{method} {path}: {v}");
    }
}

/// `VirtualMachineService.Console` is in the contract and not served yet —
/// the same "unserved RPC of a served service" answer every other
/// not-yet-wired verb in this socket gives (`ContainerService.Exec`'s
/// sibling, `NetworkService.ConnectContainer`'s).
#[tokio::test]
async fn console_is_not_served_yet() {
    let (_sock_dir, sock) = short_sock();
    let s = sock.clone();
    std::thread::spawn(move || {
        let _ = delonix_node_api::serve_blocking(&format!("unix://{s}"));
    });
    wait_for(&sock).await;
    let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_| {
            let p = sock.clone();
            async move {
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(p).await?,
                ))
            }
        }))
        .await
        .expect("connect to the server's unix socket");
    let mut vms = VirtualMachineServiceClient::new(channel);
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(tx); // an empty stream is enough to reach the handler
    let err = vms
        .console(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .expect_err("Console is not served yet");
    assert_eq!(err.code(), tonic::Code::Unimplemented);
}
