//! **Native kind mode** — a local Kubernetes cluster in containers, **without
//! Docker and without the `kind` binary**.
//!
//! Real `kind` is a Docker client: it speaks `docker run/ps/inspect/
//! network/exec/logs`. Supporting it would require a Docker compatibility shim
//! (large). This module skips that layer: it boots the `kindest/node` nodes
//! DIRECTLY on the Delonix engine (`cmd::container`) and runs `kubeadm` inside
//! them — the same destination, without the façade.
//!
//! # The rootless recipe (each step cost an investigation)
//!
//! A rootless Kind node does not boot "on the first try"; these are the
//! non-obvious steps, all validated end to end (control-plane `Ready`, see CLAUDE.md):
//!
//! 1. **`--privileged` + label `io.x-k8s.kind.*`** — enables in the engine the
//!    dedicated cgroup2 delegation (`setup_node_cgroup_ns`), the masking of the
//!    systemd units that fail in rootless (`mask_slow_node_units`) and the
//!    `seed_kind_nft` (without it the entrypoint picks the *legacy* iptables
//!    backend, unreadable in a userns, and dies).
//! 2. **`-p <port>:6443`** — this is not only to expose the apiserver: it is what
//!    makes the container gain its OWN netns with slirp4netns. It is mandatory — with
//!    `--net host` the netns belongs to the host, it is not "owned" by our userns, and
//!    so `CAP_NET_ADMIN` is worthless there: nft/iptables fail and the node does not boot.
//! 3. **`KubeletInUserNamespace: true`** — The decisive step. Without it the kubelet
//!    dies at `open /dev/kmsg`. (Giving it a `/dev/kmsg` does NOT fix it: the host's
//!    is `root:adm 0640` and a symlink to `/dev/console` only swaps ENOENT for EIO.)
//! 4. **`--fail-swap-on=false`** — a container inherits the HOST's `/proc/swaps`.
//! 5. **`conntrack.maxPerCore/min = 0`** in the kube-proxy — `nf_conntrack_max` is a
//!    global sysctl, not writable from a userns (otherwise: CrashLoopBackOff).
//! 6. **CNI** — the `/kind/manifests/default-cni.yaml` from the image itself.

use std::time::{Duration, Instant};

use delonix_image::ImageStore;
use delonix_runtime_core::{Container, Error, Result, Store};

use super::container::{self, RunOpts};

/// Default node image (pinned by digest — a moving tag would make the
/// clusters irreproducible across machines).
pub(crate) const DEFAULT_NODE_IMAGE: &str =
    "kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac133a309a2a30ba309614e8ec94d976dc5a";

/// Parameters of a kind-mode cluster (coming from the flags or the manifest).
pub(crate) struct KindCluster {
    pub name: String,
    pub image: String,
    /// `None` = delonix picks a free one (see `pick_api_port`).
    pub api_port: Option<u16>,
    pub pod_subnet: String,
    pub service_subnet: String,
    /// `default` = the image's CNI (kindnet); `none` = install none
    /// (the node stays `NotReady` until the user applies theirs — behavior
    /// of plain `kubeadm`, deliberate and documented).
    pub cni: String,
    /// Kubernetes version (e.g. "1.34"). `None` = whatever the image ships.
    pub k8s_version: Option<String>,
    /// How many workers to join to the control-plane (0 = control-plane only, no taint).
    pub workers: u32,
    /// How many control-planes. >1 requires a stable endpoint in front of them.
    pub control_planes: u32,
}

/// Is the port free? Checks the TWO places that matter: no live container
/// publishes it (our store) and nothing on the host holds it (test bind).
/// The store alone is not enough — any process on the machine could be there.
fn port_free(store: &Store, port: u16) -> bool {
    if super::container::port_owner(store, &port.to_string())
        .ok()
        .flatten()
        .is_some()
    {
        return false;
    }
    // The test bind releases right after (the listener drops). There is a race
    // window until slirp takes it — unavoidable without a central allocator, and
    // benign: at worst the `run` fails with the clear port-in-use error.
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Picks the apiserver port.
///
/// **Explicit wins; default is smart.** With `--api-port` given, it is honored and
/// it fails clearly if it is busy (the user asked for THAT port — silently giving
/// them another would be worse than the error). Without the flag, it tries 6443 (the
/// convention) and, if it is taken — typically by another cluster already running —
/// it picks a free high one instead of nagging: creating a 2nd cluster should not
/// force inventing ports by hand.
fn pick_api_port(store: &Store, preferred: Option<u16>, cluster: &str) -> Result<u16> {
    if let Some(p) = preferred {
        if !port_free(store, p) {
            let by = super::container::port_owner(store, &p.to_string())
                .ok()
                .flatten()
                .map(|n| super::po::tf(" (by container '{name}')", &[("name", &n)]))
                .unwrap_or_default();
            return Err(Error::Invalid(super::po::tf(
                "port {p} is already in use{by} — pick another with `--api-port` or drop the flag so delonix picks a free one",
                &[("p", &p.to_string()), ("by", &by)],
            )));
        }
        return Ok(p);
    }
    if port_free(store, DEFAULT_API_PORT) {
        return Ok(DEFAULT_API_PORT);
    }
    // 6443 taken: look for a free high one. The range is in the high ephemerals,
    // far from the service ports.
    for p in 36443..36543 {
        if port_free(store, p) {
            super::output::info(&super::po::tf(
                "port {default} in use — cluster '{cluster}' uses {p}",
                &[
                    ("default", &DEFAULT_API_PORT.to_string()),
                    ("cluster", cluster),
                    ("p", &p.to_string()),
                ],
            ));
            return Ok(p);
        }
    }
    Err(Error::Invalid(
        super::po::t("no free port found for the apiserver (6443 and 36443-36542 all taken)")
            .into(),
    ))
}

/// The conventional apiserver port — the first choice.
pub(crate) const DEFAULT_API_PORT: u16 = 6443;

/// The cluster directory on the HOST, mounted at `/kind/delonix` inside each node.
///
/// **Why a bind mount and not writing into the rootfs**: with `--net <net>` the
/// container is created by the 2nd step of the re-exec, which runs inside the MOUNT
/// namespace of the holder — the rootfs overlay is mounted THERE and is invisible from here.
/// From the host, `merged/` appears empty and any file we wrote there
/// (or read) would never reach the container: that is how kubeadm ended up
/// saying `unable to read config from /kind/delonix-kubeadm.conf` with the file
/// existing on disk. No path resolution fixes this — the mount
/// simply is not in our namespace.
/// A bind mount is mounted BY THE RUNTIME during container creation, inside
/// the right namespace, and the same directory is visible on both sides: it is the
/// missing bridge (and the mechanism already existed, `-v /host:/dest`).
fn cluster_dir(name: &str) -> std::path::PathBuf {
    super::util::state_root().join("clusters").join(name)
}

/// Where `cluster_dir` appears INSIDE the node.
const NODE_SHARED: &str = "/kind/delonix";

/// Runs a command inside the node and returns the exit code.
///
/// **The command's stdout/stderr is INHERITED** — it goes straight to the user's
/// terminal. Only use when that is what you WANT; for everything else,
/// [`node_exec_capture`].
fn node_exec(c: &Container, script: &str) -> Result<i32> {
    let argv = vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()];
    delonix_runtime::exec(c, &argv, false)
}

/// Like [`node_exec`], but **captures** the output instead of dumping it to the terminal.
/// Returns `(exit code, combined output)`.
///
/// # Why this way, and not with an `exec` that captures
///
/// `delonix_runtime::exec` inherits the parent process's stdio and has no capture
/// variant. Instead of touching a central engine API just for this,
/// it redirects INSIDE the node to a file in the shared directory (which is
/// a bind mount, see `cluster_dir`) and reads it from the host. Zero changes to the engine.
///
/// This was what was missing for the logs: the `systemctl is-active` of `wait_in_node`
/// printed `inactive`/`activating`/`active` on each probe, and the node's
/// systemd errors ("System has not been booted with systemd as init system",
/// "Failed to connect to bus") came out in the middle of the `cluster create` output as
/// if they were our failures. They are expected noise from inside the node — they belong to
/// the diagnostics of a step that fails, not the happy path.
fn node_exec_capture(c: &Container, script: &str) -> Result<(i32, String)> {
    // Per-node file: the workers run in PARALLEL and share this directory.
    let out_rel = format!(".out-{}", c.name);
    let code = node_exec(
        c,
        &format!("{{ {script} ; }} >{NODE_SHARED}/{out_rel} 2>&1"),
    )?;
    let path = cluster_dir_of(c).join(&out_rel);
    let out = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    Ok((code, out))
}

/// The `cluster_dir` of a node, from its label — `node_exec_capture` does not
/// have the `cfg` at hand.
fn cluster_dir_of(c: &Container) -> std::path::PathBuf {
    let name = c
        .labels
        .get("io.x-k8s.kind.cluster")
        .cloned()
        .unwrap_or_default();
    cluster_dir(&name)
}

/// Like [`node_exec_capture`], but fails if the command does not return 0 — and then (and
/// only then) shows what the node said, which is exactly when it matters.
fn node_must(c: &Container, what: &str, script: &str) -> Result<()> {
    let (code, out) = node_exec_capture(c, script)?;
    if code == 0 {
        return Ok(());
    }
    // The last lines are enough to diagnose and do not drown the terminal; the
    // whole output of a `kubeadm init` is hundreds of lines.
    let tail: Vec<&str> = out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(12)
        .collect();
    let detalhe = tail
        .into_iter()
        .rev()
        .map(|l| format!("\n    {l}"))
        .collect::<String>();
    Err(Error::Invalid(format!(
        "{what} falhou no nó '{}' (exit {code}){detalhe}",
        c.name
    )))
}

/// Waits for a condition inside the node (command with exit 0), with a timeout.
fn wait_in_node(c: &Container, what: &str, check: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        // Capture: the `systemctl is-active` writes the state to stdout on each
        // probe, and without this the user would see `inactive`/`activating`/`active`
        // trickling down the terminal during the whole boot.
        if node_exec_capture(c, check).map(|(rc, _)| rc).unwrap_or(1) == 0 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err(Error::Invalid(format!(
        "timeout à espera de {what} no nó '{}' ({}s)",
        c.name,
        timeout.as_secs()
    )))
}

/// The join data that the control-plane emits, extracted from
/// `kubeadm token create --print-join-command`.
#[derive(Debug, PartialEq)]
struct JoinInfo {
    endpoint: String,
    token: String,
    ca_hash: String,
}

/// Extracts `(endpoint, token, CA hash)` from the line that `kubeadm token create
/// --print-join-command` returns:
///
/// ```text
/// kubeadm join 10.0.0.2:6443 --token ab.cd --discovery-token-ca-cert-hash sha256:ef…
/// ```
///
/// # Why the line must be taken apart instead of run
///
/// The line is a complete command with positional arguments. Running it **and**
/// adding `--config` to it is what kubeadm refuses:
/// `can not mix '--config' with arguments [discovery-token-ca-cert-hash token]`.
/// Since the rootless node NEEDS its own config, the right path is the opposite:
/// take the data from here and put it INSIDE a `JoinConfiguration`, leaving the
/// `join` with only `--config`. It is what `kind` does.
fn parse_join_command(s: &str) -> Result<JoinInfo> {
    let toks: Vec<&str> = s.split_whitespace().collect();
    let flag = |name: &str| -> Option<String> {
        toks.iter()
            .position(|t| *t == name)
            .and_then(|i| toks.get(i + 1))
            .map(|v| v.to_string())
    };
    // The endpoint is the 1st token after "join" that is not a flag.
    let endpoint = toks
        .iter()
        .position(|t| *t == "join")
        .and_then(|i| toks.get(i + 1))
        .filter(|t| !t.starts_with('-'))
        .map(|t| t.to_string())
        .ok_or_else(|| {
            Error::Invalid(format!(
                "{}: {s:?}",
                super::po::t("could not read the join endpoint")
            ))
        })?;
    let token =
        flag("--token").ok_or_else(|| Error::Invalid(format!("join sem --token: {s:?}")))?;
    let ca_hash = flag("--discovery-token-ca-cert-hash")
        .ok_or_else(|| Error::Invalid(format!("join sem --discovery-token-ca-cert-hash: {s:?}")))?;
    Ok(JoinInfo {
        endpoint,
        token,
        ca_hash,
    })
}

/// A worker's `JoinConfiguration`.
///
/// **Only JoinConfiguration, no KubeletConfiguration**: `kubeadm join` pulls the
/// kubelet config from the cluster's `kubelet-config` ConfigMap, which `init` already
/// wrote with `KubeletInUserNamespace` and `failSwapOn: false`. The workers
/// inherit the rootless recipe without repeating it — and repeating it here would only create two
/// sources of truth that diverge.
fn join_config_yaml(j: &JoinInfo) -> String {
    format!(
        "apiVersion: kubeadm.k8s.io/v1beta4\n\
         kind: JoinConfiguration\n\
         discovery:\n  bootstrapToken:\n    apiServerEndpoint: \"{ep}\"\n    token: \"{tok}\"\n    caCertHashes:\n    - \"{hash}\"\n\
         nodeRegistration:\n  criSocket: unix:///run/containerd/containerd.sock\n",
        ep = j.endpoint,
        tok = j.token,
        hash = j.ca_hash,
    )
}

// Kings/queens + places of Angola: lists SHARED with the auto-generated
// container names — see `cmd/names.rs` (a single source of truth).
use super::names::{LUGARES, REIS};

/// Invents a cluster name (king + place + suffix), avoiding the ones already used.
///
/// Without this, `create` without `--name` always used "delonix" and collided on the second
/// invocation ("the node 'delonix-control-plane' already exists"), forcing the user
/// to invent names by hand. A readable name is better than a hash: it appears in
/// `cluster ls`, in the nodes and in the kubeconfig — and these are read and spoken.
///
/// Randomness without new dependencies: clock nanos + pid. It is not
/// cryptographic nor does it need to be; what matters is not colliding, and that is
/// guaranteed by the check against the existing names (the space is ~50k
/// combinations, a collision is unlikely and, if it happens, it tries another).
pub(crate) fn random_cluster_name(store: &Store) -> Result<String> {
    let existing: Vec<String> = store
        .list()?
        .iter()
        .filter_map(|c| c.labels.get("io.x-k8s.kind.cluster").cloned())
        .collect();
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 20;
    for _ in 0..50 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407); // LCG
        let r = (seed >> 33) as usize;
        let name = format!(
            "{}-{}-{:02}",
            REIS[r % REIS.len()],
            LUGARES[(r / REIS.len()) % LUGARES.len()],
            (r / (REIS.len() * LUGARES.len())) % 100
        );
        if !existing.contains(&name) {
            return Ok(name);
        }
    }
    Err(Error::Invalid(
        "não consegui inventar um nome livre — passa `--name`".into(),
    ))
}

/// Name of worker `i` (1-based), in the `kind` convention: the first is
/// `<cluster>-worker`, the following ones `<cluster>-worker2`, `-worker3`, …
fn worker_name(cluster: &str, i: u32) -> String {
    if i == 1 {
        format!("{cluster}-worker")
    } else {
        format!("{cluster}-worker{i}")
    }
}

/// Name of the cluster network. Each cluster has ITS OWN — like `kind`, which creates
/// one bridge per cluster. It is what lets the nodes see each other: without a shared network
/// a worker never reaches the apiserver (with `--net host -p`, each node is in its
/// own netns with NAT, isolated from the others).
fn cluster_net(name: &str) -> String {
    format!("dlx-{name}")
}

/// Boots ONE node of the cluster (control-plane or worker) on the shared network.
fn boot_node(
    images: &ImageStore,
    store: &Store,
    cfg: &KindCluster,
    node: &str,
    role: &str,
    publish: Vec<String>,
) -> Result<Container> {
    // No `eprintln` here: the progress is the caller's (see `Progress`), and this is
    // called in PARALLEL by the workers — each one writing its own line would give
    // interleaved output.
    container::cmd_run(
        images,
        store,
        RunOpts {
            detach: true,
            name: Some(node.to_string()),
            // Cluster network: this is what makes the nodes see each other (see `cluster_net`).
            net: cluster_net(&cfg.name),
            // The host<->node bridge (see `cluster_dir`): without this there is no way to put the
            // kubeadm.conf inside nor to bring the join/kubeconfig back out.
            volumes: vec![format!(
                "{}:{NODE_SHARED}",
                cluster_dir(&cfg.name).display()
            )],
            // `/dev/fuse`: the Kind entrypoint picks the
            // `fuse-overlayfs` snapshotter in userns, and without this device the
            // `containerd-fuse-overlayfs` dies in a loop ("fuse: device not
            // found") — containerd cannot extract A SINGLE image and
            // kubeadm fails at preflight with `[ERROR ImagePull]`. Docker's
            // `--privileged` exposes the whole host /dev and brings the
            // fuse for free; our /dev is a tmpfs with a curated list, so
            // it is requested explicitly. It is safe in rootless: on the host
            // /dev/fuse is crw-rw-rw-.
            devices: vec!["/dev/fuse".to_string()],
            ports: publish,
            privileged: true,
            entrypoint: None,
            rm: false,
            // The node's systemd is PID 1 and already supervises what runs inside.
            restart: "no".to_string(),
            env: Vec::new(),
            labels: vec![
                format!("io.x-k8s.kind.role={role}"),
                format!("io.x-k8s.kind.cluster={}", cfg.name),
            ],
            image: cfg.image.clone(),
            command: Vec::new(),
            // The progress belongs to `Progress`; the node IDs in the middle were noise.
            quiet: true,
            ..Default::default()
        },
    )?;
    let c = store
        .list()?
        .into_iter()
        .find(|c| c.name == node)
        .ok_or_else(|| {
            Error::Invalid(super::po::tf(
                "node '{node}' was not registered in the store",
                &[("node", node)],
            ))
        })?;
    wait_in_node(
        &c,
        "containerd",
        "systemctl is-active containerd",
        Duration::from_secs(90),
    )?;
    Ok(c)
}

/// Creates the cluster: boots the control-plane node and bootstraps it with `kubeadm`.
pub(crate) fn create(images: &ImageStore, store: &Store, cfg: &KindCluster) -> Result<()> {
    let node = format!("{}-control-plane", cfg.name); // kind naming convention
    if store.list()?.iter().any(|c| c.name == node) {
        return Err(Error::Invalid(format!(
            "o nó '{node}' já existe — usa `delonix cluster delete --name {}` ou outro nome",
            cfg.name
        )));
    }

    if cfg.control_planes == 0 {
        return Err(Error::Invalid("--control-planes tem de ser >= 1".into()));
    }
    if cfg.control_planes > 1 {
        // Refuse instead of pretending: with N control-planes and the
        // `controlPlaneEndpoint` pointing to the IP of the FIRST one, all the
        // kubelets and the other CPs talk to that node — if it dies, the
        // cluster dies. That is not HA, it is a single point of failure with 3 nodes and the
        // appearance of HA, which is worse than not having it. Real HA needs a
        // load-balancer in front (`kind` runs a haproxy), and that is not yet
        // done here. `cluster kubeadm` refuses for the SAME reason.
        let n = cfg.control_planes;
        return Err(Error::Invalid(if super::output::is_pt() {
            format!(
                "--control-planes {n} ainda não é suportado: o kubeadm em HA precisa de um endpoint \
                 estável (load-balancer/VIP) à frente dos control-planes, e o delonix ainda não o \
                 provisiona. Usa `--control-planes 1` (e `--workers N` para capacidade)."
            )
        } else {
            format!(
                "--control-planes {n} is not supported yet: kubeadm HA needs a stable endpoint \
                 (load-balancer/VIP) in front of the control-planes, which delonix does not provision \
                 yet. Use `--control-planes 1` (and `--workers N` for capacity)."
            )
        }));
    }

    super::output::info(&format!(
        "{} \"{}\"",
        super::po::t("Creating cluster"),
        cfg.name
    ));
    let mut p = super::output::Progress::new();

    // Each node boots via a re-exec (its own process) and, in rootless without
    // cgroup delegation, each one would print the same 7-line warning block —
    // 4× in a 4-node cluster, in the middle of the progress. It is warned ONCE here (with the
    // same test the engine does, `cgroup_limits_apply`) and ALL the
    // nodes are silenced via env — inherited by the whole re-exec chain.
    if !delonix_runtime::cgroup_limits_apply() {
        super::output::warn(
            super::po::t("rootless without cgroup delegation: the nodes' CPU/memory/PIDs limits are not enforced \
                 (namespace/seccomp isolation still holds). For limits, run under \
                 `systemd-run --user --scope -p Delegate=yes`."),
        );
    }
    // SAFETY: single-threaded here (before any worker thread); the env
    // var is read by the re-exec child processes, not by this thread.
    unsafe {
        std::env::set_var("DELONIX_NO_CGROUP_WARN", "1");
    }

    // The shared dir must exist BEFORE the 1st node (it is the bind mount target).
    std::fs::create_dir_all(cluster_dir(&cfg.name))?;
    // The cluster network: the nodes must all be born on it.
    let net = cluster_net(&cfg.name);
    let nstore = delonix_net::NetworkStore::open(super::util::state_root())?;
    if nstore.get(&net).is_err() {
        // `create_network` (and not `infra::network_create`) because there are TWO
        // coordinated stores: the declarative registry + the holder's physical plan, with the
        // same prefix. Only the physical one left `run --net` refusing with
        // "no such container: network <x>" — caught while testing multi-node.
        super::network::create_network(
            &nstore,
            &net,
            "bridge",
            None,
            None,
            "",
            None,
            Vec::new(),
            None,
        )?;
    }

    // The node image is ensured ONCE, here, before any parallelism.
    // If missing, it is pulled; if already in the store, it is reused (the `resolve` accepts
    // the digest-pinned reference). **This is not cosmetic**: the workers
    // boot in parallel and, without this step, N threads would call
    // `resolve_or_pull` at the same time and pull the SAME image N times.
    let curta = cfg
        .image
        .split('@')
        .next()
        .unwrap_or(&cfg.image)
        .to_string();
    p.step(
        &format!("{} ({curta})", super::po::t("Ensuring node image")),
        "🖼",
    );
    super::util::resolve_or_pull(images, &cfg.image)?;
    p.ok();

    // Resolve the port BEFORE booting the node: a 2nd cluster should not blow up
    // just because 6443 is taken.
    let api_port = pick_api_port(store, cfg.api_port, &cfg.name)?;
    p.step(
        &format!("{} ({})", super::po::t("Preparing nodes"), 1 + cfg.workers),
        "📦",
    );
    let c = boot_node(
        images,
        store,
        cfg,
        &node,
        "control-plane",
        vec![format!("{api_port}:6443")],
    )?;
    p.ok();
    // The node's REAL IP on the cluster network. With `--net host -p` it was the slirp's
    // (10.0.2.100, the same on every node and unreachable from outside it); on a shared
    // network each node has its own — and it is this one that the apiserver advertises and the
    // workers use in the `join`.
    let cp_ip = c.ip.clone().ok_or_else(|| {
        Error::Invalid(super::po::tf(
            "node '{node}' got no IP on network '{net}'",
            &[("node", &node), ("net", &net)],
        ))
    })?;

    // --- kubeadm config: EVERYTHING rootless needs, in a single pass ---
    //
    // One could run `kubeadm init` with flags and patch afterwards, but that
    // forces init to FAIL first (it waits 4min for a kubelet that
    // never becomes ready without the feature gate) and then run the phases by hand.
    // A config file carries the 3 tweaks BEFORE the kubelet boots —
    // a single pass, no patches. It is also what `kind` does (/kind/kubeadm.conf).
    p.step(super::po::t("Writing configuration"), "📜");
    let version = cfg
        .k8s_version
        .as_deref()
        .map(|v| format!("kubernetesVersion: v{v}\n"))
        .unwrap_or_default();
    let kubeadm_conf = format!(
        "apiVersion: kubeadm.k8s.io/v1beta4\n\
         kind: InitConfiguration\n\
         localAPIEndpoint:\n  advertiseAddress: {ip}\n  bindPort: 6443\n\
         nodeRegistration:\n  criSocket: unix:///run/containerd/containerd.sock\n\
         ---\n\
         apiVersion: kubeadm.k8s.io/v1beta4\n\
         kind: ClusterConfiguration\n{version}\
         networking:\n  podSubnet: {pods}\n  serviceSubnet: {svcs}\n\
         apiServer:\n  certSANs:\n\
         \x20 # O kubeconfig exportado aponta para 127.0.0.1:<porta publicada> — o\n\
         \x20 # apiserver so escuta no IP do slirp (10.0.2.100), que nao existe cá\n\
         \x20 # fora. Sem estes SANs o `kubectl` do HOST rebenta com\n\
         \x20 # `x509: certificate is valid for 10.0.2.100, not 127.0.0.1`.\n\
         \x20 - \"127.0.0.1\"\n  - \"localhost\"\n\
         ---\n\
         apiVersion: kubelet.config.k8s.io/v1beta1\n\
         kind: KubeletConfiguration\n\
         cgroupDriver: systemd\n\
         # Um container herda o /proc/swaps do HOST — sem isto o kubelet recusa arrancar.\n\
         failSwapOn: false\n\
         featureGates:\n  # O passo decisivo em rootless: sem ele o kubelet morre em `open /dev/kmsg`.\n  KubeletInUserNamespace: true\n\
         ---\n\
         apiVersion: kubeproxy.config.k8s.io/v1alpha1\n\
         kind: KubeProxyConfiguration\n\
         conntrack:\n  # nf_conntrack_max e um sysctl GLOBAL: nao escrevivel de um userns.\n  maxPerCore: 0\n  min: 0\n",
        ip = cp_ip,
        pods = cfg.pod_subnet,
        svcs = cfg.service_subnet,
    );
    std::fs::write(cluster_dir(&cfg.name).join("kubeadm.conf"), &kubeadm_conf)?;
    p.ok();

    // `kubeadm init` pulls the control-plane images in here — it is the slowest
    // step of all.
    p.step(super::po::t("Starting control-plane"), "🕹️");
    node_must(
        &c,
        "kubeadm init",
        &format!(
            "kubeadm init --config {NODE_SHARED}/kubeadm.conf \
             --ignore-preflight-errors=Swap,SystemVerification,FileContent--proc-sys-net-bridge-bridge-nf-call-iptables,Mem,NumCPU \
             2>&1 | tail -3"
        ),
    )?;

    p.ok();

    // --- CNI (otherwise the node stays NotReady forever) ---
    if cfg.cni == "default" {
        p.step(super::po::t("Installing CNI (kindnet)"), "🔌");
        node_must(
            &c,
            "CNI",
            &format!(
                "sed 's|{{{{ .PodSubnet }}}}|{pods}|g; s|{{{{.PodSubnet}}}}|{pods}|g' /kind/manifests/default-cni.yaml \
                 | KUBECONFIG=/etc/kubernetes/admin.conf kubectl apply -f - >/dev/null 2>&1",
                pods = cfg.pod_subnet
            ),
        )?;
        p.ok();
    }

    // Single node: without removing the taint, nothing user (not even coredns) schedules.
    // With workers, the taint STAYS — that is what they exist for (it is what kind does).
    if cfg.workers == 0 {
        let _ = node_exec(
            &c,
            "KUBECONFIG=/etc/kubernetes/admin.conf kubectl taint nodes --all \
         node-role.kubernetes.io/control-plane- >/dev/null 2>&1",
        );
    }

    p.step(super::po::t("Waiting for control-plane to be Ready"), "⏳");
    wait_in_node(
        &c,
        "o control-plane ficar Ready",
        "KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes --no-headers 2>/dev/null | grep -qw Ready",
        Duration::from_secs(180),
    )?;

    p.ok();

    // --- workers: they join via the cluster network (see `cluster_net`) ---
    if cfg.workers > 0 {
        // The `join` token is valid for 24h and comes from the control-plane. `--print-join-command`
        // returns the whole line (token + CA hash) — we do not build it by hand.
        // The CP writes the join into the SHARED dir — and the host reads it from there. Reading from
        // the rootfs would not work (see `cluster_dir`).
        node_must(
            &c,
            "gerar o comando de join",
            &format!(
                "KUBECONFIG=/etc/kubernetes/admin.conf kubeadm token create --print-join-command \
                 > {NODE_SHARED}/join.sh 2>/dev/null"
            ),
        )?;
        let join_cmd = std::fs::read_to_string(cluster_dir(&cfg.name).join("join.sh"))
            .map_err(|e| Error::Invalid(format!("a ler o comando de join: {e}")))?;
        // Take the line apart into (endpoint, token, hash) and write a
        // `JoinConfiguration` — see `parse_join_command` for why: running the
        // line AND passing `--config` is what kubeadm refuses with
        // "can not mix '--config' with arguments [...]", and that was what made
        // ALL the workers fail the join silently (the `cluster create`
        // carried on and only blew up at the end, with a "timeout waiting for workers
        // Ready" that said nothing about the cause).
        let join = parse_join_command(&join_cmd)?;
        let join_yaml = join_config_yaml(&join);

        p.step(
            &format!("{} {} worker(s)", super::po::t("Joining"), cfg.workers),
            "🚜",
        );
        // In PARALLEL: each worker is independent (boots, joins, done) and
        // in series the time added up. Each thread writes ITS OWN
        // `join-<node>.conf` — a shared file would be a race.
        let erros: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (1..=cfg.workers)
                .map(|i| {
                    let wnode = worker_name(&cfg.name, i);
                    let join_yaml = &join_yaml;
                    scope.spawn(move || -> Result<()> {
                        let conf = format!("join-{wnode}.conf");
                        std::fs::write(cluster_dir(&cfg.name).join(&conf), join_yaml)?;
                        let w = boot_node(images, store, cfg, &wnode, "worker", Vec::new())?;
                        node_must(
                            &w,
                            &format!("join do worker '{wnode}'"),
                            &format!(
                                "kubeadm join --config {NODE_SHARED}/{conf} \
                                 --ignore-preflight-errors=Swap,SystemVerification,FileContent--proc-sys-net-bridge-bridge-nf-call-iptables,Mem,NumCPU"
                            ),
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| match h.join() {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e.to_string()),
                    // A panic in a thread cannot pass for "worker ok".
                    Err(_) => Some("uma thread de worker entrou em panic".to_string()),
                })
                .collect()
        });
        if !erros.is_empty() {
            return Err(Error::Invalid(format!(
                "{} worker(s) falharam:\n  {}",
                erros.len(),
                erros.join("\n  ")
            )));
        }

        // Only now do we wait: the joins already returned OK, this is each one's
        // kubelet registering. The timeout scales with the number of workers — 3 nodes
        // pulling images at the same time take longer than 1.
        let espera = Duration::from_secs(180 + 60 * u64::from(cfg.workers));
        wait_in_node(
            &c,
            &format!("os {} worker(s) ficarem Ready", cfg.workers),
            &format!(
                "[ \"$(KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes --no-headers 2>/dev/null | grep -cw Ready)\" = \"{}\" ]",
                cfg.workers + 1
            ),
            espera,
        )?;
        p.ok();
    }

    write_kubeconfig(&c, &cfg.name, api_port)?;

    // Install the context into the user's kubeconfig — without this the cluster only
    // exists for whoever passes `--kubeconfig <file>` by hand, and it does not appear in
    // `kubectl config get-contexts`. It is what `kind` does at the end.
    let ctx = context_name(&cfg.name);
    match install_kubecontext(&cfg.name) {
        Ok(path) => {
            p.step(
                &format!("{} \"{ctx}\"", super::po::t("Setting kubectl context to")),
                "📇",
            );
            p.ok();
            let _ = path;
        }
        // Not a reason to fail the cluster: it IS up and its own kubeconfig
        // works. Say what went wrong and how to use it anyway.
        Err(e) => {
            super::output::warn(&if super::output::is_pt() {
                format!("não consegui instalar o contexto no ~/.kube/config: {e}")
            } else {
                format!("could not install the context into ~/.kube/config: {e}")
            });
            eprintln!(
                "   {}",
                super::output::dim(&if super::output::is_pt() {
                    format!(
                        "usa: kubectl --kubeconfig {} get nodes",
                        kubeconfig_path(&cfg.name).display()
                    )
                } else {
                    format!(
                        "use: kubectl --kubeconfig {} get nodes",
                        kubeconfig_path(&cfg.name).display()
                    )
                })
            );
        }
    }
    drop(p);

    println!();
    println!("{}", super::po::t("You can now use your cluster:"));
    println!();
    println!(
        "  {}",
        super::output::bold(&format!("kubectl cluster-info --context {ctx}"))
    );
    println!();
    Ok(())
}

/// The context/cluster/user name in the kubeconfig. `kind` uses
/// `kind-<name>`; the prefix says whose the cluster is and avoids colliding with a
/// real context with the same name.
fn context_name(cluster: &str) -> String {
    format!("delonix-{cluster}")
}

/// Path of the user's kubeconfig (`$KUBECONFIG` with ONLY ONE file, otherwise
/// the default).
///
/// With `$KUBECONFIG` listing SEVERAL files (`a:b:c`), kubectl merges them and
/// writes into the first — replicating that precedence here was easy to get wrong and
/// silently destructive. In that case we do not guess: we use the default.
fn user_kubeconfig_path() -> Option<std::path::PathBuf> {
    if let Some(kc) = std::env::var_os("KUBECONFIG") {
        let s = kc.to_string_lossy().to_string();
        if s.is_empty() {
            return home_kubeconfig();
        }
        if s.contains(':') {
            return home_kubeconfig();
        }
        return Some(std::path::PathBuf::from(s));
    }
    home_kubeconfig()
}

fn home_kubeconfig() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".kube").join("config"))
}

/// Merges the cluster's kubeconfig into the user's, with the entries renamed
/// to `delonix-<cluster>`, and makes it the current context.
///
/// **Not destructive**: reads what is there, replaces only the entries with OUR
/// name (a repeated `cluster create` updates instead of duplicating) and keeps
/// everything else. Writes to `.tmp` + `rename` — a crash midway does not leave the
/// user without `~/.kube/config`, which would be serious damage.
fn install_kubecontext(cluster: &str) -> Result<std::path::PathBuf> {
    use serde_yaml::Value;
    let name = context_name(cluster);
    let src = kubeconfig_path(cluster);
    let raw = std::fs::read_to_string(&src)
        .map_err(|e| Error::Invalid(format!("a ler {}: {e}", src.display())))?;
    let novo: Value = serde_yaml::from_str(&raw)
        .map_err(|e| Error::Invalid(format!("kubeconfig do cluster inválido: {e}")))?;

    let dest =
        user_kubeconfig_path().ok_or_else(|| Error::Invalid("sem $HOME nem $KUBECONFIG".into()))?;
    let mut cfg: Value = match std::fs::read_to_string(&dest) {
        Ok(t) if !t.trim().is_empty() => serde_yaml::from_str(&t).map_err(|e| {
            Error::Invalid(format!(
                "o {} existente não é YAML válido: {e}",
                dest.display()
            ))
        })?,
        // Does not exist (or is empty): start a kubeconfig from scratch.
        _ => serde_yaml::from_str(
            "apiVersion: v1\nkind: Config\nclusters: []\nusers: []\ncontexts: []\n",
        )
        .unwrap(),
    };

    // Take from the cluster's kubeconfig the 1st of each list and rename it.
    let pega =
        |v: &Value, chave: &str| -> Option<Value> { v.get(chave)?.as_sequence()?.first().cloned() };
    let mut cl =
        pega(&novo, "clusters").ok_or_else(|| Error::Invalid("kubeconfig sem clusters".into()))?;
    let mut us =
        pega(&novo, "users").ok_or_else(|| Error::Invalid("kubeconfig sem users".into()))?;
    if let Some(m) = cl.as_mapping_mut() {
        m.insert("name".into(), name.clone().into());
    }
    if let Some(m) = us.as_mapping_mut() {
        m.insert("name".into(), name.clone().into());
    }
    let ctx: Value = serde_yaml::from_str(&format!(
        "name: {name}\ncontext:\n  cluster: {name}\n  user: {name}\n"
    ))
    .map_err(|e| Error::Invalid(e.to_string()))?;

    // Replace the entry with our name, if it is already there; otherwise append it.
    let upsert = |cfg: &mut Value, chave: &str, item: Value| {
        let seq = cfg
            .as_mapping_mut()
            .unwrap()
            .entry(chave.into())
            .or_insert_with(|| Value::Sequence(vec![]));
        if !seq.is_sequence() {
            *seq = Value::Sequence(vec![]);
        }
        let s = seq.as_sequence_mut().unwrap();
        let nome = item.get("name").cloned();
        if let Some(pos) = s.iter().position(|e| e.get("name") == nome.as_ref()) {
            s[pos] = item;
        } else {
            s.push(item);
        }
    };
    upsert(&mut cfg, "clusters", cl);
    upsert(&mut cfg, "users", us);
    upsert(&mut cfg, "contexts", ctx);
    if let Some(m) = cfg.as_mapping_mut() {
        m.insert("current-context".into(), name.clone().into());
        m.entry("apiVersion".into()).or_insert_with(|| "v1".into());
        m.entry("kind".into()).or_insert_with(|| "Config".into());
    }

    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = serde_yaml::to_string(&cfg).map_err(|e| Error::Invalid(e.to_string()))?;
    let tmp = dest.with_extension("delonix.tmp");
    std::fs::write(&tmp, out)?;
    // 0600: the kubeconfig carries the cluster's admin credentials.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

fn kubeconfig_path(name: &str) -> std::path::PathBuf {
    super::util::state_root()
        .join("clusters")
        .join(format!("{name}-kubeconfig.yaml"))
}

/// Brings the node's `admin.conf` to the host, with the address rewritten to the
/// published port (inside the node it points to the slirp IP, which does not exist out here).
fn write_kubeconfig(c: &Container, name: &str, api_port: u16) -> Result<()> {
    let path = kubeconfig_path(name);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // The node writes into the SHARED dir; the host reads from there (see `cluster_dir`).
    node_must(
        c,
        "exportar o kubeconfig",
        &format!(
            "sed 's|server: https://.*:6443|server: https://127.0.0.1:{api_port}|' \
             /etc/kubernetes/admin.conf > {NODE_SHARED}/kubeconfig.yaml"
        ),
    )?;
    let src = cluster_dir(name).join("kubeconfig.yaml");
    let data = std::fs::read(&src).map_err(|e| {
        Error::Invalid(format!(
            "{} ({}): {e}",
            super::po::t("reading the node kubeconfig"),
            src.display()
        ))
    })?;
    std::fs::write(&path, data)?;
    eprintln!("kubeconfig: {}", path.display()); // universal label
    Ok(())
}

/// Removes a kind cluster: stops and deletes the nodes with the cluster label.
pub(crate) fn delete(images: &ImageStore, store: &Store, name: &str) -> Result<()> {
    let label = format!("io.x-k8s.kind.cluster={name}");
    let (k, v) = label.split_once('=').unwrap();
    let nodes: Vec<Container> = store
        .list()?
        .into_iter()
        .filter(|c| c.labels.get(k).map(|x| x == v).unwrap_or(false))
        .collect();
    if nodes.is_empty() {
        return Err(Error::NotFound(format!("cluster kind '{name}'")));
    }
    super::output::info(&format!("{} \"{name}\"", super::po::t("Deleting cluster")));
    let mut p = super::output::Progress::new();
    for n in &nodes {
        // Show EACH node being removed (ports/network freed, rootfs deleted)
        // — the delete stops looking magical, just like the create.
        p.step(
            &format!("{} '{}'", super::po::t("Removing node"), n.name),
            "🗑️",
        );
        container::remove_container(images, store, n, true)?;
        p.ok();
    }
    // The cluster network (`dlx-<name>`) was created FOR this cluster — it goes away with it
    // (unlike a user network, which a `container rm` never deletes).
    // This way the subnet/bridge become free to reuse. Volumes are NOT touched:
    // they are explicit, like in docker.
    let net = cluster_net(name);
    if let Ok(nstore) = delonix_net::NetworkStore::open(super::util::state_root()) {
        if nstore.get(&net).is_ok() {
            p.step(
                &format!("{} '{net}'", super::po::t("Freeing network")),
                "🌐",
            );
            let _ = nstore.remove(&net);
            delonix_net::infra::network_remove(&net);
            p.ok();
        }
    }
    p.step(super::po::t("Cleaning up kubeconfig and context"), "🧹");
    let _ = std::fs::remove_file(kubeconfig_path(name));
    let _ = std::fs::remove_dir_all(cluster_dir(name));
    // Remove the context from ~/.kube/config — otherwise `kubectl config get-contexts`
    // would keep listing a cluster that no longer exists, and a careless `kubectl`
    // would point to a port that may in the meantime belong to SOMETHING ELSE.
    if let Err(e) = remove_kubecontext(name) {
        p.step("", ""); // closes the cleanup step with ✗ before the warning
        super::output::warn(&if super::output::is_pt() {
            format!(
                "não consegui tirar o contexto '{}' do kubeconfig: {e}",
                context_name(name)
            )
        } else {
            format!(
                "could not remove context '{}' from kubeconfig: {e}",
                context_name(name)
            )
        });
    } else {
        p.ok();
    }
    drop(p);
    println!(
        "{}",
        if super::output::is_pt() {
            format!("cluster '{name}' removido ({} nó(s))", nodes.len())
        } else {
            format!("cluster '{name}' removed ({} node(s))", nodes.len())
        }
    );
    Ok(())
}

/// How long the node process has been up (seconds). Comes from
/// `/proc/<pid>/stat` (field 22: start in ticks since boot) crossed with
/// `/proc/uptime` — and NOT from the registry's `created_unix`, which is the creation time
/// and does not change on a restart (it would give "uptime" growing forever).
fn node_uptime_secs(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm may have spaces/parentheses; the fields are counted AFTER the ')'.
    let after = stat.rsplit_once(')')?.1;
    let start_ticks: u64 = after.split_whitespace().nth(19)?.parse().ok()?;
    let hz = 100u64; // USER_HZ is 100 on Linux/x86-64
    let up: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some((up as u64).saturating_sub(start_ticks / hz))
}

fn fmt_dur(mut s: u64) -> String {
    let d = s / 86400;
    s %= 86400;
    let h = s / 3600;
    s %= 3600;
    let m = s / 60;
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

/// `cluster ls` — the kind-mode clusters, grouped by the nodes'
/// `io.x-k8s.kind.cluster` label (it is the source of truth: there is no separate
/// "cluster" registry, and inventing one would create state that desyncs).
pub(crate) fn list(store: &Store) -> Result<()> {
    use std::collections::BTreeMap;
    let mut clusters: BTreeMap<String, Vec<Container>> = BTreeMap::new();
    for mut c in store.list()? {
        let Some(name) = c.labels.get("io.x-k8s.kind.cluster").cloned() else {
            continue;
        };
        if delonix_runtime::reconcile_status(&mut c) {
            let _ = store.update(&c.id, delonix_runtime::reconcile_status);
        }
        clusters.entry(name).or_default().push(c);
    }
    if clusters.is_empty() {
        println!(
            "{}",
            super::po::t("(no clusters — create one with `delonix cluster create`)")
        );
        return Ok(());
    }

    // The `last restart` comes from the EVENT LOG (the `Container` does not count restarts):
    // the most recent `start`/`die` of each node. It is the proof that the log serves for
    // more than `system events`.
    let evs = delonix_runtime_core::events::read(&super::util::state_root());

    // `output::Table` measures the columns by content — a long name like
    // `kitamba-benguela-81` stops pushing the other columns out of
    // alignment (which the fixed-width `println!` did). Full names,
    // not abbreviated.
    let mut t = super::output::Table::new(&[
        "NAME",
        "STATE",
        "CONTROL-PLANES",
        "WORKERS",
        "API PORT",
        "UPTIME",
        "LAST RESTART",
        "CRI SOCKET",
    ]);
    for (name, nodes) in clusters {
        let cp: Vec<&Container> = nodes
            .iter()
            .filter(|c| {
                c.labels
                    .get("io.x-k8s.kind.role")
                    .map(|r| r == "control-plane")
                    .unwrap_or(false)
            })
            .collect();
        let workers = nodes.len() - cp.len();
        let running = nodes
            .iter()
            .filter(|c| matches!(c.status, delonix_runtime_core::Status::Running))
            .count();
        let estado = if running == nodes.len() {
            "up".to_string()
        } else {
            format!("{running}/{} up", nodes.len())
        };

        // Apiserver port: the one published by the control-plane.
        let api = cp
            .first()
            .and_then(|c| c.ports.first())
            .and_then(|p| delonix_net::parse_publish(p).ok())
            .map(|(hp, _, _)| hp)
            .unwrap_or_else(|| "-".into());

        let uptime = cp
            .first()
            .and_then(|c| c.pid)
            .and_then(node_uptime_secs)
            .map(fmt_dur)
            .unwrap_or_else(|| "-".into());

        // The most recent restart of ANY node of the cluster.
        let ids: Vec<&str> = nodes.iter().map(|c| c.id.as_str()).collect();
        let last = evs
            .iter()
            .filter(|e| ids.contains(&e.id.as_str()) && (e.action == "start" || e.action == "die"))
            .map(|e| e.ts)
            .max()
            .map(delonix_runtime_core::fmt_local_ts)
            .unwrap_or_else(|| "—".into());

        // The CRI socket is what we WROTE into the cluster's kubeadm.conf — it is read
        // from there (the shared dir), instead of guessing or paying for an `exec`.
        let cri = std::fs::read_to_string(cluster_dir(&name).join("kubeadm.conf"))
            .ok()
            .and_then(|t| {
                t.lines()
                    .find(|l| l.trim_start().starts_with("criSocket:"))
                    .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
            })
            .unwrap_or_else(|| "-".into());

        t.row(vec![
            name.clone(),
            estado,
            cp.len().to_string(),
            workers.to_string(),
            api,
            uptime,
            last,
            cri,
        ]);
    }
    t.print();
    Ok(())
}

/// Removes this cluster's entries from the user's kubeconfig. Best-effort and
/// idempotent: a cluster that never got to install a context is not an error.
fn remove_kubecontext(cluster: &str) -> Result<()> {
    use serde_yaml::Value;
    let name = context_name(cluster);
    let Some(dest) = user_kubeconfig_path() else {
        return Ok(());
    };
    let Ok(txt) = std::fs::read_to_string(&dest) else {
        return Ok(());
    };
    if txt.trim().is_empty() {
        return Ok(());
    }
    let mut cfg: Value = serde_yaml::from_str(&txt).map_err(|e| {
        Error::Invalid(format!(
            "{} {}: {e}",
            dest.display(),
            super::po::t("is not valid YAML")
        ))
    })?;
    let mut mexeu = false;
    for chave in ["clusters", "users", "contexts"] {
        if let Some(seq) = cfg.get_mut(chave).and_then(|v| v.as_sequence_mut()) {
            let antes = seq.len();
            seq.retain(|e| e.get("name").and_then(|n| n.as_str()) != Some(name.as_str()));
            mexeu |= seq.len() != antes;
        }
    }
    // If the current context was ours, leaving it pointing to a context that no
    // longer exists would make kubectl fail at EVERYTHING — remove it.
    if cfg.get("current-context").and_then(|v| v.as_str()) == Some(name.as_str()) {
        if let Some(m) = cfg.as_mapping_mut() {
            m.remove(Value::from("current-context"));
        }
        mexeu = true;
    }
    if !mexeu {
        return Ok(());
    }
    let out = serde_yaml::to_string(&cfg).map_err(|e| Error::Invalid(e.to_string()))?;
    let tmp = dest.with_extension("delonix.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, &dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nomes_de_worker_seguem_a_convencao_do_kind() {
        // The first does NOT carry a number — it is `-worker`, not `-worker1`.
        assert_eq!(worker_name("c", 1), "c-worker");
        assert_eq!(worker_name("c", 2), "c-worker2");
        assert_eq!(worker_name("c", 3), "c-worker3");
    }

    #[test]
    fn parse_do_join_extrai_endpoint_token_e_hash() {
        let linha = "kubeadm join 10.217.227.44:6443 --token d9rxyc.0nlg8oq9xc53v4r1 \
                     --discovery-token-ca-cert-hash sha256:4ff6978e882b82bba8ec70ca603c13db";
        let j = parse_join_command(linha).unwrap();
        assert_eq!(j.endpoint, "10.217.227.44:6443");
        assert_eq!(j.token, "d9rxyc.0nlg8oq9xc53v4r1");
        assert_eq!(j.ca_hash, "sha256:4ff6978e882b82bba8ec70ca603c13db");
    }

    #[test]
    fn parse_do_join_tolera_espacos_e_quebras() {
        // The real `--print-join-command` comes with `\` and a newline in the middle.
        let linha = "kubeadm join 10.0.0.2:6443 --token ab.cd \\\n\t--discovery-token-ca-cert-hash sha256:ef \n";
        let j = parse_join_command(linha).unwrap();
        assert_eq!(j.endpoint, "10.0.0.2:6443");
        assert_eq!(j.ca_hash, "sha256:ef");
    }

    #[test]
    fn parse_do_join_recusa_linha_incompleta() {
        assert!(parse_join_command("kubeadm join 1.2.3.4:6443 --token ab.cd").is_err());
        assert!(
            parse_join_command("kubeadm join --token ab.cd --discovery-token-ca-cert-hash x")
                .is_err()
        );
        assert!(parse_join_command("").is_err());
    }

    #[test]
    fn join_config_nao_leva_kubelet_config() {
        // The kubelet config comes from the cluster's ConfigMap (written by `init`);
        // repeating it here created two sources of truth that diverge.
        let j = JoinInfo {
            endpoint: "10.0.0.2:6443".into(),
            token: "ab.cd".into(),
            ca_hash: "sha256:ef".into(),
        };
        let y = join_config_yaml(&j);
        assert!(y.contains("kind: JoinConfiguration"));
        assert!(y.contains("apiServerEndpoint: \"10.0.0.2:6443\""));
        assert!(y.contains("- \"sha256:ef\""));
        assert!(
            !y.contains("KubeletConfiguration"),
            "o join não deve trazer KubeletConfiguration"
        );
    }

    #[test]
    fn contexto_tem_prefixo_do_produto() {
        assert_eq!(context_name("njinga-huila-65"), "delonix-njinga-huila-65");
    }
}
