//! OCI seccomp profiles — `--security-opt seccomp=<file.json>` and the CRI's
//! `SecurityProfile::Localhost`.
//!
//! The engine used to REFUSE a custom profile with a clear error, deliberately:
//! accepting one and silently running the built-in allowlist instead is the
//! failure mode this repo names as its worst. Refusing was the honest
//! placeholder; this module is the feature.
//!
//! Format: the runc/Docker/containerd JSON — `defaultAction` plus a list of
//! `{names, action}` rules. It is what a Kubernetes `localhostProfile` points
//! at, what every published hardening profile is written in, and what the CRI
//! conformance suite hands over.

use seccompiler::{SeccompAction, SeccompFilter, SeccompRule};
use std::collections::BTreeMap;

/// Syscall NAME to number, for THIS architecture.
///
/// Built from `libc::SYS_*` and not from a table of literals: the numbers
/// differ between x86_64 and aarch64, and a hardcoded table would be silently
/// wrong on one of them. A seccomp filter that denies the wrong syscall is far
/// worse than one that refuses to build.
///
/// Generated from the kernel's own `unistd` header, both halves of each pair
/// from the same name, so they cannot disagree.
// These names are DELIBERATELY kept even though libc marks a handful of them
// deprecated (`create_module`, `get_kernel_syms`, `query_module` — dead since
// 2.6). A seccomp profile exists to DENY syscalls, and the ones nobody uses any
// more are exactly the ones a hardening profile names. Dropping them would make
// such a profile fail to load with "unknown syscall".
#[allow(deprecated)]
const SYSCALLS: &[(&str, i64)] = &[
    ("accept", libc::SYS_accept),
    ("accept4", libc::SYS_accept4),
    ("access", libc::SYS_access),
    ("acct", libc::SYS_acct),
    ("add_key", libc::SYS_add_key),
    ("adjtimex", libc::SYS_adjtimex),
    ("afs_syscall", libc::SYS_afs_syscall),
    ("alarm", libc::SYS_alarm),
    ("arch_prctl", libc::SYS_arch_prctl),
    ("bind", libc::SYS_bind),
    ("bpf", libc::SYS_bpf),
    ("brk", libc::SYS_brk),
    ("capget", libc::SYS_capget),
    ("capset", libc::SYS_capset),
    ("chdir", libc::SYS_chdir),
    ("chmod", libc::SYS_chmod),
    ("chown", libc::SYS_chown),
    ("chroot", libc::SYS_chroot),
    ("clock_adjtime", libc::SYS_clock_adjtime),
    ("clock_getres", libc::SYS_clock_getres),
    ("clock_gettime", libc::SYS_clock_gettime),
    ("clock_nanosleep", libc::SYS_clock_nanosleep),
    ("clock_settime", libc::SYS_clock_settime),
    ("clone", libc::SYS_clone),
    ("clone3", libc::SYS_clone3),
    ("close", libc::SYS_close),
    ("close_range", libc::SYS_close_range),
    ("connect", libc::SYS_connect),
    ("copy_file_range", libc::SYS_copy_file_range),
    ("creat", libc::SYS_creat),
    ("create_module", libc::SYS_create_module),
    ("delete_module", libc::SYS_delete_module),
    ("dup", libc::SYS_dup),
    ("dup2", libc::SYS_dup2),
    ("dup3", libc::SYS_dup3),
    ("epoll_create", libc::SYS_epoll_create),
    ("epoll_create1", libc::SYS_epoll_create1),
    ("epoll_ctl", libc::SYS_epoll_ctl),
    ("epoll_ctl_old", libc::SYS_epoll_ctl_old),
    ("epoll_pwait", libc::SYS_epoll_pwait),
    ("epoll_pwait2", libc::SYS_epoll_pwait2),
    ("epoll_wait", libc::SYS_epoll_wait),
    ("epoll_wait_old", libc::SYS_epoll_wait_old),
    ("eventfd", libc::SYS_eventfd),
    ("eventfd2", libc::SYS_eventfd2),
    ("execve", libc::SYS_execve),
    ("execveat", libc::SYS_execveat),
    ("exit", libc::SYS_exit),
    ("exit_group", libc::SYS_exit_group),
    ("faccessat", libc::SYS_faccessat),
    ("faccessat2", libc::SYS_faccessat2),
    ("fadvise64", libc::SYS_fadvise64),
    ("fallocate", libc::SYS_fallocate),
    ("fanotify_init", libc::SYS_fanotify_init),
    ("fanotify_mark", libc::SYS_fanotify_mark),
    ("fchdir", libc::SYS_fchdir),
    ("fchmod", libc::SYS_fchmod),
    ("fchmodat", libc::SYS_fchmodat),
    ("fchmodat2", libc::SYS_fchmodat2),
    ("fchown", libc::SYS_fchown),
    ("fchownat", libc::SYS_fchownat),
    ("fcntl", libc::SYS_fcntl),
    ("fdatasync", libc::SYS_fdatasync),
    ("fgetxattr", libc::SYS_fgetxattr),
    ("finit_module", libc::SYS_finit_module),
    ("flistxattr", libc::SYS_flistxattr),
    ("flock", libc::SYS_flock),
    ("fork", libc::SYS_fork),
    ("fremovexattr", libc::SYS_fremovexattr),
    ("fsconfig", libc::SYS_fsconfig),
    ("fsetxattr", libc::SYS_fsetxattr),
    ("fsmount", libc::SYS_fsmount),
    ("fsopen", libc::SYS_fsopen),
    ("fspick", libc::SYS_fspick),
    ("fstat", libc::SYS_fstat),
    ("fstatfs", libc::SYS_fstatfs),
    ("fsync", libc::SYS_fsync),
    ("ftruncate", libc::SYS_ftruncate),
    ("futex", libc::SYS_futex),
    ("futex_waitv", libc::SYS_futex_waitv),
    ("futimesat", libc::SYS_futimesat),
    ("getcpu", libc::SYS_getcpu),
    ("getcwd", libc::SYS_getcwd),
    ("getdents", libc::SYS_getdents),
    ("getdents64", libc::SYS_getdents64),
    ("getegid", libc::SYS_getegid),
    ("geteuid", libc::SYS_geteuid),
    ("getgid", libc::SYS_getgid),
    ("getgroups", libc::SYS_getgroups),
    ("getitimer", libc::SYS_getitimer),
    ("get_kernel_syms", libc::SYS_get_kernel_syms),
    ("get_mempolicy", libc::SYS_get_mempolicy),
    ("getpeername", libc::SYS_getpeername),
    ("getpgid", libc::SYS_getpgid),
    ("getpgrp", libc::SYS_getpgrp),
    ("getpid", libc::SYS_getpid),
    ("getpmsg", libc::SYS_getpmsg),
    ("getppid", libc::SYS_getppid),
    ("getpriority", libc::SYS_getpriority),
    ("getrandom", libc::SYS_getrandom),
    ("getresgid", libc::SYS_getresgid),
    ("getresuid", libc::SYS_getresuid),
    ("getrlimit", libc::SYS_getrlimit),
    ("get_robust_list", libc::SYS_get_robust_list),
    ("getrusage", libc::SYS_getrusage),
    ("getsid", libc::SYS_getsid),
    ("getsockname", libc::SYS_getsockname),
    ("getsockopt", libc::SYS_getsockopt),
    ("get_thread_area", libc::SYS_get_thread_area),
    ("gettid", libc::SYS_gettid),
    ("gettimeofday", libc::SYS_gettimeofday),
    ("getuid", libc::SYS_getuid),
    ("getxattr", libc::SYS_getxattr),
    ("init_module", libc::SYS_init_module),
    ("inotify_add_watch", libc::SYS_inotify_add_watch),
    ("inotify_init", libc::SYS_inotify_init),
    ("inotify_init1", libc::SYS_inotify_init1),
    ("inotify_rm_watch", libc::SYS_inotify_rm_watch),
    ("io_cancel", libc::SYS_io_cancel),
    ("ioctl", libc::SYS_ioctl),
    ("io_destroy", libc::SYS_io_destroy),
    ("io_getevents", libc::SYS_io_getevents),
    ("ioperm", libc::SYS_ioperm),
    ("iopl", libc::SYS_iopl),
    ("ioprio_get", libc::SYS_ioprio_get),
    ("ioprio_set", libc::SYS_ioprio_set),
    ("io_setup", libc::SYS_io_setup),
    ("io_submit", libc::SYS_io_submit),
    ("io_uring_enter", libc::SYS_io_uring_enter),
    ("io_uring_register", libc::SYS_io_uring_register),
    ("io_uring_setup", libc::SYS_io_uring_setup),
    ("kcmp", libc::SYS_kcmp),
    ("kexec_file_load", libc::SYS_kexec_file_load),
    ("kexec_load", libc::SYS_kexec_load),
    ("keyctl", libc::SYS_keyctl),
    ("kill", libc::SYS_kill),
    ("landlock_add_rule", libc::SYS_landlock_add_rule),
    ("landlock_create_ruleset", libc::SYS_landlock_create_ruleset),
    ("landlock_restrict_self", libc::SYS_landlock_restrict_self),
    ("lchown", libc::SYS_lchown),
    ("lgetxattr", libc::SYS_lgetxattr),
    ("link", libc::SYS_link),
    ("linkat", libc::SYS_linkat),
    ("listen", libc::SYS_listen),
    ("listxattr", libc::SYS_listxattr),
    ("llistxattr", libc::SYS_llistxattr),
    ("lookup_dcookie", libc::SYS_lookup_dcookie),
    ("lremovexattr", libc::SYS_lremovexattr),
    ("lseek", libc::SYS_lseek),
    ("lsetxattr", libc::SYS_lsetxattr),
    ("lstat", libc::SYS_lstat),
    ("madvise", libc::SYS_madvise),
    ("mbind", libc::SYS_mbind),
    ("membarrier", libc::SYS_membarrier),
    ("memfd_create", libc::SYS_memfd_create),
    ("memfd_secret", libc::SYS_memfd_secret),
    ("migrate_pages", libc::SYS_migrate_pages),
    ("mincore", libc::SYS_mincore),
    ("mkdir", libc::SYS_mkdir),
    ("mkdirat", libc::SYS_mkdirat),
    ("mknod", libc::SYS_mknod),
    ("mknodat", libc::SYS_mknodat),
    ("mlock", libc::SYS_mlock),
    ("mlock2", libc::SYS_mlock2),
    ("mlockall", libc::SYS_mlockall),
    ("mmap", libc::SYS_mmap),
    ("modify_ldt", libc::SYS_modify_ldt),
    ("mount", libc::SYS_mount),
    ("mount_setattr", libc::SYS_mount_setattr),
    ("move_mount", libc::SYS_move_mount),
    ("move_pages", libc::SYS_move_pages),
    ("mprotect", libc::SYS_mprotect),
    ("mq_getsetattr", libc::SYS_mq_getsetattr),
    ("mq_notify", libc::SYS_mq_notify),
    ("mq_open", libc::SYS_mq_open),
    ("mq_timedreceive", libc::SYS_mq_timedreceive),
    ("mq_timedsend", libc::SYS_mq_timedsend),
    ("mq_unlink", libc::SYS_mq_unlink),
    ("mremap", libc::SYS_mremap),
    ("msgctl", libc::SYS_msgctl),
    ("msgget", libc::SYS_msgget),
    ("msgrcv", libc::SYS_msgrcv),
    ("msgsnd", libc::SYS_msgsnd),
    ("msync", libc::SYS_msync),
    ("munlock", libc::SYS_munlock),
    ("munlockall", libc::SYS_munlockall),
    ("munmap", libc::SYS_munmap),
    ("name_to_handle_at", libc::SYS_name_to_handle_at),
    ("nanosleep", libc::SYS_nanosleep),
    ("newfstatat", libc::SYS_newfstatat),
    ("nfsservctl", libc::SYS_nfsservctl),
    ("open", libc::SYS_open),
    ("openat", libc::SYS_openat),
    ("openat2", libc::SYS_openat2),
    ("open_by_handle_at", libc::SYS_open_by_handle_at),
    ("open_tree", libc::SYS_open_tree),
    ("pause", libc::SYS_pause),
    ("perf_event_open", libc::SYS_perf_event_open),
    ("personality", libc::SYS_personality),
    ("pidfd_getfd", libc::SYS_pidfd_getfd),
    ("pidfd_open", libc::SYS_pidfd_open),
    ("pidfd_send_signal", libc::SYS_pidfd_send_signal),
    ("pipe", libc::SYS_pipe),
    ("pipe2", libc::SYS_pipe2),
    ("pivot_root", libc::SYS_pivot_root),
    ("pkey_alloc", libc::SYS_pkey_alloc),
    ("pkey_free", libc::SYS_pkey_free),
    ("pkey_mprotect", libc::SYS_pkey_mprotect),
    ("poll", libc::SYS_poll),
    ("ppoll", libc::SYS_ppoll),
    ("prctl", libc::SYS_prctl),
    ("pread64", libc::SYS_pread64),
    ("preadv", libc::SYS_preadv),
    ("preadv2", libc::SYS_preadv2),
    ("prlimit64", libc::SYS_prlimit64),
    ("process_madvise", libc::SYS_process_madvise),
    ("process_mrelease", libc::SYS_process_mrelease),
    ("process_vm_readv", libc::SYS_process_vm_readv),
    ("process_vm_writev", libc::SYS_process_vm_writev),
    ("pselect6", libc::SYS_pselect6),
    ("ptrace", libc::SYS_ptrace),
    ("putpmsg", libc::SYS_putpmsg),
    ("pwrite64", libc::SYS_pwrite64),
    ("pwritev", libc::SYS_pwritev),
    ("pwritev2", libc::SYS_pwritev2),
    ("query_module", libc::SYS_query_module),
    ("quotactl", libc::SYS_quotactl),
    ("quotactl_fd", libc::SYS_quotactl_fd),
    ("read", libc::SYS_read),
    ("readahead", libc::SYS_readahead),
    ("readlink", libc::SYS_readlink),
    ("readlinkat", libc::SYS_readlinkat),
    ("readv", libc::SYS_readv),
    ("reboot", libc::SYS_reboot),
    ("recvfrom", libc::SYS_recvfrom),
    ("recvmmsg", libc::SYS_recvmmsg),
    ("recvmsg", libc::SYS_recvmsg),
    ("remap_file_pages", libc::SYS_remap_file_pages),
    ("removexattr", libc::SYS_removexattr),
    ("rename", libc::SYS_rename),
    ("renameat", libc::SYS_renameat),
    ("renameat2", libc::SYS_renameat2),
    ("request_key", libc::SYS_request_key),
    ("restart_syscall", libc::SYS_restart_syscall),
    ("rmdir", libc::SYS_rmdir),
    ("rseq", libc::SYS_rseq),
    ("rt_sigaction", libc::SYS_rt_sigaction),
    ("rt_sigpending", libc::SYS_rt_sigpending),
    ("rt_sigprocmask", libc::SYS_rt_sigprocmask),
    ("rt_sigqueueinfo", libc::SYS_rt_sigqueueinfo),
    ("rt_sigreturn", libc::SYS_rt_sigreturn),
    ("rt_sigsuspend", libc::SYS_rt_sigsuspend),
    ("rt_sigtimedwait", libc::SYS_rt_sigtimedwait),
    ("rt_tgsigqueueinfo", libc::SYS_rt_tgsigqueueinfo),
    ("sched_getaffinity", libc::SYS_sched_getaffinity),
    ("sched_getattr", libc::SYS_sched_getattr),
    ("sched_getparam", libc::SYS_sched_getparam),
    ("sched_get_priority_max", libc::SYS_sched_get_priority_max),
    ("sched_get_priority_min", libc::SYS_sched_get_priority_min),
    ("sched_getscheduler", libc::SYS_sched_getscheduler),
    ("sched_rr_get_interval", libc::SYS_sched_rr_get_interval),
    ("sched_setaffinity", libc::SYS_sched_setaffinity),
    ("sched_setattr", libc::SYS_sched_setattr),
    ("sched_setparam", libc::SYS_sched_setparam),
    ("sched_setscheduler", libc::SYS_sched_setscheduler),
    ("sched_yield", libc::SYS_sched_yield),
    ("seccomp", libc::SYS_seccomp),
    ("security", libc::SYS_security),
    ("select", libc::SYS_select),
    ("semctl", libc::SYS_semctl),
    ("semget", libc::SYS_semget),
    ("semop", libc::SYS_semop),
    ("semtimedop", libc::SYS_semtimedop),
    ("sendfile", libc::SYS_sendfile),
    ("sendmmsg", libc::SYS_sendmmsg),
    ("sendmsg", libc::SYS_sendmsg),
    ("sendto", libc::SYS_sendto),
    ("setdomainname", libc::SYS_setdomainname),
    ("setfsgid", libc::SYS_setfsgid),
    ("setfsuid", libc::SYS_setfsuid),
    ("setgid", libc::SYS_setgid),
    ("setgroups", libc::SYS_setgroups),
    ("sethostname", libc::SYS_sethostname),
    ("setitimer", libc::SYS_setitimer),
    ("set_mempolicy", libc::SYS_set_mempolicy),
    ("set_mempolicy_home_node", libc::SYS_set_mempolicy_home_node),
    ("setns", libc::SYS_setns),
    ("setpgid", libc::SYS_setpgid),
    ("setpriority", libc::SYS_setpriority),
    ("setregid", libc::SYS_setregid),
    ("setresgid", libc::SYS_setresgid),
    ("setresuid", libc::SYS_setresuid),
    ("setreuid", libc::SYS_setreuid),
    ("setrlimit", libc::SYS_setrlimit),
    ("set_robust_list", libc::SYS_set_robust_list),
    ("setsid", libc::SYS_setsid),
    ("setsockopt", libc::SYS_setsockopt),
    ("set_thread_area", libc::SYS_set_thread_area),
    ("set_tid_address", libc::SYS_set_tid_address),
    ("settimeofday", libc::SYS_settimeofday),
    ("setuid", libc::SYS_setuid),
    ("setxattr", libc::SYS_setxattr),
    ("shmat", libc::SYS_shmat),
    ("shmctl", libc::SYS_shmctl),
    ("shmdt", libc::SYS_shmdt),
    ("shmget", libc::SYS_shmget),
    ("shutdown", libc::SYS_shutdown),
    ("sigaltstack", libc::SYS_sigaltstack),
    ("signalfd", libc::SYS_signalfd),
    ("signalfd4", libc::SYS_signalfd4),
    ("socket", libc::SYS_socket),
    ("socketpair", libc::SYS_socketpair),
    ("splice", libc::SYS_splice),
    ("stat", libc::SYS_stat),
    ("statfs", libc::SYS_statfs),
    ("statx", libc::SYS_statx),
    ("swapoff", libc::SYS_swapoff),
    ("swapon", libc::SYS_swapon),
    ("symlink", libc::SYS_symlink),
    ("symlinkat", libc::SYS_symlinkat),
    ("sync", libc::SYS_sync),
    ("sync_file_range", libc::SYS_sync_file_range),
    ("syncfs", libc::SYS_syncfs),
    ("_sysctl", libc::SYS__sysctl),
    ("sysfs", libc::SYS_sysfs),
    ("sysinfo", libc::SYS_sysinfo),
    ("syslog", libc::SYS_syslog),
    ("tee", libc::SYS_tee),
    ("tgkill", libc::SYS_tgkill),
    ("time", libc::SYS_time),
    ("timer_create", libc::SYS_timer_create),
    ("timer_delete", libc::SYS_timer_delete),
    ("timerfd_create", libc::SYS_timerfd_create),
    ("timerfd_gettime", libc::SYS_timerfd_gettime),
    ("timerfd_settime", libc::SYS_timerfd_settime),
    ("timer_getoverrun", libc::SYS_timer_getoverrun),
    ("timer_gettime", libc::SYS_timer_gettime),
    ("timer_settime", libc::SYS_timer_settime),
    ("times", libc::SYS_times),
    ("tkill", libc::SYS_tkill),
    ("truncate", libc::SYS_truncate),
    ("tuxcall", libc::SYS_tuxcall),
    ("umask", libc::SYS_umask),
    ("umount2", libc::SYS_umount2),
    ("uname", libc::SYS_uname),
    ("unlink", libc::SYS_unlink),
    ("unlinkat", libc::SYS_unlinkat),
    ("unshare", libc::SYS_unshare),
    ("uselib", libc::SYS_uselib),
    ("userfaultfd", libc::SYS_userfaultfd),
    ("ustat", libc::SYS_ustat),
    ("utime", libc::SYS_utime),
    ("utimensat", libc::SYS_utimensat),
    ("utimes", libc::SYS_utimes),
    ("vfork", libc::SYS_vfork),
    ("vhangup", libc::SYS_vhangup),
    ("vmsplice", libc::SYS_vmsplice),
    ("vserver", libc::SYS_vserver),
    ("wait4", libc::SYS_wait4),
    ("waitid", libc::SYS_waitid),
    ("write", libc::SYS_write),
    ("writev", libc::SYS_writev),
];

/// Resolves a syscall name. `None` = this kernel/arch has no such call.
///
/// Linear scan over ~370 entries, once per profile rule at container start.
/// A map would be faster and is not worth the code: this runs once, and the
/// numbers are already in the binary.
pub fn syscall_number(name: &str) -> Option<i64> {
    SYSCALLS.iter().find(|(n, _)| *n == name).map(|(_, nr)| *nr)
}

/// A parsed OCI seccomp profile, reduced to what this engine can enforce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub default_action: Act,
    /// `(syscall number, action)` — already resolved, so nothing downstream has
    /// to know about names.
    pub rules: Vec<(i64, Act)>,
}

/// The subset of `SCMP_ACT_*` this engine implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Allow,
    Errno(u32),
    KillThread,
    Log,
    Trap,
}

impl Act {
    fn to_seccompiler(self) -> SeccompAction {
        match self {
            Act::Allow => SeccompAction::Allow,
            Act::Errno(e) => SeccompAction::Errno(e),
            Act::KillThread => SeccompAction::KillThread,
            Act::Log => SeccompAction::Log,
            Act::Trap => SeccompAction::Trap,
        }
    }
}

/// Parses one `SCMP_ACT_*` string.
///
/// `SCMP_ACT_KILL` is mapped to KILL_THREAD, which is what libseccomp has
/// always meant by the bare name and what runc does. `SCMP_ACT_TRACE` and
/// `SCMP_ACT_NOTIFY` are REJECTED rather than downgraded: both hand the
/// decision to a supervising process this engine does not run, and silently
/// turning "ask the supervisor" into "allow" would be a hole in a policy the
/// caller believes is enforced.
fn parse_action(s: &str, errno_ret: Option<u32>) -> Result<Act, String> {
    Ok(match s {
        "SCMP_ACT_ALLOW" => Act::Allow,
        "SCMP_ACT_ERRNO" => Act::Errno(errno_ret.unwrap_or(libc::EPERM as u32)),
        "SCMP_ACT_KILL" | "SCMP_ACT_KILL_THREAD" => Act::KillThread,
        "SCMP_ACT_LOG" => Act::Log,
        "SCMP_ACT_TRAP" => Act::Trap,
        "SCMP_ACT_KILL_PROCESS" => {
            // seccompiler has no KILL_PROCESS. KILL_THREAD on a container's pid
            // 1 ends the container anyway, and on any other thread it is
            // strictly more permissive — say so instead of pretending.
            return Err(
                "SCMP_ACT_KILL_PROCESS is not supported (use SCMP_ACT_KILL for kill-thread)".into(),
            );
        }
        other => return Err(format!("unsupported seccomp action '{other}'")),
    })
}

/// Parses an OCI seccomp profile from JSON.
///
/// **Fail-closed at every step.** An unknown action, an unknown syscall name, a
/// malformed document — all are errors that stop the container. The alternative
/// (skip what we do not understand) produces a filter that is a SUBSET of the
/// policy the operator wrote, which is exactly the silent weakening this whole
/// module exists to avoid.
///
/// The one deliberate exception is a name this ARCHITECTURE does not have: a
/// profile written for x86_64 naming `arch_prctl` is not wrong on aarch64, it
/// is inapplicable, and refusing it would make portable profiles impossible.
/// Those are counted and reported by the caller, never silently dropped.
pub fn parse(json: &str) -> Result<(Profile, Vec<String>), String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid seccomp profile JSON: {e}"))?;
    let default_action = v
        .get("defaultAction")
        .and_then(|d| d.as_str())
        .ok_or_else(|| "seccomp profile has no 'defaultAction'".to_string())?;
    let default_action = parse_action(default_action, None)?;

    let mut rules = Vec::new();
    let mut unknown = Vec::new();
    if let Some(arr) = v.get("syscalls").and_then(|s| s.as_array()) {
        for entry in arr {
            let action = entry
                .get("action")
                .and_then(|a| a.as_str())
                .ok_or_else(|| "syscall entry has no 'action'".to_string())?;
            let errno_ret = entry
                .get("errnoRet")
                .and_then(|e| e.as_u64())
                .map(|e| e as u32);
            let act = parse_action(action, errno_ret)?;
            // `args` narrows a rule to particular argument values. Not
            // implemented, and NOT ignored: a rule that only meant to block
            // `clone(CLONE_NEWUSER)` would otherwise become "block clone",
            // breaking every threaded program in the container.
            if entry
                .get("args")
                .is_some_and(|a| !a.as_array().is_none_or(|v| v.is_empty()))
            {
                return Err(
                    "seccomp rule with 'args' is not supported (argument-filtered rules)".into(),
                );
            }
            let names = entry
                .get("names")
                .and_then(|n| n.as_array())
                .ok_or_else(|| "syscall entry has no 'names'".to_string())?;
            for n in names {
                let n = n
                    .as_str()
                    .ok_or_else(|| "syscall name is not a string".to_string())?;
                match syscall_number(n) {
                    Some(nr) => rules.push((nr, act)),
                    None => unknown.push(n.to_string()),
                }
            }
        }
    }
    Ok((
        Profile {
            default_action,
            rules,
        },
        unknown,
    ))
}

/// Compiles a parsed profile into a BPF program.
///
/// seccompiler models a filter as "match → one action, no match → another", so
/// a profile with more than TWO distinct actions cannot be expressed. That is
/// refused rather than approximated: the profiles that matter in practice
/// (`ALLOW` default with `ERRNO` exceptions, or the reverse) use exactly two.
pub fn compile(p: &Profile) -> Result<seccompiler::BpfProgram, String> {
    let arch = std::env::consts::ARCH
        .try_into()
        .map_err(|_| "architecture without seccomp support".to_string())?;
    let rule_actions: std::collections::BTreeSet<Act> = p
        .rules
        .iter()
        .map(|(_, a)| *a)
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    if rule_actions.len() > 1 {
        return Err(format!(
            "profile mixes {} different rule actions; only one non-default action is supported",
            rule_actions.len()
        ));
    }
    let match_action = rule_actions
        .into_iter()
        .next()
        .unwrap_or(Act::Allow)
        .to_seccompiler();
    let mut map: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for (nr, _) in &p.rules {
        map.insert(*nr, vec![]);
    }
    SeccompFilter::new(map, p.default_action.to_seccompiler(), match_action, arch)
        .and_then(|f| f.try_into())
        .map_err(|e| format!("failed to build the seccomp filter: {e}"))
}

impl PartialOrd for Act {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Act {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        format!("{self:?}").cmp(&format!("{other:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The profile shape the CRI conformance suite hands over.
    const BLOCK_HOSTNAME: &str = r#"{
        "defaultAction": "SCMP_ACT_ALLOW",
        "syscalls": [{"names": ["sethostname"], "action": "SCMP_ACT_ERRNO"}]
    }"#;

    #[test]
    fn parseia_o_perfil_do_conformance() {
        let (p, unknown) = parse(BLOCK_HOSTNAME).unwrap();
        assert_eq!(p.default_action, Act::Allow);
        assert_eq!(p.rules.len(), 1);
        assert_eq!(p.rules[0].0, libc::SYS_sethostname);
        assert_eq!(p.rules[0].1, Act::Errno(libc::EPERM as u32));
        assert!(unknown.is_empty());
        assert!(compile(&p).is_ok());
    }

    #[test]
    fn nomes_resolvem_para_o_numero_desta_arquitectura() {
        // Não são literais: em aarch64 os números são outros, e uma tabela
        // fixa estaria silenciosamente errada numa das duas.
        assert_eq!(syscall_number("read"), Some(libc::SYS_read));
        assert_eq!(syscall_number("mount"), Some(libc::SYS_mount));
        assert_eq!(syscall_number("nao_existe_isto"), None);
    }

    #[test]
    fn falha_fechado_no_que_nao_percebe() {
        // Acção desconhecida: erro, não «ignora a regra».
        assert!(parse(r#"{"defaultAction":"SCMP_ACT_BANANA"}"#).is_err());
        // Sem defaultAction não há política nenhuma para aplicar.
        assert!(parse(r#"{"syscalls":[]}"#).is_err());
        // JSON partido.
        assert!(parse("{").is_err());
        // TRACE/NOTIFY entregam a decisão a um supervisor que não existe aqui:
        // transformá-los em «allow» seria um buraco numa política que o
        // operador julga aplicada.
        assert!(parse(
            r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["read"],"action":"SCMP_ACT_NOTIFY"}]}"#
        )
        .is_err());
    }

    #[test]
    fn regras_com_args_sao_recusadas_e_nao_alargadas() {
        // Uma regra que só queria bloquear `clone(CLONE_NEWUSER)` viraria
        // «bloquear clone» — partindo todo o programa com threads lá dentro.
        let j = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
            {"names":["clone"],"action":"SCMP_ACT_ERRNO",
             "args":[{"index":0,"value":268435456,"op":"SCMP_CMP_MASKED_EQ"}]}]}"#;
        assert!(parse(j).is_err());
    }

    #[test]
    fn nome_inaplicavel_nesta_arquitectura_e_reportado_nao_descartado() {
        // Um perfil portátil nomeia syscalls que só existem num dos arcos. Isso
        // é inaplicável, não errado — mas quem o aplica tem de saber.
        let j = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
            {"names":["read","uma_syscall_de_outro_arco"],"action":"SCMP_ACT_ERRNO"}]}"#;
        let (p, unknown) = parse(j).unwrap();
        assert_eq!(p.rules.len(), 1);
        assert_eq!(unknown, vec!["uma_syscall_de_outro_arco"]);
    }

    #[test]
    fn perfil_com_duas_accoes_distintas_e_recusado() {
        // O modelo do seccompiler é «casou → uma acção, não casou → outra».
        // Aproximar seria aplicar uma política diferente da escrita.
        let j = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
            {"names":["read"],"action":"SCMP_ACT_ERRNO"},
            {"names":["write"],"action":"SCMP_ACT_KILL"}]}"#;
        let (p, _) = parse(j).unwrap();
        assert!(compile(&p).is_err());
    }
}
