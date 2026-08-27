//! `delonix net ingress` / `delonix net egress` — the single firewall surface.
//!
//! Both groups edit ONE source of truth: the per-container [`ContainerFw`]
//! (persisted on the `Container`, enforced as nft rules in the ingress netns).
//! `ingress` owns inbound (`dir=in`) rules + the DNAT publishes; `egress` owns
//! outbound (`dir=out`) rules + the per-network egress-to-Internet policy. A
//! container only has a firewall when it lives on a custom network (it has an
//! IP on the `delonix0` bridge) — `--net host` containers share the host stack
//! and are rejected honestly.

use super::kinds as k;
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
    /// Allow a network's egress to a HOSTNAME (and `*.hostname`). Repeatable.
    ///
    /// Learnt live from DNS answers — the FQDN allowlist nft/CIDR can't
    /// express.
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
    /// Show a NETWORK's egress policy.
    ///
    /// CIDR allowlist, FQDN hosts, and the IPs currently learnt from DNS for
    /// those hosts.
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

/// `delonix net l4guard` — the ingress-wide L4 DDoS guard (per-source
/// connection rate + concurrent-connection cap on `tap0`). Until now this was
/// reachable ONLY through a `kind: Egress`/`FirewallPolicy` manifest with
/// `scope: network` + `rateLimit` — an operator reacting to an ongoing flood
/// had to write and apply a manifest to turn it on. `set`/`clear` expose the
/// same `infra::set_l4_guard`/`clear_l4_guard` the manifest path already
/// calls (zero new dataplane); `status` is new — the guard had no query verb
/// at all before this, so "is it even on" could only be answered by `nft
/// list` inside the holder's netns, which an operator has no route to.
#[derive(Subcommand)]
pub enum L4guardCmd {
    /// Turn the guard on (or update it): new conns/s and concurrent conns, per source IP.
    Set { conn_rate: u32, conn_max: u32 },
    /// Turn the guard off.
    Clear,
    /// Show whether the guard is active, with its drop counters.
    Status,
}

pub fn run_l4guard(cmd: L4guardCmd) -> Result<()> {
    match cmd {
        L4guardCmd::Set {
            conn_rate,
            conn_max,
        } => {
            infra::set_l4_guard(conn_rate, conn_max)?;
            println!(
                "{}",
                super::po::tf(
                    "l4guard: active — up to {rate} new connection(s)/s and {max} concurrent connection(s) per source IP",
                    &[
                        ("rate", &conn_rate.to_string()),
                        ("max", &conn_max.to_string()),
                    ],
                )
            );
            Ok(())
        }
        L4guardCmd::Clear => {
            infra::clear_l4_guard()?;
            println!("{}", super::po::t("l4guard: cleared"));
            Ok(())
        }
        L4guardCmd::Status => {
            let rules = infra::l4_guard_status()?;
            if rules.is_empty() {
                println!("{}", super::po::t("l4guard: not active"));
                return Ok(());
            }
            println!("{}", super::po::t("l4guard: active"));
            for (text, packets, bytes) in rules {
                println!("  {text}  ({packets} packets, {bytes} bytes dropped)");
            }
            Ok(())
        }
    }
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

/// Like `Store::update`, but for a closure that can itself fail — every
/// firewall mutation needs to call `infra::apply_firewall` (a kernel
/// syscall) partway through, and `Store::update`'s own closure only returns
/// a commit/abort `bool`, with no room to propagate an error from inside it.
///
/// BUG FOUND (code review): every firewall mutation in this file used to do
/// a bare `store.load` -> mutate in memory -> `infra::apply_firewall` ->
/// `store.save`, with NO lock held between the read and the write.
/// `Store::update` exists *precisely* to sequence this kind of
/// read-modify-write across processes (`flock`, see its own doc comment) —
/// it just was never used here. Concrete race: two firewall commands against
/// the same container (or a firewall command racing a concurrent reconcile
/// save) both read the same starting state, both apply their own change to
/// the kernel successfully, but only the LAST `save` wins on disk — the
/// other's rule is live in `nft` right now but silently missing from the
/// persisted record, so it vanishes on the next `container start` (which
/// only re-applies what's persisted).
fn update_locked<F>(store: &Store, id_or_name: &str, f: F) -> Result<Container>
where
    F: FnOnce(&mut Container) -> Result<bool>,
{
    let mut err = None;
    let c = store.update(id_or_name, |c| match f(c) {
        Ok(commit) => commit,
        Err(e) => {
            err = Some(e);
            false
        }
    })?;
    match err {
        Some(e) => Err(e),
        None => Ok(c),
    }
}

/// One place to reject a bad CIDR, so every entry point says the same thing — and says
/// the useful thing for the mistake that actually happens: an IPv6 CIDR. It used to
/// pass validation and then surface as a raw nft parse error from deep inside the
/// dataplane, because the ruleset is a v4 `table ip` (reproduced live).
fn check_cidr(src: &str) -> Result<()> {
    if src.is_empty() || fw_src_ok(src) {
        return Ok(());
    }
    Err(Error::Invalid(if src.contains(':') {
        super::po::tf(
            "invalid CIDR '{src}' — IPv6 is not supported (the SDN and the firewall are IPv4 only)",
            &[("src", src)],
        )
    } else {
        super::po::tf(
            "invalid CIDR '{src}' (expected an IPv4 address or CIDR, e.g. 10.0.0.0/8)",
            &[("src", src)],
        )
    }))
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
    check_cidr(&src)?;
    let mut replaced: Vec<String> = Vec::new();
    let mut shadow: Option<(String, String)> = None;
    let c = update_locked(store, name, |c| {
        // Guard only: rejects a container off the SDN. The addresses the firewall is
        // keyed on come from `container_ips` (primary + every additional network).
        require_sdn_ip(c)?;
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
        replaced = fw
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
            note: note.clone().unwrap_or_default(),
        });
        // Shadow: an EARLIER overlapping rule (e.g. `deny any/8069` vs
        // `allow tcp/8069`) with the opposite action still matches first — the new
        // rule never gets evaluated. Warning here avoids the "I applied the allow and
        // it stays blocked" without explanation.
        shadow = fw
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
        super::container::apply_firewall_everywhere(c, &fw)?;
        c.firewall = Some(fw);
        Ok(true)
    })?;
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
    check_cidr(&src)?;
    let mut n = 0usize;
    let c = update_locked(store, name, |c| {
        let ip = require_sdn_ip(c)?;
        let mut fw = c.firewall.clone().unwrap_or_default();
        let rm_match = |r: &FwRule| {
            r.dir == dir
                && (proto == "any" || r.proto == proto)
                && (port == "*" || r.port == port)
                && (norm_any(&src).is_empty() || norm_any(&r.src) == norm_any(&src))
        };
        let before = fw.rules.len();
        fw.rules.retain(|r| !rm_match(r));
        n = before - fw.rules.len();
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
            super::container::apply_firewall_everywhere(c, &fw)?;
        }
        c.firewall = if empty { None } else { Some(fw) };
        Ok(true)
    })?;
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
    let c = update_locked(store, name, |c| {
        // Guard only: rejects a container off the SDN. The addresses the firewall is
        // keyed on come from `container_ips` (primary + every additional network).
        require_sdn_ip(c)?;
        let mut fw = c.firewall.clone().unwrap_or_default();
        fw.enabled = true;
        if dir == "in" {
            fw.policy_in = policy.as_str().to_string();
        } else {
            fw.policy_out = policy.as_str().to_string();
        }
        super::container::apply_firewall_everywhere(c, &fw)?;
        c.firewall = Some(fw);
        Ok(true)
    })?;
    let arrow = if dir == "in" { "inbound" } else { "outbound" };
    println!("{}: default {arrow} policy = {}", c.name, policy.as_str());
    // A `deny` default silently kills every published port whose CONTAINER port no rule
    // covers: the DNAT stays installed, the packet dies in the per-container chain, and
    // nothing in the output said so. Name the ports and the exact command that reopens them.
    if dir == "in" && policy == Action::Deny {
        let fw = c.firewall.clone().unwrap_or_default();
        for p in &c.ports {
            let Ok((_, cont_port, proto)) = delonix_net::parse_publish(p) else {
                continue;
            };
            match published_reach(&fw, &cont_port, &proto) {
                PublishReach::Blocked => println!(
                    "{}",
                    super::po::tf(
                        "warning: published port '{spec}' is now BLOCKED — reopen it with \
                         `delonix net ingress allow {name} {cont_port}` (the CONTAINER port: \
                         DNAT runs before the firewall)",
                        &[("spec", p), ("name", &c.name), ("cont_port", &cont_port)],
                    )
                ),
                // Not a warning: this is the shape a default-deny is usually set up FOR.
                // Saying it out loud still helps, because the source that keeps working
                // is easy to lose track of once the policy flips.
                PublishReach::Sources(s) => println!(
                    "{}",
                    super::po::tf(
                        "published port '{spec}' now answers only {from} \
                         (a client on the host's own loopback arrives as the gateway and will not match)",
                        &[("spec", p), ("from", &s.join(","))],
                    )
                ),
                PublishReach::Open => {}
            }
        }
    }
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
        // Honest POLICY column: a container off the SDN cannot HAVE a firewall
        // (`require_sdn_ip` rejects every mutation for it), so "allow (default)" read as
        // "governed and open" when the truth is "not governed at all".
        let governed = c.ip.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
        let policy = if !governed {
            "n/a (host net)".to_string()
        } else if policy.is_empty() {
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
    // Live counters straight off the dataplane. A firewall that cannot say whether a
    // rule ever matched is half a tool — this is the column an operator reads to tell
    // a rule that is protecting something from one that is dead weight (or worse,
    // silently shadowed by an earlier rule). Empty when the holder is down, in which
    // case the rules still print, with `-` instead of a made-up zero.
    let counters =
        c.ip.as_deref()
            .filter(|s| !s.is_empty())
            .map(delonix_net::infra::fw_counters)
            .unwrap_or_default();
    let hits = |r: &FwRule| match delonix_net::infra::fw_rule_tail(r) {
        Some(tail) => match counters.get(&tail) {
            Some((packets, bytes)) => (packets.to_string(), output::fmt_size(*bytes)),
            None => ("-".into(), "-".into()),
        },
        None => ("-".into(), "-".into()),
    };
    let mut t = output::Table::new(&[
        "PROTO",
        "PORT",
        if dir == "in" { "FROM" } else { "TO" },
        "ACTION",
        "PACKETS",
        "BYTES",
        "NOTE",
    ]);
    for r in fw.rules.iter().filter(|r| r.dir == dir) {
        let (packets, bytes) = hits(r);
        t.row(vec![
            or_any(&r.proto),
            or_any(&r.port),
            or_any(&r.src),
            r.action.clone(),
            packets,
            bytes,
            r.note.clone(),
        ]);
    }
    let mut blocked: Vec<(String, String)> = Vec::new();
    if dir == "in" {
        // A `--net host`/`none` container has no per-container chain at all (see
        // `require_sdn_ip`): its publishes are pure slirp hostfwds, governed by nothing
        // here. Saying `allow` would claim a policy that does not exist.
        let governed = c.ip.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
        for p in &c.ports {
            let (cont_port, proto) = delonix_net::parse_publish(p)
                .map(|(_, cp, pr)| (cp, pr))
                .unwrap_or_else(|_| (String::new(), "tcp".into()));
            let reach = if governed {
                published_reach(&fw, &cont_port, &proto)
            } else {
                PublishReach::Open
            };
            if let PublishReach::Blocked = reach {
                blocked.push((p.clone(), cont_port.clone()));
            }
            let (from, action) = match &reach {
                PublishReach::Open => ("any".to_string(), "allow"),
                PublishReach::Sources(s) => (s.join(","), "allow"),
                PublishReach::Blocked => ("any".to_string(), "BLOCKED"),
            };
            let note = if !governed {
                "DNAT (host net — no firewall)".to_string()
            } else if matches!(reach, PublishReach::Sources(_)) {
                // Worth spelling out, because it is the one case where the FROM column
                // does not describe every client: a request from the host's own
                // loopback reaches the container as the slirp gateway, so it does not
                // match a source rule written for the real client address.
                "DNAT (loopback clients arrive as the gateway)".to_string()
            } else {
                "DNAT".to_string()
            };
            t.row(vec![
                "publish".into(),
                p.clone(),
                from,
                action.to_string(),
                "-".into(),
                "-".into(),
                note,
            ]);
        }
    }
    t.print();
    for (spec, cont_port) in &blocked {
        println!();
        println!(
            "{}",
            super::po::tf(
                "warning: '{spec}' is published (DNAT is in place) but the inbound firewall \
                 drops it — the port answers nothing.",
                &[("spec", spec)],
            )
        );
        println!(
            "{}",
            super::po::tf(
                "  DNAT runs before the firewall, so a rule must name the CONTAINER port: \
                 `delonix net ingress allow {name} {cont_port}`",
                &[("name", &c.name), ("cont_port", cont_port)],
            )
        );
    }
    Ok(())
}

/// Does an inbound rule's port field cover `port`? Mirrors `fw_chain_body`:
/// empty/`*` means every port; otherwise an exact match or a `n-m` range.
fn port_covers(rule_port: &str, port: &str) -> bool {
    if rule_port.is_empty() || rule_port == "*" {
        return true;
    }
    let Ok(p) = port.parse::<u32>() else {
        return rule_port == port;
    };
    match rule_port.split_once('-') {
        Some((a, b)) => match (a.parse::<u32>(), b.parse::<u32>()) {
            (Ok(a), Ok(b)) => (a..=b).contains(&p),
            _ => false,
        },
        None => rule_port.parse::<u32>().map(|r| r == p).unwrap_or(false),
    }
}

/// The EFFECTIVE inbound verdict for a published port, resolved the way the dataplane
/// resolves it — the reason this exists at all: `ingress ls` used to print every publish
/// as `allow / DNAT` unconditionally, so a container under `policy deny` showed its port
/// as open while `curl` got nothing. The table has to answer the question it is read for.
///
/// Two facts drive it: (1) DNAT happens at `prerouting`, so by the time the per-container
/// chain sees the packet the destination port is the CONTAINER port, never the host port
/// — a rule must name `cont_port` to govern a publish; (2) the chain is first-match
/// terminal, so the first covering rule wins over the default policy.
///
/// Source-restricted rules are NOT ignored — they are the third answer. A publish
/// governed by `policy deny` + `allow <port> --from <cidr>` is neither open nor
/// blocked, and calling it `BLOCKED` (which this used to do) is wrong in the most
/// useful configuration there is: expose a port to exactly one network. Source
/// filtering does work on published ports, because the client address survives the
/// hop for every non-loopback client — see [`delonix_net::SLIRP_GW`].
enum PublishReach {
    /// Reachable from anywhere the bind address allows.
    Open,
    /// Reachable only from these sources.
    Sources(Vec<String>),
    /// The firewall drops it — the port answers nothing.
    Blocked,
}

fn published_reach(
    fw: &delonix_runtime_core::ContainerFw,
    cont_port: &str,
    proto: &str,
) -> PublishReach {
    let mut sources: Vec<String> = Vec::new();
    for r in fw.rules.iter().filter(|r| r.dir == "in") {
        let proto_covers = r.proto.is_empty() || r.proto == "any" || r.proto == proto;
        if !proto_covers || !port_covers(&r.port, cont_port) {
            continue;
        }
        let src = norm_any(&r.src);
        if src.is_empty() {
            // A rule with no source is terminal for EVERY source: whatever came
            // before it still stands, nothing after it is ever reached.
            return match (r.action.as_str(), sources.is_empty()) {
                ("allow", _) => PublishReach::Open,
                (_, true) => PublishReach::Blocked,
                (_, false) => PublishReach::Sources(sources),
            };
        }
        if r.action == "allow" {
            sources.push(src.to_string());
        }
    }
    match (fw.policy_in == "deny", sources.is_empty()) {
        (false, _) => PublishReach::Open,
        (true, true) => PublishReach::Blocked,
        (true, false) => PublishReach::Sources(sources),
    }
}

fn or_any(s: &str) -> String {
    if s.is_empty() || s == "*" {
        "any".to_string()
    } else {
        s.to_string()
    }
}

fn clear_dir(store: &Store, name: &str, dir: &str) -> Result<()> {
    let mut removed = 0usize;
    let mut nothing_to_clear = false;
    let c = update_locked(store, name, |c| {
        let mut fw = match c.firewall.clone() {
            Some(f) => f,
            None => {
                nothing_to_clear = true;
                return Ok(false);
            }
        };
        let before = fw.rules.len();
        fw.rules.retain(|r| r.dir != dir);
        removed = before - fw.rules.len();
        // If nothing is left (no rules, both policies default), drop the firewall
        // entirely and detach it from the ingress; otherwise re-apply what remains.
        let empty = fw.rules.is_empty() && fw.policy_in.is_empty() && fw.policy_out.is_empty();
        if let Some(ip) = c.ip.clone().filter(|s| !s.is_empty()) {
            if empty {
                infra::clear_firewall(&ip);
            } else {
                super::container::apply_firewall_everywhere(c, &fw)?;
            }
        }
        c.firewall = if empty { None } else { Some(fw) };
        Ok(true)
    })?;
    if nothing_to_clear {
        println!("{}: no firewall to clear", c.name);
        return Ok(());
    }
    let arrow = if dir == "in" { "inbound" } else { "outbound" };
    println!("{}: removed {removed} {arrow} rule(s)", c.name);
    Ok(())
}

fn egress_net(network: &str, mode: EgressMode, to: Option<String>) -> Result<()> {
    // The REAL bridge lives in the infra registry (NetDef, `dlxn{:08x}`), NOT in
    // the NetworkStore (`dlxn{:02x}{:04x}`) — using the wrong one makes the nft
    // rules never match traffic. resolve_net returns the bridge the holder created.
    let bridge = infra::resolve_net(network)?.bridge;
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
                check_cidr(c)?;
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
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct FwDocSpec {
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
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
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

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
struct FwDocRule {
    /// `tcp`/`udp`/`any` (default `any`).
    #[serde(default)]
    proto: Option<String>,
    /// Port, range `n-m`, or `*`.
    port: String,
    /// Source CIDR (ingress) — the other end of inbound traffic.
    #[serde(default)]
    from: Option<String>,
    /// Destination CIDR (egress) — the other end of outbound traffic.
    #[serde(default)]
    to: Option<String>,
    /// Source **by workload name** (ingress), resolved to that container's SDN
    /// address at apply time.
    ///
    /// Exists because **a container's IP is not stable**: it comes from the SDN
    /// and changes on a restart. A policy written as a CIDR is therefore a policy
    /// that silently stops matching the workload it was written for — the same
    /// lesson `vm bridge` already paid for, where the follow-up recorded is
    /// exactly "discovery by NAME, so as not to depend on dynamic IPs".
    ///
    /// A NAME and not a permissive `from` that also accepts names: a value that
    /// fails to parse as a CIDR falling back to "treat it as a name" would be a
    /// silent reinterpretation on a security field, which is the one place this
    /// engine refuses to guess.
    #[serde(default, rename = "fromWorkload")]
    from_workload: Option<String>,
    /// Destination by workload name (egress). Same reasoning as
    /// [`FwDocRule::from_workload`].
    #[serde(default, rename = "toWorkload")]
    to_workload: Option<String>,
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
    // `kind: FirewallPolicy` is now the ONLY firewall Kind. `kind: Ingress` stopped
    // being firewall a while back (it is the k8s-shaped L7 Ingress, see
    // `cmd::httproute`), and `kind: Egress` is rewritten into a FirewallPolicy with
    // `direction: egress` at load time (`manifest::lower_egress`) — so by the time
    // anything reaches here there is one Kind, one struct and one direction field.
    for doc in manifest::of_kind(docs, k::FIREWALL_POLICY) {
        let dir = match doc.spec.get("direction").and_then(|v| v.as_str()) {
            Some("ingress") => "in",
            Some("egress") => "out",
            other => {
                return Err(Error::Invalid(super::po::tf(
                    "FirewallPolicy/{name}: direction is required and ∈ {{ingress, egress}} (got {other})",
                    &[("name", &doc.metadata.name), ("other", &format!("{other:?}"))],
                )));
            }
        };
        apply_fw_doc(&store, doc, dir)?;
    }
    Ok(())
}

/// Fields the reconciler compares for a `kind: FirewallPolicy`.
///
/// `defaultPolicy` and `rules` converge HOT — `apply_fw_doc` already replaces
/// the whole direction, in place, with no container restart. `target` and
/// `direction` do not: they IDENTIFY which direction of which container this
/// policy governs, so changing one leaves the old target's rules exactly where
/// they were. That is a `Replace`, and the recreation clears the direction on
/// the old target — which is the only way the change means what it reads like.
pub(crate) const RECONCILED_FW_FIELDS: &[&str] =
    &["target", "direction", "defaultPolicy", "rules", "scope"];

/// One rule, rendered as one comparable string.
///
/// Sorted by the caller, never here: the ORDER of allow rules in a policy does
/// not change what it permits (the chain is built from the whole set), so a
/// reordered manifest must not read as a change.
fn rule_key(r: &FwDocRule) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        r.action.as_deref().unwrap_or("allow"),
        r.proto.as_deref().unwrap_or("any"),
        r.port,
        r.from.as_deref().or(r.to.as_deref()).unwrap_or(""),
        r.from_workload
            .as_deref()
            .or(r.to_workload.as_deref())
            .unwrap_or(""),
    )
}

/// The same rendering, from a persisted [`FwRule`].
///
/// A persisted rule holds the RESOLVED address, never the workload name it may
/// have come from — so a policy written with `fromWorkload` compares its
/// resolved `/32` against the record's. That is why the desired side resolves
/// too (below): comparing a name against an address would report drift on every
/// plan for every rule that names a peer.
fn stored_rule_key(r: &delonix_runtime_core::FwRule) -> String {
    format!("{}|{}|{}|{}|", r.action, r.proto, r.port, r.src)
}

/// What the manifest declares, for the reconciler.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: FwDocSpec = manifest::spec_of(doc)?;
    let (_images, store) = open_stores()?;
    let mut f = std::collections::BTreeMap::new();
    f.insert("target".into(), spec.target.clone());
    f.insert(
        "direction".into(),
        spec.direction.clone().unwrap_or_default(),
    );
    f.insert(
        "scope".into(),
        spec.scope.clone().unwrap_or_else(|| "container".into()),
    );
    f.insert(
        "defaultPolicy".into(),
        spec.default_policy.clone().unwrap_or_else(|| "deny".into()),
    );
    let mut keys: Vec<String> = Vec::new();
    for r in &spec.rules {
        // Resolve a workload name to its address, exactly as the apply will —
        // the record stores addresses. A workload that does not exist yet
        // resolves to nothing and the rule compares by name; the policy cannot
        // have been applied yet either, so there is nothing to be wrong about.
        let by_name = r.from_workload.clone().or_else(|| r.to_workload.clone());
        let resolved = by_name
            .as_deref()
            .and_then(|w| workload_cidr(&store, w).ok());
        match resolved {
            Some(cidr) => keys.push(format!(
                "{}|{}|{}|{}|",
                r.action.as_deref().unwrap_or("allow"),
                r.proto.as_deref().unwrap_or("any"),
                r.port,
                cidr
            )),
            None => keys.push(rule_key(r)),
        }
    }
    keys.sort();
    f.insert("rules".into(), keys.join(","));
    Ok(super::reconcile::Desired {
        kind: k::FIREWALL_POLICY.into(),
        name: doc.metadata.name.clone(),
        fields: f,
        converges: true,
        // NOT prunable, and for a different reason than an `Image`: a policy has
        // no record of its own — it lives on the target container's
        // `ContainerFw`. Once it leaves the manifest, nothing on disk says which
        // target and direction it governed, so there is nothing a prune could
        // safely clear. This matches what the firewall docs already promise
        // («removing the Dependency does NOT unprotect the `to`»); what changes
        // is that the plan no longer implies otherwise.
        ownable: false,
    })
}

/// What is on the machine, for the reconciler.
///
/// A firewall policy has no record of its own — it lives on the TARGET
/// container's `ContainerFw`. So the actual side is keyed by the document name
/// (what the plan matches on) and read from the target named by that document.
pub(crate) fn actual(docs: &[ManifestDoc]) -> Result<Vec<super::reconcile::Actual>> {
    let (_images, store) = open_stores()?;
    let mut out = Vec::new();
    for doc in manifest::of_kind(docs, k::FIREWALL_POLICY) {
        let Ok(spec) = manifest::spec_of::<FwDocSpec>(doc) else {
            continue;
        };
        let Ok(c) = store.load(&spec.target) else {
            continue; // target not created yet — the plan will say Create
        };
        let Some(fw) = &c.firewall else { continue };
        let dir = match spec.direction.as_deref() {
            Some("ingress") => "in",
            Some("egress") => "out",
            _ => continue,
        };
        let mut f = std::collections::BTreeMap::new();
        f.insert("target".into(), spec.target.clone());
        f.insert(
            "direction".into(),
            spec.direction.clone().unwrap_or_default(),
        );
        f.insert(
            "scope".into(),
            spec.scope.clone().unwrap_or_else(|| "container".into()),
        );
        let policy = if dir == "in" {
            &fw.policy_in
        } else {
            &fw.policy_out
        };
        f.insert(
            "defaultPolicy".into(),
            if policy.is_empty() {
                "deny".into()
            } else {
                policy.clone()
            },
        );
        let mut keys: Vec<String> = fw
            .rules
            .iter()
            .filter(|r| r.dir == dir)
            .map(stored_rule_key)
            .collect();
        keys.sort();
        f.insert("rules".into(), keys.join(","));
        out.push(super::reconcile::Actual {
            kind: k::FIREWALL_POLICY.into(),
            name: doc.metadata.name.clone(),
            fields: f,
            owner: c.labels.get(super::reconcile::STACK_LABEL).cloned(),
            last_applied: c
                .annotations
                .get(&format!("{}/{}", super::reconcile::LAST_APPLIED, dir))
                .and_then(|raw| super::reconcile::decode_last_applied(raw)),
        });
    }
    Ok(out)
}

/// Converges a policy: re-apply the document. `apply_fw_doc` already replaces
/// the whole direction, so «converging» is exactly «applying» — there is no
/// partial path to write, and writing one would be a second way to build the
/// same chain.
pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    let (_images, store) = open_stores()?;
    let dir = match doc.spec.get("direction").and_then(|v| v.as_str()) {
        Some("ingress") => "in",
        Some("egress") => "out",
        _ => return Ok(()),
    };
    apply_fw_doc(&store, doc, dir)
}

/// Resolves a workload name to the `/32` of its SDN address.
///
/// Fails LOUDLY when the container has no address, and says why: a workload on
/// `--net host`/`none` has no address on the SDN for a rule to name, and
/// producing an empty source instead would silently widen the rule to
/// `0.0.0.0/0` — turning "allow this one peer" into "allow everyone", which is
/// the worst possible way for a firewall to fail.
fn workload_cidr(store: &Store, name: &str) -> Result<String> {
    let c = store
        .list()?
        .into_iter()
        .find(|c| c.name == name)
        .ok_or_else(|| {
            Error::Invalid(super::po::tf(
                "workload '{name}' does not exist (a rule names it as the other end)",
                &[("name", name)],
            ))
        })?;
    let ip = c.ip.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| {
        Error::Invalid(super::po::tf(
            "workload '{name}' has no address on the SDN (is it on a custom network?) — \
             a rule cannot name it",
            &[("name", name)],
        ))
    })?;
    Ok(format!("{ip}/32"))
}

/// Applies ONE firewall document (Ingress/Egress/FirewallPolicy) in the `dir`
/// direction ("in"/"out"). The label in messages uses the document's real Kind.
fn apply_fw_doc(store: &Store, doc: &ManifestDoc, dir: &str) -> Result<()> {
    let kind = doc.kind.as_str();
    let spec: FwDocSpec = manifest::spec_of(doc)?;

    // Validate the scope explicitly — a typo (`netowrk`) must not fall silently
    // into the container path and fail later with 'container does not exist'.
    let scope = spec.scope.as_deref().unwrap_or("container");
    if !matches!(scope, "container" | "network") {
        return Err(Error::Invalid(super::po::tf(
            "{kind}/{name}: invalid scope '{scope}' (use container|network)",
            &[
                ("kind", kind),
                ("name", &doc.metadata.name),
                ("scope", scope),
            ],
        )));
    }

    // scope: network — PER-NETWORK egress policy (Egress only). The `target`
    // is a network name; wires up the engine APIs that only had a CLI.
    if scope == "network" {
        if dir != "out" {
            return Err(Error::Invalid(super::po::tf(
                "{kind}/{name}: scope: network is only supported in Egress (there is no per-network INGRESS policy)",
                &[("kind", kind), ("name", &doc.metadata.name)],
            )));
        }
        return apply_network_egress(kind, &doc.metadata.name, &spec);
    }

    // Pure spec validation first (no container/lock involved) — fail fast on
    // a bad manifest before ever touching the store.
    let policy = spec.default_policy.as_deref().unwrap_or("deny");
    if !matches!(policy, "allow" | "deny") {
        return Err(Error::Invalid(format!(
            "{kind}/{}: defaultPolicy must be allow|deny",
            doc.metadata.name
        )));
    }
    let mut new_rules = Vec::new();
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
        // A rule names the other end EITHER by address or by workload, never
        // both — two answers to "who is the other end" is a contradiction, and
        // on a firewall a contradiction is not something to resolve by
        // precedence.
        let by_name = r.from_workload.clone().or_else(|| r.to_workload.clone());
        if by_name.is_some() && (r.from.is_some() || r.to.is_some()) {
            return Err(Error::Invalid(super::po::tf(
                "{kind}/{name}: a rule uses both an address (from/to) and a workload \
                 (fromWorkload/toWorkload) — pick one",
                &[("kind", kind), ("name", &doc.metadata.name)],
            )));
        }
        let src = match &by_name {
            Some(w) => workload_cidr(store, w)
                .map_err(|e| Error::Invalid(format!("{kind}/{}: {e}", doc.metadata.name)))?,
            None => {
                let s = r.from.clone().or_else(|| r.to.clone()).unwrap_or_default();
                if let Err(e) = check_cidr(&s) {
                    return Err(Error::Invalid(format!("{kind}/{}: {e}", doc.metadata.name)));
                }
                s
            }
        };
        let action = r.action.clone().unwrap_or_else(|| "allow".into());
        if !matches!(action.as_str(), "allow" | "deny") {
            return Err(Error::Invalid(format!(
                "{kind}/{}: action must be allow|deny",
                doc.metadata.name
            )));
        }
        new_rules.push(FwRule {
            dir: dir.to_string(),
            proto,
            port: r.port.clone(),
            src,
            action,
            note: r.note.clone().unwrap_or_default(),
        });
    }
    let mut n = 0usize;
    update_locked(store, &spec.target, |c| {
        // Guard only: rejects a container off the SDN. The addresses the firewall is
        // keyed on come from `container_ips` (primary + every additional network).
        require_sdn_ip(c)?;
        let mut fw = c.firewall.clone().unwrap_or_default();
        fw.enabled = true;
        // Declarative: this direction is fully replaced by the document.
        fw.rules.retain(|r| r.dir != dir);
        if dir == "in" {
            fw.policy_in = policy.to_string();
        } else {
            fw.policy_out = policy.to_string();
        }
        fw.rules.extend(new_rules.iter().cloned());
        super::container::apply_firewall_everywhere(c, &fw)?;
        n = fw.rules.iter().filter(|r| r.dir == dir).count();
        c.firewall = Some(fw);
        Ok(true)
    })?;
    println!(
        "{kind}/{}: applied to {} ({n} rule(s), default {policy})",
        doc.metadata.name, spec.target
    );
    Ok(())
}

/// Applies a `scope: network` `Egress` — per-network egress policy + CIDR/
/// FQDN allowlist + L4 rate-limit. Mirrors exactly the CLI's `egress net`/`egress
/// host`/`l4guard`, but declaratively. **Desired state**: each field is applied
/// exactly as it stands in the document.
fn apply_network_egress(kind: &str, name: &str, spec: &FwDocSpec) -> Result<()> {
    if !spec.rules.is_empty() {
        return Err(Error::Invalid(super::po::tf(
            "{kind}/{name}: `rules` is only for scope: container — in scope: network use allowCidrs/fqdnAllowlist",
            &[("kind", kind), ("name", name)],
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
        return Err(Error::Invalid(super::po::tf(
            "{kind}/{name}: allowCidrs/fqdnAllowlist only make sense with defaultPolicy: deny (with allow, egress stays open)",
            &[("kind", kind), ("name", name)],
        )));
    }
    // VALIDATE EVERYTHING before applying ANYTHING (fail-before-touching): an
    // invalid CIDR or FQDN midway must not leave egress in a partial state.
    for c in &spec.allow_cidrs {
        if let Err(e) = check_cidr(c) {
            return Err(Error::Invalid(format!("{kind}/{name}: {e}")));
        }
    }
    for host in &spec.fqdn_allowlist {
        if !fw_host_ok(host) {
            return Err(Error::Invalid(super::po::tf(
                "{kind}/{name}: invalid hostname '{host}'",
                &[("kind", kind), ("name", name), ("host", host)],
            )));
        }
    }

    // The REAL bridge lives in the infra registry (not the NetworkStore) — see egress_net.
    let bridge = infra::resolve_net(&spec.target)?.bridge;

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
        "{}",
        super::po::tf(
            "{kind}/{name}: per-network egress applied to '{target}' (default {policy}, {extras})",
            &[
                ("kind", kind),
                ("name", name),
                ("target", &spec.target),
                ("policy", policy),
                ("extras", &extras),
            ],
        )
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
    let bridge = infra::resolve_net(network)?.bridge;
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
                .contains("only make sense with defaultPolicy: deny"),
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
                .contains("only make sense with defaultPolicy: deny"),
            "{e}"
        );
    }

    /// A publish under `policy deny` + `allow <port> --from <cidr>` is the single most
    /// useful shape there is (expose a port to exactly one network) and it used to be
    /// reported as `BLOCKED`, with a warning claiming "the port answers nothing" —
    /// while it answered that source perfectly well. Validated live before and after:
    /// allowed source 200, other source nothing.
    #[test]
    fn publish_restrito_a_uma_origem_nao_e_bloqueado() {
        let rule = |port: &str, src: &str, action: &str| FwRule {
            dir: "in".into(),
            proto: "any".into(),
            port: port.into(),
            src: src.into(),
            action: action.into(),
            note: String::new(),
        };
        let fw = |policy: &str, rules: Vec<FwRule>| delonix_runtime_core::ContainerFw {
            enabled: true,
            policy_in: policy.into(),
            policy_out: String::new(),
            rules,
            namespace: "default".into(),
        };
        // deny + a source-restricted allow → reachable, from that source.
        match published_reach(
            &fw("deny", vec![rule("80", "10.0.0.0/8", "allow")]),
            "80",
            "tcp",
        ) {
            PublishReach::Sources(s) => assert_eq!(s, vec!["10.0.0.0/8"]),
            _ => panic!("a source-restricted publish is neither open nor blocked"),
        }
        // deny with nothing covering the port → genuinely blocked.
        assert!(matches!(
            published_reach(&fw("deny", vec![]), "80", "tcp"),
            PublishReach::Blocked
        ));
        // A general allow covering the port opens it to everyone.
        assert!(matches!(
            published_reach(&fw("deny", vec![rule("80", "", "allow")]), "80", "tcp"),
            PublishReach::Open
        ));
        // First-match terminal: a general DENY placed BEFORE the source rule wins for
        // every source, so nothing gets through.
        assert!(matches!(
            published_reach(
                &fw(
                    "allow",
                    vec![rule("80", "", "deny"), rule("80", "10.0.0.0/8", "allow")]
                ),
                "80",
                "tcp"
            ),
            PublishReach::Blocked
        ));
        // ...and placed AFTER it, the source that was already allowed keeps working.
        match published_reach(
            &fw(
                "allow",
                vec![rule("80", "10.0.0.0/8", "allow"), rule("80", "", "deny")],
            ),
            "80",
            "tcp",
        ) {
            PublishReach::Sources(s) => assert_eq!(s, vec!["10.0.0.0/8"]),
            _ => panic!("the earlier source rule still matches first"),
        }
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
                .contains("invalid hostname")
        );
        // `rules` in scope network.
        let rules = vec![FwDocRule {
            from_workload: None,
            to_workload: None,
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
                .contains("`rules` is only for scope: container")
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
