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
    /// Imprime o script de autocompletion do shell (bash/zsh/fish/...).
    Completion {
        /// Shell alvo.
        shell: CompShell,
    },
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Container { action } => cmd::container::run(action),
        Cmd::Image { vm, action } => cmd::image::run(vm, action),
        Cmd::Build(args) => cmd::build::run(args),
        Cmd::Vm { action } => cmd::vm::run(action),
        Cmd::Volumes { action } => cmd::volume::run(action),
        Cmd::Network { action } => cmd::network::run(action),
        Cmd::Stack { action } => cmd::stack::run(action),
        Cmd::System { action } => cmd::system::run(action),
        Cmd::Cluster { action } => cmd::cluster::run(action),
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

    // Autocompletion dinâmico: se o shell pediu sugestões (env COMPLETE), trata
    // disso e termina; caso contrário, segue o fluxo normal.
    clap_complete::CompleteEnv::with_factory(<Cli as clap::CommandFactory>::command).complete();

    if let Err(e) = run() {
        eprintln!("delonix: {e}");
        std::process::exit(1);
    }
}
