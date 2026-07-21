//! `delonix-runtime-core` — shared types, state and errors of the **engine**
//! (Container/Vm/Status), independent of any notion of tenant, plan,
//! license or console. It is the foundation of the Delonix Runtime — meant to live in
//! its own opensource repository, without any dependency on the PaaS side
//! (`delonix-core`, which handles tenants/licensing/billing, DEPENDS on this
//! crate and re-exports it — never the other way around).

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub mod cred_vault;
mod error;
pub mod events;
pub mod metrics;
pub mod secret;
mod store;
pub mod telemetry;
pub mod typestate;
pub mod virt;
pub mod workload_net;

pub use error::{Error, Result};
pub use secret::{Secret, SecretStore};
pub use store::{JsonStore, Store};

/// Formats a unix instant as LOCAL date/time "YYYY-MM-DD HH:MM:SS".
/// Uses `localtime_r` (honors /etc/localtime|TZ); on failure, returns the raw value.
pub fn fmt_local_ts(unix: u64) -> String {
    let t = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `t` is valid; `localtime_r` writes into `tm` (our buffer, of the
    // right size) and returns NULL only on error — handled below.
    if unsafe { libc::localtime_r(&t, &mut tm).is_null() } {
        return unix.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// A mount to inject into the container (named volume or *bind mount*).
///
/// `source` is a path **on the host** (a volume's `_data`, or an arbitrary
/// path); `target` is the path **inside** the container. It is zero-copy: the
/// kernel shares the same blocks, there is no data copy.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Mount {
    /// Source path on the host.
    pub source: String,
    /// Mount point inside the container (starts with `/`).
    pub target: String,
    /// If `true`, mounts read-only.
    pub readonly: bool,
}

/// An L4 per-container firewall rule (shape from the Console UI). It is the
/// CANONICAL type: persisted in the [`Container`] and (de)serialized both on write
/// (`POST .../firewall`) and on read (`GET .../firewall`). `delonix-net`
/// re-exports it to apply via nftables.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct FwRule {
    /// `in` (traffic TO the container) or `out` (FROM the container).
    #[serde(default)]
    pub dir: String,
    /// `tcp`/`udp`/`any`.
    #[serde(default)]
    pub proto: String,
    /// port (or `*`/empty = any).
    #[serde(default)]
    pub port: String,
    /// CIDR of the other end (source on `in`, destination on `out`); `0.0.0.0/0`/`*` = any.
    #[serde(default)]
    pub src: String,
    /// `allow` (accept) or `deny` (drop).
    #[serde(default)]
    pub action: String,
    /// Free-form UI note (cosmetic; preserved in the persistence round-trip).
    #[serde(default)]
    pub note: String,
}

/// L4 firewall configuration of a container, applied via nftables and
/// persisted in the [`Container`] so the Console can READ the real rules.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ContainerFw {
    #[serde(default)]
    pub enabled: bool,
    /// default inbound policy: `allow` or `deny`.
    #[serde(default, rename = "policyIn")]
    pub policy_in: String,
    #[serde(default, rename = "policyOut")]
    pub policy_out: String,
    #[serde(default)]
    pub rules: Vec<FwRule>,
    /// Logical namespace of the container (default `default`). When the container does NOT
    /// have an explicit inbound policy (no inbound `rules` and `policy_in` !=
    /// `deny`), the inbound applies **namespace isolation**: accepts the same
    /// namespace (`@dlxns_<ns>`) and drops NEW connections from containers of another
    /// namespace (`@dlxall` + `ct state new`). An explicit policy (Dependency/
    /// Ingress) is authoritative and overrides this (see `fw_chain_body`).
    #[serde(default = "default_namespace")]
    pub namespace: String,
}

/// Default namespace (`default`) — everything in `default` = open SDN (the same
/// namespace contains everyone), preserving the pre-namespaces behavior.
pub fn default_namespace() -> String {
    "default".to_string()
}

impl Default for ContainerFw {
    fn default() -> Self {
        // `namespace` NEVER empty (the derive would give ""); everything else is the zero-value.
        ContainerFw {
            enabled: false,
            policy_in: String::new(),
            policy_out: String::new(),
            rules: Vec::new(),
            namespace: default_namespace(),
        }
    }
}

/// `proto` accepted in a firewall rule (interpolated into nft): empty, any, tcp, udp.
pub fn fw_proto_ok(p: &str) -> bool {
    matches!(p, "" | "any" | "tcp" | "udp")
}

/// safe `port`: empty, `*`, number 1..=65535, or range `n-m`.
pub fn fw_port_ok(p: &str) -> bool {
    if p.is_empty() || p == "*" {
        return true;
    }
    let num_ok = |s: &str| {
        s.parse::<u32>()
            .map(|n| (1..=65535).contains(&n))
            .unwrap_or(false)
    };
    match p.split_once('-') {
        Some((a, b)) => num_ok(a) && num_ok(b),
        None => num_ok(p),
    }
}

/// safe `src`: empty, `*`, `0.0.0.0/0`, or an IP/CIDR (v4/v6) — only IP/CIDR
/// characters (no spaces/`;`/`{`/`}`/newline, which would inject nft syntax).
pub fn fw_src_ok(s: &str) -> bool {
    if s.is_empty() || s == "*" || s == "0.0.0.0/0" {
        return true;
    }
    if s.len() > 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || matches!(b, b'.' | b':' | b'/'))
    {
        return false;
    }
    let (addr, mask) = s
        .split_once('/')
        .map(|(a, m)| (a, Some(m)))
        .unwrap_or((s, None));
    if let Some(m) = mask {
        match m.parse::<u32>() {
            Ok(n) if n <= 128 => {}
            _ => return false,
        }
    }
    addr.parse::<std::net::IpAddr>().is_ok()
}

impl FwRule {
    /// Are the fields interpolated into the `nft` script (`src`/`proto`/`port`) SAFE?
    /// Defense against nftables injection: builders MUST skip unsafe rules.
    pub fn nft_safe(&self) -> bool {
        fw_proto_ok(&self.proto) && fw_port_ok(&self.port) && fw_src_ok(&self.src)
    }
}

/// The state of a container/VM in its lifecycle (6 states). `Deserialize` is
/// manual (further below) to accept the legacy `{"Exited": code}` format.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Created, not yet started (transitioning to Running).
    Created,
    /// Running (has a live init `pid`).
    Running,
    /// Suspended (cgroup freezer / `virsh suspend`) — processes frozen.
    Paused,
    /// Cleanly stopped (intentional stop, or exit with code 0).
    Stopped,
    /// Terminated with exit code ≠ 0.
    Failed(i32),
    /// Unexpected death (killed by signal/OOM, or disappearance without a clean stop).
    Crashed,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Created => write!(f, "created"),
            Status::Running => write!(f, "running"),
            Status::Paused => write!(f, "paused"),
            Status::Stopped => write!(f, "stopped"),
            Status::Failed(code) => write!(f, "failed ({code})"),
            Status::Crashed => write!(f, "crashed"),
        }
    }
}

impl Status {
    /// Terminal state from the result of a process `wait()`:
    /// code 0 → Stopped, code ≠ 0 → Failed, killed by signal → Crashed.
    pub fn from_wait(code: i32, signaled: bool) -> Status {
        if signaled {
            Status::Crashed
        } else if code == 0 {
            Status::Stopped
        } else {
            Status::Failed(code)
        }
    }

    /// `true` if the container/VM is listed WITHOUT `-a`. Only `Failed`/`Crashed` require
    /// `-a` (hidden by default); Running/Created/Paused/Stopped are shown.
    pub fn shown_by_default(&self) -> bool {
        !matches!(self, Status::Failed(_) | Status::Crashed)
    }

    /// `true` if it has already terminated (neither active nor suspended).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Status::Stopped | Status::Failed(_) | Status::Crashed)
    }

    /// Associated exit code (Stopped=0, Failed=n, Crashed=137), for propagation.
    pub fn exit_code(&self) -> i32 {
        match self {
            Status::Failed(n) => *n,
            Status::Crashed => 137,
            _ => 0,
        }
    }
}

// Manual Deserialize: accepts the new format AND the legacy `{"Exited": code}` from
// old records (maps to Stopped/Failed), so as not to lose containers/VMs.
impl<'de> Deserialize<'de> for Status {
    fn deserialize<D>(d: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        enum Repr {
            Created,
            Running,
            Paused,
            Stopped,
            Failed(i32),
            Crashed,
            Exited(i32), // legacy
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Created => Status::Created,
            Repr::Running => Status::Running,
            Repr::Paused => Status::Paused,
            Repr::Stopped => Status::Stopped,
            Repr::Failed(n) => Status::Failed(n),
            Repr::Crashed => Status::Crashed,
            Repr::Exited(0) => Status::Stopped,
            Repr::Exited(n) => Status::Failed(n),
        })
    }
}

/// An ADDITIONAL network connection of a container (multi-homing, `network
/// connect`): the network, the assigned IP and the interface index (`eth<idx>`, >=1).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExtraNet {
    pub network: String,
    pub ip: String,
    pub idx: u32,
}

/// A container: the unit that Delonix creates, runs, inspects and destroys.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Container {
    /// Hexadecimal identifier of 16 characters.
    pub id: String,
    /// Human-readable name (also the hostname inside the container).
    pub name: String,
    /// Source image/rootfs.
    pub image: String,
    /// The init command and its arguments.
    pub command: Vec<String>,
    /// The PID (on the host) of the init process, while alive.
    pub pid: Option<i32>,
    /// The init's `starttime` (jiffies since boot, field 22 of `/proc/<pid>/stat`).
    /// Guards against PID reuse: before sending signals, we confirm that
    /// the PID still has this `starttime` — otherwise the kernel recycled it and we would kill
    /// an unrelated host process.
    #[serde(default)]
    pub pid_starttime: Option<u64>,
    /// The current state.
    pub status: Status,
    /// Creation instant (Unix seconds).
    pub created_unix: u64,
    /// cgroup memory limit (e.g.: `64M`).
    pub memory_max: String,
    /// CPU limit in cores (e.g.: `0.5`, `2`) — MANDATORY (Phase 7+security).
    #[serde(default = "default_cpus")]
    pub cpus: String,
    /// CPU weight/priority (cgroup `cpu.weight`, 1–10000) — scheduling.
    #[serde(default)]
    pub cpu_weight: Option<String>,
    /// Core affinity (cgroup `cpuset.cpus`, e.g.: `0-1`) — *pinning*.
    #[serde(default)]
    pub cpuset: Option<String>,
    /// Disk I/O weight (cgroup `io.weight`, 1–10000).
    #[serde(default)]
    pub io_weight: Option<String>,
    /// Pod the container belongs to (shares the network namespace).
    #[serde(default)]
    pub pod: Option<String>,
    /// Published ports (`hostPort:contPort[/proto]`) — DNAT on the host.
    #[serde(default)]
    pub ports: Vec<String>,
    /// Environment variables (`KEY=value`) — image `ENV` + `-e`/stack `env`.
    #[serde(default)]
    pub env: Vec<String>,
    /// Referenced secrets (`--secret <name>`): resolved to env at startup
    /// from the [`crate::SecretStore`]. The NAMES are stored (not the values), to
    /// re-resolve fresh at each start (picks up secret updates). [[Secret Manager]]
    #[serde(default)]
    pub secrets: Vec<String>,
    /// `true` → injects the secrets as **files** into an RO tmpfs at `/run/secrets`
    /// **inside the container namespace** (`--secret-files`), instead of environment
    /// variables. Safer: the values stay only in RAM (in-ns tmpfs) — never in
    /// `environ`/`inspect`, nor on the host or container fs. [[Secret Manager]]
    #[serde(default)]
    pub secret_files: bool,
    /// Process working directory (Docker/OCI `WorkingDir` of the image, or `-w`).
    /// The runtime does `chdir` to here before the `exec`. Empty/None = `/`. Without this,
    /// entrypoints that operate on the CWD (redis/postgres `chown -R`) run from `/`.
    #[serde(default)]
    pub workdir: Option<String>,
    /// `true` → rootfs mounted read-only (`--read-only`).
    #[serde(default)]
    pub read_only: bool,
    /// `true` → **privileged** container (`--privileged`): keeps all caps,
    /// seccomp unconfined, cgroup namespace (`CLONE_NEWCGROUP`) and `/sys/fs/cgroup`
    /// mounted RW delegated. Needed to run systemd+containerd (Kind nodes).
    /// ⚠️ Relaxes isolation — only for trusted workloads. Default `false`
    /// (normal containers stay exactly as before).
    #[serde(default)]
    pub privileged: bool,
    /// `key→value` labels (`docker/kubectl --label`). Persisted for
    /// `docker ps --filter label=` and `docker inspect .Config.Labels` (Kind filters
    /// nodes by `io.x-k8s.kind.cluster`).
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    /// Capabilities to drop (`--cap-drop`; `ALL` drops all).
    #[serde(default)]
    pub cap_drop: Vec<String>,
    /// Capabilities to restore (`--cap-add`), over the base or over `cap_drop ALL`.
    #[serde(default)]
    pub cap_add: Vec<String>,
    /// seccomp profile: `None` = allowlist (default); `Some("unconfined")` = no filter.
    #[serde(default)]
    pub seccomp: Option<String>,
    /// AppArmor profile applied (`aa_change_onexec`). Persisted so that `exec`
    /// also confines processes that enter the container later (probes/`crictl`).
    #[serde(default)]
    pub apparmor: Option<String>,
    /// `true` if the container has a user namespace (container root ≠ host root).
    #[serde(default)]
    pub userns: bool,
    /// The IP assigned on the `delonix0` bridge, if it has a network (Phase 3).
    #[serde(default)]
    pub ip: Option<String>,
    /// Name of the network it is connected to (`bridge` by default, or a user
    /// network). `None` = no network.
    #[serde(default)]
    pub network: Option<String>,
    /// Logical ISOLATION namespace (default `default`). Containers of different
    /// namespaces do NOT reach each other (even on the same network); only a `kind: Dependency`
    /// pierces the boundary. Propagates to `ContainerFw.namespace` and to registration in the
    /// nft sets `@dlxns_<ns>`/`@dlxall` on attach. [[namespace isolation]]
    #[serde(default = "default_namespace")]
    pub namespace: String,
    /// HTTP port auto-registered in the L7 proxy (`--expose`), under the internal FQDN
    /// `<name>.<namespace>.delonix.internal`. `None` = not exposed. Persisted to
    /// re-register on `start` and de-register on `rm`.
    #[serde(default)]
    pub expose: Option<u16>,
    /// ADDITIONAL networks the container is connected to (multi-homing, via
    /// `network connect`). Each one is its own `eth<idx>` interface.
    #[serde(default)]
    pub extra_networks: Vec<ExtraNet>,
    /// Additional DNS names of the container on its network (`--network-alias`), besides the
    /// container name — resolved by other containers on the same network.
    #[serde(default)]
    pub net_aliases: Vec<String>,
    /// DIRECTED DNS visibility (#2): allowlist of the peers that THIS container
    /// resolves. `None` = sees all (bidirectional, default). `Some([...])` = only
    /// resolves those (e.g.: app `knows=[db]` → app sees db, but db with `knows=[]` does not
    /// see app). Allows unidirectional communication where one knows the other but not
    /// vice versa.
    #[serde(default)]
    pub dns_knows: Option<Vec<String>>,
    /// tmpfs file systems to mount (`--tmpfs /path[:opts]`).
    #[serde(default)]
    pub tmpfs: Vec<String>,
    /// Resource limits (`--ulimit name=soft[:hard]`), applied before the exec.
    #[serde(default)]
    pub ulimits: Vec<String>,
    /// namespaced `sysctl`s (`--sysctl key=value`), written to `/proc/sys`.
    #[serde(default)]
    pub sysctls: Vec<String>,
    /// Devices to expose (`--device /dev/x[:/dev/y]`), attached in `/dev`.
    #[serde(default)]
    pub devices: Vec<String>,
    /// Restart policy (`no`|`on-failure[:max]`|`always`|`unless-stopped`).
    /// Consumed by the `delonix container run -d --restart` supervisor (a
    /// detached process per container, which becomes the PARENT of the container and therefore
    /// captures the real exit code); also used by the generated `systemd` unit and
    /// by the stack supervisor on the PaaS side.
    #[serde(default)]
    pub restart_policy: Option<String>,
    /// **Desired state**: the user explicitly requested `stop`. The
    /// `--restart` supervisor does NOT resurrect a container like this — it is the
    /// docker semantics (a `docker stop` on an `always` container does not
    /// restart it; only a `start` brings it back). Without this, `stop` and supervisor
    /// go to war: the container comes back on its own and the user cannot
    /// stop it. Cleared by `run`/`start`.
    #[serde(default)]
    pub stopped_by_user: bool,
    /// Mounted volumes/binds (persisted so the **zero-downtime update** can
    /// recreate the new container with EXACTLY the same volumes).
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Log driver (`file` by default, or `journald`/`syslog`).
    #[serde(default)]
    pub log_driver: Option<String>,
    /// Network bandwidth limit (`--net-bps`, e.g.: `10mbit`) — `tc`
    /// TBF/police on the host-side `veth`. `None` = no limit (free flow).
    #[serde(default)]
    pub net_bps: Option<String>,
    /// Burst (bytes) of the bandwidth limit (`--net-burst`, e.g.: `256k`). `None` =
    /// ~100 ms of flow by default. Only meaningful with [`Container::net_bps`].
    #[serde(default)]
    pub net_burst: Option<String>,
    /// CPU priority (`nice` value, -20..19; lower = higher priority),
    /// applied by `renice` to the process tree. `None` = nice 0 (normal).
    /// Persisted so startup reapplies it. `--priority high|normal|low` maps
    /// to -5/0/10; `--nice N` sets the raw value.
    #[serde(default)]
    pub nice: Option<i32>,
    /// L4 firewall CURRENTLY applied (nftables) to the container, persisted by
    /// `POST /api/containers/:id/firewall`. `None` = none was ever applied
    /// (the Console shows empty/fallback). Enables READING the real rules via
    /// `GET /api/containers/:id/firewall`, instead of hardcoded rules.
    #[serde(default)]
    pub firewall: Option<ContainerFw>,
    /// Hostname to set in the container's UTS namespace (`--hostname`; CRI
    /// `PodSandboxConfig.hostname`). `None` = uses the container name (historical
    /// behavior). Persisted so `start` reproduces the same hostname.
    #[serde(default)]
    pub hostname: Option<String>,
    /// UID to switch to before the `exec` (`--user`; CRI `run_as_user`/
    /// `run_as_username` resolved on the image). `None`/`Some(0)` = runs as root
    /// (historical). Persisted so `start` reproduces it. [[RunAsUser]]
    #[serde(default)]
    pub run_uid: Option<u32>,
    /// GID to switch to before the `exec` (`--user <uid>:<gid>`; CRI
    /// `run_as_group`). `None` = uses the UID's primary group. Persisted.
    #[serde(default)]
    pub run_gid: Option<u32>,
}

fn default_cpus() -> String {
    "1.0".to_string()
}

impl Container {
    /// Builds a container in the [`Status::Created`] state.
    pub fn new(
        id: String,
        name: String,
        image: String,
        command: Vec<String>,
        memory_max: String,
    ) -> Self {
        let created_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            id,
            name,
            image,
            command,
            pid: None,
            pid_starttime: None,
            status: Status::Created,
            created_unix,
            memory_max,
            cpus: default_cpus(),
            cpu_weight: None,
            cpuset: None,
            io_weight: None,
            pod: None,
            ports: Vec::new(),
            env: Vec::new(),
            secrets: Vec::new(),
            secret_files: false,
            workdir: None,
            read_only: false,
            privileged: false,
            labels: std::collections::BTreeMap::new(),
            cap_drop: Vec::new(),
            cap_add: Vec::new(),
            seccomp: None,
            apparmor: None,
            userns: false,
            ip: None,
            network: None,
            namespace: default_namespace(),
            expose: None,
            extra_networks: Vec::new(),
            net_aliases: Vec::new(),
            dns_knows: None,
            tmpfs: Vec::new(),
            ulimits: Vec::new(),
            sysctls: Vec::new(),
            devices: Vec::new(),
            restart_policy: None,
            stopped_by_user: false,
            mounts: Vec::new(),
            log_driver: None,
            net_bps: None,
            net_burst: None,
            nice: None,
            firewall: None,
            hostname: None,
            run_uid: None,
            run_gid: None,
        }
    }

    /// The first 12 characters of the id (as Docker shows).
    pub fn short_id(&self) -> &str {
        let n = self.id.len().min(12);
        &self.id[..n]
    }

    /// The path of this container's dedicated cgroup. It is NESTED under the
    /// `delonix.slice` (the parent cgroup with the AGGREGATE limits of all of Delonix),
    /// so that the sum of all containers never exhausts the host.
    pub fn cgroup(&self) -> String {
        format!("{}/delonix-{}", DELONIX_SLICE, self.id)
    }
}

/// A microVM (Cloud Hypervisor) — the unit of `kind: VM`. SIBLING model of the
/// [`Container`]: a VM has no rootfs/cgroup/seccomp/init-pid, so it does not make
/// sense to overload the `Container`. Persisted via [`store::JsonStore`]
/// (one JSON per name, under `$DELONIX_ROOT/vms`).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Vm {
    /// VM name (persistence key).
    pub name: String,
    /// Base disk (qcow2/raw) indicated in the manifest.
    pub disk: String,
    /// Per-VM qcow2 overlay, created over the base disk.
    pub overlay: String,
    /// Number of vCPUs.
    pub vcpus: u32,
    /// Memory (e.g.: `"2G"`).
    pub memory: String,
    /// Network used for the *tap*.
    pub network: String,
    /// Name of the *tap* interface on the bridge.
    pub tap: String,
    /// MAC derived from the name.
    pub mac: String,
    /// PID of the `cloud-hypervisor` process (if alive).
    pub pid: Option<i32>,
    /// Path of the Cloud Hypervisor API socket.
    pub api_socket: String,
    /// Lifecycle state (reuses [`Status`]).
    pub status: Status,
    /// Unix creation timestamp.
    pub created_unix: u64,
    /// Normalized restart policy (`"no"`|`"on-failure"`|`"always"`).
    #[serde(default)]
    pub restart_policy: Option<String>,
    /// IP assigned by DHCP (resolved from the MAC), when known.
    #[serde(default)]
    pub ip: Option<String>,
    /// Virtualization backend that started this VM (`"cloud-hypervisor"` or
    /// `"libvirt"`). Determines how to reconcile liveness/stop. Default for old
    /// records = `cloud-hypervisor` (the only backend before the VmBackend trait).
    #[serde(default = "default_vm_backend")]
    pub backend: String,
}

/// Default backend for VMs persisted before multi-backend support.
fn default_vm_backend() -> String {
    "cloud-hypervisor".to_string()
}

impl Vm {
    /// Builds a VM in the [`Status::Created`] state.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        disk: String,
        overlay: String,
        vcpus: u32,
        memory: String,
        network: String,
        tap: String,
        mac: String,
        api_socket: String,
    ) -> Self {
        let created_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            name,
            disk,
            overlay,
            vcpus,
            memory,
            network,
            tap,
            mac,
            pid: None,
            api_socket,
            status: Status::Created,
            created_unix,
            restart_policy: None,
            ip: None,
            backend: default_vm_backend(),
        }
    }
}

/// The parent cgroup of ALL Delonix containers. It has aggregate limits
/// (memory/CPU/PIDs) = a fraction of the host, so the host never dies from
/// an excess of containers (robustness protection).
pub const DELONIX_SLICE: &str = "/sys/fs/cgroup/delonix.slice";

/// Generates a container id: 16 hexadecimal characters.
pub fn generate_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let mixed = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ pid.rotate_left(32);
    format!("{mixed:016x}")
}

/// Path of the `delonix` binary to relaunch to delegate operations (exec, run,
/// API mutations…). Prefers the executable itself, but is **robust to
/// binary replacement** while the server runs (install/upgrade): in that
/// case `/proc/self/exe` is marked `" (deleted)"` and `current_exe()` returns
/// a nonexistent path — which made spawns fail with `os error 2`. Tries,
/// in order: current exe if it exists → path without the `(deleted)` suffix → `delonix`
/// on the `PATH` → the plain name.
pub fn self_bin() -> std::path::PathBuf {
    use std::path::{Path, PathBuf};
    if let Ok(p) = std::env::current_exe() {
        if p.exists() {
            return p;
        }
        let s = p.to_string_lossy();
        if let Some(real) = s.strip_suffix(" (deleted)") {
            let pb = PathBuf::from(real);
            if pb.exists() {
                return pb;
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let cand = Path::new(dir).join("delonix");
            if cand.exists() {
                return cand;
            }
        }
    }
    PathBuf::from("delonix")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fw_fields_reject_nft_injection() {
        // proto
        assert!(fw_proto_ok("tcp") && fw_proto_ok("udp") && fw_proto_ok("any") && fw_proto_ok(""));
        assert!(!fw_proto_ok("tcp drop; }"));
        assert!(!fw_proto_ok("tcp\n\t\taccept"));
        // port
        assert!(fw_port_ok("8080") && fw_port_ok("1000-2000") && fw_port_ok("*") && fw_port_ok(""));
        assert!(!fw_port_ok("80; flush ruleset"));
        assert!(!fw_port_ok("99999"));
        // src (the critical vector)
        assert!(
            fw_src_ok("10.0.0.0/16")
                && fw_src_ok("192.168.1.1")
                && fw_src_ok("0.0.0.0/0")
                && fw_src_ok("*")
        );
        assert!(!fw_src_ok(
            "1.2.3.4 accept; }; chain forward { policy drop; }"
        ));
        assert!(!fw_src_ok("1.2.3.4\n\t\taccept"));
        assert!(!fw_src_ok("$(reboot)"));
        // complete rule
        let bad = FwRule {
            src: "x; flush ruleset".into(),
            proto: "tcp".into(),
            port: "80".into(),
            ..Default::default()
        };
        assert!(!bad.nft_safe());
        let good = FwRule {
            src: "10.0.0.0/16".into(),
            proto: "tcp".into(),
            port: "443".into(),
            dir: "in".into(),
            action: "allow".into(),
            note: String::new(),
        };
        assert!(good.nft_safe());
    }

    fn sample(id: &str, name: &str) -> Container {
        Container::new(
            id.to_string(),
            name.to_string(),
            "/tmp/rootfs".to_string(),
            vec!["/bin/sh".to_string()],
            "64M".to_string(),
        )
    }

    #[test]
    fn id_has_16_hex_chars() {
        let id = generate_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn short_id_and_cgroup() {
        let c = sample("0123456789abcdef", "web");
        assert_eq!(c.short_id(), "0123456789ab");
        // nested under delonix.slice (aggregate limits — host protection).
        assert_eq!(
            c.cgroup(),
            "/sys/fs/cgroup/delonix.slice/delonix-0123456789abcdef"
        );
        assert_eq!(c.status, Status::Created);
    }

    #[test]
    fn status_displays_human_readably() {
        assert_eq!(Status::Running.to_string(), "running");
        assert_eq!(Status::Failed(137).to_string(), "failed (137)");
        assert_eq!(Status::Stopped.to_string(), "stopped");
        assert_eq!(Status::Crashed.to_string(), "crashed");
        // backcompat: legacy records `{"Exited": n}` deserialize to Stopped/Failed.
        assert_eq!(
            serde_json::from_str::<Status>(r#"{"Exited":0}"#).unwrap(),
            Status::Stopped
        );
        assert_eq!(
            serde_json::from_str::<Status>(r#"{"Exited":3}"#).unwrap(),
            Status::Failed(3)
        );
    }

    #[test]
    fn store_round_trip_and_lookup() {
        let dir = std::env::temp_dir().join(format!("delonix-test-{}", generate_id()));
        let store = Store::open(&dir).unwrap();

        let mut c = sample("aaaa1111bbbb2222", "web");
        c.pid = Some(4242);
        c.status = Status::Running;
        store.save(&c).unwrap();

        assert_eq!(store.load("aaaa1111bbbb2222").unwrap().pid, Some(4242));
        assert_eq!(store.load("aaaa1111").unwrap().name, "web");
        assert_eq!(store.load("web").unwrap().id, "aaaa1111bbbb2222");

        assert_eq!(store.list().unwrap().len(), 1);
        store.remove("aaaa1111bbbb2222").unwrap();
        assert!(store.load("web").is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
