//! `delonix system backup` / `delonix system restore` — save and put back the
//! state of a NODE.
//!
//! # What this exists to fix
//!
//! Everything this engine cannot rebuild lives under `$DELONIX_ROOT`, and until
//! now the only way to save it was `cp -a` of a directory whose contents nobody
//! had ever written down. That is not a backup procedure: half of what is in
//! there is live plumbing that must NOT come back (the holder's pidfiles, the
//! control socket), another half is cache that costs gigabytes and is
//! re-obtainable from a registry, and one file in there decrypts every secret on
//! the node.
//!
//! # The classifier is the contract, and it is pure
//!
//! [`classify`] decides, for one path relative to the root, whether it travels.
//! It is a pure function over the path and the [`Scope`], which is what makes the
//! interesting decisions testable as data — and it is used by BOTH sides: the
//! backup packs the covered set, and the restore makes the covered set be
//! exactly what the archive holds. Anything OUTSIDE the covered set is never
//! touched by either. That is the whole reason a restore can be described in one
//! sentence instead of a paragraph of exceptions.
//!
//! # What travels by default, and why not the rest
//!
//! Default = **what cannot be reconstructed**: the container/VM/network/volume
//! registries, IPAM allocations, secrets, `auth.json`, cluster PKI and
//! kubeconfigs, HTTPRoute config, the event log. All of it is small.
//!
//! Excluded unless asked for, each for its own reason:
//!
//! * **Volume DATA** (`--volumes`) — this is the only thing here that can be
//!   hundreds of GiB, and this host has already hit disk-pressure from less. It
//!   also cannot be read by a plain walk: in rootless a container writes `_data`
//!   as a subuid, and every managed database `chmod 700`s its data dir, so
//!   reading it as the real user yields EACCES and would silently pack an EMPTY
//!   volume. So `--volumes` does not walk the files at all — it goes through the
//!   SAME mapped `__volsnap` re-exec that `volumes snapshot` already uses, which
//!   owns the subuids and is already proven.
//! * **OCI images** (`--images`) — content-addressed cache, re-pullable. The
//!   `images/` index is tied to the same flag on purpose: an index without its
//!   blobs is a list of images that cannot be started.
//! * **VM disk images** (`--vm-images`) — gigabytes of qcow2 that `vm pull`
//!   re-fetches. Their `.json` metadata always travels, so a restored node can
//!   at least SAY which golden images it was built against.
//! * **`build-cache/` and `vm-images/_base/`** — pure caches, never travel.
//! * **Container rootfs dirs** (`containers/<id>/…`) — derived from an image; in
//!   rootless each one is a full flat copy, and this host measured 68 GiB of
//!   them. The registry entry (`containers/<id>.json`) travels; the tree does not.
//! * **`ingress/` and every pidfile/socket/lockfile** — this is the live
//!   plumbing of a RUNNING node. Restoring a stale `holder.pid` is precisely the
//!   trap this repo already paid for once: `status()` decides "up" by reading a
//!   pidfile, so a resurrected one makes the engine confidently report a holder
//!   that does not exist.
//!
//! # The master key
//!
//! The secrets are AEAD blobs under `<root>/tunnels/keyring.key`, and
//! `cred_vault`'s own doc-comment states the key is "outside backups/exports".
//! That invariant is kept: **the key does not travel by default**, so a leaked
//! archive is not a leaked vault. But the consequence has to be said out loud,
//! because it is the difference between a backup that works and one that only
//! looks like it: without the key, the encrypted secrets restore onto a
//! DIFFERENT node as bytes that will never decrypt. `--include-master-key` puts
//! it in — and then the archive IS the vault, which is why it says so, and why
//! the file is 0600 from the moment it exists.
//!
//! Either way the restore does not take this on trust: it decrypts every secret
//! it just put back and fails if any of them does not open. A restore that
//! reported success over unreadable secrets would be the exact "honest
//! reporting" failure the 208-subcommand audit catalogued.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use delonix_runtime_core::{Error, Result, Store};
use serde::{Deserialize, Serialize};

use super::po;

/// Archive format. The restore REFUSES anything else — see [`check_format`].
pub const FORMAT: u32 = 1;

/// The heavy areas, each opt-in. `master_key` is not about size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    #[serde(default)]
    pub volumes: bool,
    #[serde(default)]
    pub images: bool,
    #[serde(default)]
    pub vm_images: bool,
    #[serde(default)]
    pub master_key: bool,
}

/// Why a path stayed behind. Carried so the summary can say it instead of the
/// operator discovering the omission at restore time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Skip {
    /// Live plumbing of a running node (pidfiles, sockets, locks, `ingress/`).
    Ephemeral,
    /// Pure cache, re-derivable at no cost to correctness.
    Cache,
    /// Container rootfs — derived from an image, huge in rootless.
    Rootfs,
    /// Volume data (`--volumes`; travels as a mapped snapshot, not as files).
    VolumeData,
    /// OCI blobs/layers/index (`--images`).
    ImageData,
    /// VM disk images (`--vm-images`).
    VmImageData,
    /// The vault's master key (`--include-master-key`).
    MasterKey,
}

impl Skip {
    /// One line for the summary. Static EN text, translated at print time.
    pub fn reason(self) -> &'static str {
        match self {
            Skip::Ephemeral => "live plumbing of a running node (pids, sockets, locks)",
            Skip::Cache => "cache, rebuilt on demand",
            Skip::Rootfs => "container rootfs, rebuilt from the image",
            Skip::VolumeData => "volume data (pass --volumes)",
            Skip::ImageData => "OCI images (pass --images)",
            Skip::VmImageData => "VM disk images (pass --vm-images)",
            Skip::MasterKey => "the vault master key (pass --include-master-key)",
        }
    }
}

/// What the backup walk decided about one entry of the state root.
///
/// The `Skip` carries its reason and not a bare `false`, because the reason is
/// what the operator needs: «this backup has no VM disk images» is a different
/// fact from «this backup has no vault master key», and a boolean makes the two
/// indistinguishable in a listing. [`Skip::reason`] is what turns it into the
/// sentence shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Goes into the archive.
    Include,
    /// Left out, and why — see [`Skip`] for the flag that would include it.
    Skip(Skip),
}

/// Names that belong to a LIVE node and must never be resurrected.
///
/// `lock` with no extension is the IPAM/ingress flock file; the rest are the
/// pidfiles, unix sockets, consoles and per-VM logs the backends leave behind.
fn is_ephemeral_name(name: &str) -> bool {
    if name == "lock" {
        return true;
    }
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    matches!(
        ext,
        "pid" | "sock" | "lock" | "console" | "serial" | "log" | "tmp"
    )
}

/// Does this path travel, given the scope? PURE — path in, decision out.
///
/// Ordered from the most specific rule to the widest, the same discipline
/// `init::detect` follows: a rootfs file is also "a file under `containers/`",
/// and the wider rule must not win just for having been checked first.
///
/// `rel` is relative to `$DELONIX_ROOT` and uses `/`.
pub fn classify(rel: &str, scope: &Scope) -> Decision {
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let Some(&top) = parts.first() else {
        return Decision::Skip(Skip::Ephemeral);
    };
    let last = *parts.last().unwrap_or(&"");

    // The one file that turns an archive into a vault.
    if rel == "tunnels/keyring.key" {
        return if scope.master_key {
            Decision::Include
        } else {
            Decision::Skip(Skip::MasterKey)
        };
    }
    // The holder's whole runtime dir, plus any stray pid/socket/lock elsewhere.
    if top == "ingress" || is_ephemeral_name(last) {
        return Decision::Skip(Skip::Ephemeral);
    }
    // Caches. `vm-images/_base` is the downloaded cloud-image cache, not an image.
    if top == "build-cache" || rel.starts_with("vm-images/_base/") {
        return Decision::Skip(Skip::Cache);
    }
    // `containers/<id>.json` is the registry; anything DEEPER is inside the
    // container's own directory, i.e. its rootfs.
    if top == "containers" {
        return if parts.len() > 2 {
            Decision::Skip(Skip::Rootfs)
        } else {
            Decision::Include
        };
    }
    if matches!(top, "blobs" | "layers" | "images") {
        return if scope.images {
            Decision::Include
        } else {
            Decision::Skip(Skip::ImageData)
        };
    }
    if top == "vm-images" {
        // The metadata always travels: a restored node that cannot say which
        // golden image it ran is missing the one cheap half of the answer.
        return if last.ends_with(".json") || scope.vm_images {
            Decision::Include
        } else {
            Decision::Skip(Skip::VmImageData)
        };
    }
    if top == "volumes" {
        // Only the registry. `_data` and the snapshots go through `__volsnap`
        // when `--volumes` is on — walking them here would read EACCES on every
        // subuid-owned dir and pack an empty volume that looks like a full one.
        return if last == "meta.json" {
            Decision::Include
        } else {
            Decision::Skip(Skip::VolumeData)
        };
    }
    Decision::Include
}

/// What the archive says about itself. Written first, read first.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// Archive format — [`FORMAT`]. The gate, not decoration.
    pub format: u32,
    /// The `delonix` that wrote it (informational; the gate is `format`).
    pub delonix_version: String,
    pub created_unix: u64,
    pub hostname: String,
    /// The `$DELONIX_ROOT` it was taken from (printed, never enforced —
    /// restoring somewhere else is a legitimate use).
    pub root: String,
    pub scope: Scope,
    pub files: usize,
    pub bytes: u64,
    /// Volumes whose DATA travels, as `volumes/<name>.tar.gz` members.
    #[serde(default)]
    pub volume_data: Vec<String>,
    /// Included files per top-level area, for the summary.
    #[serde(default)]
    pub areas: BTreeMap<String, usize>,
}

/// Fail-closed on a format this build does not know.
///
/// An archive from a future version could carry members with meanings we would
/// guess wrong about — and guessing wrong here overwrites a node's state. The
/// error names both numbers so the answer ("use the version that wrote it") is
/// in the message rather than in someone's head.
pub fn check_format(m: &Manifest) -> Result<()> {
    if m.format == FORMAT {
        return Ok(());
    }
    Err(Error::Invalid(po::tf(
        "backup format {got} is not supported by this build (it handles {want}) — restore it with \
         the delonix version that wrote it ({ver})",
        &[
            ("got", &m.format.to_string()),
            ("want", &FORMAT.to_string()),
            ("ver", &m.delonix_version),
        ],
    )))
}

/// A member name from the archive, resolved under `base` — or refused.
///
/// SECURITY: a backup file is untrusted input like any other archive. Without
/// this, a member named `../../.ssh/authorized_keys` (or an absolute path, which
/// `Path::join` silently lets replace the base) writes wherever it likes, as the
/// user running the restore. Same lexical confinement `build::safe_join` applies
/// to `COPY`.
pub fn safe_rel(base: &Path, rel: &str) -> Result<PathBuf> {
    let mut out = base.to_path_buf();
    let mut any = false;
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(s) => {
                out.push(s);
                any = true;
            }
            Component::CurDir => {}
            _ => {
                return Err(Error::Invalid(po::tf(
                    "backup: refusing member '{rel}' — it escapes the target directory",
                    &[("rel", rel)],
                )))
            }
        }
    }
    if !any {
        return Err(Error::Invalid(po::tf(
            "backup: refusing member '{rel}' — empty path",
            &[("rel", rel)],
        )));
    }
    Ok(out)
}

/// A `volumes/<name>.tar.gz` member's name, as the archive spells it.
///
/// `safe_rel` already keeps the extraction inside the staging dir, but the name
/// ALSO becomes an argument to `VolumeStore::inspect` and, through it, a path
/// under `volumes/`. The store has its own whitelist and it is private, so this
/// is the same shape stated here rather than a second, looser rule.
pub fn valid_volume_member(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Everything the walk found, already decided.
#[derive(Debug, Default)]
pub struct Plan {
    /// Relative paths that travel, sorted.
    pub include: Vec<String>,
    /// How many files each skip reason accounts for.
    pub skipped: BTreeMap<Skip, usize>,
    /// Included files per top-level area.
    pub areas: BTreeMap<String, usize>,
    /// Total bytes of the included files.
    pub bytes: u64,
}

/// Walks the state root and applies [`classify`] to every regular file.
///
/// Symlinks are skipped rather than followed OR stored: the state root has none
/// in normal operation, and a stored symlink is a member whose target the
/// restore would have to reason about — the cheapest correct answer is not to
/// have them. Directories are not stored either; the restore creates the parents
/// it needs.
pub fn plan(root: &Path, scope: &Scope) -> Result<Plan> {
    let mut p = Plan::default();
    walk(root, root, scope, &mut p)?;
    p.include.sort();
    Ok(p)
}

fn walk(root: &Path, dir: &Path, scope: &Scope, p: &mut Plan) -> Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        // An unreadable directory is NOT an empty one — the trap this repo has
        // already catalogued. Refuse rather than quietly back up less.
        Err(e) => return Err(Error::Runtime {
            context: "system backup",
            message: po::tf(
                "cannot read {dir}: {err} — a directory that cannot be read is not an empty one",
                &[("dir", &dir.display().to_string()), ("err", &e.to_string())],
            ),
        }),
    };
    for e in rd {
        let e = e?;
        let path = e.path();
        let ft = e.file_type()?;
        let rel = path
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        // Our own staging/aside directories are not state.
        if rel.starts_with(".restore-") {
            continue;
        }
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            // Prune whole subtrees the classifier would reject anyway, so a
            // 68 GiB rootfs tree is not walked file by file to be discarded.
            if let Decision::Skip(s) = classify(&format!("{rel}/x"), scope) {
                if matches!(s, Skip::Rootfs | Skip::Cache | Skip::Ephemeral) {
                    *p.skipped.entry(s).or_default() += 1;
                    continue;
                }
            }
            walk(root, &path, scope, p)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        match classify(&rel, scope) {
            Decision::Include => {
                p.bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                let top = rel.split('/').next().unwrap_or("").to_string();
                *p.areas.entry(top).or_default() += 1;
                p.include.push(rel);
            }
            Decision::Skip(s) => *p.skipped.entry(s).or_default() += 1,
        }
    }
    Ok(())
}

/// UTC `YYYYMMDD-HHMMSS`, for the default archive name (no `chrono` — same
/// `gmtime_r` the volume snapshots already use).
pub fn timestamp(secs: u64) -> String {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `t` is valid and `tm` is our own buffer.
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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

fn io_err(context: &'static str) -> impl Fn(std::io::Error) -> Error {
    move |e: std::io::Error| Error::Runtime {
        context,
        message: e.to_string(),
    }
}

/// Workloads that a restore would pull the floor out from under.
///
/// A container's registry entry is about to be replaced while its process is
/// still running; a VM's disk metadata likewise. Both leave the node in a state
/// nothing on it can describe. Reported by name, because "there are 3 running"
/// is not something an operator can act on.
fn live_workloads(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(store) = Store::open(root.join("containers")) {
        for c in store.list().unwrap_or_default() {
            if c.pid.map(delonix_runtime::is_alive).unwrap_or(false) {
                out.push(format!("container {}", c.name));
            }
        }
    }
    for vm in delonix_vm::list(root).unwrap_or_default() {
        if matches!(vm.status, delonix_runtime_core::Status::Running) {
            out.push(format!("vm {}", vm.name));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// backup
// ---------------------------------------------------------------------------

/// `delonix system backup`.
pub fn cmd_backup(out: Option<String>, scope: Scope, root: &Path) -> Result<()> {
    if !root.is_dir() {
        return Err(Error::Invalid(po::tf(
            "no state root at {root} — nothing to back up",
            &[("root", &root.display().to_string())],
        )));
    }
    let p = plan(root, &scope)?;

    // Volume data goes through the mapped snapshot path, never through the walk.
    let staging = root.join(format!(".restore-staging-{}", std::process::id()));
    let mut vol_snaps: Vec<(String, PathBuf)> = Vec::new();
    if scope.volumes {
        std::fs::create_dir_all(&staging).map_err(io_err("system backup"))?;
        let store = delonix_volume::VolumeStore::open(root)?;
        for v in store.list()? {
            if delonix_volume::is_network_driver(&v.driver) {
                println!(
                    "{}",
                    po::tf(
                        "volume '{name}': data lives on the {driver} server, not on this node — \
                         only the registry entry travels",
                        &[("name", &v.name), ("driver", &v.driver)],
                    )
                );
                continue;
            }
            let tarball = staging.join(format!("{}.tar.gz", v.name));
            super::volume::volsnap_run("create", Path::new(&v.mountpoint), &tarball)?;
            vol_snaps.push((v.name.clone(), tarball));
        }
    }

    let manifest = Manifest {
        format: FORMAT,
        delonix_version: env!("CARGO_PKG_VERSION").to_string(),
        created_unix: now(),
        hostname: hostname(),
        root: root.display().to_string(),
        scope,
        files: p.include.len(),
        bytes: p.bytes,
        volume_data: vol_snaps.iter().map(|(n, _)| n.clone()).collect(),
        areas: p.areas.clone(),
    };

    let out = out
        .unwrap_or_else(|| format!("delonix-backup-{}.tar.gz", timestamp(manifest.created_unix)));
    let outp = PathBuf::from(&out);
    let write_result = write_archive(&outp, root, &p, &manifest, &vol_snaps);
    let _ = std::fs::remove_dir_all(&staging);
    write_result?;

    println!(
        "{}",
        po::tf(
            "{file} — {n} file(s), {size}",
            &[
                ("file", &out),
                ("n", &manifest.files.to_string()),
                (
                    "size",
                    &super::output::fmt_size(
                        std::fs::metadata(&outp).map(|m| m.len()).unwrap_or(0)
                    )
                ),
            ],
        )
    );
    for (area, n) in &manifest.areas {
        println!("  {area:<14} {n}");
    }
    for (s, n) in &p.skipped {
        println!(
            "  {}",
            po::tf(
                "left out: {n} × {why}",
                &[("n", &n.to_string()), ("why", po::t(s.reason()))],
            )
        );
    }
    if !vol_snaps.is_empty() {
        println!(
            "  {}",
            po::tf(
                "volume data: {list}",
                &[(
                    "list",
                    &vol_snaps
                        .iter()
                        .map(|(n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )],
            )
        );
    }
    if scope.master_key {
        println!(
            "\n{}",
            po::t(
                "WARNING: this archive CONTAINS the vault master key. Anyone holding the file can \
                 decrypt every secret of this node — treat it as the secrets themselves (it is \
                 0600; keep it that way, and encrypt it before it leaves the host)."
            )
        );
    } else if manifest.areas.contains_key("secrets") {
        println!(
            "\n{}",
            po::t(
                "The secrets travel ENCRYPTED and the master key does NOT — restoring them onto a \
                 DIFFERENT node produces secrets that will never decrypt there. Pass \
                 --include-master-key if this archive has to rebuild a node from scratch."
            )
        );
    }
    println!(
        "{}",
        po::t(
            "Taken with the node live, so it is crash-consistent, not quiesced — stop the \
             workloads first if the data needs a consistent point in time."
        )
    );
    Ok(())
}

fn write_archive(
    outp: &Path,
    root: &Path,
    p: &Plan,
    manifest: &Manifest,
    vol_snaps: &[(String, PathBuf)],
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    // 0600 AT CREATION, not chmod afterwards: the archive can carry `auth.json`,
    // the cluster PKI and (with the flag) the master key, and a window under the
    // ambient umask is a window in which another local user reads all three.
    let tmp = outp.with_extension("tar.gz.tmp");
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(io_err("system backup"))?;
    let enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
    let mut b = tar::Builder::new(enc);
    b.follow_symlinks(false);

    let mj = serde_json::to_vec_pretty(manifest)?;
    let mut h = tar::Header::new_gnu();
    h.set_size(mj.len() as u64);
    h.set_mode(0o600);
    h.set_mtime(manifest.created_unix);
    h.set_cksum();
    b.append_data(&mut h, "manifest.json", &mj[..])
        .map_err(io_err("system backup"))?;

    for rel in &p.include {
        let src = root.join(rel);
        // A file that vanished between the walk and the pack (a concurrent `rm`)
        // is not a reason to lose the whole backup, but it IS a reason to say so.
        match std::fs::File::open(&src) {
            Ok(mut f) => {
                b.append_file(format!("state/{rel}"), &mut f)
                    .map_err(io_err("system backup"))?;
            }
            Err(e) => eprintln!(
                "{}",
                po::tf(
                    "skipped {path}: {err}",
                    &[("path", rel), ("err", &e.to_string())],
                )
            ),
        }
    }
    for (name, tarball) in vol_snaps {
        let mut f = std::fs::File::open(tarball).map_err(io_err("system backup"))?;
        b.append_file(format!("volumes/{name}.tar.gz"), &mut f)
            .map_err(io_err("system backup"))?;
    }
    let mut enc = b.into_inner().map_err(io_err("system backup"))?;
    enc.flush().map_err(io_err("system backup"))?;
    let f = enc.finish().map_err(io_err("system backup"))?;
    f.sync_all().map_err(io_err("system backup"))?;
    drop(f);
    std::fs::rename(&tmp, outp).map_err(io_err("system backup"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// restore
// ---------------------------------------------------------------------------

/// What a first, read-only pass over the archive established.
#[derive(Debug)]
struct Survey {
    manifest: Manifest,
    /// `state/…` members, as paths relative to the root.
    state: Vec<String>,
    /// Volume names carried as `volumes/<name>.tar.gz`.
    volumes: Vec<String>,
}

/// Reads the archive END TO END without writing anything.
///
/// The point is not the listing — it is that a truncated gzip stream only errors
/// when the reader reaches the end, so checking the header (or just the names)
/// would pass an archive that cannot actually be extracted. `volsnap_restore`
/// learned this the expensive way: it used to clear the live data first and
/// discover the corruption afterwards, with nothing to put back.
fn survey(archive: &Path) -> Result<Survey> {
    let f = std::fs::File::open(archive).map_err(io_err("system restore"))?;
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(f));
    let mut manifest: Option<Manifest> = None;
    let (mut state, mut volumes, mut unknown) = (Vec::new(), Vec::new(), Vec::<String>::new());
    let mut sink = std::io::sink();
    for e in a.entries().map_err(io_err("system restore"))? {
        let mut e = e.map_err(io_err("system restore"))?;
        let name = e
            .path()
            .map_err(io_err("system restore"))?
            .to_string_lossy()
            .into_owned();
        let kind = e.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        // Only regular files. A symlink/hardlink member is a target this restore
        // would have to reason about, and a device node is never state of ours.
        if !kind.is_file() {
            return Err(Error::Invalid(po::tf(
                "backup: refusing member '{name}' — only regular files are restored",
                &[("name", &name)],
            )));
        }
        // `tar czf … .` writes every member with a `./` head; our own writer never
        // does, but an archive that was unpacked and repacked by hand is still the
        // operator's backup, and refusing it as "not a delonix backup" would be
        // wrong about the only thing that matters.
        let name = name.strip_prefix("./").unwrap_or(&name).to_string();
        if name == "manifest.json" {
            let mut buf = Vec::new();
            std::io::copy(&mut e, &mut buf).map_err(io_err("system restore"))?;
            manifest = Some(serde_json::from_slice(&buf)?);
            continue;
        }
        std::io::copy(&mut e, &mut sink).map_err(io_err("system restore"))?;
        if let Some(rel) = name.strip_prefix("state/") {
            state.push(rel.to_string());
        } else if let Some(v) = name
            .strip_prefix("volumes/")
            .and_then(|v| v.strip_suffix(".tar.gz"))
        {
            volumes.push(v.to_string());
        } else {
            // NOT an error yet. A future format will legitimately carry members
            // this build has never heard of, and the useful thing to tell the
            // operator then is the VERSION — not an accusation that their backup
            // is not a backup. Live-validated: a hand-repacked archive with
            // `format: 99` answered "unknown member ./volumes/…" and never
            // reached the version gate, which is the one message that would have
            // told them what to do.
            unknown.push(name);
        }
    }
    let manifest = manifest.ok_or_else(|| {
        Error::Invalid(po::t("backup: no manifest.json — not a delonix backup").to_string())
    })?;
    check_format(&manifest)?;
    if let Some(name) = unknown.first() {
        return Err(Error::Invalid(po::tf(
            "backup: unknown member '{name}' — not a delonix backup",
            &[("name", name)],
        )));
    }
    Ok(Survey {
        manifest,
        state,
        volumes,
    })
}

/// `delonix system restore`.
pub fn cmd_restore(archive: &str, force: bool, dry_run: bool, root: &Path) -> Result<()> {
    let ap = PathBuf::from(archive);
    // 1. VALIDATE BEFORE TOUCHING ANYTHING.
    let s = survey(&ap)?; // decodes end-to-end and gates the format
                          // Reject traversal here, before a single byte is written.
    for rel in &s.state {
        safe_rel(root, rel)?;
    }
    for v in &s.volumes {
        if !valid_volume_member(v) {
            return Err(Error::Invalid(po::tf(
                "backup: invalid volume name '{name}' in the archive",
                &[("name", v)],
            )));
        }
    }

    // 2. What is on disk that this restore claims authority over. Only the
    //    covered set — the classifier decides for both sides, so nothing outside
    //    it can be removed by accident.
    let current = plan(root, &s.manifest.scope)?;
    let incoming: std::collections::HashSet<&str> = s.state.iter().map(|x| x.as_str()).collect();
    let stale: Vec<String> = current
        .include
        .iter()
        .filter(|r| !incoming.contains(r.as_str()))
        .cloned()
        .collect();

    println!(
        "{}",
        po::tf(
            "backup of {host} taken {when} UTC, delonix {ver}",
            &[
                ("host", &s.manifest.hostname),
                ("when", &timestamp(s.manifest.created_unix)),
                ("ver", &s.manifest.delonix_version),
            ],
        )
    );
    println!(
        "{}",
        po::tf(
            "{n} file(s) to put back, {stale} registry entry(ies) to remove, {vol} volume(s) with \
             data",
            &[
                ("n", &s.state.len().to_string()),
                ("stale", &stale.len().to_string()),
                ("vol", &s.volumes.len().to_string()),
            ],
        )
    );
    for r in &stale {
        println!("  - {r}");
    }

    // 3. The destructive gate. A restore over a live node replaces the registry
    //    of a process that is still running — the engine would then describe a
    //    node that does not exist.
    let live = live_workloads(root);
    if !live.is_empty() {
        let list = live.join(", ");
        if !force {
            return Err(Error::Invalid(po::tf(
                "{n} workload(s) still running ({list}) — a restore replaces the registry \
                 underneath them. Stop them first, or pass --force if you accept losing track of \
                 them.",
                &[("n", &live.len().to_string()), ("list", &list)],
            )));
        }
        eprintln!(
            "{}",
            po::tf(
                "--force: restoring with {n} workload(s) still running ({list}) — the engine will \
                 no longer describe them correctly",
                &[("n", &live.len().to_string()), ("list", &list)],
            )
        );
    }

    if dry_run {
        println!("{}", po::t("--dry-run: nothing was changed."));
        return Ok(());
    }

    // 4. Extract to staging. A failure here leaves the node untouched.
    let staging = root.join(format!(".restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(io_err("system restore"))?;
    if let Err(e) = extract(&ap, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // 5. Swap. Everything replaced or removed is moved ASIDE first, so a failure
    //    half-way can put the node back the way it was — and the aside copy is
    //    deleted LAST, after the whole swap succeeded.
    let aside = root.join(format!(".restore-old-{}", std::process::id()));
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let swap = swap_in(root, &staging, &aside, &s.state, &stale, &mut moved);
    if let Err(e) = swap {
        for (orig, saved) in moved.iter().rev() {
            if let Some(d) = orig.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = std::fs::remove_file(orig);
            let _ = std::fs::rename(saved, orig);
        }
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir_all(&aside);
        return Err(e);
    }

    // 6. Volume data, now that the registry is in place. `volsnap_restore`
    //    verifies each archive before clearing the live data.
    let mut vol_err: Option<Error> = None;
    if !s.volumes.is_empty() {
        let store = delonix_volume::VolumeStore::open(root)?;
        for v in &s.volumes {
            let tarball = staging.join("volumes").join(format!("{v}.tar.gz"));
            match store.inspect(v) {
                Ok(vol) => {
                    if let Err(e) =
                        super::volume::volsnap_run("restore", Path::new(&vol.mountpoint), &tarball)
                    {
                        vol_err.get_or_insert(e);
                    } else {
                        println!("{}", po::tf("volume {name}: data restored", &[("name", v)]));
                    }
                }
                Err(e) => {
                    vol_err.get_or_insert(Error::Runtime {
                        context: "system restore",
                        message: po::tf(
                            "volume '{name}' has data in the backup but no registry entry: {err}",
                            &[("name", v), ("err", &e.to_string())],
                        ),
                    });
                }
            };
        }
    }

    let _ = std::fs::remove_dir_all(&staging);
    // The accounting goes LAST: while the aside copy exists, the node still has
    // both halves of every file this command touched.
    let _ = std::fs::remove_dir_all(&aside);

    println!(
        "{}",
        po::tf(
            "restored {n} file(s) into {root}",
            &[
                ("n", &s.state.len().to_string()),
                ("root", &root.display().to_string()),
            ],
        )
    );
    if let Some(e) = vol_err {
        return Err(e);
    }

    // 7. Prove the secrets actually came back. Bytes in the right place are not
    //    a restored secret: without the master key they are an AEAD blob nobody
    //    on this node can open, and reporting success over that is the failure
    //    mode this whole command is written against.
    verify_secrets(root, &s.manifest)?;

    if s.manifest.scope.images {
        // nothing further to say
    } else if s.manifest.areas.contains_key("containers") {
        println!(
            "{}",
            po::t(
                "The OCI images were not in this archive: on a fresh node the restored containers \
                 have no rootfs until the images are pulled again."
            )
        );
    }
    Ok(())
}

fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let f = std::fs::File::open(archive).map_err(io_err("system restore"))?;
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(f));
    a.set_preserve_permissions(true);
    a.set_overwrite(true);
    for e in a.entries().map_err(io_err("system restore"))? {
        let mut e = e.map_err(io_err("system restore"))?;
        let name = e
            .path()
            .map_err(io_err("system restore"))?
            .to_string_lossy()
            .into_owned();
        if !e.header().entry_type().is_file() {
            continue;
        }
        let out = safe_rel(dest, &name)?;
        if let Some(d) = out.parent() {
            std::fs::create_dir_all(d).map_err(io_err("system restore"))?;
        }
        e.unpack(&out).map_err(io_err("system restore"))?;
    }
    Ok(())
}

/// Moves the staged files into the root, recording every displacement so the
/// caller can undo them.
fn swap_in(
    root: &Path,
    staging: &Path,
    aside: &Path,
    state: &[String],
    stale: &[String],
    moved: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    let park = |rel: &str, moved: &mut Vec<(PathBuf, PathBuf)>| -> Result<()> {
        let live = safe_rel(root, rel)?;
        if !live.exists() {
            return Ok(());
        }
        let saved = safe_rel(aside, rel)?;
        if let Some(d) = saved.parent() {
            std::fs::create_dir_all(d).map_err(io_err("system restore"))?;
        }
        std::fs::rename(&live, &saved).map_err(io_err("system restore"))?;
        moved.push((live, saved));
        Ok(())
    };
    for rel in stale {
        park(rel, moved)?;
    }
    for rel in state {
        park(rel, moved)?;
        let src = safe_rel(&staging.join("state"), rel)?;
        let dst = safe_rel(root, rel)?;
        if let Some(d) = dst.parent() {
            std::fs::create_dir_all(d).map_err(io_err("system restore"))?;
        }
        std::fs::rename(&src, &dst).map_err(io_err("system restore"))?;
    }
    Ok(())
}

/// Opens every restored secret. A failure here names the cause, because the only
/// realistic one has a name: the archive travelled without the master key and
/// this node's key is a different one.
///
/// The names are read off the DIRECTORY and not from `SecretStore::list`, which
/// decodes each file and keeps only the ones that worked. Asking the list is
/// asking "which secrets decrypt?" of a set defined as "the ones that decrypt" —
/// on a node where none of them open it answers zero, cheerfully, and this
/// function would print `0 secret(s) restored and decrypted` over a vault the
/// operator has just lost.
fn verify_secrets(root: &Path, m: &Manifest) -> Result<()> {
    if !m.areas.contains_key("secrets") {
        return Ok(());
    }
    let store = delonix_runtime_core::SecretStore::open(root)?;
    let mut names = Vec::new();
    for e in std::fs::read_dir(root.join("secrets")).map_err(io_err("system restore"))? {
        let p = e.map_err(io_err("system restore"))?.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        if let Some(n) = p.file_stem().and_then(|x| x.to_str()) {
            names.push(n.to_string());
        }
    }
    names.sort();
    let mut bad = Vec::new();
    for n in &names {
        if store.load(n).is_err() {
            bad.push(n.clone());
        }
    }
    if bad.is_empty() {
        println!(
            "{}",
            po::tf(
                "{n} secret(s) restored and decrypted",
                &[("n", &names.len().to_string())],
            )
        );
        return Ok(());
    }
    Err(Error::Runtime {
        context: "system restore",
        message: po::tf(
            "{n} secret(s) did not decrypt after the restore ({list}). The archive carries the \
             secrets encrypted; the key that opens them is this node's master key, and it only \
             travels with --include-master-key. Everything else was restored.",
            &[("n", &bad.len().to_string()), ("list", &bad.join(", "))],
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Scope {
        Scope {
            volumes: true,
            images: true,
            vm_images: true,
            master_key: true,
        }
    }

    #[test]
    fn o_default_leva_o_que_nao_se_reconstroi() {
        let s = Scope::default();
        for rel in [
            "containers/abc123.json",
            "networks/kaeso-net",
            "ipam/10.210.json",
            "secrets/db.json",
            "auth.json",
            "vms/dev.json",
            "sharevolumes/x.json",
            "httproute/manual.json",
            "events.jsonl",
            "clusters/kaeso/id_ed25519",
            "tunnels/cred/ngrok.bin",
            "volumes/pgdata/meta.json",
            "vm-images/delonix-vm-k8s_1.34.json",
        ] {
            assert_eq!(classify(rel, &s), Decision::Include, "{rel}");
        }
    }

    #[test]
    fn a_chave_mestra_so_viaja_quando_pedida() {
        // The whole point: a leaked archive must not be a leaked vault.
        assert_eq!(
            classify("tunnels/keyring.key", &Scope::default()),
            Decision::Skip(Skip::MasterKey)
        );
        assert_eq!(classify("tunnels/keyring.key", &all()), Decision::Include);
    }

    #[test]
    fn o_encanamento_de_um_no_vivo_nunca_volta() {
        // Restoring a stale `holder.pid` makes `status()` — which decides "up"
        // by reading a pidfile — report a holder that does not exist.
        let s = all();
        for rel in [
            "ingress/holder.pid",
            "ingress/control.pid",
            "ingress/lock",
            "ingress/networks",
            "ipam/lock",
            "vms/dev.sock",
            "vms/dev.pid",
            "vms/dev.console",
            "vms/dev.log",
            "httproute/auto.lock",
            "httproute/proxy.log",
        ] {
            assert_eq!(classify(rel, &s), Decision::Skip(Skip::Ephemeral), "{rel}");
        }
    }

    #[test]
    fn o_rootfs_de_um_container_nao_viaja_mas_o_registo_sim() {
        let s = all();
        assert_eq!(classify("containers/abc.json", &s), Decision::Include);
        assert_eq!(
            classify("containers/abc/rootfs/etc/passwd", &s),
            Decision::Skip(Skip::Rootfs)
        );
        assert_eq!(
            classify("containers/abc/hostname", &s),
            Decision::Skip(Skip::Rootfs)
        );
    }

    #[test]
    fn os_dados_pesados_sao_opt_in() {
        let d = Scope::default();
        assert_eq!(
            classify("blobs/sha256/aa", &d),
            Decision::Skip(Skip::ImageData)
        );
        assert_eq!(
            classify("images/nginx.json", &d),
            Decision::Skip(Skip::ImageData)
        );
        assert_eq!(
            classify("layers/aa/layer.tar", &d),
            Decision::Skip(Skip::ImageData)
        );
        assert_eq!(
            classify("vm-images/opnsense_26.1.qcow2", &d),
            Decision::Skip(Skip::VmImageData)
        );
        // The metadata is cheap and answers "which golden image was this?".
        assert_eq!(
            classify("vm-images/opnsense_26.1.json", &d),
            Decision::Include
        );
        // The download cache never travels, not even with --vm-images.
        assert_eq!(
            classify("vm-images/_base/noble.img", &all()),
            Decision::Skip(Skip::Cache)
        );
        assert_eq!(
            classify("build-cache/aa/layer.tar", &all()),
            Decision::Skip(Skip::Cache)
        );
    }

    #[test]
    fn os_dados_de_um_volume_nunca_vao_pelo_walk() {
        // Even with --volumes: reading `_data` as the real user hits EACCES on
        // every subuid-owned dir and would pack an EMPTY volume that looks full.
        // The data travels as a mapped `__volsnap` snapshot instead.
        for s in [Scope::default(), all()] {
            assert_eq!(
                classify("volumes/pgdata/_data/base/1", &s),
                Decision::Skip(Skip::VolumeData)
            );
            assert_eq!(
                classify("volumes/pgdata/snapshots/x.tar.gz", &s),
                Decision::Skip(Skip::VolumeData)
            );
            assert_eq!(classify("volumes/pgdata/meta.json", &s), Decision::Include);
            // Namespaced ShareVolume layout goes the same way.
            assert_eq!(
                classify("volumes/.ns/teamA/pg/meta.json", &s),
                Decision::Include
            );
        }
    }

    #[test]
    fn um_formato_desconhecido_e_recusado() {
        let mut m = Manifest {
            format: FORMAT,
            delonix_version: "9.9.9".into(),
            created_unix: 0,
            hostname: "h".into(),
            root: "/r".into(),
            scope: Scope::default(),
            files: 0,
            bytes: 0,
            volume_data: vec![],
            areas: BTreeMap::new(),
        };
        assert!(check_format(&m).is_ok());
        m.format = FORMAT + 1;
        let e = check_format(&m).unwrap_err().to_string();
        assert!(
            e.contains("9.9.9"),
            "a mensagem tem de nomear a versão: {e}"
        );
    }

    #[test]
    fn um_membro_do_arquivo_nao_escapa_do_destino() {
        // A backup file is untrusted input: without this, a member named
        // `../../.ssh/authorized_keys` writes wherever it likes.
        let base = Path::new("/tmp/dest");
        assert_eq!(
            safe_rel(base, "state/containers/a.json").unwrap(),
            base.join("state/containers/a.json")
        );
        assert!(safe_rel(base, "../../.ssh/authorized_keys").is_err());
        assert!(safe_rel(base, "a/../../b").is_err());
        assert!(safe_rel(base, "/etc/passwd").is_err());
        assert!(safe_rel(base, "").is_err());
    }

    #[test]
    fn o_plano_poda_a_arvore_do_rootfs_em_vez_de_a_percorrer() {
        let d = std::env::temp_dir().join(format!("delonix-bk-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("containers/abc/rootfs/etc")).unwrap();
        std::fs::create_dir_all(d.join("ingress")).unwrap();
        std::fs::create_dir_all(d.join("networks")).unwrap();
        std::fs::write(d.join("containers/abc.json"), b"{}").unwrap();
        std::fs::write(d.join("containers/abc/rootfs/etc/passwd"), b"x").unwrap();
        std::fs::write(d.join("ingress/holder.pid"), b"1").unwrap();
        std::fs::write(d.join("networks/dev"), b"210").unwrap();

        let p = plan(&d, &Scope::default()).unwrap();
        assert_eq!(p.include, vec!["containers/abc.json", "networks/dev"]);
        assert_eq!(p.areas.get("containers"), Some(&1));
        assert!(p.skipped.contains_key(&Skip::Rootfs));
        assert!(p.skipped.contains_key(&Skip::Ephemeral));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Writes a tar.gz with the given `(name, contents)` members.
    fn tarball(path: &Path, members: &[(&str, &str)]) {
        let f = std::fs::File::create(path).unwrap();
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            f,
            flate2::Compression::default(),
        ));
        for (name, body) in members {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o600);
            h.set_cksum();
            b.append_data(&mut h, name, body.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
    }

    fn manifest_json(format: u32) -> String {
        format!(
            r#"{{"format":{format},"delonix_version":"9.9.9","created_unix":0,"hostname":"h","root":"/r","scope":{{}},"files":0,"bytes":0}}"#
        )
    }

    #[test]
    fn um_formato_futuro_nomeia_a_versao_e_nao_o_membro_desconhecido() {
        // Live-validated finding: a future format legitimately carries members
        // this build never heard of, so refusing on the FIRST unknown member
        // answered "not a delonix backup" — an accusation — instead of the one
        // sentence that tells the operator what to do. Order matters here.
        let d = std::env::temp_dir().join(format!("delonix-bk-fmt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let a = d.join("a.tar.gz");
        tarball(
            &a,
            &[
                ("manifest.json", &manifest_json(FORMAT + 98)),
                ("something/from/the/future", "x"),
            ],
        );
        let e = survey(&a).unwrap_err().to_string();
        assert!(
            e.contains("9.9.9") && !e.contains("unknown member"),
            "a recusa tinha de nomear a versão que escreveu o arquivo: {e}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn um_arquivo_reempacotado_com_ponto_barra_continua_a_ser_lido() {
        // `tar czf … .` heads every member with `./`. Our writer never does, but
        // an unpacked-and-repacked archive is still the operator's backup.
        let d = std::env::temp_dir().join(format!("delonix-bk-dot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let a = d.join("a.tar.gz");
        tarball(
            &a,
            &[
                ("./manifest.json", &manifest_json(FORMAT)),
                ("./state/networks/dev", "base=210\n"),
                ("./volumes/pgdata.tar.gz", "x"),
            ],
        );
        let s = survey(&a).unwrap();
        assert_eq!(s.state, vec!["networks/dev"]);
        assert_eq!(s.volumes, vec!["pgdata"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn um_membro_que_nao_e_ficheiro_normal_e_recusado() {
        // A symlink member is a target the restore would have to reason about;
        // the cheapest correct answer is not to have them.
        let d = std::env::temp_dir().join(format!("delonix-bk-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let a = d.join("a.tar.gz");
        {
            let f = std::fs::File::create(&a).unwrap();
            let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
                f,
                flate2::Compression::default(),
            ));
            let m = manifest_json(FORMAT);
            let mut h = tar::Header::new_gnu();
            h.set_size(m.len() as u64);
            h.set_mode(0o600);
            h.set_cksum();
            b.append_data(&mut h, "manifest.json", m.as_bytes())
                .unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_mode(0o777);
            h.set_link_name("/etc/shadow").unwrap();
            h.set_cksum();
            b.append_data(&mut h, "state/evil", &b""[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let e = survey(&a).unwrap_err().to_string();
        assert!(e.contains("regular files"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn timestamp_e_utc_legivel() {
        assert_eq!(timestamp(0), "19700101-000000");
        assert_eq!(timestamp(1_754_000_000), "20250731-221320");
    }
}
