//! `delonix flow` — live per-container traffic, from the eBPF datapath.
//!
//! When the eBPF observability datapath is available (privileged run), this
//! attaches the accounting classifiers to `delonix0` (in the ingress netns) and
//! shows per-container RX/TX read from the BPF map. When it isn't (the common
//! rootless case), it says so plainly and falls back to the per-container veth
//! counters that always work — the numbers are coarser but nothing is hidden.

use std::net::Ipv4Addr;
use std::process::Command;

use delonix_net::{bpf, infra};
use delonix_runtime_core::Result;

use super::output;
use super::util::open_stores;

pub fn run(iface: Option<String>) -> Result<()> {
    let (_images, store) = open_stores()?;
    let containers = store.list().unwrap_or_default();
    // IP → container name, for labelling the flows.
    let name_of: std::collections::HashMap<String, String> =
        containers.iter().filter_map(|c| c.ip.clone().filter(|s| !s.is_empty()).map(|ip| (ip, c.name.clone()))).collect();

    if !bpf::available() {
        output::warn("eBPF observability inactive");
        println!("  the flow datapath needs CAP_BPF + CAP_NET_ADMIN (run privileged / root install);");
        println!("  the nft firewall and the SDN are unaffected. Falling back to veth counters.\n");
        return fallback(&containers);
    }

    // Privileged: enter ONLY the infra netns (keep init-ns caps) to attach.
    let netns = infra::infra_netns_argv();
    let run_cmd = |args: &[&str]| -> bool {
        let mut c = wrap(&netns, args);
        // tc prints to stderr on the pre-clean `qdisc del`; keep it quiet.
        c.stderr(std::process::Stdio::null());
        c.status().map(|s| s.success()).unwrap_or(false)
    };
    if netns.is_some() {
        // Attach the datapath to the bridge PORTS (the container veths), not the
        // bridge master — a bridge master's clsact doesn't see bridged frames.
        // `--iface` overrides for debugging a single device. Idempotent.
        let targets = match &iface {
            Some(i) => vec![i.clone()],
            None => infra_veths(&netns),
        };
        for t in &targets {
            bpf::attach(t, &run_cmd);
        }
    }
    // The map is a global kernel object — read it directly (no netns needed).
    let flows = bpf::flows(capture);
    if flows.is_empty() {
        println!("datapath attached — no flows yet (generate some traffic and re-run `delonix flow`).");
        return Ok(());
    }

    let mut rows: Vec<(String, Ipv4Addr, bpf::Flow)> = flows
        .into_iter()
        .map(|(ip, f)| (name_of.get(&ip.to_string()).cloned().unwrap_or_else(|| "-".into()), ip, f))
        .collect();
    rows.sort_by(|a, b| (b.2.rx_bytes + b.2.tx_bytes).cmp(&(a.2.rx_bytes + a.2.tx_bytes)));

    let mut t = output::Table::new(&["CONTAINER", "IP", "RX PACKETS", "RX BYTES", "TX PACKETS", "TX BYTES"]);
    for (name, ip, f) in rows {
        t.row(vec![
            name,
            ip.to_string(),
            f.rx_packets.to_string(),
            human_bytes(f.rx_bytes),
            f.tx_packets.to_string(),
            human_bytes(f.tx_bytes),
        ]);
    }
    t.print();
    Ok(())
}

/// Per-container byte counters from the veth (always available, coarser).
fn fallback(containers: &[delonix_runtime_core::Container]) -> Result<()> {
    let mut t = output::Table::new(&["CONTAINER", "IP", "RX BYTES", "TX BYTES"]);
    let mut any = false;
    for c in containers {
        if let Some((rx, tx)) = infra::container_net_bytes(&c.id) {
            t.row(vec![c.name.clone(), c.ip.clone().unwrap_or_else(|| "-".into()), human_bytes(rx), human_bytes(tx)]);
            any = true;
        }
    }
    if any {
        t.print();
    } else {
        println!("no per-container counters available (no containers on the SDN).");
    }
    Ok(())
}

/// The bridge-port veth interfaces (`vh*`) inside the infra netns — the correct
/// attach points for per-container accounting. Empty if the holder is down.
fn infra_veths(netns: &Option<Vec<String>>) -> Vec<String> {
    let mut c = wrap(netns, &["ip", "-o", "link", "show", "type", "veth"]);
    let out = match c.output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| {
            // `5: vh25c683d0@if4: <...>` → `vh25c683d0`
            let name = l.split(':').nth(1)?.trim().split('@').next()?.trim();
            name.starts_with("vh").then(|| name.to_string())
        })
        .collect()
}

/// Build a `Command` for `args`, prefixed with the netns-enter argv when set.
fn wrap(prefix: &Option<Vec<String>>, args: &[&str]) -> Command {
    match prefix {
        Some(p) => {
            let mut c = Command::new(&p[0]);
            c.args(&p[1..]).args(args);
            c
        }
        None => {
            let mut c = Command::new(args[0]);
            c.args(&args[1..]);
            c
        }
    }
}

/// Run `args` and capture stdout (the flow map lives in the global kernel BPF
/// namespace, so no netns wrapper is needed to read it).
fn capture(args: &[&str]) -> Option<String> {
    let out = Command::new(args[0]).args(&args[1..]).output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

/// `1.5 KiB`-style humanised bytes.
fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}
