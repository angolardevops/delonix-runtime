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

/// The `--idle-timeout` of a socket-activated server (ADR-0040 P5): `90`,
/// `90s`, `500ms`, `5m`, `1h`. A bare number is seconds; `0` means «never
/// exit». One grammar for the server that reads it and the `delonix` that
/// writes it into a unit, so a value the CLI accepted is never refused at
/// activation time.
pub fn parse_idle_timeout(text: &str) -> Result<std::time::Duration, delonix_model::Error> {
    let t = text.trim();
    let bad = || {
        delonix_model::Error::Invalid(format!(
            "'{text}' is not a duration: use seconds (`120`) or a unit (`500ms`, `90s`, `5m`, `1h`)"
        ))
    };
    let (num, mult_ms) = if let Some(n) = t.strip_suffix("ms") {
        (n, 1u64)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = t.strip_suffix('h') {
        (n, 3_600_000)
    } else {
        (t, 1000)
    };
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n: u64 = num.parse().map_err(|_| bad())?;
    n.checked_mul(mult_ms)
        .map(std::time::Duration::from_millis)
        .ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::parse_idle_timeout;
    use std::time::Duration;

    #[test]
    fn idle_timeouts_read_with_and_without_a_unit() {
        assert_eq!(parse_idle_timeout("120").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_idle_timeout("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(
            parse_idle_timeout("500ms").unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(parse_idle_timeout("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_idle_timeout("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_idle_timeout("0").unwrap(), Duration::ZERO);
        for bad in ["", "s", "-1", "1.5s", "5d", "ten", "99999999999999999999h"] {
            assert!(parse_idle_timeout(bad).is_err(), "{bad}");
        }
    }
}
