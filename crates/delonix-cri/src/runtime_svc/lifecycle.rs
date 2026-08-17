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
    /// configured by CNI plugins (rootless, via holder). Empty = native SDN.
    #[serde(default)]
    cni_ip: String,
}

fn sandbox_state(r: &SandboxRec) -> i32 {
    if r.stopped {
        PodSandboxState::SandboxNotready as i32
    } else {
        PodSandboxState::SandboxReady as i32
    }
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

/// `true` if the stderr of a `delonix container rm/stop` indicates the target
/// **does not exist** — the CRI contract requires `RemoveContainer`/`StopContainer`
/// to be IDEMPOTENT (a missing container counts as already removed/stopped). The
/// canonical `delonix` message is "container não encontrado"; we also cover the
/// docker/english variants for robustness.
fn stderr_not_found(stderr: &[u8]) -> bool {
    let e = String::from_utf8_lossy(stderr).to_lowercase();
    e.contains("não encontrado")
        || e.contains("nao encontrado")
        || e.contains("not found")
        || e.contains("no such")
        || e.contains("não existe")
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
    let data = std::fs::read(dir.join(format!("{id}.json")))
        .map_err(|_| Status::not_found(format!("{id} not found")))?;
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
    crate::cli_bin()
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

/// Like [`delonix`], but with stdio to `/dev/null` — MANDATORY for `run -d`: the
/// daemonized container inherits and HOLDS the stdout/stderr *pipes*; with
/// `.output()` the `wait` would block until the container exits (the "run -d |
/// tail hangs" bug).
fn delonix_detached(base: &Path, args: &[&str]) -> Result<bool, Status> {
    Ok(delonix_detached_why(base, args)?.is_none())
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
fn delonix_detached_why(base: &Path, args: &[&str]) -> Result<Option<String>, Status> {
    use std::process::Stdio;
    let out = Command::new(delonix_bin())
        .env("DELONIX_ROOT", base)
        .env("DELONIX_INTERNAL", "1")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(st)?;
    if out.status.success() {
        return Ok(None);
    }
    let why = String::from_utf8_lossy(&out.stderr);
    let why = why.trim();
    Ok(Some(if why.is_empty() {
        format!("`delonix {}` exited {}", args.join(" "), out.status)
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
fn load_reconciled(base: &Path, cri_id: &str) -> Option<delonix_runtime_core::Container> {
    let store = delonix_runtime_core::Store::open(base.join("containers")).ok()?;
    // `update` (flock + re-reads under the lock), NOT `load`+`save`: this server
    // is CONCURRENT (the kubelet issues requests in parallel, each in a
    // `spawn_blocking`) and the CLI touches the same state. With the naive
    // pattern, two simultaneous reconciles lost writes — measured: 24 concurrent
    // updates → 1 survivor (see `store::tests::update_concorrente_nao_perde_escritas`).
    store
        .update(&format!("cri-{cri_id}"), |c| {
            delonix_runtime::reconcile_status(c)
        })
        .ok()
}

/// The runtime state of a CRI container, read (and reconciled) from the `Store`.
fn delonix_state(base: &Path, cri_id: &str) -> i32 {
    use delonix_runtime_core::Status as S;
    match load_reconciled(base, cri_id) {
        Some(c) => match c.status {
            S::Running if c.pid.map(delonix_runtime::is_alive).unwrap_or(false) => {
                ContainerState::ContainerRunning as i32
            }
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
    use delonix_runtime_core::Status as S;
    match load_reconciled(base, cri_id)?.status {
        S::Failed(code) => Some(code),
        S::Stopped => Some(0),
        S::Crashed => Some(137),
        _ => None,
    }
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
    let id = delonix_runtime_core::generate_id();
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
    // REAL Delonix pod: an infra container (`pod-cri-<id>`) holds the shared
    // netns ("pause"-style), which the sandbox's containers then join via
    // `--pod`. That is what gives pod networking and namespace sharing.
    // CNI (opt-in `DELONIX_CNI=1` + conflist): the sandbox gets its network from
    // real CNI plugins (the cluster chain, e.g. Calico), as in containerd/CRI-O.
    // Rootless → the plugins run in the holder (owner of the netns); the netns is
    // named `cri-<id>` so the sandbox's containers join via `--pod cri-<id>`
    // (join_argv). Without the flag, `enabled_conf()` is None and it follows the
    // native (SDN) path unchanged.
    let mut cni_ip = String::new();
    if !host_network {
        let pod = format!("cri-{id}");
        let cni = delonix_net::cni::enabled_conf();
        if let Some(conf) = cni.filter(|_| delonix_runtime::is_rootless()) {
            let conf_json = serde_json::to_string(&conf)
                .map_err(|e| Status::internal(format!("serializing conflist: {e}")))?;
            match delonix_net::infra::cni_attach_container(&pod, &conf_json) {
                Ok((_netns, cidr)) => {
                    cni_ip = cidr.split('/').next().unwrap_or("").to_string();
                }
                Err(e) => return Err(Status::internal(format!("CNI ADD of sandbox {pod}: {e}"))),
            }
        } else if delonix_runtime::is_rootless() {
            // ROOTLESS: the pod is a SHARED ingress netns (delonix0 + DHCP +
            // DNS + firewall); the sandbox's containers join via `--pod`.
            if let Some(why) = delonix_detached_why(base, &["net", "netns", "attach", &pod])? {
                return Err(Status::internal(format!(
                    "failed to create the ingress sandbox {pod}: {why}"
                )));
            }
        } else if let Some(why) = delonix_detached_why(base, &["pod", "create", &pod, "--network"])?
        {
            // ROOT: infra container (`pod-cri-<id>`) holds the netns ("pause"-style).
            return Err(Status::internal(format!(
                "failed to create the pod sandbox {pod}: {why}"
            )));
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
        port_mappings: cri_port_specs(&cfg.port_mappings),
        cni_ip,
    };
    write_rec(&sb_dir(base), &id, &rec)?;
    delonix_runtime_core::metrics::inc_pod_sandbox_created();
    Ok(Response::new(RunPodSandboxResponse { pod_sandbox_id: id }))
}

pub fn stop_pod_sandbox(
    base: &Path,
    id: String,
) -> Result<Response<StopPodSandboxResponse>, Status> {
    // stop the sandbox's containers and mark it NotReady.
    for c in list_recs::<ContainerRec>(&ct_dir(base)) {
        if c.sandbox_id == id {
            let _ = delonix(base, &["container", "stop", &format!("cri-{}", c.id)]);
        }
    }
    if let Ok(mut r) = read_rec::<SandboxRec>(&sb_dir(base), &id) {
        r.stopped = true;
        let _ = write_rec(&sb_dir(base), &id, &r);
    }
    Ok(Response::new(StopPodSandboxResponse {}))
}

pub fn remove_pod_sandbox(
    base: &Path,
    id: String,
) -> Result<Response<RemovePodSandboxResponse>, Status> {
    for c in list_recs::<ContainerRec>(&ct_dir(base)) {
        if c.sandbox_id == id {
            let _ = delonix(base, &["container", "rm", "-f", &format!("cri-{}", c.id)]);
            remove_rec(&ct_dir(base), &c.id);
        }
    }
    // Remove the real Delonix pod (infra container + netns), if it existed.
    if let Ok(sb) = read_rec::<SandboxRec>(&sb_dir(base), &id) {
        if !sb.host_network {
            if !sb.cni_ip.is_empty() {
                // CNI-configured sandbox (rootless): plugin DEL in the holder.
                if let Some(conf) = delonix_net::cni::enabled_conf() {
                    let cj = serde_json::to_string(&conf).unwrap_or_default();
                    let _ = delonix_net::infra::cni_detach_container(&format!("cri-{id}"), &cj);
                }
            } else if delonix_runtime::is_rootless() {
                let _ = delonix(base, &["net", "netns", "detach", &format!("cri-{id}")]);
            } else {
                let _ = delonix(base, &["pod", "rm", &format!("cri-{id}")]);
            }
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
    } else if delonix_runtime::is_rootless() {
        // ROOTLESS: IP of the pod's shared netns in the ingress (deterministic).
        delonix_net::infra::container_ip(&format!("cri-{}", r.id))
    } else {
        delonix_runtime_core::Store::open(base.join("containers"))
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
        linux: None,
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
    let id = delonix_runtime_core::generate_id();
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
        delonix_runtime::seccomp_profile::parse(&json)
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
    };
    write_rec(&ct_dir(base), &id, &rec)?;
    delonix_runtime_core::metrics::inc_container_created();
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


/// Constrói o ARGV de `delonix container run` para um container do CRI.
///
/// Extraído de [`start_container`] para ser **puro e testável**: este ARGV é a
/// fronteira onde um erro não aparece como falha de compilação nem de teste
/// unitário, só como um cluster que não arranca. Foi assim que o
/// `hostNetwork: true` passou meses a não ser rede do host — ver o comentário
/// dentro da função.
fn start_argv(
    rec: &ContainerRec,
    sandbox: Option<&SandboxRec>,
    ceiling: crate::CapCeiling,
    id: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "container".into(),
        "run".into(),
        "-d".into(),
        "--name".into(),
        format!("cri-{id}"),
    ];
    // Logs in the path/format the kubelet/crictl expect (CRI), if any.
    if !rec.log_path.is_empty() {
        args.push("--log-file".into());
        args.push(rec.log_path.clone());
        args.push("--log-cri".into());
    }
    // Joins the pod sandbox's netns (network/namespace sharing), unless the pod
    // uses the host's network.
    if let Some(sb) = sandbox {
        if sb.host_network {
            // `hostNetwork: true` tem de ser a REDE DO HOST, não "sem `--pod`".
            //
            // Omitir só o `--pod` deixava o container cair na rede por omissão
            // (bridge) com um netns SEU — e ao mesmo tempo o `pod_sandbox_status`
            // reportava ao kubelet que o pod estava na rede do host. Kubelet e
            // realidade a dizer coisas diferentes, em silêncio.
            //
            // Medido a 2026-08-16 numa golden 1.36: os quatro static pods do
            // control-plane apareciam com portas PUBLICADAS (`6443->6443`,
            // `2381->2381`), que é a assinatura de um netns próprio. O etcd
            // escutava em `127.0.0.1:2379` dentro do seu netns, o apiserver
            // procurava-o em `127.0.0.1:2379` NOUTRO netns
            // (`--etcd-servers=https://127.0.0.1:2379`), as sondas falhavam, o
            // kubelet matava-os, e o motor registava `Crashed` — morte por
            // sinal, sem código de saída. O `kubeadm init` ficava preso em
            // `wait-control-plane` e a 6443 nunca abria.
            //
            // O mesmo etcd, corrido à mão com `--net host` e as mesmas flags de
            // log do CRI, fica `Up` e serve tráfego. A diferença era esta linha.
            args.push("--net".into());
            args.push("host".into());
        } else {
            args.push("--pod".into());
            args.push(format!("cri-{}", rec.sandbox_id));
        }
        // Pod hostname (`PodSandboxConfig.hostname`) — CRI conformance checks that
        // `hostname`/`/etc/hostname` inside the container match the sandbox's.
        if !sb.hostname.is_empty() {
            args.push("--hostname".into());
            args.push(sb.hostname.clone());
        }
        // Host namespaces inherited from the pod sandbox.
        if sb.host_pid {
            args.push("--host-pid".into());
        }
        if sb.host_ipc {
            args.push("--host-ipc".into());
        }
        // The pod's resolver and published ports belong to every container of
        // the sandbox: they share the pod's netns, so this is where both live.
        for d in &sb.dns_servers {
            args.push("--dns".into());
            args.push(d.clone());
        }
        for d in &sb.dns_searches {
            args.push("--dns-search".into());
            args.push(d.clone());
        }
        for d in &sb.dns_options {
            args.push("--dns-option".into());
            args.push(d.clone());
        }
        // Na rede do HOST não há o que publicar: o processo liga-se
        // directamente aos portos do host. Publicar aqui pedia DNAT para um
        // netns que não existe — e era o sintoma pelo qual este defeito se
        // deixou ver (`6443->6443` num pod `hostNetwork`).
        if !sb.host_network {
            for p in &sb.port_mappings {
                args.push("--publish".into());
                args.push(p.clone());
            }
        }
        // pod sysctls, applied to the container (shares the pod's namespaces).
        for s in &sb.sysctls {
            args.push("--sysctl".into());
            args.push(s.clone());
        }
    }
    for v in &rec.volumes {
        args.push("--volume".into());
        args.push(v.clone());
    }
    // Security context → flags.
    if rec.readonly_rootfs {
        args.push("--read-only".into());
    }
    for g in &rec.supplemental_groups {
        args.push("--group-add".into());
        args.push(g.to_string());
    }
    for p in &rec.masked_paths {
        args.push("--masked-path".into());
        args.push(p.clone());
    }
    for p in &rec.readonly_paths {
        args.push("--readonly-path".into());
        args.push(p.clone());
    }
    args.push("--security-opt".into());
    args.push(format!("no-new-privileges={}", rec.no_new_privs));
    // `privileged` implies unconfined seccomp (the engine does the same on its
    // side), and either way an explicit profile path only applies when nothing
    // asked for unconfined.
    if rec.privileged || rec.seccomp_unconfined {
        args.push("--security-opt".into());
        args.push("seccomp=unconfined".into());
    } else if let Some(p) = &rec.seccomp_profile_path {
        args.push("--security-opt".into());
        args.push(format!("seccomp={p}"));
    }
    args.extend(cap_flags(&rec, ceiling, &id));
    if let Some(prof) = &rec.apparmor {
        args.push("--apparmor".into());
        args.push(prof.clone());
    }
    // RunAsUser/RunAsGroup/RunAsUserName → `--user <user[:group]>`. The `--user` of
    // `delonix run` resolves a NAME against the image's `/etc/passwd` (the
    // `RunAsUserName` contract) and accepts a numeric uid (`RunAsUser`); the group
    // is the numeric `RunAsGroup`. `RunAsUserName` takes precedence over `RunAsUser`
    // (the proto forbids both at the same time).
    let user_part = if !rec.run_as_username.is_empty() {
        Some(rec.run_as_username.clone())
    } else {
        rec.run_as_user.map(|u| u.to_string())
    };
    if let Some(u) = user_part {
        let spec = match rec.run_as_group {
            Some(g) => format!("{u}:{g}"),
            None => u,
        };
        args.push("--user".into());
        args.push(spec);
    }
    // `--` separates the flags from the positionals: prevents an `image`/`command`
    // coming from the CRI request and starting with `-` from being interpreted as
    // a flag (injection).
    args.push("--".into());
    args.push(rec.image.clone());
    args.extend(rec.command.iter().cloned());
    args.extend(rec.args.iter().cloned());
    args
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
    let args = start_argv(&rec, sandbox.as_ref(), ceiling, &id);
    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    if !delonix_detached(base, &argv)? {
        return Err(Status::internal(format!("failed to start container {id}")));
    }
    rec.started = true;
    rec.started_at = now_ns();
    write_rec(&ct_dir(base), &id, &rec)?;
    Ok(Response::new(StartContainerResponse {}))
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
    // CRI contract: stopping a container that no longer exists is success (idempotent).
    if !out.status.success() && stderr_not_found(&out.stderr) {
        return Ok(Response::new(StopContainerResponse {}));
    }
    // Verify it actually STOPPED (reconciled). Idempotent: already stopped/absent
    // = OK. If it is still alive, propagate an error → the kubelet retries (instead
    // of assuming it stopped and moving on to RemoveContainer on a still-running
    // process).
    if let Some(c) = load_reconciled(base, &id) {
        let alive = matches!(c.status, delonix_runtime_core::Status::Running)
            && c.pid.map(delonix_runtime::is_alive).unwrap_or(false);
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
    let out = delonix(base, &["container", "rm", "-f", &format!("cri-{id}")])?;
    let gone = out.status.success() || stderr_not_found(&out.stderr);
    if !gone {
        return Err(Status::internal(format!(
            "removal of 'cri-{id}' failed (record preserved for retry): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
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
        reason: match exit {
            Some(0) => "Completed".into(),
            Some(_) => "Error".into(),
            None => String::new(),
        },
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
    Ok(Response::new(ContainerStatusResponse {
        status: Some(status),
        info: Default::default(),
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
    let out = command
        .arg("container")
        .arg("exec")
        .arg(&name)
        .args(&cmd)
        .output()
        .map_err(st)?;
    // `timeout` returns 124 when it expires → maps to a distinct exit code.
    let exit_code = out.status.code().unwrap_or(-1);
    Ok(Response::new(ExecSyncResponse {
        stdout: out.stdout,
        stderr: out.stderr,
        exit_code,
    }))
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
    let store = delonix_runtime_core::Store::open(base.join("containers")).ok()?;
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

/// Builds a container's real metrics from its cgroup v2.
fn container_stats_for(base: &Path, r: &ContainerRec) -> ContainerStats {
    let ts = now_ns();
    let cg = container_cgroup(base, &r.id);
    let (cpu_ns, mem_cur, working_set, rss, pgfault, pgmajfault) = match &cg {
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
            (
                cpu_us.saturating_mul(1000), // µs → ns
                usage,
                working,
                rss,
                cg_field(cg, "memory.stat", "pgfault"),
                cg_field(cg, "memory.stat", "pgmajfault"),
            )
        }
        None => (0, 0, 0, 0, 0, 0),
    };
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
        writable_layer: Some(FilesystemUsage {
            timestamp: ts,
            fs_id: Some(FilesystemIdentifier {
                mountpoint: base
                    .join("containers")
                    .join(format!("cri-{}", r.id))
                    .to_string_lossy()
                    .into_owned(),
            }),
            used_bytes: u64v(0),
            inodes_used: u64v(0),
        }),
        swap: Some(SwapUsage {
            timestamp: ts,
            swap_available_bytes: u64v(0),
            swap_usage_bytes: u64v(
                cg.as_deref()
                    .map(|c| cg_u64(c, "memory.swap.current"))
                    .unwrap_or(0),
            ),
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

/// Translates the CRI's `PortMapping` list into `-p` specs.
///
/// A mapping with no `host_port` is DROPPED, not published on a random port:
/// the kubelet sends `container_port` alone for ports that are merely declared
/// (a `containerPort` with no `hostPort`), and publishing those would expose on
/// the node every port a pod ever named.
fn cri_port_specs(mappings: &[PortMapping]) -> Vec<String> {
    mappings
        .iter()
        .filter(|m| m.host_port > 0 && m.container_port > 0)
        .map(|m| {
            let proto = match Protocol::try_from(m.protocol) {
                Ok(Protocol::Udp) => "udp",
                Ok(Protocol::Sctp) => "sctp",
                _ => "tcp",
            };
            // `host_ip` is deliberately left out of the spec: the engine's
            // publish already concentrates the bind address decision in one
            // place (spec > `DELONIX_PUBLISH_ADDR` > loopback), and a CRI
            // mapping that names an address the node does not have would fail
            // at bind time with a confusing error.
            format!("{}:{}/{proto}", m.host_port, m.container_port)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn tmp_base(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "dlx-cri-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
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
    #[test]
    fn host_network_vira_rede_do_host_e_nao_publica_portas() {
        let mut rec = ContainerRec::default();
        rec.image = "registry.k8s.io/etcd:3.6.6-0".into();
        rec.sandbox_id = "sb1".into();

        let mut sb = SandboxRec::default();
        sb.id = "sb1".into();
        sb.host_network = true;
        sb.port_mappings = vec!["2381:2381".into()];

        let argv = start_argv(&rec, Some(&sb), crate::CapCeiling::default(), "abc");
        let pos = |f: &str| argv.iter().position(|a| a == f);

        // A metade que faltava: rede do host de verdade.
        let i = pos("--net").expect(&format!("`--net` ausente em {argv:?}"));
        assert_eq!(argv[i + 1], "host", "hostNetwork tem de ser `--net host`");
        assert!(pos("--pod").is_none(), "na rede do host não se entra no netns do sandbox");

        // A outra metade: publicar portas num pod hostNetwork pede DNAT para um
        // netns que não existe — e foi o sintoma que denunciou o defeito.
        assert!(
            pos("--publish").is_none(),
            "hostNetwork não publica portas — o processo liga-se aos portos do host: {argv:?}"
        );
    }

    /// O caminho normal (sem hostNetwork) não pode ter regredido: entra no
    /// netns do sandbox e publica as portas do pod.
    #[test]
    fn sem_host_network_entra_no_netns_do_pod_e_publica() {
        let mut rec = ContainerRec::default();
        rec.image = "nginx".into();
        rec.sandbox_id = "sb2".into();

        let mut sb = SandboxRec::default();
        sb.id = "sb2".into();
        sb.host_network = false;
        sb.port_mappings = vec!["8080:80".into()];

        let argv = start_argv(&rec, Some(&sb), crate::CapCeiling::default(), "xyz");
        let pos = |f: &str| argv.iter().position(|a| a == f);

        let i = pos("--pod").expect("sem hostNetwork tem de entrar no netns do sandbox");
        assert_eq!(argv[i + 1], "cri-sb2");
        assert!(pos("--net").is_none(), "só o caminho hostNetwork mexe em `--net`");
        let j = pos("--publish").expect("as portas do pod são publicadas");
        assert_eq!(argv[j + 1], "8080:80");
    }

    /// The ceiling has to bite at CREATE, on the real CRI request path — not just
    /// in the pure helper. Covers the two shapes a pod uses to ask for privilege
    /// (`capabilities.add` and `privileged: true`), plus the cases that must still
    /// go through untouched.
    #[test]
    fn create_container_recusa_o_que_o_tecto_do_no_proibe() {
        let base = tmp_base("ceiling");
        let ceiling = crate::CapCeiling::parse("default,NET_ADMIN", "reject").unwrap();

        let err = create_container(&base, req_with_caps(&["SYS_ADMIN"], false), ceiling)
            .expect_err("SYS_ADMIN está acima do tecto");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(
            err.message().contains("SYS_ADMIN"),
            "o erro tem de nomear a capability negada: {}",
            err.message()
        );

        let err = create_container(&base, req_with_caps(&[], true), ceiling)
            .expect_err("privileged pede tudo, o tecto não dá tudo");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        // Within the ceiling → created normally.
        create_container(&base, req_with_caps(&["NET_ADMIN"], false), ceiling)
            .expect("NET_ADMIN está no tecto");
        // No ceiling → a privileged pod is created exactly as before.
        create_container(
            &base,
            req_with_caps(&[], true),
            crate::CapCeiling::unlimited(),
        )
        .expect("sem tecto nada muda");
        // `clamp` mode never refuses; the reduction happens at start.
        let clamp = crate::CapCeiling::parse("default", "clamp").unwrap();
        create_container(&base, req_with_caps(&["SYS_ADMIN"], true), clamp)
            .expect("clamp corta, não recusa");

        let _ = std::fs::remove_dir_all(&base);
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

    #[test]
    fn crashed_container_reporta_137_nao_0() {
        // Container marked `Running` in the store but with a DEAD pid — simulates a
        // not-yet-reconciled crash. Without the fix, delonix_exit returned None → the
        // kubelet saw exit 0 (Completed) and restartPolicy OnFailure did NOT restart.
        let tmp = std::env::temp_dir().join(format!("dlx-cri-exit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = delonix_runtime_core::Store::open(tmp.join("containers")).unwrap();
        let mut c = delonix_runtime_core::Container::new(
            "cri-abc".into(),
            "cri-abc".into(),
            "img:1".into(),
            vec![],
            String::new(),
        );
        c.status = delonix_runtime_core::Status::Running;
        c.pid = Some(2_000_000); // nonexistent pid → dead
        store.save(&c).unwrap();

        // reconciles (Running+dead → Crashed) → exit 137 + state Exited.
        assert_eq!(
            delonix_exit(&tmp, "abc"),
            Some(137),
            "crash deve reportar 137, não 0"
        );
        assert_eq!(
            delonix_state(&tmp, "abc"),
            ContainerState::ContainerExited as i32
        );

        // A cleanly stopped container → 0 (Completed). A Failed(n) → n.
        let mut ok = c.clone();
        ok.status = delonix_runtime_core::Status::Stopped;
        store.save(&ok).unwrap();
        assert_eq!(delonix_exit(&tmp, "abc"), Some(0));
        let mut failed = c.clone();
        failed.status = delonix_runtime_core::Status::Failed(2);
        store.save(&failed).unwrap();
        assert_eq!(delonix_exit(&tmp, "abc"), Some(2));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// BUG FIXED: `ContainerStatus.finished_at` used to be `now_ns()` recomputed on
    /// EVERY poll (never stable), and `started_at` was fabricated as `created_at`
    /// instead of the real start time. Both are now persisted once and stay stable
    /// across repeated polls.
    #[test]
    fn container_status_finished_at_e_started_at_sao_estaveis_entre_polls() {
        let tmp = std::env::temp_dir().join(format!(
            "dlx-cri-status-stable-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);

        let store = delonix_runtime_core::Store::open(tmp.join("containers")).unwrap();
        let mut c = delonix_runtime_core::Container::new(
            "cri-abc".into(),
            "cri-abc".into(),
            "img:1".into(),
            vec![],
            String::new(),
        );
        c.status = delonix_runtime_core::Status::Stopped;
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
        write_rec(&ct_dir(&tmp), "abc", &rec).unwrap();

        let s1 = container_status(&tmp, "abc".into())
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
        let s2 = container_status(&tmp, "abc".into())
            .unwrap()
            .into_inner()
            .status
            .unwrap();
        assert_eq!(
            s1.finished_at, s2.finished_at,
            "finished_at não pode mudar entre polls sucessivos"
        );
        assert_eq!(s1.started_at, s2.started_at);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A base with two sandboxes and three containers: two in `sbaaaa`, one in `sbbbbb`.
    fn base_with_two_pods(tag: &str) -> PathBuf {
        let tmp = std::env::temp_dir().join(format!("dlx-cri-filter-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        for (id, name) in [("sbaaaa", "etcd"), ("sbbbbb", "kube-apiserver")] {
            write_rec(
                &sb_dir(&tmp),
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
                &ct_dir(&tmp),
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
        tmp
    }

    /// The 6th wall of the DKS control-plane: `ListContainers` ignored the filter, so the
    /// kubelet asked for ONE pod's containers and got the whole node's. It then reads every
    /// container it cannot find in that pod's spec as one to kill — which is the graceful
    /// 30s teardown the static pods were getting seconds after starting.
    ///
    /// Fails with the fix reverted: unfiltered, this returns all three.
    #[test]
    fn list_containers_honra_o_filtro_de_sandbox_do_kubelet() {
        let tmp = base_with_two_pods("ctsb");
        let got = list_containers(
            &tmp,
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
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// No filter (and an empty filter) still means "everything" — the CRI contract. Without
    /// this, closing the bug above would have broken `crictl ps` and the kubelet's own
    /// full-node sweep, which passes no filter at all.
    #[test]
    fn sem_filtro_continua_a_listar_tudo() {
        let tmp = base_with_two_pods("all");
        assert_eq!(
            list_containers(&tmp, None)
                .unwrap()
                .into_inner()
                .containers
                .len(),
            3
        );
        assert_eq!(
            list_containers(&tmp, Some(ContainerFilter::default()))
                .unwrap()
                .into_inner()
                .containers
                .len(),
            3
        );
        assert_eq!(
            list_pod_sandbox(&tmp, None)
                .unwrap()
                .into_inner()
                .items
                .len(),
            2
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn list_pod_sandbox_honra_o_id_e_o_estado() {
        let tmp = base_with_two_pods("sb");
        let got = list_pod_sandbox(
            &tmp,
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
            &tmp,
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
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `label_selector` is a SUBSET match. Comparing whole maps would match nothing in
    /// practice, because a real container carries every label the kubelet set on it.
    #[test]
    fn o_label_selector_e_subconjunto_e_nao_igualdade() {
        let tmp = base_with_two_pods("lbl");
        let got = list_containers(
            &tmp,
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
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
