//! One sample of a workload's CUMULATIVE usage counters, with what could not be
//! read named instead of reported as zero.
//!
//! This engine keeps no time series: it is daemonless, so nothing is awake
//! between two commands to record one. What it can give is the kernel's own
//! monotonic counters (`cpu.stat usage_usec`, `io.stat rbytes/wbytes`, the
//! VMM's `utime+stime`) plus the start time of the process they belong to. A
//! caller that samples twice gets a rate; a caller that sees the start time
//! change knows the counters restarted and must not subtract across it. The
//! window, the history and any judgement over it belong to whoever stores the
//! samples.
//!
//! Two bases, and they are not the same quantity:
//! - [`Basis::Cgroup`] — a container: the whole cgroup leaf, every process in it.
//! - [`Basis::VmmProcess`] — a local VM: the hypervisor process on the host. Its
//!   resident memory is what the VM costs THIS host, not what the guest uses
//!   inside; its CPU time includes the VMM's own overhead. A caller comparing
//!   it with a guest's configured memory is comparing two different things, and
//!   the sample says which one it carries.

use std::path::Path;

use serde::Serialize;

/// Where the numbers in a [`UsageSample`] come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    Cgroup,
    VmmProcess,
}

/// A cgroup ceiling as the kernel writes it: a number, or `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Bound {
    Bytes(u64),
    /// The literal `"max"` — no ceiling. Serialised as the kernel spells it so a
    /// reader never confuses it with a ceiling of zero.
    Unlimited(&'static str),
}

/// `cpu.max`: `quota_usec` is `None` when the quota is `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CpuMax {
    pub quota_usec: Option<u64>,
    pub period_usec: u64,
}

/// A field that has no value in this sample, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unmeasured {
    pub field: &'static str,
    pub reason: String,
}

/// One sample. Every `None` has a matching entry in `unmeasured` — the two are
/// built together, so a missing number never reads as a zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageSample {
    pub basis: Basis,
    /// Cumulative CPU time, microseconds, since the process (or cgroup) started.
    pub cpu_usage_usec: Option<u64>,
    /// Current memory: `memory.current` for a cgroup, `VmRSS` for a VMM.
    pub memory_bytes: Option<u64>,
    /// High-water mark: `memory.peak` for a cgroup, `VmHWM` for a VMM.
    pub memory_peak_bytes: Option<u64>,
    /// Cumulative bytes read/written to block devices.
    pub io_read_bytes: Option<u64>,
    pub io_write_bytes: Option<u64>,
    /// Processes in the cgroup (`pids.current`). Not applicable to a VMM.
    pub pids: Option<u64>,
    /// The ceilings in force in the cgroup. Not applicable to a VMM.
    pub memory_max: Option<Bound>,
    pub cpu_max: Option<CpuMax>,
    pub unmeasured: Vec<Unmeasured>,
}

impl UsageSample {
    fn empty(basis: Basis) -> Self {
        Self {
            basis,
            cpu_usage_usec: None,
            memory_bytes: None,
            memory_peak_bytes: None,
            io_read_bytes: None,
            io_write_bytes: None,
            pids: None,
            memory_max: None,
            cpu_max: None,
            unmeasured: Vec::new(),
        }
    }

    fn missing(&mut self, field: &'static str, reason: impl Into<String>) {
        self.unmeasured.push(Unmeasured {
            field,
            reason: reason.into(),
        });
    }
}

/// Samples a cgroup v2 directory. A file that is not there is how a controller
/// that was never delegated looks on a rootless host, and the reason says so.
pub fn cgroup_sample(dir: &Path) -> UsageSample {
    let mut s = UsageSample::empty(Basis::Cgroup);
    if !dir.is_dir() {
        for f in CGROUP_FIELDS {
            s.missing(f, format!("cgroup {} does not exist", dir.display()));
        }
        return s;
    }
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let absent = |name: &str| format!("{name} is absent (controller not delegated to this cgroup)");

    match read("cpu.stat").and_then(|t| crate::parse_cpu_stat_usage_usec(&t)) {
        Some(v) => s.cpu_usage_usec = Some(v),
        None => s.missing("cpu_usage_usec", absent("cpu.stat")),
    }
    match read("memory.current").and_then(|t| t.trim().parse().ok()) {
        Some(v) => s.memory_bytes = Some(v),
        None => s.missing("memory_bytes", absent("memory.current")),
    }
    match read("memory.peak").and_then(|t| t.trim().parse().ok()) {
        Some(v) => s.memory_peak_bytes = Some(v),
        None => s.missing(
            "memory_peak_bytes",
            "memory.peak is absent (kernel older than 5.19, or memory not delegated)",
        ),
    }
    match read("io.stat").map(|t| crate::parse_io_stat_totals(&t)) {
        Some((r, w)) => {
            s.io_read_bytes = Some(r);
            s.io_write_bytes = Some(w);
        }
        None => {
            s.missing("io_read_bytes", absent("io.stat"));
            s.missing("io_write_bytes", absent("io.stat"));
        }
    }
    match read("pids.current").and_then(|t| t.trim().parse().ok()) {
        Some(v) => s.pids = Some(v),
        None => s.missing("pids", absent("pids.current")),
    }
    match read("memory.max").and_then(|t| parse_bound(&t)) {
        Some(v) => s.memory_max = Some(v),
        None => s.missing("memory_max", absent("memory.max")),
    }
    match read("cpu.max").and_then(|t| parse_cpu_max(&t)) {
        Some(v) => s.cpu_max = Some(v),
        None => s.missing("cpu_max", absent("cpu.max")),
    }
    s
}

const CGROUP_FIELDS: [&str; 8] = [
    "cpu_usage_usec",
    "memory_bytes",
    "memory_peak_bytes",
    "io_read_bytes",
    "io_write_bytes",
    "pids",
    "memory_max",
    "cpu_max",
];

/// Samples one host process (a VMM) from `/proc`.
pub fn process_sample(pid: i32) -> UsageSample {
    process_sample_at(Path::new("/proc"), pid, clock_ticks_per_sec())
}

/// [`process_sample`] against any `/proc`-shaped directory — what the tests use.
pub fn process_sample_at(proc_root: &Path, pid: i32, ticks_per_sec: u64) -> UsageSample {
    let mut s = UsageSample::empty(Basis::VmmProcess);
    let dir = proc_root.join(pid.to_string());
    let read = |name: &str| std::fs::read_to_string(dir.join(name));

    match read("stat")
        .ok()
        .and_then(|t| parse_stat_cpu_ticks(&t))
        .filter(|_| ticks_per_sec > 0)
    {
        Some(ticks) => s.cpu_usage_usec = Some(ticks.saturating_mul(1_000_000) / ticks_per_sec),
        None => s.missing("cpu_usage_usec", format!("/proc/{pid}/stat is unreadable")),
    }
    match read("status") {
        Ok(t) => {
            match parse_status_kb(&t, "VmRSS:") {
                Some(kb) => s.memory_bytes = Some(kb * 1024),
                None => s.missing("memory_bytes", "VmRSS is absent from /proc/<pid>/status"),
            }
            match parse_status_kb(&t, "VmHWM:") {
                Some(kb) => s.memory_peak_bytes = Some(kb * 1024),
                None => s.missing(
                    "memory_peak_bytes",
                    "VmHWM is absent from /proc/<pid>/status",
                ),
            }
        }
        Err(e) => {
            s.missing("memory_bytes", format!("/proc/{pid}/status: {e}"));
            s.missing("memory_peak_bytes", format!("/proc/{pid}/status: {e}"));
        }
    }
    match read("io") {
        Ok(t) => match (
            parse_key_value_colon(&t, "read_bytes"),
            parse_key_value_colon(&t, "write_bytes"),
        ) {
            (Some(r), Some(w)) => {
                s.io_read_bytes = Some(r);
                s.io_write_bytes = Some(w);
            }
            _ => {
                s.missing("io_read_bytes", "/proc/<pid>/io has no read_bytes");
                s.missing("io_write_bytes", "/proc/<pid>/io has no write_bytes");
            }
        },
        // `/proc/<pid>/io` needs ptrace access: a VMM run by another user (a
        // system libvirtd) answers EACCES, and that is what the reason carries.
        Err(e) => {
            s.missing("io_read_bytes", format!("/proc/{pid}/io: {e}"));
            s.missing("io_write_bytes", format!("/proc/{pid}/io: {e}"));
        }
    }
    let na = "not applicable to a VMM process: this engine sets no cgroup ceiling on it";
    s.missing("pids", na);
    s.missing("memory_max", na);
    s.missing("cpu_max", na);
    s
}

/// Unix time the process started: the boot time from `/proc/stat` plus its
/// `starttime`. This is the value a caller compares between two samples — when
/// it changes, the counters restarted from zero.
pub fn process_started_at_unix(pid: i32) -> Option<u64> {
    let starttime = delonix_node::proc_starttime(pid)?;
    let btime = std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|t| parse_btime(&t))?;
    let hz = clock_ticks_per_sec();
    (hz > 0).then(|| btime + starttime / hz)
}

fn clock_ticks_per_sec() -> u64 {
    // SAFETY: sysconf has no preconditions and does not touch memory we own.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(v).unwrap_or(0)
}

// --- pure parsers -----------------------------------------------------------

fn parse_key_value(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

fn parse_key_value_colon(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then(|| v.trim().parse().ok()).flatten()
    })
}

fn parse_bound(text: &str) -> Option<Bound> {
    match text.trim() {
        "max" => Some(Bound::Unlimited("max")),
        n => n.parse().ok().map(Bound::Bytes),
    }
}

fn parse_cpu_max(text: &str) -> Option<CpuMax> {
    let mut it = text.split_whitespace();
    let quota = it.next()?;
    let period = it.next()?.parse().ok()?;
    let quota_usec = match quota {
        "max" => None,
        q => Some(q.parse().ok()?),
    };
    Some(CpuMax {
        quota_usec,
        period_usec: period,
    })
}

/// `utime + stime` (fields 14 and 15) of `/proc/<pid>/stat`. The comm (field 2)
/// may contain spaces and parentheses, so fields are counted after the LAST `)`.
fn parse_stat_cpu_ticks(text: &str) -> Option<u64> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut f = rest.split_whitespace();
    // After the comm: field 3 is index 0, so utime (14) is index 11.
    let utime: u64 = f.nth(11)?.parse().ok()?;
    let stime: u64 = f.next()?.parse().ok()?;
    Some(utime.saturating_add(stime))
}

fn parse_status_kb(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
}

fn parse_btime(text: &str) -> Option<u64> {
    parse_key_value(text, "btime")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn a_full_cgroup_is_read_whole_and_nothing_is_marked_missing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        write(
            p,
            "cpu.stat",
            "usage_usec 1500000\nuser_usec 1000000\nsystem_usec 500000\n",
        );
        write(p, "memory.current", "67108864\n");
        write(p, "memory.peak", "100663296\n");
        write(
            p,
            "io.stat",
            "8:0 rbytes=4096 wbytes=8192 rios=1 wios=2\n259:0 rbytes=1 wbytes=2\n",
        );
        write(p, "pids.current", "7\n");
        write(p, "memory.max", "134217728\n");
        write(p, "cpu.max", "50000 100000\n");
        let s = cgroup_sample(p);
        assert_eq!(s.basis, Basis::Cgroup);
        assert_eq!(s.cpu_usage_usec, Some(1_500_000));
        assert_eq!(s.memory_bytes, Some(67_108_864));
        assert_eq!(s.memory_peak_bytes, Some(100_663_296));
        assert_eq!(
            (s.io_read_bytes, s.io_write_bytes),
            (Some(4097), Some(8194))
        );
        assert_eq!(s.pids, Some(7));
        assert_eq!(s.memory_max, Some(Bound::Bytes(134_217_728)));
        assert_eq!(
            s.cpu_max,
            Some(CpuMax {
                quota_usec: Some(50_000),
                period_usec: 100_000
            })
        );
        assert!(s.unmeasured.is_empty(), "{:?}", s.unmeasured);
    }

    #[test]
    fn max_is_unlimited_and_never_a_ceiling_of_zero() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "memory.max", "max\n");
        write(d.path(), "cpu.max", "max 100000\n");
        let s = cgroup_sample(d.path());
        assert_eq!(s.memory_max, Some(Bound::Unlimited("max")));
        assert_eq!(serde_json::to_value(s.memory_max).unwrap(), "max");
        assert_eq!(s.cpu_max.unwrap().quota_usec, None);
    }

    #[test]
    fn an_undelegated_controller_is_named_not_zeroed() {
        // What a rootless leaf with only `cpu memory pids` looks like: no io.stat.
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "cpu.stat", "usage_usec 10\n");
        write(d.path(), "memory.current", "1\n");
        let s = cgroup_sample(d.path());
        assert_eq!(s.io_read_bytes, None);
        let io = s
            .unmeasured
            .iter()
            .find(|u| u.field == "io_read_bytes")
            .unwrap();
        assert!(io.reason.contains("io.stat is absent"), "{}", io.reason);
        // Every None field has exactly one reason.
        for f in [
            "memory_peak_bytes",
            "io_write_bytes",
            "pids",
            "memory_max",
            "cpu_max",
        ] {
            assert_eq!(
                s.unmeasured.iter().filter(|u| u.field == f).count(),
                1,
                "{f}"
            );
        }
    }

    #[test]
    fn an_empty_io_stat_is_a_real_zero() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "io.stat", "");
        let s = cgroup_sample(d.path());
        assert_eq!((s.io_read_bytes, s.io_write_bytes), (Some(0), Some(0)));
    }

    #[test]
    fn a_missing_cgroup_measures_nothing_and_says_so() {
        let d = tempfile::tempdir().unwrap();
        let s = cgroup_sample(&d.path().join("gone"));
        assert_eq!(s.memory_bytes, None);
        assert!(s
            .unmeasured
            .iter()
            .all(|u| u.reason.contains("does not exist")));
    }

    #[test]
    fn a_vmm_process_is_read_from_proc() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("4242");
        std::fs::create_dir(&p).unwrap();
        // comm with a space and a parenthesis, the case the parser must survive.
        write(
            &p,
            "stat",
            "4242 (cloud hyper) visor) S 1 4242 4242 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 4 0 12345 0 0\n",
        );
        write(
            &p,
            "status",
            "Name:\tch\nVmHWM:\t  2048 kB\nVmRSS:\t  1024 kB\n",
        );
        write(
            &p,
            "io",
            "rchar: 9\nwchar: 9\nread_bytes: 4096\nwrite_bytes: 512\n",
        );
        let s = process_sample_at(d.path(), 4242, 100);
        assert_eq!(s.basis, Basis::VmmProcess);
        // (250 + 50) ticks at 100 Hz = 3 s.
        assert_eq!(s.cpu_usage_usec, Some(3_000_000));
        assert_eq!(s.memory_bytes, Some(1024 * 1024));
        assert_eq!(s.memory_peak_bytes, Some(2048 * 1024));
        assert_eq!((s.io_read_bytes, s.io_write_bytes), (Some(4096), Some(512)));
        let fields: Vec<_> = s.unmeasured.iter().map(|u| u.field).collect();
        assert_eq!(fields, ["pids", "memory_max", "cpu_max"]);
    }

    #[test]
    fn an_unreadable_proc_io_carries_the_os_error() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("7");
        std::fs::create_dir(&p).unwrap();
        write(
            &p,
            "stat",
            "7 (x) S 1 1 1 0 -1 0 0 0 0 0 1 1 0 0 20 0 1 0 9 0 0\n",
        );
        write(&p, "status", "VmRSS:\t1 kB\nVmHWM:\t1 kB\n");
        let s = process_sample_at(d.path(), 7, 100);
        assert_eq!(s.io_read_bytes, None);
        let r = &s
            .unmeasured
            .iter()
            .find(|u| u.field == "io_read_bytes")
            .unwrap()
            .reason;
        assert!(r.contains("/proc/7/io"), "{r}");
    }

    #[test]
    fn the_start_time_of_this_process_is_after_boot_and_not_in_the_future() {
        let pid = std::process::id() as i32;
        let started = process_started_at_unix(pid).expect("readable for self");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(started <= now + 1 && started > 1_000_000_000, "{started}");
    }

    #[test]
    fn this_process_samples_from_the_real_proc() {
        let s = process_sample(std::process::id() as i32);
        assert!(s.memory_bytes.unwrap_or(0) > 0);
        assert!(s.cpu_usage_usec.is_some());
    }
}
