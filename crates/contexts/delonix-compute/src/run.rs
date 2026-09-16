//! Building the container record from the run specification.
//!
//! The `container run` use case resolves what needs a store, a file or the host
//! (the free name, the image's config, the `--env-file` contents, the `--user`
//! ids against the rootfs, the volumes, the defaults of this node) into a
//! [`ResolvedRun`], and [`build_record`] turns the specification plus that into a
//! [`Container`]. Pure: no store, no filesystem, no host — every rule that decides
//! what the record says is testable without one.
//!
//! What still happens around it, in the interface, is I/O: the seccomp profile
//! file, the AppArmor profile on the host, the secret store, the log path, and
//! everything after the record exists (network attach, spawn).

use crate::pod::parse_add_host;
use crate::ports::{DeviceResolver, ImageStore, RunHost, StorageProvider};
use crate::{Notice, RunOpts};
use delonix_runtime_core::{Container, Error, KubeCgroupParent, Mount, Result};

/// What the use case resolved before building the record.
#[derive(Debug, Clone, Default)]
pub struct ResolvedRun {
    /// The container id (the re-exec's second pass reuses the first pass's id).
    pub id: String,
    /// The final name: `--name`, or the default derived from the id, already
    /// validated and unique in the namespace.
    pub name: String,
    /// The isolation namespace, `default` when none was given.
    pub namespace: String,
    /// The image's command composed with the user's: its ENTRYPOINT plus the
    /// user's command, or its CMD when the user gave none.
    pub image_command: Vec<String>,
    /// The image's environment, in its order.
    pub image_env: Vec<String>,
    /// The image's working directory (`""` = none).
    pub image_workdir: String,
    /// The contents of each `--env-file`, in the order given.
    pub env_files: Vec<String>,
    /// Environment injected by CDI device edits.
    pub cdi_env: Vec<String>,
    /// The final device list (`--device`, `--gpus`, CDI resolved).
    pub devices: Vec<String>,
    /// The resolved `-v` mounts, CDI mounts included.
    pub mounts: Vec<Mount>,
    /// `--user` resolved against the image: uid and optional gid.
    pub run_user: Option<(u32, Option<u32>)>,
    /// This node's memory ceiling for a container started without `-m`.
    pub default_memory: String,
    /// This node's CPU ceiling for a container started without `--cpus`.
    pub default_cpus: String,
    /// The masked paths applied when none are given and the container is not privileged.
    pub default_masked_paths: Vec<String>,
    /// The read-only paths applied when none are given and the container is not privileged.
    pub default_readonly_paths: Vec<String>,
    /// Whether the engine runs without privileges.
    pub rootless: bool,
}

/// What [`resolve_run`] hands back: the inputs of [`build_record`], and what the
/// spawn needs that the record does not carry.
#[derive(Debug)]
pub struct Resolved {
    pub record: ResolvedRun,
    /// The prepared root filesystem.
    pub rootfs: String,
    /// The final mounts (volumes and device mounts), also inside `record`.
    pub mounts: Vec<Mount>,
    /// Warnings for the operator, in the order they arose.
    pub notices: Vec<Notice>,
}

/// Resolves, through the read ports, everything [`build_record`] needs.
///
/// The caller has already chosen the id (a re-exec's second pass reuses the
/// first's), the name and the namespace. The order is the one `container run`
/// has always had: volumes, devices, image, rootfs, `--env-file`, `--user`.
#[allow(clippy::too_many_arguments)]
pub fn resolve_run<I, S, D, H>(
    o: &RunOpts,
    id: String,
    name: String,
    namespace: String,
    second_pass: bool,
    images: &I,
    storage: &S,
    devices: &D,
    host: &H,
) -> Result<Resolved>
where
    I: ImageStore,
    S: StorageProvider,
    D: DeviceResolver,
    H: RunHost,
{
    let mut mounts = storage.resolve_mounts(&o.volumes, &namespace)?;
    let edits = devices.resolve(o.gpus.as_deref(), &o.devices)?;
    mounts.extend(edits.mounts);
    let image = images.resolve(&o.image)?;
    let config = images.config(&image);
    let rootfs = images.prepare_rootfs(&image, &id, second_pass)?;

    let mut env_files = Vec::with_capacity(o.env_file.len());
    for f in &o.env_file {
        env_files.push(
            host.read_file(f)
                .map_err(|e| Error::Invalid(format!("--env-file {f}: {e}")))?,
        );
    }
    let run_user = match &o.user {
        Some(u) => Some(images.resolve_user(&rootfs, u)?),
        None => None,
    };

    let image_command = compose_command(&config.entrypoint, &config.cmd, &o.command);
    Ok(Resolved {
        record: ResolvedRun {
            id,
            name,
            namespace,
            image_command,
            image_env: config.env,
            image_workdir: config.working_dir,
            env_files,
            cdi_env: edits.env,
            devices: edits.devices,
            mounts: mounts.clone(),
            run_user,
            default_memory: host.default_memory(),
            default_cpus: host.default_cpus(),
            default_masked_paths: host.default_masked_paths(),
            default_readonly_paths: host.default_readonly_paths(),
            rootless: host.rootless(),
        },
        rootfs,
        mounts,
        notices: edits.notices,
    })
}

/// The image's ENTRYPOINT followed by the user's command, or by the image's CMD
/// when the user gave none. PURE.
pub fn compose_command(entrypoint: &[String], cmd: &[String], user: &[String]) -> Vec<String> {
    let mut v = entrypoint.to_vec();
    if user.is_empty() {
        v.extend(cmd.iter().cloned());
    } else {
        v.extend(user.iter().cloned());
    }
    v
}

/// Under the kubelet's hierarchy (ADR 0038) an unspecified limit means NO limit
/// — the pod's cgroup is the ceiling. Everywhere else the node's own ceiling
/// applies, so an unspecified limit stays unspecified here and falls back to
/// [`ResolvedRun::default_memory`]/[`ResolvedRun::default_cpus`]. PURE.
pub fn kube_default_limits(
    memory: Option<String>,
    cpus: Option<String>,
    under_kubelet: bool,
) -> (Option<String>, Option<String>) {
    if !under_kubelet {
        return (memory, cpus);
    }
    (
        memory.or_else(|| Some("max".to_string())),
        cpus.or_else(|| Some(String::new())),
    )
}

/// The container record the specification describes. PURE.
pub fn build_record(o: &RunOpts, r: ResolvedRun) -> Result<Container> {
    let cmd = match o.entrypoint.as_deref() {
        // `--entrypoint ""` clears the image's ENTRYPOINT: the user's command runs as is.
        Some("") => o.command.clone(),
        Some(e) => {
            let mut v = vec![e.to_string()];
            v.extend(o.command.iter().cloned());
            v
        }
        None => r.image_command,
    };
    if cmd.is_empty() {
        return Err(Error::Invalid(
            "no command (the image defines no ENTRYPOINT/CMD)".into(),
        ));
    }

    // The kubelet's hierarchy (ADR 0038): validated at the boundary, so a bad value
    // never reaches the engine; its presence is the ONE thing that lifts the node's
    // ceiling.
    let kube_cgroup = o
        .kube_cgroup_parent
        .as_deref()
        .map(KubeCgroupParent::parse)
        .transpose()
        .map_err(Error::Invalid)?;
    let (memory, cpus) =
        kube_default_limits(o.memory.clone(), o.cpus.clone(), kube_cgroup.is_some());
    // HOUSE RULE: a workload started without `-m` gets the node's derived ceiling,
    // never `"max"` — one leaking container used to consume everything the slice had.
    let eff_memory = memory.unwrap_or(r.default_memory);
    let mut c = Container::new(r.id, r.name, o.image.clone(), cmd, eff_memory);
    c.namespace = r.namespace;

    // Environment: the image's, then each `--env-file` (before `-e`, so an explicit
    // `-e` overrides a value from a file), then `-e`, then what CDI injects.
    c.env = r.image_env;
    for content in &r.env_files {
        for (k, v) in delonix_runtime_core::secret::parse_env_file(content) {
            c.env.push(format!("{k}={v}"));
        }
    }
    c.env.extend(o.env.iter().cloned());
    c.env.extend(r.cdi_env);

    if !r.image_workdir.is_empty() {
        c.workdir = Some(r.image_workdir);
    }
    if let Some(w) = &o.workdir {
        c.workdir = Some(w.clone());
    }
    c.devices = r.devices;
    // PERSISTED, not only used at spawn: `container start` rebuilds the spawn from
    // the record, and a mount that only the creation saw came back missing on the
    // first restart (with writes silently landing in the rootfs). CDI mounts
    // included: `start` never re-resolves a CDI spec.
    c.mounts = r.mounts;
    c.privileged = o.privileged;
    for l in &o.labels {
        if let Some((k, v)) = l.split_once('=') {
            c.labels.insert(k.to_string(), v.to_string());
        }
    }
    // `--hostname` overrides the name in the UTS namespace; empty = use the name.
    c.hostname = o.hostname.clone().filter(|h| !h.trim().is_empty());
    if let Some((uid, gid)) = r.run_user {
        c.run_uid = Some(uid);
        c.run_gid = gid;
    }

    // ---- resources (cgroup v2) ----
    // Always resolved, never left to `Container::new`'s `"1.0"`: that literal is the
    // fallback for DESERIALISING an old record, not a policy for a new container.
    c.cpus = cpus.unwrap_or(r.default_cpus);
    c.cpu_weight = o.cpu_weight.clone();
    c.cpuset = o.cpuset.clone();
    c.cgroup_parent = o.cgroup_parent.clone();
    c.kube_cgroup = kube_cgroup;
    c.io_weight = o.io_weight.clone();
    c.io_max = o.io_max.clone();

    // ---- security ----
    c.read_only = o.read_only;
    c.cap_add = o.cap_add.clone();
    c.cap_drop = o.cap_drop.clone();
    // In ROOTLESS `--no-userns` is refused rather than obeyed: without privileges the
    // user namespace is what GRANTS the capabilities every other namespace needs, so
    // turning it off can only produce `clone failed: EPERM` (measured).
    if o.no_userns && r.rootless {
        return Err(Error::Invalid(
            "--no-userns cannot work without privileges: in rootless mode the user namespace is \
             what grants the privileges the other namespaces need, so disabling it can only fail \
             with EPERM. Drop the flag, or run the engine as root"
                .to_string(),
        ));
    }
    c.userns = (r.rootless || o.userns) && !o.no_userns;
    c.selinux = o.selinux.clone();
    c.host_pid = o.host_pid;
    c.host_ipc = o.host_ipc;
    c.log_cri = o.log_cri;

    // ---- fs & limits ----
    // PERSISTED: `/etc/hosts` is rewritten from scratch on every start, so without
    // this the entries vanished on the first `stop`/`start`. Validated here, before
    // the container exists, instead of being dropped silently at start.
    c.extra_hosts = {
        let mut out = Vec::with_capacity(o.add_host.len());
        for entry in &o.add_host {
            let (name, ip) = parse_add_host(entry).map_err(Error::Invalid)?;
            out.push(format!("{name}:{ip}"));
        }
        out
    };
    // The INTENT, next to the result — see `Container::net_mode`.
    c.net_mode = Some(o.net.clone());
    c.tmpfs = o.tmpfs.clone();
    c.ulimits = o.ulimit.clone();
    c.group_add = {
        let mut out = Vec::new();
        for g in &o.group_add {
            match g.trim().parse::<u32>() {
                Ok(v) => out.push(v),
                Err(_) => {
                    return Err(Error::Invalid(format!(
                        "--group-add '{g}': expected a numeric gid"
                    )))
                }
            }
        }
        out
    };
    c.dns_servers = o.dns.clone();
    c.dns_searches = o.dns_search.clone();
    c.dns_options = o.dns_option.clone();
    c.masked_paths = if o.masked_path.is_empty() && !o.privileged {
        r.default_masked_paths
    } else {
        o.masked_path.clone()
    };
    c.readonly_paths = if o.readonly_path.is_empty() && !o.privileged {
        r.default_readonly_paths
    } else {
        o.readonly_path.clone()
    };
    c.sysctls = o.sysctl.clone();

    c.net_aliases = o.network_alias.clone();
    if o.knows_none {
        c.dns_knows = Some(Vec::new());
    } else if !o.knows.is_empty() {
        c.dns_knows = Some(o.knows.clone());
    }
    c.net_bps = o.net_bps.clone();
    c.net_burst = o.net_burst.clone();
    c.log_driver = o.log_driver.clone();
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> RunOpts {
        RunOpts {
            image: "alpine:3.20".into(),
            net: "host".into(),
            ..Default::default()
        }
    }

    fn resolved() -> ResolvedRun {
        ResolvedRun {
            id: "0123456789abcdef".into(),
            name: "web".into(),
            namespace: "default".into(),
            image_command: vec!["/bin/sh".into()],
            default_memory: "512M".into(),
            default_cpus: "0.85".into(),
            default_masked_paths: vec!["/proc/kcore".into()],
            default_readonly_paths: vec!["/proc/sys".into()],
            ..Default::default()
        }
    }

    #[test]
    fn the_node_ceiling_applies_without_limits_and_not_under_the_kubelet() {
        let c = build_record(&opts(), resolved()).unwrap();
        assert_eq!(c.memory_max, "512M");
        assert_eq!(c.cpus, "0.85");

        let mut o = opts();
        o.kube_cgroup_parent = Some("kubepods-burstable-podabc.slice".into());
        let c = build_record(&o, resolved()).unwrap();
        assert_eq!(c.memory_max, "max");
        assert_eq!(c.cpus, "");
    }

    #[test]
    fn an_invalid_kubelet_parent_is_refused() {
        let mut o = opts();
        o.kube_cgroup_parent = Some("../escape".into());
        assert!(build_record(&o, resolved()).is_err());
    }

    #[test]
    fn env_is_image_then_files_then_flags_then_cdi() {
        let mut o = opts();
        o.env = vec!["B=flag".into()];
        let mut r = resolved();
        r.image_env = vec!["A=image".into()];
        r.env_files = vec!["B=file\n".into()];
        r.cdi_env = vec!["C=cdi".into()];
        let c = build_record(&o, r).unwrap();
        assert_eq!(c.env, ["A=image", "B=file", "B=flag", "C=cdi"]);
    }

    #[test]
    fn the_entrypoint_flag_composes_or_clears() {
        let mut o = opts();
        o.command = vec!["-c".into(), "true".into()];
        o.entrypoint = Some("/bin/ash".into());
        assert_eq!(
            build_record(&o, resolved()).unwrap().command,
            ["/bin/ash", "-c", "true"]
        );
        o.entrypoint = Some(String::new());
        assert_eq!(
            build_record(&o, resolved()).unwrap().command,
            ["-c", "true"]
        );
        o.command.clear();
        assert!(build_record(&o, resolved()).is_err());
    }

    #[test]
    fn the_workdir_flag_wins_over_the_image() {
        let mut r = resolved();
        r.image_workdir = "/app".into();
        let mut o = opts();
        assert_eq!(
            build_record(&o, r.clone()).unwrap().workdir.as_deref(),
            Some("/app")
        );
        o.workdir = Some("/srv".into());
        assert_eq!(
            build_record(&o, r).unwrap().workdir.as_deref(),
            Some("/srv")
        );
    }

    #[test]
    fn no_userns_is_refused_only_in_rootless() {
        let mut o = opts();
        o.no_userns = true;
        let mut r = resolved();
        r.rootless = true;
        assert!(build_record(&o, r.clone()).is_err());
        r.rootless = false;
        assert!(!build_record(&o, r).unwrap().userns);
    }

    #[test]
    fn masked_paths_default_unless_given_or_privileged() {
        let c = build_record(&opts(), resolved()).unwrap();
        assert_eq!(c.masked_paths, ["/proc/kcore"]);
        let mut o = opts();
        o.privileged = true;
        assert!(build_record(&o, resolved())
            .unwrap()
            .masked_paths
            .is_empty());
    }

    #[test]
    fn bad_add_host_and_group_add_are_refused() {
        let mut o = opts();
        o.add_host = vec!["db:not-an-ip".into()];
        assert!(build_record(&o, resolved()).is_err());
        let mut o = opts();
        o.group_add = vec!["wheel".into()];
        assert!(build_record(&o, resolved()).is_err());
    }

    // ---- resolve_run over fake ports: the use case runs without the CLI ----

    use crate::ports::{DeviceEdits, ImageConfig};
    use std::cell::RefCell;

    struct Fake {
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new() -> Self {
            Fake {
                calls: RefCell::new(Vec::new()),
            }
        }
        fn log(&self, s: impl Into<String>) {
            self.calls.borrow_mut().push(s.into());
        }
    }

    impl ImageStore for Fake {
        type Image = String;
        fn resolve(&self, reference: &str) -> Result<String> {
            self.log(format!("resolve {reference}"));
            Ok(reference.to_string())
        }
        fn config(&self, _: &String) -> ImageConfig {
            ImageConfig {
                entrypoint: vec!["/entry".into()],
                cmd: vec!["serve".into()],
                env: vec!["PATH=/bin".into()],
                working_dir: "/app".into(),
            }
        }
        fn prepare_rootfs(&self, _: &String, id: &str, second_pass: bool) -> Result<String> {
            self.log(format!("rootfs {id} second_pass={second_pass}"));
            Ok(format!("/roots/{id}"))
        }
        fn resolve_user(&self, rootfs: &str, spec: &str) -> Result<(u32, Option<u32>)> {
            self.log(format!("user {spec} in {rootfs}"));
            Ok((101, Some(101)))
        }
    }

    impl StorageProvider for Fake {
        fn resolve_mounts(&self, volumes: &[String], namespace: &str) -> Result<Vec<Mount>> {
            self.log(format!("mounts {} in {namespace}", volumes.len()));
            Ok(Vec::new())
        }
    }

    impl DeviceResolver for Fake {
        fn resolve(&self, gpus: Option<&str>, devices: &[String]) -> Result<DeviceEdits> {
            self.log(format!("devices {gpus:?} {}", devices.len()));
            Ok(DeviceEdits {
                devices: devices.to_vec(),
                env: vec!["GPU=1".into()],
                notices: vec![Notice::new("hooks not executed", &[])],
                ..Default::default()
            })
        }
    }

    impl RunHost for Fake {
        fn read_file(&self, path: &str) -> std::io::Result<String> {
            match path {
                "ok.env" => Ok("A=1\n".into()),
                _ => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing")),
            }
        }
        fn default_memory(&self) -> String {
            "256M".into()
        }
        fn default_cpus(&self) -> String {
            "1.0".into()
        }
        fn default_masked_paths(&self) -> Vec<String> {
            Vec::new()
        }
        fn default_readonly_paths(&self) -> Vec<String> {
            Vec::new()
        }
        fn rootless(&self) -> bool {
            true
        }
    }

    fn run(o: &RunOpts, f: &Fake, second_pass: bool) -> Result<Resolved> {
        resolve_run(
            o,
            "id1".into(),
            "web".into(),
            "default".into(),
            second_pass,
            f,
            f,
            f,
            f,
        )
    }

    #[test]
    fn resolve_run_keeps_the_historical_order_and_builds_a_record() {
        let f = Fake::new();
        let mut o = opts();
        o.user = Some("nginx".into());
        o.env_file = vec!["ok.env".into()];
        let r = run(&o, &f, false).unwrap();
        assert_eq!(
            *f.calls.borrow(),
            [
                "mounts 0 in default",
                "devices None 0",
                "resolve alpine:3.20",
                "rootfs id1 second_pass=false",
                "user nginx in /roots/id1",
            ]
        );
        assert_eq!(r.rootfs, "/roots/id1");
        assert_eq!(r.notices.len(), 1);
        let c = build_record(&o, r.record).unwrap();
        assert_eq!(c.command, ["/entry", "serve"]);
        assert_eq!(c.env, ["PATH=/bin", "A=1", "GPU=1"]);
        assert_eq!(c.workdir.as_deref(), Some("/app"));
        assert_eq!((c.run_uid, c.run_gid), (Some(101), Some(101)));
        assert_eq!(c.memory_max, "256M");
        assert!(c.userns);
    }

    #[test]
    fn resolve_run_passes_the_second_pass_and_names_a_missing_env_file() {
        let f = Fake::new();
        let o = opts();
        run(&o, &f, true).unwrap();
        assert!(f
            .calls
            .borrow()
            .contains(&"rootfs id1 second_pass=true".to_string()));

        let mut o = opts();
        o.env_file = vec!["gone.env".into()];
        let err = run(&o, &Fake::new(), false).unwrap_err().to_string();
        assert!(err.contains("--env-file gone.env"), "{err}");
    }
}
