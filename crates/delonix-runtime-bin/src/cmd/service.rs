//! `kind: Service` — a selector-matched workload SET, load-balanced by DNS (ADR-0032).
//!
//! `ADR-0020` named `Service` as one of the six Kinds the CLI restructuring's
//! Phase CLI-2 needed before it could even start; five of the six shipped, this
//! is the sixth. It selects containers by label (`spec.selector.matchLabels`,
//! reusing `delonix_net::matches_labels` — the same pure function
//! `FirewallPolicy`'s own planned selector, ADR-0024, is meant to share rather
//! than grow a second implementation of) and publishes the matched set as
//! MULTIPLE DNS `A` records under `<name>.<namespace>.delonix.internal`,
//! round-robin rotated per query.
//!
//! **No VIP, no L4 dataplane, no new daemon.** The membership computation and
//! the DNS answer itself live in `delonix_net::infra` (`ServiceDef` registry +
//! `build_dns_index`'s Service pass + `dns_resolve_multi_for`) — this module is
//! the thin Kind-dispatch layer: parse the spec, write the registry entry,
//! answer the reconciler's questions. See `docs/adr/0032-service-kind-dns-round-robin.md`
//! for the full design and the deliberately-deferred pieces (a real L4 VIP,
//! `type: LoadBalancer`/`NodePort`/`ExternalName`, readiness-gated membership).
//!
//! **No new CLI leaf.** Reached the same way `kind: Dependency`/
//! `kind: NetworkAccessRule` already are — through `delonix apply -f`/
//! `delonix stack apply`, which dispatch by Kind — plus the generic `get`/
//! `describe`/`delete services` verbs (`cmd/verbs.rs`).

use std::collections::BTreeMap;

use super::kinds as k;
use super::manifest::{self, ManifestDoc};
use super::output::OutputFormat;
use super::util::open_stores;
use delonix_runtime_core::{Error, Result};

/// `spec` of `kind: Service`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct ServiceSpec {
    pub selector: ServiceSelector,
    /// The CONTAINER port every matched workload is expected to listen on —
    /// mirrors the "always container-side, post-DNAT" convention `net ingress
    /// allow`'s port already uses. There is no host-side or VIP-side port,
    /// because v1 has no VIP.
    pub port: u16,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct ServiceSelector {
    #[serde(default, rename = "matchLabels")]
    pub match_labels: BTreeMap<String, String>,
}

/// Known fields of the `spec` (drift-guard, same pattern every other Kind's
/// spec uses).
pub const SERVICE_SPEC_FIELDS: &[&str] = &["selector", "port"];

/// Fields the reconciler compares.
pub const RECONCILED_SERVICE_FIELDS: &[&str] = &["matchLabels", "port"];

fn service_fields(spec: &ServiceSpec) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    // A `BTreeMap` renders its keys in sorted order deterministically, so this
    // comparable form never spuriously drifts because two applies happened to
    // write the same labels in a different order.
    f.insert(
        "matchLabels".into(),
        spec.selector
            .match_labels
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(","),
    );
    f.insert("port".into(), spec.port.to_string());
    f
}

/// What the manifest declares, for the reconciler. `ownable: true` — a
/// `Service` has its own durable identity (name + namespace), so a stack CAN
/// own and prune one, same as `NetworkAccessRule`/`NetworkRoute`.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: ServiceSpec = manifest::spec_of(doc)?;
    Ok(super::reconcile::Desired {
        kind: k::SERVICE.into(),
        name: doc.metadata.name.clone(),
        fields: service_fields(&spec),
        converges: true,
        ownable: true,
    })
}

/// Every `Service` this node has declared — the enumeration `--prune` needs,
/// same reasoning as `netroute::actual`/`network_access_rule::actual`.
pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    Ok(delonix_net::infra::service_list()
        .into_iter()
        .map(|def| {
            let mut f = BTreeMap::new();
            f.insert(
                "matchLabels".into(),
                def.match_labels
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            f.insert("port".into(), def.port.to_string());
            super::reconcile::Actual {
                kind: k::SERVICE.into(),
                name: def.name.clone(),
                fields: f,
                owner: def.labels.get(super::reconcile::STACK_LABEL).cloned(),
                last_applied: def
                    .annotations
                    .get(super::reconcile::LAST_APPLIED)
                    .and_then(|raw| super::reconcile::decode_last_applied(raw)),
            }
        })
        .collect())
}

/// Records ownership + last-applied for `name` — mirrors
/// `netroute::stamp`/`network_access_rule::stamp`, on the Service's OWN
/// registry entry (unlike `NetworkAccessRule`, a `Service` does not target a
/// container it does not own — its record is entirely its own, so stamping it
/// directly carries none of that Kind's risk).
pub(crate) fn stamp(name: &str, stack: &str, fields: &BTreeMap<String, String>) -> Result<()> {
    let namespace = doc_namespace_of(name)?;
    delonix_net::infra::service_set_metadata(
        &namespace,
        name,
        &[
            (
                super::reconcile::STACK_LABEL.to_string(),
                Some(stack.to_string()),
            ),
            (
                super::reconcile::MANAGED_BY.to_string(),
                Some("delonix".to_string()),
            ),
        ],
        &[(
            super::reconcile::LAST_APPLIED.to_string(),
            Some(super::reconcile::encode_last_applied(fields)),
        )],
    )
}

/// `--prune`/`stack destroy`'s teardown.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    let namespace = doc_namespace_of(name)?;
    delonix_net::infra::service_remove(&namespace, name)
}

/// Finds which namespace an already-registered `Service` NAMED `name` lives
/// in. The reconciler's `stamp`/teardown paths receive only a plan NAME, not
/// the document — same shape `netroute`'s `split_route_name` exists to
/// resolve, here by a registry scan instead of a name split, because a
/// `Service`'s identity is `metadata.name` (not a derived key like a route's
/// pair) but its FILE is keyed by `(namespace, name)`.
fn doc_namespace_of(name: &str) -> Result<String> {
    delonix_net::infra::service_list()
        .into_iter()
        .find(|d| d.name == name)
        .map(|d| d.namespace)
        .ok_or_else(|| Error::NotFound(format!("no such service: {name}")))
}

/// For `stack ls`/`describe`: declared vs. how many live backends it resolves
/// to right now. Reads the SAME container store the DNS index itself reads,
/// so this never disagrees with what a client asking for the name actually
/// gets.
pub(crate) fn presence_of(doc: &ManifestDoc) -> (String, String) {
    let Ok(spec) = manifest::spec_of::<ServiceSpec>(doc) else {
        return ("?".into(), super::po::t("invalid spec").into());
    };
    let namespace = doc
        .metadata
        .namespace
        .as_deref()
        .unwrap_or("default")
        .to_string();
    if delonix_net::infra::service_get(&namespace, &doc.metadata.name).is_none() {
        return ("no".into(), "-".into());
    }
    let count = match_count(&namespace, &spec.selector.match_labels);
    if count == 0 {
        return (
            "yes".into(),
            super::po::t("selector matches no workloads").into(),
        );
    }
    (
        "yes".into(),
        super::po::tf("{count} backend(s)", &[("count", &count.to_string())]),
    )
}

/// Live count of containers this selector currently matches, in `namespace` —
/// the same match `build_dns_index`'s Service pass computes, done here on the
/// CLI's OWN container store read (not a second implementation of the match:
/// `delonix_net::matches_labels` is the one function, this is just a second
/// CALLER of it, over a store this process already has open).
fn match_count(namespace: &str, match_labels: &BTreeMap<String, String>) -> usize {
    let Ok((_images, store)) = open_stores() else {
        return 0;
    };
    let Ok(containers) = store.list() else {
        return 0;
    };
    containers
        .iter()
        .filter(|c| c.namespace.eq_ignore_ascii_case(namespace))
        .filter(|c| delonix_net::matches_labels(&c.labels, match_labels))
        .count()
}

/// Applies one document — writes the selector+port to the registry
/// (`delonix_net::infra::service_set`), preserving any existing ownership
/// stamp (the reconciler's `stamp` runs separately, after `apply`, same
/// two-step order every other ownable Kind here follows).
fn apply_one(containers: &[delonix_runtime_core::Container], doc: &ManifestDoc) -> Result<()> {
    let spec: ServiceSpec = manifest::spec_of(doc)?;
    let namespace = doc
        .metadata
        .namespace
        .as_deref()
        .unwrap_or("default")
        .to_string();
    delonix_net::infra::service_set(
        &namespace,
        &doc.metadata.name,
        &spec.selector.match_labels,
        spec.port,
    )?;
    let matched = containers
        .iter()
        .filter(|c| c.namespace.eq_ignore_ascii_case(&namespace))
        .filter(|c| delonix_net::matches_labels(&c.labels, &spec.selector.match_labels))
        .count();
    if matched == 0 {
        // Deliberately a warning, not an error (ADR-0032): refusing would
        // make the declarative create-policies-before-workloads order
        // illegal, and `apply` has no rollback to undo a half-applied
        // stack over an ordering problem that resolves itself on the next
        // apply once the workload exists.
        eprintln!(
            "{}",
            super::po::tf(
                "Service/{name}: selector matched no workloads — this service resolves to nothing",
                &[("name", &doc.metadata.name)],
            )
        );
    }
    println!(
        "{}",
        super::po::tf(
            "service/{name}: {namespace}.{count} matching workload(s), port {port}",
            &[
                ("name", &doc.metadata.name),
                ("namespace", &namespace),
                ("count", &matched.to_string()),
                ("port", &spec.port.to_string()),
            ],
        )
    );
    Ok(())
}

/// Applies every `kind: Service` document.
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (_images, store) = open_stores()?;
    let containers = store.list().unwrap_or_default();
    for doc in manifest::of_kind(docs, k::SERVICE) {
        apply_one(&containers, doc)?;
    }
    Ok(())
}

/// `converge_and_stamp`'s live-update path (`stack.rs`) — same rationale as
/// `NetworkAccessRule`/`FirewallPolicy`: `apply_one` already fully overwrites
/// the registry entry, so converging IS applying, and a per-field path would
/// just be a second way to write the same record.
pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    let (_images, store) = open_stores()?;
    let containers = store.list().unwrap_or_default();
    apply_one(&containers, doc)
}

/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: ServiceSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

/// `delonix get services` — every declared `Service`, its live backend count
/// and DNS name.
#[derive(serde::Serialize)]
struct ServiceLsRow {
    name: String,
    namespace: String,
    port: u16,
    backends: usize,
    dns: String,
    stack: Option<String>,
}

pub(crate) fn cmd_ls(format: OutputFormat) -> Result<()> {
    let format = super::config::resolve_output(&super::util::state_root(), format);
    let mut defs = delonix_net::infra::service_list();
    defs.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));

    let rows: Vec<ServiceLsRow> = defs
        .iter()
        .map(|def| ServiceLsRow {
            name: def.name.clone(),
            namespace: def.namespace.clone(),
            port: def.port,
            backends: match_count(&def.namespace, &def.match_labels),
            dns: format!("{}.{}.delonix.internal", def.name, def.namespace),
            stack: def.labels.get(super::reconcile::STACK_LABEL).cloned(),
        })
        .collect();

    if format == OutputFormat::Json {
        return super::output::print_json(&rows);
    }

    let mut t =
        super::output::Table::new(&["NAME", "NAMESPACE", "PORT", "BACKENDS", "DNS", "STACK"]);
    for r in &rows {
        t.row(vec![
            r.name.clone(),
            r.namespace.clone(),
            r.port.to_string(),
            r.backends.to_string(),
            r.dns.clone(),
            r.stack.clone().unwrap_or_else(|| "-".to_string()),
        ]);
    }
    t.drop_uninformative().print();
    Ok(())
}

/// `delonix describe services <name>` — the generic verb's target.
pub(crate) fn cmd_describe(names: &[String]) -> Result<()> {
    for name in names {
        let Some(def) = delonix_net::infra::service_list()
            .into_iter()
            .find(|d| d.name == *name)
        else {
            return Err(Error::NotFound(format!("no such service: {name}")));
        };
        let backends = match_count(&def.namespace, &def.match_labels);
        let mut d = super::output::Describe::new();
        d.field("Name", &def.name);
        d.field("Namespace", &def.namespace);
        d.field(
            "Selector",
            def.match_labels
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(","),
        );
        d.field("Port", def.port.to_string());
        d.field(
            "DNS",
            format!("{}.{}.delonix.internal", def.name, def.namespace),
        );
        d.field("Backends", backends.to_string());
        d.field_opt("Stack", def.labels.get(super::reconcile::STACK_LABEL));
        d.field_opt("Managed by", def.labels.get(super::reconcile::MANAGED_BY));
        d.print();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::reconcile::{plan, Action, Actual};

    fn doc(nome: &str, ns: &str, labels: &[(&str, &str)], port: u16) -> ManifestDoc {
        let sel = labels
            .iter()
            .map(|(k, v)| format!("      {k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        serde_yaml::from_str(&format!(
            "apiVersion: delonix.io/v1\nkind: Service\nmetadata:\n  name: {nome}\n  namespace: {ns}\n\
             spec:\n  selector:\n    matchLabels:\n{sel}\n  port: {port}\n"
        ))
        .unwrap()
    }

    fn actual_de(nome: &str, labels: &[(&str, &str)], port: u16, dono: Option<&str>) -> Actual {
        let mut f = BTreeMap::new();
        f.insert(
            "matchLabels".into(),
            labels
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(","),
        );
        f.insert("port".into(), port.to_string());
        Actual {
            kind: "Service".into(),
            name: nome.into(),
            fields: f,
            owner: dono.map(String::from),
            last_applied: None,
        }
    }

    #[test]
    fn um_servico_declarado_e_desejado_com_a_selector_ordenada() {
        let d = doc("web", "teamA", &[("app", "web")], 8080);
        let des = desired(&d).unwrap();
        assert_eq!(des.kind, "Service");
        assert_eq!(des.fields.get("matchLabels").unwrap(), "app=web");
        assert_eq!(des.fields.get("port").unwrap(), "8080");
        assert!(des.ownable);
        assert!(des.converges);
    }

    /// Um `Service` retirado do manifesto é candidato a remoção — o mesmo
    /// contrato que `NetworkRoute`/`NetworkAccessRule` já garantem, aqui pela
    /// primeira vez para este Kind.
    #[test]
    fn um_servico_tirado_do_manifesto_e_candidato_a_remocao() {
        let p = plan(
            &[],
            &[actual_de("web", &[("app", "web")], 8080, Some("s"))],
            "s",
        );
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].action, Action::Delete);
        assert_eq!(p[0].name, "web");
    }

    /// `ownable: true` é o que faz um `Service` sem dono passar a `Adopt` em
    /// vez de `NoOp` — sem isto nunca ganha carimbo e nunca é podável.
    #[test]
    fn um_servico_ja_existente_e_adoptado_pela_stack_que_o_declara() {
        let p = plan(
            &[desired(&doc("web", "teamA", &[("app", "web")], 8080)).unwrap()],
            &[actual_de("web", &[("app", "web")], 8080, None)],
            "s",
        );
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].action, Action::Adopt, "{p:?}");
    }

    /// Mudar a porta é uma alteração REAL — `converges: true` tem de a fazer
    /// planear `Update`, não `NoOp`.
    #[test]
    fn mudar_a_porta_planeia_um_update() {
        let p = plan(
            &[desired(&doc("web", "teamA", &[("app", "web")], 9090)).unwrap()],
            &[actual_de("web", &[("app", "web")], 8080, Some("s"))],
            "s",
        );
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].action, Action::Update, "{p:?}");
    }
}
