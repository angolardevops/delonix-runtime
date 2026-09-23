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
//! `compose` joined on 2026-09-23. The reason it was missing was written right
//! here — "an allowlist but no printable three-state surface of its own to
//! reuse" — and that was the work to do, not a reason to stay out: the missing
//! state needs a DENOMINATOR, and the denominator is the specification's own
//! key list. That list is now `SPEC_SERVICE_KEYS` in `compose.rs`, measured one
//! key at a time against `docker compose config` instead of typed from memory,
//! and the three states derive from it.
//!
//! `cri`/`oci` stay out, and for two different reasons. `cri`'s number lives in
//! a hand-maintained doc (`docs/cri-conformance.md`) produced by running
//! `critest` on a node — nothing this binary can derive, so a `compatibility
//! cri` would be a second copy of a number with no way to notice it going
//! stale. `oci` has zero conformance work in this repo, so the honest row count
//! is zero, and a command that prints an empty table teaches nothing. Two arms
//! with real data beat four with two of them stubs.

use clap::Subcommand;
use delonix_model::Result;

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
    /// Compose Specification coverage, key by key.
    ///
    /// Served, refused with a written reason, and MISSING — the third state,
    /// against the specification's own key list. A refusal for a key the engine
    /// has under another name says which command to use instead.
    Compose {
        /// Machine-readable JSON instead of the table (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: super::output::OutputFormat,
    },
}

pub fn run(cmd: CompatibilityCmd) -> Result<()> {
    match cmd {
        CompatibilityCmd::Docker { output } => match output {
            super::output::OutputFormat::Json => {
                let s = serde_json::to_string_pretty(&super::dockerapi::matrix_json())
                    .map_err(|e| delonix_model::Error::Invalid(format!("json output: {e}")))?;
                println!("{s}");
            }
            super::output::OutputFormat::Table => super::dockerapi::print_matrix(),
        },
        CompatibilityCmd::Compose { output } => match output {
            super::output::OutputFormat::Json => {
                let s = serde_json::to_string_pretty(&compose_json())
                    .map_err(|e| delonix_model::Error::Invalid(format!("json output: {e}")))?;
                println!("{s}");
            }
            super::output::OutputFormat::Table => print_compose_matrix(),
        },
    }
    Ok(())
}

type Row = (&'static str, super::compose::KeyState);

fn state_cell(st: &super::compose::KeyState) -> (String, String) {
    use super::compose::KeyState as K;
    match st {
        K::Served => (super::po::t("served").to_string(), String::new()),
        K::Refused { why, elsewhere } => (
            super::po::t("refused").to_string(),
            match elsewhere {
                Some(flag) => super::po::tf(
                    "{why} — `delonix {flag}`",
                    &[("why", &super::po::t_dyn(why)), ("flag", flag)],
                ),
                None => (*why).to_string(),
            },
        ),
        K::Missing => (super::po::t("missing").to_string(), String::new()),
    }
}

fn count(rows: &[Row], want: fn(&super::compose::KeyState) -> bool) -> String {
    rows.iter().filter(|(_, s)| want(s)).count().to_string()
}

fn is_served(s: &super::compose::KeyState) -> bool {
    matches!(s, super::compose::KeyState::Served)
}

fn is_refused(s: &super::compose::KeyState) -> bool {
    matches!(s, super::compose::KeyState::Refused { .. })
}

fn is_missing(s: &super::compose::KeyState) -> bool {
    matches!(s, super::compose::KeyState::Missing)
}

fn print_compose_matrix() {
    let svc = super::compose::service_matrix();
    let top = super::compose::top_matrix();

    // The header carries the numbers AND where the denominator came from. A
    // coverage figure whose total nobody can check is exactly the unmeasured
    // claim this repository refuses to make.
    println!(
        "{}",
        super::po::tf(
            "Compose Specification coverage — delonix {version}: per-service {sserved} served, {srefused} refused with a reason, {smissing} missing, of {stotal}; top-level {tserved} served, {trefused} refused, of {ttotal}.",
            &[
                ("version", env!("CARGO_PKG_VERSION")),
                ("sserved", &count(&svc, is_served)),
                ("srefused", &count(&svc, is_refused)),
                ("smissing", &count(&svc, is_missing)),
                ("stotal", &svc.len().to_string()),
                ("tserved", &count(&top, is_served)),
                ("trefused", &count(&top, is_refused)),
                ("ttotal", &top.len().to_string()),
            ],
        )
    );
    println!(
        "{}",
        super::po::t(
            "The key list is the specification's own, measured against `docker compose config` (docker v29.8.1, 2026-09-23): one file per key, kept only if the client accepted it."
        )
    );
    println!();
    let mut t = super::output::Table::new(&["SERVICE KEY", "STATE", "NOTE"]);
    for (k, st) in &svc {
        let (state, note) = state_cell(st);
        t.row(vec![k.to_string(), state, note]);
    }
    t.print();
    println!("\n{}", super::po::t("Top-level keys:"));
    let mut u = super::output::Table::new(&["TOP-LEVEL KEY", "STATE", "NOTE"]);
    for (k, st) in &top {
        let (state, note) = state_cell(st);
        u.row(vec![k.to_string(), state, note]);
    }
    u.print();
    println!(
        "\n{}",
        super::po::t(
            "Score a file you already have: `delonix migrate assess -f docker-compose.yml`."
        )
    );
}

fn compose_json() -> serde_json::Value {
    let row = |(k, st): &Row| {
        let (state, note) = state_cell(st);
        serde_json::json!({"key": k, "state": state, "note": note})
    };
    serde_json::json!({
        "surface": "compose",
        "version": env!("CARGO_PKG_VERSION"),
        "measured_against": "docker compose (docker v29.8.1), 2026-09-23",
        "service_keys": super::compose::service_matrix().iter().map(row).collect::<Vec<_>>(),
        "top_level_keys": super::compose::top_matrix().iter().map(row).collect::<Vec<_>>(),
    })
}
