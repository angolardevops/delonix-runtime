//! Engine event log — the `docker events` of a runtime **without a daemon**.
//!
//! # Why a file and not a daemon
//!
//! `docker events` works because there is a `dockerd` always alive multiplexing a
//! stream to the clients. Here there is no daemon at all: each command is an
//! ephemeral process that is born, does its work and dies. The daemonless answer is the
//! opposite — a shared **append-only log** (`<root>/events.jsonl`): the
//! producer appends a line and exits; the reader does a `tail`. The file IS the bus.
//!
//! # Why it needs no lock
//!
//! A `write` in `O_APPEND` of less than `PIPE_BUF` (4 KiB) is **atomic** on
//! local filesystems: the kernel serializes the positioning and the write. Each
//! event is a short line, well below that limit — so N concurrent
//! processes append without interleaving and without `flock`. (An event that
//! exceeded 4 KiB would lose the guarantee; that is why the fields are fixed and short,
//! never arbitrary content like logs or env.)
//!
//! # Rotation
//!
//! Without a daemon there is no one to clean up in the background. Rotation is opportunistic: the
//! writer checks the size and, if it exceeded the ceiling, rotates to `.1` (a single
//! generation — history is not the point of this; for long-term auditing, export).

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// File ceiling before rotating (~4 MiB ≈ tens of thousands of events).
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// A lifecycle event. Deliberately few and short fields — see the
/// note about `PIPE_BUF` at the top.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Unix instant (seconds).
    pub ts: u64,
    /// `container` | `image` | `network` | `volume` | `vm`.
    pub kind: String,
    /// `create`|`start`|`stop`|`die`|`remove`|`pull`|…
    pub action: String,
    /// Object id (short).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Optional detail (e.g.: exit code in `die`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Event {
    /// Line for human consumption (`system events`).
    pub fn to_line(&self) -> String {
        let when = crate::fmt_local_ts(self.ts);
        let detail = self
            .detail
            .as_deref()
            .map(|d| format!(" ({d})"))
            .unwrap_or_default();
        format!(
            "{when}  {:<9} {:<7} {}  {}{}",
            self.kind,
            self.action,
            self.short_id(),
            self.name,
            detail
        )
    }

    fn short_id(&self) -> &str {
        &self.id[..12.min(self.id.len())]
    }
}

fn path(root: &Path) -> PathBuf {
    root.join("events.jsonl")
}

/// Appends an event. **Best-effort and infallible by design**: an error while
/// recording an event can never make the operation that generated it fail (a
/// `container stop` is not refused because the event log is full).
pub fn emit(root: &Path, kind: &str, action: &str, id: &str, name: &str, detail: Option<&str>) {
    let ev = Event {
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        kind: kind.to_string(),
        action: action.to_string(),
        id: id.to_string(),
        name: name.to_string(),
        detail: detail.map(str::to_string),
    };
    let Ok(mut line) = serde_json::to_string(&ev) else {
        return;
    };
    line.push('\n');
    let p = path(root);
    rotate_if_needed(&p);
    let _ = std::fs::create_dir_all(root);
    // `O_APPEND`: the atomicity comes from the kernel, not from a lock of ours.
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Rotates when it exceeds the ceiling. Opportunistic (the writer cleans up) — without a daemon
/// there is no other moment at which this could happen.
fn rotate_if_needed(p: &Path) {
    if std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) <= MAX_BYTES {
        return;
    }
    let _ = std::fs::rename(p, p.with_extension("jsonl.1"));
}

/// Reads the recorded events (from oldest to most recent). Corrupted
/// lines are silently skipped: an unreadable event cannot hide
/// the others.
pub fn read(root: &Path) -> Vec<Event> {
    let Ok(data) = std::fs::read_to_string(path(root)) else {
        return Vec::new();
    };
    data.lines()
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .collect()
}

/// The current log size (so `-f` knows where to continue).
pub fn size(root: &Path) -> u64 {
    std::fs::metadata(path(root)).map(|m| m.len()).unwrap_or(0)
}

/// Reads from an offset (for the `follow`). Returns the events and the new offset.
pub fn read_from(root: &Path, offset: u64) -> (Vec<Event>, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let p = path(root);
    let Ok(mut f) = std::fs::File::open(&p) else {
        return (Vec::new(), offset);
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    // Shrank = rotated; restart from the beginning so as not to miss the new file.
    let start = if len < offset { 0 } else { offset };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return (Vec::new(), offset);
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return (Vec::new(), offset);
    }
    let evs = buf
        .lines()
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .collect();
    (evs, start + buf.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("delonix-events-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn emit_e_read_fazem_round_trip() {
        let root = tmp("rt");
        emit(&root, "container", "create", "abc123def456", "web", None);
        emit(
            &root,
            "container",
            "die",
            "abc123def456",
            "web",
            Some("exit=42"),
        );
        let evs = read(&root);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].action, "create");
        assert_eq!(evs[1].detail.as_deref(), Some("exit=42"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The guarantee that underpins the lock-free design: N processes (here threads,
    /// each with its own `OpenOptions`) append WITHOUT interleaving — each
    /// line remains valid JSON and none is lost.
    #[test]
    fn emits_concorrentes_nao_se_entrelacam() {
        let root = tmp("race");
        const N: usize = 32;
        std::thread::scope(|sc| {
            for i in 0..N {
                let root = root.clone();
                sc.spawn(move || {
                    emit(
                        &root,
                        "container",
                        "start",
                        &format!("id{i:04}"),
                        &format!("nome-{i}"),
                        None,
                    );
                });
            }
        });
        let evs = read(&root);
        assert_eq!(
            evs.len(),
            N,
            "perderam-se ou corromperam-se eventos: {} de {N}",
            evs.len()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_from_continua_do_offset() {
        let root = tmp("off");
        emit(&root, "container", "create", "a1", "um", None);
        let (first, off) = read_from(&root, 0);
        assert_eq!(first.len(), 1);
        // With no new events, returns nothing (this is what `-f` needs).
        let (none, off2) = read_from(&root, off);
        assert!(none.is_empty());
        emit(&root, "container", "die", "a1", "um", None);
        let (novos, _) = read_from(&root, off2);
        assert_eq!(novos.len(), 1);
        assert_eq!(novos[0].action, "die");
        let _ = std::fs::remove_dir_all(&root);
    }
}
