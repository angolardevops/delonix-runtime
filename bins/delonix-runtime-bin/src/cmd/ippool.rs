//! `kind: IPPool` — a set of host addresses that routes can CLAIM (ADR-0046, D3).
//!
//! A pool is a reservation ledger, nothing more: it says which addresses exist and who
//! holds each one. A claimant (today an `HTTPRoute` with `spec.pool`) gets one address,
//! kept for as long as the claimant asks for it; the same claimant asking twice gets the
//! same address, and an address has one holder at a time.
//!
//! **`announce: local` is the only mode built.** The address must ALREADY be on an
//! interface of this host — the engine does not add it. `apply` checks that (by binding
//! it) and stops when it is missing, saying so, instead of publishing on an address
//! nobody can reach. `announce: l2` (adding it and announcing with ARP) needs root and is
//! refused until it is built.
//!
//! **The ledger is observable before anything reclaims from it.** `get ippools` and
//! `describe ippools` show every lease. Leases are released when the claimant is no longer
//! declared (an `apply` without the route, `httproute rm`), never by a sweep that guesses
//! who is alive — the IPAM held 391 leases for 47 live workloads for exactly that reason.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use super::kinds as k;
use super::manifest::{self, ManifestDoc};
use super::output::OutputFormat;
use delonix_model::{Error, Result};
use serde::{Deserialize, Serialize};

/// Most addresses one pool may hold. A pool is expanded lazily to check a claim, and a
/// `/8` is a typo more often than a plan.
const MAX_ADDRESSES: u64 = 65_536;

/// `spec` of `kind: IPPool`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct IpPoolSpec {
    /// Addresses the pool owns: a single address, a range `a.b.c.d-e.f.g.h`, or a CIDR.
    /// A CIDR wider than /31 leaves out its network and broadcast addresses.
    #[serde(default)]
    pub addresses: Vec<String>,
    /// How a claimed address is made reachable. `local` (default): it is already on an
    /// interface of this host. `l2` is planned (ADR-0046 phase 4) and refused.
    #[serde(default)]
    pub announce: Option<String>,
    /// Interface `announce: l2` would add the address to. Only valid with `l2`.
    #[serde(default)]
    pub interface: Option<String>,
}

pub const IPPOOL_SPEC_FIELDS: &[&str] = &["addresses", "announce", "interface"];
pub const RECONCILED_IPPOOL_FIELDS: &[&str] = &["addresses", "announce", "interface"];

/// One pool as persisted: its definition and its leases in ONE file, so a claim and a
/// definition change are serialised by the same lock and cannot disagree.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PoolFile {
    pub name: String,
    pub addresses: Vec<String>,
    pub announce: String,
    #[serde(default)]
    pub interface: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    /// claimant → address.
    #[serde(default)]
    pub leases: BTreeMap<String, String>,
}

fn dir() -> std::path::PathBuf {
    super::util::state_root().join("ippool")
}

fn path_in(d: &std::path::Path, name: &str) -> std::path::PathBuf {
    d.join(format!("{name}.json"))
}

/// Exclusive lock over the whole ledger. One file for every pool: claims are rare and
/// short, and a per-pool lock would leave "the same claimant in two pools" unguarded.
struct Lock(std::fs::File);

impl Lock {
    fn acquire(d: &std::path::Path) -> Result<Lock> {
        use std::os::unix::io::AsRawFd;
        std::fs::create_dir_all(d).map_err(|e| Error::Invalid(format!("ippool: {e}")))?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(d.join(".lock"))
            .map_err(|e| Error::Invalid(format!("ippool lock: {e}")))?;
        // SAFETY: valid open fd; LOCK_EX blocks until the lock is ours.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(Error::Invalid("ippool: could not lock the ledger".into()));
        }
        Ok(Lock(f))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: fd still open; we own the File until here.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn pool_get(name: &str) -> Option<PoolFile> {
    get_in(&dir(), name)
}

pub fn pool_list() -> Vec<PoolFile> {
    list_in(&dir())
}

fn get_in(d: &std::path::Path, name: &str) -> Option<PoolFile> {
    serde_json::from_slice(&std::fs::read(path_in(d, name)).ok()?).ok()
}

fn list_in(d: &std::path::Path) -> Vec<PoolFile> {
    let mut v: Vec<PoolFile> = std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

fn write_in(d: &std::path::Path, p: &PoolFile) -> Result<()> {
    let json = serde_json::to_vec_pretty(p).map_err(|e| Error::Invalid(e.to_string()))?;
    delonix_state::write_atomic(&path_in(d, &p.name), &json)
        .map_err(|e| Error::Invalid(format!("ippool: {e}")))
}

// ---- address parsing ------------------------------------------------------------

/// Inclusive `[first, last]` of one `addresses` entry, as numbers.
fn parse_entry(entry: &str) -> std::result::Result<(u32, u32), String> {
    let e = entry.trim();
    let v4 = |s: &str| -> std::result::Result<u32, String> {
        s.trim()
            .parse::<Ipv4Addr>()
            .map(u32::from)
            .map_err(|_| format!("'{s}' is not an IPv4 address (IPv6 is not supported)"))
    };
    let (first, last) = if let Some((a, b)) = e.split_once('-') {
        (v4(a)?, v4(b)?)
    } else if e.contains('/') {
        let c = delonix_sdn::Cidr::parse(e).ok_or_else(|| format!("'{e}' is not a CIDR"))?;
        let (first, last) = (c.base, c.last());
        // A /31 and a /32 have no network or broadcast address to leave out.
        if c.len <= 30 {
            (first + 1, last - 1)
        } else {
            (first, last)
        }
    } else {
        let a = v4(e)?;
        (a, a)
    };
    if first > last {
        return Err(format!("'{e}': the range ends before it starts"));
    }
    for probe in [first, last] {
        let ip = Ipv4Addr::from(probe);
        if ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() || ip.is_link_local() {
            return Err(format!(
                "'{e}' contains {ip}, which is never a host address (unspecified, broadcast, multicast or link-local)"
            ));
        }
    }
    Ok((first, last))
}

/// Every address of the pool, in declared order. Refuses an empty or oversized pool and
/// an address listed twice (two entries for one address is one holder in two places).
pub fn expand(addresses: &[String]) -> Result<Vec<Ipv4Addr>> {
    if addresses.is_empty() {
        return Err(Error::Invalid(
            "IPPool: `addresses` is required and must not be empty".into(),
        ));
    }
    let mut out: Vec<Ipv4Addr> = Vec::new();
    let mut total: u64 = 0;
    for entry in addresses {
        let (first, last) =
            parse_entry(entry).map_err(|e| Error::Invalid(format!("IPPool: addresses: {e}")))?;
        total += u64::from(last - first) + 1;
        if total > MAX_ADDRESSES {
            return Err(Error::Invalid(format!(
                "IPPool: more than {MAX_ADDRESSES} addresses — narrow it"
            )));
        }
        out.extend((first..=last).map(Ipv4Addr::from));
    }
    let mut seen = std::collections::HashSet::new();
    if let Some(dup) = out.iter().find(|a| !seen.insert(**a)) {
        return Err(Error::Invalid(format!(
            "IPPool: {dup} is listed more than once"
        )));
    }
    Ok(out)
}

fn validate(spec: &IpPoolSpec) -> Result<()> {
    expand(&spec.addresses)?;
    match spec.announce.as_deref().unwrap_or("local") {
        "local" => {
            if spec.interface.is_some() {
                return Err(Error::Invalid(
                    "IPPool: `interface` only applies to `announce: l2`".into(),
                ));
            }
        }
        "l2" => {
            return Err(Error::Invalid(
                "IPPool: `announce: l2` is not built yet (ADR-0046 phase 4) — use `local` and put the address on the host yourself".into(),
            ))
        }
        other => {
            return Err(Error::Invalid(format!(
                "IPPool: announce '{other}' is not valid (local | l2)"
            )))
        }
    }
    Ok(())
}

/// Is `ip` on an interface of this host? Decided by BINDING it: that is the exact
/// question a publish asks, it needs no `ip` binary and no privilege, and a stale
/// `ip addr` parse cannot disagree with the kernel.
pub fn address_present(ip: Ipv4Addr) -> bool {
    std::net::TcpListener::bind((ip, 0)).is_ok()
}

// ---- the ledger -----------------------------------------------------------------

/// The address `claimant` holds in `pool`, taking the first free one if it holds none;
/// idempotent. A claimant that holds an address in ANOTHER pool moves: the new
/// lease is written first and the old one is given back in the same critical section.
/// This is what a route changing its `pool:` needs — refusing it
/// left the reconciler planning an update that could never be applied.
pub fn claim_moving(pool: &str, claimant: &str) -> Result<Ipv4Addr> {
    claim_in(&dir(), pool, claimant, Mode::Move)
}

/// The address [`claim_moving`] would return, without taking it. For a plan: computing what an
/// apply would do must not change what the ledger says, and must not fail because of a
/// lease the apply is about to move.
pub fn peek(pool: &str, claimant: &str) -> Result<Ipv4Addr> {
    claim_in(&dir(), pool, claimant, Mode::Peek)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Look, do not write.
    Peek,
    /// Take an address; refuse if the claimant already holds one in another pool.
    Take,
    /// Take an address and give back the one held in another pool.
    Move,
}

fn claim_in(d: &std::path::Path, pool: &str, claimant: &str, mode: Mode) -> Result<Ipv4Addr> {
    let _l = Lock::acquire(d)?;
    let mut p = get_in(d, pool).ok_or_else(|| {
        Error::NotFound(format!(
            "IPPool '{pool}' (declare a `kind: IPPool` named '{pool}')"
        ))
    })?;
    // One claimant, one address, ONE pool: holding two would make "which address is
    // mine" depend on which file was read.
    for other in list_in(d).iter().filter(|o| o.name != pool) {
        if mode != Mode::Take {
            break;
        }
        if let Some(ip) = other.leases.get(claimant) {
            return Err(Error::Conflict(format!(
                "{claimant} already holds {ip} from IPPool '{}' — release it before claiming from '{pool}'",
                other.name
            )));
        }
    }
    if let Some(ip) = p.leases.get(claimant) {
        if let Ok(a) = ip.parse::<Ipv4Addr>() {
            if mode == Mode::Move {
                for mut other in list_in(d).into_iter().filter(|o| o.name != pool) {
                    if other.leases.remove(claimant).is_some() {
                        write_in(d, &other)?;
                    }
                }
            }
            return Ok(a);
        }
    }
    let taken: std::collections::HashSet<&String> = p.leases.values().collect();
    let free = expand(&p.addresses)?
        .into_iter()
        .find(|a| !taken.contains(&a.to_string()))
        .ok_or_else(|| {
            Error::Conflict(format!(
                "IPPool '{pool}' is exhausted: {} address(es), all leased (see `delonix get ippools`)",
                p.addresses.len()
            ))
        })?;
    if mode != Mode::Peek {
        p.leases.insert(claimant.to_string(), free.to_string());
        write_in(d, &p)?;
    }
    if mode == Mode::Move {
        // The new lease is already on disk; only now is the old one given back, so a
        // failure in between leaves the claimant with an address, never with none.
        for mut other in list_in(d).into_iter().filter(|o| o.name != pool) {
            if other.leases.remove(claimant).is_some() {
                write_in(d, &other)?;
            }
        }
    }
    Ok(free)
}

/// Gives back every lease whose claimant starts with `prefix` and is not in `keep`. This
/// is how a claimant that stopped being declared lets go: the set of live claimants is
/// what the apply just resolved, not something inferred.
pub fn release_unlisted(prefix: &str, keep: &[String]) -> Result<()> {
    release_unlisted_in(&dir(), prefix, keep)
}

fn release_unlisted_in(d: &std::path::Path, prefix: &str, keep: &[String]) -> Result<()> {
    let _l = Lock::acquire(d)?;
    for mut p in list_in(d) {
        let stale: Vec<String> = p
            .leases
            .keys()
            .filter(|c| c.starts_with(prefix) && !keep.contains(c))
            .cloned()
            .collect();
        if stale.is_empty() {
            continue;
        }
        for c in stale {
            p.leases.remove(&c);
        }
        write_in(d, &p)?;
    }
    Ok(())
}

// ---- the Kind -------------------------------------------------------------------

fn spec_fields(spec: &IpPoolSpec) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("addresses".into(), spec.addresses.join(","));
    f.insert(
        "announce".into(),
        spec.announce.clone().unwrap_or_else(|| "local".into()),
    );
    f.insert(
        "interface".into(),
        spec.interface.clone().unwrap_or_default(),
    );
    f
}

pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: IpPoolSpec = manifest::spec_of(doc)?;
    Ok(super::reconcile::Desired {
        kind: k::IPPOOL.into(),
        name: doc.metadata.name.clone(),
        fields: spec_fields(&spec),
        converges: true,
        ownable: true,
    })
}

pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    Ok(pool_list()
        .into_iter()
        .map(|p| {
            let mut f = BTreeMap::new();
            f.insert("addresses".into(), p.addresses.join(","));
            f.insert("announce".into(), p.announce.clone());
            f.insert("interface".into(), p.interface.clone().unwrap_or_default());
            super::reconcile::Actual {
                kind: k::IPPOOL.into(),
                name: p.name.clone(),
                fields: f,
                owner: p.labels.get(super::reconcile::STACK_LABEL).cloned(),
                last_applied: p
                    .annotations
                    .get(super::reconcile::LAST_APPLIED)
                    .and_then(|raw| super::reconcile::decode_last_applied(raw)),
            }
        })
        .collect())
}

pub(crate) fn stamp(name: &str, stack: &str, fields: &BTreeMap<String, String>) -> Result<()> {
    let d = dir();
    let _l = Lock::acquire(&d)?;
    let mut p = get_in(&d, name).ok_or_else(|| Error::NotFound(format!("ippool: {name}")))?;
    p.labels
        .insert(super::reconcile::STACK_LABEL.into(), stack.into());
    p.labels
        .insert(super::reconcile::MANAGED_BY.into(), "delonix".into());
    p.annotations.insert(
        super::reconcile::LAST_APPLIED.into(),
        super::reconcile::encode_last_applied(fields),
    );
    write_in(&d, &p)
}

/// Removes a pool. **Refused while it has leases**: the addresses are published on
/// listeners right now, and forgetting who holds them is how two claimants end up on
/// one address.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    let d = dir();
    let _l = Lock::acquire(&d)?;
    let Some(p) = get_in(&d, name) else {
        return Ok(());
    };
    if !p.leases.is_empty() {
        let held: Vec<String> = p.leases.iter().map(|(c, a)| format!("{c}={a}")).collect();
        return Err(Error::Conflict(format!(
            "IPPool '{name}' still has leases ({}) — remove the routes that claim it first",
            held.join(", ")
        )));
    }
    let _ = std::fs::remove_file(path_in(&d, name));
    Ok(())
}

fn apply_one(doc: &ManifestDoc) -> Result<()> {
    let spec: IpPoolSpec = manifest::spec_of(doc)?;
    validate(&spec)?;
    let d = dir();
    let _l = Lock::acquire(&d)?;
    let mut p = get_in(&d, &doc.metadata.name).unwrap_or_default();
    let new: std::collections::HashSet<Ipv4Addr> = expand(&spec.addresses)?.into_iter().collect();
    // Shrinking a pool under a live lease would leave a listener bound to an address the
    // ledger no longer owns.
    for (claimant, ip) in &p.leases {
        if ip.parse::<Ipv4Addr>().map_or(true, |a| !new.contains(&a)) {
            return Err(Error::Conflict(format!(
                "IPPool '{}': {claimant} holds {ip}, which the new `addresses` no longer contain",
                doc.metadata.name
            )));
        }
    }
    p.name = doc.metadata.name.clone();
    p.addresses = spec.addresses.clone();
    p.announce = spec.announce.clone().unwrap_or_else(|| "local".into());
    p.interface = spec.interface.clone();
    write_in(&d, &p)?;
    println!(
        "{}",
        super::po::tf(
            "ippool/{name}: {n} address(es), announce {announce}, {leased} leased",
            &[
                ("name", &p.name),
                ("n", &new.len().to_string()),
                ("announce", &p.announce),
                ("leased", &p.leases.len().to_string()),
            ],
        )
    );
    Ok(())
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, k::IPPOOL) {
        apply_one(doc)?;
    }
    Ok(())
}

pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    apply_one(doc)
}

pub(crate) fn presence_of(doc: &ManifestDoc) -> (String, String) {
    match pool_get(&doc.metadata.name) {
        None => ("no".into(), "-".into()),
        Some(p) => {
            let total = expand(&p.addresses).map(|v| v.len()).unwrap_or(0);
            (
                "yes".into(),
                super::po::tf(
                    "{leased}/{total} leased",
                    &[
                        ("leased", &p.leases.len().to_string()),
                        ("total", &total.to_string()),
                    ],
                ),
            )
        }
    }
}

pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: IpPoolSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

#[derive(Serialize)]
struct LsRow {
    name: String,
    addresses: Vec<String>,
    announce: String,
    total: usize,
    leased: usize,
    leases: BTreeMap<String, String>,
    stack: Option<String>,
}

pub(crate) fn cmd_ls(format: OutputFormat) -> Result<()> {
    let format = super::config::resolve_output(&super::util::state_root(), format);
    let rows: Vec<LsRow> = pool_list()
        .into_iter()
        .map(|p| LsRow {
            total: expand(&p.addresses).map(|v| v.len()).unwrap_or(0),
            leased: p.leases.len(),
            stack: p.labels.get(super::reconcile::STACK_LABEL).cloned(),
            name: p.name,
            addresses: p.addresses,
            announce: p.announce,
            leases: p.leases,
        })
        .collect();
    if format == OutputFormat::Json {
        return super::output::print_json(&rows);
    }
    let mut t = super::output::Table::new(&["NAME", "ADDRESSES", "ANNOUNCE", "LEASED", "STACK"]);
    for r in &rows {
        t.row(vec![
            r.name.clone(),
            r.addresses.join(","),
            r.announce.clone(),
            format!("{}/{}", r.leased, r.total),
            r.stack.clone().unwrap_or_else(|| "-".into()),
        ]);
    }
    t.drop_uninformative().print();
    Ok(())
}

pub(crate) fn cmd_describe(names: &[String]) -> Result<()> {
    for name in names {
        let Some(p) = pool_get(name) else {
            return Err(Error::NotFound(format!("ippool: {name}")));
        };
        let mut d = super::output::Describe::new();
        d.field("Name", &p.name);
        d.field("Addresses", p.addresses.join(", "));
        d.field("Announce", &p.announce);
        d.field_opt("Interface", p.interface.as_ref());
        d.field(
            "Leases",
            if p.leases.is_empty() {
                "<none>".to_string()
            } else {
                p.leases
                    .iter()
                    .map(|(c, a)| format!("{c}={a}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        );
        d.field_opt("Stack", p.labels.get(super::reconcile::STACK_LABEL));
        d.print();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_range_a_cidr_and_a_single_address_expand_in_declared_order() {
        let v = expand(&a(&[
            "203.0.113.5",
            "203.0.113.10-203.0.113.12",
            "198.51.100.0/30",
        ]))
        .unwrap();
        let s: Vec<String> = v.iter().map(|x| x.to_string()).collect();
        assert_eq!(
            s,
            [
                "203.0.113.5",
                "203.0.113.10",
                "203.0.113.11",
                "203.0.113.12",
                // a /30 leaves out its network and broadcast address
                "198.51.100.1",
                "198.51.100.2"
            ]
        );
    }

    #[test]
    fn a_slash_31_and_32_keep_every_address() {
        assert_eq!(expand(&a(&["198.51.100.4/31"])).unwrap().len(), 2);
        assert_eq!(expand(&a(&["198.51.100.4/32"])).unwrap().len(), 1);
    }

    #[test]
    fn bad_pools_are_refused_by_reason() {
        for (input, needle) in [
            (vec![], "must not be empty"),
            (vec!["2001:db8::1"], "IPv4"),
            (vec!["203.0.113.9-203.0.113.1"], "ends before"),
            (vec!["224.0.0.1"], "never a host address"),
            (vec!["169.254.1.1"], "never a host address"),
            (vec!["0.0.0.0"], "never a host address"),
            (
                vec!["203.0.113.1", "203.0.113.0-203.0.113.3"],
                "more than once",
            ),
            (vec!["10.0.0.0/8"], "narrow it"),
        ] {
            let e = expand(&a(&input)).unwrap_err().to_string();
            assert!(e.contains(needle), "{input:?} -> {e}");
        }
    }

    #[test]
    fn only_announce_local_is_built_and_interface_needs_l2() {
        let ok = |s: &str| validate(&serde_yaml::from_str(s).unwrap());
        assert!(ok("addresses: [203.0.113.1]").is_ok());
        assert!(ok("addresses: [203.0.113.1]\nannounce: local").is_ok());
        assert!(ok("addresses: [203.0.113.1]\nannounce: l2")
            .unwrap_err()
            .to_string()
            .contains("not built"));
        assert!(ok("addresses: [203.0.113.1]\ninterface: eno1")
            .unwrap_err()
            .to_string()
            .contains("only applies"));
        assert!(ok("addresses: [203.0.113.1]\nannounce: bgp")
            .unwrap_err()
            .to_string()
            .contains("not valid"));
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dlx-ippool-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seed(d: &std::path::Path, name: &str, addrs: &[&str]) {
        write_in(
            d,
            &PoolFile {
                name: name.into(),
                addresses: a(addrs),
                announce: "local".into(),
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn a_claim_is_idempotent_and_an_address_has_one_holder() {
        let d = scratch("claim");
        seed(&d, "edge", &["203.0.113.1-203.0.113.2"]);
        let first = claim_in(&d, "edge", "HTTPRoute/a", Mode::Take).unwrap();
        assert_eq!(first.to_string(), "203.0.113.1");
        assert_eq!(
            claim_in(&d, "edge", "HTTPRoute/a", Mode::Take).unwrap(),
            first
        );
        assert_eq!(
            claim_in(&d, "edge", "HTTPRoute/b", Mode::Take)
                .unwrap()
                .to_string(),
            "203.0.113.2"
        );
        let e = claim_in(&d, "edge", "HTTPRoute/c", Mode::Take).unwrap_err();
        assert_eq!(
            delonix_model::exitcode::for_error(&e),
            delonix_model::exitcode::CONFLICT,
            "{e}"
        );
        assert!(e.to_string().contains("exhausted"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn peek_shows_the_address_a_claim_would_take_without_taking_it() {
        let d = scratch("peek");
        seed(&d, "edge", &["203.0.113.1"]);
        assert_eq!(
            claim_in(&d, "edge", "HTTPRoute/a", Mode::Peek)
                .unwrap()
                .to_string(),
            "203.0.113.1"
        );
        assert!(
            get_in(&d, "edge").unwrap().leases.is_empty(),
            "peek must not write"
        );
        // and the same address is still free for a real claim by someone else
        assert_eq!(
            claim_in(&d, "edge", "HTTPRoute/b", Mode::Take)
                .unwrap()
                .to_string(),
            "203.0.113.1"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_released_address_goes_back_to_the_pool_and_release_is_idempotent() {
        let d = scratch("release");
        seed(&d, "edge", &["203.0.113.1"]);
        claim_in(&d, "edge", "HTTPRoute/a", Mode::Take).unwrap();
        release_unlisted_in(&d, "HTTPRoute/", &[]).unwrap();
        release_unlisted_in(&d, "HTTPRoute/", &[]).unwrap();
        assert_eq!(
            claim_in(&d, "edge", "HTTPRoute/b", Mode::Take)
                .unwrap()
                .to_string(),
            "203.0.113.1"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn release_unlisted_keeps_the_live_claimants_and_only_that_prefix() {
        let d = scratch("unlisted");
        seed(&d, "edge", &["203.0.113.1-203.0.113.4"]);
        for c in ["HTTPRoute/keep", "HTTPRoute/gone", "Other/x"] {
            claim_in(&d, "edge", c, Mode::Take).unwrap();
        }
        release_unlisted_in(&d, "HTTPRoute/", &["HTTPRoute/keep".to_string()]).unwrap();
        let p = get_in(&d, "edge").unwrap();
        assert!(p.leases.contains_key("HTTPRoute/keep"));
        assert!(!p.leases.contains_key("HTTPRoute/gone"));
        assert!(
            p.leases.contains_key("Other/x"),
            "a different kind of claimant is not ours to release"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn one_claimant_cannot_hold_addresses_in_two_pools() {
        let d = scratch("two");
        seed(&d, "a", &["203.0.113.1"]);
        seed(&d, "b", &["198.51.100.1"]);
        claim_in(&d, "a", "HTTPRoute/x", Mode::Take).unwrap();
        let e = claim_in(&d, "b", "HTTPRoute/x", Mode::Take)
            .unwrap_err()
            .to_string();
        assert!(e.contains("already holds"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_claimant_can_move_between_pools_and_a_peek_never_fails_on_the_old_lease() {
        let d = scratch("move");
        seed(&d, "a", &["203.0.113.1"]);
        seed(&d, "b", &["198.51.100.1"]);
        claim_in(&d, "a", "HTTPRoute/x", Mode::Take).unwrap();
        // a plan looking at the move must not fail, nor write
        assert_eq!(
            claim_in(&d, "b", "HTTPRoute/x", Mode::Peek)
                .unwrap()
                .to_string(),
            "198.51.100.1"
        );
        assert_eq!(get_in(&d, "a").unwrap().leases.len(), 1);
        assert!(get_in(&d, "b").unwrap().leases.is_empty());
        // the apply moves it: new lease written, old one given back
        assert_eq!(
            claim_in(&d, "b", "HTTPRoute/x", Mode::Move)
                .unwrap()
                .to_string(),
            "198.51.100.1"
        );
        assert!(get_in(&d, "a").unwrap().leases.is_empty());
        assert_eq!(get_in(&d, "b").unwrap().leases.len(), 1);
        // and the plain Take still refuses to hold two
        let e = claim_in(&d, "a", "HTTPRoute/x", Mode::Take)
            .unwrap_err()
            .to_string();
        assert!(e.contains("already holds"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn claiming_from_a_pool_that_does_not_exist_says_how_to_declare_it() {
        let d = scratch("missing");
        let e = claim_in(&d, "nope", "HTTPRoute/x", Mode::Take).unwrap_err();
        assert_eq!(
            delonix_model::exitcode::for_error(&e),
            delonix_model::exitcode::NOT_FOUND,
            "{e}"
        );
        assert!(e.to_string().contains("kind: IPPool"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_address_on_the_loopback_is_present_and_an_unassigned_one_is_not() {
        assert!(address_present(Ipv4Addr::new(127, 0, 0, 1)));
        // TEST-NET-3 is never assigned to an interface
        assert!(!address_present(Ipv4Addr::new(203, 0, 113, 77)));
    }
}
