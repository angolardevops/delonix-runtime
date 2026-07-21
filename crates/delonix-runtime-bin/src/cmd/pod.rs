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
//! Shared **IPC/UTS/PID** (`shareProcessNamespace`) land in later slices — this
//! one delivers the shared netns/sandbox + a unified lifecycle.

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

#[derive(Subcommand)]
pub enum PodCmd {
    /// Create a pod (N containers sharing a netns) from a manifest (`kind: Pod`).
    Create {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// List the pods (derived from container labels).
    Ls,
    /// Details of one or more pods (containers + the shared IP), `kubectl` style.
    Describe { names: Vec<String> },
    /// Remove a pod: stop/remove ALL its containers + the shared netns.
    Rm {
        names: Vec<String>,
        /// Force (kill) running containers.
        #[arg(long, short)]
        force: bool,
    },
    /// Logs of a pod's container (defaults to the first member).
    Logs {
        pod: String,
        /// Which container (its short name inside the pod). Default: the first.
        #[arg(long)]
        container: Option<String>,
        #[arg(long, short)]
        follow: bool,
    },
}

pub fn run(action: PodCmd) -> Result<()> {
    match action {
        PodCmd::Create { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
        PodCmd::Ls => ls(),
        PodCmd::Describe { names } => describe(&names),
        PodCmd::Rm { names, force } => {
            for n in &names {
                remove_pod(n, force)?;
            }
            Ok(())
        }
        PodCmd::Logs {
            pod,
            container,
            follow,
        } => logs(&pod, container.as_deref(), follow),
    }
}

/// Applies the `kind: Pod` documents of a manifest.
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, "Pod") {
        manifest::warn_unknown_fields(doc, container::POD_SPEC_FIELDS);
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
fn pod_netns_name(name: &str) -> String {
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
    let (_, ip) = infra::attach_container(&netns, "ingress", &ns).map_err(|e| Error::Runtime {
        context: "pod",
        message: format!("failed to create the pod netns '{netns}': {e}"),
    })?;

    // 2. Each container joins THAT netns (via `--pod`) — same IP, localhost peers.
    let members = container::pod_member_run_opts(name, namespace, spec, &netns)?;
    let count = members.len();
    for opts in members {
        if let Err(e) = container::cmd_run(&images, &store, opts) {
            // Roll back what we started + the shared netns, then propagate.
            let _ = remove_pod(name, true);
            return Err(e);
        }
    }
    println!("pod/{name}: {count} container(s) on shared netns (ip {ip})");
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

fn remove_pod(name: &str, force: bool) -> Result<()> {
    let (images, store) = open_stores()?;
    let members = members_of(&store, name)?;
    // Remove each member (best-effort). A member's `rm` does NOT tear down the
    // shared netns (it is `--net host` in its own record) — same contract the CRI
    // relies on; the netns is detached ONCE, below.
    for c in &members {
        let _ = container::cmd_rm(&images, &store, &c.name, force);
    }
    // Detach the pod's shared netns (idempotent-ish; harmless if already gone).
    let netns = pod_netns_name(name);
    infra::detach_container(&netns, &infra::container_ip(&netns));
    if members.is_empty() {
        return Err(Error::Invalid(format!(
            "no such pod: {name} (see `delonix pod ls`)"
        )));
    }
    println!(
        "pod/{name}: removed ({} container(s) + shared netns)",
        members.len()
    );
    Ok(())
}

fn ls() -> Result<()> {
    let (_images, store) = open_stores()?;
    let mut pods: BTreeMap<String, Vec<Container>> = BTreeMap::new();
    for c in store.list()? {
        if let Some(pod) = c.labels.get(POD_LABEL) {
            pods.entry(pod.clone()).or_default().push(c);
        }
    }
    let mut t = output::Table::new(&["POD", "CONTAINERS", "IP", "STATUS"]);
    for (pod, mut members) in pods {
        let mut running = 0;
        for c in members.iter_mut() {
            let _ = delonix_runtime::reconcile_status(c);
            if matches!(c.status, Status::Running | Status::Paused) {
                running += 1;
            }
        }
        let ip = infra::container_ip(&pod_netns_name(&pod));
        let status = if running == members.len() {
            "Running".to_string()
        } else if running == 0 {
            "Stopped".to_string()
        } else {
            "Degraded".to_string()
        };
        t.row(vec![
            pod,
            format!("{running}/{}", members.len()),
            if ip.is_empty() { "-".to_string() } else { ip },
            status,
        ]);
    }
    t.print();
    Ok(())
}

fn describe(names: &[String]) -> Result<()> {
    let (_images, store) = open_stores()?;
    for name in names {
        let mut members = members_of(&store, name)?;
        if members.is_empty() {
            return Err(Error::Invalid(format!(
                "no such pod: {name} (see `delonix pod ls`)"
            )));
        }
        let mut d = output::Describe::new();
        d.field("Pod", name);
        d.field("Namespace", &members[0].namespace);
        d.field("IP", infra::container_ip(&pod_netns_name(name)));
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

fn logs(pod: &str, container_short: Option<&str>, follow: bool) -> Result<()> {
    let (images, store) = open_stores()?;
    let members = members_of(&store, pod)?;
    if members.is_empty() {
        return Err(Error::Invalid(format!(
            "no such pod: {pod} (see `delonix pod ls`)"
        )));
    }
    let target = match container_short {
        Some(short) => members
            .iter()
            .find(|c| c.name == format!("{pod}-{short}"))
            .ok_or_else(|| Error::Invalid(format!("pod '{pod}' has no container '{short}'")))?,
        None => &members[0],
    };
    container::cmd_logs(&images, &store, &target.name, follow)
}
