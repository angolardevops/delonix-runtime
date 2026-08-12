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
pub mod peer_cred;
pub mod secret;
mod store;
pub mod telemetry;
pub mod typestate;
pub mod virt;
pub mod workload_net;

pub use error::{Error, Result};
pub use secret::{Secret, SecretStore};
pub use store::{write_atomic, write_atomic_mode, write_private_temp, JsonStore, Store};

/// Are we in the INITIAL user namespace — i.e. is uid 0 here the host's root?
///
/// **`geteuid() == 0` does not answer this**, and the difference matters
/// everywhere the engine picks a privileged path. uid 0 inside a nested user
/// namespace buys nothing on the host: no write to the host's cgroup tree, no
/// `/run`, no privileged mount of the host's filesystems. Two independent
/// places in this workspace decided by `geteuid()` alone and both took the
/// ROOT path in exactly the environment where they had the least power.
///
/// The initial namespace is the only one whose `uid_map` is the identity map
/// over the whole range; anything else is nested. It is how podman answers the
/// same question. Unreadable `/proc` answers "initial" — the behaviour this
/// workspace had for years, so an unexpected environment keeps working exactly
/// as before instead of silently switching execution modes.
pub fn in_initial_userns() -> bool {
    let Ok(map) = std::fs::read_to_string("/proc/self/uid_map") else {
        return true;
    };
    initial_uid_map(&map)
}

/// Pure half of [`in_initial_userns`], so the parsing is tested without a
/// namespace to set up.
pub fn initial_uid_map(map: &str) -> bool {
    let mut lines = map.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else {
        return true; // empty map: cannot tell, keep the historical answer
    };
    if lines.next().is_some() {
        return false; // more than one range is never the initial namespace
    }
    let f: Vec<&str> = first.split_whitespace().collect();
    f == ["0", "0", "4294967295"]
}

/// Is this process rootless — i.e. WITHOUT privilege over the host?
///
/// True when the euid is not 0, and ALSO when it is 0 inside a nested user
/// namespace. See [`in_initial_userns`].
pub fn is_rootless() -> bool {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return true;
    }
    !in_initial_userns()
}

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
    /// Mount propagation: `private` (default), `rslave` (host → container) or
    /// `rshared` (both ways). `None` = `private`.
    ///
    /// Anything other than private has a cost that is easy to miss: the
    /// container's mount namespace root has to stop being `MS_PRIVATE`, so
    /// mount events from the host reach the container for EVERY mount, not just
    /// this one. That is what Docker and runc do by default; here it is opt-in,
    /// and only turns on when a mount actually asks for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub propagation: Option<String>,
}

impl Mount {
    /// Does this mount ask for host events to reach the container?
    pub fn wants_propagation(&self) -> bool {
        matches!(self.propagation.as_deref(), Some("rslave" | "rshared"))
    }
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

/// A container's continuous health check — the `--health-*` family.
///
/// Docker's semantics, deliberately, down to the defaults: the probe runs every
/// `interval`, a run that exceeds `timeout` counts as a failure, `retries`
/// consecutive failures flip the container to `unhealthy`, and failures during
/// `start_period` do not count (a service that takes 40s to open its port is
/// starting, not broken).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthConfig {
    /// Command line, run with `/bin/sh -c` INSIDE the container. Empty means
    /// "use the image's `HEALTHCHECK`" — resolved at monitoring time so a
    /// rebuilt image is picked up without recreating the container.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cmd: String,
    pub interval_secs: u64,
    pub timeout_secs: u64,
    pub retries: u32,
    pub start_period_secs: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        // Docker's own defaults. Matching them matters more than picking
        // "better" numbers: someone porting a compose file gets the cadence
        // their service was tuned for, not ours.
        Self {
            cmd: String::new(),
            interval_secs: 30,
            timeout_secs: 30,
            retries: 3,
            start_period_secs: 0,
        }
    }
}

/// Where a monitored container's health stands right now.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    /// Inside the start period, or no probe has completed yet.
    Starting,
    Healthy,
    Unhealthy,
}

impl std::fmt::Display for Health {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Health::Starting => "starting",
            Health::Healthy => "healthy",
            Health::Unhealthy => "unhealthy",
        })
    }
}

/// The last health observation, as written by the monitor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthState {
    pub health: Health,
    /// Consecutive failures so far. Reset to 0 by any success — `retries` is
    /// about a RUN of failures, not a total, so a service that fails once an
    /// hour is not unhealthy.
    #[serde(default)]
    pub failing_streak: u32,
    /// Exit code of the last probe.
    ///
    /// The probe's OUTPUT is deliberately not kept. Capturing it would mean
    /// redirecting the supervisor's stdio around each `exec`, which is
    /// process-global and races with the container the supervisor is also
    /// starting. `container healthcheck <id>` runs the same probe in the
    /// foreground and shows everything — a verdict here, the evidence there.
    #[serde(default)]
    pub last_exit: i32,
    /// Unix seconds of the last completed probe.
    #[serde(default)]
    pub checked_unix: i64,
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

/// safe `src`: empty, `*`, `0.0.0.0/0`, or an IPv4 address/CIDR — only IP/CIDR
/// characters (no spaces/`;`/`{`/`}`/newline, which would inject nft syntax).
///
/// **IPv4 only, deliberately.** This used to accept v6 too, which was not support but
/// a trap: the whole dataplane is a `table ip` (v4) and the SDN hands out v4
/// addresses, so a v6 CIDR passed validation, was interpolated into `ip saddr
/// <v6-cidr>`, and the user got a raw nft parse error dumped at them
/// (reproduced live with `--from 2001:db8::/32`). Refusing here turns that into a
/// clear message at the boundary, and keeps the promise that anything accepted is
/// actually enforced. Real v6 support means an `inet` table and a v6 SDN — a
/// separate piece of work, not a validator relaxation.
pub fn fw_src_ok(s: &str) -> bool {
    if s.is_empty() || s == "*" || s == "0.0.0.0/0" {
        return true;
    }
    if s.len() > 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'/'))
    {
        return false;
    }
    let (addr, mask) = s
        .split_once('/')
        .map(|(a, m)| (a, Some(m)))
        .unwrap_or((s, None));
    if let Some(m) = mask {
        match m.parse::<u32>() {
            Ok(n) if n <= 32 => {}
            _ => return false,
        }
    }
    addr.parse::<std::net::Ipv4Addr>().is_ok()
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
    /// ABSOLUTE disk I/O ceiling — the value half of a cgroup-v2 `io.max` line,
    /// without the device (e.g. `rbps=1048576 wbps=2097152`). The engine
    /// prepends the major:minor of the device backing the store, which is the
    /// only device a container's writes can reach.
    ///
    /// Distinct from [`Self::io_weight`], and the distinction matters: weight is
    /// PROPORTIONAL (how you split contention between cgroups) and gives no
    /// ceiling at all when nothing else is competing — one container alone can
    /// still saturate the disk and starve the host's journald/store/swap. This
    /// is the hard cap, the `--device-read-bps` family Docker and Podman both
    /// have and this engine did not.
    #[serde(default)]
    pub io_max: Option<String>,
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
    /// `key→value` annotations — deliberately SEPARATE from `labels`, the same
    /// split Kubernetes makes and for the same reason: labels are short,
    /// identifying and get shown/filtered on (`ps --filter label=`), annotations
    /// hold non-identifying data that may be large.
    ///
    /// The declarative reconciler keeps the last applied spec here
    /// (`delonix.io/last-applied`); putting a whole JSON spec in a label would
    /// wreck every listing that prints labels.
    #[serde(default)]
    pub annotations: std::collections::BTreeMap<String, String>,
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
    /// What the caller ASKED FOR in `--net` (`host`, `none`, or a network
    /// name), as opposed to `network`, which is what it ENDED UP on.
    ///
    /// The two differ in the case that matters: a container that asked for a
    /// network and did not get one. Without this, `None` in `network` is
    /// indistinguishable between "I asked for `--net host`" and "my network
    /// went away", and `describe` reported BOTH as `host` — the second one
    /// silently, on a container with no name resolution and no route to its
    /// peers. Diagnosing it meant grepping the raw record for `"network":
    /// null`, which is what a Makefile in the wild actually ended up doing.
    ///
    /// `None` on old records = unknown intent; the display falls back to the
    /// previous behaviour rather than inventing one.
    #[serde(default)]
    pub net_mode: Option<String>,
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
    /// Extra `/etc/hosts` entries (`--add-host name:ip`), as Docker/Podman.
    ///
    /// PERSISTED on purpose. `/etc/hosts` is rewritten from scratch on every
    /// start (`write_etc_files`), so anything injected by hand into a running
    /// container is gone at the next `start`/`restart` — silently, and the
    /// symptom lands far from the cause ("connection refused" to a name that
    /// worked five minutes ago). Keeping the entries on the record is what
    /// makes them survive, and it is the same trap already paid for by `-v`,
    /// by `-p` on a custom network and by pod membership.
    #[serde(default)]
    pub extra_hosts: Vec<String>,
    /// Continuous health check (`--health-cmd` and friends), when the user asked
    /// for one. `None` means only the image's `HEALTHCHECK` exists, evaluated
    /// on demand by `container healthcheck` — never monitored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthConfig>,
    /// Last observed health, written by whoever is monitoring (the detached
    /// container's supervisor).
    ///
    /// SEPARATE from `status` on purpose: an unhealthy container is still
    /// `Running`, and collapsing the two would make `ps` lie in both directions
    /// — a failing service reported as dead, or a restart policy reading
    /// "unhealthy" as an exit that never happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_state: Option<HealthState>,
    /// tmpfs file systems to mount (`--tmpfs /path[:opts]`).
    #[serde(default)]
    pub tmpfs: Vec<String>,
    /// Resource limits (`--ulimit name=soft[:hard]`), applied before the exec.
    #[serde(default)]
    pub ulimits: Vec<String>,
    /// namespaced `sysctl`s (`--sysctl key=value`), written to `/proc/sys`.
    #[serde(default)]
    pub sysctls: Vec<String>,
    /// A custom OCI seccomp profile, stored as its JSON CONTENT.
    ///
    /// The content and not the path, deliberately: the file lives on the host,
    /// the container's init runs after `pivot_root`, and a path recorded here
    /// would resolve to something else — or nothing — by the time it is read.
    /// It also makes the policy survive a `restart` even if the file moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp_profile: Option<String>,
    /// Explicit `/etc/resolv.conf` contents (`--dns`/`--dns-search`/
    /// `--dns-option`). When any of these is set they REPLACE the resolver the
    /// engine would have picked (network gateway, slirp, or the host's copy) —
    /// the caller asked for a specific one, and merging would produce a resolver
    /// nobody configured.
    #[serde(default)]
    pub dns_servers: Vec<String>,
    #[serde(default)]
    pub dns_searches: Vec<String>,
    #[serde(default)]
    pub dns_options: Vec<String>,
    /// Supplementary group ids (`--group-add`). Applied with `setgroups(2)`
    /// before the exec, whatever the uid — a container running as root can still
    /// need a group to reach a mounted share.
    #[serde(default)]
    pub group_add: Vec<u32>,
    /// Paths made unreadable inside the container (`--masked-path`). A file is
    /// covered with `/dev/null`, a directory with an empty read-only tmpfs —
    /// runc's own technique, and the reason `/proc/kcore` is not a hole in every
    /// container that ever ran.
    #[serde(default)]
    pub masked_paths: Vec<String>,
    /// Paths remounted read-only inside the container (`--readonly-path`).
    #[serde(default)]
    pub readonly_paths: Vec<String>,
    /// `PR_SET_NO_NEW_PRIVS`. `None` = the engine's own default, which is ON.
    ///
    /// Stricter than Docker and Podman, which only set it when asked. Kept as
    /// the default here deliberately; `Some(false)` is how a caller that owns
    /// the policy — the kubelet, through the CRI — says otherwise, and there it
    /// is the kubelet's call, not ours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_new_privs: Option<bool>,
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
    /// Short, stable reason code set by `reconcile_status` when `status` flips to
    /// `Crashed`: `"process_gone"` (the init pid no longer exists) or `"pid_reused"`
    /// (the kernel recycled the pid for an unrelated process before we noticed).
    /// The engine is never this process's real parent (it's reparented away at
    /// creation — see ARCHITECTURE), so this is best-effort diagnosis from polling
    /// `/proc`, not a captured exit code/signal. Cleared on the next successful start.
    #[serde(default)]
    pub crash_reason: Option<String>,
    /// When `crash_reason` was set (Unix seconds). `None` iff `crash_reason` is `None`.
    #[serde(default)]
    pub crashed_at: Option<u64>,
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
            io_max: None,
            pod: None,
            ports: Vec::new(),
            env: Vec::new(),
            secrets: Vec::new(),
            secret_files: false,
            workdir: None,
            read_only: false,
            privileged: false,
            labels: std::collections::BTreeMap::new(),
            annotations: std::collections::BTreeMap::new(),
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
            net_mode: None,
            extra_hosts: Vec::new(),
            health: None,
            health_state: None,
            tmpfs: Vec::new(),
            ulimits: Vec::new(),
            sysctls: Vec::new(),
            seccomp_profile: None,
            dns_servers: Vec::new(),
            dns_searches: Vec::new(),
            dns_options: Vec::new(),
            group_add: Vec::new(),
            masked_paths: Vec::new(),
            readonly_paths: Vec::new(),
            no_new_privs: None,
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
            crash_reason: None,
            crashed_at: None,
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

/// CPU topology of a VM (`<topology sockets cores threads/>`).
///
/// These four shapes ([`CpuTopology`], [`ExtraDisk`], [`ExtraNic`], [`VmVolume`])
/// live HERE and not in `delonix-vm` for one reason: they are part of what
/// [`Vm`] persists, and `delonix-runtime-core` cannot depend on `delonix-vm`
/// (the dependency runs the other way). `delonix-vm` re-exports them, so
/// `delonix_vm::CpuTopology` and friends keep resolving.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct CpuTopology {
    pub sockets: u32,
    pub cores: u32,
    pub threads: u32,
}

/// An extra disk attached to the VM (beyond the main overlay + cloud-init seed).
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ExtraDisk {
    /// Host path of the disk image.
    pub source: String,
    /// `"disk"` (default) or `"cdrom"`.
    #[serde(default)]
    pub device: String,
    /// Bus: `"virtio"` (default), `"sata"`, `"scsi"`, `"ide"`.
    #[serde(default)]
    pub bus: String,
    /// Image format: `"qcow2"` (default) or `"raw"`.
    #[serde(default)]
    pub format: String,
    /// Mount read-only.
    #[serde(default)]
    pub read_only: bool,
    /// Explicit target dev (e.g. `"vdb"`); auto-assigned when `None`.
    #[serde(default)]
    pub target: Option<String>,
}

/// An extra network interface beyond the VM's primary one.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ExtraNic {
    /// `"network"` (libvirt network), `"bridge"` (host bridge) or `"user"`.
    #[serde(default)]
    pub kind: String,
    /// Network/bridge name (for `network`/`bridge`).
    #[serde(default)]
    pub source: Option<String>,
    /// NIC model: `"virtio"` (default), `"e1000"`, `"rtl8139"`, …
    #[serde(default)]
    pub model: String,
    /// Fixed MAC (auto/random when `None`).
    #[serde(default)]
    pub mac: Option<String>,
}

/// A host directory shared into the VM via virtio-9p.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct VmVolume {
    /// 9p tag (short, unique in the VM) — the guest mounts by this tag.
    pub tag: String,
    /// Directory ON THE HOST to share (resolved by the bin).
    pub source: String,
    /// Mount point INSIDE the guest (e.g. `/mnt/dados`).
    pub mount_path: String,
    /// Mount read-only.
    #[serde(default)]
    pub read_only: bool,
}

/// Everything a VM was booted WITH that the flat [`Vm`] fields do not already
/// carry — the boot shape, as opposed to the lifecycle state (pid, status, ip).
///
/// **Why this exists.** `VmConfig` has ~30 fields and the `Vm` record used to
/// persist ten of them. Everything else existed only for the duration of a
/// `vm create` and died with it, which had two consequences, both measured:
/// `vm start`/`restart` silently rebooted a DIFFERENT machine than the one the
/// operator had created (no TPM, no CPU topology, no extra disks — its own
/// `--help` documented the loss), and the declarative reconciler could not
/// compare what the registry did not remember, so a `kind: Vm` accepted 36
/// spec fields and converged five.
///
/// This is the fifth time this engine has paid for the same rule, so it is
/// worth stating plainly: **state needed to RECONSTRUCT a resource has to be
/// persisted, not merely used at creation.** (Before: `-v` mounts, `-p` on a
/// custom network, extra networks, `Container.pod`.)
///
/// Every field is `#[serde(default)]` and the whole block is skipped when
/// empty, so a record written before this existed keeps deserializing and a VM
/// that uses none of it does not grow a byte on disk.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct VmBootSpec {
    /// Kernel for *direct boot* (vmlinux/bzImage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// Initrd/initramfs (with `kernel`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initrd: Option<String>,
    /// Firmware (alternative to the kernel: rust-hypervisor-fw/EDK2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// Kernel command line (with `kernel`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,
    /// cloud-init *seed* ISO (NoCloud) — secondary disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<String>,
    /// Backs the VM memory with *hugepages*.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hugepages: bool,
    /// CPU affinity (NUMA/pinning) — host CPU list all vCPUs are pinned to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_affinity: Option<String>,
    /// Host bridge (`net_mode = "bridge"`) or libvirt network (`"nat"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    /// Volumes/Storage shared into the VM via virtio-9p.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<VmVolume>,
    /// VNC graphical console (libvirt only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vnc: bool,
    /// Static IP — libvirt `nat` mode only (a DHCP reservation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_ip: Option<String>,
    /// Machine type (`<os><type machine=…>`), default `q35`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// CPU mode/model (`host-passthrough`, `host-model`, or a named model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    /// CPU topology.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_topology: Option<CpuTopology>,
    /// Emulated TPM 2.0.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tpm: bool,
    /// Video model (`virtio`/`qxl`/`vga`/`none`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
    /// OS boot device order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boot_order: Vec<String>,
    /// Extra disks beyond the main overlay + cloud-init seed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_disks: Vec<ExtraDisk>,
    /// Extra network interfaces beyond the primary one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_nics: Vec<ExtraNic>,
    /// Raw libvirt XML fragments injected before `</devices>`. **UNVALIDATED**.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub libvirt_xml_overlay: Vec<String>,
    /// FULL `<domain>` override used verbatim. **UNVALIDATED**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub libvirt_xml: Option<String>,
}

impl VmBootSpec {
    /// Nothing to persist — the common case (a VM created from a golden image
    /// with no advanced knob). Lets the whole block be skipped on write.
    pub fn is_empty(&self) -> bool {
        self == &VmBootSpec::default()
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
    /// Logical isolation namespace, the same notion `Container.namespace` carries:
    /// VMs of different namespaces do not reach each other, even on the same
    /// network. Old records default to `default` (the open SDN) — which is
    /// exactly what they were, so the default is a statement of fact and not a
    /// guess.
    #[serde(default = "default_namespace")]
    pub namespace: String,
    /// Virtualization backend that started this VM (`"cloud-hypervisor"` or
    /// `"libvirt"`). Determines how to reconcile liveness/stop. Default for old
    /// records = `cloud-hypervisor` (the only backend before the VmBackend trait).
    #[serde(default = "default_vm_backend")]
    pub backend: String,
    /// Free labels (k8s style) — short, identifying. The declarative reconciler
    /// stamps ownership here (`delonix.io/stack=<name>`), the same as it does on
    /// a `Container`, a `Volume` and a `Network`. `#[serde(default)]` so every
    /// record already on disk keeps deserializing.
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    /// Free annotations — non-identifying data that may be large; the
    /// reconciler's `delonix.io/last-applied` lives here, never in `labels`.
    #[serde(default)]
    pub annotations: std::collections::BTreeMap<String, String>,
    /// PCI passthrough device addresses (SR-IOV VFs, typically GPUs) attached
    /// at boot — copied from `VmConfig.devices`. Empty = none. Old records
    /// (pre this field) default to empty, same honesty as `backend` above:
    /// we genuinely don't know, and "none" is the safe reading for a VM that
    /// predates device tracking.
    #[serde(default)]
    pub devices: Vec<String>,
    /// Unix instant of the CURRENT boot (not `created_unix`, which is set once
    /// and never moves) — set on every real boot (`create`/auto-heal), cleared
    /// on `stop`. Distinguishing them matters for the same reason it does for
    /// `Container` (see `pid_starttime` there): a VM created yesterday but
    /// restarted 5 minutes ago should show an uptime of 5 minutes, not 1 day.
    #[serde(default)]
    pub started_unix: Option<u64>,
    /// The boot shape this VM was created with — see [`VmBootSpec`] for why it
    /// is persisted at all. Absent in every record written before it existed,
    /// which is not the same as "this VM has none": see `config_from`.
    #[serde(default, skip_serializing_if = "VmBootSpec::is_empty")]
    pub boot: VmBootSpec,
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
            labels: std::collections::BTreeMap::new(),
            annotations: std::collections::BTreeMap::new(),
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
            namespace: default_namespace(),
            backend: default_vm_backend(),
            devices: Vec::new(),
            started_unix: None,
            // Filled in by `delonix_vm::create_with` from the `VmConfig` that
            // is booting this VM; empty here so `Vm::new` keeps its signature
            // (nine positional arguments is already too many).
            boot: VmBootSpec::default(),
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

    /// A container record written before `annotations` existed must still
    /// deserialize. Built by REMOVING the key from a real serialization rather
    /// than hand-writing a legacy blob — a hand-written fixture drifts out of
    /// date the moment another field is added, and would stop testing anything.
    #[test]
    fn registo_de_container_sem_annotations_continua_a_desserializar() {
        let mut c = Container::new(
            "abc123".into(),
            "web".into(),
            "nginx".into(),
            vec!["nginx".into()],
            "64M".into(),
        );
        c.labels.insert("delonix.io/stack".into(), "web".into());
        c.annotations
            .insert("delonix.io/last-applied".into(), "{}".into());
        let mut v: serde_json::Value = serde_json::to_value(&c).unwrap();
        v.as_object_mut().unwrap().remove("annotations");
        let old: Container = serde_json::from_value(v).unwrap();
        assert!(old.annotations.is_empty());
        // The labels of the same record are untouched — the two maps are
        // independent on purpose (see the field's doc-comment).
        assert_eq!(old.labels.get("delonix.io/stack").unwrap(), "web");
    }

    /// Um registo de VM escrito antes destes campos tem de continuar a
    /// desserializar — senão toda a VM de um host actualizado desaparece do
    /// `vm ls` enquanto o disco continua lá.
    #[test]
    fn registo_de_vm_sem_labels_continua_a_desserializar() {
        let mut vm = Vm::new(
            "db".into(),
            "base.qcow2".into(),
            "db.qcow2".into(),
            2,
            "2G".into(),
            "bridge".into(),
            "tap0".into(),
            "52:54:00:00:00:01".into(),
            "/run/db.sock".into(),
        );
        vm.labels.insert("delonix.io/stack".into(), "s".into());
        let mut v: serde_json::Value = serde_json::to_value(&vm).unwrap();
        let o = v.as_object_mut().unwrap();
        o.remove("labels");
        o.remove("annotations");
        let old: Vm = serde_json::from_value(v).unwrap();
        assert!(old.labels.is_empty() && old.annotations.is_empty());
        assert_eq!(old.name, "db");
    }

    /// The same promise for the boot shape: a record written before
    /// [`VmBootSpec`] existed has no `boot` key, and must still load. Empty
    /// there means UNKNOWN, not "this VM had no kernel/TPM/volumes" — which is
    /// why `vm start` leaves such a VM alone instead of rebooting it with
    /// defaults it never asked for.
    #[test]
    fn registo_de_vm_sem_a_forma_de_arranque_continua_a_desserializar() {
        let mut vm = Vm::new(
            "dev".into(),
            "base.qcow2".into(),
            "dev.qcow2".into(),
            4,
            "8G".into(),
            "ingress".into(),
            "nat".into(),
            "52:54:00:00:00:02".into(),
            String::new(),
        );
        vm.boot.tpm = true;
        vm.boot.kernel = Some("/boot/vmlinuz".into());
        let mut v: serde_json::Value = serde_json::to_value(&vm).unwrap();
        v.as_object_mut().unwrap().remove("boot");
        let old: Vm = serde_json::from_value(v).unwrap();
        assert!(old.boot.is_empty());
        assert_eq!(old.vcpus, 4);
    }

    /// A VM that uses none of the advanced knobs must not grow a byte on disk:
    /// the whole block is skipped when empty. Without this, every record on
    /// every host would gain twenty-one null/false keys for nothing.
    #[test]
    fn a_forma_de_arranque_vazia_nao_e_escrita() {
        let vm = Vm::new(
            "simples".into(),
            "base.qcow2".into(),
            "s.qcow2".into(),
            1,
            "1G".into(),
            "ingress".into(),
            "tap0".into(),
            "52:54:00:00:00:03".into(),
            String::new(),
        );
        let v: serde_json::Value = serde_json::to_value(&vm).unwrap();
        assert!(
            v.as_object().unwrap().get("boot").is_none(),
            "a forma de arranque vazia não devia ser serializada: {v}"
        );
    }

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
        // IPv6 is REFUSED, not "supported": the dataplane is a v4 `table ip`, so a v6
        // CIDR used to pass here and blow up as a raw nft parse error deep inside the
        // holder (reproduced live). Accepting only what is actually enforced is the
        // whole contract of this validator.
        assert!(!fw_src_ok("2001:db8::/32"));
        assert!(!fw_src_ok("::1"));
        assert!(!fw_src_ok("fe80::1/64"));
        // A v4 mask wider than 32 is not a mask.
        assert!(!fw_src_ok("10.0.0.0/64"));
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
