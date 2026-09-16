//! `SO_PEERCRED` extraction for unix-socket peer authentication — the same
//! check every privileged local socket in this engine gates on (the holder's
//! control socket, `delonix-cri`, `delonix-mgmt`, the Docker-API shim).
//!
//! BUG FOUND (code review): this exact `unsafe`/`getsockopt` block was
//! copy-pasted verbatim into `delonix-cri`, `delonix-mgmt`,
//! `delonix-net::infra`, and `cmd/dockerapi.rs` — all four already depend on
//! this crate, so there was no crate-boundary reason for the duplication
//! (unlike e.g. `dir_size`, which really can't be shared without a circular
//! dependency). A future fix to this logic meant touching 4 sites and
//! trusting they'd stay in sync.

use std::os::unix::io::AsRawFd;

/// uid of the peer of a connected unix-domain-socket stream, via
/// `SO_PEERCRED`. `None` on failure (caller should treat that as "reject" —
/// see each caller's own accept loop).
pub fn peer_uid(stream: &impl AsRawFd) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt on SO_PEERCRED with a correctly-sized ucred buffer.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if r == 0 {
        Some(cred.uid)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn peer_uid_de_um_par_local_e_o_proprio_uid() {
        let (a, _b) = UnixStream::pair().unwrap();
        // SAFETY: geteuid() has no preconditions.
        let me = unsafe { libc::geteuid() };
        assert_eq!(peer_uid(&a), Some(me));
    }
}
