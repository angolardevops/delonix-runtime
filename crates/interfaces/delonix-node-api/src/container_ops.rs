//! `ContainerService`, the write half (first pass): `StopContainer`,
//! `KillContainer` and `DeleteContainer`.
//!
//! The work is `delonix_linux` (`stop`/`send_signal`/`remove`) over the same
//! `delonix_state::Store` the CLI drives — no shelling out. `Stop` and `Kill`
//! are synchronous and answer with the updated `Container` (the resource, like
//! a VM `Pause`/`Resume`); `Delete` answers with an [`Operation`] persisted in
//! the ledger before the work starts, replayable by `request_id`, exactly as
//! [`crate::network_ops::delete_in`] does. `StartContainer`/`CreateContainer`
//! (image/rootfs resolution, spec lowering, the fork-safe launch) live in their
//! own module, [`crate::container_launch`].

use std::path::Path;

use tonic::Status;

use crate::network_ops::{stale_etag, status_of};
use crate::operations::{self, Begun};
use crate::proto::v1::{
    Container, ContainerStats, DeleteContainerRequest, KillContainerRequest, Operation,
    StatsRequest, StopContainerRequest,
};

/// The engine's persisted container record (not the proto message).
type Record = delonix_compute::Container;

/// Default grace for a `Stop` whose request carries no `grace_period`, the
/// same 10s the CLI's `container stop` uses before escalating to `SIGKILL`.
const DEFAULT_GRACE_SECS: u64 = 10;

fn requested_namespace(namespace: &str) -> &str {
    if namespace.is_empty() {
        "default"
    } else {
        namespace
    }
}

fn store(root: &Path) -> Result<delonix_state::Store, Status> {
    delonix_state::Store::open(root.join("containers")).map_err(|e| status_of(&e.into()))
}

/// Load a container by name under `root`, requiring its recorded namespace to
/// match what was asked — the same identity rule [`crate::containers`] reads
/// with.
fn load(root: &Path, name: &str, namespace: &str) -> Result<(delonix_state::Store, Record), Status> {
    let wanted = requested_namespace(namespace);
    let missing = || Status::not_found(format!("container '{name}' in namespace '{wanted}'"));
    if wanted == "*" || !root.join("containers").is_dir() {
        return Err(missing());
    }
    let s = store(root)?;
    let c = s.load(name).map_err(|e| {
        if e.is_not_found() {
            missing()
        } else {
            status_of(&e.into())
        }
    })?;
    if c.namespace != wanted {
        return Err(missing());
    }
    Ok((s, c))
}

/// A signal name (`SIGTERM`, `TERM`) or number (`15`) → its number. Kept local
/// (no `libc` dependency in this crate): the common set a `kill` sends. An
/// empty signal is `SIGKILL`, the default `docker kill` uses.
fn parse_signal(spec: &str) -> Result<i32, Status> {
    if spec.is_empty() {
        return Ok(9); // SIGKILL
    }
    if let Ok(n) = spec.parse::<i32>() {
        return Ok(n);
    }
    let upper = spec.to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    Ok(match name {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "KILL" => 9,
        "USR1" => 10,
        "USR2" => 12,
        "TERM" => 15,
        "CONT" => 18,
        "STOP" => 19,
        "TSTP" => 20,
        _ => {
            return Err(Status::invalid_argument(format!(
                "KillContainer: unknown signal '{spec}'"
            )))
        }
    })
}

/// `StopContainer`: graceful stop, then the updated record.
pub fn stop_in(root: &Path, req: &StopContainerRequest) -> Result<Container, Status> {
    let (s, mut c) = load(root, &req.name, &req.namespace)?;
    let grace = req
        .grace_period
        .as_ref()
        .map(|d| d.seconds.max(0) as u64)
        .unwrap_or(DEFAULT_GRACE_SECS);
    delonix_linux::stop(&s, &mut c, grace).map_err(|e| status_of(&e.into()))?;
    delonix_linux::reconcile_status(&mut c);
    Ok(crate::containers::message(&c))
}

/// `KillContainer`: send one signal, then the updated record.
pub fn kill_in(root: &Path, req: &KillContainerRequest) -> Result<Container, Status> {
    let (_s, mut c) = load(root, &req.name, &req.namespace)?;
    let sig = parse_signal(&req.signal)?;
    delonix_linux::send_signal(&c, sig).map_err(|e| status_of(&e.into()))?;
    delonix_linux::reconcile_status(&mut c);
    Ok(crate::containers::message(&c))
}

/// `DeleteContainer`: an [`Operation`] persisted before the work, replayable by
/// `request_id`. `force` stops a running container first (the engine's
/// `remove` does); without it, removing a running container is the engine's
/// refusal, as the operation's `FAILED`.
pub fn delete_in(root: &Path, req: &DeleteContainerRequest) -> Result<Operation, Status> {
    let target = format!("Container/{}", req.name);
    // Before "not found", which the first request itself made true.
    if let Some(found) = operations::replay(root, "delete", &target, &req.request_id, "")? {
        return Ok(operations::message(&found));
    }
    let (s, c) = load(root, &req.name, &req.namespace)?;
    if !req.etag.is_empty() {
        let current = crate::containers::message(&c)
            .meta
            .map(|m| m.etag)
            .unwrap_or_default();
        if current != req.etag {
            return Err(stale_etag(format!(
                "container '{}' changed since etag '{}' was read (it is now '{current}')",
                req.name, req.etag
            )));
        }
    }
    let rec = match operations::begin(root, "delete", &target, &req.request_id, "")? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = delonix_linux::remove(&s, &c, req.force)
        .map(|()| String::new())
        .map_err(|e| e.into());
    Ok(operations::message(&operations::finish(root, rec, outcome)?))
}

/// `Stats`: the container's live cgroup counters as the contract's
/// `ContainerStats`. CPU and working-set come from `delonix_linux::container_usage`
/// (best-effort — a stopped container reads back all-`None`, reported in
/// `unmeasured`, never a fabricated zero). The configured memory limit, pids,
/// network and writable-layer are not wired in this pass and are named in
/// `unmeasured` so a client distinguishes "zero" from "not measured". The
/// counters are cumulative: the client computes the rate across two reads.
pub fn stats_in(root: &Path, req: &StatsRequest) -> Result<ContainerStats, Status> {
    let (_s, c) = load(root, &req.name, &req.namespace)?;
    let usage = delonix_linux::container_usage(&c);
    let mut unmeasured = Vec::new();
    let cpu_usage_nanos = match usage.cpu_usage_usec {
        Some(us) => (us as i64).saturating_mul(1000),
        None => {
            unmeasured.push("cpu_usage".to_string());
            0
        }
    };
    let memory_working_set_bytes = match usage.mem_bytes {
        Some(b) => b as i64,
        None => {
            unmeasured.push("memory_working_set".to_string());
            0
        }
    };
    for f in [
        "memory_limit",
        "pids",
        "network_rx",
        "network_tx",
        "writable_layer",
    ] {
        unmeasured.push(f.to_string());
    }
    Ok(ContainerStats {
        cpu_usage_nanos,
        memory_working_set_bytes,
        memory_limit_bytes: 0,
        pids: 0,
        network_rx_bytes: 0,
        network_tx_bytes: 0,
        writable_layer_bytes: 0,
        unmeasured,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A container record of `name`/`namespace`, `Created` (never started, so
    /// no write-path touches a real process), saved under `root/containers`.
    fn seed(root: &Path, id: &str, name: &str, namespace: &str) {
        let s = delonix_state::Store::open(root.join("containers")).unwrap();
        let mut c = Record::new(
            id.to_string(),
            name.to_string(),
            "busybox".to_string(),
            vec!["sleep".to_string(), "1".to_string()],
            "64M".to_string(),
        );
        c.namespace = namespace.to_string();
        s.save(&c).unwrap();
    }

    #[test]
    fn parse_signal_takes_names_numbers_and_defaults_empty_to_kill() {
        assert_eq!(parse_signal("").unwrap(), 9);
        assert_eq!(parse_signal("9").unwrap(), 9);
        assert_eq!(parse_signal("SIGTERM").unwrap(), 15);
        assert_eq!(parse_signal("term").unwrap(), 15);
        assert_eq!(parse_signal("HUP").unwrap(), 1);
        assert_eq!(
            parse_signal("NOPE").unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn a_missing_container_is_not_found_on_every_write_verb() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for status in [
            stop_in(
                root,
                &StopContainerRequest {
                    name: "ghost".into(),
                    namespace: "default".into(),
                    grace_period: None,
                },
            )
            .unwrap_err(),
            kill_in(
                root,
                &KillContainerRequest {
                    name: "ghost".into(),
                    namespace: "default".into(),
                    signal: "TERM".into(),
                },
            )
            .unwrap_err(),
            delete_in(
                root,
                &DeleteContainerRequest {
                    name: "ghost".into(),
                    namespace: "default".into(),
                    ..Default::default()
                },
            )
            .unwrap_err(),
        ] {
            assert_eq!(status.code(), tonic::Code::NotFound, "{status:?}");
        }
    }

    #[test]
    fn a_container_of_another_namespace_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root, "a1b2c3d4e5f60718", "db", "team-a");
        let err = stop_in(
            root,
            &StopContainerRequest {
                name: "db".into(),
                namespace: "default".into(),
                grace_period: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[test]
    fn stats_of_a_never_run_container_reports_everything_unmeasured() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root, "0123456789abcdef", "idle", "default");
        let s = stats_in(
            root,
            &StatsRequest {
                name: "idle".into(),
                namespace: "default".into(),
            },
        )
        .unwrap();
        // A Created container has no live cgroup: cpu and working-set read back
        // unmeasured, and the fields this pass does not wire are named too.
        assert!(s.unmeasured.contains(&"cpu_usage".to_string()));
        assert!(s.unmeasured.contains(&"memory_working_set".to_string()));
        assert!(s.unmeasured.contains(&"pids".to_string()));
        assert_eq!(s.cpu_usage_nanos, 0);
    }

    #[test]
    fn delete_refuses_a_stale_etag_before_touching_anything() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root, "0123456789abcdef", "web", "default");
        let err = delete_in(
            root,
            &DeleteContainerRequest {
                name: "web".into(),
                namespace: "default".into(),
                etag: "not-the-current-one".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        // The record is still there — nothing was removed.
        assert!(crate::containers::get_in(
            root,
            &crate::proto::v1::GetContainerRequest {
                name: "web".into(),
                namespace: "default".into(),
            }
        )
        .is_ok());
    }
}
