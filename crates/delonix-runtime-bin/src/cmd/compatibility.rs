//! `delonix compatibility <surface>` — the three-state compatibility matrix
//! (served / refused-with-a-reason / missing) as a top-level verb, one per
//! surface this engine speaks a subset of.
//!
//! This is a FRONT DOOR, not a new source of truth: `docker`'s data already
//! lives in [`super::dockerapi::API_MATRIX`]/`API_UNIMPLEMENTED`/
//! `API_UPSTREAM_USED`, self-checked against the real dispatch by a test in
//! `dockerapi.rs`. Before this command the same
//! `serve` level down from where a reader comparing surfaces would look —
//! `delonix-engine/SKILL.md` §4 names `delonix compatibility docker/compose/
//! cri/oci` as the shape this should have. `serve docker-api --matrix` keeps
//! working (nothing here removes it); this is an additional door onto the
//! same data.
//!
//! `compose`/`cri`/`oci` are deliberately NOT here yet — `compose` has an
//! allowlist (`check_unsupported_fields`) but no printable three-state
//! surface of its own to reuse; `cri`'s number lives in a hand-maintained
//! doc (`docs/cri-conformance.md`), not a generated artifact; `oci` has zero
//! conformance work in this repo. Adding a `Docker`-only arm now, rather
//! than three arms with two of them stubs, keeps this command honest about
//! what it actually covers — the same three-state discipline it exists to
//! enforce on everyone else.

use clap::Subcommand;
use delonix_runtime_core::Result;

#[derive(Subcommand)]
pub enum CompatibilityCmd {
    /// Docker Engine API coverage.
    ///
    /// Served, refused with a reason, and what real tooling (`kind`,
    /// `compose`) is observed calling.
    Docker {
        /// Machine-readable JSON instead of the table (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: super::output::OutputFormat,
    },
}

pub fn run(cmd: CompatibilityCmd) -> Result<()> {
    match cmd {
        CompatibilityCmd::Docker { output } => match output {
            super::output::OutputFormat::Json => {
                let s = serde_json::to_string_pretty(&super::dockerapi::matrix_json()).map_err(
                    |e| delonix_runtime_core::Error::Invalid(format!("json output: {e}")),
                )?;
                println!("{s}");
            }
            super::output::OutputFormat::Table => super::dockerapi::print_matrix(),
        },
    }
    Ok(())
}
