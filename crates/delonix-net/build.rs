//! Best-effort compile of the eBPF flow-accounting object.
//!
//! If `clang` + the libbpf headers are present, compile `bpf/delonix_flow.bpf.c`
//! into `$OUT_DIR/delonix_flow.bpf.o` and set `cfg(bpf_object)` so the loader
//! embeds it. If anything is missing (no clang, no headers — a minimal build
//! host), we simply skip: the eBPF observability datapath is optional and the
//! runtime degrades to the nft-only path. eBPF is NEVER required to build.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(bpf_object)");
    println!("cargo::rerun-if-changed=bpf/delonix_flow.bpf.c");

    let src = Path::new("bpf/delonix_flow.bpf.c");
    if !src.exists() {
        return;
    }
    let clang = match which("clang") {
        Some(c) => c,
        None => return,
    };
    let helpers = match find_bpf_include() {
        Some(i) => i,
        None => return,
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("delonix_flow.bpf.o");

    let mut cmd = Command::new(clang);
    cmd.args(["-O2", "-g", "-target", "bpf", "-D__TARGET_ARCH_x86"]);
    cmd.arg("-I").arg(&helpers);
    // asm/types.h lives in the arch include dir; harmless if absent.
    cmd.arg("-I/usr/include/x86_64-linux-gnu");
    cmd.arg("-c").arg(src).arg("-o").arg(&out);

    match cmd.status() {
        Ok(s) if s.success() && out.exists() => {
            println!("cargo::rustc-cfg=bpf_object");
            println!("cargo::rustc-env=DELONIX_BPF_OBJECT={}", out.display());
        }
        _ => {
            // Compilation failed — stay silent, ship without the datapath.
        }
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(bin))
        .find(|p| p.exists())
}

/// Locate a directory that contains `bpf/bpf_helpers.h`. Prefer libbpf-dev
/// (`/usr/include`), then the running kernel's bundled libbpf headers.
fn find_bpf_include() -> Option<PathBuf> {
    if Path::new("/usr/include/bpf/bpf_helpers.h").exists() {
        return Some(PathBuf::from("/usr/include"));
    }
    let rel = Command::new("uname").arg("-r").output().ok()?;
    let ver = String::from_utf8_lossy(&rel.stdout);
    let ver = ver.trim();
    let cand = PathBuf::from(format!(
        "/usr/src/linux-headers-{ver}/tools/bpf/resolve_btfids/libbpf/include"
    ));
    if cand.join("bpf/bpf_helpers.h").exists() {
        return Some(cand);
    }
    None
}
