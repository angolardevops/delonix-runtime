//! `delonix` — the open-source CLI of the Delonix Runtime: a daemonless,
//! rootless-first, kernel-native container and microVM engine. Homologous to
//! Docker; distinct from the private `delonix`/`delonixctl` of `delonix-paas`
//! (another repo, another dependency tree — see `AGENTS.md`).
//!
//! Commands grouped semantically (instead of a flat list): `container`
//! (run/ps/stop/rm/exec/logs), `image` (pull/ls/remove/export), `build`
//! (Dockerfile/Delonixfile → image), `vm` (declarative microVMs), `volume`
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

/// Editors `delonix syntax` can hand a VMfile grammar to.
#[derive(Clone, Copy, ValueEnum)]
enum SyntaxEditor {
    Vim,
    Vscode,
}

#[derive(Parser)]
#[command(
    name = "delonix",
    version,
    long_version = long_version_text(),
    about = "Delonix Engine — a daemonless, rootless-first container & microVM engine (kernel-native, Rust). The open-source engine that powers Delonix."
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
/// The two things a program can be told about this CLI: how to complete its
/// commands, and how to colour its files.
#[derive(Subcommand)]
enum CompletionCmd {
    /// Print the shell autocompletion script (bash/zsh/fish/...).
    Shell {
        /// Target shell.
        shell: CompShell,
    },
    /// VMfile syntax highlighting for an editor (vim/vscode).
    Editor {
        /// Target editor.
        editor: SyntaxEditor,
        /// Write the editor's files into this directory instead of printing one to stdout.
        #[arg(value_hint = clap::ValueHint::DirPath, long)]
        dir: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
// Subcommand structs vary wildly in size by design (Container carries the
// biggest arg surface in the CLI) — boxing them would ripple through every
// match arm that destructures `action` by value, for no runtime benefit.
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// Start the right project for THIS directory.
    ///
    /// Detects and dispatches to `stack init`/`vm init` with the matching
    /// template, saying what it detected and why.
    Init {
        /// Project directory (default: the current one).
        #[arg(value_hint = clap::ValueHint::DirPath)]
        dir: Option<std::path::PathBuf>,
        /// Force a template instead of the detected one (`stack init -t list` shows them).
        #[arg(short = 't', long)]
        template: Option<String>,
        /// Overwrite files that already exist.
        #[arg(long)]
        force: bool,
    },
    // A subcommand as well as a flag because `<tool> version` is what people type first —
    // git, docker, kubectl and podman all answer to it. It prints the flag's text VERBATIM
    // (`long_version_text`), so the two can never drift into saying different things.
    //
    // Plain `//`, not `///`: clap turns a doc comment into the user's `--help`, and design
    // rationale has no business there. Caught by the i18n guard, which demanded a
    // translation for three paragraphs nobody should have been reading in a help screen.
    /// Print the version (same output as `--version`).
    Version,
    /// Containers: run/ps/stop/rm/exec/logs/update/describe.
    Container {
        #[command(subcommand)]
        action: cmd::container::ContainerCmd,
    },
    /// Real multi-container pods (N containers sharing a netns): create/logs/exec/cp/attach/port-forward.
    ///
    /// `describe`/`rm` moved to the generic per-Kind verbs — `delonix describe
    /// pod <name>` / `delonix delete pod <name>`.
    Pod {
        #[command(subcommand)]
        action: cmd::pod::PodCmd,
    },
    /// OCI images: pull/ls/remove/export. `image vm <cmd>` for golden VM images.
    Image {
        #[command(subcommand)]
        action: cmd::image::ImageCmd,
    },
    /// Build an image from a Dockerfile or Delonixfile.
    Build(cmd::build::BuildArgs),
    /// Declarative microVMs: create/ls/start/stop/console.
    ///
    /// `describe`/`rm` moved to the generic per-Kind verbs — `delonix
    /// describe vm <name>` / `delonix delete vm <name>`.
    Vm {
        #[command(subcommand)]
        action: cmd::vm::VmCmd,
    },
    /// Unified compute layer over containers AND VMs: ls/stop/rm (ADR-0002).
    ///
    /// Creation stays declarative — see `kind: Workload` (`stack apply`).
    Workload {
        #[command(subcommand)]
        action: cmd::workload::WorkloadCmd,
    },
    /// Named volumes and bind mounts: create/ls/rm/inspect.
    Volume {
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
    /// Field reference for a Kind, `kubectl explain` style.
    ///
    /// From the SAME generated schema, so it cannot drift from the code.
    ///
    /// `delonix explain Container` · `delonix explain Container.ports` ·
    /// `delonix explain Pod.containers.image`
    Explain {
        /// `<Kind>[.field[.field…]]`.
        path: String,
    },
    /// Every Kind this engine serves: plural, shortnames, apiVersion and form.
    ///
    /// Read from the same registry the parser, the schema and the reconciler
    /// read — there is no second table to disagree with them. It answers «what
    /// can I write in a manifest, and what do I type after `explain`».
    ///
    /// `FORM` is the column that cannot be guessed: it says whether a document
    /// of that Kind survives the load under its own name, which is why a
    /// `kind: Egress` never appears in a plan as `Egress`.
    #[command(name = "api-resources")]
    ApiResources {
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: cmd::output::OutputFormat,
    },
    /// Converge a manifest — the canonical spelling of `stack apply`.
    ///
    /// Promoted to the top because the verb belongs to the MANIFEST, not to one
    /// group: a file with a Network, a Volume and a Pod in it is not a «stack»
    /// operation any more than it is a «network» one.
    Apply {
        #[arg(short = 'f', long = "file", value_hint = clap::ValueHint::FilePath)]
        file: Option<std::path::PathBuf>,
        /// Stack name to own the resources with (`delonix.io/stack`).
        #[arg(long)]
        name: Option<String>,
        /// Print what would be applied, and apply nothing.
        #[arg(long)]
        dry_run: bool,
        /// Authorize the recreate of a resource whose cold field changed.
        #[arg(long, value_name = "Kind/name")]
        replace: Vec<String>,
        /// Remove what this stack owns and the manifest no longer declares.
        #[arg(long)]
        prune: bool,
    },
    /// Show what an apply would change, and change nothing.
    Plan {
        #[arg(short = 'f', long = "file", value_hint = clap::ValueHint::FilePath)]
        file: Option<std::path::PathBuf>,
        #[arg(long)]
        name: Option<String>,
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: cmd::output::OutputFormat,
        /// 0 = no changes, 2 = there are changes, 1 = error (`terraform plan`).
        #[arg(long)]
        detailed_exitcode: bool,
        /// Print WHICH fields the plan compares, per Kind, and exit.
        #[arg(long)]
        fields: bool,
    },
    /// Block until the manifest's resources are ready.
    Wait {
        #[arg(short = 'f', long = "file", value_hint = clap::ValueHint::FilePath)]
        file: Option<std::path::PathBuf>,
        /// Seconds to wait before giving up (exit 124).
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Work on a manifest without touching the host.
    Manifest {
        #[command(subcommand)]
        action: cmd::manifestcmd::ManifestCmd,
    },
    /// List resources of a Kind — the generic form of ten `ls` commands.
    ///
    /// The Kind is resolved through the same registry `api-resources` prints,
    /// so `pods`, `pod` and `po` are the same question, and a Kind renamed in
    /// v0.64.0 still answers to the name it had.
    Get {
        /// Kind, plural or short name: `pods`, `pod`, `po`.
        kind: String,
        /// Name a resource to get its detail instead of the list.
        names: Vec<String>,
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: cmd::output::OutputFormat,
        /// Show only this isolation namespace — only Kinds that HAVE one accept it (refused otherwise, never silently ignored).
        #[arg(short = 'n', long)]
        namespace: Option<String>,
    },
    /// Detail of one resource, in blocks — the generic form of ten `describe`s.
    Describe {
        /// Kind, plural or short name.
        kind: String,
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Remove resources by Kind and name.
    ///
    /// With no name it REFUSES rather than removing every resource of the Kind:
    /// a missing argument is a typo far more often than an intention, and this
    /// verb does not get a second chance to ask.
    Delete {
        /// Kind, plural or short name.
        kind: String,
        names: Vec<String>,
        #[arg(short, long)]
        force: bool,
    },
    /// The manifest, last-applied, and observed values for one resource — side by side.
    ///
    /// `stack plan` already computes this internally for every resource — it
    /// only ever prints the VERDICT (Create/Update/Replace/NoOp). This prints
    /// the three underlying VALUES, for one named resource, side by side.
    Diff {
        /// Kind, plural or short name: `containers`, `container`, `c`.
        kind: String,
        name: String,
        /// Manifest to read (default: `./delonix-manifest.yaml`).
        #[arg(short = 'f', long = "file")]
        file: Option<std::path::PathBuf>,
        /// Exit 2 if DESIRED and OBSERVED differ on any field, 0 otherwise —
        /// same contract as `stack plan --detailed-exitcode`.
        #[arg(long)]
        detailed_exitcode: bool,
    },
    /// A small, local preference — never a remote context.
    ///
    /// Only `output` (`table`|`json`) today. The specification's `endpoint`/
    /// `identity`/`tls` context stays out on purpose: ADR-0010 recused the
    /// remote management API, and nobody has named a concrete consumer for
    /// reopening it.
    Config {
        #[command(subcommand)]
        action: cmd::config::ConfigCmd,
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
    /// Archives of ONE resource (container/pod/vm/stack): create, ls, inspect, restore, schedule, remove.
    ///
    /// The archive carries the record and the DATA of the volumes it uses — not
    /// the image and not the rootfs, which `backup restore` derives by pulling.
    /// A VM is the exception: its overlay disk IS its state, so that one travels.
    ///
    /// For the whole node — registries, secrets, cluster PKI, the event log —
    /// the command is `delonix system snapshot create`, which is a different
    /// scope and not a second door to this one.
    Backup {
        #[command(subcommand)]
        action: cmd::rbackup::BackupCmd,
    },
    /// Kubernetes clusters: `kubeadm` bootstrap, VM provisioning, manifest generation.
    ///
    /// Idempotent `kubeadm` bootstrap over SSH (`kind: KubernetesCluster`), full VM
    /// provisioning, or generating a k8s manifest from a running
    /// container/pod (`cluster kube generate`).
    Cluster {
        #[command(subcommand)]
        action: cmd::cluster::ClusterCmd,
    },
    /// Low-level network/infra plumbing, grouped: netns/flow/ingress/egress/httproute/tunnel.
    Net {
        #[command(subcommand)]
        action: cmd::net::NetCmd,
    },
    /// Serve a protocol endpoint on a unix socket, grouped: cri/api/docker-api.
    Serve {
        #[command(subcommand)]
        action: cmd::serve::ServeCmd,
    },
    /// Model Context Protocol server — a LOCAL, tenancy-free AI control surface.
    ///
    /// ADR-0025. NOT stable: see `docs/cli-stability.md`.
    Mcp {
        #[command(subcommand)]
        action: cmd::mcp::McpCmd,
    },
    /// Runtime summary/KPI dashboard (interactive htop-style TUI).
    ///
    /// Global by default, or focused with `--scope`. Renamed from `dash` (§22):
    /// the group is declared NOT stable, and an old spelling that keeps working
    /// is one nobody migrates away from — `delonix dash` now fails loudly
    /// instead of quietly staying the habit. The five per-group `<group> dash`
    /// verbs (`container dash`, `vm dash`, ...) folded into `--scope` for the
    /// same reason: this dashboard was never a Docker/Podman/kubectl verb with
    /// a familiar per-group shape worth preserving five times over.
    Dashboard {
        /// Focus on one kind of resource instead of the global summary.
        #[arg(long, value_enum)]
        scope: Option<cmd::dash::DashScope>,
        /// Print ONE text snapshot and exit (no TUI) — for scripts/CI; the default when stdout is not a terminal.
        #[arg(long)]
        once: bool,
        /// Print ONE snapshot as JSON and exit (no TUI, no ANSI) — for scripts/Grafana JSON datasource.
        #[arg(long)]
        json: bool,
    },
    /// What another program needs to understand this CLI and its files.
    ///
    /// `syntax` folded in here (§21): both arms answer the same question — what
    /// a shell or an editor has to be told — and having them under two roofs
    /// meant nobody looking for one ever found the other.
    Completion {
        #[command(subcommand)]
        action: CompletionCmd,
    },
    /// Manual pages in roff, generated from this binary — one per command.
    Man(cmd::man::ManArgs),
    /// (internal) The embedded L7 reverse-proxy that serves the `kind: HTTPRoute`.
    /// NOT for manual use — `stack apply` launches it inside the holder's netns
    /// (see `cmd::httproute`/`cmd::ingress_proxy`).
    #[command(hide = true)]
    IngressProxy {
        /// JSON file with the `ProxyConfig` (listeners + already-resolved routes).
        #[arg(value_hint = clap::ValueHint::FilePath, long)]
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
         \x20 delonix dashboard                           # {c5}\n\
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

/// The fully-dressed `Command`: parsed from the derive, translated if the
/// session is Portuguese, then given the manual.
///
/// Extracted from `run` because `delonix man` renders the SAME tree to roff.
/// Building it a second time by hand there is how a manpage ends up describing
/// a CLI that no longer exists — the whole reason the pages are generated.
pub fn build_command() -> clap::Command {
    let mut command = <Cli as clap::CommandFactory>::command();
    if cmd::output::is_pt() {
        // Help source in EN; in pt, rewrite about/help via the pt.po catalog.
        command = cmd::po::translate_help(command);
    }
    // The manual (COMMAND MAP / EXAMPLES / SEE ALSO) goes on AFTER the
    // translation, not before: the map's category labels are built here, and a
    // pass that rewrote help strings afterwards would have to know to leave
    // this tail alone. Order is the cheap way to make that impossible.
    cmd::manual::apply(command)
}

fn run() -> Result<()> {
    // Language BEFORE the clap parse: the help is generated DURING the parse,
    // so the decision has to come from a peek at the argv/environment (`--l18n`
    // takes precedence over `$DELONIX_L18N`; with neither, English — the public
    // repo's default).
    if let Some(l) = cmd::po::peek_lang() {
        cmd::output::set_lang(&l);
    }
    let command = build_command();
    let argv: Vec<String> = std::env::args().collect();
    let cli = match <Cli as clap::FromArgMatches>::from_arg_matches(&command.get_matches_from(argv))
    {
        Ok(v) => v,
        Err(e) => e.exit(),
    };
    let _ = cli.l18n; // already consumed by the peek (kept in the schema for the help)
                      // A remote VM backend has to be in the registry BEFORE the engine is
                      // asked for one: `create_with` resolves the backend itself and never
                      // receives a target. Free when unconfigured, and it does no I/O even
                      // when it is — the node is contacted on first use (ADR-0008).
    cmd::vmbackends::register_configured();
    match cli.cmd {
        Cmd::Container { action } => cmd::container::run(action),
        Cmd::Pod { action } => cmd::pod::run(action),
        Cmd::Image { action } => cmd::image::run(action),
        Cmd::Build(args) => cmd::build::run(args),
        Cmd::Vm { action } => cmd::vm::run(action),
        Cmd::Workload { action } => cmd::workload::run(action),
        Cmd::Volume { action } => cmd::volume::run(action),
        Cmd::Network { action } => cmd::network::run(action),
        Cmd::Secret { action } => cmd::secret::run(action),
        Cmd::Explain { path } => cmd::schema::explain(&path),
        Cmd::ApiResources { output } => cmd::resource::api_resources(output),
        Cmd::Apply {
            file,
            name,
            dry_run,
            replace,
            prune,
        } => cmd::stack::run(cmd::stack::StackCmd::Apply {
            name,
            file,
            dry_run,
            replace,
            prune,
        }),
        Cmd::Plan {
            file,
            name,
            output,
            detailed_exitcode,
            fields,
        } => cmd::stack::run(cmd::stack::StackCmd::Plan {
            file,
            name,
            output,
            detailed_exitcode,
            fields,
        }),
        Cmd::Wait { file, timeout } => {
            cmd::stack::run(cmd::stack::StackCmd::Wait { file, timeout })
        }
        Cmd::Manifest { action } => cmd::manifestcmd::run(action),
        Cmd::Get {
            kind,
            names,
            output,
            namespace,
        } => cmd::verbs::get(&kind, &names, output, namespace),
        Cmd::Describe { kind, names } => cmd::verbs::describe(&kind, &names),
        Cmd::Delete { kind, names, force } => cmd::verbs::delete(&kind, &names, force),
        Cmd::Diff {
            kind,
            name,
            file,
            detailed_exitcode,
        } => cmd::diff::cmd_diff(&kind, &name, file, detailed_exitcode),
        Cmd::Config { action } => cmd::config::run(action),
        Cmd::Stack { action } => cmd::stack::run(action),
        Cmd::Compose { action } => cmd::compose::run(action),
        // clap prints "<name> <long_version>"; reproduced here so `delonix version` and
        // `delonix --version` are byte-for-byte identical.
        Cmd::Init {
            dir,
            template,
            force,
        } => cmd::init::run(dir, template, force),
        Cmd::Version => {
            // CARGO_BIN_NAME, not CARGO_PKG_NAME: the package is `delonix-runtime-bin`
            // and the binary clap names is `delonix`. Caught by diffing the two outputs —
            // "byte-for-byte" is the requirement precisely because this misses by one word.
            println!("{} {}", env!("CARGO_BIN_NAME"), long_version_text());
            Ok(())
        }
        Cmd::System { action } => cmd::system::run(action),
        Cmd::Backup { action } => cmd::rbackup::cmd_backup(action),
        Cmd::Cluster { action } => cmd::cluster::run(action),
        Cmd::Net { action } => cmd::net::run(action),
        Cmd::Serve { action } => cmd::serve::run(action),
        Cmd::Mcp { action } => cmd::mcp::run(action),
        Cmd::IngressProxy { config } => cmd::ingress_proxy::run(&config),
        Cmd::Dashboard { scope, once, json } => {
            cmd::dash::run(scope.unwrap_or(cmd::dash::DashScope::Global), once, json)
        }
        Cmd::Completion { action } => match action {
            CompletionCmd::Shell { shell } => cmd_completion(shell),
            CompletionCmd::Editor { editor, dir } => cmd_syntax(editor, dir.as_deref()),
        },
        Cmd::Man(args) => cmd::man::run(args),
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

/// `delonix syntax <editor>` — the VMfile grammar, from the binary that owns
/// the format.
///
/// Carried inside the binary (`include_str!`) for the same two reasons the
/// completions are generated rather than shipped as files: the documented
/// install is `curl … | bash`, which has no repository to copy from, and a
/// grammar kept anywhere else drifts from the parser it is supposed to
/// describe. The files under `editors/` are the source of both.
///
/// With `--dir` it writes every file that editor needs (vim also needs the
/// `ftdetect` half, or the syntax is never applied to anything); without it,
/// it prints the one file that IS the highlighting, so
/// `delonix syntax vim > ~/.vim/syntax/vmfile.vim` does what it reads like.
fn cmd_syntax(editor: SyntaxEditor, dir: Option<&std::path::Path>) -> Result<()> {
    const VIM_SYNTAX: &str = include_str!("../../../editors/vim/syntax/vmfile.vim");
    const VIM_FTDETECT: &str = include_str!("../../../editors/vim/ftdetect/vmfile.vim");
    const VSCODE_GRAMMAR: &str =
        include_str!("../../../editors/vscode/syntaxes/vmfile.tmLanguage.json");
    const VSCODE_PACKAGE: &str = include_str!("../../../editors/vscode/package.json");
    const VSCODE_LANGCFG: &str =
        include_str!("../../../editors/vscode/language-configuration.json");

    let files: &[(&str, &str)] = match editor {
        SyntaxEditor::Vim => &[
            ("syntax/vmfile.vim", VIM_SYNTAX),
            ("ftdetect/vmfile.vim", VIM_FTDETECT),
        ],
        SyntaxEditor::Vscode => &[
            ("syntaxes/vmfile.tmLanguage.json", VSCODE_GRAMMAR),
            ("package.json", VSCODE_PACKAGE),
            ("language-configuration.json", VSCODE_LANGCFG),
        ],
    };
    let Some(dir) = dir else {
        print!("{}", files[0].1);
        return Ok(());
    };
    for (rel, body) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, body)?;
        // stdout stays clean for a caller that redirects it; the list of what
        // was written is progress, not output.
        eprintln!("{}", path.display());
    }
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
            // Same classification as the normal path below. It matters most for
            // `netns run` — the 2nd pass of `--net <custom-network>`, which IS
            // the process that resolves the image and creates the container, so
            // its failure class is the one a caller would want to read.
            std::process::exit(cmd::exitcode::for_error(&e));
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
            // Same classification as the normal path below. It matters most for
            // `netns run` — the 2nd pass of `--net <custom-network>`, which IS
            // the process that resolves the image and creates the container, so
            // its failure class is the one a caller would want to read.
            std::process::exit(cmd::exitcode::for_error(&e));
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
            // Same classification as the normal path below. It matters most for
            // `netns run` — the 2nd pass of `--net <custom-network>`, which IS
            // the process that resolves the image and creates the container, so
            // its failure class is the one a caller would want to read.
            std::process::exit(cmd::exitcode::for_error(&e));
        }
        std::process::exit(0);
    }
    if raw.len() == 3 && raw[1] == "__ovlmigrate" {
        if let Err(e) = cmd::mapped::ovlmigrate(std::path::Path::new(&raw[2])) {
            eprintln!("delonix: {}", cmd::po::t_dyn(&e.to_string()));
            std::process::exit(cmd::exitcode::for_error(&e));
        }
        std::process::exit(0);
    }
    if raw.len() == 3 && raw[1] == "__ovlhold" {
        if let Err(e) = cmd::mapped::ovlhold(std::path::Path::new(&raw[2])) {
            eprintln!("delonix: {}", cmd::po::t_dyn(&e.to_string()));
            std::process::exit(cmd::exitcode::for_error(&e));
        }
        std::process::exit(0);
    }
    if raw.len() == 4 && raw[1] == "__duusage" {
        if let Err(e) =
            cmd::mapped::duusage(std::path::Path::new(&raw[2]), std::path::Path::new(&raw[3]))
        {
            eprintln!("delonix: {}", cmd::po::t_dyn(&e.to_string()));
            // Same classification as the normal path below. It matters most for
            // `netns run` — the 2nd pass of `--net <custom-network>`, which IS
            // the process that resolves the image and creates the container, so
            // its failure class is the one a caller would want to read.
            std::process::exit(cmd::exitcode::for_error(&e));
        }
        std::process::exit(0);
    }
    if raw.len() == 4 && raw[1] == "__buildtar" {
        if let Err(e) =
            cmd::mapped::buildtar(std::path::Path::new(&raw[2]), std::path::Path::new(&raw[3]))
        {
            eprintln!("delonix: {}", cmd::po::t_dyn(&e.to_string()));
            // Same classification as the normal path below. It matters most for
            // `netns run` — the 2nd pass of `--net <custom-network>`, which IS
            // the process that resolves the image and creates the container, so
            // its failure class is the one a caller would want to read.
            std::process::exit(cmd::exitcode::for_error(&e));
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
    // Hidden verb for `pod port-forward` (see `cmd::pod::port_forward`): run
    // INSIDE the pod's netns via `join_argv`, with the accepted host-side
    // socket wired as our stdin+stdout. Not a public subcommand, same idiom as
    // `netns run`/`__rmtree` above.
    if raw.len() == 3 && raw[1] == "__netnsconnect" {
        if let Err(e) = cmd::pod::netnsconnect(&raw[2]) {
            eprintln!("delonix: {}", cmd::po::t_dyn(&e.to_string()));
            std::process::exit(cmd::exitcode::for_error(&e));
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
        // The class of the failure, not just "it failed" (see `cmd::exitcode`):
        // «no such resource» (4) and «exists but is not running» (3) used to be
        // the same 1 as a syscall dying halfway through, so the only way to tell
        // them apart was to grep the message — which is TRANSLATED, so the grep
        // stops working under `--l18n=pt`. Mapped in ONE place, from the error
        // TYPE: a second decision site is how two answers to the same question
        // start disagreeing.
        std::process::exit(cmd::exitcode::for_error(&e));
    }
}

/// The advisory names flags. They have to be flags that exist.
///
/// Written after shipping four that did not: `--memory-swap`, `--pids-limit`,
/// `--cpuset-mems` and `--io-max` are Docker's spellings, and `system resources`
/// was printing them at operators as things to stop using on an engine whose
/// flag is `--cpuset` and whose I/O ceilings are the `--device-*-bps` family.
/// Nothing could tell: they are string literals in another crate, and the
/// compiler has no opinion about the inside of a string.
#[cfg(test)]
mod advisory_flag_tests {
    use clap::CommandFactory;

    /// Every long flag `container run` accepts.
    fn container_run_flags() -> std::collections::BTreeSet<String> {
        let cmd = <super::Cli as CommandFactory>::command();
        let container = cmd
            .get_subcommands()
            .find(|c| c.get_name() == "container")
            .expect("the `container` group exists");
        let run = container
            .get_subcommands()
            .find(|c| c.get_name() == "run")
            .expect("`container run` exists");
        run.get_arguments()
            .filter_map(|a| a.get_long().map(|l| format!("--{l}")))
            .collect()
    }

    #[test]
    fn named_flags_all_exist() {
        let real = container_run_flags();
        let mut missing = Vec::new();
        for c in delonix_runtime::RESOURCE_CONTROLLERS {
            for f in delonix_runtime::flags_of_controller(c) {
                if !real.contains(*f) {
                    missing.push(format!("{c}: {f}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "`flags_of_controller` nomeia flags que `container run` não tem: {missing:?}\n\
             As reais são: {real:?}"
        );
    }

    /// The four that were wrong, named, so nobody reintroduces them by copying
    /// from Docker's documentation.
    #[test]
    fn the_docker_spellings_are_not_ours() {
        let real = container_run_flags();
        for f in ["--memory-swap", "--pids-limit", "--cpuset-mems", "--io-max"] {
            assert!(
                !real.contains(f),
                "{f} passou a existir — se é mesmo nossa, acrescenta-a ao \
                 `flags_of_controller` no mesmo commit"
            );
        }
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
    const ARG_HELP_PENDING: usize = 0;

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

    /// Os comentários dos exemplos do manual são texto de utilizador como
    /// qualquer outro: sob `--l18n=pt` um bloco EXEMPLOS em inglês seria a
    /// única coisa por traduzir no ecrã. As LINHAS DE COMANDO não entram —
    /// `delonix container run -d` escreve-se igual em todas as línguas, e
    /// pedir tradução para elas é como um catálogo ganha ruído.
    #[test]
    fn todo_o_comentario_de_exemplo_tem_traducao_pt() {
        let mut sem: Vec<String> = Vec::new();
        for e in crate::cmd::manual::ENTRIES {
            for (comment, _) in e.examples {
                if !crate::cmd::po::has_pt_translation(comment) {
                    sem.push(format!("{}: {comment}", e.path));
                }
            }
        }
        assert!(
            sem.is_empty(),
            "{} comentário(s) de exemplo sem entrada no pt.po:\n  {}",
            sem.len(),
            sem.join("\n  ")
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

/// `docs/cli-stability.md` classifies every root command group as either
/// "Estável" or "NÃO estável" — but nothing checked that classification was
/// EXHAUSTIVE. Measured before this test existed: of 33 real root groups,
/// only `container`/`image` (+ `build`, folded into the same promise) and the
/// 9 named in "NÃO estável" had any status at all — 21 groups (`apply`,
/// `compose`, `volume`, `network`, `secret`, `stack`, … ) were simply never
/// mentioned by either list. A group with no status is a group nobody has to
/// think about before breaking, which is worse than one explicitly marked
/// unstable.
///
/// This mirrors the doc's own two lists in Rust and checks them against the
/// REAL `clap` tree, not a hand-copied module count (that count — 58 of 71
/// `pub mod` — was itself wrong: most of those modules are internal
/// implementation files, not CLI groups a user ever types).
#[cfg(test)]
mod cli_stability_classification_tests {
    use clap::CommandFactory;

    /// Groups `docs/cli-stability.md`'s "Estável" section covers: the name,
    /// positional order, listed flags and exit codes don't change without a
    /// major. `build` is included here even though it is its own root
    /// command — the doc's fenced block names it in the same breath as
    /// `image`, as the promise for `delonix build`.
    const STABLE_GROUPS: &[&str] = &["container", "image", "build"];

    /// Every other real top-level group, mirroring "NÃO estável" 1:1. A group
    /// added to `Cmd` has to land in ONE of these two lists (and in
    /// `docs/cli-stability.md`) in the same commit — that's what
    /// `every_root_group_is_classified_exactly_once` enforces.
    const NOT_STABLE_GROUPS: &[&str] = &[
        "api-resources",
        "apply",
        "backup",
        "cluster",
        "compose",
        "completion",
        "config",
        "dashboard",
        "delete",
        "describe",
        "diff",
        "explain",
        "get",
        "init",
        "man",
        "manifest",
        "mcp",
        "net",
        "network",
        "plan",
        "pod",
        "secret",
        "serve",
        "stack",
        "system",
        "version",
        "vm",
        "volume",
        "wait",
        "workload",
    ];

    /// `help` is `clap`-synthesized; no version has ever promised anything
    /// about it. `ingress-proxy` is `#[command(hide = true)]` — real in the
    /// tree (so `get_subcommands()` sees it) but not a public surface a user
    /// ever types; `stack apply` launches it by argv, inside the holder's
    /// netns. Neither belongs in either stability list.
    const NOT_A_GROUP: &[&str] = &["help", "ingress-proxy"];

    #[test]
    fn every_root_group_is_classified_exactly_once() {
        let cmd = super::Cli::command();
        let mut unclassified = Vec::new();
        let mut double_classified = Vec::new();
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            if NOT_A_GROUP.contains(&name) {
                continue;
            }
            let stable = STABLE_GROUPS.contains(&name);
            let not_stable = NOT_STABLE_GROUPS.contains(&name);
            if stable && not_stable {
                double_classified.push(name.to_string());
            } else if !stable && !not_stable {
                unclassified.push(name.to_string());
            }
        }
        assert!(
            unclassified.is_empty(),
            "grupo(s) de topo sem classificação de estabilidade — acrescenta a \
             STABLE_GROUPS ou NOT_STABLE_GROUPS (e a docs/cli-stability.md) no \
             MESMO commit que os introduziu: {unclassified:?}"
        );
        assert!(
            double_classified.is_empty(),
            "grupo(s) na promessa de estabilidade E na lista de não-estáveis \
             ao mesmo tempo — só pode estar numa: {double_classified:?}"
        );
    }

    /// The inverse of the guard above: a name in either list that is no
    /// longer a real group is a promise (or a disclaimer) about something
    /// that stopped existing. That's exactly what happened to
    /// `storage`/`sharevolume` after #216 — they stayed in "NÃO estável" long
    /// after `delonix storage`/`delonix sharevolume` already answered
    /// "unrecognized subcommand".
    #[test]
    fn no_classified_name_outlives_its_group() {
        let cmd = super::Cli::command();
        let real: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        let stale: Vec<&str> = STABLE_GROUPS
            .iter()
            .chain(NOT_STABLE_GROUPS.iter())
            .filter(|name| !real.contains(name))
            .copied()
            .collect();
        assert!(
            stale.is_empty(),
            "nome(s) classificado(s) que já não são um grupo de comandos real \
             — actualiza STABLE_GROUPS/NOT_STABLE_GROUPS e docs/cli-stability.md \
             no mesmo commit que os removeu: {stale:?}",
        );
    }
}

/// The actual promise for `container`/`image`, checked against the real
/// `clap` tree instead of trusted on the page. There was no such check
/// before this — the doc could drift from the code (as it already had:
/// `-i`/`-t` were listed as if they belonged to `run`, when they have only
/// ever existed on `exec`) and nothing would notice until an operator's
/// script broke on an upgrade.
#[cfg(test)]
mod container_image_contract_tests {
    use clap::CommandFactory;
    use std::collections::BTreeSet;

    fn verb<'a>(cmd: &'a clap::Command, group: &str, verb: &str) -> &'a clap::Command {
        cmd.get_subcommands()
            .find(|c| c.get_name() == group)
            .unwrap_or_else(|| panic!("`delonix {group}` desapareceu"))
            .get_subcommands()
            .find(|c| c.get_name() == verb)
            .unwrap_or_else(|| panic!("`delonix {group} {verb}` desapareceu"))
    }

    fn subcommand_names(cmd: &clap::Command, group: &str) -> BTreeSet<String> {
        cmd.get_subcommands()
            .find(|c| c.get_name() == group)
            .unwrap_or_else(|| panic!("`delonix {group}` desapareceu"))
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .collect()
    }

    #[test]
    fn container_keeps_every_promised_verb() {
        let cmd = super::Cli::command();
        let real = subcommand_names(&cmd, "container");
        for name in [
            "run", "ps", "stop", "start", "restart", "kill", "rm", "exec", "logs", "wait",
            "inspect", "port", "rename", "pause", "unpause",
        ] {
            assert!(
                real.contains(name),
                "`container {name}` desapareceu — docs/cli-stability.md promete \
                 que não quebra sem um major"
            );
        }
    }

    #[test]
    fn image_keeps_every_promised_verb() {
        let cmd = super::Cli::command();
        let real = subcommand_names(&cmd, "image");
        for name in ["pull", "ls", "remove"] {
            assert!(
                real.contains(name),
                "`image {name}` desapareceu — docs/cli-stability.md promete \
                 que não quebra sem um major"
            );
        }
    }

    #[test]
    fn container_run_keeps_every_promised_flag() {
        let cmd = super::Cli::command();
        let run = verb(&cmd, "container", "run");
        let long: BTreeSet<String> = run
            .get_arguments()
            .filter_map(|a| a.get_long().map(|l| format!("--{l}")))
            .collect();
        for f in [
            "--name",
            "--rm",
            "--net",
            "--restart",
            "--memory",
            "--cpus",
            "--entrypoint",
            "--add-host",
            "--wait",
        ] {
            assert!(long.contains(f), "`container run {f}` desapareceu");
        }
        assert!(
            long.iter().any(|f| f.starts_with("--health-")),
            "`container run --health-*` desapareceu"
        );
        let short: BTreeSet<char> = run.get_arguments().filter_map(|a| a.get_short()).collect();
        for f in ['d', 'p', 'v', 'e', 'w', 'u'] {
            assert!(short.contains(&f), "`container run -{f}` desapareceu");
        }
    }

    #[test]
    fn container_exec_keeps_every_promised_flag() {
        let cmd = super::Cli::command();
        let exec = verb(&cmd, "container", "exec");
        let short: BTreeSet<char> = exec.get_arguments().filter_map(|a| a.get_short()).collect();
        for f in ['i', 't', 'e', 'w', 'u'] {
            assert!(short.contains(&f), "`container exec -{f}` desapareceu");
        }
    }
}

/// Every command this CLI names in its own text has to EXIST in this CLI.
///
/// This replaces `comandos_citados_tests`, which was written for the same class
/// and could not see any of it. That test compared only the FIRST word after
/// the tool name against the TOP-LEVEL subcommand names, over a hand-written
/// list of twelve files. So a reference to `pod ls` passed — `pod` is real, and
/// the second word was never read — and with it every one of these, live in
/// v3.0.0:
///
/// * the top-level `--help` advertised three verbs that answer `unrecognized
///   subcommand` (`backup … list`, `pod … ls`, `vm … status`), plus `net
///   httproute … ls` and an `image vm … etc`;
/// * ten messages told the operator to run one — four `pod ls`, a `pod
///   describe`, a `pod rm -f`, a `vm rm`, an `image inspect`, and, worst of the
///   set, a `vm status` printed in the "next steps" block right after a
///   SUCCESSFUL `vm create`, which is exactly where somebody new goes next.
///
/// Two shapes go stale when a command is removed, so there are two tests, both
/// against the live `clap` tree and never a second list to forget:
/// [`no_group_summary_advertises_a_verb_that_does_not_exist`] and
/// [`no_string_in_the_sources_tells_the_operator_to_run_a_dead_command`].
///
/// The origin story of the old test is worth keeping: it came from using the
/// product, when `net boot status` said «run `delonix boot enable`» — a
/// recovery instruction pointing at a command removed 19 versions earlier.
#[cfg(test)]
mod dead_command_reference_tests {
    use clap::CommandFactory;
    use std::collections::{BTreeMap, BTreeSet};

    /// path (`["net", "ingress"]`) → the names AND aliases of its subcommands.
    /// A command absent from this map is a leaf: whatever follows it on a
    /// command line is an argument, and arguments are none of our business.
    fn tree(
        cmd: &clap::Command,
        path: Vec<String>,
        out: &mut BTreeMap<Vec<String>, BTreeSet<String>>,
    ) {
        let kids: BTreeSet<String> = cmd
            .get_subcommands()
            .flat_map(|s| {
                std::iter::once(s.get_name().to_string())
                    .chain(s.get_all_aliases().map(|a| a.to_string()))
            })
            .collect();
        if kids.is_empty() {
            return;
        }
        for sub in cmd.get_subcommands() {
            let mut p = path.clone();
            p.push(sub.get_name().to_string());
            tree(sub, p, out);
        }
        out.insert(path, kids);
    }

    /// Trailing punctuation is the norm here, not the exception: these
    /// references live inside backticks, parentheses and sentences (``(see
    /// `delonix get pods`)``). Without this the token would be ``pods`)`` and
    /// would never be checked — a silent hole exactly where the real bugs were.
    fn clean(tok: &str) -> &str {
        tok.trim_matches(|c: char| "`'\".,;:)(!?".contains(c))
    }

    fn is_word(tok: &str) -> bool {
        !tok.is_empty()
            && tok.starts_with(|c: char| c.is_ascii_lowercase())
            && tok
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }

    fn subcommand_at<'a>(root: &'a clap::Command, path: &[String]) -> Option<&'a clap::Command> {
        let mut cur = root;
        for seg in path {
            cur = cur.get_subcommands().find(|s| s.get_name() == seg)?;
        }
        Some(cur)
    }

    /// Slash runs anywhere in the summary, plus the comma list after a colon.
    fn candidate_lists(about: &str) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        for chunk in about.split_whitespace() {
            if chunk.contains('/') {
                out.push(chunk.split('/').map(str::to_string).collect());
            }
        }
        if let Some((_, tail)) = about.split_once(": ") {
            out.push(tail.split(',').map(|s| s.trim().to_string()).collect());
        }
        out
    }

    /// A group's summary lists its own verbs (`create/logs/exec/cp/attach`).
    ///
    /// **The majority rule is what makes this usable.** The same punctuation
    /// carries prose that only LOOKS like a verb list — `Archives of ONE
    /// resource (container/pod/vm/stack)`, `Low-level network/infra plumbing`,
    /// ``Kubernetes clusters: `kubeadm` bootstrap, VM provisioning`` — and
    /// demanding those resolve would flood this test with noise until somebody
    /// deleted it. So a candidate list is JUDGED only when more than half of
    /// its tokens already are subcommands of that group; then every one of them
    /// has to be. It cannot catch a list that was wrong from the start, and it
    /// is not meant to: the failure mode is a removal turning one verb of an
    /// otherwise correct list stale, which is what the last three releases did.
    #[test]
    fn no_group_summary_advertises_a_verb_that_does_not_exist() {
        let cmd = super::Cli::command();
        let mut t = BTreeMap::new();
        tree(&cmd, vec![], &mut t);
        let mut bad = Vec::new();
        for (path, kids) in &t {
            let label = if path.is_empty() {
                String::new()
            } else {
                format!(" {}", path.join(" "))
            };
            let Some(about) =
                subcommand_at(&cmd, path).and_then(|c| c.get_about().map(|s| s.to_string()))
            else {
                continue;
            };
            for list in candidate_lists(&about) {
                let toks: Vec<&str> = list
                    .iter()
                    .map(|s| clean(s))
                    .filter(|t| is_word(t))
                    .collect();
                if toks.len() < 2 {
                    continue;
                }
                let hits = toks.iter().filter(|t| kids.contains(**t)).count();
                if hits * 2 <= toks.len() {
                    continue; // prose, not a verb list
                }
                for t in toks.iter().filter(|t| !kids.contains(**t)) {
                    bad.push(format!(
                        "`delonix{label}` advertises `{t}` in its summary — no such subcommand"
                    ));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "{} lying summary/summaries:\n  {}",
            bad.len(),
            bad.join("\n  ")
        );
    }

    /// The `delonix …` runs on a line that are DELIMITED as commands: the word
    /// has to sit immediately after a backtick or a double quote, and the span
    /// ends at the matching one.
    ///
    /// That is not tidiness, it is what separates a command from a sentence.
    /// ``the `delonix network` that owns it`` closes the backtick after
    /// `network`, so `that` never enters the span; ``(see `delonix get pods`)``
    /// puts the whole path inside it. Reading to end-of-line instead flagged
    /// fifteen ordinary English comments, and a test that cries wolf gets
    /// deleted — which is how the previous one ended up matching only the first
    /// word.
    fn command_spans(line: &str) -> Vec<&str> {
        let b = line.as_bytes();
        let mut out = Vec::new();
        for (i, _) in line.match_indices("delonix ") {
            if i == 0 {
                continue;
            }
            let delim = b[i - 1];
            if delim != b'`' && delim != b'"' {
                continue;
            }
            let rest = &line[i + 8..];
            out.push(match rest.find(delim as char) {
                Some(end) => &rest[..end],
                None => rest,
            });
        }
        out
    }

    /// Every crate source, not a hand-written list of twelve: the messages that
    /// were stale live in `pod.rs`, `workload.rs`, `dockerapi.rs` and
    /// `manual_entries.rs`, and none of those was on it.
    fn rust_sources() -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        out
    }

    /// Every `delonix <path>` written in this crate's sources — error messages,
    /// "next steps" blocks, manual examples, doc comments — has to resolve.
    ///
    /// Walking a span stops at the first thing that cannot be a subcommand: a
    /// flag, a `<placeholder>`, a `{interpolation}`, or simply a leaf, after
    /// which everything is an argument. A first token that is not a top-level
    /// command means the sentence was about something else and is left alone —
    /// which is also why `` `delonix netns holder` `` needs no exception here:
    /// that re-exec is intercepted from the raw `std::env::args()` before clap
    /// sees anything, deliberately absent from the public tree.
    #[test]
    fn no_string_in_the_sources_tells_the_operator_to_run_a_dead_command() {
        let cmd = super::Cli::command();
        let mut t = BTreeMap::new();
        tree(&cmd, vec![], &mut t);
        let mut bad = Vec::new();
        for file in rust_sources() {
            let src = std::fs::read_to_string(&file).unwrap_or_default();
            for (lineno, line) in src.lines().enumerate() {
                for span in command_spans(line) {
                    let mut path: Vec<String> = Vec::new();
                    for raw in span.split_whitespace() {
                        let Some(kids) = t.get(&path) else { break }; // leaf: arguments from here on
                        let tok = clean(raw);
                        if !is_word(tok) {
                            break;
                        }
                        if !kids.contains(tok) {
                            if !path.is_empty() {
                                bad.push(format!(
                                    "{}:{}: `delonix {} {tok}` — no such `{tok}`",
                                    file.display(),
                                    lineno + 1,
                                    path.join(" ")
                                ));
                            }
                            break;
                        }
                        path.push(tok.to_string());
                    }
                }
            }
        }
        assert!(
            bad.is_empty(),
            "{} reference(s) to commands that do not exist:\n  {}",
            bad.len(),
            bad.join("\n  ")
        );
    }
}
