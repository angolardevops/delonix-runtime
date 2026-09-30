//! `kind: NetworkGateway` — declares aliases and perimeter rules on a
//! registered `GatewayProvider` (ADR-0051 Phase 3).
//!
//! **A separate Kind from `kind: NetworkPolicy`/`NetworkAccessRule` on
//! purpose.** Those describe per-workload firewalling, always enforced
//! natively (`FirewallPerWorkload`/`FirewallDefaultDeny`/…, ADR-0050) — a
//! `GatewayProvider`'s actual strength (NAT, multi-WAN, perimeter filtering
//! on an external appliance) is a different question with a different
//! shape: `source`/`destination`/`protocol` at the node's edge, not
//! `target`/`direction` on one container. Forcing it into `NetworkPolicy`'s
//! shape was the mismatch ADR-0051's own Context section measured; this
//! Kind is the honest one instead.
//!
//! **Its own registry**, the same reason `kind: Service` has one
//! (ADR-0032): the appliance a `GatewayProvider` talks to carries none of
//! this engine's `delonix.io/stack` labels to stamp, so ownership lives
//! here, not on the far end.
//!
//! **No update-in-place**, because [`delonix_sdn::gateway::GatewayProvider`]
//! has none (ADR-0051 Phase 2 measured that `set_item`/`set_rule`'s exact
//! shape was never part of the spike, and guessing it was the trap the
//! whole ADR exists to avoid). `apply()` only ENSURES every alias/rule
//! CURRENTLY declared is present — it does not retract one dropped from the
//! list while others stay. Removing the retraction that matters is the
//! WHOLE document's teardown (`--replace NetworkGateway/<name>`, or drop it
//! from the manifest under `stack apply --prune`), which removes every
//! alias/rule the registry last recorded, not just what the new spec says.
//!
//! **Ownership on the far end** (audit 62, §6 P1). The registry says which
//! aliases/rules a document declared; it does not prove the appliance's
//! objects of those names are this engine's. Each record carries an owner
//! token ([`delonix_sdn::ownership::OwnerMark`]), generated and SAVED before
//! the first remote write, that the provider attaches to every alias and rule
//! it creates (on OPNsense, a firewall category `delonix-owner:<token>`): an
//! object of the same name without it is refused on apply and left alone (and
//! named) on teardown, and the teardown retires the category last. A record written before the token
//! existed has none — its objects were never marked, so its teardown removes
//! nothing on the appliance and says so per object.
//!
//! **No new CLI leaf** beyond the generic `get`/`describe`/`delete
//! networkgateways` verbs (`cmd/verbs.rs`) — reached the same way `kind:
//! Service`/`kind: NetworkAccessRule` already are, through `delonix apply
//! -f`/`delonix stack apply`.

use std::collections::BTreeMap;

use super::kinds as k;
use super::manifest::{self, ManifestDoc};
use super::output::OutputFormat;
use super::util::state_root;
use delonix_model::{Error, Result};
use delonix_sdn::gateway::{AliasKind, GatewayAlias, GatewayProvider, GatewayRule};
use delonix_sdn::ownership::{OwnerMark, RemoveOutcome};
use delonix_state::JsonStore;

/// `spec` of `kind: NetworkGateway`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct NetworkGatewaySpec {
    /// The registered `GatewayProvider` id (`opnsense`). Optional since
    /// ADR-0059 F2c: without it the record's provider, then
    /// `networkDefaults.gateway`, then — only without a `providers.yaml` —
    /// the single registered gateway provider answers (D3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default)]
    pub aliases: Vec<GatewayAliasSpec>,
    #[serde(default)]
    pub rules: Vec<GatewayRuleSpec>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct GatewayAliasSpec {
    pub name: String,
    /// `host` or `network` — the two shapes `delonix_sdn::gateway::AliasKind`
    /// knows (ADR-0051 Phase 1: the only two a v1 client needs).
    pub kind: String,
    pub content: Vec<String>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct GatewayRuleSpec {
    /// The rule's identity on the appliance — there is no other stable name
    /// for one (ADR-0051 Phase 0/2: OPNsense's own docs use `description`
    /// as the find-or-create key).
    pub description: String,
    pub source: String,
    pub destination: String,
    #[serde(default)]
    pub protocol: Option<String>,
}

/// Known fields of the `spec` (drift-guard, the pattern every other Kind's
/// spec uses).
pub const NETWORK_GATEWAY_SPEC_FIELDS: &[&str] = &["provider", "aliases", "rules"];

/// Fields the reconciler compares.
pub const RECONCILED_NETWORK_GATEWAY_FIELDS: &[&str] = &["provider", "aliases", "rules"];

/// A registered record: what was last declared, plus the ownership fields
/// every ownable Kind's own registry carries (mirrors `ServiceDef`).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct NetworkGatewayRecord {
    name: String,
    provider: String,
    #[serde(default)]
    aliases: Vec<GatewayAliasSpec>,
    #[serde(default)]
    rules: Vec<GatewayRuleSpec>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
    /// The owner token written into every alias/rule this record created on
    /// the appliance. Empty in a record from before the token existed.
    #[serde(default)]
    owner: String,
}

fn store() -> Result<JsonStore<NetworkGatewayRecord>> {
    JsonStore::open(state_root().join("network-gateways")).map_err(Into::into)
}

fn alias_kind(raw: &str) -> Result<AliasKind> {
    match raw {
        "host" => Ok(AliasKind::Host),
        "network" => Ok(AliasKind::Network),
        other => Err(Error::Invalid(super::po::tf(
            "invalid alias kind '{kind}' — must be 'host' or 'network'",
            &[("kind", other)],
        ))),
    }
}

fn to_alias(spec: &GatewayAliasSpec) -> Result<GatewayAlias> {
    Ok(GatewayAlias {
        name: spec.name.clone(),
        kind: alias_kind(&spec.kind)?,
        content: spec.content.clone(),
        description: spec.description.clone(),
    })
}

fn to_rule(spec: &GatewayRuleSpec) -> GatewayRule {
    GatewayRule {
        description: spec.description.clone(),
        source: spec.source.clone(),
        destination: spec.destination.clone(),
        protocol: spec.protocol.clone(),
    }
}

/// The provider id records written before ADR-0059 F2b may carry. That
/// provider refused every alias and rule, so such a record owns nothing
/// remote and [`remove_for_replace`] drops it locally.
const LEGACY_NATIVE: &str = "native";

fn resolve_provider(
    named: Option<&str>,
    recorded: &str,
) -> Result<(&'static str, Box<dyn GatewayProvider>)> {
    use delonix_networking::resolve::{Role, Wanted};
    let (default, config) = super::providers_config::network_default(Role::Gateway)?;
    delonix_networking::gateway::choose_gateway_provider(&Wanted {
        role: Role::Gateway,
        named,
        recorded: Some(recorded),
        default: default.as_deref(),
        config: config.as_deref(),
    })
    .map_err(Into::into)
}

/// A comparable summary of one alias/rule list — sorted so two applies of an
/// unchanged spec never drift because a manifest happened to list them in a
/// different order.
fn aliases_field(aliases: &[GatewayAliasSpec]) -> String {
    let mut items: Vec<String> = aliases
        .iter()
        .map(|a| {
            format!(
                "{}|{}|{}|{}",
                a.name,
                a.kind,
                a.content.join(","),
                a.description
            )
        })
        .collect();
    items.sort();
    items.join(";")
}

fn rules_field(rules: &[GatewayRuleSpec]) -> String {
    let mut items: Vec<String> = rules
        .iter()
        .map(|r| {
            format!(
                "{}|{}|{}|{}",
                r.description,
                r.source,
                r.destination,
                r.protocol.as_deref().unwrap_or("")
            )
        })
        .collect();
    items.sort();
    items.join(";")
}

fn record_fields(rec: &NetworkGatewayRecord) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("provider".into(), rec.provider.clone());
    f.insert("aliases".into(), aliases_field(&rec.aliases));
    f.insert("rules".into(), rules_field(&rec.rules));
    f
}

/// What the manifest declares, for the reconciler. `ownable: true` — a
/// `NetworkGateway` has its own durable identity (name), same as `Service`.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: NetworkGatewaySpec = manifest::spec_of(doc)?;
    let mut fields = BTreeMap::new();
    // Only a named provider is compared: without one, the provider is the
    // record's, and the manifest has nothing to say about it (D3).
    if let Some(p) = &spec.provider {
        fields.insert("provider".into(), p.clone());
    }
    fields.insert("aliases".into(), aliases_field(&spec.aliases));
    fields.insert("rules".into(), rules_field(&spec.rules));
    Ok(super::reconcile::Desired {
        kind: k::NETWORK_GATEWAY.into(),
        name: doc.metadata.name.clone(),
        fields,
        converges: true,
        ownable: true,
    })
}

/// Every declared `NetworkGateway` — the enumeration `--prune` needs, same
/// reasoning as `service::actual`.
pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    Ok(store()?
        .list()?
        .into_iter()
        .map(|rec| super::reconcile::Actual {
            kind: k::NETWORK_GATEWAY.into(),
            name: rec.name.clone(),
            fields: record_fields(&rec),
            owner: rec.labels.get(super::reconcile::STACK_LABEL).cloned(),
            last_applied: rec
                .annotations
                .get(super::reconcile::LAST_APPLIED)
                .and_then(|raw| super::reconcile::decode_last_applied(raw)),
        })
        .collect())
}

/// The record's owner token, generating one when the record has none yet.
/// A NEW record gets a token; an OLD record (written before tokens existed)
/// keeps none on teardown — see [`remove_for_replace`] — but gets one on its
/// next apply, since its unmarked objects are refused either way.
fn owner_mark(rec: &mut NetworkGatewayRecord) -> Result<OwnerMark> {
    if rec.owner.is_empty() {
        let mut bytes = [0u8; 16];
        delonix_state::cred_vault::random_bytes(&mut bytes)?;
        rec.owner = OwnerMark::from_random(&bytes).token().to_string();
    }
    Ok(OwnerMark::new(&rec.owner)?)
}

/// Every entry of `old` whose key is not in `new`, then `new` — the
/// write-ahead list: what a teardown after a failure halfway must look for.
fn union_by<T: Clone>(old: &[T], new: &[T], key: impl Fn(&T) -> &str) -> Vec<T> {
    let mut out: Vec<T> = old
        .iter()
        .filter(|o| !new.iter().any(|n| key(n) == key(o)))
        .cloned()
        .collect();
    out.extend(new.iter().cloned());
    out
}

/// Applies one document: refuses if the appliance has changes staged that
/// are not this engine's, ensures every declared alias, then every declared
/// rule (aliases first — a rule referencing one that does not exist yet is
/// refused by a real appliance, measured live in ADR-0051 Phase 2), commits,
/// and overwrites the registry record — preserving any existing ownership
/// stamp, the same two-step apply-then-stamp order every other ownable Kind
/// here follows.
///
/// The record is saved BEFORE the first remote write too (write-ahead): the
/// owner token, and every alias/rule about to be ensured. A teardown after a
/// failure halfway then looks for all of them — and removes only those that
/// carry the mark, so an entry that was never created costs nothing.
fn apply_one(doc: &ManifestDoc) -> Result<()> {
    let spec: NetworkGatewaySpec = manifest::spec_of(doc)?;
    let aliases = spec
        .aliases
        .iter()
        .map(to_alias)
        .collect::<Result<Vec<_>>>()?;

    let name = doc.metadata.name.clone();
    let s = store()?;
    let mut rec = s.load(&name).unwrap_or_default();
    let named = spec.provider.as_deref();
    if let Some(new) = named.filter(|n| !rec.provider.is_empty() && *n != rec.provider) {
        return Err(Error::Conflict(super::po::tf(
            "networkgateway/{name} is recorded on provider '{old}', not '{new}' — replace the \
             document (`--replace NetworkGateway/{name}`) to move it",
            &[("name", &name), ("old", &rec.provider), ("new", new)],
        )));
    }
    let (provider_id, provider) = resolve_provider(named, &rec.provider)?;
    let owner = owner_mark(&mut rec)?;
    rec.name = name.clone();
    rec.provider = provider_id.to_string();
    rec.aliases = union_by(&rec.aliases, &spec.aliases, |a| a.name.as_str());
    rec.rules = union_by(&rec.rules, &spec.rules, |r| r.description.as_str());
    s.save(&name, &rec)?;

    provider.check_no_foreign_pending()?;
    for a in &aliases {
        provider.ensure_alias(a, &owner)?;
    }
    for r in &spec.rules {
        provider.ensure_rule(&to_rule(r), &owner)?;
    }
    provider.commit()?;

    rec.aliases = spec.aliases.clone();
    rec.rules = spec.rules.clone();
    s.save(&name, &rec)?;
    println!(
        "{}",
        super::po::tf(
            "networkgateway/{name}: {aliases} alias(es), {rules} rule(s) on '{provider}'",
            &[
                ("name", &name),
                ("aliases", &spec.aliases.len().to_string()),
                ("rules", &spec.rules.len().to_string()),
                ("provider", provider_id),
            ],
        )
    );
    Ok(())
}

/// Applies every `kind: NetworkGateway` document.
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, k::NETWORK_GATEWAY) {
        apply_one(doc)?;
    }
    Ok(())
}

/// `converge_and_stamp`'s live-update path — same rationale as
/// `service::converge_doc`: `apply_one` already fully re-ensures the
/// declared state, so converging IS applying.
pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    apply_one(doc)
}

/// Records ownership + last-applied — mirrors `service::stamp`, on the
/// record's OWN registry entry.
pub(crate) fn stamp(name: &str, stack: &str, fields: &BTreeMap<String, String>) -> Result<()> {
    let s = store()?;
    let mut rec = s
        .load(name)
        .map_err(|_| Error::NotFound(format!("network gateway: {name}")))?;
    rec.labels
        .insert(super::reconcile::STACK_LABEL.to_string(), stack.to_string());
    rec.labels.insert(
        super::reconcile::MANAGED_BY.to_string(),
        "delonix".to_string(),
    );
    rec.annotations.insert(
        super::reconcile::LAST_APPLIED.to_string(),
        super::reconcile::encode_last_applied(fields),
    );
    s.save(name, &rec).map_err(Into::into)
}

/// `--prune`/`stack destroy`'s teardown, and the generic `delete
/// networkgateways` verb: removes every rule then every alias the registry
/// last recorded (not just what a NEW spec says — a document being removed
/// entirely has no new spec to consult), commits, then drops the record.
/// Idempotent: a name with no record is not an error (`JsonStore::load`'s
/// absence maps to nothing to remove).
///
/// Removes only what carries the record's owner mark; an alias/rule of the
/// same name that does not is left on the appliance, and a line says so.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    let s = store()?;
    let Ok(rec) = s.load(name) else {
        return Ok(());
    };
    if rec.owner.is_empty() {
        // Written before owner tokens: nothing on the appliance is provably
        // this record's, so nothing there is touched.
        for r in &rec.rules {
            report_left(
                name,
                "rule",
                &r.description,
                "no owner mark (record predates marks)",
            );
        }
        for a in &rec.aliases {
            report_left(
                name,
                "alias",
                &a.name,
                "no owner mark (record predates marks)",
            );
        }
        return s.remove(name).map_err(Into::into);
    }
    if rec.provider == LEGACY_NATIVE {
        // Written by a build that still registered the `native` provider
        // (ADR-0059 F2b took it away). It refused every write, so nothing
        // on any appliance can carry this record's mark: the record is all
        // there is to remove.
        return s.remove(name).map_err(Into::into);
    }
    let owner = OwnerMark::new(&rec.owner)?;
    let (_, provider) = resolve_provider(None, &rec.provider)?;
    provider.check_no_foreign_pending()?;
    for r in &rec.rules {
        if let RemoveOutcome::NotOwned(who) = provider.remove_rule(&r.description, &owner)? {
            report_left(name, "rule", &r.description, &who.describe());
        }
    }
    for a in &rec.aliases {
        if let RemoveOutcome::NotOwned(who) = provider.remove_alias(&a.name, &owner)? {
            report_left(name, "alias", &a.name, &who.describe());
        }
    }
    provider.commit()?;
    // The owner mark's own object (an OPNsense category) goes last; the
    // appliance refuses while anything still carries it, and that is said,
    // not forced.
    if let Err(e) = provider.release_owner(&owner) {
        report_left(name, "owner mark", &owner.label(), &e.to_string());
    }
    s.remove(name).map_err(Into::into)
}

/// The audible half of a teardown that skipped an object.
fn report_left(name: &str, kind: &str, object: &str, why: &str) {
    println!(
        "{}",
        super::po::tf(
            "networkgateway/{name}: {kind} '{object}' left on the appliance: {why}",
            &[
                ("name", name),
                ("kind", kind),
                ("object", object),
                ("why", why)
            ],
        )
    );
}

/// For `stack ls`/`describe`: declared vs. what the registry last recorded.
pub(crate) fn presence_of(doc: &ManifestDoc) -> (String, String) {
    let Some(rec) = store().ok().and_then(|s| s.load(&doc.metadata.name).ok()) else {
        return ("no".into(), "-".into());
    };
    (
        "yes".into(),
        super::po::tf(
            "{aliases} alias(es), {rules} rule(s)",
            &[
                ("aliases", &rec.aliases.len().to_string()),
                ("rules", &rec.rules.len().to_string()),
            ],
        ),
    )
}

/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: NetworkGatewaySpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

#[derive(serde::Serialize)]
struct NetworkGatewayLsRow {
    name: String,
    provider: String,
    aliases: usize,
    rules: usize,
    stack: Option<String>,
}

pub(crate) fn cmd_ls(format: OutputFormat) -> Result<()> {
    let format = super::config::resolve_output(&state_root(), format);
    let mut recs = store()?.list()?;
    recs.sort_by(|a, b| a.name.cmp(&b.name));

    let rows: Vec<NetworkGatewayLsRow> = recs
        .iter()
        .map(|r| NetworkGatewayLsRow {
            name: r.name.clone(),
            provider: r.provider.clone(),
            aliases: r.aliases.len(),
            rules: r.rules.len(),
            stack: r.labels.get(super::reconcile::STACK_LABEL).cloned(),
        })
        .collect();

    if format == OutputFormat::Json {
        return super::output::print_json(&rows);
    }

    let mut t = super::output::Table::new(&["NAME", "PROVIDER", "ALIASES", "RULES", "STACK"]);
    for r in &rows {
        t.row(vec![
            r.name.clone(),
            r.provider.clone(),
            r.aliases.to_string(),
            r.rules.to_string(),
            r.stack.clone().unwrap_or_else(|| "-".to_string()),
        ]);
    }
    t.drop_uninformative().print();
    Ok(())
}

pub(crate) fn cmd_describe(names: &[String]) -> Result<()> {
    let s = store()?;
    for name in names {
        let rec = s
            .load(name)
            .map_err(|_| Error::NotFound(format!("network gateway: {name}")))?;
        let mut d = super::output::Describe::new();
        d.field("Name", &rec.name);
        d.field("Provider", &rec.provider);
        d.field("Aliases", rec.aliases.len().to_string());
        for a in &rec.aliases {
            d.field(
                "  Alias",
                format!("{} ({}): {}", a.name, a.kind, a.content.join(", ")),
            );
        }
        d.field("Rules", rec.rules.len().to_string());
        for r in &rec.rules {
            d.field(
                "  Rule",
                format!(
                    "{}: {} -> {} ({})",
                    r.description,
                    r.source,
                    r.destination,
                    r.protocol.as_deref().unwrap_or("any")
                ),
            );
        }
        d.field_opt("Stack", rec.labels.get(super::reconcile::STACK_LABEL));
        d.field_opt("Managed by", rec.labels.get(super::reconcile::MANAGED_BY));
        d.print();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_field_is_order_independent() {
        let a = vec![
            GatewayAliasSpec {
                name: "b".into(),
                kind: "host".into(),
                content: vec!["10.0.0.1".into()],
                description: String::new(),
            },
            GatewayAliasSpec {
                name: "a".into(),
                kind: "host".into(),
                content: vec!["10.0.0.2".into()],
                description: String::new(),
            },
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(aliases_field(&a), aliases_field(&b));
    }

    #[test]
    fn rules_field_is_order_independent() {
        let r = vec![
            GatewayRuleSpec {
                description: "b".into(),
                source: "any".into(),
                destination: "10.0.0.0/24".into(),
                protocol: None,
            },
            GatewayRuleSpec {
                description: "a".into(),
                source: "any".into(),
                destination: "10.0.1.0/24".into(),
                protocol: Some("TCP".into()),
            },
        ];
        let mut r2 = r.clone();
        r2.reverse();
        assert_eq!(rules_field(&r), rules_field(&r2));
    }

    #[test]
    fn alias_kind_refuses_anything_but_host_or_network() {
        assert!(alias_kind("host").is_ok());
        assert!(alias_kind("network").is_ok());
        assert!(alias_kind("port").is_err());
    }

    #[test]
    fn a_named_provider_nobody_registered_names_what_is_known() {
        let msg = match resolve_provider(Some("this-does-not-exist-at-all"), "") {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("this-does-not-exist-at-all"), "{msg}");
        assert!(msg.contains("known: "), "{msg}");
    }
}
