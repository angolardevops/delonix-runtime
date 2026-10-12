//! The driver proven against a scripted runner: the argv it would run, and
//! how it reads each answer. The samples follow the JSON layout of
//! `lvmreport(7)` (`--reportformat json --units b --nosuffix`) and the error
//! wording of `lvs(8)`/`vgs(8)`; they were NOT captured from a live LVM here
//! (no root on this host). The one exception is the unprivileged answer,
//! measured on this host and quoted in ADR-0067 («What the tested environment
//! allows without root»).

use std::os::unix::fs::MetadataExt;

use delonix_storage::allowlist::{Create, Entry};
use delonix_storage::exec::testing::Script;

use super::*;

const FIELDS: &str = "lv_name,lv_attr,lv_size,data_percent,metadata_percent,lv_tags";

/// The unprivileged `lvs`, as measured in ADR-0067: a warning, nothing
/// listed, exit 0.
const NON_ROOT_STDERR: &str = "  WARNING: Running as a non-root user. Functionality may be \
     unavailable.\n  /run/lock/lvm/P_global:aux: open failed: Permission denied\n";

fn lvs(target: &str) -> Vec<&str> {
    vec![
        "lvs",
        "--reportformat",
        "json",
        "--units",
        "b",
        "--nosuffix",
        "-o",
        FIELDS,
        target,
    ]
}

fn thin_row(data: &str, meta: &str) -> String {
    format!(
        r#"{{"report":[{{"lv":[{{"lv_name":"dlx","lv_attr":"twi-aotz--","lv_size":"10737418240","data_percent":"{data}","metadata_percent":"{meta}","lv_tags":"delonix.io_created"}}]}}]}}"#
    )
}

fn lv_row(name: &str, tags: &[String]) -> String {
    format!(
        r#"{{"report":[{{"lv":[{{"lv_name":"{name}","lv_attr":"Vwi-aotz--","lv_size":"1073741824","data_percent":"0.00","metadata_percent":"","lv_tags":"{}"}}]}}]}}"#,
        tags.join(",")
    )
}

const MISSING_LV: &str = "  Failed to find logical volume \"vg0/dlx-db\"\n";

fn me() -> u32 {
    // SAFETY: no preconditions.
    unsafe { libc::geteuid() }
}

fn entry() -> Entry {
    Entry {
        driver: ID.into(),
        vg: Some("vg0".into()),
        thin_pool: Some("dlx".into()),
        ..Default::default()
    }
}

struct Setup {
    run: Arc<Script>,
    driver: LvmThinDriver,
    _root: tempfile::TempDir,
    root: PathBuf,
}

fn setup() -> Setup {
    let run = Arc::new(Script::new());
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    Setup {
        driver: LvmThinDriver::new(run.clone(), root.clone()),
        run,
        _root: tmp,
        root,
    }
}

fn pool(e: &Entry) -> PoolRef<'_> {
    PoolRef {
        name: "fast",
        entry: e,
    }
}

fn fs_req(size: u64) -> VolumeRequest {
    VolumeRequest {
        name: "db".into(),
        shape: VolumeShape::Filesystem,
        size_bytes: size,
    }
}

fn owner() -> Owner {
    Owner::of(me(), "/home/w/.local/share/delonix")
}

/// THE gate ADR-0067 D3 names: the unprivileged warning-plus-empty answer,
/// exit 0, is never read as a pool — nor as «no pool».
#[test]
fn an_unprivileged_lvs_that_exits_0_with_nothing_is_could_not_determine() {
    let out = Output {
        code: Some(0),
        stdout: String::new(),
        stderr: NON_ROOT_STDERR.into(),
    };
    assert!(matches!(classify(&out), Answer::Undetermined(_)));
    // Even with an empty JSON report on stdout.
    let out = Output {
        stdout: r#"{"report":[{"lv":[]}]}"#.into(),
        ..out
    };
    assert!(matches!(classify(&out), Answer::Undetermined(_)));
    // And when the unprivileged run lists part of what it could read: a
    // partial listing is no more a pool than an empty one.
    let out = Output {
        stdout: thin_row("1.00", "1.00"),
        ..out
    };
    assert!(matches!(classify(&out), Answer::Undetermined(_)));

    let s = setup();
    s.run.on(
        &lvs("vg0/dlx"),
        0,
        r#"{"report":[{"lv":[]}]}"#,
        NON_ROOT_STDERR,
    );
    let e = entry();
    let p = s.driver.probe(&pool(&e));
    assert!(
        matches!(p, PoolProbe::Undetermined { ref reason, .. } if reason.contains("needs root")),
        "{p:?}"
    );
    assert_ne!(p, PoolProbe::Available);
}

#[test]
fn a_successful_but_empty_report_is_never_a_pool() {
    let out = Output {
        code: Some(0),
        stdout: r#"{"report":[{"lv":[]}]}"#.into(),
        stderr: String::new(),
    };
    assert!(matches!(classify(&out), Answer::Undetermined(_)));
    let garbage = Output {
        code: Some(0),
        stdout: "not json".into(),
        stderr: String::new(),
    };
    assert!(matches!(classify(&garbage), Answer::Undetermined(_)));
}

#[test]
fn the_probe_tells_a_thin_pool_from_a_plain_volume_and_from_nothing() {
    let e = entry();
    let s = setup();
    s.run.on(&lvs("vg0/dlx"), 0, &thin_row("1.00", "2.00"), "");
    assert_eq!(s.driver.probe(&pool(&e)), PoolProbe::Available);

    let s = setup();
    s.run.on(&lvs("vg0/dlx"), 0, &lv_row("dlx", &[]), "");
    assert!(matches!(
        s.driver.probe(&pool(&e)),
        PoolProbe::Unavailable { ref missing, .. } if missing.contains("not a thin pool")
    ));

    let s = setup();
    s.run.on(
        &lvs("vg0/dlx"),
        5,
        r#"{"report":[{"lv":[]}]}"#,
        "  Volume group \"vg0\" not found\n",
    );
    assert!(matches!(
        s.driver.probe(&pool(&e)),
        PoolProbe::Unavailable { ref remedy, .. } if remedy.contains("create-pool fast")
    ));

    let s = setup();
    s.run.missing(&["lvs"]);
    assert!(matches!(
        s.driver.probe(&pool(&e)),
        PoolProbe::Unavailable { ref missing, .. } if missing.contains("lvm2")
    ));
}

#[test]
fn usage_reads_data_and_metadata_of_the_thin_pool() {
    let s = setup();
    s.run.on(&lvs("vg0/dlx"), 0, &thin_row("12.50", "3.02"), "");
    let e = entry();
    let u = s.driver.usage(&pool(&e));
    assert_eq!(u.capacity_bytes, Some(10737418240));
    assert_eq!(u.used_bytes, Some(1342177280));
    assert_eq!(u.metadata_pct, Some(4));
}

#[test]
fn a_new_volume_is_a_stamped_thin_lv_with_ext4_mounted_under_the_helper_root() {
    let s = setup();
    s.run
        .on(
            &lvs("vg0/dlx-db"),
            5,
            r#"{"report":[{"lv":[]}]}"#,
            MISSING_LV,
        )
        .on(&lvs("vg0/dlx"), 0, &thin_row("12.50", "3.02"), "")
        .on(&["lvcreate"], 0, "", "")
        .on(&["mkfs.ext4"], 0, "", "")
        .on(&["findmnt"], 1, "", "")
        .on(&["mount"], 0, "", "");
    let e = entry();
    let a = s
        .driver
        .allocate(&pool(&e), &fs_req(1 << 30), &owner())
        .unwrap();
    let mp = s.root.join("fast").join("db");
    assert_eq!(a.path, mp.join("_data"));
    assert!(!a.adopted);
    let tags = stamp_tags("fast", "db", &owner()).unwrap();
    let mut want = argv(&[
        "lvcreate",
        "--yes",
        "--type",
        "thin",
        "--thinpool",
        "dlx",
        "--virtualsize",
        "1024m",
        "--name",
        "dlx-db",
    ]);
    for t in &tags {
        want.push("--addtag".into());
        want.push(t.clone());
    }
    want.push("vg0".into());
    assert_eq!(s.run.calls_of("lvcreate"), vec![want]);
    assert_eq!(
        s.run.calls_of("mkfs.ext4"),
        vec![argv(&[
            "mkfs.ext4",
            "-q",
            "-m",
            "0",
            "-L",
            "dlx",
            "/dev/vg0/dlx-db"
        ])]
    );
    assert_eq!(
        s.run.calls_of("mount"),
        vec![argv(&[
            "mount",
            "-t",
            "ext4",
            "-o",
            "nodev,nosuid",
            "--",
            "/dev/vg0/dlx-db",
            &mp.to_string_lossy(),
        ])]
    );
    // The data directory belongs to the volume's user, not to root.
    assert_eq!(std::fs::metadata(&a.path).unwrap().uid(), me());
    // The uid is stamped in clear, the root as a digest.
    assert!(tags.contains(&format!("delonix.io_uid={}", me())));
    assert!(tags
        .iter()
        .all(|t| !t.contains('/') || t.starts_with("delonix.io_root")));
}

#[test]
fn a_size_rounds_up_to_whole_mib_never_down() {
    let s = setup();
    s.run
        .on(&lvs("vg0/dlx-db"), 5, "", MISSING_LV)
        .on(&lvs("vg0/dlx"), 0, &thin_row("0", "0"), "")
        .on(&["lvcreate"], 0, "", "")
        .on(&["mkfs.ext4"], 0, "", "")
        .on(&["findmnt"], 1, "", "")
        .on(&["mount"], 0, "", "");
    let e = entry();
    s.driver
        .allocate(&pool(&e), &fs_req((1 << 20) + 1), &owner())
        .unwrap();
    assert!(s.run.calls_of("lvcreate")[0].contains(&"2m".to_string()));
}

#[test]
fn a_thin_pool_at_95_percent_data_or_metadata_takes_no_new_volume() {
    for (data, meta) in [("95.00", "1.00"), ("1.00", "96.50")] {
        let s = setup();
        s.run
            .on(&lvs("vg0/dlx-db"), 5, "", MISSING_LV)
            .on(&lvs("vg0/dlx"), 0, &thin_row(data, meta), "")
            .on(&["lvcreate"], 0, "", "");
        let e = entry();
        let err = s
            .driver
            .allocate(&pool(&e), &fs_req(1 << 30), &owner())
            .unwrap_err();
        assert_eq!(err.number(), 5202, "{data}/{meta}: {err}");
        assert!(s.run.calls_of("lvcreate").is_empty());
    }
}

#[test]
fn a_volume_stamped_for_this_owner_is_taken_back_without_a_new_filesystem() {
    let s = setup();
    let tags = stamp_tags("fast", "db", &owner()).unwrap();
    s.run
        .on(&lvs("vg0/dlx-db"), 0, &lv_row("dlx-db", &tags), "")
        .on(&["blkid"], 0, "TYPE=ext4\n", "")
        .on(&["findmnt"], 0, "/x\n", "");
    let e = entry();
    let a = s
        .driver
        .allocate(&pool(&e), &fs_req(1 << 30), &owner())
        .unwrap();
    assert!(a.adopted);
    assert!(s.run.calls_of("lvcreate").is_empty());
    assert!(s.run.calls_of("mkfs.ext4").is_empty());
    assert!(s.run.calls_of("mount").is_empty());
}

#[test]
fn a_stamped_volume_left_without_a_filesystem_gets_one_on_adoption() {
    let s = setup();
    let tags = stamp_tags("fast", "db", &owner()).unwrap();
    s.run
        .on(&lvs("vg0/dlx-db"), 0, &lv_row("dlx-db", &tags), "")
        .on(&["blkid"], 2, "", "")
        .on(&["mkfs.ext4"], 0, "", "")
        .on(&["findmnt"], 1, "", "")
        .on(&["mount"], 0, "", "");
    let e = entry();
    assert!(
        s.driver
            .allocate(&pool(&e), &fs_req(1 << 30), &owner())
            .unwrap()
            .adopted
    );
    assert_eq!(s.run.calls_of("mkfs.ext4").len(), 1);
}

#[test]
fn a_volume_of_another_uid_or_root_or_unstamped_is_never_adopted() {
    for tags in [
        stamp_tags(
            "fast",
            "db",
            &Owner::of(me() + 1, "/home/w/.local/share/delonix"),
        )
        .unwrap(),
        stamp_tags("fast", "db", &Owner::of(me(), "/tmp/another-root")).unwrap(),
        vec![],
    ] {
        let s = setup();
        s.run
            .on(&lvs("vg0/dlx-db"), 0, &lv_row("dlx-db", &tags), "");
        let e = entry();
        let err = s
            .driver
            .allocate(&pool(&e), &fs_req(1 << 30), &owner())
            .unwrap_err();
        assert_eq!(err.number(), 5201, "{err}");
        assert!(s.run.calls_of("lvcreate").is_empty());
        assert!(s.run.calls_of("mkfs.ext4").is_empty());
    }
}

#[test]
fn a_volume_whose_filesystem_fails_is_removed_rather_than_left_stamped() {
    let s = setup();
    s.run
        .on(&lvs("vg0/dlx-db"), 5, "", MISSING_LV)
        .on(&lvs("vg0/dlx"), 0, &thin_row("0", "0"), "")
        .on(&["lvcreate"], 0, "", "")
        .on(
            &["mkfs.ext4"],
            1,
            "",
            "mkfs.ext4: Device size reported to be zero",
        )
        .on(&["lvremove"], 0, "", "");
    let e = entry();
    let err = s
        .driver
        .allocate(&pool(&e), &fs_req(1 << 30), &owner())
        .unwrap_err();
    assert!(err.to_string().contains("Device size"), "{err}");
    assert_eq!(
        s.run.calls_of("lvremove"),
        vec![argv(&["lvremove", "--yes", "vg0/dlx-db"])]
    );
}

#[test]
fn release_unmounts_then_removes_and_only_its_own_volume() {
    let s = setup();
    let tags = stamp_tags("fast", "db", &owner()).unwrap();
    let mp = s.root.join("fast").join("db");
    std::fs::create_dir_all(&mp).unwrap();
    s.run
        .on(&lvs("vg0/dlx-db"), 0, &lv_row("dlx-db", &tags), "")
        .on(&["findmnt"], 0, "", "")
        .on(&["umount"], 0, "", "")
        .on(&["lvremove"], 0, "", "");
    let e = entry();
    s.driver.release(&pool(&e), "db", &owner()).unwrap();
    let calls: Vec<String> = s.run.calls().iter().map(|c| c[0].clone()).collect();
    let um = calls.iter().position(|c| c == "umount").unwrap();
    let rm = calls.iter().position(|c| c == "lvremove").unwrap();
    assert!(um < rm, "{calls:?}");
    assert!(!mp.exists());

    // Someone else's volume of the same name: refused, nothing removed.
    let s = setup();
    let theirs = stamp_tags("fast", "db", &Owner::of(me() + 1, "/r")).unwrap();
    s.run
        .on(&lvs("vg0/dlx-db"), 0, &lv_row("dlx-db", &theirs), "");
    let err = s.driver.release(&pool(&e), "db", &owner()).unwrap_err();
    assert_eq!(err.number(), 5201);
    assert!(s.run.calls_of("lvremove").is_empty());

    // Already gone: fine.
    let s = setup();
    s.run.on(&lvs("vg0/dlx-db"), 5, "", MISSING_LV);
    s.driver.release(&pool(&e), "db", &owner()).unwrap();
}

#[test]
fn a_busy_mount_stops_the_release_before_the_volume_goes() {
    let s = setup();
    let tags = stamp_tags("fast", "db", &owner()).unwrap();
    s.run
        .on(&lvs("vg0/dlx-db"), 0, &lv_row("dlx-db", &tags), "")
        .on(&["findmnt"], 0, "", "")
        .on(&["umount"], 32, "", "umount: /x: target is busy.");
    let e = entry();
    assert!(s.driver.release(&pool(&e), "db", &owner()).is_err());
    assert!(s.run.calls_of("lvremove").is_empty());
}

#[test]
fn names_lvm_reserves_and_block_volumes_are_refused_by_name() {
    let s = setup();
    let e = entry();
    let mut r = fs_req(1);
    r.name = "x_tdata".into();
    assert_eq!(
        s.driver
            .allocate(&pool(&e), &r, &owner())
            .unwrap_err()
            .number(),
        1217
    );
    r.name = "db".into();
    r.shape = VolumeShape::Block;
    let err = s.driver.allocate(&pool(&e), &r, &owner()).unwrap_err();
    assert!(err.to_string().contains("P5"), "{err}");
    assert!(s.run.calls().is_empty());
}

#[test]
fn every_operation_needs_the_helper() {
    let s = setup();
    for op in [
        PoolOp::Probe,
        PoolOp::Usage,
        PoolOp::Release,
        PoolOp::Allocate(VolumeShape::Filesystem),
    ] {
        assert_eq!(s.driver.required(op), Privilege::Helper);
    }
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
fn create_pool_makes_the_pv_vg_and_thin_pool_and_marks_them() {
    let s = setup();
    s.run
        .on(
            &["vgs"],
            5,
            r#"{"report":[{"vg":[]}]}"#,
            "  Volume group \"vg0\" not found\n",
        )
        .on(&["pvcreate"], 0, "", "")
        .on(&["vgcreate"], 0, "", "")
        .on(&["lvcreate"], 0, "", "");
    let e = admin_entry();
    s.driver
        .create_pool(
            &pool(&e),
            &[PathBuf::from("/dev/vdb"), PathBuf::from("/dev/vdc")],
        )
        .unwrap();
    assert_eq!(
        s.run.calls_of("pvcreate"),
        vec![argv(&["pvcreate", "--yes", "/dev/vdb", "/dev/vdc"])]
    );
    assert_eq!(
        s.run.calls_of("vgcreate"),
        vec![argv(&[
            "vgcreate",
            "--addtag",
            CREATED_TAG,
            "vg0",
            "/dev/vdb",
            "/dev/vdc"
        ])]
    );
    assert_eq!(
        s.run.calls_of("lvcreate"),
        vec![argv(&[
            "lvcreate",
            "--yes",
            "--type",
            "thin-pool",
            "--extents",
            "90%FREE",
            "--addtag",
            CREATED_TAG,
            "--name",
            "dlx",
            "vg0",
        ])]
    );
}

#[test]
fn create_pool_refuses_a_volume_group_that_already_exists() {
    let s = setup();
    s.run.on(
        &["vgs"],
        0,
        r#"{"report":[{"vg":[{"vg_name":"vg0"}]}]}"#,
        "",
    );
    let e = admin_entry();
    let err = s
        .driver
        .create_pool(&pool(&e), &[PathBuf::from("/dev/vdb")])
        .unwrap_err();
    assert_eq!(err.number(), 5205);
    assert!(s.run.calls_of("pvcreate").is_empty());
}

#[test]
fn the_creation_mark_and_the_contents_are_read_from_lvm() {
    let s = setup();
    s.run
        .on(
            &["vgs"],
            0,
            r#"{"report":[{"vg":[{"vg_name":"vg0","vg_tags":"other,delonix.io_created"}]}]}"#,
            "",
        )
        .on(
            &["lvs", "--reportformat", "json", "-o", "lv_name", "vg0"],
            0,
            r#"{"report":[{"lv":[{"lv_name":"dlx"},{"lv_name":"dlx-db"},{"lv_name":"home"}]}]}"#,
            "",
        );
    let e = admin_entry();
    assert!(s.driver.created_here(&pool(&e)).unwrap());
    assert_eq!(
        s.driver.contents(&pool(&e)).unwrap(),
        vec!["dlx-db".to_string(), "home".to_string()]
    );

    let s = setup();
    s.run.on(
        &["vgs"],
        0,
        r#"{"report":[{"vg":[{"vg_name":"vg0","vg_tags":""}]}]}"#,
        "",
    );
    assert!(!s.driver.created_here(&pool(&e)).unwrap());

    let s = setup();
    s.run.on(&["vgs"], 0, "", NON_ROOT_STDERR);
    assert_eq!(s.driver.created_here(&pool(&e)).unwrap_err().number(), 6204);
}

#[test]
fn destroy_pool_reads_the_pvs_first_and_removes_in_order() {
    let s = setup();
    s.run
        .on(
            &["pvs"],
            0,
            r#"{"report":[{"pv":[{"pv_name":"/dev/vdb"},{"pv_name":"/dev/vdc"}]}]}"#,
            "",
        )
        .on(&["lvremove"], 0, "", "")
        .on(&["vgremove"], 0, "", "")
        .on(&["pvremove"], 0, "", "");
    let e = admin_entry();
    s.driver.destroy_pool(&pool(&e)).unwrap();
    let order: Vec<String> = s.run.calls().iter().map(|c| c[0].clone()).collect();
    assert_eq!(order, ["pvs", "lvremove", "vgremove", "pvremove"]);
    assert_eq!(
        s.run.calls_of("pvremove"),
        vec![argv(&["pvremove", "--yes", "/dev/vdb", "/dev/vdc"])]
    );
}

/// The guards of `destroy_guarded` hold for this backend: a pool that holds a
/// volume is refused before anything is removed.
#[test]
fn the_guarded_destroy_refuses_a_thin_pool_with_a_volume() {
    let s = setup();
    s.run
        .on(
            &["vgs"],
            0,
            r#"{"report":[{"vg":[{"vg_name":"vg0","vg_tags":"delonix.io_created"}]}]}"#,
            "",
        )
        .on(
            &["lvs", "--reportformat", "json", "-o", "lv_name", "vg0"],
            0,
            r#"{"report":[{"lv":[{"lv_name":"dlx"},{"lv_name":"dlx-db"}]}]}"#,
            "",
        );
    let e = admin_entry();
    let err = delonix_storage::admin::destroy_guarded(&s.driver, &pool(&e)).unwrap_err();
    assert_eq!(err.number(), 5201);
    assert!(s.run.calls_of("lvremove").is_empty());
    assert!(s.run.calls_of("vgremove").is_empty());
}
