//! `delonix serve <endpoint>` — the three "serve a protocol on a unix socket"
//! background-service commands, grouped under one root instead of three
//! separate top-level commands (`cri`/`api`/`docker-api`) — they share the
//! exact same shape (long-lived listener, `--addr` override, an env-var
//! fallback, a conventional default path) and none of them is a workload verb
//! like `container`/`vm`, so they don't belong at the same level as those.

use clap::Subcommand;
use delonix_runtime_core::Result;

#[derive(Subcommand)]
pub enum ServeCmd {
    /// Serve the CRI endpoint (`runtime.v1`) on a unix socket — replaces containerd/CRI-O for a kubelet.
    Cri {
        /// Socket address (default: `$DELONIX_CRI_ADDR` or `unix:///run/delonix-cri.sock`).
        #[arg(long)]
        addr: Option<String>,
    },
    /// Serve the MANAGEMENT API (HTTP+JSON) on a unix socket — the surface an external control-plane consumes to operate the engine.
    Api {
        /// Socket address (default: `$DELONIX_API_ADDR` or `unix:///run/delonix-mgmt.sock`).
        #[arg(long)]
        addr: Option<String>,
    },
    /// Serve a slice of the Docker Engine API on a unix socket — `docker version`/`ps`/`images`/`info`/lifecycle mutations via `DOCKER_HOST=unix://<path>`.
    DockerApi {
        /// Socket address (default: `$DELONIX_DOCKER_ADDR` or `unix:///run/delonix-docker.sock`).
        #[arg(long)]
        addr: Option<String>,
    },
}

pub fn run(action: ServeCmd) -> Result<()> {
    match action {
        ServeCmd::Cri { addr } => {
            let addr = addr
                .or_else(|| std::env::var("DELONIX_CRI_ADDR").ok())
                .unwrap_or_else(|| "unix:///run/delonix-cri.sock".to_string());
            delonix_cri::serve_blocking(super::util::state_root(), &addr)
        }
        ServeCmd::Api { addr } => {
            let addr = addr
                .or_else(|| std::env::var("DELONIX_API_ADDR").ok())
                .unwrap_or_else(|| "unix:///run/delonix-mgmt.sock".to_string());
            delonix_mgmt::serve_blocking(super::util::state_root(), &addr)
        }
        ServeCmd::DockerApi { addr } => super::dockerapi::run(addr),
    }
}
