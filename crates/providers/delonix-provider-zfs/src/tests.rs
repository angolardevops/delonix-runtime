//! The driver proven against a scripted runner. The samples follow the
//! scripted-mode layout of `zfs-list(8)` and `zfs-get(8)` (`-H`: one line per
//! row, TAB between fields; `-p`: exact numbers; an unset user property reads
//! `-`) and OpenZFS's own error wording («dataset does not exist», the
//! `libzfs_error_init` messages). They were NOT captured from a live ZFS here:
//! this host has no `zfsutils-linux` (ADR-0067, measured).

use std::os::unix::fs::MetadataExt;

use delonix_storage::allowlist::{Create, Entry};
use delonix_storage::exec::testing::Script;

use super::*;

const KEYS: &str =
    "delonix.io:engine,delonix.io:owner,delonix.io:pool,delonix.io:volume,mountpoint,mounted";

fn get(ds: &str) -> Vec<&str> {
    vec!["zfs", "get", "-Hp", "-o", "property,value", KEYS, ds]
}

const NO_DS: &str = "cannot open 'tank/dlx/db': dataset does not exist\n";
const NOT_ROOT: &str = "Permission denied the ZFS utilities must be run as root.\n";

fn me() -> u32 {
    // SAFETY: no preconditions.
    unsafe { libc::geteuid() }
}

fn owner() -> Owner {
    Owner::of(me(), "/home/w/.local/share/delonix")
}

fn entry() -> Entry {
    Entry {
        driver: ID.into(),
        dataset: Some("tank/dlx".into()),
        ..Default::default()
    }
}

fn pool(e: &Entry) -> PoolRef<'_> {
    PoolRef {
        name: "tank",
        entry: e,
    }
}

struct Setup {
    run: Arc<Script>,
    driver: ZfsDriver,
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

fn setup() -> Setup {
    let run = Arc::new(Script::new());
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    Setup {
        driver: ZfsDriver::new(run.clone(), root.clone()),
        run,
        _tmp: tmp,
        root,
    }
}

fn props(owner: &Owner, mp: &str, mounted: &str) -> String {
    format!(
        "delonix.io:engine\tdelonix\ndelonix.io:owner\t{}\ndelonix.io:pool\ttank\ndelonix.io:volume\tdb\nmountpoint\t{mp}\nmounted\t{mounted}\n",
        owner.id()
    )
}

fn req(size: u64) -> VolumeRequest {
    VolumeRequest {
        name: "db".into(),
        shape: VolumeShape::Filesystem,
        size_bytes: size,
    }
}

#[test]
fn without_a_readable_dev_zfs_the_answer_is_could_not_determine() {
    let out = Output {
        code: Some(1),
        stdout: String::new(),
        stderr: NOT_ROOT.into(),
    };
    // The reason names the cause, so the remedy fits it.
    assert!(
        matches!(classify(&out), Answer::Undetermined(ref r) if r.contains("/dev/zfs")),
        "{:?}",
        classify(&out)
    );
    let s = setup();
    s.run.on(&["zfs", "list"], 1, "", NOT_ROOT);
    let e = entry();
    assert!(matches!(
        s.driver.probe(&pool(&e)),
        PoolProbe::Undetermined { .. }
    ));
    // An empty, successful answer is not a pool either.
    let empty = Output {
        code: Some(0),
        ..Output::default()
    };
    assert!(matches!(classify(&empty), Answer::Undetermined(_)));
}

#[test]
fn the_probe_tells_a_dataset_from_nothing_and_from_no_module() {
    let e = entry();
    let s = setup();
    s.run.on(&["zfs", "list"], 0, "tank/dlx\n", "");
    assert_eq!(s.driver.probe(&pool(&e)), PoolProbe::Available);

    let s = setup();
    s.run.on(
        &["zfs", "list"],
        1,
        "",
        "cannot open 'tank/dlx': dataset does not exist\n",
    );
    assert!(matches!(
        s.driver.probe(&pool(&e)),
        PoolProbe::Unavailable { ref remedy, .. } if remedy.contains("create-pool tank")
    ));

    let s = setup();
    s.run.on(
        &["zfs", "list"],
        1,
        "",
        "The ZFS modules are not loaded.\nTry running '/sbin/modprobe zfs' as root to load them.\n",
    );
    assert!(matches!(
        s.driver.probe(&pool(&e)),
        PoolProbe::Unavailable { ref missing, .. } if missing.contains("module")
    ));

    let s = setup();
    s.run.missing(&["zfs"]);
    assert!(matches!(
        s.driver.probe(&pool(&e)),
        PoolProbe::Unavailable { ref missing, .. } if missing.contains("zfsutils-linux")
    ));
}

#[test]
fn reading_is_delegated_and_writing_needs_the_helper() {
    let s = setup();
    assert!(matches!(
        s.driver.required(PoolOp::Probe),
        Privilege::Delegated(_)
    ));
    assert!(matches!(
        s.driver.required(PoolOp::Usage),
        Privilege::Delegated(_)
    ));
    assert_eq!(
        s.driver.required(PoolOp::Allocate(VolumeShape::Filesystem)),
        Privilege::Helper
    );
    assert_eq!(s.driver.required(PoolOp::Release), Privilege::Helper);
}

#[test]
fn a_new_volume_is_a_stamped_dataset_with_refquota_mounted_under_the_helper_root() {
    let s = setup();
    let mp = s.root.join("tank").join("db");
    let mp_s = mp.to_string_lossy().into_owned();
    std::fs::create_dir_all(&mp).unwrap();
    s.run
        .on(&get("tank/dlx/db"), 1, "", NO_DS)
        .on(
            &["zfs", "list", "-Hp", "-o", "used,avail", "tank/dlx"],
            0,
            "1000\t9000\n",
            "",
        )
        .on(&["zfs", "create"], 0, "", "");
    let e = entry();
    let after = props(&owner(), &mp_s, "yes");
    // The same `get` answers «does not exist» before the create and the new
    // dataset after it: a runner that wraps this setup's script.
    struct Seq {
        script: Arc<Script>,
        after: String,
    }
    impl Run for Seq {
        fn run(&self, a: &[String]) -> std::io::Result<Output> {
            let made = self
                .script
                .calls_of("zfs")
                .iter()
                .any(|c| c.get(1).map(String::as_str) == Some("create"));
            if made && a.get(1).map(String::as_str) == Some("get") {
                self.script.calls.lock().unwrap().push(a.to_vec());
                return Ok(Output {
                    code: Some(0),
                    stdout: self.after.clone(),
                    stderr: String::new(),
                });
            }
            self.script.run(a)
        }
    }
    let seq = Arc::new(Seq {
        script: s.run.clone(),
        after,
    });
    let d = ZfsDriver::new(seq, s.root.clone());
    let a = d.allocate(&pool(&e), &req(5 << 30), &owner()).unwrap();
    assert_eq!(a.path, mp.join("_data"));
    assert!(!a.adopted);
    let create = s
        .run
        .calls_of("zfs")
        .into_iter()
        .find(|c| c[1] == "create")
        .unwrap();
    assert_eq!(
        create,
        argv(&[
            "zfs",
            "create",
            "-o",
            &format!("mountpoint={mp_s}"),
            "-o",
            "refquota=5368709120",
            "-o",
            "canmount=on",
            "-o",
            "delonix.io:engine=delonix",
            "-o",
            &format!("delonix.io:owner={}", owner().id()),
            "-o",
            "delonix.io:pool=tank",
            "-o",
            "delonix.io:volume=db",
            "tank/dlx/db",
        ])
    );
    // Already mounted (`mounted yes`): no `zfs mount`.
    assert!(!s.run.calls_of("zfs").iter().any(|c| c[1] == "mount"));
    assert_eq!(std::fs::metadata(&a.path).unwrap().uid(), me());
}

#[test]
fn a_dataset_stamped_for_this_owner_is_taken_back_and_mounted_if_it_is_not() {
    let s = setup();
    let mp = s.root.join("tank").join("db");
    std::fs::create_dir_all(&mp).unwrap();
    s.run
        .on(
            &get("tank/dlx/db"),
            0,
            &props(&owner(), &mp.to_string_lossy(), "no"),
            "",
        )
        .on(&["zfs", "mount", "tank/dlx/db"], 0, "", "");
    let e = entry();
    let a = s
        .driver
        .allocate(&pool(&e), &req(1 << 30), &owner())
        .unwrap();
    assert!(a.adopted);
    assert!(!s.run.calls_of("zfs").iter().any(|c| c[1] == "create"));
    assert_eq!(
        s.run
            .calls_of("zfs")
            .iter()
            .filter(|c| c[1] == "mount")
            .count(),
        1
    );
}

#[test]
fn a_dataset_of_another_owner_is_never_adopted_nor_released() {
    let s = setup();
    let theirs = Owner::of(me() + 1, "/home/w/.local/share/delonix");
    s.run.on(
        &get("tank/dlx/db"),
        0,
        &props(&theirs, &s.root.join("tank/db").to_string_lossy(), "yes"),
        "",
    );
    let e = entry();
    assert_eq!(
        s.driver
            .allocate(&pool(&e), &req(1 << 30), &owner())
            .unwrap_err()
            .number(),
        5201
    );
    assert_eq!(
        s.driver
            .release(&pool(&e), "db", &owner())
            .unwrap_err()
            .number(),
        5201
    );
    assert!(!s
        .run
        .calls_of("zfs")
        .iter()
        .any(|c| c[1] == "create" || c[1] == "destroy"));
}

#[test]
fn a_stamped_dataset_mounted_outside_the_helper_root_is_refused() {
    let s = setup();
    s.run
        .on(&get("tank/dlx/db"), 0, &props(&owner(), "/etc", "yes"), "");
    let e = entry();
    let err = s.driver.allocate(&pool(&e), &req(1), &owner()).unwrap_err();
    assert_eq!(err.number(), 5201, "{err}");
}

#[test]
fn a_parent_at_95_percent_takes_no_new_volume() {
    let s = setup();
    s.run.on(&get("tank/dlx/db"), 1, "", NO_DS).on(
        &["zfs", "list", "-Hp", "-o", "used,avail", "tank/dlx"],
        0,
        "9500\t500\n",
        "",
    );
    let e = entry();
    assert_eq!(
        s.driver
            .allocate(&pool(&e), &req(1), &owner())
            .unwrap_err()
            .number(),
        5202
    );
    assert!(!s.run.calls_of("zfs").iter().any(|c| c[1] == "create"));
}

#[test]
fn release_destroys_without_recursion_and_only_its_own() {
    let s = setup();
    s.run
        .on(
            &get("tank/dlx/db"),
            0,
            &props(&owner(), &s.root.join("tank/db").to_string_lossy(), "yes"),
            "",
        )
        .on(&["zfs", "destroy"], 0, "", "");
    let e = entry();
    s.driver.release(&pool(&e), "db", &owner()).unwrap();
    assert_eq!(
        s.run
            .calls_of("zfs")
            .into_iter()
            .filter(|c| c[1] == "destroy")
            .collect::<Vec<_>>(),
        vec![argv(&["zfs", "destroy", "tank/dlx/db"])]
    );
    // Gone already: fine.
    let s = setup();
    s.run.on(&get("tank/dlx/db"), 1, "", NO_DS);
    s.driver.release(&pool(&e), "db", &owner()).unwrap();
}

#[test]
fn usage_is_used_over_used_plus_available() {
    let s = setup();
    s.run.on(
        &["zfs", "list", "-Hp", "-o", "used,avail", "tank/dlx"],
        0,
        "2500\t7500\n",
        "",
    );
    let e = entry();
    let u = s.driver.usage(&pool(&e));
    assert_eq!(u.capacity_bytes, Some(10000));
    assert_eq!(u.used_pct(), Some(25));
}

fn admin_entry() -> Entry {
    Entry {
        create: Some(Create {
            devices: vec![PathBuf::from("/dev/disk/by-id/a")],
        }),
        ..entry()
    }
}

#[test]
fn create_pool_makes_a_marked_zpool_and_dataset_and_refuses_an_existing_zpool() {
    let s = setup();
    s.run
        .on(
            &["zpool", "list"],
            1,
            "",
            "cannot open 'tank': no such pool\n",
        )
        .on(&["zpool", "create"], 0, "", "")
        .on(&["zfs", "create"], 0, "", "");
    let e = admin_entry();
    s.driver
        .create_pool(&pool(&e), &[PathBuf::from("/dev/vdb")])
        .unwrap();
    assert_eq!(
        s.run.calls_of("zpool")[1],
        argv(&[
            "zpool",
            "create",
            "-O",
            "mountpoint=none",
            "-O",
            "delonix.io:created=yes",
            "tank",
            "/dev/vdb",
        ])
    );
    assert_eq!(
        s.run.calls_of("zfs"),
        vec![argv(&[
            "zfs",
            "create",
            "-o",
            "delonix.io:created=yes",
            "tank/dlx"
        ])]
    );

    let s = setup();
    s.run.on(&["zpool", "list"], 0, "tank\n", "");
    assert_eq!(
        s.driver
            .create_pool(&pool(&e), &[PathBuf::from("/dev/vdb")])
            .unwrap_err()
            .number(),
        5205
    );
    assert_eq!(s.run.calls_of("zpool").len(), 1);
}

#[test]
fn the_guarded_destroy_refuses_a_zpool_holding_anything_beyond_the_pool() {
    let s = setup();
    s.run
        .on(&["zfs", "get", "-Hp", "-o", "value"], 0, "yes\n", "")
        .on(
            &["zfs", "list", "-H", "-o", "name", "-t", "all", "-r", "tank"],
            0,
            "tank\ntank/dlx\ntank/dlx/db\ntank/other\n",
            "",
        )
        .on(&["zpool", "destroy"], 0, "", "");
    let e = admin_entry();
    let err = delonix_storage::admin::destroy_guarded(&s.driver, &pool(&e)).unwrap_err();
    assert_eq!(err.number(), 5201);
    assert!(err.to_string().contains("tank/dlx/db, tank/other"), "{err}");
    assert!(s.run.calls_of("zpool").is_empty());

    // Unmarked: never destroyed.
    let s = setup();
    s.run
        .on(&["zfs", "get", "-Hp", "-o", "value"], 0, "-\n", "");
    assert_eq!(
        delonix_storage::admin::destroy_guarded(&s.driver, &pool(&e))
            .unwrap_err()
            .number(),
        5204
    );

    // Marked and empty: the zpool goes.
    let s = setup();
    s.run
        .on(&["zfs", "get", "-Hp", "-o", "value"], 0, "yes\n", "")
        .on(&["zfs", "list"], 0, "tank\ntank/dlx\n", "")
        .on(&["zpool", "destroy"], 0, "", "");
    delonix_storage::admin::destroy_guarded(&s.driver, &pool(&e)).unwrap();
    assert_eq!(
        s.run.calls_of("zpool"),
        vec![argv(&["zpool", "destroy", "tank"])]
    );
}
