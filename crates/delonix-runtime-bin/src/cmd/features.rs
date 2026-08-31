//! Maturity levels per capability — derived from EVIDENCE, not from opinion.
//!
//! # The question this answers
//!
//! «Is this ready?» is the first thing anyone asks of a 0.x engine, and until
//! now the answer lived in prose (`docs/cli-stability.md`) with nothing tying it
//! to the code. Prose drifts: this repo has already paid for a findings table
//! nobody updated and for an architecture document that omitted three crates.
//!
//! # What makes a level honest
//!
//! Every row names the **evidence** that sustains it, and a gate refuses a row
//! whose evidence is empty. That is the whole design: a level without evidence
//! is an opinion with a badge, and this programme treats an unmeasured claim as
//! a defect.
//!
//! The evidence is a sentence a reader can go and check — a gate that runs, a
//! measurement with a date, a conformance number with the suite that produced
//! it. Not «works well».
//!
//! # Why the levels stop where they stop
//!
//! Nothing here is `Certified`, and that is not modesty: certification means a
//! validated matrix of kernels, distros and providers, and this engine is
//! measured on ONE host and one kernel. Claiming it would be the exact failure
//! the level system exists to prevent.

use serde::Serialize;

/// How far a capability has been taken, and what that promises.
///
/// Ordered so `>=` means «at least this mature» — the derive is what lets
/// `--min` filter without a second table saying which beats which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Level {
    /// May change without notice. Do not build on it.
    Experimental,
    /// Works, and the contract is not finished.
    Preview,
    /// CLI and output are versioned and tested; breaking it needs a major.
    Stable,
    /// Load, security, upgrade and recovery validated against a real target.
    ProductionReady,
    /// A validated matrix of kernels, distros and providers. Nothing is here.
    Certified,
}

impl Level {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Level::Experimental => "experimental",
            Level::Preview => "preview",
            Level::Stable => "stable",
            Level::ProductionReady => "production-ready",
            Level::Certified => "certified",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "experimental" => Level::Experimental,
            "preview" => Level::Preview,
            "stable" => Level::Stable,
            "production-ready" => Level::ProductionReady,
            "certified" => Level::Certified,
            _ => return None,
        })
    }
}

/// One capability, its level, and the evidence for it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Feature {
    pub name: &'static str,
    pub level: Level,
    /// What a reader can go and CHECK. Never «works well».
    pub evidence: &'static str,
}

/// The published table. Most mature first: the first screen has to answer
/// «what can I depend on».
pub(crate) const FEATURES: &[Feature] = &[
    Feature {
        name: "rootless SDN (bridge, publish, firewall, namespace isolation)",
        level: Level::ProductionReady,
        evidence: "8 chaos scenarios kill the holder/control/slirp and assert recovery; \
                   namespace isolation validated live with real packets through the installed \
                   nft chain (AGENTS.md, v0.40.0)",
    },
    Feature {
        name: "container lifecycle (run/ps/stop/rm/exec/logs)",
        level: Level::Stable,
        evidence: "docs/cli-stability.md pins the verbs, flags, exit codes and inspect JSON; \
                   ~200 checks in scripts/e2e.sh run them against the real binary",
    },
    Feature {
        name: "manifest schema (apiVersion delonix.io/v1)",
        level: Level::Stable,
        evidence: "generated from the code (ADR-0007) with a test asserting the published \
                   docs/schema/v1/delonix.json IS the generated one; scripts/schema-diff.sh \
                   compares field by field between tags",
    },
    Feature {
        name: "exit-code classes",
        level: Level::Stable,
        evidence: "contract in docs/cli-stability.md since v0.49.0; cmd/exitcode.rs maps from \
                   the error TYPE with an exhaustive match, and e2e.sh asserts numeric classes",
    },
    Feature {
        name: "image pull/build (OCI)",
        level: Level::Stable,
        evidence: "digest verified on manifest AND blobs (registry.rs); resumable downloads \
                   with a live-validated cut connection; e2e.sh covers pull/build/export",
    },
    Feature {
        name: "declarative stack (plan/apply/destroy/history/rollback)",
        level: Level::Preview,
        evidence: "three-way diff with no state file; chaos scenarios stack_converge, \
                   stack_netroute and stack_partial_apply; history/rollback landed 2026-08-25 \
                   and are listed NOT stable in docs/cli-stability.md",
    },
    Feature {
        name: "CRI (serves a kubelet)",
        level: Level::Preview,
        evidence: "docs/cri-conformance.md — critest, a published number with the suite \
                   version, the engine version and the date next to it",
    },
    Feature {
        name: "microVMs (cloud-hypervisor, libvirt)",
        level: Level::Preview,
        evidence: "live-validated boot, snapshot and restore on both backends; the vm group \
                   is listed NOT stable in docs/cli-stability.md",
    },
    Feature {
        name: "docker-compose.yml (native)",
        level: Level::Preview,
        evidence: "validated end-to-end with Postgres + app, depends_on honouring a real \
                   healthcheck; the keys it does NOT support are documented, not silent",
    },
    Feature {
        name: "Docker Engine API",
        level: Level::Experimental,
        evidence: "`serve docker-api --matrix` publishes served/refused/missing with the \
                   version; of the 21 routes real tooling calls, 8 are refused — including \
                   the pull",
    },
    Feature {
        name: "Proxmox VM backend",
        level: Level::Experimental,
        evidence: "ADR-0008; crates/delonix-proxmox/tests/live.rs runs against a real node, \
                   and skips loudly without one",
    },
    Feature {
        name: "TrueNAS storage provisioning",
        level: Level::Experimental,
        evidence: "ADR-0009; chaos scenario truenas_destroy queries the NAS over HTTP instead \
                   of trusting what the CLI printed",
    },
    Feature {
        name: "fleet management (node add/cordon/drain)",
        level: Level::Experimental,
        evidence: "DOES NOT EXIST — ADR-0010 is Rejected (2026-08-10). Reopening needs a \
                   successor ADR naming the concrete consumer, never a command",
    },
];

#[derive(clap::Args, Debug)]
pub(crate) struct FeaturesArgs {
    /// Only show capabilities at this level or above (`experimental`, `preview`, `stable`, `production-ready`, `certified`).
    #[arg(long, value_name = "LEVEL")]
    pub min: Option<String>,
    /// Output format: `table` (default) or `json` (ADR-0005).
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    pub output: super::output::OutputFormat,
}

pub(crate) fn run(mut args: FeaturesArgs) -> delonix_runtime_core::Result<()> {
    args.output = super::config::resolve_output(&super::util::state_root(), args.output);
    let min = match &args.min {
        Some(s) => match Level::parse(s) {
            Some(l) => Some(l),
            None => {
                return Err(delonix_runtime_core::Error::Invalid(super::po::tf(
                    "--min '{value}': expected experimental, preview, stable, production-ready or certified",
                    &[("value", s)],
                )))
            }
        },
        None => None,
    };
    let rows: Vec<&Feature> = FEATURES
        .iter()
        .filter(|f| min.is_none_or(|m| f.level >= m))
        .collect();

    if matches!(args.output, super::output::OutputFormat::Json) {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!(
        "{}",
        super::po::tf(
            "delonix {version} — what each capability promises, and the evidence for it.",
            &[("version", env!("CARGO_PKG_VERSION"))],
        )
    );
    println!();
    let mut t = super::output::Table::new(&["LEVEL", "CAPABILITY", "EVIDENCE"]);
    for f in &rows {
        t.row(vec![
            f.level.label().to_string(),
            f.name.to_string(),
            f.evidence.to_string(),
        ]);
    }
    t.print();
    println!(
        "\n{}",
        super::po::t(
            "Nothing is `certified`: that level means a validated matrix of kernels, distros \
             and providers, and this engine is measured on one host. Claiming it would be the \
             failure this table exists to prevent."
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A level without evidence is an opinion with a badge.**
    ///
    /// The gate the whole design rests on. The evidence has to be something a
    /// reader can go and CHECK — so the test also refuses the filler that creeps
    /// in when a row is added in a hurry.
    #[test]
    fn every_feature_names_checkable_evidence() {
        for f in FEATURES {
            assert!(
                f.evidence.len() > 40,
                "{}: evidence too short to be checkable: {:?}",
                f.name,
                f.evidence
            );
            let lower = f.evidence.to_lowercase();
            for weasel in [
                "works well",
                "is solid",
                "battle-tested",
                "robust",
                "mature",
            ] {
                assert!(
                    !lower.contains(weasel),
                    "{}: '{weasel}' is an opinion, not evidence",
                    f.name
                );
            }
        }
    }

    /// Nothing may claim `certified` while the matrix that defines it does not
    /// exist. The level stays in the enum because the ladder is the contract —
    /// what is refused is a ROW using it.
    #[test]
    fn nothing_claims_certified_without_a_matrix() {
        for f in FEATURES {
            assert_ne!(
                f.level,
                Level::Certified,
                "{}: certified means a validated matrix of kernels/distros/providers, and \
                 there is none",
                f.name
            );
        }
    }

    /// Ordered by level, most mature first. This failed when the table was first
    /// written — `stable` rows sat above `production-ready`.
    #[test]
    fn the_table_is_ordered_most_mature_first() {
        let mut prev = Level::Certified;
        for f in FEATURES {
            assert!(
                f.level <= prev,
                "{}: {} appears after a less mature row",
                f.name,
                f.level.label()
            );
            prev = f.level;
        }
    }

    /// A capability that does NOT exist has to say so in the evidence rather
    /// than sit at `experimental` looking like early work. Fleet management is
    /// the case: refused by an accepted ADR, not unfinished.
    #[test]
    fn a_capability_that_does_not_exist_says_so() {
        let fleet = FEATURES
            .iter()
            .find(|f| f.name.contains("fleet"))
            .expect("the fleet row documents a REJECTED ADR and must not be dropped silently");
        assert!(
            fleet.evidence.contains("DOES NOT EXIST"),
            "the fleet row must say the capability is absent, not merely immature"
        );
    }

    #[test]
    fn min_parses_every_label_it_prints() {
        for l in [
            Level::Experimental,
            Level::Preview,
            Level::Stable,
            Level::ProductionReady,
            Level::Certified,
        ] {
            assert_eq!(
                Level::parse(l.label()),
                Some(l),
                "{} does not round-trip",
                l.label()
            );
        }
    }
}
