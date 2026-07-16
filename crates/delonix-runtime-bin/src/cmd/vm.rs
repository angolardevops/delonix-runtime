//! `delonix vm` — microVMs declarativas (create/ls/stop/rm/status).

use std::path::PathBuf;
use std::process::Command;

use clap::Subcommand;
use delonix_runtime_core::{Error, Result};
use delonix_vm::VmConfig;
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::util::state_root;

/// `spec` de `kind: Vm` — espelha `delonix_vm::VmConfig` (menos `name`, que
/// vem de `metadata.name`).
#[derive(Debug, Deserialize)]
struct VmSpec {
    disk: String,
    #[serde(default = "default_vcpus")]
    vcpus: u32,
    #[serde(default = "default_memory")]
    memory: String,
    #[serde(default = "default_network")]
    network: String,
    kernel: Option<String>,
    initrd: Option<String>,
    firmware: Option<String>,
    cmdline: Option<String>,
    seed: Option<String>,
    restart_policy: Option<String>,
    #[serde(default)]
    hugepages: bool,
    cpu_affinity: Option<String>,
    #[serde(default)]
    devices: Vec<String>,
    backend: Option<String>,
    net_mode: Option<String>,
    bridge: Option<String>,
}

fn default_vcpus() -> u32 {
    1
}
fn default_memory() -> String {
    "1G".to_string()
}
fn default_network() -> String {
    "bridge".to_string()
}

// `Create` é maior que as outras variantes (muitos flags opcionais de VM) — é um
// enum de CLI parseado UMA vez por invocação, não um hot-path; boxar cada campo
// só para agradar ao lint complicaria a derive do `clap` sem benefício real.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum VmCmd {
    /// Cria (ou auto-recupera) uma VM.
    Create {
        name: String,
        /// Disco base (qcow2/raw) — vira overlay por-VM.
        #[arg(long)]
        disk: String,
        #[arg(long, default_value_t = 1)]
        vcpus: u32,
        /// Memória (`"2G"`/`"1024M"`).
        #[arg(long, default_value = "1G")]
        memory: String,
        /// Rede do ingress para o tap (`delonix network create` primeiro).
        #[arg(long, default_value = "bridge")]
        network: String,
        /// Kernel para direct boot.
        #[arg(long)]
        kernel: Option<String>,
        #[arg(long)]
        initrd: Option<String>,
        /// Firmware, alternativa ao kernel (cloud images).
        #[arg(long)]
        firmware: Option<String>,
        #[arg(long)]
        cmdline: Option<String>,
        /// ISO cloud-init (NoCloud) já pronta — se dado, tem prioridade sobre
        /// `--hostname`/`--ssh-key`/`--user-data` (esses geram a ISO; este usa-a
        /// directamente).
        #[arg(long)]
        seed: Option<String>,
        /// Hostname a aplicar no primeiro boot (gera a ISO NoCloud se nenhum
        /// `--seed` explícito for dado).
        #[arg(long)]
        hostname: Option<String>,
        /// Chave SSH pública autorizada, `ssh-ed25519 AAAA...` ou `@caminho`
        /// para ler de um ficheiro. Repetível.
        #[arg(long = "ssh-key")]
        ssh_keys: Vec<String>,
        /// `user-data` cloud-init próprio (substitui totalmente o gerado por
        /// omissão) — controlo completo para quem precisar.
        #[arg(long)]
        user_data: Option<PathBuf>,
        /// `no`|`on-failure`|`always`.
        #[arg(long)]
        restart_policy: Option<String>,
        #[arg(long)]
        hugepages: bool,
        /// Afinidade de cores, ex.: `8-15`.
        #[arg(long)]
        cpu_affinity: Option<String>,
        /// VFIO PCI passthrough, repetível.
        #[arg(long = "device")]
        devices: Vec<String>,
        /// `cloud-hypervisor`|`libvirt` (omitir = auto-deteção).
        #[arg(long)]
        backend: Option<String>,
        /// Só libvirt: `user`|`nat`|`bridge`.
        #[arg(long)]
        net_mode: Option<String>,
        /// Nome da bridge (net-mode=bridge) ou rede libvirt (nat).
        #[arg(long)]
        bridge: Option<String>,
    },
    /// Lista as VMs.
    Ls,
    /// Estado actual (reconcilia liveness/IP com o backend).
    Status { name: String },
    /// Pára a VM (preserva disco/registo).
    Stop { name: String },
    /// Remove a VM (pára + apaga overlay/estado).
    Rm { name: String },
    /// Aplica os documentos `kind: Vm` de um manifesto (`delonix_vm::create` já
    /// é idempotente por nome — cria ou auto-recupera).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let base = state_root();
    for doc in manifest::of_kind(docs, "Vm") {
        let name = &doc.metadata.name;
        let spec: VmSpec = manifest::spec_of(doc)?;
        let cfg = VmConfig {
            name: name.clone(),
            disk: spec.disk,
            vcpus: spec.vcpus,
            memory: spec.memory,
            network: spec.network,
            kernel: spec.kernel,
            initrd: spec.initrd,
            firmware: spec.firmware,
            cmdline: spec.cmdline,
            seed: spec.seed,
            restart_policy: spec.restart_policy,
            hugepages: spec.hugepages,
            cpu_affinity: spec.cpu_affinity,
            devices: spec.devices,
            backend: spec.backend,
            net_mode: spec.net_mode,
            bridge: spec.bridge,
        };
        delonix_vm::create(&base, &cfg)?;
        println!("vm/{name}: garantida");
    }
    Ok(())
}

pub fn run(action: VmCmd) -> Result<()> {
    let base = state_root();
    match action {
        VmCmd::Create {
            name,
            disk,
            vcpus,
            memory,
            network,
            kernel,
            initrd,
            firmware,
            cmdline,
            seed,
            hostname,
            ssh_keys,
            user_data,
            restart_policy,
            hugepages,
            cpu_affinity,
            devices,
            backend,
            net_mode,
            bridge,
        } => {
            let seed = match seed {
                Some(s) => Some(s),
                None if hostname.is_some() || !ssh_keys.is_empty() || user_data.is_some() => {
                    let iso = generate_seed_iso(&name, hostname.as_deref(), &ssh_keys, user_data.as_deref())?;
                    Some(iso.to_string_lossy().into_owned())
                }
                None => None,
            };
            let cfg = VmConfig {
                name,
                disk,
                vcpus,
                memory,
                network,
                kernel,
                initrd,
                firmware,
                cmdline,
                seed,
                restart_policy,
                hugepages,
                cpu_affinity,
                devices,
                backend,
                net_mode,
                bridge,
            };
            let vm = delonix_vm::create(&base, &cfg)?;
            println!("{}", vm.name);
            Ok(())
        }
        VmCmd::Ls => {
            println!("{:<20}  {:<8}  {:<10}  {:<10}  IP", "NOME", "VCPUS", "MEMORY", "STATUS");
            for vm in delonix_vm::list(&base)? {
                println!(
                    "{:<20}  {:<8}  {:<10}  {:<10}  {}",
                    vm.name,
                    vm.vcpus,
                    vm.memory,
                    format!("{:?}", vm.status),
                    vm.ip.unwrap_or_default()
                );
            }
            Ok(())
        }
        VmCmd::Status { name } => {
            let vm = delonix_vm::status(&base, &name)?;
            println!("nome:     {}", vm.name);
            println!("status:   {:?}", vm.status);
            println!("backend:  {}", vm.backend);
            println!("ip:       {}", vm.ip.unwrap_or_default());
            Ok(())
        }
        VmCmd::Stop { name } => {
            delonix_vm::stop(&base, &name)?;
            println!("{name}");
            Ok(())
        }
        VmCmd::Rm { name } => {
            delonix_vm::remove(&base, &name)?;
            println!("{name}");
            Ok(())
        }
        VmCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
    }
}

// ---------------------------------------------------------------------------
// Geração da ISO cloud-init NoCloud por-instância (não confundir com o build
// da imagem dourada, em `cmd::vmimage` — isto corre uma vez por VM, no
// arranque; aquele corre uma vez por imagem, no build).
// ---------------------------------------------------------------------------

/// Resolve uma entrada de `--ssh-key`: literal, ou `@caminho` para ler de um ficheiro.
fn resolve_ssh_key(spec: &str) -> Result<String> {
    match spec.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .map_err(|e| Error::Invalid(format!("não consegui ler a chave SSH de '{path}': {e}"))),
        None => Ok(spec.trim().to_string()),
    }
}

/// `user-data` NoCloud mínimo — pura, testável sem `cloud-localds` real.
/// `package_update: false`/`package_upgrade: false` porque a imagem dourada
/// já vem pronta (ver `cmd::vmimage`); não faz sentido gastar o primeiro boot
/// a `apt update`.
fn build_user_data(hostname: &str, ssh_keys: &[String]) -> String {
    let mut out = String::from("#cloud-config\n");
    out.push_str(&format!("hostname: {hostname}\n"));
    out.push_str("package_update: false\n");
    out.push_str("package_upgrade: false\n");
    if !ssh_keys.is_empty() {
        out.push_str("ssh_authorized_keys:\n");
        for k in ssh_keys {
            out.push_str(&format!("  - {k}\n"));
        }
    }
    out
}

fn build_meta_data(instance_id: &str, hostname: &str) -> String {
    format!("instance-id: {instance_id}\nlocal-hostname: {hostname}\n")
}

/// Gera (ou reaproveita, via `user_data_override`) o `user-data`/`meta-data` e
/// empacota-os num ISO NoCloud com `cloud-localds`. Devolve o caminho da ISO.
fn generate_seed_iso(
    vm_name: &str,
    hostname: Option<&str>,
    ssh_keys: &[String],
    user_data_override: Option<&std::path::Path>,
) -> Result<PathBuf> {
    let hostname = hostname.unwrap_or(vm_name).to_string();
    let work_dir = state_root().join("vms").join(vm_name);
    std::fs::create_dir_all(&work_dir)?;

    let user_data_path = work_dir.join("user-data");
    match user_data_override {
        Some(p) => {
            std::fs::copy(p, &user_data_path)
                .map_err(|e| Error::Invalid(format!("não consegui copiar --user-data '{}': {e}", p.display())))?;
        }
        None => {
            let resolved_keys: Result<Vec<String>> = ssh_keys.iter().map(|s| resolve_ssh_key(s)).collect();
            let content = build_user_data(&hostname, &resolved_keys?);
            std::fs::write(&user_data_path, content)?;
        }
    }
    let meta_data_path = work_dir.join("meta-data");
    std::fs::write(&meta_data_path, build_meta_data(vm_name, &hostname))?;

    let iso_path = work_dir.join("seed.iso");
    let status = Command::new("cloud-localds")
        .arg(&iso_path)
        .arg(&user_data_path)
        .arg(&meta_data_path)
        .status()
        .map_err(|e| Error::Invalid(format!("a correr cloud-localds: {e}")))?;
    if !status.success() {
        return Err(Error::Invalid(format!("cloud-localds falhou (exit {:?})", status.code())));
    }
    Ok(iso_path)
}

#[cfg(test)]
mod tests {
    use super::{build_meta_data, build_user_data};

    #[test]
    fn user_data_inclui_hostname_e_chaves() {
        let ud = build_user_data("myvm", &["ssh-ed25519 AAAA foo".to_string()]);
        assert!(ud.starts_with("#cloud-config\n"));
        assert!(ud.contains("hostname: myvm\n"));
        assert!(ud.contains("ssh_authorized_keys:\n  - ssh-ed25519 AAAA foo\n"));
        assert!(ud.contains("package_update: false\n"));
    }

    #[test]
    fn user_data_sem_chaves_nao_tem_seccao_ssh() {
        let ud = build_user_data("myvm", &[]);
        assert!(!ud.contains("ssh_authorized_keys"));
    }

    #[test]
    fn meta_data_tem_instance_id_e_hostname() {
        let md = build_meta_data("vm-1", "myvm");
        assert_eq!(md, "instance-id: vm-1\nlocal-hostname: myvm\n");
    }
}
