//! Pool administration: `create-pool` and `destroy-pool` (ADR-0067 D3.3, D5,
//! phase P6).
//!
//! These are NOT on the driver port and never cross the helper's socket. They
//! are what an administrator runs as root at a terminal, from the `create:`
//! block of the root-owned allowlist — the only place a raw device is named.
//! This module holds the parts that are the same for every backend, so each
//! driver writes only what differs:
//!
//! - [`check_devices`]: every device resolves to a block device, none twice,
//!   and none carries a signature unless the administrator said `--wipe-devices`
//!   — all checked before the first one is touched.
//! - [`destroy_guarded`]: the destructive guards. A pool without `create:` is
//!   never destroyed; one without the helper's creation mark on the pool
//!   itself is never destroyed; one that holds anything is refused, naming it.

use std::path::{Path, PathBuf};

use crate::exec::{argv, Run};
use crate::pool::PoolRef;
use crate::{Error, Result};

/// What a backend does to make and unmake a pool. Implemented by the same
/// driver struct that implements the port; called only by the helper's
/// administrative commands.
pub trait PoolAdmin: Send + Sync {
    fn id(&self) -> &'static str;

    /// Builds the pool from `devices` (already checked by [`check_devices`])
    /// and puts the creation mark on it. Returns what was done, for the
    /// administrator.
    fn create_pool(&self, pool: &PoolRef, devices: &[PathBuf]) -> Result<String>;

    /// Does the pool carry this helper's creation mark? Read from the pool
    /// itself (a VG tag, a ZFS property, a file at the filesystem's root).
    fn created_here(&self, pool: &PoolRef) -> Result<bool>;

    /// Everything the pool holds — a volume of this engine or anything else.
    fn contents(&self, pool: &PoolRef) -> Result<Vec<String>>;

    /// Unmakes the pool. Called only after [`destroy_guarded`]'s checks.
    fn destroy_pool(&self, pool: &PoolRef) -> Result<String>;
}

/// Resolves a device the allowlist names to the block device it is now.
pub type ResolveDevice<'a> = &'a dyn Fn(&Path) -> Result<PathBuf>;

/// The real resolution: the link is followed, and what it reaches must be a
/// block device under `/dev`.
pub fn resolve_block_device(dev: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    let real = std::fs::canonicalize(dev).map_err(|e| {
        Error::DeviceInUse(format!(
            "{} does not resolve to a device: {e}",
            dev.display()
        ))
    })?;
    let meta = std::fs::metadata(&real)
        .map_err(|e| Error::DeviceInUse(format!("{} cannot be examined: {e}", real.display())))?;
    if !real.starts_with("/dev") || !meta.file_type().is_block_device() {
        return Err(Error::DeviceInUse(format!(
            "{} resolves to {}, which is not a block device",
            dev.display(),
            real.display()
        )));
    }
    Ok(real)
}

/// Does `dev` carry a filesystem, RAID or volume signature? `blkid -p` probes
/// the device itself (no cache): exit 0 with a `TYPE=` is a signature, exit 2
/// is none (`blkid(8)`: «no … devices could be identified»). Anything else is
/// an answer we could not read, and is a refusal — never «empty».
pub fn signature(run: &dyn Run, dev: &Path) -> Result<Option<String>> {
    let out = run
        .run(&argv(&[
            "blkid",
            "-p",
            "-o",
            "export",
            &dev.to_string_lossy(),
        ]))
        .map_err(|e| Error::PoolUnavailable(format!("blkid cannot be run: {e}")))?;
    match out.code {
        Some(2) => Ok(None),
        Some(0) => {
            let kind = out
                .stdout
                .lines()
                .find_map(|l| {
                    l.strip_prefix("TYPE=")
                        .or_else(|| l.strip_prefix("PTTYPE="))
                })
                .unwrap_or("a signature")
                .to_string();
            Ok(Some(kind))
        }
        _ => Err(Error::DeviceInUse(format!(
            "{} could not be probed for a signature: {}",
            dev.display(),
            out.why()
        ))),
    }
}

/// Every device of a `create:` block, resolved and checked, before anything is
/// written. With `wipe`, the signatures are cleared — only after every device
/// passed the other checks, so one bad name never leaves another wiped.
pub fn check_devices(
    run: &dyn Run,
    resolve: ResolveDevice,
    devices: &[PathBuf],
    wipe: bool,
) -> Result<Vec<PathBuf>> {
    if devices.is_empty() {
        return Err(Error::InvalidAllowlist(
            "create.devices is empty — name the devices the pool is built from".into(),
        ));
    }
    let mut real = Vec::new();
    for d in devices {
        let r = resolve(d)?;
        if real.contains(&r) {
            return Err(Error::DeviceInUse(format!(
                "{} resolves to {}, which another entry of create.devices already names",
                d.display(),
                r.display()
            )));
        }
        real.push(r);
    }
    let mut marked = Vec::new();
    for r in &real {
        if let Some(kind) = signature(run, r)? {
            marked.push((r.clone(), kind));
        }
    }
    if !marked.is_empty() && !wipe {
        let list: Vec<String> = marked
            .iter()
            .map(|(d, k)| format!("{} ({k})", d.display()))
            .collect();
        return Err(Error::DeviceInUse(format!(
            "these devices carry a signature and are left as they are: {} — pass \
             --wipe-devices to overwrite them, knowing it destroys what is on them",
            list.join(", ")
        )));
    }
    for (d, _) in &marked {
        let out = run
            .run(&argv(&["wipefs", "--all", "--", &d.to_string_lossy()]))
            .map_err(|e| Error::PoolUnavailable(format!("wipefs cannot be run: {e}")))?;
        if !out.success() {
            return Err(Error::DeviceInUse(format!(
                "wipefs on {} failed: {}",
                d.display(),
                out.why()
            )));
        }
    }
    Ok(real)
}

/// The destructive path, with every guard ADR-0067 D5 names, in order. A
/// refusal is before anything is touched.
pub fn destroy_guarded(admin: &dyn PoolAdmin, pool: &PoolRef) -> Result<String> {
    if pool.entry.create.is_none() {
        return Err(Error::PoolNotCreatedHere(format!(
            "storage pool '{}' has no `create:` block in the allowlist: it existed before the \
             engine, and is never destroyed — remove its entry to stop using it",
            pool.name
        )));
    }
    if !admin.created_here(pool)? {
        return Err(Error::PoolNotCreatedHere(format!(
            "storage pool '{}' does not carry this helper's creation mark — it is never \
             destroyed by this command",
            pool.name
        )));
    }
    let held = admin.contents(pool)?;
    if !held.is_empty() {
        return Err(Error::PoolConflict(format!(
            "storage pool '{}' still holds {} volume(s): {} — release them first (`delonix \
             volume rm`); nothing was destroyed",
            pool.name,
            held.len(),
            held.join(", ")
        )));
    }
    admin.destroy_pool(pool)
}

/// The creative path: the pool must declare `create:`, and every device passes
/// [`check_devices`] before the backend builds anything.
pub fn create_checked(
    admin: &dyn PoolAdmin,
    run: &dyn Run,
    resolve: ResolveDevice,
    pool: &PoolRef,
    wipe: bool,
) -> Result<String> {
    let Some(create) = &pool.entry.create else {
        return Err(Error::InvalidAllowlist(format!(
            "storage pool '{}' has no `create:` block — there is nothing to create it from",
            pool.name
        )));
    };
    let devices = check_devices(run, resolve, &create.devices, wipe)?;
    admin.create_pool(pool, &devices)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::allowlist::{Create, Entry};
    use crate::exec::testing::Script;

    struct FakeAdmin {
        created: bool,
        contents: Vec<String>,
        calls: Mutex<Vec<String>>,
    }

    impl PoolAdmin for FakeAdmin {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn create_pool(&self, _: &PoolRef, devices: &[PathBuf]) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("create {}", devices.len()));
            Ok("made".into())
        }
        fn created_here(&self, _: &PoolRef) -> Result<bool> {
            Ok(self.created)
        }
        fn contents(&self, _: &PoolRef) -> Result<Vec<String>> {
            Ok(self.contents.clone())
        }
        fn destroy_pool(&self, _: &PoolRef) -> Result<String> {
            self.calls.lock().unwrap().push("destroy".into());
            Ok("gone".into())
        }
    }

    fn admin(created: bool, contents: &[&str]) -> FakeAdmin {
        FakeAdmin {
            created,
            contents: contents.iter().map(|s| s.to_string()).collect(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn entry(create: bool) -> Entry {
        Entry {
            driver: "lvm-thin".into(),
            vg: Some("vg0".into()),
            thin_pool: Some("dlx".into()),
            create: create.then(|| Create {
                devices: vec![
                    PathBuf::from("/dev/disk/by-id/a"),
                    PathBuf::from("/dev/disk/by-id/b"),
                ],
            }),
            ..Default::default()
        }
    }

    fn pool(e: &Entry) -> PoolRef<'_> {
        PoolRef {
            name: "fast",
            entry: e,
        }
    }

    /// P6: a pool the allowlist does not mark `create:` is never destroyed.
    #[test]
    fn a_pool_without_create_is_never_destroyed() {
        let a = admin(true, &[]);
        let e = entry(false);
        let err = destroy_guarded(&a, &pool(&e)).unwrap_err();
        assert_eq!(err.number(), 5204, "{err}");
        assert!(a.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_pool_without_the_creation_mark_is_never_destroyed() {
        let a = admin(false, &[]);
        let e = entry(true);
        let err = destroy_guarded(&a, &pool(&e)).unwrap_err();
        assert_eq!(err.number(), 5204, "{err}");
        assert!(err.to_string().contains("creation mark"));
        assert!(a.calls.lock().unwrap().is_empty());
    }

    /// P6: a pool with data — stamped or not — is refused, naming it.
    #[test]
    fn a_pool_with_data_is_refused_and_names_what_it_holds() {
        let a = admin(true, &["dlx-db", "someone-elses"]);
        let e = entry(true);
        let err = destroy_guarded(&a, &pool(&e)).unwrap_err();
        assert_eq!(err.number(), 5201, "{err}");
        assert!(err.to_string().contains("dlx-db, someone-elses"), "{err}");
        assert!(a.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn an_empty_pool_this_helper_created_is_destroyed() {
        let a = admin(true, &[]);
        let e = entry(true);
        assert_eq!(destroy_guarded(&a, &pool(&e)).unwrap(), "gone");
        assert_eq!(*a.calls.lock().unwrap(), vec!["destroy".to_string()]);
    }

    fn resolve_ok(d: &Path) -> Result<PathBuf> {
        Ok(match d.to_str().unwrap() {
            "/dev/disk/by-id/a" => PathBuf::from("/dev/vdb"),
            "/dev/disk/by-id/b" => PathBuf::from("/dev/vdc"),
            "/dev/disk/by-id/a-again" => PathBuf::from("/dev/vdb"),
            other => return Err(Error::DeviceInUse(format!("{other} is not a block device"))),
        })
    }

    #[test]
    fn a_device_with_a_signature_is_left_alone_unless_wipe_is_given() {
        let run = Script::new();
        run.on(&["blkid", "-p", "-o", "export", "/dev/vdb"], 2, "", "")
            .on(
                &["blkid", "-p", "-o", "export", "/dev/vdc"],
                0,
                "DEVNAME=/dev/vdc\nTYPE=ext4\n",
                "",
            )
            .on(&["wipefs"], 0, "", "");
        let a = admin(false, &[]);
        let e = entry(true);
        let err = create_checked(&a, &run, &resolve_ok, &pool(&e), false).unwrap_err();
        assert_eq!(err.number(), 5205, "{err}");
        assert!(err.to_string().contains("/dev/vdc (ext4)"), "{err}");
        assert!(run.calls_of("wipefs").is_empty());
        assert!(a.calls.lock().unwrap().is_empty());

        // With --wipe-devices only the marked one is wiped, then the pool is made.
        assert_eq!(
            create_checked(&a, &run, &resolve_ok, &pool(&e), true).unwrap(),
            "made"
        );
        assert_eq!(
            run.calls_of("wipefs"),
            vec![argv(&["wipefs", "--all", "--", "/dev/vdc"])]
        );
    }

    #[test]
    fn one_bad_device_stops_the_creation_before_any_is_wiped() {
        let run = Script::new();
        run.on(&["blkid"], 0, "TYPE=xfs\n", "");
        let a = admin(false, &[]);
        let mut e = entry(true);
        e.create
            .as_mut()
            .unwrap()
            .devices
            .push(PathBuf::from("/dev/disk/by-id/nope"));
        assert_eq!(
            create_checked(&a, &run, &resolve_ok, &pool(&e), true)
                .unwrap_err()
                .number(),
            5205
        );
        assert!(run.calls().is_empty(), "{:?}", run.calls());

        let mut e = entry(true);
        e.create
            .as_mut()
            .unwrap()
            .devices
            .push(PathBuf::from("/dev/disk/by-id/a-again"));
        let err = create_checked(&a, &run, &resolve_ok, &pool(&e), true).unwrap_err();
        assert!(err.to_string().contains("already names"), "{err}");
        assert!(run.calls().is_empty());
    }

    #[test]
    fn an_unreadable_blkid_answer_is_a_refusal_never_no_signature() {
        let run = Script::new();
        run.on(
            &["blkid"],
            4,
            "",
            "blkid: error: /dev/vdb: Permission denied",
        );
        assert_eq!(
            signature(&run, Path::new("/dev/vdb")).unwrap_err().number(),
            5205
        );
    }

    #[test]
    fn a_pool_without_create_has_nothing_to_create_from() {
        let run = Script::new();
        let a = admin(false, &[]);
        let e = entry(false);
        assert_eq!(
            create_checked(&a, &run, &resolve_ok, &pool(&e), false)
                .unwrap_err()
                .number(),
            1218
        );
    }

    #[test]
    fn a_path_that_is_not_a_block_device_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("disk.img");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(resolve_block_device(&f).unwrap_err().number(), 5205);
        assert_eq!(
            resolve_block_device(&tmp.path().join("missing"))
                .unwrap_err()
                .number(),
            5205
        );
    }
}
