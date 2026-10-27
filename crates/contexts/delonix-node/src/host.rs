//! Questions asked of the host and of processes: the clock, the user namespace,
//! whether a pid is still the process a record names, a fresh id. They came out of
//! `delonix-runtime-core` with the rest of the node's own concerns (ADR-0040 P3).

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch, `0` if the clock is before it.
///
/// The one definition: it was copied, identical, into ten modules across six
/// crates, each with its own `SystemTime` imports.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Are we in the INITIAL user namespace — i.e. is uid 0 here the host's root?
///
/// **`geteuid() == 0` does not answer this**, and the difference matters
/// everywhere the engine picks a privileged path. uid 0 inside a nested user
/// namespace buys nothing on the host: no write to the host's cgroup tree, no
/// `/run`, no privileged mount of the host's filesystems. Two independent
/// places in this workspace decided by `geteuid()` alone and both took the
/// ROOT path in exactly the environment where they had the least power.
///
/// The initial namespace is the only one whose `uid_map` is the identity map
/// over the whole range; anything else is nested. It is how podman answers the
/// same question. Unreadable `/proc` answers "initial" — the behaviour this
/// workspace had for years, so an unexpected environment keeps working exactly
/// as before instead of silently switching execution modes.
pub fn in_initial_userns() -> bool {
    let Ok(map) = std::fs::read_to_string("/proc/self/uid_map") else {
        return true;
    };
    initial_uid_map(&map)
}

/// Pure half of [`in_initial_userns`], so the parsing is tested without a
/// namespace to set up.
pub fn initial_uid_map(map: &str) -> bool {
    let mut lines = map.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else {
        return true; // empty map: cannot tell, keep the historical answer
    };
    if lines.next().is_some() {
        return false; // more than one range is never the initial namespace
    }
    let f: Vec<&str> = first.split_whitespace().collect();
    f == ["0", "0", "4294967295"]
}

/// Is this process rootless — i.e. WITHOUT privilege over the host?
///
/// True when the euid is not 0, and ALSO when it is 0 inside a nested user
/// namespace. See [`in_initial_userns`].
pub fn is_rootless() -> bool {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return true;
    }
    !in_initial_userns()
}

/// Formats a unix instant as LOCAL date/time "YYYY-MM-DD HH:MM:SS".
/// Uses `localtime_r` (honors /etc/localtime|TZ); on failure, returns the raw value.
pub fn fmt_local_ts(unix: u64) -> String {
    let t = unix as libc::time_t;
    // SAFETY: `libc::tm` is a plain C struct of integers (and one pointer, which may be null);
    // all-zero bytes is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `t` is valid; `localtime_r` writes into `tm` (our buffer, of the
    // right size) and returns NULL only on error — handled below.
    if unsafe { libc::localtime_r(&t, &mut tm).is_null() } {
        return unix.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// A microVM (Cloud Hypervisor) — the unit of `kind: VM`. SIBLING model of the
/// [`Container`]: a VM has no rootfs/cgroup/seccomp/init-pid, so it does not make
/// sense to overload the `Container`. Persisted via [`store::JsonStore`]
/// (one JSON per name, under `$DELONIX_ROOT/vms`).
/// `true` if process `pid` still exists (signal 0 = only tests liveness).
pub fn is_alive(pid: i32) -> bool {
    // SAFETY: signal 0 sends nothing — it only tests that we may signal `pid`.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// The host's `MemAvailable` in MiB (from `/proc/meminfo`) — memory that can be
/// given to new processes without swapping. `None` if unreadable.
pub fn mem_available_mib() -> Option<u64> {
    parse_mem_available_mib(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// `MemAvailable` in MiB out of the text of `/proc/meminfo`. Pure.
fn parse_mem_available_mib(meminfo: &str) -> Option<u64> {
    let kib: u64 = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kib / 1024)
}

/// The process `starttime` (field 22 of `/proc/<pid>/stat`, jiffies since
/// boot). Unique and stable for the process's lifetime — we use it to detect
/// PID reuse.
pub fn proc_starttime(pid: i32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm (field 2) may contain spaces/parentheses — cut up to the last ')'.
    let rest = &s[s.rfind(')')? + 1..];
    rest.split_whitespace().nth(19).and_then(|f| f.parse().ok()) // field 22 = the 20th after the comm
}

/// `true` if it is safe to send a signal to `pid` on behalf of this container: the PID
/// is alive AND (if we know the recorded `starttime`) is still the SAME process.
/// Guards against PID reuse — we never kill a process belonging to the host.
pub fn safe_to_signal(pid: i32, starttime: Option<u64>) -> bool {
    if !is_alive(pid) {
        return false;
    }
    match starttime {
        Some(want) => proc_starttime(pid) == Some(want),
        None => true, // old record without starttime: legacy behavior
    }
}

/// Generates a container id: 16 hexadecimal characters.
pub fn generate_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let mixed = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ pid.rotate_left(32);
    format!("{mixed:016x}")
}

/// Path of the `delonix` binary to relaunch to delegate operations (exec, run,
/// API mutations…). Prefers the executable itself, but is **robust to
/// binary replacement** while the server runs (install/upgrade): in that
/// case `/proc/self/exe` is marked `" (deleted)"` and `current_exe()` returns
/// a nonexistent path — which made spawns fail with `os error 2`. Tries,
/// in order: current exe if it exists → path without the `(deleted)` suffix → `delonix`
/// on the `PATH` → the plain name.
pub fn self_bin() -> std::path::PathBuf {
    use std::path::{Path, PathBuf};
    if let Ok(p) = std::env::current_exe() {
        if p.exists() {
            return p;
        }
        let s = p.to_string_lossy();
        if let Some(real) = s.strip_suffix(" (deleted)") {
            let pb = PathBuf::from(real);
            if pb.exists() {
                return pb;
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let cand = Path::new(dir).join("delonix");
            if cand.exists() {
                return cand;
            }
        }
    }
    PathBuf::from("delonix")
}

/// The local host's capacity to run workloads: CPU, memory, swap and the disk
/// that holds the VM images. For a provider whose VMs run on THIS machine
/// (Cloud Hypervisor, libvirt `qemu:///system`), the "node" of `host.capacity`
/// IS this host — the parallel of Proxmox's `GET /nodes/{node}/status`.
///
/// Discipline (same as the node-api's `/v1/node/capacity`): a number that could
/// not be measured names itself in [`HostCapacity::unmeasured`] and stays `0`,
/// never a silent zero. A number that is LEGITIMATELY zero (a host with no
/// swap) is a real `0` and is NOT unmeasured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostCapacity {
    /// Logical CPUs the host reports (`processor` lines in `/proc/cpuinfo`).
    pub cpus: u64,
    /// The CPU model string (first `model name` in `/proc/cpuinfo`).
    pub cpu_model: String,
    /// Total RAM in bytes (`MemTotal`).
    pub mem_total_bytes: u64,
    /// RAM in use in bytes (`MemTotal - MemAvailable`).
    pub mem_used_bytes: u64,
    /// Total swap in bytes (`SwapTotal`); a real `0` on a host without swap.
    pub swap_total_bytes: u64,
    /// Swap in use in bytes (`SwapTotal - SwapFree`).
    pub swap_used_bytes: u64,
    /// Total bytes of the filesystem holding the VM images.
    pub disk_total_bytes: u64,
    /// Bytes in use on that filesystem.
    pub disk_used_bytes: u64,
    /// Names of the counts above that could not be measured on this host.
    pub unmeasured: Vec<&'static str>,
}

/// Reads [`HostCapacity`] for this host, using `disk_path` for the filesystem
/// figures (the provider passes the directory its VM images live in).
pub fn capacity(disk_path: &std::path::Path) -> HostCapacity {
    let mut cap = HostCapacity::default();
    match std::fs::read_to_string("/proc/cpuinfo") {
        Ok(text) => {
            let (cpus, model) = parse_cpuinfo(&text);
            if cpus > 0 {
                cap.cpus = cpus;
            } else {
                cap.unmeasured.push("cpus");
            }
            match model {
                Some(m) => cap.cpu_model = m,
                None => cap.unmeasured.push("cpu_model"),
            }
        }
        Err(_) => cap.unmeasured.extend(["cpus", "cpu_model"]),
    }
    match std::fs::read_to_string("/proc/meminfo") {
        Ok(text) => match parse_meminfo_bytes(&text) {
            Some(m) => {
                cap.mem_total_bytes = m.mem_total;
                cap.mem_used_bytes = m.mem_total.saturating_sub(m.mem_available);
                cap.swap_total_bytes = m.swap_total;
                cap.swap_used_bytes = m.swap_total.saturating_sub(m.swap_free);
            }
            None => cap
                .unmeasured
                .extend(["mem_total", "mem_used", "swap_total", "swap_used"]),
        },
        Err(_) => cap
            .unmeasured
            .extend(["mem_total", "mem_used", "swap_total", "swap_used"]),
    }
    match statvfs_bytes(disk_path) {
        Some((total, used)) => {
            cap.disk_total_bytes = total;
            cap.disk_used_bytes = used;
        }
        None => cap.unmeasured.extend(["disk_total", "disk_used"]),
    }
    cap
}

/// Logical CPU count (`processor` lines) and the first `model name`, out of the
/// text of `/proc/cpuinfo`. Pure.
fn parse_cpuinfo(text: &str) -> (u64, Option<String>) {
    let cpus = text
        .lines()
        .filter(|l| l.split_once(':').map(|(k, _)| k.trim() == "processor") == Some(true))
        .count() as u64;
    let model = text
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == "model name"))
        .map(|(_, v)| v.trim().to_string())
        .filter(|m| !m.is_empty());
    (cpus, model)
}

struct MemInfo {
    mem_total: u64,
    mem_available: u64,
    swap_total: u64,
    swap_free: u64,
}

/// The four byte counts out of the text of `/proc/meminfo` (its values are kB).
/// `None` when `MemTotal`/`MemAvailable` are absent — a meminfo without them is
/// not a measurement. Swap lines absent means a host with no swap: a real `0`.
/// Pure.
fn parse_meminfo_bytes(text: &str) -> Option<MemInfo> {
    let kib = |key: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(key))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    };
    Some(MemInfo {
        mem_total: kib("MemTotal:")?.saturating_mul(1024),
        mem_available: kib("MemAvailable:")?.saturating_mul(1024),
        swap_total: kib("SwapTotal:").unwrap_or(0).saturating_mul(1024),
        swap_free: kib("SwapFree:").unwrap_or(0).saturating_mul(1024),
    })
}

/// `(total, used)` bytes of the filesystem holding `path`, via `statvfs(3)`.
/// `None` when the path cannot be stat'd (absent store, unreadable) — surfaced
/// as unmeasured, never as zero bytes.
fn statvfs_bytes(path: &std::path::Path) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: `statvfs` fills a plain C struct of integers; all-zero is valid.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path; `st` is our out-param buffer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    let total = (st.f_blocks as u64).saturating_mul(frsize);
    let used = (st.f_blocks.saturating_sub(st.f_bfree) as u64).saturating_mul(frsize);
    (total > 0).then_some((total, used))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// The reader on the REAL host: a machine always has at least one CPU, some
    /// total RAM and a filesystem under the state root — and "in use" never
    /// exceeds "total" for any of the three. The parallel of Proxmox's
    /// `the_node_reports_its_cpu_memory_and_disk_capacity`, but of THIS host.
    #[test]
    fn host_capacity_reads_the_local_host() {
        let cap = super::capacity(Path::new("/"));
        assert!(cap.cpus >= 1, "the host has at least one CPU: {cap:?}");
        assert!(cap.mem_total_bytes > 0, "the host reports total RAM: {cap:?}");
        assert!(
            cap.mem_used_bytes <= cap.mem_total_bytes,
            "RAM in use never exceeds total: {cap:?}"
        );
        assert!(
            cap.swap_used_bytes <= cap.swap_total_bytes,
            "swap in use never exceeds total (both 0 without swap): {cap:?}"
        );
        assert!(
            cap.disk_total_bytes > 0,
            "the host reports the VM-image disk: {cap:?}"
        );
        assert!(
            cap.disk_used_bytes <= cap.disk_total_bytes,
            "disk in use never exceeds total: {cap:?}"
        );
    }

    /// `/proc/cpuinfo` parsing: one count per `processor` line, the first
    /// `model name`, both pure.
    #[test]
    fn cpuinfo_counts_processors_and_names_the_model() {
        let text = "processor\t: 0\nmodel name\t: Fake CPU @ 3GHz\nprocessor\t: 1\nmodel name\t: Fake CPU @ 3GHz\n";
        let (cpus, model) = super::parse_cpuinfo(text);
        assert_eq!(cpus, 2);
        assert_eq!(model.as_deref(), Some("Fake CPU @ 3GHz"));
        // No processor lines and no model: zero and None (caller marks unmeasured).
        assert_eq!(super::parse_cpuinfo("bogus\n"), (0, None));
    }

    /// `/proc/meminfo` kB → bytes; absent swap is a real zero, absent MemTotal
    /// is `None` (not a measurement).
    #[test]
    fn meminfo_converts_kb_to_bytes_and_tolerates_missing_swap() {
        let text = "MemTotal:       32000 kB\nMemFree: 1000 kB\nMemAvailable:   20000 kB\n";
        let m = super::parse_meminfo_bytes(text).expect("MemTotal+MemAvailable present");
        assert_eq!(m.mem_total, 32_000 * 1024);
        assert_eq!(m.mem_available, 20_000 * 1024);
        assert_eq!(m.swap_total, 0, "no SwapTotal line is a real zero");
        assert!(super::parse_meminfo_bytes("MemFree: 1 kB\n").is_none());
    }

    /// `MemAvailable` in kB becomes MiB; a text without the line is `None`,
    /// which the VM admission reads as «cannot tell» and does not block on.
    #[test]
    fn mem_available_is_read_from_meminfo() {
        let text = "MemTotal:       32000000 kB\nMemFree:  1000 kB\nMemAvailable:   20480000 kB\n";
        assert_eq!(super::parse_mem_available_mib(text), Some(20000));
        assert_eq!(super::parse_mem_available_mib("MemTotal: 1 kB\n"), None);
        assert_eq!(
            super::parse_mem_available_mib("MemAvailable: lots kB\n"),
            None
        );
    }
}
