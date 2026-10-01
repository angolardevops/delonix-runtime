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
    /// Policies for a target behind the gateway, in the same shape a
    /// `NetworkPolicy` has, lowered to the appliance's filter rules through
    /// the policy IR (ADR-0059 F3d/F3e).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<GatewayPolicySpec>,
}

/// One direction of the policy for one target (an alias name, a prefix or
/// an address the appliance resolves). Each rule lands as
/// `<name>#<n>` at `sequence + n - 1`, and the default verdict as
/// `<name>#default` right after: pf loads filter rules in `sequence` order
/// (measured on OPNsense 26.1.2_5), so the first match here is the
/// appliance's.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GatewayPolicySpec {
    /// The prefix of every rule's identity on the appliance.
    pub name: String,
    pub target: String,
    /// `ingress` (traffic to the target) or `egress` (traffic from it).
    pub direction: String,
    /// `allow` or `deny`; `deny` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_policy: Option<String>,
    /// The first rule's position among the appliance's filter rules.
    pub sequence: u32,
    #[serde(default)]
    pub rules: Vec<GatewayPolicyRuleSpec>,
}

/// One rule of a [`GatewayPolicySpec`]: the `NetworkPolicy` rule shape, plus
/// the two fields a perimeter appliance holds and the node's chain does not.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GatewayPolicyRuleSpec {
    /// `tcp`, `udp` or `any`; `any` when omitted. With a port, `any` is TCP
    /// and UDP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto: Option<String>,
    /// A port, an `n-m` range, or `*` (every port, the default).
    #[serde(default = "every_port")]
    pub port: String,
    /// The other end of an `ingress` rule: an IPv4 address or prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The other end of an `egress` rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// `allow` or `deny`; `allow` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Logs every match.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub log: bool,
    /// `false` keeps no state for the matched flow; `true` when omitted.
    #[serde(default = "stateful_default")]
    pub stateful: bool,
}

fn every_port() -> String {
    "*".to_string()
}

fn stateful_default() -> bool {
    true
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
pub const NETWORK_GATEWAY_SPEC_FIELDS: &[&str] = &["provider", "aliases", "rules", "policies"];

/// Fields the reconciler compares.
///
/// `remote` is what the appliance holds under the record's owner mark,
/// observed on every plan (ADR-0059 D4): `in sync`, or each difference from
/// what the record declared. The manifest always wants `in sync`, so a rule
/// changed or deleted on the appliance by hand is drift (`stack plan
/// --detailed-exitcode` answers 2, `delonix drift` names it).
pub const RECONCILED_NETWORK_GATEWAY_FIELDS: &[&str] = &[
    "provider", "aliases", "rules", "policies", "remote", "applied",
];

/// The `remote` field of a record that matches the appliance.
const IN_SYNC: &str = "in sync";

/// The `applied` field of a record whose last apply ran to its end. Anything
/// else is where an apply stopped (ADR-0059 D4): the manifest always wants
/// `complete`, the field converges live, and applying again resumes.
const COMPLETE: &str = "complete";

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
    /// The policies last declared: their rule identities are recomputed
    /// from them on teardown.
    #[serde(default)]
    policies: Vec<GatewayPolicySpec>,
    /// The steps of the last apply or teardown, each written before it ran
    /// and settled after (ADR-0059 D4). An unsettled one is where a process
    /// died.
    #[serde(default)]
    ledger: delonix_networking::ledger::StepLedger,
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
        ..Default::default()
    }
}

/// A policy as the appliance's filter rules: built as policy IR through the
/// same parse a container's firewall record uses, then lowered by
/// `gateway_rules`. Everything is validated before anything is sent.
fn policy_rules(p: &GatewayPolicySpec) -> Result<Vec<GatewayRule>> {
    use delonix_net_rules::policy as ir;
    let invalid = |why: String| Error::Invalid(format!("policy '{}': {why}", p.name));
    if p.name.is_empty()
        || !p
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(invalid(
            "a name is letters, digits, '.', '_' or '-' — it prefixes every rule's identity".into(),
        ));
    }
    if p.target.trim().is_empty() {
        return Err(invalid("target is empty".into()));
    }
    let (dir, direction) = match p.direction.as_str() {
        "ingress" => ("in", ir::Direction::Ingress),
        "egress" => ("out", ir::Direction::Egress),
        other => {
            return Err(invalid(format!(
                "direction '{other}' is neither ingress nor egress"
            )))
        }
    };
    let default = match p.default_policy.as_deref().unwrap_or("deny") {
        "allow" => ir::Action::Allow,
        "deny" => ir::Action::Deny,
        other => {
            return Err(invalid(format!(
                "defaultPolicy '{other}' is neither allow nor deny"
            )))
        }
    };
    let mut rules = Vec::with_capacity(p.rules.len());
    for (i, r) in p.rules.iter().enumerate() {
        let n = i + 1;
        let (peer, wrong) = match direction {
            ir::Direction::Ingress => (&r.from, &r.to),
            ir::Direction::Egress => (&r.to, &r.from),
        };
        if wrong.is_some() {
            return Err(invalid(format!(
                "rule #{n}: an {} rule names its other end with `{}`",
                p.direction,
                if dir == "in" { "from" } else { "to" }
            )));
        }
        let stored = delonix_model::records::FwRule {
            dir: dir.to_string(),
            proto: r.proto.clone().unwrap_or_else(|| "any".into()),
            port: r.port.clone(),
            src: peer.clone().unwrap_or_default(),
            action: r.action.clone().unwrap_or_else(|| "allow".into()),
            ..Default::default()
        };
        let mut rule = delonix_networking::policy::rule_of(&stored)
            .map_err(|why| invalid(format!("rule #{n}: {why}")))?;
        rule.log = r.log;
        rule.stateful = r.stateful;
        rules.push(rule);
    }
    let policy = ir::Policy {
        direction,
        default,
        rules,
    };
    delonix_networking::policy::gateway_rules(&p.target, &policy, &p.name, p.sequence)
        .map_err(Into::into)
}

/// Every policy's rules, and a refusal when two policies share a name or
/// their positions overlap (two rules at one `sequence` leave pf's order to
/// the appliance).
fn all_policy_rules(policies: &[GatewayPolicySpec]) -> Result<Vec<GatewayRule>> {
    let mut out = Vec::new();
    let mut spans: Vec<(u32, u32, &str)> = Vec::new();
    for p in policies {
        if spans.iter().any(|(_, _, n)| *n == p.name) {
            return Err(Error::Invalid(format!(
                "two policies are named '{}' — the name is every rule's identity",
                p.name
            )));
        }
        let rules = policy_rules(p)?;
        let last = p.sequence + rules.len() as u32 - 1;
        if let Some((a, b, other)) = spans
            .iter()
            .find(|(a, b, _)| p.sequence <= *b && *a <= last)
        {
            return Err(Error::Invalid(format!(
                "policy '{}' takes sequence {}-{last}, which overlaps policy '{other}' ({a}-{b})",
                p.name, p.sequence
            )));
        }
        spans.push((p.sequence, last, &p.name));
        out.extend(rules);
    }
    Ok(out)
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
    .map_err(|e| {
        let wanted = named.or(Some(recorded).filter(|r| !r.is_empty()));
        Error::from(e).with_context(delonix_networking::resolve::context(
            Role::Gateway,
            wanted.or(default.as_deref()),
            Some("resolve_provider"),
        ))
    })
}

/// Adds where a provider call failed to its error (ADR-0059 D5): the
/// provider, the gateway role and the step.
fn at<'a>(provider: &'a str, step: &'static str) -> impl FnOnce(Error) -> Error + 'a {
    move |e| {
        e.with_context(delonix_networking::resolve::context(
            delonix_networking::resolve::Role::Gateway,
            Some(provider),
            Some(step),
        ))
    }
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

/// A policy list as one comparable string: the JSON of each policy, sorted
/// by name, so the order a manifest lists them in is not drift.
fn policies_field(policies: &[GatewayPolicySpec]) -> String {
    let mut items: Vec<String> = policies
        .iter()
        .map(|p| serde_json::to_string(p).unwrap_or_default())
        .collect();
    items.sort();
    items.join(";")
}

fn record_fields(rec: &NetworkGatewayRecord) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("provider".into(), rec.provider.clone());
    f.insert("aliases".into(), aliases_field(&rec.aliases));
    f.insert("rules".into(), rules_field(&rec.rules));
    f.insert("policies".into(), policies_field(&rec.policies));
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
    fields.insert("policies".into(), policies_field(&spec.policies));
    fields.insert("remote".into(), IN_SYNC.into());
    fields.insert("applied".into(), COMPLETE.into());
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
    store()?
        .list()?
        .into_iter()
        .map(|rec| {
            let mut fields = record_fields(&rec);
            fields.insert("remote".into(), remote_field(&rec)?);
            fields.insert(
                "applied".into(),
                rec.ledger
                    .interruption()
                    .unwrap_or_else(|| COMPLETE.to_string()),
            );
            Ok(super::reconcile::Actual {
                kind: k::NETWORK_GATEWAY.into(),
                name: rec.name.clone(),
                fields,
                owner: rec.labels.get(super::reconcile::STACK_LABEL).cloned(),
                last_applied: rec
                    .annotations
                    .get(super::reconcile::LAST_APPLIED)
                    .and_then(|raw| super::reconcile::decode_last_applied(raw)),
            })
        })
        .collect()
}

/// What the appliance holds under the record's owner mark, compared with
/// what the record declared (ADR-0059 D4, observe; read-only). A record
/// without a mark, or from the retired `native` provider, owns nothing that
/// can be observed, and says so instead of claiming to be in sync.
fn remote_field(rec: &NetworkGatewayRecord) -> Result<String> {
    if rec.ledger.is_interrupted() {
        // An apply stopped mid-way: what is missing on the appliance is what
        // it had not reached, and the `applied` field says so. Comparing here
        // too would plan a replace for what applying again finishes.
        return Ok(IN_SYNC.into());
    }
    if rec.owner.is_empty() {
        return Ok("not observed: the record predates owner marks".into());
    }
    if rec.provider == LEGACY_NATIVE {
        return Ok("not observed: the retired native provider owns nothing remote".into());
    }
    let owner = OwnerMark::new(&rec.owner)?;
    let aliases = rec
        .aliases
        .iter()
        .map(to_alias)
        .collect::<Result<Vec<_>>>()?;
    let mut rules: Vec<GatewayRule> = rec.rules.iter().map(to_rule).collect();
    rules.extend(all_policy_rules(&rec.policies)?);
    let (provider_id, provider) = resolve_provider(None, &rec.provider)?;
    let observed = provider
        .observe(&owner)
        .map_err(at(provider_id, "observe"))?;
    let drift = delonix_sdn::gateway::gateway_drift(&aliases, &rules, &observed);
    Ok(if drift.is_empty() {
        IN_SYNC.to_string()
    } else {
        drift.join("; ")
    })
}

/// The capabilities a document needs from its provider: what its apply
/// uses, and what its plan digest covers (ADR-0059 D4).
fn required_capabilities(
    spec: &NetworkGatewaySpec,
    policy_rules: &[GatewayRule],
) -> Vec<delonix_compute::capability::Capability> {
    use delonix_compute::capability::Capability as C;
    let mut used = vec![C::NetApplyStaged, C::NetOwnershipMarker, C::NetObserve];
    if !spec.aliases.is_empty() {
        used.push(C::NetGatewayAlias);
    }
    if !spec.rules.is_empty() || !policy_rules.is_empty() {
        used.push(C::NetGatewayFilter);
    }
    if !policy_rules.is_empty() {
        used.push(C::NetGatewayRuleOrder);
    }
    if policy_rules.iter().any(|r| r.log) {
        used.push(C::FirewallLogging);
    }
    if policy_rules.iter().any(|r| !r.stateful) {
        used.push(C::FirewallStateless);
    }
    used
}

/// The digest of this document's plan (ADR-0059 D4): what the manifest
/// declares, what the appliance holds under the record's mark right now, the
/// provider, the catalog version and the states of the capabilities the
/// document uses. `None` when no provider can be resolved — a plan nobody
/// could apply has nothing to pin.
///
/// Read-only. A document never applied has no mark, so nothing on the
/// appliance is its own and its observed state is empty.
pub(crate) fn plan_digest(doc: &ManifestDoc) -> Result<Option<String>> {
    use delonix_compute::capability::CATALOG_VERSION;
    let spec: NetworkGatewaySpec = manifest::spec_of(doc)?;
    let rec = store()?.load(&doc.metadata.name).unwrap_or_default();
    if rec.provider == LEGACY_NATIVE {
        return Ok(None);
    }
    let Ok((provider_id, provider)) = resolve_provider(spec.provider.as_deref(), &rec.provider)
    else {
        return Ok(None);
    };
    let observed = if rec.owner.is_empty() {
        Default::default()
    } else {
        provider
            .observe(&OwnerMark::new(&rec.owner)?)
            .map_err(at(provider_id, "observe"))?
    };
    let policy_rules = all_policy_rules(&spec.policies)?;
    let used = required_capabilities(&spec, &policy_rules);
    let report = provider.capabilities();
    let states: BTreeMap<String, String> = report
        .capabilities
        .iter()
        .filter(|c| used.contains(&c.capability))
        .map(|c| (c.capability.name().to_string(), c.state.label().to_string()))
        .collect();
    let mut intent = desired(doc)?.fields;
    // The constant the reconciler compares against is not part of the intent.
    intent.remove("remote");
    intent.remove("applied");
    Ok(Some(delonix_networking::plan::plan_digest(
        &intent,
        &delonix_networking::plan::gateway_fingerprint(&observed),
        provider_id,
        CATALOG_VERSION,
        &states,
    )))
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

/// Runs one step under the record's ledger: opened and SAVED before it runs,
/// settled and saved after (ADR-0059 D4). A process killed inside `run`
/// leaves the step unsettled on disk, which is how the next plan knows.
fn step<T>(
    s: &JsonStore<NetworkGatewayRecord>,
    rec: &mut NetworkGatewayRecord,
    provider: &str,
    op: &'static str,
    target: &str,
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let id = rec.ledger.open(op, target);
    s.save(&rec.name, rec)?;
    let out = run().map_err(at(provider, op));
    rec.ledger
        .settle(id, out.as_ref().map(|_| ()).map_err(|e| e.to_string()));
    s.save(&rec.name, rec)?;
    out
}

/// When the record's last run stopped mid-way, takes over what it left
/// staged on the provider, and says so.
fn resume_interrupted(
    rec: &NetworkGatewayRecord,
    provider_id: &str,
    provider: &dyn GatewayProvider,
    owner: &OwnerMark,
) -> Result<()> {
    let Some(why) = rec.ledger.interruption() else {
        return Ok(());
    };
    let adopted = provider
        .adopt_pending(owner, &rec.ledger.removing)
        .map_err(at(provider_id, "adopt_pending"))?;
    println!(
        "{}",
        super::po::tf(
            "networkgateway/{name}: the last run was {why} — resuming, with {n} staged change(s) of it adopted",
            &[
                ("name", &rec.name),
                ("why", &why),
                ("n", &adopted.len().to_string()),
            ],
        )
    );
    Ok(())
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
    let policy_rules = all_policy_rules(&spec.policies)?;

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
    // Validate (ADR-0059 D4): before the record or the appliance is touched.
    delonix_networking::resolve::require_capabilities(
        &format!("NetworkGateway/{name}"),
        &provider.capabilities(),
        &required_capabilities(&spec, &policy_rules),
    )?;
    let owner = owner_mark(&mut rec)?;
    rec.name = name.clone();
    rec.provider = provider_id.to_string();
    rec.aliases = union_by(&rec.aliases, &spec.aliases, |a| a.name.as_str());
    rec.rules = union_by(&rec.rules, &spec.rules, |r| r.description.as_str());
    rec.policies = union_by(&rec.policies, &spec.policies, |p| p.name.as_str());
    s.save(&name, &rec)?;

    resume_interrupted(&rec, provider_id, provider.as_ref(), &owner)?;
    rec.ledger = Default::default();
    provider
        .check_no_foreign_pending()
        .map_err(at(provider_id, "check_no_foreign_pending"))?;
    for a in &aliases {
        step(&s, &mut rec, provider_id, "ensure_alias", &a.name, || {
            provider.ensure_alias(a, &owner)
        })?;
    }
    for r in spec
        .rules
        .iter()
        .map(to_rule)
        .chain(policy_rules.iter().cloned())
    {
        step(
            &s,
            &mut rec,
            provider_id,
            "ensure_rule",
            &r.description,
            || provider.ensure_rule(&r, &owner),
        )?;
    }
    step(&s, &mut rec, provider_id, "commit", "", || {
        provider.commit()
    })?;

    rec.ledger.finish();
    rec.aliases = spec.aliases.clone();
    rec.rules = spec.rules.clone();
    rec.policies = spec.policies.clone();
    s.save(&name, &rec)?;
    println!(
        "{}",
        super::po::tf(
            "networkgateway/{name}: {aliases} alias(es), {rules} rule(s) on '{provider}'",
            &[
                ("name", &name),
                ("aliases", &spec.aliases.len().to_string()),
                (
                    "rules",
                    &(spec.rules.len() + policy_rules.len()).to_string()
                ),
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
    // The policies' rule identities, recomputed from what the record last
    // declared. A policy the record holds was lowered before it was saved,
    // so lowering it again cannot fail short of a hand-edited record.
    let mut descriptions: Vec<String> = rec.rules.iter().map(|r| r.description.clone()).collect();
    for p in &rec.policies {
        descriptions.extend(policy_rules(p)?.into_iter().map(|r| r.description));
    }
    let (provider_id, provider) = resolve_provider(None, &rec.provider)?;
    let mut rec = rec;
    resume_interrupted(&rec, provider_id, provider.as_ref(), &owner)?;
    // The ids about to be deleted, saved BEFORE the first deletion: once a
    // rule is deleted it no longer carries a mark, and a teardown that dies
    // after staging the deletion is recognized by these.
    let mut removing = std::mem::take(&mut rec.ledger.removing);
    for id in provider
        .owned_rule_ids(&owner)
        .map_err(at(provider_id, "owned_rule_ids"))?
    {
        if !removing.contains(&id) {
            removing.push(id);
        }
    }
    rec.ledger = delonix_networking::ledger::StepLedger {
        steps: Vec::new(),
        finished: false,
        removing,
    };
    s.save(name, &rec)?;
    provider
        .check_no_foreign_pending()
        .map_err(at(provider_id, "check_no_foreign_pending"))?;
    for d in &descriptions {
        if let RemoveOutcome::NotOwned(who) =
            step(&s, &mut rec, provider_id, "remove_rule", d, || {
                provider.remove_rule(d, &owner)
            })?
        {
            report_left(name, "rule", d, &who.describe());
        }
    }
    for a in rec.aliases.clone() {
        if let RemoveOutcome::NotOwned(who) =
            step(&s, &mut rec, provider_id, "remove_alias", &a.name, || {
                provider.remove_alias(&a.name, &owner)
            })?
        {
            report_left(name, "alias", &a.name, &who.describe());
        }
    }
    step(&s, &mut rec, provider_id, "commit", "", || {
        provider.commit()
    })?;
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

    fn policy(yaml: &str) -> GatewayPolicySpec {
        serde_yaml::from_str(yaml).expect("a policy")
    }

    #[test]
    fn a_policy_lowers_through_the_ir_to_ordered_rules_and_a_default() {
        use delonix_sdn::gateway::GatewayAction;
        let p = policy(
            "name: web-in\ntarget: delonix_web\ndirection: ingress\nsequence: 30000\nrules:\n\
             - {proto: tcp, port: '22', from: 10.9.0.5/32, log: true}\n\
             - {port: '8000-8080', action: deny, stateful: false}\n",
        );
        let rules = policy_rules(&p).unwrap();
        let d: Vec<(&str, Option<u32>)> = rules
            .iter()
            .map(|r| (r.description.as_str(), r.sequence))
            .collect();
        assert_eq!(
            d,
            [
                ("web-in#1", Some(30000)),
                ("web-in#2", Some(30001)),
                ("web-in#default", Some(30002))
            ]
        );
        assert_eq!(
            (rules[0].source.as_str(), rules[0].destination.as_str()),
            ("10.9.0.5", "delonix_web")
        );
        assert!(rules[0].log && rules[0].stateful);
        assert_eq!(rules[1].protocol.as_deref(), Some("TCP/UDP"));
        assert_eq!(rules[1].action, GatewayAction::Block);
        assert!(!rules[1].stateful);
        assert_eq!(
            rules[2].action,
            GatewayAction::Block,
            "deny when defaultPolicy is omitted"
        );
    }

    #[test]
    fn a_policy_is_refused_before_anything_is_sent() {
        for (yaml, word) in [
            ("name: p\ntarget: t\ndirection: sideways\nsequence: 1\n", "sideways"),
            ("name: p\ntarget: t\ndirection: ingress\nsequence: 1\nrules:\n- {to: 10.0.0.0/8}\n", "`from`"),
            ("name: p\ntarget: t\ndirection: egress\nsequence: 1\nrules:\n- {from: 10.0.0.0/8}\n", "`to`"),
            ("name: p\ntarget: t\ndirection: ingress\nsequence: 1\nrules:\n- {proto: tcp, port: '90-80'}\n", "90-80"),
            ("name: p\ntarget: t\ndirection: ingress\nsequence: 1\nrules:\n- {proto: icmp}\n", "icmp"),
            ("name: 'a b'\ntarget: t\ndirection: ingress\nsequence: 1\n", "letters"),
            ("name: p\ntarget: ''\ndirection: ingress\nsequence: 1\n", "target"),
            ("name: p\ntarget: t\ndirection: ingress\ndefaultPolicy: maybe\nsequence: 1\n", "maybe"),
        ] {
            let e = policy_rules(&policy(yaml)).unwrap_err().to_string();
            assert!(e.contains(word), "{yaml}: {e}");
        }
    }

    #[test]
    fn two_policies_may_not_share_a_name_or_a_position() {
        let a = policy(
            "name: a\ntarget: t\ndirection: ingress\nsequence: 100\nrules:\n- {port: '22'}\n",
        );
        let overlap = policy("name: b\ntarget: t\ndirection: ingress\nsequence: 101\n");
        let e = all_policy_rules(&[a.clone(), overlap])
            .unwrap_err()
            .to_string();
        assert!(e.contains("overlaps policy 'a' (100-101)"), "{e}");
        let e = all_policy_rules(&[a.clone(), a.clone()])
            .unwrap_err()
            .to_string();
        assert!(e.contains("two policies are named 'a'"), "{e}");
        let after = policy("name: b\ntarget: t\ndirection: ingress\nsequence: 102\n");
        assert_eq!(all_policy_rules(&[a, after]).unwrap().len(), 3);
    }

    #[test]
    fn policies_field_is_order_independent() {
        let a = policy("name: a\ntarget: t\ndirection: ingress\nsequence: 100\n");
        let b = policy("name: b\ntarget: t\ndirection: egress\nsequence: 200\n");
        assert_eq!(
            policies_field(&[a.clone(), b.clone()]),
            policies_field(&[b, a])
        );
    }

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
