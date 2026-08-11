//! `delonix volumes` — named volumes (create/ls/rm/inspect).

use std::path::PathBuf;

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_runtime_core::{Error, Result};
use delonix_volume::{parse_size_bytes, VolumeStore};
use serde::{Deserialize, Serialize};

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::state_root;

/// `spec` of `kind: Volume` — mirrors the fields of `VolumeCmd::Create`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct VolumeSpec {
    #[serde(default = "default_driver")]
    driver: String,
    device: Option<String>,
    /// Canonical `mountOptions` (uniform with `kind: Storage`); `options`
    /// is still accepted (backward-compat).
    #[serde(rename = "mountOptions", alias = "options")]
    options: Option<String>,
    quota: Option<String>,
    /// NFS export. One of the three network-share blocks — the block's NAME is
    /// the type, so a type cannot contradict its own declaration (the same shape
    /// `kind: Workload` uses for `spec.container`/`spec.vm`).
    #[serde(default)]
    nfs: Option<super::storage::NetShareSpec>,
    /// SMB/CIFS share (Samba, Windows, TrueNAS SMB).
    #[serde(default)]
    cifs: Option<super::storage::NetShareSpec>,
    /// WebDAV (Nextcloud, ownCloud).
    #[serde(default)]
    webdav: Option<super::storage::NetShareSpec>,
    /// OPTIONAL: create what the share block consumes, instead of requiring it
    /// to exist already (ADR-0009). Absent, this Kind behaves exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provision: Option<super::provision::ProvisionSpec>,
}

impl VolumeSpec {
    /// The network-share block, if any — `(type, block)`.
    ///
    /// **Exactly one, or none.** Two blocks are two different mounts asked of
    /// one volume, and picking by precedence would mount one of them and drop
    /// the other in silence. Same fail-closed treatment `lower_workload` gives a
    /// type that brings more than its own block.
    pub(crate) fn net_share(
        &self,
    ) -> Result<Option<(&'static str, &super::storage::NetShareSpec)>> {
        let present: Vec<(&'static str, &super::storage::NetShareSpec)> = [
            ("nfs", self.nfs.as_ref()),
            ("cifs", self.cifs.as_ref()),
            ("webdav", self.webdav.as_ref()),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k, v)))
        .collect();
        match present.len() {
            0 => Ok(None),
            1 => Ok(Some(present[0])),
            _ => Err(Error::Invalid(super::po::tf(
                "a volume declares more than one network share ({kinds}) — a volume is one mount",
                &[(
                    "kinds",
                    &present
                        .iter()
                        .map(|(k, _)| *k)
                        .collect::<Vec<_>>()
                        .join(", "),
                )],
            ))),
        }
    }
}

fn default_driver() -> String {
    "local".to_string()
}

/// Names accepted in the `kind: Volume` `spec` (canonical + aliases), for the
/// unknown-field warning.
pub(crate) const VOLUME_SPEC_FIELDS: &[&str] = &[
    "driver",
    "device",
    "mountOptions",
    "options",
    "quota",
    // Network-share blocks — `kind: Storage` folded in here.
    "nfs",
    "cifs",
    "webdav",
    // Optional provisioning of what the share block consumes.
    "provision",
];

/// Fields the reconciler compares for a `kind: Volume`. `quota` is the only one
/// that converges hot (`VolumeStore::set_quota`); changing the driver or the
/// device of a volume that already holds data is a replace, and a replace of a
/// volume means the data goes — which is exactly why `apply` refuses it without
/// `--replace`.
pub(crate) const RECONCILED_VOLUME_FIELDS: &[&str] = &["driver", "device", "mountOptions", "quota"];

/// The manifest side, in comparable form.
///
/// A network-share block is translated through the SAME `share_mount` the apply
/// uses. Comparing the block's own fields instead would report a difference on
/// every plan for every network volume — the record stores the derived
/// driver/device/options, never the friendly declaration.
fn desired_volume_fields(
    name: &str,
    spec: &VolumeSpec,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut f = std::collections::BTreeMap::new();
    let (driver, device, options) = match spec.net_share()? {
        Some((kind, block)) => {
            let m = super::storage::share_mount(name, kind, block)?;
            (m.driver, Some(m.device), m.options)
        }
        None => (
            spec.driver.clone(),
            spec.device.clone(),
            spec.options.clone(),
        ),
    };
    f.insert("driver".into(), driver);
    if let Some(d) = device {
        f.insert("device".into(), d);
    }
    if let Some(o) = options {
        f.insert("mountOptions".into(), o);
    }
    // The manifest says `10G`, the record stores bytes. Normalize the manifest
    // side through the SAME parser the creation path uses, or a quota would
    // read as changed on every plan.
    if let Some(q) = &spec.quota {
        if let Some(bytes) = delonix_volume::parse_size_bytes(q) {
            f.insert("quota".into(), bytes.to_string());
        }
    }
    Ok(f)
}

/// The machine side, in the same comparable form.
fn actual_volume_fields(v: &delonix_volume::Volume) -> std::collections::BTreeMap<String, String> {
    let mut f = std::collections::BTreeMap::new();
    f.insert("driver".into(), v.driver.clone());
    if let Some(d) = &v.device {
        f.insert("device".into(), d.clone());
    }
    if let Some(o) = &v.options {
        f.insert("mountOptions".into(), o.clone());
    }
    if let Some(q) = v.quota_bytes {
        f.insert("quota".into(), q.to_string());
    }
    f
}

/// Destroys a volume so the normal creation path can rebuild it. **This
/// destroys the data** — which is the whole reason `apply` refuses a volume
/// replace unless the user names it explicitly.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    let store = VolumeStore::open(state_root())?;
    cmd_rm(&store, name, true)
}

/// Applies the hot part of a plan. `quota` is the only field that converges
/// without recreating the volume — and recreating a volume means the data goes,
/// which is why everything else is a `Replace` that `apply` refuses by default.
pub(crate) fn converge(name: &str, diffs: &[super::reconcile::FieldDiff]) -> Result<()> {
    let store = VolumeStore::open(state_root())?;
    for d in diffs {
        match d.field.as_str() {
            "quota" => {
                // `None` = the manifest dropped the quota, which means REMOVE
                // the cap, not «leave the old one» — the revert has to be as
                // real as the set.
                let bytes = d.to.as_deref().and_then(|v| v.parse::<u64>().ok());
                // `privileged: false`, the same as `create_volume`'s own call:
                // the hard ext4-loopback cap belongs to the root model, and a
                // declarative apply must not quietly take a different route
                // from the imperative one it mirrors.
                store.set_quota(name, bytes, None, false)?;
            }
            other => {
                return Err(Error::Invalid(format!(
                    "volume/{name}: '{other}' does not converge hot — bug in \
                     `reconcile::hot_fields`"
                )))
            }
        }
    }
    Ok(())
}

/// Records that this stack owns the volume, and what it last applied.
pub(crate) fn stamp(
    name: &str,
    stack: &str,
    fields: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let store = VolumeStore::open(state_root())?;
    store.set_metadata(
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
    )?;
    Ok(())
}

/// What the manifest declares, for the reconciler.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: VolumeSpec = manifest::spec_of(doc)?;
    Ok(super::reconcile::Desired {
        kind: "Volume".into(),
        name: doc.metadata.name.clone(),
        fields: desired_volume_fields(&doc.metadata.name, &spec)?,
        converges: true,
        ownable: true,
    })
}

/// What is on the machine, for the reconciler.
pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    let store = VolumeStore::open(state_root())?;
    Ok(store
        .list()?
        .into_iter()
        .map(|v| super::reconcile::Actual {
            kind: "Volume".into(),
            name: v.name.clone(),
            fields: actual_volume_fields(&v),
            owner: v.labels.get(super::reconcile::STACK_LABEL).cloned(),
            last_applied: v
                .annotations
                .get(super::reconcile::LAST_APPLIED)
                .and_then(|raw| super::reconcile::decode_last_applied(raw)),
        })
        .collect())
}

#[derive(Subcommand)]
pub enum VolumeCmd {
    /// Create a named volume.
    Create {
        name: String,
        /// `local` (default) or `nfs`.
        #[arg(long, default_value = "local")]
        driver: String,
        /// Device/export (`nfs` driver).
        #[arg(long)]
        device: Option<String>,
        /// Additional mount options (`nfs` driver).
        #[arg(long)]
        options: Option<String>,
        /// Quota (e.g. `2g`) — only applied if `--quota` is given.
        #[arg(long)]
        quota: Option<String>,
    },
    /// List the volumes.
    Ls {
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: output::OutputFormat,
    },
    /// Details of a volume (includes real on-disk usage).
    Inspect {
        #[arg(add = ArgValueCandidates::new(super::complete::volumes))]
        name: String,
    },
    /// Readable detail of one or more volumes, `kubectl describe` style.
    ///
    /// For humans; use `inspect` for the usual compact view.
    Describe {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::volumes))]
        names: Vec<String>,
    },
    /// Remove a volume.
    ///
    /// Refuses while a container or a `kind: ShareVolume` still references it
    /// (use `--force` to remove it anyway).
    Rm {
        #[arg(add = ArgValueCandidates::new(super::complete::volumes))]
        name: String,
        /// Remove it even with live references — DESTROYS the data of whatever
        /// is still using it.
        #[arg(short = 'f', long)]
        force: bool,
        /// ALSO destroy the storage this volume was provisioned with on a
        /// remote NAS (`spec.provision`). Irreversible, and on another machine.
        /// Refused for a volume that only mounts a share someone else made.
        #[arg(long = "destroy-remote")]
        destroy_remote: bool,
    },
    /// Apply the `kind: Volume` documents from a manifest (idempotent by name).
    Apply {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Point-in-time snapshots of a volume (tar.gz under the volume; safe in rootless).
    Snapshot {
        #[command(subcommand)]
        action: SnapshotCmd,
    },
}

/// `delonix volumes snapshot` — crash-consistent (taken with the workload
/// running). For application consistency (e.g. a DB), stop/dump the consumer
/// first. In rootless the tar runs in a mapped userns (effective owner of the
/// subuid files) — see `runtime::reexec_mapped`/`__volsnap`.
#[derive(clap::Subcommand)]
pub enum SnapshotCmd {
    /// Create a snapshot NOW (default name: UTC timestamp).
    Create {
        #[arg(add = ArgValueCandidates::new(super::complete::volumes))]
        volume: String,
        /// Snapshot name (default: `YYYYMMDD-HHMMSS`).
        #[arg(long)]
        name: Option<String>,
    },
    /// List the snapshots of a volume.
    Ls {
        /// Volume to query (omit for the snapshots of ALL).
        #[arg(add = ArgValueCandidates::new(super::complete::volumes))]
        volume: Option<String>,
    },
    /// Restore a snapshot INTO the volume (replaces the current data — stop the
    /// consumers first).
    Restore {
        #[arg(add = ArgValueCandidates::new(super::complete::volumes))]
        volume: String,
        /// Snapshot name (see `snapshot ls`).
        snap: String,
    },
    /// Delete a snapshot.
    Rm {
        #[arg(add = ArgValueCandidates::new(super::complete::volumes))]
        volume: String,
        snap: String,
    },
}

pub fn run(action: VolumeCmd) -> Result<()> {
    let store = VolumeStore::open(state_root())?;
    match action {
        VolumeCmd::Create {
            name,
            driver,
            device,
            options,
            quota,
        } => {
            let vol = create_volume(&store, &name, &driver, device, options, quota)?;
            println!("{}", vol.name);
            Ok(())
        }
        VolumeCmd::Ls { output } => cmd_ls(&store, output),
        VolumeCmd::Inspect { name } => cmd_inspect(&store, &name),
        VolumeCmd::Describe { names } => cmd_describe(&store, &names),
        VolumeCmd::Rm {
            name,
            force,
            destroy_remote,
        } => cmd_rm_with(&store, &name, force, destroy_remote),
        VolumeCmd::Snapshot { action } => cmd_snapshot(&store, action),
        VolumeCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
    }
}

/// Applies the `kind: Volume` documents (`create`/`create_with` are already
/// idempotent by name — no separate existence check needed).
/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: VolumeSpec = manifest::spec_of(doc)?;
    let share = spec.net_share()?;
    // With a network-share block, the driver and device are DERIVED from it.
    // Printing the struct's `driver: local` default next to an `nfs:` block
    // would read as a contradiction — and a dry-run exists to say what will
    // actually be applied, not what the struct happens to default to.
    let derived = match share {
        Some((kind, block)) => Some(super::storage::share_mount(
            &doc.metadata.name,
            kind,
            block,
        )?),
        None => None,
    };
    let mut v = serde_yaml::to_value(&spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))?;
    if let (Some(m), serde_yaml::Value::Mapping(map)) = (derived, &mut v) {
        map.insert(
            serde_yaml::Value::from("driver"),
            serde_yaml::Value::from(m.driver),
        );
        map.insert(
            serde_yaml::Value::from("device"),
            serde_yaml::Value::from(m.device),
        );
        map.insert(
            serde_yaml::Value::from("mountOptions"),
            match m.options {
                Some(o) => serde_yaml::Value::from(o),
                None => serde_yaml::Value::Null,
            },
        );
    }
    Ok(v)
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let store = VolumeStore::open(state_root())?;
    for doc in manifest::of_kind(docs, "Volume") {
        let name = &doc.metadata.name;
        manifest::warn_unknown_fields(doc, VOLUME_SPEC_FIELDS);
        let spec: VolumeSpec = manifest::spec_of(doc)?;
        // A network share (`spec.nfs`/`cifs`/`webdav`) IS this volume's driver
        // and device — the friendly declaration that `kind: Storage` used to own.
        // The credentials file is written here and only here: computing the
        // mount is pure precisely so a `plan`/`--dry-run` can describe it
        // without creating anything.
        // Provisioning runs FIRST and its result feeds the mount below: the
        // `server` is the NAS that was just provisioned and the `share` is the
        // mountpoint it reported. Deriving both means the mount cannot end up
        // pointing at a different machine, or at a path the appliance does not
        // actually use, the way a second hand-written `nfs:` block could.
        let provisioned = match &spec.provision {
            Some(p) => {
                super::provision::warn_unknown(doc);
                match &p.truenas {
                    Some(t) => Some(super::provision::run_truenas(t)?),
                    None => {
                        return Err(delonix_runtime_core::Error::Invalid(
                            super::po::t("spec.provision needs a target block (today: `truenas:`)")
                                .into(),
                        ))
                    }
                }
            }
            None => None,
        };
        // Report the NAS side as soon as it exists, BEFORE the local mount is
        // attempted. The mount needs CAP_SYS_ADMIN and fails in a plain
        // rootless session — and an apply that dies there having silently
        // created a dataset, a quota and an export on somebody's NAS would
        // leave the operator with no idea that anything was made at all. What
        // is printed is what the appliance reports it is enforcing, not what
        // the manifest asked for.
        if let Some(p) = &provisioned {
            // Adoption is not free: the quota of whatever was found gets
            // aligned to the manifest. Nothing on the NAS marks a dataset as
            // ours, so a name that belongs to something else is re-capped
            // silently unless this says so.
            if p.adopted {
                super::output::warn(&super::po::tf(
                    "volume '{name}': the dataset {ds} already existed and was adopted — its quota now follows this manifest",
                    &[("name", name), ("ds", &p.share)],
                ));
            }
            println!(
                "volume/{name}: {} {}:{} ({}, {})",
                super::po::t("provisioned"),
                p.server,
                p.share,
                match p.quota {
                    Some(b) =>
                        super::po::tf("quota {size}", &[("size", &super::output::fmt_size(b))]),
                    None => super::po::t("no quota").to_string(),
                },
                match p.available {
                    Some(b) =>
                        super::po::tf("{size} free", &[("size", &super::output::fmt_size(b))]),
                    None => super::po::t("free space unknown").to_string(),
                },
            );
        }
        let derived_share;
        let effective_share = match (&provisioned, spec.net_share()?) {
            // Provisioned and NOT separately declared: the share block is
            // built from what the NAS reported — but ONLY when an export was
            // actually asked for. `provision:` with no `share:` provisions the
            // dataset and exports nothing, so deriving a mount here would try
            // to mount an export that does not exist and fail with the NAS
            // refusing a path it never published.
            (Some(p), None) if p.exported => {
                derived_share = super::storage::NetShareSpec {
                    server: p.server.clone(),
                    share: p.share.clone(),
                    username: None,
                    password: None,
                    password_secret: None,
                    read_only: false,
                    mount_options: None,
                };
                Some(("nfs", &derived_share))
            }
            // Both: the explicit block wins for credentials and mount options,
            // but it must not name a DIFFERENT export — that is two answers to
            // where the data lives, and picking one silently is how a volume
            // ends up mounted somewhere nothing was provisioned.
            (Some(p), Some((kind, block))) => {
                if block.server != p.server || block.share != p.share {
                    return Err(delonix_runtime_core::Error::Invalid(super::po::tf(
                        "volume '{name}': spec.{kind} points at {given}, but spec.provision made {made} — remove the server/share from the block and let it be derived, or provision what you are mounting",
                        &[
                            ("name", name),
                            ("kind", kind),
                            ("given", &format!("{}:{}", block.server, block.share)),
                            ("made", &format!("{}:{}", p.server, p.share)),
                        ],
                    )));
                }
                Some((kind, block))
            }
            // Provisioned without an export: nothing to mount, so the volume
            // is whatever the manifest otherwise says (a plain local one).
            (Some(_), None) => None,
            (None, other) => other,
        };
        let (driver, device, options) = match effective_share {
            Some((kind, block)) => {
                // A typo inside the block would otherwise be swallowed: the
                // top-level warning only looks at the spec's own keys, and a
                // `serverr:` in here would silently become "no server".
                manifest::warn_unknown_fields_in(doc, kind, super::storage::NET_SHARE_FIELDS);
                super::storage::ensure_share_credentials(name, kind, block)?;
                let m = super::storage::share_mount(name, kind, block)?;
                (m.driver, Some(m.device), m.options)
            }
            None => (
                spec.driver.clone(),
                spec.device.clone(),
                spec.options.clone(),
            ),
        };
        create_volume(&store, name, &driver, device, options, spec.quota.clone())?;
        // Stamp WHO provisioned this and where. It is the ownership mark that
        // `--destroy-remote` needs: without it there is no claim, and a volume
        // that merely mounts a share made by someone else can never take that
        // share down with it. References only — the secret is named, not copied.
        if let Some(t) = spec.provision.as_ref().and_then(|p| p.truenas.as_ref()) {
            let prov = serde_json::to_string(&super::provision::Provenance::of(t))
                .map_err(|e| Error::Invalid(format!("could not record the provisioning: {e}")))?;
            // Same mechanism the reconciler already uses to stamp
            // `delonix.io/last-applied` on this very Kind — no second way to
            // write a volume's metadata.
            store.set_metadata(
                name,
                &[],
                &[(
                    super::provision::PROVENANCE_ANNOTATION.to_string(),
                    Some(prov),
                )],
            )?;
        }
        println!("volume/{name}: {}", super::po::t("ensured"));
    }
    Ok(())
}

fn create_volume(
    store: &VolumeStore,
    name: &str,
    driver: &str,
    device: Option<String>,
    options: Option<String>,
    quota: Option<String>,
) -> Result<delonix_volume::Volume> {
    // VALIDATE BEFORE CREATING. This used to create the volume first and parse
    // `--quota` after, so `volumes create v --quota abc` exited 1 with "invalid
    // quota" while leaving a REAL volume `v` behind — with no quota at all. A
    // control plane that retries on the non-zero exit then reuses that unlimited
    // volume, and the quota it asked for is silently absent forever.
    let quota_bytes = match quota {
        Some(q) => Some(parse_size_bytes(&q).ok_or_else(|| {
            delonix_runtime_core::Error::Invalid(super::po::tf("invalid quota: {q}", &[("q", &q)]))
        })?),
        None => None,
    };
    let existed = store.inspect(name).is_ok();
    let vol = if driver == "local" && device.is_none() && options.is_none() {
        store.create(name)?
    } else {
        store.create_with(name, driver, device, options)?
    };
    if let Some(bytes) = quota_bytes {
        store.set_quota(name, Some(bytes), None, false)?;
    }
    if !existed {
        delonix_runtime_core::events::emit(
            &state_root(),
            "volume",
            "create",
            &vol.name,
            &vol.name,
            Some(&vol.driver),
        );
    }
    Ok(vol)
}

/// `volumes ls -o json` row (ADR-0005): stable keys mirroring the table columns.
#[derive(serde::Serialize)]
struct VolumeLsRow {
    name: String,
    driver: String,
    mountpoint: String,
}

fn cmd_ls(store: &VolumeStore, format: output::OutputFormat) -> Result<()> {
    let vols = store.list()?;
    if format == output::OutputFormat::Json {
        let rows: Vec<VolumeLsRow> = vols
            .into_iter()
            .map(|v| VolumeLsRow {
                name: v.name,
                driver: v.driver,
                mountpoint: v.mountpoint,
            })
            .collect();
        return output::print_json(&rows);
    }
    let mut t = output::Table::new(&["NAME", "DRIVER", "MOUNTPOINT"]);
    for v in vols {
        t.row(vec![v.name, v.driver, v.mountpoint]);
    }
    t.print();
    Ok(())
}

/// On-disk usage, with the quota denominator when present: `"1.5 KiB"` or
/// `"1.5 KiB / 2.0 GiB (0%)"`. **Pure** function (the real `usage`/`quota_bytes`
/// come from the store) so the percentage arithmetic is testable — including
/// quota 0, which cannot divide by zero.
fn fmt_usage(used: u64, quota: Option<u64>) -> String {
    match quota {
        Some(q) if q > 0 => {
            let pct = (used as f64 / q as f64 * 100.0).round() as u64;
            format!(
                "{} / {} ({pct}%)",
                output::fmt_size(used),
                output::fmt_size(q)
            )
        }
        // Quota 0 = no space at all; printing "(inf%)" would be worse than just usage.
        Some(_) => format!("{} / 0 B", output::fmt_size(used)),
        None => output::fmt_size(used),
    }
}

/// `volumes describe` — readable detail in `kubectl describe` style.
/// Complements `inspect` (the usual compact view, stable for scripts).
fn cmd_describe(store: &VolumeStore, names: &[String]) -> Result<()> {
    for (i, name) in names.iter().enumerate() {
        let v = store.inspect(name)?;
        if i > 0 {
            println!();
        }
        describe_one(store, &v);
    }
    Ok(())
}

fn describe_one(_store: &VolumeStore, v: &delonix_volume::Volume) {
    let mut d = output::Describe::new();
    d.field("Name", &v.name);
    d.field("Driver", &v.driver);
    d.field("Mountpoint", &v.mountpoint);
    d.field("Created", output::fmt_local(v.created_unix));
    d.field("Age", output::fmt_age(v.created_unix));
    d.field(
        "Usage",
        fmt_measured(
            measured_usage(std::path::Path::new(&v.mountpoint)),
            v.quota_bytes,
        ),
    );
    d.field(
        "Quota",
        v.quota_bytes
            .map(output::fmt_size)
            .unwrap_or_else(|| "<none>".into()),
    );
    d.field_opt("Alert at", v.alert_pct.map(|p| format!("{p}%")));
    // Only exist in the `nfs` driver — omitted entirely for `local`.
    d.field_opt("Device", v.device.as_deref());
    d.field_opt("Options", v.options.as_deref());
    d.print();
}

fn cmd_inspect(store: &VolumeStore, name: &str) -> Result<()> {
    let v = store.inspect(name)?;
    let usage = measured_usage(std::path::Path::new(&v.mountpoint));
    println!("{:<13}{}", format!("{}:", super::po::t("name")), v.name);
    println!("{:<13}{}", format!("{}:", super::po::t("driver")), v.driver);
    println!(
        "{:<13}{}",
        format!("{}:", super::po::t("mountpoint")),
        v.mountpoint
    );
    println!(
        "{}",
        super::po::tf(
            "created:     unix={ts}",
            &[("ts", &v.created_unix.to_string())],
        )
    );
    // An unreadable subtree must never print as `0 bytes` — that reads as an
    // empty volume and is exactly how a full disk went unnoticed.
    if usage.is_complete() {
        println!(
            "{:<13}{} bytes",
            format!("{}:", super::po::t("usage")),
            usage.bytes
        );
    } else {
        println!(
            "{:<13}{}",
            format!("{}:", super::po::t("usage")),
            fmt_measured(usage, None)
        );
    }
    if let Some(q) = v.quota_bytes {
        println!("{:<13}{q} bytes", format!("{}:", super::po::t("quota")));
    }
    Ok(())
}

/// Default snapshot name: UTC timestamp `YYYYMMDD-HHMMSS` (no `chrono` — the
/// runtime does not bring it in; uses `libc::gmtime_r`).
fn default_snap_name() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `t` is valid; `gmtime_r` writes into `tm` (our buffer).
    unsafe { libc::gmtime_r(&t, &mut tm) };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Runs a snapshot operation via the right path: rootless → re-exec
/// `__volsnap` in a mapped userns (owner of the subuids); rootful/no-helpers →
/// direct. The `__volsnap` handler lives in `cmd::mapped` (see the note there on
/// the re-exec contract that the public engine was missing).
/// Measured usage of a tree, falling back to the MAPPED userns when the direct
/// walk could not read everything.
///
/// This is the CLI half of the `Usage`/`__duusage` fix. The direct walk is tried
/// first because it is free and correct for the majority of volumes; only when it
/// reports unreadable subtrees (the rootless subuid case — every managed database)
/// do we pay for a `reexec_mapped`, where we own the subuids and see the real size.
/// If even that is unavailable (no subid helpers, no rootless), the caller gets
/// the incomplete `Usage` back and must render it as UNKNOWN — never as 0, which
/// is the bug this whole path exists to kill.
pub(crate) fn measured_usage(path: &std::path::Path) -> delonix_volume::Usage {
    let direct = delonix_volume::measure(path);
    if direct.is_complete() {
        return direct;
    }
    let out = std::env::temp_dir().join(format!("delonix-duusage-{}", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let p = path.to_string_lossy().to_string();
    let o = out.to_string_lossy().to_string();
    let mapped = match delonix_runtime::reexec_mapped(&["__duusage", &p, &o]) {
        Some(true) => std::fs::read_to_string(&out)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|bytes| delonix_volume::Usage {
                bytes,
                unreadable: 0,
            }),
        // `Some(false)` = the mapped child failed; `None` = no rootless/subid
        // helpers at all. Either way we cannot improve on `direct`.
        _ => None,
    };
    let _ = std::fs::remove_file(&out);
    mapped.unwrap_or(direct)
}

/// Renders a measured usage for humans, with the quota denominator when there is
/// one. An INCOMPLETE measurement is labelled as such instead of printing a
/// number that reads as authoritative.
pub(crate) fn fmt_measured(u: delonix_volume::Usage, quota: Option<u64>) -> String {
    if !u.is_complete() {
        return super::po::tf(
            "unknown (>= {seen}, {n} dir(s) unreadable — run as the data's owner)",
            &[
                ("seen", &output::fmt_size(u.bytes)),
                ("n", &u.unreadable.to_string()),
            ],
        );
    }
    fmt_usage(u.bytes, quota)
}

fn volsnap_run(mode: &str, data: &std::path::Path, tarball: &std::path::Path) -> Result<()> {
    let d = data.to_string_lossy().to_string();
    let t = tarball.to_string_lossy().to_string();
    match delonix_runtime::reexec_mapped(&["__volsnap", mode, &d, &t]) {
        Some(true) => Ok(()),
        Some(false) => Err(Error::Runtime {
            context: "volume snapshot",
            message: super::po::tf(
                "__volsnap {mode} failed in the mapped userns (see /etc/subuid)",
                &[("mode", mode)],
            ),
        }),
        // No rootless/helpers: run direct (already owner of the files).
        None => super::mapped::volsnap(mode, data, tarball),
    }
}

fn cmd_snapshot(store: &VolumeStore, action: SnapshotCmd) -> Result<()> {
    match action {
        SnapshotCmd::Create { volume, name } => {
            let v = store.inspect(&volume)?;
            let snap = name.unwrap_or_else(default_snap_name);
            let tarball = store.snapshot_path(&volume, &snap)?;
            if tarball.exists() {
                return Err(Error::Conflict(super::po::tf(
                    "snapshot '{snap}' already exists",
                    &[("snap", &snap)],
                )));
            }
            volsnap_run("create", std::path::Path::new(&v.mountpoint), &tarball)?;
            let size = std::fs::metadata(&tarball).map(|m| m.len()).unwrap_or(0);
            println!(
                "{}",
                super::po::tf(
                    "snapshot '{snap}' of volume '{volume}' created ({size})",
                    &[
                        ("snap", &snap),
                        ("volume", &volume),
                        ("size", &super::output::fmt_size(size))
                    ],
                )
            );
            println!(
                "{}",
                super::output::dim(super::po::t(
                    "(crash-consistent: for DB consistency, stop/dump the consumer first)"
                ))
            );
        }
        SnapshotCmd::Ls { volume } => {
            // No argument: snapshots of ALL volumes, with a VOLUME column.
            let vols: Vec<String> = match volume {
                Some(v) => {
                    store.inspect(&v)?; // validates that the volume exists
                    vec![v]
                }
                None => store.list()?.into_iter().map(|v| v.name).collect(),
            };
            let mut t = super::output::Table::new(&["VOLUME", "SNAPSHOT", "SIZE", "CREATED"])
                .right_align(2);
            for v in vols {
                for (n, size, ts) in store.list_snapshots(&v)? {
                    t.row(vec![
                        v.clone(),
                        n,
                        super::output::fmt_size(size),
                        super::output::fmt_local(ts.max(0) as u64),
                    ]);
                }
            }
            t.print();
        }
        SnapshotCmd::Restore { volume, snap } => {
            let v = store.inspect(&volume)?;
            let tarball = store.snapshot_path(&volume, &snap)?;
            if !tarball.exists() {
                return Err(Error::NotFound(format!(
                    "snapshot {snap} do volume {volume}"
                )));
            }
            super::output::warn(&super::po::tf(
                "restoring '{volume}' from '{snap}' — stop the volume's consumers first",
                &[("volume", &volume), ("snap", &snap)],
            ));
            volsnap_run("restore", std::path::Path::new(&v.mountpoint), &tarball)?;
            println!(
                "{}",
                super::po::tf(
                    "volume '{volume}' restored from snapshot '{snap}'",
                    &[("volume", &volume), ("snap", &snap)],
                )
            );
        }
        SnapshotCmd::Rm { volume, snap } => {
            store.remove_snapshot(&volume, &snap)?;
            println!(
                "{}",
                super::po::tf(
                    "snapshot '{snap}' of volume '{volume}' deleted",
                    &[("snap", &snap), ("volume", &volume)],
                )
            );
        }
    }
    Ok(())
}

/// What would break if this volume disappeared right now: containers whose
/// record still mounts it, and `kind: ShareVolume` slices carved out of it.
///
/// Both halves come from state that is already persisted — `c.mounts` (which
/// exists precisely so a `start` can rebuild the same mounts) and the volume
/// records themselves — so there is no new bookkeeping to keep in sync.
pub(crate) struct VolumeRefs {
    /// `name (running)` / `name (stopped)` per referencing container.
    pub containers: Vec<String>,
    /// Names of `ShareVolume`s whose data lives INSIDE this volume.
    pub shares: Vec<String>,
}

impl VolumeRefs {
    pub fn is_empty(&self) -> bool {
        self.containers.is_empty() && self.shares.is_empty()
    }
}

/// Collects every live reference to `name`. Matching is by RESOLVED mountpoint,
/// not by the `-v` spec string: the record stores the source path, and a bind of
/// the same path must count just as much as a named reference.
pub(crate) fn volume_refs(store: &VolumeStore, name: &str) -> VolumeRefs {
    let mut out = VolumeRefs {
        containers: Vec::new(),
        shares: Vec::new(),
    };
    let Ok(vol) = store.inspect(name) else {
        return out;
    };
    let mount = std::path::Path::new(&vol.mountpoint);

    if let Ok((_, cstore)) = super::util::open_stores() {
        if let Ok(list) = cstore.list() {
            for c in list {
                let uses = c.mounts.iter().any(|m| {
                    let src = std::path::Path::new(&m.source);
                    src == mount || src.starts_with(mount)
                });
                if !uses {
                    continue;
                }
                let alive = c.pid.map(delonix_runtime::is_alive).unwrap_or(false);
                let state = if alive {
                    super::po::t("running")
                } else {
                    super::po::t("stopped")
                };
                out.containers.push(format!("{} ({state})", c.name));
            }
        }
    }

    // A ShareVolume is a real subdirectory of its parent Storage, registered as
    // its own volume record — so "is a share of this" is exactly "my mountpoint
    // lives under yours".
    if let Ok(all) = store.list() {
        for v in all {
            if v.name == vol.name {
                continue;
            }
            if std::path::Path::new(&v.mountpoint).starts_with(mount) {
                out.shares.push(v.name);
            }
        }
    }
    out
}

/// `storage rm`'s removal: the same reference check and mapped-tree removal as
/// `volumes rm`, without a `--force` escape (a `kind: Storage` is shared
/// infrastructure — the operator should take the shares down explicitly).
pub(crate) fn cmd_rm_storage(store: &VolumeStore, name: &str) -> Result<()> {
    cmd_rm(store, name, false)
}

/// Removes a volume.
///
/// **Two confirmed data-loss paths are closed here.** Before this, `cmd_rm` was
/// a bare `store.remove(name)`:
/// - it happily destroyed a volume bind-mounted into a **running** container
///   (measured: a live container's `/data` went from 30 MiB to empty, exit 0,
///   no warning) — Docker refuses this outright;
/// - and it destroyed a parent `Storage` out from under every
///   `kind: ShareVolume` carved into it, leaving the share records pointing at a
///   deleted path while `sharevolume ls` still reported them healthy at
///   `USED 0 B` — silent, total, multi-tenant data loss from one command.
///
/// So the reference check is the default and `--force` is the explicit override,
/// which is also what makes the operation predictable in a PaaS control plane:
/// a plain `rm` can never take a tenant's live data with it.
///
/// It also passes `remove_tree_mapped` down as the tree remover: in rootless the
/// data belongs to a SUBUID, and without the hook the removal failed with a bare
/// `Permission denied` and the volume could not be deleted by ANY command.
pub(crate) fn cmd_rm(store: &VolumeStore, name: &str, force: bool) -> Result<()> {
    cmd_rm_with(store, name, force, false)
}

/// `cmd_rm`, plus the option to destroy the storage this volume was
/// PROVISIONED with on a remote NAS.
///
/// `destroy_remote` defaults to false everywhere and always will: removing a
/// volume is a local operation, and taking someone's dataset down with it is
/// not something to infer. ADR-0009 puts it behind its own flag for the same
/// reason `--purge-data` exists on a share volume.
pub(crate) fn cmd_rm_with(
    store: &VolumeStore,
    name: &str,
    force: bool,
    destroy_remote: bool,
) -> Result<()> {
    // `inspect` first: a volume with no record at all is a plain NotFound, and
    // must stay one (`rm` of something absent is an error, docker parity).
    let vol = store.inspect(name)?;
    if !force {
        let refs = volume_refs(store, name);
        if !refs.is_empty() {
            let mut who: Vec<String> = Vec::new();
            if !refs.containers.is_empty() {
                who.push(super::po::tf(
                    "container(s): {list}",
                    &[("list", &refs.containers.join(", "))],
                ));
            }
            if !refs.shares.is_empty() {
                who.push(super::po::tf(
                    "share volume(s): {list}",
                    &[("list", &refs.shares.join(", "))],
                ));
            }
            return Err(Error::Invalid(super::po::tf(
                "volume '{name}' is in use by {who} — stop/remove them first, or pass --force to \
                 destroy the data anyway",
                &[("name", name), ("who", &who.join("; "))],
            )));
        }
    }
    // The remote goes FIRST, and the local record LAST. The record is the only
    // thing that says WHICH dataset on WHICH appliance belongs to this volume;
    // deleting it first and then failing to reach the NAS would leave a dataset
    // orphaned with nothing left anywhere pointing at it. Same rule the v0.37.0
    // audit wrote down after `volumes rm` deleted the bookkeeping ahead of the
    // data: destroy in order, and take the accounting down last.
    if destroy_remote {
        let what = super::provision::destroy_remote(&vol.annotations)?;
        println!(
            "volume/{name}: {} {what}",
            super::po::t("destroyed the provisioned storage"),
        );
    }
    store.remove_with(name, Some(&delonix_runtime::remove_tree_mapped))?;
    delonix_runtime_core::events::emit(
        &state_root(),
        "volume",
        "remove",
        &vol.name,
        &vol.name,
        if force { Some("force") } else { None },
    );
    println!("{name}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{fmt_usage, VolumeSpec};

    #[test]
    fn volumespec_aceita_options_legado_e_mountoptions_canonico() {
        let legado: VolumeSpec = serde_yaml::from_str("driver: nfs\noptions: vers=4,ro\n").unwrap();
        assert_eq!(legado.options.as_deref(), Some("vers=4,ro"));
        let canon: VolumeSpec =
            serde_yaml::from_str("driver: nfs\nmountOptions: vers=4,ro\n").unwrap();
        assert_eq!(canon.options.as_deref(), Some("vers=4,ro"));
    }

    #[test]
    fn usage_sem_quota_mostra_so_o_uso() {
        assert_eq!(fmt_usage(1536, None), "1.5 KiB");
    }

    #[test]
    fn usage_com_quota_mostra_percentagem() {
        assert_eq!(
            fmt_usage(512 * 1024 * 1024, Some(1024 * 1024 * 1024)),
            "512.0 MiB / 1.00 GiB (50%)"
        );
    }

    #[test]
    fn usage_com_quota_zero_nao_divide_por_zero() {
        // A quota of 0 would give `inf%`/NaN in the percentage — degrades to raw usage.
        assert_eq!(fmt_usage(100, Some(0)), "100 B / 0 B");
    }

    /// The documented field list and the code that builds it are two places
    /// saying the same thing, and this repo has already paid three times for
    /// letting two such places drift (the docs described `serve docker-api` as
    /// read-only, `cluster kubeadm` without HA, `network` without a realized
    /// overlay). This test is the constant's real consumer: it fails the moment
    /// a field is compared without being documented.
    #[test]
    fn os_campos_comparados_sao_os_documentados() {
        let spec: super::VolumeSpec = serde_yaml::from_str(
            "driver: nfs\ndevice: nas:/export\nmountOptions: vers=4\nquota: 10G\n",
        )
        .unwrap();
        let f = super::desired_volume_fields("dados", &spec).unwrap();
        assert_eq!(f.len(), super::RECONCILED_VOLUME_FIELDS.len());
        for k in f.keys() {
            assert!(
                super::RECONCILED_VOLUME_FIELDS.contains(&k.as_str()),
                "{k} is compared but undocumented"
            );
        }
        // The manifest says `10G`, the record stores bytes — normalizing the
        // manifest side is what stops a quota from reading as changed forever.
        assert_eq!(f.get("quota").unwrap(), "10737418240");
    }
}
