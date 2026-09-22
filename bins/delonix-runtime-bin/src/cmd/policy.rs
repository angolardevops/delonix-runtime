//! Node-level runtime policy — a ceiling this host puts on what may run.
//!
//! # Why in the runtime and not only in an admission controller
//!
//! The same reasoning that put the capability ceiling in the CRI
//! (`DELONIX_CRI_CAP_CEILING`): everything reaching `cmd_run` has already been
//! authorised by whoever called it. A policy that lives only in a cluster's
//! admission chain runs in another process, on another machine, and this node
//! cannot see or verify its configuration.
//!
//! This is the local answer. It holds with Pod Security misconfigured, with a
//! `crictl` talking straight to the socket, and with somebody typing
//! `delonix container run --privileged` by hand.
//!
//! # Fail-closed, and loudly
//!
//! A policy that cannot be READ is not an absent policy — a truncated or
//! malformed file means somebody's intent is unknown, and running the workload
//! anyway is the silent degradation this engine refuses everywhere else.
//!
//! Absent, on the other hand, means absent: no file, no ceiling, byte-for-byte
//! the behaviour this engine has always had. Turning a missing file into a
//! default-deny would break every existing host on upgrade.
//!
//! # What moved, and what stayed
//!
//! The decision itself now lives in `delonix-security-runtime`, so the VM path
//! can reach the same gate the container path already did (ADR-0026). This file
//! is what is left over and cannot move: reading the file, rendering a refusal
//! in the operator's language, and recording the event.
//!
//! # `kind: RuntimePolicy` — the same ceiling, given a manifest form
//!
//! Until this, `policy.json` only ever existed as a file someone wrote by hand
//! or shipped with a golden image — no `stack apply` could raise or tighten it,
//! and a fleet of nodes had no declarative way to converge on one ceiling.
//!
//! **Reuses [`SecurityPolicy`] itself, not a parallel copy.** `RuntimePolicySpec`
//! below exists ONLY because `SecurityPolicy` cannot derive `schemars::JsonSchema`
//! — `delonix-security-runtime`'s `Cargo.toml` fixes its dependency count at
//! three, on purpose (ADR-0026: this crate decides on the highest-privilege path
//! of the runtime, and every dependency it grows is supply-chain surface there).
//! So the manifest-facing struct is a field-for-field mirror, and
//! [`to_security_policy`] turns one into the real, engine-side
//! [`SecurityPolicy`] by direct construction — no second validator, no
//! serialize-then-reparse. `enforce`/`admission::evaluate`/`.lint()` all run on
//! the SAME type regardless of whether it came from `policy.json` on disk or
//! from a manifest, so the two can never disagree about what a rule means.
//!
//! **Converges, but never tears down.** `stack apply` can raise or tighten the
//! ceiling like any other field-converging Kind (see `hot_fields` in
//! `delonix-stack::reconcile` — every field here is hot, since writing
//! `policy.json` needs no restart of anything). What it must NEVER do is lower
//! it: `stack destroy`/`apply --prune` simply not mentioning `kind:
//! RuntimePolicy` any more must not silently reopen the node. So `teardown:
//! false` in the Kind table, `ownable: false` here (there is nothing to carve
//! out per-stack — the file is a node-wide singleton, not owned content), and
//! the only way down is the imperative [`PolicyCmd::Unset`].

use delonix_model::{Error, Result};
use delonix_security_runtime as srt;
use srt::admission::{Decision, Violation, Workload};
use srt::policy::SecurityPolicy;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::kinds as k;
use super::manifest::{self, ManifestDoc};
use super::output::OutputFormat;
use clap::Subcommand;
use serde::{Deserialize, Serialize};

pub(crate) use srt::admission::Request;

/// `<root>/policy.json`.
pub(crate) fn path(root: &Path) -> PathBuf {
    root.join("policy.json")
}

/// Reads the node policy. `Ok(None)` when there is none.
///
/// A file that exists and cannot be parsed is an ERROR, not a missing policy —
/// see the module doc.
pub(crate) fn load(root: &Path) -> Result<Option<SecurityPolicy>> {
    let p = path(root);
    let raw = match std::fs::read_to_string(&p) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::Invalid(super::po::tf(
                "runtime policy {path}: {err} — a policy that cannot be read is not an \
                 absent policy",
                &[("path", &p.display().to_string()), ("err", &e.to_string())],
            )))
        }
    };
    SecurityPolicy::parse(&raw).map(Some).map_err(|e| {
        Error::Invalid(super::po::tf(
            "runtime policy {path}: {err}",
            &[("path", &p.display().to_string()), ("err", &e.to_string())],
        ))
    })
}

/// One refusal, in the operator's language.
///
/// The first four `msgid`s are byte-identical to the ones that shipped in
/// v0.69.0 — changing the English would orphan the Portuguese catalog entry and
/// silently drop the translation.
fn render(v: &Violation) -> String {
    match v {
        Violation::Privileged => {
            super::po::t("--privileged is refused by this node's runtime policy").to_string()
        }
        Violation::HostNetwork => {
            super::po::t("--net host is refused by this node's runtime policy").to_string()
        }
        Violation::LatestTag { image } => super::po::tf(
            "image '{image}': this node's runtime policy refuses `:latest` and untagged \
             references — pin a version or a digest",
            &[("image", image)],
        ),
        Violation::Registry {
            image,
            host,
            allowed,
        } => super::po::tf(
            "image '{image}': registry '{host}' is not in this node's allowed list ({allowed})",
            &[
                ("image", image),
                ("host", host),
                ("allowed", &allowed.join(", ")),
            ],
        ),
        Violation::DevicePassthrough { devices } => super::po::tf(
            "device passthrough ({devices}) is refused by this node's runtime policy — a \
             passed-through device gives the guest DMA to host hardware",
            &[("devices", &devices.join(", "))],
        ),
        Violation::LatestVmImage { image } => super::po::tf(
            "VM image '{image}': this node's runtime policy refuses `:latest` and untagged \
             references — pin a version",
            &[("image", image)],
        ),
        Violation::ImageUrlHost { url, host, allowed } => super::po::tf(
            "boot image '{url}': host '{host}' is not in this node's allowed list ({allowed})",
            &[
                ("url", url),
                ("host", host),
                ("allowed", &allowed.join(", ")),
            ],
        ),
    }
}

/// Shows, at most once per process, the gaps the operator left open on the path
/// they are using.
///
/// Once per process is exactly once per command — this engine is daemonless, so
/// a CLI invocation is a process that is born, works and dies. Silenceable with
/// `DELONIX_POLICY_LINT=0` for anyone who has read it and decided otherwise.
fn show_lints(p: &SecurityPolicy, w: Workload) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static SHOWN: AtomicBool = AtomicBool::new(false);

    if std::env::var("DELONIX_POLICY_LINT").as_deref() == Ok("0") {
        return;
    }
    if SHOWN.swap(true, Ordering::Relaxed) {
        return;
    }
    for l in p.lint().iter().filter(|l| l.applies_to(w)) {
        eprintln!(
            "{} [{}] {}",
            super::po::t("warning: runtime policy"),
            l.id,
            super::po::t(l.message)
        );
    }
}

/// Refuses the request when the node policy says so. No policy = no ceiling.
///
/// Under `mode: warn` the request proceeds and the reasons are printed — the
/// distinction between «refused» and «allowed, and here is what would have
/// refused it» is one an operator must never have to infer.
pub(crate) fn enforce(root: &Path, resource: &str, r: &Request<'_>) -> Result<()> {
    let Some(p) = load(root)? else {
        return Ok(());
    };
    show_lints(&p, r.workload);

    let decision = srt::admission::evaluate(&p, r);

    // The event carries the refusal into the engine's log whether or not the
    // request was stopped, so a `mode: warn` rollout leaves a trail to count.
    for ev in srt::SecurityEvent::from_decision(&decision, r, resource) {
        ev.emit(root);
    }

    match decision {
        Decision::Allow => Ok(()),
        Decision::AllowWithWarnings(v) => {
            for line in v.iter().map(render) {
                eprintln!(
                    "{} {line}",
                    super::po::t("warning: runtime policy would refuse:")
                );
            }
            Ok(())
        }
        Decision::Deny(v) => Err(Error::Invalid(format!(
            "{}\n  {}",
            super::po::tf(
                "refused by this node's runtime policy ({path}):",
                &[("path", &path(root).display().to_string())],
            ),
            v.iter().map(render).collect::<Vec<_>>().join("\n  ")
        ))),
    }
}

// ---------------------------------------------------------------------------
// `kind: RuntimePolicy`
// ---------------------------------------------------------------------------

/// `spec` of `kind: RuntimePolicy` — a field-for-field mirror of
/// [`SecurityPolicy`]; see the module doc for why it is a separate type.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePolicySpec {
    /// `enforce` (refuse) or `warn` (allow and report). Omitted = `enforce`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Refuse `--privileged`.
    #[serde(default)]
    pub deny_privileged: bool,
    /// Refuse `--net host`, this engine's default network mode.
    #[serde(default)]
    pub deny_host_network: bool,
    /// Refuse a container image reference with no tag or with `:latest`.
    #[serde(default)]
    pub deny_latest_tag: bool,
    /// Only these registries may be pulled from. Empty = no opinion.
    #[serde(default)]
    pub allowed_registries: Vec<String>,
    /// Refuse `vm create --device` (VFIO PCI passthrough).
    #[serde(default)]
    pub deny_device_passthrough: bool,
    /// Refuse a VM disk image reference with no tag or with `:latest`.
    #[serde(default)]
    pub deny_latest_vm_image: bool,
    /// Hosts a `vm create --url-img` qcow2 may be fetched from. Empty = no opinion.
    #[serde(default)]
    pub allowed_image_url_hosts: Vec<String>,
}

/// Names accepted in the `spec` of `kind: RuntimePolicy` (unknown-field warning,
/// schema) — and also what the reconciler compares, since every field here
/// converges live (rewriting `policy.json` needs no restart of anything).
pub const RUNTIME_POLICY_SPEC_FIELDS: &[&str] = &[
    "mode",
    "denyPrivileged",
    "denyHostNetwork",
    "denyLatestTag",
    "allowedRegistries",
    "denyDevicePassthrough",
    "denyLatestVmImage",
    "allowedImageUrlHosts",
];
pub const RECONCILED_RUNTIME_POLICY_FIELDS: &[&str] = RUNTIME_POLICY_SPEC_FIELDS;

/// Turns the manifest-facing mirror into the real, engine-side
/// [`SecurityPolicy`] — direct field construction, not a second validator.
pub(crate) fn to_security_policy(spec: &RuntimePolicySpec) -> Result<SecurityPolicy> {
    let mode = match spec.mode.as_deref() {
        None | Some("enforce") => srt::policy::Mode::Enforce,
        Some("warn") => srt::policy::Mode::Warn,
        Some(other) => {
            return Err(Error::Invalid(super::po::tf(
                "RuntimePolicy: mode '{value}' is neither 'enforce' nor 'warn'",
                &[("value", other)],
            )))
        }
    };
    Ok(SecurityPolicy {
        mode,
        deny_privileged: spec.deny_privileged,
        deny_host_network: spec.deny_host_network,
        deny_latest_tag: spec.deny_latest_tag,
        allowed_registries: spec.allowed_registries.clone(),
        deny_device_passthrough: spec.deny_device_passthrough,
        deny_latest_vm_image: spec.deny_latest_vm_image,
        allowed_image_url_hosts: spec.allowed_image_url_hosts.clone(),
    })
}

fn mode_label(m: srt::policy::Mode) -> &'static str {
    match m {
        srt::policy::Mode::Enforce => "enforce",
        srt::policy::Mode::Warn => "warn",
    }
}

/// A boolean field rendered the way this table reads best: what the policy
/// DENIES, not a bare `true`/`false` a reader has to map back onto the name.
fn deny_flag(b: bool) -> &'static str {
    if b {
        "deny"
    } else {
        "-"
    }
}

fn sorted_joined(v: &[String]) -> String {
    let mut v = v.to_vec();
    v.sort();
    v.join(",")
}

/// Manifest-named field → normalized value, read by BOTH `desired` and
/// `actual` so the diff never compares apples to oranges. `mode`/booleans/lists
/// are all rendered the same way regardless of which side produced them.
fn fields_of(p: &SecurityPolicy) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("mode".into(), mode_label(p.mode).into());
    f.insert("denyPrivileged".into(), p.deny_privileged.to_string());
    f.insert("denyHostNetwork".into(), p.deny_host_network.to_string());
    f.insert("denyLatestTag".into(), p.deny_latest_tag.to_string());
    f.insert(
        "allowedRegistries".into(),
        sorted_joined(&p.allowed_registries),
    );
    f.insert(
        "denyDevicePassthrough".into(),
        p.deny_device_passthrough.to_string(),
    );
    f.insert(
        "denyLatestVmImage".into(),
        p.deny_latest_vm_image.to_string(),
    );
    f.insert(
        "allowedImageUrlHosts".into(),
        sorted_joined(&p.allowed_image_url_hosts),
    );
    f
}

/// Writes the policy to `<root>/policy.json` atomically — the same file
/// [`load`]/[`enforce`] read, so a `stack apply` takes effect on the very next
/// `container run`/`vm create`, including ones later in the SAME apply (this
/// Kind's layer runs first — see `run_layers` in `cmd/stack.rs`).
fn write(root: &Path, p: &SecurityPolicy) -> Result<()> {
    let bytes =
        serde_json::to_vec_pretty(p).map_err(|e| Error::Invalid(format!("runtime policy: {e}")))?;
    delonix_state::write_atomic(&path(root), &bytes)
        .map_err(|e| Error::Invalid(format!("runtime policy: {e}")))
}

fn apply_one(doc: &ManifestDoc) -> Result<()> {
    let root = super::util::state_root();
    let spec: RuntimePolicySpec = manifest::spec_of(doc)?;
    let policy = to_security_policy(&spec)?;
    write(&root, &policy)?;
    println!(
        "{}",
        super::po::tf(
            "RuntimePolicy/{name}: node security policy updated ({path})",
            &[
                ("name", &doc.metadata.name),
                ("path", &path(&root).display().to_string()),
            ],
        )
    );
    Ok(())
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, k::RUNTIME_POLICY) {
        apply_one(doc)?;
    }
    Ok(())
}

/// `stack apply`'s live-update path for an `Action::Update` — applying a
/// `RuntimePolicy` document already fully overwrites `policy.json`, the same
/// shape `FirewallPolicy`/`IPPool`/`Service` use: a separate per-field path
/// would be a second way to write the same file.
pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    apply_one(doc)
}

/// What the manifest declares, for the reconciler.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: RuntimePolicySpec = manifest::spec_of(doc)?;
    let policy = to_security_policy(&spec)?;
    Ok(super::reconcile::Desired {
        kind: k::RUNTIME_POLICY.into(),
        name: doc.metadata.name.clone(),
        fields: fields_of(&policy),
        converges: true,
        // No record of its own to carve out per stack — `policy.json` is a
        // node-wide singleton, not content a stack owns. See the module doc
        // and `no_teardown_reason` (`cmd/stack.rs`).
        ownable: false,
    })
}

/// What is actually on the machine, for the reconciler. Absent (no
/// `policy.json`) means no [`super::reconcile::Actual`] entry at all — that is
/// what makes the very first `stack apply` of a `RuntimePolicy` document plan
/// as `Create` rather than `Update`.
pub(crate) fn actual(docs: &[ManifestDoc]) -> Result<Vec<super::reconcile::Actual>> {
    let root = super::util::state_root();
    let mut out = Vec::new();
    for doc in manifest::of_kind(docs, k::RUNTIME_POLICY) {
        let Some(current) = load(&root)? else {
            continue;
        };
        out.push(super::reconcile::Actual {
            kind: k::RUNTIME_POLICY.into(),
            name: doc.metadata.name.clone(),
            fields: fields_of(&current),
            owner: None,
            last_applied: None,
        });
    }
    Ok(out)
}

/// `(present, status)` for `stack ls`/`describe`/`wait` — a singleton, so
/// there is nothing to key on besides the file itself existing.
pub(crate) fn presence_of() -> (String, String) {
    let root = super::util::state_root();
    match load(&root) {
        Ok(Some(p)) => (
            "yes".into(),
            super::po::tf("mode={mode}", &[("mode", mode_label(p.mode))]),
        ),
        Ok(None) => ("no".into(), "-".into()),
        Err(e) => ("?".into(), e.to_string()),
    }
}

/// Dry-run: the spec with defaults materialized (`stack apply --dry-run`).
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: RuntimePolicySpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

#[derive(Serialize)]
struct LsRow {
    mode: String,
    #[serde(rename = "denyPrivileged")]
    deny_privileged: bool,
    #[serde(rename = "denyHostNetwork")]
    deny_host_network: bool,
    #[serde(rename = "denyLatestTag")]
    deny_latest_tag: bool,
    #[serde(rename = "allowedRegistries")]
    allowed_registries: Vec<String>,
    #[serde(rename = "denyDevicePassthrough")]
    deny_device_passthrough: bool,
    #[serde(rename = "denyLatestVmImage")]
    deny_latest_vm_image: bool,
    #[serde(rename = "allowedImageUrlHosts")]
    allowed_image_url_hosts: Vec<String>,
    path: String,
}

fn ls_row(root: &Path, p: &SecurityPolicy) -> LsRow {
    LsRow {
        mode: mode_label(p.mode).to_string(),
        deny_privileged: p.deny_privileged,
        deny_host_network: p.deny_host_network,
        deny_latest_tag: p.deny_latest_tag,
        allowed_registries: p.allowed_registries.clone(),
        deny_device_passthrough: p.deny_device_passthrough,
        deny_latest_vm_image: p.deny_latest_vm_image,
        allowed_image_url_hosts: p.allowed_image_url_hosts.clone(),
        path: path(root).display().to_string(),
    }
}

/// `delonix get runtimepolicies`.
pub(crate) fn cmd_ls(format: OutputFormat) -> Result<()> {
    let root = super::util::state_root();
    let format = super::config::resolve_output(&root, format);
    let current = load(&root)?;
    if format == OutputFormat::Json {
        let rows: Vec<LsRow> = current.iter().map(|p| ls_row(&root, p)).collect();
        return super::output::print_json(&rows);
    }
    let mut t = super::output::Table::new(&[
        "MODE",
        "PRIVILEGED",
        "HOST-NET",
        "LATEST-TAG",
        "REGISTRIES",
        "PASSTHROUGH",
        "LATEST-VM",
        "URL-HOSTS",
    ]);
    if let Some(p) = &current {
        t.row(vec![
            mode_label(p.mode).to_string(),
            deny_flag(p.deny_privileged).to_string(),
            deny_flag(p.deny_host_network).to_string(),
            deny_flag(p.deny_latest_tag).to_string(),
            list_or_dash(&p.allowed_registries),
            deny_flag(p.deny_device_passthrough).to_string(),
            deny_flag(p.deny_latest_vm_image).to_string(),
            list_or_dash(&p.allowed_image_url_hosts),
        ]);
    }
    t.print();
    if current.is_none() {
        println!(
            "{}",
            super::po::t("no runtime policy is set — this node refuses nothing by policy")
        );
    }
    Ok(())
}

fn list_or_dash(v: &[String]) -> String {
    if v.is_empty() {
        "-".into()
    } else {
        v.join(",")
    }
}

/// `delonix describe runtimepolicy <name>`. A singleton addressed through a
/// manifest name that does not change what is shown — every name describes
/// the same `policy.json`, printed once per name given (same contract every
/// other `describe` gives: one block per name on the command line).
pub(crate) fn cmd_describe(names: &[String]) -> Result<()> {
    let root = super::util::state_root();
    let current = load(&root)?;
    for name in names {
        let mut d = super::output::Describe::new();
        d.field("Name", name);
        d.field("File", path(&root).display().to_string());
        match &current {
            None => {
                d.field("Present", super::po::t("no"));
            }
            Some(p) => {
                d.field("Present", super::po::t("yes"));
                d.field("Mode", mode_label(p.mode));
                // Short keys, deliberately: `Describe::field` pads to a fixed
                // column (see `KEY_W`), and a key at or past that width glues
                // straight onto its value with no gap — `deny_flag` already
                // says "deny"/"-", so dropping the "Deny " prefix here loses
                // nothing.
                d.field("Privileged", deny_flag(p.deny_privileged));
                d.field("Host network", deny_flag(p.deny_host_network));
                d.field("Latest tag", deny_flag(p.deny_latest_tag));
                d.list("Registries", &p.allowed_registries);
                d.field("Passthrough", deny_flag(p.deny_device_passthrough));
                d.field("Latest VM tag", deny_flag(p.deny_latest_vm_image));
                d.list("URL hosts", &p.allowed_image_url_hosts);
            }
        }
        d.print();
        println!();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `delonix policy unset` — the only way the ceiling comes down
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum PolicyCmd {
    /// Remove the node's security ceiling (`<root>/policy.json`).
    ///
    /// The ONLY way it comes down: `stack apply --prune`/`stack destroy` never
    /// touch it, even when the manifest stops declaring `kind: RuntimePolicy` —
    /// see the module doc. Asks for confirmation on a terminal; `--force` for
    /// scripts.
    Unset {
        /// Skip the confirmation prompt (REQUIRED when stdin is not a terminal).
        #[arg(short = 'f', long)]
        force: bool,
    },
}

pub fn run(cmd: PolicyCmd) -> Result<()> {
    match cmd {
        PolicyCmd::Unset { force } => cmd_unset(force),
    }
}

/// The settings a policy carries, as one line — shared by the confirmation
/// preview and the final "removed" message, so `--force` (which skips the
/// interactive preview entirely, same as every other `prune::confirm` caller
/// in this CLI) still leaves a printed record of what was there before it was
/// deleted. Deleting a security ceiling is never allowed to be silent about
/// what it was.
fn summarize(p: &SecurityPolicy) -> String {
    super::po::tf(
        "mode={mode} denyPrivileged={priv} denyHostNetwork={net} denyLatestTag={tag} \
         allowedRegistries=[{regs}] denyDevicePassthrough={pass} denyLatestVmImage={vmtag} \
         allowedImageUrlHosts=[{hosts}]",
        &[
            ("mode", mode_label(p.mode)),
            ("priv", &p.deny_privileged.to_string()),
            ("net", &p.deny_host_network.to_string()),
            ("tag", &p.deny_latest_tag.to_string()),
            ("regs", &p.allowed_registries.join(", ")),
            ("pass", &p.deny_device_passthrough.to_string()),
            ("vmtag", &p.deny_latest_vm_image.to_string()),
            ("hosts", &p.allowed_image_url_hosts.join(", ")),
        ],
    )
}

fn cmd_unset(force: bool) -> Result<()> {
    let root = super::util::state_root();
    let p = path(&root);
    let Some(current) = load(&root)? else {
        println!(
            "{}",
            super::po::t("no runtime policy is set — nothing to remove")
        );
        return Ok(());
    };
    let summary = summarize(&current);
    let preview = super::po::tf(
        "about to remove {path} — {summary}",
        &[("path", &p.display().to_string()), ("summary", &summary)],
    );
    if !super::prune::confirm(
        force,
        super::po::t(
            "`policy unset` removes the node's security ceiling — pass --force to confirm \
             when not on a terminal",
        ),
        Some(preview),
        super::po::t(
            "This removes every rule the node currently enforces at admission. Continue? [y/N]",
        ),
    )? {
        return Ok(());
    }
    std::fs::remove_file(&p).map_err(|e| {
        Error::Invalid(super::po::tf(
            "could not remove {path}: {err}",
            &[("path", &p.display().to_string()), ("err", &e.to_string())],
        ))
    })?;
    // Printed even under `--force`, which skips the interactive preview
    // above: the ONE thing that must never be true of removing a security
    // ceiling is that nobody can see afterwards what it used to refuse.
    println!(
        "{}",
        super::po::tf(
            "{path}: removed ({summary}) — this node no longer refuses anything by policy",
            &[("path", &p.display().to_string()), ("summary", &summary)],
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dlx-pol-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A file that exists and does not parse is an ERROR. Treating it as «no
    /// policy» would let a typo silently disable the node's ceiling.
    #[test]
    fn an_unparseable_policy_is_an_error_not_an_absent_one() {
        let d = tmp("bad");
        std::fs::write(path(&d), "{ not json").unwrap();
        assert!(load(&d).is_err());
        // An unknown field is refused too — `denyPriviledged` spelled wrong
        // would otherwise read as "allowed".
        std::fs::write(path(&d), r#"{"denyPriviledged": true}"#).unwrap();
        assert!(load(&d).is_err(), "a typo must not read as permissive");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn no_file_means_no_ceiling() {
        let d = tmp("none");
        assert_eq!(load(&d).unwrap(), None);
        assert!(enforce(&d, "web", &Request::container("alpine:latest", true, true)).is_ok());
        std::fs::remove_dir_all(&d).ok();
    }

    /// The gap this crate was written to close, end to end through the file.
    #[test]
    fn a_vm_with_passthrough_is_refused_by_the_file_on_disk() {
        let d = tmp("vm");
        std::fs::write(path(&d), r#"{"denyDevicePassthrough": true}"#).unwrap();
        let devices = vec!["0000:01:00.0".to_string()];
        let e = enforce(&d, "db-01", &Request::virtual_machine(None, &devices, None)).unwrap_err();
        assert!(format!("{e}").contains("0000:01:00.0"), "{e}");
        std::fs::remove_dir_all(&d).ok();
    }

    /// Every refusal is rendered — a violation with no message would reach the
    /// operator as a blank line.
    #[test]
    fn every_violation_renders_to_something() {
        let all = [
            Violation::Privileged,
            Violation::HostNetwork,
            Violation::LatestTag { image: "a".into() },
            Violation::Registry {
                image: "a".into(),
                host: "h".into(),
                allowed: vec!["g".into()],
            },
            Violation::DevicePassthrough {
                devices: vec!["0000:01:00.0".into()],
            },
            Violation::LatestVmImage { image: "a".into() },
            Violation::ImageUrlHost {
                url: "https://x/y".into(),
                host: "x".into(),
                allowed: vec!["z".into()],
            },
        ];
        for v in &all {
            assert!(!render(v).trim().is_empty(), "{v:?} rendered empty");
        }
    }

    fn doc(spec_yaml: &str) -> ManifestDoc {
        ManifestDoc {
            api_version: "security.delonix.io/v1alpha1".into(),
            kind: k::RUNTIME_POLICY.into(),
            metadata: manifest::Metadata {
                name: "node-ceiling".into(),
                namespace: None,
                labels: Default::default(),
                annotations: Default::default(),
            },
            spec: serde_yaml::from_str(spec_yaml).unwrap(),
        }
    }

    /// The full end-to-end shape: a manifest doc becomes a real
    /// [`SecurityPolicy`] with every field carried across, byte for byte.
    #[test]
    fn a_manifest_doc_becomes_the_real_security_policy() {
        let d = doc(r#"
            mode: warn
            denyPrivileged: true
            denyHostNetwork: true
            denyLatestTag: true
            allowedRegistries: [ghcr.io, docker.io]
            denyDevicePassthrough: true
            denyLatestVmImage: true
            allowedImageUrlHosts: [cloud.debian.org]
            "#);
        let policy =
            to_security_policy(&manifest::spec_of::<RuntimePolicySpec>(&d).unwrap()).unwrap();
        assert_eq!(policy.mode, srt::policy::Mode::Warn);
        assert!(policy.deny_privileged);
        assert!(policy.deny_host_network);
        assert!(policy.deny_latest_tag);
        assert_eq!(policy.allowed_registries, ["ghcr.io", "docker.io"]);
        assert!(policy.deny_device_passthrough);
        assert!(policy.deny_latest_vm_image);
        assert_eq!(policy.allowed_image_url_hosts, ["cloud.debian.org"]);
    }

    /// Omitted `mode` means `enforce` — the same default `SecurityPolicy`
    /// itself has, never re-decided here.
    #[test]
    fn an_empty_spec_means_enforce_and_no_opinion() {
        let d = doc("{}");
        let policy =
            to_security_policy(&manifest::spec_of::<RuntimePolicySpec>(&d).unwrap()).unwrap();
        assert_eq!(policy, SecurityPolicy::default());
    }

    /// An unrecognised `mode` is refused with the value named — never silently
    /// coerced into `enforce`, which would read as "the strictest setting
    /// applied" when the truth is a typo went unnoticed.
    #[test]
    fn an_unknown_mode_is_refused_by_name() {
        let d = doc("mode: sometimes");
        let spec: RuntimePolicySpec = manifest::spec_of(&d).unwrap();
        let e = to_security_policy(&spec).unwrap_err();
        assert!(format!("{e}").contains("sometimes"), "{e}");
    }

    /// `desired`/`actual` build the field map from the SAME function
    /// (`fields_of`), so a manifest applied once and read back produces zero
    /// diff — the round-trip `stack plan` depends on.
    #[test]
    fn desired_and_actual_agree_on_an_unchanged_policy() {
        let d = doc("denyPrivileged: true\nallowedRegistries: [ghcr.io, docker.io]");
        let spec: RuntimePolicySpec = manifest::spec_of(&d).unwrap();
        let policy = to_security_policy(&spec).unwrap();
        let want = super::desired(&d).unwrap();
        let have = fields_of(&policy);
        assert_eq!(want.fields, have);
        assert!(want.converges);
        assert!(
            !want.ownable,
            "a node-wide ceiling is not per-stack content"
        );
    }

    /// A field left out of `RUNTIME_POLICY_SPEC_FIELDS` would warn on every
    /// manifest that writes the whole spec — this is the drift-guard the other
    /// Kinds' `*_SPEC_FIELDS` tests already run.
    #[test]
    fn every_field_serde_writes_is_in_the_accept_list() {
        let spec = RuntimePolicySpec {
            mode: Some("warn".into()),
            deny_privileged: true,
            deny_host_network: true,
            deny_latest_tag: true,
            allowed_registries: vec!["ghcr.io".into()],
            deny_device_passthrough: true,
            deny_latest_vm_image: true,
            allowed_image_url_hosts: vec!["x".into()],
        };
        let v = serde_yaml::to_value(&spec).unwrap();
        let serde_yaml::Value::Mapping(m) = v else {
            panic!("not a mapping")
        };
        for key in m.keys().filter_map(|k| k.as_str()) {
            assert!(
                RUNTIME_POLICY_SPEC_FIELDS.contains(&key),
                "'{key}' is written by Serialize and missing from the accept list"
            );
        }
    }
}
