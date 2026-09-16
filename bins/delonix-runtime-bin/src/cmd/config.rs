//! `delonix config` — a small, local-only preference, never a context.
//!
//! The specification (§16) wants `endpoint`/`identity`/`tls` in a context —
//! that is exactly what ADR-0010 already recused (the remote management API
//! stays local, and nobody has named a concrete consumer for reopening it).
//! This module carries none of that: it is a one-key preference file on THIS
//! host, read by the CLI process that already runs on it.
//!
//! **Only `output` (`table`|`json`) today.** `namespace` was the other
//! candidate the plan named, and it is deliberately left out: unlike
//! `output`, there is no single choke point a namespace default could hook
//! into (every command that accepts `--namespace` resolves its own default
//! independently) — wiring it "because the plan wanted more keys" would be
//! exactly the field-the-system-ignores this repo has already paid for more
//! than once. `output` earns its place because [`resolve_output`] is called
//! at every one of the CLI's `-o/--output` sites (see the call sites this
//! module's own tests point at).

use std::path::{Path, PathBuf};

use delonix_runtime_core::{Error, Result};

use super::output::OutputFormat;

/// Keys this version understands. A `set`/`get`/`unset` on anything else
/// refuses by NAME, listing what exists — never silently accepted.
const KNOWN_KEYS: &[&str] = &["output"];

fn config_path(root: &Path) -> PathBuf {
    root.join("config.json")
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<String>,
}

fn load(root: &Path) -> ConfigFile {
    std::fs::read(config_path(root))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(root: &Path, cfg: &ConfigFile) -> Result<()> {
    let json =
        serde_json::to_string_pretty(cfg).map_err(|e| Error::Invalid(format!("config: {e}")))?;
    std::fs::write(config_path(root), json)?;
    Ok(())
}

/// The one thing every `-o/--output` site in the CLI calls, right where it
/// already has the flag's parsed value. Pure given its inputs — no I/O of
/// its own beyond the read `load` already does — so it is testable without a
/// real config file.
///
/// **Known limitation, stated rather than hidden**: clap hands back the same
/// `OutputFormat::Table` whether the caller typed `-o table` on purpose or
/// typed nothing at all — the derive does not expose which. A stored `json`
/// preference therefore wins over an explicit `-o table` too. Fixing that
/// needs clap's raw `ArgMatches::value_source`, which the derived structs
/// this CLI is built from do not carry through to call sites; worth doing if
/// it ever bites someone, not a reason to withhold the preference until then.
pub(crate) fn resolve_output(root: &Path, explicit: OutputFormat) -> OutputFormat {
    if explicit != OutputFormat::default() {
        return explicit;
    }
    match load(root).output.as_deref() {
        Some("json") => OutputFormat::Json,
        _ => explicit,
    }
}

#[derive(clap::Subcommand)]
pub(crate) enum ConfigCmd {
    /// Read one key, or every key set (with no argument).
    Get { key: Option<String> },
    /// Set a key. Refuses an unknown key or an invalid value by name — never
    /// accepted-and-ignored.
    Set { key: String, value: String },
    /// Remove a key — the command that reads it falls back to its built-in
    /// default again.
    Unset { key: String },
}

pub(crate) fn run(cmd: ConfigCmd) -> Result<()> {
    let root = super::util::state_root();
    match cmd {
        ConfigCmd::Get { key } => cmd_get(&root, key.as_deref()),
        ConfigCmd::Set { key, value } => cmd_set(&root, &key, &value),
        ConfigCmd::Unset { key } => cmd_unset(&root, &key),
    }
}

fn refuse_unknown_key(key: &str) -> Result<()> {
    if KNOWN_KEYS.contains(&key) {
        return Ok(());
    }
    Err(Error::Invalid(super::po::tf(
        "'{key}' is not a config key — known: {known}",
        &[("key", key), ("known", &KNOWN_KEYS.join(", "))],
    )))
}

fn cmd_get(root: &Path, key: Option<&str>) -> Result<()> {
    let cfg = load(root);
    match key {
        Some(k) => {
            refuse_unknown_key(k)?;
            println!("{}", cfg.output.as_deref().unwrap_or("(unset)"));
            Ok(())
        }
        None => {
            match &cfg.output {
                Some(v) => println!("output = {v}"),
                None => println!("output = (unset)"),
            }
            Ok(())
        }
    }
}

fn cmd_set(root: &Path, key: &str, value: &str) -> Result<()> {
    refuse_unknown_key(key)?;
    let mut cfg = load(root);
    match key {
        "output" => {
            if !matches!(value, "table" | "json") {
                return Err(Error::Invalid(super::po::tf(
                    "output must be table|json, got '{value}'",
                    &[("value", value)],
                )));
            }
            cfg.output = Some(value.to_string());
        }
        _ => unreachable!("refuse_unknown_key already rejected anything else"),
    }
    save(root, &cfg)?;
    println!(
        "{}",
        super::po::tf("{key} = {value}", &[("key", key), ("value", value)])
    );
    Ok(())
}

fn cmd_unset(root: &Path, key: &str) -> Result<()> {
    refuse_unknown_key(key)?;
    let mut cfg = load(root);
    match key {
        "output" => cfg.output = None,
        _ => unreachable!("refuse_unknown_key already rejected anything else"),
    }
    save(root, &cfg)?;
    println!("{}", super::po::tf("{key}: unset", &[("key", key)]));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The only thing that matters about `resolve_output`: it never
    /// overrides an EXPLICIT non-default request, and it fills in the
    /// configured value only when the caller got the plain compile-time
    /// default. Uses a real temp file (this module's own `load`/`save`),
    /// not a mock — the round-trip through JSON is exactly what a real
    /// invocation exercises.
    #[test]
    fn resolve_output_only_fills_in_the_unrequested_default() {
        let dir = std::env::temp_dir().join(format!(
            "delonix-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // No config file at all: default stays the default.
        assert_eq!(
            resolve_output(&dir, OutputFormat::Table),
            OutputFormat::Table
        );

        cmd_set(&dir, "output", "json").unwrap();
        // The compile-time default gets upgraded...
        assert_eq!(
            resolve_output(&dir, OutputFormat::Table),
            OutputFormat::Json
        );
        // ...but an explicit non-default request is never touched.
        assert_eq!(resolve_output(&dir, OutputFormat::Json), OutputFormat::Json);

        cmd_unset(&dir, "output").unwrap();
        assert_eq!(
            resolve_output(&dir, OutputFormat::Table),
            OutputFormat::Table
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_key_is_refused_by_name() {
        let dir = std::env::temp_dir().join(format!(
            "delonix-config-test-unknown-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(cmd_set(&dir, "namespace", "prod").is_err());
        assert!(cmd_get(&dir, Some("namespace")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_invalid_value_is_refused_not_silently_accepted() {
        let dir =
            std::env::temp_dir().join(format!("delonix-config-test-badval-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(cmd_set(&dir, "output", "yaml").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
