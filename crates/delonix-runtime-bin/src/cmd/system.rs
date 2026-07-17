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
    /// Virtualização do host: hipervisor, KVM, virtio — e o que há a afinar.
    Virt {
        /// Aplica as afinações recomendadas (precisa de root).
        #[arg(long)]
        tune: bool,
    },
    /// Recupera espaço: remove containers parados, imagens sem uso, blobs do
    /// CAS que ninguém referencia, cgroups vazios e — o que mais espaço liberta
    /// — **directórios de containers órfãos** (de nós/containers que morreram
    /// abruptamente sem `rm`, sem entrada no registo).
    Prune {
        /// Também remove imagens sem uso que TÊM tag (não só as dangling).
        #[arg(short, long)]
        all: bool,
    },
    /// Governador térmico: baixa o orçamento de CPU do Delonix quando o CPU
    /// aquece e repõe-no quando arrefece. Corre em contínuo (ver `--once`).
    Thermal {
        /// Temperatura (°C) a partir da qual se arrefece.
        #[arg(long, default_value_t = 85)]
        high: u64,
        /// Temperatura (°C) abaixo da qual se restaura.
        #[arg(long, default_value_t = 70)]
        low: u64,
        /// Percentagem mínima de CPU a que se desce.
        #[arg(long, default_value_t = 40)]
        floor: u64,
        /// Segundos entre leituras.
        #[arg(long, default_value_t = 5)]
        interval: u64,
        /// Uma leitura e sai (para cron/scripts, em vez do loop).
        #[arg(long)]
        once: bool,
    },
}

pub fn run(action: SystemCmd) -> Result<()> {
    match action {
        SystemCmd::Events { follow, tail } => cmd_events(follow, tail),
        SystemCmd::Info => cmd_info(),
        SystemCmd::Df => cmd_df(),
        SystemCmd::Prune { all } => cmd_prune(all),
        SystemCmd::Virt { tune } => cmd_virt(tune),
        SystemCmd::Thermal { high, low, floor, interval, once } => cmd_thermal(high, low, floor, interval, once),
    }
}

/// `system prune` — recupera espaço em disco.
///
/// A ordem importa: containers parados primeiro (libertam imagens e blobs),
/// depois o que deixou de ser referenciado. O passo que mais liberta é o **4**,
/// os directórios órfãos — o problema real medido nesta máquina: **88
/// directórios de container em disco contra 4 no registo (~36 GiB)**. Vêm de nós
/// de cluster e containers que morreram por SIGKILL/crash/sessão-fechada **sem
/// `rm`**, por isso nunca ninguém os varreu. O `container rm` normal nunca os
/// apanha (não estão no registo); só um GC explícito como este.
fn cmd_prune(all: bool) -> Result<()> {
    use std::collections::HashSet;
    let (images, store) = open_stores()?;

    // Slirps órfãos (alvo morto) — o reaper SEGURO (nunca o `reap_orphan_hostfwds`
    // fail-open; ver a história do reaper que apagava portas vivas).
    let reaped = delonix_net::reap_orphan_slirp();
    if reaped > 0 {
        println!("rede: {reaped} slirp(s) órfão(s) reapado(s)");
    }

    // 1) containers parados (no registo).
    let mut rmc = 0usize;
    for c in store.list()? {
        if c.pid.map(delonix_runtime::is_alive).unwrap_or(false) {
            continue;
        }
        let _ = delonix_runtime::remove(&store, &c, true);
        let _ = images.unmount_rootfs(&c.id);
        images.remove_container_dir(&c.id);
        rmc += 1;
    }

    // Ids ainda vivos DEPOIS do passo 1 — a base para decidir o que é órfão.
    let live_ids: HashSet<String> = store.list()?.iter().map(|c| c.id.clone()).collect();

    // 2) imagens dangling (sem tag), ou todas as não usadas com `-a`.
    let in_use: HashSet<String> = store.list()?.iter().map(|c| c.image.clone()).collect();
    let mut rmi = 0usize;
    for img in images.list()? {
        let dangling = img.repo_tags.is_empty() || img.repo_tags.iter().all(|t| t.contains("<none>"));
        let used = in_use.contains(&img.id) || img.repo_tags.iter().any(|t| in_use.contains(t));
        if (dangling || all) && !used {
            if img.repo_tags.is_empty() {
                let _ = images.remove(&img.id);
            } else {
                for t in &img.repo_tags {
                    let _ = images.remove(t);
                }
            }
            rmi += 1;
        }
    }

    // 3) blobs do CAS que já ninguém referencia.
    let mut referenced: HashSet<String> = HashSet::new();
    for img in images.list()? {
        referenced.insert(delonix_image::cas::strip(&img.id).to_string());
        for l in &img.layers {
            referenced.insert(delonix_image::cas::strip(l).to_string());
        }
    }
    let (mut rmb, mut freed) = (0usize, 0u64);
    let blobs_dir = images.root().join("blobs").join("sha256");
    if let Ok(rd) = std::fs::read_dir(&blobs_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || referenced.contains(&name) {
                continue;
            }
            freed += e.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = std::fs::remove_file(e.path());
            rmb += 1;
        }
    }

    // 4) DIRECTÓRIOS de container órfãos — o grande recuperador de espaço.
    //
    // Um `<containers>/<id>/` cujo `<id>` já não está no registo: o container
    // morreu sem `rm`. Usa-se `remove_tree_mapped` e não `remove_dir_all` porque
    // o rootfs pode ter ficheiros de SUBUID (escritos por um container rootless)
    // que o utilizador real não apaga directamente — é exactamente o caminho que
    // o `__rmtree` desta série passou a suportar de facto.
    let containers_dir = images.root().join("containers");
    let (mut rmd, mut freed_dirs) = (0usize, 0u64);
    if let Ok(rd) = std::fs::read_dir(&containers_dir) {
        for e in rd.flatten() {
            // Só directórios cujo nome é um id (as entradas do registo são
            // `<id>.json`, ficheiros — não entram aqui).
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = e.file_name().to_string_lossy().into_owned();
            if live_ids.contains(&id) {
                continue;
            }
            freed_dirs += dir_size(&e.path());
            delonix_runtime::remove_tree_mapped(&e.path());
            rmd += 1;
        }
    }

    // 5) cgroups VAZIOS órfãos na delonix.slice.
    let live_cg: HashSet<String> = live_ids.iter().map(|id| format!("delonix-{id}")).collect();
    let mut rmg = 0usize;
    if let Ok(rd) = std::fs::read_dir(delonix_runtime_core::DELONIX_SLICE) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // `remove_dir` (não `_all`): só remove se estiver VAZIO — um cgroup
            // com processos lá dentro recusa, e ainda bem.
            if name.starts_with("delonix-") && !live_cg.contains(&name) && std::fs::remove_dir(e.path()).is_ok() {
                rmg += 1;
            }
        }
    }

    // 6) hostfwds órfãos no ingress — portas de host presas por containers que já
    //    morreram (ex.: o slirp deixou um hostfwd para trás). `live_ports` = as
    //    host-ports publicadas por containers VIVOS; o reaper remove todas as
    //    outras. Aqui é SEGURO (ao contrário do caso do reaper do PaaS num
    //    ingress partilhado): o `store` deste root É a fonte de verdade de quem
    //    publica no ingress.
    let live_ports: HashSet<u32> = store
        .list()?
        .iter()
        .filter(|c| c.pid.map(delonix_runtime::is_alive).unwrap_or(false))
        .flat_map(|c| c.ports.iter())
        .filter_map(|p| delonix_net::parse_publish(p).ok().and_then(|(hp, _, _)| hp.parse::<u32>().ok()))
        .collect();
    let rmh = delonix_net::infra::reap_orphan_hostfwds(&live_ports);
    // 7) slirps órfãos (alvo morto) — já reapados no topo por `reap_orphan_slirp`.

    // 8) redes `dlx-*` VAZIAS — auto-criadas para clusters que já foram apagados
    //    (uma rede de utilizador, sem o prefixo, NUNCA se toca aqui). Livra a
    //    sub-rede/bridge para reutilizar.
    let attached: HashSet<String> = store.list()?.iter().filter_map(|c| c.network.clone()).collect();
    let mut rmn = 0usize;
    if let Ok(nstore) = delonix_net::NetworkStore::open(super::util::state_root()) {
        if let Ok(nets) = nstore.list() {
            for n in nets {
                if n.name.starts_with("dlx-") && !attached.contains(&n.name) {
                    let _ = nstore.remove(&n.name);
                    delonix_net::infra::network_remove(&n.name);
                    rmn += 1;
                }
            }
        }
    }

    let total = freed + freed_dirs;
    println!(
        "removed: {rmc} container(s), {rmd} orphan dir(s), {rmi} image(s), {rmb} blob(s), {rmg} cgroup(s), {rmh} orphan port(s), {rmn} orphan network(s) — {} freed",
        super::output::fmt_size(total)
    );
    Ok(())
}

/// `system virt` — detecta virtualização e diz o que há a afinar. Sem `--tune`
/// não muda nada: lista as recomendações e o comando para as aplicar.
fn cmd_virt(tune: bool) -> Result<()> {
    use delonix_runtime_core::virt;
    let v = virt::detect();
    if !v.virtualized {
        println!("Delonix corre em hardware físico (bare-metal) — sem virtualização detetada.");
        println!("  Nenhuma afinação de VM a aplicar; o runtime já usa o hardware diretamente.");
        return Ok(());
    }
    let kvm = if v.is_kvm { "   ← KVM nativo: caminho de máximo desempenho disponível" } else { "" };
    println!("Virtualização detetada: {}{kvm}", v.hypervisor.to_uppercase());
    println!(
        "  Aceleração KVM (/dev/kvm): {}",
        if v.kvm_accel { "sim (virtualização aninhada possível)" } else { "não" }
    );
    let join = |xs: &[String], vazio: &str| if xs.is_empty() { vazio.to_string() } else { xs.join(", ") };
    println!("  Rede virtio-net: {}", join(&v.virtio_net, "(nenhuma)"));
    println!("  Disco virtio-blk: {}", join(&v.virtio_blk, "(nenhum)"));
    println!("  Dispositivos no bus virtio: {}", v.virtio_count);
    println!();
    if !v.virtio_net.is_empty() {
        println!(
            "  ✓ Rede paravirtualizada (virtio-net: {}) — offloads de segmentação/checksum no host.",
            v.virtio_net.join(", ")
        );
    }
    // A afinação concreta: escalonador de I/O 'none' nos discos virtio-blk — num
    // guest KVM, escalonar dos dois lados só acrescenta latência.
    let mut pending: Vec<String> = Vec::new();
    for dev in &v.virtio_blk {
        match virt::blk_scheduler(dev) {
            Some((cur, true)) if tune => match virt::set_blk_scheduler_none(dev) {
                Ok(_) => println!("  ✓ /dev/{dev}: escalonador de I/O '{cur}' → 'none' (o host KVM já escalona)"),
                Err(e) => println!("  ✗ /dev/{dev}: não consegui mudar o escalonador ({e}) — corre como root"),
            },
            Some((cur, true)) => {
                pending.push(format!("/dev/{dev}: escalonador de I/O '{cur}' → 'none' (evita escalonar 2× num guest KVM)"))
            }
            Some((cur, false)) => println!("  ✓ /dev/{dev}: escalonador de I/O já ótimo ({cur})"),
            None => {}
        }
    }
    if !tune {
        if pending.is_empty() {
            println!("\nSem afinações pendentes — esta VM já está otimizada para o Delonix.");
        } else {
            println!("\nAfinações recomendadas (corre `sudo delonix system virt --tune` para aplicar):");
            for p in &pending {
                println!("  • {p}");
            }
        }
    }
    Ok(())
}

/// `system thermal` — governador térmico sobre a slice de cgroup do Delonix.
fn cmd_thermal(high: u64, low: u64, floor: u64, interval: u64, once: bool) -> Result<()> {
    use delonix_runtime::{self as runtime};
    if high <= low {
        return Err(delonix_runtime_core::Error::Invalid("--high tem de ser maior que --low".into()));
    }
    if runtime::is_rootless() {
        return Err(delonix_runtime_core::Error::Invalid(
            "o governador térmico precisa de root (escreve no cgroup do host)".into(),
        ));
    }
    let mut scale = 100u64; // % do orçamento de CPU do Delonix
    runtime::set_slice_cpu_pct(scale);
    eprintln!("governador térmico: high={high}°C low={low}°C floor={floor}% (Ctrl-C para sair)");
    loop {
        let temp = runtime::max_cpu_temp_c().unwrap_or(0);
        if temp >= high && scale > floor {
            scale = floor.max(scale.saturating_sub(20));
            runtime::set_slice_cpu_pct(scale);
            let fan = if runtime::boost_fans() { " + ventoinha no máximo" } else { "" };
            println!("{temp}°C ≥ {high}°C — a arrefecer: CPU do Delonix a {scale}%{fan}");
        } else if temp <= low && scale < 100 {
            scale = 100.min(scale + 20);
            runtime::set_slice_cpu_pct(scale);
            println!("{temp}°C ≤ {low}°C — a restaurar: CPU do Delonix a {scale}%");
        } else if once {
            println!("{temp}°C (high={high}/low={low}) — CPU do Delonix a {scale}% (sem mudança)");
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(interval.max(1)));
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
