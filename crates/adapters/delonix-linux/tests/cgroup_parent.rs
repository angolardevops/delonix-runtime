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

/// An unsafe group name is REFUSED before anything is placed — in either privilege
/// mode. It used to be dropped on the rootless path: the container started with no
/// group at all, while its caller believed it was capped with the others.
///
/// Needs no busybox and no delegation: the refusal comes before the rootfs or a
/// cgroup is touched. Kept here, beside the test it replaces, because the live
/// behaviour is what changed.
#[test]
fn an_unsafe_group_name_is_refused_before_the_container_starts() {
    let root = tmp("escape");
    let _ = std::fs::remove_dir_all(&root);
    let rootfs = root.join("rootfs");
    std::fs::create_dir_all(&rootfs).expect("create a scratch rootfs dir");
    let store = Store::open(root.join("containers"))
        .unwrap_or_else(|e| panic!("cannot open a store under {}: {e}", root.display()));
    let mut guard = Cleanup {
        store_dir: root.join("containers"),
        ids: Vec::new(),
        root: root.clone(),
    };

    let mut c = Container::new(
        "00000000deadbeef".into(),
        "escape".into(),
        rootfs.to_string_lossy().to_string(),
        vec!["/bin/true".into()],
        "32M".into(),
    );
    // `..` would climb OUT of the engine's base, into a cgroup this engine was
    // never granted — ceiling included.
    c.cgroup_parent = Some(CgroupParent {
        name: "../escaped".into(),
        ..Default::default()
    });
    match delonix_linux::create(&store, &mut c, &rootfs.to_string_lossy(), true) {
        Ok(st) => {
            guard.ids.push(c.clone());
            panic!("a container with an unsafe group name started ({st:?})");
        }
        Err(e) => {
            let code = delonix_model::Error::from(e).number();
            assert_eq!(code, 1116, "refused, with the group's own dictionary entry");
        }
    }
}

/// ROOT mode honours the group: the container lands under
/// `delonix.slice/<group>/`, and the group has its ceiling — written before the
/// container joined. Before, root mode put every container at
/// `delonix.slice/delonix-<id>`, and a group of containers had no aggregate cap.
#[test]
#[ignore = "needs root and /usr/bin/busybox: sudo -E cargo test -p delonix-linux --test cgroup_parent -- --ignored --test-threads=1 root_mode"]
fn root_mode_container_of_a_group_lands_under_the_group_with_its_ceiling() {
    // SAFETY: `geteuid` takes no arguments and has no preconditions.
    let euid = unsafe { libc::geteuid() };
    assert_eq!(
        euid, 0,
        "this test proves the ROOT path: run it with sudo (see the ignore reason)"
    );
    let root = tmp("rootgrp");
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
    let name = format!("dlx-test-group-{}", std::process::id());
    let mut c = Container::new(
        "00000000c0ffee01".into(),
        "load-root".into(),
        rootfs.to_string_lossy().to_string(),
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "while :; do sleep 1; done".into(),
        ],
        "32M".into(),
    );
    c.cgroup_parent = Some(CgroupParent {
        name: name.clone(),
        memory_max: Some("64Mi".into()),
        cpus: Some("1".into()),
        pids_max: Some("256".into()),
    });
    let st = delonix_linux::create(&store, &mut c, &rootfs.to_string_lossy(), true)
        .unwrap_or_else(|e| panic!("the root container did not start: {e}"));
    guard.ids.push(c.clone());
    assert_eq!(st, Status::Running);

    let gdir = format!("/sys/fs/cgroup/delonix.slice/{name}");
    let live = delonix_linux::live_cgroup(&c);
    assert_eq!(
        live,
        format!("{gdir}/delonix-{}", c.id),
        "where the container landed"
    );
    assert_eq!(
        live,
        c.cgroup(),
        "the static root path and the live one agree"
    );
    let read = |f: &str| {
        std::fs::read_to_string(format!("{gdir}/{f}"))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    assert_eq!(read("memory.max"), GROUP_MEM, "64Mi normalised to bytes");
    assert_eq!(read("cpu.max"), "100000 100000");
    assert_eq!(read("pids.max"), "256");
    let delegated = read("cgroup.subtree_control");
    for ctrl in ["memory", "cpu", "pids"] {
        assert!(
            delegated.split_whitespace().any(|c| c == ctrl),
            "{ctrl} not handed down to the group: {delegated:?}"
        );
    }
    drop(guard);
    // The group outlives its containers on purpose (siblings may still use it);
    // this test made it, so it removes it.
    let _ = std::fs::remove_dir(&gdir);
}
