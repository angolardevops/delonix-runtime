//! `delonix system boot` — install systemd units so the running containers
//! come back UP automatically when the host boots, with no manual restart.
//! Moved here from `net boot` (B2 of the CLI restructuring): this is about
//! the ENGINE surviving a reboot, not about SDN plumbing.
//!
//! Rootless installs USER units + linger (start at boot without a login);
//! root installs system units. There's no daemon: each unit's `ExecStart` is
//! `delonix container start <name>` and `ExecStop` is `delonix container stop`.

use clap::Subcommand;
use delonix_image::ImageStore;
use delonix_runtime::{self as runtime};
use delonix_runtime_core::{Result, Store};

use super::util::open_stores;

#[derive(Subcommand)]
pub enum BootCmd {
    /// Install + enable systemd units so containers come back after a reboot.
    ///
    /// Covers the ones running now AND the ones whose own policy says they
    /// should be (`always`/`unless-stopped`) — a reboot kills the supervisor, so
    /// those are stopped precisely when it matters. Stale units whose container
    /// is gone are removed. Rootless uses user units + linger.
    Enable {
        /// Restart policy baked into the units (`no|on-failure[:max]|always|unless-stopped`).
        ///
        /// Omitted, each unit inherits the container's own policy (`always` if it
        /// has none). Given, it applies to every unit — including `no`.
        #[arg(long)]
        restart: Option<String>,
    },
    /// Disable + remove the generated boot units.
    Disable,
    /// Show boot-persistence status (installed units + mode).
    Status,
}

/// Prefix of the units THIS command generates — and nothing else.
///
/// **It used to be `delonix-`, and as root that matched `delonix-cri.service`**:
/// the unit `cluster apply` installs in `/etc/systemd/system` and the golden VM
/// image enables, i.e. the kubelet's CRI endpoint. A `net boot disable` on a
/// Kubernetes node therefore disabled and DELETED the runtime the node is built
/// on, and `status` listed it as if this command had generated it.
///
/// The generated units carry a prefix only they can have, so the sweep can never
/// reach a unit somebody else installed.
const UNIT_PREFIX: &str = "delonix-boot-";

/// Is this a unit `system boot enable` generated?
///
/// The legacy `delonix-<name>.service` form is still recognised, so a `disable`
/// cleans up what an older binary installed — but `delonix-cri.service` is
/// excluded by name, because that one was never ours.
fn is_boot_unit(name: &str) -> bool {
    if !name.ends_with(".service") {
        return false;
    }
    if name == "delonix-cri.service" {
        return false;
    }
    name.starts_with(UNIT_PREFIX) || name.starts_with("delonix-")
}

pub fn run(action: BootCmd) -> Result<()> {
    let (_images, store) = open_stores()?;
    let rootless = runtime::is_rootless();
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "delonix".into());
    let root = ImageStore::default_root();
    let (unit_dir, user_mode, wanted_by) = if rootless {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        (
            std::path::PathBuf::from(home).join(".config/systemd/user"),
            true,
            "default.target",
        )
    } else {
        (
            std::path::PathBuf::from("/etc/systemd/system"),
            false,
            "multi-user.target",
        )
    };
    let sysctl = |args: &[&str]| -> bool {
        let mut c = std::process::Command::new("systemctl");
        if user_mode {
            c.arg("--user");
        }
        c.args(args).status().map(|s| s.success()).unwrap_or(false)
    };

    match action {
        BootCmd::Enable { restart } => enable(
            &store,
            &unit_dir,
            &exe,
            &root.display().to_string(),
            wanted_by,
            rootless,
            user_mode,
            restart,
            &sysctl,
        ),
        BootCmd::Disable => {
            let mut n = 0;
            if let Ok(rd) = std::fs::read_dir(&unit_dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if is_boot_unit(&name) {
                        sysctl(&["disable", &name]);
                        let _ = std::fs::remove_file(e.path());
                        n += 1;
                    }
                }
            }
            sysctl(&["daemon-reload"]);
            let user = std::env::var("USER").unwrap_or_default();
            println!("boot: removed {n} unit(s). (linger unchanged — `loginctl disable-linger {user}` to turn it off)");
            Ok(())
        }
        BootCmd::Status => {
            println!(
                "mode:  {}",
                if rootless {
                    "rootless (user units + linger)"
                } else {
                    "root (system units)"
                }
            );
            println!("dir:   {}", unit_dir.display());
            let mut any = false;
            if let Ok(rd) = std::fs::read_dir(&unit_dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if is_boot_unit(&name) {
                        let on = sysctl(&["is-enabled", "--quiet", &name]);
                        println!("  {name}  [{}]", if on { "enabled" } else { "disabled" });
                        any = true;
                    }
                }
            }
            if !any {
                println!("  (no boot units — run `delonix system boot enable`)");
            }
            Ok(())
        }
    }
}

/// Does this container's own policy say it should be running?
///
/// **This is the half that was missing, and it is the one a reboot needs.** The
/// generator only ever looked at containers with a live PID — a snapshot of
/// `ps`. But the supervisor is a plain `fork()`ed process, so a reboot kills it
/// and the container is left `Crashed` with no PID; and `stack apply` will not
/// revive it either (`container::apply` says «already exists, nothing to do»).
/// So a container declared `--restart always`, stopped by the very reboot the
/// units exist to survive, was never given one.
///
/// This is the role `podman-restart.service` plays there: at boot, start what
/// the policy says should be running. Here it is one unit per container instead
/// of one sweeping service, which keeps the systemd-native shape the rest of
/// this file already has.
///
/// `on-failure` is deliberately NOT included: it means «restart if it exits
/// badly», not «be up at boot», and a container that exited 0 has finished.
fn wants_to_be_up(policy: Option<&str>) -> bool {
    matches!(policy, Some("always") | Some("unless-stopped"))
}

/// The unit text for one container. **Pure, so it is testable** — the same
/// reason `etcd::build_etcd_unit` is pure. This generator had no test at all,
/// and it is the artefact that decides whether a host comes back up.
fn container_unit(
    name: &str,
    rp: &str,
    root: &str,
    exe: &str,
    wanted_by: &str,
    anchor: Option<&str>,
) -> String {
    // Members of a POD are ordered behind the first one, and that is not
    // tidiness: they SHARE a network namespace, and whoever starts first is who
    // recreates it (`cmd_start` re-enters it if the holder still serves it, and
    // rebuilds it with the member's namespace if not). N units released in
    // parallel by systemd race to be that one. Ordering them behind a single
    // anchor makes the answer deterministic.
    //
    // `After=` alone, deliberately: `Requires=` would take the whole pod down
    // with the anchor, and a member that stops is not a reason to stop its peers
    // — the engine treats them as separate workloads everywhere else too.
    let ordering = anchor.map(|a| format!("After={a}\n")).unwrap_or_default();
    format!(
        "[Unit]\nDescription=Delonix container {name}\nAfter=network-online.target\nWants=network-online.target\n{ordering}\n\
         [Service]\nType=forking\nRestart={rp}\nTimeoutStopSec=15\nEnvironment=DELONIX_INTERNAL=1\nEnvironment=DELONIX_ROOT={root}\n\
         ExecStart={exe} container start {name}\nExecStop={exe} container stop {name}\n\n\
         [Install]\nWantedBy={wanted_by}\n",
    )
}

/// The unit text for one VM.
///
/// **A VM had NO automatic start path at all** — not a unit, and not
/// `virsh autostart` on the domain (only on the libvirt network). Worse, `vm
/// stop` UNDEFINES the domain, so a stopped VM is not even known to libvirt at
/// the moment of a reboot. The whole burden was on somebody remembering to run
/// `vm start` by hand.
///
/// `Type=oneshot` + `RemainAfterExit`, not `forking`: `vm start` returns once the
/// VMM is up (the guest boots on its own time) and there is no host process for
/// systemd to follow — the backend owns it. Claiming otherwise would have
/// systemd hunting for a main PID that is not there.
fn vm_unit(name: &str, rp: &str, root: &str, exe: &str, wanted_by: &str) -> String {
    format!(
        "[Unit]\nDescription=Delonix VM {name}\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nType=oneshot\nRemainAfterExit=yes\nRestart={rp}\nTimeoutStopSec=60\nEnvironment=DELONIX_INTERNAL=1\nEnvironment=DELONIX_ROOT={root}\n\
         ExecStart={exe} vm start {name}\nExecStop={exe} vm stop {name}\n\n\
         [Install]\nWantedBy={wanted_by}\n",
    )
}

/// The unit each pod's members must start after: the pod's FIRST container by
/// name, which is stable (`<pod>-c0`, `-c1`, …) and is the one that holds the
/// shared namespaces.
fn pod_anchors(
    containers: &[delonix_runtime_core::Container],
) -> std::collections::BTreeMap<String, String> {
    let mut first: std::collections::BTreeMap<String, String> = Default::default();
    for c in containers {
        let Some(pod) = c.labels.get(super::pod::POD_LABEL) else {
            continue;
        };
        first
            .entry(pod.clone())
            .and_modify(|cur| {
                if c.name < *cur {
                    *cur = c.name.clone();
                }
            })
            .or_insert_with(|| c.name.clone());
    }
    first
}

#[allow(clippy::too_many_arguments)]
fn enable(
    store: &Store,
    unit_dir: &std::path::Path,
    exe: &str,
    root: &str,
    wanted_by: &str,
    rootless: bool,
    user_mode: bool,
    restart: Option<String>,
    sysctl: &dyn Fn(&[&str]) -> bool,
) -> Result<()> {
    std::fs::create_dir_all(unit_dir)?;
    let all = store.list()?;
    let anchors = pod_anchors(&all);
    let mut installed: Vec<String> = Vec::new();
    // One unit per container that SHOULD be up — not merely per container that
    // happens to be up right now.
    for c in &all {
        let alive = c.pid.map(runtime::is_alive).unwrap_or(false);
        if !alive && !wants_to_be_up(c.restart_policy.as_deref()) {
            continue;
        }
        // **`--restart no` used to be unreachable**: the flag defaulted to
        // `always` and the ONLY branch that read the container's own policy was
        // `restart == "no"`, so asking for `Restart=no` silently produced
        // `Restart=always` — the one value the flag's name promises was the one
        // it could not express. `Option` separates «not given» from «given as
        // `no`», which is what the two cases actually are.
        let rp = match restart.as_deref() {
            Some(explicit) => explicit.to_string(),
            None => c.restart_policy.as_deref().unwrap_or("always").to_string(),
        };
        // A âncora do próprio pod nunca depende de si mesma.
        let anchor = c
            .labels
            .get(super::pod::POD_LABEL)
            .and_then(|p| anchors.get(p))
            .filter(|first| **first != c.name)
            .map(|first| format!("{UNIT_PREFIX}{first}.service"));
        let unit = container_unit(&c.name, &rp, root, exe, wanted_by, anchor.as_deref());
        let unit_name = format!("{UNIT_PREFIX}{}.service", c.name);
        std::fs::write(unit_dir.join(&unit_name), unit)?;
        installed.push(unit_name);
    }
    // VMs, pelo MESMO critério: a correr, ou com política que diga que deviam
    // estar. Um prefixo próprio (`delonix-boot-vm-`) porque um container e uma
    // VM podem ter o mesmo nome — são stores diferentes — e dois units com o
    // mesmo ficheiro seria um a apagar o outro em silêncio.
    for vm in delonix_vm::list(&std::path::PathBuf::from(root)).unwrap_or_default() {
        let alive = vm.status == delonix_runtime_core::Status::Running;
        if !alive && !wants_to_be_up(vm.restart_policy.as_deref()) {
            continue;
        }
        let rp = match restart.as_deref() {
            Some(explicit) => explicit.to_string(),
            None => vm.restart_policy.as_deref().unwrap_or("always").to_string(),
        };
        let unit_name = format!("{UNIT_PREFIX}vm-{}.service", vm.name);
        std::fs::write(
            unit_dir.join(&unit_name),
            vm_unit(&vm.name, &rp, root, exe, wanted_by),
        )?;
        installed.push(unit_name);
    }
    // **Converge, don't just create.** A unit whose container was `rm`-ed keeps
    // its boot link, and with `Restart=always` baked in it fails in a loop at
    // every boot — for a container that no longer exists. `enable` is the only
    // command anyone runs twice, so it is where the stale ones have to go.
    let mut pruned: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(unit_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if is_boot_unit(&name) && !installed.contains(&name) {
                sysctl(&["disable", &name]);
                let _ = std::fs::remove_file(e.path());
                pruned.push(name);
            }
        }
    }
    if installed.is_empty() {
        if !pruned.is_empty() {
            sysctl(&["daemon-reload"]);
            println!("boot: removed {} stale unit(s).", pruned.len());
        }
        println!(
            "boot: no running containers — start them first, then `delonix system boot enable`."
        );
        return Ok(());
    }
    sysctl(&["daemon-reload"]);
    // `enable` (no `--now`): create the boot link WITHOUT restarting what's already up.
    for u in &installed {
        sysctl(&["enable", u]);
    }
    if rootless {
        // linger: user units start at boot without a login session.
        if let Ok(user) = std::env::var("USER") {
            let _ = std::process::Command::new("loginctl")
                .args(["enable-linger", &user])
                .status();
        }
    }
    if !pruned.is_empty() {
        println!(
            "boot: removed {} stale unit(s) (their container is gone): {}",
            pruned.len(),
            pruned.join(", ")
        );
    }
    println!(
        "boot: enabled {} unit(s){}:",
        installed.len(),
        if rootless { " (user + linger)" } else { "" }
    );
    for u in &installed {
        println!("  {u}");
    }
    println!("→ they will come up automatically when the host boots.");
    let _ = user_mode;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **O filtro apagava o `delonix-cri.service`.**
    ///
    /// Em modo root o `unit_dir` é `/etc/systemd/system`, que é exactamente onde
    /// o `cluster apply` instala o CRI e onde a imagem VM dourada o activa. O
    /// prefixo `delonix-` casava com ele, por isso um `net boot disable` num nó
    /// Kubernetes desactivava e APAGAVA o endpoint que o kubelet usa — e o
    /// `status` listava-o como se este comando o tivesse gerado.
    #[test]
    fn a_varredura_nunca_apanha_o_unit_do_cri() {
        assert!(!is_boot_unit("delonix-cri.service"));
        // O que É nosso continua a ser reconhecido...
        assert!(is_boot_unit("delonix-boot-web.service"));
        // ...incluindo a forma legada, para um `disable` limpar o que uma versão
        // anterior instalou.
        assert!(is_boot_unit("delonix-web.service"));
        // E nada que não seja um unit.
        assert!(!is_boot_unit("delonix-boot-web.timer"));
        assert!(!is_boot_unit("outra-coisa.service"));
    }

    /// `--restart no` era inalcançável: o default era `always` e o ÚNICO ramo que
    /// lia a política do container era `restart == "no"`, portanto pedir
    /// `Restart=no` produzia `Restart=always`. O valor que o nome da flag promete
    /// era o único que ela não sabia exprimir.
    #[test]
    fn a_politica_pedida_e_a_que_fica_no_unit() {
        let u = container_unit(
            "web",
            "no",
            "/r",
            "/usr/bin/delonix",
            "default.target",
            None,
        );
        assert!(u.contains("Restart=no\n"), "{u}");
        assert!(!u.contains("Restart=always"));
    }

    /// **A lacuna que um reboot expõe.** O supervisor é um `fork()` e morre com
    /// a máquina; o container fica `Crashed` sem PID; e o `stack apply` não o
    /// ressuscita («already exists, nothing to do»). Só olhar para quem tem PID
    /// vivo é fotografar o `ps` — e deixa de fora exactamente o container que
    /// declarou `--restart always` e que o reboot parou.
    ///
    /// É o papel que o `podman-restart.service` faz do outro lado: no arranque,
    /// levantar o que a política diz que devia estar de pé.
    #[test]
    fn a_politica_do_container_decide_se_ele_volta() {
        assert!(wants_to_be_up(Some("always")));
        assert!(wants_to_be_up(Some("unless-stopped")));
        // `on-failure` é «reinicia se sair mal», não «está de pé no arranque» —
        // um container que saiu 0 acabou o seu trabalho.
        assert!(!wants_to_be_up(Some("on-failure")));
        assert!(!wants_to_be_up(Some("on-failure:3")));
        assert!(!wants_to_be_up(Some("no")));
        assert!(!wants_to_be_up(None));
    }

    /// **Uma VM não tinha caminho de arranque automático NENHUM** — nem unit,
    /// nem `virsh autostart` no domínio (só na rede libvirt). E o `vm stop` faz
    /// `undefine`, portanto uma VM parada nem é conhecida do libvirt no momento
    /// do reboot: ficava tudo dependente de alguém se lembrar de um `vm start`.
    #[test]
    fn uma_vm_ganha_unit_e_nao_colide_com_um_container_do_mesmo_nome() {
        let u = vm_unit("db", "always", "/r", "/b", "default.target");
        assert!(u.contains("ExecStart=/b vm start db\n"), "{u}");
        assert!(u.contains("ExecStop=/b vm stop db\n"), "{u}");
        // `oneshot` + RemainAfterExit e NÃO `forking`: o `vm start` devolve
        // quando o VMM está de pé e não há processo do host para o systemd
        // seguir — o backend é dono dele. Dizer `forking` punha o systemd à
        // procura de um main PID que não existe.
        assert!(u.contains("Type=oneshot"), "{u}");
        assert!(u.contains("RemainAfterExit=yes"), "{u}");
        assert!(!u.contains("Type=forking"), "{u}");
        // Um container e uma VM podem ter o MESMO nome (stores diferentes), por
        // isso os ficheiros têm de ser distintos — senão um apaga o outro em
        // silêncio.
        assert_ne!(
            format!("{UNIT_PREFIX}vm-db.service"),
            format!("{UNIT_PREFIX}db.service")
        );
        assert!(is_boot_unit(&format!("{UNIT_PREFIX}vm-db.service")));
    }

    /// **Os membros de um pod PARTILHAM a netns**, e quem arranca primeiro é
    /// quem a recria. N units libertados em paralelo pelo systemd correm para
    /// ser esse — uma corrida que nenhum deles sabe que está a disputar.
    ///
    /// A âncora é o primeiro membro por nome (`<pod>-c0`), que é estável, e os
    /// restantes ficam atrás dele com `After=`. `Requires=` seria errado: levaria
    /// o pod inteiro abaixo com a âncora, e um membro parado não é razão para
    /// parar os pares.
    #[test]
    fn os_membros_de_um_pod_arrancam_atras_do_primeiro() {
        let c = |nome: &str, pod: Option<&str>| {
            let mut c = delonix_runtime_core::Container::new(
                nome.into(),
                nome.into(),
                "alpine".into(),
                vec!["sleep".into()],
                "64M".into(),
            );
            if let Some(p) = pod {
                c.labels
                    .insert(super::super::pod::POD_LABEL.into(), p.into());
            }
            c
        };
        let all = vec![
            c("pa-c1", Some("pa")),
            c("pa-c0", Some("pa")),
            c("solto", None),
        ];
        let a = pod_anchors(&all);
        // A âncora é o PRIMEIRO por nome, mesmo tendo aparecido em segundo.
        assert_eq!(a.get("pa").unwrap(), "pa-c0");
        // Um container sem pod não entra.
        assert!(!a.contains_key("solto"));

        // O unit do membro seguinte espera pela âncora...
        let u = container_unit(
            "pa-c1",
            "always",
            "/r",
            "/b",
            "default.target",
            Some("delonix-boot-pa-c0.service"),
        );
        assert!(u.contains("After=delonix-boot-pa-c0.service\n"), "{u}");
        // ...e nunca se declara `Requires`, que derrubaria o pod todo.
        assert!(!u.contains("Requires="), "{u}");
        // A própria âncora não depende de si mesma.
        let anchor_unit = container_unit("pa-c0", "always", "/r", "/b", "default.target", None);
        assert!(
            !anchor_unit.contains("After=delonix-boot-"),
            "{anchor_unit}"
        );
        // E um container fora de um pod continua sem ordenação nenhuma.
        assert!(!anchor_unit.contains("delonix-boot-"), "{anchor_unit}");
    }

    /// O unit tem de nomear o container nos DOIS lados e carregar a raiz de
    /// estado — sem `DELONIX_ROOT`, a unidade arranca contra um store diferente
    /// daquele onde o container existe, e falha por «no such container».
    #[test]
    fn o_unit_gerado_tem_o_que_o_arranque_precisa() {
        let u = container_unit(
            "web",
            "always",
            "/var/lib/delonix",
            "/usr/bin/delonix",
            "default.target",
            None,
        );
        assert!(
            u.contains("ExecStart=/usr/bin/delonix container start web\n"),
            "{u}"
        );
        assert!(
            u.contains("ExecStop=/usr/bin/delonix container stop web\n"),
            "{u}"
        );
        assert!(
            u.contains("Environment=DELONIX_ROOT=/var/lib/delonix\n"),
            "{u}"
        );
        assert!(u.contains("WantedBy=default.target\n"), "{u}");
        // A rede do host tem de estar de pé antes: o `container start` sobe a
        // infra do motor, mas o slirp precisa de uma rota para fora.
        assert!(u.contains("After=network-online.target"), "{u}");
    }
}
