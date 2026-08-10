//! `delonix stack` — applies ALL the Kinds of a manifest at once
//! (`Network`/`Volume`/`Image`/`Vm`/`Container`), in the right order by
//! name dependency (networks/volumes/images before whoever references them).
//!
//! **Fail-fast, no transactionality**: stops at the first error; whatever was
//! already applied before the error STAYS applied (there is no rollback) — same
//! "ensure present" semantics documented in `cmd::manifest`.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use delonix_runtime_core::Result;
use serde::Deserialize;

use super::manifest;
use super::reconcile::{self, Action, Change};

/// The stack that owns the resources of a manifest, in order of precedence:
/// `--name`, the `metadata.name` of a `kind: Stack` in the file, then the
/// manifest's parent DIRECTORY name.
///
/// The directory is **canonicalized first**, and that is not a detail. `compose`
/// shipped exactly this bug: `default_project_name` collapsed every project to
/// `"default"` for a relative path, so `compose down -v` in one project could
/// delete another project's volume. Worse, the test that should have caught it
/// encoded the bug — it only ever passed ABSOLUTE paths, while the real
/// invocation is always relative (`-f delonix-manifest.yaml`).
///
/// Falls back to `default` only when there is genuinely no directory name to
/// take (the filesystem root), never as the result of a path shape.
pub(crate) fn stack_name(path: &Path, explicit: Option<&str>) -> String {
    if let Some(n) = explicit.filter(|n| !n.trim().is_empty()) {
        return n.trim().to_string();
    }
    if let Some(n) = stack_kind_name(path) {
        return n;
    }
    std::fs::canonicalize(path)
        .ok()
        .and_then(|p| {
            p.parent()
                .and_then(|d| d.file_name())
                .and_then(|d| d.to_str())
                .map(String::from)
        })
        .unwrap_or_else(|| "default".to_string())
}

/// The `metadata.name` of a `kind: Stack` document, read from the RAW file.
///
/// It has to be read raw: `manifest::load` expands a Stack into its children and
/// the Stack document itself does not survive, so by the time anything else sees
/// the manifest the name is gone.
fn stack_kind_name(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for doc in serde_yaml::Deserializer::from_str(&text) {
        let v = serde_yaml::Value::deserialize(doc).ok()?;
        let kind = v.get("kind")?.as_str().unwrap_or_default();
        if manifest::canonical_kind(kind) == "Stack" {
            return v.get("metadata")?.get("name")?.as_str().map(str::to_string);
        }
    }
    None
}

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
        /// Don't apply anything — print the full manifest with every default
        /// filled in (like `kubectl apply --dry-run=client -o yaml`). Stacks are
        /// expanded and Kinds canonicalized, so you see exactly what WOULD run.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Authorize DESTROYING and recreating a resource whose change does not
        /// converge live: `--replace <Kind>/<name>` (repeatable), or
        /// `--replace all`. Without it, `apply` refuses and changes nothing.
        #[arg(long = "replace", value_name = "KIND/NAME")]
        replace: Vec<String>,
    },
    /// Shows what an `apply` WOULD change, without changing anything.
    ///
    /// Compares the manifest against the machine and against the last spec this
    /// stack applied (a three-way diff, so a field a human set by hand is left
    /// alone while a field removed from the manifest is reverted). With the
    /// manifest unchanged, whatever it prints IS drift.
    Plan {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Stack name (owner of the resources). Default: a `kind: Stack`'s name,
        /// else the manifest's directory.
        #[arg(long)]
        name: Option<String>,
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: super::output::OutputFormat,
        /// Exit 2 when there are changes (0 = none, 1 = error) — the
        /// `terraform plan -detailed-exitcode` contract, for a CI drift gate.
        #[arg(long = "detailed-exitcode")]
        detailed_exitcode: bool,
        /// Print WHICH fields the plan compares, per Kind, and exit. Answers
        /// "why is my change not showing up?" without reading the source.
        #[arg(long = "fields")]
        fields: bool,
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
        StackCmd::Init { .. } => unreachable!("handled above"),
        StackCmd::Apply {
            file,
            dry_run,
            replace,
        } => {
            if dry_run {
                let path = manifest::resolve_path(file)?;
                let docs = manifest::load(&path)?;
                print!("{}", manifest::render_with_defaults(&docs)?);
                Ok(())
            } else {
                apply(file, replace)
            }
        }
        StackCmd::Plan {
            file,
            name,
            output,
            detailed_exitcode,
            fields,
        } => plan_cmd(file, name, output, detailed_exitcode, fields),
        StackCmd::Ls { file } => ls(file),
        StackCmd::Describe { file } => describe(file),
        StackCmd::Validate { file } => validate(file),
    }
}

/// The stack Kinds, in the SAME order as `apply` — whoever reads `describe` sees
/// the order in which things are created, which is half the diagnosis when an
/// apply stops halfway.
const KINDS: [&str; 13] = [
    "Secret",
    "Network",
    "Volume",
    "Storage",
    "Image",
    "Vm",
    "Container",
    "Pod",
    "Ingress",
    "Egress",
    "FirewallPolicy",
    "HTTPRoute",
    "Dependency",
];

/// `stack ls` — the structure the manifest composes, in a single TABLE
/// (kind→name→presence→status), reusing exactly the resolution of
/// `describe` (`presence` queries the real stores; the stack has no registry
/// of its own, by design — see AGENTS.md).
/// The Kinds that CONVERGE in this version — a changed field really is applied.
/// Everything else in [`KINDS`] stays "ensure present" and the plan says so out
/// loud (`Action::NotConverged`) instead of leaving the resource out. A plan
/// that omits a resource reads as «no changes», which is the exact dishonesty
/// this whole feature exists to remove.
pub(crate) const CONVERGING_KINDS: [&str; 4] = ["Network", "Volume", "Container", "Pod"];

/// Everything the manifest asks for, in the reconciler's comparable form.
fn desired_of(docs: &[manifest::ManifestDoc]) -> Result<Vec<reconcile::Desired>> {
    let mut out = Vec::new();
    for kind in KINDS {
        for doc in manifest::of_kind(docs, kind) {
            out.push(match kind {
                "Container" => super::container::desired(doc)?,
                "Volume" => super::volume::desired(doc)?,
                "Network" => super::network::desired(doc)?,
                "Pod" => super::pod::desired(doc)?,
                _ => reconcile::Desired {
                    kind: kind.to_string(),
                    name: doc.metadata.name.clone(),
                    fields: Default::default(),
                    converges: false,
                },
            });
        }
    }
    Ok(out)
}

/// Everything on the machine, in the same form.
///
/// The converging Kinds are enumerated in full (that is what makes pruning
/// possible). The others are only probed for the names the manifest declares —
/// there is no way to enumerate, say, every HTTPRoute with an owner, so they can
/// never be prune candidates, and pretending otherwise would risk deleting
/// something nobody claimed.
fn actual_of(docs: &[manifest::ManifestDoc]) -> Result<Vec<reconcile::Actual>> {
    let mut out = super::container::actual()?;
    out.extend(super::volume::actual()?);
    out.extend(super::network::actual()?);
    out.extend(super::pod::actual()?);
    let (_, cstore) = super::util::open_stores()?;
    let containers = cstore.list().unwrap_or_default();
    for kind in KINDS {
        if CONVERGING_KINDS.contains(&kind) {
            continue;
        }
        for doc in manifest::of_kind(docs, kind) {
            let (present, _) = presence(kind, &doc.metadata.name, &containers);
            if present == "yes" {
                out.push(reconcile::Actual {
                    kind: kind.to_string(),
                    name: doc.metadata.name.clone(),
                    fields: Default::default(),
                    owner: None,
                    last_applied: None,
                });
            }
        }
    }
    Ok(out)
}

/// Reads both sides and decides. The decision itself is
/// [`reconcile::plan`] — pure, and tested as data.
pub(crate) fn build_plan(docs: &[manifest::ManifestDoc], stack: &str) -> Result<Vec<Change>> {
    Ok(reconcile::plan(
        &desired_of(docs)?,
        &actual_of(docs)?,
        stack,
    ))
}

/// Which fields the plan compares, per converging Kind.
///
/// This exists because the honest answer to «why did my `env:` change not show
/// up in the plan?» is a list, and making the user read the source for it is the
/// opposite of the goal. The comparable set is deliberately conservative: a
/// field whose two sides normalize differently would show as a difference on
/// EVERY plan, and a plan that always reports drift is worth less than no plan.
fn print_compared_fields() {
    println!(
        "{}",
        super::po::t("Fields compared by `stack plan`, per Kind:")
    );
    println!();
    let mut t = super::output::Table::new(&["KIND", "FIELDS"]);
    for (kind, fields) in [
        ("Container", super::container::RECONCILED_CONTAINER_FIELDS),
        ("Volume", super::volume::RECONCILED_VOLUME_FIELDS),
        ("Network", super::network::RECONCILED_NETWORK_FIELDS),
        ("Pod", super::pod::RECONCILED_POD_FIELDS),
    ] {
        t.row(vec![kind.to_string(), fields.join(", ")]);
    }
    t.print();
    println!();
    println!(
        "{}",
        super::po::tf(
            "Every other Kind ({kinds}) is ensure-present: `apply` creates it if missing and \
             never updates it.",
            &[(
                "kinds",
                &KINDS
                    .iter()
                    .filter(|k| !CONVERGING_KINDS.contains(k))
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", "),
            )],
        )
    );
    println!(
        "{}",
        super::po::t(
            "Not compared on a Container, and why: `env` and `command` (the record holds them \
             merged with the image's), `user` (the record stores the resolved uid), `labels` \
             (the engine adds its own)."
        )
    );
}

fn plan_cmd(
    file: Option<PathBuf>,
    name: Option<String>,
    output: super::output::OutputFormat,
    detailed_exitcode: bool,
    fields: bool,
) -> Result<()> {
    if fields {
        print_compared_fields();
        return Ok(());
    }
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;
    let stack = stack_name(&path, name.as_deref());
    let changes = build_plan(&docs, &stack)?;
    let any = changes.iter().any(|c| c.changed);
    match output {
        super::output::OutputFormat::Json => super::output::print_json(&changes)?,
        super::output::OutputFormat::Table => render_plan(&path, &stack, &changes),
    }
    // `terraform plan -detailed-exitcode`'s contract, which is what anyone
    // wiring this into CI already has in their fingers: 0 = nothing to do,
    // 2 = there are changes, 1 = the command failed. Opt-in, so the plain
    // `plan` keeps returning 0 and no existing script changes meaning.
    if detailed_exitcode && any {
        std::process::exit(2);
    }
    Ok(())
}

/// The localized justification for a change, composed from the STRUCTURED
/// fields — never by translating `Change::reason`, which stays English because
/// it is part of the `-o json` payload (ADR-0005: machine-readable output does
/// not change with the locale).
fn explain(c: &Change) -> Option<String> {
    match c.action {
        Action::Replace => Some(super::po::tf(
            "does not converge live: {fields}",
            &[("fields", &c.cold_fields.join(", "))],
        )),
        Action::Conflict => Some(super::po::tf(
            "owned by the stack '{owner}'",
            &[("owner", c.owner.as_deref().unwrap_or("?"))],
        )),
        Action::Adopt => {
            Some(super::po::t("exists and belongs to no stack — will be taken over").to_string())
        }
        Action::Delete => Some(super::po::t("no longer declared in the manifest").to_string()),
        Action::NotConverged => {
            Some(super::po::t("this Kind is ensure-present in this version").to_string())
        }
        _ => None,
    }
}

/// The human rendering of a plan.
fn render_plan(path: &Path, stack: &str, changes: &[Change]) {
    println!(
        "{}",
        super::po::tf(
            "Plan for stack \"{stack}\"  (manifest: {path})",
            &[("stack", stack), ("path", &path.display().to_string())],
        )
    );
    println!();
    if changes.is_empty() {
        println!("  {}", super::po::t("the manifest declares nothing"));
        return;
    }
    for c in changes {
        let head = format!("  {:<3} {}/{}", c.action.marker(), c.kind, c.name);
        match explain(c) {
            Some(r) => println!("{head}  — {r}"),
            None => println!("{head}"),
        }
        for d in &c.diffs {
            // `∅` for absent on either side: an empty string and «the field is
            // not there» are different facts, and a diff that renders them the
            // same sends the reader looking for a change that is not there.
            let from = d.from.as_deref().unwrap_or("∅");
            let to = d.to.as_deref().unwrap_or("∅");
            println!("        {}: {from} → {to}", d.field);
        }
    }
    println!();
    let s = reconcile::summary(changes);
    // The counter labels are translated for the human table; the JSON keeps the
    // stable `snake_case` keys (ADR-0005 — field names never change with the
    // locale, or every consumer in another language breaks).
    let parts: Vec<String> = s
        .iter()
        .map(|(k, n)| {
            let label = match *k {
                "create" => super::po::t("to create"),
                "adopt" => super::po::t("to adopt"),
                "update" => super::po::t("to update"),
                "replace" => super::po::t("to replace"),
                "unchanged" => super::po::t("unchanged"),
                "delete" => super::po::t("to remove"),
                "conflict" => super::po::t("in conflict"),
                _ => super::po::t("not converging"),
            };
            format!("{n} {label}")
        })
        .collect();
    println!("{}: {}", super::po::t("Summary"), parts.join(" · "));
    if changes.iter().any(|c| c.action == Action::Replace) {
        println!();
        println!(
            "{}",
            super::po::t(
                "a replace destroys and recreates the resource — `stack apply` refuses it \
                 unless you pass `--replace`"
            )
        );
    }
    if changes.iter().any(|c| c.action == Action::NotConverged) {
        println!(
            "{}",
            super::po::t(
                "`!` = this Kind is ensure-present in this version: `apply` creates it if \
                 missing and never updates it"
            )
        );
    }
}

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
            "{}",
            super::po::tf(
                "WARNING: kinds not supported by the stack (ignored by `apply`): {kinds}",
                &[("kinds", &desconhecidos.join(", "))],
            )
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
        // A Pod is present if it has member containers (label `delonix.io/pod`).
        "Pod" => {
            let members = containers
                .iter()
                .filter(|c| {
                    c.labels
                        .get(super::pod::POD_LABEL)
                        .map(|v| v == name)
                        .unwrap_or(false)
                })
                .count();
            if members == 0 {
                ("no".into(), "-".into())
            } else {
                ("yes".into(), format!("{members} container(s)"))
            }
        }
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
        _ => ("?".into(), super::po::t("unsupported kind").into()),
    }
}

fn yes_no(b: bool) -> (String, String) {
    if b {
        ("yes".into(), "present".into())
    } else {
        ("no".into(), "-".into())
    }
}

/// Refuses, before touching anything, whatever this `apply` is not allowed to do.
///
/// Runs BEFORE the first creation on purpose. `apply` is fail-fast without
/// rollback, so a stack containing one resource that needs recreating must not
/// get half-applied and only then complain — the user would be left with a
/// partially converged stack and a refusal, which is the worst of both.
fn refuse_unallowed(changes: &[Change], replace: &[String]) -> Result<()> {
    let allow_all = replace.iter().any(|r| r == "all");
    let mut blocked = Vec::new();
    for c in changes {
        match c.action {
            Action::Conflict => blocked.push(format!(
                "{}/{}: {}",
                c.kind,
                c.name,
                explain(c).unwrap_or_default()
            )),
            Action::Replace if !allow_all => {
                let key = format!("{}/{}", c.kind, c.name);
                if !replace.iter().any(|r| *r == key || *r == c.name) {
                    blocked.push(super::po::tf(
                        "{key}: {reason} — pass `--replace {key}` (or `--replace all`) to \
                         destroy and recreate it",
                        &[("key", &key), ("reason", &explain(c).unwrap_or_default())],
                    ));
                }
            }
            _ => {}
        }
    }
    if blocked.is_empty() {
        return Ok(());
    }
    for b in &blocked {
        eprintln!("  ✗ {b}");
    }
    Err(delonix_runtime_core::Error::Invalid(super::po::tf(
        "stack apply refused: {n} resource(s) need an explicit decision (nothing was changed)",
        &[("n", &blocked.len().to_string())],
    )))
}

fn apply(file: Option<PathBuf>, replace: Vec<String>) -> Result<()> {
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
        return Err(delonix_runtime_core::Error::Invalid(super::po::tf(
            "stack apply aborted: {n} unresolved reference(s) (fix the manifest or use `stack validate`)",
            &[("n", &issues.len().to_string())],
        )));
    }
    // Decide EVERYTHING before changing anything, and refuse up front what this
    // invocation is not allowed to do (a conflict, or a recreation nobody asked
    // for). Only then start creating.
    let stack = stack_name(&path, None);
    let changes = build_plan(&docs, &stack)?;
    refuse_unallowed(&changes, &replace)?;
    // A resource that has to be recreated is destroyed FIRST, so the normal
    // creation pass below builds it fresh. Doing it in this order means there is
    // exactly one creation path (the one that has always existed), instead of a
    // second "recreate" path that would drift away from it.
    destroy_for_replace(&changes)?;

    // Secrets first: `Storage.passwordSecret` and `Container.secret` reference them.
    // `base` = the manifest folder, so `fromEnvFile` resolves next to it.
    let base = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    super::secret::apply(&docs, base)?;
    super::network::apply(&docs)?;
    super::volume::apply(&docs)?;
    super::storage::apply(&docs)?;
    // ShareVolume right after Storage: it carves subdirectories out of an
    // already-mounted Storage, so the parent must exist first.
    super::sharevolume::apply(&docs)?;
    super::image::apply(&docs)?;
    super::vm::apply(&docs)?;
    super::container::apply(&docs)?;
    super::pod::apply(&docs)?;
    super::firewall::apply(&docs)?;
    // Dependency (directed reachability) — after the firewall and the containers
    // (it needs the IPs); compiles to default-deny ingress + allows on the `to`.
    super::dependency::apply(&docs)?;
    // HTTPRoute LAST: it needs the backend containers already created (with IP) to
    // resolve the routes; brings up/reloads the L7 reverse-proxy.
    super::httproute::apply(&docs)?;
    // Tunnel LAST of all: its `localPort` is typically the HTTPRoute proxy's own
    // listening port (see `cmd::tunnel`'s module doc) — must already be up.
    super::tunnel::apply(&docs)?;

    // Everything that exists is now created; converge what differs, and stamp
    // ownership + the applied spec on all of it. The stamp is what makes the
    // NEXT plan a three-way diff instead of a two-way one.
    converge_and_stamp(&docs, &stack, &changes)?;

    // After creating everything, say what was created but will NOT work as it
    // appears without a host prerequisite (network mount in rootless, etc.) —
    // it is here, in the real creation flow, that the user needs to know it.
    print_missing_conditions(&docs);
    Ok(())
}

/// Destroys the resources the plan marked for recreation (already authorized by
/// [`refuse_unallowed`]), so the normal creation pass rebuilds them.
fn destroy_for_replace(changes: &[Change]) -> Result<()> {
    for c in changes.iter().filter(|c| c.action == Action::Replace) {
        println!(
            "{}",
            super::po::tf(
                "{kind}/{name}: recreating",
                &[("kind", &c.kind), ("name", &c.name)],
            )
        );
        match c.kind.as_str() {
            "Container" => super::container::remove_for_replace(&c.name)?,
            "Volume" => super::volume::remove_for_replace(&c.name)?,
            "Network" => super::network::remove_for_replace(&c.name)?,
            "Pod" => super::pod::remove_pod(&c.name, true)?,
            other => {
                return Err(delonix_runtime_core::Error::Invalid(format!(
                    "{other}/{}: recreation is not implemented for this Kind",
                    c.name
                )))
            }
        }
    }
    Ok(())
}

/// Applies the hot changes and records ownership + the applied spec.
///
/// The stamp is written for EVERY converging resource the manifest declares,
/// including the ones that came out `NoOp` — a resource created by an older
/// version has no stamp, and without one it would be re-adopted on every run and
/// could never be pruned.
fn converge_and_stamp(
    docs: &[manifest::ManifestDoc],
    stack: &str,
    changes: &[Change],
) -> Result<()> {
    for c in changes {
        if !CONVERGING_KINDS.contains(&c.kind.as_str()) {
            continue;
        }
        if c.action == Action::Update {
            println!(
                "{}",
                super::po::tf(
                    "{kind}/{name}: updating {n} field(s) live",
                    &[
                        ("kind", &c.kind),
                        ("name", &c.name),
                        ("n", &c.diffs.len().to_string()),
                    ],
                )
            );
            match c.kind.as_str() {
                "Container" => super::container::converge(&c.name, &c.diffs)?,
                "Volume" => super::volume::converge(&c.name, &c.diffs)?,
                "Network" => super::network::converge(&c.name, &c.diffs)?,
                // A Pod has no hot field at all, so the planner can never emit
                // `Update` for one. Saying so beats a silent no-op if that ever
                // changes.
                other => {
                    return Err(delonix_runtime_core::Error::Invalid(format!(
                        "{other}/{}: no live update path",
                        c.name
                    )))
                }
            }
        }
    }
    // Re-derive the desired fields from the manifest (not from the plan): the
    // stamp must record what was ASKED for, which is also what the next run will
    // compare against.
    for d in desired_of(docs)? {
        if !CONVERGING_KINDS.contains(&d.kind.as_str()) {
            continue;
        }
        let r = match d.kind.as_str() {
            "Container" => super::container::stamp(&d.name, stack, &d.fields),
            "Volume" => super::volume::stamp(&d.name, stack, &d.fields),
            "Network" => super::network::stamp(&d.name, stack, &d.fields),
            "Pod" => super::pod::stamp(&d.name, stack, &d.fields),
            _ => Ok(()),
        };
        // A stamp that fails must not fail the apply — the resource IS created
        // and working; what is lost is the ownership record. Say it loudly
        // instead, because the consequence (it will be re-adopted next run, and
        // never pruned) is invisible otherwise.
        if let Err(e) = r {
            eprintln!(
                "{}",
                super::po::tf(
                    "WARNING: {kind}/{name}: could not record stack ownership ({err}) — it \
                     will be adopted again on the next apply and `--prune` will not see it",
                    &[
                        ("kind", &d.kind),
                        ("name", &d.name),
                        ("err", &e.to_string())
                    ],
                )
            );
        }
    }
    Ok(())
}

/// `stack validate` — dry-run: only runs `validate_graph` and reports, without applying.
fn validate(file: Option<PathBuf>) -> Result<()> {
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;
    let issues = validate_graph(&docs);
    if issues.is_empty() {
        println!(
            "{}",
            super::po::tf(
                "stack validate: OK — {n} document(s), all references resolved",
                &[("n", &docs.len().to_string())],
            )
        );
        Ok(())
    } else {
        for i in &issues {
            println!("  ✗ {i}");
        }
        Err(delonix_runtime_core::Error::Invalid(super::po::tf(
            "{n} unresolved reference(s)",
            &[("n", &issues.len().to_string())],
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
            issues.push(super::po::tf(
                "{kind} '{name}' declared more than once",
                &[("kind", &doc.kind), ("name", &doc.metadata.name)],
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
                        issues.push(super::po::tf(
                            "{kind} '{name}' → network '{net}' is not declared nor does it exist",
                            &[("kind", &doc.kind), ("name", name), ("net", net)],
                        ));
                    }
                }
                for vref in volume_refs(doc) {
                    if !volumes.contains(&vref) {
                        issues.push(super::po::tf(
                            "{kind} '{name}' → volume '{vref}' is not declared (Volume/Storage) nor does it exist",
                            &[("kind", &doc.kind), ("name", name), ("vref", &vref)],
                        ));
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
                                issues.push(super::po::tf(
                                    "Vm '{name}' → volume '{vname}' is not declared (Volume/Storage) nor does it exist",
                                    &[("name", name), ("vname", vname)],
                                ));
                            }
                        }
                    }
                }
                // `Container.secret: [names]` — each one must be a Secret.
                if let Some(seq) = doc.spec.get("secret").and_then(|v| v.as_sequence()) {
                    for sref in seq.iter().filter_map(|v| v.as_str()) {
                        if !secrets.contains(sref) {
                            issues.push(super::po::tf(
                                "{kind} '{name}' → secret '{sref}' is not a declared or existing Secret",
                                &[("kind", &doc.kind), ("name", name), ("sref", sref)],
                            ));
                        }
                    }
                }
            }
            "Storage" => {
                // `Storage.passwordSecret` references a Secret (the mount reads the
                // `password` key of that Secret — `storage::resolve_password`).
                if let Some(sref) = doc.spec.get("passwordSecret").and_then(|v| v.as_str()) {
                    if !secrets.contains(sref) {
                        issues.push(super::po::tf(
                            "Storage '{name}' → passwordSecret '{sref}' is not a declared or existing Secret",
                            &[("name", name), ("sref", sref)],
                        ));
                    } else if let Some(Some(keys)) = declared_secret_keys.get(sref) {
                        // Only when we know the keys (inline Secret without fromEnvFile):
                        // then we can assert with certainty that `password` is missing.
                        if !keys.contains("password") {
                            issues.push(super::po::tf(
                                "Storage '{name}' → passwordSecret '{sref}': the Secret does not declare the 'password' key (the mount reads exactly that key)",
                                &[("name", name), ("sref", sref)],
                            ));
                        }
                    }
                }
            }
            "Egress" | "FirewallPolicy" => {
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
                        issues.push(super::po::tf(
                            "FirewallPolicy '{name}' → direction is required and ∈ {{ingress, egress}}",
                            &[("name", name)],
                        ));
                    } else if dir == Some("ingress") && scope == "network" {
                        // Same incompatibility the apply rejects — catch it beforehand.
                        issues.push(super::po::tf(
                            "FirewallPolicy '{name}' → scope: network is only supported with direction: egress",
                            &[("name", name)],
                        ));
                    }
                }
                if !matches!(scope, "container" | "network") {
                    // Message consistent with the apply (which also rejects the scope).
                    issues.push(super::po::tf(
                        "{kind} '{name}' → invalid scope '{scope}' (use container|network)",
                        &[("kind", &doc.kind), ("name", name), ("scope", scope)],
                    ));
                } else if let Some(target) = doc.spec.get("target").and_then(|v| v.as_str()) {
                    // scope: network → the target is a NETWORK; otherwise, a Container.
                    if scope == "network" {
                        if !networks.contains(target) {
                            issues.push(super::po::tf(
                                "{kind} '{name}' (scope network) → target '{target}' is not a declared or existing Network",
                                &[("kind", &doc.kind), ("name", name), ("target", target)],
                            ));
                        }
                    } else if !containers.contains(target) {
                        issues.push(super::po::tf(
                            "{kind} '{name}' → target '{target}' is not a declared or existing Container",
                            &[("kind", &doc.kind), ("name", name), ("target", target)],
                        ));
                    }
                }
            }
            "HTTPRoute" | "Ingress" => {
                // Each backend.service must be a declared/existing Container;
                // the tls.secretRef (if used) a Secret. Reuses the typed parser to
                // avoid duplicating the schema (and catches an invalid spec right away).
                // `kind: Ingress` (k8s-shaped) is converted to the same HttpRouteSpec.
                let parsed = if doc.kind == "Ingress" {
                    super::httproute::ingress_spec_of(doc)
                } else {
                    manifest::spec_of::<super::httproute::HttpRouteSpec>(doc)
                };
                match parsed {
                    Ok(spec) => {
                        if let Err(e) = super::httproute::validate_spec(name, &spec) {
                            issues.push(e.to_string());
                        }
                        for rule in &spec.rules {
                            for pr in &rule.paths {
                                if !containers.contains(&pr.backend.service) {
                                    issues.push(super::po::tf(
                                        "HTTPRoute '{name}' → backend '{service}' is not a declared or existing Container",
                                        &[("name", name), ("service", &pr.backend.service)],
                                    ));
                                }
                            }
                        }
                        if let Some(tls) = &spec.tls {
                            if tls.mode.as_deref() == Some("secretRef") {
                                if let Some(sref) = &tls.secret_ref {
                                    if !secrets.contains(sref) {
                                        issues.push(super::po::tf(
                                            "HTTPRoute '{name}' → tls.secretRef '{sref}' is not a declared or existing Secret",
                                            &[("name", name), ("sref", sref)],
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
                        issues.push(super::po::tf(
                            "Dependency '{name}' → from '{f}' is not a declared or existing Container",
                            &[("name", name), ("f", f)],
                        ));
                    }
                    None => issues.push(super::po::tf(
                        "Dependency '{name}' → `from` is required",
                        &[("name", name)],
                    )),
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
                    issues.push(super::po::tf(
                        "Dependency '{name}' → `to` cannot be empty",
                        &[("name", name)],
                    ));
                }
                for t in tos {
                    if !containers.contains(t) {
                        issues.push(super::po::tf(
                            "Dependency '{name}' → to '{t}' is not a declared or existing Container",
                            &[("name", name), ("t", t)],
                        ));
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
kind: FirewallPolicy
metadata: { name: web-in }
spec: { direction: ingress, target: web }
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
            i.iter().any(|s| s.contains("direction is required")),
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
                .any(|s| s.contains("scope: network is only supported with direction: egress")),
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
        assert!(issues[0].contains("declared more than once"), "{issues:?}");
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
            issues[0].contains("does not declare the 'password' key"),
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

    /// The trap that `compose` actually shipped: `default_project_name`
    /// collapsed every project to `"default"` for a RELATIVE path, so a
    /// `down -v` in one project could delete another project's volume. And the
    /// test that should have caught it encoded the bug — it only ever passed
    /// ABSOLUTE paths, while the real invocation is always relative
    /// (`-f delonix-manifest.yaml`). So this one uses the relative form on
    /// purpose, which is the only form that proves anything.
    #[test]
    fn stack_name_vem_do_directorio_mesmo_com_caminho_relativo() {
        let base = std::env::temp_dir().join(format!("dlx-stackname-{}", std::process::id()));
        let dir = base.join("o-meu-projecto");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("delonix-manifest.yaml");
        std::fs::write(
            &file,
            "apiVersion: delonix.io/v1
kind: Volume
metadata: { name: v }
spec: {}
",
        )
        .unwrap();

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let relative = super::stack_name(std::path::Path::new("delonix-manifest.yaml"), None);
        std::env::set_current_dir(orig).unwrap();
        assert_eq!(
            relative, "o-meu-projecto",
            "a relative path must not collapse to a shared default name"
        );

        // Absolute gives the same answer — the two forms must not disagree.
        assert_eq!(super::stack_name(&file, None), "o-meu-projecto");
        // `--name` always wins, and blank is treated as absent rather than as
        // an empty stack name that would own nothing.
        assert_eq!(super::stack_name(&file, Some("outro")), "outro");
        assert_eq!(super::stack_name(&file, Some("  ")), "o-meu-projecto");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A `kind: Stack` names the stack, and it has to be read from the RAW file:
    /// `manifest::load` expands a Stack into its children and the Stack document
    /// itself does not survive the load.
    #[test]
    fn um_kind_stack_da_o_nome_e_ganha_ao_directorio() {
        let dir = std::env::temp_dir().join(format!("dlx-stackkind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("m.yaml");
        std::fs::write(
            &file,
            "apiVersion: delonix.io/v1\nkind: Stack\nmetadata: { name: loja }\nspec:\n  volumes:\n    - name: v\n      spec: {}\n",
        )
        .unwrap();
        assert_eq!(super::stack_name(&file, None), "loja");
        // ...but an explicit `--name` still wins over it.
        assert_eq!(super::stack_name(&file, Some("x")), "x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
