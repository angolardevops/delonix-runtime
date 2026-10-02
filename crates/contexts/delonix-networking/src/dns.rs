//! The DNS role (ADR-0059 D1, F5c): which DNS server a segment provider
//! registers the guests of a zone in, and under which domain.
//!
//! A provider that serves segments can also publish names for what runs on
//! them. Proxmox's SDN does: a zone names a DNS controller, a domain and a
//! reverse controller, and the node writes the records itself. This port is
//! that role, kept apart from [`crate::ipam`] for the reason every role port
//! is (D1 rule 2): a segment provider without DNS must not answer for it.
//!
//! # What the engine declares, and what the node writes
//!
//! Measured on PVE 9.2.2 (`docs/adr/0064-…`):
//!
//! * the engine declares the zone's settings — the controller, the domain,
//!   the reverse controller. It never creates a controller: a controller
//!   carries a credential to a third-party server, registered on the cluster
//!   by its administrator;
//! * the NODE writes the records: an A and a PTR for a guest (named after the
//!   guest) when the IPAM gives it an address from a DHCP range, and for a
//!   subnet's gateway (`<vnet>-gw`) when the subnet is created. So a zone's
//!   DNS needs its IPAM and its DHCP — a zone with `dns` and no DHCP range
//!   registers nobody, and is refused;
//! * a reservation made through the IPAM API gets no record (the node passes
//!   no hostname);
//! * the node leaves records behind in three cases this port cannot repair
//!   (no node API removes them): a deleted subnet keeps its gateway's A and
//!   PTR; a changed gateway keeps the old address in `<vnet>-gw`; a renamed
//!   guest keeps its old A, which then outlives the guest. The caller says
//!   so out loud ([`gateway_record_names`]).

use crate::error::{Error, Result};

/// The DNS settings of a zone.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ZoneDns {
    /// The DNS controller (by id, registered on the provider) the forward
    /// records go to.
    pub server: String,
    /// The domain the records go under, without the trailing dot.
    pub zone: String,
    /// The controller the PTR records go to. `None`: no reverse records.
    pub reverse_server: Option<String>,
}

/// One DNS controller registered on the provider: its id and plugin type.
/// Never its credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsController {
    pub id: String,
    pub kind: String,
}

/// `true` for a DNS name the provider accepts as a domain: dot-separated
/// labels of letters, digits and `-`, none empty, none starting or ending
/// with `-`, at most 63 bytes each and 253 in all, no trailing dot.
pub fn valid_domain(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 || name.ends_with('.') {
        return false;
    }
    name.split('.').all(|l| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// `true` for a controller id the provider accepts: PVE 9.2.2's
/// `pve-sdn-dns-id`, a letter then letters and digits, at least 2 (read in
/// the node's `Dns/Plugin.pm`).
pub fn valid_controller_id(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b.iter().all(|c| c.is_ascii_alphanumeric())
}

impl ZoneDns {
    /// Refuses a setting the provider would refuse, before any write.
    pub fn validate(&self, zone: &str) -> Result<()> {
        let bad = |why: String| -> Error {
            delonix_model::Error::Invalid(format!("NetworkZone/{zone}: dns: {why}")).into()
        };
        if !valid_controller_id(&self.server) {
            return Err(bad(format!(
                "server '{}' is not a DNS controller id (a letter, then letters and digits, at least 2)",
                self.server
            )));
        }
        if let Some(r) = &self.reverse_server {
            if !valid_controller_id(r) {
                return Err(bad(format!(
                    "reverseServer '{r}' is not a DNS controller id (a letter, then letters and digits, at least 2)"
                )));
            }
        }
        if !valid_domain(&self.zone) {
            return Err(bad(format!(
                "zone '{}' is not a domain name (labels of letters, digits and '-', no trailing dot)",
                self.zone
            )));
        }
        Ok(())
    }

    /// The controller ids this setting names, forward first.
    pub fn controllers(&self) -> Vec<&str> {
        let mut out = vec![self.server.as_str()];
        if let Some(r) = self.reverse_server.as_deref() {
            if r != self.server {
                out.push(r);
            }
        }
        out
    }
}

/// The controllers `dns` names that `registered` does not hold, in order.
pub fn missing_controllers(dns: &ZoneDns, registered: &[DnsController]) -> Vec<String> {
    dns.controllers()
        .into_iter()
        .filter(|id| !registered.iter().any(|c| c.id == *id))
        .map(str::to_string)
        .collect()
}

/// Each difference between the DNS settings the record declared and the ones
/// the provider runs for the zone, one sentence each. Empty: in sync.
pub fn dns_drift(declared: Option<&ZoneDns>, observed: Option<&ZoneDns>) -> Vec<String> {
    match (declared, observed) {
        (None, None) => Vec::new(),
        (None, Some(o)) => vec![format!(
            "the zone registers names in DNS controller '{}' (domain '{}'), and none was declared",
            o.server, o.zone
        )],
        (Some(d), None) => vec![format!(
            "the zone registers no names in DNS; declared: controller '{}', domain '{}'",
            d.server, d.zone
        )],
        (Some(d), Some(o)) => {
            let mut out = Vec::new();
            if d.server != o.server {
                out.push(format!(
                    "dns server is '{}', declared '{}'",
                    o.server, d.server
                ));
            }
            if !d.zone.eq_ignore_ascii_case(&o.zone) {
                out.push(format!("dns zone is '{}', declared '{}'", o.zone, d.zone));
            }
            if d.reverse_server != o.reverse_server {
                let show = |v: &Option<String>| v.clone().unwrap_or_else(|| "none".into());
                out.push(format!(
                    "dns reverseServer is '{}', declared '{}'",
                    show(&o.reverse_server),
                    show(&d.reverse_server)
                ));
            }
            out
        }
    }
}

/// The fully qualified names of the gateway records a provider wrote for the
/// subnets of `vnets` (each a `(vnet, gateway address)` pair) under `dns`:
/// `<vnet>-gw.<zone>`. What a teardown names as left on the DNS server — the
/// node writes them and no node API removes them.
pub fn gateway_record_names(dns: &ZoneDns, gateways: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = gateways
        .iter()
        .map(|(vnet, ip)| format!("{vnet}-gw.{} (A {ip})", dns.zone))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// A backend that registers the guests of the segments it serves in DNS.
/// Extends the provider skeleton (ADR-0059 D1 rule 4).
pub trait DnsProvider: delonix_compute::vm_provider::Provider {
    /// `true` if this backend can be used right now. Never a round trip.
    fn available(&self) -> bool;

    /// The DNS controllers registered on the provider. Read-only; never
    /// returns a credential.
    fn controllers(&self) -> delonix_model::Result<Vec<DnsController>>;

    /// Sets (`Some`) or clears (`None`) the zone's DNS settings (staged;
    /// inside the segment transaction, before the zone's subnets). Only a
    /// zone the caller owns is passed here. Writes only what differs.
    fn prepare_zone(&self, zone: &str, dns: Option<&ZoneDns>) -> delonix_model::Result<()>;

    /// The DNS settings the provider RUNS for `zone`; `None` when it has
    /// none (or the zone is not running). Read-only.
    fn observe(&self, zone: &str) -> delonix_model::Result<Option<ZoneDns>>;
}

/// Builds a [`DnsProvider`], or reports why it could not.
pub type DnsProviderFactory = Box<dyn Fn() -> Result<Box<dyn DnsProvider>> + Send + Sync>;

/// One provider registered for the DNS role (ADR-0059 D1 rule 2).
pub struct DnsProviderRegistration {
    /// Canonical id. Must equal what the built provider's `id` returns.
    pub id: &'static str,
    /// Extra spellings accepted from a caller; never repeats `id`.
    pub aliases: &'static [&'static str],
    pub new: DnsProviderFactory,
}

static DNS_PROVIDERS: std::sync::OnceLock<std::sync::RwLock<Vec<DnsProviderRegistration>>> =
    std::sync::OnceLock::new();

fn dns_providers() -> &'static std::sync::RwLock<Vec<DnsProviderRegistration>> {
    DNS_PROVIDERS.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

/// Adds a provider to the DNS registry. Idempotent by id; does no I/O. An
/// empty id, or a name that already belongs to a DIFFERENT provider, is
/// refused — the rules every role registry follows.
pub fn register_dns_provider(reg: DnsProviderRegistration) -> Result<()> {
    if reg.id.trim().is_empty() {
        return Err(Error::NetworkZoneProviderRegistrationRefused(
            "a dns provider registration needs an id".into(),
        ));
    }
    let mut guard = dns_providers().write().unwrap_or_else(|e| e.into_inner());
    for name in std::iter::once(&reg.id).chain(reg.aliases.iter()) {
        let want = name.trim().to_lowercase();
        if let Some(clash) = guard
            .iter()
            .find(|b| b.id != reg.id && (b.id == want || b.aliases.contains(&want.as_str())))
        {
            return Err(Error::NetworkZoneProviderRegistrationRefused(format!(
                "dns provider '{}' cannot claim the name '{}': it already belongs to '{}'",
                reg.id, name, clash.id
            )));
        }
    }
    guard.retain(|b| b.id != reg.id);
    guard.push(reg);
    Ok(())
}

/// Builds the DNS provider registered under `name` (id or alias), or `None`
/// when nothing is. Looked up by the id of the provider that serves the zone.
pub fn dns_provider_for(name: &str) -> Option<Result<Box<dyn DnsProvider>>> {
    let want = name.trim().to_lowercase();
    let guard = dns_providers().read().unwrap_or_else(|e| e.into_inner());
    guard
        .iter()
        .find(|r| r.id == want || r.aliases.contains(&want.as_str()))
        .map(|r| (r.new)())
}

/// The registered ids, in registration order.
pub fn dns_provider_ids() -> Vec<&'static str> {
    let guard = dns_providers().read().unwrap_or_else(|e| e.into_inner());
    guard.iter().map(|r| r.id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dns() -> ZoneDns {
        ZoneDns {
            server: "pdnslab".into(),
            zone: "f5c.lab".into(),
            reverse_server: Some("pdnslab".into()),
        }
    }

    #[test]
    fn a_domain_is_labels_without_a_trailing_dot() {
        assert!(valid_domain("f5c.lab"));
        assert!(valid_domain("a-b.example.org"));
        assert!(!valid_domain("f5c.lab."));
        assert!(!valid_domain(""));
        assert!(!valid_domain("a..b"));
        assert!(!valid_domain("-a.b"));
        assert!(!valid_domain("a_b.c"));
        assert!(!valid_domain(&format!("{}.lab", "x".repeat(64))));
    }

    #[test]
    fn a_controller_id_is_an_sdn_id() {
        assert!(valid_controller_id("pdnslab"));
        assert!(!valid_controller_id("pdns-lab"));
        assert!(!valid_controller_id("1pdns"));
        assert!(valid_controller_id("alongcontrollerid"));
        assert!(!valid_controller_id("p"));
        assert!(!valid_controller_id(""));
    }

    #[test]
    fn validate_names_the_field_it_refuses() {
        assert!(dns().validate("z").is_ok());
        let mut d = dns();
        d.zone = "bad_zone".into();
        assert!(d
            .validate("z")
            .unwrap_err()
            .to_string()
            .contains("zone 'bad_zone'"));
        let mut d = dns();
        d.reverse_server = Some("bad-id".into());
        assert!(d
            .validate("z")
            .unwrap_err()
            .to_string()
            .contains("reverseServer"));
    }

    #[test]
    fn the_controllers_named_are_listed_once() {
        assert_eq!(dns().controllers(), vec!["pdnslab"]);
        let mut d = dns();
        d.reverse_server = Some("rev".into());
        assert_eq!(d.controllers(), vec!["pdnslab", "rev"]);
        let registered = vec![DnsController {
            id: "pdnslab".into(),
            kind: "powerdns".into(),
        }];
        assert!(missing_controllers(&dns(), &registered).is_empty());
        assert_eq!(missing_controllers(&d, &registered), vec!["rev"]);
    }

    #[test]
    fn drift_names_each_difference() {
        assert!(dns_drift(None, None).is_empty());
        assert!(dns_drift(Some(&dns()), Some(&dns())).is_empty());
        assert_eq!(dns_drift(Some(&dns()), None).len(), 1);
        assert_eq!(dns_drift(None, Some(&dns())).len(), 1);
        let mut o = dns();
        let mut same = dns();
        same.zone = "F5C.LAB".into();
        assert!(dns_drift(Some(&dns()), Some(&same)).is_empty());
        o.zone = "other.lab".into();
        o.reverse_server = None;
        let d = dns_drift(Some(&dns()), Some(&o));
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(d[0].contains("other.lab"));
        assert!(d[1].contains("reverseServer is 'none'"));
    }

    #[test]
    fn gateway_records_are_named_after_the_vnet() {
        let names = gateway_record_names(
            &dns(),
            &[
                ("v1".into(), "10.84.0.1".into()),
                ("v1".into(), "10.84.0.1".into()),
            ],
        );
        assert_eq!(names, vec!["v1-gw.f5c.lab (A 10.84.0.1)"]);
    }
}
