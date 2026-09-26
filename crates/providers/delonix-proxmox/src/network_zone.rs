//! `NetworkZoneProvider` implementation for Proxmox VE's own SDN (Zones,
//! VNets) — ADR-0049 addendum, closing the gap D3 names.
//!
//! Thin, on purpose: every real decision (staged-vs-applied, the id format,
//! the referential order between a zone and its vnets) already lives in
//! `sdn.rs`, built and live-tested by ADR-0049 slice 2 — this module only
//! adapts that surface to the trait `delonix-sdn::network_zone` defines.

use crate::{Client, Error, Ledger};
use delonix_sdn::network_zone::{EnsureOutcome, NetworkZoneProvider, NetworkZoneSpec, VNetSpec};

/// The canonical id this provider registers under, and the only one
/// [`crate::register_network_zone_provider`] uses (`"pve"` as an alias, the
/// same pair [`crate::registration`] registers the `VmBackend` under).
pub const ID: &str = "proxmox";

/// The [`NetworkZoneProvider`] this crate exists to provide: Proxmox's
/// cluster SDN, wrapped.
pub struct ProxmoxNetworkZoneProvider {
    client: std::sync::Arc<Client>,
    ledger: Ledger,
}

impl ProxmoxNetworkZoneProvider {
    pub fn new(client: std::sync::Arc<Client>, ledger: Ledger) -> Self {
        Self { client, ledger }
    }
}

impl NetworkZoneProvider for ProxmoxNetworkZoneProvider {
    fn id(&self) -> &'static str {
        ID
    }

    fn available(&self) -> bool {
        // Registered only once the client already exists (ADR-0008's own
        // reasoning for a remote VmBackend) — by the time this value
        // exists, the target is at least reachable in principle.
        true
    }

    /// Checks presence first, and only creates when absent: `sdn.rs`'s own
    /// `create_sdn_zone` is idempotent (it will not error on a zone that
    /// already exists) but does not itself distinguish "created" from
    /// "already there" for the caller — this method does, by reading
    /// PENDING state before writing, the same read [`Client::sdn_zones`]'s
    /// own doc comment already warns is PENDING and not necessarily live.
    fn ensure_zone(&self, zone: &NetworkZoneSpec) -> delonix_model::Result<EnsureOutcome> {
        let exists = self
            .client
            .sdn_zones()
            .map_err(Error::into_root)?
            .iter()
            .any(|z| z.get("zone").and_then(|v| v.as_str()) == Some(zone.name.as_str()));
        if exists {
            return Ok(EnsureOutcome::AlreadyPresent);
        }
        self.client
            .create_sdn_zone(&self.ledger, &zone.name)
            .map_err(Error::into_root)?;
        Ok(EnsureOutcome::Created)
    }

    /// The node itself refuses this while a vnet still references the zone
    /// (`Client::delete_sdn_zone`'s own doc comment) — surfaced as-is, never
    /// pre-empted here: the caller (`cmd::network_zone::remove_for_replace`)
    /// is what orders vnets before their zone.
    fn remove_zone(&self, name: &str) -> delonix_model::Result<()> {
        self.client
            .delete_sdn_zone(&self.ledger, name)
            .map_err(Error::into_root)
    }

    fn ensure_vnet(&self, vnet: &VNetSpec) -> delonix_model::Result<EnsureOutcome> {
        let exists = self
            .client
            .sdn_vnets()
            .map_err(Error::into_root)?
            .iter()
            .any(|v| v.get("vnet").and_then(|s| s.as_str()) == Some(vnet.name.as_str()));
        if exists {
            return Ok(EnsureOutcome::AlreadyPresent);
        }
        self.client
            .create_sdn_vnet(&self.ledger, &vnet.name, &vnet.zone, vnet.alias.as_deref())
            .map_err(Error::into_root)?;
        Ok(EnsureOutcome::Created)
    }

    fn remove_vnet(&self, name: &str) -> delonix_model::Result<()> {
        self.client
            .delete_sdn_vnet(&self.ledger, name)
            .map_err(Error::into_root)
    }

    /// `PUT /cluster/sdn` — the single call that turns every staged
    /// zone/vnet create or delete into something a node actually runs
    /// (`sdn.rs`'s own module doc comment: "the trap — every write here is
    /// STAGED, not applied"). One call per `apply_one`/`remove_for_replace`,
    /// not one per zone/vnet, so a manifest with several vnets pays for one
    /// cluster reload rather than N.
    fn commit(&self) -> delonix_model::Result<()> {
        self.client
            .apply_sdn(&self.ledger)
            .map_err(Error::into_root)
    }
}
