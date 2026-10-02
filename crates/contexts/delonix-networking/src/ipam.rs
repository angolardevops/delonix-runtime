//! The IPAM role (ADR-0059 D1, F5b): the subnets inside a provider's
//! segments, the DHCP ranges they serve, and the addresses reserved per MAC.
//!
//! A provider that serves segments can also own their addressing — Proxmox's
//! SDN does, with its built-in `pve` IPAM and a per-zone `dnsmasq`. This port
//! is that role, kept apart from [`crate::segment`] for the reason every role
//! port is (D1 rule 2): a segment provider without IPAM must not be forced to
//! answer for it, and the registry by id says which providers have the role.
//!
//! # Two kinds of write, and the order they need
//!
//! Measured on PVE 9.2.2, and it shapes the port:
//!
//! * a subnet, a DHCP range and the zone's IPAM/DHCP options are STAGED like
//!   every SDN object: they ride the segment provider's transaction
//!   ([`crate::segment::SegmentProvider::transaction`]) and go live with it;
//! * a reservation is IMMEDIATE (no staging, no apply), and it is refused
//!   until its subnet is running ("can't find any subnet for ip") — so
//!   reservations are made after the transaction committed, and removed
//!   before a teardown's transaction (the node refuses to delete a subnet that
//!   still holds one: "cannot delete subnet …, not empty").
//!
//! # Ownership
//!
//! A subnet has no free-text field to carry a mark. It is this engine's when
//! the vnet it lives in carries the caller's [`OwnerMark`]. A reservation has
//! none either (an IPAM entry is ip, mac, vnet, zone): one is this engine's
//! when the caller's record declared it. An entry at the same address with
//! ANOTHER MAC — a guest's allocation, someone's reservation — is refused,
//! never overwritten.

use crate::error::{Error, Result};
use crate::ownership::{OwnerMark, RemoveOutcome};
use crate::segment::EnsureOutcome;
use delonix_net_rules::Cidr;

/// One DHCP range of a subnet, both ends included.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DhcpRange {
    pub start: String,
    pub end: String,
}

/// A subnet inside a vnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpamSubnet {
    pub vnet: String,
    /// `a.b.c.d/len`, at its network address.
    pub cidr: String,
    pub gateway: Option<String>,
    pub dhcp_ranges: Vec<DhcpRange>,
}

/// An address reserved for one MAC inside a vnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpamReservation {
    pub vnet: String,
    pub ip: String,
    /// Upper case, `AA:BB:CC:DD:EE:FF` — the form the node stores.
    pub mac: String,
}

/// An IPAM entry the provider holds that the caller did not declare: an
/// allocation the provider made for a guest, or someone's reservation. Never
/// drift; part of what a plan was decided against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpamEntry {
    pub vnet: String,
    pub ip: String,
    pub mac: Option<String>,
    /// The guest the provider made the entry for, when it says so (an
    /// allocation at guest create carries one; a reservation does not).
    pub vmid: Option<u32>,
}

/// What an IPAM provider holds for one zone, restricted to the vnets the
/// caller owns (ADR-0059 D4, observe): the RUNNING subnets, every IPAM entry
/// in those vnets, and the zone's own IPAM/DHCP options as running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IpamObserved {
    pub subnets: Vec<IpamSubnet>,
    pub entries: Vec<IpamEntry>,
    /// The zone allocates from an IPAM at all. Without one, measured, a
    /// reservation call answers success and stores nothing.
    pub zone_ipam: bool,
    /// The zone serves DHCP.
    pub zone_dhcp: bool,
}

fn cidr_of(text: &str) -> Option<Cidr> {
    // `Cidr::parse` also reads the legacy two-octet form; a subnet always
    // has the full one.
    text.contains('/').then(|| Cidr::parse(text)).flatten()
}

fn ip_of(text: &str) -> Option<u32> {
    text.parse::<std::net::Ipv4Addr>().ok().map(u32::from)
}

fn cidr_text(c: &Cidr) -> String {
    format!("{}/{}", std::net::Ipv4Addr::from(c.base), c.len)
}

/// `AA:BB:CC:DD:EE:FF`, or `None` when `mac` is not six hex pairs.
pub fn normalize_mac(mac: &str) -> Option<String> {
    let parts: Vec<&str> = mac.trim().split(':').collect();
    (parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit())))
    .then(|| mac.trim().to_ascii_uppercase())
}

impl IpamSubnet {
    /// IPv4 at its network address; the gateway and every range inside it,
    /// and a range's start not after its end.
    pub fn validate(&self) -> Result<()> {
        let bad = |why: String| Err(Error::Engine(delonix_model::Error::Invalid(why)));
        let Some(c) = cidr_of(&self.cidr) else {
            return bad(format!(
                "subnet '{}' in vnet '{}': not an IPv4 CIDR (a.b.c.d/len)",
                self.cidr, self.vnet
            ));
        };
        if cidr_text(&c) != self.cidr.trim() {
            return bad(format!(
                "subnet '{}' in vnet '{}': write it at its network address, {}",
                self.cidr,
                self.vnet,
                cidr_text(&c)
            ));
        }
        let inside = |ip: &str| ip_of(ip).is_some_and(|i| c.contains(i));
        if let Some(g) = &self.gateway {
            if !inside(g) {
                return bad(format!(
                    "subnet '{}': gateway '{g}' is not an address inside it",
                    self.cidr
                ));
            }
        }
        for r in &self.dhcp_ranges {
            if !inside(&r.start) || !inside(&r.end) {
                return bad(format!(
                    "subnet '{}': dhcp range {}-{} is not inside it",
                    self.cidr, r.start, r.end
                ));
            }
            if ip_of(&r.start) > ip_of(&r.end) {
                return bad(format!(
                    "subnet '{}': dhcp range {}-{} starts after it ends",
                    self.cidr, r.start, r.end
                ));
            }
        }
        Ok(())
    }

    /// Whether `ip` is an address of this subnet.
    pub fn holds(&self, ip: &str) -> bool {
        match (cidr_of(&self.cidr), ip_of(ip)) {
            (Some(c), Some(i)) => c.contains(i),
            _ => false,
        }
    }
}

impl IpamReservation {
    /// An IPv4 address inside one of `subnets` of the same vnet, never the
    /// subnet's gateway, and a MAC of six hex pairs.
    pub fn validate(&self, subnets: &[IpamSubnet]) -> Result<()> {
        let bad = |why: String| Err(Error::Engine(delonix_model::Error::Invalid(why)));
        if normalize_mac(&self.mac).is_none() {
            return bad(format!(
                "reservation {} in vnet '{}': '{}' is not a MAC (AA:BB:CC:DD:EE:FF)",
                self.ip, self.vnet, self.mac
            ));
        }
        let Some(subnet) = subnets
            .iter()
            .find(|s| s.vnet == self.vnet && s.holds(&self.ip))
        else {
            return bad(format!(
                "reservation {} in vnet '{}': the address is in no subnet declared for that vnet",
                self.ip, self.vnet
            ));
        };
        if subnet.gateway.as_deref() == Some(self.ip.as_str()) {
            return bad(format!(
                "reservation {} in vnet '{}': that is the subnet's gateway",
                self.ip, self.vnet
            ));
        }
        Ok(())
    }
}

/// The other addresses `r.mac` holds in `r.vnet`, besides the reserved one.
///
/// Measured on PVE 9.2.2: a reservation made BEFORE its guest exists is
/// followed, at guest create, by an allocation of a range address for the
/// same MAC, and the guest is served that one — the reservation stays in the
/// IPAM and nobody gets it. An address the MAC also holds is therefore not a
/// harmless extra: it is the address the guest actually gets. Pure.
pub fn other_addresses_of<'a>(
    r: &IpamReservation,
    observed: &'a IpamObserved,
) -> Vec<&'a IpamEntry> {
    observed
        .entries
        .iter()
        .filter(|e| e.vnet == r.vnet && e.ip != r.ip && e.mac.as_deref() == Some(r.mac.as_str()))
        .collect()
}

/// The reservations of `wanted` the provider holds as declared: the address
/// held for the declared MAC in the declared vnet, and no other address for
/// that MAC there (see [`other_addresses_of`]). Pure.
pub fn held_reservations(
    wanted: &[IpamReservation],
    observed: &IpamObserved,
) -> Vec<IpamReservation> {
    wanted
        .iter()
        .filter(|w| {
            observed.entries.iter().any(|e| {
                e.vnet == w.vnet && e.ip == w.ip && e.mac.as_deref() == Some(w.mac.as_str())
            }) && other_addresses_of(w, observed).is_empty()
        })
        .cloned()
        .collect()
}

/// Whether a zone serves DHCP: any subnet declares a range, or the zone holds
/// reservations. Measured on PVE 9.2.2: the IPAM listing skips every zone
/// without `dhcp`, so in such a zone a reservation is stored and never read
/// back — and a reservation exists to be served, which `dnsmasq` does per
/// subnet in `static` mode whether or not a range is declared. Pure.
pub fn zone_serves_dhcp(subnets: &[IpamSubnet], has_reservations: bool) -> bool {
    has_reservations || subnets.iter().any(|s| !s.dhcp_ranges.is_empty())
}

/// How what a provider holds differs from what a record declared, one line
/// per difference, sorted. Empty = in sync. `dhcp` is
/// [`zone_serves_dhcp`] of the record. Pure.
///
/// Entries nobody declared (a guest's allocation) are not differences, and a
/// subnet in an owned vnet that is not declared is one: nobody else removes
/// it.
pub fn ipam_drift(
    subnets: &[IpamSubnet],
    reservations: &[IpamReservation],
    dhcp: bool,
    observed: &IpamObserved,
) -> Vec<String> {
    let mut out = Vec::new();
    if !subnets.is_empty() && !observed.zone_ipam {
        out.push("the zone allocates from no IPAM".into());
    }
    if dhcp && !observed.zone_dhcp {
        out.push("the zone does not serve DHCP".into());
    }
    for want in subnets {
        match observed
            .subnets
            .iter()
            .find(|s| s.vnet == want.vnet && s.cidr == want.cidr)
        {
            None => out.push(format!(
                "subnet {} in vnet '{}' is missing",
                want.cidr, want.vnet
            )),
            Some(have) => {
                if have.gateway != want.gateway {
                    out.push(format!(
                        "subnet {} gateway is '{}', declared '{}'",
                        want.cidr,
                        have.gateway.as_deref().unwrap_or(""),
                        want.gateway.as_deref().unwrap_or("")
                    ));
                }
                let ranges = |r: &[DhcpRange]| {
                    let mut v: Vec<String> =
                        r.iter().map(|r| format!("{}-{}", r.start, r.end)).collect();
                    v.sort();
                    v.join(",")
                };
                if ranges(&have.dhcp_ranges) != ranges(&want.dhcp_ranges) {
                    out.push(format!(
                        "subnet {} dhcp ranges are '{}', declared '{}'",
                        want.cidr,
                        ranges(&have.dhcp_ranges),
                        ranges(&want.dhcp_ranges)
                    ));
                }
            }
        }
    }
    for have in &observed.subnets {
        if !subnets
            .iter()
            .any(|s| s.vnet == have.vnet && s.cidr == have.cidr)
        {
            out.push(format!(
                "subnet {} in vnet '{}' is in this engine's vnet and is not declared",
                have.cidr, have.vnet
            ));
        }
    }
    for want in reservations {
        match observed
            .entries
            .iter()
            .find(|e| e.vnet == want.vnet && e.ip == want.ip)
        {
            None => out.push(format!(
                "reservation {} in vnet '{}' is missing",
                want.ip, want.vnet
            )),
            Some(e) if e.mac.as_deref() != Some(want.mac.as_str()) => out.push(format!(
                "address {} in vnet '{}' is held by '{}', declared '{}'",
                want.ip,
                want.vnet,
                e.mac.as_deref().unwrap_or("no MAC"),
                want.mac
            )),
            Some(_) => {}
        }
        for e in other_addresses_of(want, observed) {
            out.push(format!(
                "MAC {} also holds {} in vnet '{}', and the guest is served that one",
                want.mac, e.ip, want.vnet
            ));
        }
    }
    out.sort();
    out
}

/// A backend that owns the addressing of the segments it serves.
/// Extends the provider skeleton (ADR-0059 D1 rule 4).
pub trait IpamProvider: delonix_compute::vm_provider::Provider {
    /// `true` if this backend can be used right now. Never a round trip.
    fn available(&self) -> bool;

    /// Sets the zone's IPAM and DHCP options so its subnets allocate and
    /// serve addresses (staged; inside the segment transaction). Only a zone
    /// the caller owns is passed here. `dhcp` says whether any subnet
    /// declares a range.
    fn prepare_zone(&self, zone: &str, dhcp: bool) -> delonix_model::Result<()>;

    /// Ensures a subnet exists in a vnet `owner` owns (staged; inside the
    /// segment transaction). One in a vnet without the mark is refused; one
    /// that exists with another gateway or other ranges is drift.
    fn ensure_subnet(
        &self,
        zone: &str,
        subnet: &IpamSubnet,
        owner: &OwnerMark,
    ) -> delonix_model::Result<EnsureOutcome>;

    /// Removes a subnet from a vnet `owner` owns (staged). An absent one is
    /// not an error; the provider refuses one that still holds addresses.
    fn remove_subnet(
        &self,
        zone: &str,
        subnet: &IpamSubnet,
        owner: &OwnerMark,
    ) -> delonix_model::Result<()>;

    /// Reserves `r.ip` for `r.mac` (immediate; after the subnet is running).
    /// The same address held for the same MAC is already present; held for
    /// another MAC, or by an allocation without one, it is refused.
    fn ensure_reservation(
        &self,
        zone: &str,
        r: &IpamReservation,
    ) -> delonix_model::Result<EnsureOutcome>;

    /// Removes a reservation only when the address is still held for the
    /// declared MAC. An absent one is `Absent`; one now held for another MAC
    /// (or by an allocation without one) is left alone and `NotOwned`.
    fn remove_reservation(
        &self,
        zone: &str,
        r: &IpamReservation,
    ) -> delonix_model::Result<RemoveOutcome>;

    /// Reads what [`IpamObserved`] describes for `zone`, restricted to
    /// `vnets` (the ones the caller owns). Read-only.
    fn observe(&self, zone: &str, vnets: &[String]) -> delonix_model::Result<IpamObserved>;
}

/// Builds an [`IpamProvider`], or reports why it could not.
pub type IpamProviderFactory = Box<dyn Fn() -> Result<Box<dyn IpamProvider>> + Send + Sync>;

/// One provider registered for the IPAM role (ADR-0059 D1 rule 2).
pub struct IpamProviderRegistration {
    /// Canonical id. Must equal what the built provider's `id` returns.
    pub id: &'static str,
    /// Extra spellings accepted from a caller; never repeats `id`.
    pub aliases: &'static [&'static str],
    pub new: IpamProviderFactory,
}

static IPAM_PROVIDERS: std::sync::OnceLock<std::sync::RwLock<Vec<IpamProviderRegistration>>> =
    std::sync::OnceLock::new();

fn ipam_providers() -> &'static std::sync::RwLock<Vec<IpamProviderRegistration>> {
    IPAM_PROVIDERS.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

fn with_ipam_providers<T>(f: impl FnOnce(&[IpamProviderRegistration]) -> T) -> T {
    let guard = ipam_providers().read().unwrap_or_else(|e| e.into_inner());
    f(&guard)
}

/// Adds a provider to the IPAM registry. Idempotent by id; does no I/O. An
/// empty id, or a name that already belongs to a DIFFERENT provider, is
/// refused — the rules every role registry follows.
pub fn register_ipam_provider(reg: IpamProviderRegistration) -> Result<()> {
    if reg.id.trim().is_empty() {
        return Err(Error::NetworkZoneProviderRegistrationRefused(
            "an ipam provider registration needs an id".into(),
        ));
    }
    let mut guard = ipam_providers().write().unwrap_or_else(|e| e.into_inner());
    for name in std::iter::once(&reg.id).chain(reg.aliases.iter()) {
        let want = name.trim().to_lowercase();
        if let Some(clash) = guard
            .iter()
            .find(|b| b.id != reg.id && (b.id == want || b.aliases.contains(&want.as_str())))
        {
            return Err(Error::NetworkZoneProviderRegistrationRefused(format!(
                "ipam provider '{}' cannot claim the name '{}': it already belongs to '{}'",
                reg.id, name, clash.id
            )));
        }
    }
    guard.retain(|b| b.id != reg.id);
    guard.push(reg);
    Ok(())
}

/// Builds the IPAM provider registered under `name` (id or alias), or `None`
/// when nothing is. A zone's addressing is served by the provider that serves
/// the zone, so the lookup is by that provider's id: one that is not in this
/// registry does not have the role.
pub fn ipam_provider_for(name: &str) -> Option<Result<Box<dyn IpamProvider>>> {
    let want = name.trim().to_lowercase();
    with_ipam_providers(|regs| {
        regs.iter()
            .find(|r| r.id == want || r.aliases.contains(&want.as_str()))
            .map(|r| (r.new)())
    })
}

/// The registered ids, in registration order.
pub fn ipam_provider_ids() -> Vec<&'static str> {
    with_ipam_providers(|regs| regs.iter().map(|r| r.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subnet() -> IpamSubnet {
        IpamSubnet {
            vnet: "v1".into(),
            cidr: "10.78.0.0/24".into(),
            gateway: Some("10.78.0.1".into()),
            dhcp_ranges: vec![DhcpRange {
                start: "10.78.0.100".into(),
                end: "10.78.0.150".into(),
            }],
        }
    }

    fn reservation() -> IpamReservation {
        IpamReservation {
            vnet: "v1".into(),
            ip: "10.78.0.20".into(),
            mac: "BC:24:11:00:00:20".into(),
        }
    }

    #[test]
    fn a_subnet_and_a_reservation_validate() {
        subnet().validate().unwrap();
        reservation().validate(&[subnet()]).unwrap();
    }

    #[test]
    fn each_refusal_names_what_is_wrong() {
        let refused = |s: IpamSubnet, needle: &str| {
            let e = s.validate().unwrap_err().to_string();
            assert!(e.contains(needle), "{e}");
        };
        let mut s = subnet();
        s.cidr = "10.78.0.5/24".into();
        refused(s, "network address, 10.78.0.0/24");
        let mut s = subnet();
        s.cidr = "10.78".into();
        refused(s, "not an IPv4 CIDR");
        let mut s = subnet();
        s.gateway = Some("10.79.0.1".into());
        refused(s, "gateway");
        let mut s = subnet();
        s.dhcp_ranges[0].end = "10.78.1.10".into();
        refused(s, "not inside it");
        let mut s = subnet();
        s.dhcp_ranges[0] = DhcpRange {
            start: "10.78.0.150".into(),
            end: "10.78.0.100".into(),
        };
        refused(s, "starts after it ends");

        let refused_r = |r: IpamReservation, needle: &str| {
            let e = r.validate(&[subnet()]).unwrap_err().to_string();
            assert!(e.contains(needle), "{e}");
        };
        let mut r = reservation();
        r.mac = "BC:24:11".into();
        refused_r(r, "not a MAC");
        let mut r = reservation();
        r.ip = "10.79.0.20".into();
        refused_r(r, "no subnet declared");
        let mut r = reservation();
        r.vnet = "v2".into();
        refused_r(r, "no subnet declared");
        let mut r = reservation();
        r.ip = "10.78.0.1".into();
        refused_r(r, "gateway");
    }

    #[test]
    fn a_mac_is_written_in_the_form_the_node_stores() {
        assert_eq!(
            normalize_mac(" bc:24:11:0a:00:20 ").as_deref(),
            Some("BC:24:11:0A:00:20")
        );
        assert_eq!(normalize_mac("bc-24-11-0a-00-20"), None);
    }

    fn in_sync() -> IpamObserved {
        IpamObserved {
            subnets: vec![subnet()],
            entries: vec![
                IpamEntry {
                    vnet: "v1".into(),
                    ip: "10.78.0.1".into(),
                    mac: None,
                    vmid: None,
                },
                IpamEntry {
                    vnet: "v1".into(),
                    ip: "10.78.0.20".into(),
                    mac: Some("BC:24:11:00:00:20".into()),
                    vmid: None,
                },
                // A guest's allocation: not declared, not drift.
                IpamEntry {
                    vnet: "v1".into(),
                    ip: "10.78.0.101".into(),
                    mac: Some("BC:24:11:00:00:99".into()),
                    vmid: None,
                },
            ],
            zone_ipam: true,
            zone_dhcp: true,
        }
    }

    #[test]
    fn what_matches_is_in_sync_and_each_difference_is_one_line() {
        assert!(ipam_drift(&[subnet()], &[reservation()], true, &in_sync()).is_empty());

        let mut o = in_sync();
        o.zone_dhcp = false;
        o.zone_ipam = false;
        o.subnets[0].gateway = Some("10.78.0.254".into());
        o.subnets[0].dhcp_ranges.clear();
        o.entries[1].mac = Some("BC:24:11:00:00:21".into());
        o.subnets.push(IpamSubnet {
            vnet: "v1".into(),
            cidr: "10.78.1.0/24".into(),
            gateway: None,
            dhcp_ranges: vec![],
        });
        let d = ipam_drift(&[subnet()], &[reservation()], true, &o);
        assert_eq!(d.len(), 6, "{d:?}");

        let mut o = in_sync();
        o.subnets.clear();
        o.entries.remove(1);
        let d = ipam_drift(&[subnet()], &[reservation()], true, &o);
        assert_eq!(
            d,
            vec![
                "reservation 10.78.0.20 in vnet 'v1' is missing".to_string(),
                "subnet 10.78.0.0/24 in vnet 'v1' is missing".to_string(),
            ]
        );
    }

    /// The order that left a guest on the wrong address (measured): the
    /// reservation first, then a guest created with that MAC, which the node
    /// gives a range address too. The reservation is not held while the MAC
    /// holds the other one.
    #[test]
    fn a_reservation_whose_mac_also_holds_an_allocation_is_not_held() {
        let mut o = in_sync();
        o.entries.push(IpamEntry {
            vnet: "v1".into(),
            ip: "10.78.0.100".into(),
            mac: Some("BC:24:11:00:00:20".into()),
            vmid: Some(9863),
        });
        assert!(held_reservations(&[reservation()], &o).is_empty());
        let d = ipam_drift(&[subnet()], &[reservation()], true, &o);
        assert_eq!(
            d,
            vec!["MAC BC:24:11:00:00:20 also holds 10.78.0.100 in vnet 'v1', and the guest is served that one".to_string()]
        );
        assert_eq!(
            held_reservations(&[reservation()], &in_sync()),
            vec![reservation()]
        );
    }

    #[test]
    fn reservations_alone_make_the_zone_serve_dhcp() {
        let mut s = subnet();
        s.dhcp_ranges.clear();
        assert!(!zone_serves_dhcp(std::slice::from_ref(&s), false));
        assert!(zone_serves_dhcp(std::slice::from_ref(&s), true));
        assert!(zone_serves_dhcp(&[subnet()], false));
    }

    #[test]
    fn a_zone_without_subnets_asks_nothing_of_the_zone() {
        assert!(ipam_drift(&[], &[], false, &IpamObserved::default()).is_empty());
    }

    #[test]
    fn the_registry_refuses_an_empty_id_and_a_name_clash() {
        let reg = |id: &'static str, aliases: &'static [&'static str]| IpamProviderRegistration {
            id,
            aliases,
            new: Box::new(|| Err(Error::NoProviderForRole("never built".into()))),
        };
        assert!(register_ipam_provider(reg("", &[])).is_err());
        register_ipam_provider(reg("ipam-clash-a", &["ipam-shared"])).unwrap();
        assert!(register_ipam_provider(reg("ipam-clash-b", &["ipam-shared"])).is_err());
        register_ipam_provider(reg("ipam-clash-a", &["ipam-shared"])).unwrap();
        assert_eq!(
            ipam_provider_ids()
                .iter()
                .filter(|i| **i == "ipam-clash-a")
                .count(),
            1
        );
        assert!(ipam_provider_for("IPAM-SHARED").is_some());
        assert!(ipam_provider_for("ipam-nobody").is_none());
    }
}
