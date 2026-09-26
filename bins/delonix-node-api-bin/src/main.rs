//! `delonix-node-api` — the node API (`delonix.node.v1`, gRPC and HTTP/JSON on a
//! unix socket, the calling uid only). `delonix serve node-api` runs this binary.
//!
//! `--addr` beats `DELONIX_NODE_API_ADDR`, which beats `unix:///run/delonix-node.sock`.

use std::process::ExitCode;

/// The `--version` line: binary name and the crate version, nothing else.
fn version_line() -> String {
    format!("delonix-node-api {}", env!("CARGO_PKG_VERSION"))
}

/// True when the first argument asks for the version. Answered before any
/// effect, including the dispatch version handshake.
fn wants_version(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("--version" | "-V"))
}

/// The socket address from the arguments, the environment, or the default.
fn addr_from(args: &[String], env: Option<String>) -> Result<String, String> {
    let mut addr = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--addr" {
            addr = Some(it.next().cloned().ok_or("--addr needs a value")?);
        } else if let Some(v) = a.strip_prefix("--addr=") {
            addr = Some(v.to_string());
        } else {
            return Err(format!("unknown argument: {a}"));
        }
    }
    Ok(addr
        .or(env)
        .unwrap_or_else(|| "unix:///run/delonix-node.sock".to_string()))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    delonix_node::dispatch::check_version("delonix-node-api", env!("CARGO_PKG_VERSION"))?;
    let addr = addr_from(&args, std::env::var("DELONIX_NODE_API_ADDR").ok())?;
    delonix_node_api::serve_blocking(&addr).map_err(|e| e.to_string())
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
            eprintln!("delonix-node-api: {e}");
            ExitCode::FAILURE
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

    #[test]
    fn the_flag_beats_the_variable_which_beats_the_default() {
        assert_eq!(
            addr_from(&args(&["--addr", "unix:///a"]), Some("unix:///b".into())).unwrap(),
            "unix:///a"
        );
        assert_eq!(
            addr_from(&args(&["--addr=unix:///a"]), None).unwrap(),
            "unix:///a"
        );
        assert_eq!(
            addr_from(&[], Some("unix:///b".into())).unwrap(),
            "unix:///b"
        );
        assert_eq!(
            addr_from(&[], None).unwrap(),
            "unix:///run/delonix-node.sock"
        );
        assert!(addr_from(&args(&["--port"]), None).is_err());
        assert!(addr_from(&args(&["--addr"]), None).is_err());
    }
}
