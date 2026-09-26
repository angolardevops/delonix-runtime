//! The providers of this node as the contract carries them (ADR-0050 D5).
//!
//! **The composition is written here a second time**, and the second copy is
//! guarded: `delonix provider ls` composes the same list in the CLI
//! (`cmd/provider.rs`), which this crate cannot depend on. The test
//! `the_declared_providers_are_the_published_matrix` reads the ids and kinds
//! the CLI published in `docs/providers/capability-matrix.md` and fails when
//! this list disagrees — so a provider added to one side and not the other is
//! a red test, not a server that quietly lists fewer providers than the CLI.

use crate::proto::v1::{Capability, Condition, ConditionStatus, ProviderInfo};
use delonix_compute::capability::{HealthStatus, ProviderReport, CATALOG_VERSION};

/// Every provider this node knows, MEASURED on this host: the VM backends of
/// the registry (plus Proxmox unconfigured when nobody registered it), and
/// the Linux provider three times — compute, network, storage.
pub fn measured_reports() -> Vec<ProviderReport> {
    let mut out = delonix_vm::provider_reports();
    if !out.iter().any(|r| r.id == "proxmox") {
        out.push(delonix_proxmox::capability_report(false));
    }
    out.push(delonix_linux::provider_report::report(
        &delonix_linux::provider_report::LinuxHost::probe(),
    ));
    out.push(delonix_sdn::provider_report::report(
        &delonix_sdn::provider_report::SdnHost::probe(),
    ));
    out.push(delonix_volume::provider_report::report(
        &delonix_volume::provider_report::StorageHost::probe(),
    ));
    out
}

/// The same set, DECLARED: every host assumed complete, Proxmox assumed
/// configured. Pure — what the equality test against the published matrix reads.
pub fn declared_reports() -> Vec<ProviderReport> {
    use delonix_vm::capabilities as vmc;
    vec![
        vmc::cloud_hypervisor_report(&vmc::CloudHypervisorHost::ASSUMED),
        vmc::libvirt_report(&vmc::LibvirtHost::ASSUMED),
        delonix_proxmox::capability_report(true),
        delonix_linux::provider_report::report(&delonix_linux::provider_report::LinuxHost::ASSUMED),
        delonix_sdn::provider_report::report(&delonix_sdn::provider_report::SdnHost::ASSUMED),
        delonix_volume::provider_report::report(
            &delonix_volume::provider_report::StorageHost::ASSUMED,
        ),
    ]
}

/// One report as the contract's `ProviderInfo`: the same fields
/// `delonix provider ls -o json` prints, under the contract's names.
pub fn provider_info(r: &ProviderReport) -> ProviderInfo {
    let status = match r.health.status {
        HealthStatus::Healthy => ConditionStatus::True,
        HealthStatus::Unavailable => ConditionStatus::False,
        HealthStatus::Unknown => ConditionStatus::Unknown,
    };
    ProviderInfo {
        id: r.id.to_string(),
        kind: r.kind.as_str().to_string(),
        available: r.available,
        capabilities: r
            .capabilities
            .iter()
            .map(|c| Capability {
                name: c.capability.name().to_string(),
                supported: c.state.is_usable(),
                detail: c.state.detail().to_string(),
                state: c.state.label().to_string(),
            })
            .collect(),
        health: Some(Condition {
            r#type: "Healthy".to_string(),
            status: status as i32,
            reason: r.health.reason.to_string(),
            message: r.health.message.clone(),
            last_transition_time: None,
        }),
        catalog_version: CATALOG_VERSION.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
    }

    /// The CLI publishes its composition as the matrix; this crate's must be
    /// the same set of (kind, id), or the socket lists fewer providers than
    /// `provider ls` for no reason anyone wrote down.
    #[test]
    fn the_declared_providers_are_the_published_matrix() {
        let md = std::fs::read_to_string(repo().join("docs/providers/capability-matrix.md"))
            .expect("docs/providers/capability-matrix.md");
        let mut published = std::collections::BTreeSet::new();
        let mut kind = String::new();
        for line in md.lines() {
            if let Some(k) = line.strip_prefix("## ") {
                kind = k.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("- **") {
                if let Some((id, _)) = rest.split_once("**") {
                    published.insert((kind.clone(), id.to_string()));
                }
            }
        }
        let ours: std::collections::BTreeSet<(String, String)> = declared_reports()
            .iter()
            .map(|r| (r.kind.as_str().to_string(), r.id.to_string()))
            .collect();
        assert_eq!(
            ours, published,
            "node-api composes a different provider list than the CLI published"
        );
    }

    /// Every field of the CLI's JSON has a home in the contract, and the
    /// boolean is the reading of the state, never a second opinion.
    #[test]
    fn provider_info_carries_the_state_and_the_catalog_version() {
        let r = declared_reports().remove(1); // libvirt
        let info = provider_info(&r);
        assert_eq!(info.id, "libvirt");
        assert_eq!(info.kind, "compute");
        assert_eq!(info.catalog_version, CATALOG_VERSION);
        assert_eq!(info.capabilities.len(), r.capabilities.len());
        for (c, d) in info.capabilities.iter().zip(&r.capabilities) {
            assert_eq!(c.supported, d.state.is_usable(), "{}", c.name);
            assert_eq!(c.state, d.state.label(), "{}", c.name);
        }
        let h = info.health.expect("health condition");
        assert_eq!(h.r#type, "Healthy");
        assert!(!h.reason.is_empty());
    }

    /// The JSON on the socket uses the proto field names the OpenAPI declares
    /// (`naming=proto`), not camelCase.
    #[test]
    fn the_json_uses_proto_field_names() {
        let info = provider_info(&declared_reports()[0]);
        let v = serde_json::to_value(&info).unwrap();
        assert!(v.get("catalog_version").is_some(), "{v}");
        assert!(v["capabilities"][0].get("state").is_some());
        assert!(v["health"]["status"].is_string(), "enum as its name: {v}");
    }
}
