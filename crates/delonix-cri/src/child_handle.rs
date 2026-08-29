//! A handle to a spawned child that cannot be confused with a recycled PID.
//!
//! # The defect this exists for
//!
//! `exec`/`attach` spawn a `delonix` child and hand the streaming loop its raw
//! `pid`. When the client goes away the loop kills that child, so an abandoned
//! interactive shell does not run forever.
//!
//! A raw PID is not a durable name for a process. While the child is a zombie
//! the slot is pinned and the number cannot be reassigned — but the wait-thread
//! reaps it (`child.wait()`), and reaping is precisely what frees the number for
//! reuse. After that, `kill(pid)` names whatever process the kernel has since
//! given that number to.
//!
//! The window is real in both streaming paths: the reap happens on a detached
//! thread and the exit code arrives afterwards through a channel, so the
//! killer's "has it exited?" check can still say no when the PID is already
//! gone. `pid_max` is 4194304 on a modern kernel, so wrapping takes a busy host
//! — but the signal is `SIGKILL` and the victim would be a workload of someone
//! else's, with nothing in any log tying it to us.
//!
//! # The fix
//!
//! `pidfd_open(2)` takes a file descriptor that refers to the *process*, not to
//! the number. `pidfd_send_signal(2)` through it either reaches that exact
//! process or fails with `ESRCH` — it can never reach a different one. This is
//! what pidfds exist for.
//!
//! Needs Linux 5.3 (`pidfd_send_signal` landed in 5.1, `pidfd_open` in 5.3). On
//! anything older `open` returns `None` and the caller falls back to the raw
//! `kill`, which is exactly today's behaviour — no worse, and better everywhere
//! the kernel allows. See `docs/adr/0027-pidfd-for-killing-exec-children.md`.

use std::os::fd::RawFd;

/// A process-stable reference to a child, for signalling it later.
///
/// Open it BEFORE anything can reap the child, and hold it for as long as the
/// right to signal is held. Dropping it closes the descriptor; it does not
/// signal or reap.
#[derive(Debug)]
pub struct ChildHandle {
    /// `None` when `pidfd_open` is unavailable or was refused: the caller then
    /// falls back to `kill(pid)` and inherits the recycling window.
    fd: Option<RawFd>,
    pid: i32,
}

impl ChildHandle {
    /// Takes a pidfd for `pid`.
    ///
    /// Call this while the child is still guaranteed to be the process meant —
    /// right after `spawn`, before handing the child to a thread that may reap
    /// it. Opening later is not an error but it is not a guarantee either: by
    /// then the number may already name someone else.
    pub fn open(pid: i32) -> Self {
        // SAFETY: `pidfd_open` reads no memory from us — it takes the pid and
        // the flags by value and returns a descriptor or -1. The pid is a
        // number; passing a stale one fails with ESRCH, it cannot corrupt
        // anything. `libc::syscall` is used because the `libc` crate exposes
        // `SYS_pidfd_open` but not a wrapper for it.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        let fd = if raw >= 0 { Some(raw as RawFd) } else { None };
        Self { fd, pid }
    }

    /// The pid, for logging and for the fallback path.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// True when this handle is backed by a pidfd, i.e. when `kill` cannot
    /// reach a recycled process. False means the kernel refused and the
    /// fallback carries the old window.
    pub fn is_process_stable(&self) -> bool {
        self.fd.is_some()
    }

    /// Sends `SIGKILL` to the child this handle was opened for.
    ///
    /// Best-effort by design: a child that already exited gives `ESRCH`, and
    /// that is the expected answer, not a failure worth reporting. Returns
    /// whether a signal was actually delivered — the tests use it to prove that
    /// a reaped child is NOT reachable through the handle.
    pub fn kill(&self) -> bool {
        match self.fd {
            // SAFETY: `pidfd_send_signal` takes the descriptor, the signal and
            // two by-value arguments; the null `siginfo_t` pointer is the
            // documented "let the kernel fill it in" form, and the flags are
            // required to be 0. Nothing of ours is read or written.
            Some(fd) => {
                let r = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd as libc::c_long,
                        libc::SIGKILL as libc::c_long,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                };
                r == 0
            }
            // SAFETY: `kill` takes two integers by value. This is the
            // pre-pidfd path and carries the recycling window described in the
            // module docs — it runs only where `pidfd_open` was refused.
            None => unsafe { libc::kill(self.pid, libc::SIGKILL) == 0 },
        }
    }
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        if let Some(fd) = self.fd.take() {
            // SAFETY: our own descriptor, taken out of the Option first so a
            // second drop cannot close a number the kernel has since reissued.
            unsafe { libc::close(fd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn sleeper() -> std::process::Child {
        Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sleep should be spawnable")
    }

    #[test]
    fn the_handle_kills_the_child_it_was_opened_for() {
        let mut child = sleeper();
        let h = ChildHandle::open(child.id() as i32);
        assert!(h.kill(), "a live child must be reachable");
        let st = child.wait().expect("the child should be reapable");
        assert!(!st.success(), "killed by SIGKILL, not a clean exit");
    }

    /// The whole point of the crate. A reaped pid is free for reuse; a pidfd
    /// opened before the reap keeps pointing at the process that ended, so the
    /// signal has nowhere to land instead of landing on a stranger.
    #[test]
    fn a_reaped_child_is_not_reachable_through_the_handle() {
        let mut child = sleeper();
        let h = ChildHandle::open(child.id() as i32);
        if !h.is_process_stable() {
            // Pre-5.3 kernel: the fallback has the window this test is about,
            // so asserting the property here would be asserting a lie. Reap
            // first — an early return that leaks a child is how a test suite
            // starts failing for reasons that have nothing to do with it.
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("skipped: no pidfd on this kernel");
            return;
        }
        assert!(h.kill());
        child.wait().unwrap(); // reaps — the pid number is now free
        assert!(
            !h.kill(),
            "after the reap the handle must refuse, never reach a recycled pid"
        );
    }

    #[test]
    fn a_handle_for_a_pid_that_never_existed_refuses_instead_of_guessing() {
        // pid 0 means "my own process group" to `kill(2)` — the one number
        // whose misreading would signal US. `pidfd_open` rejects it outright.
        let h = ChildHandle::open(0);
        assert!(!h.is_process_stable(), "pidfd_open(0) must not succeed");
    }

    #[test]
    fn the_handle_reports_the_pid_it_holds() {
        let mut child = sleeper();
        let pid = child.id() as i32;
        let h = ChildHandle::open(pid);
        assert_eq!(h.pid(), pid);
        drop(h);
        let _ = child.kill();
        let _ = child.wait();
    }
}
