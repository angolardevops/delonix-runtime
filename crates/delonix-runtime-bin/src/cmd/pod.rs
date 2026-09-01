//! `delonix pod` — real multi-container pods. N containers share the pod's
//! **network namespace** (same IP, `localhost` between them) as ONE unit, the
//! defining property of a Kubernetes Pod.
//!
//! The shared netns is a NAMED SDN netns on the holder (`pod-<name>`, with an IP
//! on `delonix0`); each container joins it via `--pod` (the re-exec
//! `nsenter … ip netns exec`, `cmd::container::reexec_into_netns`). The pod is
//! also what the CRI's root path referred to (`delonix pod create/rm`).
//!
//! **Membership without a registry** (like `cluster`/`stack`): each container
//! carries the label `delonix.io/pod=<name>`; the pod state is derived from
//! `Store::list`. Zero new store.
//!
//! **Shared IPC + UTS**: the FIRST container holds those namespaces; the peers
//! `setns` into them (`RunOpts.pod_infra_pid` → `RunSpec.pod_infra_pid`), so the
//! pod shares System V/POSIX IPC and the hostname — safe rootless because the
//! `--pod` re-exec already put us in the holder's userns (which owns them).
//! Shared **PID** (`shareProcessNamespace`) lands next.

use super::kinds as k;
use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::Subcommand;
use delonix_net::infra;
use delonix_runtime_core::{Container, Error, Result, Status};

use super::container::{self, PodSpec};
use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::open_stores;

/// Label that ties a container to its pod (membership, derived state).
pub(crate) const POD_LABEL: &str = "delonix.io/pod";

/// The address the pod's shared netns actually got, recorded on each member at create time.
///
/// `ls`/`describe`/`rm` used to RECOMPUTE it with `infra::container_ip`, which hardcodes the
/// default prefix (`10.200/16`). That was accidentally right only while every pod landed on
/// the default bridge — the moment `spec.network` started being honored, all three reported
/// (and `rm` *detached*) an address the pod never had. Same "membership from labels" idiom
/// as [`POD_LABEL`]: derived state, no new store.
pub(crate) const POD_IP_LABEL: &str = "delonix.io/pod-ip";

/// The pod's real address: what was allocated at attach time, read back from any member's
/// label. Falls back to the legacy recomputation for pods created before the label existed
/// (those are all on the default bridge, where the recomputation is correct).
fn pod_ip(members: &[Container], netns: &str) -> String {
    members
        .iter()
        .find_map(|c| c.labels.get(POD_IP_LABEL).cloned())
        .unwrap_or_else(|| infra::container_ip(netns))
}

#[derive(Subcommand)]
pub enum PodCmd {
    /// Create a pod (N containers sharing a netns) from a manifest (`kind: Pod`).
    Create {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Logs of a pod's container (defaults to the first member).
    Logs {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::pods))]
        pod: String,
        /// Which container (its short name inside the pod). Default: the first.
        #[arg(long)]
        container: Option<String>,
        #[arg(long, short)]
        follow: bool,
    },
    /// Execute a command inside one member of a pod (defaults to the first).
    Exec {
        /// Interactive (attaches stdin).
        #[arg(short = 'i', long)]
        interactive: bool,
        /// Allocate a pseudo-terminal.
        #[arg(short = 't', long)]
        tty: bool,
        /// Extra environment variable (`KEY=VAL`) for this call only, on top of
        /// the container's own env. Repeatable.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Working directory for this call only (default: the container's own
        /// configured `workdir`, or `/`).
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
        /// Run as this user for this call only: `uid[:gid]` or `name[:group]`
        /// (resolved against the container's own `/etc/passwd`/`/etc/group`).
        /// Default: the container's own configured user.
        #[arg(short = 'u', long = "user")]
        user: Option<String>,
        /// Which container (its short name inside the pod). Default: the first.
        #[arg(long)]
        container: Option<String>,
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::pods))]
        pod: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Copy files between the host and one member of a pod.
    ///
    /// Exactly one side is `pod:/path` (e.g. `delonix pod cp web:/etc/hosts .`),
    /// same convention as `container cp`. Use `--container` when the pod has
    /// more than one member and the first isn't the one you mean.
    Cp {
        /// Which container (its short name inside the pod). Default: the first.
        #[arg(long)]
        container: Option<String>,
        src: String,
        dst: String,
    },
    /// Re-attach to a pod member's output stream (output only).
    ///
    /// Same contract as `container attach`: this engine keeps no live stdin
    /// conduit to an already-started detached container, so `-i`/`--interactive`
    /// is refused with a clear error instead of silently doing nothing.
    Attach {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::pods))]
        pod: String,
        /// Which container (its short name inside the pod). Default: the first.
        #[arg(long)]
        container: Option<String>,
        /// Refused: stdin forwarding isn't supported (see the command's own doc above).
        #[arg(short, long)]
        interactive: bool,
    },
    /// Forward one or more host ports to ports inside the pod's netns.
    ///
    /// Each `hostPort[:podPort]` binds `127.0.0.1:hostPort` on this host (loopback
    /// only, same default-safe posture as the rest of this engine's port
    /// publishing) and relays every accepted connection into the pod's shared
    /// network namespace, where `podPort` (defaulting to the same number as
    /// `hostPort`) is reached over the pod's own loopback — the same semantics a
    /// real Kubernetes Pod gives `kubectl port-forward`.
    ///
    /// Runs in the FOREGROUND, blocking until Ctrl-C, like `kubectl
    /// port-forward` — it is not a background service.
    PortForward {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::pods))]
        pod: String,
        /// `hostPort[:podPort]`, repeatable.
        #[arg(required = true)]
        ports: Vec<String>,
    },
}

pub fn run(action: PodCmd) -> Result<()> {
    match action {
        PodCmd::Create { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
        PodCmd::Logs {
            pod,
            container,
            follow,
        } => logs(&pod, container.as_deref(), follow),
        PodCmd::Exec {
            interactive,
            tty,
            env,
            workdir,
            user,
            container,
            pod,
            command,
        } => exec(
            &pod,
            container.as_deref(),
            interactive,
            tty,
            &env,
            workdir.as_deref(),
            user.as_deref(),
            &command,
        ),
        PodCmd::Cp {
            container,
            src,
            dst,
        } => cp(container.as_deref(), &src, &dst),
        PodCmd::Attach {
            pod,
            container,
            interactive,
        } => attach(&pod, container.as_deref(), interactive),
        PodCmd::PortForward { pod, ports } => port_forward(&pod, &ports),
    }
}

/// Applies the `kind: Pod` documents of a manifest.
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, k::POD) {
        let spec: PodSpec = manifest::spec_of(doc)?;
        create_pod(&doc.metadata.name, doc.metadata.namespace.clone(), spec)?;
    }
    Ok(())
}

/// Dry-run: the Pod spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: PodSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

/// The name of the pod's shared SDN netns (created once on the holder).
pub(crate) fn pod_netns_name(name: &str) -> String {
    format!("pod-{name}")
}

/// A pod name that is safe as a netns/container name prefix. The downstream
/// `attach_container`/container-name paths sanitize too, but a clear error here
/// beats a surprising failure later.
fn valid_pod_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "invalid pod name '{name}' — use letters, digits, '.' or '-' (no leading '-')"
        )))
    }
}

/// The SDN network a pod's shared netns attaches to, from `spec.network`.
///
/// `create_pod` used to pass a hardcoded `ingress` here, so `spec.network` was parsed,
/// documented as a delonix extension, and had no effect whatsoever — a pod declared on a
/// custom network came up on the default bridge, unreachable from the network it asked for
/// and reachable by everything on the one it got. Measured in
/// `docs/discovery/46_GAPS_ENCONTRADOS.md` §4.4.
///
/// `host`/`none`/empty keep meaning the default bridge: a pod IS a shared netns, so it
/// never gets the host's, and `host` is the field's default — erroring on it would reject
/// every pod manifest that exists.
fn pod_network(spec_network: &str) -> &str {
    match spec_network.trim() {
        "" | "host" | "none" => "ingress",
        other => other,
    }
}

fn create_pod(name: &str, namespace: Option<String>, spec: PodSpec) -> Result<()> {
    valid_pod_name(name)?;
    let (images, store) = open_stores()?;

    // Idempotent ("ensure present"): if the pod already has containers, do nothing.
    let already = members_of(&store, name)?;
    if !already.is_empty() {
        println!(
            "pod/{name}: already exists ({} container(s)), nothing to do",
            already.len()
        );
        return Ok(());
    }

    // 1. The pod's SHARED netns on the SDN (holder). One attach for the whole pod.
    let netns = pod_netns_name(name);
    let ns = namespace.clone().unwrap_or_else(|| "default".to_string());
    let net = pod_network(&spec.network);
    // Announced because it is the one phase that belongs to the POD rather than
    // to any member, and it is silent: it may have to bring the whole holder up
    // (`ensure_up`), which is seconds. Until now the first thing a `pod create`
    // printed was a container id from somewhere inside member one.
    let (_, ip) = output::announced(super::po::t("pod network"), "🌐", || {
        infra::attach_container(&netns, net, &ns).map_err(|e| Error::Runtime {
            context: "pod",
            message: format!("failed to create the pod netns '{netns}': {e}"),
        })
    })?;
    apply_pod_namespace_isolation(&netns, &ip, &ns);
    container::warn_if_namespace_isolation_inert(&ns);

    // 2. Each container joins THAT netns (via `--pod`) — same IP, localhost peers.
    // The FIRST container holds the pod's IPC/UTS namespaces; the rest join them
    // (via `pod_infra_pid`), so the pod shares System V/POSIX IPC + the hostname.
    let mut members = container::pod_member_run_opts(name, namespace, spec, &netns)?;
    // Record the address the pod REALLY got, so nothing downstream has to guess it (see
    // [`POD_IP_LABEL`]). Set here and not in `pod_member_run_opts` because the attach —
    // the only thing that knows the address — happens after that function's inputs are
    // fixed but before it is called.
    for opts in members.iter_mut() {
        opts.labels.push(format!("{POD_IP_LABEL}={ip}"));
    }
    let count = members.len();
    let first = members.remove(0);
    let first_name = first.name.clone().unwrap_or_else(|| format!("{name}-c0"));
    // One announced line per MEMBER, by name. `cmd_run` already draws its own
    // delayed spinner over the silent phase (unpacking an image), but that line
    // says only «unpacking the image» — measured on a three-container pod it
    // appeared three times, identical and anonymous, with nothing tying any of
    // them to a member. Naming the member is what makes a slow pod readable:
    // the question is never «is something unpacking», it is «which one».
    if let Err(e) = output::announced(&first_name, "📦", || {
        container::cmd_run(&images, &store, first)
    }) {
        let _ = remove_pod(name, true);
        return Err(e);
    }
    // The cgroup-delegation warning is about the ENVIRONMENT the members share,
    // and member one has just answered it — either it warned or there was
    // nothing to warn about. The engine dedups with a `Once`, but a `Once` sees
    // one PROCESS and every member is its own (the `--pod` re-exec), so without
    // this the same eight-line block came out once per member: measured, three
    // times on a three-container pod, interleaved with the progress lines.
    //
    // AFTER the first member and not before it, deliberately. The obvious
    // version — test up here and silence everyone — was written first and was
    // WORSE than the noise: `cgroup_limits_apply()` answers for the CLI's own
    // process, which on this host reports delegation while the members, running
    // inside the holder's userns, have none and say so. It silenced a warning
    // that was true. Only the process that actually tried can answer.
    super::util::silence_cgroup_warning();

    // The holder's init PID (host pid) — the peers `setns` its /proc/<pid>/ns/{ipc,uts}.
    let infra_pid = store.load(&first_name).ok().and_then(|c| c.pid);
    for mut opts in members {
        opts.pod_infra_pid = infra_pid;
        let member_name = opts
            .name
            .clone()
            .unwrap_or_else(|| format!("{name}-<unnamed>"));
        if let Err(e) = output::announced(&member_name, "📦", || {
            container::cmd_run(&images, &store, opts)
        }) {
            let _ = remove_pod(name, true);
            return Err(e);
        }
    }
    println!("pod/{name}: {count} container(s) sharing netns + IPC/UTS (ip {ip})");
    Ok(())
}

/// The containers that belong to a pod (by the `delonix.io/pod` label).
fn members_of(store: &delonix_runtime_core::Store, pod: &str) -> Result<Vec<Container>> {
    Ok(store
        .list()?
        .into_iter()
        .filter(|c| c.labels.get(POD_LABEL).map(|v| v == pod).unwrap_or(false))
        .collect())
}

/// Installs namespace isolation on a pod's SHARED netns address.
///
/// Pods were half-wired: `attach_container` above takes the namespace, so the
/// pod's IP DOES join `@dlxall`/`@dlxns_<ns>` — which means other namespaces'
/// containers already refuse new connections coming FROM the pod. What never
/// existed is the other direction. The isolation rules live in each workload's
/// OWN chain (`fw_chain_body`: same-namespace accept, then `@dlxall ct state
/// new drop`), and a pod had no chain at all, so nothing dropped traffic INTO
/// it. The boundary was open in exactly one direction, which is the same as
/// open.
///
/// Measured before the fix — three single-container pods on the default bridge:
///
/// ```text
///   podA(teamA) → podB(teamB)   REACHABLE   ← should be blocked
///   podA(teamA) → podA2(teamA)  reachable   ← correct
/// ```
///
/// with the holder's sets already correct (`@dlxall = {.2,.3,.4}`, teamA =
/// `{.2,.4}`, teamB = `{.3}`) and `@fwmap` **empty**. Membership was there;
/// enforcement was not.
///
/// Keyed by the pod's NETNS name rather than by a member container's id: the
/// netns is what holds the address, every member shares it, and the dataplane's
/// verdict map is keyed by IP — one entry is all it could hold anyway. The
/// teardown is already covered: `remove_pod` calls `detach_container`, which
/// sends `unfirewall <ip>`.
///
/// Best-effort, like the container path: a pod whose isolation could not be
/// installed still runs, but says so loudly instead of pretending to be fenced.
pub(crate) fn apply_pod_namespace_isolation(netns: &str, ip: &str, ns: &str) {
    if ns == "default" {
        return; // `default` is the open SDN — same contract as containers
    }
    let fw = delonix_runtime_core::ContainerFw {
        enabled: true,
        namespace: ns.to_string(),
        ..Default::default()
    };
    if let Err(e) = infra::apply_firewall(netns, ip, &fw) {
        eprintln!(
            "{}",
            super::po::tf(
                "warning: namespace isolation '{namespace}' not applied: {e}",
                &[("namespace", ns), ("e", &e.to_string())],
            )
        );
    }
}

pub(crate) fn remove_pod(name: &str, force: bool) -> Result<()> {
    let (images, store) = open_stores()?;
    let members = members_of(&store, name)?;
    if members.is_empty() {
        return Err(Error::NotFound(format!(
            "no such pod: {name} (see `delonix pod ls`)"
        )));
    }
    // Remove each member, PROPAGATING failures (no silent success — the invariant).
    // Without `--force`, `cmd_rm` refuses a RUNNING container (docker semantics), so
    // a running pod needs `-f`. A member's `rm` does NOT tear down the shared netns
    // (it is `--net host` in its own record) — same contract the CRI relies on; the
    // netns is detached ONCE, below, and ONLY after every member is gone.
    let mut failed = Vec::new();
    for c in &members {
        if let Err(e) = container::cmd_rm(&images, &store, &c.name, force) {
            failed.push(format!("{}: {e}", c.name));
        }
    }
    if !failed.is_empty() {
        return Err(Error::Invalid(format!(
            "pod '{name}': {}/{} container(s) NOT removed ({}). Retry with `delonix pod rm -f {name}`.",
            failed.len(),
            members.len(),
            failed.join("; ")
        )));
    }
    // Now safe to tear down the shared netns (all members gone).
    let netns = pod_netns_name(name);
    infra::detach_container(&netns, &pod_ip(&members, &netns));
    println!(
        "pod/{name}: removed ({} container(s) + shared netns)",
        members.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Declarative reconciliation
// ---------------------------------------------------------------------------

/// Fields the reconciler compares for a `kind: Pod`.
///
/// **Nothing here converges hot.** A pod is N containers sharing one netns plus
/// IPC/UTS; changing a member's image, or the member list, or the network means
/// tearing the shared namespace down and rebuilding it — every member restarts
/// either way. Declaring that honestly as a `Replace` is better than an
/// `Update` that turns out to be a full recreate once it runs.
pub(crate) const RECONCILED_POD_FIELDS: &[&str] = &["containers", "network", "restartPolicy"];

/// `name=image` per member, sorted — the member list as one comparable value.
/// Sorted because the order of `spec.containers[]` does not change what the pod
/// IS (the shared netns is created once, before any member starts).
fn member_key(pairs: &mut [String]) -> String {
    pairs.sort();
    pairs.join(",")
}

/// Records that this stack owns the pod, and what it last applied.
///
/// Stamps EVERY member, not just the first. A pod has no record of its own —
/// membership is derived from a label — so the stamp has to live on the
/// containers; putting it on one member only would lose ownership the moment
/// that member is the one removed.
pub(crate) fn stamp(
    name: &str,
    stack: &str,
    fields: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let (_images, store) = open_stores()?;
    let encoded = super::reconcile::encode_last_applied(fields);
    for c in members_of(&store, name)? {
        store.update(&c.id, |cur| {
            cur.labels
                .insert(super::reconcile::STACK_LABEL.into(), stack.to_string());
            cur.labels
                .insert(super::reconcile::MANAGED_BY.into(), "delonix".into());
            cur.annotations
                .insert(super::reconcile::LAST_APPLIED.into(), encoded.clone());
            true
        })?;
    }
    Ok(())
}

pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: super::container::PodSpec = manifest::spec_of(doc)?;
    let mut f = BTreeMap::new();
    let mut members: Vec<String> = spec
        .containers
        .iter()
        .enumerate()
        // The SAME fallback `pod_member_run_opts` uses when a member has no
        // name. Reproducing it here is what makes an unchanged pod diff to
        // nothing; inventing another one would report drift forever.
        .map(|(i, c)| {
            let member = c.name.clone().unwrap_or_else(|| format!("c{i}"));
            format!("{member}={}", c.image)
        })
        .collect();
    f.insert("containers".into(), member_key(&mut members));
    f.insert("network".into(), spec.network.clone());
    f.insert("restartPolicy".into(), spec.restart_policy.clone());
    Ok(super::reconcile::Desired {
        kind: k::POD.into(),
        name: doc.metadata.name.clone(),
        fields: f,
        converges: true,
        ownable: true,
    })
}

pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    let (_images, store) = open_stores()?;
    let mut pods: BTreeMap<String, Vec<Container>> = BTreeMap::new();
    for c in store.list()? {
        if let Some(pod) = c.labels.get(POD_LABEL) {
            pods.entry(pod.clone()).or_default().push(c);
        }
    }
    Ok(pods
        .into_iter()
        .map(|(pod, members)| {
            let mut names: Vec<String> = members
                .iter()
                // A member's container name is `<pod>-<member>` (see
                // `pod_member_run_opts`); the manifest names the MEMBER, so the
                // prefix has to come off or every pod diffs against itself.
                .map(|c| {
                    let short = c.name.strip_prefix(&format!("{pod}-")).unwrap_or(&c.name);
                    format!("{short}={}", c.image)
                })
                .collect();
            let mut f = BTreeMap::new();
            f.insert("containers".into(), member_key(&mut names));
            f.insert(
                "network".into(),
                members
                    .first()
                    .and_then(|c| c.net_mode.clone())
                    .unwrap_or_else(|| "host".into()),
            );
            f.insert(
                "restartPolicy".into(),
                members
                    .first()
                    .and_then(|c| c.restart_policy.clone())
                    .unwrap_or_else(|| "no".into()),
            );
            // Ownership and last-applied live on the FIRST member: a pod has no
            // record of its own (membership is derived from the label), so
            // there is nowhere else to put them. `create_pod` stamps every
            // member, so any of them would do; taking the first keeps it
            // deterministic.
            let head = members.first();
            super::reconcile::Actual {
                kind: k::POD.into(),
                name: pod,
                fields: f,
                owner: head.and_then(|c| c.labels.get(super::reconcile::STACK_LABEL).cloned()),
                last_applied: head
                    .and_then(|c| c.annotations.get(super::reconcile::LAST_APPLIED))
                    .and_then(|raw| super::reconcile::decode_last_applied(raw)),
            }
        })
        .collect())
}

/// `pod ls -o json` row (ADR-0005): running/total as numbers (not `"1/2"`), ip nullable.
#[derive(serde::Serialize)]
struct PodLsRow {
    name: String,
    /// The pod's namespace, read off its MEMBERS: `pod_member_run_opts` stamps
    /// the pod's onto every one of them, so any member answers for the pod.
    namespace: String,
    running: usize,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<String>,
    status: String,
    /// The earliest member's creation instant — a pod has no `created_unix`
    /// of its own, and the oldest member is the one that first stood up the
    /// shared netns.
    created_unix: u64,
}

/// The generic `get pods`'s only implementation of "list" — `pod ls` as a
/// dedicated CLI leaf was cut (B7): identical to `get pods [-n <ns>]` in
/// every respect, once `get` gained the `--namespace` filter this needed to
/// stop being the ONLY way to filter a pod listing by namespace.
pub(crate) fn ls(format: output::OutputFormat, namespace: Option<&str>) -> Result<()> {
    let format = super::config::resolve_output(&super::util::state_root(), format);
    let (_images, store) = open_stores()?;
    let mut pods: BTreeMap<String, Vec<Container>> = BTreeMap::new();
    for c in store.list()? {
        if let Some(pod) = c.labels.get(POD_LABEL) {
            pods.entry(pod.clone()).or_default().push(c);
        }
    }
    let mut rows = Vec::new();
    for (pod, mut members) in pods {
        let mut running = 0;
        for c in members.iter_mut() {
            let _ = delonix_runtime::reconcile_status(c);
            if matches!(c.status, Status::Running | Status::Paused) {
                running += 1;
            }
        }
        let ip = pod_ip(&members, &pod_netns_name(&pod));
        let status = if running == members.len() {
            "Running"
        } else if running == 0 {
            "Stopped"
        } else {
            "Degraded"
        };
        let created_unix = members.iter().map(|c| c.created_unix).min().unwrap_or(0);
        rows.push(PodLsRow {
            namespace: members
                .first()
                .map(|c| c.namespace.clone())
                .unwrap_or_default(),
            name: pod,
            running,
            total: members.len(),
            ip: (!ip.is_empty()).then_some(ip),
            status: status.to_string(),
            created_unix,
        });
    }
    // BEFORE the format branch — a `--namespace` that narrows the table and
    // leaves the JSON alone is a flag that works in one output and not the other.
    if let Some(ns) = namespace {
        rows.retain(|r| {
            let owner = if r.namespace.is_empty() {
                "default"
            } else {
                &r.namespace
            };
            owner == ns
        });
    }
    if format == output::OutputFormat::Json {
        return output::print_json(&rows);
    }
    let mut t = output::Table::new(&["POD", "CONTAINERS", "IP", "STATUS", "AGE", "NAMESPACE"]);
    for r in rows {
        t.row(vec![
            r.name,
            format!("{}/{}", r.running, r.total),
            r.ip.unwrap_or_else(|| "-".to_string()),
            r.status,
            output::fmt_age(r.created_unix),
            output::namespace_cell(&r.namespace, namespace.is_some()),
        ]);
    }
    t.drop_uninformative().print();
    Ok(())
}

pub(crate) fn describe(names: &[String]) -> Result<()> {
    let (_images, store) = open_stores()?;
    for name in names {
        let mut members = members_of(&store, name)?;
        if members.is_empty() {
            return Err(Error::NotFound(format!(
                "no such pod: {name} (see `delonix pod ls`)"
            )));
        }
        let mut d = output::Describe::new();
        d.field(k::POD, name);
        d.field("Namespace", &members[0].namespace);
        d.field("IP", pod_ip(&members, &pod_netns_name(name)));
        d.field("Netns", pod_netns_name(name));
        d.print();
        let mut t = output::Table::new(&["CONTAINER", "IMAGE", "STATUS"]);
        let prefix = format!("{name}-");
        for c in members.iter_mut() {
            let _ = delonix_runtime::reconcile_status(c);
            let short = c.name.strip_prefix(prefix.as_str()).unwrap_or(&c.name);
            t.row(vec![
                short.to_string(),
                c.image.clone(),
                format!("{:?}", c.status),
            ]);
        }
        t.print();
    }
    Ok(())
}

/// The pod's containers, and the ONE the caller means: `--container <short>` by
/// exact `<pod>-<short>` name, or the pod's first member when omitted. Shared by
/// `logs`/`exec`/`cp`/`attach` so the "no such pod"/"pod has no container" pair
/// exists in one place instead of a fourth copy.
fn resolve_target(
    store: &delonix_runtime_core::Store,
    pod: &str,
    container_short: Option<&str>,
) -> Result<Container> {
    let members = members_of(store, pod)?;
    if members.is_empty() {
        return Err(Error::NotFound(format!(
            "no such pod: {pod} (see `delonix pod ls`)"
        )));
    }
    match container_short {
        Some(short) => members
            .into_iter()
            .find(|c| c.name == format!("{pod}-{short}"))
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "pod '{pod}' has no container '{short}' (see `delonix pod describe {pod}`)"
                ))
            }),
        None => Ok(members.into_iter().next().unwrap()),
    }
}

fn logs(pod: &str, container_short: Option<&str>, follow: bool) -> Result<()> {
    let (images, store) = open_stores()?;
    let target = resolve_target(&store, pod, container_short)?;
    container::cmd_logs(&images, &store, &target.name, follow, None, None, false)
}

#[allow(clippy::too_many_arguments)]
fn exec(
    pod: &str,
    container_short: Option<&str>,
    interactive: bool,
    tty: bool,
    env: &[String],
    workdir: Option<&str>,
    user: Option<&str>,
    command: &[String],
) -> Result<()> {
    let (images, store) = open_stores()?;
    let target = resolve_target(&store, pod, container_short)?;
    container::cmd_exec(
        &images,
        &store,
        &target.name,
        interactive,
        tty,
        env,
        workdir,
        user,
        command,
    )
}

fn attach(pod: &str, container_short: Option<&str>, interactive: bool) -> Result<()> {
    let (images, store) = open_stores()?;
    let target = resolve_target(&store, pod, container_short)?;
    container::cmd_attach(&images, &store, &target.name, interactive)
}

/// Copies host↔pod-member. Exactly one of `src`/`dst` is `<pod>:/path`: that side
/// is rewritten to `<member-name>:/path` (the pod's real container in the store),
/// then delegated to `container::cmd_cp` unmodified — its own "exactly one side"
/// validation applies to the rewritten strings, so nothing new is validated here.
fn cp(container_short: Option<&str>, src: &str, dst: &str) -> Result<()> {
    let (images, store) = open_stores()?;
    let rewrite = |arg: &str| -> Result<String> {
        match container::split_cp_arg(arg) {
            Some((pod, path)) => {
                let target = resolve_target(&store, &pod, container_short)?;
                Ok(format!("{}:{path}", target.name))
            }
            None => Ok(arg.to_string()),
        }
    };
    let new_src = rewrite(src)?;
    let new_dst = rewrite(dst)?;
    container::cmd_cp(&images, &store, &new_src, &new_dst)
}

/// Parses `hostPort[:podPort]` — the same convention `kubectl port-forward`
/// uses: no remote host (this always binds `127.0.0.1`), and `podPort` defaults
/// to the same number as `hostPort` when omitted. Pure, no I/O.
fn parse_port_forward_spec(spec: &str) -> Result<(u16, u16)> {
    let (host, target) = spec.split_once(':').unwrap_or((spec, spec));
    let parse_one = |s: &str| -> Result<u16> {
        s.parse::<u16>().map_err(|_| {
            Error::Invalid(format!(
                "invalid port forward spec '{spec}': '{s}' is not a valid port"
            ))
        })
    };
    Ok((parse_one(host)?, parse_one(target)?))
}

/// `pod port-forward <pod> <hostPort>[:<podPort>]...` — a host↔pod-netns relay,
/// the imperative sibling of `container run --net <network>`/`--pod`: it reuses
/// the exact same [`infra::join_argv`] prefix those already use to enter a
/// netns without new privilege, it just runs a byte-relay instead of the
/// container's own command.
///
/// Foreground, like `kubectl port-forward`: blocks until Ctrl-C (closing the
/// listeners frees the ports; there is no graceful-shutdown state to save).
/// Each accepted connection spawns its own hidden `__netnsconnect <podPort>`
/// process INSIDE the pod's netns, with the accepted socket wired as BOTH its
/// stdin and stdout (one TCP socket serves both directions) — so N concurrent
/// connections to the same `hostPort` are independent processes, never sharing
/// state, and one being slow never blocks another.
fn port_forward(pod: &str, ports: &[String]) -> Result<()> {
    let (_, store) = open_stores()?;
    if members_of(&store, pod)?.is_empty() {
        return Err(Error::NotFound(format!(
            "no such pod: {pod} (see `delonix pod ls`)"
        )));
    }
    let netns = pod_netns_name(pod);
    let specs: Vec<(u16, u16)> = ports
        .iter()
        .map(|s| parse_port_forward_spec(s))
        .collect::<Result<_>>()?;

    let exe = std::env::current_exe().map_err(|e| Error::Runtime {
        context: "current_exe",
        message: e.to_string(),
    })?;

    // Bind every listener BEFORE printing anything or spawning a single
    // thread — a port already in use has to fail the whole command up front,
    // not leave earlier ports silently forwarding while a later one errors.
    let mut listeners = Vec::with_capacity(specs.len());
    for &(host_port, pod_port) in &specs {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", host_port)).map_err(|e| Error::Runtime {
                context: "port-forward bind",
                message: format!("127.0.0.1:{host_port}: {e}"),
            })?;
        listeners.push((listener, host_port, pod_port));
    }

    for (_, host_port, pod_port) in &listeners {
        println!(
            "{}",
            super::po::tf(
                "Forwarding from 127.0.0.1:{host_port} -> {pod_port}",
                &[
                    ("host_port", &host_port.to_string()),
                    ("pod_port", &pod_port.to_string()),
                ],
            )
        );
    }

    let handles: Vec<_> = listeners
        .into_iter()
        .map(|(listener, _host_port, pod_port)| {
            let netns = netns.clone();
            let exe = exe.clone();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(stream) = conn else { continue };
                    let netns = netns.clone();
                    let exe = exe.clone();
                    std::thread::spawn(move || {
                        let _ = relay_one(&exe, &netns, pod_port, stream);
                    });
                }
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

/// One forwarded connection: enters the pod's netns via [`infra::join_argv`]
/// and runs the hidden `__netnsconnect <podPort>` verb with the accepted
/// socket as its stdin AND stdout. Blocks until that process exits (i.e. until
/// the connection closes on either side) — called from its own thread, so it
/// never blocks the accept loop or another connection.
fn relay_one(
    exe: &std::path::Path,
    netns: &str,
    pod_port: u16,
    stream: std::net::TcpStream,
) -> Result<()> {
    let prefix = infra::join_argv(netns).ok_or_else(|| Error::Runtime {
        context: "join_argv",
        message: super::po::t("ingress infra is down — no holder to enter").into(),
    })?;
    let stdin_side = stream.try_clone().map_err(|e| Error::Runtime {
        context: "port-forward",
        message: e.to_string(),
    })?;
    let mut child = std::process::Command::new(&prefix[0])
        .args(&prefix[1..])
        .arg(exe)
        .args(["__netnsconnect", &pod_port.to_string()])
        .stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(
            stdin_side,
        )))
        .stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
            stream,
        )))
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| Error::Runtime {
            context: "port-forward spawn",
            message: e.to_string(),
        })?;
    let _ = child.wait();
    Ok(())
}

/// `__netnsconnect <port>` — hidden verb, intercepted before clap in `main`
/// (same idiom as `netns run`/`__rmtree`) and run INSIDE a pod's netns via
/// [`relay_one`] above. Connects to `127.0.0.1:<port>` (any pod member
/// listening there is reachable over the pod's own loopback, the same
/// semantics a real Kubernetes Pod gives its containers) and relays bytes
/// between that connection and its own stdin/stdout. A short-lived process,
/// one per forwarded connection — not a persistent server, no `tokio`.
pub fn netnsconnect(port_str: &str) -> Result<()> {
    let port: u16 = port_str
        .parse()
        .map_err(|_| Error::Invalid(format!("__netnsconnect: invalid port '{port_str}'")))?;
    let stream = std::net::TcpStream::connect(("127.0.0.1", port)).map_err(|e| Error::Runtime {
        context: "__netnsconnect connect",
        message: format!("127.0.0.1:{port}: {e}"),
    })?;
    let mut reader = stream.try_clone().map_err(|e| Error::Runtime {
        context: "__netnsconnect",
        message: e.to_string(),
    })?;
    let mut writer = stream;
    let to_local = std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let _ = std::io::copy(&mut stdin, &mut writer);
        let _ = writer.shutdown(std::net::Shutdown::Write);
    });
    let mut stdout = std::io::stdout();
    let _ = std::io::copy(&mut reader, &mut stdout);
    let _ = to_local.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_port_forward_spec_defaults_pod_port_to_host_port() {
        assert_eq!(parse_port_forward_spec("8080").unwrap(), (8080, 8080));
    }

    #[test]
    fn parse_port_forward_spec_reads_hostport_colon_podport() {
        assert_eq!(parse_port_forward_spec("18080:80").unwrap(), (18080, 80));
    }

    #[test]
    fn parse_port_forward_spec_rejects_non_numeric_or_out_of_range() {
        assert!(parse_port_forward_spec("abc").is_err());
        assert!(parse_port_forward_spec("8080:abc").is_err());
        assert!(parse_port_forward_spec("70000").is_err());
        assert!(parse_port_forward_spec("8080:70000").is_err());
    }

    /// Regression: `spec.network` was parsed and ignored — `create_pod` passed a hardcoded
    /// `ingress`, so a pod asking for a custom network came up on the default bridge in
    /// silence (`docs/discovery/46_GAPS_ENCONTRADOS.md` §4.4). The custom name is the whole
    /// point of the field; `host`/`none` keep meaning the default bridge because a pod IS a
    /// shared netns and `host` is the field's own default.
    #[test]
    fn o_spec_network_de_um_pod_escolhe_mesmo_a_rede() {
        assert_eq!(pod_network("kaeso-net"), "kaeso-net");
        assert_eq!(pod_network("  kaeso-net  "), "kaeso-net");
        for default_ish in ["", "host", "none"] {
            assert_eq!(
                pod_network(default_ish),
                "ingress",
                "`{default_ish}` has to keep meaning the default bridge"
            );
        }
    }

    /// A member with no `name` is `c<i>` by position — the fallback
    /// `pod_member_run_opts` uses to build `<pod>-<member>`. Reproducing it here
    /// is what makes an unchanged pod diff to nothing; a different fallback
    /// would report drift forever.
    #[test]
    fn o_membro_sem_nome_usa_o_mesmo_fallback_por_posicao() {
        let doc: super::ManifestDoc = serde_yaml::from_str(
            "apiVersion: delonix.io/v1\nkind: Pod\nmetadata: { name: p }\nspec:\n  containers:\n    - image: nginx\n    - name: side\n      image: redis\n",
        )
        .unwrap();
        let d = super::desired(&doc).unwrap();
        assert_eq!(d.fields.get("containers").unwrap(), "c0=nginx,side=redis");
        for k in d.fields.keys() {
            assert!(
                super::RECONCILED_POD_FIELDS.contains(&k.as_str()),
                "{k} is compared but undocumented"
            );
        }
    }
}
