//! `kind: ObjectStorage` — an S3-compatible object store (ADR-0081).
//!
//! Two backends, chosen by `spec.backend`:
//!
//! - `cephRgw` — provision a tenant's S3 identity on the node's Ceph Object
//!   Gateway (`radosgw-admin`), and hand its endpoint and keys back in a
//!   generated `kind: Secret`. The bucket itself is created by the tenant over
//!   S3 with those keys — the engine never holds a tenant's data on a command
//!   line, the same boundary `delonix-provider-ceph`'s `rgw.rs` draws.
//! - `external` — bring your own S3: the endpoint, an optional region, and the
//!   name of a `kind: Secret` that already holds the credentials. Nothing is
//!   provisioned; the declaration is validated and recorded so a workload
//!   consumes it the SAME way it consumes a provisioned one (endpoint + keys
//!   from a Secret).
//!
//! Object storage has no volume shape — a bucket is not a disk and not a
//! directory — so this is a Kind of its own and not a `kind: Volume` block, the
//! same reasoning `VolumeService`/`StorageService` are split on in the node API.

use std::collections::BTreeMap;
use std::sync::Arc;

use clap::Subcommand;
use serde::{Deserialize, Serialize};

use delonix_model::{Error, Result};
use delonix_state::{JsonStore, Secret, SecretStore};

use super::kinds as k;
use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::state_root;

/// The two accepted `spec.backend` values.
const BACKEND_CEPH_RGW: &str = "cephRgw";
const BACKEND_EXTERNAL: &str = "external";

/// Sub-directory of the state root holding one JSON record per ObjectStorage.
const STORE_DIR: &str = "objectstores";

/// `spec` of `kind: ObjectStorage`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct ObjectStorageSpec {
    /// Which store backs this: `cephRgw` (the engine provisions an S3 identity
    /// on the node's Ceph gateway) or `external` (bring your own S3).
    backend: String,
    /// The bucket name. For `cephRgw` the tenant creates it over S3 with the
    /// keys handed back; for `external` it names the bucket on the foreign store.
    bucket: String,
    /// Present for (and only for) `backend: cephRgw`.
    #[serde(default, rename = "cephRgw", skip_serializing_if = "Option::is_none")]
    ceph_rgw: Option<CephRgwBlock>,
    /// Present for (and only for) `backend: external`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    external: Option<ExternalBlock>,
}

/// `spec.cephRgw` — how to provision the S3 identity on the Ceph gateway.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct CephRgwBlock {
    /// The S3 user id to provision. Defaults to the document's `metadata.name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uid: Option<String>,
    /// A human label for the S3 identity. Defaults to the uid.
    #[serde(
        default,
        rename = "displayName",
        skip_serializing_if = "Option::is_none"
    )]
    display_name: Option<String>,
    /// The PUBLIC S3 endpoint to expose to consumers (e.g. `http://rgw:80`).
    /// `radosgw-admin` provisions the identity but does not know the gateway's
    /// public URL, so the operator declares it here when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    /// An optional ceiling on the tenant's total S3 usage (a user quota on the
    /// gateway, across all its buckets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quota: Option<QuotaBlock>,
}

/// `spec.cephRgw.quota` — a user quota on the gateway.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct QuotaBlock {
    /// Maximum total bytes, in the gateway's own spelling (`100G`, `1T`, or a
    /// byte count). Omit for no byte ceiling.
    #[serde(default, rename = "maxSize", skip_serializing_if = "Option::is_none")]
    max_size: Option<String>,
    /// Maximum total object count. Omit for no object ceiling.
    #[serde(
        default,
        rename = "maxObjects",
        skip_serializing_if = "Option::is_none"
    )]
    max_objects: Option<i64>,
}

/// `spec.external` — a foreign, already-running S3-compatible store.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct ExternalBlock {
    /// The S3 endpoint of the foreign store.
    endpoint: String,
    /// The region, when the store needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    /// Name of an existing `kind: Secret` holding `accessKey` and `secretKey`.
    #[serde(rename = "credentialsSecret")]
    credentials_secret: String,
}

/// Names accepted in the `kind: ObjectStorage` `spec`, for the unknown-field
/// warning. Must carry every accepted spelling — the specs have no
/// `deny_unknown_fields` (see `manifest::warn_unknown_fields`).
pub(crate) const OBJECT_STORAGE_SPEC_FIELDS: &[&str] =
    &["backend", "bucket", "cephRgw", "external"];

/// Accepted keys of `spec.cephRgw` (drift-guard + nested unknown-field warning).
pub(crate) const CEPH_RGW_FIELDS: &[&str] = &["uid", "displayName", "endpoint", "quota"];

/// Accepted keys of `spec.cephRgw.quota`.
pub(crate) const QUOTA_FIELDS: &[&str] = &["maxSize", "maxObjects"];

/// Accepted keys of `spec.external`.
pub(crate) const EXTERNAL_FIELDS: &[&str] = &["endpoint", "region", "credentialsSecret"];

/// The fields the reconciler compares. All of them converge hot (see
/// `reconcile::hot_fields`): the apply re-provisions (idempotent) and rewrites
/// the record and the Secret, so no change recreates anything. `displayName` is
/// deliberately NOT compared — it is cosmetic and re-applied every time.
pub(crate) const RECONCILED_OBJECT_STORAGE_FIELDS: &[&str] = &[
    "backend",
    "bucket",
    "uid",
    "endpoint",
    "region",
    "credentialsSecret",
    "maxSize",
    "maxObjects",
];

/// Warns about unknown keys INSIDE the `cephRgw:`/`external:` blocks — the
/// top-level guard only sees the spec's own keys. Called from the manifest load.
pub(crate) fn warn_unknown_nested(doc: &ManifestDoc) {
    manifest::warn_unknown_fields_in(doc, "cephRgw", CEPH_RGW_FIELDS);
    manifest::warn_unknown_fields_in(doc, "cephRgw.quota", QUOTA_FIELDS);
    manifest::warn_unknown_fields_in(doc, "external", EXTERNAL_FIELDS);
}

impl ObjectStorageSpec {
    /// The S3 user id for the `cephRgw` backend — the declared one, or the
    /// document name when none is given.
    fn uid(&self, name: &str) -> String {
        self.ceph_rgw
            .as_ref()
            .and_then(|c| c.uid.clone())
            .unwrap_or_else(|| name.to_string())
    }

    /// Refuses a spec whose backend and blocks disagree, or that is incomplete.
    ///
    /// The block's presence is NOT the discriminator — `backend:` is — so a
    /// `cephRgw:` block under `backend: external` is two contradictory
    /// declarations in one document, and honouring one in silence is the
    /// fail-open this engine keeps removing. The refusal names what it found.
    pub(crate) fn check(&self, name: &str) -> Result<()> {
        if self.bucket.trim().is_empty() {
            return Err(Error::Invalid(super::po::tf(
                "ObjectStorage '{name}': `bucket` is required",
                &[("name", name)],
            )));
        }
        match self.backend.as_str() {
            BACKEND_CEPH_RGW => {
                if self.external.is_some() {
                    return Err(Error::Invalid(super::po::tf(
                        "ObjectStorage '{name}': backend is `cephRgw`, so an `external:` block does not belong — it is the declaration for the `external` backend",
                        &[("name", name)],
                    )));
                }
                Ok(())
            }
            BACKEND_EXTERNAL => {
                if self.ceph_rgw.is_some() {
                    return Err(Error::Invalid(super::po::tf(
                        "ObjectStorage '{name}': backend is `external`, so a `cephRgw:` block does not belong",
                        &[("name", name)],
                    )));
                }
                let ext = self.external.as_ref().ok_or_else(|| {
                    Error::Invalid(super::po::tf(
                        "ObjectStorage '{name}': backend `external` needs an `external:` block with an `endpoint` and a `credentialsSecret`",
                        &[("name", name)],
                    ))
                })?;
                if ext.endpoint.trim().is_empty() || ext.credentials_secret.trim().is_empty() {
                    return Err(Error::Invalid(super::po::tf(
                        "ObjectStorage '{name}': the `external:` block needs a non-empty `endpoint` and `credentialsSecret`",
                        &[("name", name)],
                    )));
                }
                Ok(())
            }
            other => Err(Error::Invalid(super::po::tf(
                "ObjectStorage '{name}': unknown backend '{backend}' — use `cephRgw` or `external`",
                &[("name", name), ("backend", other)],
            ))),
        }
    }
}

/// One ObjectStorage, as the node remembers it. The source of truth for the
/// reconciler's `actual()` — which is what makes the resource plannable and
/// stampable without having to re-read the whole Ceph cluster on every plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ObjectStorageRecord {
    name: String,
    backend: String,
    bucket: String,
    /// `cephRgw`: the provisioned S3 user id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uid: Option<String>,
    /// The S3 endpoint exposed to consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    /// `cephRgw`: the user-quota ceiling on total bytes, if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_size: Option<String>,
    /// `cephRgw`: the user-quota ceiling on object count, if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_objects: Option<i64>,
    /// `external`: the region, when the store needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    /// `external`: the referenced credentials Secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credentials_secret: Option<String>,
    /// The Secret a consumer reads for the usable credentials: the one generated
    /// here (`cephRgw`) or the one the manifest referenced (`external`).
    credentials_secret_out: String,
    /// The owning stack (reconcile stamp).
    #[serde(default)]
    owner: Option<String>,
    /// The field map last applied (reconcile three-way diff).
    #[serde(default)]
    last_applied: BTreeMap<String, String>,
}

fn store() -> Result<JsonStore<ObjectStorageRecord>> {
    JsonStore::open(state_root().join(STORE_DIR)).map_err(Into::into)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The manifest-named fields the reconciler compares, built from the spec.
/// `record_fields` MUST mirror this exactly, or a freshly-applied store reads as
/// drift forever.
fn fields_of(spec: &ObjectStorageSpec, name: &str) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("backend".into(), spec.backend.clone());
    f.insert("bucket".into(), spec.bucket.clone());
    match spec.backend.as_str() {
        BACKEND_CEPH_RGW => {
            f.insert("uid".into(), spec.uid(name));
            if let Some(e) = spec.ceph_rgw.as_ref().and_then(|c| c.endpoint.clone()) {
                f.insert("endpoint".into(), e);
            }
            if let Some(q) = spec.ceph_rgw.as_ref().and_then(|c| c.quota.as_ref()) {
                if let Some(s) = &q.max_size {
                    f.insert("maxSize".into(), s.clone());
                }
                if let Some(n) = q.max_objects {
                    f.insert("maxObjects".into(), n.to_string());
                }
            }
        }
        BACKEND_EXTERNAL => {
            if let Some(ext) = &spec.external {
                f.insert("endpoint".into(), ext.endpoint.clone());
                if let Some(r) = &ext.region {
                    f.insert("region".into(), r.clone());
                }
                f.insert("credentialsSecret".into(), ext.credentials_secret.clone());
            }
        }
        _ => {}
    }
    f
}

/// The same fields, read back off a record. The mirror of [`fields_of`].
fn record_fields(r: &ObjectStorageRecord) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("backend".into(), r.backend.clone());
    f.insert("bucket".into(), r.bucket.clone());
    match r.backend.as_str() {
        BACKEND_CEPH_RGW => {
            if let Some(u) = &r.uid {
                f.insert("uid".into(), u.clone());
            }
            if let Some(e) = &r.endpoint {
                f.insert("endpoint".into(), e.clone());
            }
            if let Some(s) = &r.max_size {
                f.insert("maxSize".into(), s.clone());
            }
            if let Some(n) = r.max_objects {
                f.insert("maxObjects".into(), n.to_string());
            }
        }
        BACKEND_EXTERNAL => {
            if let Some(e) = &r.endpoint {
                f.insert("endpoint".into(), e.clone());
            }
            if let Some(r) = &r.region {
                f.insert("region".into(), r.clone());
            }
            if let Some(cs) = &r.credentials_secret {
                f.insert("credentialsSecret".into(), cs.clone());
            }
        }
        _ => {}
    }
    f
}

/// Maps a gateway failure onto the engine's error, keeping the message.
fn rgw_error(e: delonix_provider_ceph::RgwError) -> Error {
    use delonix_provider_ceph::RgwError::*;
    match e {
        NotFound(m) => Error::NotFound(m),
        Unavailable(m) => Error::Invalid(super::po::tf(
            "the Ceph object gateway is not administrable on this node: {msg}",
            &[("msg", &m)],
        )),
        Undetermined(m) => Error::Invalid(super::po::tf(
            "the Ceph object gateway could not be reached: {msg}",
            &[("msg", &m)],
        )),
    }
}

/// Provisions (idempotently) the tenant's S3 identity on the node's gateway.
/// Reached through `radosgw-admin` as `client.admin`, exactly as the node API's
/// `StorageService::CreateObjectStoreUser` does — this is the same call,
/// in-process.
/// The node's object gateway, reached through `radosgw-admin` as `client.admin`
/// — the same resolution the node API's `gateway()` uses (`CEPH_CONF` when set,
/// else the system default the tools find on their own).
fn gateway() -> delonix_provider_ceph::Rgw {
    let run: Arc<dyn delonix_storage::exec::Run> = Arc::new(delonix_volume::pool_exec::SystemRun);
    let conf = std::env::var_os("CEPH_CONF").map(std::path::PathBuf::from);
    delonix_provider_ceph::Rgw::new(run, conf)
}

/// Provisions (idempotently) the tenant's S3 identity on the node's gateway.
/// Exactly what the node API's `StorageService::CreateObjectStoreUser` does —
/// this is the same call, in-process.
fn provision_rgw_user(uid: &str, display: &str) -> Result<delonix_provider_ceph::RgwUser> {
    gateway().ensure_user(uid, display).map_err(rgw_error)
}

/// The credentials a consumer needs, written into a generated `kind: Secret`.
struct Credentials {
    access_key: String,
    secret_key: String,
    endpoint: Option<String>,
    bucket: String,
}

fn write_credentials_secret(secret_name: &str, c: &Credentials) -> Result<()> {
    let store = SecretStore::open(state_root())?;
    let mut data = BTreeMap::new();
    data.insert("accessKey".to_string(), c.access_key.clone());
    data.insert("secretKey".to_string(), c.secret_key.clone());
    data.insert("bucket".to_string(), c.bucket.clone());
    if let Some(e) = &c.endpoint {
        data.insert("endpoint".to_string(), e.clone());
    }
    store.save(&Secret {
        name: secret_name.to_string(),
        data,
        updated_unix: now_unix(),
        version: 0,
    })?;
    Ok(())
}

/// Provisions/validates ONE document and writes its record (+ the generated
/// Secret, for `cephRgw`). Idempotent — this is both the apply and the converge.
fn materialize(doc: &ManifestDoc) -> Result<ObjectStorageRecord> {
    let name = doc.metadata.name.clone();
    let spec: ObjectStorageSpec = manifest::spec_of(doc)?;
    spec.check(&name)?;

    let store = store()?;
    let mut record = store.load(&name).ok().unwrap_or_default();
    record.name = name.clone();
    record.backend = spec.backend.clone();
    record.bucket = spec.bucket.clone();

    match spec.backend.as_str() {
        BACKEND_CEPH_RGW => {
            let uid = spec.uid(&name);
            let display = spec
                .ceph_rgw
                .as_ref()
                .and_then(|c| c.display_name.clone())
                .unwrap_or_else(|| uid.clone());
            let endpoint = spec.ceph_rgw.as_ref().and_then(|c| c.endpoint.clone());
            let user = provision_rgw_user(&uid, &display)?;
            // The ceiling is set AFTER the identity exists — quota set/enable
            // name the uid, which `ensure_user` has just guaranteed. Idempotent.
            let quota = spec.ceph_rgw.as_ref().and_then(|c| c.quota.clone());
            if let Some(q) = &quota {
                gateway()
                    .set_user_quota(&uid, q.max_size.as_deref(), q.max_objects)
                    .map_err(rgw_error)?;
            }
            let secret_name = format!("{name}-s3");
            write_credentials_secret(
                &secret_name,
                &Credentials {
                    access_key: user.access_key,
                    secret_key: user.secret_key,
                    endpoint: endpoint.clone(),
                    bucket: spec.bucket.clone(),
                },
            )?;
            record.uid = Some(uid);
            record.endpoint = endpoint;
            record.max_size = quota.as_ref().and_then(|q| q.max_size.clone());
            record.max_objects = quota.as_ref().and_then(|q| q.max_objects);
            record.region = None;
            record.credentials_secret = None;
            record.credentials_secret_out = secret_name;
        }
        BACKEND_EXTERNAL => {
            // BYO: nothing is provisioned. The Secret the manifest points at is
            // the consumer's credential source, used as-is.
            let ext = spec
                .external
                .as_ref()
                .expect("check() guarantees an external block for this backend");
            record.uid = None;
            record.endpoint = Some(ext.endpoint.clone());
            record.max_size = None;
            record.max_objects = None;
            record.region = ext.region.clone();
            record.credentials_secret = Some(ext.credentials_secret.clone());
            record.credentials_secret_out = ext.credentials_secret.clone();
        }
        _ => unreachable!("check() rejected any other backend"),
    }

    store.save(&name, &record)?;
    Ok(record)
}

/// Applies the `kind: ObjectStorage` documents (called by `stack apply`).
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, k::OBJECT_STORAGE) {
        let name = &doc.metadata.name;
        let rec = materialize(doc)?;
        match rec.backend.as_str() {
            BACKEND_CEPH_RGW => println!(
                "objectstorage/{name}: {}",
                super::po::tf(
                    "provisioned S3 user '{uid}', credentials in secret/{secret} (bucket '{bucket}')",
                    &[
                        ("uid", rec.uid.as_deref().unwrap_or("")),
                        ("secret", &rec.credentials_secret_out),
                        ("bucket", &rec.bucket),
                    ],
                )
            ),
            _ => println!(
                "objectstorage/{name}: {}",
                super::po::tf(
                    "external S3 at {endpoint}, credentials from secret/{secret} (bucket '{bucket}')",
                    &[
                        ("endpoint", rec.endpoint.as_deref().unwrap_or("")),
                        ("secret", &rec.credentials_secret_out),
                        ("bucket", &rec.bucket),
                    ],
                )
            ),
        }
    }
    Ok(())
}

/// What the manifest declares, for the reconciler.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: ObjectStorageSpec = manifest::spec_of(doc)?;
    spec.check(&doc.metadata.name)?;
    Ok(super::reconcile::Desired {
        kind: k::OBJECT_STORAGE.into(),
        name: doc.metadata.name.clone(),
        fields: fields_of(&spec, &doc.metadata.name),
        converges: true,
        ownable: true,
    })
}

/// What is on the machine, for the reconciler.
pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    let store = store()?;
    Ok(store
        .list()?
        .into_iter()
        .map(|r| super::reconcile::Actual {
            kind: k::OBJECT_STORAGE.into(),
            name: r.name.clone(),
            owner: r.owner.clone(),
            last_applied: (!r.last_applied.is_empty()).then(|| r.last_applied.clone()),
            fields: record_fields(&r),
        })
        .collect())
}

/// Records that this stack owns the resource, and what it last applied. The
/// apply layer runs before this step, so the record is already there.
pub(crate) fn stamp(name: &str, stack: &str, fields: &BTreeMap<String, String>) -> Result<()> {
    let store = store()?;
    store.update(name, |r| {
        r.owner = Some(stack.to_string());
        r.last_applied = fields.clone();
        true
    })?;
    Ok(())
}

/// Converges one resource: every comparable field converges hot by
/// re-materializing — provisioning is idempotent and the record and Secret are
/// rewritten in place, so nothing is recreated.
pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    materialize(doc)?;
    Ok(())
}

/// Whether the resource exists on the node, for `stack ls`/`wait`.
pub(crate) fn presence_of(doc: &ManifestDoc) -> (String, String) {
    match store() {
        Ok(s) => match s.load(&doc.metadata.name) {
            Ok(_) => ("yes".into(), super::po::t("provisioned").to_string()),
            Err(_) => ("no".into(), "-".into()),
        },
        Err(e) => ("?".into(), e.to_string()),
    }
}

// --------------------------------------------------------------------- CLI
//
// Imperative lifecycle alongside the declarative Kind: `ls`/`inspect` read the
// node records, and `rm` is the explicit teardown the Kind defers (its FACTS
// row is `teardown: false`, so `stack destroy`/`--prune` never removes an
// object store — this is the "remove one on purpose" path `no_teardown_reason`
// points at, done in one step instead of two manual commands).

/// `delonix objectstorage` — list, inspect and remove object stores.
#[derive(Subcommand)]
pub enum ObjectStorageCmd {
    /// List the object stores.
    Ls {
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        format: output::OutputFormat,
    },
    /// Details of an object store (backend, bucket, endpoint, credentials Secret).
    Inspect {
        name: String,
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        format: output::OutputFormat,
    },
    /// Remove an object store: its generated Secret and its record, plus the S3 identity on the gateway (cephRgw).
    Rm {
        name: String,
        /// Also delete the buckets and objects the S3 user still owns (cephRgw) — destructive; without it the gateway refuses a user that still holds data.
        #[arg(long)]
        purge: bool,
    },
}

/// A record as the CLI shows it — the fields a human/`jq` reads, never the
/// internal `last_applied`/`owner` bookkeeping.
#[derive(Serialize)]
struct ObjectStorageView {
    name: String,
    backend: String,
    bucket: String,
    endpoint: Option<String>,
    region: Option<String>,
    #[serde(rename = "credentialsSecret")]
    credentials_secret: String,
    #[serde(rename = "maxSize", skip_serializing_if = "Option::is_none")]
    max_size: Option<String>,
    #[serde(rename = "maxObjects", skip_serializing_if = "Option::is_none")]
    max_objects: Option<i64>,
    owner: Option<String>,
}

impl ObjectStorageView {
    fn of(r: &ObjectStorageRecord) -> Self {
        Self {
            name: r.name.clone(),
            backend: r.backend.clone(),
            bucket: r.bucket.clone(),
            endpoint: r.endpoint.clone(),
            region: r.region.clone(),
            credentials_secret: r.credentials_secret_out.clone(),
            max_size: r.max_size.clone(),
            max_objects: r.max_objects,
            owner: r.owner.clone(),
        }
    }
}

pub fn run(action: ObjectStorageCmd) -> Result<()> {
    match action {
        ObjectStorageCmd::Ls { format } => {
            let mut rows: Vec<ObjectStorageView> =
                store()?.list()?.iter().map(ObjectStorageView::of).collect();
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            match format {
                output::OutputFormat::Json => output::print_json(&rows)?,
                output::OutputFormat::Table => {
                    // Fixed-width header printed with the same widths as the
                    // rows below; the columns are literals by nature.
                    #[allow(clippy::print_literal)]
                    {
                        println!(
                            "{:<20} {:<9} {:<18} {:<28} {:<16} {}",
                            "NAME", "BACKEND", "BUCKET", "ENDPOINT", "SECRET", "OWNER"
                        );
                    }
                    for r in &rows {
                        println!(
                            "{:<20} {:<9} {:<18} {:<28} {:<16} {}",
                            r.name,
                            r.backend,
                            r.bucket,
                            r.endpoint.as_deref().unwrap_or("-"),
                            r.credentials_secret,
                            r.owner.as_deref().unwrap_or("-"),
                        );
                    }
                }
            }
            Ok(())
        }
        ObjectStorageCmd::Inspect { name, format } => {
            let r = store()?.load(&name).map_err(|_| {
                Error::NotFound(super::po::tf(
                    "no object store '{name}'",
                    &[("name", &name)],
                ))
            })?;
            let view = ObjectStorageView::of(&r);
            match format {
                output::OutputFormat::Json => output::print_json(std::slice::from_ref(&view))?,
                output::OutputFormat::Table => {
                    println!("Name:     {}", view.name);
                    println!("Backend:  {}", view.backend);
                    println!("Bucket:   {}", view.bucket);
                    println!("Endpoint: {}", view.endpoint.as_deref().unwrap_or("-"));
                    if let Some(region) = &view.region {
                        println!("Region:   {region}");
                    }
                    println!("Secret:   {}", view.credentials_secret);
                    if view.max_size.is_some() || view.max_objects.is_some() {
                        println!(
                            "Quota:    size {}, objects {}",
                            view.max_size.as_deref().unwrap_or("-"),
                            view.max_objects
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "-".into()),
                        );
                    }
                    println!("Owner:    {}", view.owner.as_deref().unwrap_or("-"));
                }
            }
            Ok(())
        }
        ObjectStorageCmd::Rm { name, purge } => {
            let store = store()?;
            let r = store.load(&name).map_err(|_| {
                Error::NotFound(super::po::tf(
                    "no object store '{name}'",
                    &[("name", &name)],
                ))
            })?;
            // cephRgw: the S3 identity and the generated Secret are OURS to
            // remove. external: the endpoint and the referenced Secret belong
            // to whoever declared them — only the local record goes.
            if r.backend == BACKEND_CEPH_RGW {
                if let Some(uid) = &r.uid {
                    gateway().remove_user(uid, purge).map_err(|e| match e {
                        delonix_provider_ceph::RgwError::Undetermined(m) if m.contains("buckets") => {
                            Error::Invalid(super::po::tf(
                                "object store '{name}': the S3 user still owns data — re-run with `--purge` to delete its buckets and objects too ({msg})",
                                &[("name", &name), ("msg", &m)],
                            ))
                        }
                        other => rgw_error(other),
                    })?;
                }
                SecretStore::open(state_root())?.remove(&r.credentials_secret_out)?;
            }
            store.remove(&name)?;
            println!(
                "objectstorage/{name}: {}",
                match r.backend.as_str() {
                    BACKEND_CEPH_RGW => super::po::tf(
                        "removed (S3 user '{uid}', secret/{secret}, record)",
                        &[
                            ("uid", r.uid.as_deref().unwrap_or("")),
                            ("secret", &r.credentials_secret_out),
                        ],
                    ),
                    _ => super::po::t("removed (record only — the external store and its Secret are left as they were)").to_string(),
                }
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(yaml: &str) -> ObjectStorageSpec {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn cephrgw_spec_is_accepted_and_defaults_the_uid_to_the_name() {
        let s = spec("backend: cephRgw\nbucket: media\ncephRgw:\n  endpoint: http://rgw:80\n");
        assert!(s.check("media").is_ok());
        assert_eq!(s.uid("media"), "media");
        let f = fields_of(&s, "media");
        assert_eq!(f.get("backend").map(String::as_str), Some("cephRgw"));
        assert_eq!(f.get("uid").map(String::as_str), Some("media"));
        assert_eq!(f.get("endpoint").map(String::as_str), Some("http://rgw:80"));
    }

    #[test]
    fn external_spec_is_accepted_with_endpoint_and_credentials() {
        let s = spec(
            "backend: external\nbucket: backups\nexternal:\n  endpoint: https://s3.example.com\n  region: eu-west-1\n  credentialsSecret: aws-s3\n",
        );
        assert!(s.check("backups").is_ok());
        let f = fields_of(&s, "backups");
        assert_eq!(
            f.get("endpoint").map(String::as_str),
            Some("https://s3.example.com")
        );
        assert_eq!(f.get("region").map(String::as_str), Some("eu-west-1"));
        assert_eq!(
            f.get("credentialsSecret").map(String::as_str),
            Some("aws-s3")
        );
    }

    #[test]
    fn a_cephrgw_backend_refuses_an_external_block() {
        let s = spec(
            "backend: cephRgw\nbucket: media\nexternal:\n  endpoint: https://s3.example.com\n  credentialsSecret: aws-s3\n",
        );
        let err = s.check("media").unwrap_err().to_string();
        assert!(err.contains("external"), "{err}");
    }

    #[test]
    fn an_external_backend_refuses_a_cephrgw_block() {
        let s = spec(
            "backend: external\nbucket: media\ncephRgw:\n  uid: media\nexternal:\n  endpoint: https://s3.example.com\n  credentialsSecret: aws-s3\n",
        );
        let err = s.check("media").unwrap_err().to_string();
        assert!(err.contains("cephRgw"), "{err}");
    }

    #[test]
    fn an_unknown_backend_is_refused_by_name() {
        let s = spec("backend: minio\nbucket: media\n");
        let err = s.check("media").unwrap_err().to_string();
        assert!(err.contains("minio"), "{err}");
    }

    #[test]
    fn an_empty_bucket_is_refused() {
        let s = spec("backend: cephRgw\nbucket: \"\"\n");
        assert!(s.check("media").is_err());
    }

    #[test]
    fn desired_fields_round_trip_through_a_record_as_noop() {
        // A record written from a spec must read back the SAME comparable fields,
        // or a freshly-applied store would show drift forever.
        for yaml in [
            "backend: cephRgw\nbucket: media\ncephRgw:\n  endpoint: http://rgw:80\n",
            "backend: cephRgw\nbucket: media\ncephRgw:\n  endpoint: http://rgw:80\n  quota: { maxSize: 100G, maxObjects: 10000 }\n",
            "backend: external\nbucket: backups\nexternal:\n  endpoint: https://s3.example.com\n  region: eu-west-1\n  credentialsSecret: aws-s3\n",
        ] {
            let s = spec(yaml);
            let desired = fields_of(&s, "x");
            let q = s.ceph_rgw.as_ref().and_then(|c| c.quota.clone());
            let rec = ObjectStorageRecord {
                name: "x".into(),
                backend: s.backend.clone(),
                bucket: s.bucket.clone(),
                uid: (s.backend == BACKEND_CEPH_RGW).then(|| s.uid("x")),
                endpoint: match s.backend.as_str() {
                    BACKEND_CEPH_RGW => s.ceph_rgw.as_ref().and_then(|c| c.endpoint.clone()),
                    _ => s.external.as_ref().map(|e| e.endpoint.clone()),
                },
                max_size: q.as_ref().and_then(|q| q.max_size.clone()),
                max_objects: q.as_ref().and_then(|q| q.max_objects),
                region: s.external.as_ref().and_then(|e| e.region.clone()),
                credentials_secret: s.external.as_ref().map(|e| e.credentials_secret.clone()),
                credentials_secret_out: "x-s3".into(),
                owner: None,
                last_applied: BTreeMap::new(),
            };
            assert_eq!(desired, record_fields(&rec), "drift for: {yaml}");
        }
    }

    #[test]
    fn cephrgw_quota_is_parsed_and_compared() {
        let s = spec(
            "backend: cephRgw\nbucket: media\ncephRgw:\n  quota: { maxSize: 100G, maxObjects: 10000 }\n",
        );
        assert!(s.check("media").is_ok());
        let f = fields_of(&s, "media");
        assert_eq!(f.get("maxSize").map(String::as_str), Some("100G"));
        assert_eq!(f.get("maxObjects").map(String::as_str), Some("10000"));
    }

    #[test]
    fn reconciled_fields_match_the_hot_table() {
        // Every comparable field converges hot (teardown is explicit for an
        // object store), so the two lists must agree.
        let mut reconciled = RECONCILED_OBJECT_STORAGE_FIELDS.to_vec();
        let mut hot = super::super::reconcile::hot_fields_for(k::OBJECT_STORAGE).to_vec();
        reconciled.sort_unstable();
        hot.sort_unstable();
        assert_eq!(reconciled, hot);
    }
}
