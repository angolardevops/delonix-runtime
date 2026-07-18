//! `delonix vm` — microVMs declarativas (create/ls/stop/rm/status).

use std::path::PathBuf;
use std::process::Command;

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_runtime_core::{Error, Result};
use delonix_vm::VmConfig;
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::output;
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
    /// Canónico `restartPolicy` (uniforme com `Container`); `restart_policy`
    /// mantém-se aceite para não partir manifestos anteriores.
    #[serde(rename = "restartPolicy", alias = "restart_policy")]
    restart_policy: Option<String>,
    #[serde(default)]
    hugepages: bool,
    /// Canónico `cpuAffinity`; `cpu_affinity` continua aceite (retrocompat).
    #[serde(rename = "cpuAffinity", alias = "cpu_affinity")]
    cpu_affinity: Option<String>,
    #[serde(default)]
    devices: Vec<String>,
    backend: Option<String>,
    /// Canónico `netMode`; `net_mode` continua aceite (retrocompat).
    #[serde(rename = "netMode", alias = "net_mode")]
    net_mode: Option<String>,
    bridge: Option<String>,
}

/// Nomes de campo aceites no `spec` de `kind: Vm` (canónicos + aliases legados),
/// para o aviso de campos desconhecidos. Mantém-se alinhado com `VmSpec` pelo
/// teste `vm_spec_conhece_todos_os_campos_do_exemplo`.
pub(crate) const VM_SPEC_FIELDS: &[&str] = &[
    "disk", "vcpus", "memory", "network", "kernel", "initrd", "firmware", "cmdline", "seed",
    "restartPolicy", "restart_policy", "hugepages", "cpuAffinity", "cpu_affinity", "devices",
    "backend", "netMode", "net_mode", "bridge",
];

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
    /// Inicializa um projecto com manifesto de VM — ficheiros JÁ PREENCHIDOS (imagens
    /// incluídas), prontos a usar sem editar nada.
    Init {
        /// Directório do projecto (default: o actual).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Nome do projecto (default: o nome do directório).
        #[arg(long)]
        name: Option<String>,
        /// Imagem a usar. Omitir = preenche com a imagem por omissão.
        #[arg(long)]
        image: Option<String>,
        /// Substitui ficheiros já existentes.
        #[arg(long)]
        force: bool,
    },
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
    Status {
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: String,
    },
    /// Detalhe legível de uma ou mais VMs, ao estilo `kubectl describe` (para
    /// humanos; use `status` para a vista compacta de sempre). Inclui o estado
    /// AO VIVO — `delonix_vm::status` reconcilia liveness/IP com o backend.
    Describe {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::vms))]
        names: Vec<String>,
    },
    /// Pára a VM (preserva disco/registo).
    Stop {
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: String,
    },
    /// Remove a VM (pára + apaga overlay/estado).
    Rm {
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: String,
    },
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
        manifest::warn_unknown_fields(doc, VM_SPEC_FIELDS);
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
    if let VmCmd::Init { dir, name, image, force } = action {
        return cmd_init(super::scaffold::Target::Vm, dir, name, image, force);
    }
    let base = state_root();
    match action {
        // Tratado no topo de `run` (faz `return`).
        VmCmd::Init { .. } => unreachable!("tratado acima"),
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
            let mut t = output::Table::new(&["NAME", "VCPUS", "MEMORY", "STATUS", "IP"])
                // VCPUS é uma contagem — alinhada à direita como os tamanhos.
                .right_align(1);
            for vm in delonix_vm::list(&base)? {
                t.row(vec![
                    vm.name,
                    vm.vcpus.to_string(),
                    vm.memory,
                    fmt_vm_status(&vm.status),
                    vm.ip.unwrap_or_else(|| "<none>".into()),
                ]);
            }
            t.print();
            Ok(())
        }
        VmCmd::Describe { names } => cmd_describe(&base, &names),
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

/// Estado de uma VM em texto, sem o `{:?}` cru do enum: `Failed(137)` do
/// `Debug` viraria "Failed(137)" — legível, mas o `Exited (137)` é o
/// vocabulário que o resto da CLI já usa (`container ps`). Pura.
fn fmt_vm_status(status: &delonix_runtime_core::Status) -> String {
    use delonix_runtime_core::Status as S;
    match status {
        S::Created => "Created".to_string(),
        S::Running => "Running".to_string(),
        S::Paused => "Paused".to_string(),
        S::Stopped => "Stopped".to_string(),
        S::Failed(code) => format!("Exited ({code})"),
        S::Crashed => "Dead".to_string(),
    }
}

/// `vm describe` — detalhe legível ao estilo `kubectl describe`.
///
/// Usa `delonix_vm::status` (não o registo cru): reconcilia liveness/IP com o
/// backend, portanto o que se lê é o estado AO VIVO e não o último que ficou
/// gravado. É a diferença entre "diz que está Running" e "está Running".
fn cmd_describe(base: &std::path::Path, names: &[String]) -> Result<()> {
    for (i, name) in names.iter().enumerate() {
        let vm = delonix_vm::status(base, name)?;
        if i > 0 {
            println!();
        }
        describe_one(&vm);
    }
    Ok(())
}

/// Tamanho de um ficheiro em disco, se legível. Um overlay/disco que
/// desapareceu (apagado à mão) dá `None` e o campo omite o tamanho — melhor
/// que imprimir `0 B`, que se leria como "vazio" em vez de "não existe".
fn file_size(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

fn describe_one(vm: &delonix_runtime_core::Vm) {
    let mut d = output::Describe::new();
    d.field("Name", &vm.name);
    d.field("Status", fmt_vm_status(&vm.status));
    d.field("Backend", &vm.backend);
    d.field("Created", output::fmt_local(vm.created_unix));
    d.field("Age", output::fmt_age(vm.created_unix));
    d.field("PID", vm.pid.map(|p| p.to_string()).unwrap_or_else(|| "<none>".into()));
    d.field("Restart policy", vm.restart_policy.as_deref().unwrap_or("no"));

    d.section("Resources");
    d.sub("vCPUs", vm.vcpus.to_string());
    d.sub("Memory", &vm.memory);

    d.section("Disk");
    d.sub("Base", &vm.disk);
    d.sub("Overlay", &vm.overlay);
    // Tamanho REAL do overlay em disco (o que a VM escreveu por cima da base).
    d.sub_opt("Overlay size", file_size(&vm.overlay).map(output::fmt_size));

    d.section("Network");
    d.sub("Network", &vm.network);
    d.sub("IP", vm.ip.as_deref().unwrap_or("<none>"));
    d.sub("TAP", if vm.tap.is_empty() { "<none>" } else { &vm.tap });
    d.sub("MAC", &vm.mac);

    d.field("API socket", &vm.api_socket);
    d.print();
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
/// `pub(crate)`: reaproveitada por `cmd::cluster::provision_and_apply` (cada
/// VM provisionada por `delonix cluster kubeadm` precisa do mesmo seed).
pub(crate) fn generate_seed_iso(
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
    use super::{build_meta_data, build_user_data, fmt_vm_status, VmSpec};
    use delonix_runtime_core::Status;

    #[test]
    fn vmspec_aceita_snake_case_legado_e_camel_case_canonico() {
        // Legado (snake_case) — não pode partir.
        let legado: VmSpec = serde_yaml::from_str(
            "disk: d\nrestart_policy: always\ncpu_affinity: 0-3\nnet_mode: nat\n",
        )
        .unwrap();
        assert_eq!(legado.restart_policy.as_deref(), Some("always"));
        assert_eq!(legado.cpu_affinity.as_deref(), Some("0-3"));
        assert_eq!(legado.net_mode.as_deref(), Some("nat"));
        // Canónico (camelCase) — a forma nova dos exemplos.
        let canon: VmSpec = serde_yaml::from_str(
            "disk: d\nrestartPolicy: always\ncpuAffinity: 0-3\nnetMode: nat\n",
        )
        .unwrap();
        assert_eq!(canon.restart_policy.as_deref(), Some("always"));
        assert_eq!(canon.cpu_affinity.as_deref(), Some("0-3"));
        assert_eq!(canon.net_mode.as_deref(), Some("nat"));
    }

    #[test]
    fn status_de_vm_usa_o_vocabulario_da_cli() {
        assert_eq!(fmt_vm_status(&Status::Running), "Running");
        assert_eq!(fmt_vm_status(&Status::Stopped), "Stopped");
        // `{:?}` daria "Failed(137)"; o resto da CLI diz "Exited (137)".
        assert_eq!(fmt_vm_status(&Status::Failed(137)), "Exited (137)");
        assert_eq!(fmt_vm_status(&Status::Crashed), "Dead");
    }

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

/// Trata o `init` deste grupo (ver `cmd::scaffold`).
fn cmd_init(target: super::scaffold::Target, dir: PathBuf, name: Option<String>, image: Option<String>, force: bool) -> Result<()> {
    let name = name.unwrap_or_else(|| {
        // Sem `--name`, usa o nome do DIRECTÓRIO. Não se pode usar `canonicalize`:
        // o directório ainda não existe (é o `init` que o cria) e falharia sempre,
        // caindo no fallback — todos os projectos ficavam chamados "app".
        // `.`/vazio resolvem para o cwd; um caminho novo usa o seu basename.
        let p = if dir.as_os_str().is_empty() || dir == std::path::Path::new(".") {
            std::env::current_dir().ok()
        } else {
            Some(dir.clone())
        };
        p.as_deref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".to_string())
    });
    super::scaffold::init(target, &super::scaffold::InitOpts { dir, name, image, force })
}
