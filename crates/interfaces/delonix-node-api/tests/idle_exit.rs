//! The idle exit of `serve_listener` (ADR-0040 P5): an operation this process
//! began and has not finished keeps the server up past the timeout, and the
//! server exits once it ends. The binary-level tests (`delonix-node-api-bin`'s
//! `socket_activation.rs`) cover the connection half; this one needs a ledger
//! entry owned by the serving process, which only an in-process server has.

use std::time::Duration;

use delonix_node_api::activation::IdleExit;
use delonix_node_api::operations::{self, Begun};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_operation_of_this_process_holds_the_idle_exit() {
    let sock_dir = tempfile::tempdir_in("/tmp").unwrap();
    let root = tempfile::tempdir().unwrap();
    let rec = match operations::begin(root.path(), "create", "Network/held", "", "").unwrap() {
        Begun::New(r) => r,
        Begun::Replay(r) => panic!("a replay of {}", r.id),
    };
    let uds = tokio::net::UnixListener::bind(sock_dir.path().join("n.sock")).unwrap();
    let idle = IdleExit {
        timeout: Duration::from_millis(200),
        root: root.path().to_path_buf(),
    };
    let server = tokio::spawn(delonix_node_api::serve_listener(
        uds,
        delonix_node_api::router(),
        Some(idle),
    ));

    // Five timeouts with no connection at all, and it is still up.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        !server.is_finished(),
        "the server exited with an operation of its own still RUNNING"
    );

    operations::finish(root.path(), rec, Ok(String::new())).unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the server did not exit once the operation ended");
    ended.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_an_idle_exit_the_server_stays() {
    let sock_dir = tempfile::tempdir_in("/tmp").unwrap();
    let uds = tokio::net::UnixListener::bind(sock_dir.path().join("n.sock")).unwrap();
    let server = tokio::spawn(delonix_node_api::serve_listener(
        uds,
        delonix_node_api::router(),
        None,
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!server.is_finished());
    server.abort();
}
