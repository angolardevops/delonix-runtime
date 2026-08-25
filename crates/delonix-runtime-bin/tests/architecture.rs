//! Architectural gates: the documented architecture must match the code.
//!
//! M01 of the 13-improvement programme asks for "the architecture corresponds
//! to the code". That is a property, not a paragraph, so it gets a test.
//!
//! It was written because the property was FALSE when measured (2026-08-25,
//! `b4653002b`): `ARCHITECTURE.md` said "os 10 crates" and named ten while the
//! tree had thirteen — `delonix-net-rules`, `delonix-proxmox` and
//! `delonix-truenas` existed and appeared in no C4 level. `AGENTS.md` said
//! "12 crates" and missed `delonix-net-rules`.
//!
//! Nothing had drifted deliberately: there was simply nothing that could tell.
//! This repo has paid for that shape before — a findings table nobody updated
//! read as live debt for weeks — and the lesson written down then applies here:
//! a document that describes the code needs a gate, or it starts lying in both
//! directions (a crate that exists and is undocumented, and a crate that is
//! documented and no longer exists).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The repo root, reached the way `schema.rs` and `manifest.rs` already reach it.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every directory under `crates/` that is really a crate (has a `Cargo.toml`).
///
/// Read from the filesystem and not from the workspace members list: a crate
/// present on disk but missing from the workspace is exactly the kind of thing
/// this gate should still notice.
fn crates_on_disk() -> BTreeSet<String> {
    let dir = repo_root().join("crates");
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(&dir).expect("crates/ is readable") {
        let entry = entry.expect("readable entry");
        if !entry.file_type().expect("file type").is_dir() {
            continue;
        }
        if !entry.path().join("Cargo.toml").is_file() {
            continue;
        }
        out.insert(entry.file_name().to_string_lossy().into_owned());
    }
    assert!(!out.is_empty(), "found no crates — is the repo root wrong?");
    out
}

fn read_doc(name: &str) -> String {
    let path = repo_root().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A crate is "named" by a document when its directory name appears verbatim.
///
/// Deliberately a substring check and not a table parse: the two documents
/// carry the crate names in different shapes (a markdown table in `AGENTS.md`,
/// prose plus headings in `ARCHITECTURE.md`), and a gate that demanded one
/// layout would break on an edit that improved the prose without touching the
/// property under test.
///
/// The one trap this has to dodge is prefixes: `delonix-net` is a substring of
/// `delonix-net-rules`, so a document naming only the longer one would falsely
/// count as naming the shorter. The check therefore requires the match to end
/// at a non-name character.
fn names_crate(doc: &str, krate: &str) -> bool {
    doc.match_indices(krate).any(|(at, _)| {
        let after = doc[at + krate.len()..].chars().next();
        !matches!(after, Some(c) if c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

#[test]
fn architecture_md_names_every_crate() {
    let doc = read_doc("ARCHITECTURE.md");
    let missing: Vec<_> = crates_on_disk()
        .into_iter()
        .filter(|k| !names_crate(&doc, k))
        .collect();
    assert!(
        missing.is_empty(),
        "ARCHITECTURE.md documents the architecture but does not name {} crate(s) that exist \
         on disk: {}. Either document them (C4 level 3) or delete them.",
        missing.len(),
        missing.join(", "),
    );
}

#[test]
fn agents_md_names_every_crate() {
    let doc = read_doc("AGENTS.md");
    let missing: Vec<_> = crates_on_disk()
        .into_iter()
        .filter(|k| !names_crate(&doc, k))
        .collect();
    assert!(
        missing.is_empty(),
        "AGENTS.md's architecture table does not name {} crate(s) that exist on disk: {}.",
        missing.len(),
        missing.join(", "),
    );
}

/// Both documents state a crate COUNT next to the architecture, and a count is
/// the part a reader trusts without checking. It had drifted in both.
///
/// Only the lines that make the count an assertion ABOUT THE ARCHITECTURE are
/// checked. The first version of this test checked every `N crates` in the file
/// and immediately produced three false positives in `AGENTS.md`: dated records
/// of past audits ("9 crates de motor", "~50k LOC, 9 crates") which were true
/// when written and are not claims about today's tree. "Fixing" those would
/// have falsified the history, and this repo already knows what a counter with
/// false positives is worth — noise with a number in front of it.
///
/// What the filter lets through, said out loud rather than discovered later: a
/// canonical count rewritten into a sentence carrying none of these words stops
/// being checked, silently. The `!stated.is_empty()` assertion is the floor
/// against that — a document that qualifies NO line at all fails rather than
/// passing vacuously.
const COUNT_IS_ABOUT_ARCHITECTURE: [&str; 4] =
    ["arquitect", "arquitet", "componentes", "workspace"];

#[test]
fn the_stated_crate_count_is_the_real_one() {
    let real = crates_on_disk().len();
    for (name, doc) in [
        ("ARCHITECTURE.md", read_doc("ARCHITECTURE.md")),
        ("AGENTS.md", read_doc("AGENTS.md")),
    ] {
        let mut stated: Vec<(usize, &str)> = Vec::new();
        for line in doc.lines() {
            let lower = line.to_lowercase();
            if !COUNT_IS_ABOUT_ARCHITECTURE
                .iter()
                .any(|w| lower.contains(w))
            {
                continue;
            }
            for (at, _) in line.match_indices(" crates") {
                if let Some(n) = line[..at]
                    .rsplit(|c: char| !c.is_ascii_digit())
                    .next()
                    .filter(|s| !s.is_empty())
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    stated.push((n, line));
                }
            }
        }
        assert!(
            !stated.is_empty(),
            "{name} states no crate count next to the architecture — it used to, and losing \
             it is how the number silently stops being checked",
        );
        for (n, line) in stated {
            assert_eq!(
                n,
                real,
                "{name} says {n} crates; the tree has {real} — {}",
                line.trim()
            );
        }
    }
}
