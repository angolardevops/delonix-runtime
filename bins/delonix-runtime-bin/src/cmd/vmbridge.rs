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
//! explicit `ingress` rules still apply). `unbridge` tears it all down: the
//! subnets `bridge --apply` opened are recorded under `ingress/`, so the rules
//! and the return route it removes are the ones that were inserted, not the
//! ones a `virbr*` detection happens to see at teardown time.
//!
//! NOTE: shipped on a feature branch, NOT merged/released — it has not been
//! run end-to-end in this dev sandbox (no root here). The command GENERATION is
//! pure and unit-tested; the privileged execution is validated by the operator
//! on a real host (dry-run first).

use std::path::{Path, PathBuf};
use std::process::Command;

use delonix_model::{Error, Result};
use delonix_sdn::{infra, Cidr};

use super::output;
use super::po;
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

/// The SDN network's prefix. `NetPlan.prefix` is `a.b.c.d/len` for a network
/// created with a CIDR and the legacy two-octet `10.210` for an older record;
/// `Cidr::parse` reads both. This used to assume the second form everywhere —
/// `format!("{prefix}.255.254")` — so a `172.20.4.0/22` network produced the
/// address `172.20.4.0/22.255.254` and an `iptables -d 172.20.4.0/22.0.0/16`.
/// A prefix with no room for a host address besides the gateway and the
/// broadcast (/31, /32) is refused.
fn sdn_cidr(prefix: &str) -> Result<Cidr> {
    let c = Cidr::parse(prefix).ok_or_else(|| {
        Error::Invalid(po::tf(
            "the network's prefix is not a CIDR: '{prefix}'",
            &[("prefix", prefix)],
        ))
    })?;
    if c.len > 30 {
        return Err(Error::Invalid(po::tf(
            "the network's prefix /{len} has no room for the bridge's host address",
            &[("len", &c.len.to_string())],
        )));
    }
    Ok(c)
}

/// The host-side SDN address of the bridge link: the last address before the
/// broadcast (`.255.254` on a /16), high in the prefix to avoid colliding with
/// DHCP-assigned container IPs (which grow from the bottom). Pure.
fn host_sdn_ip(sdn: &Cidr) -> String {
    Cidr::fmt_u32(sdn.last() - 1)
}

/// The SDN network CIDR, canonical (`10.210` → `10.210.0.0/16`). Pure.
fn sdn_subnet(sdn: &Cidr) -> String {
    sdn.to_string_cidr()
}

/// Validates the VM subnets BEFORE they reach an argv run as root.
///
/// Each one becomes `iptables -I FORWARD -s <sub> …` and `ip route add <sub>`
/// inside the holder, and `--vm-subnet` took any string: `default` installed a
/// default route in the holder's netns through the host end, and `0.0.0.0/0`
/// opened the SDN to every source the host forwards. Accepted: a strict
/// `a.b.c.d/len`, not `/0`, not overlapping the SDN network itself (the return
/// route would shadow the bridge's own). Returned canonical (host bits cleared),
/// so the rule `unbridge` deletes is the rule `bridge` inserted.
fn validate_vm_subnets(raw: &[String], sdn: &Cidr) -> Result<Vec<String>> {
    raw.iter()
        .map(|s| {
            let strict = s.split_once('/').is_some_and(|(addr, len)| {
                addr.parse::<std::net::Ipv4Addr>().is_ok()
                    && !len.is_empty()
                    && len.bytes().all(|b| b.is_ascii_digit())
            });
            let c = Cidr::parse(s).filter(|_| strict).ok_or_else(|| {
                Error::Invalid(po::tf(
                    "--vm-subnet must be an IPv4 prefix a.b.c.d/len: '{subnet}'",
                    &[("subnet", s)],
                ))
            })?;
            if c.len == 0 {
                return Err(Error::Invalid(po::tf(
                    "--vm-subnet '{subnet}' is every address, not a VM subnet",
                    &[("subnet", s)],
                )));
            }
            if c.overlaps(sdn) {
                return Err(Error::Invalid(po::tf(
                    "--vm-subnet '{subnet}' overlaps the SDN network {sdn}",
                    &[("subnet", s), ("sdn", &sdn.to_string_cidr())],
                )));
            }
            Ok(c.to_string_cidr())
        })
        .collect()
}

/// `--apply` runs every step as root, so it is refused up front for anyone
/// else. Before this, a non-root `--apply` ran the whole tolerated cleanup
/// plan first — `iptables -D`, `nsenter`, `ip link del`, each failing on
/// EPERM and ignored — and only then stopped on the first bridge step.
fn require_root(euid: u32) -> Result<()> {
    if euid != 0 {
        return Err(Error::Invalid(String::from(po::t(
            "`--apply` needs root: this command changes the host's network namespace \
             (run it with sudo, or drop `--apply` for the dry-run)",
        ))));
    }
    Ok(())
}

fn current_euid() -> u32 {
    // SAFETY: geteuid() has no preconditions.
    unsafe { libc::geteuid() }
}

/// Builds the ordered list of privileged commands (argv each) that establish
/// the bridge. PURE — unit-tested without touching the network. `vm_subnets`
/// are the libvirt VM subnets that must route back through the holder.
fn bridge_plan(
    holder_pid: &str,
    bridge: &str,
    prefix: &Cidr,
    vm_subnets: &[String],
) -> Vec<Vec<String>> {
    let (vh, vs) = veth_names(bridge); // keyed on the SDN bridge (1 per network)
    let host_ip = host_sdn_ip(prefix);
    let host_cidr = format!("{host_ip}/{}", prefix.len);
    let nsenter = |args: &[&str]| -> Vec<String> {
        let mut v = vec![
            // ROOT enters ONLY the net namespace (-n), NOT the userns (-U):
            // as root in the init userns it keeps CAP_NET_ADMIN, which IS valid
            // over a netns owned by a descendant userns (the holder's). With -U
            // it entered the holder userns as an UNMAPPED uid → no caps → the
            // `ip link set … master` got EPERM ("Operation not permitted").
            "nsenter".into(),
            "-t".into(),
            holder_pid.into(),
            "-n".into(),
            "--".into(),
        ];
        v.extend(args.iter().map(|s| s.to_string()));
        v
    };
    let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let sdn = sdn_subnet(prefix);
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
    // 7. FORWARD ACCEPT both ways (defense in depth): libvirt's default network
    // ends its FORWARD chain with a REJECT for traffic that doesn't match its
    // own subnet, which would drop a VM→SDN forwarded packet. Insert at the TOP
    // so it wins. (On the validated host libvirt let it through already; this
    // makes it work where the default policy is stricter.)
    for sub in vm_subnets {
        plan.push(s(&[
            "iptables", "-I", "FORWARD", "-s", sub, "-d", &sdn, "-j", "ACCEPT",
        ]));
        plan.push(s(&[
            "iptables", "-I", "FORWARD", "-s", &sdn, "-d", sub, "-j", "ACCEPT",
        ]));
    }
    // 8. return route inside the holder: VM subnets via the host end.
    for sub in vm_subnets {
        plan.push(nsenter(&["ip", "route", "add", sub, "via", &host_ip]));
    }
    plan
}

/// The teardown plan (best-effort; each command tolerated if already gone).
fn unbridge_plan(
    holder_pid: &str,
    bridge: &str,
    prefix: &Cidr,
    vm_subnets: &[String],
) -> Vec<Vec<String>> {
    let (vh, _vs) = veth_names(bridge);
    let host_ip = host_sdn_ip(prefix);
    let nsenter = |args: &[&str]| -> Vec<String> {
        let mut v = vec![
            // ROOT enters ONLY the net namespace (-n), NOT the userns (-U):
            // as root in the init userns it keeps CAP_NET_ADMIN, which IS valid
            // over a netns owned by a descendant userns (the holder's). With -U
            // it entered the holder userns as an UNMAPPED uid → no caps → the
            // `ip link set … master` got EPERM ("Operation not permitted").
            "nsenter".into(),
            "-t".into(),
            holder_pid.into(),
            "-n".into(),
            "--".into(),
        ];
        v.extend(args.iter().map(|s| s.to_string()));
        v
    };
    let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let sdn = sdn_subnet(prefix);
    let mut plan = Vec::new();
    // Remove the FORWARD ACCEPT rules (mirror of the bridge plan; tolerated —
    // may already be gone).
    for sub in vm_subnets {
        plan.push(s(&[
            "iptables", "-D", "FORWARD", "-s", sub, "-d", &sdn, "-j", "ACCEPT",
        ]));
        plan.push(s(&[
            "iptables", "-D", "FORWARD", "-s", &sdn, "-d", sub, "-j", "ACCEPT",
        ]));
    }
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

/// Where `bridge --apply` records the VM subnets it opened on a network's SDN
/// bridge: one canonical CIDR per line, next to the holder's pid.
fn applied_subnets_path(root: &Path, bridge: &str) -> PathBuf {
    root.join("ingress")
        .join(format!("vmbridge-{bridge}.subnets"))
}

/// The subnets a previous `bridge --apply` recorded, or `None` when there is
/// no record (never bridged, or bridged before the record existed). An empty
/// or unreadable file counts as no record: detection is the fallback, and the
/// entries go through `validate_vm_subnets` like any other before reaching
/// the root argv.
fn read_applied_subnets(path: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    let subs: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    (!subs.is_empty()).then_some(subs)
}

/// Records the subnets `bridge --apply` is about to open. Written BEFORE the
/// plan runs, so a bridge that fails half-way is still undone by `unbridge`.
/// A bridge whose undo cannot be recorded is not opened.
fn write_applied_subnets(path: &Path, subs: &[String]) -> Result<()> {
    let mut body = subs.join("\n");
    body.push('\n');
    path.parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, body))
        .map_err(|e| Error::Runtime {
            context: "vm bridge",
            message: po::tf(
                "could not record the bridged subnets in {path}: {err}",
                &[
                    ("path", &path.display().to_string()),
                    ("err", &e.to_string()),
                ],
            ),
        })
}

/// Which VM subnets a teardown removes. PURE (`detect` is only called when
/// needed). The record `bridge --apply` left is the truth, plus any explicit
/// `--vm-subnet`. Without a record — a bridge made before it existed — the
/// explicit subnets, or else the `virbr*` detection. Before the record,
/// `unbridge` always detected: a bridge made with `--vm-subnet` (or on a host
/// with no `virbr*`) lost only its veth, and both FORWARD ACCEPT rules and
/// the holder's return route stayed open.
fn teardown_subnets(
    explicit: Vec<String>,
    recorded: Option<Vec<String>>,
    detect: impl FnOnce() -> Vec<String>,
) -> Vec<String> {
    let mut subs = match recorded {
        Some(rec) => rec.into_iter().chain(explicit).collect(),
        None if !explicit.is_empty() => explicit,
        None => detect(),
    };
    let mut seen = std::collections::HashSet::new();
    subs.retain(|s| seen.insert(s.clone()));
    subs
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
pub(crate) fn adopt_invoking_user_root() {
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
    if apply {
        require_root(current_euid())?;
    }
    adopt_invoking_user_root();
    let p = infra::resolve_net(network)?;
    let bridge = p.bridge;
    let prefix = sdn_cidr(&p.prefix)?;
    let holder = holder_pid()?;
    // Detected subnets go through the same check: they come from the host's
    // `ip`, but they reach the same root argv.
    let subs = validate_vm_subnets(
        &if vm_subnets.is_empty() {
            detect_vm_subnets()
        } else {
            vm_subnets
        },
        &prefix,
    )?;
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
            "\nEXPERIMENTAL: this opens VM↔container on '{network}' ({} ↔ {}). Undo with `delonix vm unbridge {network}`.",
            sdn_subnet(&prefix),
            subs.join(", ")
        );
        return Ok(());
    }
    // Idempotent establish: clear any leftover from a previous run or a holder
    // respawn (a dangling host-side veth) FIRST, tolerated, so `bridge` doesn't
    // fail on "RTNETLINK: File exists". This is the auto-cleanup of orphans —
    // full lifecycle teardown (on holder death) is the persistence follow-up.
    // The cleanup covers what the previous bridge recorded too: re-bridging
    // with other subnets must not leave the old ones open.
    let record = applied_subnets_path(&state_root(), &bridge);
    let previous = validate_vm_subnets(
        &teardown_subnets(subs.clone(), read_applied_subnets(&record), Vec::new),
        &prefix,
    )?;
    let cleanup = unbridge_plan(&holder, &bridge, &prefix, &previous);
    run_plan(&cleanup, true, true)?;
    write_applied_subnets(&record, &subs)?;
    run_plan(&plan, true, false)?;
    output::info(&format!(
        "bridged '{network}' ({}) to the VM network(s) {} — VMs now reach its containers by IP",
        sdn_subnet(&prefix),
        subs.join(", ")
    ));
    Ok(())
}

/// `delonix vm unbridge <network>` — tear the bridge down: the subnets the
/// bridge recorded, plus `vm_subnets`; without a record, `vm_subnets` or the
/// `virbr*` detection (see `teardown_subnets`).
pub fn unbridge(network: &str, vm_subnets: Vec<String>, apply: bool) -> Result<()> {
    if apply {
        require_root(current_euid())?;
    }
    adopt_invoking_user_root();
    let p = infra::resolve_net(network)?;
    let bridge = p.bridge;
    let prefix = sdn_cidr(&p.prefix)?;
    let holder = holder_pid()?;
    let record = applied_subnets_path(&state_root(), &bridge);
    let subs = validate_vm_subnets(
        &teardown_subnets(vm_subnets, read_applied_subnets(&record), detect_vm_subnets),
        &prefix,
    )?;
    let plan = unbridge_plan(&holder, &bridge, &prefix, &subs);
    if !apply {
        output::warn("DRY-RUN — re-run with `--apply` (as root) to tear down:");
        run_plan(&plan, false, false)?;
        return Ok(());
    }
    run_plan(&plan, true, true)?;
    match std::fs::remove_file(&record) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => output::warn(&po::tf(
            "could not remove the bridge record {path}: {err}",
            &[
                ("path", &record.display().to_string()),
                ("err", &e.to_string()),
            ],
        )),
    }
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
        assert_eq!(host_sdn_ip(&sdn_cidr("10.200").unwrap()), "10.200.255.254");
        assert_eq!(host_sdn_ip(&sdn_cidr("10.210").unwrap()), "10.210.255.254");
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
        let sdn = sdn_cidr("10.200").unwrap();
        let plan = bridge_plan("4242", "delonix0", &sdn, &["192.168.122.0/24".into()]);
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
        // FORWARD ACCEPT nos dois sentidos, contra o REJECT default do libvirt.
        assert!(plan.iter().any(|c| c[..2] == ["iptables", "-I"]
            && c.contains(&"192.168.122.0/24".to_string())
            && c.contains(&"10.200.0.0/16".to_string())));
        // rota de retorno da subnet da VM, via o host end, DENTRO do holder — e
        // fica em ÚLTIMO (depois do enslave e das regras FORWARD).
        let ret = plan.last().unwrap();
        assert_eq!(ret[0], "nsenter");
        assert!(
            ret.contains(&"route".to_string()) && ret.contains(&"192.168.122.0/24".to_string())
        );
        assert!(ret.contains(&"10.200.255.254".to_string()));
    }

    #[test]
    fn sdn_subnet_deriva_o_16() {
        assert_eq!(sdn_subnet(&sdn_cidr("10.210").unwrap()), "10.210.0.0/16");
        assert_eq!(sdn_subnet(&sdn_cidr("10.200").unwrap()), "10.200.0.0/16");
    }

    #[test]
    fn unbridge_plan_apaga_forward_rota_e_veth() {
        let sdn = sdn_cidr("10.200").unwrap();
        let plan = unbridge_plan("4242", "delonix0", &sdn, &["192.168.122.0/24".into()]);
        // Remove as regras FORWARD (espelho do bridge)…
        assert!(plan.iter().any(|c| c[..2] == ["iptables", "-D"]));
        // …a rota de retorno…
        assert!(plan
            .iter()
            .any(|c| c.contains(&"route".to_string()) && c.contains(&"del".to_string())));
        // …e por fim o veth (que arrasta o par inteiro).
        let last = plan.last().unwrap();
        assert_eq!(last[..3], ["ip", "link", "del"]);
        assert!(last[3].starts_with("vbh"));
    }

    #[test]
    fn host_sdn_ip_follows_a_cidr_prefix_not_two_octets() {
        // The two-octet assumption turned this into `172.20.4.0/22.255.254`.
        let sdn = sdn_cidr("172.20.4.0/22").unwrap();
        assert_eq!(host_sdn_ip(&sdn), "172.20.7.254");
        assert_eq!(sdn_subnet(&sdn), "172.20.4.0/22");
        let plan = bridge_plan("4242", "dlxnabc", &sdn, &["192.168.122.0/24".into()]);
        assert!(plan
            .iter()
            .any(|c| c.contains(&"172.20.7.254/22".to_string())));
        assert!(plan.iter().all(|c| c.iter().all(|a| !a.contains("/22."))));
        // No host address left beside gateway and broadcast.
        assert!(sdn_cidr("10.0.0.0/31").is_err());
        assert!(sdn_cidr("not-a-prefix").is_err());
    }

    #[test]
    fn vm_subnet_refuses_what_would_reach_the_root_argv_as_something_else() {
        let sdn = sdn_cidr("10.200").unwrap();
        for bad in [
            "default",   // `ip route add default via …` in the holder
            "0.0.0.0/0", // FORWARD ACCEPT from every source
            "-j",        // read by iptables as an option
            "--help",
            "192.168.122.0/24;id",
            "192.168.122.0", // no prefix length
            "192.168.122.0/+24",
            "+192.168.122.0/24",
            "10.200.5.0/24", // inside the SDN network itself
            "../../etc/x",
        ] {
            assert!(
                validate_vm_subnets(&[bad.to_string()], &sdn).is_err(),
                "{bad:?} must be refused"
            );
        }
        assert_eq!(
            validate_vm_subnets(&["192.168.122.1/24".to_string()], &sdn).unwrap(),
            vec!["192.168.122.0/24".to_string()],
            "a legitimate subnet passes, canonical"
        );
    }

    /// Unique temp dir (without depending on the `tempfile` crate).
    fn tmp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "delonix-vmbridge-{tag}-{}-{}",
            // SAFETY: `getpid` takes no arguments and has no preconditions.
            unsafe { libc::getpid() },
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn detect_must_not_run() -> Vec<String> {
        panic!("the virbr* detection ran although the bridge left a record")
    }

    #[test]
    fn unbridge_removes_the_recorded_subnet_not_the_detected_one() {
        // Measured 2026-09-27: `bridge --vm-subnet 192.168.200.0/24 --apply`,
        // then `unbridge --apply` detected only virbr0's 192.168.122.0/24 and
        // left both FORWARD ACCEPT rules and the holder's return route for .200.
        let root = tmp_root("record");
        let record = applied_subnets_path(&root, "dlxnabc");
        assert!(record.starts_with(root.join("ingress")));
        write_applied_subnets(&record, &["192.168.200.0/24".into()]).unwrap();

        let subs = teardown_subnets(
            Vec::new(),
            read_applied_subnets(&record),
            detect_must_not_run,
        );
        assert_eq!(subs, vec!["192.168.200.0/24".to_string()]);

        let sdn = sdn_cidr("172.20.4.0/22").unwrap();
        let plan = unbridge_plan("4242", "dlxnabc", &sdn, &subs);
        let shown: Vec<String> = plan.iter().map(|c| c.join(" ")).collect();
        for want in [
            "iptables -D FORWARD -s 192.168.200.0/24 -d 172.20.4.0/22 -j ACCEPT",
            "iptables -D FORWARD -s 172.20.4.0/22 -d 192.168.200.0/24 -j ACCEPT",
            "nsenter -t 4242 -n -- ip route del 192.168.200.0/24 via 172.20.7.254",
        ] {
            assert!(
                shown.iter().any(|c| c == want),
                "missing {want:?} in {shown:#?}"
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn without_a_record_the_teardown_falls_back_to_explicit_then_detection() {
        let root = tmp_root("absent");
        let record = applied_subnets_path(&root, "dlxnabc");
        assert_eq!(read_applied_subnets(&record), None, "no file, no record");
        // An empty file is no record either: detection still has a chance.
        std::fs::create_dir_all(record.parent().unwrap()).unwrap();
        std::fs::write(&record, "\n  \n").unwrap();
        assert_eq!(read_applied_subnets(&record), None);

        let detected = || vec!["192.168.122.0/24".to_string()];
        assert_eq!(teardown_subnets(Vec::new(), None, detected), detected());
        // An explicit `--vm-subnet` on a bridge from before the record wins
        // over detection…
        assert_eq!(
            teardown_subnets(vec!["192.168.200.0/24".into()], None, detect_must_not_run),
            vec!["192.168.200.0/24".to_string()]
        );
        // …and adds to a record, without repeating what is already there.
        assert_eq!(
            teardown_subnets(
                vec!["192.168.200.0/24".into(), "10.9.0.0/24".into()],
                Some(vec!["192.168.200.0/24".into()]),
                detect_must_not_run,
            ),
            vec!["192.168.200.0/24".to_string(), "10.9.0.0/24".to_string()]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_record_round_trips_one_canonical_cidr_per_line() {
        let root = tmp_root("roundtrip");
        // The `ingress/` directory may not exist yet: the write creates it.
        let record = applied_subnets_path(&root, "delonix0");
        let subs = vec![
            "192.168.122.0/24".to_string(),
            "192.168.200.0/24".to_string(),
        ];
        write_applied_subnets(&record, &subs).unwrap();
        assert_eq!(
            std::fs::read_to_string(&record).unwrap(),
            "192.168.122.0/24\n192.168.200.0/24\n"
        );
        assert_eq!(read_applied_subnets(&record), Some(subs));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn apply_is_refused_without_root_before_any_command() {
        assert!(require_root(1000).is_err());
        assert!(require_root(0).is_ok());
        if current_euid() == 0 {
            return; // the ordering below is only observable without root
        }
        // The root refusal comes FIRST: before resolving the network, before
        // the holder, before the tolerated cleanup plan. A network that does
        // not exist would otherwise be the error reported.
        for r in [
            bridge("no-such-net-s4", vec!["-j".into()], true),
            unbridge("no-such-net-s4", vec!["-j".into()], true),
        ] {
            let msg = r.unwrap_err().to_string();
            assert!(msg.contains("needs root"), "{msg}");
        }
    }
}
