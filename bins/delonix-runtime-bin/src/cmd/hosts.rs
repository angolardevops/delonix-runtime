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

fn sync(print: bool, off: bool) -> Result<()> {
    use super::ingress_proxy as p;
    if print {
        print!("{}", p::hosts_block_now());
        return Ok(());
    }
    if off {
        p::set_hosts_sync(false)?;
        p::sync_hosts_now()?;
        println!(
            "{}",
            super::po::t("hosts: service names removed from /etc/hosts")
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
