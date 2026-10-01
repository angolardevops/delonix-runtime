//! The digest of a network document's plan (ADR-0059 D4).
//!
//! `apply(plan, digest)` recomputes the digest and refuses a different one
//! as `stale_plan`: the `If-Match` rule, applied to a plan. So the digest
//! covers everything a plan was decided from — the normalized intent, what
//! the provider held when it was observed, the provider, the catalog
//! version, the states of the capabilities the plan uses, and the format of
//! the plan itself. A digest over the intent alone would never go stale,
//! and so would not be one.
//!
//! Pure: every input is plain data the caller already read.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::gateway::{AliasKind, GatewayObserved};

/// The version of the digest's own composition. Changing what goes in, or
/// how it is written, changes this — and so every digest.
pub const PLAN_FORMAT: u32 = 1;

/// SHA-256 (hex) over the canonical JSON of a plan's inputs. Object keys are
/// written in sorted order, so two equal inputs give one digest whatever
/// order they were built in.
pub fn plan_digest(
    intent: &BTreeMap<String, String>,
    observed: &serde_json::Value,
    provider: &str,
    catalog_version: &str,
    capabilities: &BTreeMap<String, String>,
) -> String {
    let mut doc: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    doc.insert("capabilities", serde_json::json!(capabilities));
    doc.insert("catalog", serde_json::json!(catalog_version));
    doc.insert("format", serde_json::json!(PLAN_FORMAT));
    doc.insert("intent", serde_json::json!(intent));
    doc.insert("observed", canonical(observed));
    doc.insert("provider", serde_json::json!(provider));
    let text = serde_json::to_string(&doc).unwrap_or_default();
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `v` with every object's keys in sorted order, at every depth.
fn canonical(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let sorted: BTreeMap<&String, serde_json::Value> =
                m.iter().map(|(k, v)| (k, canonical(v))).collect();
            serde_json::json!(sorted)
        }
        serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// What a gateway provider holds under a mark, as the fingerprint a digest
/// covers: aliases by name, rules by description, each field as observed,
/// alias content sorted. The order the provider answered in is not state.
pub fn gateway_fingerprint(o: &GatewayObserved) -> serde_json::Value {
    let mut aliases: Vec<serde_json::Value> = o
        .aliases
        .iter()
        .map(|a| {
            let mut content = a.content.clone();
            content.sort();
            serde_json::json!({
                "name": a.name,
                "kind": match a.kind { AliasKind::Host => "host", AliasKind::Network => "network" },
                "content": content,
                "description": a.description,
            })
        })
        .collect();
    aliases.sort_by_key(|a| a["name"].as_str().unwrap_or_default().to_string());
    let mut rules: Vec<serde_json::Value> = o
        .rules
        .iter()
        .map(|r| {
            serde_json::json!({
                "description": r.description,
                "source": r.source,
                "destination": r.destination,
                "protocol": r.protocol.as_deref().map(str::to_ascii_uppercase),
                "action": r.action.as_str(),
                "destinationPort": r.destination_port,
                "log": r.log,
                "stateful": r.stateful,
                "sequence": r.sequence,
                "disabled": o.disabled_rules.contains(&r.description),
            })
        })
        .collect();
    rules.sort_by_key(|r| r["description"].as_str().unwrap_or_default().to_string());
    serde_json::json!({ "aliases": aliases, "rules": rules })
}

/// What a segment provider holds for a zone under a mark, as the
/// fingerprint a digest covers: the zone's presence and the owned vnets by
/// name.
pub fn segment_fingerprint(o: &crate::segment::SegmentObserved) -> serde_json::Value {
    let mut vnets: Vec<serde_json::Value> = o
        .vnets
        .iter()
        .map(|v| {
            serde_json::json!({
                "name": v.name,
                "zone": v.zone,
                "alias": v.alias.as_deref().map(str::trim).filter(|a| !a.is_empty()),
            })
        })
        .collect();
    vnets.sort_by_key(|v| v["name"].as_str().unwrap_or_default().to_string());
    serde_json::json!({ "zonePresent": o.zone_present, "vnets": vnets })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{GatewayAlias, GatewayRule};

    fn inputs() -> (
        BTreeMap<String, String>,
        GatewayObserved,
        BTreeMap<String, String>,
    ) {
        let intent = BTreeMap::from([("rules".to_string(), "a|any|10.0.0.0/24|".to_string())]);
        let observed = GatewayObserved {
            aliases: vec![GatewayAlias {
                name: "web".into(),
                kind: AliasKind::Host,
                content: vec!["10.0.0.2".into(), "10.0.0.1".into()],
                description: String::new(),
            }],
            rules: vec![
                GatewayRule {
                    description: "b".into(),
                    ..GatewayRule::default()
                },
                GatewayRule {
                    description: "a".into(),
                    ..GatewayRule::default()
                },
            ],
            disabled_rules: vec![],
        };
        let caps = BTreeMap::from([("net.gateway.filter".to_string(), "supported".to_string())]);
        (intent, observed, caps)
    }

    fn digest(
        i: &BTreeMap<String, String>,
        o: &GatewayObserved,
        c: &BTreeMap<String, String>,
    ) -> String {
        plan_digest(i, &gateway_fingerprint(o), "opnsense", "1.3.0", c)
    }

    #[test]
    fn the_same_inputs_give_the_same_digest_whatever_their_order() {
        let (intent, observed, caps) = inputs();
        let mut reordered = observed.clone();
        reordered.rules.reverse();
        reordered.aliases[0].content.reverse();
        let d = digest(&intent, &observed, &caps);
        assert_eq!(d.len(), 64);
        assert_eq!(d, digest(&intent, &reordered, &caps));
    }

    /// Each input a plan is decided from moves the digest: an apply after
    /// any of them changed is a stale plan.
    #[test]
    fn every_input_moves_the_digest() {
        let (intent, observed, caps) = inputs();
        let base = digest(&intent, &observed, &caps);
        let mut i = intent.clone();
        i.insert("policies".into(), "x".into());
        assert_ne!(base, digest(&i, &observed, &caps), "intent");
        let mut o = observed.clone();
        o.rules[0].log = true;
        assert_ne!(base, digest(&intent, &o, &caps), "an observed field");
        let mut o = observed.clone();
        o.disabled_rules.push("a".into());
        assert_ne!(base, digest(&intent, &o, &caps), "a disabled rule");
        let mut o = observed.clone();
        o.rules.pop();
        assert_ne!(
            base,
            digest(&intent, &o, &caps),
            "a rule removed out of band"
        );
        let mut c = caps.clone();
        c.insert("net.gateway.filter".into(), "partial".into());
        assert_ne!(base, digest(&intent, &observed, &c), "a capability state");
        let fp = gateway_fingerprint(&observed);
        assert_ne!(
            base,
            plan_digest(&intent, &fp, "other", "1.3.0", &caps),
            "provider"
        );
        assert_ne!(
            base,
            plan_digest(&intent, &fp, "opnsense", "1.4.0", &caps),
            "catalog"
        );
    }
}
