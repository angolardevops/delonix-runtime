//! The operation record (ADR-0040 D4, ADR-0042 D2 «Async»).
//!
//! A mutation answers with an `Operation`, and the operation is written under
//! the state root BEFORE the work starts: the server is socket-activated and
//! may exit, so what a client is told has to be on disk. One file per
//! operation, `<root>/operations/<id>.json`.
//!
//! - **Idempotency.** A request that names a `request_id` gets the id
//!   `r-<request_id>`, created exclusively: the same request sent twice finds
//!   the first one's record and is answered with it, whatever its state.
//! - **Interrupted.** A record that is not finished names the process that
//!   owns it (pid and start time). When that process is gone, the next read
//!   ends the operation `FAILED` with reason `Interrupted` — never `RUNNING`
//!   forever.
//! - **Retention.** A finished record older than [`RETENTION_SECS`] is removed
//!   when a new operation begins.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tonic::Status;

use crate::proto::v1::{
    ErrorDetail, ListOperationsRequest, ListOperationsResponse, Operation, OperationState,
    PageResponse,
};

/// How long a finished operation stays readable.
pub const RETENTION_SECS: u64 = 7 * 24 * 3600;
pub use crate::page::{DEFAULT_PAGE, MAX_PAGE};

/// Where an operation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    Succeeded,
    Failed,
}

/// Why an operation failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// The engine's error class (`DX_CONFLICT`), or `Interrupted`.
    pub reason: String,
    /// The dictionary number (`DX-5307`); empty for `Interrupted`.
    #[serde(default)]
    pub dx: String,
    pub message: String,
}

/// One operation, as persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub state: State,
    /// `Network/lab` — what it acts on.
    pub target: String,
    pub verb: String,
    /// The resource's path, linked once a create succeeded.
    #[serde(default)]
    pub target_href: String,
    pub created_ms: u64,
    #[serde(default)]
    pub ended_ms: Option<u64>,
    #[serde(default)]
    pub failure: Option<Failure>,
    #[serde(default)]
    pub request_id: String,
    /// The process doing the work, while it is not finished.
    pub owner_pid: i32,
    pub owner_starttime: u64,
}

/// What [`begin`] found.
#[derive(Debug)]
pub enum Begun {
    /// A new operation, persisted as running: the caller does the work.
    New(Record),
    /// The same `request_id` was already answered: this is that answer.
    Replay(Record),
}

fn dir(root: &Path) -> PathBuf {
    root.join("operations")
}

fn path(root: &Path, id: &str) -> PathBuf {
    dir(root).join(format!("{id}.json"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn io(what: &str, e: impl std::fmt::Display) -> Status {
    Status::internal(format!("operation record: {what}: {e}"))
}

/// An id is a file name: letters, digits, `.`, `_`, `-`, at most 80.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A `request_id` becomes part of a file name, so it is held to the same
/// alphabet (a UUID fits); anything else is refused, never rewritten.
fn check_request_id(request_id: &str) -> Result<(), Status> {
    if request_id.len() <= 64 && valid_id(request_id) {
        return Ok(());
    }
    Err(Status::invalid_argument(format!(
        "request_id '{request_id}': 1 to 64 characters of letters, digits, '.', '_' and '-' \
         (a UUID), not starting with '.'"
    )))
}

fn write(root: &Path, rec: &Record) -> Result<(), Status> {
    let bytes = serde_json::to_vec_pretty(rec).map_err(|e| io("encoding", e))?;
    delonix_state::write_atomic(&path(root, &rec.id), &bytes).map_err(|e| io("writing", e))
}

fn read(root: &Path, id: &str) -> Result<Option<Record>, Status> {
    match std::fs::read(path(root, id)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io(&format!("reading {id}"), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(&format!("reading {id}"), e)),
    }
}

/// Whether the process that owns `rec` is still the one that started it.
fn owner_alive(rec: &Record) -> bool {
    delonix_node::proc_starttime(rec.owner_pid) == Some(rec.owner_starttime)
}

/// `rec` as a reader may see it: an unfinished operation whose owner is gone
/// is ended `FAILED/Interrupted`, and that is written before it is returned.
fn settled(root: &Path, mut rec: Record) -> Result<Record, Status> {
    if rec.state == State::Running && !owner_alive(&rec) {
        rec.state = State::Failed;
        rec.ended_ms = Some(now_ms());
        rec.failure = Some(Failure {
            reason: "Interrupted".into(),
            dx: String::new(),
            message: "the process doing this operation exited before it finished; \
                      read the target to see what it left, and send the request again"
                .into(),
        });
        write(root, &rec)?;
    }
    Ok(rec)
}

/// Removes finished records past [`RETENTION_SECS`]. Best effort.
fn prune(root: &Path) {
    let cutoff = now_ms().saturating_sub(RETENTION_SECS * 1000);
    for rec in all(root).unwrap_or_default() {
        if rec.ended_ms.is_some_and(|t| t < cutoff) {
            let _ = std::fs::remove_file(path(root, &rec.id));
        }
    }
}

/// Every record under `root`, oldest first. A state root where no operation
/// ever ran has no directory — an empty list, and reading does not create it.
fn all(root: &Path) -> Result<Vec<Record>, Status> {
    let entries = match std::fs::read_dir(dir(root)) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io("listing", e)),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        // A temp file of `write_atomic` starts with '.': not a record.
        if !valid_id(id) {
            continue;
        }
        if let Some(rec) = read(root, id)? {
            out.push(rec);
        }
    }
    out.sort_by(|a, b| (a.created_ms, &a.id).cmp(&(b.created_ms, &b.id)));
    Ok(out)
}

/// Starts an operation: persisted as running, owned by this process, before
/// the caller does anything.
///
/// With a `request_id`, the id is derived from it and the record is created
/// exclusively. If it already exists and is the same `verb` on the same
/// `target`, the caller gets [`Begun::Replay`] and must not do the work again;
/// a `request_id` that answered something else is refused.
pub fn begin(root: &Path, verb: &str, target: &str, request_id: &str) -> Result<Begun, Status> {
    let id = if request_id.is_empty() {
        format!("op-{}", delonix_node::generate_id())
    } else {
        check_request_id(request_id)?;
        format!("r-{request_id}")
    };
    if let Some(found) = replay(root, verb, target, request_id)? {
        return Ok(Begun::Replay(found));
    }
    std::fs::create_dir_all(dir(root)).map_err(|e| io("creating the directory", e))?;
    prune(root);
    let pid = std::process::id() as i32;
    let rec = Record {
        id: id.clone(),
        state: State::Running,
        target: target.to_string(),
        verb: verb.to_string(),
        target_href: String::new(),
        created_ms: now_ms(),
        ended_ms: None,
        failure: None,
        request_id: request_id.to_string(),
        owner_pid: pid,
        owner_starttime: delonix_node::proc_starttime(pid).unwrap_or_default(),
    };
    // Exclusive creation: the whole record is written to a private file and
    // linked under its name. `link` fails if the name exists, so of two
    // requests with one `request_id` exactly one begins.
    let bytes = serde_json::to_vec_pretty(&rec).map_err(|e| io("encoding", e))?;
    let staged = dir(root).join(format!(".{id}.{pid}.new"));
    std::fs::write(&staged, bytes).map_err(|e| io("writing", e))?;
    let linked = std::fs::hard_link(&staged, path(root, &id));
    let _ = std::fs::remove_file(&staged);
    match linked {
        Ok(()) => Ok(Begun::New(rec)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            match replay(root, verb, target, request_id)? {
                Some(found) => Ok(Begun::Replay(found)),
                None => Err(io("creating", e)),
            }
        }
        Err(e) => Err(io("creating", e)),
    }
}

/// The record a `request_id` already produced, if it answers this same
/// request. `None` when there is none (or no `request_id`).
pub fn replay(
    root: &Path,
    verb: &str,
    target: &str,
    request_id: &str,
) -> Result<Option<Record>, Status> {
    if request_id.is_empty() {
        return Ok(None);
    }
    check_request_id(request_id)?;
    let Some(found) = read(root, &format!("r-{request_id}"))? else {
        return Ok(None);
    };
    if found.verb != verb || found.target != target {
        return Err(Status::invalid_argument(format!(
            "request_id '{request_id}' already answered '{} {}': a request_id names one request",
            found.verb, found.target
        )));
    }
    settled(root, found).map(Some)
}

/// Ends `rec` with the outcome of its work and writes it.
pub fn finish(
    root: &Path,
    mut rec: Record,
    outcome: Result<String, delonix_model::Error>,
) -> Result<Record, Status> {
    rec.ended_ms = Some(now_ms());
    match outcome {
        Ok(target_href) => {
            rec.state = State::Succeeded;
            rec.target_href = target_href;
        }
        Err(e) => {
            rec.state = State::Failed;
            rec.failure = Some(Failure {
                reason: e.code().to_string(),
                dx: delonix_model::codes::label(e.number()),
                message: e.to_string(),
            });
        }
    }
    write(root, &rec)?;
    Ok(rec)
}

fn timestamp(ms: u64) -> pbjson_types::Timestamp {
    pbjson_types::Timestamp {
        seconds: (ms / 1000) as i64,
        nanos: ((ms % 1000) * 1_000_000) as i32,
    }
}

/// A record as the contract's `Operation`.
pub fn message(rec: &Record) -> Operation {
    let mut links = vec![
        crate::node::link("self", &format!("/v1/operations/{}", rec.id)),
        crate::node::link("collection", "/v1/operations"),
    ];
    if rec.state == State::Succeeded && !rec.target_href.is_empty() {
        links.push(crate::node::link("target", &rec.target_href));
    }
    Operation {
        id: rec.id.clone(),
        state: match rec.state {
            State::Running => OperationState::Running,
            State::Succeeded => OperationState::Succeeded,
            State::Failed => OperationState::Failed,
        } as i32,
        target: rec.target.clone(),
        verb: rec.verb.clone(),
        progress_message: match (&rec.state, &rec.failure) {
            (_, Some(f)) => f.message.clone(),
            (State::Succeeded, _) => "done".into(),
            _ => String::new(),
        },
        progress_percent: match rec.state {
            State::Succeeded => 100,
            _ => -1,
        },
        create_time: Some(timestamp(rec.created_ms)),
        end_time: rec.ended_ms.map(timestamp),
        result: None,
        error: rec.failure.as_ref().map(|f| {
            let mut metadata =
                std::collections::HashMap::from([("message".to_string(), f.message.clone())]);
            if !f.dx.is_empty() {
                metadata.insert("dx".to_string(), f.dx.clone());
            }
            ErrorDetail {
                reason: f.reason.clone(),
                resource: rec.target.clone(),
                metadata,
                retryable: f.reason == "Interrupted",
                retry_after: None,
            }
        }),
        request_id: rec.request_id.clone(),
        trace_id: String::new(),
        links,
    }
}

/// `GetOperation` under `root`.
pub fn get_in(root: &Path, id: &str) -> Result<Operation, Status> {
    let missing = || Status::not_found(format!("operation '{id}'"));
    if !valid_id(id) {
        return Err(missing());
    }
    let rec = read(root, id)?.ok_or_else(missing)?;
    Ok(message(&settled(root, rec)?))
}

/// `ListOperations` under `root`: oldest first, one page at a time.
pub fn list_in(root: &Path, req: &ListOperationsRequest) -> Result<ListOperationsResponse, Status> {
    let page = req.page.clone().unwrap_or_default();
    let size = crate::page::size(&page)?;
    // A page token is `<created_ms>-<id>` of the last record of the page
    // before: opaque to the caller, nothing for the server to remember.
    let after = match page.page_token.as_str() {
        "" => None,
        token => Some(
            token
                .split_once('-')
                .and_then(|(ms, id)| Some((ms.parse::<u64>().ok()?, id.to_string())))
                .filter(|(_, id)| valid_id(id))
                .ok_or_else(|| {
                    Status::invalid_argument(format!(
                        "page_token '{token}' was not issued by this list"
                    ))
                })?,
        ),
    };
    let mut records = Vec::new();
    for rec in all(root)? {
        let rec = settled(root, rec)?;
        if req.active_only && rec.state != State::Running {
            continue;
        }
        if after
            .as_ref()
            .is_some_and(|(ms, id)| (rec.created_ms, &rec.id) <= (*ms, id))
        {
            continue;
        }
        records.push(rec);
    }
    let more = records.len() > size;
    records.truncate(size);
    let next = match (more, records.last()) {
        (true, Some(last)) => format!("{}-{}", last.created_ms, last.id),
        _ => String::new(),
    };
    let href = |token: &str| {
        let mut q = Vec::new();
        if req.active_only {
            q.push("active_only=true".to_string());
        }
        if page.page_size != 0 {
            q.push(format!("page.page_size={}", page.page_size));
        }
        if !token.is_empty() {
            q.push(format!("page.page_token={token}"));
        }
        if q.is_empty() {
            "/v1/operations".to_string()
        } else {
            format!("/v1/operations?{}", q.join("&"))
        }
    };
    let mut links = vec![
        crate::node::link("self", &href(&page.page_token)),
        crate::node::link("root", "/v1"),
    ];
    if !next.is_empty() {
        links.push(crate::node::link("next", &href(&next)));
    }
    Ok(ListOperationsResponse {
        operations: records.iter().map(message).collect(),
        page: Some(PageResponse {
            next_page_token: next,
        }),
        links,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::v1::PageRequest;

    fn new(root: &Path, verb: &str, target: &str, request_id: &str) -> Record {
        match begin(root, verb, target, request_id).unwrap() {
            Begun::New(r) => r,
            Begun::Replay(r) => panic!("expected a new operation, got a replay of {}", r.id),
        }
    }

    #[test]
    fn an_operation_is_on_disk_before_the_work_and_running_until_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let rec = new(dir.path(), "create", "Network/lab", "");
        // On disk already, with nothing done yet.
        let seen = get_in(dir.path(), &rec.id).unwrap();
        assert_eq!(seen.state, OperationState::Running as i32);
        assert_eq!(seen.target, "Network/lab");
        assert!(seen.end_time.is_none());
        assert_eq!(seen.progress_percent, -1);

        let done = finish(
            dir.path(),
            rec,
            Ok("/v1/namespaces/default/networks/lab".into()),
        )
        .unwrap();
        let seen = get_in(dir.path(), &done.id).unwrap();
        assert_eq!(seen.state, OperationState::Succeeded as i32);
        assert!(seen.end_time.is_some());
        let rels: Vec<&str> = seen.links.iter().map(|l| l.rel.as_str()).collect();
        assert_eq!(rels, ["self", "collection", "target"]);
    }

    #[test]
    fn a_failure_carries_the_engines_class_and_number() {
        let dir = tempfile::tempdir().unwrap();
        let rec = new(dir.path(), "delete", "Network/lab", "");
        let err = delonix_model::Error::coded(
            5307,
            delonix_model::Error::Conflict("network 'lab' is in use".into()),
        );
        let done = finish(dir.path(), rec, Err(err)).unwrap();
        let seen = get_in(dir.path(), &done.id).unwrap();
        assert_eq!(seen.state, OperationState::Failed as i32);
        let detail = seen.error.unwrap();
        assert_eq!(detail.reason, "DX_CONFLICT");
        assert_eq!(detail.resource, "Network/lab");
        assert_eq!(detail.metadata["dx"], "DX-5307");
        assert!(!detail.retryable);
        // No link to a target that a failed operation did not produce.
        assert!(seen.links.iter().all(|l| l.rel != "target"));
    }

    #[test]
    fn the_same_request_id_is_answered_once() {
        let dir = tempfile::tempdir().unwrap();
        let first = new(dir.path(), "create", "Network/lab", "3f2a-01");
        assert_eq!(first.id, "r-3f2a-01");
        // Sent again while it runs, and again after it ended: the same record.
        match begin(dir.path(), "create", "Network/lab", "3f2a-01").unwrap() {
            Begun::Replay(r) => assert_eq!(r.id, first.id),
            Begun::New(_) => panic!("the same request_id began twice"),
        }
        finish(dir.path(), first, Ok(String::new())).unwrap();
        match begin(dir.path(), "create", "Network/lab", "3f2a-01").unwrap() {
            Begun::Replay(r) => assert_eq!(r.state, State::Succeeded),
            Begun::New(_) => panic!("the same request_id began twice"),
        }
        assert_eq!(all(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn a_request_id_that_answered_something_else_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        new(dir.path(), "create", "Network/lab", "k1");
        for (verb, target) in [("delete", "Network/lab"), ("create", "Network/other")] {
            let err = begin(dir.path(), verb, target, "k1").unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err
                .message()
                .contains("already answered 'create Network/lab'"));
        }
    }

    #[test]
    fn a_request_id_that_is_not_a_file_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["../x", "a/b", ".hidden", "a b", &"x".repeat(65)] {
            let err = begin(dir.path(), "create", "Network/lab", bad).unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{bad}");
        }
        assert!(!dir.path().join("operations").exists());
    }

    #[test]
    fn an_operation_whose_owner_is_gone_ends_failed_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = new(dir.path(), "create", "Network/lab", "");
        // The owner: a pid that is this process with another start time —
        // what a recycled pid looks like.
        rec.owner_starttime += 1;
        write(dir.path(), &rec).unwrap();

        let seen = get_in(dir.path(), &rec.id).unwrap();
        assert_eq!(seen.state, OperationState::Failed as i32);
        let detail = seen.error.unwrap();
        assert_eq!(detail.reason, "Interrupted");
        assert!(detail.retryable);
        // And it was written: the record on disk is finished.
        let on_disk = read(dir.path(), &rec.id).unwrap().unwrap();
        assert_eq!(on_disk.state, State::Failed);
        assert!(on_disk.ended_ms.is_some());
    }

    #[test]
    fn a_running_operation_of_a_live_owner_stays_running() {
        let dir = tempfile::tempdir().unwrap();
        let rec = new(dir.path(), "create", "Network/lab", "");
        let seen = get_in(dir.path(), &rec.id).unwrap();
        assert_eq!(seen.state, OperationState::Running as i32);
    }

    #[test]
    fn an_unknown_or_malformed_id_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["nope", "../etc/passwd", ""] {
            let err = get_in(dir.path(), id).unwrap_err();
            assert_eq!(err.code(), tonic::Code::NotFound, "{id}");
        }
    }

    #[test]
    fn the_list_pages_oldest_first_and_filters_the_active_ones() {
        let dir = tempfile::tempdir().unwrap();
        let mut ids = Vec::new();
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            let mut rec = new(dir.path(), "create", &format!("Network/{name}"), "");
            // Distinct, ordered creation times whatever the clock did.
            rec.created_ms = 1_000 + i as u64;
            std::fs::remove_file(path(dir.path(), &rec.id)).unwrap();
            write(dir.path(), &rec).unwrap();
            ids.push(rec.id.clone());
            if *name != "c" {
                finish(dir.path(), rec, Ok(String::new())).unwrap();
            }
        }
        let list = |size: i32, token: &str, active_only: bool| {
            list_in(
                dir.path(),
                &ListOperationsRequest {
                    page: Some(PageRequest {
                        page_size: size,
                        page_token: token.to_string(),
                    }),
                    active_only,
                },
            )
            .unwrap()
        };
        let first = list(2, "", false);
        let got: Vec<&str> = first.operations.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(got, [ids[0].as_str(), ids[1].as_str()]);
        let token = first.page.unwrap().next_page_token;
        assert!(!token.is_empty());
        assert!(first.links.iter().any(|l| l.rel == "next"));

        let second = list(2, &token, false);
        let got: Vec<&str> = second.operations.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(got, [ids[2].as_str()]);
        assert!(second.page.unwrap().next_page_token.is_empty());
        assert!(second.links.iter().all(|l| l.rel != "next"));

        let active = list(0, "", true);
        let got: Vec<&str> = active.operations.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(got, [ids[2].as_str()]);

        let err = list_in(
            dir.path(),
            &ListOperationsRequest {
                page: Some(PageRequest {
                    page_size: 0,
                    page_token: "not-a-token/".into(),
                }),
                active_only: false,
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn a_finished_record_past_retention_is_removed_when_another_begins() {
        let dir = tempfile::tempdir().unwrap();
        let old = new(dir.path(), "create", "Network/old", "");
        let mut old = finish(dir.path(), old, Ok(String::new())).unwrap();
        old.ended_ms = Some(now_ms() - (RETENTION_SECS + 60) * 1000);
        write(dir.path(), &old).unwrap();
        let kept = new(dir.path(), "create", "Network/kept", "");
        let kept = finish(dir.path(), kept, Ok(String::new())).unwrap();

        new(dir.path(), "create", "Network/new", "");
        assert!(read(dir.path(), &old.id).unwrap().is_none());
        assert!(read(dir.path(), &kept.id).unwrap().is_some());
    }
}
