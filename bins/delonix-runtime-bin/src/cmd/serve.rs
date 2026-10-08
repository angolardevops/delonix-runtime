//! `delonix serve <endpoint>` — the three "serve a protocol on a unix socket"
//! background-service commands, grouped under one root instead of three
//! separate top-level commands (`cri`/`api`/`docker-api`) — they share the
//! exact same shape (long-lived listener, `--addr` override, an env-var
//! fallback, a conventional default path) and none of them is a workload verb
//! like `container`/`vm`, so they don't belong at the same level as those.

use clap::Subcommand;
use delonix_model::{Error, Result};

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
    /// Serve the NODE API (gRPC + HTTP/JSON of `delonix.node.v1`) on a unix socket.
    ///
    /// The versioned node contract (ADR-0040). Today it serves `GetApiRoot`
    /// (`GET /v1`, a link to everything served), `GetNodeInfo`, `GetHealth`,
    /// `GetCapacity`, `ListProviders` and the network reads (`GetNetwork`,
    /// `ListNetworks`), and `GET /openapi.json`,
    /// rendered at `GET /docs` (Swagger UI) and `GET /redoc` from UI files embedded
    /// in the binary; `WatchEvents` answers UNIMPLEMENTED with the step that brings
    /// it.
    NodeApi {
        /// Socket address (default: `$DELONIX_NODE_API_ADDR` or `unix:///run/delonix-node.sock`).
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
            // The flag also travels as the variable, for the same reason as the CRI.
            let args: Vec<String> = addr
                .iter()
                .flat_map(|a| ["--addr".into(), a.clone()])
                .collect();
            let env: Vec<(&str, String)> =
                addr.into_iter().map(|a| ("DELONIX_API_ADDR", a)).collect();
            exec_server("delonix-mgmt", &args, &env, "install.sh")
        }
        ServeCmd::NodeApi { addr } => {
            let args: Vec<String> = addr
                .iter()
                .flat_map(|a| ["--addr".into(), a.clone()])
                .collect();
            let env: Vec<(&str, String)> = addr
                .into_iter()
                .map(|a| ("DELONIX_NODE_API_ADDR", a))
                .collect();
            exec_server("delonix-node-api", &args, &env, "install.sh")
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
/// Restores the `PATH` fallback that ADR-0075 D1 took away (its D2).
const FROM_PATH_ENV: &str = "DELONIX_SERVER_FROM_PATH";

/// Which file a `delonix serve` is about to run.
///
/// Kept apart from the `exec` so the decision can be tested: `exec_server`
/// replaces the process, so nothing downstream of it is reachable from a test.
#[derive(Debug, PartialEq)]
enum Chosen {
    /// The sibling next to this executable — the only case D1 allows by default.
    Beside(std::path::PathBuf),
    /// The `PATH` copy, taken because D2's opt-in is set. `None` when the `PATH`
    /// carries none either, which leaves the `exec` to fail with its own message.
    FromPath(Option<std::path::PathBuf>),
    /// D1's refusal, carrying the directory it looked in so the message can say.
    Refused(Option<std::path::PathBuf>),
}

/// Where a search path would find a server. `conditions::which` answers yes or
/// no; a refusal and a warning both have to NAME the file they mean.
///
/// The path is an ARGUMENT, not read here: a test that had to `set_var("PATH")`
/// entered `arch_fitness`'s `env_writes` debt (measured: 23 to 26, and the gate
/// refused the push). A process-wide write is also a race against every other
/// thread, for a function that needs no state of its own.
fn on_path_in(search: &std::ffi::OsStr, name: &str) -> Option<std::path::PathBuf> {
    std::env::split_paths(search)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// `on_path_in` against this process's `PATH`.
fn on_path(name: &str) -> Option<std::path::PathBuf> {
    on_path_in(&std::env::var_os("PATH").unwrap_or_default(), name)
}

/// ADR-0075 D1/D2, as a pure decision.
///
/// A sibling that is not beside this executable belongs to another installation
/// BY CONSTRUCTION: `scripts/install.sh` puts every binary in one `BIN_DIR`, so
/// the only way the `PATH` answers where the directory did not is that the two
/// come from different installs. Taking that silently is what served a
/// 2026-09-07 `delonix-cri` to a `delonix` of 2026-10-08 — and the version
/// refusal that should have caught it runs in the CALLEE
/// (`dispatch::check_version`), which a server that old is too old to carry.
fn choose(
    name: &str,
    beside_dir: Option<&std::path::Path>,
    from_path: bool,
    lookup: impl Fn(&str) -> Option<std::path::PathBuf>,
) -> Chosen {
    if let Some(p) = beside_dir.map(|d| d.join(name)).filter(|p| p.is_file()) {
        return Chosen::Beside(p);
    }
    if from_path {
        return Chosen::FromPath(lookup(name));
    }
    Chosen::Refused(beside_dir.map(|d| d.to_path_buf()))
}

pub(crate) fn exec_server(
    name: &str,
    args: &[String],
    env: &[(&str, String)],
    install_hint: &str,
) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let beside_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.to_path_buf()));
    let opted_in = std::env::var(FROM_PATH_ENV).ok().as_deref() == Some("1");
    let program = match choose(name, beside_dir.as_deref(), opted_in, on_path) {
        Chosen::Beside(p) => p,
        Chosen::FromPath(found) => {
            // Allowed, and never in silence: the whole point of the opt-in is
            // that the user knows which file is about to serve.
            if let Some(p) = &found {
                eprintln!(
                    "{}",
                    super::po::tf(
                        "warning: serving with '{path}', which is not next to this delonix ({var}=1)",
                        &[
                            ("path", &p.display().to_string()),
                            ("var", FROM_PATH_ENV),
                        ],
                    )
                );
            }
            found.unwrap_or_else(|| std::path::PathBuf::from(name))
        }
        Chosen::Refused(dir) => {
            let looked = dir
                .map(|d| d.display().to_string())
                .unwrap_or_else(|| "?".to_string());
            return Err(Error::Unavailable(super::po::tf(
                "'{name}' is not next to delonix in {dir}, and a copy found on the PATH would be from \
                 another install — install it with `{hint}`, or set {var}=1 to serve with it anyway",
                &[
                    ("name", name),
                    ("dir", &looked),
                    ("hint", install_hint),
                    ("var", FROM_PATH_ENV),
                ],
            )));
        }
    };
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

#[cfg(test)]
mod adr_0075 {
    //! The decision of ADR-0075 D1/D2. The `exec` itself is not testable — it
    //! replaces the process — so the decision was extracted to be.
    use super::{choose, Chosen};
    use std::path::{Path, PathBuf};

    fn nothing_on_path(_: &str) -> Option<PathBuf> {
        None
    }

    #[test]
    fn the_sibling_beside_the_executable_wins() {
        let dir = tempfile::tempdir().unwrap();
        let sib = dir.path().join("delonix-cri");
        std::fs::write(&sib, "").unwrap();
        // Even with the opt-in set, and even with the PATH offering another one:
        // beside always wins, so the opt-in can never downgrade a good install.
        for opted in [false, true] {
            assert_eq!(
                choose("delonix-cri", Some(dir.path()), opted, |_| Some(
                    PathBuf::from("/usr/local/bin/delonix-cri")
                )),
                Chosen::Beside(sib.clone()),
                "opted_in={opted}"
            );
        }
    }

    #[test]
    fn a_sibling_only_on_the_path_is_refused_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let installed = PathBuf::from("/home/u/.local/bin/delonix-cri");
        // This is the measured case: a 2026-09-07 copy in ~/.local/bin served a
        // delonix of 2026-10-08, and three battery checks failed as if the
        // engine were wrong.
        assert_eq!(
            choose("delonix-cri", Some(dir.path()), false, |_| Some(
                installed.clone()
            )),
            Chosen::Refused(Some(dir.path().to_path_buf())),
        );
    }

    #[test]
    fn the_opt_in_takes_the_path_copy_and_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let installed = PathBuf::from("/usr/local/bin/delonix-cri");
        assert_eq!(
            choose("delonix-cri", Some(dir.path()), true, |_| Some(
                installed.clone()
            )),
            Chosen::FromPath(Some(installed)),
        );
    }

    #[test]
    fn the_opt_in_with_nothing_anywhere_leaves_the_exec_to_fail() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            choose("delonix-cri", Some(dir.path()), true, nothing_on_path),
            Chosen::FromPath(None),
        );
    }

    #[test]
    fn an_unknown_executable_directory_still_refuses() {
        // `current_exe()` can fail; a decision that fell through to the PATH
        // there would reopen the hole on exactly the systems where we know
        // least about where we are.
        assert_eq!(
            choose("delonix-cri", None, false, |_| Some(PathBuf::from(
                "/usr/local/bin/delonix-cri"
            ))),
            Chosen::Refused(None),
        );
    }

    #[test]
    fn a_directory_entry_that_is_not_a_file_is_not_a_sibling() {
        // A `delonix-cri/` directory next to the binary is not a server.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("delonix-cri")).unwrap();
        assert_eq!(
            choose("delonix-cri", Some(dir.path()), false, nothing_on_path),
            Chosen::Refused(Some(dir.path().to_path_buf())),
        );
    }

    #[test]
    fn every_server_name_the_cli_dispatches_goes_through_the_same_decision() {
        // The hole was found on `delonix-cri`, and `exec_server` has four call
        // sites: a fix that covered one name would leave three open.
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "delonix-cri",
            "delonix-mgmt",
            "delonix-mcp",
            "delonix-node-api",
        ] {
            assert_eq!(
                choose(name, Some(dir.path()), false, |_| Some(PathBuf::from("/x"))),
                Chosen::Refused(Some(dir.path().to_path_buf())),
                "{name} fell through"
            );
            let sib = dir.path().join(name);
            std::fs::write(&sib, "").unwrap();
            assert_eq!(
                choose(name, Some(dir.path()), false, nothing_on_path),
                Chosen::Beside(sib),
                "{name} did not resolve beside"
            );
        }
    }

    #[test]
    fn a_search_path_finds_a_file_and_ignores_a_directory_of_the_same_name() {
        // A `dlx-probe/` DIRECTORY earlier in the path must not shadow the real
        // file later in it — the same `is_file` rule `choose` applies to the
        // sibling beside the binary.
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("dlx-probe")).unwrap();
        let real = other.path().join("dlx-probe");
        std::fs::write(&real, "").unwrap();
        let joined = std::env::join_paths([dir.path(), other.path()]).unwrap();
        assert_eq!(
            super::on_path_in(&joined, "dlx-probe").as_deref(),
            Some(real.as_path())
        );
    }

    #[test]
    fn an_empty_search_path_finds_nothing() {
        assert_eq!(
            super::on_path_in(std::ffi::OsStr::new(""), "delonix-cri"),
            None
        );
    }

    #[test]
    fn the_variable_name_is_the_one_the_adr_and_the_message_promise() {
        assert_eq!(super::FROM_PATH_ENV, "DELONIX_SERVER_FROM_PATH");
        let _ = Path::new("/");
    }
}
