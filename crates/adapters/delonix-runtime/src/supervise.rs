//! The detached supervisor behind `run -d`: the container's parent, so the only
//! process that can collect its real exit status, and the one that applies a
//! `--restart` policy.
//!
//! It `fork`s, and the fork assumes a SINGLE-THREADED caller (the CLI). A
//! multi-threaded caller (a server) must re-exec a fresh process first, as the
//! Docker API and the CRI do with `__apirun`.

use crate::RunSpec;
use delonix_runtime_core::{Container, Error, Result, Store};
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
        return Err(Error::Runtime {
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
                delonix_runtime_core::events::emit(
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
                        let msg = e.to_string();
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
            delonix_runtime_core::events::emit(
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
            match store.load(&c.id) {
                Err(_) => std::process::exit(0),
                Ok(cur) if cur.stopped_by_user => std::process::exit(0),
                Ok(_) => {}
            }
            restarts += 1;
            // The previous incarnation's port frees itself on `stop`; if it's
            // still held, the restart's `publish_with_retry` clears it.
            // Capped exponential backoff (1s→32s), like docker: a container that
            // crash-loops can't burn the node.
            let backoff = std::cmp::min(1u64 << std::cmp::min(restarts, 5), 32);
            std::thread::sleep(std::time::Duration::from_secs(backoff));
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
        return Err(Error::Runtime {
            context: "supervisor",
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
