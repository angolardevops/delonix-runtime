//! `delonix netns` — low-level management of the rootless ingress infra (the
//! holder netns + `delonix0` bridge + single slirp). This is the plumbing that
//! `container run --net <network>` uses under the hood; exposing it helps debug
//! the network path directly (attach a netns, publish a port, inspect state).
//!
//! The hidden `netns holder` / `netns run <spec>` re-execs are intercepted in
//! `main` BEFORE clap (they're internal, not user-facing), so they don't appear
//! here — only the operational subcommands do.

use clap::Subcommand;
use delonix_net::infra;
use delonix_runtime_core::{ContainerFw, Error, Result};

#[derive(Subcommand)]
pub enum NetnsCmd {
    /// Bring the ingress infra up (idempotent): holder netns + delonix0 + single slirp.
    Up,
    /// Show the ingress infra status (holder/slirp pids, bridge, refcount).
    Status {
        /// Emit JSON instead of a human summary.
        #[arg(long)]
        json: bool,
    },
    /// Force tear-down of the ingress infra (kills slirp + holder, frees the netns).
    Down,
    /// Attach a netns to delonix0 via veth (the holder is the netns/veth factory).
    Attach {
        /// Netns name (typically a container id/short-id).
        name: String,
        /// IP in the infra subnet. Defaults to a deterministic one derived from `name`.
        #[arg(long)]
        ip: Option<String>,
    },
    /// Detach (and destroy) a previously attached netns.
    Detach { name: String },
    /// Run a command inside an attached netns (exercises the runtime join path).
    Exec {
        name: String,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Publish a port through the ingress (add_hostfwd + DNAT) to a container.
    Publish {
        /// Netns/container name (its IP is derived unless `--ip` is given).
        name: String,
        /// Port mapping `hostPort:containerPort[/tcp|udp]`.
        spec: String,
        /// Override the container IP (defaults to the deterministic one from `name`).
        #[arg(long)]
        ip: Option<String>,
    },
    /// Unpublish a host port from the ingress.
    Unpublish { host_port: String },
    /// Apply (or clear) a container's parameterizable firewall AT THE INGRESS.
    Firewall {
        /// Netns/container name.
        name: String,
        /// ContainerFw as JSON, e.g. `{"enabled":true,"policyIn":"deny","rules":[...]}`.
        #[arg(long, conflicts_with = "clear")]
        spec: Option<String>,
        /// Remove the container's firewall from the ingress.
        #[arg(long)]
        clear: bool,
    },
}

pub fn run(action: NetnsCmd) -> Result<()> {
    match action {
        NetnsCmd::Up => {
            if !delonix_runtime::is_rootless() {
                println!("ingress: in root mode the single ingress already exists (nft DNAT on the host); the infra netns is rootless-only.");
                return Ok(());
            }
            infra::ensure_up()?;
            let st = infra::status();
            println!(
                "ingress UP — holder pid {} · slirp pid {} · bridge {} ({})",
                fmt_pid(st.holder_pid),
                fmt_pid(st.slirp_pid),
                st.bridge,
                st.gateway,
            );
            Ok(())
        }
        NetnsCmd::Status { json } => {
            let st = infra::status();
            if json {
                println!("{}", serde_json::to_string_pretty(&st).unwrap_or_default());
            } else {
                println!(
                    "ingress {} — holder {} · slirp {} · bridge {} ({}) · refcount {}",
                    if st.up { "UP" } else { "DOWN" },
                    fmt_pid(st.holder_pid),
                    fmt_pid(st.slirp_pid),
                    st.bridge,
                    st.gateway,
                    st.refcount,
                );
            }
            Ok(())
        }
        NetnsCmd::Down => {
            infra::teardown();
            println!("ingress DOWN — infra netns torn down.");
            Ok(())
        }
        NetnsCmd::Attach { name, ip } => {
            let (netns, assigned) = infra::attach_container(&name, "ingress", "default")?;
            println!(
                "attached '{netns}' → {} on {} (refcount {})",
                ip.unwrap_or(assigned),
                infra::INFRA_BRIDGE,
                infra::status().refcount
            );
            Ok(())
        }
        NetnsCmd::Detach { name } => {
            infra::detach_container(&name, &infra::container_ip(&name));
            println!("detached '{name}' (refcount {})", infra::status().refcount);
            Ok(())
        }
        NetnsCmd::Exec { name, command } => {
            let argv = infra::join_argv(&name).ok_or_else(|| Error::Runtime {
                context: "ingress",
                message: "infra is not up".into(),
            })?;
            let status = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .args(&command)
                .status()
                .map_err(|e| Error::Runtime {
                    context: "netns exec",
                    message: e.to_string(),
                })?;
            std::process::exit(status.code().unwrap_or(1));
        }
        NetnsCmd::Publish { name, spec, ip } => {
            let cip = ip.unwrap_or_else(|| infra::container_ip(&name));
            infra::publish_port(&cip, &spec)?;
            println!("published {spec} → {cip} through the ingress");
            Ok(())
        }
        NetnsCmd::Unpublish { host_port } => {
            infra::unpublish_port(&host_port);
            println!("unpublished host port {host_port}");
            Ok(())
        }
        NetnsCmd::Firewall { name, spec, clear } => {
            let ip = infra::container_ip(&name);
            if clear {
                infra::clear_firewall(&ip);
                println!("ingress firewall removed for '{name}'");
                return Ok(());
            }
            let json =
                spec.ok_or_else(|| Error::Invalid("missing --spec <json> or --clear".into()))?;
            let fw: ContainerFw = serde_json::from_str(&json)
                .map_err(|e| Error::Invalid(format!("firewall JSON: {e}")))?;
            infra::apply_firewall(&name, &ip, &fw)?;
            println!(
                "ingress firewall applied for '{name}' ({} rule(s))",
                fw.rules.len()
            );
            Ok(())
        }
    }
}

fn fmt_pid(p: Option<i32>) -> String {
    p.map(|p| p.to_string()).unwrap_or_else(|| "—".into())
}
