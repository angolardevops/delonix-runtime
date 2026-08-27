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
//! as `kind: NetworkPolicy` — zero new dataplane. Multiple `Dependency` for the
//! same `to` ACCUMULATE the `allow`s.
//!
//! **Teardown ("ensure present", not a reconciler):** removing the `Dependency`
//! from a manifest and reapplying does NOT unprotect the `to` — the default-deny
//! ingress stays (same L4 firewall as `kind: NetworkPolicy`). To reopen, apply a
//! `FirewallPolicy` (direction: ingress) with `defaultPolicy: allow` to the
//! container, or clear its firewall by hand. (`kind: Ingress` is now the L7 HTTP
//! Ingress — unrelated to this L4 firewall.)

use super::kinds as k;
use serde::{Deserialize, Deserializer, Serialize};

use super::manifest::{self, ManifestDoc};
use delonix_runtime_core::{Error, Result};

/// `spec` of `kind: Dependency`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
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

/// Annotation recording which `kind: Dependency` documents produced a policy —
/// the lowering merges by target, so the original names would otherwise vanish
/// and nobody could tell where a rule came from.
pub const FROM_DEPENDENCIES: &str = "delonix.io/from-dependencies";

/// Lowers every `kind: Dependency` into `kind: NetworkPolicy` documents.
///
/// `Dependency` is **sugar**, and always was: it compiled to "on the `to`,
/// default-deny inbound plus an allow for the `from`" — exactly what a
/// `FirewallPolicy` with `direction: ingress` writes by hand. Having both as
/// siblings meant two constructions of the same language for the same rule, and
/// the engine had to WARN when a target was named by both, because the
/// `Dependency` silently won. Something needing a warning about its own twin is
/// the symptom; being the same thing is the cause.
///
/// **Merged by target, and that is the whole point.** Several dependencies
/// pointing at the same `to` ACCUMULATE their allows — that is the documented
/// semantics. One policy document per Dependency would not preserve it:
/// `apply_fw_doc` REPLACES the rules of a direction, so the last document
/// applied would silently drop every earlier peer's access. So the grouping
/// that used to happen at apply time now happens here, where it is visible in
/// `plan` and in `--dry-run`.
///
/// Pure: no store, no IPs. The rules name the peer by WORKLOAD
/// (`fromWorkload`), and the address is resolved when the policy is applied —
/// which is also why this lowering can run at load time at all, long before any
/// container exists.
pub fn lower_dependencies(docs: &[ManifestDoc]) -> Result<Vec<ManifestDoc>> {
    use serde_yaml::Value;
    // Target → (rules, origin dependency names). BTreeMap so the output order is
    // deterministic — a plan that reorders itself between runs is unreadable.
    let mut by_target: std::collections::BTreeMap<String, (Vec<Value>, Vec<String>)> =
        std::collections::BTreeMap::new();
    let mut namespaces: std::collections::BTreeMap<String, Option<String>> = Default::default();

    for doc in manifest::of_kind(docs, k::DEPENDENCY) {
        let spec: DependencySpec = manifest::spec_of(doc)?;
        let name = &doc.metadata.name;
        if spec.from.trim().is_empty() {
            return Err(Error::Invalid(super::po::tf(
                "Dependency '{name}': `from` cannot be empty",
                &[("name", name)],
            )));
        }
        if spec.to.is_empty() {
            return Err(Error::Invalid(super::po::tf(
                "Dependency '{name}': `to` cannot be empty",
                &[("name", name)],
            )));
        }
        let proto = spec.proto.clone().unwrap_or_else(|| "any".into());
        if !delonix_runtime_core::fw_proto_ok(&proto) {
            return Err(Error::Invalid(super::po::tf(
                "Dependency '{name}': invalid proto '{proto}'",
                &[("name", name), ("proto", &proto)],
            )));
        }
        // No ports = every port of the target, which is what `*` means to the
        // rule grammar. Kept explicit rather than defaulted downstream: the
        // lowered document is what the user reads in `--dry-run`.
        let ports = if spec.ports.is_empty() {
            vec!["*".to_string()]
        } else {
            spec.ports.clone()
        };
        for target in &spec.to {
            let entry = by_target.entry(target.clone()).or_default();
            for port in &ports {
                let mut rule = serde_yaml::Mapping::new();
                rule.insert(Value::from("action"), Value::from("allow"));
                rule.insert(Value::from("proto"), Value::from(proto.clone()));
                rule.insert(Value::from("port"), Value::from(port.clone()));
                rule.insert(Value::from("fromWorkload"), Value::from(spec.from.clone()));
                rule.insert(
                    Value::from("note"),
                    Value::from(format!("dependency:{name}")),
                );
                entry.0.push(Value::Mapping(rule));
            }
            if !entry.1.contains(name) {
                entry.1.push(name.clone());
            }
            // The policy protects the TARGET, so it belongs to the target's
            // namespace, not the declaring document's.
            namespaces
                .entry(target.clone())
                .or_insert_with(|| doc.metadata.namespace.clone());
        }
    }

    let mut out = Vec::new();
    for (target, (rules, origins)) in by_target {
        let mut spec = serde_yaml::Mapping::new();
        spec.insert(Value::from("direction"), Value::from("ingress"));
        spec.insert(Value::from("target"), Value::from(target.clone()));
        // Default-deny is what makes the `to` PROTECTED — without it the allow
        // would be decoration on an open target, which is the opposite of what
        // "only who declares it reaches it" means.
        spec.insert(Value::from("defaultPolicy"), Value::from("deny"));
        spec.insert(Value::from("rules"), Value::Sequence(rules));
        let mut meta = manifest::Metadata {
            // Named after what it IS — the inbound policy of the target — because
            // several dependencies merge into one and no single original name
            // could stand for the result.
            name: format!("dependency-{target}"),
            namespace: namespaces.get(&target).cloned().flatten(),
            labels: Default::default(),
            annotations: Default::default(),
        };
        meta.annotations
            .insert(FROM_DEPENDENCIES.to_string(), origins.join(","));
        out.push(ManifestDoc {
            api_version: "delonix.io/v1".to_string(),
            kind: k::FIREWALL_POLICY.to_string(),
            metadata: meta,
            spec: Value::Mapping(spec),
        });
    }
    Ok(out)
}

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

    fn docs(yaml: &str) -> Vec<super::ManifestDoc> {
        serde_yaml::Deserializer::from_str(yaml)
            .map(|d| serde::Deserialize::deserialize(d).unwrap())
            .collect()
    }

    /// **A propriedade que uma fusão mal feita partiria em silêncio.** Várias
    /// dependências para o mesmo `to` ACUMULAM os allows — é a semântica
    /// documentada. Um documento de política por Dependency não a preservaria:
    /// o `apply_fw_doc` SUBSTITUI as regras de uma direcção, por isso o último
    /// aplicado deixaria de fora todos os peers anteriores. Daí a fusão por
    /// alvo.
    #[test]
    fn varias_dependencias_para_o_mesmo_alvo_acumulam_numa_so_politica() {
        let d = docs(
            "apiVersion: delonix.io/v1\nkind: Dependency\nmetadata: { name: a }\nspec: { from: app, to: db, ports: ['5432'] }\n---\napiVersion: delonix.io/v1\nkind: Dependency\nmetadata: { name: b }\nspec: { from: worker, to: db, ports: ['5432'] }\n",
        );
        let out = super::lower_dependencies(&d).unwrap();
        assert_eq!(out.len(), 1, "um alvo, uma politica: {out:#?}");
        assert_eq!(out[0].kind, "NetworkPolicy");
        assert_eq!(out[0].metadata.name, "dependency-db");
        let rules = out[0].spec.get("rules").unwrap().as_sequence().unwrap();
        assert_eq!(rules.len(), 2, "os dois peers têm de sobreviver");
        let peers: Vec<&str> = rules
            .iter()
            .map(|r| r.get("fromWorkload").unwrap().as_str().unwrap())
            .collect();
        assert!(
            peers.contains(&"app") && peers.contains(&"worker"),
            "{peers:?}"
        );
        // A fusão apaga os nomes originais, por isso a origem fica registada —
        // senão ninguém sabe de onde veio uma regra.
        assert_eq!(
            out[0]
                .metadata
                .annotations
                .get(super::FROM_DEPENDENCIES)
                .unwrap(),
            "a,b"
        );
    }

    /// O `to` aceita uma lista, e cada alvo tem de ficar com a SUA política —
    /// não uma partilhada, que abriria um alvo ao peer do outro.
    #[test]
    fn uma_lista_de_alvos_da_uma_politica_por_alvo() {
        let d = docs(
            "apiVersion: delonix.io/v1\nkind: Dependency\nmetadata: { name: a }\nspec: { from: app, to: [db, cache] }\n",
        );
        let out = super::lower_dependencies(&d).unwrap();
        let mut names: Vec<&str> = out.iter().map(|d| d.metadata.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["dependency-cache", "dependency-db"]);
    }

    /// O default-deny é o que PROTEGE o alvo. Sem ele o allow seria decoração
    /// sobre um alvo aberto — o oposto de «só quem declara é que alcança».
    #[test]
    fn a_politica_gerada_e_default_deny_e_sem_portas_abre_todas() {
        let d = docs(
            "apiVersion: delonix.io/v1\nkind: Dependency\nmetadata: { name: a }\nspec: { from: app, to: db }\n",
        );
        let out = super::lower_dependencies(&d).unwrap();
        assert_eq!(
            out[0].spec.get("defaultPolicy").unwrap().as_str(),
            Some("deny")
        );
        assert_eq!(
            out[0].spec.get("direction").unwrap().as_str(),
            Some("ingress")
        );
        let rules = out[0].spec.get("rules").unwrap().as_sequence().unwrap();
        assert_eq!(rules[0].get("port").unwrap().as_str(), Some("*"));
    }

    #[test]
    fn from_ou_to_vazios_e_proto_invalido_sao_recusados() {
        for bad in [
            "spec: { from: '', to: db }",
            "spec: { from: app, to: [] }",
            "spec: { from: app, to: db, proto: 'tcp; drop' }",
        ] {
            let d = docs(&format!(
                "apiVersion: delonix.io/v1\nkind: Dependency\nmetadata: {{ name: a }}\n{bad}\n"
            ));
            assert!(super::lower_dependencies(&d).is_err(), "aceitou: {bad}");
        }
    }
}
