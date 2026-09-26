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
//! single-item read/edit routes for a zone and a vnet. On top of that base,
//! since 2026-09-25: IPAM controllers (`/cluster/sdn/ipams`), DNS
//! controllers (`/cluster/sdn/dns`), Fabrics and their nodes
//! (`/cluster/sdn/fabrics/*` and the node-side reads), a zone's DHCP/IPAM/DNS
//! fields ([`ZoneOptions`]), a subnet's DHCP ranges ([`SubnetOptions`]), and
//! IP reservations on a vnet (`.../vnets/{vnet}/ips`). Still left out on
//! purpose: per-vnet/per-subnet firewalls, EVPN controllers, route maps and
//! prefix lists, the `wireguard`/`bgp` fabric protocols.
//! [`Client::create_sdn_zone`] only ever creates a `simple` zone — an
//! isolated L3 zone with no VLAN/VXLAN encapsulation, the plainest kind
//! Proxmox has, and the one that needs no VLAN-capable hardware on the node
//! to prove the cycle works. `vlan`/`vxlan`/`qinq` zone types are real and
//! are NOT implemented here, and [`Client::update_sdn_zone`] only edits a
//! `simple` zone's `mtu` — the one field every zone type shares that is
//! simple enough to prove an edit round-trips, not the EVPN/VXLAN/QinQ
//! fields `ZONE_PROPERTIES` also has (`rt-import`, `vxlan-port`, `tag`, …).
//! A subnet is created by [`Client::create_sdn_subnet`] with only its CIDR
//! and an optional gateway; [`Client::create_sdn_subnet_with`] adds
//! `dhcp-range`, `dhcp-dns-server` and `snat` — `dnszoneprefix` is a real
//! Proxmox subnet field this module still does not send.
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
//! And an apply that reports `OK` is not proof either. Measured on this
//! repository's own Proxmox appliance image: its rewritten
//! `/etc/network/interfaces` lacked the `source /etc/network/interfaces.d/*`
//! directive, the reload task ended `TASK OK` with one warning ("missing
//! 'source /etc/network/interfaces.d/sdn' directive for SDN support!"),
//! and NOTHING was realized — no vnet bridge, no fabric interface, while
//! every `GET /cluster/sdn/...` kept agreeing the config was applied. The
//! only route that tells is [`Client::sdn_zone_content`]
//! (`GET /nodes/{node}/sdn/zones/{zone}/content`): `status: "available"`
//! per vnet when it is real, `status: "error"` with a `statusmsg` when it
//! is not. A caller that needs the network to exist, not the config, reads
//! that after every apply — the live case does.
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
        self.create_sdn_zone_with(ledger, zone, &ZoneOptions::default())
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
        let subnet = sdn_subnet_id(zone, cidr);
        let path = format!("/cluster/sdn/vnets/{vnet}/subnets/{subnet}");
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
    /// used here to compute the subnet this call returns, the same
    /// construction Proxmox itself uses (see the module doc comment).
    ///
    /// Changes nothing on any node until [`Self::apply_sdn`] — the module's
    /// central trap. Deliberately narrow, matching the rest of this module
    /// (see "Scope, deliberately narrow" in the module doc comment):
    /// `snat`, `dnszoneprefix`, `dhcp-range` and `dhcp-dns-server` are real
    /// Proxmox subnet fields this call does not send.
    ///
    /// Returns the subnet the node will file this subnet under — the ONLY way
    /// to learn it, since `POST` answers `null`.
    pub fn create_sdn_subnet(
        &self,
        ledger: &Ledger,
        vnet: &str,
        zone: &str,
        cidr: &str,
        gateway: Option<&str>,
    ) -> Result<String> {
        self.create_sdn_subnet_with(
            ledger,
            vnet,
            zone,
            cidr,
            &SubnetOptions {
                gateway,
                ..Default::default()
            },
        )
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
        self.update_sdn_subnet_with(
            ledger,
            vnet,
            zone,
            cidr,
            &SubnetOptions {
                gateway,
                ..Default::default()
            },
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
        let subnet = sdn_subnet_id(zone, cidr);
        let path = format!("/cluster/sdn/vnets/{vnet}/subnets/{subnet}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnSubnet,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .sdn_vnet_subnets(vnet)?
                    .iter()
                    .any(|s| s.get("subnet").and_then(|v| v.as_str()) == Some(subnet.as_str())))
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

// ---------------------------------------------------------------------------
// The layer on top of zones/vnets/subnets: controllers, fabrics, DHCP, IPs.
//
// Everything below was measured against a live PVE 9.2.2 node on 2026-09-25
// with the schema's own parameter list (`apidoc.js` of that node, the same
// file `docs/proxmox/api-9.2.2.routes.json` was extracted from) open next to
// it — the route shapes, the field names and the three facts no schema
// states, each written where it bites:
//
// * an IPAM or DNS controller is VERIFIED on create and on update — the node
//   calls the controller's URL before it stages anything;
// * the fabrics API (Rust-side in PVE 9) answers a write with an empty
//   STRING, not `null`;
// * `POST/PUT/DELETE /cluster/sdn/vnets/{vnet}/ips` act on the IPAM database
//   directly, against the RUNNING subnet — a subnet that is only staged does
//   not exist for them.
// ---------------------------------------------------------------------------

/// A Proxmox SDN fabric id, checked before it goes into a URL path.
///
/// Proxmox's own `pve-sdn-fabric-id` format (read from the node's `apidoc.js`,
/// pattern `[a-zA-Z0-9][a-zA-Z0-9-]{0,6}[a-zA-Z0-9]`): 2 to 8 characters,
/// letters, digits and `-`, never a `-` at either end. Wider than
/// [`validate_sdn_id`] (uppercase and dashes are allowed, and a leading digit
/// is), so it is its own function rather than a reuse — a fabric id that the
/// zone rule would refuse is one Proxmox accepts.
pub fn validate_fabric_id(id: &str) -> Result<()> {
    let bytes = id.as_bytes();
    let edge = |c: &u8| c.is_ascii_alphanumeric();
    let ok = (2..=8).contains(&bytes.len())
        && edge(&bytes[0])
        && edge(&bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'-');
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidSdnId(format!(
            "invalid Proxmox SDN fabric id '{id}': expected 2 to 8 letters, digits or '-', \
             not starting or ending with '-'"
        )))
    }
}

/// An IPv4 or IPv6 address (no prefix) for an SDN field — a DHCP range end,
/// a DNS server, an IPAM allocation. `what` names the field in the refusal.
pub fn validate_ip(what: &str, ip: &str) -> Result<()> {
    if ip.parse::<Ipv4Addr>().is_ok() || ip.parse::<Ipv6Addr>().is_ok() {
        Ok(())
    } else {
        Err(Error::InvalidSdnAddress(format!(
            "invalid Proxmox SDN {what} '{ip}': expected an IPv4 or IPv6 address"
        )))
    }
}

/// An IPv4 address only — a fabric node's `ip` is `format: ipv4` in the
/// schema, and an IPv6 literal there is a different field (`ip6`).
fn validate_ipv4(what: &str, ip: &str) -> Result<()> {
    if ip.parse::<Ipv4Addr>().is_ok() {
        Ok(())
    } else {
        Err(Error::InvalidSdnAddress(format!(
            "invalid Proxmox SDN {what} '{ip}': expected an IPv4 address"
        )))
    }
}

/// A unicast MAC address in the one form the schema accepts
/// (`XX:XX:XX:XX:XX:XX`, `format: mac-addr`).
pub fn validate_mac(mac: &str) -> Result<()> {
    let groups: Vec<&str> = mac.split(':').collect();
    let ok = groups.len() == 6
        && groups
            .iter()
            .all(|g| g.len() == 2 && g.bytes().all(|b| b.is_ascii_hexdigit()));
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidSdnAddress(format!(
            "invalid Proxmox SDN MAC address '{mac}': expected XX:XX:XX:XX:XX:XX"
        )))
    }
}

/// The external IPAM plugins a controller entry can name. `pve` (the built-in
/// one, always present as the `pve` entry) is not creatable and is not here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpamKind {
    /// NetBox, addressed at its `/api` root; verified by the node with
    /// `GET <url>/ipam/aggregates/` and `Authorization: token <token>`.
    Netbox,
    /// phpIPAM, addressed at its app root; verified by the node with
    /// `GET <url>/sections/<section>` (hence `section` is required for it).
    PhpIpam,
}

impl IpamKind {
    fn as_str(self) -> &'static str {
        match self {
            IpamKind::Netbox => "netbox",
            IpamKind::PhpIpam => "phpipam",
        }
    }
}

/// The routing protocols a fabric can run. `wireguard` and `bgp` exist in the
/// 9.2.2 schema too and are NOT offered here: neither was exercised, and a
/// variant nothing has proven is a promise this module does not make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FabricProtocol {
    OpenFabric,
    Ospf,
}

impl FabricProtocol {
    fn as_str(self) -> &'static str {
        match self {
            FabricProtocol::OpenFabric => "openfabric",
            FabricProtocol::Ospf => "ospf",
        }
    }
}

/// What a zone is created with beyond its id and `type=simple`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ZoneOptions<'a> {
    /// `dhcp=dnsmasq`: the zone hands out addresses from its subnets' DHCP
    /// ranges through a per-zone `dnsmasq` instance the node runs on apply —
    /// the node needs the `dnsmasq` package for that apply to succeed, and
    /// Proxmox's own guidance is to disable the distribution's global
    /// `dnsmasq.service` once installed (the SDN starts its own instances).
    pub dhcp_dnsmasq: bool,
    /// The IPAM controller the zone's subnets allocate from (`pve` when
    /// omitted — Proxmox's default).
    pub ipam: Option<&'a str>,
    /// A DNS controller (by id) the zone registers guests in.
    pub dns: Option<&'a str>,
    /// The DNS zone (domain) those registrations go under.
    pub dnszone: Option<&'a str>,
}

/// One DHCP range of a subnet — sent as the property string
/// `start-address=<ip>,end-address=<ip>` in ONE `dhcp-range` form field; a
/// subnet can carry several, one form field each (the schema's `dhcp-range`
/// is an array). Measured: the bare value without the `start-address=` key is
/// refused by the node ("value without key, but schema does not define a
/// default key").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DhcpRange {
    pub start: String,
    pub end: String,
}

impl DhcpRange {
    fn encoded(&self) -> String {
        format!("start-address={},end-address={}", self.start, self.end)
    }
}

/// What a subnet is created or updated with beyond its CIDR.
#[derive(Clone, Copy, Debug, Default)]
pub struct SubnetOptions<'a> {
    pub gateway: Option<&'a str>,
    pub dhcp_ranges: &'a [DhcpRange],
    /// The DNS server a DHCP lease hands the guest (`dhcp-dns-server`).
    pub dhcp_dns_server: Option<&'a str>,
    /// `snat`: masquerade the subnet's traffic on the node.
    pub snat: Option<bool>,
}

fn dhcp_ranges_of(obj: &serde_json::Value) -> Vec<DhcpRange> {
    obj.get("dhcp-range")
        .and_then(|v| v.as_array())
        .map(|ranges| {
            ranges
                .iter()
                .filter_map(|r| {
                    Some(DhcpRange {
                        start: r.get("start-address")?.as_str()?.to_string(),
                        end: r.get("end-address")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Client {
    // ----- IPAM controllers -------------------------------------------------

    /// Every IPAM controller entry (`GET /cluster/sdn/ipams`). The built-in
    /// `pve` entry is always among them.
    pub fn sdn_ipams(&self) -> Result<Vec<serde_json::Value>> {
        let body = self.get("/cluster/sdn/ipams")?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "/cluster/sdn/ipams")?;
        Ok(w.data)
    }

    /// One IPAM controller entry (`GET /cluster/sdn/ipams/{ipam}`).
    pub fn sdn_ipam(&self, ipam: &str) -> Result<serde_json::Value> {
        validate_sdn_id(ipam)?;
        let path = format!("/cluster/sdn/ipams/{ipam}");
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> = parse(&body, "GET /cluster/sdn/ipams/{ipam}")?;
        Ok(w.data)
    }

    /// Stages an external IPAM controller (`POST /cluster/sdn/ipams`).
    ///
    /// **The node contacts `url` before it stages anything** — measured: a
    /// NetBox entry makes the node `GET <url>/ipam/aggregates/` with
    /// `Authorization: token <token>`, a phpIPAM entry `GET
    /// <url>/sections/<section>`, and an unreachable URL leaves the request
    /// hanging until the node's own HTTP timeout, then fails. A controller
    /// that does not answer cannot be staged, and this call reports that as
    /// the node's error, never as success.
    pub fn create_sdn_ipam(
        &self,
        ledger: &Ledger,
        ipam: &str,
        kind: IpamKind,
        url: &str,
        token: &str,
        section: Option<u32>,
    ) -> Result<()> {
        validate_sdn_id(ipam)?;
        let section_text = section.map(|s| s.to_string());
        let mut form: Vec<(&str, &str)> = vec![
            ("ipam", ipam),
            ("type", kind.as_str()),
            ("url", url),
            ("token", token),
        ];
        if let Some(s) = &section_text {
            form.push(("section", s.as_str()));
        }
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::CreateSdnIpam,
            || self.post_form("/cluster/sdn/ipams", &form, true),
            Some(&|| {
                Ok(self
                    .sdn_ipams()?
                    .iter()
                    .any(|i| i.get("ipam").and_then(|v| v.as_str()) == Some(ipam)))
            }),
        )
    }

    /// Stages a change to an IPAM controller (`PUT /cluster/sdn/ipams/{ipam}`).
    /// Verified against the controller the same way a create is; a field
    /// left `None` is not sent and keeps its value.
    pub fn update_sdn_ipam(
        &self,
        ledger: &Ledger,
        ipam: &str,
        url: Option<&str>,
        token: Option<&str>,
        section: Option<u32>,
    ) -> Result<()> {
        validate_sdn_id(ipam)?;
        let section_text = section.map(|s| s.to_string());
        let mut form: Vec<(&str, &str)> = Vec::new();
        if let Some(u) = url {
            form.push(("url", u));
        }
        if let Some(t) = token {
            form.push(("token", t));
        }
        if let Some(s) = &section_text {
            form.push(("section", s.as_str()));
        }
        let path = format!("/cluster/sdn/ipams/{ipam}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnIpam,
            || self.put_form(&path, &form),
            Some(&|| {
                let obj = self.sdn_ipam(ipam)?;
                Ok(
                    token.is_none_or(|t| obj.get("token").and_then(|v| v.as_str()) == Some(t))
                        && url.is_none_or(|u| obj.get("url").and_then(|v| v.as_str()) == Some(u)),
                )
            }),
        )
    }

    /// Removes an IPAM controller entry (`DELETE /cluster/sdn/ipams/{ipam}`).
    pub fn delete_sdn_ipam(&self, ledger: &Ledger, ipam: &str) -> Result<()> {
        validate_sdn_id(ipam)?;
        let path = format!("/cluster/sdn/ipams/{ipam}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnIpam,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .sdn_ipams()?
                    .iter()
                    .any(|i| i.get("ipam").and_then(|v| v.as_str()) == Some(ipam)))
            }),
        )
    }

    /// The allocations an IPAM holds (`GET /cluster/sdn/ipams/{ipam}/status`).
    ///
    /// Measured: the node answers this for the built-in `pve` IPAM only —
    /// for any other entry it fails with "Currently only PVE IPAM is
    /// supported", its own limitation, surfaced as the node's error.
    pub fn sdn_ipam_status(&self, ipam: &str) -> Result<Vec<serde_json::Value>> {
        validate_sdn_id(ipam)?;
        let path = format!("/cluster/sdn/ipams/{ipam}/status");
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /cluster/sdn/ipams/{ipam}/status")?;
        Ok(w.data)
    }

    // ----- DNS controllers --------------------------------------------------

    /// Every DNS controller entry (`GET /cluster/sdn/dns`).
    pub fn sdn_dns_controllers(&self) -> Result<Vec<serde_json::Value>> {
        let body = self.get("/cluster/sdn/dns")?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "/cluster/sdn/dns")?;
        Ok(w.data)
    }

    /// One DNS controller entry (`GET /cluster/sdn/dns/{dns}`).
    pub fn sdn_dns(&self, dns: &str) -> Result<serde_json::Value> {
        validate_sdn_id(dns)?;
        let path = format!("/cluster/sdn/dns/{dns}");
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> = parse(&body, "GET /cluster/sdn/dns/{dns}")?;
        Ok(w.data)
    }

    /// Stages a PowerDNS controller (`POST /cluster/sdn/dns`, `type=powerdns`
    /// — the only DNS plugin the 9.2.2 schema has). Verified on create the
    /// same way an IPAM is: the node does `GET <url>` with `X-API-Key: <key>`
    /// first, and an unreachable server means no entry.
    pub fn create_sdn_dns(
        &self,
        ledger: &Ledger,
        dns: &str,
        url: &str,
        key: &str,
        ttl: Option<u32>,
    ) -> Result<()> {
        validate_sdn_id(dns)?;
        let ttl_text = ttl.map(|t| t.to_string());
        let mut form: Vec<(&str, &str)> = vec![
            ("dns", dns),
            ("type", "powerdns"),
            ("url", url),
            ("key", key),
        ];
        if let Some(t) = &ttl_text {
            form.push(("ttl", t.as_str()));
        }
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::CreateSdnDns,
            || self.post_form("/cluster/sdn/dns", &form, true),
            Some(&|| {
                Ok(self
                    .sdn_dns_controllers()?
                    .iter()
                    .any(|d| d.get("dns").and_then(|v| v.as_str()) == Some(dns)))
            }),
        )
    }

    /// Stages a change to a DNS controller (`PUT /cluster/sdn/dns/{dns}`).
    pub fn update_sdn_dns(
        &self,
        ledger: &Ledger,
        dns: &str,
        url: Option<&str>,
        key: Option<&str>,
        ttl: Option<u32>,
    ) -> Result<()> {
        validate_sdn_id(dns)?;
        let ttl_text = ttl.map(|t| t.to_string());
        let mut form: Vec<(&str, &str)> = Vec::new();
        if let Some(u) = url {
            form.push(("url", u));
        }
        if let Some(k) = key {
            form.push(("key", k));
        }
        if let Some(t) = &ttl_text {
            form.push(("ttl", t.as_str()));
        }
        let path = format!("/cluster/sdn/dns/{dns}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnDns,
            || self.put_form(&path, &form),
            Some(&|| {
                let obj = self.sdn_dns(dns)?;
                Ok(ttl.is_none_or(|t| {
                    obj.get("ttl").and_then(serde_json::Value::as_u64) == Some(u64::from(t))
                }) && url.is_none_or(|u| obj.get("url").and_then(|v| v.as_str()) == Some(u)))
            }),
        )
    }

    /// Removes a DNS controller entry (`DELETE /cluster/sdn/dns/{dns}`).
    pub fn delete_sdn_dns(&self, ledger: &Ledger, dns: &str) -> Result<()> {
        validate_sdn_id(dns)?;
        let path = format!("/cluster/sdn/dns/{dns}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnDns,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .sdn_dns_controllers()?
                    .iter()
                    .any(|d| d.get("dns").and_then(|v| v.as_str()) == Some(dns)))
            }),
        )
    }

    // ----- Fabrics ----------------------------------------------------------

    /// Every fabric and every fabric node in one answer
    /// (`GET /cluster/sdn/fabrics/all`, an object with `fabrics` and `nodes`).
    pub fn sdn_fabrics_all(&self) -> Result<serde_json::Value> {
        let body = self.get("/cluster/sdn/fabrics/all")?;
        let w: Wrapped<serde_json::Value> = parse(&body, "/cluster/sdn/fabrics/all")?;
        Ok(w.data)
    }

    /// Every fabric (`GET /cluster/sdn/fabrics/fabric`).
    pub fn sdn_fabrics(&self) -> Result<Vec<serde_json::Value>> {
        let body = self.get("/cluster/sdn/fabrics/fabric")?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "/cluster/sdn/fabrics/fabric")?;
        Ok(w.data)
    }

    /// One fabric (`GET /cluster/sdn/fabrics/fabric/{id}`).
    pub fn sdn_fabric(&self, id: &str) -> Result<serde_json::Value> {
        validate_fabric_id(id)?;
        let path = format!("/cluster/sdn/fabrics/fabric/{id}");
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> = parse(&body, "GET /cluster/sdn/fabrics/fabric/{id}")?;
        Ok(w.data)
    }

    /// Stages a fabric (`POST /cluster/sdn/fabrics/fabric`).
    ///
    /// `ip_prefix` is the CIDR the fabric's node addresses are drawn from
    /// (OpenFabric and OSPF both take it); `hello_interval` is OpenFabric's;
    /// `area` is OSPF's and required there. STAGED like every SDN write:
    /// nothing runs on a node until [`Self::apply_sdn`], and applying a
    /// fabric writes FRR configuration on every node it names — a node
    /// without `frr` fails that apply.
    pub fn create_sdn_fabric(
        &self,
        ledger: &Ledger,
        id: &str,
        protocol: FabricProtocol,
        ip_prefix: Option<&str>,
        hello_interval: Option<u32>,
        area: Option<&str>,
    ) -> Result<()> {
        validate_fabric_id(id)?;
        if let Some(p) = ip_prefix {
            validate_cidr(p)?;
        }
        let hello_text = hello_interval.map(|h| h.to_string());
        let mut form: Vec<(&str, &str)> = vec![("id", id), ("protocol", protocol.as_str())];
        if let Some(p) = ip_prefix {
            form.push(("ip_prefix", p));
        }
        if let Some(h) = &hello_text {
            form.push(("hello_interval", h.as_str()));
        }
        if let Some(a) = area {
            form.push(("area", a));
        }
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::CreateSdnFabric,
            || self.post_form("/cluster/sdn/fabrics/fabric", &form, true),
            Some(&|| {
                Ok(self
                    .sdn_fabrics()?
                    .iter()
                    .any(|f| f.get("id").and_then(|v| v.as_str()) == Some(id)))
            }),
        )
    }

    /// Stages a change to a fabric (`PUT /cluster/sdn/fabrics/fabric/{id}`).
    /// `protocol` is required by the route even on an update, and it has to
    /// be the fabric's own — the node refuses a change of protocol.
    pub fn update_sdn_fabric(
        &self,
        ledger: &Ledger,
        id: &str,
        protocol: FabricProtocol,
        hello_interval: Option<u32>,
        area: Option<&str>,
    ) -> Result<()> {
        validate_fabric_id(id)?;
        let hello_text = hello_interval.map(|h| h.to_string());
        let mut form: Vec<(&str, &str)> = vec![("protocol", protocol.as_str())];
        if let Some(h) = &hello_text {
            form.push(("hello_interval", h.as_str()));
        }
        if let Some(a) = area {
            form.push(("area", a));
        }
        let path = format!("/cluster/sdn/fabrics/fabric/{id}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnFabric,
            || self.put_form(&path, &form),
            Some(&|| {
                let obj = self.sdn_fabric(id)?;
                Ok(hello_interval.is_none_or(|h| {
                    obj.get("hello_interval")
                        .and_then(serde_json::Value::as_u64)
                        == Some(u64::from(h))
                }))
            }),
        )
    }

    /// Removes a fabric from the pending configuration
    /// (`DELETE /cluster/sdn/fabrics/fabric/{id}`). The node refuses it while
    /// the fabric still has nodes.
    pub fn delete_sdn_fabric(&self, ledger: &Ledger, id: &str) -> Result<()> {
        validate_fabric_id(id)?;
        let path = format!("/cluster/sdn/fabrics/fabric/{id}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnFabric,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .sdn_fabrics()?
                    .iter()
                    .any(|f| f.get("id").and_then(|v| v.as_str()) == Some(id)))
            }),
        )
    }

    /// The nodes of one fabric (`GET /cluster/sdn/fabrics/node/{fabric_id}`).
    pub fn sdn_fabric_nodes(&self, fabric: &str) -> Result<Vec<serde_json::Value>> {
        validate_fabric_id(fabric)?;
        let path = format!("/cluster/sdn/fabrics/node/{fabric_id}", fabric_id = fabric);
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /cluster/sdn/fabrics/node/{fabric_id}")?;
        Ok(w.data)
    }

    /// One node of a fabric (`GET /cluster/sdn/fabrics/node/{fabric_id}/{node_id}`).
    pub fn sdn_fabric_node(&self, fabric: &str, node: &str) -> Result<serde_json::Value> {
        validate_fabric_id(fabric)?;
        crate::validate_node_name(node)?;
        let path = format!(
            "/cluster/sdn/fabrics/node/{fabric_id}/{node_id}",
            fabric_id = fabric,
            node_id = node
        );
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> =
            parse(&body, "GET /cluster/sdn/fabrics/node/{fabric_id}/{node_id}")?;
        Ok(w.data)
    }

    /// Stages a cluster node's membership of a fabric
    /// (`POST /cluster/sdn/fabrics/node/{fabric_id}`).
    ///
    /// `interfaces` names the node's physical interfaces the protocol runs
    /// on; an EMPTY list is accepted by the node (measured) and yields a
    /// member with only its router address, which is what a single-node
    /// proof uses — a member with real interfaces changes those interfaces'
    /// configuration on apply.
    pub fn create_sdn_fabric_node(
        &self,
        ledger: &Ledger,
        fabric: &str,
        node: &str,
        protocol: FabricProtocol,
        ip: Option<&str>,
        interfaces: &[&str],
    ) -> Result<()> {
        validate_fabric_id(fabric)?;
        crate::validate_node_name(node)?;
        if let Some(ip) = ip {
            validate_ipv4("fabric node address", ip)?;
        }
        let mut form: Vec<(&str, &str)> = vec![("node_id", node), ("protocol", protocol.as_str())];
        if let Some(ip) = ip {
            form.push(("ip", ip));
        }
        for i in interfaces {
            form.push(("interfaces", i));
        }
        let path = format!("/cluster/sdn/fabrics/node/{fabric_id}", fabric_id = fabric);
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::CreateSdnFabricNode,
            || self.post_form(&path, &form, true),
            Some(&|| {
                Ok(self
                    .sdn_fabric_nodes(fabric)?
                    .iter()
                    .any(|n| n.get("node_id").and_then(|v| v.as_str()) == Some(node)))
            }),
        )
    }

    /// Stages a change to a fabric node
    /// (`PUT /cluster/sdn/fabrics/node/{fabric_id}/{node_id}`); `protocol` is
    /// required by the route, as on the fabric itself.
    pub fn update_sdn_fabric_node(
        &self,
        ledger: &Ledger,
        fabric: &str,
        node: &str,
        protocol: FabricProtocol,
        ip: Option<&str>,
    ) -> Result<()> {
        validate_fabric_id(fabric)?;
        crate::validate_node_name(node)?;
        if let Some(ip) = ip {
            validate_ipv4("fabric node address", ip)?;
        }
        let mut form: Vec<(&str, &str)> = vec![("protocol", protocol.as_str())];
        if let Some(ip) = ip {
            form.push(("ip", ip));
        }
        let path = format!(
            "/cluster/sdn/fabrics/node/{fabric_id}/{node_id}",
            fabric_id = fabric,
            node_id = node
        );
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnFabricNode,
            || self.put_form(&path, &form),
            Some(&|| {
                let obj = self.sdn_fabric_node(fabric, node)?;
                Ok(ip.is_none_or(|i| obj.get("ip").and_then(|v| v.as_str()) == Some(i)))
            }),
        )
    }

    /// Removes a node from a fabric
    /// (`DELETE /cluster/sdn/fabrics/node/{fabric_id}/{node_id}`).
    pub fn delete_sdn_fabric_node(&self, ledger: &Ledger, fabric: &str, node: &str) -> Result<()> {
        validate_fabric_id(fabric)?;
        crate::validate_node_name(node)?;
        let path = format!(
            "/cluster/sdn/fabrics/node/{fabric_id}/{node_id}",
            fabric_id = fabric,
            node_id = node
        );
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnFabricNode,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .sdn_fabric_nodes(fabric)?
                    .iter()
                    .any(|n| n.get("node_id").and_then(|v| v.as_str()) == Some(node)))
            }),
        )
    }

    /// The node-side index of a fabric (`GET /nodes/{node}/sdn/fabrics/{fabric}`):
    /// like [`Self::sdn_zone_node_index`], a fixed list of subdirectory names
    /// (`neighbors`, `routes`), answered for a fabric that is only staged too.
    pub fn sdn_fabric_node_index(&self, fabric: &str) -> Result<Vec<serde_json::Value>> {
        validate_fabric_id(fabric)?;
        let path = format!("/nodes/{}/sdn/fabrics/{fabric}", self.node);
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /nodes/{node}/sdn/fabrics/{fabric}")?;
        Ok(w.data)
    }

    /// The interfaces FRR runs the fabric on, on THIS node
    /// (`GET /nodes/{node}/sdn/fabrics/{fabric}/interfaces`). Measured: for a
    /// fabric that is only staged the node answers "does not exist in
    /// configuration" — these three read the RUNNING configuration, and only
    /// mean something after [`Self::apply_sdn`].
    pub fn sdn_fabric_interfaces(&self, fabric: &str) -> Result<Vec<serde_json::Value>> {
        validate_fabric_id(fabric)?;
        let path = format!("/nodes/{}/sdn/fabrics/{fabric}/interfaces", self.node);
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /nodes/{node}/sdn/fabrics/{fabric}/interfaces")?;
        Ok(w.data)
    }

    /// The fabric's routing neighbours seen from THIS node
    /// (`GET /nodes/{node}/sdn/fabrics/{fabric}/neighbors`). Running
    /// configuration only — see [`Self::sdn_fabric_interfaces`].
    pub fn sdn_fabric_neighbors(&self, fabric: &str) -> Result<Vec<serde_json::Value>> {
        validate_fabric_id(fabric)?;
        let path = format!("/nodes/{}/sdn/fabrics/{fabric}/neighbors", self.node);
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /nodes/{node}/sdn/fabrics/{fabric}/neighbors")?;
        Ok(w.data)
    }

    /// The routes the fabric installed on THIS node
    /// (`GET /nodes/{node}/sdn/fabrics/{fabric}/routes`). Running
    /// configuration only — see [`Self::sdn_fabric_interfaces`].
    pub fn sdn_fabric_routes(&self, fabric: &str) -> Result<Vec<serde_json::Value>> {
        validate_fabric_id(fabric)?;
        let path = format!("/nodes/{}/sdn/fabrics/{fabric}/routes", self.node);
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /nodes/{node}/sdn/fabrics/{fabric}/routes")?;
        Ok(w.data)
    }

    /// The vnets of a zone as THIS node runs them, each with a `status`
    /// (`GET /nodes/{node}/sdn/zones/{zone}/content`) — the route that
    /// answers "is this zone real here", which the directory index
    /// ([`Self::sdn_zone_node_index`]) does not.
    pub fn sdn_zone_content(&self, zone: &str) -> Result<Vec<serde_json::Value>> {
        validate_sdn_id(zone)?;
        let path = format!("/nodes/{}/sdn/zones/{zone}/content", self.node);
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /nodes/{node}/sdn/zones/{zone}/content")?;
        Ok(w.data)
    }

    // ----- Zones and subnets with the DHCP/IPAM/DNS fields -----------------

    /// [`Self::create_sdn_zone`] with the fields a zone needs to hand out
    /// addresses and register names: still `type=simple`, still STAGED.
    pub fn create_sdn_zone_with(
        &self,
        ledger: &Ledger,
        zone: &str,
        opts: &ZoneOptions<'_>,
    ) -> Result<()> {
        validate_sdn_id(zone)?;
        if let Some(i) = opts.ipam {
            validate_sdn_id(i)?;
        }
        if let Some(d) = opts.dns {
            validate_sdn_id(d)?;
        }
        let mut form: Vec<(&str, &str)> = vec![("zone", zone), ("type", "simple")];
        if opts.dhcp_dnsmasq {
            form.push(("dhcp", "dnsmasq"));
        }
        if let Some(i) = opts.ipam {
            form.push(("ipam", i));
        }
        if let Some(d) = opts.dns {
            form.push(("dns", d));
        }
        if let Some(z) = opts.dnszone {
            form.push(("dnszone", z));
        }
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::CreateSdnZone,
            || self.post_form("/cluster/sdn/zones", &form, true),
            Some(&|| {
                Ok(self
                    .sdn_zones()?
                    .iter()
                    .any(|z| z.get("zone").and_then(|v| v.as_str()) == Some(zone)))
            }),
        )
    }

    /// [`Self::create_sdn_subnet`] with DHCP ranges, the DHCP-advertised DNS
    /// server and `snat`. Returns the subnet's id, as the plain form does.
    pub fn create_sdn_subnet_with(
        &self,
        ledger: &Ledger,
        vnet: &str,
        zone: &str,
        cidr: &str,
        opts: &SubnetOptions<'_>,
    ) -> Result<String> {
        validate_sdn_id(vnet)?;
        validate_sdn_id(zone)?;
        validate_cidr(cidr)?;
        let subnet = sdn_subnet_id(zone, cidr);
        let encoded = subnet_option_fields(opts)?;
        let mut form: Vec<(&str, &str)> = vec![("subnet", cidr), ("type", "subnet")];
        form.extend(encoded.iter().map(|(k, v)| (*k, v.as_str())));
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
                    .any(|s| s.get("subnet").and_then(|v| v.as_str()) == Some(subnet.as_str())))
            }),
        )?;
        Ok(subnet)
    }

    /// [`Self::update_sdn_subnet`] for the same fields. The probe that
    /// settles a lost answer compares whichever of `gateway` and
    /// `dhcp_ranges` was given; with neither there is nothing to compare
    /// against and a lost answer stays an error.
    pub fn update_sdn_subnet_with(
        &self,
        ledger: &Ledger,
        vnet: &str,
        zone: &str,
        cidr: &str,
        opts: &SubnetOptions<'_>,
    ) -> Result<()> {
        validate_sdn_id(vnet)?;
        validate_sdn_id(zone)?;
        validate_cidr(cidr)?;
        let subnet = sdn_subnet_id(zone, cidr);
        let encoded = subnet_option_fields(opts)?;
        let form: Vec<(&str, &str)> = encoded.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let path = format!("/cluster/sdn/vnets/{vnet}/subnets/{subnet}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnSubnet,
            || self.put_form(&path, &form),
            Some(&|| {
                if opts.gateway.is_none() && opts.dhcp_ranges.is_empty() {
                    return Ok(false);
                }
                let obj = self.sdn_vnet_subnet(vnet, zone, cidr)?;
                let gateway_ok = opts
                    .gateway
                    .is_none_or(|g| obj.get("gateway").and_then(|v| v.as_str()) == Some(g));
                let ranges_ok =
                    opts.dhcp_ranges.is_empty() || dhcp_ranges_of(&obj) == opts.dhcp_ranges;
                Ok(gateway_ok && ranges_ok)
            }),
        )
    }

    // ----- IPAM allocations on a vnet ---------------------------------------

    /// Reserves `ip` in the vnet's subnet for `mac`
    /// (`POST /cluster/sdn/vnets/{vnet}/ips`).
    ///
    /// Acts on the IPAM database directly, against the RUNNING configuration
    /// — measured: with the subnet only staged the node answers "can't find
    /// any subnet for ip"; [`Self::apply_sdn`] first. The write answers
    /// `null` and forks no task (the same `task_or_done` reading as the other
    /// SDN writes); a lost answer is settled by [`Self::sdn_ipam_status`] of
    /// the `pve` IPAM listing the address.
    pub fn sdn_vnet_ip_add(
        &self,
        ledger: &Ledger,
        vnet: &str,
        zone: &str,
        ip: &str,
        mac: Option<&str>,
    ) -> Result<()> {
        validate_sdn_id(vnet)?;
        validate_sdn_id(zone)?;
        validate_ip("IPAM address", ip)?;
        if let Some(m) = mac {
            validate_mac(m)?;
        }
        let mut form: Vec<(&str, &str)> = vec![("zone", zone), ("ip", ip)];
        if let Some(m) = mac {
            form.push(("mac", m));
        }
        let path = format!("/cluster/sdn/vnets/{vnet}/ips");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::AddSdnIp,
            || self.post_form(&path, &form, true),
            Some(&|| Ok(self.ipam_holds(zone, ip)?.is_some())),
        )
    }

    /// Moves the reservation held by `mac` to `new_ip`
    /// (`PUT /cluster/sdn/vnets/{vnet}/ips`).
    ///
    /// The route's schema reads as "update the IP mapping" with `mac`
    /// optional; its handler (`PVE::API2::Network::SDN::Ips`, read on the
    /// node itself, `/usr/share/perl5/PVE/API2/Network/SDN/Ips.pm`) does
    /// something more specific: it looks up the address `mac` currently
    /// holds (`get_ips_from_mac`), releases THAT, and reserves `ip` for the
    /// same `mac` — restoring the old one if the new reservation fails. So
    /// the MAC is the key and the IP is what changes, and a call that names
    /// a MAC the IPAM does not know fails on the node with "can't find any
    /// subnet for ip " (an EMPTY ip — the old one it could not find).
    /// Measured: the first version of this crate's live case called it the
    /// other way round (same IP, new MAC) and got exactly that error. Hence
    /// `mac` is required here, not optional. `vmid` records which VM the
    /// address is for, as the node's own GUI does.
    pub fn sdn_vnet_ip_update(
        &self,
        ledger: &Ledger,
        vnet: &str,
        zone: &str,
        mac: &str,
        new_ip: &str,
        vmid: Option<u32>,
    ) -> Result<()> {
        validate_sdn_id(vnet)?;
        validate_sdn_id(zone)?;
        validate_mac(mac)?;
        validate_ip("IPAM address", new_ip)?;
        let vmid_text = vmid.map(|v| v.to_string());
        let mut form: Vec<(&str, &str)> = vec![("zone", zone), ("ip", new_ip), ("mac", mac)];
        if let Some(v) = &vmid_text {
            form.push(("vmid", v.as_str()));
        }
        let path = format!("/cluster/sdn/vnets/{vnet}/ips");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnIp,
            || self.put_form(&path, &form),
            Some(&|| {
                let Some(entry) = self.ipam_holds(zone, new_ip)? else {
                    return Ok(false);
                };
                Ok(entry
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .is_some_and(|got| got.eq_ignore_ascii_case(mac)))
            }),
        )
    }

    /// Releases `ip` (`DELETE /cluster/sdn/vnets/{vnet}/ips`).
    ///
    /// The parameters travel in the QUERY STRING: measured, a `DELETE` with
    /// a form body is refused by the node's proxy with "Unexpected content
    /// for method 'DELETE'" (501) before the handler ever runs.
    pub fn sdn_vnet_ip_delete(
        &self,
        ledger: &Ledger,
        vnet: &str,
        zone: &str,
        ip: &str,
        mac: Option<&str>,
    ) -> Result<()> {
        validate_sdn_id(vnet)?;
        validate_sdn_id(zone)?;
        validate_ip("IPAM address", ip)?;
        if let Some(m) = mac {
            validate_mac(m)?;
        }
        let mac_query = mac
            .map(|m| format!("&mac={}", crate::urlencode(m)))
            .unwrap_or_default();
        let path = format!("/cluster/sdn/vnets/{vnet}/ips?zone={zone}&ip={ip}{mac_query}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnIp,
            || self.delete(&path),
            Some(&|| Ok(self.ipam_holds(zone, ip)?.is_none())),
        )
    }

    /// The `pve` IPAM's entry for `ip` in `zone`, if it holds one.
    fn ipam_holds(&self, zone: &str, ip: &str) -> Result<Option<serde_json::Value>> {
        Ok(self.sdn_ipam_status("pve")?.into_iter().find(|e| {
            e.get("ip").and_then(|v| v.as_str()) == Some(ip)
                && e.get("zone")
                    .and_then(|v| v.as_str())
                    .is_none_or(|z| z == zone)
        }))
    }
}

/// The form fields of a [`SubnetOptions`], validated, with each DHCP range
/// encoded as its own `dhcp-range` field.
fn subnet_option_fields(opts: &SubnetOptions<'_>) -> Result<Vec<(&'static str, String)>> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    if let Some(g) = opts.gateway {
        validate_ip("subnet gateway", g)?;
        out.push(("gateway", g.to_string()));
    }
    for r in opts.dhcp_ranges {
        validate_ip("DHCP range start", &r.start)?;
        validate_ip("DHCP range end", &r.end)?;
        out.push(("dhcp-range", r.encoded()));
    }
    if let Some(d) = opts.dhcp_dns_server {
        validate_ip("DHCP DNS server", d)?;
        out.push(("dhcp-dns-server", d.to_string()));
    }
    if let Some(s) = opts.snat {
        out.push(("snat", if s { "1" } else { "0" }.to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{
        sdn_subnet_id, subnet_option_fields, validate_cidr, validate_fabric_id, validate_ip,
        validate_mac, validate_sdn_id, DhcpRange, SubnetOptions,
    };

    #[test]
    fn a_fabric_id_follows_pve_sdn_fabric_id() {
        for id in ["f1", "fab-1", "A1234567", "1a", "x-y"] {
            assert!(validate_fabric_id(id).is_ok(), "{id} should be accepted");
        }
        for id in ["", "f", "-fab", "fab-", "toolong123", "fa_b", "fa b"] {
            assert!(validate_fabric_id(id).is_err(), "{id:?} should be refused");
        }
        let e = validate_fabric_id("bad id").unwrap_err().to_string();
        assert!(e.contains("bad id"), "{e}");
    }

    #[test]
    fn an_address_is_an_address_and_a_mac_is_a_mac() {
        assert!(validate_ip("x", "10.0.0.1").is_ok());
        assert!(validate_ip("x", "fd00::1").is_ok());
        for bad in ["", "10.0.0.0/24", "10.0.0", "host", "10.0.0.1 "] {
            let e = validate_ip("DHCP DNS server", bad).unwrap_err().to_string();
            assert!(e.contains("DHCP DNS server") && e.contains(bad), "{e}");
        }
        assert!(validate_mac("BC:24:11:00:00:01").is_ok());
        assert!(validate_mac("bc:24:11:ab:cd:ef").is_ok());
        for bad in [
            "",
            "BC:24:11:00:00",
            "BC-24-11-00-00-01",
            "BC:24:11:00:00:0G",
            "BC:24:11:00:00:001",
        ] {
            assert!(validate_mac(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_dhcp_range_is_sent_as_the_property_string_the_node_accepts() {
        let ranges = [
            DhcpRange {
                start: "10.0.0.100".into(),
                end: "10.0.0.150".into(),
            },
            DhcpRange {
                start: "10.0.0.200".into(),
                end: "10.0.0.210".into(),
            },
        ];
        let fields = subnet_option_fields(&SubnetOptions {
            gateway: Some("10.0.0.1"),
            dhcp_ranges: &ranges,
            dhcp_dns_server: Some("10.0.0.1"),
            snat: Some(true),
        })
        .expect("valid options");
        assert_eq!(
            fields,
            vec![
                ("gateway", "10.0.0.1".to_string()),
                (
                    "dhcp-range",
                    "start-address=10.0.0.100,end-address=10.0.0.150".to_string()
                ),
                (
                    "dhcp-range",
                    "start-address=10.0.0.200,end-address=10.0.0.210".to_string()
                ),
                ("dhcp-dns-server", "10.0.0.1".to_string()),
                ("snat", "1".to_string()),
            ]
        );
        // A range end that is not an address is refused before any request.
        let bad = [DhcpRange {
            start: "10.0.0.100".into(),
            end: "nope".into(),
        }];
        let e = subnet_option_fields(&SubnetOptions {
            dhcp_ranges: &bad,
            ..Default::default()
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("DHCP range end") && e.contains("nope"), "{e}");
    }

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
