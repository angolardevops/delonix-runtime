//! `delonix container` — ciclo de vida de containers (run/ps/stop/rm/exec/logs).

use std::path::PathBuf;

use clap::Subcommand;
use delonix_image::ImageStore;
use delonix_net::infra;
use delonix_runtime::{self as runtime, RunSpec};
use delonix_runtime_core::{generate_id, Container, Error, Result, Store};
use delonix_volume::VolumeStore;
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::util::{effective_command, find, open_stores, prepare_rootfs, resolve_or_pull};

/// `spec` de `kind: Container` — espelha `ContainerCmd::Run` (menos `name`,
/// que vem de `metadata.name`). **`detach` default `true`** (diferente do CLI,
/// onde o default é `false`): um `apply`/`stack apply` corrido em primeiro
/// plano bloquearia à espera do processo terminar — perigoso para um comando
/// declarativo. Passa `detach: false` explicitamente no YAML se quiseres o
/// comportamento síncrono do `run` interactivo.
#[derive(Debug, Deserialize)]
struct ContainerSpec {
    image: String,
    #[serde(default = "default_true")]
    detach: bool,
    #[serde(default = "default_net")]
    network: String,
    #[serde(default)]
    volumes: Vec<String>,
    #[serde(default)]
    ports: Vec<String>,
    #[serde(default)]
    privileged: bool,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    command: Vec<String>,
}

fn default_true() -> bool {
    true
}
fn default_net() -> String {
    "host".to_string()
}

#[derive(Subcommand)]
pub enum ContainerCmd {
    /// Corre um container a partir de uma imagem (puxa se faltar).
    Run {
        /// Corre em segundo plano e imprime o ID.
        #[arg(short, long)]
        detach: bool,
        /// Nome do container (default: `dlx-<id>`).
        #[arg(long)]
        name: Option<String>,
        /// Rede: `host` (partilha a do host, default), `none` (netns isolado sem
        /// ligação), ou o NOME de uma rede criada com `delonix network create`.
        #[arg(long, default_value = "host")]
        net: String,
        /// Volume/bind mount, `nome:/destino[:ro]` ou `/host:/destino[:ro]`. Repetível.
        #[arg(short = 'v', long = "volume")]
        volumes: Vec<String>,
        /// Publica uma porta, `hostPort:contPort[/tcp|udp]` ou só `porta`. Repetível.
        /// Com `--net host` (o default) muda o container para um netns próprio com
        /// NAT em userspace (slirp4netns, como o podman rootless); com `--net
        /// <rede>` publica pelo ingress (DNAT nft + hostfwd no slirp único).
        #[arg(short = 'p', long = "publish")]
        publish: Vec<String>,
        /// Container privilegiado (todas as caps, seccomp off) — cargas de confiança.
        #[arg(long)]
        privileged: bool,
        /// Sobrepõe o ENTRYPOINT da imagem (o COMMAND passa a ser os argumentos
        /// deste binário; `--entrypoint ""` limpa-o e corre só o COMMAND).
        #[arg(long)]
        entrypoint: Option<String>,
        /// Remove o container quando o processo terminar (em `-d`, um watcher
        /// destacado trata da remoção quando o container morrer).
        #[arg(long)]
        rm: bool,
        /// Variáveis de ambiente adicionais (`KEY=VAL`), repetível.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Label (`KEY=VAL`), repetível — ex.: `io.x-k8s.kind.role=control-plane`
        /// activa a delegação de cgroup2 dedicada a nodes Kind (ver `setup_node_cgroup_ns`).
        #[arg(long = "label")]
        labels: Vec<String>,
        /// Imagem (ex.: `alpine:3.19`).
        image: String,
        /// Comando + argumentos (default: o ENTRYPOINT/CMD da imagem).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Lista containers.
    #[command(visible_alias = "ls")]
    Ps {
        /// Inclui os parados/falhados.
        #[arg(short, long)]
        all: bool,
        /// Só imprime os IDs (para compor com `stop`/`rm`).
        #[arg(short, long)]
        quiet: bool,
    },
    /// (Re)arranca containers parados/crashados, reutilizando o rootfs
    /// persistente (as escritas feitas dentro do container sobrevivem, como no
    /// docker) e a mesma rede/portas/volumes do `run` original. Sempre detached.
    Start {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Pára um ou mais containers (SIGTERM, depois SIGKILL).
    Stop {
        #[arg(required = true)]
        ids: Vec<String>,
        /// Segundos até ao SIGKILL.
        #[arg(short, long, default_value_t = 10)]
        time: u64,
    },
    /// Remove um ou mais containers.
    Rm {
        #[arg(required = true)]
        ids: Vec<String>,
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
    /// Mostra a spec completa de um ou mais containers (JSON do Store).
    Inspect {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Uso de recursos (CPU/memória/PIDs) dos containers a correr — uma
    /// amostra e sai (sem stream). Sem IDs, mostra todos os que correm.
    Stats { ids: Vec<String> },
    /// Mostra os logs (containers detached).
    Logs {
        id: String,
        /// Segue o log em contínuo (sai quando o container parar).
        #[arg(short, long)]
        follow: bool,
    },
    /// Aplica os documentos `kind: Container` de um manifesto (idempotente por
    /// nome — um container já existente com esse nome não é recriado nem
    /// verificado quanto a drift de spec, ver `cmd::manifest`).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

pub fn run(action: ContainerCmd) -> Result<()> {
    let (images, store) = open_stores()?;
    match action {
        ContainerCmd::Run { detach, name, net, volumes, publish, privileged, entrypoint, rm, env, labels, image, command } => {
            cmd_run(&images, &store, RunOpts { detach, name, net, volumes, ports: publish, privileged, entrypoint, rm, env, labels, image, command })
        }
        ContainerCmd::Ps { all, quiet } => cmd_ps(&store, all, quiet),
        ContainerCmd::Start { ids } => for_each_id(&ids, |id| cmd_start(&images, &store, id)),
        ContainerCmd::Stop { ids, time } => for_each_id(&ids, |id| cmd_stop(&store, id, time)),
        ContainerCmd::Rm { ids, force } => for_each_id(&ids, |id| cmd_rm(&images, &store, id, force)),
        ContainerCmd::Exec { interactive, tty, id, command } => cmd_exec(&store, &id, interactive, tty, &command),
        ContainerCmd::Inspect { ids } => cmd_inspect(&store, &ids),
        ContainerCmd::Stats { ids } => cmd_stats(&store, &ids),
        ContainerCmd::Logs { id, follow } => cmd_logs(&images, &store, &id, follow),
        ContainerCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
    }
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (images, store) = open_stores()?;
    for doc in manifest::of_kind(docs, "Container") {
        let name = &doc.metadata.name;
        if store.list()?.iter().any(|c| &c.name == name) {
            println!("container/{name}: já existe, nada a fazer");
            continue;
        }
        let spec: ContainerSpec = manifest::spec_of(doc)?;
        cmd_run(
            &images,
            &store,
            RunOpts {
                detach: spec.detach,
                name: Some(name.clone()),
                net: spec.network,
                volumes: spec.volumes,
                ports: spec.ports,
                privileged: spec.privileged,
                entrypoint: None,
                rm: false,
                env: spec.env,
                labels: Vec::new(),
                image: spec.image,
                command: spec.command,
            },
        )?;
        println!("container/{name}: criado");
    }
    Ok(())
}

/// Resolve os mounts de `-v` (o CLI nunca constrói `Mount` à mão — delega no
/// `VolumeStore`, que já sabe distinguir volume nomeado vs bind mount vs `:ro`).
fn resolve_mounts(volumes: &[String]) -> Result<Vec<delonix_runtime_core::Mount>> {
    if volumes.is_empty() {
        return Ok(Vec::new());
    }
    let vstore = VolumeStore::open(super::util::state_root())?;
    volumes.iter().map(|spec| vstore.resolve_spec(spec)).collect()
}

/// Argumentos do `container run` (CLI e manifesto), agrupados — a lista já
/// passou há muito o limiar do `too_many_arguments`.
struct RunOpts {
    detach: bool,
    name: Option<String>,
    net: String,
    volumes: Vec<String>,
    ports: Vec<String>,
    privileged: bool,
    entrypoint: Option<String>,
    rm: bool,
    env: Vec<String>,
    labels: Vec<String>,
    image: String,
    command: Vec<String>,
}

fn cmd_run(images: &ImageStore, store: &Store, opts: RunOpts) -> Result<()> {
    let RunOpts { detach, name, net, volumes, ports, privileged, entrypoint, rm, env, labels, image, command } = opts;
    // Valida os `-p` ANTES de criar o que quer que seja (erro claro, sem lixo).
    for spec in &ports {
        delonix_net::parse_publish(spec)?;
    }
    if net == "none" && !ports.is_empty() {
        return Err(Error::Invalid("-p/--publish não é compatível com --net none (netns sem ligação)".into()));
    }
    let mounts = resolve_mounts(&volumes)?;
    let img = resolve_or_pull(images, &image)?;
    let id = generate_id();
    let rootless = runtime::is_rootless();
    let rootfs = prepare_rootfs(images, &img, &id)?;

    // `--entrypoint X` substitui o ENTRYPOINT da imagem (o COMMAND vira os seus
    // argumentos, sem herdar o CMD da imagem — semântica docker); `--entrypoint ""`
    // limpa-o e corre só o COMMAND do utilizador.
    let cmd = match entrypoint.as_deref() {
        Some("") => command.clone(),
        Some(e) => {
            let mut v = vec![e.to_string()];
            v.extend(command.iter().cloned());
            v
        }
        None => effective_command(&img, &command),
    };
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
    for l in &labels {
        if let Some((k, v)) = l.split_once('=') {
            c.labels.insert(k.to_string(), v.to_string());
        }
    }

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

    // `--net`: host (default, sem netns próprio) | none (netns isolado, sem
    // ligação) | <nome> (junta-se à netns NOMEADA que o holder cria em
    // `infra::attach_container` — este cria a netns via `ip netns add` do LADO
    // do holder, independente do processo do container; por isso o container
    // tem de se JUNTAR a ela via `RunSpec.join_netns`, não criar a sua própria
    // com `new_netns` — essa era a abordagem errada, tentada e corrigida aqui).
    let custom_net = if net != "host" && net != "none" { Some(net.to_string()) } else { None };
    let mut join_netns = None;
    let mut attached_ip = None;
    if let Some(n) = &custom_net {
        delonix_net::NetworkStore::open(super::util::state_root())?.get(n)?;
        let (netns, ip) = infra::attach_container(&id, n)?;
        join_netns = Some(format!("/run/netns/{netns}"));
        attached_ip = Some(ip);
    }
    c.ports = ports.clone();

    // `-p` com rede custom: publica pelo INGRESS (hostfwd no slirp único + DNAT
    // nft), ANTES do arranque — as regras apontam para o IP atribuído, que já é
    // conhecido; também é este o caminho que permite (un)publish a quente com o
    // container a correr. Limpeza no stop/rm (`unpublish_ports`).
    if let Some(ip) = &attached_ip {
        for spec in &ports {
            if let Err(e) = infra::publish_port(ip, spec) {
                unpublish_ports(&c);
                infra::detach_container(&id, ip);
                return Err(e);
            }
        }
    }

    // `-p` sem rede custom (`--net host`, o default): o container deixa de
    // partilhar a rede do host e ganha um netns próprio com slirp4netns + os
    // hostfwd pedidos — o comportamento do `docker run -p` (rede NAT por
    // omissão), no modelo rootless do podman. O slirp morre com o netns.
    let slirp_ports = if custom_net.is_none() { ports.clone() } else { Vec::new() };
    let slirp_hook = |pid: i32| -> Result<()> { delonix_net::slirp_attach(pid, &slirp_ports) };
    let spec = RunSpec {
        detach,
        new_netns: net == "none" || !slirp_ports.is_empty(),
        join_netns,
        userns: c.userns,
        log_path,
        mounts,
        on_started: if slirp_ports.is_empty() { None } else { Some(&slirp_hook) },
        ..Default::default()
    };
    runtime::create_with(store, &mut c, &rootfs, &spec)?;
    if let Some(n) = &custom_net {
        c.network = Some(n.clone());
        c.ip = attached_ip;
        let _ = store.save(&c);
    }
    if rm {
        if detach {
            spawn_rm_watcher(images, store, &c.id);
        } else {
            // foreground: o `create_with` só volta depois do waitpid — remove já.
            let c = find(store, &id)?;
            unpublish_ports(&c);
            runtime::remove(store, &c, true)?;
            let _ = images.unmount_rootfs(&c.id);
            return Ok(());
        }
    }
    if detach {
        println!("{id}");
    }
    Ok(())
}

/// `--rm` em modo detached: sem daemon, quem remove é um **watcher** próprio —
/// um processo destacado (setsid, stdio em /dev/null) que sonda o estado do
/// container ~1x/s via `reconcile_status` e, quando ele deixar de correr, faz a
/// mesma limpeza do `rm -f`. Morre a seguir; um watcher por container `--rm`.
fn spawn_rm_watcher(images: &ImageStore, store: &Store, id: &str) {
    // SAFETY: fork de um processo single-threaded (CLI); o filho só sonda e sai.
    if unsafe { libc::fork() } == 0 {
        unsafe {
            libc::setsid();
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            if null >= 0 {
                libc::dup2(null, 0);
                libc::dup2(null, 1);
                libc::dup2(null, 2);
                if null > 2 {
                    libc::close(null);
                }
            }
        }
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let Ok(mut c) = find(store, id) else { std::process::exit(0) };
            let _ = runtime::reconcile_status(&mut c);
            if !matches!(c.status, delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused) {
                unpublish_ports(&c);
                let _ = runtime::remove(store, &c, true);
                let _ = images.unmount_rootfs(&c.id);
                std::process::exit(0);
            }
        }
    }
}

fn cmd_ps(store: &Store, all: bool, quiet: bool) -> Result<()> {
    let mut cs = store.list()?;
    if !quiet {
        println!("{:<14}  {:<20}  {:<24}  STATUS", "CONTAINER ID", "NAME", "IMAGE");
    }
    for c in cs.iter_mut() {
        if runtime::reconcile_status(c) {
            let _ = store.save(c);
        }
        let hidden = matches!(c.status, delonix_runtime_core::Status::Failed(_) | delonix_runtime_core::Status::Crashed);
        if !all && hidden {
            continue;
        }
        if quiet {
            println!("{}", &c.id[..12.min(c.id.len())]);
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

/// Aplica `f` a cada ID, continuando nos restantes se um falhar (semântica
/// docker: `rm a b c` remove o que conseguir e devolve o primeiro erro no fim).
fn for_each_id(ids: &[String], mut f: impl FnMut(&str) -> Result<()>) -> Result<()> {
    let mut first_err = None;
    for id in ids {
        if let Err(e) = f(id) {
            eprintln!("{id}: {e}");
            first_err.get_or_insert(e);
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Remove as publicações de ingress de um container (best-effort, idempotente).
/// Só o caminho de rede custom deixa regras persistentes (hostfwd no slirp único
/// + DNAT no holder); no caminho slirp-por-container o processo slirp morre com
/// o netns do container, não há nada para limpar.
fn unpublish_ports(c: &Container) {
    if c.network.is_none() {
        return;
    }
    for spec in &c.ports {
        if let Ok((host_port, _, _)) = delonix_net::parse_publish(spec) {
            infra::unpublish_port(&host_port);
        }
    }
}

/// `container start` — rearranca um container parado/crashado com a spec
/// guardada no `Store` (comando/env/mounts/rede/portas) e o rootfs PERSISTENTE
/// (rootless: a cópia flat em `containers/<id>/rootfs`; root: remonta o overlay,
/// cujo `upper` preserva as escritas). É o que falta ao `rm`+`run`: não perde o
/// estado escrito dentro do container.
fn cmd_start(images: &ImageStore, store: &Store, id: &str) -> Result<()> {
    let mut c = find(store, id)?;
    if runtime::reconcile_status(&mut c) {
        let _ = store.save(&c);
    }
    if matches!(c.status, delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused) {
        return Err(Error::Invalid(format!("{} já está a correr", c.name)));
    }

    let rootfs = if runtime::is_rootless() {
        let rfs = images.root().join("containers").join(&c.id).join("rootfs");
        if !rfs.exists() {
            return Err(Error::Invalid(format!("rootfs de {} já não existe — usa `run` de novo", c.name)));
        }
        rfs.to_string_lossy().into_owned()
    } else {
        let img = resolve_or_pull(images, &c.image)?;
        images.mount_rootfs(&img, &c.id)?.to_string_lossy().into_owned()
    };

    // Reconstrói a rede exactamente como o `run` original (ver `cmd_run`):
    // rede custom → attach + publish pelo ingress; `-p` sem rede → netns novo
    // com slirp4netns + hostfwd; sem nada → rede do host.
    let mut join_netns = None;
    if let Some(n) = c.network.clone() {
        let (netns, ip) = infra::attach_container(&c.id, &n)?;
        join_netns = Some(format!("/run/netns/{netns}"));
        for spec in &c.ports {
            if let Err(e) = infra::publish_port(&ip, spec) {
                unpublish_ports(&c);
                infra::detach_container(&c.id, &ip);
                return Err(e);
            }
        }
        c.ip = Some(ip);
    }
    let slirp_ports = if c.network.is_none() { c.ports.clone() } else { Vec::new() };
    let slirp_hook = |pid: i32| -> Result<()> { delonix_net::slirp_attach(pid, &slirp_ports) };

    let log_path = images
        .root()
        .join("containers")
        .join(&c.id)
        .join("log")
        .to_string_lossy()
        .into_owned();
    let spec = RunSpec {
        detach: true,
        new_netns: !slirp_ports.is_empty(),
        join_netns,
        userns: c.userns,
        log_path: Some(log_path),
        mounts: c.mounts.clone(),
        on_started: if slirp_ports.is_empty() { None } else { Some(&slirp_hook) },
        ..Default::default()
    };
    runtime::create_with(store, &mut c, &rootfs, &spec)?;
    println!("{}", c.id);
    Ok(())
}

fn cmd_stop(store: &Store, id: &str, time: u64) -> Result<()> {
    let mut c = find(store, id)?;
    runtime::stop(store, &mut c, time)?;
    unpublish_ports(&c);
    println!("{}", c.id);
    Ok(())
}

fn cmd_rm(images: &ImageStore, store: &Store, id: &str, force: bool) -> Result<()> {
    let c = find(store, id)?;
    runtime::remove(store, &c, force)?;
    unpublish_ports(&c);
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

/// `container inspect` — despeja a spec completa guardada no Store (a fonte de
/// verdade do runtime), como array JSON à docker.
fn cmd_inspect(store: &Store, ids: &[String]) -> Result<()> {
    let mut cs = Vec::new();
    for id in ids {
        let mut c = find(store, id)?;
        if runtime::reconcile_status(&mut c) {
            let _ = store.save(&c);
        }
        cs.push(c);
    }
    println!("{}", serde_json::to_string_pretty(&cs).map_err(|e| Error::Invalid(e.to_string()))?);
    Ok(())
}

/// Lê a métrica `file` do cgroup v2 do processo `pid` (via `/proc/<pid>/cgroup`
/// — funciona qualquer que seja a base delegada onde o motor pôs o container).
fn cgroup_metric(pid: i32, file: &str) -> Option<String> {
    let rel = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("0::").map(str::to_string))?;
    std::fs::read_to_string(format!("/sys/fs/cgroup{}/{file}", rel.trim())).ok()
}

/// `cpu.stat` → `usage_usec` (None se o controlador cpu não estiver delegado).
fn cpu_usage_usec(pid: i32) -> Option<u64> {
    cgroup_metric(pid, "cpu.stat")?
        .lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
}

/// `container stats` — uma amostra de CPU/mem/PIDs por container a correr.
/// CPU% = delta de `usage_usec` em 500ms; memória de `memory.current`; com o
/// cgroup não-delegado (rootless sem Delegate), cai para o VmRSS do init do
/// container em `/proc` (só esse processo, marcado com `~`).
fn cmd_stats(store: &Store, ids: &[String]) -> Result<()> {
    let mut cs: Vec<Container> = if ids.is_empty() {
        store.list()?
    } else {
        ids.iter().map(|i| find(store, i)).collect::<Result<_>>()?
    };
    let mut rows = Vec::new();
    for c in cs.iter_mut() {
        if runtime::reconcile_status(c) {
            let _ = store.save(c);
        }
        if !matches!(c.status, delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused) {
            continue;
        }
        let Some(pid) = c.pid else { continue };
        rows.push((c.name.clone(), pid, cpu_usage_usec(pid)));
    }
    if rows.is_empty() {
        println!("(nenhum container a correr)");
        return Ok(());
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    println!("{:<20}  {:>6}  {:>12}  {:>6}", "NAME", "CPU%", "MEM", "PIDS");
    for (name, pid, cpu0) in rows {
        let cpu = match (cpu0, cpu_usage_usec(pid)) {
            (Some(a), Some(b)) => format!("{:.1}", (b.saturating_sub(a)) as f64 / 500_000.0 * 100.0),
            _ => "-".into(),
        };
        let (mem, approx) = match cgroup_metric(pid, "memory.current").and_then(|v| v.trim().parse::<u64>().ok()) {
            Some(b) => (b, false),
            None => (
                std::fs::read_to_string(format!("/proc/{pid}/status"))
                    .ok()
                    .and_then(|s| {
                        s.lines()
                            .find_map(|l| l.strip_prefix("VmRSS:"))
                            .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<u64>().ok())
                    })
                    .map(|kb| kb * 1024)
                    .unwrap_or(0),
                true,
            ),
        };
        let pids = cgroup_metric(pid, "pids.current").map(|v| v.trim().to_string()).unwrap_or_else(|| "-".into());
        let mem_h = if mem >= 1 << 30 {
            format!("{:.2} GiB", mem as f64 / (1u64 << 30) as f64)
        } else {
            format!("{:.1} MiB", mem as f64 / (1u64 << 20) as f64)
        };
        println!("{:<20}  {:>6}  {:>12}  {:>6}", name, cpu, if approx { format!("~{mem_h}") } else { mem_h }, pids);
    }
    Ok(())
}

fn cmd_logs(images: &ImageStore, store: &Store, id: &str, follow: bool) -> Result<()> {
    use std::io::{Read, Seek, Write};
    let c = find(store, id)?;
    let p = images.root().join("containers").join(&c.id).join("log");
    let mut f = std::fs::File::open(&p).map_err(|_| {
        Error::Invalid(format!("sem logs para {} (só há logs em containers detached)", c.name))
    })?;
    let mut out = std::io::stdout();
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    out.write_all(&buf)?;
    if !follow {
        return Ok(());
    }
    // `-f`: segue os appends (reabre se o ficheiro encolher — rotação do shim);
    // termina quando o container deixar de correr e não houver mais nada a ler.
    let mut pos = f.stream_position()?;
    loop {
        out.flush().ok();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        if len < pos {
            f = std::fs::File::open(&p)?;
            pos = 0;
        }
        if len > pos {
            f.seek(std::io::SeekFrom::Start(pos))?;
            buf.clear();
            f.read_to_end(&mut buf)?;
            pos += buf.len() as u64;
            out.write_all(&buf)?;
            continue;
        }
        let mut c = find(store, id)?;
        let _ = runtime::reconcile_status(&mut c);
        if !matches!(c.status, delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::util::compose_command;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn user_args_replace_cmd_but_keep_entrypoint() {
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
