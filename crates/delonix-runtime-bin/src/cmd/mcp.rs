//! `delonix mcp` — the Model Context Protocol server (ADR-0025): a LOCAL,
//! tenancy-free AI control surface. Wiring only; the server itself lives in
//! `delonix-mcp` (kept out of this crate's dependents, same pattern as
//! `cmd::serve` wrapping `delonix-mgmt`/`delonix-cri`).

use clap::Subcommand;
use delonix_runtime_core::{Error, Result};

#[derive(Subcommand)]
pub enum McpCmd {
    /// Start the MCP server.
    ///
    /// `stdio` (default) is the supported transport — a child process of the
    /// AI client for one session, not a daemon.
    Serve {
        /// Transport: only `stdio` is implemented in this pass.
        #[arg(long, default_value = "stdio")]
        transport: String,
    },
    /// Check that this node is ready to serve MCP tool calls.
    ///
    /// Stores openable, state dir writable, the `delonix` binary resolvable
    /// for mutations.
    Doctor,
    /// Print the tool risk table (name, risk level, whether `confirm` is required).
    Capabilities,
}

pub fn run(action: McpCmd) -> Result<()> {
    match action {
        McpCmd::Serve { transport } => {
            if transport != "stdio" {
                return Err(Error::Invalid(format!(
                    "unsupported MCP transport '{transport}' — only 'stdio' is implemented (ADR-0025 defers loopback HTTP)"
                )));
            }
            let base = delonix_mcp::state_root();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| Error::Runtime {
                    context: "mcp runtime",
                    message: e.to_string(),
                })?;
            rt.block_on(delonix_mcp::serve_stdio(base))
                .map_err(|message| Error::Runtime {
                    context: "mcp serve",
                    message,
                })
        }
        McpCmd::Doctor => {
            let base = delonix_mcp::state_root();
            let checks = delonix_mcp::doctor_checks(&base);
            let mut all_ok = true;
            for (name, ok, detail) in &checks {
                all_ok &= ok;
                println!("{} {name}: {detail}", if *ok { "✓" } else { "✗" });
            }
            if all_ok {
                Ok(())
            } else {
                Err(Error::Invalid(
                    "one or more MCP doctor checks failed (see above)".to_string(),
                ))
            }
        }
        McpCmd::Capabilities => {
            let table = delonix_mcp::capabilities_table();
            println!(
                "{}",
                serde_json::to_string_pretty(&table).unwrap_or_default()
            );
            Ok(())
        }
    }
}
