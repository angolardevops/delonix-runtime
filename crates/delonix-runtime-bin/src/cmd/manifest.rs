//! `delonix-manifest.yaml` — declarative multi-document manifest, in the
//! Kubernetes style (`apiVersion`/`kind`/`metadata`/`spec`), for the 5 Kinds
//! already covered by a CLI group: `Container`/`Image`/`Vm`/`Volume`/`Network`.
//!
//! **`apply` semantics: "ensure present", not a reconciler.** No
//! continuous diffing/rollout/drift-detection — that is the job of an
//! orchestrator with controllers (out of scope here, deliberately). Each
//! `apply` of a resource checks whether it already exists by name; if so, it skips; if
//! not, it creates it with the same logic as the equivalent `create`/`run`/`pull` command.
//! See `cmd::stack` for the composition of all the Kinds (`stack apply`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use delonix_runtime_core::{Error, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Metadata {
    pub name: String,
    /// Logical ISOLATION namespace (default `default`). Resources of different
    /// namespaces do not reach each other (only a `kind: Dependency` breaks through). See the
    /// "namespace isolation" section in AGENTS.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Free labels to group/select resources (k8s style). Optional —
    /// the runtime is single-tenant, there are no namespaces; this is just organization.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Free annotations (notes, prereqs, references) — never interpreted by the
    /// runtime, only carried through to the `describe`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

/// A manifest document — `spec` stays raw (`serde_yaml::Value`) until the
/// right Kind's group re-deserializes it into its typed type (`ContainerSpec`,
/// `VmSpec`, ...). Avoids this module having to know the 5 spec types.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ManifestDoc {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    #[serde(default)]
    pub spec: serde_yaml::Value,
}

/// Renders the docs as YAML with every default materialized (dry-run,
/// `kubectl apply --dry-run=client -o yaml` style). Each doc's spec is
/// round-tripped through its typed struct so the `#[serde(default)]`s appear;
/// Kinds without a typed renderer fall back to the raw spec (still shown). Stacks
/// are already expanded and Kinds canonicalized by `load`, so the output is
/// exactly what WOULD be applied.
pub fn render_with_defaults(docs: &[ManifestDoc]) -> Result<String> {
    let mut out = String::new();
    for (i, doc) in docs.iter().enumerate() {
        if i > 0 {
            out.push_str("---\n");
        }
        let mut d = doc.clone();
        d.spec = filled_spec(doc)?;
        out.push_str(&serde_yaml::to_string(&d).map_err(|e| {
            Error::Invalid(format!(
                "{}: {e}",
                super::po::tf(
                    "dry-run: failed to serialize {kind} '{name}'",
                    &[("kind", &doc.kind), ("name", &doc.metadata.name)],
                )
            ))
        })?);
    }
    Ok(out)
}

/// Round-trips a doc's spec through its typed struct so `#[serde(default)]`s
/// materialize. Kinds not yet wired fall back to the raw spec.
fn filled_spec(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    use crate::cmd;
    match doc.kind.as_str() {
        "Network" => cmd::network::spec_with_defaults(doc),
        "NetworkRoute" => cmd::netroute::spec_with_defaults(doc),
        "Volume" => cmd::volume::spec_with_defaults(doc),
        // Secret DOES get a round-trip, and its values are redacted on the way
        // (`secret::spec_with_defaults`). It used to be the one Kind skipped
        // here, which made the most sensitive document in the manifest the only
        // one with no `--dry-run` — the one place you most want to check what
        // was read before applying was the one place you could not.
        "Secret" => cmd::secret::spec_with_defaults(doc),
        "Image" => cmd::image::spec_with_defaults(doc),
        "Vm" => cmd::vm::spec_with_defaults(doc),
        "Pod" => cmd::pod::spec_with_defaults(doc),
        "HTTPRoute" => cmd::httproute::spec_with_defaults(doc),
        "Ingress" => cmd::httproute::ingress_spec_with_defaults(doc),
        "FirewallPolicy" => cmd::firewall::spec_with_defaults(doc),
        "Container" if doc.spec.get("containers").is_some() => {
            cmd::container::pod_spec_with_defaults(doc)
        }
        "Container" => cmd::container::spec_with_defaults(doc),
        _ => Ok(doc.spec.clone()),
    }
}

/// explicit `-f <file>`, or `./delonix-manifest.yaml` by default.
pub fn resolve_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let default = PathBuf::from("delonix-manifest.yaml");
    if default.exists() {
        Ok(default)
    } else {
        Err(Error::Invalid(
            super::po::t("no manifest: pass -f <file> or create a ./delonix-manifest.yaml").into(),
        ))
    }
}

/// The single `apiVersion` every Kind used to share, and which **keeps
/// loading**.
///
/// Not a legacy spelling to be renamed away: `docs/cli-stability.md` promises
/// that `apiVersion: delonix.io/v1` only changes with a `v2`, and that a `v2`
/// does not ship without `v1` still being accepted. ADR-0020 chose to keep that
/// promise — clean cut for COMMANDS, a step down for MANIFESTS. A file in git,
/// reviewed in a PR and pointed at by `$schema` in an editor, does not break
/// because the engine reorganised its groups.
const LEGACY_API_VERSION: &str = "delonix.io/v1";

/// «Is this file a delonix manifest at all», for the guards over `examples/`.
///
/// Deliberately the GROUP suffix and not [`LEGACY_API_VERSION`]: a
/// `contains("delonix.io/v1")` also matches `compute.delonix.io/v1alpha1`, but
/// only by substring accident — it stops being true the day a group is spelled
/// differently, and a guard that silently stops matching goes vacuously green.
#[cfg(test)]
const MANIFEST_MARKER: &str = "delonix.io/";

/// Whether this document's `apiVersion` is one this engine serves.
///
/// Two accepted forms, and the newer one is checked **per Kind**: the group is
/// part of the identity, so `apiVersion: storage.delonix.io/v1alpha1` on a
/// `kind: Pod` is a mistake worth catching rather than a spelling to shrug at.
/// That is what Kubernetes does with its own groups, and the reason the version
/// is a column in [`super::kinds`] instead of a shared constant.
///
/// `None` means «not this engine's», and the caller composes the error — it
/// needs the Kind and the name, which this does not have.
fn api_version_accepted(kind: &str, version: &str) -> bool {
    if version == LEGACY_API_VERSION {
        return true;
    }
    super::kinds::all().any(|f| f.kind == kind && f.api_version == version)
}

/// Normalizes the `kind` to its canonical form, accepting common synonyms —
/// the Kind match is by exact string (`of_kind`), so a `VirtualMachine`
/// or `VM` in a manifest has to resolve to the same `Vm` that the rest of the code
/// uses. Returns the canonical form if known, otherwise the `kind` as-is (unknown
/// Kinds are handled downstream, see `cmd::stack::describe`).
pub fn canonical_kind(kind: &str) -> &str {
    // Case-insensitive on purpose: `Vm`/`VM`/`vm`/`VirtualMachine`/`virtualMachine`
    // (any casing) all resolve to the canonical `Vm` — a half-measure
    // (only some casings) would be worse than nothing, leaving a `kind: vm` to be
    // ignored silently by the `stack apply`.
    let lower = kind.to_ascii_lowercase();
    match lower.as_str() {
        "vm" | "virtualmachine" => "Vm",
        // A RENAME, so the old spelling is an alias and not a deprecation: no
        // warning, nothing to migrate. `docs/cli-stability.md` draws that line
        // — a renamed name stays accepted as an alias — and it is the same
        // treatment `restart`→`restartPolicy` already gets. A MERGE is the
        // other case (`Egress`→`FirewallPolicy`), and that one does warn,
        // because the semantics moved.
        "tunnel" | "gateway" => "Gateway",
        // `KnowDepends` is the name the user asked for; `Dependency` is the canonical one.
        "knowdepends" | "dependency" => "Dependency",
        "stack" => "Stack",
        "pod" => "Pod",
        "workload" => "Workload",
        _ => kind,
    }
}

/// A grouped `kind: Stack` — bundles resources of several Kinds in ONE document
/// (k8s-Service-like: everything for an app in one place). Expanded at load time
/// into the individual docs, which then flow through the normal per-Kind apply,
/// in dependency order. Each child inherits the Stack's namespace unless it sets
/// its own. The Stack doc itself does not survive the load (it becomes its parts).
/// The schema this type generates is deliberately SHALLOW, and the limit is
/// worth stating because it is invisible from the outside.
///
/// Each group is a list of [`StackItem`], whose `spec` is a raw `Value` that the
/// child Kind re-deserializes later. One item type serves all fifteen groups, so
/// there is no per-group type to point `#[schemars(with = ...)]` at the way
/// `WorkloadSpec` can — typing the insides would mean fifteen new item types.
///
/// What this DOES buy is the mistake people actually make: a typo in a GROUP
/// name. `contaienrs:` is silently dropped today — `expand_stack` reads the
/// groups it knows and never looks at the rest — so the stack applies, reports
/// success, and is missing every container in it. `additionalProperties: false`
/// over `STACK_SPEC_FIELDS` catches that in the editor.
///
/// What it does NOT buy: the children's own specs are unchecked here. The
/// per-Kind branches key on a document's `kind`, and a Stack's children have no
/// `kind` of their own until `load` gives them one.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct StackSpec {
    #[serde(default)]
    secrets: Vec<StackItem>,
    #[serde(default)]
    networks: Vec<StackItem>,
    #[serde(default)]
    volumes: Vec<StackItem>,
    #[serde(default)]
    storage: Vec<StackItem>,
    #[serde(default, rename = "shareVolumes")]
    share_volumes: Vec<StackItem>,
    #[serde(default)]
    images: Vec<StackItem>,
    #[serde(default)]
    vms: Vec<StackItem>,
    #[serde(default)]
    containers: Vec<StackItem>,
    #[serde(default)]
    pods: Vec<StackItem>,
    #[serde(default)]
    ingress: Vec<StackItem>,
    #[serde(default)]
    egress: Vec<StackItem>,
    #[serde(default, rename = "firewallPolicies")]
    firewall_policies: Vec<StackItem>,
    #[serde(default, rename = "httpRoutes")]
    http_routes: Vec<StackItem>,
    #[serde(default)]
    dependencies: Vec<StackItem>,
    #[serde(default)]
    tunnels: Vec<StackItem>,
}

/// One entry inside a `kind: Stack` group: a name + the resource's own `spec`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct StackItem {
    name: String,
    #[serde(default)]
    namespace: Option<String>,
    /// Left as «anything» in the schema on purpose: this is a different Kind's
    /// spec depending on which group the item sits in, and narrowing it to one
    /// of them would reject the other fourteen.
    #[serde(default)]
    #[schemars(with = "serde_json::Value")]
    spec: serde_yaml::Value,
}

/// Top-level field names accepted in a `kind: Stack` `spec` (unknown-field warning).
pub const STACK_SPEC_FIELDS: &[&str] = &[
    "secrets",
    "networks",
    "volumes",
    "storage",
    "shareVolumes",
    "images",
    "vms",
    "containers",
    "pods",
    "ingress",
    "egress",
    "firewallPolicies",
    "httpRoutes",
    "dependencies",
    "tunnels",
];

/// Expands a `kind: Stack` doc into its constituent resource docs, in dependency
/// order (Secret → Network → Volume → Storage → Image → Vm → Container → firewall
/// → route → Dependency). Each child inherits the Stack's namespace by default.
fn expand_stack(doc: &ManifestDoc) -> Result<Vec<ManifestDoc>> {
    let spec: StackSpec = spec_of(doc)?;
    let ns = &doc.metadata.namespace;
    // ADR-0011 §4: deliberately NOT derived from the stack name. Doing that
    // would make the reconciler find nothing of its own in the new namespace
    // and create a SECOND copy of the whole stack, leaving the running one
    // orphaned and unmanaged — a safety change whose failure mode is
    // duplicating production is not a safety change. So it warns, and the
    // scaffold (`stack init`) writes a namespace for NEW stacks instead.
    if ns.is_none() {
        super::output::warn(super::po::t(
            "this stack declares no namespace, so its resources land in the shared 'default' — everything else in 'default' can reach them. Set metadata.namespace to isolate it",
        ));
    }
    let groups: Vec<(&str, Vec<StackItem>)> = vec![
        ("Secret", spec.secrets),
        ("Network", spec.networks),
        ("Volume", spec.volumes),
        ("Storage", spec.storage),
        ("ShareVolume", spec.share_volumes),
        ("Image", spec.images),
        ("Vm", spec.vms),
        ("Container", spec.containers),
        ("Pod", spec.pods),
        ("Ingress", spec.ingress),
        ("Egress", spec.egress),
        ("FirewallPolicy", spec.firewall_policies),
        ("HTTPRoute", spec.http_routes),
        ("Dependency", spec.dependencies),
        ("Gateway", spec.tunnels),
    ];
    let mut out = Vec::new();
    for (kind, items) in groups {
        for it in items {
            out.push(ManifestDoc {
                // A child of a `kind: Stack` is synthesised, so it carries the
                // CANONICAL version of its own Kind rather than the parent's:
                // each Kind now lives in its own group, and a `--dry-run` that
                // printed them all under one version would be teaching a
                // spelling the loader accepts only for compatibility.
                api_version: super::kinds::all()
                    .find(|f| f.kind == kind)
                    .map(|f| f.api_version)
                    .unwrap_or(LEGACY_API_VERSION)
                    .to_string(),
                kind: kind.to_string(),
                metadata: Metadata {
                    name: it.name,
                    namespace: it.namespace.or_else(|| ns.clone()),
                    labels: Default::default(),
                    annotations: Default::default(),
                },
                spec: it.spec,
            });
        }
    }
    Ok(out)
}

/// Loads ALL the documents (`---`-separated) of a manifest.
/// Whether `metadata.namespace` does anything on this Kind.
///
/// Namespace here is **network isolation** — the `@dlxns_<ns>` accept plus the
/// cross-namespace `ct state new` drop — so it only means something for a workload that
/// holds an address: `Container`, `Pod`, `Vm`. `Workload` lowers to one of those and
/// `Stack` propagates its namespace to the children it expands into, so both carry it.
///
/// `ShareVolume` honors it too, and for the OTHER meaning: there it is a naming scope — two
/// namespaces can hold a share of the same name, each with its own data directory, and
/// `-v <name>` resolves to the one belonging to the workload's namespace
/// (`VolumeStore::resolve_spec_in`). That is the only Kind where namespace scopes a NAME
/// rather than reachability; `Volume`/`Storage`/`Secret` still do not, and `Storage` in
/// particular is deliberately left global because it is the NAS mount itself — node
/// infrastructure, not a tenant object.
///
/// Takes the CANONICAL kind (`canonical_kind` has already run at the call site), so
/// `VirtualMachine`/`VM` never reach here as such.
///
/// The answer is the `namespaced` column of [`crate::cmd::kinds`] — it used to be
/// a `matches!` of its own, one of the several per-Kind lists that had nothing
/// keeping them in agreement.
pub(crate) fn kind_honors_namespace(kind: &str) -> bool {
    crate::cmd::kinds::honors_namespace(kind)
}

/// The accepted `spec` field names of a Kind — the list each group's `apply`
/// used to consult on its own, now in ONE place so every entry point consults
/// the same one.
///
/// BUG THIS CLOSES, measured across all 16 Kinds with an invented field in the
/// spec: `stack validate` warned about 3 of them, `stack plan` about 3, and
/// `stack apply --dry-run` about 6 — while `delonix secret apply -f` on the very
/// same file answered `unknown field 'x' in spec — ignored (check the
/// spelling)`. The guard existed per Kind and was called from each group's
/// `apply`; `stack.rs` never called it once. So the same file was checked or not
/// depending on which command touched it, and the command whose entire job is to
/// answer «is this manifest right?» was the one that answered least.
///
/// `None` for a Kind with no flat list of its own (`Cluster` nests its specs).
pub(crate) fn spec_fields_for(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "Container" => Some(crate::cmd::container::CONTAINER_SPEC_FIELDS),
        "Pod" => Some(crate::cmd::container::POD_SPEC_FIELDS),
        "Vm" => Some(crate::cmd::vm::VM_SPEC_FIELDS),
        "Volume" => Some(crate::cmd::volume::VOLUME_SPEC_FIELDS),
        "Storage" => Some(crate::cmd::storage::STORAGE_SPEC_FIELDS),
        "Network" => Some(crate::cmd::network::NETWORK_SPEC_FIELDS),
        "Image" => Some(crate::cmd::image::IMAGE_SPEC_FIELDS),
        "Secret" => Some(crate::cmd::secret::SECRET_SPEC_FIELDS),
        // `Ingress` is the k8s-shaped L7 Ingress (→ HTTPRoute); the L4 firewall
        // keeps `Egress`/`FirewallPolicy`.
        "Ingress" => Some(crate::cmd::httproute::INGRESS_SPEC_FIELDS),
        "Egress" | "FirewallPolicy" => Some(crate::cmd::firewall::FW_SPEC_FIELDS),
        "HTTPRoute" => Some(crate::cmd::httproute::HTTP_ROUTE_SPEC_FIELDS),
        "Dependency" => Some(crate::cmd::dependency::DEPENDENCY_SPEC_FIELDS),
        "NetworkRoute" => Some(crate::cmd::netroute::NETWORK_ROUTE_SPEC_FIELDS),
        "Gateway" => Some(crate::cmd::tunnel::TUNNEL_SPEC_FIELDS),
        "ShareVolume" => Some(crate::cmd::sharevolume::SHAREVOLUME_SPEC_FIELDS),
        "Workload" => Some(crate::cmd::workload::WORKLOAD_SPEC_FIELDS),
        "Stack" => Some(STACK_SPEC_FIELDS),
        "Cluster" => Some(crate::cmd::cluster::CLUSTER_SPEC_FIELDS),
        _ => None,
    }
}

/// [`spec_fields_for`], but for a document — because one Kind can be written in
/// two schemas.
///
/// A `kind: Container` whose spec has `containers:` IS the Kubernetes Pod
/// schema, still accepted on the old Kind. Checking it against the Container
/// list would flag `containers`, `initContainers` and the rest as unknown — the
/// guard shouting at a manifest the engine handles perfectly. This is the same
/// choice `container::apply` was already making inline before the guard moved
/// here.
pub(crate) fn spec_fields_for_doc(doc: &ManifestDoc) -> Option<&'static [&'static str]> {
    if doc.kind == "Container" && doc.spec.get("containers").is_some() {
        return Some(crate::cmd::container::POD_SPEC_FIELDS);
    }
    spec_fields_for(&doc.kind)
}

/// Warns about every unknown `spec` key of one document — the top-level ones and
/// the ones nested inside a grouped form.
///
/// Both live here for the same reason: a check that only runs where the spec is
/// DESERIALIZED runs only on `apply`, and `validate` — the command whose whole
/// job is to answer «is this right?» — stays silent. Measured before the move:
/// `resources.memoria` was reported by `apply --dry-run` and not by `validate`,
/// on the very same file.
fn check_unknown_fields(doc: &ManifestDoc) {
    if let Some(fields) = spec_fields_for_doc(doc) {
        warn_unknown_fields(doc, fields);
    }
    let nested = match doc.kind.as_str() {
        "Container" => crate::cmd::container::unknown_group_keys(&doc.spec),
        "Vm" => crate::cmd::vm::unknown_group_keys(&doc.spec),
        _ => Vec::new(),
    };
    for key in nested {
        count_unknown_field_warning();
        super::output::warn(&super::po::tf(
            "{kind} '{name}': unknown field '{key}' in spec — ignored (check the spelling)",
            &[
                ("kind", &doc.kind),
                ("name", &doc.metadata.name),
                ("key", &key),
            ],
        ));
    }
}

pub fn load(path: &Path) -> Result<Vec<ManifestDoc>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        Error::Invalid(format!(
            "{} {}: {e}",
            super::po::t("could not read"),
            path.display()
        ))
    })?;
    load_str(&text, &path.display().to_string())
}

/// The same load, from text already in hand.
///
/// `label` is what an error message calls the source — a path for a file, and
/// for `stack rollback` the revision it came from. It exists because a revision
/// is replayed from the record and never from disk (ADR-0019): writing it to a
/// temporary file first would work, and would then resolve every relative path
/// in it against the WRONG directory.
pub fn load_str(text: &str, label: &str) -> Result<Vec<ManifestDoc>> {
    if text.trim().is_empty() {
        return Err(Error::Invalid(super::po::tf(
            "{path} is empty (no YAML documents)",
            &[("path", label)],
        )));
    }
    let mut docs = Vec::new();
    for de in serde_yaml::Deserializer::from_str(text) {
        let mut doc = ManifestDoc::deserialize(de).map_err(|e| {
            Error::Invalid(format!(
                "{}: {e}",
                super::po::tf("invalid manifest in {path}", &[("path", label)],)
            ))
        })?;
        // Canonicalize early: everything else (of_kind, stack::KINDS, describe) speaks
        // only the canonical form, and a `kind: VirtualMachine` becomes a `Vm`.
        let canon = canonical_kind(&doc.kind);
        if canon != doc.kind {
            doc.kind = canon.to_string();
        }
        lower_legacy_kind(&mut doc)?;
        if !api_version_accepted(&doc.kind, &doc.api_version) {
            // Names BOTH accepted forms. Saying only «delonix.io/v1 is
            // supported» would send someone who wrote a slightly-wrong group
            // back to the old spelling instead of to the right one.
            //
            // Two templates and not one with a pre-built «'A' or 'B'» string:
            // the connector belongs to the SENTENCE, and interpolating an
            // English «or» into a translated message leaves «esperava 'A' or
            // 'B'» — measured, not hypothetical. A placeholder carries a value,
            // never a piece of grammar.
            let group = super::kinds::all()
                .find(|f| f.kind == doc.kind)
                .map(|f| f.api_version);
            let msg = match group {
                Some(g) => super::po::tf(
                    "{kind} '{name}': unknown apiVersion '{version}' (expected '{group}' or '{legacy}')",
                    &[
                        ("kind", &doc.kind),
                        ("name", &doc.metadata.name),
                        ("version", &doc.api_version),
                        ("group", g),
                        ("legacy", LEGACY_API_VERSION),
                    ],
                ),
                None => super::po::tf(
                    "{kind} '{name}': unknown apiVersion '{version}' (expected '{legacy}')",
                    &[
                        ("kind", &doc.kind),
                        ("name", &doc.metadata.name),
                        ("version", &doc.api_version),
                        ("legacy", LEGACY_API_VERSION),
                    ],
                ),
            };
            return Err(Error::Invalid(msg));
        }
        // `metadata.namespace` was accepted on EVERY Kind and honored by three
        // (`docs/discovery/46_GAPS_ENCONTRADOS.md` §5). On the rest it parsed and went
        // nowhere — and "accepted and ignored" on an ISOLATION field reads as "isolated"
        // to whoever wrote it, which is the worst way for a boundary to fail.
        //
        // Warned HERE, before the Stack expansion, on purpose: a namespaced Stack
        // propagates its namespace to every child it expands into, so warning afterwards
        // would fire once per child for a field the user never wrote on that child. Only
        // a namespace written on the document itself is worth a word.
        if doc.metadata.namespace.is_some() && !kind_honors_namespace(&doc.kind) {
            super::output::warn(&super::po::tf(
                "{kind} '{name}': metadata.namespace has no effect — only Container, Pod \
                 and Vm are namespaced (it scopes network isolation, not naming)",
                &[("kind", &doc.kind), ("name", &doc.metadata.name)],
            ));
        }
        // A grouped `kind: Stack` expands into its constituent resource docs
        // (which then flow through the normal per-Kind apply). The Stack doc
        // itself does not survive — it becomes its parts.
        // The guard runs on the document AS WRITTEN, here, because two Kinds do
        // not survive this loop: a `Workload` lowers to a synthetic
        // `Container`/`Vm` and a `Stack` becomes its children. Checking at the
        // end (which is where this first landed) meant those two — and
        // `Dependency`, lowered further down — were the only Kinds to LOSE the
        // warning they already had. Measured: `Dependency` and `Workload` warned
        // before the move and went silent after it.
        check_unknown_fields(&doc);
        if doc.kind == "Stack" {
            // A Stack's children are built HERE, so they never passed through the
            // loop's own lowering — a `kind: Stack` with an `egress:` group would
            // produce `kind: Egress` docs that no handler claims any more, and
            // they would be dropped in silence. Lower each child on its way out.
            for mut child in expand_stack(&doc)? {
                lower_legacy_kind(&mut child)?;
                // The child's spec is the user's own text, moved from inside the
                // Stack — a typo there is as invisible as anywhere else.
                check_unknown_fields(&child);
                docs.push(child);
            }
        } else if doc.kind == "Workload" {
            // A `kind: Workload` lowers to a synthetic `kind: Container`/`kind: Vm`
            // doc (ADR-0001), which then flows through the normal per-Kind apply —
            // exactly like a Stack child. The Workload doc does not survive.
            docs.push(crate::cmd::workload::lower_workload(&doc)?);
        } else {
            docs.push(doc);
        }
    }
    if docs.is_empty() {
        return Err(Error::Invalid(super::po::tf(
            "{path} is empty (no YAML documents)",
            &[("path", label)],
        )));
    }
    // `kind: Dependency` lowers to `kind: FirewallPolicy`, LAST and over the whole
    // list — unlike the per-document lowerings above, it has to see every
    // Dependency at once, because several pointing at the same target accumulate
    // into ONE policy (see `dependency::lower_dependencies`). Doing it per
    // document would silently drop every peer but the last.
    let lowered = crate::cmd::dependency::lower_dependencies(&docs)?;
    if !lowered.is_empty() {
        docs.retain(|d| d.kind != "Dependency");
        docs.extend(lowered);
    }
    // The unknown-field guard, for EVERY document and therefore for every
    // command that reads a manifest — `validate`, `plan`, `apply`, and each
    // group's own `apply`, which all arrive here. See `spec_fields_for` for what
    // this replaces and why one place instead of fifteen.
    //
    // After the lowering on purpose: by now `Egress` is already `FirewallPolicy`
    // and the two share one list, so a Kind is checked against the fields it will
    // actually be parsed with rather than the ones it was written as.
    Ok(docs)
}

/// Rewrites the Kinds that folded into another one.
///
/// Kept as ONE function called from both places a document can enter the list
/// (the top-level loop and a Stack's expanded children), because a lowering that
/// only covers one of them turns a merged Kind into a document nobody claims —
/// silently, which is the failure this whole exercise exists to remove.
fn lower_legacy_kind(doc: &mut ManifestDoc) -> Result<()> {
    // `kind: Egress` folds into `kind: FirewallPolicy`. The two were the SAME
    // object: one struct (`firewall::FwDocSpec`), one validator, one apply, one
    // dataplane — the only difference being where the direction came from. Three
    // nouns for "network policy" means three places to look during an incident,
    // and no model anyone knows (k8s NetworkPolicy, AWS security groups, Azure
    // NSGs) splits inbound from outbound into separate TYPES; they split them
    // with a field.
    //
    // Not in `canonical_kind` because this is not a rename: the direction the old
    // Kind implied has to be written into the spec, and `canonical_kind` is a
    // pure name map with no spec to write into.
    if doc.kind == "Egress" {
        lower_egress(doc)?;
    }
    // `kind: Storage` folds into `kind: Volume`. Both landed in the SAME
    // `VolumeStore` and described the same mount two ways — a `Volume` with
    // `driver: nfs`/`device: nas:/export` IS a `Storage` with
    // `type: nfs`/`server: nas`/`share: /export`, and nothing said which to use.
    // `volumes ls` listed both (one store) while `storage ls` listed only some,
    // so the same question got different answers depending on the command.
    if doc.kind == "Storage" {
        lower_storage(doc)?;
    }
    // `kind: ShareVolume` folds into `kind: Volume` with a `share:` block. A
    // share was ALREADY a volume — `VolumeStore::register_external` wrote one —
    // and the separate Kind only added a second record beside it whose single
    // unique field was the parent's name. Two records for one object is how the
    // two start disagreeing, and it is why a share could never be owned by a
    // stack: ownership is stamped in `labels`, which only the volume has.
    if doc.kind == "ShareVolume" {
        lower_sharevolume(doc)?;
    }
    // `kind: Container` with `spec.containers[]` — the k8s Pod grammar applied to
    // a single container. It gets a WARNING and **no rewrite**, unlike the two
    // above, and the reason is worth writing down because the obvious move here
    // is wrong.
    //
    // The plan for this merge was to rewrite it into `kind: Pod`, on the reading
    // that a one-element Pod and a pod-shaped Container are the same object.
    // They are not, and the code says so:
    //
    //   * `pod_to_run_opts` builds ONE container named `<metadata.name>`, with
    //     no shared namespace;
    //   * `pod::create_pod` creates a shared netns `pod-<name>` and names its
    //     members `<name>-<member>` (`c0` when unnamed).
    //
    // So the rewrite would rename the container — `web` becomes `web-c0` — and
    // every reference to the old name would break: the internal DNS record, an
    // `HTTPRoute` backend, a `Dependency`'s `from`/`to`, `stack validate`'s
    // cross-references. Renaming somebody's running container as a side effect
    // of a tidy-up is not a merge, it is an outage.
    //
    // What is left is the honest half: say it is the deprecated spelling, name
    // the difference concretely, and change nothing.
    if doc.kind == "Container" && doc.spec.get("containers").is_some() {
        super::output::warn(&super::po::tf(
            "Container '{name}': `spec.containers[]` on a `kind: Container` is the deprecated \
             spelling — it still runs ONE container, named '{name}'. For containers that share \
             a namespace use `kind: Pod` (its members are named '{name}-<member>', which is why \
             this is not rewritten for you)",
            &[("name", &doc.metadata.name)],
        ));
    }
    Ok(())
}

/// Rewrites a legacy `kind: Storage` into a `kind: Volume` carrying the matching
/// network-share block.
///
/// A pure spec rewrite: no vault, no credentials file, nothing on disk. Those
/// belong to the apply — a `--dry-run` that wrote a credentials file would make
/// planning a side effect.
fn lower_storage(doc: &mut ManifestDoc) -> Result<()> {
    let ty = doc
        .spec
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Error::Invalid(super::po::tf(
                "Storage '{name}': spec.type is required (nfs|cifs|smb|webdav)",
                &[("name", &doc.metadata.name)],
            ))
        })?
        .to_string();
    // `smb` is an alias of `cifs` and always was (`build_mount` maps them to the
    // same driver); the BLOCK has one name so the two do not become two.
    let block = match ty.as_str() {
        "nfs" => "nfs",
        "cifs" | "smb" => "cifs",
        "webdav" => "webdav",
        other => {
            return Err(Error::Invalid(super::po::tf(
                "Storage '{name}': unknown type '{other}' (nfs|cifs|smb|webdav)",
                &[("name", &doc.metadata.name), ("other", other)],
            )))
        }
    };
    // Warned at the STORAGE level, before the rewrite: a typo named against the
    // Kind the user actually wrote is worth more than the same typo reported
    // against a `spec.nfs` block they never typed.
    warn_unknown_fields(doc, crate::cmd::storage::STORAGE_SPEC_FIELDS);
    let serde_yaml::Value::Mapping(m) = &doc.spec else {
        return Err(Error::Invalid(super::po::tf(
            "Storage '{name}': spec must be a mapping",
            &[("name", &doc.metadata.name)],
        )));
    };
    let mut inner = serde_yaml::Mapping::new();
    for (k, v) in m {
        if k.as_str() == Some("type") {
            continue;
        }
        inner.insert(k.clone(), v.clone());
    }
    let mut outer = serde_yaml::Mapping::new();
    outer.insert(
        serde_yaml::Value::from(block),
        serde_yaml::Value::Mapping(inner),
    );
    doc.spec = serde_yaml::Value::Mapping(outer);
    doc.kind = "Volume".to_string();
    super::output::warn(&super::po::tf(
        "Storage '{name}': `kind: Storage` is deprecated — use `kind: Volume` with a \
         `{block}:` block (same fields, same behaviour)",
        &[("name", &doc.metadata.name), ("block", block)],
    ));
    Ok(())
}

/// Rewrites a legacy `kind: ShareVolume` into `kind: Volume` with a `share:` block.
///
/// `quota`/`alertPct` move UNCHANGED to the volume's own fields — they mean the
/// same thing there and always did; only `storageRef` moves, into `share.from`.
/// Which is the whole point: one field was all the separate Kind ever added.
fn lower_sharevolume(doc: &mut ManifestDoc) -> Result<()> {
    // Warned against the Kind the user actually wrote, before the rewrite —
    // the same order `lower_storage` uses, and for the same reason.
    warn_unknown_fields(doc, crate::cmd::sharevolume::SHAREVOLUME_SPEC_FIELDS);
    let serde_yaml::Value::Mapping(m) = &doc.spec else {
        return Err(Error::Invalid(super::po::tf(
            "ShareVolume '{name}': spec must be a mapping",
            &[("name", &doc.metadata.name)],
        )));
    };
    let from = m
        .get(serde_yaml::Value::from("storageRef"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Error::Invalid(super::po::tf(
                "ShareVolume '{name}': spec.storageRef is required",
                &[("name", &doc.metadata.name)],
            ))
        })?
        .to_string();
    let mut out = serde_yaml::Mapping::new();
    for (k, v) in m {
        if k.as_str() == Some("storageRef") {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    let mut share = serde_yaml::Mapping::new();
    share.insert(
        serde_yaml::Value::from("from"),
        serde_yaml::Value::from(from),
    );
    out.insert(
        serde_yaml::Value::from("share"),
        serde_yaml::Value::Mapping(share),
    );
    doc.spec = serde_yaml::Value::Mapping(out);
    doc.kind = "Volume".to_string();
    super::output::warn(&super::po::tf(
        "ShareVolume '{name}': `kind: ShareVolume` is deprecated — use `kind: Volume` with a \
         `share: {from: ...}` block (same fields, same behaviour, and now ownable by a stack)",
        &[("name", &doc.metadata.name)],
    ));
    Ok(())
}

/// Rewrites a legacy `kind: Egress` into the canonical `kind: FirewallPolicy`,
/// writing the direction the old Kind used to imply into `spec.direction`.
///
/// **Fail-closed on a contradiction.** `kind: Egress` with `direction: ingress`
/// is not a mistake worth guessing at — it is two statements that cannot both be
/// true, and silently honouring one of them on a FIREWALL is the worst possible
/// place to guess. Same treatment `force_microvm_backend` gives a `type: microvm`
/// that asks for a non-microVM backend.
fn lower_egress(doc: &mut ManifestDoc) -> Result<()> {
    let declared = doc.spec.get("direction").and_then(|v| v.as_str());
    match declared {
        Some("egress") | None => {}
        Some(other) => {
            return Err(Error::Invalid(super::po::tf(
                "Egress '{name}': spec.direction is '{other}', but `kind: Egress` means \
                 outbound — write `kind: FirewallPolicy` with the direction you want",
                &[("name", &doc.metadata.name), ("other", other)],
            )))
        }
    }
    if declared.is_none() {
        if let serde_yaml::Value::Mapping(m) = &mut doc.spec {
            m.insert(
                serde_yaml::Value::from("direction"),
                serde_yaml::Value::from("egress"),
            );
        } else {
            // A non-mapping spec has nowhere to put the direction, and letting it
            // through would reach `apply` as "direction missing" — an error about
            // a field the user never wrote.
            return Err(Error::Invalid(super::po::tf(
                "Egress '{name}': spec must be a mapping",
                &[("name", &doc.metadata.name)],
            )));
        }
    }
    super::output::warn(&super::po::tf(
        "Egress '{name}': `kind: Egress` is deprecated — use `kind: FirewallPolicy` with \
         `direction: egress` (same fields, same behaviour)",
        &[("name", &doc.metadata.name)],
    ));
    doc.kind = "FirewallPolicy".to_string();
    Ok(())
}

/// Filters the documents of a specific `kind` (exact comparison, e.g. `"Container"`).
pub fn of_kind<'a>(docs: &'a [ManifestDoc], kind: &str) -> Vec<&'a ManifestDoc> {
    docs.iter().filter(|d| d.kind == kind).collect()
}

/// Re-deserializes the raw `spec` of a document into its Kind's typed type.
pub fn spec_of<T: for<'de> Deserialize<'de>>(doc: &ManifestDoc) -> Result<T> {
    serde_yaml::from_value(doc.spec.clone()).map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::tf(
                "{kind} '{name}': invalid spec",
                &[("kind", &doc.kind), ("name", &doc.metadata.name)],
            )
        ))
    })
}

/// Warns (stderr, NOT an error) for each top-level key of the `spec` that is not in
/// `known`. The specs deliberately do not have `deny_unknown_fields` — a
/// `delonix.io/v1` manifest written for a more recent binary may bring fields that
/// this one does not know yet, and in that case we want to ignore them and proceed, not
/// abort. But the common case of an unknown field is a TYPO (`memroy:`),
/// and an IaaS should never apply a default silently when the user
/// clearly meant something else. Hence the clear and actionable warning.
///
/// `known` must contain ALL the accepted names (the canonical one and each `alias`) — there is
/// a test per Kind that ensures the `examples/` do not trigger any warning,
/// stopping the drift between this list and the struct.
/// Like [`warn_unknown_fields`], but for the keys of a NESTED block
/// (`spec.<block>`).
///
/// The top-level warning only ever looks at the spec's own keys, so a typo
/// inside a block — `serverr:` in a `spec.nfs` — was swallowed and simply became
/// "no server". The blocks only appeared when `kind: Storage` folded into
/// `kind: Volume`; before that there was nothing nested to get wrong.
/// `block` may name a NESTED path with dots (`provision.truenas`) — a block can
/// itself hold a block, and the vendor key of `spec.provision` is one.
pub fn warn_unknown_fields_in(doc: &ManifestDoc, block: &str, known: &[&str]) {
    let mut cur = &doc.spec;
    for seg in block.split('.') {
        match cur.get(seg) {
            Some(v) => cur = v,
            None => return,
        }
    }
    let serde_yaml::Value::Mapping(m) = cur else {
        return;
    };
    for (k, _) in m {
        let Some(key) = k.as_str() else { continue };
        if known.contains(&key) {
            continue;
        }
        count_unknown_field_warning();
        super::output::warn(&super::po::tf(
            "{kind} '{name}': unknown field '{key}' in spec.{block} — ignored (check the spelling)",
            &[
                ("kind", &doc.kind),
                ("name", &doc.metadata.name),
                ("key", key),
                ("block", block),
            ],
        ));
    }
}

/// How many unknown-field warnings this process has emitted.
///
/// Exists so a command can REPORT what it warned about instead of ending on a
/// bare `OK`: `stack validate` printed the warning and then declared the
/// manifest fine on the next line, which is the same "says yes about work it did
/// not do" this engine removes everywhere else. `--strict` turns the count into
/// an exit code, for a CI that wants the typo to stop the pipeline.
pub static UNKNOWN_FIELD_WARNINGS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The count so far.
pub fn unknown_field_warnings() -> usize {
    UNKNOWN_FIELD_WARNINGS.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn count_unknown_field_warning() {
    UNKNOWN_FIELD_WARNINGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn warn_unknown_fields(doc: &ManifestDoc, known: &[&str]) {
    for key in unknown_fields(doc, known) {
        count_unknown_field_warning();
        eprintln!(
            "{}",
            super::po::tf(
                "WARNING: {kind} '{name}': unknown field '{key}' in spec — ignored (check the spelling)",
                &[("kind", &doc.kind), ("name", &doc.metadata.name), ("key", &key)],
            )
        );
    }
}

/// Pure core of `warn_unknown_fields`: returns the top-level keys of the `spec` that
/// are not in `known`. Separated so the drift tests (`examples/` should never
/// produce unknown keys) can assert on the result.
pub fn unknown_fields(doc: &ManifestDoc, known: &[&str]) -> Vec<String> {
    let serde_yaml::Value::Mapping(map) = &doc.spec else {
        return Vec::new();
    };
    map.keys()
        .filter_map(|k| k.as_str())
        .filter(|key| !known.contains(key))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    /// Every Kind this restructuring RENAMES keeps its old spelling working.
    ///
    /// The table is the point: each row is a promise made in
    /// `docs/cli-stability.md` — a renamed name stays accepted as an alias —
    /// and a rename landing without its row here is a manifest in somebody's
    /// git that stops loading. Grows one line per rename as the remaining ones
    /// land.
    ///
    /// An alias is SILENT, unlike a deprecation. A rename changes nothing about
    /// what the document means, so there is nothing for the writer to migrate;
    /// a MERGE (`Egress`→`FirewallPolicy`) does warn, because the semantics
    /// moved. Warning on a pure rename would train people to ignore warnings.
    #[test]
    fn a_renamed_kind_keeps_answering_to_its_old_name() {
        for (old, new) in [("Tunnel", "Gateway"), ("VirtualMachine", "Vm")] {
            assert_eq!(canonical_kind(old), new, "{old} stopped resolving");
            assert_eq!(canonical_kind(new), new, "{new} does not resolve to itself");
            // Casing is not part of the promise being kept, but it is part of
            // the one `canonical_kind` already made — a half-measure would let
            // a `kind: tunnel` be ignored in silence.
            assert_eq!(canonical_kind(&old.to_ascii_lowercase()), new);
            assert_eq!(canonical_kind(&old.to_ascii_uppercase()), new);
        }
    }

    /// The promise `docs/cli-stability.md` makes and ADR-0020 chose to keep:
    /// `apiVersion: delonix.io/v1` only changes with a `v2`, and a `v2` does not
    /// ship without `v1` still being accepted. Clean cut for COMMANDS, a step
    /// down for MANIFESTS — a file in git does not break because the engine
    /// reorganised its groups.
    #[test]
    fn the_legacy_api_version_keeps_loading_for_every_kind() {
        for f in super::super::kinds::all() {
            assert!(
                api_version_accepted(f.kind, LEGACY_API_VERSION),
                "{}: delonix.io/v1 stopped loading",
                f.kind
            );
        }
    }

    /// And the new group is accepted too — otherwise the column would be a
    /// value nothing reads, which is the decoration this repo keeps deleting.
    #[test]
    fn each_kind_accepts_its_own_group() {
        for f in super::super::kinds::all() {
            assert!(
                api_version_accepted(f.kind, f.api_version),
                "{}: does not accept its own {}",
                f.kind,
                f.api_version
            );
        }
    }

    /// The group is part of the identity: a `kind: Pod` under
    /// `storage.delonix.io/…` is a mistake worth catching, not a spelling to
    /// shrug at. Without this, the per-Kind check could be relaxed to «any
    /// known version» and nothing would notice.
    #[test]
    fn a_kind_does_not_accept_another_kinds_group() {
        let volume = super::super::kinds::all()
            .find(|f| f.kind == "Volume")
            .unwrap();
        let pod = super::super::kinds::all()
            .find(|f| f.kind == "Pod")
            .unwrap();
        assert_ne!(
            volume.api_version, pod.api_version,
            "pick two Kinds that differ"
        );
        assert!(!api_version_accepted("Volume", pod.api_version));
        assert!(!api_version_accepted("Pod", volume.api_version));
    }

    #[test]
    fn an_invented_group_is_refused() {
        assert!(!api_version_accepted("Volume", "acme.io/v9"));
        assert!(!api_version_accepted("Volume", ""));
        // A Kind this engine does not serve gets the legacy form and nothing
        // else — there is no group of its own to name.
        assert!(api_version_accepted("NoSuchKind", LEGACY_API_VERSION));
        assert!(!api_version_accepted(
            "NoSuchKind",
            "compute.delonix.io/v1alpha1"
        ));
    }

    /// Every group is `<name>.delonix.io/v1alpha1` — one shape, so a reader can
    /// predict the next one. The legacy form is the deliberate exception.
    #[test]
    fn every_group_follows_one_shape() {
        for f in super::super::kinds::all() {
            let v = f.api_version;
            assert!(
                v.ends_with(".delonix.io/v1alpha1") && !v.starts_with('.'),
                "{}: {v:?} is not <group>.delonix.io/v1alpha1",
                f.kind
            );
        }
    }

    use super::*;

    #[test]
    fn parse_multi_doc_com_kinds_diferentes() {
        let text = "\
apiVersion: delonix.io/v1
kind: Network
metadata: { name: appnet }
spec: { driver: bridge }
---
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: pgdata }
spec: { driver: local }
---
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: \"alpine:3.19\" }
";
        let p =
            std::env::temp_dir().join(format!("delonix-manifest-test-{}.yaml", std::process::id()));
        std::fs::write(&p, text).unwrap();
        let docs = load(&p).unwrap();
        assert_eq!(docs.len(), 3);
        assert_eq!(docs[0].kind, "Network");
        assert_eq!(docs[0].metadata.name, "appnet");
        assert_eq!(docs[2].kind, "Container");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn workload_lowers_to_underlying_kind_through_load() {
        // End-to-end through the real load() path: a Workload does not survive —
        // it becomes its underlying Kind, inheriting name/namespace. (ADR-0001)
        let text = "\
apiVersion: delonix.io/v1
kind: Workload
metadata: { name: web, namespace: prod }
spec:
  type: container
  container: { image: \"nginx:alpine\" }
---
apiVersion: delonix.io/v1
kind: Workload
metadata: { name: db }
spec:
  type: vm
  vm: { disk: \"golden.qcow2\" }
";
        let p =
            std::env::temp_dir().join(format!("delonix-workload-test-{}.yaml", std::process::id()));
        std::fs::write(&p, text).unwrap();
        let docs = load(&p).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].kind, "Container");
        assert_eq!(docs[0].metadata.name, "web");
        assert_eq!(docs[0].metadata.namespace.as_deref(), Some("prod"));
        assert_eq!(docs[1].kind, "Vm");
        assert_eq!(docs[1].metadata.name, "db");
        // No `Workload` doc survives the load.
        assert!(of_kind(&docs, "Workload").is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn of_kind_filtra_correctamente() {
        let text = "\
apiVersion: delonix.io/v1
kind: Network
metadata: { name: a }
spec: {}
---
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: b }
spec: {}
";
        let p = std::env::temp_dir().join(format!(
            "delonix-manifest-test2-{}.yaml",
            std::process::id()
        ));
        std::fs::write(&p, text).unwrap();
        let docs = load(&p).unwrap();
        assert_eq!(of_kind(&docs, "Network").len(), 1);
        assert_eq!(of_kind(&docs, "Volume").len(), 1);
        assert_eq!(of_kind(&docs, "Vm").len(), 0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn kind_virtualmachine_canonicaliza_para_vm() {
        let text = "\
apiVersion: delonix.io/v1
kind: VirtualMachine
metadata: { name: node1 }
spec: { disk: k8s-golden }
---
apiVersion: delonix.io/v1
kind: VM
metadata: { name: node2 }
spec: { disk: k8s-golden }
";
        let p = std::env::temp_dir().join(format!(
            "delonix-manifest-vm-alias-{}.yaml",
            std::process::id()
        ));
        std::fs::write(&p, text).unwrap();
        let docs = load(&p).unwrap();
        // Both synonyms become the canonical `Vm`, caught by `of_kind`.
        assert_eq!(of_kind(&docs, "Vm").len(), 2);
        assert_eq!(docs[0].kind, "Vm");
        assert_eq!(docs[1].kind, "Vm");
        let _ = std::fs::remove_file(&p);
    }

    /// Every Kind survives a round-trip through its typed spec without losing or changing a
    /// field — proved against the manifests the project SHIPS, not against fixtures written
    /// to pass.
    ///
    /// `filled_spec` is what `--dry-run` prints: the spec re-serialized through the typed
    /// struct, so every `#[serde(default)]` materializes. If a Kind's `Serialize` drops a
    /// field its `Deserialize` accepts (or renames one on the way out), feeding the output
    /// back in produces something DIFFERENT the second time — and the user is looking at a
    /// dry-run that does not describe what will be applied. Asserting `once == twice`
    /// catches exactly that asymmetry.
    ///
    /// Driven by `examples/`, so a new example is covered the day it lands and nobody has to
    /// remember to add a case here. `nas-vm-cloud-config.yaml` is skipped by CONTENT (it is
    /// cloud-init, not a delonix manifest) rather than by name, and the test fails if the
    /// directory ever stops holding manifests — a green run over zero files would be the
    /// most comfortable kind of lie.
    ///
    /// **What it catches, and what it does not — both measured, not assumed.** Renaming the
    /// serialization of `NetworkSpec::subnet` (which `examples/network.yaml` sets to
    /// `10.89.0.0/24`) makes this test FAIL, as it should. Renaming `driver` does NOT, and
    /// the reason is the blind spot: the example sets `driver: bridge`, which is the
    /// DEFAULT, so the second pass loses the key, falls back to the same value and the
    /// asymmetry cancels itself out.
    ///
    /// So the guarantee is: no Kind loses a field that some published example sets to a
    /// NON-default value. A field that every example leaves at its default is not covered
    /// here, and the way to cover it is to set it in an example — which is worth doing for
    /// its own sake, since an example that only shows defaults teaches nothing either.
    #[test]
    fn os_exemplos_publicados_fazem_round_trip_sem_perder_campos() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut files = 0usize;
        let mut docs_seen = 0usize;
        let mut kinds = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(&dir).expect("examples/ tem de existir") {
            let path = entry.expect("entrada legivel").path();
            if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("exemplo legivel");
            if !text.contains("apiVersion: delonix.io/") {
                continue; // cloud-init e afins: nao sao manifestos deste motor
            }
            files += 1;
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            // Que o exemplo publicado sequer CARREGUE ja e uma afirmacao que vale a pena.
            let loaded = load(&path).unwrap_or_else(|e| panic!("{name}: nao carrega: {e}"));
            for doc in &loaded {
                docs_seen += 1;
                kinds.insert(doc.kind.clone());
                let once = filled_spec(doc)
                    .unwrap_or_else(|e| panic!("{name}: {} nao serializa: {e}", doc.kind));
                let mut again = doc.clone();
                again.spec = once.clone();
                let twice = filled_spec(&again)
                    .unwrap_or_else(|e| panic!("{name}: {} nao re-parseia: {e}", doc.kind));
                assert_eq!(
                    once, twice,
                    "{name}: o round-trip de {} mudou o spec — o --dry-run nao descreve o que sera aplicado",
                    doc.kind
                );
            }
        }
        assert!(
            files >= 20,
            "so {files} exemplos: o filtro esta a comer manifestos"
        );
        assert!(
            docs_seen >= files,
            "menos documentos que ficheiros ({docs_seen} < {files})"
        );
        // Os Kinds com renderizador tipado tem MESMO de aparecer, senao o teste passa por
        // nao ter exercitado nada deles.
        for k in ["Container", "Network", "Volume", "Vm", "Pod"] {
            assert!(
                kinds.contains(k),
                "nenhum exemplo cobre o kind {k}: {kinds:?}"
            );
        }
    }

    /// `metadata.namespace` is honored exactly where the engine applies it, and the alias
    /// forms must not fall through the crack: `kind: VirtualMachine` is canonicalized
    /// BEFORE the check, so it has to end up on the honored side. Asserting the two
    /// functions together is the point — checking `kind_honors_namespace("Vm")` alone
    /// would still pass the day an alias stopped being canonicalized.
    #[test]
    fn a_namespace_e_honrada_exactamente_onde_o_motor_a_aplica() {
        for kind in ["Container", "Pod", "Vm", "Workload", "Stack", "ShareVolume"] {
            assert!(kind_honors_namespace(kind), "{kind} tem de honrar");
        }
        for alias in ["VirtualMachine", "VM", "vm", "pod", "workload"] {
            assert!(
                kind_honors_namespace(canonical_kind(alias)),
                "o alias {alias} tem de chegar canonicalizado ao lado honrado"
            );
        }
        // `Volume` honra-a desde a fusao do `ShareVolume`: um volume com bloco
        // `share:` e escopado pelo namespace (e o directorio dos dados que muda),
        // um volume simples nao. E o unico Kind cuja resposta vem do DOCUMENTO,
        // e por isso `honors_namespace` diz que sim — o aviso do `load` seria
        // errado num share, e um aviso errado e pior que nenhum.
        assert!(kind_honors_namespace("Volume"));
        assert!(
            kind_honors_namespace("ShareVolume"),
            "a grafia antiga tambem"
        );
        // Sem semantica de namespace hoje: aceitam o campo e nao fazem nada com ele,
        // que e precisamente o que passa a ser avisado no `load`.
        for kind in [
            "Network",
            "Storage",
            "Secret",
            "Image",
            "HTTPRoute",
            "Ingress",
            "Egress",
            "FirewallPolicy",
            "Dependency",
            "Cluster",
        ] {
            assert!(!kind_honors_namespace(kind), "{kind} nao honra hoje");
        }
    }

    #[test]
    fn canonical_kind_e_case_insensitive_para_vm() {
        // Any plausible casing from another tool resolves to `Vm`.
        for k in [
            "Vm",
            "VM",
            "vm",
            "VirtualMachine",
            "virtualMachine",
            "VIRTUALMACHINE",
        ] {
            assert_eq!(
                canonical_kind(k),
                "Vm",
                "kind {k:?} devia canonicalizar para Vm"
            );
        }
        // Non-Vm Kinds pass through intact (we don't invent synonyms).
        assert_eq!(canonical_kind("Container"), "Container");
        assert_eq!(canonical_kind("Storage"), "Storage");
    }

    #[test]
    fn metadata_labels_annotations_opcionais() {
        let text = "\
apiVersion: delonix.io/v1
kind: Container
metadata:
  name: web
  labels: { tier: frontend }
  annotations: { note: exemplo }
spec: { image: alpine }
---
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: sem-labels }
spec: {}
";
        let p =
            std::env::temp_dir().join(format!("delonix-manifest-meta-{}.yaml", std::process::id()));
        std::fs::write(&p, text).unwrap();
        let docs = load(&p).unwrap();
        assert_eq!(
            docs[0].metadata.labels.get("tier").map(String::as_str),
            Some("frontend")
        );
        assert_eq!(
            docs[0].metadata.annotations.get("note").map(String::as_str),
            Some("exemplo")
        );
        // Without a labels/annotations block → empty maps, never an error.
        assert!(docs[1].metadata.labels.is_empty());
        assert!(docs[1].metadata.annotations.is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unknown_fields_apanha_gralha_e_ignora_conhecidos() {
        let text = "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: alpine, memroy: 2G, restartPolicy: always }
";
        let p = std::env::temp_dir().join(format!(
            "delonix-manifest-unknown-{}.yaml",
            std::process::id()
        ));
        std::fs::write(&p, text).unwrap();
        let docs = load(&p).unwrap();
        let unknown = unknown_fields(&docs[0], crate::cmd::container::CONTAINER_SPEC_FIELDS);
        // `memroy` (typo) is flagged; `image`/`restartPolicy` (canonical) are not.
        assert_eq!(unknown, vec!["memroy".to_string()]);
        let _ = std::fs::remove_file(&p);
    }

    /// O `validate` dizia `OK` na linha a seguir a avisar que um campo tinha
    /// sido ignorado. O contador é o que permite ao veredicto condizer com o que
    /// foi impresso — e ao `--strict` transformá-lo em exit code.
    #[test]
    fn campo_ignorado_e_contado() {
        let dir = std::env::temp_dir().join(format!("dlx-warncount-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("m.yaml");
        std::fs::write(
            &p,
            "apiVersion: delonix.io/v1\nkind: Network\nmetadata:\n  name: n\nspec:\n  campoInexistente: 1\n",
        )
        .unwrap();
        let antes = super::unknown_field_warnings();
        super::load(&p).unwrap();
        assert_eq!(
            super::unknown_field_warnings(),
            antes + 1,
            "carregar um manifesto com um campo inventado tem de contar UM aviso"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Drift-guard: each file in `examples/` must parse without A single
    /// unknown field. If someone adds a field to the example but forgets the
    /// `*_SPEC_FIELDS` const (or vice versa), this test breaks — it is what keeps
    /// the lists of known fields aligned with the real schema and with the doc.
    #[test]
    fn examples_nao_tem_campos_desconhecidos() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let fields_for = |kind: &str| -> Option<&'static [&'static str]> { spec_fields_for(kind) };
        for entry in std::fs::read_dir(&dir).expect("examples/ existe") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Distinguish "not a delonix manifest" (cloud-config, without
            // apiVersion — skip) from "it is a manifest and it is BROKEN" (has the
            // marker but the load fails — the test MUST fail, otherwise a
            // malformed example passes unnoticed). Without this distinction, the
            // guard would stay vacuously green for a broken example.
            if !text.contains(MANIFEST_MARKER) {
                continue;
            }
            let docs = load(&path).unwrap_or_else(|e| {
                panic!(
                    "{}: é um manifesto delonix mas não parseia: {e}",
                    path.display()
                )
            });
            for doc in &docs {
                // A Pod-shaped `kind: Container` (has `spec.containers`) uses a
                // different top-level field set than the flat one.
                let known = if doc.kind == "Container" && doc.spec.get("containers").is_some() {
                    Some(crate::cmd::container::POD_SPEC_FIELDS)
                } else {
                    fields_for(&doc.kind)
                };
                let Some(known) = known else {
                    continue;
                };
                let unknown = unknown_fields(doc, known);
                assert!(
                    unknown.is_empty(),
                    "{}: {} '{}' tem campos desconhecidos {:?} — actualiza a const *_SPEC_FIELDS",
                    path.display(),
                    doc.kind,
                    doc.metadata.name,
                    unknown
                );
            }
        }
    }

    #[test]
    fn manifesto_marcado_mas_partido_falha_load_nao_e_saltado() {
        // Drift-guard contract (Fix #1): a file that HAS the `delonix.io/v1`
        // marker but is broken (here, `metadata.name` is missing) must
        // give Err on load — that is what distinguishes a malformed example (the guard
        // FAILS) from a cloud-config without a marker (the guard skips).
        let text = "\
apiVersion: delonix.io/v1
kind: Container
metadata: {}
spec: { image: alpine }
";
        assert!(text.contains(MANIFEST_MARKER));
        let p = std::env::temp_dir().join(format!(
            "delonix-manifest-partido-{}.yaml",
            std::process::id()
        ));
        std::fs::write(&p, text).unwrap();
        assert!(
            load(&p).is_err(),
            "manifesto marcado mas sem metadata.name devia falhar o load"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn ficheiro_vazio_e_erro_claro() {
        let p = std::env::temp_dir().join(format!(
            "delonix-manifest-empty-{}.yaml",
            std::process::id()
        ));
        std::fs::write(&p, "").unwrap();
        let err = load(&p).unwrap_err();
        assert!(format!("{err}").contains("is empty"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn resolve_path_sem_flag_nem_ficheiro_e_erro_claro() {
        let dir =
            std::env::temp_dir().join(format!("delonix-manifest-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let err = resolve_path(None).unwrap_err();
        assert!(format!("{err}").contains("no manifest"));
        std::env::set_current_dir(orig).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_render_fills_container_defaults() {
        let yaml = "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: nginx }
";
        let dir = std::env::temp_dir();
        let p = dir.join(format!("delonix-dryrun-{}.yaml", std::process::id()));
        std::fs::write(&p, yaml).unwrap();
        let docs = load(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        let out = render_with_defaults(&docs).unwrap();
        // The user only wrote `image: nginx`; the defaults must materialize.
        assert!(out.contains("image: nginx"));
        assert!(out.contains("detach: true"), "veio:\n{out}"); // default_true
        assert!(out.contains("network: host"), "veio:\n{out}"); // default_net
        assert!(out.contains("restartPolicy: no"), "veio:\n{out}"); // renamed default
    }

    #[test]
    fn stack_expands_into_child_docs_in_order() {
        let yaml = "\
apiVersion: delonix.io/v1
kind: Stack
metadata:
  name: myapp
  namespace: prod
spec:
  networks:
    - name: web-net
      spec: { driver: bridge }
  containers:
    - name: web
      spec: { image: nginx }
    - name: db
      namespace: data
      spec: { image: postgres }
";
        let dir = std::env::temp_dir();
        let p = dir.join(format!("delonix-stack-{}.yaml", std::process::id()));
        std::fs::write(&p, yaml).unwrap();
        let docs = load(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        // The Stack itself is gone; children present, in dependency order.
        assert!(!docs.iter().any(|d| d.kind == "Stack"));
        assert_eq!(docs.len(), 3);
        assert_eq!(docs[0].kind, "Network");
        assert_eq!(docs[0].metadata.name, "web-net");
        assert_eq!(docs[0].metadata.namespace.as_deref(), Some("prod")); // inherited
        assert_eq!(docs[1].kind, "Container");
        assert_eq!(docs[1].metadata.name, "web");
        assert_eq!(docs[2].kind, "Container");
        assert_eq!(docs[2].metadata.name, "db");
        assert_eq!(docs[2].metadata.namespace.as_deref(), Some("data")); // per-item override
    }

    /// `kind: Egress` e `kind: FirewallPolicy` eram o MESMO objecto — uma
    /// struct, um validador, um apply, um dataplane. Depois da fusão só há um
    /// Kind no fim, e o antigo é reescrito com a direcção que implicava.
    #[test]
    fn egress_e_reescrito_como_firewallpolicy_com_a_direccao() {
        let mut doc: ManifestDoc = serde_yaml::from_str(
            "apiVersion: delonix.io/v1\nkind: Egress\nmetadata: { name: db-out }\nspec: { target: db }\n",
        )
        .unwrap();
        lower_legacy_kind(&mut doc).unwrap();
        assert_eq!(doc.kind, "FirewallPolicy");
        assert_eq!(
            doc.spec.get("direction").unwrap().as_str(),
            Some("egress"),
            "a direcção que o Kind antigo implicava tem de ficar escrita no spec"
        );
        // Um `direction: egress` já escrito não é duplicado nem alterado.
        let mut ja: ManifestDoc = serde_yaml::from_str(
            "apiVersion: delonix.io/v1\nkind: Egress\nmetadata: { name: x }\nspec: { target: db, direction: egress }\n",
        )
        .unwrap();
        lower_legacy_kind(&mut ja).unwrap();
        assert_eq!(ja.spec.get("direction").unwrap().as_str(), Some("egress"));
    }

    /// Uma contradicção não se adivinha, e muito menos numa firewall: `kind:
    /// Egress` com `direction: ingress` são duas afirmações que não podem ser
    /// ambas verdadeiras. Fail-closed, como o `force_microvm_backend` faz a um
    /// `type: microvm` que pede outro backend.
    #[test]
    fn egress_com_direccao_contraditoria_e_recusado() {
        let mut doc: ManifestDoc = serde_yaml::from_str(
            "apiVersion: delonix.io/v1\nkind: Egress\nmetadata: { name: x }\nspec: { target: db, direction: ingress }\n",
        )
        .unwrap();
        let e = lower_legacy_kind(&mut doc).unwrap_err().to_string();
        assert!(e.contains("ingress"), "{e}");
        assert!(
            e.contains("FirewallPolicy"),
            "a mensagem tem de dizer o que fazer: {e}"
        );
    }

    /// Os filhos de um `kind: Stack` são construídos DENTRO do `load` e nunca
    /// passaram pelo ciclo, por isso um grupo `egress:` produziria documentos
    /// que nenhum handler reclama — e seriam largados em silêncio, que é
    /// exactamente a falha que esta fusão existe para tirar.
    #[test]
    fn um_grupo_egress_dentro_de_um_stack_tambem_e_reescrito() {
        let text = "\
apiVersion: delonix.io/v1
kind: Stack
metadata: { name: s }
spec:
  egress:
    - name: dentro
      spec: { target: db }
";
        let p = std::env::temp_dir().join(format!("dlx-stack-egress-{}.yaml", std::process::id()));
        std::fs::write(&p, text).unwrap();
        let docs = load(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(docs.len(), 1);
        assert_eq!(
            docs[0].kind, "FirewallPolicy",
            "o filho do Stack ficou por reescrever"
        );
        assert_eq!(
            docs[0].spec.get("direction").unwrap().as_str(),
            Some("egress")
        );
    }

    /// `kind: Volume` e `kind: Storage` descreviam a MESMA montagem de duas
    /// maneiras e acabavam no MESMO store, sem nada a dizer qual usar. O tipo
    /// passa a ser o NOME do bloco — a forma do `kind: Workload` — por isso um
    /// tipo não pode contradizer a sua própria declaração.
    #[test]
    fn storage_e_reescrito_como_volume_com_o_bloco_do_tipo() {
        let mut doc: ManifestDoc = serde_yaml::from_str(
            "apiVersion: delonix.io/v1\nkind: Storage\nmetadata: { name: media }\nspec: { type: nfs, server: 10.0.0.5, share: /pool/media, mountOptions: 'vers=4.1' }\n",
        )
        .unwrap();
        lower_legacy_kind(&mut doc).unwrap();
        assert_eq!(doc.kind, "Volume");
        let b = doc.spec.get("nfs").expect("bloco nfs em falta");
        assert_eq!(b.get("server").unwrap().as_str(), Some("10.0.0.5"));
        assert_eq!(b.get("mountOptions").unwrap().as_str(), Some("vers=4.1"));
        // O `type` não sobrevive: passou a ser o nome do bloco.
        assert!(doc.spec.get("type").is_none());
        assert!(b.get("type").is_none());
    }

    /// Um `kind: ShareVolume` era um `kind: Volume` com um registo a mais ao
    /// lado, cujo único campo próprio era o pai. Passa a ser o bloco `share:`,
    /// e o que a reescrita tem de garantir é que **só** o `storageRef` se move:
    /// `quota` e `alertPct` já significavam num volume exactamente o que
    /// significavam aqui, e movê-los para dentro do bloco criaria um segundo
    /// sítio para os escrever.
    #[test]
    fn sharevolume_e_reescrito_como_volume_com_bloco_share() {
        let mut doc: ManifestDoc = serde_yaml::from_str(
            "apiVersion: delonix.io/v1\nkind: ShareVolume\nmetadata: { name: db, namespace: teamA }\nspec: { storageRef: nas, quota: 5G, alertPct: 80 }\n",
        )
        .unwrap();
        lower_legacy_kind(&mut doc).unwrap();
        assert_eq!(doc.kind, "Volume");
        assert_eq!(
            doc.spec.get("share").unwrap().get("from").unwrap().as_str(),
            Some("nas")
        );
        assert_eq!(doc.spec.get("quota").unwrap().as_str(), Some("5G"));
        assert_eq!(doc.spec.get("alertPct").unwrap().as_u64(), Some(80));
        // O `storageRef` não sobrevive à solta: passou a ser `share.from`, e
        // deixá-lo nos dois sítios é como os dois passam a discordar.
        assert!(doc.spec.get("storageRef").is_none());
        // A namespace segue com o documento — é ela que decide o directório dos
        // dados, e um share que a perdesse na reescrita mudaria de sítio.
        assert_eq!(doc.metadata.namespace.as_deref(), Some("teamA"));
    }

    /// Sem `storageRef` não há share nenhum — o campo é o que o Kind existia
    /// para carregar. Recusar nomeia o campo; deixar passar produziria um
    /// `kind: Volume` com um bloco `share:` sem `from`, e o erro apareceria
    /// mais à frente contra um documento que o utilizador nunca escreveu.
    #[test]
    fn um_sharevolume_sem_storage_ref_e_recusado() {
        let mut doc: ManifestDoc = serde_yaml::from_str(
            "apiVersion: delonix.io/v1\nkind: ShareVolume\nmetadata: { name: db }\nspec: { quota: 5G }\n",
        )
        .unwrap();
        let e = lower_legacy_kind(&mut doc).unwrap_err().to_string();
        assert!(e.contains("storageRef"), "{e}");
    }

    /// `smb` sempre foi um alias de `cifs` (o `build_mount` manda os dois para o
    /// mesmo driver). O bloco tem UM nome, por isso os dois não voltam a ser dois.
    #[test]
    fn smb_e_cifs_caem_no_mesmo_bloco() {
        for ty in ["cifs", "smb"] {
            let mut doc: ManifestDoc = serde_yaml::from_str(&format!(
                "apiVersion: delonix.io/v1\nkind: Storage\nmetadata: {{ name: b }}\nspec: {{ type: {ty}, server: nas, share: media }}\n"
            ))
            .unwrap();
            lower_legacy_kind(&mut doc).unwrap();
            assert!(
                doc.spec.get("cifs").is_some(),
                "{ty} não caiu no bloco cifs"
            );
        }
    }

    #[test]
    fn storage_sem_tipo_ou_com_tipo_desconhecido_e_recusado() {
        for spec in [
            "{ server: nas, share: x }",
            "{ type: gluster, server: nas, share: x }",
        ] {
            let mut doc: ManifestDoc = serde_yaml::from_str(&format!(
                "apiVersion: delonix.io/v1\nkind: Storage\nmetadata: {{ name: b }}\nspec: {spec}\n"
            ))
            .unwrap();
            assert!(lower_legacy_kind(&mut doc).is_err(), "aceitou: {spec}");
        }
    }
}
