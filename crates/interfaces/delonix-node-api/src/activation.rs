//! Socket activation and the idle exit (ADR-0040 P5, ADR-0041 D5).
//!
//! The node API is not a daemon (guardrail #1). A systemd socket unit holds
//! the listening socket; the first connection starts `delonix-node-api`, which
//! receives the socket through `LISTEN_FDS` and exits once it has been idle
//! for `--idle-timeout`. The listening socket stays systemd's: a connection
//! that arrives while the server is exiting waits in the socket's queue and
//! starts it again, so an idle exit loses no request.
//!
//! - [`inherited_fds`] reads the two variables with `sd_listen_fds(3)`
//!   semantics, without libsystemd: `LISTEN_PID` must be this process (a child
//!   that inherits the variables reads them as not its own), the descriptors
//!   start at 3.
//! - [`check_listening_unix_stream`] refuses a descriptor that is not a
//!   listening `AF_UNIX` stream socket (DX-1903) before anything is served on
//!   it: a wrong `ListenStream=` (a TCP port, a datagram socket) would
//!   otherwise be served without the `SO_PEERCRED` check meaning anything.
//! - The variables are NOT unset: writing the process environment of a
//!   multi-threaded server is unsound, and the `LISTEN_PID` check already makes
//!   them inert for any child. The descriptor itself is marked close-on-exec,
//!   so no child the server runs holds the socket.
//!
//! The idle exit ([`IdleExit`]) is decided by the accept loop in `lib.rs`: no
//! connection open (an open stream — `WatchEvents`, `WatchOperation` — is an
//! open connection), nothing accepted or closed for the timeout, and no
//! operation of THIS process still `RUNNING` in the ledger
//! ([`crate::operations::running_owned_by`]).

use std::os::fd::RawFd;
use std::path::PathBuf;
use std::time::Duration;

use delonix_model::Error;

/// `SD_LISTEN_FDS_START`: the first descriptor systemd passes.
pub const LISTEN_FDS_START: RawFd = 3;

/// The idle timeout a socket-activated server uses when none is given: the
/// unit `--install-units` writes passes its own, so this only applies to a
/// unit written by hand.
pub const DEFAULT_ACTIVATED_IDLE: Duration = Duration::from_secs(300);

/// When the server exits on its own.
#[derive(Debug, Clone)]
pub struct IdleExit {
    /// How long the server stays with no connection before it exits.
    pub timeout: Duration,
    /// The state root whose operation ledger is read before exiting.
    pub root: PathBuf,
}

fn invalid_socket(message: String) -> Error {
    Error::coded(1903, Error::Invalid(message))
}

/// The descriptors systemd passed to THIS process, from the two variables'
/// values and this process's pid. `None` when nothing was passed — no
/// `LISTEN_PID`, a `LISTEN_PID` that names another process, or
/// `LISTEN_FDS=0`.
///
/// `LISTEN_PID` that names this process with a `LISTEN_FDS` that does not read
/// as a count is an error (DX-1903), not «nothing passed»: the manager said it
/// passed something, and serving on a default address instead would hide a
/// broken unit.
pub fn inherited_fds(
    listen_pid: Option<&str>,
    listen_fds: Option<&str>,
    my_pid: u32,
) -> Result<Option<Vec<RawFd>>, Error> {
    let Some(pid) = listen_pid else {
        return Ok(None);
    };
    match pid.trim().parse::<u32>() {
        Ok(p) if p == my_pid => {}
        // Another process's variables, inherited: not ours to read.
        _ => return Ok(None),
    }
    let count: i32 = match listen_fds.map(str::trim) {
        Some(n) => n.parse().map_err(|_| {
            invalid_socket(format!(
                "LISTEN_FDS is '{n}', not a count of descriptors (LISTEN_PID names this process)"
            ))
        })?,
        None => {
            return Err(invalid_socket(
                "LISTEN_PID names this process but LISTEN_FDS is not set".into(),
            ))
        }
    };
    if count < 0 {
        return Err(invalid_socket(format!(
            "LISTEN_FDS is {count}, not a count of descriptors"
        )));
    }
    if count == 0 {
        return Ok(None);
    }
    Ok(Some((LISTEN_FDS_START..LISTEN_FDS_START + count).collect()))
}

/// The one listening socket systemd passed, read from this process's
/// environment. More than one is refused: the unit is `Accept=no` with one
/// `ListenStream=`, and choosing one of several would serve on a socket
/// nobody asked for.
pub fn from_env() -> Result<Option<RawFd>, Error> {
    let pid = std::env::var("LISTEN_PID").ok();
    let fds = std::env::var("LISTEN_FDS").ok();
    match inherited_fds(pid.as_deref(), fds.as_deref(), std::process::id())? {
        None => Ok(None),
        Some(v) if v.len() == 1 => Ok(Some(v[0])),
        Some(v) => Err(invalid_socket(format!(
            "systemd passed {} sockets; the node API serves exactly one \
             (one ListenStream= in its .socket unit)",
            v.len()
        ))),
    }
}

fn sockopt_int(fd: RawFd, opt: libc::c_int) -> Result<libc::c_int, std::io::Error> {
    let mut v: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `v` and `len` are valid for writes of the sizes passed; an
    // invalid `fd` makes the call fail with EBADF/ENOTSOCK, nothing else.
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            opt,
            (&mut v as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if r == 0 {
        Ok(v)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Refuses `fd` unless it is a listening `AF_UNIX` stream socket (DX-1903).
pub fn check_listening_unix_stream(fd: RawFd) -> Result<(), Error> {
    let what = |e: std::io::Error| {
        invalid_socket(format!(
            "descriptor {fd} from the service manager is not a usable socket: {e}"
        ))
    };
    let domain = sockopt_int(fd, libc::SO_DOMAIN).map_err(what)?;
    let kind = sockopt_int(fd, libc::SO_TYPE).map_err(what)?;
    let listening = sockopt_int(fd, libc::SO_ACCEPTCONN).map_err(what)?;
    if domain != libc::AF_UNIX {
        return Err(invalid_socket(format!(
            "descriptor {fd} is not a unix socket (family {domain}): the node API serves on a \
             local unix socket only — the .socket unit needs ListenStream=<absolute path>"
        )));
    }
    if kind != libc::SOCK_STREAM {
        return Err(invalid_socket(format!(
            "descriptor {fd} is not a stream socket (type {kind}): the .socket unit needs \
             ListenStream=, not ListenDatagram= or ListenSequentialPacket="
        )));
    }
    if listening == 0 {
        return Err(invalid_socket(format!(
            "descriptor {fd} is not listening: the .socket unit needs Accept=no, which hands \
             over the listening socket, not one accepted connection"
        )));
    }
    Ok(())
}

/// Marks `fd` close-on-exec, so no process the server runs inherits the
/// listening socket.
pub fn set_cloexec(fd: RawFd) -> Result<(), Error> {
    let failed = || {
        invalid_socket(format!(
            "descriptor {fd}: {}",
            std::io::Error::last_os_error()
        ))
    };
    // SAFETY: F_GETFD on a descriptor has no memory effects; an invalid
    // descriptor makes the call fail.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(failed());
    }
    // SAFETY: as above, F_SETFD only changes the descriptor's flags.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(failed());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn nothing_passed_is_none() {
        assert_eq!(inherited_fds(None, None, 10).unwrap(), None);
        assert_eq!(inherited_fds(None, Some("1"), 10).unwrap(), None);
        assert_eq!(inherited_fds(Some("10"), Some("0"), 10).unwrap(), None);
    }

    #[test]
    fn variables_of_another_process_are_not_read() {
        // A child of the server inherits both variables; its pid differs.
        assert_eq!(inherited_fds(Some("10"), Some("1"), 11).unwrap(), None);
        assert_eq!(inherited_fds(Some("x"), Some("1"), 11).unwrap(), None);
    }

    #[test]
    fn the_descriptors_start_at_three() {
        assert_eq!(
            inherited_fds(Some("10"), Some("1"), 10).unwrap(),
            Some(vec![3])
        );
        assert_eq!(
            inherited_fds(Some(" 10 "), Some("3"), 10).unwrap(),
            Some(vec![3, 4, 5])
        );
    }

    #[test]
    fn a_count_that_does_not_read_is_refused_not_ignored() {
        for fds in [Some("one"), Some("-1"), None] {
            let e = inherited_fds(Some("10"), fds, 10).unwrap_err();
            assert_eq!(e.number(), 1903, "{fds:?}: {e}");
        }
    }

    #[test]
    fn a_listening_unix_stream_socket_passes() {
        let dir = tempfile::tempdir().unwrap();
        let l = std::os::unix::net::UnixListener::bind(dir.path().join("s")).unwrap();
        check_listening_unix_stream(l.as_raw_fd()).unwrap();
    }

    #[test]
    fn what_is_not_a_listening_unix_stream_socket_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file.
        let f = std::fs::File::create(dir.path().join("f")).unwrap();
        assert_eq!(
            check_listening_unix_stream(f.as_raw_fd())
                .unwrap_err()
                .number(),
            1903
        );
        // A connected (not listening) unix stream socket.
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let e = check_listening_unix_stream(a.as_raw_fd()).unwrap_err();
        assert!(e.to_string().contains("not listening"), "{e}");
        // A unix datagram socket.
        let d = std::os::unix::net::UnixDatagram::unbound().unwrap();
        let e = check_listening_unix_stream(d.as_raw_fd()).unwrap_err();
        assert!(e.to_string().contains("not a stream socket"), "{e}");
        // A TCP listener.
        let t = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let e = check_listening_unix_stream(t.as_raw_fd()).unwrap_err();
        assert!(e.to_string().contains("not a unix socket"), "{e}");
    }
}
