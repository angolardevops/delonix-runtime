//! `delonix` — a CLI opensource do Delonix Runtime: motor de containers e
//! microVMs daemonless, rootless-first, kernel-native. Homólogo ao Docker;
//! distinto do `delonix`/`delonixctl` privados do `delonix-paas` (outro repo,
//! outra árvore de dependências — ver `CLAUDE.md`).
//!
//! Comandos agrupados semanticamente (em vez de uma lista plana): `container`
//! (run/ps/stop/rm/exec/logs), `image` (pull/ls/rm/export), `build`
//! (Dockerfile/Delonixfile → imagem), `vm` (microVMs declarativas), `volumes`
//! (volumes nomeados), `network` (redes de utilizador) e `stack` (aplica um
//! `delonix-manifest.yaml` inteiro). Cada grupo com `apply` também aceita um
//! manifesto por-Kind (`delonix <grupo> apply [-f ficheiro]`) — ver
//! `cmd::manifest`. Cada grupo vive em `src/cmd/<nome>.rs`.

mod cmd;

use clap::{Parser, Subcommand, ValueEnum};
use delonix_runtime_core::Result;

/// Shells suportados por `delonix completion`.
#[derive(Clone, Copy, ValueEnum)]
enum CompShell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
}

#[derive(Parser)]
#[command(
    name = "delonix",
    version,
    long_version = long_version_text(),
    about = "Delonix Runtime — a daemonless, rootless-first container & microVM engine (kernel-native, Rust). The open-source engine that powers Delonix."
)]
struct Cli {
    /// Output language: `en` (default) or `pt` (Portuguese, pt_AO). Also settable
    /// via `$DELONIX_L18N`. Global — works before any subcommand.
    #[arg(long = "l18n", global = true, value_name = "en|pt")]
    l18n: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

// `Vm` carrega `VmCmd`, que tem uma variante `Create` grande (muitos flags
// opcionais) — mesma justificação do `#[allow]` em `cmd::vm::VmCmd`: enum de
// CLI parseado uma vez por invocação, não um hot-path.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    /// Containers: run/ps/stop/rm/exec/logs/update/describe.
    Container {
        #[command(subcommand)]
        action: cmd::container::ContainerCmd,
    },
    /// OCI images: pull/ls/rm/export (with `--vm`: golden VM images — ls/pull/push/build).
    Image {
        /// Operate on VM images (`<root>/vm-images/`) instead of container images — enables the `push`/`build` subcommands.
        #[arg(long)]
        vm: bool,
        #[command(subcommand)]
        action: cmd::image::ImageCmd,
    },
    /// Build an image from a Dockerfile or Delonixfile.
    Build(cmd::build::BuildArgs),
    /// Declarative microVMs: create/ls/stop/rm/status.
    Vm {
        #[command(subcommand)]
        action: cmd::vm::VmCmd,
    },
    /// Named volumes and bind mounts: create/ls/rm/inspect.
    Volumes {
        #[command(subcommand)]
        action: cmd::volume::VolumeCmd,
    },
    /// User networks: ls/create/rm/inspect.
    Network {
        #[command(subcommand)]
        action: cmd::network::NetworkCmd,
    },
    /// Encrypted-at-rest secret vault — the producer of `run --secret`.
    Secret {
        #[command(subcommand)]
        action: cmd::secret::SecretCmd,
    },
    /// NETWORK storage (NFS/CIFS/WebDAV) mountable as a volume — k8s PersistentVolume style.
    Storage {
        #[command(subcommand)]
        action: cmd::storage::StorageCmd,
    },
    /// Apply a whole manifest (`delonix-manifest.yaml`) — every Kind, in dependency order.
    Stack {
        #[command(subcommand)]
        action: cmd::stack::StackCmd,
    },
    /// The engine itself: events, state and disk usage.
    System {
        #[command(subcommand)]
        action: cmd::system::SystemCmd,
    },
    /// Idempotent `kubeadm` bootstrap over SSH (`kind: Cluster`), or full VM provisioning.
    Cluster {
        #[command(subcommand)]
        action: cmd::cluster::ClusterCmd,
    },
    /// Generate Kubernetes manifests from containers/pods (`generate`).
    Kube {
        #[command(subcommand)]
        action: cmd::kube::KubeCmd,
    },
    /// Low-level management of the rootless ingress infra (up/status/attach/publish/firewall).
    Netns {
        #[command(subcommand)]
        action: cmd::netns::NetnsCmd,
    },
    /// Live per-container traffic (eBPF datapath; degrades to veth counters).
    Flow {
        /// Watch only this interface (default: auto — every SDN veth).
        #[arg(long)]
        iface: Option<String>,
        /// Refresh continuously (every 2s) instead of printing once.
        #[arg(long, short)]
        watch: bool,
    },
    /// INBOUND firewall (L4 rules + DNAT publishes) for a container on the SDN.
    Ingress {
        #[command(subcommand)]
        action: cmd::firewall::IngressCmd,
    },
    /// OUTBOUND firewall (L4 rules + per-network egress policy) for a container.
    Egress {
        #[command(subcommand)]
        action: cmd::firewall::EgressCmd,
    },
    /// Embedded L7/HTTP reverse-proxy (`kind: HTTPRoute`): ls/apply/rm.
    Httproute {
        #[command(subcommand)]
        action: cmd::httproute::HttpRouteCmd,
    },
    /// Boot persistence: systemd units so containers come back up after a reboot.
    Boot {
        #[command(subcommand)]
        action: cmd::boot::BootCmd,
    },
    /// Serve the CRI endpoint (`runtime.v1`) on a unix socket — replaces containerd/CRI-O for a kubelet.
    Cri {
        /// Socket address (default: `$DELONIX_CRI_ADDR` or `unix:///run/delonix-cri.sock`).
        #[arg(long)]
        addr: Option<String>,
    },
    /// Serve the MANAGEMENT API (HTTP+JSON) on a unix socket — the surface an external control-plane consumes to operate the engine.
    Api {
        /// Socket address (default: `$DELONIX_API_ADDR` or `unix:///run/delonix-mgmt.sock`).
        #[arg(long)]
        addr: Option<String>,
    },
    /// Runtime summary/KPI dashboard (interactive htop-style TUI) — global, or per group (`container dash`, `vm dash`, ...).
    Dash {
        /// Print ONE text snapshot and exit (no TUI) — for scripts/CI; the default when stdout is not a terminal.
        #[arg(long)]
        once: bool,
    },
    /// Print the shell autocompletion script (bash/zsh/fish/...).
    Completion {
        /// Target shell.
        shell: CompShell,
    },
    /// (interno) O reverse-proxy L7 embutido que serve os `kind: HTTPRoute`. NÃO é
    /// para uso manual — o `stack apply` lança-o dentro do netns do holder (ver
    /// `cmd::httproute`/`cmd::ingress_proxy`).
    #[command(hide = true)]
    IngressProxy {
        /// Ficheiro JSON com a `ProxyConfig` (listeners + rotas já resolvidas).
        #[arg(long)]
        config: std::path::PathBuf,
    },
}

/// O cartão de visita do `--version` (o `-V` mantém a linha curta e estável
/// para scripts): identidade do build + o que fazer a seguir. É a primeira
/// coisa que um utilizador novo corre — merece apontar o caminho.
fn long_version_text() -> &'static str {
    use cmd::po::t;
    // Leak deliberado e único: o clap builder exige &'static str (sem a feature
    // "string"), e isto corre uma vez por processo — não é fuga acumulável.
    Box::leak(
        format!(
            "delonix {v}\n\
         {tag}\n\
         commit: {hash} · built: {date} · {lic}\n\
         \n\
         {try_}:\n\
         \x20 delonix container run -d -p 8080:80 nginx   # {c1}\n\
         \x20 delonix vm create --name dev ...            # {c2}\n\
         \x20 delonix cluster create                      # {c3}\n\
         \x20 delonix stack init && delonix stack apply   # {c4}\n\
         \x20 delonix dash                                # {c5}\n\
         \n\
         {docs}: https://angolardevops.github.io/delonix-runtime/ · delonix <group> --help",
            v = env!("CARGO_PKG_VERSION"),
            tag = t("daemonless, rootless-first container & microVM engine (kernel-native, Rust)"),
            hash = env!("DELONIX_GIT_HASH"),
            date = env!("DELONIX_BUILD_DATE"),
            lic = "Apache-2.0",
            try_ = t("get started"),
            c1 = t("a web service in seconds"),
            c2 = t("declarative microVMs"),
            c3 = t("local Kubernetes (kind mode, no Docker)"),
            c4 = t("a complete declarative project"),
            c5 = t("htop-style dashboard"),
            docs = t("docs"),
        )
        .into_boxed_str(),
    )
}

fn run() -> Result<()> {
    // Língua ANTES do parse do clap: o help é gerado DURANTE o parse, logo a
    // decisão tem de vir de um peek ao argv/ambiente (`--l18n` tem precedência
    // sobre `$DELONIX_L18N`; sem nenhum, inglês — o default do repo público).
    if let Some(l) = cmd::po::peek_lang() {
        cmd::output::set_lang(&l);
    }
    let mut command = <Cli as clap::CommandFactory>::command();
    if cmd::output::is_pt() {
        // Fonte do help em EN; em pt, reescreve about/help via o catálogo pt.po.
        command = cmd::po::translate_help(command);
    }
    let cli = match <Cli as clap::FromArgMatches>::from_arg_matches(&command.get_matches()) {
        Ok(v) => v,
        Err(e) => e.exit(),
    };
    let _ = cli.l18n; // já consumida pelo peek (fica no schema p/ o help)
    match cli.cmd {
        Cmd::Container { action } => cmd::container::run(action),
        Cmd::Image { vm, action } => cmd::image::run(vm, action),
        Cmd::Build(args) => cmd::build::run(args),
        Cmd::Vm { action } => cmd::vm::run(action),
        Cmd::Volumes { action } => cmd::volume::run(action),
        Cmd::Network { action } => cmd::network::run(action),
        Cmd::Secret { action } => cmd::secret::run(action),
        Cmd::Storage { action } => cmd::storage::run(action),
        Cmd::Stack { action } => cmd::stack::run(action),
        Cmd::System { action } => cmd::system::run(action),
        Cmd::Cluster { action } => cmd::cluster::run(action),
        Cmd::Kube { action } => cmd::kube::run(action),
        Cmd::Netns { action } => cmd::netns::run(action),
        Cmd::Boot { action } => cmd::boot::run(action),
        Cmd::Flow { iface, watch } => cmd::flow::run(iface, watch),
        Cmd::Ingress { action } => cmd::firewall::run_ingress(action),
        Cmd::Egress { action } => cmd::firewall::run_egress(action),
        Cmd::IngressProxy { config } => cmd::ingress_proxy::run(&config),
        Cmd::Httproute { action } => cmd::httproute::run(action),
        Cmd::Cri { addr } => {
            let addr = addr
                .or_else(|| std::env::var("DELONIX_CRI_ADDR").ok())
                .unwrap_or_else(|| "unix:///run/delonix-cri.sock".to_string());
            delonix_cri::serve_blocking(cmd::util::state_root(), &addr)
        }
        Cmd::Api { addr } => {
            let addr = addr
                .or_else(|| std::env::var("DELONIX_API_ADDR").ok())
                .unwrap_or_else(|| "unix:///run/delonix-mgmt.sock".to_string());
            delonix_mgmt::serve_blocking(cmd::util::state_root(), &addr)
        }
        Cmd::Dash { once } => cmd::dash::run(cmd::dash::DashScope::Global, once),
        Cmd::Completion { shell } => cmd_completion(shell),
    }
}

/// `delonix completion <shell>` — imprime o **script de registo** do
/// autocompletion. Usa a engine dinâmica do clap: o script chama
/// `COMPLETE=<shell> delonix -- …` para obter as sugestões de comandos/
/// subcomandos/flags em tempo real, a partir da MESMA definição de `Cli`
/// usada para o parsing — nunca fica desactualizado à mão.
fn cmd_completion(shell: CompShell) -> Result<()> {
    use clap_complete::env::{Bash, Elvish, EnvCompleter, Fish, Powershell, Zsh};
    use std::io::Write;
    let completer: &dyn EnvCompleter = match shell {
        CompShell::Bash => &Bash,
        CompShell::Zsh => &Zsh,
        CompShell::Fish => &Fish,
        CompShell::Elvish => &Elvish,
        CompShell::Powershell => &Powershell,
    };
    let mut buf = Vec::new();
    completer.write_registration("COMPLETE", "delonix", "delonix", "delonix", &mut buf)?;
    let _ = std::io::stdout().write_all(&buf);
    Ok(())
}

fn main() {
    delonix_runtime_core::telemetry::init();
    // Re-exec oculto do holder de netns (`delonix netns holder`, invocado pelo
    // próprio `delonix-net::infra::start_holder` via `unshare` — nunca pelo
    // utilizador). Tem de ser interceptado ANTES do clap parsear (não é um
    // subcomando público) — sem isto, `--net <rede-custom>` falha sempre com
    // "timeout à espera do holder do netns" (o re-exec cai no parser normal e
    // é recusado como subcomando desconhecido).
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() == 3 && raw[1] == "netns" && raw[2] == "holder" {
        delonix_net::infra::holder_main(); // nunca retorna
    }
    // Re-exec oculto do 2.º passo do `--net <rede>` (ver `container::reexec_into_netns`):
    // já corremos DENTRO do userns+netns do holder; a spec do container vem num
    // ficheiro. Interceptado ANTES do clap — não é um subcomando público.
    if raw.len() == 4 && raw[1] == "netns" && raw[2] == "run" {
        if let Err(e) = cmd::container::run_from_spec(std::path::Path::new(&raw[3])) {
            eprintln!("delonix: {e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    // Re-exec ocultos MAPEADOS (`__rmtree`, `__volsnap`): já corremos como root
    // num user namespace com os subuids mapeados (o pai usou `newuidmap` — ver
    // `delonix_runtime::{remove_tree_mapped, reexec_mapped}`), logo somos donos
    // efectivos dos ficheiros que o container escreveu.
    //
    // **Estas metades faltavam neste binário** e só existiam na CLI privada do
    // `delonix-paas`: a biblioteca PÚBLICA re-executava `delonix __rmtree` e o
    // `delonix` público respondia "unrecognized subcommand" (rc=2) — com o
    // `remove_tree_mapped` a nem olhar para o exit status, a árvore ficava por
    // apagar em SILÊNCIO. O motor público tem de bastar-se a si próprio.
    // Interceptados antes do clap, como os `netns` acima.
    if raw.len() == 3 && raw[1] == "__rmtree" {
        if let Err(e) = cmd::mapped::rmtree(std::path::Path::new(&raw[2])) {
            eprintln!("delonix: {e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    if raw.len() == 5 && raw[1] == "__volsnap" {
        if let Err(e) = cmd::mapped::volsnap(
            &raw[2],
            std::path::Path::new(&raw[3]),
            std::path::Path::new(&raw[4]),
        ) {
            eprintln!("delonix: {e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    if raw.len() == 4 && raw[1] == "__buildtar" {
        if let Err(e) =
            cmd::mapped::buildtar(std::path::Path::new(&raw[2]), std::path::Path::new(&raw[3]))
        {
            eprintln!("delonix: {e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    // Autocompletion dinâmico: se o shell pediu sugestões (env COMPLETE), trata
    // disso e termina; caso contrário, segue o fluxo normal.
    clap_complete::CompleteEnv::with_factory(<Cli as clap::CommandFactory>::command).complete();

    if let Err(e) = run() {
        // O erro de topo a vermelho (honra NO_COLOR/pipes — ver `output::cor`).
        cmd::output::error(&e.to_string());
        std::process::exit(1);
    }
}
