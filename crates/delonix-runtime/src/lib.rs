//! `delonix-runtime` — the Delonix Engine's low-level OCI runtime.
//!
//! It is Month 5's `mini-runc`, promoted to a library: it creates containers with
//! `clone` (namespaces) + `pivot_root` (rootfs) + cgroup (memory) + seccomp
//! (confinement), runs commands inside an existing container with `setns`
//! (`exec`), and manages their lifecycle (`stop`/`remove`).
//!
//! The entire `syscall` boundary lives here; the rest of Delonix never touches it.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

use delonix_runtime_core::{Container, Error, Mount, Result, Status, Store};

/// RFC3339 with nanosecond precision, for the *logging shim* (timestamped
/// container stdout). Deliberate local copy: `delonix-runtime` does not
/// depend on `delonix-core` (PaaS) — this helper is purely time formatting,
/// with no audit/tenant semantics.
fn now_rfc3339_nano() -> String {
    fn rfc3339(secs: u64) -> String {
        let days = (secs / 86_400) as i64;
        let rem = secs % 86_400;
        let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if m <= 2 { y + 1 } else { y };
        format!("{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let base = rfc3339(now.as_secs());
    format!("{}.{:09}Z", &base[..base.len() - 1], now.subsec_nanos())
}

use nix::fcntl::{open, OFlag};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{clone, setns, unshare, CloneFlags};
use nix::sys::signal::{kill, Signal};
use nix::sys::stat::Mode;
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{chdir, execvp, fork, pivot_root, sethostname, ForkResult, Pid};

use seccompiler::{
    apply_filter, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};

/// A *hook* invoked with the init PID, right after the container starts and before
/// any `waitpid`. Phase 3 uses it to configure the network (CNI-style).
pub type StartedHook<'a> = dyn Fn(i32) -> Result<()> + 'a;

fn syserr(context: &'static str) -> impl Fn(nix::Error) -> Error {
    move |e| Error::Runtime {
        context,
        message: e.to_string(),
    }
}

/// `true` if process `pid` still exists (signal 0 = only tests liveness).
pub fn is_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
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

/// Short, stable reason code for WHY `safe_to_signal` failed — the two cases it
/// collapses into one bool. Precondition: only meaningful when `safe_to_signal(pid,
/// starttime)` is `false`; called right after that check fails, in `reconcile_status`.
pub fn crash_reason_of(pid: i32, _starttime: Option<u64>) -> &'static str {
    if !is_alive(pid) {
        "process_gone"
    } else {
        // is_alive was true, so safe_to_signal only failed because the starttime
        // didn't match: the kernel recycled the pid for an unrelated process.
        "pid_reused"
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn wait_to_code(status: WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, sig, _) => 128 + sig as i32,
        _ => -1,
    }
}

/// Converts the `WaitStatus` into the correct terminal [`Status`]: exit 0 → Stopped,
/// exit ≠ 0 → Failed, killed by signal → Crashed.
fn wait_to_status(status: WaitStatus) -> Status {
    match status {
        WaitStatus::Exited(_, 0) => Status::Stopped,
        WaitStatus::Exited(_, code) => Status::Failed(code),
        WaitStatus::Signaled(..) => Status::Crashed,
        _ => Status::Crashed,
    }
}

// ----------------------------------------------------------------------------
// Create and run a container
// ----------------------------------------------------------------------------

/// A per-argument seccomp rule: the syscall matches (and is blocked) when
/// `(arg[index] & mask) == (value & mask)`.
fn rule_arg_masked(index: u8, mask: u64, value: u64) -> SeccompRule {
    SeccompRule::new(vec![SeccompCondition::new(
        index,
        SeccompCmpArgLen::Qword,
        SeccompCmpOp::MaskedEq(mask),
        value,
    )
    .expect("seccomp condition")])
    .expect("seccomp rule")
}

/// Installs a seccomp filter: blocks (with `EPERM`) the unconditional blacklist
/// PLUS per-argument rules that refine legitimate cases. Enables `no_new_privs`.
/// Loads the seccomp filter with `SECCOMP_FILTER_FLAG_LOG`: every DENIED syscall
/// (outside the allowlist) is **logged** to the kernel audit/dmesg — Falco-style
/// runtime detection (B12) — while still blocking it (still `EPERM`).
fn apply_filter_logged(prog: &seccompiler::BpfProgram) {
    const SET_MODE_FILTER: libc::c_ulong = 1;
    const FLAG_LOG: libc::c_ulong = 2;
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_ptr() as *mut libc::sock_filter,
    };
    // SAFETY: `fprog` points to a valid BPF program; NO_NEW_PRIVS is already set.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SET_MODE_FILTER,
            FLAG_LOG,
            &fprog as *const _,
        )
    };
    if rc != 0 {
        // last resort: apply without the log flag (security is preserved).
        let _ = apply_filter(prog);
    }
}

fn apply_seccomp(unconfined: bool, detect: bool) {
    if unconfined {
        return; // `--security-opt seccomp=unconfined`: no filter (trusted use)
    }
    // ALLOWLIST (default-deny): only safe syscalls are permitted; everything
    // else — incl. FUTURE/unknown syscalls — returns EPERM. Docker model.
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for nr in allowed_syscalls() {
        rules.insert(nr, vec![]); // unconditional match -> on_match (Allow)
    }
    // `clone`: permit ONLY when it does NOT create a new USER namespace (prevents
    // escape via nested userns).
    rules.insert(
        libc::SYS_clone,
        vec![rule_arg_masked(0, libc::CLONE_NEWUSER as u64, 0)], // NEWUSER not set
    );

    let arch = match std::env::consts::ARCH.try_into() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("delonix: architecture without seccomp support; aborting the container");
            unsafe { libc::_exit(126) };
        }
    };

    // CRITICAL pre-filter: `clone3` → ENOSYS. The clone3 flags go in a pointer
    // (`struct clone_args`) and can NOT be inspected by classic seccomp,
    // so a clone3(CLONE_NEWUSER) would bypass the userns block above.
    // By returning ENOSYS we force glibc to fall back to `clone` (which IS filtered).
    // ENOSYS (ERRNO) takes precedence over the main filter's Allow, so it
    // wins even with clone3 still on the allowlist (needed for threads via clone).
    //
    // It is ALWAYS installed, **including in `detect`**: the `detect` mode tunes the *log*
    // of denied syscalls (FLAG_LOG on the main filter), it does NOT loosen the
    // confinement. If this pre-filter only ran with `!detect`, a container with
    // `--security-opt seccomp=detect` could `clone3(CLONE_NEWUSER)` and escape
    // via nested userns — exactly the hole the rest of the filter closes. The
    // userns attempt that results falls into the filtered `clone` (logged in the
    // main filter), so no detection is lost.
    let mut pre: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    pre.insert(libc::SYS_clone3, vec![]);
    if let Ok(pf) = SeccompFilter::new(
        pre,
        SeccompAction::Allow,                      // not matched: let it through
        SeccompAction::Errno(libc::ENOSYS as u32), // clone3 → ENOSYS
        arch,
    ) {
        if let Ok(pp) = TryInto::<seccompiler::BpfProgram>::try_into(pf) {
            let _ = apply_filter(&pp);
        }
    }

    let prog: seccompiler::BpfProgram = match SeccompFilter::new(
        rules,
        SeccompAction::Errno(libc::EPERM as u32), // by default (not matched): EPERM
        SeccompAction::Allow,                     // matched (on the allowlist): permit
        arch,
    )
    .and_then(|f| f.try_into())
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("delonix: failed to build the seccomp filter: {e}; aborting");
            unsafe { libc::_exit(126) };
        }
    };
    if detect {
        apply_filter_logged(&prog); // B12: logs denied syscalls
    } else if let Err(e) = apply_filter(&prog) {
        eprintln!("delonix: failed to apply seccomp: {e}; aborting the container");
        unsafe { libc::_exit(126) };
    }
}

/// Allowlist of safe syscalls (based on Docker's default profile, for
/// x86_64). `clone` is handled separately (conditional). The dangerous ones (mount, ptrace,
/// bpf, kexec, init_module, setns, unshare, …) are LEFT OUT = denied.
fn allowed_syscalls() -> Vec<i64> {
    use libc::*;
    // PORTABLE allowlist (exists on x86_64 and aarch64). The legacy syscalls
    // that only exist on x86_64 (old open/stat/fork/*at…) are added
    // conditionally — on aarch64 the `*at`/`clone` variants are used.
    let mut v: Vec<i64> = vec![
        // files / FS
        SYS_read,
        SYS_write,
        SYS_openat,
        SYS_close,
        SYS_close_range,
        SYS_fstat,
        SYS_newfstatat,
        SYS_statx,
        SYS_ppoll,
        SYS_lseek,
        SYS_pread64,
        SYS_pwrite64,
        SYS_readv,
        SYS_writev,
        SYS_preadv,
        SYS_pwritev,
        SYS_preadv2,
        SYS_pwritev2,
        SYS_faccessat,
        SYS_faccessat2,
        SYS_dup,
        SYS_dup3,
        SYS_pipe2,
        SYS_fcntl,
        SYS_flock,
        SYS_fsync,
        SYS_fdatasync,
        SYS_truncate,
        SYS_ftruncate,
        SYS_getdents64,
        SYS_getcwd,
        SYS_chdir,
        SYS_fchdir,
        SYS_renameat,
        SYS_renameat2,
        SYS_mkdirat,
        SYS_linkat,
        SYS_unlinkat,
        SYS_symlinkat,
        SYS_readlinkat,
        SYS_fchmod,
        SYS_fchmodat,
        SYS_fchown,
        SYS_fchownat,
        SYS_umask,
        SYS_utimensat,
        SYS_statfs,
        SYS_fstatfs,
        SYS_sync,
        SYS_syncfs,
        SYS_sync_file_range,
        SYS_fallocate,
        SYS_readahead,
        SYS_openat2,
        SYS_mknodat,
        SYS_splice,
        SYS_tee,
        SYS_vmsplice,
        SYS_copy_file_range,
        // xattr
        SYS_getxattr,
        SYS_lgetxattr,
        SYS_fgetxattr,
        SYS_setxattr,
        SYS_lsetxattr,
        SYS_fsetxattr,
        SYS_listxattr,
        SYS_llistxattr,
        SYS_flistxattr,
        SYS_removexattr,
        SYS_lremovexattr,
        SYS_fremovexattr,
        // memory
        SYS_mmap,
        SYS_munmap,
        SYS_mprotect,
        SYS_mremap,
        SYS_msync,
        SYS_mincore,
        SYS_madvise,
        SYS_brk,
        SYS_mlock,
        SYS_munlock,
        SYS_mlockall,
        SYS_munlockall,
        SYS_mlock2,
        SYS_memfd_create,
        SYS_membarrier,
        // processes / threads
        SYS_clone3,
        SYS_execve,
        SYS_execveat,
        SYS_exit,
        SYS_exit_group,
        SYS_wait4,
        SYS_waitid,
        SYS_kill,
        SYS_tgkill,
        SYS_tkill,
        SYS_getpid,
        SYS_getppid,
        SYS_gettid,
        SYS_set_tid_address,
        SYS_set_robust_list,
        SYS_get_robust_list,
        SYS_rseq,
        SYS_futex,
        SYS_prctl,
        SYS_personality,
        SYS_getrandom,
        SYS_uname,
        SYS_sysinfo,
        SYS_getcpu,
        SYS_capget,
        SYS_capset,
        // ids / credentials (no extra privilege — NO_NEW_PRIVS+caps already limit)
        SYS_getuid,
        SYS_geteuid,
        SYS_getgid,
        SYS_getegid,
        SYS_setuid,
        SYS_setgid,
        SYS_setreuid,
        SYS_setregid,
        SYS_setresuid,
        SYS_setresgid,
        SYS_getresuid,
        SYS_getresgid,
        SYS_setfsuid,
        SYS_setfsgid,
        SYS_getgroups,
        SYS_setgroups,
        SYS_getpgid,
        SYS_setpgid,
        SYS_getsid,
        SYS_setsid,
        SYS_getpriority,
        SYS_setpriority,
        // limits / scheduling
        SYS_getrlimit,
        SYS_setrlimit,
        SYS_prlimit64,
        SYS_getrusage,
        SYS_sched_yield,
        SYS_sched_getaffinity,
        SYS_sched_setaffinity,
        SYS_sched_getparam,
        SYS_sched_setparam,
        SYS_sched_getscheduler,
        SYS_sched_setscheduler,
        SYS_sched_get_priority_max,
        SYS_sched_get_priority_min,
        SYS_sched_rr_get_interval,
        // signals
        SYS_rt_sigaction,
        SYS_rt_sigprocmask,
        SYS_rt_sigpending,
        SYS_rt_sigtimedwait,
        SYS_rt_sigqueueinfo,
        SYS_rt_sigreturn,
        SYS_rt_sigsuspend,
        SYS_sigaltstack,
        SYS_signalfd4,
        SYS_restart_syscall,
        // time / timers
        SYS_nanosleep,
        SYS_clock_nanosleep,
        SYS_clock_gettime,
        SYS_clock_getres,
        SYS_gettimeofday,
        SYS_times,
        SYS_timer_create,
        SYS_timer_settime,
        SYS_timer_gettime,
        SYS_timer_getoverrun,
        SYS_timer_delete,
        SYS_timerfd_create,
        SYS_timerfd_settime,
        SYS_timerfd_gettime,
        SYS_getitimer,
        SYS_setitimer,
        // epoll / eventfd / inotify
        SYS_pselect6,
        SYS_epoll_create1,
        SYS_epoll_ctl,
        SYS_epoll_pwait,
        SYS_eventfd2,
        SYS_inotify_init1,
        SYS_inotify_add_watch,
        SYS_inotify_rm_watch,
        // classic AIO (libaio) — nginx & co use it; Docker permits it by
        // default. (io_uring is LEFT OUT, as in Docker, being more sensitive.)
        SYS_io_setup,
        SYS_io_destroy,
        SYS_io_getevents,
        SYS_io_submit,
        SYS_io_cancel,
        // network
        SYS_socket,
        SYS_socketpair,
        SYS_bind,
        SYS_listen,
        SYS_accept,
        SYS_accept4,
        SYS_connect,
        SYS_getsockname,
        SYS_getpeername,
        SYS_sendto,
        SYS_recvfrom,
        SYS_sendmsg,
        SYS_recvmsg,
        SYS_sendmmsg,
        SYS_recvmmsg,
        SYS_shutdown,
        SYS_setsockopt,
        SYS_getsockopt,
        // IPC (System V + POSIX mq)
        SYS_shmget,
        SYS_shmat,
        SYS_shmdt,
        SYS_shmctl,
        SYS_semget,
        SYS_semop,
        SYS_semctl,
        SYS_semtimedop,
        SYS_msgget,
        SYS_msgsnd,
        SYS_msgrcv,
        SYS_msgctl,
        SYS_mq_open,
        SYS_mq_unlink,
        SYS_mq_timedsend,
        SYS_mq_timedreceive,
        SYS_mq_notify,
        SYS_mq_getsetattr,
        // ioctl
        SYS_ioctl,
    ];
    #[cfg(target_arch = "x86_64")]
    v.extend_from_slice(&[
        SYS_access,
        SYS_alarm,
        SYS_arch_prctl,
        SYS_chmod,
        SYS_chown,
        SYS_dup2,
        SYS_epoll_create,
        SYS_epoll_wait,
        SYS_eventfd,
        SYS_fadvise64,
        SYS_fork,
        SYS_futimesat,
        SYS_getdents,
        SYS_getpgrp,
        SYS_inotify_init,
        SYS_lchown,
        SYS_link,
        SYS_lstat,
        SYS_mkdir,
        SYS_mknod,
        SYS_open,
        SYS_pause,
        SYS_pipe,
        SYS_poll,
        SYS_readlink,
        SYS_rename,
        SYS_rmdir,
        SYS_select,
        SYS_sendfile,
        SYS_signalfd,
        SYS_stat,
        SYS_symlink,
        SYS_time,
        SYS_unlink,
        SYS_utime,
        SYS_utimes,
        SYS_vfork,
    ]);
    v
}

/// The capabilities the container MAY keep (Docker's model, minus
/// `CAP_MKNOD` — without a device cgroup, this is how we prevent access to host
/// disks). Everything else is dropped.
const KEPT_CAPS: &[u8] = &[
    0,  // CHOWN
    1,  // DAC_OVERRIDE
    3,  // FOWNER
    4,  // FSETID
    5,  // KILL
    6,  // SETGID
    7,  // SETUID
    8,  // SETPCAP
    10, // NET_BIND_SERVICE
    11, // NET_BROADCAST
    13, // NET_RAW
    18, // SYS_CHROOT
    29, // AUDIT_WRITE
    31, // SETFCAP
];

/// Capability number from the name (`CAP_NET_ADMIN` or `NET_ADMIN`).
fn cap_num(name: &str) -> Option<u8> {
    let n = name.trim().to_ascii_uppercase();
    let n = n.strip_prefix("CAP_").unwrap_or(&n);
    Some(match n {
        "CHOWN" => 0,
        "DAC_OVERRIDE" => 1,
        "DAC_READ_SEARCH" => 2,
        "FOWNER" => 3,
        "FSETID" => 4,
        "KILL" => 5,
        "SETGID" => 6,
        "SETUID" => 7,
        "SETPCAP" => 8,
        "LINUX_IMMUTABLE" => 9,
        "NET_BIND_SERVICE" => 10,
        "NET_BROADCAST" => 11,
        "NET_ADMIN" => 12,
        "NET_RAW" => 13,
        "IPC_LOCK" => 14,
        "IPC_OWNER" => 15,
        "SYS_MODULE" => 16,
        "SYS_RAWIO" => 17,
        "SYS_CHROOT" => 18,
        "SYS_PTRACE" => 19,
        "SYS_PACCT" => 20,
        "SYS_ADMIN" => 21,
        "SYS_BOOT" => 22,
        "SYS_NICE" => 23,
        "SYS_RESOURCE" => 24,
        "SYS_TIME" => 25,
        "SYS_TTY_CONFIG" => 26,
        "MKNOD" => 27,
        "LEASE" => 28,
        "AUDIT_WRITE" => 29,
        "AUDIT_CONTROL" => 30,
        "SETFCAP" => 31,
        "MAC_OVERRIDE" => 32,
        "MAC_ADMIN" => 33,
        "SYSLOG" => 34,
        "WAKE_ALARM" => 35,
        "BLOCK_SUSPEND" => 36,
        "AUDIT_READ" => 37,
        "PERFMON" => 38,
        "BPF" => 39,
        "CHECKPOINT_RESTORE" => 40,
        _ => return None,
    })
}

/// Mask with ALL capabilities supported by the kernel (`--privileged`).
/// Reads `/proc/sys/kernel/cap_last_cap` so as not to pass invalid bits to `capset`
/// (which would give EINVAL). Conservative fallback: CAP_CHECKPOINT_RESTORE (40).
fn all_caps_mask() -> u64 {
    let last: u32 = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(40);
    let last = last.min(63);
    if last >= 63 {
        u64::MAX
    } else {
        (1u64 << (last + 1)) - 1
    }
}

/// Computes the mask of capabilities to keep: starts at [`KEPT_CAPS`], applies
/// `--cap-drop` (`ALL` → none) and then `--cap-add`.
fn resolve_cap_keep(cap_drop: &[String], cap_add: &[String]) -> u64 {
    let mut keep: u64 = if cap_drop.iter().any(|c| c.eq_ignore_ascii_case("all")) {
        0
    } else {
        let mut m = 0u64;
        for &c in KEPT_CAPS {
            m |= 1u64 << c;
        }
        for c in cap_drop {
            if let Some(n) = cap_num(c) {
                m &= !(1u64 << n);
            }
        }
        m
    };
    // `--cap-add ALL` (docker) / the CRI translation of `privileged` → keeps ALL
    // capabilities. Without this branch, `cap_num("ALL")` returned `None` and `ALL` was
    // silently ignored — a "privileged" container via CRI ended up without
    // CAP_SYS_ADMIN (e.g. `sethostname` gave EPERM even though CRI requested it).
    if cap_add.iter().any(|c| c.eq_ignore_ascii_case("all")) {
        return all_caps_mask();
    }
    for c in cap_add {
        if let Some(n) = cap_num(c) {
            keep |= 1u64 << n;
        }
    }
    keep
}

/// Enables `NO_NEW_PRIVS`: an `execve` never gains privileges (nullifies setuid/
/// setgid/file capabilities). Key defense against escalation — always active.
fn set_no_new_privs() {
    // SAFETY: simple prctl; idempotent; does not fail on supported kernels.
    unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }
}

/// Drops all capabilities except `keep` (mask). Without this, the container's
/// root is the REAL host root (can load modules, reboot the machine,
/// create device nodes for the host disk, etc.).
fn drop_capabilities(keep: u64) {
    // 1) bounding set: prevents reacquiring caps via setuid/exec.
    for cap in 0..64i64 {
        if (keep >> cap) & 1 == 0 {
            // SAFETY: prctl is safe; nonexistent caps return EINVAL (ignored).
            unsafe {
                libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0);
            }
        }
    }
    // 2) effective/permitted/inheritable: only the allowlist ones remain.
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let hdr = CapHeader {
        version: 0x2008_0522, // _LINUX_CAPABILITY_VERSION_3
        pid: 0,               // the thread itself
    };
    let (lo, hi) = ((keep & 0xffff_ffff) as u32, (keep >> 32) as u32);
    let data = [
        CapData {
            effective: lo,
            permitted: lo,
            inheritable: 0,
        },
        CapData {
            effective: hi,
            permitted: hi,
            inheritable: 0,
        },
    ];
    // SAFETY: capset with a valid v3 header and 2 data structs; reducing one's
    // own caps to a subset is always permitted.
    unsafe {
        libc::syscall(libc::SYS_capset, &hdr as *const _, data.as_ptr());
    }
}

/// Decides whether the confinement REALLY took effect, from the fields read from
/// `/proc/self/status`. Pure logic (testable): requires `NO_NEW_PRIVS`, seccomp in
/// filter mode (2) when expected, and NO capability outside `cap_keep` in the
/// bounding set nor in the effective set. `CapBnd`/`CapEff` absent = unverifiable =
/// error (fail-closed).
fn confinement_ok(
    no_new_privs: Option<u32>,
    seccomp_mode: Option<u32>,
    cap_bnd: Option<u64>,
    cap_eff: Option<u64>,
    seccomp_expected: bool,
    cap_keep: u64,
) -> std::result::Result<(), String> {
    if no_new_privs != Some(1) {
        return Err(format!("NO_NEW_PRIVS inactive ({no_new_privs:?})"));
    }
    // 2 = SECCOMP_MODE_FILTER. (`detect` also applies a filter → mode 2.)
    if seccomp_expected && seccomp_mode != Some(2) {
        return Err(format!(
            "seccomp is not in filter mode (Seccomp={seccomp_mode:?})"
        ));
    }
    let bnd = cap_bnd.ok_or_else(|| "CapBnd missing from /proc/self/status".to_string())?;
    let eff = cap_eff.ok_or_else(|| "CapEff missing from /proc/self/status".to_string())?;
    let extra_bnd = bnd & !cap_keep;
    let extra_eff = eff & !cap_keep;
    if extra_bnd != 0 || extra_eff != 0 {
        return Err(format!(
            "capabilities outside the allowlist persist (bnd_extra={extra_bnd:#x} eff_extra={extra_eff:#x})"
        ));
    }
    Ok(())
}

/// FAIL-CLOSED: reads `/proc/self/status` and confirms that `no_new_privs`, seccomp and the
/// cap drop REALLY took effect before the `execve`. Each of these controls
/// can fail silently (capset/prctl/seccomp are, in part, best-effort); a
/// security control that fails OPEN is worse than none, because it gives false
/// confidence. If the check fails, `container_init`/`exec` aborts. Explicit opt-out
/// by the OPERATOR (not the container, whose env has not yet been applied):
/// `DELONIX_INSECURE_BESTEFFORT=1`.
fn verify_confinement(seccomp_expected: bool, cap_keep: u64) -> std::result::Result<(), String> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("/proc/self/status unreadable: {e}"))?;
    let (mut nnp, mut sec, mut bnd, mut eff) = (None, None, None, None);
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("NoNewPrivs:") {
            nnp = v.trim().parse::<u32>().ok();
        } else if let Some(v) = line.strip_prefix("Seccomp:") {
            sec = v.trim().parse::<u32>().ok();
        } else if let Some(v) = line.strip_prefix("CapBnd:") {
            bnd = u64::from_str_radix(v.trim(), 16).ok();
        } else if let Some(v) = line.strip_prefix("CapEff:") {
            eff = u64::from_str_radix(v.trim(), 16).ok();
        }
    }
    confinement_ok(nnp, sec, bnd, eff, seccomp_expected, cap_keep)
}

/// Did the operator turn off the fail-closed checks? (ENGINE variable, read before
/// `apply_env` clears the environment — a container cannot forge it.)
fn insecure_besteffort() -> bool {
    std::env::var_os("DELONIX_INSECURE_BESTEFFORT").is_some()
}

/// Detaches the terminal stdio in detached mode. `stdin` always goes to
/// `/dev/null`; `stdout`/`stderr` go to `out_fd` (the write end of the *logging
/// shim* pipe) if given, otherwise to `/dev/null`.
///
/// Without this, whoever invoked `$(delonix run -d ...)` would block until the
/// container closed its stdout — that is, until it terminated.
fn detach_stdio(out_fd: Option<i32>) {
    // SAFETY: direct FFI; duplicates the standard descriptors. Best-effort.
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            libc::dup2(null, 0);
        }
        let out = out_fd.unwrap_or(null);
        if out >= 0 {
            libc::dup2(out, 1);
            libc::dup2(out, 2);
        }
        if null > 2 {
            libc::close(null);
        }
        // the original pipe end is no longer needed (it was dup'd into 1/2).
        if let Some(fd) = out_fd {
            if fd > 2 {
                libc::close(fd);
            }
        }
    }
}

/// Maximum log file size before rotating (1 MiB).
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// The *logging shim*: reads the container's stdout/stderr (via the pipe's read
/// end) and writes it to `log_path`, **rotating** when it exceeds [`MAX_LOG_BYTES`]
/// (renames to `.1` and starts over). Runs in its own process that outlives
/// `delonix run` (reparented to init) and terminates when the container closes the pipe.
// `written` is written at the end of the `write_block!` macro and read at the START of the
// NEXT iteration (rotation accounting across calls); rustc's flow analysis
// does not see that cross-iteration read and marks the last write as "unused".
#[allow(unused_assignments)]
fn log_shim(
    read_fd: i32,
    log_path: String,
    max_bytes: u64,
    driver: String,
    tag: String,
    cri: bool,
) -> ! {
    // journald/syslog driver: forwards each line to syslog (which journald
    // captures), instead of the file. `--log-driver journald|syslog`.
    if driver == "journald" || driver == "syslog" {
        log_shim_syslog(read_fd, tag);
    }
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    // SAFETY: `read_fd` is the pipe's read end, inherited and valid.
    let mut reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let open_log = |append: bool| {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(&log_path)
    };
    let mut out = open_log(true);
    let mut written: u64 = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
    let mut buf = [0u8; 8192];
    // In CRI mode each line comes out as `<rfc3339nano> stdout F <line>\n` — which the
    // kubelet/crictl know how to parse. Accumulates up to `\n` (lines may arrive split
    // across `read`s). Cap so a line without `\n` does not grow RAM without bound.
    let mut line = Vec::<u8>::new();
    const MAX_LINE: usize = 256 * 1024;
    // Writes a block, rotating BEFORE if it would exceed `max_bytes` — in CRI mode
    // this is called only with COMPLETE records, so rotation never splits a
    // record in the middle (and the count includes the prefix, unlike before).
    macro_rules! write_block {
        ($bytes:expr) => {{
            let b: &[u8] = $bytes;
            if written + b.len() as u64 > max_bytes && written > 0 {
                drop(out);
                let _ = std::fs::rename(&log_path, format!("{log_path}.1"));
                out = open_log(false);
                written = 0;
            }
            if let Ok(f) = out.as_mut() {
                let _ = f.write_all(b);
            }
            written += b.len() as u64;
        }};
    }
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break, // EOF: the container closed the pipe (terminated)
            Ok(n) => n,
        };
        if !cri {
            write_block!(&buf[..n]);
            continue;
        }
        for &b in &buf[..n] {
            line.push(b);
            let full = b == b'\n';
            if full || line.len() >= MAX_LINE {
                let ts = now_rfc3339_nano();
                let stream = if full { "F" } else { "P" };
                let body = String::from_utf8_lossy(line.strip_suffix(b"\n").unwrap_or(&line));
                let rec = format!("{ts} stdout {stream} {body}\n");
                write_block!(rec.as_bytes());
                line.clear();
            }
        }
    }
    // final line without `\n` (CRI mode) — emit it anyway as partial.
    if cri && !line.is_empty() {
        let ts = now_rfc3339_nano();
        let rec = format!("{ts} stdout P {}\n", String::from_utf8_lossy(&line));
        write_block!(rec.as_bytes());
    }
    // SAFETY: exits without running destructors inherited from the parent process.
    unsafe { libc::_exit(0) }
}

/// Variant of the *logging shim* that writes each line to **syslog** (captured
/// by journald on systemd systems). `tag` = `delonix/<name>`.
fn log_shim_syslog(read_fd: i32, tag: String) -> ! {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    // the tag must live as long as syslog is open -> deliberate leak.
    let ctag = std::ffi::CString::new(tag).unwrap_or_default();
    // SAFETY: openlog with a valid pointer that outlives the process (leaked).
    unsafe {
        libc::openlog(
            Box::leak(ctag.into_boxed_c_str()).as_ptr(),
            libc::LOG_PID,
            libc::LOG_USER,
        )
    };
    // SAFETY: `read_fd` is the pipe's read end.
    let mut reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let mut buf = [0u8; 8192];
    let mut line = Vec::new();
    let fmt = c"%s";
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for &b in &buf[..n] {
                    if b == b'\n' {
                        if let Ok(c) = std::ffi::CString::new(line.clone()) {
                            // SAFETY: format and argument are valid C pointers.
                            unsafe { libc::syslog(libc::LOG_INFO, fmt.as_ptr(), c.as_ptr()) };
                        }
                        line.clear();
                    } else {
                        line.push(b);
                    }
                }
            }
        }
    }
    if !line.is_empty() {
        if let Ok(c) = std::ffi::CString::new(line) {
            unsafe { libc::syslog(libc::LOG_INFO, fmt.as_ptr(), c.as_ptr()) };
        }
    }
    // SAFETY: exits without running inherited destructors.
    unsafe { libc::_exit(0) }
}

/// Mounts a volume/bind into the rootfs (before `pivot_root`). Zero-copy: the
/// `MS_BIND` shares `source`'s blocks, it does not copy data.
/// Is a bind-mount `target` safe? (absolute and WITHOUT `..` components). Defense
/// against escape: `bind_volume` runs before `pivot_root`, so a relative/`..`
/// target would mount over the HOST filesystem.
fn mount_target_safe(target: &str) -> bool {
    let p = std::path::Path::new(target);
    p.is_absolute()
        && !p
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

fn bind_volume(rootfs: &str, m: &Mount) -> nix::Result<()> {
    if !mount_target_safe(&m.target) {
        return Err(nix::errno::Errno::EINVAL);
    }
    let dst = format!("{rootfs}{}", m.target);
    // File source (e.g. secret) → the target must be a FILE; directory source
    // → a directory.
    if std::path::Path::new(&m.source).is_file() {
        if let Some(parent) = std::path::Path::new(&dst).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&dst);
    } else {
        let _ = std::fs::create_dir_all(&dst);
    }
    mount(
        Some(m.source.as_str()),
        dst.as_str(),
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )?;
    // Remount to apply `nosuid`+`nodev` — a bind ignores these flags on the first
    // `mount`, so without this a volume could bring setuid binaries or device
    // nodes into the container. Additional `rdonly` if requested. (`noexec` NOT,
    // so as not to break volumes with legitimate executables, e.g. code.)
    let mut rflags =
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_NOSUID | MsFlags::MS_NODEV;
    if m.readonly {
        rflags |= MsFlags::MS_RDONLY;
    }
    mount(
        None::<&str>,
        dst.as_str(),
        None::<&str>,
        rflags,
        None::<&str>,
    )?;
    Ok(())
}

/// The essential device nodes every container should have in `/dev`.
const ESSENTIAL_DEVS: &[&str] = &["null", "zero", "full", "random", "urandom", "tty"];

/// Mounts a clean `/dev` (tmpfs) and wires up the host's essential device nodes.
///
/// Without this, the image brings an empty `/dev` and, with a user namespace, the container
/// cannot even create files there (the `/dev` belongs to an unmapped uid). Runs
/// BEFORE `pivot_root` (the host nodes are still accessible) and while we have
/// `CAP_DAC_OVERRIDE` (creator of the user ns). The nodes are character devices → the device
/// cgroup eBPF permits them.
fn setup_dev(rootfs: &str) -> nix::Result<()> {
    let dev = format!("{rootfs}/dev");
    let _ = std::fs::create_dir_all(&dev);
    mount(
        Some("tmpfs"),
        dev.as_str(),
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("mode=0755,size=1m"),
    )?;
    for name in ESSENTIAL_DEVS {
        let target = format!("{dev}/{name}");
        let _ = std::fs::File::create(&target); // mount point (we have CAP_DAC_OVERRIDE)
                                                // bind of the host's real node (survives pivot_root).
        let _ = mount(
            Some(format!("/dev/{name}").as_str()),
            target.as_str(),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        );
    }
    dev_std_symlinks(&dev); // /dev/stdout→/proc/self/fd/1 etc. (nginx/etc. logs)
    mount_devpts(&dev, true); // own pseudo-terminals (gid=5 = host's tty group)
    Ok(())
}

/// Creates the standard *stream* symlinks in `<dev>`: `/dev/stdout`, `/dev/stderr`,
/// `/dev/stdin` and `/dev/fd` → `/proc/self/fd/...`. It is what runc/Docker do —
/// and what makes programs like nginx (which link `access.log` → `/dev/stdout`)
/// write to the CAPTURED stdout, instead of to a lost file. The
/// targets resolve at runtime using the container's `/proc`.
fn dev_std_symlinks(dev: &str) {
    use std::os::unix::fs::symlink;
    let _ = symlink("/proc/self/fd", format!("{dev}/fd"));
    let _ = symlink("/proc/self/fd/0", format!("{dev}/stdin"));
    let _ = symlink("/proc/self/fd/1", format!("{dev}/stdout"));
    let _ = symlink("/proc/self/fd/2", format!("{dev}/stderr"));
}

/// Mounts its own `devpts` (`newinstance`) at `<dev>/pts` and creates `<dev>/ptmx`
/// → `pts/ptmx`. Gives the container its **own** pseudo-terminals — this is what
/// makes `exec -it` a real interactive shell and makes the terminal name
/// (`/dev/pts/N`) resolve inside it (like Docker). Best-effort.
fn mount_devpts(dev: &str, with_gid: bool) {
    let pts = format!("{dev}/pts");
    let _ = std::fs::create_dir_all(&pts);
    // `newinstance` isolates these ptys from the host's; `ptmxmode=0666` lets the
    // multiplexer be opened without a CAP. No `gid=5` in a user ns (gid not mappable).
    let opts = if with_gid {
        "newinstance,ptmxmode=0666,mode=0620,gid=5"
    } else {
        "newinstance,ptmxmode=0666,mode=0620"
    };
    let _ = mount(
        Some("devpts"),
        pts.as_str(),
        Some("devpts"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some(opts),
    );
    let ptmx = format!("{dev}/ptmx");
    let _ = std::fs::remove_file(&ptmx);
    let _ = std::os::unix::fs::symlink("pts/ptmx", &ptmx);
}

/// `/dev` for a container with a user namespace. Runs AFTER `setuid(0)` — only
/// then is the container's uid 0 mappable, and the `/dev` tmpfs ends up owned by uid 0 (if
/// mounted before, it would be owned by `overflow` and the container's root could not
/// write there). In a user ns there is no CAP_MKNOD, so we bind the
/// REAL host device nodes over the mount points — the only way to have real
/// `crw-rw-rw-` inside, like runc/Docker. The host nodes remain
/// accessible under `old_root` (the old root preserved by `pivot_root`, still to
/// be unmounted). The caller unmounts `old_root` right after.
fn setup_dev_userns(old_root: &str, devices: &[String]) {
    let _ = mount(
        Some("tmpfs"),
        "/dev",
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("mode=0755,size=1m"),
    );
    for name in ESSENTIAL_DEVS {
        let target = format!("/dev/{name}");
        let _ = std::fs::File::create(&target); // mount point
        let _ = mount(
            Some(format!("{old_root}/dev/{name}").as_str()),
            target.as_str(),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        );
    }
    bind_devices(old_root, "", devices); // --device (host's real nodes, via the old root)
    dev_std_symlinks("/dev"); // /dev/stdout→/proc/self/fd/1 etc.
    mount_devpts("/dev", false); // no gid=5 (not mappable in the user ns)
}

/// Wires the requested devices (`--device /dev/host[:/dev/cont]`) into the container's
/// `/dev` (bind of the host's real node). Char devices are permitted by the device
/// cgroup eBPF; block ones remain denied.
///
/// `src_prefix` prefixes the host node's path (empty without a user ns — the host is still
/// the root, before `pivot_root`; `/.delonix_old` with a user ns — the old root
/// preserved by `pivot_root`, where the host's `/dev` remains accessible after
/// the `setuid`). `rootfs` prefixes the mount point inside the container.
fn bind_devices(src_prefix: &str, rootfs: &str, devices: &[String]) {
    for spec in devices {
        let mut it = spec.split(':');
        let host = it.next().unwrap_or("");
        if host.is_empty() {
            continue;
        }
        let src = format!("{src_prefix}{host}");
        // Refuses BLOCK devices in code (does not rely solely on eBPF, which is
        // best-effort and may fail to load): giving `/dev/sda` to a container =
        // raw access to the host disk. Only char devices are permitted.
        match nix::sys::stat::stat(src.as_str()) {
            Ok(st) => {
                let mode = st.st_mode & libc::S_IFMT;
                if mode == libc::S_IFBLK {
                    eprintln!("delonix: --device {host}: block device refused (char devices only)");
                    continue;
                }
            }
            Err(_) => {
                eprintln!("delonix: --device {host}: node does not exist, ignored");
                continue;
            }
        }
        // destination: 2nd field if it starts with '/', otherwise = host path.
        let cont = match it.next() {
            Some(c) if c.starts_with('/') => c,
            _ => host,
        };
        let target = format!("{rootfs}{cont}");
        if let Some(parent) = std::path::Path::new(&target).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::File::create(&target); // mount point
        let _ = mount(
            Some(src.as_str()),
            target.as_str(),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        );
    }
}

/// Mounts the container rootfs and does `pivot_root` (runs INSIDE the `clone`).
#[allow(clippy::too_many_arguments)]
fn setup_rootfs(
    rootfs: &str,
    hostname: &str,
    mounts: &[Mount],
    userns: bool,
    devices: &[String],
    sysctls: &[String],
    host_pid: bool,
    privileged: bool,
) -> nix::Result<()> {
    sethostname(hostname)?;
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )?;
    mount(
        Some(rootfs),
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )?;
    // clean /dev with the essential nodes (tmpfs + bind of the host's real device nodes:
    // the only way to have real `crw-rw-rw-` without CAP_MKNOD). Without a user ns it runs HERE,
    // before pivot_root, as real root and while the host's `/dev` is the root. With
    // a user ns it is done AFTER setuid (otherwise the tmpfs would be owned by `overflow`); the
    // host nodes remain accessible in pivot_root's old root — see the caller of
    // setup_rootfs and setup_dev_userns.
    if !userns {
        setup_dev(rootfs)?;
        bind_devices("", rootfs, devices); // --device (host's real nodes)
    }
    // Volumes and bind mounts: injected BEFORE pivot_root, over the rootfs.
    for m in mounts {
        bind_volume(rootfs, m)?;
    }
    let put_old = format!("{rootfs}/.delonix_old");
    let _ = std::fs::create_dir_all(&put_old);
    // Essential mount points: MINIMAL images (e.g. the Kubernetes `e2e-test-images`)
    // may not bring `/proc` and `/sys`; create them on the overlay
    // (writable) BEFORE pivot_root, otherwise the `mount` of /proc fails with ENOENT and
    // the container does not start. It is what runc/Docker do (they create the mountpoints).
    let _ = std::fs::create_dir_all(format!("{rootfs}/proc"));
    let _ = std::fs::create_dir_all(format!("{rootfs}/sys"));
    chdir(rootfs)?;
    pivot_root(".", ".delonix_old")?;
    chdir("/")?;
    if host_pid {
        // Sharing the host's pidns, the kernel refuses to mount a NEW procfs (the
        // "fully visible" rule → EPERM); we bind the host's /proc (preserved in the old
        // root by pivot_root), which already has the correct view of the processes.
        mount(
            Some("/.delonix_old/proc"),
            "/proc",
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )?;
    } else {
        mount(
            Some("proc"),
            "/proc",
            Some("proc"),
            MsFlags::empty(),
            None::<&str>,
        )?;
    }
    apply_sysctls(sysctls); // --sysctl: BEFORE /proc/sys becomes read-only (B13)
    mask_proc_paths();
    // `/sys` READ-ONLY (B13): prevents writing to kernel/device controls
    // from the container. nosuid/nodev/noexec for defense. (Skips if there is no /sys.)
    // --privileged EXCEPTION: `/sys` RW + `cgroup2` RW delegated, so the systemd inside
    // the container (Kind nodes) can create and manage sub-cgroups. Only with `--privileged`.
    let sys_base = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    let fresh_sysfs = mount(
        Some("sysfs"),
        "/sys",
        Some("sysfs"),
        if privileged {
            sys_base
        } else {
            sys_base | MsFlags::MS_RDONLY
        },
        None::<&str>,
    );
    if fresh_sysfs.is_err() && privileged {
        // Mounting a NEW sysfs in a user ns is only permitted if the userns OWNS the
        // netns — with `--net host` (the default) it does not, and the kernel returns EPERM,
        // leaving /sys EMPTY (this is what prevented kindest/node from starting:
        // without /sys/fs/cgroup, the kind entrypoint "detects cgroup v1" and dies).
        // Fallback = what rootless runc does: recursive bind of the host's /sys,
        // preserved in the old root by pivot_root. Only in --privileged (Docker
        // semantics: --privileged exposes the host's /sys RW); non-privileged keeps the
        // usual behavior (no /sys — never expose the host's without asking).
        let _ = mount(
            Some("/.delonix_old/sys"),
            "/sys",
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        );
    }
    if privileged {
        // cgroup2 RW over /sys/fs/cgroup. With CLONE_NEWCGROUP, the view is
        // rooted at the container's cgroup (delegated by the host — cgroup v2
        // Delegate=yes prerequisite). nsdelegate lets systemd manage its subtree.
        // NOTE: we are already AFTER pivot_root — the mountpoint is created at
        // `/sys/fs/cgroup` (the earlier `{rootfs}/...` no longer resolved here, and the
        // create_dir_all failed silently).
        let _ = std::fs::create_dir_all("/sys/fs/cgroup");
        let _ = mount(
            Some("cgroup2"),
            "/sys/fs/cgroup",
            Some("cgroup2"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            Some("nsdelegate"),
        );
    }
    // With a user ns the old root is unmounted only AFTER setuid + setup_dev_userns
    // (which needs it to wire the host's real nodes into /dev) — done in the caller.
    if !userns {
        umount2("/.delonix_old", MntFlags::MNT_DETACH)?;
        let _ = std::fs::remove_dir("/.delonix_old");
    }
    Ok(())
}

/// Masks the `/proc` entries that give host control: `sysrq-trigger`
/// (can cause host panic/reboot) and `kcore` (kernel memory). Wires them to
/// `/dev/null`/read-only. Best-effort: runs before seccomp, with caps still
/// present. (Replicates Docker's *masked paths*.)
fn mask_proc_paths() {
    // bind /dev/null over sysrq-trigger -> writes go to the void.
    let _ = mount(
        Some("/dev/null"),
        "/proc/sysrq-trigger",
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    );
    // /proc/kcore (image of the kernel RAM): make it inaccessible.
    let _ = mount(
        Some("/dev/null"),
        "/proc/kcore",
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    );
    // /proc/sys read-only (prevents changing host sysctls).
    let _ = mount(
        Some("/proc/sys"),
        "/proc/sys",
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    );
    let _ = mount(
        None::<&str>,
        "/proc/sys",
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    );
}

/// Base and size of the UID mapping for the user namespace: the container's
/// root (uid 0) becomes the uid `USERNS_UID_BASE` (unprivileged) on the
/// host. Without this, the container's root is the REAL host root.
pub const USERNS_UID_BASE: u32 = 100_000;
pub const USERNS_RANGE: u32 = 65_536;

/// Writes the uid/gid maps of a container with a user namespace (runs in the PARENT).
/// - **As root** (engine with `sudo`): maps the range `100000+65536` (container's
///   root = unprivileged uid on the host).
/// - **Rootless** (engine without `sudo`): maps a SINGLE uid — `0 <euid> 1` — because
///   without `newuidmap` (setuid helper) a non-root can only map its own uid.
fn write_userns_maps(pid: i32, want_range: bool) -> Result<()> {
    // SAFETY: geteuid/getegid have no preconditions.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    // ROOTLESS + image with USER≠0: the target uid (e.g. 1000) does NOT exist in a map of
    // a single uid. Maps a RANGE via `newuidmap`/`newgidmap` (setuid helpers that
    // consult /etc/subuid|subgid): container uid 0 → our euid, and 1..N → the
    // delegated subuids. This way the `setuid(1000)` inside the container becomes
    // valid. If the helpers/subuid do not exist, it falls back to the single-uid map (and the
    // caller degrades to running as root, with a warning).
    if want_range && euid != 0 && have_subid_helpers() {
        // Do NOT write `setgroups=deny` here. It is only MANDATORY when a non-root
        // writes the `gid_map` BY HAND (see the branch below) — the kernel requires it to
        // prevent a restrictive group from being dropped. With `newgidmap` (setuid
        // helper, validates against /etc/subgid) the mapping is privileged and the
        // kernel lets `setgroups` stay at `allow`.
        //
        // Setting it to `deny` here broke half the official images: the
        // entrypoints that drop privilege (`su-exec`/`gosu`/`setpriv`) call
        // `setgroups()` BEFORE `setuid()` and got EPERM. `postgres` died
        // right away with `failed switching to 'postgres': operation not permitted`,
        // even though the target uid was well mapped and we had CAP_SETUID/SETGID.
        // docker/podman rootless also leave `allow` on this path.
        let range = USERNS_RANGE - 1; // 1..USERNS_RANGE delegated to the subuids
        let map_uid = format!("0 {euid} 1 1 {USERNS_UID_BASE} {range}");
        let map_gid = format!("0 {egid} 1 1 {USERNS_UID_BASE} {range}");
        run_idmap("newuidmap", pid, &map_uid)?;
        run_idmap("newgidmap", pid, &map_gid)?;
        return Ok(());
    }
    // `setgroups=deny` before the gid_map (good practice; mandatory for non-root).
    let _ = std::fs::write(format!("/proc/{pid}/setgroups"), "deny");
    let (uid_map, gid_map) = if euid == 0 {
        let m = format!("0 {USERNS_UID_BASE} {USERNS_RANGE}\n");
        (m.clone(), m)
    } else {
        (format!("0 {euid} 1\n"), format!("0 {egid} 1\n"))
    };
    std::fs::write(format!("/proc/{pid}/uid_map"), &uid_map).map_err(|e| Error::Runtime {
        context: "uid_map",
        message: e.to_string(),
    })?;
    std::fs::write(format!("/proc/{pid}/gid_map"), &gid_map).map_err(|e| Error::Runtime {
        context: "gid_map",
        message: e.to_string(),
    })?;
    Ok(())
}

/// `true` if the `newuidmap`/`newgidmap` helpers exist (needed to map a
/// range of subuids in rootless — the image's `USER` ≠ root path).
fn have_subid_helpers() -> bool {
    ["/usr/bin/newuidmap", "/bin/newuidmap"]
        .iter()
        .any(|p| std::path::Path::new(p).exists())
        && ["/usr/bin/newgidmap", "/bin/newgidmap"]
            .iter()
            .any(|p| std::path::Path::new(p).exists())
}

/// Runs `newuidmap`/`newgidmap <pid> <map...>` (the map args are triplets
/// `<id_in_ns> <id_on_host> <count>`).
fn run_idmap(tool: &str, pid: i32, map: &str) -> Result<()> {
    let mut cmd = std::process::Command::new(tool);
    cmd.arg(pid.to_string());
    for tok in map.split_whitespace() {
        cmd.arg(tok);
    }
    let st = cmd.status().map_err(|e| Error::Runtime {
        context: "idmap",
        message: format!("{tool}: {e}"),
    })?;
    if !st.success() {
        return Err(Error::Runtime {
            context: "idmap",
            message: format!(
                "{tool} failed (code {:?}) — check /etc/subuid and /etc/subgid",
                st.code()
            ),
        });
    }
    Ok(())
}

/// `true` if the engine runs without root privileges (*rootless* mode, A13).
pub fn is_rootless() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() != 0 }
}

/// Removes a file tree that may contain **subuid** files (chowned
/// to a rootless container's service uid — e.g. nginx chowns the
/// caches to 101 → host 100100). The user (real uid) can NOT delete them.
/// Solution (`podman unshare rm`-style): fork a child in a user namespace; the parent
/// maps the subuid range (`newuidmap`); the child becomes root IN THAT userns
/// (hence effective owner of the subuids) and re-execs `delonix __rmtree <path>` which deletes them.
/// Re-executes `delonix <args…>` as **root in a mapped user namespace** (the parent
/// writes the subuid map via `newuidmap` — the same mechanism as
/// [`remove_tree_mapped`], generalized). It is the way to do file
/// operations over trees with **subuid** owners (volumes written by
/// rootless containers): inside the userns the child is their effective owner.
///
/// Returns `None` if the mechanism does not apply (non-rootless, or without the
/// `newuidmap`/`newgidmap` helpers) — the caller should then do the operation directly.
/// `Some(true)` = the child finished successfully; `Some(false)` = it failed.
pub fn reexec_mapped(args: &[&str]) -> Option<bool> {
    if !is_rootless() || !have_subid_helpers() {
        return None;
    }
    // Pre-computes EVERYTHING that allocates BEFORE the fork (post-fork only async-signal-safe ops).
    let exe = std::env::current_exe().ok()?;
    let prog = std::ffi::CString::new(exe.as_os_str().as_encoded_bytes()).ok()?;
    let cargs: Vec<std::ffi::CString> = args
        .iter()
        .map(|a| std::ffi::CString::new(*a))
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    let mut argv: Vec<*const libc::c_char> = Vec::with_capacity(cargs.len() + 2);
    argv.push(prog.as_ptr());
    argv.extend(cargs.iter().map(|c| c.as_ptr()));
    argv.push(std::ptr::null());
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Some(false);
    }
    let (r, w) = (fds[0], fds[1]);
    // SAFETY: fork; the child only does close/unshare/read/setuid/execv (async-signal-safe;
    // the CStrings/argv were built above, before the fork).
    match unsafe { libc::fork() } {
        0 => unsafe {
            libc::close(w);
            if libc::unshare(libc::CLONE_NEWUSER) != 0 {
                libc::_exit(1);
            }
            let mut b = [0u8; 1];
            let _ = libc::read(r, b.as_mut_ptr() as *mut libc::c_void, 1);
            libc::close(r);
            libc::setgid(0);
            libc::setuid(0);
            libc::execv(prog.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        },
        pid if pid > 0 => {
            unsafe { libc::close(r) };
            // small wait for the child to unshare before we map.
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = write_userns_maps(pid, true);
            let ok = unsafe {
                let go = [1u8; 1];
                let _ = libc::write(w, go.as_ptr() as *const libc::c_void, 1);
                libc::close(w);
                let mut st = 0;
                libc::waitpid(pid, &mut st, 0);
                libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0
            };
            Some(ok)
        }
        _ => {
            unsafe {
                libc::close(r);
                libc::close(w);
            }
            Some(false)
        }
    }
}

/// Without rootless/helpers → direct removal.
pub fn remove_tree_mapped(path: &std::path::Path) {
    if !is_rootless() || !have_subid_helpers() {
        let _ = std::fs::remove_dir_all(path);
        return;
    }
    // Pre-computes EVERYTHING that allocates BEFORE the fork (in the post-fork child, in a process that
    // may have threads, allocating can deadlock — only async-signal-safe ops there).
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("/usr/local/bin/delonix"));
    let prog = match std::ffi::CString::new(exe.as_os_str().as_encoded_bytes()) {
        Ok(p) => p,
        Err(_) => {
            let _ = std::fs::remove_dir_all(path);
            return;
        }
    };
    let a1 = std::ffi::CString::new("__rmtree").unwrap();
    let a2 = match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(p) => p,
        Err(_) => {
            let _ = std::fs::remove_dir_all(path);
            return;
        }
    };
    let argv = [prog.as_ptr(), a1.as_ptr(), a2.as_ptr(), std::ptr::null()];
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        let _ = std::fs::remove_dir_all(path);
        return;
    }
    let (r, w) = (fds[0], fds[1]);
    // SAFETY: fork; the child only does close/unshare/read/setuid/execv (async-signal-safe,
    // without allocation — the CStrings/argv were created above, before the fork).
    match unsafe { libc::fork() } {
        0 => unsafe {
            libc::close(w);
            if libc::unshare(libc::CLONE_NEWUSER) != 0 {
                libc::_exit(1);
            }
            let mut b = [0u8; 1];
            let _ = libc::read(r, b.as_mut_ptr() as *mut libc::c_void, 1);
            libc::close(r);
            libc::setgid(0);
            libc::setuid(0);
            libc::execv(prog.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        },
        pid if pid > 0 => {
            unsafe { libc::close(r) };
            // small wait for the child to unshare before we map.
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = write_userns_maps(pid, true);
            let ok = unsafe {
                let go = [1u8; 1];
                let _ = libc::write(w, go.as_ptr() as *const libc::c_void, 1);
                libc::close(w);
                let mut st = 0;
                libc::waitpid(pid, &mut st, 0);
                libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0
            };
            // The exit status was READ AND IGNORED, without fallback: if the child failed
            // (it was the case — it re-executed `delonix __rmtree`, a subcommand the
            // public binary did not have, and clap returned rc=2), the tree stayed
            // undeleted SILENTLY and no one knew. The sibling `reexec_mapped` already
            // verified; this one did not. Now it verifies, and still tries the direct
            // path — which resolves the common case (files of our own uid,
            // no subuid in the mix) even if the re-exec breaks again.
            if !ok {
                let _ = std::fs::remove_dir_all(path);
            }
        }
        _ => {
            unsafe {
                libc::close(r);
                libc::close(w);
            }
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// `true` if we run INSIDE a non-initial user namespace (mapped uid 0, not
/// the real host root) — e.g. the rootless ingress spawn, which runs in the holder's
/// userns. The initial userns has the identity map `0 0 4294967295`; any other
/// indicates a child-userns. Without cgroup delegation here too → best-effort limits.
pub fn in_userns() -> bool {
    std::fs::read_to_string("/proc/self/uid_map")
        .map(|s| s.split_whitespace().collect::<Vec<_>>() != ["0", "0", "4294967295"])
        .unwrap_or(false)
}

/// Requests the transition to an AppArmor profile on the next `execve`
/// (`aa_change_onexec`). Best-effort: if AppArmor is not available,
/// it proceeds without MAC confinement.
fn apply_apparmor(profile: &str) {
    let cmd = format!("exec {profile}");
    // Recent kernels: /proc/self/attr/apparmor/exec; old ones: /proc/self/attr/exec.
    if std::fs::write("/proc/self/attr/apparmor/exec", &cmd).is_err() {
        let _ = std::fs::write("/proc/self/attr/exec", &cmd);
    }
}

/// `true` if SELinux is the active LSM (mounted at `/sys/fs/selinux`). On hosts
/// with AppArmor (Debian/Ubuntu) it is `false`; on RHEL/Fedora it is `true`.
fn selinux_active() -> bool {
    std::path::Path::new("/sys/fs/selinux/enforce").exists()
}

/// Requests the transition to a SELinux context on the next `execve` (`setexeccon`),
/// writing to `/proc/.../attr/exec`. Only acts if SELinux is the active LSM —
/// on AppArmor hosts that path belongs to AppArmor, hence the *gate*.
/// (The major LSMs are exclusive: either AppArmor or SELinux.)
fn apply_selinux(context: &str) {
    if selinux_active() && std::fs::write("/proc/thread-self/attr/exec", context).is_err() {
        let _ = std::fs::write("/proc/self/attr/exec", context);
    }
}

/// The body that runs inside the new namespaces (the container's PID 1).
#[allow(clippy::too_many_arguments)]
/// Replaces the inherited environment with a clean, predictable one (like Docker):
/// default `PATH`/`HOME`/`HOSTNAME`/`TERM` + the `KEY=value` from the image/stack/CLI
/// (these override). Runs in the single-threaded child, before the `execvp`.
fn apply_env(hostname: &str, env: &[String]) {
    let keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    for k in keys {
        std::env::remove_var(k);
    }
    std::env::set_var(
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    );
    std::env::set_var("HOME", "/root");
    std::env::set_var("HOSTNAME", hostname);
    std::env::set_var("TERM", "xterm");
    for kv in env {
        if let Some((k, v)) = kv.split_once('=') {
            let k = k.trim();
            if !k.is_empty() {
                std::env::set_var(k, v);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // container init: many namespace/security parameters
/// Mounts the requested tmpfs (`--tmpfs /path[:opts]`). Runs after `pivot_root`
/// and before dropping caps; `nosuid,nodev` by default (hardening).
fn apply_tmpfs(specs: &[String]) {
    for spec in specs {
        let (target, opts) = match spec.split_once(':') {
            Some((t, o)) => (t, o.to_string()),
            None => (spec.as_str(), "mode=1777".to_string()),
        };
        let _ = std::fs::create_dir_all(target);
        let _ = mount(
            Some("tmpfs"),
            target,
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            Some(opts.as_str()),
        );
    }
}

/// Mounts a **tmpfs** at `/run/secrets` and writes the key→value pairs (0600) there,
/// for `--secret-files`. Runs INSIDE the container's namespace (post-`pivot_root`,
/// still with caps): the values stay only in RAM (tmpfs) — they never touch the host fs
/// nor the container's, nor the environment. The mount is left read-only for the container.
fn write_secret_files(pairs: &[(String, String)]) {
    use std::os::unix::fs::PermissionsExt;
    let dir = "/run/secrets";
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let _ = mount(
        Some("tmpfs"),
        dir,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=0700"),
    );
    for (k, v) in pairs {
        // only safe file names (they are valid env keys, but defensive).
        if k.is_empty() || k.contains('/') {
            continue;
        }
        let p = format!("{dir}/{k}");
        if std::fs::write(&p, v).is_ok() {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
    }
    // makes the tmpfs read-only for the container (the values are already there).
    let _ = mount(
        None::<&str>,
        dir,
        None::<&str>,
        MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        None::<&str>,
    );
}

/// Writes the namespaced `sysctl`s (`--sysctl net.x=y`) into the container's
/// `/proc/sys/...` (after `/proc` is mounted). Only those the namespace permits.
fn apply_sysctls(specs: &[String]) {
    for kv in specs {
        if let Some((k, v)) = kv.split_once('=') {
            let k = k.trim();
            // Allowlist of NAMESPACED sysctls (Docker model): only these are
            // safe in a container — the rest (`kernel.*`, `vm.*`, …) are
            // GLOBAL to the host and a container cannot touch them. Without this, and since
            // this runs before `/proc/sys` becomes RO and before dropping caps,
            // a container could write HOST kernel knobs.
            if !sysctl_namespaced(k) {
                eprintln!("delonix: --sysctl {k}: not namespaced; refused (affects the host)");
                continue;
            }
            let path = format!("/proc/sys/{}", k.replace('.', "/"));
            let _ = std::fs::write(&path, v.trim());
        }
    }
}

/// `true` if the sysctl is namespaced (safe for a container to change). Same
/// set that Docker permits by default.
fn sysctl_namespaced(k: &str) -> bool {
    if k.contains("..") || k.starts_with('/') {
        return false;
    }
    k == "kernel.sem"
        || k.starts_with("kernel.shm")
        || k.starts_with("kernel.msg")
        || k.starts_with("fs.mqueue.")
        || k.starts_with("net.")
}

/// The type of the 1st argument of `setrlimit`: enum (`__rlimit_resource_t`) in glibc,
/// `c_int` in musl. Conditional alias so the static musl build compiles.
#[cfg(target_env = "musl")]
type RlimitResource = libc::c_int;
#[cfg(not(target_env = "musl"))]
type RlimitResource = libc::__rlimit_resource_t;

/// Maps a `--ulimit` name to the `RLIMIT_*` resource.
fn rlimit_resource(name: &str) -> Option<RlimitResource> {
    Some(match name {
        "nofile" => libc::RLIMIT_NOFILE,
        "nproc" => libc::RLIMIT_NPROC,
        "core" => libc::RLIMIT_CORE,
        "fsize" => libc::RLIMIT_FSIZE,
        "cpu" => libc::RLIMIT_CPU,
        "memlock" => libc::RLIMIT_MEMLOCK,
        "stack" => libc::RLIMIT_STACK,
        "as" => libc::RLIMIT_AS,
        "nofile_hard" => libc::RLIMIT_NOFILE,
        _ => return None,
    })
}

/// Applies `--ulimit name=soft[:hard]` via `setrlimit` (before dropping caps, so it can
/// raise hard limits with `CAP_SYS_RESOURCE`).
fn apply_ulimits(specs: &[String]) {
    let parse = |s: &str| -> Option<u64> {
        if s == "unlimited" || s == "-1" {
            Some(libc::RLIM_INFINITY)
        } else {
            s.parse().ok()
        }
    };
    for spec in specs {
        let Some((name, vals)) = spec.split_once('=') else {
            continue;
        };
        let Some(res) = rlimit_resource(name.trim()) else {
            continue;
        };
        let (soft, hard) = match vals.split_once(':') {
            Some((s, h)) => (s, h),
            None => (vals, vals),
        };
        if let (Some(rc), Some(rm)) = (parse(soft.trim()), parse(hard.trim())) {
            let rl = libc::rlimit {
                rlim_cur: rc,
                rlim_max: rm,
            };
            // SAFETY: `res` is a valid RLIMIT_* and `rl` is initialized.
            unsafe { libc::setrlimit(res, &rl) };
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// (privileged / Kind node) Gives the container a DEDICATED, EMPTY CGROUP ROOT.
///
/// On the rootless-with-network path the `ip netns exec` mounts a FRESH sysfs over
/// `/sys`, COVERING the delegated cgroup2; and the node would inherit as its cgroup-ns root
/// the very cgroup-scope where `kind` (and the delonix helpers) run — but the
/// node's kubelet aborts if that root has DIRECT processes
/// (`create-kubelet-cgroup-v2.sh` needs to write the top-level `cgroup.subtree_control`).
/// Solution: (1) uncover the real cgroup2 (umount the sysfs), (2) move us
/// to an EMPTY leaf `<base>/dlx-<id>`, (3) `unshare(CLONE_NEWCGROUP)` — the cgroup-ns
/// root becomes the leaf (only our init) and `kind`/`delonix`/helpers stay
/// ABOVE it. Best-effort: the final `unshare` ALWAYS runs (even without the leaf it gives the
/// cgroup-ns as before — no regression for non-node privileged).
/// systemd units that on a rootless Kind node **fail and exhaust the timeout** (they have no
/// access to kernel-fs/modules/udev), delaying boot ~2min and making Kind give up
/// on readiness detection. We mask them (symlink → `/dev/null`, the mechanism of
/// `systemctl mask`) in the rootfs's `/etc/systemd/system/`, BEFORE systemd starts.
/// None is needed by `containerd`/`kubelet` (the host modules are already
/// loaded and visible). Runs post-pivot, uid 0 in the userns. Best-effort.
fn mask_slow_node_units() {
    const UNITS: &[&str] = &[
        "dev-mqueue.mount",
        "sys-kernel-debug.mount",
        "sys-kernel-tracing.mount",
        "sys-kernel-config.mount",
        "kmod-static-nodes.service",
        "systemd-modules-load.service",
        "systemd-udev-trigger.service",
        "modprobe@configfs.service",
        "modprobe@dm_mod.service",
        "modprobe@fuse.service",
        "modprobe@loop.service",
    ];
    let dir = "/etc/systemd/system";
    let _ = std::fs::create_dir_all(dir);
    for u in UNITS {
        let link = format!("{dir}/{u}");
        let _ = std::fs::remove_file(&link); // idempotent (node restart)
        let _ = std::os::unix::fs::symlink("/dev/null", &link);
    }
}

/// Kind node: seeds ONE `iptables-nft` rule so the Kind entrypoint's `select_iptables`
/// chooses the **nft** backend. Without this, in a fresh netns
/// both `iptables-legacy-save` and `iptables-nft-save` return 0 lines and the
/// Kind script, on the tie (`num_legacy >= num_nft`), chooses `legacy` — which
/// is UNUSABLE here: the legacy backend reads `/proc/net/ip_tables_names`, a
/// `0440` file owned by the HOST root that, in our user namespace, appears with
/// an unmapped owner (nobody) and hence unreadable → EPERM, and the node boot aborts
/// right after "setting iptables to detected mode: legacy". The nft backend
/// does not touch that file and works (the netns is OURS, we have effective CAP_NET_ADMIN
/// in it). The rule is harmless (the INPUT policy is already ACCEPT); it only serves
/// to make `iptables-nft-save` report ≥1 line and the Kind tie fall to nft.
/// Best-effort, BEFORE the entrypoint's `execve` (the `select_iptables` runs at
/// entrypoint startup, before systemd) and still with CAP_NET_ADMIN.
fn seed_kind_nft() {
    for bin in ["/usr/sbin/iptables-nft", "/sbin/iptables-nft"] {
        if std::path::Path::new(bin).exists() {
            let _ = std::process::Command::new(bin)
                .args(["-A", "INPUT", "-j", "ACCEPT"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            return;
        }
    }
}

fn setup_node_cgroup_ns(cid: &str) {
    // 1) Uncover the real cgroup2. On the rootless-with-network path the `ip netns exec` mounts
    //    a FRESH sysfs over `/sys` (WITHOUT cgroup2 → WITHOUT the
    //    `cgroup.controllers` file), covering the delegated cgroup2. It is detected by the ABSENCE
    //    of that file — a real cgroup2 ALWAYS has it, even without delegated
    //    controllers (reading "non-empty" gave a false-negative in that case).
    let real_cg2 = std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
    if !real_cg2 {
        // Hygiene: makes `/` (recursively) PRIVATE before the umount so the umount
        // does not escape this mount-ns. (The `ip netns exec` already set `/` to rslave, so the
        // umount does NOT propagate to the holder's mount-ns anyway — this is defense
        // in depth, not the cause of the readiness flakiness.) SAFETY: root in the
        // userns (full caps), only in this mount-ns.
        let _ = mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            None::<&str>,
        );
        let _ = umount2("/sys", MntFlags::empty());
    }
    // 2) Move us to a SIBLING leaf of the `kind` cgroup, under the parent SCOPE, and delegate
    //    the scope's controllers to the leaf. `kind` (and the helpers) run in
    //    `<scope>/kind` (see `paas.rs`), freeing the `<scope>` root; this way the
    //    leaf `<scope>/dlx-<id>` gets cpu delegated (the node entrypoint requires it)
    //    AND as the cgroup-ns root it has 0 direct processes (the kubelet requires it).
    if let Some(base) = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("0::")
                    .map(|r| format!("/sys/fs/cgroup{}", r.trim()))
            })
        })
    {
        // scope = parent of the current cgroup (the node inherits `<scope>/kind` from `kind`).
        if let Some(scope) = std::path::Path::new(&base)
            .parent()
            .map(|p| p.to_path_buf())
        {
            let scope = scope.to_string_lossy().to_string();
            // RACE-CLOSE (deterministic): delegating `subtree_control` with DIRECT
            // processes in the scope is rejected (no-internal-processes) → the `+cpu` did not
            // engage and the node did not become Ready (part of the ~50% flakiness, masked
            // by the retry). Wait (briefly) for the scope root to become EMPTY — `paas.rs`
            // moves `kind` to `<scope>/kind`, but we close any window here.
            for _ in 0..30 {
                let empty = std::fs::read_to_string(format!("{scope}/cgroup.procs"))
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true); // unreadable → do not wait (best-effort)
                if empty {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            // Delegate the scope's controllers to the children. Best-effort: silent failure
            // on hosts without this delegated cgroup structure. `cpuset`/`io` only pass
            // if the host delegates them to the user session (drop-in `Delegate=cpu cpuset io
            // memory pids` in the `user@.service` — see CLAUDE.md); without that, the kubelet
            // marks "missing required cgroups: cpuset" but the rest works.
            for ctrl in ["+cpuset", "+cpu", "+io", "+memory", "+pids"] {
                let _ = std::fs::write(format!("{scope}/cgroup.subtree_control"), ctrl);
            }
            let leaf = format!("{scope}/dlx-{cid}");
            if std::fs::create_dir_all(&leaf).is_ok() {
                let _ = std::fs::write(
                    format!("{leaf}/cgroup.procs"),
                    std::process::id().to_string(),
                );
            }
        }
    }
    // 3) Anchor the cgroup-ns root at the CURRENT cgroup (the leaf, if the move worked).
    let _ = unshare(CloneFlags::CLONE_NEWCGROUP);
}

// FIXME(follow-up): 29 positional arguments — a real smell. Refactor to a typed
// `ContainerInitSpec` (groups rootfs/hostname/argv/limits/flags) in a
// dedicated, reviewed change; do not mix with the lint gate.
#[allow(clippy::too_many_arguments)]
fn container_init(
    rootfs: &str,
    hostname: &str,
    argv: &[CString],
    detach: bool,
    log_fd: Option<i32>,
    mounts: &[Mount],
    sync: Option<(i32, i32)>,
    apparmor: Option<&str>,
    selinux: Option<&str>,
    pod_infra_pid: Option<i32>,
    env: &[String],
    read_only: bool,
    cap_keep: u64,
    seccomp_unconfined: bool,
    seccomp_detect: bool,
    devices: &[String],
    tmpfs: &[String],
    ulimits: &[String],
    sysctls: &[String],
    host_pid: bool,
    inherit_userns: bool,
    run_uid: Option<u32>,
    run_gid: Option<u32>,
    privileged: bool,
    console_sock: Option<(i32, i32)>,
    secret_files: &[(String, String)],
    workdir: Option<&str>,
    cid: &str,
    node_cgroup: bool,
) -> isize {
    // User namespace: wait for the PARENT to write uid_map/gid_map before continuing
    // (until then, we are `nobody` without caps). The received byte is the "you may proceed".
    // In the rootless ingress we inherit the holder's userns (already as uid 0) — no clone
    // nor sync, but the rootfs is treated as `userns` (we are root in the inherited userns).
    let userns = sync.is_some() || inherit_userns;
    if let Some((r, w)) = sync {
        // SAFETY: pipe fds inherited from the clone; close the write, read 1 byte from the read.
        unsafe {
            libc::close(w);
            let mut b = [0u8; 1];
            let _ = libc::read(r, b.as_mut_ptr() as *mut libc::c_void, 1);
            libc::close(r);
        }
    }
    // Pod IPC/UTS sharing: join the infra container's IPC + UTS namespaces so the
    // pod's containers share System V/POSIX IPC and the UTS/hostname. The netns is
    // already joined (the `--pod` re-exec's `ip netns exec`). We run in the
    // holder's userns (via that re-exec), which OWNS these namespaces, so `setns`
    // has privilege — the reason the old `join_netns` setns failed rootless no
    // longer applies. IPC/UTS are "immediate" namespaces (setns moves us directly);
    // we suppressed CLONE_NEWIPC/NEWUTS in `spawn` so there is one to join into.
    if let Some(pid) = pod_infra_pid {
        for (sub, flag) in [
            ("ipc", CloneFlags::CLONE_NEWIPC),
            ("uts", CloneFlags::CLONE_NEWUTS),
        ] {
            let path = format!("/proc/{pid}/ns/{sub}");
            match open(
                path.as_str(),
                OFlag::O_RDONLY | OFlag::O_CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => {
                    // SAFETY: valid fd; setns joins the pod infra's ipc/uts ns.
                    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
                    if setns(owned, flag).is_err() {
                        eprintln!("delonix: failed to join the pod {sub} namespace");
                        return 125;
                    }
                }
                Err(_) => {
                    eprintln!("delonix: pod {sub} namespace unavailable (infra pid {pid})");
                    return 125;
                }
            }
        }
    }
    if detach {
        detach_stdio(log_fd);
    }
    // KIND node: BEFORE mounting the rootfs (which remounts `/sys`), gives the node a dedicated,
    // empty cgroup-ns root (uncovers the real cgroup2, moves us to a leaf,
    // `unshare(NEWCGROUP)`). This way the cgroup2 the rootfs will mount reflects the leaf
    // as root. Gated to the Kind node (label `io.x-k8s.kind.*`), NOT to every
    // `--privileged` — a normal privileged container should not have its cgroup
    // hierarchy meddled with. Best-effort.
    if node_cgroup {
        setup_node_cgroup_ns(cid);
    }
    // `setup_rootfs` runs as the user ns creator (full caps, even being
    // `nobody`): the pivot_root and the files go to the host overlay (which accepts
    // the host uid). Without a user ns, it mounts `/dev` right away (bind of the host's real nodes).
    // With a user ns, `/dev` is mounted next, after the setuid — see below.
    if let Err(e) = setup_rootfs(
        rootfs, hostname, mounts, userns, devices, sysctls, host_pid, privileged,
    ) {
        eprintln!("delonix: failed to prepare the rootfs: {e}");
        return 126;
    }
    if userns {
        // uid 0 INSIDE the user ns (= USERNS_UID_BASE on the host, mappable).
        // nonzero->0 copies permitted->effective (keeps caps).
        // SAFETY: we are the user ns creator -> we have CAP_SETUID/SETGID.
        unsafe {
            libc::setgid(0);
            libc::setuid(0);
        }
        // /dev: tmpfs (now owned by uid 0) + bind of the host's real nodes from the
        // old root preserved by pivot_root. Then unmount it (no longer needed).
        setup_dev_userns("/.delonix_old", devices);
        let _ = umount2("/.delonix_old", MntFlags::MNT_DETACH);
        let _ = std::fs::remove_dir("/.delonix_old");
    }
    // Kind node: masks the systemd units that FAIL and delay boot in a rootless
    // container (kernel-fs mounts, module modprobe, udev/modules-load). Without
    // this, boot takes ~2min (each exhausts the timeout) and Kind gives up on readiness
    // detection ("Preparing nodes"). Here we are already post-pivot (`/` is the node's
    // rootfs), uid 0 in the userns and writable. Best-effort.
    if node_cgroup {
        mask_slow_node_units();
        // Bias the Kind `select_iptables` toward nft (the legacy backend is
        // unreadable in rootless — see `seed_kind_nft`). Still with CAP_NET_ADMIN,
        // in the node's netns, before the entrypoint's `execve`.
        seed_kind_nft();
    }
    // --privileged detached (Kind nodes): allocates a `/dev/console` (pty) for
    // PID 1 and captures it in the log. Must be AFTER the `/dev`/devpts is mounted (above)
    // and BEFORE dropping caps (the bind of `/dev/console` needs CAP_SYS_ADMIN).
    // The `detach_stdio` above already pointed the inherited stdio to /dev/null; this
    // re-points it to the pty. See `setup_console`.
    if let Some(cs) = console_sock {
        setup_console(cs);
    }
    apply_tmpfs(tmpfs); // --tmpfs (after the pivot, still with caps)
    if !secret_files.is_empty() {
        write_secret_files(secret_files); // --secret-files: in-namespace tmpfs (still with caps)
    }
    // `--read-only`: remounts the rootfs (`/`) read-only. Volumes/dev/proc are
    // separate mounts and stay writable; the rest becomes immutable.
    if read_only {
        let _ = mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY,
            None::<&str>,
        );
    }
    apply_ulimits(ulimits); // --ulimit (before dropping CAP_SYS_RESOURCE)
    set_no_new_privs(); // no execve gains privileges (anti-escalation) — always
    drop_capabilities(cap_keep); // drop caps (after the mounts, before the exec)
    apply_seccomp(seccomp_unconfined, seccomp_detect); // allowlist (default-deny)
                                                       // FAIL-CLOSED: confirms the confinement REALLY took effect before the execve.
                                                       // Runs BEFORE the USER's `setuid` (further below) and BEFORE `apply_env` (so it reads the
                                                       // ENGINE's opt-out, not the container's). See `verify_confinement`.
    if !insecure_besteffort() {
        if let Err(e) = verify_confinement(!seccomp_unconfined, cap_keep) {
            eprintln!("delonix: confinement NOT verified ({e}); aborting the container");
            return 126;
        }
    }
    if let Some(p) = apparmor {
        apply_apparmor(p); // MAC confinement (AppArmor) — transitions on the execve
    }
    if let Some(c) = selinux {
        apply_selinux(c); // MAC confinement (SELinux) — only on SELinux hosts
    }
    // image CWD (OCI `WorkingDir`) — AFTER the pivot, BEFORE the exec. Without this,
    // entrypoints that operate on the CWD (redis/postgres `chown -R .`) run from `/`
    // and touch `/sys` (RO). If the dir does not exist, create it (Docker WORKDIR semantics).
    if let Some(w) = workdir.filter(|w| !w.is_empty() && *w != "/") {
        let _ = std::fs::create_dir_all(w);
        if chdir(w).is_err() {
            eprintln!("delonix: warning — failed to enter WORKDIR {w}");
        }
    }
    apply_env(hostname, env); // clean environment + image/stack/CLI ENV
                              // image `USER` (≠ root): switch to the requested uid/gid BEFORE the `execve`. Done
                              // last — after the mounts/caps/seccomp, which needed uid 0. We are
                              // inside the user namespace (root of the ns), so we have CAP_CHOWN/SETUID over the
                              // mapped range: we hand ownership of the rootfs to the uid (once; marker so as not to
                              // repeat) and drop privileges. setgid BEFORE setuid (after setuid one can no longer
                              // change group). E.g.: Elasticsearch refuses to run as root.
    if let Some(uid) = run_uid {
        if uid != 0 {
            let gid = run_gid.unwrap_or(uid);
            chown_tree_once("/", uid, gid);
            // The stdout/stderr are the log_shim's pipe, created as uid 0. "unprivileged"
            // images (nginx, etc.) link /var/log/.../*.log → /dev/stdout
            // (= /proc/self/fd/1) and REOPEN it already as the USER — which would fail without
            // the pipe belonging to it. fchown of the 3 fds gives them that access.
            // SAFETY: fchown over valid open fds (0/1/2); errors ignored.
            unsafe {
                libc::fchown(0, uid, gid);
                libc::fchown(1, uid, gid);
                libc::fchown(2, uid, gid);
            }
            // SAFETY: we are root in the user ns → setgid/setgroups/setuid succeed.
            unsafe {
                libc::setgroups(1, [gid].as_ptr());
                if libc::setgid(gid) != 0 {
                    eprintln!("delonix: setgid({gid}) failed");
                }
                if libc::setuid(uid) != 0 {
                    eprintln!(
                        "delonix: setuid({uid}) failed — the image USER is not mapped (subuid?)"
                    );
                    return 126;
                }
            }
        }
    }
    let _ = execvp(&argv[0], argv);
    eprintln!("delonix: exec failed: {:?}", argv[0]);
    127
}

/// `chown -R <uid>:<gid>` of `root` using **`lchown`** (never follows symlinks — a
/// malicious symlink inside the tree, e.g. `usr/x -> /etc/shadow`, cannot make us
/// change the ownership of a file outside the tree). Skips `proc`/`sys`/`dev` at the
/// top (they are mounts, not part of the exported rootfs). Best-effort: individual
/// errors are ignored (special files), what matters is the app's tree.
///
/// Public because it is shared with `delonix-runtime-bin` (rootless FLAT rootfs) —
/// **never reimplement this with `std::fs::chown`/`std::os::unix::fs::chown`**, which
/// follows symlinks.
pub fn lchown_tree(root: &std::path::Path, uid: u32, gid: u32) {
    fn lchown_path(p: &std::path::Path, uid: u32, gid: u32) {
        if let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) {
            // SAFETY: lchown over a valid path; does not follow the symlink.
            unsafe {
                libc::lchown(c.as_ptr(), uid, gid);
            }
        }
    }
    fn rec(dir: &std::path::Path, uid: u32, gid: u32, depth: u32) {
        if depth > 64 {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for ent in entries.flatten() {
            let p = ent.path();
            let ft = ent.file_type().ok();
            lchown_path(&p, uid, gid);
            if ft.map(|t| t.is_dir()).unwrap_or(false) {
                rec(&p, uid, gid, depth + 1);
            }
        }
    }
    for top in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let name = top.file_name();
        if matches!(name.to_str(), Some("proc") | Some("sys") | Some("dev")) {
            continue;
        }
        let p = top.path();
        lchown_path(&p, uid, gid);
        if top.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            rec(&p, uid, gid, 0);
        }
    }
}

/// Like [`lchown_tree`], but idempotent via a `/.delonix_user_<uid>` marker —
/// only runs the 1st time for a given uid, avoiding the cost on every startup.
fn chown_tree_once(root: &str, uid: u32, gid: u32) {
    let marker = format!("{}/.delonix_user_{uid}", root.trim_end_matches('/'));
    if std::path::Path::new(&marker).exists() {
        return;
    }
    lchown_tree(std::path::Path::new(root), uid, gid);
    let _ = std::fs::File::create(&marker);
}

/// PID limit per container (anti fork-bomb).
const DEFAULT_PIDS_MAX: &str = "512";

/// Encodes an eBPF instruction (8 bytes) into a `u64` (little-endian).
fn bpf_insn(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> u64 {
    (code as u64)
        | (((dst & 0xf) as u64) << 8)
        | (((src & 0xf) as u64) << 12)
        | ((off as u16 as u64) << 16)
        | ((imm as u32 as u64) << 32)
}

/// Loads a `CGROUP_DEVICE` eBPF program that **denies block devices**
/// (disks) and permits char ones, and attaches it to the container's cgroup. It is the
/// cgroup v2 *device cgroup* (device control via eBPF, like runc).
/// Best-effort: if the kernel does not support it, the other layers (caps/seccomp/userns/
/// AppArmor) already deny device access.
fn attach_device_filter(cgroup: &str) -> bool {
    const BPF_PROG_LOAD: i64 = 5;
    const BPF_PROG_ATTACH: i64 = 8;
    const BPF_PROG_TYPE_CGROUP_DEVICE: u32 = 15;
    const BPF_CGROUP_DEVICE: u32 = 6;

    // Program: r2 = ctx->access_type; type = r2 & 0xffff;
    //           if type == 1 (BLK) -> r0=0 (deny); otherwise r0=1 (allow).
    let insns: [u64; 7] = [
        bpf_insn(0x61, 2, 1, 0, 0),      // LDX_W r2 = *(u32*)(r1+0)
        bpf_insn(0x54, 2, 0, 0, 0xffff), // AND32 r2 &= 0xffff
        bpf_insn(0x15, 2, 0, 2, 1),      // JEQ r2,1 -> +2 (BLK = negar)
        bpf_insn(0xb7, 0, 0, 0, 1),      // MOV r0 = 1 (permitir)
        bpf_insn(0x95, 0, 0, 0, 0),      // EXIT
        bpf_insn(0xb7, 0, 0, 0, 0),      // MOV r0 = 0 (negar)
        bpf_insn(0x95, 0, 0, 0, 0),      // EXIT
    ];
    let license = b"GPL\0";
    let mut log = [0u8; 4096];

    // bpf_attr for PROG_LOAD (zeroed buffer; fields at the kernel offsets).
    let mut attr = [0u8; 128];
    attr[0..4].copy_from_slice(&BPF_PROG_TYPE_CGROUP_DEVICE.to_ne_bytes());
    attr[4..8].copy_from_slice(&(insns.len() as u32).to_ne_bytes());
    attr[8..16].copy_from_slice(&(insns.as_ptr() as u64).to_ne_bytes());
    attr[16..24].copy_from_slice(&(license.as_ptr() as u64).to_ne_bytes());
    attr[24..28].copy_from_slice(&1u32.to_ne_bytes()); // log_level
    attr[28..32].copy_from_slice(&(log.len() as u32).to_ne_bytes());
    attr[32..40].copy_from_slice(&(log.as_mut_ptr() as u64).to_ne_bytes());

    // SAFETY: bpf(PROG_LOAD) call with a valid, zeroed bpf_attr.
    let prog_fd = unsafe { libc::syscall(libc::SYS_bpf, BPF_PROG_LOAD, attr.as_ptr(), attr.len()) };
    if prog_fd < 0 {
        return false;
    }

    let cg = match std::ffi::CString::new(cgroup) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // SAFETY: opens the cgroup directory as an fd for the attach.
    let cg_fd = unsafe { libc::open(cg.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if cg_fd < 0 {
        unsafe { libc::close(prog_fd as i32) };
        return false;
    }

    let mut at = [0u8; 128];
    at[0..4].copy_from_slice(&(cg_fd as u32).to_ne_bytes()); // target_fd (cgroup)
    at[4..8].copy_from_slice(&(prog_fd as u32).to_ne_bytes()); // attach_bpf_fd
    at[8..12].copy_from_slice(&BPF_CGROUP_DEVICE.to_ne_bytes()); // attach_type
                                                                 // SAFETY: bpf(PROG_ATTACH) attaches the program to the cgroup.
    let r = unsafe { libc::syscall(libc::SYS_bpf, BPF_PROG_ATTACH, at.as_ptr(), at.len()) };
    unsafe {
        libc::close(prog_fd as i32);
        libc::close(cg_fd);
    }
    r == 0
}

/// Converts `cpus` (e.g. "0.5", "2") into the cgroup v2 `cpu.max` syntax
/// (`<quota> <period>`); `period` = 100000 µs. Minimum 0.01 of a core.
fn cpu_max_value(cpus: &str) -> String {
    let c: f64 = cpus.parse().unwrap_or(1.0);
    let quota = ((c * 100_000.0).round() as i64).max(1000);
    format!("{quota} 100000")
}

/// Writes a limit into the cgroup; failing is an ERROR (limits are MANDATORY — a
/// container should never run without a resource ceiling).
fn write_limit(cgroup: &str, file: &str, value: &str) -> Result<()> {
    std::fs::write(format!("{cgroup}/{file}"), value).map_err(|e| Error::Runtime {
        context: "cgroup limit",
        message: format!("{file}={value}: {e}"),
    })
}

/// Creates a dedicated cgroup and applies MANDATORY memory, CPU and PID limits,
/// then moves `pid` there. Unlike Docker (which by default limits
/// nothing), Delonix refuses to run a container without resource ceilings.
/// Percentage of the host reserved for Delonix in total (the rest is host headroom).
fn host_reserve_pct() -> u64 {
    std::env::var("DELONIX_RESERVE_PCT")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|p| (10..=95).contains(p))
        .unwrap_or(85)
}

/// Total host memory (bytes), from `/proc/meminfo`.
fn host_mem_bytes() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse::<u64>().ok())
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

fn host_ncpu() -> u64 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(1)
}

/// Aggregate disk I/O ceiling of the slice in bytes/s (`DELONIX_IO_MAX_BPS`,
/// def. **500 MB/s**). 0 disables the limit. Serves as a safety ceiling against
/// a container saturating the disk and killing the host, not as fine QoS.
fn host_io_max_bps() -> u64 {
    std::env::var("DELONIX_IO_MAX_BPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500_000_000)
}

/// `MAJ:MIN` of the block device backing the Delonix store (where the
/// overlays/images live). Needed for the slice's `io.max`. The cgroup-v2
/// `io.max` requires the WHOLE disk (not a partition: a partition gives ENODEV),
/// so we resolve the parent device when what contains the store is a partition.
fn slice_io_device() -> Option<String> {
    // The store lives under /var/lib/delonix (root) — use the device that contains it.
    let probe = ["/var/lib/delonix", "/var/lib", "/"];
    for p in probe {
        if let Ok(st) = nix::sys::stat::stat(p) {
            let dev = st.st_dev;
            let (maj, min) = (libc::major(dev), libc::minor(dev));
            if maj == 0 {
                continue; // virtual device (overlay/tmpfs) — no useful io.max
            }
            // If it is a partition, go up to the parent disk (`/sys/dev/block/M:m/../dev`).
            let sysfs = format!("/sys/dev/block/{maj}:{min}");
            if std::path::Path::new(&format!("{sysfs}/partition")).exists() {
                if let Ok(parent) = std::fs::read_to_string(format!("{sysfs}/../dev")) {
                    return Some(parent.trim().to_string());
                }
            }
            return Some(format!("{maj}:{min}"));
        }
    }
    None
}

/// Host 1-minute load average (`/proc/loadavg`).
fn host_load1() -> Option<f64> {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|f| f.parse().ok()))
}

/// Converts `64M`/`1G`/`512K`/bytes into bytes.
fn parse_mem_bytes(s: &str) -> u64 {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1024u64),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    // `saturating_mul` avoids overflow (e.g. "99999999999G"); an unparseable value
    // saturates to u64::MAX (and not 0), so admission control refuses it — never
    // treat garbage as "0 bytes" and let it through.
    match num.trim().parse::<u64>() {
        Ok(n) => n.saturating_mul(mult),
        Err(_) => u64::MAX,
    }
}

/// Will the cgroup limits (cpu/memory/pids) actually apply to a
/// new container on this host/session?
///
/// Mirrors EXACTLY the condition `spawn` tests — `mkdir` under the
/// `delonix.slice` — without starting any container: if we can create a
/// cgroup there (and clean it up right away), there is delegation; if not, it is
/// rootless-without-delegation and the limits will be best-effort.
///
/// Exists so the caller can warn ONCE BEFORE starting N nodes (e.g.
/// `cluster create`), instead of letting each node re-exec repeat the same warning.
pub fn cgroup_limits_apply() -> bool {
    let probe = format!(
        "{}/.delonix-probe-{}",
        delonix_runtime_core::DELONIX_SLICE,
        std::process::id()
    );
    match std::fs::create_dir(&probe) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            true
        }
        // Already existing (race) counts as "I can write there".
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_dir(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Ensures the `delonix.slice` with AGGREGATE limits (a fraction of the host) and the
/// controllers active for children. It is what prevents the SUM of all
/// containers from killing the host: the slice has total `memory.max`/`cpu.max`/`pids.max`,
/// and the kernel OOM-kills INSIDE the slice (a container), never the host. Idempotent.
pub fn ensure_delonix_slice() {
    let slice = delonix_runtime_core::DELONIX_SLICE;
    if std::fs::create_dir_all(slice).is_err() {
        return; // no permission (rootless) → best-effort
    }
    let pct = host_reserve_pct();
    let mem = host_mem_bytes();
    if mem > 0 {
        let _ = std::fs::write(format!("{slice}/memory.max"), (mem / 100 * pct).to_string());
        let _ = std::fs::write(format!("{slice}/memory.swap.max"), "0");
    }
    let ncpu = host_ncpu();
    let quota = ncpu * 100_000 / 100 * pct; // pct% of `ncpu` cores
    let _ = std::fs::write(format!("{slice}/cpu.max"), format!("{quota} 100000"));
    let _ = std::fs::write(format!("{slice}/pids.max"), (ncpu * 4096).to_string());
    // Aggregate DISK I/O ceiling: without this, a single container writing at
    // full tilt saturates the disk and kills the host (journald/store/swap) even with CPU and
    // memory limited. `io.max` (cgroup-v2) limits rbps/wbps on the device that
    // backs the store. Best-effort: may be above the device's real limit —
    // serves as a safety ceiling, not fine QoS. Tunable via env.
    if let Some(dev) = slice_io_device() {
        let cap_bps = host_io_max_bps();
        if cap_bps > 0 {
            let _ = std::fs::write(
                format!("{slice}/io.max"),
                format!("{dev} rbps={cap_bps} wbps={cap_bps}"),
            );
        }
    }
    // enable the controllers for the children (ONE by one — if some do not exist on the
    // host, the others stay active anyway).
    for ctrl in ["+memory", "+cpu", "+pids", "+io"] {
        let _ = std::fs::write(format!("{slice}/cgroup.subtree_control"), ctrl);
    }
}

/// ADMISSION control (robustness, #1/#4): gracefully refuses a new
/// container when Delonix's aggregate budget is exhausted or the host is
/// under excessive load — instead of letting the host drown. (The slice is already the
/// hard ceiling; this is the soft, informative refusal.)
pub fn admission_check(memory_max: &str) -> Result<()> {
    if is_rootless() {
        return Ok(()); // no delegated cgroup → no budget to check
    }
    ensure_delonix_slice();
    let slice = delonix_runtime_core::DELONIX_SLICE;
    let read = |f: &str| -> Option<u64> {
        std::fs::read_to_string(format!("{slice}/{f}"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
    };
    if let (Some(cap), Some(cur)) = (read("memory.max"), read("memory.current")) {
        let want = parse_mem_bytes(memory_max);
        if cur.saturating_add(want) > cap {
            return Err(Error::Runtime {
                context: "admission",
                message: format!(
                    "host protection: Delonix memory budget exhausted \
                     ({} MiB used of {} MiB; this container requests {}). \
                     Stop containers or raise DELONIX_RESERVE_PCT.",
                    cur / 1048576,
                    cap / 1048576,
                    memory_max
                ),
            });
        }
    }
    if let Some(load1) = host_load1() {
        let limit = host_ncpu() as f64 * 4.0;
        if load1 > limit {
            return Err(Error::Runtime {
                context: "admission",
                message: format!(
                    "host protection: load average too high ({load1:.1} > {limit:.0}) — try again later"
                ),
            });
        }
    }
    Ok(())
}

/// Maximum temperature (°C) among the host's thermal sensors (CPU and the like).
/// Basis of the thermal governor (#2): when Delonix heats up the machine, we lower
/// the slice's CPU ceiling to reduce the heat source.
pub fn max_cpu_temp_c() -> Option<u64> {
    let mut max: Option<u64> = None;
    if let Ok(rd) = std::fs::read_dir("/sys/class/thermal") {
        for e in rd.flatten() {
            if let Ok(s) = std::fs::read_to_string(e.path().join("temp")) {
                if let Ok(milli) = s.trim().parse::<i64>() {
                    if milli > 0 {
                        let c = (milli / 1000) as u64;
                        max = Some(max.map_or(c, |m| m.max(c)));
                    }
                }
            }
        }
    }
    max
}

/// The TOTAL CPU quota of delonix.slice (100% of the budget), in µs/period.
pub fn slice_full_cpu_quota() -> u64 {
    host_ncpu() * 100_000 / 100 * host_reserve_pct()
}

/// Sets the slice's `cpu.max` to `pct`% of the total budget — the thermal
/// governor lowers it to cool down and restores it when the temperature drops.
pub fn set_slice_cpu_pct(pct: u64) {
    ensure_delonix_slice();
    // Safety floor: never write quota 0 (`cpu.max "0 100000"` would freeze
    // ALL of the slice's containers). Guarantees at least ~1% of a core.
    let quota = (slice_full_cpu_quota() * pct.min(100) / 100).max(1_000);
    let _ = std::fs::write(
        format!("{}/cpu.max", delonix_runtime_core::DELONIX_SLICE),
        format!("{quota} 100000"),
    );
}

/// Best-effort: tries to set the controllable fans to max (if writable
/// `pwmN` exist). On many laptops the PWM is managed by firmware and is not
/// writable — so the real cooling is the slice *throttle*.
pub fn boost_fans() -> bool {
    let mut bumped = false;
    if let Ok(rd) = std::fs::read_dir("/sys/class/hwmon") {
        for e in rd.flatten() {
            for n in 1..=5 {
                let pwm = e.path().join(format!("pwm{n}"));
                if pwm.exists() {
                    let _ = std::fs::write(e.path().join(format!("pwm{n}_enable")), "1");
                    if std::fs::write(&pwm, "255").is_ok() {
                        bumped = true;
                    }
                }
            }
        }
    }
    bumped
}

/// State of Delonix's aggregate budget (for `system info`).
pub fn slice_budget() -> (u64, u64, u64, f64, u64) {
    ensure_delonix_slice();
    let slice = delonix_runtime_core::DELONIX_SLICE;
    let read = |f: &str| -> u64 {
        std::fs::read_to_string(format!("{slice}/{f}"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    };
    (
        read("memory.max"),
        read("memory.current"),
        read("pids.current"),
        host_load1().unwrap_or(0.0),
        host_ncpu(),
    )
}

/// Current cgroup v2 of the process (from `/proc/self/cgroup`, the `0::` line).
fn current_cgroup_v2() -> Option<String> {
    let s = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = s.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    Some(format!("/sys/fs/cgroup{rel}"))
}

/// Uncovers the real cgroup2 when `/sys` has been COVERED by a fresh sysfs (the
/// `ip netns exec` of the rootless-with-network path does that) — without the
/// `cgroup.controllers` visible NO cgroup operation works and the
/// container stayed in the session's INHERITED cgroup (0 metrics in the Console,
/// limits not applied). Same technique as `setup_node_cgroup_ns` (Kind
/// nodes), now available to the GENERAL path: makes `/` private (the umount does not
/// propagate; the `ip netns exec` already set `/` to rslave — defense in depth) and
/// unmounts the netns's `/sys`, revealing the cgroup2 underneath. It only acts on the
/// CALLER's mount-ns; readers of `/sys/class/net` (network metrics) always create
/// their own fresh mount-ns, so they are not affected. Best-effort.
pub fn reveal_cgroup2_if_masked() {
    if std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        return; // real cgroup2 already visible
    }
    let _ = mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    );
    let _ = umount2("/sys", MntFlags::empty());
}

/// Delonix's OWN cgroup base under the systemd --user delegated tree:
/// given the current cgroup (absolute, `/sys/fs/cgroup/...`), find the ancestor
/// `user@<uid>.service` (the delegation boundary — the files belong to the
/// user and systemd already activates `cpu memory pids` in its `subtree_control`)
/// and return `<ancestor>/dlx-containers`. The session's CURRENT cgroup
/// (e.g. `app-*.scope`) is POPULATED — the no-internal-processes rule refuses
/// `subtree_control` there; this escape gives us an EMPTY, delegable base.
fn user_service_base(cur_abs: &str) -> Option<String> {
    let mut end = 0usize;
    for seg in cur_abs.split('/') {
        end += seg.len() + 1; // +1 for the '/'
        if seg.starts_with("user@") && seg.ends_with(".service") {
            // `end-1` = end of the segment (not counting the following '/').
            return Some(format!("{}/dlx-containers", &cur_abs[..end - 1]));
        }
    }
    None
}

/// ROOTLESS cgroup delegation for a **privileged** container (Kind nodes, which
/// verify that the `cpu` controller is delegated). Uses the process's own DELEGATED
/// cgroup (under `user@<uid>.service`, writable) as the base, moves the
/// delonix and the container to leaves (cgroup v2 no-internal-processes rule) and
/// enables `+cpu +memory +pids` in the base's `subtree_control` — passing the
/// controllers to the container's cgroup. Best-effort: returns `false` if the base
/// is not delegated/clean (e.g. shared scope without `cpu`), the caller falling back to
/// current behavior (no regression). Requires the engine in a delegated cgroup
/// (`systemd-run --user --scope -p Delegate=yes` or a user service).
fn setup_cgroup_delegated(c: &Container, pid: i32) -> bool {
    let cur = match current_cgroup_v2() {
        Some(b) => b,
        None => return false,
    };
    // Candidate 1: the CURRENT cgroup as base (works when delonix runs in a
    // `Delegate=yes` DEDICATED scope, e.g. `systemd-run --user --scope`). Moves
    // our process to a `dlx-mgr` to free the base.
    if try_delegated_base(&cur, c, pid, true) {
        return true;
    }
    // Candidate 2 (escape): the session's cgroup is POPULATED (the
    // no-internal-processes rule refuses `subtree_control`) — use an OWN base
    // `<user@uid.service>/dlx-containers` (empty, delegable; systemd already activates
    // `cpu memory pids` in the `user@` subtree). The common-ancestor rule
    // allows moving the pid from the session scope there (user's files).
    if let Some(base) = user_service_base(&cur) {
        if std::fs::create_dir_all(&base).is_ok() && try_delegated_base(&base, c, pid, false) {
            return true;
        }
    }
    false
}

/// Tries to use `base` as a delegated base: moves the container to `<base>/dlx-<id>`,
/// activates the controllers in the base's `subtree_control` and applies the limits on the
/// leaf. `move_self`: move our own process to `<base>/dlx-mgr` (needed
/// when the base is the CURRENT cgroup — otherwise our processes block the
/// subtree_control; unnecessary on the escape-base, which starts empty).
fn try_delegated_base(base: &str, c: &Container, pid: i32, move_self: bool) -> bool {
    if std::fs::metadata(format!("{base}/cgroup.subtree_control")).is_err() {
        return false; // base not writable/delegated
    }
    let leaf = format!("{base}/dlx-{}", c.id);
    if std::fs::create_dir_all(&leaf).is_err() {
        return false;
    }
    if move_self {
        // free the base of DIRECT processes (no-internal-processes) before
        // trying the subtree_control.
        let mgr = format!("{base}/dlx-mgr");
        if std::fs::create_dir_all(&mgr).is_ok() {
            let _ = std::fs::write(
                format!("{mgr}/cgroup.procs"),
                std::process::id().to_string(),
            );
        }
    }
    // 1) Delegate the base's controllers to the children BEFORE moving the container
    //    to the leaf: the accounting (memory.current/cpu.stat) only catches allocations
    //    made WITH the controller active — moving first left the init's pages
    //    uncounted (metrics at 0 despite the right leaf). One by one; fails if
    //    the base has direct processes (shared scope) → fallback.
    let mut any = false;
    for ctrl in ["+cpuset", "+cpu", "+io", "+memory", "+pids"] {
        if std::fs::write(format!("{base}/cgroup.subtree_control"), ctrl).is_ok() {
            any = true;
        }
    }
    if !any {
        return false; // no delegation (no-internal-processes or no permission)
    }
    // 2) Limits on the leaf (controllers already active) — BEFORE the process enters,
    //    so the ceiling holds from the first allocation.
    let _ = std::fs::write(format!("{leaf}/memory.max"), &c.memory_max);
    let _ = std::fs::write(format!("{leaf}/pids.max"), DEFAULT_PIDS_MAX);
    let _ = std::fs::write(format!("{leaf}/cpu.max"), cpu_max_value(&c.cpus));
    // 3) Only now does the container enter the leaf.
    if std::fs::write(format!("{leaf}/cgroup.procs"), pid.to_string()).is_err() {
        return false;
    }
    true
}

fn setup_cgroup(c: &Container, pid: i32) -> Result<()> {
    // Rootless: ALWAYS tries cgroup delegation (cpu/memory/pids) in the user's
    // delegated cgroup (systemd --user with `Delegate=yes`) — it is Podman's model
    // and the way to get REAL limits without root. Before, it was only tried with
    // `--privileged` (Kind nodes), leaving EVERY rootless-with-network container without
    // memory.max/pids.max/cpu.max — a fork-bomb/leak killed the host. If
    // delegation does not exist, it returns false and falls to the best-effort path below.
    // EXCEPT Kind nodes (privileged + io.x-k8s.kind* labels): those manage
    // their own cgroup in the CHILD (`setup_node_cgroup_ns` — sibling leaf with cpu
    // delegated + empty cgroup-ns root, kubelet invariants). Placing them
    // here in the parent changed the base the child uses and broke that validated dance.
    let kind_node = c.labels.keys().any(|k| k.starts_with("io.x-k8s.kind"));
    if !kind_node && (is_rootless() || in_userns()) && setup_cgroup_delegated(c, pid) {
        return Ok(());
    }
    ensure_delonix_slice(); // the parent-slice with the aggregate limits (robustness)
    let cgroup = c.cgroup();
    let cg = cgroup.as_str();
    // Rootless (A13): without cgroup delegation (systemd), a non-root cannot
    // write to `/sys/fs/cgroup`. The limits become best-effort — the
    // namespace/seccomp isolation remains. (Like rootless Podman.)
    // Also `in_userns()`: the rootless ingress runs the spawn INSIDE the holder's userns
    // (MAPPED uid 0) — `is_rootless()` (geteuid) would be false, but there is no
    // cgroup delegation all the same; we treat it as rootless.
    if std::fs::create_dir_all(cg).is_err() {
        // `DELONIX_NO_CGROUP_WARN`: the caller already warned and does not want the
        // block repeated. This is how `cluster create` silences it: each node starts by
        // a RE-EXEC (new process), so the `Once` below — which only dedups within
        // ONE process — was not enough; a `--workers 3` showed the warning 4×.
        // The env var propagates through the re-exec chain (the children inherit it), the
        // `Once` handles the rest (a single process starting N containers).
        if is_rootless() || in_userns() {
            // The warning is about the ENVIRONMENT (there is no cgroup delegation in this
            // session), not about this container — hence ONCE per process, and
            // silenceable via env when the caller already warned (see above).
            let calado = std::env::var_os("DELONIX_NO_CGROUP_WARN").is_some();
            if !calado {
                static AVISO: std::sync::Once = std::sync::Once::new();
                AVISO.call_once(|| {
                    eprintln!(
                        "delonix: warning — rootless WITHOUT cgroup delegation: memory/cpu/pids are\n\
                         \x20        NOT enforced (a fork-bomb or leak may affect the host). To get\n\
                         \x20        limits, run the engine under a systemd --user session with\n\
                         \x20        delegation: `systemctl --user edit --force --full delonix.service`\n\
                         \x20        with `[Service] Delegate=yes`, or start it via `systemd-run --user\n\
                         \x20        --scope -p Delegate=yes ...`. Namespace/seccomp isolation\n\
                         \x20        remains intact."
                    );
                });
            }
            return Ok(());
        }
        return Err(Error::Runtime {
            context: "cgroup",
            message: format!("could not create {cg}"),
        });
    }
    // device cgroup (eBPF): denies block devices (host disks). Best-effort
    // (kernels without BPF_CGROUP_DEVICE). If it fails, warn instead of ignoring silently:
    // the primary protection remains (without CAP_MKNOD device nodes cannot be created, and
    // `bind_devices` refuses block devices), but the operator should know that this layer
    // is not active.
    if !attach_device_filter(cg) {
        eprintln!(
            "delonix: warning — device cgroup (eBPF) not applied on {}; block devices rely on caps/seccomp only",
            c.name
        );
    }
    write_limit(cg, "memory.max", &c.memory_max)?; // memory ceiling (kernel OOM-kill)
                                                   // no swap beyond memory, otherwise the memory limit would be bypassable;
                                                   // best-effort: the swap controller may be disabled on the system.
    let _ = std::fs::write(format!("{cg}/memory.swap.max"), "0");
    write_limit(cg, "cpu.max", &cpu_max_value(&c.cpus))?; // CPU ceiling
    write_limit(cg, "pids.max", DEFAULT_PIDS_MAX)?; // anti fork-bomb
                                                    // --- scheduling / QoS (cgroup v2, best-effort) ---
    if let Some(w) = &c.cpu_weight {
        let _ = std::fs::write(format!("{cg}/cpu.weight"), w); // CPU priority
    }
    if let Some(set) = &c.cpuset {
        let _ = std::fs::write(format!("{cg}/cpuset.cpus"), set); // core pinning
    }
    if let Some(w) = &c.io_weight {
        let _ = std::fs::write(format!("{cg}/io.weight"), w); // I/O priority
    }
    std::fs::write(format!("{cg}/cgroup.procs"), pid.to_string())?;
    Ok(())
}

/// The startup specification of a container. Gathers the options that
/// grew over the phases: `detach` (1), `mounts`/volumes (4),
/// `new_netns`+`on_started` (3).
#[derive(Default)]
pub struct RunSpec<'a> {
    /// Runs in the background (does not wait for the `waitpid`).
    pub detach: bool,
    /// Creates its own *network namespace* (`CLONE_NEWNET`).
    pub new_netns: bool,
    /// **Pod IPC/UTS sharing**: the init PID of the pod's infra container. When
    /// set, this container does NOT create its own IPC/UTS namespaces — it
    /// `setns` into the infra's (`/proc/<pid>/ns/{ipc,uts}`), so the pod's
    /// containers share System V/POSIX IPC and the UTS/hostname. Safe in rootless
    /// because the `--pod` re-exec already put us in the holder's userns (which
    /// owns those namespaces), where `setns` has privilege — the reason the old
    /// `join_netns` setns failed no longer applies. The netns is joined earlier by
    /// the re-exec's `ip netns exec`. (PID sharing lands with `shareProcessNamespace`.)
    pub pod_infra_pid: Option<i32>,
    /// Volumes/bind mounts to inject into the rootfs.
    pub mounts: Vec<Mount>,
    /// Log file for the stdout/stderr (detached) — the "file" *log driver*.
    pub log_path: Option<String>,
    /// Writes each line in the CRI log format (`<rfc3339nano> stdout F <line>`),
    /// so `crictl`/kubelet can read the logs. Default: raw format.
    pub log_cri: bool,
    /// Creates a *user namespace* (`CLONE_NEWUSER`): the container's root stops
    /// being the host's root. Requires the write layer to be `chown`ed
    /// to [`USERNS_UID_BASE`] (the `delonix-cli` handles that).
    pub userns: bool,
    /// AppArmor profile to apply on the `execve` (must be loaded on the host).
    pub apparmor: Option<String>,
    /// SELinux context to apply on the `execve` (only on hosts where SELinux is the LSM).
    pub selinux: Option<String>,
    /// *Hook* called with the PID after startup (Phase 3 configures the network there).
    /// **With userns it runs BEFORE releasing the child** — see `spawn`: the network must
    /// be ready before the entrypoint starts.
    pub on_started: Option<&'a StartedHook<'a>>,
    /// Container IP to write into `/etc/hosts` (mapped to the hostname), as
    /// Docker/Podman do. `None` = only the loopback entries.
    pub hosts_ip: Option<String>,
    /// DNS server to write into the container's `/etc/resolv.conf` (Docker/Podman
    /// always generate this file). On a custom network it is the *gateway* — the
    /// ingress internal resolver, which resolves container/VM names and forwards the
    /// rest; with `-p` (slirp) it is the slirp DNS. `None` → the host's is copied
    /// (`--net host` containers inherit the machine's DNS).
    pub dns: Option<String>,
    /// Shares the host's *PID namespace* (`--host-pid`; CRI `namespace_options.pid
    /// = NODE`): the container sees the host's processes. By default, isolated.
    pub host_pid: bool,
    /// Shares the host's *IPC namespace* (`--host-ipc`; CRI `namespace_options.ipc
    /// = NODE`): the host's shared memory/queues. By default, isolated.
    pub host_ipc: bool,
    /// **Rootless ingress:** the process already runs INSIDE the ingress holder's
    /// user+network namespace (re-exec via `nsenter … ip netns exec`). It does not create
    /// `CLONE_NEWUSER` nor `CLONE_NEWNET` (inherits the holder's, already as uid 0), but
    /// treats the rootfs as `userns` (it is root in the inherited userns). See `delonix-net::infra`.
    pub inherit_userns: bool,
    /// image `USER`: uid/gid to switch to BEFORE the `exec` (Docker `User`).
    /// `None` or `Some(0)` = runs as root (uid 0) — the historical behavior.
    /// `Some(uid != 0)` makes the runtime (a) map a subuid range via
    /// `newuidmap` in rootless (otherwise the non-zero uid does not exist in the userns), (b)
    /// `chown` the rootfs to that uid/gid and (c) `setgid`/`setuid` before the `execve`.
    /// Needed for images that refuse root (e.g. Elasticsearch).
    pub run_uid: Option<u32>,
    pub run_gid: Option<u32>,
}

/// Creates and starts a container (without its own network) — the Phase 1 signature.
pub fn create(store: &Store, container: &mut Container, rootfs: &str, detach: bool) -> Result<()> {
    spawn(
        store,
        container,
        rootfs,
        &RunSpec {
            detach,
            userns: container.userns, // honors the userns (needed in rootless)
            ..Default::default()
        },
    )
}

/// Like [`create`], but with its own *network namespace* and a CNI *hook*.
pub fn create_networked(
    store: &Store,
    container: &mut Container,
    rootfs: &str,
    detach: bool,
    on_started: &StartedHook<'_>,
) -> Result<()> {
    spawn(
        store,
        container,
        rootfs,
        &RunSpec {
            detach,
            new_netns: true,
            on_started: Some(on_started),
            ..Default::default()
        },
    )
}

/// The general entry point (Phase 4): starts a container per a
/// [`RunSpec`] — combines volumes, network and detached mode.
pub fn create_with(
    store: &Store,
    container: &mut Container,
    rootfs: &str,
    spec: &RunSpec<'_>,
) -> Result<()> {
    spawn(store, container, rootfs, spec)
}

/// Writes `/etc/hostname` and `/etc/hosts` into the rootfs, as Docker/Podman (which
/// always manage these files). Done in the PARENT, before the clone, because it is here
/// that the name and IP are known; the rootfs is a host path (flat copy in
/// rootless, mounted overlay in root).
///
/// Why both:
/// - **`/etc/hostname`**: the `sethostname` of `container_init` is not enough in a
///   container with systemd — systemd RE-READS `/etc/hostname` at startup and
///   overrides it (a Kind node ended up with the image's `debuerreotype`).
/// - **`/etc/hosts`**: without it, `getent ahostsv4 $(hostname)` does not resolve — and it is
///   EXACTLY how the `kindest/node` entrypoint discovers the node's IP
///   ("detected IPv4 address:"); it came empty and the node did not start as control-plane.
fn write_etc_files(rootfs: &str, hostname: &str, ip: Option<&str>, dns: Option<&str>) {
    let etc = format!("{rootfs}/etc");
    if std::fs::metadata(&etc).is_err() {
        return; // image without /etc (e.g. scratch) — nothing to do
    }
    let _ = std::fs::write(format!("{etc}/hostname"), format!("{hostname}\n"));
    // `/etc/resolv.conf`: without it the libc resolver falls back to 127.0.0.1 and NOTHING
    // resolves by name (only by IP). On a custom network it points to the gateway (the
    // ingress resolver); on `--net host` the host's is copied. Like Docker.
    match dns {
        Some(server) => {
            let _ = std::fs::write(
                format!("{etc}/resolv.conf"),
                format!("nameserver {server}\noptions ndots:0\n"),
            );
        }
        None => {
            if let Ok(host_resolv) = std::fs::read("/etc/resolv.conf") {
                let _ = std::fs::write(format!("{etc}/resolv.conf"), host_resolv);
            }
        }
    }
    let mut hosts = String::from(
        "127.0.0.1\tlocalhost\n\
         ::1\tlocalhost ip6-localhost ip6-loopback\n\
         fe00::0\tip6-localnet\n\
         ff00::0\tip6-mcastprefix\n\
         ff02::1\tip6-allnodes\n\
         ff02::2\tip6-allrouters\n",
    );
    if let Some(ip) = ip {
        hosts.push_str(&format!("{ip}\t{hostname}\n"));
    }
    let _ = std::fs::write(format!("{etc}/hosts"), hosts);
}

fn spawn(store: &Store, container: &mut Container, rootfs: &str, spec: &RunSpec<'_>) -> Result<()> {
    // `--hostname` (CRI `PodSandboxConfig.hostname`) overrides the container's
    // name in the UTS namespace and in `/etc/hostname`+`/etc/hosts`; without it,
    // the name is used (historical behavior).
    let hostname = container
        .hostname
        .clone()
        .unwrap_or_else(|| container.name.clone());
    write_etc_files(
        rootfs,
        &hostname,
        spec.hosts_ip.as_deref(),
        spec.dns.as_deref(),
    );
    let argv: Vec<CString> = container
        .command
        .iter()
        .map(|a| {
            CString::new(a.as_str()).map_err(|_| Error::Invalid(format!("invalid argument: {a:?}")))
        })
        .collect::<Result<_>>()?;
    if argv.is_empty() {
        return Err(Error::Invalid("empty command".into()));
    }

    let rootfs_owned = rootfs.to_string();
    let detach = spec.detach;
    let mounts = spec.mounts.clone();
    let apparmor = spec.apparmor.clone();
    let selinux = spec.selinux.clone();
    let pod_infra_pid = spec.pod_infra_pid;
    let env = container.env.clone();
    let read_only = container.read_only;
    // --privileged: keeps ALL caps + seccomp unconfined + cgroupns + /sys RW
    // (see setup_rootfs). Strictly gated — the non-privileged path is identical.
    let privileged = container.privileged;
    let cap_keep = if privileged {
        all_caps_mask()
    } else {
        resolve_cap_keep(&container.cap_drop, &container.cap_add)
    };
    let seccomp_unconfined = privileged || container.seccomp.as_deref() == Some("unconfined");
    let seccomp_detect = container.seccomp.as_deref() == Some("detect");
    let devices = container.devices.clone();
    let tmpfs = container.tmpfs.clone();
    let ulimits = container.ulimits.clone();
    let sysctls = container.sysctls.clone();

    // Console (pty) for PID 1: ONLY in `--privileged` detached with log (Kind
    // nodes, which run systemd as PID 1). Gives a real `/dev/console` whose output
    // — including the systemd boot state — is captured in the log file, so
    // that `docker logs -f` (= what Kind uses to detect readiness) sees it.
    // The NON-privileged path stays byte-for-byte identical (no pty, normal pipe).
    let console = privileged && detach && spec.log_path.is_some();

    // Logging shim: in detached, the container's stdout/stderr go through a pipe to
    // a `log_shim` process that writes to `log_path` WITH size-based rotation. In
    // console mode the "pipe" is instead the MASTER of the pty (received from the container), so
    // no pipe is created here.
    let log_pipe: Option<(i32, i32)> = match (detach && !console, &spec.log_path) {
        (true, Some(_)) => {
            let mut fds = [0i32; 2];
            // SAFETY: pipe() fills 2 fds.
            if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
                Some((fds[0], fds[1]))
            } else {
                None
            }
        }
        _ => None,
    };
    let log_fd = log_pipe.map(|(_, w)| w); // the container writes to the write end

    // Socketpair of the *console socket* (runc): the init allocates the pty in the container's
    // devpts and returns the master through here. `(parent, child)`; the child inherits both in the
    // clone and closes the parent's (see `setup_console`).
    let console_sock: Option<(i32, i32)> = if console {
        let mut sv = [0i32; 2];
        // SAFETY: socketpair() fills 2 fds (AF_UNIX/SOCK_DGRAM, as in `exec`).
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sv.as_mut_ptr()) } == 0 {
            Some((sv[0], sv[1]))
        } else {
            None
        }
    } else {
        None
    };

    // Isolation by default: mount, PID, UTS and **IPC** (System V/POSIX). The isolated
    // IPC prevents a container from seeing/altering the host's shared memory and message
    // queues (like Docker). `--host-pid`/`--host-ipc` (and the CRI
    // `namespace_options: NODE`) waive that isolation.
    // Pod IPC/UTS sharing: a pod member does NOT create its own IPC/UTS — it will
    // `setns` into the infra container's (in `container_init`), so the pod shares
    // System V/POSIX IPC and the UTS/hostname. (PID stays isolated here; sharing
    // it is `shareProcessNamespace`, handled separately.)
    let join_pod = spec.pod_infra_pid.is_some();
    let mut flags = CloneFlags::CLONE_NEWNS;
    if !join_pod {
        flags |= CloneFlags::CLONE_NEWUTS;
    }
    if !spec.host_pid {
        flags |= CloneFlags::CLONE_NEWPID;
    }
    if !spec.host_ipc && !join_pod {
        flags |= CloneFlags::CLONE_NEWIPC;
    }
    // Rootless ingress: we inherit the holder's netns + userns (we are already there via
    // nsenter), so we do NOT create our own. Only the mount/pid/ipc/uts ones.
    if spec.new_netns && !spec.inherit_userns {
        flags |= CloneFlags::CLONE_NEWNET;
    }
    let userns = spec.userns && !spec.inherit_userns;
    if userns {
        flags |= CloneFlags::CLONE_NEWUSER;
    }
    // --privileged: its own cgroup namespace (the systemd inside the container sees
    // its cgroup as root and can delegate sub-cgroups). We do NOT create it here: the
    // `container_init` does `unshare(CLONE_NEWCGROUP)` AFTER moving to a
    // dedicated leaf `dlx-<id>` (see `setup_node_cgroup_ns`), so that the node's
    // cgroup-ns root is EMPTY (the no-internal-processes rule the kubelet
    // requires) — instead of anchoring in the cgroup-scope shared with `kind`.
    // Sync pipe: the child waits for the parent to map the uid/gid (user ns).
    let sync = if userns {
        let mut fds = [0i32; 2];
        // SAFETY: pipe() fills the 2-fd array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(Error::Runtime {
                context: "pipe",
                message: "failed to create pipe".into(),
            });
        }
        Some((fds[0], fds[1]))
    } else {
        None
    };

    let host_pid = spec.host_pid;
    let inherit_userns = spec.inherit_userns;
    let run_uid = spec.run_uid;
    let run_gid = spec.run_gid;
    // --secret-files: reads+decrypts the values NOW (on the host, before the child's pivot)
    // to write them into an in-namespace tmpfs. Only names/root touch outside memory;
    // the values are captured (moved) into the clone's closure = the child's memory.
    let secret_files: Vec<(String, String)> =
        if container.secret_files && !container.secrets.is_empty() {
            match delonix_runtime_core::SecretStore::open(store.base()) {
                Ok(ss) => {
                    let mut map = std::collections::BTreeMap::new();
                    for n in &container.secrets {
                        if let Ok(s) = ss.load(n) {
                            for (k, v) in s.data {
                                map.insert(k, v);
                            }
                        }
                    }
                    map.into_iter().collect()
                }
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };
    // image CWD (OCI WorkingDir) — captured for the child to `chdir` before the exec.
    let workdir = container.workdir.clone();
    // Container id: used for the Kind node's dedicated cgroup leaf.
    let cid = container.id.clone();
    // KIND node (label `io.x-k8s.kind.*`): only these get the dedicated cgroup-ns —
    // a normal `--privileged` container keeps its cgroup hierarchy intact.
    let node_cgroup = privileged
        && container
            .labels
            .keys()
            .any(|k| k.starts_with("io.x-k8s.kind"));
    let mut stack = vec![0u8; 1024 * 1024];
    let cb = Box::new(move || {
        container_init(
            &rootfs_owned,
            &hostname,
            &argv,
            detach,
            log_fd,
            &mounts,
            sync,
            apparmor.as_deref(),
            selinux.as_deref(),
            pod_infra_pid,
            &env,
            read_only,
            cap_keep,
            seccomp_unconfined,
            seccomp_detect,
            &devices,
            &tmpfs,
            &ulimits,
            &sysctls,
            host_pid,
            inherit_userns,
            run_uid,
            run_gid,
            privileged,
            console_sock,
            &secret_files,
            workdir.as_deref(),
            &cid,
            node_cgroup,
        )
    });

    // SAFETY: single-threaded; the child mounts the container and does `exec`.
    let pid = unsafe { clone(cb, &mut stack, flags, Some(Signal::SIGCHLD as i32)) }
        .map_err(syserr("clone"))?;

    // CRITICAL ORDER: the user namespace handshake (releasing the child with the byte
    // "GO") MUST come BEFORE the console recv. In console mode the init only allocates the
    // pty and sends the master AFTER receiving the GO; if the parent blocked on recv_fd
    // before writing the GO, it would deadlock (parent waits for the master, child waits for the
    // GO). Hence: 1st userns, 2nd console+log_shim.

    // User namespace: the parent maps the uid/gid and releases the child via the pipe.
    // Network already configured by the hook before the GO? (only on the userns/sync path)
    let mut net_done = false;
    if let Some((r, w)) = sync {
        // SAFETY: the parent closes the read and uses the write to release the child.
        unsafe {
            libc::close(r);
        }
        // Subuid map (range) by default in rootless when there are helpers
        // `newuidmap`/`newgidmap` + /etc/subuid: besides allowing the image's USER≠0,
        // it lets the entrypoints that `chown` to service uids work —
        // e.g.: nginx chowns the caches to uid 101; with a single-uid map that
        // gave `chown(...) failed (22: Invalid argument)` and the container exited. Without the
        // helpers, it keeps the single-uid map (historical behavior). Does not affect
        // ingress containers (they inherit the holder's userns).
        let want_range = run_uid.map(|u| u != 0).unwrap_or(false) || have_subid_helpers();
        if let Err(e) = write_userns_maps(pid.as_raw(), want_range) {
            unsafe {
                libc::close(w);
            }
            let _ = kill(pid, Signal::SIGKILL);
            return Err(e);
        }
        // NETWORK BEFORE THE GO (critical order): the child is still BLOCKED waiting
        // for this byte, so it is here — and only here — that we can guarantee the
        // network is ready BEFORE the entrypoint runs. With the hook after the GO
        // there was a real race: `slirp4netns` configures `tap0` from the PARENT
        // while the child is already running, and an entrypoint that reads the IP ONCE at
        // startup (e.g. that of `kindest/node`: "detected IPv4 address:") saw it
        // EMPTY and the node died. Docker/podman also only start the process with the
        // network already up. Without userns (sync=None) there is no blocking point: the
        // hook runs further below, as before (the race remains, but that path
        // is not the rootless/Kind one).
        let net_err = spec.on_started.and_then(|hook| hook(pid.as_raw()).err());
        // SAFETY: writes 1 byte (the "you may proceed") and closes the write.
        unsafe {
            let go = [1u8; 1];
            let _ = libc::write(w, go.as_ptr() as *const libc::c_void, 1);
            libc::close(w);
        }
        if let Some(e) = net_err {
            let _ = kill(pid, Signal::SIGKILL);
            return Err(e);
        }
        net_done = true;
    }

    // Log source: in console mode it is the pty MASTER (received from the init by
    // SCM_RIGHTS, ALREADY after the userns GO above); otherwise the READ end
    // of the stdout/stderr pipe.
    let log_src: Option<i32> = if let Some((csp, csc)) = console_sock {
        // SAFETY: the parent drops the child's end and receives the master from the init.
        unsafe { libc::close(csc) };
        // Receive timeout: if the init DIES before sending the pty master
        // (e.g. the kindest/node entrypoint aborts early) and a reparented grandchild
        // holds the socketpair end, the `recvmsg` would block FOREVER — the
        // `run` would hang without log nor exit. With SO_RCVTIMEO, after 10s
        // it gives up (None → no log, but the container proceeds and the status reconciles).
        unsafe {
            let tv = libc::timeval {
                tv_sec: 10,
                tv_usec: 0,
            };
            libc::setsockopt(
                csp,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&tv as *const libc::timeval).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
        }
        let m = recv_fd(csp);
        unsafe { libc::close(csp) };
        m // None if the init could not allocate the pty (falls through without log, without blocking)
    } else {
        log_pipe.map(|(r, _)| r)
    };

    // Starts the logging shim (reads the pipe/master, writes the log with rotation). Reparented
    // to the init when `delonix run` finishes; dies when the container closes the source.
    if let Some(src) = log_src {
        let lp = spec.log_path.clone().unwrap_or_default();
        let driver = container.log_driver.clone().unwrap_or_default();
        let tag = format!("delonix/{}", container.name);
        // SAFETY: fork of a single-threaded process; the child-shim only does I/O and _exit.
        if let Ok(ForkResult::Child) = unsafe { fork() } {
            // Drops the WRITE end of the pipe (if it exists) — only the container keeps it.
            if let Some((_, logw)) = log_pipe {
                unsafe { libc::close(logw) };
            }
            // The shim outlives `delonix run` (it lives as long as the container lives).
            // It must DROP the stdio inherited from the parent — otherwise a caller that captures
            // the stdout of `run -d` (the Docker shim, `$(...)`, CI/scripts) stays
            // blocked waiting for EOF until the container dies. setsid + /dev/null
            // detach it completely; the shim writes to its own log file.
            unsafe {
                libc::setsid();
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if null >= 0 {
                    libc::dup2(null, 0);
                    libc::dup2(null, 1);
                    libc::dup2(null, 2);
                    if null > 2 {
                        libc::close(null);
                    }
                }
            }
            log_shim(src, lp, MAX_LOG_BYTES, driver, tag, spec.log_cri); // does not return (the parent does not wait)
        }
        // The parent drops the ends: only the container (the source, via fd 1/2 or the pty
        // slave) and the shim (src) keep them. When the container dies, the shim sees EOF/EIO.
        unsafe { libc::close(src) };
        if let Some((_, logw)) = log_pipe {
            unsafe { libc::close(logw) };
        }
    }

    container.pid = Some(pid.as_raw());
    container.pid_starttime = proc_starttime(pid.as_raw());
    container.status = Status::Running;
    // A fresh, successful start makes any earlier crash diagnosis stale — clear it so
    // `describe`/`ls` don't keep pointing at a cause that no longer applies.
    container.crash_reason = None;
    container.crashed_at = None;
    setup_cgroup(container, pid.as_raw())?;
    store.save(container)?;

    // Configures the network (or other startup) BEFORE waiting/returning. Only the
    // path WITHOUT userns reaches here (no sync point to block the child on) — with
    // userns the hook already ran before the GO, with the child stopped (see above).
    if !net_done {
        if let Some(hook) = spec.on_started {
            if let Err(e) = hook(pid.as_raw()) {
                let _ = kill(pid, Signal::SIGKILL);
                remove_container_cgroup(container);
                return Err(e);
            }
        }
    }

    if detach {
        return Ok(());
    }

    let status = waitpid(pid, None).map_err(syserr("waitpid"))?;
    container.status = wait_to_status(status);
    container.pid = None;
    store.save(container)?;
    remove_container_cgroup(container);
    Ok(())
}

/// Waits for the container to terminate and **records the REAL state** (with the exit code) in
/// the `Store`. Returns the final state.
///
/// Only works for whoever called [`create_with`] — because `waitpid` is only
/// permitted to the **parent** of the process. That is its reason to exist: in a normal `run -d`
/// the CLI exits right away, the container is reparented to the host `init` and the
/// exit code dies with it — `reconcile_status` can then only
/// say "it died" (`Crashed`/137), never *why*. A supervisor that calls
/// `create_with` and then this becomes the container's parent and captures the
/// true code (`Failed(n)`), which is what an `on-failure` restart policy
/// needs to know to decide.
pub fn wait_and_record(store: &Store, container: &mut Container) -> Result<Status> {
    let pid = container
        .pid
        .ok_or_else(|| Error::NotRunning(container.short_id().to_string()))?;
    let st = waitpid(Pid::from_raw(pid), None).map_err(syserr("waitpid"))?;
    let status = wait_to_status(st);
    // `update` (flock) and not `save`: the CRI/CLI may be reconciling the
    // same container right now — see `Store::update`.
    let final_status = status.clone();
    let _ = store.update(&container.id, |c| {
        c.status = final_status.clone();
        c.pid = None;
        true
    });
    container.status = status.clone();
    container.pid = None;
    remove_container_cgroup(container);
    Ok(status)
}

// ----------------------------------------------------------------------------
// Exec: run a command inside an existing container
// ----------------------------------------------------------------------------

/// Runs `argv` inside the container's namespaces, via `setns`.
///
/// Uses a **double fork**: the 1st child stays single-threaded (kernel requirement
/// for `setns` to the *user namespace*); does `setns` to all namespaces; and the
/// 2nd child — created after joining the *pid namespace* — is the one that actually
/// enters that namespace (`setns(PID)` only affects future children).
/// Allocates a pseudo-terminal (master, slave) with the current terminal's size.
/// Uses `posix_openpt` (without libutil). `None` if not possible.
/// Allocates a pty from the **container's** devpts (`/dev/ptmx` → `pts/ptmx`).
/// Runs in the grandchild (already inside the container's mnt ns), so the resulting `/dev/pts/N`
/// resolves inside it — `tty` prints the right name, just like
/// Docker. Returns `(master, slave, slave_path)`; the master is sent to the parent
/// by SCM_RIGHTS and the `path` (`/dev/pts/N`) serves for the `/dev/console` bind.
fn open_pty_in_container() -> Option<(i32, i32, String)> {
    unsafe {
        let m = libc::open(c"/dev/ptmx".as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if m < 0 {
            return None;
        }
        if libc::grantpt(m) != 0 || libc::unlockpt(m) != 0 {
            libc::close(m);
            return None;
        }
        let mut buf = [0 as libc::c_char; 128];
        if libc::ptsname_r(m, buf.as_mut_ptr(), buf.len()) != 0 {
            libc::close(m);
            return None;
        }
        let path = std::ffi::CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .into_owned();
        let s = libc::open(buf.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if s < 0 {
            libc::close(m);
            return None;
        }
        Some((m, s, path))
    }
}

/// Gives a `/dev/console` (pty) to PID 1 of a `--privileged` detached container and
/// captures it into the log file. The `master` goes to the parent (which wires it to the
/// `log_shim`), the `slave` becomes `/dev/console` + stdio — it is where **systemd**,
/// as PID 1, writes the boot state (e.g. "Reached target Multi-User
/// System"). Without this that state went only to the *journal*, invisible to the
/// `docker logs -f` that Kind uses to detect the node ready. runc *console
/// socket* model. Runs in the container's init (already with the `/dev`/devpts mounted and
/// still with caps), before dropping privileges and the `execve`.
fn setup_console(console_sock: (i32, i32)) {
    let (sp, sc) = console_sock;
    // SAFETY: the init does not use the parent's end of the socketpair.
    unsafe { libc::close(sp) };
    let Some((m, s, path)) = open_pty_in_container() else {
        unsafe { libc::close(sc) };
        return;
    };
    // Delivers the master to the parent (which pumps it to the log) and drops it here.
    send_fd(sc, m);
    // SAFETY: master and socket are no longer needed inside the container.
    unsafe {
        libc::close(m);
        libc::close(sc);
    }
    // `/dev/console` = bind of the slave's node (char device of the pty). It is what systemd
    // opens by name to print the boot state. Best-effort.
    let _ = std::fs::File::create("/dev/console"); // mount point
    let _ = mount(
        Some(path.as_str()),
        "/dev/console",
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    );
    // New session + controlling tty on the slave + stdio = slave (runc
    // `terminal:true` model). PID 1 (child of the clone) is not a group leader → the setsid
    // succeeds; systemd inherits this and writes to the captured pty.
    // SAFETY: direct FFI over the valid slave; best-effort.
    unsafe {
        libc::setsid();
        libc::ioctl(s, libc::TIOCSCTTY as _, 0);
        libc::dup2(s, 0);
        libc::dup2(s, 1);
        libc::dup2(s, 2);
        if s > 2 {
            libc::close(s);
        }
    }
}

/// Sends an fd over a Unix socket (SCM_RIGHTS). The grandchild allocates the pty in the container's
/// devpts and passes the `master` to the parent through here (runc *console socket* model).
fn send_fd(sock: i32, fd: i32) -> bool {
    unsafe {
        let mut dummy: u8 = 0;
        let mut iov = libc::iovec {
            iov_base: (&mut dummy as *mut u8).cast::<libc::c_void>(),
            iov_len: 1,
        };
        let mut cmsgbuf = [0u8; 64];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsgbuf.as_mut_ptr().cast::<libc::c_void>();
        // `as _`: the type of the cmsg fields differs between glibc (size_t) and musl (socklen_t).
        msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
        std::ptr::copy_nonoverlapping(&fd, libc::CMSG_DATA(cmsg).cast::<libc::c_int>(), 1);
        libc::sendmsg(sock, &msg, 0) >= 0
    }
}

/// Receives an fd sent by SCM_RIGHTS (the parent receives the grandchild's pty master).
fn recv_fd(sock: i32) -> Option<i32> {
    unsafe {
        let mut dummy: u8 = 0;
        let mut iov = libc::iovec {
            iov_base: (&mut dummy as *mut u8).cast::<libc::c_void>(),
            iov_len: 1,
        };
        let mut cmsgbuf = [0u8; 64];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsgbuf.as_mut_ptr().cast::<libc::c_void>();
        msg.msg_controllen = cmsgbuf.len() as _;
        if libc::recvmsg(sock, &mut msg, 0) < 0 {
            return None;
        }
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return None;
        }
        let mut fd: libc::c_int = -1;
        std::ptr::copy_nonoverlapping(libc::CMSG_DATA(cmsg).cast::<libc::c_int>(), &mut fd, 1);
        if fd < 0 {
            None
        } else {
            Some(fd)
        }
    }
}

/// Copies bytes from one fd to another until EOF (one direction of the pty proxy).
fn pump_fd(from: i32, to: i32) {
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(from, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
        let mut off = 0isize;
        while off < n {
            let w = unsafe {
                libc::write(
                    to,
                    buf.as_ptr().offset(off) as *const libc::c_void,
                    (n - off) as usize,
                )
            };
            if w <= 0 {
                return;
            }
            off += w;
        }
    }
}

/// Puts the user's terminal in raw mode (for the interactive shell). Returns
/// the previous state to restore. `None` if the stdin is not a terminal.
fn set_raw_mode() -> Option<libc::termios> {
    unsafe {
        if libc::isatty(0) == 0 {
            return None;
        }
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return None;
        }
        let saved = t;
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(0, libc::TCSANOW, &t);
        Some(saved)
    }
}

fn restore_mode(saved: Option<libc::termios>) {
    if let Some(t) = saved {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &t);
        }
    }
}

pub fn exec(container: &Container, argv: &[String], tty: bool) -> Result<i32> {
    // Guard against PID reuse: the `exec` enters the namespaces via
    // setns(pid) — if the PID was recycled, we would enter the namespaces of a
    // process belonging to the host. We require the same `starttime`.
    let pid = container
        .pid
        .filter(|p| safe_to_signal(*p, container.pid_starttime))
        .ok_or_else(|| Error::NotRunning(container.short_id().to_string()))?;

    // Joins the user namespace FIRST (to gain caps in that ns and be able to join the
    // rest); then UTS, NET, PID and MNT (mnt last). The `user` is ALWAYS included:
    // the skip-by-inode logic below removes it if we already share it (host
    // container). Crucial for the rootless INGRESS containers, which **inherit** the holder's user ns
    // (they do not create their own) and therefore had `container.userns=false` — without the
    // setns(user) the setns(uts) gave EPERM (the UTS belongs to that user ns).
    let ns_list: &[&str] = &["user", "uts", "net", "pid", "mnt"];
    // Open the fds in the PARENT (they resolve in the host context); they are inherited by the fork.
    // Skip the namespaces we ALREADY share (same inode) — e.g. a container
    // with a user ns but no network shares the host's `net`, and joining it after
    // entering the user ns would give EPERM (we lose privilege over the host's ns).
    use std::os::unix::fs::MetadataExt;
    let self_pid = std::process::id();
    let mut fds: Vec<(&str, i32)> = Vec::new();
    for ns in ns_list {
        let target = format!("/proc/{pid}/ns/{ns}");
        let mine = format!("/proc/{self_pid}/ns/{ns}");
        if let (Ok(a), Ok(b)) = (std::fs::metadata(&target), std::fs::metadata(&mine)) {
            if a.ino() == b.ino() {
                continue; // we are already in this namespace
            }
        }
        let fd = open(
            target.as_str(),
            OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(syserr("open ns"))?;
        fds.push((ns, fd));
    }
    // Did we actually enter a user ns? (i.e., we did not already share it). If so, we become
    // uid 0 inside it after the setns — whether the container CREATED it (`userns`)
    // or INHERITED it from the ingress holder.
    let joined_userns = fds.iter().any(|(n, _)| *n == "user");

    let cargv: Vec<CString> = argv
        .iter()
        .map(|a| {
            CString::new(a.as_str()).map_err(|_| Error::Invalid(format!("invalid argument: {a:?}")))
        })
        .collect::<Result<_>>()?;

    // `exec -t`: the grandchild allocates a pty in the container's devpts and passes the master to the
    // PARENT over a socketpair (SCM_RIGHTS). This way the `/dev/pts/N` resolves inside the
    // container (`tty` prints the name) — runc *console socket* model.
    let pty_sock: Option<(i32, i32)> = if tty {
        let mut sv = [0i32; 2];
        // SAFETY: socketpair with valid arguments; sv filled on success.
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sv.as_mut_ptr()) } == 0 {
            Some((sv[0], sv[1]))
        } else {
            None
        }
    } else {
        None
    };

    // 1st fork: the child stays single-threaded.
    // SAFETY: the child only does simple syscalls and `_exit`.
    match unsafe { fork() }.map_err(syserr("fork"))? {
        ForkResult::Child => {
            for (ns, fd) in &fds {
                // SAFETY: valid inherited fd; `OwnedFd` closes it after the `setns`.
                let owned = unsafe { OwnedFd::from_raw_fd(*fd) };
                if let Err(e) = setns(owned, CloneFlags::empty()) {
                    eprintln!("delonix: setns({ns}) failed: {e}");
                    unsafe { libc::_exit(125) };
                }
            }
            // If we joined a user ns (created OR inherited), we become uid 0 INSIDE
            // (same as the container's init).
            // SAFETY: after setns(user) we have CAP_SETUID in the container's user ns.
            if joined_userns {
                unsafe {
                    libc::setgid(0);
                    libc::setuid(0);
                }
            }
            // 2nd fork: enters the already-joined pid namespace.
            // SAFETY: the grandchild does `exec` or `_exit`.
            match unsafe { fork() } {
                Ok(ForkResult::Child) => {
                    let _ = chdir("/");
                    // pty: allocates in the container's devpts, sends the master to the parent, and
                    // uses the slave as stdio (new session + controlling terminal).
                    if let Some((sp, sc)) = pty_sock {
                        unsafe { libc::close(sp) }; // the grandchild only uses its own side
                        if let Some((m, s, _path)) = open_pty_in_container() {
                            send_fd(sc, m);
                            unsafe {
                                libc::close(m);
                                libc::close(sc);
                                libc::setsid();
                                libc::ioctl(s, libc::TIOCSCTTY as _, 0);
                                libc::dup2(s, 0);
                                libc::dup2(s, 1);
                                libc::dup2(s, 2);
                                if s > 2 {
                                    libc::close(s);
                                }
                            }
                        } else {
                            unsafe { libc::close(sc) };
                        }
                    }
                    set_no_new_privs();
                    // Mirrors the INIT's confinement (see `spawn`): a `--privileged`
                    // container keeps ALL caps also in `exec` — without
                    // this, `exec` always fell back to the default set (KEPT_CAPS, without
                    // CAP_NET_ADMIN), and debugging a Kind node from inside (`nft`,
                    // `iptables`) gave "Operation not permitted" even though the init
                    // had the caps. Docker/podman: `exec` inherits the container's profile.
                    let exec_keep = if container.privileged {
                        all_caps_mask()
                    } else {
                        resolve_cap_keep(&container.cap_drop, &container.cap_add)
                    };
                    drop_capabilities(exec_keep); // same confinement
                    let exec_unconf =
                        container.privileged || container.seccomp.as_deref() == Some("unconfined");
                    apply_seccomp(exec_unconf, container.seccomp.as_deref() == Some("detect"));
                    // FAIL-CLOSED: the `exec` process must stay as confined as
                    // the container's init; aborts if some control failed silently.
                    if !insecure_besteffort() {
                        if let Err(e) = verify_confinement(!exec_unconf, exec_keep) {
                            eprintln!("delonix: exec confinement NOT verified ({e}); aborting");
                            unsafe { libc::_exit(126) };
                        }
                    }
                    apply_env(&container.name, &container.env); // same environment as the container
                    if let Some(p) = &container.apparmor {
                        apply_apparmor(p); // same MAC confinement as the init process
                    }
                    // `--user` (CRI `RunAsUser`/`RunAsUserName`): the `exec` runs as the
                    // SAME user as the init process — it is what the CRI conformance
                    // checks (`execSync id -u` == RunAsUser). Without this, the `exec`
                    // always stayed uid 0 of the userns and `id -u` reported 0. setgid
                    // BEFORE setuid (after dropping the uid one can no longer change group).
                    if let Some(uid) = container.run_uid.filter(|u| *u != 0) {
                        let gid = container.run_gid.unwrap_or(uid);
                        // SAFETY: uid/gid mapped in the container's userns (the init already
                        // mapped the range at startup); setgroups/setgid/setuid
                        // succeed while we are root in the userns.
                        unsafe {
                            libc::setgroups(1, [gid].as_ptr());
                            if libc::setgid(gid) != 0 {
                                eprintln!("delonix: exec setgid({gid}) failed");
                            }
                            if libc::setuid(uid) != 0 {
                                eprintln!("delonix: exec setuid({uid}) failed");
                                libc::_exit(126);
                            }
                        }
                    }
                    let _ = execvp(&cargv[0], &cargv);
                    unsafe { libc::_exit(127) };
                }
                Ok(ForkResult::Parent { child }) => {
                    // the middle does not hold the pty/socket (otherwise the master never gives EOF).
                    if let Some((sp, sc)) = pty_sock {
                        unsafe {
                            libc::close(sp);
                            libc::close(sc);
                        }
                    }
                    let code = waitpid(child, None).map(wait_to_code).unwrap_or(-1);
                    unsafe { libc::_exit((code & 0xff) as i32) };
                }
                Err(_) => unsafe { libc::_exit(126) },
            }
        }
        ForkResult::Parent { child } => {
            if let Some((sp, sc)) = pty_sock {
                unsafe { libc::close(sc) }; // the parent receives on its side
                let master = recv_fd(sp);
                unsafe { libc::close(sp) };
                if let Some(m) = master {
                    // adjusts the pty to the client terminal's size.
                    unsafe {
                        let mut ws: libc::winsize = std::mem::zeroed();
                        if libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) == 0 {
                            libc::ioctl(m, libc::TIOCSWINSZ, &ws);
                        }
                    }
                    // the parent talks to the master: stdin→master and master→stdout, in raw mode.
                    let saved = set_raw_mode();
                    std::thread::spawn(move || pump_fd(m, 1)); // master -> stdout
                    std::thread::spawn(move || pump_fd(0, m)); // stdin -> master
                    let status = waitpid(child, None).map_err(syserr("waitpid"));
                    restore_mode(saved);
                    unsafe { libc::close(m) };
                    return Ok(wait_to_code(status?));
                }
                // the grandchild could not allocate the pty: behaves as non-tty.
                let status = waitpid(child, None).map_err(syserr("waitpid"))?;
                Ok(wait_to_code(status))
            } else {
                let status = waitpid(child, None).map_err(syserr("waitpid"))?;
                Ok(wait_to_code(status))
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Volume hot-plug (zero-downtime): mount/unmount in a RUNNING container
// ----------------------------------------------------------------------------
//
// The kernel's new mount API (open_tree/move_mount, Linux 5.2+; mount_setattr
// 5.12+) allows mounting in a live container without stopping it. The key problem is the
// ROOTLESS model: after the `pivot_root` the source (host path) is NO longer
// visible inside the container's mnt ns, and the container's userns does not command the
// host's mnt ns. Solution (works in rootless AND root):
//   1. setns(user) → enter the container's userns (gain CAP_SYS_ADMIN there)
//   2. unshare(CLONE_NEWNS) → new mnt ns, a COPY of the host's (source visible),
//      but OWNED by the container's userns
//   3. open_tree(CLONE) → clone the source subtree into a DETACHED mount (fd)
//   4. setns(mnt) → enter the container's REAL mnt ns (root = container root)
//   5. move_mount(fd, target) → attach the mount; the same userns owns source and
//      destination, so the kernel authorizes it
// All in the child of a fork (single-threaded, requirement of setns(user)).

const OPEN_TREE_CLONE: libc::c_uint = 1;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x0000_0004;
const MOUNT_ATTR_RDONLY: u64 = 0x0000_0001;
const MOUNT_ATTR_NOSUID: u64 = 0x0000_0002;
const MOUNT_ATTR_NODEV: u64 = 0x0000_0004;

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

/// `open_tree(AT_FDCWD, src, OPEN_TREE_CLONE [|AT_RECURSIVE])` → fd of a detached mount
/// (copy of the subtree covering `src`). Error if the kernel does not support it.
fn open_tree_clone(src: &str, recursive: bool) -> nix::Result<OwnedFd> {
    let c = CString::new(src).map_err(|_| nix::errno::Errno::EINVAL)?;
    let mut flags = OPEN_TREE_CLONE | (OFlag::O_CLOEXEC.bits() as libc::c_uint);
    if recursive {
        flags |= libc::AT_RECURSIVE as libc::c_uint;
    }
    // SAFETY: syscall with a valid path (NUL-terminated CString) and valid flags.
    let fd = unsafe { libc::syscall(libc::SYS_open_tree, libc::AT_FDCWD, c.as_ptr(), flags) };
    if fd < 0 {
        return Err(nix::errno::Errno::last());
    }
    // SAFETY: fd >= 0 returned by the kernel, with ownership transferred to the OwnedFd.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// `move_mount(dfd, "", AT_FDCWD, target, MOVE_MOUNT_F_EMPTY_PATH)` → attaches the
/// detached mount `dfd` at `target` (resolved against the current root).
fn move_mount_to(dfd: RawFd, target: &str) -> nix::Result<()> {
    let empty = CString::new("").unwrap();
    let c = CString::new(target).map_err(|_| nix::errno::Errno::EINVAL)?;
    // SAFETY: valid dfd, NUL-terminated paths, valid flag.
    let r = unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            dfd,
            empty.as_ptr(),
            libc::AT_FDCWD,
            c.as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH,
        )
    };
    if r < 0 {
        return Err(nix::errno::Errno::last());
    }
    Ok(())
}

/// `mount_setattr(dfd, "", AT_EMPTY_PATH|AT_RECURSIVE, &attr)` — fixes attributes
/// (nosuid/nodev always, rdonly optional) on the detached mount before attaching it.
fn mount_setattr_fd(dfd: RawFd, attr_set: u64) -> nix::Result<()> {
    let empty = CString::new("").unwrap();
    let attr = MountAttr {
        attr_set,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    // SAFETY: valid dfd, struct of the declared size, valid flags.
    let r = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            dfd,
            empty.as_ptr(),
            (libc::AT_EMPTY_PATH | libc::AT_RECURSIVE) as libc::c_uint,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if r < 0 {
        return Err(nix::errno::Errno::last());
    }
    Ok(())
}

/// Opens the fd of a container namespace, skipping it if we ALREADY share it (same
/// inode) — joining it afterward would give EPERM. Returns `Ok(None)` if already shared.
fn open_container_ns(pid: i32, ns: &str) -> Result<Option<OwnedFd>> {
    use std::os::unix::fs::MetadataExt;
    let target = format!("/proc/{pid}/ns/{ns}");
    let mine = format!("/proc/{}/ns/{ns}", std::process::id());
    if let (Ok(a), Ok(b)) = (std::fs::metadata(&target), std::fs::metadata(&mine)) {
        if a.ino() == b.ino() {
            return Ok(None);
        }
    }
    let fd = open(
        target.as_str(),
        OFlag::O_RDONLY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(syserr("open ns"))?;
    // SAFETY: fd >= 0 from the kernel; ownership transferred to the OwnedFd.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// Mounts a bind-volume in a RUNNING container, without stopping it (hot-plug). See the
/// module comment for the setns/unshare/open_tree/move_mount sequence.
pub fn mount_live(container: &Container, m: &Mount) -> Result<()> {
    if !mount_target_safe(&m.target) {
        return Err(Error::Invalid(format!("unsafe mount target: {}", m.target)));
    }
    let pid = container
        .pid
        .filter(|p| safe_to_signal(*p, container.pid_starttime))
        .ok_or_else(|| Error::NotRunning(container.short_id().to_string()))?;
    let src_is_dir = std::fs::metadata(&m.source)
        .map_err(|_| Error::Invalid(format!("mount source does not exist: {}", m.source)))?
        .is_dir();

    // namespace fds (opened in the PARENT, in the host context; inherited by the fork).
    //
    // The `user` is ALWAYS opened, regardless of `container.userns` — `open_container_ns`
    // already returns `None` by inode comparison when the ns is the same as ours.
    // The `userns` field only says whether the container CREATED its own; the rootless ingress ones
    // INHERIT the holder's and end up with `userns=false` despite being in a userns
    // different from ours. Trusting the field, the `setns(user)` was skipped and the
    // following `unshare(NEWNS)` gave EPERM (without CAP_SYS_ADMIN in our userns) —
    // exactly the same bug that `exec` already had and fixed the same way
    // (see the `ns_list` comment in `exec`).
    let user_fd = open_container_ns(pid, "user")?;
    let mnt_fd = open_container_ns(pid, "mnt")?.ok_or_else(|| {
        Error::Invalid("container shares the host mnt ns — nothing to mount".into())
    })?;

    let mut attr = MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV;
    if m.readonly {
        attr |= MOUNT_ATTR_RDONLY;
    }
    let source = m.source.clone();
    let target = m.target.clone();

    // fork: the child stays single-threaded (requirement of setns(user)).
    // SAFETY: the child only does simple syscalls and `_exit`, without running destructors.
    match unsafe { fork() }.map_err(syserr("fork"))? {
        ForkResult::Child => {
            let fail = |code: i32, msg: &str| -> ! {
                eprintln!("delonix: mount_live: {msg}");
                unsafe { libc::_exit(code) }
            };
            // 1) enter the container's userns (gain CAP_SYS_ADMIN there).
            if let Some(u) = user_fd {
                if setns(u, CloneFlags::empty()).is_err() {
                    fail(125, "setns(user)");
                }
                // SAFETY: we have CAP_SETUID in the container's userns.
                unsafe {
                    libc::setgid(0);
                    libc::setuid(0);
                }
            }
            // 2) new mnt ns (copy of the host's) owned by the container's userns.
            if unshare(CloneFlags::CLONE_NEWNS).is_err() {
                fail(124, "unshare(NEWNS)");
            }
            // 3) clone the source subtree (visible: we still see the host's tree).
            let dfd = match open_tree_clone(&source, true) {
                Ok(f) => f,
                Err(e) => fail(
                    123,
                    &format!("open_tree: {e} (does the kernel support the new mount API?)"),
                ),
            };
            if mount_setattr_fd(dfd.as_raw_fd(), attr).is_err() {
                fail(122, "mount_setattr");
            }
            // 4) enter the container's REAL mnt ns (the root becomes the container's).
            if setns(mnt_fd, CloneFlags::CLONE_NEWNS).is_err() {
                fail(121, "setns(mnt)");
            }
            // 5) create the mount point (resolves against the root = container root).
            if src_is_dir {
                let _ = std::fs::create_dir_all(&target);
            } else {
                if let Some(p) = std::path::Path::new(&target).parent() {
                    let _ = std::fs::create_dir_all(p);
                }
                let _ = std::fs::File::create(&target);
            }
            // 6) attach the detached mount at the target.
            if move_mount_to(dfd.as_raw_fd(), &target).is_err() {
                fail(120, "move_mount");
            }
            unsafe { libc::_exit(0) }
        }
        ForkResult::Parent { child } => {
            let status = waitpid(child, None).map_err(syserr("waitpid"))?;
            match status {
                WaitStatus::Exited(_, 0) => Ok(()),
                WaitStatus::Exited(_, code) => Err(Error::Invalid(format!(
                    "failed to mount {} → {} in the live container (code {code})",
                    m.source, m.target
                ))),
                _ => Err(Error::Invalid("live mount interrupted".into())),
            }
        }
    }
}

/// Unmounts a bind-volume from a RUNNING container (hot-unplug). Enters the container's
/// mnt ns and does `umount2(target, MNT_DETACH)` (lazy: does not fail if busy).
pub fn unmount_live(container: &Container, target: &str) -> Result<()> {
    if !mount_target_safe(target) {
        return Err(Error::Invalid(format!("unsafe unmount target: {target}")));
    }
    let pid = container
        .pid
        .filter(|p| safe_to_signal(*p, container.pid_starttime))
        .ok_or_else(|| Error::NotRunning(container.short_id().to_string()))?;
    // Always the `user` (skip-by-inode in `open_container_ns`) — see the extensive
    // note in `mount_live`: `container.userns` is not the same as "is in a
    // userns different from mine".
    let user_fd = open_container_ns(pid, "user")?;
    let mnt_fd = open_container_ns(pid, "mnt")?
        .ok_or_else(|| Error::Invalid("container shares the host mnt ns".into()))?;
    let target = target.to_string();

    // SAFETY: the child only does simple syscalls and `_exit`.
    match unsafe { fork() }.map_err(syserr("fork"))? {
        ForkResult::Child => {
            if let Some(u) = user_fd {
                if setns(u, CloneFlags::empty()).is_err() {
                    unsafe { libc::_exit(125) };
                }
                unsafe {
                    libc::setgid(0);
                    libc::setuid(0);
                }
            }
            if setns(mnt_fd, CloneFlags::CLONE_NEWNS).is_err() {
                unsafe { libc::_exit(121) };
            }
            match umount2(target.as_str(), MntFlags::MNT_DETACH) {
                Ok(()) => unsafe { libc::_exit(0) },
                Err(_) => unsafe { libc::_exit(119) },
            }
        }
        ForkResult::Parent { child } => {
            let status = waitpid(child, None).map_err(syserr("waitpid"))?;
            match status {
                WaitStatus::Exited(_, 0) => Ok(()),
                _ => Err(Error::Invalid(format!(
                    "failed to unmount {target} in the live container"
                ))),
            }
        }
    }
}

// ----------------------------------------------------------------------------
// CPU priority (nice/renice) — QoS #6
// ----------------------------------------------------------------------------

/// Gathers a container's (host) PIDs: first via `cgroup.procs` (precise);
/// if missing (rootless without cgroup delegation), does a BFS over the process tree
/// from the init's `pid`, reading the `ppid` (field 4 of `/proc/<pid>/stat`).
fn container_pids(container: &Container) -> Vec<i32> {
    if let Ok(procs) = std::fs::read_to_string(format!("{}/cgroup.procs", container.cgroup())) {
        let v: Vec<i32> = procs
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        if !v.is_empty() {
            return v;
        }
    }
    let Some(root) = container.pid else {
        return Vec::new();
    };
    // ppid→[children] map from /proc, then BFS from the init.
    let mut children: std::collections::HashMap<i32, Vec<i32>> = std::collections::HashMap::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                // field 4 (ppid) comes AFTER the `comm` in parentheses — slice after ')'.
                if let Some(rest) = stat.rsplit(')').next() {
                    let f: Vec<&str> = rest.split_whitespace().collect();
                    if let Some(ppid) = f.get(1).and_then(|s| s.parse::<i32>().ok()) {
                        children.entry(ppid).or_default().push(pid);
                    }
                }
            }
        }
    }
    let mut out = vec![root];
    let mut queue = vec![root];
    while let Some(p) = queue.pop() {
        if let Some(kids) = children.get(&p) {
            for &k in kids {
                out.push(k);
                queue.push(k);
            }
        }
    }
    out
}

/// Applies a CPU priority (`nice`) to the WHOLE process tree of a
/// RUNNING container (live renice). Best-effort: lowering priority (positive nice)
/// works without privilege; raising (negative) requires `CAP_SYS_NICE`/root,
/// so individual failures do not abort. Returns `(applied, total)`.
pub fn set_priority(container: &Container, nice: i32) -> Result<(usize, usize)> {
    let nice = nice.clamp(-20, 19);
    let pid = container
        .pid
        .filter(|p| safe_to_signal(*p, container.pid_starttime))
        .ok_or_else(|| Error::NotRunning(container.short_id().to_string()))?;
    let _ = pid;
    let pids = container_pids(container);
    if pids.is_empty() {
        return Err(Error::Invalid("no processes in the container".into()));
    }
    let mut applied = 0usize;
    for p in &pids {
        // SAFETY: setpriority with PRIO_PROCESS and a valid pid; no memory effects.
        let r = unsafe { libc::setpriority(libc::PRIO_PROCESS, *p as libc::id_t, nice) };
        if r == 0 {
            applied += 1;
        }
    }
    Ok((applied, pids.len()))
}

// ----------------------------------------------------------------------------
// Lifecycle: stop / remove
// ----------------------------------------------------------------------------

/// Stops a container: `SIGTERM`, waits up to `timeout_secs`, then `SIGKILL`.
pub fn stop(store: &Store, container: &mut Container, timeout_secs: u64) -> Result<()> {
    let pid = container
        .pid
        .ok_or_else(|| Error::NotRunning(container.short_id().to_string()))?;
    let st = container.pid_starttime;
    // PID reuse protection: if the PID is no longer our process
    // (the kernel recycled it), we do NOT send signals — we only clean up the state.
    if !safe_to_signal(pid, st) {
        container.status = Status::Stopped;
        container.pid = None;
        store.save(container)?;
        remove_container_cgroup(container);
        return Ok(());
    }
    let target = Pid::from_raw(pid);

    let _ = kill(target, Signal::SIGTERM);
    let mut waited = 0u64;
    while safe_to_signal(pid, st) && waited < timeout_secs * 10 {
        std::thread::sleep(Duration::from_millis(100));
        waited += 1;
    }
    if safe_to_signal(pid, st) {
        let _ = kill(target, Signal::SIGKILL);
    }
    // INTENTIONAL stop (by the user) → always Stopped, even if SIGKILL was
    // needed (it is not a crash: it was a requested stop).
    container.status = Status::Stopped;
    container.pid = None;
    store.save(container)?;
    remove_container_cgroup(container);
    Ok(())
}

/// Removes a container's cgroup, waiting for it to empty. After `SIGKILL` the
/// process may take a few ms to be reaped by the init → `rmdir` would give `EBUSY`;
/// so we retry briefly (avoids leaking empty cgroups — robustness).
fn remove_cgroup(cgroup: &str) {
    for _ in 0..100 {
        if !std::path::Path::new(cgroup).exists() || std::fs::remove_dir(cgroup).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Removes the container's cgroup in ALL possible locations: the
/// root-mode path (`Container::cgroup()`) and the rootless delegated leaves — the
/// current base (dedicated scope) and the escape-base `dlx-containers` under the `user@`.
/// Best-effort; only removes empty dirs (the `remove_cgroup` retries).
fn remove_container_cgroup(container: &Container) {
    remove_cgroup(&container.cgroup());
    if let Some(cur) = current_cgroup_v2() {
        remove_cgroup(&format!("{cur}/dlx-{}", container.id));
        if let Some(base) = user_service_base(&cur) {
            remove_cgroup(&format!("{base}/dlx-{}", container.id));
        }
    }
}

/// The container's REAL cgroup v2 (read from the init's `/proc/<pid>/cgroup`) — covers
/// the rootless delegated leaf (`.../dlx-<id>`), where `Container::cgroup()` (the
/// static root-mode path) does not point. Fallback: `cgroup()`.
pub fn live_cgroup(container: &Container) -> String {
    if let Some(pid) = container.pid {
        if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) {
            if let Some(rel) = s.lines().find_map(|l| l.strip_prefix("0::")) {
                return format!("/sys/fs/cgroup{}", rel.trim());
            }
        }
    }
    container.cgroup()
}

/// Suspends (`pause`) or resumes (`unpause`) a container using the cgroup v2
/// *freezer* (`cgroup.freeze`): `1` freezes all processes, `0` resumes. Unlike
/// `SIGSTOP`, it is atomic for the whole tree and invisible to the
/// process (cannot be caught/ignored).
pub fn set_frozen(container: &Container, frozen: bool) -> Result<()> {
    if !container
        .pid
        .map(|p| safe_to_signal(p, container.pid_starttime))
        .unwrap_or(false)
    {
        return Err(Error::NotRunning(container.short_id().to_string()));
    }
    let path = format!("{}/cgroup.freeze", live_cgroup(container));
    std::fs::write(&path, if frozen { "1" } else { "0" }).map_err(|e| Error::Runtime {
        context: "cgroup.freeze",
        message: format!("{path}: {e}"),
    })?;
    Ok(())
}

/// `true` if the container is frozen (`cgroup.freeze` == 1).
pub fn is_frozen(container: &Container) -> bool {
    std::fs::read_to_string(format!("{}/cgroup.freeze", live_cgroup(container)))
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// Reconciles a container's `status` against the kernel reality (without
/// saving it — the caller saves if it returns `true`). Centralizes the 6-state
/// logic in the listings (`ps`, API, Console):
///   * Running + dead pid  → **Crashed** (unexpected death; a clean stop would already have
///     set Stopped). pid = None.
///   * Running + frozen     → **Paused** (cgroup freezer active).
///   * Paused + thawed      → **Running** (resumed externally).
///   * Paused + dead pid    → **Crashed**.
///
/// Terminal states (Stopped/Failed/Crashed) and Created are not touched.
pub fn reconcile_status(c: &mut Container) -> bool {
    // `safe_to_signal` (not raw `is_alive`) to close the PID-reuse window:
    // if the init died and the kernel recycled the PID for a process belonging to the
    // host, `is_alive` would give `true` and the container would be stuck in Running
    // pointing to a PID that is not its own. The recorded `starttime` breaks the tie.
    match c.status {
        Status::Running => match c.pid {
            Some(pid) if !safe_to_signal(pid, c.pid_starttime) => {
                c.status = Status::Crashed;
                c.crash_reason = Some(crash_reason_of(pid, c.pid_starttime).to_string());
                c.crashed_at = Some(now_unix());
                c.pid = None;
                true
            }
            Some(_) if is_frozen(c) => {
                c.status = Status::Paused;
                true
            }
            _ => false,
        },
        Status::Paused => match c.pid {
            Some(pid) if !safe_to_signal(pid, c.pid_starttime) => {
                c.status = Status::Crashed;
                c.crash_reason = Some(crash_reason_of(pid, c.pid_starttime).to_string());
                c.crashed_at = Some(now_unix());
                c.pid = None;
                true
            }
            Some(_) if !is_frozen(c) => {
                c.status = Status::Running;
                true
            }
            _ => false,
        },
        _ => false,
    }
}

/// Rewrites, LIVE, a container's cgroup limits (`docker update`).
/// If the container is stopped, there is no cgroup — only the record changes (in the CLI), and
/// the new limits apply on the next `start`.
pub fn update_limits(
    container: &Container,
    memory: Option<&str>,
    cpus: Option<&str>,
) -> Result<()> {
    let cg = container.cgroup();
    if !std::path::Path::new(&cg).exists() {
        return Ok(());
    }
    if let Some(m) = memory {
        write_limit(&cg, "memory.max", m)?;
    }
    if let Some(c) = cpus {
        write_limit(&cg, "cpu.max", &cpu_max_value(c))?;
    }
    Ok(())
}

/// Removes a container. If it is running, requires `force` (and kills it).
pub fn remove(store: &Store, container: &Container, force: bool) -> Result<()> {
    if let Some(pid) = container.pid {
        if safe_to_signal(pid, container.pid_starttime) {
            if !force {
                return Err(Error::Invalid(format!(
                    "container {} is running (use --force)",
                    container.short_id()
                )));
            }
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
    remove_container_cgroup(container);
    store.remove(&container.id)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `lchown_tree` can NEVER follow symlinks: a symlink inside the tree to
    /// a file OUTSIDE the tree (e.g. malicious OCI image `usr/x -> /etc/shadow`)
    /// cannot make us touch that external file's ownership. It is proven without
    /// root privileges: any real call to chown(2)/lchown(2) updates the
    /// ctime of the targeted inode, even passing the already-current uid/gid — if the ctime of the
    /// symlink target changes, the function followed the link (bug); if only the ctime of the
    /// link ITSELF changes, `lchown` was used correctly.
    #[test]
    fn lchown_tree_nao_segue_symlinks() {
        use std::os::unix::fs::MetadataExt;

        let base = std::env::temp_dir().join(format!(
            "delonix-lchown-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let tree = base.join("tree");
        let outside = base.join("outside");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let victim = outside.join("victim.txt");
        std::fs::write(&victim, b"nao mexer").unwrap();
        let real_file = tree.join("sub").join("file.txt");
        std::fs::write(&real_file, b"parte da arvore").unwrap();
        let link = tree.join("link_to_victim");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        // ctime "before" — sleeps 1s because the ctime resolution on some FS is 1s.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let victim_ctime_before = std::fs::metadata(&victim).unwrap().ctime();
        let link_ctime_before = std::fs::symlink_metadata(&link).unwrap().ctime();
        let file_ctime_before = std::fs::metadata(&real_file).unwrap().ctime();

        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        lchown_tree(&tree, uid, gid);

        let victim_ctime_after = std::fs::metadata(&victim).unwrap().ctime();
        let link_ctime_after = std::fs::symlink_metadata(&link).unwrap().ctime();
        let file_ctime_after = std::fs::metadata(&real_file).unwrap().ctime();

        assert_eq!(
            victim_ctime_before, victim_ctime_after,
            "lchown_tree TOCOU no alvo do symlink fora da árvore — seguiu o link (regressão do bug chown-vs-lchown)"
        );
        assert!(
            link_ctime_after >= link_ctime_before,
            "o próprio link devia ter sido processado (lchown sobre o link em si)"
        );
        assert!(
            file_ctime_after >= file_ctime_before,
            "um ficheiro real dentro da árvore devia continuar a ser chown'd normalmente"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn user_service_base_encontra_a_fronteira_de_delegacao() {
        // normal case: user session → base under the user@<uid>.service.
        assert_eq!(
            user_service_base(
                "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/app-x.scope"
            )
            .as_deref(),
            Some("/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/dlx-containers")
        );
        // the user@ itself (end of the path) also serves as an anchor.
        assert_eq!(
            user_service_base("/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service")
                .as_deref(),
            Some("/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/dlx-containers")
        );
        // outside the session tree (system service) → no escape.
        assert_eq!(
            user_service_base("/sys/fs/cgroup/system.slice/foo.service"),
            None
        );
        // a segment with "user@" without the ".service" suffix does not fool the parser.
        assert_eq!(
            user_service_base("/sys/fs/cgroup/system.slice/user@fake"),
            None
        );
    }

    #[test]
    fn mount_target_rejects_traversal() {
        // safe
        assert!(mount_target_safe("/data"));
        assert!(mount_target_safe("/var/lib/app/config"));
        // escape via `..` (would mount over the host before the pivot_root)
        assert!(!mount_target_safe("/../../etc"));
        assert!(!mount_target_safe("/data/../../etc/shadow"));
        assert!(!mount_target_safe("/a/../b"));
        // relative (resolves from the holder's cwd, not the rootfs)
        assert!(!mount_target_safe("etc/passwd"));
        assert!(!mount_target_safe(""));
    }

    #[test]
    fn confinement_ok_is_fail_closed() {
        let keep = (1u64 << 1) | (1u64 << 3); // only caps 1 and 3 on the allowlist
                                              // good state: no_new_privs, seccomp filter mode, caps ⊆ keep
        assert!(confinement_ok(Some(1), Some(2), Some(keep), Some(keep), true, keep).is_ok());
        // NO_NEW_PRIVS inactive → aborts
        assert!(confinement_ok(Some(0), Some(2), Some(keep), Some(keep), true, keep).is_err());
        // seccomp expected but not in filter mode (failed to apply) → aborts
        assert!(confinement_ok(Some(1), Some(0), Some(keep), Some(keep), true, keep).is_err());
        // cap outside the allowlist persists in the bounding set (capset/capbset failed) → aborts
        let leaked = keep | (1u64 << 21); // + CAP_SYS_ADMIN
        assert!(confinement_ok(Some(1), Some(2), Some(leaked), Some(keep), true, keep).is_err());
        // …or in the effective
        assert!(confinement_ok(Some(1), Some(2), Some(keep), Some(leaked), true, keep).is_err());
        // unconfined (--security-opt seccomp=unconfined): mode 0 is accepted
        assert!(confinement_ok(Some(1), Some(0), Some(keep), Some(keep), false, keep).is_ok());
        // cap fields absent = unverifiable = aborts (fail-closed)
        assert!(confinement_ok(Some(1), Some(2), None, Some(keep), true, keep).is_err());
        assert!(confinement_ok(Some(1), Some(2), Some(keep), None, true, keep).is_err());
        // privileged (keep = all caps): nothing ends up "outside" → ok
        let allcaps = u64::MAX;
        assert!(confinement_ok(
            Some(1),
            Some(2),
            Some(allcaps),
            Some(allcaps),
            true,
            allcaps
        )
        .is_ok());
    }

    #[test]
    fn cpu_max_translates_cores_to_quota() {
        assert_eq!(cpu_max_value("0.5"), "50000 100000");
        assert_eq!(cpu_max_value("1.0"), "100000 100000");
        assert_eq!(cpu_max_value("2"), "200000 100000");
        // absurd values have a floor (0.01 of a core)
        assert_eq!(cpu_max_value("0"), "1000 100000");
    }

    #[test]
    fn dangerous_caps_are_not_kept() {
        // SYS_ADMIN(21), SYS_MODULE(16), SYS_BOOT(22), MKNOD(27), SYS_RAWIO(17),
        // SYS_PTRACE(19), BPF(39) can NOT be on the allowlist.
        for dangerous in [21u8, 16, 22, 27, 17, 19, 39] {
            assert!(
                !KEPT_CAPS.contains(&dangerous),
                "cap {dangerous} não devia ser mantida"
            );
        }
    }

    #[test]
    fn bpf_insn_encoding() {
        // EXIT (code 0x95, without regs/off/imm).
        assert_eq!(bpf_insn(0x95, 0, 0, 0, 0), 0x95);
        // MOV r0 = 1 (imm in the high 32 bits).
        assert_eq!(bpf_insn(0xb7, 0, 0, 0, 1), 0xb7 | (1u64 << 32));
        // LDX r2 = *(u32*)(r1+0): dst=2 (bits 8-11), src=1 (bits 12-15).
        assert_eq!(bpf_insn(0x61, 2, 1, 0, 0), 0x61 | (2 << 8) | (1 << 12));
    }

    #[test]
    fn seccomp_allowlist_excludes_dangerous_and_includes_common() {
        let allowed = allowed_syscalls();
        // dangerous: OUTSIDE the allowlist (= denied by default).
        for nr in [
            libc::SYS_mount,
            libc::SYS_reboot,
            libc::SYS_init_module,
            libc::SYS_bpf,
            libc::SYS_ptrace,
            libc::SYS_kexec_load,
            libc::SYS_setns,
            libc::SYS_unshare,
            libc::SYS_keyctl,
        ] {
            assert!(
                !allowed.contains(&nr),
                "syscall {nr} perigoso NÃO devia estar na allowlist"
            );
        }
        // common/essential: INSIDE the allowlist.
        for nr in [
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_openat,
            libc::SYS_mmap,
            libc::SYS_futex,
            libc::SYS_execve,
            libc::SYS_exit_group,
        ] {
            assert!(
                allowed.contains(&nr),
                "syscall {nr} essencial DEVIA estar na allowlist"
            );
        }
    }

    #[test]
    fn parse_mem_satura_e_nao_zera_em_lixo() {
        assert_eq!(parse_mem_bytes("64M"), 64 * 1024 * 1024);
        assert_eq!(parse_mem_bytes("1G"), 1024 * 1024 * 1024);
        assert_eq!(parse_mem_bytes("512"), 512);
        // overflow → saturates (does not panic/wrap).
        assert_eq!(parse_mem_bytes("99999999999G"), u64::MAX);
        // garbage → u64::MAX (refused at admission), NEVER 0 (which would let everything through).
        assert_eq!(parse_mem_bytes("64MB"), u64::MAX);
        assert_eq!(parse_mem_bytes("abc"), u64::MAX);
    }

    #[test]
    fn sysctl_allowlist_so_aceita_namespaced() {
        // namespaced (safe) → permitted.
        for k in [
            "net.ipv4.ip_forward",
            "kernel.shmmax",
            "kernel.sem",
            "fs.mqueue.msg_max",
        ] {
            assert!(sysctl_namespaced(k), "{k} devia ser permitido");
        }
        // global to the host / traversal → refused.
        for k in [
            "kernel.hostname",
            "vm.swappiness",
            "kernel.core_pattern",
            "net/../kernel.x",
        ] {
            assert!(!sysctl_namespaced(k), "{k} NÃO devia ser permitido");
        }
    }
}
