//! The run specification checked on its own, before any side effect.
//!
//! Every check here reads only the [`RunOpts`] — no store, no host, no network —
//! so a combination of flags that cannot mean anything is refused before an
//! image is pulled, an address is leased or a process exists.

use crate::RunOpts;
use delonix_runtime_core::{Error, Result};

/// The custom network a `--net` value names: `host` and `none` are the two
/// built-in modes, anything else is a network. PURE.
pub fn custom_net_name(net: &str) -> Option<String> {
    (net != "host" && net != "none").then(|| net.to_string())
}

/// Refuses the flag combinations that cannot mean anything. PURE.
///
/// `--net-bps` without a custom network was checked only AFTER the workload had
/// been created: in the foreground the process ran to completion and the command
/// then failed, leaving the record behind; with `-d` the supervisor returned
/// first and the flag was accepted and silently ignored. Both measured.
pub fn check_run_opts(o: &RunOpts) -> Result<()> {
    if o.net_burst.is_some() && o.net_bps.is_none() {
        return Err(Error::Invalid(
            "--net-burst only makes sense together with --net-bps".into(),
        ));
    }
    let custom = custom_net_name(&o.net);
    if o.net_bps.is_some() && custom.is_none() {
        return Err(Error::Invalid(
            "--net-bps only applies with `--net <network>` (shaping is on the ingress veth)".into(),
        ));
    }
    // `--ip` with `--net host/none` has no SDN address to fix, and accepting it
    // there would silently do nothing.
    if o.ip.is_some() && custom.is_none() {
        return Err(Error::Invalid(
            "--ip requires --net <network> — a fixed address only makes sense on the SDN, not with --net host/none"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(net: &str) -> RunOpts {
        RunOpts {
            net: net.into(),
            ..Default::default()
        }
    }

    #[test]
    fn host_and_none_are_not_custom_networks() {
        assert_eq!(custom_net_name("host"), None);
        assert_eq!(custom_net_name("none"), None);
        assert_eq!(custom_net_name("pnet"), Some("pnet".to_string()));
    }

    #[test]
    fn net_bps_needs_a_custom_network() {
        let mut o = opts("none");
        o.net_bps = Some("1mbit".into());
        assert!(check_run_opts(&o).is_err());
        o.net = "host".into();
        assert!(check_run_opts(&o).is_err());
        o.net = "pnet".into();
        assert!(check_run_opts(&o).is_ok());
    }

    #[test]
    fn net_burst_needs_net_bps() {
        let mut o = opts("pnet");
        o.net_burst = Some("32kb".into());
        assert!(check_run_opts(&o).is_err());
        o.net_bps = Some("1mbit".into());
        assert!(check_run_opts(&o).is_ok());
    }

    #[test]
    fn a_fixed_ip_needs_a_custom_network() {
        let mut o = opts("host");
        o.ip = Some("10.200.0.9".into());
        assert!(check_run_opts(&o).is_err());
        o.net = "pnet".into();
        assert!(check_run_opts(&o).is_ok());
        assert!(check_run_opts(&opts("host")).is_ok());
    }
}
