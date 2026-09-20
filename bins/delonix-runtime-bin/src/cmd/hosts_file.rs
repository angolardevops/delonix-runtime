//! The operator host's `/etc/hosts`, kept in step with the routes that asked for it
//! (`hosts: [host]`, ADR-0046).
//!
//! **One delimited block, and nothing else in the file is ever touched.** The block
//! is rewritten whole, so a name disappears the moment no route asks for it, with
//! nothing to reap. A name that already has an entry OUTSIDE the block is refused
//! rather than shadowed or overwritten: the operator wrote that line, and a second
//! answer for the same name is a silent change of where it points.
//!
//! The address is `127.0.0.1`: the proxy runs in the holder netns and reaches the
//! host through the slirp forward, which binds loopback by default. The PORT is the
//! route's entrypoint — a hosts file cannot carry one.
//!
//! Writing needs root. Without it the apply STOPS with the exact block to add, and
//! does not pretend: an unprivileged run is not a reason to skip the name silently.

use delonix_model::{Error, Result};
use std::path::PathBuf;

const BEGIN: &str = "# BEGIN delonix (managed — do not edit)";
const END: &str = "# END delonix";
const ADDR: &str = "127.0.0.1";

/// `/etc/hosts`, or `DELONIX_HOSTS_FILE` (a test seam, and a way to keep an
/// isolated run away from the real file).
pub(crate) fn hosts_path() -> PathBuf {
    std::env::var_os("DELONIX_HOSTS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/hosts"))
}

/// The block for `hosts`, sorted and deduplicated. Empty for no hosts.
pub(crate) fn block(hosts: &[String]) -> String {
    let mut h: Vec<String> = hosts.iter().map(|h| h.to_lowercase()).collect();
    h.sort();
    h.dedup();
    if h.is_empty() {
        return String::new();
    }
    let mut out = format!("{BEGIN}\n");
    for name in h {
        out.push_str(&format!("{ADDR}\t{name}\n"));
    }
    out.push_str(END);
    out.push('\n');
    out
}

/// `existing` with the managed block replaced by the block for `hosts`. PURE.
pub(crate) fn render(existing: &str, hosts: &[String]) -> Result<String> {
    // Everything outside the block, in order.
    let mut kept: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in existing.lines() {
        if line.trim() == BEGIN {
            inside = true;
            continue;
        }
        if inside {
            if line.trim() == END {
                inside = false;
            }
            continue;
        }
        kept.push(line);
    }
    // An unterminated block (a hand edit that removed the END line) is dropped up
    // to the end of the file. Refusing would leave the file impossible to fix
    // from here, and everything after BEGIN was ours to begin with.
    let wanted: Vec<String> = hosts.iter().map(|h| h.to_lowercase()).collect();
    for line in &kept {
        let body = line.split('#').next().unwrap_or("");
        let mut tokens = body.split_whitespace();
        if tokens.next().is_none() {
            continue;
        }
        if let Some(name) = tokens.find(|n| wanted.iter().any(|w| w == &n.to_lowercase())) {
            return Err(Error::Invalid(super::po::tf(
                "hosts: '{name}' already has an entry in {path} outside the delonix block — remove it or drop the name from the route: {line}",
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
    out.push_str(&block(hosts));
    Ok(out)
}

/// Brings the block in `/etc/hosts` in line with `hosts`. No write when nothing
/// would change, so an unprivileged `apply` of a manifest that does not use
/// `hosts:` never needs root.
pub(crate) fn sync(hosts: &[String]) -> Result<()> {
    let path = hosts_path();
    let old = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::Invalid(format!("{}: {e}", path.display()))),
    };
    let new = render(&old, hosts)?;
    if new == old {
        return Ok(());
    }
    // Through the symlink, if any, so the link itself is not replaced by a file.
    let target = std::fs::canonicalize(&path).unwrap_or(path.clone());
    let dir = target.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let tmp = dir.join(format!(".hosts.delonix.{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        std::fs::write(&tmp, &new)?;
        if let Ok(meta) = std::fs::metadata(&target) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, &target)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            Error::Invalid(super::po::tf(
                "hosts: cannot write {path} without root. Apply again as root, or add this block yourself:\n{block}",
                &[
                    ("path", &target.display().to_string()),
                    ("block", block(hosts).trim_end()),
                ],
            ))
        } else {
            Error::Invalid(format!("hosts: {}: {e}", target.display()))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_block_is_added_after_the_existing_lines_and_nothing_else_moves() {
        let out = render("127.0.0.1\tlocalhost\n10.0.0.5 db\n", &h(&["b.pt", "a.pt"])).unwrap();
        assert!(out.starts_with("127.0.0.1\tlocalhost\n10.0.0.5 db\n"));
        assert!(out.contains("127.0.0.1\ta.pt\n127.0.0.1\tb.pt\n"));
        assert!(out.trim_end().ends_with(END));
    }

    #[test]
    fn rendering_twice_is_the_same_as_once() {
        let once = render("127.0.0.1 localhost\n", &h(&["a.pt"])).unwrap();
        assert_eq!(render(&once, &h(&["a.pt"])).unwrap(), once);
    }

    #[test]
    fn no_hosts_removes_the_block_and_leaves_the_rest_byte_for_byte() {
        let base = "127.0.0.1 localhost\n10.0.0.5 db\n";
        let with = render(base, &h(&["a.pt"])).unwrap();
        assert_eq!(render(&with, &[]).unwrap(), base);
        // and with no block to begin with, nothing is invented
        assert_eq!(render(base, &[]).unwrap(), base);
    }

    #[test]
    fn a_name_that_already_has_an_entry_is_refused() {
        let e = render("10.0.0.9 app.example.pt other\n", &h(&["App.Example.pt"])).unwrap_err();
        assert!(e.to_string().contains("outside the delonix block"), "{e}");
        // a comment that merely mentions the name is not an entry
        assert!(render(
            "# app.example.pt is documented here\n",
            &h(&["app.example.pt"])
        )
        .is_ok());
    }

    #[test]
    fn a_name_inside_our_own_block_is_not_a_conflict() {
        let with = render("127.0.0.1 localhost\n", &h(&["a.pt"])).unwrap();
        assert!(render(&with, &h(&["a.pt", "b.pt"])).is_ok());
    }

    #[test]
    fn an_unterminated_block_is_replaced_not_stacked() {
        let broken = format!("127.0.0.1 localhost\n{BEGIN}\n127.0.0.1\told.pt\n");
        let out = render(&broken, &h(&["new.pt"])).unwrap();
        assert!(!out.contains("old.pt"));
        assert_eq!(out.matches(BEGIN).count(), 1);
    }

    #[test]
    fn sync_writes_the_file_and_is_a_no_op_the_second_time() {
        let dir = std::env::temp_dir().join(format!("dlx-hosts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("hosts");
        std::fs::write(&f, "127.0.0.1 localhost\n").unwrap();
        std::env::set_var("DELONIX_HOSTS_FILE", &f);
        sync(&h(&["a.pt"])).unwrap();
        let first = std::fs::read_to_string(&f).unwrap();
        assert!(first.contains("127.0.0.1\ta.pt"));
        let m1 = std::fs::metadata(&f).unwrap().modified().unwrap();
        sync(&h(&["a.pt"])).unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), m1);
        sync(&[]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "127.0.0.1 localhost\n"
        );
        std::env::remove_var("DELONIX_HOSTS_FILE");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
