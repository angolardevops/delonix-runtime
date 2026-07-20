//! `delonix vm` — microVMs declarativas (create/ls/stop/rm/status).

use std::path::PathBuf;
use std::process::Command;

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_runtime_core::{Error, Result};
use delonix_vm::VmConfig;
use delonix_volume::VolumeStore;
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
    /// Volumes/Storage a montar dentro da VM (virtio-9p) — fecha o gap de dar
    /// storage a uma VM sem escrever cloud-init/XML. Ver `VmVolumeSpec`.
    #[serde(default)]
    volumes: Vec<VmVolumeSpec>,
}

/// Uma entrada de `spec.volumes` de uma VM: refere um `Volume`/`Storage` por
/// nome e diz onde montá-lo no guest.
#[derive(Debug, Deserialize)]
struct VmVolumeSpec {
    /// Nome de um `kind: Volume` ou `kind: Storage` (resolvido no apply).
    name: String,
    /// Ponto de montagem no guest (ex.: `/mnt/dados`).
    #[serde(rename = "mountPath")]
    mount_path: String,
    /// Montar só-de-leitura.
    #[serde(default, rename = "readOnly")]
    read_only: bool,
}

/// Nomes de campo aceites no `spec` de `kind: Vm` (canónicos + aliases legados),
/// para o aviso de campos desconhecidos. Mantém-se alinhado com `VmSpec` pelo
/// teste `manifest::tests::examples_nao_tem_campos_desconhecidos`.
pub(crate) const VM_SPEC_FIELDS: &[&str] = &[
    "disk",
    "vcpus",
    "memory",
    "network",
    "kernel",
    "initrd",
    "firmware",
    "cmdline",
    "seed",
    "restartPolicy",
    "restart_policy",
    "hugepages",
    "cpuAffinity",
    "cpu_affinity",
    "devices",
    "backend",
    "netMode",
    "net_mode",
    "bridge",
    "volumes",
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
    /// Dashboard (KPIs + tabela) das VMs — TUI interactivo, ou `--once` snapshot.
    Dash {
        #[arg(long)]
        once: bool,
    },
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
        /// Gera um PROJECTO completo de uma stack (ex.: `python`) com boas práticas,
        /// em vez do scaffold genérico. `--template list` mostra os disponíveis.
        #[arg(long, short = 't')]
        template: Option<String>,
        /// Depois de gerar, constrói a imagem, arranca e espera ficar saudável.
        #[arg(long)]
        up: bool,
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

/// Tag 9p base a partir do nome do volume: `[a-zA-Z0-9_]`, ≤31 chars (limite do
/// 9p). Como `.` e `-` colapsam ambos em `_`, dois nomes distintos podem gerar a
/// mesma base — a unicidade é garantida por `resolve_vm_volumes` (sufixo por
/// índice), não aqui.
fn vol_tag(name: &str) -> String {
    let mut t: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    t.truncate(31);
    t
}

/// Um `mountPath` de volume tem de ser um caminho absoluto SEM caracteres que
/// partam a sequência de fluxo YAML do cloud-init (`,`/`]`/`#`/`"`) nem control
/// chars — senão a entrada `mounts` fica malformada e o volume não monta em
/// silêncio depois do boot.
fn valid_mount_path(p: &str) -> bool {
    p.starts_with('/')
        && !p
            .chars()
            .any(|c| c.is_control() || matches!(c, ',' | ']' | '[' | '#' | '"'))
}

/// Resolve `spec.volumes` (nomes de Volume/Storage) para `VmVolume` com o
/// directório no host, garantindo que um Storage de rede está montado antes de o
/// partilhar por 9p. Tags únicas (sufixo `_N` em colisão). O `Volume`/`Storage`
/// tem de já existir (o `stack apply` aplica-os antes da VM; o `validate_graph`
/// já confirma a referência).
fn resolve_vm_volumes(
    base: &std::path::Path,
    specs: &[VmVolumeSpec],
) -> Result<Vec<delonix_vm::VmVolume>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let store = VolumeStore::open(base)?;
    let mut used_tags: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(specs.len());
    for v in specs {
        if !valid_mount_path(&v.mount_path) {
            return Err(Error::Invalid(format!(
                "spec.volumes: mountPath {:?} inválido (tem de ser um caminho absoluto sem , ] [ # \" nem control chars)",
                v.mount_path
            )));
        }
        let vol = store.inspect(&v.name).map_err(|_| {
            Error::Invalid(format!(
                "spec.volumes: volume/storage '{}' não existe (cria-o antes da VM)",
                v.name
            ))
        })?;
        // Se for um Storage de rede, garante a montagem no host antes de partilhar.
        store.ensure_mounted(&vol)?;
        // Unicidade da tag: `.` e `-` colapsam em `_`, por isso nomes distintos
        // podem colidir — desambigua com um sufixo `_N` estável por ordem.
        let base_tag = vol_tag(&v.name);
        let mut tag = base_tag.clone();
        let mut n = 1;
        while used_tags.contains(&tag) {
            let suffix = format!("_{n}");
            let keep = 31usize.saturating_sub(suffix.len());
            tag = format!("{}{suffix}", &base_tag[..base_tag.len().min(keep)]);
            n += 1;
        }
        used_tags.insert(tag.clone());
        out.push(delonix_vm::VmVolume {
            tag,
            source: vol.mountpoint.clone(),
            mount_path: v.mount_path.clone(),
            read_only: v.read_only,
        });
    }
    Ok(out)
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let base = state_root();
    for doc in manifest::of_kind(docs, "Vm") {
        let name = &doc.metadata.name;
        manifest::warn_unknown_fields(doc, VM_SPEC_FIELDS);
        let spec: VmSpec = manifest::spec_of(doc)?;

        // Resolve cada volume (nome de Volume/Storage → directório no host) e
        // garante que um Storage de rede está montado antes de o partilhar.
        let vm_volumes = resolve_vm_volumes(&base, &spec.volumes)?;

        // NB: a regra "volumes ⇒ libvirt" vive no motor (`delonix_vm::create`),
        // para qualquer consumidor da API a herdar — aqui passa-se o backend tal
        // como declarado (com CH explícito + volumes, o motor recusa com erro claro).

        // Se há volumes e não foi dado um seed próprio, gera um seed com os mounts
        // 9p (senão o `<filesystem>` existe mas o guest não o monta sozinho).
        let seed = match spec.seed {
            Some(s) => Some(s),
            None if !vm_volumes.is_empty() => Some(
                generate_seed_iso(name, None, &[], None, &vm_volumes)?
                    .to_string_lossy()
                    .into_owned(),
            ),
            None => None,
        };

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
            seed,
            restart_policy: spec.restart_policy,
            hugepages: spec.hugepages,
            cpu_affinity: spec.cpu_affinity,
            devices: spec.devices,
            backend: spec.backend,
            net_mode: spec.net_mode,
            bridge: spec.bridge,
            volumes: vm_volumes,
        };
        delonix_vm::create(&base, &cfg)?;
        println!("vm/{name}: garantida");
    }
    Ok(())
}

pub fn run(action: VmCmd) -> Result<()> {
    if let VmCmd::Init {
        dir,
        name,
        image,
        force,
        template,
        up,
    } = action
    {
        return cmd_init(
            super::scaffold::Target::Vm,
            dir,
            name,
            image,
            force,
            template,
            up,
        );
    }
    if let VmCmd::Dash { once } = action {
        return super::dash::run(super::dash::DashScope::Vms, once);
    }
    let base = state_root();
    match action {
        // Tratado no topo de `run` (faz `return`).
        VmCmd::Init { .. } => unreachable!("tratado acima"),
        VmCmd::Dash { .. } => unreachable!("tratado acima"),
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
                    let iso = generate_seed_iso(
                        &name,
                        hostname.as_deref(),
                        &ssh_keys,
                        user_data.as_deref(),
                        &[],
                    )?;
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
                volumes: vec![],
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
    d.field(
        "PID",
        vm.pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "<none>".into()),
    );
    d.field(
        "Restart policy",
        vm.restart_policy.as_deref().unwrap_or("no"),
    );

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
            .map_err(|e| {
                Error::Invalid(format!(
                    "{} '{path}': {e}",
                    super::po::t("could not read the SSH key from")
                ))
            }),
        None => Ok(spec.trim().to_string()),
    }
}

/// `user-data` NoCloud mínimo — pura, testável sem `cloud-localds` real.
/// `package_update: false`/`package_upgrade: false` porque a imagem dourada
/// já vem pronta (ver `cmd::vmimage`); não faz sentido gastar o primeiro boot
/// a `apt update`.
fn build_user_data(
    hostname: &str,
    ssh_keys: &[String],
    volumes: &[delonix_vm::VmVolume],
) -> String {
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
    // Monta cada volume 9p partilhado pelo `<filesystem>` do domínio. O
    // `_netdev` evita bloquear o boot se o share não estiver pronto; `trans=virtio`
    // + `9p2000.L` é o dialecto que o libvirt/QEMU expõem. Assim o guest monta o
    // NAS/volume SEM o utilizador escrever fstab nem cloud-init à mão.
    if !volumes.is_empty() {
        out.push_str("mounts:\n");
        for v in volumes {
            let mode = if v.read_only { "ro" } else { "rw" };
            // `mount_path` entre aspas (validado sem `"` em `valid_mount_path`) e
            // `tag` saneada (`vol_tag`) — a sequência de fluxo YAML não parte.
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

/// Gera (ou reaproveita, via `user_data_override`) o `user-data`/`meta-data` e
/// empacota-os num ISO NoCloud com `cloud-localds`. Devolve o caminho da ISO.
/// `pub(crate)`: reaproveitada por `cmd::cluster::provision_and_apply` (cada
/// VM provisionada por `delonix cluster kubeadm` precisa do mesmo seed).
pub(crate) fn generate_seed_iso(
    vm_name: &str,
    hostname: Option<&str>,
    ssh_keys: &[String],
    user_data_override: Option<&std::path::Path>,
    volumes: &[delonix_vm::VmVolume],
) -> Result<PathBuf> {
    let hostname = hostname.unwrap_or(vm_name).to_string();
    let work_dir = state_root().join("vms").join(vm_name);
    std::fs::create_dir_all(&work_dir)?;

    let user_data_path = work_dir.join("user-data");
    match user_data_override {
        Some(p) => {
            std::fs::copy(p, &user_data_path).map_err(|e| {
                Error::Invalid(format!(
                    "não consegui copiar --user-data '{}': {e}",
                    p.display()
                ))
            })?;
            // O user-data próprio do utilizador substitui TUDO — não há onde
            // injectar os mounts dos volumes sem os fundir. Avisa em vez de os
            // perder em silêncio (o `<filesystem>` fica no XML, mas o guest não
            // os monta sozinho sem uma entrada `mounts:`).
            if !volumes.is_empty() {
                eprintln!(
                    "AVISO: VM '{vm_name}': --user-data/seed próprio não inclui os mounts dos volumes 9p — acrescenta-os manualmente (tags: {})",
                    volumes.iter().map(|v| v.tag.as_str()).collect::<Vec<_>>().join(", ")
                );
            }
        }
        None => {
            let resolved_keys: Result<Vec<String>> =
                ssh_keys.iter().map(|s| resolve_ssh_key(s)).collect();
            let content = build_user_data(&hostname, &resolved_keys?, volumes);
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
        return Err(Error::Invalid(format!(
            "cloud-localds falhou (exit {:?})",
            status.code()
        )));
    }
    Ok(iso_path)
}

/// Trata o `init` deste grupo (ver `cmd::scaffold`).
fn cmd_init(
    target: super::scaffold::Target,
    dir: PathBuf,
    name: Option<String>,
    image: Option<String>,
    force: bool,
    template: Option<String>,
    up: bool,
) -> Result<()> {
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
    super::scaffold::init(
        target,
        &super::scaffold::InitOpts {
            dir,
            name,
            image,
            force,
            template,
            up,
        },
    )
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
        let ud = build_user_data("myvm", &["ssh-ed25519 AAAA foo".to_string()], &[]);
        assert!(ud.starts_with("#cloud-config\n"));
        assert!(ud.contains("hostname: myvm\n"));
        assert!(ud.contains("ssh_authorized_keys:\n  - ssh-ed25519 AAAA foo\n"));
        assert!(ud.contains("package_update: false\n"));
    }

    #[test]
    fn user_data_sem_chaves_nao_tem_seccao_ssh() {
        let ud = build_user_data("myvm", &[], &[]);
        assert!(!ud.contains("ssh_authorized_keys"));
    }

    #[test]
    fn user_data_com_volumes_injecta_mounts_9p() {
        let vols = vec![
            delonix_vm::VmVolume {
                tag: "dados".into(),
                source: "/srv/dados".into(),
                mount_path: "/mnt/dados".into(),
                read_only: false,
            },
            delonix_vm::VmVolume {
                tag: "ro".into(),
                source: "/srv/ro".into(),
                mount_path: "/mnt/ro".into(),
                read_only: true,
            },
        ];
        let ud = build_user_data("myvm", &[], &vols);
        assert!(ud.contains("mounts:\n"));
        assert!(ud.contains("[ \"dados\", \"/mnt/dados\", 9p, \"trans=virtio,version=9p2000.L,rw,_netdev\", \"0\", \"0\" ]"), "{ud}");
        assert!(ud.contains("[ \"ro\", \"/mnt/ro\", 9p, \"trans=virtio,version=9p2000.L,ro,_netdev\", \"0\", \"0\" ]"), "{ud}");
        // Sem volumes → sem secção mounts.
        assert!(!build_user_data("myvm", &[], &[]).contains("mounts:"));
    }

    #[test]
    fn vol_tag_saneia_e_trunca() {
        assert_eq!(super::vol_tag("nas-creds.db"), "nas_creds_db");
        assert_eq!(super::vol_tag(&"x".repeat(40)).len(), 31);
        // `.` e `-` colapsam ambos em `_` → base igual (a unicidade é no resolve).
        assert_eq!(super::vol_tag("nas.creds"), super::vol_tag("nas-creds"));
    }

    #[test]
    fn valid_mount_path_rejeita_relativos_e_chars_que_partem_o_yaml() {
        assert!(super::valid_mount_path("/mnt/dados"));
        assert!(super::valid_mount_path("/mnt/com espaco")); // espaço é ok (vai entre aspas)
        assert!(!super::valid_mount_path("relativo/x")); // não absoluto
        for bad in ["/mnt/a,b", "/mnt/a]b", "/mnt/a\"b", "/mnt/a#b", "/mnt/a\nb"] {
            assert!(!super::valid_mount_path(bad), "{bad:?} devia ser rejeitado");
        }
    }

    #[test]
    fn meta_data_tem_instance_id_e_hostname() {
        let md = build_meta_data("vm-1", "myvm");
        assert_eq!(md, "instance-id: vm-1\nlocal-hostname: myvm\n");
    }
}
