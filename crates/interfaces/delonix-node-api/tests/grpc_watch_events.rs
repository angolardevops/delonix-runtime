//! `WatchEvents` as a live stream, driven through the crate's public API — the
//! exact function the gRPC server dispatches to (`service.rs`:
//! `crate::events::watch(node::state_root(), …)`). A watcher opens the stream
//! over a real state root on disk, a SEPARATE writer appends an event to the
//! file bus (the daemonless way every `delonix` command records one —
//! ADR-0042 step E), and the record reaches the watcher through the real
//! polling loop. This is the capability's claim: events observed during a
//! change — including a change made by another writer — reach a watcher as a
//! stream, not only a snapshot of the log.
//!
//! It drives the public `events::watch` instead of standing up the socket so
//! the test needs no process-wide `DELONIX_ROOT`: the server adds only the
//! tonic transport around this same function, and that the stream opens over
//! the wire is covered by `grpc_list_providers`.

use delonix_node_api::events;
use delonix_node_api::proto::v1::WatchEventsRequest;
use tokio_stream::StreamExt;

#[tokio::test]
async fn watch_stream_delivers_an_event_emitted_after_it_opened() {
    let dir = tempfile::tempdir().expect("a state root");
    let root = dir.path().to_path_buf();

    // The engine already has history before the watch opens.
    delonix_node::events::emit(&root, "volume", "create", "v0", "data", None);

    // The watch opens with no `since`: only what happens AFTER it is sent. A
    // fast poll keeps the test brisk; the server runs the same loop at 200ms.
    let poll = std::time::Duration::from_millis(20);
    let mut stream = events::watch(root.clone(), WatchEventsRequest::default(), poll)
        .expect("the stream opens");

    // An EXTERNAL change: another writer appends to the file bus, exactly as a
    // concurrent `delonix container stop` would, with no daemon in between.
    delonix_node::events::emit(&root, "container", "stop", "c1", "web", Some("live"));

    // The record arrives through the polling loop — and it is the NEW one, not
    // the pre-existing volume/create, since no `since` was asked.
    let ev = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
        .await
        .expect("an event within 10s")
        .expect("the stream ended")
        .expect("an error in the stream");
    assert_eq!(
        (ev.kind.as_str(), ev.action.as_str()),
        ("container", "stop"),
        "{ev:?}"
    );
    assert_eq!(ev.target, "Container/web", "{ev:?}");
    assert_eq!(
        ev.attributes.get("detail").map(String::as_str),
        Some("live"),
        "{ev:?}"
    );

    // Filtering by kind, as a client narrowing the stream would: a volume event
    // appended now is skipped, the next container event is delivered.
    let req = WatchEventsRequest {
        kinds: vec!["container".into()],
        ..Default::default()
    };
    let mut only_containers = events::watch(root.clone(), req, poll).expect("the stream opens");
    delonix_node::events::emit(&root, "volume", "remove", "v0", "data", None);
    delonix_node::events::emit(&root, "container", "start", "c2", "api", None);
    let ev = tokio::time::timeout(std::time::Duration::from_secs(10), only_containers.next())
        .await
        .expect("an event within 10s")
        .expect("the stream ended")
        .expect("an error in the stream");
    assert_eq!(ev.target, "Container/api", "the volume event was filtered: {ev:?}");
}
