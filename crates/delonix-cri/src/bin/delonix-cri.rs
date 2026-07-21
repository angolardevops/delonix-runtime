//! `delonix-cri` — standalone entry point for the CRI server. Runs inside a
//! VM guest (e.g. the golden image `delonix image --vm build`,
//! `dist/delonix-cri.service`) and exposes a unix socket that the `kubelet`
//! speaks to via `--container-runtime-endpoint=unix:///run/delonix-cri.sock`.

use std::path::PathBuf;

fn main() {
    delonix_runtime_core::telemetry::init();
    let base = std::env::var_os("DELONIX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/delonix"));
    let addr = std::env::var("DELONIX_CRI_ADDR")
        .unwrap_or_else(|_| "unix:///run/delonix-cri.sock".to_string());

    tracing::info!(%addr, root = %base.display(), "delonix-cri starting");
    if let Err(e) = delonix_cri::serve_blocking(base, &addr) {
        tracing::error!(error = %e, "delonix-cri exited with error");
        std::process::exit(1);
    }
}
