//! CRI lifecycle (pods + containers) over the Delonix engine.
//!
//! Strategy: the CRI state (sandboxes/containers) lives in JSON files under
//! `<base>/cri/`; the operations that use `clone` (run/stop/rm) **delegate to
//! the `delonix` binary** (single-threaded, already-verified logic), because the
//! CRI server is multi-threaded (Tokio) and `clone` is not safe outside a single
//! thread. The runtime STATE is read directly from Delonix's `Store`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tonic::{Response, Status};

use crate::cri::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default)]
struct SandboxRec {
    id: String,
    name: String,
    namespace: String,
    uid: String,
    attempt: u32,
    created_at: i64,
    /// Pod hostname (`PodSandboxConfig.hostname`) — applied to each container of
    /// the sandbox via `delonix run --hostname`. Empty only when the network is
    /// the NODE's.
    #[serde(default)]
    hostname: String,
    log_directory: String,
    #[serde(default)]
    stopped: bool,
    labels: HashMap<String, String>,
    annotations: HashMap<String, String>,
    /// `true` if the pod uses the NODE's network (host network); then there is NO
    /// own infra/netns and the containers run on the host's network.
    #[serde(default)]
    host_network: bool,
    /// Shares the host's PID/IPC namespace (`namespace_options.{pid,ipc} = NODE`).
    #[serde(default)]
    host_pid: bool,
    #[serde(default)]
    host_ipc: bool,
    /// Pod `sysctl`s (`key=value`), applied to the sandbox's containers.
    #[serde(default)]
    sysctls: Vec<String>,
    /// The kubelet's `linux.cgroup_parent` for this pod (ADR 0038), validated at
    /// `RunPodSandbox`. Empty = the kubelet sent none (a bare `crictl`), and the containers
    /// keep the engine's own placement and house ceiling.
    #[serde(default)]
    cgroup_parent: String,
    /// The pod's `DNSConfig` and `PortMappings`, both of which were read by
    /// nobody. Same shape of gap as the container mounts: accepted by the API,
    /// dropped on the floor, and invisible because nothing errored.
    #[serde(default)]
    dns_servers: Vec<String>,
    #[serde(default)]
    dns_searches: Vec<String>,
    #[serde(default)]
    dns_options: Vec<String>,
    #[serde(default)]
    port_mappings: Vec<String>,
    /// IP (address, without CIDR) assigned by the CNI IPAM when the sandbox was
    /// configured by CNI plugins (rootless via the holder, or root in the host).
    /// Empty = native SDN.
    #[serde(default)]
    cni_ip: String,
    /// ROOT + CNI: the sandbox's named netns in the HOST (`/run/netns/cri-<id>`),
    /// which its containers enter. Empty on every other path.
    #[serde(default)]
    cni_netns: String,
    /// The conflist (JSON) the sandbox was configured with, so the `DEL` runs the
    /// same chain — and frees the same IPAM lease — even if `/etc/cni/net.d`
    /// changed in the meantime.
    #[serde(default)]
    cni_conf: String,
}

/// `NODE` when the sandbox shares the host's namespace, `POD` otherwise — the
/// same two values `run_pod_sandbox` decodes on the way in (`is_node`).
fn ns_mode(host: bool) -> i32 {
    if host {
        NamespaceMode::Node as i32
    } else {
        NamespaceMode::Pod as i32
    }
}

fn sandbox_state(r: &SandboxRec) -> i32 {
    if r.stopped {
        PodSandboxState::SandboxNotready as i32
    } else {
        PodSandboxState::SandboxReady as i32
    }
}

/// The CRI's `LinuxContainerResources`, kept field by field so `start_run_opts` can
/// turn them into `container run` flags. `0`/empty means «not specified», the
/// CRI's own convention.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
struct CriResources {
    #[serde(default)]
    memory_limit_in_bytes: i64,
    #[serde(default)]
    cpu_quota: i64,
    #[serde(default)]
    cpu_period: i64,
    #[serde(default)]
    cpu_shares: i64,
    #[serde(default)]
    cpuset_cpus: String,
    /// ADR 0038 item 3. `0` is indistinguishable, at the wire, from "not
    /// set" (the CRI proto field is a bare `int64`, no `Option`) — treated
    /// the same way `memory_limit_in_bytes`/`cpu_shares`/`cpuset_cpus` above
    /// already treat their own zero/empty: "nothing to apply", which is
    /// also the correct behavior for a kubelet that genuinely does not care.
    #[serde(default)]
    oom_score_adj: i64,
    /// ADR 0038 item 3, `LinuxContainerResources.cpuset_mems`. "" = not set.
    #[serde(default)]
    cpuset_mems: String,
    /// ADR 0038 item 3, `LinuxContainerResources.hugepage_limits`, kept as
    /// `(page_size, limit)` pairs — the exact shape `hugetlb.<page_size>.limit_in_bytes`
    /// needs, with no intermediate type.
    #[serde(default)]
    hugepage_limits: Vec<(String, u64)>,
    /// ADR 0038 item 3, `LinuxContainerResources.unified` — raw cgroup v2
    /// `(file, value)` pairs (e.g. `("memory.high", "100000000")`), sorted by
    /// key so two requests with the same map never compare unequal because a
    /// `HashMap` iterated them in a different order.
    #[serde(default)]
    unified: Vec<(String, String)>,
}

impl CriResources {
    fn from_cri(r: Option<&LinuxContainerResources>) -> Self {
        r.map(|r| {
            let mut unified: Vec<(String, String)> = r
                .unified
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            unified.sort();
            CriResources {
                memory_limit_in_bytes: r.memory_limit_in_bytes,
                cpu_quota: r.cpu_quota,
                cpu_period: r.cpu_period,
                cpu_shares: r.cpu_shares,
                cpuset_cpus: r.cpuset_cpus.clone(),
                oom_score_adj: r.oom_score_adj,
                cpuset_mems: r.cpuset_mems.clone(),
                hugepage_limits: drop_vacuous_zero_hugepage_limits(
                    r.hugepage_limits
                        .iter()
                        .map(|h| (h.page_size.clone(), h.limit))
                        .collect(),
                    host_hugepage_pool,
                ),
                unified,
            }
        })
        .unwrap_or_default()
    }

    /// PURE. ADR 0038 item 4: `UpdateContainerResources` is a PARTIAL
    /// request — a field `update` leaves unspecified (0/empty) means "not
    /// touched by this call", never "clear it" (see [`ResourceUpdate`]'s own
    /// doc for why this differs from `CreateContainer`'s reading of the same
    /// zero). Overwriting `self` with `update` wholesale would silently
    /// erase every OTHER field the container was created with the moment any
    /// one field is updated — this merges field by field instead, so the
    /// persisted record keeps meaning the container's FULL current desired
    /// state, the shape `start_run_opts` already expects.
    ///
    /// `cpu_quota`/`cpu_period` are kept as a PAIR, deliberately: a caller
    /// that updates one without the other is not a shape any real kubelet or
    /// `crictl update` produces (the two are always sent together), and
    /// merging them independently could leave a quota paired with the OLD
    /// period, silently changing the core count nobody asked to change.
    fn merge_update(&self, update: &CriResources) -> CriResources {
        let mut merged = self.clone();
        if update.memory_limit_in_bytes != 0 {
            merged.memory_limit_in_bytes = update.memory_limit_in_bytes;
        }
        if update.cpu_quota != 0 || update.cpu_period != 0 {
            merged.cpu_quota = update.cpu_quota;
            merged.cpu_period = update.cpu_period;
        }
        if update.cpu_shares != 0 {
            merged.cpu_shares = update.cpu_shares;
        }
        if !update.cpuset_cpus.is_empty() {
            merged.cpuset_cpus = update.cpuset_cpus.clone();
        }
        if update.oom_score_adj != 0 {
            merged.oom_score_adj = update.oom_score_adj;
        }
        if !update.cpuset_mems.is_empty() {
            merged.cpuset_mems = update.cpuset_mems.clone();
        }
        if !update.hugepage_limits.is_empty() {
            merged.hugepage_limits = update.hugepage_limits.clone();
        }
        if !update.unified.is_empty() {
            merged.unified = update.unified.clone();
        }
        merged
    }
}

/// `cpu.shares` (cgroup v1, what the kubelet sends: 2..262144) → `cpu.weight`
/// (cgroup v2, 1..10000). The same linear map runc and crun use, so a pod gets
/// the same relative weight here as on any other runtime.
fn shares_to_weight(shares: i64) -> i64 {
    (1 + ((shares.clamp(2, 262_144) - 2) * 9999) / 262_142).clamp(1, 10_000)
}

/// The resource flags for `container run`, from what the kubelet asked.
///
/// This did not exist: `linux.resources` was read by nobody, so a pod's
/// `resources.limits` never reached the container. Measured (2026-09-15,
/// `crictl` with `memory_limit_in_bytes: 50331648`): the engine record said
/// `memory_max: 6646M` — the engine's default — and a memory hog exited on its
/// own instead of being OOM-killed. The scheduler believed the pod was capped.
///
/// Only what was specified: an unspecified field keeps the engine's default,
/// exactly as `container run` without the flag does.
fn apply_resources(r: &CriResources, o: &mut delonix_compute::RunOpts) {
    if r.memory_limit_in_bytes > 0 {
        o.memory = Some(r.memory_limit_in_bytes.to_string());
    }
    if r.cpu_quota > 0 && r.cpu_period > 0 {
        let cores = r.cpu_quota as f64 / r.cpu_period as f64;
        o.cpus = Some(format!("{cores:.3}"));
    }
    if r.cpu_shares > 0 {
        o.cpu_weight = Some(shares_to_weight(r.cpu_shares).to_string());
    }
    if !r.cpuset_cpus.is_empty() {
        o.cpuset = Some(r.cpuset_cpus.clone());
    }
    if r.oom_score_adj != 0 {
        // The kernel's own range (/proc/<pid>/oom_score_adj); a value
        // outside i32 from a malformed request is clamped at the write
        // site (`apply_oom_score_adj`) rather than refused here.
        o.oom_score_adj = Some(r.oom_score_adj.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
    }
    if !r.cpuset_mems.is_empty() {
        o.cpuset_mems = Some(r.cpuset_mems.clone());
    }
    if !r.hugepage_limits.is_empty() {
        o.hugepage_limits = r.hugepage_limits.clone();
    }
    if !r.unified.is_empty() {
        o.unified = r.unified.clone();
    }
}

/// PURE. Drops the `hugepage_limits` entries that ask for nothing the kernel
/// does not already enforce: a limit of `0` for a page size whose host pool is
/// empty (`pool(size) == Some(0)`).
///
/// The kubelet sends a limit for EVERY page size the node reports, `0` for the
/// ones the pod did not request, so a pod with no hugepages still carries
/// `hugetlb.2MB.limit_in_bytes=0`. On a node whose kubelet parent does not
/// delegate `hugetlb`, honouring that entry literally refused every pod —
/// measured on a kubeadm node built from main (2026-10-10): etcd and the
/// apiserver never started, `kubeadm init` timed out in `wait-control-plane`.
/// With no pages in the pool, no process can allocate one, so a zero cap is
/// already in force without the controller; nothing is being ignored.
///
/// Everything else is kept and goes through the honour-or-refuse path: a
/// non-zero limit, and a zero limit over a pool that HAS pages (there the
/// controller is what stops the container taking them). A pool that cannot be
/// read (`None`) also keeps the entry — not knowing is never read as empty.
fn drop_vacuous_zero_hugepage_limits(
    limits: Vec<(String, u64)>,
    pool: impl Fn(&str) -> Option<u64>,
) -> Vec<(String, u64)> {
    limits
        .into_iter()
        .filter(|(size, limit)| !(*limit == 0 && pool(size) == Some(0)))
        .collect()
}

/// Pages in the host pool for a CRI page size (`"2MB"`, `"1GB"`, `"64KB"`):
/// `nr_hugepages + nr_overcommit_hugepages` under
/// `/sys/kernel/mm/hugepages/hugepages-<kB>kB/`. `None` when the size does not
/// parse or the files cannot be read.
fn host_hugepage_pool(page_size: &str) -> Option<u64> {
    let kb = hugepage_size_kb(page_size)?;
    let dir = format!("/sys/kernel/mm/hugepages/hugepages-{kb}kB");
    let read = |f: &str| -> Option<u64> {
        std::fs::read_to_string(format!("{dir}/{f}"))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    Some(read("nr_hugepages")? + read("nr_overcommit_hugepages")?)
}

/// PURE. `"2MB"` → 2048, `"1GB"` → 1048576, `"64KB"` → 64. The kubelet
/// normalises page sizes to this unit form before sending them.
fn hugepage_size_kb(page_size: &str) -> Option<u64> {
    let digits: String = page_size
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let n: u64 = digits.parse().ok()?;
    let mult = match &page_size[digits.len()..] {
        "KB" => 1,
        "MB" => 1024,
        "GB" => 1024 * 1024,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// ADR 0038 item 3: `cpuset_mems`/`hugepage_limits`/`unified` are honoured or
/// refused, never silently ignored — the same guardrail #307 already gives the
/// CLI for `-m`/`--cpus`/`--cpuset`. The CRI had none at all for ANY resource
/// field: a kubelet asking for one of these on a leaf whose cgroup does not
/// delegate the controller got a container that reports `Running` with the
/// limit simply absent, discovered only by reading the cgroup by hand.
///
/// Checked here, at `StartContainer` — before it answers success, not guessed
/// at write time — against the controllers the container is ACTUALLY about to
/// land in: [`delonix_linux::leaf_controllers`] when the sandbox has no
/// kubelet `cgroup_parent` (the engine's own slice/leaf, the same destination
/// `container run` uses), or that parent's own `cgroup.controllers`
/// ([`parent_cgroup_controllers`]) when it does — the engine's slice is not
/// where that container will live (ADR 0038 item 1).
///
/// Also refuses a `hugepage_limits[].page_size` or `unified` key that would
/// escape the leaf directory it is written into (`/`, `..`): both become part
/// of a cgroup FILE PATH (`hugetlb.<page_size>.limit_in_bytes`, or the `unified`
/// key verbatim), and the kubelet is not sandboxed input the engine should
/// trust with a path component.
///
/// PURE validation and decision live in [`validate_resource_paths`],
/// [`wanted_resource_controllers`] and [`resources_decision`] — this is the
/// glue that reads the host (or, with `DELONIX_ALLOW_UNENFORCED_LIMITS`, skips
/// reading it at all).
fn refuse_unenforceable_resources(
    r: &CriResources,
    kube_cgroup_parent: Option<&str>,
) -> Result<(), Status> {
    validate_resource_paths(r)?;
    let wanted = wanted_resource_controllers(r);
    if wanted.is_empty() {
        return Ok(());
    }
    if std::env::var_os("DELONIX_ALLOW_UNENFORCED_LIMITS").is_some() {
        tracing::warn!(
            fields = %wanted.iter().map(|(_, f)| f.as_str()).collect::<Vec<_>>().join(", "),
            "cri: resource field(s) requested without checking cgroup delegation — \
             DELONIX_ALLOW_UNENFORCED_LIMITS is set"
        );
        return Ok(());
    }
    let have: Vec<String> = match kube_cgroup_parent {
        Some(parent) => parent_cgroup_controllers(parent),
        None => delonix_linux::leaf_controllers(),
    };
    resources_decision(&wanted, &have)
}

/// PURE. Rejects a `hugepage_limits[].page_size`/`unified` key that would
/// escape the leaf directory it becomes part of the path of.
fn validate_resource_paths(r: &CriResources) -> Result<(), Status> {
    for (size, _) in &r.hugepage_limits {
        if size.is_empty() || size.contains('/') || size.contains("..") {
            return Err(Status::invalid_argument(format!(
                "hugepage_limits: invalid page size {size:?}"
            )));
        }
    }
    for (key, _) in &r.unified {
        if key.is_empty() || key.contains('/') || key.contains("..") || !key.contains('.') {
            return Err(Status::invalid_argument(format!(
                "unified: invalid cgroup key {key:?}"
            )));
        }
    }
    Ok(())
}

/// PURE. The `(controller, field)` pairs `r` needs honoured, in the order
/// asked — `unified`'s controller is the prefix of the key before its first
/// `.` (cgroup v2's own naming: `memory.high`, `io.weight`, `cpuset.cpus`, …).
fn wanted_resource_controllers(r: &CriResources) -> Vec<(String, String)> {
    let mut wanted: Vec<(String, String)> = Vec::new();
    // `cpuset_cpus` was wired into `apply_resources` long before this preflight
    // existed (ADR 0038 item 3's other half, the one the context note already
    // named: "measured in a rootless session: nproc 32 inside a container that
    // asked for 0-1"). It never got a refusal of its own — closed here, in the
    // same place its sibling `cpuset_mems` was closed.
    if !r.cpuset_cpus.is_empty() {
        wanted.push(("cpuset".to_string(), "cpuset_cpus".to_string()));
    }
    if !r.cpuset_mems.is_empty() {
        wanted.push(("cpuset".to_string(), "cpuset_mems".to_string()));
    }
    if !r.hugepage_limits.is_empty() {
        wanted.push(("hugetlb".to_string(), "hugepage_limits".to_string()));
    }
    for (key, _) in &r.unified {
        let ctrl = key.split('.').next().unwrap_or(key.as_str()).to_string();
        wanted.push((ctrl, format!("unified[\"{key}\"]")));
    }
    wanted
}

/// PURE. The decision [`refuse_unenforceable_resources`] acts on, tested
/// without a cgroup2 tree — the same split the CLI's
/// `controller_limits_decision` already uses for `--cpuset`/`--io-weight`.
fn resources_decision(wanted: &[(String, String)], have: &[String]) -> Result<(), Status> {
    let missing: Vec<&(String, String)> = wanted
        .iter()
        .filter(|(c, _)| !have.iter().any(|h| h == c))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let fields = missing
        .iter()
        .map(|(_, f)| f.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut controllers: Vec<&str> = missing.iter().map(|(c, _)| c.as_str()).collect();
    controllers.sort();
    controllers.dedup();
    Err(Status::failed_precondition(format!(
        "cannot enforce {fields}: the cgroup this container would run in does not have the \
         controller(s) `{}` — the limit would not exist while StartContainer reports success. \
         Set DELONIX_ALLOW_UNENFORCED_LIMITS=1 to start without it",
        controllers.join(" ")
    )))
}

/// The controllers a kubelet-shaped `cgroup_parent` offers, read from its OWN
/// `cgroup.controllers` — never [`delonix_linux::leaf_controllers`], which
/// answers about the engine's own slice/leaf, a different cgroup than the one
/// a container with a `cgroup_parent` actually lands in (ADR 0038 item 1). An
/// unparsable or unreadable parent answers "nothing available", which is the
/// fail-closed side for a refusal check.
fn parent_cgroup_controllers(raw_parent: &str) -> Vec<String> {
    let Ok(parent) = delonix_compute::KubeCgroupParent::parse(raw_parent) else {
        return Vec::new();
    };
    std::fs::read_to_string(format!("{}/cgroup.controllers", parent.path()))
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct ContainerRec {
    id: String,
    sandbox_id: String,
    name: String,
    attempt: u32,
    image: String,
    command: Vec<String>,
    args: Vec<String>,
    created_at: i64,
    started: bool,
    /// Real wall-clock time `StartContainer` succeeded (CRI `ContainerStatus.started_at`).
    /// BUG FIXED: this used to be fabricated as `created_at` on every read instead of the
    /// real start time — anything computing container age/uptime from it (kubectl describe,
    /// probes) was wrong from the moment the container actually started.
    #[serde(default)]
    started_at: i64,
    /// Real wall-clock time the container was first observed exited (CRI
    /// `ContainerStatus.finished_at`). BUG FIXED: this used to be `now_ns()` recomputed on
    /// EVERY `ContainerStatus` poll — the kubelet polls this repeatedly, so the reported
    /// finish time kept moving forward long after the container actually died, breaking
    /// anything keyed on "how long has this been dead" (crash-loop backoff timing, log
    /// rotation heuristics). Persisted once, the first time an exit is observed.
    #[serde(default)]
    finished_at: i64,
    /// FULL path of the log file (the sandbox's log_directory + the container's
    /// log_path) — where the kubelet/crictl expect to read stdout/stderr (CRI format).
    #[serde(default)]
    log_path: String,
    /// The pod's `linux.resources` — see [`apply_resources`].
    #[serde(default)]
    resources: CriResources,
    labels: HashMap<String, String>,
    annotations: HashMap<String, String>,
    // --- security context (CRI) translated to `delonix run` flags ---
    #[serde(default)]
    readonly_rootfs: bool,
    #[serde(default)]
    privileged: bool,
    #[serde(default)]
    seccomp_unconfined: bool,
    /// The CRI's `ContainerConfig.mounts`, kept FIELD BY FIELD.
    ///
    /// Not the `-v` strings alone: `ContainerStatus` has to give the mounts back
    /// exactly as they were set, and a spec string cannot round-trip
    /// `selinux_relabel` or tell `PROPAGATION_PRIVATE` from "no propagation
    /// given". Reconstructing from the string would answer a question the
    /// string never carried. The `-v` specs are DERIVED from these, so there is
    /// one source of truth.
    #[serde(default)]
    mounts: Vec<CriMount>,
    /// `-v host:container[:opts]` specs, derived from `mounts`.
    ///
    /// This was NOT implemented at all: `cfg.mounts` was read by nobody, so a
    /// kubelet's configMaps, secrets, emptyDirs and hostPaths simply did not
    /// reach the container — silently, since nothing errored. Five conformance
    /// specs failed on it (two volume, three mount-propagation) and they read as
    /// five separate gaps rather than one missing feature.
    #[serde(default)]
    volumes: Vec<String>,
    /// Path to a `localhostProfile` on the node. Stored as the PATH here (the
    /// CRI server reads it at start, on the host, where it resolves) and passed
    /// to the engine, which reads the content before entering the container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seccomp_profile_path: Option<String>,
    #[serde(default)]
    supplemental_groups: Vec<i64>,
    #[serde(default)]
    masked_paths: Vec<String>,
    #[serde(default)]
    readonly_paths: Vec<String>,
    /// The kubelet's `no_new_privs`, honoured LITERALLY.
    ///
    /// The engine's own default is ON, stricter than Docker and Podman. Here it
    /// is not ours to pick: the CRI field is a plain bool whose zero value is
    /// `false`, containerd and CRI-O pass it through as-is, and a kubelet that
    /// says `false` for a pod with `allowPrivilegeEscalation: true` is exercising
    /// a policy it owns. A CRI implementation that quietly hardened past its
    /// client would break setuid workloads with no way to see why.
    #[serde(default)]
    no_new_privs: bool,
    #[serde(default)]
    cap_add: Vec<String>,
    #[serde(default)]
    cap_drop: Vec<String>,
    #[serde(default)]
    apparmor: Option<String>,
    /// `RunAsUser` (numeric uid) from the security context. `None` = root (historical).
    #[serde(default)]
    run_as_user: Option<i64>,
    /// `RunAsGroup` (numeric gid). Only valid with `run_as_user`/`run_as_username`.
    #[serde(default)]
    run_as_group: Option<i64>,
    /// `RunAsUserName`: the user is resolved in the image's `/etc/passwd` (the
    /// `delonix run --user <name>` does it). Empty = not used.
    #[serde(default)]
    run_as_username: String,
    /// The container's environment (`ContainerConfig.envs`) as an `--env-file0`
    /// file, `<root>/cri/env/<id>.env0`. Only the PATH is recorded: the values
    /// are Secret material as often as not, and this JSON is written with the
    /// default mode. Empty = the pod declared no variables.
    #[serde(default)]
    env_file0: String,
}

/// Where a container's environment file lives (see `ContainerRec.env_file0`).
fn env_path(base: &Path, id: &str) -> PathBuf {
    base.join("cri").join("env").join(format!("{id}.env0"))
}

/// `KEY=VAL` entries, NUL-separated — the `/proc/<pid>/environ` format, which
/// carries a value byte-exact (newlines, leading spaces), unlike `--env-file`'s
/// line-per-variable `.env`.
fn env0_bytes(envs: &[KeyValue]) -> Vec<u8> {
    let mut out = Vec::new();
    for kv in envs.iter().filter(|kv| !kv.key.is_empty()) {
        out.extend_from_slice(kv.key.as_bytes());
        out.push(b'=');
        out.extend_from_slice(kv.value.as_bytes());
        out.push(0);
    }
    out
}

/// Writes the environment file 0600 in a 0700 directory, created that way
/// (never widened after the fact, so there is no window where it is readable).
fn write_env_file(base: &Path, id: &str, envs: &[KeyValue]) -> Result<String, Status> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    let path = env_path(base, id);
    let dir = path.parent().expect("env_path has a parent");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(st)?;
    let _ = std::fs::remove_file(&path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(st)?;
    f.write_all(&env0_bytes(envs)).map_err(st)?;
    Ok(path.to_string_lossy().into_owned())
}

/// `true` if the AppArmor profile is loaded on the host (in
/// `/sys/kernel/security/apparmor/profiles`).
fn apparmor_loaded(profile: &str) -> bool {
    std::fs::read_to_string("/sys/kernel/security/apparmor/profiles")
        .map(|s| {
            s.lines()
                .any(|l| l.split_whitespace().next() == Some(profile))
        })
        .unwrap_or(false)
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn sb_dir(base: &Path) -> PathBuf {
    base.join("cri").join("sandboxes")
}
fn ct_dir(base: &Path) -> PathBuf {
    base.join("cri").join("containers")
}
fn st<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

/// Does the engine still hold `cri-<id>`? `None` when its store cannot be read —
/// which is NOT absence, and callers keep the CRI record in that case.
///
/// This replaced a stderr classifier, `stderr_not_found`, that took any output
/// containing «not found» or «no such» as «the container does not exist». The
/// engine's `Error::NotFound` prints «no such {0}» for EVERY resource (image,
/// network, volume, a missing file), so a `rm -f` that failed halfway on one of
/// those was read as idempotent success and the record was deleted with the
/// container still on disk. Measured on a `kubeadm reset` (2026-09-15, k8s 1.36.4):
/// etcd, `Exited (0)` in the engine, sandbox and CRI record gone — invisible to the
/// kubelet, and `rm -f` on it by hand afterwards worked first time. The store is
/// the thing to ask, not a sentence about it.
fn engine_has(base: &Path, cri_id: &str) -> Option<bool> {
    let store = delonix_state::Store::open(base.join("containers")).ok()?;
    match store.load(&format!("cri-{cri_id}")) {
        Ok(_) => Some(true),
        Err(e) if e.is_not_found() => Some(false),
        Err(_) => None,
    }
}

/// `rm -f` of a CRI container, judged by the engine's store afterwards. `Ok` only
/// when the container is really gone — which also makes it idempotent: a container
/// that never existed is gone too. A failed `rm` is logged with its stderr even when
/// the container turns out to be gone, so the cause is never swallowed again.
fn engine_remove(base: &Path, cri_id: &str) -> Result<(), Status> {
    let name = format!("cri-{cri_id}");
    let out = delonix(base, &["container", "rm", "-f", &name])?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = stderr.trim();
    if !out.status.success() {
        tracing::warn!(container = %name, status = %out.status, stderr = %stderr, "container rm -f failed");
    }
    match engine_has(base, cri_id) {
        Some(false) => Ok(()),
        Some(true) => Err(Status::internal(format!(
            "removal of '{name}' failed, the engine still has it (record preserved for retry): {stderr}"
        ))),
        None => Err(Status::internal(format!(
            "removal of '{name}' could not be verified, the engine store is unreadable (record preserved for retry): {stderr}"
        ))),
    }
}

/// Whitelist for CRI ids (`container_id`/`pod_sandbox_id`) used to build filesystem
/// paths (`<dir>/<id>.json`). SECURITY: these ids come straight from CRI requests —
/// a compromised/malicious kubelet (or anyone with access to the CRI socket) could
/// send `container_id: "../../../../home/<u>/somefile"` and reach paths outside
/// `ct_dir`/`sb_dir`. Mirrors `delonix_vm::valid_vm_name` and `Store::safe_key`.
fn valid_cri_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn write_rec<T: Serialize>(dir: &Path, id: &str, rec: &T) -> Result<(), Status> {
    if !valid_cri_id(id) {
        return Err(Status::invalid_argument(format!("invalid id: {id:?}")));
    }
    std::fs::create_dir_all(dir).map_err(st)?;
    let bytes = serde_json::to_vec_pretty(rec).map_err(st)?;
    // ATOMIC write (temp + rename): the CRI server is multi-threaded, and a
    // concurrent `container_status`/`list_containers` must never read a file
    // truncated mid-write.
    let final_path = dir.join(format!("{id}.json"));
    let tmp = dir.join(format!(".{id}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(st)?;
    std::fs::rename(&tmp, &final_path).map_err(st)
}
fn read_rec<T: for<'de> Deserialize<'de>>(dir: &Path, id: &str) -> Result<T, Status> {
    if !valid_cri_id(id) {
        return Err(Status::invalid_argument(format!("invalid id: {id:?}")));
    }
    // A read that fails with anything other than ENOENT is not absence, and
    // `NOT_FOUND` tells the kubelet to stop looking. Permission or I/O problems
    // come back as `INTERNAL`: the code that makes it retry, and that points the
    // operator at the disk instead of at a container that "does not exist".
    let data = std::fs::read(dir.join(format!("{id}.json"))).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Status::not_found(format!("{id} not found"))
        } else {
            Status::internal(format!("reading the record of {id}: {e}"))
        }
    })?;
    serde_json::from_slice(&data).map_err(st)
}
/// Guarded `remove_file` for the raw (non-`write_rec`) deletion call sites — same
/// whitelist, silently skipped (best-effort, matching the existing `let _ =` style)
/// rather than erroring, since these run during cleanup paths that must not abort.
fn remove_rec(dir: &Path, id: &str) {
    if valid_cri_id(id) {
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
    }
}
fn list_recs<T: for<'de> Deserialize<'de>>(dir: &Path) -> Vec<T> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(data) = std::fs::read(e.path()) {
                if let Ok(r) = serde_json::from_slice(&data) {
                    out.push(r);
                }
            }
        }
    }
    out
}

fn delonix_bin() -> PathBuf {
    delonix_node::dispatch::cli_bin()
}

/// Runs the `delonix` binary (single-threaded) with the CRI's `DELONIX_ROOT`.
/// `DELONIX_INTERNAL=1` bypasses the grouped-commands barrier (machine-to-machine
/// delegation): the CRI uses the top-level `run`/`stop`/`rm` forms.
fn delonix(base: &Path, args: &[&str]) -> Result<std::process::Output, Status> {
    Command::new(delonix_bin())
        .env("DELONIX_ROOT", base)
        .env("DELONIX_INTERNAL", "1")
        .args(args)
        .output()
        .map_err(st)
}

/// Runs a `delonix` subcommand and, on failure, returns WHY.
///
/// This used to send stderr to `/dev/null` and return a bare bool, so every
/// sandbox failure surfaced to the kubelet as "failed to create the ingress
/// sandbox <id>" with no cause — a message that names the victim and hides the
/// killer. It cost a full `critest` run to find that the actual error had been
/// `unrecognized subcommand 'netns'` all along: the v0.30.0 CLI reorganisation
/// moved `netns` under `net` with a deliberate clean break, and this call site
/// was never updated. A visible stderr would have said so on the first pod.
///
/// **stderr goes to a FILE, never a pipe.** A `run -d` daemonizes, and the
/// detached container inherits and HOLDS whatever stdout/stderr it was given: a
/// pipe read with `.output()` would not reach EOF until the container exits, so
/// `StartContainer` would hang for the pod's whole life (the "run -d | tail
/// hangs" bug). A file has no EOF to wait for — the command's exit status is
/// what we wait on, and the file is read afterwards.
fn delonix_detached_why(base: &Path, args: &[&str]) -> Result<Option<String>, Status> {
    delonix_detached_why_in(base, None, args)
}

/// [`delonix_detached_why`], run inside the network namespace at `netns` when
/// given (`nsenter --net`, which switches ONLY the netns — the engine keeps the
/// host's mount namespace, so the rootfs it mounts stays visible to the `rm`
/// that unmounts it).
fn delonix_detached_why_in(
    base: &Path,
    netns: Option<&str>,
    args: &[&str],
) -> Result<Option<String>, Status> {
    use std::process::Stdio;
    let err_dir = base.join("cri").join("tmp");
    std::fs::create_dir_all(&err_dir).map_err(st)?;
    let err_path = err_dir.join(format!(
        "delonix-{}-{}.err",
        std::process::id(),
        delonix_node::generate_id()
    ));
    let err_file = std::fs::File::create(&err_path).map_err(st)?;
    let mut cmd = match netns {
        Some(ns) => {
            let mut c = Command::new("nsenter");
            c.arg(format!("--net={ns}")).arg("--").arg(delonix_bin());
            c
        }
        None => Command::new(delonix_bin()),
    };
    let status = cmd
        .env("DELONIX_ROOT", base)
        .env("DELONIX_INTERNAL", "1")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err_file))
        .status();
    let stderr = std::fs::read(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    let status = status.map_err(st)?;
    if status.success() {
        return Ok(None);
    }
    let why = String::from_utf8_lossy(&stderr);
    let why = why.trim();
    Ok(Some(if why.is_empty() {
        format!("`delonix {}` exited {}", args.join(" "), status)
    } else {
        // First line only: clap appends usage text, and a kubelet event that
        // carries a whole help screen is unreadable where it actually shows up.
        why.lines().next().unwrap_or(why).to_string()
    }))
}

/// Loads a CRI container and **reconciles** its status against the kernel
/// (`Running`+dead pid → `Crashed`/`Failed`) before returning it, persisting the
/// change (best-effort). This is the heart of the exit-code fix: without
/// reconciling, a container that crashed but whose store still says `Running`
/// reported state `Exited` with exit-code 0 → the kubelet (restartPolicy
/// `OnFailure`) did NOT restart it. After reconciling, the crash becomes
/// `Crashed` (137) and the kubelet reacts.
fn load_reconciled(base: &Path, cri_id: &str) -> Option<delonix_compute::Container> {
    let store = delonix_state::Store::open(base.join("containers")).ok()?;
    // `update` (flock + re-reads under the lock), NOT `load`+`save`: this server
    // is CONCURRENT (the kubelet issues requests in parallel, each in a
    // `spawn_blocking`) and the CLI touches the same state. With the naive
    // pattern, two simultaneous reconciles lost writes — measured: 24 concurrent
    // updates → 1 survivor (see `store::tests::update_concorrente_nao_perde_escritas`).
    store
        .update(&format!("cri-{cri_id}"), |c| {
            delonix_linux::reconcile_status(c)
        })
        .ok()
}

/// The runtime state of a CRI container, read (and reconciled) from the `Store`.
fn delonix_state(base: &Path, cri_id: &str) -> i32 {
    use delonix_model::records::Status as S;
    match load_reconciled(base, cri_id) {
        Some(c) => match c.status {
            S::Running if c.is_live() => ContainerState::ContainerRunning as i32,
            S::Running => ContainerState::ContainerExited as i32, // defensive (post-reconcile)
            S::Paused => ContainerState::ContainerRunning as i32, // frozen, but exists
            S::Stopped | S::Failed(_) | S::Crashed => ContainerState::ContainerExited as i32,
            S::Created => ContainerState::ContainerCreated as i32,
        },
        None => ContainerState::ContainerUnknown as i32,
    }
}

/// The exit code of a CRI container (reconciled), or `None` if it is still
/// running/created. Lets the kubelet see the true exit cause (137/143/n) and
/// apply the `restartPolicy` — instead of assuming 0 (`Completed`) for everything.
fn delonix_exit(base: &Path, cri_id: &str) -> Option<i32> {
    use delonix_model::records::Status as S;
    match load_reconciled(base, cri_id)?.status {
        S::Failed(code) => Some(code),
        S::Stopped => Some(0),
        S::Crashed => Some(137),
        _ => None,
    }
}

/// Whether the kernel's OOM killer took this CRI container down (the engine
/// records it on the cgroup before removing it — see `delonix_linux::OOM_KILLED`).
fn delonix_oom_killed(base: &Path, cri_id: &str) -> bool {
    load_reconciled(base, cri_id)
        .is_some_and(|c| c.crash_reason.as_deref() == Some(delonix_linux::OOM_KILLED))
}

/// Whether this CRI container's image declared a non-root `USER` the host
/// could not honour, so it is actually running as root (ADR-0062's documented
/// fallback). Before this, the fact lived only in `Container.
/// user_fallback_to_root` — readable from `delonix container inspect`, but
/// invisible to anything going through the CRI: a Pod declaring
/// `runAsNonRoot` could be running as root with nothing in `ContainerStatus`
/// to show it. `None` (container unknown) reads the same as `false` here —
/// there is no fallback fact to report about a container that cannot be
/// found, which is the state `container_status` reports on it either way.
fn delonix_user_fallback_to_root(base: &Path, cri_id: &str) -> bool {
    load_reconciled(base, cri_id).is_some_and(|c| c.user_fallback_to_root)
}

/// The CRI `reason` for an exited container. `OOMKilled` is the string the
/// kubelet and `kubectl describe` already know; before it existed an OOM came
/// back as `Error` with exit 137, indistinguishable from an external SIGKILL.
fn exit_reason(exit: Option<i32>, oom_killed: bool) -> String {
    match exit {
        None => String::new(),
        Some(_) if oom_killed => "OOMKilled".into(),
        Some(0) => "Completed".into(),
        Some(_) => "Error".into(),
    }
}

/// The sandbox's `linux.cgroup_parent`, validated (ADR 0038). Empty when the kubelet sent
/// none. An invalid one is `invalid_argument`, never silently dropped: a pod started outside
/// the hierarchy its limits live on would run unlimited while the kubelet believes otherwise.
fn cgroup_parent_of(cfg: &PodSandboxConfig) -> Result<String, Status> {
    let raw = cfg
        .linux
        .as_ref()
        .map(|l| l.cgroup_parent.trim().to_string())
        .unwrap_or_default();
    if raw.is_empty() {
        return Ok(raw);
    }
    delonix_compute::KubeCgroupParent::parse(&raw)
        .map(|k| k.parent)
        .map_err(Status::invalid_argument)
}

// ---- pods (sandboxes) -----------------------------------------------------

pub fn run_pod_sandbox(
    base: &Path,
    req: RunPodSandboxRequest,
) -> Result<Response<RunPodSandboxResponse>, Status> {
    let cfg = req
        .config
        .ok_or_else(|| Status::invalid_argument("missing config"))?;
    let md = cfg.metadata.clone().unwrap_or_default();
    // Validated FIRST, before the sandbox has created anything: an invalid parent is a
    // refusal the kubelet sees on the pod, not a netns left behind (ADR 0038).
    let cgroup_parent = cgroup_parent_of(&cfg)?;
    let id = delonix_node::generate_id();
    // Host network? (namespace_options.network == NODE) → no own infra/netns.
    let ns = cfg
        .linux
        .as_ref()
        .and_then(|l| l.security_context.as_ref())
        .and_then(|s| s.namespace_options.as_ref());
    let is_node = |m: i32| m == NamespaceMode::Node as i32;
    let host_network = ns.map(|n| is_node(n.network)).unwrap_or(false);
    let host_pid = ns.map(|n| is_node(n.pid)).unwrap_or(false);
    let host_ipc = ns.map(|n| is_node(n.ipc)).unwrap_or(false);
    // pod sysctls (`net.*`, `kernel.shm*`, …) → `key=value`.
    let sysctls: Vec<String> = cfg
        .linux
        .as_ref()
        .map(|l| l.sysctls.iter().map(|(k, v)| format!("{k}={v}")).collect())
        .unwrap_or_default();
    // What the kubelet ASKED for. Judged PER PATH below, because the paths
    // genuinely differ and a blanket answer was wrong: measured 2026-10-07, the
    // rootless `slirp4netns` refuses SCTP at `add_hostfwd` while the CNI
    // `portmap` plugin publishes it end to end (a real SCTP client on the node
    // reached a server inside the pod netns through its DNAT). A refusal that
    // ignored the path would have closed a door that is open in root mode.
    //
    // Under hostNetwork there is nothing to publish, so nothing to judge — and
    // refusing there would break a case that WORKS. Measured the same day: a
    // `--net host` container is in the host's REAL netns (same `net:[...]` inode,
    // and it sees the node's own interfaces), so it binds the node's ports
    // itself, whatever the transport. Which is also why the mappings are not
    // handed to it at all (see `run_opts_of`'s guard).
    let asked_ports = if host_network {
        Vec::new()
    } else {
        cri_port_mappings(&cfg.port_mappings)
    };
    // The specs the SLIRP path publishes, and the refusal that belongs to it.
    // Computed before anything is created, so a transport that path cannot carry
    // is a refusal the kubelet shows on the pod and not a sandbox left behind.
    // The CNI branches below never read it — they publish through the chain.
    let port_mappings = if host_network || delonix_sdn::cni::enabled_conf().is_some() {
        Vec::new()
    } else if delonix_linux::is_rootless() {
        publishable_port_specs(&cfg.port_mappings)?
    } else {
        Vec::new()
    };
    // REAL Delonix pod: an infra container (`pod-cri-<id>`) holds the shared
    // netns ("pause"-style), which the sandbox's containers then join via
    // `--pod`. That is what gives pod networking and namespace sharing.
    // CNI: the sandbox gets its network from real CNI plugins (the cluster chain,
    // e.g. Calico), as in containerd/CRI-O.
    // * Rootless, opt-in (`DELONIX_CNI=1` + conflist) → the plugins run in the
    //   holder (owner of the netns); the netns is named `cri-<id>` so the
    //   sandbox's containers join via `--pod cri-<id>` (join_argv). Without the
    //   flag, `enabled_conf()` is None and it follows the native (SDN) path.
    // * Root → ALWAYS the node's conflist, in the host (see the branch below);
    //   without a usable one the sandbox is refused and `NetworkReady` says why.
    let mut cni_ip = String::new();
    let mut cni_netns = String::new();
    let mut cni_conf = String::new();
    if !host_network {
        let pod = format!("cri-{id}");
        let cni = delonix_sdn::cni::enabled_conf();
        if let Some(conf) = cni.filter(|_| delonix_linux::is_rootless()) {
            refuse_chain_without_port_mappings(&conf, &asked_ports, &pod)?;
            // The capability argument travels INSIDE the conflist, so the
            // holder's control line keeps its shape (the JSON is hex-encoded and
            // opaque to it) and the same bytes serve the `DEL`.
            let conf = delonix_sdn::cni::with_port_mappings(&conf, &asked_ports);
            let conf_json = serde_json::to_string(&conf)
                .map_err(|e| Status::internal(format!("serializing conflist: {e}")))?;
            match delonix_sdn::infra::cni_attach_container(&pod, &conf_json) {
                Ok((_netns, cidr)) => {
                    cni_ip = cidr.split('/').next().unwrap_or("").to_string();
                }
                Err(e) => return Err(Status::internal(format!("CNI ADD of sandbox {pod}: {e}"))),
            }
            // Kept so the DEL hands the plugins the identical config, which is
            // what the CNI spec asks of a runtime.
            cni_conf = conf_json;
        } else if delonix_linux::is_rootless() {
            // ROOTLESS: the pod is a SHARED ingress netns (delonix0 + DHCP +
            // DNS + firewall); the sandbox's containers join via `--pod`.
            if let Some(why) = delonix_detached_why(base, &["net", "netns", "attach", &pod])? {
                return Err(Status::internal(format!(
                    "failed to create the ingress sandbox {pod}: {why}"
                )));
            }
        } else {
            // ROOT: the pod network is the node's CNI chain, in the HOST — the
            // containerd/CRI-O model. The plugins create the bridge (`cni0`), the
            // veth and the IPAM lease; the host routes to the pod IP, which is what
            // the kubelet's probes, kube-proxy and every other node need.
            //
            // It used to run `pod create cri-<id> --network`, and that never
            // worked: `pod create` only takes `-f <manifest>`, so clap refused it
            // (`unexpected argument 'cri-…' found`) — measured 2026-09-15 on a
            // kubeadm node, and the call dates from the initial commit. Nor could
            // the native infra have served it there: in root it did not come up at
            // all (`control socket: No such file or directory`). The CRI in root
            // never had a working non-`hostNetwork` pod; `NetworkReady` pinned to
            // `BridgeMissing` hid it, because no such pod was ever scheduled.
            let dirs = delonix_sdn::cni::plugin_dirs();
            let conf = match super::root_cni_readiness(&dirs) {
                delonix_sdn::cni::Readiness::Ready(conf) => conf,
                other => {
                    let (_, why) = other.not_ready(&dirs).unwrap_or_default();
                    return Err(Status::failed_precondition(format!(
                        "cannot network the pod sandbox {pod}: {why}"
                    )));
                }
            };
            refuse_chain_without_port_mappings(&conf, &asked_ports, &pod)?;
            // `hostPort` through the chain, which is what containerd does and
            // what this path never did: the mappings were stored and then
            // DROPPED (`run_opts_of`'s guard skips a CNI sandbox), so every pod
            // came up `Running` with a port that answered nothing, for every
            // transport. The capability argument goes into the conflist so the
            // same bytes serve the `ADD` here and the `DEL` later.
            let conf = delonix_sdn::cni::with_port_mappings(&conf, &asked_ports);
            let conf_json = serde_json::to_string(&conf)
                .map_err(|e| Status::internal(format!("serializing conflist: {e}")))?;
            let cidr = delonix_sdn::cni::attach_named_netns(
                &conf,
                &pod,
                &pod,
                delonix_sdn::cni::DEFAULT_IFNAME,
            )
            .map_err(|e| Status::internal(format!("CNI ADD of sandbox {pod}: {e}")))?;
            cni_ip = cidr.split('/').next().unwrap_or("").to_string();
            if cni_ip.is_empty() {
                // A pod without an address reads as networked and is not.
                delonix_sdn::cni::detach_named_netns(
                    Some(&conf),
                    &pod,
                    &pod,
                    delonix_sdn::cni::DEFAULT_IFNAME,
                );
                return Err(Status::internal(format!(
                    "CNI ADD of sandbox {pod} returned no IP address (network `{}`)",
                    conf.name
                )));
            }
            cni_netns = delonix_sdn::cni::named_netns_path(&pod);
            // The pod's `net.*` sysctls belong to the pod's netns, and here the
            // containers join it with `--net host`, where the engine (rightly)
            // refuses `net.*`. So they are set HERE, once, on top of the two
            // defaults containerd 2.x applies to every pod netns
            // (`enable_unprivileged_ports`/`_icmp`): a non-root process may bind
            // a port below 1024 and ping inside its OWN namespace. Without the
            // first, the kubeadm CoreDNS (uid 65532, `drop: ALL`) died on
            // `listen tcp :53: bind: permission denied` — measured 2026-09-15.
            let sysctls = pod_netns_sysctls(&sysctls);
            if let Err(e) = delonix_sdn::cni::set_netns_sysctls(&cni_netns, &sysctls) {
                delonix_sdn::cni::detach_named_netns(
                    Some(&conf),
                    &pod,
                    &pod,
                    delonix_sdn::cni::DEFAULT_IFNAME,
                );
                return Err(Status::internal(format!(
                    "sysctls of the pod sandbox {pod}: {e}"
                )));
            }
            cni_conf = conf_json;
        }
    }
    let rec = SandboxRec {
        id: id.clone(),
        name: md.name,
        namespace: md.namespace,
        uid: md.uid,
        attempt: md.attempt,
        created_at: now_ns(),
        hostname: cfg.hostname,
        log_directory: cfg.log_directory,
        stopped: false,
        labels: cfg.labels,
        annotations: cfg.annotations,
        host_network,
        host_pid,
        host_ipc,
        sysctls,
        cgroup_parent,
        dns_servers: cfg
            .dns_config
            .as_ref()
            .map(|d| d.servers.clone())
            .unwrap_or_default(),
        dns_searches: cfg
            .dns_config
            .as_ref()
            .map(|d| d.searches.clone())
            .unwrap_or_default(),
        dns_options: cfg
            .dns_config
            .as_ref()
            .map(|d| d.options.clone())
            .unwrap_or_default(),
        port_mappings,
        cni_ip,
        cni_netns,
        cni_conf,
    };
    write_rec(&sb_dir(base), &id, &rec)?;
    delonix_telemetry::metrics::inc_pod_sandbox_created();
    Ok(Response::new(RunPodSandboxResponse { pod_sandbox_id: id }))
}

/// What a root CNI sandbox writes into its netns: containerd's two defaults,
/// then the pod's own `net.*` (which win — a pod that sets
/// `ip_unprivileged_port_start` keeps its value).
fn pod_netns_sysctls(pod: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![
        ("net.ipv4.ip_unprivileged_port_start".into(), "0".into()),
        ("net.ipv4.ping_group_range".into(), "0 2147483647".into()),
    ];
    for s in pod {
        if let Some((k, v)) = s.split_once('=').filter(|(k, _)| k.starts_with("net.")) {
            out.retain(|(ok, _)| ok != k);
            out.push((k.to_string(), v.to_string()));
        }
    }
    out
}

pub fn stop_pod_sandbox(
    base: &Path,
    id: String,
) -> Result<Response<StopPodSandboxResponse>, Status> {
    // The CRI contract: «if there are any running containers in the sandbox, they
    // must be forcibly terminated». The graceful stop is the kubelet's, and it
    // already happened (`StopContainer` with the pod's grace) before it gets here.
    //
    // This used to call `container stop` WITHOUT `-t`, i.e. with the engine's own
    // long grace, one container after the other, and ignore the result. `kubeadm
    // reset` gives `StopPodSandbox` a 2 s deadline: measured 2026-09-15 (k8s
    // 1.36.4), every call ended `DeadlineExceeded`, kubeadm gave up after its
    // retries, and the node was left with 2 engine containers, 1 sandbox and 2
    // container records that nothing would ever remove.
    // Logged for the same reason `StopContainer` is: without it a `kubeadm reset`
    // left no trace on this side at all, even when it worked.
    tracing::info!(sandbox = %id, "CRI StopPodSandbox");
    let mut still_running = Vec::new();
    for c in list_recs::<ContainerRec>(&ct_dir(base)) {
        if c.sandbox_id != id {
            continue;
        }
        let name = format!("cri-{}", c.id);
        let _ = delonix(base, &["container", "stop", "-t", "0", &name])?;
        // Judged by the reconciled state, not by the exit status: stopping an
        // already-exited container is success, and a "stopped" process that is
        // still alive is not.
        if load_reconciled(base, &c.id).is_some_and(|k| {
            matches!(k.status, delonix_model::records::Status::Running) && k.is_live()
        }) {
            still_running.push(name);
        }
    }
    // Only a sandbox whose containers are really down is NotReady: an error makes
    // the kubelet (or kubeadm) retry, instead of believing a live pod is gone.
    if !still_running.is_empty() {
        return Err(Status::internal(format!(
            "sandbox {id}: still running after a forced stop: {}",
            still_running.join(", ")
        )));
    }
    if let Ok(mut r) = read_rec::<SandboxRec>(&sb_dir(base), &id) {
        r.stopped = true;
        write_rec(&sb_dir(base), &id, &r)?;
    }
    Ok(Response::new(StopPodSandboxResponse {}))
}

pub fn remove_pod_sandbox(
    base: &Path,
    id: String,
) -> Result<Response<RemovePodSandboxResponse>, Status> {
    // Same rule as `remove_container`: the record goes only AFTER the engine
    // removed the container. Dropping it on a failed `rm -f` is how a node ends up
    // with an engine container that no CRI record points at — invisible to the
    // kubelet, so never retried and never reclaimed (measured after a `kubeadm
    // reset`, 2026-09-15). `rm -f` kills a container that is still running, so
    // this holds whether or not `StopPodSandbox` came first.
    tracing::info!(sandbox = %id, "CRI RemovePodSandbox");
    for c in list_recs::<ContainerRec>(&ct_dir(base)) {
        if c.sandbox_id != id {
            continue;
        }
        engine_remove(base, &c.id)
            .map_err(|e| Status::internal(format!("sandbox {id}: {}", e.message())))?;
        remove_rec(&ct_dir(base), &c.id);
    }
    // Remove the real Delonix pod (infra container + netns), if it existed.
    if let Ok(sb) = read_rec::<SandboxRec>(&sb_dir(base), &id) {
        if !sb.host_network {
            if !sb.cni_netns.is_empty() {
                // ROOT + CNI: the chain it was configured with, then the netns.
                let pod = format!("cri-{id}");
                let conf = delonix_sdn::cni::parse_config(&sb.cni_conf).ok();
                delonix_sdn::cni::detach_named_netns(
                    conf.as_ref(),
                    &pod,
                    &pod,
                    delonix_sdn::cni::DEFAULT_IFNAME,
                );
            } else if !sb.cni_ip.is_empty() {
                // CNI-configured sandbox (rootless): plugin DEL in the holder.
                if let Some(conf) = delonix_sdn::cni::enabled_conf() {
                    let cj = serde_json::to_string(&conf).unwrap_or_default();
                    let _ = delonix_sdn::infra::cni_detach_container(&format!("cri-{id}"), &cj);
                }
            } else if delonix_linux::is_rootless() {
                let _ = delonix(base, &["net", "netns", "detach", &format!("cri-{id}")]);
            }
            // No root branch without `cni_netns`: it used to run `pod rm cri-<id>`,
            // a command that does not exist, with its result discarded. A root
            // sandbox record is only ever written after the CNI `ADD` succeeded.
        }
    }
    remove_rec(&sb_dir(base), &id);
    Ok(Response::new(RemovePodSandboxResponse {}))
}

fn to_pod_sandbox(r: &SandboxRec) -> PodSandbox {
    PodSandbox {
        id: r.id.clone(),
        metadata: Some(PodSandboxMetadata {
            name: r.name.clone(),
            uid: r.uid.clone(),
            namespace: r.namespace.clone(),
            attempt: r.attempt,
        }),
        state: sandbox_state(r),
        created_at: r.created_at,
        labels: r.labels.clone(),
        annotations: r.annotations.clone(),
        runtime_handler: String::new(),
    }
}

/// Does this sandbox satisfy the kubelet's `PodSandboxFilter`?
///
/// Pure, and on the ALREADY-BUILT `PodSandbox` rather than on the record: `state` is
/// derived (`sandbox_state`), so matching on the built value is what keeps a filtered
/// list from deriving it twice and from ever disagreeing with what it reports.
///
/// An absent/empty filter matches everything — that is the CRI contract for "list all",
/// and it is why `unwrap_or_default()` upstream is correct rather than lenient.
fn sandbox_matches(s: &PodSandbox, f: &PodSandboxFilter) -> bool {
    if !f.id.is_empty() && s.id != f.id {
        return false;
    }
    if let Some(st) = &f.state {
        if s.state != st.state {
            return false;
        }
    }
    // SUBSET match, not map equality: the kubelet selects on the two or three labels it
    // cares about while a sandbox carries every label the pod was created with.
    f.label_selector
        .iter()
        .all(|(k, v)| s.labels.get(k) == Some(v))
}

pub fn list_pod_sandbox(
    base: &Path,
    filter: Option<PodSandboxFilter>,
) -> Result<Response<ListPodSandboxResponse>, Status> {
    let f = filter.unwrap_or_default();
    let items = list_recs::<SandboxRec>(&sb_dir(base))
        .iter()
        .map(to_pod_sandbox)
        .filter(|s| sandbox_matches(s, &f))
        .collect();
    Ok(Response::new(ListPodSandboxResponse { items }))
}

pub fn pod_sandbox_status(
    base: &Path,
    id: String,
) -> Result<Response<PodSandboxStatusResponse>, Status> {
    let r: SandboxRec = read_rec(&sb_dir(base), &id)?;
    // Pod IP: that of the infra container (`pod-cri-<id>`), which holds the netns.
    let ip = if r.host_network {
        String::new()
    } else if !r.cni_ip.is_empty() {
        // CNI-configured sandbox: the IP came from the plugin's IPAM.
        r.cni_ip.clone()
    } else if delonix_linux::is_rootless() {
        // ROOTLESS: IP of the pod's shared netns in the ingress (deterministic).
        delonix_sdn::infra::container_ip(&format!("cri-{}", r.id))
    } else {
        delonix_state::Store::open(base.join("containers"))
            .ok()
            .and_then(|s| s.load(&format!("pod-cri-{}", r.id)).ok())
            .and_then(|c| c.ip)
            .unwrap_or_default()
    };
    let status = PodSandboxStatus {
        id: r.id.clone(),
        metadata: Some(PodSandboxMetadata {
            name: r.name.clone(),
            uid: r.uid.clone(),
            namespace: r.namespace.clone(),
            attempt: r.attempt,
        }),
        state: sandbox_state(&r),
        created_at: r.created_at,
        network: Some(PodSandboxNetworkStatus {
            ip,
            additional_ips: vec![],
        }),
        // The namespace modes the sandbox was CREATED with, given back verbatim.
        //
        // This used to be `None`, and that single word cost a control plane. The
        // kubelet compares `linux.namespaces.options.network` against what the pod
        // asks for, on every sync, and starts a NEW sandbox whenever they differ.
        // With `linux: None` prost hands it the zero value — `POD` — so every
        // `hostNetwork: true` pod (which is EVERY kubeadm control-plane static pod:
        // etcd, apiserver, controller-manager, scheduler) compared `POD != NODE`
        // and was torn down and rebuilt about once per second, for ever.
        //
        // MEASURED 2026-09-07, v0.66.0, k8s 1.36.4: `kubeadm init` never got past
        // `wait-control-plane`, the four containers showed `Exited (0)` with no
        // probe having run, and `crictl pods` reached ATTEMPT 401 on the etcd pod
        // in under four minutes — 128 live sandboxes and climbing. The same
        // container started by hand stayed `Up`, because there is no kubelet to
        // compare anything.
        //
        // `run_pod_sandbox` already reads all three modes and stores them; only
        // the way back was missing.
        linux: Some(LinuxPodSandboxStatus {
            namespaces: Some(Namespace {
                options: Some(NamespaceOption {
                    network: ns_mode(r.host_network),
                    pid: ns_mode(r.host_pid),
                    ipc: ns_mode(r.host_ipc),
                    ..Default::default()
                }),
            }),
        }),
        labels: r.labels.clone(),
        annotations: r.annotations.clone(),
        runtime_handler: String::new(),
    };
    Ok(Response::new(PodSandboxStatusResponse {
        status: Some(status),
        info: Default::default(),
        containers_statuses: vec![],
        timestamp: now_ns(),
    }))
}

// ---- containers -----------------------------------------------------------

pub fn create_container(
    base: &Path,
    req: CreateContainerRequest,
    ceiling: crate::CapCeiling,
) -> Result<Response<CreateContainerResponse>, Status> {
    let cfg = req
        .config
        .ok_or_else(|| Status::invalid_argument("missing config"))?;
    let md = cfg.metadata.unwrap_or_default();
    let image = cfg.image.map(|s| s.image).unwrap_or_default();
    if image.is_empty() {
        return Err(Status::invalid_argument("imagem em falta"));
    }
    let id = delonix_node::generate_id();
    // Security context (CRI) → `delonix run` flags (applied at start).
    let sc = cfg.linux.as_ref().and_then(|l| l.security_context.as_ref());
    let readonly_rootfs = sc.map(|s| s.readonly_rootfs).unwrap_or(false);
    let privileged = sc.map(|s| s.privileged).unwrap_or(false);
    let (cap_add, cap_drop) = sc
        .and_then(|s| s.capabilities.as_ref())
        .map(|c| (c.add_capabilities.clone(), c.drop_capabilities.clone()))
        .unwrap_or_default();
    // Node capability ceiling (`DELONIX_CRI_CAP_CEILING`) — enforced HERE, at
    // create, like the rest of the security context (AppArmor profile, the
    // run_as_group contract): the kubelet then surfaces the refusal on the pod
    // right away, instead of a container that starts with less privilege than its
    // spec asked for and fails later for a reason that looks unrelated. In `clamp`
    // mode `rejected()` is empty by construction and the reduction happens at
    // start; see `cap_ceiling`.
    let denied = ceiling.rejected(&cap_add, privileged);
    if !denied.is_empty() {
        let asked = if privileged {
            "privileged: true".to_string()
        } else {
            cap_add.join(",")
        };
        tracing::warn!(
            denied = %denied.join(","),
            requested = %asked,
            ceiling = %ceiling.describe(),
            "cri: capability request denied by the node ceiling"
        );
        return Err(Status::permission_denied(format!(
            "capabilities denied by the node ceiling: {} (requested via {}; ceiling: {}). \
             Lower the pod's securityContext, or widen {} on this node.",
            denied.join(", "),
            asked,
            ceiling.describe(),
            crate::cap_ceiling::CEILING_ENV,
        )));
    }
    // The node has no capability ceiling at all: nothing stands between this
    // elevated request and the full capability set the engine would
    // otherwise grant. `policy::enforce` (the CLI's own admission point)
    // cannot reach this path — it lives in the `-bin` crate, which this
    // interface cannot depend on — so without this, an elevated pod admitted
    // here left no structural signal anywhere. Only when the request itself
    // asks for something elevated: an ordinary pod with no privilege and no
    // added capability is not the posture this gap is about, and logging it
    // too would drown the signal — the same restraint `SecurityEvent::
    // from_decision` already applies to a plain `Decision::Allow`.
    if ceiling.is_unlimited() && (privileged || !cap_add.is_empty()) {
        let asked = if privileged {
            "privileged: true".to_string()
        } else {
            cap_add.join(",")
        };
        let severity = if privileged {
            delonix_security_runtime::Severity::High
        } else {
            delonix_security_runtime::Severity::Medium
        };
        delonix_security_runtime::SecurityEvent::unconstrained(
            delonix_security_runtime::event::unconstrained::NO_CAP_CEILING,
            severity,
            delonix_security_runtime::Workload::Container,
            &format!("cri-{id}"),
            Some(&asked),
        )
        .emit(base);
    }
    let seccomp_unconfined = sc
        .and_then(|s| s.seccomp.as_ref())
        .map(|p| p.profile_type == security_profile::ProfileType::Unconfined as i32)
        .unwrap_or(false);
    // `SecurityProfile::Localhost` — a path to an OCI profile on the node, which
    // is what a pod's `securityContext.seccompProfile.localhostProfile`
    // becomes. Resolved at CREATE so a missing or malformed file is an error the
    // kubelet sees immediately, not a container that dies at start.
    let seccomp_profile_path = sc
        .and_then(|s| s.seccomp.as_ref())
        .filter(|p| p.profile_type == security_profile::ProfileType::Localhost as i32)
        .map(|p| p.localhost_ref.clone())
        .filter(|p| !p.is_empty());
    if let Some(path) = &seccomp_profile_path {
        let json = std::fs::read_to_string(path)
            .map_err(|e| Status::invalid_argument(format!("seccomp profile {path}: {e}")))?;
        delonix_linux::seccomp_profile::parse(&json)
            .map_err(|e| Status::invalid_argument(format!("seccomp profile {path}: {e}")))?;
    }
    // AppArmor: the NEW field (`apparmor`, SecurityProfile) takes precedence; if it
    // is not set, it falls back to the DEPRECATED field `apparmor_profile` (string,
    // format `unconfined` | `localhost/<profile>` | `runtime/default` | `<profile>`).
    let apparmor = sc
        .and_then(|s| s.apparmor.as_ref())
        .and_then(
            |p| match security_profile::ProfileType::try_from(p.profile_type) {
                Ok(security_profile::ProfileType::Unconfined) => Some("unconfined".to_string()),
                Ok(security_profile::ProfileType::Localhost) if !p.localhost_ref.is_empty() => {
                    Some(p.localhost_ref.clone())
                }
                _ => None,
            },
        )
        .or_else(|| {
            #[allow(deprecated)] // intentional support for the deprecated CRI field
            let s = sc.map(|s| s.apparmor_profile.as_str()).unwrap_or("");
            match s {
                "" | "runtime/default" => None,
                "unconfined" => Some("unconfined".into()),
                _ => Some(s.strip_prefix("localhost/").unwrap_or(s).to_string()),
            }
        });
    // RunAsUser/RunAsGroup/RunAsUserName (→ `delonix run --user`, applied at start).
    // `Int64Value` is optional (the ABSENCE of the message = not specified).
    let run_as_user = sc.and_then(|s| s.run_as_user.as_ref()).map(|v| v.value);
    let run_as_group = sc.and_then(|s| s.run_as_group.as_ref()).map(|v| v.value);
    let run_as_username = sc.map(|s| s.run_as_username.clone()).unwrap_or_default();
    // CRI contract: `run_as_group` can only exist with `run_as_user` OR
    // `run_as_username`; otherwise the runtime MUST fail (proto spec). Validated in
    // CreateContainer, like the rest of the security context.
    if run_as_group.is_some() && run_as_user.is_none() && run_as_username.is_empty() {
        return Err(Status::invalid_argument(
            "run_as_group specified without run_as_user or run_as_username",
        ));
    }
    // Validate ALREADY in CreateContainer (like runc): an AppArmor profile not
    // loaded on the host makes creation fail (cri-tools checks it here).
    if let Some(p) = &apparmor {
        if p != "unconfined" && p != "delonix-default" && !apparmor_loaded(p) {
            return Err(Status::invalid_argument(format!(
                "AppArmor profile '{p}' is not loaded on the host"
            )));
        }
    }
    // Full log path: `log_path` is relative to the sandbox's `log_directory`
    // (the kubelet always provides it that way). REJECTS `..` and absolute paths —
    // otherwise a malicious request would write files outside the log directory.
    let full_log_path = {
        let lp = cfg.log_path.clone();
        if lp.is_empty() {
            String::new()
        } else if lp.starts_with('/') || lp.split('/').any(|seg| seg == ".." || seg == ".") {
            return Err(Status::invalid_argument(
                "invalid log_path: must be relative and without '..'",
            ));
        } else {
            let dir = read_rec::<SandboxRec>(&sb_dir(base), &req.pod_sandbox_id)
                .map(|s| s.log_directory)
                .unwrap_or_default();
            if dir.is_empty() {
                String::new()
            } else {
                format!("{}/{}", dir.trim_end_matches('/'), lp)
            }
        }
    };
    // The environment the kubelet computed — the pod's `env`, `envFrom`, and the
    // service variables (`KUBERNETES_SERVICE_HOST`/`_PORT`) it adds to every
    // container. Nothing read `cfg.envs` before this: no container this CRI ever
    // ran got a single variable from its pod. Measured 2026-09-15 on a kubeadm
    // node: CoreDNS dying on «unable to load in-cluster configuration,
    // KUBERNETES_SERVICE_HOST and KUBERNETES_SERVICE_PORT must be defined».
    let env_file0 = if cfg.envs.is_empty() {
        String::new()
    } else {
        write_env_file(base, &id, &cfg.envs)?
    };
    let rec = ContainerRec {
        id: id.clone(),
        sandbox_id: req.pod_sandbox_id,
        name: md.name,
        attempt: md.attempt,
        image,
        command: cfg.command,
        args: cfg.args,
        created_at: now_ns(),
        started: false,
        started_at: 0,
        finished_at: 0,
        log_path: full_log_path,
        resources: CriResources::from_cri(cfg.linux.as_ref().and_then(|l| l.resources.as_ref())),
        labels: cfg.labels,
        annotations: cfg.annotations,
        readonly_rootfs,
        privileged,
        seccomp_unconfined,
        seccomp_profile_path,
        mounts: cfg.mounts.iter().map(CriMount::from_cri).collect(),
        volumes: cri_mount_specs(&cfg.mounts)?,
        supplemental_groups: sc
            .map(|s| s.supplemental_groups.clone())
            .unwrap_or_default(),
        masked_paths: sc.map(|s| s.masked_paths.clone()).unwrap_or_default(),
        readonly_paths: sc.map(|s| s.readonly_paths.clone()).unwrap_or_default(),
        no_new_privs: sc.map(|s| s.no_new_privs).unwrap_or(false),
        cap_add,
        cap_drop,
        apparmor,
        run_as_user,
        run_as_group,
        run_as_username,
        env_file0,
    };
    write_rec(&ct_dir(base), &id, &rec)?;
    delonix_telemetry::metrics::inc_container_created();
    Ok(Response::new(CreateContainerResponse { container_id: id }))
}

/// The `--cap-*` flags for this container's `delonix run`.
///
/// With a node ceiling in force the final set is computed ONCE (engine semantics
/// ∩ ceiling — see [`crate::cap_ceiling`]) and emitted as `--cap-drop ALL` plus
/// explicit adds. Without a ceiling the flags are exactly what they always were:
/// `privileged` keeps its `--cap-add ALL` and the pod's own add/drop lists pass
/// through verbatim.
///
/// NOTE: the ceiling bounds capabilities ONLY. The other facets of `privileged`
/// (unconfined seccomp, writable `/sys`, its own cgroup namespace) are deliberately
/// untouched — clamping capabilities does not make a privileged pod safe, and
/// pretending otherwise would be the more dangerous outcome.
///
/// Extracted from the middle of `start_container`'s argv building so that BOTH
/// paths are testable, including the legacy one: "unchanged when there is no
/// ceiling" is a claim that deserves a test, not a reading.
fn cap_flags(rec: &ContainerRec, ceiling: crate::CapCeiling, id: &str) -> Vec<String> {
    match ceiling.cap_args(&rec.cap_add, &rec.cap_drop, rec.privileged) {
        Some(capped) => {
            if ceiling_reduces(&capped, rec) {
                tracing::warn!(
                    container = %id,
                    privileged = rec.privileged,
                    requested_add = %rec.cap_add.join(","),
                    ceiling = %ceiling.describe(),
                    effective = %capped
                        .chunks(2)
                        .filter(|p| p[0] == "--cap-add")
                        .map(|p| p[1].as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                    "cri: capabilities clamped by the node ceiling"
                );
            }
            capped
        }
        None => {
            let mut args = Vec::new();
            if rec.privileged {
                args.push("--cap-add".to_string());
                args.push("ALL".to_string());
            }
            for c in &rec.cap_add {
                args.push("--cap-add".to_string());
                args.push(c.trim_start_matches("CAP_").to_string());
            }
            for c in &rec.cap_drop {
                args.push("--cap-drop".to_string());
                args.push(c.trim_start_matches("CAP_").to_string());
            }
            args
        }
    }
}

/// Whether the clamped argv takes away something the container EXPLICITLY asked
/// for — as opposed to merely lowering the engine's implicit default set, which is
/// the ceiling's ordinary job and would otherwise log a line on every single
/// container start on the node.
fn ceiling_reduces(capped: &[String], rec: &ContainerRec) -> bool {
    if rec.privileged {
        // `privileged` asks for every capability the kernel has; a ceiling narrow
        // enough to be expressed as names always gives back less.
        return true;
    }
    let granted: Vec<&str> = capped
        .chunks(2)
        .filter(|p| p[0] == "--cap-add")
        .map(|p| p[1].as_str())
        .collect();
    rec.cap_add.iter().any(|c| {
        let want = c.trim_start_matches("CAP_");
        !granted.iter().any(|g| g.eq_ignore_ascii_case(want))
    })
}

/// The run specification of a CRI container, built as data.
///
/// It used to be the ARGV of `delonix container run`, which the CLI then parsed
/// back into this same structure — two translations of one thing, and the
/// boundary where a wrong flag showed up only as a cluster that did not start
/// (`hostNetwork: true` spent months not being the host's network). Pure and
/// testable, like the argv was.
fn start_run_opts(
    rec: &ContainerRec,
    sandbox: Option<&SandboxRec>,
    ceiling: crate::CapCeiling,
    id: &str,
    env: Vec<String>,
) -> delonix_compute::RunOpts {
    // The CLI's defaults for the fields this spec does not decide: the same run
    // specification `container run -d` builds.
    let mut o = delonix_compute::RunOpts {
        detach: true,
        name: Some(format!("cri-{id}")),
        net: "host".into(),
        restart: "no".into(),
        wait_timeout: 60,
        image: rec.image.clone(),
        env,
        ..Default::default()
    };
    apply_resources(&rec.resources, &mut o);
    // The kubelet's hierarchy (ADR 0038). Only with it do unspecified limits mean no limit.
    if let Some(sb) = sandbox.filter(|sb| !sb.cgroup_parent.is_empty()) {
        o.kube_cgroup_parent = Some(sb.cgroup_parent.clone());
    }
    // Logs in the path/format the kubelet/crictl expect (CRI), if any.
    if !rec.log_path.is_empty() {
        o.log_file = Some(rec.log_path.clone());
        o.log_cri = true;
    }
    if let Some(sb) = sandbox {
        if sb.host_network || !sb.cni_netns.is_empty() {
            // hostNetwork is the host's network; a root CNI sandbox is entered by
            // `nsenter --net` around the whole run, so inside it `host` is the pod's.
            o.net = "host".into();
        } else {
            o.pod = Some(format!("cri-{}", rec.sandbox_id));
        }
        if !sb.hostname.is_empty() {
            o.hostname = Some(sb.hostname.clone());
        }
        o.host_pid = sb.host_pid;
        o.host_ipc = sb.host_ipc;
        o.dns = sb.dns_servers.clone();
        o.dns_search = sb.dns_searches.clone();
        o.dns_option = sb.dns_options.clone();
        // Ports go through the engine's own netns only: on the host's network the
        // process binds the host's ports, and a CNI sandbox leaves them to the
        // `portmap` plugin.
        if !sb.host_network && sb.cni_netns.is_empty() {
            o.ports = sb.port_mappings.clone();
        }
        // The `net.*` sysctls of a CNI sandbox were applied to its netns at
        // creation; under `--net host` the engine would refuse them.
        let own_net = !sb.cni_netns.is_empty();
        o.sysctl = sb
            .sysctls
            .iter()
            .filter(|s| !(own_net && s.starts_with("net.")))
            .cloned()
            .collect();
    }
    o.volumes = rec.volumes.clone();
    o.read_only = rec.readonly_rootfs;
    o.group_add = rec
        .supplemental_groups
        .iter()
        .map(|g| g.to_string())
        .collect();
    // A privileged container gets neither masked nor read-only paths.
    if !rec.privileged {
        o.masked_path = rec.masked_paths.clone();
        o.readonly_path = rec.readonly_paths.clone();
    }
    o.privileged = rec.privileged;
    o.security_opt
        .push(format!("no-new-privileges={}", rec.no_new_privs));
    if rec.privileged || rec.seccomp_unconfined {
        o.security_opt.push("seccomp=unconfined".into());
    } else if let Some(p) = &rec.seccomp_profile_path {
        o.security_opt.push(format!("seccomp={p}"));
    }
    // The node's capability ceiling decides; its answer is `--cap-add`/`--cap-drop`
    // pairs, read back into the two lists.
    for pair in cap_flags(rec, ceiling, id).chunks(2) {
        match (pair[0].as_str(), pair.get(1)) {
            ("--cap-add", Some(c)) => o.cap_add.push(c.clone()),
            ("--cap-drop", Some(c)) => o.cap_drop.push(c.clone()),
            _ => {}
        }
    }
    o.apparmor = rec.apparmor.clone();
    let user_part = if !rec.run_as_username.is_empty() {
        Some(rec.run_as_username.clone())
    } else {
        rec.run_as_user.map(|u| u.to_string())
    };
    o.user = user_part.map(|u| match rec.run_as_group {
        Some(g) => format!("{u}:{g}"),
        None => u,
    });
    o.command = rec.command.iter().chain(rec.args.iter()).cloned().collect();
    o
}

/// Writes the run specification for `delonix __apirun`: `0600` in a `0700`
/// directory, a unique name, never followed if pre-created. It carries the pod's
/// environment, which must never appear in an argv (`ps` shows it for the life of
/// the process).
fn write_run_spec(
    base: &Path,
    opts: &delonix_compute::RunOpts,
) -> Result<std::path::PathBuf, Status> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    let dir = base.join("cri").join("run");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(st)?;
    let path = dir.join(format!(
        "{}-{}.json",
        std::process::id(),
        delonix_node::generate_id()
    ));
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(st)?;
    serde_json::to_writer(f, opts).map_err(st)?;
    Ok(path)
}

pub fn start_container(
    base: &Path,
    id: String,
    ceiling: crate::CapCeiling,
) -> Result<Response<StartContainerResponse>, Status> {
    let mut rec: ContainerRec = read_rec(&ct_dir(base), &id)?;
    let sandbox = read_rec::<SandboxRec>(&sb_dir(base), &rec.sandbox_id).ok();
    // O `--log-file` precisa que o directório exista ANTES de o motor abrir o
    // ficheiro; é o único efeito lateral que sobra deste caminho.
    if !rec.log_path.is_empty() {
        if let Some(dir) = std::path::Path::new(&rec.log_path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
    }
    let env = if rec.env_file0.is_empty() {
        Vec::new()
    } else {
        let bytes = std::fs::read(&rec.env_file0).map_err(st)?;
        delonix_compute::run::parse_env0(&bytes, &rec.env_file0).map_err(st)?
    };
    // ADR 0038 item 3: before anything starts, not guessed at write time.
    let kube_cgroup_parent = sandbox
        .as_ref()
        .map(|sb| sb.cgroup_parent.as_str())
        .filter(|s| !s.is_empty());
    refuse_unenforceable_resources(&rec.resources, kube_cgroup_parent)?;
    let opts = start_run_opts(&rec, sandbox.as_ref(), ceiling, &id, env);
    // The run specification goes to the engine as data, not argv — one translation
    // (above) instead of two — in a fresh single-threaded process, where the
    // supervisor's `fork` is safe.
    let spec = write_run_spec(base, &opts)?;
    let spec_arg = spec.to_string_lossy().into_owned();
    // WITH the engine's reason. The bare "failed to start container <id>" hid
    // a refusal that named its own fix, and it took reproducing the argv by
    // hand on the node to read it (2026-09-15) — the same trap
    // `delonix_detached_why` already documents for sandboxes.
    let netns = sandbox
        .as_ref()
        .filter(|sb| !sb.host_network && !sb.cni_netns.is_empty())
        .map(|sb| sb.cni_netns.as_str());
    let started = delonix_detached_why_in(base, netns, &["__apirun", &spec_arg]);
    let _ = std::fs::remove_file(&spec);
    if let Some(why) = started? {
        return Err(Status::internal(format!(
            "failed to start container {id}: {why}"
        )));
    }
    rec.started = true;
    rec.started_at = now_ns();
    write_rec(&ct_dir(base), &id, &rec)?;
    Ok(Response::new(StartContainerResponse {}))
}

/// PURE. The `cpu.max` ratio string an `UpdateContainerResources` call must write to
/// the live cgroup, or the refusal it must answer with instead of writing one.
///
/// [`CriResources::merge_update`] pairs `cpu_quota`/`cpu_period` together: a request
/// that only sends ONE of the two (a bare `crictl update --cpu-quota N`, with no
/// `--cpu-period`) merges to a pair where the field the caller did NOT send is 0 — by
/// design, see that function's own doc comment. Dividing that pair blindly used to do
/// one of two things, and only one of them failed loudly: a given period over a ZERO
/// quota computed `0.000` cores, which [`cpu_quota_usec_or_unlimited`]
/// (`delonix_linux`) correctly rejects (`v > 0.0`); but a given QUOTA over a zero
/// PERIOD computed `quota / period.max(1)` = `quota` cores (e.g. `50000.000`), which
/// that same guard happily accepts — and `update_kube_resources` then writes to the
/// live cgroup as an absurd, effectively unenforced CPU limit, SILENTLY. Measured: a
/// partial update that asked only for `cpu_quota: 50000` wrote a quota 50000 CORES
/// wide, removing whatever throttling the container had instead of changing it.
///
/// This treats both halves of the pair the same way: a touched request that does not
/// resolve, after the merge, to a positive quota AND a positive period is refused up
/// front — never silently computed into nonsense, never silently dropped.
fn cpus_for_update(
    asked: &CriResources,
    merged: &CriResources,
) -> std::result::Result<Option<String>, Status> {
    if asked.cpu_quota == 0 && asked.cpu_period == 0 {
        return Ok(None);
    }
    if merged.cpu_quota > 0 && merged.cpu_period > 0 {
        Ok(Some(format!(
            "{:.3}",
            merged.cpu_quota as f64 / merged.cpu_period as f64
        )))
    } else {
        Err(Status::invalid_argument(format!(
            "cpu_quota and cpu_period must both be positive after this update (asked \
             quota={}, period={}; merged quota={}, period={}) — a partial update that \
             only sends one of the two must send it together with the other",
            asked.cpu_quota, asked.cpu_period, merged.cpu_quota, merged.cpu_period
        )))
    }
}

/// CRI `UpdateContainerResources` (ADR 0038 item 4) — the live counterpart of
/// `StartContainer`'s own resource path, on a container already created (and
/// usually running; a stopped one is a normal case too, see below).
///
/// Honour-or-refuse FIRST, reusing [`refuse_unenforceable_resources`] — the
/// identical check `StartContainer` already runs, against the SAME question
/// (does the leaf this container is actually in have the controller?), before
/// any write. Then merges the request into the container's recorded resources
/// ([`CriResources::merge_update`] — this call is partial, never a full
/// replace) and applies it: [`delonix_linux::update_kube_resources`] dispatches
/// to `SetUnitProperties` under a kubelet systemd scope or a direct cgroup
/// write otherwise (ADR 0038 item 1's placement decides which).
///
/// The record is updated whether or not the container is CURRENTLY live — a
/// stopped container has nothing to write to right now, and the merge is
/// exactly what the next `start` is supposed to pick up (the same `Deferred`
/// case `update_limits` already names for the CLI's `container update`). A
/// container that LOOKS alive but whose cgroup vanished out from under it
/// (`NotEnforced`) is not an error either — by the time anyone could act on
/// one, the race that caused it is already over — but it is logged, because
/// `UpdateContainerResourcesResponse` has no field to carry the distinction
/// back to the kubelet.
pub fn update_container_resources(
    base: &Path,
    req: UpdateContainerResourcesRequest,
) -> Result<Response<UpdateContainerResourcesResponse>, Status> {
    let mut rec: ContainerRec = read_rec(&ct_dir(base), &req.container_id)?;
    let sandbox = read_rec::<SandboxRec>(&sb_dir(base), &rec.sandbox_id).ok();
    let kube_cgroup_parent = sandbox
        .as_ref()
        .map(|sb| sb.cgroup_parent.as_str())
        .filter(|s| !s.is_empty());
    let asked = CriResources::from_cri(req.linux.as_ref());
    refuse_unenforceable_resources(&asked, kube_cgroup_parent)?;
    let merged = rec.resources.merge_update(&asked);

    let store = delonix_state::Store::open(base.join("containers")).map_err(st)?;
    let id = format!("cri-{}", req.container_id);
    let container = store.load(&id).map_err(st)?;

    // What THIS request actually asked to change — recomputed from the full
    // MERGED pair for cpus (never just `asked`'s own cpu_quota/cpu_period),
    // so touching only one of the two still reads a consistent core count,
    // or is refused instead of computed into nonsense (`cpus_for_update`).
    let memory =
        (asked.memory_limit_in_bytes != 0).then(|| merged.memory_limit_in_bytes.to_string());
    let cpus = cpus_for_update(&asked, &merged)?;
    let cpu_weight =
        (asked.cpu_shares != 0).then(|| shares_to_weight(merged.cpu_shares).to_string());
    let cpuset = (!asked.cpuset_cpus.is_empty()).then(|| merged.cpuset_cpus.clone());
    let cpuset_mems = (!asked.cpuset_mems.is_empty()).then(|| merged.cpuset_mems.clone());
    let oom_score_adj = (asked.oom_score_adj != 0)
        .then(|| merged.oom_score_adj.clamp(i32::MIN as i64, i32::MAX as i64) as i32);

    let update = delonix_linux::ResourceUpdate {
        memory: memory.as_deref(),
        cpus: cpus.as_deref(),
        cpu_weight: cpu_weight.as_deref(),
        cpuset: cpuset.as_deref(),
        cpuset_mems: cpuset_mems.as_deref(),
        hugepage_limits: &asked.hugepage_limits,
        unified: &asked.unified,
        oom_score_adj,
    };
    match delonix_linux::update_kube_resources(&container, &update) {
        Ok(delonix_linux::LimitUpdate::NotEnforced) => tracing::warn!(
            container = %req.container_id,
            "cri: UpdateContainerResources did not reach a live cgroup — the \
             container looked alive and its cgroup is gone"
        ),
        Ok(_) => {}
        Err(e) => return Err(Status::internal(e.to_string())),
    }

    // Persisted regardless of liveness — see the function doc for why a
    // stopped container is a normal case here, not an error.
    store
        .update(&id, |c| {
            if let Some(m) = &memory {
                c.memory_max = m.clone();
            }
            if let Some(cp) = &cpus {
                c.cpus = cp.clone();
            }
            if cpu_weight.is_some() {
                c.cpu_weight = cpu_weight.clone();
            }
            if cpuset.is_some() {
                c.cpuset = cpuset.clone();
            }
            if cpuset_mems.is_some() {
                c.cpuset_mems = cpuset_mems.clone();
            }
            if !asked.hugepage_limits.is_empty() {
                c.hugepage_limits = merged.hugepage_limits.clone();
            }
            if !asked.unified.is_empty() {
                c.unified = merged.unified.clone();
            }
            if oom_score_adj.is_some() {
                c.oom_score_adj = oom_score_adj;
            }
            true
        })
        .map_err(st)?;

    // The CRI's own record must agree, or a later `start` — which rebuilds
    // the run spec from `rec.resources`, not from the engine's own record —
    // would silently discard this update the moment the container restarts.
    rec.resources = merged;
    write_rec(&ct_dir(base), &req.container_id, &rec)?;

    Ok(Response::new(UpdateContainerResourcesResponse {}))
}

pub fn stop_container(
    base: &Path,
    id: String,
    timeout: i64,
) -> Result<Response<StopContainerResponse>, Status> {
    // Cada paragem é REGISTADA, e não é cosmética: este caminho apagava as suas
    // próprias pistas.
    //
    // Um `StopContainer` não deixava rasto nenhum no journal, só o SIGTERM do
    // lado do container. A depurar um control-plane que não estabilizava
    // (2026-08-17), um `journalctl -u delonix-cri | grep -i stop` devolvia
    // ZERO — o que levou à conclusão errada de que «ninguém manda parar». O
    // actor só apareceu por acidente, num `systemd-cgls` que mostrou um
    // `delonix container stop cri-…` vivo dentro do cgroup do serviço.
    //
    // Ausência de log lida como ausência de facto custou duas conclusões
    // erradas na mesma investigação. Quem para um container do kubelet fica
    // agora escrito, com o id e o prazo, do lado de quem o executa.
    tracing::info!(
        container = %format!("cri-{id}"),
        grace_secs = timeout.max(0),
        "CRI StopContainer"
    );
    // Honor the CRI request's grace period (seconds): the kubelet/crictl impose
    // their own deadline, so we CANNOT use `delonix stop`'s long default.
    // `timeout=0` → immediate stop (SIGKILL).
    let secs = timeout.max(0).to_string();
    let out = delonix(
        base,
        &["container", "stop", "-t", &secs, &format!("cri-{id}")],
    )?;
    if !out.status.success() {
        tracing::warn!(container = %format!("cri-{id}"), status = %out.status,
            stderr = %String::from_utf8_lossy(&out.stderr).trim(), "container stop failed");
    }
    // Verify it actually STOPPED (reconciled), whatever `stop` printed: an absent
    // container is stopped (the CRI's idempotence), an alive one is not — and a
    // stderr that merely MENTIONED «not found» used to return success here without
    // looking. If it is still alive, propagate an error → the kubelet retries (instead
    // of assuming it stopped and moving on to RemoveContainer on a still-running
    // process).
    if let Some(c) = load_reconciled(base, &id) {
        let alive = matches!(c.status, delonix_model::records::Status::Running) && c.is_live();
        if alive {
            tracing::warn!(container = %format!("cri-{id}"), "ainda a correr depois do stop — o kubelet vai repetir");
            return Err(Status::internal(format!(
                "'cri-{id}' is still running after stop"
            )));
        }
    }
    Ok(Response::new(StopContainerResponse {}))
}

pub fn remove_container(
    base: &Path,
    id: String,
) -> Result<Response<RemoveContainerResponse>, Status> {
    // Registado pela mesma razão que o `StopContainer`: sem isto, a sequência
    // stop→remove que o kubelet faz num pod em churn é invisível deste lado.
    tracing::info!(container = %format!("cri-{id}"), "CRI RemoveContainer");
    // ONLY delete the CRI record AFTER the runtime removes the container. Before,
    // the JSON was deleted even with a failed `rm -f` → leak of rootfs/subuid/netns
    // with no trace for the kubelet to retry. Idempotent (CRI contract): a container
    // that no longer exists counts as removed.
    // The engine's store is the verdict (not a phrase on stderr), and the env file
    // written for this container goes with it (#322).
    engine_remove(base, &id)?;
    if valid_cri_id(&id) {
        let _ = std::fs::remove_file(env_path(base, &id));
    }
    remove_rec(&ct_dir(base), &id);
    Ok(Response::new(RemoveContainerResponse {}))
}

fn to_container(base: &Path, r: &ContainerRec) -> Container {
    Container {
        id: r.id.clone(),
        pod_sandbox_id: r.sandbox_id.clone(),
        metadata: Some(ContainerMetadata {
            name: r.name.clone(),
            attempt: r.attempt,
        }),
        image: Some(ImageSpec {
            image: r.image.clone(),
            ..Default::default()
        }),
        image_ref: r.image.clone(),
        state: delonix_state(base, &r.id),
        created_at: r.created_at,
        labels: r.labels.clone(),
        annotations: r.annotations.clone(),
        image_id: r.image.clone(),
    }
}

/// Does this container satisfy the kubelet's `ContainerFilter`?
///
/// `pod_sandbox_id` is the field that matters most here: the kubelet builds a pod's status
/// by listing the containers of THAT pod's sandbox. Ignoring it hands every container on
/// the node to every pod — and the kubelet then reads the ones it cannot find in the pod
/// spec as containers it must kill. Same shape and same reasoning as `sandbox_matches`.
fn container_matches(c: &Container, f: &ContainerFilter) -> bool {
    if !f.id.is_empty() && c.id != f.id {
        return false;
    }
    if !f.pod_sandbox_id.is_empty() && c.pod_sandbox_id != f.pod_sandbox_id {
        return false;
    }
    if let Some(st) = &f.state {
        if c.state != st.state {
            return false;
        }
    }
    f.label_selector
        .iter()
        .all(|(k, v)| c.labels.get(k) == Some(v))
}

pub fn list_containers(
    base: &Path,
    filter: Option<ContainerFilter>,
) -> Result<Response<ListContainersResponse>, Status> {
    let f = filter.unwrap_or_default();
    let containers = list_recs::<ContainerRec>(&ct_dir(base))
        .iter()
        .map(|r| to_container(base, r))
        .filter(|c| container_matches(c, &f))
        .collect();
    Ok(Response::new(ListContainersResponse { containers }))
}

pub fn container_status(
    base: &Path,
    id: String,
    verbose: bool,
) -> Result<Response<ContainerStatusResponse>, Status> {
    let mut r: ContainerRec = read_rec(&ct_dir(base), &id)?;
    // Real exit code (from the Store), so the kubelet sees the exit cause instead
    // of a fixed `0`. `finished_at`/`reason` follow along.
    let exit = delonix_exit(base, &r.id);
    // Persist the finish time only the FIRST time an exit is observed — see the
    // BUG FIXED note on `ContainerRec::finished_at`. Best-effort: a failed write
    // here still reports a correct (just not yet durable) timestamp this call.
    if exit.is_some() && r.finished_at == 0 {
        r.finished_at = now_ns();
        let _ = write_rec(&ct_dir(base), &id, &r);
    }
    // `started_at` falls back to `created_at` only for records written before this
    // fix (upgrade path) — old JSON on disk never persisted a real start time.
    let started_at = if !r.started {
        0
    } else if r.started_at != 0 {
        r.started_at
    } else {
        r.created_at
    };
    let status = ContainerStatus {
        id: r.id.clone(),
        metadata: Some(ContainerMetadata {
            name: r.name.clone(),
            attempt: r.attempt,
        }),
        state: delonix_state(base, &r.id),
        created_at: r.created_at,
        started_at,
        finished_at: r.finished_at,
        exit_code: exit.unwrap_or(0),
        image: Some(ImageSpec {
            image: r.image.clone(),
            ..Default::default()
        }),
        image_ref: r.image.clone(),
        log_path: r.log_path.clone(),
        reason: exit_reason(exit, delonix_oom_killed(base, &r.id)),
        // Preserve the CreateContainer attributes — the conformance spec
        // `preserving container attributes` requires labels/annotations to come
        // back exactly as they were set; with `..Default::default()` they came empty.
        labels: r.labels.clone(),
        annotations: r.annotations.clone(),
        // Mounts came back EMPTY, which reads as "this container has no
        // volumes" — the opposite of the truth for anything a kubelet mounts.
        mounts: r.mounts.iter().map(CriMount::to_cri).collect(),
        ..Default::default()
    };
    // `info` is only populated for a verbose request (CRI contract) — same
    // gating `status()` already uses for `capabilityCeiling`. The ADR-0062
    // fallback goes here so `crictl inspect --output json <id>` (or a kubelet
    // that logs `.info` on an unexpected restart) can show it, instead of a
    // `runAsNonRoot` Pod running as root with nothing in `ContainerStatus` to
    // say so.
    let mut info = std::collections::HashMap::new();
    if verbose {
        info.insert(
            "userFallbackToRoot".to_string(),
            delonix_user_fallback_to_root(base, &r.id).to_string(),
        );
    }
    Ok(Response::new(ContainerStatusResponse {
        status: Some(status),
        info,
    }))
}

// ---------------------------------------------------------------------------
// ExecSync: runs a command in the container and returns stdout/stderr/exit. It's
// what the kubelet uses for `exec` probes (liveness/readiness) and `crictl exec -s`.
// ---------------------------------------------------------------------------

pub fn exec_sync(
    base: &Path,
    id: String,
    cmd: Vec<String>,
    timeout: i64,
) -> Result<Response<ExecSyncResponse>, Status> {
    if cmd.is_empty() {
        return Err(Status::invalid_argument("exec_sync without a command"));
    }
    let name = format!("cri-{id}");
    // Delegates to the `delonix exec` binary (single-threaded; does setns into the
    // container). The timeout (seconds, >0) is enforced by the `timeout` coreutil
    // for robustness.
    let mut command = Command::new(delonix_bin());
    command
        .env("DELONIX_ROOT", base)
        .env("DELONIX_INTERNAL", "1");
    if timeout > 0 {
        command = Command::new("timeout");
        command
            .env("DELONIX_ROOT", base)
            .env("DELONIX_INTERNAL", "1")
            .arg(timeout.to_string())
            .arg(delonix_bin());
    }
    let (status, stdout, stderr) = run_capped(
        command.arg("container").arg("exec").arg(&name).args(&cmd),
        EXEC_SYNC_MAX_OUTPUT,
    )
    .map_err(st)?;
    // `timeout` returns 124 when it expires → maps to a distinct exit code.
    let exit_code = status.code().unwrap_or(-1);
    Ok(Response::new(ExecSyncResponse {
        stdout,
        stderr,
        exit_code,
    }))
}

/// Per-stream ceiling for an `ExecSync` answer. `Command::output()` buffered
/// everything: a probe (or a compromised pod's `exec`) writing without pause
/// grew the CRI server's memory until the timeout killed it, and the whole
/// buffer was then serialized into the gRPC reply. 16 MiB is already the gRPC
/// default message size, so nothing a kubelet could receive is lost.
const EXEC_SYNC_MAX_OUTPUT: usize = 16 * 1024 * 1024;

/// Reads `r` keeping at most `max` bytes but DRAINING the rest, so the child
/// never blocks on a full pipe (which, with no timeout, would hang it forever).
fn read_keep_first<R: std::io::Read>(mut r: R, max: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if kept.len() < max {
                    let take = n.min(max - kept.len());
                    kept.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
    kept
}

/// Like `Command::output()`, but with bounded memory per stream.
fn run_capped(
    cmd: &mut Command,
    max: usize,
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = child.stdout.take().expect("piped");
    let err = child.stderr.take().expect("piped");
    let t = std::thread::spawn(move || read_keep_first(err, max));
    let stdout = read_keep_first(out, max);
    let stderr = t.join().unwrap_or_default();
    let status = child.wait()?;
    Ok((status, stdout, stderr))
}

#[cfg(test)]
mod exec_sync_cap_tests {
    use super::*;

    #[test]
    fn output_beyond_the_cap_is_dropped_but_the_child_still_finishes() {
        let mut c = Command::new("sh");
        c.arg("-c").arg("head -c 3000000 /dev/zero; echo err >&2");
        let (st, out, err) = run_capped(&mut c, 1000).unwrap();
        assert!(st.success());
        assert_eq!(out.len(), 1000);
        assert_eq!(err, b"err\n");
    }

    #[test]
    fn small_output_is_returned_whole() {
        let mut c = Command::new("sh");
        c.arg("-c").arg("printf hello");
        let (_, out, _) = run_capped(&mut c, 1000).unwrap();
        assert_eq!(out, b"hello");
    }
}

// ---------------------------------------------------------------------------
// Metrics (CRI stats) — real, read from the container's cgroup v2. It's what the
// kubelet uses for the Summary API / HPA. C2.
// ---------------------------------------------------------------------------

/// Reads an integer from a cgroup file (`memory.current`, `pids.current`, …).
fn cg_u64(cgroup: &str, file: &str) -> u64 {
    std::fs::read_to_string(format!("{cgroup}/{file}"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Reads a `key value` field from a `cpu.stat`/`memory.stat`-style file.
fn cg_field(cgroup: &str, file: &str, key: &str) -> u64 {
    std::fs::read_to_string(format!("{cgroup}/{file}"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                let mut it = l.split_whitespace();
                (it.next() == Some(key)).then(|| it.next().and_then(|v| v.parse().ok()))?
            })
        })
        .unwrap_or(0)
}

/// The cgroup of a CRI container (`cri-<id>`), via Delonix's `Store`.
fn container_cgroup(base: &Path, cri_id: &str) -> Option<String> {
    let store = delonix_state::Store::open(base.join("containers")).ok()?;
    store
        .load(&format!("cri-{cri_id}"))
        .ok()
        .map(|c| c.cgroup())
}

/// Sums the `VmRSS` (bytes) of all the cgroup's processes, reading `/proc`. It's
/// the memory source when the cgroup's `memory.current` under-reports (the init is
/// placed into the cgroup after the *exec*, so pages faulted before are not
/// charged to this cgroup — but the PIDs ARE here, and `/proc` tells the truth).
fn cgroup_rss_bytes(cgroup: &str) -> u64 {
    let procs = match std::fs::read_to_string(format!("{cgroup}/cgroup.procs")) {
        Ok(p) => p,
        Err(_) => return 0,
    };
    let mut total = 0u64;
    for pid in procs.lines() {
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", pid.trim())) {
            for l in status.lines() {
                if let Some(rest) = l.strip_prefix("VmRSS:") {
                    if let Some(kb) = rest
                        .split_whitespace()
                        .next()
                        .and_then(|v| v.parse::<u64>().ok())
                    {
                        total += kb * 1024;
                    }
                }
            }
        }
    }
    total
}

fn u64v(value: u64) -> Option<UInt64Value> {
    Some(UInt64Value { value })
}

/// The cgroup v2 numbers behind BOTH the (older) Stats API and the (newer)
/// generic Metrics API — one read, two shapes. Extracted so
/// `list_pod_sandbox_metrics` does not grow a second, drifting copy of the
/// same cgroup-field math `container_stats_for` already has.
struct ContainerCgroupMetrics {
    cpu_ns: u64,
    usage_bytes: u64,
    working_set_bytes: u64,
    rss_bytes: u64,
    pgfault: u64,
    pgmajfault: u64,
    swap_bytes: u64,
}

fn container_cgroup_metrics(base: &Path, id: &str) -> ContainerCgroupMetrics {
    let cg = container_cgroup(base, id);
    match &cg {
        Some(cg) => {
            let cpu_us = cg_field(cg, "cpu.stat", "usage_usec");
            let cur = cg_u64(cg, "memory.current");
            let inactive = cg_field(cg, "memory.stat", "inactive_file");
            let anon = cg_field(cg, "memory.stat", "anon");
            // The cgroup under-reports memory (late charging); falls back to the
            // real RSS of the cgroup's processes, which is the observable truth.
            let (usage, working, rss) = if cur > 0 {
                (cur, cur.saturating_sub(inactive), anon)
            } else {
                let rss = cgroup_rss_bytes(cg);
                (rss, rss, rss)
            };
            ContainerCgroupMetrics {
                cpu_ns: cpu_us.saturating_mul(1000), // µs → ns
                usage_bytes: usage,
                working_set_bytes: working,
                rss_bytes: rss,
                pgfault: cg_field(cg, "memory.stat", "pgfault"),
                pgmajfault: cg_field(cg, "memory.stat", "pgmajfault"),
                swap_bytes: cg_u64(cg, "memory.swap.current"),
            }
        }
        None => ContainerCgroupMetrics {
            cpu_ns: 0,
            usage_bytes: 0,
            working_set_bytes: 0,
            rss_bytes: 0,
            pgfault: 0,
            pgmajfault: 0,
            swap_bytes: 0,
        },
    }
}

/// The directory holding a CRI container's writable layer, as the engine laid it
/// out: `<root>/containers/<engine id>/upper`, or the container directory itself
/// for a flat rootfs. `None` when the engine has no record or no directory.
///
/// The engine id is NOT the CRI name. This used to announce
/// `<root>/containers/cri-<cri id>` — the container's NAME — which never exists on
/// disk, so every `stat` the kubelet made failed («failed to get device for dir
/// …/containers/cri-<id>: stat failed … no such file or directory») and the
/// eviction manager got no summary stats at all: node-pressure eviction, the
/// kubelet's last line of node protection, was off on every node this runtime
/// served (measured 2026-09-15, k8s 1.36.4).
fn writable_layer_dir(base: &Path, cri_id: &str) -> Option<PathBuf> {
    let root = base.join("containers");
    let c = delonix_state::Store::open(&root)
        .ok()?
        .load(&format!("cri-{cri_id}"))
        .ok()?;
    let dir = root.join(&c.id);
    let upper = dir.join("upper");
    if upper.is_dir() {
        Some(upper)
    } else if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

/// Bytes on disk (allocated blocks, like `du`) and inodes under `dir`, without
/// following symlinks and without leaving `dir`'s filesystem. A hard link counts
/// once. Unreadable entries are skipped: a partial sum is still a lower bound,
/// which is the safe direction for eviction.
fn dir_usage(dir: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let Ok(top) = std::fs::symlink_metadata(dir) else {
        return (0, 0);
    };
    let dev = top.dev();
    let mut seen = std::collections::HashSet::new();
    let (mut bytes, mut inodes) = (0u64, 0u64);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(m) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if m.dev() != dev || !seen.insert(m.ino()) {
            continue;
        }
        bytes += m.blocks() * 512;
        inodes += 1;
        if m.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        }
    }
    (bytes, inodes)
}

/// The CRI `writable_layer` of a container: a mountpoint the kubelet can `stat`
/// and the usage measured there. A container the engine no longer knows reports
/// the engine root (which exists) with zero usage, instead of a path that fails.
fn writable_layer_usage(base: &Path, cri_id: &str, ts: i64) -> FilesystemUsage {
    let (mountpoint, (bytes, inodes)) = match writable_layer_dir(base, cri_id) {
        Some(d) => {
            let usage = dir_usage(&d);
            (d, usage)
        }
        None => (base.to_path_buf(), (0, 0)),
    };
    FilesystemUsage {
        timestamp: ts,
        fs_id: Some(FilesystemIdentifier {
            mountpoint: mountpoint.to_string_lossy().into_owned(),
        }),
        used_bytes: u64v(bytes),
        inodes_used: u64v(inodes),
    }
}

/// Builds a container's real metrics from its cgroup v2.
fn container_stats_for(base: &Path, r: &ContainerRec) -> ContainerStats {
    let ts = now_ns();
    let ContainerCgroupMetrics {
        cpu_ns,
        usage_bytes: mem_cur,
        working_set_bytes: working_set,
        rss_bytes: rss,
        pgfault,
        pgmajfault,
        swap_bytes,
    } = container_cgroup_metrics(base, &r.id);
    ContainerStats {
        attributes: Some(ContainerAttributes {
            id: r.id.clone(),
            metadata: Some(ContainerMetadata {
                name: r.name.clone(),
                attempt: r.attempt,
            }),
            labels: r.labels.clone(),
            annotations: r.annotations.clone(),
        }),
        cpu: Some(CpuUsage {
            timestamp: ts,
            usage_core_nano_seconds: u64v(cpu_ns),
            usage_nano_cores: u64v(0),
        }),
        memory: Some(MemoryUsage {
            timestamp: ts,
            working_set_bytes: u64v(working_set),
            available_bytes: u64v(0),
            usage_bytes: u64v(mem_cur),
            rss_bytes: u64v(rss),
            page_faults: u64v(pgfault),
            major_page_faults: u64v(pgmajfault),
        }),
        writable_layer: Some(writable_layer_usage(base, &r.id, ts)),
        swap: Some(SwapUsage {
            timestamp: ts,
            swap_available_bytes: u64v(0),
            swap_usage_bytes: u64v(swap_bytes),
        }),
    }
}

pub fn container_stats(
    base: &Path,
    id: String,
) -> Result<Response<ContainerStatsResponse>, Status> {
    let r: ContainerRec = read_rec(&ct_dir(base), &id)?;
    Ok(Response::new(ContainerStatsResponse {
        stats: Some(container_stats_for(base, &r)),
    }))
}

pub fn list_container_stats(
    base: &Path,
    filter: Option<ContainerStatsFilter>,
) -> Result<Response<ListContainerStatsResponse>, Status> {
    // `label_selector` was DROPPED. The filter has three fields and only two
    // were read, so a caller narrowing by label got every container back — a
    // filter that silently returns more than asked is worse than one that
    // errors, because the extra rows look like real answers.
    let (fid, fsb, flabels) = filter
        .map(|f| (f.id, f.pod_sandbox_id, f.label_selector))
        .unwrap_or_default();
    let stats = list_recs::<ContainerRec>(&ct_dir(base))
        .into_iter()
        .filter(|r| {
            (fid.is_empty() || r.id == fid)
                && (fsb.is_empty() || r.sandbox_id == fsb)
                // SUBSET, as the CRI defines it: every selector pair must match,
                // extra labels on the container are fine.
                && flabels
                    .iter()
                    .all(|(k, v)| r.labels.get(k).is_some_and(|got| got == v))
        })
        .map(|r| container_stats_for(base, &r))
        .collect();
    Ok(Response::new(ListContainerStatsResponse { stats }))
}

/// Metrics of a pod sandbox: aggregates the sandbox's containers (cpu/memory).
fn pod_sandbox_stats_for(base: &Path, sb: &SandboxRec) -> PodSandboxStats {
    let ts = now_ns();
    let conts: Vec<ContainerStats> = list_recs::<ContainerRec>(&ct_dir(base))
        .into_iter()
        .filter(|r| r.sandbox_id == sb.id)
        .map(|r| container_stats_for(base, &r))
        .collect();
    let sum = |pick: &dyn Fn(&ContainerStats) -> u64| conts.iter().map(pick).sum::<u64>();
    let cpu_ns = sum(&|c| {
        c.cpu
            .as_ref()
            .and_then(|x| x.usage_core_nano_seconds.as_ref())
            .map(|v| v.value)
            .unwrap_or(0)
    });
    let mem = sum(&|c| {
        c.memory
            .as_ref()
            .and_then(|x| x.usage_bytes.as_ref())
            .map(|v| v.value)
            .unwrap_or(0)
    });
    let ws = sum(&|c| {
        c.memory
            .as_ref()
            .and_then(|x| x.working_set_bytes.as_ref())
            .map(|v| v.value)
            .unwrap_or(0)
    });
    PodSandboxStats {
        attributes: Some(PodSandboxAttributes {
            id: sb.id.clone(),
            metadata: Some(PodSandboxMetadata {
                name: sb.name.clone(),
                namespace: sb.namespace.clone(),
                uid: sb.uid.clone(),
                attempt: sb.attempt,
            }),
            labels: sb.labels.clone(),
            annotations: sb.annotations.clone(),
        }),
        linux: Some(LinuxPodSandboxStats {
            cpu: Some(CpuUsage {
                timestamp: ts,
                usage_core_nano_seconds: u64v(cpu_ns),
                usage_nano_cores: u64v(0),
            }),
            memory: Some(MemoryUsage {
                timestamp: ts,
                working_set_bytes: u64v(ws),
                available_bytes: u64v(0),
                usage_bytes: u64v(mem),
                rss_bytes: u64v(0),
                page_faults: u64v(0),
                major_page_faults: u64v(0),
            }),
            network: None,
            process: Some(ProcessUsage {
                timestamp: ts,
                process_count: u64v(conts.len() as u64),
            }),
            containers: conts,
        }),
        windows: None,
    }
}

pub fn pod_sandbox_stats(
    base: &Path,
    id: String,
) -> Result<Response<PodSandboxStatsResponse>, Status> {
    let sb: SandboxRec = read_rec(&sb_dir(base), &id)?;
    Ok(Response::new(PodSandboxStatsResponse {
        stats: Some(pod_sandbox_stats_for(base, &sb)),
    }))
}

pub fn list_pod_sandbox_stats(
    base: &Path,
    filter: Option<PodSandboxStatsFilter>,
) -> Result<Response<ListPodSandboxStatsResponse>, Status> {
    let fid = filter.map(|f| f.id).unwrap_or_default();
    let stats = list_recs::<SandboxRec>(&sb_dir(base))
        .into_iter()
        .filter(|s| fid.is_empty() || s.id == fid)
        .map(|s| pod_sandbox_stats_for(base, &s))
        .collect();
    Ok(Response::new(ListPodSandboxStatsResponse { stats }))
}

/// The generic Metrics API (`ListPodSandboxMetrics`/`ListMetricDescriptors`)
/// is a DIFFERENT shape from the Stats API above — Prometheus-like `{name,
/// value, metric_type, labels}` tuples instead of a fixed struct — but it is
/// NOT a different measurement: it reads the exact same
/// `container_cgroup_metrics` this file already computes for `ContainerStats`.
/// Two shapes, one source of truth.
///
/// **A metric whose name never appeared in `ListMetricDescriptors` is
/// spec-defined to be IGNORED by the caller** ("Name must match a name
/// previously returned in a MetricDescriptors call, otherwise, it will be
/// ignored" — `api.proto`'s own doc-comment on `Metric.name`). Emitting
/// metrics without matching descriptors would be the same "accepted and
/// ignored" failure this codebase refuses elsewhere — the two lists below are
/// the single source both `list_metric_descriptors` and
/// `list_pod_sandbox_metrics` read, so they cannot drift apart.
struct MetricSpec {
    name: &'static str,
    help: &'static str,
    metric_type: MetricType,
}

const POD_METRICS: &[MetricSpec] = &[
    MetricSpec {
        name: "pod_cpu_usage_core_nanoseconds",
        help: "Cumulative CPU time consumed by the pod's containers, in nanoseconds.",
        metric_type: MetricType::Counter,
    },
    MetricSpec {
        name: "pod_memory_working_set_bytes",
        help: "Current working set memory of the pod's containers, in bytes.",
        metric_type: MetricType::Gauge,
    },
    MetricSpec {
        name: "pod_memory_usage_bytes",
        help: "Current memory usage of the pod's containers, in bytes.",
        metric_type: MetricType::Gauge,
    },
];

const CONTAINER_METRICS: &[MetricSpec] = &[
    MetricSpec {
        name: "container_cpu_usage_core_nanoseconds",
        help: "Cumulative CPU time consumed by the container, in nanoseconds.",
        metric_type: MetricType::Counter,
    },
    MetricSpec {
        name: "container_memory_working_set_bytes",
        help: "Current working set memory of the container, in bytes.",
        metric_type: MetricType::Gauge,
    },
    MetricSpec {
        name: "container_memory_usage_bytes",
        help: "Current memory usage of the container, in bytes.",
        metric_type: MetricType::Gauge,
    },
    MetricSpec {
        name: "container_memory_rss_bytes",
        help: "Current anonymous-memory (RSS) usage of the container, in bytes.",
        metric_type: MetricType::Gauge,
    },
];

pub fn list_metric_descriptors() -> Result<Response<ListMetricDescriptorsResponse>, Status> {
    let descriptors = POD_METRICS
        .iter()
        .chain(CONTAINER_METRICS.iter())
        .map(|m| MetricDescriptor {
            name: m.name.to_string(),
            help: m.help.to_string(),
            // No per-metric dimension beyond the pod/container id already
            // carried by `PodSandboxMetrics.pod_sandbox_id`/
            // `ContainerMetrics.container_id` — nothing to declare here.
            label_keys: vec![],
        })
        .collect();
    Ok(Response::new(ListMetricDescriptorsResponse { descriptors }))
}

fn container_metrics_for(base: &Path, r: &ContainerRec) -> ContainerMetrics {
    let m = container_cgroup_metrics(base, &r.id);
    let values = [m.cpu_ns, m.working_set_bytes, m.usage_bytes, m.rss_bytes];
    let metrics = CONTAINER_METRICS
        .iter()
        .zip(values)
        .map(|(spec, value)| Metric {
            name: spec.name.to_string(),
            // Live-gathered, not cached — the spec's own convention for this
            // field ("should be 0 if the metric was gathered live").
            timestamp: 0,
            metric_type: spec.metric_type as i32,
            label_values: vec![],
            value: u64v(value),
        })
        .collect();
    ContainerMetrics {
        container_id: r.id.clone(),
        metrics,
    }
}

pub fn list_pod_sandbox_metrics(
    base: &Path,
) -> Result<Response<ListPodSandboxMetricsResponse>, Status> {
    let pod_metrics = list_recs::<SandboxRec>(&sb_dir(base))
        .into_iter()
        .map(|sb| {
            let container_metrics: Vec<ContainerMetrics> = list_recs::<ContainerRec>(&ct_dir(base))
                .into_iter()
                .filter(|r| r.sandbox_id == sb.id)
                .map(|r| container_metrics_for(base, &r))
                .collect();
            // The pod-level totals are the same sum-of-containers
            // `pod_sandbox_stats_for` computes — just re-derived from
            // `ContainerMetrics` instead of `ContainerStats`, so the two APIs
            // can never report different pod totals for the same underlying
            // cgroups.
            let sum_of = |name: &str| -> u64 {
                container_metrics
                    .iter()
                    .flat_map(|cm| &cm.metrics)
                    .filter(|m| m.name == name)
                    .filter_map(|m| m.value.as_ref().map(|v| v.value))
                    .sum()
            };
            let pod_values = [
                sum_of("container_cpu_usage_core_nanoseconds"),
                sum_of("container_memory_working_set_bytes"),
                sum_of("container_memory_usage_bytes"),
            ];
            let metrics = POD_METRICS
                .iter()
                .zip(pod_values)
                .map(|(spec, value)| Metric {
                    name: spec.name.to_string(),
                    timestamp: 0,
                    metric_type: spec.metric_type as i32,
                    label_values: vec![],
                    value: u64v(value),
                })
                .collect();
            PodSandboxMetrics {
                pod_sandbox_id: sb.id.clone(),
                metrics,
                container_metrics,
            }
        })
        .collect();
    Ok(Response::new(ListPodSandboxMetricsResponse { pod_metrics }))
}

/// `ReopenContainerLog` — recreates the container's log file at its configured
/// path.
///
/// The shim is what actually redirects the writes (it compares the path's inode
/// with the one it holds open, before every batch). This half exists because the
/// caller checks the file the instant the call returns, and because a rotation
/// whose new file only appears on the next log line looks like data loss to
/// whatever is tailing it.
pub fn reopen_container_log(base: &Path, id: &str) -> Result<(), Status> {
    let rec: ContainerRec = read_rec(&ct_dir(base), id)
        .map_err(|_| Status::not_found(format!("no such container: {id}")))?;
    if rec.log_path.is_empty() {
        return Err(Status::failed_precondition(
            "container has no log path (created without one)",
        ));
    }
    if let Some(parent) = Path::new(&rec.log_path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| Status::internal(e.to_string()))?;
    }
    // `create_new` would fail when the file is still there — which is the case
    // whenever the caller rotates by COPY-truncate rather than rename. Opening
    // with `create(true)` and no truncate is right for both.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&rec.log_path)
        .map_err(|e| Status::internal(format!("{}: {e}", rec.log_path)))?;
    Ok(())
}

/// A CRI mount, persisted.
///
/// Mirrors the proto's fields rather than reusing the generated type: the
/// record is `serde` JSON on disk and the prost type is not `Serialize`. The
/// two conversions live next to each other so a field added to one is obvious
/// in the other.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CriMount {
    #[serde(default)]
    host_path: String,
    #[serde(default)]
    container_path: String,
    #[serde(default)]
    readonly: bool,
    #[serde(default)]
    selinux_relabel: bool,
    /// The proto's enum value, kept as the integer it arrived as. Mapping it to
    /// a name here and back would be two places to get wrong for no gain — the
    /// only consumer is the round-trip.
    #[serde(default)]
    propagation: i32,
}

impl CriMount {
    fn from_cri(m: &Mount) -> Self {
        Self {
            host_path: m.host_path.clone(),
            container_path: m.container_path.clone(),
            readonly: m.readonly,
            selinux_relabel: m.selinux_relabel,
            propagation: m.propagation,
        }
    }

    fn to_cri(&self) -> Mount {
        Mount {
            host_path: self.host_path.clone(),
            container_path: self.container_path.clone(),
            readonly: self.readonly,
            selinux_relabel: self.selinux_relabel,
            propagation: self.propagation,
            ..Default::default()
        }
    }
}

/// Translates the CRI's `Mount` list into the engine's `-v` specs.
///
/// The `-v` grammar splits on `:`, so a path containing one would be parsed into
/// the wrong pieces. The kubelet never produces such a path, but "never" is not
/// a validation: a colon is REFUSED here with the offending path named, rather
/// than quietly mounting somewhere else. Same rule this repo applies to every
/// other foreign-schema translator.
fn cri_mount_specs(mounts: &[Mount]) -> Result<Vec<String>, Status> {
    let mut out = Vec::with_capacity(mounts.len());
    for m in mounts {
        for (what, p) in [
            ("host_path", &m.host_path),
            ("container_path", &m.container_path),
        ] {
            if p.is_empty() {
                return Err(Status::invalid_argument(format!("mount with empty {what}")));
            }
            if p.contains(':') {
                return Err(Status::invalid_argument(format!(
                    "mount {what} {p:?} contains ':', which the volume spec uses as a separator"
                )));
            }
        }
        // The propagation names are the engine's (`rslave`/`rshared`), which are
        // also Docker's — the enum is the kubelet's, and the mapping is the
        // whole reason this function is not a `format!` at the call site.
        let prop = match MountPropagation::try_from(m.propagation) {
            Ok(MountPropagation::PropagationHostToContainer) => Some("rslave"),
            Ok(MountPropagation::PropagationBidirectional) => Some("rshared"),
            // PRIVATE is the default and needs no third field; an UNKNOWN value
            // is treated as private, which is the safe direction (no propagation
            // rather than more than asked).
            _ => None,
        };
        let spec = match (m.readonly, prop) {
            (false, None) => format!("{}:{}", m.host_path, m.container_path),
            (true, None) => format!("{}:{}:ro", m.host_path, m.container_path),
            (false, Some(p)) => format!("{}:{}:{p}", m.host_path, m.container_path),
            (true, Some(p)) => format!("{}:{}:ro,{p}", m.host_path, m.container_path),
        };
        out.push(spec);
    }
    Ok(out)
}

/// Translates the CRI's `PortMapping` list into `-p` specs, REFUSING any mapping
/// this node cannot publish.
///
/// A mapping with no `host_port` is DROPPED, not published on a random port:
/// the kubelet sends `container_port` alone for ports that are merely declared
/// (a `containerPort` with no `hostPort`), and publishing those would expose on
/// the node every port a pod ever named. That drop is what keeps an SCTP
/// `containerPort` — the shape an SCTP *Service* uses, since it goes through
/// kube-proxy and not through a hostPort — untouched by the refusal below.
///
/// **The transport is asked of [`delonix_sdn::Proto`], not decided here.** This
/// used to map `Protocol::Sctp` to the string `"sctp"` and an unknown protocol
/// number to `"tcp"`, i.e. it held a second opinion about what the engine can
/// publish — and it was wrong on both counts. Measured 2026-10-07:
///
/// * `slirp4netns` 1.2.1 (libslirp 4.7.0) REFUSES SCTP outright —
///   `add_hostfwd` with `"proto":"sctp"` answers `bad request:
///   add_hostfwd: bad arguments.proto`, where tcp and udp both return an id. So
///   it is the DATAPLANE that cannot carry it, not the spec parser.
///
/// **«does not implement», never «cannot».** The engine is rootless-FIRST, not
/// rootless-only (owner, 2026-10-07): a capability that needs root is built
/// behind root mode or cgroup delegation, not dropped. And SCTP publishing is
/// reachable that way — measured the same day, in a throwaway netns, the kernel
/// accepts the engine's own DNAT for it:
///
/// ```text
/// sctp dport 5070 counter packets 0 bytes 0 dnat to 10.0.0.5:5070
/// ```
///
/// So this refusal is about what is BUILT on this node's publish paths, and the
/// wording says so — a message that reads «impossible» would teach the next
/// reader to stop looking. ADR-0074 carries the implementation path.
/// * The spec reached the container anyway (`o.ports = sb.port_mappings`), where
///   `parse_publish_addr` refused it — so the pod died at `StartContainer`, AFTER
///   the sandbox existed, with an error naming the SPEC (`invalid protocol in
///   '8080:80/sctp'`) and not the cause, retried by the kubelet forever on a
///   condition that can never clear.
///
/// Refusing here, from `RunPodSandbox`, is the engine validating its own
/// contract instead of trusting a caller to not ask for what it cannot do. A
/// `failed_precondition` puts the pod in `Pending` with the reason as an event,
/// which is where an operator looks; a silent drop would leave the pod `Running`
/// with a port that never answers, which is the failure class this engine has
/// already had to remove three times (`--network-alias`, `--security-opt
/// seccomp=`, `-v …:z`).
/// The chain is asked to publish host ports — does it declare a plugin that can?
///
/// A chain without `capabilities.portMappings` cannot publish a `hostPort`, and
/// attaching anyway is the silence this closes: until now the CNI path published
/// NOTHING, for any transport, and the pod came up `Running` with a port that
/// never answered. Refused by name, with the fix, instead of a pod that looks
/// healthy and is not reachable.
fn refuse_chain_without_port_mappings(
    conf: &delonix_sdn::cni::NetConfList,
    asked: &[delonix_sdn::cni::PortMapping],
    pod: &str,
) -> Result<(), Status> {
    if asked.is_empty() || delonix_sdn::cni::publishes_host_ports(conf) {
        return Ok(());
    }
    let ports: Vec<String> = asked
        .iter()
        .map(|m| format!("{}/{}", m.host_port, m.protocol))
        .collect();
    Err(Status::failed_precondition(format!(
        "cannot publish hostPort(s) {} of {pod}: the node's CNI chain `{}` declares \
         no plugin with the `portMappings` capability — add `portmap` to the \
         conflist (`{{\"type\": \"portmap\", \"capabilities\": {{\"portMappings\": true}}}}` \
         as its last plugin), drop the hostPort and reach the pod through a \
         Service, or run the pod with hostNetwork",
        ports.join(", "),
        conf.name
    )))
}

/// The transport name a CRI mapping asks for, as the plugins and the engine spell
/// it. A number outside the CRI's own enum is NAMED rather than guessed — it used
/// to become `"tcp"`, a guess about the one field whose whole job is to say what
/// the traffic is.
fn cri_proto_name(protocol: i32) -> String {
    match Protocol::try_from(protocol) {
        Ok(Protocol::Tcp) => "tcp".to_string(),
        Ok(Protocol::Udp) => "udp".to_string(),
        Ok(Protocol::Sctp) => "sctp".to_string(),
        Err(_) => format!("protocol {protocol}"),
    }
}

/// What the kubelet ASKED for, unjudged, as the CNI capability argument.
///
/// Judging belongs to the caller because it depends on the PATH, and the paths
/// genuinely differ — see [`publishable_port_specs`] for the slirp one and
/// `run_pod_sandbox` for the CNI one. A mapping with no `host_port` is dropped
/// here, as it always was: the kubelet sends `container_port` alone for ports
/// that are merely declared.
///
/// `host_ip` is CARRIED here, unlike on the slirp path where the engine decides
/// the bind address in one place. Dropping it would publish on every interface a
/// port the pod asked to keep on one — widening exposure in silence, which is the
/// publish bug this engine already paid for from the other end.
fn cri_port_mappings(mappings: &[PortMapping]) -> Vec<delonix_sdn::cni::PortMapping> {
    mappings
        .iter()
        .filter(|m| m.host_port > 0 && m.container_port > 0)
        .map(|m| delonix_sdn::cni::PortMapping {
            host_port: m.host_port as u16,
            container_port: m.container_port as u16,
            protocol: cri_proto_name(m.protocol),
            host_ip: Some(m.host_ip.clone()).filter(|a| !a.is_empty()),
        })
        .collect()
}

fn publishable_port_specs(mappings: &[PortMapping]) -> Result<Vec<String>, Status> {
    let mut specs = Vec::new();
    for m in mappings {
        if m.host_port <= 0 || m.container_port <= 0 {
            continue;
        }
        let name = cri_proto_name(m.protocol);
        let proto = delonix_sdn::Proto::parse(&name).map_err(|_| {
            Status::failed_precondition(format!(
                "cannot publish hostPort {}: this node does not implement {} \
                 publishing (the rootless datapath's `add_hostfwd` takes tcp or \
                 udp, and the CNI `portmap` plugin the same) — drop the hostPort \
                 and reach the pod through a Service, or run the pod with \
                 hostNetwork, where it binds the node's ports itself",
                m.host_port, name
            ))
        })?;
        let proto = match proto {
            delonix_sdn::Proto::Tcp => "tcp",
            delonix_sdn::Proto::Udp => "udp",
        };
        // `host_ip` is deliberately left out of the spec: the engine's
        // publish already concentrates the bind address decision in one
        // place (spec > `DELONIX_PUBLISH_ADDR` > loopback), and a CRI
        // mapping that names an address the node does not have would fail
        // at bind time with a confusing error.
        specs.push(format!("{}:{}/{proto}", m.host_port, m.container_port));
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pod's limits reach `container run` — they used to be read by nobody.
    #[test]
    fn pod_limits_become_run_flags() {
        let r = CriResources {
            memory_limit_in_bytes: 50_331_648,
            cpu_quota: 50_000,
            cpu_period: 100_000,
            cpu_shares: 512,
            cpuset_cpus: "0-1".into(),
            // A Guaranteed-QoS pod's real value (kubelet's own formula),
            // and the field this test originally shipped without — ADR
            // 0038 item 3, fixed in the CaaS capability audit's gap #4:
            // this used to be read off the wire into `CriResources` and
            // never referenced again anywhere.
            oom_score_adj: -998,
            cpuset_mems: "0-1".into(),
            hugepage_limits: vec![("2MB".into(), 1_048_576)],
            unified: vec![("memory.high".into(), "100000000".into())],
        };
        let mut o = delonix_compute::RunOpts::default();
        apply_resources(&r, &mut o);
        assert_eq!(o.memory.as_deref(), Some("50331648"));
        assert_eq!(o.cpus.as_deref(), Some("0.500"));
        assert_eq!(o.cpu_weight.as_deref(), Some("20"));
        assert_eq!(o.cpuset.as_deref(), Some("0-1"));
        assert_eq!(o.oom_score_adj, Some(-998));
        assert_eq!(o.cpuset_mems.as_deref(), Some("0-1"));
        assert_eq!(o.hugepage_limits, vec![("2MB".to_string(), 1_048_576)]);
        assert_eq!(
            o.unified,
            vec![("memory.high".to_string(), "100000000".to_string())]
        );
        let rec = ContainerRec {
            resources: r,
            ..Default::default()
        };
        let o = start_run_opts(&rec, None, crate::CapCeiling::default(), "abc", Vec::new());
        assert_eq!(
            o.memory.as_deref(),
            Some("50331648"),
            "the run spec lost the memory limit"
        );
    }

    /// Nothing specified: no flag, the engine default applies as before.
    #[test]
    fn unspecified_resources_add_no_flags() {
        let mut o = delonix_compute::RunOpts::default();
        apply_resources(&CriResources::default(), &mut o);
        assert!(
            o.memory.is_none() && o.cpus.is_none() && o.cpu_weight.is_none() && o.cpuset.is_none()
        );
        // A quota without a period is not a limit.
        let half = CriResources {
            cpu_quota: 50_000,
            ..Default::default()
        };
        apply_resources(&half, &mut o);
        assert!(o.cpus.is_none());
    }

    /// The kubelet's zero cap for an unrequested page size is dropped only
    /// when the pool is empty; a real cap, a zero over a populated pool, and a
    /// pool that cannot be read are all kept for honour-or-refuse.
    #[test]
    fn a_zero_hugepage_cap_over_an_empty_pool_is_not_a_request() {
        let pool = |size: &str| match size {
            "2MB" => Some(0),
            "1GB" => Some(4),
            _ => None,
        };
        let asked = vec![
            ("2MB".to_string(), 0),
            ("1GB".to_string(), 0),
            ("64KB".to_string(), 0),
            ("2MB".to_string(), 1 << 21),
        ];
        assert_eq!(
            drop_vacuous_zero_hugepage_limits(asked, pool),
            vec![
                ("1GB".to_string(), 0),
                ("64KB".to_string(), 0),
                ("2MB".to_string(), 1 << 21),
            ]
        );
    }

    /// What every pod without hugepages carries on a node with 2MB and 1GB
    /// sizes and empty pools: nothing left to ask a controller for.
    #[test]
    fn a_pod_without_hugepages_on_an_empty_pool_wants_no_hugetlb() {
        let r = CriResources {
            hugepage_limits: drop_vacuous_zero_hugepage_limits(
                vec![("2MB".to_string(), 0), ("1GB".to_string(), 0)],
                |_| Some(0),
            ),
            ..Default::default()
        };
        assert!(wanted_resource_controllers(&r).is_empty());
    }

    #[test]
    fn hugepage_sizes_parse_to_kib() {
        assert_eq!(hugepage_size_kb("2MB"), Some(2048));
        assert_eq!(hugepage_size_kb("1GB"), Some(1_048_576));
        assert_eq!(hugepage_size_kb("64KB"), Some(64));
        assert_eq!(hugepage_size_kb("2M"), None);
        assert_eq!(hugepage_size_kb("MB"), None);
    }

    /// ADR 0038 item 4: an `UpdateContainerResources` is PARTIAL — touching
    /// one field must leave every other one exactly as the container was
    /// created with. A wholesale replace would erase them the moment any
    /// single field is updated.
    #[test]
    fn merge_update_only_overwrites_the_fields_the_request_gave() {
        let created = CriResources {
            memory_limit_in_bytes: 536_870_912,
            cpu_quota: 50_000,
            cpu_period: 100_000,
            cpuset_cpus: "0-1".into(),
            ..Default::default()
        };
        let only_memory = CriResources {
            memory_limit_in_bytes: 268_435_456,
            ..Default::default()
        };
        let merged = created.merge_update(&only_memory);
        assert_eq!(
            merged.memory_limit_in_bytes, 268_435_456,
            "the field asked for"
        );
        assert_eq!(merged.cpu_quota, 50_000, "untouched");
        assert_eq!(merged.cpu_period, 100_000, "untouched");
        assert_eq!(merged.cpuset_cpus, "0-1", "untouched");
    }

    /// `cpu_quota`/`cpu_period` merge as a PAIR: an update that only touches
    /// one of the two replaces BOTH with the update's own values, never a
    /// mix of the new one and the old one — that mix would silently change
    /// the core count nobody asked to change.
    #[test]
    fn merge_update_keeps_cpu_quota_and_period_paired() {
        let created = CriResources {
            cpu_quota: 50_000,
            cpu_period: 100_000,
            ..Default::default()
        };
        let only_period = CriResources {
            cpu_period: 50_000,
            ..Default::default()
        };
        let merged = created.merge_update(&only_period);
        assert_eq!(
            merged.cpu_quota, 0,
            "the pair moves together, not independently"
        );
        assert_eq!(merged.cpu_period, 50_000);
    }

    /// ADR 0038 item 4, the asymmetric half `merge_update_keeps_cpu_quota_and_period_
    /// paired` does not cover: a partial update that sends only `cpu_quota` (a bare
    /// `crictl update --cpu-quota N`, no `--cpu-period`) merges to a pair whose PERIOD
    /// is 0 — the field the caller did not send. Dividing that pair naively
    /// (`quota / period.max(1)`) computes `quota` as a core count, which silently
    /// writes an absurd, effectively unenforced CPU limit to the live cgroup instead
    /// of refusing the ill-formed partial request. Both halves of the pair must be
    /// refused the same way: `merge_update_keeps_cpu_quota_and_period_paired` already
    /// proves the "only period" half fails loudly (`0.000` cores is rejected by
    /// `cpu_quota_usec_or_unlimited`); this proves the "only quota" half does too,
    /// instead of silently accepting `50000.000` cores.
    #[test]
    fn cpus_for_update_refuses_a_quota_without_a_period_instead_of_computing_nonsense() {
        let created = CriResources {
            cpu_quota: 50_000,
            cpu_period: 100_000,
            ..Default::default()
        };
        let only_quota = CriResources {
            cpu_quota: 50_000,
            ..Default::default()
        };
        let merged = created.merge_update(&only_quota);
        assert_eq!(
            merged.cpu_period, 0,
            "the pair moves together, not independently"
        );
        let err = cpus_for_update(&only_quota, &merged)
            .expect_err("a quota without a period must be refused, not computed");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            !err.message().contains("50000.000") && !err.message().contains("cores"),
            "must not echo the nonsense ratio it refused to write: {}",
            err.message()
        );
    }

    /// The symmetric control: a request that gives BOTH halves of the pair computes
    /// the real ratio, never a refusal.
    #[test]
    fn cpus_for_update_computes_the_ratio_when_both_halves_are_given() {
        let created = CriResources::default();
        let both = CriResources {
            cpu_quota: 50_000,
            cpu_period: 100_000,
            ..Default::default()
        };
        let merged = created.merge_update(&both);
        assert_eq!(
            cpus_for_update(&both, &merged).unwrap(),
            Some("0.500".to_string())
        );
    }

    /// A request that touches neither field asks for no cgroup write at all.
    #[test]
    fn cpus_for_update_is_a_noop_when_neither_field_is_asked() {
        let created = CriResources {
            cpu_quota: 50_000,
            cpu_period: 100_000,
            ..Default::default()
        };
        let nothing = CriResources::default();
        let merged = created.merge_update(&nothing);
        assert_eq!(cpus_for_update(&nothing, &merged).unwrap(), None);
    }

    /// The runc/crun map: the kubelet's floor (2) and ceiling (262144) land on
    /// cpu.weight's own bounds, and the default 1024 shares lands on 39.
    #[test]
    fn shares_map_onto_cpu_weight_like_runc() {
        assert_eq!(shares_to_weight(2), 1);
        assert_eq!(shares_to_weight(1024), 39);
        assert_eq!(shares_to_weight(262_144), 10_000);
        assert_eq!(shares_to_weight(1), 1);
    }

    /// ADR 0038 item 3: a `page_size`/`unified` key that would escape the
    /// leaf directory it becomes a path component of is refused, not written.
    #[test]
    fn resource_paths_refuse_what_would_escape_the_leaf() {
        for bad in ["../x", "a/b", "/abs", "..", ""] {
            let r = CriResources {
                hugepage_limits: vec![(bad.to_string(), 1)],
                ..Default::default()
            };
            let err = validate_resource_paths(&r).expect_err(&format!("{bad:?} must be refused"));
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }
        for bad in ["../memory.high", "a/b", "/abs", "no_dot_at_all", ""] {
            let r = CriResources {
                unified: vec![(bad.to_string(), "1".to_string())],
                ..Default::default()
            };
            validate_resource_paths(&r).expect_err(&format!("{bad:?} must be refused"));
        }
        // The legitimate shapes pass through untouched.
        let ok = CriResources {
            hugepage_limits: vec![("2MB".to_string(), 1)],
            unified: vec![("memory.high".to_string(), "1".to_string())],
            ..Default::default()
        };
        assert!(validate_resource_paths(&ok).is_ok());
    }

    /// ADR 0038 item 3: which controller each field needs, and `unified`'s is
    /// the key's own prefix (cgroup v2's naming, not a lookup table).
    #[test]
    fn wanted_controllers_match_the_field_and_the_unified_prefix() {
        assert_eq!(wanted_resource_controllers(&CriResources::default()), []);
        let r = CriResources {
            cpuset_cpus: "0-1".into(),
            cpuset_mems: "0-1".into(),
            hugepage_limits: vec![("2MB".into(), 1)],
            unified: vec![
                ("memory.high".into(), "1".into()),
                ("io.weight".into(), "default 100".into()),
            ],
            ..Default::default()
        };
        assert_eq!(
            wanted_resource_controllers(&r),
            vec![
                ("cpuset".to_string(), "cpuset_cpus".to_string()),
                ("cpuset".to_string(), "cpuset_mems".to_string()),
                ("hugetlb".to_string(), "hugepage_limits".to_string()),
                ("memory".to_string(), "unified[\"memory.high\"]".to_string()),
                ("io".to_string(), "unified[\"io.weight\"]".to_string()),
            ]
        );
    }

    /// ADR 0038 item 3, the decision itself: refuses what is missing, names
    /// the controller and the field, and is silent when every controller the
    /// request needs is present — tested without a cgroup2 tree, the same
    /// split the CLI's `controller_limits_decision` uses.
    #[test]
    fn resources_decision_refuses_only_the_missing_controllers() {
        let wanted = vec![
            ("cpuset".to_string(), "cpuset_mems".to_string()),
            ("hugetlb".to_string(), "hugepage_limits".to_string()),
        ];
        assert!(
            resources_decision(&wanted, &["cpuset".to_string(), "hugetlb".to_string()]).is_ok()
        );

        let err = resources_decision(&wanted, &["cpuset".to_string()])
            .expect_err("hugetlb is missing, this must refuse");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("hugetlb"), "{}", err.message());
        assert!(
            err.message().contains("hugepage_limits"),
            "{}",
            err.message()
        );
        assert!(
            !err.message().contains("cpuset_mems"),
            "cpuset_mems was satisfied, it must not be named: {}",
            err.message()
        );

        assert!(
            resources_decision(&[], &[]).is_ok(),
            "nothing wanted, nothing to refuse"
        );
    }

    /// `refuse_unenforceable_resources` end to end: nothing requested is a
    /// no-op without touching the host at all (no `cgroup_parent`, and the
    /// probe would be `leaf_controllers()` — a real host read this test must
    /// not depend on), and the escape hatch skips the probe outright.
    #[test]
    fn refuse_unenforceable_resources_is_a_noop_with_nothing_requested() {
        assert!(refuse_unenforceable_resources(&CriResources::default(), None).is_ok());
    }

    /// `parent_cgroup_controllers` resolves against the KUBELET PARENT's own
    /// path (`KubeCgroupParent::path()`, always under `/sys/fs/cgroup`) —
    /// never the engine's own slice/leaf, a different cgroup than the one this
    /// container actually lands in (ADR 0038 item 1). Reading the real file at
    /// that path on a live host is what the battery/chaos scripts cover; this
    /// tests only the two things provable without one: the path it builds,
    /// and the fail-closed answer for a parent that was never valid (refused
    /// long before this is reached, at `RunPodSandbox`).
    #[test]
    fn parent_cgroup_controllers_resolves_under_the_cgroup2_mount() {
        let parsed = delonix_compute::KubeCgroupParent::parse("/kubepods/burstable/podabc")
            .expect("a valid cgroupfs parent");
        assert_eq!(parsed.path(), "/sys/fs/cgroup/kubepods/burstable/podabc");
        assert_eq!(
            parent_cgroup_controllers("not/a/valid/parent"),
            Vec::<String>::new()
        );
    }

    /// The kubelet's parent reaches `container run`, and only when there is one.
    #[test]
    fn the_kubelets_cgroup_parent_reaches_container_run() {
        let rec = ContainerRec::default();
        let sb = SandboxRec {
            cgroup_parent: "kubepods-burstable-podabc.slice".into(),
            ..Default::default()
        };
        let o = start_run_opts(
            &rec,
            Some(&sb),
            crate::CapCeiling::default(),
            "x",
            Vec::new(),
        );
        assert_eq!(
            o.kube_cgroup_parent.as_deref(),
            Some("kubepods-burstable-podabc.slice"),
            "the parent was not passed"
        );
        let bare = start_run_opts(
            &rec,
            Some(&SandboxRec::default()),
            crate::CapCeiling::default(),
            "x",
            Vec::new(),
        );
        assert!(bare.kube_cgroup_parent.is_none());
    }

    /// An escaping parent is refused at the sandbox, not dropped.
    #[test]
    fn an_escaping_cgroup_parent_is_refused() {
        let cfg = |p: &str| PodSandboxConfig {
            linux: Some(LinuxPodSandboxConfig {
                cgroup_parent: p.into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(cgroup_parent_of(&cfg("")).unwrap(), "");
        assert_eq!(
            cgroup_parent_of(&cfg("/kubepods/besteffort/podx")).unwrap(),
            "/kubepods/besteffort/podx"
        );
        assert!(cgroup_parent_of(&cfg("/kubepods/../..")).is_err());
        assert!(cgroup_parent_of(&cfg("delonix.slice")).is_err());
    }

    /// An OOM is `OOMKilled`, not the `Error` an external SIGKILL also gets.
    #[test]
    fn an_oom_death_is_reported_as_oomkilled() {
        assert_eq!(exit_reason(Some(137), true), "OOMKilled");
        assert_eq!(exit_reason(Some(137), false), "Error");
        assert_eq!(exit_reason(Some(0), false), "Completed");
        assert_eq!(exit_reason(None, true), "", "still running: no reason yet");
    }

    /// A `CreateContainerRequest` carrying the given capability security context.
    fn req_with_caps(add: &[&str], privileged: bool) -> CreateContainerRequest {
        CreateContainerRequest {
            pod_sandbox_id: "sb".into(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "app".into(),
                    attempt: 0,
                }),
                image: Some(ImageSpec {
                    image: "alpine:latest".into(),
                    ..Default::default()
                }),
                linux: Some(LinuxContainerConfig {
                    security_context: Some(LinuxContainerSecurityContext {
                        privileged,
                        capabilities: Some(Capability {
                            add_capabilities: add.iter().map(|s| s.to_string()).collect(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            sandbox_config: None,
        }
    }

    fn tmp_base() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// `hostNetwork: true` tem de virar REDE DO HOST no ARGV do motor.
    ///
    /// Este é o teste que faltava. Antes, o caminho `host_network` limitava-se
    /// a NÃO passar `--pod`, e o container caía na rede por omissão com um
    /// netns só dele — enquanto o `pod_sandbox_status` dizia ao kubelet que o
    /// pod estava na rede do host. Medido numa golden 1.36: os static pods do
    /// control-plane apareciam com `6443->6443`/`2381->2381` publicados, o
    /// apiserver não alcançava o etcd em `127.0.0.1:2379` (netns diferente), o
    /// kubelet matava-os e o `kubeadm init` ficava preso em
    /// `wait-control-plane`. Nada disto falha a compilar nem falha um teste
    /// unitário — só falha um cluster.
    /// `kube-proxy` is privileged and writes to `/proc/sys/net/netfilter`. The
    /// kubelet sends `readonly_paths` with `/proc/sys` in it for EVERY
    /// container; deciding which to honour is the runtime's job, and for a
    /// privileged one the answer is none — as in containerd and CRI-O.
    ///
    /// Without this: `open /proc/sys/net/netfilter/nf_conntrack_max: read-only
    /// file system`, `kube-proxy` in CrashLoopBackOff, and without it there is
    /// no ClusterIP and no CoreDNS. The cluster's whole service plane, over two
    /// arguments.
    #[test]
    fn privileged_gets_neither_masked_nor_readonly_paths() {
        let paths = || vec!["/proc/sys".to_string(), "/proc/sysrq-trigger".to_string()];

        let unprivileged = ContainerRec {
            image: "registry.k8s.io/kube-proxy:v1.36.4".into(),
            masked_paths: paths(),
            readonly_paths: paths(),
            privileged: false,
            ..Default::default()
        };
        let o = start_run_opts(
            &unprivileged,
            None,
            crate::CapCeiling::default(),
            "a",
            Vec::new(),
        );
        assert_eq!(
            o.readonly_path,
            paths(),
            "without privilege the paths MUST apply"
        );
        assert_eq!(
            o.masked_path,
            paths(),
            "without privilege the paths MUST apply"
        );

        let with_privilege = ContainerRec {
            privileged: true,
            ..unprivileged
        };
        let o = start_run_opts(
            &with_privilege,
            None,
            crate::CapCeiling::default(),
            "a",
            Vec::new(),
        );
        assert!(
            o.readonly_path.is_empty(),
            "a privileged container gets no read-only paths"
        );
        assert!(
            o.masked_path.is_empty(),
            "a privileged container gets no masked paths"
        );
        assert!(o.privileged, "`privileged: true` has to REACH the engine");
    }

    /// The CNI path CARRIES what the slirp path refuses, and that asymmetry is
    /// measured, not assumed: 2026-10-07, `portmap` from `kubernetes-cni` wrote
    /// `-p sctp -m sctp --dport 31070 -j DNAT` and a real SCTP client on the node
    /// reached a server inside the pod netns through it. A blanket refusal would
    /// have closed a door that is open in root mode — the engine is
    /// rootless-FIRST, not rootless-only.
    #[test]
    fn the_cni_path_carries_the_transports_the_slirp_path_refuses() {
        let mk = |proto: Protocol, hp: i32, cp: i32, ip: &str| PortMapping {
            protocol: proto as i32,
            container_port: cp,
            host_port: hp,
            host_ip: ip.into(),
        };
        let asked = super::cri_port_mappings(&[
            mk(Protocol::Tcp, 31080, 80, ""),
            mk(Protocol::Sctp, 31070, 5070, "127.0.0.1"),
            // A bare containerPort is still DROPPED — informational in k8s, and
            // the shape an SCTP Service uses (it goes through kube-proxy).
            mk(Protocol::Sctp, 0, 5080, ""),
        ]);
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert_eq!(asked[0].protocol, "tcp");
        assert_eq!(
            asked[0].host_ip, None,
            "an empty hostIp is absent, not \"\""
        );
        assert_eq!(asked[1].protocol, "sctp");
        // Carried, not dropped: publishing on every interface a port the pod
        // asked to keep on one widens exposure in silence.
        assert_eq!(asked[1].host_ip.as_deref(), Some("127.0.0.1"));
    }

    /// A chain asked to publish that declares no `portMappings` plugin is
    /// REFUSED with the fix, instead of attaching and staying quiet. That
    /// silence is what the CNI path did for every transport until now: the
    /// mappings were stored and then dropped, and the pod came up `Running`
    /// with a port that answered nothing.
    #[test]
    fn a_cni_chain_that_cannot_publish_is_refused_with_the_fix() {
        let conf = delonix_sdn::cni::parse_config(
            r#"{"cniVersion":"1.0.0","name":"lab","plugins":[{"type":"bridge"}]}"#,
        )
        .unwrap();
        let asked = vec![delonix_sdn::cni::PortMapping {
            host_port: 31080,
            container_port: 80,
            protocol: "tcp".into(),
            host_ip: None,
        }];
        let err = super::refuse_chain_without_port_mappings(&conf, &asked, "cri-1")
            .expect_err("a chain without the capability cannot publish");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        let m = err.message();
        assert!(m.contains("31080/tcp"), "{m}");
        assert!(m.contains("portmap"), "the fix has to be named: {m}");
        assert!(m.contains("lab"), "the chain has to be named: {m}");

        // Nothing asked → nothing refused, on the very same chain.
        assert!(super::refuse_chain_without_port_mappings(&conf, &[], "cri-1").is_ok());

        // And a chain that DOES declare it is let through.
        let ok = delonix_sdn::cni::parse_config(
            r#"{"cniVersion":"1.0.0","name":"lab","plugins":
                [{"type":"bridge"},{"type":"portmap","capabilities":{"portMappings":true}}]}"#,
        )
        .unwrap();
        assert!(super::refuse_chain_without_port_mappings(&ok, &asked, "cri-1").is_ok());
    }

    /// An SCTP `hostPort` is refused, by name, with the class the kubelet turns
    /// into a pod event. It used to be emitted as the string `"sctp"`, reach the
    /// container, and die at `StartContainer` with `invalid protocol in
    /// '8080:80/sctp'` — late, after the sandbox existed, naming the spec instead
    /// of the cause. See [`super::publishable_port_specs`] for the measurement
    /// that says the dataplane, and not the parser, is what cannot carry it.
    #[test]
    fn an_sctp_host_port_is_refused_by_name() {
        let m = PortMapping {
            protocol: Protocol::Sctp as i32,
            container_port: 80,
            host_port: 8080,
            host_ip: String::new(),
        };
        let err = super::publishable_port_specs(&[m]).expect_err("sctp cannot be published");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        let msg = err.message();
        // The port, the transport, and a way out — not a bare "unsupported".
        assert!(msg.contains("8080"), "{msg}");
        assert!(msg.contains("sctp"), "{msg}");
        assert!(
            msg.contains("Service") || msg.contains("hostNetwork"),
            "the refusal has to say what to do instead: {msg}"
        );
    }

    /// The blast radius of the refusal above, and what makes it cheap: an SCTP
    /// `containerPort` with NO `hostPort` is still DROPPED, not refused. That is
    /// correct Kubernetes semantics (it is informational) and it is the shape an
    /// SCTP *Service* uses — it goes through kube-proxy, never through a
    /// hostPort. Refusing it would break every SCTP service on the node.
    #[test]
    fn an_sctp_container_port_without_a_host_port_is_still_dropped() {
        let m = PortMapping {
            protocol: Protocol::Sctp as i32,
            container_port: 80,
            host_port: 0,
            host_ip: String::new(),
        };
        assert_eq!(
            super::publishable_port_specs(&[m])
                .expect("a bare containerPort is dropped, not refused"),
            Vec::<String>::new()
        );
    }

    /// tcp and udp keep translating exactly as they did, and the `host_ip` stays
    /// out of the spec on purpose (the bind address is decided in one place).
    #[test]
    fn tcp_and_udp_translate_unchanged() {
        let mk = |proto: Protocol, hp: i32, cp: i32| PortMapping {
            protocol: proto as i32,
            container_port: cp,
            host_port: hp,
            host_ip: "10.0.0.1".into(),
        };
        let specs = super::publishable_port_specs(&[
            mk(Protocol::Tcp, 8080, 80),
            mk(Protocol::Udp, 5353, 53),
        ])
        .expect("tcp and udp are publishable");
        assert_eq!(specs, vec!["8080:80/tcp", "5353:53/udp"]);
    }

    /// A protocol number outside the CRI's own enum used to become `"tcp"` — a
    /// guess about the one field whose whole job is to say what the traffic is.
    /// Named by number, and refused.
    #[test]
    fn an_unknown_protocol_number_is_refused_not_published_as_tcp() {
        let m = PortMapping {
            protocol: 99,
            container_port: 80,
            host_port: 8080,
            host_ip: String::new(),
        };
        let err = super::publishable_port_specs(&[m]).expect_err("99 is not a protocol");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("99"), "{}", err.message());
    }

    #[test]
    fn host_network_vira_rede_do_host_e_nao_publica_portas() {
        let rec = ContainerRec {
            image: "registry.k8s.io/etcd:3.6.6-0".into(),
            sandbox_id: "sb1".into(),
            ..Default::default()
        };
        let sb = SandboxRec {
            id: "sb1".into(),
            host_network: true,
            port_mappings: vec!["2381:2381".into()],
            ..Default::default()
        };
        let o = start_run_opts(
            &rec,
            Some(&sb),
            crate::CapCeiling::default(),
            "abc",
            Vec::new(),
        );
        assert_eq!(o.net, "host", "hostNetwork has to be `--net host`");
        assert!(
            o.pod.is_none(),
            "on the host's network the sandbox netns is not entered"
        );
        assert!(
            o.ports.is_empty(),
            "hostNetwork publishes nothing — the process binds the host's ports"
        );
    }

    /// O caminho normal (sem hostNetwork) não pode ter regredido: entra no
    /// netns do sandbox e publica as portas do pod.
    #[test]
    fn sem_host_network_entra_no_netns_do_pod_e_publica() {
        let rec = ContainerRec {
            image: "nginx".into(),
            sandbox_id: "sb2".into(),
            ..Default::default()
        };
        let sb = SandboxRec {
            id: "sb2".into(),
            host_network: false,
            port_mappings: vec!["8080:80".into()],
            ..Default::default()
        };
        let o = start_run_opts(
            &rec,
            Some(&sb),
            crate::CapCeiling::default(),
            "xyz",
            Vec::new(),
        );
        assert_eq!(
            o.pod.as_deref(),
            Some("cri-sb2"),
            "without hostNetwork the container enters the sandbox netns"
        );
        assert_eq!(o.ports, ["8080:80"], "the pod's ports are published");
    }

    /// ROOT + CNI: the container enters the sandbox netns through
    /// `start_container`'s `nsenter`, so the argv asks for the network it is in
    /// (`--net host`) and NOT `--pod` (which looks for a holder netns this path
    /// never made). Nor does it publish: `hostPort` under CNI is `portmap`'s.
    #[test]
    fn root_cni_sandbox_enters_by_netns_and_does_not_publish() {
        let rec = ContainerRec {
            image: "registry.k8s.io/coredns/coredns:v1.14.2".into(),
            sandbox_id: "sb3".into(),
            ..Default::default()
        };
        let sb = SandboxRec {
            id: "sb3".into(),
            host_network: false,
            port_mappings: vec!["53:53/udp".into()],
            cni_ip: "10.244.0.2".into(),
            cni_netns: "/run/netns/cri-sb3".into(),
            ..Default::default()
        };
        let o = start_run_opts(
            &rec,
            Some(&sb),
            crate::CapCeiling::default(),
            "c3",
            Vec::new(),
        );
        assert_eq!(o.net, "host");
        assert!(o.pod.is_none());
        assert!(o.ports.is_empty(), "a CNI sandbox leaves ports to portmap");
    }

    /// The netns sysctls of a root CNI sandbox: containerd's two defaults, the
    /// pod's `net.*` on top (they win); every other sysctl stays with the
    /// container.
    #[test]
    fn pod_netns_sysctls_containerd_defaults_and_the_pod_wins() {
        let got = pod_netns_sysctls(&[
            "net.ipv4.ip_unprivileged_port_start=80".into(),
            "kernel.shm_rmid_forced=1".into(),
            "net.core.somaxconn=1024".into(),
        ]);
        assert_eq!(
            got,
            [
                (
                    "net.ipv4.ping_group_range".to_string(),
                    "0 2147483647".to_string()
                ),
                (
                    "net.ipv4.ip_unprivileged_port_start".to_string(),
                    "80".to_string()
                ),
                ("net.core.somaxconn".to_string(), "1024".to_string()),
            ]
        );

        let rec = ContainerRec {
            image: "img".into(),
            ..Default::default()
        };
        let sb = SandboxRec {
            cni_netns: "/run/netns/cri-x".into(),
            sysctls: vec![
                "net.core.somaxconn=1024".into(),
                "kernel.shm_rmid_forced=1".into(),
            ],
            ..Default::default()
        };
        let o = start_run_opts(
            &rec,
            Some(&sb),
            crate::CapCeiling::default(),
            "c",
            Vec::new(),
        );
        assert_eq!(o.sysctl, ["kernel.shm_rmid_forced=1"]);
    }

    /// The pod's variables reach the engine by file, byte-exact (multi-line and
    /// spaces included) and off the argv, which a `run -d` keeps visible in `ps`.
    #[test]
    fn pod_envs_go_by_file_never_by_argv() {
        let envs = vec![
            KeyValue {
                key: "KUBERNETES_SERVICE_HOST".into(),
                value: "10.96.0.1".into(),
            },
            KeyValue {
                key: "CERT".into(),
                value: "-----BEGIN-----\n  line\n-----END-----".into(),
            },
        ];
        assert_eq!(
            env0_bytes(&envs),
            b"KUBERNETES_SERVICE_HOST=10.96.0.1\0CERT=-----BEGIN-----\n  line\n-----END-----\0"
        );

        let base_dir = tmp_base();
        let base = base_dir.path();
        let path = write_env_file(base, "abc", &envs).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(Path::new(&path)), 0o600);
        assert_eq!(mode(Path::new(&path).parent().unwrap()), 0o700);

        // The engine gets the environment inside the run specification — a file
        // `0600` in a `0700` directory — and the process's argv carries only its
        // path. The values reach it by data, never by argv.
        let bytes = std::fs::read(&path).unwrap();
        let env = delonix_compute::run::parse_env0(&bytes, &path).unwrap();
        let rec = ContainerRec {
            image: "img".into(),
            env_file0: path.clone(),
            ..Default::default()
        };
        let o = start_run_opts(&rec, None, crate::CapCeiling::default(), "abc", env);
        assert!(o
            .env
            .iter()
            .any(|e| e == "KUBERNETES_SERVICE_HOST=10.96.0.1"));
        let spec = write_run_spec(base, &o).unwrap();
        assert_eq!(mode(&spec), 0o600);
        assert_eq!(mode(spec.parent().unwrap()), 0o700);
        let spec_arg = spec.to_string_lossy().into_owned();
        let argv = ["__apirun", spec_arg.as_str()];
        assert!(!argv.iter().any(|a| a.contains("10.96.0.1")), "{argv:?}");
    }

    /// The ceiling has to bite at CREATE, on the real CRI request path — not just
    /// in the pure helper. Covers the two shapes a pod uses to ask for privilege
    /// (`capabilities.add` and `privileged: true`), plus the cases that must still
    /// go through untouched.
    #[test]
    fn create_container_recusa_o_que_o_tecto_do_no_proibe() {
        let base_dir = tmp_base();
        let base = base_dir.path();
        let ceiling = crate::CapCeiling::parse("default,NET_ADMIN", "reject").unwrap();

        let err = create_container(base, req_with_caps(&["SYS_ADMIN"], false), ceiling)
            .expect_err("SYS_ADMIN está acima do tecto");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(
            err.message().contains("SYS_ADMIN"),
            "o erro tem de nomear a capability negada: {}",
            err.message()
        );

        let err = create_container(base, req_with_caps(&[], true), ceiling)
            .expect_err("privileged pede tudo, o tecto não dá tudo");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        // Within the ceiling → created normally.
        create_container(base, req_with_caps(&["NET_ADMIN"], false), ceiling)
            .expect("NET_ADMIN está no tecto");
        // No ceiling → a privileged pod is created exactly as before.
        create_container(
            base,
            req_with_caps(&[], true),
            crate::CapCeiling::unlimited(),
        )
        .expect("sem tecto nada muda");
        // `clamp` mode never refuses; the reduction happens at start.
        let clamp = crate::CapCeiling::parse("default", "clamp").unwrap();
        create_container(base, req_with_caps(&["SYS_ADMIN"], true), clamp)
            .expect("clamp corta, não recusa");
    }

    /// Counts of `delonix_security_runtime::event::unconstrained::NO_CAP_CEILING`
    /// events on `base`'s log, since `rule` doubles as the generic `Event::action`.
    fn no_cap_ceiling_events(base: &std::path::Path) -> usize {
        delonix_node::events::read(base)
            .iter()
            .filter(|e| e.kind == "security" && e.action == "ADM-NO-CAP-CEILING")
            .count()
    }

    /// The gap this closes: a node with no ceiling configured refuses
    /// nothing (unchanged) — but until now it also *recorded* nothing. The
    /// event is for ELEVATED requests only: an ordinary pod with no
    /// privilege and no added capability is not the posture gap #5 names,
    /// and logging every ordinary pod would drown the signal.
    #[test]
    fn unconstrained_is_recorded_only_for_elevated_requests_with_no_ceiling() {
        let base_dir = tmp_base();
        let base = base_dir.path();
        let unlimited = crate::CapCeiling::unlimited();

        // An ordinary, unprivileged pod with no added capability: no ceiling
        // to speak of, and nothing elevated either — zero events.
        create_container(base, req_with_caps(&[], false), unlimited)
            .expect("an ordinary pod is admitted as before");
        assert_eq!(
            no_cap_ceiling_events(base),
            0,
            "an ordinary pod must never produce this signal"
        );

        // `privileged: true` with no ceiling at all: this is the gap.
        create_container(base, req_with_caps(&[], true), unlimited)
            .expect("still admitted — this gap never refuses, only records");
        assert_eq!(no_cap_ceiling_events(base), 1);

        // An explicit `capabilities.add` with no ceiling is the same gap.
        create_container(base, req_with_caps(&["NET_ADMIN"], false), unlimited)
            .expect("still admitted");
        assert_eq!(no_cap_ceiling_events(base), 2);

        // With a REAL ceiling configured, even an elevated request records
        // nothing here — the node has an answer, whether it allows or
        // refuses this particular one.
        let real = crate::CapCeiling::parse("default,NET_ADMIN", "reject").unwrap();
        create_container(base, req_with_caps(&["NET_ADMIN"], false), real)
            .expect("NET_ADMIN is within the real ceiling");
        assert_eq!(
            no_cap_ceiling_events(base),
            2,
            "a configured ceiling is not this gap, even for an elevated request"
        );
    }

    /// The flags `start_container` actually puts on the `delonix run` command line.
    /// Without a ceiling they must be BYTE-FOR-BYTE the historical ones — the whole
    /// premise of shipping this is that a node that configures nothing sees no
    /// change at all.
    #[test]
    fn cap_flags_sem_tecto_sao_exactamente_os_de_sempre() {
        let unlimited = crate::CapCeiling::unlimited();

        let plain = ContainerRec::default();
        assert!(cap_flags(&plain, unlimited, "x").is_empty());

        let privileged = ContainerRec {
            privileged: true,
            ..Default::default()
        };
        assert_eq!(cap_flags(&privileged, unlimited, "x"), ["--cap-add", "ALL"]);

        let asks = ContainerRec {
            cap_add: vec!["CAP_NET_ADMIN".into()],
            cap_drop: vec!["CAP_CHOWN".into()],
            ..Default::default()
        };
        assert_eq!(
            cap_flags(&asks, unlimited, "x"),
            ["--cap-add", "NET_ADMIN", "--cap-drop", "CHOWN"],
            "o prefixo CAP_ é retirado, tal como sempre foi"
        );
    }

    /// ...and with a ceiling, a privileged container's `--cap-add ALL` is REPLACED
    /// by the bounded set. If this ever emitted `ALL` alongside the clamp, the
    /// engine's `cap_add ALL` branch would hand back every capability and the
    /// ceiling would be decorative.
    #[test]
    fn cap_flags_com_tecto_substituem_o_cap_add_all_do_privileged() {
        let ceiling = crate::CapCeiling::parse("CHOWN,NET_ADMIN", "clamp").unwrap();
        let privileged = ContainerRec {
            privileged: true,
            ..Default::default()
        };
        let flags = cap_flags(&privileged, ceiling, "x");
        assert_eq!(
            flags,
            [
                "--cap-drop",
                "ALL",
                "--cap-add",
                "CHOWN",
                "--cap-add",
                "NET_ADMIN"
            ]
        );
        assert!(
            !flags
                .chunks(2)
                .any(|p| p[0] == "--cap-add" && p[1] == "ALL"),
            "um `--cap-add ALL` sobrevivente anularia o tecto por inteiro"
        );
    }

    /// `ceiling_reduces` decides whether a clamp is worth a `warn` line: yes when
    /// the pod asked for something it did not get, no when only the engine's
    /// implicit default was lowered (which happens on EVERY container start on a
    /// node with a ceiling, and would drown the log).
    #[test]
    fn ceiling_reduces_so_avisa_quando_um_pedido_explicito_foi_cortado() {
        let capped = vec![
            "--cap-drop".to_string(),
            "ALL".to_string(),
            "--cap-add".to_string(),
            "CHOWN".to_string(),
        ];
        let baseline_only = ContainerRec::default();
        assert!(!ceiling_reduces(&capped, &baseline_only));

        let asked_and_got = ContainerRec {
            cap_add: vec!["CAP_CHOWN".to_string()],
            ..Default::default()
        };
        assert!(
            !ceiling_reduces(&capped, &asked_and_got),
            "pediu CHOWN e recebeu CHOWN (o prefixo CAP_ não conta como diferença)"
        );

        let asked_and_lost = ContainerRec {
            cap_add: vec!["NET_ADMIN".to_string()],
            ..Default::default()
        };
        assert!(ceiling_reduces(&capped, &asked_and_lost));

        let privileged = ContainerRec {
            privileged: true,
            ..Default::default()
        };
        assert!(ceiling_reduces(&capped, &privileged));
    }

    /// The kubelet `stat`s the writable layer's mountpoint to find its device. The
    /// engine keys the container directory by its OWN id, not by the `cri-<id>`
    /// name — announcing the name gave a path that never existed, and the eviction
    /// manager ran without stats.
    #[test]
    fn writable_layer_points_at_the_engine_directory_and_measures_it() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp = tmp_dir.path();
        let store = delonix_state::Store::open(tmp.join("containers")).unwrap();
        let c = delonix_compute::Container::new(
            "e1e1e1e1e1e1e1e1".into(),
            "cri-abc".into(),
            "img:1".into(),
            vec![],
            String::new(),
        );
        store.save(&c).unwrap();
        let upper = tmp
            .join("containers")
            .join("e1e1e1e1e1e1e1e1")
            .join("upper");
        std::fs::create_dir_all(upper.join("var")).unwrap();
        std::fs::write(upper.join("var").join("data"), vec![7u8; 64 * 1024]).unwrap();

        let fs = writable_layer_usage(tmp, "abc", 1);
        let mp = fs.fs_id.unwrap().mountpoint;
        assert_eq!(mp, upper.to_string_lossy());
        assert!(
            std::path::Path::new(&mp).is_dir(),
            "the kubelet must be able to stat it"
        );
        assert!(fs.used_bytes.unwrap().value >= 64 * 1024);
        assert_eq!(fs.inodes_used.unwrap().value, 3, "upper, var, data");

        // Unknown to the engine: a path that exists, and no invented usage.
        let gone = writable_layer_usage(tmp, "nope", 1);
        assert_eq!(gone.fs_id.unwrap().mountpoint, tmp.to_string_lossy());
        assert_eq!(gone.used_bytes.unwrap().value, 0);
    }

    /// Removal is judged by the engine's store, three ways: present, absent, and
    /// unknown. Unknown must never read as absent — that is how a record got
    /// deleted with its container still on disk.
    #[test]
    fn engine_has_answers_from_the_store_and_unreadable_is_not_absent() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp = tmp_dir.path();
        let store = delonix_state::Store::open(tmp.join("containers")).unwrap();
        let c = delonix_compute::Container::new(
            "a9c6bb47c90e87bf".into(),
            "cri-bfc39487ccd4adf6".into(),
            "img:1".into(),
            vec![],
            String::new(),
        );
        store.save(&c).unwrap();
        assert_eq!(engine_has(tmp, "bfc39487ccd4adf6"), Some(true));
        assert_eq!(engine_has(tmp, "0000000000000000"), Some(false));

        // A store that cannot be read (here: `containers` is a file) is unknown.
        let broken = tmp.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("containers"), b"not a directory").unwrap();
        assert_eq!(engine_has(&broken, "bfc39487ccd4adf6"), None);
    }

    #[test]
    fn crashed_container_reporta_137_nao_0() {
        // Container marked `Running` in the store but with a DEAD pid — simulates a
        // not-yet-reconciled crash. Without the fix, delonix_exit returned None → the
        // kubelet saw exit 0 (Completed) and restartPolicy OnFailure did NOT restart.
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp = tmp_dir.path();
        let store = delonix_state::Store::open(tmp.join("containers")).unwrap();
        let mut c = delonix_compute::Container::new(
            "cri-abc".into(),
            "cri-abc".into(),
            "img:1".into(),
            vec![],
            String::new(),
        );
        c.status = delonix_model::records::Status::Running;
        c.pid = Some(2_000_000); // nonexistent pid → dead
        store.save(&c).unwrap();

        // reconciles (Running+dead → Crashed) → exit 137 + state Exited.
        assert_eq!(
            delonix_exit(tmp, "abc"),
            Some(137),
            "crash deve reportar 137, não 0"
        );
        assert_eq!(
            delonix_state(tmp, "abc"),
            ContainerState::ContainerExited as i32
        );

        // A cleanly stopped container → 0 (Completed). A Failed(n) → n.
        let mut ok = c.clone();
        ok.status = delonix_model::records::Status::Stopped;
        store.save(&ok).unwrap();
        assert_eq!(delonix_exit(tmp, "abc"), Some(0));
        let mut failed = c.clone();
        failed.status = delonix_model::records::Status::Failed(2);
        store.save(&failed).unwrap();
        assert_eq!(delonix_exit(tmp, "abc"), Some(2));
    }

    /// BUG FIXED: `ContainerStatus.finished_at` used to be `now_ns()` recomputed on
    /// EVERY poll (never stable), and `started_at` was fabricated as `created_at`
    /// instead of the real start time. Both are now persisted once and stay stable
    /// across repeated polls.
    #[test]
    fn container_status_finished_at_e_started_at_sao_estaveis_entre_polls() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp = tmp_dir.path();

        let store = delonix_state::Store::open(tmp.join("containers")).unwrap();
        let mut c = delonix_compute::Container::new(
            "cri-abc".into(),
            "cri-abc".into(),
            "img:1".into(),
            vec![],
            String::new(),
        );
        c.status = delonix_model::records::Status::Stopped;
        store.save(&c).unwrap();

        let real_started_at = 111_222_333;
        let rec = ContainerRec {
            id: "abc".into(),
            created_at: 1,
            started: true,
            started_at: real_started_at,
            finished_at: 0,
            ..Default::default()
        };
        write_rec(&ct_dir(tmp), "abc", &rec).unwrap();

        let s1 = container_status(tmp, "abc".into(), false)
            .unwrap()
            .into_inner()
            .status
            .unwrap();
        assert_eq!(
            s1.started_at, real_started_at,
            "started_at deve ser o valor persistido, não created_at"
        );
        assert_ne!(
            s1.finished_at, 0,
            "um container Stopped tem de ter finished_at"
        );

        std::thread::sleep(std::time::Duration::from_millis(5));
        let s2 = container_status(tmp, "abc".into(), false)
            .unwrap()
            .into_inner()
            .status
            .unwrap();
        assert_eq!(
            s1.finished_at, s2.finished_at,
            "finished_at não pode mudar entre polls sucessivos"
        );
        assert_eq!(s1.started_at, s2.started_at);
    }

    /// The other half of the ADR-0062 fix (the CLI/`inspect` half landed
    /// first): a Pod declaring `runAsNonRoot` running as root had nothing in
    /// `ContainerStatus` to show it. `info` is CRI's own sanctioned
    /// free-form debug extension, gated on `verbose` exactly like
    /// `StatusResponse.info`'s `capabilityCeiling` already is.
    #[test]
    fn container_status_reports_the_adr_0062_fallback_only_when_verbose() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp = tmp_dir.path();

        let store = delonix_state::Store::open(tmp.join("containers")).unwrap();
        let mut c = delonix_compute::Container::new(
            "cri-abc".into(),
            "cri-abc".into(),
            "img:1".into(),
            vec![],
            String::new(),
        );
        c.status = delonix_model::records::Status::Running;
        c.user_fallback_to_root = true;
        store.save(&c).unwrap();

        let rec = ContainerRec {
            id: "abc".into(),
            created_at: 1,
            ..Default::default()
        };
        write_rec(&ct_dir(tmp), "abc", &rec).unwrap();

        let quiet = container_status(tmp, "abc".into(), false)
            .unwrap()
            .into_inner();
        assert!(
            quiet.info.is_empty(),
            "a non-verbose request must get an empty info map, per the CRI contract"
        );

        let loud = container_status(tmp, "abc".into(), true)
            .unwrap()
            .into_inner();
        assert_eq!(
            loud.info.get("userFallbackToRoot").map(String::as_str),
            Some("true"),
            "a verbose request on a container that fell back to root must say so"
        );
    }

    /// The symmetric case: a container that DID get its declared `USER` has
    /// nothing to report — `info` says `"false"`, never omits the key (an
    /// absent key and an explicit `"false"` read very differently to a
    /// kubelet logging `.info` on an unexpected restart).
    #[test]
    fn container_status_reports_false_when_the_user_was_honoured() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp = tmp_dir.path();

        let store = delonix_state::Store::open(tmp.join("containers")).unwrap();
        let mut c = delonix_compute::Container::new(
            "cri-abc".into(),
            "cri-abc".into(),
            "img:1".into(),
            vec![],
            String::new(),
        );
        c.status = delonix_model::records::Status::Running;
        store.save(&c).unwrap();

        let rec = ContainerRec {
            id: "abc".into(),
            created_at: 1,
            ..Default::default()
        };
        write_rec(&ct_dir(tmp), "abc", &rec).unwrap();

        let loud = container_status(tmp, "abc".into(), true)
            .unwrap()
            .into_inner();
        assert_eq!(
            loud.info.get("userFallbackToRoot").map(String::as_str),
            Some("false")
        );
    }

    /// A base with two sandboxes and three containers: two in `sbaaaa`, one in `sbbbbb`.
    fn base_with_two_pods() -> tempfile::TempDir {
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp = tmp_dir.path();
        for (id, name) in [("sbaaaa", "etcd"), ("sbbbbb", "kube-apiserver")] {
            write_rec(
                &sb_dir(tmp),
                id,
                &SandboxRec {
                    id: id.into(),
                    name: name.into(),
                    namespace: "kube-system".into(),
                    labels: HashMap::from([(
                        "io.kubernetes.pod.name".to_string(),
                        name.to_string(),
                    )]),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        for (id, sb, name) in [
            ("ctaaa1", "sbaaaa", "etcd"),
            ("ctaaa2", "sbaaaa", "etcd-sidecar"),
            ("ctbbb1", "sbbbbb", "kube-apiserver"),
        ] {
            write_rec(
                &ct_dir(tmp),
                id,
                &ContainerRec {
                    id: id.into(),
                    sandbox_id: sb.into(),
                    name: name.into(),
                    labels: HashMap::from([(
                        "io.kubernetes.container.name".to_string(),
                        name.to_string(),
                    )]),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        tmp_dir
    }

    /// The 6th wall of the DKS control-plane: `ListContainers` ignored the filter, so the
    /// kubelet asked for ONE pod's containers and got the whole node's. It then reads every
    /// container it cannot find in that pod's spec as one to kill — which is the graceful
    /// 30s teardown the static pods were getting seconds after starting.
    ///
    /// Fails with the fix reverted: unfiltered, this returns all three.
    #[test]
    fn list_containers_honra_o_filtro_de_sandbox_do_kubelet() {
        let tmp_dir = base_with_two_pods();
        let tmp = tmp_dir.path();
        let got = list_containers(
            tmp,
            Some(ContainerFilter {
                pod_sandbox_id: "sbaaaa".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .into_inner()
        .containers;
        let mut ids: Vec<_> = got.iter().map(|c| c.id.clone()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["ctaaa1", "ctaaa2"],
            "o filtro por sandbox devolveu containers de outro pod"
        );
    }

    /// No filter (and an empty filter) still means "everything" — the CRI contract. Without
    /// this, closing the bug above would have broken `crictl ps` and the kubelet's own
    /// full-node sweep, which passes no filter at all.
    #[test]
    fn sem_filtro_continua_a_listar_tudo() {
        let tmp_dir = base_with_two_pods();
        let tmp = tmp_dir.path();
        assert_eq!(
            list_containers(tmp, None)
                .unwrap()
                .into_inner()
                .containers
                .len(),
            3
        );
        assert_eq!(
            list_containers(tmp, Some(ContainerFilter::default()))
                .unwrap()
                .into_inner()
                .containers
                .len(),
            3
        );
        assert_eq!(
            list_pod_sandbox(tmp, None)
                .unwrap()
                .into_inner()
                .items
                .len(),
            2
        );
    }

    #[test]
    fn list_pod_sandbox_honra_o_id_e_o_estado() {
        let tmp_dir = base_with_two_pods();
        let tmp = tmp_dir.path();
        let got = list_pod_sandbox(
            tmp,
            Some(PodSandboxFilter {
                id: "sbbbbb".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .into_inner()
        .items;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "sbbbbb");

        // A state the records do not have must match nothing — a filter that silently
        // ignored `state` would report a terminated pod as alive to the kubelet.
        let none = list_pod_sandbox(
            tmp,
            Some(PodSandboxFilter {
                state: Some(PodSandboxStateValue {
                    state: PodSandboxState::SandboxNotready as i32,
                }),
                ..Default::default()
            }),
        )
        .unwrap()
        .into_inner()
        .items;
        assert!(none.is_empty(), "o filtro de estado não foi aplicado");
    }

    /// The kubelet compares `linux.namespaces.options.network` against what the pod asks
    /// for, on EVERY sync, and starts a new sandbox whenever they differ. Reporting `None`
    /// hands it the zero value (`POD`), so every `hostNetwork: true` pod — which is every
    /// kubeadm control-plane static pod — never matched and was rebuilt about once per
    /// second, for ever. MEASURED 2026-09-07: `crictl pods` reached ATTEMPT 401 on the etcd
    /// pod in under four minutes, and no control plane could ever finish `kubeadm init`.
    #[test]
    fn pod_sandbox_status_reports_the_namespace_modes() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp = tmp_dir.path();
        write_rec(
            &sb_dir(tmp),
            "sbhost",
            &SandboxRec {
                id: "sbhost".into(),
                name: "etcd".into(),
                namespace: "kube-system".into(),
                host_network: true,
                ..Default::default()
            },
        )
        .unwrap();

        let st = pod_sandbox_status(tmp, "sbhost".into())
            .unwrap()
            .into_inner()
            .status
            .expect("status");
        let opts = st
            .linux
            .expect("the kubelet reads `linux` — `None` is what broke the control plane")
            .namespaces
            .expect("namespaces")
            .options
            .expect("options");

        assert_eq!(
            opts.network,
            NamespaceMode::Node as i32,
            "a host-network sandbox must report NODE, or the kubelet rebuilds it every sync"
        );
        // The other two are POD here, and asserting them keeps a future edit from wiring
        // `network` alone and leaving the neighbours reporting whatever the zero value is.
        assert_eq!(opts.pid, NamespaceMode::Pod as i32);
        assert_eq!(opts.ipc, NamespaceMode::Pod as i32);
    }

    /// `label_selector` is a SUBSET match. Comparing whole maps would match nothing in
    /// practice, because a real container carries every label the kubelet set on it.
    #[test]
    fn o_label_selector_e_subconjunto_e_nao_igualdade() {
        let tmp_dir = base_with_two_pods();
        let tmp = tmp_dir.path();
        let got = list_containers(
            tmp,
            Some(ContainerFilter {
                label_selector: HashMap::from([(
                    "io.kubernetes.container.name".to_string(),
                    "etcd".to_string(),
                )]),
                ..Default::default()
            }),
        )
        .unwrap()
        .into_inner()
        .containers;
        assert_eq!(got.len(), 1, "esperava só o container 'etcd'");
        assert_eq!(got[0].id, "ctaaa1");
    }

    /// The CRI spec's own rule for `Metric.name`: a name that never appeared
    /// in a `ListMetricDescriptors` call is defined to be IGNORED by the
    /// caller. Every name this runtime ever emits from
    /// `list_pod_sandbox_metrics` has to come from the same table
    /// `list_metric_descriptors` reads — this proves the two cannot drift,
    /// rather than trusting the two literal name lists to stay in sync by hand.
    #[test]
    fn every_emitted_metric_name_has_a_descriptor() {
        let tmp_dir = tmp_base();
        let tmp = tmp_dir.path();
        write_rec(
            &sb_dir(tmp),
            "sb1",
            &SandboxRec {
                id: "sb1".into(),
                ..Default::default()
            },
        )
        .unwrap();
        write_rec(
            &ct_dir(tmp),
            "ct1",
            &ContainerRec {
                id: "ct1".into(),
                sandbox_id: "sb1".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let descriptor_names: std::collections::HashSet<String> = list_metric_descriptors()
            .unwrap()
            .into_inner()
            .descriptors
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(
            descriptor_names.len(),
            POD_METRICS.len() + CONTAINER_METRICS.len(),
            "descriptor names must be unique and cover both tables"
        );

        let pods = list_pod_sandbox_metrics(tmp)
            .unwrap()
            .into_inner()
            .pod_metrics;
        assert_eq!(pods.len(), 1);
        let pod = &pods[0];
        assert_eq!(pod.pod_sandbox_id, "sb1");
        for m in &pod.metrics {
            assert!(
                descriptor_names.contains(&m.name),
                "pod metric '{}' has no matching descriptor — the caller must ignore it",
                m.name
            );
        }
        assert_eq!(pod.container_metrics.len(), 1);
        let cm = &pod.container_metrics[0];
        assert_eq!(cm.container_id, "ct1");
        for m in &cm.metrics {
            assert!(
                descriptor_names.contains(&m.name),
                "container metric '{}' has no matching descriptor — the caller must ignore it",
                m.name
            );
        }
    }

    /// A container belonging to a DIFFERENT sandbox must never show up under
    /// this one's `container_metrics` — the same isolation
    /// `pod_sandbox_stats_for` already gives the older Stats API.
    #[test]
    fn container_metrics_are_scoped_to_their_own_sandbox() {
        let tmp_dir = tmp_base();
        let tmp = tmp_dir.path();
        for (sb_id, ct_id) in [("sbA", "ctA"), ("sbB", "ctB")] {
            write_rec(
                &sb_dir(tmp),
                sb_id,
                &SandboxRec {
                    id: sb_id.into(),
                    ..Default::default()
                },
            )
            .unwrap();
            write_rec(
                &ct_dir(tmp),
                ct_id,
                &ContainerRec {
                    id: ct_id.into(),
                    sandbox_id: sb_id.into(),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let pods = list_pod_sandbox_metrics(tmp)
            .unwrap()
            .into_inner()
            .pod_metrics;
        assert_eq!(pods.len(), 2);
        for pod in &pods {
            assert_eq!(
                pod.container_metrics.len(),
                1,
                "sandbox {} must see only its own container",
                pod.pod_sandbox_id
            );
            let expected_ct = if pod.pod_sandbox_id == "sbA" {
                "ctA"
            } else {
                "ctB"
            };
            assert_eq!(pod.container_metrics[0].container_id, expected_ct);
        }
    }
}
