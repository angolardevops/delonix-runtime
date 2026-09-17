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
