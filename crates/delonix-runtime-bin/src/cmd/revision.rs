//! Revision history of a stack — a RECORD of what was applied, never a source
//! of truth about what exists (ADR-0019).
//!
//! # The distinction this rests on
//!
//! A `terraform.tfstate` is the source of truth for what exists, which is what
//! makes it dangerous: it is consulted to decide reality, so when it drifts from
//! the machine the tool acts on a lie.
//!
//! A revision here is written after the fact and read only by a human — or by
//! `rollback`, which re-applies it as if it were a manifest. It is **never**
//! consulted to decide what exists. Ownership and the three-way diff keep coming
//! from the `delonix.io/stack` label and the `delonix.io/last-applied`
//! annotation, both stamped on the resource itself.
//!
//! The testable form of that promise, and there is a gate for it: **delete
//! `<root>/stacks/` and `plan`/`apply`/`prune`/`destroy` keep working byte for
//! byte — only the history is lost.**
//!
//! # Why not the event log
//!
//! `delonix-runtime-core::events` was the first idea and reading it ruled it
//! out: its fields are short on purpose (atomicity without a lock comes from
//! every append staying under `PIPE_BUF`, and a manifest does not fit), and its
//! rotation keeps a single generation — its own doc-comment says «history is not
//! the point of this».
//!
//! # Why `O_EXCL` and not a lock
//!
//! Two applies of the same stack in parallel must not both claim `0007`. The
//! `FileLock` the stores use is private to `delonix-runtime-core::store`, and a
//! second copy of it here would be a second thing to get right. `create_new`
//! (`O_EXCL`) already gives the exclusion: the kernel refuses the second
//! creator, who then tries the next number. Same idiom as `write_private_temp`.

use delonix_runtime_core::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How many revisions to keep. There is no daemon, so the writer prunes on its
/// way out — the same opportunistic shape the event log's rotation has.
///
/// Not configurable: a knob invites the question of what happens at zero, and no
/// measured need for one exists. A rendered manifest is kilobytes.
const KEEP: usize = 20;

/// What an apply recorded about itself.
///
/// Deliberately small and flat — this is read by `history` to render a table, so
/// every field here is a column someone asked for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Revision {
    /// Sequential, starting at 1.
    pub number: u32,
    /// Unix instant (seconds).
    pub ts: u64,
    /// The manifest path as given, for a human to recognise the apply.
    pub manifest: String,
    /// Whether the apply SUCCEEDED. A failed apply is recorded too — after an
    /// incident the interesting question is what the machine was asked to do,
    /// not what it managed to do.
    pub ok: bool,
    /// The error, when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Plan counts (`create`, `update`, …), for the summary column.
    #[serde(default)]
    pub summary: std::collections::BTreeMap<String, usize>,
}

/// `<root>/stacks/<stack>/revisions`.
///
/// The stack name reaches this from a manifest, so it is sanitised the way the
/// stores sanitise a key — a `metadata.name` is untrusted input, and this repo
/// has already paid for a name flowing raw into a path.
fn dir(root: &Path, stack: &str) -> PathBuf {
    let safe: String = stack
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = if safe.is_empty() || safe.starts_with('.') {
        format!("_{safe}")
    } else {
        safe
    };
    root.join("stacks").join(safe).join("revisions")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Records one revision. **Best-effort and infallible by design.**
///
/// A revision that cannot be written must never fail an apply that worked —
/// losing the record is bad, refusing to run a workload because a log directory
/// is read-only is worse. Same rule the ownership stamp and `events::emit`
/// already follow, and the one an implementer is most likely to get wrong by
/// making this `?`-propagate.
pub(crate) fn record(
    root: &Path,
    stack: &str,
    manifest_path: &str,
    rendered: &str,
    ok: bool,
    error: Option<&str>,
    summary: std::collections::BTreeMap<String, usize>,
) {
    let d = dir(root, stack);
    if std::fs::create_dir_all(&d).is_err() {
        return;
    }
    // Claim a number with O_EXCL. A concurrent apply that got there first makes
    // `create_new` fail, and we take the next one instead of overwriting theirs.
    let start = list(root, stack).last().map(|r| r.number + 1).unwrap_or(1);
    let mut number = start;
    let mut claimed = None;
    for _ in 0..64 {
        let p = d.join(format!("{number:04}.json"));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&p)
        {
            Ok(f) => {
                claimed = Some((number, p, f));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => number += 1,
            Err(_) => return,
        }
    }
    let Some((number, header_path, mut f)) = claimed else {
        return;
    };
    let rev = Revision {
        number,
        ts: now(),
        manifest: manifest_path.to_string(),
        ok,
        error: error.map(|e| {
            // One line, bounded: this is a header field rendered in a table, and
            // a multi-line registry error would wreck it.
            let one = e.replace('\n', " ");
            if one.chars().count() > 300 {
                one.chars().take(300).collect::<String>() + "…"
            } else {
                one
            }
        }),
        summary,
    };
    // The manifest goes FIRST: a header on disk claims a revision exists, so
    // writing it before its content would leave `history show` pointing at
    // nothing if the process died between the two.
    if std::fs::write(d.join(format!("{number:04}.yaml")), rendered).is_err() {
        let _ = std::fs::remove_file(&header_path);
        return;
    }
    use std::io::Write;
    let body = match serde_json::to_vec_pretty(&rev) {
        Ok(b) => b,
        Err(_) => {
            let _ = std::fs::remove_file(&header_path);
            return;
        }
    };
    if f.write_all(&body).is_err() {
        let _ = std::fs::remove_file(&header_path);
        return;
    }
    drop(f);
    prune_old(&d, KEEP);
}

/// Drops the oldest revisions past `keep`. Best-effort, like everything here.
fn prune_old(d: &Path, keep: usize) {
    let mut nums: Vec<u32> = read_nums(d);
    if nums.len() <= keep {
        return;
    }
    nums.sort_unstable();
    let drop_n = nums.len() - keep;
    for n in nums.into_iter().take(drop_n) {
        let _ = std::fs::remove_file(d.join(format!("{n:04}.json")));
        let _ = std::fs::remove_file(d.join(format!("{n:04}.yaml")));
    }
}

fn read_nums(d: &Path) -> Vec<u32> {
    let Ok(rd) = std::fs::read_dir(d) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_suffix(".json")?.parse::<u32>().ok()
        })
        .collect()
}

/// Every revision of a stack, oldest first. Never fails: no history and an
/// unreadable directory both read as «nothing recorded», which is what a
/// listing should say rather than refusing.
pub(crate) fn list(root: &Path, stack: &str) -> Vec<Revision> {
    let d = dir(root, stack);
    let mut nums = read_nums(&d);
    nums.sort_unstable();
    nums.into_iter()
        .filter_map(|n| {
            let raw = std::fs::read(d.join(format!("{n:04}.json"))).ok()?;
            serde_json::from_slice::<Revision>(&raw).ok()
        })
        .collect()
}

/// The rendered manifest of one revision.
pub(crate) fn manifest_of(root: &Path, stack: &str, number: u32) -> Result<String> {
    let p = dir(root, stack).join(format!("{number:04}.yaml"));
    std::fs::read_to_string(&p).map_err(|_| {
        delonix_runtime_core::Error::NotFound(format!("revision {number} of stack '{stack}'"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "dlx-rev-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn a_revision_round_trips_and_numbers_from_one() {
        let root = tmp();
        record(
            &root,
            "s",
            "m.yaml",
            "kind: Volume\n",
            true,
            None,
            Default::default(),
        );
        let l = list(&root, "s");
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].number, 1, "numbering starts at 1, not 0");
        assert!(l[0].ok);
        assert_eq!(manifest_of(&root, "s", 1).unwrap(), "kind: Volume\n");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A failed apply is recorded, and says so. Recording only successes would
    /// hide the revision most worth looking at after an incident.
    #[test]
    fn a_failed_apply_is_recorded_and_marked() {
        let root = tmp();
        record(
            &root,
            "s",
            "m.yaml",
            "kind: Volume\n",
            false,
            Some("boom"),
            Default::default(),
        );
        let l = list(&root, "s");
        assert!(!l[0].ok);
        assert_eq!(l[0].error.as_deref(), Some("boom"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// Retention has to drop the OLDEST, and drop both files. Dropping only the
    /// header would leave an orphan manifest that nothing lists and nothing
    /// cleans — a leak in the very mechanism meant to bound growth.
    #[test]
    fn retention_keeps_the_newest_and_leaves_no_orphan_manifest() {
        let root = tmp();
        for i in 0..KEEP + 5 {
            record(
                &root,
                "s",
                "m.yaml",
                &format!("n: {i}\n"),
                true,
                None,
                Default::default(),
            );
        }
        let l = list(&root, "s");
        assert_eq!(l.len(), KEEP);
        assert_eq!(l[0].number, 6, "the five oldest go");
        assert_eq!(l[KEEP - 1].number, (KEEP + 5) as u32);
        let d = dir(&root, "s");
        let yamls = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".yaml"))
            .count();
        assert_eq!(yamls, KEEP, "a dropped revision must take its manifest too");
        std::fs::remove_dir_all(&root).ok();
    }

    /// The stack name comes from a manifest, which is untrusted input. This repo
    /// has already paid for a `metadata.name` flowing raw into a path.
    #[test]
    fn a_hostile_stack_name_cannot_escape_the_root() {
        let root = tmp();
        record(
            &root,
            "../../etc",
            "m.yaml",
            "x: 1\n",
            true,
            None,
            Default::default(),
        );
        assert!(
            root.join("stacks").exists(),
            "the write must land under the root"
        );
        assert!(
            !root.parent().unwrap().join("etc/revisions").exists(),
            "a name with .. escaped the state root"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Two writers must not both claim the same number.
    #[test]
    fn concurrent_writers_do_not_collide_on_a_number() {
        let root = tmp();
        std::thread::scope(|s| {
            for i in 0..8 {
                let r = root.clone();
                s.spawn(move || {
                    record(
                        &r,
                        "s",
                        "m.yaml",
                        &format!("i: {i}\n"),
                        true,
                        None,
                        Default::default(),
                    )
                });
            }
        });
        let l = list(&root, "s");
        assert_eq!(l.len(), 8, "every writer got its own revision");
        let mut nums: Vec<u32> = l.iter().map(|r| r.number).collect();
        nums.sort_unstable();
        nums.dedup();
        assert_eq!(nums.len(), 8, "two writers claimed the same number");
        std::fs::remove_dir_all(&root).ok();
    }
}
