//! Reclaiming network infra whose state root is gone.
//!
//! # The failure this exists for
//!
//! [`crate::infra::teardown`] kills the pin, the control process and the slirp
//! **by pidfile**, and the pidfiles live in `$DELONIX_ROOT/ingress/`. That is
//! the right place for them — until the root is deleted, at which point the
//! only name those three processes ever had goes with it. They keep running,
//! holding a user namespace, a network namespace, a slirp and their swap, and
//! **no command in this engine can name them**.
//!
//! It is not hypothetical. Measured on the host this was written against:
//!
//! ```text
//! pins: 12   controls: 12   slirps: 14
//! 10 of 11 pin/control pairs had DELONIX_ROOT=/tmp/delonix-itest-<pid>-<n>, DELETED
//! VmSwap held by the set                                              107 MiB
//! the binary that started them (/tmp/tgt-int5/debug/delonix)          also deleted
//! ```
//!
//! Every integration-test run that creates a temporary root, uses it, and
//! removes the directory leaks one pin, one control and one slirp — for the
//! life of the machine.
//!
//! # Why this can identify its own processes and the slirp reaper could not
//!
//! `reap_orphan_slirp` had to work hard for its ownership token: `slirp4netns`
//! is somebody else's binary, Podman rootless runs it with the same argv shape,
//! and an earlier version that matched on `argv[0]` alone would have killed
//! another engine's networking. Its answer was the `--api-socket` path, the one
//! element of the argv this engine chooses.
//!
//! `delonix netns pin` has no such problem — it is this binary, running one of
//! its own internal subcommands. What still has to be earned is that the
//! process is **ours to reclaim** rather than another live root's, and that is
//! what [`classify_stray`] decides: only a root that is gone from disk counts,
//! never the current one and never another root that still exists.
//!
//! # Reporting is not removing
//!
//! [`find_strays`] only looks. Nothing here signals a process; the caller does,
//! after a confirmation. A sweep that killed processes it found by scanning
//! `/proc` without asking would be the shape of bug this module was written to
//! clean up after.

use crate::infra::PidKind;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What a stray process is: this engine's own network infra, started against a
/// state root that no longer exists.
///
/// The `kind` is [`PidKind`] — the SAME enum `teardown` matches on, not a
/// parallel one. Two tables describing the same three processes is how they
/// come to disagree about what a pin is, and the pre-split `netns holder`
/// spelling is exactly the sort of case only one of them would learn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stray {
    pub pid: i32,
    pub kind: PidKind,
    /// The `DELONIX_ROOT` it was started with — always `Some` for a pin or a
    /// control (that is how they were classified); `None` for a slirp, which is
    /// identified through the pin it serves.
    pub root: Option<PathBuf>,
    /// Command line, for the report. An operator about to kill eleven processes
    /// is owed the ability to check them.
    pub argv0: String,
}

/// **PURE** — should the process holding `proc_root` be reclaimed?
///
/// Four answers, and only one of them is yes:
///
/// * `None` — the environment could not be read, or the process predates the
///   variable being passed. **Not a stray.** An unreadable environment is not
///   evidence of abandonment, and this repo has a standing rule about not
///   folding *unknown* into a verdict.
/// * the current root — **never a stray**, whatever else is true. This is the
///   infra serving the containers running right now.
/// * a different root that still exists on disk — **not a stray.** Another
///   root's live infra is none of our business; two engines sharing a host is
///   supported and killing the other one is not a cleanup.
/// * a root that is gone — **stray.** Nothing can ever tear it down through the
///   normal path again, because the pidfiles went with the directory.
pub fn classify_stray(proc_root: Option<&Path>, current_root: &Path, root_exists: bool) -> bool {
    match proc_root {
        None => false,
        Some(r) if r == current_root => false,
        Some(_) => !root_exists,
    }
}

/// `DELONIX_ROOT` as the process at `pid` was started with it.
fn proc_root(pid: i32) -> Option<PathBuf> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    raw.split(|b| *b == 0)
        .filter_map(|kv| {
            let s = String::from_utf8_lossy(kv);
            s.strip_prefix("DELONIX_ROOT=").map(PathBuf::from)
        })
        .next()
}

/// The uid owning the process at `pid`, from `/proc/<pid>/status`.
///
/// Another user's pin is not ours to reclaim even when its root is gone: we
/// cannot signal it anyway, and listing it would put a process in a report that
/// the reported command could never act on.
fn proc_uid(pid: i32) -> Option<u32> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

/// **PURE** — the pid a `slirp4netns` argv targets: the second-to-last
/// positional, as in `slirp4netns … <target-pid> tap0`.
///
/// Parsed rather than assumed to be at a fixed index: the flags before it vary
/// with the options the caller passed, and an index would silently read a flag
/// value as a pid the day one is added.
pub fn slirp_target_pid(argv: &[String]) -> Option<i32> {
    let positional: Vec<&String> = argv
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    // `<pid> tap0` — the interface name is last, the pid before it.
    if positional.len() < 2 {
        return None;
    }
    positional[positional.len() - 2].parse().ok()
}

/// **PURE** — is this `--api-socket` path one that only THIS engine writes?
///
/// `runtime_dir()` builds `<tmp>/delonix-net-<uid>` for the default root and
/// `<tmp>/delonix-net-<uid>-<hash8>` for any other, and nothing else on a host
/// writes that shape. It is the same ownership token
/// [`crate::Slirp::is_ours`](crate) uses — with one deliberate difference.
///
/// That one compares against `infra::slirp_sock_path()`, the socket of the
/// **current** root, and it is right to: it runs from `publish_with_retry`, in
/// any process, where reaping another live root's slirp would take down an
/// unrelated engine's networking. It is exactly the check that was added after a
/// podman-shaped slirp was nearly reaped.
///
/// Here the question is different, and the difference is earned: this only runs
/// against processes whose state root has already been proven gone. So the
/// pattern is matched instead of one exact path — which is what catches the
/// slirp whose pin died before it, leaving it serving a namespace nobody holds.
/// Measured on this host: two such slirps, alive for 2h28m, invisible to every
/// existing reaper.
pub fn slirp_socket_is_ours(sock: &Path, uid: u32) -> bool {
    if sock.file_name().and_then(|f| f.to_str()) != Some("slirp.sock") {
        return false;
    }
    let Some(dir) = sock
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|d| d.to_str())
    else {
        return false;
    };
    let prefix = format!("delonix-net-{uid}");
    // Exactly the directory, or that directory plus the `-<hash8>` root suffix.
    // A `starts_with` alone would also accept `delonix-net-10001` for uid 1000.
    dir == prefix
        || dir
            .strip_prefix(&format!("{prefix}-"))
            .is_some_and(|h| h.len() == 8 && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// **PURE** — the `--api-socket` a slirp argv carries, in either spelling.
fn slirp_api_socket(argv: &[String]) -> Option<PathBuf> {
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if let Some(p) = a.strip_prefix("--api-socket=") {
            return Some(PathBuf::from(p));
        }
        if a == "--api-socket" {
            return it.next().map(PathBuf::from);
        }
    }
    None
}

/// Every abandoned pin, control and slirp on this machine, for this user.
///
/// `current_root` is exempt by [`classify_stray`]; pass the root the caller is
/// operating on, never a guess.
///
/// Best-effort throughout: a process that disappears mid-scan, an unreadable
/// `environ`, a `/proc` that will not open — each drops the candidate rather
/// than the sweep. A report that fails because one process exited while it was
/// being read is a report nobody can run on a busy host.
pub fn find_strays(current_root: &Path) -> Vec<Stray> {
    // SAFETY: geteuid() has no preconditions.
    let me = unsafe { libc::geteuid() };
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    // Whether a root exists is asked ONCE per distinct root, not once per
    // process: a pin and its control share a root, and on the measured host the
    // roots live in `/tmp`, where a stat is cheap but a stale answer between two
    // processes of the same pair would report half a pair.
    let mut root_exists: BTreeMap<PathBuf, bool> = BTreeMap::new();
    let mut out: Vec<Stray> = Vec::new();
    let mut candidates: Vec<(i32, Vec<String>)> = Vec::new();

    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if proc_uid(pid) != Some(me) {
            continue;
        }
        let Some(argv) = crate::infra::proc_argv(pid) else {
            continue;
        };
        candidates.push((pid, argv));
    }

    let mut stray_pins: Vec<i32> = Vec::new();
    for (pid, argv) in &candidates {
        let kind = match crate::infra::argv_kind(argv) {
            Some(k) => k,
            None => continue,
        };
        // A slirp is never classified on its own — it carries no root. It is
        // reclaimed only through the pin it serves, below.
        if kind == PidKind::Slirp {
            continue;
        }
        let root = proc_root(*pid);
        let exists = match &root {
            Some(r) => *root_exists.entry(r.clone()).or_insert_with(|| r.exists()),
            None => true,
        };
        if !classify_stray(root.as_deref(), current_root, exists) {
            continue;
        }
        if kind == PidKind::Pin {
            stray_pins.push(*pid);
        }
        out.push(Stray {
            pid: *pid,
            kind,
            root,
            argv0: argv.join(" "),
        });
    }

    // The slirps, by two independent tests — and BOTH are needed, which was
    // measured rather than assumed.
    //
    // 1. Its target is a pin we just condemned. The strongest token there is:
    //    this slirp was started to serve that namespace and says so in its own
    //    argv.
    // 2. Its target is already DEAD and its api-socket has the shape only
    //    `runtime_dir()` writes. Test 1 alone misses these entirely — on this
    //    host, two slirps had outlived their pins by 2h28m, holding a tap and a
    //    namespace for a process that no longer existed, and no reaper in the
    //    engine could see them.
    let live_sock = crate::infra::slirp_sock_path();
    for (pid, argv) in &candidates {
        if crate::infra::argv_kind(argv) != Some(PidKind::Slirp) {
            continue;
        }
        let target = slirp_target_pid(argv);
        let by_pin = target.is_some_and(|t| stray_pins.contains(&t));
        let by_orphan = {
            // `kill(pid, 0)` == 0 ⇒ the target exists; ESRCH ⇒ it is gone.
            // SAFETY: signal 0 sends nothing — it only tests for the pid.
            let dead = target.is_none_or(|t| unsafe { libc::kill(t, 0) } != 0);
            let sock = slirp_api_socket(argv);
            // Never the socket of the root we are running against, whatever else
            // is true — the same exemption `classify_stray` makes, for the same
            // reason.
            dead && sock
                .as_ref()
                .is_some_and(|s| *s != live_sock && slirp_socket_is_ours(s, me))
        };
        if by_pin || by_orphan {
            out.push(Stray {
                pid: *pid,
                kind: PidKind::Slirp,
                root: None,
                argv0: argv.join(" "),
            });
        }
    }

    out.sort_by_key(|s| s.pid);
    out
}

/// Signals `SIGTERM` to a stray, **re-checking its identity first**.
///
/// The re-check is not belt-and-braces: a scan and the kill that follows it are
/// separated by however long the operator took to answer the prompt, and a pid
/// freed in that window can be reused. `argv_kind` reading the same kind back is
/// what makes the number an identity again. Returns whether the signal was sent.
pub fn terminate(stray: &Stray) -> bool {
    let Some(argv) = crate::infra::proc_argv(stray.pid) else {
        return false;
    };
    if crate::infra::argv_kind(&argv) != Some(stray.kind) {
        return false;
    }
    // SAFETY: kill() with a pid whose identity was just re-confirmed.
    unsafe { libc::kill(stray.pid, libc::SIGTERM) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn so_uma_raiz_desaparecida_conta_como_orfa() {
        let atual = Path::new("/home/w/.local/share/delonix");
        let outra = Path::new("/tmp/delonix-itest-1-0");

        // A raiz actual NUNCA é órfã — nem que o `exists` chegue a dizer o contrário.
        assert!(!classify_stray(Some(atual), atual, true));
        assert!(!classify_stray(Some(atual), atual, false));

        // Outra raiz VIVA é de outro motor: não se toca.
        assert!(!classify_stray(Some(outra), atual, true));

        // Outra raiz APAGADA: é este o caso, e é o único.
        assert!(classify_stray(Some(outra), atual, false));

        // Ambiente ilegível não é prova de abandono.
        assert!(!classify_stray(None, atual, false));
    }

    /// O token de posse do slirp é o CAMINHO do api-socket, e a fronteira tem de
    /// ser exacta: um `starts_with` cru aceitaria `delonix-net-10001` para o uid
    /// 1000 — o slirp de outro utilizador, num host partilhado.
    #[test]
    fn o_socket_do_slirp_identifica_o_dono_sem_apanhar_o_uid_ao_lado() {
        let ours = |p: &str| slirp_socket_is_ours(Path::new(p), 1000);
        assert!(ours("/tmp/delonix-net-1000/slirp.sock"));
        assert!(ours("/tmp/delonix-net-1000-d13bf346/slirp.sock"));

        // uid vizinho — o prefixo bate como texto e NÃO como dono.
        assert!(!ours("/tmp/delonix-net-10001/slirp.sock"));
        assert!(!ours("/tmp/delonix-net-1000x/slirp.sock"));
        // sufixo que não é o hash de 8 hex que o `root_suffix` escreve.
        assert!(!ours("/tmp/delonix-net-1000-zz/slirp.sock"));
        assert!(!ours("/tmp/delonix-net-1000-d13bf3467/slirp.sock"));
        // outra ferramenta, outro socket.
        assert!(!ours("/run/user/1000/podman/slirp.sock"));
        assert!(!ours("/tmp/delonix-net-1000/control.sock"));
    }

    #[test]
    fn o_api_socket_le_se_nas_duas_grafias() {
        let junto: Vec<String> = ["slirp4netns", "--api-socket=/tmp/a/slirp.sock", "1", "tap0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            slirp_api_socket(&junto),
            Some(PathBuf::from("/tmp/a/slirp.sock"))
        );
        let separado: Vec<String> = [
            "slirp4netns",
            "--api-socket",
            "/tmp/a/slirp.sock",
            "1",
            "tap0",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            slirp_api_socket(&separado),
            Some(PathBuf::from("/tmp/a/slirp.sock"))
        );
        let sem: Vec<String> = ["slirp4netns", "1", "tap0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(slirp_api_socket(&sem), None);
    }

    /// O alvo é POSICIONAL e a sua posição depende das flags que vieram antes.
    /// Um índice fixo lia uma flag como pid no dia em que se acrescentasse uma.
    #[test]
    fn o_alvo_do_slirp_le_se_dos_posicionais_e_nao_de_um_indice() {
        let argv: Vec<String> = [
            "slirp4netns",
            "--configure",
            "--mtu=65520",
            "--disable-host-loopback",
            "--ready-fd=5",
            "--api-socket=/tmp/delonix-net-1000-4b321c71/slirp.sock",
            "716912",
            "tap0",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(slirp_target_pid(&argv), Some(716912));

        // Uma flag acrescentada à frente não desloca a resposta.
        let mut com_flag = argv.clone();
        com_flag.insert(1, "--enable-sandbox".into());
        assert_eq!(slirp_target_pid(&com_flag), Some(716912));

        // Sem posicionais suficientes não se inventa um pid.
        let curto: Vec<String> = ["slirp4netns", "--configure"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(slirp_target_pid(&curto), None);
    }
}
