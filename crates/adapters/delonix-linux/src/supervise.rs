//! The detached supervisor behind `run -d`: the container's parent, so the only
//! process that can collect its real exit status, and the one that applies a
//! `--restart` policy.
//!
//! It `fork`s, and the fork assumes a SINGLE-THREADED caller (the CLI). A
//! multi-threaded caller (a server) must re-exec a fresh process first, as the
//! Docker API and the CRI do with `__apirun`.

use crate::{Error, Result, RunSpec};
use delonix_compute::Container;
use delonix_state::Store;
use std::path::Path;

/// What the supervisor needs from its caller.
pub struct Supervision<'a> {
    /// Where the event log lives (`die`, and `start` for each policy restart).
    pub state_root: &'a Path,
    /// Called once in the supervisor, after the first start was reported to the
    /// caller and stdio went to `/dev/null` — where a health monitor belongs.
    pub on_first_start: &'a dyn Fn(&Container),
    /// The error when the supervisor died before reporting why the start failed.
    pub silent_death: &'a str,
}

/// The operation a failed detached start is reported as.
const START_CONTEXT: &str = "container start";

/// What the supervisor sends the caller when the first start fails: the error's
/// MESSAGE, with its own operation in front when that is not a container start.
///
/// The caller reports it as one `container start` failure. Sending the error's
/// full text made the caller wrap it a second time — measured: «system call
/// `supervisor` failed: system call `container start` failed: nb: …». The class
/// of the error does not survive the pipe either way; every class that reaches
/// here exits 1.
fn handshake_reason(e: &Error) -> String {
    match e {
        Error::Syscall { context, message } if *context == START_CONTEXT => message.clone(),
        Error::Syscall { context, message } => format!("{context}: {message}"),
        other => other.to_string(),
    }
}

/// Whether a policy restart should go ahead, given the container's record as it
/// is NOW.
///
/// It stops when the record is gone (`rm -f`), when the user asked for `stop`,
/// and when the container is already live — a `container start` in the backoff
/// window brought up its own incarnation with its own supervisor, and restarting
/// here too would run the command twice in one container.
fn resume_restart(current: Option<&Container>) -> bool {
    match current {
        None => false,
        Some(cur) => !cur.stopped_by_user && !cur.is_live(),
    }
}

/// `--restart` with `-d`: creates the container inside a **detached supervisor**
/// (one per container, ephemeral — there's still no daemon) and enforces the
/// restart policy.
///
/// Why it has to be this way: `waitpid` is only allowed to the PARENT. In a
/// normal `run -d` the CLI creates the container and exits — it's reparented to
/// the host's `init` and the exit code dies there; `reconcile_status` can only
/// say "it died" (`Crashed`/137), never *why*, and `on-failure` would have no way
/// to decide. Here it's the supervisor that calls `create_with`, so it's the
/// parent: it catches the real code (`Failed(n)`) and restarts according to the
/// policy. It's the same role as podman's `conmon`, without a global resident process.
///
/// The parent (the CLI) waits for the first startup through a pipe, to keep the
/// `run -d` semantics: when the command returns, the container ALREADY exists.
pub fn run_supervised(
    store: &Store,
    c: &mut Container,
    rootfs: &str,
    spec: &RunSpec<'_>,
    policy: &str,
    sup: &Supervision<'_>,
) -> Result<()> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe() fills 2 fds; used only for the startup handshake.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(Error::Syscall {
            context: "pipe",
            message: "handshake do supervisor".into(),
        });
    }
    let (rd, wr) = (fds[0], fds[1]);
    // SAFETY: fork of a single-threaded process (CLI).
    if unsafe { libc::fork() } == 0 {
        // ---- supervisor ----
        // SAFETY: in the forked supervisor: `rd` is the read end of the handshake pipe created
        // above, which only the parent reads; our copy is closed once. `setsid` has no
        // preconditions.
        unsafe {
            libc::close(rd);
            libc::setsid(); // survives the terminal/CLI closing
        }
        let mut restarts: u32 = 0;
        let mut first = true;
        loop {
            let started = crate::create_with(store, c, rootfs, spec);
            // A restart the policy made is a start like any other, and `container
            // ls` counts starts from the event log. The loop used to record only
            // the `die` of each run, so `RESTARTS` said 0 for a container that had
            // restarted twice (measured: 3 runs of an `on-failure:2`).
            if restarts > 0 && started.is_ok() {
                delonix_node::events::emit(
                    sup.state_root,
                    "container",
                    "start",
                    &c.id,
                    &c.name,
                    Some("restart policy"),
                );
            }
            if first {
                // Handshake: 1 byte of status, and — when it failed — the REASON
                // right behind it.
                //
                // It used to be the byte alone, and the parent answered "the
                // container did not start (see the error above)" with nothing
                // above: `create_with`'s error was computed here and dropped on
                // the floor. Measured with `run -d --no-userns`, where the
                // foreground path says `clone failed: EPERM` and the detached one
                // said nothing at all. The comment below already asserted the
                // error "still has to reach the user" — it just had no code
                // making that true.
                //
                // Through the PIPE rather than an `eprintln!` here: this is a
                // forked child whose stderr is about to become /dev/null, and the
                // parent is the process whose error the caller is reading.
                let b = [u8::from(started.is_ok())];
                // SAFETY: writes the status byte, then the reason, then closes.
                unsafe {
                    libc::write(wr, b.as_ptr() as *const libc::c_void, 1);
                    if let Err(e) = &started {
                        let msg = handshake_reason(e);
                        let bytes = msg.as_bytes();
                        libc::write(wr, bytes.as_ptr() as *const libc::c_void, bytes.len());
                    }
                    libc::close(wr);
                    // Only NOW release stdio: until here a `create_with` error
                    // still has to reach the user.
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
                first = false;
                // The health monitor starts only after the handshake: before
                // it, an `eprintln!` from a failing probe would land on the
                // user's terminal interleaved with the real startup error.
                (sup.on_first_start)(c);
            }
            if started.is_err() {
                std::process::exit(1);
            }
            // We're the container's PARENT: this captures the REAL exit code and records it.
            let status = match crate::wait_and_record(store, c) {
                Ok(s) => s,
                Err(_) => std::process::exit(1),
            };
            // `die` with the REAL exit code — the supervisor is the only one that
            // knows it (and the container's parent); a normal `run -d` would only see "Crashed".
            delonix_node::events::emit(
                sup.state_root,
                "container",
                "die",
                &c.id,
                &c.name,
                Some(&format!("exit={}", status.exit_code())),
            );
            if !delonix_compute::launch::should_restart(policy, &status, restarts) {
                std::process::exit(0);
            }
            // Desired state trumps the policy: if the record disappeared (`rm -f`)
            // or the user asked for `stop`, don't resurrect — that's docker's semantics.
            if !resume_restart(store.load(&c.id).ok().as_ref()) {
                std::process::exit(0);
            }
            restarts += 1;
            // The previous incarnation's port frees itself on `stop`; if it's
            // still held, the restart's `publish_with_retry` clears it.
            // Capped exponential backoff (1s→32s), like docker: a container that
            // crash-loops can't burn the node.
            let backoff = std::cmp::min(1u64 << std::cmp::min(restarts, 5), 32);
            std::thread::sleep(std::time::Duration::from_secs(backoff));
            // Ask again after the wait. The desired state is most likely to change
            // DURING it — a crash-looping container spends nearly all its time
            // here — and asking only before made `stop` a no-op (measured: RESTARTS
            // kept climbing after the stop, because `create_with` saved this copy of
            // the record over the flag the stop had just written). A `start` in the
            // same window left TWO incarnations running, one of them surviving
            // `rm -f`.
            let current = store.load(&c.id).ok();
            if !resume_restart(current.as_ref()) {
                std::process::exit(0);
            }
            // Restart from the record as it is NOW, not from the copy this
            // supervisor has carried since the first start: `create_with` writes
            // the whole record, so a stale copy undid whatever changed during the
            // wait. Measured: a `rename` in the backoff window was reverted by the
            // next restart.
            if let Some(cur) = current {
                *c = cur;
            }
        }
    }

    // ---- parent (CLI): waits for the first startup ----
    // SAFETY: closes the write-end and reads the supervisor's handshake byte.
    unsafe { libc::close(wr) };
    let mut b = [0u8; 1];
    // SAFETY: reads 1 byte; 0 = EOF (supervisor died before signaling).
    let n = unsafe { libc::read(rd, b.as_mut_ptr() as *mut libc::c_void, 1) };
    if n != 1 || b[0] != 1 {
        // Drain the reason the supervisor sent behind the status byte. Empty
        // only when it died before it could say anything — and THAT is the one
        // case where there is genuinely nothing to report but the fact itself.
        let mut reason = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            // SAFETY: reads into our own buffer until EOF.
            let k = unsafe { libc::read(rd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if k <= 0 {
                break;
            }
            reason.extend_from_slice(&buf[..k as usize]);
        }
        // SAFETY: `rd` is the read end created above; the parent closes its copy once on this
        // path.
        unsafe { libc::close(rd) };
        let reason = String::from_utf8_lossy(&reason).trim().to_string();
        return Err(Error::Syscall {
            context: START_CONTEXT,
            message: if reason.is_empty() {
                sup.silent_death.to_string()
            } else {
                reason
            },
        });
    }
    // SAFETY: `rd` is the read end created above; the parent closes its copy once on this path.
    unsafe { libc::close(rd) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_failure_is_said_once() {
        let e = Error::Syscall {
            context: START_CONTEXT,
            message: "nb: the container's command did not start".into(),
        };
        assert_eq!(
            handshake_reason(&e),
            "nb: the container's command did not start"
        );
        let rebuilt = Error::Syscall {
            context: START_CONTEXT,
            message: handshake_reason(&e),
        };
        assert_eq!(rebuilt.to_string().matches("system call").count(), 1);
    }

    #[test]
    fn a_restart_goes_ahead_only_for_a_record_nobody_has_claimed() {
        let base = Container::new(
            "id1".into(),
            "c".into(),
            "alpine".into(),
            vec!["true".into()],
            "64M".into(),
        );
        assert!(
            resume_restart(Some(&base)),
            "a dead, unstopped container restarts"
        );
        assert!(!resume_restart(None), "a removed container stays removed");

        let mut stopped = base.clone();
        stopped.stopped_by_user = true;
        assert!(!resume_restart(Some(&stopped)), "a stop is honoured");

        // A `start` in the backoff window: the record now points at a live
        // process. This test process is one.
        let mut started = base.clone();
        started.pid = Some(std::process::id() as i32);
        started.pid_starttime = delonix_node::proc_starttime(std::process::id() as i32);
        assert!(
            !resume_restart(Some(&started)),
            "a live incarnation is not doubled"
        );
    }

    #[test]
    fn another_operation_keeps_its_name() {
        let e = Error::Syscall {
            context: "clone",
            message: "EPERM".into(),
        };
        assert_eq!(handshake_reason(&e), "clone: EPERM");
        assert_eq!(
            handshake_reason(&Error::EmptyCommand("bad".into())),
            Error::EmptyCommand("bad".into()).to_string()
        );
    }
}
