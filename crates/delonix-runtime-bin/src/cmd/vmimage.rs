//! `delonix image --vm` — imagens VM douradas (Ubuntu + kubeadm/kubelet/
//! kubectl + `delonix-cri`), geridas à parte das imagens de container (essas
//! ficam em `cmd::image`/`ImageStore`). Um `.qcow2` solto por imagem (sem
//! CAS/layers — só há um blob por imagem, nada a deduplicar) + um `.json` de
//! metadados, ambos em `<root>/vm-images/`.
//!
//! `build` produz a imagem de raiz (download da cloud image Ubuntu + `virt-
//! customize`); `push`/`pull` publicam/obtêm-na de um registo OCI (artefacto
//! de blob único, ver `delonix_image::registry::{push_oci_artifact,
//! pull_oci_artifact}`) — o mesmo protocolo das imagens de container, só sem
//! o modelo de layers/config Docker.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Subcommand;
use delonix_runtime_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::util::state_root;

const VM_IMAGE_MEDIA_TYPE: &str = "application/vnd.delonix.vmimage.v1.qcow2";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmImage {
    pub name: String,
    pub tag: String,
    pub digest: String,
    pub size: u64,
    pub ubuntu_release: Option<String>,
    pub k8s_version: Option<String>,
    pub created_unix: u64,
}

pub struct VmImageStore {
    root: PathBuf,
}

impl VmImageStore {
    pub fn open(base: impl Into<PathBuf>) -> Result<Self> {
        let root = base.into().join("vm-images");
        std::fs::create_dir_all(root.join("_base"))?;
        Ok(Self { root })
    }

    fn sanitize(name: &str) -> String {
        name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect()
    }

    fn meta_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{}.json", Self::sanitize(name)))
    }

    pub fn qcow2_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{}.qcow2", Self::sanitize(name)))
    }

    pub fn base_cache_path(&self, ubuntu_release: &str) -> PathBuf {
        // `sanitize` (não aplicado aqui antes — achado de auditoria de segurança,
        // ver CLAUDE.md) elimina `/` de `ubuntu_release`, impedindo que
        // `--ubuntu-release '../../../etc/cron.d/x'` escreva fora de `_base/`.
        self.root.join("_base").join(format!("ubuntu-{}-server-cloudimg-amd64.img", Self::sanitize(ubuntu_release)))
    }

    pub fn save(&self, img: &VmImage) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(img)?;
        std::fs::write(self.meta_path(&img.name), bytes)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<VmImage>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root)?.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(bytes) = std::fs::read(&p) {
                    if let Ok(img) = serde_json::from_slice::<VmImage>(&bytes) {
                        out.push(img);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn get(&self, name: &str) -> Result<VmImage> {
        let bytes = std::fs::read(self.meta_path(name))
            .map_err(|_| Error::NotFound(format!("imagem VM '{name}'")))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[derive(Subcommand)]
pub enum VmImageCmd {
    /// Lista as imagens VM locais.
    Ls,
    /// Publica uma imagem VM local num registo OCI (artefacto de blob único).
    Push { name: String, target: String },
    /// Puxa uma imagem VM de um registo OCI.
    Pull { source: String, #[arg(long)] name: Option<String> },
    /// Constrói a imagem dourada: Ubuntu cloud image + kubeadm/kubelet/kubectl
    /// + `delonix-cri` (endpoint CRI para o kubelet), via `virt-customize`.
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        #[arg(long, default_value = "26.04")]
        ubuntu_release: String,
        /// Versão do Kubernetes (ex.: `1.31`) — omitir usa a última estável.
        #[arg(long)]
        k8s_version: Option<String>,
        /// Pacote apt adicional, repetível — extensibilidade sem tocar no código.
        #[arg(long = "extra-package")]
        extra_packages: Vec<String>,
        /// Comando adicional a correr dentro do guest durante o build, repetível.
        #[arg(long = "extra-run")]
        extra_run: Vec<String>,
        /// Caminho explícito do binário `delonix-cri` a instalar (senão:
        /// procura ao lado do `delonix` actual, depois tenta compilar do
        /// workspace se um `Cargo.toml` for detectado a partir do cwd).
        #[arg(long)]
        cri_bin: Option<PathBuf>,
    },
}

pub fn run(action: VmImageCmd) -> Result<()> {
    let store = VmImageStore::open(state_root())?;
    match action {
        VmImageCmd::Ls => cmd_ls(&store),
        VmImageCmd::Push { name, target } => cmd_push(&store, &name, &target),
        VmImageCmd::Pull { source, name } => cmd_pull(&store, &source, name),
        VmImageCmd::Build { tag, ubuntu_release, k8s_version, extra_packages, extra_run, cri_bin } => {
            cmd_build(&store, &tag, &ubuntu_release, k8s_version, extra_packages, extra_run, cri_bin)
        }
    }
}

fn cmd_ls(store: &VmImageStore) -> Result<()> {
    println!("{:<28}  {:<10}  {:<10}  {:<10}  TAMANHO", "NOME", "UBUNTU", "K8S", "CRIADA(unix)");
    for img in store.list()? {
        println!(
            "{:<28}  {:<10}  {:<10}  {:<10}  {}",
            img.name,
            img.ubuntu_release.as_deref().unwrap_or("-"),
            img.k8s_version.as_deref().unwrap_or("-"),
            img.created_unix,
            img.size
        );
    }
    Ok(())
}

fn cmd_push(store: &VmImageStore, name: &str, target: &str) -> Result<()> {
    let img = store.get(name)?;
    let data = std::fs::read(store.qcow2_path(name))
        .map_err(|e| Error::Invalid(format!("não consegui ler o qcow2 de '{name}': {e}")))?;
    let digest = delonix_image::registry::push_oci_artifact(&state_root(), target, VM_IMAGE_MEDIA_TYPE, &data)?;
    println!("{digest}");
    let _ = img;
    Ok(())
}

fn cmd_pull(store: &VmImageStore, source: &str, name: Option<String>) -> Result<()> {
    let data = delonix_image::registry::pull_oci_artifact(&state_root(), source)?;
    let name = name.unwrap_or_else(|| source.rsplit('/').next().unwrap_or(source).to_string());
    let digest = format!("sha256:{}", hex_sha256(&data));
    std::fs::write(store.qcow2_path(&name), &data)?;
    let img = VmImage {
        name: name.clone(),
        tag: source.to_string(),
        digest,
        size: data.len() as u64,
        ubuntu_release: None,
        k8s_version: None,
        created_unix: now_unix(),
    };
    store.save(&img)?;
    println!("{name}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_build(
    store: &VmImageStore,
    tag: &str,
    ubuntu_release: &str,
    k8s_version: Option<String>,
    extra_packages: Vec<String>,
    extra_run: Vec<String>,
    cri_bin: Option<PathBuf>,
) -> Result<()> {
    // `k8s_version` entra num `format!` que vira comando `virt-customize --run-command`
    // (via `k8s_recipes::k8s_host_recipes`) — validar aqui fecha o mesmo achado de
    // segurança de `cmd::cluster::valid_version` (o repositório apt embutido não pode
    // conter metacaracteres de shell). Achado de auditoria, ver CLAUDE.md.
    if let Some(v) = &k8s_version {
        if !super::cluster::valid_version(v) {
            return Err(Error::Invalid(format!("--k8s-version '{v}' inválido (só dígitos e pontos, ex.: '1.31')")));
        }
    }
    let base = download_ubuntu_base(store, ubuntu_release)?;
    let cri = resolve_cri_bin(cri_bin)?;

    let work_dir = std::env::temp_dir().join(format!("delonix-vmimage-build-{}", std::process::id()));
    std::fs::create_dir_all(&work_dir)?;
    let work_qcow2 = work_dir.join("work.qcow2");

    eprintln!("a preparar imagem de trabalho (achatada, sem backing file)...");
    run_tool("qemu-img", &["convert", "-O", "qcow2", &base.to_string_lossy(), &work_qcow2.to_string_lossy()])?;

    let service_unit = workspace_dist_file("delonix-cri.service")?;
    let ops = k8s_customization_steps(k8s_version.as_deref(), &extra_packages, &extra_run, &cri, &service_unit);
    let args = customize_args(&work_qcow2, &ops);

    eprintln!("a correr virt-customize ({} passos)...", ops.len());
    run_tool("virt-customize", &args.iter().map(String::as_str).collect::<Vec<_>>())?;

    let data = std::fs::read(&work_qcow2)?;
    let digest = format!("sha256:{}", hex_sha256(&data));
    let size = data.len() as u64;
    std::fs::rename(&work_qcow2, store.qcow2_path(tag))
        .or_else(|_| std::fs::copy(&work_qcow2, store.qcow2_path(tag)).map(|_| ()))?;
    let _ = std::fs::remove_dir_all(&work_dir);

    let img = VmImage {
        name: tag.to_string(),
        tag: tag.to_string(),
        digest,
        size,
        ubuntu_release: Some(ubuntu_release.to_string()),
        k8s_version,
        created_unix: now_unix(),
    };
    store.save(&img)?;
    println!("{tag}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Download + verificação da cloud image Ubuntu
// ---------------------------------------------------------------------------

fn download_ubuntu_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
    let cached = store.base_cache_path(release);
    if cached.exists() {
        return Ok(cached);
    }
    let base_url = format!("https://cloud-images.ubuntu.com/releases/{release}/release");
    let img_name = format!("ubuntu-{release}-server-cloudimg-amd64.img");
    let img_url = format!("{base_url}/{img_name}");
    let sums_url = format!("{base_url}/SHA256SUMS");

    eprintln!("a descarregar {img_url}...");
    let tmp = cached.with_extension("download");
    stream_download(&img_url, &tmp)?;

    eprintln!("a verificar SHA256SUMS...");
    let sums = http_get_text(&sums_url)?;
    let expected = sums
        .lines()
        .find(|l| l.trim_end().ends_with(&img_name))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| Error::Invalid(format!("SHA256SUMS não tem entrada para {img_name}")))?
        .to_string();
    let got = hex_sha256_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(format!(
            "checksum inválido para {img_name}: esperado {expected}, obtido {got} — download descartado"
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    Ok(cached)
}

fn stream_download(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(std::time::Duration::from_secs(3600))
        .build()
        .map_err(|e| Error::Invalid(format!("cliente HTTP: {e}")))?;
    let mut resp = client.get(url).send().map_err(|e| Error::Invalid(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Invalid(format!("GET {url}: HTTP {}", resp.status())));
    }
    let mut file = std::fs::File::create(dest)?;
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = resp.read(&mut buf).map_err(|e| Error::Invalid(format!("a ler resposta: {e}")))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
    }
    Ok(())
}

fn http_get_text(url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| Error::Invalid(format!("cliente HTTP: {e}")))?;
    let resp = client.get(url).send().map_err(|e| Error::Invalid(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Invalid(format!("GET {url}: HTTP {}", resp.status())));
    }
    resp.text().map_err(|e| Error::Invalid(format!("corpo de {url}: {e}")))
}

fn hex_sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finalize())
}

fn hex_sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Resolução do binário `delonix-cri` a instalar no guest
// ---------------------------------------------------------------------------

pub(crate) fn resolve_cri_bin(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.exists() {
            return Err(Error::Invalid(format!("--cri-bin '{}' não existe", p.display())));
        }
        return Ok(p);
    }
    // Ao lado do `delonix` actual (instalação normal, release).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("delonix-cri");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    // Conveniência de dev: workspace do código-fonte a partir do cwd.
    if let Some(workspace_root) = find_workspace_root() {
        eprintln!("a compilar delonix-cri (release) a partir de {}...", workspace_root.display());
        let status = Command::new("cargo")
            .args(["build", "--release", "-p", "delonix-cri", "--bin", "delonix-cri"])
            .current_dir(&workspace_root)
            .status()
            .map_err(|e| Error::Invalid(format!("a correr cargo build: {e}")))?;
        if !status.success() {
            return Err(Error::Invalid("cargo build do delonix-cri falhou".into()));
        }
        let built = workspace_root.join("target/release/delonix-cri");
        if built.exists() {
            return Ok(built);
        }
    }
    Err(Error::Invalid(
        "não encontrei o binário delonix-cri: usa --cri-bin <caminho>, instala-o ao lado do \
         delonix, ou corre a partir do checkout do código-fonte"
            .into(),
    ))
}

fn find_workspace_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("Cargo.toml").exists() && dir.join("crates/delonix-cri").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub(crate) fn workspace_dist_file(name: &str) -> Result<PathBuf> {
    if let Some(root) = find_workspace_root() {
        let p = root.join("dist").join(name);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(Error::Invalid(format!(
        "não encontrei dist/{name} — corre a partir do checkout do código-fonte ou fornece via --extra-run"
    )))
}

// ---------------------------------------------------------------------------
// Passos de customização (função pura — testável sem VM/virt-customize real)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CustomizeOp {
    RunCommand(String),
    CopyIn(PathBuf, String),
    Password { user: String, password: String },
    RootPassword(String),
}

/// Constrói a lista de passos de customização a aplicar à imagem base — a
/// parte "100% parametrizada": `extra_packages`/`extra_run` estendem sem
/// tocar nesta função. Pura (sem I/O), testável isoladamente. As receitas
/// tecnicamente sensíveis (repo/pacotes/swap/módulos/sysctls) vêm de
/// `k8s_recipes::k8s_host_recipes` — o MESMO catálogo que `cmd::cluster`
/// usa via SSH, para a imagem dourada e um host preparado por `cluster
/// apply` ficarem exactamente iguais.
pub(crate) fn k8s_customization_steps(
    k8s_version: Option<&str>,
    extra_packages: &[String],
    extra_run: &[String],
    cri_bin: &Path,
    cri_service: &Path,
) -> Vec<CustomizeOp> {
    let mut ops: Vec<CustomizeOp> = super::k8s_recipes::k8s_host_recipes(k8s_version, extra_packages)
        .into_iter()
        .map(|r| CustomizeOp::RunCommand(r.apply_offline().to_string()))
        .collect();
    ops.extend([
        // `delonix-cri` — endpoint CRI para o kubelet (substitui containerd).
        CustomizeOp::CopyIn(cri_bin.to_path_buf(), "/usr/local/bin".to_string()),
        CustomizeOp::RunCommand("chmod +x /usr/local/bin/delonix-cri".into()),
        CustomizeOp::CopyIn(cri_service.to_path_buf(), "/etc/systemd/system".to_string()),
        CustomizeOp::RunCommand("systemctl enable delonix-cri.service".into()),
        // Conta padrão: root/delonix e delonix:delonix em sudoers (pedido explícito).
        CustomizeOp::RootPassword("delonix".to_string()),
        CustomizeOp::RunCommand("useradd -m -s /bin/bash -G sudo delonix || true".into()),
        CustomizeOp::Password { user: "delonix".to_string(), password: "delonix".to_string() },
        CustomizeOp::RunCommand(
            "echo 'delonix ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/90-delonix && chmod 440 /etc/sudoers.d/90-delonix"
                .into(),
        ),
    ]);
    ops.extend(extra_run.iter().cloned().map(CustomizeOp::RunCommand));
    ops
}

/// Traduz os `CustomizeOp` para os argumentos reais do `virt-customize`.
pub(crate) fn customize_args(disk: &Path, ops: &[CustomizeOp]) -> Vec<String> {
    let mut args = vec!["-a".to_string(), disk.to_string_lossy().into_owned()];
    for op in ops {
        match op {
            CustomizeOp::RunCommand(cmd) => {
                args.push("--run-command".into());
                args.push(cmd.clone());
            }
            CustomizeOp::CopyIn(src, dst) => {
                args.push("--copy-in".into());
                args.push(format!("{}:{}", src.display(), dst));
            }
            CustomizeOp::Password { user, password } => {
                args.push("--password".into());
                args.push(format!("{user}:password:{password}"));
            }
            CustomizeOp::RootPassword(password) => {
                args.push("--root-password".into());
                args.push(format!("password:{password}"));
            }
        }
    }
    args
}

fn run_tool(bin: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(bin)
        .args(args)
        .status()
        .map_err(|e| Error::Invalid(format!("a correr {bin}: {e}")))?;
    if !status.success() {
        return Err(Error::Invalid(format!("{bin} falhou (exit {:?})", status.code())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn customization_steps_incluem_pacotes_extra() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(None, &["htop".to_string()], &[], &cri, &svc);
        let install_step = ops
            .iter()
            .find_map(|op| match op {
                CustomizeOp::RunCommand(c) if c.contains("apt-get install") => Some(c),
                _ => None,
            })
            .expect("devia haver um RunCommand de apt-get install");
        assert!(install_step.contains("kubeadm"));
        assert!(install_step.contains("htop"));
    }

    #[test]
    fn customization_steps_incluem_extra_run_no_fim() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(None, &[], &["echo oi".to_string()], &cri, &svc);
        assert!(matches!(ops.last(), Some(CustomizeOp::RunCommand(c)) if c == "echo oi"));
    }

    #[test]
    fn customization_steps_configuram_delonix_user_e_root_password() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(None, &[], &[], &cri, &svc);
        assert!(ops.iter().any(|op| matches!(op, CustomizeOp::RootPassword(p) if p == "delonix")));
        assert!(ops.iter().any(|op| matches!(op, CustomizeOp::Password{user,password} if user=="delonix" && password=="delonix")));
    }

    #[test]
    fn customize_args_traduz_run_command_e_copy_in_correctamente() {
        let ops = vec![
            CustomizeOp::RunCommand("apt-get install -y a b".to_string()),
            CustomizeOp::CopyIn(PathBuf::from("/host/bin"), "/usr/local/bin".to_string()),
            CustomizeOp::RootPassword("x".to_string()),
        ];
        let args = customize_args(Path::new("/tmp/disk.qcow2"), &ops);
        assert_eq!(args[0], "-a");
        assert_eq!(args[1], "/tmp/disk.qcow2");
        assert!(args.windows(2).any(|w| w == ["--run-command".to_string(), "apt-get install -y a b".to_string()]));
        assert!(args.windows(2).any(|w| w == ["--copy-in".to_string(), "/host/bin:/usr/local/bin".to_string()]));
        assert!(args.windows(2).any(|w| w == ["--root-password".to_string(), "password:x".to_string()]));
    }

    #[test]
    fn hex_sha256_e_consistente() {
        assert_eq!(hex_sha256(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
