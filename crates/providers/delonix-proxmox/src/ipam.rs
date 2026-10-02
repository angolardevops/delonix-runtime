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
    normalize_mac, IpamEntry, IpamObserved, IpamProvider, IpamReservation, IpamSubnet,
};
use delonix_networking::ownership::{Owner, OwnerMark, RemoveOutcome};
use delonix_networking::segment::EnsureOutcome;

/// The IPAM every zone this provider prepares allocates from: Proxmox's
/// built-in one, the only plugin that needs no external server.
pub const IPAM: &str = "pve";

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
        })
        .collect()
}

/// How an owned subnet on the node differs from the declaration.
pub(crate) fn subnet_drift(have: &IpamSubnet, want: &IpamSubnet) -> Vec<String> {
    let mut out = Vec::new();
    if have.gateway != want.gateway {
        out.push(format!(
            "its gateway is '{}', declared '{}'",
            have.gateway.as_deref().unwrap_or(""),
            want.gateway.as_deref().unwrap_or("")
        ));
    }
    let ranges = |r: &[delonix_networking::ipam::DhcpRange]| {
        let mut v: Vec<String> = r.iter().map(|r| format!("{}-{}", r.start, r.end)).collect();
        v.sort();
        v.join(",")
    };
    if ranges(&have.dhcp_ranges) != ranges(&want.dhcp_ranges) {
        out.push(format!(
            "its dhcp ranges are '{}', declared '{}'",
            ranges(&have.dhcp_ranges),
            ranges(&want.dhcp_ranges)
        ));
    }
    out
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

    /// Reads the zone (pending) and writes only what differs: `ipam=pve`, and
    /// `dhcp=dnsmasq` set or deleted. The node refuses an `ipam` change on a
    /// zone that already holds a subnet; that refusal is surfaced as-is.
    fn prepare_zone(&self, zone: &str, dhcp: bool) -> delonix_model::Result<()> {
        let z = self
            .client
            .sdn_zone(zone)
            .map_err(delonix_model::Error::from)?;
        let has_ipam = z.get("ipam").and_then(|v| v.as_str()) == Some(IPAM);
        let has_dhcp = z.get("dhcp").and_then(|v| v.as_str()) == Some("dnsmasq");
        if dhcp {
            self.warn_if_the_node_firewall_drops_dhcp(zone);
        }
        if has_ipam && has_dhcp == dhcp {
            return Ok(());
        }
        self.client
            .set_sdn_zone_addressing(&self.ledger, zone, IPAM, dhcp)
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
            let drift = subnet_drift(&subnet_from(row, &subnet.vnet), subnet);
            if !drift.is_empty() {
                return Err(delonix_networking::Error::RemoteObjectDrifted(format!(
                    "subnet {} in vnet '{}' (this engine's) was changed on the cluster: {} — put \
                     it back, or replace the document so the engine recreates it",
                    subnet.cidr,
                    subnet.vnet,
                    drift.join("; ")
                ))
                .into());
            }
            return Ok(EnsureOutcome::AlreadyPresent);
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
                return Ok(EnsureOutcome::AlreadyPresent);
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
        })
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
        assert!(subnet_drift(&s, &s).is_empty());
        let mut other = s.clone();
        other.gateway = None;
        other.dhcp_ranges.clear();
        assert_eq!(subnet_drift(&s, &other).len(), 2);
    }
}
