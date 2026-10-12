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
    /// The versioned node contract (ADR-0040): node info, health, capacity,
    /// providers, events, operations, networks, volumes and virtual machines,
    /// with `GET /openapi.json` rendered at `GET /docs` and `GET /redoc`. It is
    /// not a daemon: `--install-units` writes a systemd socket unit that starts
    /// it on the first connection, and it exits after `--idle-timeout` with
    /// nothing open.
    NodeApi {
        /// Socket address (default: `$DELONIX_NODE_API_ADDR` or `unix:///run/delonix-node.sock`).
        #[arg(long)]
        addr: Option<String>,
        /// Exit after this long with no connection open and no operation running: `120`, `90s`, `5m`, `0` for never. Default: 300s when started by socket activation, never otherwise.
        #[arg(long, value_name = "DURATION")]
        idle_timeout: Option<String>,
        /// Write a systemd socket unit and its service (user units when rootless, system units as root), reload systemd and enable the socket, instead of serving. The node API then starts on the first connection and exits when idle (default `--idle-timeout` in the unit: 120s).
        #[arg(long)]
        install_units: bool,
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
        ServeCmd::NodeApi {
            addr,
            idle_timeout,
            install_units,
        } => {
            if install_units {
                return install_node_api_units(addr.as_deref(), idle_timeout.as_deref());
            }
            let mut args = Vec::new();
            let mut env = Vec::new();
            for (flag, var, value) in [
                ("--addr", "DELONIX_NODE_API_ADDR", addr),
                (
                    "--idle-timeout",
                    "DELONIX_NODE_API_IDLE_TIMEOUT",
                    idle_timeout,
                ),
            ] {
                if let Some(v) = value {
                    args.push(flag.to_string());
                    args.push(v.clone());
                    env.push((var, v));
                }
            }
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

/// The file a server name resolves to, by ADR-0075 D1/D2 (beside this
/// executable, or the `PATH` copy under the opt-in, said aloud). The bare name
/// when the opt-in finds nothing, which leaves an `exec` to fail with its own
/// message.
fn resolve_server(name: &str, install_hint: &str) -> Result<std::path::PathBuf> {
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
    Ok(program)
}

pub(crate) fn exec_server(
    name: &str,
    args: &[String],
    env: &[(&str, String)],
    install_hint: &str,
) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let program = resolve_server(name, install_hint)?;
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

/// The node API's socket unit, as shipped (`dist/`). A template: [`node_api_units`]
/// fills it.
const NODE_API_SOCKET_UNIT: &str = include_str!("../../../../dist/delonix-node-api.socket");
/// The node API's service unit, as shipped (`dist/`).
const NODE_API_SERVICE_UNIT: &str = include_str!("../../../../dist/delonix-node-api.service");

/// The idle timeout `--install-units` writes when none is given: short enough
/// that an idle node shows no process within two minutes of the last call.
const UNIT_DEFAULT_IDLE: &str = "120s";

/// What the two units are filled with.
#[derive(Debug)]
struct NodeApiUnits<'a> {
    /// The `delonix-node-api` executable.
    exec: &'a str,
    /// `DELONIX_ROOT`.
    root: &'a str,
    /// The socket's `ListenStream=`.
    listen: &'a str,
    /// The `--idle-timeout` value, already validated.
    idle: &'a str,
    /// `DELONIX_NET_RUNTIME_DIR`, when this `delonix` runs with one: the
    /// activated server has to reach the same network holder.
    net_runtime_dir: Option<&'a str>,
}

/// A value written into a unit line as is. systemd splits `ExecStart=` on
/// whitespace, expands `%` specifiers and `$` variables and reads quotes and
/// backslashes as syntax; a path carrying any of them would start another
/// program than the one named, so it is refused instead of quoted.
fn unit_safe(what: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "\"'\\%$;`".contains(c))
    {
        return Err(Error::Invalid(super::po::tf(
            "{what} '{value}' cannot go into a systemd unit as is: it has whitespace, a quote, a backslash, '%', '$', ';' or '`' — use a path without them",
            &[("what", what), ("value", value)],
        )));
    }
    Ok(())
}

/// The socket and service units, from the shipped templates.
fn node_api_units(u: &NodeApiUnits) -> (String, String) {
    let extra = u
        .net_runtime_dir
        .map(|d| format!("Environment=DELONIX_NET_RUNTIME_DIR={d}\n"))
        .unwrap_or_default();
    let socket = NODE_API_SOCKET_UNIT.replace("@LISTEN@", u.listen);
    let service = NODE_API_SERVICE_UNIT
        .replace("@ROOT@", u.root)
        .replace("@EXTRA_ENV@", &extra)
        .replace("@EXEC@", u.exec)
        .replace("@IDLE@", u.idle);
    (socket, service)
}

/// `delonix serve node-api --install-units`: writes the socket and service
/// units, reloads systemd and enables the socket (ADR-0040 P5).
fn install_node_api_units(addr: Option<&str>, idle: Option<&str>) -> Result<()> {
    let rootless = delonix_node::is_rootless();
    let exec = resolve_server("delonix-node-api", "install.sh")?;
    if !exec.is_absolute() {
        return Err(Error::Unavailable(super::po::tf(
            "'{name}' is not installed next to delonix nor on the PATH — install it with `{hint}`",
            &[("name", "delonix-node-api"), ("hint", "install.sh")],
        )));
    }
    let exec = exec.display().to_string();
    let root = super::util::state_root().display().to_string();
    let idle = idle.unwrap_or(UNIT_DEFAULT_IDLE);
    let never_idle = delonix_node::dispatch::parse_idle_timeout(idle)?.is_zero();
    let listen = match addr {
        Some(a) => {
            let p = a.strip_prefix("unix://").unwrap_or(a);
            if !p.starts_with('/') {
                return Err(Error::Invalid(super::po::tf(
                    "--addr '{addr}' is not an absolute socket path; a .socket unit needs one",
                    &[("addr", a)],
                )));
            }
            unit_safe("--addr", p)?;
            p.to_string()
        }
        // `%t` is systemd's runtime directory: `$XDG_RUNTIME_DIR` for a user
        // manager, `/run` for the system one.
        None if rootless => "%t/delonix-node.sock".to_string(),
        None => "/run/delonix-node.sock".to_string(),
    };
    let net = std::env::var("DELONIX_NET_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty());
    unit_safe("delonix-node-api", &exec)?;
    unit_safe("DELONIX_ROOT", &root)?;
    if let Some(d) = &net {
        unit_safe("DELONIX_NET_RUNTIME_DIR", d)?;
    }
    let (socket, service) = node_api_units(&NodeApiUnits {
        exec: &exec,
        root: &root,
        listen: &listen,
        idle,
        net_runtime_dir: net.as_deref(),
    });
    let unit_dir = if rootless {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                    .join(".config")
            });
        config.join("systemd/user")
    } else {
        std::path::PathBuf::from("/etc/systemd/system")
    };
    std::fs::create_dir_all(&unit_dir)?;
    for (name, body) in [
        ("delonix-node-api.socket", &socket),
        ("delonix-node-api.service", &service),
    ] {
        let path = unit_dir.join(name);
        std::fs::write(&path, body)?;
        println!("{}", path.display());
    }
    let scope: &[&str] = if rootless { &["--user"] } else { &[] };
    for step in [
        &["daemon-reload"][..],
        &["enable", "--now", "delonix-node-api.socket"][..],
    ] {
        let mut cmd = std::process::Command::new("systemctl");
        cmd.args(scope).args(step);
        let line = format!(
            "systemctl {}",
            scope
                .iter()
                .chain(step)
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        );
        let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            return Err(Error::Unavailable(super::po::tf(
                "the units are written, but `{cmd}` failed — run it yourself once systemd is reachable",
                &[("cmd", &line)],
            )));
        }
    }
    let shown = if listen.starts_with("%t/") {
        listen.replacen(
            "%t",
            &std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "$XDG_RUNTIME_DIR".into()),
            1,
        )
    } else {
        listen.clone()
    };
    // `0` means the server never exits on its own, and saying "exits after 0"
    // reads as the opposite.
    let template = if never_idle {
        "node API socket enabled at {sock}: the first connection starts delonix-node-api, which then stays up (`--idle-timeout 0`). Remove: systemctl {scope}disable --now delonix-node-api.socket, then delete the two files above."
    } else {
        "node API socket enabled at {sock}: the first connection starts delonix-node-api, which exits after {idle} with nothing open. Remove: systemctl {scope}disable --now delonix-node-api.socket, then delete the two files above."
    };
    println!(
        "{}",
        super::po::tf(
            template,
            &[
                ("sock", &shown),
                ("idle", idle),
                ("scope", if rootless { "--user " } else { "" }),
            ],
        )
    );
    Ok(())
}

#[cfg(test)]
mod node_api_units_tests {
    use super::{node_api_units, unit_safe, NodeApiUnits};

    fn units(net: Option<&str>) -> (String, String) {
        node_api_units(&NodeApiUnits {
            exec: "/home/u/.local/bin/delonix-node-api",
            root: "/home/u/.local/share/delonix",
            listen: "%t/delonix-node.sock",
            idle: "120s",
            net_runtime_dir: net,
        })
    }

    #[test]
    fn every_placeholder_of_the_shipped_templates_is_filled() {
        for net in [None, Some("/tmp/dlx-net")] {
            let (socket, service) = units(net);
            for (name, body) in [("socket", &socket), ("service", &service)] {
                let left: Vec<&str> = body
                    .split('@')
                    .skip(1)
                    .step_by(2)
                    .filter(|t| {
                        !t.is_empty() && t.chars().all(|c| c.is_ascii_uppercase() || c == '_')
                    })
                    .collect();
                assert!(
                    left.is_empty(),
                    "{name} keeps placeholders {left:?}:\n{body}"
                );
            }
        }
    }

    #[test]
    fn the_units_carry_what_activation_needs() {
        let (socket, service) = units(Some("/tmp/dlx-net"));
        let line = |body: &str, key: &str| {
            body.lines()
                .filter(|l| l.starts_with(key))
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            line(&socket, "ListenStream="),
            ["ListenStream=%t/delonix-node.sock"]
        );
        assert_eq!(line(&socket, "Accept="), ["Accept=no"]);
        assert_eq!(line(&socket, "SocketMode="), ["SocketMode=0600"]);
        assert_eq!(
            line(&service, "ExecStart="),
            ["ExecStart=/home/u/.local/bin/delonix-node-api --idle-timeout=120s"]
        );
        assert_eq!(
            line(&service, "Environment="),
            [
                "Environment=DELONIX_ROOT=/home/u/.local/share/delonix",
                "Environment=DELONIX_NET_RUNTIME_DIR=/tmp/dlx-net",
            ]
        );
        // The service is started by the socket, never enabled on its own, and
        // an idle exit (status 0) is not a failure to restart.
        assert!(!service.lines().any(|l| l.trim() == "[Install]"));
        assert_eq!(line(&service, "Restart="), ["Restart=on-failure"]);
        assert_eq!(
            line(&service, "Requires="),
            ["Requires=delonix-node-api.socket"]
        );
        // No --addr: under activation the address is the socket's (DX-1904).
        assert!(!line(&service, "ExecStart=")[0].contains("--addr"));
        let (_, plain) = units(None);
        assert_eq!(line(&plain, "Environment=").len(), 1);
    }

    #[test]
    fn a_value_systemd_would_reinterpret_is_refused() {
        unit_safe("x", "/home/u/.local/bin/delonix-node-api").unwrap();
        for bad in ["/a b", "/a%t", "/a$HOME", "/a\"b", "/a;b", "/a\nb", ""] {
            assert!(unit_safe("x", bad).is_err(), "{bad:?}");
        }
    }
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
