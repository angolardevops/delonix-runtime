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
use delonix_model::Result;

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
    register_proxmox_zone_provider_with(&*super::vmbackends::configured_lookup()?)
}

/// [`register_proxmox_zone_provider`] with the configuration read through
/// `lookup`, so a test can hand it a map instead of writing the PROCESS
/// environment — the same reason `vmbackends`/`gatewayproviders` take one.
///
fn register_proxmox_zone_provider_with(lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    // ONE parser for the Proxmox target, shared with the VM backend
    // (ADR-0054 D4): the two registrations read the same node the same way,
    // whether it came from the environment or from the node's providers file.
    let Some((target, opts)) = super::vmbackends::proxmox_target_with(lookup)? else {
        return Ok(());
    };
    delonix_proxmox::register_network_zone_provider(
        target,
        opts,
        // The cluster-wide `apply_sdn` reload has no VM directory of its
        // own to keep a task ledger in (it is cluster-scoped, not
        // VM-scoped) — this registry's own directory is the closest honest
        // answer, the same way `kind: NetworkZone`'s own registry
        // (`cmd::network_zone`) lives under `network-zones/`.
        state_root().join("network-zones"),
    )
}

/// An environment variable that is set AND not blank.
#[cfg(test)]
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
