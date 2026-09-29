//! `kind: SystemContainer` — a whole userland run as one unit on a remote
//! provider, with VM-like semantics (ADR-0058, plan 63 slice 4). Not a
//! `kind: Container`: the provider offers no `exec`, logs or exit status, and
//! the engine's dataplane does not reach it.
//!
//! **The engine pulls the image, never the provider.** `spec.image` is
//! resolved through the engine's own pull (digest verified, `image login`
//! credentials), written as an archive with OCI media types
//! (`write_oci_media_archive`, slice 1), and handed to the port, which uploads
//! it once under its manifest digest (slice 2).
//!
//! **No `provider` field**, like `kind: NetworkZone`: the runtime's own
//! configuration (`DELONIX_PROXMOX_*`, or `providers.yaml`) decides which
//! node runs it. This module is the composition root that turns that
//! configuration into a `SystemContainerProvider`.
//!
//! **Its own registry** keeps the provider's locator and what was declared.
//! What the container IS — its memory, swap, cores, entrypoint and
//! environment — is read back from the provider on every plan, so a change
//! made on the node by hand is drift, not something the record hides.
//!
//! `memory`, `swap` and `cores` converge in place (measured on a running
//! container: the node writes them to its cgroup and cpuset). `rootfs`
//! converges in place when it grows (plan 63 slice 5) and plans a `Replace`
//! when it shrinks. `image`, `entrypoint`, `env` and `network` are cold:
//! changing one plans a `Replace`, refused without `--replace
//! SystemContainer/<name>`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::kinds as k;
use super::manifest::{self, ManifestDoc};
use super::output::OutputFormat;
use super::po;
use super::util::state_root;
use delonix_compute::system_container::{
    NetworkState, SystemContainerHandle, SystemContainerNet, SystemContainerProvider,
    SystemContainerResources, SystemContainerSpec,
};
use delonix_model::{Error, Result};
use delonix_state::JsonStore;

/// `spec` of `kind: SystemContainer`.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SystemContainerSpecDoc {
    /// The OCI image, as `image pull` takes it (`alpine:3.20`,
    /// `registry/repo@sha256:…`). Pulled by the engine.
    pub image: String,
    /// The command to run as init. Empty keeps the image's.
    #[serde(default)]
    pub entrypoint: Vec<String>,
    /// The runtime environment. Empty keeps the image's; non-empty replaces
    /// it whole.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `256M`, `1G`, `2Gi`.
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default = "default_swap")]
    pub swap: String,
    #[serde(default = "default_cores")]
    pub cores: u32,
    /// Size of the root filesystem, in GiB.
    #[serde(default = "default_rootfs")]
    pub rootfs: u32,
    /// A network interface on a bridge of the node. Absent: none.
    #[serde(default)]
    pub network: Option<SystemContainerNetDoc>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct SystemContainerNetDoc {
    pub bridge: String,
    #[serde(default)]
    pub vlan: Option<u16>,
    #[serde(default = "default_true")]
    pub dhcp: bool,
}

fn default_memory() -> String {
    "512M".into()
}
fn default_swap() -> String {
    "0".into()
}
fn default_cores() -> u32 {
    1
}
fn default_rootfs() -> u32 {
    4
}
fn default_true() -> bool {
    true
}

/// Known fields of the `spec` (drift-guard).
pub const SYSTEM_CONTAINER_SPEC_FIELDS: &[&str] = &[
    "image",
    "entrypoint",
    "env",
    "memory",
    "swap",
    "cores",
    "rootfs",
    "network",
];

/// Fields the reconciler compares.
pub const RECONCILED_SYSTEM_CONTAINER_FIELDS: &[&str] = SYSTEM_CONTAINER_SPEC_FIELDS;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct SystemContainerRecord {
    name: String,
    /// The provider's locator (`proxmox:<node>:<vmid>`).
    locator: String,
    image: String,
    manifest_digest: String,
    #[serde(default)]
    entrypoint: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    rootfs_gib: u32,
    #[serde(default)]
    network: Option<SystemContainerNetDoc>,
    /// The network verdict of the last start, in words.
    #[serde(default)]
    network_state: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

fn store() -> Result<JsonStore<SystemContainerRecord>> {
    JsonStore::open(state_root().join("system-containers")).map_err(Into::into)
}

/// The container's own state directory, where the provider keeps its task
/// ledger. Beside the registry, not inside it: the registry lists `*.json`.
fn ledger_dir(name: &str) -> PathBuf {
    state_root().join("system-containers.d").join(name)
}

/// The provider the runtime's configuration names. The one place this Kind
/// meets a concrete provider (ADR-0044: selection lives in a composition root).
fn resolve_provider() -> Result<Box<dyn SystemContainerProvider>> {
    let lookup = super::vmbackends::configured_lookup()?;
    let Some((target, opts)) = super::vmbackends::proxmox_target_with(&*lookup)? else {
        return Err(Error::Unavailable(
            po::t(
                "no system-container provider is configured: set DELONIX_PROXMOX_URL, \
             DELONIX_PROXMOX_NODE and a credential, or a proxmox entry in providers.yaml",
            )
            .to_string(),
        ));
    };
    let template = target
        .import_storage
        .clone()
        .unwrap_or_else(|| "local".into());
    let rootfs = target
        .disk_storage
        .clone()
        .unwrap_or_else(|| "local-lvm".into());
    let client = delonix_proxmox::Client::connect_with(&target, opts)?;
    Ok(Box::new(
        delonix_proxmox::ProxmoxSystemContainerProvider::new(Arc::new(client), &template, &rootfs),
    ))
}

fn mib(field: &str, raw: &str) -> Result<u32> {
    // `0` is a size here (no swap); the shared parser reads sizes of memory,
    // where zero is never one.
    if raw.trim() == "0" {
        return Ok(0);
    }
    let v = delonix_compute::vm_backend::parse_mem_mib(raw).ok_or_else(|| {
        Error::Invalid(po::tf(
            "systemcontainer: {field} '{raw}' is not a size (256M, 1G, 2Gi)",
            &[("field", field), ("raw", raw)],
        ))
    })?;
    u32::try_from(v).map_err(|_| {
        Error::Invalid(po::tf(
            "systemcontainer: {field} '{raw}' is too large",
            &[("field", field), ("raw", raw)],
        ))
    })
}

/// The resources a spec asks for, parsed once.
fn resources_of(spec: &SystemContainerSpecDoc) -> Result<SystemContainerResources> {
    Ok(SystemContainerResources {
        memory_mib: mib("memory", &spec.memory)?,
        swap_mib: mib("swap", &spec.swap)?,
        cores: spec.cores,
    })
}

fn list_field(items: &[String]) -> String {
    serde_json::to_string(items).unwrap_or_default()
}

fn env_field(env: &BTreeMap<String, String>) -> String {
    let items: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    list_field(&items)
}

fn network_field(net: &Option<SystemContainerNetDoc>) -> String {
    match net {
        None => String::new(),
        Some(n) => format!(
            "bridge={},vlan={},dhcp={}",
            n.bridge,
            n.vlan.map(|v| v.to_string()).unwrap_or_default(),
            n.dhcp
        ),
    }
}

/// The comparable fields of a spec, keyed by manifest name. `entrypoint` and
/// `env` are present only when declared: left out, the image's own are on the
/// node, and comparing those with "nothing" would be drift on every plan.
fn spec_fields(spec: &SystemContainerSpecDoc) -> Result<BTreeMap<String, String>> {
    let r = resources_of(spec)?;
    let mut f = BTreeMap::new();
    f.insert("image".into(), spec.image.clone());
    if !spec.entrypoint.is_empty() {
        f.insert("entrypoint".into(), list_field(&spec.entrypoint));
    }
    if !spec.env.is_empty() {
        f.insert("env".into(), env_field(&spec.env));
    }
    f.insert("memory".into(), r.memory_mib.to_string());
    f.insert("swap".into(), r.swap_mib.to_string());
    f.insert("cores".into(), r.cores.to_string());
    f.insert("rootfs".into(), spec.rootfs.to_string());
    f.insert("network".into(), network_field(&spec.network));
    Ok(f)
}

/// Refuses, by name, the fields that would ask for privilege or nesting.
///
/// The Kind has no such field on purpose, and the generic «unknown field —
/// ignored» warning would be the worst answer here: a manifest that says
/// `unprivileged: false` gets an unprivileged container with exit 0, the
/// opposite of what it asked. A privileged container on a remote node is a
/// decision of its own (ADR-0058), so the request fails before anything is
/// pulled or created.
fn reject_privilege(doc: &ManifestDoc) -> Result<()> {
    const NOT_HERE: &[&str] = &["unprivileged", "privileged", "features", "nesting"];
    let serde_yaml::Value::Mapping(map) = &doc.spec else {
        return Ok(());
    };
    for field in NOT_HERE {
        if map.contains_key(serde_yaml::Value::String((*field).to_string())) {
            return Err(Error::coded(
                1540,
                Error::Invalid(po::tf(
                    "systemcontainer/{name}: `{field}` is refused — a system container is \
                     always unprivileged and without nesting; privilege on a remote node is a \
                     decision of its own (ADR-0058)",
                    &[("name", &doc.metadata.name), ("field", field)],
                )),
            ));
        }
    }
    Ok(())
}

pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    reject_privilege(doc)?;
    let spec: SystemContainerSpecDoc = manifest::spec_of(doc)?;
    Ok(super::reconcile::Desired {
        kind: k::SYSTEM_CONTAINER.into(),
        name: doc.metadata.name.clone(),
        fields: spec_fields(&spec)?,
        converges: true,
        ownable: true,
    })
}

fn handle_of(rec: &SystemContainerRecord) -> SystemContainerHandle {
    SystemContainerHandle {
        name: rec.name.clone(),
        locator: rec.locator.clone(),
    }
}

/// Every registered container, with its fields read back from the provider.
/// A container the provider no longer has is left out: the plan then offers
/// to create it again. Nothing registered: the provider is never contacted.
pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    let recs = store()?.list()?;
    if recs.is_empty() {
        return Ok(Vec::new());
    }
    let provider = resolve_provider()?;
    let mut out = Vec::new();
    for rec in recs {
        let Some(live) = provider.configuration(&ledger_dir(&rec.name), &handle_of(&rec))? else {
            continue;
        };
        let mut fields = BTreeMap::new();
        fields.insert("image".into(), rec.image.clone());
        if !rec.entrypoint.is_empty() {
            fields.insert("entrypoint".into(), list_field(&live.entrypoint));
        }
        if !rec.env.is_empty() {
            let env: BTreeMap<String, String> = live.env.into_iter().collect();
            fields.insert("env".into(), env_field(&env));
        }
        fields.insert("memory".into(), live.memory_mib.to_string());
        fields.insert("swap".into(), live.swap_mib.to_string());
        fields.insert("cores".into(), live.cores.to_string());
        // The node's size, not the record's: a volume grown by hand on the
        // node is drift. A size the provider could not read (0) falls back to
        // the record, so an unreadable answer is never a planned change.
        let rootfs = if live.rootfs_gib > 0 {
            live.rootfs_gib
        } else {
            rec.rootfs_gib
        };
        fields.insert("rootfs".into(), rootfs.to_string());
        fields.insert("network".into(), network_field(&rec.network));
        out.push(super::reconcile::Actual {
            kind: k::SYSTEM_CONTAINER.into(),
            name: rec.name.clone(),
            fields,
            owner: rec.labels.get(super::reconcile::STACK_LABEL).cloned(),
            last_applied: rec
                .annotations
                .get(super::reconcile::LAST_APPLIED)
                .and_then(|raw| super::reconcile::decode_last_applied(raw)),
        });
    }
    Ok(out)
}

/// The image as an archive with OCI media types, pulled by the engine.
/// Returns the archive's path and its manifest digest. One file per manifest
/// digest, kept as a cache.
fn image_archive(image: &str) -> Result<(PathBuf, String)> {
    let images = delonix_oci::ImageStore::open(state_root())?;
    let img = super::util::resolve_or_pull(&images, image)?;
    let dir = state_root().join("system-containers.d").join("archives");
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{}.tar.tmp", std::process::id()));
    let written = delonix_oci::write_oci_media_archive(&images, &img, image, &tmp)?;
    let hex = written
        .manifest_digest
        .strip_prefix("sha256:")
        .unwrap_or(&written.manifest_digest)
        .to_string();
    let path = dir.join(format!("{hex}.tar"));
    std::fs::rename(&tmp, &path)?;
    Ok((path, written.manifest_digest))
}

fn port_spec(
    name: &str,
    spec: &SystemContainerSpecDoc,
    archive: PathBuf,
    manifest_digest: String,
) -> Result<SystemContainerSpec> {
    let r = resources_of(spec)?;
    Ok(SystemContainerSpec {
        name: name.to_string(),
        archive,
        manifest_digest,
        entrypoint: spec.entrypoint.clone(),
        env: spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        memory_mib: r.memory_mib,
        swap_mib: r.swap_mib,
        cores: r.cores,
        rootfs_gib: spec.rootfs,
        network: spec.network.as_ref().map(|n| SystemContainerNet {
            bridge: n.bridge.clone(),
            vlan: n.vlan,
            dhcp: n.dhcp,
        }),
        unprivileged: true,
    })
}

fn network_words(state: &NetworkState) -> String {
    match state {
        NetworkState::NotRequested => "none".into(),
        NetworkState::Ready { ipv4 } => ipv4.clone(),
        NetworkState::NotReady { reason } => format!("not ready: {reason}"),
        NetworkState::Unknown => "unknown".into(),
    }
}

/// The cold fields of a spec, for the refusal of an in-place change.
/// The fields of `now` that changed from `had` and cannot converge live —
/// the same rule the reconciler plans with (`is_hot_change`), so `apply` and
/// `plan` never disagree about what needs a recreate.
fn cold_changes(had: &BTreeMap<String, String>, now: &BTreeMap<String, String>) -> Vec<String> {
    now.iter()
        .filter(|(k, v)| had.get(*k) != Some(v))
        .filter(|(k, v)| {
            !super::reconcile::is_hot_change(
                super::kinds::SYSTEM_CONTAINER,
                k,
                had.get(*k).map(String::as_str),
                Some(v),
            )
        })
        .map(|(k, _)| k.clone())
        .collect()
}

fn record_fields(rec: &SystemContainerRecord) -> Result<BTreeMap<String, String>> {
    let spec = SystemContainerSpecDoc {
        image: rec.image.clone(),
        entrypoint: rec.entrypoint.clone(),
        env: rec.env.clone(),
        memory: default_memory(),
        swap: default_swap(),
        cores: default_cores(),
        rootfs: rec.rootfs_gib,
        network: rec.network.clone(),
    };
    spec_fields(&spec)
}

/// Applies one document: creates and starts the container when there is none,
/// converges `memory`/`swap`/`cores` when there is. A cold field that differs
/// is refused here and planned as a `Replace` by `stack plan`.
fn apply_one(doc: &ManifestDoc) -> Result<()> {
    reject_privilege(doc)?;
    let spec: SystemContainerSpecDoc = manifest::spec_of(doc)?;
    let name = doc.metadata.name.clone();
    if spec.image.trim().is_empty() {
        return Err(Error::Invalid(po::tf(
            "systemcontainer/{name}: spec.image is empty",
            &[("name", &name)],
        )));
    }
    let wanted = spec_fields(&spec)?;
    let provider = resolve_provider()?;
    let s = store()?;
    let dir = ledger_dir(&name);
    std::fs::create_dir_all(&dir)?;

    if let Ok(rec) = s.load(&name) {
        let h = handle_of(&rec);
        if provider.configuration(&dir, &h)?.is_some() {
            let live = provider.configuration(&dir, &h)?;
            let mut had = record_fields(&rec)?;
            if let Some(gib) = live.as_ref().map(|c| c.rootfs_gib).filter(|g| *g > 0) {
                had.insert("rootfs".into(), gib.to_string());
            }
            let changed = cold_changes(&had, &wanted);
            if !changed.is_empty() {
                return Err(Error::Invalid(po::tf(
                    "systemcontainer/{name}: {fields} cannot change in place — use `stack apply \
                     --replace SystemContainer/{name}` to recreate it",
                    &[
                        ("name", &name),
                        (
                            "fields",
                            &changed
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", "),
                        ),
                    ],
                )));
            }
            provider.resize(&dir, &h, resources_of(&spec)?)?;
            if had.get("rootfs") != Some(&spec.rootfs.to_string()) {
                provider.grow_rootfs(&dir, &h, spec.rootfs)?;
                s.update(&name, |r| {
                    r.rootfs_gib = spec.rootfs;
                    true
                })?;
            }
            println!(
                "{}",
                po::tf("systemcontainer/{name}: converged", &[("name", &name)])
            );
            return Ok(());
        }
        println!(
            "{}",
            po::tf(
                "systemcontainer/{name}: the provider no longer has it ({locator}) — creating it again",
                &[("name", &name), ("locator", &rec.locator)]
            )
        );
    }

    let (archive, digest) = image_archive(&spec.image)?;
    let pspec = port_spec(&name, &spec, archive, digest.clone())?;
    let h = provider.create(&dir, &pspec)?;
    // Write-ahead: the locator is saved before the start, so a start that
    // fails leaves a record a teardown can find.
    let mut rec = s.load(&name).unwrap_or_default();
    rec.name = name.clone();
    rec.locator = h.locator.clone();
    rec.image = spec.image.clone();
    rec.manifest_digest = digest;
    rec.entrypoint = spec.entrypoint.clone();
    rec.env = spec.env.clone();
    rec.rootfs_gib = spec.rootfs;
    rec.network = spec.network.clone();
    s.save(&name, &rec)?;

    let obs = provider.start(&dir, &h, &pspec)?;
    rec.network_state = network_words(&obs.network);
    s.save(&name, &rec)?;
    println!(
        "{}",
        po::tf(
            "systemcontainer/{name}: running on {locator}, network: {network}",
            &[
                ("name", &name),
                ("locator", &h.locator),
                ("network", &rec.network_state)
            ]
        )
    );
    if let NetworkState::NotReady { reason } = &obs.network {
        eprintln!(
            "{}",
            po::tf(
                "warning: systemcontainer/{name} is running without the address it asked for: {reason}",
                &[("name", &name), ("reason", reason)]
            )
        );
    }
    Ok(())
}

/// Applies every `kind: SystemContainer` document.
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, k::SYSTEM_CONTAINER) {
        apply_one(doc)?;
    }
    Ok(())
}

/// `converge_and_stamp`'s live-update path: the hot fields are the only ones
/// an `Update` can carry, and `apply_one` resizes them in place.
pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    apply_one(doc)
}

pub(crate) fn stamp(name: &str, stack: &str, fields: &BTreeMap<String, String>) -> Result<()> {
    let s = store()?;
    let mut rec = s
        .load(name)
        .map_err(|_| Error::NotFound(format!("system container: {name}")))?;
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

/// Teardown (`--replace`, `--prune`, `destroy`, `delete systemcontainers`):
/// destroys the container and its volume on the provider, then drops the
/// record. A name with no record is not an error.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    let s = store()?;
    let Ok(rec) = s.load(name) else {
        return Ok(());
    };
    let provider = resolve_provider()?;
    let dir = ledger_dir(name);
    std::fs::create_dir_all(&dir)?;
    provider.destroy(&dir, &handle_of(&rec))?;
    s.remove(name)?;
    println!(
        "{}",
        po::tf(
            "systemcontainer/{name}: destroyed ({locator})",
            &[("name", name), ("locator", &rec.locator)]
        )
    );
    Ok(())
}

/// For `stack ls`/`describe`: from the registry, without contacting the
/// provider.
pub(crate) fn presence_of(doc: &ManifestDoc) -> (String, String) {
    let Some(rec) = store().ok().and_then(|s| s.load(&doc.metadata.name).ok()) else {
        return ("no".into(), "-".into());
    };
    ("yes".into(), rec.locator)
}

/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: SystemContainerSpecDoc = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

#[derive(serde::Serialize)]
struct SystemContainerLsRow {
    name: String,
    image: String,
    locator: String,
    network: String,
    stack: Option<String>,
}

pub(crate) fn cmd_ls(format: OutputFormat) -> Result<()> {
    let format = super::config::resolve_output(&state_root(), format);
    let mut recs = store()?.list()?;
    recs.sort_by(|a, b| a.name.cmp(&b.name));
    let rows: Vec<SystemContainerLsRow> = recs
        .iter()
        .map(|r| SystemContainerLsRow {
            name: r.name.clone(),
            image: r.image.clone(),
            locator: r.locator.clone(),
            network: r.network_state.clone(),
            stack: r.labels.get(super::reconcile::STACK_LABEL).cloned(),
        })
        .collect();
    if format == OutputFormat::Json {
        return super::output::print_json(&rows);
    }
    let mut t = super::output::Table::new(&["NAME", "IMAGE", "LOCATOR", "NETWORK", "STACK"]);
    for r in &rows {
        t.row(vec![
            r.name.clone(),
            r.image.clone(),
            r.locator.clone(),
            if r.network.is_empty() {
                "-".into()
            } else {
                r.network.clone()
            },
            r.stack.clone().unwrap_or_else(|| "-".to_string()),
        ]);
    }
    t.drop_uninformative().print();
    Ok(())
}

/// `describe`: the record, and what the provider holds now.
/// `delonix systemcontainer`: the one-off operations a manifest cannot say
/// (plan 63 slice 5; the owner revised decision D3 so they have a home). What
/// a manifest CAN say stays in `kind: SystemContainer`.
#[derive(clap::Subcommand)]
pub enum SystemContainerCmd {
    /// Snapshots of a system container's root volume.
    Snapshot {
        #[command(subcommand)]
        action: SnapshotCmd,
    },
}

/// The same four verbs, in the same order, as `vm snapshot` and `volume
/// snapshot`.
#[derive(clap::Subcommand)]
pub enum SnapshotCmd {
    /// Take a named snapshot of the root volume (a container has no memory
    /// state to keep).
    Create {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::system_containers))]
        name: String,
        /// Snapshot name.
        snapshot: String,
    },
    /// List the container's snapshots.
    Ls {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::system_containers))]
        name: String,
    },
    /// Delete a snapshot.
    Rm {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::system_containers))]
        name: String,
        /// Snapshot name (see `systemcontainer snapshot ls`).
        snapshot: String,
    },
    /// Roll the container back to a snapshot. A running container comes
    /// back running, a stopped one stays stopped.
    Restore {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::system_containers))]
        name: String,
        /// Snapshot name to roll back to.
        snapshot: String,
    },
}

/// The registered names, for completion.
pub(crate) fn registered_names() -> Vec<String> {
    let Ok(s) = store() else {
        return Vec::new();
    };
    s.list()
        .map(|v| v.into_iter().map(|r| r.name).collect())
        .unwrap_or_default()
}

fn record(name: &str) -> Result<SystemContainerRecord> {
    store()?
        .load(name)
        .map_err(|_| Error::NotFound(format!("system container: {name}")))
}

pub fn run(cmd: SystemContainerCmd) -> Result<()> {
    let SystemContainerCmd::Snapshot { action } = cmd;
    match action {
        SnapshotCmd::Create { name, snapshot } => {
            let rec = record(&name)?;
            resolve_provider()?.snapshot(&ledger_dir(&name), &handle_of(&rec), &snapshot)?;
            println!("{snapshot}");
        }
        SnapshotCmd::Ls { name } => {
            let rec = record(&name)?;
            for s in resolve_provider()?.snapshots(&ledger_dir(&name), &handle_of(&rec))? {
                println!("{s}");
            }
        }
        SnapshotCmd::Rm { name, snapshot } => {
            let rec = record(&name)?;
            resolve_provider()?.delete_snapshot(&ledger_dir(&name), &handle_of(&rec), &snapshot)?;
            println!("{snapshot}");
        }
        SnapshotCmd::Restore { name, snapshot } => {
            let rec = record(&name)?;
            resolve_provider()?.restore(&ledger_dir(&name), &handle_of(&rec), &snapshot)?;
            println!("{name}");
        }
    }
    Ok(())
}

/// The provider storage a backup goes to or is read from. `.` — the backup
/// group's default, which means «this directory» for the local kinds — means
/// `DELONIX_PROXMOX_BACKUP_STORAGE`, else `local`.
pub(crate) fn backup_storage(to: &str) -> String {
    if to != "." {
        return to.to_string();
    }
    std::env::var("DELONIX_PROXMOX_BACKUP_STORAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".into())
}

/// Whether `s` names an archive on a provider (`<storage>:backup/vzdump-lxc-…`)
/// rather than a file here.
pub(crate) fn is_node_archive(s: &str) -> bool {
    s.split_once(':')
        .is_some_and(|(_, rest)| rest.starts_with("backup/vzdump-lxc-"))
}

fn locator_vmid(locator: &str) -> Option<u32> {
    locator.rsplit(':').next()?.parse().ok()
}

fn archive_vmid(archive: &str) -> Option<u32> {
    archive
        .rsplit('/')
        .next()?
        .strip_prefix("vzdump-lxc-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// The registered container an archive belongs to, by the container id its
/// name carries.
pub(crate) fn backup_owner(archive: &str) -> Result<String> {
    let vmid = archive_vmid(archive).ok_or_else(|| {
        Error::Invalid(po::tf(
            "'{archive}' is not a system container archive",
            &[("archive", archive)],
        ))
    })?;
    store()?
        .list()?
        .into_iter()
        .find(|r| locator_vmid(&r.locator) == Some(vmid))
        .map(|r| r.name)
        .ok_or_else(|| {
            Error::NotFound(format!(
                "system container for archive {archive} (container id {vmid} is not registered here)"
            ))
        })
}

pub(crate) fn locator_of(name: &str) -> Result<String> {
    Ok(record(name)?.locator)
}

pub(crate) fn backup_create(name: &str, storage: &str, stop: bool) -> Result<String> {
    let rec = record(name)?;
    resolve_provider()?.backup(&ledger_dir(name), &handle_of(&rec), storage, stop)
}

/// Deletes the oldest archives of `name` on `storage` beyond `keep`; the
/// archive names carry the date, so their order is the order they were taken.
pub(crate) fn backup_prune(name: &str, storage: &str, keep: usize) -> Result<Vec<String>> {
    let rec = record(name)?;
    let p = resolve_provider()?;
    let dir = ledger_dir(name);
    let h = handle_of(&rec);
    let all = p.backups(&dir, &h, storage)?;
    let over = all.len().saturating_sub(keep);
    let mut gone = Vec::new();
    for (archive, _) in all.into_iter().take(over) {
        p.delete_backup(&dir, &h, &archive)?;
        gone.push(archive);
    }
    Ok(gone)
}

/// `(container, archive, bytes)` for one container, or for every registered one.
pub(crate) fn backup_list(name: Option<&str>, storage: &str) -> Result<Vec<(String, String, u64)>> {
    let recs = match name {
        Some(n) => vec![record(n)?],
        None => store()?.list()?,
    };
    if recs.is_empty() {
        return Ok(Vec::new());
    }
    let p = resolve_provider()?;
    let mut out = Vec::new();
    for rec in recs {
        for (archive, bytes) in p.backups(&ledger_dir(&rec.name), &handle_of(&rec), storage)? {
            out.push((rec.name.clone(), archive, bytes));
        }
    }
    Ok(out)
}

/// The owner and size of one provider archive.
pub(crate) fn backup_describe(archive: &str) -> Result<(String, u64)> {
    let name = backup_owner(archive)?;
    let storage = archive.split_once(':').map(|(s, _)| s).unwrap_or_default();
    backup_list(Some(&name), storage)?
        .into_iter()
        .find(|(_, a, _)| a == archive)
        .map(|(n, _, b)| (n, b))
        .ok_or_else(|| Error::NotFound(format!("archive {archive}")))
}

pub(crate) fn backup_delete(archive: &str) -> Result<String> {
    let name = backup_owner(archive)?;
    let rec = record(&name)?;
    resolve_provider()?.delete_backup(&ledger_dir(&name), &handle_of(&rec), archive)?;
    Ok(name)
}

/// Restores `name` from `archive`; answers the settings the provider did not
/// put back.
pub(crate) fn backup_restore(name: &str, archive: &str) -> Result<Vec<String>> {
    let rec = record(name)?;
    resolve_provider()?.restore_backup(&ledger_dir(name), &handle_of(&rec), archive)
}

/// Whether the provider reads the container as running.
pub(crate) fn is_running(name: &str) -> Result<bool> {
    let rec = record(name)?;
    let spec = SystemContainerSpecDoc {
        image: rec.image.clone(),
        entrypoint: rec.entrypoint.clone(),
        env: rec.env.clone(),
        memory: default_memory(),
        swap: default_swap(),
        cores: default_cores(),
        rootfs: rec.rootfs_gib,
        network: rec.network.clone(),
    };
    let port = port_spec(name, &spec, PathBuf::new(), rec.manifest_digest.clone())?;
    Ok(resolve_provider()?
        .observe(&ledger_dir(name), &handle_of(&rec), &port)?
        .running)
}

pub(crate) fn cmd_describe(names: &[String]) -> Result<()> {
    let s = store()?;
    for name in names {
        let rec = s
            .load(name)
            .map_err(|_| Error::NotFound(format!("system container: {name}")))?;
        let mut d = super::output::Describe::new();
        d.field("Name", &rec.name);
        d.field("Image", &rec.image);
        d.field("Digest", &rec.manifest_digest);
        d.field("Locator", &rec.locator);
        d.field(
            "Network",
            if rec.network_state.is_empty() {
                "-"
            } else {
                &rec.network_state
            },
        );
        match resolve_provider().and_then(|p| p.configuration(&ledger_dir(name), &handle_of(&rec)))
        {
            Ok(Some(live)) => {
                d.field("Memory", format!("{} MiB", live.memory_mib));
                d.field("Swap", format!("{} MiB", live.swap_mib));
                d.field("Cores", live.cores.to_string());
                d.field("Entrypoint", live.entrypoint.join(" "));
            }
            Ok(None) => {
                d.field("State", po::t("gone from the provider"));
            }
            Err(e) => {
                d.field(
                    "State",
                    po::tf("not read: {err}", &[("err", &e.to_string())]),
                );
            }
        }
        d.field_opt("Stack", rec.labels.get(super::reconcile::STACK_LABEL));
        d.print();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SystemContainerSpecDoc {
        serde_yaml::from_str("image: alpine:3.20\n").unwrap()
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let s = spec();
        assert_eq!(
            (s.memory.as_str(), s.swap.as_str(), s.cores, s.rootfs),
            ("512M", "0", 1, 4)
        );
        assert!(s.entrypoint.is_empty() && s.env.is_empty() && s.network.is_none());
    }

    /// An undeclared entrypoint/env is not compared: the image's own are on
    /// the node, and «nothing» against them would be drift on every plan.
    #[test]
    fn undeclared_entrypoint_and_env_are_left_out_of_the_fields() {
        let f = spec_fields(&spec()).unwrap();
        assert!(!f.contains_key("entrypoint") && !f.contains_key("env"));
        assert_eq!(f["memory"], "512");
        let mut s = spec();
        s.entrypoint = vec!["/bin/sleep".into(), "3600".into()];
        s.env.insert("B".into(), "2".into());
        s.env.insert("A".into(), "1".into());
        let f = spec_fields(&s).unwrap();
        assert_eq!(f["entrypoint"], r#"["/bin/sleep","3600"]"#);
        assert_eq!(f["env"], r#"["A=1","B=2"]"#);
    }

    #[test]
    fn a_size_that_does_not_parse_is_refused_not_defaulted() {
        let mut s = spec();
        s.memory = "2 GB".into();
        let err = spec_fields(&s).unwrap_err();
        assert!(err.to_string().contains("memory"), "{err}");
    }

    /// A field asking for privilege is refused by name (DX-1540), never warned
    /// about and dropped: dropped, it would give the opposite of what was asked.
    #[test]
    fn a_privilege_field_is_refused_not_ignored() {
        for field in [
            "unprivileged: false",
            "privileged: true",
            "features: nesting=1",
        ] {
            let text = format!(
                "apiVersion: compute.delonix.io/v1alpha1\nkind: SystemContainer\n\
                 metadata: {{name: t}}\nspec: {{image: alpine:3.20, {field}}}\n"
            );
            let docs = manifest::load_str(&text, "test").unwrap();
            let err = desired(&docs[0]).unwrap_err();
            assert_eq!(err.number(), 1540, "{field}: {err}");
        }
        let docs = manifest::load_str(
            "apiVersion: compute.delonix.io/v1alpha1\nkind: SystemContainer\n\
             metadata: {name: t}\nspec: {image: alpine:3.20}\n",
            "test",
        )
        .unwrap();
        assert!(desired(&docs[0]).is_ok());
    }

    /// Memory, swap and cores converge live; the root volume only when it
    /// grows; image and network never. The same rule as the reconciler's.
    #[test]
    fn only_a_cold_change_asks_for_a_recreate() {
        let had = spec_fields(&spec()).unwrap();
        let change = |field: &str, value: &str| {
            let mut now = had.clone();
            now.insert(field.into(), value.into());
            cold_changes(&had, &now)
        };
        assert!(change("memory", "1024").is_empty());
        assert!(change("cores", "4").is_empty());
        assert!(change("rootfs", "8").is_empty(), "growing converges live");
        assert_eq!(
            change("rootfs", "1"),
            vec!["rootfs".to_string()],
            "shrinking is cold"
        );
        assert_eq!(change("image", "nginx"), vec!["image".to_string()]);
        assert_eq!(
            change("network", "bridge=vmbr1"),
            vec!["network".to_string()]
        );
    }
}
