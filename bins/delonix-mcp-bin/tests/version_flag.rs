//! `delonix-mcp --version` prints the release and exits 0 without serving.

use std::process::Command;

#[test]
fn version_prints_and_exits() {
    for flag in ["--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_delonix-mcp"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(out.status.success(), "{flag}: {:?}", out.status);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("delonix-mcp {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}
