//! A cgroup v2 leaf for a process tree that is NOT a container — a VMM, say.
//!
//! The container path has always had its leaf: `setup_cgroup` creates it, writes the
//! ceilings and only then lets the container in. A process the engine launches for any
//! other reason (the Cloud Hypervisor VMM is the first) had nothing: it ran in whatever
//! cgroup its launcher happened to be in, so a VM's `cpu.max`/`memory.max` were never
//! enforced on the process that actually holds its vCPUs and its RAM.
//!
//! What lives here is the part that does not depend on the privilege model: the limit
//! files a leaf gets (pure, so their contents are tested without a cgroupfs), the name
//! rule, and the handle a caller uses once a leaf exists — enter it, read its event
//! counters, remove it. WHERE a leaf can be created (the root slice, or a delegated
//! base in rootless) is the adapter's knowledge, reached through
//! [`crate::ports::ProcessLeaves`].

use std::path::{Path, PathBuf};

/// The `cpu.max` period every leaf uses, in microseconds — the same 100 ms the
/// container leaves use, so `quota/period` means «cores» everywhere in the engine.
pub const CPU_PERIOD_USEC: u64 = 100_000;

/// The ceilings of a process leaf. `None` = that dimension is not limited at this
/// level (an ancestor's ceiling, if any, still applies).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeafLimits {
    /// `cpu.max` quota per [`CPU_PERIOD_USEC`]: `2 * CPU_PERIOD_USEC` is two cores.
    pub cpu_quota_usec: Option<u64>,
    /// `memory.max`, in bytes. Swap is closed with it (see [`limit_files`]).
    pub memory_max_bytes: Option<u64>,
    /// `pids.max` — tasks, which means THREADS: a VMM has one per vCPU and more.
    pub pids_max: Option<u64>,
    /// A ready `io.max` line (`MAJ:MIN rbps=… wbps=…`). Best-effort: a rootless base
    /// rarely has the `io` controller to hand down.
    pub io_max: Option<String>,
}

impl LeafLimits {
    /// The `cpu.max` quota that gives `cores` whole cores.
    pub fn quota_for_cores(cores: u32) -> u64 {
        u64::from(cores.max(1)) * CPU_PERIOD_USEC
    }

    /// No dimension limited at all.
    pub fn is_empty(&self) -> bool {
        self.cpu_quota_usec.is_none()
            && self.memory_max_bytes.is_none()
            && self.pids_max.is_none()
            && self.io_max.is_none()
    }
}

/// One file a leaf gets, and whether a failed write must fail the leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitFile {
    /// The cgroup interface file, relative to the leaf.
    pub file: &'static str,
    /// What is written to it.
    pub value: String,
    /// `true`: the ceiling the caller asked for — a leaf where it did not land is
    /// not the leaf that was asked for, and is refused. `false`: hardening that a
    /// host may legitimately lack (swap accounting off, no `io` controller).
    pub required: bool,
}

/// The files a leaf with `limits` gets, in the order they are written. PURE.
///
/// Memory travels with two companions, and both are measured lessons of the
/// container leaf, not decoration:
///
/// * `memory.swap.max = 0` — on a host with swap, a memory ceiling alone only caps
///   RESIDENT memory: the workload swaps at the limit instead of being held to it.
/// * `memory.oom.group = 1` — the OOM killer takes the whole leaf, not one task in
///   it. For a VMM that is the difference between a dead VM and a VMM with a dead
///   I/O thread still reporting the VM as running.
///
/// Both are best-effort (`required: false`): swap accounting can be disabled on a
/// host (`swapaccount=0`), and that must not make every leaf impossible.
pub fn limit_files(limits: &LeafLimits) -> Vec<LimitFile> {
    let mut out = Vec::new();
    let mut push = |file: &'static str, value: String, required: bool| {
        out.push(LimitFile {
            file,
            value,
            required,
        })
    };
    if let Some(bytes) = limits.memory_max_bytes {
        push("memory.max", bytes.to_string(), true);
        push("memory.swap.max", "0".to_string(), false);
        push("memory.oom.group", "1".to_string(), false);
    }
    if let Some(quota) = limits.cpu_quota_usec {
        // The kernel refuses a quota below 1 ms; the container leaf floors at the
        // same value (0.01 of a core).
        push(
            "cpu.max",
            format!("{} {CPU_PERIOD_USEC}", quota.max(1000)),
            true,
        );
    }
    if let Some(pids) = limits.pids_max {
        push("pids.max", pids.max(1).to_string(), true);
    }
    if let Some(line) = &limits.io_max {
        push("io.max", line.clone(), false);
    }
    out
}

/// Is `name` usable as a leaf name? `[A-Za-z0-9._-]`, at most 128 bytes, not starting
/// with `-` or `.`, and no `..` anywhere. PURE.
///
/// A privilege boundary for the same reason [`crate::safe_cgroup_segment`] is: the
/// name becomes a directory under a base the engine writes processes into, and a
/// `/` or a `..` would put the leaf somewhere the ceiling does not hold. Upper case
/// is allowed (unlike a group name) because a leaf is named after a resource, and VM
/// names allow it; 128 bytes leave room for a prefix on a 64-byte resource name and
/// stay well under the kernel's 255. Refused, never rewritten.
pub fn valid_leaf_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('-')
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The directory a leaf called `name` gets under its base. PURE, and the one
/// definition: a later process (a `stop`) recognises a leaf it did not create by
/// this name.
pub fn leaf_dir_name(name: &str) -> String {
    format!("dlx-{name}")
}

/// The counter `key` in a cgroup `*.events` file (`key value` lines), or `None`
/// when the key is absent. PURE.
pub fn event_counter(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        (it.next() == Some(key))
            .then(|| it.next().and_then(|v| v.parse::<u64>().ok()))
            .flatten()
    })
}

/// What the kernel counted against a leaf. `None` = the file (or the key) is not
/// there, which is NOT the same as zero: a leaf without the `memory` controller has
/// no `memory.events` to read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LeafEvents {
    /// `memory.events` `oom_kill`: tasks the OOM killer took inside the leaf.
    pub oom_kill: Option<u64>,
    /// `pids.events` `max`: forks refused because `pids.max` was reached.
    pub pids_max: Option<u64>,
}

/// A cgroup leaf that exists. Obtained from [`crate::ports::ProcessLeaves::create`]
/// (which applied its ceilings before returning it) or recognised from a running
/// process with [`ProcessLeaf::of_pid`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessLeaf {
    path: PathBuf,
}

impl ProcessLeaf {
    /// A handle on the leaf at `path`. For the adapter that created it; a caller
    /// that did not create the leaf uses [`ProcessLeaf::of_pid`].
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The leaf a live process is in, read from `/proc/<pid>/cgroup` (the `0::`
    /// line). `None` when the process is gone or has no cgroup v2 entry.
    pub fn of_pid(pid: i32) -> Option<Self> {
        let s = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        let rel = s.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
        Some(Self::at(format!("/sys/fs/cgroup{rel}")))
    }

    /// The leaf's directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The leaf's `cgroup.procs`: writing a pid there moves that whole process.
    pub fn procs_path(&self) -> PathBuf {
        self.path.join("cgroup.procs")
    }

    /// The leaf's own directory name (`dlx-<name>`).
    pub fn dir_name(&self) -> Option<&str> {
        self.path.file_name().and_then(|n| n.to_str())
    }

    /// Moves process `pid` (all of its threads) into the leaf. Memory it already
    /// touched stays charged where it was — enter BEFORE the work starts, which is
    /// what a launcher does by entering the child before it execs.
    pub fn enter(&self, pid: i32) -> std::io::Result<()> {
        std::fs::write(self.procs_path(), pid.to_string())
    }

    /// Moves the calling process into the leaf (`0` names the writer).
    pub fn enter_self(&self) -> std::io::Result<()> {
        std::fs::write(self.procs_path(), "0")
    }

    /// The leaf's event counters, read now.
    pub fn events(&self) -> LeafEvents {
        let read = |f: &str| std::fs::read_to_string(self.path.join(f)).ok();
        LeafEvents {
            oom_kill: read("memory.events").and_then(|t| event_counter(&t, "oom_kill")),
            pids_max: read("pids.events").and_then(|t| event_counter(&t, "max")),
        }
    }

    /// Removes the leaf once it is empty. Best-effort: `rmdir` on a cgroup only
    /// succeeds when no process is left in it, which is exactly when removing it is
    /// right — a failure means something is still in there, and it is never forced.
    /// Retries briefly because a process that was just reaped can take a few
    /// milliseconds to leave the cgroup.
    pub fn remove(&self) {
        for _ in 0..25 {
            if !self.path.exists() || std::fs::remove_dir(&self.path).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_leaf_writes_memory_with_its_companions_then_cpu_pids_and_io() {
        let l = LeafLimits {
            cpu_quota_usec: Some(LeafLimits::quota_for_cores(2)),
            memory_max_bytes: Some(1_207_959_552),
            pids_max: Some(512),
            io_max: Some("8:0 rbps=1048576 wbps=1048576".into()),
        };
        let files: Vec<(&str, String, bool)> = limit_files(&l)
            .into_iter()
            .map(|f| (f.file, f.value, f.required))
            .collect();
        assert_eq!(
            files,
            vec![
                ("memory.max", "1207959552".to_string(), true),
                ("memory.swap.max", "0".to_string(), false),
                ("memory.oom.group", "1".to_string(), false),
                ("cpu.max", "200000 100000".to_string(), true),
                ("pids.max", "512".to_string(), true),
                ("io.max", "8:0 rbps=1048576 wbps=1048576".to_string(), false),
            ]
        );
    }

    #[test]
    fn an_empty_leaf_writes_nothing_and_a_dimension_left_out_is_not_written() {
        assert!(LeafLimits::default().is_empty());
        assert!(limit_files(&LeafLimits::default()).is_empty());
        let only_pids = LeafLimits {
            pids_max: Some(64),
            ..Default::default()
        };
        let files = limit_files(&only_pids);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file, "pids.max");
    }

    #[test]
    fn the_cpu_quota_has_the_kernels_floor_and_whole_cores_scale() {
        assert_eq!(LeafLimits::quota_for_cores(0), 100_000, "zero cores is one");
        assert_eq!(LeafLimits::quota_for_cores(4), 400_000);
        let tiny = LeafLimits {
            cpu_quota_usec: Some(10),
            ..Default::default()
        };
        assert_eq!(limit_files(&tiny)[0].value, "1000 100000");
    }

    #[test]
    fn leaf_names_are_one_safe_segment() {
        let longest_vm = format!("vm-{}", "a".repeat(64));
        for ok in ["vm-web01", "vm-Web.01", "vm_a-b", longest_vm.as_str()] {
            assert!(valid_leaf_name(ok), "{ok:?}");
        }
        let too_long = "a".repeat(129);
        for bad in [
            "",
            "..",
            "../x",
            "a/b",
            "-rf",
            ".hidden",
            "a..b",
            "a b",
            "a\0b",
            too_long.as_str(),
        ] {
            assert!(!valid_leaf_name(bad), "{bad:?}");
        }
        assert_eq!(leaf_dir_name("vm-web01"), "dlx-vm-web01");
    }

    #[test]
    fn event_counters_are_read_by_key_and_absence_is_not_zero() {
        let mem = "low 0\nhigh 12\nmax 1466\noom 1\noom_kill 1\noom_group_kill 0\n";
        assert_eq!(event_counter(mem, "oom_kill"), Some(1));
        assert_eq!(event_counter(mem, "max"), Some(1466));
        assert_eq!(event_counter("max 3\n", "max"), Some(3));
        assert_eq!(event_counter(mem, "nope"), None);
        assert_eq!(event_counter("oom_kill x\n", "oom_kill"), None);
    }

    #[test]
    fn a_leaf_handle_names_its_files() {
        let l = ProcessLeaf::at("/sys/fs/cgroup/delonix.slice/dlx-vm-a");
        assert_eq!(
            l.procs_path(),
            Path::new("/sys/fs/cgroup/delonix.slice/dlx-vm-a/cgroup.procs")
        );
        assert_eq!(l.dir_name(), Some("dlx-vm-a"));
    }

    /// A leaf that is not on a cgroupfs at all reads as «no counters», never as
    /// zero OOM kills.
    #[test]
    fn events_of_a_missing_leaf_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let l = ProcessLeaf::at(dir.path().join("absent"));
        assert_eq!(l.events(), LeafEvents::default());
        std::fs::create_dir(dir.path().join("present")).unwrap();
        std::fs::write(dir.path().join("present/memory.events"), "oom_kill 2\n").unwrap();
        std::fs::write(dir.path().join("present/pids.events"), "max 7\n").unwrap();
        let p = ProcessLeaf::at(dir.path().join("present"));
        assert_eq!(
            p.events(),
            LeafEvents {
                oom_kill: Some(2),
                pids_max: Some(7)
            }
        );
    }
}
