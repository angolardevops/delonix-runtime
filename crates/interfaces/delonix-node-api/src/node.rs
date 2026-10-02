//! `GetNodeInfo`, `GetHealth` and `GetCapacity` (ADR-0042 step C): what this
//! node is, whether it can run workloads, and what room it has.
//!
//! Every answer is computed at call time from the same functions the CLI uses
//! (`delonix system info`, `provider ls`), never from a second copy of their
//! rules. A value that could not be measured is said so — `UNKNOWN` in a
//! condition, a name in `Capacity.unmeasured` — and never reported as zero or
//! as healthy: "unknown" and "no" have opposite remedies.

use std::path::{Path, PathBuf};

use delonix_compute::capability::{HealthStatus, ProviderKind, ProviderReport};
use delonix_model::records::Status;

use crate::proto::v1::{
    Capacity, Condition, ConditionStatus, FilesystemCapacity, Health, NodeInfo,
};

/// The engine's state root: the parent of the container store's default root
/// (`$DELONIX_ROOT`, else the rootless/root default) — the one rule
/// `delonix_state::Store` already owns.
pub fn state_root() -> PathBuf {
    delonix_state::Store::default_root()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// The contract's API identity (ADR-0042 D1).
pub const API_VERSION: &str = "delonix.node.v1";

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Backends whose VMs run ON this node. A remote backend's VMs (a Proxmox
/// node) are not this node's capacity, and reconciling them would be a round
/// trip per VM.
const LOCAL_VM_BACKENDS: &[&str] = &["libvirt", "cloud-hypervisor"];

fn compute(reports: &[ProviderReport], id: &str) -> Option<bool> {
    reports
        .iter()
        .find(|r| r.kind == ProviderKind::Compute && r.id == id)
        .map(|r| r.available)
}

/// The workload types this node can run right now, from the provider reports:
/// containers and pods with the Linux compute provider; VMs with any VM
/// backend available here; microVMs with Cloud Hypervisor.
pub fn supported_workload_types(reports: &[ProviderReport]) -> Vec<String> {
    let mut out = Vec::new();
    if compute(reports, "linux") == Some(true) {
        out.push("container".to_string());
        out.push("pod".to_string());
    }
    let vm = reports
        .iter()
        .any(|r| r.kind == ProviderKind::Compute && r.id != "linux" && r.available);
    if vm {
        out.push("vm".to_string());
    }
    if compute(reports, "cloud-hypervisor") == Some(true) {
        out.push("microvm".to_string());
    }
    out
}

/// `GetNodeInfo`.
pub fn node_info(reports: &[ProviderReport]) -> NodeInfo {
    NodeInfo {
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        engine_commit: env!("DELONIX_GIT_HASH").to_string(),
        api_version: API_VERSION.to_string(),
        hostname: read_trimmed("/proc/sys/kernel/hostname").unwrap_or_default(),
        kernel: read_trimmed("/proc/sys/kernel/osrelease").unwrap_or_default(),
        arch: std::env::consts::ARCH.to_string(),
        rootless: delonix_linux::is_rootless(),
        // The engine writes cgroupfs itself and never asks systemd for units —
        // the same answer the CRI gives the kubelet (`RuntimeConfig`).
        cgroup_driver: "cgroupfs".to_string(),
        cgroup_delegated: delonix_linux::cgroup_limits_apply(),
        supported_workload_types: supported_workload_types(reports),
    }
}

fn condition(kind: &str, status: ConditionStatus, reason: &str, message: String) -> Condition {
    Condition {
        r#type: kind.to_string(),
        status: status as i32,
        reason: reason.to_string(),
        message,
        last_transition_time: None,
    }
}

fn from_health(status: &HealthStatus) -> ConditionStatus {
    match status {
        HealthStatus::Healthy => ConditionStatus::True,
        HealthStatus::Unavailable => ConditionStatus::False,
        HealthStatus::Unknown => ConditionStatus::Unknown,
    }
}

/// Whether the store can be written, asked without writing anything: a health
/// check that created the state root or left a file would change the node it
/// reports on. `access(W_OK)` on the root, or — before the first workload
/// creates it — on its nearest existing ancestor (the root is then created on
/// demand). `access` answers for the permissions and for a read-only mount
/// (`EROFS`).
pub fn store_writable(root: &Path) -> Condition {
    use std::os::unix::ffi::OsStrExt;
    let existing = root
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(Path::new("/"));
    let writable = std::ffi::CString::new(existing.as_os_str().as_bytes())
        .ok()
        // SAFETY: a valid NUL-terminated path; `access` only reads it.
        .map(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0);
    let not_yet = existing != root;
    match writable {
        Some(true) if not_yet => condition(
            "StoreWritable",
            ConditionStatus::True,
            "CreatedOnDemand",
            format!(
                "{} does not exist yet; {} is writable and it is created with the first workload",
                root.display(),
                existing.display()
            ),
        ),
        Some(true) => condition(
            "StoreWritable",
            ConditionStatus::True,
            "Writable",
            format!("{} is writable", root.display()),
        ),
        _ => condition(
            "StoreWritable",
            ConditionStatus::False,
            "NotWritable",
            format!(
                "{}: {}",
                existing.display(),
                std::io::Error::last_os_error()
            ),
        ),
    }
}

/// The conditions of `GetHealth`, given the measured parts. Pure, so the
/// worst-of rule and each reason are tested without the host.
pub fn health_from(store: Condition, cgroup_delegated: bool, reports: &[ProviderReport]) -> Health {
    let cgroup = if cgroup_delegated {
        condition(
            "CgroupDelegated",
            ConditionStatus::True,
            "Delegated",
            "memory, cpu and pids limits are enforced".to_string(),
        )
    } else {
        condition(
            "CgroupDelegated",
            ConditionStatus::False,
            "NotDelegated",
            "memory, cpu and pids limits are NOT enforced: run under systemd-run --user \
             --scope -p Delegate=yes"
                .to_string(),
        )
    };
    // The network provider's own health (`provider ls`), not a second probe.
    let network = match reports
        .iter()
        .find(|r| r.kind == ProviderKind::Network && r.id == "linux")
    {
        Some(r) => condition(
            "NetworkReady",
            from_health(&r.health.status),
            r.health.reason,
            r.health.message.clone(),
        ),
        None => condition(
            "NetworkReady",
            ConditionStatus::Unknown,
            "NoReport",
            "the node's network provider gave no report".to_string(),
        ),
    };
    // The workloads this node runs itself need the Linux compute provider; a
    // backend that is merely not installed (or a remote one not configured) is
    // named, and does not make the node unhealthy.
    let providers = match compute(reports, "linux") {
        Some(true) => {
            let mut down: Vec<&str> = reports
                .iter()
                .filter(|r| r.kind == ProviderKind::Compute && !r.available)
                .map(|r| r.id.as_str())
                .collect();
            down.sort_unstable();
            let message = if down.is_empty() {
                "every compute provider is available".to_string()
            } else {
                format!(
                    "the linux compute provider is available; not available here: {}",
                    down.join(", ")
                )
            };
            condition(
                "ProvidersAvailable",
                ConditionStatus::True,
                "LinuxAvailable",
                message,
            )
        }
        Some(false) => condition(
            "ProvidersAvailable",
            ConditionStatus::False,
            "LinuxUnavailable",
            reports
                .iter()
                .find(|r| r.kind == ProviderKind::Compute && r.id == "linux")
                .map(|r| r.health.message.clone())
                .unwrap_or_default(),
        ),
        None => condition(
            "ProvidersAvailable",
            ConditionStatus::Unknown,
            "NoReport",
            "the linux compute provider gave no report".to_string(),
        ),
    };
    let conditions = vec![network, store, cgroup, providers];
    Health {
        overall: worst(&conditions) as i32,
        conditions,
    }
}

/// Overall = worst of the parts: any FALSE is FALSE, else any UNKNOWN is
/// UNKNOWN, else TRUE.
pub fn worst(conditions: &[Condition]) -> ConditionStatus {
    let has = |s: ConditionStatus| conditions.iter().any(|c| c.status == s as i32);
    if has(ConditionStatus::False) {
        ConditionStatus::False
    } else if has(ConditionStatus::Unknown) || conditions.is_empty() {
        ConditionStatus::Unknown
    } else {
        ConditionStatus::True
    }
}

/// `GetHealth`.
pub fn health(reports: &[ProviderReport]) -> Health {
    health_from(
        store_writable(&state_root()),
        delonix_linux::cgroup_limits_apply(),
        reports,
    )
}

/// A cgroup v2 limit file: `None` when unreadable, `Some(None)` for `max`.
fn cgroup_limit(path: &str) -> Option<Option<String>> {
    let v = read_trimmed(path)?;
    Some((v != "max").then_some(v))
}

/// `cpu.max` (`<quota> <period>` or `max <period>`) as millicores; `max` is
/// the whole host.
pub fn cpu_max_millis(raw: &str, total: i64) -> Option<i64> {
    let mut parts = raw.split_whitespace();
    let quota = parts.next()?;
    let period: i64 = parts.next().unwrap_or("100000").parse().ok()?;
    if quota == "max" {
        return Some(total);
    }
    let quota: i64 = quota.parse().ok()?;
    (period > 0).then(|| (quota * 1000 / period).min(total))
}

/// The mount point holding `path`: the highest ancestor on the same device.
fn mountpoint(path: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let Ok(dev) = std::fs::metadata(path).map(|m| m.dev()) else {
        return path.to_path_buf();
    };
    let mut cur = path.to_path_buf();
    while let Some(parent) = cur.parent() {
        match std::fs::metadata(parent) {
            Ok(m) if m.dev() == dev => cur = parent.to_path_buf(),
            _ => break,
        }
    }
    cur
}

fn filesystem(role: &str, path: &Path) -> Option<FilesystemCapacity> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` is a plain C struct of integers; all-zero is a valid
    // value for it.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` an out-param statvfs
    // fills on success; nothing is read from it on failure.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as i64;
    Some(FilesystemCapacity {
        role: role.to_string(),
        mountpoint: mountpoint(path).display().to_string(),
        bytes_total: st.f_blocks as i64 * frsize,
        bytes_available: st.f_bavail as i64 * frsize,
        inodes_total: st.f_files as i64,
        inodes_available: st.f_favail as i64,
    })
}

/// `GetCapacity`.
pub fn capacity() -> Capacity {
    let mut c = Capacity::default();
    let mut unmeasured: Vec<&str> = Vec::new();
    let slice = delonix_linux::slice_path();
    let limit = |file: &str| {
        slice
            .as_deref()
            .and_then(|s| cgroup_limit(&format!("{s}/{file}")))
    };

    let ncpu = delonix_linux::host_ncpu() as i64;
    if ncpu > 0 {
        c.cpu_millis_total = ncpu * 1000;
        match slice
            .as_deref()
            .and_then(|s| read_trimmed(&format!("{s}/cpu.max")))
            .and_then(|raw| cpu_max_millis(&raw, c.cpu_millis_total))
        {
            Some(m) => c.cpu_millis_allocatable = m,
            None => unmeasured.push("cpu_millis_allocatable"),
        }
    } else {
        unmeasured.extend(["cpu_millis_total", "cpu_millis_allocatable"]);
    }

    let mem = delonix_linux::host_mem_bytes() as i64;
    if mem > 0 {
        c.memory_bytes_total = mem;
        match limit("memory.max") {
            Some(None) => c.memory_bytes_allocatable = mem,
            Some(Some(v)) => match v.parse::<i64>() {
                Ok(b) => c.memory_bytes_allocatable = b.min(mem),
                Err(_) => unmeasured.push("memory_bytes_allocatable"),
            },
            None => unmeasured.push("memory_bytes_allocatable"),
        }
    } else {
        unmeasured.extend(["memory_bytes_total", "memory_bytes_allocatable"]);
    }

    let pid_max = read_trimmed("/proc/sys/kernel/pid_max").and_then(|v| v.parse::<i64>().ok());
    match (limit("pids.max"), pid_max) {
        (Some(Some(v)), _) if v.parse::<i64>().is_ok() => {
            c.pids_allocatable = v.parse::<i64>().unwrap_or_default();
        }
        (Some(None), Some(max)) => c.pids_allocatable = max,
        _ => unmeasured.push("pids_allocatable"),
    }

    let root = state_root();
    for (role, dir) in [
        ("state", root.clone()),
        ("images", root.join("blobs")),
        ("volumes", root.join("volumes")),
        ("vm-images", root.join("vm-images")),
    ] {
        // A store not created yet has no filesystem to report: absent, not
        // unmeasured. One that exists and cannot be read IS unmeasured.
        if !dir.exists() {
            continue;
        }
        match filesystem(role, &dir) {
            Some(fs) => c.filesystems.push(fs),
            None => unmeasured.push(match role {
                "state" => "filesystems.state",
                "images" => "filesystems.images",
                "volumes" => "filesystems.volumes",
                _ => "filesystems.vm-images",
            }),
        }
    }

    match delonix_state::Store::open(root.join("containers")).and_then(|s| s.list()) {
        Ok(cs) => {
            c.containers_running = cs
                .iter()
                .filter(|x| matches!(x.status, Status::Running))
                .count() as i32
        }
        Err(_) => unmeasured.push("containers_running"),
    }

    match delonix_state::JsonStore::<delonix_compute::Vm>::open(root.join("vms"))
        .and_then(|s| s.list())
    {
        Ok(vms) => {
            c.vms_running = vms
                .iter()
                .filter(|v| LOCAL_VM_BACKENDS.contains(&v.backend.as_str()))
                .filter(|v| {
                    // Reconciled one by one, local backends only (a record
                    // can say Running for a VM whose process died).
                    delonix_vm::status(&root, &v.name)
                        .map(|s| matches!(s.status, Status::Running))
                        .unwrap_or(false)
                })
                .count() as i32
        }
        Err(_) => unmeasured.push("vms_running"),
    }

    c.unmeasured = unmeasured.into_iter().map(str::to_string).collect();
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use delonix_compute::capability::ProviderHealth;

    fn report(
        kind: ProviderKind,
        id: &str,
        available: bool,
        status: HealthStatus,
    ) -> ProviderReport {
        ProviderReport {
            id: id.to_string(),
            kind,
            available,
            health: ProviderHealth {
                status,
                reason: if available { "Ready" } else { "Missing" },
                message: format!("{id} message"),
            },
            capabilities: Vec::new(),
        }
    }

    fn host(ch: bool) -> Vec<ProviderReport> {
        vec![
            report(ProviderKind::Compute, "linux", true, HealthStatus::Healthy),
            report(
                ProviderKind::Compute,
                "libvirt",
                true,
                HealthStatus::Healthy,
            ),
            report(
                ProviderKind::Compute,
                "cloud-hypervisor",
                ch,
                HealthStatus::Healthy,
            ),
            report(
                ProviderKind::Compute,
                "proxmox",
                false,
                HealthStatus::Unknown,
            ),
            report(ProviderKind::Network, "linux", true, HealthStatus::Healthy),
        ]
    }

    fn writable() -> Condition {
        condition(
            "StoreWritable",
            ConditionStatus::True,
            "Writable",
            String::new(),
        )
    }

    #[test]
    fn workload_types_follow_the_providers_available_here() {
        assert_eq!(
            supported_workload_types(&host(true)),
            vec!["container", "pod", "vm", "microvm"]
        );
        assert_eq!(
            supported_workload_types(&host(false)),
            vec!["container", "pod", "vm"]
        );
        let mut no_linux = host(false);
        no_linux[0].available = false;
        no_linux[1].available = false;
        assert!(supported_workload_types(&no_linux).is_empty());
    }

    #[test]
    fn health_is_the_worst_of_its_parts_and_names_what_is_missing() {
        let h = health_from(writable(), true, &host(false));
        assert_eq!(h.overall, ConditionStatus::True as i32);
        let reasons: Vec<&str> = h.conditions.iter().map(|c| c.r#type.as_str()).collect();
        assert_eq!(
            reasons,
            vec![
                "NetworkReady",
                "StoreWritable",
                "CgroupDelegated",
                "ProvidersAvailable"
            ]
        );
        let p = &h.conditions[3];
        assert!(
            p.message.contains("cloud-hypervisor, proxmox"),
            "{}",
            p.message
        );

        // Undelegated cgroups: FALSE, and the overall follows.
        let h = health_from(writable(), false, &host(false));
        assert_eq!(h.overall, ConditionStatus::False as i32);
        assert!(h.conditions[2].message.contains("Delegate=yes"));

        // A network provider that could not tell: UNKNOWN, never TRUE.
        let mut r = host(false);
        r[4].health.status = HealthStatus::Unknown;
        assert_eq!(
            health_from(writable(), true, &r).overall,
            ConditionStatus::Unknown as i32
        );

        // No linux compute provider at all.
        let mut r = host(false);
        r[0].available = false;
        let h = health_from(writable(), true, &r);
        assert_eq!(h.conditions[3].reason, "LinuxUnavailable");
        assert_eq!(h.overall, ConditionStatus::False as i32);
    }

    #[test]
    fn the_store_check_writes_nothing_and_says_what_it_found() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let c = store_writable(dir.path());
        assert_eq!(
            (c.status, c.reason.as_str()),
            (ConditionStatus::True as i32, "Writable")
        );
        let missing = dir.path().join("not/yet");
        let c = store_writable(&missing);
        assert_eq!(c.reason, "CreatedOnDemand");
        assert!(!missing.exists(), "a health check must not create the root");
        assert!(
            std::fs::read_dir(dir.path()).unwrap().next().is_none(),
            "nothing written"
        );
        // Read-only for the owner (root ignores modes: only checked as non-root).
        if !delonix_linux::is_rootless() {
            return;
        }
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let c = store_writable(&ro);
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            (c.status, c.reason.as_str()),
            (ConditionStatus::False as i32, "NotWritable")
        );
    }

    #[test]
    fn cpu_max_reads_as_millicores() {
        assert_eq!(cpu_max_millis("max 100000", 8000), Some(8000));
        assert_eq!(cpu_max_millis("150000 100000", 8000), Some(1500));
        assert_eq!(cpu_max_millis("900000 100000", 8000), Some(8000));
        assert_eq!(cpu_max_millis("garbage", 8000), None);
        assert_eq!(cpu_max_millis("100000 0", 8000), None);
    }

    #[test]
    fn worst_never_reads_nothing_as_healthy() {
        assert_eq!(worst(&[]), ConditionStatus::Unknown);
    }
}
