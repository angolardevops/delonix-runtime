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
    /// Check the HOST prerequisites this engine needs, and say how to fix each.
    ///
    /// `info` reports state and `setup` fixes cgroup delegation. This answers a
    /// third question — «is this host able to do what the engine promises?» —
    /// and it exists because several of those promises fail SILENTLY when a
    /// prerequisite is missing. Read-only: it changes nothing.
    Doctor {
        /// Exit non-zero when a check fails, for a CI or provisioning gate.
        #[arg(long)]
        strict: bool,
    },
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
        #[arg(value_hint = clap::ValueHint::FilePath)]
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
        /// SCHEDULED mode: reclaim only when the disk is at or above
        /// `--threshold`, and do nothing at all below it.
        ///
        /// This is the flag that makes `system prune` safe to put in a timer.
        /// Below the threshold it exits 0 having changed nothing, and that is
        /// SUCCESS, not a no-op to be fixed: pruning every night by habit
        /// deletes images that get pulled again in the morning, which costs
        /// bandwidth and start-up time and reclaims nothing that was a problem.
        ///
        /// It implies `--force` — an unattended sweep has nobody to answer a
        /// prompt — and it never touches volumes (see `volumes prune`).
        #[arg(long)]
        auto: bool,
        /// Occupancy percentage at or above which `--auto` acts.
        ///
        /// Measured the way `df` measures it, on the filesystem holding the
        /// engine's state. Keep it BELOW the threshold of whatever alerts on
        /// the same disk: the GC has to act before someone is woken up, and
        /// with equal numbers the alert always arrives first and the GC looks
        /// useless.
        #[arg(long, default_value_t = 75, requires = "auto", value_parser = clap::value_parser!(u8).range(1..=100))]
        threshold: u8,
        /// Print what this sweep WOULD take, split by category, and take
        /// nothing.
        ///
        /// The split is the reason it exists. `prune` mixes two different
        /// things in one verb — resources somebody DECLARED (stopped
        /// containers, tagged images) and debris that never had a record
        /// (orphan directories, unreferenced blobs) — and only the second is
        /// safe to hand to a timer. Whoever arms `--auto` has to be able to see
        /// which half it would touch first.
        ///
        /// Combines with `--auto`: it then also says whether the threshold gate
        /// would let the sweep run at all right now.
        ///
        /// The three network reapers (orphan slirps, ingress refs, host ports)
        /// are NOT previewed — they compute and act in one call inside
        /// `delonix-net`. None of them frees a byte of disk, so everything that
        /// reclaims space is in the report.
        #[arg(long = "dry-run")]
        dry_run: bool,
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
        SystemCmd::Doctor { strict } => cmd_doctor(strict),
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
        SystemCmd::Prune {
            all,
            force,
            auto,
            threshold,
            dry_run,
        } => cmd_prune(all, force, auto, threshold, dry_run),
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
/// Since the per-resource prunes exist, this is the COMPOSITION of them and no
/// longer a second implementation: it calls the same `prune::sweep_*` that
/// `container prune`/`image prune` call. Duplicating a destructive sweep is how
/// two answers to the same question start to diverge.
///
/// The order is not a preference: containers first (removing them is what makes
/// their images and blobs unreferenced), then images, then the empty `dlx-*`
/// networks. **Volumes are deliberately not here** — `docker system prune`
/// leaves them alone too, and removing a volume destroys data that nothing else
/// in this sweep does.
/// Whether the engine's filesystem is full enough for `--auto` to act, saying
/// out loud what it measured either way.
///
/// Both answers print. A scheduled job whose only output is silence is
/// indistinguishable from one that never ran — which is exactly the failure
/// mode a nightly reaper has to avoid, because nobody reads a journal that
/// says nothing.
///
/// Errors when the occupancy cannot be read at all. That is reaper rule 4 (no
/// visibility, defer) with the sign deliberately chosen: a sweep that cannot
/// see how full the disk is must not assume it is full and start deleting, and
/// must not assume it is empty and report success — it has to fail loudly so
/// the timer goes red and somebody looks.
fn above_threshold(threshold: u8) -> Result<bool> {
    let root = super::util::state_root();
    let Some(pct) = super::prune::filesystem_used_pct(&root) else {
        return Err(delonix_runtime_core::Error::Invalid(super::po::tf(
            "cannot read the occupancy of the filesystem holding {path} — refusing to reclaim \
             blind",
            &[("path", &root.display().to_string())],
        )));
    };
    if pct < threshold {
        println!(
            "{}",
            super::po::tf(
                "disk at {pct}%, below the {threshold}% threshold — nothing to do",
                &[
                    ("pct", &pct.to_string()),
                    ("threshold", &threshold.to_string()),
                ]
            )
        );
        return Ok(false);
    }
    println!(
        "{}",
        super::po::tf(
            "disk at {pct}%, at or above the {threshold}% threshold — reclaiming",
            &[
                ("pct", &pct.to_string()),
                ("threshold", &threshold.to_string()),
            ]
        )
    );
    Ok(true)
}

/// Renders a [`super::prune::PrunePlan`] as the two halves it is, never as one
/// total.
///
/// A single "would free 12 GiB" line is the number that gets an `--auto` into a
/// unit file, and it is the wrong number to decide on: most of it can sit
/// behind the half that destroys declared resources. So the totals are printed
/// per category and the categories are named, not implied by indentation.
fn print_plan(p: &super::prune::PrunePlan, auto: bool, would_run: bool) {
    if auto {
        println!(
            "{}",
            super::po::t(if would_run {
                "--auto: the threshold gate WOULD let this run now"
            } else {
                "--auto: the threshold gate would NOT let this run now (the report below is what \
                 it would take once it does)"
            })
        );
    }
    if p.is_empty() {
        println!("{}", super::po::t("nothing to reclaim"));
        return;
    }

    println!(
        "{}",
        super::po::t("DECLARED — resources someone created (a `prune` destroys these):")
    );
    println!(
        "{}",
        super::po::tf(
            "  {n} stopped container(s), {i} image(s) — {size}",
            &[
                ("n", &p.containers.len().to_string()),
                ("i", &p.images.len().to_string()),
                ("size", &p.bytes_a().fmt()),
            ]
        )
    );
    for name in &p.containers {
        println!("  -   container/{name}");
    }
    for name in &p.images {
        println!("  -   image/{name}");
    }

    println!(
        "{}",
        super::po::t("DEBRIS — never had a record (safe for an unattended sweep):")
    );
    println!(
        "{}",
        super::po::tf(
            "  {d} orphan dir(s), {b} blob(s), {g} cgroup(s), {n} network(s) — {size}",
            &[
                ("d", &p.dirs.to_string()),
                ("b", &p.blobs.to_string()),
                ("g", &p.cgroups.to_string()),
                ("n", &p.networks.to_string()),
                ("size", &p.bytes_b().fmt()),
            ]
        )
    );

    let mut total = p.bytes_a();
    total.add(p.bytes_b());
    println!(
        "{}",
        super::po::tf(
            "total if BOTH halves run: {size}",
            &[("size", &total.fmt())]
        )
    );
    super::prune::note_partial(total);
}

fn cmd_prune(all: bool, force: bool, auto: bool, threshold: u8, dry_run: bool) -> Result<()> {
    // THE GATE COMES FIRST — before opening a store, before listing anything.
    // A scheduled sweep that is not going to act should cost nothing and, above
    // all, should not have started deciding what to delete.
    //
    // Under `--dry-run` the gate REPORTS instead of returning: "the timer would
    // not fire right now" is half the answer somebody arming `--auto` came for,
    // and stopping here would hide the other half.
    let would_run = !auto || above_threshold(threshold)?;
    if !would_run && !dry_run {
        return Ok(());
    }
    let (images, store) = open_stores()?;

    if dry_run {
        let p = super::prune::plan(&images, &store, all)?;
        print_plan(&p, auto, would_run);
        return Ok(());
    }

    // CONFIRM FIRST. This is destructive well beyond what its name suggests: the
    // help leads with "unused images", but the container sweep removes EVERY
    // stopped container — including ones merely `Created` and not yet started.
    // Docker asks. An operator who types `delonix system prune` expecting a disk
    // cleanup should not discover afterwards that a stopped container they were
    // about to `start` is gone.
    let doomed = super::prune::doomed_containers(&store)?;
    let preview = (!doomed.is_empty()).then(|| {
        super::po::tf(
            "This will remove {n} stopped container(s): {list}",
            &[
                ("n", &doomed.len().to_string()),
                ("list", &doomed.join(", ")),
            ],
        )
    });
    if !super::prune::confirm(
        // `--auto` IS the unattended mode: it runs from a timer, where there is
        // no terminal and nobody to answer. Requiring `--force` beside it would
        // be a second word for the same intent, and the kind of papercut that
        // ends with `--auto` in a unit file that fails every night.
        force || auto,
        super::po::t(
            "`system prune` removes every stopped container and unreferenced data — pass --force \
             to confirm when not on a terminal",
        ),
        preview,
        super::po::t(
            "Also removes unused images, CAS blobs and orphan directories. Continue? [y/N]",
        ),
    )? {
        return Ok(());
    }

    let c = super::prune::sweep_containers(&images, &store)?;
    if c.slirps > 0 {
        println!(
            "{}",
            super::po::tf(
                "net: {n} orphan slirp(s) reaped",
                &[("n", &c.slirps.to_string())]
            )
        );
    }
    if c.refs > 0 {
        println!(
            "{}",
            super::po::tf(
                "net: {n} orphan ingress ref(s) reaped",
                &[("n", &c.refs.to_string())]
            )
        );
    }
    let i = super::prune::sweep_images(&images, &store, all)?;
    let rmn = super::prune::sweep_networks(&store)?;

    let mut total = c.freed;
    total.add(i.freed);
    println!(
        "{}",
        super::po::tf(
            "removed: {c} container(s), {d} orphan dir(s), {i} image(s), {b} blob(s), {g} cgroup(s), {p} orphan port(s), {n} orphan network(s) — {size} freed",
            &[
                ("c", &c.containers.to_string()),
                ("d", &c.dirs.to_string()),
                ("i", &i.images.to_string()),
                ("b", &i.blobs.to_string()),
                ("g", &c.cgroups.to_string()),
                ("p", &c.ports.to_string()),
                ("n", &rmn.to_string()),
                ("size", &total.fmt()),
            ]
        )
    );
    super::prune::note_partial(total);
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
    match serde_json::to_string(e) {
        Ok(s) => println!("{s}"),
        // Unreachable today (`Event` is a u64 and five strings), and it says so
        // instead of swallowing: a dropped audit record is exactly what an event
        // log exists NOT to have, so if the type ever grows a field that can
        // fail to serialize, the operator hears about it rather than reading a
        // stream with a hole in it.
        Err(err) => eprintln!("delonix: event skipped, it did not serialize: {err}"),
    }
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
/// One host prerequisite: what it is, whether it holds, and how to fix it.
struct Check {
    name: &'static str,
    /// `Some(true)` holds, `Some(false)` does not, `None` could not be measured
    /// — which is a THIRD answer and never folded into «fails». A sysctl this
    /// user cannot read is not a sysctl set to zero.
    ok: Option<bool>,
    /// What was actually read, so the verdict can be checked rather than trusted.
    detail: String,
    /// Empty when it holds. When it does not, the exact command to fix it.
    fix: String,
    /// Whether failing this one breaks a SAFETY promise rather than a feature.
    /// The distinction drives the ordering: a silently inert isolation boundary
    /// outranks a port that will not bind, because the second one tells you.
    silent: bool,
}

fn read_sysctl(name: &str) -> Option<String> {
    let path = format!("/proc/sys/{}", name.replace('.', "/"));
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn have_tool(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|d| {
                let p = d.join(bin);
                p.is_file() && {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::metadata(&p)
                        .map(|m| m.permissions().mode() & 0o111 != 0)
                        .unwrap_or(false)
                }
            })
        })
        .unwrap_or(false)
}

/// `system doctor` — the host prerequisites, measured.
///
/// # Why this is a command and not a paragraph in the README
///
/// Several of this engine's promises fail SILENTLY when the host is not ready,
/// and each one has already cost somebody a session:
///
/// - **`br_netfilter`.** Namespace isolation lives in nftables chains on the
///   `forward` hook. Traffic between two containers on the SAME bridge only
///   reaches that layer if `br_netfilter` carries it there. Without the module
///   the chains ARE installed, the sets ARE populated, every command reports
///   success — and the boundary does not exist. Measured 2026-08-12 in a clean
///   VM: `teamA` reached `teamB`. It is the most expensive silent failure this
///   engine can have, because the thing that fails reads as applied.
/// - **cgroup delegation.** Without it `-m`/`--cpus`/`--pids-limit` are
///   accepted and ignored — the limits go nowhere and nothing says so.
/// - **subuid/subgid.** Without a range the userns maps a single uid, and
///   images that need more than one user break in confusing ways.
///
/// It does NOT refuse anything and does NOT change anything. Whether the engine
/// should REFUSE to promise isolation on a host without `br_netfilter` is a
/// policy decision the AGENTS.md deliberately leaves open: warning is the floor,
/// refusing would be genuinely fail-closed and would break everyone running that
/// way today. This is the floor, and it breaks nobody.
fn cmd_doctor(strict: bool) -> Result<()> {
    let rootless = runtime::is_rootless();
    let mut checks: Vec<Check> = Vec::new();

    // The silent safety one goes first, on purpose.
    let module = std::path::Path::new("/sys/module/br_netfilter").is_dir();
    let call = read_sysctl("net.bridge.bridge-nf-call-iptables");
    let (ok, detail) = match (module, call.as_deref()) {
        (true, Some("1")) => (
            Some(true),
            "module loaded, bridge-nf-call-iptables=1".to_string(),
        ),
        (true, Some(other)) => (
            Some(false),
            format!("module loaded but bridge-nf-call-iptables={other}"),
        ),
        // The sysctl only EXISTS once the module is loaded, so «module absent»
        // and «cannot read the sysctl» are the same observation here.
        (false, _) => (Some(false), "module not loaded".to_string()),
        (true, None) => (None, "module loaded, sysctl unreadable".to_string()),
    };
    checks.push(Check {
        name: "br_netfilter (namespace isolation)",
        ok,
        detail,
        fix: "sudo modprobe br_netfilter && sudo sysctl -w net.bridge.bridge-nf-call-iptables=1               (persist: scripts/install.sh, which writes /etc/modules-load.d and /etc/sysctl.d)"
            .to_string(),
        silent: true,
    });

    let limits = runtime::cgroup_limits_apply();
    checks.push(Check {
        name: "cgroup2 delegation (--memory/--cpus/--pids-limit)",
        ok: Some(limits),
        detail: runtime::current_cgroup_v2().unwrap_or_else(|| "<unknown>".into()),
        fix: "systemd-run --user --scope -p Delegate=yes -- delonix …  (or, if that still               lacks `cpu`: sudo delonix system setup --delegate)"
            .to_string(),
        silent: true,
    });

    if rootless {
        // Read the user's own line rather than assuming the login name matches.
        let user = std::env::var("USER").unwrap_or_default();
        let subid = std::fs::read_to_string("/etc/subuid").ok().map(|t| {
            t.lines()
                .filter(|l| !user.is_empty() && l.starts_with(&format!("{user}:")))
                .count()
        });
        checks.push(Check {
            name: "subuid range (more than one uid inside the container)",
            ok: subid.map(|n| n > 0),
            detail: match subid {
                Some(n) => format!("{n} line(s) for '{user}' in /etc/subuid"),
                None => "/etc/subuid unreadable".to_string(),
            },
            fix: format!(
                "sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 {user}"
            ),
            silent: true,
        });
    }

    // NOT a pass/fail check, and the first version got it wrong twice: it tested
    // `== 0` (so this host's perfectly working `80` read as broken) and printed
    // ✗ FAILED next to a `fix` that said OPTIONAL. The default (1024) is the
    // SAFE one — lowering it lets every local program bind 80-1023 — so calling
    // it a failure is how a doctor teaches people to ignore its output. It
    // reports the threshold and what it means.
    let low = read_sysctl("net.ipv4.ip_unprivileged_port_start");
    let threshold = low.as_deref().and_then(|v| v.parse::<u32>().ok());
    checks.push(Check {
        name: "lowest publishable port",
        ok: Some(true),
        detail: match threshold {
            Some(1024) => {
                "1024 (the default) — publishing below it needs `--low-ports` or root".to_string()
            }
            Some(v) => format!("{v} — `-p {v}:…` and up work without root"),
            None => "ip_unprivileged_port_start unreadable".to_string(),
        },
        fix: String::new(),
        silent: false,
    });

    for (bin, why) in [
        ("slirp4netns", "rootless networking"),
        ("nft", "firewall and isolation"),
        ("ip", "every network operation"),
    ] {
        checks.push(Check {
            name: "tool",
            ok: Some(have_tool(bin)),
            detail: format!("{bin} — {why}"),
            fix: format!("install {bin} (see scripts/install.sh)"),
            silent: false,
        });
    }

    let mut failed = 0usize;
    let mut unknown = 0usize;
    for c in &checks {
        let (mark, label) = match c.ok {
            Some(true) => ("✓", super::po::t("ok")),
            Some(false) => ("✗", super::po::t("FAILED")),
            None => ("?", super::po::t("unknown")),
        };
        let name = if c.name == "tool" { &c.detail } else { c.name };
        println!("  {mark} {label:<8} {name}");
        if c.name != "tool" {
            println!("      {}", super::output::dim(&c.detail));
        }
        match c.ok {
            Some(false) => {
                failed += 1;
                println!("      → {}", c.fix);
            }
            None => {
                unknown += 1;
                // An unmeasurable check gets the fix printed too: the reader
                // still has to decide, and hiding it would make «unknown» read
                // as «fine».
                println!("      → {}", c.fix);
            }
            Some(true) => {}
        }
    }

    println!();
    // The count of SILENT failures is called out separately, because that is
    // the number that decides whether this host is lying to its operator.
    let silent_failed = checks
        .iter()
        .filter(|c| c.ok == Some(false) && c.silent)
        .count();
    if failed == 0 && unknown == 0 {
        println!("{}", super::po::t("every prerequisite holds."));
    } else {
        println!(
            "{}",
            super::po::tf(
                "{failed} failing, {unknown} unmeasurable — {silent} break a promise SILENTLY (success is reported and the thing does not happen).",
                &[
                    ("failed", &failed.to_string()),
                    ("unknown", &unknown.to_string()),
                    ("silent", &silent_failed.to_string()),
                ],
            )
        );
    }
    if strict && failed > 0 {
        return Err(delonix_runtime_core::Error::Invalid(super::po::tf(
            "{n} host prerequisite(s) not met",
            &[("n", &failed.to_string())],
        )));
    }
    Ok(())
}

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
