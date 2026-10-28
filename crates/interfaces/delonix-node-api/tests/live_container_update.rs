//! `UpdateContainer` against a REAL, running container, over gRPC: create +
//! start a disposable busybox with a memory limit, lower that limit with
//! `UpdateContainer`, and confirm the NEW value in the live cgroup's
//! `memory.max` — the resize reached the kernel without the PID changing.
//!
//! `#[ignore]` by default, same harness as `live_container_lifecycle`:
//!
//! ```sh
//! cargo build -p delonix-runtime-bin
//! ROOT=$(mktemp -d)
//! DELONIX_ROOT=$ROOT target/debug/delonix pull busybox:latest
//! DELONIX_ROOT=$ROOT \
//!   DELONIX_BIN=$PWD/target/debug/delonix \
//!   cargo test -p delonix-node-api --test live_container_update -- --ignored --nocapture
//! ```

use std::time::Duration;

use delonix_node_api::proto::v1::container_service_client::ContainerServiceClient;
use delonix_node_api::proto::v1::{
    ContainerPhase, ContainerSpec, CreateContainerRequest, DeleteContainerRequest,
    GetContainerRequest, Resources, UpdateContainerRequest,
};
use delonix_node_api::proto::v1::{Container, ResourceMeta};

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
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the server never created {path}");
}

/// The live cgroup of `pid`, read exactly as `delonix_linux::live_cgroup` does
/// (`/proc/<pid>/cgroup`'s `0::<rel>` line → `/sys/fs/cgroup<rel>`), so the
/// test reads the SAME `memory.max` the resize wrote.
fn memory_max_of(pid: i32) -> String {
    let cg = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .unwrap_or_else(|e| panic!("reading /proc/{pid}/cgroup: {e}"));
    let rel = cg
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .unwrap_or_else(|| panic!("no cgroup v2 line in /proc/{pid}/cgroup: {cg:?}"))
        .trim();
    let path = format!("/sys/fs/cgroup{rel}/memory.max");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {path}: {e}"))
        .trim()
        .to_string()
}

#[tokio::test]
#[ignore = "launches a real container; run with --ignored and DELONIX_BIN + a DELONIX_ROOT holding busybox"]
async fn update_container_lowers_the_live_memory_max() {
    assert!(
        std::env::var_os("DELONIX_BIN").is_some() && std::env::var_os("DELONIX_ROOT").is_some(),
        "needs DELONIX_BIN=<path to delonix> and DELONIX_ROOT=<root with busybox pulled>: \
         run with --ignored"
    );

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
    let mut c = ContainerServiceClient::new(channel);

    let name = "nodeapi-resize-probe";
    let _ = c
        .delete_container(DeleteContainerRequest {
            name: name.into(),
            namespace: "default".into(),
            force: true,
            ..Default::default()
        })
        .await;

    // ---- Create + start a busybox capped at 128 MiB.
    const START: i64 = 128 * 1024 * 1024;
    const LOWERED: i64 = 64 * 1024 * 1024;
    c.create_container(CreateContainerRequest {
        namespace: "default".into(),
        name: name.into(),
        spec: Some(ContainerSpec {
            image: "busybox:latest".into(),
            command: vec!["sh".into(), "-c".into(), "sleep 120".into()],
            resources: Some(Resources {
                memory_limit_bytes: START,
                ..Default::default()
            }),
            ..Default::default()
        }),
        start: true,
        request_id: "resize-probe-1".into(),
        ..Default::default()
    })
    .await
    .expect("CreateContainer over gRPC");

    let got = c
        .get_container(GetContainerRequest {
            name: name.into(),
            namespace: "default".into(),
        })
        .await
        .expect("GetContainer")
        .into_inner();
    let status = got.status.clone().unwrap();
    assert_eq!(
        status.phase,
        ContainerPhase::Running as i32,
        "not running: {got:?}"
    );
    let pid = status.pid;
    assert!(pid > 0, "no pid: {got:?}");

    let before = memory_max_of(pid);
    eprintln!("before → pid={pid} memory.max={before}");
    assert_eq!(
        before,
        START.to_string(),
        "the created limit did not reach the live cgroup"
    );

    // ---- Lower memory_limit to 64 MiB, masked to just that field.
    let updated = c
        .update_container(UpdateContainerRequest {
            namespace: "default".into(),
            name: name.into(),
            container: Some(Container {
                meta: Some(ResourceMeta {
                    name: name.into(),
                    namespace: "default".into(),
                    ..Default::default()
                }),
                spec: Some(ContainerSpec {
                    resources: Some(Resources {
                        memory_limit_bytes: LOWERED,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            update_mask: Some(pbjson_types::FieldMask {
                paths: vec!["spec.resources.memory_limit_bytes".into()],
            }),
        })
        .await
        .expect("UpdateContainer over gRPC")
        .into_inner();
    eprintln!(
        "update → name={} phase={}",
        updated.meta.as_ref().unwrap().name,
        updated.status.as_ref().unwrap().phase
    );
    // The PID did not change — this was a live resize, not a restart.
    assert_eq!(
        updated.status.as_ref().unwrap().pid,
        pid,
        "the pid changed: this was a restart, not a live resize"
    );

    // ---- The kernel now enforces the lowered limit, same live cgroup.
    let after = memory_max_of(pid);
    eprintln!("after → memory.max={after}");
    assert_eq!(
        after,
        LOWERED.to_string(),
        "UpdateContainer did not lower the live memory.max"
    );

    // ---- Cleanup.
    c.delete_container(DeleteContainerRequest {
        name: name.into(),
        namespace: "default".into(),
        force: true,
        ..Default::default()
    })
    .await
    .expect("DeleteContainer");
}
