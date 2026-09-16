//! The run specification of a container: what every entry point that starts one
//! builds — the CLI flags, a `kind: Container`/`Pod` document, a compose service,
//! a Docker Engine API create, a kind node, an app build — before one execution
//! path runs it.

use delonix_runtime_core::HealthConfig;

/// Arguments for `container run` (CLI and manifest), grouped — the list passed
/// the `too_many_arguments` threshold long ago.
///
/// **`Default` + `#[serde(default)]` on everything new**: the new fields (parity
/// with a full `run` specification) were added all at once; internal callers that only want
/// the essentials (`stack apply`, `cluster create`) use `..Default::default()`
/// and don't have to enumerate them all.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RunOpts {
    pub detach: bool,
    pub name: Option<String>,
    /// Internal hostname (`--hostname`). `None` = use the container name.
    #[serde(default)]
    pub hostname: Option<String>,
    /// The process user (`--user`, `uid[:gid]`|`name[:group]`). `None` = root.
    #[serde(default)]
    pub user: Option<String>,
    pub net: String,
    /// Logical ISOLATION namespace (default `default`). See "namespace isolation" in AGENTS.md.
    #[serde(default)]
    pub namespace: Option<String>,
    /// HTTP port to auto-register in the L7 proxy under the internal FQDN (`--expose`).
    #[serde(default)]
    pub expose: Option<u16>,
    pub volumes: Vec<String>,
    pub ports: Vec<String>,
    pub privileged: bool,
    pub entrypoint: Option<String>,
    /// Working directory the process starts in. `None` = the image's own
    /// configured workdir, or `/`.
    #[serde(default)]
    pub workdir: Option<String>,
    pub rm: bool,
    pub restart: String,
    pub devices: Vec<String>,
    pub env: Vec<String>,
    pub labels: Vec<String>,
    pub image: String,
    pub command: Vec<String>,
    /// Don't print the ID at the end of `-d`. For internal callers that compose
    /// their own output (e.g. `cluster create`, which starts N nodes and shows
    /// kind-style progress — the IDs in the middle were noise).
    #[serde(default)]
    pub quiet: bool,
    // ---- parity with a full `run` specification (all #[serde(default)]) ----
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub cpus: Option<String>,
    #[serde(default)]
    pub cpu_weight: Option<String>,
    #[serde(default)]
    pub cpuset: Option<String>,
    #[serde(default)]
    pub cgroup_parent: Option<delonix_runtime_core::CgroupParent>,
    /// The kubelet's cgroup for the pod (ADR 0038) — set only by the CRI.
    #[serde(default)]
    pub kube_cgroup_parent: Option<String>,
    #[serde(default)]
    pub io_weight: Option<String>,
    /// Composed cgroup-v2 `io.max` value half (`rbps=… wbps=…`), device excluded
    /// — the engine prepends the store device. `None` = no absolute ceiling.
    pub io_max: Option<String>,
    /// Caller cannot safely `fork()` — see `should_supervise` in the CLI. Set by the
    /// `serve docker-api` server, which is multi-threaded; everything else
    /// leaves it `false`.
    #[serde(default)]
    pub no_supervisor: bool,
    // (see `compose_io_max` for how the four `--device-*` flags become this)
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub cap_add: Vec<String>,
    #[serde(default)]
    pub cap_drop: Vec<String>,
    #[serde(default)]
    pub security_opt: Vec<String>,
    #[serde(default)]
    pub apparmor: Option<String>,
    #[serde(default)]
    pub selinux: Option<String>,
    #[serde(default)]
    pub userns: bool,
    #[serde(default)]
    pub no_userns: bool,
    #[serde(default)]
    pub host_pid: bool,
    #[serde(default)]
    pub host_ipc: bool,
    #[serde(default)]
    pub detect: bool,
    #[serde(default)]
    pub secret: Vec<String>,
    #[serde(default)]
    pub secret_files: bool,
    #[serde(default)]
    pub env_file: Vec<String>,
    #[serde(default)]
    pub tmpfs: Vec<String>,
    #[serde(default)]
    pub ulimit: Vec<String>,
    #[serde(default)]
    pub dns: Vec<String>,
    #[serde(default)]
    pub dns_search: Vec<String>,
    #[serde(default)]
    pub dns_option: Vec<String>,
    #[serde(default)]
    pub group_add: Vec<String>,
    #[serde(default)]
    pub masked_path: Vec<String>,
    #[serde(default)]
    pub readonly_path: Vec<String>,
    #[serde(default)]
    pub sysctl: Vec<String>,
    #[serde(default)]
    pub gpus: Option<String>,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub network_alias: Vec<String>,
    /// `--add-host name:ip` — extra `/etc/hosts` entries, PERSISTED so they
    /// survive the rewrite that every start does.
    #[serde(default)]
    pub add_host: Vec<String>,
    /// `--wait`: block until the image's HEALTHCHECK passes. Not persisted —
    /// it is a property of THIS invocation, not of the container.
    #[serde(default)]
    pub wait_healthy: bool,
    #[serde(default)]
    pub wait_timeout: u64,
    /// Continuous health check. PERSISTED (unlike `--wait`): it describes the
    /// container, and the monitor has to find it again after a `restart`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthConfig>,
    #[serde(default)]
    pub knows: Vec<String>,
    #[serde(default)]
    pub knows_none: bool,
    #[serde(default)]
    pub pod: Option<String>,
    /// (internal) init PID of the pod's infra container. When set, this container
    /// joins the infra's IPC/UTS namespaces (shared pod IPC + hostname). Set by

    /// `cmd::pod` for the pod's app containers; flows through the `--pod` re-exec.
    #[serde(default)]
    pub pod_infra_pid: Option<i32>,
    #[serde(default)]
    pub net_bps: Option<String>,
    #[serde(default)]
    pub net_burst: Option<String>,
    #[serde(default)]
    pub log_driver: Option<String>,
    #[serde(default)]
    pub log_file: Option<String>,
    #[serde(default)]
    pub log_cri: bool,
}
