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
        EgressCmd::Show { network } => egress_show(&network),
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
    /// `container` (default) ou `network`. Em `network` (só `Egress`), o `target`
    /// é o NOME DE UMA REDE e aplica-se a política de egress por-rede + allowlist
    /// de CIDR/FQDN + rate-limit L4 — não regras L4 por-container.
    #[serde(default)]
    scope: Option<String>,
    /// `container` (default): nome do container. `network`: nome da rede.
    target: String,
    /// `allow` or `deny` when no rule matches. Default `deny` (allowlist).
    #[serde(default, rename = "defaultPolicy")]
    default_policy: Option<String>,
    #[serde(default)]
    rules: Vec<FwDocRule>,
    // ---- só `scope: network` (Egress por-rede) --------------------------------
    /// CIDRs permitidos quando `defaultPolicy: deny` (allowlist de saída, além do
    /// DNS). Traduz para `set_egress_policy_net_allowlist`.
    #[serde(default, rename = "allowCidrs")]
    allow_cidrs: Vec<String>,
    /// FQDNs permitidos (e `*.fqdn`), aprendidos AO VIVO do DNS (DNS-snooping).
    /// Traduz para `set_egress_host` por host.
    #[serde(default, rename = "fqdnAllowlist")]
    fqdn_allowlist: Vec<String>,
    /// Protecção L4 (conn-rate/conn-max) — **GLOBAL** ao ingress rootless, não
    /// por-rede (a API do motor `set_l4_guard` é global). Traduz para `set_l4_guard`.
    #[serde(default, rename = "rateLimit")]
    rate_limit: Option<RateLimitSpec>,
}

/// `spec.rateLimit` — a protecção DDoS L4 do ingress (global). `{connRate: 0,
/// connMax: 0}` DESLIGA o guard explicitamente (clear_l4_guard).
#[derive(Deserialize)]
struct RateLimitSpec {
    /// Novas conexões por segundo permitidas.
    #[serde(default, rename = "connRate")]
    conn_rate: u32,
    /// Máximo de conexões concorrentes.
    #[serde(default, rename = "connMax")]
    conn_max: u32,
}

/// Nomes aceites no `spec` de `kind: Ingress`/`Egress`, para o aviso de campos
/// desconhecidos (o `rules[]` é validado pela desserialização de `FwDocRule`).
pub(crate) const FW_SPEC_FIELDS: &[&str] =
    &["direction", "scope", "target", "defaultPolicy", "rules", "allowCidrs", "fqdnAllowlist", "rateLimit"];

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
    apply_kind(&store, docs, "in")?; // kind: Ingress
    apply_kind(&store, docs, "out")?; // kind: Egress
    // kind: FirewallPolicy — a forma UNIFICADA (a direcção vem do `spec.direction`
    // em vez do nome do Kind, resolvendo a confusão de que aqui `Ingress` é
    // firewall L4, não o Ingress L7/HTTP do k8s). Aplica a MESMA lógica.
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
    // `dir` "in" → o Kind Ingress; "out" → o Kind Egress.
    let kind = if dir == "in" { "Ingress" } else { "Egress" };
    for doc in manifest::of_kind(docs, kind) {
        apply_fw_doc(store, doc, dir)?;
    }
    Ok(())
}

/// Aplica UM documento de firewall (Ingress/Egress/FirewallPolicy) na direcção
/// `dir` ("in"/"out"). O rótulo nas mensagens usa o Kind real do documento.
fn apply_fw_doc(store: &Store, doc: &ManifestDoc, dir: &str) -> Result<()> {
    let kind = doc.kind.as_str();
    manifest::warn_unknown_fields(doc, FW_SPEC_FIELDS);
    let spec: FwDocSpec = manifest::spec_of(doc)?;

    // Valida o scope explicitamente — uma gralha (`netowrk`) não pode cair em
    // silêncio no caminho container e falhar depois com 'container não existe'.
    let scope = spec.scope.as_deref().unwrap_or("container");
    if !matches!(scope, "container" | "network") {
        return Err(Error::Invalid(format!(
            "{kind}/{}: scope inválido '{scope}' (usa container|network)",
            doc.metadata.name
        )));
    }

    // scope: network — política de egress POR-REDE (Egress apenas). O `target`
    // é o nome de uma rede; liga às APIs de motor que só tinham CLI.
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
    Ok(())
}

/// Aplica um `Egress` de `scope: network` — política de egress por-rede + CIDR/
/// FQDN allowlist + rate-limit L4. Espelha exactamente o `egress net`/`egress
/// host`/`l4guard` da CLI, mas de forma declarativa. **Estado desejado**: cada
/// campo é aplicado tal como está no documento.
fn apply_network_egress(kind: &str, name: &str, spec: &FwDocSpec) -> Result<()> {
    if !spec.rules.is_empty() {
        return Err(Error::Invalid(format!(
            "{kind}/{name}: `rules` é só para scope: container — em scope: network usa allowCidrs/fqdnAllowlist"
        )));
    }
    let policy = spec.default_policy.as_deref().unwrap_or("allow");
    if !matches!(policy, "allow" | "deny") {
        return Err(Error::Invalid(format!("{kind}/{name}: defaultPolicy must be allow|deny")));
    }
    // A allowlist (CIDR/FQDN) SÓ tem efeito com `deny` — com `allow` a saída fica
    // aberta e a lista seria descartada em silêncio (o utilizador pensaria que
    // fechou a rede). Erro claro em vez de aparência falsa de restrição.
    if policy == "allow" && (!spec.allow_cidrs.is_empty() || !spec.fqdn_allowlist.is_empty()) {
        return Err(Error::Invalid(format!(
            "{kind}/{name}: allowCidrs/fqdnAllowlist só fazem sentido com defaultPolicy: deny (com allow a saída fica aberta)"
        )));
    }
    // VALIDA TUDO antes de aplicar QUALQUER coisa (falha-antes-de-tocar): um CIDR
    // ou FQDN inválido a meio não pode deixar o egress em estado parcial.
    for c in &spec.allow_cidrs {
        if !fw_src_ok(c) {
            return Err(Error::Invalid(format!("{kind}/{name}: invalid CIDR '{c}'")));
        }
    }
    for host in &spec.fqdn_allowlist {
        if !fw_host_ok(host) {
            return Err(Error::Invalid(format!("{kind}/{name}: hostname inválido '{host}'")));
        }
    }

    // A bridge REAL vive no registo do infra (não no NetworkStore) — ver egress_net.
    let bridge = infra::resolve_net(&spec.target)?.0;

    if policy == "deny" && !spec.allow_cidrs.is_empty() {
        // deny + allowCidrs → allowlist (nega tudo excepto DNS + estes CIDRs).
        let cidrs: Vec<&str> = spec.allow_cidrs.iter().map(String::as_str).collect();
        infra::set_egress_policy_net_allowlist(&bridge, &cidrs)?;
    } else {
        // allow → sem restrição; deny (sem CIDRs) → nega tudo (só o DNS passa).
        infra::set_egress_policy_net(&bridge, policy == "deny")?;
    }

    // FQDN allowlist — aprendida ao vivo do DNS (DNS-snooping), acrescenta a `*.host`.
    for host in &spec.fqdn_allowlist {
        infra::set_egress_host(&bridge, host)?;
    }

    // rate-limit L4 (GLOBAL — não por-rede). `{0,0}` = desligar EXPLICITAMENTE o
    // guard (clear_l4_guard), não "l4guard 0 0" (cuja semântica do zero é ambígua).
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
        if spec.rate_limit.is_some() { " + rateLimit" } else { "" }
    );
    println!("{kind}/{name}: egress por-rede aplicado a '{}' (default {policy}, {extras})", spec.target);
    Ok(())
}

/// Um hostname/FQDN válido para a allowlist de egress (labels alfanuméricas +
/// hífen, separadas por `.`, ≤253). Recusa o que possa injectar num set nft.
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
    let def = infra::network_get(network).ok_or_else(|| Error::NotFound(format!("network '{network}'")))?;
    let policy = def.egress.policy.as_deref().unwrap_or("allow (default — no egress restriction)");
    println!("egress for network {} (bridge {}):", output::bold(network), def.bridge);
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
    println!("network {network}: egress now allows {} (and *.{}) — learnt live from DNS", hostname, hostname);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net_spec(policy: &str, cidrs: &[&str], fqdns: &[&str], rules: Vec<FwDocRule>) -> FwDocSpec {
        FwDocSpec {
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
        // #1: allow + allowlist = restrição só na aparência → erro claro.
        let e = apply_network_egress("Egress", "e", &net_spec("allow", &["10.0.0.0/8"], &[], vec![])).unwrap_err();
        assert!(e.to_string().contains("só fazem sentido com defaultPolicy: deny"), "{e}");
        let e = apply_network_egress("Egress", "e", &net_spec("allow", &[], &["github.com"], vec![])).unwrap_err();
        assert!(e.to_string().contains("só fazem sentido com defaultPolicy: deny"), "{e}");
    }

    #[test]
    fn network_egress_valida_tudo_antes_de_tocar_no_motor() {
        // Estes erros disparam ANTES do resolve_net (que precisaria do ingress a
        // correr) — validação pura, testável sem infra.
        // #3: CIDR inválido.
        assert!(apply_network_egress("Egress", "e", &net_spec("deny", &["nope"], &[], vec![]))
            .unwrap_err().to_string().contains("invalid CIDR"));
        // #3: FQDN inválido (injecção).
        assert!(apply_network_egress("Egress", "e", &net_spec("deny", &[], &["x;rm -rf"], vec![]))
            .unwrap_err().to_string().contains("hostname inválido"));
        // `rules` em scope network.
        let rules = vec![FwDocRule { proto: None, port: "80".into(), from: None, to: None, action: None, note: None }];
        assert!(apply_network_egress("Egress", "e", &net_spec("deny", &[], &[], rules))
            .unwrap_err().to_string().contains("`rules` é só para scope: container"));
    }

    #[test]
    fn fw_host_ok_aceita_fqdn_valido_recusa_lixo() {
        assert!(fw_host_ok("github.com"));
        assert!(fw_host_ok("sub.dominio-x.example.co"));
        assert!(!fw_host_ok("")); // vazio
        assert!(!fw_host_ok("a b.com")); // espaço
        assert!(!fw_host_ok("x;rm -rf.com")); // injecção
        assert!(!fw_host_ok("-lead.com")); // label começa por hífen
        assert!(!fw_host_ok("trail-.com")); // label termina por hífen
        assert!(!fw_host_ok("a..b")); // label vazio
    }

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
