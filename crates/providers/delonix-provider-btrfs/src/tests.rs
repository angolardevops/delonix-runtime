//! The driver proven on a temporary directory with a scripted `btrfs`: the
//! runner makes the directory a `subvolume create` would, so the filesystem
//! side (stamp, `_data`, release order) runs for real. The `btrfs filesystem
//! show --raw` sample follows the layout `btrfs-filesystem(8)` documents
//! (`devid <n> size <bytes> used <bytes> path <device>`); it was NOT captured
//! from a live btrfs here — this host has no btrfs filesystem to read.

use std::os::unix::fs::MetadataExt;

use delonix_storage::admin::{destroy_guarded, PoolAdmin};
use delonix_storage::allowlist::{Create, Entry};
use delonix_storage::exec::testing::Script;
use delonix_storage::exec::Output;

use super::*;

fn me() -> u32 {
    // SAFETY: no preconditions.
    unsafe { libc::geteuid() }
}

fn owner() -> Owner {
    Owner::of(me(), "/home/w/.local/share/delonix")
}

fn other_owner() -> Owner {
    Owner::of(me(), "/srv/another-engine")
}

/// A scripted runner that also does on disk what `btrfs subvolume create`
/// and `delete` would.
struct Fs(Arc<Script>);

impl Run for Fs {
    fn run(&self, a: &[String]) -> std::io::Result<Output> {
        let out = self.0.run(a)?;
        if out.success() && a.len() == 4 && a[0] == "btrfs" && a[1] == "subvolume" {
            match a[2].as_str() {
                "create" => std::fs::create_dir(&a[3])?,
                "delete" => std::fs::remove_dir_all(&a[3])?,
                _ => {}
            }
        }
        Ok(out)
    }
}

fn btrfs(_: &Path) -> Option<i64> {
    Some(BTRFS_MAGIC)
}
fn ext4(_: &Path) -> Option<i64> {
    Some(0xEF53)
}
fn not_a_subvolume(_: &Path) -> bool {
    false
}
fn a_subvolume(_: &Path) -> bool {
    true
}
fn half_full(_: &Path) -> PoolUsage {
    PoolUsage {
        capacity_bytes: Some(100),
        used_bytes: Some(50),
        metadata_pct: None,
    }
}
fn full(_: &Path) -> PoolUsage {
    PoolUsage {
        capacity_bytes: Some(100),
        used_bytes: Some(95),
        metadata_pct: None,
    }
}

struct Setup {
    run: Arc<Script>,
    _tmp: tempfile::TempDir,
    root: PathBuf,
    entry: Entry,
}

impl Setup {
    fn driver(&self, qgroups: bool) -> BtrfsDriver {
        BtrfsDriver::new(Arc::new(Fs(self.run.clone())), None, qgroups)
            .with_magic(btrfs)
            .with_subvolume_check(not_a_subvolume)
            .with_fill(half_full)
    }
    fn pool(&self) -> PoolRef<'_> {
        PoolRef {
            name: "fast",
            entry: &self.entry,
        }
    }
}

fn setup() -> Setup {
    let run = Arc::new(Script::new());
    run.on(&["btrfs", "--version"], 0, "btrfs-progs v6.6.3\n", "")
        .on(&["btrfs", "subvolume", "create"], 0, "", "")
        .on(&["btrfs", "subvolume", "delete"], 0, "", "")
        .on(&["btrfs", "quota", "enable"], 0, "", "")
        .on(&["btrfs", "qgroup", "limit"], 0, "", "");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("pool");
    std::fs::create_dir(&root).unwrap();
    let entry = Entry {
        driver: ID.into(),
        path: Some(root.clone()),
        ..Default::default()
    };
    Setup {
        run,
        _tmp: tmp,
        root,
        entry,
    }
}

fn req(name: &str) -> VolumeRequest {
    VolumeRequest {
        name: name.into(),
        shape: VolumeShape::Filesystem,
        size_bytes: 5 << 30,
    }
}

fn s(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

#[test]
fn a_pool_that_is_not_a_mounted_btrfs_is_unavailable_and_says_so() {
    let st = setup();
    let p = st.driver(false).with_magic(ext4).probe(&st.pool());
    match p {
        PoolProbe::Unavailable { missing, .. } => {
            assert!(missing.contains("not a mounted btrfs"), "{missing}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(st.driver(false).probe(&st.pool()), PoolProbe::Available);
}

#[test]
fn a_missing_pool_directory_names_the_administrators_command() {
    let st = setup();
    std::fs::remove_dir(&st.root).unwrap();
    match st.driver(false).probe(&st.pool()) {
        PoolProbe::Unavailable { remedy, .. } => {
            assert!(remedy.contains("create-pool fast"), "{remedy}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn without_btrfs_progs_the_pool_is_unavailable() {
    let st = setup();
    st.run.missing(&["btrfs", "--version"]);
    match st.driver(false).probe(&st.pool()) {
        PoolProbe::Unavailable { missing, .. } => {
            assert!(missing.contains("btrfs-progs"), "{missing}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_new_volume_is_a_subvolume_stamped_and_handed_to_its_user() {
    let st = setup();
    let a = st
        .driver(false)
        .allocate(&st.pool(), &req("db"), &owner())
        .unwrap();
    let dir = st.root.join("db");
    assert_eq!(a.path, dir.join("_data"));
    assert!(!a.adopted);
    let creates = st.run.calls_of("btrfs");
    assert!(creates
        .iter()
        .any(|c| s(c) == ["btrfs", "subvolume", "create", dir.to_str().unwrap()]));
    // Rootless: no qgroup — it is an administrative ioctl.
    assert!(!creates
        .iter()
        .any(|c| c.get(1).map(String::as_str) == Some("qgroup")));
    let stamp = Stamp::decode(&std::fs::read(dir.join(STAMP_FILE)).unwrap()).unwrap();
    assert!(stamp.is_for("fast", "db", &owner()));
    assert_eq!(std::fs::metadata(&a.path).unwrap().uid(), me());
    // No temporary of the atomic write is left beside the stamp.
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left.len(), 2, "{left:?}");
}

#[test]
fn a_root_engine_also_sets_the_qgroup_limit_to_the_requested_size() {
    let st = setup();
    st.driver(true)
        .allocate(&st.pool(), &req("db"), &owner())
        .unwrap();
    let dir = st.root.join("db");
    let size = (5u64 << 30).to_string();
    assert!(st
        .run
        .calls_of("btrfs")
        .iter()
        .any(|c| s(c) == ["btrfs", "qgroup", "limit", &size, dir.to_str().unwrap()]));
}

#[test]
fn a_root_engine_enables_quota_on_the_pool_before_limiting_a_subvolume() {
    // Regression: a qgroup limit is refused ("quota not enabled") unless quota
    // accounting is on for the filesystem. A pool the administrator mounted by
    // hand has it off, so the driver must enable it (on the pool root) before
    // the limit — and in that order.
    let st = setup();
    st.driver(true)
        .allocate(&st.pool(), &req("db"), &owner())
        .unwrap();
    let calls = st.run.calls_of("btrfs");
    let enable = calls
        .iter()
        .position(|c| s(c) == ["btrfs", "quota", "enable", st.root.to_str().unwrap()])
        .expect("quota is enabled on the pool root");
    let limit = calls
        .iter()
        .position(|c| c.get(1).map(String::as_str) == Some("qgroup"))
        .expect("a qgroup limit is set");
    assert!(enable < limit, "quota must be enabled before the limit");
}

#[test]
fn a_volume_stamped_for_this_owner_is_taken_back_not_made_twice() {
    let st = setup();
    let d = st.driver(false);
    d.allocate(&st.pool(), &req("db"), &owner()).unwrap();
    let before = st.run.calls_of("btrfs").len();
    let a = d.allocate(&st.pool(), &req("db"), &owner()).unwrap();
    assert!(a.adopted);
    assert_eq!(
        st.run.calls_of("btrfs").len(),
        before,
        "nothing new was run"
    );
}

#[test]
fn a_volume_of_another_engine_is_refused_and_left_alone() {
    let st = setup();
    let d = st.driver(false);
    d.allocate(&st.pool(), &req("db"), &other_owner()).unwrap();
    std::fs::write(st.root.join("db/_data/keep"), b"x").unwrap();
    let e = d.allocate(&st.pool(), &req("db"), &owner()).unwrap_err();
    assert!(matches!(e, Error::PoolConflict(_)), "{e}");
    let e = d.release(&st.pool(), "db", &owner()).unwrap_err();
    assert!(matches!(e, Error::PoolConflict(_)), "{e}");
    assert!(st.root.join("db/_data/keep").exists());
}

#[test]
fn an_unstamped_directory_is_never_adopted_by_its_name() {
    let st = setup();
    std::fs::create_dir(st.root.join("db")).unwrap();
    // Empty, but not a subvolume: the driver did not make it.
    let e = st
        .driver(false)
        .allocate(&st.pool(), &req("db"), &owner())
        .unwrap_err();
    assert!(matches!(e, Error::PoolConflict(_)), "{e}");
    // A subvolume with something in it: someone's data.
    std::fs::write(st.root.join("db/x"), b"x").unwrap();
    let e = st
        .driver(false)
        .with_subvolume_check(a_subvolume)
        .allocate(&st.pool(), &req("db"), &owner())
        .unwrap_err();
    assert!(matches!(e, Error::PoolConflict(_)), "{e}");
    assert!(st.root.join("db/x").exists());
}

#[test]
fn an_empty_subvolume_left_by_a_dead_apply_is_stamped_and_used() {
    let st = setup();
    std::fs::create_dir(st.root.join("db")).unwrap();
    let a = st
        .driver(false)
        .with_subvolume_check(a_subvolume)
        .allocate(&st.pool(), &req("db"), &owner())
        .unwrap();
    assert!(!a.adopted);
    assert!(st.root.join("db").join(STAMP_FILE).exists());
    assert!(st.run.calls_of("btrfs").iter().all(|c| c[1] != "subvolume"));
}

#[test]
fn a_filesystem_at_95_percent_takes_no_new_volume() {
    let st = setup();
    let e = st
        .driver(false)
        .with_fill(full)
        .allocate(&st.pool(), &req("db"), &owner())
        .unwrap_err();
    assert!(matches!(e, Error::PoolExhausted(_)), "{e}");
    assert!(!st.root.join("db").exists());
    assert!(st.run.calls_of("btrfs").is_empty());
}

#[test]
fn a_block_volume_and_a_path_like_name_are_refused_before_anything_runs() {
    let st = setup();
    let mut r = req("db");
    r.shape = VolumeShape::Block;
    assert!(st.driver(false).allocate(&st.pool(), &r, &owner()).is_err());
    for bad in ["../x", "a/b", "-o", ".delonix-pool.json"] {
        assert!(
            st.driver(false)
                .allocate(&st.pool(), &req(bad), &owner())
                .is_err(),
            "{bad}"
        );
        assert!(st.driver(false).release(&st.pool(), bad, &owner()).is_err());
    }
    assert!(st.run.calls().is_empty());
}

#[test]
fn a_release_removes_the_data_then_the_stamp_then_the_subvolume() {
    let st = setup();
    let d = st.driver(false);
    d.allocate(&st.pool(), &req("db"), &owner()).unwrap();
    std::fs::create_dir_all(st.root.join("db/_data/a/b")).unwrap();
    std::fs::write(st.root.join("db/_data/a/b/f"), b"x").unwrap();
    d.release(&st.pool(), "db", &owner()).unwrap();
    assert!(!st.root.join("db").exists());
    // The empty subvolume went by `rmdir`: no `btrfs subvolume delete`.
    assert!(!st
        .run
        .calls_of("btrfs")
        .iter()
        .any(|c| c.get(2).map(String::as_str) == Some("delete")));
    // Already gone is Ok.
    d.release(&st.pool(), "db", &owner()).unwrap();
}

#[test]
fn the_devices_of_a_filesystem_are_read_from_btrfs_filesystem_show() {
    let out = "Label: 'dlx-fast'  uuid: 3a5e0c1d-8a4f-4e0b-9d2f-7c1a2b3c4d5e\n\
               \tTotal devices 2 FS bytes used 114688\n\
               \tdevid    1 size 10737418240 used 2172649472 path /dev/vdb\n\
               \tdevid    2 size 10737418240 used 2172649472 path /dev/vdc\n\n";
    assert_eq!(
        parse_show_devices(out),
        vec![PathBuf::from("/dev/vdb"), PathBuf::from("/dev/vdc")]
    );
    assert!(parse_show_devices("").is_empty());
}

fn admin_entry(root: &Path) -> Entry {
    Entry {
        driver: ID.into(),
        path: Some(root.to_path_buf()),
        create: Some(Create {
            devices: vec!["/dev/vdb".into()],
        }),
        ..Default::default()
    }
}

#[test]
fn create_pool_makes_mounts_and_marks_the_filesystem() {
    let st = setup();
    let root = st.root.join("new");
    let entry = admin_entry(&root);
    let pool = PoolRef {
        name: "fast",
        entry: &entry,
    };
    st.run
        .on(&["mkfs.btrfs"], 0, "", "")
        .on(&["mount"], 0, "", "")
        .on(&["blkid"], 0, "3a5e0c1d\n", "");
    let d = st.driver(false);
    let msg = d.create_pool(&pool, &[PathBuf::from("/dev/vdb")]).unwrap();
    assert_eq!(
        s(&st.run.calls_of("mkfs.btrfs")[0]),
        ["mkfs.btrfs", "-q", "-L", "dlx-fast", "/dev/vdb"]
    );
    assert_eq!(
        s(&st.run.calls_of("mount")[0]),
        [
            "mount",
            "-t",
            "btrfs",
            "-o",
            "user_subvol_rm_allowed,nodev,nosuid",
            "--",
            "/dev/vdb",
            root.to_str().unwrap()
        ]
    );
    assert!(msg.contains("UUID=3a5e0c1d"), "{msg}");
    assert!(d.created_here(&pool).unwrap());
    assert!(
        d.contents(&pool).unwrap().is_empty(),
        "the mark is not content"
    );
}

#[test]
fn create_pool_refuses_a_mount_directory_with_something_in_it() {
    let st = setup();
    std::fs::write(st.root.join("x"), b"x").unwrap();
    let entry = admin_entry(&st.root);
    let pool = PoolRef {
        name: "fast",
        entry: &entry,
    };
    let e = st
        .driver(false)
        .create_pool(&pool, &[PathBuf::from("/dev/vdb")])
        .unwrap_err();
    assert!(matches!(e, Error::DeviceInUse(_)), "{e}");
    assert!(st.run.calls().is_empty());
}

#[test]
fn a_filesystem_this_helper_did_not_mark_is_never_destroyed() {
    let st = setup();
    let entry = admin_entry(&st.root);
    let pool = PoolRef {
        name: "fast",
        entry: &entry,
    };
    let d = st.driver(false);
    assert!(!d.created_here(&pool).unwrap());
    // Another pool's mark is not this pool's.
    std::fs::write(
        st.root.join(POOL_MARK),
        br#"{"engine":"delonix","pool":"other"}"#,
    )
    .unwrap();
    assert!(!d.created_here(&pool).unwrap());
    let e = destroy_guarded(&d, &pool).unwrap_err();
    assert!(matches!(e, Error::PoolNotCreatedHere(_)), "{e}");
    assert!(st.run.calls().is_empty());
}

#[test]
fn destroy_pool_reads_the_devices_first_then_unmounts_and_wipes_them() {
    let st = setup();
    let entry = admin_entry(&st.root);
    let pool = PoolRef {
        name: "fast",
        entry: &entry,
    };
    std::fs::write(
        st.root.join(POOL_MARK),
        br#"{"engine":"delonix","pool":"fast"}"#,
    )
    .unwrap();
    st.run
        .on(
            &["btrfs", "filesystem", "show"],
            0,
            "\tdevid    1 size 10737418240 used 0 path /dev/vdb\n",
            "",
        )
        .on(&["umount"], 0, "", "")
        .on(&["wipefs"], 0, "", "");
    let d = st.driver(false);
    // A volume in it: refused, nothing run.
    d.allocate(&st.pool(), &req("db"), &owner()).unwrap();
    let e = destroy_guarded(&d, &pool).unwrap_err();
    assert!(matches!(e, Error::PoolConflict(_)), "{e}");
    assert!(st.run.calls_of("umount").is_empty());
    d.release(&st.pool(), "db", &owner()).unwrap();
    destroy_guarded(&d, &pool).unwrap();
    let progs: Vec<String> = st.run.calls().iter().map(|c| c[0].clone()).collect();
    let i_show = st
        .run
        .calls()
        .iter()
        .position(|c| c.get(1).map(String::as_str) == Some("filesystem"))
        .unwrap();
    let i_umount = progs.iter().position(|p| p == "umount").unwrap();
    let i_wipe = progs.iter().position(|p| p == "wipefs").unwrap();
    assert!(i_show < i_umount && i_umount < i_wipe, "{progs:?}");
    assert_eq!(
        s(&st.run.calls_of("wipefs")[0]),
        ["wipefs", "--all", "--", "/dev/vdb"]
    );
}
