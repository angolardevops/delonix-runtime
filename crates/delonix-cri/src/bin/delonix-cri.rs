//! `delonix-cri` — ponto de entrada standalone do servidor CRI. Corre dentro
//! do guest de uma VM (ex.: a imagem dourada `delonix image --vm build`,
//! `dist/delonix-cri.service`) e expõe um socket unix que o `kubelet` fala
//! via `--container-runtime-endpoint=unix:///run/delonix-cri.sock`.

use std::path::PathBuf;

fn main() {
    let base = std::env::var_os("DELONIX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/delonix"));
    let addr = std::env::var("DELONIX_CRI_ADDR").unwrap_or_else(|_| "unix:///run/delonix-cri.sock".to_string());

    if let Err(e) = delonix_cri::serve_blocking(base, &addr) {
        eprintln!("delonix-cri: {e}");
        std::process::exit(1);
    }
}
