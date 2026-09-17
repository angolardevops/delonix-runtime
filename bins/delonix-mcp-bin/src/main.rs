//! `delonix-mcp` — the Model Context Protocol server (ADR-0025): a LOCAL,
//! tenancy-free AI control surface. `delonix mcp <verb>` runs this binary.
//!
//! Verbs: `serve [--transport stdio]` (the default transport, a child process of
//! the AI client for one session, not a daemon), `doctor`, `capabilities`.

use std::process::ExitCode;

fn usage() -> String {
    "usage: delonix-mcp <serve [--transport stdio] | doctor | capabilities>".to_string()
}

fn run(args: &[String]) -> Result<(), String> {
    delonix_runtime_core::dispatch::check_version("delonix-mcp", env!("CARGO_PKG_VERSION"))?;
    match args.first().map(String::as_str) {
        Some("serve") => {
            let mut transport = "stdio".to_string();
            let mut rest = args[1..].iter();
            while let Some(a) = rest.next() {
                if a == "--transport" {
                    transport = rest.next().cloned().ok_or_else(usage)?;
                } else if let Some(v) = a.strip_prefix("--transport=") {
                    transport = v.to_string();
                } else {
                    return Err(format!("unknown argument: {a}"));
                }
            }
            if transport != "stdio" {
                return Err(format!(
                    "unsupported MCP transport '{transport}' — only 'stdio' is implemented \
                     (ADR-0025 defers loopback HTTP)"
                ));
            }
            let base = delonix_mcp::state_root();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("mcp runtime: {e}"))?;
            rt.block_on(delonix_mcp::serve_stdio(base))
        }
        Some("doctor") => {
            let base = delonix_mcp::state_root();
            let checks = delonix_mcp::doctor_checks(&base);
            let mut report = String::new();
            let mut all_ok = true;
            for (name, ok, detail) in &checks {
                all_ok &= ok;
                report.push_str(&format!(
                    "{} {name}: {detail}\n",
                    if *ok { "✓" } else { "✗" }
                ));
            }
            print!("{report}");
            if all_ok {
                Ok(())
            } else {
                Err("one or more MCP doctor checks failed (see above)".to_string())
            }
        }
        Some("capabilities") => {
            let table = delonix_mcp::capabilities_table();
            println!(
                "{}",
                serde_json::to_string_pretty(&table).unwrap_or_default()
            );
            Ok(())
        }
        _ => Err(usage()),
    }
}

fn main() -> ExitCode {
    delonix_telemetry::telemetry::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("delonix-mcp: {e}");
            ExitCode::FAILURE
        }
    }
}
