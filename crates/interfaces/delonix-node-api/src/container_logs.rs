//! `ContainerService.Logs` — the container's log file as a gRPC server stream,
//! the same follow the CLI's `container logs -f` does, lifted out of stdout
//! into a channel of `LogChunk`s (the molde of [`crate::events::watch`]).
//!
//! The log is a single append-only file written by the per-container shim,
//! rotated by size (the handle is reopened when the file shrinks). The shim
//! merges stdout and stderr into that one file, so every chunk is reported on
//! `STDOUT` — the merged file cannot tell them apart. `tail_lines` trims the
//! backlog to its last lines; `follow` then streams every append until the
//! container leaves `Running`/`Paused`. `timestamps`/`since` need the per-line
//! CRI format (`--log-cri`) and are a later pass — asked for here, they are
//! `UNIMPLEMENTED`, not silently ignored.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tonic::Status;

use crate::proto::v1::{LogChunk, LogStream, LogsRequest};

/// The follow poll, the same 300ms cadence `container logs -f` uses.
pub const POLL: Duration = Duration::from_millis(300);

/// Where a container's log file is: its recorded `log_path` (set when it runs),
/// or the engine's default `<root>/containers/<id>/log` for a record written
/// before that field, or without one.
fn log_path(root: &Path, c: &delonix_compute::Container) -> PathBuf {
    match c.log_path.as_deref().filter(|s| !s.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => root.join("containers").join(&c.id).join("log"),
    }
}

fn chunk(data: Vec<u8>) -> LogChunk {
    LogChunk {
        stream: LogStream::Stdout as i32,
        data,
        timestamp_unix_nanos: 0,
    }
}

/// The last `n` newline-delimited lines of `buf` (a trailing newline does not
/// count as an empty final line). `n == 0` means the whole backlog.
fn tail(buf: &[u8], n: usize) -> Vec<u8> {
    if n == 0 {
        return buf.to_vec();
    }
    let mut starts = vec![0usize];
    for (i, &b) in buf.iter().enumerate() {
        if b == b'\n' && i + 1 < buf.len() {
            starts.push(i + 1);
        }
    }
    let start = if starts.len() > n {
        starts[starts.len() - n]
    } else {
        0
    };
    buf[start..].to_vec()
}

/// Read what was appended to `path` since byte `pos`, handling shim rotation
/// (when the file is now shorter than `pos`, it rotated — start over at 0).
/// Returns the new bytes and the new position.
fn read_delta(path: &Path, pos: u64) -> std::io::Result<(Vec<u8>, u64)> {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let pos = if len < pos { 0 } else { pos };
    if len <= pos {
        return Ok((Vec::new(), pos));
    }
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(pos))?;
    let mut b = Vec::new();
    f.read_to_end(&mut b)?;
    let newpos = pos + b.len() as u64;
    Ok((b, newpos))
}

/// Is the container still producing logs — i.e. still `Running`/`Paused` after
/// a liveness reconcile? Stops the follow when it is not.
fn still_live(root: &Path, id: &str) -> bool {
    let Ok(store) = delonix_state::Store::open(root.join("containers")) else {
        return false;
    };
    let Ok(mut c) = store.load(id) else {
        return false;
    };
    delonix_linux::reconcile_status(&mut c);
    matches!(
        c.status,
        delonix_model::records::Status::Running | delonix_model::records::Status::Paused
    )
}

/// `Logs` under `root`: the backlog (optionally tailed), then — with
/// `follow` — every append until the container stops.
pub fn stream(
    root: PathBuf,
    req: LogsRequest,
    poll: Duration,
) -> Result<tokio_stream::wrappers::ReceiverStream<Result<LogChunk, Status>>, Status> {
    if req.timestamps || req.since.is_some() {
        return Err(Status::unimplemented(
            "ContainerService.Logs: `timestamps` and `since` need the per-line CRI log format \
             and are not served in this pass — `follow` and `tail_lines` are",
        ));
    }
    let ns = if req.namespace.is_empty() {
        "default"
    } else {
        &req.namespace
    };
    let missing =
        || Status::not_found(format!("container '{}' in namespace '{ns}'", req.container));
    if ns == "*" || !root.join("containers").is_dir() {
        return Err(missing());
    }
    let store = delonix_state::Store::open(root.join("containers"))
        .map_err(|e| crate::network_ops::status_of(&e.into()))?;
    let c = store.load(&req.container).map_err(|e| {
        if e.is_not_found() {
            missing()
        } else {
            crate::network_ops::status_of(&e.into())
        }
    })?;
    if c.namespace != ns {
        return Err(missing());
    }
    let path = log_path(&root, &c);
    let mut file = std::fs::File::open(&path).map_err(|e| {
        Status::not_found(format!(
            "no logs for '{}' at {}: {e}",
            req.container,
            path.display()
        ))
    })?;

    let mut backlog = Vec::new();
    file.read_to_end(&mut backlog)
        .map_err(|e| Status::internal(format!("reading logs for '{}': {e}", req.container)))?;
    let mut pos = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if req.tail_lines > 0 {
        backlog = tail(&backlog, req.tail_lines as usize);
    }

    let follow = req.follow;
    let id = c.id.clone();
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        if !backlog.is_empty() && tx.send(Ok(chunk(backlog))).await.is_err() {
            return;
        }
        if !follow {
            return;
        }
        loop {
            tokio::select! {
                _ = tx.closed() => return,
                _ = tokio::time::sleep(poll) => {}
            }
            let p = path.clone();
            let read = tokio::task::spawn_blocking(move || read_delta(&p, pos)).await;
            let Ok(Ok((data, next))) = read else {
                return;
            };
            pos = next;
            if !data.is_empty() {
                if tx.send(Ok(chunk(data))).await.is_err() {
                    return;
                }
                continue;
            }
            // Nothing new: stop once the container is no longer producing.
            let (r, id2) = (root.clone(), id.clone());
            match tokio::task::spawn_blocking(move || still_live(&r, &id2)).await {
                Ok(true) => {}
                _ => return,
            }
        }
    });
    Ok(tokio_stream::wrappers::ReceiverStream::new(rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_lines_and_zero_keeps_all() {
        let b = b"a\nb\nc\nd\n";
        assert_eq!(tail(b, 0), b.to_vec());
        assert_eq!(tail(b, 2), b"c\nd\n".to_vec());
        assert_eq!(tail(b, 10), b.to_vec());
        // No trailing newline: the last line still counts.
        assert_eq!(tail(b"x\ny\nz", 1), b"z".to_vec());
    }

    #[test]
    fn read_delta_sees_appends_and_restarts_on_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        std::fs::write(&p, b"hello\n").unwrap();
        let (data, pos) = read_delta(&p, 0).unwrap();
        assert_eq!(data, b"hello\n");
        assert_eq!(pos, 6);
        // No new bytes.
        let (data, pos2) = read_delta(&p, pos).unwrap();
        assert!(data.is_empty());
        assert_eq!(pos2, pos);
        // Append.
        std::fs::write(&p, b"hello\nmore\n").unwrap();
        let (data, _) = read_delta(&p, pos).unwrap();
        assert_eq!(data, b"more\n");
        // Rotation: file shorter than pos → read from the top.
        std::fs::write(&p, b"new\n").unwrap();
        let (data, _) = read_delta(&p, 999).unwrap();
        assert_eq!(data, b"new\n");
    }

    #[tokio::test]
    async fn logs_of_a_missing_container_are_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = stream(
            dir.path().to_path_buf(),
            LogsRequest {
                container: "ghost".into(),
                namespace: "default".into(),
                follow: false,
                tail_lines: 0,
                since: None,
                timestamps: false,
            },
            POLL,
        )
        .err()
        .unwrap();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn timestamps_and_since_are_unimplemented_for_now() {
        let dir = tempfile::tempdir().unwrap();
        let err = stream(
            dir.path().to_path_buf(),
            LogsRequest {
                container: "c".into(),
                namespace: "default".into(),
                follow: false,
                tail_lines: 0,
                since: None,
                timestamps: true,
            },
            POLL,
        )
        .err()
        .unwrap();
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }
}
