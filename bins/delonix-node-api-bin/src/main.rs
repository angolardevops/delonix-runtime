//! `delonix-node-api` — the node API (`delonix.node.v1`, gRPC and HTTP/JSON on a
//! unix socket, the calling uid only). `delonix serve node-api` runs this binary.
//!
//! A listening socket passed by systemd socket activation (`LISTEN_FDS`) is
//! served as is; otherwise `--addr` beats `DELONIX_NODE_API_ADDR`, which beats
//! `unix:///run/delonix-node.sock`. `--idle-timeout` (or
//! `DELONIX_NODE_API_IDLE_TIMEOUT`) makes the server exit once idle — the
//! default when socket-activated, so nothing stays resident (ADR-0040 P5).

use std::os::fd::RawFd;
use std::process::ExitCode;

use delonix_model::Error;
use delonix_node_api::{activation, Listen};

/// The `--version` line: binary name and the crate version, nothing else.
fn version_line() -> String {
    format!("delonix-node-api {}", env!("CARGO_PKG_VERSION"))
}

/// True when the first argument asks for the version. Answered before any
/// effect, including the dispatch version handshake.
fn wants_version(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("--version" | "-V"))
}

/// What the arguments say. Each `None` falls back to its environment variable,
/// then to the default ([`resolve`]).
#[derive(Debug, Default, PartialEq)]
struct Args {
    addr: Option<String>,
    idle_timeout: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Args, Error> {
    let mut out = Args::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        let slot = match flag {
            "--addr" => &mut out.addr,
            "--idle-timeout" => &mut out.idle_timeout,
            _ => return Err(Error::Invalid(format!("unknown argument: {a}"))),
        };
        let value = match inline {
            Some(v) => v,
            None => it
                .next()
                .cloned()
                .ok_or_else(|| Error::Invalid(format!("{flag} needs a value")))?,
        };
        *slot = Some(value);
    }
    Ok(out)
}

/// The default address when nothing is inherited and nothing is given.
const DEFAULT_ADDR: &str = "unix:///run/delonix-node.sock";

/// Where to listen and when to exit, from the arguments, the environment and
/// an inherited socket (`fd`, ADR-0040 P5).
///
/// - An inherited socket wins over `DELONIX_NODE_API_ADDR` and the default;
///   an EXPLICIT `--addr` together with one is refused (DX-1904): the two name
///   different sockets, and serving one silently would leave the other's
///   clients unanswered.
/// - The idle timeout: `--idle-timeout`, else `DELONIX_NODE_API_IDLE_TIMEOUT`,
///   else [`activation::DEFAULT_ACTIVATED_IDLE`] when activated and «never»
///   when this process binds its own socket. `0` is «never».
fn resolve(
    args: &Args,
    env_addr: Option<String>,
    env_idle: Option<String>,
    fd: Option<RawFd>,
    root: std::path::PathBuf,
) -> Result<(Listen, Option<activation::IdleExit>), Error> {
    let listen = match (fd, &args.addr) {
        (Some(fd), Some(addr)) => {
            return Err(Error::coded(
                1904,
                Error::Invalid(format!(
                    "--addr {addr} was given, but the service manager passed a listening socket \
                     (descriptor {fd}); drop --addr from the unit's ExecStart= — the socket is \
                     the .socket unit's ListenStream="
                )),
            ))
        }
        (Some(fd), None) => Listen::Inherited(fd),
        (None, addr) => Listen::Path(
            addr.clone()
                .or(env_addr)
                .unwrap_or_else(|| DEFAULT_ADDR.to_string()),
        ),
    };
    let timeout = match args.idle_timeout.clone().or(env_idle) {
        Some(t) => Some(delonix_node::dispatch::parse_idle_timeout(&t)?),
        None if fd.is_some() => Some(activation::DEFAULT_ACTIVATED_IDLE),
        None => None,
    };
    let idle = timeout.map(|timeout| activation::IdleExit { timeout, root });
    Ok((listen, idle))
}

fn run() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    delonix_node::dispatch::check_version("delonix-node-api", env!("CARGO_PKG_VERSION"))
        .map_err(Error::Unavailable)?;
    let parsed = parse_args(&args)?;
    let (listen, idle) = resolve(
        &parsed,
        std::env::var("DELONIX_NODE_API_ADDR").ok(),
        std::env::var("DELONIX_NODE_API_IDLE_TIMEOUT").ok(),
        activation::from_env()?,
        delonix_node_api::node::state_root(),
    )?;
    delonix_node_api::serve(listen, idle)
}

fn main() -> ExitCode {
    // `--version` must not initialise telemetry (no exporter, no side effects).
    if wants_version(&std::env::args().skip(1).collect::<Vec<_>>()) {
        println!("{}", version_line());
        return ExitCode::SUCCESS;
    }
    delonix_telemetry::telemetry::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "delonix-node-api: {}: {e}",
                delonix_model::codes::label(e.number())
            );
            ExitCode::from(delonix_model::exitcode::for_error(&e).clamp(1, 255) as u8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn version_flags_are_recognised() {
        assert!(wants_version(&args(&["--version"])));
        assert!(wants_version(&args(&["-V"])));
        assert!(!wants_version(&args(&[])));
        assert!(!wants_version(&args(&["--addr", "--version"])));
    }

    fn resolve_plain(a: &[&str], env: Option<&str>, fd: Option<RawFd>) -> Result<Listen, Error> {
        let parsed = parse_args(&args(a))?;
        resolve(&parsed, env.map(String::from), None, fd, "/r".into()).map(|(l, _)| l)
    }

    fn path_of(l: Listen) -> String {
        match l {
            Listen::Path(p) => p,
            other => panic!("expected a path, got {other:?}"),
        }
    }

    #[test]
    fn the_flag_beats_the_variable_which_beats_the_default() {
        assert_eq!(
            path_of(resolve_plain(&["--addr", "unix:///a"], Some("unix:///b"), None).unwrap()),
            "unix:///a"
        );
        assert_eq!(
            path_of(resolve_plain(&["--addr=unix:///a"], None, None).unwrap()),
            "unix:///a"
        );
        assert_eq!(
            path_of(resolve_plain(&[], Some("unix:///b"), None).unwrap()),
            "unix:///b"
        );
        assert_eq!(
            path_of(resolve_plain(&[], None, None).unwrap()),
            "unix:///run/delonix-node.sock"
        );
        assert!(parse_args(&args(&["--port"])).is_err());
        assert!(parse_args(&args(&["--addr"])).is_err());
    }

    #[test]
    fn an_inherited_socket_beats_the_variable_and_refuses_an_explicit_addr() {
        assert!(matches!(
            resolve_plain(&[], Some("unix:///b"), Some(3)).unwrap(),
            Listen::Inherited(3)
        ));
        let e = resolve_plain(&["--addr", "unix:///a"], None, Some(3)).unwrap_err();
        assert_eq!(e.number(), 1904, "{e}");
    }

    #[test]
    fn the_idle_timeout_defaults_only_when_activated() {
        let r = |a: &[&str], env: Option<&str>, fd| {
            resolve(
                &parse_args(&args(a)).unwrap(),
                None,
                env.map(String::from),
                fd,
                "/r".into(),
            )
            .map(|(_, i)| i.map(|i| i.timeout))
        };
        use std::time::Duration;
        assert_eq!(r(&[], None, None).unwrap(), None);
        assert_eq!(
            r(&[], None, Some(3)).unwrap(),
            Some(activation::DEFAULT_ACTIVATED_IDLE)
        );
        assert_eq!(
            r(&["--idle-timeout", "2s"], Some("9"), None).unwrap(),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            r(&[], Some("9"), None).unwrap(),
            Some(Duration::from_secs(9))
        );
        assert_eq!(
            r(&["--idle-timeout=0"], None, Some(3)).unwrap(),
            Some(Duration::ZERO)
        );
        assert!(r(&["--idle-timeout", "soon"], None, None).is_err());
    }
}
