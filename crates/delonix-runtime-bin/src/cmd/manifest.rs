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
    /// "namespace isolation" section in CLAUDE.md.
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
                "dry-run: falha a serializar {} '{}': {e}",
                doc.kind, doc.metadata.name
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
        "Volume" => cmd::volume::spec_with_defaults(doc),
        "Storage" => cmd::storage::spec_with_defaults(doc),
        // Secret is intentionally left as raw (no typed round-trip) — no need to
        // reformat its `stringData` through the renderer.
        "Image" => cmd::image::spec_with_defaults(doc),
        "Dependency" => cmd::dependency::spec_with_defaults(doc),
        "Vm" => cmd::vm::spec_with_defaults(doc),
        "Pod" => cmd::pod::spec_with_defaults(doc),
        "HTTPRoute" => cmd::httproute::spec_with_defaults(doc),
        "Ingress" => cmd::httproute::ingress_spec_with_defaults(doc),
        "Egress" | "FirewallPolicy" => cmd::firewall::spec_with_defaults(doc),
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
            "sem manifesto: passa -f <ficheiro> ou cria um ./delonix-manifest.yaml".into(),
        ))
    }
}

/// The only `apiVersion` recognized today — refuses early (instead of advancing
/// silently) if the manifest comes from a future/incompatible version.
const SUPPORTED_API_VERSION: &str = "delonix.io/v1";

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
        // `KnowDepends` is the name the user asked for; `Dependency` is the canonical one.
        "knowdepends" | "dependency" => "Dependency",
        "stack" => "Stack",
        "pod" => "Pod",
        _ => kind,
    }
}

/// A grouped `kind: Stack` — bundles resources of several Kinds in ONE document
/// (k8s-Service-like: everything for an app in one place). Expanded at load time
/// into the individual docs, which then flow through the normal per-Kind apply,
/// in dependency order. Each child inherits the Stack's namespace unless it sets
/// its own. The Stack doc itself does not survive the load (it becomes its parts).
#[derive(Debug, Deserialize)]
struct StackSpec {
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
#[derive(Debug, Deserialize)]
struct StackItem {
    name: String,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
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
    warn_unknown_fields(doc, STACK_SPEC_FIELDS);
    let spec: StackSpec = spec_of(doc)?;
    let ns = &doc.metadata.namespace;
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
        ("Tunnel", spec.tunnels),
    ];
    let mut out = Vec::new();
    for (kind, items) in groups {
        for it in items {
            out.push(ManifestDoc {
                api_version: SUPPORTED_API_VERSION.to_string(),
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
pub fn load(path: &Path) -> Result<Vec<ManifestDoc>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        Error::Invalid(format!(
            "{} {}: {e}",
            super::po::t("could not read"),
            path.display()
        ))
    })?;
    if text.trim().is_empty() {
        return Err(Error::Invalid(format!(
            "{} está vazio (sem documentos YAML)",
            path.display()
        )));
    }
    let mut docs = Vec::new();
    for de in serde_yaml::Deserializer::from_str(&text) {
        let mut doc = ManifestDoc::deserialize(de).map_err(|e| {
            Error::Invalid(format!("manifesto inválido em {}: {e}", path.display()))
        })?;
        // Canonicalize early: everything else (of_kind, stack::KINDS, describe) speaks
        // only the canonical form, and a `kind: VirtualMachine` becomes a `Vm`.
        let canon = canonical_kind(&doc.kind);
        if canon != doc.kind {
            doc.kind = canon.to_string();
        }
        if doc.api_version != SUPPORTED_API_VERSION {
            return Err(Error::Invalid(format!(
                "{} '{}': apiVersion '{}' desconhecida (só '{SUPPORTED_API_VERSION}' é suportada)",
                doc.kind, doc.metadata.name, doc.api_version
            )));
        }
        // A grouped `kind: Stack` expands into its constituent resource docs
        // (which then flow through the normal per-Kind apply). The Stack doc
        // itself does not survive — it becomes its parts.
        if doc.kind == "Stack" {
            docs.extend(expand_stack(&doc)?);
        } else {
            docs.push(doc);
        }
    }
    if docs.is_empty() {
        return Err(Error::Invalid(format!(
            "{} está vazio (sem documentos YAML)",
            path.display()
        )));
    }
    Ok(docs)
}

/// Filters the documents of a specific `kind` (exact comparison, e.g. `"Container"`).
pub fn of_kind<'a>(docs: &'a [ManifestDoc], kind: &str) -> Vec<&'a ManifestDoc> {
    docs.iter().filter(|d| d.kind == kind).collect()
}

/// Re-deserializes the raw `spec` of a document into its Kind's typed type.
pub fn spec_of<T: for<'de> Deserialize<'de>>(doc: &ManifestDoc) -> Result<T> {
    serde_yaml::from_value(doc.spec.clone()).map_err(|e| {
        Error::Invalid(format!(
            "{} '{}': spec inválido: {e}",
            doc.kind, doc.metadata.name
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
pub fn warn_unknown_fields(doc: &ManifestDoc, known: &[&str]) {
    for key in unknown_fields(doc, known) {
        eprintln!(
            "AVISO: {} '{}': campo desconhecido '{}' no spec — ignorado (verifica a ortografia)",
            doc.kind, doc.metadata.name, key
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

    /// Drift-guard: each file in `examples/` must parse without A single
    /// unknown field. If someone adds a field to the example but forgets the
    /// `*_SPEC_FIELDS` const (or vice versa), this test breaks — it is what keeps
    /// the lists of known fields aligned with the real schema and with the doc.
    #[test]
    fn examples_nao_tem_campos_desconhecidos() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let fields_for = |kind: &str| -> Option<&'static [&'static str]> {
            match kind {
                "Container" => Some(crate::cmd::container::CONTAINER_SPEC_FIELDS),
                "Pod" => Some(crate::cmd::container::POD_SPEC_FIELDS),
                "Vm" => Some(crate::cmd::vm::VM_SPEC_FIELDS),
                "Volume" => Some(crate::cmd::volume::VOLUME_SPEC_FIELDS),
                "Storage" => Some(crate::cmd::storage::STORAGE_SPEC_FIELDS),
                "Network" => Some(crate::cmd::network::NETWORK_SPEC_FIELDS),
                "Image" => Some(crate::cmd::image::IMAGE_SPEC_FIELDS),
                "Secret" => Some(crate::cmd::secret::SECRET_SPEC_FIELDS),
                // `Ingress` is now the k8s-shaped L7 Ingress (→ HTTPRoute); the
                // L4 firewall keeps `Egress`/`FirewallPolicy`.
                "Ingress" => Some(crate::cmd::httproute::INGRESS_SPEC_FIELDS),
                "Egress" | "FirewallPolicy" => Some(crate::cmd::firewall::FW_SPEC_FIELDS),
                "HTTPRoute" => Some(crate::cmd::httproute::HTTP_ROUTE_SPEC_FIELDS),
                "Dependency" => Some(crate::cmd::dependency::DEPENDENCY_SPEC_FIELDS),
                _ => None, // Cluster has its own nested specs; outside this guard.
            }
        };
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
            if !text.contains(SUPPORTED_API_VERSION) {
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
        assert!(text.contains(SUPPORTED_API_VERSION));
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
        assert!(format!("{err}").contains("vazio"));
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
        assert!(format!("{err}").contains("sem manifesto"));
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
}
