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
        NetCmd::Ingress { action } => super::firewall::run_ingress(action),
        NetCmd::Egress { action } => super::firewall::run_egress(action),
        NetCmd::L4guard { action } => super::firewall::run_l4guard(action),
        NetCmd::Httproute { action } => super::httproute::run(action),
        NetCmd::Tunnel { action } => super::tunnel::run(action),
    }
}
