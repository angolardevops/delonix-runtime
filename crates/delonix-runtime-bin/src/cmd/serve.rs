//! `delonix serve <endpoint>` — the three "serve a protocol on a unix socket"
//! background-service commands, grouped under one root instead of three
//! separate top-level commands (`cri`/`api`/`docker-api`) — they share the
//! exact same shape (long-lived listener, `--addr` override, an env-var
//! fallback, a conventional default path) and none of them is a workload verb
//! like `container`/`vm`, so they don't belong at the same level as those.

use clap::Subcommand;
use delonix_runtime_core::{Error, Result};

#[derive(Subcommand)]
pub enum ServeCmd {
    /// Serve the CRI endpoint (`runtime.v1`) on a unix socket — replaces containerd/CRI-O for a kubelet.
    Cri {
        /// Socket address (default: `$DELONIX_CRI_ADDR` or `unix:///run/delonix-cri.sock`).
        #[arg(long)]
        addr: Option<String>,
        /// Node-level UPPER BOUND on container capabilities, whatever the kubelet asks for: a comma-separated list of names (`NET_ADMIN`, `CAP_CHOWN`), or `all` (no bound, the default), `none`, `default` (the engine's default set), or `default,<extra>...`. Overrides `$DELONIX_CRI_CAP_CEILING`. Bounds capabilities ONLY — a privileged pod still gets unconfined seccomp.
        #[arg(long, value_name = "LIST")]
        cap_ceiling: Option<String>,
        /// What to do with a pod that asks for more than `--cap-ceiling`: `reject` (default, fails CreateContainer so the kubelet reports it) or `clamp` (reduce to the ceiling and log a warning). Overrides `$DELONIX_CRI_CAP_CEILING_MODE`.
        #[arg(long, value_name = "MODE")]
        cap_ceiling_mode: Option<String>,
    },
    /// Serve the MANAGEMENT API (HTTP+JSON) on a unix socket.
    ///
    /// The surface an external control-plane consumes to operate the engine.
    Api {
        /// Socket address (default: `$DELONIX_API_ADDR` or `unix:///run/delonix-mgmt.sock`).
        #[arg(long)]
        addr: Option<String>,
    },
    /// Serve a slice of the Docker Engine API on a unix socket.
    ///
    /// `docker version`/`ps`/`images`/`info`/lifecycle mutations via
    /// `DOCKER_HOST=unix://<path>`.
    DockerApi {
        /// Socket address (default: `$DELONIX_DOCKER_ADDR` or `unix:///run/delonix-docker.sock`).
        #[arg(long)]
        addr: Option<String>,
        /// Print the coverage matrix and exit, without serving anything.
        /// This layer is a SLICE of the Docker API, and third-party tooling
        /// deserves to know where it ends before it hits a 404 mid-run.
        #[arg(long)]
        matrix: bool,
    },
}

pub fn run(action: ServeCmd) -> Result<()> {
    match action {
        ServeCmd::Cri {
            addr,
            cap_ceiling,
            cap_ceiling_mode,
        } => {
            let addr = addr
                .or_else(|| std::env::var("DELONIX_CRI_ADDR").ok())
                .unwrap_or_else(|| "unix:///run/delonix-cri.sock".to_string());
            // Flag beats env var, same precedence as `--addr`. A malformed value
            // is refused BEFORE the socket is bound — a capability ceiling that
            // silently degraded to "unlimited" on a typo would be worse than
            // having none at all.
            let spec = cap_ceiling
                .or_else(|| std::env::var(delonix_cri::cap_ceiling::CEILING_ENV).ok())
                .unwrap_or_default();
            let mode = cap_ceiling_mode
                .or_else(|| std::env::var(delonix_cri::cap_ceiling::MODE_ENV).ok())
                .unwrap_or_default();
            let ceiling = delonix_cri::CapCeiling::parse(&spec, &mode).map_err(Error::Invalid)?;
            delonix_cri::serve_blocking(super::util::state_root(), &addr, ceiling)
        }
        ServeCmd::Api { addr } => {
            let addr = addr
                .or_else(|| std::env::var("DELONIX_API_ADDR").ok())
                .unwrap_or_else(|| "unix:///run/delonix-mgmt.sock".to_string());
            delonix_mgmt::serve_blocking(super::util::state_root(), &addr)
        }
        ServeCmd::DockerApi { addr, matrix } => {
            if matrix {
                super::dockerapi::print_matrix();
                return Ok(());
            }
            super::dockerapi::run(addr)
        }
    }
}
