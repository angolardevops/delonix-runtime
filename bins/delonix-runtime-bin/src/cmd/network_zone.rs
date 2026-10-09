//! `kind: NetworkZone` — declares a cluster-native SDN zone and the vnets
//! inside it, realized by whichever `SegmentProvider` the RUNTIME has
//! configured (ADR-0049 addendum, closing the gap D3 names).
//!
//! **No `provider` field, deliberately — unlike `kind: NetworkGateway`
//! (ADR-0051).** The owner's own framing (this decision's discovery
//! conversation): the Kind stays transparent to WHICH infrastructure
//! realizes it. `cmd::network_zone_providers::register_configured` reads
//! `DELONIX_PROXMOX_*` once at startup and registers what it finds;
//! `delonix_sdn::segment::active_network_zone_provider` resolves by
//! COUNT (zero/one/ambiguous), never by a name this document would have to
//! carry. A tenant applying this manifest never learns it runs on Proxmox.
//!
//! **`metadata.name` is the ZONE's own Proxmox SDN id** (`pve-sdn-id`: a
//! lowercase letter then up to 7 more lowercase letters/digits — the
//! provider validates the exact format, this Kind does not repeat that
//! rule). `spec.vnets[]` are the vnets inside it.
//!
//! **Its own registry**, the same reason `kind: NetworkGateway`/`kind:
//! Service` have one: the target is a Proxmox cluster's PENDING SDN
//! configuration, which carries none of this engine's `delonix.io/stack`
//! labels to stamp.
//!
//! **What changes in place, and what does not**: a zone and its vnets are
//! created and deleted, never updated — `apply()` only ENSURES every vnet
//! CURRENTLY declared is present, and never retracts one dropped from the
//! list while others stay. A subnet's gateway and DHCP ranges are the
//! exception (ADR-0063 D2): changed in place, inside the transaction; its
//! CIDR is its identity and stays cold. A gateway change moves the
//! provider's IPAM gateway entry the moment it is staged, so a transaction
//! that fails is followed by a repair of that entry, recorded as a ledger
//! step ([`repair_gateways_step`]). The full document's teardown
//! (`--replace NetworkZone/<name>`, or dropping it under `stack apply
//! --prune`) removes every vnet the registry last recorded, then the zone.
//!
//! **Ownership on the cluster** (audit 62, §6 P1). A zone or vnet found
//! under the declared name is not this engine's by being there. Each record
//! carries an owner token ([`OwnerMark`]), saved before the first remote
//! write: the provider writes it into every vnet's `alias` (the vnet's one
//! free-text field) and refuses a vnet of that name without it. A ZONE has no
//! free-text field, so it is this engine's only when the record says this
//! engine CREATED it (`zone_owned`); an existing zone it did not create is
//! refused, never adopted, and never deleted by the teardown. A record from
//! before the token existed owns nothing it can prove — its teardown touches
//! nothing on the cluster and says so.
//!
//! **One transaction per apply and per teardown**
//! ([`SegmentProvider::transaction`]): refused up front when the cluster
//! carries someone else's staged SDN changes, rolled back if any step fails,
//! applied once when all succeed.
//!
//! **Teardown order is vnets, then the zone, then ONE commit**: a real
//! Proxmox node refuses to delete a zone a vnet still references
//! (`delonix_proxmox`'s own `Client::delete_sdn_zone` doc comment) — the
//! same referential-integrity reason `kind: NetworkGateway` removes rules
//! before aliases.

use std::collections::BTreeMap;

use super::kinds as k;
use super::manifest::{self, ManifestDoc};
use super::output::OutputFormat;
use super::util::state_root;
use delonix_model::{Error, Result};
use delonix_networking::dns::{DnsProvider, ZoneDns};
use delonix_networking::ipam::{
    normalize_mac, DhcpRange, IpamObserved, IpamProvider, IpamReservation, IpamSubnet,
};
use delonix_sdn::ownership::{OwnerMark, RemoveOutcome};
use delonix_sdn::segment::{EnsureOutcome, NetworkZoneSpec, SegmentProvider, VNetSpec};
use delonix_state::JsonStore;

/// `spec` of `kind: NetworkZone`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct NetworkZoneSpecDoc {
    #[serde(default)]
    pub vnets: Vec<VNetSpecInput>,
    /// The DNS server the provider registers the zone's guests in (ADR-0059
    /// F5c). Hot: a change converges live; the records the provider already
    /// wrote stay as they are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<DnsInput>,
    /// The IPAM controller (its id on the provider) the zone's subnets
    /// allocate from (ADR-0063 D1); `pve`, the provider's built-in one, when
    /// omitted. The controller is the provider administrator's — the engine
    /// never creates one — and one the provider does not have, or cannot
    /// serve a zone from, is refused before any write. Cold: the provider
    /// refuses to change it once the zone holds a subnet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipam: Option<String>,
}

/// `spec.dns` of a zone: a DNS controller the provider's administrator
/// registered, and the domain the guests' records go under. The provider
/// writes the records — an A and a PTR for each guest that gets an address
/// from a DHCP range, and for each subnet's gateway (`<vnet>-gw`).
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct DnsInput {
    /// The DNS controller (its id on the provider) the A records go to.
    pub server: String,
    /// The domain, without the trailing dot.
    pub zone: String,
    /// The controller the PTR records go to; none when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse_server: Option<String>,
}

impl DnsInput {
    fn port(&self) -> ZoneDns {
        ZoneDns {
            server: self.server.trim().to_string(),
            // DNS names do not distinguish case.
            zone: self.zone.trim().to_ascii_lowercase(),
            reverse_server: self.reverse_server.as_ref().map(|r| r.trim().to_string()),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct VNetSpecInput {
    pub name: String,
    #[serde(default)]
    pub alias: Option<String>,
    /// The vnet's subnets, allocated from the provider's IPAM (ADR-0059
    /// F5b). A subnet's `gateway` and `dhcpRange` are hot: a change is made
    /// in place (ADR-0063 D2). Its `cidr` is its identity: a changed,
    /// added or removed subnet replaces the document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subnets: Vec<SubnetInput>,
}

/// A subnet inside a vnet.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubnetInput {
    /// `a.b.c.d/len`, at its network address.
    pub cidr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    /// The ranges the zone's DHCP server hands out. Declaring one, or any
    /// reservation, makes the zone serve DHCP.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dhcp_range: Vec<DhcpRangeInput>,
    /// Addresses reserved per MAC. Hot: a change converges live.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reservations: Vec<ReservationInput>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct DhcpRangeInput {
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct ReservationInput {
    pub ip: String,
    pub mac: String,
}

/// Known fields of the `spec` (drift-guard, the pattern every other Kind's
/// spec uses).
pub const NETWORK_ZONE_SPEC_FIELDS: &[&str] = &["vnets", "dns", "ipam"];

/// Fields the reconciler compares.
///
/// `remote` is what the cluster holds for the zone under the record's owner
/// mark, observed on every plan (ADR-0059 D4): `in sync`, or each difference
/// from what the record declared. The manifest always wants `in sync`.
///
/// `subnets` (each subnet's vnet and CIDR, its identity) and `ipam` (the
/// controller, which the provider will not change under a subnet) are cold:
/// a change replaces the document. `subnetSettings` (each subnet's gateway
/// and DHCP ranges, ADR-0063 D2), `reservations` and `dns` are hot: an apply
/// writes the subnets in place, removes the reservations no longer declared
/// and makes the new ones, and writes the zone's DNS settings. All three are
/// read from the provider, not the record, so a change made by hand
/// converges too.
pub const RECONCILED_NETWORK_ZONE_FIELDS: &[&str] = &[
    "vnets",
    "subnets",
    "subnetSettings",
    "ipam",
    "dns",
    "reservations",
    "remote",
    "applied",
];

/// The `remote` field of a record that matches the cluster.
const IN_SYNC: &str = "in sync";

/// The `applied` field of a record whose last apply ran to its end. Anything
/// else is where it stopped (ADR-0059 D4): the manifest wants `complete`,
/// the field converges live, and applying again resumes.
const COMPLETE: &str = "complete";

/// A registered record: what was last declared, plus the ownership fields
/// every ownable Kind's own registry carries (mirrors `NetworkGatewayRecord`).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct NetworkZoneRecord {
    name: String,
    #[serde(default)]
    vnets: Vec<VNetSpecInput>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
    /// The owner token written into every vnet's alias. Empty in a record
    /// from before the token existed.
    #[serde(default)]
    owner: String,
    /// This engine created the zone itself (the zone has no field to carry
    /// the mark). Only then does the teardown remove it.
    #[serde(default)]
    zone_owned: bool,
    /// The segment provider that served this zone (ADR-0059 D3): the zone
    /// stays on it when a default changes. Empty in a record from before
    /// the field existed; resolution then falls to the default.
    #[serde(default)]
    provider: String,
    /// The last apply's one step — the cluster transaction — opened before
    /// it ran and settled after (ADR-0059 D4). Unsettled is where a process
    /// died.
    #[serde(default)]
    ledger: delonix_networking::ledger::StepLedger,
    /// The reservations this engine holds on the provider, updated after
    /// each one is made or removed: what a teardown releases, and what an
    /// apply compares the declared ones with (ADR-0059 F5b).
    #[serde(default)]
    reservations: Vec<ReservationRec>,
    /// The DNS settings this engine gave the zone (ADR-0059 F5c).
    #[serde(default)]
    dns: Option<DnsInput>,
    /// The IPAM controller the zone's subnets allocate from (ADR-0063 D1).
    /// `None` — and every record from before the field — is the default.
    #[serde(default)]
    ipam: Option<String>,
}

/// One reservation this engine holds.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ReservationRec {
    vnet: String,
    ip: String,
    mac: String,
}

impl ReservationRec {
    fn port(&self) -> IpamReservation {
        IpamReservation {
            vnet: self.vnet.clone(),
            ip: self.ip.clone(),
            mac: self.mac.clone(),
        }
    }
}

/// The declared subnets of `vnets`, as the port's type.
fn declared_subnets(vnets: &[VNetSpecInput]) -> Vec<IpamSubnet> {
    vnets
        .iter()
        .flat_map(|v| {
            v.subnets.iter().map(|s| IpamSubnet {
                vnet: v.name.clone(),
                cidr: s.cidr.trim().to_string(),
                gateway: s.gateway.clone(),
                dhcp_ranges: s
                    .dhcp_range
                    .iter()
                    .map(|r| DhcpRange {
                        start: r.start.clone(),
                        end: r.end.clone(),
                    })
                    .collect(),
            })
        })
        .collect()
}

/// The declared reservations of `vnets`, MAC in the node's form (an invalid
/// MAC is kept as written; validation names it).
fn declared_reservations(vnets: &[VNetSpecInput]) -> Vec<ReservationRec> {
    vnets
        .iter()
        .flat_map(|v| {
            v.subnets.iter().flat_map(|s| {
                s.reservations.iter().map(|r| ReservationRec {
                    vnet: v.name.clone(),
                    ip: r.ip.trim().to_string(),
                    mac: normalize_mac(&r.mac).unwrap_or_else(|| r.mac.clone()),
                })
            })
        })
        .collect()
}

/// Every subnet and reservation of a spec checked before anything is
/// touched; two reservations of one address are refused.
/// Refuses a `dns:` the provider would refuse or that would register
/// nobody: the node writes records only for addresses it hands out from a
/// DHCP range, so a zone with `dns` needs a subnet with one.
fn validate_dns(name: &str, spec: &NetworkZoneSpecDoc) -> Result<()> {
    let Some(dns) = &spec.dns else {
        return Ok(());
    };
    dns.port().validate(name)?;
    if !declared_subnets(&spec.vnets)
        .iter()
        .any(|s| !s.dhcp_ranges.is_empty())
    {
        return Err(Error::Invalid(super::po::tf(
            "NetworkZone/{name}: dns: no subnet declares a dhcpRange, and the provider registers \
             only the guests that get an address from one — add a dhcpRange, or drop `dns:`",
            &[("name", name)],
        )));
    }
    Ok(())
}

/// The `dns` field: `server|zone|reverse`, empty when the zone has none.
fn dns_field(dns: Option<&DnsInput>) -> String {
    dns_field_of(dns.map(DnsInput::port).as_ref())
}

fn dns_field_of(dns: Option<&ZoneDns>) -> String {
    dns.map(|p| {
        format!(
            "{}|{}|{}",
            p.server,
            p.zone.to_ascii_lowercase(),
            p.reverse_server.as_deref().unwrap_or("")
        )
    })
    .unwrap_or_default()
}

/// The `dns` field as the provider runs it for the record's zone: what a
/// plan compares the declared settings with. Settings put on the zone by hand
/// read as a hot change the next apply converges. A record that cannot be
/// observed (interrupted, no mark, provider without the role) keeps what it
/// recorded.
fn dns_held(rec: &NetworkZoneRecord) -> Result<String> {
    if rec.ledger.is_interrupted() || rec.owner.is_empty() {
        return Ok(dns_field(rec.dns.as_ref()));
    }
    let (provider_id, _) = resolve_provider(&rec.provider)?;
    let Some(dns) = dns_provider(provider_id)? else {
        return Ok(dns_field(rec.dns.as_ref()));
    };
    let observed = dns
        .observe(&rec.name)
        .map_err(at(provider_id, "observe_dns"))?;
    Ok(dns_field_of(observed.as_ref()))
}

/// The DNS provider of the zone's segment provider, or `None` when that
/// provider does not have the role.
fn dns_provider(provider_id: &str) -> Result<Option<Box<dyn DnsProvider>>> {
    delonix_networking::dns::dns_provider_for(provider_id)
        .transpose()
        .map_err(Error::from)
}

/// The DNS provider a zone that declares `dns:` needs. One without the role
/// cannot carry it.
fn resolve_dns(provider_id: &str) -> Result<Box<dyn DnsProvider>> {
    dns_provider(provider_id)?.ok_or_else(|| {
        delonix_networking::Error::ProviderNotRegistered(super::po::tf(
            "the segment provider '{provider}' has no dns role: a NetworkZone on it cannot \
             declare dns",
            &[("provider", provider_id)],
        ))
        .into()
    })
}

fn validate_addressing(vnets: &[VNetSpecInput]) -> Result<()> {
    let subnets = declared_subnets(vnets);
    for s in &subnets {
        s.validate()?;
    }
    let reservations = declared_reservations(vnets);
    for (i, r) in reservations.iter().enumerate() {
        r.port().validate(&subnets)?;
        if reservations[..i].iter().any(|o| o.ip == r.ip) {
            return Err(Error::Invalid(format!(
                "address {} is reserved twice in this zone",
                r.ip
            )));
        }
    }
    Ok(())
}

/// The `subnets` field: each subnet's identity, `vnet|cidr` (ADR-0063 D2.1 —
/// the CIDR is what the subnet is; a change of it is another subnet).
fn subnets_field(vnets: &[VNetSpecInput]) -> String {
    let mut items: Vec<String> = declared_subnets(vnets)
        .iter()
        .map(|s| format!("{}|{}", s.vnet, s.cidr))
        .collect();
    items.sort();
    items.join(";")
}

/// The `subnetSettings` field: each subnet's gateway and DHCP ranges,
/// `vnet|cidr|gateway|ranges` — what changes in place (ADR-0063 D2.1).
fn subnet_settings_field(subnets: &[IpamSubnet]) -> String {
    let mut items: Vec<String> = subnets
        .iter()
        .map(|s| {
            let mut ranges: Vec<String> = s
                .dhcp_ranges
                .iter()
                .map(|r| format!("{}-{}", r.start, r.end))
                .collect();
            ranges.sort();
            format!(
                "{}|{}|{}|{}",
                s.vnet,
                s.cidr,
                s.gateway.as_deref().unwrap_or(""),
                ranges.join(",")
            )
        })
        .collect();
    items.sort();
    items.join(";")
}

/// The controller a spec or record names, or the default — the one the
/// provider is asked to allocate from.
fn controller_of(ipam: Option<&String>) -> &str {
    ipam.map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(delonix_networking::ipam::DEFAULT_CONTROLLER)
}

/// The `ipam` field: the controller, when the zone has subnets that allocate
/// from it; empty otherwise (a zone without subnets allocates nothing).
fn ipam_field(ipam: Option<&String>, vnets: &[VNetSpecInput]) -> String {
    if declared_subnets(vnets).is_empty() {
        String::new()
    } else {
        controller_of(ipam).to_string()
    }
}

/// Refuses an `ipam:` the provider would refuse or that would allocate
/// nothing (ADR-0063 D1): a controller id the provider cannot be sent, or a
/// zone that declares no subnet. Whether the provider HAS the controller is
/// asked in the apply, before any write.
fn validate_ipam(name: &str, spec: &NetworkZoneSpecDoc) -> Result<()> {
    let Some(id) = spec.ipam.as_deref().map(str::trim) else {
        return Ok(());
    };
    if !delonix_networking::ipam::valid_controller_id(id) {
        return Err(Error::Invalid(super::po::tf(
            "NetworkZone/{name}: ipam '{id}' is not an IPAM controller id (a lowercase letter, \
             then up to 7 lowercase letters or digits)",
            &[("name", name), ("id", id)],
        )));
    }
    if declared_subnets(&spec.vnets).is_empty() {
        return Err(Error::Invalid(super::po::tf(
            "NetworkZone/{name}: ipam: no subnet is declared, so the zone allocates nothing from \
             '{id}' — declare a subnet, or drop `ipam:`",
            &[("name", name), ("id", id)],
        )));
    }
    Ok(())
}

fn reservations_field(rs: &[ReservationRec]) -> String {
    let mut items: Vec<String> = rs
        .iter()
        .map(|r| format!("{}|{}|{}", r.vnet, r.ip, r.mac))
        .collect();
    items.sort();
    items.join(";")
}

/// The IPAM provider of the zone's segment provider (ADR-0059 D1 rule 2:
/// the role is served by the provider that serves the zone). One without
/// the role cannot carry subnets.
fn resolve_ipam(provider_id: &str) -> Result<Box<dyn IpamProvider>> {
    match delonix_networking::ipam::ipam_provider_for(provider_id) {
        Some(built) => built.map_err(Error::from),
        None => Err(
            delonix_networking::Error::ProviderNotRegistered(super::po::tf(
                "the segment provider '{provider}' has no ipam role: a NetworkZone on it cannot \
             declare subnets",
                &[("provider", provider_id)],
            ))
            .into(),
        ),
    }
}

fn store() -> Result<JsonStore<NetworkZoneRecord>> {
    JsonStore::open(state_root().join("network-zones")).map_err(Into::into)
}

/// The segment provider for a zone (ADR-0059 D3): the one on its record,
/// then `networkDefaults.segment`, then — only without a `providers.yaml` —
/// the single registered one.
fn resolve_provider(recorded: &str) -> Result<(&'static str, Box<dyn SegmentProvider>)> {
    use delonix_networking::resolve::{Role, Wanted};
    let (default, config) = super::providers_config::network_default(Role::Segment)?;
    delonix_networking::segment::choose_segment_provider(&Wanted {
        role: Role::Segment,
        named: None,
        recorded: Some(recorded),
        default: default.as_deref(),
        config: config.as_deref(),
    })
    .map_err(|e| {
        let wanted = Some(recorded).filter(|r| !r.is_empty());
        Error::from(e).with_context(delonix_networking::resolve::context(
            Role::Segment,
            wanted.or(default.as_deref()),
            Some("resolve_provider"),
        ))
    })
}

/// Adds where a provider call failed to its error (ADR-0059 D5): the
/// provider, the segment role and the step.
fn at<'a>(provider: &'a str, step: &'static str) -> impl FnOnce(Error) -> Error + 'a {
    move |e| {
        e.with_context(delonix_networking::resolve::context(
            delonix_networking::resolve::Role::Segment,
            Some(provider),
            Some(step),
        ))
    }
}

/// A comparable summary of the vnet list — sorted so two applies of an
/// unchanged spec never drift because a manifest happened to list them in a
/// different order (same reasoning as `network_gateway::aliases_field`).
fn vnets_field(vnets: &[VNetSpecInput]) -> String {
    let mut items: Vec<String> = vnets
        .iter()
        .map(|v| format!("{}|{}", v.name, v.alias.as_deref().unwrap_or("")))
        .collect();
    items.sort();
    items.join(";")
}

fn record_fields(rec: &NetworkZoneRecord) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("vnets".into(), vnets_field(&rec.vnets));
    f.insert("subnets".into(), subnets_field(&rec.vnets));
    f.insert(
        "subnetSettings".into(),
        subnet_settings_field(&declared_subnets(&rec.vnets)),
    );
    f.insert("ipam".into(), ipam_field(rec.ipam.as_ref(), &rec.vnets));
    f.insert("reservations".into(), reservations_field(&rec.reservations));
    f.insert("dns".into(), dns_field(rec.dns.as_ref()));
    f
}

/// What the manifest declares, for the reconciler. `ownable: true` — a
/// `NetworkZone` has its own durable identity (name), same as `Service`/
/// `NetworkGateway`.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: NetworkZoneSpecDoc = manifest::spec_of(doc)?;
    let mut fields = BTreeMap::new();
    fields.insert("vnets".into(), vnets_field(&spec.vnets));
    fields.insert("subnets".into(), subnets_field(&spec.vnets));
    fields.insert(
        "subnetSettings".into(),
        subnet_settings_field(&declared_subnets(&spec.vnets)),
    );
    fields.insert("ipam".into(), ipam_field(spec.ipam.as_ref(), &spec.vnets));
    fields.insert("dns".into(), dns_field(spec.dns.as_ref()));
    fields.insert(
        "reservations".into(),
        reservations_field(&declared_reservations(&spec.vnets)),
    );
    fields.insert("remote".into(), IN_SYNC.into());
    fields.insert("applied".into(), COMPLETE.into());
    Ok(super::reconcile::Desired {
        kind: k::NETWORK_ZONE.into(),
        name: doc.metadata.name.clone(),
        fields,
        converges: true,
        ownable: true,
    })
}

/// Every declared `NetworkZone` — the enumeration `--prune` needs, same
/// reasoning as `network_gateway::actual`.
pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    store()?
        .list()?
        .into_iter()
        .map(|rec| {
            let mut fields = record_fields(&rec);
            fields.insert("subnetSettings".into(), subnet_settings_held(&rec)?);
            fields.insert("reservations".into(), reservations_held(&rec)?);
            fields.insert("dns".into(), dns_held(&rec)?);
            fields.insert("remote".into(), remote_field(&rec)?);
            fields.insert(
                "applied".into(),
                rec.ledger
                    .interruption()
                    .unwrap_or_else(|| COMPLETE.to_string()),
            );
            Ok(super::reconcile::Actual {
                kind: k::NETWORK_ZONE.into(),
                name: rec.name.clone(),
                fields,
                owner: rec.labels.get(super::reconcile::STACK_LABEL).cloned(),
                last_applied: rec
                    .annotations
                    .get(super::reconcile::LAST_APPLIED)
                    .and_then(|raw| super::reconcile::decode_last_applied(raw)),
            })
        })
        .collect()
}

/// The capabilities a zone needs from its provider: what its apply uses, and
/// what its plan digest covers (ADR-0059 D4) — the IPAM ones only when the
/// zone declares subnets, reservations or a DHCP range.
fn required_capabilities(
    spec: &NetworkZoneSpecDoc,
) -> Vec<delonix_compute::capability::Capability> {
    let vnets = &spec.vnets;
    use delonix_compute::capability::Capability as C;
    let mut out = vec![
        C::NetSegmentRemote,
        C::NetApplyStaged,
        C::NetOwnershipMarker,
        C::NetObserve,
    ];
    let subnets = declared_subnets(vnets);
    if !subnets.is_empty() {
        out.push(C::NetIpamProvider);
    }
    if !declared_reservations(vnets).is_empty() {
        out.push(C::NetIpamReservation);
    }
    if subnets.iter().any(|s| !s.dhcp_ranges.is_empty()) {
        out.push(C::NetIpamDhcp);
    }
    if spec.dns.is_some() {
        out.push(C::NetDnsRecords);
    }
    out
}

/// Whether a zone's addressing is part of what is observed: it declares
/// subnets, or the record holds some.
fn uses_ipam(vnets: &[VNetSpecInput], rec: &NetworkZoneRecord) -> bool {
    !declared_subnets(vnets).is_empty()
        || !declared_subnets(&rec.vnets).is_empty()
        || !rec.reservations.is_empty()
}

/// What the IPAM holds for the record's zone, in the record's vnets.
fn observe_ipam(provider_id: &str, rec: &NetworkZoneRecord) -> Result<IpamObserved> {
    let vnets: Vec<String> = rec.vnets.iter().map(|v| v.name.clone()).collect();
    resolve_ipam(provider_id)?
        .observe(&rec.name, &vnets)
        .map_err(at(provider_id, "observe_ipam"))
}

/// The record's vnets as the port's type, in this zone.
fn declared_vnets(rec: &NetworkZoneRecord) -> Vec<VNetSpec> {
    rec.vnets
        .iter()
        .map(|v| VNetSpec {
            name: v.name.clone(),
            zone: rec.name.clone(),
            alias: v.alias.clone(),
        })
        .collect()
}

/// What the cluster holds for the record's zone under its owner mark,
/// compared with what the record declared (ADR-0059 D4, observe; read-only).
/// A record without a mark owns nothing that can be observed, and says so
/// instead of claiming to be in sync.
fn remote_field(rec: &NetworkZoneRecord) -> Result<String> {
    if rec.ledger.is_interrupted() {
        // An apply stopped mid-way: what is missing on the cluster is what it
        // had not made live, and the `applied` field says so.
        return Ok(IN_SYNC.into());
    }
    if rec.owner.is_empty() {
        return Ok("not observed: the record predates owner marks".into());
    }
    let owner = OwnerMark::new(&rec.owner)?;
    let (provider_id, provider) = resolve_provider(&rec.provider)?;
    let observed = provider
        .observe(&rec.name, &owner)
        .map_err(at(provider_id, "observe"))?;
    let mut drift = delonix_sdn::segment::segment_drift(&rec.name, &declared_vnets(rec), &observed);
    if uses_ipam(&[], rec) {
        // Reservations and each subnet's gateway and ranges are compared in
        // their own (hot) fields; here only what is cold: the zone's options,
        // its controller, and which subnets exist.
        let subnets = declared_subnets(&rec.vnets);
        let dhcp = delonix_networking::ipam::zone_serves_dhcp(
            &subnets,
            !declared_reservations(&rec.vnets).is_empty(),
        );
        let observed = observe_ipam(provider_id, rec)?;
        drift.extend(delonix_networking::ipam::ipam_layout_drift(
            &subnets, dhcp, &observed,
        ));
        if !subnets.is_empty() {
            drift.extend(delonix_networking::ipam::controller_drift(
                controller_of(rec.ipam.as_ref()),
                &observed,
            ));
        }
    }
    Ok(if drift.is_empty() {
        IN_SYNC.to_string()
    } else {
        drift.join("; ")
    })
}

/// The `subnetSettings` field as the provider runs it: the gateway and DHCP
/// ranges of each of the record's subnets on the cluster. A gateway or a range
/// changed by hand reads as a hot change the next apply converges in place
/// (ADR-0063 D2), not as drift that would replace the zone. A declared subnet
/// that is not running is missing from this field and named by `remote`. A
/// record that cannot be observed keeps what it recorded.
fn subnet_settings_held(rec: &NetworkZoneRecord) -> Result<String> {
    let declared = declared_subnets(&rec.vnets);
    if rec.ledger.is_interrupted() || rec.owner.is_empty() || declared.is_empty() {
        return Ok(subnet_settings_field(&declared));
    }
    let (provider_id, _) = resolve_provider(&rec.provider)?;
    let running: Vec<IpamSubnet> = observe_ipam(provider_id, rec)?
        .subnets
        .into_iter()
        .filter(|s| {
            declared
                .iter()
                .any(|d| d.vnet == s.vnet && d.cidr == s.cidr)
        })
        .collect();
    Ok(subnet_settings_field(&running))
}

/// The `reservations` field as the provider holds it: the record's
/// reservations that the node still holds for their MAC. One released on the
/// node (by hand, or by destroying the guest that held the MAC) drops out, so
/// the plan reads it as a hot change and the next apply makes it again — not
/// as drift that would replace the zone. A record that cannot be observed
/// (interrupted, no mark, no addressing) keeps what it recorded.
fn reservations_held(rec: &NetworkZoneRecord) -> Result<String> {
    if rec.ledger.is_interrupted() || rec.owner.is_empty() || rec.reservations.is_empty() {
        return Ok(reservations_field(&rec.reservations));
    }
    let (provider_id, _) = resolve_provider(&rec.provider)?;
    let wanted: Vec<IpamReservation> = rec.reservations.iter().map(ReservationRec::port).collect();
    let held: Vec<ReservationRec> =
        delonix_networking::ipam::held_reservations(&wanted, &observe_ipam(provider_id, rec)?)
            .into_iter()
            .map(|r| ReservationRec {
                vnet: r.vnet,
                ip: r.ip,
                mac: r.mac,
            })
            .collect();
    Ok(reservations_field(&held))
}

/// The digest of this document's plan (ADR-0059 D4): what the manifest
/// declares, what the cluster holds for the zone under the record's mark
/// right now, the provider, the catalog version and the states of the
/// capabilities a zone uses. `None` when no provider resolves.
pub(crate) fn plan_digest(doc: &ManifestDoc) -> Result<Option<String>> {
    use delonix_compute::capability::CATALOG_VERSION;
    let rec = store()?.load(&doc.metadata.name).unwrap_or_default();
    let Ok((provider_id, provider)) = resolve_provider(&rec.provider) else {
        return Ok(None);
    };
    let observed = if rec.owner.is_empty() {
        Default::default()
    } else {
        provider
            .observe(&doc.metadata.name, &OwnerMark::new(&rec.owner)?)
            .map_err(at(provider_id, "observe"))?
    };
    let spec: NetworkZoneSpecDoc = manifest::spec_of(doc)?;
    let used = required_capabilities(&spec);
    let states: BTreeMap<String, String> = provider
        .capabilities()
        .capabilities
        .iter()
        .filter(|c| used.contains(&c.capability))
        .map(|c| (c.capability.name().to_string(), c.state.label().to_string()))
        .collect();
    let mut intent = desired(doc)?.fields;
    intent.remove("remote");
    intent.remove("applied");
    let mut fingerprint = delonix_networking::plan::segment_fingerprint(&observed);
    if uses_ipam(&spec.vnets, &rec) && !rec.owner.is_empty() {
        let ipam = delonix_networking::plan::ipam_fingerprint(&observe_ipam(provider_id, &rec)?);
        fingerprint = serde_json::json!({ "segment": fingerprint, "ipam": ipam });
    }
    // The same reading the `dns` field makes (`dns_held`): every zone of a
    // provider with the role, declared or not.
    if !rec.owner.is_empty() {
        if let Some(dns) = dns_provider(provider_id)? {
            let observed = dns
                .observe(&doc.metadata.name)
                .map_err(at(provider_id, "observe_dns"))?;
            fingerprint = serde_json::json!({ "zone": fingerprint, "dns": observed });
        }
    }
    Ok(Some(delonix_networking::plan::plan_digest(
        &intent,
        &fingerprint,
        provider_id,
        CATALOG_VERSION,
        &states,
    )))
}

/// The record's owner token, generating one when it has none yet.
fn owner_mark(rec: &mut NetworkZoneRecord) -> Result<OwnerMark> {
    if rec.owner.is_empty() {
        let mut bytes = [0u8; 16];
        delonix_state::cred_vault::random_bytes(&mut bytes)?;
        rec.owner = OwnerMark::from_random(&bytes).token().to_string();
    }
    Ok(OwnerMark::new(&rec.owner)?)
}

/// ADR-0063 D2.3 as one ledger step: puts the IPAM gateway entry of each of
/// the record's owned subnets back on its running gateway, where a discarded
/// gateway change left it. Opened and saved before it runs, settled and saved
/// after — a process that dies during it leaves the step open, and the next
/// apply repairs again. Each subnet repaired is said.
fn repair_gateways_step(
    s: &JsonStore<NetworkZoneRecord>,
    rec: &mut NetworkZoneRecord,
    provider_id: &str,
    ipam: &dyn IpamProvider,
    owner: &OwnerMark,
) -> Result<()> {
    let name = rec.name.clone();
    let vnets: Vec<String> = rec.vnets.iter().map(|v| v.name.clone()).collect();
    let step = rec.ledger.open("repair_gateways", &name);
    s.save(&name, rec)?;
    let done = ipam
        .repair_gateways(&name, &vnets, owner)
        .map_err(at(provider_id, "repair_gateways"));
    rec.ledger
        .settle(step, done.as_ref().map(|_| ()).map_err(|e| e.to_string()));
    s.save(&name, rec)?;
    for line in done.as_deref().unwrap_or_default() {
        println!(
            "{}",
            super::po::tf(
                "networkzone/{name}: IPAM gateway entry repaired — {line}",
                &[("name", &name), ("line", line)],
            )
        );
    }
    done.map(|_| ())
}

/// Applies one document, in ONE transaction on the cluster: ensures the
/// zone, then every declared vnet inside it (zone first — a vnet
/// referencing one that does not exist is refused by the provider), and
/// applies once. A zone that was already there and that this record did not
/// create is refused; so is a vnet without this record's mark. The record is
/// saved before the transaction (write-ahead: the token, and every vnet
/// about to be ensured — a teardown removes only what carries the mark) and
/// again after it, preserving any existing ownership stamp.
fn apply_one(doc: &ManifestDoc) -> Result<()> {
    let spec: NetworkZoneSpecDoc = manifest::spec_of(doc)?;
    let name = doc.metadata.name.clone();
    validate_addressing(&spec.vnets)?;
    validate_dns(&name, &spec)?;
    validate_ipam(&name, &spec)?;

    let s = store()?;
    let mut rec = s.load(&name).unwrap_or_default();
    let (provider_id, provider) = resolve_provider(&rec.provider)?;
    // Validate (ADR-0059 D4): before the record or the cluster is touched.
    delonix_networking::resolve::require_capabilities(
        &format!("NetworkZone/{name}"),
        &provider.capabilities(),
        &required_capabilities(&spec),
    )?;
    let subnets = declared_subnets(&spec.vnets);
    let declared = declared_reservations(&spec.vnets);
    let ipam = if subnets.is_empty() && rec.reservations.is_empty() {
        None
    } else {
        Some(resolve_ipam(provider_id)?)
    };
    let wanted_dns = spec.dns.as_ref().map(DnsInput::port);
    // Declared: the role is required. Not declared: settings left by an
    // earlier apply, or put on the zone by hand, are cleared when the
    // provider has the role.
    let dns = if wanted_dns.is_some() {
        Some(resolve_dns(provider_id)?)
    } else {
        dns_provider(provider_id)?
    };
    // Compared as the plan compares them: a change of case only is no change.
    let dns_changed =
        !rec.vnets.is_empty() && dns_field(rec.dns.as_ref()) != dns_field(spec.dns.as_ref());
    // The controllers are the cluster administrator's (the engine never
    // creates one): one the cluster does not have is refused before any write.
    if let (Some(dns), Some(want)) = (&dns, &wanted_dns) {
        let registered = dns
            .controllers()
            .map_err(at(provider_id, "dns_controllers"))?;
        let missing = delonix_networking::dns::missing_controllers(want, &registered);
        if !missing.is_empty() {
            return Err(delonix_networking::Error::RemotePrerequisiteMissing(super::po::tf(
                "DNS controller '{id}' on provider '{provider}' — the engine does not create DNS \
                 controllers: register it on the cluster (it holds the DNS server's credential)",
                &[("id", &missing.join("', '")), ("provider", provider_id)],
            ))
            .into());
        }
    }
    // The IPAM controller is the cluster administrator's too (ADR-0063 D1):
    // one the cluster does not have, or one the provider cannot serve a zone
    // from, is refused before any write.
    let controller = controller_of(spec.ipam.as_ref()).to_string();
    if let (Some(ipam), false) = (&ipam, subnets.is_empty()) {
        let registered = ipam
            .controllers()
            .map_err(at(provider_id, "ipam_controllers"))?;
        if !delonix_networking::ipam::missing_controllers(&controller, &registered).is_empty() {
            return Err(
                delonix_networking::Error::RemotePrerequisiteMissing(super::po::tf(
                    "IPAM controller '{id}' on provider '{provider}' — the engine does not create \
                 IPAM controllers: register it on the cluster (it holds the IPAM's credential)",
                    &[("id", &controller), ("provider", provider_id)],
                ))
                .into(),
            );
        }
        if let Some(c) = registered.iter().find(|c| c.id == controller) {
            ipam.refuse_unsupported(c)
                .map_err(at(provider_id, "ipam_controllers"))?;
        }
    }
    // The gateways the zone ran with before this apply, to say which ones a
    // DNS server keeps the old record of (ADR-0064).
    let prior_subnets = declared_subnets(&rec.vnets);
    let owner = owner_mark(&mut rec)?;
    rec.name = name.clone();
    rec.provider = provider_id.to_string();
    let mut vnets = rec.vnets.clone();
    vnets.retain(|o| !spec.vnets.iter().any(|n| n.name == o.name));
    vnets.extend(spec.vnets.iter().cloned());
    rec.vnets = vnets;
    // The zone has no field to carry a mark, so whether it is this record's
    // is the record's own word. A zone that is NOT running when this apply
    // starts is one this apply creates: claimed here, before the transaction,
    // so a process killed after the cluster committed and before the record
    // was saved does not come back to a zone it made and refuses as someone
    // else's. A transaction that FAILS takes the claim back.
    let had_zone = rec.zone_owned;
    if !had_zone
        && !provider
            .observe(&name, &owner)
            .map_err(at(provider_id, "observe"))?
            .zone_present
    {
        rec.zone_owned = true;
    }
    let interrupted = rec.ledger.interruption();
    if let Some(why) = &interrupted {
        println!(
            "{}",
            super::po::tf(
                "networkzone/{name}: the last run was {why} — applying again; the provider discards what it left staged",
                &[("name", &name), ("why", why)],
            )
        );
    }
    rec.ledger = Default::default();
    // A run that stopped may have staged a gateway change: discarding it does
    // not move the IPAM's gateway entry back (ADR-0063 D2.3). Repaired before
    // this run stages anything — and if the repair fails, nothing is staged.
    if let (Some(ipam), true) = (&ipam, interrupted.is_some()) {
        repair_gateways_step(&s, &mut rec, provider_id, ipam.as_ref(), &owner)?;
    }
    let step = rec.ledger.open("transaction", &name);
    s.save(&name, &rec)?;

    // What the closure trusts is what the record said BEFORE this run's
    // claim: a zone someone else staged and never applied is still refused.
    let zone_owned = had_zone;
    let mut created_zone = false;
    let outcome = provider.transaction(&mut || {
        match provider
            .ensure_zone(&NetworkZoneSpec { name: name.clone() })
            .map_err(at(provider_id, "ensure_zone"))?
        {
            EnsureOutcome::Created => created_zone = true,
            EnsureOutcome::AlreadyPresent if !zone_owned => {
                return Err(delonix_networking::Error::RemoteObjectNotOwned(super::po::tf(
                    "zone '{name}' already exists in the cluster's SDN and this engine did not \
                     create it — refusing to adopt it by name; pick another zone name, or \
                     remove the zone on the cluster if it is really stale",
                    &[("name", &name)],
                ))
                .into());
            }
            EnsureOutcome::AlreadyPresent => {}
        }
        for v in &spec.vnets {
            provider
                .ensure_vnet(
                    &VNetSpec {
                        name: v.name.clone(),
                        zone: name.clone(),
                        alias: v.alias.clone(),
                    },
                    &owner,
                )
                .map_err(at(provider_id, "ensure_vnet"))?;
        }
        // The addressing rides the same transaction (ADR-0059 F5b): the zone's
        // IPAM/DHCP options first — the node refuses an IPAM change once a
        // subnet exists — then each subnet.
        if let (Some(ipam), false) = (&ipam, subnets.is_empty()) {
            let dhcp = delonix_networking::ipam::zone_serves_dhcp(&subnets, !declared.is_empty());
            ipam.prepare_zone(&name, &controller, dhcp)
                .map_err(at(provider_id, "prepare_zone"))?;
        }
        // DNS before the subnets: the node registers a subnet's gateway when
        // the subnet is created (ADR-0059 F5c). It writes only what differs.
        if let Some(dns) = &dns {
            dns.prepare_zone(&name, wanted_dns.as_ref())
                .map_err(at(provider_id, "prepare_dns"))?;
        }
        if let (Some(ipam), false) = (&ipam, subnets.is_empty()) {
            for subnet in &subnets {
                ipam.ensure_subnet(&name, subnet, &owner)
                    .map_err(at(provider_id, "ensure_subnet"))?;
            }
        }
        Ok(())
    });
    rec.ledger.settle(
        step,
        outcome.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    );
    if outcome.is_err() {
        // A transaction can fail AFTER the cluster committed (measured: a
        // node's network reload that never ran). The zone this run created
        // is then running, and it is this record's: dropping the claim made
        // the next apply refuse its own zone, and a delete leave it behind.
        // When the zone cannot be read back the claim is kept — the closure
        // above decides again on the next run, against what is there.
        let committed = created_zone
            && provider
                .observe(&name, &owner)
                .map(|o| o.zone_present)
                .unwrap_or(true);
        rec.zone_owned = had_zone || committed;
        s.save(&name, &rec)?;
        // ADR-0063 D2.3: a gateway change this transaction staged moved the
        // IPAM's gateway entry, and the rollback left it there. Repaired now;
        // a repair that fails is a failed step, and the next apply repairs
        // again before it stages anything. The transaction's error is the
        // one returned.
        if let Some(ipam) = &ipam {
            if let Err(e) = repair_gateways_step(&s, &mut rec, provider_id, ipam.as_ref(), &owner) {
                eprintln!(
                    "{}",
                    super::po::tf(
                        "networkzone/{name}: the IPAM gateway repair after the failed transaction failed too: {error}",
                        &[("name", &name), ("error", &e.to_string())],
                    )
                );
            }
        }
    }
    outcome?;

    if created_zone {
        rec.zone_owned = true;
    }
    rec.vnets = spec.vnets.clone();
    rec.dns = spec.dns.clone();
    rec.ipam = spec.ipam.clone();
    s.save(&name, &rec)?;
    if spec.dns.is_some() {
        // ADR-0064: the node writes `<vnet>-gw` when a subnet is created and
        // never rewrites it, so a gateway changed in place keeps its old
        // address on the DNS server.
        for now in &subnets {
            if let Some(before) = prior_subnets.iter().find(|p| {
                p.vnet == now.vnet
                    && p.cidr == now.cidr
                    && p.gateway.is_some()
                    && p.gateway != now.gateway
            }) {
                println!(
                    "{}",
                    super::po::tf(
                        "networkzone/{name}: subnet {cidr} changed its gateway in place — the DNS \
                         record '{vnet}-gw' keeps the old address {old}; the provider never \
                         rewrites it",
                        &[
                            ("name", &name),
                            ("cidr", &now.cidr),
                            ("vnet", &now.vnet),
                            ("old", before.gateway.as_deref().unwrap_or_default()),
                        ],
                    )
                );
            }
        }
    }
    if dns_changed {
        // Measured on PVE 9.2.2: the node writes records when it hands out an
        // address or creates a subnet, and never rewrites them.
        println!(
            "{}",
            super::po::tf(
                "networkzone/{name}: dns changed — the records the provider already wrote (each \
                 subnet gateway's and each guest's) stay as they are; a guest is registered \
                 under the new settings when it next gets an address",
                &[("name", &name)],
            )
        );
    }

    // Reservations are immediate and need the subnet RUNNING: after the
    // transaction, one ledger step each, the record updated after every one.
    // The ones no longer declared go first, so an address can move to
    // another MAC in one apply.
    if let Some(ipam) = &ipam {
        let gone: Vec<ReservationRec> = rec
            .reservations
            .iter()
            .filter(|r| !declared.contains(r))
            .cloned()
            .collect();
        for r in gone {
            let step = rec.ledger.open("remove_reservation", &r.ip);
            s.save(&name, &rec)?;
            let done = ipam
                .remove_reservation(&name, &r.port())
                .map_err(at(provider_id, "remove_reservation"));
            rec.ledger
                .settle(step, done.as_ref().map(|_| ()).map_err(|e| e.to_string()));
            if done.is_ok() {
                rec.reservations.retain(|o| o != &r);
            }
            s.save(&name, &rec)?;
            done?;
        }
        // Every declared one, not only the new ones: ensuring is idempotent,
        // and one released on the node (by hand, or by destroying the guest
        // that held the MAC) comes back on the next apply.
        for r in declared.clone() {
            let step = rec.ledger.open("ensure_reservation", &r.ip);
            s.save(&name, &rec)?;
            let done = ipam
                .ensure_reservation(&name, &r.port())
                .map_err(at(provider_id, "ensure_reservation"));
            rec.ledger
                .settle(step, done.as_ref().map(|_| ()).map_err(|e| e.to_string()));
            if done.is_ok() && !rec.reservations.contains(&r) {
                rec.reservations.push(r);
            }
            s.save(&name, &rec)?;
            done?;
        }
    }
    rec.ledger.finish();
    s.save(&name, &rec)?;
    println!(
        "{}",
        super::po::tf(
            "networkzone/{name}: {vnets} vnet(s), {subnets} subnet(s), {reservations} reservation(s) on '{provider}'",
            &[
                ("name", &name),
                ("vnets", &spec.vnets.len().to_string()),
                ("subnets", &subnets.len().to_string()),
                ("reservations", &rec.reservations.len().to_string()),
                ("provider", provider_id),
            ],
        )
    );
    Ok(())
}

/// Applies every `kind: NetworkZone` document.
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, k::NETWORK_ZONE) {
        apply_one(doc)?;
    }
    Ok(())
}

/// `converge_and_stamp`'s live-update path — `apply_one` already fully
/// re-ensures the declared state, so converging IS applying.
pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    apply_one(doc)
}

/// Records ownership + last-applied — mirrors `network_gateway::stamp`.
pub(crate) fn stamp(name: &str, stack: &str, fields: &BTreeMap<String, String>) -> Result<()> {
    let s = store()?;
    let mut rec = s
        .load(name)
        .map_err(|_| Error::NotFound(format!("network zone: {name}")))?;
    rec.labels
        .insert(super::reconcile::STACK_LABEL.to_string(), stack.to_string());
    rec.labels.insert(
        super::reconcile::MANAGED_BY.to_string(),
        "delonix".to_string(),
    );
    rec.annotations.insert(
        super::reconcile::LAST_APPLIED.to_string(),
        super::reconcile::encode_last_applied(fields),
    );
    s.save(name, &rec).map_err(Into::into)
}

/// `--prune`/`stack destroy`'s teardown, and the generic `delete
/// networkzones` verb: removes every vnet the registry last recorded (not
/// just what a NEW spec says — a document being removed entirely has no new
/// spec to consult), then the zone, commits once, then drops the record.
/// Idempotent: a name with no record is not an error.
///
/// In one transaction: only vnets carrying the record's mark are removed,
/// and the zone only when the record says this engine created it; whatever
/// is left is named. A zone that still holds someone else's vnet is refused
/// by the node, and the whole teardown is rolled back.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    let s = store()?;
    let Ok(mut rec) = s.load(name) else {
        return Ok(());
    };
    if rec.owner.is_empty() {
        for v in &rec.vnets {
            report_left(
                name,
                "vnet",
                &v.name,
                "no owner mark (record predates marks)",
            );
        }
        report_left(name, "zone", name, "no owner mark (record predates marks)");
        return s.remove(name).map_err(Into::into);
    }
    let owner = OwnerMark::new(&rec.owner)?;
    let (provider_id, provider) = resolve_provider(&rec.provider)?;
    let subnets = declared_subnets(&rec.vnets);
    let ipam = if subnets.is_empty() && rec.reservations.is_empty() {
        None
    } else {
        Some(resolve_ipam(provider_id)?)
    };
    // Reservations first, outside the transaction (they are immediate): the
    // node refuses to delete a subnet that still holds one. The record drops
    // each one as it goes, so a teardown that stops resumes where it was.
    if let Some(ipam) = &ipam {
        for r in rec.reservations.clone() {
            if let RemoveOutcome::NotOwned(_) = ipam
                .remove_reservation(name, &r.port())
                .map_err(at(provider_id, "remove_reservation"))?
            {
                report_left(name, "reservation", &r.ip, "now held for another MAC");
            }
            rec.reservations.retain(|o| o != &r);
            s.save(name, &rec)?;
        }
    }
    let mut left: Vec<(String, String, String)> = Vec::new();
    provider.transaction(&mut || {
        left.clear();
        if let Some(ipam) = &ipam {
            for subnet in &subnets {
                ipam.remove_subnet(name, subnet, &owner)
                    .map_err(at(provider_id, "remove_subnet"))?;
            }
        }
        for v in &rec.vnets {
            if let RemoveOutcome::NotOwned(who) = provider
                .remove_vnet(&v.name, &owner)
                .map_err(at(provider_id, "remove_vnet"))?
            {
                left.push(("vnet".into(), v.name.clone(), who.describe()));
            }
        }
        if rec.zone_owned {
            provider
                .remove_zone(name)
                .map_err(at(provider_id, "remove_zone"))?;
        } else {
            left.push((
                "zone".into(),
                name.to_string(),
                "this engine did not create it".into(),
            ));
        }
        Ok(())
    })?;
    for (kind, object, why) in &left {
        report_left(name, kind, object, why);
    }
    // The node writes a subnet gateway's A and PTR and no node API removes
    // them (measured on PVE 9.2.2, ADR-0064): said out loud, never left
    // silently in someone's DNS.
    if let Some(dns) = rec.dns.as_ref().map(DnsInput::port) {
        let gateways: Vec<(String, String)> = subnets
            .iter()
            .filter_map(|s| s.gateway.clone().map(|g| (s.vnet.clone(), g)))
            .collect();
        for record in delonix_networking::dns::gateway_record_names(&dns, &gateways) {
            println!(
                "{}",
                super::po::tf(
                    "networkzone/{name}: dns record '{record}' and its PTR left on DNS server \
                     '{server}': the provider writes a subnet gateway's records and never \
                     removes them",
                    &[("name", name), ("record", &record), ("server", &dns.server)],
                )
            );
        }
    }
    s.remove(name).map_err(Into::into)
}

/// The audible half of a teardown that skipped an object.
fn report_left(name: &str, kind: &str, object: &str, why: &str) {
    println!(
        "{}",
        super::po::tf(
            "networkzone/{name}: {kind} '{object}' left on the cluster: {why}",
            &[
                ("name", name),
                ("kind", kind),
                ("object", object),
                ("why", why)
            ],
        )
    );
}

/// For `stack ls`/`describe`: declared vs. what the registry last recorded.
pub(crate) fn presence_of(doc: &ManifestDoc) -> (String, String) {
    let Some(rec) = store().ok().and_then(|s| s.load(&doc.metadata.name).ok()) else {
        return ("no".into(), "-".into());
    };
    (
        "yes".into(),
        super::po::tf(
            "{vnets} vnet(s)",
            &[("vnets", &rec.vnets.len().to_string())],
        ),
    )
}

/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: NetworkZoneSpecDoc = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

#[derive(serde::Serialize)]
struct NetworkZoneLsRow {
    name: String,
    vnets: usize,
    stack: Option<String>,
}

pub(crate) fn cmd_ls(format: OutputFormat) -> Result<()> {
    let format = super::config::resolve_output(&state_root(), format);
    let mut recs = store()?.list()?;
    recs.sort_by(|a, b| a.name.cmp(&b.name));

    let rows: Vec<NetworkZoneLsRow> = recs
        .iter()
        .map(|r| NetworkZoneLsRow {
            name: r.name.clone(),
            vnets: r.vnets.len(),
            stack: r.labels.get(super::reconcile::STACK_LABEL).cloned(),
        })
        .collect();

    if format == OutputFormat::Json {
        return super::output::print_json(&rows);
    }

    let mut t = super::output::Table::new(&["NAME", "VNETS", "STACK"]);
    for r in &rows {
        t.row(vec![
            r.name.clone(),
            r.vnets.to_string(),
            r.stack.clone().unwrap_or_else(|| "-".to_string()),
        ]);
    }
    t.drop_uninformative().print();
    Ok(())
}

pub(crate) fn cmd_describe(names: &[String]) -> Result<()> {
    let s = store()?;
    for name in names {
        let rec = s
            .load(name)
            .map_err(|_| Error::NotFound(format!("network zone: {name}")))?;
        let mut d = super::output::Describe::new();
        d.field("Name", &rec.name);
        d.field("Vnets", rec.vnets.len().to_string());
        if !rec.provider.is_empty() {
            d.field("Provider", &rec.provider);
        }
        for v in &rec.vnets {
            d.field(
                "  Vnet",
                format!("{} ({})", v.name, v.alias.as_deref().unwrap_or("-")),
            );
            for sn in &v.subnets {
                let ranges: Vec<String> = sn
                    .dhcp_range
                    .iter()
                    .map(|r| format!("{}-{}", r.start, r.end))
                    .collect();
                d.field(
                    "    Subnet",
                    format!(
                        "{} gw {} dhcp {}",
                        sn.cidr,
                        sn.gateway.as_deref().unwrap_or("-"),
                        if ranges.is_empty() {
                            "-".to_string()
                        } else {
                            ranges.join(",")
                        }
                    ),
                );
            }
        }
        for r in &rec.reservations {
            d.field("  Reservation", format!("{} {} ({})", r.ip, r.mac, r.vnet));
        }
        if let Some(dns) = &rec.dns {
            d.field(
                "DNS",
                format!(
                    "{} on '{}' (reverse: {})",
                    dns.zone,
                    dns.server,
                    dns.reverse_server.as_deref().unwrap_or("-")
                ),
            );
        }
        d.field_opt("Stack", rec.labels.get(super::reconcile::STACK_LABEL));
        d.field_opt("Managed by", rec.labels.get(super::reconcile::MANAGED_BY));
        d.print();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vnets_field_is_order_independent() {
        let v = vec![
            VNetSpecInput {
                name: "b".into(),
                alias: None,
                subnets: vec![],
            },
            VNetSpecInput {
                name: "a".into(),
                alias: Some("Prod".into()),
                subnets: vec![],
            },
        ];
        let mut v2 = v.clone();
        v2.reverse();
        assert_eq!(vnets_field(&v), vnets_field(&v2));
    }

    fn addressed() -> Vec<VNetSpecInput> {
        serde_yaml::from_str(
            r#"
- name: v1
  subnets:
    - cidr: 10.78.0.0/24
      gateway: 10.78.0.1
      dhcpRange: [{start: 10.78.0.100, end: 10.78.0.150}]
      reservations:
        - {ip: 10.78.0.20, mac: "bc:24:11:00:00:20"}
        - {ip: 10.78.0.21, mac: "BC:24:11:00:00:21"}
- name: v2
"#,
        )
        .unwrap()
    }

    #[test]
    fn subnets_and_reservations_read_from_the_manifest_shape() {
        let v = addressed();
        validate_addressing(&v).unwrap();
        let s = declared_subnets(&v);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].dhcp_ranges.len(), 1);
        let r = declared_reservations(&v);
        assert_eq!(r[0].mac, "BC:24:11:00:00:20", "the MAC in the node's form");
        assert_eq!(subnets_field(&v), "v1|10.78.0.0/24");
        assert_eq!(
            subnet_settings_field(&s),
            "v1|10.78.0.0/24|10.78.0.1|10.78.0.100-10.78.0.150"
        );
        let mut reversed = r.clone();
        reversed.reverse();
        assert_eq!(reservations_field(&r), reservations_field(&reversed));
        use delonix_compute::capability::Capability as C;
        let spec = |vnets: &[VNetSpecInput]| NetworkZoneSpecDoc {
            vnets: vnets.to_vec(),
            dns: None,
            ipam: None,
        };
        let caps = required_capabilities(&spec(&v));
        for c in [C::NetIpamProvider, C::NetIpamReservation, C::NetIpamDhcp] {
            assert!(caps.contains(&c), "{c:?}");
        }
        assert!(!caps.contains(&C::NetDnsRecords));
        assert_eq!(required_capabilities(&spec(&v[1..])).len(), 4);
    }

    fn with_dns(vnets: Vec<VNetSpecInput>) -> NetworkZoneSpecDoc {
        NetworkZoneSpecDoc {
            vnets,
            dns: Some(DnsInput {
                server: "pdnslab".into(),
                zone: "f5c.lab".into(),
                reverse_server: Some("pdnslab".into()),
            }),
            ipam: None,
        }
    }

    /// ADR-0063 D2.1, at plan time: a gateway or a range changed moves only
    /// the hot `subnetSettings` field; a changed CIDR moves the cold
    /// `subnets` field, so the plan replaces the document.
    #[test]
    fn a_gateway_change_is_hot_and_a_cidr_change_is_cold() {
        use super::super::reconcile::is_hot_change;
        let before = addressed();
        let mut gw = addressed();
        gw[0].subnets[0].gateway = Some("10.78.0.254".into());
        gw[0].subnets[0].dhcp_range[0].start = "10.78.0.10".into();
        assert_eq!(subnets_field(&before), subnets_field(&gw));
        assert_ne!(
            subnet_settings_field(&declared_subnets(&before)),
            subnet_settings_field(&declared_subnets(&gw))
        );
        let mut moved = addressed();
        moved[0].subnets[0].cidr = "10.79.0.0/24".into();
        assert_ne!(subnets_field(&before), subnets_field(&moved));

        let k = super::super::kinds::NETWORK_ZONE;
        assert!(is_hot_change(k, "subnetSettings", Some("a"), Some("b")));
        assert!(!is_hot_change(k, "subnets", Some("a"), Some("b")));
        assert!(!is_hot_change(k, "ipam", Some("pve"), Some("other")));
        for f in ["subnetSettings", "ipam"] {
            assert!(RECONCILED_NETWORK_ZONE_FIELDS.contains(&f), "{f}");
        }
    }

    /// ADR-0063 D1.1: the controller defaults to the built-in one, is
    /// compared only for a zone that allocates, and is refused when it cannot
    /// be sent or would allocate nothing.
    #[test]
    fn the_ipam_controller_defaults_and_is_refused_when_it_cannot_apply() {
        let v = addressed();
        assert_eq!(ipam_field(None, &v), "pve");
        assert_eq!(ipam_field(Some(&"pve".to_string()), &v), "pve");
        assert_eq!(ipam_field(Some(&"netbox1".to_string()), &v), "netbox1");
        assert_eq!(ipam_field(Some(&"netbox1".to_string()), &v[1..]), "");

        let spec = |ipam: &str, vnets: Vec<VNetSpecInput>| NetworkZoneSpecDoc {
            vnets,
            dns: None,
            ipam: Some(ipam.into()),
        };
        assert!(validate_ipam("z", &spec("pve", addressed())).is_ok());
        let e = validate_ipam("z", &spec("Net-Box", addressed()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("not an IPAM controller id"), "{e}");
        let e = validate_ipam("z", &spec("pve", addressed()[1..].to_vec()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("no subnet is declared"), "{e}");

        let parsed: NetworkZoneSpecDoc =
            serde_yaml::from_str("vnets: []\nipam: netbox1\n").unwrap();
        assert_eq!(parsed.ipam.as_deref(), Some("netbox1"));
        assert!(NETWORK_ZONE_SPEC_FIELDS.contains(&"ipam"));
    }

    #[test]
    fn dns_reads_from_the_manifest_shape_and_is_compared_without_case() {
        let spec: NetworkZoneSpecDoc = serde_yaml::from_str(
            "vnets: []\ndns: {server: pdnslab, zone: f5c.lab, reverseServer: rev}\n",
        )
        .unwrap();
        assert_eq!(dns_field(spec.dns.as_ref()), "pdnslab|f5c.lab|rev");
        assert_eq!(dns_field(None), "");
        let mut upper = spec.dns.clone().unwrap();
        upper.zone = "F5C.Lab".into();
        assert_eq!(dns_field(Some(&upper)), "pdnslab|f5c.lab|rev");
        assert!(RECONCILED_NETWORK_ZONE_FIELDS.contains(&"dns"));
        assert!(NETWORK_ZONE_SPEC_FIELDS.contains(&"dns"));
        use delonix_compute::capability::Capability as C;
        assert!(required_capabilities(&with_dns(addressed())).contains(&C::NetDnsRecords));
    }

    #[test]
    fn dns_without_a_dhcp_range_is_refused_because_it_registers_nobody() {
        assert!(validate_dns("z", &with_dns(addressed())).is_ok());
        let mut v = addressed();
        v[0].subnets[0].dhcp_range.clear();
        let e = validate_dns("z", &with_dns(v)).unwrap_err().to_string();
        assert!(e.contains("dhcpRange"), "{e}");
        let mut bad = with_dns(addressed());
        bad.dns.as_mut().unwrap().zone = "f5c.lab.".into();
        let e = validate_dns("z", &bad).unwrap_err().to_string();
        assert!(e.contains("not a domain name"), "{e}");
    }

    #[test]
    fn an_address_reserved_twice_or_outside_its_subnet_is_refused() {
        let mut v = addressed();
        v[0].subnets[0].reservations[1].ip = "10.78.0.20".into();
        let e = validate_addressing(&v).unwrap_err().to_string();
        assert!(e.contains("reserved twice"), "{e}");
        let mut v = addressed();
        v[0].subnets[0].reservations[1].ip = "10.79.0.5".into();
        let e = validate_addressing(&v).unwrap_err().to_string();
        assert!(e.contains("no subnet declared"), "{e}");
    }

    #[test]
    fn resolve_provider_names_what_to_configure_when_none_is() {
        // This test does not register anything of its own — it only asserts
        // the SHAPE of the refusal when the process-wide registry happens to
        // be empty. If another test in this binary registered a provider
        // first (the registry is process-wide), this is a false negative to
        // skip rather than a flake to chase — the real guarantee (zero
        // registered -> named refusal) is proven without the shared static
        // by `delonix_networking::segment::tests::zero_registered_names_what_to_configure`.
        if !delonix_sdn::segment::segment_provider_ids().is_empty()
            || matches!(super::super::providers_config::loaded(), Ok(Some(_)))
        {
            eprintln!("SKIP: a provider is registered, or a providers.yaml is in force");
            return;
        }
        // `Box<dyn SegmentProvider>` is not `Debug`, so `unwrap_err()`
        // does not apply — match instead.
        match resolve_provider("") {
            Ok(_) => panic!("expected a refusal"),
            Err(e) => assert!(e.to_string().contains("DELONIX_PROXMOX_URL"), "{e}"),
        }
    }
}
