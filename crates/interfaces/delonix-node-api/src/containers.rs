//! `ContainerService`, the read half: `GetContainer` and `ListContainers`
//! (ADR-0040 — the container slice of the node contract that the Desktop needs
//! to stop shelling out to `delonix container ls`/`inspect`).
//!
//! A container's name is unique at the NODE level (one flat `containers/`
//! store, read by `delonix_state::Store`), so a read is: find the record by
//! name, then require its recorded `namespace` to match what was asked —
//! exactly how [`crate::vms`] reads a VM. Liveness is refreshed in memory with
//! `delonix_linux::reconcile_status` before the record is mapped, so a record
//! whose init died since the last write reports `Exited`/`Dead`, not a stale
//! `Running`. The refresh is NOT persisted here: a read must not write.

use std::path::Path;

use tonic::Status;

use crate::page::fnv;
use crate::proto::v1::{
    Condition, ConditionStatus, Container, ContainerPhase, ContainerSpec, ContainerStatus,
    GetContainerRequest, ListContainersRequest, ListContainersResponse, PageResponse, ResourceMeta,
};
use crate::selector::Selector;

/// The engine's persisted container record (not the proto message).
type Record = delonix_compute::Container;

/// Which namespace a container is reported under: an explicit name (checked
/// against the record), the empty string (→ `default`), or `*` (all).
fn requested_namespace(namespace: &str) -> &str {
    if namespace.is_empty() {
        "default"
    } else {
        namespace
    }
}

fn timestamp_secs(secs: u64) -> pbjson_types::Timestamp {
    pbjson_types::Timestamp {
        seconds: secs as i64,
        nanos: 0,
    }
}

/// `KEY=value` lines (the engine's storage form) → the proto's `map`. A line
/// with no `=` is a bare key with an empty value, the same reading `inspect`
/// gives it.
fn env_map(env: &[String]) -> std::collections::HashMap<String, String> {
    env.iter()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (kv.clone(), String::new()),
        })
        .collect()
}

/// The phase, plus the exit code and reason that only mean something once the
/// container has stopped. `Failed(code)` carries the observed code; a clean
/// `Stopped` is exit 0; `Crashed` has no code to show (killed/OOM/vanished).
fn phase_of(status: &delonix_model::records::Status) -> (ContainerPhase, Option<i32>, &'static str) {
    use delonix_model::records::Status as S;
    match status {
        S::Created => (ContainerPhase::Created, None, ""),
        S::Running => (ContainerPhase::Running, None, ""),
        S::Paused => (ContainerPhase::Paused, None, ""),
        S::Stopped => (ContainerPhase::Exited, Some(0), "Completed"),
        S::Failed(code) => (ContainerPhase::Exited, Some(*code), "Error"),
        S::Crashed => (ContainerPhase::Dead, None, "Crashed"),
    }
}

/// One record as its proto message. The spec reports what the engine actually
/// persists and a reader needs to see (image, command, env, workdir); the
/// fuller run spec (mounts, ports, resources, networks) is a later pass — an
/// unset field means "not reported", the same meaning the proto gives create.
pub fn message(c: &Record) -> Container {
    let (phase, exit_code, reason) = phase_of(&c.status);
    let condition = match phase {
        ContainerPhase::Running => Condition {
            r#type: "Ready".into(),
            status: ConditionStatus::True as i32,
            reason: "Running".into(),
            message: "the init process is alive".into(),
            last_transition_time: None,
        },
        _ => Condition {
            r#type: "Ready".into(),
            status: ConditionStatus::False as i32,
            reason: format!("{phase:?}"),
            message: format!("the container is not running ({})", c.status),
            last_transition_time: None,
        },
    };
    Container {
        meta: Some(ResourceMeta {
            name: c.name.clone(),
            namespace: c.namespace.clone(),
            labels: c.labels.clone().into_iter().collect(),
            annotations: c.annotations.clone().into_iter().collect(),
            etag: fnv(&format!("{c:?}")),
            create_time: Some(timestamp_secs(c.created_unix)),
            ..Default::default()
        }),
        spec: Some(ContainerSpec {
            image: c.image.clone(),
            command: c.command.clone(),
            env: env_map(&c.env),
            workdir: c.workdir.clone().unwrap_or_default(),
            user: String::new(),
            read_only_rootfs: c.read_only,
            pod: c.pod.clone().unwrap_or_default(),
            ..Default::default()
        }),
        status: Some(ContainerStatus {
            phase: phase as i32,
            pid: c.pid.unwrap_or(0),
            exit_code,
            reason: reason.to_string(),
            ip_addresses: c.ip.clone().into_iter().filter(|s| !s.is_empty()).collect(),
            conditions: vec![condition],
            image_digest: String::new(),
        }),
    }
}

/// The container records under `root`, liveness-reconciled. A root where no
/// container was ever created has no `containers/` directory — an empty list,
/// which reading must not materialise.
pub fn records(root: &Path) -> Result<Vec<Record>, Status> {
    if !root.join("containers").is_dir() {
        return Ok(Vec::new());
    }
    let store = delonix_state::Store::open(root.join("containers"))
        .map_err(|e| crate::network_ops::status_of(&e.into()))?;
    let mut all = store
        .list()
        .map_err(|e| crate::network_ops::status_of(&e.into()))?;
    for c in &mut all {
        delonix_linux::reconcile_status(c);
    }
    Ok(all)
}

/// `GetContainer` under `root`.
pub fn get_in(root: &Path, req: &GetContainerRequest) -> Result<Container, Status> {
    let wanted = requested_namespace(&req.namespace);
    let missing = || Status::not_found(format!("container '{}' in namespace '{wanted}'", req.name));
    if wanted == "*" {
        return Err(missing());
    }
    if !root.join("containers").is_dir() {
        return Err(missing());
    }
    let store = delonix_state::Store::open(root.join("containers"))
        .map_err(|e| crate::network_ops::status_of(&e.into()))?;
    let mut c = store.load(&req.name).map_err(|e| {
        if e.is_not_found() {
            missing()
        } else {
            crate::network_ops::status_of(&e.into())
        }
    })?;
    if c.namespace != wanted {
        return Err(missing());
    }
    delonix_linux::reconcile_status(&mut c);
    Ok(message(&c))
}

/// `ListContainers` under `root`: filtered by `label_selector` and the
/// requested namespace, ordered by name, one page at a time.
pub fn list_in(root: &Path, req: &ListContainersRequest) -> Result<ListContainersResponse, Status> {
    let selector = Selector::parse(&req.label_selector).map_err(Status::invalid_argument)?;
    let page = req.page.clone().unwrap_or_default();
    let size = crate::page::size(&page)?;
    let after = crate::page::after(&page)?;
    let ns = requested_namespace(&req.namespace);
    let mut all = records(root)?;
    all.sort_by(|a, b| a.name.cmp(&b.name));
    let mut matching = all
        .iter()
        .filter(|c| ns == "*" || c.namespace == ns)
        .filter(|c| selector.matches(&c.labels))
        .filter(|c| after.as_ref().is_none_or(|a| c.name.as_str() > a.as_str()));
    let containers: Vec<Container> = matching.by_ref().take(size).map(message).collect();
    let next = match (matching.next(), containers.last()) {
        (Some(_), Some(last)) => crate::page::encode_token(
            last.meta.as_ref().map(|m| m.name.as_str()).unwrap_or_default(),
        ),
        _ => String::new(),
    };
    Ok(ListContainersResponse {
        containers,
        page: Some(PageResponse {
            next_page_token: next,
        }),
    })
}
