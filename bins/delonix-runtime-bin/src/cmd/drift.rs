//! `delonix drift` — what the MACHINE changed since the last apply.
//!
//! # Why this is not `stack plan` with another name
//!
//! `plan` compares three faces — the manifest as it is NOW, the last applied
//! spec, and the machine — and prints what an `apply` WOULD do. That answer
//! mixes two different things: the edits someone just made to the file, and
//! the edits someone made to the running node by hand. `plan`'s own help admits
//! the limit ("with the manifest unchanged, whatever it prints IS drift") —
//! which is exactly the point: it is only drift while nobody touched the file.
//!
//! This verb drops the manifest from the comparison and keeps the two faces
//! that belong to the node: the spec stamped on each resource when it was last
//! applied (`delonix.io/last-applied`), and what the resource looks like right
//! now. Whatever differs is something that happened OUTSIDE the declarative
//! path — a `container update`, a hand-run `nft`, an operator fixing something
//! at 3am. That question does not need the file to be answered, and a node is
//! precisely where the file often is not.
//!
//! # A resource with no stamp is not drift
//!
//! Created by hand, or adopted by a stack that predates the stamp: there is no
//! recorded intent to compare against. Reporting it as drift would be inventing
//! a baseline; hiding it would be worse (a reader would read the empty table as
//! "nothing was touched"). It gets counted and named as `unstamped`, which is a
//! third state, not a verdict.

use std::path::PathBuf;

use delonix_model::Result;
use serde::Serialize;

use super::manifest;
use super::output::{OutputFormat, Table};
use super::reconcile::Actual;

/// One field that moved under the declarative path's feet.
#[derive(Serialize)]
pub(crate) struct DriftRow {
    pub stack: String,
    pub kind: String,
    pub name: String,
    pub field: String,
    pub last_applied: String,
    pub observed: String,
}

/// A resource that carries no `last-applied` stamp — see the module doc.
#[derive(Serialize)]
pub(crate) struct Unstamped {
    pub stack: String,
    pub kind: String,
    pub name: String,
}

#[derive(Serialize, Default)]
pub(crate) struct Report {
    pub drift: Vec<DriftRow>,
    pub unstamped: Vec<Unstamped>,
    /// Kinds this run could not look at, and why — never silence.
    pub not_checked: Vec<&'static str>,
}

/// Kinds whose observed state is only enumerable FROM the manifest: their
/// `actual()` takes the parsed documents and answers about those, because the
/// node keeps no registry that lists them on its own (a `FirewallPolicy` lives
/// as nft rules attached to a target; an `Image` is content-addressed cache).
///
/// Kept honest by `doc_scoped_matches_the_stack_wiring` below, which reads
/// `stack.rs` and fails if the two ever disagree — the same discipline the
/// `kinds` table already enforces for the lists that used to drift apart.
const DOC_SCOPED: &[(&str, &str)] = &[
    ("policy", "RuntimePolicy"),
    ("image", "Image"),
    ("app", "App"),
    ("firewall", "NetworkPolicy"),
    ("httproute", "HTTPRoute"),
    ("tunnel", "Gateway"),
];

/// The sources the gate below reads to tell a module that USES the documents
/// from one that merely receives them. `network_access_rule::actual(_docs)` is
/// the case that made this necessary: it is wired with `docs` like the others
/// and ignores them, so it answers perfectly well with no manifest — listing it
/// as "not checked" would have been a warning about nothing, which costs the
/// same credit as a missing one.
#[cfg(test)]
const ACTUAL_SOURCES: &[(&str, &str)] = &[
    ("policy", include_str!("policy.rs")),
    ("image", include_str!("image.rs")),
    ("app", include_str!("app.rs")),
    ("firewall", include_str!("firewall.rs")),
    ("httproute", include_str!("httproute.rs")),
    ("tunnel", include_str!("tunnel.rs")),
    (
        "network_access_rule",
        include_str!("network_access_rule.rs"),
    ),
];

/// Pure: compares the stamp against what was observed, for ONE resource.
///
/// Only keys the stamp carries are compared. A field present in the observed
/// state and absent from the stamp is a default the apply never set, and
/// reporting it would bury the real drift under engine defaults.
fn rows_for(a: &Actual) -> Vec<DriftRow> {
    let stack = a.owner.clone().unwrap_or_default();
    let Some(stamp) = a.last_applied.as_ref() else {
        return Vec::new();
    };
    stamp
        .iter()
        .filter_map(|(field, applied)| {
            let observed = a.fields.get(field).map(String::as_str).unwrap_or("-");
            (observed != applied).then(|| DriftRow {
                stack: stack.clone(),
                kind: a.kind.clone(),
                name: a.name.clone(),
                field: field.clone(),
                last_applied: applied.clone(),
                observed: observed.to_string(),
            })
        })
        .collect()
}

/// Pure: the whole report, given what the stores answered.
fn report_of(actual: &[Actual], stack: Option<&str>, saw_manifest: bool) -> Report {
    let mut r = Report::default();
    for a in actual {
        let Some(owner) = a.owner.as_deref() else {
            continue; // not a stack's resource: not this verb's business
        };
        if stack.is_some_and(|s| s != owner) {
            continue;
        }
        if a.last_applied.is_none() {
            r.unstamped.push(Unstamped {
                stack: owner.to_string(),
                kind: a.kind.clone(),
                name: a.name.clone(),
            });
            continue;
        }
        r.drift.extend(rows_for(a));
    }
    if !saw_manifest {
        r.not_checked = DOC_SCOPED.iter().map(|(_, kind)| *kind).collect();
    }
    r
}

/// `delonix drift [-f <manifest>] [--stack <name>] [-o json] [--detailed-exitcode]`.
pub(crate) fn cmd_drift(
    file: Option<PathBuf>,
    stack: Option<String>,
    output: OutputFormat,
    detailed_exitcode: bool,
) -> Result<()> {
    // The manifest is OPTIONAL here, unlike every other verb in this family.
    // Without it the doc-scoped Kinds above cannot be enumerated — and the
    // report says so out loud instead of printing a shorter table that reads
    // like a clean node. `-f` given explicitly and missing is still an error:
    // that is a typo, not a node without a repo.
    let (docs, saw_manifest) = match &file {
        Some(_) => (
            manifest::load(&manifest::resolve_path(file.clone())?)?,
            true,
        ),
        None => match manifest::resolve_path(None) {
            Ok(p) => (manifest::load(&p)?, true),
            Err(_) => (Vec::new(), false),
        },
    };

    let actual = super::stack::actual_of(&docs)?;
    let r = report_of(&actual, stack.as_deref(), saw_manifest);

    match output {
        OutputFormat::Json => {
            let s = serde_json::to_string_pretty(&r)
                .map_err(|e| delonix_model::Error::Invalid(format!("json output: {e}")))?;
            println!("{s}");
        }
        OutputFormat::Table => print_table(&r),
    }

    if detailed_exitcode && !r.drift.is_empty() {
        std::process::exit(2);
    }
    Ok(())
}

fn print_table(r: &Report) {
    if r.drift.is_empty() {
        println!(
            "{}",
            super::po::t("no drift: every stamped field still matches the machine")
        );
    } else {
        let mut t = Table::new(&["STACK", "KIND", "NAME", "FIELD", "LAST-APPLIED", "OBSERVED"]);
        for d in &r.drift {
            t.row(vec![
                d.stack.clone(),
                d.kind.clone(),
                d.name.clone(),
                d.field.clone(),
                d.last_applied.clone(),
                d.observed.clone(),
            ]);
        }
        t.print();
    }

    if !r.unstamped.is_empty() {
        let names: Vec<String> = r
            .unstamped
            .iter()
            .map(|u| format!("{}/{}", u.kind, u.name))
            .collect();
        println!();
        println!(
            "{}",
            super::po::tf(
                "{count} resource(s) carry no last-applied stamp, so there is nothing to compare: {names}",
                &[("count", &r.unstamped.len().to_string()), ("names", &names.join(", "))],
            )
        );
    }

    if !r.not_checked.is_empty() {
        println!();
        println!(
            "{}",
            super::po::tf(
                "NOT CHECKED (no manifest found): {kinds} — their state is only enumerable from the document. Pass -f to include them.",
                &[("kinds", &r.not_checked.join(", "))],
            )
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn actual(
        kind: &str,
        name: &str,
        owner: Option<&str>,
        fields: &[(&str, &str)],
        stamp: Option<&[(&str, &str)]>,
    ) -> Actual {
        let map = |f: &[(&str, &str)]| -> BTreeMap<String, String> {
            f.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        Actual {
            kind: kind.into(),
            name: name.into(),
            fields: map(fields),
            owner: owner.map(str::to_string),
            last_applied: stamp.map(map),
        }
    }

    #[test]
    fn a_field_that_moved_since_the_apply_is_drift() {
        let a = actual(
            "Container",
            "web",
            Some("shop"),
            &[("image", "nginx:1.27"), ("memory", "128M")],
            Some(&[("image", "nginx:1.27"), ("memory", "64M")]),
        );
        let rows = rows_for(&a);
        assert_eq!(rows.len(), 1, "only the field that moved");
        assert_eq!(rows[0].field, "memory");
        assert_eq!(rows[0].last_applied, "64M");
        assert_eq!(rows[0].observed, "128M");
    }

    #[test]
    fn an_observed_field_the_apply_never_set_is_not_drift() {
        // Engine defaults fill in fields nobody declared. Counting them would
        // bury the one line that matters under a screenful of noise.
        let a = actual(
            "Container",
            "web",
            Some("shop"),
            &[("image", "nginx:1.27"), ("cpus", "1.0")],
            Some(&[("image", "nginx:1.27")]),
        );
        assert!(rows_for(&a).is_empty());
    }

    #[test]
    fn a_stamped_field_that_vanished_is_drift() {
        let a = actual(
            "Container",
            "web",
            Some("shop"),
            &[],
            Some(&[("memory", "64M")]),
        );
        let rows = rows_for(&a);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].observed, "-");
    }

    #[test]
    fn a_resource_with_no_stamp_is_a_third_state_not_drift() {
        let a = actual("Volume", "data", Some("shop"), &[("quota", "1G")], None);
        let r = report_of(std::slice::from_ref(&a), None, true);
        assert!(r.drift.is_empty());
        assert_eq!(r.unstamped.len(), 1);
        assert_eq!(r.unstamped[0].name, "data");
    }

    #[test]
    fn a_resource_owned_by_nobody_is_not_this_verbs_business() {
        let a = actual("Container", "byhand", None, &[("image", "a")], None);
        let r = report_of(&[a], None, true);
        assert!(r.drift.is_empty() && r.unstamped.is_empty());
    }

    #[test]
    fn the_stack_filter_leaves_other_stacks_alone() {
        let mine = actual(
            "Container",
            "web",
            Some("shop"),
            &[("image", "b")],
            Some(&[("image", "a")]),
        );
        let theirs = actual(
            "Container",
            "api",
            Some("other"),
            &[("image", "b")],
            Some(&[("image", "a")]),
        );
        let r = report_of(&[mine, theirs], Some("shop"), true);
        assert_eq!(r.drift.len(), 1);
        assert_eq!(r.drift[0].name, "web");
    }

    #[test]
    fn without_a_manifest_the_doc_scoped_kinds_are_named_not_silently_skipped() {
        let r = report_of(&[], None, false);
        assert!(!r.not_checked.is_empty());
        assert!(r.not_checked.contains(&"NetworkPolicy"));
        // With a manifest there is nothing to warn about.
        assert!(report_of(&[], None, true).not_checked.is_empty());
    }

    /// The list above says which Kinds need the documents. Its source of truth
    /// is the wiring in `stack::actual_of`, and a comment cannot be trusted to
    /// stay in step with it — this repository has paid for six lists that had
    /// to agree and quietly stopped agreeing. So read the wiring and compare.
    #[test]
    fn doc_scoped_matches_the_stack_wiring() {
        let src = include_str!("stack.rs");
        let body = src
            .split_once("pub(crate) fn actual_of(")
            .expect("actual_of moved")
            .1
            .split_once("\nfn ")
            .map(|(b, _)| b)
            .unwrap_or(src);
        let wired: Vec<&str> = body
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                // `super::X::actual(docs)` needs the documents;
                // `super::X::actual()` and `actual(_docs)` do not.
                let rest = l.strip_prefix("out.extend(super::")?;
                let (module, tail) = rest.split_once("::actual(")?;
                tail.starts_with("docs)").then_some(module)
            })
            .collect();
        // `policy::actual` is called before the `out.extend` chain starts.
        let wired: Vec<&str> = if body.contains("super::policy::actual(docs)") {
            wired.into_iter().chain(["policy"]).collect()
        } else {
            wired
        };
        // Being handed `docs` is not the same as USING them. Read each
        // candidate's own signature and drop the ones that ignore the
        // parameter — `fn actual(_docs: ...)`.
        let uses_docs = |module: &str| -> bool {
            let src = ACTUAL_SOURCES
                .iter()
                .find(|(m, _)| *m == module)
                .unwrap_or_else(|| {
                    panic!(
                        "`{module}::actual(docs)` is wired in `stack::actual_of` and its source \
                         is not in ACTUAL_SOURCES — add it, so this gate can tell whether it \
                         really needs the manifest"
                    )
                })
                .1;
            let sig = src
                .split_once("pub(crate) fn actual(")
                .expect("actual() signature moved")
                .1;
            !sig.starts_with('_')
        };
        let mut wired: Vec<&str> = wired.into_iter().filter(|m| uses_docs(m)).collect();
        wired.sort_unstable();
        let mut declared: Vec<&str> = DOC_SCOPED.iter().map(|(m, _)| *m).collect();
        declared.sort_unstable();
        assert_eq!(
            wired, declared,
            "DOC_SCOPED drifted from `stack::actual_of`: the report would name the wrong Kinds"
        );
    }
}
