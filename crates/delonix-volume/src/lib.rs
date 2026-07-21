//! `delonix-volume` — named volumes and *bind mounts* for the Delonix Engine.
//!
//! Two kinds of mount, both **zero-copy** (the kernel shares the blocks via
//! `MS_BIND`, there is no data copy):
//! - **named volume**: a directory managed by Delonix at
//!   `<root>/volumes/<name>/_data`, which **survives** the container;
//! - **bind mount**: an arbitrary host path, mounted into the container.
//!
//! The `-v` syntax follows Docker: `name:/target` (volume) or
//! `/host/path:/target` (bind), with an optional `:ro` for read-only.

use delonix_runtime_core::{Error, Mount, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Metadata of a named volume.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Volume {
    /// The volume name.
    pub name: String,
    /// The data directory on the host (`.../_data`).
    pub mountpoint: String,
    /// Creation instant (Unix seconds).
    pub created_unix: u64,
    /// Driver: `local` (default) or `nfs` (external TrueNAS/NFS).
    #[serde(default = "default_driver")]
    pub driver: String,
    /// For `nfs`: the *export* (`server:/path`).
    #[serde(default)]
    pub device: Option<String>,
    /// Mount options (`mount -o ...`), e.g.: `vers=4,ro`.
    #[serde(default)]
    pub options: Option<String>,
    /// Size quota in bytes (`--quota`). `None` = no limit. With privilege
    /// (root model) it is a HARD cap via a loop-mounted ext4 image; in rootless it is
    /// a MONITORED limit (measured usage, alert near the limit). [[hybrid #7]]
    #[serde(default)]
    pub quota_bytes: Option<u64>,
    /// Usage percentage above which an alert is raised (default 90).
    #[serde(default)]
    pub alert_pct: Option<u8>,
}

/// The drivers that mount a network share (as opposed to `local`/loopback).
pub fn is_network_driver(driver: &str) -> bool {
    matches!(driver, "nfs" | "cifs" | "smb" | "webdav" | "dav")
}

/// The `mount` `-t <fstype>` for each network driver. `smb` is an alias of
/// `cifs` (the kernel only knows `cifs`); `dav` of `webdav` (`davfs`).
fn mount_fstype(driver: &str) -> &'static str {
    match driver {
        "cifs" | "smb" => "cifs",
        "webdav" | "dav" => "davfs",
        _ => "nfs",
    }
}

fn default_driver() -> String {
    "local".to_string()
}

/// Human size (`512m`, `2g`, `10G`, `1048576`) → bytes. Binary suffixes
/// (k=1024, m=1024², g=1024³, t=1024⁴); a trailing `b`/`B` is accepted. `None` if invalid.
pub fn parse_size_bytes(s: &str) -> Option<u64> {
    let lower = s.trim().to_lowercase();
    let body = lower.strip_suffix('b').unwrap_or(lower.as_str());
    let (num, mult) = match body.chars().last() {
        Some('k') => (&body[..body.len() - 1], 1024u64),
        Some('m') => (&body[..body.len() - 1], 1024 * 1024),
        Some('g') => (&body[..body.len() - 1], 1024 * 1024 * 1024),
        Some('t') => (&body[..body.len() - 1], 1024u64.pow(4)),
        _ => (body, 1),
    };
    let n: f64 = num.trim().parse().ok()?;
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    Some((n * mult as f64) as u64)
}

/// The volume store, under `<root>/volumes`.
pub struct VolumeStore {
    root: PathBuf,
}

impl VolumeStore {
    /// Opens (creating) the volume store.
    pub fn open(base: impl Into<PathBuf>) -> Result<Self> {
        let root = base.into().join("volumes");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// The root directory of a volume (`<root>/volumes/<name>`) — for recovery
    /// operations in the CLI (e.g.: rm of orphans/subuids in a mapped userns).
    pub fn volume_dir(&self, name: &str) -> PathBuf {
        self.dir(name)
    }
    fn data_dir(&self, name: &str) -> PathBuf {
        self.dir(name).join("_data")
    }
    fn meta_path(&self, name: &str) -> PathBuf {
        self.dir(name).join("meta.json")
    }

    fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    }

    /// Creates a `local` volume (idempotent: returns the existing one if it already exists).
    pub fn create(&self, name: &str) -> Result<Volume> {
        if self.meta_path(name).exists() {
            return self.inspect(name); // preserves the driver/device of an already-created volume
        }
        self.create_with(name, "local", None, None)
    }

    /// Creates a volume with a driver (`local`/`nfs`). For `nfs`, it immediately
    /// mounts the *export* (`server:/path`) into the data directory — useful to
    /// connect to a TrueNAS or another NFS server. Idempotent.
    pub fn create_with(
        &self,
        name: &str,
        driver: &str,
        device: Option<String>,
        options: Option<String>,
    ) -> Result<Volume> {
        if !Self::valid_name(name) {
            return Err(Error::Invalid(format!("invalid volume name: {name:?}")));
        }
        if self.meta_path(name).exists() {
            let v = self.inspect(name)?;
            self.ensure_mounted(&v)?;
            return Ok(v);
        }
        // Network drivers require a `device` (the mount target): nfs
        // `server:/export`, cifs `//server/share`, webdav `https://…`.
        if is_network_driver(driver) && device.as_deref().unwrap_or("").is_empty() {
            return Err(Error::Invalid(format!(
                "{driver} volume requires a device (the mount target)"
            )));
        }
        let data = self.data_dir(name);
        fs::create_dir_all(&data)?;
        let vol = Volume {
            name: name.to_string(),
            mountpoint: data.to_string_lossy().into_owned(),
            created_unix: now_unix(),
            driver: driver.to_string(),
            device,
            options,
            quota_bytes: None,
            alert_pct: None,
        };
        // Mount BEFORE persisting: if NFS fails, we don't leave an orphan volume.
        if let Err(e) = self.ensure_mounted(&vol) {
            let _ = fs::remove_dir_all(self.dir(name));
            return Err(e);
        }
        fs::write(self.meta_path(name), serde_json::to_vec_pretty(&vol)?)?;
        Ok(vol)
    }

    /// Ensures a NETWORK volume is mounted. No-op for local volumes or
    /// if it is already mounted. Best-effort: requires the type's mount helper
    /// (`mount.nfs`, `mount.cifs`, `mount.davfs`) and, typically, privilege.
    ///
    /// Supported types and their respective `mount -t`:
    /// - `nfs`   → `mount -t nfs   server:/export`  (external TrueNAS/NFS)
    /// - `cifs`/`smb` → `mount -t cifs //server/share` (Samba/Windows/TrueNAS SMB)
    /// - `webdav`/`dav` → `mount -t davfs https://…`  (Nextcloud/ownCloud WebDAV)
    pub fn ensure_mounted(&self, vol: &Volume) -> Result<()> {
        // Volume with a HARD quota (ext4 loopback): remounts the image if unmounted
        // (e.g.: after a host reboot). Best-effort — without privilege, no-op.
        let img = self.loop_img(&vol.name);
        if vol.quota_bytes.is_some() && img.exists() && !is_mounted(&vol.mountpoint) {
            let _ = Self::run(
                "mount",
                &["-o", "loop", &img.to_string_lossy(), &vol.mountpoint],
            );
        }
        if !is_network_driver(&vol.driver) || is_mounted(&vol.mountpoint) {
            return Ok(());
        }
        let fstype = mount_fstype(&vol.driver);
        let device = vol.device.as_ref().ok_or_else(|| {
            Error::Invalid(format!(
                "{} volume '{}' has no device",
                vol.driver, vol.name
            ))
        })?;
        let mut args = vec!["-t", fstype, device.as_str(), vol.mountpoint.as_str()];
        if let Some(o) = &vol.options {
            args.push("-o");
            args.push(o);
        }
        let ctx: &'static str = match fstype {
            "cifs" => "mount cifs",
            "davfs" => "mount webdav",
            _ => "mount nfs",
        };
        let out = std::process::Command::new("mount")
            .args(&args)
            .output()
            .map_err(|e| Error::Runtime {
                context: ctx,
                message: e.to_string(),
            })?;
        if !out.status.success() {
            return Err(Error::Runtime {
                context: ctx,
                message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    /// Lists the existing volumes.
    pub fn list(&self) -> Result<Vec<Volume>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            let meta = path.join("meta.json");
            if meta.exists() {
                if let Ok(bytes) = fs::read(&meta) {
                    if let Ok(v) = serde_json::from_slice::<Volume>(&bytes) {
                        out.push(v);
                    }
                }
            }
        }
        out.sort_by_key(|v| std::cmp::Reverse(v.created_unix));
        Ok(out)
    }

    /// Inspects a volume by name.
    pub fn inspect(&self, name: &str) -> Result<Volume> {
        let meta = self.meta_path(name);
        if !meta.exists() {
            return Err(Error::NotFound(format!("volume {name}")));
        }
        Ok(serde_json::from_slice(&fs::read(meta)?)?)
    }

    // ---- Snapshots (Block B of the Odoo plan) ---------------------------------
    // A snapshot is a tar.gz of `_data`, stored in `<vol>/_snapshots/<snap>.tar.gz`
    // (survives the container; does NOT survive `volume rm` — it is a snapshot, not an
    // external backup). Crash-consistent: taken with the workload running; for
    // application consistency (e.g.: DB), the orchestrated backup (Block C) stops/dumps.
    // In rootless the tar runs in a mapped userns (effective owner of the subuids) — see the
    // CLI (`__volsnap`); this layer only knows about paths and listing.

    /// The snapshots directory of a volume.
    pub fn snapshots_dir(&self, name: &str) -> PathBuf {
        self.dir(name).join("_snapshots")
    }

    /// The file path of a snapshot (validates the name first).
    pub fn snapshot_path(&self, volume: &str, snap: &str) -> Result<PathBuf> {
        if !safe_snapshot_name(snap) {
            return Err(Error::Invalid(format!(
                "invalid snapshot name: '{snap}' (use [a-zA-Z0-9._-], no '/' or '..')"
            )));
        }
        Ok(self.snapshots_dir(volume).join(format!("{snap}.tar.gz")))
    }

    /// Lists the snapshots of a volume: `(name, bytes, mtime-unix)`.
    pub fn list_snapshots(&self, name: &str) -> Result<Vec<(String, u64, i64)>> {
        let dir = self.snapshots_dir(name);
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(&dir) else {
            return Ok(out);
        };
        for e in rd.flatten() {
            let p = e.path();
            let Some(fname) = p.file_name().and_then(|f| f.to_str()) else {
                continue;
            };
            let Some(snap) = fname.strip_suffix(".tar.gz") else {
                continue;
            };
            let md = e.metadata().ok();
            let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = md
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push((snap.to_string(), size, mtime));
        }
        out.sort_by_key(|s| s.2); // oldest first
        Ok(out)
    }

    /// Deletes a snapshot.
    pub fn remove_snapshot(&self, volume: &str, snap: &str) -> Result<()> {
        let p = self.snapshot_path(volume, snap)?;
        if !p.exists() {
            return Err(Error::NotFound(format!(
                "snapshot {snap} of volume {volume}"
            )));
        }
        fs::remove_file(p)?;
        Ok(())
    }

    /// Removes a volume (and its data). Unmounts first if it is `nfs`.
    pub fn remove(&self, name: &str) -> Result<()> {
        let dir = self.dir(name);
        if !dir.exists() {
            return Err(Error::NotFound(format!("volume {name}")));
        }
        if let Ok(v) = self.inspect(name) {
            // unmount nfs OR the hard-quota loopback before deleting the data.
            if (is_network_driver(&v.driver) || v.quota_bytes.is_some())
                && is_mounted(&v.mountpoint)
            {
                let _ = std::process::Command::new("umount")
                    .arg(&v.mountpoint)
                    .output();
            }
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    // ---- Quota (#7, hybrid) --------------------------------------------------
    // ROOT model (privileged): HARD cap via a loop-mounted ext4 image on `_data`
    // (writes fail with ENOSPC when full; resize2fs grows it hot). ROOTLESS model
    // (monitor): the quota is a measured limit — `usage()`+`over_quota()`
    // expose the state and the alert; there is no hard cap (losetup needs CAP_SYS_ADMIN).

    fn loop_img(&self, name: &str) -> PathBuf {
        self.dir(name).join("data.img")
    }

    /// REAL usage in bytes of the volume (`du` of `_data`, recursive). For volumes with
    /// loopback, reflects what is used inside the ext4; for local ones, the data size.
    pub fn usage(&self, name: &str) -> u64 {
        fn walk(p: &std::path::Path) -> u64 {
            let mut total = 0u64;
            if let Ok(rd) = fs::read_dir(p) {
                for e in rd.flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    if ft.is_dir() {
                        total += walk(&e.path());
                    } else if let Ok(m) = e.metadata() {
                        total += m.len();
                    }
                }
            }
            total
        }
        walk(&self.data_dir(name))
    }

    /// Is the volume at (or above) the alert threshold? `(in_alert, above_quota)`.
    pub fn quota_state(&self, vol: &Volume) -> (bool, bool) {
        match vol.quota_bytes {
            Some(q) if q > 0 => {
                let used = self.usage(&vol.name);
                let pct = vol.alert_pct.unwrap_or(90) as u64;
                (used * 100 >= q * pct, used >= q)
            }
            _ => (false, false),
        }
    }

    fn run(cmd: &str, args: &[&str]) -> Result<()> {
        let out = std::process::Command::new(cmd)
            .args(args)
            .output()
            .map_err(|e| Error::Runtime {
                context: "quota",
                message: format!("{cmd}: {e}"),
            })?;
        if !out.status.success() {
            return Err(Error::Runtime {
                context: "quota",
                message: format!(
                    "{cmd} {}: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            });
        }
        Ok(())
    }

    /// Finds the loop device serving the image (`losetup -j`), if any.
    fn loop_dev(img: &std::path::Path) -> Option<String> {
        let out = std::process::Command::new("losetup")
            .args(["-j", &img.to_string_lossy(), "-O", "NAME", "--noheadings"])
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        s.lines()
            .next()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
    }

    /// Ensures the ext4 image (privileged) with `quota` bytes is mounted on `_data`.
    /// Creates it the 1st time (empty volume) or resizes it hot (grows: truncate +
    /// online resize2fs). Returns `Err` if privilege/tools are missing.
    fn apply_loopback(&self, name: &str, quota: u64) -> Result<()> {
        let img = self.loop_img(name);
        let data = self.data_dir(name);
        let data_s = data.to_string_lossy().into_owned();
        if !img.exists() {
            // we only create a loopback over an EMPTY `_data` (otherwise we'd hide data).
            if self.usage(name) > 0 {
                return Err(Error::Invalid(
                    "hard quota (loopback) only on an empty volume; create with --quota or empty it first".into(),
                ));
            }
            // sparse image the size of the quota → ext4 → loop mount.
            Self::run(
                "truncate",
                &["-s", &quota.to_string(), &img.to_string_lossy()],
            )?;
            Self::run(
                "mkfs.ext4",
                &["-q", "-F", "-m", "0", &img.to_string_lossy()],
            )?;
            fs::create_dir_all(&data)?;
            Self::run("mount", &["-o", "loop", &img.to_string_lossy(), &data_s])?;
            return Ok(());
        }
        // image already exists → ensure mounted and resize to the new quota.
        if !is_mounted(&data_s) {
            Self::run("mount", &["-o", "loop", &img.to_string_lossy(), &data_s])?;
        }
        let cur = fs::metadata(&img).map(|m| m.len()).unwrap_or(0);
        if quota > cur {
            // GROW hot: increase the image and the fs (online).
            Self::run(
                "truncate",
                &["-s", &quota.to_string(), &img.to_string_lossy()],
            )?;
            let dev = Self::loop_dev(&img).ok_or_else(|| Error::Runtime {
                context: "quota",
                message: "loop device not found".into(),
            })?;
            Self::run("losetup", &["-c", &dev])?; // recognizes the backing's new size
            Self::run("resize2fs", &[&dev])?; // online grow
        } else if quota < cur {
            // SHRINK: ext4 does not shrink online — do it offline (unmount/resize/mount).
            // Refuses if busy (container in use) or if the quota < current usage.
            if self.usage(name) > quota {
                return Err(Error::Invalid(
                    "the new quota is smaller than the current usage — free up space first".into(),
                ));
            }
            if std::process::Command::new("umount")
                .arg(&data_s)
                .output()
                .map(|o| !o.status.success())
                .unwrap_or(true)
            {
                return Err(Error::Invalid(
                    "volume in use — stop the containers to shrink the quota".into(),
                ));
            }
            let blocks = format!("{}s", quota / 512); // resize2fs accepts size in sectors
                                                      // resize2fs needs e2fsck before shrinking; temporary loop.
            Self::run("e2fsck", &["-f", "-y", &img.to_string_lossy()]).ok();
            Self::run("resize2fs", &[&img.to_string_lossy(), &blocks])?;
            Self::run(
                "truncate",
                &["-s", &quota.to_string(), &img.to_string_lossy()],
            )?;
            Self::run("mount", &["-o", "loop", &img.to_string_lossy(), &data_s])?;
        }
        Ok(())
    }

    /// Sets (or removes) a volume's quota. `privileged` (root model) enables the
    /// HARD cap via ext4 loopback; otherwise it stays in MONITOR mode (only persists the limit).
    /// `quota=None` removes the limit (does not undo an already-created loopback).
    pub fn set_quota(
        &self,
        name: &str,
        quota: Option<u64>,
        alert_pct: Option<u8>,
        privileged: bool,
    ) -> Result<Volume> {
        let mut vol = self.inspect(name)?;
        if let (Some(q), true) = (quota, privileged) {
            self.apply_loopback(name, q)?;
        }
        vol.quota_bytes = quota;
        if alert_pct.is_some() {
            vol.alert_pct = alert_pct;
        }
        fs::write(self.meta_path(name), serde_json::to_vec_pretty(&vol)?)?;
        Ok(vol)
    }

    /// Translates a `-v` specification into a [`Mount`].
    ///
    /// - `name:/target[:ro]` → named volume (created if it does not exist);
    /// - `/host:/target[:ro]` (or `./rel`) → *bind mount* of a host path.
    pub fn resolve_spec(&self, spec: &str) -> Result<Mount> {
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() < 2 || parts.len() > 3 {
            return Err(Error::Invalid(format!(
                "invalid volume spec: {spec:?} (use source:/target[:ro])"
            )));
        }
        let src = parts[0];
        let target = parts[1];
        // 3rd field: only `ro`/`rw` recognized. Before, ANY other option
        // (`z`/`Z` SELinux, `U`, propagation) was SILENTLY ignored — the
        // bind mounted without the SELinux label and failed on RHEL/Fedora enforcing
        // with the user believing `:z` was handled. Fail-closed: explicit
        // error (finding from the Docker/Podman analysis; "no silent failure").
        let readonly = match parts.get(2) {
            None | Some(&"rw") => false,
            Some(&"ro") => true,
            Some(other) => {
                return Err(Error::Invalid(format!(
                    "unsupported bind option ':{other}' — only ':ro'/':rw' are supported (SELinux ':z'/':Z', ':U' and propagation are not implemented)"
                )))
            }
        };
        if !target.starts_with('/') {
            return Err(Error::Invalid(format!(
                "target must be absolute: {target:?}"
            )));
        }

        let source = if src.starts_with('/') || src.starts_with('.') {
            // bind mount of a host path
            let p = fs::canonicalize(src)
                .map_err(|_| Error::Invalid(format!("bind path does not exist: {src}")))?;
            p.to_string_lossy().into_owned()
        } else {
            // named volume (creates on demand, like Docker; mounts the NFS if applicable)
            let vol = self.create(src)?;
            self.ensure_mounted(&vol)?;
            vol.mountpoint
        };

        Ok(Mount {
            source,
            target: target.to_string(),
            readonly,
        })
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Safe snapshot name: `[A-Za-z0-9._-]+`, no path traversal.
pub fn safe_snapshot_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `true` if `path` is an active mount point (queries `/proc/mounts`).
fn is_mounted(path: &str) -> bool {
    fs::read_to_string("/proc/mounts")
        .map(|s| s.lines().any(|l| l.split_whitespace().nth(1) == Some(path)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_option_rejeita_selinux_e_desconhecidas() {
        // Fail-closed: an unsupported bind option (`:z`/`:Z` SELinux, `:U`,
        // propagation) gives an ERROR instead of being silently ignored.
        let tmp = std::env::temp_dir().join(format!("dlx-vol-bindopt-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = VolumeStore::open(&tmp).unwrap();
        let src = tmp.to_string_lossy();
        assert!(store.resolve_spec(&format!("{src}:/dst:z")).is_err());
        assert!(store.resolve_spec(&format!("{src}:/dst:Z")).is_err());
        assert!(store.resolve_spec(&format!("{src}:/dst:U")).is_err());
        // `ro`/`rw` still work (no regression).
        assert!(store.resolve_spec(&format!("{src}:/dst:ro")).is_ok());
        assert!(store.resolve_spec(&format!("{src}:/dst:rw")).is_ok());
        assert!(store.resolve_spec(&format!("{src}:/dst")).is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size_bytes("1024"), Some(1024));
        assert_eq!(parse_size_bytes("1k"), Some(1024));
        assert_eq!(parse_size_bytes("2m"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size_bytes("1g"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size_bytes("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size_bytes("512mb"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size_bytes("0"), None);
        assert_eq!(parse_size_bytes("abc"), None);
        assert_eq!(parse_size_bytes(""), None);
    }

    #[test]
    fn quota_state_alerts() {
        let (s, dir) = store();
        s.create("qv").unwrap();
        std::fs::write(s.data_dir("qv").join("f"), vec![0u8; 950]).unwrap();
        // quota 1000, alert at 90% → 950/1000 = 95% ⇒ in alert, not above.
        let v = s.set_quota("qv", Some(1000), Some(90), false).unwrap();
        let (warn, over) = s.quota_state(&v);
        assert!(warn && !over, "950/1000 deve estar em alerta mas não acima");
        // above the quota
        std::fs::write(s.data_dir("qv").join("g"), vec![0u8; 200]).unwrap();
        let (_, over2) = s.quota_state(&v);
        assert!(over2, "1150/1000 deve estar acima da quota");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn store() -> (VolumeStore, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "delonix-vol-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (VolumeStore::open(&base).unwrap(), base)
    }

    #[test]
    fn create_list_inspect_remove() {
        let (vs, base) = store();
        let v = vs.create("data").unwrap();
        assert!(v.mountpoint.ends_with("/data/_data"));
        assert_eq!(vs.list().unwrap().len(), 1);
        assert_eq!(vs.inspect("data").unwrap().name, "data");
        vs.remove("data").unwrap();
        assert!(vs.inspect("data").is_err());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn create_with_driver_idempotent_and_meta_on_disk() {
        let (vs, base) = store();
        // create with explicit `local` driver
        let v = vs.create_with("app_data", "local", None, None).unwrap();
        assert_eq!(v.driver, "local");
        // meta.json must exist on disk
        assert!(base.join("volumes/app_data/meta.json").exists());
        // idempotent: re-creating returns the existing one without error
        let v2 = vs.create_with("app_data", "local", None, None).unwrap();
        assert_eq!(v2.name, "app_data");
        assert_eq!(vs.list().unwrap().len(), 1);
        // invalid name → Error::Invalid
        assert!(matches!(
            vs.create_with("bad name!", "local", None, None),
            Err(Error::Invalid(_))
        ));
        // nfs without device → Error::Invalid
        assert!(matches!(
            vs.create_with("nas", "nfs", None, None),
            Err(Error::Invalid(_))
        ));
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn resolve_named_volume_creates_it() {
        let (vs, base) = store();
        let m = vs.resolve_spec("cache:/var/cache").unwrap();
        assert!(m.source.ends_with("/cache/_data"));
        assert_eq!(m.target, "/var/cache");
        assert!(!m.readonly);
        assert_eq!(vs.inspect("cache").unwrap().name, "cache");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn resolve_bind_readonly() {
        let (vs, base) = store();
        let host = base.join("hostdir");
        fs::create_dir_all(&host).unwrap();
        let spec = format!("{}:/mnt:ro", host.display());
        let m = vs.resolve_spec(&spec).unwrap();
        assert_eq!(m.target, "/mnt");
        assert!(m.readonly);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn rejects_relative_target_and_bad_spec() {
        let (vs, base) = store();
        assert!(vs.resolve_spec("data:relative").is_err());
        assert!(vs.resolve_spec("oneword").is_err());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn snapshot_names_reject_traversal() {
        assert!(safe_snapshot_name("pre-upgrade-1"));
        assert!(safe_snapshot_name("2026.07.06_0300"));
        for bad in ["", "../x", "a/b", ".oculto", "a b", &"x".repeat(129)] {
            assert!(!safe_snapshot_name(bad), "aceitou '{bad}'");
        }
    }

    #[test]
    fn snapshot_paths_and_listing() {
        let (vs, base) = store();
        vs.create("v1").unwrap();
        // validated path + non-existent ones list empty
        assert!(vs.snapshot_path("v1", "../evil").is_err());
        assert_eq!(vs.list_snapshots("v1").unwrap().len(), 0);
        // a "made" snapshot (file in place) appears in the listing
        let p = vs.snapshot_path("v1", "s1").unwrap();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"tar").unwrap();
        let ls = vs.list_snapshots("v1").unwrap();
        assert_eq!(ls.len(), 1);
        assert_eq!(ls[0].0, "s1");
        assert_eq!(ls[0].1, 3);
        // remove
        vs.remove_snapshot("v1", "s1").unwrap();
        assert!(vs.remove_snapshot("v1", "s1").is_err());
        fs::remove_dir_all(&base).ok();
    }
}
