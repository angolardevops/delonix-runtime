//! Proxmox VE's OWN NATIVE software-defined networking: Zones and VNets.
//!
//! **Not `delonix-sdn`.** This engine already has its own, completely
//! separate SDN — netns/nftables, rootless, running on THIS host for THIS
//! engine's own containers and VMs (`delonix-sdn`). What lives in this file
//! is a different thing entirely: a Proxmox VE **cluster's** own SDN
//! subsystem, a set of configuration objects (`/cluster/sdn/*`) that
//! *Proxmox itself* provisions on every node's own bridges, VLANs or VXLANs.
//! A doc comment or a message in this file that says "SDN" without saying
//! which one means Proxmox's — everywhere else in this crate, "the SDN" that
//! a VM cannot join (`refuse_unsupported`'s `network` case) is the OTHER one.
//!
//! # Scope, deliberately narrow
//!
//! Only the core that proves the whole staged-config cycle end to end:
//! Zones, VNets, and the apply that makes either one real. Left out on
//! purpose, each a real and separately-sized Proxmox SDN feature layered on
//! top of this same base: Subnets, Fabrics, IPAM controllers, DNS
//! controllers, DHCP ranges. [`Client::create_sdn_zone`] only ever creates a
//! `simple` zone — an isolated L3 zone with no VLAN/VXLAN encapsulation,
//! the plainest kind Proxmox has, and the one that needs no VLAN-capable
//! hardware on the node to prove the cycle works. `vlan`/`vxlan`/`qinq` zone
//! types are real and are NOT implemented here.
//!
//! # The trap: every write here is STAGED, not applied
//!
//! **This is the single most important thing about this module.** Creating,
//! editing or deleting a zone or a vnet only edits a PENDING configuration —
//! nothing on any node in the cluster changes until a SEPARATE call,
//! [`Client::apply_sdn`] (`PUT /cluster/sdn`, no body), reloads that pending
//! config onto every node as its own asynchronous task. Call
//! [`Client::sdn_zones`] right after [`Client::create_sdn_zone`] and the zone
//! is THERE — because a `GET` reads the same pending state a `POST` just
//! wrote, not because anything real happened. A caller that creates a
//! zone/vnet and never calls `apply_sdn` has changed nothing any node will
//! ever act on, and every `GET` will keep agreeing with them anyway.
//!
//! It is the same shape of trap this crate already documents for the node's
//! own firewall ("does nothing until enabled") — a case where reading the
//! API's own state is not the same question as asking what the node is
//! actually doing. Every test and every caller in this module has to close
//! the loop with `apply_sdn`, or it has proven nothing.

use crate::{parse, Client, Error, Ledger, Result, TaskKind, Wrapped};

/// The `vmid` [`Client::apply_sdn`] hands to the shared task-submission path.
///
/// Every task-submitting helper in this crate (`Client::task`/`task_or_done`,
/// the ledger, `recover_lost_answer`) is written assuming a task belongs to
/// ONE VM — that is what every other caller in `lib.rs` is. The SDN reload
/// has no VM of its own at all: it is cluster-scoped, not VM-scoped. Rather
/// than add a second, parallel task-submission path with no VM parameter (and
/// duplicate the lock-retry/ledger/lost-answer logic that path already has),
/// this reuses it with a sentinel: `0` is never a real Proxmox vmid (they are
/// allocated starting at 100 — see [`Client::next_vmid`]), so it can never
/// collide with a genuine VM's ledger entry, and it reads, to anyone looking
/// at a ledger file by eye, as "not a VM".
pub(crate) const SDN_VMID: u32 = 0;

/// A Proxmox SDN zone or vnet id, checked before it goes into a URL path and
/// into the node's own SDN namespace.
///
/// Proxmox's own `pve-sdn-id` format: a lowercase letter, then up to 7 more
/// lowercase letters or digits — 8 characters total. Refused rather than
/// guessed loosely, the same discipline [`crate::validate_bridge_name`] and
/// [`crate::validate_node_name`] already apply to what goes into a URL path:
/// an id this backend accepted and the node refused would report a Proxmox
/// error instead of ours, and an id the node silently reinterpreted would be
/// exactly the failure mode this whole crate treats as its worst.
pub fn validate_sdn_id(id: &str) -> Result<()> {
    let mut chars = id.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && id.len() <= 8
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidSdnId(format!(
            "invalid Proxmox SDN id '{id}': expected a lowercase letter then up to 7 more \
             lowercase letters or digits (8 characters total)"
        )))
    }
}

impl Client {
    /// Every SDN zone in the cluster's PENDING configuration
    /// (`GET /cluster/sdn/zones`).
    ///
    /// **PENDING, not live** — see the module doc comment. A zone this shows
    /// may not exist on any node yet, and a zone a node is still realizing
    /// may already be gone from here if it was deleted and not yet applied.
    pub fn sdn_zones(&self) -> Result<Vec<serde_json::Value>> {
        let body = self.get("/cluster/sdn/zones")?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "/cluster/sdn/zones")?;
        Ok(w.data)
    }

    /// Stages a new `simple` SDN zone (`POST /cluster/sdn/zones`).
    ///
    /// `simple` only — see the module doc comment for why. Changes nothing on
    /// any node until [`Self::apply_sdn`].
    pub fn create_sdn_zone(&self, ledger: &Ledger, zone: &str) -> Result<()> {
        validate_sdn_id(zone)?;
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::CreateSdnZone,
            || {
                self.post_form(
                    "/cluster/sdn/zones",
                    &[("zone", zone), ("type", "simple")],
                    true,
                )
            },
            Some(&|| {
                Ok(self
                    .sdn_zones()?
                    .iter()
                    .any(|z| z.get("zone").and_then(|v| v.as_str()) == Some(zone)))
            }),
        )
    }

    /// Removes a zone from the PENDING configuration
    /// (`DELETE /cluster/sdn/zones/{zone}`).
    ///
    /// The node refuses this while a vnet still references the zone — its
    /// own business logic, surfaced as an ordinary node error and not
    /// pre-empted here. Changes nothing on any node until [`Self::apply_sdn`]:
    /// a node that already realized this zone keeps running it until then.
    pub fn delete_sdn_zone(&self, ledger: &Ledger, zone: &str) -> Result<()> {
        validate_sdn_id(zone)?;
        let path = format!("/cluster/sdn/zones/{zone}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnZone,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .sdn_zones()?
                    .iter()
                    .any(|z| z.get("zone").and_then(|v| v.as_str()) == Some(zone)))
            }),
        )
    }

    /// Every SDN vnet in the cluster's PENDING configuration
    /// (`GET /cluster/sdn/vnets`). Same PENDING-not-live caveat as
    /// [`Self::sdn_zones`].
    pub fn sdn_vnets(&self) -> Result<Vec<serde_json::Value>> {
        let body = self.get("/cluster/sdn/vnets")?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "/cluster/sdn/vnets")?;
        Ok(w.data)
    }

    /// Stages a new vnet inside `zone` (`POST /cluster/sdn/vnets`), with an
    /// optional human-readable `alias`.
    ///
    /// Changes nothing on any node until [`Self::apply_sdn`] — same trap as
    /// the zone calls above.
    pub fn create_sdn_vnet(
        &self,
        ledger: &Ledger,
        vnet: &str,
        zone: &str,
        alias: Option<&str>,
    ) -> Result<()> {
        validate_sdn_id(vnet)?;
        validate_sdn_id(zone)?;
        let mut form: Vec<(&str, &str)> = vec![("vnet", vnet), ("zone", zone)];
        if let Some(a) = alias {
            form.push(("alias", a));
        }
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::CreateSdnVnet,
            || self.post_form("/cluster/sdn/vnets", &form, true),
            Some(&|| {
                Ok(self
                    .sdn_vnets()?
                    .iter()
                    .any(|v| v.get("vnet").and_then(|s| s.as_str()) == Some(vnet)))
            }),
        )
    }

    /// Removes a vnet from the PENDING configuration
    /// (`DELETE /cluster/sdn/vnets/{vnet}`). Same PENDING-not-live caveat as
    /// [`Self::delete_sdn_zone`].
    pub fn delete_sdn_vnet(&self, ledger: &Ledger, vnet: &str) -> Result<()> {
        validate_sdn_id(vnet)?;
        let path = format!("/cluster/sdn/vnets/{vnet}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnVnet,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .sdn_vnets()?
                    .iter()
                    .any(|v| v.get("vnet").and_then(|s| s.as_str()) == Some(vnet)))
            }),
        )
    }

    /// Reloads the PENDING SDN configuration onto EVERY node in the cluster
    /// (`PUT /cluster/sdn`, no body) — the call that makes a staged zone or
    /// vnet real.
    ///
    /// **This is the one call in this module that genuinely forks a task.**
    /// Every zone/vnet create/delete above almost certainly only edits a
    /// config file inline; this is what reads that file back and reprograms
    /// every node's own networking stack. A caller that creates a zone/vnet
    /// and never calls this has changed nothing on any node — see the module
    /// doc comment.
    pub fn apply_sdn(&self, ledger: &Ledger) -> Result<()> {
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::ApplySdn,
            || self.put_form("/cluster/sdn", &[]),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::validate_sdn_id;

    #[test]
    fn accepts_the_shape_pve_documents() {
        for id in ["z", "z1", "zone1", "a1234567"] {
            assert!(validate_sdn_id(id).is_ok(), "{id} should be accepted");
        }
    }

    #[test]
    fn refuses_what_pve_would_refuse() {
        let bad = [
            "",          // empty
            "Zone1",     // uppercase leading letter
            "1zone",     // leading digit
            "-zone",     // leading dash
            "zone_1",    // underscore is not accepted
            "toolong99", // 9 characters, over the 8-char limit
            "z one",     // whitespace
        ];
        for id in bad {
            assert!(validate_sdn_id(id).is_err(), "{id:?} should be refused");
        }
    }

    /// The refusal names the id so a script grepping the message can find it,
    /// the same convention [`crate::validate_bridge_name`]'s and
    /// [`crate::validate_node_name`]'s own messages already follow.
    #[test]
    fn the_refusal_names_the_bad_id() {
        let e = validate_sdn_id("Not-Valid").unwrap_err().to_string();
        assert!(e.contains("Not-Valid"), "{e}");
    }
}
