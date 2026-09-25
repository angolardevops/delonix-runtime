//! Registers the network zone providers this process knows how to configure
//! (ADR-0049 addendum) — reads the SAME `DELONIX_PROXMOX_*` configuration
//! `cmd::vmbackends` already reads, because it is the SAME Proxmox target:
//! one node, one credential, two ports (`VmBackend` and, now,
//! `NetworkZoneProvider`). `kind: NetworkZone` itself never names a
//! provider (deliberate — the runtime's own configuration decides which
//! infrastructure realizes it, not the manifest), so this module is the
//! ONLY place that connects a target to that Kind.
//!
//! **Costs nothing when unconfigured.** With no `DELONIX_PROXMOX_URL` this
//! is a failed `env::var` and a return, and even when it IS configured
//! nothing connects: the registry stores a factory, and the node is
//! contacted the first time `kind: NetworkZone` is applied.

use super::po;
use super::util::state_root;
use delonix_model::{Error, Result};

/// Reads the process-wide Proxmox configuration and registers its SDN as a
/// `NetworkZoneProvider`. Called once at startup, alongside
/// `vmbackends::register_configured`.
///
/// A misconfigured target is **reported and skipped**, not fatal: a typo in
/// `DELONIX_PROXMOX_TOKEN` must not stop `delonix container ls` from
/// running — the same contract `vmbackends::register_configured` and
/// `gatewayproviders::register_configured` already keep.
pub fn register_configured() {
    if let Err(e) = register_proxmox_zone_provider() {
        eprintln!(
            "{}",
            po::tf(
                "warning: the Proxmox network zone provider was configured but could not be \
                 registered: {err}",
                &[("err", &e.to_string())]
            )
        );
    }
}

fn register_proxmox_zone_provider() -> Result<()> {
    register_proxmox_zone_provider_with(&|key| nonempty(std::env::var(key).ok()))
}

/// [`register_proxmox_zone_provider`] with the configuration read through
/// `lookup`, so a test can hand it a map instead of writing the PROCESS
/// environment — the same reason `vmbackends`/`gatewayproviders` take one.
///
/// **Deliberately duplicates `vmbackends::register_proxmox_with`'s
/// URL/node/TLS/credential reading** rather than sharing a helper: the two
/// registrations build different port objects (`VmBackend` vs.
/// `NetworkZoneProvider`) from the same raw configuration, and
/// `gatewayproviders`/`vmbackends` already keep their own copies of this
/// exact shape (`credential_value`/`nonempty`) rather than share one — this
/// follows the same precedent instead of inventing a third pattern.
fn register_proxmox_zone_provider_with(lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    let Some(url) = lookup("DELONIX_PROXMOX_URL") else {
        return Ok(());
    };
    let node = lookup("DELONIX_PROXMOX_NODE").ok_or_else(|| {
        Error::Invalid(
            po::t(
                "DELONIX_PROXMOX_URL is set but DELONIX_PROXMOX_NODE is not — this backend \
                 addresses ONE node and never picks one for you (the name `GET /nodes` reports, \
                 e.g. `pve`)",
            )
            .into(),
        )
    })?;
    let auth = proxmox_auth(lookup)?;
    let insecure_tls = lookup("DELONIX_PROXMOX_INSECURE_TLS")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let bridge = lookup("DELONIX_PROXMOX_BRIDGE");
    let vlan = lookup("DELONIX_PROXMOX_VLAN")
        .map(|v| parse_vlan(&v))
        .transpose()?;
    let ca_cert_pem = lookup("DELONIX_PROXMOX_CA_FILE")
        .map(|path| {
            std::fs::read(&path).map_err(|e| {
                Error::Invalid(format!(
                    "DELONIX_PROXMOX_CA_FILE: could not read '{path}': {e}"
                ))
            })
        })
        .transpose()?;
    let opts = delonix_proxmox::ClientOptions {
        trace_routes: lookup(delonix_proxmox::TRACE_ROUTES_ENV)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from),
        ..Default::default()
    };
    delonix_proxmox::register_network_zone_provider(
        delonix_proxmox::Target {
            base_url: url,
            node,
            auth,
            insecure_tls,
            bridge,
            vlan,
            ca_cert_pem,
        },
        opts,
        // The cluster-wide `apply_sdn` reload has no VM directory of its
        // own to keep a task ledger in (it is cluster-scoped, not
        // VM-scoped) — this registry's own directory is the closest honest
        // answer, the same way `kind: NetworkZone`'s own registry
        // (`cmd::network_zone`) lives under `network-zones/`.
        state_root().join("network-zones"),
    )
}

/// The credential, preferring an API token — identical reading to
/// `vmbackends::proxmox_auth` (see this module's doc comment for why it is
/// not shared).
fn proxmox_auth(lookup: &dyn Fn(&str) -> Option<String>) -> Result<delonix_proxmox::Auth> {
    if let Some(name) = lookup("DELONIX_PROXMOX_SECRET") {
        let s = delonix_state::SecretStore::open(state_root())?.load(&name)?;
        let get = |k: &str| s.data.get(k).cloned();
        if let (Some(id), Some(secret)) = (
            get("tokenId").or_else(|| get("token_id")),
            get("tokenSecret").or_else(|| get("token_secret")),
        ) {
            return Ok(delonix_proxmox::Auth::ApiToken { id, secret });
        }
        if let (Some(username), Some(password)) = (get("username"), get("password")) {
            return Ok(delonix_proxmox::Auth::Password { username, password });
        }
        return Err(Error::Invalid(po::tf(
            "secret '{name}' has neither `tokenId`+`tokenSecret` nor `username`+`password`",
            &[("name", &name)],
        )));
    }
    if let (Some(id), Some(secret)) = (
        lookup("DELONIX_PROXMOX_TOKEN_ID"),
        credential_value(lookup, "DELONIX_PROXMOX_TOKEN")?,
    ) {
        return Ok(delonix_proxmox::Auth::ApiToken { id, secret });
    }
    if let (Some(username), Some(password)) = (
        lookup("DELONIX_PROXMOX_USER"),
        credential_value(lookup, "DELONIX_PROXMOX_PASSWORD")?,
    ) {
        return Ok(delonix_proxmox::Auth::Password { username, password });
    }
    Err(Error::Invalid(
        po::t(
            "DELONIX_PROXMOX_URL is set but no credential is: use DELONIX_PROXMOX_SECRET (a \
             `kind: Secret` with `tokenId`+`tokenSecret`, preferred), or DELONIX_PROXMOX_TOKEN_ID \
             with DELONIX_PROXMOX_TOKEN, or DELONIX_PROXMOX_USER with DELONIX_PROXMOX_PASSWORD",
        )
        .into(),
    ))
}

/// A credential from `<key>_FILE` (preferred) or, failing that, `<key>` itself.
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
                 prefer {key}_FILE or DELONIX_PROXMOX_SECRET",
                &[("key", key)],
            )
        );
    }
    Ok(v)
}

fn parse_vlan(v: &str) -> Result<u16> {
    v.parse::<u16>()
        .ok()
        .filter(|t| (1..=4094).contains(t))
        .ok_or_else(|| {
            Error::Invalid(po::tf(
                "DELONIX_PROXMOX_VLAN: '{v}' is not a VLAN tag (1-4094)",
                &[("v", v)],
            ))
        })
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
        if nonempty(std::env::var("DELONIX_PROXMOX_URL").ok()).is_some() {
            eprintln!("SKIP: DELONIX_PROXMOX_URL is set in this environment");
            return;
        }
        assert!(register_proxmox_zone_provider().is_ok());
    }

    #[test]
    fn a_target_with_no_node_names_what_is_missing() {
        if nonempty(std::env::var("DELONIX_PROXMOX_URL").ok()).is_some() {
            eprintln!("SKIP: DELONIX_PROXMOX_URL is set in this environment");
            return;
        }
        let only_url = |key: &str| {
            (key == "DELONIX_PROXMOX_URL").then(|| "https://pve.local:8006".to_string())
        };
        let e = register_proxmox_zone_provider_with(&only_url)
            .unwrap_err()
            .to_string();
        assert!(e.contains("DELONIX_PROXMOX_NODE"), "{e}");
    }

    #[test]
    fn an_exported_but_empty_variable_does_not_count_as_configured() {
        assert!(nonempty(Some("   ".to_string())).is_none());
        assert!(nonempty(None).is_none());
        assert_eq!(nonempty(Some(" pve ".to_string())).as_deref(), Some("pve"));
    }
}
