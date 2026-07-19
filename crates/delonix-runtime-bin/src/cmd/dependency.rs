//! `kind: Dependency` (alias `KnowDepends`) — alcançabilidade **DIRIGIDA** entre
//! containers/VMs. Ao contrário de uma `Network` (comunicação bidirecional), uma
//! dependência abre UM sentido: `from` alcança `to`, mas `to` **não** inicia para
//! `from`. É o caso "a app conhece a DB, a DB não conhece a app" — a DB deixa de
//! ficar exposta a todos os containers de uma rede partilhada.
//!
//! **Como funciona (açúcar sobre o firewall L4 por-container):** declarar
//! `Dependency { from: app, to: [db] }` compila para, no `db`: ingress
//! **default-deny** (protege a DB de TODA a SDN) + um `allow` do IP do `app`. O
//! sentido inverso (db→app) nunca é aberto, e o retorno da conversa app↔db flui
//! porque a SDN é stateful (`ct state established,related accept`). Reutiliza o
//! mesmo `ContainerFw`/`infra::apply_firewall` do `kind: Ingress` — zero dataplane
//! novo. Várias `Dependency` para o mesmo `to` ACUMULAM os `allow`.
//!
//! **Teardown ("garante presente", não reconciliador):** remover a `Dependency`
//! de um manifesto e reaplicar NÃO desprotege o `to` — o ingress default-deny
//! fica (mesma semântica do `kind: Ingress`). Para reabrir, aplica um `Ingress`
//! com `defaultPolicy: allow` ao container, ou limpa a firewall dele à mão.

use serde::{Deserialize, Deserializer};

use super::manifest::{self, ManifestDoc};
use delonix_runtime_core::{Error, FwRule, Result};

/// `spec` de `kind: Dependency`.
#[derive(Debug, Clone, Deserialize)]
pub struct DependencySpec {
    /// Container/VM que INICIA a ligação (o que "conhece"). Ganha acesso a `to`.
    pub from: String,
    /// Alvo(s) que `from` passa a alcançar (e que ficam protegidos: só os `from`
    /// declarados os alcançam). Aceita um nome só (`to: db`) ou uma lista.
    #[serde(default, deserialize_with = "string_or_vec")]
    pub to: Vec<String>,
    /// Portas de `to` abertas ao `from` (ex.: `["5432"]`). Vazio = qualquer porta.
    #[serde(default)]
    pub ports: Vec<String>,
    /// `tcp`/`udp`/`any` (default `any`).
    #[serde(default)]
    pub proto: Option<String>,
}

/// Campos conhecidos do `spec` (drift-guard).
pub const DEPENDENCY_SPEC_FIELDS: &[&str] = &["from", "to", "ports", "proto"];

/// Desserializa `to` como um nome único OU uma lista de nomes (ergonomia).
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

/// Resolve os `kind: Dependency` do manifesto e aplica-os. Corre no `stack apply`
/// DEPOIS dos containers existirem (precisa dos IPs). Idempotente ("garante
/// presente" — reaplica o estado desejado do ingress de cada `to`).
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let deps = manifest::of_kind(docs, "Dependency");
    if deps.is_empty() {
        return Ok(());
    }
    let (_, store) = super::util::open_stores()?;
    // nome → IP na SDN (do record); só containers com IP servem de from/to.
    let ips: std::collections::HashMap<String, String> = store
        .list()?
        .into_iter()
        .filter_map(|c| c.ip.map(|ip| (c.name, ip)))
        .collect();

    // Agrupa por ALVO: cada `to` junta os `allow` de todos os `from` que o conhecem.
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
        // Portas: vazio = qualquer; senão uma regra por porta.
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

    // Alvos que TAMBÉM têm um Ingress/FirewallPolicy(ingress) explícito: o
    // Dependency corre depois e substitui a direção `in`, logo apaga essas regras.
    // Avisar em vez de as perder em silêncio (a composição das duas fontes é um
    // follow-up — ver revisão).
    let explicit_ingress: std::collections::HashSet<&str> = docs
        .iter()
        .filter(|d| {
            d.kind == "Ingress"
                || (d.kind == "FirewallPolicy"
                    && d.spec.get("direction").and_then(|v| v.as_str()) == Some("ingress"))
        })
        .filter_map(|d| d.spec.get("target").and_then(|v| v.as_str()))
        .collect();

    // Aplica: cada alvo fica default-deny + os allow acumulados.
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
