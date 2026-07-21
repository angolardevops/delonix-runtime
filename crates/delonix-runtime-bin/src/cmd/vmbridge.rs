//! `delonix vm bridge`/`unbridge` — **EXPERIMENTAL, privileged, opt-in**.
//!
//! Gives a libvirt VM DIRECT L3 reachability to a container SDN network's IPs
//! (and back), by stitching a `veth` pair from the host netns into the holder
//! netns (where the SDN bridge lives) plus the routes/forwarding. This is the
//! ONE thing the rootless model can't do on its own: the SDN bridge
//! (`delonix0`/`dlxn…`) lives inside the holder's `unshare --user --net`
//! namespace, unreachable from the host without `CAP_NET_ADMIN` in the host
//! init-netns. So this command REQUIRES root (or an equivalent privileged run) —
//! it is the deliberate, documented exception to daemonless-rootless, gated
//! behind `--apply` and defaulting to a DRY-RUN that only prints the plan.
//!
//! Security: it opens VM↔container on that network. The VM subnet is the
//! libvirt NAT network (e.g. `192.168.122.0/24`), NOT the external LAN, so the
//! blast radius is "VMs on that libvirt network + the host", not the internet.
//! The container's per-container nft chain still governs the traffic (a VM IP
//! is not in `@dlxall`, so namespace isolation lets it through like a gateway;
//! explicit `ingress` rules still apply). `unbridge` tears it all down.
//!
//! NOTE: shipped on a feature branch, NOT merged/released — it has not been
//! run end-to-end in this dev sandbox (no root here). The command GENERATION is
//! pure and unit-tested; the privileged execution is validated by the operator
//! on a real host (dry-run first).

use std::path::Path;
use std::process::Command;

use delonix_net::infra;
use delonix_runtime_core::{Error, Result};

use super::output;
use super::util::state_root;

/// FNV-1a 32-bit — a stable short hash for deterministic interface names
/// (same network → same veth names across runs, so `unbridge` finds them).
fn fnv32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Deterministic `veth` names for a network, both ≤ 15 chars (IFNAMSIZ): the
/// host end (`vbh…`) and the SDN end (`vbs…`, moved into the holder netns).
fn veth_names(network: &str) -> (String, String) {
    let h = fnv32(network);
    (format!("vbh{h:08x}"), format!("vbs{h:08x}"))
}

/// The host-side SDN address of the bridge link: `<prefix>.255.254`, high in
/// the /16 to avoid colliding with DHCP-assigned container IPs (which grow from
/// `.0.2` up). Pure.
fn host_sdn_ip(prefix: &str) -> String {
    format!("{prefix}.255.254")
}

/// Builds the ordered list of privileged commands (argv each) that establish
/// the bridge. PURE — unit-tested without touching the network. `vm_subnets`
/// are the libvirt VM subnets that must route back through the holder.
fn bridge_plan(
    holder_pid: &str,
    bridge: &str,
    prefix: &str,
    vm_subnets: &[String],
) -> Vec<Vec<String>> {
    let (vh, vs) = veth_names(bridge); // keyed on the SDN bridge (1 per network)
    let host_ip = host_sdn_ip(prefix);
    let host_cidr = format!("{host_ip}/16");
    let nsenter = |args: &[&str]| -> Vec<String> {
        let mut v = vec![
            "nsenter".into(),
            "-t".into(),
            holder_pid.into(),
            "-U".into(),
            "-n".into(),
            "--preserve-credentials".into(),
            "--".into(),
        ];
        v.extend(args.iter().map(|s| s.to_string()));
        v
    };
    let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let mut plan = vec![
        // 1. veth pair in the host netns.
        s(&[
            "ip", "link", "add", &vh, "type", "veth", "peer", "name", &vs,
        ]),
        // 2. move the SDN end into the holder netns.
        s(&["ip", "link", "set", &vs, "netns", holder_pid]),
        // 3. inside the holder: enslave it to the SDN bridge + up.
        nsenter(&["ip", "link", "set", &vs, "master", bridge, "up"]),
        // 4-5. host end gets an SDN address and comes up.
        s(&["ip", "addr", "add", &host_cidr, "dev", &vh]),
        s(&["ip", "link", "set", &vh, "up"]),
        // 6. host forwards between virbr0 and the SDN link.
        s(&["sysctl", "-w", "net.ipv4.ip_forward=1"]),
    ];
    // 7. return route inside the holder: VM subnets via the host end.
    for sub in vm_subnets {
        plan.push(nsenter(&["ip", "route", "add", sub, "via", &host_ip]));
    }
    plan
}

/// The teardown plan (best-effort; each command tolerated if already gone).
fn unbridge_plan(
    holder_pid: &str,
    bridge: &str,
    prefix: &str,
    vm_subnets: &[String],
) -> Vec<Vec<String>> {
    let (vh, _vs) = veth_names(bridge);
    let host_ip = host_sdn_ip(prefix);
    let nsenter = |args: &[&str]| -> Vec<String> {
        let mut v = vec![
            "nsenter".into(),
            "-t".into(),
            holder_pid.into(),
            "-U".into(),
            "-n".into(),
            "--preserve-credentials".into(),
            "--".into(),
        ];
        v.extend(args.iter().map(|s| s.to_string()));
        v
    };
    let mut plan = Vec::new();
    for sub in vm_subnets {
        plan.push(nsenter(&["ip", "route", "del", sub, "via", &host_ip]));
    }
    // Deleting the host end removes the whole pair (and its netns peer).
    plan.push(vec![
        "ip".into(),
        "link".into(),
        "del".into(),
        vh,
        // no-op tolerance handled by the runner
    ]);
    plan
}

/// Extracts the home directory (field 6) from a `getent passwd` line. Pure.
fn home_from_passwd_line(line: &str) -> Option<String> {
    let home = line.trim().split(':').nth(5)?.trim();
    (!home.is_empty()).then(|| home.to_string())
}

/// Under `sudo`, point Delonix state resolution at the INVOKING user's data dir,
/// not root's. The network defs (`resolve_net`) and the holder PID live in
/// `~<user>/.local/share/delonix`; run as root, `state_root()`/`base_root()`
/// would resolve to `/var/lib/delonix` and NOT find `kaeso-net` (real bug:
/// `sudo delonix vm bridge` → "network does not exist"). Honors an explicit
/// `DELONIX_ROOT` if already set (power users / custom `XDG_DATA_HOME`).
fn adopt_invoking_user_root() {
    if std::env::var_os("DELONIX_ROOT").is_some() {
        return;
    }
    let Some(user) = std::env::var_os("SUDO_USER") else {
        return; // not under sudo — state_root() is already the user's
    };
    if let Ok(out) = Command::new("getent").arg("passwd").arg(&user).output() {
        if out.status.success() {
            if let Some(home) = home_from_passwd_line(&String::from_utf8_lossy(&out.stdout)) {
                let root = Path::new(&home).join(".local/share/delonix");
                std::env::set_var("DELONIX_ROOT", root);
            }
        }
    }
}

/// Reads the holder PID (the netns target) — the infra must be UP.
fn holder_pid() -> Result<String> {
    let p = state_root().join("ingress").join("holder.pid");
    let pid = std::fs::read_to_string(&p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.parse::<i32>().is_ok())
        .filter(|s| Path::new(&format!("/proc/{s}")).exists());
    pid.ok_or_else(|| {
        Error::Invalid(
            "the ingress infra is not running (no live holder) — start a container/VM on a custom network first".into(),
        )
    })
}

/// Auto-detects the libvirt VM subnets (the `virbr*` bridge CIDRs) so the return
/// route is added for each. `--vm-subnet` overrides. Best-effort.
fn detect_vm_subnets() -> Vec<String> {
    let out = match Command::new("ip")
        .args(["-br", "-4", "addr", "show"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return Vec::new(),
    };
    let mut subs = Vec::new();
    for line in out.lines() {
        let mut it = line.split_whitespace();
        let iface = it.next().unwrap_or("");
        if !iface.starts_with("virbr") {
            continue;
        }
        if let Some(cidr) = it.find(|s| s.contains('.')) {
            // Normalize a host CIDR (192.168.122.1/24) to the network (192.168.122.0/24).
            if let Some(net) = cidr_network(cidr) {
                subs.push(net);
            }
        }
    }
    subs
}

/// `192.168.122.1/24` → `192.168.122.0/24` (only handles /24, the libvirt
/// default; other prefixes are passed through unchanged with a note upstream).
fn cidr_network(cidr: &str) -> Option<String> {
    let (ip, prefix) = cidr.split_once('/')?;
    let o: Vec<&str> = ip.split('.').collect();
    if o.len() != 4 {
        return None;
    }
    if prefix == "24" {
        Some(format!("{}.{}.{}.0/24", o[0], o[1], o[2]))
    } else {
        Some(cidr.to_string())
    }
}

fn run_plan(plan: &[Vec<String>], apply: bool, tolerate: bool) -> Result<()> {
    for cmd in plan {
        let shown = cmd.join(" ");
        if !apply {
            println!("  {shown}");
            continue;
        }
        output::info(&format!("+ {shown}"));
        let status = Command::new(&cmd[0]).args(&cmd[1..]).status();
        match status {
            Ok(st) if st.success() => {}
            Ok(_) if tolerate => {} // teardown: already-gone is fine
            Ok(st) => {
                return Err(Error::Runtime {
                    context: "vm bridge",
                    message: format!(
                        "`{shown}` failed ({st}) — run as root; `delonix vm unbridge` to roll back"
                    ),
                });
            }
            Err(e) => {
                return Err(Error::Runtime {
                    context: "vm bridge",
                    message: format!("`{cmd:?}`: {e}"),
                });
            }
        }
    }
    Ok(())
}

/// `delonix vm bridge <network>` — establish (or dry-run) the host↔SDN bridge.
pub fn bridge(network: &str, vm_subnets: Vec<String>, apply: bool) -> Result<()> {
    adopt_invoking_user_root();
    let (bridge, prefix, _gw) = infra::resolve_net(network)?;
    let holder = holder_pid()?;
    let subs = if vm_subnets.is_empty() {
        detect_vm_subnets()
    } else {
        vm_subnets
    };
    if subs.is_empty() {
        return Err(Error::Invalid(
            "no libvirt VM subnet found (no `virbr*` with an IPv4) — pass `--vm-subnet <cidr>`"
                .into(),
        ));
    }
    let plan = bridge_plan(&holder, &bridge, &prefix, &subs);
    if !apply {
        output::warn(
            "DRY-RUN — the plan below needs root. Review it, then re-run with `--apply` (as root):",
        );
        run_plan(&plan, false, false)?;
        println!(
            "\nEXPERIMENTAL: this opens VM↔container on '{network}' ({}.0.0/16 ↔ {}). Undo with `delonix vm unbridge {network}`.",
            prefix,
            subs.join(", ")
        );
        return Ok(());
    }
    run_plan(&plan, true, false)?;
    output::info(&format!(
        "bridged '{network}' ({prefix}.0.0/16) to the VM network(s) {} — VMs now reach its containers by IP",
        subs.join(", ")
    ));
    Ok(())
}

/// `delonix vm unbridge <network>` — tear the bridge down.
pub fn unbridge(network: &str, apply: bool) -> Result<()> {
    adopt_invoking_user_root();
    let (bridge, prefix, _gw) = infra::resolve_net(network)?;
    let holder = holder_pid()?;
    let subs = detect_vm_subnets();
    let plan = unbridge_plan(&holder, &bridge, &prefix, &subs);
    if !apply {
        output::warn("DRY-RUN — re-run with `--apply` (as root) to tear down:");
        run_plan(&plan, false, false)?;
        return Ok(());
    }
    run_plan(&plan, true, true)?;
    output::info(&format!("unbridged '{network}'"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_from_passwd_line_pega_o_campo_6() {
        assert_eq!(
            home_from_passwd_line("walter:x:1000:1000:Walter:/home/walter:/bin/bash").as_deref(),
            Some("/home/walter")
        );
        // Sem home (campo vazio) → None; linha malformada → None.
        assert_eq!(home_from_passwd_line("x:x:0:0::::").as_deref(), None);
        assert_eq!(home_from_passwd_line("lixo").as_deref(), None);
    }

    #[test]
    fn veth_names_estaveis_e_curtos() {
        let (vh, vs) = veth_names("delonix0");
        assert!(vh.len() <= 15 && vs.len() <= 15, "IFNAMSIZ");
        assert_ne!(vh, vs);
        // Determinístico: mesma rede → mesmos nomes (o unbridge tem de os achar).
        assert_eq!(veth_names("delonix0"), (vh, vs));
        assert_ne!(veth_names("dlxned04b6d4").0, veth_names("delonix0").0);
    }

    #[test]
    fn host_sdn_ip_fica_alto_no_16() {
        assert_eq!(host_sdn_ip("10.200"), "10.200.255.254");
        assert_eq!(host_sdn_ip("10.210"), "10.210.255.254");
    }

    #[test]
    fn cidr_network_normaliza_para_a_rede() {
        assert_eq!(
            cidr_network("192.168.122.1/24").as_deref(),
            Some("192.168.122.0/24")
        );
        assert_eq!(
            cidr_network("10.10.100.1/24").as_deref(),
            Some("10.10.100.0/24")
        );
        // /16 e outros passam intactos (nota a montante).
        assert_eq!(
            cidr_network("172.16.5.1/16").as_deref(),
            Some("172.16.5.1/16")
        );
        assert_eq!(cidr_network("lixo").as_deref(), None);
    }

    #[test]
    fn bridge_plan_tem_a_ordem_e_os_passos_certos() {
        let plan = bridge_plan("4242", "delonix0", "10.200", &["192.168.122.0/24".into()]);
        // veth criado ANTES de mover para a netns; enslave DENTRO do holder.
        assert_eq!(plan[0][..3], ["ip", "link", "add"]);
        assert!(plan[1].contains(&"netns".to_string()) && plan[1].contains(&"4242".to_string()));
        assert!(plan[2][0] == "nsenter" && plan[2].contains(&"master".to_string()));
        // host end recebe o IP alto do /16.
        assert!(plan
            .iter()
            .any(|c| c.contains(&"10.200.255.254/16".to_string())));
        // ip_forward ligado.
        assert!(plan
            .iter()
            .any(|c| c.contains(&"net.ipv4.ip_forward=1".to_string())));
        // rota de retorno da subnet da VM, via o host end, DENTRO do holder.
        let ret = plan.last().unwrap();
        assert_eq!(ret[0], "nsenter");
        assert!(
            ret.contains(&"route".to_string()) && ret.contains(&"192.168.122.0/24".to_string())
        );
        assert!(ret.contains(&"10.200.255.254".to_string()));
    }

    #[test]
    fn unbridge_plan_apaga_rota_e_veth() {
        let plan = unbridge_plan("4242", "delonix0", "10.200", &["192.168.122.0/24".into()]);
        assert!(plan[0].contains(&"route".to_string()) && plan[0].contains(&"del".to_string()));
        let last = plan.last().unwrap();
        assert_eq!(last[..3], ["ip", "link", "del"]);
        assert!(last[3].starts_with("vbh"));
    }
}
