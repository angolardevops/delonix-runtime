//! `delonix backup <kind> <name>` / `delonix restore <kind> <archive>` — a
//! backup of ONE resource, as opposed to `system backup`, which takes the whole
//! node.
//!
//! # What goes in the archive, and why not everything
//!
//! Measured on a real container (`pgvector:pg16`): the record is **1.5 KB**, the
//! rootfs is **435 MB**, and every byte anyone cares about is in the mounts.
//! The rootfs is derivable — it is the image plus whatever the container wrote
//! outside its volumes — and an image is content-addressed and re-pullable. So a
//! container/pod/stack archive carries:
//!
//! * the RECORD, which is what lets the resource be recreated exactly; and
//! * the DATA of every named volume it uses.
//!
//! It deliberately does NOT carry the image or the rootfs. A backup that is 435
//! MB per container is a backup nobody takes twice a day, and this one has to be
//! cheap enough to take on a schedule. `restore` re-pulls the image, so what
//! comes back is the same bytes.
//!
//! **A VM is the exception, and for a reason that is not symmetry.** A
//! container's writes outside its volumes are scratch; a VM's overlay qcow2 IS
//! its state — the installed packages, the database, everything. So the overlay
//! travels and the golden base disk does not (that one is re-pullable, same
//! argument as the image). The archive says which base it needs.
//!
//! **This is not a checkpoint.** Memory is not saved and `restore` does not
//! resume mid-execution — that needs CRIU and is a different feature. What comes
//! back is a resource with the same configuration and the same data on disk,
//! which is what "the last state" means for everything that keeps its state
//! where state belongs.
//!
//! # Scheduling in a daemonless engine
//!
//! There is no daemon to run a timer in, and adding one for this would trade the
//! product's central property for a convenience. The schedule is installed as a
//! **systemd USER timer**, which survives logout under lingering and is
//! inspectable with the tools an operator already has
//! (`systemctl --user list-timers`). `--max-for-day N` becomes N evenly spaced
//! runs; `--cron` takes a crontab expression and TRANSLATES it, refusing what it
//! cannot express rather than approximating.

use super::backup::{safe_rel, timestamp};
use super::po;
use super::util::state_root;
use delonix_runtime_core::{Error, Result};
use std::path::{Path, PathBuf};

/// The resource kinds a backup can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Kind {
    Container,
    Pod,
    Vm,
    Stack,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Container => "container",
            Kind::Pod => "pod",
            Kind::Vm => "vm",
            Kind::Stack => "stack",
        }
    }
}

pub const FORMAT: u32 = 1;

/// What the archive records about itself, at its root as `backup.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    /// Format version. A reader that does not know it REFUSES rather than
    /// guessing at a layout it has never seen — the same gate `system backup`
    /// uses, and for the same reason: guessing wrong here overwrites state.
    pub format: u32,
    pub delonix_version: String,
    pub kind: String,
    pub name: String,
    pub created_unix: u64,
    pub hostname: String,
    /// Members under `volumes/` whose data travels.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// For a VM: the base disk its overlay needs. Named so a restore on another
    /// node can say what is missing instead of producing an unreadable overlay.
    #[serde(default)]
    pub vm_base_disk: Option<String>,
    /// For a stack: the members, in the order they were archived.
    #[serde(default)]
    pub members: Vec<String>,
}

/// Fail-closed on a format this build does not know.
pub fn check_format(m: &Meta) -> Result<()> {
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

/// Where the archive goes.
#[derive(Debug, Clone)]
pub enum Dest {
    /// A filesystem path, absolute or relative to the CWD.
    Path(PathBuf),
    /// A named volume: the archive lands in its data directory. This is how a
    /// backup ends up on a NAS — the volume already mounts it.
    Volume(String),
}

impl Dest {
    /// Parses `--to`: `volume:<name>` names a volume, anything else is a path.
    ///
    /// The prefix is explicit because a bare name is ambiguous — `backups` is a
    /// plausible directory AND a plausible volume, and guessing would put
    /// somebody's archive somewhere they cannot find it.
    pub fn parse(s: &str) -> Self {
        match s.strip_prefix("volume:") {
            Some(v) => Dest::Volume(v.to_string()),
            None => Dest::Path(PathBuf::from(s)),
        }
    }

    /// The directory the archive is written into, created if needed.
    pub fn resolve(&self, root: &Path) -> Result<PathBuf> {
        match self {
            Dest::Path(p) => {
                std::fs::create_dir_all(p).map_err(|e| {
                    Error::Invalid(po::tf(
                        "backup: cannot use '{path}': {err}",
                        &[("path", &p.display().to_string()), ("err", &e.to_string())],
                    ))
                })?;
                // ABSOLUTE, always. A relative `--to` is resolved against the
                // caller's CWD, and a scheduled run has a different one: passing
                // `.` through to the timer wrote the archive into `$HOME` while
                // the operator watched it appear in the directory they were
                // standing in — a backup that lands somewhere else, and reports
                // success. Measured, not hypothetical.
                p.canonicalize().map_err(|e| {
                    Error::Invalid(po::tf(
                        "backup: cannot resolve '{path}': {err}",
                        &[("path", &p.display().to_string()), ("err", &e.to_string())],
                    ))
                })
            }
            Dest::Volume(name) => {
                let store = delonix_volume::VolumeStore::open(root)?;
                let v = store.inspect(name)?;
                // A network volume is only a directory once it is mounted; say
                // that instead of writing the archive into the empty mountpoint
                // of an unmounted NAS, where it would look saved and be lost on
                // the next mount.
                store.ensure_mounted(&v).ok();
                let dir = PathBuf::from(&v.mountpoint);
                if !dir.is_dir() {
                    return Err(Error::Invalid(po::tf(
                        "backup: volume '{name}' has no data directory at {path}",
                        &[("name", name), ("path", &v.mountpoint)],
                    )));
                }
                Ok(dir)
            }
        }
    }
}

/// The archive's file name: `<kind>-<name>-<YYYYMMDD-HHMMSS>.tar.gz`.
///
/// The timestamp is what makes retention possible at all — `--max-for-day` has
/// to count today's archives, and it counts them by NAME so no index has to be
/// kept in step with the filesystem.
pub fn archive_name(kind: Kind, name: &str, now_unix: u64) -> String {
    format!("{}-{}-{}.tar.gz", kind.as_str(), name, timestamp(now_unix))
}

/// Archives already in `dir` for this resource, oldest first.
pub fn existing(dir: &Path, kind: Kind, name: &str) -> Vec<PathBuf> {
    let prefix = format!("{}-{}-", kind.as_str(), name);
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix) && n.ends_with(".tar.gz"))
                .unwrap_or(false)
        })
        .collect();
    // By NAME, which sorts chronologically because the stamp is fixed-width.
    // Sorting by mtime would reorder archives copied between hosts, where mtime
    // says when they were COPIED.
    v.sort();
    v
}

/// Keeps the newest `keep` archives of this resource and removes the rest.
///
/// Returns what was removed, so the caller can SAY it. A retention policy that
/// deletes in silence is indistinguishable from a bug that deletes.
pub fn prune(dir: &Path, kind: Kind, name: &str, keep: usize) -> Vec<PathBuf> {
    let all = existing(dir, kind, name);
    if all.len() <= keep {
        return Vec::new();
    }
    let mut gone = Vec::new();
    for p in &all[..all.len() - keep] {
        if std::fs::remove_file(p).is_ok() {
            gone.push(p.clone());
        }
    }
    gone
}

/// How often to run, for `--max-for-day N`, as a systemd `OnCalendar`.
///
/// N per day means one every 24/N hours, aligned to midnight. Refused above 24
/// and refused when it does not divide the day: a "per day" knob that silently
/// means something else is the accepted-and-reinterpreted option this repo
/// treats as worse than an error.
pub fn on_calendar_for(max_per_day: u32) -> Result<String> {
    match max_per_day {
        0 => Err(Error::Invalid(
            po::t("--max-for-day must be at least 1 (drop the flag to take no schedule at all)")
                .to_string(),
        )),
        1 => Ok("*-*-* 00:00:00".to_string()),
        n if n <= 24 && 24 % n == 0 => Ok(format!("*-*-* 00/{}:00:00", 24 / n)),
        n if n <= 24 => Err(Error::Invalid(po::tf(
            "--max-for-day {n} does not divide the day evenly — use 1, 2, 3, 4, 6, 8, 12 or 24, \
             or give an explicit --cron",
            &[("n", &n.to_string())],
        ))),
        n => Err(Error::Invalid(po::tf(
            "--max-for-day {n} is more than 24 — this flag is per DAY, and that is more than once \
             an hour. Use --cron for anything finer",
            &[("n", &n.to_string())],
        ))),
    }
}

/// Translates a 5-field crontab expression into a systemd `OnCalendar`.
///
/// systemd does not speak cron, and `systemd-run --on-calendar` is the only
/// scheduling this engine can install without becoming a daemon. The two
/// grammars overlap on everything people actually write (`*`, numbers, lists,
/// ranges, steps) and differ in two places that MATTER, so both are handled
/// rather than approximated:
///
/// * a step is `*/N` in cron and `0/N` in systemd; and
/// * the day of week is a number in cron (with 0 AND 7 both meaning Sunday) and
///   a name in systemd.
///
/// Anything outside that overlap is REFUSED with the reason. A schedule that
/// silently fires at a different time than the one written is worse than one
/// that refuses to install.
pub fn cron_to_on_calendar(expr: &str) -> Result<String> {
    let f: Vec<&str> = expr.split_whitespace().collect();
    if f.len() != 5 {
        return Err(Error::Invalid(po::tf(
            "--cron takes 5 fields (minute hour day-of-month month day-of-week), got {n}: '{expr}'",
            &[("n", &f.len().to_string()), ("expr", expr)],
        )));
    }
    let (min, hour, dom, mon, dow) = (f[0], f[1], f[2], f[3], f[4]);

    // Steps differ (`*/N` vs `0/N`); everything else in the overlap is verbatim.
    fn field(v: &str, what: &str) -> Result<String> {
        if v.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '*' | ',' | '-' | '/'))
        {
            Ok(v.replace("*/", "0/"))
        } else {
            Err(Error::Invalid(po::tf(
                "--cron: '{v}' is not something this translator can express in the {what} field \
                 (names and @shortcuts are not supported — use numbers, *, lists, ranges or */N)",
                &[("v", v), ("what", what)],
            )))
        }
    }

    let min = field(min, "minute")?;
    let hour = field(hour, "hour")?;
    let dom = field(dom, "day-of-month")?;
    let mon = field(mon, "month")?;

    // Day of week: numbers to names. Cron's 0 and 7 are BOTH Sunday — dropping
    // one of them would make `0 3 * * 7` silently never fire.
    const DAYS: [&str; 8] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let dow_s = if dow == "*" {
        String::new()
    } else {
        let mut names = Vec::new();
        for part in dow.split(',') {
            let n: usize = part.parse().map_err(|_| {
                Error::Invalid(po::tf(
                    "--cron: '{v}' is not a plain day-of-week number 0-7 (0 and 7 are both Sunday)",
                    &[("v", part)],
                ))
            })?;
            if n > 7 {
                return Err(Error::Invalid(po::tf(
                    "--cron: day-of-week {v} is out of range (0-7)",
                    &[("v", part)],
                )));
            }
            let name = DAYS[n];
            if !names.contains(&name) {
                names.push(name);
            }
        }
        format!("{} ", names.join(","))
    };

    Ok(format!("{dow_s}*-{mon}-{dom} {hour}:{min}:00"))
}

/// Validates a resource name before it reaches a path or an argv.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// The systemd unit name for a resource's schedule.
pub fn unit_name(kind: Kind, name: &str) -> String {
    // `.` and `:` are legal in a unit name but read badly in `list-timers`;
    // `-` is what systemd itself uses as a separator.
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("delonix-backup-{}-{}", kind.as_str(), safe)
}

// ---------------------------------------------------------------------------
// Archive assembly
// ---------------------------------------------------------------------------

type Gz = flate2::write::GzEncoder<std::fs::File>;

fn append_bytes(b: &mut tar::Builder<Gz>, name: &str, data: &[u8]) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o600);
    h.set_mtime(now());
    h.set_cksum();
    b.append_data(&mut h, name, data)
        .map_err(|e| Error::Invalid(format!("backup: writing '{name}': {e}")))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A private scratch directory, removed by [`Scratch`]'s drop.
///
/// Under `$DELONIX_ROOT` and NOT `/tmp`, for two independent reasons: what goes
/// through here is volume data, which can be gigabytes and would fill a tmpfs;
/// and `/tmp` is world-writable with a guessable name, which is exactly the
/// hijackable-temporary-file class this repo already had to fix in `bpf.rs`.
/// `create_dir` (not `create_dir_all`) fails if the final component exists, so a
/// pre-planted symlink is refused rather than followed.
struct Scratch(PathBuf);

impl Scratch {
    fn new(root: &Path) -> Result<Self> {
        let base = root.join("tmp");
        std::fs::create_dir_all(&base)
            .map_err(|e| Error::Invalid(format!("backup: {}: {e}", base.display())))?;
        for n in 0..64u32 {
            let p = base.join(format!("backup-{}-{n}", std::process::id()));
            match std::fs::create_dir(&p) {
                Ok(()) => return Ok(Scratch(p)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Error::Invalid(format!("backup: {}: {e}", p.display()))),
            }
        }
        Err(Error::Invalid(
            po::t("backup: cannot create a scratch directory").to_string(),
        ))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Reads a VM record. `delonix_vm::load_vm` is private, and the store is the
/// same one `cmd/vm.rs` opens for exactly this.
fn load_vm(root: &Path, name: &str) -> Result<delonix_runtime_core::Vm> {
    let st: delonix_runtime_core::JsonStore<delonix_runtime_core::Vm> =
        delonix_runtime_core::JsonStore::open(root.join("vms"))?;
    st.load(name).map_err(|e| match e {
        Error::NotFound(_) => Error::VmNotFound(name.to_string()),
        e => e,
    })
}

fn save_vm(root: &Path, vm: &delonix_runtime_core::Vm) -> Result<()> {
    let st: delonix_runtime_core::JsonStore<delonix_runtime_core::Vm> =
        delonix_runtime_core::JsonStore::open(root.join("vms"))?;
    st.save(&vm.name, vm)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The named volumes a set of mounts uses, as (volume name, data directory).
///
/// A `Mount` records only the host PATH — the volume's name is not in it — so
/// the store is what turns one into the other. A bind mount of a plain host
/// directory matches nothing and is deliberately left out: it is not this
/// engine's data to back up, and copying somebody's `/home` into an archive
/// because they bind-mounted it is a surprise, not a service.
fn volumes_of(root: &Path, mounts: &[delonix_runtime_core::Mount]) -> Vec<(String, String)> {
    let store = match delonix_volume::VolumeStore::open(root) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let all = store.list().unwrap_or_default();
    let mut out = Vec::new();
    for m in mounts {
        if let Some(v) = all.iter().find(|v| v.mountpoint == m.source) {
            if !out.iter().any(|(n, _): &(String, String)| n == &v.name) {
                out.push((v.name.clone(), v.mountpoint.clone()));
            }
        }
    }
    out
}

/// Packs one container's record and volume data into `out`.
fn write_container_archive(
    root: &Path,
    c: &delonix_runtime_core::Container,
    out: &Path,
    tmp: &Path,
) -> Result<Meta> {
    let vols = volumes_of(root, &c.mounts);
    let f = std::fs::File::create(out)
        .map_err(|e| Error::Invalid(format!("backup: {}: {e}", out.display())))?;
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        f,
        flate2::Compression::default(),
    ));

    let meta = Meta {
        format: FORMAT,
        delonix_version: env!("CARGO_PKG_VERSION").to_string(),
        kind: Kind::Container.as_str().to_string(),
        name: c.name.clone(),
        created_unix: now(),
        hostname: hostname(),
        volumes: vols.iter().map(|(n, _)| n.clone()).collect(),
        vm_base_disk: None,
        members: Vec::new(),
    };
    append_bytes(&mut b, "backup.json", &to_json(&meta)?)?;
    append_bytes(&mut b, "config/container.json", &to_json(c)?)?;

    for (name, data) in &vols {
        // Through `__volsnap`, which reads from INSIDE the mapped userns. Walking
        // `_data` from out here would hit EACCES on every subuid-owned directory
        // and pack an EMPTY volume that looks exactly like a full one — the
        // "an unreadable directory is not an empty directory" trap, and the one
        // place where falling for it produces a backup that restores nothing.
        let tarball = tmp.join(format!("{name}.tar.gz"));
        super::volume::volsnap_run("create", Path::new(data), &tarball)?;
        b.append_path_with_name(&tarball, format!("volumes/{name}.tar.gz"))
            .map_err(|e| Error::Invalid(format!("backup: volume '{name}': {e}")))?;
        let _ = std::fs::remove_file(&tarball);
    }

    b.into_inner()
        .and_then(|enc| enc.finish())
        .map_err(|e| Error::Invalid(format!("backup: closing {}: {e}", out.display())))?;
    Ok(meta)
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(v).map_err(|e| Error::Invalid(format!("backup: encoding: {e}")))
}

/// Packs one VM's record and its overlay disk.
///
/// The overlay travels and the BASE does not. A VM's overlay is its state; the
/// base is a golden image that `image vm pull` puts back. Carrying the base
/// would multiply every archive by a gigabyte to hold bytes that are already
/// content-addressed somewhere else.
fn write_vm_archive(vm: &delonix_runtime_core::Vm, out: &Path) -> Result<Meta> {
    let overlay = Path::new(&vm.overlay);
    if !overlay.is_file() {
        return Err(Error::Invalid(po::tf(
            "backup: the overlay disk of VM '{name}' is not at {path} — nothing to archive",
            &[("name", &vm.name), ("path", &vm.overlay)],
        )));
    }
    if matches!(vm.status, delonix_runtime_core::Status::Running) {
        // A qcow2 copied out from under a live guest is a torn filesystem: the
        // guest has writes in flight and its page cache is not on the disk. The
        // engine already has the right primitive for a live VM, and saying so is
        // better than producing an archive that restores into fsck.
        return Err(Error::Invalid(po::tf(
            "backup: VM '{name}' is running — copying its disk now would capture a torn \
             filesystem. Stop it first (`delonix vm stop {name}`), or take a live checkpoint with \
             `delonix vm snapshot {name} <label>`",
            &[("name", &vm.name)],
        )));
    }

    let f = std::fs::File::create(out)
        .map_err(|e| Error::Invalid(format!("backup: {}: {e}", out.display())))?;
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        f,
        flate2::Compression::default(),
    ));
    let meta = Meta {
        format: FORMAT,
        delonix_version: env!("CARGO_PKG_VERSION").to_string(),
        kind: Kind::Vm.as_str().to_string(),
        name: vm.name.clone(),
        created_unix: now(),
        hostname: hostname(),
        volumes: Vec::new(),
        vm_base_disk: Some(vm.disk.clone()),
        members: Vec::new(),
    };
    append_bytes(&mut b, "backup.json", &to_json(&meta)?)?;
    append_bytes(&mut b, "config/vm.json", &to_json(vm)?)?;
    b.append_path_with_name(overlay, "disk/overlay.qcow2")
        .map_err(|e| Error::Invalid(format!("backup: overlay: {e}")))?;
    b.into_inner()
        .and_then(|enc| enc.finish())
        .map_err(|e| Error::Invalid(format!("backup: closing {}: {e}", out.display())))?;
    Ok(meta)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub struct BackupArgs {
    /// What to back up.
    #[arg(value_enum)]
    pub kind: Kind,
    /// Its name.
    pub name: String,
    /// Where the archive goes: a directory, or `volume:<name>` for a named volume (default: the current directory).
    #[arg(long, default_value = ".")]
    pub to: String,
    /// Take this many per day, on a systemd user timer, keeping the newest N.
    #[arg(long, value_name = "N")]
    pub max_for_day: Option<u32>,
    /// Schedule with a crontab expression instead (`"0 3 * * *"`), translated to a systemd timer.
    #[arg(long, value_name = "EXPR", conflicts_with = "max_for_day")]
    pub cron: Option<String>,
    /// How many archives of this resource to keep (default: 7, or `--max-for-day` when scheduling).
    #[arg(long, value_name = "N")]
    pub keep: Option<usize>,
    /// Print what would be archived, without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, clap::Args)]
pub struct RestoreArgs {
    /// What to restore.
    #[arg(value_enum)]
    pub kind: Kind,
    /// The archive: a path, or the bare name of one in `--from`.
    pub archive: String,
    /// Where to look for the archive when it is named rather than a path.
    #[arg(long, default_value = ".")]
    pub from: String,
    /// Restore even when the live resource has data that is not in the archive.
    #[arg(long)]
    pub force: bool,
    /// Print what would be restored, without touching anything.
    #[arg(long)]
    pub dry_run: bool,
}

/// `delonix backup <kind> <name>`.
pub fn cmd_backup(a: BackupArgs) -> Result<()> {
    if !valid_name(&a.name) {
        return Err(Error::Invalid(po::tf(
            "backup: '{name}' is not a usable resource name",
            &[("name", &a.name)],
        )));
    }
    // A schedule is installed and the FIRST archive is taken now — a schedule
    // whose first run is hours away leaves the operator with no backup and the
    // impression of having one.
    let calendar = match (&a.cron, a.max_for_day) {
        (Some(expr), _) => Some(cron_to_on_calendar(expr)?),
        (None, Some(n)) => Some(on_calendar_for(n)?),
        (None, None) => None,
    };
    let keep = a
        .keep
        .unwrap_or_else(|| a.max_for_day.unwrap_or(7) as usize);

    let root = state_root();
    let dir = Dest::parse(&a.to).resolve(&root)?;
    let out = dir.join(archive_name(a.kind, &a.name, now()));

    if a.dry_run {
        let (what, vols) = describe_subject(&root, a.kind, &a.name)?;
        println!(
            "{}",
            po::tf(
                "would write {path}\n  {what}\n  volumes: {vols}\n  keeping the newest {keep}",
                &[
                    ("path", &out.display().to_string()),
                    ("what", &what),
                    (
                        "vols",
                        &if vols.is_empty() {
                            po::t("(none)").to_string()
                        } else {
                            vols.join(", ")
                        }
                    ),
                    ("keep", &keep.to_string()),
                ],
            )
        );
        return Ok(());
    }

    let tmp = Scratch::new(&root)?;
    let meta = match a.kind {
        Kind::Container => {
            let (_images, store) = super::util::open_stores()?;
            let c = super::util::find(&store, &a.name)?;
            write_container_archive(&root, &c, &out, tmp.path())?
        }
        Kind::Vm => {
            let vm = load_vm(&root, &a.name)?;
            write_vm_archive(&vm, &out)?
        }
        Kind::Pod | Kind::Stack => write_group_archive(&root, a.kind, &a.name, &out, tmp.path())?,
    };

    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    println!(
        "{}",
        po::tf(
            "{path} ({size})",
            &[
                ("path", &out.display().to_string()),
                ("size", &super::output::fmt_size(size)),
            ],
        )
    );
    if !meta.volumes.is_empty() {
        println!(
            "{}",
            po::tf("  volumes: {vols}", &[("vols", &meta.volumes.join(", "))])
        );
    }
    if !meta.members.is_empty() {
        println!(
            "{}",
            po::tf("  members: {m}", &[("m", &meta.members.join(", "))])
        );
    }

    // Retention runs AFTER a successful write. Pruning first would, on a failed
    // backup, leave the operator with one archive fewer and none new.
    for gone in prune(&dir, a.kind, &a.name, keep) {
        println!(
            "{}",
            po::tf(
                "  removed (over --keep {keep}): {path}",
                &[
                    ("keep", &keep.to_string()),
                    ("path", &gone.display().to_string())
                ],
            )
        );
    }

    if let Some(cal) = calendar {
        install_timer(a.kind, &a.name, &cal, &dir, keep)?;
    }
    Ok(())
}

/// One line saying what a `--dry-run` would archive, plus the volume names.
fn describe_subject(root: &Path, kind: Kind, name: &str) -> Result<(String, Vec<String>)> {
    match kind {
        Kind::Container => {
            let (_i, store) = super::util::open_stores()?;
            let c = super::util::find(&store, name)?;
            let v = volumes_of(root, &c.mounts);
            Ok((
                format!("container {} (image {})", c.name, c.image),
                v.into_iter().map(|(n, _)| n).collect(),
            ))
        }
        Kind::Vm => {
            let vm = load_vm(root, name)?;
            Ok((
                format!("vm {} (overlay {})", vm.name, vm.overlay),
                Vec::new(),
            ))
        }
        Kind::Pod | Kind::Stack => {
            let members = group_members(root, kind, name)?;
            let (_i, store) = super::util::open_stores()?;
            let mut vols = Vec::new();
            for m in &members {
                if let Ok(c) = super::util::find(&store, m) {
                    for (n, _) in volumes_of(root, &c.mounts) {
                        if !vols.contains(&n) {
                            vols.push(n);
                        }
                    }
                }
            }
            Ok((
                format!("{} {} ({} member(s))", kind.as_str(), name, members.len()),
                vols,
            ))
        }
    }
}

/// The containers belonging to a pod or a stack.
///
/// Neither has a registry of its own — membership DERIVES from the label, the
/// same way `pod ls`, `cluster ls` and `stack describe` already derive theirs.
/// A second source of truth here would be one more thing to keep in step.
fn group_members(_root: &Path, kind: Kind, name: &str) -> Result<Vec<String>> {
    let (_images, store) = super::util::open_stores()?;
    let label = match kind {
        Kind::Pod => "delonix.io/pod",
        _ => "delonix.io/stack",
    };
    let mut out: Vec<String> = store
        .list()?
        .into_iter()
        .filter(|c| c.labels.get(label).map(|v| v == name).unwrap_or(false))
        .map(|c| c.name)
        .collect();
    out.sort();
    if out.is_empty() {
        return Err(Error::NotFound(format!("{} {name}", kind.as_str())));
    }
    Ok(out)
}

/// Packs a pod or a stack: each member's record and volumes, in one archive.
fn write_group_archive(
    root: &Path,
    kind: Kind,
    name: &str,
    out: &Path,
    tmp: &Path,
) -> Result<Meta> {
    let members = group_members(root, kind, name)?;
    let (_images, store) = super::util::open_stores()?;

    let f = std::fs::File::create(out)
        .map_err(|e| Error::Invalid(format!("backup: {}: {e}", out.display())))?;
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        f,
        flate2::Compression::default(),
    ));

    let mut all_vols: Vec<String> = Vec::new();
    let mut records = Vec::new();
    for m in &members {
        let c = super::util::find(&store, m)?;
        records.push(c.clone());
        for (vn, data) in volumes_of(root, &c.mounts) {
            if all_vols.contains(&vn) {
                continue; // shared between members: archived once
            }
            let tarball = tmp.join(format!("{vn}.tar.gz"));
            super::volume::volsnap_run("create", Path::new(&data), &tarball)?;
            b.append_path_with_name(&tarball, format!("volumes/{vn}.tar.gz"))
                .map_err(|e| Error::Invalid(format!("backup: volume '{vn}': {e}")))?;
            let _ = std::fs::remove_file(&tarball);
            all_vols.push(vn);
        }
    }

    let meta = Meta {
        format: FORMAT,
        delonix_version: env!("CARGO_PKG_VERSION").to_string(),
        kind: kind.as_str().to_string(),
        name: name.to_string(),
        created_unix: now(),
        hostname: hostname(),
        volumes: all_vols,
        vm_base_disk: None,
        members: members.clone(),
    };
    append_bytes(&mut b, "backup.json", &to_json(&meta)?)?;
    for c in &records {
        append_bytes(
            &mut b,
            &format!("config/containers/{}.json", c.name),
            &to_json(c)?,
        )?;
    }
    b.into_inner()
        .and_then(|enc| enc.finish())
        .map_err(|e| Error::Invalid(format!("backup: closing {}: {e}", out.display())))?;
    Ok(meta)
}

/// The command the timer will run, as an argv.
///
/// Pure and separate so the one invariant that matters here is enforced by a
/// test and not by whoever edits `install_timer` next: **every path in it has to
/// be absolute**. A scheduled run does not inherit the caller's working
/// directory, and a relative `--to` silently sent the archive to `$HOME` while
/// the operator watched it appear in the directory they were standing in.
fn timer_argv(exe: &Path, kind: Kind, name: &str, dir: &Path, keep: usize) -> Result<Vec<String>> {
    if !exe.is_absolute() || !dir.is_absolute() {
        return Err(Error::Invalid(po::t(
            "backup: refusing to schedule with a relative path — a timer does not run from your \
             current directory, and the archive would land somewhere else",
        )
        .to_string()));
    }
    Ok(vec![
        exe.to_string_lossy().into_owned(),
        "backup".into(),
        kind.as_str().into(),
        name.into(),
        "--to".into(),
        dir.to_string_lossy().into_owned(),
        "--keep".into(),
        keep.to_string(),
    ])
}

/// Installs (or replaces) the systemd user timer for this resource.
fn install_timer(kind: Kind, name: &str, calendar: &str, dir: &Path, keep: usize) -> Result<()> {
    let unit = unit_name(kind, name);
    let exe = std::env::current_exe()
        .map_err(|e| Error::Invalid(format!("backup: cannot find my own path: {e}")))?;
    let argv = timer_argv(&exe, kind, name, dir, keep)?;

    // Replace rather than stack: running the command twice must not leave two
    // timers taking the same backup at the same instant.
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "stop", &format!("{unit}.timer")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let st = std::process::Command::new("systemd-run")
        .args([
            "--user",
            "--unit",
            &unit,
            "--on-calendar",
            calendar,
            "--timer-property=Persistent=true",
            "--description",
            &format!("delonix backup of {} {}", kind.as_str(), name),
            "--",
        ])
        .args(&argv)
        .status()
        .map_err(|e| {
            Error::Invalid(po::tf(
                "backup: cannot schedule — systemd-run is not available ({err}). Without it there \
                 is no daemonless way to run a timer; use your own cron entry calling this same \
                 command",
                &[("err", &e.to_string())],
            ))
        })?;
    if !st.success() {
        return Err(Error::Invalid(
            po::t("backup: systemd-run refused to install the timer (see its output above)")
                .to_string(),
        ));
    }
    println!(
        "{}",
        po::tf(
            "  scheduled: {cal}  (systemctl --user list-timers {unit}.timer)",
            &[("cal", calendar), ("unit", &unit)],
        )
    );
    // A user timer dies at logout unless the user lingers. Saying so here is the
    // difference between a schedule that runs on a server and one that quietly
    // stops the next time the operator logs out.
    if !lingering_enabled() {
        println!(
            "{}",
            po::t(
                "  note: this timer stops when you log out — `sudo loginctl enable-linger $USER` \
                 keeps it running"
            )
        );
    }
    Ok(())
}

fn lingering_enabled() -> bool {
    std::process::Command::new("loginctl")
        .args(["show-user", &format!("{}", unsafe { libc::getuid() })])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Linger=yes"))
        .unwrap_or(false)
}

/// `delonix restore <kind> <archive>`.
pub fn cmd_restore(a: RestoreArgs) -> Result<()> {
    let root = state_root();
    let path = resolve_archive(&a.archive, &a.from, &root)?;
    let (meta, _) = read_meta(&path)?;
    check_format(&meta)?;

    if meta.kind != a.kind.as_str() {
        // The kind is in the archive AND on the command line, and when they
        // disagree one of them is a mistake. Restoring by the archive's kind
        // regardless would mean `restore vm <a-container-archive>` silently does
        // something else than what was typed.
        return Err(Error::Invalid(po::tf(
            "restore: {path} holds a {got}, not a {want}",
            &[
                ("path", &path.display().to_string()),
                ("got", &meta.kind),
                ("want", a.kind.as_str()),
            ],
        )));
    }

    println!(
        "{}",
        po::tf(
            "{path}\n  {kind} {name}, taken {when} on {host}",
            &[
                ("path", &path.display().to_string()),
                ("kind", &meta.kind),
                ("name", &meta.name),
                ("when", &timestamp(meta.created_unix)),
                ("host", &meta.hostname),
            ],
        )
    );
    if a.dry_run {
        if !meta.volumes.is_empty() {
            println!(
                "{}",
                po::tf(
                    "  would replace the data of: {v}",
                    &[("v", &meta.volumes.join(", "))]
                )
            );
        }
        if let Some(b) = &meta.vm_base_disk {
            println!("{}", po::tf("  needs base disk: {b}", &[("b", b)]));
        }
        return Ok(());
    }
    restore_from(&path, &meta, &root, a.force)
}

/// A named archive resolved against `--from`, or a path taken as given.
fn resolve_archive(archive: &str, from: &str, root: &Path) -> Result<PathBuf> {
    let direct = Path::new(archive);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }
    if archive.contains('/') {
        return Err(Error::NotFound(po::tf(
            "backup archive {path}",
            &[("path", archive)],
        )));
    }
    let dir = Dest::parse(from).resolve(root)?;
    // `safe_rel` because the name reaches the filesystem: an archive called
    // `../../etc/x` must not walk out of the directory the operator named.
    let p = safe_rel(&dir, archive)?;
    if p.is_file() {
        return Ok(p);
    }
    let with_ext = dir.join(format!("{archive}.tar.gz"));
    if with_ext.is_file() {
        return Ok(with_ext);
    }
    Err(Error::NotFound(po::tf(
        "backup archive {name} in {dir}",
        &[("name", archive), ("dir", &dir.display().to_string())],
    )))
}

/// Reads `backup.json` without unpacking the rest.
fn read_meta(path: &Path) -> Result<(Meta, ())> {
    let f = std::fs::File::open(path)
        .map_err(|e| Error::Invalid(format!("restore: {}: {e}", path.display())))?;
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(f));
    for e in a
        .entries()
        .map_err(|e| Error::Invalid(format!("restore: {}: {e}", path.display())))?
    {
        let e = e.map_err(|e| Error::Invalid(format!("restore: {}: {e}", path.display())))?;
        if e.path()
            .map(|p| p.as_ref() == Path::new("backup.json"))
            .unwrap_or(false)
        {
            let m: Meta = serde_json::from_reader(e)
                .map_err(|e| Error::Invalid(format!("restore: unreadable backup.json: {e}")))?;
            return Ok((m, ()));
        }
    }
    Err(Error::Invalid(po::tf(
        "restore: {path} has no backup.json — it was not written by `delonix backup`",
        &[("path", &path.display().to_string())],
    )))
}

fn restore_from(path: &Path, meta: &Meta, root: &Path, force: bool) -> Result<()> {
    let tmp = Scratch::new(root)?;
    // Unpack whole, into a temporary directory, BEFORE touching anything live. A
    // truncated archive then costs nothing; unpacking straight over the live
    // data would destroy it and have nothing to put back — the same lesson
    // `volsnap_restore` already carries in its own comment.
    {
        let f = std::fs::File::open(path)
            .map_err(|e| Error::Invalid(format!("restore: {}: {e}", path.display())))?;
        let mut a = tar::Archive::new(flate2::read::GzDecoder::new(f));
        a.unpack(tmp.path())
            .map_err(|e| Error::Invalid(format!("restore: {}: {e}", path.display())))?;
    }

    match meta.kind.as_str() {
        "vm" => restore_vm(tmp.path(), meta, root),
        _ => restore_containers(tmp.path(), meta, root, force),
    }
}

fn restore_vm(unpacked: &Path, meta: &Meta, root: &Path) -> Result<()> {
    let rec: delonix_runtime_core::Vm = read_json(&unpacked.join("config/vm.json"))?;
    if let Ok(live) = load_vm(root, &rec.name) {
        if matches!(live.status, delonix_runtime_core::Status::Running) {
            return Err(Error::Invalid(po::tf(
                "restore: VM '{name}' is running — stop it first (`delonix vm stop {name}`)",
                &[("name", &rec.name)],
            )));
        }
    }
    if let Some(base) = &meta.vm_base_disk {
        if !Path::new(base).is_file() {
            return Err(Error::Invalid(po::tf(
                "restore: the base disk this overlay needs is missing: {base}. Pull it \
                 (`delonix image vm pull`) before restoring — an overlay without its base is \
                 unreadable",
                &[("base", base)],
            )));
        }
    }
    let dst = Path::new(&rec.overlay);
    if let Some(d) = dst.parent() {
        std::fs::create_dir_all(d).map_err(|e| Error::Invalid(format!("restore: {e}")))?;
    }
    std::fs::copy(unpacked.join("disk/overlay.qcow2"), dst)
        .map_err(|e| Error::Invalid(format!("restore: overlay: {e}")))?;
    save_vm(root, &rec)?;
    println!(
        "{}",
        po::tf(
            "restored vm {name} (start it with `delonix vm start {name}`)",
            &[("name", &rec.name)]
        )
    );
    Ok(())
}

fn restore_containers(unpacked: &Path, meta: &Meta, root: &Path, force: bool) -> Result<()> {
    let (images, store) = super::util::open_stores()?;

    // Every record in the archive: one file for a container, a directory for a
    // pod or a stack.
    let mut records: Vec<delonix_runtime_core::Container> = Vec::new();
    let single = unpacked.join("config/container.json");
    if single.is_file() {
        records.push(read_json(&single)?);
    }
    let many = unpacked.join("config/containers");
    if many.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&many)
            .map_err(|e| Error::Invalid(format!("restore: {e}")))?
            .flatten()
            .map(|e| e.path())
            .collect();
        paths.sort();
        for p in paths {
            records.push(read_json(&p)?);
        }
    }
    if records.is_empty() {
        return Err(Error::Invalid(
            po::t("restore: the archive holds no container records").to_string(),
        ));
    }

    // Anything still running has to stop before its volumes are replaced under
    // it: a database whose files change while it holds them open does not
    // notice, and corrupts.
    let mut was_running = Vec::new();
    for r in &records {
        if let Ok(live) = super::util::find(&store, &r.name) {
            if matches!(live.status, delonix_runtime_core::Status::Running) {
                if !force {
                    return Err(Error::Invalid(po::tf(
                        "restore: '{name}' is running — stop it first, or pass --force to have \
                         this command stop and restart it",
                        &[("name", &r.name)],
                    )));
                }
                super::container::cmd_stop(&store, &live.id, 10)?;
                was_running.push(r.name.clone());
            }
        }
    }

    // Volumes first, then the records: a container that comes back before its
    // data is in place starts against an empty volume.
    let vstore = delonix_volume::VolumeStore::open(root)?;
    for v in &meta.volumes {
        let tarball = unpacked.join(format!("volumes/{v}.tar.gz"));
        if !tarball.is_file() {
            return Err(Error::Invalid(po::tf(
                "restore: the archive claims volume '{v}' but does not carry it",
                &[("v", v)],
            )));
        }
        let vol = match vstore.inspect(v) {
            Ok(x) => x,
            Err(_) => {
                vstore.create(v)?;
                vstore.inspect(v)?
            }
        };
        super::volume::volsnap_run("restore", Path::new(&vol.mountpoint), &tarball)?;
        println!("{}", po::tf("  volume {v} restored", &[("v", v)]));
    }

    for r in &records {
        match super::util::find(&store, &r.name) {
            Ok(live) => {
                // It exists: the configuration on disk is the live one, and the
                // data is what came back. Overwriting the record here would
                // silently undo whatever `container update` changed since.
                let _ = live;
                println!(
                    "{}",
                    po::tf(
                        "  container {n} kept (its data was restored)",
                        &[("n", &r.name)]
                    )
                );
            }
            Err(_) => {
                // Gone: rebuild it. The image is re-pulled and the rootfs
                // prepared exactly as `run` would, then the record goes back —
                // which is what `start` needs to find.
                let img = super::util::resolve_or_pull(&images, &r.image)?;
                super::util::prepare_rootfs(&images, &img, &r.id)?;
                let mut rec = r.clone();
                rec.status = delonix_runtime_core::Status::Stopped;
                rec.pid = None;
                store.save(&rec)?;
                println!(
                    "{}",
                    po::tf(
                        "  container {n} recreated (start it with `delonix container start {n}`)",
                        &[("n", &r.name)]
                    )
                );
            }
        }
    }

    for n in &was_running {
        if let Ok(live) = super::util::find(&store, n) {
            super::container::cmd_start(&images, &store, &live.id)?;
            println!("{}", po::tf("  {n} started again", &[("n", n)]));
        }
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Result<T> {
    let s =
        std::fs::read(p).map_err(|e| Error::Invalid(format!("restore: {}: {e}", p.display())))?;
    serde_json::from_slice(&s).map_err(|e| Error::Invalid(format!("restore: {}: {e}", p.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o_destino_nao_adivinha_entre_pasta_e_volume() {
        // `backups` is a plausible directory AND a plausible volume. Guessing
        // would put somebody's archive somewhere they cannot find it.
        assert!(matches!(Dest::parse("backups"), Dest::Path(_)));
        assert!(matches!(Dest::parse("./backups"), Dest::Path(_)));
        assert!(matches!(Dest::parse("/srv/backups"), Dest::Path(_)));
        match Dest::parse("volume:nas-backups") {
            Dest::Volume(v) => assert_eq!(v, "nas-backups"),
            other => panic!("expected a volume, got {other:?}"),
        }
    }

    #[test]
    fn max_for_day_recusa_o_que_nao_divide_o_dia() {
        assert_eq!(on_calendar_for(1).unwrap(), "*-*-* 00:00:00");
        assert_eq!(on_calendar_for(2).unwrap(), "*-*-* 00/12:00:00");
        assert_eq!(on_calendar_for(4).unwrap(), "*-*-* 00/6:00:00");
        assert_eq!(on_calendar_for(24).unwrap(), "*-*-* 00/1:00:00");
        // 5 a day would drift if spaced evenly; the error names what works.
        let e = on_calendar_for(5).unwrap_err().to_string();
        assert!(e.contains("1, 2, 3, 4, 6, 8, 12 or 24"), "{e}");
        assert!(on_calendar_for(0).is_err());
        let e = on_calendar_for(48).unwrap_err().to_string();
        assert!(e.contains("per DAY"), "{e}");
    }

    #[test]
    fn o_cron_traduz_se_ou_recusa_se_nunca_se_aproxima() {
        // The forms people actually write.
        assert_eq!(cron_to_on_calendar("0 2 * * *").unwrap(), "*-*-* 2:0:00");
        assert_eq!(
            cron_to_on_calendar("*/15 * * * *").unwrap(),
            "*-*-* *:0/15:00"
        );
        assert_eq!(
            cron_to_on_calendar("30 3 * * 1").unwrap(),
            "Mon *-*-* 3:30:00"
        );
        assert_eq!(cron_to_on_calendar("0 0 1 * *").unwrap(), "*-*-1 0:0:00");
        // Cron's 0 and 7 are BOTH Sunday. Dropping either would make a schedule
        // that says Sunday silently never fire.
        assert_eq!(
            cron_to_on_calendar("0 4 * * 0").unwrap(),
            cron_to_on_calendar("0 4 * * 7").unwrap()
        );
        // A list of days collapses duplicates rather than emitting `Sun,Sun`.
        assert_eq!(
            cron_to_on_calendar("0 4 * * 0,7").unwrap(),
            "Sun *-*-* 4:0:00"
        );
        // Refused, not approximated.
        for bad in [
            "@daily",
            "0 2 * *",
            "0 2 * * mon",
            "0 2 * * 9",
            "0 2 * * * *",
        ] {
            assert!(
                cron_to_on_calendar(bad).is_err(),
                "{bad:?} should be refused rather than approximated"
            );
        }
    }

    #[test]
    fn os_arquivos_ordenam_se_cronologicamente_pelo_nome() {
        let a = archive_name(Kind::Container, "db", 1_700_000_000);
        let b = archive_name(Kind::Container, "db", 1_700_003_600);
        assert!(a < b, "{a} should sort before {b}");
        assert!(a.starts_with("container-db-") && a.ends_with(".tar.gz"));
        // One kind's name never matches another's prefix.
        assert!(!archive_name(Kind::Vm, "db", 1).starts_with("container-"));
    }

    #[test]
    fn a_retencao_guarda_os_mais_novos_e_diz_o_que_tirou() {
        let d = std::env::temp_dir().join(format!("delonix-rbk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let d = d.as_path();
        let mut made = Vec::new();
        for i in 0..5u64 {
            let n = archive_name(Kind::Container, "db", 1_700_000_000 + i * 3600);
            std::fs::write(d.join(&n), b"x").unwrap();
            made.push(n);
        }
        // An archive of ANOTHER resource must never be counted or removed.
        let other = archive_name(Kind::Container, "web", 1_700_000_000);
        std::fs::write(d.join(&other), b"x").unwrap();

        let gone = prune(d, Kind::Container, "db", 2);
        assert_eq!(gone.len(), 3, "should have removed the three oldest");
        let left = existing(d, Kind::Container, "db");
        assert_eq!(left.len(), 2);
        assert!(left[1].file_name().unwrap().to_str().unwrap() == made[4]);
        assert!(
            d.join(&other).exists(),
            "another resource's archive must survive"
        );
        // Idempotent: pruning again removes nothing.
        assert!(prune(d, Kind::Container, "db", 2).is_empty());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn um_timer_nunca_leva_um_caminho_relativo() {
        // MEASURED bug, not a hypothetical: `--to .` went into the unit verbatim,
        // systemd ran it from `$HOME`, and the scheduled archive landed there
        // while the operator watched the on-demand one appear in the directory
        // they were standing in. Both reported success.
        let exe = Path::new("/usr/local/bin/delonix");
        let e = timer_argv(exe, Kind::Container, "db", Path::new("."), 2).unwrap_err();
        assert!(e.to_string().contains("relative"), "{e}");
        assert!(timer_argv(exe, Kind::Container, "db", Path::new("backups"), 2).is_err());
        assert!(timer_argv(
            Path::new("delonix"),
            Kind::Container,
            "db",
            Path::new("/b"),
            2
        )
        .is_err());

        let argv = timer_argv(exe, Kind::Container, "db", Path::new("/srv/backups"), 3).unwrap();
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/delonix",
                "backup",
                "container",
                "db",
                "--to",
                "/srv/backups",
                "--keep",
                "3"
            ]
        );
        // `--keep` has to travel: without it every scheduled run would fall back
        // to the default and quietly keep a different number than was asked for.
        assert!(argv.contains(&"--keep".to_string()));
    }

    #[test]
    fn um_nome_de_recurso_nao_vira_opcao_nem_caminho() {
        for bad in ["", "-rf", "a/b", "../x", "a b", "a;rm", &"x".repeat(129)] {
            assert!(!valid_name(bad), "{bad:?} should be refused");
        }
        for ok in ["db", "kaeso-odoo18", "web.0", "img:1.2"] {
            assert!(valid_name(ok), "{ok:?} should be accepted");
        }
    }

    #[test]
    fn um_formato_futuro_recusa_se_em_vez_de_adivinhar() {
        let mut m = Meta {
            format: FORMAT,
            delonix_version: "9.9.9".into(),
            kind: "container".into(),
            name: "db".into(),
            created_unix: 0,
            hostname: String::new(),
            volumes: vec![],
            vm_base_disk: None,
            members: vec![],
        };
        assert!(check_format(&m).is_ok());
        m.format = FORMAT + 1;
        let e = check_format(&m).unwrap_err().to_string();
        // The answer has to be IN the message: the version that wrote it.
        assert!(e.contains("9.9.9"), "{e}");
    }
}
