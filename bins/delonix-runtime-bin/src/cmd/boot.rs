//! `delonix system boot` — install systemd units so the running containers
//! come back UP automatically when the host boots, with no manual restart.
//! Moved here from `net boot` (B2 of the CLI restructuring): this is about
//! the ENGINE surviving a reboot, not about SDN plumbing.
//!
//! Rootless installs USER units + linger (start at boot without a login);
//! root installs system units. There's no daemon: each unit's `ExecStart` is
//! `delonix container start <name>` and `ExecStop` is `delonix container stop`.

use clap::Subcommand;
use delonix_linux::{self as runtime};
use delonix_model::Result;
use delonix_oci::ImageStore;
use delonix_state::Store;

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

/// Is this a unit `system boot enable` generated, judged by its NAME alone?
///
/// Only the current shape. The legacy `delonix-<name>.service` form is handled
/// by [`claims_unit`], which reads the file: by name it is indistinguishable
/// from any other `delonix-*.service` in a shared unit directory.
fn is_boot_unit(name: &str) -> bool {
    if !name.ends_with(".service") {
        return false;
    }
    name.starts_with(UNIT_PREFIX)
}

/// The engine's own marker, written into every unit this command generates
/// (`container_unit`, `vm_unit`). Nothing a user writes by hand carries it.
const OWN_MARKER: &str = "DELONIX_INTERNAL=1";

/// Does this command own the unit at `path`, and may it therefore DELETE it?
///
/// Name alone is not enough, and that is the whole point. `enable` names its
/// units `delonix-boot-` ([`UNIT_PREFIX`]), but a `disable` also has to clean
/// up what a version before the prefix installed — so the rule used to accept
/// any `delonix-*.service`, with `delonix-cri.service` and
/// `delonix-node-api.service` carved out BY NAME. The second was added after
/// `serve node-api --install-units` began writing into this very directory and
/// a `disable` deleted the service its socket unit starts.
///
/// A blocklist cannot keep up, because this directory is shared with
/// everything else the user runs. Measured on a maintainer's host 2026-10-18:
/// `~/.config/systemd/user/` held `delonix-issues-sync.service` — tooling with
/// nothing to do with bringing containers back after a reboot — and the legacy
/// clause claimed it. `system boot disable` would have REMOVED it
/// (`std::fs::remove_file`, in the `Disable` arm), and this is the one leaf of
/// the nineteen unexecuted ones with zero checks.
///
/// So the legacy shape is now identified by CONTENT: a `delonix-*.service` that
/// carries [`OWN_MARKER`] was written by this engine and is ours to remove; one
/// that does not is somebody else's and is left alone. An unreadable file is
/// NOT claimed — erring toward a leftover an operator can delete, never toward
/// a deletion they cannot undo.
fn claims_unit(name: &str, path: &std::path::Path) -> bool {
    if is_boot_unit(name) {
        return true;
    }
    if !name.ends_with(".service") || !name.starts_with("delonix-") {
        return false;
    }
    // Kept even though the marker test below would also exclude them: these two
    // are load-bearing (the kubelet's CRI endpoint, and the node API's socket
    // unit), and a regression here is worse than a redundant check.
    if name == "delonix-cri.service" || name == "delonix-node-api.service" {
        return false;
    }
    std::fs::read_to_string(path)
        .map(|t| t.contains(OWN_MARKER))
        .unwrap_or(false)
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
                    if claims_unit(&name, &e.path()) {
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
                    if claims_unit(&name, &e.path()) {
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

/// The unit each pod's members must start after: the pod's member zero — the
/// first entry of `spec.containers`, via [`super::pod::member_order`].
///
/// **The comment that used to sit here said «the FIRST container by name, which
/// is stable (`<pod>-c0`, `-c1`, …) and is the one that holds the shared
/// namespaces», and both halves were wrong.** `<pod>-c0` is only the fallback
/// name a member gets when the manifest names none; the moment members ARE named
/// — the normal case — the name orders alphabetically and `create_pod`'s order is
/// lost. Measured live against a pod of `web` (first) and `api` (second): the
/// generated `After=` pointed at `api`, ordering `web` — the member that actually
/// holds the pod's IPC/UTS — behind its peer.
///
/// [`super::pod::member_order`] is the same key `ls`, `exec` and `describe` use,
/// deliberately: «the pod's first member» has to mean ONE thing across the CLI,
/// and this was the last place still answering it differently. It also carries
/// the legacy fallback — a member with no index sorts last and the name decides,
/// which for those pods is the answer this function always gave.
fn pod_anchors(
    containers: &[delonix_compute::Container],
) -> std::collections::BTreeMap<String, String> {
    let mut first: std::collections::BTreeMap<String, (u32, String)> = Default::default();
    for c in containers {
        let Some(pod) = c.labels.get(super::pod::POD_LABEL) else {
            continue;
        };
        let key = super::pod::member_order(c);
        first
            .entry(pod.clone())
            .and_modify(|cur| {
                if key < *cur {
                    *cur = key.clone();
                }
            })
            .or_insert(key);
    }
    first.into_iter().map(|(p, (_, n))| (p, n)).collect()
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
        let alive = c.is_live();
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
        let alive = vm.status == delonix_model::records::Status::Running;
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
            if claims_unit(&name, &e.path()) && !installed.contains(&name) {
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
        // Written by `serve node-api --install-units` into the same directory.
        assert!(!is_boot_unit("delonix-node-api.service"));
        // O que É nosso continua a ser reconhecido...
        assert!(is_boot_unit("delonix-boot-web.service"));
        // The legacy shape is no longer recognised BY NAME: it moved to
        // `claims_unit`, which reads the file, because by name it cannot be
        // told apart from any other `delonix-*.service` in the shared unit
        // directory (see `disable_only_claims_the_units_enable_itself_wrote`).
        assert!(!is_boot_unit("delonix-web.service"));
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

    /// `system boot disable` REMOVES every file it calls a boot unit, so saying
    /// yes to a unit this command did not write is a `remove_file` on somebody
    /// else's service.
    ///
    /// It did, and by a blocklist: the rule was `starts_with(UNIT_PREFIX) ||
    /// starts_with("delonix-")`, with `delonix-cri.service` and
    /// `delonix-node-api.service` enumerated as exceptions. Measured on this
    /// host 2026-10-18, `~/.config/systemd/user/` held
    /// `delonix-issues-sync.service` — a unit of the maintainer's own tooling,
    /// nothing to do with bringing containers back after a reboot — and
    /// `is_boot_unit` said yes to it. A `system boot disable` would have
    /// deleted it, and this is the one leaf of the nineteen with ZERO checks.
    ///
    /// A blocklist cannot keep up: the comment above it records two names
    /// already added after the fact. What this command owns is what it names,
    /// and it names `delonix-boot-` (see `enable`). Anything else is left on
    /// disk — a leftover is recoverable, a deleted unit is not.
    #[test]
    fn disable_only_claims_the_units_enable_itself_wrote() {
        let dir = std::env::temp_dir().join(format!("dlx-boot-claims-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, body: &str| {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            p
        };

        // Ours, by the current prefix — claimed whatever the content.
        let own = write(&format!("{UNIT_PREFIX}db.service"), "[Service]\n");
        assert!(claims_unit(&format!("{UNIT_PREFIX}db.service"), &own));

        // Ours, in the LEGACY shape: claimed because the file carries the
        // engine's marker, which is what `container_unit`/`vm_unit` write.
        let legacy = write(
            "delonix-web.service",
            "[Service]\nEnvironment=DELONIX_INTERNAL=1\nExecStart=/x container start web\n",
        );
        assert!(
            claims_unit("delonix-web.service", &legacy),
            "a pre-prefix unit this engine wrote still has to be cleanable"
        );

        // Somebody else's, same `delonix-` name shape, no marker. Claiming any
        // of these means `std::fs::remove_file` on a service we did not write.
        for foreign in [
            "delonix-issues-sync.service", // measured on a maintainer's host
            "delonix-gc.service",
            "delonix-regulate.service", // `system regulate --install-timer`
        ] {
            let p = write(foreign, "[Service]\nExecStart=/usr/bin/true\n");
            assert!(
                !claims_unit(foreign, &p),
                "{foreign} was not written by this command, and `disable` DELETES what it claims"
            );
        }

        // The two load-bearing ones stay excluded even if a marker shows up.
        for pinned in ["delonix-cri.service", "delonix-node-api.service"] {
            let p = write(pinned, "[Service]\nEnvironment=DELONIX_INTERNAL=1\n");
            assert!(!claims_unit(pinned, &p), "{pinned} is load-bearing");
        }

        // A file we cannot read is not claimed: a leftover beats a deletion.
        assert!(!claims_unit(
            "delonix-gone.service",
            &dir.join("delonix-gone.service")
        ));

        // Timers were never claimed, and stay unclaimed.
        assert!(!claims_unit("delonix-issues-sync.timer", &dir.join("x")));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// **A pod's members SHARE the netns**, and whoever starts first is the one
    /// that recreates it. N units released in parallel by systemd race to be
    /// that one — a race none of them knows it is in.
    ///
    /// This is the LEGACY half: members with no [`super::super::pod::POD_INDEX_LABEL`],
    /// i.e. a pod created by a binary from before the label existed. There the
    /// only order available is the name, and it is still used. `Requires=` would
    /// be wrong either way: it would take the whole pod down with the anchor, and
    /// a member that stops is not a reason to stop its peers.
    #[test]
    fn os_membros_de_um_pod_arrancam_atras_do_primeiro() {
        let c = |nome: &str, pod: Option<&str>| {
            let mut c = delonix_compute::Container::new(
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
        // With no index to read, the name is what is left — and it is still
        // right for these, whose names ARE the creation order (`c0`, `c1`, …).
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

    /// **The anchor used to be the smallest NAME, and the holder is the first
    /// entry of `spec.containers`** — two different members the moment a
    /// manifest names them. Measured live against a pod of `web` (first) and
    /// `api` (second): `system boot enable` wrote `After=delonix-boot-demo-api`
    /// into `demo-web`'s unit, ordering the member that holds the pod's IPC/UTS
    /// behind its peer.
    ///
    /// The pair below is exactly that one, and it is the pair the legacy test
    /// above cannot distinguish: it is reverse-alphabetical, so name order and
    /// manifest order disagree, and only reading
    /// [`super::super::pod::POD_INDEX_LABEL`] gets it right.
    #[test]
    fn the_anchor_is_manifest_member_zero_not_the_smallest_name() {
        let member = |name: &str, pod: &str, idx: usize| {
            let mut c = delonix_compute::Container::new(
                name.into(),
                name.into(),
                "alpine".into(),
                vec!["sleep".into()],
                "64M".into(),
            );
            c.labels
                .insert(super::super::pod::POD_LABEL.into(), pod.into());
            c.labels
                .insert(super::super::pod::POD_INDEX_LABEL.into(), idx.to_string());
            c
        };
        // `web` is member zero and holds the shared IPC/UTS; `api` sorts first.
        let all = vec![member("demo-api", "demo", 1), member("demo-web", "demo", 0)];
        assert_eq!(pod_anchors(&all).get("demo").unwrap(), "demo-web");

        // A pod half-migrated — one member stamped by this binary, one written by
        // an older one — still puts the stamped member zero in front, because an
        // absent index sorts last rather than being guessed at.
        let mut legacy = member("demo-api", "demo", 1);
        legacy.labels.remove(super::super::pod::POD_INDEX_LABEL);
        assert_eq!(
            pod_anchors(&[legacy, member("demo-web", "demo", 0)])
                .get("demo")
                .unwrap(),
            "demo-web"
        );
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
