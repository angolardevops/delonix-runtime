//! `delonix mcp` — the Model Context Protocol server (ADR-0025): a LOCAL,
//! tenancy-free AI control surface. The server is its own executable,
//! `delonix-mcp`, which this command runs (ADR-0040 D2.4 as amended): a user and
//! an AI client's configuration only ever name `delonix`.

use clap::Subcommand;
use delonix_runtime_core::Result;

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
    let args: Vec<String> = match action {
        McpCmd::Serve { transport } => vec!["serve".into(), "--transport".into(), transport],
        McpCmd::Doctor => vec!["doctor".into()],
        McpCmd::Capabilities => vec!["capabilities".into()],
    };
    super::serve::exec_server("delonix-mcp", &args, &[], "install.sh")
}
