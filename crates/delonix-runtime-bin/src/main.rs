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
    about = "Delonix Runtime — a daemonless, rootless-first container & microVM engine (kernel-native, Rust). The open-source engine that powers Delonix.",
    after_help = "SHORTCUTS:\n  ps, run, exec, logs, rm      same as `delonix container <verb>`\n  images                       same as `delonix image ls`"
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
    /// JSON Schema of the manifest, GENERATED from the code (ADR-0007).
    ///
    /// Point an editor at it with one comment at the top of the manifest and you
    /// get completion, type checking and the field docs while you type:
    /// `# yaml-language-server: $schema=./delonix.schema.json`
    Schema {
        #[command(subcommand)]
        action: cmd::schema::SchemaCmd,
    },
    /// Field reference for a Kind, `kubectl explain` style — from the SAME
    /// generated schema, so it cannot drift from the code.
    ///
    /// `delonix explain Container` · `delonix explain Container.ports` ·
    /// `delonix explain Pod.containers.image`
    Explain {
        /// `<Kind>[.field[.field…]]`.
        path: String,
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

/// Top-level aliases for the verbs used dozens of times a day.
///
/// `delonix ps`, not `delonix container ps`. On its own that is a detail;
/// multiplied by every day of use it is the constant friction anyone coming
/// from Docker or Podman feels first, and the reason a grouped CLI reads as
/// bureaucracy even when the grouping is right.
///
/// **Implemented by rewriting argv, deliberately, and not by duplicating the
/// clap variants.** A second declaration of `run`'s ~70 flags is a second
/// declaration to keep in sync, and the day they diverge the alias silently
/// stops accepting a flag the real command has. Splicing the group name in
/// makes them the SAME command by construction — including `--help`, which
/// prints the real one.
///
/// The groups stay: this shortens the hot path, it does not flatten the tree.
fn expand_alias(argv: &mut Vec<String>) {
    // Only the verbs whose meaning is unambiguous across engines. `stop`/
    // `start`/`restart` are deliberately NOT here: this engine also stops VMs
    // and pods, and a top-level `delonix stop <name>` that silently means
    // "container" would be guessing at the user's intent — `workload stop`
    // already exists for the cross-type case, and it refuses ambiguity out loud.
    const CONTAINER: &[&str] = &["ps", "run", "exec", "logs", "rm"];
    let Some(first) = argv.get(1) else { return };
    let group = if CONTAINER.contains(&first.as_str()) {
        "container"
    } else if first == "images" {
        // `docker images` is `image ls` here — the alias carries the rename,
        // which is exactly the friction it exists to remove.
        argv[1] = "ls".to_string();
        "image"
    } else {
        return;
    };
    argv.insert(1, group.to_string());
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
    let mut argv: Vec<String> = std::env::args().collect();
    expand_alias(&mut argv);
    let cli = match <Cli as clap::FromArgMatches>::from_arg_matches(&command.get_matches_from(argv))
    {
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
        Cmd::Schema { action } => cmd::schema::run(action),
        Cmd::Explain { path } => cmd::schema::explain(&path),
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
    // `serve docker-api` starting a container: the API server is a MULTI-THREADED
    // tokio runtime, and `spawn()` reaches `clone()`, whose safety argument is
    // "single-threaded". `clone()` does NOT run the `pthread_atfork` handlers
    // (unlike `fork()`), so the child inherits the glibc malloc arena lock exactly
    // as it stood at clone time — if another worker thread held it (parsing the
    // next request's JSON, say), the child's first allocation in `container_init`
    // deadlocks forever. Re-exec ourselves so the `clone()` happens in a fresh,
    // single-threaded process, which is what the CRI already does for the same
    // reason. Intercepted before clap, like the four above.
    if raw.len() == 3 && raw[1] == "__apirun" {
        cmd::dockerapi::run_from_spec_file(std::path::Path::new(&raw[2]));
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

/// Every user-facing help string has to have a Portuguese translation, and the
/// only honest way to know is to walk the built `Command` and ask the catalog.
///
/// Measured before this test existed: **103 of the 232 subcommands printed
/// their help in English under `--l18n=pt`** — the `container` group, the
/// everyday surface, was 28 of them. The mechanism was never broken (see
/// `po::translate_help`); what was missing were the catalog entries, and
/// nothing was watching.
#[cfg(test)]
mod help_i18n_tests {
    use clap::CommandFactory;

    /// Flag descriptions still waiting for a translation. This is a RATCHET, not
    /// a tolerance: the test fails when the number goes UP (a regression) *and*
    /// when it goes DOWN (translate some, lower the number in the same commit).
    /// A plain `<=` would let the debt sit here forever while still reading as
    /// green.
    ///
    /// Measured after the `container` group was translated: 206 → 120.
    const ARG_HELP_PENDING: usize = 120;

    /// Walks about/long_about of the whole command tree, collecting whatever the
    /// catalog does not translate.
    fn untranslated(
        cmd: &clap::Command,
        path: &str,
        out: &mut Vec<String>,
        args: &mut Vec<String>,
    ) {
        for (what, text) in [
            ("about", cmd.get_about().map(|s| s.to_string())),
            ("long_about", cmd.get_long_about().map(|s| s.to_string())),
        ] {
            let Some(text) = text else { continue };
            // A string that is the SAME in both languages (`Containers:
            // run/ps/stop/...`) is not a gap — asking for a translation that
            // would be identical is how a catalog grows noise.
            if !crate::cmd::po::has_pt_translation(&text) && !is_same_in_both(&text) {
                out.push(format!("{path} ({what}): {text}"));
            }
        }
        for arg in cmd.get_arguments() {
            for (what, text) in [
                ("help", arg.get_help().map(|s| s.to_string())),
                ("long_help", arg.get_long_help().map(|s| s.to_string())),
            ] {
                let Some(text) = text else { continue };
                if !crate::cmd::po::has_pt_translation(&text) && !is_same_in_both(&text) {
                    args.push(format!("{path} --{} ({what}): {text}", arg.get_id()));
                }
            }
        }
        for sub in cmd.get_subcommands() {
            if sub.get_name() == "help" {
                continue;
            }
            untranslated(sub, &format!("{path} {}", sub.get_name()), out, args);
        }
    }

    /// The short list of strings that read identically in EN and pt_AO — a
    /// command-name enumeration is the same text in both.
    fn is_same_in_both(s: &str) -> bool {
        s.starts_with("Containers: run/ps/stop")
    }

    /// The command descriptions — what `--help` shows at the top and what the
    /// parent lists next to each subcommand. Strict: zero tolerated.
    #[test]
    fn todo_o_help_de_comando_tem_traducao_pt() {
        let (mut missing, mut args) = (Vec::new(), Vec::new());
        untranslated(&super::Cli::command(), "delonix", &mut missing, &mut args);
        assert!(
            missing.is_empty(),
            "{} descrição(ões) de comando sem entrada no pt.po:\n  {}",
            missing.len(),
            missing.join("\n  ")
        );
    }

    #[test]
    fn o_help_dos_argumentos_so_pode_encolher() {
        let (mut missing, mut args) = (Vec::new(), Vec::new());
        untranslated(&super::Cli::command(), "delonix", &mut missing, &mut args);
        assert_eq!(
            args.len(),
            ARG_HELP_PENDING,
            "help de argumento por traduzir: {} (esperado {ARG_HELP_PENDING}). \
             Se subiu, é uma flag nova sem entrada no pt.po; se desceu, baixa a \
             constante no mesmo commit.\n  {}",
            args.len(),
            args.join("\n  ")
        );
    }
}

#[cfg(test)]
mod alias_tests {
    use super::expand_alias;

    fn ex(a: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = a.iter().map(|s| s.to_string()).collect();
        expand_alias(&mut v);
        v
    }

    #[test]
    fn atalhos_expandem_para_o_comando_real() {
        assert_eq!(
            ex(&["delonix", "ps", "-a"]),
            ["delonix", "container", "ps", "-a"]
        );
        assert_eq!(
            ex(&["delonix", "rm", "-f", "x"]),
            ["delonix", "container", "rm", "-f", "x"]
        );
        // `images` carrega também o renome do verbo — é a fricção que existe
        // para remover.
        assert_eq!(ex(&["delonix", "images"]), ["delonix", "image", "ls"]);
    }

    #[test]
    fn nao_toca_no_que_nao_e_atalho() {
        assert_eq!(
            ex(&["delonix", "container", "ps"]),
            ["delonix", "container", "ps"]
        );
        assert_eq!(ex(&["delonix", "vm", "ls"]), ["delonix", "vm", "ls"]);
        assert_eq!(ex(&["delonix"]), ["delonix"]);
        // `stop` fica DE FORA: este motor também pára VMs e pods, e adivinhar
        // "container" seria escolher pelo utilizador — `workload stop` já cobre
        // o caso cruzado e recusa a ambiguidade em voz alta.
        assert_eq!(ex(&["delonix", "stop", "x"]), ["delonix", "stop", "x"]);
    }

    #[test]
    fn um_argumento_com_o_nome_de_um_atalho_nao_e_reescrito() {
        // Só a POSIÇÃO 1 conta. Um container chamado `ps` passado a outro
        // comando não pode fazer o argv mudar de forma.
        assert_eq!(
            ex(&["delonix", "vm", "rm", "ps"]),
            ["delonix", "vm", "rm", "ps"]
        );
    }
}
