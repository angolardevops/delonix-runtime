//! Socket activation of the real `delonix-node-api` binary (ADR-0040 P5).
//!
//! The test plays the service manager: it binds the listening socket itself,
//! hands it to the binary as descriptor 3, and sets `LISTEN_FDS=1` and
//! `LISTEN_PID` to the binary's own pid — what `systemd` does for a `.socket`
//! unit with `Accept=no`. `LISTEN_PID` is only known after the fork, so a
//! shell sets it to `$$` and `exec`s the binary, which keeps that pid.
//!
//! The socket lives under `/tmp` (`tempdir_in`): a `sun_path` has 108 bytes,
//! and a session's `TMPDIR` can already be past 90.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_delonix-node-api");

/// Starts the binary with `fd` as its descriptor 3 and the two variables set
/// for its own pid.
fn activate(fd: RawFd, root: &std::path::Path, args: &[&str]) -> Child {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(r#"export LISTEN_PID=$$ LISTEN_FDS=1; exec "$0" "$@""#)
        .arg(BIN)
        .args(args)
        .env("DELONIX_ROOT", root)
        .env_remove("DELONIX_NODE_API_ADDR")
        .env_remove("DELONIX_NODE_API_IDLE_TIMEOUT")
        .env_remove("DELONIX_DISPATCH_VERSION")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: only async-signal-safe calls (dup2, fcntl) between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if fd != 3 && libc::dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // dup2 clears close-on-exec; a descriptor that already was 3 keeps it.
            let flags = libc::fcntl(3, libc::F_GETFD);
            if flags < 0 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().expect("spawning the node API")
}

/// One `GET` over HTTP/1.1, answered or not within 10 s.
fn get(sock: &std::path::Path, path: &str) -> String {
    let mut s = UnixStream::connect(sock).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

fn wait_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(st) = child.try_wait().unwrap() {
            return Some(st);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn stderr_of(child: &mut Child) -> String {
    let mut e = String::new();
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut e);
    }
    e
}

#[test]
fn an_inherited_listening_socket_is_served() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let root = tempfile::tempdir().unwrap();
    let sock = dir.path().join("n.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    // `0`: never exit on its own, so only the answer is measured here.
    let mut child = activate(listener.as_raw_fd(), root.path(), &["--idle-timeout", "0"]);
    let answer = get(&sock, "/v1");
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        answer.starts_with("HTTP/1.1 200"),
        "no answer on the inherited socket: {answer:?}\nstderr: {}",
        stderr_of(&mut child)
    );
    assert!(answer.contains("delonix.node.v1"), "{answer}");
}

#[test]
fn the_server_exits_when_idle_and_not_while_a_connection_is_open() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let root = tempfile::tempdir().unwrap();
    let sock = dir.path().join("n.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let mut child = activate(
        listener.as_raw_fd(),
        root.path(),
        &["--idle-timeout", "700ms"],
    );
    assert!(get(&sock, "/v1").starts_with("HTTP/1.1 200"));

    // An open connection — a client between two requests — holds it up for
    // well past the timeout.
    let held = UnixStream::connect(&sock).unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    if let Some(st) = child.try_wait().unwrap() {
        panic!(
            "exited with a connection open ({st}): {}",
            stderr_of(&mut child)
        );
    }
    drop(held);

    // With nothing open it exits on its own, cleanly: systemd reads a 0 as
    // «stopped», keeps the socket and starts it again on the next connection.
    let started = Instant::now();
    let Some(st) = wait_exit(&mut child, Duration::from_secs(10)) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("still running 10 s after the last connection closed");
    };
    assert!(st.success(), "{st}: {}", stderr_of(&mut child));
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "exited before the timeout: {:?}",
        started.elapsed()
    );

    // The listening socket was never the server's to close: a connection now
    // is queued for the next activation, as with systemd holding it.
    let mut queued = UnixStream::connect(&sock).expect("the socket outlives the server");
    queued.write_all(b"x").unwrap();
}

#[test]
fn a_descriptor_that_is_not_a_listening_socket_is_refused_by_number() {
    let root = tempfile::tempdir().unwrap();
    let file = std::fs::File::create(root.path().join("not-a-socket")).unwrap();
    let mut child = activate(file.as_raw_fd(), root.path(), &[]);
    let st = wait_exit(&mut child, Duration::from_secs(10)).expect("it should refuse, not serve");
    let err = stderr_of(&mut child);
    assert_eq!(st.code(), Some(1), "{err}");
    assert!(err.contains("DX-1903"), "{err}");
}

#[test]
fn an_explicit_addr_with_an_inherited_socket_is_refused() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let root = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("n.sock")).unwrap();
    let other = dir.path().join("other.sock");
    let mut child = activate(
        listener.as_raw_fd(),
        root.path(),
        &["--addr", other.to_str().unwrap()],
    );
    let st = wait_exit(&mut child, Duration::from_secs(10)).expect("it should refuse, not serve");
    let err = stderr_of(&mut child);
    assert_eq!(st.code(), Some(1), "{err}");
    assert!(err.contains("DX-1904"), "{err}");
    assert!(!other.exists(), "it bound --addr anyway");
}
