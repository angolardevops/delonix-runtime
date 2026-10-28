//! A pod whose two members share a NAMED volume, through the REAL node-api
//! transports end to end. `CreatePod` serializes each member's docker-style
//! `spec.mounts` into the `kind: Pod` manifest's pod-level `volumes[]` and the
//! member `volumeMounts[]`; the engine launches both members onto the one
//! shared volume; and a file the first member writes is read by the second —
//! proving the mounts are carried, not dropped (they used to be refused).
//!
//! `#[ignore]` by default — it launches a real workload, so it runs only on a
//! host that can (rootless containers), and on demand:
//!
//! ```sh
//! cargo build -p delonix-runtime-bin            # the `delonix` the launch re-execs
//! ROOT=$(mktemp -d)
//! DELONIX_ROOT=$ROOT target/debug/delonix pull busybox:1.36
//! DELONIX_ROOT=$ROOT \
//!   DELONIX_CLI_BIN=$PWD/target/debug/delonix \
//!   cargo test -p delonix-node-api --test live_pod_mounts -- --ignored --nocapture
//! ```
//!
//! `CreatePod` re-execs `$DELONIX_CLI_BIN` (the `delonix` CLI), so that variable
//! and a `DELONIX_ROOT` with `busybox:1.36` already pulled are required; without
//! them the test skips with a message rather than fail.

use std::time::Duration;

use delonix_node_api::proto::v1::container_service_client::ContainerServiceClient;
use delonix_node_api::proto::v1::pod_service_client::PodServiceClient;
use delonix_node_api::proto::v1::{
    mount, ContainerSpec, CreatePodRequest, DeletePodRequest, Mount, NamedContainer,
    OperationState, PodSpec,
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

async fn connect(sock: &str) -> tonic::transport::Channel {
    let s = sock.to_owned();
    tonic::transport::Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_| {
            let p = s.clone();
            async move {
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(p).await?,
                ))
            }
        }))
        .await
        .expect("connect to the server's unix socket")
}

#[tokio::test]
#[ignore = "launches a real pod; run with --ignored and DELONIX_CLI_BIN + a DELONIX_ROOT holding busybox:1.36"]
async fn a_pod_shares_a_named_volume_between_its_members_over_grpc() {
    if std::env::var_os("DELONIX_CLI_BIN").is_none() || std::env::var_os("DELONIX_ROOT").is_none() {
        eprintln!(
            "SKIP: set DELONIX_CLI_BIN=<path to delonix> and DELONIX_ROOT=<root with busybox:1.36 \
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

    let mut pods = PodServiceClient::new(connect(&sock).await);
    let mut containers = ContainerServiceClient::new(connect(&sock).await);

    let pod = "nodeapi-pod-mounts";
    // Best effort: a leftover from an earlier run must not fail the create.
    let _ = pods
        .delete_pod(DeletePodRequest {
            name: pod.into(),
            namespace: "default".into(),
            force: true,
            ..Default::default()
        })
        .await;

    // Two members of ONE pod mount the SAME named volume at DIFFERENT paths:
    // `writer` drops a sentinel into it at start-up; `reader` just stays alive.
    // If the mounts are serialized, both resolve to the one `nodeapi-pod-share`
    // volume and the sentinel `writer` wrote is visible under `reader`'s path.
    let payload = "shared-payload-OK";
    let member = |name: &str, cmd: &str, target: &str| NamedContainer {
        name: name.into(),
        spec: Some(ContainerSpec {
            image: "busybox:1.36".into(),
            entrypoint: vec!["sh".into(), "-c".into()],
            command: vec![cmd.into()],
            mounts: vec![Mount {
                source: Some(mount::Source::Volume("nodeapi-pod-share".into())),
                target: target.into(),
                read_only: false,
            }],
            ..Default::default()
        }),
    };

    let op = pods
        .create_pod(CreatePodRequest {
            namespace: "default".into(),
            name: pod.into(),
            spec: Some(PodSpec {
                containers: vec![
                    member(
                        "writer",
                        &format!("echo {payload} > /data/msg; sleep 3600"),
                        "/data",
                    ),
                    member("reader", "sleep 3600", "/mnt/shared"),
                ],
                ..Default::default()
            }),
            request_id: "live-pod-mounts-1".into(),
            ..Default::default()
        })
        .await
        .expect("CreatePod over gRPC")
        .into_inner();
    eprintln!("create → operation {} state={}", op.id, op.state);
    assert_eq!(op.verb, "create");
    assert_eq!(
        op.state,
        OperationState::Succeeded as i32,
        "the pod create operation did not succeed: {op:?}"
    );

    // Read the sentinel from the OTHER member, at its OWN mount path. The two
    // members start together, so `writer`'s `echo` may not have landed the
    // instant `reader` is up: retry the read a few times before giving up.
    // The member container is named `<pod>-<member>`.
    use delonix_node_api::proto::v1::{exec_request, exec_response, ExecRequest, ExecStart};
    let mut saw = String::new();
    for attempt in 0..30 {
        let start = ExecRequest {
            frame: Some(exec_request::Frame::Start(ExecStart {
                container: format!("{pod}-reader"),
                namespace: "default".into(),
                command: vec!["cat".into(), "/mnt/shared/msg".into()],
                ..Default::default()
            })),
        };
        let mut resp = containers
            .exec(tokio_stream::iter(vec![start]))
            .await
            .expect("Exec into the reader member over gRPC")
            .into_inner();
        let mut out = Vec::new();
        while let Ok(Some(frame)) = resp.message().await {
            if let Some(exec_response::Frame::Stdout(b)) = frame.frame {
                out.extend_from_slice(&b);
            }
        }
        saw = String::from_utf8_lossy(&out).trim().to_string();
        if saw.contains(payload) {
            eprintln!("reader read the writer's file after {attempt} retr(ies): {saw:?}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(
        saw.contains(payload),
        "the reader member never saw the file the writer wrote to the shared volume: {saw:?}"
    );

    // Cleanup: removes every member and the shared netns.
    pods.delete_pod(DeletePodRequest {
        name: pod.into(),
        namespace: "default".into(),
        force: true,
        ..Default::default()
    })
    .await
    .expect("DeletePod over gRPC");
}
