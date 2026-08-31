//! `delonix diff <kind> <name>` — the three faces of one resource, side by side.
//!
//! The three-way comparison itself is not new: `cmd::reconcile::diff_fields`
//! already computes it for every resource in a manifest, and `stack plan`
//! already prints its VERDICT (`Create`/`Update`/`Replace`/`NoOp`) for the
//! whole stack. What was missing is a place that shows the three underlying
//! VALUES — what the manifest asks for, what was last applied, what the
//! machine actually has — for ONE named resource, instead of a list of
//! actions for all of them. This module adds that view; it does not add a
//! second comparison engine.

use std::path::PathBuf;

use delonix_runtime_core::{Error, Result};

use super::manifest;
use super::output::Table;
use super::reconcile::{Actual, Desired};
use super::resource::resolve_kind;

/// One row of the three-way table, plus whether DESIRED and OBSERVED agree —
/// pure, so the row-building logic is testable without a manifest file or a
/// store on disk (same discipline `reconcile::plan` itself already follows).
struct Row {
    field: String,
    desired: String,
    last_applied: String,
    observed: String,
}

fn diff_rows(d: &Desired, a: Option<&Actual>) -> (Vec<Row>, bool) {
    let empty = std::collections::BTreeMap::new();
    let last_applied = a.and_then(|a| a.last_applied.as_ref()).unwrap_or(&empty);
    let observed = a.map(|a| &a.fields).unwrap_or(&empty);

    let mut keys: std::collections::BTreeSet<&String> = d.fields.keys().collect();
    keys.extend(last_applied.keys());
    keys.extend(observed.keys());

    let mut differs = false;
    let rows = keys
        .into_iter()
        .map(|key| {
            let want = d.fields.get(key).map(String::as_str).unwrap_or("-");
            let applied = last_applied.get(key).map(String::as_str).unwrap_or("-");
            let have = observed.get(key).map(String::as_str).unwrap_or("-");
            if want != have {
                differs = true;
            }
            Row {
                field: key.clone(),
                desired: want.to_string(),
                last_applied: applied.to_string(),
                observed: have.to_string(),
            }
        })
        .collect();
    (rows, differs)
}

/// `delonix diff <kind> <name> [-f <manifest>] [--detailed-exitcode]`.
pub(crate) fn cmd_diff(
    kind: &str,
    name: &str,
    file: Option<PathBuf>,
    detailed_exitcode: bool,
) -> Result<()> {
    let f = resolve_kind(kind)?;
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;

    let desired: Vec<Desired> = super::stack::desired_of(&docs)?;
    let actual: Vec<Actual> = super::stack::actual_of(&docs)?;

    let Some(d) = desired.iter().find(|d| d.kind == f.kind && d.name == name) else {
        return Err(Error::Invalid(format!(
            "no {} named '{name}' in {} — `stack ls -f {}` lists what it declares",
            f.kind,
            path.display(),
            path.display()
        )));
    };
    let a = actual.iter().find(|a| a.kind == f.kind && a.name == name);

    if a.is_none() {
        println!(
            "{}",
            super::po::tf(
                "{kind}/{name}: not created yet — OBSERVED is empty",
                &[("kind", f.kind), ("name", name)],
            )
        );
    }

    let (rows, differs) = diff_rows(d, a);
    let mut t = Table::new(&["FIELD", "DESIRED", "LAST-APPLIED", "OBSERVED"]);
    for r in rows {
        t.row(vec![r.field, r.desired, r.last_applied, r.observed]);
    }
    t.print();

    if detailed_exitcode && differs {
        std::process::exit(2);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(fields: &[(&str, &str)]) -> Desired {
        Desired {
            kind: "Container".into(),
            name: "web".into(),
            fields: fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            converges: true,
            ownable: true,
        }
    }

    fn actual(fields: &[(&str, &str)], last_applied: Option<&[(&str, &str)]>) -> Actual {
        Actual {
            kind: "Container".into(),
            name: "web".into(),
            fields: fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            owner: None,
            last_applied: last_applied.map(|f| {
                f.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            }),
        }
    }

    /// No actual at all (resource never created): every field shows DESIRED
    /// against `-`/`-`, and nothing "differs" in the sense that matters for
    /// `--detailed-exitcode` — a Create is not a drift.
    ///
    /// Wait — that is deliberately NOT what this checks. `differs` here is
    /// desired-vs-observed at the FIELD level, and an absent resource has no
    /// observed value for anything it declares, so it DOES differ. A `diff`
    /// on a resource that does not exist yet reporting "no changes" would be
    /// exactly the dishonest silence `--detailed-exitcode` exists to prevent.
    #[test]
    fn an_uncreated_resource_differs_on_every_declared_field() {
        let d = desired(&[("image", "nginx")]);
        let (rows, differs) = diff_rows(&d, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].desired, "nginx");
        assert_eq!(rows[0].observed, "-");
        assert!(differs);
    }

    /// A field only in `last_applied` (removed from the manifest, still on
    /// the machine) gets its own row — exactly the case the three-way diff
    /// exists to distinguish from "never ours".
    #[test]
    fn a_field_dropped_from_the_manifest_still_gets_a_row() {
        let d = desired(&[]);
        let a = actual(&[("memory", "512m")], Some(&[("memory", "256m")]));
        let (rows, differs) = diff_rows(&d, Some(&a));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].field, "memory");
        assert_eq!(rows[0].desired, "-");
        assert_eq!(rows[0].last_applied, "256m");
        assert_eq!(rows[0].observed, "512m");
        // Desired (absent) vs observed (512m): differs, by the same rule the
        // reconciler applies (nothing in the manifest for a field the
        // machine has is not automatically "fine" from `diff`'s point of
        // view — the reconciler decides revert-vs-leave-alone separately).
        assert!(differs);
    }

    /// Everything lines up: desired equals observed, `last_applied` matches
    /// too. `--detailed-exitcode` must be able to report a clean 0 here.
    #[test]
    fn identical_desired_and_observed_do_not_differ() {
        let d = desired(&[("image", "nginx:alpine")]);
        let a = actual(
            &[("image", "nginx:alpine")],
            Some(&[("image", "nginx:alpine")]),
        );
        let (_, differs) = diff_rows(&d, Some(&a));
        assert!(!differs);
    }
}
