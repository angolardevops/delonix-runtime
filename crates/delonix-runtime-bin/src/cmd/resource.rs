//! `ResourceRef`: the one place a string becomes «which Kind, and which name».
//!
//! # Why this is a type and not a `match` at each call site
//!
//! The declarative verbs (`get`, `describe`, `delete`, `wait`) all take the same
//! shapes — `pods`, `pod api`, `pod/api`, `po/api` — and each one resolving them
//! itself is how they start disagreeing about what `po` means. It is the same
//! defect [`super::kinds`] exists to have removed one layer down: six lists that
//! had to agree, drifted, and the wrong answer still looked like a working
//! command.
//!
//! So the registry is the authority. A Kind is reachable by its canonical name
//! (`Pod`), its lowercase singular (`pod`), its plural (`pods`) or a declared
//! shortname (`po`) — and by nothing else.
//!
//! # Fail-closed, twice
//!
//! * An unknown token is an error naming the closest thing, never a guess. A
//!   verb that guesses which resource you meant will eventually guess `delete`.
//! * An AMBIGUOUS token is an error too, and there is a test making sure the
//!   registry can never produce one: every canonical name, plural and shortname
//!   in the whole table has to be distinct. A duplicate would silently shadow —
//!   one Kind unreachable, with no error anywhere.
//!
//! # What this deliberately does NOT do
//!
//! It does not look anything up, touch a store, or know whether the resource
//! exists. It turns a token into a Kind and stops.
//!
//! **There is no `ResourceRef` type here yet, and that is on purpose.** One was
//! written — `kind`/`kind/name` parsing, with tests — and then deleted, because
//! nothing consumes a resource REFERENCE until the declarative verbs (`get`,
//! `describe`, `delete`, `wait`) exist. This repo has deleted four public APIs
//! that sat uncalled and grew latent bugs while nobody could notice
//! (`publish_port_allow`, `Net`, and the wrong-parameter bugs in `mount_live`
//! and `reexec_start`); a fifth written by the same hand that wrote this
//! sentence would be no better. The two callers that exist TODAY —
//! `explain` and `stack apply --replace` — both want a Kind and nothing more.
//!
//! What the deleted type had settled, so it does not have to be re-derived:
//! `kind/name` is one argument and `kind name` is two, the verb owns its
//! argument shape; naming the resource twice (`pod/api web`) is refused rather
//! than resolved; and a trailing slash (`pod/`) is refused rather than read as
//! the collection — otherwise a typo in a `delete` becomes every pod.

use super::kinds::{self, KindFacts};
use delonix_runtime_core::Error;

/// Resolves one token to exactly one Kind, or says why it could not.
pub(crate) fn resolve_kind(token: &str) -> Result<&'static KindFacts, Error> {
    if token.is_empty() {
        return Err(Error::Invalid("a resource kind is missing".into()));
    }
    let want = token.to_ascii_lowercase();
    let hits: Vec<&KindFacts> = kinds::all()
        .filter(|f| {
            f.kind.to_ascii_lowercase() == want
                || f.plural == want
                || f.short.iter().any(|s| *s == want)
        })
        .collect();

    match hits.as_slice() {
        [one] => Ok(one),
        // Cannot happen while `every_name_in_the_registry_is_unique` passes; kept
        // because «cannot happen» is how a silent shadow gets in later, and an
        // error naming both is far cheaper than picking one.
        [..] if hits.len() > 1 => Err(Error::Invalid(format!(
            "'{token}' is ambiguous: {}",
            hits.iter().map(|f| f.kind).collect::<Vec<_>>().join(", ")
        ))),
        _ => Err(Error::NotFound(format!(
            "resource kind '{token}' (see `delonix stack plan --fields` for the catalogue)"
        ))),
    }
}

/// One row of `delonix api-resources` (ADR-0005: the JSON is the stable half).
///
/// Field names follow the table a caller already knows from `kubectl
/// api-resources`, because the whole point of the command is that they do not
/// have to learn a second vocabulary to ask the same question.
#[derive(serde::Serialize)]
struct ApiResourceRow {
    name: &'static str,
    #[serde(rename = "shortNames")]
    short_names: &'static [&'static str],
    #[serde(rename = "apiVersion")]
    api_version: &'static str,
    kind: &'static str,
    namespaced: bool,
    domain: &'static str,
    /// What a document of this Kind BECOMES. Not in `kubectl`'s table, and the
    /// one column here that cannot be guessed: it is the answer to «why does my
    /// `kind: Egress` never show up in the plan under that name».
    form: String,
}

/// `delonix api-resources` — what this engine serves, read from the registry
/// every other verb reads.
///
/// There is deliberately no second table: the CLI, the schema, the parser, the
/// completion and the reconciler all resolve Kinds through
/// [`super::kinds::FACTS`], and a hand-written listing beside it is how the two
/// start disagreeing about which Kinds exist — the defect that module was
/// written to remove.
pub(crate) fn api_resources(output: super::output::OutputFormat) -> Result<(), Error> {
    use super::kinds::{Form, Namespaced};

    let rows: Vec<ApiResourceRow> = kinds::all()
        .map(|f| ApiResourceRow {
            name: f.plural,
            short_names: f.short,
            api_version: f.api_version,
            kind: f.kind,
            // `PerDocument` answers `true`: a `Volume` with a `share:` block is
            // namespaced and a plain one is not, and the honest summary of «it
            // depends» is «yes, it can» — the same answer `honors_namespace`
            // gives, so the table cannot disagree with the loader.
            namespaced: !matches!(f.namespaced, Namespaced::Never),
            domain: f.domain.label(),
            form: match f.form {
                Form::Primary => "primary".to_string(),
                Form::Aggregate => "aggregate".to_string(),
                Form::Sugar(k) => format!("sugar → {k}"),
                Form::Deprecated(k) => format!("deprecated → {k}"),
                Form::Compat(k) => format!("compat → {k}"),
            },
        })
        .collect();

    if matches!(output, super::output::OutputFormat::Json) {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    let mut t = super::output::Table::new(&[
        "NAME",
        "SHORTNAMES",
        "APIVERSION",
        "KIND",
        "NAMESPACED",
        "DOMAIN",
        "FORM",
    ]);
    for r in &rows {
        t.row(vec![
            r.name.to_string(),
            r.short_names.join(","),
            r.api_version.to_string(),
            r.kind.to_string(),
            r.namespaced.to_string(),
            r.domain.to_string(),
            r.form.clone(),
        ]);
    }
    t.print();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The NAMESPACED column has to answer what the LOADER answers.
    ///
    /// `honors_namespace` is what decides whether a `metadata.namespace` is
    /// honoured or warned away, and the table printing a different answer would
    /// be worse than printing none: a caller writes the namespace because the
    /// table said the Kind takes one, and the load silently tells them it does
    /// nothing. `PerDocument` is the case that makes this non-trivial — a
    /// `Volume` with a `share:` block is namespaced and a plain one is not.
    #[test]
    fn the_namespaced_column_cannot_disagree_with_the_loader() {
        for f in kinds::all() {
            let shown = !matches!(f.namespaced, kinds::Namespaced::Never);
            assert_eq!(
                shown,
                kinds::honors_namespace(f.kind),
                "{}: api-resources says namespaced={shown}, the loader disagrees",
                f.kind
            );
        }
    }

    /// Every Kind in the registry reaches the listing, and by the same name a
    /// caller can then type. Derived rather than hand-written on purpose — a
    /// second list beside `FACTS` is how the two start disagreeing about which
    /// Kinds exist — so this checks the derivation, not the contents.
    #[test]
    fn every_registry_kind_is_listed_and_resolvable_by_its_listed_name() {
        let mut seen = 0;
        for f in kinds::all() {
            assert_eq!(resolve_kind(f.plural).unwrap().kind, f.kind);
            for sh in f.short {
                assert_eq!(resolve_kind(sh).unwrap().kind, f.kind, "{sh}");
            }
            assert!(
                f.api_version.starts_with("delonix.io/") || f.api_version.contains(".delonix.io/"),
                "{}: apiVersion {:?} is outside the delonix.io namespace",
                f.kind,
                f.api_version
            );
            seen += 1;
        }
        assert_eq!(seen, kinds::all().count());
        assert!(seen >= 12, "the registry lost Kinds: {seen}");
    }

    /// The invariant the whole module rests on. A duplicate anywhere in the
    /// registry — canonical name, plural or shortname — makes one Kind
    /// unreachable with no error raised anywhere, which is exactly the class of
    /// silent shadowing `cmd/kinds.rs` was written to remove.
    #[test]
    fn every_name_in_the_registry_is_unique() {
        let mut seen: HashMap<String, &str> = HashMap::new();
        for f in kinds::all() {
            let mut tokens = vec![f.kind.to_ascii_lowercase(), f.plural.to_string()];
            tokens.extend(f.short.iter().map(|s| s.to_string()));
            for t in tokens {
                if let Some(prev) = seen.insert(t.clone(), f.kind) {
                    panic!("'{t}' is claimed by both {prev} and {}", f.kind);
                }
            }
        }
    }

    /// A plural is a field and not `kind + "s"` — these two are the reason,
    /// and they are the ONLY two in the registry today. Most Kinds do take a
    /// bare `s`, which is precisely why guessing looks safe until it is not:
    /// a resolver built on the rule answers «no such resource» for a Kind
    /// sitting right there in the table.
    ///
    /// (The first version of this test also listed `Pod`, and the assertion
    /// below caught it — `pods` IS just an `s`.)
    #[test]
    fn the_plural_is_declared_and_not_guessed() {
        for (kind, plural) in [("Dependency", "dependencies"), ("Ingress", "ingresses")] {
            let f = resolve_kind(plural).expect(plural);
            assert_eq!(f.kind, kind);
            assert_ne!(
                format!("{}s", kind.to_ascii_lowercase()),
                f.plural,
                "{kind}: appending an s would have worked, pick another example"
            );
        }
    }

    #[test]
    fn a_kind_is_reachable_by_all_four_spellings() {
        for t in ["Pod", "pod", "pods", "po", "POD", "Po"] {
            assert_eq!(resolve_kind(t).unwrap().kind, "Pod", "{t}");
        }
    }

    #[test]
    fn an_unknown_kind_is_not_found_and_not_a_guess() {
        let e = resolve_kind("poddd").unwrap_err();
        assert_eq!(e.code(), "DX_NOT_FOUND");
        // Not `Invalid`: the argument is well formed, there is simply no such
        // resource — and that is exit 4, which a reconciler acts on.
        assert!(format!("{e}").contains("poddd"));
    }
}
