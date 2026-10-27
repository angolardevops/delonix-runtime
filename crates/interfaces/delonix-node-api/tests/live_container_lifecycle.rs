//! The container lifecycle through the REAL transports, end to end, against a
//! REAL container: `CreateContainer` → `StartContainer` → `Stats` → `Logs -f`
//! → `StopContainer` → `WaitContainer`, all over gRPC on the unix socket the
//! `delonix serve node-api` server runs.
//!
//! `#[ignore]` by default — it launches a real workload, so it runs only on a
//! host that can (rootless containers), and on demand:
//!
//! ```sh
//! cargo build -p delonix-runtime-bin            # the `delonix` the launch re-execs
//! ROOT=$(mktemp -d)
//! DELONIX_ROOT=$ROOT target/debug/delonix pull busybox:latest
//! DELONIX_ROOT=$ROOT \
//!   DELONIX_CLI_BIN=$PWD/target/debug/delonix \
//!   cargo test -p delonix-node-api --test live_container_lifecycle -- --ignored --nocapture
//! ```
//!
//! `Start` re-execs `$DELONIX_CLI_BIN` (the `delonix` CLI), so that variable and
//! a `DELONIX_ROOT` with `busybox:latest` already pulled are required; without
//! them the test skips with a message rather than fail.

use std::time::Duration;

use delonix_node_api::proto::v1::container_service_client::ContainerServiceClient;
use delonix_node_api::proto::v1::{
    ContainerPhase, ContainerSpec, CreateContainerRequest, DeleteContainerRequest,
    GetContainerRequest, LogsRequest, OperationState, StartContainerRequest, StatsRequest,
    StopContainerRequest, WaitContainerRequest,
};

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

#[tokio::test]
#[ignore = "launches a real container; run with --ignored and DELONIX_CLI_BIN + a DELONIX_ROOT holding busybox"]
async fn the_container_lifecycle_runs_end_to_end_over_grpc() {
    // The launch re-execs the CLI, and the server/CLI/image store all read
    // DELONIX_ROOT. Both must be set for a real run; otherwise skip.
    if std::env::var_os("DELONIX_CLI_BIN").is_none() || std::env::var_os("DELONIX_ROOT").is_none() {
        eprintln!(
            "SKIP: set DELONIX_CLI_BIN=<path to delonix> and DELONIX_ROOT=<root with busybox \
             pulled> to run this live test"
        );
        return;
    }

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

    let name = "nodeapi-live-probe";
    // Best effort: a leftover from an earlier run must not fail the create.
    let _ = c
        .delete_container(DeleteContainerRequest {
            name: name.into(),
            namespace: "default".into(),
            force: true,
            ..Default::default()
        })
        .await;

    // ---- Create (record only): a disposable busybox that logs every second.
    let op = c
        .create_container(CreateContainerRequest {
            namespace: "default".into(),
            name: name.into(),
            spec: Some(ContainerSpec {
                image: "busybox:latest".into(),
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    "i=0; while true; do i=$((i+1)); echo tick $i; sleep 1; done".into(),
                ],
                ..Default::default()
            }),
            start: false,
            request_id: "live-probe-2".into(),
            ..Default::default()
        })
        .await
        .expect("CreateContainer over gRPC")
        .into_inner();
    eprintln!("create → operation {} state={}", op.id, op.state);
    assert_eq!(op.verb, "create");
    assert_eq!(
        op.state,
        OperationState::Succeeded as i32,
        "create operation did not succeed: {op:?}"
    );

    // ---- Start: the record is launched; the RPC answers with it Running.
    let started = c
        .start_container(StartContainerRequest {
            name: name.into(),
            namespace: "default".into(),
        })
        .await
        .expect("StartContainer over gRPC")
        .into_inner();
    eprintln!("start → phase={}", started.status.as_ref().unwrap().phase);
    assert_eq!(
        started.status.as_ref().unwrap().phase,
        ContainerPhase::Running as i32,
        "StartContainer did not return a running container: {started:?}"
    );

    // ---- Get: it is Running, with a pid.
    let got = c
        .get_container(GetContainerRequest {
            name: name.into(),
            namespace: "default".into(),
        })
        .await
        .expect("GetContainer")
        .into_inner();
    let status = got.status.clone().unwrap();
    eprintln!("get → phase={} pid={}", status.phase, status.pid);
    assert_eq!(
        status.phase,
        ContainerPhase::Running as i32,
        "not running: {got:?}"
    );
    assert!(status.pid > 0, "no pid: {got:?}");

    // ---- Stats: the live cgroup counters.
    let stats = c
        .stats(StatsRequest {
            name: name.into(),
            namespace: "default".into(),
        })
        .await
        .expect("Stats")
        .into_inner();
    eprintln!(
        "stats → cpu_ns={} mem_ws={} unmeasured={:?}",
        stats.cpu_usage_nanos, stats.memory_working_set_bytes, stats.unmeasured
    );

    // ---- Logs (follow): read a couple of chunks, which must carry the ticks.
    let mut logs = c
        .logs(LogsRequest {
            container: name.into(),
            namespace: "default".into(),
            follow: true,
            ..Default::default()
        })
        .await
        .expect("Logs")
        .into_inner();
    let mut seen = String::new();
    for _ in 0..3 {
        match tokio::time::timeout(Duration::from_secs(8), logs.message()).await {
            Ok(Ok(Some(chunk))) => {
                let text = String::from_utf8_lossy(&chunk.data);
                eprintln!("logs → {}", text.trim_end());
                seen.push_str(&text);
                if seen.contains("tick") {
                    break;
                }
            }
            other => {
                eprintln!("logs stream ended early: {other:?}");
                break;
            }
        }
    }
    drop(logs);
    assert!(
        seen.contains("tick"),
        "the logs did not carry the container's output: {seen:?}"
    );

    // ---- Stop: graceful, then the record says it is no longer running.
    let stopped = c
        .stop_container(StopContainerRequest {
            name: name.into(),
            namespace: "default".into(),
            grace_period: Some(pbjson_types::Duration {
                seconds: 3,
                nanos: 0,
            }),
        })
        .await
        .expect("StopContainer")
        .into_inner();
    let phase = stopped.status.as_ref().unwrap().phase;
    eprintln!("stop → phase={phase}");
    assert_ne!(
        phase,
        ContainerPhase::Running as i32,
        "still running after stop: {stopped:?}"
    );

    // ---- Wait: a stopped container answers at once with its exit result.
    let waited = c
        .wait_container(WaitContainerRequest {
            name: name.into(),
            namespace: "default".into(),
        })
        .await
        .expect("WaitContainer")
        .into_inner();
    eprintln!(
        "wait → exit_code={:?} reason={}",
        waited.exit_code, waited.reason
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
