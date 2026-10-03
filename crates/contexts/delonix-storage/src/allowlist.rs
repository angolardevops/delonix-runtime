//! The administrator's allowlist of storage pools (ADR-0067 D3).
//!
//! `/etc/delonix/storage-pools.yaml` says which pools exist on this node, which
//! driver serves each and what backs it. A manifest only ever carries a pool
//! NAME; the driver, the path, the volume group or the dataset come from here,
//! and here only. That is the boundary: a document someone else wrote cannot
//! point the engine at a device or a directory.
//!
//! ```yaml
//! pools:
//!   media:
//!     driver: dir
//!     path: /srv/dlx
//!     maxVolumeBytes: 50G
//!   fast:
//!     driver: lvm-thin
//!     vg: vg0
//!     thinPool: dlx
//!     allowUsers: [walter]
//! ```
//!
//! The file is read on every use — there is no cache to go stale — and it is
//! refused when anyone but its owner can write it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Error, Result};

/// Where the allowlist lives.
pub const DEFAULT_PATH: &str = "/etc/delonix/storage-pools.yaml";

/// Names another allowlist file, for a node whose administrator keeps it
/// elsewhere and for tests. It is the OPERATOR's variable, never a manifest
/// field; the privileged helper (ADR-0067 P1) does not read it.
pub const PATH_ENV: &str = "DELONIX_STORAGE_POOLS_FILE";

/// The drivers the decision names, and the phase of ADR-0067 that builds each.
/// A pool whose driver is here and not registered is refused as «not built
/// yet», naming the phase; a driver that is not here is a typo.
pub const DRIVERS: &[(&str, &str)] = &[
    ("dir", "P0"),
    ("btrfs", "P2"),
    ("zfs", "P3"),
    ("lvm-thin", "P4"),
];

/// The whole file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allowlist {
    /// How privileged operations run: absent or `helper` (the socket-activated
    /// helper), or `in-process` (an engine the administrator runs as root).
    /// Never inferred from the uid.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub pools: BTreeMap<String, Entry>,
}

/// One pool, as the administrator declared it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Entry {
    pub driver: String,
    /// `dir`, `btrfs`: the directory (or mounted filesystem) that holds the volumes.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// `lvm-thin`: the volume group.
    #[serde(default)]
    pub vg: Option<String>,
    /// `lvm-thin`: the thin pool inside the volume group.
    #[serde(default)]
    pub thin_pool: Option<String>,
    /// `zfs`: the parent dataset.
    #[serde(default)]
    pub dataset: Option<String>,
    /// What `delonix-storage-helper create-pool` may build the pool from. Read
    /// by that administrative command only, never by the engine.
    #[serde(default)]
    pub create: Option<Create>,
    /// Users allowed to allocate through the helper.
    #[serde(default)]
    pub allow_users: Vec<String>,
    /// The largest volume a caller may ask of this pool (`50G`).
    #[serde(default)]
    pub max_volume_bytes: Option<String>,
}

/// The `create:` block of an entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    #[serde(default)]
    pub devices: Vec<PathBuf>,
}

/// The file the engine reads: [`PATH_ENV`] when set, else [`DEFAULT_PATH`].
pub fn path() -> PathBuf {
    match std::env::var_os(PATH_ENV) {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(DEFAULT_PATH),
    }
}

/// Is `name` a pool name? Lower-case letters, digits, `.`, `_` and `-`,
/// starting with a letter or digit, at most 63 bytes: it becomes a path
/// component and, in the block drivers, part of a volume's name.
pub fn valid_pool_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 63
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        && !name.contains("..")
}

/// Reads the allowlist the engine uses. `Ok(None)` when the file does not
/// exist: no pool was ever declared on this node, which is a different answer
/// from a file that is there and cannot be used.
pub fn load() -> Result<Option<Allowlist>> {
    load_from(&path())
}

/// [`load`] from a given file.
pub fn load_from(file: &Path) -> Result<Option<Allowlist>> {
    let meta = match std::fs::metadata(file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::PoolUndetermined(format!(
                "the storage pool allowlist {} cannot be read: {e}",
                file.display()
            )))
        }
    };
    check_writers(file, &meta)?;
    let text = std::fs::read_to_string(file).map_err(|e| {
        Error::PoolUndetermined(format!(
            "the storage pool allowlist {} cannot be read: {e}",
            file.display()
        ))
    })?;
    parse(&text, file).map(Some)
}

/// Only the file's owner may be able to write it, and the owner is root or
/// whoever runs the engine. An allowlist another user can edit is that user
/// choosing where this engine creates and deletes directories.
fn check_writers(file: &Path, meta: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if meta.mode() & 0o022 != 0 {
        return Err(Error::InvalidAllowlist(format!(
            "{} is writable by its group or by everyone (mode {:o}) — `chmod 644` it; \
             only its owner may decide which pools exist",
            file.display(),
            meta.mode() & 0o7777
        )));
    }
    if meta.uid() != 0 && meta.uid() != euid {
        return Err(Error::InvalidAllowlist(format!(
            "{} belongs to uid {}, which is neither root nor the user running the engine",
            file.display(),
            meta.uid()
        )));
    }
    Ok(())
}

/// Parses and validates the text of an allowlist. `file` is only named in errors.
pub fn parse(text: &str, file: &Path) -> Result<Allowlist> {
    let list: Allowlist = if text.trim().is_empty() {
        Allowlist::default()
    } else {
        serde_yaml::from_str(text)
            .map_err(|e| Error::InvalidAllowlist(format!("{}: {e}", file.display())))?
    };
    if let Some(mode) = list.mode.as_deref() {
        if !matches!(mode, "helper" | "in-process") {
            return Err(Error::InvalidAllowlist(format!(
                "{}: mode '{mode}' is not valid (helper | in-process)",
                file.display()
            )));
        }
    }
    for (name, entry) in &list.pools {
        entry
            .validate(name)
            .map_err(|why| Error::InvalidAllowlist(format!("{}: pool '{name}': {why}", file.display())))?;
    }
    Ok(list)
}

impl Entry {
    /// What is wrong with this entry, if anything.
    fn validate(&self, name: &str) -> std::result::Result<(), String> {
        if !valid_pool_name(name) {
            return Err(
                "the name must be lower-case letters, digits, '.', '_' or '-', starting with a \
                 letter or digit, at most 63 characters"
                    .into(),
            );
        }
        if !DRIVERS.iter().any(|(id, _)| *id == self.driver) {
            let known: Vec<&str> = DRIVERS.iter().map(|(id, _)| *id).collect();
            return Err(format!(
                "driver '{}' is not one of: {}",
                self.driver,
                known.join(", ")
            ));
        }
        // Each driver has its own backing object, and a field that belongs to
        // another driver is refused: an entry that says `driver: dir` and
        // `vg: vg0` is two answers to where the data lives.
        let given = [
            ("path", self.path.is_some()),
            ("vg", self.vg.is_some()),
            ("thinPool", self.thin_pool.is_some()),
            ("dataset", self.dataset.is_some()),
        ];
        let wanted: &[&str] = match self.driver.as_str() {
            "dir" | "btrfs" => &["path"],
            "zfs" => &["dataset"],
            _ => &["vg", "thinPool"],
        };
        for (field, present) in given {
            match (wanted.contains(&field), present) {
                (true, false) => return Err(format!("driver '{}' needs `{field}`", self.driver)),
                (false, true) => {
                    return Err(format!(
                        "`{field}` does not belong to driver '{}'",
                        self.driver
                    ))
                }
                _ => {}
            }
        }
        if let Some(p) = &self.path {
            if !p.is_absolute()
                || p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                || p == Path::new("/")
            {
                return Err(format!(
                    "`path` must be an absolute path without '..', and not '/' (got {})",
                    p.display()
                ));
            }
        }
        if let Some(c) = &self.create {
            for d in &c.devices {
                if !d.starts_with("/dev/disk/by-id") && !d.starts_with("/dev/disk/by-path") {
                    return Err(format!(
                        "create.devices: {} is not a stable name — use /dev/disk/by-id/… or \
                         /dev/disk/by-path/…",
                        d.display()
                    ));
                }
            }
        }
        Ok(())
    }

    /// `maxVolumeBytes` in bytes. The size grammar belongs to whoever parses
    /// every other size in the engine, so it is handed in.
    pub fn max_volume_bytes(
        &self,
        pool: &str,
        parse_size: impl Fn(&str) -> Option<u64>,
    ) -> Result<Option<u64>> {
        match self.max_volume_bytes.as_deref() {
            None => Ok(None),
            Some(raw) => parse_size(raw).map(Some).ok_or_else(|| {
                Error::InvalidAllowlist(format!(
                    "pool '{pool}': maxVolumeBytes '{raw}' is not a size (e.g. 50G)"
                ))
            }),
        }
    }
}

/// The ADR-0067 phase that builds `driver`, for the refusal of a pool whose
/// driver is declared and not registered yet.
pub fn phase_of(driver: &str) -> Option<&'static str> {
    DRIVERS
        .iter()
        .find(|(id, _)| *id == driver)
        .map(|(_, phase)| *phase)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(text: &str) -> Allowlist {
        parse(text, Path::new("test.yaml")).unwrap()
    }

    fn parse_err(text: &str) -> String {
        parse(text, Path::new("test.yaml")).unwrap_err().to_string()
    }

    #[test]
    fn a_dir_pool_needs_a_path_and_nothing_of_another_driver() {
        let l = parse_ok("pools:\n  media:\n    driver: dir\n    path: /srv/dlx\n");
        assert_eq!(l.pools["media"].path.as_deref(), Some(Path::new("/srv/dlx")));
        assert!(parse_err("pools:\n  media:\n    driver: dir\n").contains("needs `path`"));
        assert!(
            parse_err("pools:\n  media:\n    driver: dir\n    path: /srv/x\n    vg: vg0\n")
                .contains("`vg` does not belong to driver 'dir'")
        );
    }

    #[test]
    fn the_block_drivers_parse_with_their_own_backing() {
        let l = parse_ok(
            "mode: in-process\npools:\n  fast:\n    driver: lvm-thin\n    vg: vg0\n    thinPool: dlx\n    \
             allowUsers: [walter]\n    maxVolumeBytes: 500G\n    create:\n      devices: [/dev/disk/by-id/nvme-X]\n  \
             tank:\n    driver: zfs\n    dataset: tank/dlx\n",
        );
        assert_eq!(l.pools["fast"].thin_pool.as_deref(), Some("dlx"));
        assert_eq!(l.pools["tank"].dataset.as_deref(), Some("tank/dlx"));
        assert_eq!(l.mode.as_deref(), Some("in-process"));
    }

    #[test]
    fn a_field_nobody_defined_is_refused_not_ignored() {
        assert!(parse_err("pools:\n  m:\n    driver: dir\n    path: /srv/x\n    command: rm\n")
            .contains("command"));
        assert!(parse_err("poools: {}\n").contains("poools"));
    }

    #[test]
    fn a_path_that_could_leave_where_it_points_is_refused() {
        for bad in ["srv/x", "/srv/../etc", "/"] {
            let text = format!("pools:\n  m:\n    driver: dir\n    path: {bad}\n");
            assert!(parse_err(&text).contains("absolute path"), "{bad}");
        }
    }

    #[test]
    fn a_create_device_must_have_a_stable_name() {
        let text = "pools:\n  f:\n    driver: lvm-thin\n    vg: v\n    thinPool: t\n    create:\n      devices: [/dev/sdb]\n";
        assert!(parse_err(text).contains("not a stable name"));
    }

    #[test]
    fn pool_names_are_one_safe_path_component() {
        for ok in ["media", "fast-1", "a.b_c", "0"] {
            assert!(valid_pool_name(ok), "{ok}");
        }
        for bad in ["", "Media", "-x", ".x", "a/b", "a..b", "a b", &"x".repeat(64)] {
            assert!(!valid_pool_name(bad), "{bad}");
        }
    }

    #[test]
    fn an_unknown_driver_or_mode_names_the_valid_ones() {
        assert!(parse_err("pools:\n  m:\n    driver: ceph\n").contains("dir, btrfs, zfs, lvm-thin"));
        assert!(parse_err("mode: sudo\n").contains("helper | in-process"));
    }

    /// No file is «no pool was ever declared»; a file that is there and that
    /// someone else can write is a refusal.
    #[test]
    fn a_missing_file_is_none_and_a_writable_one_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("pools.yaml");
        assert!(load_from(&file).unwrap().is_none());
        std::fs::write(&file, "pools: {}\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_from(&file).unwrap().is_some());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
        let e = load_from(&file).unwrap_err().to_string();
        assert!(e.contains("writable by its group or by everyone"), "{e}");
    }

    #[test]
    fn max_volume_bytes_uses_the_callers_size_grammar() {
        let l = parse_ok("pools:\n  m:\n    driver: dir\n    path: /srv/x\n    maxVolumeBytes: 2G\n");
        let parse = |s: &str| s.strip_suffix('G').and_then(|n| n.parse::<u64>().ok()).map(|n| n << 30);
        assert_eq!(l.pools["m"].max_volume_bytes("m", parse).unwrap(), Some(2 << 30));
        let none = |_: &str| None;
        assert!(l.pools["m"].max_volume_bytes("m", none).is_err());
    }
}
