//! Registers the gateway providers this process knows how to configure
//! (ADR-0051) — the same shape `cmd::vmbackends` already has for
//! `VmBackend`, cut down to what OPNsense's own auth actually needs: a
//! key/secret pair, never a username/password (ADR-0051 Phase 0 measured
//! that a GUI account's credentials are refused by the API).
//!
//! **Costs nothing when unconfigured.** With no `DELONIX_OPNSENSE_URL` this
//! is a failed `env::var` and a return, and even when it IS configured
//! nothing connects: the registry stores a factory, and the appliance is
//! contacted the first time somebody selects the provider.

use super::po;
use super::util::state_root;
use delonix_model::{Error, Result};

/// Reads the process-wide gateway-provider configuration and registers what
/// is there. Called once at startup, alongside `vmbackends::
/// register_configured`.
///
/// A misconfigured target is **reported and skipped**, not fatal: a typo in
/// `DELONIX_OPNSENSE_SECRET` must not stop `delonix container ls` from
/// running.
pub fn register_configured() {
    if let Err(e) = register_opnsense() {
        eprintln!(
            "{}",
            po::tf(
                "warning: the OPNsense gateway provider was configured but could not be \
                 registered: {err}",
                &[("err", &e.to_string())]
            )
        );
    }
}

fn register_opnsense() -> Result<()> {
    register_opnsense_with(&|key| nonempty(std::env::var(key).ok()))
}

/// [`register_opnsense`] with the configuration read through `lookup`, so a
/// test can hand it a map instead of writing the PROCESS environment.
fn register_opnsense_with(lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    let Some(base_url) = lookup("DELONIX_OPNSENSE_URL") else {
        return Ok(());
    };
    let auth = opnsense_auth(lookup)?;
    // Opt-in, never a fallback after a TLS error: a stock OPNsense serves a
    // self-signed certificate (measured live, ADR-0051 Phase 0), but
    // skipping the check also removes what stops another machine answering
    // in the appliance's name — with the key/secret.
    let insecure_tls = lookup("DELONIX_OPNSENSE_INSECURE_TLS")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let ca_cert_pem = lookup("DELONIX_OPNSENSE_CA_FILE")
        .map(|path| {
            std::fs::read(&path)
                .map_err(|e| Error::Invalid(format!("DELONIX_OPNSENSE_CA_FILE: could not read '{path}': {e}")))
        })
        .transpose()?;
    delonix_opnsense::register_with(delonix_opnsense::Target {
        base_url,
        auth,
        insecure_tls,
        ca_cert_pem,
    })
}

/// The credential, preferring a `kind: Secret` over the plain environment
/// (a value there is inherited by every child this engine spawns).
fn opnsense_auth(lookup: &dyn Fn(&str) -> Option<String>) -> Result<delonix_opnsense::Auth> {
    if let Some(name) = lookup("DELONIX_OPNSENSE_CREDENTIAL") {
        let s = delonix_state::SecretStore::open(state_root())?.load(&name)?;
        let get = |k: &str| s.data.get(k).cloned();
        if let (Some(key), Some(secret)) = (get("key"), get("secret")) {
            return Ok(delonix_opnsense::Auth { key, secret });
        }
        return Err(Error::Invalid(po::tf(
            "secret '{name}' has neither `key` nor `secret` — generate an API key on the \
             appliance (System \u{2023} Access \u{2023} Users \u{2023} API Keys) and store the \
             two halves under those field names",
            &[("name", &name)],
        )));
    }
    if let (Some(key), Some(secret)) = (
        lookup("DELONIX_OPNSENSE_KEY"),
        credential_value(lookup, "DELONIX_OPNSENSE_SECRET")?,
    ) {
        return Ok(delonix_opnsense::Auth { key, secret });
    }
    Err(Error::Invalid(
        po::t(
            "DELONIX_OPNSENSE_URL is set but no credential is: use DELONIX_OPNSENSE_CREDENTIAL \
             (a `kind: Secret` with `key`+`secret` fields, preferred), or DELONIX_OPNSENSE_KEY \
             with DELONIX_OPNSENSE_SECRET. A GUI account's username/password does not work here \
             — only a generated API key/secret pair does (ADR-0051 Phase 0, measured).",
        )
        .into(),
    ))
}

/// A credential from `<key>_FILE` (preferred) or, failing that, `<key>`
/// itself — the same shape `vmbackends::credential_value` uses for Proxmox.
fn credential_value(lookup: &dyn Fn(&str) -> Option<String>, key: &str) -> Result<Option<String>> {
    if let Some(path) = lookup(&format!("{key}_FILE")) {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&path)?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(Error::Invalid(po::tf(
                "{path} is readable by other users — chmod 600 it before using it as {key}_FILE",
                &[("path", &path), ("key", key)],
            )));
        }
        return Ok(nonempty(Some(std::fs::read_to_string(&path)?)));
    }
    let v = lookup(key);
    if v.is_some() {
        eprintln!(
            "{} {}",
            po::t("warning:"),
            po::tf(
                "{key} is set in the environment, where every child process inherits it — \
                 prefer {key}_FILE or DELONIX_OPNSENSE_CREDENTIAL",
                &[("key", key)],
            )
        );
    }
    Ok(v)
}

/// An environment variable that is set AND not blank.
fn nonempty(raw: Option<String>) -> Option<String> {
    raw.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_configuration_registers_nothing_and_does_not_complain() {
        if nonempty(std::env::var("DELONIX_OPNSENSE_URL").ok()).is_some() {
            eprintln!("SKIP: DELONIX_OPNSENSE_URL is set in this environment");
            return;
        }
        assert!(register_opnsense().is_ok());
        assert!(delonix_sdn::gateway::gateway_provider_for("opnsense").is_none());
    }

    #[test]
    fn a_target_with_no_credential_names_what_is_missing() {
        if nonempty(std::env::var("DELONIX_OPNSENSE_URL").ok()).is_some() {
            eprintln!("SKIP: DELONIX_OPNSENSE_URL is set in this environment");
            return;
        }
        let only_url = |key: &str| {
            (key == "DELONIX_OPNSENSE_URL").then(|| "https://opnsense.local".to_string())
        };
        let e = register_opnsense_with(&only_url).unwrap_err().to_string();
        assert!(e.contains("DELONIX_OPNSENSE_KEY"), "{e}");
    }

    #[test]
    fn an_exported_but_empty_variable_does_not_count_as_configured() {
        assert!(nonempty(Some("   ".to_string())).is_none());
        assert!(nonempty(None).is_none());
        assert_eq!(
            nonempty(Some(" opnsense.local ".to_string())).as_deref(),
            Some("opnsense.local")
        );
    }
}
