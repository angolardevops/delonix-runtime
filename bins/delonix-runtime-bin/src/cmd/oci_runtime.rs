//! `delonix oci-runtime` — the engine's own init behind the runc command-line
//! contract (`create/start/state/kill/delete <id> --bundle <dir> --root <state>`),
//! so Delonix *is* an OCI runtime, not only a producer of bundles for one
//! (ADR-0080, GAP-RT-4). Phase 1a is the OCI `state` model + the pure
//! `config.json` → `Container` translator; Phase 1b is the CLI handlers + the
//! live lifecycle (`create_with`/`send_signal`/`remove`); Phase 2 broadens the
//! translator — `process.user`, bind mounts, CPU bandwidth/weight, an isolated
//! network namespace — and refuses by name what has no honoured path here
//! (interactive terminal, per-container pids limit, cpuset, joining a netns).
//!
//! The key reuse (see `docs/discovery/69_RT4_PHASE1_IMPL_MAP.md`): `delonix_linux::create_with`
//! launches from a `Container` + a rootfs PATH, not an image — and a bundle
//! already carries `rootfs/` + `config.json`, so no image store is involved.

use delonix_compute::{Container, Mount};
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
    // `gather`/`spawn` read uid/gid/mounts from the RunSpec, not the Container
    // (launch_spec.rs is the engine's own builder): carry them explicitly or the
    // translated `process.user`/`mounts` become silent no-ops on launch.
    let spec = runtime::RunSpec {
        detach: true,
        userns: c.userns,
        new_netns: c.net_mode.as_deref() == Some("none"),
        run_uid: c.run_uid,
        run_gid: c.run_gid,
        mounts: c.mounts.clone(),
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
    // Refuse-by-name the pieces this engine does not honour, so a caller never
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
    // An interactive terminal has no honoured path here (the only pty is the
    // privileged+detached+logged `/dev/console` for a systemd PID 1, not an OCI
    // terminal). Refuse `terminal: true` rather than run without the tty the
    // caller asked for.
    if process.terminal() == Some(true) {
        return Err(Error::Invalid(
            "oci-runtime: config.json `process.terminal: true` — an interactive terminal is not \
             implemented; set it to false or run it under runc"
                .to_string(),
        ));
    }

    // `linux.resources`: only the controllers delegated to a rootless user on
    // cgroup2 can be enforced (memory + cpu bandwidth/weight). The rest is
    // refused by name so a limit the caller set never silently fails to apply.
    let resources = spec.linux().as_ref().and_then(|l| l.resources().as_ref());
    // A per-container pids.max is NOT honoured: the init writes a fixed
    // DEFAULT_PIDS_MAX to the leaf `pids.max` (only an aggregate group ceiling
    // exists). Refuse rather than ignore a requested limit.
    if resources.and_then(|r| r.pids().as_ref()).is_some() {
        return Err(Error::Invalid(
            "oci-runtime: config.json `linux.resources.pids` — a per-container pids limit is not \
             honoured here (the engine writes a fixed leaf pids.max); remove it or run it under runc"
                .to_string(),
        ));
    }
    let cpu = resources.and_then(|r| r.cpu().as_ref());
    // cpuset (cpus/mems pinning) needs the `cpuset` controller, which is not
    // delegated to a rootless user here — refuse rather than fail mid-launch
    // writing `cpuset.cpus`.
    if let Some(cpu) = cpu {
        if cpu.cpus().is_some() || cpu.mems().is_some() {
            return Err(Error::Invalid(
                "oci-runtime: config.json `linux.resources.cpu.cpus`/`mems` (cpuset) — the cpuset \
                 controller is not delegated rootless; remove it or run it under runc"
                    .to_string(),
            ));
        }
    }
    // Memory ceiling from `linux.resources.memory.limit` when present; else the
    // engine's own default (`max` lets the engine derive its quarter-budget, as
    // `container run` without `-m` does). Same honest default as the CLI.
    let memory_max = resources
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

    // `process.user`: honour a non-root uid/gid (the common rootless-image case).
    // uid/gid 0 is the container-root default the init already runs as, so leave
    // `run_uid`/`run_gid` unset there (keeps the proven root path untouched).
    // `additionalGids` always map to supplementary groups.
    let user = process.user();
    if user.uid() != 0 {
        c.run_uid = Some(user.uid());
    }
    if user.gid() != 0 {
        c.run_gid = Some(user.gid());
    }
    if let Some(gids) = user.additional_gids() {
        c.group_add = gids.clone();
    }

    // `linux.resources.cpu`: quota/period → a cores string (`cpu.max`), shares →
    // a cgroup2 `cpu.weight` (the kernel's own shares↦weight conversion). Both
    // controllers are delegated, so these actually take effect.
    if let Some(cpu) = cpu {
        if let Some(cores) = cpu_cores(cpu.quota(), cpu.period()) {
            c.cpus = cores;
        }
        if let Some(w) = cpu.shares().map(shares_to_weight) {
            c.cpu_weight = Some(w.to_string());
        }
    }

    // Bind mounts the caller added (the `-v` case). The standard pseudo-
    // filesystems (proc/sys/dev/devpts/mqueue/shm/cgroup) are set up by the init
    // itself, so they are skipped — passing them as binds would double-mount.
    c.mounts = oci_bind_mounts(spec);

    // Namespaces: a user namespace means a rootless-style launch (mappings come
    // from the bundle's own `linux.uidMappings`, which the init honours). A
    // network namespace with NO path is an isolated netns (`--net none`); one
    // WITH a path asks to join an existing netns, which has no honoured path.
    if let Some(nss) = spec.linux().as_ref().and_then(|l| l.namespaces().as_ref()) {
        use oci_spec::runtime::LinuxNamespaceType as NsT;
        for ns in nss {
            match ns.typ() {
                NsT::User => c.userns = true,
                NsT::Network => {
                    if ns.path().is_some() {
                        return Err(Error::Invalid(
                            "oci-runtime: config.json declares a network namespace with a `path` \
                             (join an existing netns) — not implemented; remove the path (for an \
                             isolated netns) or run it under runc"
                                .to_string(),
                        ));
                    }
                    c.net_mode = Some("none".to_string());
                }
                _ => {}
            }
        }
    }

    // --- Phase 3: security/limits the init reads off the Container ----------

    // `root.readonly` → remount the rootfs read-only after pivot (fail-closed).
    if spec.root().as_ref().and_then(|r| r.readonly()) == Some(true) {
        c.read_only = true;
    }
    // `process.noNewPrivileges` / `oomScoreAdj` → honoured directly.
    if let Some(nnp) = process.no_new_privileges() {
        c.no_new_privs = Some(nnp);
    }
    if let Some(adj) = process.oom_score_adj() {
        c.oom_score_adj = Some(adj);
    }

    // `process.rlimits` → the engine's `ulimits` ("name=soft:hard"). Only the
    // resources it maps to `setrlimit` are honoured; any other RLIMIT is refused
    // by name rather than silently dropped.
    if let Some(rlimits) = process.rlimits().as_ref() {
        for rl in rlimits {
            let name = rlimit_name(rl.typ()).ok_or_else(|| {
                Error::Invalid(format!(
                    "oci-runtime: config.json `process.rlimits` has {:?}, which this engine does \
                     not setrlimit; remove it or run it under runc",
                    rl.typ()
                ))
            })?;
            c.ulimits.push(format!("{name}={}:{}", rl.soft(), rl.hard()));
        }
    }

    // `linux.sysctl` → "k=v". The init installs only NAMESPACED sysctls (net.*
    // only with an own netns); it refuses the rest loudly at start, so they are
    // forwarded rather than silently filtered. Sorted for a deterministic order.
    if let Some(sysctls) = spec.linux().as_ref().and_then(|l| l.sysctl().as_ref()) {
        let mut kv: Vec<String> = sysctls.iter().map(|(k, v)| format!("{k}={v}")).collect();
        kv.sort();
        c.sysctls = kv;
    }

    // `linux.devices` → the engine binds the EXISTING host node at each path
    // (it does not `mknod` the spec's major:minor), and only character devices.
    // So honour char/unbuffered-char devices as `--device <path>`, and refuse
    // block/fifo by name rather than appear to create a node that never shows up.
    if let Some(devs) = spec.linux().as_ref().and_then(|l| l.devices().as_ref()) {
        use oci_spec::runtime::LinuxDeviceType as DevT;
        for d in devs {
            match d.typ() {
                DevT::C | DevT::U => c.devices.push(d.path().to_string_lossy().into_owned()),
                other => {
                    return Err(Error::Invalid(format!(
                        "oci-runtime: config.json `linux.devices` has a {other:?} device {} — only \
                         character devices (bound from the host by path) are supported; remove it \
                         or run it under runc",
                        d.path().display()
                    )))
                }
            }
        }
    }

    // `linux.seccomp` → a custom profile REPLACES the engine's built-in
    // allowlist, but the compiler accepts only a bounded subset (≤2 actions, no
    // per-arg rules, no KILL_PROCESS/TRACE/NOTIFY). Parse it NOW so an
    // unsupported profile is refused at `create` instead of aborting the
    // container at init (exit 126); on success store the JSON the init installs.
    if let Some(seccomp) = spec.linux().as_ref().and_then(|l| l.seccomp().as_ref()) {
        let json = serde_json::to_string(seccomp)
            .map_err(|e| Error::Invalid(format!("oci-runtime: serialise linux.seccomp: {e}")))?;
        // parse() rejects per-arg rules / unsupported action types; compile()
        // rejects a profile with more than one non-default action. The init runs
        // both, so run both here to refuse at `create`, not abort at init.
        let (profile, _) = runtime::seccomp_profile::parse(&json).map_err(|e| {
            Error::Invalid(format!(
                "oci-runtime: config.json `linux.seccomp` is outside the supported subset ({e}); \
                 simplify it (≤2 actions, no per-arg rules) or run it under runc"
            ))
        })?;
        runtime::seccomp_profile::compile(&profile).map_err(|e| {
            Error::Invalid(format!(
                "oci-runtime: config.json `linux.seccomp` is outside the supported subset ({e}); \
                 simplify it (≤2 actions, no per-arg rules) or run it under runc"
            ))
        })?;
        c.seccomp_profile = Some(json);
    }

    // `process.capabilities`: make the kept set equal the bundle's BOUNDING set
    // (the ceiling) by expressing it as cap_add/cap_drop deltas off the engine's
    // KEPT_CAPS baseline — so a bundle that drops caps to harden is honoured
    // instead of silently left at the baseline (over-privileged). The engine's
    // model is a single kept mask, so it cannot represent distinct effective/
    // permitted/inheritable sets or raise ambient caps: an ambient request is
    // refused by name. (bounding is the ceiling the others are subsets of.)
    if let Some(caps) = process.capabilities().as_ref() {
        if caps.ambient().as_ref().is_some_and(|a| !a.is_empty()) {
            return Err(Error::Invalid(
                "oci-runtime: config.json `process.capabilities.ambient` — ambient capabilities are \
                 not raised by this engine; remove them or run it under runc"
                    .to_string(),
            ));
        }
        if let Some(bounding) = caps.bounding().as_ref() {
            use runtime::capabilities::{cap_name, cap_num, KEPT_CAPS};
            let mut want = std::collections::BTreeSet::new();
            for cap in bounding {
                let name = oci_cap_name(cap);
                let num = cap_num(&name).ok_or_else(|| {
                    Error::Invalid(format!(
                        "oci-runtime: config.json names a capability this engine does not know \
                         ({name:?}); remove it or run it under runc"
                    ))
                })?;
                want.insert(num);
            }
            let kept: std::collections::BTreeSet<u8> = KEPT_CAPS.iter().copied().collect();
            // Drop what the baseline keeps but the bundle does not want...
            for &n in &kept {
                if !want.contains(&n) {
                    if let Some(name) = cap_name(n) {
                        c.cap_drop.push(format!("CAP_{name}"));
                    }
                }
            }
            // ...and add what the bundle wants beyond the baseline.
            for &n in &want {
                if !kept.contains(&n) {
                    if let Some(name) = cap_name(n) {
                        c.cap_add.push(format!("CAP_{name}"));
                    }
                }
            }
        }
    }

    Ok((c, rootfs.to_string_lossy().into_owned()))
}

/// CPU bandwidth as a cores string for `Container.cpus` (the engine writes it to
/// `cpu.max`). OCI gives `quota` (µs of CPU per `period`); cores = quota/period.
/// The OCI default period is 100 000 µs. `None` when there is no positive quota.
fn cpu_cores(quota: Option<i64>, period: Option<u64>) -> Option<String> {
    let quota = quota.filter(|q| *q > 0)? as f64;
    let period = period.unwrap_or(100_000).max(1) as f64;
    let cores = quota / period;
    // Trim to a compact decimal ("0.5", "2", "1.5") the engine's parser accepts.
    Some(format!("{cores}"))
}

/// OCI/Docker CPU `shares` (2..=262144, default 1024) → a cgroup2 `cpu.weight`
/// (1..=10000). This is the kernel's own conversion, the same one runc applies
/// when it translates a v1 `shares` request onto a v2 host.
fn shares_to_weight(shares: u64) -> u64 {
    let s = shares.clamp(2, 262_144);
    (1 + ((s - 2) * 9999) / 262_142).clamp(1, 10_000)
}

/// The `CAP_<NAME>` string for an OCI capability. The enum serde-renames to
/// exactly that form, which `capabilities::cap_num` accepts; an unknown value
/// yields `""` (→ refused upstream as an unknown capability).
fn oci_cap_name(cap: &oci_spec::runtime::Capability) -> String {
    serde_json::to_value(cap)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// OCI `PosixRlimitType` → the engine's `ulimit` name, for the resources it
/// actually `setrlimit`s (the rest return `None`, to be refused by name).
fn rlimit_name(t: oci_spec::runtime::PosixRlimitType) -> Option<&'static str> {
    use oci_spec::runtime::PosixRlimitType::*;
    Some(match t {
        RlimitCpu => "cpu",
        RlimitFsize => "fsize",
        RlimitStack => "stack",
        RlimitCore => "core",
        RlimitNproc => "nproc",
        RlimitNofile => "nofile",
        RlimitMemlock => "memlock",
        RlimitAs => "as",
        _ => return None,
    })
}

/// The caller-added **bind** mounts (`type: bind`, or any mount whose options
/// carry `bind`/`rbind`). The standard pseudo-filesystems (proc/sysfs/tmpfs/
/// devpts/mqueue/cgroup) are mounted by the init itself, so they are skipped —
/// forwarding them as binds would double-mount. A bind with no `source` is
/// skipped (nothing to bind from).
fn oci_bind_mounts(spec: &Spec) -> Vec<Mount> {
    let Some(mounts) = spec.mounts().as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for m in mounts {
        let opts = m.options().clone().unwrap_or_default();
        let is_bind = m.typ().as_deref() == Some("bind")
            || opts.iter().any(|o| o == "bind" || o == "rbind");
        if !is_bind {
            continue;
        }
        let Some(source) = m.source().as_ref() else {
            continue;
        };
        let propagation = if opts.iter().any(|o| o == "rshared" || o == "shared") {
            Some("rshared".to_string())
        } else if opts.iter().any(|o| o == "rslave" || o == "slave") {
            Some("rslave".to_string())
        } else {
            None
        };
        out.push(Mount {
            source: source.to_string_lossy().into_owned(),
            target: m.destination().to_string_lossy().into_owned(),
            readonly: opts.iter().any(|o| o == "ro"),
            propagation,
            optional: false,
        });
    }
    out
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
            .capabilities(no_caps())
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

    // ---- Phase 2: broader config.json coverage ----------------------------

    use oci_spec::runtime::{
        Capability, Linux, LinuxBuilder, LinuxCapabilitiesBuilder, LinuxCpuBuilder,
        LinuxDeviceBuilder, LinuxDeviceType, LinuxNamespaceBuilder, LinuxNamespaceType,
        LinuxPidsBuilder, LinuxResourcesBuilder, LinuxSeccompAction, LinuxSeccompBuilder,
        LinuxSyscallBuilder, MountBuilder, PosixRlimitBuilder, PosixRlimitType, Process,
        UserBuilder,
    };
    use std::collections::{HashMap, HashSet};

    // The oci_spec builders consume `self`, so everything is chained in one
    // expression. These helpers keep the tests to the field under test.

    // oci_spec's programmatic `Process` default fills `capabilities` with a
    // non-empty ambient set — unlike a real serialized config (and a real
    // `delonix image export` bundle), whose `ambient` is `[]`. Give test
    // processes an all-unset capabilities block so the cap path is a no-op
    // except in the tests that exercise it on purpose.
    fn no_caps() -> oci_spec::runtime::LinuxCapabilities {
        let empty: HashSet<Capability> = HashSet::new();
        LinuxCapabilitiesBuilder::default()
            .bounding(empty.clone())
            .effective(empty.clone())
            .inheritable(empty.clone())
            .permitted(empty.clone())
            .ambient(empty)
            .build()
            .expect("caps")
    }

    fn proc_true() -> Process {
        ProcessBuilder::default()
            .args(vec!["/bin/true".to_string()])
            .cwd("/")
            .capabilities(no_caps())
            .build()
            .expect("process")
    }

    fn spec_with_linux(process: Process, linux: Linux) -> Spec {
        let root = RootBuilder::default().path("rootfs").build().expect("root");
        SpecBuilder::default()
            .version("1.3.0")
            .root(root)
            .process(process)
            .linux(linux)
            .build()
            .expect("spec")
    }

    fn spec_no_linux(process: Process) -> Spec {
        let root = RootBuilder::default().path("rootfs").build().expect("root");
        SpecBuilder::default()
            .version("1.3.0")
            .root(root)
            .process(process)
            .build()
            .expect("spec")
    }

    #[test]
    fn honours_non_root_user_and_additional_gids() {
        let user = UserBuilder::default()
            .uid(1000u32)
            .gid(1000u32)
            .additional_gids(vec![10u32, 20u32])
            .build()
            .expect("user");
        let process = ProcessBuilder::default()
            .args(vec!["/bin/true".to_string()])
            .cwd("/")
            .capabilities(no_caps())
            .user(user)
            .build()
            .expect("process");
        let (c, _) = spec_to_container("c1", Path::new("/b"), &spec_no_linux(process)).expect("ok");
        assert_eq!(c.run_uid, Some(1000));
        assert_eq!(c.run_gid, Some(1000));
        assert_eq!(c.group_add, vec![10, 20]);
    }

    #[test]
    fn root_user_leaves_run_ids_unset() {
        let user = UserBuilder::default().uid(0u32).gid(0u32).build().expect("user");
        let process = ProcessBuilder::default()
            .args(vec!["/bin/true".to_string()])
            .cwd("/")
            .capabilities(no_caps())
            .user(user)
            .build()
            .expect("process");
        let (c, _) = spec_to_container("c1", Path::new("/b"), &spec_no_linux(process)).expect("ok");
        assert_eq!(c.run_uid, None);
        assert_eq!(c.run_gid, None);
    }

    #[test]
    fn translates_bind_mounts_and_skips_pseudo_fs() {
        let bind = MountBuilder::default()
            .destination("/data")
            .typ("bind")
            .source("/host/data")
            .options(vec!["rbind".to_string(), "ro".to_string()])
            .build()
            .expect("bind");
        let procfs = MountBuilder::default()
            .destination("/proc")
            .typ("proc")
            .source("proc")
            .build()
            .expect("proc");
        let root = RootBuilder::default().path("rootfs").build().expect("root");
        let spec = SpecBuilder::default()
            .version("1.3.0")
            .root(root)
            .process(proc_true())
            .mounts(vec![bind, procfs])
            .build()
            .expect("spec");
        let (c, _) = spec_to_container("c1", Path::new("/b"), &spec).expect("ok");
        assert_eq!(c.mounts.len(), 1, "only the bind, not /proc");
        assert_eq!(c.mounts[0].source, "/host/data");
        assert_eq!(c.mounts[0].target, "/data");
        assert!(c.mounts[0].readonly);
    }

    #[test]
    fn cpu_cores_and_shares_conversions() {
        assert_eq!(cpu_cores(Some(50_000), Some(100_000)).as_deref(), Some("0.5"));
        assert_eq!(cpu_cores(Some(200_000), Some(100_000)).as_deref(), Some("2"));
        assert_eq!(cpu_cores(Some(150_000), None).as_deref(), Some("1.5"));
        assert_eq!(cpu_cores(Some(-1), Some(100_000)), None);
        assert_eq!(cpu_cores(None, Some(100_000)), None);
        assert_eq!(shares_to_weight(2), 1);
        assert_eq!(shares_to_weight(1024), 39);
        assert_eq!(shares_to_weight(262_144), 10_000);
    }

    #[test]
    fn cpu_quota_and_shares_land_on_container() {
        let cpu = LinuxCpuBuilder::default()
            .quota(50_000i64)
            .period(100_000u64)
            .shares(2048u64)
            .build()
            .expect("cpu");
        let res = LinuxResourcesBuilder::default().cpu(cpu).build().expect("res");
        let linux = LinuxBuilder::default().resources(res).build().expect("linux");
        let (c, _) =
            spec_to_container("c1", Path::new("/b"), &spec_with_linux(proc_true(), linux))
                .expect("ok");
        assert_eq!(c.cpus, "0.5");
        assert_eq!(c.cpu_weight, Some(shares_to_weight(2048).to_string()));
    }

    #[test]
    fn refuses_per_container_pids_limit() {
        let pids = LinuxPidsBuilder::default().limit(100i64).build().expect("pids");
        let res = LinuxResourcesBuilder::default().pids(pids).build().expect("res");
        let linux = LinuxBuilder::default().resources(res).build().expect("linux");
        let err = spec_to_container("c1", Path::new("/b"), &spec_with_linux(proc_true(), linux))
            .expect_err("pids refused");
        assert!(err.to_string().contains("pids"), "{err}");
    }

    #[test]
    fn refuses_cpuset_pinning() {
        let cpu = LinuxCpuBuilder::default().cpus("0-1").build().expect("cpu");
        let res = LinuxResourcesBuilder::default().cpu(cpu).build().expect("res");
        let linux = LinuxBuilder::default().resources(res).build().expect("linux");
        let err = spec_to_container("c1", Path::new("/b"), &spec_with_linux(proc_true(), linux))
            .expect_err("cpuset refused");
        assert!(err.to_string().contains("cpuset"), "{err}");
    }

    #[test]
    fn refuses_network_namespace_with_path() {
        let ns = LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::Network)
            .path("/var/run/netns/x")
            .build()
            .expect("ns");
        let linux = LinuxBuilder::default().namespaces(vec![ns]).build().expect("linux");
        let err = spec_to_container("c1", Path::new("/b"), &spec_with_linux(proc_true(), linux))
            .expect_err("netns path refused");
        assert!(err.to_string().contains("network namespace"), "{err}");
    }

    #[test]
    fn pathless_network_namespace_is_isolated_none() {
        let ns = LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::Network)
            .build()
            .expect("ns");
        let linux = LinuxBuilder::default().namespaces(vec![ns]).build().expect("linux");
        let (c, _) =
            spec_to_container("c1", Path::new("/b"), &spec_with_linux(proc_true(), linux))
                .expect("ok");
        assert_eq!(c.net_mode.as_deref(), Some("none"));
    }

    #[test]
    fn refuses_interactive_terminal() {
        let process = ProcessBuilder::default()
            .args(vec!["/bin/true".to_string()])
            .cwd("/")
            .capabilities(no_caps())
            .terminal(true)
            .build()
            .expect("process");
        let err = spec_to_container("c1", Path::new("/b"), &spec_no_linux(process))
            .expect_err("terminal refused");
        assert!(err.to_string().contains("terminal"), "{err}");
    }

    // ---- Phase 3: security / limits --------------------------------------

    #[test]
    fn honours_readonly_rootfs_nnp_and_oom() {
        let process = ProcessBuilder::default()
            .args(vec!["/bin/true".to_string()])
            .cwd("/")
            .capabilities(no_caps())
            .no_new_privileges(false)
            .oom_score_adj(-500)
            .build()
            .expect("process");
        let root = RootBuilder::default()
            .path("rootfs")
            .readonly(true)
            .build()
            .expect("root");
        let spec = SpecBuilder::default()
            .version("1.3.0")
            .root(root)
            .process(process)
            .build()
            .expect("spec");
        let (c, _) = spec_to_container("c1", Path::new("/b"), &spec).expect("ok");
        assert!(c.read_only);
        assert_eq!(c.no_new_privs, Some(false));
        assert_eq!(c.oom_score_adj, Some(-500));
    }

    #[test]
    fn translates_allowlisted_rlimits() {
        let nofile = PosixRlimitBuilder::default()
            .typ(PosixRlimitType::RlimitNofile)
            .soft(1024u64)
            .hard(4096u64)
            .build()
            .expect("rlimit");
        let process = ProcessBuilder::default()
            .args(vec!["/bin/true".to_string()])
            .cwd("/")
            .capabilities(no_caps())
            .rlimits(vec![nofile])
            .build()
            .expect("process");
        let (c, _) = spec_to_container("c1", Path::new("/b"), &spec_no_linux(process)).expect("ok");
        assert_eq!(c.ulimits, vec!["nofile=1024:4096".to_string()]);
    }

    #[test]
    fn refuses_unmapped_rlimit() {
        let rss = PosixRlimitBuilder::default()
            .typ(PosixRlimitType::RlimitRss)
            .soft(1u64)
            .hard(2u64)
            .build()
            .expect("rlimit");
        let process = ProcessBuilder::default()
            .args(vec!["/bin/true".to_string()])
            .cwd("/")
            .capabilities(no_caps())
            .rlimits(vec![rss])
            .build()
            .expect("process");
        let err = spec_to_container("c1", Path::new("/b"), &spec_no_linux(process))
            .expect_err("rlimit refused");
        assert!(err.to_string().contains("rlimits"), "{err}");
    }

    #[test]
    fn translates_sysctls_sorted() {
        let mut sysctl = HashMap::new();
        sysctl.insert("net.ipv4.ip_forward".to_string(), "1".to_string());
        sysctl.insert("kernel.shmmax".to_string(), "65536".to_string());
        let linux = LinuxBuilder::default().sysctl(sysctl).build().expect("linux");
        let (c, _) =
            spec_to_container("c1", Path::new("/b"), &spec_with_linux(proc_true(), linux))
                .expect("ok");
        assert_eq!(
            c.sysctls,
            vec![
                "kernel.shmmax=65536".to_string(),
                "net.ipv4.ip_forward=1".to_string(),
            ]
        );
    }

    #[test]
    fn refuses_linux_devices() {
        let dev = LinuxDeviceBuilder::default()
            .path("/dev/sda")
            .typ(LinuxDeviceType::B)
            .major(8i64)
            .minor(0i64)
            .build()
            .expect("device");
        let linux = LinuxBuilder::default().devices(vec![dev]).build().expect("linux");
        let err = spec_to_container("c1", Path::new("/b"), &spec_with_linux(proc_true(), linux))
            .expect_err("devices refused");
        assert!(err.to_string().contains("devices"), "{err}");
    }

    fn seccomp_syscall(name: &str, action: LinuxSeccompAction) -> oci_spec::runtime::LinuxSyscall {
        LinuxSyscallBuilder::default()
            .names(vec![name.to_string()])
            .action(action)
            .build()
            .expect("syscall")
    }

    #[test]
    fn honours_simple_seccomp_profile() {
        let seccomp = LinuxSeccompBuilder::default()
            .default_action(LinuxSeccompAction::ScmpActErrno)
            .syscalls(vec![seccomp_syscall("read", LinuxSeccompAction::ScmpActAllow)])
            .build()
            .expect("seccomp");
        let linux = LinuxBuilder::default().seccomp(seccomp).build().expect("linux");
        let (c, _) =
            spec_to_container("c1", Path::new("/b"), &spec_with_linux(proc_true(), linux))
                .expect("ok");
        assert!(c.seccomp_profile.is_some(), "a 2-action profile is installed");
    }

    #[test]
    fn refuses_seccomp_with_more_than_one_non_default_action() {
        let seccomp = LinuxSeccompBuilder::default()
            .default_action(LinuxSeccompAction::ScmpActErrno)
            .syscalls(vec![
                seccomp_syscall("read", LinuxSeccompAction::ScmpActAllow),
                seccomp_syscall("write", LinuxSeccompAction::ScmpActLog),
            ])
            .build()
            .expect("seccomp");
        let linux = LinuxBuilder::default().seccomp(seccomp).build().expect("linux");
        let err = spec_to_container("c1", Path::new("/b"), &spec_with_linux(proc_true(), linux))
            .expect_err("complex seccomp refused");
        assert!(err.to_string().contains("seccomp"), "{err}");
    }

    // ---- Phase 4: capabilities (per bounding set) + char devices ----------

    #[test]
    fn honours_bounding_capabilities_as_delta_off_baseline() {
        // bounding = {CHOWN (0, IN KEPT_CAPS), NET_ADMIN (12, NOT in KEPT_CAPS)};
        // the other sets empty (ambient must be set empty or its builder default
        // is non-empty and would trip the ambient refusal).
        let empty: HashSet<Capability> = HashSet::new();
        let caps = LinuxCapabilitiesBuilder::default()
            .bounding(
                vec![Capability::Chown, Capability::NetAdmin]
                    .into_iter()
                    .collect::<HashSet<_>>(),
            )
            .effective(empty.clone())
            .inheritable(empty.clone())
            .permitted(empty.clone())
            .ambient(empty)
            .build()
            .expect("caps");
        let process = ProcessBuilder::default()
            .args(vec!["/bin/true".to_string()])
            .cwd("/")
            .capabilities(caps)
            .build()
            .expect("process");
        let (c, _) = spec_to_container("c1", Path::new("/b"), &spec_no_linux(process)).expect("ok");
        // NET_ADMIN is beyond the baseline → added; a baseline cap not wanted
        // (DAC_OVERRIDE=1) is dropped; a wanted baseline cap (CHOWN) is neither.
        assert!(c.cap_add.contains(&"CAP_NET_ADMIN".to_string()), "add: {:?}", c.cap_add);
        assert!(c.cap_drop.contains(&"CAP_DAC_OVERRIDE".to_string()), "drop: {:?}", c.cap_drop);
        assert!(!c.cap_drop.contains(&"CAP_CHOWN".to_string()), "CHOWN kept");
        assert!(!c.cap_add.contains(&"CAP_CHOWN".to_string()), "CHOWN already baseline");
    }

    #[test]
    fn refuses_ambient_capabilities() {
        let caps = LinuxCapabilitiesBuilder::default()
            .ambient(
                vec![Capability::NetRaw]
                    .into_iter()
                    .collect::<std::collections::HashSet<_>>(),
            )
            .build()
            .expect("caps");
        let process = ProcessBuilder::default()
            .args(vec!["/bin/true".to_string()])
            .cwd("/")
            .capabilities(caps)
            .build()
            .expect("process");
        let err = spec_to_container("c1", Path::new("/b"), &spec_no_linux(process))
            .expect_err("ambient refused");
        assert!(err.to_string().contains("ambient"), "{err}");
    }

    #[test]
    fn honours_char_device_by_path() {
        let dev = LinuxDeviceBuilder::default()
            .path("/dev/fuse")
            .typ(LinuxDeviceType::C)
            .major(10i64)
            .minor(229i64)
            .build()
            .expect("device");
        let linux = LinuxBuilder::default().devices(vec![dev]).build().expect("linux");
        let (c, _) =
            spec_to_container("c1", Path::new("/b"), &spec_with_linux(proc_true(), linux))
                .expect("ok");
        assert_eq!(c.devices, vec!["/dev/fuse".to_string()]);
    }
}
