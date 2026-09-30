//! `SegmentProvider` implementation for Proxmox VE's own SDN (Zones,
//! VNets) — ADR-0049 addendum, closing the gap D3 names.
//!
//! Thin, on purpose: every real decision (staged-vs-applied, the id format,
//! the referential order between a zone and its vnets) already lives in
//! `sdn.rs`, built and live-tested by ADR-0049 slice 2 — this module only
//! adapts that surface to the trait `delonix-sdn::network_zone` defines.

use crate::{Client, Error, Ledger};
use delonix_networking::ownership::{split_mark, Owner, OwnerMark, RemoveOutcome};
use delonix_networking::segment::{EnsureOutcome, NetworkZoneSpec, SegmentProvider, VNetSpec};

/// The canonical id this provider registers under, and the only one
/// [`crate::register_segment_provider`] uses (`"pve"` as an alias, the
/// same pair [`crate::registration`] registers the `VmBackend` under).
pub const ID: &str = "proxmox";

/// The longest vnet `alias` the node accepts (`pvesh usage
/// /cluster/sdn/vnets`, PVE 9.2.2: `[\(\)-_.\w\d\s]{0,256}`) — the declared
/// alias plus the owner mark must fit.
const MAX_VNET_ALIAS: usize = 256;

/// The [`SegmentProvider`] this crate exists to provide: Proxmox's
/// cluster SDN, wrapped.
pub struct ProxmoxSegmentProvider {
    client: std::sync::Arc<Client>,
    ledger: Ledger,
}

impl ProxmoxSegmentProvider {
    pub fn new(client: std::sync::Arc<Client>, ledger: Ledger) -> Self {
        Self { client, ledger }
    }
}

/// The alias written to the node: the declared one, then the mark.
pub(crate) fn stamped_alias(
    declared: Option<&str>,
    owner: &OwnerMark,
) -> delonix_model::Result<String> {
    let alias = owner.stamp(declared.unwrap_or(""));
    if alias.len() > MAX_VNET_ALIAS {
        return Err(delonix_model::Error::Invalid(format!(
            "vnet alias too long: with the owner mark it is {} characters, and Proxmox accepts \
             {MAX_VNET_ALIAS} — shorten the declared alias to {} or fewer",
            alias.len(),
            MAX_VNET_ALIAS - owner.tag().len() - 1
        )));
    }
    Ok(alias)
}

/// How an owned vnet (a row of `GET /cluster/sdn/vnets`) differs from the
/// declaration.
pub(crate) fn vnet_drift(row: &serde_json::Value, want: &VNetSpec) -> Vec<String> {
    let mut out = Vec::new();
    let zone = row.get("zone").and_then(|v| v.as_str()).unwrap_or("");
    if zone != want.zone {
        out.push(format!("it is in zone '{zone}', declared '{}'", want.zone));
    }
    let alias = split_mark(row.get("alias").and_then(|v| v.as_str()).unwrap_or("")).0;
    let declared = want.alias.as_deref().unwrap_or("").trim();
    if alias != declared {
        out.push(format!("its alias is '{alias}', declared '{declared}'"));
    }
    out
}

fn sdn_err(e: delonix_networking::Error) -> delonix_model::Error {
    delonix_model::Error::from(e)
}

// Failures cross the trait with their dictionary number
// (`delonix_model::Error::from`), never `into_root`, which strips it: measured
// live, a DX-5340 refusal from inside the SDN transaction arrived as 5000.
/// The skeleton every role port extends (ADR-0059 D1 rule 4). The report is
/// the declared network one: this value exists only once the node was
/// configured, and answering never contacts it.
impl delonix_compute::vm_provider::Provider for ProxmoxSegmentProvider {
    fn id(&self) -> delonix_compute::vm_provider::ProviderId {
        delonix_compute::vm_provider::ProviderId(ID)
    }

    fn capabilities(&self) -> delonix_compute::capability::ProviderReport {
        crate::network_capability_report(true)
    }
}

impl SegmentProvider for ProxmoxSegmentProvider {
    fn available(&self) -> bool {
        // Registered only once the client already exists (ADR-0008's own
        // reasoning for a remote VmBackend) — by the time this value
        // exists, the target is at least reachable in principle.
        true
    }

    /// Checks presence first, and only creates when absent — reading the
    /// PENDING configuration, the one a write inside the transaction sees.
    /// A zone carries no mark (no free-text field): whether an
    /// `AlreadyPresent` zone is the caller's is decided by the caller's
    /// record, never here.
    fn ensure_zone(&self, zone: &NetworkZoneSpec) -> delonix_model::Result<EnsureOutcome> {
        let exists = self
            .client
            .sdn_zones()
            .map_err(delonix_model::Error::from)?
            .iter()
            .any(|z| z.get("zone").and_then(|v| v.as_str()) == Some(zone.name.as_str()));
        if exists {
            return Ok(EnsureOutcome::AlreadyPresent);
        }
        self.client
            .create_sdn_zone(&self.ledger, &zone.name)
            .map_err(delonix_model::Error::from)?;
        Ok(EnsureOutcome::Created)
    }

    /// The node itself refuses this while a vnet still references the zone
    /// (`Client::delete_sdn_zone`'s own doc comment) — surfaced as-is, never
    /// pre-empted here: the caller (`cmd::network_zone::remove_for_replace`)
    /// is what orders vnets before their zone.
    fn remove_zone(&self, name: &str) -> delonix_model::Result<()> {
        self.client
            .delete_sdn_zone(&self.ledger, name)
            .map_err(delonix_model::Error::from)
    }

    fn ensure_vnet(
        &self,
        vnet: &VNetSpec,
        owner: &OwnerMark,
    ) -> delonix_model::Result<EnsureOutcome> {
        let alias = stamped_alias(vnet.alias.as_deref(), owner)?;
        let vnets = self
            .client
            .sdn_vnets()
            .map_err(delonix_model::Error::from)?;
        if let Some(row) = vnets
            .iter()
            .find(|v| v.get("vnet").and_then(|s| s.as_str()) == Some(vnet.name.as_str()))
        {
            let found = owner.owner_of(row.get("alias").and_then(|v| v.as_str()).unwrap_or(""));
            if found != Owner::Ours {
                return Err(sdn_err(delonix_networking::Error::RemoteObjectNotOwned(
                    format!(
                        "vnet '{}' already exists in the cluster's SDN and is {} — refusing to \
                     adopt it by name; pick another vnet name, or remove the one on the \
                     cluster if it is really stale",
                        vnet.name,
                        found.describe()
                    ),
                )));
            }
            let drift = vnet_drift(row, vnet);
            if !drift.is_empty() {
                return Err(sdn_err(delonix_networking::Error::RemoteObjectDrifted(
                    format!(
                    "vnet '{}' (this engine's) was changed on the cluster: {} — put it back, or \
                     replace the document so the engine recreates it",
                    vnet.name,
                    drift.join("; ")
                ),
                )));
            }
            return Ok(EnsureOutcome::AlreadyPresent);
        }
        self.client
            .create_sdn_vnet(&self.ledger, &vnet.name, &vnet.zone, Some(&alias))
            .map_err(delonix_model::Error::from)?;
        Ok(EnsureOutcome::Created)
    }

    fn remove_vnet(&self, name: &str, owner: &OwnerMark) -> delonix_model::Result<RemoveOutcome> {
        let vnets = self
            .client
            .sdn_vnets()
            .map_err(delonix_model::Error::from)?;
        let Some(row) = vnets
            .iter()
            .find(|v| v.get("vnet").and_then(|s| s.as_str()) == Some(name))
        else {
            return Ok(RemoveOutcome::Absent);
        };
        let found = owner.owner_of(row.get("alias").and_then(|v| v.as_str()).unwrap_or(""));
        if found != Owner::Ours {
            return Ok(RemoveOutcome::NotOwned(found));
        }
        self.client
            .delete_sdn_vnet(&self.ledger, name)
            .map_err(delonix_model::Error::from)?;
        Ok(RemoveOutcome::Removed)
    }

    /// [`Client::sdn_transaction`] (ADR-0049, the cluster's global SDN
    /// lock): the lock is taken WITHOUT `allow-pending`, so a cluster that
    /// already carries someone else's staged changes refuses before `change`
    /// runs (`SdnPendingChanges`, DX-5516) and one whose lock is held refuses
    /// too (`SdnLocked`, DX-5515); `change` failing rolls back what it staged
    /// with the token; `change` succeeding is applied with the token
    /// (`PUT /cluster/sdn`) — one cluster reload per apply or teardown, as
    /// before, and never someone else's work pushed along with it.
    fn transaction(
        &self,
        change: &mut dyn FnMut() -> delonix_model::Result<()>,
    ) -> delonix_model::Result<()> {
        self.client
            .sdn_transaction(&self.ledger, || change().map_err(Error::Engine))
            .map_err(delonix_model::Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark() -> OwnerMark {
        OwnerMark::new("dlx-0123456789abcdef").unwrap()
    }

    fn spec(alias: Option<&str>) -> VNetSpec {
        VNetSpec {
            name: "v1".into(),
            zone: "z1".into(),
            alias: alias.map(str::to_string),
        }
    }

    #[test]
    fn the_alias_carries_the_declared_text_then_the_mark() {
        assert_eq!(
            stamped_alias(Some("web tier"), &mark()).unwrap(),
            "web tier [delonix-owner:dlx-0123456789abcdef]"
        );
        assert_eq!(stamped_alias(None, &mark()).unwrap(), mark().tag());
    }

    #[test]
    fn an_alias_that_no_longer_fits_with_the_mark_is_refused_before_any_request() {
        let long = "a".repeat(250);
        let e = stamped_alias(Some(&long), &mark()).unwrap_err().to_string();
        assert!(e.contains("256"), "{e}");
    }

    #[test]
    fn a_vnet_moved_to_another_zone_or_realiased_is_drift() {
        let row = serde_json::json!({
            "vnet": "v1", "zone": "z2", "alias": format!("other {}", mark().tag()),
        });
        let d = vnet_drift(&row, &spec(Some("web")));
        assert_eq!(d.len(), 2, "{d:?}");
        let same = serde_json::json!({
            "vnet": "v1", "zone": "z1", "alias": format!("web {}", mark().tag()),
        });
        assert!(vnet_drift(&same, &spec(Some("web"))).is_empty());
    }
}
