//! The rule a server executable follows when `delonix` runs it (ADR-0040 D2.4 as
//! amended: each server is its own binary, and `delonix serve <x>` / `delonix mcp`
//! `exec` it, so a user only needs to know `delonix`).

/// Set by `delonix` when it runs a server: the version that server must be.
pub const DISPATCH_VERSION_ENV: &str = "DELONIX_DISPATCH_VERSION";

/// Set by `delonix` when it runs a server: the `delonix` executable itself, for a
/// server that runs the CLI back (the CRI's lifecycle, the MCP's mutations).
pub const CLI_BIN_ENV: &str = "DELONIX_BIN";

/// Refuses a server from another release: `Err` with the reason when `delonix`
/// said which version it expects and `mine` is not it.
///
/// A server left on the `PATH` from an older install would otherwise serve with
/// code the user never installed alongside their `delonix`. A server started
/// directly — by a unit, say — carries no expectation and is not checked.
pub fn check_version(server: &str, mine: &str) -> Result<(), String> {
    match std::env::var(DISPATCH_VERSION_ENV) {
        Ok(expected) if expected != mine => Err(format!(
            "this {server} is version {mine} but delonix is {expected} — install the {server} \
             of the same release next to delonix"
        )),
        _ => Ok(()),
    }
}

/// The `delonix` CLI a server runs back: `DELONIX_BIN`, else the `delonix` next to
/// this executable, else `delonix` on the `PATH`. NEVER this executable itself —
/// that is the server, and re-running it serves again instead of running a
/// command (measured with the CRI: the re-run bound the socket and stole it from
/// the server). The one resolver; the CRI and the MCP both call it.
pub fn cli_bin() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os(CLI_BIN_ENV) {
        return std::path::PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(sib) = exe.parent().map(|d| d.join("delonix")) {
            if sib.is_file() {
                return sib;
            }
        }
    }
    std::path::PathBuf::from("delonix")
}
