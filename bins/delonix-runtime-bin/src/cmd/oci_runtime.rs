//! `delonix oci-runtime` — the engine's own init behind the runc command-line
//! contract (`create/start/state/kill/delete <id> --bundle <dir> --root <state>`),
//! so Delonix *is* an OCI runtime, not only a producer of bundles for one
//! (ADR-0080, GAP-RT-4). This file is Phase 1a: the OCI `state` model and the
//! pure `config.json` → `Container` translator. The CLI handlers + the live
//! lifecycle (wiring `create_with`/`send_signal`/`remove`) are Phase 1b.
//!
//! The key reuse (see `docs/dev/rt4-phase1-impl-map.md`): `delonix_linux::create_with`
//! launches from a `Container` + a rootfs PATH, not an image — and a bundle
//! already carries `rootfs/` + `config.json`, so no image store is involved.

use delonix_compute::Container;
use delonix_model::records::Status;
use delonix_model::{Error, Result};
use delonix_state::Store;
use oci_spec::runtime::Spec;
use std::path::{Path, PathBuf};

use delonix_linux::{self as runtime};

/// `delonix oci-runtime` — the runc command-line contract over the engine's init.
#[derive(clap::Args)]
pub struct OciRuntimeArgs {
    /// State directory (the per-runtime root, like `runc --root`). Default:
    /// `$XDG_RUNTIME_DIR/delonix-runtime` (rootless) or `/run/delonix-runtime`.
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,
    #[command(subcommand)]
    pub cmd: OciRuntimeCmd,
}

#[derive(clap::Subcommand)]
pub enum OciRuntimeCmd {
    /// Create a container from an OCI bundle WITHOUT starting it.
    Create {
        id: String,
        /// Path to the OCI bundle (its `rootfs/` and `config.json`).
        #[arg(short = 'b', long)]
        bundle: PathBuf,
    },
    /// Start a container previously `create`d.
    Start { id: String },
    /// Print the OCI state JSON of a container.
    State { id: String },
    /// Send a signal to a container (default `SIGTERM`).
    Kill {
        id: String,
        #[arg(default_value = "SIGTERM")]
        signal: String,
    },
    /// Delete a container's state (teardown; `--force` kills it first).
    Delete {
        id: String,
        #[arg(short, long)]
        force: bool,
    },
}

/// The per-runtime state dir (`--root`), honouring the default when unset.
fn state_root(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(|x| PathBuf::from(x).join("delonix-runtime"))
            .unwrap_or_else(|| PathBuf::from("/run/delonix-runtime"))
    })
}

/// Map the engine's container status to the OCI runtime-spec `state.status`
/// (one of `creating`/`created`/`running`/`stopped`).
fn oci_status(s: &Status) -> &'static str {
    match s {
        Status::Created => "created",
        Status::Running | Status::Paused => "running",
        Status::Stopped | Status::Failed(_) | Status::Crashed => "stopped",
    }
}

/// The bundle path a `Container` was created from — stored in its `image` field
/// as `oci-bundle:<path>` by [`spec_to_container`].
fn bundle_of(c: &Container) -> Option<&str> {
    c.image.strip_prefix("oci-bundle:")
}

pub fn run(args: OciRuntimeArgs) -> Result<()> {
    let root = state_root(args.root);
    let store = Store::open(root.join("containers"))?;
    match args.cmd {
        OciRuntimeCmd::Create { id, bundle } => create(&store, &id, &bundle),
        OciRuntimeCmd::Start { id } => start(&store, &id),
        OciRuntimeCmd::State { id } => state(&store, &id),
        OciRuntimeCmd::Kill { id, signal } => super::container::cmd_kill(&store, &id, &signal),
        OciRuntimeCmd::Delete { id, force } => delete(&store, &id, force),
    }
}

fn create(store: &Store, id: &str, bundle: &Path) -> Result<()> {
    let bundle = bundle
        .canonicalize()
        .map_err(|e| Error::Invalid(format!("oci-runtime: bundle '{}': {e}", bundle.display())))?;
    let spec = Spec::load(bundle.join("config.json"))
        .map_err(|e| Error::Invalid(format!("oci-runtime: config.json: {e}")))?;
    let (mut c, _rootfs) = spec_to_container(id, &bundle, &spec)?;
    c.status = Status::Created;
    store.save(&c)?;
    println!("{id}");
    Ok(())
}

fn start(store: &Store, id: &str) -> Result<()> {
    let mut c = super::util::find(store, id)?;
    runtime::reconcile_status(&mut c);
    if matches!(c.status, Status::Running | Status::Paused) {
        return Err(Error::Invalid(format!("oci-runtime: {id} is already running")));
    }
    // Recover the bundle + re-derive the rootfs the init pivots into. A runc
    // bundle has no delonix network, so this is the host-net launch path.
    let bundle = bundle_of(&c)
        .ok_or_else(|| Error::Invalid(format!("oci-runtime: {id} has no bundle recorded")))?
        .to_string();
    let spec = Spec::load(Path::new(&bundle).join("config.json"))
        .map_err(|e| Error::Invalid(format!("oci-runtime: config.json: {e}")))?;
    let (_c2, rootfs) = spec_to_container(id, Path::new(&bundle), &spec)?;
    let spec = runtime::RunSpec {
        detach: true,
        userns: c.userns,
        ..Default::default()
    };
    runtime::create_with(store, &mut c, &rootfs, &spec)?;
    Ok(())
}

fn state(store: &Store, id: &str) -> Result<()> {
    let mut c = super::util::find(store, id)?;
    runtime::reconcile_status(&mut c);
    let st = OciState {
        oci_version: "1.3.0".to_string(),
        id: c.id.clone(),
        status: oci_status(&c.status).to_string(),
        pid: c.pid.unwrap_or(0),
        bundle: bundle_of(&c).unwrap_or("").to_string(),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&st)
            .map_err(|e| Error::Invalid(format!("oci-runtime: state json: {e}")))?
    );
    Ok(())
}

fn delete(store: &Store, id: &str, force: bool) -> Result<()> {
    let c = super::util::find(store, id)?;
    runtime::remove(store, &c, force)?;
    println!("{}", c.id);
    Ok(())
}

/// The OCI runtime **state** (`runtime-spec/schema/state-schema.json`): what
/// `oci-runtime state <id>` prints and what a bundle-keyed record under `--root`
/// persists between `create` and `start`. Serialized with the OCI field names.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct OciState {
    #[serde(rename = "ociVersion")]
    pub oci_version: String,
    pub id: String,
    /// `creating` | `created` | `running` | `stopped` (runtime-spec).
    pub status: String,
    /// Init PID as seen by the runtime; `0` before `start`.
    pub pid: i32,
    /// Absolute path of the bundle the container was `create`d from.
    pub bundle: String,
}

impl OciState {
    pub fn created(id: &str, bundle: &str) -> Self {
        Self {
            // Match what `delonix image export` stamps and what runc emits.
            oci_version: "1.3.0".to_string(),
            id: id.to_string(),
            status: "created".to_string(),
            pid: 0,
            bundle: bundle.to_string(),
        }
    }
}

/// Translates a bundle's `config.json` (already parsed) into the `Container` the
/// init runs, plus the absolute rootfs path. PURE (no IO) so it is unit-testable
/// without running a container. Fields the init cannot enforce are **refused by
/// name** (guardrail 6), never silently dropped.
///
/// `id` names the container; `bundle` is the bundle directory (its `rootfs/` and
/// `config.json` live there); `spec` is the parsed `config.json`.
pub fn spec_to_container(id: &str, bundle: &Path, spec: &Spec) -> Result<(Container, String)> {
    // Refuse-by-name the pieces Phase 1 does not honour, so a caller never
    // believes they took effect (the whiteout/--add-host audits' lesson).
    if spec.hooks().is_some() {
        return Err(Error::Invalid(format!(
            "oci-runtime: bundle '{}' declares OCI hooks — not implemented (this engine has no \
             OCI-hook stdin-state protocol); remove `hooks` or run it under runc",
            bundle.display()
        )));
    }

    let process = spec
        .process()
        .as_ref()
        .ok_or_else(|| Error::Invalid("oci-runtime: config.json has no `process`".to_string()))?;
    let args: Vec<String> = process.args().clone().unwrap_or_default();
    if args.is_empty() {
        return Err(Error::Invalid(
            "oci-runtime: config.json `process.args` is empty".to_string(),
        ));
    }

    // Memory ceiling from `linux.resources.memory.limit` when present; else the
    // engine's own default (`max` lets the engine derive its quarter-budget, as
    // `container run` without `-m` does). Same honest default as the CLI.
    let memory_max = spec
        .linux()
        .as_ref()
        .and_then(|l| l.resources().as_ref())
        .and_then(|r| r.memory().as_ref())
        .and_then(|m| m.limit())
        .map(|bytes| bytes.to_string())
        .unwrap_or_else(|| "max".to_string());

    // `root.path` is relative to the bundle (OCI). Resolve to an absolute path
    // the init pivots into.
    let root_path = spec
        .root()
        .as_ref()
        .ok_or_else(|| Error::Invalid("oci-runtime: config.json has no `root`".to_string()))?
        .path();
    let rootfs = if root_path.is_absolute() {
        root_path.to_path_buf()
    } else {
        bundle.join(root_path)
    };

    let mut c = Container::new(
        id.to_string(),
        id.to_string(),
        // The "image" of a bundle container is the bundle itself; recorded for
        // provenance, never resolved against the image store.
        format!("oci-bundle:{}", bundle.display()),
        args,
        memory_max,
    );
    c.env = process.env().clone().unwrap_or_default();
    c.workdir = {
        let cwd = process.cwd();
        let s = cwd.to_string_lossy();
        if s.is_empty() || s == "/" {
            None
        } else {
            Some(s.into_owned())
        }
    };
    if let Some(h) = spec.hostname().as_ref() {
        c.hostname = Some(h.clone());
    }
    // A user namespace in the bundle means a rootless-style launch (the mappings
    // come from the bundle's own `linux.uidMappings`, which the init honours).
    c.userns = spec
        .linux()
        .as_ref()
        .and_then(|l| l.namespaces().as_ref())
        .map(|ns| {
            ns.iter().any(|n| {
                n.typ() == oci_spec::runtime::LinuxNamespaceType::User
            })
        })
        .unwrap_or(false);

    Ok((c, rootfs.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_spec::runtime::{ProcessBuilder, RootBuilder, SpecBuilder};
    use std::path::PathBuf;

    fn spec_with(args: &[&str], cwd: &str, env: &[&str]) -> Spec {
        let process = ProcessBuilder::default()
            .args(args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .cwd(cwd)
            .env(env.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .build()
            .expect("process");
        let root = RootBuilder::default().path("rootfs").build().expect("root");
        SpecBuilder::default()
            .version("1.3.0")
            .root(root)
            .process(process)
            .build()
            .expect("spec")
    }

    #[test]
    fn translates_core_fields_and_resolves_rootfs() {
        let spec = spec_with(&["/bin/sh", "-c", "echo hi"], "/work", &["PATH=/usr/bin"]);
        let bundle = PathBuf::from("/srv/bundles/c1");
        let (c, rootfs) = spec_to_container("c1", &bundle, &spec).expect("translate");

        assert_eq!(c.id, "c1");
        assert_eq!(c.command, vec!["/bin/sh", "-c", "echo hi"]);
        assert_eq!(c.env, vec!["PATH=/usr/bin"]);
        assert_eq!(c.workdir.as_deref(), Some("/work"));
        // `root.path` "rootfs" is relative to the bundle → absolute under it.
        assert_eq!(rootfs, "/srv/bundles/c1/rootfs");
    }

    #[test]
    fn refuses_hooks_by_name() {
        use oci_spec::runtime::{Hook, HookBuilder, HooksBuilder};
        let mut spec = spec_with(&["/bin/true"], "/", &[]);
        let hook: Hook = HookBuilder::default()
            .path("/bin/true")
            .build()
            .expect("hook");
        spec.set_hooks(Some(
            HooksBuilder::default()
                .prestart(vec![hook])
                .build()
                .expect("hooks"),
        ));
        let err = spec_to_container("c1", Path::new("/b"), &spec)
            .expect_err("hooks must be refused, not ignored");
        assert!(err.to_string().contains("hooks"), "{err}");
    }

    #[test]
    fn empty_args_is_refused() {
        let process = ProcessBuilder::default()
            .args(Vec::<String>::new())
            .cwd("/")
            .build()
            .expect("process");
        let root = RootBuilder::default().path("rootfs").build().expect("root");
        let spec = SpecBuilder::default()
            .version("1.3.0")
            .root(root)
            .process(process)
            .build()
            .expect("spec");
        assert!(spec_to_container("c1", Path::new("/b"), &spec).is_err());
    }

    #[test]
    fn oci_state_serializes_with_oci_field_names() {
        let s = OciState::created("c1", "/srv/bundles/c1");
        let j = serde_json::to_value(&s).expect("json");
        assert_eq!(j["ociVersion"], "1.3.0");
        assert_eq!(j["id"], "c1");
        assert_eq!(j["status"], "created");
        assert_eq!(j["pid"], 0);
        assert_eq!(j["bundle"], "/srv/bundles/c1");
    }
}
