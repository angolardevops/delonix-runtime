//! Contention series of one workload, read from its cgroup v2 directory:
//! did the workload get the CPU and the memory its limits promised?
//!
//! Three families of signal, none of which needs a privilege the workload's
//! owner does not already have (they are plain reads of files the kernel
//! exposes in the cgroup directory):
//!
//! - **CPU usage** — `cpu.stat` `usage_usec`: the total CPU time the workload
//!   consumed, a monotonic counter. Unlike the bandwidth fields below it is
//!   ALWAYS present in cgroup v2 (it does not need a `cpu.max` quota), so it is
//!   the one per-workload CPU signal that is non-zero for any running
//!   container, limited or not.
//! - **Memory usage** — `memory.current`: the bytes the workload's cgroup
//!   charges right now, always present when the memory controller is on.
//! - **CPU throttling** — `cpu.stat` `nr_periods`, `nr_throttled`,
//!   `throttled_usec`: how many enforcement periods the workload ran in, how
//!   many of them it hit its `cpu.max` quota in, and for how long it was held
//!   back. Monotonic counters.
//! - **Pressure stall information (PSI)** — `cpu.pressure`, `memory.pressure`,
//!   `io.pressure`: the share of wall-clock time tasks of the cgroup were
//!   stalled waiting for the resource (`some`: at least one task; `full`: all
//!   of them), as a moving average (`avg10`, percent) and a cumulative total
//!   (`total`, microseconds).
//! - **Memory events** — `memory.events.local` (or `memory.events` on a kernel
//!   without the `.local` file): `oom` (the limit was hit and the kernel went
//!   into out-of-memory handling) and `oom_kill` (processes it killed).
//!
//! A value the host cannot provide is an `Err(Why)`, never a zero: a missing
//! `io.pressure` on a rootless leaf is not "no I/O stall", and a dashboard that
//! shows it as 0 is lying. The reason is a small closed set so it can travel as
//! a metric label without growing the series count.
//!
//! The parsers are pure functions over the file text so the odd shapes (partial
//! files, absent `full` lines, garbage values) are covered without a kernel.

use std::io;
use std::path::{Path, PathBuf};

/// Why a group of series has no value in this reading. Closed set on purpose:
/// it is exported as a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The cgroup directory does not exist (the workload is gone).
    CgroupMissing,
    /// The directory is not a cgroup v2 directory (no `cgroup.controllers`).
    NotCgroupV2,
    /// The file is absent: controller not delegated to this cgroup, PSI not
    /// compiled in or switched off with `cgroup_disable=pressure`.
    FileMissing,
    /// The file exists but reading it is refused because PSI is disabled at
    /// run time (`psi=0`): the kernel answers `EOPNOTSUPP`.
    PsiDisabled,
    /// Any other read error (permissions, I/O).
    Unreadable,
    /// The file was read but lacks the fields the series needs (controller not
    /// enabled for the cgroup, or a truncated/garbled file).
    Incomplete,
    /// The cgroup also holds other processes, so its numbers are not this
    /// workload's. Only used when a workload has no cgroup of its own.
    SharedCgroup,
}

impl Why {
    /// The label value.
    pub fn as_str(self) -> &'static str {
        match self {
            Why::CgroupMissing => "cgroup_missing",
            Why::NotCgroupV2 => "not_cgroup_v2",
            Why::FileMissing => "file_missing",
            Why::PsiDisabled => "psi_disabled",
            Why::Unreadable => "unreadable",
            Why::Incomplete => "incomplete",
            Why::SharedCgroup => "shared_cgroup",
        }
    }
}

/// `cpu.stat` bandwidth-control fields. All three are cumulative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuThrottle {
    /// Enforcement periods that elapsed while the cgroup had runnable tasks.
    pub nr_periods: u64,
    /// Periods in which the cgroup used its whole quota and was throttled.
    pub nr_throttled: u64,
    /// Total time the cgroup spent throttled, microseconds.
    pub throttled_usec: u64,
}

/// One `some`/`full` line of a PSI file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StallLine {
    /// Percent of the last 10 s some/all tasks were stalled (0..=100).
    pub avg10_percent: f64,
    /// Cumulative stall time, microseconds.
    pub total_usec: u64,
}

/// A parsed PSI file. `full` is `None` where the kernel does not write it
/// (`cpu.pressure` on older kernels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pressure {
    pub some: StallLine,
    pub full: Option<StallLine>,
}

/// `memory.events` counters. Either may be missing on an old kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryEvents {
    pub oom: Option<u64>,
    pub oom_kill: Option<u64>,
}

/// One reading of a cgroup: each group is independent, so a host that lacks
/// `io.pressure` still reports the CPU and memory groups.
#[derive(Debug, Clone, PartialEq)]
pub struct CgroupSeries {
    /// `cpu.stat` `usage_usec`: total CPU time the cgroup consumed, microseconds.
    pub cpu_usage_usec: Result<u64, Why>,
    pub cpu_throttle: Result<CpuThrottle, Why>,
    pub cpu_pressure: Result<Pressure, Why>,
    pub memory_pressure: Result<Pressure, Why>,
    pub io_pressure: Result<Pressure, Why>,
    pub memory_events: Result<MemoryEvents, Why>,
    /// `memory.current`: bytes the cgroup charges right now.
    pub memory_current: Result<u64, Why>,
}

impl CgroupSeries {
    /// A reading where every group failed for the same reason (no cgroup to
    /// read at all).
    pub fn unavailable(why: Why) -> Self {
        Self {
            cpu_usage_usec: Err(why),
            cpu_throttle: Err(why),
            cpu_pressure: Err(why),
            memory_pressure: Err(why),
            io_pressure: Err(why),
            memory_events: Err(why),
            memory_current: Err(why),
        }
    }
}

// --- pure parsers -----------------------------------------------------------

/// The three bandwidth fields of a `cpu.stat`. `None` unless all three are
/// present and numeric: a counter pair with one half missing cannot be turned
/// into a ratio, and a throttle count without its period count means nothing.
pub fn parse_cpu_stat_throttle(text: &str) -> Option<CpuThrottle> {
    let (mut periods, mut throttled, mut usec) = (None, None, None);
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(key), Some(value)) = (it.next(), it.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "nr_periods" => periods = Some(value),
            "nr_throttled" => throttled = Some(value),
            "throttled_usec" => usec = Some(value),
            _ => {}
        }
    }
    Some(CpuThrottle {
        nr_periods: periods?,
        nr_throttled: throttled?,
        throttled_usec: usec?,
    })
}

/// `usage_usec` of a `cpu.stat`: the total CPU time the cgroup consumed, in
/// microseconds. Always present in cgroup v2 (the bandwidth fields are not —
/// they appear only once a `cpu.max` quota is set), so this is the one CPU
/// number that is non-zero for any running workload. `None` when the line is
/// absent or does not parse.
pub fn parse_cpu_usage_usec(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        match (it.next(), it.next()) {
            (Some("usage_usec"), Some(value)) => value.parse::<u64>().ok(),
            _ => None,
        }
    })
}

/// `avg10=<f> avg60=<f> avg300=<f> total=<n>` after the leading `some`/`full`.
fn parse_stall_line(line: &str) -> Option<StallLine> {
    let (mut avg10, mut total) = (None, None);
    for field in line.split_whitespace().skip(1) {
        if let Some(v) = field.strip_prefix("avg10=") {
            avg10 = v.parse::<f64>().ok().filter(|x| x.is_finite() && *x >= 0.0);
        } else if let Some(v) = field.strip_prefix("total=") {
            total = v.parse::<u64>().ok();
        }
    }
    Some(StallLine {
        avg10_percent: avg10?,
        total_usec: total?,
    })
}

/// A `*.pressure` file. The `some` line is required; `full` is optional.
pub fn parse_pressure(text: &str) -> Option<Pressure> {
    let (mut some, mut full) = (None, None);
    for line in text.lines() {
        match line.split_whitespace().next() {
            Some("some") => some = parse_stall_line(line),
            Some("full") => full = parse_stall_line(line),
            _ => {}
        }
    }
    Some(Pressure { some: some?, full })
}

/// `oom` and `oom_kill` of a `memory.events`. `None` when neither is there.
pub fn parse_memory_events(text: &str) -> Option<MemoryEvents> {
    let (mut oom, mut kill) = (None, None);
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(key), Some(value)) = (it.next(), it.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "oom" => oom = Some(value),
            "oom_kill" => kill = Some(value),
            _ => {}
        }
    }
    (oom.is_some() || kill.is_some()).then_some(MemoryEvents {
        oom,
        oom_kill: kill,
    })
}

// --- readers ----------------------------------------------------------------

fn classify(e: &io::Error, psi: bool) -> Why {
    if e.kind() == io::ErrorKind::NotFound {
        Why::FileMissing
    } else if psi && e.raw_os_error() == Some(libc::EOPNOTSUPP) {
        Why::PsiDisabled
    } else {
        Why::Unreadable
    }
}

fn read_pressure(dir: &Path, name: &str) -> Result<Pressure, Why> {
    let text = std::fs::read_to_string(dir.join(name)).map_err(|e| classify(&e, true))?;
    parse_pressure(&text).ok_or(Why::Incomplete)
}

/// `memory.events.local` first: the plain `memory.events` is hierarchical, so
/// a leaf that holds a whole subtree would report its children's kills as its
/// own. The plain file is the fallback for a kernel without `.local`.
fn read_memory_events(dir: &Path) -> Result<MemoryEvents, Why> {
    for name in ["memory.events.local", "memory.events"] {
        match std::fs::read_to_string(dir.join(name)) {
            Ok(text) => return parse_memory_events(&text).ok_or(Why::Incomplete),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(classify(&e, false)),
        }
    }
    Err(Why::FileMissing)
}

/// `memory.current`: a single integer file (bytes). Always present in a cgroup
/// v2 with the memory controller enabled.
fn read_memory_current(dir: &Path) -> Result<u64, Why> {
    let text =
        std::fs::read_to_string(dir.join("memory.current")).map_err(|e| classify(&e, false))?;
    text.trim().parse::<u64>().map_err(|_| Why::Incomplete)
}

/// Reads every group from one cgroup v2 directory.
pub fn read_cgroup(dir: &Path) -> CgroupSeries {
    if !dir.is_dir() {
        return CgroupSeries::unavailable(Why::CgroupMissing);
    }
    if !dir.join("cgroup.controllers").exists() {
        return CgroupSeries::unavailable(Why::NotCgroupV2);
    }
    // `cpu.stat` is read once and both its usage and its bandwidth fields are
    // parsed from the same text.
    let cpu_stat = std::fs::read_to_string(dir.join("cpu.stat")).map_err(|e| classify(&e, false));
    CgroupSeries {
        cpu_usage_usec: cpu_stat
            .as_ref()
            .map_err(|e| *e)
            .and_then(|t| parse_cpu_usage_usec(t).ok_or(Why::Incomplete)),
        cpu_throttle: cpu_stat.and_then(|t| parse_cpu_stat_throttle(&t).ok_or(Why::Incomplete)),
        cpu_pressure: read_pressure(dir, "cpu.pressure"),
        memory_pressure: read_pressure(dir, "memory.pressure"),
        io_pressure: read_pressure(dir, "io.pressure"),
        memory_events: read_memory_events(dir),
        memory_current: read_memory_current(dir),
    }
}

/// The cgroup directory of a VMM process, but only when that cgroup belongs to
/// the VM alone.
///
/// The engine does not create a cgroup for a VMM: it runs in whatever cgroup
/// its launcher was in (a user session scope, a service). Reading that cgroup
/// would report the whole session's pressure under the VM's name. A cgroup is
/// the VM's own only when the VMM is the sole process in `cgroup.procs` (a
/// hypervisor that places each domain in a scope of its own passes; a shared
/// scope is refused with [`Why::SharedCgroup`]).
pub fn vmm_cgroup(proc_root: &Path, cgroup_root: &Path, pid: i32) -> Result<PathBuf, Why> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup"))
        .map_err(|e| classify(&e, false))?;
    let rel = text
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or(Why::NotCgroupV2)?;
    let dir = cgroup_root.join(rel.trim().trim_start_matches('/'));
    let procs =
        std::fs::read_to_string(dir.join("cgroup.procs")).map_err(|e| classify(&e, false))?;
    let mut members = procs.lines().filter_map(|l| l.trim().parse::<i32>().ok());
    match (members.next(), members.next()) {
        (Some(only), None) if only == pid => Ok(dir),
        _ => Err(Why::SharedCgroup),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn v2_dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "cgroup.controllers", "cpu memory io pids\n");
        d
    }

    const CPU_STAT: &str = "usage_usec 9000000\nuser_usec 6000000\nsystem_usec 3000000\n\
        nr_periods 200\nnr_throttled 50\nthrottled_usec 1500000\nnr_bursts 0\nburst_usec 0\n";
    const PSI_FULL: &str = "some avg10=12.50 avg60=3.00 avg300=1.00 total=4200000\n\
        full avg10=2.25 avg60=0.50 avg300=0.10 total=900000\n";

    #[test]
    fn cpu_stat_yields_the_three_bandwidth_counters() {
        assert_eq!(
            parse_cpu_stat_throttle(CPU_STAT),
            Some(CpuThrottle {
                nr_periods: 200,
                nr_throttled: 50,
                throttled_usec: 1_500_000
            })
        );
    }

    #[test]
    fn cpu_stat_without_the_bandwidth_fields_is_none_not_zero() {
        // What a cgroup without the cpu controller enabled writes.
        assert_eq!(
            parse_cpu_stat_throttle("usage_usec 10\nuser_usec 5\nsystem_usec 5\n"),
            None
        );
        // One of the three missing.
        assert_eq!(
            parse_cpu_stat_throttle("nr_periods 4\nnr_throttled 1\n"),
            None
        );
        assert_eq!(parse_cpu_stat_throttle(""), None);
    }

    #[test]
    fn cpu_stat_garbage_values_do_not_count_as_present() {
        let t = "nr_periods 4\nnr_throttled lots\nthrottled_usec 7\n";
        assert_eq!(parse_cpu_stat_throttle(t), None);
        // A truncated final line (no value) is skipped, not a panic.
        assert_eq!(
            parse_cpu_stat_throttle("nr_periods 4\nnr_throttled 1\nthrottled_usec"),
            None
        );
    }

    #[test]
    fn pressure_reads_some_and_full() {
        let p = parse_pressure(PSI_FULL).unwrap();
        assert_eq!(p.some.avg10_percent, 12.5);
        assert_eq!(p.some.total_usec, 4_200_000);
        let full = p.full.unwrap();
        assert_eq!(full.avg10_percent, 2.25);
        assert_eq!(full.total_usec, 900_000);
    }

    #[test]
    fn pressure_without_a_full_line_keeps_some() {
        // cpu.pressure on a kernel that predates the cgroup-level full line.
        let p = parse_pressure("some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n").unwrap();
        assert_eq!(p.some.total_usec, 0);
        assert_eq!(p.full, None);
    }

    #[test]
    fn pressure_malformed_lines_are_dropped_and_some_is_required() {
        assert_eq!(parse_pressure(""), None);
        assert_eq!(
            parse_pressure("full avg10=1.00 avg60=0 avg300=0 total=5\n"),
            None
        );
        // `some` with no total: unusable, so the whole file is.
        assert_eq!(parse_pressure("some avg10=1.00 avg60=0 avg300=0\n"), None);
        // Non-numeric / NaN / negative averages are not accepted.
        assert_eq!(
            parse_pressure("some avg10=nan avg60=0 avg300=0 total=1\n"),
            None
        );
        assert_eq!(
            parse_pressure("some avg10=-1.0 avg60=0 avg300=0 total=1\n"),
            None
        );
        // A broken `full` does not take a good `some` with it.
        let p = parse_pressure(
            "some avg10=1.50 avg60=0 avg300=0 total=9\nfull avg10=oops avg60=0 avg300=0 total=1\n",
        )
        .unwrap();
        assert_eq!(p.some.avg10_percent, 1.5);
        assert_eq!(p.full, None);
    }

    #[test]
    fn memory_events_reads_oom_and_oom_kill_by_exact_key() {
        let t = "low 0\nhigh 3\nmax 12\noom 2\noom_kill 1\noom_group_kill 0\n";
        assert_eq!(
            parse_memory_events(t),
            Some(MemoryEvents {
                oom: Some(2),
                oom_kill: Some(1)
            })
        );
    }

    #[test]
    fn memory_events_partial_and_malformed() {
        // An older kernel with no oom_kill: the oom counter still counts.
        assert_eq!(
            parse_memory_events("max 1\noom 4\n"),
            Some(MemoryEvents {
                oom: Some(4),
                oom_kill: None
            })
        );
        // `oom_kill` must not be read as `oom`.
        assert_eq!(
            parse_memory_events("oom_kill 7\n"),
            Some(MemoryEvents {
                oom: None,
                oom_kill: Some(7)
            })
        );
        assert_eq!(parse_memory_events("low 0\nhigh 0\n"), None);
        assert_eq!(parse_memory_events("oom x\noom_kill\n"), None);
        assert_eq!(parse_memory_events(""), None);
    }

    #[test]
    fn a_full_cgroup_reads_every_group() {
        let d = v2_dir();
        write(d.path(), "cpu.stat", CPU_STAT);
        write(d.path(), "cpu.pressure", PSI_FULL);
        write(d.path(), "memory.pressure", PSI_FULL);
        write(d.path(), "io.pressure", PSI_FULL);
        write(d.path(), "memory.events", "oom 9\noom_kill 9\n");
        write(d.path(), "memory.events.local", "oom 2\noom_kill 1\n");
        write(d.path(), "memory.current", "4194304\n");
        let s = read_cgroup(d.path());
        // `usage_usec` and `memory.current` are read from the same cgroup.
        assert_eq!(s.cpu_usage_usec, Ok(9_000_000));
        assert_eq!(s.memory_current, Ok(4_194_304));
        assert_eq!(s.cpu_throttle.unwrap().nr_throttled, 50);
        assert_eq!(s.cpu_pressure.unwrap().some.avg10_percent, 12.5);
        assert_eq!(s.memory_pressure.unwrap().full.unwrap().total_usec, 900_000);
        assert!(s.io_pressure.is_ok());
        // `.local` wins over the hierarchical file.
        assert_eq!(s.memory_events.unwrap().oom_kill, Some(1));
    }

    #[test]
    fn cpu_usage_usec_is_read_even_without_the_bandwidth_fields() {
        // An unlimited container: `cpu.stat` has no nr_periods/nr_throttled, so
        // the throttle group is Incomplete, but `usage_usec` is still there and
        // is the one per-workload CPU number that survives. `memory.current`
        // likewise has no bandwidth prerequisite.
        let d = v2_dir();
        write(
            d.path(),
            "cpu.stat",
            "usage_usec 123456\nuser_usec 100000\nsystem_usec 23456\n",
        );
        write(d.path(), "memory.current", "1048576\n");
        let s = read_cgroup(d.path());
        assert_eq!(s.cpu_usage_usec, Ok(123_456));
        assert_eq!(s.cpu_throttle, Err(Why::Incomplete));
        assert_eq!(s.memory_current, Ok(1_048_576));
        assert_eq!(parse_cpu_usage_usec("nr_periods 4\n"), None);
    }

    #[test]
    fn memory_events_falls_back_to_the_plain_file() {
        let d = v2_dir();
        write(d.path(), "memory.events", "oom 3\noom_kill 3\n");
        assert_eq!(read_cgroup(d.path()).memory_events.unwrap().oom, Some(3));
    }

    #[test]
    fn a_missing_pressure_file_is_a_reason_and_not_a_zero() {
        let d = v2_dir();
        write(d.path(), "cpu.stat", CPU_STAT);
        write(d.path(), "cpu.pressure", PSI_FULL);
        write(d.path(), "memory.pressure", PSI_FULL);
        // no io.pressure: io not delegated to this leaf
        let s = read_cgroup(d.path());
        assert_eq!(s.io_pressure, Err(Why::FileMissing));
        assert!(s.cpu_pressure.is_ok() && s.memory_pressure.is_ok());
        assert_eq!(s.memory_events, Err(Why::FileMissing));
    }

    #[test]
    fn an_unreadable_pressure_file_is_unreadable() {
        // A directory where the file should be: read_to_string fails with a
        // non-NotFound error, the stand-in for EACCES/EIO.
        let d = v2_dir();
        std::fs::create_dir(d.path().join("io.pressure")).unwrap();
        assert_eq!(read_cgroup(d.path()).io_pressure, Err(Why::Unreadable));
    }

    #[test]
    fn a_truncated_file_is_incomplete() {
        let d = v2_dir();
        write(d.path(), "cpu.stat", "usage_usec 1\n");
        write(d.path(), "memory.pressure", "");
        let s = read_cgroup(d.path());
        assert_eq!(s.cpu_throttle, Err(Why::Incomplete));
        assert_eq!(s.memory_pressure, Err(Why::Incomplete));
    }

    #[test]
    fn a_gone_cgroup_and_a_non_v2_directory_say_which() {
        let d = tempfile::tempdir().unwrap();
        let gone = read_cgroup(&d.path().join("nope"));
        assert_eq!(gone, CgroupSeries::unavailable(Why::CgroupMissing));
        // A directory that is not a cgroup v2 one (v1 hierarchy, a wrong path).
        write(d.path(), "cpu.stat", CPU_STAT);
        assert_eq!(
            read_cgroup(d.path()),
            CgroupSeries::unavailable(Why::NotCgroupV2)
        );
    }

    #[test]
    fn reasons_are_a_closed_set_of_label_safe_strings() {
        for w in [
            Why::CgroupMissing,
            Why::NotCgroupV2,
            Why::FileMissing,
            Why::PsiDisabled,
            Why::Unreadable,
            Why::Incomplete,
            Why::SharedCgroup,
        ] {
            assert!(w
                .as_str()
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'));
        }
    }

    fn vmm_fixture(procs: &str) -> (tempfile::TempDir, tempfile::TempDir) {
        let proc_root = tempfile::tempdir().unwrap();
        let cg_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proc_root.path().join("4242")).unwrap();
        write(
            &proc_root.path().join("4242"),
            "cgroup",
            "0::/machine.slice/vm-a.scope\n",
        );
        let scope = cg_root.path().join("machine.slice/vm-a.scope");
        std::fs::create_dir_all(&scope).unwrap();
        write(&scope, "cgroup.procs", procs);
        (proc_root, cg_root)
    }

    #[test]
    fn a_vmm_alone_in_its_cgroup_owns_it() {
        let (p, c) = vmm_fixture("4242\n");
        let dir = vmm_cgroup(p.path(), c.path(), 4242).unwrap();
        assert!(dir.ends_with("machine.slice/vm-a.scope"));
    }

    #[test]
    fn a_shared_cgroup_is_refused_rather_than_misattributed() {
        let (p, c) = vmm_fixture("4242\n999\n");
        assert_eq!(vmm_cgroup(p.path(), c.path(), 4242), Err(Why::SharedCgroup));
        let (p, c) = vmm_fixture("");
        assert_eq!(vmm_cgroup(p.path(), c.path(), 4242), Err(Why::SharedCgroup));
        // Someone else alone in it is not this VMM's either.
        let (p, c) = vmm_fixture("777\n");
        assert_eq!(vmm_cgroup(p.path(), c.path(), 4242), Err(Why::SharedCgroup));
    }

    #[test]
    fn a_vmm_without_a_v2_line_or_a_proc_entry_is_named() {
        let (p, c) = vmm_fixture("4242\n");
        write(&p.path().join("4242"), "cgroup", "12:cpu:/legacy\n");
        assert_eq!(vmm_cgroup(p.path(), c.path(), 4242), Err(Why::NotCgroupV2));
        assert_eq!(vmm_cgroup(p.path(), c.path(), 1), Err(Why::FileMissing));
    }
}
