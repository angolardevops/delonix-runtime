//! Live proof of the process leaf (`delonix_linux::create_process_leaf`): a cgroup
//! leaf for a process tree that is not a container, with its ceilings written
//! before anything enters it.
//!
//! `#[ignore]`d for the same reason as `cgroup_parent.rs`: the CI container cannot
//! deliver a delegated cgroup v2 base. On a host that can (root, or rootless under
//! a `Delegate=yes` scope), an environment that cannot create the leaf FAILS
//! naming it — a test that returned early would read as `passed`.
//!
//! `cargo test -p delonix-linux --test process_leaf -- --ignored --nocapture`

use delonix_linux::{create_process_leaf, LeafLimits, ProcessLeaf};
use std::process::{Child, Command};

/// Kills the child and removes the leaf even when an assert blows up midway.
struct Guard {
    child: Option<Child>,
    leaf: Option<ProcessLeaf>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(l) = &self.leaf {
            l.remove();
        }
    }
}

#[test]
#[ignore = "needs root or a delegated cgroup v2 (systemd-run --user --scope -p Delegate=yes): cargo test -p delonix-linux --test process_leaf -- --ignored"]
fn a_process_leaf_has_its_ceilings_before_the_process_enters() {
    let mut guard = Guard {
        child: None,
        leaf: None,
    };
    let limits = LeafLimits {
        cpu_quota_usec: Some(LeafLimits::quota_for_cores(1)),
        memory_max_bytes: Some(64 * 1024 * 1024),
        pids_max: Some(32),
        io_max: None,
    };
    let name = format!("test-{}", std::process::id());
    let leaf = create_process_leaf(&name, &limits)
        .unwrap_or_else(|e| panic!("the leaf could not be created: {e}"))
        .expect("no leaf can exist here — run as root or under a Delegate=yes scope");
    guard.leaf = Some(leaf.clone());
    eprintln!("leaf: {}", leaf.path().display());
    assert_eq!(leaf.dir_name(), Some(format!("dlx-{name}").as_str()));

    let read = |f: &str| {
        std::fs::read_to_string(leaf.path().join(f))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    // Written BEFORE anything entered: nothing is in it yet.
    assert_eq!(
        read("cgroup.procs"),
        "",
        "the leaf had a process before the ceiling"
    );
    assert_eq!(read("memory.max"), "67108864");
    assert_eq!(read("cpu.max"), "100000 100000");
    assert_eq!(read("pids.max"), "32");

    let child = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pid = child.id() as i32;
    guard.child = Some(child);
    leaf.enter(pid).expect("move the child into the leaf");
    assert_eq!(
        ProcessLeaf::of_pid(pid).as_ref(),
        Some(&leaf),
        "where the child is"
    );
    let ev = leaf.events();
    assert_eq!(ev.oom_kill, Some(0), "memory.events is readable and clean");
    assert_eq!(ev.pids_max, Some(0), "pids.events is readable and clean");

    drop(guard);
    assert!(!leaf.path().exists(), "the leaf outlived its process");
}

/// A name that would leave the base is refused before anything is created.
#[test]
fn an_unsafe_leaf_name_is_refused() {
    for bad in ["../escape", "a/b", "-rf", ""] {
        let e = create_process_leaf(bad, &LeafLimits::default())
            .expect_err("an unsafe leaf name was accepted");
        assert!(e.to_string().contains("process leaf"), "{bad:?}: {e}");
    }
}
