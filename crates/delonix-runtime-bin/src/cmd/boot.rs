//! `delonix net boot` — install systemd units so the running containers come back
//! UP automatically when the host boots, with no manual restart.
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
    /// Install + enable systemd units for the RUNNING containers.
    ///
    /// So they come back up when the host boots. Rootless uses user units +
    /// linger.
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

/// Is this a unit `net boot enable` generated?
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
                println!("  (no boot units — run `delonix net boot enable`)");
            }
            Ok(())
        }
    }
}

/// The unit text for one container. **Pure, so it is testable** — the same
/// reason `etcd::build_etcd_unit` is pure. This generator had no test at all,
/// and it is the artefact that decides whether a host comes back up.
fn container_unit(name: &str, rp: &str, root: &str, exe: &str, wanted_by: &str) -> String {
    format!(
        "[Unit]\nDescription=Delonix container {name}\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nType=forking\nRestart={rp}\nTimeoutStopSec=15\nEnvironment=DELONIX_INTERNAL=1\nEnvironment=DELONIX_ROOT={root}\n\
         ExecStart={exe} container start {name}\nExecStop={exe} container stop {name}\n\n\
         [Install]\nWantedBy={wanted_by}\n",
    )
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
    let mut installed: Vec<String> = Vec::new();
    // One unit per RUNNING container (those are the ones that should come back up).
    for c in store.list()? {
        if !c.pid.map(runtime::is_alive).unwrap_or(false) {
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
        let unit = container_unit(&c.name, &rp, root, exe, wanted_by);
        let unit_name = format!("{UNIT_PREFIX}{}.service", c.name);
        std::fs::write(unit_dir.join(&unit_name), unit)?;
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
        println!("boot: no running containers — start them first, then `delonix net boot enable`.");
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
        let u = container_unit("web", "no", "/r", "/usr/bin/delonix", "default.target");
        assert!(u.contains("Restart=no\n"), "{u}");
        assert!(!u.contains("Restart=always"));
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
