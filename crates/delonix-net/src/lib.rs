//! `delonix-net` — Delonix Engine networking and firewall.
//!
//! Philosophy (from the architecture): **native netfilter, don't reinvent the firewall**.
//! This crate orchestrates the kernel tools — `ip` (iproute2) for
//! `bridge`/`veth`/`netns` and `nft` (nftables) for NAT and firewall — behind
//! a clean Rust API. It's the same pattern as `dockerd`, which invokes `iptables`.
//!
//! Network model (bridge, `docker0` style):
//! - bridge `delonix0` on a **free auto-detected** `/16` (avoids collision with
//!   Docker `172.17/16`, Podman `10.88/16` and the host's networks), with IP
//!   forwarding and `MASQUERADE`;
//! - each container gets a `veth` (`eth0`) attached to the bridge, with a
//!   deterministic IP derived from its id;
//! - the per-container firewall is a chain of its OWN in table `ip dlxing`,
//!   reached through a verdict map (`@fwmap`) keyed by IP — so dispatch costs
//!   the same with one container as with fifty.
//!
//! **The attach lives in [`infra`], not here.** This crate used to carry a
//! `Net` manager that ran `ip`/`nft` directly in the calling process; that is
//! gone (it could not work rootless, and with privilege it reached the HOST's
//! network, outside the isolation). Today the runtime creates the `netns` and
//! `infra::attach_container` asks the **holder** for the wiring over a control
//! socket.

use delonix_runtime_core::{Error, Result};
use std::process::{Command, Stdio};

pub mod bpf;
pub mod cni;
pub mod discover;
pub mod infra;
pub mod ipam;
pub mod wg;

pub use discover::{discover_ports, DiscoveredPort};

const BRIDGE: &str = "delonix0";

/// Base octet (`10.<base>.0.0/16`) of the default network. To **not collide**
/// with Docker (`172.17.0.0/16`), Podman (`10.88.0.0/16`) or the networks
/// already present on the host, we detect a free `/16` on the FIRST bridge creation
/// and **persist it** — the derived IPs have to be stable across invocations.
/// `DELONIX_SUBNET_BASE` forces a value; otherwise it reads the persisted file; otherwise
/// it scans the host and picks a free one.
fn default_base() -> u8 {
    if let Ok(Ok(b)) = std::env::var("DELONIX_SUBNET_BASE").map(|s| s.trim().parse::<u8>()) {
        return b;
    }
    let path = net_state_path();
    if let Ok(Ok(b)) = std::fs::read_to_string(&path).map(|s| s.trim().parse::<u8>()) {
        return b;
    }
    let base = pick_free_base();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = delonix_runtime_core::write_atomic(&path, base.to_string().as_bytes());
    base
}

fn net_state_path() -> std::path::PathBuf {
    let root = std::env::var("DELONIX_ROOT").unwrap_or_else(|_| "/var/lib/delonix".into());
    std::path::Path::new(&root).join("net").join("default-base")
}

/// `10.X` octets already in use in the host's routes/addresses (avoids collision with the
/// host, Docker, Podman and other active Delonix networks).
fn used_10_octets() -> std::collections::HashSet<u8> {
    let mut used = std::collections::HashSet::new();
    used.insert(88); // Podman's default
    used.insert(90); // Delonix service VIPs
    for args in [["-o", "addr"].as_slice(), ["route"].as_slice()] {
        if let Ok(out) = capture("ip", args) {
            for tok in out.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
                if let Some(rest) = tok.strip_prefix("10.") {
                    if let Some(Ok(b)) = rest.split('.').next().map(|o| o.parse::<u8>()) {
                        used.insert(b);
                    }
                }
            }
        }
    }
    used
}

/// Picks a free `10.X` base octet (preferring `200..=239`, far from the
/// Docker/Podman defaults and the most common user networks).
fn pick_free_base() -> u8 {
    let used = used_10_octets();
    (200..=239)
        .chain(11..=87)
        .chain(91..=199)
        .find(|b| !used.contains(b))
        .unwrap_or(201)
}

/// The `prefix`/`gateway`/`subnet` of the default network (derived from the base octet).
fn default_prefix() -> String {
    format!("10.{}", default_base())
}
fn default_gateway() -> String {
    format!("10.{}.0.1", default_base())
}
fn default_subnet() -> String {
    format!("10.{}.0.0/16", default_base())
}

// ---- process helpers -----------------------------------------------------

/// Runs a command; errors if the exit code is not zero.
fn run(prog: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| Error::Runtime {
            context: "spawn",
            message: format!("{prog}: {e}"),
        })?;
    if !out.status.success() {
        return Err(Error::Runtime {
            context: "net cmd",
            message: format!(
                "{prog} {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(())
}

/// Runs a command ignoring the result (for idempotent/cleanup steps).
fn run_ok(prog: &str, args: &[&str]) {
    let _ = Command::new(prog).args(args).output();
}

/// Runs a command and returns the stdout.
fn capture(prog: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| Error::Runtime {
            context: "spawn",
            message: format!("{prog}: {e}"),
        })?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parses an overlay peer entry: `<node_ip>` (flat VXLAN) OR
/// `<node_ip>=<wg_pubkey>=<wg_ip>` (encrypted). Returns (node_ip, Option<(pubkey, wg_ip)>).
pub fn parse_overlay_peer(s: &str) -> (String, Option<(String, String)>) {
    // Format `node_ip=wg_pubkey=wg_ip`. The pubkey is base64 and ENDS in `=`
    // (padding) — it collides with the delimiter. Since node_ip and wg_ip are IPs (never
    // contain `=`), we delimit by the FIRST and the LAST `=`; what remains in the
    // middle is the pubkey WITH its padding intact. (Flat VXLAN peer = just `node_ip`.)
    match (s.find('='), s.rfind('=')) {
        (Some(first), Some(last)) if last > first => {
            let node = &s[..first];
            let pubkey = &s[first + 1..last];
            let wgip = &s[last + 1..];
            if !pubkey.is_empty() && !wgip.is_empty() {
                return (
                    node.to_string(),
                    Some((pubkey.to_string(), wgip.to_string())),
                );
            }
            (node.to_string(), None)
        }
        _ => (s.split('=').next().unwrap_or_default().to_string(), None),
    }
}

fn link_exists(name: &str) -> bool {
    Command::new("ip")
        .args(["link", "show", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---- deterministic names and IPs -----------------------------------------

// Veths of an *extra* interface (multi-homing): suffixed by the index (>=1) so as
// not to collide with the primary interface's nor between networks. <= 15 chars.

/// Validates that `ip` is a usable unicast address in `prefix`'s `/16` subnet
/// (e.g.: prefix `10.88`): 4 octets, first two == prefix, not the gateway
/// (`prefix.0.1`), the network (`prefix.0.0`) or the broadcast (`prefix.255.255`).
pub fn valid_ip_in_subnet(prefix: &str, ip: &str) -> bool {
    // `prefix` chega aqui em DUAS formas e ambas têm de continuar a valer: a
    // legada de dois octetos (`10.210`, que sempre quis dizer `10.210.0.0/16`) e
    // um CIDR a sério. O `Cidr::parse` resolve as duas, por isso não há aqui um
    // caminho novo — há o mesmo caminho a saber mais.
    let Some(net) = Cidr::parse(prefix) else {
        return false;
    };
    let Some(addr) = Cidr::parse_addr(ip) else {
        return false;
    };
    if !net.contains(addr) {
        return false;
    }
    // Exclui a rede, o gateway e o broadcast. No `/16` isto é exactamente o que
    // a versão anterior excluía à mão (`.0.0`, `.0.1`, `.255.255`) — a regra
    // geral não é uma mudança de comportamento, é a mesma regra dita de uma
    // forma que também serve um /22.
    addr != net.base && addr != net.base.wrapping_add(1) && addr != net.last()
}

/// Deterministic IP in `10.88.A.B`, derived from the id (avoids .0/.1/.255).
/// Parses `hostPort:contPort[/tcp|udp]`, `contPort` or `hp:cp`. Returns
/// `(host_port, cont_port, proto)` — the host ADDRESS, if the spec carries one, is
/// dropped here; use [`parse_publish_addr`] when you need it (the bind side).
pub fn parse_publish(spec: &str) -> Result<(String, String, String)> {
    let (_, h, c, p) = parse_publish_addr(spec)?;
    Ok((h, c, p))
}

/// Parses the FULL publish spec, Docker-style: `[hostIp:]hostPort:contPort[/tcp|udp]`
/// (or a bare `contPort`). Returns `(host_addr, host_port, cont_port, proto)`.
///
/// `host_addr` is `None` when the spec doesn't name one — the caller then falls back to
/// `DELONIX_PUBLISH_ADDR` and, failing that, to the safe default `127.0.0.1`. Naming the
/// address in the spec is the ONLY way to expose a published port beyond the host's own
/// loopback without an environment variable: `-p 0.0.0.0:8080:80` (whole LAN),
/// `-p 192.168.1.106:8080:80` (one interface), `-p 192.168.122.1:8080:80` (the libvirt
/// gateway — reachable from VMs on that network, see `delonix vm reach`).
///
/// Only IPv4 is accepted: it is interpolated into the slirp api-socket's JSON, and the
/// `Ipv4Addr` parse is what keeps that boundary safe (same reason `DELONIX_PUBLISH_ADDR`
/// is validated). An IPv6 literal would also collide with the `:` splitting below.
pub fn parse_publish_addr(spec: &str) -> Result<(Option<String>, String, String, String)> {
    let (mapping, proto) = match spec.split_once('/') {
        Some((m, p)) => (m, p.to_lowercase()),
        None => (spec, "tcp".to_string()),
    };
    if proto != "tcp" && proto != "udp" {
        return Err(Error::Invalid(format!(
            "invalid protocol in '{spec}' (tcp|udp)"
        )));
    }
    // Split from the RIGHT: `contPort` is always last, `hostPort` before it, and
    // whatever remains in front is the host address.
    let (head, cont_port) = match mapping.rsplit_once(':') {
        Some((h, c)) => (h.trim(), c.trim()),
        None => (mapping.trim(), mapping.trim()),
    };
    let (host_addr, host_port) = match head.rsplit_once(':') {
        Some((a, p)) => (Some(a.trim()), p.trim()),
        None => (None, head),
    };
    let valid = |p: &str| {
        !p.is_empty()
            && p.chars().all(|c| c.is_ascii_digit())
            && p.parse::<u16>().map(|n| n > 0).unwrap_or(false)
    };
    if !valid(host_port) || !valid(cont_port) {
        // A range (`8000-8010:8000-8010`) is the shape people reach for next, and the
        // generic "invalid port" left them guessing whether it was the syntax or the
        // range that was wrong. Name it: expanding a range means N hostfwds and N DNAT
        // rules, which `expand_publish_range` does at the CLI boundary — this parser
        // stays one-spec-one-port on purpose, since everything downstream (port
        // ownership, unpublish, the store's `ports`) is keyed on a single port.
        let ranged = host_port.contains('-') || cont_port.contains('-');
        let hint = if ranged {
            " — a port RANGE has to be expanded first (`8000-8010:8000-8010` becomes one publish per port)"
        } else {
            " (e.g. 8080:80)"
        };
        return Err(Error::Invalid(format!("invalid port in '{spec}'{hint}")));
    }
    let host_addr = match host_addr {
        Some(a) if a.parse::<std::net::Ipv4Addr>().is_ok() => Some(a.to_string()),
        Some(a) => {
            return Err(Error::Invalid(format!(
                "invalid host address '{a}' in '{spec}' (an IPv4 literal, e.g. 0.0.0.0:8080:80)"
            )))
        }
        None => None,
    };
    Ok((
        host_addr,
        host_port.to_string(),
        cont_port.to_string(),
        proto,
    ))
}

/// Expands a publish spec that carries a port RANGE into one spec per port —
/// `-p 8000-8002:9000-9002` becomes `8000:9000`, `8001:9001`, `8002:9002`. A spec
/// without a range comes back unchanged (one element), so callers can pipe every
/// spec through this unconditionally.
///
/// Docker/Podman accept ranges and this engine did not: `parse_publish_addr` takes a
/// single port because everything downstream — port ownership, `unpublish`, the
/// container's stored `ports` — is keyed on one port, and that is worth keeping.
/// Expanding at the boundary gives the familiar syntax without making a range a
/// second kind of thing the whole stack has to understand.
///
/// Both sides must have the SAME width; a one-sided range (`8000-8002:80`) is refused
/// rather than guessed at. The host side may also be a single port with a ranged
/// container side, which Docker reads as "start here" — also refused, because it makes
/// the host allocation implicit and it is the shape people get wrong.
pub fn expand_publish_range(spec: &str) -> Result<Vec<String>> {
    let (mapping, proto_suffix) = match spec.split_once('/') {
        Some((m, p)) => (m, format!("/{p}")),
        None => (spec, String::new()),
    };
    if !mapping.contains('-') {
        return Ok(vec![spec.to_string()]);
    }
    // Split off the container port from the right, exactly like `parse_publish_addr`,
    // so a `hostIp:` head keeps working (`0.0.0.0:8000-8002:9000-9002`).
    let Some((head, cont)) = mapping.rsplit_once(':') else {
        return Err(Error::Invalid(format!(
            "invalid port range in '{spec}' (expected hostStart-hostEnd:contStart-contEnd)"
        )));
    };
    let (addr_prefix, host) = match head.rsplit_once(':') {
        Some((a, p)) => (format!("{a}:"), p),
        None => (String::new(), head),
    };
    let bounds = |s: &str| -> Option<(u32, u32)> {
        match s.split_once('-') {
            Some((a, b)) => Some((a.trim().parse().ok()?, b.trim().parse().ok()?)),
            None => {
                let n = s.trim().parse().ok()?;
                Some((n, n))
            }
        }
    };
    let (Some((hs, he)), Some((cs, ce))) = (bounds(host), bounds(cont)) else {
        return Err(Error::Invalid(format!(
            "invalid port range in '{spec}' (ports must be numbers)"
        )));
    };
    if hs > he || cs > ce || he > 65535 || ce > 65535 || hs == 0 || cs == 0 {
        return Err(Error::Invalid(format!(
            "invalid port range in '{spec}' (start must be <= end, within 1-65535)"
        )));
    }
    if he - hs != ce - cs {
        return Err(Error::Invalid(format!(
            "port range mismatch in '{spec}': {} host port(s) for {} container port(s) — both sides must be the same width",
            he - hs + 1,
            ce - cs + 1
        )));
    }
    Ok((0..=(he - hs))
        .map(|i| format!("{addr_prefix}{}:{}{proto_suffix}", hs + i, cs + i))
        .collect())
}

/// The address a published port should BIND to on the host: the spec's own
/// `hostIp` if it names one, else `DELONIX_PUBLISH_ADDR`, else the safe default
/// `127.0.0.1`. Single source of truth — the two publish datapaths (per-container
/// slirp and the single ingress slirp) must not diverge on this.
pub fn publish_bind_addr(spec_addr: Option<&str>) -> String {
    spec_addr
        .map(|a| a.to_string())
        .or_else(|| {
            std::env::var("DELONIX_PUBLISH_ADDR")
                .ok()
                .filter(|a| a.parse::<std::net::Ipv4Addr>().is_ok())
        })
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/// Can this process bind `port` on `addr`, as far as PERMISSION goes? Ports below
/// `net.ipv4.ip_unprivileged_port_start` (1024 by default) need `CAP_NET_BIND_SERVICE`,
/// which a rootless engine does not have — and the bind that publishes a port happens
/// on the HOST side, performed by `slirp4netns` as this same unprivileged user. Hence
/// `-p 80:80` failing with the slirp's raw `add_hostfwd` JSON while `-p 8080:80` works.
///
/// Probes with a REAL bind instead of comparing against the sysctl: the sysctl is not
/// the whole rule (a binary carrying `CAP_NET_BIND_SERVICE` binds 80 with the sysctl
/// untouched), and only the kernel knows for sure. `EADDRINUSE` is deliberately NOT a
/// failure here — a busy port is a different diagnosis, with its own error that names
/// the owner, and this check must not steal it.
pub fn can_bind_host_port(addr: &str, port: u16) -> bool {
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
    let ip: Ipv4Addr = addr.parse().unwrap_or(Ipv4Addr::LOCALHOST);
    match TcpListener::bind(SocketAddrV4::new(ip, port)) {
        Ok(_) => true,
        Err(e) => !matches!(e.kind(), std::io::ErrorKind::PermissionDenied),
    }
}

/// Specification of a container's network bandwidth limit.
/// `rate_bit` is the throughput in bits/second; `burst_bytes` is the (token) bucket
/// of the TBF/police, in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetRate {
    pub rate_bit: u64,
    pub burst_bytes: u64,
}

/// Separates a value with a `k`/`m`/`g`/`t` suffix from its multiplier (base 1000
/// for network throughput, 1024 for buffer sizes). No suffix, mult. = 1.
fn split_unit(s: &str, base: u64) -> (&str, u64) {
    let mult = match s.chars().last().map(|c| c.to_ascii_lowercase()) {
        Some('k') => base,
        Some('m') => base * base,
        Some('g') => base * base * base,
        Some('t') => base * base * base * base,
        _ => return (s, 1),
    };
    (&s[..s.len() - 1], mult)
}

/// Human throughput (`10mbit`, `1g`, `512k`, `1000000`) → bits/second. The suffixes
/// are decimal (k=10³, m=10⁶, g=10⁹), as is the convention in networking; the trailing
/// `bit`/`bps` tokens are ignored.
fn parse_rate_bits(s: &str) -> Result<u64> {
    let lower = s.trim().to_lowercase();
    let body = lower
        .strip_suffix("bps")
        .or_else(|| lower.strip_suffix("bit"))
        .unwrap_or(lower.as_str());
    let (num, mult) = split_unit(body.trim(), 1000);
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| Error::Invalid(format!("invalid --net-bps: '{s}'")))?;
    if !n.is_finite() || n <= 0.0 {
        return Err(Error::Invalid(format!("--net-bps must be positive: '{s}'")));
    }
    scaled(n, mult).ok_or_else(|| Error::Invalid(format!("--net-bps is out of range: '{s}'")))
}

/// `value * mult` as a `u64`, or `None` if it does not fit.
///
/// `as u64` on an `f64` is a SATURATING cast in Rust, so without this a
/// `--net-bps 99999999999g` became `u64::MAX` — not a refusal, and not the limit
/// asked for either. The same guard is in `delonix-volume::parse_size_bytes`,
/// written there for a quota that reported itself as SET and could never be
/// reached; it just was never carried over to its siblings here.
fn scaled(value: f64, mult: u64) -> Option<u64> {
    let scaled = value * mult as f64;
    if !scaled.is_finite() || scaled >= u64::MAX as f64 {
        return None;
    }
    Some(scaled as u64)
}

/// Human size in bytes (`256k`, `1m`, `4096`). Binary suffixes (k=1024, …);
/// a trailing `b`/`B` is accepted (`256kb`). Returns `None` if invalid.
fn parse_size_bytes(s: &str) -> Option<u64> {
    let lower = s.trim().to_lowercase();
    let body = lower.strip_suffix('b').unwrap_or(lower.as_str());
    let (num, mult) = split_unit(body.trim(), 1024);
    let n: f64 = num.trim().parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    scaled(n, mult)
}

/// Parses a bandwidth limit: a throughput (`--net-bps`) and an optional
/// burst in bytes (`--net-burst`). Without a burst, it uses ~100 ms of throughput, with a
/// floor of 16 KiB (enough that the token bucket doesn't throttle startup).
pub fn parse_net_rate(rate: &str, burst: Option<&str>) -> Result<NetRate> {
    let rate_bit = parse_rate_bits(rate)?;
    let burst_bytes = match burst {
        Some(b) => {
            let v = parse_size_bytes(b)
                .ok_or_else(|| Error::Invalid(format!("invalid --net-burst: '{b}'")))?;
            if v == 0 {
                return Err(Error::Invalid("--net-burst cannot be zero".into()));
            }
            v
        }
        None => (rate_bit / 8 / 10).max(16 * 1024),
    };
    Ok(NetRate {
        rate_bit,
        burst_bytes,
    })
}

/// A network's address space, as a real prefix instead of the two octets of a
/// hardcoded `/16`.
///
/// **Why this type exists (ADR-0013 tier A).** Everything about a network used
/// to be derived from ONE octet: the record on disk held `210`, the bridge was
/// named from it, the gateway was `10.<n>.0.1`, and the IPAM allocated inside
/// `10.<n>.0.0/16` because that was the only shape there was. `--subnet` could
/// only ever pick which octet. That is a fine default and a poor contract: a
/// network engineer has an address plan, and «any /16 you like as long as it is
/// `10.<200-254>`» does not meet it.
///
/// Kept deliberately small — no dependency, no `ipnet` crate (this repo does not
/// grow its supply chain for arithmetic), and no IPv6: v6 is DISABLED by design
/// in this engine (v0.37.1, it was a complete bypass of the policy model) and
/// re-enabling it is a security decision, not a widening of this struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    /// Network address, host bits already cleared.
    pub base: u32,
    pub len: u8,
}

impl Cidr {
    /// Parses `a.b.c.d/len`, and ALSO the legacy two-octet form (`10.210`),
    /// which every record written before this meant as `10.210.0.0/16`.
    ///
    /// Accepting the old form here is what makes the migration a non-event:
    /// a record holding a bare octet keeps meaning exactly what it always meant,
    /// and is rewritten in the new shape on the next write. Same promotion the
    /// `base=<n>` line already does.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (addr, len) = match s.split_once('/') {
            Some((a, l)) => (a, l.parse::<u8>().ok()?),
            // Legacy `10.210` — two octets, always a /16.
            None if s.split('.').count() == 2 => (s, 16),
            None => return None,
        };
        if len > 32 {
            return None;
        }
        let mut oct = [0u8; 4];
        let parts: Vec<&str> = addr.split('.').collect();
        if parts.is_empty() || parts.len() > 4 {
            return None;
        }
        for (i, p) in parts.iter().enumerate() {
            oct[i] = p.parse::<u8>().ok()?;
        }
        let raw = u32::from_be_bytes(oct);
        // Host bits are CLEARED rather than rejected: `10.210.5.7/16` is how
        // people write «the /16 that address is in», and refusing it would be
        // pedantry that helps nobody.
        let mask = Self::mask(len);
        Some(Self {
            base: raw & mask,
            len,
        })
    }

    fn mask(len: u8) -> u32 {
        if len == 0 {
            0
        } else {
            u32::MAX << (32 - len)
        }
    }

    /// How many addresses the prefix holds, saturating — a `/0` does not fit in
    /// a `u32` and nothing here ever wants one.
    pub fn size(&self) -> u32 {
        1u32.checked_shl(32 - self.len as u32).unwrap_or(u32::MAX)
    }

    /// The gateway: the FIRST usable address — `None` when the prefix has none.
    ///
    /// **It returns an `Option` because the honest answer is sometimes «there
    /// isn't one», and the first version of this could not say that.** It was
    /// `base + 1` unconditionally, and that is wrong twice:
    ///
    /// * on a `/32` (one address) and a `/31` (RFC 3021 point-to-point, two
    ///   addresses and no room for a gateway), `base + 1` lands OUTSIDE the
    ///   prefix or on the peer. Measured: `10.0.0.0/32` answered `10.0.0.1`;
    /// * on `255.255.255.255/32` it **overflowed** — a panic in debug, and in
    ///   release a silent wrap to `0.0.0.0`, which is the dangerous half: a
    ///   network whose gateway is the null address, configured without a word.
    ///
    /// `checked_add` and the length guard together make both impossible. A
    /// caller that needs a gateway has to handle the `None`, which is the point.
    pub fn gateway(&self) -> Option<String> {
        // A gateway needs the network address, itself, and at least one host —
        // so /31 and /32 are excluded by arithmetic, not by taste.
        if self.len >= 31 {
            return None;
        }
        self.base.checked_add(1).map(Self::fmt_u32)
    }

    /// The last address of the prefix (the broadcast, on a /24 and wider).
    pub fn last(&self) -> u32 {
        self.base | !Self::mask(self.len)
    }

    /// Is this prefix usable as a WORKLOAD network here?
    ///
    /// Separate from `parse` on purpose: parsing answers «is this a prefix»,
    /// this answers «can a network live in it», and conflating the two would
    /// make the type unusable for reading a peer's address or a route.
    ///
    /// The floor is /28 (16 addresses: network, gateway, 13 hosts, broadcast) —
    /// below that a network cannot hold enough workloads to be worth declaring,
    /// and the ceiling is /8 so one network cannot swallow a whole private
    /// range by typo.
    pub fn usable_for_network(&self) -> std::result::Result<(), String> {
        if !(8..=28).contains(&self.len) {
            return Err(format!(
                "prefix /{} is outside the usable range /8–/28 (a /29 or smaller has no room for \
                 a gateway plus hosts; wider than /8 swallows a whole private range)",
                self.len
            ));
        }
        Ok(())
    }

    pub fn contains(&self, ip: u32) -> bool {
        ip & Self::mask(self.len) == self.base
    }

    /// Do the two prefixes share any address? The question `network create` has
    /// to answer before handing out a second network that overlaps the first.
    pub fn overlaps(&self, other: &Cidr) -> bool {
        let shorter = self.len.min(other.len);
        self.base & Self::mask(shorter) == other.base & Self::mask(shorter)
    }

    pub fn to_string_cidr(&self) -> String {
        format!("{}/{}", Self::fmt_u32(self.base), self.len)
    }

    pub(crate) fn fmt_u32(v: u32) -> String {
        let b = v.to_be_bytes();
        format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
    }

    /// Parses a dotted address into a `u32`.
    pub fn parse_addr(ip: &str) -> Option<u32> {
        let p: Vec<&str> = ip.trim().split('.').collect();
        if p.len() != 4 {
            return None;
        }
        let mut o = [0u8; 4];
        for (i, x) in p.iter().enumerate() {
            o[i] = x.parse::<u8>().ok()?;
        }
        Some(u32::from_be_bytes(o))
    }
}

/// **Preferred** IP (deterministic, pure) in an arbitrary `/16` (`<prefix>.A.B`),
/// derived from the id. It's just the starting point: on its own it collides by the birthday
/// paradox at ~300 containers (32 bits of the id → 16 bits of host). Real uniqueness comes from
/// the lease registry + probing in [`ipam::allocate`]; see [`alloc_ip_in`].
pub fn derive_ip_in(prefix: &str, id: &str) -> String {
    let hex = &id[..id.len().min(8)];
    let n = u32::from_str_radix(hex, 16).unwrap_or(2);
    // O caminho do /16 fica BYTE A BYTE como estava, e isso é deliberado: toda a
    // rede que existe hoje tem leases derivados desta fórmula exacta, e a
    // generalização não tem nada a ganhar em mexer na base instalada. A fórmula
    // geral abaixo NÃO é equivalente a esta (o clamp do último octeto não é o
    // mesmo que o resto da divisão sobre o espaço todo) — por isso as duas
    // coexistem em vez de uma fingir ser a outra.
    if let Some(net) = Cidr::parse(prefix) {
        if net.len != 16 {
            return derive_ip_general(&net, n);
        }
    }
    let a = ((n >> 8) & 0xff) as u8;
    let mut b = (n & 0xff) as u8;
    if b < 2 {
        b = 2;
    }
    if b == 255 {
        b = 254;
    }
    format!("{prefix}.{a}.{b}")
}

/// O preferido para um prefixo de tamanho arbitrário.
///
/// Salta a rede e o gateway (os dois primeiros) e o broadcast (o último), e
/// distribui o resto pelo espaço utilizável. Num prefixo pequeno de mais para
/// ter hosts devolve o próprio gateway — o `valid_ip_in_subnet` recusa-o a
/// seguir e o `probe_free` também não encontra nada, que é a resposta correcta:
/// não há endereço para dar, e é o `allocate` que o diz com um erro.
fn derive_ip_general(net: &Cidr, n: u32) -> String {
    let uteis = net.size().saturating_sub(3); // rede + gateway + broadcast
    if uteis == 0 {
        return Cidr::fmt_u32(net.base.wrapping_add(1));
    }
    let off = n % uteis;
    Cidr::fmt_u32(net.base + 2 + off)
}

/// IP of a container in `prefix`'s `/16`, to **recompute** the IP from the
/// id (cleanup: detach/publish/firewall/egress). Looks up the persisted lease
/// first (the REAL IP assigned at attach, which may have been probed on top of
/// a collision) and only falls back to the hash-derived IP if there is no lease (container
/// pre-registry or not yet attached). **Does not create** a lease — the allocator is
/// [`ipam::allocate`], called at the attach points.
pub fn alloc_ip_in(prefix: &str, id: &str) -> String {
    ipam::lookup(prefix, id).unwrap_or_else(|| derive_ip_in(prefix, id))
}

pub fn alloc_ip(id: &str) -> String {
    alloc_ip_in(&default_prefix(), id)
}

/// Converts an IPv4 `a.b.c.d` into a `u32`.
fn ipv4_to_u32(ip: &str) -> Option<u32> {
    let o: Vec<u8> = ip.split('.').filter_map(|p| p.parse().ok()).collect();
    if o.len() != 4 {
        return None;
    }
    Some(((o[0] as u32) << 24) | ((o[1] as u32) << 16) | ((o[2] as u32) << 8) | o[3] as u32)
}

fn u32_to_ipv4(n: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        (n >> 24) & 0xff,
        (n >> 16) & 0xff,
        (n >> 8) & 0xff,
        n & 0xff
    )
}

/// Allocates a deterministic IP within a CIDR subnet (e.g.: `192.168.1.0/24`),
/// derived from the `id`. Avoids the network address, the broadcast and the `.1` (typical
/// gateway). Used by the `macvlan`/`ipvlan` drivers, whose subnet is the physical LAN.
/// Returns `None` if the subnet is invalid or there aren't enough hosts.
pub fn alloc_ip_cidr(subnet: &str, id: &str) -> Option<String> {
    let (base, plen) = subnet.split_once('/')?;
    let plen: u32 = plen.parse().ok()?;
    if plen >= 31 {
        return None;
    }
    let net = ipv4_to_u32(base)? & (u32::MAX << (32 - plen));
    let host_bits = 32 - plen;
    let size = 1u32 << host_bits; // total addresses
                                  // Usable hosts: [2 .. size-2] (skips network, .1=gateway and broadcast).
    let usable = size.saturating_sub(3);
    if usable == 0 {
        return None;
    }
    let hex = &id[..id.len().min(8)];
    let n = u32::from_str_radix(hex, 16).unwrap_or(2);
    let offset = 2 + (n % usable);
    Some(u32_to_ipv4(net + offset))
}

/// 32-bit FNV-1a hash (to derive a network's subnet/bridge from its name).
fn fnv32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for byte in s.bytes() {
        h ^= byte as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// The bridge device name of a user network — **the single formula**, shared by
/// the two stores that talk about the same device (the same
/// generator-and-reader-share-the-format discipline as `fw_rule_tail`/
/// `antispoof_rule_args`).
///
/// The authority is the physical plane: `infra::network_create{,_with}` writes
/// this into the `NetDef` and it is that name the holder actually creates the
/// link with (`netadd`/`link_exists`/`resolve_net`). The declarative
/// `NetworkStore` only ever REPORTS it (`network ls`/`inspect`/`describe`), and
/// it derives it here rather than recomputing — it had its own formula
/// (`dlxn{base:02x}{hash:04x}`) and printed a device that does not exist on the
/// host, in the very column an operator reads to go and debug the device.
///
/// Note this deliberately does NOT depend on the base octet: the name alone is
/// unique in both stores, and adding the base is what made the two diverge.
/// 12 chars, comfortably inside IFNAMSIZ (15).
pub(crate) fn bridge_name(name: &str) -> String {
    format!("dlxn{:08x}", fnv32(name))
}

/// The name of the default network (the `delonix0` bridge, `docker0` style).
pub const DEFAULT_NET: &str = "bridge";

/// A Delonix network: the default bridge (`delonix0`/`10.88.0.0/16`) or a
/// **user-defined network** (its own bridge + subnet, isolated from the
/// others). Everything is deterministic from the name + base octet.
#[derive(Clone, Debug)]
pub struct Network {
    pub name: String,
    pub bridge: String,
    pub gateway: String,
    pub prefix: String, // e.g.: "10.88"
    pub subnet: String, // e.g.: "10.88.0.0/16"
    /// Driver: `"bridge"` (default), `"macvlan"` or `"ipvlan"`. The latter two
    /// put the container directly on the physical LAN (its own interface, no veth).
    pub driver: String,
    /// Host parent NIC (only `macvlan`/`ipvlan`): the physical interface on which
    /// the container's sub-interface is created (e.g.: `eno1`).
    pub parent: Option<String>,
    /// VXLAN Network Identifier (only `overlay`): the L2 segment shared between nodes.
    pub vni: Option<u32>,
    /// Peer node IPs (only `overlay`): each entry is `<node_ip>` (flat VXLAN)
    /// OR `<node_ip>=<wg_pubkey>=<wg_ip>` (ENCRYPTED overlay via a WireGuard tunnel).
    pub peers: Vec<String>,
    /// THIS node's WireGuard tunnel IP (only encrypted overlay, req #6). Present ⇒
    /// `ensure_overlay_wg` brings up the wg and the VXLAN FDB uses the peers' `wg_ip`,
    /// encrypting the transport.
    pub wg_ip: Option<String>,
    /// Free labels (k8s style) — short, identifying. The declarative reconciler
    /// stamps ownership here (`delonix.io/stack=<name>`), which is what makes
    /// `stack destroy`/`--prune` possible without a registry of stacks that would
    /// drift out of sync.
    ///
    /// Persisted as `label.<key>=<value>` lines in the record. A record written
    /// by an older binary simply has none, and an older binary reading a record
    /// with them ignores the unknown keys — this file format has always parsed
    /// `key=value` into a map and only read the keys it knows.
    pub labels: std::collections::BTreeMap<String, String>,
    /// Free annotations — same idea, for data that is NOT identifying and may be
    /// large (the reconciler's `delonix.io/last-applied` lives here, never in
    /// `labels`). Persisted as `annotation.<key>=<value>`.
    ///
    /// The value must not contain a literal newline (it would split the record
    /// into two lines); `set_metadata` rejects one rather than writing a record
    /// that reads back as something else. Compact JSON is safe — `serde_json`
    /// escapes newlines inside strings as `\n`, two characters.
    pub annotations: std::collections::BTreeMap<String, String>,
}

/// `bridge` driver (the default case of a user network/`delonix0`).
pub const DRIVER_BRIDGE: &str = "bridge";
/// `macvlan` driver — each container gets its own MAC on the `parent`'s LAN.
pub const DRIVER_MACVLAN: &str = "macvlan";
/// `ipvlan` driver — like macvlan but shares the `parent`'s MAC (L2 mode).
pub const DRIVER_IPVLAN: &str = "ipvlan";
/// `overlay` driver — bridge with a VXLAN uplink: L2 shared across several nodes.
pub const DRIVER_OVERLAY: &str = "overlay";
/// VXLAN UDP port (the IANA-registered one, same as Docker/Linux).
pub const VXLAN_PORT: &str = "4789";

impl Network {
    /// `true` if the driver puts the container on the physical LAN (no bridge/veth).
    pub fn is_lan_driver(&self) -> bool {
        self.driver == DRIVER_MACVLAN || self.driver == DRIVER_IPVLAN
    }
    /// Name of this overlay network's VXLAN device (e.g.: `dlxvx0042`).
    pub fn vxlan_dev(&self) -> Option<String> {
        self.vni.map(|v| format!("dlxvx{v:04x}"))
    }
}

impl Network {
    /// The default network (`delonix0`).
    pub fn default_bridge() -> Self {
        Network {
            name: DEFAULT_NET.to_string(),
            bridge: BRIDGE.to_string(),
            gateway: default_gateway(),
            prefix: default_prefix(),
            subnet: default_subnet(),
            driver: DRIVER_BRIDGE.to_string(),
            parent: None,
            vni: None,
            peers: Vec::new(),
            wg_ip: None,
            labels: std::collections::BTreeMap::new(),
            annotations: std::collections::BTreeMap::new(),
        }
    }

    /// Builds a user network from an ARBITRARY prefix.
    ///
    /// O nome da bridge usa o hash do NOME e não o octeto — que é exactamente a
    /// fórmula autoritativa do plano físico (`dlxn{:08x}` no registo do holder).
    /// Uma rede com CIDR livre não tem octeto para pôr no nome, e usar o hash
    /// fecha de caminho a divergência que este repo já documentou: o
    /// `NetworkStore` a imprimir um dispositivo que não existe no host. As redes
    /// LEGADAS mantêm a fórmula antiga, para o nome delas não mudar debaixo dos
    /// pés de quem já as tem.
    fn user_with_cidr(name: &str, cidr: Cidr) -> Self {
        Network {
            name: name.to_string(),
            bridge: format!("dlxn{:08x}", fnv32(name)),
            gateway: cidr.gateway().unwrap_or_else(|| cidr.to_string_cidr()),
            prefix: cidr.to_string_cidr(),
            subnet: cidr.to_string_cidr(),
            driver: DRIVER_BRIDGE.to_string(),
            parent: None,
            vni: None,
            peers: Vec::new(),
            wg_ip: None,
            labels: std::collections::BTreeMap::new(),
            annotations: std::collections::BTreeMap::new(),
        }
    }

    /// Builds a user network with a given base octet (`10.<base>.0.0/16`).
    /// The bridge name comes from [`bridge_name`] — the SAME formula the physical
    /// plane uses, so what `network ls`/`inspect` print is the device that really
    /// exists in the holder netns.
    fn user_with_base(name: &str, base: u8) -> Self {
        let bridge = bridge_name(name);
        Network {
            name: name.to_string(),
            bridge,
            gateway: format!("10.{base}.0.1"),
            prefix: format!("10.{base}"),
            subnet: format!("10.{base}.0.0/16"),
            driver: DRIVER_BRIDGE.to_string(),
            parent: None,
            vni: None,
            peers: Vec::new(),
            wg_ip: None,
            labels: std::collections::BTreeMap::new(),
            annotations: std::collections::BTreeMap::new(),
        }
    }

    /// Builds an `overlay` network: same as a user bridge (same
    /// `/16`/gateway/veth), but with a VXLAN uplink (`vni`) enslaved to the bridge
    /// and an FDB for the `peers` — the L2 segment extends to several nodes.
    fn overlay_with_base(
        name: &str,
        base: u8,
        vni: u32,
        peers: Vec<String>,
        wg_ip: Option<String>,
    ) -> Self {
        let mut n = Self::user_with_base(name, base);
        n.driver = DRIVER_OVERLAY.to_string();
        n.vni = Some(vni);
        n.peers = peers;
        n.wg_ip = wg_ip;
        n
    }

    /// Builds a `macvlan`/`ipvlan` network from a record: the container
    /// sits on the `parent`'s physical LAN, so subnet/gateway are from the LAN itself
    /// (given by the user, not derived). `prefix` holds the subnet in CIDR.
    fn lan(name: &str, driver: &str, parent: &str, subnet: &str, gateway: &str) -> Self {
        Network {
            name: name.to_string(),
            bridge: parent.to_string(), // for macvlan the "master" is the physical NIC
            gateway: gateway.to_string(),
            prefix: subnet.to_string(), // full CIDR (e.g.: "192.168.1.0/24")
            subnet: subnet.to_string(),
            driver: driver.to_string(),
            parent: Some(parent.to_string()),
            vni: None,
            peers: Vec::new(),
            wg_ip: None,
            labels: std::collections::BTreeMap::new(),
            annotations: std::collections::BTreeMap::new(),
        }
    }

    /// The candidate base octet from the name (range `[100, 239]`, outside of
    /// 88 = default and 90 = service VIPs).
    /// The network's 2nd octet, derived from the name. **It MUST fall within the
    /// ingress workload space** (`10.200.x`–`10.254.x`, see
    /// `delonix_runtime_core::workload_net`): that's where the ingress's DNAT/firewall
    /// accepts publishing ports.
    ///
    /// It was `100 + (fnv32 % 140)` → `10.100.x`–`10.239.x`, and the ingress only
    /// accepts from 200 up: **71% of the network names generated a network where
    /// `-p` failed** with "IP ... outside the ingress space". It was a lottery —
    /// `dlx-delonix` landed on 10.207 (worked) and `dlx-delonix-01` on
    /// 10.173 (blew up). The limits come from the shared constant, not from
    /// numbers repeated by hand: that boundary also underpins the tunnel's
    /// "no-bypass" guard, and duplicating it here was what created the divergence.
    fn base_for(name: &str) -> u8 {
        let lo = delonix_runtime_core::workload_net::WORKLOAD_IPV4_LO.octets()[1];
        let hi = delonix_runtime_core::workload_net::WORKLOAD_IPV4_HI.octets()[1];
        let span = (hi - lo) as u32 + 1;
        lo + (fnv32(name) % span) as u8
    }
}

/// Persistent registry of user networks, at `<root>/networks/<name>`
/// (the file only holds the base octet; the rest is derived from the name). The
/// `bridge` network is implicit (has no file).
pub struct NetworkStore {
    dir: std::path::PathBuf,
}

impl NetworkStore {
    pub fn open(root: impl AsRef<std::path::Path>) -> Result<Self> {
        let dir = root.as_ref().join("networks");
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.join(name)
    }

    /// Resolves a network by name (`bridge`/empty → the default network).
    ///
    /// File format (backward-compatible): a **plain integer** = `bridge` network
    /// with that base octet (old format); or `key=value` lines
    /// (`driver`/`parent`/`subnet`/`gateway`/`base`) for the new drivers.
    pub fn get(&self, name: &str) -> Result<Network> {
        if name.is_empty() || name == DEFAULT_NET {
            return Ok(Network::default_bridge());
        }
        let body = std::fs::read_to_string(self.path(name))
            .map_err(|_| Error::NotFound(format!("network {name}")))?;
        let trimmed = body.trim();
        // Old format: just the base octet → bridge network.
        if let Ok(base) = trimmed.parse::<u8>() {
            return Ok(Network::user_with_base(name, base));
        }
        // New format: key=value.
        let mut kv = std::collections::HashMap::new();
        // `cidr=` é a forma nova e ganha ao `base=`: uma rede escrita com prefixo
        // livre não tem octeto que a descreva.
        //
        // **Guardado, e NÃO devolvido aqui.** Este ramo fazia `return` a meio do
        // ciclo, e o que ficava por correr era a passagem que lê os `label.`/
        // `annotation.` — ou seja, uma rede com `cidr=` voltava SEM POSSE. O
        // efeito é deriva eterna no caso mais comum que existe: o `stack apply`
        // carimba `delonix.io/stack`, o `get` seguinte deita-o fora, o plano diz
        // «exists and belongs to no stack — will be taken over», e o
        // `--detailed-exitcode` responde 2 num manifesto que ninguém tocou. Um
        // gate de deriva em CI fica vermelho todos os dias, que é precisamente
        // aquilo de que o gate serve para avisar.
        let mut cidr = None;
        for line in trimmed.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == "cidr" {
                    if let Some(c) = Cidr::parse(v.trim()) {
                        cidr = Some(c);
                    }
                }
                kv.insert(k.trim(), v.trim().to_string());
            }
        }
        // `label.<k>` / `annotation.<k>` — collected in their own pass because they
        // are the only PREFIXED keys in this format; everything else is a fixed
        // name. Unknown keys keep being ignored, which is what lets an older
        // binary read a record written by this one.
        let mut labels = std::collections::BTreeMap::new();
        let mut annotations = std::collections::BTreeMap::new();
        for (k, v) in &kv {
            if let Some(key) = k.strip_prefix("label.") {
                labels.insert(key.to_string(), v.clone());
            } else if let Some(key) = k.strip_prefix("annotation.") {
                annotations.insert(key.to_string(), v.clone());
            }
        }
        let driver = kv
            .get("driver")
            .map(String::as_str)
            .unwrap_or(DRIVER_BRIDGE);
        // The `cidr` wins over the driver-specific reconstruction, exactly as it
        // did when it returned from inside the loop — what changed is only that
        // the labels below now get applied to it too.
        if let Some(c) = cidr {
            let mut net = Network::user_with_cidr(name, c);
            net.labels = labels;
            net.annotations = annotations;
            return Ok(net);
        }
        let mut net = match driver {
            DRIVER_MACVLAN | DRIVER_IPVLAN => {
                let parent = kv.get("parent").cloned().ok_or_else(|| {
                    Error::Invalid(format!("network '{name}' ({driver}) has no parent"))
                })?;
                let subnet = kv.get("subnet").cloned().ok_or_else(|| {
                    Error::Invalid(format!("network '{name}' ({driver}) has no subnet"))
                })?;
                let gateway = kv.get("gateway").cloned().unwrap_or_default();
                Network::lan(name, driver, &parent, &subnet, &gateway)
            }
            DRIVER_OVERLAY => {
                let base: u8 = kv
                    .get("base")
                    .and_then(|b| b.parse().ok())
                    .ok_or_else(|| Error::Invalid(format!("network '{name}' is corrupted")))?;
                let vni: u32 = kv.get("vni").and_then(|v| v.parse().ok()).ok_or_else(|| {
                    Error::Invalid(format!("network '{name}' (overlay) has no vni"))
                })?;
                let peers: Vec<String> = kv
                    .get("peers")
                    .map(|p| {
                        p.split(',')
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| s.trim().to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                let wg_ip = kv
                    .get("wgip")
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                Network::overlay_with_base(name, base, vni, peers, wg_ip)
            }
            _ => {
                let base: u8 = kv
                    .get("base")
                    .and_then(|b| b.parse().ok())
                    .ok_or_else(|| Error::Invalid(format!("network '{name}' is corrupted")))?;
                Network::user_with_base(name, base)
            }
        };
        net.labels = labels;
        net.annotations = annotations;
        Ok(net)
    }

    /// Lists the user networks (does not include the default `bridge`).
    pub fn list(&self) -> Result<Vec<Network>> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for entry in rd.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if let Ok(n) = self.get(name) {
                        out.push(n);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Creates a user network (free subnet, no collision with existing ones).
    pub fn create(&self, name: &str) -> Result<Network> {
        if name.is_empty() || name == DEFAULT_NET {
            return Err(Error::Invalid(
                "'bridge' is the default network (reserved)".into(),
            ));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(Error::Invalid(format!("invalid network name: '{name}'")));
        }
        if self.path(name).exists() {
            return Err(Error::Conflict(format!("network '{name}' already exists")));
        }
        let used: Vec<u8> = self
            .list()?
            .iter()
            .filter_map(|n| n.prefix.rsplit('.').next().and_then(|o| o.parse().ok()))
            .collect();
        // searches for a free base octet starting from the candidate.
        let mut base = Network::base_for(name);
        for _ in 0..140 {
            if !used.contains(&base) {
                break;
            }
            // Wrap WITHIN the workload space (not 100..239, which fell outside it).
            base = if base >= delonix_runtime_core::workload_net::WORKLOAD_IPV4_HI.octets()[1] {
                delonix_runtime_core::workload_net::WORKLOAD_IPV4_LO.octets()[1]
            } else {
                base + 1
            };
        }
        delonix_runtime_core::write_atomic(&self.path(name), base.to_string().as_bytes())?;
        self.get(name)
    }

    /// The base octet a requested `subnet` maps to — `10.<base>.0.0/16` is the
    /// only shape a bridge network has ever had here, because the on-disk
    /// record holds ONE OCTET and everything else (bridge name, gateway, IPAM
    /// range) is derived from it.
    ///
    /// Anything else is REFUSED, naming what is supported. Until this existed,
    /// `--subnet` and `spec.subnet` were accepted and silently dropped for the
    /// bridge driver: the caller asked for `10.50.0.0/16`, got whatever octet
    /// the store picked from the network's name hash, and was told nothing.
    pub fn base_from_subnet(subnet: &str) -> Result<u8> {
        let lo = delonix_runtime_core::workload_net::WORKLOAD_IPV4_LO.octets()[1];
        let hi = delonix_runtime_core::workload_net::WORKLOAD_IPV4_HI.octets()[1];
        let unsupported = |why: &str| {
            Error::Invalid(format!(
                "subnet '{subnet}': {why}. A bridge network is always \
                 `10.<{lo}-{hi}>.0.0/16` (the record holds one octet, and the \
                 gateway/IPAM are derived from it) — pass one of those, or omit \
                 --subnet to let the engine pick a free one"
            ))
        };
        let (addr, prefix_len) = subnet
            .split_once('/')
            .ok_or_else(|| unsupported("no prefix length"))?;
        if prefix_len.trim() != "16" {
            return Err(unsupported("only /16 is supported"));
        }
        let octets: Vec<&str> = addr.split('.').collect();
        if octets.len() != 4 {
            return Err(unsupported("not an IPv4 network"));
        }
        let parsed: Vec<u8> = octets
            .iter()
            .map(|o| {
                o.parse::<u8>()
                    .map_err(|_| unsupported("not an IPv4 network"))
            })
            .collect::<Result<Vec<u8>>>()?;
        if parsed[0] != 10 {
            return Err(unsupported("outside the workload address space"));
        }
        // A /16 whose host part is not zero is a typo, not an intent — say so
        // rather than quietly rounding it down to the network address.
        if parsed[2] != 0 || parsed[3] != 0 {
            return Err(unsupported("a /16 must end in .0.0"));
        }
        if !(lo..=hi).contains(&parsed[1]) {
            return Err(unsupported("outside the workload address space"));
        }
        Ok(parsed[1])
    }

    /// Valida um `--subnet` ARBITRÁRIO e devolve-o como prefixo.
    ///
    /// Três perguntas, e cada uma recusa por uma razão diferente porque os
    /// enganos são diferentes:
    ///
    /// 1. **é um prefixo?** — o `Cidr::parse` responde;
    /// 2. **é privado?** — só 10/8, 172.16/12 e 192.168/16. Uma rede de
    ///    workloads num espaço público faria este motor entregar endereços de
    ///    outra pessoa aos containers, e o sintoma seria tráfego a desaparecer
    ///    para um destino que existe mesmo na internet;
    /// 3. **cabe lá uma rede?** — o `usable_for_network` (/8–/28).
    ///
    /// A sobreposição com redes existentes é verificada por quem tem a lista, no
    /// `create` — aqui não há acesso ao store, e ir buscá-lo tornaria esta
    /// função impossível de testar sem um.
    pub fn validate_subnet(subnet: &str) -> Result<Cidr> {
        let porque = |why: &str| {
            Error::Invalid(format!(
                "subnet '{subnet}': {why}. Use a private range — `10.0.0.0/8`, \
                 `172.16.0.0/12` or `192.168.0.0/16` — with a prefix between /8 \
                 and /28, or omit --subnet to let the engine pick a free one"
            ))
        };
        let c = Cidr::parse(subnet).ok_or_else(|| porque("not an IPv4 prefix"))?;
        if subnet.split_once('/').is_none() {
            return Err(porque("no prefix length"));
        }
        let privado = [("10.0.0.0/8"), ("172.16.0.0/12"), ("192.168.0.0/16")]
            .iter()
            .filter_map(|r| Cidr::parse(r))
            .any(|r| r.len <= c.len && r.contains(c.base));
        if !privado {
            return Err(porque("outside the private address space (RFC 1918)"));
        }
        c.usable_for_network().map_err(|e| porque(&e))?;
        Ok(c)
    }

    /// Valida um gateway DECLARADO contra o prefixo da rede.
    ///
    /// «Gateway externo» numa SDN rootless quer dizer uma coisa concreta e vale
    /// a pena dizê-la: o holder continua a ser dono da bridge e a encaminhar; o
    /// que muda é a ROTA DEFAULT que os containers recebem, que passa a apontar
    /// para o endereço declarado — tipicamente um appliance de fronteira NESSA
    /// rede. Não é «a rede passa a ser de outro».
    ///
    /// Por isso tem de estar DENTRO do prefixo (um endereço fora não é
    /// alcançável por link, e a rota nem se instala), e não pode ser a rede nem
    /// o broadcast.
    pub fn validate_gateway(cidr: &Cidr, gw: &str) -> Result<()> {
        let porque = |why: &str| {
            Error::Invalid(format!(
                "gateway '{gw}': {why}. It must be an address INSIDE {} that a workload on that \
                 network actually answers on (a firewall/appliance) — the holder keeps owning the \
                 bridge and routing; what changes is the default route the containers get",
                cidr.to_string_cidr()
            ))
        };
        let a = Cidr::parse_addr(gw).ok_or_else(|| porque("not an IPv4 address"))?;
        if !cidr.contains(a) {
            return Err(porque("outside the network's prefix"));
        }
        if a == cidr.base || a == cidr.last() {
            return Err(porque("is the network or broadcast address"));
        }
        Ok(())
    }

    /// Cria uma rede com um prefixo ARBITRÁRIO, recusando sobreposições.
    ///
    /// A sobreposição é verificada AQUI e não no `validate_subnet` porque é a
    /// única pergunta que precisa de saber o que mais existe — e entregar duas
    /// redes que partilham endereços é a falha que não dá erro nenhum: dá dois
    /// containers com o mesmo IP e uma rede que funciona para um deles.
    pub fn create_with_cidr(&self, name: &str, cidr: Cidr) -> Result<Network> {
        if name.is_empty() || name == DEFAULT_NET {
            return Err(Error::Invalid(
                "'bridge' is the default network (reserved)".into(),
            ));
        }
        if let Ok(existente) = self.get(name) {
            return Ok(existente);
        }
        for outra in self.list().unwrap_or_default() {
            if let Some(c) = Cidr::parse(&outra.subnet) {
                if c.overlaps(&cidr) {
                    return Err(Error::Conflict(format!(
                        "subnet {} overlaps network '{}' ({})",
                        cidr.to_string_cidr(),
                        outra.name,
                        outra.subnet
                    )));
                }
            }
        }
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.path(name), format!("cidr={}\n", cidr.to_string_cidr()))?;
        self.get(name)
    }

    /// Creates a user network with an **explicit base octet** (`10.{base}.0.0/16`).
    /// Used to honor a `kind: Network`'s `spec.subnet` and to ALIGN the VMs' (infra)
    /// network plan to this — the `NetworkStore` is the source of truth for the
    /// prefix. Idempotent: if the network already exists, returns it as-is.
    pub fn create_with_base(&self, name: &str, base: u8) -> Result<Network> {
        if name.is_empty() || name == DEFAULT_NET {
            return Err(Error::Invalid(
                "'bridge' is the default network (reserved)".into(),
            ));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(Error::Invalid(format!("invalid network name: '{name}'")));
        }
        if self.path(name).exists() {
            let existing = self.get(name)?;
            // Idempotent for the SAME subnet (a re-`apply` of an unchanged
            // manifest must be a no-op), but a different one is a change this
            // cannot make: the base octet is the network's identity here, and
            // renumbering a live network would strand every workload already
            // addressed on it. Say so instead of returning the old one as if
            // the request had been honoured.
            if existing.prefix != format!("10.{base}") {
                return Err(Error::Invalid(format!(
                    "network '{name}' already exists as {} — a subnet cannot be \
                     changed in place (workloads are addressed on it); remove it \
                     and create it again if that is what you want",
                    existing.subnet
                )));
            }
            return Ok(existing);
        }
        let lo = delonix_runtime_core::workload_net::WORKLOAD_IPV4_LO.octets()[1];
        let hi = delonix_runtime_core::workload_net::WORKLOAD_IPV4_HI.octets()[1];
        if !(lo..=hi).contains(&base) {
            return Err(Error::Invalid(format!(
                "invalid /16 base octet: {base} (workload space is 10.{lo}..10.{hi})"
            )));
        }
        // Two networks on the same /16 would share an IPAM range without either
        // knowing — the second one's allocations would collide with the first's.
        if let Some(clash) = self
            .list()?
            .into_iter()
            .find(|n| n.prefix == format!("10.{base}"))
        {
            return Err(Error::Invalid(format!(
                "10.{base}.0.0/16 is already used by network '{}'",
                clash.name
            )));
        }
        delonix_runtime_core::write_atomic(&self.path(name), base.to_string().as_bytes())?;
        self.get(name)
    }

    /// Merges `labels`/`annotations` into an existing network's record. A key
    /// mapped to `None` is REMOVED — the only way to unstamp a network that left
    /// a stack, so it has to be expressible.
    ///
    /// Rewrites the record LINE BY LINE, preserving every line it does not own —
    /// the same idiom as [`NetworkStore::add_overlay_peer`], and for the same
    /// reason: this format has several independent writers (`create`,
    /// `create_overlay`, `create_lan`) and a full re-serialization here would have
    /// to reproduce all of them correctly forever.
    ///
    /// Upgrades a legacy plain-integer record (`create` still writes that form) to
    /// `base=<n>` on the way, because a bare integer has nowhere to put a key.
    /// `get()` reads both, so the upgrade is invisible.
    pub fn set_metadata(
        &self,
        name: &str,
        labels: &[(String, Option<String>)],
        annotations: &[(String, Option<String>)],
    ) -> Result<Network> {
        if name.is_empty() || name == DEFAULT_NET {
            // The default bridge has no record on disk — there is nothing to stamp,
            // and silently succeeding would make a stack believe it owns it.
            return Err(Error::Invalid(
                "the default 'bridge' network has no record to annotate".into(),
            ));
        }
        for (k, v) in labels.iter().chain(annotations.iter()) {
            if k.is_empty() || k.contains('=') || k.contains('\n') {
                return Err(Error::Invalid(format!("invalid metadata key: {k:?}")));
            }
            // A literal newline would split the record into two lines and the
            // second one would read back as a key of its own. Refuse instead of
            // writing a record that means something else than what was asked.
            if v.as_deref().is_some_and(|v| v.contains('\n')) {
                return Err(Error::Invalid(format!(
                    "metadata value for {k:?} contains a newline"
                )));
            }
        }
        let raw = std::fs::read_to_string(self.path(name))
            .map_err(|_| Error::NotFound(format!("network {name}")))?;
        // Legacy form: the whole record is the base octet.
        let mut out: Vec<String> = if let Ok(base) = raw.trim().parse::<u8>() {
            vec![format!("base={base}")]
        } else {
            raw.lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.to_string())
                .collect()
        };
        let mut upsert = |prefix: &str, pairs: &[(String, Option<String>)]| {
            for (k, v) in pairs {
                let head = format!("{prefix}{k}=");
                out.retain(|l| !l.starts_with(&head));
                if let Some(v) = v {
                    out.push(format!("{head}{v}"));
                }
            }
        };
        upsert("label.", labels);
        upsert("annotation.", annotations);
        delonix_runtime_core::write_atomic(&self.path(name), (out.join("\n") + "\n").as_bytes())?;
        self.get(name)
    }

    /// Free `/16` base octet for the given name (avoids collision with existing ones).
    fn free_base(&self, name: &str) -> Result<u8> {
        let used: Vec<u8> = self
            .list()?
            .iter()
            .filter_map(|n| n.prefix.rsplit('.').next().and_then(|o| o.parse().ok()))
            .collect();
        let mut base = Network::base_for(name);
        for _ in 0..140 {
            if !used.contains(&base) {
                break;
            }
            base = if base >= 239 { 100 } else { base + 1 };
        }
        Ok(base)
    }

    /// Creates an `overlay` network (bridge + VXLAN uplink): same as a user
    /// network (its own `/16`), but extends to several nodes via the `vni` and the
    /// list of `peers` (IPs of the other Delonix nodes). Without peers, it's local but already
    /// ready to join nodes (just recreate it with the same `vni`/`peers` there).
    pub fn create_overlay(
        &self,
        name: &str,
        vni: u32,
        peers: &[String],
        wg_ip: Option<&str>,
    ) -> Result<Network> {
        if name.is_empty() || name == DEFAULT_NET {
            return Err(Error::Invalid(
                "'bridge' is the default network (reserved)".into(),
            ));
        }
        if name == "host" || name == "none" {
            return Err(Error::Invalid(format!("'{name}' is a reserved driver")));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(Error::Invalid(format!("invalid network name: '{name}'")));
        }
        if self.path(name).exists() {
            return Err(Error::Conflict(format!("network '{name}' already exists")));
        }
        if vni == 0 || vni > 0x00ff_ffff {
            return Err(Error::Invalid("invalid VNI (1..16777215)".into()));
        }
        let base = self.free_base(name)?;
        let wgip_line = wg_ip.map(|w| format!("wgip={w}\n")).unwrap_or_default();
        let body = format!(
            "driver=overlay\nbase={base}\nvni={vni}\npeers={}\n{wgip_line}",
            peers.join(",")
        );
        delonix_runtime_core::write_atomic(&self.path(name), body.as_bytes())?;
        self.get(name)
    }

    /// Adds/updates a peer of an existing overlay (idempotent) and returns the
    /// updated network. `peer` = `<node_ip>` or `<node_ip>=<pubkey>=<wg_ip>`. It's the
    /// building block of the gossip/reconciler (#6 phase 4): applying learned peers. Dedup
    /// by `node_ip` (key/wg_ip may have rotated → replaces).
    pub fn add_overlay_peer(&self, name: &str, peer: &str) -> Result<Network> {
        let net = self.get(name)?;
        if net.driver != DRIVER_OVERLAY {
            return Err(Error::Invalid(format!("'{name}' is not an overlay")));
        }
        let (new_ip, _) = parse_overlay_peer(peer);
        if new_ip.is_empty() {
            return Err(Error::Invalid("invalid peer (missing node_ip)".into()));
        }
        let mut peers: Vec<String> = net
            .peers
            .iter()
            .filter(|p| parse_overlay_peer(p).0 != new_ip)
            .cloned()
            .collect();
        peers.push(peer.to_string());
        self.rewrite_peers_line(name, &peers)
    }

    /// **Removes an overlay peer** — the inverse of [`Self::add_overlay_peer`],
    /// and its absence was a real gap: a peer taken out of the manifest stayed
    /// in the registry, and `network::converge` could only print a warning
    /// saying so.
    ///
    /// Matches on the `node_ip` alone, like the add does, so `10.0.0.7` removes
    /// `10.0.0.7=<key>=<wg_ip>` — the caller has the address, not necessarily
    /// the whole triple.
    ///
    /// **Idempotent**: removing a peer that is not there is `Ok`, symmetric with
    /// an add that replaces rather than duplicates. A converge that failed
    /// because the peer was already gone would turn a no-op into an error.
    pub fn remove_overlay_peer(&self, name: &str, peer: &str) -> Result<Network> {
        let net = self.get(name)?;
        if net.driver != DRIVER_OVERLAY {
            return Err(Error::Invalid(format!("'{name}' is not an overlay")));
        }
        let (gone, _) = parse_overlay_peer(peer);
        if gone.is_empty() {
            return Err(Error::Invalid("invalid peer (missing node_ip)".into()));
        }
        let peers: Vec<String> = net
            .peers
            .iter()
            .filter(|p| parse_overlay_peer(p).0 != gone)
            .cloned()
            .collect();
        self.rewrite_peers_line(name, &peers)
    }

    /// Re-persists replacing ONLY the `peers=` line (preserves base/vni/wgip).
    ///
    /// Extracted because add and remove both write it: two writers of the same
    /// line is how the two versions come to disagree — the discipline the
    /// `set_metadata` line-by-line rewrite already follows for the same reason.
    fn rewrite_peers_line(&self, name: &str, peers: &[String]) -> Result<Network> {
        let raw = std::fs::read_to_string(self.path(name)).map_err(|e| Error::Runtime {
            context: "read overlay",
            message: e.to_string(),
        })?;
        let new_line = format!("peers={}", peers.join(","));
        let mut out: Vec<String> = raw
            .lines()
            .map(|l| {
                if l.starts_with("peers=") {
                    new_line.clone()
                } else {
                    l.to_string()
                }
            })
            .collect();
        if !out.iter().any(|l| l.starts_with("peers=")) {
            out.push(new_line);
        }
        delonix_runtime_core::write_atomic(&self.path(name), (out.join("\n") + "\n").as_bytes())?;
        self.get(name)
    }

    /// Creates a `macvlan`/`ipvlan` network: the container sits directly on the
    /// `parent`'s physical LAN (e.g.: `eno1`), with that LAN's `subnet`/`gateway`. Validates
    /// the name, the driver, the parent NIC's existence and the subnet format (CIDR).
    pub fn create_lan(
        &self,
        name: &str,
        driver: &str,
        parent: &str,
        subnet: &str,
        gateway: &str,
    ) -> Result<Network> {
        if name.is_empty() || name == DEFAULT_NET {
            return Err(Error::Invalid(
                "'bridge' is the default network (reserved)".into(),
            ));
        }
        if name == "host" || name == "none" {
            return Err(Error::Invalid(format!("'{name}' is a reserved driver")));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(Error::Invalid(format!("invalid network name: '{name}'")));
        }
        if driver != DRIVER_MACVLAN && driver != DRIVER_IPVLAN {
            return Err(Error::Invalid(format!("unknown driver: '{driver}'")));
        }
        if self.path(name).exists() {
            return Err(Error::Conflict(format!("network '{name}' already exists")));
        }
        if !link_exists(parent) {
            return Err(Error::Invalid(format!(
                "parent NIC '{parent}' does not exist on the host"
            )));
        }
        if alloc_ip_cidr(subnet, "deadbeef").is_none() {
            return Err(Error::Invalid(format!(
                "invalid subnet: '{subnet}' (e.g. 192.168.1.0/24)"
            )));
        }
        // SECURITY WARNING (informed consent): macvlan/ipvlan put the container
        // DIRECTLY on the `parent`'s physical LAN, with its own IP/MAC. Traffic egresses
        // through the physical NIC BELOW the host's forward chain → it is NOT filterable by
        // Delonix's nft: NO per-container firewall, NO anti-spoof, NO inter-network isolation.
        // It's the nature of macvlan, not a bug — but the operator has to know it. For
        // FILTERED isolation, use a `bridge` network (default). See `is_lan_driver`.
        tracing::warn!(
            network = %name,
            driver = %driver,
            parent = %parent,
            "SECURITY WARNING — this network is UNFILTERED: containers sit directly on the \
             physical LAN of '{parent}', OUTSIDE Delonix's firewall, anti-spoof and isolation. \
             Use a `bridge` network if you need filtering."
        );
        let body =
            format!("driver={driver}\nparent={parent}\nsubnet={subnet}\ngateway={gateway}\n");
        delonix_runtime_core::write_atomic(&self.path(name), body.as_bytes())?;
        self.get(name)
    }

    /// Removes a network's record (does not touch the nft/bridge infrastructure).
    pub fn remove(&self, name: &str) -> Result<Network> {
        let net = self.get(name)?;
        std::fs::remove_file(self.path(name))
            .map_err(|_| Error::NotFound(format!("network {name}")))?;
        Ok(net)
    }
}

/// CANONICAL types of the per-container L4 firewall, defined in `delonix-core`
/// (where they are also persisted in the `Container` record). Re-exported here so that
/// `apply_container_firewall` and the API keep using `delonix_net::ContainerFw`.
pub use delonix_runtime_core::{ContainerFw, FwRule};

/// Default slirp4netns IP/gateway/DNS (rootless network).
pub const SLIRP_IP: &str = "10.0.2.100";
pub const SLIRP_DNS: &str = "10.0.2.3";
/// The slirp's own gateway address — and the source address a published port sees for
/// clients coming from the host's own **loopback**, and only those.
///
/// Measured, not assumed (three clients, one container, nginx access log): a client on
/// `127.0.0.1` arrives as this address, while a client on the host's LAN address or on
/// a libvirt gateway arrives as ITSELF. libslirp cannot use a loopback address as a
/// source inside the emulated network — there is no route back to it — so it
/// substitutes the gateway; every routable source is carried through unchanged.
///
/// So source-based filtering DOES work on published ports (`ingress allow <c> <port>
/// --from <cidr>`, validated end to end: the allowed source gets 200, another source
/// gets nothing), and the per-source rate limit (`infra::do_l4guard`, keyed on
/// `ip saddr`) buckets real clients apart. The single exception is the loopback client,
/// which no source rule written for a real address will match — worth knowing when
/// testing a rule with `curl localhost`, since it fails for a reason that has nothing
/// to do with the rule.
pub const SLIRP_GW: &str = "10.0.2.2";

/// Attaches a **rootless** network to the container via `slirp4netns`: creates a `tap0` in the
/// container's netns (by PID) with NAT in *userspace* — **without root**. Waits for the
/// ready signal (`--ready-fd`) before returning; the slirp process follows the
/// container's life (exits when the netns disappears). (A13.)
/// Path of a container's OWN slirp api-socket (slirp-per-container
/// path, no custom network), by its init's PID.
///
/// **The naming convention lives only here.** `container update` needs this
/// path to publish/unpublish ports hot, and duplicating the `format!`
/// on the CLI side would make the two halves silently diverge the day
/// this changed.
pub fn slirp_container_sock(pid: i32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("delonix-slirp-{pid}.sock"))
}

pub fn slirp_attach(pid: i32, publish: &[String]) -> Result<()> {
    // If there are ports to publish, we open the slirp api-socket to ask it for the
    // *host-forwards* (port publishing WITHOUT root, like rootless Podman).
    let api_sock = if publish.is_empty() {
        None
    } else {
        Some(slirp_container_sock(pid))
    };
    let mut fds = [0i32; 2];
    // SAFETY: pipe() fills 2 fds.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(Error::Runtime {
            context: "pipe",
            message: "slirp ready-fd".into(),
        });
    }
    let (rd, wr) = (fds[0], fds[1]);
    let mut args = vec![
        "--configure".to_string(),
        "--mtu=65520".to_string(),
        "--disable-host-loopback".to_string(),
        format!("--ready-fd={wr}"),
    ];
    if let Some(sock) = &api_sock {
        let _ = std::fs::remove_file(sock);
        args.push(format!("--api-socket={}", sock.display()));
    }
    args.push(pid.to_string());
    args.push("tap0".to_string());
    let spawned = Command::new("slirp4netns")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    // SAFETY: the parent closes its write copy; only the slirp keeps it.
    unsafe { libc::close(wr) };
    match spawned {
        Ok(child) => {
            // BUG FOUND: this used to be a BARE blocking `read` with no poll
            // guard at all — if slirp4netns never signals AND never closes
            // the write end (a grandchild inheriting it, created WITHOUT
            // O_CLOEXEC, is enough), the read hangs the calling `run`
            // forever with no log and no exit. Same class of deadlock the
            // sibling `start_slirp` (infra.rs) already guards against with
            // `wait_readable` — applying the identical capped-wait pattern
            // here closes the gap in the per-container attach path too.
            if crate::infra::wait_readable(rd, 10_000) {
                let mut b = [0u8; 1];
                // SAFETY: reads 1 byte from a read-end already confirmed readable.
                unsafe {
                    libc::read(rd, b.as_mut_ptr() as *mut libc::c_void, 1);
                }
            } else {
                tracing::warn!(
                    "slirp4netns did not signal ready within 10s; the container network may not be operational"
                );
            }
            // SAFETY: rd is a valid fd owned by this function either way.
            unsafe {
                libc::close(rd);
            }
            // Publishes the ports via the api-socket (host → container, in userspace).
            if let Some(sock) = &api_sock {
                for spec in publish {
                    if let Ok((addr, hp, cp, proto)) = parse_publish_addr(spec) {
                        if let Err(e) = slirp_add_hostfwd(sock, &hp, &cp, &proto, addr.as_deref()) {
                            std::mem::forget(child);
                            return Err(e);
                        }
                    }
                }
            }
            // The slirp runs for the container's lifetime — we don't wait for it.
            std::mem::forget(child);
            Ok(())
        }
        Err(e) => {
            // SAFETY: closes the read-end on error.
            unsafe { libc::close(rd) };
            Err(Error::Runtime {
                context: "slirp4netns",
                message: e.to_string(),
            })
        }
    }
}

/// **Reaper of orphan slirp4netns** (#1 port-leak): when a container's process
/// exits ON ITS OWN (crash/exit, without `delonix stop`), the `slirp4netns` that
/// served its network may keep running — holding the published host port, which
/// blocks the restart ("add_hostfwd failed"). Scans `/proc` once, identifies
/// the slirp4netns whose **target pid** (last numeric arg of the cmdline) no longer exists and
/// kills them; also removes the obsolete `delonix-slirp-<pid>.sock` api-sockets.
/// Cheap (one pass over /proc) and safe (only touches slirp4netns with a dead target).
/// Returns how many it reaped.
pub fn reap_orphan_slirp() -> usize {
    // Dead target = orphan. `kill(pid, 0)` == 0 ⇒ exists; ESRCH ⇒ dead.
    // SAFETY: kill with signal 0 sends no signal — only tests the pid's existence.
    reap_slirp_where(|target| unsafe { libc::kill(target, 0) } != 0)
}

/// **Kills ONE container's slirp4netns** (the one serving `target_pid`) and waits
/// for it to actually release the host port. Returns `true` if it killed any.
///
/// It exists because of a 100%-reproducible race: `slirp4netns` only exits
/// when it NOTICES the target's netns disappeared, and until then it keeps holding the
/// port published on the host. A `delonix container stop && delonix container
/// start` — the most natural restart idiom there is — always failed with
/// `add_hostfwd: slirp_add_hostfwd failed`, and started working a few seconds
/// later, on its own. `stop` has to release the resources `run` took,
/// synchronously, instead of leaving it to chance.
///
/// Surgical by design: only touches the slirp whose target is EXACTLY this pid.
/// Unlike [`reap_orphan_slirp`], it doesn't depend on the target already being dead
/// — the caller is the one who killed it.
pub fn reap_slirp_for(target_pid: i32) -> bool {
    let n = reap_slirp_where(|target| target == target_pid);
    if n == 0 {
        return false;
    }
    // Short wait until the process actually exits: SIGTERM is asynchronous and without
    // this the next `start` would catch the port still occupied again — which is
    // exactly the bug this code exists to close.
    for _ in 0..50 {
        if !slirp_exists_for(target_pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    true
}

/// Scans `/proc` for `slirp4netns` processes and kills (SIGTERM) those
/// whose target pid satisfies `should_reap`. Returns how many it killed.
///
/// The scan was embedded in `reap_orphan_slirp`; it was extracted so the
/// surgical reaper ([`reap_slirp_for`]) shares exactly the same identification
/// logic — two copies would diverge the day the slirp's argv changed.
fn reap_slirp_where(should_reap: impl Fn(i32) -> bool) -> usize {
    let mut reaped = 0;
    for (pid, target) in list_slirps() {
        if !should_reap(target) {
            continue;
        }
        // SAFETY: SIGTERM to a slirp4netns identified by its argv.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let _ = std::fs::remove_file(slirp_container_sock(target));
        reaped += 1;
    }
    reaped
}

fn slirp_exists_for(target_pid: i32) -> bool {
    list_slirps().into_iter().any(|(_, t)| t == target_pid)
}

/// `(slirp pid, pid of the container it serves)` of each running slirp4netns.
fn list_slirps() -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<i32>() else {
            continue; // not a process directory
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        // cmdline = NUL-separated args. argv[0] has to be slirp4netns.
        let argv: Vec<&[u8]> = cmdline
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .collect();
        if argv.is_empty() || !argv[0].ends_with(b"slirp4netns") {
            continue;
        }
        // the target pid is the second-to-last arg (… <pid> tap0). Finds the last numeric arg.
        let target = argv.iter().rev().find_map(|a| {
            std::str::from_utf8(a)
                .ok()
                .and_then(|s| s.parse::<i32>().ok())
        });
        if let Some(t) = target {
            out.push((pid, t));
        }
    }
    out
}

/// Asks slirp4netns (via the JSON api-socket) for a *host-forward* `host_port` →
/// `guest_port` on the container's IP ([`SLIRP_IP`]). It's how Podman publishes ports
/// in rootless. Retries briefly until the socket exists (the slirp creates it on startup).
pub fn slirp_add_hostfwd(
    sock: &std::path::Path,
    host_port: &str,
    guest_port: &str,
    proto: &str,
    spec_addr: Option<&str>,
) -> Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    // SAFE BY DEFAULT: binds the published port only to the loopback (127.0.0.1), not to
    // all interfaces. Two explicit opt-ins widen it, in this order of precedence: the
    // spec's own `hostIp` (`-p 0.0.0.0:8080:80`, Docker syntax) and the
    // DELONIX_PUBLISH_ADDR env var. Both validated as IPv4 so as not to inject into the
    // slirp api-socket's JSON — see `publish_bind_addr`.
    let host_addr = publish_bind_addr(spec_addr);
    let cmd = format!(
        r#"{{"execute":"add_hostfwd","arguments":{{"proto":"{proto}","host_addr":"{host_addr}","host_port":{host_port},"guest_addr":"{SLIRP_IP}","guest_port":{guest_port}}}}}"#
    );
    let mut last = String::new();
    for _ in 0..50 {
        match UnixStream::connect(sock) {
            Ok(mut s) => {
                // 500ms was too tight for the SINGLE slirp the whole ingress
                // shares, and the discarded read error made the consequence
                // silent AND wrong: a timeout left `resp` empty, an empty string
                // does not contain `"error"`, and the function fell through to
                // `Ok(())` — reporting a publish that may never have happened.
                // Same class as the control socket's 5s ceiling, one step worse:
                // there the symptom was an error with no subject, here it is a
                // false success.
                let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                s.write_all(cmd.as_bytes()).map_err(|e| reg_io(&e))?;
                let mut resp = String::new();
                if let Err(e) = s.read_to_string(&mut resp) {
                    return Err(Error::Runtime {
                        context: "slirp hostfwd",
                        message: format!(
                            "port {host_port}: no reply from the slirp api-socket ({e}) - \
                             the publish may or may not have been applied; check with \
                             `delonix net ingress ls`"
                        ),
                    });
                }
                if resp.contains("\"error\"") {
                    // The slirp answers with an opaque `add_hostfwd failed` for every
                    // cause. The overwhelmingly common one in rootless is a port below
                    // 1024, so name it here instead of leaving raw JSON as the only
                    // clue — the callers that don't preflight (ingress, compose,
                    // `container update --publish-add`, the docker API) all land here.
                    let hint = match host_port.parse::<u16>() {
                        Ok(p) if !can_bind_host_port(&host_addr, p) => format!(
                            " — binding port {p} on the host needs privilege \
                             (rootless cannot publish below \
                             net.ipv4.ip_unprivileged_port_start); publish on a higher \
                             port instead, e.g. -p 8080:{guest_port}"
                        ),
                        _ => String::new(),
                    };
                    return Err(Error::Runtime {
                        context: "slirp hostfwd",
                        message: format!("port {host_port}: {}{hint}", resp.trim()),
                    });
                }
                return Ok(()); // {"return":{}} = success
            }
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        }
    }
    Err(Error::Runtime {
        context: "slirp api-socket",
        message: last,
    })
}

fn reg_io(e: &std::io::Error) -> Error {
    Error::Runtime {
        context: "slirp hostfwd",
        message: e.to_string(),
    }
}

/// An active network connection relevant to a container, from `conntrack`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Connection {
    /// `external_in` (someone outside → container), `egress` (container → outside),
    /// `internal` (container ↔ container).
    pub kind: String,
    /// Name of the container involved (the destination in `external_in`/`internal`-from;
    /// the source in `egress`).
    pub container: String,
    /// The other end: external IP (`external_in`/`egress`) or container (`internal`).
    pub peer: String,
    pub port: String,
    pub proto: String,
}

/// Reads the ACTIVE connections via `conntrack -L` (netlink) and classifies those
/// that involve containers (`ip2name`: container IP → name). It's the basis of the
/// **engine**'s security monitor — only the host (global netns, root) sees this; each
/// container, in its own netns and without `CAP_NET_ADMIN`, sees only its
/// own connections, never another's. Best-effort: without `conntrack`, empty.
pub fn list_connections(ip2name: &std::collections::HashMap<String, String>) -> Vec<Connection> {
    if ip2name.is_empty() {
        return vec![];
    }
    let text = match Command::new("conntrack").arg("-L").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => return vec![],
    };
    let is_cont = |ip: &str| ip2name.contains_key(ip);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let proto = line.split_whitespace().next().unwrap_or("tcp").to_string();
        let mut src = vec![];
        let mut dst = vec![];
        let mut dport = vec![];
        for tok in line.split_whitespace() {
            if let Some(v) = tok.strip_prefix("src=") {
                src.push(v);
            } else if let Some(v) = tok.strip_prefix("dst=") {
                dst.push(v);
            } else if let Some(v) = tok.strip_prefix("dport=") {
                dport.push(v);
            }
        }
        if src.len() < 2 || dst.is_empty() {
            continue;
        }
        let (o_src, o_dst, r_src) = (src[0], dst[0], src[1]);
        let port = dport.first().copied().unwrap_or("").to_string();
        if is_cont(r_src) && !is_cont(o_src) && o_src != "127.0.0.1" {
            let c = ip2name[r_src].clone();
            if seen.insert(format!("in:{o_src}:{c}:{port}")) {
                out.push(Connection {
                    kind: "external_in".into(),
                    container: c,
                    peer: o_src.into(),
                    port,
                    proto,
                });
            }
        } else if is_cont(o_src) && !is_cont(o_dst) && o_dst != "127.0.0.1" {
            let c = ip2name[o_src].clone();
            if seen.insert(format!("out:{c}:{o_dst}")) {
                out.push(Connection {
                    kind: "egress".into(),
                    container: c,
                    peer: o_dst.into(),
                    port,
                    proto,
                });
            }
        } else if is_cont(o_src) && is_cont(o_dst) {
            let (a, b) = (ip2name[o_src].clone(), ip2name[o_dst].clone());
            if seen.insert(format!("int:{a}:{b}")) {
                out.push(Connection {
                    kind: "internal".into(),
                    container: a,
                    peer: b,
                    port,
                    proto,
                });
            }
        }
    }
    out.truncate(200);
    out
}

#[cfg(test)]
mod tests {

    /// **A base instalada não pode mexer-se.** Toda a rede que existe hoje tem
    /// leases derivados da fórmula do /16, e a generalização não tem nada a
    /// ganhar em mudá-la. Este teste fixa a fórmula ANTIGA, valor a valor, para
    /// que uma generalização futura não a "arrume" sem dar por isso.
    #[test]
    fn o_16_continua_byte_a_byte_como_estava() {
        // ids escolhidos para tocar os três ramos do clamp: b<2, b normal, b=255.
        for (id, esperado) in [
            ("00000000", "10.210.0.2"),   // b=0 -> 2
            ("00000001", "10.210.0.2"),   // b=1 -> 2
            ("0000010f", "10.210.1.15"),  // normal
            ("000000ff", "10.210.0.254"), // b=255 -> 254
            ("abcdef12", "10.210.239.18"),
        ] {
            assert_eq!(derive_ip_in("10.210", id), esperado, "id={id}");
        }
    }

    /// E os prefixos que não são /16 passam a funcionar — com as MESMAS
    /// exclusões (rede, gateway, broadcast), que é o que torna a regra geral a
    /// mesma regra e não outra.
    #[test]
    fn um_prefixo_arbitrario_aloca_dentro_de_si_e_respeita_as_exclusoes() {
        for cidr in ["172.20.4.0/22", "192.168.1.0/28", "10.0.0.0/8"] {
            let net = Cidr::parse(cidr).unwrap();
            for id in ["00000000", "deadbeef", "0000000a", "ffffffff"] {
                let ip = derive_ip_in(cidr, id);
                let a = Cidr::parse_addr(&ip).unwrap_or_else(|| panic!("{ip} não parseia"));
                assert!(net.contains(a), "{cidr}: {ip} fora do prefixo");
                assert_ne!(a, net.base, "{cidr}: {ip} é o endereço de rede");
                assert_ne!(a, net.base + 1, "{cidr}: {ip} é o gateway");
                assert_ne!(a, net.last(), "{cidr}: {ip} é o broadcast");
                assert!(valid_ip_in_subnet(cidr, &ip), "{cidr}: {ip} recusado");
            }
        }
    }

    /// As exclusões continuam a ser as três, e nem uma a mais — um teste que só
    /// verificasse «está dentro» deixaria passar o gateway a ser entregue a um
    /// container, que é uma colisão que só aparece quando a rede deixa de
    /// funcionar para todos.
    #[test]
    fn a_rede_o_gateway_e_o_broadcast_sao_os_unicos_excluidos() {
        let cidr = "192.168.1.0/28"; // 16 endereços: fácil de enumerar por inteiro
        let net = Cidr::parse(cidr).unwrap();
        let mut aceites = 0;
        for a in net.base..=net.last() {
            let ip = Cidr::fmt_u32(a);
            let ok = valid_ip_in_subnet(cidr, &ip);
            let deve = a != net.base && a != net.base + 1 && a != net.last();
            assert_eq!(ok, deve, "{ip}");
            aceites += ok as u32;
        }
        assert_eq!(aceites, 13, "um /28 tem 16 endereços menos os 3 reservados");
        // E um endereço de FORA nunca é aceite, por muito parecido que seja.
        assert!(!valid_ip_in_subnet(cidr, "192.168.2.5"));
        assert!(!valid_ip_in_subnet(cidr, "192.168.1.16"));
    }

    /// **Exaustivo sobre TODOS os prefixos**, que é a única forma de garantir que
    /// a aritmética de máscara não tem um buraco algures no meio: um erro de
    /// shift não aparece no /16 que toda a gente testa, aparece no /31.
    ///
    /// Cinco invariantes que um prefixo tem SEMPRE de respeitar:
    #[test]
    fn invariantes_de_todos_os_prefixos_de_0_a_32() {
        for len in 0u8..=32 {
            for base_s in [
                "0.0.0.0",
                "10.0.0.0",
                "172.16.0.0",
                "192.168.1.0",
                "255.255.255.255",
            ] {
                let c = Cidr::parse(&format!("{base_s}/{len}")).expect("prefixo válido");

                // 1. contém a sua própria base e o seu próprio último endereço.
                assert!(c.contains(c.base), "{c:?} não contém a base");
                assert!(c.contains(c.last()), "{c:?} não contém o último");

                // 2. o último nunca é menor que a base.
                assert!(c.last() >= c.base, "{c:?} last < base");

                // 3. sobrepõe-se consigo próprio, sempre (reflexivo).
                assert!(c.overlaps(&c), "{c:?} não se sobrepõe a si mesmo");

                // 4. o gateway, quando existe, está DENTRO do prefixo — foi
                //    exactamente isto que falhou no /31 e no /32.
                if let Some(gw) = c.gateway() {
                    let g = Cidr::parse_addr(&gw).expect("gateway parseável");
                    assert!(c.contains(g), "{c:?} gateway {gw} fora do prefixo");
                    assert!(c.len <= 30, "{c:?} não devia ter gateway");
                }

                // 5. round-trip: o texto que imprime volta a dar o mesmo prefixo.
                let volta = Cidr::parse(&c.to_string_cidr()).expect("round-trip");
                assert_eq!(volta, c, "round-trip mudou {c:?}");
            }
        }
    }

    /// O caso que rebentou a implementação anterior, isolado para não voltar:
    /// no topo do espaço de endereços, `base + 1` transborda. Em debug era
    /// pânico; em RELEASE era um wrap silencioso para `0.0.0.0` — uma rede com
    /// o endereço nulo por gateway, configurada sem uma palavra.
    #[test]
    fn o_topo_do_espaco_de_enderecos_nao_transborda() {
        let topo = Cidr::parse("255.255.255.255/32").unwrap();
        assert_eq!(topo.gateway(), None, "um /32 não tem gateway");
        assert_eq!(topo.last(), u32::MAX);
        // E um /30 lá no topo, onde ainda há gateway mas o +1 anda perto do fim.
        let quase = Cidr::parse("255.255.255.252/30").unwrap();
        assert_eq!(quase.gateway().as_deref(), Some("255.255.255.253"));
    }

    /// `/31` (RFC 3021, ponto-a-ponto) e `/32` são prefixos VÁLIDOS — o `parse`
    /// tem de os aceitar, porque servem para ler o endereço de um par ou uma
    /// rota — mas não são redes de workloads. As duas perguntas são separadas
    /// de propósito, e juntá-las tornaria o tipo inútil para o resto.
    #[test]
    fn parse_aceita_o_que_nao_serve_como_rede_e_a_validacao_e_que_recusa() {
        for s in ["10.0.0.1/32", "10.0.0.0/31", "10.0.0.0/30", "10.0.0.0/0"] {
            assert!(Cidr::parse(s).is_some(), "parse recusou {s}");
            assert!(
                Cidr::parse(s).unwrap().usable_for_network().is_err(),
                "{s} não devia servir como rede de workloads"
            );
        }
        for s in [
            "10.0.0.0/8",
            "10.210.0.0/16",
            "172.20.4.0/22",
            "192.168.1.0/28",
        ] {
            assert!(
                Cidr::parse(s).unwrap().usable_for_network().is_ok(),
                "{s} devia servir"
            );
        }
    }

    /// Sobreposição, exaustiva num espaço pequeno: um /24 partido em dois /25
    /// não se sobrepõe entre si, mas cada um sobrepõe-se ao pai — e a relação é
    /// simétrica em TODOS os casos. É o erro de fencepost com todas as
    /// combinações, não com a que calhou escrever.
    #[test]
    fn sobreposicao_e_simetrica_e_hierarquica_em_todo_o_espaco() {
        let pai = Cidr::parse("192.168.0.0/24").unwrap();
        let mut filhos = Vec::new();
        for i in (0..256).step_by(16) {
            filhos.push(Cidr::parse(&format!("192.168.0.{i}/28")).unwrap());
        }
        for (i, a) in filhos.iter().enumerate() {
            assert!(pai.overlaps(a) && a.overlaps(&pai), "{a:?} vs pai");
            for (j, b) in filhos.iter().enumerate() {
                let esperado = i == j;
                assert_eq!(a.overlaps(b), esperado, "{a:?} vs {b:?}");
                assert_eq!(a.overlaps(b), b.overlaps(a), "assimétrico: {a:?} {b:?}");
            }
        }
        // E um vizinho de outro espaço nunca se sobrepõe.
        let outro = Cidr::parse("192.168.1.0/24").unwrap();
        assert!(!pai.overlaps(&outro) && !outro.overlaps(&pai));
    }

    /// A forma LEGADA continua a querer dizer o que sempre quis.
    ///
    /// Todo o registo escrito antes disto guarda um octeto (`210`) ou dois
    /// (`10.210`), e significava `10.210.0.0/16`. Se esta leitura mudasse, cada
    /// rede existente passava a apontar para outro espaço de endereços — a pior
    /// migração possível, e silenciosa.
    #[test]
    fn a_forma_legada_de_dois_octetos_e_um_slash_16() {
        let legado = Cidr::parse("10.210").expect("dois octetos");
        assert_eq!(legado.len, 16);
        assert_eq!(legado.to_string_cidr(), "10.210.0.0/16");
        assert_eq!(legado, Cidr::parse("10.210.0.0/16").unwrap());
        // E o gateway continua a ser o `.0.1` que sempre foi.
        assert_eq!(legado.gateway().as_deref(), Some("10.210.0.1"));
    }

    /// Os bits de host são LIMPOS, não recusados: `10.210.5.7/16` é como se
    /// escreve «o /16 onde este endereço está», e recusá-lo era pedantismo.
    #[test]
    fn os_bits_de_host_sao_limpos_em_vez_de_recusados() {
        assert_eq!(
            Cidr::parse("10.210.5.7/16").unwrap().to_string_cidr(),
            "10.210.0.0/16"
        );
        assert_eq!(
            Cidr::parse("192.168.1.130/25").unwrap().to_string_cidr(),
            "192.168.1.128/25"
        );
    }

    /// A pergunta que o `network create` tem de responder antes de entregar uma
    /// segunda rede: partilham algum endereço?
    ///
    /// O caso que engana é o de prefixos de tamanho DIFERENTE — um `/24` dentro
    /// de um `/16` não tem a mesma base e sobrepõe-se na mesma. Comparar as
    /// bases só funciona depois de as mascarar pelo prefixo mais CURTO.
    #[test]
    fn sobreposicao_apanha_um_prefixo_dentro_de_outro() {
        let dezasseis = Cidr::parse("10.210.0.0/16").unwrap();
        let dentro = Cidr::parse("10.210.7.0/24").unwrap();
        assert!(dezasseis.overlaps(&dentro), "um /24 dentro de um /16");
        assert!(dentro.overlaps(&dezasseis), "e é simétrico");

        let fora = Cidr::parse("10.211.0.0/16").unwrap();
        assert!(!dezasseis.overlaps(&fora));
        // Adjacentes não se sobrepõem, que é o erro de fencepost clássico.
        let a = Cidr::parse("192.168.0.0/25").unwrap();
        let b = Cidr::parse("192.168.0.128/25").unwrap();
        assert!(!a.overlaps(&b), "adjacentes não se sobrepõem");
    }

    #[test]
    fn tamanho_e_pertenca() {
        let c = Cidr::parse("172.20.4.0/22").unwrap();
        assert_eq!(c.size(), 1024);
        assert_eq!(c.gateway().as_deref(), Some("172.20.4.1"));
        assert!(c.contains(Cidr::parse_addr("172.20.5.9").unwrap()));
        assert!(!c.contains(Cidr::parse_addr("172.20.8.1").unwrap()));
        // Um /32 é um endereço só, e não deve rebentar no shift.
        assert_eq!(Cidr::parse("10.0.0.1/32").unwrap().size(), 1);
    }

    #[test]
    fn recusa_o_que_nao_e_um_prefixo() {
        for mau in [
            "",
            "10.210.0.0/33",
            "10.300.0.0/16",
            "abc",
            "10.1.1.1.1/8",
            "/16",
        ] {
            assert!(Cidr::parse(mau).is_none(), "aceitou {mau:?}");
        }
    }

    /// **Uma rede com `cidr=` perdia a posse a cada leitura.** O ramo do `cidr`
    /// devolvia de dentro do ciclo das linhas, antes da passagem que recolhe os
    /// `label.`/`annotation.` — por isso o `stack apply` carimbava
    /// `delonix.io/stack`, o `get` seguinte deitava-o fora, e o plano voltava a
    /// propor a adopção do que acabara de possuir. Deriva ETERNA, no caso mais
    /// comum que há: um `kind: Network` com `subnet:`.
    ///
    /// Medido antes da correcção com a bateria E2E: `stack plan
    /// --detailed-exitcode` respondia **2** sobre um manifesto inalterado — um
    /// gate de deriva em CI vermelho todos os dias.
    #[test]
    fn uma_rede_com_cidr_nao_perde_a_posse() {
        use super::NetworkStore;
        let tmp = std::env::temp_dir().join(format!("dlx-net-cidr-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = NetworkStore::open(&tmp).unwrap();
        let c = super::Cidr::parse("10.251.0.0/16").expect("cidr valido");
        store.create_with_cidr("comcidr", c).unwrap();
        // Pré-condição: o registo é MESMO da forma que perdia os labels.
        let raw = std::fs::read_to_string(tmp.join("networks/comcidr")).unwrap();
        assert!(raw.contains("cidr="), "raw={raw:?}");

        store
            .set_metadata(
                "comcidr",
                &[("delonix.io/stack".into(), Some("loja".into()))],
                &[("delonix.io/last-applied".into(), Some("{\"a\":1}".into()))],
            )
            .unwrap();

        let n = store.get("comcidr").unwrap();
        assert_eq!(
            n.labels.get("delonix.io/stack").map(String::as_str),
            Some("loja"),
            "a posse tem de sobreviver à leitura, senão o plano readopta para sempre"
        );
        assert_eq!(
            n.annotations
                .get("delonix.io/last-applied")
                .map(String::as_str),
            Some("{\"a\":1}")
        );
        // E o `cidr` continua a ganhar: a rede é a que foi pedida, não uma
        // reconstruída a partir de um `base=` que este registo nem tem.
        assert_eq!(n.subnet, "10.251.0.0/16");
        // O `list` é o que o reconciliador usa, e passa pelo MESMO `get`.
        let listed = store.list().unwrap();
        let found = listed
            .iter()
            .find(|x| x.name == "comcidr")
            .expect("listada");
        assert_eq!(
            found.labels.get("delonix.io/stack").map(String::as_str),
            Some("loja")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `create` still writes the LEGACY form (the bare base octet), which has
    /// nowhere to put a key. Stamping ownership has to upgrade it to `base=<n>`
    /// on the way — and the upgrade must be invisible, i.e. the network keeps the
    /// exact same bridge/prefix/subnet it had, because containers are already
    /// attached to them.
    #[test]
    fn set_metadata_actualiza_um_registo_legado_sem_mudar_a_rede() {
        use super::NetworkStore;
        let tmp = std::env::temp_dir().join(format!("dlx-net-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = NetworkStore::open(&tmp).unwrap();
        let before = store.create("interna").unwrap();
        // Precondition: the record really is the legacy bare-integer form.
        let raw = std::fs::read_to_string(tmp.join("networks/interna")).unwrap();
        assert!(raw.trim().parse::<u8>().is_ok(), "raw={raw:?}");

        let after = store
            .set_metadata(
                "interna",
                &[("delonix.io/stack".into(), Some("web".into()))],
                &[(
                    "delonix.io/last-applied".into(),
                    Some("{\"subnet\":1}".into()),
                )],
            )
            .unwrap();
        assert_eq!(after.bridge, before.bridge);
        assert_eq!(after.subnet, before.subnet);
        assert_eq!(after.gateway, before.gateway);
        assert_eq!(after.labels.get("delonix.io/stack").unwrap(), "web");
        assert_eq!(
            after.annotations.get("delonix.io/last-applied").unwrap(),
            "{\"subnet\":1}"
        );
        // Re-reading from disk gives the same thing (the upgrade was persisted).
        let reread = store.get("interna").unwrap();
        assert_eq!(reread.labels, after.labels);
        assert_eq!(reread.subnet, before.subnet);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The record has several independent writers, so `set_metadata` rewrites it
    /// line by line and must not eat the lines it does not own — an overlay that
    /// lost its `vni`/`peers` to an ownership stamp would silently stop reaching
    /// the other nodes.
    /// REGRESSION: a reader must NEVER observe a partially-written network record.
    ///
    /// Every record here was written with a bare `fs::write`, which TRUNCATES the target
    /// and only then fills it — while `ipam.rs`, in this same crate, had been using
    /// `write_atomic` all along. Two failure modes, and the second is the nasty one:
    ///
    /// * a torn multi-line body loses lines, and `get()` ignores missing keys on purpose
    ///   (that is what lets an older binary read a newer record), so the network comes back
    ///   DEGRADED rather than failing;
    /// * a torn base-octet write is worse — `get()` parses a bare number as the old format,
    ///   so `"142"` truncated to `"14"` is still a perfectly valid octet. The network
    ///   silently moves to another /16 and every container on the old prefix goes dark.
    ///
    /// Same technique as `store.rs`'s `save_concorrente_nunca_publica_json_corrompido`:
    /// writers of DIFFERENT sizes so the interleaving is observable. Reverting
    /// `write_atomic` to `fs::write` makes this fail.
    #[test]
    fn escrita_de_rede_nunca_deixa_um_leitor_ver_registo_parcial() {
        let tmp = std::env::temp_dir().join(format!("dlx-net-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let st = NetworkStore::open(&tmp).unwrap();
        st.create_overlay("ov", 42, &[], None).unwrap();

        std::thread::scope(|sc| {
            for i in 0..16 {
                let tmp = tmp.clone();
                sc.spawn(move || {
                    let st = NetworkStore::open(&tmp).unwrap();
                    // Peers of growing length: the body changes size on every write.
                    let peer = format!("10.0.0.{}={}", i + 1, "k".repeat(i * 9));
                    let _ = st.add_overlay_peer("ov", &peer);
                    // Whatever the interleaving, a reader sees a COMPLETE record.
                    let n = st
                        .get("ov")
                        .expect("registo parcial publicado pela escrita");
                    assert_eq!(n.driver, "overlay", "driver perdido num corpo truncado");
                    assert_eq!(n.vni, Some(42), "vni perdido num corpo truncado");
                });
            }
        });

        let n = st.get("ov").expect("estado final tem de ser legivel");
        assert_eq!(n.driver, "overlay");
        assert_eq!(n.vni, Some(42));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn set_metadata_preserva_vni_e_peers_de_um_overlay() {
        use super::NetworkStore;
        let tmp = std::env::temp_dir().join(format!("dlx-net-ovl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = NetworkStore::open(&tmp).unwrap();
        store
            .create_overlay("malha", 42, &["10.0.0.7".to_string()], Some("10.9.0.1"))
            .unwrap();
        let after = store
            .set_metadata(
                "malha",
                &[("delonix.io/stack".into(), Some("infra".into()))],
                &[],
            )
            .unwrap();
        assert_eq!(after.vni, Some(42));
        assert_eq!(after.peers, vec!["10.0.0.7".to_string()]);
        assert_eq!(after.wg_ip.as_deref(), Some("10.9.0.1"));
        assert_eq!(after.driver, super::DRIVER_OVERLAY);
        assert_eq!(after.labels.get("delonix.io/stack").unwrap(), "infra");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// O inverso do `add_overlay_peer`, que durante muito tempo não existiu: um
    /// peer tirado do manifesto ficava no registo e o `converge` só sabia avisar.
    ///
    /// Exige as três coisas que a reescrita LINHA A LINHA existe para garantir —
    /// o `vni`, o `wgIp` e as labels sobrevivem à remoção. Reescrever o ficheiro
    /// inteiro perderia-os, e o sintoma seria um overlay que deixa de saber o seu
    /// próprio VNI depois de perder um par.
    #[test]
    fn remove_overlay_peer_tira_o_peer_e_preserva_vni_wgip_e_labels() {
        use super::NetworkStore;
        let tmp = std::env::temp_dir().join(format!("dlx-net-rmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = NetworkStore::open(&tmp).unwrap();
        store
            .create_overlay(
                "malha",
                42,
                &[
                    "10.0.0.7=chave=10.9.0.7".to_string(),
                    "10.0.0.8".to_string(),
                ],
                Some("10.9.0.1"),
            )
            .unwrap();
        store
            .set_metadata(
                "malha",
                &[("delonix.io/stack".into(), Some("infra".into()))],
                &[],
            )
            .unwrap();

        // Casa pelo `node_ip` SOZINHO — quem remove tem o endereço, não
        // necessariamente o triplo inteiro que foi escrito.
        let after = store.remove_overlay_peer("malha", "10.0.0.7").unwrap();
        assert_eq!(
            after.peers,
            vec!["10.0.0.8".to_string()],
            "{:?}",
            after.peers
        );
        assert_eq!(after.vni, Some(42), "o vni não pode desaparecer");
        assert_eq!(after.wg_ip.as_deref(), Some("10.9.0.1"));
        assert_eq!(after.driver, super::DRIVER_OVERLAY);
        assert_eq!(after.labels.get("delonix.io/stack").unwrap(), "infra");

        // Idempotente: remover o que já não lá está é `Ok`, simétrico com um add
        // que substitui em vez de duplicar. Um erro aqui transformaria um no-op
        // num apply falhado.
        let de_novo = store.remove_overlay_peer("malha", "10.0.0.7").unwrap();
        assert_eq!(de_novo.peers, vec!["10.0.0.8".to_string()]);

        // E o último peer deixa a linha presente e vazia, não ausente.
        let vazio = store.remove_overlay_peer("malha", "10.0.0.8").unwrap();
        assert!(vazio.peers.is_empty(), "{:?}", vazio.peers);
        assert_eq!(vazio.vni, Some(42));

        // Não é overlay → recusa, como o add.
        store.create("simples").unwrap();
        assert!(store.remove_overlay_peer("simples", "10.0.0.7").is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A literal newline in a value would split the record in two and the second
    /// half would read back as a key of its own. Refuse, rather than write a
    /// record that means something different from what was asked. (Compact JSON
    /// is safe — `serde_json` escapes newlines as two characters.)
    #[test]
    fn set_metadata_recusa_um_valor_com_newline() {
        use super::NetworkStore;
        let tmp = std::env::temp_dir().join(format!("dlx-net-nl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = NetworkStore::open(&tmp).unwrap();
        store.create("interna").unwrap();
        assert!(store
            .set_metadata(
                "interna",
                &[("k".into(), Some("linha1\nbase=7".into()))],
                &[]
            )
            .is_err());
        // The refusal is total: nothing was written.
        assert!(store.get("interna").unwrap().labels.is_empty());
        // The default bridge has no record on disk — stamping it would make a
        // stack believe it owns something it cannot own.
        assert!(store.set_metadata("bridge", &[], &[]).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The Docker `[hostIp:]hostPort:contPort` form: the address is what lets a
    /// published port answer anything other than the host's own loopback. Before this,
    /// the ONLY way to widen the bind was the undocumented `DELONIX_PUBLISH_ADDR`, and a
    /// spec carrying an address was rejected outright as an "invalid port".
    #[test]
    fn parse_publish_addr_reads_the_docker_host_ip_form() {
        use super::parse_publish_addr;
        assert_eq!(
            parse_publish_addr("0.0.0.0:8080:80").unwrap(),
            (
                Some("0.0.0.0".into()),
                "8080".into(),
                "80".into(),
                "tcp".into()
            )
        );
        assert_eq!(
            parse_publish_addr("192.168.1.10:8080:80/udp").unwrap(),
            (
                Some("192.168.1.10".into()),
                "8080".into(),
                "80".into(),
                "udp".into()
            )
        );
        // No address = None (the caller falls back to DELONIX_PUBLISH_ADDR/127.0.0.1),
        // and the pre-existing forms keep parsing exactly as they did.
        assert_eq!(
            parse_publish_addr("8080:80").unwrap(),
            (None, "8080".into(), "80".into(), "tcp".into())
        );
        assert_eq!(
            parse_publish_addr("80").unwrap(),
            (None, "80".into(), "80".into(), "tcp".into())
        );
        assert_eq!(
            super::parse_publish("8080:80/udp").unwrap(),
            ("8080".into(), "80".into(), "udp".into())
        );
        // A non-IPv4 head is REJECTED, never silently dropped — the compose bug of
        // discarding `127.0.0.1:9000:80` and publishing on every interface instead was
        // exactly this failure mode.
        assert!(parse_publish_addr("localhost:8080:80").is_err());
        assert!(parse_publish_addr("999.0.0.1:8080:80").is_err());
        assert!(parse_publish_addr("::1:8080:80").is_err());
    }

    /// Ranges expand at the boundary into one spec per port, so nothing downstream has
    /// to learn about ranges. The mismatched-width case is the one that matters: it
    /// must be REFUSED, never silently truncated to the shorter side (which would
    /// publish some ports and quietly skip others).
    #[test]
    fn expand_publish_range_expande_e_recusa_larguras_diferentes() {
        use super::expand_publish_range;
        assert_eq!(
            expand_publish_range("8000-8002:9000-9002").unwrap(),
            vec!["8000:9000", "8001:9001", "8002:9002"]
        );
        // Protocol and host address survive the expansion.
        assert_eq!(
            expand_publish_range("0.0.0.0:8000-8001:80-81/udp").unwrap(),
            vec!["0.0.0.0:8000:80/udp", "0.0.0.0:8001:81/udp"]
        );
        // No range = untouched, so callers can pipe every spec through this.
        assert_eq!(expand_publish_range("8080:80").unwrap(), vec!["8080:80"]);
        // Different widths, a one-sided range, and an inverted range are all refused.
        for bad in ["8000-8010:9000-9002", "8000-8002:80", "8002-8000:9002-9000"] {
            assert!(
                expand_publish_range(bad).is_err(),
                "{bad} should be refused"
            );
        }
        // Out of range must not wrap around into a valid-looking port.
        assert!(expand_publish_range("65534-65536:1-3").is_err());
    }

    /// A range that reaches `parse_publish_addr` unexpanded has to say so — the generic
    /// "invalid port" left the user unable to tell a typo from an unsupported shape.
    #[test]
    fn parse_publish_addr_nomeia_o_range_em_vez_de_porta_invalida() {
        let err = super::parse_publish_addr("8000-8010:80")
            .unwrap_err()
            .to_string();
        assert!(err.to_lowercase().contains("range"), "{err}");
        // Port 0 is not a port, and used to pass the digits-only check.
        assert!(super::parse_publish_addr("0:80").is_err());
        assert!(super::parse_publish_addr("70000:80").is_err());
    }

    /// The bind address is decided in ONE place, with the spec winning over the env var
    /// and `127.0.0.1` as the floor — the two publish datapaths (per-container slirp and
    /// the single ingress slirp) must never diverge on it.
    #[test]
    fn publish_bind_addr_precedence() {
        use super::publish_bind_addr;
        assert_eq!(publish_bind_addr(Some("0.0.0.0")), "0.0.0.0");
        // Without a spec address and without the env var set, the safe default holds.
        if std::env::var_os("DELONIX_PUBLISH_ADDR").is_none() {
            assert_eq!(publish_bind_addr(None), "127.0.0.1");
        }
    }

    /// The permission probe answers about PERMISSION only: an unprivileged port is
    /// always bindable, and a port already TAKEN must still come back `true` — a busy
    /// port has its own diagnosis (which names the owner) and this check must not
    /// shadow it with a "needs privilege" that would be plain wrong.
    #[test]
    fn can_bind_host_port_separa_privilegio_de_porta_ocupada() {
        use super::can_bind_host_port;
        use std::net::TcpListener;
        let held = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let busy = held.local_addr().unwrap().port();
        assert!(can_bind_host_port("127.0.0.1", busy));
        // Unprivileged and free.
        drop(held);
        assert!(can_bind_host_port("127.0.0.1", busy));
        // A privileged port is only refused when we really lack the privilege — as
        // root (or with the sysctl lowered) the answer legitimately flips, so the
        // assertion is conditioned on what the kernel actually allows here.
        let root = unsafe { libc::geteuid() } == 0;
        let low = std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .unwrap_or(1024);
        if !root && low > 80 {
            assert!(!can_bind_host_port("127.0.0.1", 80));
        }
    }

    /// REGRESSION: the prefix of ANY network name has to fall within the ingress
    /// workload space. It was `100 + (fnv32 % 140)` and the ingress only accepts
    /// from 200 up — 71% of the names generated a network where publishing ports
    /// failed ("IP ... outside the ingress space"). A test over real and random
    /// names catches the divergence as soon as it comes back.
    #[test]
    fn prefixo_de_rede_cai_sempre_no_espaco_de_ingress() {
        use delonix_runtime_core::workload_net::is_workload_ipv4;
        let mut nomes: Vec<String> = vec![
            "kind".into(),
            "dlx-delonix".into(),
            "dlx-delonix-01".into(),
            "backend".into(),
            "lab-net".into(),
            "a".into(),
            "".into(),
            "rede-com-nome-muito-comprido-mesmo".into(),
        ];
        // Serious coverage: 500 generated names, not just the ones I remembered.
        nomes.extend((0..500).map(|i| format!("net-{i}")));
        for n in &nomes {
            let base = Network::base_for(n);
            let ip: std::net::Ipv4Addr = format!("10.{base}.1.2").parse().unwrap();
            assert!(
                is_workload_ipv4(ip),
                "a rede '{n}' ficou em 10.{base}.x — fora do espaço de ingress; o `-p` falharia lá"
            );
        }
    }

    use super::*;

    #[test]
    fn overlay_peer_parse() {
        // flat VXLAN (only node_ip)
        assert_eq!(parse_overlay_peer("10.0.0.2"), ("10.0.0.2".into(), None));
        // encrypted: node_ip=pubkey=wg_ip
        let (ip, wg) = parse_overlay_peer("10.0.0.2=AbCdEf0123/+key=10.250.0.2");
        assert_eq!(ip, "10.0.0.2");
        assert_eq!(wg, Some(("AbCdEf0123/+key".into(), "10.250.0.2".into())));
        // REGRESSION: a REAL WireGuard pubkey (base64 44c) ENDS in `=` (padding) —
        // the delimiter collides. The parser has to preserve the padding and a clean wg_ip.
        let real = "VpKM6MYFVDIvcMBxnkBkf7/clXq+itJlPaW71o2iK24=";
        let (ip2, wg2) = parse_overlay_peer(&format!("127.0.0.1={real}=10.250.0.1"));
        assert_eq!(ip2, "127.0.0.1");
        assert_eq!(wg2, Some((real.to_string(), "10.250.0.1".into())));
        // malformed → treats as flat (no wg)
        assert_eq!(parse_overlay_peer("10.0.0.2=").0, "10.0.0.2");
        assert!(parse_overlay_peer("10.0.0.2=").1.is_none());
    }

    #[test]
    fn overlay_add_peer_dedup() {
        let dir = std::env::temp_dir().join(format!("dlx-addpeer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = NetworkStore::open(&dir).unwrap();
        store
            .create_overlay("ov", 7, &[], Some("10.250.0.1"))
            .unwrap();
        // learns a peer
        let n = store
            .add_overlay_peer("ov", "10.0.0.2=PUB2=10.250.0.2")
            .unwrap();
        assert_eq!(n.peers, vec!["10.0.0.2=PUB2=10.250.0.2"]);
        assert_eq!(n.wg_ip.as_deref(), Some("10.250.0.1")); // preserves wgip
                                                            // rotation: same node_ip, new key → REPLACES (doesn't duplicate)
        let n2 = store
            .add_overlay_peer("ov", "10.0.0.2=PUBNEW=10.250.0.2")
            .unwrap();
        assert_eq!(n2.peers, vec!["10.0.0.2=PUBNEW=10.250.0.2"]);
        // 2nd distinct peer → adds
        let n3 = store
            .add_overlay_peer("ov", "10.0.0.3=PUB3=10.250.0.3")
            .unwrap();
        assert_eq!(n3.peers.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlay_wgip_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dlx-wgo-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = NetworkStore::open(&dir).unwrap();
        let peers = vec!["10.0.0.2=PUB2=10.250.0.2".to_string()];
        let n = store
            .create_overlay("ov", 42, &peers, Some("10.250.0.1"))
            .unwrap();
        assert_eq!(n.wg_ip.as_deref(), Some("10.250.0.1"));
        // reloads from disk → wg_ip persists
        let n2 = store.get("ov").unwrap();
        assert_eq!(n2.wg_ip.as_deref(), Some("10.250.0.1"));
        assert_eq!(n2.peers, peers);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ip_is_deterministic_and_avoids_reserved() {
        // fixed prefix (the default is auto-detected/persisted at runtime). The
        // DERIVED IP (pure hash) is stable and avoids reserved ones — it's just the
        // starting point; real uniqueness comes from the lease + probing (see `ipam`).
        let ip = derive_ip_in("10.88", "0000000a00000000");
        assert!(ip.starts_with("10.88."));
        // ids that share the first 8 hex DERIVE the same IP — this was the root of the
        // collision. It's `ipam::allocate` that separates them (tested in `ipam::tests`).
        assert_eq!(
            derive_ip_in("10.88", "deadbeef1234"),
            derive_ip_in("10.88", "deadbeef9999")
        );
        // the last octet is never 0/1/255
        for id in ["00000000", "00000001", "000000ff"] {
            let last: u8 = derive_ip_in("10.88", id)
                .rsplit('.')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert!(last >= 2 && last != 255, "id {id} -> {last}");
        }
    }

    #[test]
    fn valid_ip_in_subnet_aceita_e_rejeita() {
        // within the subnet, usable unicast
        assert!(valid_ip_in_subnet("10.88", "10.88.0.77"));
        assert!(valid_ip_in_subnet("10.88", "10.88.255.254"));
        assert!(valid_ip_in_subnet("10.204", "10.204.19.189"));
        // outside the subnet (wrong prefix)
        assert!(!valid_ip_in_subnet("10.88", "10.9.0.5"));
        assert!(!valid_ip_in_subnet("10.88", "192.168.0.5"));
        // reserved: network, gateway, broadcast
        assert!(!valid_ip_in_subnet("10.88", "10.88.0.0"));
        assert!(!valid_ip_in_subnet("10.88", "10.88.0.1"));
        assert!(!valid_ip_in_subnet("10.88", "10.88.255.255"));
        // malformed
        assert!(!valid_ip_in_subnet("10.88", "10.88.0"));
        assert!(!valid_ip_in_subnet("10.88", "10.88.0.300"));
        assert!(!valid_ip_in_subnet("10.88", "10.88.0.x"));
    }

    #[test]
    fn default_base_evita_docker_e_podman() {
        // the default base can never be 88 (Podman) nor land in 172/16 (Docker
        // is not 10.x). pick_free_base always picks outside the used octets.
        let used = used_10_octets();
        assert!(
            used.contains(&88),
            "Podman (10.88) tem de estar marcado como usado"
        );
        assert!(used.contains(&90), "VIPs (10.90) reservado");
        let base = pick_free_base();
        assert!(
            !used.contains(&base),
            "a base escolhida ({base}) colide com algo já usado"
        );
        assert!(base != 88 && base != 90);
    }

    /// The two stores that name the same bridge MUST agree, in both directions.
    ///
    /// Regression for a measured bug: `network ls`/`inspect` printed
    /// `dlxne9623e` for `lab-net` while the device the holder had actually
    /// created — and that the `NetDef`, `resolve_net`, `vm bridge` and the
    /// egress view all use — was `dlxn0536623e`. Two independent formulas for
    /// the same thing, so the column an operator reads to go and debug the
    /// device sent them to a device that is not there.
    ///
    /// The literal is the ground truth captured on a live host (kaeso-sys-01,
    /// network `lab-net`, `ip -br link show` inside the holder netns), NOT a
    /// value computed from the code under test. Without it this test would be
    /// tautological the moment both sides call the same helper: it is what
    /// makes a change to the shared formula itself fail here, rather than
    /// silently renaming every bridge in the fleet.
    #[test]
    fn o_nome_da_bridge_e_o_mesmo_nos_dois_stores() {
        // The measured device name, byte for byte.
        assert_eq!(bridge_name("lab-net"), "dlxn0536623e");

        for name in ["lab-net", "frontend", "a", "kaeso-net", "com-hifen_e_under"] {
            let expected = bridge_name(name);
            // Physical-plane direction: the `NetDef` that gets serialized to
            // disk and whose `bridge` the holder creates the link with. Asserted
            // through the real constructor, not through `bridge_name` again —
            // otherwise this side of the comparison proves nothing.
            assert_eq!(
                infra::NetDef::new(name, "10.201").bridge,
                expected,
                "network '{name}': the physical plane disagrees with the store"
            );
            // `NetworkStore` direction: the declarative record REPORTS the name.
            // Swept across the whole base space because the old formula folded
            // the base into the name — a single base would have hidden it.
            for base in 100..=239u8 {
                assert_eq!(
                    Network::user_with_base(name, base).bridge,
                    expected,
                    "network '{name}' (base {base}): the store disagrees with the physical plane"
                );
            }
            // An overlay is realized physically too, and inherits the same name.
            assert_eq!(
                Network::overlay_with_base(name, 201, 42, vec![], None).bridge,
                expected
            );
            // IFNAMSIZ, the reason the name is a hash and not the network's own.
            assert!(expected.starts_with("dlxn") && expected.len() <= 15);
        }

        // The default network is not hashed — the two stores name it with their
        // own constant, and those must not drift apart either.
        assert_eq!(Network::default_bridge().bridge, infra::INFRA_BRIDGE);
    }

    #[test]
    fn user_network_is_isolated_subnet() {
        let base = Network::base_for("frontend");
        assert!((100..=239).contains(&base), "base {base} fora do intervalo");
        let n = Network::user_with_base("frontend", base);
        assert_eq!(n.subnet, format!("10.{base}.0.0/16"));
        assert_eq!(n.gateway, format!("10.{base}.0.1"));
        assert!(n.bridge.starts_with("dlxn") && n.bridge.len() <= 15);
        // outside the default subnet (88) and the VIPs (90).
        assert_ne!(base, 88);
        assert_ne!(base, 90);
        // a container IP lands in the network's subnet.
        assert!(alloc_ip_in(&n.prefix, "deadbeef").starts_with(&format!("10.{base}.")));
    }

    #[test]
    fn net_rate_spec_parsing() {
        // throughputs: decimal suffixes (k/m/g), with or without `bit`/`bps`.
        assert_eq!(parse_rate_bits("1000000").unwrap(), 1_000_000);
        assert_eq!(parse_rate_bits("10mbit").unwrap(), 10_000_000);
        assert_eq!(parse_rate_bits("512k").unwrap(), 512_000);
        assert_eq!(parse_rate_bits("1G").unwrap(), 1_000_000_000);
        assert_eq!(parse_rate_bits("100mbps").unwrap(), 100_000_000);
        // invalid / non-positive.
        assert!(parse_rate_bits("").is_err());
        assert!(parse_rate_bits("abc").is_err());
        assert!(parse_rate_bits("0").is_err());
        assert!(parse_rate_bits("-5m").is_err());

        // burst: binary suffixes (k=1024), optional trailing `b`.
        assert_eq!(parse_size_bytes("4096"), Some(4096));
        assert_eq!(parse_size_bytes("256k"), Some(256 * 1024));
        assert_eq!(parse_size_bytes("1mb"), Some(1024 * 1024));
        assert_eq!(parse_size_bytes("xyz"), None);

        // default burst = ~100 ms of throughput, with a floor of 16 KiB.
        let r = parse_net_rate("10mbit", None).unwrap();
        assert_eq!(r.rate_bit, 10_000_000);
        assert_eq!(r.burst_bytes, 10_000_000 / 8 / 10); // 125_000 bytes
        let small = parse_net_rate("100k", None).unwrap();
        assert_eq!(small.burst_bytes, 16 * 1024); // floor applied

        // an explicit burst is respected; the `tc` format is as expected.
        let r = parse_net_rate("1mbit", Some("32k")).unwrap();
        assert_eq!(
            r,
            NetRate {
                rate_bit: 1_000_000,
                burst_bytes: 32 * 1024
            }
        );
        // The `tc` rendering used to be asserted here, on `NetRate::tc_rate`/
        // `tc_burst`. Those went with `Net`: this crate hands the holder the raw
        // numbers (`netrate <vh> <rate_bit> <burst_bytes>`) and the `tc` argv is
        // built THERE, in `infra::do_netrate`. Keeping an assertion here would
        // have been testing a formatter no caller reaches.
        assert!(parse_net_rate("1mbit", Some("0")).is_err());
        assert!(parse_net_rate("1mbit", Some("bad")).is_err());
    }

    /// `as u64` on an `f64` SATURATES in Rust, so a value that does not fit
    /// silently became `u64::MAX` — shaping the operator never asked for,
    /// reported as applied. The same guard exists in
    /// `delonix-volume::parse_size_bytes`; it was written there and never
    /// carried over here. A value out of range is an input error, never a clamp.
    #[test]
    fn um_valor_fora_de_alcance_e_recusado_em_vez_de_saturar() {
        assert!(parse_rate_bits("99999999999g").is_err());
        assert!(parse_rate_bits("1e400").is_err());
        // The largest sane values still parse — no over-eager rejection.
        assert_eq!(parse_rate_bits("100g").unwrap(), 100_000_000_000);

        assert_eq!(parse_size_bytes("99999999999t"), None);
        assert_eq!(parse_size_bytes("1024t"), Some(1024 * 1024u64.pow(4)));
    }

    #[test]
    fn network_store_create_get_list_remove() {
        let tmp = std::env::temp_dir().join(format!("dlxnet-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let s = NetworkStore::open(&tmp).unwrap();
        assert!(s.get("bridge").unwrap().name == DEFAULT_NET);
        assert!(s.get("nope").is_err());
        let a = s.create("alpha").unwrap();
        let b = s.create("beta").unwrap();
        assert_ne!(a.subnet, b.subnet, "redes distintas têm subnets distintas");
        assert_eq!(s.list().unwrap().len(), 2);
        assert!(s.create("alpha").is_err(), "duplicado deve falhar");
        assert!(s.create("bridge").is_err(), "nome reservado deve falhar");
        s.remove("alpha").unwrap();
        assert_eq!(s.list().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn base_from_subnet_aceita_a_forma_suportada() {
        assert_eq!(
            NetworkStore::base_from_subnet("10.200.0.0/16").unwrap(),
            200
        );
        assert_eq!(
            NetworkStore::base_from_subnet("10.254.0.0/16").unwrap(),
            254
        );
    }

    #[test]
    fn base_from_subnet_recusa_e_diz_o_que_e_suportado() {
        // Each of these used to be ACCEPTED and silently dropped, which is the
        // whole reason this function exists. The message has to name the shape
        // that works, or the refusal is just a different way of being useless.
        for bad in [
            "172.20.0.0/16", // outside the workload space
            "10.50.0.0/16",  // right shape, wrong range
            "10.200.0.0/24", // only /16
            "10.200.0.0",    // no prefix length
            "10.200.1.0/16", // /16 that does not end in .0.0
            "banana",
        ] {
            let e = NetworkStore::base_from_subnet(bad)
                .expect_err("devia recusar {bad}")
                .to_string();
            assert!(
                e.contains("10.<200-254>.0.0/16"),
                "a recusa de {bad} tem de dizer o que e suportado: {e}"
            );
        }
    }

    #[test]
    fn create_with_base_e_idempotente_mas_nao_renumera() {
        let dir = std::env::temp_dir().join(format!(
            "delonix-netstore-test-{}-{}",
            std::process::id(),
            fnv32("renumera")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = NetworkStore::open(&dir).unwrap();

        let a = store.create_with_base("vpc", 210).unwrap();
        assert_eq!(a.subnet, "10.210.0.0/16");
        // Same subnet again: a no-op, so re-applying an unchanged manifest works.
        assert_eq!(store.create_with_base("vpc", 210).unwrap().subnet, a.subnet);
        // A DIFFERENT one must fail loudly: returning the old network as if the
        // request had been honoured is exactly the silence being removed here.
        let e = store.create_with_base("vpc", 211).unwrap_err().to_string();
        assert!(e.contains("cannot be changed in place"), "{e}");

        // And two networks may not share a /16 — their IPAM ranges would collide.
        let e = store
            .create_with_base("outra", 210)
            .unwrap_err()
            .to_string();
        assert!(e.contains("already used by network 'vpc'"), "{e}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_with_base_recusa_fora_do_espaco_de_workload() {
        let dir = std::env::temp_dir().join(format!(
            "delonix-netstore-range-{}-{}",
            std::process::id(),
            fnv32("range")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = NetworkStore::open(&dir).unwrap();
        // 50 is a valid octet but not a valid WORKLOAD one; the old guard was
        // `1..=254`, which let a network be created where nothing else looks.
        assert!(store.create_with_base("x", 50).is_err());
        assert!(store.create_with_base("x", 200).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
