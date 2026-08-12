//! `delonix network vlan` — an 802.1Q VLAN on a PHYSICAL NIC (ADR-0013 tier C).
//!
//! **This is the one command in `delonix network` that is not rootless, and it
//! says so every time it runs.** Everything else in this engine works as an
//! unprivileged user; a VLAN interface on a host NIC does not, and no amount of
//! configuration changes that:
//!
//! ```text
//! ip link add link wlp4s0 name x.100 type vlan id 100   → Operation not permitted
//! systemd-run --user --scope -p Delegate=yes -- (same)  → Operation not permitted
//! …and that scope's CapEff is 0000000000000000
//! ```
//!
//! Measured on this host, 2026-08-12. `Delegate=yes` delegates CGROUP
//! controllers (cpu/memory/pids) — it is what makes `-m`/`--cpus` take effect —
//! and has nothing to do with the host's network namespace. `CAP_NET_ADMIN`
//! there is root over the host's networking, and there is no user-level route to
//! it. So tier C is privileged or it does not exist.
//!
//! **The principle is kept by CONTAINMENT, not by pretending.** This follows the
//! `vm bridge` precedent exactly, and for the same reason — that command had to
//! cross the same line first:
//!
//! * a separate command, never a flag that silently escalates another one;
//! * **dry-run by default** — it prints the plan and changes nothing until
//!   `--apply`, so the privileged step is always something the operator read
//!   first;
//! * refuses without privilege, naming what it needs, instead of degrading into
//!   a half-configured NIC;
//! * warns, every run, WHY this one is different — a reader who meets it without
//!   context should not have to guess that the rest of the engine is unlike it.
//!
//! What it does NOT do is put the daemonless model at risk: nothing is left
//! running. `ip link add` is a one-shot state change on the host, the same shape
//! as `vm bridge`'s veth, and `--rm` takes it back out.

use delonix_runtime_core::{Error, Result};

use super::output;

/// One step of the plan: the argv, and what it is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) why: &'static str,
    pub(crate) argv: Vec<String>,
}

/// The commands that create (or remove) the VLAN interface — PURE, so the one
/// thing worth checking (what exactly runs as root) is asserted in a test rather
/// than discovered by running it as root.
///
/// The name is `<parent>.<id>`, the kernel's own convention, so an operator
/// looking at `ip link` sees what they would expect from any other VLAN on the
/// box rather than a delonix-specific name they have to learn.
pub(crate) fn vlan_plan(parent: &str, id: u16, up: bool) -> Vec<Step> {
    let dev = format!("{parent}.{id}");
    if !up {
        return vec![Step {
            why: "remove the VLAN interface",
            argv: vec!["ip".into(), "link".into(), "del".into(), dev],
        }];
    }
    vec![
        Step {
            why: "create the 802.1Q interface on the parent NIC",
            argv: vec![
                "ip".into(),
                "link".into(),
                "add".into(),
                "link".into(),
                parent.into(),
                "name".into(),
                dev.clone(),
                "type".into(),
                "vlan".into(),
                "id".into(),
                id.to_string(),
            ],
        },
        Step {
            why: "bring it up",
            argv: vec!["ip".into(), "link".into(), "set".into(), dev, "up".into()],
        },
    ]
}

/// A VLAN id the kernel accepts. 0 and 4095 are reserved by 802.1Q itself.
fn valid_vlan_id(id: u16) -> Result<()> {
    if (1..=4094).contains(&id) {
        Ok(())
    } else {
        Err(Error::Invalid(super::po::tf(
            "VLAN id {id}: must be 1-4094 (0 and 4095 are reserved by 802.1Q)",
            &[("id", &id.to_string())],
        )))
    }
}

/// The parent goes into an argv as a device name.
fn valid_parent(nic: &str) -> Result<()> {
    let ok = !nic.is_empty()
        && nic.len() <= 15
        && !nic.starts_with('-')
        && nic
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(super::po::tf(
            "invalid parent NIC '{nic}'",
            &[("nic", nic)],
        )))
    }
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// `delonix network vlan <parent> <id> [--rm] [--apply]`.
pub fn run(parent: &str, id: u16, rm: bool, apply: bool) -> Result<()> {
    valid_parent(parent)?;
    valid_vlan_id(id)?;
    let plan = vlan_plan(parent, id, !rm);

    if !apply {
        output::warn(super::po::t(
            "DRY-RUN — nothing was changed. This is the ONE command in `delonix network` that \
             needs root: an 802.1Q interface on a host NIC requires CAP_NET_ADMIN in the host's \
             network namespace, which no unprivileged user has (a `Delegate=yes` scope has zero \
             capabilities — that flag delegates cgroup controllers, not networking). Review the \
             plan, then re-run with `--apply` as root.",
        ));
        for s in &plan {
            println!("  {}  # {}", s.argv.join(" "), s.why);
        }
        return Ok(());
    }

    if !is_root() {
        return Err(Error::Invalid(String::from(super::po::t(
            "`network vlan --apply` needs root — re-run with `sudo`. It is refused rather than \
             attempted because a half-created VLAN is worse than none: the interface would exist \
             and never come up, and the failure would surface later as a network that silently \
             carries no traffic.",
        ))));
    }

    output::warn(super::po::t(
        "this step runs as ROOT and touches the HOST's networking, unlike the rest of this \
         engine. It is a one-shot state change (no daemon, nothing left running) and \
         `--rm` takes it back out.",
    ));
    for s in &plan {
        let out = std::process::Command::new(&s.argv[0])
            .args(&s.argv[1..])
            .output()
            .map_err(|e| Error::Runtime {
                context: "vlan",
                message: format!("{}: {e}", s.argv[0]),
            })?;
        if !out.status.success() {
            return Err(Error::Runtime {
                context: "vlan",
                message: format!(
                    "{}: {}",
                    s.argv.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            });
        }
    }
    println!(
        "{}",
        super::po::tf(
            if rm {
                "VLAN {dev} removed"
            } else {
                "VLAN {dev} up"
            },
            &[("dev", &format!("{parent}.{id}"))],
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O plano é PURO para isto poder ser afirmado sem correr nada como root: o
    /// que corre com privilégio é exactamente o que está aqui, e nada mais.
    #[test]
    fn o_plano_e_o_nome_do_device_seguem_a_convencao_do_kernel() {
        let p = vlan_plan("eth0", 100, true);
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p[0].argv.join(" ").contains("type vlan id 100"), "{p:?}");
        // `<parent>.<id>` — o que um operador espera ver num `ip link`.
        assert!(p[0].argv.contains(&"eth0.100".to_string()), "{p:?}");
        assert!(p[1].argv.join(" ").ends_with("eth0.100 up"), "{p:?}");
        // A remoção é UM passo e nomeia o mesmo device.
        let d = vlan_plan("eth0", 100, false);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].argv, vec!["ip", "link", "del", "eth0.100"]);
    }

    /// 0 e 4095 são reservados pelo próprio 802.1Q — recusá-los aqui é mais
    /// claro que deixar o kernel responder com um EINVAL sem sujeito.
    #[test]
    fn os_ids_reservados_do_802_1q_sao_recusados() {
        assert!(valid_vlan_id(0).is_err());
        assert!(valid_vlan_id(4095).is_err());
        assert!(valid_vlan_id(1).is_ok());
        assert!(valid_vlan_id(4094).is_ok());
    }

    #[test]
    fn um_parent_que_viraria_opcao_e_recusado() {
        for mau in ["", "-x", "a b", "umnomedemasiadolongoparaumanic"] {
            assert!(valid_parent(mau).is_err(), "aceitou {mau:?}");
        }
        assert!(valid_parent("wlp4s0").is_ok());
        assert!(valid_parent("eth0.10").is_ok());
    }
}
