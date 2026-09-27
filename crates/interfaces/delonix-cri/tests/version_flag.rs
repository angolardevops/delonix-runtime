//! `delonix-cri --version` prints the release and exits 0 without serving.

use std::process::Command;

/// The binary under test, read at RUN time first.
///
/// Measured on the pinned toolchain (cargo 1.96.0, 2026-09-27): cargo sets
/// `CARGO_BIN_EXE_*` in the test process's environment but not at compile
/// time, so `env!` failed to compile this test under `cargo test` and under
/// `cargo check --all-targets` (the pre-commit gate) alike. The compile-time
/// value stays as the fallback for toolchains that set only that one; a test
/// that finds neither fails loudly instead of being skipped.
fn bin() -> String {
    std::env::var("CARGO_BIN_EXE_delonix-cri")
        .ok()
        .or_else(|| option_env!("CARGO_BIN_EXE_delonix-cri").map(str::to_string))
        .expect("cargo sets CARGO_BIN_EXE_delonix-cri for integration tests")
}

#[test]
fn version_prints_and_exits_without_opening_a_socket() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("cri.sock");
    for flag in ["--version", "-V"] {
        let out = Command::new(bin())
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
