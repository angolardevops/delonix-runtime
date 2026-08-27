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
    ///
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
    // A namespace has no record of its own — it exists while something is in
    // it — so there is deliberately no `create`/`rm`: an empty namespace would
    // be a resource with a lifecycle, and that is an ADR, not a listing. Said
    // in the `about` (one paragraph, so `about == long_about`) rather than a
    // second paragraph: the catalog looks the rendered string up VERBATIM, and
    // a multi-paragraph help comes out untranslated under `--l18n=pt`.
    /// Isolation namespaces: ls/describe — a namespace exists while something is in it, so there is no create/rm.
    Namespace {
        #[command(subcommand)]
        action: cmd::namespace::NamespaceCmd,
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
    /// An isolated, individually-quota'd slice of a `Storage`.
    ///
    /// Multiple container/vm/pod share ONE NAS export without seeing each
    /// other's data.
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
    /// Archives of ONE resource (container/pod/vm/stack): create, list, inspect, restore, schedule, remove.
    ///
    /// The archive carries the record and the DATA of the volumes it uses — not
    /// the image and not the rootfs, which `backup restore` derives by pulling.
    /// A VM is the exception: its overlay disk IS its state, so that one travels.
    ///
    /// For the whole node — registries, secrets, cluster PKI, the event log —
    /// the command is `delonix system backup`, which is a different scope and
    /// not a second door to this one.
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
    /// Runtime summary/KPI dashboard (interactive htop-style TUI).
    ///
    /// Global, or per group (`container dash`, `vm dash`, ...). Renamed from
    /// `dash` (§22): the group is declared NOT stable, and an old spelling that
    /// keeps working is one nobody migrates away from — `delonix dash` now fails
    /// loudly instead of quietly staying the habit.
    Dashboard {
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
    let mut argv: Vec<String> = std::env::args().collect();
    expand_alias(&mut argv);
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
        Cmd::Image { vm, action } => cmd::image::run(vm, action),
        Cmd::Build(args) => cmd::build::run(args),
        Cmd::Vm { action } => cmd::vm::run(action),
        Cmd::Workload { action } => cmd::workload::run(action),
        Cmd::Volumes { action } => cmd::volume::run(action),
        Cmd::Namespace { action } => cmd::namespace::run(action),
        Cmd::Network { action } => cmd::network::run(action),
        Cmd::Secret { action } => cmd::secret::run(action),
        Cmd::Storage { action } => cmd::storage::run(action),
        Cmd::Sharevolume { action } => cmd::sharevolume::run(action),
        Cmd::Schema { action } => cmd::schema::run(action),
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
        } => cmd::verbs::get(&kind, &names, output),
        Cmd::Describe { kind, names } => cmd::verbs::describe(&kind, &names),
        Cmd::Delete { kind, names, force } => cmd::verbs::delete(&kind, &names, force),
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
        Cmd::IngressProxy { config } => cmd::ingress_proxy::run(&config),
        Cmd::Dashboard { once, json } => {
            cmd::dash::run(cmd::dash::DashScope::Global, once, json)
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

/// Every user-facing help string has to have a Portuguese translation, and the
/// only honest way to know is to walk the built `Command` and ask the catalog.
///
/// Measured before this test existed: **103 of the 232 subcommands printed
/// their help in English under `--l18n=pt`** — the `container` group, the
/// everyday surface, was 28 of them. The mechanism was never broken (see
/// `po::translate_help`); what was missing were the catalog entries, and
/// nothing was watching.
/// Every `delonix <x>` a message tells the user to run has to EXIST.
///
/// The v0.30.0 reorganization moved seven groups under `net` and three under
/// `serve`, as a clean cut with no aliases — which is the right call for a CLI,
/// and leaves every message that still names the old form telling the operator
/// to run something that answers `unrecognized subcommand`.
///
/// Found by using the product: after a reboot, `net boot status` said
/// «run `delonix boot enable`» — the recovery instruction, pointing at a command
/// removed 19 versions earlier. Five more were sitting next to it (`tunnel ls`,
/// `tunnel describe`, `ingress publish`/`unpublish`). None of them is caught by
/// anything else: they are string literals, and the compiler has no opinion
/// about the inside of a string.
#[cfg(test)]
mod comandos_citados_tests {
    use clap::CommandFactory;

    /// Words that follow `delonix ` in prose without naming a subcommand.
    ///
    /// Kept EXPLICIT rather than heuristic: a regex that skipped anything it did
    /// not recognise would skip the typo too, and this test exists precisely to
    /// catch the name that stopped being real.
    const NAO_SAO_COMANDOS: &[&str] = &[
        "run",
        "ps",
        "exec",
        "logs",
        "rm",
        "images", // atalhos de topo (after_help)
        "<group>",
        "<grupo>",
        "<x>",
        "<command>",
        "<comando>",
        "--help",
        "--version",
        "-h",
        "-v",
        "engine",
        "runtime",
        // `delonix netns holder|run` EXISTE — é o re-exec interno do holder,
        // interceptado a partir de `std::env::args()` CRUA em `main()`, antes de
        // o clap ver seja o que for. Não estar na árvore do clap é deliberado
        // (não é superfície pública), e por isso tem de ser dito aqui em vez de
        // o teste o dar como morto.
        "netns",
    ];

    fn fontes() -> Vec<(&'static str, &'static str)> {
        vec![
            ("boot.rs", include_str!("cmd/boot.rs")),
            ("tunnel.rs", include_str!("cmd/tunnel.rs")),
            ("vm.rs", include_str!("cmd/vm.rs")),
            ("container.rs", include_str!("cmd/container.rs")),
            ("network.rs", include_str!("cmd/network.rs")),
            ("volume.rs", include_str!("cmd/volume.rs")),
            ("firewall.rs", include_str!("cmd/firewall.rs")),
            ("rbackup.rs", include_str!("cmd/rbackup.rs")),
            ("system.rs", include_str!("cmd/system.rs")),
            ("stack.rs", include_str!("cmd/stack.rs")),
            ("image.rs", include_str!("cmd/image.rs")),
            ("vmimage.rs", include_str!("cmd/vmimage.rs")),
        ]
    }

    #[test]
    fn nenhuma_mensagem_manda_correr_um_comando_que_nao_existe() {
        let cmd = super::Cli::command();
        let reais: Vec<String> = cmd
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .collect();

        let mut mortos: Vec<String> = Vec::new();
        for (ficheiro, src) in fontes() {
            for (i, linha) in src.lines().enumerate() {
                // Só o que está entre CRASES. É como este código escreve um
                // comando executável, em comentários e em mensagens; sem esta
                // restrição a varredura casa prosa (`delonix and`, `delonix tem`)
                // e o ruído afogaria o achado — que foi exactamente o que a 1.ª
                // versão deste teste fez.
                let mut resto = linha;
                while let Some(p) = resto.find("`delonix ") {
                    resto = &resto[p + "`delonix ".len()..];
                    let palavra: String = resto
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                        .collect();
                    if palavra.is_empty()
                        || NAO_SAO_COMANDOS.contains(&palavra.as_str())
                        || reais.contains(&palavra)
                    {
                        continue;
                    }
                    mortos.push(format!("{ficheiro}:{}: delonix {palavra}", i + 1));
                }
            }
        }
        assert!(
            mortos.is_empty(),
            "mensagem(ns) a mandar correr um comando inexistente:\n  {}",
            mortos.join("\n  ")
        );
    }
}

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
