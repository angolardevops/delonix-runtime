//! `delonix-runtime` — o **executor de containers** do Delonix, empacotado como um
//! binário LEAN só-runtime (contexto de nó/CRI), sem a API/Console/orquestrador do
//! PaaS. Puxa imagens OCI, prepara o rootfs e corre o container em namespaces
//! (modelo runc-like). É o binário que o DaemonSet-engine do delonix-paas corre em
//! cada nó do cluster k8s; o control-plane/UX completos ficam no binário `delonix`.
//!
//! Reutiliza as MESMAS crates do motor (`delonix-runtime`, `delonix-image`,
//! `delonix-core`) — não há um segundo runtime, só uma fachada mínima por cima do
//! mesmo código já validado.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use delonix_core::{generate_id, Container, Error, Result, Store};
use delonix_image::{Image, ImageStore};
use delonix_runtime::{self as runtime, RunSpec};

#[derive(Parser)]
#[command(
    name = "delonix-runtime",
    version,
    about = "Delonix runtime — executor de containers OCI (nó/CRI), sem o control-plane do PaaS"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Corre um container a partir de uma imagem (puxa se faltar).
    Run {
        /// Corre em segundo plano e imprime o ID.
        #[arg(short, long)]
        detach: bool,
        /// Nome do container (default: `dlx-<id>`).
        #[arg(long)]
        name: Option<String>,
        /// Rede: `host` (partilha a do host, default) ou `none` (netns isolado).
        #[arg(long, default_value = "host")]
        net: String,
        /// Container privilegiado (todas as caps, seccomp off) — cargas de confiança.
        #[arg(long)]
        privileged: bool,
        /// Variáveis de ambiente adicionais (`KEY=VAL`), repetível.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Imagem (ex.: `alpine:3.19`).
        image: String,
        /// Comando + argumentos (default: o ENTRYPOINT/CMD da imagem).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Lista containers.
    Ps {
        /// Inclui os parados/falhados.
        #[arg(short, long)]
        all: bool,
    },
    /// Pára um container (SIGTERM, depois SIGKILL).
    Stop {
        id: String,
        /// Segundos até ao SIGKILL.
        #[arg(short, long, default_value_t = 10)]
        time: u64,
    },
    /// Remove um container.
    Rm {
        id: String,
        /// Força (mata se estiver a correr).
        #[arg(short, long)]
        force: bool,
    },
    /// Executa um comando dentro de um container a correr.
    Exec {
        /// Interativo (liga o stdin).
        #[arg(short = 'i', long)]
        interactive: bool,
        /// Aloca um pseudo-terminal.
        #[arg(short = 't', long)]
        tty: bool,
        id: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Mostra os logs (containers detached).
    Logs { id: String },
    /// Puxa uma imagem de um registo.
    Pull { image: String },
    /// Lista imagens locais.
    Images,
    /// Remove uma imagem local.
    Rmi { image: String },
    /// Exporta um bundle OCI runtime (rootfs + config.json) para `runc`/`crun`.
    Bundle { image: String, dir: PathBuf },
}

/// Raiz de estado do runtime: `$DELONIX_ROOT` ou o default do `ImageStore`.
fn state_root() -> PathBuf {
    std::env::var_os("DELONIX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(ImageStore::default_root)
}

fn open_stores() -> Result<(ImageStore, Store)> {
    let root = state_root();
    let images = ImageStore::open(&root)?;
    let store = Store::open(root.join("containers"))?;
    Ok((images, store))
}

/// Resolve uma imagem local; se faltar, puxa-a do registo.
fn resolve_or_pull(images: &ImageStore, reference: &str) -> Result<Image> {
    match images.resolve(reference) {
        Ok(img) => Ok(img),
        Err(_) => {
            eprintln!("a puxar {reference}…");
            delonix_image::pull_from_registry(images, reference)
        }
    }
}

/// Comando efetivo (função pura): ENTRYPOINT + (args do utilizador, senão CMD da
/// imagem) — a mesma semântica do Docker/OCI (o `run <cmd>` substitui o CMD, não o
/// ENTRYPOINT).
fn compose_command(entrypoint: &[String], cmd: &[String], user: &[String]) -> Vec<String> {
    let mut v = entrypoint.to_vec();
    if user.is_empty() {
        v.extend(cmd.iter().cloned());
    } else {
        v.extend(user.iter().cloned());
    }
    v
}

/// Como [`compose_command`], mas a partir da config da imagem.
fn effective_command(img: &Image, user: &[String]) -> Vec<String> {
    compose_command(&img.config.entrypoint, &img.config.cmd, user)
}

/// `chown -R <uid>:<uid>` de um rootfs FLAT (rootless): sem isto, os ficheiros
/// pertencem ao uid 0 do host, que fica não-mapeado dentro do user namespace.
/// Delega em `delonix_runtime::lchown_tree` (usa `lchown`, nunca segue symlinks —
/// ver nota de segurança lá; não reimplementar isto localmente com
/// `std::os::unix::fs::chown`, que segue symlinks).
fn chown_tree(path: &Path, uid: u32) -> Result<()> {
    delonix_runtime::lchown_tree(path, uid, uid);
    Ok(())
}

/// Localiza um container pelo prefixo do ID ou pelo nome exato.
fn find(store: &Store, q: &str) -> Result<Container> {
    let all = store.list()?;
    all.into_iter()
        .find(|c| c.id == q || c.id.starts_with(q) || c.name == q)
        .ok_or_else(|| Error::Invalid(format!("container não encontrado: {q}")))
}

fn cmd_run(
    images: &ImageStore,
    store: &Store,
    detach: bool,
    name: Option<String>,
    net: &str,
    privileged: bool,
    env: Vec<String>,
    image: String,
    command: Vec<String>,
) -> Result<()> {
    if net != "host" && net != "none" {
        return Err(Error::Invalid(format!("--net inválido: {net} (use host|none)")));
    }
    let img = resolve_or_pull(images, &image)?;
    let id = generate_id();
    let rootless = runtime::is_rootless();
    // ROOTLESS → rootfs FLAT (o overlay precisa de root) + user namespace;
    // ROOT → overlay via mount_rootfs. (Mesma regra do binário `delonix`.)
    let rootfs = if rootless {
        let rfs = images.root().join("containers").join(&id).join("rootfs");
        images.export_rootfs(&img, &rfs)?;
        chown_tree(&rfs, runtime::USERNS_UID_BASE)?;
        rfs.to_string_lossy().into_owned()
    } else {
        images.mount_rootfs(&img, &id)?.to_string_lossy().into_owned()
    };

    let cmd = effective_command(&img, &command);
    if cmd.is_empty() {
        return Err(Error::Invalid("sem comando (a imagem não define ENTRYPOINT/CMD)".into()));
    }
    let cname = name.unwrap_or_else(|| format!("dlx-{}", &id[..8.min(id.len())]));
    // `max` = sem teto de memória (cgroup v2); em k8s o cgroup do pod já limita.
    let mut c = Container::new(id.clone(), cname, image.clone(), cmd, "max".into());
    c.env = img.config.env.clone();
    c.env.extend(env);
    if !img.config.working_dir.is_empty() {
        c.workdir = Some(img.config.working_dir.clone());
    }
    c.userns = rootless;
    c.privileged = privileged;

    let log_path = if detach {
        Some(
            images
                .root()
                .join("containers")
                .join(&id)
                .join("log")
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        None
    };
    let spec = RunSpec {
        detach,
        new_netns: net == "none",
        userns: c.userns,
        log_path,
        ..Default::default()
    };
    runtime::create_with(store, &mut c, &rootfs, &spec)?;
    if detach {
        println!("{id}");
    }
    Ok(())
}

fn cmd_ps(store: &Store, all: bool) -> Result<()> {
    let mut cs = store.list()?;
    println!("{:<14}  {:<20}  {:<24}  {}", "CONTAINER ID", "NAME", "IMAGE", "STATUS");
    for c in cs.iter_mut() {
        if runtime::reconcile_status(c) {
            let _ = store.save(c);
        }
        let hidden = matches!(c.status, delonix_core::Status::Failed(_) | delonix_core::Status::Crashed);
        if !all && hidden {
            continue;
        }
        println!(
            "{:<14}  {:<20}  {:<24}  {:?}",
            &c.id[..12.min(c.id.len())],
            c.name,
            c.image,
            c.status
        );
    }
    Ok(())
}

fn cmd_stop(store: &Store, id: &str, time: u64) -> Result<()> {
    let mut c = find(store, id)?;
    runtime::stop(store, &mut c, time)?;
    println!("{}", c.id);
    Ok(())
}

fn cmd_rm(images: &ImageStore, store: &Store, id: &str, force: bool) -> Result<()> {
    let c = find(store, id)?;
    runtime::remove(store, &c, force)?;
    let _ = images.unmount_rootfs(&c.id);
    println!("{}", c.id);
    Ok(())
}

fn cmd_exec(store: &Store, id: &str, interactive: bool, tty: bool, command: &[String]) -> Result<()> {
    let c = find(store, id)?;
    let _ = interactive; // o stdin é herdado; a flag mantém a paridade de CLI
    let code = runtime::exec(&c, command, tty)?;
    std::process::exit(code);
}

fn cmd_logs(images: &ImageStore, store: &Store, id: &str) -> Result<()> {
    let c = find(store, id)?;
    let p = images.root().join("containers").join(&c.id).join("log");
    match std::fs::read(&p) {
        Ok(b) => {
            use std::io::Write;
            std::io::stdout().write_all(&b).ok();
            Ok(())
        }
        Err(_) => Err(Error::Invalid(format!(
            "sem logs para {} (só há logs em containers detached)",
            c.name
        ))),
    }
}

fn cmd_pull(images: &ImageStore, reference: &str) -> Result<()> {
    let img = delonix_image::pull_from_registry(images, reference)?;
    println!("{}", img.short_id());
    Ok(())
}

fn cmd_images(images: &ImageStore) -> Result<()> {
    println!("{:<24}  {:<16}  {}", "REPOSITORY:TAG", "IMAGE ID", "CRIADA(unix)");
    for img in images.list()? {
        let tag = img.repo_tags.first().cloned().unwrap_or_else(|| "<none>".into());
        println!("{:<24}  {:<16}  {}", tag, img.short_id(), img.created_unix);
    }
    Ok(())
}

fn cmd_rmi(images: &ImageStore, reference: &str) -> Result<()> {
    let removed = images.remove(reference)?;
    println!("{removed}");
    Ok(())
}

/// Escreve um bundle OCI runtime mínimo (rootfs + config.json) para `runc`/`crun`.
fn cmd_bundle(images: &ImageStore, reference: &str, dir: &Path) -> Result<()> {
    let img = resolve_or_pull(images, reference)?;
    std::fs::create_dir_all(dir).map_err(|e| Error::Invalid(format!("mkdir {}: {e}", dir.display())))?;
    let rootfs = dir.join("rootfs");
    images.export_rootfs(&img, &rootfs)?;
    let args = effective_command(&img, &[]);
    let args = if args.is_empty() { vec!["/bin/sh".to_string()] } else { args };
    let cwd = if img.config.working_dir.is_empty() {
        "/".to_string()
    } else {
        img.config.working_dir.clone()
    };
    let spec = serde_json::json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "user": { "uid": 0, "gid": 0 },
            "args": args,
            "env": img.config.env,
            "cwd": cwd,
            "capabilities": {
                "bounding": ["CAP_CHOWN","CAP_DAC_OVERRIDE","CAP_FOWNER","CAP_SETGID","CAP_SETUID","CAP_NET_BIND_SERVICE"]
            },
            "noNewPrivileges": true
        },
        "root": { "path": "rootfs", "readonly": false },
        "hostname": "delonix",
        "linux": {
            "namespaces": [
                {"type": "pid"}, {"type": "ipc"}, {"type": "uts"}, {"type": "mount"}
            ]
        }
    });
    let cfg = dir.join("config.json");
    std::fs::write(&cfg, serde_json::to_vec_pretty(&spec).unwrap_or_default())
        .map_err(|e| Error::Invalid(format!("escrever {}: {e}", cfg.display())))?;
    println!("bundle OCI em {}", dir.display());
    println!("corre com:  runc run -b {} delonix-oci", dir.display());
    Ok(())
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let (images, store) = open_stores()?;
    match cli.cmd {
        Cmd::Run { detach, name, net, privileged, env, image, command } => {
            cmd_run(&images, &store, detach, name, &net, privileged, env, image, command)
        }
        Cmd::Ps { all } => cmd_ps(&store, all),
        Cmd::Stop { id, time } => cmd_stop(&store, &id, time),
        Cmd::Rm { id, force } => cmd_rm(&images, &store, &id, force),
        Cmd::Exec { interactive, tty, id, command } => cmd_exec(&store, &id, interactive, tty, &command),
        Cmd::Logs { id } => cmd_logs(&images, &store, &id),
        Cmd::Pull { image } => cmd_pull(&images, &image),
        Cmd::Images => cmd_images(&images),
        Cmd::Rmi { image } => cmd_rmi(&images, &image),
        Cmd::Bundle { image, dir } => cmd_bundle(&images, &image, &dir),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("delonix-runtime: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::compose_command;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn user_args_replace_cmd_but_keep_entrypoint() {
        // ENTRYPOINT=["/docker-entrypoint.sh"], CMD=["nginx"], run ... sh -c ...
        let ep = v(&["/docker-entrypoint.sh"]);
        let cmd = v(&["nginx", "-g", "daemon off;"]);
        assert_eq!(
            compose_command(&ep, &cmd, &v(&["sh", "-c", "echo hi"])),
            v(&["/docker-entrypoint.sh", "sh", "-c", "echo hi"])
        );
    }

    #[test]
    fn no_user_args_uses_cmd() {
        assert_eq!(
            compose_command(&v(&["/entry"]), &v(&["serve"]), &[]),
            v(&["/entry", "serve"])
        );
    }

    #[test]
    fn plain_cmd_without_entrypoint() {
        assert_eq!(compose_command(&[], &v(&["sleep", "1"]), &[]), v(&["sleep", "1"]));
        assert_eq!(compose_command(&[], &[], &v(&["sh"])), v(&["sh"]));
    }
}
