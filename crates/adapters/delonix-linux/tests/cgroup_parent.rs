//! Live proof of the intermediate cgroup level (`Container::cgroup_parent`).
//!
//! Not a mock: it starts REAL containers from a busybox rootfs and reads the cgroup
//! tree back from `/sys/fs/cgroup`. What it proves is what the feature claims — that
//! two containers of the same group land under ONE intermediate cgroup and that the
//! group's ceiling is on it, applied before either of them could allocate anything.
//!
//! `#[ignore]`d: the normal CI container cannot deliver a delegated cgroup v2 base,
//! and a test that RETURNED there was reported as `passed` having started nothing.
//! Run it with `--ignored` on a host that can; there, an environment that cannot
//! deliver what the test needs FAILS naming it.
//!
//! `cargo test -p delonix-linux --test cgroup_parent -- --ignored --nocapture --test-threads=1`

use std::path::{Path, PathBuf};

use delonix_compute::{CgroupParent, Container};
use delonix_model::records::Status;
use delonix_state::Store;

const GROUP_MEM: &str = "67108864"; // 64 MiB for the whole group

/// Minimal rootfs: a static busybox plus the mount points the runtime expects.
fn build_rootfs(dir: &Path) -> Option<()> {
    if !Path::new("/usr/bin/busybox").exists() {
        return None;
    }
    for d in ["bin", "proc", "sys", "dev", "tmp", "etc"] {
        std::fs::create_dir_all(dir.join(d)).ok()?;
    }
    std::fs::copy("/usr/bin/busybox", dir.join("bin/busybox")).ok()?;
    let _ = std::os::unix::fs::symlink("busybox", dir.join("bin/sh"));
    Some(())
}

/// Removes the containers and the cgroup dirs even when an assert blows up midway —
/// otherwise a failed run leaves processes burning on the host forever.
struct Cleanup {
    /// Reaberto no `Drop` — o `Store` não é `Clone`, e guardar uma referência
    /// prendia o guard ao tempo de vida do `store` local.
    store_dir: PathBuf,
    ids: Vec<Container>,
    root: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Ok(store) = Store::open(&self.store_dir) {
            for c in &self.ids {
                let _ = delonix_linux::stop(&store, &mut c.clone(), 2);
                let _ = store.remove(&c.id);
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn tmp(tag: &str) -> PathBuf {
    // Short path on purpose: the runtime opens unix sockets under the root, and
    // `sun_path` is 108 bytes — a long temp dir fails with `path must be shorter
    // than SUN_LEN`, which looks like a bug in the feature and is not.
    PathBuf::from(format!("/tmp/dlx-cgp-{tag}-{}", std::process::id()))
}

#[test]
#[ignore = "needs /usr/bin/busybox and a host that delegates cgroup v2 (memory, cpu, pids) to this user: cargo test -p delonix-linux --test cgroup_parent -- --ignored --test-threads=1"]
fn two_containers_of_a_group_share_one_intermediate_cgroup_with_its_ceiling() {
    let root = tmp("group");
    let _ = std::fs::remove_dir_all(&root);
    let rootfs = root.join("rootfs");
    build_rootfs(&rootfs)
        .expect("a busybox rootfs (from /usr/bin/busybox) is needed to run this --ignored test");
    let store = Store::open(root.join("containers"))
        .unwrap_or_else(|e| panic!("cannot open a store under {}: {e}", root.display()));

    let mut guard = Cleanup {
        store_dir: root.join("containers"),
        ids: Vec::new(),
        root: root.clone(),
    };
    let group = CgroupParent {
        name: "tenant-acme".into(),
        memory_max: Some(GROUP_MEM.into()),
        cpus: Some("1".into()),
        pids_max: Some("256".into()),
    };

    for n in 0..2 {
        let mut c = Container::new(
            format!("{:016x}", 0xc0ffee00u64 + n),
            format!("carga-{n}"),
            rootfs.to_string_lossy().to_string(),
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "while :; do sleep 1; done".into(),
            ],
            "32M".into(),
        );
        c.cgroup_parent = Some(group.clone());
        match delonix_linux::create(&store, &mut c, &rootfs.to_string_lossy(), true) {
            Ok(st) => {
                eprintln!("container {} -> {st:?}", c.name);
                guard.ids.push(c.clone());
                assert_eq!(st, Status::Running, "container had to start");
            }
            Err(e) => panic!(
                "this host cannot start a rootless container ({e}) — the test needs a \
                 delegated cgroup v2 base"
            ),
        }
    }

    // Where did they actually land? `live_cgroup` is the same resolver production uses.
    let paths: Vec<String> = guard.ids.iter().map(delonix_linux::live_cgroup).collect();
    for p in &paths {
        eprintln!("cgroup: {p}");
    }

    let group_dirs: Vec<&str> = paths
        .iter()
        .map(|p| p.rsplit_once('/').map(|(parent, _)| parent).unwrap_or(p))
        .collect();
    assert_eq!(
        group_dirs[0], group_dirs[1],
        "both containers must share ONE group cgroup"
    );
    let gdir = group_dirs[0];
    assert!(
        gdir.ends_with("/tenant-acme"),
        "the group level is missing from the path: {gdir}"
    );

    // The ceiling is on the GROUP, not only on each leaf.
    let read = |p: &str| {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let mem = read(&format!("{gdir}/memory.max"));
    assert!(
        !mem.is_empty(),
        "no `memory.max` on the group — this host does not delegate the memory controller \
         down to it. The nesting is proven; the ceiling is not."
    );
    assert_eq!(mem, GROUP_MEM, "the group's aggregate ceiling");
    assert_eq!(
        read(&format!("{gdir}/memory.swap.max")),
        "0",
        "a memory ceiling that swap walks around is not a ceiling — measured: 64 MiB \
         group let a process allocate 200 MiB until swap was closed"
    );
    assert_eq!(read(&format!("{gdir}/pids.max")), "256");
    eprintln!(
        "group {gdir}: memory.max={mem} swap.max=0 pids.max=256 cpu.max={}",
        read(&format!("{gdir}/cpu.max"))
    );
}

#[test]
#[ignore = "needs /usr/bin/busybox and a host that delegates cgroup v2 (memory, cpu, pids) to this user: cargo test -p delonix-linux --test cgroup_parent -- --ignored --test-threads=1"]
fn an_unsafe_group_name_does_not_escape_the_delegated_base() {
    let root = tmp("escape");
    let _ = std::fs::remove_dir_all(&root);
    let rootfs = root.join("rootfs");
    build_rootfs(&rootfs)
        .expect("a busybox rootfs (from /usr/bin/busybox) is needed to run this --ignored test");
    let store = Store::open(root.join("containers"))
        .unwrap_or_else(|e| panic!("cannot open a store under {}: {e}", root.display()));
    let mut guard = Cleanup {
        store_dir: root.join("containers"),
        ids: Vec::new(),
        root: root.clone(),
    };

    let mut c = Container::new(
        "00000000deadbeef".into(),
        "fuga".into(),
        rootfs.to_string_lossy().to_string(),
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "while :; do sleep 1; done".into(),
        ],
        "32M".into(),
    );
    // `..` would climb OUT of the delegated base, into a cgroup this engine was
    // never granted — ceiling included.
    c.cgroup_parent = Some(CgroupParent {
        name: "../escapou".into(),
        ..Default::default()
    });
    match delonix_linux::create(&store, &mut c, &rootfs.to_string_lossy(), true) {
        Ok(_) => guard.ids.push(c.clone()),
        Err(e) => panic!(
            "this host cannot start a rootless container ({e}) — the test needs a \
             delegated cgroup v2 base"
        ),
    }
    let path = delonix_linux::live_cgroup(&c);
    eprintln!("cgroup with a rejected name: {path}");
    assert!(
        !path.contains("escapou"),
        "the unsafe name was honoured: {path}"
    );
    assert!(!path.contains(".."), "the path climbed out: {path}");
}
