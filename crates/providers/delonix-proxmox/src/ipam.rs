//! `IpamProvider` for Proxmox VE's own SDN IPAM (ADR-0059 F5b): the subnets
//! of a vnet, their DHCP ranges served by the per-zone `dnsmasq`, and the
//! addresses the built-in `pve` IPAM reserves per MAC.
//!
//! Every fact this module leans on was measured on PVE 9.2.2:
//!
//! * a zone without `ipam` takes `POST …/ips` with a success answer and
//!   stores NOTHING, so a reservation is read back after it is made;
//! * a reservation is refused until its subnet is RUNNING, and is immediate
//!   (no staging): it is made after the segment transaction committed;
//! * the node refuses to delete a subnet that still holds a reservation
//!   ("not empty") and a vnet that still holds a subnet;
//! * `dnsmasq` serves only the MACs written in its `ethers` file
//!   (`dhcp-ignore=tag:!known`), and a guest's start writes its MAC there with
//!   the address the IPAM holds for it — so the reservation IS what the guest
//!   gets by DHCP.
//!
//! It shares the [`Client`] of the segment provider (`lib.rs`'s
//! `register_segment_provider`): the staged writes here run inside that
//! provider's transaction and carry the SDN lock token the client holds.

use crate::sdn::{dhcp_ranges_of, DhcpRange, SubnetOptions};
use crate::{Client, Ledger};
use delonix_networking::ipam::{
    gateway_held_by, normalize_mac, subnet_change, IpamController, IpamEntry, IpamObserved,
    IpamProvider, IpamReservation, IpamSubnet, SubnetChange,
};
use delonix_networking::ownership::{Owner, OwnerMark, RemoveOutcome};
use delonix_networking::segment::EnsureOutcome;

/// The IPAM whose allocations this provider reads back
/// (`GET /cluster/sdn/ipams/pve/status`): Proxmox's built-in one, the only
/// plugin that needs no external server — and the only one that route
/// answers for (read in the node's `API2/Network/SDN/Ipams.pm`: `die
/// "Currently only PVE IPAM is supported!" if $id ne 'pve'`). It is also the
/// only controller kind [`ProxmoxIpamProvider::refuse_unsupported`] accepts
/// (ADR-0063 D1).
pub const IPAM: &str = "pve";

/// Why this provider refuses a zone on a controller of plugin `kind`, or
/// `None` for the one it serves (ADR-0063 D1.2, D1.3). Pure.
///
/// * `phpipam`: the node's plugin cannot map a MAC to an address
///   (`get_ips_from_mac` ends in `die "parsing of result not yet
///   implemented"`, `Network/SDN/Ipams/PhpIpamPlugin.pm`, PVE 9.2.2), and
///   that lookup is how a starting guest's reserved address reaches the
///   zone's `dnsmasq` — a reservation would be stored and never served.
/// * `netbox`: accepted only after the live spike ADR-0063 D1.3/D1.4 names;
///   until then the node's status route refuses it, so this provider could
///   neither read a reservation back nor see a gateway entry.
/// * any other plugin: never measured.
pub(crate) fn unsupported_reason(kind: &str) -> Option<String> {
    match kind {
        IPAM => None,
        "phpipam" => Some(
            "unsupported-by-provider: the node's phpIPAM plugin cannot map a MAC to an address \
             (`get_ips_from_mac` dies with \"parsing of result not yet implemented\", PVE 9.2.2), \
             so a reservation would be stored in phpIPAM and never served by DHCP (ADR-0063 D1.2)"
                .into(),
        ),
        "netbox" => Some(
            "not accepted yet: a NetBox-backed zone waits for the live spike of ADR-0063 D1.3 — \
             the node's IPAM status route refuses every IPAM but 'pve', so the engine could not \
             read a reservation or a gateway entry back"
                .into(),
        ),
        other => Some(format!(
            "IPAM plugin '{other}' was never measured against this provider (ADR-0063 D1)"
        )),
    }
}

/// A controller row of `GET /cluster/sdn/ipams` as the port's type: its id
/// and plugin type, never its credential.
pub(crate) fn controller_from(row: &serde_json::Value) -> Option<IpamController> {
    let id = row.get("ipam")?.as_str()?.to_string();
    let kind = row
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some(IpamController { id, kind })
}

/// Whether an IPAM status row is a subnet's GATEWAY entry: the node marks it
/// `"gateway": 1` (measured on PVE 9.2.2) and gives it no MAC.
fn is_gateway_row(row: &serde_json::Value) -> bool {
    match row.get("gateway") {
        Some(serde_json::Value::Number(n)) => n.as_u64() == Some(1),
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s == "1",
        _ => false,
    }
}

/// The gateway entries of `zone` in an IPAM status listing, as
/// `(vnet, address)`. Pure.
pub(crate) fn gateway_entries_from(
    rows: &[serde_json::Value],
    zone: &str,
) -> Vec<(String, String)> {
    rows.iter()
        .filter(|r| text(r, "zone") == zone && is_gateway_row(r))
        .map(|r| (text(r, "vnet"), text(r, "ip")))
        .collect()
}

/// One subnet whose IPAM gateway entries do not match its running gateway
/// (ADR-0063 D2.3), and what the repair does about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GatewayRepair {
    pub vnet: String,
    pub cidr: String,
    /// The running subnet's gateway: where the ONE gateway entry belongs.
    /// `None` when the subnet runs without a gateway, so no entry belongs.
    pub gateway: Option<String>,
    /// The gateway entries the IPAM holds on any OTHER address — released.
    pub stale: Vec<String>,
    /// A free address of the subnet the gateway is moved through, when no
    /// entry is on the running gateway; `None` when nothing is moved.
    pub through: Option<String>,
}

/// Whether `ip` is an address of the subnet a repair is about.
fn r_holds(r: &GatewayRepair, ip: &str) -> bool {
    IpamSubnet {
        vnet: r.vnet.clone(),
        cidr: r.cidr.clone(),
        gateway: None,
        dhcp_ranges: Vec::new(),
    }
    .holds(ip)
}

/// The first address of `subnet`, from the top down, that is neither the
/// network nor the broadcast address, nor its gateway, nor held by any entry
/// of the vnet, nor inside a DHCP range. Pure.
pub(crate) fn free_address(subnet: &IpamSubnet, entries: &[IpamEntry]) -> Option<String> {
    let (base, len) = subnet.cidr.split_once('/')?;
    let base = u32::from(base.parse::<std::net::Ipv4Addr>().ok()?);
    let len: u32 = len.parse().ok()?;
    if len >= 31 {
        return None;
    }
    let size = 1u32 << (32 - len);
    let ip = |t: &str| t.parse::<std::net::Ipv4Addr>().ok().map(u32::from);
    let in_range = |a: u32| {
        subnet
            .dhcp_ranges
            .iter()
            .any(|r| match (ip(&r.start), ip(&r.end)) {
                (Some(s), Some(e)) => (s..=e).contains(&a),
                _ => false,
            })
    };
    (base + 1..base + size - 1).rev().find_map(|a| {
        let text = std::net::Ipv4Addr::from(a).to_string();
        let held = entries
            .iter()
            .any(|e| e.vnet == subnet.vnet && e.ip == text);
        (subnet.gateway.as_deref() != Some(text.as_str()) && !held && !in_range(a)).then_some(text)
    })
}

/// The repairs ADR-0063 D2.3 calls for: a running subnet ends with exactly
/// one gateway entry, on its running gateway, or none when it runs without
/// a gateway. Three states, each measured on PVE 9.2.2 after a gateway
/// change was staged and rolled back:
///
/// * a gateway MOVED: the entry stays on the staged address, none on the
///   running gateway — moved through a free address and back, the stale
///   one released;
/// * a gateway REMOVED (`delete=gateway`): the entry is gone and the
///   running gateway's address is free (the node then accepts a
///   reservation of the router's address for a guest) — moved through a
///   free address and back, which puts the entry back;
/// * a gateway ADDED to a subnet that had none: the subnet runs without one
///   and the IPAM holds a gateway entry — released.
///
/// A right entry next to stale ones (a repair cut short before its release)
/// only releases the stale ones. `Err` names a subnet with no free address
/// to move through. Pure.
pub(crate) fn gateway_repairs(
    running: &[IpamSubnet],
    gateway_entries: &[(String, String)],
    entries: &[IpamEntry],
) -> Result<Vec<GatewayRepair>, String> {
    let mut out = Vec::new();
    for s in running {
        let mine: Vec<&str> = gateway_entries
            .iter()
            .filter(|(vnet, ip)| *vnet == s.vnet && s.holds(ip))
            .map(|(_, ip)| ip.as_str())
            .collect();
        let gateway = s.gateway.as_deref();
        let stale: Vec<String> = mine
            .iter()
            .filter(|ip| Some(**ip) != gateway)
            .map(|ip| (*ip).to_string())
            .collect();
        let through = match gateway {
            Some(g) if !mine.contains(&g) => Some(free_address(s, entries).ok_or_else(|| {
                format!(
                    "subnet {} in vnet '{}': no free address to move the gateway through",
                    s.cidr, s.vnet
                )
            })?),
            _ => None,
        };
        if stale.is_empty() && through.is_none() {
            continue;
        }
        out.push(GatewayRepair {
            vnet: s.vnet.clone(),
            cidr: s.cidr.clone(),
            gateway: gateway.map(str::to_string),
            stale,
            through,
        });
    }
    Ok(out)
}

/// What a repair says it did, one line per subnet.
fn repair_line(r: &GatewayRepair) -> String {
    let at = format!("subnet {} in vnet '{}'", r.cidr, r.vnet);
    match (&r.gateway, r.stale.as_slice(), &r.through) {
        (Some(g), [], Some(_)) => {
            format!("{at}: the IPAM held no gateway entry, put back on {g}")
        }
        (Some(g), stale, Some(_)) => format!(
            "{at}: the IPAM gateway entry was on {}, put back on {g}",
            stale.join(", ")
        ),
        (Some(g), stale, None) => format!(
            "{at}: released the extra IPAM gateway entries on {} (the entry on {g} stays)",
            stale.join(", ")
        ),
        (None, stale, _) => format!(
            "{at}: the subnet runs without a gateway; released the IPAM gateway entries on {}",
            stale.join(", ")
        ),
    }
}

/// The [`IpamProvider`] for the cluster's SDN.
pub struct ProxmoxIpamProvider {
    client: std::sync::Arc<Client>,
    ledger: Ledger,
}

impl ProxmoxIpamProvider {
    pub fn new(client: std::sync::Arc<Client>, ledger: Ledger) -> Self {
        Self { client, ledger }
    }

    /// Refuses unless `vnet` (pending configuration, the one a write inside
    /// the transaction sees) carries `owner`'s mark. `Ok(false)` when there is
    /// no such vnet.
    fn owned_vnet(&self, vnet: &str, owner: &OwnerMark) -> delonix_model::Result<bool> {
        let vnets = self
            .client
            .sdn_vnets()
            .map_err(delonix_model::Error::from)?;
        let Some(row) = vnets
            .iter()
            .find(|v| v.get("vnet").and_then(|s| s.as_str()) == Some(vnet))
        else {
            return Ok(false);
        };
        let found = owner.owner_of(row.get("alias").and_then(|v| v.as_str()).unwrap_or(""));
        if found != Owner::Ours {
            return Err(delonix_networking::Error::RemoteObjectNotOwned(format!(
                "vnet '{vnet}' is {} — a subnet is this engine's only inside its own vnet",
                found.describe()
            ))
            .into());
        }
        Ok(true)
    }

    /// The DHCP server of a zone is the NODE's own `dnsmasq`, so a guest's
    /// request is input to the node. Measured on PVE 9.2.2: with the
    /// datacenter firewall on and the default input policy (DROP), every
    /// DHCPDISCOVER reached the vnet's bridge and died in `PVEFW-HOST-IN`; one
    /// inbound udp/67 rule on the vnet and the guest got its reserved address.
    /// The engine neither writes nor reads the node's firewall rules
    /// (administration, ADR-0049 D3) — only the datacenter switch it already
    /// reads for `scope: vm` — so it names the condition, without claiming the
    /// rule is missing. A read that fails says nothing: this is advice.
    fn warn_if_the_node_firewall_drops_dhcp(&self, zone: &str) {
        let Ok(o) = self.client.cluster_firewall_options() else {
            return;
        };
        let enabled = o.get("enable").and_then(serde_json::Value::as_u64) == Some(1);
        let policy = o
            .get("policy_in")
            .and_then(|v| v.as_str())
            .unwrap_or("DROP");
        if enabled && !policy.eq_ignore_ascii_case("ACCEPT") {
            tracing::warn!(
                zone,
                policy_in = policy,
                "the datacenter firewall is on and drops input by default, and the zone's DHCP \
                 server runs on the node: unless each node's firewall lets udp/67 in on the zone's \
                 vnets (e.g. `IN ACCEPT -i <vnet> -p udp -dport 67`), no guest gets an address — \
                 the engine reads the datacenter switch, not the node's rules (ADR-0049 D3)"
            );
        }
    }

    /// The reservation is held; releases every OTHER address the MAC holds in
    /// the vnet, which is the one its guest would be served (measured: a guest
    /// created after the reservation gets a range address of its own). A guest
    /// already running keeps the lease it has (the leases are infinite) until
    /// it restarts, and the warning names it.
    fn drop_other_addresses(
        &self,
        zone: &str,
        r: &IpamReservation,
        mac: &str,
    ) -> delonix_model::Result<EnsureOutcome> {
        let wanted = IpamReservation {
            mac: mac.to_string(),
            ..r.clone()
        };
        let observed = IpamObserved {
            entries: self.entries(zone)?,
            ..Default::default()
        };
        let others: Vec<IpamEntry> =
            delonix_networking::ipam::other_addresses_of(&wanted, &observed)
                .into_iter()
                .cloned()
                .collect();
        if others.is_empty() {
            return Ok(EnsureOutcome::AlreadyPresent);
        }
        for e in &others {
            self.client
                .sdn_vnet_ip_delete(&self.ledger, &r.vnet, zone, &e.ip, Some(mac))
                .map_err(delonix_model::Error::from)?;
            tracing::warn!(
                zone,
                mac,
                released = e.ip.as_str(),
                reserved = r.ip.as_str(),
                vmid = e.vmid,
                "the MAC also held another address, which its guest is served; released it — a \
                 guest already running keeps that lease until it restarts"
            );
        }
        Ok(EnsureOutcome::Created)
    }

    /// Every entry of the `pve` IPAM in `zone`.
    fn entries(&self, zone: &str) -> delonix_model::Result<Vec<IpamEntry>> {
        Ok(entries_from(
            &self
                .client
                .sdn_ipam_status(IPAM)
                .map_err(delonix_model::Error::from)?,
            zone,
        ))
    }

    /// ADR-0063 D2: an owned subnet whose gateway or DHCP ranges differ from
    /// the declaration is staged as declared, inside the caller's
    /// transaction. A new gateway an IPAM entry holds is refused BEFORE the
    /// write (D2.4) — the node moves the IPAM's gateway entry the moment the
    /// write is staged, and moving it onto a guest's address is never meant.
    fn update_in_place(
        &self,
        zone: &str,
        have: &IpamSubnet,
        want: &IpamSubnet,
        lines: &[String],
    ) -> delonix_model::Result<EnsureOutcome> {
        let entries = self.entries(zone)?;
        if let Some(g) = want
            .gateway
            .as_deref()
            .filter(|g| have.gateway.as_deref() != Some(*g))
        {
            if let Some(e) = gateway_held_by(&want.vnet, g, &entries) {
                return Err(delonix_networking::Error::RemoteObjectNotOwned(format!(
                    "subnet {} in vnet '{}': the new gateway {g} is already held {} — refusing \
                     to move the gateway onto it; free the address first (a reservation of this \
                     document is released AFTER the subnets are written, so drop it in one \
                     apply and move the gateway in the next)",
                    want.cidr,
                    want.vnet,
                    match (&e.mac, e.vmid) {
                        (Some(m), Some(id)) => format!("for MAC {m} (guest {id})"),
                        (Some(m), None) => format!("for MAC {m}"),
                        (None, _) => "without a MAC (a gateway entry, or an allocation)".into(),
                    }
                ))
                .into());
            }
        }
        // D2.5: a range narrowed under a guest's allocation leaves the
        // allocation where it is (not measured). Said, not silently assumed.
        let in_ranges = |ip: &str| {
            let n = ip.parse::<std::net::Ipv4Addr>().ok().map(u32::from);
            want.dhcp_ranges.iter().any(|r| {
                match (
                    n,
                    r.start.parse::<std::net::Ipv4Addr>().ok().map(u32::from),
                    r.end.parse::<std::net::Ipv4Addr>().ok().map(u32::from),
                ) {
                    (Some(n), Some(s), Some(e)) => (s..=e).contains(&n),
                    _ => false,
                }
            })
        };
        for e in entries
            .iter()
            .filter(|e| e.vnet == want.vnet && e.vmid.is_some() && want.holds(&e.ip))
            .filter(|e| !in_ranges(&e.ip))
        {
            tracing::warn!(
                zone,
                vnet = want.vnet.as_str(),
                address = e.ip.as_str(),
                vmid = e.vmid,
                "a guest's allocation is outside the subnet's new DHCP ranges; the node leaves it \
                 where it is"
            );
        }
        let ranges: Vec<DhcpRange> = want
            .dhcp_ranges
            .iter()
            .map(|r| DhcpRange {
                start: r.start.clone(),
                end: r.end.clone(),
            })
            .collect();
        self.client
            .set_sdn_subnet_addressing(
                &self.ledger,
                &want.vnet,
                zone,
                &want.cidr,
                want.gateway.as_deref(),
                &ranges,
            )
            .map_err(delonix_model::Error::from)?;
        tracing::info!(
            zone,
            vnet = want.vnet.as_str(),
            cidr = want.cidr.as_str(),
            change = lines.join("; ").as_str(),
            "subnet updated in place"
        );
        Ok(EnsureOutcome::Created)
    }

    /// The running subnets of `vnets` in `zone`, as the port's type.
    fn running_subnets(
        &self,
        zone: &str,
        vnets: &[String],
    ) -> delonix_model::Result<Vec<IpamSubnet>> {
        let running: Vec<String> = self
            .client
            .sdn_vnets_running()
            .map_err(delonix_model::Error::from)?
            .iter()
            .filter(|v| text(v, "zone") == zone)
            .map(|v| text(v, "vnet"))
            .collect();
        let mut subnets = Vec::new();
        for vnet in vnets.iter().filter(|v| running.contains(v)) {
            for r in self
                .client
                .sdn_vnet_subnets_running(vnet)
                .map_err(delonix_model::Error::from)?
            {
                subnets.push(subnet_from(&r, vnet));
            }
        }
        Ok(subnets)
    }
}

fn text(row: &serde_json::Value, k: &str) -> String {
    row.get(k)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// A subnet row of `GET /cluster/sdn/vnets/{vnet}/subnets` as the port's type.
pub(crate) fn subnet_from(row: &serde_json::Value, vnet: &str) -> IpamSubnet {
    IpamSubnet {
        vnet: vnet.to_string(),
        cidr: text(row, "cidr"),
        gateway: Some(text(row, "gateway")).filter(|g| !g.is_empty()),
        dhcp_ranges: dhcp_ranges_of(row)
            .into_iter()
            .map(|r| delonix_networking::ipam::DhcpRange {
                start: r.start,
                end: r.end,
            })
            .collect(),
    }
}

/// The rows of `GET /cluster/sdn/ipams/pve/status` that belong to `zone`.
/// The gateway's own entry has no MAC; a MAC is upper-cased, the form the
/// node stores and the port compares.
pub(crate) fn entries_from(rows: &[serde_json::Value], zone: &str) -> Vec<IpamEntry> {
    rows.iter()
        .filter(|r| text(r, "zone") == zone)
        .map(|r| IpamEntry {
            vnet: text(r, "vnet"),
            ip: text(r, "ip"),
            mac: Some(text(r, "mac"))
                .filter(|m| !m.is_empty())
                .map(|m| m.to_ascii_uppercase()),
            vmid: r
                .get("vmid")
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .and_then(|v| u32::try_from(v).ok()),
        })
        .collect()
}

impl delonix_compute::vm_provider::Provider for ProxmoxIpamProvider {
    fn id(&self) -> delonix_compute::vm_provider::ProviderId {
        delonix_compute::vm_provider::ProviderId(crate::network_zone::ID)
    }

    fn capabilities(&self) -> delonix_compute::capability::ProviderReport {
        crate::network_capability_report(true)
    }
}

impl IpamProvider for ProxmoxIpamProvider {
    fn available(&self) -> bool {
        true
    }

    fn controllers(&self) -> delonix_model::Result<Vec<IpamController>> {
        Ok(self
            .client
            .sdn_ipams()
            .map_err(delonix_model::Error::from)?
            .iter()
            .filter_map(controller_from)
            .collect())
    }

    fn refuse_unsupported(&self, controller: &IpamController) -> delonix_model::Result<()> {
        match unsupported_reason(&controller.kind) {
            None => Ok(()),
            Some(why) => Err(delonix_networking::Error::CapabilityUnmet(format!(
                "IPAM controller '{}' (plugin '{}'): {why}",
                controller.id, controller.kind
            ))
            .into()),
        }
    }

    /// Reads the zone (pending) and writes only what differs: `ipam=<id>`,
    /// and `dhcp=dnsmasq` set or deleted. The node refuses an `ipam` change
    /// on a zone that already holds a subnet; that refusal is surfaced as-is.
    fn prepare_zone(&self, zone: &str, controller: &str, dhcp: bool) -> delonix_model::Result<()> {
        let z = self
            .client
            .sdn_zone(zone)
            .map_err(delonix_model::Error::from)?;
        let has_ipam = z.get("ipam").and_then(|v| v.as_str()) == Some(controller);
        let has_dhcp = z.get("dhcp").and_then(|v| v.as_str()) == Some("dnsmasq");
        if dhcp {
            self.warn_if_the_node_firewall_drops_dhcp(zone);
        }
        if has_ipam && has_dhcp == dhcp {
            return Ok(());
        }
        self.client
            .set_sdn_zone_addressing(&self.ledger, zone, controller, dhcp)
            .map_err(delonix_model::Error::from)
    }

    fn ensure_subnet(
        &self,
        zone: &str,
        subnet: &IpamSubnet,
        owner: &OwnerMark,
    ) -> delonix_model::Result<EnsureOutcome> {
        subnet.validate()?;
        if !self.owned_vnet(&subnet.vnet, owner)? {
            return Err(delonix_model::Error::Invalid(format!(
                "subnet {}: vnet '{}' does not exist",
                subnet.cidr, subnet.vnet
            )));
        }
        let rows = self
            .client
            .sdn_vnet_subnets(&subnet.vnet)
            .map_err(delonix_model::Error::from)?;
        if let Some(row) = rows.iter().find(|r| text(r, "cidr") == subnet.cidr) {
            let have = subnet_from(row, &subnet.vnet);
            return match subnet_change(&have, subnet) {
                SubnetChange::Same => Ok(EnsureOutcome::AlreadyPresent),
                // Found by its CIDR in its own vnet, so the identity matches;
                // kept for the rule's sake rather than assumed.
                SubnetChange::Replace(lines) => {
                    Err(delonix_networking::Error::RemoteObjectDrifted(format!(
                        "{} — replace the document so the engine recreates it",
                        lines.join("; ")
                    ))
                    .into())
                }
                SubnetChange::InPlace(lines) => self.update_in_place(zone, &have, subnet, &lines),
            };
        }
        let ranges: Vec<DhcpRange> = subnet
            .dhcp_ranges
            .iter()
            .map(|r| DhcpRange {
                start: r.start.clone(),
                end: r.end.clone(),
            })
            .collect();
        self.client
            .create_sdn_subnet_with(
                &self.ledger,
                &subnet.vnet,
                zone,
                &subnet.cidr,
                &SubnetOptions {
                    gateway: subnet.gateway.as_deref(),
                    dhcp_ranges: &ranges,
                    ..Default::default()
                },
            )
            .map_err(delonix_model::Error::from)?;
        Ok(EnsureOutcome::Created)
    }

    fn remove_subnet(
        &self,
        zone: &str,
        subnet: &IpamSubnet,
        owner: &OwnerMark,
    ) -> delonix_model::Result<()> {
        if !self.owned_vnet(&subnet.vnet, owner)? {
            return Ok(());
        }
        let present = self
            .client
            .sdn_vnet_subnets(&subnet.vnet)
            .map_err(delonix_model::Error::from)?
            .iter()
            .any(|r| text(r, "cidr") == subnet.cidr);
        if !present {
            return Ok(());
        }
        self.client
            .delete_sdn_subnet(&self.ledger, &subnet.vnet, zone, &subnet.cidr)
            .map_err(delonix_model::Error::from)
    }

    /// Made, then read back: a zone that allocates from no IPAM answers the
    /// write with success and keeps nothing.
    fn ensure_reservation(
        &self,
        zone: &str,
        r: &IpamReservation,
    ) -> delonix_model::Result<EnsureOutcome> {
        let mac = normalize_mac(&r.mac).ok_or_else(|| {
            delonix_model::Error::Invalid(format!("reservation {}: '{}' is not a MAC", r.ip, r.mac))
        })?;
        if let Some(e) = self.entries(zone)?.into_iter().find(|e| e.ip == r.ip) {
            if e.mac.as_deref() == Some(mac.as_str()) {
                return self.drop_other_addresses(zone, r, &mac);
            }
            return Err(delonix_networking::Error::RemoteObjectNotOwned(format!(
                "address {} in zone '{zone}' is already held {} — refusing to take it over; pick \
                 another address, or release that one on the cluster if it is really stale",
                r.ip,
                e.mac
                    .map(|m| format!("for MAC {m}"))
                    .unwrap_or_else(|| "without a MAC (the gateway, or an allocation)".into()),
            ))
            .into());
        }
        // Measured: a guest created on a vnet whose zone serves DHCP already
        // holds an address of the range for its MAC (the node allocates at
        // create). Adding a second address for the same MAC is accepted, and
        // which of the two the guest is then served depends on the order the
        // node reads them back — so an address the MAC already holds in this
        // vnet is MOVED to the reserved one (`PUT …/ips`, which moves a MAC),
        // and the MAC ends with exactly one.
        let held_by_mac = self
            .entries(zone)?
            .into_iter()
            .any(|e| e.vnet == r.vnet && e.mac.as_deref() == Some(mac.as_str()));
        if held_by_mac {
            self.client
                .sdn_vnet_ip_update(&self.ledger, &r.vnet, zone, &mac, &r.ip, None)
                .map_err(delonix_model::Error::from)?;
        } else {
            self.client
                .sdn_vnet_ip_add(&self.ledger, &r.vnet, zone, &r.ip, Some(&mac))
                .map_err(delonix_model::Error::from)?;
        }
        let of_mac: Vec<String> = self
            .entries(zone)?
            .into_iter()
            .filter(|e| e.vnet == r.vnet && e.mac.as_deref() == Some(mac.as_str()))
            .map(|e| e.ip)
            .collect();
        if of_mac != [r.ip.clone()] {
            return Err(delonix_model::Error::Invalid(format!(
                "reservation {} for {mac}: the node answered success and holds {:?} for that MAC \
                 — the zone '{zone}' allocates from no IPAM, or the move did not happen",
                r.ip, of_mac
            )));
        }
        Ok(EnsureOutcome::Created)
    }

    fn remove_reservation(
        &self,
        zone: &str,
        r: &IpamReservation,
    ) -> delonix_model::Result<RemoveOutcome> {
        // (`drop_other_addresses` is the reservation's counterpart: it makes
        // the reserved address the MAC's only one in the vnet.)
        let Some(mac) = normalize_mac(&r.mac) else {
            return Ok(RemoveOutcome::Absent);
        };
        let Some(e) = self
            .entries(zone)?
            .into_iter()
            .find(|e| e.vnet == r.vnet && e.ip == r.ip)
        else {
            return Ok(RemoveOutcome::Absent);
        };
        if e.mac.as_deref() != Some(mac.as_str()) {
            return Ok(RemoveOutcome::NotOwned(Owner::Unmarked));
        }
        self.client
            .sdn_vnet_ip_delete(&self.ledger, &r.vnet, zone, &r.ip, Some(&mac))
            .map_err(delonix_model::Error::from)?;
        Ok(RemoveOutcome::Removed)
    }

    fn observe(&self, zone: &str, vnets: &[String]) -> delonix_model::Result<IpamObserved> {
        let zones = self
            .client
            .sdn_zones_running()
            .map_err(delonix_model::Error::from)?;
        let row = zones.iter().find(|z| text(z, "zone") == zone);
        let subnets = self.running_subnets(zone, vnets)?;
        let entries = self
            .entries(zone)?
            .into_iter()
            .filter(|e| vnets.contains(&e.vnet))
            .collect();
        Ok(IpamObserved {
            subnets,
            entries,
            zone_ipam: row.is_some_and(|z| !text(z, "ipam").is_empty()),
            zone_dhcp: row.is_some_and(|z| text(z, "dhcp") == "dnsmasq"),
            controller: row.map(|z| text(z, "ipam")).filter(|c| !c.is_empty()),
        })
    }

    /// ADR-0063 D2.3, the sequences measured on PVE 9.2.2 (see
    /// [`gateway_repairs`]): with the SDN lock held, stage the gateway on a
    /// free address and back on the running one (two writes) where no entry
    /// is on the running gateway; once that entry is confirmed, release
    /// every other gateway entry of the subnet (`DELETE …/ips`, which acts on
    /// the IPAM, not on the staged configuration); then roll back — nothing
    /// staged survives. Read back after: a subnet that does not end with
    /// exactly its running gateway's entry (or none, without a gateway) is an
    /// error, never a success. Nothing is written when every entry is right.
    fn repair_gateways(
        &self,
        zone: &str,
        vnets: &[String],
        owner: &OwnerMark,
    ) -> delonix_model::Result<Vec<String>> {
        // Only this engine's vnets: a subnet is this engine's only inside a
        // vnet carrying the caller's mark.
        let rows = self
            .client
            .sdn_vnets()
            .map_err(delonix_model::Error::from)?;
        let owned: Vec<String> = vnets
            .iter()
            .filter(|v| {
                rows.iter().any(|r| {
                    text(r, "vnet") == **v
                        && owner.owner_of(r.get("alias").and_then(|a| a.as_str()).unwrap_or(""))
                            == Owner::Ours
                })
            })
            .cloned()
            .collect();
        let running = self.running_subnets(zone, &owned)?;
        let rows = self
            .client
            .sdn_ipam_status(IPAM)
            .map_err(delonix_model::Error::from)?;
        let repairs = gateway_repairs(
            &running,
            &gateway_entries_from(&rows, zone),
            &entries_from(&rows, zone),
        )
        .map_err(delonix_model::Error::Invalid)?;
        if repairs.is_empty() {
            return Ok(Vec::new());
        }
        let gateways = || -> crate::Result<Vec<(String, String)>> {
            Ok(gateway_entries_from(
                &self.client.sdn_ipam_status(IPAM)?,
                zone,
            ))
        };
        self.client
            .sdn_discarded_change(&self.ledger, || {
                for r in &repairs {
                    let (Some(through), Some(gateway)) = (&r.through, &r.gateway) else {
                        continue;
                    };
                    for g in [through.as_str(), gateway.as_str()] {
                        self.client.update_sdn_subnet_with(
                            &self.ledger,
                            &r.vnet,
                            zone,
                            &r.cidr,
                            &SubnetOptions {
                                gateway: Some(g),
                                ..Default::default()
                            },
                        )?;
                    }
                }
                // Measured on PVE 9.2.2 (2026-10-09): the move-and-back puts
                // a gateway entry on the running gateway and LEAVES the one
                // on the staged address, still flagged gateway — which holds
                // that address and makes the node refuse the subnet's delete
                // ("not empty"). A subnet has one gateway, so every other
                // gateway entry is released — but only once the right one is
                // confirmed: without it the read-back below names the subnet.
                let held = gateways()?;
                for r in &repairs {
                    let mine = |vnet: &str, ip: &str| vnet == r.vnet && r_holds(r, ip);
                    let right = match &r.gateway {
                        Some(g) => held.iter().any(|(vnet, ip)| mine(vnet, ip) && ip == g),
                        None => true,
                    };
                    if !right {
                        continue;
                    }
                    for (_, ip) in held
                        .iter()
                        .filter(|(vnet, ip)| mine(vnet, ip) && Some(ip) != r.gateway.as_ref())
                    {
                        self.client
                            .sdn_vnet_ip_delete(&self.ledger, &r.vnet, zone, ip, None)?;
                    }
                }
                Ok(())
            })
            .map_err(delonix_model::Error::from)?;
        let after = gateways().map_err(delonix_model::Error::from)?;
        let mut out = Vec::new();
        for r in &repairs {
            let entries: Vec<&str> = after
                .iter()
                .filter(|(vnet, ip)| *vnet == r.vnet && r_holds(r, ip))
                .map(|(_, ip)| ip.as_str())
                .collect();
            let want: Vec<&str> = r.gateway.as_deref().into_iter().collect();
            if entries != want {
                return Err(delonix_model::Error::Invalid(match &r.gateway {
                    Some(g) => format!(
                        "subnet {} in vnet '{}': after the repair the IPAM holds gateway entries \
                         on {entries:?}, expected only {g} — release any other on the cluster \
                         and put the gateway entry back by hand before a guest is given {g}",
                        r.cidr, r.vnet
                    ),
                    None => format!(
                        "subnet {} in vnet '{}' runs without a gateway, and after the repair the \
                         IPAM still holds gateway entries on {entries:?} — release them on the \
                         cluster",
                        r.cidr, r.vnet
                    ),
                }));
            }
            out.push(repair_line(r));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows as PVE 9.2.2 answers them (measured, `GET
    /// /cluster/sdn/ipams/pve/status` and `…/subnets?running=1`).
    #[test]
    fn the_node_rows_read_as_the_ports_types() {
        let status = [
            serde_json::json!({"ip":"10.78.0.1","vnet":"f5bv","zone":"f5bz","gateway":1,"subnet":"10.78.0.0/24"}),
            serde_json::json!({"ip":"10.78.0.20","mac":"bc:24:11:00:00:20","vnet":"f5bv","zone":"f5bz","subnet":"10.78.0.0/24"}),
            serde_json::json!({"ip":"10.9.0.5","mac":"BC:24:11:00:00:99","vnet":"other","zone":"zz"}),
        ];
        let e = entries_from(&status, "f5bz");
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].mac, None);
        assert_eq!(e[1].mac.as_deref(), Some("BC:24:11:00:00:20"));

        let row = serde_json::json!({
            "id":"f5bz-10.78.0.0-24","mask":"24","gateway":"10.78.0.1","cidr":"10.78.0.0/24",
            "dhcp-range":[{"end-address":"10.78.0.150","start-address":"10.78.0.100"}],
            "vnet":"f5bv","zone":"f5bz","type":"subnet"
        });
        let s = subnet_from(&row, "f5bv");
        assert_eq!(s.cidr, "10.78.0.0/24");
        assert_eq!(s.gateway.as_deref(), Some("10.78.0.1"));
        assert_eq!(s.dhcp_ranges.len(), 1);
        assert_eq!(subnet_change(&s, &s), SubnetChange::Same);
        let mut other = s.clone();
        other.gateway = None;
        other.dhcp_ranges.clear();
        assert!(matches!(subnet_change(&s, &other), SubnetChange::InPlace(l) if l.len() == 2));
        // The gateway row is the one the repair reads.
        assert_eq!(
            gateway_entries_from(&status, "f5bz"),
            vec![("f5bv".to_string(), "10.78.0.1".to_string())]
        );
    }

    /// ADR-0063 D1.2/D1.3: only the built-in controller is served; phpIPAM
    /// is refused by name with the node's own reason, NetBox until its spike.
    #[test]
    fn only_the_built_in_controller_is_served_and_each_refusal_says_why() {
        assert_eq!(unsupported_reason("pve"), None);
        let php = unsupported_reason("phpipam").unwrap();
        assert!(
            php.contains("parsing of result not yet implemented"),
            "{php}"
        );
        assert!(php.contains("unsupported-by-provider"), "{php}");
        let netbox = unsupported_reason("netbox").unwrap();
        assert!(netbox.contains("D1.3"), "{netbox}");
        assert!(unsupported_reason("infoblox")
            .unwrap()
            .contains("never measured"));

        // `GET /cluster/sdn/ipams` rows: the id and the type, never the token.
        let rows = [
            serde_json::json!({"ipam":"pve","type":"pve","digest":"x"}),
            serde_json::json!({"ipam":"php1","type":"phpipam","url":"https://p","token":"secret"}),
            serde_json::json!({"type":"netbox"}),
        ];
        let c: Vec<IpamController> = rows.iter().filter_map(controller_from).collect();
        assert_eq!(
            c,
            vec![
                IpamController {
                    id: "pve".into(),
                    kind: "pve".into()
                },
                IpamController {
                    id: "php1".into(),
                    kind: "phpipam".into()
                },
            ]
        );
    }

    fn sub(gateway: &str) -> IpamSubnet {
        IpamSubnet {
            vnet: "v".into(),
            cidr: "10.82.0.0/24".into(),
            gateway: Some(gateway.into()),
            dhcp_ranges: vec![delonix_networking::ipam::DhcpRange {
                start: "10.82.0.100".into(),
                end: "10.82.0.150".into(),
            }],
        }
    }

    fn entry(ip: &str, mac: Option<&str>) -> IpamEntry {
        IpamEntry {
            vnet: "v".into(),
            ip: ip.into(),
            mac: mac.map(str::to_string),
            vmid: None,
        }
    }

    /// The state ADR-0063 measured after a rolled-back gateway change: the
    /// subnet runs with `.1`, the IPAM holds the gateway entry on `.254`.
    #[test]
    fn a_gateway_entry_left_elsewhere_is_repaired_through_a_free_address() {
        let running = [sub("10.82.0.1")];
        let entries = [
            entry("10.82.0.254", None),
            entry("10.82.0.20", Some("BC:24:11:00:00:20")),
        ];
        let stale = [("v".to_string(), "10.82.0.254".to_string())];
        let r = gateway_repairs(&running, &stale, &entries).unwrap();
        assert_eq!(
            r,
            vec![GatewayRepair {
                vnet: "v".into(),
                cidr: "10.82.0.0/24".into(),
                gateway: Some("10.82.0.1".into()),
                stale: vec!["10.82.0.254".into()],
                // .254 is held (the stale entry), .255 is the broadcast.
                through: Some("10.82.0.253".into()),
            }]
        );
        assert!(repair_line(&r[0]).contains("was on 10.82.0.254, put back on 10.82.0.1"));

        // In sync: nothing.
        let right = [("v".to_string(), "10.82.0.1".to_string())];
        assert!(gateway_repairs(&running, &right, &entries)
            .unwrap()
            .is_empty());
        // A gateway entry of ANOTHER vnet does not count — and this subnet
        // has none of its own, which is the next case.
        let elsewhere = [("w".to_string(), "10.82.0.254".to_string())];
        assert_eq!(
            gateway_repairs(&running, &elsewhere, &entries).unwrap(),
            gateway_repairs(&running, &[], &entries).unwrap()
        );
    }

    /// A `delete=gateway` staged and rolled back (measured on PVE 9.2.2,
    /// 2026-10-09): the subnet runs with its gateway and the IPAM holds no
    /// gateway entry — the router's address is free for a guest. Moved
    /// through a free address and back, nothing to release.
    #[test]
    fn a_running_gateway_with_no_entry_is_put_back() {
        let running = [sub("10.82.0.1")];
        let entries = [entry("10.82.0.20", Some("BC:24:11:00:00:20"))];
        let r = gateway_repairs(&running, &[], &entries).unwrap();
        assert_eq!(
            r,
            vec![GatewayRepair {
                vnet: "v".into(),
                cidr: "10.82.0.0/24".into(),
                gateway: Some("10.82.0.1".into()),
                stale: Vec::new(),
                through: Some("10.82.0.254".into()),
            }]
        );
        assert!(repair_line(&r[0]).contains("held no gateway entry, put back on 10.82.0.1"));
    }

    /// A gateway added to a subnet without one, staged and rolled back
    /// (measured on PVE 9.2.2): the subnet runs without a gateway and the
    /// IPAM holds the new one's entry. Released, nothing moved — and it would
    /// otherwise make D2.4 refuse that gateway on the next apply.
    #[test]
    fn a_gateway_entry_of_a_subnet_without_a_gateway_is_released() {
        let mut no_gw = sub("10.82.0.1");
        no_gw.gateway = None;
        let held = [("v".to_string(), "10.82.0.1".to_string())];
        let r = gateway_repairs(&[no_gw.clone()], &held, &[entry("10.82.0.1", None)]).unwrap();
        assert_eq!(
            r,
            vec![GatewayRepair {
                vnet: "v".into(),
                cidr: "10.82.0.0/24".into(),
                gateway: None,
                stale: vec!["10.82.0.1".into()],
                through: None,
            }]
        );
        assert!(repair_line(&r[0]).contains("runs without a gateway; released"));
        // No gateway and no entry: nothing.
        assert!(gateway_repairs(&[no_gw], &[], &[]).unwrap().is_empty());
    }

    /// A repair cut short after its move and before its release: the right
    /// entry is there, the stale one is still flagged — only released.
    #[test]
    fn an_extra_gateway_entry_next_to_the_right_one_is_released() {
        let running = [sub("10.82.0.1")];
        let held = [
            ("v".to_string(), "10.82.0.200".to_string()),
            ("v".to_string(), "10.82.0.1".to_string()),
        ];
        let r = gateway_repairs(&running, &held, &[]).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].stale, vec!["10.82.0.200".to_string()]);
        assert_eq!(r[0].through, None);
        assert!(repair_line(&r[0]).contains("the entry on 10.82.0.1 stays"));
    }

    #[test]
    fn the_address_moved_through_is_free_and_outside_every_range() {
        let mut s = sub("10.82.0.254");
        s.cidr = "10.82.0.0/29".into();
        s.dhcp_ranges = vec![delonix_networking::ipam::DhcpRange {
            start: "10.82.0.4".into(),
            end: "10.82.0.5".into(),
        }];
        // .7 broadcast, .6 held, .5 and .4 in the range, .3 free.
        assert_eq!(
            free_address(&s, &[entry("10.82.0.6", Some("BC:24:11:00:00:06"))]).as_deref(),
            Some("10.82.0.3")
        );
        let full: Vec<IpamEntry> = (1..=3)
            .chain(6..=6)
            .map(|i| entry(&format!("10.82.0.{i}"), None))
            .collect();
        assert_eq!(free_address(&s, &full), None);
        let r = gateway_repairs(
            &[IpamSubnet {
                gateway: Some("10.82.0.1".into()),
                ..s.clone()
            }],
            &[("v".into(), "10.82.0.2".into())],
            &full,
        );
        assert!(r.unwrap_err().contains("no free address"));
        s.cidr = "10.82.0.0/31".into();
        assert_eq!(free_address(&s, &[]), None);
    }
}
