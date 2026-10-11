//! `NodeService.WatchEvents` — the engine's event log as a stream (ADR-0042
//! step E).
//!
//! There is no daemon to multiplex events: every command appends a line to
//! `<root>/events.jsonl` and exits (`delonix_node::events`), and the file IS
//! the bus. This module tails it, the way `delonix system events -f` does:
//! the same records, polled from the file.
//!
//! - **`since` unset**: only what is appended after the watch starts.
//! - **`since` set**: the retained log is replayed first — the rotated
//!   generation (`events.jsonl.1`) and the current file — then the stream
//!   follows. The FIRST message states the retention, as the contract asks:
//!   `kind: "events"`, `action: "replay"`, with `oldest_retained_unix` (the
//!   oldest record kept, empty when the log is empty) and `complete` — `false`
//!   when the log has rotated and `since` is older than what it kept, so
//!   events in between may be gone.
//! - **`kinds`** filters by the record's kind (`container`, `volume`, …).
//! - **`namespace`** is refused: the engine's event records carry no
//!   namespace, and answering with every namespace's events under a filter
//!   that names one would be the filter silently ignored.
//!
//! The stream never ends on its own; a watcher that goes away stops the
//! polling.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use delonix_node::events::Event as Record;
use tonic::Status;

use crate::proto::v1::{Event, WatchEventsRequest};

/// How often the log is read for new records.
pub const POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// The contract's resource kind for an event record's kind.
fn kind_name(kind: &str) -> String {
    match kind {
        "container" => "Container".into(),
        "vm" => "VirtualMachine".into(),
        "network" => "Network".into(),
        "volume" => "Volume".into(),
        "image" => "Image".into(),
        "pod" => "Pod".into(),
        "stack" => "Stack".into(),
        "secret" => "Secret".into(),
        other => {
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().chain(c).collect(),
                None => String::new(),
            }
        }
    }
}

/// A record as the contract's `Event`.
pub fn message(r: &Record) -> Event {
    let mut attributes = HashMap::new();
    if !r.id.is_empty() {
        attributes.insert("id".to_string(), r.id.clone());
    }
    if let Some(d) = &r.detail {
        attributes.insert("detail".to_string(), d.clone());
    }
    Event {
        time: Some(pbjson_types::Timestamp {
            seconds: r.ts as i64,
            nanos: 0,
        }),
        kind: r.kind.clone(),
        action: r.action.clone(),
        target: format!("{}/{}", kind_name(&r.kind), r.name),
        namespace: String::new(),
        attributes,
        trace_id: String::new(),
    }
}

/// The records of one file, oldest first; an unreadable line is skipped (the
/// same rule `delonix system events` reads by).
fn read_file(path: &Path) -> Vec<Record> {
    std::fs::read_to_string(path)
        .map(|d| {
            d.lines()
                .filter_map(|l| serde_json::from_str::<Record>(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The first message of a replay: what the log still holds.
fn retention(oldest: Option<u64>, rotated: bool, since: u64) -> Event {
    let complete = !rotated || oldest.is_some_and(|o| since >= o);
    Event {
        time: None,
        kind: "events".into(),
        action: "replay".into(),
        target: String::new(),
        namespace: String::new(),
        attributes: HashMap::from([
            (
                "oldest_retained_unix".to_string(),
                oldest.map(|o| o.to_string()).unwrap_or_default(),
            ),
            ("complete".to_string(), complete.to_string()),
        ]),
        trace_id: String::new(),
    }
}

/// What a request selects, validated before the stream starts.
struct Filter {
    kinds: Vec<String>,
}

impl Filter {
    fn wants(&self, r: &Record) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&r.kind)
    }
}

/// `WatchEvents` under `root`.
pub fn watch(
    root: PathBuf,
    req: WatchEventsRequest,
    poll: std::time::Duration,
) -> Result<tokio_stream::wrappers::ReceiverStream<Result<Event, Status>>, Status> {
    if !req.namespace.is_empty() {
        return Err(Status::unimplemented(format!(
            "WatchEvents: namespace '{}': the engine's event records carry no namespace yet, \
             so this filter cannot be honoured — watch without it, or filter by kind",
            req.namespace
        )));
    }
    let filter = Filter {
        kinds: req.kinds.clone(),
    };
    let mut first: Vec<Event> = Vec::new();
    let offset = match &req.since {
        None => delonix_node::events::size(&root),
        Some(since) => {
            let since = since.seconds.max(0) as u64;
            let rotated_path = root.join("events.jsonl.1");
            let rotated = rotated_path.exists();
            let mut kept = read_file(&rotated_path);
            let (current, offset) = delonix_node::events::read_from(&root, 0);
            kept.extend(current);
            first.push(retention(kept.first().map(|r| r.ts), rotated, since));
            first.extend(
                kept.iter()
                    .filter(|r| r.ts >= since && filter.wants(r))
                    .map(message),
            );
            offset
        }
    };
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        for ev in first {
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
        let mut offset = offset;
        loop {
            tokio::select! {
                _ = tx.closed() => return,
                _ = tokio::time::sleep(poll) => {}
            }
            let r = root.clone();
            let read =
                tokio::task::spawn_blocking(move || delonix_node::events::read_from(&r, offset))
                    .await;
            let Ok((records, next)) = read else {
                return;
            };
            offset = next;
            for r in records.iter().filter(|r| filter.wants(r)) {
                if tx.send(Ok(message(r))).await.is_err() {
                    return;
                }
            }
        }
    });
    Ok(tokio_stream::wrappers::ReceiverStream::new(rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt;

    const FAST: std::time::Duration = std::time::Duration::from_millis(20);

    async fn next(s: &mut tokio_stream::wrappers::ReceiverStream<Result<Event, Status>>) -> Event {
        tokio::time::timeout(std::time::Duration::from_secs(10), s.next())
            .await
            .expect("no event within 10s")
            .expect("the stream ended")
            .expect("an error in the stream")
    }

    #[tokio::test]
    async fn without_since_only_what_happens_after_the_watch_starts_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        delonix_node::events::emit(&root, "volume", "create", "old", "old", None);
        let mut s = watch(root.clone(), WatchEventsRequest::default(), FAST).unwrap();
        delonix_node::events::emit(&root, "volume", "remove", "v1", "v1", Some("force"));
        let ev = next(&mut s).await;
        assert_eq!((ev.kind.as_str(), ev.action.as_str()), ("volume", "remove"));
        assert_eq!(ev.target, "Volume/v1");
        assert_eq!(
            ev.attributes.get("detail").map(String::as_str),
            Some("force")
        );
    }

    #[tokio::test]
    async fn kinds_filter_what_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let req = WatchEventsRequest {
            kinds: vec!["container".into()],
            ..Default::default()
        };
        let mut s = watch(root.clone(), req, FAST).unwrap();
        delonix_node::events::emit(&root, "volume", "create", "v", "v", None);
        delonix_node::events::emit(&root, "container", "start", "abc", "web", None);
        let ev = next(&mut s).await;
        assert_eq!(ev.target, "Container/web", "{ev:?}");
    }

    #[tokio::test]
    async fn since_replays_the_log_after_a_first_message_that_states_its_retention() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        delonix_node::events::emit(&root, "volume", "create", "v", "v", None);
        delonix_node::events::emit(&root, "container", "start", "abc", "web", None);
        let req = WatchEventsRequest {
            since: Some(pbjson_types::Timestamp {
                seconds: 0,
                nanos: 0,
            }),
            ..Default::default()
        };
        let mut s = watch(root.clone(), req, FAST).unwrap();
        let head = next(&mut s).await;
        assert_eq!(
            (head.kind.as_str(), head.action.as_str()),
            ("events", "replay")
        );
        assert!(!head.attributes["oldest_retained_unix"].is_empty());
        // Nothing rotated: the log holds everything since the epoch it saw.
        assert_eq!(head.attributes["complete"], "true");
        assert_eq!(next(&mut s).await.target, "Volume/v");
        assert_eq!(next(&mut s).await.target, "Container/web");
        // Then it follows.
        delonix_node::events::emit(&root, "volume", "remove", "v", "v", None);
        assert_eq!(next(&mut s).await.action, "remove");
    }

    #[test]
    fn a_rotated_log_older_than_since_is_said_to_be_incomplete() {
        assert_eq!(
            retention(Some(100), true, 50).attributes["complete"],
            "false"
        );
        assert_eq!(
            retention(Some(100), true, 100).attributes["complete"],
            "true"
        );
        assert_eq!(
            retention(Some(100), false, 50).attributes["complete"],
            "true"
        );
        assert_eq!(retention(None, true, 50).attributes["complete"], "false");
    }

    #[test]
    fn a_namespace_filter_is_refused_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let req = WatchEventsRequest {
            namespace: "team-a".into(),
            ..Default::default()
        };
        let err = watch(dir.path().to_path_buf(), req, FAST).err().unwrap();
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }
}
