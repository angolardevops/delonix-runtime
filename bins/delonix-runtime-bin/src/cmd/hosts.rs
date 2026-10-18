//! `delonix hosts` — the service names in the operator's `/etc/hosts` (ADR-0048, phase 2).
//!
//! The listings print the standard service name of every exposed container
//! (`<name>.<ns>.svc.delonix.internal`). A browser only opens it once the name resolves, and
//! nothing is written to `/etc/hosts` until the operator asks: `hosts sync` is that ask.
//! It reuses the per-state-root block of ADR-0046, so a name a route asked for with
//! `hosts: [host]` and a name published here share ONE block and one refusal rule.

use clap::Subcommand;
use delonix_model::Result;

#[derive(Subcommand, Debug)]
pub enum HostsCmd {
    /// Publish the service names of exposed containers in `/etc/hosts` and keep them current.
    ///
    /// Writes one delimited block (`# BEGIN delonix <root> … # END`) pointing every name at
    /// `127.0.0.1` — the L7 proxy's published port is how the host reaches a workload on the
    /// SDN. Rewriting `/etc/hosts` needs root: without it the command refuses and prints the
    /// block to add by hand. After the first run, `container run --expose` and `rm` keep the
    /// block current. When the file cannot be written that only warns — unless a route asked
    /// for names with `hosts:`, and then the operation fails instead. `--off` removes the block
    /// and stops maintaining it.
    Sync {
        /// Print the block that would be written and touch nothing.
        #[arg(long, conflicts_with = "off")]
        print: bool,
        /// Remove the block and stop keeping it current.
        #[arg(long)]
        off: bool,
    },
}

pub fn run(cmd: HostsCmd) -> Result<()> {
    // Writing `/etc/hosts` needs root, so the natural way to run this is `sudo delonix hosts
    // sync`. As root, `state_root()` is `/var/lib/delonix`: it would read root's (empty)
    // registrations and publish "0 service names", not the containers of the user who ran
    // `--expose`. The `vm bridge` had the same bug; it already has the fix. An explicit
    // `DELONIX_ROOT` still wins.
    super::vmbridge::adopt_invoking_user_root();
    match cmd {
        HostsCmd::Sync { print, off } => sync(print, off),
    }
}

/// The single line `--off` writes, decided from the two numbers that actually
/// determine what happened: how many names were published BEFORE the flag
/// moved, and how many a route still declares (`sync_hosts_now` writes those
/// back, so they stay).
///
/// Pure, and separate from the I/O, because all three outcomes are messages
/// about state and the old code could only say one of them. `path` comes from
/// `hosts_file::hosts_path()`, never the literal `/etc/hosts`.
fn off_message(published: usize, declared: &[String], path: &str) -> String {
    if !declared.is_empty() {
        return super::po::tf(
            "hosts: auto-publishing is off, but {n} name(s) stay in {path} — a route \
             still declares them ({list}); remove the route's `hosts:` to drop them",
            &[
                ("n", &declared.len().to_string()),
                ("path", path),
                ("list", &declared.join(", ")),
            ],
        );
    }
    if published == 0 {
        return super::po::tf(
            "hosts: auto-publishing is off; no service names were published in {path}, \
             so none were removed",
            &[("path", path)],
        );
    }
    super::po::tf(
        "hosts: {n} service name(s) removed from {path}",
        &[("n", &published.to_string()), ("path", path)],
    )
}

fn sync(print: bool, off: bool) -> Result<()> {
    use super::ingress_proxy as p;
    if print {
        print!("{}", p::hosts_block_now());
        return Ok(());
    }
    if off {
        // What is published RIGHT NOW, read before the flag moves — afterwards
        // the block is already the declared-only one and cannot answer this.
        // Measured 2026-10-18 in an isolated root with nothing exposed: `--off`
        // announced «service names removed from /etc/hosts» having written
        // nothing at all, and not even holding write permission on the file.
        // A removal of nothing is not a removal, and saying so is the same lie
        // this command was fixed for, one branch over.
        let published = p::hosts_block_now()
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .count();
        p::set_hosts_sync(false)?;
        p::sync_hosts_now()?;
        // `--off` stops the AUTO-registered names. A name an HTTPRoute declares
        // (`hosts:`) is not auto-registered, so `sync_hosts_now` writes it back
        // and the block survives — announcing a removal there was false. The
        // file is named from `hosts_path()`, which honours `DELONIX_HOSTS_FILE`,
        // because the literal `/etc/hosts` was wrong whenever it was set.
        let path = super::hosts_file::hosts_path().display().to_string();
        println!(
            "{}",
            off_message(published, &p::declared_host_names(), &path)
        );
        return Ok(());
    }
    // Writing first: on a refusal (no root) the flag must NOT be left on, or the next
    // `run --expose` would keep warning about a block the operator never accepted.
    p::set_hosts_sync(true)?;
    if let Err(e) = p::sync_hosts_now() {
        let _ = p::set_hosts_sync(false);
        return Err(e);
    }
    let block = p::hosts_block_now();
    let names = block.lines().filter(|l| !l.starts_with('#')).count();
    println!(
        "{}",
        super::po::tf(
            "hosts: {n} service name(s) published in {path}",
            &[
                ("n", &names.to_string()),
                (
                    "path",
                    &super::hosts_file::hosts_path().display().to_string()
                ),
            ],
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case the fix was measured on (#806): a run that published nothing
    /// announced a removal anyway. A removal of nothing is not a removal.
    #[test]
    fn nothing_published_says_so_instead_of_claiming_a_removal() {
        let m = off_message(0, &[], "/etc/hosts");
        assert!(
            m.contains("no service names were published") && m.contains("none were removed"),
            "must not claim a removal it did not make: {m}"
        );
    }

    /// The success path `--off` always had, now carrying the count so the
    /// operator can check it against the file.
    #[test]
    fn names_that_were_published_and_no_route_are_reported_as_removed() {
        let m = off_message(3, &[], "/etc/hosts");
        assert!(m.contains('3') && m.contains("removed"), "{m}");
        assert!(
            !m.contains("stay"),
            "nothing declares these, so nothing stays: {m}"
        );
    }

    /// The original #806: a name an HTTPRoute declares is written back by
    /// `sync_hosts_now`, so the block SURVIVES `--off`. Announcing a removal
    /// there was false; the names that stay are listed by name.
    #[test]
    fn names_a_route_declares_are_reported_as_staying_not_removed() {
        let declared = vec!["a.example".to_string(), "b.example".to_string()];
        let m = off_message(5, &declared, "/etc/hosts");
        assert!(m.contains("stay"), "{m}");
        assert!(m.contains("a.example") && m.contains("b.example"), "{m}");
        assert!(
            !m.contains("removed"),
            "the block survives, so it must not say removed: {m}"
        );
        assert!(
            m.contains('2') && !m.contains('5'),
            "the count is the names that STAY, not what was published: {m}"
        );
    }

    /// `DELONIX_HOSTS_FILE` moves the file; the message named the literal
    /// `/etc/hosts` regardless, which was wrong whenever it was set.
    #[test]
    fn the_message_names_the_file_it_was_given() {
        for m in [
            off_message(0, &[], "/tmp/other-hosts"),
            off_message(2, &[], "/tmp/other-hosts"),
            off_message(1, &["x.example".to_string()], "/tmp/other-hosts"),
        ] {
            assert!(m.contains("/tmp/other-hosts"), "{m}");
            assert!(!m.contains("/etc/hosts"), "{m}");
        }
    }
}
