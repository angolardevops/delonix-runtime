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
//! The core that proves the whole staged-config cycle end to end: Zones,
//! VNets, Subnets, and the apply that makes any of them real — plus the
//! single-item read/edit routes for a zone and a vnet. Left out on purpose,
//! each a real and separately-sized Proxmox SDN feature layered on top of
//! this same base: Fabrics, IPAM controllers, DNS controllers, DHCP ranges,
//! per-vnet/per-subnet firewalls, IP allocation (`.../vnets/{vnet}/ips`).
//! [`Client::create_sdn_zone`] only ever creates a `simple` zone — an
//! isolated L3 zone with no VLAN/VXLAN encapsulation, the plainest kind
//! Proxmox has, and the one that needs no VLAN-capable hardware on the node
//! to prove the cycle works. `vlan`/`vxlan`/`qinq` zone types are real and
//! are NOT implemented here, and [`Client::update_sdn_zone`] only edits a
//! `simple` zone's `mtu` — the one field every zone type shares that is
//! simple enough to prove an edit round-trips, not the EVPN/VXLAN/QinQ
//! fields `ZONE_PROPERTIES` also has (`rt-import`, `vxlan-port`, `tag`, …).
//! A subnet, similarly, is created here with only its CIDR and an optional
//! gateway — `snat`, `dnszoneprefix`, `dhcp-range` and `dhcp-dns-server` are
//! real Proxmox subnet fields this module does not send.
//!
//! **Every fact this module states about the shape of a Proxmox route was
//! read from `git.proxmox.com/pve-network.git` (the `pve-network` Debian
//! source package this crate's target node's SDN stack actually ships),
//! not guessed and not inferred from `docs/proxmox/api-9.2.2.routes.json`
//! alone** — that inventory (`scripts/proxmox_api_inventory.py`) records
//! only `method`/`path`/`returns`/`perm`, never a route's parameter schema,
//! and this crate's tests are not free to reach a live node to fill that
//! gap by trial and error. Every doc comment below that claims a field name,
//! an id format, or what a route's handler actually does names the exact
//! `.pm` file it was read from.
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
//!
//! # A subnet's id is not something a caller ever writes
//!
//! A zone and a vnet are named by the id the caller GIVES them
//! ([`validate_sdn_id`]). A subnet is different: `POST
//! /cluster/sdn/vnets/{vnet}/subnets` takes a `subnet` body parameter that
//! is the plain CIDR (`10.0.0.0/24`), and Proxmox computes the object's own
//! id server-side from it — `<zoneid>-<network>-<mask>` (the CIDR with its
//! `/` swapped for a `-`, prefixed with the zone id and another `-`; e.g.
//! zone `z1`, CIDR `10.0.0.0/24` → `z1-10.0.0.0-24`). Read from
//! `PVE::API2::Network::SDN::Subnets`'s `create` handler
//! (`my $id = $cidr =~ s/\//-/r; $id = "$zoneid-$id";`) and confirmed by
//! `PVE::Network::SDN::Subnets::sdn_subnets_config`'s inverse
//! (`my ($zone, $network, $mask) = split(/-/, $id);`), both in
//! `git.proxmox.com/pve-network.git`.
//!
//! `POST` answers `null` (the same trap as every other write in this
//! module), so there is no way to learn this id from the node's own
//! response to a create — [`sdn_subnet_id`] computes it, and every function
//! that addresses ONE subnet ([`Client::sdn_vnet_subnet`],
//! [`Client::update_sdn_subnet`], [`Client::delete_sdn_subnet`]) takes the
//! same `(vnet, zone, cidr)` a caller already has — never Proxmox's encoded
//! id — and re-derives it, the same way [`validate_sdn_id`] already keeps
//! this module's callers from having to know that a zone id is limited to
//! 8 characters. `zone` is not looked up from `vnet`'s own record to build
//! it: it is the SAME value the caller already gave
//! [`Client::create_sdn_vnet`], and re-deriving it here costs nothing while
//! sparing an extra round trip.
//!
//! # The two node-side routes here answer a directory index, not "is this real"
//!
//! [`Client::sdn_zone_node_index`]/[`Client::sdn_vnet_node_index`]
//! (`GET /nodes/{node}/sdn/zones/{zone}`/`GET /nodes/{node}/sdn/vnets/{vnet}`)
//! read as if they were the per-node answer to "is this zone/vnet actually
//! live here" — that is what a caller reasonably expects from a route under
//! `/nodes/{node}/sdn/...` rather than `/cluster/sdn/...`, and it is NOT
//! what they do. Read from `PVE::API2::Network::SDN::Nodes::Zone`'s and
//! `::Nodes::Vnet`'s `diridx` handlers (`git.proxmox.com/pve-network.git`):
//! each checks only that the caller holds `SDN.Audit` on
//! `/sdn/zones/<zone>[/<vnet>]` — a PERMISSION check, which `root@pam`
//! passes unconditionally — and then answers a FIXED, hardcoded list of
//! subdirectory names (`content`/`bridges`/`ip-vrf` for a zone,
//! `mac-vrf` for a vnet), the same whether the object exists, is only
//! staged, or was applied and is genuinely running on this node. The route
//! that actually proves realization is `GET
//! /nodes/{node}/sdn/zones/{zone}/content` (lists the zone's vnets WITH a
//! `status` field) — a separate route, out of this slice's scope. What
//! these two calls are worth: proving the route exists and answers against
//! a real node, and that `zone`/`vnet` reached the URL path unmangled —
//! nothing about the dataplane.

use crate::{parse, Client, Error, Ledger, Result, TaskKind, Wrapped};
use std::net::{Ipv4Addr, Ipv6Addr};

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

/// An SDN subnet's CIDR, checked before it goes into the `subnet` body
/// parameter of a create/update AND — with its `/` swapped for `-` by
/// [`sdn_subnet_id`] — into a URL path.
///
/// Parsed with [`Ipv4Addr`]/[`Ipv6Addr`] and nothing else: any character
/// that is not part of a well-formed dotted-quad or a well-formed IPv6
/// literal fails to parse there, which is what keeps a crafted `cidr`
/// value from ever reaching the URL path this module builds around it —
/// the same discipline [`validate_sdn_id`] already applies to a zone/vnet
/// name. A prefix length is bounds-checked against the address family it
/// actually parsed as (32 for v4, 128 for v6): a v4 address with a v6-sized
/// prefix is refused rather than silently accepted as "technically a
/// number".
pub fn validate_cidr(cidr: &str) -> Result<()> {
    let invalid = || {
        Error::InvalidCidr(format!(
            "invalid Proxmox SDN subnet '{cidr}': expected <address>/<prefix-length>, e.g. \
             '10.0.0.0/24' or 'fd00::/64', with a prefix length valid for the address family"
        ))
    };
    let (addr, prefix) = cidr.split_once('/').ok_or_else(invalid)?;
    let prefix: u8 = prefix.parse().map_err(|_| invalid())?;
    let ok = match (addr.parse::<Ipv4Addr>(), addr.parse::<Ipv6Addr>()) {
        (Ok(_), _) => prefix <= 32,
        (_, Ok(_)) => prefix <= 128,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(invalid())
    }
}

/// Proxmox's own construction of a subnet object's id — see the module doc
/// comment's "A subnet's id is not something a caller ever writes" section
/// for where this was read and why it exists at all. `cidr` is assumed
/// already checked by [`validate_cidr`] and `zone` by [`validate_sdn_id`]:
/// every caller of this function validates both first.
fn sdn_subnet_id(zone: &str, cidr: &str) -> String {
    format!("{zone}-{}", cidr.replacen('/', "-", 1))
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

    /// One SDN zone from the PENDING configuration
    /// (`GET /cluster/sdn/zones/{zone}`). Same PENDING-not-live caveat as
    /// [`Self::sdn_zones`].
    pub fn sdn_zone(&self, zone: &str) -> Result<serde_json::Value> {
        validate_sdn_id(zone)?;
        let path = format!("/cluster/sdn/zones/{zone}");
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> = parse(&body, "GET /cluster/sdn/zones/{zone}")?;
        Ok(w.data)
    }

    /// Stages a change to an existing `simple` zone's `mtu`
    /// (`PUT /cluster/sdn/zones/{zone}`) — the bridge MTU Proxmox gives
    /// every VNet the zone creates. STAGED, same trap as every write in
    /// this module: nothing on any node changes until [`Self::apply_sdn`].
    ///
    /// `mtu: None` sends no `mtu` field at all, which Proxmox's own `PUT`
    /// handler treats as "leave it as it is" — an omitted optional field is
    /// not the same request as an explicit clear, which needs the route's
    /// own `delete=<field>` list and is out of this narrow scope (see the
    /// module doc comment's "Scope, deliberately narrow" section for why
    /// `mtu` is the one field this call touches at all).
    pub fn update_sdn_zone(&self, ledger: &Ledger, zone: &str, mtu: Option<u16>) -> Result<()> {
        validate_sdn_id(zone)?;
        let mtu_text = mtu.map(|m| m.to_string());
        let mut form: Vec<(&str, &str)> = Vec::new();
        if let Some(m) = &mtu_text {
            form.push(("mtu", m.as_str()));
        }
        let path = format!("/cluster/sdn/zones/{zone}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnZone,
            || self.put_form(&path, &form),
            Some(&|| {
                Ok(self
                    .sdn_zone(zone)?
                    .get("mtu")
                    .and_then(serde_json::Value::as_u64)
                    == mtu.map(u64::from))
            }),
        )
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

    /// One SDN vnet from the PENDING configuration
    /// (`GET /cluster/sdn/vnets/{vnet}`). Same PENDING-not-live caveat as
    /// [`Self::sdn_vnets`].
    pub fn sdn_vnet(&self, vnet: &str) -> Result<serde_json::Value> {
        validate_sdn_id(vnet)?;
        let path = format!("/cluster/sdn/vnets/{vnet}");
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> = parse(&body, "GET /cluster/sdn/vnets/{vnet}")?;
        Ok(w.data)
    }

    /// Stages a change to an existing vnet's `alias`
    /// (`PUT /cluster/sdn/vnets/{vnet}`). STAGED, same trap as every write
    /// in this module.
    ///
    /// `alias: None` leaves it as it is, same "an omitted field is not a
    /// clear" reasoning as [`Self::update_sdn_zone`]'s `mtu`. Reassigning
    /// `vnet` to a different `zone` is deliberately not exposed here: the
    /// node itself refuses that once the vnet has subnets
    /// (`PVE::API2::Network::SDN::Vnets`'s `update` handler,
    /// `git.proxmox.com/pve-network.git`), and a caller of
    /// [`Self::create_sdn_subnet`] can leave one behind without knowing it.
    pub fn update_sdn_vnet(&self, ledger: &Ledger, vnet: &str, alias: Option<&str>) -> Result<()> {
        validate_sdn_id(vnet)?;
        let mut form: Vec<(&str, &str)> = Vec::new();
        if let Some(a) = alias {
            form.push(("alias", a));
        }
        let path = format!("/cluster/sdn/vnets/{vnet}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnVnet,
            || self.put_form(&path, &form),
            Some(&|| Ok(self.sdn_vnet(vnet)?.get("alias").and_then(|v| v.as_str()) == alias)),
        )
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

    /// Every SDN subnet of `vnet`, in the cluster's PENDING configuration
    /// (`GET /cluster/sdn/vnets/{vnet}/subnets`). Same PENDING-not-live
    /// caveat as [`Self::sdn_zones`]/[`Self::sdn_vnets`].
    pub fn sdn_vnet_subnets(&self, vnet: &str) -> Result<Vec<serde_json::Value>> {
        validate_sdn_id(vnet)?;
        let path = format!("/cluster/sdn/vnets/{vnet}/subnets");
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /cluster/sdn/vnets/{vnet}/subnets")?;
        Ok(w.data)
    }

    /// One SDN subnet of `vnet`, identified by the zone it was created in and
    /// its CIDR (`GET /cluster/sdn/vnets/{vnet}/subnets/{subnet}`) — never by
    /// Proxmox's own encoded id, see the module doc comment's "A subnet's id
    /// is not something a caller ever writes" section. Same PENDING-not-live
    /// caveat as [`Self::sdn_vnet_subnets`].
    pub fn sdn_vnet_subnet(&self, vnet: &str, zone: &str, cidr: &str) -> Result<serde_json::Value> {
        validate_sdn_id(vnet)?;
        validate_sdn_id(zone)?;
        validate_cidr(cidr)?;
        let id = sdn_subnet_id(zone, cidr);
        let path = format!("/cluster/sdn/vnets/{vnet}/subnets/{id}");
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> =
            parse(&body, "GET /cluster/sdn/vnets/{vnet}/subnets/{subnet}")?;
        Ok(w.data)
    }

    /// Stages a new subnet inside `vnet` — the actual routed CIDR, with an
    /// optional `gateway` (`POST /cluster/sdn/vnets/{vnet}/subnets`).
    /// `zone` is the same zone `vnet` was created in
    /// ([`Self::create_sdn_vnet`]'s own `zone` argument): it is never sent
    /// to the node (the subnet object's schema,
    /// `PVE::Network::SDN::SubnetPlugin::properties`, has no `zone` field
    /// of its own — the node derives it server-side from `vnet`), only
    /// used here to compute the id this call returns, the same
    /// construction Proxmox itself uses (see the module doc comment).
    ///
    /// Changes nothing on any node until [`Self::apply_sdn`] — the module's
    /// central trap. Deliberately narrow, matching the rest of this module
    /// (see "Scope, deliberately narrow" in the module doc comment):
    /// `snat`, `dnszoneprefix`, `dhcp-range` and `dhcp-dns-server` are real
    /// Proxmox subnet fields this call does not send.
    ///
    /// Returns the id the node will file this subnet under — the ONLY way
    /// to learn it, since `POST` answers `null`.
    pub fn create_sdn_subnet(
        &self,
        ledger: &Ledger,
        vnet: &str,
        zone: &str,
        cidr: &str,
        gateway: Option<&str>,
    ) -> Result<String> {
        validate_sdn_id(vnet)?;
        validate_sdn_id(zone)?;
        validate_cidr(cidr)?;
        let id = sdn_subnet_id(zone, cidr);
        let mut form: Vec<(&str, &str)> = vec![("subnet", cidr), ("type", "subnet")];
        if let Some(g) = gateway {
            form.push(("gateway", g));
        }
        let path = format!("/cluster/sdn/vnets/{vnet}/subnets");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::CreateSdnSubnet,
            || self.post_form(&path, &form, true),
            Some(&|| {
                Ok(self
                    .sdn_vnet_subnets(vnet)?
                    .iter()
                    .any(|s| s.get("subnet").and_then(|v| v.as_str()) == Some(id.as_str())))
            }),
        )?;
        Ok(id)
    }

    /// Stages a change to an existing subnet's `gateway`
    /// (`PUT /cluster/sdn/vnets/{vnet}/subnets/{subnet}`). Same STAGED
    /// trap, same `zone`-is-not-looked-up reasoning as
    /// [`Self::sdn_vnet_subnet`], same "an omitted field leaves it as it
    /// is" reasoning as [`Self::update_sdn_zone`]'s `mtu`.
    pub fn update_sdn_subnet(
        &self,
        ledger: &Ledger,
        vnet: &str,
        zone: &str,
        cidr: &str,
        gateway: Option<&str>,
    ) -> Result<()> {
        validate_sdn_id(vnet)?;
        validate_sdn_id(zone)?;
        validate_cidr(cidr)?;
        let id = sdn_subnet_id(zone, cidr);
        let mut form: Vec<(&str, &str)> = Vec::new();
        if let Some(g) = gateway {
            form.push(("gateway", g));
        }
        let path = format!("/cluster/sdn/vnets/{vnet}/subnets/{id}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnSubnet,
            || self.put_form(&path, &form),
            Some(&|| {
                Ok(self
                    .sdn_vnet_subnet(vnet, zone, cidr)?
                    .get("gateway")
                    .and_then(|v| v.as_str())
                    == gateway)
            }),
        )
    }

    /// Removes a subnet from the PENDING configuration
    /// (`DELETE /cluster/sdn/vnets/{vnet}/subnets/{subnet}`). Same
    /// PENDING-only caveat as [`Self::delete_sdn_zone`]/
    /// [`Self::delete_sdn_vnet`]; same `zone`-is-not-looked-up reasoning as
    /// [`Self::sdn_vnet_subnet`].
    pub fn delete_sdn_subnet(
        &self,
        ledger: &Ledger,
        vnet: &str,
        zone: &str,
        cidr: &str,
    ) -> Result<()> {
        validate_sdn_id(vnet)?;
        validate_sdn_id(zone)?;
        validate_cidr(cidr)?;
        let id = sdn_subnet_id(zone, cidr);
        let path = format!("/cluster/sdn/vnets/{vnet}/subnets/{id}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnSubnet,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .sdn_vnet_subnets(vnet)?
                    .iter()
                    .any(|s| s.get("subnet").and_then(|v| v.as_str()) == Some(id.as_str())))
            }),
        )
    }

    /// The node's OWN directory index for `zone`
    /// (`GET /nodes/{node}/sdn/zones/{zone}`) — NOT the zone's realized
    /// state. See the module doc comment's "The two node-side routes here
    /// answer a directory index, not 'is this real'" section before
    /// reading anything into what this returns: a fixed
    /// `[{"subdir": "content"}, {"subdir": "bridges"}, {"subdir": "ip-vrf"}]`,
    /// unconditionally, whether `zone` is only staged or genuinely applied
    /// on this node.
    pub fn sdn_zone_node_index(&self, zone: &str) -> Result<Vec<serde_json::Value>> {
        validate_sdn_id(zone)?;
        let path = format!("/nodes/{}/sdn/zones/{zone}", self.node);
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /nodes/{node}/sdn/zones/{zone}")?;
        Ok(w.data)
    }

    /// The node's OWN directory index for `vnet`
    /// (`GET /nodes/{node}/sdn/vnets/{vnet}`) — same shape and the same
    /// "not proof of realization" caveat as
    /// [`Self::sdn_zone_node_index`], answering a fixed
    /// `[{"subdir": "mac-vrf"}]`.
    pub fn sdn_vnet_node_index(&self, vnet: &str) -> Result<Vec<serde_json::Value>> {
        validate_sdn_id(vnet)?;
        let path = format!("/nodes/{}/sdn/vnets/{vnet}", self.node);
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /nodes/{node}/sdn/vnets/{vnet}")?;
        Ok(w.data)
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
    use super::{sdn_subnet_id, validate_cidr, validate_sdn_id};

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

    #[test]
    fn validate_cidr_accepts_v4_and_v6_within_their_own_prefix_range() {
        for cidr in [
            "10.0.0.0/24",
            "0.0.0.0/0",
            "255.255.255.255/32",
            "10.1.2.3/8",
        ] {
            assert!(validate_cidr(cidr).is_ok(), "{cidr} should be accepted");
        }
        for cidr in ["fd00::/64", "::/0", "fd00:1234::1/128", "2001:db8::/32"] {
            assert!(validate_cidr(cidr).is_ok(), "{cidr} should be accepted");
        }
    }

    #[test]
    fn validate_cidr_refuses_a_prefix_too_long_for_the_address_family() {
        // 33..=128 is a real prefix length — just not for an IPv4 address,
        // which is exactly the "technically a number" trap the doc comment
        // warns against accepting.
        assert!(validate_cidr("10.0.0.0/33").is_err());
        assert!(validate_cidr("10.0.0.0/128").is_err());
    }

    #[test]
    fn validate_cidr_refuses_what_is_not_a_cidr_at_all() {
        let bad = [
            "",                  // empty
            "10.0.0.0",          // no prefix length
            "10.0.0.0/",         // prefix length missing
            "10.0.0.0/-1",       // negative
            "not-an-address/24", // not IPv4 or IPv6 at all
            "10.0.0.0/24/extra", // a third segment
        ];
        for cidr in bad {
            assert!(validate_cidr(cidr).is_err(), "{cidr:?} should be refused");
        }
    }

    /// The same discipline [`the_refusal_names_the_bad_id`] already checks
    /// for a zone/vnet id.
    #[test]
    fn validate_cidr_names_the_bad_value() {
        let e = validate_cidr("not-a-cidr").unwrap_err().to_string();
        assert!(e.contains("not-a-cidr"), "{e}");
    }

    /// The exact construction read from
    /// `PVE::API2::Network::SDN::Subnets`'s `create` handler
    /// (`git.proxmox.com/pve-network.git`) — see the module doc comment.
    #[test]
    fn sdn_subnet_id_matches_what_pve_itself_computes() {
        assert_eq!(sdn_subnet_id("z1", "10.0.0.0/24"), "z1-10.0.0.0-24");
        // IPv6: the address itself carries colons, never a dash, so the
        // split on `-` a reader (or PVE's own inverse,
        // `PVE::Network::SDN::Subnets::sdn_subnets_config`) does still lands
        // on exactly three fields.
        assert_eq!(sdn_subnet_id("z1", "fd00::/64"), "z1-fd00::-64");
    }
}
