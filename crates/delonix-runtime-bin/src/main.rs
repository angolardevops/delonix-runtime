//! `delonix` — the open-source CLI of the Delonix Runtime: a daemonless,
//! rootless-first, kernel-native container and microVM engine. Homologous to
//! Docker; distinct from the private `delonix`/`delonixctl` of `delonix-paas`
//! (another repo, another dependency tree — see `CLAUDE.md`).
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
    delonix_runtime_core::telemetry::init();
    // Hidden re-exec of the netns holder (`delonix netns holder`, invoked by
    // `delonix-net::infra::start_holder` itself via `unshare` — never by the
    // user). It has to be intercepted BEFORE clap parses (it's not a public
    // subcommand) — without this, `--net <custom-network>` always fails with
    // "timeout waiting for the netns holder" (the re-exec falls into the normal
    // parser and is rejected as an unknown subcommand).
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() == 3 && raw[1] == "netns" && raw[2] == "holder" {
        delonix_net::infra::holder_main(); // never returns
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
        // The top-level error in red (honors NO_COLOR/pipes — see `output::cor`).
        cmd::output::error(&e.to_string());
        std::process::exit(1);
    }
}
