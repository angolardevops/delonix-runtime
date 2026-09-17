//! `delonix-cri` — the CRI server's executable. The kubelet speaks to its unix
//! socket via `--container-runtime-endpoint=unix:///run/delonix-cri.sock`.
//!
//! People reach it as `delonix serve cri`, which runs THIS binary (ADR-0040
//! D2.4 as amended): one server, one executable, and `delonix` stays the only
//! name a user needs. A unit may also call it directly.
//!
//! Flags beat environment variables, which beat the defaults:
//! `--addr` / `DELONIX_CRI_ADDR`, `--cap-ceiling` / `DELONIX_CRI_CAP_CEILING`,
//! `--cap-ceiling-mode` / `DELONIX_CRI_CAP_CEILING_MODE`.

use std::path::PathBuf;

/// Set by `delonix` when it runs this binary: the version it expects. A
/// `delonix-cri` from another release left on the `PATH` would otherwise serve a
/// kubelet with a server the user never installed alongside their `delonix`.
const DISPATCH_VERSION_ENV: &str = "DELONIX_DISPATCH_VERSION";

fn fail(message: &str) -> ! {
    tracing::error!(error = %message, "delonix-cri: refused to start");
    eprintln!("delonix-cri: {message}");
    std::process::exit(2);
}

/// The value of `--name <v>` or `--name=<v>` in `args`, if given.
fn flag(args: &[String], name: &str) -> Option<String> {
    let long = format!("--{name}");
    let eq = format!("--{name}=");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if *a == long {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(&eq) {
            return Some(v.to_string());
        }
    }
    None
}

fn main() {
    delonix_telemetry::telemetry::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Ok(expected) = std::env::var(DISPATCH_VERSION_ENV) {
        let mine = env!("CARGO_PKG_VERSION");
        if expected != mine {
            fail(&format!(
                "this delonix-cri is version {mine} but delonix is {expected} — install the \
                 delonix-cri of the same release next to delonix"
            ));
        }
    }
    for a in &args {
        let name = a.split('=').next().unwrap_or(a);
        if a.starts_with("--") && !matches!(name, "--addr" | "--cap-ceiling" | "--cap-ceiling-mode")
        {
            fail(&format!("unknown argument: {a}"));
        }
    }
    let base = std::env::var_os("DELONIX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/delonix"));
    let addr = flag(&args, "addr")
        .or_else(|| std::env::var("DELONIX_CRI_ADDR").ok())
        .unwrap_or_else(|| "unix:///run/delonix-cri.sock".to_string());
    // Node capability ceiling. A malformed value REFUSES TO START, before the
    // socket is bound: this is a security bound, and a typo that fell back to
    // "unlimited" would be the exact silent failure it exists to prevent.
    let spec = flag(&args, "cap-ceiling")
        .or_else(|| std::env::var(delonix_cri::cap_ceiling::CEILING_ENV).ok())
        .unwrap_or_default();
    let mode = flag(&args, "cap-ceiling-mode")
        .or_else(|| std::env::var(delonix_cri::cap_ceiling::MODE_ENV).ok())
        .unwrap_or_default();
    let ceiling = match delonix_cri::CapCeiling::parse(&spec, &mode) {
        Ok(c) => c,
        Err(e) => fail(&e),
    };

    tracing::info!(%addr, root = %base.display(), "delonix-cri starting");
    if let Err(e) = delonix_cri::serve_blocking(base, &addr, ceiling) {
        tracing::error!(error = %e, "delonix-cri exited with error");
        eprintln!("delonix-cri: {e}");
        std::process::exit(1);
    }
}
