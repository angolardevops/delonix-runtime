//! The records the engine persists that are plain data: a container's status and
//! its firewall rules (ADR-0040 D2.1, foundation). Pure — no I/O.

use serde::{Deserialize, Serialize};

/// An L4 per-container firewall rule (shape from the Console UI). It is the
/// CANONICAL type: persisted in the `Container` record and (de)serialized both on write
/// (`POST .../firewall`) and on read (`GET .../firewall`). `delonix-sdn`
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
    /// Name of the `kind: NetworkAccessRule` document that contributed this
    /// rule, if any. `None` for everything else — an imperative `net ingress
    /// allow` rule, a `kind: FirewallPolicy` rule, or a record written before
    /// this field existed. This is the contribution ledger that lets an
    /// independently-applied-and-removed `NetworkAccessRule` document retract
    /// only its own rule from a container's shared list, instead of the
    /// direction-wide replace `FirewallPolicy` does. `apply_fw_doc`'s replace
    /// is origin-aware for exactly this reason: it only replaces rules with
    /// `origin: None`, so it never silently erases a `NetworkAccessRule`'s
    /// contribution while reporting success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// L4 firewall configuration of a container, applied via nftables and
/// persisted in the `Container` record so the Console can READ the real rules.
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
