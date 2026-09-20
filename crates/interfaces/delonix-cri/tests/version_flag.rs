//! `delonix-cri --version` prints the release and exits 0 without serving.

use std::process::Command;

#[test]
fn version_prints_and_exits_without_opening_a_socket() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("cri.sock");
    for flag in ["--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_delonix-cri"))
            .arg(flag)
            .env("DELONIX_CRI_ADDR", format!("unix://{}", sock.display()))
            .output()
            .unwrap();
        assert!(out.status.success(), "{flag}: {:?}", out.status);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("delonix-cri {}\n", env!("CARGO_PKG_VERSION"))
        );
        assert!(!sock.exists(), "{flag} must not bind the socket");
    }
}
