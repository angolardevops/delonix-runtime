//! Realizes cloud-init INTENT (hostname, users, SSH keys) for a VM.
//!
//! # Why this lives in the engine and not in the CLI
//!
//! It used to live in `delonix-runtime-bin`, and that put a mechanism where an
//! intent belonged. [`crate::VmConfig`] could say only `seed: <path>` — a
//! NoCloud ISO on THIS filesystem — so every consumer had to build one itself,
//! and any backend that cannot read this host's disk was structurally excluded
//! from cloud-init. The Proxmox backend has cloud-init of its own and could
//! honour the same intent natively; it just never received it, because the only
//! way to express it was a local file path.
//!
//! So `VmConfig` now carries what the operator MEANT (`hostname`, `ci_user`,
//! `ssh_keys`) and each backend realizes it its own way — a NoCloud ISO here for
//! the local backends, `--ciuser`/`--sshkeys` on the node for Proxmox. `seed`
//! stays as the escape hatch for whoever brings their own.
//!
//! The move also takes `cloud-localds` out of the private PaaS: it was calling
//! this exact sequence in its own copy, which is two sources of truth for a
//! format the guest has to agree with.

use crate::{mac_for, valid_vm_name, VmVolume};
use delonix_runtime_core::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The user the golden image creates at build time (`sudo` NOPASSWD), and the
/// account everything else here assumes is the login target — the serial
/// autologin below and `cluster kubeadm`'s SSH user.
pub const DEFAULT_CI_USER: &str = "delonix";

/// Minimal NoCloud `user-data` — pure, testable without a real `cloud-localds`.
/// `package_update: false`/`package_upgrade: false` because the golden image
/// already comes ready (see `cmd::vmimage`); no point spending the first boot
/// on `apt update`.
pub fn build_user_data(
    hostname: &str,
    ci_user: &str,
    ssh_keys: &[String],
    volumes: &[VmVolume],
) -> String {
    let mut out = String::from("#cloud-config\n");
    out.push_str(&format!("hostname: {hostname}\n"));
    out.push_str("package_update: false\n");
    out.push_str("package_upgrade: false\n");
    if !ssh_keys.is_empty() {
        // BUG FIXED HERE: a bare top-level `ssh_authorized_keys:` only reaches
        // cloud-init's DEFAULT distro user (`ubuntu` on this Ubuntu-based golden
        // image) — NOT the `delonix` user the golden image itself creates at
        // build time (`vmimage.rs`, `sudo` NOPASSWD) and that everything else
        // here assumes is the login target: the autologin config right below
        // (`agetty --autologin`), and `cluster kubeadm`'s SSH user, hardcoded to
        // `delonix` (the account "the golden image already creates"). Found
        // live: `delonix cluster kubeadm` consistently failed "SSH did not
        // respond within --boot-timeout" — the VM WAS reachable and the key WAS
        // installed, just onto `ubuntu`, not `delonix`.
        // Scoping the key under `users:` (keeping `- default` so the `ubuntu`
        // account nothing else here relies on still gets created too) targets
        // the EXISTING account directly — cloud-init adds keys to an
        // already-existing user without trying to recreate it.
        out.push_str("users:\n");
        out.push_str("  - default\n");
        out.push_str(&format!("  - name: {ci_user}\n"));
        out.push_str("    ssh_authorized_keys:\n");
        for k in ssh_keys {
            out.push_str(&format!("      - {k}\n"));
        }
    }
    // Auto-login on the serial console (ttyS0) as the golden's user: `vm console`
    // enters directly, without asking for a password (user's choice — a dev VM's
    // serial console is local access, like in multipass/kind). Without this,
    // cloud-init reconfigures the getty and the console asks for login.
    out.push_str("write_files:\n");
    out.push_str("  - path: /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf\n");
    out.push_str("    content: |\n");
    out.push_str("      [Service]\n");
    out.push_str("      ExecStart=\n");
    out.push_str(&format!(
        "      ExecStart=-/sbin/agetty --autologin {ci_user} --keep-baud 115200,57600,38400,9600 - $TERM\n",
    ));
    out.push_str("runcmd:\n");
    out.push_str("  - [ systemctl, daemon-reload ]\n");
    out.push_str("  - [ systemctl, restart, serial-getty@ttyS0 ]\n");
    // Mount each 9p volume shared by the domain's `<filesystem>`. The `_netdev`
    // avoids blocking the boot if the share isn't ready; `trans=virtio`
    // + `9p2000.L` is the dialect that libvirt/QEMU expose. This way the guest
    // mounts the NAS/volume WITHOUT the user writing fstab or cloud-init by hand.
    if !volumes.is_empty() {
        out.push_str("mounts:\n");
        for v in volumes {
            let mode = if v.read_only { "ro" } else { "rw" };
            // `mount_path` quoted (validated without `"` in `valid_mount_path`) and
            // `tag` sanitized (`vol_tag`) — the YAML flow sequence doesn't break.
            out.push_str(&format!(
                "  - [ \"{}\", \"{}\", 9p, \"trans=virtio,version=9p2000.L,{mode},_netdev\", \"0\", \"0\" ]\n",
                v.tag, v.mount_path
            ));
        }
    }
    out
}

fn build_meta_data(instance_id: &str, hostname: &str) -> String {
    format!("instance-id: {instance_id}\nlocal-hostname: {hostname}\n")
}

/// The NoCloud `network-config`: DHCP on the primary NIC, matched by **MAC**.
///
/// It used to match by NAME with a glob (`match: {name: "e*"}`), to cover
/// `eth0`/`ens3`/`enp1s0` without knowing which the guest would pick. That
/// works where the renderer is netplan (Ubuntu, Debian) and is BROKEN wherever
/// it is NetworkManager (Fedora, Rocky) — measured, and the cloud-init source
/// says why in one line:
///
/// ```text
/// if if_type == "bridge" or not self.config.has_option(if_type, "mac-address"):
///     self.config["connection"]["interface-name"] = iface["name"]
/// ```
///
/// With no MAC to write, the renderer falls back to naming the interface after
/// the netplan KEY — so a Fedora guest got a keyfile saying
/// `interface-name=eth-all`, NetworkManager waited for a device by that name,
/// and `enp1s0` stayed down forever.
///
/// A MAC is the one thing about the NIC that IS known before the guest exists
/// ([`crate::mac_for`], stamped by both backends), so there is nothing to guess.
///
/// **Scope: the primary NIC only** — that is the interface everything else in
/// this engine depends on (DNS, namespace isolation, `vm ssh`).
pub fn build_network_config(vm_name: &str) -> String {
    format!(
        "version: 2\nethernets:\n  nic0:\n    match:\n      macaddress: \"{}\"\n    dhcp4: true\n",
        mac_for(vm_name)
    )
}

/// Generates (or reuses, via `user_data_override`) the `user-data`/`meta-data`
/// and packages them into a NoCloud ISO with `cloud-localds`. Returns the path.
///
/// `base` is the state root — this used to read a global `state_root()` from the
/// CLI, which is precisely what kept the function trapped in the binary crate.
///
/// SSH keys arrive ALREADY RESOLVED: turning `@~/.ssh/id_ed25519.pub` into a key
/// is CLI convenience, and doing it here would mean the engine reading arbitrary
/// files on behalf of a caller that may not have meant to.
pub fn generate_seed_iso(
    base: &Path,
    vm_name: &str,
    hostname: Option<&str>,
    ci_user: Option<&str>,
    ssh_keys: &[String],
    user_data_override: Option<&Path>,
    volumes: &[VmVolume],
) -> Result<PathBuf> {
    // SECURITY: this runs BEFORE `create()` — which is where `valid_vm_name` is
    // enforced — so a `../../../home/<u>/.ssh` name reached `create_dir_all`/
    // `fs::write` here (seed.iso with fully attacker-controlled content via
    // `--user-data`) before ever hitting that check. Enforce it here too: this
    // function is a path-writing boundary of its own, not just an API consumer
    // of `create()`.
    if !valid_vm_name(vm_name) {
        return Err(Error::Invalid(format!("invalid VM name: {vm_name:?}")));
    }
    let hostname = hostname.unwrap_or(vm_name).to_string();
    let ci_user = ci_user.unwrap_or(DEFAULT_CI_USER);
    let work_dir = base.join("vms").join(vm_name);
    std::fs::create_dir_all(&work_dir)?;

    let user_data_path = work_dir.join("user-data");
    match user_data_override {
        Some(p) => {
            std::fs::copy(p, &user_data_path).map_err(|e| {
                Error::Invalid(format!("could not copy user-data '{}': {e}", p.display()))
            })?;
        }
        None => {
            let content = build_user_data(&hostname, ci_user, ssh_keys, volumes);
            std::fs::write(&user_data_path, content)?;
        }
    }
    let meta_data_path = work_dir.join("meta-data");
    std::fs::write(&meta_data_path, build_meta_data(vm_name, &hostname))?;

    // network-config (NoCloud v2): DHCP on the primary NIC, matched by MAC.
    let net_cfg_path = work_dir.join("network-config");
    std::fs::write(&net_cfg_path, build_network_config(vm_name))?;

    let iso_path = work_dir.join("seed.iso");
    let status = Command::new("cloud-localds")
        .arg(format!("--network-config={}", net_cfg_path.display()))
        .arg(&iso_path)
        .arg(&user_data_path)
        .arg(&meta_data_path)
        .status()
        .map_err(|e| {
            // An ENOENT here is the TOOL missing, not a file — see the class
            // already catalogued in CLAUDE.md («the ENOENT of a `Command` is not
            // a missing file»). Saying so beats sending the reader looking for a
            // path that was never named.
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::Invalid(
                    "cloud-localds not found — install it (Debian/Ubuntu: `cloud-image-utils`, \
                     Fedora/Rocky: `cloud-utils`), or pass a ready-made seed"
                        .into(),
                )
            } else {
                Error::Invalid(format!("running cloud-localds: {e}"))
            }
        })?;
    if !status.success() {
        return Err(Error::Invalid(format!(
            "cloud-localds failed (exit {:?})",
            status.code()
        )));
    }
    Ok(iso_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chave_vai_para_a_conta_da_golden_e_nao_para_a_default_da_distro() {
        let ud = build_user_data("no1", DEFAULT_CI_USER, &["ssh-ed25519 AAAA".into()], &[]);
        // O `- default` fica, para a conta da distro continuar a existir; a
        // chave é que tem de aterrar na conta que o `cluster kubeadm` usa.
        assert!(ud.contains("  - default\n"), "{ud}");
        assert!(ud.contains("  - name: delonix\n"), "{ud}");
        let keys_at = ud.find("ssh_authorized_keys").unwrap();
        let user_at = ud.find("  - name: delonix").unwrap();
        assert!(user_at < keys_at, "a chave saiu de baixo da conta:\n{ud}");
    }

    /// O `ci_user` governa as DUAS coisas que dependem da conta — a chave e o
    /// autologin da consola série. Ter uma sem a outra dá uma VM em que se entra
    /// por SSH e não pela consola, ou o contrário.
    #[test]
    fn o_ci_user_governa_a_chave_e_o_autologin() {
        let ud = build_user_data("no1", "operador", &["ssh-ed25519 AAAA".into()], &[]);
        assert!(ud.contains("  - name: operador\n"), "{ud}");
        assert!(ud.contains("--autologin operador "), "{ud}");
        assert!(!ud.contains("delonix"), "sobrou a conta antiga:\n{ud}");
    }

    /// Sem chaves não há bloco `users:` nenhum — mas o autologin fica, porque é
    /// o que faz o `vm console` entrar sem password.
    #[test]
    fn sem_chaves_nao_ha_bloco_de_utilizadores() {
        let ud = build_user_data("no1", DEFAULT_CI_USER, &[], &[]);
        assert!(!ud.contains("users:"), "{ud}");
        assert!(ud.contains("--autologin delonix "), "{ud}");
    }

    /// O `network-config` casa por MAC e NUNCA por nome — é o que faz o mesmo
    /// ficheiro servir netplan e NetworkManager (ver o doc-comment).
    #[test]
    fn a_rede_casa_por_mac_e_nunca_por_nome() {
        let nc = build_network_config("no1");
        assert!(nc.contains("macaddress:"), "{nc}");
        assert!(!nc.contains("name:"), "voltou a casar por nome:\n{nc}");
        assert!(nc.contains(&mac_for("no1")), "{nc}");
    }

    /// Cada volume 9p vira uma linha de `mounts:` — é o que faz o convidado
    /// montar o volume/NAS sem ninguém escrever fstab à mão.
    #[test]
    fn com_volumes_injecta_os_mounts_9p() {
        let vols = vec![
            VmVolume {
                tag: "dados".into(),
                source: "/srv/dados".into(),
                mount_path: "/mnt/dados".into(),
                read_only: false,
            },
            VmVolume {
                tag: "ro".into(),
                source: "/srv/ro".into(),
                mount_path: "/mnt/ro".into(),
                read_only: true,
            },
        ];
        let ud = build_user_data("myvm", DEFAULT_CI_USER, &[], &vols);
        assert!(ud.contains("mounts:\n"), "{ud}");
        assert!(
            ud.contains("[ \"dados\", \"/mnt/dados\", 9p, \"trans=virtio,version=9p2000.L,rw,_netdev\", \"0\", \"0\" ]"),
            "{ud}"
        );
        assert!(
            ud.contains("[ \"ro\", \"/mnt/ro\", 9p, \"trans=virtio,version=9p2000.L,ro,_netdev\", \"0\", \"0\" ]"),
            "{ud}"
        );
        // Sem volumes → sem secção nenhuma.
        assert!(!build_user_data("myvm", DEFAULT_CI_USER, &[], &[]).contains("mounts:"));
    }

    #[test]
    fn o_meta_data_tem_instance_id_e_hostname() {
        assert_eq!(
            build_meta_data("vm-1", "myvm"),
            "instance-id: vm-1\nlocal-hostname: myvm\n"
        );
    }

    /// Um nome de VM com travessia é recusado ANTES de escrever seja o que for —
    /// esta função é uma fronteira de escrita por direito próprio.
    #[test]
    fn um_nome_com_travessia_nao_escreve_nada() {
        let base = std::env::temp_dir().join(format!("dlx-ci-test-{}", std::process::id()));
        let mau = "../../../etc/delonix-teste";
        assert!(generate_seed_iso(&base, mau, None, None, &[], None, &[]).is_err());
        assert!(!base.join("vms").join(mau).exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
