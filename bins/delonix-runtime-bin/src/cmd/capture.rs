//! `delonix net capture <container>` — raw packet capture on a container's
//! SDN interface.
//!
//! No eBPF/`pcap`/`AF_PACKET` of our own: this reuses the exact
//! `infra::join_argv(sanitize(container_id))` prefix `container run --net
//! <network>` already uses to enter a netns without new privilege, and
//! invokes the HOST's own `tcpdump` inside it (`tcpdump -i <iface> -w -`),
//! inheriting stdio so `-w -`/redirection behave exactly like the real
//! `tcpdump`. `CAP_NET_RAW` is already in every container's `KEPT_CAPS` by
//! default — nothing new to grant; the process here enters as root mapped in
//! the HOLDER's userns (`-U -m -n`), which already has full capabilities
//! there regardless.
//!
//! **v1 scope: containers only, not pods.** A pod member's netns is the
//! pod's, shared with its peers — `join_argv(member_id)` would enter a netns
//! named after the CONTAINER, which does not exist (the exact bug already
//! documented at `cmd/container.rs`'s `reexec_start`, in the same class as
//! `--net <custom>` vs `--pod`). Rather than repeat that mistake, a pod
//! member is refused here, pointing at the pod's name.
//!
//! **The one real blocker is a HOST dependency, not an engine one**:
//! `tcpdump` is not something `install.sh` installs by default (see
//! `install.sh`'s `tcpdump` entry, added alongside this feature as an
//! OPTIONAL dependency — same category as `virt-customize`/`libguestfs-tools`,
//! only needed for this one feature, never for the base engine). Checked with
//! a clear, actionable preflight (naming the package for the host's actual
//! package manager) BEFORE any `nsenter`, never a raw `ENOENT`.

use std::process::{Command, Stdio};

use delonix_runtime_core::{Error, Result};

use super::util::open_stores;

/// `tcpdump --version` on the HOST — the netns we `nsenter` into shares the
/// holder's mount namespace (a shallow copy of the host's at holder-start
/// time, not a different filesystem), so a host-side check is equivalent to
/// checking from inside it, and it runs before any privilege is touched.
fn ensure_tcpdump() -> Result<()> {
    if Command::new("tcpdump")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return Ok(());
    }
    let fam = super::vmimage::host_family();
    let cmd = super::vmimage::install_cmd(fam, "tcpdump", "tcpdump", "tcpdump");
    Err(Error::Invalid(format!(
        "tcpdump is not installed on this host (network capture runs it here, not inside the container).\nFix:\n  {cmd}"
    )))
}

/// `net capture <container> [-i <iface>] -w <file|-> [-c <N>] [--duration <s>]`.
///
/// Foreground: blocks until `tcpdump` exits — naturally at `--count`, after
/// `--duration` (a watcher thread sends it a real `SIGINT` so it flushes the
/// capture file cleanly, exactly like pressing Ctrl-C at a terminal), or at
/// an actual Ctrl-C when neither is given.
#[allow(clippy::too_many_arguments)]
pub fn run(
    container: &str,
    iface: &str,
    write: &str,
    count: Option<u32>,
    duration: Option<u64>,
) -> Result<()> {
    ensure_tcpdump()?;
    let (_, store) = open_stores()?;
    let target = super::util::find(&store, container)?;
    if let Some(pod) = &target.pod {
        // `Container.pod` stores the netns name (`pod_netns_name`, "pod-<name>"), not the
        // pod's own name as `pod ls`/`pod exec` take it — strip the prefix so the error
        // names something the operator can actually act on. Falls back to the raw value if
        // the format ever changes, rather than mangling an unrecognized string.
        let pod_name = pod.strip_prefix("pod-").unwrap_or(pod);
        return Err(Error::Invalid(format!(
            "'{container}' is a member of pod '{pod_name}' — capturing on a pod isn't supported \
             yet; this needs its own netns, and a pod member shares the pod's"
        )));
    }
    if target.network.is_none() {
        return Err(Error::Invalid(format!(
            "'{container}' has no netns of its own (`--net host`/`none`) — nothing to capture on"
        )));
    }
    let netns = delonix_net::infra::sanitize(&target.id);
    let prefix = delonix_net::infra::join_argv(&netns).ok_or_else(|| Error::Runtime {
        context: "join_argv",
        message: super::po::t("ingress infra is down — no holder to enter").into(),
    })?;

    let mut args: Vec<String> = vec!["-i".into(), iface.into(), "-w".into(), write.into()];
    if let Some(n) = count {
        args.push("-c".into());
        args.push(n.to_string());
    }

    let mut child = Command::new(&prefix[0])
        .args(&prefix[1..])
        .arg("tcpdump")
        .args(&args)
        .spawn()
        .map_err(|e| Error::Runtime {
            context: "tcpdump spawn",
            message: e.to_string(),
        })?;

    if let Some(secs) = duration {
        let pid = child.id() as i32;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            // SAFETY: `pid` is a plain libc call with no aliasing/lifetime
            // requirements; a `kill` on an already-exited pid returns ESRCH,
            // harmless and ignored — the `--count` case races this exactly.
            unsafe {
                libc::kill(pid, libc::SIGINT);
            }
        });
    }

    let status = child.wait().map_err(|e| Error::Runtime {
        context: "tcpdump wait",
        message: e.to_string(),
    })?;
    // A `SIGINT` we sent ourselves (the `--duration` timeout) is the SUCCESS
    // path, not a crash — `tcpdump` exits non-zero on a signal exactly like
    // any other Unix tool, and the operator watching a clean capture finish
    // should never see a non-zero exit code they didn't cause.
    if status.success() || (duration.is_some() && status.code().is_none()) {
        Ok(())
    } else {
        Err(Error::Runtime {
            context: "tcpdump",
            message: format!("exited with {status:?}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_tcpdump_names_the_package_when_missing() {
        // `tcpdump` is not guaranteed present in a CI/test sandbox; this only
        // asserts the SHAPE of the failure when it is indeed missing, never
        // that it must be missing here.
        if Command::new("tcpdump").arg("--version").output().is_ok() {
            return;
        }
        let e = ensure_tcpdump().unwrap_err().to_string();
        assert!(e.contains("tcpdump"), "{e}");
        assert!(e.contains("install"), "{e}");
    }
}
