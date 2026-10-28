//! Live-path tests: the crate's own code driving a real Ceph cluster.
//!
//! Each is `#[ignore]` — it runs only when asked for by name or with
//! `--ignored`, never in the default suite, because it needs a cluster. They
//! run the Ceph tools through `sudo -n` (the map/mount/create paths are root on
//! the node, served by the helper in production); the host they are written for
//! is the session's MicroCeph (see `reports/ceph-live/cluster-access.md`).
//!
//! Run with:
//!   cargo test -p delonix-provider-ceph --test live -- --ignored --nocapture --test-threads=1

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use delonix_storage::allowlist::Entry;
use delonix_storage::exec::{Output, Run};
use delonix_storage::ownership::Owner;
use delonix_storage::pool::{PoolProbe, PoolRef, StoragePoolDriver, VolumeRequest, VolumeShape};

use delonix_provider_ceph::ceph::CephCtx;
use delonix_provider_ceph::{cluster_health, CephFsDriver, RbdDriver, Rgw};

/// A `Run` that executes each argv through `sudo -n` — the tools need root on
/// the node, and this host grants passwordless sudo.
struct SudoRun;

impl Run for SudoRun {
    fn run(&self, argv: &[String]) -> std::io::Result<Output> {
        let out = Command::new("sudo").arg("-n").args(argv).output()?;
        Ok(Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

const CONF: &str = "/etc/ceph/ceph.conf";

fn rbd_pool_entry() -> Entry {
    Entry {
        driver: "ceph-rbd".into(),
        ceph_pool: Some("rbdpool".into()),
        ceph_conf: Some(PathBuf::from(CONF)),
        ..Default::default()
    }
}

fn cephfs_pool_entry() -> Entry {
    Entry {
        driver: "cephfs".into(),
        ceph_fs: Some("delonixfs".into()),
        ceph_conf: Some(PathBuf::from(CONF)),
        ..Default::default()
    }
}

fn owner() -> Owner {
    Owner::of(1000, "/home/walter/.local/share/delonix")
}

#[test]
#[ignore = "needs a live Ceph cluster: run with --ignored (see reports/ceph-live/cluster-access.md)"]
fn rados_cluster_is_healthy() {
    let ctx = CephCtx {
        conf: Some(PathBuf::from(CONF)),
        user: "admin".into(),
    };
    let h = cluster_health(&SudoRun, &ctx).expect("ceph status readable");
    println!("[RADOS] {h:?}");
    assert!(h.status.starts_with("HEALTH_"), "status: {}", h.status);
    assert!(h.osds_up >= 1, "expected at least one OSD up");
    assert!(h.pools >= 1, "expected at least one pool");
}

#[test]
#[ignore = "needs a live Ceph cluster with a 'rbdpool' RADOS pool: run with --ignored"]
fn rbd_block_volume_full_lifecycle() {
    let driver = RbdDriver::new(Arc::new(SudoRun));
    let entry = rbd_pool_entry();
    let pool = PoolRef {
        name: "block",
        entry: &entry,
    };
    assert_eq!(driver.probe(&pool), PoolProbe::Available, "rbdpool probes");

    let req = VolumeRequest {
        name: "dlxlive".into(),
        shape: VolumeShape::Block,
        size_bytes: 64 * (1 << 20),
    };
    // Clean any leftover from a prior run.
    let _ = driver.release(&pool, "dlxlive", &owner());

    let a = driver.allocate(&pool, &req, &owner()).expect("allocate");
    println!("[RBD] allocated {:?} at {}", a.volume, a.path.display());
    assert!(
        a.path.to_string_lossy().starts_with("/dev/rbd"),
        "block path is a kernel device: {}",
        a.path.display()
    );
    assert!(a.path.exists(), "the device node exists");

    // Adopt-by-stamp: a second allocate returns the same device, adopted.
    let again = driver
        .allocate(&pool, &req, &owner())
        .expect("re-allocate adopts");
    assert!(again.adopted, "the second allocate adopts the stamped image");
    assert_eq!(again.path, a.path);

    driver.release(&pool, "dlxlive", &owner()).expect("release");
    // Releasing again is a no-op (already gone).
    driver
        .release(&pool, "dlxlive", &owner())
        .expect("idempotent release");
    println!("[RBD] released dlxlive");
}

#[test]
#[ignore = "needs a live Ceph cluster with a 'delonixfs' CephFS: run with --ignored"]
fn cephfs_pool_probes_available() {
    let driver = CephFsDriver::new(Arc::new(SudoRun), PathBuf::from("/tmp/dlx-ceph-mnt"));
    let entry = cephfs_pool_entry();
    let pool = PoolRef {
        name: "shared",
        entry: &entry,
    };
    assert_eq!(driver.probe(&pool), PoolProbe::Available, "delonixfs probes");

    // An unstamped subvolume (one the engine did not make) is never released
    // by its name — the stamp boundary, exercised live.
    let run = SudoRun;
    let _ = run.run(&svc(&[
        "fs",
        "subvolume",
        "create",
        "delonixfs",
        "dlxlive-unstamped",
    ]));
    let err = driver.release(&pool, "dlxlive-unstamped", &owner());
    let _ = run.run(&svc(&["fs", "subvolume", "rm", "delonixfs", "dlxlive-unstamped"]));
    assert!(err.is_err(), "release refuses an unstamped subvolume");
    println!("[CephFS] probe Available; unstamped subvolume refused by release");
}

fn svc(parts: &[&str]) -> Vec<String> {
    let mut v = vec![
        "ceph".to_string(),
        "-c".into(),
        CONF.into(),
        "--id".into(),
        "admin".into(),
    ];
    v.extend(parts.iter().map(|s| s.to_string()));
    v
}

#[test]
#[ignore = "needs a live Ceph cluster with RGW and the 'delonix-bucket' bucket: run with --ignored"]
fn rgw_user_and_buckets() {
    let rgw = Rgw::new(Arc::new(SudoRun), Some(PathBuf::from(CONF)));
    rgw.probe().expect("gateway reachable");

    let user = rgw
        .ensure_user("delonix", "Delonix Test")
        .expect("ensure_user");
    println!("[RGW] user {} ak={}", user.uid, user.access_key);
    assert_eq!(user.uid, "delonix");
    assert!(!user.access_key.is_empty());

    let buckets = rgw.list_buckets(None).expect("list_buckets");
    println!("[RGW] buckets: {buckets:?}");
    assert!(
        buckets.iter().any(|b| b == "delonix-bucket"),
        "the bucket created over S3 earlier is listed"
    );

    let stats = rgw.bucket_stats("delonix-bucket").expect("bucket_stats");
    println!(
        "[RGW] delonix-bucket objects={} bytes={}",
        stats.objects, stats.size_bytes
    );
    assert_eq!(stats.name, "delonix-bucket");
}
