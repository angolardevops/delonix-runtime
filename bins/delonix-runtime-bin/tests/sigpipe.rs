//! SIGPIPE: a closed stdout ends the CLI quietly, a closed SOCKET does not.
//!
//! `main` used to put SIGPIPE back to SIG_DFL for `delonix image ls | head`,
//! and that killed the process on every EPIPE — including the one a registry
//! causes by closing the connection in the middle of a `vm image push` upload. The
//! push then died with 141 and not one word: no retry, no error line (seen
//! 2026-09-28 against ghcr.io). These run the real binary, because the test
//! harness runs with SIGPIPE ignored and would never see the signal.

use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

fn delonix() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_delonix"));
    cmd.env("NO_COLOR", "1");
    cmd
}

/// A registry that opens every upload session and, on a blob upload (the
/// whole-blob PUT, or a chunk's PATCH), reads `cut` bytes of the body and
/// closes. What a registry that gives up on a slow upload does, seen from
/// the client.
fn serve_closing_registry(cut: usize) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let hend = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..hend]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let is_blob_put = (head.starts_with("put ") || head.starts_with("patch "))
                    && head.contains("/uploads/");
                let want = if is_blob_put && len > cut { cut } else { len };
                let mut got = buf.len() - hend - 4;
                while got < want {
                    match s.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got += n,
                    }
                }
                if is_blob_put && len > cut {
                    return; // dropped: closes with the rest of the body unread
                }
                let reply = if head.starts_with("head ") {
                    "404 Not Found\r\n".to_string()
                } else if head.starts_with("post ") {
                    format!("202 Accepted\r\nlocation: /v2/t/vm/blobs/uploads/u{port}\r\n")
                } else {
                    "201 Created\r\n".to_string()
                };
                let _ = s.write_all(
                    format!("HTTP/1.1 {reply}content-length: 0\r\nconnection: close\r\n\r\n")
                        .as_bytes(),
                );
            });
        }
    });
    port
}

#[test]
fn a_push_whose_connection_is_closed_says_why() {
    let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("sigpipe-push-{}", std::process::id()));
    let images = root.join("vm-images");
    std::fs::create_dir_all(&images).unwrap();
    // Well past what the loopback socket buffers absorb, so the client is
    // still writing when the registry closes — the write that used to raise
    // the signal.
    std::fs::write(images.join("big.qcow2"), vec![3u8; 32 * 1024 * 1024]).unwrap();
    std::fs::write(
        images.join("big.json"),
        r#"{"name":"big","tag":"big","digest":"x","size":33554432,
           "ubuntu_release":null,"k8s_version":null,"created_unix":0}"#,
    )
    .unwrap();
    let port = serve_closing_registry(64 * 1024);

    let out = delonix()
        .env("DELONIX_ROOT", &root)
        .args(["vm", "image", "push", "big"])
        .arg(format!("127.0.0.1:{port}/t/vm:1"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        out.status.signal(),
        None,
        "killed by signal {:?} with nothing said — stderr:\n{stderr}",
        out.status.signal()
    );
    assert_ne!(out.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("upload gave up after"), "{stderr}");
    assert!(stderr.contains("connection lost with"), "{stderr}");
}

#[test]
fn a_closed_stdout_still_ends_quietly() {
    // A pipe whose read end is already closed: the first write to stdout is
    // the EPIPE, deterministically — no race with a reader that has not yet
    // gone away.
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors `pipe` writes.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: fds[0] was just created and is not used again.
    unsafe { libc::close(fds[0]) };
    // SAFETY: fds[1] was just created and is owned by nothing else.
    let write_end = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) };

    let out = delonix()
        .arg("--help")
        .stdout(Stdio::from(write_end))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.signal(), Some(libc::SIGPIPE), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}
