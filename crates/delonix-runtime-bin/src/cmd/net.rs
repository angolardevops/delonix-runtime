//! `delonix net <x>` — low-level network/infra plumbing, grouped under one
//! root instead of separate top-level commands (`netns`/`flow`/`ingress`/
//! `egress`/`httproute`/`tunnel`) — a flat root command list was hard to
//! scan; these are all "operate on the rootless ingress/SDN plumbing", the
//! natural sibling of `delonix network` (user-facing SDN networks) rather
//! than a peer of `container`/`image`/`vm`. `boot` moved to `delonix system
//! boot` (B2 of the CLI restructuring): boot persistence is about the ENGINE
//! surviving a reboot, not about SDN plumbing, and it never belonged here.
//! Pure routing — each subcommand delegates to the SAME per-group module/
//! `run()` this always had; no behavior changed, only the CLI path to reach it.

use clap::Subcommand;
use delonix_runtime_core::Result;

#[derive(Subcommand)]
pub enum NetCmd {
    /// Low-level management of the rootless ingress infra (up/status/attach/publish/firewall).
    Netns {
        #[command(subcommand)]
        action: super::netns::NetnsCmd,
    },
    /// Live per-container traffic (eBPF datapath; degrades to veth counters).
    Flow {
        /// Watch only this interface (default: auto — every SDN veth).
        #[arg(long)]
        iface: Option<String>,
        /// Refresh continuously (every 2s) instead of printing once.
        #[arg(long, short)]
        watch: bool,
    },
    /// Raw packet capture on a container's SDN interface, via the host's own `tcpdump`.
    ///
    /// Runs `tcpdump` INSIDE the container's netns (the same `join_argv`
    /// prefix `--net <network>` uses to enter one), never a second capture
    /// engine. Containers only in this version — a pod member shares its
    /// pod's netns and is refused, pointing at the pod's name.
    Capture {
        /// Container to capture (must be attached to a custom network —
        /// `--net host`/`none` have no netns of their own to enter).
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::containers))]
        container: String,
        /// Interface inside the container's netns.
        #[arg(short = 'i', long, default_value = "eth0")]
        iface: String,
        /// Where to write the capture: a file path, or `-` for stdout.
        #[arg(short = 'w', long)]
        write: String,
        /// Stop after this many packets.
        #[arg(short = 'c', long)]
        count: Option<u32>,
        /// Stop after this many seconds (a real `SIGINT`, same as Ctrl-C — ignored if `--count` is reached first).
        #[arg(long)]
        duration: Option<u64>,
    },
    /// INBOUND firewall (L4 rules + DNAT publishes) for a container on the SDN.
    Ingress {
        #[command(subcommand)]
        action: super::firewall::IngressCmd,
    },
    /// OUTBOUND firewall (L4 rules + per-network egress policy) for a container.
    Egress {
        #[command(subcommand)]
        action: super::firewall::EgressCmd,
    },
    /// Ingress-wide L4 DDoS guard (per-source connection rate + concurrent cap).
    L4guard {
        #[command(subcommand)]
        action: super::firewall::L4guardCmd,
    },
    /// Embedded L7/HTTP reverse-proxy (`kind: HTTPRoute`): ls/apply/rm.
    Httproute {
        #[command(subcommand)]
        action: super::httproute::HttpRouteCmd,
    },
    /// Expose a local port to the public internet (`kind: Gateway`).
    ///
    /// Via pinggy/ngrok/cloudflare — pair with `httproute`'s listening port to
    /// route by Host header behind ONE public URL.
    Tunnel {
        #[command(subcommand)]
        action: super::tunnel::TunnelCmd,
    },
}

pub fn run(action: NetCmd) -> Result<()> {
    match action {
        NetCmd::Netns { action } => super::netns::run(action),
        NetCmd::Flow { iface, watch } => super::flow::run(iface, watch),
        NetCmd::Capture {
            container,
            iface,
            write,
            count,
            duration,
        } => super::capture::run(&container, &iface, &write, count, duration),
        NetCmd::Ingress { action } => super::firewall::run_ingress(action),
        NetCmd::Egress { action } => super::firewall::run_egress(action),
        NetCmd::L4guard { action } => super::firewall::run_l4guard(action),
        NetCmd::Httproute { action } => super::httproute::run(action),
        NetCmd::Tunnel { action } => super::tunnel::run(action),
    }
}
