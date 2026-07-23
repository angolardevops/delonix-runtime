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
    use std::process::Stdio;
    let status = Command::new(delonix_bin())
        .env("DELONIX_ROOT", base)
        .env("DELONIX_INTERNAL", "1")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(st)?;
    Ok(status.success())
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
            if !delonix_detached(base, &["netns", "attach", &pod])? {
                return Err(Status::internal(format!(
                    "failed to create the ingress sandbox {pod}"
                )));
            }
        } else if !delonix_detached(base, &["pod", "create", &pod, "--network"])? {
            // ROOT: infra container (`pod-cri-<id>`) holds the netns ("pause"-style).
            return Err(Status::internal(format!(
                "failed to create the pod sandbox {pod}"
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
                let _ = delonix(base, &["netns", "detach", &format!("cri-{id}")]);
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

pub fn list_pod_sandbox(base: &Path) -> Result<Response<ListPodSandboxResponse>, Status> {
    let items = list_recs::<SandboxRec>(&sb_dir(base))
        .iter()
        .map(to_pod_sandbox)
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
    let seccomp_unconfined = sc
        .and_then(|s| s.seccomp.as_ref())
        .map(|p| p.profile_type == security_profile::ProfileType::Unconfined as i32)
        .unwrap_or(false);
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
        log_path: full_log_path,
        labels: cfg.labels,
        annotations: cfg.annotations,
        readonly_rootfs,
        privileged,
        seccomp_unconfined,
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

pub fn start_container(
    base: &Path,
    id: String,
) -> Result<Response<StartContainerResponse>, Status> {
    let mut rec: ContainerRec = read_rec(&ct_dir(base), &id)?;
    let name = format!("cri-{id}");
    let mut args: Vec<String> = vec![
        "container".into(),
        "run".into(),
        "-d".into(),
        "--name".into(),
        name,
    ];
    // Logs in the path/format the kubelet/crictl expect (CRI), if any.
    if !rec.log_path.is_empty() {
        if let Some(dir) = std::path::Path::new(&rec.log_path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        args.push("--log-file".into());
        args.push(rec.log_path.clone());
        args.push("--log-cri".into());
    }
    // Joins the pod sandbox's netns (network/namespace sharing), unless the pod
    // uses the host's network.
    if let Ok(sb) = read_rec::<SandboxRec>(&sb_dir(base), &rec.sandbox_id) {
        if !sb.host_network {
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
        // pod sysctls, applied to the container (shares the pod's namespaces).
        for s in &sb.sysctls {
            args.push("--sysctl".into());
            args.push(s.clone());
        }
    }
    // Security context → flags.
    if rec.readonly_rootfs {
        args.push("--read-only".into());
    }
    if rec.privileged {
        args.push("--cap-add".into());
        args.push("ALL".into());
        args.push("--security-opt".into());
        args.push("seccomp=unconfined".into());
    } else if rec.seccomp_unconfined {
        args.push("--security-opt".into());
        args.push("seccomp=unconfined".into());
    }
    for c in &rec.cap_add {
        args.push("--cap-add".into());
        args.push(c.trim_start_matches("CAP_").to_string());
    }
    for c in &rec.cap_drop {
        args.push("--cap-drop".into());
        args.push(c.trim_start_matches("CAP_").to_string());
    }
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
    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    if !delonix_detached(base, &argv)? {
        return Err(Status::internal(format!("failed to start container {id}")));
    }
    rec.started = true;
    write_rec(&ct_dir(base), &id, &rec)?;
    Ok(Response::new(StartContainerResponse {}))
}

pub fn stop_container(
    base: &Path,
    id: String,
    timeout: i64,
) -> Result<Response<StopContainerResponse>, Status> {
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

pub fn list_containers(base: &Path) -> Result<Response<ListContainersResponse>, Status> {
    let containers = list_recs::<ContainerRec>(&ct_dir(base))
        .iter()
        .map(|r| to_container(base, r))
        .collect();
    Ok(Response::new(ListContainersResponse { containers }))
}

pub fn container_status(
    base: &Path,
    id: String,
) -> Result<Response<ContainerStatusResponse>, Status> {
    let r: ContainerRec = read_rec(&ct_dir(base), &id)?;
    // Real exit code (from the Store), so the kubelet sees the exit cause instead
    // of a fixed `0`. `finished_at`/`reason` follow along.
    let exit = delonix_exit(base, &r.id);
    let status = ContainerStatus {
        id: r.id.clone(),
        metadata: Some(ContainerMetadata {
            name: r.name.clone(),
            attempt: r.attempt,
        }),
        state: delonix_state(base, &r.id),
        created_at: r.created_at,
        started_at: if r.started { r.created_at } else { 0 },
        finished_at: if exit.is_some() { now_ns() } else { 0 },
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
    let (fid, fsb) = filter.map(|f| (f.id, f.pod_sandbox_id)).unwrap_or_default();
    let stats = list_recs::<ContainerRec>(&ct_dir(base))
        .into_iter()
        .filter(|r| (fid.is_empty() || r.id == fid) && (fsb.is_empty() || r.sandbox_id == fsb))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
