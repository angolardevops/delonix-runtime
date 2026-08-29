//! Local append-only audit log (ADR-0025 §8) — one JSON line per tool call under
//! `$DELONIX_ROOT/mcp/audit.log`, `0600`. No central Delonix audit pipeline exists
//! in this repo (that is a `delonix-paas`/platform concept); this is a record, not
//! a source of truth, in the same spirit as the stack revision history (ADR-0019).

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Serialize)]
pub struct AuditEvent<'a> {
    pub timestamp_unix: u64,
    pub principal: &'a str,
    pub tool: &'a str,
    pub risk: &'a str,
    /// "allowed" | "denied"
    pub policy_decision: &'a str,
    /// "ok" | "error"
    pub result: &'a str,
    pub arguments_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub fn open(root: &Path) -> std::io::Result<Self> {
        let dir = root.join("mcp");
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            path: dir.join("audit.log"),
        })
    }

    /// Best-effort: a full disk or a permission problem on the audit log must
    /// never fail the tool call it is trying to log — that would make the audit
    /// trail a new way to break the runtime, which is worse than an audit gap.
    pub fn append(&self, event: &AuditEvent<'_>) {
        let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)
        else {
            return;
        };
        if let Ok(line) = serde_json::to_string(event) {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn tail(&self, limit: usize) -> Vec<Value> {
        let Ok(content) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        content
            .lines()
            .rev()
            .take(limit)
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Digest of the call's arguments, for audit correlation — never the arguments
/// themselves, so a secret accidentally passed as a tool argument cannot leak
/// into the audit trail.
pub fn hash_args(value: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.to_string().as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_args_never_contains_the_secret_it_hashes() {
        let secret = serde_json::json!({ "password": "hunter2" });
        let digest = hash_args(&secret);
        assert!(!digest.contains("hunter2"));
        assert!(digest.starts_with("sha256:"));
    }

    #[test]
    fn append_and_tail_round_trip_in_order_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        for i in 0..3 {
            log.append(&AuditEvent {
                timestamp_unix: 1000 + i,
                principal: "uid:1000",
                tool: "runtime.info",
                risk: "READ",
                policy_decision: "allowed",
                result: "ok",
                arguments_hash: hash_args(&serde_json::json!({ "i": i })),
                resource: None,
                task_id: None,
                message: None,
            });
        }
        let events = log.tail(2);
        assert_eq!(events.len(), 2);
        // Most-recent first.
        assert_eq!(events[0]["timestamp_unix"], 1002);
        assert_eq!(events[1]["timestamp_unix"], 1001);

        let meta = std::fs::metadata(dir.path().join("mcp").join("audit.log")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn tail_on_a_missing_log_is_an_empty_list_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        assert!(log.tail(10).is_empty());
    }
}
