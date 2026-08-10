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
    // Node capability ceiling (`DELONIX_CRI_CAP_CEILING`). A malformed value
    // REFUSES TO START: this is a security bound, and a typo that fell back to
    // "unlimited" would be the exact silent failure it exists to prevent.
    let ceiling = match delonix_cri::CapCeiling::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "delonix-cri: invalid capability ceiling");
            eprintln!("delonix-cri: {e}");
            std::process::exit(2);
        }
    };

    tracing::info!(%addr, root = %base.display(), "delonix-cri starting");
    if let Err(e) = delonix_cri::serve_blocking(base, &addr, ceiling) {
        tracing::error!(error = %e, "delonix-cri exited with error");
        std::process::exit(1);
    }
}
