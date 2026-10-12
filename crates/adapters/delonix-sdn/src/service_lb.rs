//! The L4 VIP of a `kind: Service` with `type: VirtualIP` (ADR-0066).
//!
//! A `VirtualIP` Service gets ONE address, drawn from a pool the operator
//! configures (`delonix network vip-pool set`), and the holder DNATs every new
//! connection to that address over the backends that are ready, spread by
//! `numgen random`. A backend whose declared port stops accepting, or whose
//! health check is not `healthy`, leaves the rotation; one that comes back
//! re-enters it. With no ready backend the VIP refuses at once (TCP reset /
//! ICMP port-unreachable) instead of leaking through the holder's default route.
//!
//! Three halves live here:
//!
//! * **pure** — pool validation and overlap ([`validate_pool`],
//!   [`first_overlap`]), the readiness state machine ([`Tracker`]), the backend
//!   selection ([`service_backends`]) and the `nft` script ([`render_script`]).
//!   Everything that decides is testable without a holder;
//! * **host side** — the pool file, the VIP lease in the IPAM, the overlap
//!   collector (runs in the host netns, where the routes are), and the two
//!   control-socket calls ([`sync`], [`state`]);
//! * **holder side** — the prober thread in the control process
//!   ([`prober_main`]), the `svcsync` verb ([`sync_now`]) and the read-only
//!   `svc-state` verb ([`state_reply`]).
//!
//! **Where the dataplane lives**: four base chains of their own in `dlxing` —
//! `svcpre` (nat prerouting, priority -101) and `svcout` (nat output, -101) hold
//! the DNAT; `vipguard` (filter forward, -20) and `vipguardout` (filter output,
//! -20) reject whatever reaches a pool address undNATed. Own base chains rather
//! than a `jump svc` from `pre`: they are created with `add chain` (idempotent)
//! and rewritten with `flush chain` in ONE `nft -f`, so a holder built by an
//! older binary gains them on the first sync with no jump to duplicate, and no
//! packet ever sees a half-written set (ADR-0066 E10, E12).
//!
//! **Deviation from ADR-0066 D7, written down**: the ADR keeps membership
//! event-driven only. The prober must tick anyway (D8 a), and it re-reads the
//! container records on each tick it probes, so a membership change none of the
//! triggers saw (a record edited by hand, a death whose supervisor did not send
//! anything) is picked up within one interval instead of waiting for the next
//! event. The triggers still exist and are what makes a change immediate: the
//! CLI's `svcsync` on apply/delete, and `nsleave` (sent by every detach), which
//! drops the address from the map before the detach returns.

use crate::{Cidr, Error, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

/// `spec.type` of a `kind: Service`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ServiceType {
    /// ADR-0032: the name resolves to every ready backend, no VIP.
    #[default]
    #[serde(rename = "DNS")]
    Dns,
    /// ADR-0066: one VIP, DNATed by the holder to the ready backends.
    #[serde(rename = "VirtualIP")]
    VirtualIp,
}

impl ServiceType {
    /// The spelling the manifest and the CLI use.
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceType::Dns => "DNS",
            ServiceType::VirtualIp => "VirtualIP",
        }
    }
}

/// `spec.readiness.tcp` — the knobs of the TCP check that always runs for a
/// VIP's backends (ADR-0066 D8 a). Defaults: every 2 s, 500 ms timeout, one
/// success admits, two failures eject.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TcpReadiness {
    pub interval_seconds: u32,
    pub timeout_milliseconds: u32,
    pub success_threshold: u32,
    pub failure_threshold: u32,
}

impl Default for TcpReadiness {
    fn default() -> Self {
        TcpReadiness {
            interval_seconds: 2,
            timeout_milliseconds: 500,
            success_threshold: 1,
            failure_threshold: 2,
        }
    }
}

impl TcpReadiness {
    /// The bounds a manifest may ask for. An interval of 0 would spin the
    /// prober; a timeout at or above the interval would let one filtered
    /// backend delay the next round of every other one.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !(1..=300).contains(&self.interval_seconds) {
            return Err(format!(
                "readiness.tcp.intervalSeconds {} is outside 1..=300",
                self.interval_seconds
            ));
        }
        if !(10..=10_000).contains(&self.timeout_milliseconds)
            || u64::from(self.timeout_milliseconds) >= u64::from(self.interval_seconds) * 1000
        {
            return Err(format!(
                "readiness.tcp.timeoutMilliseconds {} must be 10..=10000 and below the interval \
                 ({} s)",
                self.timeout_milliseconds, self.interval_seconds
            ));
        }
        for (field, v) in [
            ("successThreshold", self.success_threshold),
            ("failureThreshold", self.failure_threshold),
        ] {
            if !(1..=10).contains(&v) {
                return Err(format!("readiness.tcp.{field} {v} is outside 1..=10"));
            }
        }
        Ok(())
    }
}

// ---- the pool -------------------------------------------------------------

/// Overrides the pool file for one process (CI, labs). A comma list.
pub const VIP_POOL_ENV: &str = "DELONIX_SERVICE_VIP_CIDR";

/// One pool CIDR as the operator may give it: strict dotted IPv4 with a
/// length, written as the prefix itself (host bits clear), inside RFC 1918 or
/// `100.64.0.0/10`, between `/16` and `/29`.
///
/// Private only: a public range would shadow real Internet addresses for every
/// container on the node. `/16` at the widest so one pool cannot swallow a
/// whole private block by typo; `/29` at the narrowest so it holds at least a
/// few addresses after the network and the first host.
pub fn validate_pool(s: &str) -> std::result::Result<Cidr, String> {
    let s = s.trim();
    let Some((addr, len)) = s.split_once('/') else {
        return Err(format!(
            "VIP pool {s:?} has no prefix length (write a.b.c.d/len)"
        ));
    };
    let ip: std::net::Ipv4Addr = addr
        .parse()
        .map_err(|_| format!("VIP pool {s:?} is not an IPv4 prefix"))?;
    if len.is_empty() || !len.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("VIP pool {s:?} has an invalid prefix length"));
    }
    let cidr = Cidr::parse(s).ok_or_else(|| format!("VIP pool {s:?} is not an IPv4 prefix"))?;
    if cidr.base != u32::from(ip) {
        return Err(format!(
            "VIP pool {s:?} has host bits set; the prefix is {}",
            cidr.to_string_cidr()
        ));
    }
    if !(16..=29).contains(&cidr.len) {
        return Err(format!(
            "VIP pool {s:?} is /{}; a pool must be between /16 and /29",
            cidr.len
        ));
    }
    let private = [
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "100.64.0.0/10",
    ]
    .iter()
    .filter_map(|p| Cidr::parse(p))
    .any(|p| p.len <= cidr.len && p.contains(cidr.base));
    if !private {
        return Err(format!(
            "VIP pool {s:?} is not private address space (RFC 1918 or 100.64.0.0/10); a public \
             range would shadow real Internet addresses for every container"
        ));
    }
    Ok(cidr)
}

/// A comma- or newline-separated list of pools, each validated, none
/// overlapping another (two pools sharing addresses would make the guard's set
/// refuse to load, and a VIP would belong to two pools).
pub fn parse_pool_list(s: &str) -> std::result::Result<Vec<Cidr>, String> {
    let mut out: Vec<Cidr> = Vec::new();
    for item in s
        .split([',', '\n'])
        .map(str::trim)
        .filter(|x| !x.is_empty())
    {
        let c = validate_pool(item)?;
        if let Some(prev) = out.iter().find(|p| p.overlaps(&c)) {
            return Err(format!(
                "VIP pools {} and {} overlap",
                prev.to_string_cidr(),
                c.to_string_cidr()
            ));
        }
        out.push(c);
    }
    Ok(out)
}

/// Where the pool in force came from — `show` says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolSource {
    /// `DELONIX_SERVICE_VIP_CIDR`.
    Env,
    /// `<root>/ingress/vip-pool`.
    File,
    /// Nothing configured.
    None,
}

/// `<root>/ingress/vip-pool`, one CIDR per line.
pub fn pool_file() -> PathBuf {
    crate::infra::ingress_dir().join("vip-pool")
}

/// The pools written in the node's file, ignoring the environment — what the
/// holder reads (the CLI's environment never reaches it).
pub fn recorded_pools() -> Vec<Cidr> {
    std::fs::read_to_string(pool_file())
        .ok()
        .and_then(|raw| parse_pool_list(&raw).ok())
        .unwrap_or_default()
}

/// The pools in force for THIS process: the environment overrides the file.
/// An invalid value is an error, never «no pool»: an operator who typed the
/// variable wrong must hear about it rather than see VIPs refused for a
/// missing pool.
pub fn configured_pools() -> Result<(Vec<Cidr>, PoolSource)> {
    if let Ok(env) = std::env::var(VIP_POOL_ENV) {
        if !env.trim().is_empty() {
            let pools = parse_pool_list(&env)
                .map_err(|e| Error::VipPoolInvalid(format!("{VIP_POOL_ENV}: {e}")))?;
            return Ok((pools, PoolSource::Env));
        }
    }
    match std::fs::read_to_string(pool_file()) {
        Ok(raw) => {
            let pools = parse_pool_list(&raw)
                .map_err(|e| Error::VipPoolInvalid(format!("{}: {e}", pool_file().display())))?;
            if pools.is_empty() {
                Ok((pools, PoolSource::None))
            } else {
                Ok((pools, PoolSource::File))
            }
        }
        Err(_) => Ok((Vec::new(), PoolSource::None)),
    }
}

/// Writes the node's pool file atomically.
pub fn write_pool_file(pools: &[Cidr]) -> Result<()> {
    let dir = crate::infra::ingress_dir();
    let _ = std::fs::create_dir_all(&dir);
    let body: String = pools
        .iter()
        .map(|c| format!("{}\n", c.to_string_cidr()))
        .collect();
    delonix_state::write_atomic(&pool_file(), body.as_bytes()).map_err(|e| Error::Command {
        context: "vip-pool",
        message: e.to_string(),
    })
}

/// Removes the node's pool file. Idempotent.
pub fn clear_pool_file() -> Result<()> {
    match std::fs::remove_file(pool_file()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Command {
            context: "vip-pool",
            message: e.to_string(),
        }),
    }
}

// ---- overlap --------------------------------------------------------------

/// An address range the engine can see from the host, with where it came from
/// — the name the refusal prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressSource {
    pub label: String,
    pub cidr: Cidr,
}

/// The first source `pool` shares an address with. Pure.
pub fn first_overlap<'a>(pool: &Cidr, sources: &'a [AddressSource]) -> Option<&'a AddressSource> {
    sources.iter().find(|s| s.cidr.overlaps(pool))
}

/// `ip -4 -o addr show` → one source per address PREFIX (`inet a.b.c.d/len`),
/// labelled with the interface. Pure.
pub fn parse_ip_addr(raw: &str) -> Vec<AddressSource> {
    raw.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let _idx = w.next()?;
            let ifname = w.next()?.trim_end_matches(':').to_string();
            let mut it = w.skip_while(|t| *t != "inet");
            it.next()?;
            let cidr = Cidr::parse(it.next()?)?;
            Some(AddressSource {
                label: format!("host address on {ifname}"),
                cidr,
            })
        })
        .collect()
}

/// `ip -4 route show table all` → one source per routed PREFIX, except the
/// default route (which would overlap everything) and host-scope noise
/// (`local`/`broadcast` entries of `table local` are the addresses themselves,
/// already covered by [`parse_ip_addr`]). Pure.
pub fn parse_ip_route(raw: &str) -> Vec<AddressSource> {
    raw.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let mut first = w.next()?;
            if matches!(
                first,
                "local" | "broadcast" | "unreachable" | "prohibit" | "blackhole" | "throw"
            ) {
                if matches!(first, "local" | "broadcast") {
                    return None;
                }
                first = w.next()?;
            }
            if first == "default" || first.contains(':') {
                return None;
            }
            let dst = if first.contains('/') {
                first.to_string()
            } else {
                format!("{first}/32")
            };
            let cidr = Cidr::parse(&dst)?;
            if cidr.len == 0 {
                return None;
            }
            let dev = l
                .split_whitespace()
                .skip_while(|t| *t != "dev")
                .nth(1)
                .unwrap_or("?");
            Some(AddressSource {
                label: format!("host route {} dev {dev}", cidr.to_string_cidr()),
                cidr,
            })
        })
        .collect()
}

/// Every `"subnet"` value anywhere in a CNI config (`ipam.subnet` and each
/// `ipam.ranges[][].subnet`), labelled with the file. Pure.
pub fn cni_subnets(label: &str, json: &serde_json::Value) -> Vec<AddressSource> {
    let mut out = Vec::new();
    fn walk(v: &serde_json::Value, label: &str, out: &mut Vec<AddressSource>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, val) in m {
                    if k == "subnet" {
                        if let Some(c) = val.as_str().and_then(Cidr::parse) {
                            out.push(AddressSource {
                                label: format!("CNI pod range in {label}"),
                                cidr: c,
                            });
                        }
                    } else {
                        walk(val, label, out);
                    }
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, label, out)),
            _ => {}
        }
    }
    walk(json, label, &mut out);
    out
}

/// `podSubnet:`/`serviceSubnet:` lines of a kind-mode cluster's
/// `kubeadm.conf`. Pure.
pub fn kubeadm_subnets(cluster: &str, raw: &str) -> Vec<AddressSource> {
    raw.lines()
        .filter_map(|l| {
            let l = l.trim();
            let (key, val) = l.split_once(':')?;
            let what = match key.trim() {
                "podSubnet" => "pod subnet",
                "serviceSubnet" => "service subnet",
                _ => return None,
            };
            let val = val.trim().trim_matches(|c| c == '"' || c == '\'');
            // A dual-stack value is a comma list; the IPv4 half is the one
            // that can collide here.
            val.split(',')
                .find_map(|v| Cidr::parse(v.trim()))
                .map(|c| AddressSource {
                    label: format!("kind cluster {cluster} {what}"),
                    cidr: c,
                })
        })
        .collect()
}

/// Every source of ADR-0066 D2 the engine can see without host privilege.
/// Runs in the HOST netns (the CLI), before talking to the holder.
pub fn collect_sources() -> Vec<AddressSource> {
    let mut out = vec![
        AddressSource {
            label: "the holder's slirp subnet".into(),
            cidr: Cidr::parse("10.0.2.0/24").expect("literal"),
        },
        AddressSource {
            label: "the default ingress network".into(),
            cidr: Cidr::parse(crate::infra::INFRA_CIDR).expect("literal"),
        },
    ];
    for def in crate::infra::network_list() {
        if let Some(c) = Cidr::parse(&def.prefix) {
            out.push(AddressSource {
                label: format!("engine network {}", def.name),
                cidr: c,
            });
        }
    }
    out.extend(parse_ip_addr(
        &crate::capture("ip", &["-4", "-o", "addr", "show"]).unwrap_or_default(),
    ));
    out.extend(parse_ip_route(
        &crate::capture("ip", &["-4", "route", "show", "table", "all"]).unwrap_or_default(),
    ));
    let conf_dir = std::env::var("CNI_CONF_DIR").unwrap_or_else(|_| "/etc/cni/net.d".into());
    if let Ok(rd) = std::fs::read_dir(&conf_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if let Some(v) = std::fs::read(&p)
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            {
                out.extend(cni_subnets(&p.display().to_string(), &v));
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(crate::infra::base_root().join("clusters")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Ok(raw) = std::fs::read_to_string(e.path().join("kubeadm.conf")) {
                out.extend(kubeadm_subnets(&name, &raw));
            }
        }
    }
    out
}

/// Refuses a pool list that shares an address with anything the node routes
/// or serves (exit 5, naming the source).
pub fn check_pools_against(pools: &[Cidr], sources: &[AddressSource]) -> Result<()> {
    for p in pools {
        if let Some(src) = first_overlap(p, sources) {
            return Err(Error::VipPoolOverlap(format!(
                "VIP pool {} overlaps {} ({}); pick a range nothing on this node uses",
                p.to_string_cidr(),
                src.label,
                src.cidr.to_string_cidr()
            )));
        }
    }
    Ok(())
}

// ---- the VIP lease --------------------------------------------------------

/// The IPAM id of a Service's VIP. Never a container id (they are hex), so
/// `prune::lease_owners` can tell the two apart.
pub fn lease_id(namespace: &str, name: &str) -> String {
    format!("svc:{namespace}/{name}")
}

/// Is `id` a VIP lease id ([`lease_id`])?
pub fn is_lease_id(id: &str) -> bool {
    id.starts_with("svc:")
}

/// Allocates (or keeps) the VIP of a Service. An explicit `requested` address
/// is reserved — refused when it is outside every pool or not a usable host of
/// it, and a conflict when another Service holds it. Without one, the existing
/// lease is kept if it is still inside a pool; otherwise the first pool with a
/// free address gives one.
pub fn allocate_vip(
    namespace: &str,
    name: &str,
    requested: Option<&str>,
    pools: &[Cidr],
) -> Result<String> {
    let id = lease_id(namespace, name);
    let current: Option<(Cidr, String)> = pools
        .iter()
        .find_map(|p| crate::ipam::lookup(&p.to_string_cidr(), &id).map(|ip| (*p, ip)));
    if let Some(want) = requested {
        let addr = want.parse::<std::net::Ipv4Addr>().map_err(|_| {
            Error::VipOutsidePool(format!("spec.vip {want:?} is not an IPv4 address"))
        })?;
        let want = addr.to_string();
        let Some(pool) = pools.iter().find(|p| p.contains(u32::from(addr))) else {
            return Err(Error::VipOutsidePool(format!(
                "spec.vip {want} is outside the VIP pool ({})",
                pool_list_text(pools)
            )));
        };
        if !crate::valid_ip_in_subnet(&pool.to_string_cidr(), &want) {
            return Err(Error::VipOutsidePool(format!(
                "spec.vip {want} is the network, first or broadcast address of the pool {}",
                pool.to_string_cidr()
            )));
        }
        if let Some((p, ip)) = &current {
            if *ip == want {
                return Ok(want);
            }
            crate::ipam::release(&p.to_string_cidr(), &id);
        }
        crate::ipam::reserve(&pool.to_string_cidr(), &id, &want).map_err(|e| match e {
            Error::IpInUse(m) => Error::IpInUse(format!("spec.vip: {m}")),
            other => other,
        })?;
        return Ok(want);
    }
    if let Some((_, ip)) = current {
        return Ok(ip);
    }
    let mut last = None;
    for p in pools {
        match crate::ipam::allocate(&p.to_string_cidr(), &id) {
            Ok(ip) => return Ok(ip),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        Error::VipPoolUnconfigured(
            "no VIP pool is configured on this node; set one with `delonix network vip-pool \
             set <cidr>`"
                .into(),
        )
    }))
}

/// Frees a Service's VIP lease, wherever it is. Idempotent.
pub fn release_vip(namespace: &str, name: &str) {
    crate::ipam::release_everywhere(&lease_id(namespace, name));
}

fn pool_list_text(pools: &[Cidr]) -> String {
    pools
        .iter()
        .map(|c| c.to_string_cidr())
        .collect::<Vec<_>>()
        .join(", ")
}

// ---- backends and readiness ----------------------------------------------

/// What the selector and the readiness gate need from one container record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub namespace: String,
    pub ip: String,
    pub labels: BTreeMap<String, String>,
    /// `Running`, and `healthy` when it declares a health check
    /// (ADR-0066 D8 b; `Starting` and `Unhealthy` are out).
    pub serving: bool,
    /// It declares a health check — decides `healthy` vs `ready (tcp)`.
    pub has_health_check: bool,
    /// The last health verdict (`starting`, `healthy`, `unhealthy`), when it
    /// declares a check — what `describe` says a gated-out backend is waiting
    /// for.
    pub health: Option<String>,
    /// `Running` — a gated-out backend is still listed, with why it is out.
    pub running: bool,
}

/// THE evaluation of a Service's selector: same namespace, every label
/// matching, serving. The DNS index and the dataplane both call this; there is
/// no second reading of `matchLabels` (ADR-0066 D7).
/// The members a Service would select if they were serving: the same
/// namespace and labels, `Running`. What `describe` lists, so a backend the
/// health gate keeps out is shown with its reason instead of vanishing.
pub fn service_candidates<'a>(
    namespace: &str,
    match_labels: &BTreeMap<String, String>,
    members: &'a [Member],
) -> Vec<&'a Member> {
    members
        .iter()
        .filter(|m| {
            m.running
                && m.namespace.eq_ignore_ascii_case(namespace)
                && crate::matches_labels(&m.labels, match_labels)
        })
        .collect()
}

pub fn service_backends<'a>(
    namespace: &str,
    match_labels: &BTreeMap<String, String>,
    members: &'a [Member],
) -> Vec<&'a Member> {
    members
        .iter()
        .filter(|m| {
            m.serving
                && m.namespace.eq_ignore_ascii_case(namespace)
                && crate::matches_labels(&m.labels, match_labels)
        })
        .collect()
}

/// Readiness of one backend of one Service.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProbeState {
    ready: bool,
    successes: u32,
    failures: u32,
}

/// The TCP readiness state machine (ADR-0066 D8 a). Starts NOT ready —
/// fail-closed after a control restart: a backend is admitted on its first
/// `success_threshold` consecutive successes, ejected on `failure_threshold`
/// consecutive failures.
#[derive(Debug, Default)]
pub struct Tracker {
    states: HashMap<(String, String), ProbeState>,
}

impl Tracker {
    /// Records one probe of `ip` for the Service `key`; returns whether it is
    /// ready now.
    pub fn observe(&mut self, key: &str, ip: &str, ok: bool, cfg: &TcpReadiness) -> bool {
        let st = self
            .states
            .entry((key.to_string(), ip.to_string()))
            .or_default();
        if ok {
            st.failures = 0;
            st.successes = st.successes.saturating_add(1);
            if st.successes >= cfg.success_threshold.max(1) {
                st.ready = true;
            }
        } else {
            st.successes = 0;
            st.failures = st.failures.saturating_add(1);
            if st.failures >= cfg.failure_threshold.max(1) {
                st.ready = false;
            }
        }
        st.ready
    }

    /// Is `ip` currently ready for `key`? Unknown is not ready.
    pub fn is_ready(&self, key: &str, ip: &str) -> bool {
        self.states
            .get(&(key.to_string(), ip.to_string()))
            .is_some_and(|s| s.ready)
    }

    /// Forgets every backend no longer a candidate, so an address that comes
    /// back later (a re-used lease) starts not ready again.
    pub fn retain(&mut self, live: &HashSet<(String, String)>) {
        self.states.retain(|k, _| live.contains(k));
    }
}

// ---- the script -----------------------------------------------------------

/// The base chains this module owns in `dlxing`.
pub const SVC_PRE_CHAIN: &str = "svcpre";
pub const SVC_OUT_CHAIN: &str = "svcout";
pub const VIP_GUARD_CHAIN: &str = "vipguard";
pub const VIP_GUARD_OUT_CHAIN: &str = "vipguardout";

/// One served VIP: its port and the READY backend addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VipPlan {
    pub vip: String,
    pub port: u16,
    pub backends: Vec<String>,
}

fn strict_v4(s: &str) -> bool {
    s.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|a| a.to_string() == s)
}

/// The whole `nft -f` transaction: create the four chains if absent, flush
/// them, write one DNAT per VIP with at least one ready backend (in `svcpre`
/// and `svcout`), and reject every address of `guard` that no DNAT rewrote
/// (ADR-0066 D3). An address that is not a strict dotted IPv4 is dropped here,
/// before it can reach the script. Pure.
pub fn render_script(table: &str, plans: &[VipPlan], guard: &[Cidr]) -> String {
    let mut s = String::new();
    for (chain, hook, kind, prio) in [
        (SVC_PRE_CHAIN, "prerouting", "nat", -101),
        (SVC_OUT_CHAIN, "output", "nat", -101),
        (VIP_GUARD_CHAIN, "forward", "filter", -20),
        (VIP_GUARD_OUT_CHAIN, "output", "filter", -20),
    ] {
        s.push_str(&format!(
            "add chain ip {table} {chain} {{ type {kind} hook {hook} priority {prio}; }}\n\
             flush chain ip {table} {chain}\n"
        ));
    }
    for p in plans {
        let backends: Vec<&String> = p.backends.iter().filter(|b| strict_v4(b)).collect();
        if !strict_v4(&p.vip) || backends.is_empty() || p.port == 0 {
            continue;
        }
        let map = backends
            .iter()
            .enumerate()
            .map(|(i, b)| format!("{i} : {b} . {}", p.port))
            .collect::<Vec<_>>()
            .join(", ");
        for chain in [SVC_PRE_CHAIN, SVC_OUT_CHAIN] {
            s.push_str(&format!(
                "add rule ip {table} {chain} ip daddr {} tcp dport {} counter dnat ip to numgen \
                 random mod {} map {{ {map} }}\n",
                p.vip,
                p.port,
                backends.len()
            ));
        }
    }
    if !guard.is_empty() {
        let set = guard
            .iter()
            .map(|c| {
                if c.len == 32 {
                    Cidr::fmt_u32(c.base)
                } else {
                    c.to_string_cidr()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        for chain in [VIP_GUARD_CHAIN, VIP_GUARD_OUT_CHAIN] {
            s.push_str(&format!(
                "add rule ip {table} {chain} ip daddr {{ {set} }} meta l4proto tcp counter reject \
                 with tcp reset\n\
                 add rule ip {table} {chain} ip daddr {{ {set} }} counter reject with icmp type \
                 port-unreachable\n"
            ));
        }
    }
    s
}

/// The guard set: every pool, plus each VIP that no pool contains (a pool
/// given only through the CLI's environment never reaches the holder, but the
/// VIPs drawn from it are in their records). Pure; no two elements overlap,
/// which an anonymous `nft` set refuses.
pub fn guard_set(pools: &[Cidr], vips: &[String]) -> Vec<Cidr> {
    let mut out: Vec<Cidr> = Vec::new();
    for p in pools {
        if !out.iter().any(|o| o.overlaps(p)) {
            out.push(*p);
        }
    }
    let mut seen = BTreeSet::new();
    for v in vips {
        let Some(a) = Cidr::parse_addr(v) else {
            continue;
        };
        if !strict_v4(v) || !seen.insert(a) {
            continue;
        }
        if !out.iter().any(|o| o.contains(a)) {
            out.push(Cidr { base: a, len: 32 });
        }
    }
    out
}

// ---- what `svc-state` reports ---------------------------------------------

/// One backend as the holder sees it right now.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackendState {
    pub name: String,
    pub ip: String,
    /// `healthy` (a health check passes and the port accepts), `ready (tcp)`
    /// (no health check, the port accepts), or `not ready`.
    pub state: String,
}

/// One `VirtualIP` Service as the holder serves it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServiceState {
    pub namespace: String,
    pub name: String,
    pub vip: String,
    pub port: u16,
    pub backends: Vec<BackendState>,
}

impl ServiceState {
    /// How many backends are in rotation.
    pub fn ready(&self) -> usize {
        self.backends
            .iter()
            .filter(|b| !b.state.starts_with("not ready"))
            .count()
    }
}

/// Asks the holder for what it serves. `Err` = could not ask (holder down or
/// older than this build) — never read as «nothing ready».
pub fn state() -> Result<Vec<ServiceState>> {
    let body = crate::infra::control_query("svc-state")?;
    let raw = crate::infra::hex_decode(body.trim())
        .ok_or_else(|| Error::InvalidHex("svc-state: the holder's reply is not hex".into()))?;
    serde_json::from_slice(&raw).map_err(|e| Error::Command {
        context: "svc-state",
        message: e.to_string(),
    })
}

/// Tells the holder to re-evaluate and rewrite the VIP dataplane now. `Ok`
/// with the holder down: the next control start syncs from the records.
pub fn sync() -> Result<()> {
    if crate::infra::status().holder_pid.is_none() {
        return Ok(());
    }
    crate::infra::control_query("svcsync").map(|_| ())
}

// ---- holder side ----------------------------------------------------------

#[derive(Default)]
struct Runtime {
    tracker: Tracker,
    /// When each Service is next due a probe round.
    next_due: HashMap<String, std::time::Instant>,
    /// The script last applied; `None` until the first apply.
    applied: Option<String>,
    snapshot: Vec<ServiceState>,
    /// Addresses whose workload left the wire (`nsleave`), kept out of every
    /// map until something attaches on them again.
    gone: HashSet<String>,
}

fn runtime() -> &'static std::sync::Mutex<Runtime> {
    static R: std::sync::OnceLock<std::sync::Mutex<Runtime>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

fn wake() -> &'static (std::sync::Mutex<bool>, std::sync::Condvar) {
    static W: std::sync::OnceLock<(std::sync::Mutex<bool>, std::sync::Condvar)> =
        std::sync::OnceLock::new();
    W.get_or_init(Default::default)
}

/// Wakes the prober without waiting for it.
pub(crate) fn kick() {
    let (m, cv) = wake();
    *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
    cv.notify_all();
}

/// An address left the wire (`nsleave`): out of every map NOW, before the
/// detach returns — the next probe round would only eject it after
/// `failure_threshold` failures.
pub(crate) fn forget_address(ip: &str) {
    {
        let mut rt = runtime().lock().unwrap_or_else(|e| e.into_inner());
        if rt.snapshot.is_empty() && rt.applied.is_none() {
            return;
        }
        rt.gone.insert(ip.to_string());
    }
    let _ = sync_now(false);
}

/// An address was attached again: it may serve.
pub(crate) fn address_back(ip: &str) {
    let mut rt = runtime().lock().unwrap_or_else(|e| e.into_inner());
    if rt.gone.remove(ip) {
        drop(rt);
        kick();
    }
}

/// One TCP connect from the holder netns: «something listens there».
fn probe(ip: &str, port: u16, timeout: std::time::Duration) -> bool {
    let Ok(addr) = format!("{ip}:{port}").parse::<std::net::SocketAddr>() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&addr, timeout).is_ok()
}

/// Reads every container record the way the DNS index does.
pub(crate) fn read_members() -> Vec<Member> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(crate::infra::base_root().join("containers")) else {
        return out;
    };
    for e in rd.flatten() {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(e.path()).unwrap_or_default(),
        ) else {
            continue;
        };
        let (Some(name), Some(ip)) = (v["name"].as_str(), v["ip"].as_str()) else {
            continue;
        };
        if !strict_v4(ip) {
            continue;
        }
        let labels = v["labels"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let has_health_check = !v["health"].is_null();
        let health = v["health_state"]["health"].as_str().map(str::to_string);
        out.push(Member {
            name: name.to_string(),
            namespace: v["namespace"].as_str().unwrap_or("default").to_string(),
            ip: ip.to_string(),
            labels,
            serving: crate::infra::service_member_ready(
                v["status"].as_str(),
                has_health_check,
                v["health_state"]["health"].as_str(),
            ),
            has_health_check,
            health,
            running: v["status"].as_str() == Some("Running"),
        });
    }
    out
}

/// One round: re-read the Services and the members, probe the due ones, and
/// rewrite the dataplane if what it serves changed. `force_probe` probes every
/// Service now (a `svcsync` from an apply), not only the due ones.
pub(crate) fn sync_now(force_probe: bool) -> Result<()> {
    // One round at a time: the prober thread and the control worker both call
    // this, and two interleaved rewrites could apply an older plan last.
    static ROUND: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _round = ROUND.lock().unwrap_or_else(|e| e.into_inner());

    let defs: Vec<crate::infra::ServiceDef> = crate::infra::service_list()
        .into_iter()
        .filter(|d| d.service_type == ServiceType::VirtualIp && d.vip.is_some())
        .collect();
    let pools = recorded_pools();
    {
        let rt = runtime().lock().unwrap_or_else(|e| e.into_inner());
        if defs.is_empty() && pools.is_empty() && rt.applied.is_none() {
            return Ok(()); // never served a VIP: leave the table alone
        }
    }
    // Hairpin needs the bridged reply to go through conntrack (ADR-0066 D5);
    // writable by the holder's root, and a fresh netns already has it at 1.
    if !defs.is_empty() {
        let _ = std::fs::write("/proc/sys/net/bridge/bridge-nf-call-iptables", "1");
    }
    let members = read_members();
    let now = std::time::Instant::now();

    // Who to probe this round, decided under the lock; probed without it.
    struct Job {
        key: String,
        ip: String,
        port: u16,
        cfg: TcpReadiness,
    }
    let mut jobs: Vec<Job> = Vec::new();
    let gone: HashSet<String>;
    {
        let mut rt = runtime().lock().unwrap_or_else(|e| e.into_inner());
        gone = rt.gone.clone();
        for d in &defs {
            let key = format!("{}/{}", d.namespace, d.name);
            let cfg = d.readiness.unwrap_or_default();
            let due = force_probe || rt.next_due.get(&key).is_none_or(|t| *t <= now);
            if !due {
                continue;
            }
            rt.next_due.insert(
                key.clone(),
                now + std::time::Duration::from_secs(cfg.interval_seconds.max(1).into()),
            );
            for m in service_backends(&d.namespace, &d.match_labels, &members) {
                if gone.contains(&m.ip) {
                    continue;
                }
                jobs.push(Job {
                    key: key.clone(),
                    ip: m.ip.clone(),
                    port: d.port,
                    cfg,
                });
            }
        }
    }
    // Parallel, capped: a filtered backend's timeout must not delay the others
    // (ADR-0066 E13), and a node with hundreds of backends must not spawn
    // hundreds of threads at once.
    let mut results: Vec<(String, String, bool, TcpReadiness)> = Vec::new();
    for chunk in jobs.chunks(16) {
        let r: Vec<_> = std::thread::scope(|s| {
            let hs: Vec<_> = chunk
                .iter()
                .map(|j| {
                    s.spawn(move || {
                        let ok = probe(
                            &j.ip,
                            j.port,
                            std::time::Duration::from_millis(j.cfg.timeout_milliseconds.into()),
                        );
                        (j.key.clone(), j.ip.clone(), ok, j.cfg)
                    })
                })
                .collect();
            hs.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        results.extend(r);
    }

    let mut rt = runtime().lock().unwrap_or_else(|e| e.into_inner());
    for (key, ip, ok, cfg) in &results {
        rt.tracker.observe(key, ip, *ok, cfg);
    }
    let mut live: HashSet<(String, String)> = HashSet::new();
    let mut plans = Vec::new();
    let mut snapshot = Vec::new();
    let mut vips = Vec::new();
    for d in &defs {
        let key = format!("{}/{}", d.namespace, d.name);
        let vip = d.vip.clone().unwrap_or_default();
        vips.push(vip.clone());
        let mut ready = Vec::new();
        let mut backends = Vec::new();
        for m in service_candidates(&d.namespace, &d.match_labels, &members) {
            // A gated-out backend loses its TCP history: it re-enters only on a
            // fresh probe success after its health check passes again.
            if m.serving {
                live.insert((key.clone(), m.ip.clone()));
            }
            let in_rotation =
                m.serving && !gone.contains(&m.ip) && rt.tracker.is_ready(&key, &m.ip);
            if in_rotation {
                ready.push(m.ip.clone());
            }
            backends.push(BackendState {
                name: m.name.clone(),
                ip: m.ip.clone(),
                state: match (in_rotation, m.has_health_check) {
                    (true, true) => "healthy".to_string(),
                    (true, false) => "ready (tcp)".to_string(),
                    (false, true) if !m.serving => format!(
                        "not ready (health: {})",
                        m.health.as_deref().unwrap_or("starting")
                    ),
                    (false, _) => "not ready".to_string(),
                },
            });
        }
        ready.sort();
        plans.push(VipPlan {
            vip: vip.clone(),
            port: d.port,
            backends: ready,
        });
        snapshot.push(ServiceState {
            namespace: d.namespace.clone(),
            name: d.name.clone(),
            vip,
            port: d.port,
            backends,
        });
    }
    rt.tracker.retain(&live);
    let keys: HashSet<String> = defs
        .iter()
        .map(|d| format!("{}/{}", d.namespace, d.name))
        .collect();
    rt.next_due.retain(|k, _| keys.contains(k));
    rt.snapshot = snapshot;
    let script = render_script(
        crate::infra::INGRESS_TABLE,
        &plans,
        &guard_set(&pools, &vips),
    );
    if rt.applied.as_deref() == Some(script.as_str()) {
        return Ok(());
    }
    crate::infra::apply_nft_stdin(&script)?;
    rt.applied = Some(script);
    Ok(())
}

/// The `svc-state` reply: the last round's snapshot, hex-encoded JSON.
pub(crate) fn state_reply() -> String {
    let rt = runtime().lock().unwrap_or_else(|e| e.into_inner());
    let json = serde_json::to_vec(&rt.snapshot).unwrap_or_else(|_| b"[]".to_vec());
    crate::infra::hex_encode(&json)
}

/// The prober thread of the control process (ADR-0066 D8 a). Sleeps until the
/// next Service is due or something kicks it; does nothing on a node that has
/// never served a VIP. Lives exactly as long as the control process.
pub(crate) fn prober_main() {
    let _ = sync_now(true);
    loop {
        let (m, cv) = wake();
        let guard = m.lock().unwrap_or_else(|e| e.into_inner());
        let (mut kicked, _) = cv
            .wait_timeout_while(guard, std::time::Duration::from_millis(500), |k| !*k)
            .unwrap_or_else(|e| e.into_inner());
        *kicked = false;
        drop(kicked);
        if let Err(e) = sync_now(false) {
            tracing::warn!(error = %e, "VIP dataplane sync failed; retrying next round");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Cidr {
        Cidr::parse(s).unwrap()
    }

    #[test]
    fn a_pool_is_private_ipv4_between_16_and_29() {
        assert!(validate_pool("10.90.0.0/16").is_ok());
        assert!(validate_pool("192.168.77.0/29").is_ok());
        assert!(validate_pool("100.64.8.0/24").is_ok());
        for bad in [
            "10.90.0.0",
            "10.90.0.0/8",
            "10.90.0.0/30",
            "8.8.8.0/24",
            "10.90.0.1/16",
            "fd00::/64",
            "10.90.0.0/+16",
            "172.32.0.0/16",
        ] {
            assert!(validate_pool(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn two_overlapping_pools_are_refused() {
        assert!(parse_pool_list("10.90.0.0/16,10.90.4.0/24").is_err());
        assert_eq!(
            parse_pool_list("10.90.0.0/16, 10.91.0.0/24\n")
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn the_overlap_names_each_kind_of_source() {
        let addr = parse_ip_addr(
            "2: wlp3s0    inet 192.168.1.106/24 brd 192.168.1.255 scope global dynamic wlp3s0\n\
             5: wg0    inet 10.66.0.2/32 scope global wg0\n",
        );
        let routes = parse_ip_route(
            "default via 192.168.1.1 dev wlp3s0 proto dhcp metric 600\n\
             10.66.0.0/24 dev wg0 scope link\n\
             172.17.0.0/16 dev docker0 proto kernel scope link src 172.17.0.1 linkdown\n\
             local 192.168.1.106 dev wlp3s0 table local proto kernel scope host src 192.168.1.106\n\
             broadcast 192.168.1.255 dev wlp3s0 table local\n\
             fd00::/64 dev x\n",
        );
        let cni = cni_subnets(
            "/etc/cni/net.d/10-bridge.conflist",
            &serde_json::json!({"plugins":[{"ipam":{"ranges":[[{"subnet":"10.244.0.0/24"}]]}}]}),
        );
        let kind = kubeadm_subnets(
            "dev",
            "networking:\n  podSubnet: \"10.245.0.0/16\"\n  serviceSubnet: 10.96.0.0/16\n",
        );
        let mut all = Vec::new();
        all.extend(addr);
        all.extend(routes);
        all.extend(cni);
        all.extend(kind);
        all.push(AddressSource {
            label: "engine network lab".into(),
            cidr: c("10.233.0.0/16"),
        });
        for (pool, want) in [
            ("192.168.1.0/29", "host address on wlp3s0"),
            ("10.66.0.0/29", "host address on wg0"),
            ("172.17.4.0/24", "host route 172.17.0.0/16 dev docker0"),
            (
                "10.244.0.0/29",
                "CNI pod range in /etc/cni/net.d/10-bridge.conflist",
            ),
            ("10.245.9.0/24", "kind cluster dev pod subnet"),
            ("10.96.0.0/24", "kind cluster dev service subnet"),
            ("10.233.7.0/24", "engine network lab"),
        ] {
            let got = first_overlap(&c(pool), &all).map(|s| s.label.clone());
            assert_eq!(got.as_deref(), Some(want), "{pool}");
        }
        assert!(first_overlap(&c("10.90.0.0/16"), &all).is_none());
        // The default route never counts: it would overlap everything.
        assert!(!all.iter().any(|s| s.cidr.len == 0));
    }

    #[test]
    fn no_ready_backend_means_no_dnat_and_the_pool_in_the_guard() {
        let s = render_script(
            "dlxing",
            &[VipPlan {
                vip: "10.90.0.10".into(),
                port: 80,
                backends: vec![],
            }],
            &[c("10.90.0.0/16")],
        );
        assert!(!s.contains("dnat"), "{s}");
        assert!(s.contains("add rule ip dlxing vipguard ip daddr { 10.90.0.0/16 } meta l4proto tcp counter reject with tcp reset"), "{s}");
        assert!(s.contains("vipguardout ip daddr { 10.90.0.0/16 } counter reject with icmp type port-unreachable"), "{s}");
        assert!(s.contains("flush chain ip dlxing svcpre"), "{s}");
    }

    #[test]
    fn n_ready_backends_make_a_map_of_n_with_random_not_inc() {
        let s = render_script(
            "dlxing",
            &[VipPlan {
                vip: "10.90.0.10".into(),
                port: 8080,
                backends: vec!["10.233.0.11".into(), "10.233.0.12".into()],
            }],
            &[c("10.90.0.0/16")],
        );
        let want = "ip daddr 10.90.0.10 tcp dport 8080 counter dnat ip to numgen random mod 2 map { 0 : 10.233.0.11 . 8080, 1 : 10.233.0.12 . 8080 }";
        assert!(
            s.contains(&format!("add rule ip dlxing svcpre {want}")),
            "{s}"
        );
        assert!(
            s.contains(&format!("add rule ip dlxing svcout {want}")),
            "{s}"
        );
        assert!(!s.contains("numgen inc"));
    }

    #[test]
    fn an_address_that_is_not_strict_ipv4_never_reaches_the_script() {
        let s = render_script(
            "dlxing",
            &[VipPlan {
                vip: "10.90.0.10".into(),
                port: 80,
                backends: vec!["10.233.0.11; flush ruleset".into(), "10.233.0.12".into()],
            }],
            &[],
        );
        assert!(!s.contains("flush ruleset"), "{s}");
        assert!(s.contains("mod 1 map { 0 : 10.233.0.12 . 80 }"), "{s}");
    }

    #[test]
    fn the_guard_holds_each_pool_and_only_the_vips_outside_them() {
        let g = guard_set(
            &[c("10.90.0.0/16")],
            &["10.90.4.20".into(), "10.91.0.5".into(), "10.91.0.5".into()],
        );
        assert_eq!(g, vec![c("10.90.0.0/16"), c("10.91.0.5/32")]);
        let s = render_script("dlxing", &[], &g);
        assert!(s.contains("{ 10.90.0.0/16, 10.91.0.5 }"), "{s}");
    }

    fn member(name: &str, ns: &str, ip: &str, app: &str, serving: bool) -> Member {
        Member {
            name: name.into(),
            namespace: ns.into(),
            ip: ip.into(),
            labels: [("app".to_string(), app.to_string())].into(),
            serving,
            has_health_check: false,
            health: None,
            running: true,
        }
    }

    #[test]
    fn backends_are_the_serving_members_of_the_namespace_that_match() {
        let ms = vec![
            member("a", "teamA", "10.233.0.11", "web", true),
            member("b", "teamA", "10.233.0.12", "web", false), // Starting / Unhealthy
            member("c", "teamB", "10.233.0.13", "web", true),
            member("d", "teamA", "10.233.0.14", "db", true),
        ];
        let sel: BTreeMap<String, String> = [("app".to_string(), "web".to_string())].into();
        let got: Vec<&str> = service_backends("teamA", &sel, &ms)
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(got, vec!["a"]);
        assert!(
            service_backends("teamA", &BTreeMap::new(), &ms).is_empty(),
            "an empty selector selects nothing (fail-closed, `matches_labels`)"
        );
    }

    #[test]
    fn readiness_starts_closed_admits_on_success_and_ejects_on_failures() {
        let cfg = TcpReadiness::default();
        let mut t = Tracker::default();
        assert!(!t.is_ready("s", "ip"), "unknown is not ready (fail-closed)");
        assert!(t.observe("s", "ip", true, &cfg), "one success admits");
        assert!(
            t.observe("s", "ip", false, &cfg),
            "one failure is not enough"
        );
        assert!(!t.observe("s", "ip", false, &cfg), "two failures eject");
        assert!(t.observe("s", "ip", true, &cfg));
        let mut live = HashSet::new();
        live.insert(("other".to_string(), "x".to_string()));
        t.retain(&live);
        assert!(
            !t.is_ready("s", "ip"),
            "a forgotten backend starts closed again"
        );
    }

    #[test]
    fn readiness_knobs_are_bounded() {
        assert!(TcpReadiness::default().validate().is_ok());
        let mut r = TcpReadiness {
            interval_seconds: 0,
            ..Default::default()
        };
        assert!(r.validate().is_err());
        r.interval_seconds = 1;
        r.timeout_milliseconds = 1000;
        assert!(r.validate().is_err(), "timeout at the interval is refused");
        r.timeout_milliseconds = 500;
        r.failure_threshold = 0;
        assert!(r.validate().is_err());
    }

    #[test]
    fn a_vip_lease_id_is_never_a_container_id() {
        assert!(is_lease_id(&lease_id("teamA", "web")));
        assert!(!is_lease_id("0123abcd"));
        assert!(!is_lease_id("pod-web"));
    }
}
