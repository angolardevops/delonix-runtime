//! `delonix-mgmt` — the LOCAL management API (HTTP+JSON on a unix socket, the
//! calling uid only). `delonix serve api` runs this binary.
//!
//! `--addr` beats `DELONIX_API_ADDR`, which beats `unix:///run/delonix-mgmt.sock`.
//! The state root is `DELONIX_ROOT` (set by `delonix`), else `/var/lib/delonix`.

use std::path::PathBuf;
use std::process::ExitCode;

fn run() -> Result<(), String> {
    delonix_node::dispatch::check_version("delonix-mgmt", env!("CARGO_PKG_VERSION"))?;
    let args: Vec<String> = std::env::args().skip(1).collect();
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
    let addr = addr
        .or_else(|| std::env::var("DELONIX_API_ADDR").ok())
        .unwrap_or_else(|| "unix:///run/delonix-mgmt.sock".to_string());
    let base = std::env::var_os("DELONIX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/delonix"));
    delonix_mgmt::serve_blocking(base, &addr).map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    delonix_telemetry::telemetry::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("delonix-mgmt: {e}");
            ExitCode::FAILURE
        }
    }
}
