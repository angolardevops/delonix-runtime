//! The operator host's `/etc/hosts`, kept in step with the routes that asked for it
//! (`hosts: [host]`, ADR-0046).
//!
//! **One delimited block, and nothing else in the file is ever touched.** The block
//! is rewritten whole, so a name disappears the moment no route asks for it, with
//! nothing to reap. A name that already has an entry OUTSIDE the block is refused
//! rather than shadowed or overwritten: the operator wrote that line, and a second
//! answer for the same name is a silent change of where it points.
//!
//! The address is `127.0.0.1` by default: the proxy runs in the holder netns and reaches
//! the host through the slirp forward, which binds loopback by default. A route that
//! claims an `IPPool` address gets that address instead. The PORT is the route's
//! entrypoint — a hosts file cannot carry one.
//!
//! Writing needs root. Without it the apply STOPS with the exact block to add, and
//! does not pretend: an unprivileged run is not a reason to skip the name silently.

use delonix_model::{Error, Result};
use std::path::PathBuf;

const BEGIN_PREFIX: &str = "# BEGIN delonix ";
const END_PREFIX: &str = "# END delonix ";

/// Identity of one state root, stable across runs and releases (FNV-1a over the path —
/// `DefaultHasher` is not promised stable, and a block orphaned by a hash change is a
/// name that never goes away).
///
/// Several state roots can share one `/etc/hosts` (an isolated root next to the real
/// one, root and rootless users). Each rebuilds only its OWN names, so each owns its OWN
/// block: with a single shared block, whichever root rebuilt last erased the other's
/// names.
pub(crate) fn root_id() -> String {
    let path = super::util::state_root();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.to_string_lossy().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", h & 0xffff_ffff)
}

fn begin(id: &str) -> String {
    format!("{BEGIN_PREFIX}{id} (managed — do not edit)")
}

fn end(id: &str) -> String {
    format!("{END_PREFIX}{id}")
}

/// `/etc/hosts`, or `DELONIX_HOSTS_FILE` (a test seam, and a way to keep an
/// isolated run away from the real file).
pub(crate) fn hosts_path() -> PathBuf {
    std::env::var_os("DELONIX_HOSTS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/hosts"))
}

/// One published name and the address it points at.
pub(crate) type Entry = (String, String);

/// The block for `hosts`, sorted and deduplicated. Empty for no hosts.
pub(crate) fn block(id: &str, hosts: &[Entry]) -> String {
    let mut h: Vec<Entry> = hosts
        .iter()
        .map(|(n, a)| (n.to_lowercase(), a.clone()))
        .collect();
    h.sort();
    h.dedup();
    if h.is_empty() {
        return String::new();
    }
    let mut out = format!("{}\n", begin(id));
    for (name, addr) in h {
        out.push_str(&format!("{addr}\t{name}\n"));
    }
    out.push_str(&end(id));
    out.push('\n');
    out
}

/// `existing` with the managed block replaced by the block for `hosts`. PURE.
pub(crate) fn render(existing: &str, hosts: &[Entry], id: &str) -> Result<String> {
    // Everything outside the block, in order.
    let mut kept: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in existing.lines() {
        // Only THIS root's block is ours. Another root's block stays where it is and its
        // names count as somebody else's entries below.
        if line.trim() == begin(id) {
            inside = true;
            continue;
        }
        if inside {
            if line.trim() == end(id) {
                inside = false;
            }
            continue;
        }
        kept.push(line);
    }
    // An unterminated block (a hand edit that removed the END line) is REFUSED: dropping
    // "up to the end of the file" would also erase whatever follows it — the hand-written
    // lines and the other state roots' blocks, none of which are ours.
    if inside {
        return Err(Error::Invalid(super::po::tf(
            "hosts: the delonix block of this state root in {path} has no END line (a hand edit?) — restore the line '{end}' or delete the block by hand, then apply again",
            &[
                ("path", &hosts_path().display().to_string()),
                ("end", &end(id)),
            ],
        )));
    }
    let wanted: Vec<String> = hosts.iter().map(|(h, _)| h.to_lowercase()).collect();
    // One name, two addresses is two answers to one question: refuse it rather than
    // let whichever line the resolver reads first win.
    let mut by_name: std::collections::BTreeMap<String, &str> = std::collections::BTreeMap::new();
    for (name, addr) in hosts {
        match by_name.insert(name.to_lowercase(), addr) {
            Some(prev) if prev != addr => {
                return Err(Error::Invalid(super::po::tf(
                    "hosts: '{name}' is published with two addresses ({a} and {b}) — two routes claim it from different pools",
                    &[("name", name), ("a", prev), ("b", addr)],
                )))
            }
            _ => {}
        }
    }
    for line in &kept {
        let body = line.split('#').next().unwrap_or("");
        let mut tokens = body.split_whitespace();
        if tokens.next().is_none() {
            continue;
        }
        if let Some(name) = tokens.find(|n| wanted.iter().any(|w| w == &n.to_lowercase())) {
            return Err(Error::Invalid(super::po::tf(
                "hosts: '{name}' already has an entry in {path} outside this state root's delonix block (a hand-written line, or another state root's block) — remove it or drop the name from the route: {line}",
                &[
                    ("name", name),
                    ("path", &hosts_path().display().to_string()),
                    ("line", line.trim()),
                ],
            )));
        }
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&block(id, hosts));
    Ok(out)
}

/// Brings the block in `/etc/hosts` in line with `hosts`. No write when nothing
/// would change, so an unprivileged `apply` of a manifest that does not use
/// `hosts:` never needs root.
pub(crate) fn sync(hosts: &[Entry]) -> Result<()> {
    sync_at(&hosts_path(), hosts, &root_id())
}

/// [`sync`] against an explicit file (the seam the test uses, so it never has to
/// touch the process environment).
fn sync_at(path: &std::path::Path, hosts: &[Entry], id: &str) -> Result<()> {
    let old = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::Invalid(format!("{}: {e}", path.display()))),
    };
    let new = render(&old, hosts, id)?;
    if new == old {
        return Ok(());
    }
    // Through the symlink, if any, so the link itself is not replaced by a file.
    let target = std::fs::canonicalize(path).unwrap_or(path.to_path_buf());
    let dir = target.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    // Two writers (two state roots, two applies) must not interleave a
    // read-modify-write: the second would erase the first's block.
    let _lock = lock_beside(&dir);
    // Re-read under the lock: what was read above may be stale by now.
    let old2 = std::fs::read_to_string(&target).unwrap_or_default();
    let new = if old2 == old {
        new
    } else {
        render(&old2, hosts, id)?
    };
    if new == old2 {
        return Ok(());
    }
    // A name nobody can guess and `create_new` (O_EXCL): a hostile local user cannot
    // pre-create it as a symlink to make this write land somewhere else.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".hosts.delonix.{}.{nanos}", std::process::id()));
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(new.as_bytes())?;
        if let Ok(meta) = std::fs::metadata(&target) {
            let _ = f.set_permissions(meta.permissions());
        }
        // The name→address map of the whole host: a crash between the rename and the
        // data reaching the disk must not leave an empty file behind.
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &target)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            Error::Invalid(super::po::tf(
                "hosts: cannot write {path} without root. Apply again as root, or add this block yourself:\n{block}",
                &[
                    ("path", &target.display().to_string()),
                    ("block", block(id, hosts).trim_end()),
                ],
            ))
        } else {
            Error::Invalid(format!("hosts: {}: {e}", target.display()))
        }
    })
}

/// Holds an exclusive `flock` on a lock file beside the hosts file for as long as the
/// returned guard lives. `None` when the lock file cannot be created (an unprivileged
/// run that will fail to write anyway, and says so with the block to add by hand).
fn lock_beside(dir: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".hosts.delonix.lock"))
        .ok()?;
    // SAFETY: a valid fd we own; released when `f` drops.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "aaaa0001";
    const OTHER: &str = "bbbb0002";

    fn h(v: &[&str]) -> Vec<Entry> {
        v.iter()
            .map(|s| (s.to_string(), "127.0.0.1".to_string()))
            .collect()
    }

    #[test]
    fn a_block_is_added_after_the_existing_lines_and_nothing_else_moves() {
        let out = render(
            "127.0.0.1\tlocalhost\n10.0.0.5 db\n",
            &h(&["b.pt", "a.pt"]),
            ID,
        )
        .unwrap();
        assert!(out.starts_with("127.0.0.1\tlocalhost\n10.0.0.5 db\n"));
        assert!(out.contains("127.0.0.1\ta.pt\n127.0.0.1\tb.pt\n"));
        assert!(out.trim_end().ends_with(&end(ID)));
    }

    #[test]
    fn a_name_can_point_at_a_reserved_address_and_two_addresses_for_one_name_are_refused() {
        let out = render("", &[("a.pt".into(), "203.0.113.7".into())], ID).unwrap();
        assert!(out.contains("203.0.113.7\ta.pt"));
        let e = render(
            "",
            &[
                ("a.pt".into(), "203.0.113.7".into()),
                ("A.pt".into(), "203.0.113.8".into()),
            ],
            ID,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("two addresses"), "{e}");
    }

    #[test]
    fn rendering_twice_is_the_same_as_once() {
        let once = render("127.0.0.1 localhost\n", &h(&["a.pt"]), ID).unwrap();
        assert_eq!(render(&once, &h(&["a.pt"]), ID).unwrap(), once);
    }

    #[test]
    fn no_hosts_removes_the_block_and_leaves_the_rest_byte_for_byte() {
        let base = "127.0.0.1 localhost\n10.0.0.5 db\n";
        let with = render(base, &h(&["a.pt"]), ID).unwrap();
        assert_eq!(render(&with, &[], ID).unwrap(), base);
        // and with no block to begin with, nothing is invented
        assert_eq!(render(base, &[], ID).unwrap(), base);
    }

    #[test]
    fn a_name_that_already_has_an_entry_is_refused() {
        let e = render(
            "10.0.0.9 app.example.pt other\n",
            &h(&["App.Example.pt"]),
            ID,
        )
        .unwrap_err();
        assert!(e.to_string().contains("outside this state root's"), "{e}");
        // a comment that merely mentions the name is not an entry
        assert!(render(
            "# app.example.pt is documented here\n",
            &h(&["app.example.pt"]),
            ID
        )
        .is_ok());
    }

    #[test]
    fn a_name_inside_our_own_block_is_not_a_conflict() {
        let with = render("127.0.0.1 localhost\n", &h(&["a.pt"]), ID).unwrap();
        assert!(render(&with, &h(&["a.pt", "b.pt"]), ID).is_ok());
    }

    #[test]
    fn an_unterminated_block_is_refused_and_takes_nothing_with_it() {
        let broken = format!(
            "127.0.0.1 localhost\n{}\n127.0.0.1\told.pt\n10.0.0.9 hand.written\n",
            begin(ID)
        );
        let e = render(&broken, &h(&["new.pt"]), ID)
            .unwrap_err()
            .to_string();
        assert!(e.contains("no END line"), "{e}");
    }

    #[test]
    fn two_state_roots_keep_their_own_blocks_and_never_erase_each_other() {
        let a = render("127.0.0.1 localhost\n", &h(&["a.pt"]), ID).unwrap();
        let both = render(&a, &h(&["b.pt"]), OTHER).unwrap();
        assert!(both.contains("\ta.pt\n") && both.contains("\tb.pt\n"));
        // the second root rebuilding with NOTHING to publish removes only its own block
        let after = render(&both, &[], OTHER).unwrap();
        assert!(
            after.contains("\ta.pt\n"),
            "the other root's names survived"
        );
        assert!(!after.contains("\tb.pt\n"));
        // and the first root emptying does not touch a block it does not own
        let after2 = render(&both, &[], ID).unwrap();
        assert!(after2.contains("\tb.pt\n") && !after2.contains("\ta.pt\n"));
    }

    #[test]
    fn a_name_published_by_another_root_is_refused_not_shadowed() {
        let a = render("127.0.0.1 localhost\n", &h(&["a.pt"]), ID).unwrap();
        let e = render(&a, &h(&["a.pt"]), OTHER).unwrap_err().to_string();
        assert!(e.contains("another state root"), "{e}");
    }

    #[test]
    fn sync_writes_the_file_and_is_a_no_op_the_second_time() {
        let dir = std::env::temp_dir().join(format!("dlx-hosts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("hosts");
        std::fs::write(&f, "127.0.0.1 localhost\n").unwrap();
        sync_at(&f, &h(&["a.pt"]), ID).unwrap();
        let first = std::fs::read_to_string(&f).unwrap();
        assert!(first.contains("127.0.0.1\ta.pt"));
        let m1 = std::fs::metadata(&f).unwrap().modified().unwrap();
        sync_at(&f, &h(&["a.pt"]), ID).unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), m1);
        sync_at(&f, &[], ID).unwrap();
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "127.0.0.1 localhost\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
