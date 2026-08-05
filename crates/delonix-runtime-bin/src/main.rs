//! `delonix` — the open-source CLI of the Delonix Runtime: a daemonless,
//! rootless-first, kernel-native container and microVM engine. Homologous to
//! Docker; distinct from the private `delonix`/`delonixctl` of `delonix-paas`
//! (another repo, another dependency tree — see `AGENTS.md`).
//!
//! Commands grouped semantically (instead of a flat list): `container`
//! (run/ps/stop/rm/exec/logs), `image` (pull/ls/rm/export), `build`
//! (Dockerfile/Delonixfile → image), `vm` (declarative microVMs), `volumes`
//! (named volumes), `network` (user networks) and `stack` (applies a whole
//! `delonix-manifest.yaml`). Each group with `apply` also accepts a per-Kind
//! manifest (`delonix <group> apply [-f file]`) — see `cmd::manifest`. Each
//! group lives in `src/cmd/<name>.rs`.

mod cmd;

use clap::{Parser, Subcommand, ValueEnum};
use delonix_runtime_core::Result;

/// Shells supported by `delonix completion`.
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

// `Vm` carries `VmCmd`, which has a large `Create` variant (many optional
// flags) — same justification as the `#[allow]` in `cmd::vm::VmCmd`: a CLI enum
// parsed once per invocation, not a hot path.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    /// Containers: run/ps/stop/rm/exec/logs/update/describe.
    Container {
        #[command(subcommand)]
        action: cmd::container::ContainerCmd,
    },
    /// Real multi-container pods (N containers sharing a netns): create/ls/describe/rm/logs.
    Pod {
        #[command(subcommand)]
        action: cmd::pod::PodCmd,
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
    /// Unified compute layer over containers AND VMs: ls/stop/rm (ADR-0002).
    /// Creation stays declarative — see `kind: Workload` (`stack apply`).
    Workload {
        #[command(subcommand)]
        action: cmd::workload::WorkloadCmd,
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
    /// An isolated, individually-quota'd slice of a `Storage` — multiple
    /// container/vm/pod share ONE NAS export without seeing each other's data.
    Sharevolume {
        #[command(subcommand)]
        action: cmd::sharevolume::ShareVolumeCmd,
    },
    /// Apply a whole manifest (`delonix-manifest.yaml`) — every Kind, in dependency order.
    Stack {
        #[command(subcommand)]
        action: cmd::stack::StackCmd,
    },
    /// Native `docker-compose.yml` support (up/down/ps/logs/config).
    Compose {
        #[command(subcommand)]
        action: cmd::compose::ComposeCmd,
    },
    /// The engine itself: events, state and disk usage.
    System {
        #[command(subcommand)]
        action: cmd::system::SystemCmd,
    },
    /// Idempotent `kubeadm` bootstrap over SSH (`kind: Cluster`), full VM provisioning, or
    /// generating a k8s manifest from a running container/pod (`cluster kube generate`).
    Cluster {
        #[command(subcommand)]
        action: cmd::cluster::ClusterCmd,
    },
    /// Low-level network/infra plumbing, grouped: netns/flow/ingress/egress/httproute/tunnel/boot.
    Net {
        #[command(subcommand)]
        action: cmd::net::NetCmd,
    },
    /// Serve a protocol endpoint on a unix socket, grouped: cri/api/docker-api.
    Serve {
        #[command(subcommand)]
        action: cmd::serve::ServeCmd,
    },
    /// Runtime summary/KPI dashboard (interactive htop-style TUI) — global, or per group (`container dash`, `vm dash`, ...).
    Dash {
        /// Print ONE text snapshot and exit (no TUI) — for scripts/CI; the default when stdout is not a terminal.
        #[arg(long)]
        once: bool,
        /// Print ONE snapshot as JSON and exit (no TUI, no ANSI) — for scripts/Grafana JSON datasource.
        #[arg(long)]
        json: bool,
    },
    /// Print the shell autocompletion script (bash/zsh/fish/...).
    Completion {
        /// Target shell.
        shell: CompShell,
    },
    /// (internal) The embedded L7 reverse-proxy that serves the `kind: HTTPRoute`.
    /// NOT for manual use — `stack apply` launches it inside the holder's netns
    /// (see `cmd::httproute`/`cmd::ingress_proxy`).
    #[command(hide = true)]
    IngressProxy {
        /// JSON file with the `ProxyConfig` (listeners + already-resolved routes).
        #[arg(long)]
        config: std::path::PathBuf,
    },
}

/// The `--version` business card (the `-V` keeps the short, stable line for
/// scripts): build identity + what to do next. It's the first thing a new user
/// runs — it deserves to point the way.
fn long_version_text() -> &'static str {
    use cmd::po::t;
    // Deliberate, one-off leak: the clap builder requires &'static str (without
    // the "string" feature), and this runs once per process — not an
    // accumulating leak. clap prints "<name> <long_version>" — the text does
    // NOT repeat the name.
    Box::leak(
        format!(
            "{v}\n\
         {tag}\n\
         commit: {hash} · built: {date} · {lic}\n\
         \n\
         {try_}:\n\
         \x20 delonix container run -d -p 8080:80 nginx   # {c1}\n\
         \x20 delonix vm create dev                       # {c2}\n\
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
    // Language BEFORE the clap parse: the help is generated DURING the parse,
    // so the decision has to come from a peek at the argv/environment (`--l18n`
    // takes precedence over `$DELONIX_L18N`; with neither, English — the public
    // repo's default).
    if let Some(l) = cmd::po::peek_lang() {
        cmd::output::set_lang(&l);
    }
    let mut command = <Cli as clap::CommandFactory>::command();
    if cmd::output::is_pt() {
        // Help source in EN; in pt, rewrite about/help via the pt.po catalog.
        command = cmd::po::translate_help(command);
    }
    let cli = match <Cli as clap::FromArgMatches>::from_arg_matches(&command.get_matches()) {
        Ok(v) => v,
        Err(e) => e.exit(),
    };
    let _ = cli.l18n; // already consumed by the peek (kept in the schema for the help)
    match cli.cmd {
        Cmd::Container { action } => cmd::container::run(action),
        Cmd::Pod { action } => cmd::pod::run(action),
        Cmd::Image { vm, action } => cmd::image::run(vm, action),
        Cmd::Build(args) => cmd::build::run(args),
        Cmd::Vm { action } => cmd::vm::run(action),
        Cmd::Workload { action } => cmd::workload::run(action),
        Cmd::Volumes { action } => cmd::volume::run(action),
        Cmd::Network { action } => cmd::network::run(action),
        Cmd::Secret { action } => cmd::secret::run(action),
        Cmd::Storage { action } => cmd::storage::run(action),
        Cmd::Sharevolume { action } => cmd::sharevolume::run(action),
        Cmd::Stack { action } => cmd::stack::run(action),
        Cmd::Compose { action } => cmd::compose::run(action),
        Cmd::System { action } => cmd::system::run(action),
        Cmd::Cluster { action } => cmd::cluster::run(action),
        Cmd::Net { action } => cmd::net::run(action),
        Cmd::Serve { action } => cmd::serve::run(action),
        Cmd::IngressProxy { config } => cmd::ingress_proxy::run(&config),
        Cmd::Dash { once, json } => cmd::dash::run(cmd::dash::DashScope::Global, once, json),
        Cmd::Completion { shell } => cmd_completion(shell),
    }
}

/// `delonix completion <shell>` — prints the autocompletion **registration
/// script**. Uses clap's dynamic engine: the script calls
/// `COMPLETE=<shell> delonix -- …` to get command/subcommand/flag suggestions
/// in real time, from the SAME `Cli` definition used for parsing — it never
/// goes out of date by hand.
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
    // `delonix image ls | head` used to end in a Rust PANIC — "failed printing to
    // stdout: Broken pipe (os error 32)" plus a backtrace note — because Rust's
    // runtime sets SIGPIPE to ignore, so a write to a closed pipe returns EPIPE
    // and `println!` unwraps it. Piping into `head`/`grep -q`/`less` is completely
    // ordinary CLI use, and every one of those turned into a crash trace in the
    // logs of whatever was calling us. Restoring the default disposition makes the
    // kernel end the process quietly on SIGPIPE, exactly like every other UNIX
    // tool in a pipeline.
    //
    // SAFETY: `signal(2)` with SIG_DFL has no preconditions; done first, before
    // any thread exists, so no other thread can be mid-write.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    delonix_runtime_core::telemetry::init();
    // Hidden re-exec of the netns holder (`delonix netns holder`, invoked by
    // `delonix-net::infra::start_holder` itself via `unshare` — never by the
    // user). It has to be intercepted BEFORE clap parses (it's not a public
    // subcommand) — without this, `--net <custom-network>` always fails with
    // "timeout waiting for the netns holder" (the re-exec falls into the normal
    // parser and is rejected as an unknown subcommand).
    let raw: Vec<String> = std::env::args().collect();
    // The PIN: owns the userns/netns/mountns and does nothing else, for the whole
    // life of the infra. The CONTROL runs inside it and is restartable — that
    // split is what stops a control-plane restart from destroying every wire on
    // the node (see `infra::pin_main`).
    if raw.len() == 3 && raw[1] == "netns" && raw[2] == "pin" {
        delonix_net::infra::pin_main(); // never returns
    }
    if raw.len() == 3 && raw[1] == "netns" && raw[2] == "control" {
        delonix_net::infra::control_main(); // never returns
    }
    // Hidden re-exec of the 2nd step of `--net <network>` (see
    // `container::reexec_into_netns`): we already run INSIDE the holder's
    // userns+netns; the container spec comes in a file. Intercepted BEFORE clap
    // — it's not a public subcommand.
    if raw.len() == 4 && raw[1] == "netns" && raw[2] == "run" {
        if let Err(e) = cmd::container::run_from_spec(std::path::Path::new(&raw[3])) {
            eprintln!("delonix: {}", cmd::po::t_dyn(&e.to_string()));
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    // Hidden MAPPED re-execs (`__rmtree`, `__volsnap`): we already run as root
    // in a user namespace with the subuids mapped (the parent used `newuidmap` —
    // see `delonix_runtime::{remove_tree_mapped, reexec_mapped}`), so we are the
    // effective owners of the files the container wrote.
    //
    // **These halves were missing in this binary** and only existed in the
    // private CLI of `delonix-paas`: the PUBLIC library re-executed
    // `delonix __rmtree` and the public `delonix` replied "unrecognized
    // subcommand" (rc=2) — with `remove_tree_mapped` not even looking at the
    // exit status, the tree stayed undeleted in SILENCE. The public engine has
    // to stand on its own. Intercepted before clap, like the `netns` above.
    if raw.len() == 3 && raw[1] == "__rmtree" {
        if let Err(e) = cmd::mapped::rmtree(std::path::Path::new(&raw[2])) {
            eprintln!("delonix: {}", cmd::po::t_dyn(&e.to_string()));
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
            eprintln!("delonix: {}", cmd::po::t_dyn(&e.to_string()));
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    if raw.len() == 4 && raw[1] == "__duusage" {
        if let Err(e) =
            cmd::mapped::duusage(std::path::Path::new(&raw[2]), std::path::Path::new(&raw[3]))
        {
            eprintln!("delonix: {}", cmd::po::t_dyn(&e.to_string()));
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    if raw.len() == 4 && raw[1] == "__buildtar" {
        if let Err(e) =
            cmd::mapped::buildtar(std::path::Path::new(&raw[2]), std::path::Path::new(&raw[3]))
        {
            eprintln!("delonix: {}", cmd::po::t_dyn(&e.to_string()));
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    // Dynamic autocompletion: if the shell asked for suggestions (env
    // COMPLETE), handle that and exit; otherwise, follow the normal flow.
    clap_complete::CompleteEnv::with_factory(<Cli as clap::CommandFactory>::command).complete();

    if let Err(e) = run() {
        // Gap closed: the 4 hidden re-exec paths above already ran their errors
        // through `po::t_dyn` (exact-text lookup against `pt.po` — see its doc
        // comment: engine crates can't depend on this catalog, so translation
        // happens here, at output), but THIS is the error path virtually every
        // normal user-facing command failure actually takes — it was printing
        // the raw (often untranslated, sometimes PT/EN-mixed historically)
        // engine-crate message verbatim even under `--l18n=pt`. A message
        // without a matching `pt.po` entry still degrades to the original text
        // (same graceful fallback `t_dyn` already guarantees everywhere else).
        cmd::output::error(&cmd::po::t_dyn(&e.to_string()));
        std::process::exit(1);
    }
}
