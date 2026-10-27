//! `ContainerService`, the lifecycle write-path that brings a container up:
//! `CreateContainer`, `StartContainer` and `WaitContainer`.
//!
//! `Create` is the heavy translator ([`spec_to_run_opts`]): the contract's
//! `ContainerSpec` becomes the engine's [`delonix_compute::RunOpts`], lowered
//! through the SAME `resolve_run`/`build_record` pipeline every other entry
//! point builds a container from (the CLI, compose, the Docker API). Reusing it
//! is on purpose — a second lowering would be the seventh copy of `RunSpec` this
//! repo has had to unify. `Create` resolves the image and prepares the rootfs,
//! persists the record in its born `Created` state, and answers with an
//! [`Operation`] linked to the resource — exactly as [`crate::network_ops`].
//!
//! `Start` is NOT done in this multi-threaded server's address space. The
//! engine's supervisor is a `libc::fork()` that assumes a single-threaded
//! process (a `clone()` in `container_init` whose first allocation would
//! deadlock on a glibc arena lock another worker held at fork time). So `Start`
//! re-execs the `delonix` CLI (`delonix start <id>`) in a fresh, single-threaded
//! process — the same answer the CRI's `start_container` and the Docker API's
//! `__apirun` give to the identical hazard. The record `Create` wrote is what
//! that start reads; its rootfs is the one `Create` prepared.
//!
//! `Wait` polls the record (liveness-reconciled) until it leaves `Running`, with
//! a ceiling so a client cannot hold a worker forever.
//!
//! Fields the node API does not serve on this path yet — ports (need the
//! container's own slirp wired in), custom networks, pods, secret delivery, a
//! continuous health check — are REFUSED (`UNIMPLEMENTED`), never silently
//! dropped: a container that came up missing what it was created with is the
//! degradation this engine refuses elsewhere (see [`crate::network_ops::create_with`]).

use std::path::Path;
use std::time::{Duration, Instant};

use tonic::Status;

use crate::network_ops::status_of;
use crate::operations::{self, Begun};
use crate::proto::v1::{
    Container, ContainerSpec, CreateContainerRequest, Operation, RestartMode,
    StartContainerRequest, WaitContainerRequest, WaitContainerResponse,
};

/// The engine's persisted container record (not the proto message).
type Record = delonix_compute::Container;

/// How long [`wait_in`] polls before it answers `DEADLINE_EXCEEDED` rather than
/// hold a worker on a container that may never exit (a `sleep infinity`). A
/// client that wants to keep waiting sends `WaitContainer` again.
const WAIT_CEILING: Duration = Duration::from_secs(50);
/// How often [`wait_in`] re-reads the record while it polls.
const WAIT_POLL: Duration = Duration::from_millis(200);

fn requested_namespace(namespace: &str) -> &str {
    if namespace.is_empty() {
        "default"
    } else {
        namespace
    }
}

/// A container name is a node-wide identity and a DNS label: letters, digits,
/// `-`, `_`, not starting with `-`, at most 253. The same rule `container run`
/// enforces (`valid_container_name`); an invalid name is refused before the
/// record exists, never rewritten.
fn valid_container_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `k=v` lines, sorted, so a map's iteration order never changes what the
/// record stores (and so [`operations::fingerprint_of`] is stable).
fn sorted_kv(map: &std::collections::HashMap<String, String>) -> Vec<String> {
    let mut v: Vec<String> = map.iter().map(|(k, val)| format!("{k}={val}")).collect();
    v.sort();
    v
}

/// A field in the contract that this path does not serve yet: `UNIMPLEMENTED`,
/// naming the CLI that does, the same shape [`crate::network_ops`] refuses the
/// overlay topology with.
fn not_served_yet(what: &str, cli: &str) -> Status {
    Status::unimplemented(format!(
        "CreateContainer: {what} is in the contract and this engine does not serve it on the \
         node API yet ({cli} does)"
    ))
}

/// The contract's `ContainerSpec` lowered to the engine's [`RunOpts`]. PURE —
/// no store, no host; every refusal is decided here, before anything is
/// resolved or created.
///
/// `labels` come from the request (not the spec), as `CreateContainerRequest`
/// defines identity. Fields that need machinery not wired on this path are
/// refused rather than dropped (see the module doc).
pub fn spec_to_run_opts(
    req: &CreateContainerRequest,
    spec: &ContainerSpec,
) -> Result<delonix_compute::RunOpts, Status> {
    if spec.image.is_empty() {
        return Err(Status::invalid_argument(
            "CreateContainer: spec.image is required",
        ));
    }
    // Not served on this path yet — refused, never silently ignored.
    if !spec.ports.is_empty() {
        return Err(not_served_yet(
            "spec.ports (published ports need the container's own slirp)",
            "`delonix run -p`",
        ));
    }
    if !spec.networks.is_empty() {
        return Err(not_served_yet(
            "spec.networks (a custom network attach)",
            "`delonix run --net`",
        ));
    }
    if !spec.pod.is_empty() {
        return Err(not_served_yet("spec.pod (joining a pod)", "`delonix pod`"));
    }
    if !spec.secrets.is_empty() {
        return Err(not_served_yet(
            "spec.secrets (secret delivery at spawn)",
            "`delonix run --secret`",
        ));
    }
    if spec.health_check.is_some() {
        return Err(not_served_yet(
            "spec.health_check (a continuous health monitor)",
            "`delonix run --health-*`",
        ));
    }

    // ENTRYPOINT override, exactly as `RunOpts::entrypoint` reads it:
    // empty = no override (image default); `[""]` = clear; otherwise the argv.
    let entrypoint = match spec.entrypoint.as_slice() {
        [] => None,
        [one] if one.is_empty() => Some(Vec::new()),
        argv => Some(argv.to_vec()),
    };

    let mut o = delonix_compute::RunOpts {
        image: spec.image.clone(),
        // The default network: the container shares the host's netns (no slirp),
        // the rootless default when no `-p`/`--net` was asked for.
        net: "host".to_string(),
        namespace: Some(requested_namespace(&req.namespace).to_string()),
        entrypoint,
        command: spec.command.clone(),
        env: sorted_kv(&spec.env),
        workdir: (!spec.workdir.is_empty()).then(|| spec.workdir.clone()),
        user: (!spec.user.is_empty()).then(|| spec.user.clone()),
        hostname: (!spec.hostname.is_empty()).then(|| spec.hostname.clone()),
        read_only: spec.read_only_rootfs,
        devices: spec.devices.clone(),
        tmpfs: spec.tmpfs.clone(),
        sysctl: sorted_kv(&spec.sysctls),
        log_cri: spec.log_format == "cri",
        labels: sorted_kv(&req.labels),
        // A detached run: the record outlives the call. Start happens in the
        // re-exec, where the supervisor's fork is safe — these only shape the
        // `RunOpts` the record is built from.
        detach: true,
        ..Default::default()
    };

    // ---- resources (cgroup v2), the same mapping the CRI's `apply_resources`
    // does: raw bytes/millis as the engine's own string form, only what was set.
    if let Some(r) = &spec.resources {
        if r.memory_reservation_bytes > 0 {
            return Err(not_served_yet(
                "spec.resources.memory_reservation_bytes",
                "the cgroup `memory.low` path (not on the node API)",
            ));
        }
        if r.pids_limit > 0 {
            return Err(not_served_yet(
                "spec.resources.pids_limit",
                "`delonix run --pids-limit` (not on the node API)",
            ));
        }
        if r.memory_limit_bytes > 0 {
            o.memory = Some(r.memory_limit_bytes.to_string());
        }
        if r.cpu_limit_millis > 0 {
            o.cpus = Some(format!("{:.3}", r.cpu_limit_millis as f64 / 1000.0));
        }
        if r.cpu_weight > 0 {
            o.cpu_weight = Some(r.cpu_weight.to_string());
        }
        if !r.cpuset.is_empty() {
            o.cpuset = Some(r.cpuset.clone());
        }
    }

    // ---- security: `SecurityContext` → the flags `resolve_run` reads.
    if let Some(s) = &spec.security {
        o.privileged = s.privileged;
        o.cap_add = s.cap_add.clone();
        o.cap_drop = s.cap_drop.clone();
        o.masked_path = s.masked_paths.clone();
        o.readonly_path = s.readonly_paths.clone();
        match s.seccomp_profile.as_str() {
            "" => {}
            "unconfined" => o.security_opt.push("seccomp=unconfined".to_string()),
            path => o.security_opt.push(format!("seccomp={path}")),
        }
        if !s.apparmor_profile.is_empty() {
            o.apparmor = Some(s.apparmor_profile.clone());
        }
        if s.no_new_privileges {
            o.security_opt.push("no-new-privileges".to_string());
        }
    }

    // ---- restart policy: the enum → the string the engine's supervisor reads.
    if let Some(p) = &spec.restart {
        o.restart = match RestartMode::try_from(p.mode) {
            Ok(RestartMode::Unspecified | RestartMode::No) => String::new(),
            Ok(RestartMode::OnFailure) => {
                if p.max_retries > 0 {
                    format!("on-failure:{}", p.max_retries)
                } else {
                    "on-failure".to_string()
                }
            }
            Ok(RestartMode::Always) => "always".to_string(),
            Ok(RestartMode::UnlessStopped) => "unless-stopped".to_string(),
            Err(_) => {
                return Err(Status::invalid_argument(format!(
                    "CreateContainer: unknown restart mode {}",
                    p.mode
                )))
            }
        };
    }

    Ok(o)
}

/// The host adapters [`delonix_compute::run::resolve_run`] needs, wired for the
/// node API: no terminal (no pull announcement, no progress line), and refusals
/// as plain `Error::Invalid` — this is a server, not a CLI.
fn resolve(
    root: &Path,
    images: &delonix_oci::ImageStore,
    o: &delonix_compute::RunOpts,
    id: String,
    name: String,
    namespace: String,
) -> delonix_model::Result<delonix_compute::run::Resolved> {
    use delonix_oci::rootfs_user::UserLookupError;
    let announce_pull = |_r: &str| {};
    // No progress line: run the unpacking straight through.
    let unpacking =
        |prepare: &mut dyn FnMut() -> delonix_model::Result<std::path::PathBuf>| prepare();
    let user_error = |e: UserLookupError| {
        delonix_model::Error::Invalid(match e {
            UserLookupError::EmptyUser => "spec.user: empty user".to_string(),
            UserLookupError::NoSuchUser(u) => {
                format!("spec.user: user '{u}' does not exist in the image (/etc/passwd)")
            }
            UserLookupError::NoSuchGroup(g) => {
                format!("spec.user: group '{g}' does not exist in the image (/etc/group)")
            }
        })
    };
    let apparmor_disabled = |p: &str| {
        delonix_linux::Error::ApparmorUnavailable(format!(
            "spec.security.apparmor_profile '{p}': this host has no AppArmor to apply it"
        ))
    };
    delonix_compute::run::resolve_run(
        o,
        id,
        name,
        namespace,
        false,
        &delonix_oci::run_images::HostImages {
            store: images,
            announce_pull: &announce_pull,
            unpacking: &unpacking,
            user_error: &user_error,
        },
        &delonix_volume::HostVolumes {
            root: root.to_path_buf(),
        },
        &delonix_linux::cdi::HostDevices,
        &delonix_linux::run_host::HostRuntime {
            state_root: root.to_path_buf(),
            apparmor_disabled: &apparmor_disabled,
        },
    )
}

/// Whether a container of this name already exists at the node (the store is
/// flat, so the name alone is the identity).
fn name_taken(root: &Path, name: &str) -> Result<bool, Status> {
    if !root.join("containers").is_dir() {
        return Ok(false);
    }
    let store =
        delonix_state::Store::open(root.join("containers")).map_err(|e| status_of(&e.into()))?;
    match store.load(name) {
        Ok(_) => Ok(true),
        Err(e) if e.is_not_found() => Ok(false),
        Err(e) => Err(status_of(&e.into())),
    }
}

/// [`operations::fingerprint_of`] of a `CreateContainer` request's content:
/// everything that distinguishes WHAT gets created, so the same `request_id`
/// retried with a different spec is refused, not answered with the first one.
fn create_fingerprint(req: &CreateContainerRequest, spec: &ContainerSpec) -> String {
    operations::fingerprint_of(&[
        ("image", &spec.image),
        ("entrypoint", &spec.entrypoint.join("\u{1f}")),
        ("command", &spec.command.join("\u{1f}")),
        ("env", &sorted_kv(&spec.env).join("\u{1f}")),
        ("workdir", &spec.workdir),
        ("user", &spec.user),
        ("labels", &operations::canonical_map(&req.labels)),
        ("annotations", &operations::canonical_map(&req.annotations)),
        ("start", if req.start { "1" } else { "" }),
    ])
}

/// The real create: resolve the image, prepare the rootfs, build the record,
/// persist it `Created`. If `start` was asked, launch it (the re-exec).
fn create_work(
    root: &Path,
    images: &delonix_oci::ImageStore,
    req: &CreateContainerRequest,
    o: &delonix_compute::RunOpts,
) -> delonix_model::Result<String> {
    let namespace = requested_namespace(&req.namespace).to_string();
    let id = delonix_node::generate_id();
    let resolved = resolve(
        root,
        images,
        o,
        id.clone(),
        req.name.clone(),
        namespace.clone(),
    )?;
    let mut c = delonix_compute::run::build_record(o, resolved.record)?;
    // `build_record` carries the labels; annotations are the request's, kept on
    // the record so a reader sees what it was created with.
    for (k, v) in &req.annotations {
        c.annotations.insert(k.clone(), v.clone());
    }
    let store = delonix_state::Store::open(root.join("containers"))?;
    store.save(&c)?;
    if req.start {
        // The same re-exec `StartContainer` uses: the fork-safe launch of the
        // record just written. A failed start fails the operation — but the
        // record stays, in `Created`, for a ret/ `StartContainer`.
        launch(root, &id).map_err(|why| delonix_model::Error::Runtime {
            context: "start",
            message: why,
        })?;
    }
    Ok(id)
}

/// `CreateContainer` under `root`: an [`Operation`], persisted before the work,
/// replayable by `request_id`.
pub fn create_in(
    root: &Path,
    images: &delonix_oci::ImageStore,
    req: &CreateContainerRequest,
) -> Result<Operation, Status> {
    if !valid_container_name(&req.name) {
        return Err(Status::invalid_argument(format!(
            "CreateContainer: invalid name '{}' — letters, digits, '-', '_', not starting with \
             '-', at most 253 characters",
            req.name
        )));
    }
    let spec = req
        .spec
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("CreateContainer: spec is required"))?;
    // What is wrong with the request is refused BEFORE it is acknowledged.
    let o = spec_to_run_opts(req, spec)?;
    let target = format!("Container/{}", req.name);
    let fp = create_fingerprint(req, spec);
    // The same request again gets the first answer — before "already exists",
    // which the first request itself made true.
    if let Some(found) = operations::replay(root, "create", &target, &req.request_id, &fp)? {
        return Ok(operations::message(&found));
    }
    if name_taken(root, &req.name)? {
        return Err(Status::already_exists(format!(
            "container '{}' already exists",
            req.name
        )));
    }
    let rec = match operations::begin(root, "create", &target, &req.request_id, &fp)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let namespace = requested_namespace(&req.namespace).to_string();
    let outcome = create_work(root, images, req, &o)
        .map(|_id| format!("/v1/namespaces/{namespace}/containers/{}", req.name));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

/// Re-exec `delonix start <id>` in a fresh, single-threaded process, with this
/// server's state root. Returns `Err(why)` with the engine's own reason on
/// failure (stderr captured to a file, never a pipe: a detached start inherits
/// and holds stdout/stderr, so a pipe read would wait for the container's whole
/// life — the "run -d | tail hangs" trap the CRI documents).
fn launch(root: &Path, id: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let err_dir = root.join("node-api").join("tmp");
    std::fs::create_dir_all(&err_dir).map_err(|e| format!("preparing the launch: {e}"))?;
    let err_path = err_dir.join(format!(
        "start-{}-{}.err",
        std::process::id(),
        delonix_node::generate_id()
    ));
    let err_file =
        std::fs::File::create(&err_path).map_err(|e| format!("preparing the launch: {e}"))?;
    let status = Command::new(delonix_node::dispatch::cli_bin())
        .env("DELONIX_ROOT", root)
        // Machine-to-machine delegation: the top-level `start` form, past the
        // grouped-commands barrier, as the CRI uses `run`/`stop`/`rm`.
        .env("DELONIX_INTERNAL", "1")
        .args(["start", id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err_file))
        .status();
    let outcome = match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            let why = std::fs::read_to_string(&err_path).unwrap_or_default();
            let why = why.trim();
            Err(if why.is_empty() {
                format!("`delonix start {id}` exited {s}")
            } else {
                why.to_string()
            })
        }
        Err(e) => Err(format!("could not run `delonix start {id}`: {e}")),
    };
    let _ = std::fs::remove_file(&err_path);
    outcome
}

/// Load a container by name under `root`, requiring its recorded namespace to
/// match — the identity rule the whole service reads with.
fn load(root: &Path, name: &str, namespace: &str) -> Result<Record, Status> {
    let wanted = requested_namespace(namespace);
    let missing = || Status::not_found(format!("container '{name}' in namespace '{wanted}'"));
    if wanted == "*" || !root.join("containers").is_dir() {
        return Err(missing());
    }
    let store =
        delonix_state::Store::open(root.join("containers")).map_err(|e| status_of(&e.into()))?;
    let c = store.load(name).map_err(|e| {
        if e.is_not_found() {
            missing()
        } else {
            status_of(&e.into())
        }
    })?;
    if c.namespace != wanted {
        return Err(missing());
    }
    Ok(c)
}

/// `StartContainer`: launch a `Created`/stopped container, then its updated
/// record. The launch is a re-exec (see [`launch`]); the record it reads and
/// updates is the one on disk, which this reads back once the start returned.
pub fn start_in(root: &Path, req: &StartContainerRequest) -> Result<Container, Status> {
    let mut c = load(root, &req.name, &req.namespace)?;
    delonix_linux::reconcile_status(&mut c);
    if matches!(
        c.status,
        delonix_model::records::Status::Running | delonix_model::records::Status::Paused
    ) {
        return Err(Status::failed_precondition(format!(
            "container '{}' is already running",
            req.name
        )));
    }
    launch(root, &c.id).map_err(Status::internal)?;
    // The start wrote the record (Running, with a pid); read it back and
    // reconcile liveness, the same view `GetContainer` gives.
    let mut started = load(root, &req.name, &req.namespace)?;
    delonix_linux::reconcile_status(&mut started);
    Ok(crate::containers::message(&started))
}

/// A terminal status as the contract's `WaitContainerResponse`:
/// `Stopped` = exit 0, `Failed(code)` = that code, `Crashed` = no code captured.
fn terminal_response(status: &delonix_model::records::Status) -> Option<WaitContainerResponse> {
    use delonix_model::records::Status as S;
    match status {
        S::Stopped => Some(WaitContainerResponse {
            exit_code: Some(0),
            reason: "Completed".to_string(),
        }),
        S::Failed(code) => Some(WaitContainerResponse {
            exit_code: Some(*code),
            reason: "Error".to_string(),
        }),
        S::Crashed => Some(WaitContainerResponse {
            exit_code: None,
            reason: "Crashed".to_string(),
        }),
        S::Created | S::Running | S::Paused => None,
    }
}

/// `WaitContainer`: poll the record (liveness-reconciled) until it leaves a live
/// state, up to [`WAIT_CEILING`]. A container still running at the ceiling is
/// `DEADLINE_EXCEEDED`, not a false exit code — the client sends the request
/// again to keep waiting.
pub fn wait_in(root: &Path, req: &WaitContainerRequest) -> Result<WaitContainerResponse, Status> {
    wait_in_bounded(root, req, WAIT_CEILING, WAIT_POLL)
}

/// [`wait_in`] with the ceiling and poll injected (the tests' seam).
fn wait_in_bounded(
    root: &Path,
    req: &WaitContainerRequest,
    ceiling: Duration,
    poll: Duration,
) -> Result<WaitContainerResponse, Status> {
    let deadline = Instant::now() + ceiling;
    loop {
        let mut c = load(root, &req.name, &req.namespace)?;
        delonix_linux::reconcile_status(&mut c);
        if let Some(r) = terminal_response(&c.status) {
            return Ok(r);
        }
        if Instant::now() >= deadline {
            return Err(Status::deadline_exceeded(format!(
                "container '{}' was still {} after {}s; send WaitContainer again to keep waiting",
                req.name,
                c.status,
                ceiling.as_secs()
            )));
        }
        std::thread::sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::v1::{Resources, RestartPolicy, SecurityContext};

    fn spec(image: &str) -> ContainerSpec {
        ContainerSpec {
            image: image.to_string(),
            ..Default::default()
        }
    }

    fn req(name: &str, spec: ContainerSpec) -> CreateContainerRequest {
        CreateContainerRequest {
            namespace: String::new(),
            name: name.to_string(),
            spec: Some(spec),
            ..Default::default()
        }
    }

    #[test]
    fn the_translator_maps_the_core_fields() {
        let mut s = spec("alpine:3.20");
        s.command = vec!["echo".into(), "hi".into()];
        s.env =
            std::collections::HashMap::from([("B".into(), "2".into()), ("A".into(), "1".into())]);
        s.workdir = "/app".into();
        s.user = "1000".into();
        s.read_only_rootfs = true;
        s.resources = Some(Resources {
            memory_limit_bytes: 64 * 1024 * 1024,
            cpu_limit_millis: 500,
            ..Default::default()
        });
        s.security = Some(SecurityContext {
            privileged: false,
            cap_add: vec!["NET_ADMIN".into()],
            no_new_privileges: true,
            seccomp_profile: "unconfined".into(),
            ..Default::default()
        });
        s.restart = Some(RestartPolicy {
            mode: RestartMode::OnFailure as i32,
            max_retries: 3,
        });
        let o = spec_to_run_opts(&req("web", s.clone()), &s).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(o.image, "alpine:3.20");
        assert_eq!(o.command, ["echo", "hi"]);
        // Env sorted, so the record (and the fingerprint) are stable.
        assert_eq!(o.env, ["A=1", "B=2"]);
        assert_eq!(o.workdir.as_deref(), Some("/app"));
        assert_eq!(o.user.as_deref(), Some("1000"));
        assert!(o.read_only);
        assert_eq!(o.memory.as_deref(), Some("67108864"));
        assert_eq!(o.cpus.as_deref(), Some("0.500"));
        assert_eq!(o.cap_add, ["NET_ADMIN"]);
        assert!(o.security_opt.contains(&"seccomp=unconfined".to_string()));
        assert!(o.security_opt.contains(&"no-new-privileges".to_string()));
        assert_eq!(o.restart, "on-failure:3");
        assert_eq!(o.net, "host");
    }

    #[test]
    fn the_entrypoint_three_states_are_distinguished() {
        let mut s = spec("alpine");
        assert_eq!(
            spec_to_run_opts(&req("a", s.clone()), &s)
                .unwrap()
                .entrypoint,
            None
        );
        s.entrypoint = vec!["".into()];
        assert_eq!(
            spec_to_run_opts(&req("a", s.clone()), &s)
                .unwrap()
                .entrypoint,
            Some(Vec::new())
        );
        s.entrypoint = vec!["/bin/sh".into(), "-c".into()];
        assert_eq!(
            spec_to_run_opts(&req("a", s.clone()), &s)
                .unwrap()
                .entrypoint,
            Some(vec!["/bin/sh".to_string(), "-c".to_string()])
        );
    }

    #[test]
    fn fields_not_served_on_this_path_are_refused_not_dropped() {
        use crate::proto::v1::{HealthCheck, NetworkAttachment, PortMapping, SecretRef};
        type Mutate = Box<dyn Fn(&mut ContainerSpec)>;
        let cases: Vec<Mutate> = vec![
            Box::new(|s| s.ports = vec![PortMapping::default()]),
            Box::new(|s| s.networks = vec![NetworkAttachment::default()]),
            Box::new(|s| s.pod = "p1".into()),
            Box::new(|s| s.secrets = vec![SecretRef::default()]),
            Box::new(|s| s.health_check = Some(HealthCheck::default())),
        ];
        for mutate in cases {
            let mut s = spec("alpine");
            mutate(&mut s);
            let err = spec_to_run_opts(&req("a", s.clone()), &s).err().unwrap();
            assert_eq!(err.code(), tonic::Code::Unimplemented, "{}", err.message());
        }
    }

    #[test]
    fn an_image_less_spec_and_a_bad_name_are_refused_before_anything() {
        let err = spec_to_run_opts(
            &req("a", ContainerSpec::default()),
            &ContainerSpec::default(),
        )
        .err()
        .unwrap();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        let dir = tempfile::tempdir().unwrap();
        let images = delonix_oci::ImageStore::open(dir.path()).unwrap();
        for bad in ["-leading", "has.dot", "a b", ""] {
            let err = create_in(dir.path(), &images, &req(bad, spec("alpine"))).unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{bad}");
        }
    }

    #[test]
    fn a_duplicate_name_is_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Seed a record by hand (no launch, no image needed).
        let store = delonix_state::Store::open(root.join("containers")).unwrap();
        let mut c = Record::new(
            "0123456789abcdef".into(),
            "web".into(),
            "alpine".into(),
            vec!["true".into()],
            "64M".into(),
        );
        c.namespace = "default".into();
        store.save(&c).unwrap();
        let images = delonix_oci::ImageStore::open(root).unwrap();
        let err = create_in(root, &images, &req("web", spec("alpine"))).unwrap_err();
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
    }

    #[test]
    fn start_and_wait_on_a_missing_container_are_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let s = start_in(
            root,
            &StartContainerRequest {
                name: "ghost".into(),
                namespace: "default".into(),
            },
        )
        .unwrap_err();
        assert_eq!(s.code(), tonic::Code::NotFound);
        let w = wait_in(
            root,
            &WaitContainerRequest {
                name: "ghost".into(),
                namespace: "default".into(),
            },
        )
        .unwrap_err();
        assert_eq!(w.code(), tonic::Code::NotFound);
    }

    #[test]
    fn wait_returns_the_terminal_result_and_bounds_a_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = delonix_state::Store::open(root.join("containers")).unwrap();
        let mut c = Record::new(
            "0123456789abcdef".into(),
            "done".into(),
            "alpine".into(),
            vec!["true".into()],
            "64M".into(),
        );
        c.namespace = "default".into();
        // Stopped: no live process, so `reconcile_status` leaves it terminal.
        c.status = delonix_model::records::Status::Stopped;
        store.save(&c).unwrap();
        let r = wait_in(
            root,
            &WaitContainerRequest {
                name: "done".into(),
                namespace: "default".into(),
            },
        )
        .unwrap();
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.reason, "Completed");

        // A Failed container carries its observed code.
        let mut f = Record::new(
            "fedcba9876543210".into(),
            "boom".into(),
            "alpine".into(),
            vec!["false".into()],
            "64M".into(),
        );
        f.namespace = "default".into();
        f.status = delonix_model::records::Status::Failed(7);
        store.save(&f).unwrap();
        let r = wait_in(
            root,
            &WaitContainerRequest {
                name: "boom".into(),
                namespace: "default".into(),
            },
        )
        .unwrap();
        assert_eq!(r.exit_code, Some(7));
        assert_eq!(r.reason, "Error");
    }

    #[test]
    fn create_replays_the_same_request_id_and_refuses_a_divergent_one() {
        // Drive the ledger directly: a real create needs an image to resolve,
        // but the idempotency contract is independent of the work succeeding.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let r = req("web", spec("alpine"));
        let spec = r.spec.as_ref().unwrap();
        let fp = create_fingerprint(&r, spec);
        let rec = match operations::begin(root, "create", "Container/web", "k-1", &fp).unwrap() {
            Begun::New(rec) => rec,
            Begun::Replay(_) => panic!("first begin must be new"),
        };
        operations::finish(
            root,
            rec,
            Ok("/v1/namespaces/default/containers/web".into()),
        )
        .unwrap();
        // Same request_id, same content: a replay.
        let again = operations::replay(root, "create", "Container/web", "k-1", &fp).unwrap();
        assert!(again.is_some());
        // Same request_id, different content: refused.
        let mut other = spec.clone();
        other.image = "busybox".into();
        let r2 = req("web", other);
        let fp2 = create_fingerprint(&r2, r2.spec.as_ref().unwrap());
        let err = operations::replay(root, "create", "Container/web", "k-1", &fp2).unwrap_err();
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
    }
}
