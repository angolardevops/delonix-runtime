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
    /// Mostra os logs (containers detached).
    Logs { id: String },
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
        ContainerCmd::Run { detach, name, net, volumes, publish, privileged, env, labels, image, command } => {
            cmd_run(&images, &store, detach, name, &net, volumes, publish, privileged, env, labels, image, command)
        }
        ContainerCmd::Ps { all, quiet } => cmd_ps(&store, all, quiet),
        ContainerCmd::Start { ids } => for_each_id(&ids, |id| cmd_start(&images, &store, id)),
        ContainerCmd::Stop { ids, time } => for_each_id(&ids, |id| cmd_stop(&store, id, time)),
        ContainerCmd::Rm { ids, force } => for_each_id(&ids, |id| cmd_rm(&images, &store, id, force)),
        ContainerCmd::Exec { interactive, tty, id, command } => cmd_exec(&store, &id, interactive, tty, &command),
        ContainerCmd::Logs { id } => cmd_logs(&images, &store, &id),
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
            spec.detach,
            Some(name.clone()),
            &spec.network,
            spec.volumes,
            spec.ports,
            spec.privileged,
            spec.env,
            Vec::new(),
            spec.image,
            spec.command,
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

#[allow(clippy::too_many_arguments)]
fn cmd_run(
    images: &ImageStore,
    store: &Store,
    detach: bool,
    name: Option<String>,
    net: &str,
    volumes: Vec<String>,
    ports: Vec<String>,
    privileged: bool,
    env: Vec<String>,
    labels: Vec<String>,
    image: String,
    command: Vec<String>,
) -> Result<()> {
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
    if detach {
        println!("{id}");
    }
    Ok(())
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
