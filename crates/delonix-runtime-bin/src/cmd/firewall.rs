//! `delonix ingress` / `delonix egress` — the single firewall surface.
//!
//! Both groups edit ONE source of truth: the per-container [`ContainerFw`]
//! (persisted on the `Container`, enforced as nft rules in the ingress netns).
//! `ingress` owns inbound (`dir=in`) rules + the DNAT publishes; `egress` owns
//! outbound (`dir=out`) rules + the per-network egress-to-Internet policy. A
//! container only has a firewall when it lives on a custom network (it has an
//! IP on the `delonix0` bridge) — `--net host` containers share the host stack
//! and are rejected honestly.

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_net::infra;
use delonix_runtime_core::{
    fw_port_ok, fw_proto_ok, fw_src_ok, Container, Error, FwRule, Result, Store,
};
use serde::{Deserialize, Serialize};

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::open_stores;

/// `allow` (accept) or `deny` (drop) — the action baked into a rule or policy.
#[derive(clap::ValueEnum, Clone, Copy, PartialEq)]
pub enum Action {
    Allow,
    Deny,
}
impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Deny => "deny",
        }
    }
}

/// How a network's egress to the Internet is governed.
#[derive(clap::ValueEnum, Clone, Copy)]
pub enum EgressMode {
    /// Allow all egress (the default).
    Allow,
    /// Block all egress to the Internet.
    Deny,
    /// Deny all egress EXCEPT DNS and the CIDRs given in `--to` (allowlist).
    Allowlist,
}

#[derive(Subcommand)]
pub enum IngressCmd {
    /// Allow inbound traffic to a container: `[proto/]port` from an optional CIDR.
    Allow {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        /// `tcp/5432`, `udp/53`, `5432` (any proto), or `tcp/*` (all ports).
        port: String,
        /// Only from this source CIDR (default: anywhere).
        #[arg(long)]
        from: Option<String>,
        /// Free-form note kept with the rule.
        #[arg(long)]
        note: Option<String>,
    },
    /// Deny inbound traffic to a container (same shape as `allow`).
    Deny {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        port: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Set the default inbound policy when no rule matches.
    Policy {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        policy: Action,
    },
    /// Publish a host port to the container (DNAT through the ingress).
    Publish {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        /// `hostPort:containerPort[/tcp|udp]` or just `port`.
        spec: String,
    },
    /// Remove a published host port.
    Unpublish {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        host_port: String,
    },
    /// Show the inbound firewall (policy + rules) and published ports.
    Ls {
        /// Container to inspect (omit to list every container's inbound state).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: Option<String>,
    },
    /// Remove inbound rule(s) matching `[proto/]port` (all protos if none given).
    Rm {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        /// `tcp/5432`, `5432` (any proto), or `*` (all ports).
        port: String,
        /// Only rules from this source CIDR (default: any recorded source).
        #[arg(long)]
        from: Option<String>,
    },
    /// Remove all inbound rules (keeps published ports).
    Clear {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
    },
}

#[derive(Subcommand)]
pub enum EgressCmd {
    /// Allow outbound traffic from a container: `[proto/]port` to an optional CIDR.
    Allow {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        port: String,
        /// Only to this destination CIDR (default: anywhere).
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Deny outbound traffic from a container (same shape as `allow`).
    Deny {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        port: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Set the default outbound policy when no rule matches.
    Policy {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        policy: Action,
    },
    /// Govern a whole network's egress to the Internet.
    Net {
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        network: String,
        mode: EgressMode,
        /// CIDRs for `allowlist` mode (comma-separated), e.g. `10.0.0.0/8,1.1.1.1/32`.
        #[arg(long)]
        to: Option<String>,
    },
    /// Allow a network's egress to a HOSTNAME (and `*.hostname`), learnt live from
    /// DNS answers — the FQDN allowlist nft/CIDR can't express. Repeatable.
    Host {
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        network: String,
        /// e.g. `github.com` (matches `github.com` and `*.github.com`).
        hostname: String,
    },
    /// Show the outbound firewall (policy + rules).
    Ls {
        /// Container to inspect (omit to list every container's outbound state).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: Option<String>,
    },
    /// Remove outbound rule(s) matching `[proto/]port` (all protos if none given).
    Rm {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        /// `tcp/5432`, `5432` (any proto), or `*` (all ports).
        port: String,
        /// Only rules to this destination CIDR (default: any recorded destination).
        #[arg(long)]
        to: Option<String>,
    },
    /// Show a NETWORK's egress policy: CIDR allowlist, FQDN hosts, and the IPs
    /// currently learnt from DNS for those hosts.
    Show {
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        network: String,
    },
    /// Remove all outbound rules.
    Clear {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
    },
}

pub fn run_ingress(cmd: IngressCmd) -> Result<()> {
    let (_images, store) = open_stores()?;
    match cmd {
        IngressCmd::Allow {
            container,
            port,
            from,
            note,
        } => add_rule(&store, &container, "in", Action::Allow, &port, from, note),
        IngressCmd::Deny {
            container,
            port,
            from,
            note,
        } => add_rule(&store, &container, "in", Action::Deny, &port, from, note),
        IngressCmd::Policy { container, policy } => set_policy(&store, &container, "in", policy),
        IngressCmd::Publish { container, spec } => {
            let mut c = store.load(&container)?;
            super::container::publish_live(&store, &mut c, &spec)
        }
        IngressCmd::Unpublish {
            container,
            host_port,
        } => {
            let mut c = store.load(&container)?;
            super::container::unpublish_live(&store, &mut c, &host_port)
        }
        IngressCmd::Ls { container } => match container {
            Some(c) => list_rules(&store, &c, "in"),
            None => list_all(&store, "in"),
        },
        IngressCmd::Rm {
            container,
            port,
            from,
        } => remove_rule(&store, &container, "in", &port, from),
        IngressCmd::Clear { container } => clear_dir(&store, &container, "in"),
    }
}

pub fn run_egress(cmd: EgressCmd) -> Result<()> {
    let (_images, store) = open_stores()?;
    match cmd {
        EgressCmd::Allow {
            container,
            port,
            to,
            note,
        } => add_rule(&store, &container, "out", Action::Allow, &port, to, note),
        EgressCmd::Deny {
            container,
            port,
            to,
            note,
        } => add_rule(&store, &container, "out", Action::Deny, &port, to, note),
        EgressCmd::Policy { container, policy } => set_policy(&store, &container, "out", policy),
        EgressCmd::Net { network, mode, to } => egress_net(&network, mode, to),
        EgressCmd::Host { network, hostname } => egress_host(&network, &hostname),
        EgressCmd::Show { network } => egress_show(&network),
        EgressCmd::Ls { container } => match container {
            Some(c) => list_rules(&store, &c, "out"),
            None => list_all(&store, "out"),
        },
        EgressCmd::Rm {
            container,
            port,
            to,
        } => remove_rule(&store, &container, "out", &port, to),
        EgressCmd::Clear { container } => clear_dir(&store, &container, "out"),
    }
}

/// Split `[proto/]port` into a validated `(proto, port)`. `proto` defaults to
/// `any`; `port` accepts a number, a `n-m` range, or `*`.
fn parse_port_spec(spec: &str) -> Result<(String, String)> {
    let (proto, port) = match spec.split_once('/') {
        Some((p, port)) => (p.to_string(), port.to_string()),
        None => ("any".to_string(), spec.to_string()),
    };
    if !fw_proto_ok(&proto) {
        return Err(Error::Invalid(format!(
            "invalid proto '{proto}' (tcp|udp|any)"
        )));
    }
    if !fw_port_ok(&port) {
        return Err(Error::Invalid(format!(
            "invalid port '{port}' (1-65535, a range n-m, or *)"
        )));
    }
    Ok((proto, port))
}

/// The container's SDN IP, or an error explaining why a firewall can't attach.
fn require_sdn_ip(c: &Container) -> Result<String> {
    c.ip.clone().filter(|s| !s.is_empty()).ok_or_else(|| {
        Error::Invalid(format!(
            "'{}' has no firewall: it is not on a custom network (attach it with `--net <network>`; `--net host` shares the host stack)",
            c.name
        ))
    })
}

/// `""`, `0.0.0.0/0` and `*` all mean "from/to anywhere" — the
/// dataplane treats them alike (see `fw_chain_body`); normalize to compare.
fn norm_any(s: &str) -> &str {
    if s == "0.0.0.0/0" || s == "*" {
        ""
    } else {
        s
    }
}

/// `true` if two values of a field overlap in the first-match sense:
/// equal, or one is one of the given wildcards. (Conservative approximation — does
/// not parse `n-m` ranges; serves the shadow WARNING, not exact replacement.)
fn field_overlaps(a: &str, b: &str, wilds: &[&str]) -> bool {
    a == b || wilds.contains(&a) || wilds.contains(&b)
}

/// A rule's `[proto/]port` spec, to reproduce in `ingress rm`.
fn rule_spec(r: &FwRule) -> String {
    if r.proto.is_empty() || r.proto == "any" {
        r.port.clone()
    } else {
        format!("{}/{}", r.proto, r.port)
    }
}

fn add_rule(
    store: &Store,
    name: &str,
    dir: &str,
    action: Action,
    port_spec: &str,
    cidr: Option<String>,
    note: Option<String>,
) -> Result<()> {
    let (proto, port) = parse_port_spec(port_spec)?;
    let src = cidr.unwrap_or_default();
    if !src.is_empty() && !fw_src_ok(&src) {
        return Err(Error::Invalid(format!("invalid CIDR '{src}'")));
    }
    let mut c = store.load(name)?;
    let ip = require_sdn_ip(&c)?;
    let mut fw = c.firewall.clone().unwrap_or_default();
    fw.enabled = true;
    // The LAST command wins (ufw semantics): a new rule for the SAME match
    // (dir/proto/port/source) REPLACES the existing one. Without this, `deny 8069`
    // followed by `allow 8069` left the service blocked forever — the rules
    // accumulated and the nft chain is first-match terminal: the old deny,
    // above, always won (real bug report).
    let same_match = |r: &FwRule| {
        r.dir == dir && r.proto == proto && r.port == port && norm_any(&r.src) == norm_any(&src)
    };
    let replaced: Vec<String> = fw
        .rules
        .iter()
        .filter(|r| same_match(r))
        .map(|r| r.action.clone())
        .collect();
    fw.rules.retain(|r| !same_match(r));
    fw.rules.push(FwRule {
        dir: dir.to_string(),
        proto: proto.clone(),
        port: port.clone(),
        src: src.clone(),
        action: action.as_str().to_string(),
        note: note.unwrap_or_default(),
    });
    // Shadow: an EARLIER overlapping rule (e.g. `deny any/8069` vs
    // `allow tcp/8069`) with the opposite action still matches first — the new
    // rule never gets evaluated. Warning here avoids the "I applied the allow and
    // it stays blocked" without explanation.
    let shadow = fw
        .rules
        .iter()
        .take(fw.rules.len() - 1)
        .find(|r| {
            r.dir == dir
                && r.action != action.as_str()
                && field_overlaps(&r.proto, &proto, &["any", ""])
                && field_overlaps(&r.port, &port, &["*", ""])
                && field_overlaps(norm_any(&r.src), norm_any(&src), &[""])
        })
        .map(|r| (r.action.clone(), rule_spec(r)));
    infra::apply_firewall(&c.id, &ip, &fw)?;
    c.firewall = Some(fw);
    store.save(&c)?;
    let arrow = if dir == "in" { "inbound" } else { "outbound" };
    println!(
        "{}: {arrow} rule added ({})",
        c.name,
        output::bold(&format!("{} {port_spec}", action.as_str()))
    );
    if let Some(old) = replaced.iter().find(|a| *a != action.as_str()) {
        println!(
            "{}",
            super::po::tf(
                "  (replaces the previous {old} rule for this match — the last command wins)",
                &[("old", old)],
            )
        );
    }
    if let Some((sh_action, sh_spec)) = shadow {
        let group = if dir == "in" { "ingress" } else { "egress" };
        output::warn(&super::po::tf(
            "an earlier overlapping rule ({action} {spec}) still matches first and can override this one — remove it with `delonix {group} rm {name} {spec}`",
            &[
                ("action", &sh_action),
                ("spec", &sh_spec),
                ("group", group),
                ("name", &c.name),
            ],
        ));
    }
    Ok(())
}

/// Remove rule(s) matching `[proto/]port` (+ CIDR, if given). The SPEC's
/// wildcards work as a filter: `rm c 8069` (proto `any`) removes the tcp/udp/any
/// rules for that port; `rm c '*'` removes all; without `--from`, any source.
/// Complements `clear` (all-or-nothing) with surgical removal.
fn remove_rule(
    store: &Store,
    name: &str,
    dir: &str,
    port_spec: &str,
    cidr: Option<String>,
) -> Result<()> {
    let (proto, port) = parse_port_spec(port_spec)?;
    let src = cidr.unwrap_or_default();
    if !src.is_empty() && !fw_src_ok(&src) {
        return Err(Error::Invalid(format!("invalid CIDR '{src}'")));
    }
    let mut c = store.load(name)?;
    let ip = require_sdn_ip(&c)?;
    let mut fw = c.firewall.clone().unwrap_or_default();
    let rm_match = |r: &FwRule| {
        r.dir == dir
            && (proto == "any" || r.proto == proto)
            && (port == "*" || r.port == port)
            && (norm_any(&src).is_empty() || norm_any(&r.src) == norm_any(&src))
    };
    let before = fw.rules.len();
    fw.rules.retain(|r| !rm_match(r));
    let n = before - fw.rules.len();
    if n == 0 {
        let arrow = if dir == "in" { "inbound" } else { "outbound" };
        return Err(Error::Invalid(format!(
            "'{}' has no {arrow} rule matching {port_spec}",
            c.name
        )));
    }
    // Same rule as `clear`: with no rules and no explicit policies, the firewall
    // disappears entirely (clean chain) instead of leaving an empty record.
    let empty = fw.rules.is_empty() && fw.policy_in.is_empty() && fw.policy_out.is_empty();
    if empty {
        infra::clear_firewall(&ip);
    } else {
        infra::apply_firewall(&c.id, &ip, &fw)?;
    }
    c.firewall = if empty { None } else { Some(fw) };
    store.save(&c)?;
    let arrow = if dir == "in" { "inbound" } else { "outbound" };
    println!(
        "{}",
        super::po::tf(
            "{name}: {n} {arrow} rule(s) removed ({spec})",
            &[
                ("name", &c.name),
                ("n", &n.to_string()),
                ("arrow", arrow),
                ("spec", port_spec),
            ],
        )
    );
    Ok(())
}

fn set_policy(store: &Store, name: &str, dir: &str, policy: Action) -> Result<()> {
    let mut c = store.load(name)?;
    let ip = require_sdn_ip(&c)?;
    let mut fw = c.firewall.clone().unwrap_or_default();
    fw.enabled = true;
    if dir == "in" {
        fw.policy_in = policy.as_str().to_string();
    } else {
        fw.policy_out = policy.as_str().to_string();
    }
    infra::apply_firewall(&c.id, &ip, &fw)?;
    c.firewall = Some(fw);
    store.save(&c)?;
    let arrow = if dir == "in" { "inbound" } else { "outbound" };
    println!("{}: default {arrow} policy = {}", c.name, policy.as_str());
    Ok(())
}

/// Overview of every container's firewall state in one table — `ls` without an
/// argument, like `docker ps`. Per-container detail stays in `ls <container>`.
fn list_all(store: &Store, dir: &str) -> Result<()> {
    let mut t = output::Table::new(&[
        "NAME",
        "POLICY",
        "RULES",
        if dir == "in" { "PUBLISHED" } else { "NETWORKS" },
    ]);
    for c in store.list()? {
        let fw = c.firewall.clone().unwrap_or_default();
        let policy = if dir == "in" {
            &fw.policy_in
        } else {
            &fw.policy_out
        };
        let policy = if policy.is_empty() {
            "allow (default)".to_string()
        } else {
            policy.clone()
        };
        let rules = fw.rules.iter().filter(|r| r.dir == dir).count();
        let last = if dir == "in" {
            c.ports.join(", ")
        } else {
            // Main network + extras (multi-homing) — the targets of the egress policy.
            let mut nets: Vec<String> = c.network.clone().into_iter().collect();
            nets.extend(c.extra_networks.iter().map(|e| e.network.clone()));
            nets.join(", ")
        };
        t.row(vec![c.name.clone(), policy, rules.to_string(), last]);
    }
    t.print();
    Ok(())
}

fn list_rules(store: &Store, name: &str, dir: &str) -> Result<()> {
    let c = store.load(name)?;
    let fw = c.firewall.clone().unwrap_or_default();
    let policy = if dir == "in" {
        &fw.policy_in
    } else {
        &fw.policy_out
    };
    let default = if policy.is_empty() {
        "allow (default)"
    } else {
        policy.as_str()
    };
    let arrow = if dir == "in" { "INBOUND" } else { "OUTBOUND" };
    println!(
        "{} firewall for {} — default policy: {}",
        arrow, c.name, default
    );
    let mut t = output::Table::new(&[
        "PROTO",
        "PORT",
        if dir == "in" { "FROM" } else { "TO" },
        "ACTION",
        "NOTE",
    ]);
    for r in fw.rules.iter().filter(|r| r.dir == dir) {
        t.row(vec![
            or_any(&r.proto),
            or_any(&r.port),
            or_any(&r.src),
            r.action.clone(),
            r.note.clone(),
        ]);
    }
    if dir == "in" {
        for p in &c.ports {
            t.row(vec![
                "publish".into(),
                p.clone(),
                "0.0.0.0/0".into(),
                "allow".into(),
                "DNAT".into(),
            ]);
        }
    }
    t.print();
    Ok(())
}

fn or_any(s: &str) -> String {
    if s.is_empty() || s == "*" {
        "any".to_string()
    } else {
        s.to_string()
    }
}

fn clear_dir(store: &Store, name: &str, dir: &str) -> Result<()> {
    let mut c = store.load(name)?;
    let mut fw = match c.firewall.clone() {
        Some(f) => f,
        None => {
            println!("{}: no firewall to clear", c.name);
            return Ok(());
        }
    };
    let before = fw.rules.len();
    fw.rules.retain(|r| r.dir != dir);
    let removed = before - fw.rules.len();
    // If nothing is left (no rules, both policies default), drop the firewall
    // entirely and detach it from the ingress; otherwise re-apply what remains.
    let empty = fw.rules.is_empty() && fw.policy_in.is_empty() && fw.policy_out.is_empty();
    if let Some(ip) = c.ip.clone().filter(|s| !s.is_empty()) {
        if empty {
            infra::clear_firewall(&ip);
        } else {
            infra::apply_firewall(&c.id, &ip, &fw)?;
        }
    }
    c.firewall = if empty { None } else { Some(fw) };
    store.save(&c)?;
    let arrow = if dir == "in" { "inbound" } else { "outbound" };
    println!("{}: removed {removed} {arrow} rule(s)", c.name);
    Ok(())
}

fn egress_net(network: &str, mode: EgressMode, to: Option<String>) -> Result<()> {
    // The REAL bridge lives in the infra registry (NetDef, `dlxn{:08x}`), NOT in
    // the NetworkStore (`dlxn{:02x}{:04x}`) — using the wrong one makes the nft
    // rules never match traffic. resolve_net returns the bridge the holder created.
    let bridge = infra::resolve_net(network)?.0;
    match mode {
        EgressMode::Allow => {
            infra::set_egress_policy_net(&bridge, false)?;
            println!("network {network}: egress to the Internet ALLOWED");
        }
        EgressMode::Deny => {
            infra::set_egress_policy_net(&bridge, true)?;
            println!("network {network}: egress to the Internet DENIED");
        }
        EgressMode::Allowlist => {
            let raw =
                to.ok_or_else(|| Error::Invalid("allowlist mode needs `--to <cidr,...>`".into()))?;
            let cidrs: Vec<&str> = raw
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            for c in &cidrs {
                if !fw_src_ok(c) {
                    return Err(Error::Invalid(format!("invalid CIDR '{c}'")));
                }
            }
            infra::set_egress_policy_net_allowlist(&bridge, &cidrs)?;
            println!(
                "network {network}: egress DENIED except DNS + {}",
                cidrs.join(", ")
            );
        }
    }
    Ok(())
}

// ---- declarative: `kind: Ingress` / `kind: Egress` ---------------------------

/// A `kind: Ingress`/`Egress` document. Each doc is the DESIRED STATE of one
/// direction (inbound for `Ingress`, outbound for `Egress`) for its `target`
/// container — applying it REPLACES that direction's rules and policy, leaving
/// the other direction untouched, so an `Ingress` and an `Egress` doc compose
/// on the same container. Allowlist by default (`defaultPolicy: deny`), like a
/// k8s NetworkPolicy.
#[derive(Deserialize, Serialize)]
struct FwDocSpec {
    /// `ingress`|`egress` — only for `kind: FirewallPolicy` (the direction comes
    /// from the Kind for the legacy `Egress`). Captured so the dry-run round-trip
    /// preserves it; `apply` reads it directly from `doc.spec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    direction: Option<String>,
    /// `container` (default) or `network`. In `network` (only `Egress`), the `target`
    /// is a NETWORK NAME and the per-network egress policy + CIDR/FQDN allowlist +
    /// L4 rate-limit apply — not per-container L4 rules.
    #[serde(default)]
    scope: Option<String>,
    /// `container` (default): container name. `network`: network name.
    target: String,
    /// `allow` or `deny` when no rule matches. Default `deny` (allowlist).
    #[serde(default, rename = "defaultPolicy")]
    default_policy: Option<String>,
    #[serde(default)]
    rules: Vec<FwDocRule>,
    // ---- only `scope: network` (per-network Egress) ---------------------------
    /// CIDRs allowed when `defaultPolicy: deny` (egress allowlist, besides
    /// DNS). Translates to `set_egress_policy_net_allowlist`.
    #[serde(default, rename = "allowCidrs")]
    allow_cidrs: Vec<String>,
    /// FQDNs allowed (and `*.fqdn`), learnt LIVE from DNS (DNS-snooping).
    /// Translates to `set_egress_host` per host.
    #[serde(default, rename = "fqdnAllowlist")]
    fqdn_allowlist: Vec<String>,
    /// L4 protection (conn-rate/conn-max) — **GLOBAL** to the rootless ingress, not
    /// per-network (the engine API `set_l4_guard` is global). Translates to `set_l4_guard`.
    #[serde(default, rename = "rateLimit")]
    rate_limit: Option<RateLimitSpec>,
}

/// `spec.rateLimit` — the ingress L4 DDoS protection (global). `{connRate: 0,
/// connMax: 0}` explicitly TURNS OFF the guard (clear_l4_guard).
#[derive(Deserialize, Serialize)]
struct RateLimitSpec {
    /// New connections per second allowed.
    #[serde(default, rename = "connRate")]
    conn_rate: u32,
    /// Maximum concurrent connections.
    #[serde(default, rename = "connMax")]
    conn_max: u32,
}

/// Names accepted in the `spec` of `kind: Ingress`/`Egress`, for the unknown-field
/// warning (the `rules[]` is validated by `FwDocRule`'s deserialization).
pub(crate) const FW_SPEC_FIELDS: &[&str] = &[
    "direction",
    "scope",
    "target",
    "defaultPolicy",
    "rules",
    "allowCidrs",
    "fqdnAllowlist",
    "rateLimit",
];

#[derive(Deserialize, Serialize)]
struct FwDocRule {
    /// `tcp`/`udp`/`any` (default `any`).
    #[serde(default)]
    proto: Option<String>,
    /// Port, range `n-m`, or `*`.
    port: String,
    /// Source CIDR (Ingress) — the other end of inbound traffic.
    #[serde(default)]
    from: Option<String>,
    /// Destination CIDR (Egress) — the other end of outbound traffic.
    #[serde(default)]
    to: Option<String>,
    /// `allow` (default) or `deny`.
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

/// Applies every `Ingress` and `Egress` document in the manifest. Called last in
/// `stack apply` (the target containers must already exist).
/// Dry-run: the firewall spec (`Egress`/`FirewallPolicy`) with defaults materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: FwDocSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (_images, store) = open_stores()?;
    // NB: `kind: Ingress` is NO LONGER firewall — it is now the k8s-shaped L7/HTTP
    // Ingress (see `cmd::httproute`). Inbound L4 firewall lives under
    // `kind: FirewallPolicy` (direction: ingress). `kind: Egress` (outbound) stays.
    apply_kind(&store, docs, "out")?; // kind: Egress
                                      // kind: FirewallPolicy — the UNIFIED form (the direction comes from `spec.direction`
                                      // instead of the Kind name). Applies the SAME logic; the canonical inbound form.
    for doc in manifest::of_kind(docs, "FirewallPolicy") {
        let dir = match doc.spec.get("direction").and_then(|v| v.as_str()) {
            Some("ingress") => "in",
            Some("egress") => "out",
            other => {
                return Err(Error::Invalid(format!(
                    "FirewallPolicy/{}: direction obrigatório e ∈ {{ingress, egress}} (veio {other:?})",
                    doc.metadata.name
                )));
            }
        };
        apply_fw_doc(&store, doc, dir)?;
    }
    Ok(())
}

fn apply_kind(store: &Store, docs: &[ManifestDoc], dir: &str) -> Result<()> {
    // `dir` "in" → the Ingress Kind; "out" → the Egress Kind.
    let kind = if dir == "in" { "Ingress" } else { "Egress" };
    for doc in manifest::of_kind(docs, kind) {
        apply_fw_doc(store, doc, dir)?;
    }
    Ok(())
}

/// Applies ONE firewall document (Ingress/Egress/FirewallPolicy) in the `dir`
/// direction ("in"/"out"). The label in messages uses the document's real Kind.
fn apply_fw_doc(store: &Store, doc: &ManifestDoc, dir: &str) -> Result<()> {
    let kind = doc.kind.as_str();
    manifest::warn_unknown_fields(doc, FW_SPEC_FIELDS);
    let spec: FwDocSpec = manifest::spec_of(doc)?;

    // Validate the scope explicitly — a typo (`netowrk`) must not fall silently
    // into the container path and fail later with 'container does not exist'.
    let scope = spec.scope.as_deref().unwrap_or("container");
    if !matches!(scope, "container" | "network") {
        return Err(Error::Invalid(format!(
            "{kind}/{}: scope inválido '{scope}' (usa container|network)",
            doc.metadata.name
        )));
    }

    // scope: network — PER-NETWORK egress policy (Egress only). The `target`
    // is a network name; wires up the engine APIs that only had a CLI.
    if scope == "network" {
        if dir != "out" {
            return Err(Error::Invalid(format!(
                "{kind}/{}: scope: network só é suportado em Egress (não há política de INGRESS por-rede)",
                doc.metadata.name
            )));
        }
        return apply_network_egress(kind, &doc.metadata.name, &spec);
    }

    let mut c = store.load(&spec.target)?;
    let ip = require_sdn_ip(&c)?;
    let mut fw = c.firewall.clone().unwrap_or_default();
    fw.enabled = true;
    // Declarative: this direction is fully replaced by the document.
    fw.rules.retain(|r| r.dir != dir);
    let policy = spec.default_policy.as_deref().unwrap_or("deny");
    if !matches!(policy, "allow" | "deny") {
        return Err(Error::Invalid(format!(
            "{kind}/{}: defaultPolicy must be allow|deny",
            doc.metadata.name
        )));
    }
    if dir == "in" {
        fw.policy_in = policy.to_string();
    } else {
        fw.policy_out = policy.to_string();
    }
    for r in &spec.rules {
        let proto = r.proto.clone().unwrap_or_else(|| "any".into());
        if !fw_proto_ok(&proto) {
            return Err(Error::Invalid(format!(
                "{kind}/{}: invalid proto '{proto}'",
                doc.metadata.name
            )));
        }
        if !fw_port_ok(&r.port) {
            return Err(Error::Invalid(format!(
                "{kind}/{}: invalid port '{}'",
                doc.metadata.name, r.port
            )));
        }
        let src = r.from.clone().or_else(|| r.to.clone()).unwrap_or_default();
        if !src.is_empty() && !fw_src_ok(&src) {
            return Err(Error::Invalid(format!(
                "{kind}/{}: invalid CIDR '{src}'",
                doc.metadata.name
            )));
        }
        let action = r.action.clone().unwrap_or_else(|| "allow".into());
        if !matches!(action.as_str(), "allow" | "deny") {
            return Err(Error::Invalid(format!(
                "{kind}/{}: action must be allow|deny",
                doc.metadata.name
            )));
        }
        fw.rules.push(FwRule {
            dir: dir.to_string(),
            proto,
            port: r.port.clone(),
            src,
            action,
            note: r.note.clone().unwrap_or_default(),
        });
    }
    infra::apply_firewall(&c.id, &ip, &fw)?;
    let n = fw.rules.iter().filter(|r| r.dir == dir).count();
    c.firewall = Some(fw);
    store.save(&c)?;
    println!(
        "{kind}/{}: applied to {} ({n} rule(s), default {policy})",
        doc.metadata.name, spec.target
    );
    Ok(())
}

/// **Shared per-container ingress core**: replaces a container's `in` direction
/// (default-policy + `allow` rules), preserving the outbound rules. Used by
/// `kind: Dependency` ('A knows B' compiles to: on B, ingress default-deny +
/// allow of A's IP) without duplicating the `ContainerFw` construction.
/// The `allows` already come with `dir="in"`.
pub(crate) fn apply_container_ingress(
    store: &Store,
    target: &str,
    policy: &str,
    allows: &[FwRule],
) -> Result<()> {
    if !matches!(policy, "allow" | "deny") {
        return Err(Error::Invalid(format!(
            "ingress de '{target}': policy tem de ser allow|deny"
        )));
    }
    let mut c = store.load(target)?;
    let ip = require_sdn_ip(&c)?;
    let mut fw = c.firewall.clone().unwrap_or_default();
    fw.enabled = true;
    fw.rules.retain(|r| r.dir != "in"); // declarative: the `in` direction is replaced
    fw.policy_in = policy.to_string();
    fw.rules.extend(allows.iter().cloned());
    infra::apply_firewall(&c.id, &ip, &fw)?;
    c.firewall = Some(fw);
    store.save(&c)?;
    Ok(())
}

/// Applies a `scope: network` `Egress` — per-network egress policy + CIDR/
/// FQDN allowlist + L4 rate-limit. Mirrors exactly the CLI's `egress net`/`egress
/// host`/`l4guard`, but declaratively. **Desired state**: each field is applied
/// exactly as it stands in the document.
fn apply_network_egress(kind: &str, name: &str, spec: &FwDocSpec) -> Result<()> {
    if !spec.rules.is_empty() {
        return Err(Error::Invalid(format!(
            "{kind}/{name}: `rules` é só para scope: container — em scope: network usa allowCidrs/fqdnAllowlist"
        )));
    }
    let policy = spec.default_policy.as_deref().unwrap_or("allow");
    if !matches!(policy, "allow" | "deny") {
        return Err(Error::Invalid(format!(
            "{kind}/{name}: defaultPolicy must be allow|deny"
        )));
    }
    // The allowlist (CIDR/FQDN) ONLY takes effect with `deny` — with `allow` egress
    // stays open and the list would be silently discarded (the user would think
    // they closed the network). Clear error instead of a false show of restriction.
    if policy == "allow" && (!spec.allow_cidrs.is_empty() || !spec.fqdn_allowlist.is_empty()) {
        return Err(Error::Invalid(format!(
            "{kind}/{name}: allowCidrs/fqdnAllowlist só fazem sentido com defaultPolicy: deny (com allow a saída fica aberta)"
        )));
    }
    // VALIDATE EVERYTHING before applying ANYTHING (fail-before-touching): an
    // invalid CIDR or FQDN midway must not leave egress in a partial state.
    for c in &spec.allow_cidrs {
        if !fw_src_ok(c) {
            return Err(Error::Invalid(format!("{kind}/{name}: invalid CIDR '{c}'")));
        }
    }
    for host in &spec.fqdn_allowlist {
        if !fw_host_ok(host) {
            return Err(Error::Invalid(format!(
                "{kind}/{name}: hostname inválido '{host}'"
            )));
        }
    }

    // The REAL bridge lives in the infra registry (not the NetworkStore) — see egress_net.
    let bridge = infra::resolve_net(&spec.target)?.0;

    if policy == "deny" && !spec.allow_cidrs.is_empty() {
        // deny + allowCidrs → allowlist (denies everything except DNS + these CIDRs).
        let cidrs: Vec<&str> = spec.allow_cidrs.iter().map(String::as_str).collect();
        infra::set_egress_policy_net_allowlist(&bridge, &cidrs)?;
    } else {
        // allow → no restriction; deny (no CIDRs) → deny everything (only DNS passes).
        infra::set_egress_policy_net(&bridge, policy == "deny")?;
    }

    // FQDN allowlist — learnt live from DNS (DNS-snooping), adds `*.host`.
    for host in &spec.fqdn_allowlist {
        infra::set_egress_host(&bridge, host)?;
    }

    // L4 rate-limit (GLOBAL — not per-network). `{0,0}` = EXPLICITLY turn off the
    // guard (clear_l4_guard), not "l4guard 0 0" (whose zero semantics is ambiguous).
    if let Some(rl) = &spec.rate_limit {
        if rl.conn_rate == 0 && rl.conn_max == 0 {
            infra::clear_l4_guard()?;
        } else {
            infra::set_l4_guard(rl.conn_rate, rl.conn_max)?;
        }
    }

    let extras = format!(
        "{} CIDR + {} FQDN{}",
        spec.allow_cidrs.len(),
        spec.fqdn_allowlist.len(),
        if spec.rate_limit.is_some() {
            " + rateLimit"
        } else {
            ""
        }
    );
    println!(
        "{kind}/{name}: egress por-rede aplicado a '{}' (default {policy}, {extras})",
        spec.target
    );
    Ok(())
}

/// A valid hostname/FQDN for the egress allowlist (alphanumeric labels +
/// hyphen, separated by `.`, ≤253). Rejects anything that could inject into an nft set.
fn fw_host_ok(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

/// `egress show <net>` — the network's egress policy (CIDR allowlist + FQDN hosts
/// + the IPs currently learnt from DNS for those hosts).
fn egress_show(network: &str) -> Result<()> {
    let def = infra::network_get(network)
        .ok_or_else(|| Error::NotFound(format!("network '{network}'")))?;
    let policy = def
        .egress
        .policy
        .as_deref()
        .unwrap_or("allow (default — no egress restriction)");
    println!(
        "egress for network {} (bridge {}):",
        output::bold(network),
        def.bridge
    );
    println!("  policy: {policy}");
    if def.egress.hosts.is_empty() {
        println!("  FQDN allowlist: (none)");
    } else {
        println!("  FQDN allowlist ({} host(s)):", def.egress.hosts.len());
        for h in &def.egress.hosts {
            println!("    {h}  (and *.{h})");
        }
        let learnt = infra::egress_members(&def.bridge);
        if learnt.is_empty() {
            println!("  learnt IPs (live): (none yet — resolve a host from a container)");
        } else {
            println!("  learnt IPs (live): {}", learnt.join(", "));
        }
    }
    Ok(())
}

/// `egress host <net> <hostname>` — FQDN allowlist for a network's egress.
fn egress_host(network: &str, hostname: &str) -> Result<()> {
    let bridge = infra::resolve_net(network)?.0;
    infra::set_egress_host(&bridge, hostname)?;
    println!(
        "network {network}: egress now allows {} (and *.{}) — learnt live from DNS",
        hostname, hostname
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(dir: &str, proto: &str, port: &str, src: &str, action: &str) -> FwRule {
        FwRule {
            dir: dir.into(),
            proto: proto.into(),
            port: port.into(),
            src: src.into(),
            action: action.into(),
            note: String::new(),
        }
    }

    // Bug-report regression: `deny 8069` followed by `allow 8069` accumulated and
    // the deny (above, first-match) won forever. The replacement compares with
    // norm_any: source ""/"0.0.0.0/0"/"*" are the same match.
    #[test]
    fn norm_any_iguala_as_tres_formas_de_qualquer_origem() {
        assert_eq!(norm_any(""), norm_any("0.0.0.0/0"));
        assert_eq!(norm_any(""), norm_any("*"));
        assert_eq!(norm_any("10.0.0.0/8"), "10.0.0.0/8");
    }

    #[test]
    fn field_overlaps_apanha_coringas_e_iguais() {
        // `deny any/8069` shadows `allow tcp/8069` — the warning must fire.
        assert!(field_overlaps("any", "tcp", &["any", ""]));
        assert!(field_overlaps("8069", "8069", &["*", ""]));
        assert!(field_overlaps("*", "8069", &["*", ""]));
        assert!(!field_overlaps("tcp", "udp", &["any", ""]));
        assert!(!field_overlaps("8069", "5432", &["*", ""]));
    }

    #[test]
    fn rule_spec_reproduz_o_formato_do_cli() {
        assert_eq!(rule_spec(&rule("in", "any", "8069", "", "deny")), "8069");
        assert_eq!(
            rule_spec(&rule("in", "tcp", "5432", "", "allow")),
            "tcp/5432"
        );
    }

    fn net_spec(policy: &str, cidrs: &[&str], fqdns: &[&str], rules: Vec<FwDocRule>) -> FwDocSpec {
        FwDocSpec {
            direction: None,
            scope: Some("network".into()),
            target: "n".into(),
            default_policy: Some(policy.into()),
            rules,
            allow_cidrs: cidrs.iter().map(|s| s.to_string()).collect(),
            fqdn_allowlist: fqdns.iter().map(|s| s.to_string()).collect(),
            rate_limit: None,
        }
    }

    #[test]
    fn network_egress_recusa_allowlist_com_policy_allow() {
        // #1: allow + allowlist = restriction only in appearance → clear error.
        let e = apply_network_egress(
            "Egress",
            "e",
            &net_spec("allow", &["10.0.0.0/8"], &[], vec![]),
        )
        .unwrap_err();
        assert!(
            e.to_string()
                .contains("só fazem sentido com defaultPolicy: deny"),
            "{e}"
        );
        let e = apply_network_egress(
            "Egress",
            "e",
            &net_spec("allow", &[], &["github.com"], vec![]),
        )
        .unwrap_err();
        assert!(
            e.to_string()
                .contains("só fazem sentido com defaultPolicy: deny"),
            "{e}"
        );
    }

    #[test]
    fn network_egress_valida_tudo_antes_de_tocar_no_motor() {
        // These errors fire BEFORE resolve_net (which would need the ingress
        // running) — pure validation, testable without infra.
        // #3: invalid CIDR.
        assert!(
            apply_network_egress("Egress", "e", &net_spec("deny", &["nope"], &[], vec![]))
                .unwrap_err()
                .to_string()
                .contains("invalid CIDR")
        );
        // #3: invalid FQDN (injection).
        assert!(
            apply_network_egress("Egress", "e", &net_spec("deny", &[], &["x;rm -rf"], vec![]))
                .unwrap_err()
                .to_string()
                .contains("hostname inválido")
        );
        // `rules` in scope network.
        let rules = vec![FwDocRule {
            proto: None,
            port: "80".into(),
            from: None,
            to: None,
            action: None,
            note: None,
        }];
        assert!(
            apply_network_egress("Egress", "e", &net_spec("deny", &[], &[], rules))
                .unwrap_err()
                .to_string()
                .contains("`rules` é só para scope: container")
        );
    }

    #[test]
    fn fw_host_ok_aceita_fqdn_valido_recusa_lixo() {
        assert!(fw_host_ok("github.com"));
        assert!(fw_host_ok("sub.dominio-x.example.co"));
        assert!(!fw_host_ok("")); // empty
        assert!(!fw_host_ok("a b.com")); // space
        assert!(!fw_host_ok("x;rm -rf.com")); // injection
        assert!(!fw_host_ok("-lead.com")); // label starts with a hyphen
        assert!(!fw_host_ok("trail-.com")); // label ends with a hyphen
        assert!(!fw_host_ok("a..b")); // empty label
    }

    #[test]
    fn parse_port_spec_defaults_proto_to_any() {
        assert_eq!(
            parse_port_spec("5432").unwrap(),
            ("any".into(), "5432".into())
        );
        assert_eq!(
            parse_port_spec("tcp/5432").unwrap(),
            ("tcp".into(), "5432".into())
        );
        assert_eq!(
            parse_port_spec("udp/*").unwrap(),
            ("udp".into(), "*".into())
        );
    }

    #[test]
    fn parse_port_spec_rejects_bad_proto_and_port() {
        assert!(parse_port_spec("sctp/80").is_err());
        assert!(parse_port_spec("tcp/99999").is_err());
    }
}
