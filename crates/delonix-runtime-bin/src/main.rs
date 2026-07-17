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
    about = "Delonix Runtime — motor de containers e microVMs daemonless, rootless-first, kernel-native, em Rust"
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
    /// Containers: run/ps/stop/rm/exec/logs.
    Container {
        #[command(subcommand)]
        action: cmd::container::ContainerCmd,
    },
    /// Imagens: pull/ls/rm/export. Com `--vm`: imagens VM douradas
    /// (ls/pull/push/build) em vez de imagens de container.
    Image {
        /// Opera sobre imagens VM (`<root>/vm-images/`) em vez de imagens de
        /// container — activa os subcomandos `push`/`build`.
        #[arg(long)]
        vm: bool,
        #[command(subcommand)]
        action: cmd::image::ImageCmd,
    },
    /// Constrói uma imagem a partir de um Dockerfile.
    Build(cmd::build::BuildArgs),
    /// microVMs declarativas: create/ls/stop/rm/status.
    Vm {
        #[command(subcommand)]
        action: cmd::vm::VmCmd,
    },
    /// Volumes nomeados: create/ls/rm/inspect.
    Volumes {
        #[command(subcommand)]
        action: cmd::volume::VolumeCmd,
    },
    /// Redes de utilizador: ls/create/rm/inspect.
    Network {
        #[command(subcommand)]
        action: cmd::network::NetworkCmd,
    },
    /// Cofre de segredos (cifrado em repouso) — o produtor do `run --secret`.
    Secret {
        #[command(subcommand)]
        action: cmd::secret::SecretCmd,
    },
    /// Armazenamento de REDE (NFS/CIFS/WebDAV) montável como volume — estilo PV do k8s.
    Storage {
        #[command(subcommand)]
        action: cmd::storage::StorageCmd,
    },
    /// Aplica um manifesto (`delonix-manifest.yaml`) inteiro — todos os Kinds.
    Stack {
        #[command(subcommand)]
        action: cmd::stack::StackCmd,
    },
    /// O motor em si: eventos, estado e uso de disco.
    System {
        #[command(subcommand)]
        action: cmd::system::SystemCmd,
    },
    /// Bootstrap `kubeadm` idempotente sobre SSH (`kind: Cluster`).
    Cluster {
        #[command(subcommand)]
        action: cmd::cluster::ClusterCmd,
    },
    /// Gera manifestos Kubernetes a partir de containers/pods (`generate`).
    Kube {
        #[command(subcommand)]
        action: cmd::kube::KubeCmd,
    },
    /// Gestão de baixo nível da infra de ingress rootless (up/status/attach/publish/firewall).
    Netns {
        #[command(subcommand)]
        action: cmd::netns::NetnsCmd,
    },
    /// Serve o endpoint CRI (`runtime.v1`) num socket unix — substitui o
    /// containerd/CRI-O para um kubelet falar com o Delonix.
    Cri {
        /// Endereço do socket (default: `$DELONIX_CRI_ADDR` ou `unix:///run/delonix-cri.sock`).
        #[arg(long)]
        addr: Option<String>,
    },
    /// Imprime o script de autocompletion do shell (bash/zsh/fish/...).
    Completion {
        /// Shell alvo.
        shell: CompShell,
    },
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    // Locale ANTES de qualquer output: a flag `--l18n` tem precedência sobre
    // `$DELONIX_L18N`; sem nenhum, fica em inglês (o default do repo público).
    if let Some(l) = cli.l18n.or_else(|| std::env::var("DELONIX_L18N").ok()) {
        cmd::output::set_lang(&l);
    }
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
        Cmd::Cri { addr } => {
            let addr = addr
                .or_else(|| std::env::var("DELONIX_CRI_ADDR").ok())
                .unwrap_or_else(|| "unix:///run/delonix-cri.sock".to_string());
            delonix_cri::serve_blocking(cmd::util::state_root(), &addr)
        }
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
        if let Err(e) = cmd::mapped::volsnap(&raw[2], std::path::Path::new(&raw[3]), std::path::Path::new(&raw[4])) {
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
