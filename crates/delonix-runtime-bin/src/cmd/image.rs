//! `delonix image` — pull/ls/rm/export.

use std::path::PathBuf;

use clap::Subcommand;
use delonix_image::ImageStore;
use delonix_runtime_core::{Error, Result};
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::util::{effective_command, open_stores, resolve_or_pull};

/// `spec` de `kind: Image` — ou `pull: <ref>`, ou `build: {...}` (mutuamente
/// exclusivos; erro claro se faltarem os dois).
#[derive(Debug, Deserialize)]
struct ImageSpec {
    pull: Option<String>,
    build: Option<BuildSpec>,
}

#[derive(Debug, Deserialize)]
struct BuildSpec {
    #[serde(default = "default_context")]
    context: PathBuf,
    file: Option<PathBuf>,
    tag: String,
}

fn default_context() -> PathBuf {
    PathBuf::from(".")
}

#[derive(Subcommand)]
pub enum ImageCmd {
    /// Puxa uma imagem de um registo.
    Pull { image: String },
    /// Lista imagens locais.
    Ls,
    /// Remove uma imagem local.
    Rm { image: String },
    /// Exporta um bundle OCI runtime (rootfs + config.json) para `runc`/`crun`.
    Export { image: String, dir: PathBuf },
    /// Aplica os documentos `kind: Image` de um manifesto (`pull` idempotente
    /// por referência; `build` reconstrói e substitui a tag a cada apply).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Autentica num registo OCI (guarda as credenciais em `<root>/auth.json`,
    /// formato docker/podman). A password vem SEMPRE do stdin — nunca de um
    /// argumento (ficaria no histórico da shell e no /proc).
    Login {
        /// Registo (ex.: `ghcr.io`, `docker.io`).
        registry: String,
        #[arg(short = 'u', long = "username")]
        username: String,
        /// Lê a password/token do stdin (única forma suportada).
        #[arg(long = "password-stdin")]
        password_stdin: bool,
    },
    /// Remove as credenciais guardadas de um registo.
    Logout { registry: String },
    /// (só com `--vm`) Publica uma imagem VM local num registo OCI.
    Push { name: String, target: String },
    /// (só com `--vm`) Constrói a imagem VM dourada (Ubuntu + kubeadm/kubelet/
    /// kubectl + `delonix-cri`).
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        #[arg(long, default_value = "26.04")]
        ubuntu_release: String,
        #[arg(long)]
        k8s_version: Option<String>,
        #[arg(long = "extra-package")]
        extra_packages: Vec<String>,
        #[arg(long = "extra-run")]
        extra_run: Vec<String>,
        #[arg(long)]
        cri_bin: Option<PathBuf>,
    },
}

/// `vm`: activa `--vm` no grupo `image` — despacha `ls`/`pull`/`push`/`build`
/// para `cmd::vmimage` (imagens VM douradas) em vez de `ImageStore` (imagens
/// de container). `rm`/`export`/`apply` não fazem sentido para imagens VM
/// nesta fase — erro claro em vez de um comportamento silenciosamente errado.
pub fn run(vm: bool, action: ImageCmd) -> Result<()> {
    // login/logout são agnósticos a container-vs-VM (mesmo auth.json).
    match &action {
        ImageCmd::Login { registry, username, password_stdin } => {
            return cmd_login(registry, username, *password_stdin);
        }
        ImageCmd::Logout { registry } => {
            delonix_image::auth::logout(&super::util::state_root(), registry)?;
            println!("credenciais de {registry} removidas");
            return Ok(());
        }
        _ => {}
    }
    if vm {
        return run_vm(action);
    }
    let (images, _store) = open_stores()?;
    match action {
        ImageCmd::Pull { image } => cmd_pull(&images, &image),
        ImageCmd::Ls => cmd_ls(&images),
        ImageCmd::Rm { image } => cmd_rm(&images, &image),
        ImageCmd::Export { image, dir } => cmd_export(&images, &image, &dir),
        ImageCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
        ImageCmd::Push { .. } | ImageCmd::Build { .. } => {
            Err(Error::Invalid("push/build de imagens são só para VM — usa `delonix image --vm push|build`".into()))
        }
        ImageCmd::Login { .. } | ImageCmd::Logout { .. } => unreachable!("tratados acima"),
    }
}

/// `image login` — lê a password do stdin (obrigatório: um argumento ficaria no
/// histórico da shell e visível em /proc) e delega no `delonix_image::auth`.
fn cmd_login(registry: &str, username: &str, password_stdin: bool) -> Result<()> {
    if !password_stdin {
        return Err(Error::Invalid(
            "usa --password-stdin (ex.: `gh auth token | delonix image login ghcr.io -u USER --password-stdin`)".into(),
        ));
    }
    let mut pw = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut pw)
        .map_err(|e| Error::Invalid(format!("a ler a password do stdin: {e}")))?;
    let pw = pw.trim();
    if pw.is_empty() {
        return Err(Error::Invalid("password vazia no stdin".into()));
    }
    delonix_image::auth::login(&super::util::state_root(), registry, username, pw)?;
    println!("login em {registry} guardado (auth.json)");
    Ok(())
}

fn run_vm(action: ImageCmd) -> Result<()> {
    use super::vmimage::{self, VmImageCmd};
    let mapped = match action {
        ImageCmd::Ls => VmImageCmd::Ls,
        ImageCmd::Pull { image } => VmImageCmd::Pull { source: image, name: None },
        ImageCmd::Push { name, target } => VmImageCmd::Push { name, target },
        ImageCmd::Build { tag, ubuntu_release, k8s_version, extra_packages, extra_run, cri_bin } => {
            VmImageCmd::Build { tag, ubuntu_release, k8s_version, extra_packages, extra_run, cri_bin }
        }
        ImageCmd::Rm { .. } | ImageCmd::Export { .. } | ImageCmd::Apply { .. } => {
            return Err(Error::Invalid(
                "comando não disponível para imagens VM (--vm) — usa ls/pull/push/build".into(),
            ))
        }
        ImageCmd::Login { .. } | ImageCmd::Logout { .. } => unreachable!("tratados em run()"),
    };
    vmimage::run(mapped)
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (images, _store) = open_stores()?;
    for doc in manifest::of_kind(docs, "Image") {
        let name = &doc.metadata.name;
        let spec: ImageSpec = manifest::spec_of(doc)?;
        match (spec.pull, spec.build) {
            (Some(reference), None) => {
                resolve_or_pull(&images, &reference)?;
                println!("image/{name}: garantida ({reference})");
            }
            (None, Some(b)) => {
                let file = b.file.unwrap_or_else(|| super::build::default_build_file(&b.context));
                let img = super::build::build_from_spec(&b.context, &file, &b.tag)?;
                println!("image/{name}: construída ({})", img.short_id());
            }
            (Some(_), Some(_)) => {
                return Err(Error::Invalid(format!("image/{name}: spec tem `pull` E `build` — só um dos dois")))
            }
            (None, None) => return Err(Error::Invalid(format!("image/{name}: spec sem `pull` nem `build`"))),
        }
    }
    Ok(())
}

fn cmd_pull(images: &ImageStore, reference: &str) -> Result<()> {
    let img = delonix_image::pull_from_registry(images, reference)?;
    println!("{}", img.short_id());
    Ok(())
}

fn cmd_ls(images: &ImageStore) -> Result<()> {
    println!("{:<24}  {:<16}  CRIADA(unix)", "REPOSITORY:TAG", "IMAGE ID");
    for img in images.list()? {
        let tag = img.repo_tags.first().cloned().unwrap_or_else(|| "<none>".into());
        println!("{:<24}  {:<16}  {}", tag, img.short_id(), img.created_unix);
    }
    Ok(())
}

fn cmd_rm(images: &ImageStore, reference: &str) -> Result<()> {
    let removed = images.remove(reference)?;
    println!("{removed}");
    Ok(())
}

/// Escreve um bundle OCI runtime mínimo (rootfs + config.json) para `runc`/`crun`.
fn cmd_export(images: &ImageStore, reference: &str, dir: &std::path::Path) -> Result<()> {
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
