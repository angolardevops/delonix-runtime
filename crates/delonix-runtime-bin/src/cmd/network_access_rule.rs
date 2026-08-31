//! `kind: NetworkAccessRule` — one INCREMENTAL firewall rule per document.
//!
//! `kind: FirewallPolicy` (also spelled `NetworkPolicy`, `cmd::kinds::FIREWALL_POLICY`)
//! REPLACES the whole state of one direction on every apply — two documents
//! naming the same (target, direction) are refused outright (`stack.rs`), because
//! a second one would silently erase the first's rules while both report
//! success. That is the right call for "declare the whole policy of a
//! direction", and this module does not change it.
//!
//! It does not help the case B4 of the CLI restructuring plan needed, though:
//! `net ingress allow`/`net egress allow` are INCREMENTAL — add one rule, keep
//! the others — and nothing declarative could express that until now. Measured
//! twice (`docs/releases/v0.68.0.md`) that this is what blocked collapsing
//! `net ingress`/`net egress`'s CLI surface into a Kind.
//!
//! **The mechanism**: `FwRule` gained an `origin: Option<String>` field — the
//! name of the `NetworkAccessRule` document that contributed it. A document's
//! own apply finds-and-replaces only the rule carrying ITS OWN origin on the
//! target container, leaving every other rule (from other `NetworkAccessRule`
//! documents, from imperative `net ingress`/`net egress` commands, or from a
//! `FirewallPolicy`) untouched. Removing the document retracts only that one
//! rule. This is new: the one prior accumulation mechanism, `kind: Dependency`,
//! sidesteps the problem by merging every sibling document into ONE
//! `FirewallPolicy` at manifest-**load** time (`dependency.rs::lower_dependencies`)
//! — which only works within a single `stack apply` pass, and by its own
//! module doc admits it does not support retracting one Dependency
//! independently. A `NetworkAccessRule` document is `Form::Primary` (survives
//! the load, has its own identity) precisely so it can be applied and removed
//! on its own, like any other Kind.
//!
//! **No new dataplane primitive.** The holder already rebuilds a container's
//! whole firewall chain from its full rule list on every apply, in one atomic
//! `nft -f` (`delonix-net/src/infra.rs::do_firewall`) — there is no per-rule
//! nft add/delete to build. The only thing this module adds is *which* Rust
//! rule list to hand it when several documents target the same container.
//!
//! **No new CLI leaf.** Reached the same way `kind: Dependency` already is:
//! through `delonix apply -f`/`delonix stack apply`, which dispatch by Kind.
//! Collapsing `net ingress`/`net egress`'s own CLI surface onto this Kind is a
//! deliberately separate, later step — see `docs/adr/0028-network-access-rule-incremental.md`.

use super::kinds as k;
use delonix_runtime_core::{fw_port_ok, fw_proto_ok, Error, FwRule, Result, Store};

use super::firewall::{check_cidr, require_sdn_ip, update_locked};
use super::manifest::{self, ManifestDoc};
use super::util::open_stores;

/// `spec` of `kind: NetworkAccessRule`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct NetworkAccessRuleSpec {
    /// Container this rule applies to. Must be on the SDN — a `--net
    /// host`/`none` container has no firewall to govern.
    pub target: String,
    /// `ingress` (traffic TO the container) or `egress` (FROM it).
    pub direction: String,
    /// `allow` (accept) or `deny` (drop). Default `allow`.
    #[serde(default)]
    pub action: Option<String>,
    /// `tcp`/`udp`/`any`. Default `any`.
    #[serde(default)]
    pub proto: Option<String>,
    /// Port, a range `n-m`, or `*` (any).
    pub port: String,
    /// CIDR of the other end (source on ingress, destination on egress).
    /// Empty/omitted = anywhere.
    #[serde(default)]
    pub from: Option<String>,
}

/// Known fields of the `spec` (drift-guard, matches the pattern every other
/// Kind's spec uses — e.g. `DEPENDENCY_SPEC_FIELDS`, `FW_SPEC_FIELDS`).
pub const NETWORK_ACCESS_RULE_SPEC_FIELDS: &[&str] =
    &["target", "direction", "action", "proto", "port", "from"];

/// Per-origin annotation keys on the TARGET container. Deliberately NOT the
/// container's own `STACK_LABEL`/`MANAGED_BY` labels: those decide whether the
/// container itself gets adopted/pruned by a stack, and a `NetworkAccessRule`
/// does not own the container it targets — only its own one rule on it. Two
/// `NetworkAccessRule` documents (from the same or different stacks) can
/// target the same container without fighting over who "owns" it; each has
/// its own annotation pair. Found live, not by reasoning: an earlier version
/// stamped `STACK_LABEL` on the container itself, and a `stack apply --prune`
/// of the stack owning the RULE went on to delete the unrelated container the
/// rule happened to target.
fn last_applied_key(name: &str) -> String {
    format!(
        "{}/networkaccessrule/{name}",
        super::reconcile::LAST_APPLIED
    )
}
fn owner_key(name: &str) -> String {
    format!("delonix.io/networkaccessrule-owner/{name}")
}

/// `ingress`/`egress` → the internal `in`/`out` `FwRule.dir`, or a clear error
/// naming the document — same mapping `firewall.rs::apply` already uses for
/// `FirewallPolicy`, repeated here because the two Kinds do not share a
/// dispatch point worth factoring out for two call sites.
fn resolve_dir(doc: &ManifestDoc, direction: &str) -> Result<&'static str> {
    match direction {
        "ingress" => Ok("in"),
        "egress" => Ok("out"),
        other => Err(Error::Invalid(super::po::tf(
            "NetworkAccessRule/{name}: direction must be ingress|egress (got {other})",
            &[("name", &doc.metadata.name), ("other", other)],
        ))),
    }
}

/// Applies one document: find-and-replace the rule carrying this document's
/// own `origin` on the target container, leaving every other rule (owned by
/// another document, or unowned) untouched.
fn apply_one(store: &Store, doc: &ManifestDoc) -> Result<()> {
    let spec: NetworkAccessRuleSpec = manifest::spec_of(doc)?;
    let dir = resolve_dir(doc, &spec.direction)?;
    let name = &doc.metadata.name;

    let proto = spec.proto.clone().unwrap_or_else(|| "any".into());
    if !fw_proto_ok(&proto) {
        return Err(Error::Invalid(super::po::tf(
            "NetworkAccessRule/{name}: invalid proto '{proto}'",
            &[("name", name), ("proto", &proto)],
        )));
    }
    if !fw_port_ok(&spec.port) {
        return Err(Error::Invalid(super::po::tf(
            "NetworkAccessRule/{name}: invalid port '{port}'",
            &[("name", name), ("port", &spec.port)],
        )));
    }
    let src = spec.from.clone().unwrap_or_default();
    check_cidr(&src).map_err(|e| Error::Invalid(format!("NetworkAccessRule/{name}: {e}")))?;
    let action = spec.action.clone().unwrap_or_else(|| "allow".into());
    if !matches!(action.as_str(), "allow" | "deny") {
        return Err(Error::Invalid(super::po::tf(
            "NetworkAccessRule/{name}: action must be allow|deny",
            &[("name", name)],
        )));
    }

    let rule = FwRule {
        dir: dir.to_string(),
        proto,
        port: spec.port.clone(),
        src,
        action,
        note: String::new(),
        origin: Some(name.clone()),
    };
    set_rule_by_origin(store, &spec.target, name, rule)?;
    println!(
        "{}",
        super::po::tf(
            "NetworkAccessRule/{name}: applied to {target}",
            &[("name", name), ("target", &spec.target)],
        )
    );
    Ok(())
}

/// Removes any existing rule with `origin`, then adds `rule` (whose own
/// `origin` is the same name) — a find-and-replace keyed by origin instead of
/// by match, so editing a document's port/action/CIDR across applies replaces
/// cleanly instead of leaving the old value behind under a new one.
fn set_rule_by_origin(store: &Store, target: &str, origin: &str, rule: FwRule) -> Result<()> {
    update_locked(store, target, |c| {
        require_sdn_ip(c)?;
        let mut fw = c.firewall.clone().unwrap_or_default();
        fw.enabled = true;
        fw.rules.retain(|r| r.origin.as_deref() != Some(origin));
        fw.rules.push(rule.clone());
        super::container::apply_firewall_everywhere(c, &fw)?;
        c.firewall = Some(fw);
        Ok(true)
    })?;
    Ok(())
}

/// Retracts `origin`'s rule from wherever it lives. `destroy_one`
/// (`stack.rs`) only ever hands this a document NAME, not the document
/// itself — this Kind has no registry of its own (the "record" is the
/// `origin`-tagged rule sitting on the target container), so which container
/// to look at is found by scanning, the same way `firewall.rs`'s reconciler
/// already reads state off containers rather than a store of its own. Origin
/// names are unique manifest-wide (the generic "declared more than once"
/// check), so at most one container ever matches — idempotent if none do
/// (mirrors `JsonStore::remove`'s "absence = Ok").
pub(crate) fn remove_by_origin_anywhere(store: &Store, origin: &str) -> Result<()> {
    for c in store.list()? {
        let Some(fw) = &c.firewall else { continue };
        if !fw.rules.iter().any(|r| r.origin.as_deref() == Some(origin)) {
            continue;
        }
        update_locked(store, &c.id, |c| {
            let mut fw = c.firewall.clone().unwrap_or_default();
            fw.rules.retain(|r| r.origin.as_deref() != Some(origin));
            super::container::apply_firewall_everywhere(c, &fw)?;
            c.firewall = Some(fw);
            // Retracted — the per-origin annotations would otherwise linger on
            // the container forever, outliving the rule they described.
            c.annotations.remove(&owner_key(origin));
            c.annotations.remove(&last_applied_key(origin));
            Ok(true)
        })?;
    }
    Ok(())
}

/// Applies every `kind: NetworkAccessRule` document in the manifest. Runs
/// after `FirewallPolicy` in `stack apply`'s layer order — harmless either
/// way now that the replace is origin-aware, but keeping it beside the Kind
/// it is closest in spirit to is the more readable order.
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (_images, store) = open_stores()?;
    for doc in manifest::of_kind(docs, k::NETWORK_ACCESS_RULE) {
        apply_one(&store, doc)?;
    }
    Ok(())
}

/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: NetworkAccessRuleSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

/// Fields the reconciler compares for a `kind: NetworkAccessRule`.
pub(crate) const RECONCILED_NETWORK_ACCESS_RULE_FIELDS: &[&str] =
    &["target", "direction", "action", "proto", "port", "from"];

/// What the manifest declares, for the reconciler. `ownable: true` — unlike
/// `FirewallPolicy`'s `desired()`, this Kind's rule has a durable identity
/// (its own `origin`), so a stack CAN own and prune it.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: NetworkAccessRuleSpec = manifest::spec_of(doc)?;
    let mut f = std::collections::BTreeMap::new();
    f.insert("target".into(), spec.target.clone());
    f.insert("direction".into(), spec.direction.clone());
    f.insert(
        "action".into(),
        spec.action.clone().unwrap_or_else(|| "allow".into()),
    );
    f.insert(
        "proto".into(),
        spec.proto.clone().unwrap_or_else(|| "any".into()),
    );
    f.insert("port".into(), spec.port.clone());
    f.insert("from".into(), spec.from.clone().unwrap_or_default());
    Ok(super::reconcile::Desired {
        kind: k::NETWORK_ACCESS_RULE.into(),
        name: doc.metadata.name.clone(),
        fields: f,
        converges: true,
        ownable: true,
    })
}

/// What is on the machine, for the reconciler. No record of its own — a
/// rule lives on whichever container's `ContainerFw` carries it, tagged by
/// `origin`. **Scans every container, not just the current manifest's
/// docs**: unlike `firewall.rs::actual` (which only ever needs to compare
/// against a document that is still there, because `FirewallPolicy` is not
/// `ownable`), this Kind IS ownable — `stack apply --prune` has to see a rule
/// whose document was just deleted from the manifest in order to plan its
/// removal, and that rule's origin no longer appears in `docs` at all by the
/// time this runs. Ignoring `docs` here is deliberate, not an oversight.
pub(crate) fn actual(_docs: &[ManifestDoc]) -> Result<Vec<super::reconcile::Actual>> {
    let (_images, store) = open_stores()?;
    let mut out = Vec::new();
    for c in store.list()? {
        let Some(fw) = &c.firewall else { continue };
        for r in &fw.rules {
            let Some(origin) = &r.origin else { continue };
            let mut f = std::collections::BTreeMap::new();
            f.insert("target".into(), c.name.clone());
            f.insert(
                "direction".into(),
                if r.dir == "in" { "ingress" } else { "egress" }.into(),
            );
            f.insert("action".into(), r.action.clone());
            f.insert("proto".into(), r.proto.clone());
            f.insert("port".into(), r.port.clone());
            f.insert("from".into(), r.src.clone());
            out.push(super::reconcile::Actual {
                kind: k::NETWORK_ACCESS_RULE.into(),
                name: origin.clone(),
                fields: f,
                owner: c.annotations.get(&owner_key(origin)).cloned(),
                last_applied: c
                    .annotations
                    .get(&last_applied_key(origin))
                    .and_then(|raw| super::reconcile::decode_last_applied(raw)),
            });
        }
    }
    Ok(out)
}

/// Records ownership + last-applied for `name`'s rule, on the container
/// holding it (found by scanning — no registry of its own, see
/// `remove_by_origin_anywhere`). Deliberately its OWN per-origin annotations
/// (`owner_key`/`last_applied_key`), never the container's `STACK_LABEL`/
/// `MANAGED_BY` — those decide the CONTAINER's own adopt/prune fate, and a
/// rule targeting a container it did not create must not be able to hand that
/// container to its stack. Live-caught: an earlier version wrote the shared
/// labels here, and `stack apply --prune` of the stack owning only the RULE
/// went on to delete the unrelated container it targeted.
pub(crate) fn stamp(
    name: &str,
    stack: &str,
    fields: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let (_images, store) = open_stores()?;
    let encoded = super::reconcile::encode_last_applied(fields);
    let owner_k = owner_key(name);
    let last_applied_k = last_applied_key(name);
    for c in store.list()? {
        let owns = c
            .firewall
            .as_ref()
            .is_some_and(|fw| fw.rules.iter().any(|r| r.origin.as_deref() == Some(name)));
        if !owns {
            continue;
        }
        store.update(&c.id, |cur| {
            cur.annotations.insert(owner_k.clone(), stack.to_string());
            cur.annotations
                .insert(last_applied_k.clone(), encoded.clone());
            true
        })?;
    }
    Ok(())
}

/// Converges a rule: re-apply the document. Same rationale as
/// `firewall.rs::converge_doc` — `apply_one` already achieves convergence
/// (find-and-replace by origin), so there is no separate per-field patch path
/// to maintain alongside it.
pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    let (_images, store) = open_stores()?;
    apply_one(&store, doc)
}

/// `destroy_one`'s arm for this Kind: retract `name`'s rule wherever it is.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    let (_images, store) = open_stores()?;
    remove_by_origin_anywhere(&store, name)?;
    println!(
        "{}",
        super::po::tf("NetworkAccessRule/{name}: removed", &[("name", name)],)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(yaml: &str) -> NetworkAccessRuleSpec {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn defaults_de_action_e_proto() {
        let s = spec("target: web\ndirection: ingress\nport: '8080'\n");
        assert!(s.action.is_none());
        assert!(s.proto.is_none());
        assert!(s.from.is_none());
    }

    #[test]
    fn target_direction_port_sao_obrigatorios() {
        assert!(
            serde_yaml::from_str::<NetworkAccessRuleSpec>("direction: ingress\nport: '80'\n")
                .is_err()
        );
        assert!(
            serde_yaml::from_str::<NetworkAccessRuleSpec>("target: web\nport: '80'\n").is_err()
        );
        assert!(
            serde_yaml::from_str::<NetworkAccessRuleSpec>("target: web\ndirection: ingress\n")
                .is_err()
        );
    }

    fn doc(yaml: &str) -> ManifestDoc {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn resolve_dir_mapeia_ingress_egress_e_recusa_o_resto() {
        let d = doc(
            "apiVersion: networking.delonix.io/v1alpha1\nkind: NetworkAccessRule\nmetadata: { name: a }\nspec: { target: web, direction: ingress, port: '80' }\n",
        );
        assert_eq!(resolve_dir(&d, "ingress").unwrap(), "in");
        assert_eq!(resolve_dir(&d, "egress").unwrap(), "out");
        assert!(resolve_dir(&d, "sideways").is_err());
    }
}
