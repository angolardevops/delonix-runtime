//! `kind: Dependency` (alias `KnowDepends`) — **DIRECTED** reachability between
//! containers/VMs. Unlike a `Network` (bidirectional communication), a
//! dependency opens ONE direction: `from` reaches `to`, but `to` does **not**
//! initiate towards `from`. It is the "the app knows the DB, the DB does not
//! know the app" case — the DB stops being exposed to every container of a
//! shared network.
//!
//! **How it works (sugar over the per-container L4 firewall):** declaring
//! `Dependency { from: app, to: [db] }` compiles to, on `db`: ingress
//! **default-deny** (protects the DB from the WHOLE SDN) + an `allow` for
//! `app`'s IP. The reverse direction (db→app) is never opened, and the return
//! of the app↔db conversation flows because the SDN is stateful (`ct state
//! established,related accept`). Reuses the same `ContainerFw`/`infra::apply_firewall`
//! as `kind: FirewallPolicy` — zero new dataplane. Multiple `Dependency` for the
//! same `to` ACCUMULATE the `allow`s.
//!
//! **Teardown ("ensure present", not a reconciler):** removing the `Dependency`
//! from a manifest and reapplying does NOT unprotect the `to` — the default-deny
//! ingress stays (same L4 firewall as `kind: FirewallPolicy`). To reopen, apply a
//! `FirewallPolicy` (direction: ingress) with `defaultPolicy: allow` to the
//! container, or clear its firewall by hand. (`kind: Ingress` is now the L7 HTTP
//! Ingress — unrelated to this L4 firewall.)

use serde::{Deserialize, Deserializer, Serialize};

use super::manifest::{self, ManifestDoc};
use delonix_runtime_core::{Error, FwRule, Result};

/// `spec` of `kind: Dependency`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DependencySpec {
    /// Container/VM that INITIATES the connection (the one that "knows"). Gains access to `to`.
    pub from: String,
    /// Target(s) that `from` gets to reach (and which become protected: only the
    /// declared `from`s reach them). Accepts a single name (`to: db`) or a list.
    #[serde(default, deserialize_with = "string_or_vec")]
    pub to: Vec<String>,
    /// Ports of `to` opened to `from` (e.g. `["5432"]`). Empty = any port.
    #[serde(default)]
    pub ports: Vec<String>,
    /// `tcp`/`udp`/`any` (default `any`).
    #[serde(default)]
    pub proto: Option<String>,
}

/// Known fields of the `spec` (drift-guard).
pub const DEPENDENCY_SPEC_FIELDS: &[&str] = &["from", "to", "ports", "proto"];

/// Deserializes `to` as a single name OR a list of names (ergonomics).
fn string_or_vec<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

/// Resolves the manifest's `kind: Dependency` and applies them. Runs in
/// `stack apply` AFTER the containers exist (it needs the IPs). Idempotent
/// ("ensure present" — reapplies the desired ingress state of each `to`).
/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: DependencySpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let deps = manifest::of_kind(docs, "Dependency");
    if deps.is_empty() {
        return Ok(());
    }
    let (_, store) = super::util::open_stores()?;
    // name → IP on the SDN (from the record); only containers with an IP serve as from/to.
    let ips: std::collections::HashMap<String, String> = store
        .list()?
        .into_iter()
        .filter_map(|c| c.ip.map(|ip| (c.name, ip)))
        .collect();

    // Group by TARGET: each `to` gathers the `allow`s of every `from` that knows it.
    let mut by_target: std::collections::BTreeMap<String, Vec<FwRule>> =
        std::collections::BTreeMap::new();
    for doc in &deps {
        manifest::warn_unknown_fields(doc, DEPENDENCY_SPEC_FIELDS);
        let spec: DependencySpec = manifest::spec_of(doc)?;
        let name = &doc.metadata.name;
        if spec.to.is_empty() {
            return Err(Error::Invalid(format!(
                "Dependency '{name}': `to` não pode ser vazio"
            )));
        }
        let proto = spec.proto.clone().unwrap_or_else(|| "any".into());
        if !delonix_runtime_core::fw_proto_ok(&proto) {
            return Err(Error::Invalid(format!(
                "Dependency '{name}': proto inválido '{proto}'"
            )));
        }
        let from_ip = ips.get(&spec.from).ok_or_else(|| {
            Error::Invalid(format!(
                "Dependency '{name}': from '{}' não tem IP na SDN (existe e está numa rede custom?)",
                spec.from
            ))
        })?;
        // Ports: empty = any; otherwise one rule per port.
        let ports: Vec<String> = if spec.ports.is_empty() {
            vec![String::new()]
        } else {
            spec.ports.clone()
        };
        for port in &ports {
            if !delonix_runtime_core::fw_port_ok(port) {
                return Err(Error::Invalid(format!(
                    "Dependency '{name}': porta inválida '{port}'"
                )));
            }
        }
        for target in &spec.to {
            if target == &spec.from {
                return Err(Error::Invalid(format!(
                    "Dependency '{name}': from e to são o mesmo ('{target}')"
                )));
            }
            if !ips.contains_key(target) {
                return Err(Error::Invalid(format!(
                    "Dependency '{name}': to '{target}' não tem IP na SDN (existe e está numa rede custom?)"
                )));
            }
            for port in &ports {
                by_target.entry(target.clone()).or_default().push(FwRule {
                    dir: "in".into(),
                    proto: proto.clone(),
                    port: port.clone(),
                    src: format!("{from_ip}/32"),
                    action: "allow".into(),
                    note: format!("Dependency: {} conhece {target}", spec.from),
                });
            }
        }
    }

    // Targets that ALSO have an explicit inbound firewall (`FirewallPolicy`,
    // direction: ingress): the Dependency runs afterwards and replaces the `in`
    // direction, so it deletes those rules. Warn instead of losing them silently
    // (composing the two sources is a follow-up — see review). NB: `kind: Ingress`
    // is now the L7 HTTP Ingress (no `target`), so it is NOT a firewall source here.
    let explicit_ingress: std::collections::HashSet<&str> = docs
        .iter()
        .filter(|d| {
            d.kind == "FirewallPolicy"
                && d.spec.get("direction").and_then(|v| v.as_str()) == Some("ingress")
        })
        .filter_map(|d| d.spec.get("target").and_then(|v| v.as_str()))
        .collect();

    // Apply: each target becomes default-deny + the accumulated allows.
    for (target, allows) in &by_target {
        if explicit_ingress.contains(target.as_str()) {
            eprintln!(
                "AVISO: '{target}' tem um Ingress/FirewallPolicy explícito E é alvo de Dependency — \
                 o Dependency é autoritativo e substitui a direção de entrada (as regras do Ingress \
                 explícito são apagadas). Usa só um dos dois para este container."
            );
        }
        super::firewall::apply_container_ingress(&store, target, "deny", allows)?;
        println!(
            "Dependency: '{target}' protegido (ingress default-deny) + {} allow(s)",
            allows.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(yaml: &str) -> DependencySpec {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn to_aceita_escalar_ou_lista() {
        assert_eq!(spec("from: app\nto: db\n").to, vec!["db"]);
        assert_eq!(spec("from: app\nto: [db, cache]\n").to, vec!["db", "cache"]);
    }

    #[test]
    fn ports_e_proto_default() {
        let s = spec("from: app\nto: [db]\n");
        assert!(s.ports.is_empty());
        assert!(s.proto.is_none());
    }

    #[test]
    fn from_obrigatorio() {
        assert!(serde_yaml::from_str::<DependencySpec>("to: [db]\n").is_err());
    }
}
