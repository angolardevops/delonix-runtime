//! What each LOCAL VM backend says about the capability catalog
//! (`delonix_compute::capability`, ADR-0050), and the host probe that narrows
//! a declared "yes" into "not on this host".
//!
//! The declaration is a `match` with no wildcard arm, on purpose: a catalog
//! entry added tomorrow fails to compile here until the backend answers it.
//! Every `Supported` names its evidence — a battery check, a scenario or a
//! test — and `bins/delonix-runtime-bin` has a test that greps for each one,
//! so an evidence string that names nothing real is a red test, not a claim.
//!
//! Two backends, two probes, and the probe is a plain struct so the SAME
//! declaration can be rendered against an assumed-complete host (the published
//! matrix) and against this machine (`delonix provider ls`).

// The libvirt half moved to its provider crate (ADR-0044 P4b.4b); re-exported
// so the bin and the node API keep reading both reports from here.
pub use delonix_provider_cloud_hypervisor::{cloud_hypervisor_report, CloudHypervisorHost};
pub use delonix_provider_libvirt::{libvirt_report, LibvirtHost};

use delonix_compute::capability::{
    CapabilityState as S, HealthStatus, ProviderHealth, ProviderKind, ProviderReport,
};

/// A report for a backend that declares nothing — every row `not-implemented`
/// and health `Unknown`/`NotDeclared`. What a test fake registers; never a
/// real backend, which is why the id is stamped in the message.
pub fn undeclared(id: &'static str) -> super::ReportFactory {
    Box::new(move || {
        ProviderReport::build(
            id,
            ProviderKind::Compute,
            false,
            ProviderHealth {
                status: HealthStatus::Unknown,
                reason: "NotDeclared",
                message: format!("backend '{id}' registered without a capability report"),
            },
            |_| S::NotImplemented,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use delonix_compute::capability::Capability as C;

    #[test]
    fn cloud_hypervisor_without_firmware_is_available_but_cannot_boot() {
        let host = CloudHypervisorHost {
            firmware: false,
            ..CloudHypervisorHost::ASSUMED
        };
        let r = cloud_hypervisor_report(&host);
        assert!(r.available, "the binary is there: selectable");
        assert_eq!(r.health.reason, "FirmwareMissing");
        let start = r
            .capabilities
            .iter()
            .find(|x| x.capability == C::VmStart)
            .unwrap();
        assert_eq!(start.state.label(), "unavailable-on-host");
    }

    #[test]
    fn the_two_local_backends_answer_every_compute_row() {
        let n = C::ALL
            .iter()
            .filter(|c| c.kind() == ProviderKind::Compute)
            .count();
        assert_eq!(libvirt_report(&LibvirtHost::ASSUMED).capabilities.len(), n);
        assert_eq!(
            cloud_hypervisor_report(&CloudHypervisorHost::ASSUMED)
                .capabilities
                .len(),
            n
        );
    }
}
