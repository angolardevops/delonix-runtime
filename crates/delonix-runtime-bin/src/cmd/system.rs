//! `delonix system` — o motor em si: eventos, estado e uso de disco.
//!
//! É um GRUPO, não comandos soltos: `events`/`info`/`df` são sobre o motor, não
//! sobre um container ou uma imagem em particular — tal como no docker
//! (`docker system ...`). O que é por-objecto continua no grupo do objecto
//! (`container stats`, `image ls`).

use clap::Subcommand;
use delonix_runtime_core::{events, Result, Store};

use super::util::{open_stores, state_root};

#[derive(Subcommand)]
pub enum SystemCmd {
    /// Eventos do motor (create/start/die/remove/…), do mais antigo ao mais
    /// recente. Sem daemon, o registo é um log append-only partilhado — cada
    /// comando acrescenta a sua linha (ver `delonix_runtime_core::events`).
    Events {
        /// Segue em contínuo (Ctrl-C para sair).
        #[arg(short, long)]
        follow: bool,
        /// Mostra só os últimos N (default: todos).
        #[arg(short = 'n', long)]
        tail: Option<usize>,
    },
    /// Estado do motor: rootless?, delegação de cgroup, infra de rede, contagens.
    Info,
    /// Uso de disco por área (imagens, containers, volumes, imagens VM).
    Df,
}

pub fn run(action: SystemCmd) -> Result<()> {
    match action {
        SystemCmd::Events { follow, tail } => cmd_events(follow, tail),
        SystemCmd::Info => cmd_info(),
        SystemCmd::Df => cmd_df(),
    }
}

fn cmd_events(follow: bool, tail: Option<usize>) -> Result<()> {
    let root = state_root();
    let evs = events::read(&root);
    let start = tail.map(|n| evs.len().saturating_sub(n)).unwrap_or(0);
    for e in &evs[start..] {
        println!("{}", e.to_line());
    }
    if !follow {
        return Ok(());
    }
    // `-f`: sonda o crescimento do ficheiro. Sem daemon não há push — mas o
    // custo é um `stat` por segundo, e o log é a única fonte de verdade.
    let mut offset = events::size(&root);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let (novos, next) = events::read_from(&root, offset);
        offset = next;
        for e in novos {
            println!("{}", e.to_line());
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

/// Soma recursiva do tamanho de um directório (aparente, como o `du`).
fn dir_size(p: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else { return 0 };
    rd.flatten()
        .map(|e| {
            let path = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => dir_size(&path),
                Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => 0, // symlinks não contam (contariam duas vezes)
            }
        })
        .sum()
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let (mut v, mut i) = (b as f64, 0);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

/// `system df` — onde está o disco. Existe por uma razão concreta: rootfs
/// órfãos chegaram a acumular 45 GiB sem nada os reportar, até o kubelet marcar
/// o nó com `disk-pressure`. O `RECUPERÁVEL` é a coluna que interessa.
fn cmd_df() -> Result<()> {
    let root = state_root();
    let (_, store) = open_stores()?;
    let live: std::collections::HashSet<String> = store.list()?.into_iter().map(|c| c.id).collect();

    let containers_dir = root.join("containers");
    let mut orphan = 0u64;
    let mut orphan_n = 0usize;
    if let Ok(rd) = std::fs::read_dir(&containers_dir) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = e.file_name().to_string_lossy().into_owned();
                if !live.contains(&name) {
                    orphan += dir_size(&e.path());
                    orphan_n += 1;
                }
            }
        }
    }

    println!("{:<16}  {:>10}  {:>12}", "ÁREA", "TAMANHO", "RECUPERÁVEL");
    for (label, dir) in [
        ("imagens", root.join("blobs")),
        ("layers", root.join("layers")),
        ("containers", containers_dir.clone()),
        ("volumes", root.join("volumes")),
        ("imagens VM", root.join("vm-images")),
    ] {
        let size = dir_size(&dir);
        let recl = if label == "containers" { human(orphan) } else { "-".to_string() };
        println!("{label:<16}  {:>10}  {recl:>12}", human(size));
    }
    if orphan_n > 0 {
        println!(
            "\n{orphan_n} directório(s) de container órfão(s) — {} recuperáveis.\n\
             São restos de containers mortos abruptamente (o `rm` normal limpa-os).",
            human(orphan)
        );
    }
    Ok(())
}

/// `system info` — o que o motor É nesta máquina. Sem isto, diagnosticar
/// "porque é que os limites não se aplicam" ou "porque é que o `-p` falha"
/// obriga a ler código.
fn cmd_info() -> Result<()> {
    let (_, store) = open_stores()?;
    let cs = store.list()?;
    let running = cs
        .iter()
        .filter(|c| matches!(c.status, delonix_runtime_core::Status::Running))
        .count();

    println!("Delonix Runtime {}", env!("CARGO_PKG_VERSION"));
    println!("  raiz de estado:     {}", state_root().display());
    let rootless = delonix_runtime::is_rootless();
    println!("  modo:               {}", if rootless { "rootless (sem daemon)" } else { "root (sem daemon)" });
    // Isto é a pergunta nº1 quando os limites "não funcionam".
    let delegated = std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists()
        && std::fs::read_to_string("/sys/fs/cgroup/cgroup.subtree_control")
            .map(|s| s.contains("memory"))
            .unwrap_or(false);
    println!(
        "  cgroup2 delegado:   {}",
        if delegated { "sim" } else { "não — memory/cpu/pids NÃO são aplicados (corre sob systemd-run --user --scope -p Delegate=yes)" }
    );
    let infra = delonix_net::infra::status();
    println!(
        "  infra de rede:      {}",
        match infra.holder_pid {
            Some(p) => format!("de pé (holder pid {p})"),
            None => "em baixo (sobe sozinha quando precisar)".to_string(),
        }
    );
    println!("  containers:         {} ({running} a correr)", cs.len());
    println!("  eventos:            {}", events::read(&state_root()).len());
    Ok(())
}

/// Atalho para o `Store` — o `system` mexe em contagens, não em ciclo de vida.
#[allow(dead_code)]
fn store_only() -> Result<Store> {
    Store::open(Store::default_root())
}
