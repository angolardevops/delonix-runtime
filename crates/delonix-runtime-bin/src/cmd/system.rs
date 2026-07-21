//! `delonix system` — the engine itself: events, state and disk usage.
//!
//! It is a GROUP, not standalone commands: `events`/`info`/`df` are about the
//! engine, not about a particular container or image — just like docker
//! (`docker system ...`). Whatever is per-object stays in the object's group
//! (`container stats`, `image ls`).

use clap::Subcommand;
use delonix_runtime_core::{events, Result, Store};

use super::util::{open_stores, state_root};

#[derive(Subcommand)]
pub enum SystemCmd {
    /// Engine events (create/start/die/remove/…), from oldest to most
    /// recent. With no daemon, the log is a shared append-only file — each
    /// command appends its own line (see `delonix_runtime_core::events`).
    Events {
        /// Follow continuously (Ctrl-C to exit).
        #[arg(short, long)]
        follow: bool,
        /// Show only the last N (default: all).
        #[arg(short = 'n', long)]
        tail: Option<usize>,
    },
    /// Engine state: rootless?, cgroup delegation, network infra, counts.
    Info,
    /// Disk usage by area (images, containers, volumes, VM images).
    Df,
    /// Host virtualization: hypervisor, KVM, virtio — and what there is to tune.
    Virt {
        /// Apply the recommended tuning (needs root).
        #[arg(long)]
        tune: bool,
    },
    /// Reclaim space: remove stopped containers, unused images, CAS blobs
    /// nobody references, empty cgroups and — the biggest space saver
    /// — **orphan container directories** (from nodes/containers that died
    /// abruptly without `rm`, with no registry entry).
    Prune {
        /// Also remove unused images that DO have a tag (not just the dangling ones).
        #[arg(short, long)]
        all: bool,
    },
    /// Active network connections per container (via conntrack): who comes in,
    /// who goes out, and between containers. Refreshes continuously (see `--no-stream`).
    Monitor {
        /// Milliseconds between refreshes (minimum 300).
        #[arg(long, default_value_t = 1000)]
        interval: u64,
        /// One sample and exit (without clearing the screen or repeating).
        #[arg(long = "no-stream")]
        no_stream: bool,
    },
    /// Thermal governor: lowers Delonix's CPU budget when the CPU heats
    /// up and restores it when it cools down. Runs continuously (see `--once`).
    Thermal {
        /// Temperature (°C) at or above which it cools down.
        #[arg(long, default_value_t = 85)]
        high: u64,
        /// Temperature (°C) below which it restores.
        #[arg(long, default_value_t = 70)]
        low: u64,
        /// Minimum CPU percentage it drops to.
        #[arg(long, default_value_t = 40)]
        floor: u64,
        /// Seconds between readings.
        #[arg(long, default_value_t = 5)]
        interval: u64,
        /// One reading and exit (for cron/scripts, instead of the loop).
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
        SystemCmd::Monitor {
            interval,
            no_stream,
        } => cmd_monitor(interval, no_stream),
        SystemCmd::Virt { tune } => cmd_virt(tune),
        SystemCmd::Thermal {
            high,
            low,
            floor,
            interval,
            once,
        } => cmd_thermal(high, low, floor, interval, once),
    }
}

/// `system monitor` — active network connections per container, via conntrack.
///
/// Reads the host conntrack (`delonix_net::list_connections`), mapping each IP
/// to the name of the container that owns it, and classifies each connection: from
/// outside into a container (someone accessing), from a container to the outside (egress), or
/// between containers. Refreshes continuously unless `--no-stream`.
fn cmd_monitor(interval: u64, no_stream: bool) -> Result<()> {
    use delonix_runtime::is_alive;
    let (_images, store) = open_stores()?;
    loop {
        let conts = store.list().unwrap_or_default();
        let ip2name: std::collections::HashMap<String, String> = conts
            .iter()
            .filter(|c| c.pid.map(is_alive).unwrap_or(false))
            .filter_map(|c| c.ip.clone().map(|ip| (ip, c.name.clone())))
            .collect();
        let conns = delonix_net::list_connections(&ip2name);
        if !no_stream {
            print!("\x1b[2J\x1b[H"); // clear the screen
        }
        println!(
            "delonix monitor — {} {}, {} {}\n",
            ip2name.len(),
            super::po::t("containers"),
            conns.len(),
            super::po::t("active connections (conntrack)"),
        );
        if ip2name.is_empty() {
            println!(
                "  {}",
                super::output::dim(super::po::t("(no running containers with a network)"))
            );
        }
        let mut ext_in: Vec<&delonix_net::Connection> =
            conns.iter().filter(|c| c.kind == "external_in").collect();
        let mut egress: Vec<&delonix_net::Connection> =
            conns.iter().filter(|c| c.kind == "egress").collect();
        let internal: Vec<&delonix_net::Connection> =
            conns.iter().filter(|c| c.kind == "internal").collect();
        ext_in.sort_by(|a, b| a.container.cmp(&b.container));
        egress.sort_by(|a, b| a.container.cmp(&b.container));
        if !ext_in.is_empty() {
            println!(
                "  ⬇ {}",
                super::po::t("INBOUND → CONTAINER (external access)")
            );
            for c in &ext_in {
                println!(
                    "    {:<22} ← {}:{}/{}",
                    c.container, c.peer, c.port, c.proto
                );
            }
            println!();
        }
        if !egress.is_empty() {
            println!("  ⬆ {}", super::po::t("CONTAINER → OUTBOUND (egress)"));
            for c in &egress {
                println!(
                    "    {:<22} → {}:{}/{}",
                    c.container, c.peer, c.port, c.proto
                );
            }
            println!();
        }
        if !internal.is_empty() {
            println!("  ⇄ {}", super::po::t("BETWEEN CONTAINERS"));
            for c in &internal {
                println!("    {} ↔ {}", c.container, c.peer);
            }
        }
        if conns.is_empty() && !ip2name.is_empty() {
            println!(
                "  {}",
                super::output::dim(super::po::t("(no active connections right now)"))
            );
        }
        if no_stream {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(interval.max(300)));
    }
}

/// `system prune` — reclaims disk space.
///
/// Order matters: stopped containers first (they free images and blobs),
/// then whatever is no longer referenced. The step that frees the most is **4**,
/// the orphan directories — the real problem measured on this machine: **88
/// container directories on disk against 4 in the registry (~36 GiB)**. They come from
/// cluster nodes and containers that died from SIGKILL/crash/closed-session **without
/// `rm`**, so nobody ever swept them. The normal `container rm` never
/// catches them (they aren't in the registry); only an explicit GC like this one.
fn cmd_prune(all: bool) -> Result<()> {
    use std::collections::HashSet;
    let (images, store) = open_stores()?;

    // Orphan slirps (dead target) — the SAFE reaper (never the fail-open
    // `reap_orphan_hostfwds`; see the history of the reaper that deleted live ports).
    let reaped = delonix_net::reap_orphan_slirp();
    if reaped > 0 {
        println!(
            "{}",
            super::po::tf(
                "net: {n} orphan slirp(s) reaped",
                &[("n", &reaped.to_string())]
            )
        );
    }

    // 1) stopped containers (in the registry).
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

    // Ids still alive AFTER step 1 — the basis for deciding what is orphan.
    let live_ids: HashSet<String> = store.list()?.iter().map(|c| c.id.clone()).collect();

    // 1b) orphan ingress ref markers — the "16 refs with 3 live
    //     containers" leak. A container that dies from SIGKILL/crash without `rm` leaves its
    //     ref marker holding the shared infra forever. `live` = ids
    //     of running containers + the CRI pods (`cri-*`) and VMs (`vm-*`), managed
    //     by other stores — preserved, never reaped here. The reaper frees
    //     only the markers with no live owner and tears down the infra if it becomes empty; it NEVER
    //     touches a live id.
    let mut live_refs: HashSet<String> = store
        .list()?
        .iter()
        .filter(|c| c.pid.map(delonix_runtime::is_alive).unwrap_or(false))
        .map(|c| c.id.clone())
        .collect();
    for id in delonix_net::infra::attached_refs() {
        if id.starts_with("cri-") || id.starts_with("vm-") {
            live_refs.insert(id);
        }
    }
    let reaped_refs = delonix_net::infra::reap_orphan_refs(&live_refs);
    if reaped_refs > 0 {
        println!(
            "{}",
            super::po::tf(
                "net: {n} orphan ingress ref(s) reaped",
                &[("n", &reaped_refs.to_string())]
            )
        );
    }

    // 2) dangling images (no tag), or all unused ones with `-a`.
    let in_use: HashSet<String> = store.list()?.iter().map(|c| c.image.clone()).collect();
    let mut rmi = 0usize;
    for img in images.list()? {
        let dangling =
            img.repo_tags.is_empty() || img.repo_tags.iter().all(|t| t.contains("<none>"));
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

    // 3) CAS blobs that nobody references anymore.
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

    // 4) orphan container DIRECTORIES — the big space reclaimer.
    //
    // A `<containers>/<id>/` whose `<id>` is no longer in the registry: the container
    // died without `rm`. We use `remove_tree_mapped` and not `remove_dir_all` because
    // the rootfs may hold SUBUID files (written by a rootless container)
    // that the real user cannot delete directly — it is exactly the path that
    // this series' `__rmtree` came to actually support.
    let containers_dir = images.root().join("containers");
    let (mut rmd, mut freed_dirs) = (0usize, 0u64);
    for path in orphan_container_dirs(&containers_dir, &live_ids) {
        freed_dirs += dir_size(&path);
        delonix_runtime::remove_tree_mapped(&path);
        rmd += 1;
    }

    // 5) orphan EMPTY cgroups in delonix.slice.
    let live_cg: HashSet<String> = live_ids.iter().map(|id| format!("delonix-{id}")).collect();
    let mut rmg = 0usize;
    if let Ok(rd) = std::fs::read_dir(delonix_runtime_core::DELONIX_SLICE) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // `remove_dir` (not `_all`): only removes if EMPTY — a cgroup
            // with processes inside refuses, and rightly so.
            if name.starts_with("delonix-")
                && !live_cg.contains(&name)
                && std::fs::remove_dir(e.path()).is_ok()
            {
                rmg += 1;
            }
        }
    }

    // 6) orphan hostfwds in the ingress — host ports held by containers that already
    //    died (e.g.: slirp left a hostfwd behind). `live_ports` = the
    //    host ports published by LIVE containers; the reaper removes all the
    //    others. Here it is SAFE (unlike the PaaS reaper case on a
    //    shared ingress): this root's `store` IS the source of truth about who
    //    publishes on the ingress.
    let live_ports: HashSet<u32> = store
        .list()?
        .iter()
        .filter(|c| c.pid.map(delonix_runtime::is_alive).unwrap_or(false))
        .flat_map(|c| c.ports.iter())
        .filter_map(|p| {
            delonix_net::parse_publish(p)
                .ok()
                .and_then(|(hp, _, _)| hp.parse::<u32>().ok())
        })
        .collect();
    let rmh = delonix_net::infra::reap_orphan_hostfwds(&live_ports);
    // 7) orphan slirps (dead target) — already reaped at the top by `reap_orphan_slirp`.

    // 8) EMPTY `dlx-*` networks — auto-created for clusters that have been deleted
    //    (a user network, without the prefix, is NEVER touched here). Frees the
    //    subnet/bridge for reuse.
    let attached: HashSet<String> = store
        .list()?
        .iter()
        .filter_map(|c| c.network.clone())
        .collect();
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
        "{}",
        super::po::tf(
            "removed: {c} container(s), {d} orphan dir(s), {i} image(s), {b} blob(s), {g} cgroup(s), {p} orphan port(s), {n} orphan network(s) — {size} freed",
            &[
                ("c", &rmc.to_string()),
                ("d", &rmd.to_string()),
                ("i", &rmi.to_string()),
                ("b", &rmb.to_string()),
                ("g", &rmg.to_string()),
                ("p", &rmh.to_string()),
                ("n", &rmn.to_string()),
                ("size", &super::output::fmt_size(total)),
            ]
        )
    );
    Ok(())
}

/// `system virt` — detects virtualization and says what to tune. Without `--tune`
/// it changes nothing: it lists the recommendations and the command to apply them.
fn cmd_virt(tune: bool) -> Result<()> {
    use delonix_runtime_core::virt;
    let v = virt::detect();
    if !v.virtualized {
        println!(
            "{}",
            super::po::t(
                "Delonix runs on physical hardware (bare-metal) — no virtualization detected."
            )
        );
        println!(
            "  {}",
            super::po::t(
                "No VM tuning to apply; the runtime already talks to the hardware directly."
            )
        );
        return Ok(());
    }
    let kvm = if v.is_kvm {
        "   ← KVM nativo: caminho de máximo desempenho disponível"
    } else {
        ""
    };
    println!(
        "{}: {}{kvm}",
        super::po::t("Detected virtualization"),
        v.hypervisor.to_uppercase()
    );
    println!(
        "  {}: {}",
        super::po::t("KVM acceleration (/dev/kvm)"),
        if v.kvm_accel {
            super::po::t("yes (nested virtualization possible)")
        } else {
            super::po::t("no")
        }
    );
    let join = |xs: &[String], vazio: &str| {
        if xs.is_empty() {
            vazio.to_string()
        } else {
            xs.join(", ")
        }
    };
    println!(
        "  {}: {}",
        super::po::t("virtio-net network"),
        join(&v.virtio_net, super::po::t("(none)"))
    );
    println!(
        "  {}: {}",
        super::po::t("virtio-blk disk"),
        join(&v.virtio_blk, super::po::t("(none)"))
    );
    println!(
        "  {}: {}",
        super::po::t("Devices on the virtio bus"),
        v.virtio_count
    );
    println!();
    if !v.virtio_net.is_empty() {
        println!(
            "  ✓ {}",
            super::po::tf(
                "Paravirtualized network (virtio-net: {ifs}) — segmentation/checksum offloads on the host.",
                &[("ifs", &v.virtio_net.join(", "))]
            )
        );
    }
    // The concrete tuning: I/O scheduler 'none' on virtio-blk disks — in a
    // KVM guest, scheduling on both sides only adds latency.
    let mut pending: Vec<String> = Vec::new();
    for dev in &v.virtio_blk {
        match virt::blk_scheduler(dev) {
            Some((cur, true)) if tune => match virt::set_blk_scheduler_none(dev) {
                Ok(_) => println!(
                    "  ✓ /dev/{dev}: {}",
                    super::po::tf(
                        "I/O scheduler '{cur}' → 'none' (the KVM host already schedules)",
                        &[("cur", &cur)]
                    )
                ),
                Err(e) => println!(
                    "  ✗ /dev/{dev}: {}",
                    super::po::tf(
                        "could not change the scheduler ({err}) — run as root",
                        &[("err", &e.to_string())]
                    )
                ),
            },
            Some((cur, true)) => pending.push(format!(
                "/dev/{dev}: {}",
                super::po::tf(
                    "I/O scheduler '{cur}' → 'none' (avoids double scheduling in a KVM guest)",
                    &[("cur", &cur)]
                )
            )),
            Some((cur, false)) => println!(
                "  ✓ /dev/{dev}: {}",
                super::po::tf("I/O scheduler already optimal ({cur})", &[("cur", &cur)])
            ),
            None => {}
        }
    }
    if !tune {
        if pending.is_empty() {
            println!(
                "\n{}",
                super::po::t("No pending tuning — this VM is already optimized for Delonix.")
            );
        } else {
            println!(
                "\n{}",
                super::po::t(
                    "Recommended tuning (run `sudo delonix system virt --tune` to apply):"
                )
            );
            for p in &pending {
                println!("  • {p}");
            }
        }
    }
    Ok(())
}

/// `system thermal` — thermal governor over Delonix's cgroup slice.
fn cmd_thermal(high: u64, low: u64, floor: u64, interval: u64, once: bool) -> Result<()> {
    use delonix_runtime::{self as runtime};
    if high <= low {
        return Err(delonix_runtime_core::Error::Invalid(
            super::po::t("--high must be greater than --low").into(),
        ));
    }
    if runtime::is_rootless() {
        return Err(delonix_runtime_core::Error::Invalid(
            super::po::t("the thermal governor needs root (it writes to the host cgroup)").into(),
        ));
    }
    let mut scale = 100u64; // % of Delonix's CPU budget
    runtime::set_slice_cpu_pct(scale);
    eprintln!(
        "{}: high={high}°C low={low}°C floor={floor}% (Ctrl-C {})",
        super::po::t("thermal governor"),
        super::po::t("to exit")
    );
    loop {
        let temp = runtime::max_cpu_temp_c().unwrap_or(0);
        if temp >= high && scale > floor {
            scale = floor.max(scale.saturating_sub(20));
            runtime::set_slice_cpu_pct(scale);
            let fan = if runtime::boost_fans() {
                super::po::t(" + fans at max")
            } else {
                ""
            };
            println!(
                "{temp}°C ≥ {high}°C — {}: {}{fan}",
                super::po::t("cooling down"),
                super::po::tf("Delonix CPU at {pct}%", &[("pct", &scale.to_string())])
            );
        } else if temp <= low && scale < 100 {
            scale = 100.min(scale + 20);
            runtime::set_slice_cpu_pct(scale);
            println!(
                "{temp}°C ≤ {low}°C — {}: {}",
                super::po::t("restoring"),
                super::po::tf("Delonix CPU at {pct}%", &[("pct", &scale.to_string())])
            );
        } else if once {
            println!(
                "{temp}°C (high={high}/low={low}) — {} ({})",
                super::po::tf("Delonix CPU at {pct}%", &[("pct", &scale.to_string())]),
                super::po::t("no change")
            );
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
    // `-f`: polls the file's growth. With no daemon there is no push — but the
    // cost is one `stat` per second, and the log is the only source of truth.
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

/// **PURE** — subdirectories (name = id) of `containers_dir` whose id is NOT in
/// `live` (registered containers): the orphans to reap. The reapable core of step 4
/// of `prune`, isolated from `remove_tree_mapped` (which needs subuid) so it can
/// be tested dry, without privilege. Only directories count — registry
/// entries are `<id>.json` files and never enter here. **It never returns a live
/// id.**
fn orphan_container_dirs(
    containers_dir: &std::path::Path,
    live: &std::collections::HashSet<String>,
) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(containers_dir) {
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = e.file_name().to_string_lossy().into_owned();
            if !live.contains(&id) {
                out.push(e.path());
            }
        }
    }
    out
}

/// Recursive sum of a directory's size (apparent, like `du`).
fn dir_size(p: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| {
            let path = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => dir_size(&path),
                Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => 0, // symlinks don't count (they would count twice)
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

/// `system df` — where the disk is. It exists for a concrete reason: orphan
/// rootfs dirs once piled up 45 GiB with nothing reporting them, until the kubelet marked
/// the node with `disk-pressure`. The `RECLAIMABLE` column is the one that matters.
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

    println!(
        "{:<16}  {:>10}  {:>12}",
        super::po::t("AREA"),
        super::po::t("SIZE"),
        super::po::t("RECLAIMABLE")
    );
    for (label, dir) in [
        (super::po::t("images"), root.join("blobs")),
        ("layers", root.join("layers")),
        ("containers", containers_dir.clone()),
        ("volumes", root.join("volumes")),
        (super::po::t("VM images"), root.join("vm-images")),
    ] {
        let size = dir_size(&dir);
        let recl = if label == "containers" {
            human(orphan)
        } else {
            "-".to_string()
        };
        println!("{label:<16}  {:>10}  {recl:>12}", human(size));
    }
    if orphan_n > 0 {
        println!(
            "\n{}",
            super::po::tf(
                "{n} orphan container dir(s) — {size} reclaimable.\nLeftovers from abruptly killed containers (a normal `rm` cleans them).",
                &[("n", &orphan_n.to_string()), ("size", &human(orphan))]
            )
        );
    }
    Ok(())
}

/// `system info` — what the engine IS on this machine. Without it, diagnosing
/// "why the limits don't apply" or "why `-p` fails"
/// forces reading code.
fn cmd_info() -> Result<()> {
    let (_, store) = open_stores()?;
    let cs = store.list()?;
    let running = cs
        .iter()
        .filter(|c| matches!(c.status, delonix_runtime_core::Status::Running))
        .count();

    println!("Delonix Runtime {}", env!("CARGO_PKG_VERSION"));
    println!(
        "  {:<19} {}",
        super::po::t("state root:"),
        state_root().display()
    );
    let rootless = delonix_runtime::is_rootless();
    println!(
        "  {:<19} {}",
        super::po::t("mode:"),
        if rootless {
            super::po::t("rootless (daemonless)")
        } else {
            super::po::t("root (daemonless)")
        }
    );
    // This is the #1 question when the limits "don't work".
    let delegated = std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists()
        && std::fs::read_to_string("/sys/fs/cgroup/cgroup.subtree_control")
            .map(|s| s.contains("memory"))
            .unwrap_or(false);
    println!(
        "  {:<19} {}",
        super::po::t("cgroup2 delegated:"),
        if delegated {
            super::po::t("yes")
        } else {
            super::po::t("no — memory/cpu/pids are NOT enforced (run under systemd-run --user --scope -p Delegate=yes)")
        }
    );
    let infra = delonix_net::infra::status();
    println!(
        "  {:<19} {}",
        super::po::t("network infra:"),
        match infra.holder_pid {
            Some(p) => super::po::tf("up (holder pid {pid})", &[("pid", &p.to_string())]),
            None => super::po::t("down (comes up on demand)").to_string(),
        }
    );
    println!(
        "  {:<19} {} ({running} {})",
        super::po::t("containers:"),
        cs.len(),
        super::po::t("running")
    );
    println!(
        "  {:<19} {}",
        super::po::t("events:"),
        events::read(&state_root()).len()
    );
    Ok(())
}

/// Shortcut for the `Store` — `system` deals in counts, not lifecycle.
#[allow(dead_code)]
fn store_only() -> Result<Store> {
    Store::open(Store::default_root())
}

#[cfg(test)]
mod tests {
    use super::orphan_container_dirs;
    use std::collections::HashSet;
    use std::path::PathBuf;

    /// Unique temp dir (without depending on the `tempfile` crate).
    fn tmp_dir(tag: &str) -> PathBuf {
        // SAFETY: getpid() has no preconditions.
        let uniq = format!(
            "delonix-prune-{tag}-{}-{}",
            unsafe { libc::getpid() },
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let d = std::env::temp_dir().join(uniq);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// STRESS test of the orphan-rootfs reaper: create→destroy of N container
    /// directories at the disk level, crossed with the "Store" (set of live
    /// ids). Asserts that the reaper catches ALL the orphans (containers killed without
    /// `rm`), preserves the live ones, and that after deleting them ZERO orphans remain.
    /// Runs without privilege — it tests the DECISION (`orphan_container_dirs`), not
    /// `remove_tree_mapped` (which needs subuid).
    #[test]
    fn stress_reaper_rootfs_orfaos_deixa_zero() {
        const N: usize = 300;
        let root = tmp_dir("rootfs");
        let containers = root.join("containers");
        std::fs::create_dir_all(&containers).unwrap();

        // N dead container directories + M live ones, and some `<id>.json`
        // files (registry entries) that are NOT directories and must be
        // ignored by the reaper.
        for i in 0..N {
            std::fs::create_dir_all(containers.join(format!("dead{i}"))).unwrap();
        }
        let live: HashSet<String> = (0..5).map(|i| format!("alive{i}")).collect();
        for id in &live {
            let d = containers.join(id);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("rootfs-marker"), b"x").unwrap();
        }
        std::fs::write(containers.join("alive0.json"), b"{}").unwrap();
        std::fs::write(containers.join("dead0.json"), b"{}").unwrap();

        // The reaper sees exactly the N orphans (none live, no files).
        let orphans = orphan_container_dirs(&containers, &live);
        assert_eq!(
            orphans.len(),
            N,
            "todos os `dead*` são órfãos, ficheiros ignorados"
        );
        for id in &live {
            let p = containers.join(id);
            assert!(!orphans.contains(&p), "container vivo NUNCA é reapado");
        }

        // Delete them and reconfirm: ZERO orphans remain, the live ones intact.
        for p in &orphans {
            std::fs::remove_dir_all(p).unwrap();
        }
        assert!(
            orphan_container_dirs(&containers, &live).is_empty(),
            "após o reap, zero directórios órfãos"
        );
        for id in &live {
            assert!(containers.join(id).is_dir(), "vivo preservado no disco");
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
