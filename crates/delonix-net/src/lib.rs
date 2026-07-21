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
//! - the per-container firewall is a `set` of blocked IPs in a dedicated `forward`
//!   chain (table `ip delonix`) — reversible per element.
//!
//! The container attach is done CNI-style: the runtime creates the `netns`
//! (`CLONE_NEWNET`); [`Net::attach`] configures it from the host, by PID.

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
const TABLE: &str = "delonix"; // dedicated nft table (ip family)
const VIP_SUBNET: &str = "10.90.0.0/16"; // service VIPs (OUTSIDE the container subnet)

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
    let _ = std::fs::write(&path, base.to_string());
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

/// A1 — **inbound default-deny** for a Delonix network's subnet. Blocks
/// NEW connections forwarded TO a container that are not:
///   - return traffic (any state != `new` passes, e.g.: `established`), or
///   - a **published** port (`ct status dnat`, i.e. it already went through DNAT at the ingress).
///
/// Keeps EGRESS fully open (the rule only matches `ip daddr <subnet>`) and does NOT
/// touch the `forward` hook's policy — the host's non-Delonix forwarding stays
/// intact (unlike a `policy drop`, which would affect Docker/k8s/libvirt on the
/// same host). The two rules are self-sufficient (the `dnat accept` precedes the
/// `new drop`), idempotent, and work on new or pre-existing tables.
///
/// Disableable with `DELONIX_FORWARD_OPEN=1` (restores the historical open
/// forwarding — direct access to the container IP from other networks).
fn forward_inbound_deny(subnet: &str) {
    if std::env::var_os("DELONIX_FORWARD_OPEN").is_some() {
        // NET-03: the opt-out reverts to default-allow — don't leave this silent.
        tracing::warn!(
            "SECURITY WARNING — DELONIX_FORWARD_OPEN is active: the forward inbound-deny is \
             OFF (containers directly reachable from other networks/the host). For \
             debugging only — do NOT use in production."
        );
        return;
    }
    let drop_needle = format!("ip daddr {subnet} ct state new drop");
    if let Ok(out) = capture("nft", &["list", "chain", "ip", TABLE, "forward"]) {
        if out.contains(&drop_needle) {
            return; // already applied
        }
    }
    // 1st: let published-port traffic through (DNAT); 2nd: deny the rest.
    run_ok(
        "nft",
        &[
            "add", "rule", "ip", TABLE, "forward", "ip", "daddr", subnet, "ct", "status", "dnat",
            "accept",
        ],
    );
    run_ok(
        "nft",
        &[
            "add", "rule", "ip", TABLE, "forward", "ip", "daddr", subnet, "ct", "state", "new",
            "drop",
        ],
    );
}

/// The Delonix network manager.
pub struct Net;

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

/// Lists the Delonix bridges present on the host (`delonix0` + `dlxn*` from user
/// networks) — used to build the inter-network isolation rules.
fn list_delonix_bridges() -> Vec<String> {
    let out = capture("ip", &["-o", "link", "show", "type", "bridge"]).unwrap_or_default();
    let mut names = Vec::new();
    for line in out.lines() {
        // format: "N: name: <...>" (the name may have "@" for the peer).
        if let Some(name) = line
            .split(':')
            .nth(1)
            .map(|s| s.trim().split('@').next().unwrap_or("").trim())
        {
            if name == BRIDGE || name.starts_with("dlxn") {
                names.push(name.to_string());
            }
        }
    }
    names
}

fn table_exists() -> bool {
    Command::new("nft")
        .args(["list", "table", "ip", TABLE])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---- deterministic names and IPs -----------------------------------------

fn netns_name(id: &str) -> String {
    format!("delonix-{}", &id[..id.len().min(12)])
}

/// Removes the anti-spoofing rules (`iifname "<hv>" ip saddr != … drop`) from the ROOT
/// table's `forward`, by handle (idempotency). Mirror of `infra.rs::clear_antispoof`
/// for the root/legacy path. Best-effort.
fn clear_antispoof_root(hv: &str) {
    let listed =
        capture("nft", &["-a", "list", "chain", "ip", TABLE, "forward"]).unwrap_or_default();
    let needle = format!("iifname \"{hv}\"");
    for line in listed.lines() {
        if line.contains(&needle) && line.contains("saddr") && line.contains("drop") {
            if let Some(h) = line
                .rsplit("# handle ")
                .next()
                .and_then(|x| x.trim().parse::<u32>().ok())
            {
                run_ok(
                    "nft",
                    &[
                        "delete",
                        "rule",
                        "ip",
                        TABLE,
                        "forward",
                        "handle",
                        &h.to_string(),
                    ],
                );
            }
        }
    }
}
fn host_veth(id: &str) -> String {
    format!("dlx{}", &id[..id.len().min(8)]) // <= 15 chars (IFNAMSIZ)
}
fn peer_veth(id: &str) -> String {
    format!("dlxp{}", &id[..id.len().min(8)])
}
// Veths of an *extra* interface (multi-homing): suffixed by the index (>=1) so as
// not to collide with the primary interface's nor between networks. <= 15 chars.
fn host_veth_n(id: &str, idx: u32) -> String {
    format!("dlx{}{idx}", &id[..id.len().min(6)])
}
fn peer_veth_n(id: &str, idx: u32) -> String {
    format!("dlxp{}{idx}", &id[..id.len().min(6)])
}

/// Validates that `ip` is a usable unicast address in `prefix`'s `/16` subnet
/// (e.g.: prefix `10.88`): 4 octets, first two == prefix, not the gateway
/// (`prefix.0.1`), the network (`prefix.0.0`) or the broadcast (`prefix.255.255`).
pub fn valid_ip_in_subnet(prefix: &str, ip: &str) -> bool {
    let oct: Vec<&str> = ip.split('.').collect();
    if oct.len() != 4 {
        return false;
    }
    let nums: Vec<u16> = match oct
        .iter()
        .map(|o| o.parse::<u16>())
        .collect::<std::result::Result<_, _>>()
    {
        Ok(v) => v,
        Err(_) => return false,
    };
    if nums.iter().any(|&n| n > 255) {
        return false;
    }
    let pfx = format!("{}.{}", nums[0], nums[1]);
    if pfx != prefix {
        return false;
    }
    let host = (nums[2], nums[3]);
    // excludes network (.0.0), gateway (.0.1) and broadcast (.255.255).
    !(host == (0, 0) || host == (0, 1) || host == (255, 255))
}

/// Deterministic IP in `10.88.A.B`, derived from the id (avoids .0/.1/.255).
/// Parses `hostPort:contPort[/tcp|udp]`, `contPort` or `hp:cp`. Returns
/// `(host_port, cont_port, proto)`.
pub fn parse_publish(spec: &str) -> Result<(String, String, String)> {
    let (mapping, proto) = match spec.split_once('/') {
        Some((m, p)) => (m, p.to_lowercase()),
        None => (spec, "tcp".to_string()),
    };
    if proto != "tcp" && proto != "udp" {
        return Err(Error::Invalid(format!(
            "invalid protocol in '{spec}' (tcp|udp)"
        )));
    }
    let (host_port, cont_port) = match mapping.rsplit_once(':') {
        Some((h, c)) => (h.trim(), c.trim()),
        None => (mapping.trim(), mapping.trim()),
    };
    let valid = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit());
    if !valid(host_port) || !valid(cont_port) {
        return Err(Error::Invalid(format!(
            "invalid port in '{spec}' (e.g. 8080:80)"
        )));
    }
    Ok((host_port.to_string(), cont_port.to_string(), proto))
}

/// Specification of a container's network bandwidth limit.
/// `rate_bit` is the throughput in bits/second; `burst_bytes` is the (token) bucket
/// of the TBF/police, in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetRate {
    pub rate_bit: u64,
    pub burst_bytes: u64,
}

impl NetRate {
    /// Throughput in the format `tc` accepts (e.g.: `10000000bit`).
    fn tc_rate(&self) -> String {
        format!("{}bit", self.rate_bit)
    }
    /// Burst (bytes) in the format `tc` accepts (a plain number = bytes).
    fn tc_burst(&self) -> String {
        self.burst_bytes.to_string()
    }
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
    Ok((n * mult as f64) as u64)
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
    Some((n * mult as f64) as u64)
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

/// Stable VIP of a service (FNV-1a hash → `10.90.a.b`), outside the container
/// subnet so that traffic passes through the host (where nftables load-balances).
pub fn service_vip(key: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for byte in key.bytes() {
        h ^= byte as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    let a = ((h >> 8) & 0xff) as u8;
    let mut b = (h & 0xff) as u8;
    if b < 2 {
        b = 2;
    }
    if b == 255 {
        b = 254;
    }
    format!("10.90.{a}.{b}")
}

/// **Preferred** IP (deterministic, pure) in an arbitrary `/16` (`<prefix>.A.B`),
/// derived from the id. It's just the starting point: on its own it collides by the birthday
/// paradox at ~300 containers (32 bits of the id → 16 bits of host). Real uniqueness comes from
/// the lease registry + probing in [`ipam::allocate`]; see [`alloc_ip_in`].
pub fn derive_ip_in(prefix: &str, id: &str) -> String {
    let hex = &id[..id.len().min(8)];
    let n = u32::from_str_radix(hex, 16).unwrap_or(2);
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

/// The prefix length (`/24`) of a CIDR subnet, or `24` by default.
pub fn cidr_prefix_len(subnet: &str) -> u32 {
    subnet
        .rsplit_once('/')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(24)
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
        }
    }

    /// Builds a user network with a given base octet (`10.<base>.0.0/16`).
    /// The bridge name includes the base + a hash of the name (unique, ≤ 15 chars).
    fn user_with_base(name: &str, base: u8) -> Self {
        let bridge = format!("dlxn{:02x}{:04x}", base, fnv32(name) & 0xffff);
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
        for line in trimmed.lines() {
            if let Some((k, v)) = line.split_once('=') {
                kv.insert(k.trim(), v.trim().to_string());
            }
        }
        let driver = kv
            .get("driver")
            .map(String::as_str)
            .unwrap_or(DRIVER_BRIDGE);
        match driver {
            DRIVER_MACVLAN | DRIVER_IPVLAN => {
                let parent = kv.get("parent").cloned().ok_or_else(|| {
                    Error::Invalid(format!("network '{name}' ({driver}) has no parent"))
                })?;
                let subnet = kv.get("subnet").cloned().ok_or_else(|| {
                    Error::Invalid(format!("network '{name}' ({driver}) has no subnet"))
                })?;
                let gateway = kv.get("gateway").cloned().unwrap_or_default();
                Ok(Network::lan(name, driver, &parent, &subnet, &gateway))
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
                Ok(Network::overlay_with_base(name, base, vni, peers, wg_ip))
            }
            _ => {
                let base: u8 = kv
                    .get("base")
                    .and_then(|b| b.parse().ok())
                    .ok_or_else(|| Error::Invalid(format!("network '{name}' is corrupted")))?;
                Ok(Network::user_with_base(name, base))
            }
        }
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
            return Err(Error::Invalid(format!("network '{name}' already exists")));
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
        std::fs::write(self.path(name), base.to_string())?;
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
            return self.get(name);
        }
        if !(1..=254).contains(&base) {
            return Err(Error::Invalid(format!(
                "invalid /16 base octet: {base} (1..=254)"
            )));
        }
        std::fs::write(self.path(name), base.to_string())?;
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
            return Err(Error::Invalid(format!("network '{name}' already exists")));
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
        std::fs::write(self.path(name), body)?;
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
        // re-persists replacing ONLY the `peers=` line (preserves base/vni/wgip).
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
        std::fs::write(self.path(name), out.join("\n") + "\n").map_err(|e| Error::Runtime {
            context: "write overlay",
            message: e.to_string(),
        })?;
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
            return Err(Error::Invalid(format!("network '{name}' already exists")));
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
        std::fs::write(self.path(name), body)?;
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

/// Name of the nft chain dedicated to a container's firewall (derived from the IP).
fn cfw_chain(ip: &str) -> String {
    format!("cfw{:08x}", fnv32(ip))
}

impl Net {
    /// **Applies a container's L4 firewall** (Phase-1 issue): translates the UI's
    /// rules into a dedicated nftables chain (`cfw<ip-hash>`), with jumps from the
    /// `forward` for traffic to/from the container's IP. Idempotent (rebuilds
    /// the chain on each call). Replaces the "apply" that was just a toast in the Console.
    pub fn apply_container_firewall(&self, ip: &str, fw: &ContainerFw) -> Result<()> {
        self.ensure_bridge()?; // the `delonix` table has to exist
        let chain = cfw_chain(ip);
        // ensures the chain (regular, no hook — only a jump target). NOTE: `capture`
        // returns Ok even on failure, so we test the CONTENT, not the error.
        let exists = capture("nft", &["list", "chain", "ip", TABLE, &chain])
            .map(|o| o.contains(&chain))
            .unwrap_or(false);
        if !exists {
            run_ok("nft", &["add", "chain", "ip", TABLE, &chain]);
        }
        // idempotent jumps in the forward: traffic TO (daddr) and FROM (saddr) the IP.
        let fwd = capture("nft", &["list", "chain", "ip", TABLE, "forward"]).unwrap_or_default();
        for dir in ["daddr", "saddr"] {
            let needle = format!("ip {dir} {ip} jump {chain}");
            if !fwd.contains(&needle) {
                run_ok(
                    "nft",
                    &[
                        "add", "rule", "ip", TABLE, "forward", "ip", dir, ip, "jump", &chain,
                    ],
                );
            }
        }
        // rebuilds the chain body (rules + default policy).
        let mut body = String::new();
        if fw.enabled {
            for r in &fw.rules {
                // Defense against nft injection: skips rules with unsafe fields.
                if !r.nft_safe() {
                    continue;
                }
                let (self_dir, peer_dir) = if r.dir == "out" {
                    ("saddr", "daddr")
                } else {
                    ("daddr", "saddr")
                };
                let mut line = format!("ip {self_dir} {ip}");
                if !r.src.is_empty() && r.src != "0.0.0.0/0" && r.src != "*" {
                    line.push_str(&format!(" ip {peer_dir} {}", r.src));
                }
                if !r.proto.is_empty() && r.proto != "any" {
                    line.push_str(&format!(" {}", r.proto));
                    if !r.port.is_empty() && r.port != "*" {
                        line.push_str(&format!(" dport {}", r.port));
                    }
                }
                line.push_str(if r.action == "allow" {
                    " accept"
                } else {
                    " drop"
                });
                body.push_str(&format!("\t\t{line}\n"));
            }
            // default policy (final drop in the indicated direction).
            if fw.policy_in == "deny" {
                body.push_str(&format!("\t\tip daddr {ip} drop\n"));
            }
            if fw.policy_out == "deny" {
                body.push_str(&format!("\t\tip saddr {ip} drop\n"));
            }
        }
        // flush + re-add in a single script (the chain stays, the jumps remain valid).
        let script = format!(
            "flush chain ip {TABLE} {chain}\ntable ip {TABLE} {{\n\tchain {chain} {{\n{body}\t}}\n}}\n"
        );
        apply_nft(&script)
    }

    /// Removes a container's firewall: takes the jumps out of the `forward` (by handle) and
    /// deletes the chain. Called on `detach` so as not to leave orphan rules.
    pub fn remove_container_firewall(&self, ip: &str) {
        let chain = cfw_chain(ip);
        // removes the forward jumps (needs each rule's handle).
        if let Ok(out) = capture("nft", &["-a", "list", "chain", "ip", TABLE, "forward"]) {
            for line in out.lines() {
                if line.contains(&format!("jump {chain}")) {
                    if let Some(h) = line.rsplit("handle ").next().map(|s| s.trim()) {
                        run_ok(
                            "nft",
                            &["delete", "rule", "ip", TABLE, "forward", "handle", h],
                        );
                    }
                }
            }
        }
        run_ok("nft", &["delete", "chain", "ip", TABLE, &chain]);
    }

    /// Ensures the `delonix0` bridge, IP forwarding and the nft table (NAT + fw).
    pub fn ensure_bridge(&self) -> Result<()> {
        let gateway = default_gateway();
        let subnet = default_subnet();
        if !link_exists(BRIDGE) {
            run("ip", &["link", "add", BRIDGE, "type", "bridge"])?;
            run(
                "ip",
                &["addr", "add", &format!("{gateway}/16"), "dev", BRIDGE],
            )?;
            run("ip", &["link", "set", BRIDGE, "up"])?;
        }
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");

        if !table_exists() {
            // A dedicated table: NAT (masquerade + DNAT) + per-container firewall.
            let ruleset = format!(
                "table ip {TABLE} {{\n\
                 \tchain postrouting {{\n\
                 \t\ttype nat hook postrouting priority 100;\n\
                 \t\tip saddr {subnet} oifname != \"{BRIDGE}\" masquerade\n\
                 \t}}\n\
                 \tchain prerouting {{\n\
                 \t\ttype nat hook prerouting priority -100;\n\
                 \t}}\n\
                 \tchain output {{\n\
                 \t\ttype nat hook output priority -100;\n\
                 \t}}\n\
                 \tset blocked {{ type ipv4_addr; }}\n\
                 \tchain forward {{\n\
                 \t\ttype filter hook forward priority 0;\n\
                 \t\tip saddr @blocked drop\n\
                 \t\tip daddr @blocked drop\n\
                 \t}}\n\
                 }}\n"
            );
            apply_nft(&ruleset)?;
        }
        // A1: inbound default-deny on the default subnet (idempotent).
        forward_inbound_deny(&subnet);
        Ok(())
    }

    /// Attaches a container to the DEFAULT network (`delonix0`). Shortcut of [`Net::attach_on`].
    pub fn attach(&self, pid: i32, id: &str) -> Result<String> {
        self.attach_on(&Network::default_bridge(), pid, id)
    }

    /// Ensures a user network's infrastructure: its own bridge, the
    /// `MASQUERADE` of its subnet and the **isolation** (forward drop) from the
    /// other Delonix networks. The default network is just `ensure_bridge`.
    pub fn ensure_network(&self, net: &Network) -> Result<()> {
        // LAN drivers (macvlan/ipvlan): there's no bridge or NAT — the container
        // goes straight to the physical LAN. Just ensure the parent NIC is up.
        if net.is_lan_driver() {
            if let Some(parent) = &net.parent {
                if !link_exists(parent) {
                    return Err(Error::Invalid(format!(
                        "parent NIC '{parent}' does not exist"
                    )));
                }
                run_ok("ip", &["link", "set", parent, "up"]);
            }
            return Ok(());
        }
        self.ensure_bridge()?; // the nft table lives here (applies to all networks)
        if net.name == DEFAULT_NET {
            return Ok(());
        }
        if !link_exists(&net.bridge) {
            run("ip", &["link", "add", &net.bridge, "type", "bridge"])?;
            run(
                "ip",
                &[
                    "addr",
                    "add",
                    &format!("{}/16", net.gateway),
                    "dev",
                    &net.bridge,
                ],
            )?;
            run("ip", &["link", "set", &net.bridge, "up"])?;
        }
        // MASQUERADE of this network's subnet (egress to the Internet).
        if let Ok(out) = capture("nft", &["list", "chain", "ip", TABLE, "postrouting"]) {
            if !out.contains(&net.subnet) {
                run_ok(
                    "nft",
                    &[
                        "add",
                        "rule",
                        "ip",
                        TABLE,
                        "postrouting",
                        "ip",
                        "saddr",
                        &net.subnet,
                        "oifname",
                        "!=",
                        &net.bridge,
                        "masquerade",
                    ],
                );
            }
        }
        // A1: inbound default-deny also on this network's subnet (idempotent).
        forward_inbound_deny(&net.subnet);
        // Isolation: blocks the forward between this bridge and any OTHER
        // Delonix bridge (containers of different networks can't reach each other).
        for other in list_delonix_bridges() {
            if other != net.bridge {
                self.isolate_pair(&net.bridge, &other);
            }
        }
        // Overlay: creates the VXLAN uplink and enslaves it to the bridge, so the
        // L2 segment crosses the peer nodes (FDB per peer).
        if net.driver == DRIVER_OVERLAY {
            self.ensure_vxlan(net)?;
            self.ensure_overlay_wg(net)?; // encrypts the VXLAN transport between nodes (#6)
        }
        Ok(())
    }

    /// Creates (idempotent) the VXLAN device `dlxvx<vni>`, enslaves it to the overlay
    /// network's bridge and adds an FDB entry for each peer node (unknown L2
    /// frames are replicated to the peers via `dst <ip>`). Multi-node: each
    /// node has to create the same overlay (same `vni`) listing the others as peers.
    fn ensure_vxlan(&self, net: &Network) -> Result<()> {
        let Some(vni) = net.vni else { return Ok(()) };
        let Some(dev) = net.vxlan_dev() else {
            return Ok(());
        };
        if !link_exists(&dev) {
            // `nolearning` + manual FDB = deterministic control of the flooding.
            run(
                "ip",
                &[
                    "link",
                    "add",
                    &dev,
                    "type",
                    "vxlan",
                    "id",
                    &vni.to_string(),
                    "dstport",
                    VXLAN_PORT,
                    "nolearning",
                ],
            )?;
            run_ok("ip", &["link", "set", &dev, "master", &net.bridge]);
            run_ok("ip", &["link", "set", &dev, "up"]);
        }
        // FDB: replicates unknown L2 frames (broadcast/unknown-unicast) to
        // each peer — it's what makes the L2 reach the containers on the other nodes.
        let have = capture("bridge", &["fdb", "show", "dev", &dev]).unwrap_or_default();
        for peer in &net.peers {
            // encrypted overlay: the FDB points to the peer's wg_ip (not the node_ip),
            // so the VXLAN UDP is routed through the wg tunnel = encrypted.
            let (node_ip, wg) = parse_overlay_peer(peer);
            let dst = wg.map(|(_, wgip)| wgip).unwrap_or(node_ip);
            if !have.contains(dst.as_str()) {
                run_ok(
                    "bridge",
                    &[
                        "fdb",
                        "append",
                        "00:00:00:00:00:00",
                        "dev",
                        &dev,
                        "dst",
                        &dst,
                    ],
                );
            }
        }
        Ok(())
    }

    /// WireGuard over the overlay (req #6, phase 3): brings up the overlay's wg interface
    /// and configures the peers, so as to ENCRYPT the VXLAN transport between nodes. Only acts
    /// if the network has a `wg_ip` (of this node); otherwise it stays flat VXLAN (compatible).
    /// `ensure_vxlan` already points the FDB to the peers' `wg_ip`, so the VXLAN
    /// UDP (4789) travels through the wg tunnel. Without `wg` on the host → degrades (no encryption).
    fn ensure_overlay_wg(&self, net: &Network) -> Result<()> {
        let Some(my_wg_ip) = net.wg_ip.as_deref() else {
            return Ok(());
        };
        let Some(vni) = net.vni else { return Ok(()) };
        if !crate::wg::available() {
            return Ok(());
        }
        let key = crate::wg::ensure_node_key()?;
        let iface = format!("wgo{vni:06x}"); // ≤ 15 chars
        let port: u16 = 51820;
        crate::wg::ensure_iface(&iface, &key.private, port, &format!("{my_wg_ip}/24"))?;
        for peer in &net.peers {
            let (node_ip, wg) = parse_overlay_peer(peer);
            if let Some((pubkey, wgip)) = wg {
                crate::wg::set_peer(
                    &iface,
                    &crate::wg::Peer {
                        public: pubkey,
                        endpoint: format!("{node_ip}:{port}"),
                        allowed_ips: vec![format!("{wgip}/32")],
                    },
                )?;
            }
        }
        Ok(())
    }

    /// Adds (idempotent) the forward rules that block traffic
    /// forwarded between two distinct Delonix bridges, in both directions.
    fn isolate_pair(&self, a: &str, b: &str) {
        let have = capture("nft", &["list", "chain", "ip", TABLE, "forward"]).unwrap_or_default();
        for (i, o) in [(a, b), (b, a)] {
            let needle = format!("iifname \"{i}\" oifname \"{o}\"");
            if !have.contains(&needle) {
                run_ok(
                    "nft",
                    &[
                        "add", "rule", "ip", TABLE, "forward", "iifname", i, "oifname", o, "drop",
                    ],
                );
            }
        }
    }

    /// Attaches a container to a specific network (CNI-style): configures the `netns`
    /// by PID on that network's bridge/subnet, returning the assigned IP.
    pub fn attach_on(&self, net: &Network, pid: i32, id: &str) -> Result<String> {
        self.attach_on_ip(net, pid, id, None)
    }

    /// Like [`Net::attach_on`], but allows **pinning the IP** (`Some(ip)`); `None`
    /// derives the IP from the id (default behavior). Validates the IP against the subnet.
    pub fn attach_on_ip(
        &self,
        net: &Network,
        pid: i32,
        id: &str,
        ip: Option<&str>,
    ) -> Result<String> {
        self.ensure_network(net)?;
        // macvlan/ipvlan path: creates the sub-interface over the parent NIC and moves it
        // into the container's netns (no veth, no bridge — the container sits on the
        // physical LAN with that LAN's IP/gateway).
        if net.is_lan_driver() {
            return self.attach_lan(net, pid, id, ip);
        }
        let ip = match ip {
            Some(want) => {
                if !valid_ip_in_subnet(&net.prefix, want) {
                    return Err(Error::Invalid(format!(
                        "IP {want} outside subnet {} of network '{}'",
                        net.subnet, net.name
                    )));
                }
                // Registers the pinned IP so other containers' probing sees it
                // as occupied (otherwise it would auto-allocate on top of it).
                ipam::reserve(&net.prefix, id, want);
                want.to_string()
            }
            None => ipam::allocate(&net.prefix, id)?,
        };
        let ns = netns_name(id);
        let hv = host_veth(id);
        let pv = peer_veth(id);

        // Gives the container's netns a name (bind-mount of /proc/<pid>/ns/net).
        run("ip", &["netns", "attach", &ns, &pid.to_string()])?;
        // Creates the veth pair and moves one end into the container's netns.
        run(
            "ip",
            &["link", "add", &hv, "type", "veth", "peer", "name", &pv],
        )?;
        run("ip", &["link", "set", &pv, "netns", &ns])?;
        // Inside the container: renames to eth0, sets IP, route and brings it up.
        run("ip", &["-n", &ns, "link", "set", &pv, "name", "eth0"])?;
        run(
            "ip",
            &["-n", &ns, "addr", "add", &format!("{ip}/16"), "dev", "eth0"],
        )?;
        run("ip", &["-n", &ns, "link", "set", "eth0", "up"])?;
        run("ip", &["-n", &ns, "link", "set", "lo", "up"])?;
        run(
            "ip",
            &["-n", &ns, "route", "add", "default", "via", &net.gateway],
        )?;
        // On the host: attaches the end to this network's bridge and brings it up.
        run("ip", &["link", "set", &hv, "master", &net.bridge])?;
        run("ip", &["link", "set", &hv, "up"])?;
        // ANTI-SPOOFING (parity with the rootless path `infra.rs::do_attach`): the
        // container can only emit with ITS OWN source IP. Without this it could forge the
        // `saddr` and bypass the firewall's per-IP rules (the per-IP guarantees
        // would no longer be real). Idempotent (clears first); `insert` puts the rule at the
        // TOP of the `forward`, before the per-container jumps and the `@blocked` drops.
        clear_antispoof_root(&hv);
        run_ok(
            "nft",
            &[
                "insert", "rule", "ip", TABLE, "forward", "iifname", &hv, "ip", "saddr", "!=", &ip,
                "drop",
            ],
        );
        Ok(ip)
    }

    /// Attaches a container to a `macvlan`/`ipvlan` network: creates the sub-interface on the
    /// host (over the `parent`), moves it into the netns, sets IP (from the LAN) + route.
    fn attach_lan(&self, net: &Network, pid: i32, id: &str, ip: Option<&str>) -> Result<String> {
        let parent = net
            .parent
            .as_deref()
            .ok_or_else(|| Error::Invalid(format!("network '{}' has no parent NIC", net.name)))?;
        let ip = match ip {
            Some(want) => want.to_string(),
            None => alloc_ip_cidr(&net.subnet, id)
                .ok_or_else(|| Error::Invalid(format!("no free IP in subnet {}", net.subnet)))?,
        };
        let plen = cidr_prefix_len(&net.subnet);
        let ns = netns_name(id);
        let dev = peer_veth(id); // temporary name on the host before moving/renaming
        let (kind, mode) = if net.driver == DRIVER_IPVLAN {
            ("ipvlan", "l2")
        } else {
            ("macvlan", "bridge")
        };
        run("ip", &["netns", "attach", &ns, &pid.to_string()])?;
        // Creates the sub-interface over the parent NIC and pushes it into the netns.
        run(
            "ip",
            &[
                "link", "add", &dev, "link", parent, "type", kind, "mode", mode,
            ],
        )?;
        run("ip", &["link", "set", &dev, "netns", &ns])?;
        run("ip", &["-n", &ns, "link", "set", &dev, "name", "eth0"])?;
        run(
            "ip",
            &[
                "-n",
                &ns,
                "addr",
                "add",
                &format!("{ip}/{plen}"),
                "dev",
                "eth0",
            ],
        )?;
        run("ip", &["-n", &ns, "link", "set", "eth0", "up"])?;
        run("ip", &["-n", &ns, "link", "set", "lo", "up"])?;
        if !net.gateway.is_empty() {
            run_ok(
                "ip",
                &["-n", &ns, "route", "add", "default", "via", &net.gateway],
            );
        }
        Ok(ip)
    }

    /// Detaches a container from the network and cleans up the `veth`, the netns name and
    /// any block/publication. `ip` is the container's real IP (of a user
    /// network); if `None`, assumes the default subnet.
    pub fn detach(&self, id: &str, ip: Option<&str>) -> Result<()> {
        let ns = netns_name(id);
        let hv = host_veth(id);
        self.clear_net_rate(id); // removes any bandwidth limit (tc) from the veth
        clear_antispoof_root(&hv); // removes the anti-spoof rule from the forward (before the veth)
        run_ok("ip", &["link", "del", &hv]); // removes the veth pair
        run_ok("ip", &["netns", "del", &ns]); // removes the netns name
        let ip = ip.map(String::from).unwrap_or_else(|| alloc_ip(id));
        run_ok(
            "nft",
            &[
                "delete",
                "element",
                "ip",
                TABLE,
                "blocked",
                &format!("{{ {ip} }}"),
            ],
        );
        self.unpublish_all(&ip); // removes the DNAT rules of published ports
        self.remove_container_firewall(&ip); // removes the L4 firewall chain/jumps
        ipam::release(&ipam::prefix_of(&ip), id); // frees the IP lease for reuse
        Ok(())
    }

    /// Attaches an **already-running** container to an ADDITIONAL network (multi-homing,
    /// `docker network connect` style). Creates a new `eth<idx>` interface in the
    /// existing netns, with veths suffixed by `idx` (>=1) so as not to collide with
    /// the primary interface. Does NOT touch the default route (that belongs to the primary
    /// network). Returns the assigned IP (pinned if `Some`, otherwise derived). The netns
    /// already has a name (the primary network did it); if not, names it by the `pid`.
    pub fn attach_extra(
        &self,
        net: &Network,
        pid: i32,
        id: &str,
        ip: Option<&str>,
        idx: u32,
    ) -> Result<String> {
        self.ensure_network(net)?;
        let ip = match ip {
            Some(want) => {
                if !valid_ip_in_subnet(&net.prefix, want) {
                    return Err(Error::Invalid(format!(
                        "IP {want} outside subnet {} of network '{}'",
                        net.subnet, net.name
                    )));
                }
                ipam::reserve(&net.prefix, id, want);
                want.to_string()
            }
            None => ipam::allocate(&net.prefix, id)?,
        };
        let ns = netns_name(id);
        let hv = host_veth_n(id, idx);
        let pv = peer_veth_n(id, idx);
        let eth = format!("eth{idx}");

        // Ensures the netns has a name (idempotent: ignores "File exists").
        if !std::path::Path::new(&format!("/var/run/netns/{ns}")).exists() {
            run("ip", &["netns", "attach", &ns, &pid.to_string()])?;
        }
        run(
            "ip",
            &["link", "add", &hv, "type", "veth", "peer", "name", &pv],
        )?;
        run("ip", &["link", "set", &pv, "netns", &ns])?;
        run("ip", &["-n", &ns, "link", "set", &pv, "name", &eth])?;
        run(
            "ip",
            &["-n", &ns, "addr", "add", &format!("{ip}/16"), "dev", &eth],
        )?;
        run("ip", &["-n", &ns, "link", "set", &eth, "up"])?;
        // Attaches the host end to this network's bridge (without touching the default route).
        run("ip", &["link", "set", &hv, "master", &net.bridge])?;
        run("ip", &["link", "set", &hv, "up"])?;
        Ok(ip)
    }

    /// Detaches an ADDITIONAL interface (`docker network disconnect`): removes the
    /// `idx` veth and any publications on that network's IP. Does not touch the netns
    /// nor the primary interface. Best-effort on the veth (it may no longer exist).
    pub fn detach_extra(&self, id: &str, idx: u32, ip: &str) -> Result<()> {
        let hv = host_veth_n(id, idx);
        run_ok("ip", &["link", "del", &hv]);
        run_ok(
            "nft",
            &[
                "delete",
                "element",
                "ip",
                TABLE,
                "blocked",
                &format!("{{ {ip} }}"),
            ],
        );
        self.unpublish_all(ip);
        ipam::release(&ipam::prefix_of(ip), id); // frees the extra network's lease
        Ok(())
    }

    /// Removes a user network's infrastructure: the bridge and its
    /// nft rules (masquerade + isolation). Best-effort.
    pub fn remove_network(&self, net: &Network) -> Result<()> {
        if net.name == DEFAULT_NET {
            return Err(Error::Invalid(
                "the default 'bridge' network cannot be removed (use `network prune`)".into(),
            ));
        }
        // forward (isolation) rules that mention this bridge.
        if let Ok(out) = capture("nft", &["-a", "list", "chain", "ip", TABLE, "forward"]) {
            for line in out.lines() {
                if line.contains(&format!("\"{}\"", net.bridge)) {
                    if let Some(h) = line.rsplit("# handle ").next() {
                        run_ok(
                            "nft",
                            &["delete", "rule", "ip", TABLE, "forward", "handle", h.trim()],
                        );
                    }
                }
            }
        }
        // subnet masquerade rule.
        if let Ok(out) = capture("nft", &["-a", "list", "chain", "ip", TABLE, "postrouting"]) {
            for line in out.lines() {
                if line.contains(&net.subnet) {
                    if let Some(h) = line.rsplit("# handle ").next() {
                        run_ok(
                            "nft",
                            &[
                                "delete",
                                "rule",
                                "ip",
                                TABLE,
                                "postrouting",
                                "handle",
                                h.trim(),
                            ],
                        );
                    }
                }
            }
        }
        run_ok("ip", &["link", "del", &net.bridge]);
        Ok(())
    }

    /// Publishes a host port → `container_ip:cont_port` (DNAT), accessible from
    /// outside and via `localhost`. `spec` is `hostPort:contPort[/tcp|udp]` or just `port`.
    pub fn publish_port(&self, container_ip: &str, spec: &str) -> Result<()> {
        self.ensure_bridge()?;
        // localhost → DNAT needs route_localnet on the bridge.
        let _ = std::fs::write(
            format!("/proc/sys/net/ipv4/conf/{BRIDGE}/route_localnet"),
            "1",
        );
        let (host_port, cont_port, proto) = parse_publish(spec)?;
        let to = format!("{container_ip}:{cont_port}");
        // SAFE BY DEFAULT: the port stays only on the LOOPBACK (`output` rule below).
        // External exposure (LAN/other machines) requires explicit opt-in via
        // DELONIX_PUBLISH_ADDR ("0.0.0.0" = all interfaces; or a host IP).
        match std::env::var("DELONIX_PUBLISH_ADDR")
            .ok()
            .filter(|a| a.parse::<std::net::Ipv4Addr>().is_ok())
        {
            Some(ref ip) if ip == "0.0.0.0" => {
                run(
                    "nft",
                    &[
                        "add",
                        "rule",
                        "ip",
                        TABLE,
                        "prerouting",
                        &proto,
                        "dport",
                        &host_port,
                        "dnat",
                        "to",
                        &to,
                    ],
                )?;
            }
            Some(ip) => {
                run(
                    "nft",
                    &[
                        "add",
                        "rule",
                        "ip",
                        TABLE,
                        "prerouting",
                        "ip",
                        "daddr",
                        &ip,
                        &proto,
                        "dport",
                        &host_port,
                        "dnat",
                        "to",
                        &to,
                    ],
                )?;
            }
            None => {} // loopback-only (safe default): no DNAT on the external prerouting
        }
        // From the host itself (curl localhost:port) — always.
        run(
            "nft",
            &[
                "add",
                "rule",
                "ip",
                TABLE,
                "output",
                "ip",
                "daddr",
                "127.0.0.0/8",
                &proto,
                "dport",
                &host_port,
                "dnat",
                "to",
                &to,
            ],
        )?;
        // Hairpin: traffic coming from the loopback has to be masqueraded, otherwise the
        // container responds to 127.0.0.1 (to ITSELF) and the response never comes back.
        run(
            "nft",
            &[
                "add",
                "rule",
                "ip",
                TABLE,
                "postrouting",
                "ip",
                "saddr",
                "127.0.0.0/8",
                "ip",
                "daddr",
                container_ip,
                "masquerade",
            ],
        )?;
        Ok(())
    }

    /// Removes all publication rules (DNAT + hairpin) that mention
    /// `container_ip` (cleanup on `rm`/`detach`).
    pub fn unpublish_all(&self, container_ip: &str) {
        for chain in ["prerouting", "output", "postrouting"] {
            if let Ok(out) = capture("nft", &["-a", "list", "chain", "ip", TABLE, chain]) {
                for line in out.lines() {
                    // The `daddr <ip>` / `dnat to <ip>:` identifies this container's rules.
                    if line.contains(&format!("dnat to {container_ip}:"))
                        || line.contains(&format!("daddr {container_ip} "))
                    {
                        if let Some(handle) = line.rsplit("# handle ").next() {
                            let handle = handle.trim();
                            run_ok(
                                "nft",
                                &["delete", "rule", "ip", TABLE, chain, "handle", handle],
                            );
                        }
                    }
                }
            }
        }
    }

    /// Removes the publication of ONE host port (DNAT in prerouting+output) of a
    /// container, without touching the others or the shared hairpin. Best-effort.
    pub fn unpublish_port(&self, container_ip: &str, host_port: &str) {
        for chain in ["prerouting", "output"] {
            if let Ok(out) = capture("nft", &["-a", "list", "chain", "ip", TABLE, chain]) {
                for line in out.lines() {
                    if line.contains(&format!("dnat to {container_ip}:"))
                        && line.contains(&format!("dport {host_port} "))
                    {
                        if let Some(handle) = line.rsplit("# handle ").next() {
                            run_ok(
                                "nft",
                                &[
                                    "delete",
                                    "rule",
                                    "ip",
                                    TABLE,
                                    chain,
                                    "handle",
                                    handle.trim(),
                                ],
                            );
                        }
                    }
                }
            }
        }
    }

    /// Structured summary of the Delonix firewall (the `delonix` nft table): DNAT
    /// (published ports), blocked IPs, inter-network isolation pairs and
    /// egress masquerades. For the console's Firewall panel (#10).
    pub fn firewall_summary(&self) -> FirewallSummary {
        let mut s = FirewallSummary::default();
        // DNAT (published ports) — from the prerouting chain.
        if let Ok(out) = capture("nft", &["list", "chain", "ip", TABLE, "prerouting"]) {
            for line in out.lines() {
                let l = line.trim();
                if let Some(i) = l.find("dnat to ") {
                    let to = l[i + 8..]
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string();
                    let proto = if l.starts_with("udp") { "udp" } else { "tcp" }.to_string();
                    let dport = l
                        .split("dport ")
                        .nth(1)
                        .and_then(|x| x.split_whitespace().next())
                        .unwrap_or("")
                        .to_string();
                    if !to.is_empty() && !dport.is_empty() {
                        s.dnat.push(DnatRule {
                            proto,
                            host_port: dport,
                            to,
                        });
                    }
                }
            }
        }
        // Blocked IPs (set `blocked`).
        if let Ok(out) = capture("nft", &["list", "set", "ip", TABLE, "blocked"]) {
            if let Some(i) = out.find("elements = {") {
                let rest = &out[i + 12..];
                if let Some(j) = rest.find('}') {
                    for ip in rest[..j].split(',') {
                        let ip = ip.trim();
                        if !ip.is_empty() {
                            s.blocked.push(ip.to_string());
                        }
                    }
                }
            }
        }
        // Inter-network isolation (forward drops) + masquerades (postrouting).
        if let Ok(out) = capture("nft", &["list", "chain", "ip", TABLE, "forward"]) {
            for line in out.lines() {
                let l = line.trim();
                if l.contains("drop") && l.contains("iifname") && l.contains("oifname") {
                    let a = l
                        .split("iifname ")
                        .nth(1)
                        .and_then(|x| x.split_whitespace().next())
                        .unwrap_or("")
                        .trim_matches('"')
                        .to_string();
                    let b = l
                        .split("oifname ")
                        .nth(1)
                        .and_then(|x| x.split_whitespace().next())
                        .unwrap_or("")
                        .trim_matches('"')
                        .to_string();
                    if !a.is_empty() && !b.is_empty() {
                        s.isolation.push(format!("{a} ✗ {b}"));
                    }
                }
            }
        }
        if let Ok(out) = capture("nft", &["list", "chain", "ip", TABLE, "postrouting"]) {
            for line in out.lines() {
                let l = line.trim();
                if l.contains("masquerade") {
                    if let Some(sub) = l
                        .split("saddr ")
                        .nth(1)
                        .and_then(|x| x.split_whitespace().next())
                    {
                        s.masquerade.push(sub.to_string());
                    }
                }
            }
        }
        s
    }

    /// Ensures (once) the masquerade rule for load-balanced connections: traffic
    /// to a service VIP has to be SNAT-ed, otherwise the backend responds to the
    /// client directly (on the bridge) and the response doesn't go through the un-DNAT.
    fn ensure_vip_masq(&self) {
        if let Ok(out) = capture("nft", &["list", "chain", "ip", TABLE, "postrouting"]) {
            if out.contains(VIP_SUBNET) {
                return;
            }
        }
        let _ = apply_nft(&format!(
            "add rule ip {TABLE} postrouting ct original ip daddr {VIP_SUBNET} masquerade\n"
        ));
    }

    /// (Re)defines a service VIP's L4 load-balancing to the `backends`
    /// (per-connection round-robin via `numgen inc` + conntrack). Idempotent.
    pub fn set_service_lb(&self, vip: &str, backends: &[String]) -> Result<()> {
        self.set_service_lb_algo(vip, backends, "round-robin")
    }

    /// Like [`set_service_lb`], but with a selectable **algorithm** (C6):
    /// - `round-robin` — `numgen inc` (distributes per connection, in order).
    /// - `random` — `numgen random` (distributes randomly).
    /// - `ip-hash` / `sticky` — `jhash ip saddr` (**session affinity**: the same
    ///   client always lands on the same backend, as long as the pool doesn't change).
    /// - `weighted` — `ip:port#weight` backends (weight ≥1); repeats the backend in the map
    ///   proportionally to the weight (no weight → round-robin).
    ///
    /// nftables does the selection in the kernel (zero copy in userspace). `least-conn` is not
    /// expressible with `dnat`/`numgen` alone — it's left to the L7 path (follow-up).
    pub fn set_service_lb_algo(&self, vip: &str, backends: &[String], algo: &str) -> Result<()> {
        self.ensure_bridge()?;
        self.ensure_vip_masq();
        self.clear_service_lb(vip);
        if backends.is_empty() {
            return Ok(());
        }
        // optional weight "ip:port#weight" → expands the list for the weighted map.
        let expand = || -> Vec<String> {
            let mut out = Vec::new();
            for b in backends {
                if let Some((ipp, w)) = b.rsplit_once('#') {
                    let n: usize = w.parse().unwrap_or(1).clamp(1, 64);
                    for _ in 0..n {
                        out.push(ipp.to_string());
                    }
                } else {
                    out.push(b.clone());
                }
            }
            out
        };
        let strip = |b: &str| {
            b.rsplit_once('#')
                .map(|(a, _)| a.to_string())
                .unwrap_or_else(|| b.to_string())
        };
        let rule = if backends.len() == 1 {
            format!(
                "add rule ip {TABLE} prerouting ip daddr {vip} dnat to {}\n",
                strip(&backends[0])
            )
        } else {
            let pool = if algo == "weighted" {
                expand()
            } else {
                backends.iter().map(|b| strip(b)).collect()
            };
            let map = pool
                .iter()
                .enumerate()
                .map(|(i, ip)| format!("{i} : {ip}"))
                .collect::<Vec<_>>()
                .join(", ");
            let selector = match algo {
                "random" => format!("numgen random mod {}", pool.len()),
                "ip-hash" | "sticky" => format!("jhash ip saddr mod {}", pool.len()),
                // round-robin (default) and weighted (already expanded) use numgen inc.
                _ => format!("numgen inc mod {}", pool.len()),
            };
            format!("add rule ip {TABLE} prerouting ip daddr {vip} dnat to {selector} map {{ {map} }}\n")
        };
        apply_nft(&rule)
    }

    /// Removes a VIP's LB rule (on `down`/`scale`).
    pub fn clear_service_lb(&self, vip: &str) {
        let needle = format!("ip daddr {vip} ");
        if let Ok(out) = capture("nft", &["-a", "list", "chain", "ip", TABLE, "prerouting"]) {
            for line in out.lines() {
                if line.contains(&needle) && line.contains("dnat") {
                    if let Some(handle) = line.rsplit("# handle ").next() {
                        run_ok(
                            "nft",
                            &[
                                "delete",
                                "rule",
                                "ip",
                                TABLE,
                                "prerouting",
                                "handle",
                                handle.trim(),
                            ],
                        );
                    }
                }
            }
        }
    }

    /// Micro-segmentation (B14): denies or allows traffic BETWEEN two container
    /// IPs (in both directions), with `forward` rules. Enables
    /// `bridge-nf-call-iptables` so that the filter also sees the traffic switched
    /// on the SAME bridge (otherwise it would only catch what's forwarded between subnets).
    pub fn set_policy(&self, from_ip: &str, to_ip: &str, deny: bool) -> Result<()> {
        self.ensure_bridge()?;
        // the IP filter has to see bridged traffic (as Docker does).
        let _ = std::fs::write("/proc/sys/net/bridge/bridge-nf-call-iptables", "1");
        for (a, b) in [(from_ip, to_ip), (to_ip, from_ip)] {
            let needle = format!("ip saddr {a} ip daddr {b} ");
            let have = capture("nft", &["-a", "list", "chain", "ip", TABLE, "forward"])
                .unwrap_or_default();
            if deny {
                if !have
                    .lines()
                    .any(|l| l.contains(&needle) && l.contains("drop"))
                {
                    run(
                        "nft",
                        &[
                            "add", "rule", "ip", TABLE, "forward", "ip", "saddr", a, "ip", "daddr",
                            b, "drop",
                        ],
                    )?;
                }
            } else {
                for line in have.lines() {
                    if line.contains(&needle) && line.contains("drop") {
                        if let Some(h) = line.rsplit("# handle ").next() {
                            run_ok(
                                "nft",
                                &["delete", "rule", "ip", TABLE, "forward", "handle", h.trim()],
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Per-container firewall: blocks (or unblocks) the IP in the `blocked` `set`.
    pub fn set_egress(&self, id: &str, allow: bool) -> Result<()> {
        self.ensure_bridge()?;
        let ip = alloc_ip(id);
        let elem = format!("{{ {ip} }}");
        if allow {
            run_ok("nft", &["delete", "element", "ip", TABLE, "blocked", &elem]);
        } else {
            run("nft", &["add", "element", "ip", TABLE, "blocked", &elem])?;
        }
        Ok(())
    }

    /// Limits a container's bandwidth on the host-side `veth` (the
    /// missing piece of the "the host never dies" story: without this a container
    /// saturates the uplink/bridge and degrades the API and the host). Applies, at the SAME rate:
    /// - **DOWNLOAD** (host → container): the host transmits on the veth's *egress*,
    ///   so it's modeled with a TBF (token bucket) at the root;
    /// - **UPLOAD** (container → host): the container's traffic reaches the veth as
    ///   *ingress*; there only `police`+`drop` is possible (drops above the rate), which
    ///   is enough to prevent it from saturating the uplink.
    ///
    /// Idempotent: clears any previous qdisc before reapplying. Checked
    /// with `tc qdisc show dev <veth>` (shows the `tbf` at the root and the `ingress`).
    pub fn set_net_rate(&self, id: &str, rate: &NetRate) -> Result<()> {
        let hv = host_veth(id);
        self.clear_net_rate(id); // clean reapplication
        let r = rate.tc_rate();
        let b = rate.tc_burst();
        // DOWNLOAD: TBF on egress (the root of the host's veth).
        run(
            "tc",
            &[
                "qdisc", "add", "dev", &hv, "root", "tbf", "rate", &r, "burst", &b, "latency",
                "50ms",
            ],
        )?;
        // UPLOAD: ingress qdisc + filter that applies `police`+`drop` to everything.
        run(
            "tc",
            &["qdisc", "add", "dev", &hv, "handle", "ffff:", "ingress"],
        )?;
        run(
            "tc",
            &[
                "filter", "add", "dev", &hv, "parent", "ffff:", "protocol", "all", "prio", "1",
                "u32", "match", "u32", "0", "0", "police", "rate", &r, "burst", &b, "drop",
            ],
        )?;
        Ok(())
    }

    /// Removes the `veth`'s bandwidth limit (best-effort). Called on
    /// `detach` and before reapplying. Deleting the `veth` already takes the qdiscs with it,
    /// but we clear explicitly in case the link survives (reapplication,
    /// orphan container).
    pub fn clear_net_rate(&self, id: &str) {
        let hv = host_veth(id);
        run_ok("tc", &["qdisc", "del", "dev", &hv, "root"]);
        run_ok(
            "tc",
            &["qdisc", "del", "dev", &hv, "handle", "ffff:", "ingress"],
        );
    }

    /// Imports an `iptables-save` file: reads it, summarizes the user's intent
    /// and translates a sample to `nft` (does NOT change the host — preserves, informs).
    pub fn import_iptables(&self, path: &std::path::Path) -> Result<String> {
        let text = std::fs::read_to_string(path)?;
        let mut tables = 0usize;
        let mut chains = 0usize;
        let mut rules = 0usize;
        let mut sample: Option<String> = None;
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with('*') {
                tables += 1;
            } else if l.starts_with(':') {
                chains += 1;
            } else if l.starts_with("-A") {
                rules += 1;
                if sample.is_none() {
                    sample = Some(l.to_string());
                }
            }
        }
        let mut report = format!(
            "iptables-save: {tables} table(s), {chains} chain(s), {rules} rule(s) — intent preserved"
        );
        if let Some(rule) = sample {
            // `iptables-translate` shows the nft equivalent (dry-run).
            let args: Vec<&str> = rule.split_whitespace().collect();
            if let Ok(nft) = capture("iptables-translate", &args) {
                let nft = nft.trim();
                if !nft.is_empty() {
                    report.push_str(&format!("\n  example: {rule}\n     -> nft {nft}"));
                }
            }
        }
        Ok(report)
    }

    /// Removes all of Delonix's network infrastructure (all bridges and the
    /// nft table) — including the user networks' bridges.
    pub fn teardown(&self) -> Result<()> {
        run_ok("nft", &["delete", "table", "ip", TABLE]);
        for br in list_delonix_bridges() {
            run_ok("ip", &["link", "del", &br]);
        }
        run_ok("ip", &["link", "del", BRIDGE]);
        Ok(())
    }
}

/// Default slirp4netns IP/gateway/DNS (rootless network).
pub const SLIRP_IP: &str = "10.0.2.100";
pub const SLIRP_DNS: &str = "10.0.2.3";

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
            // Waits for the "ready" byte (network configured) before continuing.
            let mut b = [0u8; 1];
            // SAFETY: reads 1 byte from the read-end; blocks until the slirp signals.
            unsafe {
                libc::read(rd, b.as_mut_ptr() as *mut libc::c_void, 1);
                libc::close(rd);
            }
            // Publishes the ports via the api-socket (host → container, in userspace).
            if let Some(sock) = &api_sock {
                for spec in publish {
                    if let Ok((hp, cp, proto)) = parse_publish(spec) {
                        if let Err(e) = slirp_add_hostfwd(sock, &hp, &cp, &proto) {
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
) -> Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    // SAFE BY DEFAULT: binds the published port only to the loopback (127.0.0.1), not to
    // all interfaces. To expose on the LAN, explicit opt-in via
    // DELONIX_PUBLISH_ADDR (e.g.: "0.0.0.0" or a host IP). Validated as IPv4
    // so as not to inject into the slirp api-socket's JSON.
    let host_addr = std::env::var("DELONIX_PUBLISH_ADDR")
        .ok()
        .filter(|a| a.parse::<std::net::Ipv4Addr>().is_ok())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let cmd = format!(
        r#"{{"execute":"add_hostfwd","arguments":{{"proto":"{proto}","host_addr":"{host_addr}","host_port":{host_port},"guest_addr":"{SLIRP_IP}","guest_port":{guest_port}}}}}"#
    );
    let mut last = String::new();
    for _ in 0..50 {
        match UnixStream::connect(sock) {
            Ok(mut s) => {
                let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(500)));
                s.write_all(cmd.as_bytes()).map_err(|e| reg_io(&e))?;
                let mut resp = String::new();
                let _ = s.read_to_string(&mut resp);
                if resp.contains("\"error\"") {
                    return Err(Error::Runtime {
                        context: "slirp hostfwd",
                        message: format!("port {host_port}: {}", resp.trim()),
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

/// Applies an nftables *ruleset* via `nft -f -` (stdin).
fn apply_nft(ruleset: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Runtime {
            context: "spawn nft",
            message: e.to_string(),
        })?;
    child.stdin.take().unwrap().write_all(ruleset.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(Error::Runtime {
            context: "nft -f",
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// A DNAT rule (published port): `host_port`/`proto` → `to` (ip:port).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct DnatRule {
    pub proto: String,
    pub host_port: String,
    pub to: String,
}

/// Summary of the Delonix firewall (`delonix` nft table) for panel #10.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct FirewallSummary {
    /// Published ports (DNAT host → container).
    pub dnat: Vec<DnatRule>,
    /// Blocked container IPs (per-element firewall).
    pub blocked: Vec<String>,
    /// Pairs of isolated bridges (forward drop) — `"a ✗ b"`.
    pub isolation: Vec<String>,
    /// Subnets with egress masquerade.
    pub masquerade: Vec<String>,
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
        assert_eq!(r.tc_rate(), "1000000bit");
        assert_eq!(r.tc_burst(), "32768");
        assert!(parse_net_rate("1mbit", Some("0")).is_err());
        assert!(parse_net_rate("1mbit", Some("bad")).is_err());
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
}
