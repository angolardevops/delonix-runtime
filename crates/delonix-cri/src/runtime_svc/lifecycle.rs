//! Ciclo de vida CRI (pods + containers) sobre o engine Delonix.
//!
//! Estratégia: o estado CRI (sandboxes/containers) vive em ficheiros JSON sob
//! `<base>/cri/`; as operações que usam `clone` (run/stop/rm) **delegam no
//! binário `delonix`** (single-threaded, lógica já verificada), porque o servidor
//! CRI é multi-thread (Tokio) e `clone` não é seguro fora de single-thread. O
//! ESTADO de execução lê-se directamente do `Store` do Delonix.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tonic::{Response, Status};

use crate::cri::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default)]
struct SandboxRec {
    id: String,
    name: String,
    namespace: String,
    uid: String,
    attempt: u32,
    created_at: i64,
    log_directory: String,
    #[serde(default)]
    stopped: bool,
    labels: HashMap<String, String>,
    annotations: HashMap<String, String>,
    /// `true` se o pod usa a rede do NÓ (host network); então NÃO há infra/netns
    /// próprio e os containers correm na rede do host.
    #[serde(default)]
    host_network: bool,
    /// Partilha o PID/IPC namespace do host (`namespace_options.{pid,ipc} = NODE`).
    #[serde(default)]
    host_pid: bool,
    #[serde(default)]
    host_ipc: bool,
    /// `sysctl`s do pod (`chave=valor`), aplicados aos containers do sandbox.
    #[serde(default)]
    sysctls: Vec<String>,
    /// IP (endereço, sem CIDR) atribuído pelo IPAM do CNI quando o sandbox foi
    /// configurado por plugins CNI (rootless, via holder). Vazio = SDN nativo.
    #[serde(default)]
    cni_ip: String,
}

fn sandbox_state(r: &SandboxRec) -> i32 {
    if r.stopped {
        PodSandboxState::SandboxNotready as i32
    } else {
        PodSandboxState::SandboxReady as i32
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct ContainerRec {
    id: String,
    sandbox_id: String,
    name: String,
    attempt: u32,
    image: String,
    command: Vec<String>,
    args: Vec<String>,
    created_at: i64,
    started: bool,
    /// Caminho COMPLETO do ficheiro de log (log_directory do sandbox + log_path do
    /// container) — onde o kubelet/crictl esperam ler stdout/stderr (formato CRI).
    #[serde(default)]
    log_path: String,
    labels: HashMap<String, String>,
    annotations: HashMap<String, String>,
    // --- security context (CRI) traduzido para flags do `delonix run` ---
    #[serde(default)]
    readonly_rootfs: bool,
    #[serde(default)]
    privileged: bool,
    #[serde(default)]
    seccomp_unconfined: bool,
    #[serde(default)]
    cap_add: Vec<String>,
    #[serde(default)]
    cap_drop: Vec<String>,
    #[serde(default)]
    apparmor: Option<String>,
}

/// `true` se o perfil AppArmor está carregado no host (em
/// `/sys/kernel/security/apparmor/profiles`).
fn apparmor_loaded(profile: &str) -> bool {
    std::fs::read_to_string("/sys/kernel/security/apparmor/profiles")
        .map(|s| s.lines().any(|l| l.split_whitespace().next() == Some(profile)))
        .unwrap_or(false)
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn sb_dir(base: &Path) -> PathBuf {
    base.join("cri").join("sandboxes")
}
fn ct_dir(base: &Path) -> PathBuf {
    base.join("cri").join("containers")
}
fn st<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

fn write_rec<T: Serialize>(dir: &Path, id: &str, rec: &T) -> Result<(), Status> {
    std::fs::create_dir_all(dir).map_err(st)?;
    let bytes = serde_json::to_vec_pretty(rec).map_err(st)?;
    // Escrita ATÓMICA (temp + rename): o servidor CRI é multi-thread, e um
    // `container_status`/`list_containers` concorrente nunca deve ler um ficheiro
    // truncado a meio de uma escrita.
    let final_path = dir.join(format!("{id}.json"));
    let tmp = dir.join(format!(".{id}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(st)?;
    std::fs::rename(&tmp, &final_path).map_err(st)
}
fn read_rec<T: for<'de> Deserialize<'de>>(dir: &Path, id: &str) -> Result<T, Status> {
    let data = std::fs::read(dir.join(format!("{id}.json")))
        .map_err(|_| Status::not_found(format!("{id} não encontrado")))?;
    serde_json::from_slice(&data).map_err(st)
}
fn list_recs<T: for<'de> Deserialize<'de>>(dir: &Path) -> Vec<T> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(data) = std::fs::read(e.path()) {
                if let Ok(r) = serde_json::from_slice(&data) {
                    out.push(r);
                }
            }
        }
    }
    out
}

fn delonix_bin() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("delonix"))
}

/// Corre o binário `delonix` (single-threaded) com o `DELONIX_ROOT` da CRI.
/// `DELONIX_INTERNAL=1` ignora a barreira dos comandos agrupados (delegação
/// máquina-a-máquina): o CRI usa as formas de topo `run`/`stop`/`rm`.
fn delonix(base: &Path, args: &[&str]) -> Result<std::process::Output, Status> {
    Command::new(delonix_bin())
        .env("DELONIX_ROOT", base)
        .env("DELONIX_INTERNAL", "1")
        .args(args)
        .output()
        .map_err(st)
}

/// Como [`delonix`], mas com stdio em `/dev/null` — OBRIGATÓRIO para `run -d`: o
/// container daemonizado herda e SEGURA os *pipes* de stdout/stderr; com `.output()`
/// o `wait` ficaria preso até o container terminar (bug "run -d | tail pendura").
fn delonix_detached(base: &Path, args: &[&str]) -> Result<bool, Status> {
    use std::process::Stdio;
    let status = Command::new(delonix_bin())
        .env("DELONIX_ROOT", base)
        .env("DELONIX_INTERNAL", "1")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(st)?;
    Ok(status.success())
}

/// Carrega um container CRI e **reconcilia** o seu status contra o kernel
/// (`Running`+pid morto → `Crashed`/`Failed`) antes de o devolver, persistindo a
/// mudança (best-effort). É o cerne da correção de exit-codes: sem reconciliar,
/// um container que crashou mas cujo store ainda diz `Running` reportava estado
/// `Exited` com exit-code 0 → o kubelet (restartPolicy `OnFailure`) NÃO o
/// reiniciava. Após reconciliar, o crash vira `Crashed` (137) e o kubelet reage.
fn load_reconciled(base: &Path, cri_id: &str) -> Option<delonix_runtime_core::Container> {
    let store = delonix_runtime_core::Store::open(base.join("containers")).ok()?;
    let mut c = store.load(&format!("cri-{cri_id}")).ok()?;
    if delonix_runtime::reconcile_status(&mut c) {
        let _ = store.save(&c); // propaga a reconciliação a outros leitores
    }
    Some(c)
}

/// O estado de execução de um container CRI, lido (e reconciliado) do `Store`.
fn delonix_state(base: &Path, cri_id: &str) -> i32 {
    use delonix_runtime_core::Status as S;
    match load_reconciled(base, cri_id) {
        Some(c) => match c.status {
            S::Running if c.pid.map(delonix_runtime::is_alive).unwrap_or(false) => {
                ContainerState::ContainerRunning as i32
            }
            S::Running => ContainerState::ContainerExited as i32, // defensivo (pós-reconcile)
            S::Paused => ContainerState::ContainerRunning as i32, // congelado, mas existe
            S::Stopped | S::Failed(_) | S::Crashed => ContainerState::ContainerExited as i32,
            S::Created => ContainerState::ContainerCreated as i32,
        },
        None => ContainerState::ContainerUnknown as i32,
    }
}

/// O código de saída de um container CRI (reconciliado), ou `None` se ainda está
/// a correr/criado. Permite ao kubelet ver a verdadeira causa de saída (137/143/n)
/// e aplicar a `restartPolicy` — em vez de assumir 0 (`Completed`) para tudo.
fn delonix_exit(base: &Path, cri_id: &str) -> Option<i32> {
    use delonix_runtime_core::Status as S;
    match load_reconciled(base, cri_id)?.status {
        S::Failed(code) => Some(code),
        S::Stopped => Some(0),
        S::Crashed => Some(137),
        _ => None,
    }
}

// ---- pods (sandboxes) -----------------------------------------------------

pub fn run_pod_sandbox(
    base: &Path,
    req: RunPodSandboxRequest,
) -> Result<Response<RunPodSandboxResponse>, Status> {
    let cfg = req.config.ok_or_else(|| Status::invalid_argument("sem config"))?;
    let md = cfg.metadata.clone().unwrap_or_default();
    let id = delonix_runtime_core::generate_id();
    // Rede do host? (namespace_options.network == NODE) → sem infra/netns próprio.
    let ns = cfg
        .linux
        .as_ref()
        .and_then(|l| l.security_context.as_ref())
        .and_then(|s| s.namespace_options.as_ref());
    let is_node = |m: i32| m == NamespaceMode::Node as i32;
    let host_network = ns.map(|n| is_node(n.network)).unwrap_or(false);
    let host_pid = ns.map(|n| is_node(n.pid)).unwrap_or(false);
    let host_ipc = ns.map(|n| is_node(n.ipc)).unwrap_or(false);
    // sysctls do pod (`net.*`, `kernel.shm*`, …) → `chave=valor`.
    let sysctls: Vec<String> = cfg
        .linux
        .as_ref()
        .map(|l| l.sysctls.iter().map(|(k, v)| format!("{k}={v}")).collect())
        .unwrap_or_default();
    // Pod REAL do Delonix: um infra container (`pod-cri-<id>`) detém o netns
    // partilhado (estilo "pause"), que os containers do sandbox passam a juntar
    // via `--pod`. É o que dá networking de pod e partilha de namespaces.
    // CNI (opt-in `DELONIX_CNI=1` + conflist): o sandbox obtém a rede de plugins CNI
    // reais (a cadeia do cluster, ex. Calico), como no containerd/CRI-O. Rootless →
    // os plugins correm no holder (dono da netns); a netns chama-se `cri-<id>` para
    // os containers do sandbox se juntarem via `--pod cri-<id>` (join_argv). Sem a
    // flag, `enabled_conf()` é None e segue o caminho nativo (SDN) inalterado.
    let mut cni_ip = String::new();
    if !host_network {
        let pod = format!("cri-{id}");
        let cni = delonix_net::cni::enabled_conf();
        if cni.is_some() && delonix_runtime::is_rootless() {
            let conf = cni.unwrap();
            let conf_json = serde_json::to_string(&conf)
                .map_err(|e| Status::internal(format!("serializar conflist: {e}")))?;
            match delonix_net::infra::cni_attach_container(&pod, &conf_json) {
                Ok((_netns, cidr)) => {
                    cni_ip = cidr.split('/').next().unwrap_or("").to_string();
                }
                Err(e) => return Err(Status::internal(format!("CNI ADD do sandbox {pod}: {e}"))),
            }
        } else if delonix_runtime::is_rootless() {
            // ROOTLESS: o pod é um netns PARTILHADO do ingress (delonix0 + DHCP +
            // DNS + firewall); os containers do sandbox juntam-se via `--pod`.
            if !delonix_detached(base, &["netns", "attach", &pod])? {
                return Err(Status::internal(format!("falha a criar o sandbox de ingress {pod}")));
            }
        } else if !delonix_detached(base, &["pod", "create", &pod, "--network"])? {
            // ROOT: infra container (`pod-cri-<id>`) detém o netns (estilo "pause").
            return Err(Status::internal(format!("falha a criar o pod sandbox {pod}")));
        }
    }
    let rec = SandboxRec {
        id: id.clone(),
        name: md.name,
        namespace: md.namespace,
        uid: md.uid,
        attempt: md.attempt,
        created_at: now_ns(),
        log_directory: cfg.log_directory,
        stopped: false,
        labels: cfg.labels,
        annotations: cfg.annotations,
        host_network,
        host_pid,
        host_ipc,
        sysctls,
        cni_ip,
    };
    write_rec(&sb_dir(base), &id, &rec)?;
    Ok(Response::new(RunPodSandboxResponse { pod_sandbox_id: id }))
}

pub fn stop_pod_sandbox(
    base: &Path,
    id: String,
) -> Result<Response<StopPodSandboxResponse>, Status> {
    // pára os containers do sandbox e marca-o NotReady.
    for c in list_recs::<ContainerRec>(&ct_dir(base)) {
        if c.sandbox_id == id {
            let _ = delonix(base, &["stop", &format!("cri-{}", c.id)]);
        }
    }
    if let Ok(mut r) = read_rec::<SandboxRec>(&sb_dir(base), &id) {
        r.stopped = true;
        let _ = write_rec(&sb_dir(base), &id, &r);
    }
    Ok(Response::new(StopPodSandboxResponse {}))
}

pub fn remove_pod_sandbox(
    base: &Path,
    id: String,
) -> Result<Response<RemovePodSandboxResponse>, Status> {
    for c in list_recs::<ContainerRec>(&ct_dir(base)) {
        if c.sandbox_id == id {
            let _ = delonix(base, &["rm", "-f", &format!("cri-{}", c.id)]);
            let _ = std::fs::remove_file(ct_dir(base).join(format!("{}.json", c.id)));
        }
    }
    // Remove o pod real do Delonix (infra container + netns), se existia.
    if let Ok(sb) = read_rec::<SandboxRec>(&sb_dir(base), &id) {
        if !sb.host_network {
            if !sb.cni_ip.is_empty() {
                // sandbox configurado por CNI (rootless): DEL dos plugins no holder.
                if let Some(conf) = delonix_net::cni::enabled_conf() {
                    let cj = serde_json::to_string(&conf).unwrap_or_default();
                    let _ = delonix_net::infra::cni_detach_container(&format!("cri-{id}"), &cj);
                }
            } else if delonix_runtime::is_rootless() {
                let _ = delonix(base, &["netns", "detach", &format!("cri-{id}")]);
            } else {
                let _ = delonix(base, &["pod", "rm", &format!("cri-{id}")]);
            }
        }
    }
    let _ = std::fs::remove_file(sb_dir(base).join(format!("{id}.json")));
    Ok(Response::new(RemovePodSandboxResponse {}))
}

fn to_pod_sandbox(r: &SandboxRec) -> PodSandbox {
    PodSandbox {
        id: r.id.clone(),
        metadata: Some(PodSandboxMetadata {
            name: r.name.clone(),
            uid: r.uid.clone(),
            namespace: r.namespace.clone(),
            attempt: r.attempt,
        }),
        state: sandbox_state(r),
        created_at: r.created_at,
        labels: r.labels.clone(),
        annotations: r.annotations.clone(),
        runtime_handler: String::new(),
    }
}

pub fn list_pod_sandbox(base: &Path) -> Result<Response<ListPodSandboxResponse>, Status> {
    let items = list_recs::<SandboxRec>(&sb_dir(base)).iter().map(to_pod_sandbox).collect();
    Ok(Response::new(ListPodSandboxResponse { items }))
}

pub fn pod_sandbox_status(
    base: &Path,
    id: String,
) -> Result<Response<PodSandboxStatusResponse>, Status> {
    let r: SandboxRec = read_rec(&sb_dir(base), &id)?;
    // IP do pod: o do infra container (`pod-cri-<id>`), que detém o netns.
    let ip = if r.host_network {
        String::new()
    } else if !r.cni_ip.is_empty() {
        // sandbox configurado por CNI: o IP veio do IPAM do plugin.
        r.cni_ip.clone()
    } else if delonix_runtime::is_rootless() {
        // ROOTLESS: IP do netns partilhado do pod no ingress (determinístico).
        delonix_net::infra::container_ip(&format!("cri-{}", r.id))
    } else {
        delonix_runtime_core::Store::open(base.join("containers"))
            .ok()
            .and_then(|s| s.load(&format!("pod-cri-{}", r.id)).ok())
            .and_then(|c| c.ip)
            .unwrap_or_default()
    };
    let status = PodSandboxStatus {
        id: r.id.clone(),
        metadata: Some(PodSandboxMetadata {
            name: r.name.clone(),
            uid: r.uid.clone(),
            namespace: r.namespace.clone(),
            attempt: r.attempt,
        }),
        state: sandbox_state(&r),
        created_at: r.created_at,
        network: Some(PodSandboxNetworkStatus { ip, additional_ips: vec![] }),
        linux: None,
        labels: r.labels.clone(),
        annotations: r.annotations.clone(),
        runtime_handler: String::new(),
    };
    Ok(Response::new(PodSandboxStatusResponse {
        status: Some(status),
        info: Default::default(),
        containers_statuses: vec![],
        timestamp: now_ns(),
    }))
}

// ---- containers -----------------------------------------------------------

pub fn create_container(
    base: &Path,
    req: CreateContainerRequest,
) -> Result<Response<CreateContainerResponse>, Status> {
    let cfg = req.config.ok_or_else(|| Status::invalid_argument("sem config"))?;
    let md = cfg.metadata.unwrap_or_default();
    let image = cfg.image.map(|s| s.image).unwrap_or_default();
    if image.is_empty() {
        return Err(Status::invalid_argument("imagem em falta"));
    }
    let id = delonix_runtime_core::generate_id();
    // Security context (CRI) → flags do `delonix run` (aplicadas no start).
    let sc = cfg.linux.as_ref().and_then(|l| l.security_context.as_ref());
    let readonly_rootfs = sc.map(|s| s.readonly_rootfs).unwrap_or(false);
    let privileged = sc.map(|s| s.privileged).unwrap_or(false);
    let (cap_add, cap_drop) = sc
        .and_then(|s| s.capabilities.as_ref())
        .map(|c| (c.add_capabilities.clone(), c.drop_capabilities.clone()))
        .unwrap_or_default();
    let seccomp_unconfined = sc
        .and_then(|s| s.seccomp.as_ref())
        .map(|p| p.profile_type == security_profile::ProfileType::Unconfined as i32)
        .unwrap_or(false);
    // AppArmor: o campo NOVO (`apparmor`, SecurityProfile) tem precedência; se não
    // estiver definido, cai para o campo DEPRECIADO `apparmor_profile` (string,
    // formato `unconfined` | `localhost/<perfil>` | `runtime/default` | `<perfil>`).
    let apparmor = sc
        .and_then(|s| s.apparmor.as_ref())
        .and_then(|p| match security_profile::ProfileType::try_from(p.profile_type) {
            Ok(security_profile::ProfileType::Unconfined) => Some("unconfined".to_string()),
            Ok(security_profile::ProfileType::Localhost) if !p.localhost_ref.is_empty() => {
                Some(p.localhost_ref.clone())
            }
            _ => None,
        })
        .or_else(|| {
            #[allow(deprecated)] // suporte intencional ao campo CRI depreciado
            let s = sc.map(|s| s.apparmor_profile.as_str()).unwrap_or("");
            match s {
                "" | "runtime/default" => None,
                "unconfined" => Some("unconfined".into()),
                _ => Some(s.strip_prefix("localhost/").unwrap_or(s).to_string()),
            }
        });
    // Valida JÁ no CreateContainer (como o runc): um perfil AppArmor que não esteja
    // carregado no host faz a criação falhar (o cri-tools verifica-o aqui).
    if let Some(p) = &apparmor {
        if p != "unconfined" && p != "delonix-default" && !apparmor_loaded(p) {
            return Err(Status::invalid_argument(format!(
                "perfil AppArmor '{p}' não está carregado no host"
            )));
        }
    }
    // Caminho de log completo: o `log_path` é relativo ao `log_directory` do
    // sandbox (o kubelet sempre o dá assim). REJEITA `..` e caminhos absolutos —
    // senão um pedido malicioso escreveria ficheiros fora do diretório de logs.
    let full_log_path = {
        let lp = cfg.log_path.clone();
        if lp.is_empty() {
            String::new()
        } else if lp.starts_with('/')
            || lp.split('/').any(|seg| seg == ".." || seg == ".")
        {
            return Err(Status::invalid_argument(
                "log_path inválido: tem de ser relativo e sem '..'",
            ));
        } else {
            let dir = read_rec::<SandboxRec>(&sb_dir(base), &req.pod_sandbox_id)
                .map(|s| s.log_directory)
                .unwrap_or_default();
            if dir.is_empty() {
                String::new()
            } else {
                format!("{}/{}", dir.trim_end_matches('/'), lp)
            }
        }
    };
    let rec = ContainerRec {
        id: id.clone(),
        sandbox_id: req.pod_sandbox_id,
        name: md.name,
        attempt: md.attempt,
        image,
        command: cfg.command,
        args: cfg.args,
        created_at: now_ns(),
        started: false,
        log_path: full_log_path,
        labels: cfg.labels,
        annotations: cfg.annotations,
        readonly_rootfs,
        privileged,
        seccomp_unconfined,
        cap_add,
        cap_drop,
        apparmor,
    };
    write_rec(&ct_dir(base), &id, &rec)?;
    Ok(Response::new(CreateContainerResponse { container_id: id }))
}

pub fn start_container(
    base: &Path,
    id: String,
) -> Result<Response<StartContainerResponse>, Status> {
    let mut rec: ContainerRec = read_rec(&ct_dir(base), &id)?;
    let name = format!("cri-{id}");
    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), name];
    // Logs no caminho/formato que o kubelet/crictl esperam (CRI), se houver.
    if !rec.log_path.is_empty() {
        if let Some(dir) = std::path::Path::new(&rec.log_path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        args.push("--log-file".into());
        args.push(rec.log_path.clone());
        args.push("--log-cri".into());
    }
    // Junta-se ao netns do pod sandbox (partilha de rede/namespaces), salvo se o
    // pod usa a rede do host.
    if let Ok(sb) = read_rec::<SandboxRec>(&sb_dir(base), &rec.sandbox_id) {
        if !sb.host_network {
            args.push("--pod".into());
            args.push(format!("cri-{}", rec.sandbox_id));
        }
        // Namespaces de host herdados do pod sandbox.
        if sb.host_pid {
            args.push("--host-pid".into());
        }
        if sb.host_ipc {
            args.push("--host-ipc".into());
        }
        // sysctls do pod, aplicados no container (partilha os namespaces do pod).
        for s in &sb.sysctls {
            args.push("--sysctl".into());
            args.push(s.clone());
        }
    }
    // Security context → flags.
    if rec.readonly_rootfs {
        args.push("--read-only".into());
    }
    if rec.privileged {
        args.push("--cap-add".into());
        args.push("ALL".into());
        args.push("--security-opt".into());
        args.push("seccomp=unconfined".into());
    } else if rec.seccomp_unconfined {
        args.push("--security-opt".into());
        args.push("seccomp=unconfined".into());
    }
    for c in &rec.cap_add {
        args.push("--cap-add".into());
        args.push(c.trim_start_matches("CAP_").to_string());
    }
    for c in &rec.cap_drop {
        args.push("--cap-drop".into());
        args.push(c.trim_start_matches("CAP_").to_string());
    }
    if let Some(prof) = &rec.apparmor {
        args.push("--apparmor".into());
        args.push(prof.clone());
    }
    // `--` separa as flags dos posicionais: impede que um `image`/`command` vindo
    // do pedido CRI e começado por `-` seja interpretado como flag (injecção).
    args.push("--".into());
    args.push(rec.image.clone());
    args.extend(rec.command.iter().cloned());
    args.extend(rec.args.iter().cloned());
    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    if !delonix_detached(base, &argv)? {
        return Err(Status::internal(format!("falha a arrancar o container {id}")));
    }
    rec.started = true;
    write_rec(&ct_dir(base), &id, &rec)?;
    Ok(Response::new(StartContainerResponse {}))
}

pub fn stop_container(
    base: &Path,
    id: String,
    timeout: i64,
) -> Result<Response<StopContainerResponse>, Status> {
    // Honra o período de graça do pedido CRI (segundos): o kubelet/crictl impõem
    // a sua própria deadline, por isso NÃO podemos usar o default longo do
    // `delonix stop`. `timeout=0` → paragem imediata (SIGKILL).
    let secs = timeout.max(0).to_string();
    let _ = delonix(base, &["stop", "-t", &secs, &format!("cri-{id}")])?;
    // Verifica que PAROU de facto (reconciliado). Idempotente: já parado/inexistente
    // = OK. Se continua vivo, propaga erro → o kubelet repete (em vez de assumir
    // que parou e seguir para o RemoveContainer sobre um processo ainda a correr).
    if let Some(c) = load_reconciled(base, &id) {
        let alive = matches!(c.status, delonix_runtime_core::Status::Running)
            && c.pid.map(delonix_runtime::is_alive).unwrap_or(false);
        if alive {
            return Err(Status::internal(format!("'cri-{id}' continua a correr após stop")));
        }
    }
    Ok(Response::new(StopContainerResponse {}))
}

pub fn remove_container(
    base: &Path,
    id: String,
) -> Result<Response<RemoveContainerResponse>, Status> {
    // SÓ apagar o registo CRI DEPOIS de o runtime remover o container. Antes,
    // apagava-se o JSON mesmo com o `rm -f` falhado → fuga de rootfs/subuid/netns
    // sem rasto para o kubelet reptir. Idempotente (contrato CRI): um container
    // que já não existe conta como removido.
    let out = delonix(base, &["rm", "-f", &format!("cri-{id}")])?;
    let gone = out.status.success() || {
        let e = String::from_utf8_lossy(&out.stderr).to_lowercase();
        e.contains("no such") || e.contains("não existe") || e.contains("not found")
    };
    if !gone {
        return Err(Status::internal(format!(
            "remoção de 'cri-{id}' falhou (registo preservado p/ retry): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let _ = std::fs::remove_file(ct_dir(base).join(format!("{id}.json")));
    Ok(Response::new(RemoveContainerResponse {}))
}

fn to_container(base: &Path, r: &ContainerRec) -> Container {
    Container {
        id: r.id.clone(),
        pod_sandbox_id: r.sandbox_id.clone(),
        metadata: Some(ContainerMetadata { name: r.name.clone(), attempt: r.attempt }),
        image: Some(ImageSpec { image: r.image.clone(), ..Default::default() }),
        image_ref: r.image.clone(),
        state: delonix_state(base, &r.id),
        created_at: r.created_at,
        labels: r.labels.clone(),
        annotations: r.annotations.clone(),
        image_id: r.image.clone(),
    }
}

pub fn list_containers(base: &Path) -> Result<Response<ListContainersResponse>, Status> {
    let containers = list_recs::<ContainerRec>(&ct_dir(base))
        .iter()
        .map(|r| to_container(base, r))
        .collect();
    Ok(Response::new(ListContainersResponse { containers }))
}

pub fn container_status(
    base: &Path,
    id: String,
) -> Result<Response<ContainerStatusResponse>, Status> {
    let r: ContainerRec = read_rec(&ct_dir(base), &id)?;
    // Código de saída real (do Store), para o kubelet ver a causa de saída em vez
    // de um `0` fixo. `finished_at`/`reason` acompanham.
    let exit = delonix_exit(base, &r.id);
    let status = ContainerStatus {
        id: r.id.clone(),
        metadata: Some(ContainerMetadata { name: r.name.clone(), attempt: r.attempt }),
        state: delonix_state(base, &r.id),
        created_at: r.created_at,
        started_at: if r.started { r.created_at } else { 0 },
        finished_at: if exit.is_some() { now_ns() } else { 0 },
        exit_code: exit.unwrap_or(0),
        image: Some(ImageSpec { image: r.image.clone(), ..Default::default() }),
        image_ref: r.image.clone(),
        log_path: r.log_path.clone(),
        reason: match exit {
            Some(0) => "Completed".into(),
            Some(_) => "Error".into(),
            None => String::new(),
        },
        ..Default::default()
    };
    Ok(Response::new(ContainerStatusResponse { status: Some(status), info: Default::default() }))
}

// ---------------------------------------------------------------------------
// ExecSync: corre um comando no container e devolve stdout/stderr/exit. É o que
// o kubelet usa para sondas `exec` (liveness/readiness) e o `crictl exec -s`.
// ---------------------------------------------------------------------------

pub fn exec_sync(
    base: &Path,
    id: String,
    cmd: Vec<String>,
    timeout: i64,
) -> Result<Response<ExecSyncResponse>, Status> {
    if cmd.is_empty() {
        return Err(Status::invalid_argument("exec_sync sem comando"));
    }
    let name = format!("cri-{id}");
    // Delega no binário `delonix exec` (single-threaded; faz setns ao container).
    // O timeout (segundos, >0) é imposto pelo coreutil `timeout` por robustez.
    let mut command = Command::new(delonix_bin());
    command.env("DELONIX_ROOT", base).env("DELONIX_INTERNAL", "1");
    if timeout > 0 {
        command = Command::new("timeout");
        command
            .env("DELONIX_ROOT", base)
            .env("DELONIX_INTERNAL", "1")
            .arg(timeout.to_string())
            .arg(delonix_bin());
    }
    let out = command.arg("exec").arg(&name).args(&cmd).output().map_err(st)?;
    // `timeout` devolve 124 quando expira → mapeia para um exit code distinto.
    let exit_code = out.status.code().unwrap_or(-1);
    Ok(Response::new(ExecSyncResponse {
        stdout: out.stdout,
        stderr: out.stderr,
        exit_code,
    }))
}

// ---------------------------------------------------------------------------
// Métricas (CRI stats) — reais, lidas do cgroup v2 do container. É o que o
// kubelet usa para Summary API / HPA. C2.
// ---------------------------------------------------------------------------

/// Lê um inteiro de um ficheiro do cgroup (`memory.current`, `pids.current`, …).
fn cg_u64(cgroup: &str, file: &str) -> u64 {
    std::fs::read_to_string(format!("{cgroup}/{file}"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Lê um campo `chave valor` de um ficheiro estilo `cpu.stat`/`memory.stat`.
fn cg_field(cgroup: &str, file: &str, key: &str) -> u64 {
    std::fs::read_to_string(format!("{cgroup}/{file}"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                let mut it = l.split_whitespace();
                (it.next() == Some(key)).then(|| it.next().and_then(|v| v.parse().ok()))?
            })
        })
        .unwrap_or(0)
}

/// O cgroup de um container CRI (`cri-<id>`), via o `Store` do Delonix.
fn container_cgroup(base: &Path, cri_id: &str) -> Option<String> {
    let store = delonix_runtime_core::Store::open(base.join("containers")).ok()?;
    store.load(&format!("cri-{cri_id}")).ok().map(|c| c.cgroup())
}

/// Soma o `VmRSS` (bytes) de todos os processos do cgroup, lendo `/proc`. É a
/// fonte de memória quando o `memory.current` do cgroup sub-reporta (o init é
/// colocado no cgroup após o *exec*, logo as páginas faltadas antes não são
/// cobradas a este cgroup — mas os PIDs ESTÃO cá, e o `/proc` diz a verdade).
fn cgroup_rss_bytes(cgroup: &str) -> u64 {
    let procs = match std::fs::read_to_string(format!("{cgroup}/cgroup.procs")) {
        Ok(p) => p,
        Err(_) => return 0,
    };
    let mut total = 0u64;
    for pid in procs.lines() {
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", pid.trim())) {
            for l in status.lines() {
                if let Some(rest) = l.strip_prefix("VmRSS:") {
                    if let Some(kb) = rest.split_whitespace().next().and_then(|v| v.parse::<u64>().ok()) {
                        total += kb * 1024;
                    }
                }
            }
        }
    }
    total
}

fn u64v(value: u64) -> Option<UInt64Value> {
    Some(UInt64Value { value })
}

/// Constrói as métricas reais de um container a partir do seu cgroup v2.
fn container_stats_for(base: &Path, r: &ContainerRec) -> ContainerStats {
    let ts = now_ns();
    let cg = container_cgroup(base, &r.id);
    let (cpu_ns, mem_cur, working_set, rss, pgfault, pgmajfault) = match &cg {
        Some(cg) => {
            let cpu_us = cg_field(cg, "cpu.stat", "usage_usec");
            let cur = cg_u64(cg, "memory.current");
            let inactive = cg_field(cg, "memory.stat", "inactive_file");
            let anon = cg_field(cg, "memory.stat", "anon");
            // O cgroup sub-reporta a memória (cobrança tardia); cai para o RSS
            // real dos processos do cgroup, que é a verdade observável.
            let (usage, working, rss) = if cur > 0 {
                (cur, cur.saturating_sub(inactive), anon)
            } else {
                let rss = cgroup_rss_bytes(cg);
                (rss, rss, rss)
            };
            (
                cpu_us.saturating_mul(1000), // µs → ns
                usage,
                working,
                rss,
                cg_field(cg, "memory.stat", "pgfault"),
                cg_field(cg, "memory.stat", "pgmajfault"),
            )
        }
        None => (0, 0, 0, 0, 0, 0),
    };
    ContainerStats {
        attributes: Some(ContainerAttributes {
            id: r.id.clone(),
            metadata: Some(ContainerMetadata { name: r.name.clone(), attempt: r.attempt }),
            labels: r.labels.clone(),
            annotations: r.annotations.clone(),
        }),
        cpu: Some(CpuUsage {
            timestamp: ts,
            usage_core_nano_seconds: u64v(cpu_ns),
            usage_nano_cores: u64v(0),
        }),
        memory: Some(MemoryUsage {
            timestamp: ts,
            working_set_bytes: u64v(working_set),
            available_bytes: u64v(0),
            usage_bytes: u64v(mem_cur),
            rss_bytes: u64v(rss),
            page_faults: u64v(pgfault),
            major_page_faults: u64v(pgmajfault),
        }),
        writable_layer: Some(FilesystemUsage {
            timestamp: ts,
            fs_id: Some(FilesystemIdentifier {
                mountpoint: base.join("containers").join(format!("cri-{}", r.id)).to_string_lossy().into_owned(),
            }),
            used_bytes: u64v(0),
            inodes_used: u64v(0),
        }),
        swap: Some(SwapUsage {
            timestamp: ts,
            swap_available_bytes: u64v(0),
            swap_usage_bytes: u64v(cg.as_deref().map(|c| cg_u64(c, "memory.swap.current")).unwrap_or(0)),
        }),
    }
}

pub fn container_stats(base: &Path, id: String) -> Result<Response<ContainerStatsResponse>, Status> {
    let r: ContainerRec = read_rec(&ct_dir(base), &id)?;
    Ok(Response::new(ContainerStatsResponse { stats: Some(container_stats_for(base, &r)) }))
}

pub fn list_container_stats(
    base: &Path,
    filter: Option<ContainerStatsFilter>,
) -> Result<Response<ListContainerStatsResponse>, Status> {
    let (fid, fsb) = filter
        .map(|f| (f.id, f.pod_sandbox_id))
        .unwrap_or_default();
    let stats = list_recs::<ContainerRec>(&ct_dir(base))
        .into_iter()
        .filter(|r| (fid.is_empty() || r.id == fid) && (fsb.is_empty() || r.sandbox_id == fsb))
        .map(|r| container_stats_for(base, &r))
        .collect();
    Ok(Response::new(ListContainerStatsResponse { stats }))
}

/// Métricas de um pod sandbox: agrega os containers do sandbox (cpu/memória).
fn pod_sandbox_stats_for(base: &Path, sb: &SandboxRec) -> PodSandboxStats {
    let ts = now_ns();
    let conts: Vec<ContainerStats> = list_recs::<ContainerRec>(&ct_dir(base))
        .into_iter()
        .filter(|r| r.sandbox_id == sb.id)
        .map(|r| container_stats_for(base, &r))
        .collect();
    let sum = |pick: &dyn Fn(&ContainerStats) -> u64| conts.iter().map(pick).sum::<u64>();
    let cpu_ns = sum(&|c| c.cpu.as_ref().and_then(|x| x.usage_core_nano_seconds.as_ref()).map(|v| v.value).unwrap_or(0));
    let mem = sum(&|c| c.memory.as_ref().and_then(|x| x.usage_bytes.as_ref()).map(|v| v.value).unwrap_or(0));
    let ws = sum(&|c| c.memory.as_ref().and_then(|x| x.working_set_bytes.as_ref()).map(|v| v.value).unwrap_or(0));
    PodSandboxStats {
        attributes: Some(PodSandboxAttributes {
            id: sb.id.clone(),
            metadata: Some(PodSandboxMetadata {
                name: sb.name.clone(),
                namespace: sb.namespace.clone(),
                uid: sb.uid.clone(),
                attempt: sb.attempt,
            }),
            labels: sb.labels.clone(),
            annotations: sb.annotations.clone(),
        }),
        linux: Some(LinuxPodSandboxStats {
            cpu: Some(CpuUsage { timestamp: ts, usage_core_nano_seconds: u64v(cpu_ns), usage_nano_cores: u64v(0) }),
            memory: Some(MemoryUsage {
                timestamp: ts,
                working_set_bytes: u64v(ws),
                available_bytes: u64v(0),
                usage_bytes: u64v(mem),
                rss_bytes: u64v(0),
                page_faults: u64v(0),
                major_page_faults: u64v(0),
            }),
            network: None,
            process: Some(ProcessUsage { timestamp: ts, process_count: u64v(conts.len() as u64) }),
            containers: conts,
        }),
        windows: None,
    }
}

pub fn pod_sandbox_stats(base: &Path, id: String) -> Result<Response<PodSandboxStatsResponse>, Status> {
    let sb: SandboxRec = read_rec(&sb_dir(base), &id)?;
    Ok(Response::new(PodSandboxStatsResponse { stats: Some(pod_sandbox_stats_for(base, &sb)) }))
}

pub fn list_pod_sandbox_stats(
    base: &Path,
    filter: Option<PodSandboxStatsFilter>,
) -> Result<Response<ListPodSandboxStatsResponse>, Status> {
    let fid = filter.map(|f| f.id).unwrap_or_default();
    let stats = list_recs::<SandboxRec>(&sb_dir(base))
        .into_iter()
        .filter(|s| fid.is_empty() || s.id == fid)
        .map(|s| pod_sandbox_stats_for(base, &s))
        .collect();
    Ok(Response::new(ListPodSandboxStatsResponse { stats }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crashed_container_reporta_137_nao_0() {
        // Container marcado `Running` no store mas com pid MORTO — simula um crash
        // ainda não reconciliado. Sem o fix, delonix_exit devolvia None → o kubelet
        // via exit 0 (Completed) e o restartPolicy OnFailure NÃO reiniciava.
        let tmp = std::env::temp_dir().join(format!("dlx-cri-exit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = delonix_runtime_core::Store::open(tmp.join("containers")).unwrap();
        let mut c = delonix_runtime_core::Container::new(
            "cri-abc".into(),
            "cri-abc".into(),
            "img:1".into(),
            vec![],
            String::new(),
        );
        c.status = delonix_runtime_core::Status::Running;
        c.pid = Some(2_000_000); // pid inexistente → morto
        store.save(&c).unwrap();

        // reconcilia (Running+morto → Crashed) → exit 137 + estado Exited.
        assert_eq!(delonix_exit(&tmp, "abc"), Some(137), "crash deve reportar 137, não 0");
        assert_eq!(delonix_state(&tmp, "abc"), ContainerState::ContainerExited as i32);

        // Um container parado limpo → 0 (Completed). Um Failed(n) → n.
        let mut ok = c.clone();
        ok.status = delonix_runtime_core::Status::Stopped;
        store.save(&ok).unwrap();
        assert_eq!(delonix_exit(&tmp, "abc"), Some(0));
        let mut failed = c.clone();
        failed.status = delonix_runtime_core::Status::Failed(2);
        store.save(&failed).unwrap();
        assert_eq!(delonix_exit(&tmp, "abc"), Some(2));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
