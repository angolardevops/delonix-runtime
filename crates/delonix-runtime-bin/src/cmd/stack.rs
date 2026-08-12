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
    /// Initializes a COMPLETE project: Delonixfile + manifest + cluster + README.
    ///
    /// Files ALREADY FILLED IN (images included), ready to use without
    /// editing anything.
    Init {
        /// Project directory (default: the current one).
        #[arg(value_hint = clap::ValueHint::DirPath, default_value = ".")]
        dir: PathBuf,
        /// Project name (default: the directory name).
        #[arg(long)]
        name: Option<String>,
        /// Image to use. Omit = fills in with the default image.
        #[arg(long, add = clap_complete::engine::ArgValueCandidates::new(super::complete::images))]
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
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
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
        /// Also REMOVE what this stack owns and the manifest no longer declares.
        /// Never happens without this flag.
        #[arg(long = "prune")]
        prune: bool,
    },
    /// Removes everything this stack owns (by the `delonix.io/stack` label).
    ///
    /// A resource created by hand, or belonging to another stack, is never
    /// touched. Removal happens in the REVERSE of the creation order, so a
    /// network is not pulled from under the containers still attached to it.
    Destroy {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Stack name. Default: a `kind: Stack`'s name, else the manifest's directory.
        #[arg(long)]
        name: Option<String>,
        /// List what would be removed, and remove nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Shows what an `apply` WOULD change, without changing anything.
    ///
    /// Compares the manifest against the machine and against the last spec this
    /// stack applied (a three-way diff, so a field a human set by hand is left
    /// alone while a field removed from the manifest is reverted). With the
    /// manifest unchanged, whatever it prints IS drift.
    Plan {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
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
    /// List the structure the manifest composes, and whether each resource exists.
    ///
    /// Containers, volumes, networks, ... — the tabular summary of
    /// `describe`.
    Ls {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Stack detail in `kubectl describe` style.
    ///
    /// Each resource DECLARED in the manifest and whether or not it is present
    /// on the machine.
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
    Describe {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Blocks until every declared resource is present and, where it has one, healthy.
    ///
    /// The command an `apply` in CI is missing: `apply` returns as soon as it has
    /// created things, which is not the same as the stack working. Exits non-zero
    /// on timeout, naming exactly what did not come up.
    Wait {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Give up after this many seconds (default 120).
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Validates the manifest WITHOUT touching anything (dry-run).
    ///
    /// Resolves the cross-references (`Container.network`/`.volumes`,
    /// `Vm.network`, `Ingress/Egress.target`) against what the manifest
    /// declares PLUS what already exists in the stores. Exits with an error if
    /// any reference is left unresolved — it is the safety net against an
    /// `apply` that would only fail halfway through (fail-fast, no rollback).
    Validate {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Fail (exit != 0) when a field was ignored, instead of only warning about it — for CI.
        #[arg(long)]
        strict: bool,
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
        return init_for(
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
            prune,
        } => {
            if dry_run {
                let path = manifest::resolve_path(file)?;
                let docs = manifest::load(&path)?;
                print!("{}", manifest::render_with_defaults(&docs)?);
                Ok(())
            } else {
                apply(file, replace, prune)
            }
        }
        StackCmd::Plan {
            file,
            name,
            output,
            detailed_exitcode,
            fields,
        } => plan_cmd(file, name, output, detailed_exitcode, fields),
        StackCmd::Destroy {
            file,
            name,
            dry_run,
        } => destroy(file, name, dry_run),
        StackCmd::Wait { file, timeout } => wait(file, timeout),
        StackCmd::Ls { file } => ls(file),
        StackCmd::Describe { file } => describe(file),
        StackCmd::Validate { file, strict } => validate(file, strict),
    }
}

/// The stack Kinds, in the SAME order as `apply` — whoever reads `describe` sees
/// the order in which things are created, which is half the diagnosis when an
/// apply stops halfway.
const KINDS: [&str; 13] = [
    "Secret",
    "Network",
    "NetworkRoute",
    "Volume",
    // Carved out of a Volume, so it comes after one. It was APPLIED by
    // `stack apply` and missing from this list, which meant `ls`, `describe`
    // and `plan` never mentioned a resource the apply creates — the same silent
    // omission this whole effort keeps removing. Same for `Tunnel` below.
    "ShareVolume",
    "Image",
    "Vm",
    "Container",
    "Pod",
    "Ingress",
    "FirewallPolicy",
    "HTTPRoute",
    // Last, like the apply: a tunnel's `localPort` is typically the L7 proxy's
    // own listening port, so the proxy has to be up first.
    "Tunnel",
];

/// `stack ls` — the structure the manifest composes, in a single TABLE
/// (kind→name→presence→status), reusing exactly the resolution of
/// `describe` (`presence` queries the real stores; the stack has no registry
/// of its own, by design — see CLAUDE.md).
/// The Kinds that CONVERGE in this version — a changed field really is applied.
/// Everything else in [`KINDS`] stays "ensure present" and the plan says so out
/// loud (`Action::NotConverged`) instead of leaving the resource out. A plan
/// that omits a resource reads as «no changes», which is the exact dishonesty
/// this whole feature exists to remove.
pub(crate) const CONVERGING_KINDS: [&str; 11] = [
    "Network",
    "Volume",
    "ShareVolume",
    "Image",
    "Vm",
    "Container",
    "Pod",
    "FirewallPolicy",
    "HTTPRoute",
    "Ingress",
    "Tunnel",
];

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
                "Image" => super::image::desired(doc)?,
                "Vm" => super::vm::desired(doc)?,
                "FirewallPolicy" => super::firewall::desired(doc)?,
                "ShareVolume" => super::sharevolume::desired(doc)?,
                "HTTPRoute" | "Ingress" => super::httproute::desired(doc)?,
                "Tunnel" => super::tunnel::desired(doc)?,
                _ => reconcile::Desired {
                    kind: kind.to_string(),
                    name: doc.metadata.name.clone(),
                    fields: Default::default(),
                    converges: false,
                    ownable: true,
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
    out.extend(super::image::actual(docs)?);
    out.extend(super::vm::actual()?);
    out.extend(super::firewall::actual(docs)?);
    out.extend(super::sharevolume::actual(docs)?);
    out.extend(super::httproute::actual(docs)?);
    out.extend(super::tunnel::actual(docs)?);
    let (_, cstore) = super::util::open_stores()?;
    let containers = cstore.list().unwrap_or_default();
    for kind in KINDS {
        if CONVERGING_KINDS.contains(&kind) {
            continue;
        }
        for doc in manifest::of_kind(docs, kind) {
            let (present, _) = presence(kind, doc, &containers);
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
    let mut changes = reconcile::plan(&desired_of(docs)?, &actual_of(docs)?, stack);
    // Attach the prerequisites the host does not meet — the difference between
    // «will exist» and «will work». They were already computed before this, but
    // only the END of an `apply` printed them: a user learned that their NFS
    // volume does not actually mount AFTER creating it, and only if the apply
    // got that far (it often does not — the mount is what fails).
    //
    // The host is probed ONCE for the whole plan: the probe shells out looking
    // for mount helpers and a hypervisor, and doing it per resource would make a
    // plan of twenty volumes twenty times slower for twenty identical answers.
    let env = super::conditions::Env::probe();
    for c in changes.iter_mut() {
        // A deletion candidate is not in the manifest at all — there is no
        // document to derive prerequisites from, and it is on its way out.
        let Some(doc) = docs
            .iter()
            .find(|d| d.kind == c.kind && d.metadata.name == c.name)
        else {
            continue;
        };
        // Only the FAILING ones: a list of satisfied prerequisites is noise, and
        // noise in a plan is what makes people stop reading it.
        c.conditions = super::conditions::conditions_for(doc, &env)
            .into_iter()
            .filter(|x| !x.ok)
            .collect();
        // A `kind: Vm` accepts 36 spec fields and the reconciler compares five.
        // On a Create that is harmless — creation applies the whole spec. On a
        // VM that ALREADY EXISTS it was a silent drop: the plan said "no
        // changes" for a manifest declaring a TPM, a CPU topology and two extra
        // disks the machine did not have, and the apply reported success.
        //
        // Naming the fields is deliberately NOT the same as converging them
        // (see the ADR on the reboot class): converging these means rebooting
        // the VM, which is a capability, whereas saying so is honesty. The
        // engine ships the honesty first.
        if c.kind == "Vm" && c.action != reconcile::Action::Create {
            if let Some(cond) = super::vm::unconverged_fields_condition(doc) {
                c.conditions.push(cond);
            }
        }
    }
    Ok(changes)
}

/// Why a Kind is still ensure-present.
///
/// A generic «this Kind does not converge yet» reads as «nobody got to it», and
/// for most of these that is not true — each one has a concrete obstacle, and
/// naming it is the difference between a gap and a decision. Kept next to
/// `CONVERGING_KINDS` so adding a Kind to that list without removing its excuse
/// here is visible in one screen.
fn not_converged_reason(kind: &str) -> &'static str {
    match kind {
        // The applied state is COLLECTIVE: `resolve_config` merges every
        // HTTPRoute document into one proxy config, and `manual.json` records no
        // provenance — nothing says which document produced which route. So a
        // per-document diff has nothing to compare against. Recording provenance
        // in the proxy config is what would change this.
        // A secret's VALUES are the state, and they are encrypted at rest and
        // never read back for display. A diff would either say nothing useful or
        // decrypt to compare — and decrypting to draw a plan is not a trade
        // worth making.
        "Secret" => super::po::t(
            "the state is the encrypted values, and a plan will not decrypt them to compare",
        ),
        // A tunnel's identity is a live process and a URL a third party hands
        // out; the declared half (`localPort`/`provider`) is comparable, the
        // rest is status.
        _ => super::po::t("not converged in this version"),
    }
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
    for (kind, fields) in compared_fields_table() {
        t.row(vec![kind.to_string(), fields.join(", ")]);
    }
    t.print();
    println!();
    print_not_converged();
}

/// The compared fields, per Kind — ONE source for the printed table and for the
/// test that keeps it aligned with `CONVERGING_KINDS`.
pub(crate) fn compared_fields_table() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        ("Container", super::container::RECONCILED_CONTAINER_FIELDS),
        ("Volume", super::volume::RECONCILED_VOLUME_FIELDS),
        ("Network", super::network::RECONCILED_NETWORK_FIELDS),
        ("Image", super::image::RECONCILED_IMAGE_FIELDS),
        ("Vm", super::vm::RECONCILED_VM_FIELDS),
        ("FirewallPolicy", super::firewall::RECONCILED_FW_FIELDS),
        ("ShareVolume", super::sharevolume::RECONCILED_SHARE_FIELDS),
        ("HTTPRoute", super::httproute::RECONCILED_HTTPROUTE_FIELDS),
        ("Ingress", super::httproute::RECONCILED_HTTPROUTE_FIELDS),
        ("Tunnel", super::tunnel::RECONCILED_TUNNEL_FIELDS),
        ("Pod", super::pod::RECONCILED_POD_FIELDS),
    ]
}

/// The reasons, printed under the table.
fn print_not_converged() {
    println!();
    println!(
        "{}",
        super::po::t(
            "The remaining Kinds are ensure-present — `apply` creates them if missing \
                      and never updates them. Each for a concrete reason, not for lack of \
                      attention:"
        )
    );
    let mut t = super::output::Table::new(&["KIND", "WHY IT DOES NOT CONVERGE"]);
    for kind in KINDS.iter().filter(|k| !CONVERGING_KINDS.contains(k)) {
        t.row(vec![
            kind.to_string(),
            not_converged_reason(kind).to_string(),
        ]);
    }
    t.print();
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
        // A prerequisite the host does not meet is printed right under the
        // resource, because that is where it changes the decision: `+ Volume/x`
        // alone reads as "this will work", and this is the line that says it
        // will exist WITHOUT working.
        for cond in &c.conditions {
            println!(
                "        {} {}: {}",
                super::po::t("prerequisite"),
                cond.kind,
                cond.message
            );
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

/// `stack wait` — block until the stack is actually up.
///
/// **Why this is a command and not a `status` field on the manifest.** The
/// review that started this work asked for `status` separated from `spec`, and
/// the honest place for it turned out not to be the schema: a manifest
/// describes what the user WRITES, and nobody writes status. This engine also
/// persists no status — `reconcile_status`, `vm status` and `pod_ip` all derive
/// it on read, so a stored field would be a copy that goes stale. What was
/// genuinely missing is the CONSUMER: `apply` returns as soon as it has created
/// things, which is not the same as the stack working, and every CI pipeline
/// then invents its own `sleep`.
///
/// A failing PREREQUISITE (`conditions`) is reported but never waited on: a
/// volume that cannot mount in rootless will not start mounting in ninety
/// seconds, and blocking on it would turn an honest warning into a hang.
fn wait(file: Option<PathBuf>, timeout: u64) -> Result<()> {
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let env = super::conditions::Env::probe();

    // Said ONCE, up front: these never become true by waiting.
    for doc in &docs {
        for c in super::conditions::conditions_for(doc, &env)
            .into_iter()
            .filter(|c| !c.ok)
        {
            super::output::warn(&super::po::tf(
                "{kind}/{name}: {message}",
                &[
                    ("kind", &doc.kind),
                    ("name", &doc.metadata.name),
                    ("message", &c.message),
                ],
            ));
        }
    }

    loop {
        let (_, cstore) = super::util::open_stores()?;
        let containers = cstore.list().unwrap_or_default();
        let mut pending: Vec<String> = Vec::new();
        for kind in KINDS {
            for doc in manifest::of_kind(&docs, kind) {
                let (present, status) = presence(kind, doc, &containers);
                if is_pending(&present, kind, &status) {
                    pending.push(format!(
                        "{kind}/{} ({})",
                        doc.metadata.name,
                        // `absent` only for what is genuinely absent: `?` is a
                        // store that could not be read, and telling that one
                        // "absent" sends the reader looking for the wrong thing.
                        match present.as_str() {
                            "yes" => status.as_str(),
                            "no" => "absent",
                            _ => "unknown",
                        }
                    ));
                }
            }
        }
        if pending.is_empty() {
            println!(
                "{}",
                super::po::tf(
                    "stack \"{stack}\" is up",
                    &[("stack", &stack_name(&path, None))],
                )
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            for p in &pending {
                eprintln!("  ✗ {p}");
            }
            return Err(delonix_runtime_core::Error::Invalid(super::po::tf(
                "timed out after {secs}s waiting for {n} resource(s)",
                &[
                    ("secs", &timeout.to_string()),
                    ("n", &pending.len().to_string()),
                ],
            )));
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Whether `wait` still has to wait for this resource.
///
/// **A presence marker of `-` is not "absent".** This used to be
/// `present == "yes" && ready_status(...)`, which lumped together two states
/// that mean opposite things: a resource that is MISSING, and one that has no
/// observable presence to begin with. The declarative Kinds
/// (`Ingress`/`FirewallPolicy`/`HTTPRoute`/`Dependency`) are firewall directives
/// applied to a target, with no store of their own — `presence` reports `-` for
/// them and never `yes`. So ANY manifest containing one of them burned the whole
/// `--timeout` and then failed, over a stack that was entirely up: the command
/// written to replace CI's `sleep` was precisely the one CI could not use.
///
/// `ready_status` next door already had the right intent in its doc-comment
/// («Only the Kinds that HAVE a runtime state are judged on it»); what was
/// missing was the decision upstream of it.
///
/// **`?` stays pending, and that is deliberate**: it is what a store that could
/// not be read reports (`VolumeStore::open` failing), not just the unsupported
/// -kind arm. Calling an unknown "ready" would be the same dishonesty in the
/// other direction — which is why a Kind missing its `presence` arm is fixed
/// THERE and not by relaxing this.
fn is_pending(present: &str, kind: &str, status: &str) -> bool {
    match present {
        // Declarative: nothing to observe, so nothing to wait for.
        "-" => false,
        "yes" => !ready_status(kind, status),
        // "no" (absent) and "?" (unknown/unreadable) both keep waiting.
        _ => true,
    }
}

/// Whether a resource's reported status counts as ready.
///
/// Only the Kinds that HAVE a runtime state are judged on it. A volume or a
/// network is ready by existing — inventing a readiness notion for them would
/// make `wait` block on something that will never change.
fn ready_status(kind: &str, status: &str) -> bool {
    match kind {
        // `unhealthy` is deliberately NOT ready, and `starting` is not either:
        // a container with a healthcheck is exactly the case this command
        // exists for, and treating "starting" as done would return the moment
        // the process forks — which is the `sleep 5` this replaces.
        "Container" | "Pod" => {
            status.starts_with("Running") || status.starts_with("running") || status == "healthy"
        }
        "Vm" => status.starts_with("Running") || status.starts_with("running"),
        _ => true,
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
            let (present, status) = presence(kind, doc, &containers);
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
            let (present, status) = presence(kind, doc, &containers);
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

    // Same source as `plan` and `apply` — a describe that showed a different
    // set of conditions than the plan of the same manifest would be a third
    // answer to one question. `build_plan` is read-only (the reconciler is
    // pure; `actual_of` only reads the stores, which this command already did).
    match build_plan(&docs, &stack_name(&path, None)) {
        Ok(changes) => print_missing_conditions(&changes),
        // A describe is a read: if the plan cannot be built, say so and still
        // print everything above rather than failing the whole command.
        Err(e) => super::output::warn(&super::po::tf(
            "could not evaluate prerequisites: {err}",
            &[("err", &e.to_string())],
        )),
    }
    Ok(())
}

/// Prints the MISSING honesty conditions (privilege/host prerequisites that would
/// make a resource be created but not work as it appears to: network mount in
/// rootless, hard quota without root, network driver without a physical plane,
/// restart on a Cloud Hypervisor VM). Only the missing ones — it is the actionable
/// surface of "what is missing for this to really work". Shared by `describe`
/// AND by the end of `apply`: whoever runs `apply` (the real creation flow)
/// MUST see this right then, not only if they happen to run `describe` afterwards.
/// Reads the conditions off the PLAN, not off the documents.
///
/// It used to recompute them from the docs with a second `Env::probe()` in the
/// same run, which was already wasteful and became wrong the moment a condition
/// depended on anything the plan knows and a document alone does not. The VM
/// «declared but not applied» warning is exactly that: it only makes sense once
/// the resource exists, so it is attached in `build_plan` where the action is
/// known — and this function silently did not show it, meaning the warning
/// appeared in `stack plan` and vanished in `stack apply`, which is the command
/// most people actually run. One source, both commands.
fn print_missing_conditions(changes: &[Change]) {
    let mut header = false;
    for change in changes {
        for c in &change.conditions {
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
                change.kind, change.name, c.kind, c.reason, c.message
            );
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
/// Whether a declared resource is present on the machine.
///
/// Takes the DOCUMENT and not just the name: an `Image`'s identity is its ref
/// (`spec.pull`/`spec.build.tag`), not `metadata.name`, and resolving the name
/// reported every image as absent unless the document happened to be named after
/// the tag.
fn presence(
    kind: &str,
    doc: &manifest::ManifestDoc,
    containers: &[delonix_runtime_core::Container],
) -> (String, String) {
    let root = super::util::state_root();
    let name = doc.metadata.name.as_str();
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
        "Volume" => match delonix_volume::VolumeStore::open(&root).and_then(|s| s.list()) {
            Ok(vs) => yes_no(vs.iter().any(|v| v.name == name)),
            Err(e) => ("?".into(), e.to_string()),
        },
        "Network" => match delonix_net::NetworkStore::open(&root).and_then(|s| s.list()) {
            Ok(ns) => yes_no(ns.iter().any(|n| n.name == name)),
            Err(e) => ("?".into(), e.to_string()),
        },
        // An image's identity is its REF, never the document name.
        "Image" => match delonix_image::ImageStore::open(&root) {
            Ok(s) => yes_no(
                s.resolve(super::image::image_ref(doc).as_deref().unwrap_or(name))
                    .is_ok(),
            ),
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
        //
        // `NetworkRoute` belongs here for the same reason, and its absence was a
        // real bug: it is in `KINDS` and `stack apply` DOES apply it, but with no
        // arm it fell through to `_ => ("?", "unsupported kind")` — printed by
        // `ls`/`describe` about a resource the apply creates, and treated as
        // never-ready by `wait`. It is realized as one element of the holder's
        // `@netpair` verdict map and `delonix-net` exposes only the write
        // (`infra::network_route`), so there is nothing to read back.
        "Ingress" | "FirewallPolicy" | "HTTPRoute" | "Dependency" | "NetworkRoute" => {
            ("-".into(), super::po::t("declarative").into())
        }
        // A share has a record of its own, keyed by (namespace, name) — the
        // namespace comes from the document, which is why `load_record` takes
        // both and why guessing it is not an option.
        "ShareVolume" => super::sharevolume::presence_of(&root, doc),
        // A tunnel's record says whether an agent was started; the public URL
        // is status and deliberately not part of "is it there".
        "Tunnel" => super::tunnel::presence_of(name),
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

/// How many documents of each Kind the manifest carries — a layer with none is
/// announced as skipped instead of pretending to have done work.
fn count_of_kinds(docs: &[manifest::ManifestDoc]) -> std::collections::BTreeMap<String, usize> {
    let mut m = std::collections::BTreeMap::new();
    for d in docs {
        *m.entry(d.kind.clone()).or_insert(0) += 1;
    }
    m
}

fn apply(file: Option<PathBuf>, replace: Vec<String>, do_prune: bool) -> Result<()> {
    // `--replace` is the flag that AUTHORIZES a destructive recreate, so a value
    // it cannot possibly match is refused here rather than ignored. Accepting
    // `--replace lixo` in silence gives the illusion of having authorised
    // something; the recreate is then refused downstream by `refuse_unallowed`,
    // and the error the user reads talks about the resource, never about the
    // typo they made. `all` and `<Kind>/<name>` are the two shapes that mean
    // anything — see `refuse_unallowed`, which also accepts a bare name and is
    // why one is allowed here too.
    for r in &replace {
        let shape_ok = r == "all"
            || (r.split('/').count() == 2 && r.split('/').all(|p| !p.is_empty()))
            || (!r.contains('/') && !r.is_empty());
        if !shape_ok {
            return Err(delonix_runtime_core::Error::Invalid(super::po::tf(
                "--replace '{value}': expected `<Kind>/<name>` (e.g. `Container/web`), \
                 a bare resource name, or `all`",
                &[("value", r)],
            )));
        }
    }
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
    // A `--replace` that names nothing in this manifest is a typo, and a typo in
    // the flag that authorises a DESTRUCTIVE recreate has to be loud. Without
    // this, `--replace Container/wev` reads as authorised, the recreate is then
    // refused downstream, and the error the user reads names the resource — never
    // the misspelling that caused it.
    if !replace.iter().any(|r| r == "all") {
        for r in &replace {
            let hits = changes
                .iter()
                .any(|c| format!("{}/{}", c.kind, c.name) == *r || c.name == *r);
            if !hits {
                return Err(delonix_runtime_core::Error::Invalid(super::po::tf(
                    "--replace '{value}': no resource with that name in this manifest \
                     (`stack plan` lists them)",
                    &[("value", r)],
                )));
            }
        }
    }
    refuse_unallowed(&changes, &replace)?;
    // A resource that has to be recreated is destroyed FIRST, so the normal
    // creation pass below builds it fresh. Doing it in this order means there is
    // exactly one creation path (the one that has always existed), instead of a
    // second "recreate" path that would drift away from it.
    destroy_for_replace(&changes)?;

    // Secrets first: `Storage.passwordSecret` and `Container.secret` reference them.
    // `base` = the manifest folder, so `fromEnvFile` resolves next to it.
    let base = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    // One LAYER per Kind, in the order below, each announced before it runs and
    // ticked with the time it took — the shape a CI log has, because that is what
    // an apply is: a pipeline whose stages depend on the previous one.
    //
    // No spinner and no fold here, deliberately: each layer prints its own
    // per-resource lines (`container/web: created`), which are the record of what
    // happened. A spinner would fight them for the same row, and folding them
    // would hide the one thing worth keeping. The animation belongs where a step
    // is SILENT for seconds — see `Progress` in `vm build`/`vm create`.
    let mut layers = super::output::Layers::new(count_of_kinds(&docs));
    layers.run("Secret", "🔑", || super::secret::apply(&docs, base))?;
    layers.run("Network", "🌐", || super::network::apply(&docs))?;
    // Logo a seguir às redes: uma rota nomeia DUAS que têm de existir, e nada
    // do que vem abaixo depende dela para ser criado.
    layers.run("NetworkRoute", "🔗", || super::netroute::apply(&docs))?;
    layers.run("Volume", "💽", || super::volume::apply(&docs))?;
    // ShareVolume right after Storage: it carves subdirectories out of an
    // already-mounted Storage, so the parent must exist first.
    layers.run("ShareVolume", "📂", || super::sharevolume::apply(&docs))?;
    layers.run("Image", "📦", || super::image::apply(&docs))?;
    layers.run("Vm", "🖥", || super::vm::apply(&docs, base))?;
    layers.run("Container", "📦", || super::container::apply(&docs))?;
    layers.run("Pod", "🧩", || super::pod::apply(&docs))?;
    layers.run("FirewallPolicy", "🧱", || super::firewall::apply(&docs))?;
    // HTTPRoute LAST: it needs the backend containers already created (with IP) to
    // resolve the routes; brings up/reloads the L7 reverse-proxy.
    layers.run("HTTPRoute", "🔀", || super::httproute::apply(&docs))?;
    // Tunnel LAST of all: its `localPort` is typically the HTTPRoute proxy's own
    // listening port (see `cmd::tunnel`'s module doc) — must already be up.
    layers.run("Tunnel", "🌍", || super::tunnel::apply(&docs))?;
    layers.done();

    // Everything that exists is now created; converge what differs, and stamp
    // ownership + the applied spec on all of it. The stamp is what makes the
    // NEXT plan a three-way diff instead of a two-way one.
    converge_and_stamp(&docs, &stack, &changes)?;

    // Pruning LAST, and only when asked. Removing before converging could pull a
    // network out from under a container this same apply is about to attach to
    // it — the resource is only really unused once everything else has settled.
    if do_prune {
        prune(&changes)?;
    }

    // After creating everything, say what was created but will NOT work as it
    // appears without a host prerequisite (network mount in rootless, etc.) —
    // it is here, in the real creation flow, that the user needs to know it.
    print_missing_conditions(&changes);
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
        destroy_one(&c.kind, &c.name)?;
    }
    Ok(())
}

/// Destroys ONE resource of a converging Kind.
///
/// Deliberately the single place that removes anything: `--replace`, `--prune`
/// and `destroy` all come through here, so there is no chance of three subtly
/// different teardown paths — which is how a resource ends up half-removed in
/// one of them.
fn destroy_one(kind: &str, name: &str) -> Result<()> {
    match kind {
        "Container" => super::container::remove_for_replace(name),
        "Volume" => super::volume::remove_for_replace(name),
        "Network" => super::network::remove_for_replace(name),
        "Pod" => super::pod::remove_pod(name, true),
        "Vm" => super::vm::remove_for_replace(name),
        // A firewall policy cannot be torn down automatically, and saying so is
        // the only honest answer: it has no record of its own (it lives on the
        // target's `ContainerFw`), so when `target`/`direction` change the OLD
        // target is not written down anywhere — the manifest holds the new one.
        // Leaving stale rules on a container nobody is looking at any more is
        // the worst outcome available, so the user is told exactly what to run.
        "FirewallPolicy" => Err(delonix_runtime_core::Error::Invalid(super::po::tf(
            "FirewallPolicy/{name}: changing `target` or `direction` cannot be undone              automatically — the previous target keeps its rules. Clear them by hand with              `delonix net ingress clear <old-target>` (or `net egress clear`), then apply again",
            &[("name", name)],
        ))),
        // `Image` is deliberately absent: it is not ownable (shared content), so
        // it never reaches a prune or a destroy — and a `Replace` of an image is
        // just a pull, handled by `converge`, never by destroying anything.
        other => Err(delonix_runtime_core::Error::Invalid(format!(
            "{other}/{name}: removing this Kind declaratively is not implemented"
        ))),
    }
}

/// The order in which resources are torn down: the REVERSE of the order they
/// are created in.
///
/// Removing a network while containers are still attached to it, or a volume
/// while a container still mounts it, is how a teardown leaves a mess behind.
/// `KINDS` already encodes the dependency order for creation; reversing it is
/// the only correct teardown order, and deriving it instead of writing a second
/// list means the two cannot drift apart.
fn teardown_order(changes: &[Change]) -> Vec<&Change> {
    let mut out: Vec<&Change> = Vec::new();
    for kind in KINDS.iter().rev() {
        out.extend(changes.iter().filter(|c| c.kind == **kind));
    }
    out
}

/// Removes what this stack owns and the manifest no longer declares.
///
/// Never runs unless asked for. An `apply` that deletes without being told to is
/// the failure that destroys trust in an IaC tool, and no amount of correctness
/// elsewhere makes up for it.
fn prune(changes: &[Change]) -> Result<()> {
    let doomed: Vec<&Change> = teardown_order(changes)
        .into_iter()
        .filter(|c| c.action == Action::Delete)
        .collect();
    for c in doomed {
        println!(
            "{}",
            super::po::tf(
                "{kind}/{name}: removing (no longer in the manifest)",
                &[("kind", &c.kind), ("name", &c.name)],
            )
        );
        destroy_one(&c.kind, &c.name)?;
    }
    Ok(())
}

/// `stack destroy` — removes everything this stack owns.
///
/// Ownership comes from the `delonix.io/stack` label, so a resource created by
/// hand, or belonging to another stack, is never touched. That is also why the
/// candidate list is computed against an EMPTY desired set: «nothing is
/// declared any more» is exactly what a destroy means, and it reuses the same
/// planner rather than a second, divergent notion of what belongs to a stack.
fn destroy(file: Option<PathBuf>, name: Option<String>, dry_run: bool) -> Result<()> {
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;
    let stack = stack_name(&path, name.as_deref());
    let changes: Vec<Change> = reconcile::plan(&[], &actual_of(&docs)?, &stack)
        .into_iter()
        .filter(|c| c.action == Action::Delete)
        .collect();
    if changes.is_empty() {
        println!(
            "{}",
            super::po::tf(
                "stack \"{stack}\" owns nothing — nothing to remove",
                &[("stack", &stack)],
            )
        );
        // Deliberately not an error: a destroy of an already-destroyed stack
        // succeeding is what makes it safe to put in a teardown script.
        return Ok(());
    }
    let ordered = teardown_order(&changes);
    if dry_run {
        println!(
            "{}",
            super::po::tf(
                "stack \"{stack}\" would remove {n} resource(s):",
                &[("stack", &stack), ("n", &ordered.len().to_string())],
            )
        );
        for c in ordered {
            println!("  -   {}/{}", c.kind, c.name);
        }
        return Ok(());
    }
    for c in ordered {
        println!(
            "{}",
            super::po::tf(
                "{kind}/{name}: removing",
                &[("kind", &c.kind), ("name", &c.name)],
            )
        );
        destroy_one(&c.kind, &c.name)?;
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
                "Image" => super::image::converge(&c.name, &c.diffs)?,
                // A firewall policy re-applies WHOLE: `apply_fw_doc` already
                // replaces the entire direction, so there is no per-field path
                // to write — and writing one would be a second way to build the
                // same nft chain, which is how two ways start to disagree.
                "FirewallPolicy" => {
                    let doc = docs
                        .iter()
                        .find(|d| d.kind == c.kind && d.metadata.name == c.name)
                        .ok_or_else(|| {
                            delonix_runtime_core::Error::Invalid(format!(
                                "FirewallPolicy/{}: not in the manifest",
                                c.name
                            ))
                        })?;
                    super::firewall::converge_doc(doc)?
                }
                // Same shape as a firewall policy: `apply_one` is already
                // idempotent and updates the record in place, so converging IS
                // applying — a per-field path would be a second way to write the
                // same record.
                // The proxy config is COLLECTIVE: there is no per-document
                // apply to call, so converging one route means recomposing the
                // whole thing — which is what `apply` does, and it SIGHUPs the
                // live proxy instead of restarting it.
                "HTTPRoute" | "Ingress" => super::httproute::converge_all(docs)?,
                "Tunnel" => {
                    let doc = docs
                        .iter()
                        .find(|d| d.kind == c.kind && d.metadata.name == c.name)
                        .ok_or_else(|| {
                            delonix_runtime_core::Error::Invalid(format!(
                                "Tunnel/{}: not in the manifest",
                                c.name
                            ))
                        })?;
                    super::tunnel::converge_doc(doc)?
                }
                "ShareVolume" => {
                    let doc = docs
                        .iter()
                        .find(|d| d.kind == c.kind && d.metadata.name == c.name)
                        .ok_or_else(|| {
                            delonix_runtime_core::Error::Invalid(format!(
                                "ShareVolume/{}: not in the manifest",
                                c.name
                            ))
                        })?;
                    super::sharevolume::converge_doc(doc)?
                }
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
            "Vm" => super::vm::stamp(&d.name, stack, &d.fields),
            // `Image` is shared content and deliberately not ownable — stamping
            // it for one stack would hand another stack's cache an owner.
            "Image" => Ok(()),
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
fn validate(file: Option<PathBuf>, strict: bool) -> Result<()> {
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;
    let issues = validate_graph(&docs);
    // Fields the load has just warned about. Saying `OK` on the line after
    // `unknown field 'resources.memoria' — ignored` was the engine contradicting
    // itself inside two lines: the reference graph WAS fine, and the manifest
    // still did not mean what it says. The count makes the verdict match what
    // was printed; `--strict` turns it into an exit code for a pipeline.
    let ignored = manifest::unknown_field_warnings();
    if issues.is_empty() {
        if ignored == 0 {
            println!(
                "{}",
                super::po::tf(
                    "stack validate: OK — {n} document(s), all references resolved",
                    &[("n", &docs.len().to_string())],
                )
            );
            return Ok(());
        }
        println!(
            "{}",
            super::po::tf(
                "stack validate: {n} document(s), all references resolved — but {w} field(s) were ignored (see the warnings above)",
                &[("n", &docs.len().to_string()), ("w", &ignored.to_string())],
            )
        );
        if strict {
            return Err(delonix_runtime_core::Error::Invalid(super::po::tf(
                "{w} ignored field(s) — refused by --strict",
                &[("w", &ignored.to_string())],
            )));
        }
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

    // Uma rota nomeia DUAS redes, e nomear uma que não existe é o mesmo engano
    // que o `vm init` cometia: um manifesto que se recusa a si próprio. Ambas
    // as pontas são verificadas, e a mensagem diz QUAL delas falta.
    for doc in docs.iter().filter(|d| d.kind == "NetworkRoute") {
        if let Ok(spec) = manifest::spec_of::<super::netroute::NetworkRouteSpec>(doc) {
            for (lado, rede) in [("from", &spec.from), ("to", &spec.to)] {
                if !networks.contains(rede) {
                    issues.push(super::po::tf(
                        "NetworkRoute '{name}' → {side} network '{net}' is not declared nor does it exist",
                        &[
                            ("name", &doc.metadata.name),
                            ("side", lado),
                            ("net", rede),
                        ],
                    ));
                }
            }
        }
    }

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
            "Volume" => {
                // A network share's `passwordSecret` references a Secret (the mount
                // reads that Secret's `password` key — `storage::resolve_password`).
                //
                // Read from INSIDE the block: `kind: Storage` folded into
                // `kind: Volume`, so the field moved from `spec.passwordSecret` to
                // `spec.<nfs|cifs|webdav>.passwordSecret`. Looking at the old place
                // would silently stop validating it — a check that quietly stops
                // running is worse than one that was never written.
                let sref = ["nfs", "cifs", "webdav"].iter().find_map(|b| {
                    doc.spec
                        .get(b)
                        .and_then(|v| v.get("passwordSecret"))
                        .and_then(|v| v.as_str())
                });
                if let Some(sref) = sref {
                    if !secrets.contains(sref) {
                        issues.push(super::po::tf(
                            "Volume '{name}' → passwordSecret '{sref}' is not a declared or existing Secret",
                            &[("name", name), ("sref", sref)],
                        ));
                    } else if let Some(Some(keys)) = declared_secret_keys.get(sref) {
                        // Only when we know the keys (inline Secret without fromEnvFile):
                        // then we can assert with certainty that `password` is missing.
                        if !keys.contains("password") {
                            issues.push(super::po::tf(
                                "Volume '{name}' → passwordSecret '{sref}': the Secret does not declare the 'password' key (the mount reads exactly that key)",
                                &[("name", name), ("sref", sref)],
                            ));
                        }
                    }
                }
            }
            "FirewallPolicy" => {
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
    // Two firewall policies claiming the SAME (target, direction) is silent data
    // loss on a security surface: `apply_fw_doc` REPLACES the rules of a
    // direction, so the second document wipes the first's rules while both
    // report success. Measured: `stack validate` said "OK — all references
    // resolved" for exactly that manifest.
    //
    // Refused rather than merged, unlike `kind: Dependency` (which merges by
    // target when it lowers). A Dependency states one peer's access and several
    // of them plainly add up; a FirewallPolicy states the WHOLE desired state of
    // a direction — `defaultPolicy` included — so two of them are two different
    // answers to the same question, and on a firewall a contradiction must not
    // be resolved by document order.
    let mut seen: std::collections::HashMap<(String, String, String), String> = Default::default();
    for doc in docs.iter().filter(|d| d.kind == "FirewallPolicy") {
        let get = |k: &str, dflt: &str| {
            doc.spec
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or(dflt)
                .to_string()
        };
        let key = (
            get("target", ""),
            get("direction", ""),
            get("scope", "container"),
        );
        if key.0.is_empty() || key.1.is_empty() {
            continue; // already reported by the checks above
        }
        match seen.get(&key) {
            Some(first) => issues.push(super::po::tf(
                "FirewallPolicy '{name}' and '{first}' both define {direction} for '{target}' — \
                 a policy is the WHOLE desired state of a direction, so the second would \
                 silently replace the first; merge them into one document",
                &[
                    ("name", &doc.metadata.name),
                    ("first", first),
                    ("direction", &key.1),
                    ("target", &key.0),
                ],
            )),
            None => {
                seen.insert(key, doc.metadata.name.clone());
            }
        }
    }

    issues
}

/// Handles the `init` of this group (see `cmd::scaffold`).
/// The generator behind `stack init`/`vm init`, exposed so `delonix init` can dispatch to
/// it after DETECTING which one the directory calls for (`cmd::init`) instead of copying it.
pub(crate) fn init_for(
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

    /// Teardown must be the REVERSE of creation: pulling a network out from
    /// under the containers still attached to it, or a volume from under a
    /// container that mounts it, is how a destroy leaves a mess behind.
    ///
    /// The order is DERIVED from `KINDS` rather than written down a second
    /// time, so the two cannot drift apart — this test is what fixes that
    /// property, not just the current ordering.
    #[test]
    fn a_ordem_de_remocao_e_a_inversa_da_de_criacao() {
        let mk = |kind: &str| super::reconcile::Change {
            kind: kind.to_string(),
            name: "x".into(),
            action: super::reconcile::Action::Delete,
            reason: None,
            cold_fields: vec![],
            owner: None,
            conditions: vec![],
            diffs: vec![],
            changed: true,
        };
        // Deliberately fed in creation order, to prove the function reorders.
        let changes: Vec<_> = ["Network", "Volume", "Container", "Pod"]
            .iter()
            .map(|k| mk(k))
            .collect();
        let order: Vec<&str> = super::teardown_order(&changes)
            .iter()
            .map(|c| c.kind.as_str())
            .collect();
        assert_eq!(order, vec!["Pod", "Container", "Volume", "Network"]);

        // And it really is derived: every ordered kind keeps the relative
        // position `KINDS` gives it, reversed.
        let idx = |k: &str| super::KINDS.iter().position(|x| *x == k).unwrap();
        assert!(idx("Network") < idx("Volume"));
        assert!(idx("Volume") < idx("Container"));
        assert!(idx("Container") < idx("Pod"));
    }

    /// **Perda silenciosa numa superfície de segurança.** O `apply_fw_doc`
    /// SUBSTITUI as regras de uma direcção, por isso duas políticas para o mesmo
    /// alvo+direcção fazem a segunda apagar as regras da primeira — e ambas
    /// reportavam sucesso. Medido: o `stack validate` dizia «OK, todas as
    /// referências resolvidas» para exactamente esse manifesto.
    ///
    /// Recusado e não fundido, ao contrário do `kind: Dependency`: uma
    /// Dependency declara o acesso de UM peer e várias somam-se; uma
    /// FirewallPolicy declara o estado desejado INTEIRO de uma direcção,
    /// `defaultPolicy` incluído, logo duas são duas respostas à mesma pergunta.
    #[test]
    fn duas_politicas_para_o_mesmo_alvo_e_direccao_sao_recusadas() {
        let mk = |name: &str, dir: &str| -> super::manifest::ManifestDoc {
            serde_yaml::from_str(&format!(
                "apiVersion: delonix.io/v1\nkind: FirewallPolicy\nmetadata: {{ name: {name} }}\nspec: {{ direction: {dir}, target: db }}\n"
            ))
            .unwrap()
        };
        let dois = vec![mk("a", "ingress"), mk("b", "ingress")];
        let issues = super::validate_graph_with(&dois, &[], &[], &["db".into()], &[]);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(
            issues[0].contains("ingress") && issues[0].contains("db"),
            "{issues:?}"
        );

        // Uma por DIRECÇÃO não colide — são estados desejados de coisas
        // diferentes, e é assim que se escreve uma política completa.
        let por_direccao = vec![mk("a", "ingress"), mk("b", "egress")];
        assert!(
            super::validate_graph_with(&por_direccao, &[], &[], &["db".into()], &[]).is_empty()
        );
    }

    /// **Três listas que têm de concordar, e que derivaram.** O
    /// `CONVERGING_KINDS` decide TRÊS coisas — se o `actual_of` sonda a presença
    /// em vez de usar o adaptador, se o `converge_and_stamp` aplica um Update, e
    /// se carimba a posse — enquanto os braços do `match` em `desired_of` e a
    /// tabela do `--fields` são escritos à parte.
    ///
    /// Aconteceu mesmo: o `Vm`, o `FirewallPolicy` e o `ShareVolume` ganharam
    /// adaptador e ficaram fora da constante, por isso o `converge_and_stamp`
    /// SALTAVA-OS. O sintoma escondeu-se porque o `apply` de cada Kind continua a
    /// correr na cadeia antiga e é idempotente — convergiam pelo caminho errado,
    /// e o resultado observado parecia certo.
    #[test]
    fn as_tres_listas_de_kinds_convergentes_concordam() {
        // Tudo o que a constante declara tem de ter uma linha no `--fields`...
        let com_campos: Vec<&str> = compared_fields_table()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        for k in super::CONVERGING_KINDS {
            assert!(
                com_campos.contains(&k),
                "{k} converge mas o `--fields` não diz que campos compara"
            );
        }
        // ...e o inverso: nada no `--fields` pode estar fora da constante, senão
        // a tabela promete uma comparação que o apply nunca faz.
        for k in com_campos {
            assert!(
                super::CONVERGING_KINDS.contains(&k),
                "o `--fields` lista {k} mas o converge_and_stamp salta-o"
            );
        }
        // E um Kind convergente nunca pode ter uma desculpa de não-convergência.
        for k in super::CONVERGING_KINDS {
            assert_eq!(
                super::not_converged_reason(k),
                "not converged in this version",
                "{k} converge e ainda assim tem uma razão para não convergir"
            );
        }
    }

    /// Os Kinds de `KINDS` que não têm presença observável — o `presence`
    /// devolve-lhes `-`, e é essa a marca que o `wait` tem de ler como pronta.
    const DECLARATIVOS: [&str; 4] = ["Ingress", "FirewallPolicy", "HTTPRoute", "NetworkRoute"];

    /// **Um marcador de presença `-` não é «ausente».** O `wait` decidia
    /// prontidão com `present == "yes"`, e os Kinds declarativos NUNCA dizem
    /// `"yes"` — qualquer manifesto com um deles esgotava o `--timeout` inteiro
    /// e saía com erro sobre uma stack inteiramente a correr.
    ///
    /// O `"?"` continua pendente de propósito: é também o que um erro de store
    /// devolve, e dar «pronto» a um desconhecido é o mesmo defeito ao contrário.
    #[test]
    fn um_kind_declarativo_nao_fica_pendente_para_sempre() {
        for k in DECLARATIVOS {
            assert!(
                !super::is_pending("-", k, "declarative"),
                "{k} é declarativo e ficaria pendente para sempre"
            );
        }
        // Ausente continua pendente — é o que o `wait` existe para esperar.
        assert!(super::is_pending("no", "Container", "-"));
        assert!(super::is_pending("no", "Vm", "-"));
        // Presente é julgado pelo estado, como antes.
        assert!(!super::is_pending("yes", "Container", "Running"));
        assert!(super::is_pending("yes", "Container", "Exited"));
        assert!(super::is_pending("yes", "Container", "unhealthy"));
        assert!(!super::is_pending("yes", "Volume", "-"));
        // Desconhecido (store ilegível) NÃO é pronto.
        assert!(super::is_pending(
            "?",
            "Volume",
            "permission denied: /var/lib/delonix/volumes"
        ));
    }

    /// Um Kind que o `apply` aplica tem de ter braço no `presence()`, senão cai
    /// no `_ => ("?", "unsupported kind")` — que o `ls`/`describe` imprimem e o
    /// `wait` conta como pendente para sempre. Aconteceu com o `NetworkRoute`,
    /// que está em `KINDS` e é aplicado desde que existe.
    ///
    /// Sem I/O: `util::state_root()` só constrói um `PathBuf` e estes braços
    /// nunca abrem store nenhum.
    #[test]
    fn todo_o_kind_declarativo_de_kinds_tem_braco_no_presence() {
        let d = docs("apiVersion: delonix.io/v1\nkind: Network\nmetadata:\n  name: n1\nspec: {}\n");
        let doc = &d[0];
        for k in DECLARATIVOS {
            assert!(
                super::KINDS.contains(&k),
                "{k} saiu de KINDS — este teste deixou de dizer o que promete"
            );
            let (present, status) = super::presence(k, doc, &[]);
            assert_eq!(present, "-", "{k}: presence devolveu {present}/{status}");
        }
    }
}
