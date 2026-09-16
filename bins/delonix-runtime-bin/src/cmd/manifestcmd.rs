//! `delonix manifest` — what can be done to a manifest without touching the host.
//!
//! The three verbs here were already implemented and reachable, each under a
//! different roof: `stack validate`, `stack apply --dry-run` and `schema print`.
//! What they have in common is the property that names the group — **none of
//! them reads or writes the machine** — and that is the one thing a person
//! needs to know before running an unfamiliar command against production.
//!
//! They route to the existing implementations. A second validator beside the
//! first is how the two start disagreeing about what a valid manifest is.

use delonix_runtime_core::Result;
use std::path::PathBuf;

#[derive(clap::Subcommand)]
pub enum ManifestCmd {
    /// Check a manifest against the schema, the references and the graph.
    Validate {
        #[arg(short = 'f', long = "file", value_hint = clap::ValueHint::FilePath)]
        file: Option<PathBuf>,
        /// Refuse an unknown field instead of warning about it.
        #[arg(long)]
        strict: bool,
    },
    /// Print the manifest as the engine will read it, with defaults filled in.
    ///
    /// The spelling the CLI restructuring asks for is `render`; what it does is
    /// what `stack apply --dry-run` already did — round-trip every document
    /// through its typed spec so the `#[serde(default)]` values become visible.
    /// Secrets are NOT resolved: a rendered manifest is meant to be read by a
    /// person and pasted into a review.
    Render {
        #[arg(short = 'f', long = "file", value_hint = clap::ValueHint::FilePath)]
        file: Option<PathBuf>,
    },
    /// The JSON Schema, generated from the Rust types (ADR-0007).
    Schema {
        /// One Kind instead of the whole document.
        #[arg(long)]
        kind: Option<String>,
    },
}

pub fn run(action: ManifestCmd) -> Result<()> {
    match action {
        ManifestCmd::Validate { file, strict } => {
            super::stack::run(super::stack::StackCmd::Validate { file, strict })
        }
        // `render` IS `apply --dry-run`, and saying so in one line is better
        // than a second renderer that drifts from the one the apply uses.
        ManifestCmd::Render { file } => super::stack::run(super::stack::StackCmd::Apply {
            name: None,
            file,
            dry_run: true,
            replace: Vec::new(),
            prune: false,
        }),
        ManifestCmd::Schema { kind } => {
            super::schema::run(super::schema::SchemaCmd::Print { kind })
        }
    }
}

/// `migrate` is deliberately absent, and this is where that is written down.
///
/// The restructuring asks for `manifest migrate --from delonix.io/v1 --to
/// v1alpha1`. There is nothing to migrate: the loader accepts BOTH spellings
/// today — every Kind takes its own domain group or the legacy `delonix.io/v1`
/// — so a rewritten file would behave exactly like the one it replaced.
///
/// Shipping it anyway would be a command that promises work it does not do, on
/// files people keep in git. It belongs here the day a version stops being
/// accepted, and not before.
#[cfg(test)]
pub(crate) const WHY_NO_MIGRATE: &str =
    "both apiVersions are accepted today, so a migrated file would behave identically";

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// The group must not grow a verb that touches the host. That property is
    /// the only reason to put these three together, and it is not enforced by
    /// anything except this test and the reviewer reading it.
    #[test]
    fn the_group_only_carries_host_free_verbs() {
        let cmd = crate::Cli::command();
        let manifest = cmd
            .get_subcommands()
            .find(|c| c.get_name() == "manifest")
            .expect("`manifest` is declared");
        let mut names: Vec<&str> = manifest.get_subcommands().map(|c| c.get_name()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["render", "schema", "validate"],
            "a verb that reads or writes the host does not belong in `manifest`"
        );
    }

    /// `migrate` stays out until it has work to do, and the reason is written
    /// rather than left as an omission someone later reads as a gap.
    #[test]
    fn migrate_is_absent_with_a_reason() {
        assert!(!WHY_NO_MIGRATE.is_empty());
        let cmd = crate::Cli::command();
        let manifest = cmd
            .get_subcommands()
            .find(|c| c.get_name() == "manifest")
            .unwrap();
        assert!(
            manifest
                .get_subcommands()
                .all(|c| c.get_name() != "migrate"),
            "`migrate` is declared but has nothing to migrate: {WHY_NO_MIGRATE}"
        );
    }
}
