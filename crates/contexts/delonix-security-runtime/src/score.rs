//! A workload's security posture as one number — and the reasons that produced
//! it, which are the part that is actually worth reading.
//!
//! # Why the score and the explanation are one type
//!
//! «Security Score: 82/100» on its own changes nothing: nobody can act on it,
//! and nobody can check it. So [`Score`] does not expose a bare number without
//! also holding the deductions that got there — `value()` and `deductions()`
//! come from the same struct, and [`std::fmt::Display`] prints both. A caller
//! that wants only the digit has to walk past the reasons to get it.
//!
//! # Posture, not policy
//!
//! This is deliberately independent of [`crate::policy`]. The policy answers
//! «is this refused **here**»; the score answers «how exposed is this,
//! wherever it runs». A node with no policy file still has workloads worth
//! scoring, and a workload that a permissive node allows is not thereby safe.
//!
//! # What it does NOT score
//!
//! Only what admission can observe: the request. Image signature, SBOM
//! freshness, CVE count, seccomp profile, capability set and network reachability
//! all belong in the score and none of them are here — the data reaches this
//! crate through callers that do not exist yet. [`Score::COVERAGE`] says so out
//! loud, and every rendering prints it, so an 100/100 is never mistaken for a
//! clean bill of health. That is the same honesty `image scan` already applies
//! to its five-entry placeholder advisory database.

use serde::{Deserialize, Serialize};

use crate::admission::{Request, Workload};
use crate::severity::Severity;

/// Which part of the posture a deduction belongs to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Area {
    /// What is being run: pinning, provenance, where it came from.
    Image,
    /// How isolated the process is once it runs.
    Runtime,
    /// What it can reach, and what can reach it.
    Network,
}

impl Area {
    pub fn as_str(&self) -> &'static str {
        match self {
            Area::Image => "image",
            Area::Runtime => "runtime",
            Area::Network => "network",
        }
    }
}

/// How many points each observation costs.
///
/// A struct rather than constants because §29 asks for the weighting to be
/// configurable and explainable: an operator who thinks `:latest` matters more
/// than host networking on their fleet can say so, and the explanation still
/// names the same reasons.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Weights {
    pub privileged: u8,
    pub device_passthrough: u8,
    pub host_network: u8,
    pub floating_tag: u8,
    pub not_digest_pinned: u8,
    pub remote_boot_image: u8,
}

impl Default for Weights {
    fn default() -> Self {
        // The two that break isolation outright cost the most, and cost the
        // SAME: a passed-through device is not a lesser `--privileged`.
        Weights {
            privileged: 35,
            device_passthrough: 35,
            host_network: 20,
            floating_tag: 15,
            not_digest_pinned: 5,
            remote_boot_image: 10,
        }
    }
}

/// One reason the score is not 100.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Deduction {
    /// Stable identifier, greppable and alertable.
    pub id: &'static str,
    pub area: Area,
    pub severity: Severity,
    pub points: u8,
    /// English; the `-bin` translates. One sentence, and it says what to DO.
    pub reason: &'static str,
}

/// Something this workload got right. Present because a report that only lists
/// faults gives an operator no way to tell a hardened workload from an
/// unexamined one.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Merit {
    pub id: &'static str,
    pub area: Area,
    pub note: &'static str,
}

/// A posture score with the reasons that produced it.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Score {
    value: u8,
    deductions: Vec<Deduction>,
    merits: Vec<Merit>,
}

impl Score {
    /// What this score can and cannot see. Printed with every rendering.
    pub const COVERAGE: &'static str = "covers the admission request only — image signature, \
                                        SBOM, CVEs, seccomp profile, capability set and network \
                                        reachability are NOT scored";

    /// Scores a request with the default weighting.
    pub fn assess(r: &Request<'_>) -> Self {
        Self::assess_with(r, &Weights::default())
    }

    pub fn assess_with(r: &Request<'_>, w: &Weights) -> Self {
        let mut d: Vec<Deduction> = Vec::new();
        let mut m: Vec<Merit> = Vec::new();

        if r.workload == Workload::Container {
            if r.privileged {
                d.push(Deduction {
                    id: "SCORE-PRIVILEGED",
                    area: Area::Runtime,
                    severity: Severity::Critical,
                    points: w.privileged,
                    reason: "runs `--privileged`: the container's isolation is off — drop it and \
                             grant the specific capabilities it needs",
                });
            } else {
                m.push(Merit {
                    id: "SCORE-NOT-PRIVILEGED",
                    area: Area::Runtime,
                    note: "not privileged",
                });
            }

            if r.host_network {
                d.push(Deduction {
                    id: "SCORE-HOST-NETWORK",
                    area: Area::Network,
                    severity: Severity::High,
                    points: w.host_network,
                    reason: "shares the host network namespace: it can reach every service bound \
                             on the host — name a network instead",
                });
            } else {
                m.push(Merit {
                    id: "SCORE-OWN-NETNS",
                    area: Area::Network,
                    note: "has its own network namespace",
                });
            }
        }

        if r.workload == Workload::VirtualMachine && !r.devices.is_empty() {
            d.push(Deduction {
                id: "SCORE-DEVICE-PASSTHROUGH",
                area: Area::Runtime,
                severity: Severity::Critical,
                points: w.device_passthrough,
                reason: "has VFIO PCI passthrough: the guest gets DMA to host hardware, which is \
                         wider than any privileged container — remove it unless the workload \
                         genuinely needs the device",
            });
        }

        if let Some(image) = r.image {
            if crate::admission::is_latest(image) {
                d.push(Deduction {
                    id: "SCORE-FLOATING-TAG",
                    area: Area::Image,
                    severity: Severity::Medium,
                    points: w.floating_tag,
                    reason: "runs `:latest` or an untagged reference: the same manifest gives a \
                             different workload tomorrow — pin a version",
                });
            } else if !image.contains('@') {
                d.push(Deduction {
                    id: "SCORE-NOT-DIGEST-PINNED",
                    area: Area::Image,
                    severity: Severity::Low,
                    points: w.not_digest_pinned,
                    reason: "pinned by tag, not by digest: a tag can be moved — pin `@sha256:…` \
                             for a reference that cannot change under you",
                });
            } else {
                m.push(Merit {
                    id: "SCORE-DIGEST-PINNED",
                    area: Area::Image,
                    note: "image pinned by digest",
                });
            }
        }

        if r.image_url.is_some() {
            d.push(Deduction {
                id: "SCORE-REMOTE-BOOT-IMAGE",
                area: Area::Image,
                severity: Severity::Medium,
                points: w.remote_boot_image,
                reason: "boots a qcow2 fetched over the network: without a publisher checksum \
                         this is trusted on TLS alone — mirror it locally, or restrict the \
                         source with `allowedImageUrlHosts`",
            });
        }

        // Saturating, and floored at zero: a workload cannot be worse than
        // nothing, and a weighting an operator over-tuned must not wrap around
        // to a high score.
        let lost: u32 = d.iter().map(|x| u32::from(x.points)).sum();
        let value = 100u32.saturating_sub(lost).min(100) as u8;

        Score {
            value,
            deductions: d,
            merits: m,
        }
    }

    pub fn value(&self) -> u8 {
        self.value
    }

    pub fn deductions(&self) -> &[Deduction] {
        &self.deductions
    }

    pub fn merits(&self) -> &[Merit] {
        &self.merits
    }

    /// The worst severity among the deductions, if any. What an alert keys on —
    /// a single `Critical` matters more than a low total.
    pub fn worst(&self) -> Option<Severity> {
        self.deductions.iter().map(|d| d.severity).max()
    }
}

impl std::fmt::Display for Score {
    /// Prints the number **with** its reasons, always. §29: never return a
    /// score without explaining the important deductions.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Security score: {}/100", self.value)?;
        writeln!(f, "  ({})", Score::COVERAGE)?;
        for d in &self.deductions {
            writeln!(
                f,
                "  -{:<3} {:<8} {:<24} {}",
                d.points,
                d.severity.as_str(),
                d.id,
                d.reason
            )?;
        }
        for m in &self.merits {
            writeln!(f, "   ok  {:<8} {:<24} {}", m.area.as_str(), m.id, m.note)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_DEV: &[String] = &[];

    #[test]
    fn a_well_behaved_container_scores_high_and_says_what_it_got_right() {
        let r = Request::container("ghcr.io/org/app@sha256:aaaa", false, false);
        let s = Score::assess(&r);
        assert_eq!(s.value(), 100);
        assert!(s.deductions().is_empty());
        let ids: Vec<_> = s.merits().iter().map(|m| m.id).collect();
        assert!(ids.contains(&"SCORE-DIGEST-PINNED"), "{ids:?}");
        assert!(ids.contains(&"SCORE-NOT-PRIVILEGED"), "{ids:?}");
    }

    #[test]
    fn privileged_with_host_network_and_latest_drops_far_and_explains_why() {
        let r = Request::container("alpine", true, true);
        let s = Score::assess(&r);
        assert_eq!(s.value(), 100 - 35 - 20 - 15);
        assert_eq!(s.worst(), Some(Severity::Critical));
        assert_eq!(s.deductions().len(), 3);
    }

    #[test]
    fn passthrough_costs_the_same_as_privileged() {
        // Um dispositivo passado não é um `--privileged` menor.
        let w = Weights::default();
        assert_eq!(w.device_passthrough, w.privileged);
        let dev = vec!["0000:01:00.0".to_string()];
        let s = Score::assess(&Request::virtual_machine(Some("golden:1.0"), &dev, None));
        assert_eq!(s.value(), 100 - 35 - 5);
        assert_eq!(s.worst(), Some(Severity::Critical));
    }

    #[test]
    fn a_remote_qcow2_deducts_and_points_at_the_way_out() {
        let s = Score::assess(&Request::virtual_machine(
            None,
            NO_DEV,
            Some("https://example.org/x.qcow2"),
        ));
        assert_eq!(s.value(), 90);
        assert!(s.deductions()[0].reason.contains("allowedImageUrlHosts"));
    }

    #[test]
    fn a_container_rule_does_not_deduct_on_a_vm() {
        // `privileged`/`host_network` are `false` by construction on a VM
        // request; scoring them would award merit for something never checked.
        let s = Score::assess(&Request::virtual_machine(Some("g@sha256:a"), NO_DEV, None));
        assert!(s.merits().iter().all(|m| m.id != "SCORE-NOT-PRIVILEGED"));
    }

    #[test]
    fn overtuned_weights_do_not_wrap_the_counter_around() {
        // A `u8` that went past 100 and wrapped would give a terrible workload
        // a HIGH score — failure in exactly the wrong direction.
        let w = Weights {
            privileged: 200,
            host_network: 200,
            floating_tag: 200,
            ..Default::default()
        };
        let s = Score::assess_with(&Request::container("alpine", true, true), &w);
        assert_eq!(s.value(), 0);
    }

    #[test]
    fn display_never_prints_the_number_without_the_reasons() {
        let out = Score::assess(&Request::container("alpine", true, true)).to_string();
        assert!(out.contains("Security score: 30/100"), "{out}");
        assert!(out.contains("SCORE-PRIVILEGED"), "{out}");
        // And it always says what it did NOT look at.
        assert!(out.contains("NOT scored"), "{out}");
    }

    #[test]
    fn weights_refuse_an_unknown_field() {
        assert!(serde_json::from_str::<Weights>(r#"{"privilegd": 1}"#).is_err());
    }
}
