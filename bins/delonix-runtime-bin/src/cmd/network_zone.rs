//! `kind: NetworkZone` — declares a cluster-native SDN zone and the vnets
//! inside it, realized by whichever `NetworkZoneProvider` the RUNTIME has
//! configured (ADR-0049 addendum, closing the gap D3 names).
//!
//! **No `provider` field, deliberately — unlike `kind: NetworkGateway`
//! (ADR-0051).** The owner's own framing (this decision's discovery
//! conversation): the Kind stays transparent to WHICH infrastructure
//! realizes it. `cmd::network_zone_providers::register_configured` reads
//! `DELONIX_PROXMOX_*` once at startup and registers what it finds;
//! `delonix_sdn::network_zone::active_network_zone_provider` resolves by
//! COUNT (zero/one/ambiguous), never by a name this document would have to
//! carry. A tenant applying this manifest never learns it runs on Proxmox.
//!
//! **`metadata.name` is the ZONE's own Proxmox SDN id** (`pve-sdn-id`: a
//! lowercase letter then up to 7 more lowercase letters/digits — the
//! provider validates the exact format, this Kind does not repeat that
//! rule). `spec.vnets[]` are the vnets inside it.
//!
//! **Its own registry**, the same reason `kind: NetworkGateway`/`kind:
//! Service` have one: the target is a Proxmox cluster's PENDING SDN
//! configuration, which carries none of this engine's `delonix.io/stack`
//! labels to stamp.
//!
//! **No update-in-place**: `sdn.rs` (the Proxmox client) has no
//! `set_sdn_zone`/`set_sdn_vnet` route — only create/delete. `apply()` only
//! ENSURES every vnet CURRENTLY declared is present; it never retracts one
//! dropped from the list while others stay. The full document's teardown
//! (`--replace NetworkZone/<name>`, or dropping it under `stack apply
//! --prune`) removes every vnet the registry last recorded, then the zone.
//!
//! **Teardown order is vnets, then the zone, then ONE commit**: a real
//! Proxmox node refuses to delete a zone a vnet still references
//! (`delonix_proxmox`'s own `Client::delete_sdn_zone` doc comment) — the
//! same referential-integrity reason `kind: NetworkGateway` removes rules
//! before aliases.

use std::collections::BTreeMap;

use super::kinds as k;
use super::manifest::{self, ManifestDoc};
use super::output::OutputFormat;
use super::util::state_root;
use delonix_model::{Error, Result};
use delonix_sdn::network_zone::{NetworkZoneProvider, NetworkZoneSpec, VNetSpec};
use delonix_state::JsonStore;

/// `spec` of `kind: NetworkZone`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct NetworkZoneSpecDoc {
    #[serde(default)]
    pub vnets: Vec<VNetSpecInput>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct VNetSpecInput {
    pub name: String,
    #[serde(default)]
    pub alias: Option<String>,
}

/// Known fields of the `spec` (drift-guard, the pattern every other Kind's
/// spec uses).
pub const NETWORK_ZONE_SPEC_FIELDS: &[&str] = &["vnets"];

/// Fields the reconciler compares.
pub const RECONCILED_NETWORK_ZONE_FIELDS: &[&str] = &["vnets"];

/// A registered record: what was last declared, plus the ownership fields
/// every ownable Kind's own registry carries (mirrors `NetworkGatewayRecord`).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct NetworkZoneRecord {
    name: String,
    #[serde(default)]
    vnets: Vec<VNetSpecInput>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

fn store() -> Result<JsonStore<NetworkZoneRecord>> {
    JsonStore::open(state_root().join("network-zones")).map_err(Into::into)
}

fn resolve_provider() -> Result<Box<dyn NetworkZoneProvider>> {
    delonix_sdn::network_zone::active_network_zone_provider().map_err(Into::into)
}

/// A comparable summary of the vnet list — sorted so two applies of an
/// unchanged spec never drift because a manifest happened to list them in a
/// different order (same reasoning as `network_gateway::aliases_field`).
fn vnets_field(vnets: &[VNetSpecInput]) -> String {
    let mut items: Vec<String> = vnets
        .iter()
        .map(|v| format!("{}|{}", v.name, v.alias.as_deref().unwrap_or("")))
        .collect();
    items.sort();
    items.join(";")
}

fn record_fields(rec: &NetworkZoneRecord) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("vnets".into(), vnets_field(&rec.vnets));
    f
}

/// What the manifest declares, for the reconciler. `ownable: true` — a
/// `NetworkZone` has its own durable identity (name), same as `Service`/
/// `NetworkGateway`.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: NetworkZoneSpecDoc = manifest::spec_of(doc)?;
    let mut fields = BTreeMap::new();
    fields.insert("vnets".into(), vnets_field(&spec.vnets));
    Ok(super::reconcile::Desired {
        kind: k::NETWORK_ZONE.into(),
        name: doc.metadata.name.clone(),
        fields,
        converges: true,
        ownable: true,
    })
}

/// Every declared `NetworkZone` — the enumeration `--prune` needs, same
/// reasoning as `network_gateway::actual`.
pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    Ok(store()?
        .list()?
        .into_iter()
        .map(|rec| super::reconcile::Actual {
            kind: k::NETWORK_ZONE.into(),
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

/// Applies one document: ensures the zone, then every declared vnet inside
/// it (zone first — a vnet referencing one that does not exist is refused
/// by the provider), commits once, and overwrites the registry record —
/// preserving any existing ownership stamp, the same two-step apply-then-
/// stamp order every other ownable Kind here follows.
fn apply_one(doc: &ManifestDoc) -> Result<()> {
    let spec: NetworkZoneSpecDoc = manifest::spec_of(doc)?;
    let name = doc.metadata.name.clone();
    let provider = resolve_provider()?;

    provider.ensure_zone(&NetworkZoneSpec { name: name.clone() })?;
    for v in &spec.vnets {
        provider.ensure_vnet(&VNetSpec {
            name: v.name.clone(),
            zone: name.clone(),
            alias: v.alias.clone(),
        })?;
    }
    provider.commit()?;

    let s = store()?;
    let mut rec = s.load(&name).unwrap_or_default();
    rec.name = name.clone();
    rec.vnets = spec.vnets.clone();
    s.save(&name, &rec)?;
    println!(
        "{}",
        super::po::tf(
            "networkzone/{name}: {vnets} vnet(s) on '{provider}'",
            &[
                ("name", &name),
                ("vnets", &spec.vnets.len().to_string()),
                ("provider", provider.id()),
            ],
        )
    );
    Ok(())
}

/// Applies every `kind: NetworkZone` document.
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, k::NETWORK_ZONE) {
        apply_one(doc)?;
    }
    Ok(())
}

/// `converge_and_stamp`'s live-update path — `apply_one` already fully
/// re-ensures the declared state, so converging IS applying.
pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    apply_one(doc)
}

/// Records ownership + last-applied — mirrors `network_gateway::stamp`.
pub(crate) fn stamp(name: &str, stack: &str, fields: &BTreeMap<String, String>) -> Result<()> {
    let s = store()?;
    let mut rec = s
        .load(name)
        .map_err(|_| Error::NotFound(format!("network zone: {name}")))?;
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
/// networkzones` verb: removes every vnet the registry last recorded (not
/// just what a NEW spec says — a document being removed entirely has no new
/// spec to consult), then the zone, commits once, then drops the record.
/// Idempotent: a name with no record is not an error.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    let s = store()?;
    let Ok(rec) = s.load(name) else {
        return Ok(());
    };
    let provider = resolve_provider()?;
    for v in &rec.vnets {
        provider.remove_vnet(&v.name)?;
    }
    provider.remove_zone(name)?;
    provider.commit()?;
    s.remove(name).map_err(Into::into)
}

/// For `stack ls`/`describe`: declared vs. what the registry last recorded.
pub(crate) fn presence_of(doc: &ManifestDoc) -> (String, String) {
    let Some(rec) = store().ok().and_then(|s| s.load(&doc.metadata.name).ok()) else {
        return ("no".into(), "-".into());
    };
    (
        "yes".into(),
        super::po::tf(
            "{vnets} vnet(s)",
            &[("vnets", &rec.vnets.len().to_string())],
        ),
    )
}

/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: NetworkZoneSpecDoc = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

#[derive(serde::Serialize)]
struct NetworkZoneLsRow {
    name: String,
    vnets: usize,
    stack: Option<String>,
}

pub(crate) fn cmd_ls(format: OutputFormat) -> Result<()> {
    let format = super::config::resolve_output(&state_root(), format);
    let mut recs = store()?.list()?;
    recs.sort_by(|a, b| a.name.cmp(&b.name));

    let rows: Vec<NetworkZoneLsRow> = recs
        .iter()
        .map(|r| NetworkZoneLsRow {
            name: r.name.clone(),
            vnets: r.vnets.len(),
            stack: r.labels.get(super::reconcile::STACK_LABEL).cloned(),
        })
        .collect();

    if format == OutputFormat::Json {
        return super::output::print_json(&rows);
    }

    let mut t = super::output::Table::new(&["NAME", "VNETS", "STACK"]);
    for r in &rows {
        t.row(vec![
            r.name.clone(),
            r.vnets.to_string(),
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
            .map_err(|_| Error::NotFound(format!("network zone: {name}")))?;
        let mut d = super::output::Describe::new();
        d.field("Name", &rec.name);
        d.field("Vnets", rec.vnets.len().to_string());
        for v in &rec.vnets {
            d.field(
                "  Vnet",
                format!("{} ({})", v.name, v.alias.as_deref().unwrap_or("-")),
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
    fn vnets_field_is_order_independent() {
        let v = vec![
            VNetSpecInput {
                name: "b".into(),
                alias: None,
            },
            VNetSpecInput {
                name: "a".into(),
                alias: Some("Prod".into()),
            },
        ];
        let mut v2 = v.clone();
        v2.reverse();
        assert_eq!(vnets_field(&v), vnets_field(&v2));
    }

    #[test]
    fn resolve_provider_names_what_to_configure_when_none_is() {
        // This test does not register anything of its own — it only asserts
        // the SHAPE of the refusal when the process-wide registry happens to
        // be empty. If another test in this binary registered a provider
        // first (the registry is process-wide), this is a false negative to
        // skip rather than a flake to chase — the real guarantee (zero
        // registered -> named refusal) is proven without the shared static
        // by `delonix_sdn::network_zone::tests::zero_registered_names_what_to_configure`.
        if !delonix_sdn::network_zone::network_zone_provider_ids().is_empty() {
            eprintln!("SKIP: a provider is already registered in this process");
            return;
        }
        // `Box<dyn NetworkZoneProvider>` is not `Debug`, so `unwrap_err()`
        // does not apply — match instead.
        match resolve_provider() {
            Ok(_) => panic!("expected a refusal"),
            Err(e) => assert!(e.to_string().contains("DELONIX_PROXMOX_URL"), "{e}"),
        }
    }
}
