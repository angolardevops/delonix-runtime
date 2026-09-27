//! The routing layer of the Proxmox cluster's OWN SDN (not `delonix-sdn`):
//! BGP/EVPN controllers, prefix lists and route maps — and the firewall a
//! vnet puts on traffic forwarded through it.
//!
//! Every controller, prefix-list and route-map write here is STAGED, the same
//! as a zone ([`crate::sdn`]'s doc comment): nothing reaches a node's FRR
//! until an apply, and [`crate::Client::sdn_dry_run`] shows what it would
//! write. The three are one chain on the node: a prefix list is matched by a
//! route-map entry (`match ip-address-prefix-list=<id>`), and a route map is
//! hung on a controller (`route-map-in`/`route-map-out`) — measured on a live
//! PVE 9.2.2 node, an EVPN controller's `route-map-in=<rm>` renders as
//! `route-map MAP_VTEP_IN permit 1 / call <rm>` in `frr.conf`. The node
//! checks the chain when it stages the controller ("route map … does not
//! exist!"), so the prefix list and the route map go first.
//!
//! **The vnet firewall is not staged.** `…/vnets/{vnet}/firewall/*` writes
//! `/etc/pve/sdn/firewall/<vnet>.fw` at once, answers only for a vnet in the
//! RUNNING configuration ("invalid vnet specified" for a staged one — the
//! handler looks the vnet up with `running => 1`), and takes only `forward`
//! rules (the node refuses `in`/`out`: "invalid rule type 'in' for rule_env
//! 'vnet'"). Measured too: a rule is inserted at the TOP of the list, as on a
//! VM's own firewall, and ENFORCING it is the nftables backend's job
//! (`proxmox-firewall`, turned on per node by the host firewall option
//! `nftables: 1`) — on a node running the legacy iptables `pve-firewall`, the
//! rules are stored and read back and nothing compiles them. Reading them back
//! is what this client can prove; that the node filters with them is not.
//!
//! # What is offered, and what is not
//!
//! Two controller types: `evpn` (the one an EVPN zone needs) and `bgp` (one
//! per node, to peer the node with an external router). `isis` needs physical
//! interfaces to run on and `faucet` has no plugin logic in 9.2.2 — neither is
//! offered, because neither was exercised.

use crate::sdn::{validate_cidr, validate_fabric_id, validate_ip, validate_sdn_id, SDN_VMID};
use crate::{parse, Client, Error, FirewallRuleOpts, Ledger, Result, TaskKind, Wrapped};
use std::net::Ipv4Addr;

fn invalid(msg: String) -> Error {
    Error::InvalidSdnRouting(msg)
}

/// A controller id (`POST /cluster/sdn/controllers`'s `controller`): a letter,
/// then letters, digits, `_` or `-`, ending in a letter or digit — 2 to 64
/// characters (the schema's own pattern and `maxLength`).
pub fn validate_controller_id(id: &str) -> Result<()> {
    let b = id.as_bytes();
    let ok = (2..=64).contains(&b.len())
        && b[0].is_ascii_alphabetic()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-');
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "invalid Proxmox SDN controller id '{id}': expected a letter, then letters, digits, \
             '_' or '-', ending in a letter or digit (2 to 64 characters)"
        )))
    }
}

/// A prefix-list or route-map id. Both formats are the same regular
/// expression on the node (`PVE::Network::SDN::PrefixLists`/`::RouteMaps`,
/// read on the node: `^[a-zA-Z0-9][a-zA-Z0-9-_]{0,30}[a-zA-Z0-9]?$`, 1 to 32
/// characters), each with its own reserved names — the ones the node's own
/// generated FRR configuration uses.
fn validate_list_id(what: &str, id: &str, reserved: &dyn Fn(&str) -> bool) -> Result<()> {
    let b = id.as_bytes();
    let shape = (1..=32).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_');
    if !shape {
        return Err(invalid(format!(
            "invalid Proxmox SDN {what} id '{id}': expected 1 to 32 letters, digits, '-' or '_', \
             starting with a letter or digit"
        )));
    }
    if reserved(id) {
        return Err(invalid(format!(
            "invalid Proxmox SDN {what} id '{id}': the node reserves it for its own FRR \
             configuration"
        )));
    }
    Ok(())
}

/// A prefix-list id — see [`validate_list_id`]; `only_default`,
/// `only_default_v6` and `loopbacks_ips` are reserved.
pub fn validate_prefix_list_id(id: &str) -> Result<()> {
    validate_list_id("prefix list", id, &|i| {
        matches!(i, "only_default" | "only_default_v6" | "loopbacks_ips")
    })
}

/// A route-map id — see [`validate_list_id`]; `pve_*`, `MAP_VTEP_IN`,
/// `MAP_VTEP_OUT` and `correct_src` are reserved.
pub fn validate_route_map_id(id: &str) -> Result<()> {
    validate_list_id("route map", id, &|i| {
        i.starts_with("pve_") || matches!(i, "MAP_VTEP_IN" | "MAP_VTEP_OUT" | "correct_src")
    })
}

/// Either kind of routing list, by name — what the re-export offers a
/// caller that holds the kind as data.
pub fn validate_routing_list_id(kind: &str, id: &str) -> Result<()> {
    match kind {
        "prefix-list" => validate_prefix_list_id(id),
        "route-map" => validate_route_map_id(id),
        other => Err(invalid(format!(
            "unknown Proxmox SDN routing list kind '{other}': expected 'prefix-list' or 'route-map'"
        ))),
    }
}

/// The controller types this client stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControllerKind {
    /// The BGP-EVPN control plane an EVPN zone runs on: an ASN and exactly one
    /// of `peers`/`fabric` (the plugin's own `on_update_hook` refuses both or
    /// neither).
    Evpn,
    /// A per-node BGP session to an external router: `node`, an ASN and
    /// `peers`, all required; one per node.
    Bgp,
}

impl ControllerKind {
    fn as_str(self) -> &'static str {
        match self {
            ControllerKind::Evpn => "evpn",
            ControllerKind::Bgp => "bgp",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "evpn" => Some(ControllerKind::Evpn),
            "bgp" => Some(ControllerKind::Bgp),
            _ => None,
        }
    }
}

/// What a controller is created or updated with. On an update, `None` (and an
/// empty `peers`) means "leave it as the node has it".
#[derive(Clone, Copy, Debug, Default)]
pub struct ControllerOptions<'a> {
    pub asn: Option<u32>,
    /// The BGP peers' addresses — sent as the comma list `peers` is.
    pub peers: &'a [&'a str],
    /// EVPN only: the fabric that carries the underlay, instead of `peers`.
    pub fabric: Option<&'a str>,
    /// BGP only: the node the session runs on.
    pub node: Option<&'a str>,
    pub route_map_in: Option<&'a str>,
    pub route_map_out: Option<&'a str>,
    /// BGP only.
    pub ebgp: Option<bool>,
    pub ebgp_multihop: Option<u32>,
    /// BGP only: the dummy interface whose address is the router id.
    pub loopback: Option<&'a str>,
    /// BGP only.
    pub bgp_multipath_as_path_relax: Option<bool>,
    /// EVPN only (default `VTEP`).
    pub peer_group_name: Option<&'a str>,
}

fn validate_iface(what: &str, name: &str) -> Result<()> {
    let ok = (1..=15).contains(&name.len())
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "invalid Proxmox SDN {what} '{name}': expected an interface name (1 to 15 letters, \
             digits, '_', '-' or '.')"
        )))
    }
}

/// The form fields of a controller create/update, validated. Pure, so the
/// per-type rules are tests: `create` enforces what the type REQUIRES; both
/// refuse a field the type does not have (the plugin's own `options` list),
/// which the node would otherwise refuse with its own words.
pub(crate) fn controller_fields(
    kind: ControllerKind,
    opts: &ControllerOptions<'_>,
    create: bool,
) -> Result<Vec<(&'static str, String)>> {
    let bgp_only = [
        ("node", opts.node.is_some()),
        ("ebgp", opts.ebgp.is_some()),
        ("loopback", opts.loopback.is_some()),
        (
            "bgp-multipath-as-path-relax",
            opts.bgp_multipath_as_path_relax.is_some(),
        ),
    ];
    let evpn_only = [
        ("fabric", opts.fabric.is_some()),
        ("peer-group-name", opts.peer_group_name.is_some()),
    ];
    let foreign: Vec<&str> = match kind {
        ControllerKind::Evpn => bgp_only
            .iter()
            .filter(|(_, s)| *s)
            .map(|(k, _)| *k)
            .collect(),
        ControllerKind::Bgp => evpn_only
            .iter()
            .filter(|(_, s)| *s)
            .map(|(k, _)| *k)
            .collect(),
    };
    if !foreign.is_empty() {
        return Err(invalid(format!(
            "a Proxmox SDN {} controller has no {}",
            kind.as_str(),
            foreign.join(", ")
        )));
    }
    if create {
        let missing: Vec<&str> = match kind {
            ControllerKind::Evpn => {
                let mut m = Vec::new();
                if opts.asn.is_none() {
                    m.push("asn");
                }
                if opts.peers.is_empty() == opts.fabric.is_none() {
                    return Err(invalid(
                        "a Proxmox SDN evpn controller needs exactly one of peers or fabric \
                         (the node refuses both, and neither)"
                            .into(),
                    ));
                }
                m
            }
            ControllerKind::Bgp => [
                ("node", opts.node.is_none()),
                ("asn", opts.asn.is_none()),
                ("peers", opts.peers.is_empty()),
            ]
            .iter()
            .filter(|(_, m)| *m)
            .map(|(k, _)| *k)
            .collect(),
        };
        if !missing.is_empty() {
            return Err(invalid(format!(
                "a Proxmox SDN {} controller needs {}",
                kind.as_str(),
                missing.join(", ")
            )));
        }
    }
    for p in opts.peers {
        validate_ip("controller peer", p)?;
    }
    if let Some(f) = opts.fabric {
        validate_fabric_id(f)?;
    }
    if let Some(n) = opts.node {
        crate::validate_node_name(n)?;
    }
    if let Some(r) = opts.route_map_in {
        validate_route_map_id(r)?;
    }
    if let Some(r) = opts.route_map_out {
        validate_route_map_id(r)?;
    }
    if let Some(l) = opts.loopback {
        validate_iface("controller loopback", l)?;
    }
    if let Some(g) = opts.peer_group_name {
        validate_controller_id(g).map_err(|_| {
            invalid(format!(
                "invalid Proxmox SDN peer group name '{g}': expected a letter, then letters, \
                 digits, '_' or '-'"
            ))
        })?;
    }
    let flag = |b: bool| if b { "1" } else { "0" }.to_string();
    let mut out: Vec<(&'static str, String)> = Vec::new();
    if let Some(a) = opts.asn {
        out.push(("asn", a.to_string()));
    }
    if !opts.peers.is_empty() {
        out.push(("peers", opts.peers.join(",")));
    }
    let mut push = |k: &'static str, v: Option<String>| {
        if let Some(v) = v {
            out.push((k, v));
        }
    };
    push("fabric", opts.fabric.map(str::to_string));
    push("node", opts.node.map(str::to_string));
    push("route-map-in", opts.route_map_in.map(str::to_string));
    push("route-map-out", opts.route_map_out.map(str::to_string));
    push("ebgp", opts.ebgp.map(flag));
    push("ebgp-multihop", opts.ebgp_multihop.map(|h| h.to_string()));
    push("loopback", opts.loopback.map(str::to_string));
    push(
        "bgp-multipath-as-path-relax",
        opts.bgp_multipath_as_path_relax.map(flag),
    );
    push("peer-group-name", opts.peer_group_name.map(str::to_string));
    Ok(out)
}

/// `permit` or `deny` — the verdict of a prefix-list or route-map entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutingAction {
    Permit,
    Deny,
}

impl RoutingAction {
    fn as_str(self) -> &'static str {
        match self {
            RoutingAction::Permit => "permit",
            RoutingAction::Deny => "deny",
        }
    }
}

/// One entry of a prefix list. `seq` omitted: the node numbers it (measured —
/// the first entry got 5, FRR's own step).
#[derive(Clone, Copy, Debug)]
pub struct PrefixListEntry<'a> {
    pub action: RoutingAction,
    /// The network matched, as a CIDR (`0.0.0.0/0` included).
    pub prefix: &'a str,
    /// Match prefixes at least this long …
    pub ge: Option<u8>,
    /// … and at most this long.
    pub le: Option<u8>,
    pub seq: Option<u32>,
}

/// What an entry update changes; `None` leaves the field as it is, and
/// `delete` names fields to clear (`ge`, `le`).
#[derive(Clone, Copy, Debug, Default)]
pub struct PrefixListEntryUpdate<'a> {
    pub action: Option<RoutingAction>,
    pub prefix: Option<&'a str>,
    pub ge: Option<u8>,
    pub le: Option<u8>,
    pub delete: &'a [&'a str],
}

/// The prefix length of an already validated CIDR, and its family's maximum.
fn prefix_bounds(cidr: &str) -> (u8, u8) {
    let (addr, len) = cidr.split_once('/').unwrap_or((cidr, "0"));
    let max = if addr.parse::<Ipv4Addr>().is_ok() {
        32
    } else {
        128
    };
    (len.parse().unwrap_or(0), max)
}

/// FRR's own rule for a prefix-list range: `len < ge <= le <= max` (and
/// `len < le`). A range the node stages and FRR then refuses at apply time
/// fails the whole reload — refused here instead.
fn validate_range(prefix: &str, ge: Option<u8>, le: Option<u8>) -> Result<()> {
    validate_cidr(prefix).map_err(|_| {
        invalid(format!(
            "invalid Proxmox SDN prefix-list prefix '{prefix}': expected a CIDR such as \
             '10.0.0.0/16' or '0.0.0.0/0'"
        ))
    })?;
    let (len, max) = prefix_bounds(prefix);
    let bad = |what: &str, v: u8| {
        invalid(format!(
            "invalid Proxmox SDN prefix-list range for {prefix}: {what} {v} — FRR needs \
             prefix length < ge <= le <= {max}"
        ))
    };
    if let Some(g) = ge {
        if g <= len || g > max {
            return Err(bad("ge", g));
        }
    }
    if let Some(l) = le {
        if l <= len || l > max || ge.is_some_and(|g| g > l) {
            return Err(bad("le", l));
        }
    }
    Ok(())
}

impl PrefixListEntry<'_> {
    fn validate(&self) -> Result<()> {
        validate_range(self.prefix, self.ge, self.le)?;
        if self.seq == Some(0) {
            return Err(invalid(
                "invalid Proxmox SDN prefix-list seq 0: the node numbers entries from 1".into(),
            ));
        }
        Ok(())
    }

    /// The property string an entry travels as inside a list create/update
    /// (`entries=action=permit,prefix=…`), as the node itself echoes it.
    fn encoded(&self) -> String {
        let mut s = format!("action={},prefix={}", self.action.as_str(), self.prefix);
        if let Some(g) = self.ge {
            s.push_str(&format!(",ge={g}"));
        }
        if let Some(l) = self.le {
            s.push_str(&format!(",le={l}"));
        }
        if let Some(q) = self.seq {
            s.push_str(&format!(",seq={q}"));
        }
        s
    }

    fn fields(&self) -> Vec<(&'static str, String)> {
        let mut f = vec![
            ("action", self.action.as_str().to_string()),
            ("prefix", self.prefix.to_string()),
        ];
        if let Some(g) = self.ge {
            f.push(("ge", g.to_string()));
        }
        if let Some(l) = self.le {
            f.push(("le", l.to_string()));
        }
        if let Some(q) = self.seq {
            f.push(("seq", q.to_string()));
        }
        f
    }
}

/// The keys a route-map `match` may name (the node's `ROUTE_MAP_MATCH_FORMAT`).
const MATCH_KEYS: &[&str] = &[
    "route-type",
    "vni",
    "ip-address-prefix-list",
    "ip6-address-prefix-list",
    "ip-next-hop-prefix-list",
    "ip6-next-hop-prefix-list",
    "ip-next-hop-address",
    "ip6-next-hop-address",
    "metric",
    "local-preference",
    "peer",
    "tag",
];

/// The keys a route-map `set` may name.
const SET_KEYS: &[&str] = &[
    "ip-next-hop-peer-address",
    "ip-next-hop",
    "ip-next-hop-unchanged",
    "ip6-next-hop-peer-address",
    "ip6-next-hop-prefer-global",
    "ip6-next-hop",
    "local-preference",
    "tag",
    "weight",
    "metric",
    "src",
];

/// One `match` or `set` clause of a route-map entry: `key=<key>[,value=<v>]`
/// on the wire.
#[derive(Clone, Copy, Debug)]
pub struct RouteMapClause<'a> {
    pub key: &'a str,
    pub value: Option<&'a str>,
}

impl RouteMapClause<'_> {
    fn encoded(&self, allowed: &[&str], what: &str) -> Result<String> {
        if !allowed.contains(&self.key) {
            return Err(invalid(format!(
                "unknown Proxmox SDN route-map {what} key '{}': expected one of {}",
                self.key,
                allowed.join(", ")
            )));
        }
        match self.value {
            None => Ok(format!("key={}", self.key)),
            Some(v) if v.is_empty() || v.contains([',', '=', '\n']) => Err(invalid(format!(
                "invalid Proxmox SDN route-map {what} value '{v}' for '{}': it travels inside a \
                 property string, so it cannot be empty or carry ',', '=' or a newline",
                self.key
            ))),
            Some(v) => Ok(format!("key={},value={v}", self.key)),
        }
    }
}

/// How an entry that matched continues: `on-match-next`, `continue`, or
/// `on-match-goto` to another entry's order.
fn exit_action_encoded(exit: (&str, Option<u16>)) -> Result<String> {
    match exit {
        ("on-match-next", None) | ("continue", None) => Ok(format!("key={}", exit.0)),
        ("on-match-goto", Some(o)) | ("continue", Some(o)) => {
            Ok(format!("key={},value={o}", exit.0))
        }
        (k, v) => Err(invalid(format!(
            "invalid Proxmox SDN route-map exit action '{k}'{}: expected 'on-match-next', \
             'continue' (optionally with an order) or 'on-match-goto' with an order",
            v.map(|o| format!(" {o}")).unwrap_or_default()
        ))),
    }
}

/// One entry of a route map — the map itself exists while it has an entry.
#[derive(Clone, Copy, Debug)]
pub struct RouteMapEntry<'a> {
    pub action: RoutingAction,
    pub matches: &'a [RouteMapClause<'a>],
    pub sets: &'a [RouteMapClause<'a>],
    /// Another route map to call when this entry matches.
    pub call: Option<&'a str>,
    pub exit_action: Option<(&'a str, Option<u16>)>,
}

/// What an entry update changes. `Some` replaces the whole `matches`/`sets`
/// list; `delete` names fields to clear (`match`, `set`, `call`,
/// `exit-action`).
#[derive(Clone, Copy, Debug, Default)]
pub struct RouteMapEntryUpdate<'a> {
    pub action: Option<RoutingAction>,
    pub matches: Option<&'a [RouteMapClause<'a>]>,
    pub sets: Option<&'a [RouteMapClause<'a>]>,
    pub call: Option<&'a str>,
    pub exit_action: Option<(&'a str, Option<u16>)>,
    pub delete: &'a [&'a str],
}

fn clause_fields(
    out: &mut Vec<(&'static str, String)>,
    matches: Option<&[RouteMapClause<'_>]>,
    sets: Option<&[RouteMapClause<'_>]>,
    call: Option<&str>,
    exit: Option<(&str, Option<u16>)>,
) -> Result<()> {
    for m in matches.unwrap_or_default() {
        out.push(("match", m.encoded(MATCH_KEYS, "match")?));
    }
    for s in sets.unwrap_or_default() {
        out.push(("set", s.encoded(SET_KEYS, "set")?));
    }
    if let Some(c) = call {
        validate_route_map_id(c)?;
        out.push(("call", c.to_string()));
    }
    if let Some(e) = exit {
        out.push(("exit-action", exit_action_encoded(e)?));
    }
    Ok(())
}

fn validate_delete(what: &str, delete: &[&str], allowed: &[&str]) -> Result<()> {
    for d in delete {
        if !allowed.contains(d) {
            return Err(invalid(format!(
                "cannot clear '{d}' on a Proxmox SDN {what}: expected one of {}",
                allowed.join(", ")
            )));
        }
    }
    Ok(())
}

/// A vnet firewall's options. `None` leaves a field as the node has it;
/// `delete` clears (`enable`, `policy_forward`, `log_level_forward`).
#[derive(Clone, Copy, Debug, Default)]
pub struct VnetFirewallOptions<'a> {
    pub enable: Option<bool>,
    /// `ACCEPT` or `DROP` — what happens to forwarded traffic no rule matched.
    pub policy_forward: Option<&'a str>,
    pub log_level_forward: Option<&'a str>,
    pub delete: &'a [&'a str],
}

const LOG_LEVELS: &[&str] = &[
    "emerg", "alert", "crit", "err", "warning", "notice", "info", "debug", "nolog",
];

fn vnet_rule_invalid(msg: String) -> Error {
    Error::InvalidVnetFirewallRule(msg)
}

fn validate_vnet_rule(rule_type: Option<&str>, action: Option<&str>) -> Result<()> {
    if let Some(t) = rule_type {
        if t != "forward" {
            return Err(vnet_rule_invalid(format!(
                "invalid Proxmox vnet firewall rule type '{t}': a vnet's firewall only takes \
                 'forward' rules (the node refuses 'in' and 'out' there)"
            )));
        }
    }
    if let Some(a) = action {
        if !matches!(a, "ACCEPT" | "DROP" | "REJECT") {
            return Err(vnet_rule_invalid(format!(
                "invalid Proxmox vnet firewall rule action '{a}': expected ACCEPT, DROP or REJECT"
            )));
        }
    }
    Ok(())
}

fn has_id(items: &[serde_json::Value], key: &str, id: &str) -> bool {
    items
        .iter()
        .any(|i| i.get(key).and_then(|v| v.as_str()) == Some(id))
}

impl Client {
    // ----- Controllers ------------------------------------------------------

    /// Every controller of the PENDING configuration
    /// (`GET /cluster/sdn/controllers`), or the RUNNING one with `running`.
    pub fn sdn_controllers(&self, running: bool) -> Result<Vec<serde_json::Value>> {
        let body = if running {
            self.get("/cluster/sdn/controllers?running=1")?
        } else {
            self.get("/cluster/sdn/controllers")?
        };
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "/cluster/sdn/controllers")?;
        Ok(w.data)
    }

    /// One controller (`GET /cluster/sdn/controllers/{controller}`).
    pub fn sdn_controller(&self, controller: &str) -> Result<serde_json::Value> {
        validate_controller_id(controller)?;
        let path = format!("/cluster/sdn/controllers/{controller}");
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> =
            parse(&body, "GET /cluster/sdn/controllers/{controller}")?;
        Ok(w.data)
    }

    /// Stages a controller (`POST /cluster/sdn/controllers`). The node checks
    /// a `route-map-in`/`route-map-out` against the pending route maps here,
    /// so they are staged first.
    pub fn create_sdn_controller(
        &self,
        ledger: &Ledger,
        controller: &str,
        kind: ControllerKind,
        opts: &ControllerOptions<'_>,
    ) -> Result<()> {
        validate_controller_id(controller)?;
        let mut fields = vec![
            ("controller", controller.to_string()),
            ("type", kind.as_str().to_string()),
        ];
        fields.extend(controller_fields(kind, opts, true)?);
        let form: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::CreateSdnController,
            || self.post_form("/cluster/sdn/controllers", &form, true),
            Some(&|| {
                Ok(has_id(
                    &self.sdn_controllers(false)?,
                    "controller",
                    controller,
                ))
            }),
        )
    }

    /// Stages a change to a controller
    /// (`PUT /cluster/sdn/controllers/{controller}`). Its type is read from the
    /// node first — a field the type does not have is refused before the PUT.
    /// `delete` names fields to clear.
    pub fn update_sdn_controller(
        &self,
        ledger: &Ledger,
        controller: &str,
        opts: &ControllerOptions<'_>,
        delete: &[&str],
    ) -> Result<()> {
        let current = self.sdn_controller(controller)?;
        let kind = current
            .get("type")
            .and_then(|v| v.as_str())
            .and_then(ControllerKind::parse)
            .ok_or_else(|| {
                invalid(format!(
                    "Proxmox SDN controller '{controller}' is of a type this client does not \
                     edit ({}); only evpn and bgp are",
                    current
                        .get("type")
                        .map(|t| t.to_string())
                        .unwrap_or_default()
                ))
            })?;
        validate_delete(
            "controller",
            delete,
            &[
                "peers",
                "fabric",
                "route-map-in",
                "route-map-out",
                "ebgp",
                "ebgp-multihop",
                "loopback",
                "bgp-multipath-as-path-relax",
                "peer-group-name",
            ],
        )?;
        let mut fields = controller_fields(kind, opts, false)?;
        let joined = delete.join(",");
        if !delete.is_empty() {
            fields.push(("delete", joined));
        }
        let form: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let path = format!("/cluster/sdn/controllers/{controller}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnController,
            || self.put_form(&path, &form),
            Some(&|| {
                let obj = self.sdn_controller(controller)?;
                Ok(opts.asn.is_none_or(|a| {
                    obj.get("asn").and_then(serde_json::Value::as_u64) == Some(u64::from(a))
                }))
            }),
        )
    }

    /// Removes a controller from the pending configuration
    /// (`DELETE /cluster/sdn/controllers/{controller}`). The node refuses it
    /// while a zone still uses it.
    pub fn delete_sdn_controller(&self, ledger: &Ledger, controller: &str) -> Result<()> {
        validate_controller_id(controller)?;
        let path = format!("/cluster/sdn/controllers/{controller}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnController,
            || self.delete(&path),
            Some(&|| {
                Ok(!has_id(
                    &self.sdn_controllers(false)?,
                    "controller",
                    controller,
                ))
            }),
        )
    }

    // ----- Prefix lists -----------------------------------------------------

    /// Every prefix list with its entries (`GET /cluster/sdn/prefix-lists?verbose=1`).
    pub fn sdn_prefix_lists(&self) -> Result<Vec<serde_json::Value>> {
        let body = self.get("/cluster/sdn/prefix-lists?verbose=1")?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "/cluster/sdn/prefix-lists")?;
        Ok(w.data)
    }

    /// One prefix list (`GET /cluster/sdn/prefix-lists/{id}`), its entries as
    /// the property strings the node stores.
    pub fn sdn_prefix_list(&self, id: &str) -> Result<serde_json::Value> {
        validate_prefix_list_id(id)?;
        let path = format!("/cluster/sdn/prefix-lists/{id}");
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> = parse(&body, "GET /cluster/sdn/prefix-lists/{id}")?;
        Ok(w.data)
    }

    /// Stages a prefix list with its first entries
    /// (`POST /cluster/sdn/prefix-lists`, one `entries` field per entry).
    pub fn create_sdn_prefix_list(
        &self,
        ledger: &Ledger,
        id: &str,
        entries: &[PrefixListEntry<'_>],
    ) -> Result<()> {
        validate_prefix_list_id(id)?;
        let encoded: Vec<String> = entries
            .iter()
            .map(|e| e.validate().map(|()| e.encoded()))
            .collect::<Result<_>>()?;
        let mut form: Vec<(&str, &str)> = vec![("id", id)];
        form.extend(encoded.iter().map(|e| ("entries", e.as_str())));
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::CreateSdnPrefixList,
            || self.post_form("/cluster/sdn/prefix-lists", &form, true),
            Some(&|| Ok(self.sdn_prefix_list(id).is_ok())),
        )
    }

    /// Replaces a prefix list's entries (`PUT /cluster/sdn/prefix-lists/{id}`
    /// with `entries`) — measured: the list becomes exactly what is sent, the
    /// entries not sent are gone.
    pub fn update_sdn_prefix_list(
        &self,
        ledger: &Ledger,
        id: &str,
        entries: &[PrefixListEntry<'_>],
    ) -> Result<()> {
        validate_prefix_list_id(id)?;
        let encoded: Vec<String> = entries
            .iter()
            .map(|e| e.validate().map(|()| e.encoded()))
            .collect::<Result<_>>()?;
        let form: Vec<(&str, &str)> = encoded.iter().map(|e| ("entries", e.as_str())).collect();
        let path = format!("/cluster/sdn/prefix-lists/{id}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnPrefixList,
            || self.put_form(&path, &form),
            None,
        )
    }

    /// Removes a prefix list (`DELETE /cluster/sdn/prefix-lists/{id}`). The
    /// node refuses it while a route-map entry still matches on it.
    pub fn delete_sdn_prefix_list(&self, ledger: &Ledger, id: &str) -> Result<()> {
        validate_prefix_list_id(id)?;
        let path = format!("/cluster/sdn/prefix-lists/{id}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnPrefixList,
            || self.delete(&path),
            Some(&|| Ok(!has_id(&self.sdn_prefix_lists()?, "id", id))),
        )
    }

    /// The entries of a prefix list (`GET /cluster/sdn/prefix-lists/{id}/entries`).
    pub fn sdn_prefix_list_entries(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        validate_prefix_list_id(id)?;
        let path = format!("/cluster/sdn/prefix-lists/{id}/entries");
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /cluster/sdn/prefix-lists/{id}/entries")?;
        Ok(w.data)
    }

    /// One entry (`GET /cluster/sdn/prefix-lists/{id}/entries/{url_seq}`).
    pub fn sdn_prefix_list_entry(&self, id: &str, seq: u32) -> Result<serde_json::Value> {
        validate_prefix_list_id(id)?;
        let path = format!(
            "/cluster/sdn/prefix-lists/{id}/entries/{url_seq}",
            url_seq = seq
        );
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> = parse(
            &body,
            "GET /cluster/sdn/prefix-lists/{id}/entries/{url_seq}",
        )?;
        Ok(w.data)
    }

    /// Adds one entry (`POST /cluster/sdn/prefix-lists/{id}/entries`).
    pub fn add_sdn_prefix_list_entry(
        &self,
        ledger: &Ledger,
        id: &str,
        entry: &PrefixListEntry<'_>,
    ) -> Result<()> {
        validate_prefix_list_id(id)?;
        entry.validate()?;
        let fields = entry.fields();
        let form: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let path = format!("/cluster/sdn/prefix-lists/{id}/entries");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::AddSdnPrefixListEntry,
            || self.post_form(&path, &form, true),
            None,
        )
    }

    /// Changes one entry (`PUT /cluster/sdn/prefix-lists/{id}/entries/{url_seq}`).
    /// The range is checked against the entry's CURRENT prefix when the update
    /// does not change it — read from the node first.
    pub fn update_sdn_prefix_list_entry(
        &self,
        ledger: &Ledger,
        id: &str,
        seq: u32,
        update: &PrefixListEntryUpdate<'_>,
    ) -> Result<()> {
        validate_prefix_list_id(id)?;
        validate_delete("prefix-list entry", update.delete, &["ge", "le"])?;
        if update.ge.is_some() || update.le.is_some() || update.prefix.is_some() {
            let current = self.sdn_prefix_list_entry(id, seq)?;
            let prefix = update
                .prefix
                .map(str::to_string)
                .or_else(|| {
                    current
                        .get("prefix")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let kept = |k: &str, new: Option<u8>| {
                new.or_else(|| {
                    (!update.delete.contains(&k))
                        .then(|| current.get(k).and_then(serde_json::Value::as_u64))
                        .flatten()
                        .and_then(|v| u8::try_from(v).ok())
                })
            };
            validate_range(&prefix, kept("ge", update.ge), kept("le", update.le))?;
        }
        let mut fields: Vec<(&'static str, String)> = Vec::new();
        if let Some(a) = update.action {
            fields.push(("action", a.as_str().to_string()));
        }
        if let Some(p) = update.prefix {
            fields.push(("prefix", p.to_string()));
        }
        if let Some(g) = update.ge {
            fields.push(("ge", g.to_string()));
        }
        if let Some(l) = update.le {
            fields.push(("le", l.to_string()));
        }
        for d in update.delete {
            fields.push(("delete", d.to_string()));
        }
        let form: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let path = format!(
            "/cluster/sdn/prefix-lists/{id}/entries/{url_seq}",
            url_seq = seq
        );
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnPrefixListEntry,
            || self.put_form(&path, &form),
            None,
        )
    }

    /// Removes one entry (`DELETE /cluster/sdn/prefix-lists/{id}/entries/{url_seq}`).
    pub fn delete_sdn_prefix_list_entry(&self, ledger: &Ledger, id: &str, seq: u32) -> Result<()> {
        validate_prefix_list_id(id)?;
        let path = format!(
            "/cluster/sdn/prefix-lists/{id}/entries/{url_seq}",
            url_seq = seq
        );
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnPrefixListEntry,
            || self.delete(&path),
            Some(&|| {
                Ok(!self.sdn_prefix_list_entries(id)?.iter().any(|e| {
                    e.get("seq").and_then(serde_json::Value::as_u64) == Some(u64::from(seq))
                }))
            }),
        )
    }

    // ----- Route maps -------------------------------------------------------

    /// The route maps' ids (`GET /cluster/sdn/route-maps`) — one per map that
    /// has at least one entry.
    pub fn sdn_route_maps(&self) -> Result<Vec<serde_json::Value>> {
        let body = self.get("/cluster/sdn/route-maps")?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "/cluster/sdn/route-maps")?;
        Ok(w.data)
    }

    /// Every entry of every route map (`GET /cluster/sdn/route-maps/entries`),
    /// the RUNNING configuration with `running`.
    pub fn sdn_route_map_entries_all(&self, running: bool) -> Result<Vec<serde_json::Value>> {
        let body = if running {
            self.get("/cluster/sdn/route-maps/entries?running=1")?
        } else {
            self.get("/cluster/sdn/route-maps/entries")?
        };
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "/cluster/sdn/route-maps/entries")?;
        Ok(w.data)
    }

    /// The entries of one route map
    /// (`GET /cluster/sdn/route-maps/entries/{route-map-id}`).
    pub fn sdn_route_map_entries(&self, route_map: &str) -> Result<Vec<serde_json::Value>> {
        validate_route_map_id(route_map)?;
        let path = format!(
            "/cluster/sdn/route-maps/entries/{route_map_id}",
            route_map_id = route_map
        );
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /cluster/sdn/route-maps/entries/{route-map-id}")?;
        Ok(w.data)
    }

    /// One entry
    /// (`GET /cluster/sdn/route-maps/entries/{route-map-id}/entry/{order}`).
    pub fn sdn_route_map_entry(&self, route_map: &str, order: u16) -> Result<serde_json::Value> {
        validate_route_map_id(route_map)?;
        let path = format!(
            "/cluster/sdn/route-maps/entries/{route_map_id}/entry/{order}",
            route_map_id = route_map
        );
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> = parse(
            &body,
            "GET /cluster/sdn/route-maps/entries/{route-map-id}/entry/{order}",
        )?;
        Ok(w.data)
    }

    /// Stages one entry of a route map, creating the map if it has none yet
    /// (`POST /cluster/sdn/route-maps/entries`).
    pub fn create_sdn_route_map_entry(
        &self,
        ledger: &Ledger,
        route_map: &str,
        order: u16,
        entry: &RouteMapEntry<'_>,
    ) -> Result<()> {
        validate_route_map_id(route_map)?;
        let order_text = order.to_string();
        let mut fields: Vec<(&'static str, String)> = vec![
            ("route-map-id", route_map.to_string()),
            ("order", order_text),
            ("action", entry.action.as_str().to_string()),
        ];
        clause_fields(
            &mut fields,
            Some(entry.matches),
            Some(entry.sets),
            entry.call,
            entry.exit_action,
        )?;
        let form: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::CreateSdnRouteMapEntry,
            || self.post_form("/cluster/sdn/route-maps/entries", &form, true),
            Some(&|| Ok(self.sdn_route_map_entry(route_map, order).is_ok())),
        )
    }

    /// Changes one entry
    /// (`PUT /cluster/sdn/route-maps/entries/{route-map-id}/entry/{order}`).
    pub fn update_sdn_route_map_entry(
        &self,
        ledger: &Ledger,
        route_map: &str,
        order: u16,
        update: &RouteMapEntryUpdate<'_>,
    ) -> Result<()> {
        validate_route_map_id(route_map)?;
        validate_delete(
            "route-map entry",
            update.delete,
            &["match", "set", "call", "exit-action"],
        )?;
        let mut fields: Vec<(&'static str, String)> = Vec::new();
        if let Some(a) = update.action {
            fields.push(("action", a.as_str().to_string()));
        }
        clause_fields(
            &mut fields,
            update.matches,
            update.sets,
            update.call,
            update.exit_action,
        )?;
        for d in update.delete {
            fields.push(("delete", d.to_string()));
        }
        let form: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let path = format!(
            "/cluster/sdn/route-maps/entries/{route_map_id}/entry/{order}",
            route_map_id = route_map
        );
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnRouteMapEntry,
            || self.put_form(&path, &form),
            None,
        )
    }

    /// Removes one entry
    /// (`DELETE /cluster/sdn/route-maps/entries/{route-map-id}/entry/{order}`);
    /// the map goes with its last entry. The node refuses it while a
    /// controller still names the map.
    pub fn delete_sdn_route_map_entry(
        &self,
        ledger: &Ledger,
        route_map: &str,
        order: u16,
    ) -> Result<()> {
        validate_route_map_id(route_map)?;
        let path = format!(
            "/cluster/sdn/route-maps/entries/{route_map_id}/entry/{order}",
            route_map_id = route_map
        );
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnRouteMapEntry,
            || self.delete(&path),
            Some(&|| {
                Ok(!self.sdn_route_map_entries_all(false)?.iter().any(|e| {
                    e.get("route-map-id").and_then(|v| v.as_str()) == Some(route_map)
                        && e.get("order").and_then(serde_json::Value::as_u64)
                            == Some(u64::from(order))
                }))
            }),
        )
    }

    // ----- The vnet firewall ------------------------------------------------

    /// The vnet firewall's index (`GET /cluster/sdn/vnets/{vnet}/firewall`:
    /// `rules`, `options`). Like every route below, answers only for a vnet in
    /// the RUNNING configuration.
    pub fn sdn_vnet_firewall_index(&self, vnet: &str) -> Result<Vec<serde_json::Value>> {
        validate_sdn_id(vnet)?;
        let path = format!("/cluster/sdn/vnets/{vnet}/firewall");
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /cluster/sdn/vnets/{vnet}/firewall")?;
        Ok(w.data)
    }

    /// The vnet firewall's options (`GET …/vnets/{vnet}/firewall/options`).
    pub fn sdn_vnet_firewall_options(&self, vnet: &str) -> Result<serde_json::Value> {
        validate_sdn_id(vnet)?;
        let path = format!("/cluster/sdn/vnets/{vnet}/firewall/options");
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> =
            parse(&body, "GET /cluster/sdn/vnets/{vnet}/firewall/options")?;
        Ok(w.data)
    }

    /// Sets the vnet firewall's options (`PUT …/vnets/{vnet}/firewall/options`).
    /// Written at once, not staged.
    pub fn set_sdn_vnet_firewall_options(
        &self,
        ledger: &Ledger,
        vnet: &str,
        opts: &VnetFirewallOptions<'_>,
    ) -> Result<()> {
        validate_sdn_id(vnet)?;
        if let Some(p) = opts.policy_forward {
            if !matches!(p, "ACCEPT" | "DROP") {
                return Err(vnet_rule_invalid(format!(
                    "invalid Proxmox vnet firewall forward policy '{p}': expected ACCEPT or DROP"
                )));
            }
        }
        if let Some(l) = opts.log_level_forward {
            if !LOG_LEVELS.contains(&l) {
                return Err(vnet_rule_invalid(format!(
                    "invalid Proxmox vnet firewall log level '{l}': expected one of {}",
                    LOG_LEVELS.join(", ")
                )));
            }
        }
        for d in opts.delete {
            if !matches!(*d, "enable" | "policy_forward" | "log_level_forward") {
                return Err(vnet_rule_invalid(format!(
                    "cannot clear '{d}' on a Proxmox vnet firewall: expected enable, \
                     policy_forward or log_level_forward"
                )));
            }
        }
        let mut fields: Vec<(&'static str, String)> = Vec::new();
        if let Some(e) = opts.enable {
            fields.push(("enable", if e { "1" } else { "0" }.to_string()));
        }
        if let Some(p) = opts.policy_forward {
            fields.push(("policy_forward", p.to_string()));
        }
        if let Some(l) = opts.log_level_forward {
            fields.push(("log_level_forward", l.to_string()));
        }
        if !opts.delete.is_empty() {
            fields.push(("delete", opts.delete.join(",")));
        }
        let form: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let path = format!("/cluster/sdn/vnets/{vnet}/firewall/options");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::SdnVnetFirewallOptions,
            || self.put_form(&path, &form),
            Some(&|| {
                let o = self.sdn_vnet_firewall_options(vnet)?;
                Ok(opts
                    .policy_forward
                    .is_none_or(|p| o.get("policy_forward").and_then(|v| v.as_str()) == Some(p)))
            }),
        )
    }

    /// The vnet firewall's rules, in the node's order
    /// (`GET …/vnets/{vnet}/firewall/rules`).
    pub fn sdn_vnet_firewall_rules(&self, vnet: &str) -> Result<Vec<serde_json::Value>> {
        validate_sdn_id(vnet)?;
        let path = format!("/cluster/sdn/vnets/{vnet}/firewall/rules");
        let body = self.get(&path)?;
        let w: Wrapped<Vec<serde_json::Value>> =
            parse(&body, "GET /cluster/sdn/vnets/{vnet}/firewall/rules")?;
        Ok(w.data)
    }

    /// One rule, at position `pos` (`GET …/firewall/rules/{pos}`). Measured:
    /// the single-rule answer carries `pos` as a STRING, the list as a number.
    pub fn sdn_vnet_firewall_rule(&self, vnet: &str, pos: u32) -> Result<serde_json::Value> {
        validate_sdn_id(vnet)?;
        let path = format!("/cluster/sdn/vnets/{vnet}/firewall/rules/{pos}");
        let body = self.get(&path)?;
        let w: Wrapped<serde_json::Value> =
            parse(&body, "GET /cluster/sdn/vnets/{vnet}/firewall/rules/{pos}")?;
        Ok(w.data)
    }

    /// Adds a `forward` rule (`POST …/vnets/{vnet}/firewall/rules`) — at the
    /// TOP of the list, where the node inserts it. `opts.rule_type`, if given,
    /// has to be `forward`; `enable` is sent explicitly, defaulting to on, for
    /// the reason [`Client::add_firewall_rule`] gives. No probe, for the same
    /// reason that one has none.
    pub fn add_sdn_vnet_firewall_rule(
        &self,
        ledger: &Ledger,
        vnet: &str,
        action: &str,
        opts: &FirewallRuleOpts<'_>,
    ) -> Result<()> {
        validate_sdn_id(vnet)?;
        validate_vnet_rule(opts.rule_type, Some(action))?;
        let mut fields: Vec<(&str, String)> = vec![
            ("type", "forward".to_string()),
            ("action", action.to_string()),
            (
                "enable",
                if opts.enable.unwrap_or(true) {
                    "1"
                } else {
                    "0"
                }
                .to_string(),
            ),
        ];
        fields.extend(crate::firewall_rule_common_fields(opts));
        let form: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let path = format!("/cluster/sdn/vnets/{vnet}/firewall/rules");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::AddSdnVnetFirewallRule,
            || self.post_form(&path, &form, true),
            None,
        )
    }

    /// Changes a rule at `pos` (`PUT …/firewall/rules/{pos}`); only the fields
    /// given are sent. `moveto` moves the rule to another position instead —
    /// the node ignores every other field when it is set (its own schema
    /// says so), so the two are refused together.
    pub fn update_sdn_vnet_firewall_rule(
        &self,
        ledger: &Ledger,
        vnet: &str,
        pos: u32,
        opts: &FirewallRuleOpts<'_>,
        moveto: Option<u32>,
    ) -> Result<()> {
        validate_sdn_id(vnet)?;
        validate_vnet_rule(opts.rule_type, opts.action)?;
        let mut fields = crate::firewall_rule_common_fields(opts);
        if let Some(t) = opts.rule_type {
            fields.push(("type", t.to_string()));
        }
        if let Some(a) = opts.action {
            fields.push(("action", a.to_string()));
        }
        if let Some(e) = opts.enable {
            fields.push(("enable", if e { "1" } else { "0" }.to_string()));
        }
        if let Some(m) = moveto {
            if !fields.is_empty() {
                return Err(vnet_rule_invalid(
                    "a Proxmox vnet firewall rule update cannot both move the rule and change \
                     it: the node ignores every other field when `moveto` is set"
                        .into(),
                ));
            }
            fields.push(("moveto", m.to_string()));
        }
        let form: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let path = format!("/cluster/sdn/vnets/{vnet}/firewall/rules/{pos}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::UpdateSdnVnetFirewallRule,
            || self.put_form(&path, &form),
            None,
        )
    }

    /// Removes the rule at `pos` (`DELETE …/firewall/rules/{pos}`).
    pub fn delete_sdn_vnet_firewall_rule(
        &self,
        ledger: &Ledger,
        vnet: &str,
        pos: u32,
    ) -> Result<()> {
        validate_sdn_id(vnet)?;
        let path = format!("/cluster/sdn/vnets/{vnet}/firewall/rules/{pos}");
        self.task_or_done(
            ledger,
            SDN_VMID,
            TaskKind::DeleteSdnVnetFirewallRule,
            || self.delete(&path),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_follow_the_nodes_own_formats() {
        for ok in ["ev", "evpn1", "my_ctl-2", "A1"] {
            assert!(validate_controller_id(ok).is_ok(), "{ok}");
        }
        for bad in ["", "e", "1ev", "ev-", "e v", &"x".repeat(65)] {
            assert!(validate_controller_id(bad).is_err(), "{bad:?}");
        }
        for ok in ["p", "pl-1", "a_b", "1x", "a-"] {
            assert!(validate_prefix_list_id(ok).is_ok(), "{ok}");
            assert!(validate_route_map_id(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-a", "_a", "a b", &"x".repeat(33)] {
            assert!(validate_prefix_list_id(bad).is_err(), "{bad:?}");
        }
        for reserved in ["only_default", "only_default_v6", "loopbacks_ips"] {
            let e = validate_prefix_list_id(reserved).unwrap_err().to_string();
            assert!(e.contains("reserves"), "{e}");
        }
        for reserved in ["pve_x", "MAP_VTEP_IN", "MAP_VTEP_OUT", "correct_src"] {
            assert!(validate_route_map_id(reserved).is_err(), "{reserved}");
        }
        assert!(validate_routing_list_id("route-map", "rm1").is_ok());
        assert!(validate_routing_list_id("acl", "rm1").is_err());
    }

    #[test]
    fn a_controller_has_the_fields_of_its_type_and_no_others() {
        let peers = ["10.0.0.1", "10.0.0.2"];
        let evpn = ControllerOptions {
            asn: Some(65000),
            peers: &peers,
            route_map_in: Some("rm1"),
            ..Default::default()
        };
        let f = controller_fields(ControllerKind::Evpn, &evpn, true).unwrap();
        assert!(
            f.contains(&("peers", "10.0.0.1,10.0.0.2".to_string())),
            "{f:?}"
        );
        assert!(f.contains(&("route-map-in", "rm1".to_string())), "{f:?}");

        let both = ControllerOptions {
            fabric: Some("f1"),
            ..evpn
        };
        let e = controller_fields(ControllerKind::Evpn, &both, true).unwrap_err();
        assert!(
            e.to_string().contains("exactly one of peers or fabric"),
            "{e}"
        );
        let neither = ControllerOptions {
            asn: Some(1),
            ..Default::default()
        };
        assert!(controller_fields(ControllerKind::Evpn, &neither, true).is_err());

        let on_evpn = ControllerOptions {
            ebgp: Some(true),
            ..evpn
        };
        let e = controller_fields(ControllerKind::Evpn, &on_evpn, true).unwrap_err();
        assert!(e.to_string().contains("has no ebgp"), "{e}");

        let bgp = ControllerOptions {
            asn: Some(65001),
            peers: &peers[..1],
            ..Default::default()
        };
        let e = controller_fields(ControllerKind::Bgp, &bgp, true).unwrap_err();
        assert!(e.to_string().contains("needs node"), "{e}");
        // An update sends only what it is given, with no requirement.
        assert!(controller_fields(ControllerKind::Bgp, &bgp, false).is_ok());
        let bad_peer = ["10.0.0.1/32"];
        let bad = ControllerOptions {
            peers: &bad_peer,
            ..bgp
        };
        assert!(controller_fields(ControllerKind::Bgp, &bad, false).is_err());
    }

    #[test]
    fn a_prefix_range_follows_frrs_rule() {
        assert!(validate_range("10.0.0.0/16", None, Some(24)).is_ok());
        assert!(validate_range("10.0.0.0/16", Some(20), Some(24)).is_ok());
        assert!(validate_range("0.0.0.0/0", None, None).is_ok());
        assert!(validate_range("fd00::/48", Some(64), Some(128)).is_ok());
        for (p, ge, le) in [
            ("10.0.0.0/16", Some(16), None),
            ("10.0.0.0/16", None, Some(16)),
            ("10.0.0.0/16", Some(25), Some(24)),
            ("10.0.0.0/16", None, Some(33)),
            ("10.0.0.0", None, None),
        ] {
            assert!(validate_range(p, ge, le).is_err(), "{p} {ge:?} {le:?}");
        }
        let e = PrefixListEntry {
            action: RoutingAction::Deny,
            prefix: "0.0.0.0/0",
            ge: None,
            le: Some(32),
            seq: Some(100),
        };
        assert_eq!(e.encoded(), "action=deny,prefix=0.0.0.0/0,le=32,seq=100");
    }

    #[test]
    fn a_route_map_clause_is_a_property_string_with_a_known_key() {
        let m = RouteMapClause {
            key: "ip-address-prefix-list",
            value: Some("pl1"),
        };
        assert_eq!(
            m.encoded(MATCH_KEYS, "match").unwrap(),
            "key=ip-address-prefix-list,value=pl1"
        );
        let unchanged = RouteMapClause {
            key: "ip-next-hop-unchanged",
            value: None,
        };
        assert_eq!(
            unchanged.encoded(SET_KEYS, "set").unwrap(),
            "key=ip-next-hop-unchanged"
        );
        // A set key is not a match key.
        assert!(unchanged.encoded(MATCH_KEYS, "match").is_err());
        let smuggled = RouteMapClause {
            key: "tag",
            value: Some("1,key=metric"),
        };
        assert!(smuggled.encoded(MATCH_KEYS, "match").is_err());
        assert_eq!(
            exit_action_encoded(("on-match-goto", Some(20))).unwrap(),
            "key=on-match-goto,value=20"
        );
        assert!(exit_action_encoded(("on-match-goto", None)).is_err());
        assert!(exit_action_encoded(("stop", None)).is_err());
    }

    #[test]
    fn a_vnet_rule_is_a_forward_rule() {
        assert!(validate_vnet_rule(Some("forward"), Some("DROP")).is_ok());
        for t in ["in", "out", "group"] {
            let e = validate_vnet_rule(Some(t), None).unwrap_err();
            assert_eq!(e.number(), 1551, "{e}");
        }
        assert!(validate_vnet_rule(None, Some("+group")).is_err());
    }
}
