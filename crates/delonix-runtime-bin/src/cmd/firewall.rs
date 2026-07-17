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
use delonix_runtime_core::{fw_port_ok, fw_proto_ok, fw_src_ok, Container, Error, FwRule, Result, Store};
use serde::Deserialize;

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
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
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
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
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
        IngressCmd::Allow { container, port, from, note } => add_rule(&store, &container, "in", Action::Allow, &port, from, note),
        IngressCmd::Deny { container, port, from, note } => add_rule(&store, &container, "in", Action::Deny, &port, from, note),
        IngressCmd::Policy { container, policy } => set_policy(&store, &container, "in", policy),
        IngressCmd::Publish { container, spec } => {
            let mut c = store.load(&container)?;
            super::container::publish_live(&store, &mut c, &spec)
        }
        IngressCmd::Unpublish { container, host_port } => {
            let mut c = store.load(&container)?;
            super::container::unpublish_live(&store, &mut c, &host_port)
        }
        IngressCmd::Ls { container } => list_rules(&store, &container, "in"),
        IngressCmd::Clear { container } => clear_dir(&store, &container, "in"),
    }
}

pub fn run_egress(cmd: EgressCmd) -> Result<()> {
    let (_images, store) = open_stores()?;
    match cmd {
        EgressCmd::Allow { container, port, to, note } => add_rule(&store, &container, "out", Action::Allow, &port, to, note),
        EgressCmd::Deny { container, port, to, note } => add_rule(&store, &container, "out", Action::Deny, &port, to, note),
        EgressCmd::Policy { container, policy } => set_policy(&store, &container, "out", policy),
        EgressCmd::Net { network, mode, to } => egress_net(&network, mode, to),
        EgressCmd::Host { network, hostname } => egress_host(&network, &hostname),
        EgressCmd::Ls { container } => list_rules(&store, &container, "out"),
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
        return Err(Error::Invalid(format!("invalid proto '{proto}' (tcp|udp|any)")));
    }
    if !fw_port_ok(&port) {
        return Err(Error::Invalid(format!("invalid port '{port}' (1-65535, a range n-m, or *)")));
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

fn add_rule(store: &Store, name: &str, dir: &str, action: Action, port_spec: &str, cidr: Option<String>, note: Option<String>) -> Result<()> {
    let (proto, port) = parse_port_spec(port_spec)?;
    let src = cidr.unwrap_or_default();
    if !src.is_empty() && !fw_src_ok(&src) {
        return Err(Error::Invalid(format!("invalid CIDR '{src}'")));
    }
    let mut c = store.load(name)?;
    let ip = require_sdn_ip(&c)?;
    let mut fw = c.firewall.clone().unwrap_or_default();
    fw.enabled = true;
    fw.rules.push(FwRule {
        dir: dir.to_string(),
        proto,
        port,
        src,
        action: action.as_str().to_string(),
        note: note.unwrap_or_default(),
    });
    infra::apply_firewall(&c.id, &ip, &fw)?;
    c.firewall = Some(fw);
    store.save(&c)?;
    let arrow = if dir == "in" { "inbound" } else { "outbound" };
    println!("{}: {arrow} rule added ({})", c.name, output::bold(&format!("{} {port_spec}", action.as_str())));
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

fn list_rules(store: &Store, name: &str, dir: &str) -> Result<()> {
    let c = store.load(name)?;
    let fw = c.firewall.clone().unwrap_or_default();
    let policy = if dir == "in" { &fw.policy_in } else { &fw.policy_out };
    let default = if policy.is_empty() { "allow (default)" } else { policy.as_str() };
    let arrow = if dir == "in" { "INBOUND" } else { "OUTBOUND" };
    println!("{} firewall for {} — default policy: {}", arrow, c.name, default);
    let mut t = output::Table::new(&["PROTO", "PORT", if dir == "in" { "FROM" } else { "TO" }, "ACTION", "NOTE"]);
    for r in fw.rules.iter().filter(|r| r.dir == dir) {
        t.row(vec![or_any(&r.proto), or_any(&r.port), or_any(&r.src), r.action.clone(), r.note.clone()]);
    }
    if dir == "in" {
        for p in &c.ports {
            t.row(vec!["publish".into(), p.clone(), "0.0.0.0/0".into(), "allow".into(), "DNAT".into()]);
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
    // A bridge REAL vive no registo do infra (NetDef, `dlxn{:08x}`), NÃO no
    // NetworkStore (`dlxn{:02x}{:04x}`) — usar a errada faz as regras nft nunca
    // casarem o tráfego. resolve_net devolve a bridge que o holder criou.
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
            let raw = to.ok_or_else(|| Error::Invalid("allowlist mode needs `--to <cidr,...>`".into()))?;
            let cidrs: Vec<&str> = raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            for c in &cidrs {
                if !fw_src_ok(c) {
                    return Err(Error::Invalid(format!("invalid CIDR '{c}'")));
                }
            }
            infra::set_egress_policy_net_allowlist(&bridge, &cidrs)?;
            println!("network {network}: egress DENIED except DNS + {}", cidrs.join(", "));
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
#[derive(Deserialize)]
struct FwDocSpec {
    /// Target container (name). Must exist and be on a custom network.
    target: String,
    /// `allow` or `deny` when no rule matches. Default `deny` (allowlist).
    #[serde(default, rename = "defaultPolicy")]
    default_policy: Option<String>,
    #[serde(default)]
    rules: Vec<FwDocRule>,
}

#[derive(Deserialize)]
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
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (_images, store) = open_stores()?;
    apply_kind(&store, docs, "Ingress", "in")?;
    apply_kind(&store, docs, "Egress", "out")?;
    Ok(())
}

fn apply_kind(store: &Store, docs: &[ManifestDoc], kind: &str, dir: &str) -> Result<()> {
    for doc in manifest::of_kind(docs, kind) {
        let spec: FwDocSpec = manifest::spec_of(doc)?;
        let mut c = store.load(&spec.target)?;
        let ip = require_sdn_ip(&c)?;
        let mut fw = c.firewall.clone().unwrap_or_default();
        fw.enabled = true;
        // Declarative: this direction is fully replaced by the document.
        fw.rules.retain(|r| r.dir != dir);
        let policy = spec.default_policy.as_deref().unwrap_or("deny");
        if !matches!(policy, "allow" | "deny") {
            return Err(Error::Invalid(format!("{kind}/{}: defaultPolicy must be allow|deny", doc.metadata.name)));
        }
        if dir == "in" {
            fw.policy_in = policy.to_string();
        } else {
            fw.policy_out = policy.to_string();
        }
        for r in &spec.rules {
            let proto = r.proto.clone().unwrap_or_else(|| "any".into());
            if !fw_proto_ok(&proto) {
                return Err(Error::Invalid(format!("{kind}/{}: invalid proto '{proto}'", doc.metadata.name)));
            }
            if !fw_port_ok(&r.port) {
                return Err(Error::Invalid(format!("{kind}/{}: invalid port '{}'", doc.metadata.name, r.port)));
            }
            let src = r.from.clone().or_else(|| r.to.clone()).unwrap_or_default();
            if !src.is_empty() && !fw_src_ok(&src) {
                return Err(Error::Invalid(format!("{kind}/{}: invalid CIDR '{src}'", doc.metadata.name)));
            }
            let action = r.action.clone().unwrap_or_else(|| "allow".into());
            if !matches!(action.as_str(), "allow" | "deny") {
                return Err(Error::Invalid(format!("{kind}/{}: action must be allow|deny", doc.metadata.name)));
            }
            fw.rules.push(FwRule { dir: dir.to_string(), proto, port: r.port.clone(), src, action, note: r.note.clone().unwrap_or_default() });
        }
        infra::apply_firewall(&c.id, &ip, &fw)?;
        let n = fw.rules.iter().filter(|r| r.dir == dir).count();
        c.firewall = Some(fw);
        store.save(&c)?;
        println!("{kind}/{}: applied to {} ({n} rule(s), default {policy})", doc.metadata.name, spec.target);
    }
    Ok(())
}

/// `egress host <net> <hostname>` — FQDN allowlist for a network's egress.
fn egress_host(network: &str, hostname: &str) -> Result<()> {
    let bridge = infra::resolve_net(network)?.0;
    infra::set_egress_host(&bridge, hostname)?;
    println!("network {network}: egress now allows {} (and *.{}) — learnt live from DNS", hostname, hostname);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_port_spec_defaults_proto_to_any() {
        assert_eq!(parse_port_spec("5432").unwrap(), ("any".into(), "5432".into()));
        assert_eq!(parse_port_spec("tcp/5432").unwrap(), ("tcp".into(), "5432".into()));
        assert_eq!(parse_port_spec("udp/*").unwrap(), ("udp".into(), "*".into()));
    }

    #[test]
    fn parse_port_spec_rejects_bad_proto_and_port() {
        assert!(parse_port_spec("sctp/80").is_err());
        assert!(parse_port_spec("tcp/99999").is_err());
    }
}
