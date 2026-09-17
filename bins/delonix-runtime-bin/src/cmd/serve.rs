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
            // Flags AND their environment variables: a `delonix-cri` from before
            // it took flags reads only the variables, and would otherwise ignore
            // `--addr` and serve on the default socket.
            let mut args = Vec::new();
            let mut env = Vec::new();
            for (flag, var, value) in [
                ("--addr", "DELONIX_CRI_ADDR", addr),
                ("--cap-ceiling", "DELONIX_CRI_CAP_CEILING", cap_ceiling),
                (
                    "--cap-ceiling-mode",
                    "DELONIX_CRI_CAP_CEILING_MODE",
                    cap_ceiling_mode,
                ),
            ] {
                if let Some(v) = value {
                    args.push(flag.to_string());
                    args.push(v.clone());
                    env.push((var, v));
                }
            }
            exec_server("delonix-cri", &args, &env, "install.sh --with-cri")
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

/// Runs a server's own executable in place of this process (ADR-0040 D2.4 as
/// amended): each server is its own binary, and `delonix` stays the one name a
/// user needs — `delonix serve cri` runs `delonix-cri`, as `git lfs` runs
/// `git-lfs`.
///
/// `exec` and not a child: the server takes this process's pid, so a unit, a
/// signal or a `kill` aimed at `delonix serve cri` reaches the server itself.
///
/// The sibling next to this executable wins over the `PATH`, and it is told the
/// version it must be: an older `delonix-cri` left in `~/.local/bin` would
/// otherwise serve a kubelet with a server from another release. Only a server
/// from this release on checks it; one from before cannot, which is why the
/// flags also travel as the environment variables it does read. The state root
/// is passed explicitly, because the server's own default (`/var/lib/delonix`)
/// is not this user's.
pub(crate) fn exec_server(
    name: &str,
    args: &[String],
    env: &[(&str, String)],
    install_hint: &str,
) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file());
    let program = beside.unwrap_or_else(|| std::path::PathBuf::from(name));
    let err = std::process::Command::new(&program)
        .args(args)
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .env("DELONIX_ROOT", super::util::state_root())
        // The CLI a server runs back: THIS executable, not whichever `delonix`
        // the `PATH` finds first.
        .envs(std::env::current_exe().ok().map(|p| ("DELONIX_BIN", p)))
        .env("DELONIX_DISPATCH_VERSION", env!("CARGO_PKG_VERSION"))
        .exec();
    if err.kind() == std::io::ErrorKind::NotFound {
        return Err(Error::Unavailable(super::po::tf(
            "'{name}' is not installed next to delonix nor on the PATH — install it with `{hint}`",
            &[("name", name), ("hint", install_hint)],
        )));
    }
    Err(Error::Runtime {
        context: "exec",
        message: format!("{}: {err}", program.display()),
    })
}
