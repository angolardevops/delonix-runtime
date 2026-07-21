//! `delonix container` — container lifecycle (run/ps/stop/rm/exec/logs).

use std::path::PathBuf;

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_image::ImageStore;
use delonix_net::infra;
use delonix_runtime::{self as runtime, RunSpec};
use delonix_runtime_core::{generate_id, Container, Error, Result, Status, Store};
use delonix_volume::VolumeStore;
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::{effective_command, find, open_stores, prepare_rootfs, resolve_or_pull};

/// `spec` for `kind: Container` — mirrors `ContainerCmd::Run` (minus `name`,
/// which comes from `metadata.name`). **`detach` defaults to `true`** (unlike the
/// CLI, where the default is `false`): an `apply`/`stack apply` run in the
/// foreground would block waiting for the process to exit — dangerous for a
/// declarative command. Pass `detach: false` explicitly in the YAML if you want
/// the synchronous behavior of the interactive `run`.
#[derive(Debug, Deserialize)]
struct ContainerSpec {
    pub(crate) image: String,
    #[serde(default = "default_true")]
    pub(crate) detach: bool,
    #[serde(default = "default_net")]
    network: String,
    /// HTTP port to auto-register in the L7 proxy (internal FQDN). See `--expose`.
    #[serde(default)]
    expose: Option<u16>,
    #[serde(default)]
    pub(crate) volumes: Vec<String>,
    #[serde(default)]
    pub(crate) ports: Vec<String>,
    #[serde(default)]
    pub(crate) privileged: bool,
    #[serde(default)]
    pub(crate) env: Vec<String>,
    #[serde(default)]
    pub(crate) command: Vec<String>,
    /// `no` (default) | `on-failure[:max]` | `always` | `unless-stopped` —
    /// a detached supervisor becomes the container's parent and restarts it (see
    /// `run_supervised`). This is what makes a manifest resilient. Canonical
    /// field name is `restartPolicy` (uniform with `kind: Vm`); the legacy
    /// `restart` stays accepted so existing manifests don't break.
    #[serde(
        rename = "restartPolicy",
        alias = "restart",
        default = "default_restart"
    )]
    pub(crate) restart: String,
    // ---- parity with `container run` (all optional, k8s-style camelCase) ----
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    entrypoint: Option<String>,
    #[serde(default)]
    devices: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default, rename = "envFile")]
    env_file: Vec<String>,
    #[serde(default)]
    memory: Option<String>,
    #[serde(default)]
    cpus: Option<String>,
    #[serde(default, rename = "cpuWeight")]
    cpu_weight: Option<String>,
    #[serde(default)]
    cpuset: Option<String>,
    #[serde(default, rename = "ioWeight")]
    io_weight: Option<String>,
    #[serde(default, rename = "readOnly")]
    read_only: bool,
    #[serde(default, rename = "capAdd")]
    cap_add: Vec<String>,
    #[serde(default, rename = "capDrop")]
    cap_drop: Vec<String>,
    #[serde(default, rename = "securityOpt")]
    security_opt: Vec<String>,
    #[serde(default)]
    apparmor: Option<String>,
    #[serde(default)]
    selinux: Option<String>,
    #[serde(default)]
    userns: bool,
    #[serde(default, rename = "hostPid")]
    host_pid: bool,
    #[serde(default, rename = "hostIpc")]
    host_ipc: bool,
    #[serde(default)]
    detect: bool,
    #[serde(default)]
    secret: Vec<String>,
    #[serde(default, rename = "secretFiles")]
    secret_files: bool,
    #[serde(default)]
    tmpfs: Vec<String>,
    #[serde(default)]
    ulimit: Vec<String>,
    #[serde(default)]
    sysctl: Vec<String>,
    #[serde(default)]
    gpus: Option<String>,
    #[serde(default, rename = "networkAlias")]
    network_alias: Vec<String>,
    #[serde(default)]
    knows: Vec<String>,
    #[serde(default, rename = "netBps")]
    net_bps: Option<String>,
    #[serde(default, rename = "netBurst")]
    net_burst: Option<String>,
    #[serde(default, rename = "logDriver")]
    log_driver: Option<String>,
}

/// Names accepted in the `spec` of `kind: Container` (canonical + aliases), for the
/// unknown-fields warning. Kept aligned with `ContainerSpec` by the test
/// `manifest::tests::examples_nao_tem_campos_desconhecidos`.
pub(crate) const CONTAINER_SPEC_FIELDS: &[&str] = &[
    "image",
    "detach",
    "network",
    "volumes",
    "ports",
    "privileged",
    "env",
    "command",
    "restartPolicy",
    "restart",
    "hostname",
    "user",
    "entrypoint",
    "devices",
    "labels",
    "envFile",
    "memory",
    "cpus",
    "cpuWeight",
    "cpuset",
    "ioWeight",
    "readOnly",
    "capAdd",
    "capDrop",
    "securityOpt",
    "apparmor",
    "selinux",
    "userns",
    "hostPid",
    "hostIpc",
    "detect",
    "secret",
    "secretFiles",
    "tmpfs",
    "ulimit",
    "sysctl",
    "gpus",
    "networkAlias",
    "knows",
    "netBps",
    "netBurst",
    "logDriver",
    "expose",
];

fn default_restart() -> String {
    "no".to_string()
}

fn default_true() -> bool {
    true
}
fn default_net() -> String {
    "host".to_string()
}
/// `host`/`none` are the two built-in networks (no user bridge); anything else
/// is the name of a custom `delonix network` to attach to.
fn custom_net_name(net: &str) -> Option<String> {
    (net != "host" && net != "none").then(|| net.to_string())
}

// FIXME(follow-up): variants with a large size disparity (≥880 B). Boxing the
// fat variants is a real optimization but awkward with clap's `#[derive(Subcommand)]`
// — left for a dedicated change; the cost here is a short-lived CLI.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum ContainerCmd {
    /// Dashboard (KPIs + table + problems) of the containers — interactive TUI, or
    /// `--once` for a text snapshot.
    Dash {
        #[arg(long)]
        once: bool,
    },
    /// Initialize a project with a Delonixfile + manifest — files ALREADY FILLED IN (images
    /// included), ready to use without editing anything.
    Init {
        /// Project directory (default: the current one).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Project name (default: the directory name).
        #[arg(long)]
        name: Option<String>,
        /// Image to use. Omit = fill in with the default image.
        #[arg(long)]
        image: Option<String>,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
        /// Generate a complete PROJECT for a stack (e.g. `python`) with best
        /// practices, instead of the generic scaffold. `--template list` shows the available ones.
        #[arg(long, short = 't')]
        template: Option<String>,
        /// After generating, build the image, start it, and wait until it's healthy.
        #[arg(long)]
        up: bool,
    },
    /// Run a container from an image (pulls it if missing).
    Run {
        /// Run in the background and print the ID.
        #[arg(short, long)]
        detach: bool,
        /// Container name (default: `dlx-<id>`).
        #[arg(long)]
        name: Option<String>,
        /// Hostname inside the container (UTS namespace + `/etc/hostname`). Default:
        /// the container name (docker `--hostname`).
        #[arg(long)]
        hostname: Option<String>,
        /// Run the process as this user: `uid[:gid]` or `name[:group]` (docker
        /// `--user`). Names are resolved in the image's `/etc/passwd`/`/etc/group`.
        #[arg(short = 'u', long)]
        user: Option<String>,
        /// Network: `host` (shares the host's, default), `none` (isolated netns with
        /// no connectivity), or the NAME of a network created with `delonix network create`.
        #[arg(long, default_value = "host", add = ArgValueCandidates::new(super::complete::networks))]
        net: String,
        /// Logical ISOLATION namespace (default `default`). Containers in different
        /// namespaces cannot reach each other (even on the same network); only a
        /// `kind: Dependency` crosses the boundary.
        #[arg(long)]
        namespace: Option<String>,
        /// Auto-register this container's HTTP port in the L7 proxy under its internal
        /// FQDN `<name>.<namespace>.delonix.internal` (reachable via the proxy). Needs
        /// `--net <network>`. Removed automatically on `container rm`.
        #[arg(long)]
        expose: Option<u16>,
        /// Volume/bind mount, `name:/target[:ro]` or `/host:/target[:ro]`. Repeatable.
        #[arg(short = 'v', long = "volume")]
        volumes: Vec<String>,
        /// Publish a port, `hostPort:contPort[/tcp|udp]` or just `port`. Repeatable.
        /// With `--net host` (the default) the container moves to its own netns with
        /// userspace NAT (slirp4netns, like rootless podman); with `--net
        /// <network>` it publishes via the ingress (nft DNAT + hostfwd on the single slirp).
        #[arg(short = 'p', long = "publish")]
        publish: Vec<String>,
        /// Privileged container (all caps, seccomp off) — trusted workloads.
        #[arg(long)]
        privileged: bool,
        /// Override the image's ENTRYPOINT (COMMAND becomes the arguments to this
        /// binary; `--entrypoint ""` clears it and runs just the COMMAND).
        #[arg(long)]
        entrypoint: Option<String>,
        /// Remove the container when the process exits (with `-d`, a detached
        /// watcher handles removal when the container dies).
        #[arg(long)]
        rm: bool,
        /// Restart policy (only with `-d`): `no` (default), `on-failure[:max]`,
        /// `always`, `unless-stopped`. A detached supervisor (one per container,
        /// ephemeral — there's no daemon) becomes the container's parent, captures
        /// the real exit code, and restarts it according to the policy.
        #[arg(long, default_value = "no")]
        restart: String,
        /// Attach a host device, `/dev/x[:/dev/y]`. Repeatable. The container's
        /// `/dev` is a tmpfs with a curated list (null/zero/tty/...); this
        /// adds real host nodes to it, like `docker --device`.
        #[arg(long = "device")]
        devices: Vec<String>,
        /// Additional environment variables (`KEY=VAL`), repeatable.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Label (`KEY=VAL`), repeatable — e.g. `io.x-k8s.kind.role=control-plane`
        /// enables the dedicated cgroup2 delegation for Kind nodes (see `setup_node_cgroup_ns`).
        #[arg(long = "label")]
        labels: Vec<String>,
        // ---- resources (cgroup v2) ----
        /// Memory limit (`64M`, `2G`, `max`). Default: `max` (no cap).
        #[arg(short = 'm', long)]
        memory: Option<String>,
        /// CPU quota (number of cores, e.g. `0.5`, `2`). Default: `1.0`.
        #[arg(short = 'c', long)]
        cpus: Option<String>,
        /// Relative CPU weight (`cpu.weight`, 1–10000) under contention.
        #[arg(long = "cpu-weight")]
        cpu_weight: Option<String>,
        /// CPUs the container is pinned to (`cpuset.cpus`, e.g. `0-3`, `0,2`).
        #[arg(long)]
        cpuset: Option<String>,
        /// Relative I/O weight (`io.weight`, 1–10000).
        #[arg(long = "io-weight")]
        io_weight: Option<String>,
        // ---- security ----
        /// Read-only rootfs (writes go to tmpfs/volumes).
        #[arg(long = "read-only")]
        read_only: bool,
        /// Add a capability (e.g. `NET_ADMIN`). Repeatable.
        #[arg(long = "cap-add")]
        cap_add: Vec<String>,
        /// Drop a capability. Repeatable.
        #[arg(long = "cap-drop")]
        cap_drop: Vec<String>,
        /// `seccomp=unconfined` | `apparmor=<profile>` (docker-style). Repeatable.
        #[arg(long = "security-opt")]
        security_opt: Vec<String>,
        /// AppArmor profile to apply (`unconfined`, `delonix-default`, or an
        /// already-loaded name). `delonix-default` is loaded automatically.
        #[arg(long)]
        apparmor: Option<String>,
        /// SELinux context/profile to apply.
        #[arg(long)]
        selinux: Option<String>,
        /// User namespace: enables the subuid mapping (default in rootless).
        #[arg(long)]
        userns: bool,
        /// Disable the automatic activation of the user namespace.
        #[arg(long = "no-userns")]
        no_userns: bool,
        /// Share the host's PID namespace (`--pid host`).
        #[arg(long = "host-pid")]
        host_pid: bool,
        /// Share the host's IPC namespace.
        #[arg(long = "host-ipc")]
        host_ipc: bool,
        /// Detection mode: seccomp in log mode (doesn't block), to discover syscalls.
        #[arg(long)]
        detect: bool,
        // ---- secrets & env ----
        /// Inject a secret from the vault (`name`), as an environment variable.
        /// Repeatable. With `--secret-files`, it goes to `/run/secrets/<name>`.
        #[arg(long)]
        secret: Vec<String>,
        /// The `--secret`s come in as files in `/run/secrets/` (tmpfs), not env.
        #[arg(long = "secret-files")]
        secret_files: bool,
        /// Load variables from a `.env` file (`KEY=VAL` per line). Repeatable.
        #[arg(long = "env-file")]
        env_file: Vec<String>,
        // ---- fs & limits ----
        /// Mount a tmpfs (`/path[:options]`). Repeatable.
        #[arg(long)]
        tmpfs: Vec<String>,
        /// Ulimit (`nofile=1024:2048`). Repeatable.
        #[arg(long)]
        ulimit: Vec<String>,
        /// Container sysctl (`net.core.somaxconn=1024`). Repeatable.
        #[arg(long)]
        sysctl: Vec<String>,
        /// Expose GPUs: `all` | `nvidia` | `dri` (expands to the `/dev` nodes).
        #[arg(long)]
        gpus: Option<String>,
        // ---- network (only with `--net <network>`) ----
        /// Fixed IP on the network (`--net <network>`), e.g. `10.89.0.10`.
        #[arg(long)]
        ip: Option<String>,
        /// The container's DNS alias on the network. Repeatable.
        #[arg(long = "network-alias")]
        network_alias: Vec<String>,
        /// Restrict DNS resolution to these containers (isolation). Repeatable.
        #[arg(long)]
        knows: Vec<String>,
        /// The container resolves NO other container by name.
        #[arg(long = "knows-none")]
        knows_none: bool,
        /// Join a pod's netns (`--net <network>`), sharing IP/ports.
        #[arg(long)]
        pod: Option<String>,
        /// Egress bandwidth cap (`10mbit`, `512kbit`). Only with `--net <network>`.
        #[arg(long = "net-bps")]
        net_bps: Option<String>,
        /// Burst for the bandwidth cap. Only with `--net-bps`.
        #[arg(long = "net-burst")]
        net_burst: Option<String>,
        // ---- logs ----
        /// Log driver (`json`, `cri`, ...).
        #[arg(long = "log-driver")]
        log_driver: Option<String>,
        /// Log file path (overrides the default).
        #[arg(long = "log-file")]
        log_file: Option<String>,
        /// CRI format in the log file (for the kubelet/`crictl logs`).
        #[arg(long = "log-cri")]
        log_cri: bool,
        /// Image (e.g. `alpine:3.19`).
        #[arg(add = ArgValueCandidates::new(super::complete::images))]
        image: String,
        /// Command + arguments (default: the image's ENTRYPOINT/CMD).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List containers.
    #[command(visible_alias = "ls")]
    Ps {
        /// Include stopped/failed ones.
        #[arg(short, long)]
        all: bool,
        /// Print only the IDs (to compose with `stop`/`rm`).
        #[arg(short, long)]
        quiet: bool,
    },
    /// (Re)start stopped/crashed containers, reusing the persistent rootfs
    /// (writes made inside the container survive, like in docker) and the same
    /// network/ports/volumes as the original `run`. Always detached.
    Start {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
    },
    /// Stop one or more containers (SIGTERM, then SIGKILL).
    Stop {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
        /// Seconds until SIGKILL.
        #[arg(short, long, default_value_t = 10)]
        time: u64,
    },
    /// Remove one or more containers.
    Rm {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
        /// Force (kill it if running).
        #[arg(short, long)]
        force: bool,
    },
    /// Suspend a container's processes (cgroup v2 freezer) — the state stays
    /// in memory, unlike `stop`. Resume with `unpause`.
    Pause {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
    },
    /// Resume a container suspended with `pause`.
    Unpause {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
    },
    /// Create an image from a container's CURRENT rootfs state
    /// (whatever was written inside becomes a new layer).
    Commit {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        id: String,
        /// Tag for the new image (e.g. `app:v2`).
        tag: String,
    },
    /// Interactive shell inside a container (shortcut for `exec -t`): with no
    /// command, it tries `bash` and falls back to `sh`, which exists in any image.
    Ssh {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        id: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Run the image's `HEALTHCHECK` inside the container. Exits with 1 if
    /// `unhealthy` — usable in a script/CI.
    Healthcheck {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        id: String,
    },
    /// Processes running inside a container (read from `cgroup.procs`).
    Top {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        id: String,
    },
    /// Files changed relative to the image: `A` = created/changed, `D` = deleted.
    Diff {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        id: String,
    },
    /// Copy files between the host and a container. Exactly one side is
    /// `container:/path` (e.g. `delonix container cp web:/etc/nginx.conf .`).
    Cp { src: String, dst: String },
    /// Execute a command inside a running container.
    Exec {
        /// Interactive (attaches stdin).
        #[arg(short = 'i', long)]
        interactive: bool,
        /// Allocate a pseudo-terminal.
        #[arg(short = 't', long)]
        tty: bool,
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        id: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Show the full spec of one or more containers (Store JSON).
    Inspect {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
    },
    /// Human-readable detail of one or more containers, `kubectl describe`-style
    /// (for humans; use `inspect` for script-consumable JSON).
    Describe {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
    },
    /// **Reconfigure a RUNNING container without stopping it** — ports, volumes,
    /// networks, and bandwidth cap.
    ///
    /// Unlike docker (where changing a port or a volume forces recreating the
    /// container), here the dataplane doesn't belong to the process lifecycle:
    /// ports are DNAT/hostfwd in front of the network and volumes come in through
    /// the kernel's mount API (`open_tree`/`move_mount`) in the mount namespace of
    /// the already-live container. The PID doesn't change and the process is never
    /// interrupted.
    ///
    /// The changes are persisted in the registry, so a later `container start`
    /// reproduces the new configuration, not the original.
    Update {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        id: String,
        /// Publish one more port hot, `hostPort:contPort[/tcp|udp]`. Repeatable.
        #[arg(short = 'p', long = "publish-add", value_name = "SPEC")]
        publish_add: Vec<String>,
        /// Unpublish a port hot, by HOST PORT. Repeatable.
        #[arg(long = "publish-rm", value_name = "HOST_PORT")]
        publish_rm: Vec<String>,
        /// Mount a volume hot, `name:/target[:ro]` or `/host:/target[:ro]`. Repeatable.
        #[arg(short = 'v', long = "volume-add", value_name = "SPEC")]
        volume_add: Vec<String>,
        /// Unmount hot, by the TARGET path inside the container. Repeatable.
        #[arg(long = "volume-rm", value_name = "TARGET")]
        volume_rm: Vec<String>,
        /// Connect the container to an additional network hot (multi-homing). Repeatable.
        #[arg(long = "net-connect", value_name = "REDE")]
        net_connect: Vec<String>,
        /// Disconnect the container from an additional network. Repeatable.
        #[arg(long = "net-disconnect", value_name = "REDE")]
        net_disconnect: Vec<String>,
        /// Bandwidth cap, in bit/s with a suffix (`10mbit`, `512kbit`, `1gbit`).
        #[arg(long = "net-rate", value_name = "RATE")]
        net_rate: Option<String>,
        /// Burst for the bandwidth cap (default: `32kb`). Only with `--net-rate`.
        #[arg(long = "net-burst", value_name = "BURST")]
        net_burst: Option<String>,
        /// Remove the bandwidth cap.
        #[arg(long = "net-rate-clear", conflicts_with = "net_rate")]
        net_rate_clear: bool,
    },
    /// Resource usage (CPU/memory/PIDs) of the running containers — one
    /// sample and exits (no stream). With no IDs, shows all running ones.
    Stats {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
    },
    /// Show the logs (detached containers).
    Logs {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        id: String,
        /// Follow the log continuously (exits when the container stops).
        #[arg(short, long)]
        follow: bool,
    },
    /// Apply the `kind: Container` documents of a manifest (idempotent by
    /// name — an existing container with that name is neither recreated nor
    /// checked for spec drift, see `cmd::manifest`).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

pub fn run(action: ContainerCmd) -> Result<()> {
    if let ContainerCmd::Init {
        dir,
        name,
        image,
        force,
        template,
        up,
    } = action
    {
        return cmd_init(
            super::scaffold::Target::Container,
            dir,
            name,
            image,
            force,
            template,
            up,
        );
    }
    if let ContainerCmd::Dash { once } = action {
        return super::dash::run(super::dash::DashScope::Containers, once);
    }
    let (images, store) = open_stores()?;
    match action {
        // Handled at the top of `run` (returns early).
        ContainerCmd::Init { .. } => unreachable!("tratado acima"),
        ContainerCmd::Dash { .. } => unreachable!("tratado acima"),
        ContainerCmd::Run {
            detach,
            name,
            hostname,
            user,
            net,
            volumes,
            publish,
            privileged,
            entrypoint,
            rm,
            restart,
            devices,
            env,
            labels,
            memory,
            cpus,
            cpu_weight,
            cpuset,
            io_weight,
            read_only,
            cap_add,
            cap_drop,
            security_opt,
            apparmor,
            selinux,
            userns,
            no_userns,
            host_pid,
            host_ipc,
            detect,
            secret,
            secret_files,
            env_file,
            tmpfs,
            ulimit,
            sysctl,
            gpus,
            ip,
            network_alias,
            knows,
            knows_none,
            pod,
            net_bps,
            net_burst,
            log_driver,
            log_file,
            log_cri,
            image,
            command,
            namespace,
            expose,
        } => cmd_run(
            &images,
            &store,
            RunOpts {
                detach,
                name,
                hostname,
                user,
                net,
                namespace,
                expose,
                volumes,
                ports: publish,
                privileged,
                entrypoint,
                rm,
                restart,
                devices,
                env,
                labels,
                image,
                command,
                quiet: false,
                memory,
                cpus,
                cpu_weight,
                cpuset,
                io_weight,
                read_only,
                cap_add,
                cap_drop,
                security_opt,
                apparmor,
                selinux,
                userns,
                no_userns,
                host_pid,
                host_ipc,
                detect,
                secret,
                secret_files,
                env_file,
                tmpfs,
                ulimit,
                sysctl,
                gpus,
                ip,
                network_alias,
                knows,
                knows_none,
                pod,
                net_bps,
                net_burst,
                log_driver,
                log_file,
                log_cri,
            },
        ),
        ContainerCmd::Ps { all, quiet } => cmd_ps(&store, all, quiet),
        ContainerCmd::Start { ids } => for_each_id(&ids, |id| cmd_start(&images, &store, id)),
        ContainerCmd::Stop { ids, time } => for_each_id(&ids, |id| cmd_stop(&store, id, time)),
        ContainerCmd::Rm { ids, force } => {
            for_each_id(&ids, |id| cmd_rm(&images, &store, id, force))
        }
        ContainerCmd::Exec {
            interactive,
            tty,
            id,
            command,
        } => cmd_exec(&store, &id, interactive, tty, &command),
        ContainerCmd::Pause { ids } => for_each_id(&ids, |id| cmd_freeze(&store, id, true)),
        ContainerCmd::Unpause { ids } => for_each_id(&ids, |id| cmd_freeze(&store, id, false)),
        ContainerCmd::Commit { id, tag } => cmd_commit(&images, &store, &id, &tag),
        ContainerCmd::Ssh { id, command } => cmd_ssh(&store, &id, &command),
        ContainerCmd::Healthcheck { id } => cmd_healthcheck(&images, &store, &id),
        ContainerCmd::Top { id } => cmd_top(&store, &id),
        ContainerCmd::Diff { id } => cmd_diff(&images, &store, &id),
        ContainerCmd::Cp { src, dst } => cmd_cp(&images, &store, &src, &dst),
        ContainerCmd::Inspect { ids } => cmd_inspect(&store, &ids),
        ContainerCmd::Describe { ids } => cmd_describe(&store, &ids),
        ContainerCmd::Update {
            id,
            publish_add,
            publish_rm,
            volume_add,
            volume_rm,
            net_connect,
            net_disconnect,
            net_rate,
            net_burst,
            net_rate_clear,
        } => cmd_update(
            &store,
            &id,
            UpdateOpts {
                publish_add,
                publish_rm,
                volume_add,
                volume_rm,
                net_connect,
                net_disconnect,
                net_rate,
                net_burst,
                net_rate_clear,
            },
        ),
        ContainerCmd::Stats { ids } => cmd_stats(&store, &ids),
        ContainerCmd::Logs { id, follow } => cmd_logs(&images, &store, &id, follow),
        ContainerCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
    }
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (images, store) = open_stores()?;
    for doc in manifest::of_kind(docs, "Container") {
        let name = &doc.metadata.name;
        // Warn about typos BEFORE the early-continue: a manifest re-applied
        // against an already-existing resource should also see the warning (otherwise
        // the typo never shows up after the first creation).
        manifest::warn_unknown_fields(doc, CONTAINER_SPEC_FIELDS);
        if store.list()?.iter().any(|c| &c.name == name) {
            println!("container/{name}: already exists, nothing to do");
            continue;
        }
        let spec: ContainerSpec = manifest::spec_of(doc)?;
        cmd_run(
            &images,
            &store,
            RunOpts {
                detach: spec.detach,
                name: Some(name.clone()),
                hostname: spec.hostname,
                user: spec.user,
                net: spec.network,
                namespace: doc.metadata.namespace.clone(),
                expose: spec.expose,
                volumes: spec.volumes,
                ports: spec.ports,
                privileged: spec.privileged,
                entrypoint: spec.entrypoint,
                rm: false,
                restart: spec.restart.clone(),
                devices: spec.devices,
                env: spec.env,
                labels: spec.labels,
                image: spec.image,
                command: spec.command,
                quiet: false,
                memory: spec.memory,
                cpus: spec.cpus,
                cpu_weight: spec.cpu_weight,
                cpuset: spec.cpuset,
                io_weight: spec.io_weight,
                read_only: spec.read_only,
                cap_add: spec.cap_add,
                cap_drop: spec.cap_drop,
                security_opt: spec.security_opt,
                apparmor: spec.apparmor,
                selinux: spec.selinux,
                userns: spec.userns,
                host_pid: spec.host_pid,
                host_ipc: spec.host_ipc,
                detect: spec.detect,
                secret: spec.secret,
                secret_files: spec.secret_files,
                env_file: spec.env_file,
                tmpfs: spec.tmpfs,
                ulimit: spec.ulimit,
                sysctl: spec.sysctl,
                gpus: spec.gpus,
                network_alias: spec.network_alias,
                knows: spec.knows,
                net_bps: spec.net_bps,
                net_burst: spec.net_burst,
                log_driver: spec.log_driver,
                ..Default::default()
            },
        )?;
        println!("container/{name}: created");
    }
    Ok(())
}

/// Expand `--gpus <spec>` into the list of device nodes to expose. `all` = NVIDIA +
/// DRI; `nvidia` = only `/dev/nvidia*`; `dri` = only `/dev/dri/*`. Includes only
/// the nodes that EXIST on the host (a `--gpus all` on a GPU-less machine invents
/// no devices).
fn expand_gpu_devices(spec: &str) -> Vec<String> {
    let want_nvidia = spec == "all" || spec.contains("nvidia");
    let want_dri = spec == "all" || spec.contains("dri");
    let mut out = Vec::new();
    let mut add_glob = |dir: &str, prefix: &str| {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if prefix.is_empty() || name.starts_with(prefix) {
                    out.push(format!("{dir}/{name}"));
                }
            }
        }
    };
    if want_nvidia {
        add_glob("/dev", "nvidia"); // /dev/nvidia0, /dev/nvidiactl, /dev/nvidia-uvm, …
    }
    if want_dri {
        add_glob("/dev/dri", ""); // /dev/dri/card0, /dev/dri/renderD128, …
    }
    out
}

/// Ensure the AppArmor profile `profile` is loaded. `unconfined` does nothing;
/// `delonix-default` is loaded from the embedded profile; any other name is
/// assumed already loaded on the host (we don't invent it).
fn ensure_apparmor(profile: &str) -> Result<()> {
    if profile == "unconfined" {
        return Ok(());
    }
    if profile == "delonix-default" {
        const PROFILE: &str =
            include_str!("../../../delonix-runtime-bin/data/apparmor-delonix-default");
        let path = std::env::temp_dir().join("delonix-default.aa");
        std::fs::write(&path, PROFILE)?;
        let out = std::process::Command::new("apparmor_parser")
            .arg("-r")
            .arg(&path)
            .output()
            .map_err(|_| {
                Error::Invalid(
                    "apparmor_parser unavailable (AppArmor not supported on this host?)".into(),
                )
            })?;
        if !out.status.success() {
            return Err(Error::Invalid(format!(
                "failed to load AppArmor profile: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    }
    Ok(())
}

/// Resolve the `-v` mounts (the CLI never builds `Mount` by hand — it delegates
/// to `VolumeStore`, which already knows how to tell a named volume from a bind
/// mount from `:ro`).
fn resolve_mounts(volumes: &[String]) -> Result<Vec<delonix_runtime_core::Mount>> {
    if volumes.is_empty() {
        return Ok(Vec::new());
    }
    let vstore = VolumeStore::open(super::util::state_root())?;
    volumes
        .iter()
        .map(|spec| vstore.resolve_spec(spec))
        .collect()
}

/// Resolve `--user <uid[:gid]|name[:group]>` into `(uid, Option<gid>)`.
///
/// The user part is a number (used verbatim) or a name looked up in the image's
/// `/etc/passwd` — returning its uid AND its primary gid, which becomes the gid
/// when no `:group` is given (like docker/`RunAsUsername`, where the runtime MUST
/// resolve the user in the image). The optional group part is a number or a name
/// looked up in `/etc/group`. A name that doesn't exist in the image is an error
/// (never invented) — the CRI `RunAsUserName` contract requires it.
fn resolve_run_user(rootfs: &str, spec: &str) -> Result<(u32, Option<u32>)> {
    let (user_part, group_part) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    if user_part.is_empty() {
        return Err(Error::Invalid("--user: utilizador vazio".into()));
    }
    let (uid, primary_gid) = if let Ok(n) = user_part.parse::<u32>() {
        (n, None)
    } else {
        let (uid, gid) = passwd_lookup(rootfs, user_part).ok_or_else(|| {
            Error::Invalid(format!(
                "--user: utilizador '{user_part}' não existe na imagem (/etc/passwd)"
            ))
        })?;
        (uid, Some(gid))
    };
    let gid = match group_part {
        Some(g) if !g.is_empty() => Some(if let Ok(n) = g.parse::<u32>() {
            n
        } else {
            group_lookup(rootfs, g).ok_or_else(|| {
                Error::Invalid(format!(
                    "--user: grupo '{g}' não existe na imagem (/etc/group)"
                ))
            })?
        }),
        _ => primary_gid,
    };
    Ok((uid, gid))
}

/// Look up `name` in `<rootfs>/etc/passwd`, returning `(uid, primary_gid)`.
/// Format: `name:passwd:uid:gid:gecos:home:shell`.
fn passwd_lookup(rootfs: &str, name: &str) -> Option<(u32, u32)> {
    let content = std::fs::read_to_string(format!("{rootfs}/etc/passwd")).ok()?;
    for line in content.lines() {
        let mut f = line.split(':');
        if f.next() == Some(name) {
            let uid = f.nth(1)?.parse().ok()?; // skip passwd field, then uid
            let gid = f.next()?.parse().ok()?;
            return Some((uid, gid));
        }
    }
    None
}

/// Look up `name` in `<rootfs>/etc/group`, returning its gid.
/// Format: `name:passwd:gid:members`.
fn group_lookup(rootfs: &str, name: &str) -> Option<u32> {
    let content = std::fs::read_to_string(format!("{rootfs}/etc/group")).ok()?;
    for line in content.lines() {
        let mut f = line.split(':');
        if f.next() == Some(name) {
            return f.nth(1)?.parse().ok(); // skip passwd field, then gid
        }
    }
    None
}

/// Arguments for `container run` (CLI and manifest), grouped — the list passed
/// the `too_many_arguments` threshold long ago.
///
/// **`Default` + `#[serde(default)]` on everything new**: the new fields (parity
/// with the PaaS `run`) were added all at once; internal callers that only want
/// the essentials (`stack apply`, `cluster create`) use `..Default::default()`
/// and don't have to enumerate them all.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct RunOpts {
    pub(crate) detach: bool,
    pub(crate) name: Option<String>,
    /// Internal hostname (`--hostname`). `None` = use the container name.
    #[serde(default)]
    pub(crate) hostname: Option<String>,
    /// The process user (`--user`, `uid[:gid]`|`name[:group]`). `None` = root.
    #[serde(default)]
    pub(crate) user: Option<String>,
    pub(crate) net: String,
    /// Logical ISOLATION namespace (default `default`). See [[namespace isolation]].
    #[serde(default)]
    pub(crate) namespace: Option<String>,
    /// HTTP port to auto-register in the L7 proxy under the internal FQDN (`--expose`).
    #[serde(default)]
    pub(crate) expose: Option<u16>,
    pub(crate) volumes: Vec<String>,
    pub(crate) ports: Vec<String>,
    pub(crate) privileged: bool,
    pub(crate) entrypoint: Option<String>,
    pub(crate) rm: bool,
    pub(crate) restart: String,
    pub(crate) devices: Vec<String>,
    pub(crate) env: Vec<String>,
    pub(crate) labels: Vec<String>,
    pub(crate) image: String,
    pub(crate) command: Vec<String>,
    /// Don't print the ID at the end of `-d`. For internal callers that compose
    /// their own output (e.g. `cluster create`, which starts N nodes and shows
    /// kind-style progress — the IDs in the middle were noise).
    #[serde(default)]
    pub(crate) quiet: bool,
    // ---- parity with the PaaS `run` (all #[serde(default)]) ----
    #[serde(default)]
    pub(crate) memory: Option<String>,
    #[serde(default)]
    pub(crate) cpus: Option<String>,
    #[serde(default)]
    pub(crate) cpu_weight: Option<String>,
    #[serde(default)]
    pub(crate) cpuset: Option<String>,
    #[serde(default)]
    pub(crate) io_weight: Option<String>,
    #[serde(default)]
    pub(crate) read_only: bool,
    #[serde(default)]
    pub(crate) cap_add: Vec<String>,
    #[serde(default)]
    pub(crate) cap_drop: Vec<String>,
    #[serde(default)]
    pub(crate) security_opt: Vec<String>,
    #[serde(default)]
    pub(crate) apparmor: Option<String>,
    #[serde(default)]
    pub(crate) selinux: Option<String>,
    #[serde(default)]
    pub(crate) userns: bool,
    #[serde(default)]
    pub(crate) no_userns: bool,
    #[serde(default)]
    pub(crate) host_pid: bool,
    #[serde(default)]
    pub(crate) host_ipc: bool,
    #[serde(default)]
    pub(crate) detect: bool,
    #[serde(default)]
    pub(crate) secret: Vec<String>,
    #[serde(default)]
    pub(crate) secret_files: bool,
    #[serde(default)]
    pub(crate) env_file: Vec<String>,
    #[serde(default)]
    pub(crate) tmpfs: Vec<String>,
    #[serde(default)]
    pub(crate) ulimit: Vec<String>,
    #[serde(default)]
    pub(crate) sysctl: Vec<String>,
    #[serde(default)]
    pub(crate) gpus: Option<String>,
    #[serde(default)]
    pub(crate) ip: Option<String>,
    #[serde(default)]
    pub(crate) network_alias: Vec<String>,
    #[serde(default)]
    pub(crate) knows: Vec<String>,
    #[serde(default)]
    pub(crate) knows_none: bool,
    #[serde(default)]
    pub(crate) pod: Option<String>,
    #[serde(default)]
    pub(crate) net_bps: Option<String>,
    #[serde(default)]
    pub(crate) net_burst: Option<String>,
    #[serde(default)]
    pub(crate) log_driver: Option<String>,
    #[serde(default)]
    pub(crate) log_file: Option<String>,
    #[serde(default)]
    pub(crate) log_cri: bool,
}

pub(crate) fn cmd_run(images: &ImageStore, store: &Store, opts: RunOpts) -> Result<()> {
    // Intact copy for the re-exec (the destructuring below consumes opts).
    let opts_copy = opts.clone();
    let RunOpts {
        detach,
        name,
        hostname,
        user,
        net,
        namespace,
        expose,
        volumes,
        ports,
        privileged,
        entrypoint,
        rm,
        restart,
        devices,
        env,
        labels,
        image,
        command,
        quiet,
        memory,
        cpus,
        cpu_weight,
        cpuset,
        io_weight,
        read_only,
        cap_add,
        cap_drop,
        security_opt,
        apparmor,
        selinux,
        userns,
        no_userns,
        host_pid,
        host_ipc,
        detect,
        secret,
        secret_files,
        env_file,
        tmpfs,
        ulimit,
        sysctl,
        gpus,
        ip,
        network_alias,
        knows,
        knows_none,
        pod,
        net_bps,
        net_burst,
        log_driver,
        log_file,
        log_cri,
    } = opts;
    // Isolation namespace (default `default`). It goes into an nft set name (via
    // `dlxns_set`, which HASHES it → safe) and into a control-line token (which
    // `attach_container` sanitizes). Here we only ensure it's non-empty.
    let namespace = namespace
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "default".into());
    if net_burst.is_some() && net_bps.is_none() {
        return Err(Error::Invalid(
            "--net-burst only makes sense together with --net-bps".into(),
        ));
    }
    // Validate the `-p`s BEFORE creating anything (clear error, no leftovers).
    for spec in &ports {
        delonix_net::parse_publish(spec)?;
    }
    if net == "none" && !ports.is_empty() {
        return Err(Error::Invalid(
            "-p/--publish is not compatible with --net none (netns has no connectivity)".into(),
        ));
    }
    // Port taken: fail HERE, with an error that says who holds it and what to do.
    // Without this, the collision only blew up deep down in the slirp and dumped
    // raw JSON (`add_hostfwd: slirp_add_hostfwd failed`) — the user was left not
    // knowing it was a port conflict, nor with whom.
    // On the 2nd re-exec pass the port was already checked (and the container
    // itself isn't in the store yet) — checking here would give a false conflict.
    if std::env::var("DELONIX_REEXEC_ID").is_err() {
        for spec in &ports {
            let (hp, cp, _) = delonix_net::parse_publish(spec)?;
            if let Some(owner) = port_owner(store, &hp)? {
                // Structured like the `cluster apply` recipes: the fact first,
                // then the possible ways out as ready-to-copy commands — whoever
                // hits this error resolves it without going to --help.
                let alt = hp.parse::<u32>().map(|n| n + 10000).unwrap_or(18080);
                return Err(Error::Invalid(super::po::tf(
                    "port {hp} is already published by container '{owner}'\n\
                     \n\
                     fix it with ONE of these:\n\
                     \x20 delonix container stop {owner}    # stops whoever holds port {hp}\n\
                     \x20 delonix container run -p {alt}:{cp} ...    # or publish on another port\n\
                     \x20 delonix container update {owner} --publish-rm {hp}    # or hot-unpublish it",
                    &[
                        ("hp", hp.as_str()),
                        ("owner", owner.as_str()),
                        ("alt", &alt.to_string()),
                        ("cp", cp.as_str()),
                    ],
                )));
            }
        }
    }
    let mounts = resolve_mounts(&volumes)?;
    let img = resolve_or_pull(images, &image)?;
    // On the 2nd re-exec pass (see `reexec_into_netns`) the id MUST be the same:
    // the named netns was already created with it on the holder's side.
    let id = std::env::var("DELONIX_REEXEC_ID").unwrap_or_else(|_| generate_id());
    let reexec = std::env::var("DELONIX_REEXEC_ID").is_ok();
    let rootless = runtime::is_rootless();
    let rootfs = prepare_rootfs(images, &img, &id)?;

    // `--entrypoint X` replaces the image's ENTRYPOINT (COMMAND becomes its
    // arguments, without inheriting the image's CMD — docker semantics);
    // `--entrypoint ""` clears it and runs just the user's COMMAND.
    let cmd = match entrypoint.as_deref() {
        Some("") => command.clone(),
        Some(e) => {
            let mut v = vec![e.to_string()];
            v.extend(command.iter().cloned());
            v
        }
        None => effective_command(&img, &command),
    };
    if cmd.is_empty() {
        return Err(Error::Invalid(
            "no command (the image defines no ENTRYPOINT/CMD)".into(),
        ));
    }
    // Default name in the Angolan pattern (king + place, like the kind-mode
    // clusters and the VMs) — derived from the `id` so the TWO re-exec passes arrive
    // at the same name (the id travels in DELONIX_REEXEC_ID; see `names::derived_name`).
    // `dlx-<id>` is only a last resort if the 50 attempts all collide.
    let cname = match name {
        Some(n) => n,
        None => {
            let existing: Vec<String> = store.list()?.iter().map(|c| c.name.clone()).collect();
            super::names::derived_name(&id, |n| existing.iter().any(|e| e == n))
                .unwrap_or_else(|| format!("dlx-{}", &id[..8.min(id.len())]))
        }
    };
    // UNIQUE name, like docker ("name is already in use"). Without this, several
    // containers with the same name got created: `find` resolves to the first, and
    // an `rm <name>` only caught that one — the rest were left orphaned and invisible
    // to management by name (seen the hard way: 2x `loja-app` + 2x `loja-db`).
    if let Some(dup) = store.list()?.iter().find(|c| c.name == cname) {
        return Err(Error::Invalid(super::po::tf(
            "the name '{name}' is already in use by container {id} — pick another or remove it first",
            &[("name", cname.as_str()), ("id", dup.short_id())],
        )));
    }
    // `max` = no memory cap (cgroup v2); in k8s the pod's cgroup already limits.
    let eff_memory = memory.unwrap_or_else(|| "max".to_string());
    let mut c = Container::new(id.clone(), cname, image.clone(), cmd, eff_memory);
    c.namespace = namespace.clone();
    c.env = img.config.env.clone();
    // `--env-file`: each `.env` file (KEY=VAL per line) BEFORE `-e`, so an
    // explicit `-e` can override a value from the file.
    for f in &env_file {
        let content = std::fs::read_to_string(f)
            .map_err(|e| Error::Invalid(format!("--env-file {f}: {e}")))?;
        for (k, v) in delonix_runtime_core::secret::parse_env_file(&content) {
            c.env.push(format!("{k}={v}"));
        }
    }
    c.env.extend(env);
    if !img.config.working_dir.is_empty() {
        c.workdir = Some(img.config.working_dir.clone());
    }
    c.devices = devices;
    // `--gpus`: expands `all`/`nvidia`/`dri` into the `/dev` nodes present on the host.
    if let Some(g) = &gpus {
        c.devices.extend(expand_gpu_devices(g));
    }
    c.privileged = privileged;
    for l in &labels {
        if let Some((k, v)) = l.split_once('=') {
            c.labels.insert(k.to_string(), v.to_string());
        }
    }
    // `--hostname`: overrides the container name in the UTS namespace (the engine reads
    // `c.hostname`). Empty = use the name (historical).
    c.hostname = hostname.filter(|h| !h.trim().is_empty());
    // `--user <uid[:gid]|name[:group]>`: resolves against the image's
    // `/etc/passwd`/`/etc/group` (names) or uses the numbers; the engine switches to
    // the uid/gid before `execve` (`RunSpec.run_uid`/`run_gid`). It's the thread of the
    // CRI `RunAsUser`/`RunAsGroup`/`RunAsUserName`.
    if let Some(u) = &user {
        let (uid, gid) = resolve_run_user(&rootfs, u)?;
        c.run_uid = Some(uid);
        c.run_gid = gid;
    }

    // ---- resources (cgroup v2) ----
    if let Some(cp) = cpus {
        c.cpus = cp;
    }
    c.cpu_weight = cpu_weight;
    c.cpuset = cpuset;
    c.io_weight = io_weight;

    // ---- security ----
    c.read_only = read_only;
    c.cap_add = cap_add;
    c.cap_drop = cap_drop;
    // userns: on by default in rootless; `--no-userns` disables it; `--userns`
    // forces it (useful if it ever stops being the default in rootless).
    c.userns = (rootless || userns) && !no_userns;
    // `--security-opt seccomp=unconfined` / `apparmor=<profile>` (docker-style).
    let mut apparmor_profile = apparmor;
    for opt in &security_opt {
        match opt.split_once('=') {
            // Only `unconfined` (off) and `detect` (log mode) are supported; a
            // custom PROFILE (`seccomp=/x.json`) used to be ACCEPTED and then IGNORED —
            // the container ran with the built-in profile while the user
            // thought theirs was active. Fail-closed: explicit error (a finding from
            // the Docker/Podman analysis; invariant "no silent failure").
            Some(("seccomp", "unconfined")) => c.seccomp = Some("unconfined".into()),
            Some(("seccomp", v)) => {
                return Err(Error::Invalid(format!(
                    "unsupported seccomp profile '{v}' — only `seccomp=unconfined` is supported (the built-in profile applies otherwise); custom profiles are not implemented yet"
                )))
            }
            Some(("apparmor", v)) => apparmor_profile = Some(v.to_string()),
            _ => {
                return Err(Error::Invalid(format!(
                    "invalid --security-opt: '{opt}' (seccomp=… | apparmor=…)"
                )))
            }
        }
    }
    // `--detect`: seccomp in log mode (doesn't block) — to discover syscalls.
    // Doesn't override an explicit `seccomp=` from `--security-opt`.
    if detect && c.seccomp.is_none() {
        c.seccomp = Some("detect".to_string());
    }
    if let Some(p) = &apparmor_profile {
        ensure_apparmor(p)?;
        if p != "unconfined" {
            c.apparmor = Some(p.clone());
        }
    }

    // ---- secrets ----
    if !secret.is_empty() {
        let sstore = delonix_runtime_core::SecretStore::open(super::util::state_root())?;
        c.secrets = secret.clone();
        c.secret_files = secret_files;
        // As env (default) or as files in /run/secrets (the engine handles the
        // tmpfs when `secret_files`). The resolution to env is done here.
        if !secret_files {
            c.env.extend(sstore.resolve_env(&secret));
        }
    }

    // ---- fs & limits ----
    c.tmpfs = tmpfs;
    c.ulimits = ulimit;
    c.sysctls = sysctl;

    // ---- network ----
    // `--network-alias` is recorded but the internal DNS (dns_resolve) does NOT yet
    // consult it — it resolves by name only. Warn instead of pretending (a finding from
    // the Docker/Podman analysis; invariant "no silent failure").
    if !network_alias.is_empty() {
        super::output::warn(super::po::t(
            "--network-alias is recorded but the internal DNS does not resolve aliases yet — only the container name resolves",
        ));
    }
    c.net_aliases = network_alias;
    if knows_none {
        c.dns_knows = Some(Vec::new());
    } else if !knows.is_empty() {
        c.dns_knows = Some(knows);
    }
    c.net_bps = net_bps.clone();
    c.net_burst = net_burst.clone();
    // `--ip` and `--pod`: accepted for flag parity, but the runtime's network
    // model (holder + slirp) doesn't honor them YET — `attach_container` derives
    // the IP from the container's id (it doesn't accept a fixed one), and the pod's
    // `join_netns` isn't wired into the `run` path. We reject rather than
    // accept-and-ignore (which would silently give an IP different from the one
    // requested). These are engine work.
    if ip.is_some() {
        return Err(Error::Invalid(
            "--ip ainda não é suportado: o holder atribui o IP do container (não aceita um fixo). Gap de motor conhecido.".into(),
        ));
    }

    // ---- logs ----
    c.log_driver = log_driver;

    // `--log-file` overrides the default path (`<root>/containers/<id>/log`).
    let log_path = if let Some(lf) = &log_file {
        Some(lf.clone())
    } else if detach {
        Some(
            images
                .root()
                .join("containers")
                .join(&id)
                .join("log")
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        None
    };

    // `--net`: host (default, no own netns) | none (isolated netns, no
    // connectivity) | <name> (joins the NAMED netns that the holder creates in
    // `infra::attach_container` — which creates the netns via `ip netns add` on the
    // holder's SIDE, independent of the container's process; so the container has
    // to JOIN it via `RunSpec.join_netns`, not create its own with `new_netns` —
    // that was the wrong approach, tried and corrected here).
    let custom_net = custom_net_name(&net);
    // `--expose` needs an IP on the SDN (custom network) — the proxy reaches the backend
    // via that IP. With `--net host/none` there's no IP → warn instead of silently ignoring.
    if expose.is_some() && custom_net.is_none() {
        eprintln!(
            "{} {}",
            super::po::t("warning:"),
            super::po::t("--expose requires `--net <network>` (the proxy reaches the container via its SDN IP) — ignored")
        );
    }
    let mut attached_ip = None;
    if let Some(n) = &custom_net {
        if reexec {
            // 2nd pass: we're already running INSIDE the holder's userns+netns (the
            // `ip netns exec` of `join_argv` put us there). The netns already exists and is ours.
            attached_ip = std::env::var("DELONIX_REEXEC_IP").ok();
        } else {
            // 1st pass: creates the netns on the holder's side and RE-EXECUTES itself inside it.
            delonix_net::NetworkStore::open(super::util::state_root())?.get(n)?;
            let (netns, ip) = infra::attach_container(&id, n, &namespace)?;
            // `--expose`: auto-register in the L7 proxy HERE, on the HOST side — the
            // proxy spawn is via `nsenter` into the holder, which fails from the
            // already-reexec'd process (inside the container's netns). `c.expose` is
            // persisted later, in the custom_net block of the reexec pass.
            if let Some(port) = expose {
                if let Err(e) = super::ingress_proxy::auto_register(&c.name, &namespace, &ip, port)
                {
                    eprintln!(
                        "aviso: --expose de '{}' não registado no proxy: {e}",
                        c.name
                    );
                }
            }
            return reexec_into_netns(&id, &netns, &ip, &opts_copy, true);
        }
    }
    // `--pod <name>`: joins the pod sandbox's SHARED netns ("pause" model),
    // used by the CRI server (`delonix-cri`). The pod's netns already exists (the CRI
    // created it with `netns attach cri-<pod>`); each container of the pod joins THAT
    // netns (shared IP/ports) instead of creating its own. Same re-exec mechanism as
    // `--net <custom>`, but ENTERING the POD's netns (not one named after this
    // container). It does NOT detach the netns on failure — it belongs to the pod, not to
    // this container (the pod's other containers share it).
    if let Some(pn) = &pod {
        if reexec {
            attached_ip = std::env::var("DELONIX_REEXEC_IP").ok();
        } else {
            let ip = infra::container_ip(pn);
            return reexec_into_netns(&id, pn, &ip, &opts_copy, false);
        }
    }
    c.ports = ports.clone();

    // `-p` with a custom network: publishes via the INGRESS (hostfwd on the single
    // slirp + nft DNAT), BEFORE startup — the rules point at the assigned IP, which
    // is already known; this is also the path that allows hot (un)publish with the
    // container running. Cleanup in stop/rm (`unpublish_ports`).
    if let Some(ip) = &attached_ip {
        for spec in &ports {
            if let Err(e) = publish_with_retry(ip, spec) {
                // Custom-network path: cleanup is in the ingress, there's no own
                // slirp to reap (and the container hasn't even started yet).
                unpublish_ports(&c, None);
                infra::detach_container(&id, ip);
                return Err(e);
            }
        }
    }

    // `-p` without a custom network (`--net host`, the default): the container
    // stops sharing the host's network and gets its own netns with slirp4netns +
    // the requested hostfwds — the behavior of `docker run -p` (NAT network by
    // default), in podman's rootless model. The slirp dies with the netns.
    let slirp_ports = if custom_net.is_none() {
        ports.clone()
    } else {
        Vec::new()
    };
    let slirp_hook = |pid: i32| -> Result<()> { delonix_net::slirp_attach(pid, &slirp_ports) };
    // DNS for /etc/resolv.conf: on a custom network it's the gateway (the ingress's
    // resolver); with `-p` (slirp) it's the slirp's DNS; on `--net host` it's `None`
    // (the runtime copies the host's resolv.conf).
    let dns = match &custom_net {
        Some(n) => infra::resolve_net(n).ok().map(|(_, _, gw)| gw),
        // POD container (`--pod`): it's on delonix0 like any custom-network container
        // → the resolver is the holder's DNS on the infra gateway. Without this the
        // `/etc/resolv.conf` was left unwritten (the re-exec runs in the holder's mount-ns,
        // where the host's `/etc/resolv.conf` doesn't exist) and NOTHING resolved by name in the pod.
        None if pod.is_some() => Some(infra::INFRA_GATEWAY.to_string()),
        None if !slirp_ports.is_empty() => Some(delonix_net::SLIRP_DNS.to_string()),
        None => None,
    };
    let spec = RunSpec {
        detach,
        // On re-exec we're already in the right netns: DON'T create another (nor join
        // anything — the `ip netns exec` handled that).
        new_netns: !reexec && (net == "none" || !slirp_ports.is_empty()),
        join_netns: None,
        userns: c.userns && !reexec,
        // Inherits the holder's user+network namespace instead of creating its own.
        inherit_userns: reexec,
        log_path,
        mounts,
        on_started: if slirp_ports.is_empty() {
            None
        } else {
            Some(&slirp_hook)
        },
        // /etc/hosts: the custom network's IP, or the slirp's when `-p` without a network.
        hosts_ip: attached_ip
            .clone()
            .or_else(|| (!slirp_ports.is_empty()).then(|| delonix_net::SLIRP_IP.to_string())),
        dns,
        host_pid,
        host_ipc,
        apparmor: apparmor_profile.clone(),
        selinux: selinux.clone(),
        log_cri,
        run_uid: c.run_uid,
        run_gid: c.run_gid,
    };
    // BEFORE the supervised branch (which returns): otherwise containers with
    // `--restart` would never emit `create`.
    delonix_runtime_core::events::emit(
        &super::util::state_root(),
        "container",
        "create",
        &c.id,
        &c.name,
        Some(&image),
    );
    // `--restart`: instead of the CLI creating the container and exiting (leaving
    // it orphaned from `init`, with the exit code lost), a detached SUPERVISOR
    // creates it and becomes its parent — see `run_supervised`.
    if detach && policy_supervised(&restart) {
        c.restart_policy = Some(restart.clone());
        return run_supervised(store, &mut c, &rootfs, &spec, &restart, &id);
    }
    runtime::create_with(store, &mut c, &rootfs, &spec)?;
    if let Some(n) = &custom_net {
        c.network = Some(n.clone());
        c.ip = attached_ip;
        // Namespace isolation: a container outside `default` gets the namespace
        // firewall (fw_chain_body emits same-ns accept + cross-ns `ct new` drop).
        // In `default` nothing applies — open SDN, unchanged behavior.
        if c.namespace != "default" {
            if let Some(ip) = c.ip.clone() {
                let mut fw = c.firewall.clone().unwrap_or_default();
                fw.enabled = true;
                fw.namespace = c.namespace.clone();
                match infra::apply_firewall(&c.id, &ip, &fw) {
                    Ok(()) => c.firewall = Some(fw),
                    Err(e) => eprintln!(
                        "aviso: isolamento de namespace '{}' não aplicado: {e}",
                        c.namespace
                    ),
                }
            }
        }
        // `--expose <port>`: persists in the record (to re-register on `start` and
        // de-register on `rm`). The proxy auto-register was ALREADY done in the 1st
        // pass (host), because the nsenter spawn doesn't run from the reexec.
        if let Some(port) = expose {
            c.expose = Some(port);
        }
        let _ = store.save(&c);
        // `--net-bps`: the shaping lives on the veth on the holder's side, which only
        // exists on the custom-network path. Applied now (the field is already
        // persisted; a later `container update --net-rate` would redo it the same way).
        if let Some(bps) = &net_bps {
            let rate = delonix_net::parse_net_rate(bps, net_burst.as_deref())?;
            infra::set_net_rate(&c.id, rate.rate_bit, rate.burst_bytes)?;
        }
    } else if net_bps.is_some() {
        return Err(Error::Invalid(
            "--net-bps only applies with `--net <network>` (shaping is on the ingress veth)".into(),
        ));
    }
    if rm {
        if detach {
            spawn_rm_watcher(images, store, &c.id);
        } else {
            // foreground: `create_with` only returns after waitpid — remove right away.
            let c = find(store, &id)?;
            let pid = c.pid;
            runtime::remove(store, &c, true)?;
            unpublish_ports(&c, pid);
            let _ = images.unmount_rootfs(&c.id);
            return Ok(());
        }
    }
    if detach && !quiet {
        println!("{id}");
        // Death at birth: a successful `-d` with an already-dead init misleads —
        // the user would only find out when running `curl`/`ps` later. 400ms
        // are enough to catch the immediate crashes (bind <1024 on rootless
        // `--net host`, a broken entrypoint) without perceptibly delaying the
        // happy path. A warning with the most likely cause, not an error: the
        // container is registered and the logs have the full story.
        std::thread::sleep(std::time::Duration::from_millis(400));
        if let Ok(cur) = find(store, &id) {
            let dead = match cur.pid {
                // SAFETY: kill(pid, 0) sends no signal — it only tests existence.
                Some(p) => (unsafe { libc::kill(p, 0) } != 0),
                None => true,
            };
            if dead {
                super::output::warn(&super::po::tf(
                    "container '{name}' exited immediately — see `delonix container logs {name}`",
                    &[("name", &cur.name)],
                ));
                if runtime::is_rootless() && custom_net.is_none() && ports.is_empty() {
                    super::output::warn(super::po::t(
                        "rootless with the default `--net host` cannot bind ports below 1024 — if the image binds one (nginx, httpd, ...), publish it (`-p 8080:80`) or use `--net <network>`",
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `--rm` in detached mode: with no daemon, removal is done by a dedicated
/// **watcher** — a detached process (setsid, stdio to /dev/null) that polls the
/// container's state ~1x/s via `reconcile_status` and, once it stops running, does
/// the same cleanup as `rm -f`. It dies afterwards; one watcher per `--rm` container.
fn spawn_rm_watcher(images: &ImageStore, store: &Store, id: &str) {
    // SAFETY: fork of a single-threaded process (CLI); the child only polls and exits.
    if unsafe { libc::fork() } == 0 {
        unsafe {
            libc::setsid();
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            if null >= 0 {
                libc::dup2(null, 0);
                libc::dup2(null, 1);
                libc::dup2(null, 2);
                if null > 2 {
                    libc::close(null);
                }
            }
        }
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let Ok(mut c) = find(store, id) else {
                std::process::exit(0)
            };
            let _ = runtime::reconcile_status(&mut c);
            if !matches!(
                c.status,
                delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused
            ) {
                let pid = c.pid;
                let _ = runtime::remove(store, &c, true);
                unpublish_ports(&c, pid);
                let _ = images.unmount_rootfs(&c.id);
                std::process::exit(0);
            }
        }
    }
}

/// Short ID, like `docker ps`'s (12 chars).
pub(crate) fn short_id(id: &str) -> &str {
    &id[..12.min(id.len())]
}

/// STATUS column in `docker ps` style: "Up 5 minutes", "Exited (0)".
/// `uptime` is the time since init started (`None` if unknown — a stopped
/// container has no process to read it from).
fn fmt_status(status: &Status, uptime: Option<u64>) -> String {
    let up = || match uptime {
        Some(s) => format!("Up {}", output::fmt_duration_secs(s)),
        // Running with no readable uptime: the record is old (no `pid_starttime`)
        // or init's /proc isn't readable. We don't invent a duration.
        None => "Up".to_string(),
    };
    match status {
        Status::Created => "Created".to_string(),
        Status::Running => up(),
        Status::Paused => format!("{} (Paused)", up()),
        // Without a `finished_at` on `Container`, there's no way to say "how long
        // ago" it exited — docker would show "Exited (0) 2 minutes ago". Better to
        // show less than to fabricate a time from `created_unix`.
        Status::Stopped => "Exited (0)".to_string(),
        Status::Failed(code) => format!("Exited ({code})"),
        Status::Crashed => "Dead".to_string(),
    }
}

/// PORTS column in `docker ps` style: `8080->80/tcp`, comma-separated.
///
/// Docker prefixes the host address (`0.0.0.0:8080->80/tcp`). Not here: the
/// effective address depends on the publication path (per-container slirp vs
/// ingress DNAT) and on `DELONIX_PUBLISH_ADDR`, and printing a fixed `0.0.0.0`
/// would be an exposure claim that could be false — in a column used precisely to
/// decide whether something is exposed.
fn fmt_ports(ports: &[String]) -> String {
    ports
        .iter()
        .map(|p| {
            let (spec, proto) = match p.split_once('/') {
                Some((s, pr)) => (s, pr),
                None => (p.as_str(), "tcp"),
            };
            match spec.split_once(':') {
                Some((hp, cp)) => format!("{hp}->{cp}/{proto}"),
                // Only the container's port (published without a fixed host port).
                None => format!("{spec}/{proto}"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn cmd_ps(store: &Store, all: bool, quiet: bool) -> Result<()> {
    let mut cs = store.list()?;
    // Stable, useful order: most recent first, like `docker ps`.
    cs.sort_by_key(|c| std::cmp::Reverse(c.created_unix));
    let mut t = output::Table::new(&[
        "CONTAINER ID",
        "IMAGE",
        "COMMAND",
        "CREATED",
        "STATUS",
        "PORTS",
        "NAMES",
    ]);
    for c in cs.iter_mut() {
        // `update` (flock) and not `save`: the CRI is concurrent and may be
        // reconciling the same container right now — see `Store::update`.
        if runtime::reconcile_status(c) {
            let _ = store.update(&c.id, runtime::reconcile_status);
        }
        let hidden = matches!(c.status, Status::Failed(_) | Status::Crashed);
        if !all && hidden {
            continue;
        }
        if quiet {
            println!("{}", short_id(&c.id));
            continue;
        }
        let uptime = match c.status {
            Status::Running | Status::Paused => {
                c.pid_starttime.and_then(output::uptime_from_starttime)
            }
            _ => None,
        };
        t.row(vec![
            short_id(&c.id).to_string(),
            // `display_ref` strips the `@sha256:…` when there's a tag: a
            // `kindest/node:v1.34.0@sha256:7416a61b…` (84 chars) pushed all the
            // columns off the screen and the digest says nothing to the reader.
            output::truncate(&output::display_ref(&c.image), 30),
            output::truncate(&format!("\"{}\"", c.command.join(" ")), 22),
            output::fmt_age(c.created_unix),
            fmt_status(&c.status, uptime),
            output::truncate(&fmt_ports(&c.ports), 28),
            c.name.clone(),
        ]);
    }
    if !quiet {
        t.print();
    }
    Ok(())
}

/// Apply `f` to each ID, continuing with the rest if one fails (docker
/// semantics: `rm a b c` removes what it can and returns the first error at the end).
fn for_each_id(ids: &[String], mut f: impl FnMut(&str) -> Result<()>) -> Result<()> {
    let mut failed = false;
    for id in ids {
        if let Err(e) = f(id) {
            // Each failure exits HERE with the id's context; returning the error made
            // main print it a second time, without context (duplicated message).
            eprintln!("{id}: {e}");
            failed = true;
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// `--restart` with `-d`: creates the container inside a **detached supervisor**
/// (one per container, ephemeral — there's still no daemon) and enforces the
/// restart policy.
///
/// Why it has to be this way: `waitpid` is only allowed to the PARENT. In a
/// normal `run -d` the CLI creates the container and exits — it's reparented to
/// the host's `init` and the exit code dies there; `reconcile_status` can only
/// say "it died" (`Crashed`/137), never *why*, and `on-failure` would have no way
/// to decide. Here it's the supervisor that calls `create_with`, so it's the
/// parent: it catches the real code (`Failed(n)`) and restarts according to the
/// policy. It's the same role as podman's `conmon`, without a global resident process.
///
/// The parent (the CLI) waits for the first startup through a pipe, to keep the
/// `run -d` semantics: when the command returns, the container ALREADY exists.
fn run_supervised(
    store: &Store,
    c: &mut Container,
    rootfs: &str,
    spec: &RunSpec<'_>,
    policy: &str,
    id: &str,
) -> Result<()> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe() fills 2 fds; used only for the startup handshake.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(Error::Runtime {
            context: "pipe",
            message: "handshake do supervisor".into(),
        });
    }
    let (rd, wr) = (fds[0], fds[1]);

    // SAFETY: fork of a single-threaded process (CLI).
    if unsafe { libc::fork() } == 0 {
        // ---- supervisor ----
        unsafe {
            libc::close(rd);
            libc::setsid(); // survives the terminal/CLI closing
        }
        let mut restarts: u32 = 0;
        let mut first = true;
        loop {
            let started = runtime::create_with(store, c, rootfs, spec);
            if first {
                // signal the parent: 1 = started, 0 = failed (and the parent returns an error)
                let b = [u8::from(started.is_ok())];
                // SAFETY: writes 1 byte to the write-end and closes it.
                unsafe {
                    libc::write(wr, b.as_ptr() as *const libc::c_void, 1);
                    libc::close(wr);
                    // Only NOW release stdio: until here a `create_with` error
                    // still has to reach the user.
                    let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                    if null >= 0 {
                        libc::dup2(null, 0);
                        libc::dup2(null, 1);
                        libc::dup2(null, 2);
                        if null > 2 {
                            libc::close(null);
                        }
                    }
                }
                first = false;
            }
            if started.is_err() {
                std::process::exit(1);
            }
            // We're the container's PARENT: this captures the REAL exit code and records it.
            let status = match runtime::wait_and_record(store, c) {
                Ok(s) => s,
                Err(_) => std::process::exit(1),
            };
            // `die` with the REAL exit code — the supervisor is the only one that
            // knows it (and the container's parent); a normal `run -d` would only see "Crashed".
            delonix_runtime_core::events::emit(
                &super::util::state_root(),
                "container",
                "die",
                &c.id,
                &c.name,
                Some(&format!("exit={}", status.exit_code())),
            );
            if !should_restart(policy, &status, restarts) {
                std::process::exit(0);
            }
            // Desired state trumps the policy: if the record disappeared (`rm -f`)
            // or the user asked for `stop`, don't resurrect — that's docker's semantics.
            match store.load(&c.id) {
                Err(_) => std::process::exit(0),
                Ok(cur) if cur.stopped_by_user => std::process::exit(0),
                Ok(_) => {}
            }
            restarts += 1;
            // The previous incarnation's port frees itself on `stop`; if it's
            // still held, the restart's `publish_with_retry` clears it.
            // Capped exponential backoff (1s→32s), like docker: a container that
            // crash-loops can't burn the node.
            let backoff = std::cmp::min(1u64 << std::cmp::min(restarts, 5), 32);
            std::thread::sleep(std::time::Duration::from_secs(backoff));
        }
    }

    // ---- parent (CLI): waits for the first startup ----
    // SAFETY: closes the write-end and reads the supervisor's handshake byte.
    unsafe { libc::close(wr) };
    let mut b = [0u8; 1];
    // SAFETY: reads 1 byte; 0 = EOF (supervisor died before signaling).
    let n = unsafe { libc::read(rd, b.as_mut_ptr() as *mut libc::c_void, 1) };
    unsafe { libc::close(rd) };
    if n != 1 || b[0] != 1 {
        return Err(Error::Runtime {
            context: "supervisor",
            message: "o container não arrancou (ver o erro acima)".into(),
        });
    }
    println!("{id}");
    Ok(())
}

/// Decide whether a container should be restarted, given the policy, the state
/// it died with, and how many times it's already been restarted. **Pure**
/// function — the restart state machine is tested without cloning any processes.
///
/// Docker semantics: `no` never; `on-failure[:max]` only on exit ≠ 0 (or signal),
/// up to `max` attempts (no `max` = no limit); `always`/`unless-stopped` always.
/// The real distinction between `always` and `unless-stopped` is what happens on
/// **host reboot** (`unless-stopped` doesn't resurrect a container the user
/// stopped) — without a daemon doing a boot-time reconcile, here the two behave
/// the same WHILE ALIVE; documented so as not to promise what isn't there.
fn should_restart(policy: &str, status: &delonix_runtime_core::Status, restarts: u32) -> bool {
    use delonix_runtime_core::Status as S;
    let failed = matches!(status, S::Failed(_) | S::Crashed);
    let (kind, max) = match policy.split_once(':') {
        Some((k, m)) => (k, m.parse::<u32>().ok()),
        None => (policy, None),
    };
    match kind {
        "always" | "unless-stopped" => true,
        "on-failure" => failed && max.map(|m| restarts < m).unwrap_or(true),
        _ => false, // "no" and anything unknown: don't restart
    }
}

/// Does the policy require supervision? (`no` needs no supervisor at all.)
fn policy_supervised(policy: &str) -> bool {
    matches!(
        policy.split(':').next().unwrap_or(""),
        "always" | "unless-stopped" | "on-failure"
    )
}

/// **Closes the known limitation of `--net <network>` in rootless.**
///
/// The problem: `infra::attach_container` creates the NAMED netns on the holder's
/// side (`ip netns add`, inside its `unshare --user --map-auto --net --mount`).
/// The container tried to join via `setns("/run/netns/<x>")` and always failed
/// with "pod netns unavailable" — for TWO reasons, not one:
///   1. `/run/netns/<x>` lives in the **holder's mount namespace**: from outside
///      the path doesn't even exist (the `open` fails before there's any `setns`);
///   2. even if it did, the netns is **owned by the holder's userns** — without
///      privilege in that userns, the `setns` would be refused.
///
/// Neither is solvable from inside `container_init`: you have to ENTER the
/// holder's userns+mountns BEFORE the container exists.
///
/// The solution (the one `delonix-net`'s doc already pointed to, with nobody
/// wiring it up): re-execute the binary itself through `infra::join_argv` —
/// `nsenter -t <holder> -U -m -n --preserve-credentials -- ip netns exec <netns>`
/// — and run the SAME command there. The 2nd pass is born inside the right
/// userns+netns, so it creates no new namespaces (`inherit_userns`).
///
/// The `DELONIX_REEXEC_ID` distinguishes the two passes AND carries the id:
/// without it the 2nd pass would generate a new id and the netns created in the
/// 1st would be orphaned.
fn reexec_into_netns(
    id: &str,
    netns: &str,
    ip: &str,
    opts: &RunOpts,
    detach_on_fail: bool,
) -> Result<()> {
    // Enters the netns `netns` (the container's in `--net <custom>`, where
    // `netns == sanitize(id)`; the shared POD's in `--pod`, where it differs from `id`).
    let prefix = infra::join_argv(netns).ok_or_else(|| Error::Runtime {
        context: "join_argv",
        message: "infra de ingress em baixo — não há holder onde entrar".into(),
    })?;
    let exe = std::env::current_exe().map_err(|e| Error::Runtime {
        context: "current_exe",
        message: e.to_string(),
    })?;
    // The spec goes by FILE, not by `std::env::args()`. Re-executing the original
    // arguments seemed simpler and was WRONG: `cmd_run` is also called as a
    // library (kind mode starts nodes this way), and there the process args are
    // `cluster create ...` — the re-exec ran the WHOLE `cluster create` again
    // inside the netns, recursively. An explicit internal form doesn't depend on
    // who called it.
    let spec_path = super::util::state_root().join(format!(".reexec-{id}.json"));
    let json = serde_json::to_string(opts).map_err(|e| Error::Invalid(e.to_string()))?;
    std::fs::write(&spec_path, json)?;
    let status = std::process::Command::new(&prefix[0])
        .args(&prefix[1..])
        .arg(&exe)
        .args(["netns", "run"])
        .arg(&spec_path)
        .env("DELONIX_REEXEC_ID", id)
        .env("DELONIX_REEXEC_IP", ip)
        .env("DELONIX_ROOT", super::util::state_root())
        .status();
    let _ = std::fs::remove_file(&spec_path);
    let status = status.map_err(|e| Error::Runtime {
        context: "re-exec nsenter",
        message: e.to_string(),
    })?;
    if !status.success() {
        // Only detach if THIS container owns the netns (`--net <custom>`); in a pod the
        // netns belongs to the sandbox and is shared — detaching it would take down the peers.
        if detach_on_fail {
            infra::detach_container(id, ip);
        }
        return Err(Error::Invalid(format!(
            "o container não arrancou dentro da rede '{netns}' (exit {:?})",
            status.code()
        )));
    }
    Ok(())
}

/// The 2nd re-exec pass (`delonix netns run <spec.json>`, hidden — not a public
/// subcommand). Runs ALREADY inside the holder's userns+netns.
pub(crate) fn run_from_spec(path: &std::path::Path) -> Result<()> {
    let json = std::fs::read_to_string(path)?;
    let opts: RunOpts = serde_json::from_str(&json).map_err(|e| Error::Invalid(e.to_string()))?;
    let (images, store) = open_stores()?;
    cmd_run(&images, &store, opts)
}

/// Which LIVE container is publishing this host port? `None` = free.
/// (Only live containers count: dead ones no longer hold it — and if some orphan
/// process holds it, `reap_orphan_net` clears it before this.)
pub(crate) fn port_owner(store: &Store, host_port: &str) -> Result<Option<String>> {
    for c in store.list()? {
        if !matches!(
            c.status,
            delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused
        ) {
            continue;
        }
        for p in &c.ports {
            if let Ok((hp, _, _)) = delonix_net::parse_publish(p) {
                if hp == host_port {
                    return Ok(Some(c.name));
                }
            }
        }
    }
    Ok(None)
}

/// Publish a port; if it fails because the port is held by an **orphan process**
/// (the container died without `stop` and the slirp kept holding it), clears ONLY
/// that one and tries again.
///
/// Why this way and not sweeping everything beforehand: the preventive reaper ran
/// on EVERY `run` with ports and deleted by default — all it took was the
/// container list coming back empty (a read error, or a store view without the
/// records) for `live_ports` to be empty and it to conclude that NOTHING is in
/// use, deleting the hostfwds of LIVE containers. That's what made a `Ready`
/// cluster's apiserver unreachable and made two containers with `-p` never
/// coexist. Here the cleanup is REACTIVE and surgical: it only happens when the
/// port we want fails, and only touches that one. With no conflict, nothing is
/// deleted — and a state-read error can no longer destroy what's working.
fn publish_with_retry(ip: &str, spec: &str) -> Result<()> {
    match infra::publish_port(ip, spec) {
        Ok(()) => Ok(()),
        Err(e) => {
            let (hp, _, _) = delonix_net::parse_publish(spec)?;
            // Orphans first (dead container's slirp still holding the port),
            // then the hostfwd for that specific port.
            let _ = delonix_net::reap_orphan_slirp();
            infra::unpublish_port(&hp);
            infra::publish_port(ip, spec).map_err(|_| e)
        }
    }
}

/// Release the ports published by a container (best-effort, idempotent).
///
/// Two paths, both need cleanup:
///
/// - **custom network**: persistent rules in the ingress (hostfwd on the single
///   slirp + DNAT on the holder) — removed per port.
/// - **per-container slirp**: ITS slirp is killed. This branch used to claim that
///   "the slirp process dies with the container's netns, there's nothing to clean
///   up" and returned right away. That's false: the slirp only exits once it
///   NOTICES the netns is gone, and in that window it keeps holding the host port.
///   Measured thus: `stop` followed by an immediate `start` failed 3 times out of
///   3 with `add_hostfwd: slirp_add_hostfwd failed`, and started working on its
///   own a few seconds later.
///
/// `slirp_pid` has to be the init's pid **from before** stopping it: `runtime::stop`
/// and `runtime::remove` set `container.pid = None`, so reading `c.pid` in here
/// would give `None` for every caller that already stopped the container — the
/// slirp would never be reaped and the bug above would stand. Hence an explicit
/// parameter instead of coming from the record.
fn unpublish_ports(c: &Container, slirp_pid: Option<i32>) {
    match &c.network {
        Some(_) => {
            // 1) ports: release the hostfwd/DNAT in the ingress (idempotent — removing
            //    a port that's no longer there is harmless).
            for spec in &c.ports {
                if let Ok((host_port, _, _)) = delonix_net::parse_publish(spec) {
                    infra::unpublish_port(&host_port);
                }
            }
            // 2) network: release the veth/IP and drop the ingress ref marker.
            //    ALWAYS detach when there's an ip — `infra::release` is now
            //    IDEMPOTENT (a per-id marker set, not a blind counter), so `stop`
            //    then `rm` of the same container no longer double-counts, and a
            //    container that died ABRUPTLY (no `stop`, `reconcile_status` already
            //    nulled the pid) still gets its marker released here. The old guard
            //    `slirp_pid.is_some()` skipped the detach precisely in that abrupt
            //    path → the ref leaked (seen: 16 with 3 containers alive). The
            //    `system prune` reaper (`reap_orphan_refs`) is the backstop for
            //    containers that die and are never `rm`'d at all.
            if let Some(ip) = &c.ip {
                infra::detach_container(&c.id, ip);
            }
        }
        None => {
            // With no published ports there's no slirp with an api-socket holding anything.
            if c.ports.is_empty() {
                return;
            }
            if let Some(pid) = slirp_pid {
                delonix_net::reap_slirp_for(pid);
            }
        }
    }
}

/// `container start` — restarts a stopped/crashed container with the spec stored
/// in the `Store` (command/env/mounts/network/ports) and the PERSISTENT rootfs
/// (rootless: the flat copy in `containers/<id>/rootfs`; root: remounts the
/// overlay, whose `upper` preserves the writes). It's what `rm`+`run` lacks: it
/// doesn't lose the state written inside the container.
fn cmd_start(images: &ImageStore, store: &Store, id: &str) -> Result<()> {
    let mut c = find(store, id)?;
    if runtime::reconcile_status(&mut c) {
        c = store.update(&c.id, runtime::reconcile_status).unwrap_or(c);
    }
    // `start` reasserts the desired state = running (clears the user's `stop`).
    let _ = store.update(&c.id, |cur| {
        cur.stopped_by_user = false;
        true
    });
    c.stopped_by_user = false;
    if matches!(
        c.status,
        delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused
    ) {
        return Err(Error::Invalid(format!("{} is already running", c.name)));
    }

    // Custom network: the SAME two-pass re-exec as `cmd_run` (see
    // `reexec_into_netns`). It was forgotten on the old `join_netns` path — which
    // never worked in rootless — and a `start` of a container with a network blew
    // up with `clone failed: EPERM`. Fixing only `run` wasn't enough: `start`
    // creates the container just like `run`, and has exactly the same namespace
    // problem.
    let reexec = std::env::var("DELONIX_REEXEC_ID").is_ok();
    if let Some(n) = c.network.clone() {
        if !reexec {
            let (netns, ip) = infra::attach_container(&c.id, &n, &c.namespace)?;
            // Re-register in the L7 proxy (`--expose`) HERE, on the host — the spawn via
            // nsenter doesn't run from the reexec'd process.
            if let Some(port) = c.expose {
                let _ = super::ingress_proxy::auto_register(&c.name, &c.namespace, &ip, port);
            }
            return reexec_start(&c.id, &netns, &ip);
        }
        c.ip = std::env::var("DELONIX_REEXEC_IP").ok();
        if let Some(ip) = c.ip.clone() {
            for spec in &c.ports {
                if let Err(e) = infra::publish_port(&ip, spec) {
                    // Custom network: cleanup in the ingress, no own slirp.
                    unpublish_ports(&c, None);
                    infra::detach_container(&c.id, &ip);
                    return Err(e);
                }
            }
            // Re-applies the persisted firewall (namespace isolation, Dependency,
            // Ingress) — the nft chain lives in the holder's EPHEMERAL netns, so a
            // restarted container would lose the isolation without this. Best-effort.
            if let Some(fw) = &c.firewall {
                if fw.enabled {
                    if let Err(e) = infra::apply_firewall(&c.id, &ip, fw) {
                        eprintln!(
                            "aviso: firewall/isolamento de '{}' não reaplicado no start: {e}",
                            c.name
                        );
                    }
                }
            }
        }
    }

    let rootfs = if runtime::is_rootless() {
        let rfs = images.root().join("containers").join(&c.id).join("rootfs");
        if !rfs.exists() {
            return Err(Error::Invalid(format!(
                "rootfs of {} no longer exists — use `run` again",
                c.name
            )));
        }
        rfs.to_string_lossy().into_owned()
    } else {
        let img = resolve_or_pull(images, &c.image)?;
        images
            .mount_rootfs(&img, &c.id)?
            .to_string_lossy()
            .into_owned()
    };

    let slirp_ports = if c.network.is_none() {
        c.ports.clone()
    } else {
        Vec::new()
    };
    let slirp_hook = |pid: i32| -> Result<()> { delonix_net::slirp_attach(pid, &slirp_ports) };
    // resolv.conf: the custom network's gateway (the ingress resolver), the slirp's DNS
    // with `-p`, or the host's (`--net host`) — see `run`.
    let dns = match &c.network {
        Some(n) => infra::resolve_net(n).ok().map(|(_, _, gw)| gw),
        None if !slirp_ports.is_empty() => Some(delonix_net::SLIRP_DNS.to_string()),
        None => None,
    };

    let log_path = images
        .root()
        .join("containers")
        .join(&c.id)
        .join("log")
        .to_string_lossy()
        .into_owned();
    let spec = RunSpec {
        detach: true,
        new_netns: !reexec && !slirp_ports.is_empty(),
        join_netns: None,
        userns: c.userns && !reexec,
        inherit_userns: reexec,
        log_path: Some(log_path),
        mounts: c.mounts.clone(),
        on_started: if slirp_ports.is_empty() {
            None
        } else {
            Some(&slirp_hook)
        },
        hosts_ip: c
            .ip
            .clone()
            .or_else(|| (!slirp_ports.is_empty()).then(|| delonix_net::SLIRP_IP.to_string())),
        dns,
        // Reproduces the original `run`'s `--user` (the `--hostname` comes from
        // `c.hostname`, read by the engine). Without this, a `start` ran as root.
        run_uid: c.run_uid,
        run_gid: c.run_gid,
        ..Default::default()
    };
    runtime::create_with(store, &mut c, &rootfs, &spec)?;
    delonix_runtime_core::events::emit(
        &super::util::state_root(),
        "container",
        "start",
        &c.id,
        &c.name,
        None,
    );
    println!("{}", c.id);
    Ok(())
}

/// The 1st pass of `start` with a custom network: re-executes itself inside the
/// netns (see `reexec_into_netns`, same mechanism, no spec — the container
/// already exists in the store, the id is enough).
fn reexec_start(id: &str, netns: &str, ip: &str) -> Result<()> {
    let prefix = infra::join_argv(id).ok_or_else(|| Error::Runtime {
        context: "join_argv",
        message: "infra de ingress em baixo — não há holder onde entrar".into(),
    })?;
    let exe = std::env::current_exe().map_err(|e| Error::Runtime {
        context: "current_exe",
        message: e.to_string(),
    })?;
    let status = std::process::Command::new(&prefix[0])
        .args(&prefix[1..])
        .arg(&exe)
        .args(["container", "start", id])
        .env("DELONIX_REEXEC_ID", id)
        .env("DELONIX_REEXEC_IP", ip)
        .env("DELONIX_ROOT", super::util::state_root())
        .status()
        .map_err(|e| Error::Runtime {
            context: "re-exec nsenter",
            message: e.to_string(),
        })?;
    if !status.success() {
        infra::detach_container(id, ip);
        return Err(Error::Invalid(format!(
            "o container não rearrancou dentro da rede '{netns}' (exit {:?})",
            status.code()
        )));
    }
    Ok(())
}

fn cmd_stop(store: &Store, id: &str, time: u64) -> Result<()> {
    let mut c = find(store, id)?;
    // BEFORE stopping: mark the desired state, otherwise the `--restart always`
    // supervisor resurrects it and the user can't stop it (measured: 6
    // incarnations after a `stop`). See `Container::stopped_by_user`.
    let _ = store.update(&c.id, |cur| {
        cur.stopped_by_user = true;
        true
    });
    // The pid MUST be read before `stop`, which sets `container.pid = None`.
    let pid = c.pid;
    // Idempotent like docker: stopping an already-stopped container succeeds
    // (it broke the natural `stop X && rm X` idiom, RC=1 for a no-op).
    if let Err(e) = runtime::stop(store, &mut c, time) {
        if matches!(e, delonix_runtime_core::Error::NotRunning(_)) {
            println!("{}", c.name);
            return Ok(());
        }
        return Err(e);
    }
    unpublish_ports(&c, pid);
    delonix_runtime_core::events::emit(
        &super::util::state_root(),
        "container",
        "stop",
        &c.id,
        &c.name,
        None,
    );
    println!("{}", c.id);
    Ok(())
}

/// Remove an ALREADY resolved container (`cmd_rm` resolves the id first). Extracted
/// so kind mode's `cluster delete` can remove nodes without going through strings.
pub(crate) fn remove_container(
    images: &ImageStore,
    store: &Store,
    c: &Container,
    force: bool,
) -> Result<()> {
    let pid = c.pid;
    runtime::remove(store, c, force)?;
    unpublish_ports(c, pid);
    let _ = images.unmount_rootfs(&c.id);
    images.remove_container_dir(&c.id);
    Ok(())
}

fn cmd_rm(images: &ImageStore, store: &Store, id: &str, force: bool) -> Result<()> {
    let c = find(store, id)?;
    let pid = c.pid;
    runtime::remove(store, &c, force)?;
    unpublish_ports(&c, pid);
    // De-register from the L7 proxy if it was exposed (`--expose`) — removes the route + SIGHUP.
    if c.expose.is_some() {
        super::ingress_proxy::auto_deregister(&c.name);
    }
    let _ = images.unmount_rootfs(&c.id); // unmounts/cleans up the overlay scratch
                                          // Definitive DESTROY of the container's directory (including the flat `rootfs/`).
                                          // `unmount_rootfs` PRESERVES it on purpose (it's the container's state, for
                                          // `start` to reuse); only `rm` may delete it. Without this the rootfs was left
                                          // orphaned forever: 49 directories (45 GiB) piled up in a single test session,
                                          // and the kubelet marked the node with `disk-pressure`. The `remove_container_dir`
                                          // doc already said "called by `rm`" — but it wasn't.
    images.remove_container_dir(&c.id);
    delonix_runtime_core::events::emit(
        &super::util::state_root(),
        "container",
        "remove",
        &c.id,
        &c.name,
        None,
    );
    println!("{}", c.id);
    Ok(())
}

fn cmd_exec(
    store: &Store,
    id: &str,
    interactive: bool,
    tty: bool,
    command: &[String],
) -> Result<()> {
    let c = find(store, id)?;
    let _ = interactive; // stdin is inherited; the flag keeps CLI parity
    let code = runtime::exec(&c, command, tty)?;
    std::process::exit(code);
}

/// `container inspect` — dumps the full spec stored in the Store (the runtime's
/// source of truth), as a docker-style JSON array.
fn cmd_inspect(store: &Store, ids: &[String]) -> Result<()> {
    let mut cs = Vec::new();
    for id in ids {
        let mut c = find(store, id)?;
        if runtime::reconcile_status(&mut c) {
            c = store.update(&c.id, runtime::reconcile_status).unwrap_or(c);
        }
        cs.push(c);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&cs).map_err(|e| Error::Invalid(e.to_string()))?
    );
    Ok(())
}

/// `container pause`/`unpause` — cgroup v2 freezer.
///
/// **Needs cgroup delegation**: in rootless without it (`systemd-run --user
/// --scope -p Delegate=yes`, or a unit with `Delegate=yes`), `cgroup.freeze`
/// isn't writable and this fails — not a bug, it's the model.
fn cmd_freeze(store: &Store, id: &str, frozen: bool) -> Result<()> {
    let c = find(store, id)?;
    runtime::set_frozen(&c, frozen)?;
    println!("{}", short_id(&c.id));
    Ok(())
}

/// `container commit` — the container's current rootfs becomes a new image.
///
/// **Two paths, the SAME as `delonix build`** (see `cmd::build`): in rootless the
/// rootfs is FLAT (no overlay, so no upperdir to take a diff from) and the whole
/// rootfs is packaged with `commit_flat_rootfs`; in root there's an overlay and
/// `commit_upper` takes just the diff layer, which is much cheaper.
///
/// The version that was in the PaaS only did the overlay path and, in rootless,
/// blew up with "failed to package the diff: No such file or directory" — the
/// upperdir doesn't exist. Porting without this would be porting the bug.
fn cmd_commit(images: &ImageStore, store: &Store, id: &str, tag: &str) -> Result<()> {
    let c = find(store, id)?;
    let base = images.resolve(&c.image).map_err(|_| {
        Error::Invalid(format!(
            "the container's base image '{}' no longer exists",
            c.image
        ))
    })?;
    let img = if runtime::is_rootless() {
        let rootfs = images.root().join("containers").join(&c.id).join("rootfs");
        if !rootfs.exists() {
            return Err(Error::Invalid(format!(
                "'{}' não tem rootfs em disco — foi removido, ou o container nunca chegou a arrancar",
                c.name
            )));
        }
        images.commit_flat_rootfs(
            &rootfs,
            c.command.clone(),
            c.env.clone(),
            c.workdir.clone().unwrap_or_default(),
            tag,
        )?
    } else {
        let layer = images.commit_upper(&c.id)?; // tar of the upperdir → CAS
        images.commit_container(&base, layer, c.command.clone(), c.env.clone(), tag)?
    };
    println!("{}  {}", img.short_id(), img.repo_tags.join(", "));
    Ok(())
}

/// `container ssh` — interactive shell. With no command, tries bash and falls back to sh.
fn cmd_ssh(store: &Store, id: &str, command: &[String]) -> Result<()> {
    let c = find(store, id)?;
    let argv: Vec<String> = if command.is_empty() {
        // `exec` in the shell: bash replaces sh instead of leaving a parent waiting.
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "exec /bin/bash 2>/dev/null || exec /bin/sh".into(),
        ]
    } else {
        command.to_vec()
    };
    std::process::exit(runtime::exec(&c, &argv, true)?);
}

/// `container healthcheck` — runs the image's `HEALTHCHECK` inside it.
/// Exits with 1 on `unhealthy`, to serve as a gate in scripts/CI.
fn cmd_healthcheck(images: &ImageStore, store: &Store, id: &str) -> Result<()> {
    let c = find(store, id)?;
    let img = images.resolve(&c.image)?;
    let hc = img
        .config
        .healthcheck
        .clone()
        .ok_or_else(|| Error::Invalid(format!("image '{}' defines no HEALTHCHECK", c.image)))?;
    if !c.pid.map(runtime::is_alive).unwrap_or(false) {
        return Err(Error::NotRunning(short_id(&c.id).to_string()));
    }
    let code = runtime::exec(&c, &["/bin/sh".to_string(), "-c".to_string(), hc], false)?;
    if code == 0 {
        println!("healthy");
        Ok(())
    } else {
        println!("unhealthy (exit {code})");
        std::process::exit(1);
    }
}

/// `container top` — the container's processes, via `cgroup.procs`.
///
/// The PIDs are the HOST's (that's what the cgroup lists); inside the container,
/// with its own PID namespace, the numbers are different. The column says
/// `HOST-PID` so as not to mislead anyone comparing with a `ps` from inside.
fn cmd_top(store: &Store, id: &str) -> Result<()> {
    let c = find(store, id)?;
    if !c.pid.map(runtime::is_alive).unwrap_or(false) {
        return Err(Error::NotRunning(short_id(&c.id).to_string()));
    }
    // `Container::cgroup()` is the path the engine TRIED to use
    // (`<slice>/delonix-<id>`); in rootless without delegation the container isn't
    // there. We read init's REAL cgroup from `/proc/<pid>/cgroup` — the same
    // technique as the `cgroup_metric` that `stats` already uses, and which works
    // whatever the delegated base is. The PaaS version used the guessed path and
    // gave "cgroup.procs: No such file or directory" on any host without delegation.
    let pid = c
        .pid
        .ok_or_else(|| Error::NotRunning(short_id(&c.id).to_string()))?;
    let procs = cgroup_metric(pid, "cgroup.procs").ok_or_else(|| {
        Error::Invalid(format!(
            "não consigo ler o cgroup.procs de '{}' — o cgroup do container não está acessível (rootless sem delegação?)",
            c.name
        ))
    })?;
    let mut t = output::Table::new(&["HOST-PID", "STATE", "COMMAND"]);
    for line in procs.lines() {
        let pid = line.trim();
        if pid.is_empty() {
            continue;
        }
        // Field 3 of /proc/<pid>/stat, after the comm — which can have spaces and
        // parentheses, hence cutting at the LAST ')'.
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                s.rsplit(')')
                    .next()
                    .map(|r| r.trim().chars().next().unwrap_or('?').to_string())
            })
            .unwrap_or_else(|| "?".into());
        let cmd = std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .map(|s| s.replace('\0', " ").trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            // A kernel/zombie process has an empty cmdline — comm is the fallback.
            .or_else(|| {
                std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .ok()
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_default();
        t.row(vec![pid.to_string(), state, cmd]);
    }
    t.print();
    Ok(())
}

/// `container diff` — the overlay's upperdir IS the diff against the image.
/// Whiteouts (char device 0:0) = `D`(eleted); the rest = `A`(dded/changed).
fn cmd_diff(images: &ImageStore, store: &Store, id: &str) -> Result<()> {
    let c = find(store, id)?;
    let upper = images.root().join("containers").join(&c.id).join("upper");
    if !upper.exists() {
        // Rootless uses a FLAT rootfs (no overlay), so there's no upperdir to take
        // a diff from. Saying so is better than printing nothing and looking like
        // "no changes" — which is a different answer.
        return Err(Error::Invalid(format!(
            "'{}' não tem upperdir de overlay — o `diff` compara o overlay com a imagem, e em rootless o rootfs é flat",
            c.name
        )));
    }
    fn walk(
        base: &std::path::Path,
        dir: &std::path::Path,
        out: &mut Vec<(char, String)>,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::FileTypeExt;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let ft = entry.file_type()?;
            if ft.is_char_device() {
                out.push(('D', format!("/{rel}"))); // overlay whiteout = deleted
            } else if ft.is_dir() {
                out.push(('A', format!("/{rel}")));
                walk(base, &path, out)?;
            } else {
                out.push(('A', format!("/{rel}")));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(&upper, &upper, &mut out).map_err(|e| Error::Invalid(format!("diff: {e}")))?;
    out.sort_by(|a, b| a.1.cmp(&b.1));
    for (k, p) in out {
        println!("{k} {p}");
    }
    Ok(())
}

/// A container's filesystem root, for `cp`: if it's alive,
/// `/proc/<pid>/root` (which respects the mounts it has, including those that
/// `container update --volume-add` added hot); otherwise, the rootfs on disk.
fn container_fs_root(images: &ImageStore, c: &Container) -> Result<std::path::PathBuf> {
    if let Some(pid) = c.pid.filter(|p| runtime::is_alive(*p)) {
        return Ok(std::path::PathBuf::from(format!("/proc/{pid}/root")));
    }
    let dir = images.root().join("containers").join(&c.id);
    for cand in ["merged", "rootfs"] {
        let p = dir.join(cand);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(Error::Invalid(format!(
        "container '{}' parado e sem rootfs em disco — arranca-o (`delonix container start {}`)",
        c.name, c.name
    )))
}

fn copy_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// Splits `name:/path`. `None` = it's a host path.
///
/// The `:` has to come before any `/`, otherwise `./a:b/c` or an absolute path
/// with `:` in the name would be read as a container.
fn split_cp_arg(s: &str) -> Option<(String, String)> {
    let colon = s.find(':')?;
    if s[..colon].is_empty() || s[..colon].contains('/') {
        return None;
    }
    Some((s[..colon].to_string(), s[colon + 1..].to_string()))
}

/// `container cp` — copies host↔container. Exactly one side is `container:/path`.
fn cmd_cp(images: &ImageStore, store: &Store, src: &str, dst: &str) -> Result<()> {
    let join_root = |root: &std::path::Path, p: &str| root.join(p.trim_start_matches('/'));
    match (split_cp_arg(src), split_cp_arg(dst)) {
        (Some((name, cpath)), None) => {
            let c = find(store, &name)?;
            let root = container_fs_root(images, &c)?;
            copy_recursive(&join_root(&root, &cpath), std::path::Path::new(dst))
                .map_err(|e| Error::Invalid(format!("cp: {e}")))?;
        }
        (None, Some((name, cpath))) => {
            let c = find(store, &name)?;
            let root = container_fs_root(images, &c)?;
            copy_recursive(std::path::Path::new(src), &join_root(&root, &cpath))
                .map_err(|e| Error::Invalid(format!("cp: {e}")))?;
        }
        _ => {
            return Err(Error::Invalid(
                "uso: delonix container cp <SRC> <DST> — exactamente um dos lados é `container:/caminho`".into(),
            ));
        }
    }
    Ok(())
}

/// `container describe` — human-readable detail in `kubectl describe` style.
///
/// Complements `inspect` (JSON, for machines/`jq`) rather than replacing it:
/// this is the view for a human to understand a container's state without
/// counting braces. `inspect` remains the stable contract for scripts.
fn cmd_describe(store: &Store, ids: &[String]) -> Result<()> {
    for (i, id) in ids.iter().enumerate() {
        let mut c = find(store, id)?;
        if runtime::reconcile_status(&mut c) {
            c = store.update(&c.id, runtime::reconcile_status).unwrap_or(c);
        }
        if i > 0 {
            println!();
        }
        describe_one(&c);
    }
    Ok(())
}

fn describe_one(c: &Container) {
    let mut d = output::Describe::new();
    d.field("Name", &c.name);
    d.field("ID", &c.id);
    d.field("Image", &c.image);
    d.field("Command", c.command.join(" "));
    d.field_opt("Workdir", c.workdir.as_deref());
    d.field("Created", output::fmt_local(c.created_unix));

    let uptime = match c.status {
        Status::Running | Status::Paused => c.pid_starttime.and_then(output::uptime_from_starttime),
        _ => None,
    };
    d.field("Status", fmt_status(&c.status, uptime));
    match c.pid {
        Some(p) => d.field("PID", p.to_string()),
        None => d.field("PID", "<none>"),
    };
    d.field_opt("Pod", c.pod.as_deref());

    d.section("Resources");
    d.sub("CPUs", &c.cpus);
    d.sub("Memory", &c.memory_max);
    d.sub_opt("CPU weight", c.cpu_weight.as_deref());
    d.sub_opt("Cpuset", c.cpuset.as_deref());
    d.sub_opt("IO weight", c.io_weight.as_deref());
    d.sub_opt("Nice", c.nice.map(|n| n.to_string()));

    d.section("Network");
    d.sub("Mode", c.network.as_deref().unwrap_or("host"));
    d.sub("IP", c.ip.as_deref().unwrap_or("<none>"));
    if !c.extra_networks.is_empty() {
        d.sub(
            "Extra",
            c.extra_networks
                .iter()
                .map(|n| format!("{} ({} em eth{})", n.network, n.ip, n.idx))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if !c.net_aliases.is_empty() {
        d.sub("Aliases", c.net_aliases.join(", "));
    }
    if let Some(bps) = &c.net_bps {
        d.sub(
            "Rate limit",
            format!(
                "{bps}{}",
                c.net_burst
                    .as_ref()
                    .map(|b| format!(" (burst {b})"))
                    .unwrap_or_default()
            ),
        );
    }
    d.sub(
        "Ports",
        if c.ports.is_empty() {
            "<none>".to_string()
        } else {
            fmt_ports(&c.ports)
        },
    );

    if c.mounts.is_empty() {
        d.field("Mounts", "<none>");
    } else {
        d.section("Mounts");
        for m in &c.mounts {
            // `kubectl describe pod` format: "<target> from <source> (rw)".
            d.item(format!(
                "{} from {} ({})",
                m.target,
                m.source,
                if m.readonly { "ro" } else { "rw" }
            ));
        }
    }

    d.list("Tmpfs", &c.tmpfs);
    d.list("Devices", &c.devices);
    d.list("Env", &c.env);
    // Only the NAMES of the secrets — the value is never printed (the `describe`
    // is routinely pasted into issues/chats).
    d.list("Secrets", &c.secrets);

    if c.labels.is_empty() {
        d.field("Labels", "<none>");
    } else {
        d.section("Labels");
        for (k, v) in &c.labels {
            d.item(format!("{k}={v}"));
        }
    }

    d.section("Security");
    d.sub("Privileged", c.privileged.to_string());
    d.sub("Read-only", c.read_only.to_string());
    d.sub("Userns", c.userns.to_string());
    d.sub_opt("Seccomp", c.seccomp.as_deref());
    d.sub_opt("AppArmor", c.apparmor.as_deref());
    if !c.cap_add.is_empty() {
        d.sub("Cap add", c.cap_add.join(", "));
    }
    if !c.cap_drop.is_empty() {
        d.sub("Cap drop", c.cap_drop.join(", "));
    }

    d.field(
        "Restart policy",
        c.restart_policy.as_deref().unwrap_or("no"),
    );
    d.field_opt("Log driver", c.log_driver.as_deref());
    d.print();
}

/// Arguments for `container update`, grouped (clippy would complain about the list).
pub(crate) struct UpdateOpts {
    pub(crate) publish_add: Vec<String>,
    pub(crate) publish_rm: Vec<String>,
    pub(crate) volume_add: Vec<String>,
    pub(crate) volume_rm: Vec<String>,
    pub(crate) net_connect: Vec<String>,
    pub(crate) net_disconnect: Vec<String>,
    pub(crate) net_rate: Option<String>,
    pub(crate) net_burst: Option<String>,
    pub(crate) net_rate_clear: bool,
}

impl UpdateOpts {
    fn is_empty(&self) -> bool {
        self.publish_add.is_empty()
            && self.publish_rm.is_empty()
            && self.volume_add.is_empty()
            && self.volume_rm.is_empty()
            && self.net_connect.is_empty()
            && self.net_disconnect.is_empty()
            && self.net_rate.is_none()
            && !self.net_rate_clear
    }
}

/// Converts a rate (`10mbit`, `512kbit`, `1gbit`, or raw bit/s) into bit/s.
/// Pure function — the suffixes are decimal (k=1000), like `tc`, and NOT 1024:
/// a `10mbit` that gave 10485760 bit/s would not be what `tc` programs.
fn parse_rate_bits(s: &str) -> Result<u64> {
    let t = s.trim().to_lowercase();
    let t = t.strip_suffix("bit").unwrap_or(&t);
    let (num, mult) = match t.strip_suffix('g') {
        Some(n) => (n, 1_000_000_000u64),
        None => match t.strip_suffix('m') {
            Some(n) => (n, 1_000_000),
            None => match t.strip_suffix('k') {
                Some(n) => (n, 1_000),
                None => (t, 1),
            },
        },
    };
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| Error::Invalid(format!("invalid rate: {s} (e.g. 10mbit, 512kbit, 1gbit)")))?;
    if v <= 0.0 {
        return Err(Error::Invalid(format!("taxa tem de ser positiva: {s}")));
    }
    Ok((v * mult as f64) as u64)
}

/// Converts a burst size (`32kb`, `1mb`, or raw bytes) into bytes.
fn parse_burst_bytes(s: &str) -> Result<u64> {
    let t = s.trim().to_lowercase();
    let t = t.strip_suffix('b').unwrap_or(&t);
    let (num, mult) = match t.strip_suffix('m') {
        Some(n) => (n, 1_000_000u64),
        None => match t.strip_suffix('k') {
            Some(n) => (n, 1_000),
            None => (t, 1),
        },
    };
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| Error::Invalid(format!("invalid burst: {s} (e.g. 32kb, 1mb)")))?;
    Ok((v * mult as f64) as u64)
}

/// Next free interface index for an additional network. `eth0` is always the
/// primary network, so the extras start at 1 — and we reuse holes left by a
/// `--net-disconnect` instead of always counting upward.
fn next_extra_idx(c: &Container) -> u32 {
    (1u32..)
        .find(|i| !c.extra_networks.iter().any(|n| n.idx == *i))
        .unwrap_or(1)
}

/// `container update` — HOT reconfiguration of a running container.
///
/// The operation order is deliberate: **removals before additions**. A
/// `--publish-rm 8080 --publish-add 8080:9000` in a single command has to work
/// (it's the obvious use case: "move this port to another target"); in the
/// reverse order, the add would collide with the port the rm was about to free.
///
/// Each operation persists to the registry AS SOON AS the dataplane confirms, one
/// by one, and not in a final `update`: if the third fails, the first two are
/// ALREADY applied in fact in the kernel — a record written only at the end would
/// lie about the real state. So there's no transactionality nor rollback; it
/// fails fast and whatever went through stays (same semantics as `stack apply`).
fn cmd_update(store: &Store, id: &str, o: UpdateOpts) -> Result<()> {
    if o.is_empty() {
        return Err(Error::Invalid("nothing to do: pass at least one change (--publish-add/--publish-rm/--volume-add/--volume-rm/--net-connect/--net-disconnect/--net-rate/--net-rate-clear)".into()));
    }
    let mut c = find(store, id)?;
    runtime::reconcile_status(&mut c);
    if !matches!(c.status, Status::Running | Status::Paused) {
        return Err(Error::Invalid(format!(
            "o container '{}' não está a correr ({}) — o update a quente actua no processo VIVO. \
             Arranca-o com `delonix container start {}` primeiro.",
            c.name, c.status, c.name
        )));
    }

    // --- removals first (see doc-comment) ---
    for hp in &o.publish_rm {
        unpublish_live(store, &mut c, hp)?;
    }
    for target in &o.volume_rm {
        runtime::unmount_live(&c, target)?;
        let t = target.clone();
        c = store.update(&c.id, |cur| {
            let before = cur.mounts.len();
            cur.mounts.retain(|m| m.target != t);
            cur.mounts.len() != before
        })?;
        println!("{}: volume {target} hot-unmounted", c.name);
    }
    for net in &o.net_disconnect {
        let Some(en) = c.extra_networks.iter().find(|n| &n.network == net).cloned() else {
            return Err(Error::Invalid(format!(
                "container '{}' is not attached to the extra network '{net}'",
                c.name
            )));
        };
        infra::detach_extra_container(&c.id, en.idx, &en.ip);
        let n = net.clone();
        c = store.update(&c.id, |cur| {
            let before = cur.extra_networks.len();
            cur.extra_networks.retain(|x| x.network != n);
            cur.extra_networks.len() != before
        })?;
        println!("{}: detached from network {net} (eth{})", c.name, en.idx);
    }

    // --- additions ---
    for spec in &o.publish_add {
        publish_live(store, &mut c, spec)?;
    }
    for spec in &o.volume_add {
        let mounts = resolve_mounts(std::slice::from_ref(spec))?;
        for m in mounts {
            if c.mounts.iter().any(|x| x.target == m.target) {
                return Err(Error::Invalid(format!(
                    "a volume is already mounted at {} — unmount it first (--volume-rm {})",
                    m.target, m.target
                )));
            }
            runtime::mount_live(&c, &m)?;
            let mm = m.clone();
            c = store.update(&c.id, |cur| {
                cur.mounts.push(mm.clone());
                true
            })?;
            println!(
                "{}: {} hot-mounted at {} ({})",
                c.name,
                m.source,
                m.target,
                if m.readonly { "ro" } else { "rw" }
            );
        }
    }
    for net in &o.net_connect {
        if c.network.is_none() {
            return Err(Error::Invalid(format!(
                "'{}' corre no caminho slirp-por-container (--net host/none), que não tem netns gerido pelo holder — \
                 ligar redes adicionais a quente só é possível a partir de um container criado com `--net <rede>`",
                c.name
            )));
        }
        if c.extra_networks.iter().any(|n| &n.network == net)
            || c.network.as_deref() == Some(net.as_str())
        {
            return Err(Error::Invalid(format!(
                "'{}' is already attached to network '{net}'",
                c.name
            )));
        }
        let idx = next_extra_idx(&c);
        let (ifname, ip) = infra::attach_extra_container(&c.id, idx, net)?;
        let en = delonix_runtime_core::ExtraNet {
            network: net.clone(),
            ip: ip.clone(),
            idx,
        };
        c = store.update(&c.id, |cur| {
            cur.extra_networks.push(en.clone());
            true
        })?;
        println!("{}: attached to network {net} — {ip} on {ifname}", c.name);
    }

    // --- bandwidth cap ---
    if o.net_rate_clear {
        infra::clear_net_rate(&c.id);
        c = store.update(&c.id, |cur| {
            cur.net_bps = None;
            cur.net_burst = None;
            true
        })?;
        println!("{}: bandwidth limit removed", c.name);
    }
    if let Some(rate) = &o.net_rate {
        if c.network.is_none() {
            return Err(Error::Invalid(format!(
                "'{}' corre no caminho slirp-por-container (--net host/none) — o shaping é feito no veth do lado do \
                 ingress, que só existe para containers criados com `--net <rede>`",
                c.name
            )));
        }
        let bits = parse_rate_bits(rate)?;
        let burst_s = o.net_burst.clone().unwrap_or_else(|| "32kb".to_string());
        let burst = parse_burst_bytes(&burst_s)?;
        infra::set_net_rate(&c.id, bits, burst)?;
        let (r, b) = (rate.clone(), burst_s.clone());
        store.update(&c.id, |cur| {
            cur.net_bps = Some(r.clone());
            cur.net_burst = Some(b.clone());
            true
        })?;
        println!("{}: banda limitada a {rate} (burst {burst_s})", c.name);
    }
    Ok(())
}

/// Publish a port on a LIVE container, by the right path for its network.
pub(crate) fn publish_live(store: &Store, c: &mut Container, spec: &str) -> Result<()> {
    let (hp, cp, proto) = delonix_net::parse_publish(spec)?;
    if c.ports.iter().any(|p| {
        delonix_net::parse_publish(p)
            .map(|(h, _, _)| h == hp)
            .unwrap_or(false)
    }) {
        return Err(Error::Invalid(format!(
            "'{}' already publishes host port {hp} — unpublish it first (--publish-rm {hp})",
            c.name
        )));
    }
    if let Some(owner) = port_owner(store, &hp)? {
        return Err(Error::Invalid(format!(
            "port {hp} is already published by container '{owner}'"
        )));
    }
    match c.network.as_deref() {
        // Custom network: DNAT on the holder + hostfwd on the single slirp (the ingress).
        Some(_) => {
            let ip = c.ip.clone().ok_or_else(|| {
                Error::Invalid(format!(
                    "'{}' is on a custom network but has no IP in the record",
                    c.name
                ))
            })?;
            publish_with_retry(&ip, spec)?;
        }
        // Per-container slirp path: requests the hostfwd from ITS slirp.
        None => {
            let pid = c.pid.ok_or_else(|| Error::NotRunning(c.name.clone()))?;
            let sock = delonix_net::slirp_container_sock(pid);
            if !sock.exists() {
                // The slirp's api-socket is only opened when `run` carries `-p`
                // (see `slirp_attach`): a container created without ports has no
                // way to receive a hot hostfwd. An error that teaches, instead of
                // a raw "connection refused" coming from the socket.
                return Err(Error::Invalid(format!(
                    "'{}' foi criado sem `-p` e sem `--net <rede>`, por isso o seu slirp não tem api-socket aberto — \
                     não há por onde publicar a quente. Publica pelo menos uma porta no `run`, ou usa `--net <rede>` \
                     (o ingress aceita publicações a quente sempre).",
                    c.name
                )));
            }
            delonix_net::slirp_add_hostfwd(&sock, &hp, &cp, &proto)?;
        }
    }
    let s = spec.to_string();
    *c = store.update(&c.id, |cur| {
        cur.ports.push(s.clone());
        true
    })?;
    println!("{}: port {hp}->{cp}/{proto} hot-published", c.name);
    Ok(())
}

/// Unpublish a host port on a LIVE container.
pub(crate) fn unpublish_live(store: &Store, c: &mut Container, host_port: &str) -> Result<()> {
    let hit = c
        .ports
        .iter()
        .find(|p| {
            delonix_net::parse_publish(p)
                .map(|(h, _, _)| h == host_port)
                .unwrap_or(false)
        })
        .cloned()
        .ok_or_else(|| {
            Error::Invalid(format!(
                "'{}' does not publish host port {host_port}",
                c.name
            ))
        })?;
    match c.network.as_deref() {
        Some(_) => infra::unpublish_port(host_port),
        None => {
            // Without a custom network, the hostfwd lives in the PER-container slirp —
            // which dies with it. On a stopped container there's no dataplane to clean up,
            // only the record (before: an error "container is not running" and the publish
            // stayed stuck in the record forever — a real bug report).
            if let Some(pid) = c.pid.filter(|&p| runtime::is_alive(p)) {
                let sock = delonix_net::slirp_container_sock(pid);
                if sock.exists() {
                    infra::slirp_remove_hostfwd(&sock, host_port)?;
                }
            }
        }
    }
    *c = store.update(&c.id, |cur| {
        let before = cur.ports.len();
        cur.ports.retain(|p| p != &hit);
        cur.ports.len() != before
    })?;
    println!("{}: port {host_port} hot-unpublished", c.name);
    Ok(())
}

/// Reads the cgroup v2 metric `file` of process `pid` (via `/proc/<pid>/cgroup`
/// — works whatever the delegated base where the engine placed the container).
fn cgroup_metric(pid: i32, file: &str) -> Option<String> {
    let rel = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("0::").map(str::to_string))?;
    std::fs::read_to_string(format!("/sys/fs/cgroup{}/{file}", rel.trim())).ok()
}

/// `cpu.stat` → `usage_usec` (None if the cpu controller isn't delegated).
fn cpu_usage_usec(pid: i32) -> Option<u64> {
    cgroup_metric(pid, "cpu.stat")?
        .lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
}

/// `container stats` — one sample of CPU/mem/PIDs per running container.
/// CPU% = delta of `usage_usec` over 500ms; memory from `memory.current`; with the
/// cgroup non-delegated (rootless without Delegate), it falls back to the container
/// init's VmRSS in `/proc` (only that process, marked with `~`).
fn cmd_stats(store: &Store, ids: &[String]) -> Result<()> {
    let mut cs: Vec<Container> = if ids.is_empty() {
        store.list()?
    } else {
        ids.iter().map(|i| find(store, i)).collect::<Result<_>>()?
    };
    let mut rows = Vec::new();
    for c in cs.iter_mut() {
        if runtime::reconcile_status(c) {
            let _ = store.save(c);
        }
        if !matches!(
            c.status,
            delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused
        ) {
            continue;
        }
        let Some(pid) = c.pid else { continue };
        rows.push((c.name.clone(), pid, cpu_usage_usec(pid)));
    }
    if rows.is_empty() {
        println!("(nenhum container a correr)");
        return Ok(());
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    println!(
        "{:<20}  {:>6}  {:>12}  {:>6}",
        "NAME", "CPU%", "MEM", "PIDS"
    );
    for (name, pid, cpu0) in rows {
        let cpu = match (cpu0, cpu_usage_usec(pid)) {
            (Some(a), Some(b)) => {
                format!("{:.1}", (b.saturating_sub(a)) as f64 / 500_000.0 * 100.0)
            }
            _ => "-".into(),
        };
        let (mem, approx) =
            match cgroup_metric(pid, "memory.current").and_then(|v| v.trim().parse::<u64>().ok()) {
                Some(b) => (b, false),
                None => (
                    std::fs::read_to_string(format!("/proc/{pid}/status"))
                        .ok()
                        .and_then(|s| {
                            s.lines()
                                .find_map(|l| l.strip_prefix("VmRSS:"))
                                .and_then(|v| {
                                    v.trim().trim_end_matches(" kB").trim().parse::<u64>().ok()
                                })
                        })
                        .map(|kb| kb * 1024)
                        .unwrap_or(0),
                    true,
                ),
            };
        let pids = cgroup_metric(pid, "pids.current")
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|| "-".into());
        let mem_h = if mem >= 1 << 30 {
            format!("{:.2} GiB", mem as f64 / (1u64 << 30) as f64)
        } else {
            format!("{:.1} MiB", mem as f64 / (1u64 << 20) as f64)
        };
        println!(
            "{:<20}  {:>6}  {:>12}  {:>6}",
            name,
            cpu,
            if approx { format!("~{mem_h}") } else { mem_h },
            pids
        );
    }
    Ok(())
}

fn cmd_logs(images: &ImageStore, store: &Store, id: &str, follow: bool) -> Result<()> {
    use std::io::{Read, Seek, Write};
    let c = find(store, id)?;
    let p = images.root().join("containers").join(&c.id).join("log");
    let mut f = std::fs::File::open(&p).map_err(|_| {
        Error::Invalid(format!(
            "no logs for {} (only detached containers have logs)",
            c.name
        ))
    })?;
    let mut out = std::io::stdout();
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    out.write_all(&buf)?;
    if !follow {
        return Ok(());
    }
    // `-f`: follows the appends (reopens if the file shrinks — shim rotation);
    // ends when the container stops running and there's nothing left to read.
    let mut pos = f.stream_position()?;
    loop {
        out.flush().ok();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        if len < pos {
            f = std::fs::File::open(&p)?;
            pos = 0;
        }
        if len > pos {
            f.seek(std::io::SeekFrom::Start(pos))?;
            buf.clear();
            f.read_to_end(&mut buf)?;
            pos += buf.len() as u64;
            out.write_all(&buf)?;
            continue;
        }
        let mut c = find(store, id)?;
        let _ = runtime::reconcile_status(&mut c);
        if !matches!(
            c.status,
            delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused
        ) {
            return Ok(());
        }
    }
}

/// Handles this group's `init` (see `cmd::scaffold`).
fn cmd_init(
    target: super::scaffold::Target,
    dir: PathBuf,
    name: Option<String>,
    image: Option<String>,
    force: bool,
    template: Option<String>,
    up: bool,
) -> Result<()> {
    let name = name.unwrap_or_else(|| {
        // Without `--name`, uses the DIRECTORY name. `canonicalize` can't be used:
        // the directory doesn't exist yet (it's `init` that creates it) and would
        // always fail, falling into the fallback — every project would be called "app".
        // `.`/empty resolve to the cwd; a new path uses its basename.
        let p = if dir.as_os_str().is_empty() || dir == std::path::Path::new(".") {
            std::env::current_dir().ok()
        } else {
            Some(dir.clone())
        };
        p.as_deref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".to_string())
    });
    super::scaffold::init(
        target,
        &super::scaffold::InitOpts {
            dir,
            name,
            image,
            force,
            template,
            up,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::super::util::compose_command;
    use super::{
        fmt_ports, fmt_status, next_extra_idx, parse_burst_bytes, parse_rate_bits,
        policy_supervised, should_restart, ContainerSpec,
    };
    use delonix_runtime_core::{Container, ExtraNet, Status};

    #[test]
    fn containerspec_aceita_restart_legado_e_restartpolicy_canonico() {
        let legado: ContainerSpec =
            serde_yaml::from_str("image: alpine\nrestart: always\n").unwrap();
        assert_eq!(legado.restart, "always");
        let canon: ContainerSpec =
            serde_yaml::from_str("image: alpine\nrestartPolicy: always\n").unwrap();
        assert_eq!(canon.restart, "always");
        // Without the field → the default `no`.
        let vazio: ContainerSpec = serde_yaml::from_str("image: alpine\n").unwrap();
        assert_eq!(vazio.restart, "no");
    }

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn taxas_usam_multiplos_decimais_como_o_tc() {
        // k/m/g are 1000, not 1024 — it's what `tc` programs. A `10mbit` giving
        // 10485760 bit/s would be a different limit than the one requested.
        assert_eq!(parse_rate_bits("10mbit").unwrap(), 10_000_000);
        assert_eq!(parse_rate_bits("512kbit").unwrap(), 512_000);
        assert_eq!(parse_rate_bits("1gbit").unwrap(), 1_000_000_000);
        assert_eq!(parse_rate_bits("1000").unwrap(), 1000);
        assert_eq!(parse_rate_bits("  10MBIT ").unwrap(), 10_000_000);
    }

    #[test]
    fn invalid_or_nonpositive_rate_is_rejected() {
        assert!(parse_rate_bits("depressa").is_err());
        assert!(parse_rate_bits("0").is_err());
        assert!(parse_rate_bits("-5mbit").is_err());
        assert!(parse_burst_bytes("grande").is_err());
    }

    #[test]
    fn bursts_legiveis() {
        assert_eq!(parse_burst_bytes("32kb").unwrap(), 32_000);
        assert_eq!(parse_burst_bytes("1mb").unwrap(), 1_000_000);
        assert_eq!(parse_burst_bytes("4096").unwrap(), 4096);
    }

    fn c_com_extras(idxs: &[u32]) -> Container {
        let mut c = Container::new(
            "id".into(),
            "t".into(),
            "img".into(),
            v(&["sh"]),
            "max".into(),
        );
        c.extra_networks = idxs
            .iter()
            .map(|i| ExtraNet {
                network: format!("n{i}"),
                ip: "10.0.0.2".into(),
                idx: *i,
            })
            .collect();
        c
    }

    #[test]
    fn extra_network_index_starts_at_1_and_reuses_holes() {
        // eth0 is always the primary network, so the extras start at 1.
        assert_eq!(next_extra_idx(&c_com_extras(&[])), 1);
        assert_eq!(next_extra_idx(&c_com_extras(&[1, 2])), 3);
        // A --net-disconnect of the middle one leaves a hole: it's reused, otherwise
        // the index would climb forever and the interface names would drift off eth1..N.
        assert_eq!(next_extra_idx(&c_com_extras(&[1, 3])), 2);
    }

    #[test]
    fn cp_distingue_container_de_caminho_de_host() {
        use super::split_cp_arg;
        assert_eq!(
            split_cp_arg("web:/etc/conf"),
            Some(("web".into(), "/etc/conf".into()))
        );
        assert_eq!(
            split_cp_arg("web:relativo"),
            Some(("web".into(), "relativo".into()))
        );
        // Pure host paths.
        assert_eq!(split_cp_arg("/tmp/x"), None);
        assert_eq!(split_cp_arg("ficheiro.txt"), None);
        // The ':' MUST come before any '/', otherwise a host path with a colon in
        // the name (`./a:b/c`, `/mnt/disco:1/f`) would be read as a container named
        // "./a" — and cp would write to the wrong place.
        assert_eq!(split_cp_arg("./a:b/c"), None);
        assert_eq!(split_cp_arg("/mnt/disco:1/f"), None);
        // An empty name is not a container.
        assert_eq!(split_cp_arg(":/etc"), None);
    }

    #[test]
    fn custom_net_distinguishes_host_none_from_a_network() {
        assert_eq!(super::custom_net_name("host"), None);
        assert_eq!(super::custom_net_name("none"), None);
        assert_eq!(super::custom_net_name("pnet"), Some("pnet".to_string()));
    }

    #[test]
    fn gpus_sem_dispositivos_no_host_da_lista_vazia() {
        // On a test host without /dev/nvidia* or /dev/dri, `all` invents nothing.
        // (If the CI machine has DRI, the list may not be empty — so we only assert
        // that it does NOT blow up and that an unknown spec gives empty.)
        assert!(super::expand_gpu_devices("nenhum-desses").is_empty());
    }

    #[test]
    fn ports_in_docker_ps_format() {
        assert_eq!(fmt_ports(&v(&["8080:80/tcp"])), "8080->80/tcp");
        // Without an explicit protocol, tcp (docker's default).
        assert_eq!(fmt_ports(&v(&["8080:80"])), "8080->80/tcp");
        assert_eq!(
            fmt_ports(&v(&["8080:80", "53:53/udp"])),
            "8080->80/tcp, 53->53/udp"
        );
        assert_eq!(fmt_ports(&[]), "");
    }

    #[test]
    fn status_no_formato_do_docker_ps() {
        assert_eq!(fmt_status(&Status::Running, Some(300)), "Up 5 minutes");
        assert_eq!(
            fmt_status(&Status::Paused, Some(300)),
            "Up 5 minutes (Paused)"
        );
        assert_eq!(fmt_status(&Status::Stopped, None), "Exited (0)");
        assert_eq!(fmt_status(&Status::Failed(137), None), "Exited (137)");
        assert_eq!(fmt_status(&Status::Crashed, None), "Dead");
        assert_eq!(fmt_status(&Status::Created, None), "Created");
        // Running with no readable uptime invents no duration.
        assert_eq!(fmt_status(&Status::Running, None), "Up");
    }

    #[test]
    fn user_args_replace_cmd_but_keep_entrypoint() {
        let ep = v(&["/docker-entrypoint.sh"]);
        let cmd = v(&["nginx", "-g", "daemon off;"]);
        assert_eq!(
            compose_command(&ep, &cmd, &v(&["sh", "-c", "echo hi"])),
            v(&["/docker-entrypoint.sh", "sh", "-c", "echo hi"])
        );
    }

    #[test]
    fn no_user_args_uses_cmd() {
        assert_eq!(
            compose_command(&v(&["/entry"]), &v(&["serve"]), &[]),
            v(&["/entry", "serve"])
        );
    }

    #[test]
    fn plain_cmd_without_entrypoint() {
        assert_eq!(
            compose_command(&[], &v(&["sleep", "1"]), &[]),
            v(&["sleep", "1"])
        );
        assert_eq!(compose_command(&[], &[], &v(&["sh"])), v(&["sh"]));
    }

    #[test]
    fn restart_policy_docker_semantics() {
        use delonix_runtime_core::Status as S;
        // `no` (and unknown ones): never restarts, however it died.
        for st in [S::Stopped, S::Failed(1), S::Crashed] {
            assert!(!should_restart("no", &st, 0));
            assert!(!should_restart("qualquer-coisa", &st, 0));
        }
        // `always`/`unless-stopped`: always, even on a clean exit.
        for p in ["always", "unless-stopped"] {
            assert!(should_restart(p, &S::Stopped, 0));
            assert!(should_restart(p, &S::Failed(1), 99));
            assert!(should_restart(p, &S::Crashed, 99));
        }
        // `on-failure`: only on failure; exit 0 stops.
        assert!(!should_restart("on-failure", &S::Stopped, 0));
        assert!(should_restart("on-failure", &S::Failed(2), 0));
        assert!(should_restart("on-failure", &S::Crashed, 0));
        // `on-failure:max` respects the cap (the `max` counts RESTARTS already done).
        assert!(should_restart("on-failure:3", &S::Failed(1), 2));
        assert!(!should_restart("on-failure:3", &S::Failed(1), 3));
        assert!(!should_restart("on-failure:0", &S::Failed(1), 0));
        // `on-failure` without `max` has no cap.
        assert!(should_restart("on-failure", &S::Failed(1), 10_000));
    }

    #[test]
    fn supervised_policy_only_for_active_policies() {
        assert!(!policy_supervised("no"));
        assert!(!policy_supervised(""));
        assert!(policy_supervised("always"));
        assert!(policy_supervised("unless-stopped"));
        assert!(policy_supervised("on-failure"));
        assert!(policy_supervised("on-failure:5"));
    }
}
