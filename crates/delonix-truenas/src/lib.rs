//! Provisioning against a TrueNAS SCALE appliance: create the dataset, its
//! quota, its share and its permissions, so a `kind: Volume` no longer requires
//! someone to have made them by hand first.
//!
//! **This crate only CREATES what lives on the NAS.** Mounting it is unchanged
//! — a provisioned volume is mounted by exactly the same path as one that was
//! made by hand (`storage::build_mount` → `ensure_mounted` → `mount -t nfs`).
//! There is deliberately no second mounting mechanism (ADR-0009).
//!
//! # What was measured, not assumed
//!
//! Every shape below was exercised against a real TrueNAS SCALE 25.10.5
//! appliance (the one this repo builds in `scripts/appliances/`), because a
//! provisioner validated only against recorded responses does not meet this
//! repo's bar. Four findings shaped the design:
//!
//! 1. **Some operations are asynchronous jobs.** `POST /filesystem/setperm`
//!    returns a bare job id — `99`, not a result. Treating that number as
//!    success reports the permissions applied before anything has happened, and
//!    a job that fails does so *later*, where nobody is looking. [`wait_job`]
//!    polls to a terminal state and surfaces the job's own error text.
//! 2. **The permission endpoint moved.** `POST /pool/dataset/permission/id/{id}`
//!    is a 404 on 25.10; it is `/filesystem/setperm` now. This is exactly why
//!    [`Client::connect`] pins a major instead of trying to be liberal — a
//!    best-effort client that silently does the wrong thing on an unknown
//!    version is worse than one that refuses.
//! 3. **Numeric properties are objects, and their number can be null.** A
//!    dataset reports `quota: {"parsed": 1073741824, "rawvalue": "1073741824",
//!    …}` and, with no quota, `{"parsed": null, "rawvalue": "0", …}`. Reading
//!    `rawvalue` would turn "no quota" into the number 0 — the same
//!    unmeasured-is-not-zero distinction as `Usage { bytes, unreadable }`.
//! 4. **Deleting a dataset takes its NFS share with it.** Measured: after
//!    `DELETE /pool/dataset/id/tank%2Fx`, `GET /sharing/nfs` came back empty.
//!    Useful, but [`Client::remove_dataset`] still removes the share first and
//!    verifies — cascade behaviour is the appliance's to change, and a share
//!    left pointing at a path that no longer exists is a working export serving
//!    whatever gets created there next.

use delonix_runtime_core::{Error, Result};
use serde::Deserialize;
use std::time::{Duration, Instant};

/// The TrueNAS major this client speaks. The REST surface has moved between
/// majors (see finding 2 in the module docs), so a version it has not been
/// exercised against is refused by name rather than attempted.
pub const SUPPORTED_MAJOR: &str = "25";

/// The smallest quota TrueNAS accepts on a dataset. Measured, not read in a
/// manual: `quota: 536870912` came back
/// `422 … "Input should be greater than or equal to 1073741824"`.
///
/// It is checked HERE so the manifest gets a sentence instead of a pydantic
/// validation dump listing three failed constraints — and so it is checked
/// before anything has been created, not halfway through.
pub const MIN_QUOTA_BYTES: u64 = 1024 * 1024 * 1024;

/// How long to wait for an asynchronous job before giving up. Generous: a
/// recursive `setperm` over a populated dataset is real work on the appliance,
/// and the failure this guards against is a job that never reaches a terminal
/// state — not a slow one.
const JOB_TIMEOUT: Duration = Duration::from_secs(300);

/// How the client authenticates. An API key is the form to prefer — it is
/// revocable on the appliance without touching an account, and it is what a
/// `kind: Secret` should carry.
#[derive(Debug, Clone)]
pub enum Auth {
    /// `Authorization: Bearer <key>` — a TrueNAS API key.
    ApiKey(String),
    /// Account credentials. Accepted because a freshly installed appliance has
    /// an account before it has any API key, so requiring a key would make the
    /// first provisioning impossible without a trip to the web UI.
    Password { username: String, password: String },
}

/// Where to provision, and how to get in.
#[derive(Debug, Clone)]
pub struct Target {
    /// `https://<host>` (no path). The scheme is part of it: a target given as
    /// plain `http://` sends the credential in the clear, and that is the
    /// caller's explicit choice to make, not a default this code picks.
    pub base_url: String,
    pub auth: Auth,
    /// Accept a certificate this host cannot verify. A TrueNAS out of the box
    /// serves a self-signed certificate, so **many real targets need this** —
    /// but it disables the check that stops another machine from answering in
    /// the appliance's name, and with it goes the API key. Opt-in, never a
    /// fallback after a TLS error: silently retrying insecurely would mean the
    /// setting says one thing and the connection does another.
    pub insecure_tls: bool,
}

/// What to provision. `dataset` is the full ZFS path (`tank/projects/db`); the
/// pool has to exist already — creating pools is a storage-layout decision
/// about physical disks, not something a volume manifest should trigger.
#[derive(Debug, Clone)]
pub struct DatasetSpec {
    pub dataset: String,
    /// Bytes. `None` leaves whatever the dataset already has (and, on a new
    /// one, means unlimited).
    pub quota: Option<u64>,
    pub owner: Option<Owner>,
}

/// POSIX ownership applied to the dataset's mountpoint. The uid/gid are the
/// ones the CONSUMER will use — a container writing to the mounted share — so
/// they are numeric on purpose: a name would have to resolve on the appliance,
/// which has its own user database and no knowledge of ours.
#[derive(Debug, Clone, Copy)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
    /// Octal, as written (`0o770`). `None` leaves the mode alone.
    pub mode: Option<u32>,
}

/// An NFS export of the provisioned dataset.
#[derive(Debug, Clone, Default)]
pub struct NfsShareSpec {
    /// CIDRs allowed to mount. **Empty means every network can**, which is what
    /// the appliance does with an empty list — so the caller is the one that
    /// has to decide, and a manifest that says nothing gets told rather than
    /// silently exported to the world.
    pub networks: Vec<String>,
    /// Map the client's root to this account (`root` for read-write access from
    /// a container running as root inside its userns).
    pub maproot_user: Option<String>,
    pub maproot_group: Option<String>,
    pub read_only: bool,
}

/// What the appliance says exists after provisioning — read back, never echoed
/// from the request. The quota in particular: [`Provisioned::quota`] is what
/// the NAS reports it is enforcing, so a value it clamped or refused shows up
/// here as the truth instead of as the number we asked for.
#[derive(Debug, Clone)]
pub struct Provisioned {
    pub dataset: String,
    /// `/mnt/tank/…` on the appliance — the path an NFS export refers to.
    pub mountpoint: String,
    /// Bytes the NAS enforces. `None` = no quota (distinct from zero).
    pub quota: Option<u64>,
    /// Bytes free, as the NAS reports. `None` when it did not say.
    pub available: Option<u64>,
    /// Id of the NFS share, when one was ensured.
    pub nfs_share_id: Option<i64>,
}

// ===========================================================================
// Wire types
// ===========================================================================

/// A ZFS property as the API reports it. `parsed` is the typed value and is
/// `null` when the property has none — which is NOT the same as the `rawvalue`
/// string `"0"` that sits next to it (finding 3 in the module docs).
#[derive(Debug, Deserialize)]
struct Prop {
    #[serde(default)]
    parsed: Option<serde_json::Value>,
}

impl Prop {
    fn as_bytes(&self) -> Option<u64> {
        match self.parsed.as_ref()? {
            serde_json::Value::Number(n) => n.as_u64(),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Dataset {
    id: String,
    #[serde(default)]
    mountpoint: Option<String>,
    #[serde(default)]
    quota: Option<Prop>,
    #[serde(default)]
    available: Option<Prop>,
}

#[derive(Debug, Deserialize)]
struct SystemInfo {
    version: String,
}

#[derive(Debug, Deserialize)]
struct Job {
    state: String,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct NfsShare {
    id: i64,
    path: String,
}

// ===========================================================================
// Client
// ===========================================================================

pub struct Client {
    http: reqwest::blocking::Client,
    base: String,
    auth: Auth,
    /// The appliance's reported version, for error messages that name what we
    /// are actually talking to.
    version: String,
}

impl Client {
    /// Connects and refuses anything but [`SUPPORTED_MAJOR`].
    ///
    /// The version check is a real request, so it doubles as the credential
    /// check: a wrong API key fails here, before any provisioning has begun,
    /// rather than halfway through leaving a dataset without its share.
    pub fn connect(target: &Target) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .danger_accept_invalid_certs(target.insecure_tls)
            .build()
            .map_err(|e| {
                Error::Invalid(format!("truenas: could not build the HTTP client: {e}"))
            })?;
        let me = Self {
            http,
            base: target.base_url.trim_end_matches('/').to_string(),
            auth: target.auth.clone(),
            version: String::new(),
        };
        let info: SystemInfo = me.get("/system/info")?;
        let major = info.version.split('.').next().unwrap_or("").to_string();
        if major != SUPPORTED_MAJOR {
            return Err(Error::Invalid(format!(
                "truenas {} at {} is not supported: this build speaks the {}.x REST API. \
                 The surface moves between majors — `/pool/dataset/permission` became \
                 `/filesystem/setperm`, for one — so provisioning against an untested major \
                 would fail somewhere in the middle, not here",
                info.version, me.base, SUPPORTED_MAJOR
            )));
        }
        Ok(Self {
            version: info.version,
            ..me
        })
    }

    /// The appliance version this client is talking to.
    pub fn version(&self) -> &str {
        &self.version
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v2.0{path}", self.base)
    }

    fn authed(&self, rb: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        match &self.auth {
            Auth::ApiKey(k) => rb.bearer_auth(k),
            Auth::Password { username, password } => rb.basic_auth(username, Some(password)),
        }
    }

    /// Sends a request and turns a non-2xx into an error carrying the body.
    ///
    /// The body matters: TrueNAS puts the actionable part there and nowhere
    /// else — `[EINVAL] pool_create.topology: Disks have duplicate serial
    /// numbers` was a real response, and a client that reported only "HTTP 422"
    /// would have sent someone hunting through the wrong subsystem.
    fn send(&self, rb: reqwest::blocking::RequestBuilder) -> Result<String> {
        let resp = self
            .authed(rb)
            .send()
            .map_err(|e| Error::Invalid(format!("truenas: request failed: {e}")))?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            let detail = body.trim();
            let detail = if detail.is_empty() {
                String::new()
            } else {
                format!(": {}", &detail[..detail.len().min(400)])
            };
            return Err(Error::Invalid(format!(
                "truenas: {} returned HTTP {}{detail}",
                self.base, status
            )));
        }
        Ok(body)
    }

    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let body = self.send(self.http.get(self.url(path)))?;
        parse_json(&body, path)
    }

    fn post<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        payload: &serde_json::Value,
    ) -> Result<T> {
        let body = self.send(self.http.post(self.url(path)).json(payload))?;
        parse_json(&body, path)
    }

    /// Waits for an asynchronous job to reach a terminal state.
    ///
    /// Several endpoints (`filesystem/setperm`, `pool`) answer with a bare job
    /// id, so "the POST succeeded" says only that the work was *accepted*.
    /// Returning at that point is how a provisioner reports permissions applied
    /// on a dataset that is still world-readable.
    fn wait_job(&self, id: i64) -> Result<()> {
        let deadline = Instant::now() + JOB_TIMEOUT;
        loop {
            let jobs: Vec<Job> = self.get(&format!("/core/get_jobs?id={id}"))?;
            let job = jobs.into_iter().next().ok_or_else(|| {
                Error::Invalid(format!(
                    "truenas: job {id} vanished before it finished — the appliance no longer \
                     reports it, so whether the work happened is unknown"
                ))
            })?;
            match job.state.as_str() {
                "SUCCESS" => return Ok(()),
                "FAILED" | "ABORTED" => {
                    let why = job
                        .error
                        .as_ref()
                        .and_then(|e| e.as_str().map(str::to_string))
                        .or_else(|| job.error.as_ref().map(|e| e.to_string()))
                        .unwrap_or_else(|| "no reason given".into());
                    return Err(Error::Invalid(format!(
                        "truenas: job {id} {}: {}",
                        job.state.to_lowercase(),
                        why.trim()
                    )));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(Error::Invalid(format!(
                    "truenas: job {id} was still '{}' after {}s — giving up. It may still be \
                     running on the appliance; nothing here was rolled back",
                    job.state,
                    JOB_TIMEOUT.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(750));
        }
    }

    /// Reads a dataset, or `None` if it does not exist.
    fn get_dataset(&self, dataset: &str) -> Result<Option<Dataset>> {
        let path = format!("/pool/dataset/id/{}", encode_id(dataset));
        let resp = self
            .authed(self.http.get(self.url(&path)))
            .send()
            .map_err(|e| Error::Invalid(format!("truenas: request failed: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(Error::Invalid(format!(
                "truenas: reading dataset '{dataset}' returned HTTP {status}: {}",
                body.trim()
            )));
        }
        parse_json(&body, &path).map(Some)
    }

    /// Creates the dataset if it is missing, aligns its quota if it is not, and
    /// reports what the appliance says it now has.
    ///
    /// Idempotent by design: this is called from a declarative apply, which may
    /// run any number of times over the same manifest.
    pub fn ensure_dataset(&self, spec: &DatasetSpec) -> Result<Provisioned> {
        validate_dataset_name(&spec.dataset)?;
        validate_quota(spec.quota)?;
        let existing = self.get_dataset(&spec.dataset)?;
        let ds: Dataset = match existing {
            None => {
                let mut payload = serde_json::json!({
                    "name": spec.dataset,
                    "type": "FILESYSTEM",
                });
                if let Some(q) = spec.quota {
                    payload["quota"] = serde_json::json!(q);
                }
                self.post("/pool/dataset", &payload)?
            }
            Some(ds) => {
                let current = ds.quota.as_ref().and_then(Prop::as_bytes);
                if let Some(want) = spec.quota {
                    if current != Some(want) {
                        let path = format!("/pool/dataset/id/{}", encode_id(&spec.dataset));
                        let body = self.send(
                            self.http
                                .put(self.url(&path))
                                .json(&serde_json::json!({ "quota": want })),
                        )?;
                        parse_json(&body, &path)?
                    } else {
                        ds
                    }
                } else {
                    ds
                }
            }
        };

        let mountpoint = ds.mountpoint.clone().ok_or_else(|| {
            Error::Invalid(format!(
                "truenas: dataset '{}' reports no mountpoint — it cannot be shared or mounted",
                ds.id
            ))
        })?;

        if let Some(owner) = spec.owner {
            self.set_permissions(&mountpoint, owner)?;
        }

        // Read back rather than echo. The quota reported here is what the NAS
        // enforces; a value it clamped or ignored has to surface as the truth,
        // not as the number that was asked for.
        let fresh = self.get_dataset(&spec.dataset)?.ok_or_else(|| {
            Error::Invalid(format!(
                "truenas: dataset '{}' was not there when read back immediately after \
                 provisioning it",
                spec.dataset
            ))
        })?;
        Ok(Provisioned {
            dataset: fresh.id,
            mountpoint,
            quota: fresh.quota.as_ref().and_then(Prop::as_bytes),
            available: fresh.available.as_ref().and_then(Prop::as_bytes),
            nfs_share_id: None,
        })
    }

    /// Applies ownership/mode to a path. Asynchronous on the appliance, so this
    /// waits for the job rather than returning when it is queued.
    pub fn set_permissions(&self, path: &str, owner: Owner) -> Result<()> {
        let mut payload = serde_json::json!({
            "path": path,
            "uid": owner.uid,
            "gid": owner.gid,
            "options": { "recursive": true, "traverse": false },
        });
        if let Some(mode) = owner.mode {
            // The appliance wants the octal digits as a string ("770").
            payload["mode"] = serde_json::json!(format!("{mode:o}"));
        }
        let id: i64 = self.post("/filesystem/setperm", &payload)?;
        self.wait_job(id)
    }

    /// Ensures an NFS export of `path` exists with the given rules.
    ///
    /// Matched by PATH, which is what makes it idempotent — the share id is the
    /// appliance's, not ours, and nothing in a manifest can carry it across
    /// applies.
    pub fn ensure_nfs_share(&self, path: &str, spec: &NfsShareSpec) -> Result<i64> {
        let shares: Vec<NfsShare> = self.get("/sharing/nfs")?;
        let mut payload = serde_json::json!({
            "path": path,
            "enabled": true,
            "ro": spec.read_only,
            "networks": spec.networks,
            "comment": "delonix",
        });
        if let Some(u) = &spec.maproot_user {
            payload["maproot_user"] = serde_json::json!(u);
        }
        if let Some(g) = &spec.maproot_group {
            payload["maproot_group"] = serde_json::json!(g);
        }
        let share: NfsShare = match shares.into_iter().find(|s| s.path == path) {
            Some(existing) => {
                let p = format!("/sharing/nfs/id/{}", existing.id);
                let body = self.send(self.http.put(self.url(&p)).json(&payload))?;
                parse_json(&body, &p)?
            }
            None => self.post("/sharing/nfs", &payload)?,
        };
        self.ensure_nfs_service()?;
        Ok(share.id)
    }

    /// Starts the NFS service AND marks it to start at boot.
    ///
    /// Both halves matter: `POST /service/start` leaves `enable: false`
    /// (measured), so a share provisioned this way serves fine until the
    /// appliance reboots and then stops — a failure that arrives days later,
    /// detached from anything anyone did.
    fn ensure_nfs_service(&self) -> Result<()> {
        let _: serde_json::Value =
            self.post("/service/start", &serde_json::json!({ "service": "nfs" }))?;
        let services: Vec<serde_json::Value> = self.get("/service?service=nfs")?;
        if let Some(svc) = services.first() {
            let enabled = svc.get("enable").and_then(|v| v.as_bool()).unwrap_or(false);
            if !enabled {
                if let Some(id) = svc.get("id").and_then(|v| v.as_i64()) {
                    let p = format!("/service/id/{id}");
                    self.send(
                        self.http
                            .put(self.url(&p))
                            .json(&serde_json::json!({ "enable": true })),
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Removes the NFS share of `path`, if there is one. Returns whether one
    /// was removed.
    pub fn remove_nfs_share(&self, path: &str) -> Result<bool> {
        let shares: Vec<NfsShare> = self.get("/sharing/nfs")?;
        match shares.into_iter().find(|s| s.path == path) {
            Some(s) => {
                self.send(
                    self.http
                        .delete(self.url(&format!("/sharing/nfs/id/{}", s.id))),
                )?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// **Destroys a dataset and everything in it.** There is no undo on the
    /// appliance side, and the data is not ours.
    ///
    /// The order is deliberate, and it is the one the v0.37.0 audit wrote down:
    /// take the export away FIRST, destroy the data LAST. Reversed, there is a
    /// window in which the dataset is gone and a working NFS export still
    /// points at its path — so whatever gets created there next is served to
    /// the same clients under the same name.
    ///
    /// Callers must have decided that this dataset is theirs to destroy. This
    /// function does not infer ownership; `kind: Volume` gates it behind an
    /// explicit opt-in of its own.
    pub fn remove_dataset(&self, dataset: &str, recursive: bool) -> Result<()> {
        validate_dataset_name(dataset)?;
        let Some(ds) = self.get_dataset(dataset)? else {
            // Idempotent: already gone is the desired state, not an error.
            return Ok(());
        };
        if let Some(mp) = ds.mountpoint.as_deref() {
            self.remove_nfs_share(mp)?;
        }
        let path = format!("/pool/dataset/id/{}", encode_id(dataset));
        self.send(
            self.http
                .delete(self.url(&path))
                .json(&serde_json::json!({ "recursive": recursive })),
        )?;
        if self.get_dataset(dataset)?.is_some() {
            return Err(Error::Invalid(format!(
                "truenas: dataset '{dataset}' is still there after the delete reported success"
            )));
        }
        Ok(())
    }
}

// ===========================================================================
// Pure helpers
// ===========================================================================

fn parse_json<T: for<'de> Deserialize<'de>>(body: &str, path: &str) -> Result<T> {
    serde_json::from_str(body).map_err(|e| {
        Error::Invalid(format!(
            "truenas: could not read the answer from {path}: {e} (body starts: {})",
            &body[..body.len().min(160)]
        ))
    })
}

/// Percent-encodes a dataset id for use as a URL path segment. Only `/` needs
/// it — a dataset name is already restricted to a safe alphabet by
/// [`validate_dataset_name`], which runs first at every call site.
fn encode_id(dataset: &str) -> String {
    dataset.replace('/', "%2F")
}

/// Checks a requested quota against what the appliance will accept.
///
/// A quota under [`MIN_QUOTA_BYTES`] is refused rather than rounded up. Growing
/// someone's limit to eight times what they wrote, silently, is how a dataset
/// meant to hold 128 MiB quietly fills a pool — and this repo treats
/// accepted-then-altered as worse than refused.
pub fn validate_quota(quota: Option<u64>) -> Result<()> {
    match quota {
        None => Ok(()),
        Some(0) => Ok(()), // the appliance's own spelling of "no quota"
        Some(q) if q >= MIN_QUOTA_BYTES => Ok(()),
        Some(q) => Err(Error::Invalid(format!(
            "quota of {q} bytes is below the {} TrueNAS enforces as its minimum \
             ({} GiB) — ask for at least that, or drop the quota to leave it unlimited",
            MIN_QUOTA_BYTES,
            MIN_QUOTA_BYTES / (1024 * 1024 * 1024)
        ))),
    }
}

/// A dataset name has to be a plain ZFS path, and this is a security boundary
/// rather than a nicety: the value is interpolated into a URL path, and it also
/// names what [`Client::remove_dataset`] destroys.
///
/// `..` is refused outright. ZFS has no parent traversal, so it can only be an
/// attempt to make one path mean another — and the destination of that attempt
/// is a delete on somebody else's machine.
pub fn validate_dataset_name(dataset: &str) -> Result<()> {
    let bad = |why: &str| {
        Err(Error::Invalid(format!(
            "invalid dataset '{dataset}': {why} (expected `<pool>/<name>`, e.g. `tank/projects`)"
        )))
    };
    if dataset.is_empty() {
        return bad("it is empty");
    }
    if dataset.len() > 255 {
        return bad("it is longer than 255 characters");
    }
    if dataset.starts_with('/') || dataset.ends_with('/') {
        return bad("it starts or ends with '/'");
    }
    if !dataset.contains('/') {
        return bad("it names a pool with no dataset under it");
    }
    for part in dataset.split('/') {
        if part.is_empty() {
            return bad("it has an empty path component");
        }
        if part == "." || part == ".." {
            return bad("'.' and '..' are not path components in ZFS");
        }
        if part.starts_with('-') {
            return bad("a component starts with '-'");
        }
    }
    if let Some(c) = dataset
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || "-_.:/".contains(*c)))
    {
        return bad(&format!("it contains '{c}'"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nome_de_dataset_recusa_travessia_e_lixo() {
        // The shapes that matter are the ones that would make a delete land
        // somewhere other than where the manifest says.
        for bad in [
            "",
            "tank", // a pool alone: `remove_dataset` on it is the whole pool
            "/tank/x",
            "tank/x/",
            "tank//x",
            "tank/../other",
            "tank/..",
            "tank/x y",
            "tank/x;rm",
            "tank/x?a=b", // would end the path and start a query
            "tank/-x",
            "tank/x\nY",
        ] {
            assert!(
                validate_dataset_name(bad).is_err(),
                "should have refused {bad:?}"
            );
        }
        for ok in ["tank/x", "tank/a/b/c", "pool-1/data_2", "tank/x.snap:1"] {
            assert!(
                validate_dataset_name(ok).is_ok(),
                "should have taken {ok:?}"
            );
        }
    }

    #[test]
    fn so_a_barra_e_codificada_no_id() {
        assert_eq!(encode_id("tank/x"), "tank%2Fx");
        assert_eq!(encode_id("tank/a/b"), "tank%2Fa%2Fb");
    }

    #[test]
    fn uma_propriedade_sem_valor_e_desconhecida_e_nao_zero() {
        // The exact shape a real appliance returns for a dataset with no quota:
        // `parsed` is null while `rawvalue` sits there saying "0". Reading the
        // string would report a quota of zero bytes — a limit that allows
        // nothing — for a dataset that has no limit at all.
        let p: Prop =
            serde_json::from_str(r#"{"parsed":null,"rawvalue":"0","value":null}"#).unwrap();
        assert_eq!(p.as_bytes(), None);
        let p: Prop =
            serde_json::from_str(r#"{"parsed":1073741824,"rawvalue":"1073741824"}"#).unwrap();
        assert_eq!(p.as_bytes(), Some(1073741824));
        // A non-numeric parsed value is not a byte count either.
        let p: Prop = serde_json::from_str(r#"{"parsed":"1 GiB"}"#).unwrap();
        assert_eq!(p.as_bytes(), None);
    }

    #[test]
    fn um_job_falhado_carrega_a_razao_da_appliance() {
        let j: Job = serde_json::from_str(
            r#"{"state":"FAILED","error":"[EINVAL] pool_create.topology: Disks have duplicate serial numbers"}"#,
        )
        .unwrap();
        assert_eq!(j.state, "FAILED");
        assert!(j.error.unwrap().as_str().unwrap().contains("EINVAL"));
    }
}
