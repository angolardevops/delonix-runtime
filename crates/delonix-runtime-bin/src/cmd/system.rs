//! `delonix system` — the engine itself: events, state and disk usage.
//!
//! It is a GROUP, not standalone commands: `events`/`info`/`df` are about the
//! engine, not about a particular container or image — just like docker
//! (`docker system ...`). Whatever is per-object stays in the object's group
//! (`container stats`, `image ls`).

use clap::Subcommand;
use delonix_runtime::{self as runtime};
use delonix_runtime_core::{events, Error, Result, Store};

use super::util::{open_stores, state_root};

#[derive(Subcommand)]
pub enum SystemCmd {
    /// Engine events (create/start/die/remove/…), from oldest to most recent.
    ///
    /// With no daemon, the log is a shared append-only file — each command
    /// appends its own line (see `delonix_runtime_core::events`).
    Events {
        /// Follow continuously (Ctrl-C to exit).
        #[arg(short, long)]
        follow: bool,
        /// Show only the last N (default: all).
        #[arg(short = 'n', long)]
        tail: Option<usize>,
        /// Output format: `table` (default) or `json` (ADR-0005). With `-f`, `json` streams ONE OBJECT PER LINE (JSONL) — a JSON array would never close on a stream
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: super::output::OutputFormat,
    },
    /// Engine state: rootless?, cgroup delegation, network infra, counts.
    Info,
    /// Diagnose — and, with `--delegate`, fix — cgroup delegation.
    ///
    /// It is the prerequisite for `--memory`/`--cpus`/`--pids-limit` to mean
    /// anything. Without it the flags are ACCEPTED and INERT: the container
    /// runs with no limit at all. That is the failure mode this command exists
    /// for, and it is invisible without asking.
    Setup {
        /// Apply the fix instead of only reporting it. Writing the system-wide
        /// drop-in needs root; the per-session remedy never does.
        #[arg(long)]
        delegate: bool,
    },
    /// Disk usage by area (images, containers, volumes, VM images).
    Df,
    /// Save this node's state into one archive.
    ///
    /// By default it packs what CANNOT be reconstructed: the registries, IPAM,
    /// secrets, `auth.json`, cluster PKI, HTTPRoute config and the event log.
    /// The heavy, re-obtainable areas are opt-in, and the live plumbing of a
    /// running node (pidfiles, sockets, locks) never travels — a resurrected
    /// `holder.pid` makes the engine report a holder that does not exist.
    Backup {
        /// Where to write the archive (default: `delonix-backup-<UTC>.tar.gz`
        /// in the current directory). Always created 0600.
        #[arg(short, long)]
        output: Option<String>,
        /// Also pack the volumes' DATA, via the same mapped snapshot
        /// `volumes snapshot` uses. This is the only area that can be hundreds
        /// of GiB.
        #[arg(long)]
        volumes: bool,
        /// Also pack the OCI images (blobs, layers and index) — content-addressed
        /// cache that a registry can serve again.
        #[arg(long)]
        images: bool,
        /// Also pack the VM disk images (gigabytes of qcow2 that `vm pull`
        /// re-fetches). Their metadata always travels.
        #[arg(long = "vm-images")]
        vm_images: bool,
        /// Also pack the vault MASTER KEY. Without it the secrets restore onto
        /// another node as bytes that never decrypt; with it, the archive IS
        /// the vault.
        #[arg(long = "include-master-key")]
        include_master_key: bool,
    },
    /// Put a node's state back from a `system backup` archive.
    ///
    /// Destructive on purpose: the covered set becomes exactly what the archive
    /// holds. It verifies the whole archive BEFORE touching anything, refuses a
    /// format it does not know, refuses to run over live workloads without
    /// `--force`, and proves afterwards that the restored secrets decrypt.
    Restore {
        /// The archive written by `system backup`.
        archive: String,
        /// Restore even with containers/VMs still running (their registry is
        /// replaced underneath them).
        #[arg(short = 'f', long)]
        force: bool,
        /// Say what would change and stop, without writing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Host virtualization: hypervisor, KVM, virtio — and what there is to tune.
    Virt {
        /// Apply the recommended tuning (needs root).
        #[arg(long)]
        tune: bool,
    },
    /// Reclaim space taken by what nothing uses any more.
    ///
    /// Removes stopped containers, unused images, CAS blobs nobody
    /// references, empty cgroups and — the biggest space saver — **orphan
    /// container directories** (from nodes/containers that died abruptly
    /// without `rm`, with no registry entry).
    Prune {
        /// Skip the confirmation prompt (REQUIRED when stdin is not a terminal).
        #[arg(short = 'f', long)]
        force: bool,
        /// Also remove unused images that DO have a tag (not just the dangling ones).
        #[arg(short, long)]
        all: bool,
    },
    /// Active network connections per container (via conntrack).
    ///
    /// Who comes in, who goes out, and between containers. Refreshes
    /// continuously (see `--no-stream`).
    Monitor {
        /// Milliseconds between refreshes (minimum 300).
        #[arg(long, default_value_t = 1000)]
        interval: u64,
        /// One sample and exit (without clearing the screen or repeating).
        #[arg(long = "no-stream")]
        no_stream: bool,
    },
    /// Thermal governor for Delonix's CPU budget. Runs continuously (see
    /// `--once`).
    ///
    /// Lowers the budget when the CPU heats up and restores it when it cools
    /// down.
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
        SystemCmd::Events {
            follow,
            tail,
            output,
        } => cmd_events(follow, tail, output),
        SystemCmd::Info => cmd_info(),
        SystemCmd::Setup { delegate } => cmd_setup(delegate),
        SystemCmd::Df => cmd_df(),
        SystemCmd::Backup {
            output,
            volumes,
            images,
            vm_images,
            include_master_key,
        } => super::backup::cmd_backup(
            output,
            super::backup::Scope {
                volumes,
                images,
                vm_images,
                master_key: include_master_key,
            },
            &state_root(),
        ),
        SystemCmd::Restore {
            archive,
            force,
            dry_run,
        } => super::backup::cmd_restore(&archive, force, dry_run, &state_root()),
        SystemCmd::Prune { all, force } => cmd_prune(all, force),
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
fn cmd_prune(all: bool, force: bool) -> Result<()> {
    use std::collections::HashSet;
    let (images, store) = open_stores()?;

    // CONFIRM FIRST. This is destructive well beyond what its name suggests: the
    // help leads with "unused images", but step 1 below removes EVERY stopped
    // container — including ones merely `Created` and not yet started — with no
    // prompt at all. Docker asks. An operator who types `delonix system prune`
    // expecting a disk cleanup should not discover afterwards that a stopped
    // container they were about to `start` is gone.
    //
    // Only when stdin is a TTY: in a script/CI there is nobody to answer, and
    // blocking forever would be worse than the old behaviour — there, `--force`
    // is required instead, so an unattended prune is always explicit.
    if !force {
        let doomed: Vec<String> = store
            .list()?
            .into_iter()
            .filter(|c| !c.pid.map(delonix_runtime::is_alive).unwrap_or(false))
            .map(|c| c.name)
            .collect();
        let tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        if !tty {
            return Err(delonix_runtime_core::Error::Invalid(
                super::po::t(
                    "`system prune` removes every stopped container and unreferenced data — pass \
                     --force to confirm when not on a terminal",
                )
                .into(),
            ));
        }
        if !doomed.is_empty() {
            println!(
                "{}",
                super::po::tf(
                    "This will remove {n} stopped container(s): {list}",
                    &[
                        ("n", &doomed.len().to_string()),
                        ("list", &doomed.join(", ")),
                    ],
                )
            );
        }
        print!(
            "{} ",
            super::po::t(
                "Also removes unused images, CAS blobs and orphan directories. Continue? [y/N]"
            )
        );
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err()
            || !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
        {
            println!("{}", super::po::t("aborted"));
            return Ok(());
        }
    }

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
    // Safe to assert authoritative here, and only here: this root's `store` IS
    // the source of truth for who publishes on this ingress, and `store.list()`
    // above propagates its error rather than yielding an empty list on failure.
    let rmh = delonix_net::infra::reap_orphan_hostfwds(
        delonix_net::infra::AuthoritativeLivePorts::new(&live_ports),
    );
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
        super::po::t("   ← native KVM: maximum-performance path available")
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

/// The engine's event log, for humans (`table`) or for machines (`json`).
///
/// `-o json` closes a gap that was pure friction: the log is ALREADY
/// `events.jsonl` on disk, and the only reader of it re-rendered every record
/// into a padded human line. Anyone automating on top of the engine — the
/// audience `cli-stability.md` promises `-o json` to — had to either parse
/// columns or reach behind the CLI into the state directory, which is not a
/// contract this project makes.
///
/// **`-f` streams JSONL, not an array**, and that is deliberate: a JSON array
/// has to close, and a follow never does. One object per line is what a stream
/// consumer (`jq -c`, a log shipper) can read incrementally, and it is the same
/// shape the file itself already has. The non-following form keeps the ADR-0005
/// array contract that every other `-o json` in this CLI honours.
fn cmd_events(
    follow: bool,
    tail: Option<usize>,
    output: super::output::OutputFormat,
) -> Result<()> {
    use super::output::OutputFormat;
    let root = state_root();
    let evs = events::read(&root);
    let start = tail.map(|n| evs.len().saturating_sub(n)).unwrap_or(0);
    let shown = &evs[start..];
    match output {
        OutputFormat::Table => {
            for e in shown {
                println!("{}", e.to_line());
            }
        }
        // In follow mode the backlog is streamed as JSONL too, so a consumer
        // never has to switch parsers halfway through the same invocation.
        OutputFormat::Json if follow => {
            for e in shown {
                print_event_line(e);
            }
        }
        OutputFormat::Json => super::output::print_json(shown)?,
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
            match output {
                OutputFormat::Table => println!("{}", e.to_line()),
                OutputFormat::Json => print_event_line(&e),
            }
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

/// One event as a single JSON line (JSONL). A record that fails to serialize is
/// SKIPPED rather than printed half-formed: a consumer reading line by line
/// would take a truncated object as a parse error for the whole stream.
fn print_event_line(e: &delonix_runtime_core::events::Event) {
    if let Ok(s) = serde_json::to_string(e) {
        println!("{s}");
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
/// Recursive disk usage of a directory, `du`-style — the number behind
/// `system df` and the `system prune` reclaim figures.
///
/// BUG FIXED HERE: this was one of THREE private copies of the same walk, all
/// of which summed the *apparent* size and counted hardlinked files once per
/// name, while describing themselves as `du`. Against real `du` on a ~94 GiB
/// store the error measured **+4.9 %** — and it is the number `system df`
/// prints when an operator is deciding whether the disk is filling up, which
/// on this engine has already caused a real kubelet disk-pressure incident.
///
/// Now delegates to `delonix-volume`'s corrected walk (allocated blocks,
/// `(dev, ino)` deduplication), the single implementation shared with the
/// volume quota and the dashboard/Prometheus collector, so the three can no
/// longer drift. Symlinks are still not followed and not counted.
fn dir_size(p: &std::path::Path) -> u64 {
    delonix_volume::measure(p).bytes
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
    // This is the #1 question when the limits "don't work" — so it has to be
    // answered about THIS session, not about the host in general.
    //
    // BUG FIXED HERE: it used to read `/sys/fs/cgroup/cgroup.controllers` and
    // `/sys/fs/cgroup/cgroup.subtree_control` — the files of the HOST'S ROOT
    // cgroup, which on any systemd machine list `memory` regardless. So this
    // reported `cgroup2 delegated: yes` unconditionally, including on the plain
    // SSH session where all five limits were measured to be silently inert. The
    // one command a user runs to diagnose "why aren't my limits working" gave the
    // confidently wrong answer.
    //
    // `cgroup_limits_apply` asks the engine's own question, about the base
    // `spawn` would actually use.
    let delegated = delonix_runtime::cgroup_limits_apply();
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

/// Is this cgroup a LOGIN session scope — the case where the system-wide
/// drop-in does not help?
///
/// **Pure, and the subtle half of the whole command.** systemd puts an SSH or
/// tty login in `user-<uid>.slice/session-<n>.scope`, a SIBLING of
/// `user@<uid>.service`, so `Delegate=yes` on the latter never reaches it —
/// measured on this host, where the session scope's `cgroup.subtree_control` is
/// `root:root` while the user manager's is the user's own. Telling someone in
/// that shell to write a drop-in and log back in is the answer that wastes an
/// afternoon.
///
/// A scope UNDER `user@.service` (`systemd-run --user --scope`, an app scope) is
/// a different animal: it inherits the delegation and needs no warning.
pub(crate) fn is_login_session_scope(cgroup: &str) -> bool {
    cgroup
        .rsplit('/')
        .find(|c| !c.is_empty())
        .map(|leaf| leaf.starts_with("session-") && leaf.ends_with(".scope"))
        .unwrap_or(false)
}

/// The system-wide drop-in that gives every user's `user@.service` a delegated
/// cgroup. System scope, so it needs root — and it is the only half that
/// persists across reboots.
const DELEGATE_DROPIN: &str = "/etc/systemd/system/user@.service.d/50-delonix-delegate.conf";
const DELEGATE_BODY: &str = "# Written by `delonix system setup --delegate`.\n\
                             # Gives each user's systemd manager a delegated cgroup subtree, so\n\
                             # rootless containers can carry --memory/--cpus/--pids-limit AND a\n\
                             # Kubernetes node can boot (its entrypoint refuses without `cpu`).\n\
                             #\n\
                             # The controllers are NAMED rather than `Delegate=yes`: on this host\n\
                             # `yes` produced only `memory pids`, which passes every check the\n\
                             # engine makes and still kills a kind node at boot.\n\
                             [Service]\n\
                             Delegate=cpu cpuset io memory pids\n";

/// `system setup [--delegate]` — the one command for cgroup delegation.
///
/// It exists because the engine's honest warning ("no delegation, limits will
/// not apply") left the user with a research problem: the fix is a systemd
/// drop-in whose location, scope and reboot semantics are not obvious, and the
/// answer differs for the CURRENT shell and for future ones.
///
/// **Two remedies, because there are two problems**, and this is the part every
/// StackOverflow answer gets half of. A drop-in on `user@.service` fixes user
/// services and future logins. It does NOT fix the shell you are typing in over
/// SSH: a `session-N.scope` is a SIBLING of `user@.service`, not a child, so it
/// inherits nothing from it — measured on this host, not assumed. For the live
/// session the only answer is a delegated scope of your own.
/// The cgroup v2 controllers actually available in `cgroup`.
///
/// **Reading them is the half the diagnostic was missing.** `cgroup_limits_apply`
/// answers "can I make a child and write `subtree_control`" — which is necessary
/// and says nothing about WHICH knobs exist. A host can pass that check with only
/// `memory pids` delegated, and then a Kubernetes node dies at boot with
/// `UserNS: cpu controller needs to be delegated` while `system setup` reports
/// everything fine. Measured on this host.
fn delegated_controllers(cgroup: &str) -> Vec<String> {
    std::fs::read_to_string(format!("{cgroup}/cgroup.controllers"))
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Controllers whose ABSENCE stops a Kubernetes node from booting.
///
/// Only `cpu`. Measured: `kindest/node`'s entrypoint exits with
/// `UserNS: cpu controller needs to be delegated` and nothing else in that list
/// is checked by it. This is deliberately narrower than "what a node would
/// like" — see [`NICE_CONTROLLERS`].
const FATAL_CONTROLLERS: &[&str] = &["cpu"];

/// Controllers a node uses when they are there and lives without when they are
/// not.
///
/// `cpuset` and `io` are missing on plenty of hosts where a node boots fine —
/// on Ubuntu 24.04 the root passes them down but `user.slice` does not, and
/// that `subtree_control` belongs to root, so NO drop-in on `user@.service` can
/// conjure them. Reporting their absence as a failure would send people to edit
/// `/etc` for something that was never going to work and that they do not need.
const NICE_CONTROLLERS: &[&str] = &["cpuset", "io", "memory", "pids"];

/// Splits the missing controllers into the ones that BREAK a Kubernetes node
/// and the ones that merely limit it. **Pure**, so the rule is testable without
/// a cgroup tree.
///
/// The previous version returned one flat list and the caller printed the `cpu`
/// error text next to it — so a host missing only `cpuset`/`io` was told
/// `cluster create` would fail with a message quoting a controller it actually
/// had. Two different facts wearing the same sentence.
pub(crate) fn missing_controllers(have: &[String]) -> (Vec<&'static str>, Vec<&'static str>) {
    let absent = |set: &[&'static str]| -> Vec<&'static str> {
        set.iter()
            .copied()
            .filter(|c| !have.iter().any(|h| h == c))
            .collect()
    };
    (absent(FATAL_CONTROLLERS), absent(NICE_CONTROLLERS))
}

fn cmd_setup(delegate: bool) -> Result<()> {
    let rootless = runtime::is_rootless();
    let ok = runtime::cgroup_limits_apply();
    let cur = runtime::current_cgroup_v2().unwrap_or_else(|| "<unknown>".into());

    println!("{}", super::po::t("cgroup delegation"));
    println!("  mode:     {}", if rootless { "rootless" } else { "root" });
    println!("  cgroup:   {cur}");
    println!(
        "  limits:   {}",
        if ok {
            super::po::t("APPLY — --memory/--cpus/--pids-limit take effect")
        } else {
            super::po::t("INERT — --memory/--cpus/--pids-limit are accepted and ignored")
        }
    );

    let have = delegated_controllers(&cur);
    println!(
        "  controllers: {}",
        if have.is_empty() {
            "<none readable>".to_string()
        } else {
            have.join(" ")
        }
    );
    let (fatal, nice) = missing_controllers(&have);
    if !fatal.is_empty() {
        println!(
            "  missing:  {}  {}",
            fatal.join(" "),
            super::po::t("← a Kubernetes node CANNOT boot without this")
        );
    }
    if !nice.is_empty() {
        println!(
            "  absent:   {}  {}",
            nice.join(" "),
            super::po::t("← optional; nothing here needs them")
        );
    }

    if ok && fatal.is_empty() {
        // `cpuset`/`io` absent is the NORMAL state on a stock Ubuntu and breaks
        // nothing. Calling that "something to do" sent people to edit /etc for a
        // delegation their distro's `user.slice` will never pass down anyway.
        println!("\n{}", super::po::t("Nothing to do."));
        return Ok(());
    }
    // Limits apply but a controller a k8s node needs is absent. Reporting
    // "nothing to do" here — which is what this did — sends the operator into a
    // 90-second timeout and a dead node with no connection to this command.
    if ok && !fatal.is_empty() {
        println!(
            "\n{}",
            super::po::t(
                "Container limits work, but `cpu` is not delegated here. `delonix cluster create` \
                 (kind mode) will fail: the node's entrypoint exits with `UserNS: cpu controller \
                 needs to be delegated`.",
            )
        );
        // THE FREE FIX FIRST. A delegated scope needs no root, no reboot and no
        // change to the machine, and on a stock Ubuntu it is usually enough —
        // `user@.service` already ships `Delegate=pids memory cpu`, and what is
        // missing is only that THIS shell's slice does not pass `cpu` down.
        // Leading with the /etc drop-in sent people to change the whole machine
        // for something a prefix on one command already solves.
        println!(
            "\n  1. {}\n     systemd-run --user --scope -p Delegate=yes -- delonix cluster create …\n     {}",
            super::po::t("Try this first — no root, no reboot, works right now:"),
            super::po::t("(check it with: systemd-run --user --scope -p Delegate=yes -- delonix system setup)"),
        );
        println!(
            "\n  2. {}\n     {DELEGATE_DROPIN}\n     {}",
            super::po::t(
                "Only if the above still says `cpu` is missing (needs root, survives reboot):"
            ),
            super::po::t(
                "then log out and back in — a running user@.service keeps the old setting"
            ),
        );
        if !delegate {
            println!(
                "\n{}",
                super::po::t("Re-run with --delegate to write fix 2. (This run changed nothing.)")
            );
            return Ok(());
        }
        return write_delegate_dropin();
    }
    if !rootless {
        // As root the engine owns `delonix.slice` outright; a missing delegation
        // there is a different problem (cgroup v1, or no cgroup2 mount) and this
        // drop-in would not touch it.
        println!(
            "\n{}",
            super::po::t(
                "Running as root: delegation is not the blocker. Check that cgroup v2 is \
                 mounted (`stat -fc %T /sys/fs/cgroup` should say `cgroup2fs`)."
            )
        );
        return Ok(());
    }

    let session_scope = is_login_session_scope(&cur);
    println!(
        "\n{}",
        super::po::t("Two fixes, for two different problems:")
    );
    println!(
        "\n  1. {}\n     systemd-run --user --scope -p Delegate=yes -- delonix container run ...",
        super::po::t("THIS shell, right now (no root):")
    );
    if session_scope {
        println!(
            "     {}",
            super::po::t(
                "(you are in an SSH/login session scope — a SIBLING of user@.service, so \
                 fix 2 alone will NOT reach it)"
            )
        );
    }
    println!(
        "\n  2. {}\n     {DELEGATE_DROPIN}",
        super::po::t("every FUTURE user session (needs root, survives reboot):")
    );

    if !delegate {
        println!(
            "\n{}",
            super::po::t("Re-run with --delegate to write fix 2. (This run changed nothing.)")
        );
        return Ok(());
    }

    write_delegate_dropin()
}

/// Writes the system-wide delegation drop-in. Shared by both paths that reach
/// it — "no delegation at all" and "delegation without the controllers a
/// Kubernetes node needs" — so the remedy cannot drift between them.
fn write_delegate_dropin() -> Result<()> {
    // SAFETY: geteuid() has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error::Invalid(
            super::po::t(
                "--delegate writes under /etc/systemd/system and needs root: re-run with sudo. \
                 Fix 1 above needs no privilege and works right now.",
            )
            .to_string(),
        ));
    }
    let dir = std::path::Path::new(DELEGATE_DROPIN)
        .parent()
        .expect("drop-in path has a parent");
    std::fs::create_dir_all(dir).map_err(|e| Error::Runtime {
        context: "create_dir_all",
        message: format!("{}: {e}", dir.display()),
    })?;
    std::fs::write(DELEGATE_DROPIN, DELEGATE_BODY).map_err(|e| Error::Runtime {
        context: "write",
        message: format!("{DELEGATE_DROPIN}: {e}"),
    })?;
    println!("\n{DELEGATE_DROPIN} {}", super::po::t("written"));
    // Best effort on purpose: the file on disk is the durable half. A failing
    // `daemon-reload` (no systemd in a container image, for instance) must not
    // make a successful write report as failure.
    let reloaded = std::process::Command::new("systemctl")
        .arg("daemon-reload")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    println!(
        "{}",
        if reloaded {
            super::po::t("systemctl daemon-reload: ok")
        } else {
            super::po::t("systemctl daemon-reload: FAILED — run it by hand")
        }
    );
    println!(
        "\n{}",
        super::po::t(
            "Takes effect on the NEXT login (an already-running user@.service keeps the old \
             setting). For this shell, use fix 1."
        )
    );
    Ok(())
}

#[cfg(test)]
mod setup_tests {
    use super::is_login_session_scope;

    #[test]
    fn scope_de_login_e_o_unico_que_o_dropin_nao_alcanca() {
        // O caso que dói: irmão do user@.service, subtree_control root:root.
        assert!(is_login_session_scope(
            "/sys/fs/cgroup/user.slice/user-1000.slice/session-223.scope"
        ));
        // Um scope POR BAIXO do user@.service herda a delegação — avisar aqui
        // mandaria a pessoa resolver um problema que não tem.
        assert!(!is_login_session_scope(
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/run-r0.scope"
        ));
        assert!(!is_login_session_scope(
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service"
        ));
        assert!(!is_login_session_scope("/sys/fs/cgroup/"));
        assert!(!is_login_session_scope(""));
    }

    #[test]
    fn so_a_folha_e_que_decide() {
        // Um caminho que CONTÉM "session-N.scope" a meio, mas cuja folha é
        // outra coisa, não é uma sessão de login — a primeira versão desta
        // função usava `contains` e teria dito que sim.
        assert!(!is_login_session_scope(
            "/sys/fs/cgroup/user.slice/session-9.scope/delonix/dlx-abc"
        ));
    }

    /// `cpu` em falta e `cpuset`/`io` em falta são factos DIFERENTES, e a
    /// primeira versão disto devolvia uma lista só.
    ///
    /// A consequência foi medida, não imaginada: num Ubuntu 24.04 de fábrica o
    /// `user.slice` não passa `cpuset`/`io` para baixo — é o estado NORMAL — e
    /// o comando dizia que o `cluster create` ia falhar, citando o erro de um
    /// controlador que a máquina tinha. Mandava editar o `/etc` para uma
    /// delegação que aquele `subtree_control` (da root) nunca ia passar.
    #[test]
    fn so_o_cpu_e_fatal_para_um_no_kubernetes() {
        let have = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // O caso deste host dentro de um scope delegado: o nó arranca.
        let (fatal, nice) = super::missing_controllers(&have(&["cpu", "memory", "pids"]));
        assert!(fatal.is_empty(), "cpu presente ⇒ nada de fatal");
        assert_eq!(nice, vec!["cpuset", "io"]);

        // O caso que realmente parte um nó.
        let (fatal, _) = super::missing_controllers(&have(&["memory", "pids"]));
        assert_eq!(fatal, vec!["cpu"]);

        // Tudo delegado: nada a dizer.
        let (fatal, nice) =
            super::missing_controllers(&have(&["cpu", "cpuset", "io", "memory", "pids"]));
        assert!(fatal.is_empty() && nice.is_empty());
    }
}
