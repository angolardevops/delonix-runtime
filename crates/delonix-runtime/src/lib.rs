//! `delonix-runtime` — o runtime OCI de baixo nível do Delonix Engine.
//!
//! É o `mini-runc` do Mês 5, promovido a biblioteca: cria containers com
//! `clone` (namespaces) + `pivot_root` (rootfs) + cgroup (memória) + seccomp
//! (confinamento), corre comandos dentro de um container existente com `setns`
//! (`exec`), e gere o seu ciclo de vida (`stop`/`remove`).
//!
//! Toda a fronteira de `syscalls` vive aqui; o resto do Delonix nunca lhe toca.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

use delonix_core::{Container, Error, Mount, Result, Status, Store};

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

/// Um *hook* invocado com o PID do init, logo após o container arrancar e antes
/// de qualquer `waitpid`. A Fase 3 usa-o para configurar a rede (estilo CNI).
pub type StartedHook<'a> = dyn Fn(i32) -> Result<()> + 'a;

fn syserr(context: &'static str) -> impl Fn(nix::Error) -> Error {
    move |e| Error::Runtime {
        context,
        message: e.to_string(),
    }
}

/// `true` se o processo `pid` ainda existe (sinal 0 = só testa vida).
pub fn is_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// `starttime` do processo (campo 22 de `/proc/<pid>/stat`, jiffies desde o
/// boot). Único e estável durante a vida do processo — usamo-lo para detectar
/// reutilização de PID.
pub fn proc_starttime(pid: i32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // O comm (campo 2) pode conter espaços/parênteses — corta até ao último ')'.
    let rest = &s[s.rfind(')')? + 1..];
    rest.split_whitespace().nth(19).and_then(|f| f.parse().ok()) // campo 22 = 20.º após o comm
}

/// `true` se é seguro enviar um sinal a `pid` em nome deste container: o PID
/// está vivo E (se conhecemos o `starttime` registado) ainda é o MESMO processo.
/// Protege contra reutilização de PID — nunca matamos um processo alheio do host.
pub fn safe_to_signal(pid: i32, starttime: Option<u64>) -> bool {
    if !is_alive(pid) {
        return false;
    }
    match starttime {
        Some(want) => proc_starttime(pid) == Some(want),
        None => true, // registo antigo sem starttime: comportamento legado
    }
}

fn wait_to_code(status: WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, sig, _) => 128 + sig as i32,
        _ => -1,
    }
}

/// Converte o `WaitStatus` no [`Status`] terminal correto: saída 0 → Stopped,
/// saída ≠ 0 → Failed, morto por sinal → Crashed.
fn wait_to_status(status: WaitStatus) -> Status {
    match status {
        WaitStatus::Exited(_, 0) => Status::Stopped,
        WaitStatus::Exited(_, code) => Status::Failed(code),
        WaitStatus::Signaled(..) => Status::Crashed,
        _ => Status::Crashed,
    }
}

// ----------------------------------------------------------------------------
// Criar e correr um container
// ----------------------------------------------------------------------------

/// Uma regra seccomp por-argumento: o syscall casa (e é bloqueado) quando
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

/// Instala um filtro seccomp: bloqueia (com `EPERM`) a lista negra incondicional
/// MAIS regras por-argumento que afinam casos legítimos. Activa `no_new_privs`.
/// Carrega o filtro seccomp com `SECCOMP_FILTER_FLAG_LOG`: cada syscall NEGADO
/// (fora da allowlist) é **registado** no audit/dmesg do kernel — detecção em
/// runtime tipo Falco (B12) — sem deixar de o bloquear (continua `EPERM`).
fn apply_filter_logged(prog: &seccompiler::BpfProgram) {
    const SET_MODE_FILTER: libc::c_ulong = 1;
    const FLAG_LOG: libc::c_ulong = 2;
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_ptr() as *mut libc::sock_filter,
    };
    // SAFETY: `fprog` aponta para um programa BPF válido; NO_NEW_PRIVS já está set.
    let rc = unsafe {
        libc::syscall(libc::SYS_seccomp, SET_MODE_FILTER, FLAG_LOG, &fprog as *const _)
    };
    if rc != 0 {
        // recurso: aplica sem o flag de log (segurança mantém-se).
        let _ = apply_filter(prog);
    }
}

fn apply_seccomp(unconfined: bool, detect: bool) {
    if unconfined {
        return; // `--security-opt seccomp=unconfined`: sem filtro (uso confiável)
    }
    // ALLOWLIST (default-deny): só os syscalls seguros são permitidos; tudo o
    // resto — incl. syscalls FUTUROS/desconhecidos — devolve EPERM. Modelo Docker.
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for nr in allowed_syscalls() {
        rules.insert(nr, vec![]); // match incondicional -> on_match (Allow)
    }
    // `clone`: permitir SÓ quando NÃO cria um novo USER namespace (impede escape
    // por userns aninhado).
    rules.insert(
        libc::SYS_clone,
        vec![rule_arg_masked(0, libc::CLONE_NEWUSER as u64, 0)], // NEWUSER não setado
    );

    let arch = match std::env::consts::ARCH.try_into() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("delonix: arquitectura sem suporte seccomp; a abortar o container");
            unsafe { libc::_exit(126) };
        }
    };

    // Pré-filtro CRÍTICO: `clone3` → ENOSYS. Os flags do clone3 vão num ponteiro
    // (`struct clone_args`) e NÃO podem ser inspeccionados pelo seccomp clássico,
    // por isso um clone3(CLONE_NEWUSER) contornaria o bloqueio do userns acima.
    // Devolvendo ENOSYS forçamos o glibc a cair no `clone` (que É filtrado).
    // ENOSYS (ERRNO) tem precedência sobre o Allow do filtro principal, por isso
    // ganha mesmo com clone3 ainda na allowlist (necessário p/ threads via clone).
    //
    // Instala-se SEMPRE, **inclusive em `detect`**: o modo `detect` afina o *log*
    // dos syscalls negados (FLAG_LOG no filtro principal), NÃO afrouxa o
    // confinamento. Se este pré-filtro só corresse com `!detect`, um container com
    // `--security-opt seccomp=detect` poderia `clone3(CLONE_NEWUSER)` e escapar
    // por userns aninhado — exactamente o buraco que o resto do filtro fecha. A
    // tentativa de userns que daí resulta cai no `clone` filtrado (logado no
    // filtro principal), por isso não se perde detecção.
    let mut pre: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    pre.insert(libc::SYS_clone3, vec![]);
    if let Ok(pf) = SeccompFilter::new(
        pre,
        SeccompAction::Allow,                      // não-casado: deixa passar
        SeccompAction::Errno(libc::ENOSYS as u32), // clone3 → ENOSYS
        arch,
    ) {
        if let Ok(pp) = TryInto::<seccompiler::BpfProgram>::try_into(pf) {
            let _ = apply_filter(&pp);
        }
    }

    let prog: seccompiler::BpfProgram = match SeccompFilter::new(
        rules,
        SeccompAction::Errno(libc::EPERM as u32), // por omissão (não-casado): EPERM
        SeccompAction::Allow,                     // casado (na allowlist): permitir
        arch,
    )
    .and_then(|f| f.try_into())
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("delonix: falha a construir o filtro seccomp: {e}; a abortar");
            unsafe { libc::_exit(126) };
        }
    };
    if detect {
        apply_filter_logged(&prog); // B12: regista os syscalls negados
    } else if let Err(e) = apply_filter(&prog) {
        eprintln!("delonix: falha a aplicar o seccomp: {e}; a abortar o container");
        unsafe { libc::_exit(126) };
    }
}

/// Allowlist de syscalls seguros (baseada no perfil por omissão do Docker, para
/// x86_64). `clone` é tratado à parte (condicional). Os perigosos (mount, ptrace,
/// bpf, kexec, init_module, setns, unshare, …) ficam DE FORA = negados.
fn allowed_syscalls() -> Vec<i64> {
    use libc::*;
    // Allowlist PORTÁVEL (existe em x86_64 e aarch64). Os syscalls legados
    // que só existem em x86_64 (open/stat/fork/*at antigos…) são adicionados
    // condicionalmente — em aarch64 usam-se as variantes `*at`/`clone`.
    let mut v: Vec<i64> = vec![
        // ficheiros / FS
        SYS_read, SYS_write, SYS_openat, SYS_close, SYS_close_range, SYS_fstat,
        SYS_newfstatat, SYS_statx, SYS_ppoll, SYS_lseek, SYS_pread64,
        SYS_pwrite64, SYS_readv, SYS_writev, SYS_preadv, SYS_pwritev, SYS_preadv2, SYS_pwritev2,
        SYS_faccessat, SYS_faccessat2, SYS_dup, SYS_dup3, SYS_pipe2,
        SYS_fcntl, SYS_flock, SYS_fsync, SYS_fdatasync, SYS_truncate, SYS_ftruncate, 
        SYS_getdents64, SYS_getcwd, SYS_chdir, SYS_fchdir, SYS_renameat, SYS_renameat2,
        SYS_mkdirat, SYS_linkat, SYS_unlinkat,
        SYS_symlinkat, SYS_readlinkat, SYS_fchmod,
        SYS_fchmodat, SYS_fchown, SYS_fchownat, SYS_umask, 
        SYS_utimensat, SYS_statfs, SYS_fstatfs, SYS_sync, SYS_syncfs,
        SYS_sync_file_range, SYS_fallocate, SYS_readahead, SYS_openat2, 
        SYS_mknodat, SYS_splice, SYS_tee, SYS_vmsplice, SYS_copy_file_range,
        // xattr
        SYS_getxattr, SYS_lgetxattr, SYS_fgetxattr, SYS_setxattr, SYS_lsetxattr, SYS_fsetxattr,
        SYS_listxattr, SYS_llistxattr, SYS_flistxattr, SYS_removexattr, SYS_lremovexattr,
        SYS_fremovexattr,
        // memória
        SYS_mmap, SYS_munmap, SYS_mprotect, SYS_mremap, SYS_msync, SYS_mincore, SYS_madvise,
        SYS_brk, SYS_mlock, SYS_munlock, SYS_mlockall, SYS_munlockall, SYS_mlock2, SYS_memfd_create,
        SYS_membarrier,
        // processos / threads
        SYS_clone3, SYS_execve, SYS_execveat, SYS_exit, SYS_exit_group,
        SYS_wait4, SYS_waitid, SYS_kill, SYS_tgkill, SYS_tkill, SYS_getpid, SYS_getppid, SYS_gettid,
        SYS_set_tid_address, SYS_set_robust_list, SYS_get_robust_list, SYS_rseq, SYS_futex,
        SYS_prctl, SYS_personality, SYS_getrandom, SYS_uname, SYS_sysinfo,
        SYS_getcpu, SYS_capget, SYS_capset,
        // ids / credenciais (sem privilégio extra — NO_NEW_PRIVS+caps já limitam)
        SYS_getuid, SYS_geteuid, SYS_getgid, SYS_getegid, SYS_setuid, SYS_setgid, SYS_setreuid,
        SYS_setregid, SYS_setresuid, SYS_setresgid, SYS_getresuid, SYS_getresgid, SYS_setfsuid,
        SYS_setfsgid, SYS_getgroups, SYS_setgroups, SYS_getpgid, SYS_setpgid, 
        SYS_getsid, SYS_setsid, SYS_getpriority, SYS_setpriority,
        // limites / scheduling
        SYS_getrlimit, SYS_setrlimit, SYS_prlimit64, SYS_getrusage, SYS_sched_yield,
        SYS_sched_getaffinity, SYS_sched_setaffinity, SYS_sched_getparam, SYS_sched_setparam,
        SYS_sched_getscheduler, SYS_sched_setscheduler, SYS_sched_get_priority_max,
        SYS_sched_get_priority_min, SYS_sched_rr_get_interval,
        // sinais
        SYS_rt_sigaction, SYS_rt_sigprocmask, SYS_rt_sigpending, SYS_rt_sigtimedwait,
        SYS_rt_sigqueueinfo, SYS_rt_sigreturn, SYS_rt_sigsuspend, SYS_sigaltstack, 
        SYS_signalfd4, SYS_restart_syscall,
        // tempo / timers
        SYS_nanosleep, SYS_clock_nanosleep, SYS_clock_gettime, SYS_clock_getres, SYS_gettimeofday,
        SYS_times, SYS_timer_create, SYS_timer_settime, SYS_timer_gettime,
        SYS_timer_getoverrun, SYS_timer_delete, SYS_timerfd_create, SYS_timerfd_settime,
        SYS_timerfd_gettime, SYS_getitimer, SYS_setitimer,
        // epoll / eventfd / inotify
        SYS_pselect6, SYS_epoll_create1, SYS_epoll_ctl,
        SYS_epoll_pwait, SYS_eventfd2, 
        SYS_inotify_init1, SYS_inotify_add_watch, SYS_inotify_rm_watch,
        // AIO clássico (libaio) — o nginx & cia usam-no; o Docker permite-o por
        // omissão. (io_uring fica DE FORA, como no Docker, por ser mais sensível.)
        SYS_io_setup, SYS_io_destroy, SYS_io_getevents, SYS_io_submit, SYS_io_cancel,
        // rede
        SYS_socket, SYS_socketpair, SYS_bind, SYS_listen, SYS_accept, SYS_accept4, SYS_connect,
        SYS_getsockname, SYS_getpeername, SYS_sendto, SYS_recvfrom, SYS_sendmsg, SYS_recvmsg,
        SYS_sendmmsg, SYS_recvmmsg, SYS_shutdown, SYS_setsockopt, SYS_getsockopt,
        // IPC (System V + POSIX mq)
        SYS_shmget, SYS_shmat, SYS_shmdt, SYS_shmctl, SYS_semget, SYS_semop, SYS_semctl,
        SYS_semtimedop, SYS_msgget, SYS_msgsnd, SYS_msgrcv, SYS_msgctl, SYS_mq_open,
        SYS_mq_unlink, SYS_mq_timedsend, SYS_mq_timedreceive, SYS_mq_notify, SYS_mq_getsetattr,
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

/// As capabilities que o container PODE manter (modelo do Docker, menos
/// `CAP_MKNOD` — sem device cgroup, é a forma de impedir o acesso a discos do
/// host). Tudo o resto é removido.
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

/// Número da capability a partir do nome (`CAP_NET_ADMIN` ou `NET_ADMIN`).
fn cap_num(name: &str) -> Option<u8> {
    let n = name.trim().to_ascii_uppercase();
    let n = n.strip_prefix("CAP_").unwrap_or(&n);
    Some(match n {
        "CHOWN" => 0, "DAC_OVERRIDE" => 1, "DAC_READ_SEARCH" => 2, "FOWNER" => 3,
        "FSETID" => 4, "KILL" => 5, "SETGID" => 6, "SETUID" => 7, "SETPCAP" => 8,
        "LINUX_IMMUTABLE" => 9, "NET_BIND_SERVICE" => 10, "NET_BROADCAST" => 11,
        "NET_ADMIN" => 12, "NET_RAW" => 13, "IPC_LOCK" => 14, "IPC_OWNER" => 15,
        "SYS_MODULE" => 16, "SYS_RAWIO" => 17, "SYS_CHROOT" => 18, "SYS_PTRACE" => 19,
        "SYS_PACCT" => 20, "SYS_ADMIN" => 21, "SYS_BOOT" => 22, "SYS_NICE" => 23,
        "SYS_RESOURCE" => 24, "SYS_TIME" => 25, "SYS_TTY_CONFIG" => 26, "MKNOD" => 27,
        "LEASE" => 28, "AUDIT_WRITE" => 29, "AUDIT_CONTROL" => 30, "SETFCAP" => 31,
        "MAC_OVERRIDE" => 32, "MAC_ADMIN" => 33, "SYSLOG" => 34, "WAKE_ALARM" => 35,
        "BLOCK_SUSPEND" => 36, "AUDIT_READ" => 37, "PERFMON" => 38, "BPF" => 39,
        "CHECKPOINT_RESTORE" => 40,
        _ => return None,
    })
}

/// Máscara com TODAS as capabilities suportadas pelo kernel (`--privileged`).
/// Lê `/proc/sys/kernel/cap_last_cap` para não passar bits inválidos ao `capset`
/// (que daria EINVAL). Fallback conservador: CAP_CHECKPOINT_RESTORE (40).
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

/// Calcula a máscara de capabilities a manter: começa em [`KEPT_CAPS`], aplica
/// `--cap-drop` (`ALL` → nenhuma) e depois `--cap-add`.
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
    for c in cap_add {
        if let Some(n) = cap_num(c) {
            keep |= 1u64 << n;
        }
    }
    keep
}

/// Activa `NO_NEW_PRIVS`: um `execve` nunca ganha privilégios (anula setuid/
/// setgid/capabilities de ficheiro). Defesa-chave contra escalada — sempre activo.
fn set_no_new_privs() {
    // SAFETY: prctl simples; idempotente; não falha em kernels suportados.
    unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }
}

/// Remove todas as capabilities excepto `keep` (máscara). Sem isto, o root do
/// container é o root REAL do host (pode carregar módulos, reiniciar a máquina,
/// criar device nodes para o disco do host, etc.).
fn drop_capabilities(keep: u64) {
    // 1) bounding set: impede readquirir caps via setuid/exec.
    for cap in 0..64i64 {
        if (keep >> cap) & 1 == 0 {
            // SAFETY: prctl é seguro; caps inexistentes devolvem EINVAL (ignorado).
            unsafe {
                libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0);
            }
        }
    }
    // 2) effective/permitted/inheritable: ficam só os da allowlist.
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
        pid: 0,               // o próprio thread
    };
    let (lo, hi) = ((keep & 0xffff_ffff) as u32, (keep >> 32) as u32);
    let data = [
        CapData { effective: lo, permitted: lo, inheritable: 0 },
        CapData { effective: hi, permitted: hi, inheritable: 0 },
    ];
    // SAFETY: capset com cabeçalho v3 válido e 2 estruturas de dados; reduzir as
    // próprias caps a um subconjunto é sempre permitido.
    unsafe {
        libc::syscall(libc::SYS_capset, &hdr as *const _, data.as_ptr());
    }
}

/// Decide se o confinamento ficou MESMO em vigor, a partir dos campos lidos de
/// `/proc/self/status`. Lógica pura (testável): exige `NO_NEW_PRIVS`, seccomp em
/// modo filtro (2) quando esperado, e NENHUMA capability fora de `cap_keep` no
/// bounding set nem no effective. `CapBnd`/`CapEff` ausentes = não-verificável =
/// erro (fail-closed).
fn confinement_ok(
    no_new_privs: Option<u32>,
    seccomp_mode: Option<u32>,
    cap_bnd: Option<u64>,
    cap_eff: Option<u64>,
    seccomp_expected: bool,
    cap_keep: u64,
) -> std::result::Result<(), String> {
    if no_new_privs != Some(1) {
        return Err(format!("NO_NEW_PRIVS inativo ({no_new_privs:?})"));
    }
    // 2 = SECCOMP_MODE_FILTER. (`detect` também aplica um filtro → modo 2.)
    if seccomp_expected && seccomp_mode != Some(2) {
        return Err(format!("seccomp não está em modo filtro (Seccomp={seccomp_mode:?})"));
    }
    let bnd = cap_bnd.ok_or_else(|| "CapBnd ausente em /proc/self/status".to_string())?;
    let eff = cap_eff.ok_or_else(|| "CapEff ausente em /proc/self/status".to_string())?;
    let extra_bnd = bnd & !cap_keep;
    let extra_eff = eff & !cap_keep;
    if extra_bnd != 0 || extra_eff != 0 {
        return Err(format!(
            "capabilities fora da allowlist persistem (bnd_extra={extra_bnd:#x} eff_extra={extra_eff:#x})"
        ));
    }
    Ok(())
}

/// FAIL-CLOSED: lê `/proc/self/status` e confirma que `no_new_privs`, o seccomp e o
/// drop de caps ficaram MESMO em vigor antes do `execve`. Cada um destes controlos
/// pode falhar em silêncio (capset/prctl/seccomp são, em parte, best-effort); um
/// controlo de segurança que falha ABERTO é pior que nenhum, porque dá falsa
/// confiança. Se a verificação falhar, o `container_init`/`exec` aborta. Opt-out
/// explícito do OPERADOR (não do container, cujo env ainda não foi aplicado):
/// `DELONIX_INSECURE_BESTEFFORT=1`.
fn verify_confinement(seccomp_expected: bool, cap_keep: u64) -> std::result::Result<(), String> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("/proc/self/status ilegível: {e}"))?;
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

/// O operador desligou as verificações fail-closed? (variável do ENGINE, lida antes
/// de `apply_env` limpar o ambiente — um container não a pode forjar.)
fn insecure_besteffort() -> bool {
    std::env::var_os("DELONIX_INSECURE_BESTEFFORT").is_some()
}

/// Desliga o stdio do terminal em modo detached. `stdin` vai sempre para
/// `/dev/null`; `stdout`/`stderr` vão para `out_fd` (a ponta de escrita do pipe
/// do *logging shim*) se dado, senão para `/dev/null`.
///
/// Sem isto, quem invocou `$(delonix run -d ...)` ficaria bloqueado até o
/// container fechar o stdout — ou seja, até terminar.
fn detach_stdio(out_fd: Option<i32>) {
    // SAFETY: FFI directa; duplica os descritores-padrão. Best-effort.
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
        // a ponta original do pipe já não é precisa (ficou dupada em 1/2).
        if let Some(fd) = out_fd {
            if fd > 2 {
                libc::close(fd);
            }
        }
    }
}

/// Tamanho máximo do ficheiro de log antes de rodar (1 MiB).
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// O *logging shim*: lê o stdout/stderr do container (pela ponta de leitura do
/// pipe) e escreve-o em `log_path`, **rodando** quando passa [`MAX_LOG_BYTES`]
/// (renomeia para `.1` e recomeça). Corre num processo próprio que sobrevive ao
/// `delonix run` (reparentado ao init) e termina quando o container fecha o pipe.
fn log_shim(read_fd: i32, log_path: String, max_bytes: u64, driver: String, tag: String, cri: bool) -> ! {
    // Driver journald/syslog: encaminha cada linha para o syslog (que o journald
    // capta), em vez do ficheiro. `--log-driver journald|syslog`.
    if driver == "journald" || driver == "syslog" {
        log_shim_syslog(read_fd, tag);
    }
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    // SAFETY: `read_fd` é a ponta de leitura do pipe, herdada e válida.
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
    // No modo CRI cada linha sai como `<rfc3339nano> stdout F <linha>\n` — o que o
    // kubelet/crictl sabem parsear. Acumula até ao `\n` (linhas podem vir partidas
    // entre `read`s). Teto p/ uma linha sem `\n` não crescer a RAM sem limite.
    let mut line = Vec::<u8>::new();
    const MAX_LINE: usize = 256 * 1024;
    // Escreve um bloco, rodando ANTES se ultrapassasse `max_bytes` — no modo CRI
    // isto é chamado só com registos COMPLETOS, logo a rotação nunca parte um
    // registo a meio (e a contagem inclui o prefixo, ao contrário de antes).
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
            Ok(0) | Err(_) => break, // EOF: o container fechou o pipe (terminou)
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
                let ts = delonix_core::audit::now_rfc3339_nano();
                let stream = if full { "F" } else { "P" };
                let body = String::from_utf8_lossy(line.strip_suffix(b"\n").unwrap_or(&line));
                let rec = format!("{ts} stdout {stream} {body}\n");
                write_block!(rec.as_bytes());
                line.clear();
            }
        }
    }
    // linha final sem `\n` (modo CRI) — emite na mesma como partial.
    if cri && !line.is_empty() {
        let ts = delonix_core::audit::now_rfc3339_nano();
        let rec = format!("{ts} stdout P {}\n", String::from_utf8_lossy(&line));
        write_block!(rec.as_bytes());
    }
    // SAFETY: sai sem correr destrutores herdados do processo-pai.
    unsafe { libc::_exit(0) }
}

/// Variante do *logging shim* que escreve cada linha no **syslog** (capturado
/// pelo journald em sistemas systemd). `tag` = `delonix/<nome>`.
fn log_shim_syslog(read_fd: i32, tag: String) -> ! {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    // o tag tem de viver enquanto o syslog estiver aberto -> fuga deliberada.
    let ctag = std::ffi::CString::new(tag).unwrap_or_default();
    // SAFETY: openlog com um ponteiro válido que sobrevive ao processo (leaked).
    unsafe { libc::openlog(Box::leak(ctag.into_boxed_c_str()).as_ptr(), libc::LOG_PID, libc::LOG_USER) };
    // SAFETY: `read_fd` é a ponta de leitura do pipe.
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
                            // SAFETY: formato e argumento são ponteiros C válidos.
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
    // SAFETY: termina sem correr destrutores herdados.
    unsafe { libc::_exit(0) }
}

/// Monta um volume/bind no rootfs (antes do `pivot_root`). Zero-copy: o
/// `MS_BIND` partilha os blocos do `source`, não copia dados.
/// Um `target` de bind-mount é seguro? (absoluto e SEM componentes `..`). Defesa
/// contra escape: `bind_volume` corre antes do `pivot_root`, logo um target
/// relativo/`..` montaria sobre o filesystem do HOST.
fn mount_target_safe(target: &str) -> bool {
    let p = std::path::Path::new(target);
    p.is_absolute() && !p.components().any(|c| matches!(c, std::path::Component::ParentDir))
}

fn bind_volume(rootfs: &str, m: &Mount) -> nix::Result<()> {
    if !mount_target_safe(&m.target) {
        return Err(nix::errno::Errno::EINVAL);
    }
    let dst = format!("{rootfs}{}", m.target);
    // Origem ficheiro (ex.: segredo) → o alvo tem de ser um FICHEIRO; origem
    // directório → um directório.
    if std::path::Path::new(&m.source).is_file() {
        if let Some(parent) = std::path::Path::new(&dst).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&dst);
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
    // Remount para aplicar `nosuid`+`nodev` — um bind ignora estas flags no 1.º
    // `mount`, logo sem isto um volume podia trazer binários setuid ou device
    // nodes para dentro do container. `rdonly` adicional se pedido. (`noexec` NÃO,
    // para não partir volumes com executáveis legítimos, ex.: código.)
    let mut rflags = MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_NOSUID | MsFlags::MS_NODEV;
    if m.readonly {
        rflags |= MsFlags::MS_RDONLY;
    }
    mount(None::<&str>, dst.as_str(), None::<&str>, rflags, None::<&str>)?;
    Ok(())
}

/// Os device nodes essenciais que todo o container deve ter em `/dev`.
const ESSENTIAL_DEVS: &[&str] = &["null", "zero", "full", "random", "urandom", "tty"];

/// Monta um `/dev` limpo (tmpfs) e liga os device nodes essenciais do host.
///
/// Sem isto, a imagem traz um `/dev` vazio e, com user namespace, o container
/// nem consegue criar lá ficheiros (o `/dev` é de um uid não-mapeado). Corre
/// ANTES do `pivot_root` (os nós do host ainda são acessíveis) e enquanto temos
/// `CAP_DAC_OVERRIDE` (criador do user ns). Os nós são de caracteres → o device
/// cgroup eBPF permite-os.
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
        let _ = std::fs::File::create(&target); // ponto de montagem (temos CAP_DAC_OVERRIDE)
        // bind do nó real do host (sobrevive ao pivot_root).
        let _ = mount(
            Some(format!("/dev/{name}").as_str()),
            target.as_str(),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        );
    }
    dev_std_symlinks(&dev); // /dev/stdout→/proc/self/fd/1 etc. (logs do nginx/etc.)
    mount_devpts(&dev, true); // pseudo-terminais próprios (gid=5 = grupo tty do host)
    Ok(())
}

/// Cria os symlinks padrão dos *streams* em `<dev>`: `/dev/stdout`, `/dev/stderr`,
/// `/dev/stdin` e `/dev/fd` → `/proc/self/fd/...`. É o que o runc/Docker fazem —
/// e o que faz programas como o nginx (que ligam `access.log` → `/dev/stdout`)
/// escreverem para o stdout CAPTURADO, em vez de para um ficheiro perdido. Os
/// alvos resolvem-se em tempo de execução com o `/proc` do container.
fn dev_std_symlinks(dev: &str) {
    use std::os::unix::fs::symlink;
    let _ = symlink("/proc/self/fd", format!("{dev}/fd"));
    let _ = symlink("/proc/self/fd/0", format!("{dev}/stdin"));
    let _ = symlink("/proc/self/fd/1", format!("{dev}/stdout"));
    let _ = symlink("/proc/self/fd/2", format!("{dev}/stderr"));
}

/// Monta um `devpts` próprio (`newinstance`) em `<dev>/pts` e cria `<dev>/ptmx`
/// → `pts/ptmx`. Dá ao container os seus **próprios** pseudo-terminais — é o que
/// torna o `exec -it` numa shell interactiva real e faz o nome do terminal
/// (`/dev/pts/N`) resolver lá dentro (como o Docker). Best-effort.
fn mount_devpts(dev: &str, with_gid: bool) {
    let pts = format!("{dev}/pts");
    let _ = std::fs::create_dir_all(&pts);
    // `newinstance` isola estes ptys dos do host; `ptmxmode=0666` deixa o
    // multiplexador abrível sem CAP. Sem `gid=5` num user ns (gid não mapeável).
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

/// `/dev` para um container com user namespace. Corre DEPOIS do `setuid(0)` — só
/// então o uid 0 do container é mapeável, e o tmpfs `/dev` fica com dono uid 0 (se
/// fosse montado antes, ficaria com dono `overflow` e o root do container não lá
/// conseguiria escrever). Em user ns não há CAP_MKNOD, por isso ligamos (`bind`) os
/// device nodes REAIS do host por cima dos pontos de montagem — única forma de ter
/// `crw-rw-rw-` reais lá dentro, como o runc/Docker. Os nós do host continuam
/// acessíveis em `old_root` (a raiz antiga preservada pelo `pivot_root`, ainda por
/// desmontar). O caller desmonta `old_root` logo a seguir.
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
        let _ = std::fs::File::create(&target); // ponto de montagem
        let _ = mount(
            Some(format!("{old_root}/dev/{name}").as_str()),
            target.as_str(),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        );
    }
    bind_devices(old_root, "", devices); // --device (nós reais do host, via raiz antiga)
    dev_std_symlinks("/dev"); // /dev/stdout→/proc/self/fd/1 etc.
    mount_devpts("/dev", false); // sem gid=5 (não mapeável no user ns)
}

/// Liga os dispositivos pedidos (`--device /dev/host[:/dev/cont]`) ao `/dev` do
/// container (bind do nó real do host). Char devices são permitidos pelo device
/// cgroup eBPF; os de bloco continuam negados.
///
/// `src_prefix` prefixa o caminho do nó do host (vazio sem user ns — o host ainda
/// é a raiz, antes do `pivot_root`; `/.delonix_old` com user ns — a raiz antiga
/// preservada pelo `pivot_root`, onde o `/dev` do host continua acessível depois
/// do `setuid`). `rootfs` prefixa o ponto de montagem dentro do container.
fn bind_devices(src_prefix: &str, rootfs: &str, devices: &[String]) {
    for spec in devices {
        let mut it = spec.split(':');
        let host = it.next().unwrap_or("");
        if host.is_empty() {
            continue;
        }
        let src = format!("{src_prefix}{host}");
        // Recusa dispositivos de BLOCO em código (não confia só no eBPF, que é
        // best-effort e pode falhar a carregar): dar `/dev/sda` a um container =
        // acesso bruto ao disco do host. Só char devices são permitidos.
        match nix::sys::stat::stat(src.as_str()) {
            Ok(st) => {
                let mode = st.st_mode & libc::S_IFMT;
                if mode == libc::S_IFBLK {
                    eprintln!("delonix: --device {host}: dispositivo de bloco recusado (só char devices)");
                    continue;
                }
            }
            Err(_) => {
                eprintln!("delonix: --device {host}: nó inexistente, ignorado");
                continue;
            }
        }
        // destino: 2.º campo se começar por '/', senão = caminho do host.
        let cont = match it.next() {
            Some(c) if c.starts_with('/') => c,
            _ => host,
        };
        let target = format!("{rootfs}{cont}");
        if let Some(parent) = std::path::Path::new(&target).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::File::create(&target); // ponto de montagem
        let _ = mount(Some(src.as_str()), target.as_str(), None::<&str>, MsFlags::MS_BIND, None::<&str>);
    }
}

/// Monta o rootfs do container e faz `pivot_root` (corre DENTRO da `clone`).
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
    // /dev limpo com os nós essenciais (tmpfs + bind dos device nodes reais do host:
    // única forma de ter `crw-rw-rw-` reais sem CAP_MKNOD). Sem user ns corre AQUI,
    // antes do pivot_root, como root real e enquanto o `/dev` do host é a raiz. Com
    // user ns é feito DEPOIS do setuid (senão o tmpfs ficava com dono `overflow`); os
    // nós do host continuam acessíveis na raiz antiga do pivot_root — ver o caller de
    // setup_rootfs e setup_dev_userns.
    if !userns {
        setup_dev(rootfs)?;
        bind_devices("", rootfs, devices); // --device (nós reais do host)
    }
    // Volumes e bind mounts: injectados ANTES do pivot_root, sobre o rootfs.
    for m in mounts {
        bind_volume(rootfs, m)?;
    }
    let put_old = format!("{rootfs}/.delonix_old");
    let _ = std::fs::create_dir_all(&put_old);
    // Pontos de montagem essenciais: imagens MÍNIMAS (ex.: as `e2e-test-images`
    // do Kubernetes) podem não trazer `/proc` e `/sys`; cria-os no overlay
    // (escrita) ANTES do pivot_root, senão o `mount` de /proc falha com ENOENT e
    // o container não arranca. É o que o runc/Docker fazem (criam os mountpoints).
    let _ = std::fs::create_dir_all(format!("{rootfs}/proc"));
    let _ = std::fs::create_dir_all(format!("{rootfs}/sys"));
    chdir(rootfs)?;
    pivot_root(".", ".delonix_old")?;
    chdir("/")?;
    if host_pid {
        // A partilhar o pidns do host, o kernel recusa montar um procfs NOVO (regra
        // "fully visible" → EPERM); faz-se bind do /proc do host (preservado na raiz
        // antiga pelo pivot_root), que já tem a vista correcta dos processos.
        mount(
            Some("/.delonix_old/proc"),
            "/proc",
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )?;
    } else {
        mount(Some("proc"), "/proc", Some("proc"), MsFlags::empty(), None::<&str>)?;
    }
    apply_sysctls(sysctls); // --sysctl: ANTES de /proc/sys ficar só-leitura (B13)
    mask_proc_paths();
    // `/sys` SÓ-LEITURA (B13): impede escrita em controlos do kernel/dispositivos
    // a partir do container. nosuid/nodev/noexec por defesa. (Ignora se não há /sys.)
    // EXCEÇÃO --privileged: `/sys` RW + `cgroup2` RW delegado, para o systemd dentro
    // do container (nodes Kind) criar e gerir sub-cgroups. Só com `--privileged`.
    let sys_base = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    let _ = mount(
        Some("sysfs"),
        "/sys",
        Some("sysfs"),
        if privileged { sys_base } else { sys_base | MsFlags::MS_RDONLY },
        None::<&str>,
    );
    if privileged {
        // cgroup2 RW por cima de /sys/fs/cgroup. Com CLONE_NEWCGROUP, a vista fica
        // enraizada no cgroup do container (delegado pelo host — pré-requisito
        // cgroup v2 Delegate=yes). nsdelegate deixa o systemd gerir o seu subtree.
        let _ = std::fs::create_dir_all(format!("{rootfs}/sys/fs/cgroup"));
        let _ = mount(
            Some("cgroup2"),
            "/sys/fs/cgroup",
            Some("cgroup2"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            Some("nsdelegate"),
        );
    }
    // Com user ns a raiz antiga é desmontada só DEPOIS do setuid + setup_dev_userns
    // (que precisa dela para ligar os nós reais do host ao /dev) — feito no caller.
    if !userns {
        umount2("/.delonix_old", MntFlags::MNT_DETACH)?;
        let _ = std::fs::remove_dir("/.delonix_old");
    }
    Ok(())
}

/// Mascara as entradas de `/proc` que dão controlo do host: `sysrq-trigger`
/// (pode causar panic/reboot do host) e `kcore` (memória do kernel). Liga-as a
/// `/dev/null`/read-only. Best-effort: corre antes do seccomp, com caps ainda
/// presentes. (Replica as *masked paths* do Docker.)
fn mask_proc_paths() {
    // bind /dev/null por cima de sysrq-trigger -> escritas vão para o vazio.
    let _ = mount(
        Some("/dev/null"),
        "/proc/sysrq-trigger",
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    );
    // /proc/kcore (imagem da RAM do kernel): tornar inacessível.
    let _ = mount(
        Some("/dev/null"),
        "/proc/kcore",
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    );
    // /proc/sys read-only (impede alterar sysctls do host).
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

/// Base e tamanho do mapeamento de UIDs para o user namespace: o root do
/// container (uid 0) passa a ser o uid `USERNS_UID_BASE` (não privilegiado) no
/// host. Sem isto, o root do container é o root REAL do host.
pub const USERNS_UID_BASE: u32 = 100_000;
pub const USERNS_RANGE: u32 = 65_536;

/// Escreve os mapas de uid/gid de um container com user namespace (corre no PAI).
/// - **Como root** (engine com `sudo`): mapeia o intervalo `100000+65536` (root do
///   container = uid não privilegiado no host).
/// - **Rootless** (engine sem `sudo`): mapeia UM só uid — `0 <euid> 1` — porque
///   sem `newuidmap` (helper setuid) um não-root só pode mapear o seu próprio uid.
fn write_userns_maps(pid: i32, want_range: bool) -> Result<()> {
    // SAFETY: geteuid/getegid não têm pré-condições.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    // ROOTLESS + imagem com USER≠0: o uid alvo (ex.: 1000) NÃO existe num mapa de
    // um só uid. Mapeia um INTERVALO via `newuidmap`/`newgidmap` (helpers setuid que
    // consultam /etc/subuid|subgid): container uid 0 → o nosso euid, e 1..N → os
    // subuids delegados. Assim o `setuid(1000)` dentro do container passa a ser
    // válido. Se os helpers/subuid não existirem, cai no mapa de um só uid (e o
    // chamador degrada para correr como root, com aviso).
    if want_range && euid != 0 && have_subid_helpers() {
        let _ = std::fs::write(format!("/proc/{pid}/setgroups"), "deny");
        let range = USERNS_RANGE - 1; // 1..USERNS_RANGE delegados aos subuids
        let map_uid = format!("0 {euid} 1 1 {USERNS_UID_BASE} {range}");
        let map_gid = format!("0 {egid} 1 1 {USERNS_UID_BASE} {range}");
        run_idmap("newuidmap", pid, &map_uid)?;
        run_idmap("newgidmap", pid, &map_gid)?;
        return Ok(());
    }
    // `setgroups=deny` antes do gid_map (boa prática; obrigatório p/ não-root).
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

/// `true` se os helpers `newuidmap`/`newgidmap` existem (necessários p/ mapear um
/// intervalo de subuids em rootless — o caminho do `USER` da imagem ≠ root).
fn have_subid_helpers() -> bool {
    ["/usr/bin/newuidmap", "/bin/newuidmap"].iter().any(|p| std::path::Path::new(p).exists())
        && ["/usr/bin/newgidmap", "/bin/newgidmap"].iter().any(|p| std::path::Path::new(p).exists())
}

/// Corre `newuidmap`/`newgidmap <pid> <map...>` (os args do mapa são tripletos
/// `<id_no_ns> <id_no_host> <count>`).
fn run_idmap(tool: &str, pid: i32, map: &str) -> Result<()> {
    let mut cmd = std::process::Command::new(tool);
    cmd.arg(pid.to_string());
    for tok in map.split_whitespace() {
        cmd.arg(tok);
    }
    let st = cmd.status().map_err(|e| Error::Runtime { context: "idmap", message: format!("{tool}: {e}") })?;
    if !st.success() {
        return Err(Error::Runtime { context: "idmap", message: format!("{tool} falhou (código {:?}) — verifica /etc/subuid e /etc/subgid", st.code()) });
    }
    Ok(())
}

/// `true` se o engine corre sem privilégios de root (modo *rootless*, A13).
pub fn is_rootless() -> bool {
    // SAFETY: geteuid não tem pré-condições.
    unsafe { libc::geteuid() != 0 }
}

/// Remove uma árvore de ficheiros que pode conter ficheiros de **subuid** (chowned
/// para o uid de serviço de um container rootless — ex.: o nginx faz chown das
/// caches para 101 → host 100100). O utilizador (uid real) NÃO os consegue apagar.
/// Solução (estilo `podman unshare rm`): fork dum filho num user namespace; o pai
/// mapeia o intervalo de subuid (`newuidmap`); o filho torna-se root NESSE userns
/// (logo dono efectivo dos subuids) e re-exec `delonix __rmtree <path>` que os apaga.
/// Sem rootless/helpers → remoção directa.
pub fn remove_tree_mapped(path: &std::path::Path) {
    if !is_rootless() || !have_subid_helpers() {
        let _ = std::fs::remove_dir_all(path);
        return;
    }
    // Pré-computa TUDO o que aloca ANTES do fork (no filho pós-fork, num processo que
    // possa ter threads, alocar pode deadlockar — só ops async-signal-safe lá).
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("/usr/local/bin/delonix"));
    let prog = match std::ffi::CString::new(exe.as_os_str().as_encoded_bytes()) {
        Ok(p) => p,
        Err(_) => { let _ = std::fs::remove_dir_all(path); return; }
    };
    let a1 = std::ffi::CString::new("__rmtree").unwrap();
    let a2 = match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(p) => p,
        Err(_) => { let _ = std::fs::remove_dir_all(path); return; }
    };
    let argv = [prog.as_ptr(), a1.as_ptr(), a2.as_ptr(), std::ptr::null()];
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        let _ = std::fs::remove_dir_all(path);
        return;
    }
    let (r, w) = (fds[0], fds[1]);
    // SAFETY: fork; o filho só faz close/unshare/read/setuid/execv (async-signal-safe,
    // sem alocação — os CStrings/argv foram criados acima, antes do fork).
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
            // pequena espera para o filho fazer unshare antes de mapearmos.
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = write_userns_maps(pid, true);
            unsafe {
                let go = [1u8; 1];
                let _ = libc::write(w, go.as_ptr() as *const libc::c_void, 1);
                libc::close(w);
                let mut st = 0;
                libc::waitpid(pid, &mut st, 0);
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

/// `true` se corremos DENTRO de um user namespace não-inicial (uid 0 mapeado, não
/// o root real do host) — ex.: o spawn do ingress rootless, que corre no userns do
/// holder. O userns inicial tem o mapa identidade `0 0 4294967295`; qualquer outro
/// indica um userns-filho. Sem delegação de cgroup também aqui → limites best-effort.
pub fn in_userns() -> bool {
    std::fs::read_to_string("/proc/self/uid_map")
        .map(|s| s.split_whitespace().collect::<Vec<_>>() != ["0", "0", "4294967295"])
        .unwrap_or(false)
}

/// Pede a transição para um perfil AppArmor no próximo `execve`
/// (`aa_change_onexec`). Best-effort: se o AppArmor não estiver disponível,
/// segue sem confinamento MAC.
fn apply_apparmor(profile: &str) {
    let cmd = format!("exec {profile}");
    // Kernels recentes: /proc/self/attr/apparmor/exec; antigos: /proc/self/attr/exec.
    if std::fs::write("/proc/self/attr/apparmor/exec", &cmd).is_err() {
        let _ = std::fs::write("/proc/self/attr/exec", &cmd);
    }
}

/// `true` se o SELinux é o LSM activo (montado em `/sys/fs/selinux`). Em hosts
/// com AppArmor (Debian/Ubuntu) é `false`; em RHEL/Fedora é `true`.
fn selinux_active() -> bool {
    std::path::Path::new("/sys/fs/selinux/enforce").exists()
}

/// Pede a transição para um contexto SELinux no próximo `execve` (`setexeccon`),
/// escrevendo em `/proc/.../attr/exec`. Só actua se o SELinux for o LSM activo —
/// em hosts AppArmor aquele caminho pertence ao AppArmor, daí o *gate*.
/// (Os LSMs major são exclusivos: ou AppArmor ou SELinux.)
fn apply_selinux(context: &str) {
    if selinux_active() && std::fs::write("/proc/thread-self/attr/exec", context).is_err() {
        let _ = std::fs::write("/proc/self/attr/exec", context);
    }
}

/// O corpo que corre dentro dos novos namespaces (o PID 1 do container).
#[allow(clippy::too_many_arguments)]
/// Substitui o ambiente herdado por um limpo e previsível (como o Docker):
/// `PATH`/`HOME`/`HOSTNAME`/`TERM` por omissão + as `KEY=value` da imagem/stack/CLI
/// (estas sobrepõem-se). Corre no filho single-threaded, antes do `execvp`.
fn apply_env(hostname: &str, env: &[String]) {
    let keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    for k in keys {
        std::env::remove_var(k);
    }
    std::env::set_var("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
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

#[allow(clippy::too_many_arguments)] // init do container: muitos parâmetros do namespace/segurança
/// Monta os tmpfs pedidos (`--tmpfs /path[:opts]`). Corre depois do `pivot_root`
/// e antes de largar caps; `nosuid,nodev` por omissão (endurecimento).
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

/// Monta um **tmpfs** em `/run/secrets` e escreve lá os pares chave→valor (0600),
/// para `--secret-files`. Corre DENTRO do namespace do container (pós-`pivot_root`,
/// ainda com caps): os valores ficam só em RAM (tmpfs) — nunca tocam o fs do host
/// nem o do container, nem o ambiente. O mount fica read-only para o container.
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
        // só nomes de ficheiro seguros (são chaves de env válidas, mas defensivo).
        if k.is_empty() || k.contains('/') {
            continue;
        }
        let p = format!("{dir}/{k}");
        if std::fs::write(&p, v).is_ok() {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
    }
    // torna o tmpfs só-leitura para o container (os valores já lá estão).
    let _ = mount(
        None::<&str>,
        dir,
        None::<&str>,
        MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        None::<&str>,
    );
}

/// Escreve os `sysctl`s namespaced (`--sysctl net.x=y`) em `/proc/sys/...` do
/// container (depois de `/proc` estar montado). Só os que o namespace permite.
fn apply_sysctls(specs: &[String]) {
    for kv in specs {
        if let Some((k, v)) = kv.split_once('=') {
            let k = k.trim();
            // Allowlist de sysctls NAMESPACED (modelo Docker): só estes são
            // seguros num container — os restantes (`kernel.*`, `vm.*`, …) são
            // GLOBAIS ao host e um container não os pode tocar. Sem isto, e como
            // isto corre antes de `/proc/sys` ficar RO e antes de largar caps,
            // um container poderia escrever knobs do kernel do HOST.
            if !sysctl_namespaced(k) {
                eprintln!("delonix: --sysctl {k}: não-namespaced; recusado (afecta o host)");
                continue;
            }
            let path = format!("/proc/sys/{}", k.replace('.', "/"));
            let _ = std::fs::write(&path, v.trim());
        }
    }
}

/// `true` se o sysctl é namespaced (seguro para um container alterar). Mesmo
/// conjunto que o Docker permite por omissão.
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

/// O tipo do 1.º argumento de `setrlimit`: enum (`__rlimit_resource_t`) no glibc,
/// `c_int` no musl. Alias condicional para a build estática musl compilar.
#[cfg(target_env = "musl")]
type RlimitResource = libc::c_int;
#[cfg(not(target_env = "musl"))]
type RlimitResource = libc::__rlimit_resource_t;

/// Mapeia o nome de um `--ulimit` ao recurso `RLIMIT_*`.
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

/// Aplica `--ulimit nome=soft[:hard]` via `setrlimit` (antes de largar caps, para
/// poder subir limites rígidos com `CAP_SYS_RESOURCE`).
fn apply_ulimits(specs: &[String]) {
    let parse = |s: &str| -> Option<u64> {
        if s == "unlimited" || s == "-1" {
            Some(libc::RLIM_INFINITY)
        } else {
            s.parse().ok()
        }
    };
    for spec in specs {
        let Some((name, vals)) = spec.split_once('=') else { continue };
        let Some(res) = rlimit_resource(name.trim()) else { continue };
        let (soft, hard) = match vals.split_once(':') {
            Some((s, h)) => (s, h),
            None => (vals, vals),
        };
        if let (Some(rc), Some(rm)) = (parse(soft.trim()), parse(hard.trim())) {
            let rl = libc::rlimit { rlim_cur: rc, rlim_max: rm };
            // SAFETY: `res` é um RLIMIT_* válido e `rl` está inicializado.
            unsafe { libc::setrlimit(res, &rl) };
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// (privileged / node Kind) Dá ao container uma RAIZ DE CGROUP DEDICADA e VAZIA.
///
/// No caminho rootless-com-rede o `ip netns exec` monta um sysfs FRESCO sobre
/// `/sys`, TAPANDO o cgroup2 delegado; e o node herdaria como raiz do seu cgroup-ns
/// o próprio cgroup-scope onde o `kind` (e helpers do delonix) correm — mas o
/// kubelet do node aborta se essa raiz tiver processos DIRETOS
/// (`create-kubelet-cgroup-v2.sh` precisa de escrever o `cgroup.subtree_control`
/// de topo). Solução: (1) destapar o cgroup2 real (umount do sysfs), (2) mover-nos
/// para um leaf `<base>/dlx-<id>` VAZIO, (3) `unshare(CLONE_NEWCGROUP)` — a raiz do
/// cgroup-ns passa a ser o leaf (só o nosso init) e `kind`/`delonix`/helpers ficam
/// ACIMA dela. Best-effort: o `unshare` final corre SEMPRE (mesmo sem o leaf dá o
/// cgroup-ns como antes — sem regressão para privileged não-node).
fn setup_node_cgroup_ns(cid: &str) {
    // 1) Destapa o cgroup2 real se o `/sys/fs/cgroup` visível for um sysfs vazio
    //    (remount do `ip netns exec`). Um cgroup2 real expõe `cgroup.controllers`.
    let real_cg2 = std::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !real_cg2 {
        // Torna `/` (recursivamente) PRIVADO ANTES do umount, para o umount de
        // `/sys` NUNCA propagar ao mount-ns do holder — que os `exec` de
        // `journalctl` da deteção de readiness do Kind usam. Sem isto, um umount
        // propagado deixava intermitentemente o `/sys` do holder partido → o
        // `docker logs -f` não surfava a readiness → o Kind pendurava em
        // "Preparing nodes". SAFETY: root no userns (caps completas), só neste ns.
        let _ = mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            None::<&str>,
        );
        let _ = umount2("/sys", MntFlags::empty());
    }
    // 2) Move-nos para um leaf IRMÃO do cgroup do `kind`, sob o SCOPE-pai, e delega
    //    os controladores do scope ao leaf. O `kind` (e os helpers) correm em
    //    `<scope>/kind` (ver `paas.rs`), libertando a raiz do `<scope>`; assim o
    //    leaf `<scope>/dlx-<id>` fica com cpu delegado (o entrypoint do node exige)
    //    E como raiz do cgroup-ns fica com 0 processos diretos (o kubelet exige).
    if let Some(base) = std::fs::read_to_string("/proc/self/cgroup").ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("0::").map(|r| format!("/sys/fs/cgroup{}", r.trim())))
    }) {
        // scope = pai do cgroup atual (o node herda `<scope>/kind` do `kind`).
        if let Some(scope) = std::path::Path::new(&base).parent().map(|p| p.to_path_buf()) {
            let scope = scope.to_string_lossy().to_string();
            // Delega os controladores do scope aos filhos (só engata se o scope não
            // tiver processos diretos — garantido por `paas.rs` ao pôr o kind em
            // `<scope>/kind`). Best-effort: falha silenciosa em hosts sem esta estrutura.
            for ctrl in ["+cpu", "+memory", "+pids"] {
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
    // 3) Ancora a raiz do cgroup-ns no cgroup ATUAL (o leaf, se o move funcionou).
    let _ = unshare(CloneFlags::CLONE_NEWCGROUP);
}

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
    join_netns: Option<&str>,
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
) -> isize {
    // User namespace: espera que o PAI escreva uid_map/gid_map antes de continuar
    // (até lá, somos `nobody` sem caps). O byte recebido é o "podes avançar".
    // No ingress rootless herdamos o userns do holder (já como uid 0) — sem clone
    // nem sync, mas o rootfs trata-se como `userns` (somos root no userns herdado).
    let userns = sync.is_some() || inherit_userns;
    if let Some((r, w)) = sync {
        // SAFETY: fds do pipe herdados da clone; fecha o write, lê 1 byte do read.
        unsafe {
            libc::close(w);
            let mut b = [0u8; 1];
            let _ = libc::read(r, b.as_mut_ptr() as *mut libc::c_void, 1);
            libc::close(r);
        }
    }
    // Pod: junta-se ao network namespace do infra container (partilha IP/localhost).
    if let Some(path) = join_netns {
        match open(path, OFlag::O_RDONLY | OFlag::O_CLOEXEC, Mode::empty()) {
            Ok(fd) => {
                // SAFETY: fd válido; setns(NEWNET) junta o netns do pod.
                let owned = unsafe { OwnedFd::from_raw_fd(fd) };
                if setns(owned, CloneFlags::CLONE_NEWNET).is_err() {
                    eprintln!("delonix: falha a juntar ao netns do pod");
                    return 125;
                }
            }
            Err(_) => {
                eprintln!("delonix: netns do pod indisponível");
                return 125;
            }
        }
    }
    if detach {
        detach_stdio(log_fd);
    }
    // --privileged (nodes Kind): ANTES de montar o rootfs (que remonta `/sys`),
    // dá ao node uma raiz de cgroup-ns dedicada e vazia (destapa o cgroup2 real,
    // move-nos p/ um leaf, `unshare(NEWCGROUP)`). Assim o cgroup2 que o rootfs vai
    // montar reflete o leaf como raiz. Best-effort; só para privileged.
    if privileged {
        setup_node_cgroup_ns(cid);
    }
    // `setup_rootfs` corre como o criador do user ns (caps completas, mesmo sendo
    // `nobody`): o pivot_root e os ficheiros vão para o overlay do host (que aceita
    // o uid do host). Sem user ns, monta logo o `/dev` (bind dos nós reais do host).
    // Com user ns, o `/dev` é montado a seguir, já depois do setuid — ver abaixo.
    if let Err(e) = setup_rootfs(rootfs, hostname, mounts, userns, devices, sysctls, host_pid, privileged) {
        eprintln!("delonix: falha a preparar o rootfs: {e}");
        return 126;
    }
    if userns {
        // uid 0 DENTRO do user ns (= USERNS_UID_BASE no host, mapeável).
        // nonzero->0 copia permitted->effective (mantém caps).
        // SAFETY: somos o criador do user ns -> temos CAP_SETUID/SETGID.
        unsafe {
            libc::setgid(0);
            libc::setuid(0);
        }
        // /dev: tmpfs (agora com dono uid 0) + bind dos nós reais do host a partir da
        // raiz antiga preservada pelo pivot_root. Depois desmonta-a (já não é precisa).
        setup_dev_userns("/.delonix_old", devices);
        let _ = umount2("/.delonix_old", MntFlags::MNT_DETACH);
        let _ = std::fs::remove_dir("/.delonix_old");
    }
    // --privileged detached (nodes Kind): aloca um `/dev/console` (pty) para o
    // PID 1 e captura-o no log. Tem de ser DEPOIS do `/dev`/devpts montado (acima)
    // e ANTES de largar caps (o bind de `/dev/console` precisa de CAP_SYS_ADMIN).
    // O `detach_stdio` acima já apontou o stdio herdado para /dev/null; isto
    // reaponta-o para o pty. Ver `setup_console`.
    if let Some(cs) = console_sock {
        setup_console(cs);
    }
    apply_tmpfs(tmpfs); // --tmpfs (depois do pivot, ainda com caps)
    if !secret_files.is_empty() {
        write_secret_files(secret_files); // --secret-files: tmpfs in-namespace (ainda com caps)
    }
    // `--read-only`: remonta o rootfs (`/`) só-leitura. Volumes/dev/proc são
    // mounts separados e mantêm-se escrevíveis; o resto fica imutável.
    if read_only {
        let _ = mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY,
            None::<&str>,
        );
    }
    apply_ulimits(ulimits); // --ulimit (antes de largar CAP_SYS_RESOURCE)
    set_no_new_privs(); // nenhum execve ganha privilégios (anti-escalada) — sempre
    drop_capabilities(cap_keep); // largar caps (depois das montagens, antes do exec)
    apply_seccomp(seccomp_unconfined, seccomp_detect); // allowlist (default-deny)
    // FAIL-CLOSED: confirma que o confinamento ficou MESMO em vigor antes do execve.
    // Corre ANTES do `setuid` do USER (mais abaixo) e ANTES do `apply_env` (logo lê o
    // opt-out do ENGINE, não do container). Ver `verify_confinement`.
    if !insecure_besteffort() {
        if let Err(e) = verify_confinement(!seccomp_unconfined, cap_keep) {
            eprintln!("delonix: confinamento NÃO verificado ({e}); a abortar o container");
            return 126;
        }
    }
    if let Some(p) = apparmor {
        apply_apparmor(p); // confinamento MAC (AppArmor) — transita no execve
    }
    if let Some(c) = selinux {
        apply_selinux(c); // confinamento MAC (SELinux) — só em hosts SELinux
    }
    // CWD da imagem (OCI `WorkingDir`) — DEPOIS do pivot, ANTES do exec. Sem isto,
    // entrypoints que operam no CWD (redis/postgres `chown -R .`) correm a partir de `/`
    // e tocam `/sys` (RO). Se o dir não existir, cria-o (semântica Docker do WORKDIR).
    if let Some(w) = workdir.filter(|w| !w.is_empty() && *w != "/") {
        let _ = std::fs::create_dir_all(w);
        if chdir(w).is_err() {
            eprintln!("delonix: aviso — falha a entrar no WORKDIR {w}");
        }
    }
    apply_env(hostname, env); // ambiente limpo + ENV da imagem/stack/CLI
    // `USER` da imagem (≠ root): troca para o uid/gid pedido ANTES do `execve`. Faz-se
    // por último — depois das montagens/caps/seccomp, que precisaram de uid 0. Estamos
    // dentro do user namespace (root do ns), logo temos CAP_CHOWN/SETUID sobre o
    // intervalo mapeado: damos a posse do rootfs ao uid (uma vez; marcador para não
    // repetir) e largamos privilégios. setgid ANTES de setuid (depois de setuid já não
    // se pode mudar de grupo). Ex.: o Elasticsearch recusa correr como root.
    if let Some(uid) = run_uid {
        if uid != 0 {
            let gid = run_gid.unwrap_or(uid);
            chown_tree_once("/", uid, gid);
            // O stdout/stderr são o pipe do log_shim, criado como uid 0. Imagens
            // "unprivileged" (nginx, etc.) ligam /var/log/.../*.log → /dev/stdout
            // (= /proc/self/fd/1) e REABREM-no já como o USER — o que falharia sem
            // o pipe lhes pertencer. fchown dos 3 fds dá-lhes esse acesso.
            // SAFETY: fchown sobre fds abertos válidos (0/1/2); erros ignorados.
            unsafe {
                libc::fchown(0, uid, gid);
                libc::fchown(1, uid, gid);
                libc::fchown(2, uid, gid);
            }
            // SAFETY: somos root no user ns → setgid/setgroups/setuid sucedem.
            unsafe {
                libc::setgroups(1, [gid].as_ptr());
                if libc::setgid(gid) != 0 {
                    eprintln!("delonix: setgid({gid}) falhou");
                }
                if libc::setuid(uid) != 0 {
                    eprintln!("delonix: setuid({uid}) falhou — o USER da imagem não está mapeado (subuid?)");
                    return 126;
                }
            }
        }
    }
    let _ = execvp(&argv[0], argv);
    eprintln!("delonix: exec falhou: {:?}", argv[0]);
    127
}

/// `chown -R <uid>:<gid>` no rootfs do container (caminho `root` já dentro do
/// pivot_root). Idempotente via um marcador `/.delonix_user_<uid>` — só corre na
/// 1.ª vez para um dado uid, evitando o custo a cada arranque. Best-effort: erros
/// individuais são ignorados (ficheiros especiais), o que conta é a árvore da app.
fn chown_tree_once(root: &str, uid: u32, gid: u32) {
    let marker = format!("{}/.delonix_user_{uid}", root.trim_end_matches('/'));
    if std::path::Path::new(&marker).exists() {
        return;
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
            // não segue symlinks (lchown via libc); recursa só em dirs reais.
            let ft = ent.file_type().ok();
            let cpath = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok();
            if let Some(c) = &cpath {
                // SAFETY: lchown sobre um caminho válido; não segue symlink.
                unsafe { libc::lchown(c.as_ptr(), uid, gid); }
            }
            if ft.map(|t| t.is_dir()).unwrap_or(false) {
                rec(&p, uid, gid, depth + 1);
            }
        }
    }
    let rootp = std::path::Path::new(root);
    // chown da própria raiz + árvore (exceto /proc, /sys, /dev que são mounts).
    for top in std::fs::read_dir(rootp).into_iter().flatten().flatten() {
        let name = top.file_name();
        if matches!(name.to_str(), Some("proc") | Some("sys") | Some("dev")) {
            continue;
        }
        let p = top.path();
        if let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) {
            // SAFETY: lchown sobre caminho válido.
            unsafe { libc::lchown(c.as_ptr(), uid, gid); }
        }
        if top.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            rec(&p, uid, gid, 0);
        }
    }
    let _ = std::fs::File::create(&marker);
}

/// Limite de PIDs por container (anti fork-bomb).
const DEFAULT_PIDS_MAX: &str = "512";

/// Codifica uma instrução eBPF (8 bytes) num `u64` (little-endian).
fn bpf_insn(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> u64 {
    (code as u64)
        | (((dst & 0xf) as u64) << 8)
        | (((src & 0xf) as u64) << 12)
        | ((off as u16 as u64) << 16)
        | ((imm as u32 as u64) << 32)
}

/// Carrega um programa eBPF `CGROUP_DEVICE` que **nega dispositivos de bloco**
/// (discos) e permite os de caracteres, e anexa-o ao cgroup do container. É o
/// *device cgroup* do cgroup v2 (controlo de dispositivos por eBPF, como o runc).
/// Best-effort: se o kernel não suportar, as outras camadas (caps/seccomp/userns/
/// AppArmor) já negam o acesso a dispositivos.
fn attach_device_filter(cgroup: &str) -> bool {
    const BPF_PROG_LOAD: i64 = 5;
    const BPF_PROG_ATTACH: i64 = 8;
    const BPF_PROG_TYPE_CGROUP_DEVICE: u32 = 15;
    const BPF_CGROUP_DEVICE: u32 = 6;

    // Programa: r2 = ctx->access_type; tipo = r2 & 0xffff;
    //           se tipo == 1 (BLK) -> r0=0 (negar); senão r0=1 (permitir).
    let insns: [u64; 7] = [
        bpf_insn(0x61, 2, 1, 0, 0),        // LDX_W r2 = *(u32*)(r1+0)
        bpf_insn(0x54, 2, 0, 0, 0xffff),   // AND32 r2 &= 0xffff
        bpf_insn(0x15, 2, 0, 2, 1),        // JEQ r2,1 -> +2 (BLK = negar)
        bpf_insn(0xb7, 0, 0, 0, 1),        // MOV r0 = 1 (permitir)
        bpf_insn(0x95, 0, 0, 0, 0),        // EXIT
        bpf_insn(0xb7, 0, 0, 0, 0),        // MOV r0 = 0 (negar)
        bpf_insn(0x95, 0, 0, 0, 0),        // EXIT
    ];
    let license = b"GPL\0";
    let mut log = [0u8; 4096];

    // bpf_attr para PROG_LOAD (buffer zerado; campos nos offsets do kernel).
    let mut attr = [0u8; 128];
    attr[0..4].copy_from_slice(&BPF_PROG_TYPE_CGROUP_DEVICE.to_ne_bytes());
    attr[4..8].copy_from_slice(&(insns.len() as u32).to_ne_bytes());
    attr[8..16].copy_from_slice(&(insns.as_ptr() as u64).to_ne_bytes());
    attr[16..24].copy_from_slice(&(license.as_ptr() as u64).to_ne_bytes());
    attr[24..28].copy_from_slice(&1u32.to_ne_bytes()); // log_level
    attr[28..32].copy_from_slice(&(log.len() as u32).to_ne_bytes());
    attr[32..40].copy_from_slice(&(log.as_mut_ptr() as u64).to_ne_bytes());

    // SAFETY: chamada bpf(PROG_LOAD) com um bpf_attr válido e zerado.
    let prog_fd = unsafe { libc::syscall(libc::SYS_bpf, BPF_PROG_LOAD, attr.as_ptr(), attr.len()) };
    if prog_fd < 0 {
        return false;
    }

    let cg = match std::ffi::CString::new(cgroup) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // SAFETY: abre o directório do cgroup como fd para o attach.
    let cg_fd = unsafe { libc::open(cg.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if cg_fd < 0 {
        unsafe { libc::close(prog_fd as i32) };
        return false;
    }

    let mut at = [0u8; 128];
    at[0..4].copy_from_slice(&(cg_fd as u32).to_ne_bytes()); // target_fd (cgroup)
    at[4..8].copy_from_slice(&(prog_fd as u32).to_ne_bytes()); // attach_bpf_fd
    at[8..12].copy_from_slice(&BPF_CGROUP_DEVICE.to_ne_bytes()); // attach_type
    // SAFETY: bpf(PROG_ATTACH) liga o programa ao cgroup.
    let r = unsafe { libc::syscall(libc::SYS_bpf, BPF_PROG_ATTACH, at.as_ptr(), at.len()) };
    unsafe {
        libc::close(prog_fd as i32);
        libc::close(cg_fd);
    }
    r == 0
}

/// Converte `cpus` (ex.: "0.5", "2") na sintaxe `cpu.max` do cgroup v2
/// (`<quota> <period>`); `period` = 100000 µs. Mínimo 0.01 de um core.
fn cpu_max_value(cpus: &str) -> String {
    let c: f64 = cpus.parse().unwrap_or(1.0);
    let quota = ((c * 100_000.0).round() as i64).max(1000);
    format!("{quota} 100000")
}

/// Escreve um limite no cgroup; falhar é ERRO (os limites são OBRIGATÓRIOS — um
/// container nunca deve correr sem teto de recursos).
fn write_limit(cgroup: &str, file: &str, value: &str) -> Result<()> {
    std::fs::write(format!("{cgroup}/{file}"), value).map_err(|e| Error::Runtime {
        context: "cgroup limit",
        message: format!("{file}={value}: {e}"),
    })
}

/// Cria um cgroup dedicado e aplica limites OBRIGATÓRIOS de memória, CPU e PIDs,
/// depois move `pid` para lá. Ao contrário do Docker (que por omissão não limita
/// nada), o Delonix recusa-se a correr um container sem tetos de recursos.
/// Percentagem do host reservada ao Delonix no total (o resto é folga do host).
fn host_reserve_pct() -> u64 {
    std::env::var("DELONIX_RESERVE_PCT")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|p| (10..=95).contains(p))
        .unwrap_or(85)
}

/// Memória total do host (bytes), de `/proc/meminfo`.
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
    std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1)
}

/// Tecto agregado de I/O de disco da slice em bytes/s (`DELONIX_IO_MAX_BPS`,
/// def. **500 MB/s**). 0 desactiva o limite. Serve de tecto de segurança contra
/// um container saturar o disco e matar o host, não de QoS fino.
fn host_io_max_bps() -> u64 {
    std::env::var("DELONIX_IO_MAX_BPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500_000_000)
}

/// `MAJ:MIN` do dispositivo de bloco que suporta o store do Delonix (onde os
/// overlays/imagens vivem). Necessário para o `io.max` da slice. O `io.max` do
/// cgroup-v2 exige o disco INTEIRO (não uma partição: uma partição dá ENODEV),
/// por isso resolvemos o device-pai quando o que contém o store é uma partição.
fn slice_io_device() -> Option<String> {
    // O store fica sob /var/lib/delonix (root) — usa o device que o contém.
    let probe = ["/var/lib/delonix", "/var/lib", "/"];
    for p in probe {
        if let Ok(st) = nix::sys::stat::stat(p) {
            let dev = st.st_dev;
            let (maj, min) = (libc::major(dev), libc::minor(dev));
            if maj == 0 {
                continue; // device virtual (overlay/tmpfs) — sem io.max útil
            }
            // Se for uma partição, sobe ao disco-pai (`/sys/dev/block/M:m/../dev`).
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

/// Carga média a 1 minuto do host (`/proc/loadavg`).
fn host_load1() -> Option<f64> {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|f| f.parse().ok()))
}

/// Converte `64M`/`1G`/`512K`/bytes em bytes.
fn parse_mem_bytes(s: &str) -> u64 {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1024u64),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    // `saturating_mul` evita overflow (ex.: "99999999999G"); um valor não-parseável
    // satura para u64::MAX (e não 0), para o controlo de admissão recusar — nunca
    // tratar lixo como "0 bytes" e deixar passar.
    match num.trim().parse::<u64>() {
        Ok(n) => n.saturating_mul(mult),
        Err(_) => u64::MAX,
    }
}

/// Garante a `delonix.slice` com limites AGREGADOS (uma fracção do host) e os
/// controladores activos para os filhos. É o que impede que a SOMA de todos os
/// containers mate o host: a slice tem `memory.max`/`cpu.max`/`pids.max` totais,
/// e o kernel OOM-mata DENTRO da slice (um container), nunca o host. Idempotente.
pub fn ensure_delonix_slice() {
    let slice = delonix_core::DELONIX_SLICE;
    if std::fs::create_dir_all(slice).is_err() {
        return; // sem permissão (rootless) → best-effort
    }
    let pct = host_reserve_pct();
    let mem = host_mem_bytes();
    if mem > 0 {
        let _ = std::fs::write(format!("{slice}/memory.max"), (mem / 100 * pct).to_string());
        let _ = std::fs::write(format!("{slice}/memory.swap.max"), "0");
    }
    let ncpu = host_ncpu();
    let quota = ncpu * 100_000 / 100 * pct; // pct% de `ncpu` cores
    let _ = std::fs::write(format!("{slice}/cpu.max"), format!("{quota} 100000"));
    let _ = std::fs::write(format!("{slice}/pids.max"), (ncpu * 4096).to_string());
    // Tecto de I/O de DISCO agregado: sem isto, um único container a escrever a
    // fundo satura o disco e mata o host (journald/store/swap) mesmo com CPU e
    // memória limitadas. `io.max` (cgroup-v2) limita rbps/wbps no dispositivo que
    // suporta o store. Best-effort: pode estar acima do limite real do device —
    // serve de tecto de segurança, não de QoS fino. Ajustável por env.
    if let Some(dev) = slice_io_device() {
        let cap_bps = host_io_max_bps();
        if cap_bps > 0 {
            let _ = std::fs::write(
                format!("{slice}/io.max"),
                format!("{dev} rbps={cap_bps} wbps={cap_bps}"),
            );
        }
    }
    // activa os controladores para os filhos (UM a um — se algum não existir no
    // host, os outros ficam à mesma activos).
    for ctrl in ["+memory", "+cpu", "+pids", "+io"] {
        let _ = std::fs::write(format!("{slice}/cgroup.subtree_control"), ctrl);
    }
}

/// Controlo de ADMISSÃO (robustez, #1/#4): recusa graciosamente um novo
/// container quando o orçamento agregado do Delonix está esgotado ou o host está
/// sob carga excessiva — em vez de deixar o host afogar-se. (A slice já é o
/// tecto rígido; isto é a recusa suave e informativa.)
pub fn admission_check(memory_max: &str) -> Result<()> {
    if is_rootless() {
        return Ok(()); // sem cgroup delegado → sem orçamento a verificar
    }
    ensure_delonix_slice();
    let slice = delonix_core::DELONIX_SLICE;
    let read = |f: &str| -> Option<u64> {
        std::fs::read_to_string(format!("{slice}/{f}")).ok().and_then(|s| s.trim().parse().ok())
    };
    if let (Some(cap), Some(cur)) = (read("memory.max"), read("memory.current")) {
        let want = parse_mem_bytes(memory_max);
        if cur.saturating_add(want) > cap {
            return Err(Error::Runtime {
                context: "admissão",
                message: format!(
                    "protecção do host: orçamento de memória do Delonix esgotado \
                     ({} MiB usados de {} MiB; este container pede {}). \
                     Pára containers ou sobe DELONIX_RESERVE_PCT.",
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
                context: "admissão",
                message: format!(
                    "protecção do host: carga média demasiado alta ({load1:.1} > {limit:.0}) — tenta mais tarde"
                ),
            });
        }
    }
    Ok(())
}

/// Temperatura máxima (°C) entre os sensores térmicos do host (CPU e afins).
/// Base do governador térmico (#2): quando o Delonix aquece a máquina, baixamos
/// o tecto de CPU da slice para reduzir a fonte de calor.
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

/// O quota TOTAL de CPU da delonix.slice (100% do orçamento), em µs/período.
pub fn slice_full_cpu_quota() -> u64 {
    host_ncpu() * 100_000 / 100 * host_reserve_pct()
}

/// Define o `cpu.max` da slice como `pct`% do orçamento total — o governador
/// térmico baixa-o para arrefecer e repõe-no quando a temperatura desce.
pub fn set_slice_cpu_pct(pct: u64) {
    ensure_delonix_slice();
    // Piso de segurança: nunca escrever quota 0 (`cpu.max "0 100000"` congelaria
    // TODOS os containers da slice). Garante pelo menos ~1% de um core.
    let quota = (slice_full_cpu_quota() * pct.min(100) / 100).max(1_000);
    let _ = std::fs::write(
        format!("{}/cpu.max", delonix_core::DELONIX_SLICE),
        format!("{quota} 100000"),
    );
}

/// Best-effort: tenta pôr os ventiladores controláveis no máximo (se existirem
/// `pwmN` escrevíveis). Em muitos portáteis o PWM é gerido pelo firmware e não é
/// escrevível — por isso o arrefecimento real é o *throttle* da slice.
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

/// Estado do orçamento agregado do Delonix (para `system info`).
pub fn slice_budget() -> (u64, u64, u64, f64, u64) {
    ensure_delonix_slice();
    let slice = delonix_core::DELONIX_SLICE;
    let read = |f: &str| -> u64 {
        std::fs::read_to_string(format!("{slice}/{f}")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    };
    (
        read("memory.max"),
        read("memory.current"),
        read("pids.current"),
        host_load1().unwrap_or(0.0),
        host_ncpu(),
    )
}

/// Cgroup v2 atual do processo (de `/proc/self/cgroup`, linha `0::`).
fn current_cgroup_v2() -> Option<String> {
    let s = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = s.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    Some(format!("/sys/fs/cgroup{rel}"))
}

/// Delegação cgroup ROOTLESS para um container **privileged** (nodes Kind, que
/// verificam que o controlador `cpu` está delegado). Usa o cgroup DELEGADO do
/// próprio processo (sob `user@<uid>.service`, escrevível) como base, move o
/// delonix e o container para leaves (regra no-internal-processes do cgroup v2) e
/// ativa `+cpu +memory +pids` no `subtree_control` da base — passando os
/// controladores ao cgroup do container. Best-effort: devolve `false` se a base
/// não for delegada/limpa (ex.: scope partilhado sem `cpu`), caindo o caller no
/// comportamento atual (sem regressão). Requer o engine num cgroup delegado
/// (`systemd-run --user --scope -p Delegate=yes` ou serviço de utilizador).
fn setup_cgroup_delegated(c: &Container, pid: i32) -> bool {
    let base = match current_cgroup_v2() {
        Some(b) => b,
        None => return false,
    };
    if std::fs::metadata(format!("{base}/cgroup.subtree_control")).is_err() {
        return false; // base não escrevível/delegada
    }
    let leaf = format!("{base}/dlx-{}", c.id);
    let mgr = format!("{base}/dlx-mgr");
    if std::fs::create_dir_all(&leaf).is_err() || std::fs::create_dir_all(&mgr).is_err() {
        return false;
    }
    // 1) container → leaf;  2) o nosso processo → mgr (liberta a base de processos).
    if std::fs::write(format!("{leaf}/cgroup.procs"), pid.to_string()).is_err() {
        return false;
    }
    let _ = std::fs::write(format!("{mgr}/cgroup.procs"), std::process::id().to_string());
    // 3) delega os controladores aos filhos da base (um a um; falha se a base
    // ainda tiver processos diretos → scope partilhado → abortar p/ fallback).
    let mut any = false;
    for ctrl in ["+cpu", "+memory", "+pids"] {
        if std::fs::write(format!("{base}/cgroup.subtree_control"), ctrl).is_ok() {
            any = true;
        }
    }
    if !any {
        return false; // sem delegação (no-internal-processes ou sem permissão)
    }
    // limites best-effort no leaf (agora com os controladores disponíveis).
    let _ = std::fs::write(format!("{leaf}/memory.max"), &c.memory_max);
    let _ = std::fs::write(format!("{leaf}/pids.max"), DEFAULT_PIDS_MAX);
    true
}

fn setup_cgroup(c: &Container, pid: i32) -> Result<()> {
    // Rootless: tenta SEMPRE a delegação cgroup (cpu/memory/pids) no cgroup
    // delegado do utilizador (systemd --user com `Delegate=yes`) — é o modelo do
    // Podman e a forma de ter limites REAIS sem root. Antes só se tentava com
    // `--privileged` (nodes Kind), deixando TODO container rootless-com-rede sem
    // memory.max/pids.max/cpu.max — um fork-bomb/leak matava o host. Se a
    // delegação não existir, devolve false e cai no caminho best-effort abaixo.
    if (is_rootless() || in_userns()) && setup_cgroup_delegated(c, pid) {
        return Ok(());
    }
    ensure_delonix_slice(); // a slice-pai com os limites agregados (robustez)
    let cgroup = c.cgroup();
    let cg = cgroup.as_str();
    // Rootless (A13): sem delegação de cgroup (systemd), um não-root não pode
    // escrever em `/sys/fs/cgroup`. Os limites tornam-se best-effort — o
    // isolamento de namespaces/seccomp mantém-se. (Como o Podman rootless.)
    // Também `in_userns()`: o ingress rootless corre o spawn DENTRO do userns do
    // holder (uid 0 MAPEADO) — `is_rootless()` (geteuid) seria falso, mas não há
    // delegação de cgroup na mesma; tratamos como rootless.
    if std::fs::create_dir_all(cg).is_err() {
        if is_rootless() || in_userns() {
            eprintln!(
                "delonix: aviso — rootless SEM delegação de cgroup: memory/cpu/pids NÃO são\n\
                 \x20        aplicados (um fork-bomb ou leak pode afetar o host). Para ter\n\
                 \x20        limites, corre o engine sob uma sessão systemd --user com\n\
                 \x20        delegação: `systemctl --user edit --force --full delonix.service`\n\
                 \x20        com `[Service] Delegate=yes`, ou inicia via `systemd-run --user\n\
                 \x20        --scope -p Delegate=yes ...`. O isolamento de namespaces/seccomp\n\
                 \x20        mantém-se intacto."
            );
            return Ok(());
        }
        return Err(Error::Runtime {
            context: "cgroup",
            message: format!("não foi possível criar {cg}"),
        });
    }
    // device cgroup (eBPF): nega dispositivos de bloco (discos do host). Best-effort
    // (kernels sem BPF_CGROUP_DEVICE). Se falhar, avisa em vez de ignorar em silêncio:
    // a proteção primária mantém-se (sem CAP_MKNOD não se criam device nodes, e o
    // `bind_devices` recusa block devices), mas o operador deve saber que esta camada
    // não está activa.
    if !attach_device_filter(cg) {
        eprintln!(
            "delonix: aviso — device cgroup (eBPF) não aplicado em {}; block devices dependem só de caps/seccomp",
            c.name
        );
    }
    write_limit(cg, "memory.max", &c.memory_max)?; // teto de memória (kernel OOM-kill)
    // sem swap além da memória, senão o limite de memória seria contornável;
    // best-effort: o controlador de swap pode estar desligado no sistema.
    let _ = std::fs::write(format!("{cg}/memory.swap.max"), "0");
    write_limit(cg, "cpu.max", &cpu_max_value(&c.cpus))?; // teto de CPU
    write_limit(cg, "pids.max", DEFAULT_PIDS_MAX)?; // anti fork-bomb
    // --- escalonamento / QoS (cgroup v2, best-effort) ---
    if let Some(w) = &c.cpu_weight {
        let _ = std::fs::write(format!("{cg}/cpu.weight"), w); // prioridade de CPU
    }
    if let Some(set) = &c.cpuset {
        let _ = std::fs::write(format!("{cg}/cpuset.cpus"), set); // pinning de cores
    }
    if let Some(w) = &c.io_weight {
        let _ = std::fs::write(format!("{cg}/io.weight"), w); // prioridade de I/O
    }
    std::fs::write(format!("{cg}/cgroup.procs"), pid.to_string())?;
    Ok(())
}

/// A especificação de arranque de um container. Concentra as opções que
/// cresceram ao longo das fases: `detach` (1), `mounts`/volumes (4),
/// `new_netns`+`on_started` (3).
#[derive(Default)]
pub struct RunSpec<'a> {
    /// Corre em segundo plano (não espera o `waitpid`).
    pub detach: bool,
    /// Cria um *network namespace* próprio (`CLONE_NEWNET`).
    pub new_netns: bool,
    /// Em vez de criar um netns, junta-se a este (caminho p/ `/proc/<pid>/ns/net`)
    /// — usado pelos membros de um **pod** (partilham a rede do infra container).
    pub join_netns: Option<String>,
    /// Volumes/bind mounts a injectar no rootfs.
    pub mounts: Vec<Mount>,
    /// Ficheiro de log para o stdout/stderr (detached) — o *log driver* "file".
    pub log_path: Option<String>,
    /// Escreve cada linha no formato de log do CRI (`<rfc3339nano> stdout F <linha>`),
    /// para o `crictl`/kubelet conseguirem ler os logs. Default: formato cru.
    pub log_cri: bool,
    /// Cria um *user namespace* (`CLONE_NEWUSER`): o root do container deixa de
    /// ser o root do host. Requer que a camada de escrita esteja `chown`-ada
    /// para [`USERNS_UID_BASE`] (o `delonix-cli` trata disso).
    pub userns: bool,
    /// Perfil AppArmor a aplicar no `execve` (tem de estar carregado no host).
    pub apparmor: Option<String>,
    /// Contexto SELinux a aplicar no `execve` (só em hosts onde o SELinux é o LSM).
    pub selinux: Option<String>,
    /// *Hook* chamado com o PID após o arranque (a Fase 3 configura aí a rede).
    pub on_started: Option<&'a StartedHook<'a>>,
    /// Partilha o *PID namespace* do host (`--host-pid`; CRI `namespace_options.pid
    /// = NODE`): o container vê os processos do host. Por omissão, isolado.
    pub host_pid: bool,
    /// Partilha o *IPC namespace* do host (`--host-ipc`; CRI `namespace_options.ipc
    /// = NODE`): memória partilhada/filas do host. Por omissão, isolado.
    pub host_ipc: bool,
    /// **Ingress rootless:** o processo já corre DENTRO do user+network namespace
    /// do holder do ingress (re-exec via `nsenter … ip netns exec`). Não cria
    /// `CLONE_NEWUSER` nem `CLONE_NEWNET` (herda os do holder, já como uid 0), mas
    /// trata o rootfs como `userns` (é root no userns herdado). Ver `delonix-net::infra`.
    pub inherit_userns: bool,
    /// `USER` da imagem: uid/gid para os quais trocar ANTES do `exec` (Docker `User`).
    /// `None` ou `Some(0)` = corre como root (uid 0) — o comportamento histórico.
    /// `Some(uid != 0)` faz o runtime (a) mapear um intervalo de subuid via
    /// `newuidmap` em rootless (senão o uid não-zero não existe no userns), (b)
    /// `chown` o rootfs para esse uid/gid e (c) `setgid`/`setuid` antes do `execve`.
    /// Necessário para imagens que recusam root (ex.: Elasticsearch).
    pub run_uid: Option<u32>,
    pub run_gid: Option<u32>,
}

/// Cria e arranca um container (sem rede própria) — a assinatura da Fase 1.
pub fn create(store: &Store, container: &mut Container, rootfs: &str, detach: bool) -> Result<()> {
    spawn(
        store,
        container,
        rootfs,
        &RunSpec {
            detach,
            userns: container.userns, // honra o userns (necessário em rootless)
            ..Default::default()
        },
    )
}

/// Como [`create`], mas com *network namespace* próprio e um *hook* CNI.
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

/// O ponto de entrada geral (Fase 4): arranca um container segundo uma
/// [`RunSpec`] — combina volumes, rede e modo detached.
pub fn create_with(
    store: &Store,
    container: &mut Container,
    rootfs: &str,
    spec: &RunSpec<'_>,
) -> Result<()> {
    spawn(store, container, rootfs, spec)
}

fn spawn(store: &Store, container: &mut Container, rootfs: &str, spec: &RunSpec<'_>) -> Result<()> {
    let argv: Vec<CString> = container
        .command
        .iter()
        .map(|a| CString::new(a.as_str()).map_err(|_| Error::Invalid(format!("argumento inválido: {a:?}"))))
        .collect::<Result<_>>()?;
    if argv.is_empty() {
        return Err(Error::Invalid("comando vazio".into()));
    }

    let rootfs_owned = rootfs.to_string();
    let hostname = container.name.clone();
    let detach = spec.detach;
    let mounts = spec.mounts.clone();
    let apparmor = spec.apparmor.clone();
    let selinux = spec.selinux.clone();
    let join_netns = spec.join_netns.clone();
    let env = container.env.clone();
    let read_only = container.read_only;
    // --privileged: mantém TODAS as caps + seccomp unconfined + cgroupns + /sys RW
    // (ver setup_rootfs). Estritamente gated — o caminho não-privileged é idêntico.
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

    // Console (pty) para o PID 1: SÓ em `--privileged` detached com log (nodes
    // Kind, que correm systemd como PID 1). Dá um `/dev/console` real cujo output
    // — incluindo o estado do boot do systemd — é capturado no ficheiro de log, de
    // modo que `docker logs -f` (= o que o Kind usa para detetar readiness) o veja.
    // O caminho NÃO-privileged fica byte-a-byte idêntico (sem pty, pipe normal).
    let console = privileged && detach && spec.log_path.is_some();

    // Logging shim: em detached, o stdout/stderr do container vão por um pipe para
    // um processo `log_shim` que escreve em `log_path` COM rotação por tamanho. Em
    // modo console o "pipe" é antes o MASTER do pty (recebido do container), por
    // isso aqui não se cria pipe.
    let log_pipe: Option<(i32, i32)> = match (detach && !console, &spec.log_path) {
        (true, Some(_)) => {
            let mut fds = [0i32; 2];
            // SAFETY: pipe() preenche 2 fds.
            if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
                Some((fds[0], fds[1]))
            } else {
                None
            }
        }
        _ => None,
    };
    let log_fd = log_pipe.map(|(_, w)| w); // o container escreve na ponta de escrita

    // Socketpair do *console socket* (runc): o init aloca o pty no devpts do
    // container e devolve o master por aqui. `(pai, filho)`; o filho herda ambos na
    // clone e fecha o do pai (ver `setup_console`).
    let console_sock: Option<(i32, i32)> = if console {
        let mut sv = [0i32; 2];
        // SAFETY: socketpair() preenche 2 fds (AF_UNIX/SOCK_DGRAM, como no `exec`).
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sv.as_mut_ptr()) } == 0 {
            Some((sv[0], sv[1]))
        } else {
            None
        }
    } else {
        None
    };

    // Isolamento por omissão: mount, PID, UTS e **IPC** (System V/POSIX). O IPC
    // isolado impede um container de ver/alterar a memória partilhada e as filas
    // de mensagens do host (como o Docker). `--host-pid`/`--host-ipc` (e o
    // `namespace_options: NODE` do CRI) abdicam desse isolamento.
    let mut flags = CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUTS;
    if !spec.host_pid {
        flags |= CloneFlags::CLONE_NEWPID;
    }
    if !spec.host_ipc {
        flags |= CloneFlags::CLONE_NEWIPC;
    }
    // Ingress rootless: herdamos o netns + userns do holder (já lá estamos via
    // nsenter), por isso NÃO criamos os nossos. Só os de mount/pid/ipc/uts.
    if spec.new_netns && !spec.inherit_userns {
        flags |= CloneFlags::CLONE_NEWNET;
    }
    let userns = spec.userns && !spec.inherit_userns;
    if userns {
        flags |= CloneFlags::CLONE_NEWUSER;
    }
    // --privileged: cgroup namespace próprio (o systemd dentro do container vê o
    // seu cgroup como raiz e pode delegar sub-cgroups). NÃO o criamos aqui: o
    // `container_init` faz `unshare(CLONE_NEWCGROUP)` DEPOIS de se mover para um
    // leaf dedicado `dlx-<id>` (ver `setup_node_cgroup_ns`), para que a raiz do
    // cgroup-ns do node fique VAZIA (regra no-internal-processes que o kubelet
    // exige) — em vez de ancorar no cgroup-scope partilhado com o `kind`.
    // Pipe de sincronização: o filho espera o pai mapear os uid/gid (user ns).
    let sync = if userns {
        let mut fds = [0i32; 2];
        // SAFETY: pipe() preenche o array de 2 fds.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(Error::Runtime { context: "pipe", message: "falha a criar pipe".into() });
        }
        Some((fds[0], fds[1]))
    } else {
        None
    };

    let host_pid = spec.host_pid;
    let inherit_userns = spec.inherit_userns;
    let run_uid = spec.run_uid;
    let run_gid = spec.run_gid;
    // --secret-files: lê+decifra os valores AGORA (no host, antes do pivot do filho)
    // para os escrever num tmpfs in-namespace. Só nomes/raiz tocam fora da memória;
    // os valores são capturados (move) no closure do clone = memória do filho.
    let secret_files: Vec<(String, String)> =
        if container.secret_files && !container.secrets.is_empty() {
            match delonix_core::SecretStore::open(store.base()) {
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
    // CWD da imagem (OCI WorkingDir) — capturado p/ o filho fazer `chdir` antes do exec.
    let workdir = container.workdir.clone();
    // Id do container: usado (em privileged) p/ o leaf de cgroup dedicado do node.
    let cid = container.id.clone();
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
            join_netns.as_deref(),
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
        )
    });

    // SAFETY: single-threaded; o filho monta o container e faz `exec`.
    let pid = unsafe { clone(cb, &mut stack, flags, Some(Signal::SIGCHLD as i32)) }
        .map_err(syserr("clone"))?;

    // ORDEM CRÍTICA: o handshake do user namespace (libertar o filho com o byte
    // "GO") TEM de vir ANTES do recv do console. Em modo console o init só aloca o
    // pty e envia o master DEPOIS de receber o GO; se o pai bloqueasse no recv_fd
    // antes de escrever o GO, dava deadlock (pai espera o master, filho espera o
    // GO). Por isso: 1.º userns, 2.º console+log_shim.

    // User namespace: o pai mapeia os uid/gid e liberta o filho pelo pipe.
    if let Some((r, w)) = sync {
        // SAFETY: o pai fecha o read e usa o write para libertar o filho.
        unsafe {
            libc::close(r);
        }
        // Mapa de subuid (intervalo) por omissão em rootless quando há helpers
        // `newuidmap`/`newgidmap` + /etc/subuid: além de permitir o USER≠0 da imagem,
        // deixa os entrypoints que fazem `chown` para uids de serviço funcionarem —
        // ex.: o nginx faz chown das caches para o uid 101; com mapa de um só uid isso
        // dava `chown(...) failed (22: Invalid argument)` e o container saía. Sem os
        // helpers, mantém o mapa de um só uid (comportamento histórico). Não afecta
        // containers de ingress (herdam o userns do holder).
        let want_range = run_uid.map(|u| u != 0).unwrap_or(false) || have_subid_helpers();
        if let Err(e) = write_userns_maps(pid.as_raw(), want_range) {
            unsafe {
                libc::close(w);
            }
            let _ = kill(pid, Signal::SIGKILL);
            return Err(e);
        }
        // SAFETY: escreve 1 byte (o "podes avançar") e fecha o write.
        unsafe {
            let go = [1u8; 1];
            let _ = libc::write(w, go.as_ptr() as *const libc::c_void, 1);
            libc::close(w);
        }
    }

    // Origem do log: em modo console é o MASTER do pty (recebido do init por
    // SCM_RIGHTS, JÁ depois do GO do userns acima); caso contrário a ponta de
    // LEITURA do pipe de stdout/stderr.
    let log_src: Option<i32> = if let Some((csp, csc)) = console_sock {
        // SAFETY: o pai larga a ponta do filho e recebe o master do init.
        unsafe { libc::close(csc) };
        let m = recv_fd(csp);
        unsafe { libc::close(csp) };
        m // None se o init não conseguiu alocar o pty (cai sem log, sem bloquear)
    } else {
        log_pipe.map(|(r, _)| r)
    };

    // Arranca o logging shim (lê o pipe/master, escreve o log com rotação). Reparentado
    // ao init quando o `delonix run` termina; morre quando o container fecha a fonte.
    if let Some(src) = log_src {
        let lp = spec.log_path.clone().unwrap_or_default();
        let driver = container.log_driver.clone().unwrap_or_default();
        let tag = format!("delonix/{}", container.name);
        // SAFETY: fork de um processo single-threaded; o filho-shim só faz I/O e _exit.
        if let Ok(ForkResult::Child) = unsafe { fork() } {
            // Larga a ponta de ESCRITA do pipe (se existir) — só o container a mantém.
            if let Some((_, logw)) = log_pipe {
                unsafe { libc::close(logw) };
            }
            // O shim sobrevive ao `delonix run` (vive enquanto o container viver).
            // Tem de LARGAR o stdio herdado do pai — senão um chamador que capture
            // o stdout do `run -d` (o shim Docker, `$(...)`, CI/scripts) fica
            // bloqueado à espera de EOF até o container morrer. setsid + /dev/null
            // destacam-no por completo; o shim escreve no ficheiro de log próprio.
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
            log_shim(src, lp, MAX_LOG_BYTES, driver, tag, spec.log_cri); // não regressa (o pai não espera)
        }
        // O pai larga as pontas: só o container (a fonte, via fd 1/2 ou o slave do
        // pty) e o shim (src) as mantêm. Quando o container morre, o shim vê EOF/EIO.
        unsafe { libc::close(src) };
        if let Some((_, logw)) = log_pipe {
            unsafe { libc::close(logw) };
        }
    }

    container.pid = Some(pid.as_raw());
    container.pid_starttime = proc_starttime(pid.as_raw());
    container.status = Status::Running;
    setup_cgroup(container, pid.as_raw())?;
    store.save(container)?;

    // Configura a rede (ou outro arranque) ANTES de esperar/devolver.
    if let Some(hook) = spec.on_started {
        if let Err(e) = hook(pid.as_raw()) {
            let _ = kill(pid, Signal::SIGKILL);
            remove_cgroup(&container.cgroup());
            return Err(e);
        }
    }

    if detach {
        return Ok(());
    }

    let status = waitpid(pid, None).map_err(syserr("waitpid"))?;
    container.status = wait_to_status(status);
    container.pid = None;
    store.save(container)?;
    remove_cgroup(&container.cgroup());
    Ok(())
}

// ----------------------------------------------------------------------------
// Exec: correr um comando dentro de um container existente
// ----------------------------------------------------------------------------

/// Corre `argv` dentro dos namespaces do container, via `setns`.
///
/// Usa um **duplo fork**: o 1.º filho fica single-threaded (requisito do kernel
/// para `setns` ao *user namespace*); faz `setns` a todos os namespaces; e o
/// 2.º filho — criado depois de juntar o *pid namespace* — é quem realmente
/// entra nesse namespace (o `setns(PID)` só afecta filhos futuros).
/// Aloca um pseudo-terminal (master, slave) com o tamanho do terminal actual.
/// Usa `posix_openpt` (sem libutil). `None` se não for possível.
/// Aloca um pty a partir do devpts **do container** (`/dev/ptmx` → `pts/ptmx`).
/// Corre no neto (já dentro do mnt ns do container), por isso o `/dev/pts/N`
/// resultante resolve lá dentro — o `tty` imprime o nome certo, tal como o
/// Docker. Devolve `(master, slave, path_do_slave)`; o master é enviado ao pai
/// por SCM_RIGHTS e o `path` (`/dev/pts/N`) serve para o bind de `/dev/console`.
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
        let path = std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
        let s = libc::open(buf.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if s < 0 {
            libc::close(m);
            return None;
        }
        Some((m, s, path))
    }
}

/// Dá um `/dev/console` (pty) ao PID 1 de um container `--privileged` detached e
/// captura-o para o ficheiro de log. O `master` vai para o pai (que o liga ao
/// `log_shim`), o `slave` vira `/dev/console` + stdio — é onde o **systemd**,
/// como PID 1, escreve o estado do boot (ex.: "Reached target Multi-User
/// System"). Sem isto esse estado ia só para o *journal*, invisível ao
/// `docker logs -f` que o Kind usa para detetar o node pronto. Modelo *console
/// socket* do runc. Corre no init do container (já com o `/dev`/devpts montado e
/// ainda com caps), antes de largar privilégios e do `execve`.
fn setup_console(console_sock: (i32, i32)) {
    let (sp, sc) = console_sock;
    // SAFETY: o init não usa a ponta do pai do socketpair.
    unsafe { libc::close(sp) };
    let Some((m, s, path)) = open_pty_in_container() else {
        unsafe { libc::close(sc) };
        return;
    };
    // Entrega o master ao pai (que o pumpa para o log) e larga-o aqui.
    send_fd(sc, m);
    // SAFETY: master e socket já não são precisos dentro do container.
    unsafe {
        libc::close(m);
        libc::close(sc);
    }
    // `/dev/console` = bind do nó do slave (char device do pty). É o que o systemd
    // abre por nome para imprimir o estado do boot. Best-effort.
    let _ = std::fs::File::create("/dev/console"); // ponto de montagem
    let _ = mount(Some(path.as_str()), "/dev/console", None::<&str>, MsFlags::MS_BIND, None::<&str>);
    // Sessão nova + controlling tty no slave + stdio = slave (modelo runc
    // `terminal:true`). O PID 1 (filho da clone) não é líder de grupo → o setsid
    // sucede; o systemd herda isto e escreve para o pty capturado.
    // SAFETY: FFI directa sobre o slave válido; best-effort.
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

/// Envia um fd por um socket Unix (SCM_RIGHTS). O neto aloca o pty no devpts do
/// container e passa o `master` ao pai por aqui (modelo *console socket* do runc).
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
        // `as _`: o tipo dos campos cmsg difere entre glibc (size_t) e musl (socklen_t).
        msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
        std::ptr::copy_nonoverlapping(&fd, libc::CMSG_DATA(cmsg).cast::<libc::c_int>(), 1);
        libc::sendmsg(sock, &msg, 0) >= 0
    }
}

/// Recebe um fd enviado por SCM_RIGHTS (o pai recebe o master do pty do neto).
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

/// Copia bytes de um fd para outro até EOF (uma direcção do proxy do pty).
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
                libc::write(to, buf.as_ptr().offset(off) as *const libc::c_void, (n - off) as usize)
            };
            if w <= 0 {
                return;
            }
            off += w;
        }
    }
}

/// Põe o terminal do utilizador em modo raw (para a shell interactiva). Devolve
/// o estado anterior para restaurar. `None` se o stdin não for um terminal.
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
    // Guarda contra reutilização de PID: o `exec` entra nos namespaces via
    // setns(pid) — se o PID foi reciclado, entraríamos nos namespaces de um
    // processo alheio do host. Exigimos o mesmo `starttime`.
    let pid = container
        .pid
        .filter(|p| safe_to_signal(*p, container.pid_starttime))
        .ok_or_else(|| Error::NotRunning(container.short_id().to_string()))?;

    // Junta o user namespace PRIMEIRO (para ganhar caps nesse ns e poder juntar os
    // restantes); depois UTS, NET, PID e MNT (mnt por último). Inclui-se SEMPRE o
    // `user`: a lógica de skip-por-inode abaixo remove-o se já o partilhamos (container
    // host). Crucial para os containers do INGRESS rootless, que **herdam** o user ns
    // do holder (não criam o seu) e por isso tinham `container.userns=false` — sem o
    // setns(user) o setns(uts) dava EPERM (o UTS pertence a esse user ns).
    let ns_list: &[&str] = &["user", "uts", "net", "pid", "mnt"];
    // Abre os fds no PAI (resolvem-se no contexto do host); herdam-se pela fork.
    // Salta os namespaces que JÁ partilhamos (mesmo inode) — ex.: um container
    // com user ns mas sem rede partilha o `net` do host, e juntá-lo depois de
    // entrar no user ns daria EPERM (perdemos privilégio sobre o ns do host).
    use std::os::unix::fs::MetadataExt;
    let self_pid = std::process::id();
    let mut fds: Vec<(&str, i32)> = Vec::new();
    for ns in ns_list {
        let target = format!("/proc/{pid}/ns/{ns}");
        let mine = format!("/proc/{self_pid}/ns/{ns}");
        if let (Ok(a), Ok(b)) = (std::fs::metadata(&target), std::fs::metadata(&mine)) {
            if a.ino() == b.ino() {
                continue; // já estamos neste namespace
            }
        }
        let fd = open(target.as_str(), OFlag::O_RDONLY | OFlag::O_CLOEXEC, Mode::empty())
            .map_err(syserr("open ns"))?;
        fds.push((ns, fd));
    }
    // Entrámos mesmo num user ns? (i.e., não o partilhávamos já). Se sim, tornamo-nos
    // uid 0 dentro dele depois dos setns — quer o container o tenha CRIADO (`userns`)
    // quer o tenha HERDADO do holder do ingress.
    let joined_userns = fds.iter().any(|(n, _)| *n == "user");

    let cargv: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(a.as_str()).map_err(|_| Error::Invalid(format!("argumento inválido: {a:?}"))))
        .collect::<Result<_>>()?;

    // `exec -t`: o neto aloca um pty no devpts DO container e passa o master ao
    // PAI por um socketpair (SCM_RIGHTS). Assim o `/dev/pts/N` resolve dentro do
    // container (o `tty` imprime o nome) — modelo *console socket* do runc.
    let pty_sock: Option<(i32, i32)> = if tty {
        let mut sv = [0i32; 2];
        // SAFETY: socketpair com argumentos válidos; sv preenchido em sucesso.
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sv.as_mut_ptr()) } == 0 {
            Some((sv[0], sv[1]))
        } else {
            None
        }
    } else {
        None
    };

    // 1.º fork: o filho fica single-threaded.
    // SAFETY: o filho só faz syscalls simples e `_exit`.
    match unsafe { fork() }.map_err(syserr("fork"))? {
        ForkResult::Child => {
            for (ns, fd) in &fds {
                // SAFETY: fd válido herdado; `OwnedFd` fecha-o após o `setns`.
                let owned = unsafe { OwnedFd::from_raw_fd(*fd) };
                if let Err(e) = setns(owned, CloneFlags::empty()) {
                    eprintln!("delonix: setns({ns}) falhou: {e}");
                    unsafe { libc::_exit(125) };
                }
            }
            // Se juntámos um user ns (criado OU herdado), tornamo-nos uid 0 DENTRO
            // (igual ao init do container).
            // SAFETY: após setns(user) temos CAP_SETUID no user ns do container.
            if joined_userns {
                unsafe {
                    libc::setgid(0);
                    libc::setuid(0);
                }
            }
            // 2.º fork: entra no pid namespace já juntado.
            // SAFETY: o neto faz `exec` ou `_exit`.
            match unsafe { fork() } {
                Ok(ForkResult::Child) => {
                    let _ = chdir("/");
                    // pty: aloca no devpts do container, envia o master ao pai, e
                    // usa o slave como stdio (nova sessão + controlling terminal).
                    if let Some((sp, sc)) = pty_sock {
                        unsafe { libc::close(sp) }; // o neto só usa o seu lado
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
                    let exec_keep = resolve_cap_keep(&container.cap_drop, &container.cap_add);
                    drop_capabilities(exec_keep); // mesmo confinamento
                    let exec_unconf = container.seccomp.as_deref() == Some("unconfined");
                    apply_seccomp(exec_unconf, container.seccomp.as_deref() == Some("detect"));
                    // FAIL-CLOSED: o processo do `exec` tem de ficar tão confinado como
                    // o init do container; aborta se algum controlo falhou em silêncio.
                    if !insecure_besteffort() {
                        if let Err(e) = verify_confinement(!exec_unconf, exec_keep) {
                            eprintln!("delonix: confinamento do exec NÃO verificado ({e}); a abortar");
                            unsafe { libc::_exit(126) };
                        }
                    }
                    apply_env(&container.name, &container.env); // mesmo ambiente do container
                    if let Some(p) = &container.apparmor {
                        apply_apparmor(p); // mesmo confinamento MAC que o processo de init
                    }
                    let _ = execvp(&cargv[0], &cargv);
                    unsafe { libc::_exit(127) };
                }
                Ok(ForkResult::Parent { child }) => {
                    // o meio não segura o pty/socket (senão o master nunca dá EOF).
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
                unsafe { libc::close(sc) }; // o pai recebe pelo seu lado
                let master = recv_fd(sp);
                unsafe { libc::close(sp) };
                if let Some(m) = master {
                    // ajusta o pty ao tamanho do terminal do cliente.
                    unsafe {
                        let mut ws: libc::winsize = std::mem::zeroed();
                        if libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) == 0 {
                            libc::ioctl(m, libc::TIOCSWINSZ, &ws);
                        }
                    }
                    // o pai fala com o master: stdin→master e master→stdout, em modo raw.
                    let saved = set_raw_mode();
                    std::thread::spawn(move || pump_fd(m, 1)); // master -> stdout
                    std::thread::spawn(move || pump_fd(0, m)); // stdin -> master
                    let status = waitpid(child, None).map_err(syserr("waitpid"));
                    restore_mode(saved);
                    unsafe { libc::close(m) };
                    return Ok(wait_to_code(status?));
                }
                // o neto não conseguiu alocar o pty: comporta-se como não-tty.
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
// Hot-plug de volumes (zero-downtime): montar/desmontar num container A CORRER
// ----------------------------------------------------------------------------
//
// A nova mount API do kernel (open_tree/move_mount, Linux 5.2+; mount_setattr
// 5.12+) permite montar num container vivo sem o parar. O problema-chave é o
// modelo ROOTLESS: depois do `pivot_root` a fonte (caminho do host) já NÃO é
// visível dentro do mnt ns do container, e o userns do container não manda no
// mnt ns do host. Solução (funciona em rootless E root):
//   1. setns(user) → entra no userns do container (ganha CAP_SYS_ADMIN lá)
//   2. unshare(CLONE_NEWNS) → mnt ns novo, CÓPIA do do host (fonte visível),
//      mas POSSUÍDO pelo userns do container
//   3. open_tree(CLONE) → clona a subárvore-fonte num mount DESTACADO (fd)
//   4. setns(mnt) → entra no mnt ns REAL do container (raiz = raiz do container)
//   5. move_mount(fd, alvo) → anexa o mount; o mesmo userns possui origem e
//      destino, por isso o kernel autoriza
// Tudo no filho de uma fork (single-threaded, requisito do setns(user)).

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

/// `open_tree(AT_FDCWD, src, OPEN_TREE_CLONE [|AT_RECURSIVE])` → fd dum mount
/// destacado (cópia da subárvore que cobre `src`). Erro se o kernel não suportar.
fn open_tree_clone(src: &str, recursive: bool) -> nix::Result<OwnedFd> {
    let c = CString::new(src).map_err(|_| nix::errno::Errno::EINVAL)?;
    let mut flags = OPEN_TREE_CLONE | (OFlag::O_CLOEXEC.bits() as libc::c_uint);
    if recursive {
        flags |= libc::AT_RECURSIVE as libc::c_uint;
    }
    // SAFETY: syscall com caminho válido (CString terminado em NUL) e flags válidas.
    let fd = unsafe { libc::syscall(libc::SYS_open_tree, libc::AT_FDCWD, c.as_ptr(), flags) };
    if fd < 0 {
        return Err(nix::errno::Errno::last());
    }
    // SAFETY: fd >= 0 devolvido pelo kernel, com posse transferida ao OwnedFd.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// `move_mount(dfd, "", AT_FDCWD, target, MOVE_MOUNT_F_EMPTY_PATH)` → anexa o
/// mount destacado `dfd` em `target` (resolvido contra a raiz actual).
fn move_mount_to(dfd: RawFd, target: &str) -> nix::Result<()> {
    let empty = CString::new("").unwrap();
    let c = CString::new(target).map_err(|_| nix::errno::Errno::EINVAL)?;
    // SAFETY: dfd válido, caminhos NUL-terminados, flag válida.
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

/// `mount_setattr(dfd, "", AT_EMPTY_PATH|AT_RECURSIVE, &attr)` — fixa atributos
/// (nosuid/nodev sempre, rdonly opcional) no mount destacado antes de o anexar.
fn mount_setattr_fd(dfd: RawFd, attr_set: u64) -> nix::Result<()> {
    let empty = CString::new("").unwrap();
    let attr = MountAttr { attr_set, attr_clr: 0, propagation: 0, userns_fd: 0 };
    // SAFETY: dfd válido, struct do tamanho declarado, flags válidas.
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

/// Abre o fd dum namespace do container, saltando-o se JÁ o partilhamos (mesmo
/// inode) — juntá-lo depois daria EPERM. Devolve `Ok(None)` se já partilhado.
fn open_container_ns(pid: i32, ns: &str) -> Result<Option<OwnedFd>> {
    use std::os::unix::fs::MetadataExt;
    let target = format!("/proc/{pid}/ns/{ns}");
    let mine = format!("/proc/{}/ns/{ns}", std::process::id());
    if let (Ok(a), Ok(b)) = (std::fs::metadata(&target), std::fs::metadata(&mine)) {
        if a.ino() == b.ino() {
            return Ok(None);
        }
    }
    let fd = open(target.as_str(), OFlag::O_RDONLY | OFlag::O_CLOEXEC, Mode::empty())
        .map_err(syserr("open ns"))?;
    // SAFETY: fd >= 0 do kernel; posse transferida ao OwnedFd.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// Monta um bind-volume num container A CORRER, sem o parar (hot-plug). Ver o
/// comentário do módulo para a sequência setns/unshare/open_tree/move_mount.
pub fn mount_live(container: &Container, m: &Mount) -> Result<()> {
    if !mount_target_safe(&m.target) {
        return Err(Error::Invalid(format!("alvo de montagem inseguro: {}", m.target)));
    }
    let pid = container
        .pid
        .filter(|p| safe_to_signal(*p, container.pid_starttime))
        .ok_or_else(|| Error::NotRunning(container.short_id().to_string()))?;
    let src_is_dir = std::fs::metadata(&m.source)
        .map_err(|_| Error::Invalid(format!("fonte de montagem inexistente: {}", m.source)))?
        .is_dir();

    // fds dos namespaces (abertos no PAI, no contexto do host; herdados pela fork).
    let user_fd = if container.userns { open_container_ns(pid, "user")? } else { None };
    let mnt_fd = open_container_ns(pid, "mnt")?
        .ok_or_else(|| Error::Invalid("container partilha o mnt ns do host — nada a montar".into()))?;

    let mut attr = MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV;
    if m.readonly {
        attr |= MOUNT_ATTR_RDONLY;
    }
    let source = m.source.clone();
    let target = m.target.clone();

    // fork: o filho fica single-threaded (requisito do setns(user)).
    // SAFETY: o filho só faz syscalls simples e `_exit`, sem correr destrutores.
    match unsafe { fork() }.map_err(syserr("fork"))? {
        ForkResult::Child => {
            let fail = |code: i32, msg: &str| -> ! {
                eprintln!("delonix: mount_live: {msg}");
                unsafe { libc::_exit(code) }
            };
            // 1) entra no userns do container (ganha CAP_SYS_ADMIN lá).
            if let Some(u) = user_fd {
                if setns(u, CloneFlags::empty()).is_err() {
                    fail(125, "setns(user)");
                }
                // SAFETY: temos CAP_SETUID no userns do container.
                unsafe {
                    libc::setgid(0);
                    libc::setuid(0);
                }
            }
            // 2) mnt ns novo (cópia do do host) possuído pelo userns do container.
            if unshare(CloneFlags::CLONE_NEWNS).is_err() {
                fail(124, "unshare(NEWNS)");
            }
            // 3) clona a subárvore-fonte (visível: ainda vemos a árvore do host).
            let dfd = match open_tree_clone(&source, true) {
                Ok(f) => f,
                Err(e) => fail(123, &format!("open_tree: {e} (kernel suporta a nova mount API?)")),
            };
            if mount_setattr_fd(dfd.as_raw_fd(), attr).is_err() {
                fail(122, "mount_setattr");
            }
            // 4) entra no mnt ns REAL do container (a raiz passa a ser a do container).
            if setns(mnt_fd, CloneFlags::CLONE_NEWNS).is_err() {
                fail(121, "setns(mnt)");
            }
            // 5) cria o ponto de montagem (resolve contra a raiz = raiz do container).
            if src_is_dir {
                let _ = std::fs::create_dir_all(&target);
            } else {
                if let Some(p) = std::path::Path::new(&target).parent() {
                    let _ = std::fs::create_dir_all(p);
                }
                let _ = std::fs::File::create(&target);
            }
            // 6) anexa o mount destacado no alvo.
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
                    "falha a montar {} → {} no container vivo (código {code})",
                    m.source, m.target
                ))),
                _ => Err(Error::Invalid("montagem ao vivo interrompida".into())),
            }
        }
    }
}

/// Desmonta um bind-volume dum container A CORRER (hot-unplug). Entra no mnt ns
/// do container e faz `umount2(target, MNT_DETACH)` (lazy: não falha se ocupado).
pub fn unmount_live(container: &Container, target: &str) -> Result<()> {
    if !mount_target_safe(target) {
        return Err(Error::Invalid(format!("alvo de desmontagem inseguro: {target}")));
    }
    let pid = container
        .pid
        .filter(|p| safe_to_signal(*p, container.pid_starttime))
        .ok_or_else(|| Error::NotRunning(container.short_id().to_string()))?;
    let user_fd = if container.userns { open_container_ns(pid, "user")? } else { None };
    let mnt_fd = open_container_ns(pid, "mnt")?
        .ok_or_else(|| Error::Invalid("container partilha o mnt ns do host".into()))?;
    let target = target.to_string();

    // SAFETY: o filho só faz syscalls simples e `_exit`.
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
                _ => Err(Error::Invalid(format!("falha a desmontar {target} no container vivo"))),
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Prioridade de CPU (nice/renice) — QoS #6
// ----------------------------------------------------------------------------

/// Reúne os PIDs (host) de um container: primeiro via `cgroup.procs` (preciso);
/// se faltar (rootless sem delegação de cgroup), faz BFS pela árvore de processos
/// a partir do `pid` do init, lendo o `ppid` (campo 4 de `/proc/<pid>/stat`).
fn container_pids(container: &Container) -> Vec<i32> {
    if let Ok(procs) = std::fs::read_to_string(format!("{}/cgroup.procs", container.cgroup())) {
        let v: Vec<i32> = procs.lines().filter_map(|l| l.trim().parse().ok()).collect();
        if !v.is_empty() {
            return v;
        }
    }
    let Some(root) = container.pid else { return Vec::new() };
    // mapa ppid→[filhos] a partir de /proc, depois BFS desde o init.
    let mut children: std::collections::HashMap<i32, Vec<i32>> = std::collections::HashMap::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                // campo 4 (ppid) vem DEPOIS do `comm` entre parênteses — fatia após ')'.
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

/// Aplica uma prioridade de CPU (`nice`) a TODA a árvore de processos de um
/// container A CORRER (renice ao vivo). Best-effort: baixar prioridade (nice
/// positivo) funciona sem privilégio; subir (negativo) exige `CAP_SYS_NICE`/root,
/// por isso falhas individuais não abortam. Devolve `(aplicados, total)`.
pub fn set_priority(container: &Container, nice: i32) -> Result<(usize, usize)> {
    let nice = nice.clamp(-20, 19);
    let pid = container
        .pid
        .filter(|p| safe_to_signal(*p, container.pid_starttime))
        .ok_or_else(|| Error::NotRunning(container.short_id().to_string()))?;
    let _ = pid;
    let pids = container_pids(container);
    if pids.is_empty() {
        return Err(Error::Invalid("sem processos no container".into()));
    }
    let mut applied = 0usize;
    for p in &pids {
        // SAFETY: setpriority com PRIO_PROCESS e um pid válido; sem efeitos de memória.
        let r = unsafe { libc::setpriority(libc::PRIO_PROCESS, *p as libc::id_t, nice) };
        if r == 0 {
            applied += 1;
        }
    }
    Ok((applied, pids.len()))
}

// ----------------------------------------------------------------------------
// Ciclo de vida: stop / remove
// ----------------------------------------------------------------------------

/// Pára um container: `SIGTERM`, espera até `timeout_secs`, depois `SIGKILL`.
pub fn stop(store: &Store, container: &mut Container, timeout_secs: u64) -> Result<()> {
    let pid = container
        .pid
        .ok_or_else(|| Error::NotRunning(container.short_id().to_string()))?;
    let st = container.pid_starttime;
    // Protecção contra reutilização de PID: se o PID já não é o nosso processo
    // (kernel reciclou-o), NÃO enviamos sinais — só limpamos o estado.
    if !safe_to_signal(pid, st) {
        container.status = Status::Stopped;
        container.pid = None;
        store.save(container)?;
        remove_cgroup(&container.cgroup());
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
    // Paragem INTENCIONAL (pelo utilizador) → sempre Stopped, mesmo que tenha sido
    // preciso SIGKILL (não é um crash: foi um stop pedido).
    container.status = Status::Stopped;
    container.pid = None;
    store.save(container)?;
    remove_cgroup(&container.cgroup());
    Ok(())
}

/// Remove o cgroup de um container, esperando que esvazie. Após `SIGKILL` o
/// processo pode levar uns ms a ser ceifado pelo init → `rmdir` daria `EBUSY`;
/// por isso reentamos brevemente (evita fuga de cgroups vazios — robustez).
fn remove_cgroup(cgroup: &str) {
    for _ in 0..100 {
        if !std::path::Path::new(cgroup).exists() || std::fs::remove_dir(cgroup).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Suspende (`pause`) ou retoma (`unpause`) um container usando o *freezer* do
/// cgroup v2 (`cgroup.freeze`): `1` congela todos os processos, `0` retoma. Ao
/// contrário do `SIGSTOP`, é atómico para a árvore inteira e invisível ao
/// processo (não pode ser apanhado/ignorado).
pub fn set_frozen(container: &Container, frozen: bool) -> Result<()> {
    if !container.pid.map(|p| safe_to_signal(p, container.pid_starttime)).unwrap_or(false) {
        return Err(Error::NotRunning(container.short_id().to_string()));
    }
    let path = format!("{}/cgroup.freeze", container.cgroup());
    std::fs::write(&path, if frozen { "1" } else { "0" }).map_err(|e| Error::Runtime {
        context: "cgroup.freeze",
        message: format!("{path}: {e}"),
    })?;
    Ok(())
}

/// `true` se o container está congelado (`cgroup.freeze` == 1).
pub fn is_frozen(container: &Container) -> bool {
    std::fs::read_to_string(format!("{}/cgroup.freeze", container.cgroup()))
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// Reconcilia o `status` de um container contra a realidade do kernel (sem o
/// gravar — o chamador grava se devolver `true`). Centraliza a lógica dos 6
/// estados nas listagens (`ps`, API, Console):
///   * Running + pid morto  → **Crashed** (morte inesperada; um stop limpo já teria
///     posto Stopped). pid = None.
///   * Running + congelado   → **Paused** (freezer do cgroup ativo).
///   * Paused + descongelado → **Running** (retomado externamente).
///   * Paused + pid morto    → **Crashed**.
/// Estados terminais (Stopped/Failed/Crashed) e Created não são tocados.
pub fn reconcile_status(c: &mut Container) -> bool {
    // `safe_to_signal` (não `is_alive` cru) para fechar a janela de reutilização
    // de PID: se o init morreu e o kernel reciclou o PID para um processo alheio
    // do host, `is_alive` daria `true` e o container ficaria preso em Running a
    // apontar para um PID que não é o seu. O `starttime` registado desempata.
    match c.status {
        Status::Running => match c.pid {
            Some(pid) if !safe_to_signal(pid, c.pid_starttime) => {
                c.status = Status::Crashed;
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

/// Reescreve, AO VIVO, os limites de cgroup de um container (`docker update`).
/// Se o container estiver parado, não há cgroup — só o registo muda (na CLI), e
/// os novos limites aplicam-se no próximo `start`.
pub fn update_limits(container: &Container, memory: Option<&str>, cpus: Option<&str>) -> Result<()> {
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

/// Remove um container. Se estiver a correr, exige `force` (e mata-o).
pub fn remove(store: &Store, container: &Container, force: bool) -> Result<()> {
    if let Some(pid) = container.pid {
        if safe_to_signal(pid, container.pid_starttime) {
            if !force {
                return Err(Error::Invalid(format!(
                    "o container {} está a correr (usa --force)",
                    container.short_id()
                )));
            }
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
    remove_cgroup(&container.cgroup());
    store.remove(&container.id)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_target_rejects_traversal() {
        // seguros
        assert!(mount_target_safe("/data"));
        assert!(mount_target_safe("/var/lib/app/config"));
        // escape via `..` (montaria sobre o host antes do pivot_root)
        assert!(!mount_target_safe("/../../etc"));
        assert!(!mount_target_safe("/data/../../etc/shadow"));
        assert!(!mount_target_safe("/a/../b"));
        // relativo (resolve a partir do cwd do holder, não do rootfs)
        assert!(!mount_target_safe("etc/passwd"));
        assert!(!mount_target_safe(""));
    }

    #[test]
    fn confinement_ok_is_fail_closed() {
        let keep = (1u64 << 1) | (1u64 << 3); // só caps 1 e 3 na allowlist
        // estado bom: no_new_privs, seccomp modo filtro, caps ⊆ keep
        assert!(confinement_ok(Some(1), Some(2), Some(keep), Some(keep), true, keep).is_ok());
        // NO_NEW_PRIVS inativo → aborta
        assert!(confinement_ok(Some(0), Some(2), Some(keep), Some(keep), true, keep).is_err());
        // seccomp esperado mas não em modo filtro (falhou a aplicar) → aborta
        assert!(confinement_ok(Some(1), Some(0), Some(keep), Some(keep), true, keep).is_err());
        // cap fora da allowlist persiste no bounding set (capset/capbset falhou) → aborta
        let leaked = keep | (1u64 << 21); // + CAP_SYS_ADMIN
        assert!(confinement_ok(Some(1), Some(2), Some(leaked), Some(keep), true, keep).is_err());
        // …ou no effective
        assert!(confinement_ok(Some(1), Some(2), Some(keep), Some(leaked), true, keep).is_err());
        // unconfined (--security-opt seccomp=unconfined): modo 0 é aceite
        assert!(confinement_ok(Some(1), Some(0), Some(keep), Some(keep), false, keep).is_ok());
        // campos de caps ausentes = não-verificável = aborta (fail-closed)
        assert!(confinement_ok(Some(1), Some(2), None, Some(keep), true, keep).is_err());
        assert!(confinement_ok(Some(1), Some(2), Some(keep), None, true, keep).is_err());
        // privileged (keep = todas as caps): nada fica "fora" → ok
        let allcaps = u64::MAX;
        assert!(confinement_ok(Some(1), Some(2), Some(allcaps), Some(allcaps), true, allcaps).is_ok());
    }

    #[test]
    fn cpu_max_translates_cores_to_quota() {
        assert_eq!(cpu_max_value("0.5"), "50000 100000");
        assert_eq!(cpu_max_value("1.0"), "100000 100000");
        assert_eq!(cpu_max_value("2"), "200000 100000");
        // valores absurdos têm um piso (0.01 de um core)
        assert_eq!(cpu_max_value("0"), "1000 100000");
    }

    #[test]
    fn dangerous_caps_are_not_kept() {
        // SYS_ADMIN(21), SYS_MODULE(16), SYS_BOOT(22), MKNOD(27), SYS_RAWIO(17),
        // SYS_PTRACE(19), BPF(39) NÃO podem estar na allowlist.
        for dangerous in [21u8, 16, 22, 27, 17, 19, 39] {
            assert!(!KEPT_CAPS.contains(&dangerous), "cap {dangerous} não devia ser mantida");
        }
    }

    #[test]
    fn bpf_insn_encoding() {
        // EXIT (code 0x95, sem regs/off/imm).
        assert_eq!(bpf_insn(0x95, 0, 0, 0, 0), 0x95);
        // MOV r0 = 1 (imm nos 32 bits altos).
        assert_eq!(bpf_insn(0xb7, 0, 0, 0, 1), 0xb7 | (1u64 << 32));
        // LDX r2 = *(u32*)(r1+0): dst=2 (bits 8-11), src=1 (bits 12-15).
        assert_eq!(bpf_insn(0x61, 2, 1, 0, 0), 0x61 | (2 << 8) | (1 << 12));
    }

    #[test]
    fn seccomp_allowlist_excludes_dangerous_and_includes_common() {
        let allowed = allowed_syscalls();
        // perigosos: FORA da allowlist (= negados por omissão).
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
            assert!(!allowed.contains(&nr), "syscall {nr} perigoso NÃO devia estar na allowlist");
        }
        // comuns/essenciais: DENTRO da allowlist.
        for nr in [
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_openat,
            libc::SYS_mmap,
            libc::SYS_futex,
            libc::SYS_execve,
            libc::SYS_exit_group,
        ] {
            assert!(allowed.contains(&nr), "syscall {nr} essencial DEVIA estar na allowlist");
        }
    }

    #[test]
    fn parse_mem_satura_e_nao_zera_em_lixo() {
        assert_eq!(parse_mem_bytes("64M"), 64 * 1024 * 1024);
        assert_eq!(parse_mem_bytes("1G"), 1024 * 1024 * 1024);
        assert_eq!(parse_mem_bytes("512"), 512);
        // overflow → satura (não faz panic/wrap).
        assert_eq!(parse_mem_bytes("99999999999G"), u64::MAX);
        // lixo → u64::MAX (recusa na admissão), NUNCA 0 (que deixaria passar tudo).
        assert_eq!(parse_mem_bytes("64MB"), u64::MAX);
        assert_eq!(parse_mem_bytes("abc"), u64::MAX);
    }

    #[test]
    fn sysctl_allowlist_so_aceita_namespaced() {
        // namespaced (seguros) → permitidos.
        for k in ["net.ipv4.ip_forward", "kernel.shmmax", "kernel.sem", "fs.mqueue.msg_max"] {
            assert!(sysctl_namespaced(k), "{k} devia ser permitido");
        }
        // globais ao host / travessia → recusados.
        for k in ["kernel.hostname", "vm.swappiness", "kernel.core_pattern", "net/../kernel.x"] {
            assert!(!sysctl_namespaced(k), "{k} NÃO devia ser permitido");
        }
    }
}
