//! `kind: GitOpsSource` and `delonix gitops` — the pull reconciler (ADR-0021).
//!
//! A source names a git repository, a ref and a path. Applying one writes a
//! record and, when `sync.automated` is set, a systemd timer. Each fire runs
//! ONE pass and exits — there is no resident process, so an engine with no
//! source configured is byte for byte the engine without this module.
//!
//! A pass, in order, and nothing is applied before the last step:
//!
//! 1. take the source's lock (a fire that cannot take it skips and says so);
//! 2. fetch the ref into a private bare mirror (`cmd::gitops_git`);
//! 3. resolve it to a commit, and verify the commit's signature when asked;
//! 4. read the YAML under `path` straight from the commit's tree (nothing is
//!    checked out: no symlink, hook or filter of the repository ever runs);
//! 5. load it, and run the pull-source guard over what it declares;
//! 6. validate the graph and plan, and weigh the plan against the safety policy;
//! 7. apply the plan with its digests, prune what the policy allows, record
//!    the commit and emit an event.
//!
//! A commit refused at any step before 7 leaves the running stack untouched,
//! and the refusal is recorded (`gitops status`) with the commit it names.
//!
//! The threat list every refusal here answers is the ADR's «Security pass».

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::Subcommand;
use delonix_compute::RunOpts;
use delonix_model::{Error, Result};

use super::gitops_git::{Git, SshFiles};
use super::kinds as k;
use super::manifest::{self, ManifestDoc};
use super::output::OutputFormat;
use super::reconcile::{Action, Change};

// ---------------------------------------------------------------------------
// The spec
// ---------------------------------------------------------------------------

/// `spec` of `kind: GitOpsSource`.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitOpsSourceSpec {
    /// Where the manifests come from.
    pub source: SourceSpec,
    /// Whether a timer reconciles it.
    #[serde(default)]
    pub sync: SyncSpec,
    /// What a pass may do to the machine.
    #[serde(default)]
    pub safety: SafetySpec,
    /// The stack a pass applies into. Defaults to the source's own name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

/// `spec.source`.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SourceSpec {
    /// `file:///<absolute path>`, `https://…` (anonymous) or `ssh://…`.
    pub url: String,
    /// `refs/heads/<branch>` or `refs/tags/<tag>`.
    #[serde(rename = "ref", default = "default_ref")]
    pub git_ref: String,
    /// The directory inside the repository whose YAML is the manifest.
    #[serde(default)]
    pub path: String,
    /// How often the timer fires (`30s`, `5m`, `1h`): 15 seconds to 24 hours.
    #[serde(default = "default_interval")]
    pub interval: String,
    /// A `kind: Secret` holding the SSH private key, under the key `identity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    /// A `kind: Secret` holding the pinned host keys, under `known_hosts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_hosts_ref: Option<String>,
    /// Refuse a commit whose SSH signature is not from `trustedKeysRef`.
    #[serde(default)]
    pub signed_commits_required: bool,
    /// A `kind: Secret` holding an SSH `allowed_signers` file, under
    /// `allowed_signers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_keys_ref: Option<String>,
}

/// `spec.sync`.
#[derive(
    Debug, Clone, Default, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SyncSpec {
    /// Install the timer. `false` registers the source and runs a pass only
    /// when asked (`delonix gitops reconcile`).
    #[serde(default)]
    pub automated: bool,
}

/// `spec.safety`.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SafetySpec {
    /// The plan actions a pass may carry out, in the reconciler's own names.
    /// `NoOp` is always allowed; `Delete` is `prune`'s, `Conflict` never is.
    #[serde(default = "default_allow")]
    pub allow: Vec<String>,
    /// Remove what the stack owns and the commit no longer declares.
    #[serde(default)]
    pub prune: bool,
    /// The most a single pass may prune.
    #[serde(default = "default_max_deletes")]
    pub maximum_deletes: u32,
    /// `Kind/name` (or `Kind/*`) a pass never prunes.
    #[serde(default)]
    pub protected_resources: Vec<String>,
    /// The Kinds a pass may apply. Empty is the default set; naming a Kind
    /// that is off by default is how it is let in.
    #[serde(default)]
    pub kinds: Vec<String>,
}

impl Default for SafetySpec {
    fn default() -> Self {
        SafetySpec {
            allow: default_allow(),
            prune: false,
            maximum_deletes: default_max_deletes(),
            protected_resources: Vec::new(),
            kinds: Vec::new(),
        }
    }
}

fn default_ref() -> String {
    "refs/heads/main".to_string()
}
fn default_interval() -> String {
    "60s".to_string()
}
fn default_allow() -> Vec<String> {
    vec!["Create".to_string(), "Update".to_string()]
}
fn default_max_deletes() -> u32 {
    5
}

/// Known fields of the `spec`.
pub const GITOPS_SOURCE_SPEC_FIELDS: &[&str] = &["source", "sync", "safety", "stack"];

/// The fields the reconciler compares — all hot (`reconcile::hot_fields`).
pub const RECONCILED_GITOPS_SOURCE_FIELDS: &[&str] = &[
    "url",
    "ref",
    "path",
    "interval",
    "secretRef",
    "knownHostsRef",
    "signedCommitsRequired",
    "trustedKeysRef",
    "automated",
    "allow",
    "prune",
    "maximumDeletes",
    "protectedResources",
    "kinds",
    "stack",
];

/// The fields a source declared IN ITS OWN REPOSITORY may change. Everything
/// else — the URL, the credentials, the signature requirement, the safety
/// policy, the timer — is the local registration's, so a commit cannot loosen
/// the rules that judge it.
const SELF_MUTABLE: &[&str] = &["ref", "path", "interval"];

/// Kinds a pass never applies, whatever the registration says.
///
/// - `RuntimePolicy`: the node's admission ceiling is the administrator's; a
///   repository that could write it could lower it.
/// - `Secret`: a credential never comes from the repository it unlocks, and
///   values committed to git are values every reader of the repository has.
/// - `App`: a buildpack build runs the repository's code with the node's
///   builder; out of v1 (as is `Image` with `build:`).
const HARD_REFUSED: &[&str] = &[k::RUNTIME_POLICY, k::SECRET, k::APP];

/// Kinds a pass applies only when the registration names them in
/// `safety.kinds`: each one reaches beyond a container on this node.
///
/// - `VirtualMachine`: its spec carries host paths (disk, kernel, firmware, raw
///   libvirt XML).
/// - `SystemContainer`, `NetworkZone`, `NetworkGateway`: they act on a remote
///   provider with the node's own provider credentials.
/// - `StoragePool`: it consumes the administrator's allowlist.
/// - `Gateway`: it tunnels a local port to the internet.
/// - `IPPool`: it announces addresses on a host interface.
const OPT_IN: &[&str] = &[
    k::VM,
    k::SYSTEM_CONTAINER,
    k::NETWORK_ZONE,
    k::NETWORK_GATEWAY,
    k::STORAGE_POOL,
    k::GATEWAY,
    k::IPPOOL,
];

/// `spec` keys of a `Volume` that reach beyond a plain local directory.
const VOLUME_HOST_KEYS: &[&str] = &[
    "driver",
    "device",
    "mountOptions",
    "options",
    "share",
    "nfs",
    "cifs",
    "webdav",
    "provision",
    "pool",
];

/// Bounds on what a commit may carry (the ADR's threat list).
const MAX_TREE_ENTRIES: usize = 4096;
const MAX_MANIFEST_FILES: usize = 256;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DOCUMENTS: usize = 512;
const MAX_MIRROR_BYTES: u64 = 256 * 1024 * 1024;

fn invalid_source(msg: String) -> Error {
    Error::coded(1601, Error::Invalid(msg))
}

fn rejected(msg: String) -> Error {
    Error::coded(1602, Error::Invalid(msg))
}

fn too_large(msg: String) -> Error {
    Error::coded(1603, Error::Invalid(msg))
}

// ---------------------------------------------------------------------------
// Validation of the registration (pure)
// ---------------------------------------------------------------------------

/// The transport a URL resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Transport {
    File(PathBuf),
    Https,
    Ssh,
}

/// Parses a source URL. Only three forms are accepted, and each is checked
/// here rather than left to git: a value git would read as an option, a helper
/// transport (`ext::`), or a URL with a password in it never reaches a process.
pub(crate) fn validate_url(url: &str) -> Result<Transport> {
    let refuse = |why: &'static str| {
        Err(invalid_source(super::po::tf(
            "spec.source.url '{url}': {why}",
            &[("url", url), ("why", super::po::t(why))],
        )))
    };
    if url.is_empty() || url.len() > 2048 {
        return refuse("empty or longer than 2048 characters");
    }
    if url.starts_with('-') {
        return refuse("starts with '-', which git would read as an option");
    }
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return refuse("carries a space, a backslash or a control character");
    }
    if let Some(path) = url.strip_prefix("file://") {
        let p = Path::new(path);
        if !p.is_absolute() || path.split('/').any(|c| c == "..") {
            return refuse("a file:// URL names an absolute path without '..'");
        }
        return Ok(Transport::File(p.to_path_buf()));
    }
    let (scheme, rest) = match url.split_once("://") {
        Some(x) => x,
        None => {
            return refuse("not a URL this engine fetches — use file:///<path>, https:// or ssh://")
        }
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() || authority.starts_with('-') {
        return refuse("names no host");
    }
    match scheme {
        "https" => {
            if authority.contains('@') {
                return refuse(
                    "carries credentials in the URL — name a kind: Secret instead (https with a credential is not implemented)",
                );
            }
            Ok(Transport::Https)
        }
        "ssh" => {
            if let Some((user, _)) = authority.split_once('@') {
                if user.contains(':') {
                    return refuse("carries a password in the URL");
                }
            }
            Ok(Transport::Ssh)
        }
        _ => refuse("only file://, https:// and ssh:// are fetched (no http://, git:// or ext::)"),
    }
}

/// A ref a pass may fetch: a branch or a tag, spelled so neither git nor a
/// shell can read it as anything else.
pub(crate) fn validate_ref(r: &str) -> Result<()> {
    let ok = (r.starts_with("refs/heads/") || r.starts_with("refs/tags/"))
        && r.len() <= 255
        && !r.contains("..")
        && !r.contains("//")
        && !r.contains("@{")
        && !r.ends_with('/')
        && !r.ends_with(".lock")
        && !r.ends_with('.')
        && r.split('/')
            .all(|c| !c.is_empty() && !c.starts_with('.') && !c.starts_with('-'))
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(invalid_source(super::po::tf(
            "spec.source.ref '{ref}': expected refs/heads/<branch> or refs/tags/<tag> (letters, digits, '.', '_', '-', '/')",
            &[("ref", r)],
        )))
    }
}

/// A directory inside the repository: relative, and made of plain components.
pub(crate) fn validate_repo_path(p: &str) -> Result<()> {
    if p.is_empty() {
        return Ok(());
    }
    let ok = p.len() <= 1024
        && !p.starts_with('/')
        && p.trim_end_matches('/').split('/').all(|c| {
            !c.is_empty()
                && c != "."
                && c != ".."
                && !c.starts_with('-')
                && c.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        });
    if ok {
        Ok(())
    } else {
        Err(invalid_source(super::po::tf(
            "spec.source.path '{path}': a relative directory of plain components (no '..', no leading '/' or '-')",
            &[("path", p)],
        )))
    }
}

/// `30s`, `5m`, `1h` → seconds, between 15 seconds and 24 hours.
pub(crate) fn parse_interval(s: &str) -> Result<u64> {
    let t = s.trim();
    let (num, mult) = match t.char_indices().last() {
        Some((i, 's')) => (&t[..i], 1),
        Some((i, 'm')) => (&t[..i], 60),
        Some((i, 'h')) => (&t[..i], 3600),
        _ => (t, 1),
    };
    let secs = num
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .filter(|n| (15..=86_400).contains(n));
    secs.ok_or_else(|| {
        invalid_source(super::po::tf(
            "spec.source.interval '{value}': a number of seconds, minutes or hours (30s, 5m, 1h), from 15s to 24h",
            &[("value", s)],
        ))
    })
}

/// A source's name and namespace become a directory and a systemd unit name,
/// so they are DNS labels and nothing else.
pub(crate) fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

fn valid_secret_ref(s: &str) -> bool {
    delonix_model::secret::valid_name(s) && !s.starts_with('-') && !s.starts_with('.')
}

/// The plan actions `safety.allow` may name.
const ALLOWABLE: &[&str] = &["Create", "Update", "Adopt", "Replace", "Reboot"];

/// Everything about a registration that can be judged without the machine.
pub(crate) fn validate_spec(spec: &GitOpsSourceSpec, namespace: &str, name: &str) -> Result<()> {
    if !valid_ident(name) || !valid_ident(namespace) {
        return Err(invalid_source(super::po::tf(
            "GitOpsSource '{ns}/{name}': the name and namespace are lowercase letters, digits and '-' (at most 63)",
            &[("ns", namespace), ("name", name)],
        )));
    }
    let transport = validate_url(&spec.source.url)?;
    validate_ref(&spec.source.git_ref)?;
    validate_repo_path(&spec.source.path)?;
    parse_interval(&spec.source.interval)?;
    for (field, v) in [
        ("secretRef", &spec.source.secret_ref),
        ("knownHostsRef", &spec.source.known_hosts_ref),
        ("trustedKeysRef", &spec.source.trusted_keys_ref),
    ] {
        if let Some(v) = v {
            if !valid_secret_ref(v) {
                return Err(invalid_source(super::po::tf(
                    "spec.source.{field} '{value}': not a secret name",
                    &[("field", field), ("value", v)],
                )));
            }
        }
    }
    match transport {
        Transport::Ssh => {
            if spec.source.secret_ref.is_none() || spec.source.known_hosts_ref.is_none() {
                return Err(invalid_source(
                    super::po::t(
                        "an ssh:// source needs secretRef (the key) and knownHostsRef (the pinned host keys) — a host key is never accepted on first sight",
                    )
                    .into(),
                ));
            }
        }
        Transport::Https | Transport::File(_) => {
            if spec.source.secret_ref.is_some() || spec.source.known_hosts_ref.is_some() {
                return Err(invalid_source(
                    super::po::t(
                        "secretRef/knownHostsRef are read only for an ssh:// source (an https:// credential is not implemented) — remove them, or use ssh://",
                    )
                    .into(),
                ));
            }
        }
    }
    if spec.source.signed_commits_required != spec.source.trusted_keys_ref.is_some() {
        return Err(invalid_source(
            super::po::t(
                "signedCommitsRequired and trustedKeysRef go together: one without the other would read as a protection that is not there",
            )
            .into(),
        ));
    }
    for a in &spec.safety.allow {
        if !ALLOWABLE.contains(&a.as_str()) {
            return Err(invalid_source(super::po::tf(
                "spec.safety.allow '{value}': the actions a pass may take are Create, Update, Adopt, Replace and Reboot (NoOp always; Delete is spec.safety.prune; Conflict never)",
                &[("value", a)],
            )));
        }
    }
    for kind in &spec.safety.kinds {
        let canon = manifest::canonical_kind(kind);
        if HARD_REFUSED.contains(&canon) {
            return Err(invalid_source(super::po::tf(
                "spec.safety.kinds '{kind}': a pull source never applies RuntimePolicy, Secret or App",
                &[("kind", kind)],
            )));
        }
        if canon == k::GITOPS_SOURCE || !super::kinds::in_stack(canon) {
            return Err(invalid_source(super::po::tf(
                "spec.safety.kinds '{kind}': not a Kind a pass applies (a source declares itself without being listed, and never another source)",
                &[("kind", kind)],
            )));
        }
    }
    for p in &spec.safety.protected_resources {
        let ok = p
            .split_once('/')
            .map(|(kind, n)| {
                super::kinds::facts(manifest::canonical_kind(kind)).is_some() && !n.is_empty()
            })
            .unwrap_or(false);
        if !ok {
            return Err(invalid_source(super::po::tf(
                "spec.safety.protectedResources '{value}': expected <Kind>/<name> or <Kind>/*",
                &[("value", p)],
            )));
        }
    }
    if let Some(s) = &spec.stack {
        if !valid_ident(s) {
            return Err(invalid_source(super::po::tf(
                "spec.stack '{value}': lowercase letters, digits and '-' (at most 63)",
                &[("value", s)],
            )));
        }
    }
    Ok(())
}

/// The comparable form of a spec.
fn gitops_fields(spec: &GitOpsSourceSpec) -> BTreeMap<String, String> {
    let opt = |o: &Option<String>| o.clone().unwrap_or_default();
    let mut f = BTreeMap::new();
    f.insert("url".into(), spec.source.url.clone());
    f.insert("ref".into(), spec.source.git_ref.clone());
    f.insert("path".into(), spec.source.path.clone());
    f.insert("interval".into(), spec.source.interval.clone());
    f.insert("secretRef".into(), opt(&spec.source.secret_ref));
    f.insert("knownHostsRef".into(), opt(&spec.source.known_hosts_ref));
    f.insert(
        "signedCommitsRequired".into(),
        spec.source.signed_commits_required.to_string(),
    );
    f.insert("trustedKeysRef".into(), opt(&spec.source.trusted_keys_ref));
    f.insert("automated".into(), spec.sync.automated.to_string());
    f.insert("allow".into(), spec.safety.allow.join(","));
    f.insert("prune".into(), spec.safety.prune.to_string());
    f.insert(
        "maximumDeletes".into(),
        spec.safety.maximum_deletes.to_string(),
    );
    f.insert(
        "protectedResources".into(),
        spec.safety.protected_resources.join(","),
    );
    f.insert("kinds".into(), spec.safety.kinds.join(","));
    f.insert("stack".into(), opt(&spec.stack));
    f
}

fn namespace_of(doc: &ManifestDoc) -> String {
    doc.metadata
        .namespace
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

fn spec_of(doc: &ManifestDoc) -> Result<GitOpsSourceSpec> {
    let spec: GitOpsSourceSpec = manifest::spec_of(doc)?;
    validate_spec(&spec, &namespace_of(doc), &doc.metadata.name)?;
    Ok(spec)
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// What `stack apply` wrote for one source.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SourceRecord {
    pub namespace: String,
    pub name: String,
    pub spec: GitOpsSourceSpec,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

/// What the last pass found.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct PassStatus {
    /// `synced`, `rejected`, `blocked` or `failed`.
    pub result: String,
    /// The commit that pass looked at.
    #[serde(default)]
    pub commit: Option<String>,
    /// The last commit applied successfully — carried across a refusal, so a
    /// refused commit never hides what is running.
    #[serde(default)]
    pub applied: Option<String>,
    pub at_unix: u64,
    pub message: String,
}

fn gitops_root(root: &Path) -> PathBuf {
    root.join("gitops")
}

fn source_dir(root: &Path, namespace: &str, name: &str) -> PathBuf {
    gitops_root(root).join(namespace).join(name)
}

fn record_path(root: &Path, namespace: &str, name: &str) -> PathBuf {
    source_dir(root, namespace, name).join("source.json")
}

fn status_path(root: &Path, namespace: &str, name: &str) -> PathBuf {
    source_dir(root, namespace, name).join("status.json")
}

fn suspend_marker(root: &Path, namespace: &str, name: &str) -> PathBuf {
    source_dir(root, namespace, name).join("suspended")
}

pub(crate) fn load_record(root: &Path, namespace: &str, name: &str) -> Result<SourceRecord> {
    if !valid_ident(namespace) || !valid_ident(name) {
        return Err(not_found(namespace, name));
    }
    let p = record_path(root, namespace, name);
    let bytes = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(not_found(namespace, name))
        }
        Err(e) => return Err(e.into()),
    };
    Ok(serde_json::from_slice(&bytes)?)
}

fn not_found(namespace: &str, name: &str) -> Error {
    Error::coded(
        4601,
        Error::NotFound(format!(
            "gitopssource {namespace}/{name} (see `delonix get gitopssources`)"
        )),
    )
}

fn save_record(root: &Path, rec: &SourceRecord) -> Result<()> {
    let dir = source_dir(root, &rec.namespace, &rec.name);
    std::fs::create_dir_all(&dir)?;
    let bytes = serde_json::to_vec_pretty(rec)?;
    delonix_state::write_atomic_mode(&dir.join("source.json"), &bytes, Some(0o600))
        .map_err(Into::into)
}

fn load_status(root: &Path, namespace: &str, name: &str) -> Option<PassStatus> {
    std::fs::read(status_path(root, namespace, name))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn save_status(root: &Path, namespace: &str, name: &str, st: &PassStatus) {
    if let Ok(bytes) = serde_json::to_vec_pretty(st) {
        let _ = delonix_state::write_atomic_mode(
            &status_path(root, namespace, name),
            &bytes,
            Some(0o600),
        );
    }
}

/// Every registered source, in (namespace, name) order.
pub(crate) fn list_records(root: &Path) -> Vec<SourceRecord> {
    let mut out = Vec::new();
    let Ok(nss) = std::fs::read_dir(gitops_root(root)) else {
        return out;
    };
    for ns in nss.flatten() {
        let Ok(names) = std::fs::read_dir(ns.path()) else {
            continue;
        };
        for n in names.flatten() {
            if let Ok(b) = std::fs::read(n.path().join("source.json")) {
                if let Ok(rec) = serde_json::from_slice::<SourceRecord>(&b) {
                    out.push(rec);
                }
            }
        }
    }
    out.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    out
}

/// The namespaces sources live in — for completion.
pub(crate) fn namespaces(root: &Path) -> Vec<String> {
    list_records(root)
        .into_iter()
        .map(|r| r.namespace)
        .collect()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The reconciler's questions
// ---------------------------------------------------------------------------

pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec = spec_of(doc)?;
    Ok(super::reconcile::Desired {
        kind: k::GITOPS_SOURCE.into(),
        name: manifest::plan_name(doc),
        fields: gitops_fields(&spec),
        converges: true,
        ownable: true,
    })
}

pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    let root = super::util::state_root();
    Ok(list_records(&root)
        .into_iter()
        .map(|rec| super::reconcile::Actual {
            kind: k::GITOPS_SOURCE.into(),
            name: manifest::scoped_plan_name(&rec.namespace, &rec.name),
            fields: gitops_fields(&rec.spec),
            owner: rec.labels.get(super::reconcile::STACK_LABEL).cloned(),
            last_applied: rec
                .annotations
                .get(super::reconcile::LAST_APPLIED)
                .and_then(|raw| super::reconcile::decode_last_applied(raw)),
        })
        .collect())
}

pub(crate) fn stamp(plan: &str, stack: &str, fields: &BTreeMap<String, String>) -> Result<()> {
    let root = super::util::state_root();
    let (namespace, name) = manifest::split_plan_name(plan);
    let mut rec = load_record(&root, namespace, name)?;
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
    save_record(&root, &rec)
}

pub(crate) fn presence_of(doc: &ManifestDoc) -> (String, String) {
    let root = super::util::state_root();
    let ns = namespace_of(doc);
    if load_record(&root, &ns, &doc.metadata.name).is_err() {
        return ("no".into(), "-".into());
    }
    ("yes".into(), status_word(&root, &ns, &doc.metadata.name))
}

/// One line for `stack ls`/`get`: suspended, or what the last pass found.
fn status_word(root: &Path, ns: &str, name: &str) -> String {
    if suspend_marker(root, ns, name).exists() {
        return super::po::t("suspended").into();
    }
    match load_status(root, ns, name) {
        None => super::po::t("never reconciled").into(),
        Some(st) => format!(
            "{} {}",
            st.result,
            st.commit.as_deref().map(short).unwrap_or("-")
        ),
    }
}

fn short(sha: &str) -> &str {
    &sha[..12.min(sha.len())]
}

/// Applies every `kind: GitOpsSource` document: the record, then the timer.
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, k::GITOPS_SOURCE) {
        apply_one(doc)?;
    }
    Ok(())
}

pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    apply_one(doc)
}

fn apply_one(doc: &ManifestDoc) -> Result<()> {
    let root = super::util::state_root();
    let spec = spec_of(doc)?;
    let ns = namespace_of(doc);
    let name = doc.metadata.name.clone();
    // The bootstrap secret is created out of band (ADR-0021); a reference to
    // one that is not there is refused now, not on the first fire.
    let store = delonix_state::SecretStore::open(&root)?;
    for (field, v) in [
        ("secretRef", &spec.source.secret_ref),
        ("knownHostsRef", &spec.source.known_hosts_ref),
        ("trustedKeysRef", &spec.source.trusted_keys_ref),
    ] {
        if let Some(v) = v {
            store.load(v).map_err(|_| {
                invalid_source(super::po::tf(
                    "spec.source.{field}: secret '{value}' does not exist on this node — create it first (a credential never comes from the repository it unlocks)",
                    &[("field", field), ("value", v)],
                ))
            })?;
        }
    }
    let previous = load_record(&root, &ns, &name).ok();
    let rec = SourceRecord {
        namespace: ns.clone(),
        name: name.clone(),
        spec: spec.clone(),
        labels: previous
            .as_ref()
            .map(|p| p.labels.clone())
            .unwrap_or_default(),
        annotations: previous.map(|p| p.annotations).unwrap_or_default(),
    };
    save_record(&root, &rec)?;
    let timer = if spec.sync.automated {
        install_timer(&root, &rec)?;
        super::po::tf(
            "timer every {interval}",
            &[("interval", &spec.source.interval)],
        )
    } else {
        remove_timer(&ns, &name)?;
        super::po::t("no timer (sync.automated is false)").into()
    };
    println!(
        "{}",
        super::po::tf(
            "gitopssource/{ns}/{name}: registered — {url} {ref}, {timer}",
            &[
                ("ns", &ns),
                ("name", &name),
                ("url", &spec.source.url),
                ("ref", &spec.source.git_ref),
                ("timer", &timer),
            ],
        )
    );
    Ok(())
}

/// `--prune`/`destroy`/`delete gitopssources`: the timer first, so nothing
/// reconciles while the rest goes, then the source's whole directory.
pub(crate) fn remove_for_replace(plan: &str) -> Result<()> {
    let root = super::util::state_root();
    let (namespace, name) = resolve_name(&root, plan)?;
    remove_timer(&namespace, &name)?;
    let dir = source_dir(&root, &namespace, &name);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    // The namespace directory goes with its last source; `remove_dir` refuses
    // a directory that still holds another one, which is the answer wanted.
    let _ = std::fs::remove_dir(gitops_root(&root).join(&namespace));
    Ok(())
}

/// `ns/name`, or a bare name that names exactly one source.
fn resolve_name(root: &Path, given: &str) -> Result<(String, String)> {
    if let Some((ns, name)) = given.split_once('/') {
        load_record(root, ns, name)?;
        return Ok((ns.to_string(), name.to_string()));
    }
    let hits: Vec<SourceRecord> = list_records(root)
        .into_iter()
        .filter(|r| r.name == given)
        .collect();
    match hits.len() {
        0 => Err(not_found("default", given)),
        1 => Ok((hits[0].namespace.clone(), hits[0].name.clone())),
        _ => Err(Error::Invalid(super::po::tf(
            "gitopssource '{name}' exists in several namespaces — name it as <namespace>/{name}",
            &[("name", given)],
        ))),
    }
}

pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: GitOpsSourceSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

// ---------------------------------------------------------------------------
// The timer
// ---------------------------------------------------------------------------

/// `delonix-gitops-<namespace>.<name>`. The prefix is the ADR's decision 6:
/// `delonix-` alone once matched `delonix-cri.service`. The `.` cannot occur in
/// either part (both are DNS labels), so two sources never share a unit.
pub(crate) fn unit_base(namespace: &str, name: &str) -> String {
    format!("delonix-gitops-{namespace}.{name}")
}

fn unit_dir() -> (PathBuf, bool) {
    if delonix_node::is_rootless() {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
                    .join(".config")
            });
        (base.join("systemd/user"), true)
    } else {
        (PathBuf::from("/etc/systemd/system"), false)
    }
}

/// A value placed inside a unit file: `%` is a systemd specifier, and a quote,
/// a backslash or a newline would end the value early.
fn unit_value(s: &str) -> Result<String> {
    if s.chars().any(|c| matches!(c, '"' | '\\' | '\n' | '\r')) {
        return Err(Error::Invalid(super::po::tf(
            "gitops: '{value}' cannot be written into a systemd unit (quote, backslash or newline)",
            &[("value", s)],
        )));
    }
    Ok(s.replace('%', "%%"))
}

/// The service a fire runs: one pass, then exit.
pub(crate) fn service_unit(
    exe: &Path,
    root: &Path,
    net_runtime: Option<&Path>,
    namespace: &str,
    name: &str,
) -> Result<String> {
    if !exe.is_absolute() || !root.is_absolute() {
        return Err(Error::Invalid(
            super::po::t(
                "gitops: refusing to write a timer with a relative path — a timer does not run from your current directory",
            )
            .into(),
        ));
    }
    let mut env = format!(
        "Environment=\"DELONIX_ROOT={}\"\n",
        unit_value(&root.to_string_lossy())?
    );
    if let Some(n) = net_runtime {
        env.push_str(&format!(
            "Environment=\"DELONIX_NET_RUNTIME_DIR={}\"\n",
            unit_value(&n.to_string_lossy())?
        ));
    }
    Ok(format!(
        "[Unit]\nDescription=delonix GitOps pass — {namespace}/{name}\n\n\
         [Service]\nType=oneshot\n{env}\
         ExecStart=\"{exe}\" gitops reconcile {name} --namespace {namespace}\n",
        exe = unit_value(&exe.to_string_lossy())?,
    ))
}

/// The timer: first fire one interval after boot, then one interval after the
/// previous pass ENDED (never overlapping), with up to a tenth of the interval
/// (at most 30 s) of jitter so a fleet does not fetch in lockstep.
pub(crate) fn timer_unit(namespace: &str, name: &str, interval_secs: u64) -> String {
    let jitter = (interval_secs / 10).min(30);
    let base = unit_base(namespace, name);
    format!(
        "[Unit]\nDescription=delonix GitOps timer — {namespace}/{name}\n\n\
         [Timer]\nOnBootSec={interval_secs}s\nOnUnitInactiveSec={interval_secs}s\n\
         RandomizedDelaySec={jitter}s\nAccuracySec=1s\nUnit={base}.service\n\n\
         [Install]\nWantedBy=timers.target\n"
    )
}

fn systemctl(user: bool, args: &[&str]) -> Result<()> {
    let mut c = std::process::Command::new("systemctl");
    if user {
        c.arg("--user");
    }
    let st = c.args(args).status().map_err(|e| {
        Error::coded(
            6603,
            Error::Unavailable(super::po::tf(
                "gitops: systemctl is not available ({err}) — a source with sync.automated needs systemd; set it to false and run `delonix gitops reconcile` from your own scheduler",
                &[("err", &e.to_string())],
            )),
        )
    })?;
    if !st.success() {
        return Err(Error::coded(
            6603,
            Error::Unavailable(super::po::tf(
                "gitops: `systemctl {args}` failed (see its output above)",
                &[("args", &args.join(" "))],
            )),
        ));
    }
    Ok(())
}

/// Writes the two units when they changed, then enables the timer. Unchanged
/// units are left alone, so a pass that re-applies its own source does not
/// reload the manager every fire.
fn install_timer(root: &Path, rec: &SourceRecord) -> Result<()> {
    let (dir, user) = unit_dir();
    let exe = std::env::current_exe()?;
    let root_abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let net = std::env::var_os("DELONIX_NET_RUNTIME_DIR").map(PathBuf::from);
    let base = unit_base(&rec.namespace, &rec.name);
    let service = service_unit(&exe, &root_abs, net.as_deref(), &rec.namespace, &rec.name)?;
    let timer = timer_unit(
        &rec.namespace,
        &rec.name,
        parse_interval(&rec.spec.source.interval)?,
    );
    std::fs::create_dir_all(&dir)?;
    let mut changed = false;
    for (file, body) in [
        (format!("{base}.service"), &service),
        (format!("{base}.timer"), &timer),
    ] {
        let p = dir.join(&file);
        if std::fs::read_to_string(&p).ok().as_deref() != Some(body.as_str()) {
            std::fs::write(&p, body)?;
            changed = true;
        }
    }
    if changed {
        systemctl(user, &["daemon-reload"])?;
        systemctl(user, &["enable", "--now", &format!("{base}.timer")])?;
        if user && !lingering() {
            println!(
                "{}",
                super::po::t(
                    "  note: this timer stops when you log out — `sudo loginctl enable-linger $USER` keeps it running"
                )
            );
        }
    }
    Ok(())
}

fn remove_timer(namespace: &str, name: &str) -> Result<()> {
    let (dir, user) = unit_dir();
    let base = unit_base(namespace, name);
    let timer = dir.join(format!("{base}.timer"));
    let service = dir.join(format!("{base}.service"));
    if !timer.exists() && !service.exists() {
        return Ok(());
    }
    // A timer that cannot be stopped is not left running behind a removed
    // record: the error stops the removal, and the record stays to say so.
    systemctl(user, &["disable", "--now", &format!("{base}.timer")])?;
    let _ = std::fs::remove_file(&timer);
    let _ = std::fs::remove_file(&service);
    systemctl(user, &["daemon-reload"])
}

fn lingering() -> bool {
    std::process::Command::new("loginctl")
        // SAFETY: `getuid` takes no arguments and has no preconditions.
        .args(["show-user", &format!("{}", unsafe { libc::getuid() })])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Linger=yes"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Reading a commit (pure parts)
// ---------------------------------------------------------------------------

/// One line of `git ls-tree -r -l -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeEntry {
    pub mode: String,
    pub kind: String,
    pub oid: String,
    pub size: Option<u64>,
    pub path: String,
}

pub(crate) fn parse_ls_tree(out: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut v = Vec::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let text = std::str::from_utf8(rec).map_err(|_| {
            rejected(super::po::t("gitops: a path in the commit is not UTF-8").into())
        })?;
        let (meta, path) = text
            .split_once('\t')
            .ok_or_else(|| rejected(super::po::t("gitops: unreadable tree listing").into()))?;
        let mut it = meta.split_whitespace();
        let (Some(mode), Some(kind), Some(oid), Some(size)) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            return Err(rejected(
                super::po::t("gitops: unreadable tree listing").into(),
            ));
        };
        v.push(TreeEntry {
            mode: mode.into(),
            kind: kind.into(),
            oid: oid.into(),
            size: size.parse().ok(),
            path: path.into(),
        });
    }
    Ok(v)
}

fn is_hex_oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Which entries of the commit's tree are read as manifest, refusing the
/// whole commit for anything that would leave the tree or the bounds.
///
/// Hidden paths (`.github/…`) are skipped: a repository's own CI files are
/// YAML too, and are not manifests.
pub(crate) fn select_manifests(entries: &[TreeEntry]) -> Result<Vec<TreeEntry>> {
    if entries.len() > MAX_TREE_ENTRIES {
        return Err(too_large(super::po::tf(
            "gitops: the path holds {n} entries — at most {max} are read",
            &[
                ("n", &entries.len().to_string()),
                ("max", &MAX_TREE_ENTRIES.to_string()),
            ],
        )));
    }
    let mut out = Vec::new();
    let mut total: u64 = 0;
    for e in entries {
        if e.path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        {
            return Err(rejected(super::po::tf(
                "gitops: '{path}' is not a plain path",
                &[("path", &e.path)],
            )));
        }
        match (e.mode.as_str(), e.kind.as_str()) {
            ("120000", _) => {
                return Err(rejected(super::po::tf(
                    "gitops: '{path}' is a symlink — a source never follows one (it could point outside the tree)",
                    &[("path", &e.path)],
                )))
            }
            ("160000", _) | (_, "commit") => {
                return Err(rejected(super::po::tf(
                    "gitops: '{path}' is a submodule — a source reads one repository, never the ones it points at",
                    &[("path", &e.path)],
                )))
            }
            ("100644" | "100755", "blob") => {}
            _ => {
                return Err(rejected(super::po::tf(
                    "gitops: '{path}' is not a regular file (mode {mode})",
                    &[("path", &e.path), ("mode", &e.mode)],
                )))
            }
        }
        if !is_hex_oid(&e.oid) {
            return Err(rejected(
                super::po::t("gitops: unreadable tree listing").into(),
            ));
        }
        let hidden = e.path.split('/').any(|c| c.starts_with('.'));
        let yaml = e.path.ends_with(".yaml") || e.path.ends_with(".yml");
        if hidden || !yaml {
            continue;
        }
        let size = e.size.unwrap_or(u64::MAX);
        if size > MAX_FILE_BYTES {
            return Err(too_large(super::po::tf(
                "gitops: '{path}' is larger than {max} bytes",
                &[("path", &e.path), ("max", &MAX_FILE_BYTES.to_string())],
            )));
        }
        total = total.saturating_add(size);
        out.push(e.clone());
    }
    if out.len() > MAX_MANIFEST_FILES || total > MAX_TOTAL_BYTES {
        return Err(too_large(super::po::tf(
            "gitops: {n} manifest files and {bytes} bytes — at most {files} files and {max} bytes are read",
            &[
                ("n", &out.len().to_string()),
                ("bytes", &total.to_string()),
                ("files", &MAX_MANIFEST_FILES.to_string()),
                ("max", &MAX_TOTAL_BYTES.to_string()),
            ],
        )));
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// What a manifest file may not be: an LFS pointer (the content is somewhere
/// else, and fetching it is out of scope) or a carrier of private key material.
pub(crate) fn scan_file(path: &str, text: &str) -> Result<()> {
    if text.starts_with("version https://git-lfs.github.com/spec/") {
        return Err(rejected(super::po::tf(
            "gitops: '{path}' is a Git LFS pointer — a source reads the manifest from the commit itself",
            &[("path", path)],
        )));
    }
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return Err(rejected(super::po::tf(
            "gitops: '{path}' carries private key material — a secret never comes from the repository; create it on the node and reference it by name",
            &[("path", path)],
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The pull-source guard (pure)
// ---------------------------------------------------------------------------

/// The ways a container asks for the host, read off the SAME [`RunOpts`] the
/// apply would run (`container::doc_run_opts`).
pub(crate) fn host_access_reasons(o: &RunOpts) -> Vec<&'static str> {
    let mut r = Vec::new();
    if o.privileged {
        r.push("privileged");
    }
    // The same reading admission makes (`container::cmd_run`): an empty or
    // `host` network is the node's own namespace — the default of a flat spec,
    // which is why a pulled container names a network of its own.
    if (o.net.is_empty() || o.net == "host") && o.pod.is_none() {
        r.push("host network");
    }
    if o.host_pid {
        r.push("host PID namespace");
    }
    if o.host_ipc {
        r.push("host IPC namespace");
    }
    if o.volumes.iter().any(|v| is_bind_source(v)) {
        r.push("a bind mount of a host path");
    }
    if !o.devices.is_empty() {
        r.push("a host device");
    }
    if o.gpus.is_some() {
        r.push("a GPU");
    }
    if !o.cap_add.is_empty() {
        r.push("an added capability");
    }
    let unconfined = |v: &Option<String>| v.as_deref() == Some("unconfined");
    if o.security_opt.iter().any(|s| s.contains("unconfined"))
        || unconfined(&o.apparmor)
        || unconfined(&o.selinux)
    {
        r.push("an unconfined security profile");
    }
    if !o.env_file.is_empty() {
        r.push("an env file read from a host path");
    }
    r
}

/// A `-v` source that is a path rather than a volume name.
fn is_bind_source(spec: &str) -> bool {
    let src = spec.split(':').next().unwrap_or_default();
    src.starts_with('/') || src.starts_with('.') || src.starts_with('~')
}

/// The Kinds a pass of this registration may apply.
fn kind_allowed(spec: &GitOpsSourceSpec, kind: &str) -> std::result::Result<(), String> {
    if HARD_REFUSED.contains(&kind) {
        return Err(super::po::tf(
            "{kind} is never applied from a pull source",
            &[("kind", kind)],
        ));
    }
    if spec.safety.kinds.is_empty() {
        if OPT_IN.contains(&kind) {
            return Err(super::po::tf(
                "{kind} is applied from a pull source only when spec.safety.kinds names it",
                &[("kind", kind)],
            ));
        }
        return Ok(());
    }
    if spec
        .safety
        .kinds
        .iter()
        .any(|k2| manifest::canonical_kind(k2) == kind)
    {
        Ok(())
    } else {
        Err(super::po::tf(
            "{kind} is not in spec.safety.kinds",
            &[("kind", kind)],
        ))
    }
}

fn names_any(v: &serde_yaml::Value, needles: &BTreeSet<&str>) -> bool {
    match v {
        serde_yaml::Value::String(s) => needles.contains(s.as_str()),
        serde_yaml::Value::Sequence(seq) => seq.iter().any(|x| names_any(x, needles)),
        serde_yaml::Value::Mapping(m) => m
            .iter()
            .any(|(k2, x)| names_any(k2, needles) || names_any(x, needles)),
        serde_yaml::Value::Tagged(t) => names_any(&t.value, needles),
        _ => false,
    }
}

/// Puts every always-namespaced document into the source's namespace, and
/// refuses one that names another: a source applies INTO its namespace and
/// never reaches into a neighbour's.
pub(crate) fn scope_documents(docs: &mut [ManifestDoc], namespace: &str) -> Vec<String> {
    let mut reasons = Vec::new();
    for d in docs.iter_mut() {
        let always = super::kinds::facts(&d.kind)
            .map(|f| f.namespaced == super::kinds::Namespaced::Always)
            .unwrap_or(false);
        if !always {
            continue;
        }
        let have = d
            .metadata
            .namespace
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "default".to_string());
        if d.metadata.namespace.as_deref().unwrap_or("").is_empty() {
            if namespace != "default" {
                d.metadata.namespace = Some(namespace.to_string());
            }
        } else if !have.eq_ignore_ascii_case(namespace) {
            reasons.push(super::po::tf(
                "{kind}/{name}: namespace '{have}' — this source applies only into '{ns}'",
                &[
                    ("kind", &d.kind),
                    ("name", &d.metadata.name),
                    ("have", &have),
                    ("ns", namespace),
                ],
            ));
        }
    }
    reasons
}

/// Every reason the documents of a commit cannot be applied by this source.
/// Empty means the guard lets them through to the plan.
pub(crate) fn guard_documents(
    docs: &[ManifestDoc],
    namespace: &str,
    name: &str,
    spec: &GitOpsSourceSpec,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if docs.len() > MAX_DOCUMENTS {
        reasons.push(super::po::tf(
            "{n} documents — at most {max} are applied from one commit",
            &[
                ("n", &docs.len().to_string()),
                ("max", &MAX_DOCUMENTS.to_string()),
            ],
        ));
        return reasons;
    }
    let creds: BTreeSet<&str> = [
        &spec.source.secret_ref,
        &spec.source.known_hosts_ref,
        &spec.source.trusted_keys_ref,
    ]
    .into_iter()
    .flatten()
    .map(String::as_str)
    .collect();
    for d in docs {
        let id = format!("{}/{}", d.kind, d.metadata.name);
        if d.kind == k::GITOPS_SOURCE {
            reasons.extend(guard_self(d, namespace, name, spec));
            continue;
        }
        if let Err(why) = kind_allowed(spec, &d.kind) {
            reasons.push(format!("{id}: {why}"));
            continue;
        }
        if !creds.is_empty() && names_any(&d.spec, &creds) {
            reasons.push(super::po::tf(
                "{id}: names the source's own credential — a workload never consumes the key that fetches its manifest",
                &[("id", &id)],
            ));
        }
        match d.kind.as_str() {
            k::IMAGE if d.spec.get("build").is_some() => reasons.push(super::po::tf(
                "{id}: an Image with build: runs the repository's code on the node — pull a built image instead",
                &[("id", &id)],
            )),
            k::VOLUME => {
                for key in VOLUME_HOST_KEYS {
                    if d.spec.get(*key).is_some() {
                        reasons.push(super::po::tf(
                            "{id}: spec.{key} reaches beyond a plain local volume",
                            &[("id", &id), ("key", key)],
                        ));
                    }
                }
            }
            k::NETWORK => {
                let driver = d
                    .spec
                    .get("driver")
                    .and_then(|v| v.as_str())
                    .unwrap_or("bridge");
                if driver != "bridge" || d.spec.get("peers").is_some() {
                    reasons.push(super::po::tf(
                        "{id}: only a bridge network on this node is applied from a pull source",
                        &[("id", &id)],
                    ));
                }
            }
            k::CONTAINER => match super::container::doc_run_opts(d) {
                Ok(o) => push_host_access(&mut reasons, &id, &o),
                Err(e) => reasons.push(format!("{id}: {e}")),
            },
            k::POD => match pod_members(d) {
                Ok(members) => {
                    for o in &members {
                        push_host_access(&mut reasons, &id, o);
                    }
                }
                Err(e) => reasons.push(format!("{id}: {e}")),
            },
            _ => {}
        }
    }
    reasons
}

fn push_host_access(reasons: &mut Vec<String>, id: &str, o: &RunOpts) {
    let found = host_access_reasons(o);
    if !found.is_empty() {
        reasons.push(super::po::tf(
            "{id}: asks for host access ({what}) — refused from a pull source",
            &[("id", id), ("what", &found.join(", "))],
        ));
    }
}

fn pod_members(d: &ManifestDoc) -> Result<Vec<RunOpts>> {
    let pod: delonix_compute::pod::PodSpec = manifest::spec_of(d)?;
    let mut notices = Vec::new();
    delonix_compute::pod::pod_member_run_opts(
        &d.metadata.name,
        d.metadata.namespace.clone(),
        pod,
        &super::pod::pod_netns_name(&d.metadata.name),
        &mut notices,
    )
}

/// A source declared in its own repository: the same identity, and nothing
/// changed but `ref`, `path` and `interval`.
fn guard_self(
    d: &ManifestDoc,
    namespace: &str,
    name: &str,
    spec: &GitOpsSourceSpec,
) -> Vec<String> {
    let id = format!("{}/{}", d.kind, d.metadata.name);
    if d.metadata.name != name || !namespace_of(d).eq_ignore_ascii_case(namespace) {
        return vec![super::po::tf(
            "{id}: a source declares only itself — another source in its tree is how one repository would acquire the node's others",
            &[("id", &id)],
        )];
    }
    let theirs = match manifest::spec_of::<GitOpsSourceSpec>(d) {
        Ok(s) => s,
        Err(e) => return vec![format!("{id}: {e}")],
    };
    let mine = gitops_fields(spec);
    let them = gitops_fields(&theirs);
    let changed: Vec<&str> = mine
        .iter()
        .filter(|(key, v)| !SELF_MUTABLE.contains(&key.as_str()) && them.get(*key) != Some(v))
        .map(|(key, _)| key.as_str())
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }
    vec![super::po::tf(
        "{id}: the repository may change only ref, path and interval of its own source — {fields} differ from the local registration (change them on the node)",
        &[("id", &id), ("fields", &changed.join(", "))],
    )]
}

// ---------------------------------------------------------------------------
// The safety policy over a plan (pure)
// ---------------------------------------------------------------------------

/// What a plan is allowed to do, and what it is not.
#[derive(Debug, Default)]
pub(crate) struct Decision {
    /// One line per change the policy refuses. Non-empty refuses the pass.
    pub blocked: Vec<String>,
    /// The `Delete`s that run.
    pub prune: Vec<Change>,
    /// `Delete`s left in place, and why.
    pub kept: Vec<String>,
    /// `Kind/name` of each allowed `Replace`, for `apply_docs`.
    pub replace: Vec<String>,
    /// `Kind/name` of each allowed `Reboot`.
    pub reboot: Vec<String>,
    /// Whether anything at all is to be done.
    pub changes: bool,
}

fn protected(spec: &GitOpsSourceSpec, c: &Change) -> bool {
    spec.safety.protected_resources.iter().any(|p| {
        p.split_once('/').is_some_and(|(kind, n)| {
            manifest::canonical_kind(kind) == c.kind && (n == "*" || n == c.name)
        })
    })
}

pub(crate) fn decide(changes: &[Change], spec: &GitOpsSourceSpec) -> Decision {
    let allowed = |a: &str| spec.safety.allow.iter().any(|x| x == a);
    let mut d = Decision::default();
    let mut deletes = Vec::new();
    for c in changes {
        let id = format!("{}/{}", c.kind, c.name);
        let rule = |action: &str| {
            super::po::tf(
                "{id}: {action} is not in spec.safety.allow",
                &[("id", &id), ("action", action)],
            )
        };
        match c.action {
            Action::NoOp | Action::NotConverged => {}
            Action::Create if !allowed("Create") => d.blocked.push(rule("Create")),
            Action::Update if !allowed("Update") => d.blocked.push(rule("Update")),
            Action::Adopt if !allowed("Adopt") => d.blocked.push(rule("Adopt")),
            Action::Replace if !allowed("Replace") => d.blocked.push(rule("Replace")),
            Action::Reboot if !allowed("Reboot") => d.blocked.push(rule("Reboot")),
            Action::Conflict => d.blocked.push(super::po::tf(
                "{id}: owned by another stack — a pass never takes it",
                &[("id", &id)],
            )),
            Action::Replace => d.replace.push(id),
            Action::Reboot => d.reboot.push(id),
            Action::Delete => deletes.push(c.clone()),
            Action::Create | Action::Update | Action::Adopt => {}
        }
        if c.action.is_change() && c.action != Action::Delete {
            d.changes = true;
        }
    }
    for c in deletes {
        let id = format!("{}/{}", c.kind, c.name);
        if c.kind == k::GITOPS_SOURCE {
            // A source never prunes a source — least of all itself: that would
            // switch off the reconciliation that would pull the revert.
            d.kept.push(super::po::tf(
                "{id}: a pass never prunes a GitOpsSource (remove one on the node)",
                &[("id", &id)],
            ));
        } else if !spec.safety.prune {
            d.kept.push(super::po::tf(
                "{id}: no longer declared — kept (spec.safety.prune is false)",
                &[("id", &id)],
            ));
        } else if protected(spec, &c) {
            d.kept.push(super::po::tf(
                "{id}: no longer declared — kept (spec.safety.protectedResources)",
                &[("id", &id)],
            ));
        } else {
            d.prune.push(c);
        }
    }
    if d.prune.len() > spec.safety.maximum_deletes as usize {
        d.blocked.push(super::po::tf(
            "the commit would prune {n} resource(s) — spec.safety.maximumDeletes is {max}",
            &[
                ("n", &d.prune.len().to_string()),
                ("max", &spec.safety.maximum_deletes.to_string()),
            ],
        ));
    }
    if !d.prune.is_empty() {
        d.changes = true;
    }
    d
}

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

/// An exclusive, non-blocking lock on one source. Dropped, it is released.
struct PassLock(#[allow(dead_code)] std::fs::File);

fn try_lock(path: &Path) -> Result<Option<PassLock>> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    // SAFETY: a valid, open fd; LOCK_NB makes it return instead of waiting.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(PassLock(f)));
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(None);
    }
    Err(e.into())
}

/// The credential files of one pass, removed when it ends.
struct PassFiles(Vec<PathBuf>);

impl Drop for PassFiles {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn write_private(dir: &Path, name: &str, body: &str) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let p = dir.join(name);
    let _ = std::fs::remove_file(&p);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&p)?;
    f.write_all(body.as_bytes())?;
    if !body.ends_with('\n') {
        f.write_all(b"\n")?;
    }
    Ok(p)
}

fn secret_value(root: &Path, secret: &str, key: &str) -> Result<String> {
    let s = delonix_state::SecretStore::open(root)?.load(secret)?;
    s.data.get(key).cloned().ok_or_else(|| {
        invalid_source(super::po::tf(
            "secret '{secret}' has no key '{key}'",
            &[("secret", secret), ("key", key)],
        ))
    })
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => total += dir_size(&e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

/// What one pass produced, for the status record and the event.
enum Outcome {
    Synced {
        commit: String,
        changed: bool,
    },
    Refused {
        commit: Option<String>,
        err: Error,
        result: &'static str,
    },
}

/// `delonix gitops reconcile` — one pass, then exit.
pub(crate) fn reconcile(namespace: &str, name: &str) -> Result<()> {
    let root = super::util::state_root();
    let rec = load_record(&root, namespace, name)?;
    let dir = source_dir(&root, namespace, name);
    let who = format!("{namespace}/{name}");
    if suspend_marker(&root, namespace, name).exists() {
        println!(
            "{}",
            super::po::tf(
                "gitops/{who}: suspended — nothing fetched (`delonix gitops resume {name}` lifts it)",
                &[("who", &who), ("name", name)],
            )
        );
        delonix_node::events::emit(&root, "gitops", "skip", &who, &who, Some("suspended"));
        return Ok(());
    }
    let Some(_lock) = try_lock(&dir.join("lock"))? else {
        println!(
            "{}",
            super::po::tf(
                "gitops/{who}: another pass is running — this one skipped",
                &[("who", &who)],
            )
        );
        delonix_node::events::emit(&root, "gitops", "skip", &who, &who, Some("locked"));
        return Ok(());
    };
    let previous = load_status(&root, namespace, name).and_then(|s| s.applied);
    let outcome = pass(&root, &dir, &rec);
    let (status, result) = match outcome {
        Outcome::Synced { commit, changed } => {
            let msg = if changed {
                super::po::t("applied").to_string()
            } else {
                super::po::t("in sync").to_string()
            };
            println!(
                "{}",
                super::po::tf(
                    "gitops/{who}: synced {commit} ({msg})",
                    &[("who", &who), ("commit", short(&commit)), ("msg", &msg)],
                )
            );
            delonix_node::events::emit(&root, "gitops", "sync", &commit, &who, Some(&msg));
            (
                PassStatus {
                    result: "synced".into(),
                    commit: Some(commit.clone()),
                    applied: Some(commit),
                    at_unix: now(),
                    message: msg,
                },
                Ok(()),
            )
        }
        Outcome::Refused {
            commit,
            err,
            result,
        } => {
            let message = err.to_string();
            delonix_node::events::emit(
                &root,
                "gitops",
                result,
                commit.as_deref().unwrap_or(&who),
                &who,
                Some(&message.chars().take(200).collect::<String>()),
            );
            (
                PassStatus {
                    result: result.into(),
                    commit,
                    applied: previous,
                    at_unix: now(),
                    message,
                },
                Err(err),
            )
        }
    };
    save_status(&root, namespace, name, &status);
    result
}

/// One pass under the lock. Every early return before `apply_docs` leaves the
/// machine exactly as it was.
fn pass(root: &Path, dir: &Path, rec: &SourceRecord) -> Outcome {
    let refused = |commit: Option<String>, err: Error, result: &'static str| Outcome::Refused {
        commit,
        err,
        result,
    };
    let (git, _files) = match prepare_git(root, dir, rec) {
        Ok(x) => x,
        Err(e) => return refused(None, e, "failed"),
    };
    let commit = match fetch_and_resolve(root, &git, dir, rec) {
        Ok(c) => c,
        Err(e) => {
            let result = if e.number() == 7601 {
                "rejected"
            } else {
                "failed"
            };
            return refused(None, e, result);
        }
    };
    let manifest_path = match read_commit(&git, dir, &commit, &rec.spec) {
        Ok(p) => p,
        Err(e) => return refused(Some(commit), e, "rejected"),
    };
    let mut docs = match manifest::load(&manifest_path) {
        Ok(d) => d,
        Err(e) => {
            return refused(
                Some(commit.clone()),
                rejected(super::po::tf(
                    "gitops: commit {commit} does not load: {err}",
                    &[("commit", short(&commit)), ("err", &e.to_string())],
                )),
                "rejected",
            )
        }
    };
    let mut reasons = scope_documents(&mut docs, &rec.namespace);
    reasons.extend(guard_documents(&docs, &rec.namespace, &rec.name, &rec.spec));
    reasons.extend(super::stack::validate_graph(&docs));
    if !reasons.is_empty() {
        for r in &reasons {
            eprintln!("  ✗ {r}");
        }
        return refused(
            Some(commit.clone()),
            rejected(super::po::tf(
                "gitops: commit {commit} refused — {n} problem(s), nothing was applied",
                &[
                    ("commit", short(&commit)),
                    ("n", &reasons.len().to_string()),
                ],
            )),
            "rejected",
        );
    }
    let stack = rec.spec.stack.clone().unwrap_or_else(|| rec.name.clone());
    let changes = match super::stack::plan_with_digests(&docs, &stack) {
        Ok(c) => c,
        Err(e) => {
            return refused(
                Some(commit.clone()),
                rejected(super::po::tf(
                    "gitops: commit {commit} does not plan: {err}",
                    &[("commit", short(&commit)), ("err", &e.to_string())],
                )),
                "rejected",
            )
        }
    };
    let decision = decide(&changes, &rec.spec);
    for kept in &decision.kept {
        println!("  · {kept}");
    }
    if !decision.blocked.is_empty() {
        for b in &decision.blocked {
            eprintln!("  ✗ {b}");
        }
        return refused(
            Some(commit.clone()),
            Error::coded(
                1604,
                Error::Invalid(super::po::tf(
                    "gitops: commit {commit} plans {n} action(s) the safety policy blocks — nothing was applied",
                    &[
                        ("commit", short(&commit)),
                        ("n", &decision.blocked.len().to_string()),
                    ],
                )),
            ),
            "blocked",
        );
    }
    if !decision.changes {
        return Outcome::Synced {
            commit,
            changed: false,
        };
    }
    // The plan the policy weighed is the plan that runs: a machine that moved
    // in between refuses the apply instead of carrying out something unread.
    let digests: Vec<String> = changes
        .iter()
        .filter_map(|c| c.plan_digest.clone())
        .collect();
    let applied = super::stack::refuse_stale_plan(&docs, &stack, &digests)
        .and_then(|()| {
            super::stack::apply_docs(
                &docs,
                &manifest_path,
                Some(&stack),
                decision.replace.clone(),
                decision.reboot.clone(),
                false,
                None,
            )
        })
        .and_then(|()| super::stack::prune(&decision.prune));
    match applied {
        Ok(()) => Outcome::Synced {
            commit,
            changed: true,
        },
        Err(e) => refused(Some(commit), e, "failed"),
    }
}

/// The git of one pass, with the source's credentials written for it.
fn prepare_git(root: &Path, dir: &Path, rec: &SourceRecord) -> Result<(Git, PassFiles)> {
    validate_spec(&rec.spec, &rec.namespace, &rec.name)?;
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let git = Git::locate(&home)?;
    let mut files = PassFiles(Vec::new());
    let git = match validate_url(&rec.spec.source.url)? {
        Transport::Ssh => {
            let (Some(key_ref), Some(kh_ref)) = (
                &rec.spec.source.secret_ref,
                &rec.spec.source.known_hosts_ref,
            ) else {
                return Err(invalid_source(
                    super::po::t(
                        "an ssh:// source needs secretRef (the key) and knownHostsRef (the pinned host keys) — a host key is never accepted on first sight",
                    )
                    .into(),
                ));
            };
            let key = write_private(&home, "identity", &secret_value(root, key_ref, "identity")?)?;
            files.0.push(key.clone());
            let kh = write_private(
                &home,
                "known_hosts",
                &secret_value(root, kh_ref, "known_hosts")?,
            )?;
            files.0.push(kh.clone());
            git.with_ssh(SshFiles {
                key,
                known_hosts: kh,
            })
        }
        _ => git,
    };
    Ok((git, files))
}

/// Fetches the ref into the mirror, resolves it to a commit and verifies it.
fn fetch_and_resolve(root: &Path, git: &Git, dir: &Path, rec: &SourceRecord) -> Result<String> {
    let mirror = dir.join("mirror.git");
    if !mirror.join("HEAD").exists() {
        let _ = std::fs::remove_dir_all(&mirror);
        let m = mirror.to_string_lossy().into_owned();
        git.run(&["init", "--bare", "--quiet", &m], 1 << 16)?;
    }
    let refspec = format!("+{}:refs/delonix/fetched", rec.spec.source.git_ref);
    let fetched = git.in_repo(
        &mirror,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--depth=1",
            "--end-of-options",
            &rec.spec.source.url,
            &refspec,
        ],
        1 << 20,
    );
    if let Err(e) = fetched {
        if e.number() == 8601 || e.number() == 6601 {
            return Err(e);
        }
        return Err(Error::coded(
            6602,
            Error::Unavailable(super::po::tf(
                "gitops: could not fetch {ref} from {url}: {err}",
                &[
                    ("ref", &rec.spec.source.git_ref),
                    ("url", &rec.spec.source.url),
                    ("err", &e.to_string()),
                ],
            )),
        ));
    }
    let size = dir_size(&mirror);
    if size > MAX_MIRROR_BYTES {
        let _ = std::fs::remove_dir_all(&mirror);
        return Err(too_large(super::po::tf(
            "gitops: the fetched repository is {bytes} bytes — at most {max} are kept (the mirror was removed)",
            &[
                ("bytes", &size.to_string()),
                ("max", &MAX_MIRROR_BYTES.to_string()),
            ],
        )));
    }
    let out = git.in_repo(
        &mirror,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            "refs/delonix/fetched^{commit}",
        ],
        256,
    )?;
    let commit = String::from_utf8_lossy(&out).trim().to_string();
    if !is_hex_oid(&commit) {
        return Err(rejected(
            super::po::t("gitops: the ref did not resolve to a commit").into(),
        ));
    }
    if rec.spec.source.signed_commits_required {
        verify_signature(root, git, &mirror, &commit, rec)?;
    }
    Ok(commit)
}

fn verify_signature(
    root: &Path,
    git: &Git,
    mirror: &Path,
    commit: &str,
    rec: &SourceRecord,
) -> Result<()> {
    let Some(tk) = &rec.spec.source.trusted_keys_ref else {
        return Err(invalid_source(
            super::po::t(
                "signedCommitsRequired and trustedKeysRef go together: one without the other would read as a protection that is not there",
            )
            .into(),
        ));
    };
    let home = mirror.parent().unwrap_or(mirror).join("home");
    let signers = write_private(
        &home,
        "allowed_signers",
        &secret_value(root, tk, "allowed_signers")?,
    )?;
    let _cleanup = PassFiles(vec![signers.clone()]);
    let opt = format!("gpg.ssh.allowedSignersFile={}", signers.display());
    git.in_repo(
        mirror,
        &["-c", "gpg.format=ssh", "-c", &opt, "verify-commit", commit],
        1 << 16,
    )
    .map(|_| ())
    .map_err(|e| {
        Error::coded(
            7601,
            Error::PermissionDenied(super::po::tf(
                "gitops: commit {commit} is not signed by a key in '{secret}' — nothing was read from it ({err})",
                &[
                    ("commit", short(commit)),
                    ("secret", tk),
                    ("err", &e.to_string()),
                ],
            )),
        )
    })
}

/// Reads the YAML under `path` straight from the commit's tree into one
/// manifest file. Nothing of the repository is checked out.
fn read_commit(git: &Git, dir: &Path, commit: &str, spec: &GitOpsSourceSpec) -> Result<PathBuf> {
    let mirror = dir.join("mirror.git");
    let treeish = if spec.source.path.is_empty() {
        commit.to_string()
    } else {
        format!("{commit}:{}", spec.source.path.trim_end_matches('/'))
    };
    let listing = git
        .in_repo(
            &mirror,
            &["ls-tree", "-r", "-l", "-z", "--end-of-options", &treeish],
            8 << 20,
        )
        .map_err(|e| {
            rejected(super::po::tf(
                "gitops: commit {commit} has no directory '{path}': {err}",
                &[
                    ("commit", short(commit)),
                    ("path", &spec.source.path),
                    ("err", &e.to_string()),
                ],
            ))
        })?;
    let selected = select_manifests(&parse_ls_tree(&listing)?)?;
    if selected.is_empty() {
        return Err(rejected(super::po::tf(
            "gitops: commit {commit} holds no manifest under '{path}' — an empty commit is never applied (it would read as «remove everything»)",
            &[("commit", short(commit)), ("path", &spec.source.path)],
        )));
    }
    let mut combined = String::new();
    for e in &selected {
        let bytes = git.in_repo(
            &mirror,
            &["cat-file", "blob", &e.oid],
            MAX_FILE_BYTES as usize,
        )?;
        let text = String::from_utf8(bytes).map_err(|_| {
            rejected(super::po::tf(
                "gitops: '{path}' is not UTF-8",
                &[("path", &e.path)],
            ))
        })?;
        scan_file(&e.path, &text)?;
        combined.push_str("---\n");
        combined.push_str(&text);
        if !text.ends_with('\n') {
            combined.push('\n');
        }
    }
    let commits = dir.join("commits");
    let _ = std::fs::remove_dir_all(&commits);
    std::fs::create_dir_all(&commits)?;
    let path = commits.join(format!("{commit}.yaml"));
    delonix_state::write_atomic_mode(&path, combined.as_bytes(), Some(0o600))
        .map_err(Error::from)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// The CLI
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum GitopsCmd {
    /// Run one pass of a source now: fetch, verify, plan, apply, then exit.
    ///
    /// A commit that does not load, fails the guard or the graph, or plans an
    /// action the safety policy blocks leaves the running stack untouched and
    /// is recorded with the commit it names (`gitops status`).
    Reconcile {
        /// The source's name.
        name: String,
        /// The source's namespace.
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
    },
    /// Show each source's last pass: the commit it looked at, the commit applied, and why.
    Status {
        /// One source; omitted, every source.
        name: Option<String>,
        /// Only this namespace.
        #[arg(short = 'n', long)]
        namespace: Option<String>,
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: OutputFormat,
    },
    /// Stop a source's passes without a commit (a local marker outside the spec).
    Suspend {
        /// The source's name.
        name: String,
        /// The source's namespace.
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
    },
    /// Lift a suspend: the next fire fetches again.
    Resume {
        /// The source's name.
        name: String,
        /// The source's namespace.
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
    },
}

pub fn run(action: GitopsCmd) -> Result<()> {
    let root = super::util::state_root();
    match action {
        GitopsCmd::Reconcile { name, namespace } => reconcile(&namespace, &name),
        GitopsCmd::Status {
            name,
            namespace,
            output,
        } => status(&root, name.as_deref(), namespace.as_deref(), output),
        GitopsCmd::Suspend { name, namespace } => {
            load_record(&root, &namespace, &name)?;
            std::fs::write(suspend_marker(&root, &namespace, &name), b"")?;
            delonix_node::events::emit(
                &root,
                "gitops",
                "suspend",
                &format!("{namespace}/{name}"),
                &name,
                None,
            );
            println!(
                "{}",
                super::po::tf(
                    "gitops/{ns}/{name}: suspended — passes fetch nothing until `delonix gitops resume {name}`",
                    &[("ns", &namespace), ("name", &name)],
                )
            );
            Ok(())
        }
        GitopsCmd::Resume { name, namespace } => {
            load_record(&root, &namespace, &name)?;
            match std::fs::remove_file(suspend_marker(&root, &namespace, &name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            delonix_node::events::emit(
                &root,
                "gitops",
                "resume",
                &format!("{namespace}/{name}"),
                &name,
                None,
            );
            println!(
                "{}",
                super::po::tf(
                    "gitops/{ns}/{name}: resumed",
                    &[("ns", &namespace), ("name", &name)],
                )
            );
            Ok(())
        }
    }
}

#[derive(serde::Serialize)]
struct StatusRow {
    namespace: String,
    name: String,
    url: String,
    #[serde(rename = "ref")]
    git_ref: String,
    timer: bool,
    suspended: bool,
    result: Option<String>,
    commit: Option<String>,
    applied: Option<String>,
    at_unix: Option<u64>,
    message: Option<String>,
    stack: Option<String>,
}

fn rows(root: &Path, name: Option<&str>, namespace: Option<&str>) -> Vec<StatusRow> {
    list_records(root)
        .into_iter()
        .filter(|r| name.is_none_or(|n| r.name == n))
        .filter(|r| namespace.is_none_or(|n| r.namespace == n))
        .map(|r| {
            let st = load_status(root, &r.namespace, &r.name);
            StatusRow {
                suspended: suspend_marker(root, &r.namespace, &r.name).exists(),
                timer: r.spec.sync.automated,
                url: r.spec.source.url.clone(),
                git_ref: r.spec.source.git_ref.clone(),
                result: st.as_ref().map(|s| s.result.clone()),
                commit: st.as_ref().and_then(|s| s.commit.clone()),
                applied: st.as_ref().and_then(|s| s.applied.clone()),
                at_unix: st.as_ref().map(|s| s.at_unix),
                message: st.map(|s| s.message),
                stack: r.labels.get(super::reconcile::STACK_LABEL).cloned(),
                namespace: r.namespace,
                name: r.name,
            }
        })
        .collect()
}

fn status(
    root: &Path,
    name: Option<&str>,
    namespace: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let rows = rows(root, name, namespace);
    if let (Some(n), true) = (name, rows.is_empty()) {
        return Err(not_found(namespace.unwrap_or("default"), n));
    }
    if format == OutputFormat::Json {
        return super::output::print_json(&rows);
    }
    let mut t = super::output::Table::new(&[
        "NAMESPACE",
        "NAME",
        "REF",
        "RESULT",
        "COMMIT",
        "APPLIED",
        "TIMER",
        "MESSAGE",
    ]);
    for r in &rows {
        let result = if r.suspended {
            super::po::t("suspended").to_string()
        } else {
            r.result.clone().unwrap_or_else(|| "-".into())
        };
        t.row(vec![
            r.namespace.clone(),
            r.name.clone(),
            r.git_ref.clone(),
            result,
            r.commit.as_deref().map(short).unwrap_or("-").to_string(),
            r.applied.as_deref().map(short).unwrap_or("-").to_string(),
            if r.timer { "yes".into() } else { "-".into() },
            r.message
                .as_deref()
                .map(|m| m.chars().take(80).collect())
                .unwrap_or_else(|| "-".into()),
        ]);
    }
    t.print();
    Ok(())
}

/// `delonix get gitopssources`.
pub(crate) fn cmd_ls(format: OutputFormat, namespace: Option<&str>) -> Result<()> {
    let format = super::config::resolve_output(&super::util::state_root(), format);
    status(&super::util::state_root(), None, namespace, format)
}

/// `delonix describe gitopssources <name>`.
pub(crate) fn cmd_describe(names: &[String]) -> Result<()> {
    let root = super::util::state_root();
    for given in names {
        let (ns, name) = resolve_name(&root, given)?;
        let rec = load_record(&root, &ns, &name)?;
        let st = load_status(&root, &ns, &name);
        let mut d = super::output::Describe::new();
        d.field("Name", &rec.name);
        d.field("Namespace", &rec.namespace);
        d.field("URL", &rec.spec.source.url);
        d.field("Ref", &rec.spec.source.git_ref);
        d.field(
            "Path",
            if rec.spec.source.path.is_empty() {
                "/"
            } else {
                &rec.spec.source.path
            },
        );
        d.field("Interval", &rec.spec.source.interval);
        d.field(
            "Signed commits",
            if rec.spec.source.signed_commits_required {
                "required"
            } else {
                "not required"
            },
        );
        d.field(
            "Timer",
            if rec.spec.sync.automated {
                unit_base(&rec.namespace, &rec.name) + ".timer"
            } else {
                "-".to_string()
            },
        );
        d.field("Allow", rec.spec.safety.allow.join(", "));
        d.field(
            "Prune",
            format!(
                "{} (at most {})",
                rec.spec.safety.prune, rec.spec.safety.maximum_deletes
            ),
        );
        d.field(
            "Stack",
            rec.spec.stack.clone().unwrap_or_else(|| rec.name.clone()),
        );
        d.field("State", status_word(&root, &ns, &name));
        if let Some(st) = st {
            d.field_opt("Applied", st.applied.as_deref());
            d.field("Last pass", st.message);
        }
        d.print();
    }
    Ok(())
}

#[cfg(test)]
mod tests;
