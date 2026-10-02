//! `DnsProvider` for Proxmox VE's SDN DNS (ADR-0059 F5c): the zone's DNS
//! controller, domain and reverse controller. The node writes the records.
//!
//! Every fact this module leans on was measured on PVE 9.2.2 (ADR-0064):
//!
//! * the zone fields are `dns`, `dnszone` and `reversedns`, staged like any
//!   SDN change; a `dnszone` without `dns` in the same request is refused;
//! * the node writes the records itself — for a guest that gets an address
//!   from a DHCP range, and for a subnet's gateway — and verifies the domain
//!   (and the reverse zone it derives) on the DNS server when a subnet is
//!   created, failing with the server's 404 when one is missing;
//! * `GET /cluster/sdn/dns` returns each controller's API key in clear. This
//!   module reads the id and the type, and drops the rest of the row on the
//!   spot: the engine never holds, logs or reuses that credential.
//!
//! It shares the [`Client`] of the segment provider, like the IPAM provider:
//! the staged write runs inside that provider's transaction.

use crate::{Client, Ledger};
use delonix_networking::dns::{DnsController, DnsProvider, ZoneDns};

/// The [`DnsProvider`] for the cluster's SDN.
pub struct ProxmoxDnsProvider {
    client: std::sync::Arc<Client>,
    ledger: Ledger,
}

impl ProxmoxDnsProvider {
    pub fn new(client: std::sync::Arc<Client>, ledger: Ledger) -> Self {
        Self { client, ledger }
    }
}

fn text(row: &serde_json::Value, k: &str) -> String {
    row.get(k)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// A zone row's DNS settings; `None` when it names no controller.
pub(crate) fn zone_dns_from(row: &serde_json::Value) -> Option<ZoneDns> {
    let server = text(row, "dns");
    if server.is_empty() {
        return None;
    }
    let reverse = text(row, "reversedns");
    Some(ZoneDns {
        server,
        zone: text(row, "dnszone").to_ascii_lowercase(),
        reverse_server: (!reverse.is_empty()).then_some(reverse),
    })
}

/// The controllers in a `GET /cluster/sdn/dns` answer: id and type only.
pub(crate) fn controllers_from(rows: &[serde_json::Value]) -> Vec<DnsController> {
    rows.iter()
        .map(|r| DnsController {
            id: text(r, "dns"),
            kind: text(r, "type"),
        })
        .filter(|c| !c.id.is_empty())
        .collect()
}

impl delonix_compute::vm_provider::Provider for ProxmoxDnsProvider {
    fn id(&self) -> delonix_compute::vm_provider::ProviderId {
        delonix_compute::vm_provider::ProviderId(crate::network_zone::ID)
    }

    fn capabilities(&self) -> delonix_compute::capability::ProviderReport {
        crate::network_capability_report(true)
    }
}

impl DnsProvider for ProxmoxDnsProvider {
    fn available(&self) -> bool {
        true
    }

    fn controllers(&self) -> delonix_model::Result<Vec<DnsController>> {
        let rows = self
            .client
            .sdn_dns_controllers()
            .map_err(delonix_model::Error::from)?;
        Ok(controllers_from(&rows))
    }

    /// Reads the zone (pending) and writes only when its settings differ.
    fn prepare_zone(&self, zone: &str, dns: Option<&ZoneDns>) -> delonix_model::Result<()> {
        let row = self
            .client
            .sdn_zone(zone)
            .map_err(delonix_model::Error::from)?;
        if zone_dns_from(&row).as_ref() == dns {
            return Ok(());
        }
        self.client
            .set_sdn_zone_dns(
                &self.ledger,
                zone,
                dns.map(|d| {
                    (
                        d.server.as_str(),
                        d.zone.as_str(),
                        d.reverse_server.as_deref(),
                    )
                }),
            )
            .map_err(delonix_model::Error::from)
    }

    fn observe(&self, zone: &str) -> delonix_model::Result<Option<ZoneDns>> {
        let zones = self
            .client
            .sdn_zones_running()
            .map_err(delonix_model::Error::from)?;
        Ok(zones
            .iter()
            .find(|z| text(z, "zone") == zone)
            .and_then(zone_dns_from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zone row and a controller row as PVE 9.2.2 answers them (measured,
    /// `GET /cluster/sdn/zones?running=1` and `GET /cluster/sdn/dns`).
    #[test]
    fn the_node_rows_read_as_the_ports_types() {
        let zone: serde_json::Value = serde_json::json!({
            "zone": "f5cz", "type": "simple", "ipam": "pve", "dhcp": "dnsmasq",
            "dns": "pdnslab", "reversedns": "pdnslab", "dnszone": "f5c.lab", "digest": "x"
        });
        assert_eq!(
            zone_dns_from(&zone),
            Some(ZoneDns {
                server: "pdnslab".into(),
                zone: "f5c.lab".into(),
                reverse_server: Some("pdnslab".into()),
            })
        );
        let bare: serde_json::Value = serde_json::json!({"zone": "z", "type": "simple"});
        assert_eq!(zone_dns_from(&bare), None);
        let controller: serde_json::Value = serde_json::json!({
            "dns": "pdnslab", "type": "powerdns", "key": "SECRET-VALUE",
            "url": "http://pdns:8081/api/v1/servers/localhost", "ttl": 300, "digest": "y"
        });
        let got = controllers_from(&[controller]);
        assert_eq!(
            got,
            vec![DnsController {
                id: "pdnslab".into(),
                kind: "powerdns".into()
            }]
        );
        assert!(!format!("{got:?}").contains("SECRET"));
    }

    /// An answer that cannot be read must not carry the controller's key into
    /// the error: `parse` quotes the body, this route's parser does not.
    #[test]
    fn a_controller_answer_that_cannot_be_read_does_not_leak_its_key() {
        for body in [
            r#"{"data":[{"dns":"pdnslab","type":"powerdns","key":"SECRET-VALUE","url":"x"}"#,
            r#"{"data":[{"dns":"pdnslab","type":"powerdns","key":"SECRET-VALUE","ttl":"#,
            r#"{"data":"SECRET-VALUE"}"#,
        ] {
            let e = crate::parse_secret_bearing::<crate::Wrapped<Vec<serde_json::Value>>>(
                body,
                "/cluster/sdn/dns",
            )
            .unwrap_err()
            .to_string();
            assert!(!e.contains("SECRET"), "{e}");
            assert!(e.contains("/cluster/sdn/dns"), "{e}");
        }
    }
}
