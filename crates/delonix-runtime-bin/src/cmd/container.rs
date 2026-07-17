//! `delonix container` — ciclo de vida de containers (run/ps/stop/rm/exec/logs).

use std::path::PathBuf;

use clap::Subcommand;
use delonix_image::ImageStore;
use delonix_net::infra;
use delonix_runtime::{self as runtime, RunSpec};
use delonix_runtime_core::{generate_id, Container, Error, Result, Status, Store};
use delonix_volume::VolumeStore;
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::{effective_command, find, open_stores, prepare_rootfs, resolve_or_pull};

/// `spec` de `kind: Container` — espelha `ContainerCmd::Run` (menos `name`,
/// que vem de `metadata.name`). **`detach` default `true`** (diferente do CLI,
/// onde o default é `false`): um `apply`/`stack apply` corrido em primeiro
/// plano bloquearia à espera do processo terminar — perigoso para um comando
/// declarativo. Passa `detach: false` explicitamente no YAML se quiseres o
/// comportamento síncrono do `run` interactivo.
#[derive(Debug, Deserialize)]
struct ContainerSpec {
    pub(crate) image: String,
    #[serde(default = "default_true")]
    pub(crate) detach: bool,
    #[serde(default = "default_net")]
    network: String,
    #[serde(default)]
    pub(crate) volumes: Vec<String>,
    #[serde(default)]
    pub(crate) ports: Vec<String>,
    #[serde(default)]
    pub(crate) privileged: bool,
    #[serde(default)]
    pub(crate) env: Vec<String>,
    #[serde(default)]
    pub(crate) command: Vec<String>,
    /// `no` (default) | `on-failure[:max]` | `always` | `unless-stopped` —
    /// um supervisor destacado fica pai do container e reinicia-o (ver
    /// `run_supervised`). É o que torna um manifesto resiliente.
    #[serde(default = "default_restart")]
    pub(crate) restart: String,
}

fn default_restart() -> String {
    "no".to_string()
}

fn default_true() -> bool {
    true
}
fn default_net() -> String {
    "host".to_string()
}

#[derive(Subcommand)]
pub enum ContainerCmd {
    /// Inicializa um projecto com Delonixfile + manifesto — ficheiros JÁ PREENCHIDOS (imagens
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
        /// Política de reinício (só com `-d`): `no` (default), `on-failure[:max]`,
        /// `always`, `unless-stopped`. Um supervisor destacado (um por container,
        /// efémero — não há daemon) fica pai do container, captura o exit code
        /// real e reinicia-o conforme a política.
        #[arg(long, default_value = "no")]
        restart: String,
        /// Liga um device do host, `/dev/x[:/dev/y]`. Repetível. O `/dev` do
        /// container é um tmpfs com uma lista curada (null/zero/tty/...); isto
        /// acrescenta-lhe nós reais do host, como o `docker --device`.
        #[arg(long = "device")]
        devices: Vec<String>,
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
    /// Detalhe legível de um ou mais containers, ao estilo `kubectl describe`
    /// (para humanos; use `inspect` para JSON consumível por scripts).
    Describe {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// **Reconfigura um container A CORRER, sem o parar** — portas, volumes,
    /// redes e limite de banda.
    ///
    /// Ao contrário do docker (onde mudar uma porta ou um volume obriga a
    /// recriar o container), aqui o dataplane não pertence ao ciclo de vida do
    /// processo: as portas são DNAT/hostfwd à frente da rede e os volumes
    /// entram pela mount API do kernel (`open_tree`/`move_mount`) no mount
    /// namespace do container já vivo. O PID não muda e o processo nunca é
    /// interrompido.
    ///
    /// As mudanças ficam persistidas no registo, logo um `container start`
    /// posterior reproduz a configuração nova, não a original.
    Update {
        id: String,
        /// Publica mais uma porta a quente, `hostPort:contPort[/tcp|udp]`. Repetível.
        #[arg(short = 'p', long = "publish-add", value_name = "SPEC")]
        publish_add: Vec<String>,
        /// Despublica uma porta a quente, pela PORTA DE HOST. Repetível.
        #[arg(long = "publish-rm", value_name = "HOST_PORT")]
        publish_rm: Vec<String>,
        /// Monta um volume a quente, `nome:/destino[:ro]` ou `/host:/destino[:ro]`. Repetível.
        #[arg(short = 'v', long = "volume-add", value_name = "SPEC")]
        volume_add: Vec<String>,
        /// Desmonta a quente, pelo caminho de DESTINO dentro do container. Repetível.
        #[arg(long = "volume-rm", value_name = "TARGET")]
        volume_rm: Vec<String>,
        /// Liga o container a uma rede adicional a quente (multi-homing). Repetível.
        #[arg(long = "net-connect", value_name = "REDE")]
        net_connect: Vec<String>,
        /// Desliga o container de uma rede adicional. Repetível.
        #[arg(long = "net-disconnect", value_name = "REDE")]
        net_disconnect: Vec<String>,
        /// Limite de banda, em bit/s com sufixo (`10mbit`, `512kbit`, `1gbit`).
        #[arg(long = "net-rate", value_name = "RATE")]
        net_rate: Option<String>,
        /// Burst do limite de banda (default: `32kb`). Só com `--net-rate`.
        #[arg(long = "net-burst", value_name = "BURST")]
        net_burst: Option<String>,
        /// Remove o limite de banda.
        #[arg(long = "net-rate-clear", conflicts_with = "net_rate")]
        net_rate_clear: bool,
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
    if let ContainerCmd::Init { dir, name, image, force } = action {
        return cmd_init(super::scaffold::Target::Container, dir, name, image, force);
    }
    let (images, store) = open_stores()?;
    match action {
        // Tratado no topo de `run` (faz `return`).
        ContainerCmd::Init { .. } => unreachable!("tratado acima"),
        ContainerCmd::Run { detach, name, net, volumes, publish, privileged, entrypoint, rm, restart, devices, env, labels, image, command } => {
            cmd_run(&images, &store, RunOpts { detach, name, net, volumes, ports: publish, privileged, entrypoint, rm, restart, devices, env, labels, image, command })
        }
        ContainerCmd::Ps { all, quiet } => cmd_ps(&store, all, quiet),
        ContainerCmd::Start { ids } => for_each_id(&ids, |id| cmd_start(&images, &store, id)),
        ContainerCmd::Stop { ids, time } => for_each_id(&ids, |id| cmd_stop(&store, id, time)),
        ContainerCmd::Rm { ids, force } => for_each_id(&ids, |id| cmd_rm(&images, &store, id, force)),
        ContainerCmd::Exec { interactive, tty, id, command } => cmd_exec(&store, &id, interactive, tty, &command),
        ContainerCmd::Inspect { ids } => cmd_inspect(&store, &ids),
        ContainerCmd::Describe { ids } => cmd_describe(&store, &ids),
        ContainerCmd::Update { id, publish_add, publish_rm, volume_add, volume_rm, net_connect, net_disconnect, net_rate, net_burst, net_rate_clear } => cmd_update(
            &store,
            &id,
            UpdateOpts { publish_add, publish_rm, volume_add, volume_rm, net_connect, net_disconnect, net_rate, net_burst, net_rate_clear },
        ),
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
                restart: spec.restart.clone(),
                devices: Vec::new(),
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
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct RunOpts {
    pub(crate) detach: bool,
    pub(crate) name: Option<String>,
    pub(crate) net: String,
    pub(crate) volumes: Vec<String>,
    pub(crate) ports: Vec<String>,
    pub(crate) privileged: bool,
    pub(crate) entrypoint: Option<String>,
    pub(crate) rm: bool,
    pub(crate) restart: String,
    pub(crate) devices: Vec<String>,
    pub(crate) env: Vec<String>,
    pub(crate) labels: Vec<String>,
    pub(crate) image: String,
    pub(crate) command: Vec<String>,
}

pub(crate) fn cmd_run(images: &ImageStore, store: &Store, opts: RunOpts) -> Result<()> {
    // Cópia intacta para o re-exec (o destructuring a seguir consome as opts).
    let opts_copy = opts.clone();
    let RunOpts { detach, name, net, volumes, ports, privileged, entrypoint, rm, restart, devices, env, labels, image, command } = opts;
    // Valida os `-p` ANTES de criar o que quer que seja (erro claro, sem lixo).
    for spec in &ports {
        delonix_net::parse_publish(spec)?;
    }
    if net == "none" && !ports.is_empty() {
        return Err(Error::Invalid("-p/--publish não é compatível com --net none (netns sem ligação)".into()));
    }
    // Porta ocupada: falhar AQUI, com um erro que diz quem a tem e o que fazer.
    // Sem isto, a colisão só rebentava lá ao fundo, no slirp, e despejava JSON
    // cru (`add_hostfwd: slirp_add_hostfwd failed`) — o utilizador ficava sem
    // saber que era conflito de porta, nem com quem.
    // No 2.º passo do re-exec a porta já foi verificada (e o próprio container
    // ainda não está no store) — verificar aqui daria um falso conflito.
    if std::env::var("DELONIX_REEXEC_ID").is_err() {
        for spec in &ports {
            let (hp, _, _) = delonix_net::parse_publish(spec)?;
            if let Some(owner) = port_owner(store, &hp)? {
                return Err(Error::Invalid(format!(
                    "a porta {hp} já está publicada pelo container '{owner}' — usa outra porta \
                     (ex.: `-p {}:...`) ou pára esse container primeiro",
                    hp.parse::<u32>().map(|n| n + 10000).unwrap_or(0)
                )));
            }
        }
    }
    let mounts = resolve_mounts(&volumes)?;
    let img = resolve_or_pull(images, &image)?;
    // No 2.º passo do re-exec (ver `reexec_into_netns`) o id TEM de ser o mesmo:
    // a netns nomeada já foi criada com ele do lado do holder.
    let id = std::env::var("DELONIX_REEXEC_ID").unwrap_or_else(|_| generate_id());
    let reexec = std::env::var("DELONIX_REEXEC_ID").is_ok();
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
    // Nome ÚNICO, como no docker ("name is already in use"). Sem isto criavam-se
    // vários containers com o mesmo nome: o `find` resolve pelo primeiro, e um
    // `rm <nome>` só apanhava esse — os outros ficavam órfãos e invisíveis à
    // gestão por nome (visto a doer: 2x `loja-app` + 2x `loja-db`).
    if let Some(dup) = store.list()?.iter().find(|c| c.name == cname) {
        return Err(Error::Invalid(format!(
            "o nome '{cname}' já está em uso pelo container {} — escolhe outro ou remove-o primeiro",
            dup.short_id()
        )));
    }
    // `max` = sem teto de memória (cgroup v2); em k8s o cgroup do pod já limita.
    let mut c = Container::new(id.clone(), cname, image.clone(), cmd, "max".into());
    c.env = img.config.env.clone();
    c.env.extend(env);
    if !img.config.working_dir.is_empty() {
        c.workdir = Some(img.config.working_dir.clone());
    }
    c.devices = devices;
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
    let mut attached_ip = None;
    if let Some(n) = &custom_net {
        if reexec {
            // 2.º passo: já corremos DENTRO do userns+netns do holder (o `ip netns
            // exec` do `join_argv` pôs-nos lá). A netns já existe e já é nossa.
            attached_ip = std::env::var("DELONIX_REEXEC_IP").ok();
        } else {
            // 1.º passo: cria a netns do lado do holder e RE-EXECUTA-SE lá dentro.
            delonix_net::NetworkStore::open(super::util::state_root())?.get(n)?;
            let (netns, ip) = infra::attach_container(&id, n)?;
            return reexec_into_netns(&id, &netns, &ip, &opts_copy);
        }
    }
    c.ports = ports.clone();

    // `-p` com rede custom: publica pelo INGRESS (hostfwd no slirp único + DNAT
    // nft), ANTES do arranque — as regras apontam para o IP atribuído, que já é
    // conhecido; também é este o caminho que permite (un)publish a quente com o
    // container a correr. Limpeza no stop/rm (`unpublish_ports`).
    if let Some(ip) = &attached_ip {
        for spec in &ports {
            if let Err(e) = publish_with_retry(ip, spec) {
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
        // No re-exec já estamos no netns certo: NÃO criar outro (nem juntar-se a
        // nada — o `ip netns exec` tratou disso).
        new_netns: !reexec && (net == "none" || !slirp_ports.is_empty()),
        join_netns: None,
        userns: c.userns && !reexec,
        // Herda o user+network namespace do holder em vez de criar os seus.
        inherit_userns: reexec,
        log_path,
        mounts,
        on_started: if slirp_ports.is_empty() { None } else { Some(&slirp_hook) },
        // /etc/hosts: IP da rede custom, ou o do slirp quando `-p` sem rede.
        hosts_ip: attached_ip
            .clone()
            .or_else(|| (!slirp_ports.is_empty()).then(|| delonix_net::SLIRP_IP.to_string())),
        ..Default::default()
    };
    // ANTES do ramo supervisionado (que faz `return`): senão os containers com
    // `--restart` nunca emitiam `create`.
    delonix_runtime_core::events::emit(
        &super::util::state_root(), "container", "create", &c.id, &c.name, Some(&image),
    );
    // `--restart`: em vez de a CLI criar o container e sair (deixando-o órfão do
    // `init`, com o exit code perdido), um SUPERVISOR destacado cria-o e fica
    // seu pai — ver `run_supervised`.
    if detach && policy_supervised(&restart) {
        c.restart_policy = Some(restart.clone());
        return run_supervised(store, &mut c, &rootfs, &spec, &restart, &id);
    }
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

/// ID curto, como o do `docker ps` (12 chars).
pub(crate) fn short_id(id: &str) -> &str {
    &id[..12.min(id.len())]
}

/// Coluna STATUS ao estilo do `docker ps`: "Up 5 minutes", "Exited (0)".
/// `uptime` é o tempo desde o arranque do init (`None` se desconhecido — um
/// container parado não tem processo de onde o ler).
fn fmt_status(status: &Status, uptime: Option<u64>) -> String {
    let up = || match uptime {
        Some(s) => format!("Up {}", output::fmt_duration_secs(s)),
        // Running sem uptime legível: o registo é antigo (sem `pid_starttime`)
        // ou o /proc do init não é legível. Não inventamos uma duração.
        None => "Up".to_string(),
    };
    match status {
        Status::Created => "Created".to_string(),
        Status::Running => up(),
        Status::Paused => format!("{} (Paused)", up()),
        // Sem `finished_at` no `Container`, não há como dizer "há quanto tempo"
        // saiu — o docker mostraria "Exited (0) 2 minutes ago". Preferível
        // mostrar menos do que fabricar um tempo a partir do `created_unix`.
        Status::Stopped => "Exited (0)".to_string(),
        Status::Failed(code) => format!("Exited ({code})"),
        Status::Crashed => "Dead".to_string(),
    }
}

/// Coluna PORTS ao estilo do `docker ps`: `8080->80/tcp`, separadas por vírgula.
///
/// O docker prefixa o endereço do host (`0.0.0.0:8080->80/tcp`). Aqui não:
/// o endereço efectivo depende do caminho de publicação (slirp por container
/// vs DNAT do ingress) e de `DELONIX_PUBLISH_ADDR`, e imprimir um `0.0.0.0`
/// fixo seria uma afirmação de exposição que pode ser falsa — numa coluna que
/// se usa exactamente para decidir se algo está exposto.
fn fmt_ports(ports: &[String]) -> String {
    ports
        .iter()
        .map(|p| {
            let (spec, proto) = match p.split_once('/') {
                Some((s, pr)) => (s, pr),
                None => (p.as_str(), "tcp"),
            };
            match spec.split_once(':') {
                Some((hp, cp)) => format!("{hp}->{cp}/{proto}"),
                // Só a porta do container (publicada sem porta de host fixa).
                None => format!("{spec}/{proto}"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn cmd_ps(store: &Store, all: bool, quiet: bool) -> Result<()> {
    let mut cs = store.list()?;
    // Ordem estável e útil: o mais recente primeiro, como no `docker ps`.
    cs.sort_by(|a, b| b.created_unix.cmp(&a.created_unix));
    let mut t = output::Table::new(&["CONTAINER ID", "IMAGE", "COMMAND", "CREATED", "STATUS", "PORTS", "NAMES"]);
    for c in cs.iter_mut() {
        // `update` (flock) e não `save`: o CRI é concorrente e pode estar a
        // reconciliar o mesmo container agora — ver `Store::update`.
        if runtime::reconcile_status(c) {
            let _ = store.update(&c.id, |cur| runtime::reconcile_status(cur));
        }
        let hidden = matches!(c.status, Status::Failed(_) | Status::Crashed);
        if !all && hidden {
            continue;
        }
        if quiet {
            println!("{}", short_id(&c.id));
            continue;
        }
        let uptime = match c.status {
            Status::Running | Status::Paused => c.pid_starttime.and_then(output::uptime_from_starttime),
            _ => None,
        };
        t.row(vec![
            short_id(&c.id).to_string(),
            output::truncate(&c.image, 30),
            output::truncate(&format!("\"{}\"", c.command.join(" ")), 22),
            output::fmt_age(c.created_unix),
            fmt_status(&c.status, uptime),
            output::truncate(&fmt_ports(&c.ports), 28),
            c.name.clone(),
        ]);
    }
    if !quiet {
        t.print();
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

/// `--restart` em `-d`: cria o container dentro de um **supervisor destacado**
/// (um por container, efémero — continua a não haver daemon) e mantém a
/// política de reinício.
///
/// Porque tem de ser assim: `waitpid` só é permitido ao PAI. Num `run -d`
/// normal a CLI cria o container e sai — ele é reparentado ao `init` do host e o
/// exit code morre lá; o `reconcile_status` só consegue dizer "morreu"
/// (`Crashed`/137), nunca *porquê*, e `on-failure` não teria como decidir. Aqui
/// é o supervisor que chama `create_with`, portanto é ele o pai: apanha o
/// código real (`Failed(n)`) e reinicia conforme a política. É o mesmo papel do
/// `conmon` do podman, sem processo residente global.
///
/// O pai (a CLI) espera pelo primeiro arranque através de um pipe, para manter
/// a semântica do `run -d`: quando o comando volta, o container JÁ existe.
fn run_supervised(
    store: &Store,
    c: &mut Container,
    rootfs: &str,
    spec: &RunSpec<'_>,
    policy: &str,
    id: &str,
) -> Result<()> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe() preenche 2 fds; usados só para o handshake de arranque.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(Error::Runtime { context: "pipe", message: "handshake do supervisor".into() });
    }
    let (rd, wr) = (fds[0], fds[1]);

    // SAFETY: fork de um processo single-threaded (CLI).
    if unsafe { libc::fork() } == 0 {
        // ---- supervisor ----
        unsafe {
            libc::close(rd);
            libc::setsid(); // sobrevive ao fecho do terminal/CLI
        }
        let mut restarts: u32 = 0;
        let mut first = true;
        loop {
            let started = runtime::create_with(store, c, rootfs, spec);
            if first {
                // sinaliza o pai: 1 = arrancou, 0 = falhou (e o pai devolve erro)
                let b = [u8::from(started.is_ok())];
                // SAFETY: escreve 1 byte no write-end e fecha-o.
                unsafe {
                    libc::write(wr, b.as_ptr() as *const libc::c_void, 1);
                    libc::close(wr);
                    // Só AGORA larga o stdio: até aqui um erro do `create_with`
                    // ainda tem de chegar ao utilizador.
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
                first = false;
            }
            if started.is_err() {
                std::process::exit(1);
            }
            // Somos o PAI do container: isto captura o exit code REAL e grava-o.
            let status = match runtime::wait_and_record(store, c) {
                Ok(s) => s,
                Err(_) => std::process::exit(1),
            };
            // `die` com o exit code REAL — o supervisor e o unico que o sabe
            // (e pai do container); um `run -d` normal so veria "Crashed".
            delonix_runtime_core::events::emit(
                &super::util::state_root(), "container", "die", &c.id, &c.name,
                Some(&format!("exit={}", status.exit_code())),
            );
            if !should_restart(policy, &status, restarts) {
                std::process::exit(0);
            }
            // Estado desejado manda mais que a política: se o registo
            // desapareceu (`rm -f`) ou o utilizador pediu `stop`, não
            // ressuscitar — é a semântica do docker.
            match store.load(&c.id) {
                Err(_) => std::process::exit(0),
                Ok(cur) if cur.stopped_by_user => std::process::exit(0),
                Ok(_) => {}
            }
            restarts += 1;
            // A porta da encarnação anterior liberta-se sozinha no `stop`; se
            // ainda estiver presa, o `publish_with_retry` do restart limpa-a.
            // Backoff exponencial travado (1s→32s), como o docker: um container
            // que crasha em ciclo não pode queimar o nó.
            let backoff = std::cmp::min(1u64 << std::cmp::min(restarts, 5), 32);
            std::thread::sleep(std::time::Duration::from_secs(backoff));
        }
    }

    // ---- pai (CLI): espera o primeiro arranque ----
    // SAFETY: fecha o write-end e lê o byte de handshake do supervisor.
    unsafe { libc::close(wr) };
    let mut b = [0u8; 1];
    // SAFETY: lê 1 byte; 0 = EOF (supervisor morreu antes de sinalizar).
    let n = unsafe { libc::read(rd, b.as_mut_ptr() as *mut libc::c_void, 1) };
    unsafe { libc::close(rd) };
    if n != 1 || b[0] != 1 {
        return Err(Error::Runtime {
            context: "supervisor",
            message: "o container não arrancou (ver o erro acima)".into(),
        });
    }
    println!("{id}");
    Ok(())
}

/// Decide se um container deve ser reiniciado, dada a política, o estado com
/// que morreu e quantas vezes já foi reiniciado. Função **pura** — a máquina de
/// estados do restart testa-se sem clonar processos nenhuns.
///
/// Semântica docker: `no` nunca; `on-failure[:max]` só em saída ≠ 0 (ou sinal),
/// até `max` tentativas (sem `max` = sem limite); `always`/`unless-stopped`
/// sempre. A distinção real entre `always` e `unless-stopped` é o que acontece
/// ao **rearrancar o host** (o `unless-stopped` não ressuscita um container que
/// o utilizador parou) — sem um daemon a fazer boot-time reconcile, aqui os
/// dois comportam-se igual EM VIDA; documentado para não prometer o que não há.
fn should_restart(policy: &str, status: &delonix_runtime_core::Status, restarts: u32) -> bool {
    use delonix_runtime_core::Status as S;
    let failed = matches!(status, S::Failed(_) | S::Crashed);
    let (kind, max) = match policy.split_once(':') {
        Some((k, m)) => (k, m.parse::<u32>().ok()),
        None => (policy, None),
    };
    match kind {
        "always" | "unless-stopped" => true,
        "on-failure" => failed && max.map(|m| restarts < m).unwrap_or(true),
        _ => false, // "no" e qualquer coisa desconhecida: não reiniciar
    }
}

/// A política pede supervisão? (`no` não precisa de supervisor nenhum.)
fn policy_supervised(policy: &str) -> bool {
    matches!(policy.split(':').next().unwrap_or(""), "always" | "unless-stopped" | "on-failure")
}

/// **Fecha a limitação conhecida do `--net <rede>` em rootless.**
///
/// O problema: `infra::attach_container` cria a netns NOMEADA do lado do holder
/// (`ip netns add`, dentro do `unshare --user --map-auto --net --mount` dele).
/// O container tentava juntar-se por `setns("/run/netns/<x>")` e falhava sempre
/// com "netns do pod indisponível" — por DOIS motivos, não um:
///   1. `/run/netns/<x>` vive no **mount namespace do holder**: de fora nem o
///      caminho existe (o `open` falha antes de haver `setns`);
///   2. mesmo que existisse, a netns é **propriedade do userns do holder** — sem
///      privilégio nesse userns, o `setns` seria recusado.
/// Nenhum dos dois se resolve de dentro do `container_init`: é preciso ENTRAR no
/// userns+mountns do holder ANTES de existir container.
///
/// A solução (a que a doc do `delonix-net` já apontava, sem ninguém a ligar):
/// re-executar o próprio binário através do `infra::join_argv` —
/// `nsenter -t <holder> -U -m -n --preserve-credentials -- ip netns exec <netns>`
/// — e correr aí o MESMO comando. O 2.º passo já nasce dentro do userns+netns
/// certos, por isso não cria namespaces novos (`inherit_userns`).
///
/// O `DELONIX_REEXEC_ID` distingue os dois passos E carrega o id: sem ele o 2.º
/// passo geraria um id novo e a netns criada no 1.º ficaria órfã.
fn reexec_into_netns(id: &str, netns: &str, ip: &str, opts: &RunOpts) -> Result<()> {
    let prefix = infra::join_argv(id).ok_or_else(|| Error::Runtime {
        context: "join_argv",
        message: "infra de ingress em baixo — não há holder onde entrar".into(),
    })?;
    let exe = std::env::current_exe().map_err(|e| Error::Runtime {
        context: "current_exe",
        message: e.to_string(),
    })?;
    // A spec vai por FICHEIRO, não por `std::env::args()`. Reexecutar os
    // argumentos originais parecia mais simples e estava ERRADO: o `cmd_run`
    // também é chamado como biblioteca (o modo kind arranca nós assim), e aí os
    // args do processo são `cluster create ...` — o re-exec corria o `cluster
    // create` INTEIRO outra vez dentro da netns, recursivamente. Uma forma
    // interna explícita não depende de quem chamou.
    let spec_path = super::util::state_root().join(format!(".reexec-{id}.json"));
    let json = serde_json::to_string(opts).map_err(|e| Error::Invalid(e.to_string()))?;
    std::fs::write(&spec_path, json)?;
    let status = std::process::Command::new(&prefix[0])
        .args(&prefix[1..])
        .arg(&exe)
        .args(["netns", "run"])
        .arg(&spec_path)
        .env("DELONIX_REEXEC_ID", id)
        .env("DELONIX_REEXEC_IP", ip)
        .env("DELONIX_ROOT", super::util::state_root())
        .status();
    let _ = std::fs::remove_file(&spec_path);
    let status = status.map_err(|e| Error::Runtime { context: "re-exec nsenter", message: e.to_string() })?;
    if !status.success() {
        // A netns ficaria pendurada se o 2.º passo falhasse.
        infra::detach_container(id, ip);
        return Err(Error::Invalid(format!(
            "o container não arrancou dentro da rede '{netns}' (exit {:?})",
            status.code()
        )));
    }
    Ok(())
}

/// O 2.º passo do re-exec (`delonix netns run <spec.json>`, oculto — não é um
/// subcomando público). Corre JÁ dentro do userns+netns do holder.
pub(crate) fn run_from_spec(path: &std::path::Path) -> Result<()> {
    let json = std::fs::read_to_string(path)?;
    let opts: RunOpts = serde_json::from_str(&json).map_err(|e| Error::Invalid(e.to_string()))?;
    let (images, store) = open_stores()?;
    cmd_run(&images, &store, opts)
}

/// Que container VIVO está a publicar esta porta de host? `None` = livre.
/// (Só containers vivos contam: os mortos já não a seguram — e se algum
/// processo órfão a segurar, o `reap_orphan_net` limpa-o antes disto.)
pub(crate) fn port_owner(store: &Store, host_port: &str) -> Result<Option<String>> {
    for c in store.list()? {
        if !matches!(c.status, delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused) {
            continue;
        }
        for p in &c.ports {
            if let Ok((hp, _, _)) = delonix_net::parse_publish(p) {
                if hp == host_port {
                    return Ok(Some(c.name));
                }
            }
        }
    }
    Ok(None)
}

/// Reapa a rede deixada por containers que já morreram, ANTES de publicar
/// portas novas. Sem isto, um `slirp4netns` órfão (o container morreu sem
/// `stop` — crash, SIGKILL, sessão fechada) fica a segurar a porta de HOST e o
/// `run` seguinte falha com `add_hostfwd: slirp_add_hostfwd failed` — visto 3×
/// numa só sessão de testes, sempre com limpeza manual à mão.
///
/// Os dois reapers já existiam em `delonix-net` (`reap_orphan_slirp`,
/// `reap_orphan_hostfwds`) mas eram **código morto**: nenhum chamador em todo o
/// workspace. Barato (uma passagem por `/proc` + 1 query ao api-socket) e
/// seguro (só mexe em slirps cujo pid-alvo já não existe e em hostfwds sem
/// container vivo).
/// Publica uma porta; se falhar por a porta estar presa por um **processo
/// órfão** (o container morreu sem `stop` e o slirp ficou a segurá-la), limpa
/// SÓ essa e tenta outra vez.
///
/// Porquê assim e não a varrer tudo antes: o reaper preventivo corria a CADA
/// `run` com portas e apagava por omissão — bastava a lista de containers vir
/// vazia (um erro de leitura, ou uma vista do store sem os registos) para
/// `live_ports` ficar vazio e ele concluir que NADA está em uso, apagando os
/// hostfwds de containers VIVOS. Foi isso que pôs o apiserver de um cluster
/// `Ready` inalcançável e fez dois containers com `-p` nunca coexistirem.
/// Aqui a limpeza é REACTIVA e cirúrgica: só acontece quando a porta que
/// queremos falha, e só toca nessa. Sem conflito, não se apaga nada — e um
/// erro de leitura do estado deixa de poder destruir o que está a funcionar.
fn publish_with_retry(ip: &str, spec: &str) -> Result<()> {
    match infra::publish_port(ip, spec) {
        Ok(()) => Ok(()),
        Err(e) => {
            let (hp, _, _) = delonix_net::parse_publish(spec)?;
            // Órfãos primeiro (slirp de container morto ainda a segurar a porta),
            // depois o hostfwd dessa porta em concreto.
            let _ = delonix_net::reap_orphan_slirp();
            infra::unpublish_port(&hp);
            infra::publish_port(ip, spec).map_err(|_| e)
        }
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
        c = store.update(&c.id, |cur| runtime::reconcile_status(cur)).unwrap_or(c);
    }
    // `start` reafirma o estado desejado = a correr (limpa o `stop` do utilizador).
    let _ = store.update(&c.id, |cur| {
        cur.stopped_by_user = false;
        true
    });
    c.stopped_by_user = false;
    if matches!(c.status, delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused) {
        return Err(Error::Invalid(format!("{} já está a correr", c.name)));
    }

    // Rede custom: o MESMO re-exec de dois passos do `cmd_run` (ver
    // `reexec_into_netns`). Ficou esquecido no caminho antigo do `join_netns` —
    // que nunca funcionou em rootless — e um `start` de um container com rede
    // rebentava com `clone failed: EPERM`. Corrigir só o `run` não chegava: o
    // `start` cria o container tal como o `run`, e tem exactamente o mesmo
    // problema de namespaces.
    let reexec = std::env::var("DELONIX_REEXEC_ID").is_ok();
    if let Some(n) = c.network.clone() {
        if !reexec {
            let (netns, ip) = infra::attach_container(&c.id, &n)?;
            return reexec_start(&c.id, &netns, &ip);
        }
        c.ip = std::env::var("DELONIX_REEXEC_IP").ok();
        if let Some(ip) = c.ip.clone() {
            for spec in &c.ports {
                if let Err(e) = infra::publish_port(&ip, spec) {
                    unpublish_ports(&c);
                    infra::detach_container(&c.id, &ip);
                    return Err(e);
                }
            }
        }
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
        new_netns: !reexec && !slirp_ports.is_empty(),
        join_netns: None,
        userns: c.userns && !reexec,
        inherit_userns: reexec,
        log_path: Some(log_path),
        mounts: c.mounts.clone(),
        on_started: if slirp_ports.is_empty() { None } else { Some(&slirp_hook) },
        hosts_ip: c
            .ip
            .clone()
            .or_else(|| (!slirp_ports.is_empty()).then(|| delonix_net::SLIRP_IP.to_string())),
        ..Default::default()
    };
    runtime::create_with(store, &mut c, &rootfs, &spec)?;
    delonix_runtime_core::events::emit(
        &super::util::state_root(), "container", "start", &c.id, &c.name, None,
    );
    println!("{}", c.id);
    Ok(())
}

/// O 1.º passo do `start` com rede custom: re-executa-se dentro do netns (ver
/// `reexec_into_netns`, mesmo mecanismo, sem spec — o container já existe no
/// store, basta o id).
fn reexec_start(id: &str, netns: &str, ip: &str) -> Result<()> {
    let prefix = infra::join_argv(id).ok_or_else(|| Error::Runtime {
        context: "join_argv",
        message: "infra de ingress em baixo — não há holder onde entrar".into(),
    })?;
    let exe = std::env::current_exe().map_err(|e| Error::Runtime {
        context: "current_exe",
        message: e.to_string(),
    })?;
    let status = std::process::Command::new(&prefix[0])
        .args(&prefix[1..])
        .arg(&exe)
        .args(["container", "start", id])
        .env("DELONIX_REEXEC_ID", id)
        .env("DELONIX_REEXEC_IP", ip)
        .env("DELONIX_ROOT", super::util::state_root())
        .status()
        .map_err(|e| Error::Runtime { context: "re-exec nsenter", message: e.to_string() })?;
    if !status.success() {
        infra::detach_container(id, ip);
        return Err(Error::Invalid(format!(
            "o container não rearrancou dentro da rede '{netns}' (exit {:?})",
            status.code()
        )));
    }
    Ok(())
}

fn cmd_stop(store: &Store, id: &str, time: u64) -> Result<()> {
    let mut c = find(store, id)?;
    // ANTES de parar: marca o estado desejado, senão o supervisor de
    // `--restart always` ressuscita-o e o utilizador não o consegue parar
    // (medido: 6 encarnações depois de um `stop`). Ver `Container::stopped_by_user`.
    let _ = store.update(&c.id, |cur| {
        cur.stopped_by_user = true;
        true
    });
    runtime::stop(store, &mut c, time)?;
    unpublish_ports(&c);
    delonix_runtime_core::events::emit(
        &super::util::state_root(), "container", "stop", &c.id, &c.name, None,
    );
    println!("{}", c.id);
    Ok(())
}

/// Remove um container JÁ resolvido (o `cmd_rm` resolve o id primeiro). Extraído
/// para o `cluster delete` do modo kind poder remover nós sem passar por strings.
pub(crate) fn remove_container(images: &ImageStore, store: &Store, c: &Container, force: bool) -> Result<()> {
    runtime::remove(store, c, force)?;
    unpublish_ports(c);
    let _ = images.unmount_rootfs(&c.id);
    images.remove_container_dir(&c.id);
    Ok(())
}

fn cmd_rm(images: &ImageStore, store: &Store, id: &str, force: bool) -> Result<()> {
    let c = find(store, id)?;
    runtime::remove(store, &c, force)?;
    unpublish_ports(&c);
    let _ = images.unmount_rootfs(&c.id); // desmonta/limpa o scratch do overlay
    // DESTROY definitivo do directório do container (inclui o `rootfs/` flat).
    // O `unmount_rootfs` PRESERVA-o de propósito (é o estado do container, para
    // o `start` o reusar); só o `rm` o pode apagar. Sem isto o rootfs ficava
    // órfão para sempre: 49 directórios (45 GiB) acumulados numa só sessão de
    // testes, e o kubelet a marcar o nó com `disk-pressure`. A doc do
    // `remove_container_dir` já dizia "chamado pelo `rm`" — mas não era.
    images.remove_container_dir(&c.id);
    delonix_runtime_core::events::emit(
        &super::util::state_root(), "container", "remove", &c.id, &c.name, None,
    );
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
            c = store.update(&c.id, |cur| runtime::reconcile_status(cur)).unwrap_or(c);
        }
        cs.push(c);
    }
    println!("{}", serde_json::to_string_pretty(&cs).map_err(|e| Error::Invalid(e.to_string()))?);
    Ok(())
}

/// `container describe` — detalhe legível ao estilo `kubectl describe`.
///
/// Complementa o `inspect` (JSON, para máquinas/`jq`) em vez de o substituir:
/// esta é a vista para um humano perceber o estado de um container sem contar
/// chavetas. O `inspect` continua a ser o contrato estável para scripts.
fn cmd_describe(store: &Store, ids: &[String]) -> Result<()> {
    for (i, id) in ids.iter().enumerate() {
        let mut c = find(store, id)?;
        if runtime::reconcile_status(&mut c) {
            c = store.update(&c.id, |cur| runtime::reconcile_status(cur)).unwrap_or(c);
        }
        if i > 0 {
            println!();
        }
        describe_one(&c);
    }
    Ok(())
}

fn describe_one(c: &Container) {
    let mut d = output::Describe::new();
    d.field("Name", &c.name);
    d.field("ID", &c.id);
    d.field("Image", &c.image);
    d.field("Command", c.command.join(" "));
    d.field_opt("Workdir", c.workdir.as_deref());
    d.field("Created", output::fmt_local(c.created_unix));

    let uptime = match c.status {
        Status::Running | Status::Paused => c.pid_starttime.and_then(output::uptime_from_starttime),
        _ => None,
    };
    d.field("Status", fmt_status(&c.status, uptime));
    match c.pid {
        Some(p) => d.field("PID", p.to_string()),
        None => d.field("PID", "<none>"),
    };
    d.field_opt("Pod", c.pod.as_deref());

    d.section("Resources");
    d.sub("CPUs", &c.cpus);
    d.sub("Memory", &c.memory_max);
    d.sub_opt("CPU weight", c.cpu_weight.as_deref());
    d.sub_opt("Cpuset", c.cpuset.as_deref());
    d.sub_opt("IO weight", c.io_weight.as_deref());
    d.sub_opt("Nice", c.nice.map(|n| n.to_string()));

    d.section("Network");
    d.sub("Mode", c.network.as_deref().unwrap_or("host"));
    d.sub("IP", c.ip.as_deref().unwrap_or("<none>"));
    if !c.extra_networks.is_empty() {
        d.sub("Extra", c.extra_networks.iter().map(|n| format!("{} ({} em eth{})", n.network, n.ip, n.idx)).collect::<Vec<_>>().join(", "));
    }
    if !c.net_aliases.is_empty() {
        d.sub("Aliases", c.net_aliases.join(", "));
    }
    if let Some(bps) = &c.net_bps {
        d.sub("Rate limit", format!("{bps}{}", c.net_burst.as_ref().map(|b| format!(" (burst {b})")).unwrap_or_default()));
    }
    d.sub("Ports", if c.ports.is_empty() { "<none>".to_string() } else { fmt_ports(&c.ports) });

    if c.mounts.is_empty() {
        d.field("Mounts", "<none>");
    } else {
        d.section("Mounts");
        for m in &c.mounts {
            // Formato do `kubectl describe pod`: "<destino> from <origem> (rw)".
            d.item(format!("{} from {} ({})", m.target, m.source, if m.readonly { "ro" } else { "rw" }));
        }
    }

    d.list("Tmpfs", &c.tmpfs);
    d.list("Devices", &c.devices);
    d.list("Env", &c.env);
    // Só os NOMES dos secrets — o valor nunca é impresso (o `describe` é
    // rotineiramente colado em issues/chats).
    d.list("Secrets", &c.secrets);

    if c.labels.is_empty() {
        d.field("Labels", "<none>");
    } else {
        d.section("Labels");
        for (k, v) in &c.labels {
            d.item(format!("{k}={v}"));
        }
    }

    d.section("Security");
    d.sub("Privileged", c.privileged.to_string());
    d.sub("Read-only", c.read_only.to_string());
    d.sub("Userns", c.userns.to_string());
    d.sub_opt("Seccomp", c.seccomp.as_deref());
    d.sub_opt("AppArmor", c.apparmor.as_deref());
    if !c.cap_add.is_empty() {
        d.sub("Cap add", c.cap_add.join(", "));
    }
    if !c.cap_drop.is_empty() {
        d.sub("Cap drop", c.cap_drop.join(", "));
    }

    d.field("Restart policy", c.restart_policy.as_deref().unwrap_or("no"));
    d.field_opt("Log driver", c.log_driver.as_deref());
    d.print();
}

/// Argumentos do `container update`, agrupados (o clippy reclamaria da lista).
pub(crate) struct UpdateOpts {
    pub(crate) publish_add: Vec<String>,
    pub(crate) publish_rm: Vec<String>,
    pub(crate) volume_add: Vec<String>,
    pub(crate) volume_rm: Vec<String>,
    pub(crate) net_connect: Vec<String>,
    pub(crate) net_disconnect: Vec<String>,
    pub(crate) net_rate: Option<String>,
    pub(crate) net_burst: Option<String>,
    pub(crate) net_rate_clear: bool,
}

impl UpdateOpts {
    fn is_empty(&self) -> bool {
        self.publish_add.is_empty()
            && self.publish_rm.is_empty()
            && self.volume_add.is_empty()
            && self.volume_rm.is_empty()
            && self.net_connect.is_empty()
            && self.net_disconnect.is_empty()
            && self.net_rate.is_none()
            && !self.net_rate_clear
    }
}

/// Converte uma taxa (`10mbit`, `512kbit`, `1gbit`, ou bit/s cru) em bit/s.
/// Função pura — os sufixos são decimais (k=1000), como no `tc`, e NÃO 1024:
/// um `10mbit` que desse 10485760 bit/s não seria o que o `tc` programa.
fn parse_rate_bits(s: &str) -> Result<u64> {
    let t = s.trim().to_lowercase();
    let t = t.strip_suffix("bit").unwrap_or(&t);
    let (num, mult) = match t.strip_suffix('g') {
        Some(n) => (n, 1_000_000_000u64),
        None => match t.strip_suffix('m') {
            Some(n) => (n, 1_000_000),
            None => match t.strip_suffix('k') {
                Some(n) => (n, 1_000),
                None => (t, 1),
            },
        },
    };
    let v: f64 = num.trim().parse().map_err(|_| Error::Invalid(format!("taxa inválida: {s} (ex.: 10mbit, 512kbit, 1gbit)")))?;
    if v <= 0.0 {
        return Err(Error::Invalid(format!("taxa tem de ser positiva: {s}")));
    }
    Ok((v * mult as f64) as u64)
}

/// Converte um tamanho de burst (`32kb`, `1mb`, ou bytes crus) em bytes.
fn parse_burst_bytes(s: &str) -> Result<u64> {
    let t = s.trim().to_lowercase();
    let t = t.strip_suffix('b').unwrap_or(&t);
    let (num, mult) = match t.strip_suffix('m') {
        Some(n) => (n, 1_000_000u64),
        None => match t.strip_suffix('k') {
            Some(n) => (n, 1_000),
            None => (t, 1),
        },
    };
    let v: f64 = num.trim().parse().map_err(|_| Error::Invalid(format!("burst inválido: {s} (ex.: 32kb, 1mb)")))?;
    Ok((v * mult as f64) as u64)
}

/// Próximo índice de interface livre para uma rede adicional. `eth0` é sempre a
/// rede primária, por isso as extra começam em 1 — e reutilizamos buracos
/// deixados por um `--net-disconnect` em vez de contar sempre a subir.
fn next_extra_idx(c: &Container) -> u32 {
    (1u32..).find(|i| !c.extra_networks.iter().any(|n| n.idx == *i)).unwrap_or(1)
}

/// `container update` — reconfiguração A QUENTE de um container a correr.
///
/// A ordem das operações é deliberada: **remoções antes de adições**. Um
/// `--publish-rm 8080 --publish-add 8080:9000` num só comando tem de funcionar
/// (é o caso de uso óbvio: "muda esta porta para outro destino"); pela ordem
/// inversa, o add colidiria com a porta que o rm ia libertar.
///
/// Cada operação persiste no registo ASSIM QUE o dataplane confirma, uma a uma,
/// e não num `update` final: se a terceira falhar, as duas primeiras JÁ estão
/// aplicadas de facto no kernel — um registo escrito só no fim ficaria a mentir
/// sobre o estado real. Sem transacionalidade nem rollback, portanto; falha
/// fail-fast e o que passou fica (mesma semântica do `stack apply`).
fn cmd_update(store: &Store, id: &str, o: UpdateOpts) -> Result<()> {
    if o.is_empty() {
        return Err(Error::Invalid("nada a fazer: dá pelo menos uma mudança (--publish-add/--publish-rm/--volume-add/--volume-rm/--net-connect/--net-disconnect/--net-rate/--net-rate-clear)".into()));
    }
    let mut c = find(store, id)?;
    runtime::reconcile_status(&mut c);
    if !matches!(c.status, Status::Running | Status::Paused) {
        return Err(Error::Invalid(format!(
            "o container '{}' não está a correr ({}) — o update a quente actua no processo VIVO. \
             Arranca-o com `delonix container start {}` primeiro.",
            c.name, c.status, c.name
        )));
    }

    // --- remoções primeiro (ver doc-comment) ---
    for hp in &o.publish_rm {
        unpublish_live(store, &mut c, hp)?;
    }
    for target in &o.volume_rm {
        runtime::unmount_live(&c, target)?;
        let t = target.clone();
        c = store.update(&c.id, |cur| {
            let before = cur.mounts.len();
            cur.mounts.retain(|m| m.target != t);
            cur.mounts.len() != before
        })?;
        println!("{}: volume {target} desmontado a quente", c.name);
    }
    for net in &o.net_disconnect {
        let Some(en) = c.extra_networks.iter().find(|n| &n.network == net).cloned() else {
            return Err(Error::Invalid(format!("o container '{}' não está ligado à rede adicional '{net}'", c.name)));
        };
        infra::detach_extra_container(&c.id, en.idx);
        let n = net.clone();
        c = store.update(&c.id, |cur| {
            let before = cur.extra_networks.len();
            cur.extra_networks.retain(|x| x.network != n);
            cur.extra_networks.len() != before
        })?;
        println!("{}: desligado da rede {net} (eth{})", c.name, en.idx);
    }

    // --- adições ---
    for spec in &o.publish_add {
        publish_live(store, &mut c, spec)?;
    }
    for spec in &o.volume_add {
        let mounts = resolve_mounts(std::slice::from_ref(spec))?;
        for m in mounts {
            if c.mounts.iter().any(|x| x.target == m.target) {
                return Err(Error::Invalid(format!("já existe um volume montado em {} — desmonta-o primeiro (--volume-rm {})", m.target, m.target)));
            }
            runtime::mount_live(&c, &m)?;
            let mm = m.clone();
            c = store.update(&c.id, |cur| {
                cur.mounts.push(mm.clone());
                true
            })?;
            println!("{}: {} montado a quente em {} ({})", c.name, m.source, m.target, if m.readonly { "ro" } else { "rw" });
        }
    }
    for net in &o.net_connect {
        if c.network.is_none() {
            return Err(Error::Invalid(format!(
                "'{}' corre no caminho slirp-por-container (--net host/none), que não tem netns gerido pelo holder — \
                 ligar redes adicionais a quente só é possível a partir de um container criado com `--net <rede>`",
                c.name
            )));
        }
        if c.extra_networks.iter().any(|n| &n.network == net) || c.network.as_deref() == Some(net.as_str()) {
            return Err(Error::Invalid(format!("'{}' já está ligado à rede '{net}'", c.name)));
        }
        let idx = next_extra_idx(&c);
        let (ifname, ip) = infra::attach_extra_container(&c.id, idx, net)?;
        let en = delonix_runtime_core::ExtraNet { network: net.clone(), ip: ip.clone(), idx };
        c = store.update(&c.id, |cur| {
            cur.extra_networks.push(en.clone());
            true
        })?;
        println!("{}: ligado à rede {net} — {ip} em {ifname}", c.name);
    }

    // --- limite de banda ---
    if o.net_rate_clear {
        infra::clear_net_rate(&c.id);
        c = store.update(&c.id, |cur| {
            cur.net_bps = None;
            cur.net_burst = None;
            true
        })?;
        println!("{}: limite de banda removido", c.name);
    }
    if let Some(rate) = &o.net_rate {
        if c.network.is_none() {
            return Err(Error::Invalid(format!(
                "'{}' corre no caminho slirp-por-container (--net host/none) — o shaping é feito no veth do lado do \
                 ingress, que só existe para containers criados com `--net <rede>`",
                c.name
            )));
        }
        let bits = parse_rate_bits(rate)?;
        let burst_s = o.net_burst.clone().unwrap_or_else(|| "32kb".to_string());
        let burst = parse_burst_bytes(&burst_s)?;
        infra::set_net_rate(&c.id, bits, burst)?;
        let (r, b) = (rate.clone(), burst_s.clone());
        store.update(&c.id, |cur| {
            cur.net_bps = Some(r.clone());
            cur.net_burst = Some(b.clone());
            true
        })?;
        println!("{}: banda limitada a {rate} (burst {burst_s})", c.name);
    }
    Ok(())
}

/// Publica uma porta num container VIVO, pelo caminho certo para a rede dele.
fn publish_live(store: &Store, c: &mut Container, spec: &str) -> Result<()> {
    let (hp, cp, proto) = delonix_net::parse_publish(spec)?;
    if c.ports.iter().any(|p| delonix_net::parse_publish(p).map(|(h, _, _)| h == hp).unwrap_or(false)) {
        return Err(Error::Invalid(format!("'{}' já publica a porta de host {hp} — despublica-a primeiro (--publish-rm {hp})", c.name)));
    }
    if let Some(owner) = port_owner(store, &hp)? {
        return Err(Error::Invalid(format!("a porta {hp} já está publicada pelo container '{owner}'")));
    }
    match c.network.as_deref() {
        // Rede custom: DNAT no holder + hostfwd no slirp único (o ingress).
        Some(_) => {
            let ip = c.ip.clone().ok_or_else(|| Error::Invalid(format!("'{}' está numa rede custom mas não tem IP no registo", c.name)))?;
            publish_with_retry(&ip, spec)?;
        }
        // Caminho slirp-por-container: pede o hostfwd ao slirp DELE.
        None => {
            let pid = c.pid.ok_or_else(|| Error::NotRunning(c.name.clone()))?;
            let sock = delonix_net::slirp_container_sock(pid);
            if !sock.exists() {
                // O api-socket do slirp só é aberto quando o `run` leva `-p`
                // (ver `slirp_attach`): um container criado sem portas não tem
                // por onde receber um hostfwd a quente. Erro que ensina, em vez
                // de um "connection refused" cru vindo do socket.
                return Err(Error::Invalid(format!(
                    "'{}' foi criado sem `-p` e sem `--net <rede>`, por isso o seu slirp não tem api-socket aberto — \
                     não há por onde publicar a quente. Publica pelo menos uma porta no `run`, ou usa `--net <rede>` \
                     (o ingress aceita publicações a quente sempre).",
                    c.name
                )));
            }
            delonix_net::slirp_add_hostfwd(&sock, &hp, &cp, &proto)?;
        }
    }
    let s = spec.to_string();
    *c = store.update(&c.id, |cur| {
        cur.ports.push(s.clone());
        true
    })?;
    println!("{}: porta {hp}->{cp}/{proto} publicada a quente", c.name);
    Ok(())
}

/// Despublica uma porta de host num container VIVO.
fn unpublish_live(store: &Store, c: &mut Container, host_port: &str) -> Result<()> {
    let hit = c
        .ports
        .iter()
        .find(|p| delonix_net::parse_publish(p).map(|(h, _, _)| h == host_port).unwrap_or(false))
        .cloned()
        .ok_or_else(|| Error::Invalid(format!("'{}' não publica a porta de host {host_port}", c.name)))?;
    match c.network.as_deref() {
        Some(_) => infra::unpublish_port(host_port),
        None => {
            let pid = c.pid.ok_or_else(|| Error::NotRunning(c.name.clone()))?;
            let sock = delonix_net::slirp_container_sock(pid);
            infra::slirp_remove_hostfwd(&sock, host_port)?;
        }
    }
    *c = store.update(&c.id, |cur| {
        let before = cur.ports.len();
        cur.ports.retain(|p| p != &hit);
        cur.ports.len() != before
    })?;
    println!("{}: porta {host_port} despublicada a quente", c.name);
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
    use super::{fmt_ports, fmt_status, next_extra_idx, parse_burst_bytes, parse_rate_bits, policy_supervised, should_restart};
    use delonix_runtime_core::{Container, ExtraNet, Status};

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn taxas_usam_multiplos_decimais_como_o_tc() {
        // k/m/g são 1000, não 1024 — é o que o `tc` programa. Um `10mbit` a dar
        // 10485760 bit/s seria um limite diferente do pedido.
        assert_eq!(parse_rate_bits("10mbit").unwrap(), 10_000_000);
        assert_eq!(parse_rate_bits("512kbit").unwrap(), 512_000);
        assert_eq!(parse_rate_bits("1gbit").unwrap(), 1_000_000_000);
        assert_eq!(parse_rate_bits("1000").unwrap(), 1000);
        assert_eq!(parse_rate_bits("  10MBIT ").unwrap(), 10_000_000);
    }

    #[test]
    fn taxa_invalida_ou_nao_positiva_e_recusada() {
        assert!(parse_rate_bits("depressa").is_err());
        assert!(parse_rate_bits("0").is_err());
        assert!(parse_rate_bits("-5mbit").is_err());
        assert!(parse_burst_bytes("grande").is_err());
    }

    #[test]
    fn bursts_legiveis() {
        assert_eq!(parse_burst_bytes("32kb").unwrap(), 32_000);
        assert_eq!(parse_burst_bytes("1mb").unwrap(), 1_000_000);
        assert_eq!(parse_burst_bytes("4096").unwrap(), 4096);
    }

    fn c_com_extras(idxs: &[u32]) -> Container {
        let mut c = Container::new("id".into(), "t".into(), "img".into(), v(&["sh"]), "max".into());
        c.extra_networks = idxs.iter().map(|i| ExtraNet { network: format!("n{i}"), ip: "10.0.0.2".into(), idx: *i }).collect();
        c
    }

    #[test]
    fn indice_de_rede_extra_comeca_no_1_e_reutiliza_buracos() {
        // eth0 é sempre a rede primária, por isso as extra começam em 1.
        assert_eq!(next_extra_idx(&c_com_extras(&[])), 1);
        assert_eq!(next_extra_idx(&c_com_extras(&[1, 2])), 3);
        // Um --net-disconnect da do meio deixa um buraco: reutiliza-se, senão o
        // índice subia para sempre e os nomes de interface fugiam do eth1..N.
        assert_eq!(next_extra_idx(&c_com_extras(&[1, 3])), 2);
    }

    #[test]
    fn portas_no_formato_do_docker_ps() {
        assert_eq!(fmt_ports(&v(&["8080:80/tcp"])), "8080->80/tcp");
        // Sem protocolo explícito, tcp (default do docker).
        assert_eq!(fmt_ports(&v(&["8080:80"])), "8080->80/tcp");
        assert_eq!(fmt_ports(&v(&["8080:80", "53:53/udp"])), "8080->80/tcp, 53->53/udp");
        assert_eq!(fmt_ports(&[]), "");
    }

    #[test]
    fn status_no_formato_do_docker_ps() {
        assert_eq!(fmt_status(&Status::Running, Some(300)), "Up 5 minutes");
        assert_eq!(fmt_status(&Status::Paused, Some(300)), "Up 5 minutes (Paused)");
        assert_eq!(fmt_status(&Status::Stopped, None), "Exited (0)");
        assert_eq!(fmt_status(&Status::Failed(137), None), "Exited (137)");
        assert_eq!(fmt_status(&Status::Crashed, None), "Dead");
        assert_eq!(fmt_status(&Status::Created, None), "Created");
        // Running sem uptime legível não inventa uma duração.
        assert_eq!(fmt_status(&Status::Running, None), "Up");
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

    #[test]
    fn restart_policy_semantica_docker() {
        use delonix_runtime_core::Status as S;
        // `no` (e desconhecidas): nunca reinicia, tenha morrido como tiver.
        for st in [S::Stopped, S::Failed(1), S::Crashed] {
            assert!(!should_restart("no", &st, 0));
            assert!(!should_restart("qualquer-coisa", &st, 0));
        }
        // `always`/`unless-stopped`: sempre, mesmo em saída limpa.
        for p in ["always", "unless-stopped"] {
            assert!(should_restart(p, &S::Stopped, 0));
            assert!(should_restart(p, &S::Failed(1), 99));
            assert!(should_restart(p, &S::Crashed, 99));
        }
        // `on-failure`: só em falha; saída 0 pára.
        assert!(!should_restart("on-failure", &S::Stopped, 0));
        assert!(should_restart("on-failure", &S::Failed(2), 0));
        assert!(should_restart("on-failure", &S::Crashed, 0));
        // `on-failure:max` respeita o tecto (o `max` conta REINÍCIOS já feitos).
        assert!(should_restart("on-failure:3", &S::Failed(1), 2));
        assert!(!should_restart("on-failure:3", &S::Failed(1), 3));
        assert!(!should_restart("on-failure:0", &S::Failed(1), 0));
        // `on-failure` sem `max` não tem tecto.
        assert!(should_restart("on-failure", &S::Failed(1), 10_000));
    }

    #[test]
    fn policy_supervised_so_para_politicas_activas() {
        assert!(!policy_supervised("no"));
        assert!(!policy_supervised(""));
        assert!(policy_supervised("always"));
        assert!(policy_supervised("unless-stopped"));
        assert!(policy_supervised("on-failure"));
        assert!(policy_supervised("on-failure:5"));
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
