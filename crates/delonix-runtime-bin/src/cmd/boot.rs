//! `delonix boot` — install systemd units so the running containers come back
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
    /// Install + enable systemd units for the RUNNING containers, so they come
    /// back up when the host boots. Rootless uses user units + linger.
    Enable {
        /// Restart policy baked into the units (`no|on-failure[:max]|always|unless-stopped`).
        #[arg(long, default_value = "always")]
        restart: String,
    },
    /// Disable + remove the generated boot units.
    Disable,
    /// Show boot-persistence status (installed units + mode).
    Status,
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
                    if name.starts_with("delonix-") && name.ends_with(".service") {
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
                    if name.starts_with("delonix-") && name.ends_with(".service") {
                        let on = sysctl(&["is-enabled", "--quiet", &name]);
                        println!("  {name}  [{}]", if on { "enabled" } else { "disabled" });
                        any = true;
                    }
                }
            }
            if !any {
                println!("  (no boot units — run `delonix boot enable`)");
            }
            Ok(())
        }
    }
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
    restart: String,
    sysctl: &dyn Fn(&[&str]) -> bool,
) -> Result<()> {
    std::fs::create_dir_all(unit_dir)?;
    let mut installed: Vec<String> = Vec::new();
    // One unit per RUNNING container (those are the ones that should come back up).
    for c in store.list()? {
        if !c.pid.map(runtime::is_alive).unwrap_or(false) {
            continue;
        }
        let rp = if restart == "no" {
            c.restart_policy.as_deref().unwrap_or("always").to_string()
        } else {
            restart.clone()
        };
        let unit = format!(
            "[Unit]\nDescription=Delonix container {name}\nAfter=network-online.target\nWants=network-online.target\n\n\
             [Service]\nType=forking\nRestart={rp}\nTimeoutStopSec=15\nEnvironment=DELONIX_INTERNAL=1\nEnvironment=DELONIX_ROOT={root}\n\
             ExecStart={exe} container start {name}\nExecStop={exe} container stop {name}\n\n\
             [Install]\nWantedBy={wb}\n",
            name = c.name,
            rp = rp,
            root = root,
            exe = exe,
            wb = wanted_by,
        );
        std::fs::write(unit_dir.join(format!("delonix-{}.service", c.name)), unit)?;
        installed.push(format!("delonix-{}.service", c.name));
    }
    if installed.is_empty() {
        println!("boot: no running containers — start them first, then `delonix boot enable`.");
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
