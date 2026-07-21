//! `delonix stack` — applies ALL the Kinds of a manifest at once
//! (`Network`/`Volume`/`Image`/`Vm`/`Container`), in the right order by
//! name dependency (networks/volumes/images before whoever references them).
//!
//! **Fail-fast, no transactionality**: stops at the first error; whatever was
//! already applied before the error STAYS applied (there is no rollback) — same
//! "ensure present" semantics documented in `cmd::manifest`.

use std::path::PathBuf;

use clap::Subcommand;
use delonix_runtime_core::Result;

use super::manifest;

#[derive(Subcommand)]
pub enum StackCmd {
    /// Initializes a COMPLETE project: Delonixfile + manifest + cluster + README — files ALREADY FILLED IN (images
    /// included), ready to use without editing anything.
    Init {
        /// Project directory (default: the current one).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Project name (default: the directory name).
        #[arg(long)]
        name: Option<String>,
        /// Image to use. Omit = fills in with the default image.
        #[arg(long)]
        image: Option<String>,
        /// Overwrites already existing files.
        #[arg(long)]
        force: bool,
        /// Generates a complete PROJECT for a stack (e.g. `python`) with best practices,
        /// instead of the generic scaffold. `--template list` shows the available ones.
        #[arg(long, short = 't')]
        template: Option<String>,
        /// After generating, builds the image, starts it and waits for it to become healthy.
        #[arg(long)]
        up: bool,
    },
    /// Applies all the manifest Kinds (Network → Volume → Image → Vm → Container).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Stack detail in `kubectl describe` style: each resource DECLARED in the
    /// manifest and whether or not it is present on the machine.
    ///
    /// **The stack has no state of its own** — there is no registry of "stacks", only
    /// a manifest and the resources it creates. That is why this `describe` always
    /// starts from the file and goes to confirm each resource against the respective
    /// store, instead of inventing a new registry that would drift out of sync (the
    /// same reason `cluster ls` derives its state from the container labels).
    ///
    /// The column that matters is PRESENCE: an `apply` is fail-fast and without
    /// rollback, so a half-applied stack is a normal state and this is exactly
    /// what it shows.
    /// List the structure the manifest composes (containers, volumes,
    /// networks, ...) and whether each resource exists — the tabular summary
    /// of `describe`.
    Ls {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    Describe {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Validates the manifest WITHOUT touching anything (dry-run): resolves the
    /// cross-references (`Container.network`/`.volumes`, `Vm.network`, `Ingress/Egress.
    /// target`) against what the manifest declares PLUS what already exists in the stores.
    /// Exits with an error if any reference is left unresolved — it is the safety
    /// net against an `apply` that would only fail halfway through (fail-fast, no rollback).
    Validate {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

pub fn run(action: StackCmd) -> Result<()> {
    if let StackCmd::Init {
        dir,
        name,
        image,
        force,
        template,
        up,
    } = action
    {
        return cmd_init(
            super::scaffold::Target::Stack,
            dir,
            name,
            image,
            force,
            template,
            up,
        );
    }
    match action {
        // Handled at the top of `run` (it does a `return`).
        StackCmd::Init { .. } => unreachable!("tratado acima"),
        StackCmd::Apply { file } => apply(file),
        StackCmd::Ls { file } => ls(file),
        StackCmd::Describe { file } => describe(file),
        StackCmd::Validate { file } => validate(file),
    }
}

/// The stack Kinds, in the SAME order as `apply` — whoever reads `describe` sees
/// the order in which things are created, which is half the diagnosis when an
/// apply stops halfway.
const KINDS: [&str; 12] = [
    "Secret",
    "Network",
    "Volume",
    "Storage",
    "Image",
    "Vm",
    "Container",
    "Ingress",
    "Egress",
    "FirewallPolicy",
    "HTTPRoute",
    "Dependency",
];

/// `stack ls` — the structure the manifest composes, in a single TABLE
/// (kind→name→presence→status), reusing exactly the resolution of
/// `describe` (`presence` queries the real stores; the stack has no registry
/// of its own, by design — see CLAUDE.md).
fn ls(file: Option<PathBuf>) -> Result<()> {
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;
    let (_, cstore) = super::util::open_stores()?;
    let containers = cstore.list().unwrap_or_default();
    let mut t = super::output::Table::new(&["KIND", "NAME", "PRESENT", "STATUS"]);
    for kind in KINDS {
        for doc in manifest::of_kind(&docs, kind) {
            let name = &doc.metadata.name;
            let (present, status) = presence(kind, name, &containers);
            t.row(vec![kind.to_string(), name.clone(), present, status]);
        }
    }
    t.print();
    Ok(())
}

fn describe(file: Option<PathBuf>) -> Result<()> {
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;

    let mut d = super::output::Describe::new();
    d.field("Manifest", path.display().to_string());
    d.field("Documents", docs.len().to_string());
    d.print();

    // Kinds the manifest brings but the stack does not know how to apply: better
    // to say so than to ignore silently (the `apply` would also ignore them, without warning).
    let desconhecidos: Vec<&str> = docs
        .iter()
        .map(|doc| doc.kind.as_str())
        .filter(|k| !KINDS.contains(k))
        .collect();
    if !desconhecidos.is_empty() {
        println!();
        println!(
            "AVISO: kinds não suportados pelo stack (ignorados pelo `apply`): {}",
            desconhecidos.join(", ")
        );
    }

    let (_, cstore) = super::util::open_stores()?;
    let containers = cstore.list().unwrap_or_default();

    for kind in KINDS {
        let of = manifest::of_kind(&docs, kind);
        if of.is_empty() {
            continue;
        }
        println!();
        let mut t = super::output::Table::new(&["KIND", "NAME", "PRESENT", "STATUS", "LABELS"]);
        for doc in of {
            let name = &doc.metadata.name;
            let (present, status) = presence(kind, name, &containers);
            t.row(vec![
                kind.to_string(),
                name.clone(),
                present,
                status,
                fmt_labels(&doc.metadata),
            ]);
        }
        t.print();
    }

    print_missing_conditions(&docs);
    Ok(())
}

/// Prints the MISSING honesty conditions (privilege/host prerequisites that would
/// make a resource be created but not work as it appears to: network mount in
/// rootless, hard quota without root, network driver without a physical plane,
/// restart on a Cloud Hypervisor VM). Only the missing ones — it is the actionable
/// surface of "what is missing for this to really work". Shared by `describe`
/// AND by the end of `apply`: whoever runs `apply` (the real creation flow)
/// MUST see this right then, not only if they happen to run `describe` afterwards.
fn print_missing_conditions(docs: &[manifest::ManifestDoc]) {
    let env = super::conditions::Env::probe();
    let mut header = false;
    for doc in docs {
        for c in super::conditions::conditions_for(doc, &env) {
            if !c.ok {
                if !header {
                    eprintln!();
                    eprintln!(
                        "{}",
                        super::po::t("Conditions (attention — missing prerequisites):")
                    );
                    header = true;
                }
                eprintln!(
                    "  {} '{}': {}=False ({}) — {}",
                    doc.kind, doc.metadata.name, c.kind, c.reason, c.message
                );
            }
        }
    }
}

/// `key=value` of the `metadata` labels (plus a `+N anno` if there are annotations),
/// or `-` if there are none — the organizational column of `describe`.
fn fmt_labels(meta: &manifest::Metadata) -> String {
    let mut parts: Vec<String> = meta
        .labels
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    if !meta.annotations.is_empty() {
        parts.push(format!("+{} anno", meta.annotations.len()));
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(",")
    }
}

/// `(present, state)` of a declared resource. **Not a reconciler**: it only
/// answers "is there something with this name?", never compares the declared spec
/// with the real one (drift-detection is an orchestrator's job, deliberately out of
/// scope for this runtime — see `cmd::manifest`).
fn presence(
    kind: &str,
    name: &str,
    containers: &[delonix_runtime_core::Container],
) -> (String, String) {
    let root = super::util::state_root();
    match kind {
        "Container" => match containers.iter().find(|c| c.name == name) {
            Some(c) => {
                let mut c = c.clone();
                delonix_runtime::reconcile_status(&mut c);
                ("yes".into(), c.status.to_string())
            }
            None => ("no".into(), "-".into()),
        },
        // Storage is a network volume — it lives in the same store as the volumes.
        "Volume" | "Storage" => {
            match delonix_volume::VolumeStore::open(&root).and_then(|s| s.list()) {
                Ok(vs) => yes_no(vs.iter().any(|v| v.name == name)),
                Err(e) => ("?".into(), e.to_string()),
            }
        }
        "Network" => match delonix_net::NetworkStore::open(&root).and_then(|s| s.list()) {
            Ok(ns) => yes_no(ns.iter().any(|n| n.name == name)),
            Err(e) => ("?".into(), e.to_string()),
        },
        "Image" => match delonix_image::ImageStore::open(&root) {
            Ok(s) => yes_no(s.resolve(name).is_ok()),
            Err(e) => ("?".into(), e.to_string()),
        },
        "Secret" => match delonix_runtime_core::SecretStore::open(&root) {
            Ok(s) => yes_no(s.list().iter().any(|sec| sec.name == name)),
            Err(e) => ("?".into(), e.to_string()),
        },
        // `status` (and not the raw record) so the state comes reconciled with the
        // backend — a VM that died externally shows as Stopped, not Running.
        "Vm" => match delonix_vm::status(&root, name) {
            Ok(vm) => ("yes".into(), vm.status.to_string()),
            Err(_) => ("no".into(), "-".into()),
        },
        // Ingress/Egress have no store of their own — they are firewall directives
        // applied to a target container, not resources with state. The `apply`
        // always applies them (idempotent); here we only note the nature.
        "Ingress" | "Egress" | "FirewallPolicy" => ("-".into(), "declarative".into()),
        "HTTPRoute" => ("-".into(), "declarative".into()),
        "Dependency" => ("-".into(), "declarative".into()),
        _ => ("?".into(), "kind não suportado".into()),
    }
}

fn yes_no(b: bool) -> (String, String) {
    if b {
        ("yes".into(), "present".into())
    } else {
        ("no".into(), "-".into())
    }
}

fn apply(file: Option<PathBuf>) -> Result<()> {
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;
    // Validate the graph BEFORE touching anything: the `apply` is fail-fast without
    // rollback, so a broken reference (an `Ingress` pointing to a container that
    // nobody declares) must stop everything BEFORE the first creation, not halfway
    // with half the stack already in the kernel.
    let issues = validate_graph(&docs);
    if !issues.is_empty() {
        for i in &issues {
            eprintln!("  ✗ {i}");
        }
        return Err(delonix_runtime_core::Error::Invalid(format!(
            "stack apply abortado: {} referência(s) por resolver (corrige o manifesto ou usa `stack validate`)",
            issues.len()
        )));
    }
    // Secrets first: `Storage.passwordSecret` and `Container.secret` reference them.
    // `base` = the manifest folder, so `fromEnvFile` resolves next to it.
    let base = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    super::secret::apply(&docs, base)?;
    super::network::apply(&docs)?;
    super::volume::apply(&docs)?;
    super::storage::apply(&docs)?;
    super::image::apply(&docs)?;
    super::vm::apply(&docs)?;
    super::container::apply(&docs)?;
    super::firewall::apply(&docs)?;
    // Dependency (directed reachability) — after the firewall and the containers
    // (it needs the IPs); compiles to default-deny ingress + allows on the `to`.
    super::dependency::apply(&docs)?;
    // HTTPRoute LAST: it needs the backend containers already created (with IP) to
    // resolve the routes; brings up/reloads the L7 reverse-proxy.
    super::httproute::apply(&docs)?;
    // After creating everything, say what was created but will NOT work as it
    // appears without a host prerequisite (network mount in rootless, etc.) —
    // it is here, in the real creation flow, that the user needs to know it.
    print_missing_conditions(&docs);
    Ok(())
}

/// `stack validate` — dry-run: only runs `validate_graph` and reports, without applying.
fn validate(file: Option<PathBuf>) -> Result<()> {
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;
    let issues = validate_graph(&docs);
    if issues.is_empty() {
        println!(
            "stack validate: OK — {} documento(s), todas as referências resolvidas",
            docs.len()
        );
        Ok(())
    } else {
        for i in &issues {
            println!("  ✗ {i}");
        }
        Err(delonix_runtime_core::Error::Invalid(format!(
            "{} referência(s) por resolver",
            issues.len()
        )))
    }
}

/// Built-in network names (not references to a `kind: Network`): containers
/// have `host`/`none`; VMs use `bridge` as the ingress default.
fn is_builtin_net(net: &str, is_vm: bool) -> bool {
    matches!(net, "" | "host" | "none") || (is_vm && net == "bridge")
}

/// Extracts the named VOLUME names from a `spec.volumes` (`["data:/x", ...]`).
/// Bind mounts (`/host:/x`) and empty entries are not references to resources.
fn volume_refs(doc: &manifest::ManifestDoc) -> Vec<String> {
    let Some(seq) = doc.spec.get("volumes").and_then(|v| v.as_sequence()) else {
        return Vec::new();
    };
    seq.iter()
        .filter_map(|v| v.as_str())
        .filter_map(|s| {
            let name = s.split(':').next().unwrap_or("");
            if name.is_empty() || name.starts_with('/') {
                None // bind mount or junk — not a named volume
            } else {
                Some(name.to_string())
            }
        })
        .collect()
}

/// Resolves all the manifest cross-references against what it DECLARES plus
/// what already EXISTS in the stores (read, best-effort). Returns the list of
/// problems (empty = intact graph). **Touches nothing** — it is the base shared
/// by `stack validate` (dry-run) and by the `apply` gate.
fn validate_graph(docs: &[manifest::ManifestDoc]) -> Vec<String> {
    let root = super::util::state_root();

    // Resources already present on the machine count as resolved (a manifest may
    // reference a network created in a previous apply). Best-effort: if a store does
    // not open, we proceed with only what the manifest declares.
    let existing_networks: Vec<String> = delonix_net::NetworkStore::open(&root)
        .and_then(|s| s.list())
        .map(|ns| ns.into_iter().map(|n| n.name).collect())
        .unwrap_or_default();
    let existing_volumes: Vec<String> = delonix_volume::VolumeStore::open(&root)
        .and_then(|s| s.list())
        .map(|vs| vs.into_iter().map(|v| v.name).collect())
        .unwrap_or_default();
    let existing_containers: Vec<String> = super::util::open_stores()
        .and_then(|(_, cstore)| cstore.list())
        .map(|cs| cs.into_iter().map(|c| c.name).collect())
        .unwrap_or_default();
    let existing_secrets: Vec<String> = delonix_runtime_core::SecretStore::open(&root)
        .map(|s| s.list().into_iter().map(|sec| sec.name).collect())
        .unwrap_or_default();

    validate_graph_with(
        docs,
        &existing_networks,
        &existing_volumes,
        &existing_containers,
        &existing_secrets,
    )
}

/// PURE core of `validate_graph`: receives what already exists on the machine as
/// explicit lists (instead of reading the stores), so the tests are
/// deterministic and do not depend on the real state of the dev machine.
fn validate_graph_with(
    docs: &[manifest::ManifestDoc],
    existing_networks: &[String],
    existing_volumes: &[String],
    existing_containers: &[String],
    existing_secrets: &[String],
) -> Vec<String> {
    use std::collections::HashSet;

    let declared = |kinds: &[&str]| -> HashSet<String> {
        docs.iter()
            .filter(|d| kinds.contains(&d.kind.as_str()))
            .map(|d| d.metadata.name.clone())
            .collect()
    };
    let mut networks = declared(&["Network"]);
    let mut volumes = declared(&["Volume", "Storage"]);
    let mut containers = declared(&["Container"]);
    let mut secrets = declared(&["Secret"]);
    networks.extend(existing_networks.iter().cloned());
    volumes.extend(existing_volumes.iter().cloned());
    containers.extend(existing_containers.iter().cloned());
    secrets.extend(existing_secrets.iter().cloned());

    // Known keys of each Secret DECLARED inline (stringData). `None` = the
    // keys are not knowable at validation time (it uses `fromEnvFile`, whose file
    // is not read here) — in that case no key presence is validated
    // (never a false positive). Only for `Storage.passwordSecret`, which reads the
    // specific `password` key.
    let mut declared_secret_keys: std::collections::HashMap<String, Option<HashSet<String>>> =
        std::collections::HashMap::new();
    for doc in docs.iter().filter(|d| d.kind == "Secret") {
        let has_env_file = doc.spec.get("fromEnvFile").is_some_and(|v| !v.is_null());
        let keys = if has_env_file {
            None
        } else {
            doc.spec
                .get("stringData")
                .and_then(|v| v.as_mapping())
                .map(|m| {
                    m.keys()
                        .filter_map(|k| k.as_str())
                        .map(str::to_string)
                        .collect()
                })
        };
        declared_secret_keys.insert(doc.metadata.name.clone(), keys);
    }

    let mut issues = Vec::new();

    // Duplicates within the manifest (same Kind + name) — the `apply` would create one
    // and skip the other; better to warn than to blindly apply one of the two.
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for doc in docs {
        let key = (doc.kind.clone(), doc.metadata.name.clone());
        if !seen.insert(key) {
            issues.push(format!(
                "{} '{}' declarado mais do que uma vez",
                doc.kind, doc.metadata.name
            ));
        }
    }

    for doc in docs {
        let name = &doc.metadata.name;
        match doc.kind.as_str() {
            "Container" | "Vm" => {
                let is_vm = doc.kind == "Vm";
                if let Some(net) = doc.spec.get("network").and_then(|v| v.as_str()) {
                    if !is_builtin_net(net, is_vm) && !networks.contains(net) {
                        issues.push(format!(
                            "{} '{name}' → network '{net}' não é declarada nem existe",
                            doc.kind
                        ));
                    }
                }
                for vref in volume_refs(doc) {
                    if !volumes.contains(&vref) {
                        issues.push(format!("{} '{name}' → volume '{vref}' não é declarado (Volume/Storage) nem existe", doc.kind));
                    }
                }
                // `Vm.volumes` is a list of OBJECTS `{name, mountPath}` (not the
                // docker string-syntax of the Container) — resolve `name` of each one.
                if is_vm {
                    if let Some(seq) = doc.spec.get("volumes").and_then(|v| v.as_sequence()) {
                        for vname in seq
                            .iter()
                            .filter_map(|it| it.get("name"))
                            .filter_map(|v| v.as_str())
                        {
                            if !volumes.contains(vname) {
                                issues.push(format!("Vm '{name}' → volume '{vname}' não é declarado (Volume/Storage) nem existe"));
                            }
                        }
                    }
                }
                // `Container.secret: [names]` — each one must be a Secret.
                if let Some(seq) = doc.spec.get("secret").and_then(|v| v.as_sequence()) {
                    for sref in seq.iter().filter_map(|v| v.as_str()) {
                        if !secrets.contains(sref) {
                            issues.push(format!("{} '{name}' → secret '{sref}' não é um Secret declarado nem existente", doc.kind));
                        }
                    }
                }
            }
            "Storage" => {
                // `Storage.passwordSecret` references a Secret (the mount reads the
                // `password` key of that Secret — `storage::resolve_password`).
                if let Some(sref) = doc.spec.get("passwordSecret").and_then(|v| v.as_str()) {
                    if !secrets.contains(sref) {
                        issues.push(format!("Storage '{name}' → passwordSecret '{sref}' não é um Secret declarado nem existente"));
                    } else if let Some(Some(keys)) = declared_secret_keys.get(sref) {
                        // Only when we know the keys (inline Secret without fromEnvFile):
                        // then we can assert with certainty that `password` is missing.
                        if !keys.contains("password") {
                            issues.push(format!(
                                "Storage '{name}' → passwordSecret '{sref}': o Secret não declara a chave 'password' (o mount lê exactamente essa chave)"
                            ));
                        }
                    }
                }
            }
            "Ingress" | "Egress" | "FirewallPolicy" => {
                let scope = doc
                    .spec
                    .get("scope")
                    .and_then(|v| v.as_str())
                    .unwrap_or("container");
                // FirewallPolicy requires `direction` ∈ {ingress, egress} — catch it
                // HERE (before the apply creates anything) instead of only at apply.
                if doc.kind == "FirewallPolicy" {
                    let dir = doc.spec.get("direction").and_then(|v| v.as_str());
                    if !matches!(dir, Some("ingress" | "egress")) {
                        issues.push(format!("FirewallPolicy '{name}' → direction obrigatório e ∈ {{ingress, egress}}"));
                    } else if dir == Some("ingress") && scope == "network" {
                        // Same incompatibility the apply rejects — catch it beforehand.
                        issues.push(format!("FirewallPolicy '{name}' → scope: network só é suportado com direction: egress"));
                    }
                }
                if !matches!(scope, "container" | "network") {
                    // Message consistent with the apply (which also rejects the scope).
                    issues.push(format!(
                        "{} '{name}' → scope inválido '{scope}' (usa container|network)",
                        doc.kind
                    ));
                } else if let Some(target) = doc.spec.get("target").and_then(|v| v.as_str()) {
                    // scope: network → the target is a NETWORK; otherwise, a Container.
                    if scope == "network" {
                        if !networks.contains(target) {
                            issues.push(format!("{} '{name}' (scope network) → target '{target}' não é uma Network declarada nem existente", doc.kind));
                        }
                    } else if !containers.contains(target) {
                        issues.push(format!("{} '{name}' → target '{target}' não é um Container declarado nem existente", doc.kind));
                    }
                }
            }
            "HTTPRoute" => {
                // Each backend.service must be a declared/existing Container;
                // the tls.secretRef (if used) a Secret. Reuses the typed parser to
                // avoid duplicating the schema (and catches an invalid spec right away).
                match manifest::spec_of::<super::httproute::HttpRouteSpec>(doc) {
                    Ok(spec) => {
                        if let Err(e) = super::httproute::validate_spec(name, &spec) {
                            issues.push(e.to_string());
                        }
                        for rule in &spec.rules {
                            for pr in &rule.paths {
                                if !containers.contains(&pr.backend.service) {
                                    issues.push(format!(
                                        "HTTPRoute '{name}' → backend '{}' não é um Container declarado nem existente",
                                        pr.backend.service
                                    ));
                                }
                            }
                        }
                        if let Some(tls) = &spec.tls {
                            if tls.mode.as_deref() == Some("secretRef") {
                                if let Some(sref) = &tls.secret_ref {
                                    if !secrets.contains(sref) {
                                        issues.push(format!(
                                            "HTTPRoute '{name}' → tls.secretRef '{sref}' não é um Secret declarado nem existente"
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => issues.push(e.to_string()),
                }
            }
            "Dependency" => {
                // `from` and each `to` must be declared/existing containers.
                let from = doc.spec.get("from").and_then(|v| v.as_str());
                match from {
                    Some(f) if !containers.contains(f) => {
                        issues.push(format!("Dependency '{name}' → from '{f}' não é um Container declarado nem existente"));
                    }
                    None => issues.push(format!("Dependency '{name}' → `from` obrigatório")),
                    _ => {}
                }
                // `to` can be a scalar OR a list.
                let tos: Vec<&str> = match doc.spec.get("to") {
                    Some(v) if v.is_string() => v.as_str().into_iter().collect(),
                    Some(v) => v
                        .as_sequence()
                        .map(|s| s.iter().filter_map(|x| x.as_str()).collect())
                        .unwrap_or_default(),
                    None => Vec::new(),
                };
                if tos.is_empty() {
                    issues.push(format!("Dependency '{name}' → `to` não pode ser vazio"));
                }
                for t in tos {
                    if !containers.contains(t) {
                        issues.push(format!("Dependency '{name}' → to '{t}' não é um Container declarado nem existente"));
                    }
                }
            }
            _ => {}
        }
    }
    issues
}

/// Handles the `init` of this group (see `cmd::scaffold`).
fn cmd_init(
    target: super::scaffold::Target,
    dir: PathBuf,
    name: Option<String>,
    image: Option<String>,
    force: bool,
    template: Option<String>,
    up: bool,
) -> Result<()> {
    let name = name.unwrap_or_else(|| {
        // Without `--name`, use the DIRECTORY name. `canonicalize` cannot be used:
        // the directory does not exist yet (it is `init` that creates it) and it would
        // always fail, falling into the fallback — every project would be named "app".
        // `.`/empty resolve to the cwd; a new path uses its basename.
        let p = if dir.as_os_str().is_empty() || dir == std::path::Path::new(".") {
            std::env::current_dir().ok()
        } else {
            Some(dir.clone())
        };
        p.as_deref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".to_string())
    });
    super::scaffold::init(
        target,
        &super::scaffold::InitOpts {
            dir,
            name,
            image,
            force,
            template,
            up,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses multi-doc YAML to `Vec<ManifestDoc>` via the same real `load`
    /// (so the canonicalization/apiVersion rules hold in the tests).
    fn docs(yaml: &str) -> Vec<manifest::ManifestDoc> {
        // UNIQUE name per call: the tests run in threads of the SAME process,
        // so `process::id()` is not enough to distinguish them — without the counter,
        // two calls collided on the path and one deleted the other's file.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "delonix-stack-test-{}-{n}.yaml",
            std::process::id()
        ));
        std::fs::write(&p, yaml).unwrap();
        let d = manifest::load(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        d
    }

    fn check(yaml: &str) -> Vec<String> {
        // Nothing "existing" on the machine — the test sees only what the manifest declares.
        validate_graph_with(&docs(yaml), &[], &[], &[], &[])
    }

    #[test]
    fn grafo_integro_nao_tem_problemas() {
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Network
metadata: { name: appnet }
spec: { driver: bridge }
---
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: data }
spec: {}
---
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: nginx, network: appnet, volumes: [\"data:/var\", \"/host/x:/y:ro\"] }
---
apiVersion: delonix.io/v1
kind: Ingress
metadata: { name: web-in }
spec: { target: web }
",
        );
        assert!(
            issues.is_empty(),
            "esperava grafo íntegro, veio: {issues:?}"
        );
    }

    #[test]
    fn network_por_declarar_e_sinalizada() {
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: nginx, network: fantasma }
",
        );
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("network 'fantasma'"), "{issues:?}");
    }

    #[test]
    fn builtins_de_rede_nao_sao_referencias() {
        // host/none (container) and bridge (vm) are not a kind: Network.
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: c1 }
spec: { image: nginx, network: host }
---
apiVersion: delonix.io/v1
kind: Vm
metadata: { name: v1 }
spec: { disk: d, network: bridge }
",
        );
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn volume_nomeado_por_declarar_e_sinalizado_mas_bind_mount_nao() {
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: nginx, volumes: [\"semvolume:/x\", \"/host/ok:/y\"] }
",
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("volume 'semvolume'"), "{issues:?}");
    }

    #[test]
    fn firewallpolicy_valida_target_como_ingress_egress() {
        // FirewallPolicy resolves the target the same way (scope-aware) as Ingress/Egress.
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: dbapp }
spec: { image: postgres }
---
apiVersion: delonix.io/v1
kind: FirewallPolicy
metadata: { name: ok }
spec: { direction: ingress, target: dbapp }
---
apiVersion: delonix.io/v1
kind: FirewallPolicy
metadata: { name: bad }
spec: { direction: egress, target: fantasma }
",
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("target 'fantasma'"), "{issues:?}");
    }

    #[test]
    fn firewallpolicy_direction_e_scope_incompativel_apanhados_no_validate() {
        // invalid direction.
        let i = check("apiVersion: delonix.io/v1\nkind: FirewallPolicy\nmetadata: { name: a }\nspec: { direction: sideways, target: x }\n");
        assert!(
            i.iter().any(|s| s.contains("direction obrigatório")),
            "{i:?}"
        );
        // ingress + scope: network is incompatible (egress only) — caught BEFORE the apply.
        let i = check(
            "\
apiVersion: delonix.io/v1
kind: Network
metadata: { name: n }
spec: { driver: bridge }
---
apiVersion: delonix.io/v1
kind: FirewallPolicy
metadata: { name: b }
spec: { direction: ingress, scope: network, target: n }
",
        );
        assert!(
            i.iter()
                .any(|s| s.contains("scope: network só é suportado com direction: egress")),
            "{i:?}"
        );
    }

    #[test]
    fn egress_scope_network_valida_target_contra_redes() {
        // scope: network → the target must be a Network (not a container).
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Network
metadata: { name: prod-net }
spec: { driver: bridge }
---
apiVersion: delonix.io/v1
kind: Egress
metadata: { name: e1 }
spec: { scope: network, target: prod-net, defaultPolicy: deny }
---
apiVersion: delonix.io/v1
kind: Egress
metadata: { name: e2 }
spec: { scope: network, target: rede-fantasma, defaultPolicy: deny }
",
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(
            issues[0].contains("scope network") && issues[0].contains("rede-fantasma"),
            "{issues:?}"
        );
    }

    #[test]
    fn ingress_target_inexistente_e_sinalizado() {
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Egress
metadata: { name: out }
spec: { target: nao-existe }
",
        );
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("target 'nao-existe'"), "{issues:?}");
    }

    #[test]
    fn duplicado_no_manifesto_e_sinalizado() {
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: data }
spec: {}
---
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: data }
spec: {}
",
        );
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("declarado mais do que uma vez"),
            "{issues:?}"
        );
    }

    #[test]
    fn vm_volumes_object_style_valida_a_referencia() {
        // `Vm.volumes` are objects {name, mountPath} — the ref must be resolved.
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Storage
metadata: { name: dados }
spec: { type: nfs, server: h, share: /s }
---
apiVersion: delonix.io/v1
kind: Vm
metadata: { name: v }
spec: { disk: d, volumes: [ { name: dados, mountPath: /mnt/d }, { name: fantasma, mountPath: /mnt/f } ] }
",
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("volume 'fantasma'"), "{issues:?}");
    }

    #[test]
    fn secret_por_declarar_e_sinalizado_em_container_e_storage() {
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Secret
metadata: { name: creds }
spec: { stringData: { password: x } }
---
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: nginx, secret: [creds, fantasma] }
---
apiVersion: delonix.io/v1
kind: Storage
metadata: { name: nas }
spec: { type: nfs, server: h, share: /s, passwordSecret: outro-fantasma }
",
        );
        // `creds` resolves; `fantasma` (container) and `outro-fantasma` (storage) do not.
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(
            issues.iter().any(|i| i.contains("secret 'fantasma'")),
            "{issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|i| i.contains("passwordSecret 'outro-fantasma'")),
            "{issues:?}"
        );
    }

    #[test]
    fn storage_passwordsecret_sem_chave_password_e_sinalizado() {
        // The Secret exists but declares only `token` (inline) — the mount would read `password`.
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Secret
metadata: { name: creds }
spec: { stringData: { token: x } }
---
apiVersion: delonix.io/v1
kind: Storage
metadata: { name: nas }
spec: { type: cifs, server: h, share: /s, passwordSecret: creds }
",
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(
            issues[0].contains("não declara a chave 'password'"),
            "{issues:?}"
        );

        // With the `password` key present → no problems.
        let ok = check(
            "\
apiVersion: delonix.io/v1
kind: Secret
metadata: { name: creds }
spec: { stringData: { password: x } }
---
apiVersion: delonix.io/v1
kind: Storage
metadata: { name: nas }
spec: { type: cifs, server: h, share: /s, passwordSecret: creds }
",
        );
        assert!(ok.is_empty(), "{ok:?}");

        // Secret via fromEnvFile → keys unknown at validation → does NOT
        // risk a false positive (even without knowing whether it has `password`).
        let unknown = check(
            "\
apiVersion: delonix.io/v1
kind: Secret
metadata: { name: creds }
spec: { fromEnvFile: ./x.env }
---
apiVersion: delonix.io/v1
kind: Storage
metadata: { name: nas }
spec: { type: cifs, server: h, share: /s, passwordSecret: creds }
",
        );
        assert!(unknown.is_empty(), "{unknown:?}");
    }

    #[test]
    fn recurso_ja_existente_na_maquina_resolve_a_referencia() {
        let d = docs(
            "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: nginx, network: prod-net }
",
        );
        // prod-net is not in the manifest, but exists on the machine → resolved.
        let issues = validate_graph_with(&d, &["prod-net".to_string()], &[], &[], &[]);
        assert!(issues.is_empty(), "{issues:?}");
    }
}
