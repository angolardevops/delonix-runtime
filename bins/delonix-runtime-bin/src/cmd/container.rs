//! `delonix container` — container lifecycle (run/ps/stop/rm/exec/logs).

use super::kinds as k;
use std::path::PathBuf;

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_image::ImageStore;
use delonix_net::infra;
use delonix_runtime::{self as runtime, RunSpec};
use delonix_runtime_core::{
    generate_id, Container, Error, Health, HealthConfig, HealthState, Result, Status, Store,
};
use serde::{Deserialize, Serialize};

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::{container_writable_dir, find, open_stores, resolve_or_pull};
use delonix_net::run_network::{publish_with_retry, unpublish_ports};

/// `spec` for `kind: Container` — mirrors `ContainerCmd::Run` (minus `name`,
/// which comes from `metadata.name`). **`detach` defaults to `true`** (unlike the
/// CLI, where the default is `false`): an `apply`/`stack apply` run in the
/// foreground would block waiting for the process to exit — dangerous for a
/// declarative command. Pass `detach: false` explicitly in the YAML if you want
/// the synchronous behavior of the interactive `run`.
/// Manifest mirror of [`delonix_runtime_core::CgroupParent`].
///
/// It exists here, and not in `delonix-runtime-core`, for two reasons: the core crate
/// stays free of `schemars` (the manifest schema is a concern of this binary, not of
/// the domain types), and the manifest speaks camelCase (`memoryMax`) while the
/// persisted `Container` keeps its own field names. One conversion at the boundary is
/// cheaper than either a new dependency in core or a rename that breaks stored records.
#[derive(Debug, Deserialize, Serialize, Clone, schemars::JsonSchema)]
pub(crate) struct SpecCgroupParent {
    /// Group directory name — a single, safe path segment.
    pub(crate) name: String,
    /// Aggregate memory ceiling for the whole group (e.g. `1073741824`).
    #[serde(default, rename = "memoryMax", skip_serializing_if = "Option::is_none")]
    pub(crate) memory_max: Option<String>,
    /// Aggregate CPU in cores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cpus: Option<String>,
    /// Aggregate process ceiling.
    #[serde(default, rename = "pidsMax", skip_serializing_if = "Option::is_none")]
    pub(crate) pids_max: Option<String>,
}

impl From<SpecCgroupParent> for delonix_runtime_core::CgroupParent {
    fn from(s: SpecCgroupParent) -> Self {
        delonix_runtime_core::CgroupParent {
            name: s.name,
            memory_max: s.memory_max,
            cpus: s.cpus,
            pids_max: s.pids_max,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct ContainerSpec {
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
    /// field name is `restartPolicy` (uniform with `kind: VirtualMachine`); the legacy
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
    /// Intermediate cgroup shared by a GROUP of containers, with its own aggregate
    /// ceiling — the only way to bound what N containers hold TOGETHER (see
    /// `delonix_runtime_core::CgroupParent`).
    #[serde(default, rename = "cgroupParent")]
    cgroup_parent: Option<SpecCgroupParent>,
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
    #[serde(default, rename = "addHost")]
    add_host: Vec<String>,
    #[serde(default)]
    knows: Vec<String>,
    #[serde(default, rename = "netBps")]
    net_bps: Option<String>,
    #[serde(default, rename = "netBurst")]
    net_burst: Option<String>,
    #[serde(default, rename = "logDriver")]
    log_driver: Option<String>,
}

// ---------------------------------------------------------------------------
// Declarative reconciliation — the comparable form of a container
// ---------------------------------------------------------------------------

/// The container fields the reconciler compares, and NOTHING else.
///
/// The set is deliberately conservative, because the failure mode of getting it
/// wrong is worse than the gap of leaving a field out: a field whose two sides
/// normalize differently shows as a difference on EVERY plan, forever, and a
/// plan that always reports drift is worth less than no plan at all. Each entry
/// below has a test proving that an unchanged manifest yields no diff.
///
/// **Deliberately NOT compared, with the reason** (documented, never silent —
/// `stack plan` prints this list on request):
///
/// - `env` — the record holds the IMAGE's environment merged with the user's
///   (`c.env = img.config.env` then the `-e` values appended). Comparing it
///   against `spec.env` would report every image variable as an addition.
/// - `command` / `entrypoint` — `compose_command` folds the image's
///   ENTRYPOINT/CMD into the stored command, so the record is not what the
///   manifest said even when nothing changed.
/// - `user` — the manifest says `app`, the record stores the resolved
///   `run_uid`/`run_gid`; mapping back needs the image's `/etc/passwd`.
/// - `labels` — the engine adds its own (`delonix.io/stack`, pod membership,
///   compose project), so the record is a superset by construction.
///
/// These are gaps in coverage, not in honesty: an `image` change is caught, and
/// that is the change that matters most. Closing them means normalizing both
/// sides through the same function, which is its own piece of work.
pub(crate) const RECONCILED_CONTAINER_FIELDS: &[&str] = &[
    "image",
    "ports",
    "volumes",
    "memory",
    "cpus",
    "privileged",
    "restartPolicy",
    "network",
    "hostname",
    // These two were in `hot_fields(k::CONTAINER)` and in `converge`, and NOT
    // here — so `diff_fields` (which iterates desired ∪ actual ∪ last) never saw
    // the key, the two `converge` arms were unreachable, and changing `netBps`
    // in an applied manifest was a no-op that `stack plan` reported as «no
    // changes» with `--detailed-exitcode` 0. A drift gate in CI passed over real
    // drift. Found in adversarial review; `hot_fields_sao_um_subconjunto_dos_comparados`
    // now makes the three lists impossible to leave disagreeing.
    "netBps",
    "netBurst",
];

/// Os campos que o manifesto declara e o reconciliador NÃO compara — nomeados,
/// num container que já existe, em vez de descartados em silêncio.
///
/// **O irmão do `vm::unconverged_fields_condition`, e a mesma medição por trás.**
/// Um `kind: Container` aceita 43 campos de spec e o `RECONCILED_CONTAINER_FIELDS`
/// tem onze. Num container que ainda não existe isso é inofensivo — a criação
/// aplica o spec inteiro. Num container que JÁ CORRE era um descarte mudo: mudar
/// `env`, `user`, `capAdd`, `readOnly`, `securityOpt`, `sysctl`, `devices` ou
/// `command` num manifesto aplicado dava `Summary: no changes` e
/// `--detailed-exitcode` **0**, ou seja um gate de deriva em CI verde por cima de
/// deriva verdadeira. É o defeito estrutural que a v0.47.0 existiu para fechar,
/// ainda vivo no Kind mais usado — e o comentário do `netBps`/`netBurst` acima
/// mostra que a casa já pagou esta classe uma vez.
///
/// **Nomear não é convergir, e a diferença é deliberada.** Convergir `capAdd` ou
/// `readOnly` obriga a recriar o container, que é uma capacidade com o seu próprio
/// desenho (e o `-/+` é fail-closed, exige `--replace`); dizer que não se converge
/// custa uma frase. O motor entrega primeiro a honestidade — a mesma ordem que o
/// `Vm` seguiu.
///
/// **Derivado do `RECONCILED_CONTAINER_FIELDS`, nunca uma segunda lista.** Um
/// campo acrescentado ao conjunto comparado sai deste aviso sozinho. Duas listas
/// que têm de concordar é como este repo já partiu o `CONVERGING_KINDS` uma vez.
pub(crate) fn unconverged_fields_condition(
    doc: &ManifestDoc,
) -> Option<super::conditions::Condition> {
    let mapping = doc.spec.as_mapping()?;
    let mut fields: Vec<String> = mapping
        .keys()
        .filter_map(|k| k.as_str())
        .filter(|k| !RECONCILED_CONTAINER_FIELDS.contains(k))
        // `detach` não é estado do recurso, é o modo de invocação de quem cria —
        // um container a correr não «tem» um detach para divergir. Listá-lo faria
        // TODOS os manifestos avisarem, e um aviso que sai sempre deixa de se ler.
        .filter(|k| *k != "detach")
        .map(|k| k.to_string())
        .collect();
    if fields.is_empty() {
        return None;
    }
    fields.sort();
    Some(super::conditions::Condition::bad(
        "Converged",
        "FieldsNotCompared",
        super::po::tf(
            "declared but NOT applied to an existing container: {fields} — the reconciler compares only {compared}. Recreate it (`--replace Container/<name>`) or change it with `container update` where that field is hot",
            &[
                ("fields", &fields.join(", ")),
                ("compared", &RECONCILED_CONTAINER_FIELDS.join(", ")),
            ],
        ),
    ))
}

/// Renders a persisted [`Mount`] back into the `source:/target[:ro]` form the
/// manifest uses, so the two sides of a diff are comparable.
///
/// Cannot use `VolumeStore::resolve_spec` for this: that function **creates the
/// volume on demand** (Docker semantics, and correct there), and `plan` is
/// read-only — computing a plan must never bring a resource into existence.
/// So the mapping is done here, in the opposite direction and without I/O.
///
/// A named volume lives at `<root>/volumes/<name>/_data`; anything else is a
/// bind and keeps its host path.
pub(crate) fn mount_to_spec(
    m: &delonix_runtime_core::Mount,
    volumes_root: &std::path::Path,
) -> String {
    let source = std::path::Path::new(&m.source)
        .strip_prefix(volumes_root)
        .ok()
        .and_then(|rest| {
            // `<name>/_data` — and only that shape, so a bind that merely
            // happens to sit under the volumes root is not mistaken for a
            // named volume.
            let mut it = rest.components();
            let name = it.next()?.as_os_str().to_str()?.to_string();
            match it.next()?.as_os_str().to_str()? {
                "_data" if it.next().is_none() => Some(name),
                _ => None,
            }
        })
        .unwrap_or_else(|| m.source.clone());
    let ro = if m.readonly { ":ro" } else { "" };
    format!("{source}:{}{ro}", m.target)
}

/// The manifest side of the same form. Binds are canonicalized (read-only, and
/// it is what the engine stores) so `./data` and `/abs/data` compare equal;
/// a path that does not exist yet keeps its literal spelling rather than
/// failing — the container does not exist yet either, so the diff is moot.
fn volume_spec_key(spec: &str) -> String {
    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() < 2 {
        return spec.to_string();
    }
    let (src, target) = (parts[0], parts[1]);
    let ro = if parts.get(2) == Some(&"ro") {
        ":ro"
    } else {
        ""
    };
    let src = if src.starts_with('/') || src.starts_with('.') {
        std::fs::canonicalize(src)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| src.to_string())
    } else {
        src.to_string()
    };
    format!("{src}:{target}{ro}")
}

/// A `Vec<String>` rendered as one comparable value. Sorted, because the ORDER
/// of ports and volumes carries no meaning — leaving it unsorted would report a
/// reordered manifest as a change and, worse, as a `Replace`.
fn list_key(mut items: Vec<String>) -> String {
    items.sort();
    items.join(",")
}

/// What the manifest asks for, in comparable form.
pub(crate) fn desired_container_fields(
    spec: &ContainerSpec,
) -> std::collections::BTreeMap<String, String> {
    let mut f = std::collections::BTreeMap::new();
    f.insert("image".into(), spec.image.clone());
    // Ranges are expanded at the boundary by `run`, so the record holds
    // single ports; expand here too or `8000-8001:80-81` would diff against
    // its own expansion forever.
    let ports: Vec<String> = spec
        .ports
        .iter()
        .flat_map(|p| delonix_net::expand_publish_range(p).unwrap_or_else(|_| vec![p.clone()]))
        .collect();
    f.insert("ports".into(), list_key(ports));
    f.insert(
        "volumes".into(),
        list_key(spec.volumes.iter().map(|v| volume_spec_key(v)).collect()),
    );
    // Undeclared limits are DERIVED (a quarter of the engine's budget), never
    // absent — so the desired state matches what `cmd_run` will really apply.
    // These two used to be `"max"`/`"1.0"`; a literal here and a derived value
    // there would make every such container read as drifted forever.
    f.insert(
        "memory".into(),
        spec.memory
            .clone()
            .unwrap_or_else(runtime::default_memory_max),
    );
    f.insert(
        "cpus".into(),
        spec.cpus.clone().unwrap_or_else(runtime::default_cpus),
    );
    f.insert("privileged".into(), spec.privileged.to_string());
    f.insert("restartPolicy".into(), spec.restart.clone());
    f.insert("network".into(), spec.network.clone());
    if let Some(h) = &spec.hostname {
        f.insert("hostname".into(), h.clone());
    }
    // Only emitted when the manifest names them: an absent `netBps` means "no
    // shaping asked for", not "shaping of zero", and emitting a default here
    // would report drift on every plan for every container that never used it.
    if let Some(b) = &spec.net_bps {
        f.insert("netBps".into(), b.clone());
    }
    if let Some(b) = &spec.net_burst {
        f.insert("netBurst".into(), b.clone());
    }
    f
}

/// The same comparable form, built from the already-normalized [`RunOpts`].
///
/// Used by the Pod-shaped `kind: Container` (`spec.containers[]`), which reaches
/// `RunOpts` through `pod_to_run_opts` — the very function the apply path calls.
/// Deriving the diff from the same normalization is what stops the two manifest
/// shapes from disagreeing about what they asked for.
fn desired_fields_from_run_opts(o: &RunOpts) -> std::collections::BTreeMap<String, String> {
    let mut f = std::collections::BTreeMap::new();
    f.insert("image".into(), o.image.clone());
    let ports: Vec<String> = o
        .ports
        .iter()
        .flat_map(|p| delonix_net::expand_publish_range(p).unwrap_or_else(|_| vec![p.clone()]))
        .collect();
    f.insert("ports".into(), list_key(ports));
    f.insert(
        "volumes".into(),
        list_key(o.volumes.iter().map(|v| volume_spec_key(v)).collect()),
    );
    f.insert(
        "memory".into(),
        o.memory.clone().unwrap_or_else(runtime::default_memory_max),
    );
    f.insert(
        "cpus".into(),
        o.cpus.clone().unwrap_or_else(runtime::default_cpus),
    );
    f.insert("privileged".into(), o.privileged.to_string());
    f.insert("restartPolicy".into(), o.restart.clone());
    f.insert("network".into(), o.net.clone());
    if let Some(h) = &o.hostname {
        f.insert("hostname".into(), h.clone());
    }
    f
}

/// What the machine actually has, in the same comparable form.
pub(crate) fn actual_container_fields(
    c: &delonix_runtime_core::Container,
    volumes_root: &std::path::Path,
) -> std::collections::BTreeMap<String, String> {
    let mut f = std::collections::BTreeMap::new();
    f.insert("image".into(), c.image.clone());
    f.insert("ports".into(), list_key(c.ports.clone()));
    f.insert(
        "volumes".into(),
        list_key(
            c.mounts
                .iter()
                .map(|m| mount_to_spec(m, volumes_root))
                .collect(),
        ),
    );
    f.insert("memory".into(), c.memory_max.clone());
    f.insert("cpus".into(), c.cpus.clone());
    f.insert("privileged".into(), c.privileged.to_string());
    f.insert(
        "restartPolicy".into(),
        c.restart_policy.clone().unwrap_or_else(|| "no".into()),
    );
    // `net_mode` records the INTENT (`host`/`none`/`<network>`), which is what
    // the manifest states. `network` alone cannot: it is `None` for both `host`
    // and a degraded attach, a conflation this record already had to fix once.
    f.insert(
        "network".into(),
        c.net_mode
            .clone()
            .or_else(|| c.network.clone())
            .unwrap_or_else(|| "host".into()),
    );
    if let Some(h) = &c.hostname {
        f.insert("hostname".into(), h.clone());
    }
    // Mirrors `desired_container_fields`: absent means no shaping, and the two
    // sides have to agree on that or every unshaped container reports drift.
    if let Some(b) = &c.net_bps {
        f.insert("netBps".into(), b.clone());
    }
    if let Some(b) = &c.net_burst {
        f.insert("netBurst".into(), b.clone());
    }
    f
}

/// Destroys a container so the normal creation path can rebuild it.
///
/// `--force`, deliberately: a resource marked for recreation was ALREADY
/// authorized by the user with `--replace`, and stopping halfway because the
/// container happens to be running would leave the stack in the one state this
/// command exists to avoid — half converged, with the refusal arriving after
/// the destruction of everything before it.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    let (images, store) = open_stores()?;
    cmd_rm(&images, &store, name, true)
}

/// The HOST port of a publish spec, which is what `--publish-rm` takes.
///
/// Three fields means the Docker `hostIp:hostPort:contPort` form, where the host
/// port is the SECOND — taking the first would hand `--publish-rm` an IP
/// address, and the unpublish would silently match nothing. That form is exactly
/// the one the compose path already got wrong once (`127.0.0.1:9000:80` was read
/// as `hostPort:contPort` and published on every interface).
fn host_port_of(spec: &str) -> String {
    let bare = spec.split('/').next().unwrap_or(spec);
    let parts: Vec<&str> = bare.split(':').collect();
    match parts.len() {
        3 => parts[1].to_string(),
        _ => parts.first().unwrap_or(&bare).to_string(),
    }
}

/// Applies the hot part of a plan to a live container — **without changing the
/// PID**.
///
/// Delegates to [`cmd_update`], the very code path `container update` uses, so
/// the declarative and imperative routes cannot drift apart. That function had
/// been in the tree, tested, with a caller, for versions; the declarative side
/// simply never called it, which is why changing a manifest used to do nothing.
///
/// Only fields marked hot in `reconcile::hot_fields` reach here — anything else
/// has already been classified as a `Replace` and refused unless asked for.
pub(crate) fn converge(name: &str, diffs: &[super::reconcile::FieldDiff]) -> Result<()> {
    let (_, store) = open_stores()?;
    let mut o = UpdateOpts::default();
    for d in diffs {
        let (removed, added) = super::reconcile::list_delta(d.from.as_deref(), d.to.as_deref());
        match d.field.as_str() {
            "ports" => {
                // `--publish-rm` takes the HOST port, not the whole spec.
                o.publish_rm.extend(removed.iter().map(|p| host_port_of(p)));
                o.publish_add.extend(added);
            }
            "volumes" => {
                // `--volume-rm` takes the TARGET path inside the container.
                o.volume_rm.extend(
                    removed
                        .iter()
                        .filter_map(|v| v.split(':').nth(1).map(String::from)),
                );
                o.volume_add.extend(added);
            }
            "memory" => o.memory = d.to.clone(),
            "cpus" => o.cpus = d.to.clone(),
            "netBps" => match &d.to {
                Some(v) => o.net_rate = Some(v.clone()),
                // The manifest dropped the cap: clearing it is the revert, and
                // it is a different operation from setting it to some value.
                None => o.net_rate_clear = true,
            },
            "netBurst" => o.net_burst = d.to.clone(),
            other => {
                return Err(Error::Invalid(format!(
                    "container/{name}: '{other}' does not converge hot — this is a bug in \
                     `reconcile::hot_fields`, which promised something the executor cannot do"
                )))
            }
        }
    }
    cmd_update(&store, name, o)
}

/// Records that this stack owns the container, and what it last applied.
pub(crate) fn stamp(
    name: &str,
    stack: &str,
    fields: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let (_, store) = open_stores()?;
    let c = find(&store, name)?;
    let encoded = super::reconcile::encode_last_applied(fields);
    store.update(&c.id, |cur| {
        cur.labels
            .insert(super::reconcile::STACK_LABEL.into(), stack.to_string());
        cur.labels
            .insert(super::reconcile::MANAGED_BY.into(), "delonix".into());
        cur.annotations
            .insert(super::reconcile::LAST_APPLIED.into(), encoded.clone());
        true
    })?;
    Ok(())
}

/// What the manifest declares, for the reconciler.
///
/// The Pod-shaped form (`spec.containers[]`) is normalized through the SAME
/// `pod_to_run_opts` the apply path uses, so the two shapes cannot disagree
/// about what they asked for.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let fields = if doc.spec.get("containers").is_some() {
        let pod: PodSpec = manifest::spec_of(doc)?;
        let opts = pod_to_run_opts(&doc.metadata.name, doc.metadata.namespace.clone(), pod)?;
        desired_fields_from_run_opts(&opts)
    } else {
        desired_container_fields(&container_spec_of(doc)?)
    };
    Ok(super::reconcile::Desired {
        kind: k::CONTAINER.into(),
        name: doc.metadata.name.clone(),
        fields,
        converges: true,
        ownable: true,
    })
}

/// What is on the machine, for the reconciler.
pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    let (_, store) = open_stores()?;
    let volumes_root = super::util::state_root().join("volumes");
    Ok(store
        .list()?
        .into_iter()
        // A pod member is not a `kind: Container` — it belongs to its pod, and
        // listing it here would make every pod member look like an unmanaged
        // container the stack should adopt.
        .filter(|c| c.pod.is_none() && !c.labels.contains_key(super::pod::POD_LABEL))
        .map(|c| super::reconcile::Actual {
            kind: k::CONTAINER.into(),
            name: c.name.clone(),
            fields: actual_container_fields(&c, &volumes_root),
            owner: c.labels.get(super::reconcile::STACK_LABEL).cloned(),
            last_applied: c
                .annotations
                .get(super::reconcile::LAST_APPLIED)
                .and_then(|raw| super::reconcile::decode_last_applied(raw)),
        })
        .collect())
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
    "cgroupParent",
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
    "addHost",
    // Grouped-form-only keys (see `normalize_container_spec`) — `network`
    // and `env` need no entry of their own: they're ALREADY above, reused
    // for both shapes (a scalar/array in the old flat form, a mapping in
    // the new grouped one).
    "resources",
    "security",
    "storage",
    "limits",
];

/// Re-deserializes a FLAT `kind: Container` document's spec, accepting BOTH
/// the historic flat shape (every field at the top level — still fully
/// supported) and a newer GROUPED one (`resources:`/`network:`/`security:`/
/// `storage:`/`env:`/`limits:`), by hoisting each group's sub-fields to
/// their flat top-level name on the raw YAML `Value` BEFORE the
/// strongly-typed `ContainerSpec` (unchanged) ever sees it — same pattern as
/// `cmd::vm::vm_spec_of`/`normalize_vm_spec`, see its doc for the reasoning.
/// Only the FLAT spec gets this treatment — the Pod-shaped form
/// (`spec.containers[]`) already mirrors k8s's own grouping
/// (`resources.limits`/`securityContext`/...), nothing to improve there.
fn container_spec_of(doc: &ManifestDoc) -> Result<ContainerSpec> {
    let normalized = normalize_container_spec(doc.spec.clone());
    serde_yaml::from_value(normalized).map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::tf(
                "{kind} '{name}': invalid spec",
                &[("kind", &doc.kind), ("name", &doc.metadata.name)],
            )
        ))
    })
}

/// The grouped form's tables: group name -> (sub-key, flat field it becomes).
/// ONE source for both the hoist and the unknown-sub-key check — two copies
/// would drift, silently, in exactly the way that check exists to stop.
const CONTAINER_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "resources",
        &[
            ("memory", "memory"),
            ("cpus", "cpus"),
            ("cpuWeight", "cpuWeight"),
            ("cpuset", "cpuset"),
            ("cgroupParent", "cgroupParent"),
            ("ioWeight", "ioWeight"),
        ],
    ),
    (
        "security",
        &[
            ("privileged", "privileged"),
            ("readOnly", "readOnly"),
            ("capAdd", "capAdd"),
            ("capDrop", "capDrop"),
            ("securityOpt", "securityOpt"),
            ("apparmor", "apparmor"),
            ("selinux", "selinux"),
            ("userns", "userns"),
            ("hostPid", "hostPid"),
            ("hostIpc", "hostIpc"),
            ("detect", "detect"),
        ],
    ),
    ("storage", &[("volumes", "volumes"), ("tmpfs", "tmpfs")]),
    (
        "limits",
        &[
            ("ulimit", "ulimit"),
            ("sysctl", "sysctl"),
            ("gpus", "gpus"),
            ("devices", "devices"),
        ],
    ),
];

/// Sub-keys accepted inside the grouped `network:` mapping.
const CONTAINER_NETWORK_KEYS: &[&str] = &[
    "name",
    "ports",
    "expose",
    "alias",
    "knows",
    "rateBps",
    "rateBurst",
];

/// Sub-keys of the GROUPED `env:` form. A plain `KEY: value` mapping is the
/// other accepted shape and every key in it is the user's own variable, so it
/// is never checked against this list.
const CONTAINER_ENV_KEYS: &[&str] = &["vars", "files", "secrets", "secretFiles"];

/// Sub-keys inside a grouped spec that the hoist does not know — and therefore
/// throws away.
///
/// Pure, and reading the spec BEFORE `normalize_container_spec` touches it.
/// Returns dotted paths (`resources.memoria`) so the message names the exact
/// line to fix.
///
/// Two shapes are deliberately NOT checked, because in them every key is the
/// user's own data rather than a field name: a `network:` that is a plain string
/// (the flat form) and an `env:` that is a plain `KEY: value` mapping. `env` is
/// only checked when it is the grouped form, which is what having one of
/// `vars`/`files`/`secrets`/`secretFiles` means — the same test the normalizer
/// itself makes.
pub(crate) fn unknown_group_keys(spec: &serde_yaml::Value) -> Vec<String> {
    use serde_yaml::Value;
    let Value::Mapping(m) = spec else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut scan = |group: &str, known: &dyn Fn(&str) -> bool| {
        if let Some(Value::Mapping(g)) = m.get(group) {
            for k in g.keys().filter_map(|k| k.as_str()) {
                if !known(k) {
                    out.push(format!("{group}.{k}"));
                }
            }
        }
    };
    scan("network", &|k| CONTAINER_NETWORK_KEYS.contains(&k));
    if let Some(Value::Mapping(env)) = m.get("env") {
        if CONTAINER_ENV_KEYS
            .iter()
            .any(|k| env.get(Value::from(*k)).is_some())
        {
            scan("env", &|k| CONTAINER_ENV_KEYS.contains(&k));
        }
    }
    for (group, pairs) in CONTAINER_GROUPS {
        scan(group, &|k| pairs.iter().any(|(from, _)| *from == k));
    }
    out
}

/// Hoists each recognized group's sub-fields to their flat top-level name.
/// Pure, testable independently of serde — see `cmd::vm::normalize_vm_spec`
/// for the general pattern this mirrors, including the precedence rule (an
/// explicit flat key always wins over a grouped one of the same target).
///
/// `network` and `env` are the two special cases: their OLD flat forms are a
/// SCALAR (`network: host`) and a SEQUENCE (`env: ["K=v"]`) respectively —
/// the NEW grouped forms are MAPPINGS, so the two are told apart by the YAML
/// node's own type, same trick as `network` in `normalize_vm_spec`.
fn normalize_container_spec(mut v: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value;
    let Value::Mapping(m) = &mut v else {
        return v;
    };

    if let Some(Value::Mapping(net)) = m.get("network").cloned() {
        m.remove("network");
        if let Some(name) = net.get("name") {
            m.insert(Value::from("network"), name.clone());
        }
        hoist(m, &net, "ports", "ports");
        hoist(m, &net, "expose", "expose");
        hoist(m, &net, "alias", "networkAlias");
        hoist(m, &net, "knows", "knows");
        hoist(m, &net, "rateBps", "netBps");
        hoist(m, &net, "rateBurst", "netBurst");
    }
    if let Some(Value::Mapping(env)) = m.get("env").cloned() {
        // Two different mappings can appear under `env`, and telling them apart
        // matters: the GROUPED form (`vars`/`files`/`secrets`/`secretFiles`) and
        // a plain `KEY: value` map, which is what anyone coming from compose or
        // k8s writes.
        //
        // BUG THIS FIXES: only the grouped form was handled, and the `env` key
        // was removed BEFORE checking — so `env: { POSTGRES_PASSWORD: dev }` was
        // accepted and silently dropped. It shipped that way in
        // `examples/dependency.yaml`, where it meant the example's Postgres came
        // up with no password variable at all. Accepted-then-ignored is the
        // failure mode this repo has removed everywhere else; it survived here
        // because nothing ever compared the parsed spec against the file.
        let grouped = ["vars", "files", "secrets", "secretFiles"]
            .iter()
            .any(|k| env.get(*k).is_some());
        m.remove("env");
        if grouped {
            if let Some(vars) = env.get("vars") {
                m.insert(Value::from("env"), vars.clone());
            }
            hoist(m, &env, "files", "envFile");
            hoist(m, &env, "secrets", "secret");
            hoist(m, &env, "secretFiles", "secretFiles");
        } else {
            // `{K: v}` → `["K=v"]`, the flat form the engine already speaks.
            let vars: Vec<Value> = env
                .iter()
                .map(|(k, v)| {
                    let key = k.as_str().unwrap_or_default();
                    let val = match v {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => serde_yaml::to_string(other)
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                    };
                    Value::from(format!("{key}={val}"))
                })
                .collect();
            m.insert(Value::from("env"), Value::Sequence(vars));
        }
    }
    for (group, pairs) in CONTAINER_GROUPS {
        if let Some(Value::Mapping(g)) = m.get(*group).cloned() {
            for (from, to) in pairs.iter() {
                hoist(m, &g, from, to);
            }
            m.remove(*group);
        }
    }
    v
}

fn hoist(m: &mut serde_yaml::Mapping, group: &serde_yaml::Mapping, from: &str, to: &str) {
    if m.contains_key(to) {
        return;
    }
    if let Some(val) = group.get(from) {
        m.insert(serde_yaml::Value::from(to), val.clone());
    }
}

use delonix_compute::pod::{default_net, default_restart, default_true};
/// `host`/`none` are the two built-in networks (no user bridge); anything else
/// is the name of a custom `delonix network` to attach to.
/// Como mostrar o modo de rede, cruzando a INTENÇÃO com o RESULTADO.
///
/// Puro e testado de propósito: é a única coisa que separa uma degradação de
/// uma escolha deliberada, e uma regressão aqui volta a torná-la invisível.
pub(crate) fn net_mode_display(c: &delonix_runtime_core::Container) -> String {
    match (c.net_mode.as_deref(), c.network.as_deref()) {
        // Ligado à rede que pediu — o caso normal.
        (_, Some(net)) => net.to_string(),
        // Pediu explicitamente host/none: não há nada de errado.
        (Some("host"), None) => "host".to_string(),
        (Some("none"), None) => "none".to_string(),
        // Pediu uma REDE e não a tem. É o caso que estava mudo.
        (Some(asked), None) => format!("host (degraded: asked for '{asked}')"),
        // Registo anterior a este campo: não se inventa intenção.
        (None, None) => "host".to_string(),
    }
}

pub(crate) use delonix_compute::pod::parse_add_host;

use delonix_compute::preflight::custom_net_name;

/// Whitelist for a container's name: alnum + `-`/`_`, non-empty, doesn't
/// start with `-`. Deliberately excludes `.` (unlike `delonix_vm::
/// valid_vm_name`, which allows it) — see the call site for why: a dotted
/// container name is indistinguishable from an external FQDN to the DNS
/// resolver's whole-name match, letting it hijack that domain node-wide.
fn valid_container_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// The Pod-shaped `kind: Container` types and their translators live in the
// Compute context (`delonix_compute::pod`). These wrappers keep the names the
// rest of the CLI calls, and print the translators' warnings in the operator's
// language — the context returns them as data and never prints.
#[cfg(test)]
use delonix_compute::pod::HostAlias;
pub(crate) use delonix_compute::pod::{PodSpec, POD_SPEC_FIELDS};

fn print_notices(notices: &[delonix_compute::Notice]) {
    for n in notices {
        let args: Vec<(&str, &str)> = n.args.iter().map(|(k, v)| (*k, v.as_str())).collect();
        eprintln!("{}", super::po::tf(n.template, &args));
    }
}

fn pod_to_run_opts(name: &str, namespace: Option<String>, pod: PodSpec) -> Result<RunOpts> {
    let mut notices = Vec::new();
    let out = delonix_compute::pod::pod_to_run_opts(name, namespace, pod, &mut notices);
    print_notices(&notices);
    out
}

pub(crate) fn pod_member_run_opts(
    name: &str,
    namespace: Option<String>,
    spec: PodSpec,
    netns: &str,
) -> Result<Vec<RunOpts>> {
    let mut notices = Vec::new();
    let out = delonix_compute::pod::pod_member_run_opts(name, namespace, spec, netns, &mut notices);
    print_notices(&notices);
    out
}

// FIXME(follow-up): variants with a large size disparity (≥880 B). Boxing the
// fat variants is a real optimization but awkward with clap's `#[derive(Subcommand)]`
// — left for a dedicated change; the cost here is a short-lived CLI.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum ContainerCmd {
    /// Initialize a project with a Delonixfile + manifest.
    ///
    /// Files ALREADY FILLED IN (images included), ready to use without editing
    /// anything.
    Init {
        /// Project directory (default: the current one).
        #[arg(value_hint = clap::ValueHint::DirPath, default_value = ".")]
        dir: PathBuf,
        /// Project name (default: the directory name).
        #[arg(long)]
        name: Option<String>,
        /// Image to use. Omit = fill in with the default image.
        #[arg(long, add = ArgValueCandidates::new(super::complete::images))]
        image: Option<String>,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
        /// Generate a complete PROJECT for a stack (e.g. `python`) with best
        /// practices, instead of the generic scaffold. `--template list` shows the available ones.
        #[arg(long, short = 't')]
        template: Option<String>,
        /// Version parameter some templates read — an image tag, a framework
        /// version, or a toolchain version, depending on the template; the
        /// exact accepted form is documented in that template's own README.
        /// Refused with a clear error on a template that has none.
        #[arg(short = 'v', long = "template-version")]
        template_version: Option<String>,
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
        #[arg(long, add = ArgValueCandidates::new(super::complete::namespaces))]
        namespace: Option<String>,
        /// Auto-register this container's HTTP port in the L7 proxy under its internal
        /// FQDN `<name>.<namespace>.delonix.internal` (reachable via the proxy). Needs
        /// `--net <network>`. Removed automatically on `container rm`.
        #[arg(long)]
        expose: Option<u16>,
        /// Volume/bind mount, `name:/target[:ro]` or `/host:/target[:ro]`. Repeatable.
        #[arg(short = 'v', long = "volume")]
        volumes: Vec<String>,
        /// Publish a port, `[hostIp:]hostPort:contPort[/tcp|udp]` or just `port`. Repeatable.
        /// SAFE BY DEFAULT: without `hostIp` the port binds to `127.0.0.1` only — reachable
        /// from the host itself, NOT from a browser on another machine. Name the address to
        /// widen it: `0.0.0.0:8080:80` (every interface), `192.168.1.10:8080:80` (one), or the
        /// libvirt gateway to reach it from VMs (see `delonix vm reach`).
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
        /// Working directory the container's process starts in (default: the
        /// image's own configured workdir, or `/`). Persists in the record — an
        /// `exec -w` overrides it for that one call only.
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
        /// Remove the container when the process exits (with `-d`, a detached
        /// watcher handles removal when the container dies).
        #[arg(long)]
        rm: bool,
        /// Restart policy (only with `-d`): `no` (default), `on-failure[:max]`,
        /// `always`, `unless-stopped`. A detached supervisor (one per container,
        /// ephemeral — there's no daemon) becomes the container's parent, captures
        /// the real exit code, and restarts it according to the policy.
        #[arg(long, default_value = "no", value_parser = parse_restart_policy)]
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
        /// Memory limit (`64M`, `2G`, `max`). Default: a quarter of the engine's
        /// budget (`DELONIX_DEFAULT_PCT`); `max` lifts the cap entirely.
        #[arg(short = 'm', long)]
        memory: Option<String>,
        /// CPU quota (number of cores, e.g. `0.5`, `2`). Default: the lesser of
        /// 1.0 and a quarter of the engine's budget.
        #[arg(short = 'c', long)]
        cpus: Option<String>,
        /// Relative CPU weight (`cpu.weight`, 1–10000) under contention.
        #[arg(long = "cpu-weight", value_parser = parse_cgroup_weight)]
        cpu_weight: Option<String>,
        /// CPUs the container is pinned to (`cpuset.cpus`, e.g. `0-3`, `0,2`).
        #[arg(long)]
        cpuset: Option<String>,
        /// Internal: the kubelet's cgroup for this pod (CRI `cgroup_parent`, ADR 0038).
        /// Unspecified limits mean no limit under it.
        #[arg(long = "kube-cgroup-parent", hide = true)]
        kube_cgroup_parent: Option<String>,
        /// Relative I/O weight (`io.weight`, 1–10000).
        #[arg(long = "io-weight", value_parser = parse_cgroup_weight)]
        io_weight: Option<String>,
        /// Absolute read limit from the store's disk (`10mb`, `1g`). Docker's `--device-read-bps`.
        #[arg(long = "device-read-bps")]
        device_read_bps: Option<String>,
        /// Absolute write limit to the store's disk (`10mb`, `1g`). Docker's `--device-write-bps`.
        #[arg(long = "device-write-bps")]
        device_write_bps: Option<String>,
        /// Absolute read IOPS limit from the store's disk.
        #[arg(long = "device-read-iops")]
        device_read_iops: Option<String>,
        /// Absolute write IOPS limit to the store's disk.
        #[arg(long = "device-write-iops")]
        device_write_iops: Option<String>,
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
        /// Security options (docker-style), repeatable:
        /// `seccomp=unconfined` | `seccomp=<profile.json>` (OCI/runc format) |
        /// `apparmor=<profile>` | `no-new-privileges[=true|false]` (default true,
        /// stricter than docker/podman).
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
        #[arg(long, add = ArgValueCandidates::new(super::complete::secrets))]
        secret: Vec<String>,
        /// The `--secret`s come in as files in `/run/secrets/` (tmpfs), not env.
        #[arg(long = "secret-files")]
        secret_files: bool,
        /// Load variables from a `.env` file (`KEY=VAL` per line). Repeatable.
        #[arg(long = "env-file")]
        env_file: Vec<String>,
        /// (internal) `KEY=VAL` entries separated by NUL — the `/proc/<pid>/environ`
        /// format. Used by `delonix-cri`: values arrive byte-exact (newlines,
        /// spaces) and never on the argv. Applied before `-e`, which still wins.
        #[arg(long = "env-file0", hide = true)]
        env_file0: Vec<String>,
        // ---- fs & limits ----
        /// Mount a tmpfs (`/path[:options]`). Repeatable.
        #[arg(long)]
        tmpfs: Vec<String>,
        /// Ulimit (`nofile=1024:2048`). Repeatable.
        #[arg(long)]
        ulimit: Vec<String>,
        /// DNS server for the container's `/etc/resolv.conf`. Repeatable.
        /// Overrides the resolver the engine would pick (network gateway, slirp,
        /// or the host's copy).
        #[arg(long = "dns")]
        dns: Vec<String>,
        /// DNS search domain. Repeatable.
        #[arg(long = "dns-search")]
        dns_search: Vec<String>,
        /// `resolv.conf` option (`ndots:2`, `timeout:1`). Repeatable.
        #[arg(long = "dns-option")]
        dns_option: Vec<String>,
        /// Supplementary group id for the process (`--group-add 1234`).
        /// Repeatable. Applied even when the container runs as root — a root
        /// process still needs a group to reach a mounted share.
        #[arg(long = "group-add")]
        group_add: Vec<String>,
        /// Make a path unreadable inside the container (`--masked-path /proc/kcore`).
        /// Repeatable. A file is covered with `/dev/null`, a directory with an
        /// empty read-only tmpfs.
        #[arg(long = "masked-path")]
        masked_path: Vec<String>,
        /// Remount a path read-only inside the container (`--readonly-path /proc/sys`).
        /// Repeatable.
        #[arg(long = "readonly-path")]
        readonly_path: Vec<String>,
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
        /// Extra `/etc/hosts` entry, `name:ip` (as Docker/Podman). Repeatable.
        /// PERSISTED: survives `stop`/`start` and `restart`, which rewrite
        /// `/etc/hosts` from scratch.
        #[arg(long = "add-host")]
        add_host: Vec<String>,
        /// With `-d`, block until the image's `HEALTHCHECK` passes (or the
        /// timeout elapses). Replaces the `until curl ...; do sleep; done`
        /// that every script ends up writing. No `HEALTHCHECK` in the image
        /// is a clear error, never a silent instant return.
        #[arg(long = "wait")]
        wait_healthy: bool,
        /// How long `--wait` waits before giving up (seconds).
        #[arg(long = "wait-timeout", default_value_t = 60)]
        wait_timeout: u64,
        // ---- continuous health check ----
        /// Probe command, run with `/bin/sh -c` inside the container. Without
        /// it, the image's own `HEALTHCHECK` is monitored; any other
        /// `--health-*` flag turns monitoring on for an image that has one.
        #[arg(long = "health-cmd")]
        health_cmd: Option<String>,
        /// Seconds between probes.
        #[arg(long = "health-interval", default_value_t = 30)]
        health_interval: u64,
        /// A probe that runs longer than this counts as a failure. The probe
        /// kills ITSELF (the wrapper is `sh`), so nothing is left stuck inside
        /// the container.
        #[arg(long = "health-timeout", default_value_t = 30)]
        health_timeout: u64,
        /// Consecutive failures before the container is `unhealthy`.
        #[arg(long = "health-retries", default_value_t = 3)]
        health_retries: u32,
        /// Grace at startup: failures inside this window do not count, and the
        /// container reads `starting` rather than `unhealthy`.
        #[arg(long = "health-start-period", default_value_t = 0)]
        health_start_period: u64,
        /// Restrict DNS resolution to these containers (isolation). Repeatable.
        #[arg(long, add = ArgValueCandidates::new(super::complete::containers))]
        knows: Vec<String>,
        /// The container resolves NO other container by name.
        #[arg(long = "knows-none")]
        knows_none: bool,
        /// Join a pod's netns (`--net <network>`), sharing IP/ports.
        #[arg(long, add = ArgValueCandidates::new(super::complete::pods))]
        pod: Option<String>,
        /// (internal) init PID of the pod's infra container — join its IPC/UTS
        /// namespaces (shared pod IPC + hostname). Set by `delonix pod`.
        #[arg(long = "pod-infra-pid", hide = true)]
        pod_infra_pid: Option<i32>,
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
        /// Output format: `table` (default) or `json` (ADR-0005). `json` honors
        /// `--all` (same filter as the table) and ignores `--quiet`.
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: super::output::OutputFormat,
        /// Show only the containers of this isolation namespace. Omit to list every one.
        #[arg(short = 'n', long, add = ArgValueCandidates::new(super::complete::namespaces))]
        namespace: Option<String>,
        /// Show the SIZE column — each container's own writable-layer usage
        /// (never the shared read-only image layers). Off by default: it
        /// walks a directory tree per container, the same cost `volumes
        /// inspect` already opts into rather than pays on every listing.
        #[arg(short = 's', long)]
        size: bool,
    },
    /// (Re)start stopped/crashed containers. Always detached.
    ///
    /// Reuses the persistent rootfs (writes made inside the container
    /// survive, like in docker) and the same network/ports/volumes as the
    /// original `run`.
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
    /// Send a signal to one or more containers (default SIGKILL).
    ///
    /// Unlike `stop`, does not wait or force a `Stopped` status: the real
    /// outcome (e.g. `Crashed` for a `KILL`) is picked up on the next
    /// observation.
    Kill {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
        /// Signal name (`KILL`, `SIGKILL`, case-insensitive, `SIG` prefix
        /// optional) or number (`9`).
        #[arg(short = 's', long, default_value = "KILL")]
        signal: String,
    },
    /// Block until one or more containers exit, then print their exit code
    /// (one per line, in the order given).
    Wait {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
    },
    /// Stop then start one or more containers.
    ///
    /// Reuses the persistent rootfs and the original run configuration, like
    /// `start`.
    Restart {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
        /// Seconds until SIGKILL, for the `stop` half.
        #[arg(short, long, default_value_t = 10)]
        time: u64,
    },
    /// Give a container a new name.
    Rename {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        id: String,
        new_name: String,
    },
    /// Published ports of a container (`hostPort/proto -> containerPort`).
    Port {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        id: String,
    },
    /// Remove one or more containers.
    Rm {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
        /// Force (kill it if running).
        #[arg(short, long)]
        force: bool,
    },
    /// Remove every stopped container and the rootfs debris they left behind.
    ///
    /// Also sweeps what no `rm` will ever reach: **orphan rootfs directories**
    /// (containers killed by SIGKILL/crash with no registry entry left), empty
    /// cgroups, orphan ingress refs and host ports held by processes that are
    /// already gone. Images and volumes are untouched — see `image prune` and
    /// `volume prune`.
    Prune {
        /// Skip the confirmation prompt (REQUIRED when stdin is not a terminal).
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Suspend a container's processes (cgroup v2 freezer).
    ///
    /// The state stays in memory, unlike `stop`. Resume with `unpause`.
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
    /// Copy files between the host and a container.
    ///
    /// Exactly one side is `container:/path` (e.g. `delonix container cp
    /// web:/etc/nginx.conf .`).
    Cp { src: String, dst: String },
    /// Execute a command inside a running container.
    ///
    /// With no command, tries `bash` and falls back to `sh` (which exists in any image) — `container exec -it <id>` is the interactive shell, same as `container exec -it <id> bash` would give if `bash` is there.
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
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        id: String,
        /// Command + arguments. Omit it for the bash/sh fallback shell.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Show the full spec of one or more containers (Store JSON).
    Inspect {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
    },
    /// Human-readable detail of one or more containers, `kubectl
    /// describe`-style.
    ///
    /// For humans; use `inspect` for script-consumable JSON.
    Describe {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::containers))]
        ids: Vec<String>,
    },
    /// **Reconfigure a RUNNING container without stopping it** — ports,
    /// volumes, and bandwidth cap.
    ///
    /// Unlike docker (where changing a port or a volume forces recreating the
    /// container), here the dataplane doesn't belong to the process lifecycle:
    /// ports are DNAT/hostfwd in front of the network and volumes come in through
    /// the kernel's mount API (`open_tree`/`move_mount`) in the mount namespace of
    /// the already-live container. The PID doesn't change and the process is never
    /// interrupted.
    ///
    /// For which NETWORKS the container is on, see `network connect`/`network
    /// disconnect` — Docker's own verb for that, kept apart from ports/
    /// volumes/limits, which Docker cannot reconfigure hot at all.
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
        /// Bandwidth cap, in bit/s with a suffix (`10mbit`, `512kbit`, `1gbit`).
        #[arg(long = "net-rate", value_name = "RATE")]
        net_rate: Option<String>,
        /// Burst for the bandwidth cap (default: ~100 ms of throughput, at least 16 KiB). Only with `--net-rate`.
        #[arg(long = "net-burst", value_name = "BURST")]
        net_burst: Option<String>,
        /// Remove the bandwidth cap.
        #[arg(long = "net-rate-clear", conflicts_with = "net_rate")]
        net_rate_clear: bool,
        /// New memory limit hot (`64M`, `2G`, `max`).
        #[arg(short = 'm', long)]
        memory: Option<String>,
        /// New CPU quota hot (number of cores, e.g. `0.5`, `2`).
        #[arg(short = 'c', long)]
        cpus: Option<String>,
    },
    /// Resource usage (CPU/memory/PIDs) of the running containers.
    ///
    /// One sample and exits (no stream). With no IDs, shows all running ones.
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
        /// Show only the last N lines. Requires the container to have been run
        /// with `--log-cri` (per-line timestamps needed to find "the last N" —
        /// see `--timestamps`'s doc for why).
        #[arg(long)]
        tail: Option<usize>,
        /// Show only lines at or after this Unix timestamp (seconds). Same
        /// `--log-cri` requirement as `--tail`.
        #[arg(long)]
        since: Option<u64>,
        /// Prefix each line with its RFC3339 timestamp. Only available for
        /// containers run with `--log-cri` (`container run --log-cri`) — the
        /// plain log format is raw bytes with no per-line timestamp to show;
        /// a container without it gets a clear error naming the flag, not a
        /// silently-blank column.
        #[arg(long)]
        timestamps: bool,
    },
    /// Re-attach to a running container's output stream (output only).
    ///
    /// Same log file `logs -f` reads. Unlike `docker attach`, this is
    /// OUTPUT-ONLY: a detached container's stdin has nowhere to go (this
    /// engine keeps no live conduit to it once started, unlike a persistent
    /// per-container shim) — `-i`/`--stdin` is refused with a clear error
    /// instead of silently doing nothing.
    Attach {
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        id: String,
        /// Refused: stdin forwarding isn't supported (see the command's own doc above).
        #[arg(short, long)]
        interactive: bool,
    },
    /// Apply the `kind: Container` documents of a manifest (idempotent by name).
    ///
    /// An existing container with that name is neither recreated nor checked
    /// for spec drift, see `cmd::manifest`.
    Apply {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
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
        template_version,
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
            template_version,
            up,
        );
    }
    let (images, store) = open_stores()?;
    match action {
        // Handled at the top of `run` (returns early).
        ContainerCmd::Init { .. } => unreachable!("handled above"),
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
            workdir,
            rm,
            restart,
            devices,
            env,
            labels,
            memory,
            cpus,
            cpu_weight,
            cpuset,
            kube_cgroup_parent,
            io_weight,
            device_read_bps,
            device_write_bps,
            device_read_iops,
            device_write_iops,
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
            env_file0,
            tmpfs,
            ulimit,
            dns,
            dns_search,
            dns_option,
            group_add,
            masked_path,
            readonly_path,
            sysctl,
            gpus,
            ip,
            network_alias,
            add_host,
            wait_healthy,
            wait_timeout,
            health_cmd,
            health_interval,
            health_timeout,
            health_retries,
            health_start_period,
            knows,
            knows_none,
            pod,
            pod_infra_pid,
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
                workdir,
                rm,
                restart,
                devices,
                env: with_env_file0(&env_file0, env)?,
                labels,
                image,
                command,
                quiet: false,
                memory,
                cpus,
                cpu_weight,
                cpuset,
                // Sem flag de CLI: o nível de grupo entra pelo MANIFESTO
                // (`cgroupParent`), que é quem sabe agrupar cargas.
                cgroup_parent: None,
                kube_cgroup_parent,
                io_weight,
                no_supervisor: false,
                io_max: compose_io_max(
                    device_read_bps.as_deref(),
                    device_write_bps.as_deref(),
                    device_read_iops.as_deref(),
                    device_write_iops.as_deref(),
                )
                .map_err(delonix_runtime_core::Error::Invalid)?,
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
                dns,
                dns_search,
                dns_option,
                group_add,
                masked_path,
                readonly_path,
                sysctl,
                gpus,
                ip,
                network_alias,
                add_host,
                wait_healthy,
                wait_timeout,
                health: health_opts(
                    health_cmd,
                    health_interval,
                    health_timeout,
                    health_retries,
                    health_start_period,
                ),
                knows,
                knows_none,
                pod,
                pod_infra_pid,
                net_bps,
                net_burst,
                log_driver,
                log_file,
                log_cri,
            },
        ),
        ContainerCmd::Ps {
            all,
            quiet,
            output,
            namespace,
            size,
        } => cmd_ps(
            &images,
            &store,
            all,
            quiet,
            output,
            namespace.as_deref(),
            size,
        ),
        ContainerCmd::Start { ids } => for_each_id(&ids, |id| cmd_start(&images, &store, id)),
        ContainerCmd::Stop { ids, time } => for_each_id(&ids, |id| cmd_stop(&store, id, time)),
        ContainerCmd::Kill { ids, signal } => for_each_id(&ids, |id| cmd_kill(&store, id, &signal)),
        ContainerCmd::Wait { ids } => for_each_id(&ids, |id| cmd_wait(&store, id)),
        ContainerCmd::Restart { ids, time } => {
            for_each_id(&ids, |id| cmd_restart(&images, &store, id, time))
        }
        ContainerCmd::Rename { id, new_name } => cmd_rename(&store, &id, &new_name),
        ContainerCmd::Port { id } => cmd_port(&store, &id),
        ContainerCmd::Rm { ids, force } => {
            for_each_id(&ids, |id| cmd_rm(&images, &store, id, force))
        }
        ContainerCmd::Prune { force } => cmd_prune(&images, &store, force),
        ContainerCmd::Exec {
            interactive,
            tty,
            env,
            workdir,
            user,
            id,
            command,
        } => cmd_exec(
            &images,
            &store,
            &id,
            interactive,
            tty,
            &env,
            workdir.as_deref(),
            user.as_deref(),
            &command,
        ),
        ContainerCmd::Pause { ids } => for_each_id(&ids, |id| cmd_freeze(&store, id, true)),
        ContainerCmd::Unpause { ids } => for_each_id(&ids, |id| cmd_freeze(&store, id, false)),
        ContainerCmd::Commit { id, tag } => cmd_commit(&images, &store, &id, &tag),
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
            net_rate,
            net_burst,
            net_rate_clear,
            memory,
            cpus,
        } => cmd_update(
            &store,
            &id,
            UpdateOpts {
                publish_add,
                publish_rm,
                volume_add,
                volume_rm,
                net_rate,
                net_burst,
                net_rate_clear,
                memory,
                cpus,
            },
        ),
        ContainerCmd::Stats { ids } => cmd_stats(&store, &ids),
        ContainerCmd::Logs {
            id,
            follow,
            tail,
            since,
            timestamps,
        } => cmd_logs(&images, &store, &id, follow, tail, since, timestamps),
        ContainerCmd::Attach { id, interactive } => cmd_attach(&images, &store, &id, interactive),
        ContainerCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
    }
}

/// Dry-run: the FLAT spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: ContainerSpec = container_spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

/// Dry-run: the Pod-shaped spec with defaults materialized (round-trips `PodSpec`).
pub fn pod_spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: PodSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (images, store) = open_stores()?;
    for doc in manifest::of_kind(docs, k::CONTAINER) {
        let name = &doc.metadata.name;
        // Pod-shaped (k8s-like) when `spec.containers` is present; otherwise the
        // flat spec. The two shapes never mix.
        let pod_shaped = doc.spec.get("containers").is_some();
        // The typo warning used to live here, before the early-continue below, so
        // that re-applying against an existing resource still showed it. It now
        // runs in `manifest::load` — which every path arrives at, including this
        // one — so it keeps that property AND gains `validate`/`plan`, which it
        // never had. See `manifest::spec_fields_for`.
        if store.list()?.iter().any(|c| &c.name == name) {
            println!("container/{name}: already exists, nothing to do");
            continue;
        }
        if pod_shaped {
            let pod: PodSpec = manifest::spec_of(doc)?;
            let opts = pod_to_run_opts(name, doc.metadata.namespace.clone(), pod)?;
            cmd_run(&images, &store, opts)?;
            println!("container/{name}: created");
            continue;
        }
        let spec: ContainerSpec = container_spec_of(doc)?;
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
                cgroup_parent: spec.cgroup_parent.map(Into::into),
                io_weight: spec.io_weight,
                io_max: None,
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
                add_host: spec.add_host,
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

/// `--user <uid[:gid]|name[:group]>` resolved against the image rootfs
/// ([`delonix_image::rootfs_user`]), worded for the terminal.
fn resolve_run_user(rootfs: &str, spec: &str) -> Result<(u32, Option<u32>)> {
    delonix_image::rootfs_user::resolve_user(std::path::Path::new(rootfs), spec)
        .map_err(user_lookup_error)
}

/// A `--user` the image cannot satisfy, in the operator's language.
fn user_lookup_error(e: delonix_image::rootfs_user::UserLookupError) -> Error {
    use delonix_image::rootfs_user::UserLookupError;
    Error::Invalid(match e {
        UserLookupError::EmptyUser => super::po::t("--user: empty user").into(),
        UserLookupError::NoSuchUser(user) => super::po::tf(
            "--user: user '{user}' does not exist in the image (/etc/passwd)",
            &[("user", &user)],
        ),
        UserLookupError::NoSuchGroup(group) => super::po::tf(
            "--user: group '{group}' does not exist in the image (/etc/group)",
            &[("group", &group)],
        ),
    })
}

/// Parses one `--device-*-bps`/`--device-*-iops` value into a plain number.
///
/// Docker's syntax is `<device>:<rate>` (`/dev/sda:10mb`). The device part is
/// accepted and IGNORED, on purpose: a container's writes can only ever reach
/// the disk backing the store, so there is exactly one device to limit and
/// letting the user name a different one would accept an instruction the engine
/// cannot honour. Bare `10mb` — the useful form here — works too.
///
/// Sizes take the same binary suffixes as `-m` (`k`/`m`/`g`/`t`, optional
/// trailing `b`). IOPS values are plain counts.
pub(crate) fn parse_io_rate(spec: &str, is_bytes: bool) -> std::result::Result<u64, String> {
    // Drop a leading `<device>:` if present. Split from the RIGHT so a device
    // path containing ':' can't eat the rate.
    let raw = match spec.rsplit_once(':') {
        Some((dev, rate)) if dev.starts_with('/') || dev.contains(':') => rate,
        _ => spec,
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(format!("empty I/O rate in {spec:?}"));
    }
    if !is_bytes {
        return raw
            .parse::<u64>()
            .map_err(|_| format!("invalid IOPS value {raw:?} (expected a plain number)"));
    }
    let lower = raw.to_ascii_lowercase();
    let body = lower.strip_suffix('b').unwrap_or(&lower);
    let (num, mult) = match body.chars().last() {
        Some('k') => (&body[..body.len() - 1], 1024u64),
        Some('m') => (&body[..body.len() - 1], 1024 * 1024),
        Some('g') => (&body[..body.len() - 1], 1024 * 1024 * 1024),
        Some('t') => (&body[..body.len() - 1], 1024u64.pow(4)),
        _ => (body, 1),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid size {raw:?} (use e.g. 10mb, 1g)"))?;
    if !n.is_finite() || n <= 0.0 {
        return Err(format!("I/O rate must be positive: {raw:?}"));
    }
    let bytes = n * mult as f64;
    // Same overflow guard as `parse_size_bytes`: `as u64` on f64 SATURATES, so
    // an absurd value would become u64::MAX — a limit that reads as set and is
    // no limit at all.
    if bytes >= u64::MAX as f64 {
        return Err(format!("I/O rate does not fit in 64 bits: {raw:?}"));
    }
    Ok(bytes as u64)
}

/// Composes the four `--device-*` flags into the value half of a cgroup-v2
/// `io.max` line (`rbps=… wbps=… riops=… wiops=…`). `None` when none was given.
///
/// The engine prepends the store device's `major:minor` — see
/// `delonix_runtime`'s `slice_io_device`.
pub(crate) fn compose_io_max(
    read_bps: Option<&str>,
    write_bps: Option<&str>,
    read_iops: Option<&str>,
    write_iops: Option<&str>,
) -> std::result::Result<Option<String>, String> {
    let mut parts = Vec::new();
    for (flag, key, val, bytes) in [
        ("--device-read-bps", "rbps", read_bps, true),
        ("--device-write-bps", "wbps", write_bps, true),
        ("--device-read-iops", "riops", read_iops, false),
        ("--device-write-iops", "wiops", write_iops, false),
    ] {
        if let Some(v) = val {
            let n = parse_io_rate(v, bytes).map_err(|e| format!("{flag}: {e}"))?;
            parts.push(format!("{key}={n}"));
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join(" ")))
}

/// Reads `--env-file0` files (NUL-separated `KEY=VAL`, the `/proc/<pid>/environ`
/// format) and puts their entries BEFORE the `-e` values, so an explicit `-e`
/// still overrides. An entry without `KEY=` is refused, not skipped: a variable
/// that silently does not arrive is how a pod dies later for an unrelated-looking
/// reason.
fn with_env_file0(files: &[String], env: Vec<String>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for f in files {
        let bytes =
            std::fs::read(f).map_err(|e| Error::Invalid(format!("--env-file0 {f}: {e}")))?;
        out.extend(delonix_compute::run::parse_env0(&bytes, f)?);
    }
    out.extend(env);
    Ok(out)
}

pub(crate) use delonix_compute::RunOpts;

/// Warns, loudly, when `--namespace <ns>` was requested but the kernel is
/// not actually filtering intra-bridge traffic — the precondition namespace
/// isolation silently depends on (see the `br_netfilter` section of
/// AGENTS.md — isolation is INERT without it, measured 2026-08-12). Without
/// the module
/// (or with `bridge-nf-call-iptables=0`), every rule installs, `stack ls`/
/// the firewall listing report success, and a container in a DIFFERENT
/// namespace on the same bridge is reachable anyway — a security property
/// that reads as applied while it does nothing.
///
/// Called on every attach of a NAMED namespace rather than once, matching
/// the existing style for `--network-alias`/`--expose` (see nearby): an
/// operator who fixes the host later stops seeing it, one who does not is
/// reminded every time, not just the first.
///
/// Never refuses — this host may be one where the fix is not in the
/// operator's hands yet. A query failure (holder unreachable/too old) stays
/// silent: "could not ask" is not "it's off", same discipline as
/// `infra::network_routes_live`.
pub(crate) fn warn_if_namespace_isolation_inert(namespace: &str) {
    if namespace.is_empty() || namespace == "default" {
        return;
    }
    if let Ok(false) = infra::br_netfilter_active() {
        super::output::warn(&super::po::tf(
            "--namespace '{namespace}' is active, but this host is not filtering bridge traffic (br_netfilter not loaded, or net.bridge.bridge-nf-call-iptables=0) — isolation between namespaces on the SAME network reports success but does not actually block traffic. Fix: modprobe br_netfilter && sysctl -w net.bridge.bridge-nf-call-iptables=1 net.bridge.bridge-nf-call-ip6tables=1 (persist via /etc/modules-load.d and /etc/sysctl.d, or `install.sh --tune`)",
            &[("namespace", namespace)],
        ));
    }
}

/// Refuses `-m`/`--cpus`/`--cpu-weight` when this session has no real cgroup2
/// delegation, instead of accepting the flag and letting the kernel ignore it.
///
/// # Why this is a refusal now, and was a warning before
///
/// `cgroup_limits_apply()` already told the truth here — it just never
/// stopped anything. A container created without delegation runs with
/// `memory.max=max`/`cpu.max=max` regardless of `-m 128M`, and the `run`
/// command still exits 0. That is not a best-effort limit, it is a limit that
/// does not exist while claiming success — the exact shape of silent failure
/// this engine refuses everywhere else (`ingress ls` used to say `allow` for a
/// blocked port; this is the same mistake on the cgroup side).
///
/// # Scope, deliberately narrow
///
/// `cgroup_limits_apply()` proves delegation of the base that `memory`/`cpu`/
/// `pids` share (a `Delegate=yes` scope typically hands over exactly those
/// three). It says nothing about `cpuset`/`io`, which this host's own
/// `user@.service` routinely does NOT delegate even when `memory`/`cpu` are
/// fine — see this repo's own measurement of that split (AGENTS.md, "cgroup
/// delegation: cpu fatal, cpuset/io optional"). Refusing `--cpuset`/
/// `--io-weight`/`--io-max` on the same signal would be
/// inventing a precision this probe was never built to have, and would refuse
/// requests this host can actually satisfy. Those three keep the pre-existing
/// best-effort behaviour until they get their own controller-specific probe.
///
/// # Escape hatch
///
/// `DELONIX_ALLOW_UNENFORCED_LIMITS=1` runs anyway, loudly. Same shape as
/// `DELONIX_FORWARD_POLICY=accept`/`DELONIX_ENABLE_IPV6=1` elsewhere in this
/// engine: a deliberate, visible opt-out, never a silent one.
fn preflight_resource_limits(opts: &RunOpts) -> Result<()> {
    let asked = opts.memory.is_some() || opts.cpus.is_some() || opts.cpu_weight.is_some();
    let escape_hatch = std::env::var_os("DELONIX_ALLOW_UNENFORCED_LIMITS").is_some();
    resource_limits_decision(asked, runtime::cgroup_limits_apply(), escape_hatch)
}

/// The decision `preflight_resource_limits` makes, pulled out pure so it can
/// be tested against the three states (not asked / asked-and-delegated /
/// asked-and-not) without a real cgroup2 tree.
fn resource_limits_decision(asked: bool, delegated: bool, escape_hatch: bool) -> Result<()> {
    if !asked || delegated {
        return Ok(());
    }
    if escape_hatch {
        eprintln!(
            "{}",
            super::po::t(
                "warning: -m/--cpus/--cpu-weight were requested but this session has \
                 no cgroup2 delegation, so the kernel will not see them — continuing \
                 unenforced because DELONIX_ALLOW_UNENFORCED_LIMITS is set"
            )
        );
        return Ok(());
    }
    Err(delonix_runtime_core::Error::Unavailable(
        super::po::t(
            "-m/--cpus/--cpu-weight were requested but this session has no cgroup2 \
             delegation, so the kernel would never see them — the container would run \
             unlimited while this command reports success. Fix: systemd-run --user \
             --scope -p Delegate=yes -- delonix container run ... (see `delonix system \
             doctor`), or set DELONIX_ALLOW_UNENFORCED_LIMITS=1 to run unenforced anyway.",
        )
        .to_string(),
    ))
}

#[cfg(test)]
mod resource_limits_preflight_tests {
    use super::resource_limits_decision;

    #[test]
    fn nada_pedido_nunca_recusa_seja_qual_for_a_delegacao() {
        assert!(resource_limits_decision(false, false, false).is_ok());
        assert!(resource_limits_decision(false, true, false).is_ok());
    }

    #[test]
    fn pedido_e_delegado_segue_sem_aviso_nenhum() {
        assert!(resource_limits_decision(true, true, false).is_ok());
    }

    #[test]
    fn pedido_sem_delegacao_e_sem_escape_hatch_recusa() {
        let err = resource_limits_decision(true, false, false).unwrap_err();
        assert_eq!(
            crate::cmd::exitcode::for_error(&err),
            crate::cmd::exitcode::UNAVAILABLE
        );
    }

    #[test]
    fn pedido_sem_delegacao_com_escape_hatch_segue() {
        assert!(resource_limits_decision(true, false, true).is_ok());
    }
}

/// The `container run` read ports over this CLI's image store and host helpers
/// (`docs/discovery/54_P2_COMPUTE_RUN.md`). Each method is the code `cmd_run`
/// ran inline before, unchanged — progress line and messages included.
struct CliRunPorts<'a> {
    images: &'a ImageStore,
    store: &'a Store,
}

impl CliRunPorts<'_> {
    /// The engine's spawn specification for a [`delonix_compute::launch::Launch`]:
    /// what the network decides (`run_network::launch_addresses`), then the one
    /// builder (`delonix_runtime::launch_spec::run_spec`).
    fn run_spec<'h>(
        c: &Container,
        l: &delonix_compute::launch::Launch,
        slirp_hook: &'h runtime::StartedHook<'h>,
    ) -> RunSpec<'h> {
        let addrs = delonix_net::run_network::launch_addresses(l);
        runtime::launch_spec::run_spec(c, l, addrs.dns, addrs.hosts_ip, slirp_hook)
    }
}

impl delonix_compute::launch::WorkloadRuntime for CliRunPorts<'_> {
    fn create(
        &self,
        c: &mut Container,
        l: &delonix_compute::launch::Launch,
    ) -> Result<delonix_runtime_core::Status> {
        let hook = |pid: i32| -> Result<()> { delonix_net::slirp_attach(pid, &l.slirp_ports) };
        let spec = Self::run_spec(c, l, &hook);
        runtime::create_with(self.store, c, &l.rootfs, &spec)
    }

    fn supervise(
        &self,
        c: &mut Container,
        l: &delonix_compute::launch::Launch,
        policy: &str,
    ) -> Result<()> {
        let hook = |pid: i32| -> Result<()> { delonix_net::slirp_attach(pid, &l.slirp_ports) };
        let spec = Self::run_spec(c, l, &hook);
        let health = c.health.clone();
        let on_first_start = |c: &Container| {
            if let Some(cfg) = health.clone() {
                spawn_health_monitor(c.id.clone(), cfg);
            }
        };
        let state_root = super::util::state_root();
        runtime::supervise::run_supervised(
            self.store,
            c,
            &l.rootfs,
            &spec,
            policy,
            &runtime::supervise::Supervision {
                state_root: &state_root,
                on_first_start: &on_first_start,
                silent_death: super::po::t(
                    "the container did not start, and the supervisor died before saying why",
                ),
            },
        )?;
        println!("{}", c.id);
        Ok(())
    }

    fn discard_unstarted(&self, id: &str) {
        discard_unstarted(self.images, self.store, id);
    }
}

pub(crate) fn cmd_run(images: &ImageStore, store: &Store, opts: RunOpts) -> Result<()> {
    // The node's runtime policy, BEFORE anything is created or pulled. Same
    // placement reason as the capability ceiling in the CRI: everything reaching
    // here was already authorised by whoever called it, and a policy that lives
    // only in a cluster's admission chain runs on another machine this node
    // cannot verify.
    //
    // Ahead of the image resolution on purpose: a policy that bans a registry
    // stops the request before a single byte is fetched from it.
    //
    // No policy file = no ceiling, byte for byte the old behaviour. A file that
    // exists and does not parse is an ERROR: somebody's intent is unknown, and
    // running anyway is the silent degradation this engine refuses elsewhere.
    super::policy::enforce(
        &super::util::state_root(),
        opts.name.as_deref().unwrap_or(&opts.image),
        &super::policy::Request::container(
            &opts.image,
            opts.privileged,
            opts.net.is_empty() || opts.net == "host",
        ),
    )?;
    // Same reasoning, same place as the policy check above: refuse before
    // anything is created, not after. See `preflight_resource_limits`.
    preflight_resource_limits(&opts)?;
    // The combinations of flags that cannot mean anything, refused before any
    // side effect — see `delonix_compute::preflight` for the two that used to be
    // checked only after the workload had run, or never.
    delonix_compute::preflight::check_run_opts(&opts)?;
    // Intact copy for the re-exec (the destructuring below consumes opts).
    let opts_copy = opts.clone();
    let RunOpts {
        detach,
        name,
        net,
        namespace,
        ports,
        rm,
        image,
        quiet,
        wait_healthy,
        wait_timeout,
        pod,
        pod_infra_pid,
        ..
    } = opts;
    // Isolation namespace (default `default`). It goes into an nft set name (via
    // `dlxns_set`, which HASHES it → safe) and into a control-line token (which
    // `attach_container` sanitizes). Here we only ensure it's non-empty.
    let namespace = namespace
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "default".into());
    // Port RANGES (`-p 8000-8002:9000-9002`, Docker syntax) expand into one spec per
    // port here, at the boundary — everything downstream (ownership, unpublish, the
    // stored `ports`) is keyed on a single port and stays that way.
    let ports: Vec<String> = ports
        .iter()
        .map(|s| delonix_net::expand_publish_range(s))
        .collect::<Result<Vec<_>>>()?
        .concat();
    // Validate the `-p`s BEFORE creating anything (clear error, no leftovers).
    for spec in &ports {
        let (addr, hp, cp, _) = delonix_net::parse_publish_addr(spec)?;
        // A host port below 1024 is bound by the slirp as THIS unprivileged user, so
        // it fails with the slirp's opaque `add_hostfwd` JSON — after the container is
        // already up. Same treatment as the port-conflict error below: state the fact,
        // then the ways out as ready-to-copy commands.
        let bind = delonix_net::publish_bind_addr(addr.as_deref());
        match hp.parse::<u16>() {
            Ok(p) if !delonix_net::can_bind_host_port(&bind, p) => {
                return Err(Error::Invalid(super::po::tf(
                    "host port {hp} needs privilege to bind — rootless cannot publish \
                     ports below net.ipv4.ip_unprivileged_port_start\n\
                     \n\
                     fix it with ONE of these:\n\
                     \x20 delonix container run -p {alt}:{cp} ...    # publish on an unprivileged port\n\
                     \x20 curl -fsSL {url} | bash -s -- --low-ports    # or lower the threshold for good (host-wide)",
                    &[
                        ("hp", hp.as_str()),
                        // 80 → 8080, 443 → 8443, 22 → 8022; always inside u16.
                        ("alt", &(p as u32 + 8000).to_string()),
                        ("cp", cp.as_str()),
                        // The installer's `--low-ports` writes the sysctl to
                        // /etc/sysctl.d, so it survives a reboot — a bare
                        // `sysctl -w` here would send the user down a path that
                        // silently reverts on the next boot.
                        (
                            "url",
                            "https://github.com/angolardevops/delonix-runtime/\
                             releases/latest/download/install.sh",
                        ),
                    ],
                )));
            }
            _ => {}
        }
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
            let (addr, hp, cp, _) = delonix_net::parse_publish_addr(spec)?;
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
            // `port_owner` only knows about OUR containers — a host process this
            // engine never heard of (another user's server, a stray listener) is
            // invisible to it and, on top of that, sails through the privilege
            // probe above too (`can_bind_host_port` deliberately answers `true`
            // for `EADDRINUSE` — see its doc comment — precisely so as not to
            // steal THIS diagnosis). Without this check the conflict only
            // surfaced deep inside the slirp handshake, minutes later once the
            // image had already been pulled and a container already created for
            // it — measured: `delonix container run -d -p 8080:8080 <image>`
            // against a host process already on :8080 took 15s+ to fail, with the
            // raw `add_hostfwd failed` JSON as the only clue.
            if let Some(e) = host_port_conflict_error(&hp, &cp, addr.as_deref()) {
                return Err(e);
            }
        }
    }
    let id = std::env::var("DELONIX_REEXEC_ID").unwrap_or_else(|_| generate_id());
    let reexec = std::env::var("DELONIX_REEXEC_ID").is_ok();

    // The name, and its uniqueness in the namespace, need the store and a
    // translated refusal; everything after that goes through the Compute use case.
    let cname = match name {
        Some(n) => n,
        None => {
            let existing: Vec<String> = store.list()?.iter().map(|c| c.name.clone()).collect();
            super::names::derived_name(&id, |n| existing.iter().any(|e| e == n))
                .unwrap_or_else(|| format!("dlx-{}", &id[..8.min(id.len())]))
        }
    };
    // BUG FIXED HERE (HIGH, found live by adversarial review): no code path
    // anywhere ever validated a container's name — unlike VM/Secret/Volume,
    // which all got a `valid_*_name` boundary check in earlier audits. The
    // internal DNS resolver (`delonix-net::infra::parse_internal_name`)
    // treats any name WITHOUT a `.delonix.internal`/`.delonix.io` suffix as a
    // whole-name match resolvable from ANY namespace — an ordinary
    // `container run --name registry.npmjs.org` (no manifest, no privilege)
    // makes the holder's node-wide DNS server answer every OTHER container's
    // lookup of that hostname with the attacker's own IP, hijacking it across
    // every namespace, silently, ahead of the real upstream forward.
    if !valid_container_name(&cname) {
        return Err(Error::Invalid(format!(
            "invalid container name '{cname}' — only letters, digits, '-', '_' allowed (no '.': \
             a dotted name would be indistinguishable from an external domain to internal DNS \
             resolution, letting a container hijack that domain node-wide)"
        )));
    }
    // UNIQUE name WITHIN THE NAMESPACE, like k8s. Without uniqueness at all,
    // several containers with the same name got created: `find` resolves to the
    // first, and an `rm <name>` only caught that one — the rest were left
    // orphaned and invisible to management by name (seen the hard way: 2x
    // `loja-app` + 2x `loja-db`).
    //
    // It used to be unique GLOBALLY, which made a namespace something that was
    // not a name space: `--name web -n teamA` refused `web` in `teamB`, so two
    // teams could not both own `db`/`web`/`api` — exactly the names everyone
    // uses — and had to hand-prefix what the namespace already said (ADR-0011
    // §3). The pair is the boundary the rest of the model already draws.
    if let Some(dup) = store
        .list()?
        .iter()
        .find(|c| c.name == cname && c.namespace == namespace)
    {
        return Err(Error::Invalid(super::po::tf(
            "the name '{name}' is already in use in namespace '{ns}' by container {id} — pick another or remove it first",
            &[
                ("name", cname.as_str()),
                ("ns", namespace.as_str()),
                ("id", dup.short_id()),
            ],
        )));
    }
    // ---- what the record needs from files, the image and this node ----
    // `--env-file`: read here, parsed by the builder (each file before `-e`).
    // Volumes, devices, image, rootfs, `--env-file`, `--user` — resolved through the
    // read ports (`delonix_compute::ports`), then the record is built from them.
    let read_ports = CliRunPorts { images, store };
    let apparmor_disabled = |profile: &str| {
        Error::Invalid(super::po::tf(
            "--apparmor {p}: AppArmor is not enabled on this host, so nothing would confine this \
             container",
            &[("p", profile)],
        ))
    };
    let on_attached = |namespace: &str| warn_if_namespace_isolation_inert(namespace);
    let register_expose = |name: &str, namespace: &str, ip: &str, port: u16| {
        super::ingress_proxy::auto_register(name, namespace, ip, port)
    };
    let unpacking = |prepare: &mut dyn FnMut() -> Result<std::path::PathBuf>| {
        let mut prog = super::output::Progress::new();
        prog.step_after(
            super::po::t("unpacking the image"),
            "📦",
            std::time::Duration::from_millis(800),
        );
        let path = prepare()?;
        prog.ok();
        Ok(path)
    };
    let host_network = delonix_net::run_network::HostNetwork {
        state_root: super::util::state_root(),
        on_attached: &on_attached,
        register_expose: &register_expose,
    };
    // Resolving prepares the rootfs, and every refusal from here to the launch (an
    // unknown `--user`, a bad `--security-opt`, a network that will not attach)
    // used to leave that directory behind with no record to find it by. The guard
    // discards it on any early return; `launch::start` owns the cleanup after that.
    let mut unstarted = UnstartedGuard {
        images,
        store,
        id: &id,
        armed: true,
    };
    let resolved = delonix_compute::run::resolve_run(
        &opts_copy,
        id.clone(),
        cname,
        namespace.clone(),
        reexec,
        &delonix_image::run_images::HostImages {
            store: images,
            announce_pull: &super::util::announce_pull,
            unpacking: &unpacking,
            user_error: &user_lookup_error,
        },
        &delonix_volume::HostVolumes {
            root: super::util::state_root(),
        },
        &delonix_runtime::cdi::HostDevices,
        &delonix_runtime::run_host::HostRuntime {
            state_root: super::util::state_root(),
            apparmor_disabled: &apparmor_disabled,
        },
    )?;
    print_notices(&resolved.notices);
    let rootfs = resolved.rootfs;
    let mounts = resolved.mounts;
    let apparmor_profile = resolved.apparmor_profile;
    let mut c = delonix_compute::run::build_record(&opts_copy, resolved.record)?;

    let custom_net = custom_net_name(&net);
    let mut attached_ip = None;
    if let Some(n) = &custom_net {
        if reexec {
            attached_ip = std::env::var("DELONIX_REEXEC_IP").ok();
        } else {
            let mut notices = Vec::new();
            let attached = delonix_compute::network::attach_custom_network(
                &opts_copy,
                &c,
                n,
                &host_network,
                &mut notices,
            );
            print_notices(&notices);
            let (netns, ip) = attached?;
            return reexec_into_netns(&id, &netns, &ip, &opts_copy, true);
        }
    }
    if let Some(pn) = &pod {
        // PERSIST THE MEMBERSHIP, and do it BEFORE the branch — the fourth time
        // this exact trap has been paid for in this repo (`-v` never persisted,
        // `-p` on a custom network, extra networks lost on restart). `Container`
        // has had a `pod` field all along, `describe` has always printed it, and
        // NOTHING ever assigned it: the only trace of membership on disk was a
        // label. Consequences, both measured: `container describe` on a pod
        // member showed no pod at all, and `cmd_start` — which rebuilds the spec
        // from the record — had no way to know it must re-enter the pod's shared
        // netns, so a member's `restart` died with `clone failed: EPERM` and left
        // it `Dead` with no path back.
        c.pod = Some(pn.clone());
        if reexec {
            attached_ip = std::env::var("DELONIX_REEXEC_IP").ok();
            // PERSIST IT. `c.ip` stayed `None` for every pod member: the branch
            // that assigns it only runs for a custom network. The record then
            // described a container with no address — so `describe` lied, and
            // anything rebuilding from the record (`ingress ls`, a firewall
            // reapply) had no address to work with. Fifth instance of the same
            // trap in this file.
            c.ip = attached_ip.clone();
        } else {
            // The address the pod REALLY got, carried on the member's labels by
            // `pod::create_pod` (`POD_IP_LABEL`) straight from the attach.
            //
            // BUG FIXED: this used to be `infra::container_ip(pn)`, whose own
            // doc-comment says "on the DEFAULT ingress network (10.200.A.B)" —
            // it derives the address from the netns name in a FIXED prefix and
            // never looks at the network the pod is actually on. For a pod on a
            // custom network the value was simply wrong, and it is what gets
            // persisted as `c.ip` and therefore what the internal DNS serves.
            // Measured: pod `p1` on network `audit` really held 10.250.0.2 while
            // its record said 10.200.0.2 — the address of a DIFFERENT pod on the
            // default bridge — so resolving `p1-a` handed callers the wrong
            // workload. `container_ip_on(prefix, id)` (the network-aware sibling)
            // existed all along; the sixth instance in this repo of the right
            // helper being there and never called.
            // The label KEY comes from the constant the writer uses, never a
            // second copy of the string: a literal here would keep working until
            // the day someone renames it there, and then fail silently by
            // falling back to the wrong-prefix guess this fix exists to remove.
            let pod_ip_prefix = format!("{}=", super::pod::POD_IP_LABEL);
            let ip = opts_copy
                .labels
                .iter()
                .find_map(|l| l.strip_prefix(&pod_ip_prefix).map(str::to_string))
                .unwrap_or_else(|| infra::container_ip(pn));
            return reexec_into_netns(&id, pn, &ip, &opts_copy, false);
        }
    }
    c.ports = ports.clone();
    // Ports, and what a custom network carries (isolation, `--expose`, shaping),
    // before the workload starts — see `delonix_compute::network`.
    let mut notices = Vec::new();
    let wired = delonix_compute::network::wire_network(
        &opts_copy,
        &mut c,
        custom_net.as_deref(),
        attached_ip.as_deref(),
        &host_network,
        &mut notices,
    );
    print_notices(&notices);
    wired?;

    // `-p` without a custom network (`--net host`, the default): the container
    // stops sharing the host's network and gets its own netns with slirp4netns +
    // the requested hostfwds — the behavior of `docker run -p` (NAT network by
    // default), in podman's rootless model. The slirp dies with the netns.
    let launch = delonix_compute::launch::Launch {
        slirp_ports: delonix_compute::launch::slirp_ports(
            custom_net.is_some(),
            pod.is_some(),
            &ports,
        ),
        rootfs,
        mounts,
        detach,
        second_pass: reexec,
        net_none: net == "none",
        custom_net: custom_net.clone(),
        pod: pod.is_some(),
        attached_ip: attached_ip.clone(),
        pod_infra_pid,
        apparmor: apparmor_profile,
        log_path: c.log_path.clone(),
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
    // A detached start gets a SUPERVISOR that becomes the container's parent, so the
    // exit code is not lost; see `delonix_compute::launch::start`.
    unstarted.armed = false;
    let final_status =
        match delonix_compute::launch::start(&opts_copy, &mut c, &launch, &read_ports)? {
            delonix_compute::launch::Started::Supervised => {
                // The supervisor takes the whole detached path, so this is where most
                // `-d` runs end — and where `--wait` used to be silently ignored.
                if wait_healthy {
                    wait_until_healthy(images, store, &id, wait_timeout)?;
                }
                return Ok(());
            }
            delonix_compute::launch::Started::Created(status) => status,
        };
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
            // BUG FOUND: `--rm` (foreground) stopped at `unmount_rootfs`, which
            // deliberately PRESERVES the flat rootless rootfs (it's meant for
            // `start` to reuse) — only `remove_container_dir` actually deletes
            // it. `--rm`'s whole contract is full auto-cleanup, same as `rm -f`
            // (see `cmd_rm`, which already does this); without it every
            // foreground `--rm` run left its full rootfs behind forever, the
            // exact same disk-pressure leak `cmd_rm` was already fixed for.
            purge_container_dir(images, &c.id);
            // `--rm` still has to speak the container's exit code — a one-shot
            // job (`run --rm ... pg_dump`) is the single most common shape that
            // depends on it.
            propagate_exit_status(&final_status);
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
    // `--wait`: só depois de o container estar registado e a correr — e depois
    // da guarda de morte-ao-nascer acima, para não esperar 60s por algo que já
    // morreu.
    if wait_healthy {
        wait_until_healthy(images, store, &id, wait_timeout)?;
    }
    if !detach {
        propagate_exit_status(&final_status);
    }
    Ok(())
}

/// Bloqueia até o `HEALTHCHECK` da imagem passar, ou até ao tempo limite.
///
/// Existe porque toda a gente acaba por escrever `until curl ...; do sleep 2;
/// done` à volta de um `run -d` — e escreve-o mal (sem limite, ou a sondar uma
/// coisa que não é a saúde real do serviço). A resolução do comando é a MESMA
/// que o `depends_on: service_healthy` do compose usa (`image_health_argv`),
/// para os dois não poderem divergir.
///
/// Sem `HEALTHCHECK` na imagem é ERRO, não um retorno instantâneo: quem pede
/// `--wait` está a dizer "não continues até isto servir", e devolver de
/// imediato seria responder à pergunta errada em silêncio.
fn wait_until_healthy(
    images: &ImageStore,
    store: &Store,
    id: &str,
    timeout_secs: u64,
) -> Result<()> {
    let c = find(store, id)?;
    let Some(argv) = super::compose::image_health_argv(images, &c.image) else {
        return Err(Error::Invalid(format!(
            "--wait: image '{}' declares no HEALTHCHECK — nothing to wait for \
             (add one to the image, or drop --wait)",
            c.image
        )));
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut attempt: u32 = 0;
    // Same self-limiting wrapper the monitor uses: a probe that hangs would
    // otherwise pin `--wait` past its own deadline, which is the one thing the
    // flag exists to prevent.
    let script = argv.join(" ");
    let argv = if argv.len() == 3 && argv[0] == "/bin/sh" && argv[1] == "-c" {
        health_probe_argv(&argv[2], 30)
    } else {
        health_probe_argv(&script, 30)
    };
    loop {
        if runtime::exec(&c, &argv, false).unwrap_or(1) == 0 {
            return Ok(());
        }
        // Morreu enquanto esperávamos: dizer "não ficou saudável em 60s" seria
        // esconder a causa real, que está nos logs.
        if let Ok(cur) = find(store, id) {
            let dead = match cur.pid {
                // SAFETY: kill(pid, 0) sends no signal — it only tests existence.
                Some(p) => (unsafe { libc::kill(p, 0) } != 0),
                None => true,
            };
            if dead {
                return Err(Error::Invalid(format!(
                    "--wait: container '{}' exited while waiting to become healthy \
                     — see `delonix container logs {}`",
                    cur.name, cur.name
                )));
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Invalid(format!(
                "--wait: '{}' did not become healthy within {timeout_secs}s \
                 ({attempt} check(s)) — see `delonix container logs {}`",
                c.name, c.name
            )));
        }
        attempt += 1;
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// The shell exit code a terminal [`Status`] corresponds to, by the same
/// convention every container engine and shell uses.
pub(crate) fn exit_code_of(status: &Status) -> i32 {
    match status {
        Status::Failed(n) => *n,
        // Killed by a signal. We do not keep WHICH signal, and 137 (128+SIGKILL)
        // is both the overwhelmingly common case (OOM-kill, `stop` timeout) and
        // what `wait_to_status` already reports.
        Status::Crashed => 137,
        // `Stopped` is a clean exit 0; the non-terminal states cannot reach here
        // from a foreground run, and 0 is the safe reading if they somehow do.
        _ => 0,
    }
}

/// Makes the process exit with the container's own exit code.
///
/// **This is what made `container run` honest.** A foreground run used to return
/// `Ok(())` no matter how the workload ended: `exit 42`, `exit 1` and a failed
/// `execve` of the entrypoint all produced `$? = 0`, and so did a container that
/// never started at all (`failed to prepare the rootfs`, which the child reports
/// as 126). Every orchestrator, CI job and deploy step reading that exit
/// code was told "success" — a failed schema migration or a failed backup looked
/// fine until restore time.
///
/// Exiting the process directly, rather than threading a code back through
/// `Result`, is deliberate and is what docker/podman do: by the time we get here
/// the container is finished and any `--rm` cleanup has already run, and `main`
/// maps every `Err` onto exit 1 — which cannot express 42. A zero code returns
/// normally so the caller keeps its usual control flow.
pub(crate) fn propagate_exit_status(status: &Status) {
    let code = exit_code_of(status);
    if code == 0 {
        return;
    }
    // stdout is block-buffered when piped; `process::exit` runs no destructors.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

// ---- Adapters for the unified `delonix workload` layer (ADR-0002, Phase 2a) ----
// Thin wrappers over the existing listing/stop/rm logic so `cmd/workload.rs` can
// drive containers uniformly WITHOUT reaching into this module's private helpers.

/// Rows for `delonix workload ls` — the container half. Reconciles status like
/// `container ps` (same `reconcile_with_diagnostics`, so a workload seen here
/// matches what `container ps` reports).
pub(crate) fn workload_rows() -> Result<Vec<super::workload::WorkloadRow>> {
    let (_images, store) = super::util::open_stores()?;
    let mut cs = store.list()?;
    cs.sort_by_key(|c| std::cmp::Reverse(c.created_unix));
    let mut rows = Vec::new();
    for c in cs.iter_mut() {
        reconcile_with_diagnostics(&store, c);
        let uptime = match c.status {
            Status::Running | Status::Paused => {
                c.pid_starttime.and_then(output::uptime_from_starttime)
            }
            _ => None,
        };
        rows.push(super::workload::WorkloadRow {
            kind: "container",
            name: c.name.clone(),
            status: fmt_status_of(c, uptime),
            info: output::display_ref(&c.image),
            namespace: c.namespace.clone(),
        });
    }
    Ok(rows)
}

/// `true` if a container with this exact NAME exists (workload routing is by
/// name, not id-prefix — deliberately unambiguous).
pub(crate) fn workload_owns(name: &str) -> Result<bool> {
    let (_images, store) = super::util::open_stores()?;
    Ok(store.list()?.iter().any(|c| c.name == name))
}

pub(crate) fn workload_stop(name: &str) -> Result<()> {
    let (_images, store) = super::util::open_stores()?;
    cmd_stop(&store, name, 10)
}

pub(crate) fn workload_remove(name: &str, force: bool) -> Result<()> {
    let (images, store) = super::util::open_stores()?;
    cmd_rm(&images, &store, name, force)
}

pub(crate) fn workload_describe(name: &str) -> Result<()> {
    let (_images, store) = super::util::open_stores()?;
    cmd_describe(&store, &[name.to_string()])
}

/// Reconciles `c`'s status and, if it just flipped to `Crashed` in THIS call, records a
/// best-effort forensic snapshot (see [`record_crash_forensics`]). Same idiom as the
/// repeated `if reconcile_status(c) { store.update(...) }` seen throughout this file —
/// used at the handful of call sites (`ls`, `describe`, `start`) a human/kubelet is
/// actually likely to observe a fresh crash from first; other sites (`dash`, the `--rm`
/// watcher) keep the plain call, since `crash_reason`/`crashed_at` are set either way
/// (they live on `reconcile_status` itself) and duplicate forensics/events would be
/// wasted work, not wrong — the guard below only fires once per crash regardless of
/// which caller happens to be first.
fn reconcile_with_diagnostics(store: &Store, c: &mut Container) -> bool {
    if !runtime::reconcile_status(c) {
        return false;
    }
    *c = store
        .update(&c.id, runtime::reconcile_status)
        .unwrap_or_else(|_| c.clone());
    if matches!(c.status, Status::Crashed) {
        record_crash_forensics(c);
    }
    true
}

/// Best-effort forensics for a container that just flipped to `Crashed`: a short
/// `container crashed` line in `system events` (see `events.rs` on why it stays short)
/// plus the tail of the container's log saved alongside it
/// (`<root>/containers/<id>/crash-<ts>.log`) — since the engine is never this
/// process's real parent (see ARCHITECTURE), this is the only forensic trail available;
/// there is no captured exit code/signal to fall back on. Never fails the caller.
fn record_crash_forensics(c: &Container) {
    let (Some(reason), Some(ts)) = (c.crash_reason.as_deref(), c.crashed_at) else {
        return;
    };
    let root = super::util::state_root();
    let dir = root.join("containers").join(&c.id);
    const TAIL_BYTES: u64 = 8 * 1024;
    let log_path = dir.join("log");
    let tail = std::fs::metadata(&log_path).ok().and_then(|meta| {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&log_path).ok()?;
        f.seek(SeekFrom::Start(meta.len().saturating_sub(TAIL_BYTES)))
            .ok()?;
        let mut buf = String::new();
        f.read_to_string(&mut buf).ok()?;
        Some(buf)
    });
    let snapshot = format!(
        "reason={reason}\ncrashed_at={}\n\n--- tail of the container log ---\n{}\n",
        output::fmt_local(ts),
        tail.as_deref().unwrap_or("<no log>")
    );
    let _ = std::fs::write(dir.join(format!("crash-{ts}.log")), snapshot);
    delonix_runtime_core::events::emit(&root, "container", "crashed", &c.id, &c.name, Some(reason));
}

/// `--rm` in detached mode: with no daemon, removal is done by a dedicated
/// **watcher** — a detached process (setsid, stdio to /dev/null) that polls the
/// container's state ~1x/s via `reconcile_status` and, once it stops running, does
/// the same cleanup as `rm -f`. It dies afterwards; one watcher per `--rm` container.
fn spawn_rm_watcher(images: &ImageStore, store: &Store, id: &str) {
    // SAFETY: fork of a single-threaded process (CLI); the child only polls and exits.
    if unsafe { libc::fork() } == 0 {
        // SAFETY: in the forked watcher child: `setsid` has no preconditions, `/dev/null` is a
        // static C string, and the fd is checked before `dup2` onto 0/1/2 and closed once.
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
                // Same fix as the foreground `--rm` branch above: `unmount_rootfs`
                // preserves the flat rootless rootfs on purpose, only this
                // actually deletes it.
                purge_container_dir(images, &c.id);
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
/// `true` when a `Crashed` record only means "the process is gone and nobody was
/// its parent to collect the code" — as opposed to a death this process actually
/// observed via `waitpid`.
///
/// The distinction is the honest half of a documented architectural limit: for a
/// detached container the engine is never the real parent, so `reconcile_status`
/// can see THAT it died but never WHY. It marks such records with
/// `crash_reason = "process_gone"`. Everything downstream used to flatten that
/// into "Dead" and a fabricated exit code of 137 — telling the operator a clean
/// `exit 43` had been SIGKILLed. We do not have the code; saying so is the fix.
pub(crate) fn exit_code_unknown(c: &Container) -> bool {
    matches!(c.status, Status::Crashed) && c.crash_reason.as_deref() == Some("process_gone")
}

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

/// [`fmt_status`] for a real record, so it can tell an observed death from one
/// whose exit code was never captured.
///
/// A container that exited on its own while detached shows `Exited (unknown)` —
/// not `Dead`, which reads as "killed", and not `Exited (137)`, which is a
/// number we do not have. `Dead` stays for the cases where something really did
/// kill it (OOM, an external SIGKILL), which `crash_reason` distinguishes.
fn fmt_status_of(c: &Container, uptime: Option<u64>) -> String {
    if exit_code_unknown(c) {
        return "Exited (unknown)".to_string();
    }
    // The column an operator reads to find out why a container is down. «Dead»
    // alone reads the same for an OOM and for an external `kill -9`, and those
    // two have opposite fixes (raise `-m`, or find who sent the signal).
    if c.crash_reason.as_deref() == Some(delonix_runtime::OOM_KILLED)
        && matches!(c.status, Status::Crashed | Status::Failed(_))
    {
        return format!("{} (OOMKilled)", fmt_status(&c.status, uptime));
    }
    let base = fmt_status(&c.status, uptime);
    // Health only qualifies a RUNNING container. Appending "(healthy)" to
    // `Exited (0)` would be reporting the last thing we saw as if it were still
    // true — the exact silent-staleness this column exists to prevent.
    match (&c.status, &c.health_state) {
        (Status::Running, Some(h)) => format!("{base} ({})", h.health),
        _ => base,
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
            // O parser do motor, e não um `split_once(':')` cru — a forma
            // `hostIp:hostPort:contPort` tem DOIS dois-pontos, e cortar no
            // primeiro dá `127.0.0.1` como porta do host e `19555:80` como porta
            // do container. Medido: um serviço restrito a loopback aparecia como
            // `127.0.0.1->19555:80/tcp` na coluna que se lê exactamente para
            // decidir o que está exposto.
            // Só quando há mesmo uma porta de host. O `parse_publish_addr` aceita
            // uma porta nua e devolve-a nas DUAS pontas, o que fazia `"80"`
            // imprimir `80->80/tcp` em vez de `80/tcp` — apanhado pelo teste das
            // formas antigas, e a razão de ele cobrir o caso aborrecido também.
            let sem_proto = p.split_once('/').map(|(s, _)| s).unwrap_or(p.as_str());
            if sem_proto.contains(':') {
                if let Ok((addr, hp, cp, proto)) = delonix_net::parse_publish_addr(p) {
                    return match addr {
                        // Com endereço explícito imprime-se a forma do docker
                        // (`127.0.0.1:8080->80/tcp`): aqui o endereço é FACTO, veio
                        // da spec, e é a informação que mais importa nesta coluna.
                        Some(a) => format!("{a}:{hp}->{cp}/{proto}"),
                        // Sem endereço mantém-se a omissão deliberada do `0.0.0.0`
                        // — ver o doc-comment acima: o endereço efectivo depende do
                        // caminho de publicação e do `DELONIX_PUBLISH_ADDR`, e
                        // inventar um seria uma afirmação de exposição talvez falsa.
                        None => format!("{hp}->{cp}/{proto}"),
                    };
                }
            }
            let (spec, proto) = match p.split_once('/') {
                Some((s, pr)) => (s, pr),
                None => (p.as_str(), "tcp"),
            };
            // Só a porta do container (publicada sem porta de host fixa).
            format!("{spec}/{proto}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `container ps -o json` row (ADR-0005): stable keys mirroring the columns, with
/// machine-friendly values (full `id`, `created_unix` as a number, raw command/image).
#[derive(serde::Serialize)]
struct ContainerLsRow {
    id: String,
    image: String,
    command: String,
    created_unix: u64,
    status: String,
    ports: String,
    name: String,
    restarts: u32,
    /// Only populated when `-s/--size` is passed — a directory walk per
    /// container is not something a plain `ls` should pay for by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
}

/// How many times a container has been restarted, counted from the event log.
/// Same source `cluster ls`'s LAST RESTART column already reads, read once for
/// every container instead of once per row.
///
/// `cmd_run`'s initial launch emits `"create"`, never `"start"`, so there is no
/// off-by-one to subtract. Two things emit `"start"`: `cmd_start` (a `container
/// start`/`restart` on an EXISTING container), once per invocation, and the
/// restart-policy supervisor, once for each restart it actually made.
fn restart_counts(root: &std::path::Path) -> std::collections::HashMap<String, u32> {
    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for e in delonix_runtime_core::events::read(root) {
        if e.kind == "container" && e.action == "start" {
            *counts.entry(e.id).or_insert(0) += 1;
        }
    }
    counts
}

fn cmd_ps(
    images: &ImageStore,
    store: &Store,
    all: bool,
    quiet: bool,
    format: super::output::OutputFormat,
    namespace: Option<&str>,
    size: bool,
) -> Result<()> {
    let format = super::config::resolve_output(&super::util::state_root(), format);
    let mut cs = store.list()?;
    // The filter runs BEFORE anything else reads the list — `--namespace` that
    // narrowed the table and left the JSON, the `--quiet` ids or the counts
    // untouched would be a flag that works in one output and not in another,
    // which is worse than no flag at all.
    if let Some(ns) = namespace {
        cs.retain(|c| {
            let owner = if c.namespace.is_empty() {
                "default"
            } else {
                &c.namespace
            };
            owner == ns
        });
    }
    // Stable, useful order: most recent first, like `docker ps`.
    cs.sort_by_key(|c| std::cmp::Reverse(c.created_unix));
    // Reconcile + apply the `--all` filter once, then render in the chosen format.
    // `update` (flock) and not `save`: the CRI is concurrent and may be
    // reconciling the same container right now — see `Store::update`.
    let mut included: Vec<Container> = Vec::new();
    for mut c in cs {
        reconcile_with_diagnostics(store, &mut c);
        let hidden = matches!(c.status, Status::Failed(_) | Status::Crashed);
        if !all && hidden {
            continue;
        }
        included.push(c);
    }
    let uptime_of = |c: &Container| match c.status {
        Status::Running | Status::Paused => c.pid_starttime.and_then(output::uptime_from_starttime),
        _ => None,
    };
    let restarts = restart_counts(&super::util::state_root());
    if format == super::output::OutputFormat::Json {
        let rows: Vec<ContainerLsRow> = included
            .iter()
            .map(|c| ContainerLsRow {
                id: c.id.clone(),
                image: output::display_ref(&c.image),
                command: c.command.join(" "),
                created_unix: c.created_unix,
                status: fmt_status_of(c, uptime_of(c)),
                ports: fmt_ports(&c.ports),
                name: c.name.clone(),
                restarts: restarts.get(&c.id).copied().unwrap_or(0),
                size_bytes: size
                    .then(|| container_writable_dir(images, &c.id))
                    .flatten()
                    .map(|dir| super::volume::measured_usage(&dir))
                    .and_then(|u| u.is_complete().then_some(u.bytes)),
            })
            .collect();
        return output::print_json(&rows);
    }
    if quiet {
        for c in &included {
            println!("{}", short_id(&c.id));
        }
        return Ok(());
    }
    let mut headers: Vec<&str> = vec![
        "CONTAINER ID",
        "IMAGE",
        "COMMAND",
        "CREATED",
        "STATUS",
        "PORTS",
        "RESTARTS",
        "NAMES",
    ];
    if size {
        headers.push("SIZE");
    }
    // Last, and it hides itself on a host that uses no namespaces — see
    // `output::namespace_cell`. Until it existed, the boundary the engine
    // enforces in nftables was invisible in the listing an operator reads
    // most.
    headers.push("NAMESPACE");
    let mut t = output::Table::new(&headers);
    for c in &included {
        let mut row = vec![
            short_id(&c.id).to_string(),
            // `display_ref` strips the `@sha256:…` when there's a tag: a
            // `kindest/node:v1.34.0@sha256:7416a61b…` (84 chars) pushed all the
            // columns off the screen and the digest says nothing to the reader.
            output::truncate(&output::display_ref(&c.image), 30),
            output::truncate(&format!("\"{}\"", c.command.join(" ")), 22),
            output::fmt_age(c.created_unix),
            fmt_status_of(c, uptime_of(c)),
            output::truncate(&fmt_ports(&c.ports), 28),
            restarts.get(&c.id).copied().unwrap_or(0).to_string(),
            c.name.clone(),
        ];
        if size {
            let cell = container_writable_dir(images, &c.id)
                .map(|dir| super::volume::measured_usage(&dir))
                .map(|u| super::volume::fmt_size_cell(&u))
                .unwrap_or_else(|| "-".to_string());
            row.push(cell);
        }
        row.push(output::namespace_cell(&c.namespace, namespace.is_some()));
        t.row(row);
    }
    // Drops NAMESPACE on a host where every row would say `default` — the
    // column exists to show a boundary, and a column of dashes shows none.
    t.drop_uninformative().print();
    Ok(())
}

/// Apply `f` to each ID, continuing with the rest if one fails (docker
/// semantics: `rm a b c` removes what it can and returns the first error at the end).
fn for_each_id(ids: &[String], mut f: impl FnMut(&str) -> Result<()>) -> Result<()> {
    // Same reasoning as the i18n note below: exiting here bypasses `main.rs`, so
    // the exit-code classification has to be applied here too or a batched
    // command would answer 1 where the same command with ONE id answers 4.
    let mut codes: Vec<i32> = Vec::new();
    for id in ids {
        if let Err(e) = f(id) {
            // Each failure exits HERE with the id's context; returning the error made
            // main print it a second time, without context (duplicated message).
            // BUG FIXED (i18n gap): this bypassed `main.rs`'s error printer entirely
            // (exits before `run()` ever returns), so it never went through
            // `po::t_dyn` — every batched `stop`/`rm`/... failure stayed in EN
            // even under `--l18n=pt`, unlike a single-id failure of the same command.
            eprintln!("{id}: {}", super::po::t_dyn(&e.to_string()));
            codes.push(super::exitcode::for_error(&e));
        }
    }
    if !codes.is_empty() {
        std::process::exit(super::exitcode::merge(&codes));
    }
    Ok(())
}

/// Does the policy require supervision? (`no` needs no supervisor at all.)
/// Validates `--cpu-weight`/`--io-weight` against the range their own `--help`
/// promises (1–10000, which is also the kernel's).
///
/// Measured 2026-09-07: `0`, `99999` and `abc` were all accepted with rc=0 and
/// all produced `cpu.weight = 100` — the kernel default. The flag documented a
/// range it did not enforce, and out-of-range meant "ignored", which reads as
/// "applied" from the outside.
pub(crate) fn parse_cgroup_weight(s: &str) -> std::result::Result<String, String> {
    match s.parse::<u32>() {
        Ok(n) if (1..=10_000).contains(&n) => Ok(s.to_string()),
        _ => Err(format!("'{s}': weight must be a number from 1 to 10000")),
    }
}

/// Validates `--restart` at the CLI boundary, so a typo is refused instead of
/// producing a container that never comes back.
///
/// Measured 2026-09-07: `--restart talvez` was ACCEPTED (rc=0) and left
/// `restart_policy: null` in the record — `policy_supervised` does not recognise
/// it, so no policy was stored and nothing ever restarted the container. A
/// misspelling (`alwyas`, `on-failre`) produced a service that never comes back
/// up, silently, while the operator believed a supervisor was configured. Of
/// everything in that class this is the one with the sharpest edge: the others
/// waste resources, this one loses availability.
///
/// A plain `ValueEnum` cannot express it — `on-failure` takes an optional
/// `:max` — so the grammar is checked here, in one place, and the message lists
/// what is valid rather than only saying what was not.
pub(crate) fn parse_restart_policy(s: &str) -> std::result::Result<String, String> {
    let (name, max) = match s.split_once(':') {
        Some((n, m)) => (n, Some(m)),
        None => (s, None),
    };
    match (name, max) {
        ("no" | "always" | "unless-stopped", None) => Ok(s.to_string()),
        ("on-failure", None) => Ok(s.to_string()),
        // `on-failure:0` would mean "retry zero times", which is `no` written the
        // long way; refusing it keeps one spelling per meaning.
        ("on-failure", Some(m)) => match m.parse::<u32>() {
            Ok(n) if n > 0 => Ok(s.to_string()),
            _ => Err(format!(
                "'{s}': the retry count after `on-failure:` must be a positive number (e.g. on-failure:3)"
            )),
        },
        ("no" | "always" | "unless-stopped", Some(_)) => Err(format!(
            "'{s}': only `on-failure` takes a `:max` (e.g. on-failure:3)"
        )),
        _ => Err(format!(
            "'{s}': not a restart policy — use no, always, unless-stopped, on-failure or on-failure:<max>"
        )),
    }
}

#[cfg(test)]
pub(crate) use delonix_compute::launch::policy_supervised;

// Should this `run` fork a supervisor?
//
// **This is what makes a detached container's exit code knowable at all.**
// `waitpid` is the only source of a real exit status and the kernel grants it
// to the PARENT alone. A plain `run -d` had no lasting parent: the CLI exited,
// the container was reparented to `init`, and the status died with it — so
// `ps -a` could only ever say `Exited (unknown)` and `wait` had to refuse. Any
// CI job, migration, backup or health probe driven through that path could not
// tell success from failure.
//
// The supervisor already existed; it was simply gated on a restart policy. With
// no policy `should_restart` returns `false`, so it does exactly one useful
// thing — `wait_and_record` the true code, emit `die`, exit — and costs one
// short-lived process that goes away with the container.
//
// **This does not cross the daemonless line, despite appearances.** Daemonless
// here means *no central daemon* — no `dockerd` that owns every container and
// takes them all down with it. A supervisor per container is the standard
// daemonless design: Podman is daemonless and runs a `conmon` per container for
// this precise reason. This engine already keeps persistent per-node processes
// (the netns holder, slirp) and already forked this very supervisor for
// `--restart`.
//
// `forkable` is the one real constraint. `run_supervised` does a bare `fork()`
// of a process it assumes is single-threaded; that holds for the CLI and NOT
// for the `serve docker-api` server, which is why that path already refuses
// `--restart`. It keeps the old behaviour rather than risking a fork from a
// multi-threaded process — an honest, documented gap instead of a crash.
pub(crate) use delonix_compute::launch::should_supervise;

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
        message: super::po::t("ingress infra is down — no holder to enter").into(),
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
    // BUG FOUND: `std::fs::write` creates the file at the ambient umask
    // (typically 0644, world-readable). `opts.env` carries the raw `-e
    // KEY=VALUE` pairs the user passed — commonly credentials — and for a
    // FOREGROUND container this file sits on disk for the container's whole
    // lifetime (the spawned child blocks until the container exits). Fixed
    // with the same `create_new` (O_EXCL, no symlink-follow) + 0600 pattern
    // already used for the libvirt network XML temp file.
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = std::fs::remove_file(&spec_path); // leftover OF OURS from a crashed previous run
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&spec_path)?;
        f.write_all(json.as_bytes())?;
    }
    let status = std::process::Command::new(&prefix[0])
        .args(&prefix[1..])
        .arg(&exe)
        .args(["netns", "run"])
        .arg(&spec_path)
        .envs(reexec_env(id, ip))
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
        // For a FOREGROUND run the 2nd pass IS the container's parent, so the code
        // it exits with is the CONTAINER's own code (see `propagate_exit_status`,
        // which the inner `cmd_run` calls). Flattening that into a generic error
        // meant `run --net <net> sh -c "exit 42"` came back as 1 — better than the
        // 0 it used to be before exit codes propagated at all, but still not the
        // number the workload chose, so a job runner on a custom network still
        // could not tell one failure from another. Detached runs keep the old
        // message: there the child only ever reports whether the START worked.
        if !opts.detach {
            propagate_exit_status(&Status::Failed(status.code().unwrap_or(1)));
        }
        return Err(Error::Invalid(super::po::tf(
            "the container did not start inside the network '{netns}' (exit {code})",
            &[("netns", netns), ("code", &format!("{:?}", status.code()))],
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

/// EVERY IP a container holds on the SDN: the primary first, then one per additional
/// network (multi-homing). This is what the firewall has to be keyed on — keying it on
/// `c.ip` alone left every extra network ungoverned, so `ingress policy deny` blocked the
/// primary address while the container answered normally on the second one.
pub(crate) fn container_ips(c: &Container) -> Vec<String> {
    let mut ips: Vec<String> = c.ip.iter().filter(|s| !s.is_empty()).cloned().collect();
    ips.extend(
        c.extra_networks
            .iter()
            .map(|e| e.ip.clone())
            .filter(|s| !s.is_empty()),
    );
    ips
}

/// Applies `fw` over ALL of the container's IPs (see [`container_ips`]).
pub(crate) fn apply_firewall_everywhere(
    c: &Container,
    fw: &delonix_runtime_core::ContainerFw,
) -> Result<()> {
    let ips = container_ips(c);
    let refs: Vec<&str> = ips.iter().map(|s| s.as_str()).collect();
    infra::apply_firewall_all(&c.id, &refs, fw)
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

/// Host-level "is this port already held by something delonix doesn't track"
/// check — factored out of `cmd_run`'s preflight loop so it's unit-testable on
/// its own, without spinning up a whole container. `None` = free to bind (as far
/// as a busy-port conflict goes; privilege and `port_owner` are separate checks
/// upstream of this one). Structured like the sibling errors in this preflight:
/// the fact first, then ready-to-copy ways out.
fn host_port_conflict_error(hp: &str, cp: &str, addr: Option<&str>) -> Option<Error> {
    let p = hp.parse::<u16>().ok()?;
    let bind = delonix_net::publish_bind_addr(addr);
    if !delonix_net::host_port_busy(&bind, p) {
        return None;
    }
    let who =
        delonix_net::host_port_owner_process(p).unwrap_or_else(|| "another process".to_string());
    let alt = p as u32 + 10000;
    Some(Error::Invalid(super::po::tf(
        "port {hp} is already in use on the host by {who} — not a delonix container\n\
         \n\
         fix it with ONE of these:\n\
         \x20 delonix container run -p {alt}:{cp} ...    # publish on another port\n\
         \x20 ss -tlnp | grep :{hp}    # or find and stop whoever holds it",
        &[
            ("hp", hp),
            ("who", &who),
            ("alt", &alt.to_string()),
            ("cp", cp),
        ],
    )))
}

/// `container start` — restarts a stopped/crashed container with the spec stored
/// in the `Store` (command/env/mounts/network/ports) and the PERSISTENT rootfs
/// (rootless: the flat copy in `containers/<id>/rootfs`; root: remounts the
/// overlay, whose `upper` preserves the writes). It's what `rm`+`run` lacks: it
/// doesn't lose the state written inside the container.
pub(crate) fn cmd_start(images: &ImageStore, store: &Store, id: &str) -> Result<()> {
    let mut c = find(store, id)?;
    reconcile_with_diagnostics(store, &mut c);
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
            warn_if_namespace_isolation_inert(&c.namespace);
            // Re-register in the L7 proxy (`--expose`) HERE, on the host — the spawn via
            // nsenter doesn't run from the reexec'd process.
            if let Some(port) = c.expose {
                let _ = super::ingress_proxy::auto_register(&c.name, &c.namespace, &ip, port);
            }
            return reexec_start(&c.id, &netns, &ip, true);
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
            // Re-attach the ADDITIONAL networks. The record kept them across the stop,
            // but nothing ever replayed them: a multi-homed container came back with
            // `eth1` simply gone while `describe` still listed the network — a service
            // reachable only over that second network broke on the first restart, in
            // silence. The IP is re-requested from IPAM under the same container id, so
            // it normally comes back identical; if it differs, the record is corrected
            // rather than left pointing at an address that no longer exists.
            let extras = c.extra_networks.clone();
            for en in &extras {
                match infra::attach_extra_container(&c.id, en.idx, &en.network, &c.namespace) {
                    Ok((_ifname, new_ip)) if new_ip != en.ip => {
                        let (net, ip2) = (en.network.clone(), new_ip.clone());
                        if let Some(slot) = c
                            .extra_networks
                            .iter_mut()
                            .find(|x| x.network == en.network)
                        {
                            slot.ip = new_ip;
                        }
                        let _ = store.update(&c.id, |cur| {
                            match cur.extra_networks.iter_mut().find(|x| x.network == net) {
                                Some(s) => {
                                    s.ip = ip2.clone();
                                    true
                                }
                                None => false,
                            }
                        });
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!(
                        "{}",
                        super::po::tf(
                            "warning: network '{net}' of '{name}' not reattached on start: {e}",
                            &[
                                ("net", &en.network),
                                ("name", &c.name),
                                ("e", &e.to_string()),
                            ],
                        )
                    ),
                }
            }
            // Re-applies the persisted firewall (namespace isolation, Dependency,
            // Ingress) — the nft chain lives in the holder's EPHEMERAL netns, so a
            // restarted container would lose the isolation without this. Best-effort.
            // Keyed on EVERY IP (primary + extras), otherwise the additional networks
            // come back ungoverned.
            if let Some(fw) = &c.firewall {
                if fw.enabled {
                    if let Err(e) = apply_firewall_everywhere(&c, fw) {
                        eprintln!(
                            "{}",
                            super::po::tf(
                                "warning: firewall/isolation of '{name}' not reapplied on start: {e}",
                                &[("name", &c.name), ("e", &e.to_string())],
                            )
                        );
                    }
                }
            }
        }
    }

    // POD MEMBER: re-enter the pod's SHARED netns, the same two-pass re-exec the
    // custom-network branch above uses — but pointed at the pod's netns instead of
    // one named after this container.
    //
    // Two cases, and telling them apart is what makes this safe:
    //
    //   * the holder still serves the pod's netns (a peer is alive, or this is a
    //     lone member being restarted) — re-enter it and touch nothing else.
    //     Re-attaching here would DESTROY the netns and take the peers' network
    //     with it, which is the failure this guard exists to prevent;
    //   * the holder came back and the netns died with the old one — recreate it,
    //     with the member's own namespace so the isolation comes back with it.
    //
    // Before this, a pod member had no path back at all: `restart` rebuilt a spec
    // that could not work and died with `clone failed: EPERM`, leaving it `Dead`.
    if let Some(pn) = c.pod.clone() {
        if !reexec {
            if !infra::holder_serves_netns(&pn) {
                let (_, ip) = infra::attach_container(&pn, "ingress", &c.namespace)?;
                super::pod::apply_pod_namespace_isolation(&pn, &ip, &c.namespace);
            }
            let ip = infra::container_ip(&pn);
            return reexec_start(&c.id, &pn, &ip, false);
        }
        // Deliberately NOT setting `c.ip` here: `cmd_run` leaves a pod member's
        // record without one (the address belongs to the pod's netns, not to the
        // member), and a restarted member reporting an IP that a freshly-run one
        // does not would be a new inconsistency, not a fix.
    }

    let rootfs = if runtime::is_rootless() {
        // A container created before the layers were shared still carries a full
        // private copy of the image. This is the ONE moment it can be converted:
        // the tree is nobody's while the container is down, and a running one
        // cannot be converted at all (its process is pivot_root'ed into it).
        // Best-effort by construction — see `migrate_flat_to_overlay`; a start
        // never fails because of it.
        super::util::migrate_flat_to_overlay(images, &c.id, &c.image);
        // Overlay (`merged/`, remounted by the container's own init) or the
        // legacy flat copy — `existing_rootfs_path` knows which this container is.
        let rfs = images.existing_rootfs_path(&c.id).ok_or_else(|| {
            Error::Invalid(format!(
                "rootfs of {} no longer exists — use `run` again",
                c.name
            ))
        })?;
        rfs.to_string_lossy().into_owned()
    } else {
        let img = resolve_or_pull(images, &c.image)?;
        images
            .mount_rootfs(&img, &c.id)?
            .to_string_lossy()
            .into_owned()
    };

    // The same launch and the same spawn specification as `run` — one builder.
    // Two literals of `RunSpec` diverged SIX times (`-v`, `-p` on a custom network,
    // extra networks, pod membership, DNS, confinement), and a seventh was found
    // while unifying them: `start` ignored `--net none` (a container created with
    // no network came back with the HOST's, measured: 1 interface after `run`, 27
    // after `stop`+`start`) and `--log-file` (the new output went to the default
    // log while the record pointed at the requested one).
    let default_log = images
        .root()
        .join("containers")
        .join(&c.id)
        .join("log")
        .to_string_lossy()
        .into_owned();
    let launch = delonix_compute::launch::Launch {
        slirp_ports: delonix_compute::launch::slirp_ports(
            c.network.is_some(),
            c.pod.is_some(),
            &c.ports,
        ),
        rootfs,
        mounts: c.mounts.clone(),
        detach: true,
        second_pass: reexec,
        net_none: c.net_mode.as_deref() == Some("none"),
        custom_net: c.network.clone(),
        pod: c.pod.is_some(),
        attached_ip: c.ip.clone(),
        pod_infra_pid: None,
        apparmor: c.apparmor.clone(),
        log_path: Some(c.log_path.clone().unwrap_or(default_log)),
    };
    let ports = CliRunPorts { images, store };
    let policy = c.restart_policy.clone().unwrap_or_default();
    if should_supervise(&policy, true, true) {
        delonix_runtime_core::events::emit(
            &super::util::state_root(),
            "container",
            "start",
            &c.id,
            &c.name,
            None,
        );
        return delonix_compute::launch::WorkloadRuntime::supervise(
            &ports, &mut c, &launch, &policy,
        );
    }
    delonix_compute::launch::WorkloadRuntime::create(&ports, &mut c, &launch)?;
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

/// Env every re-exec pass needs. The 2nd pass runs inside the holder's userns with
/// our uid mapped to **0**, so ANY path this binary resolves from `geteuid()` diverges
/// there and has to be pinned explicitly — `DELONIX_ROOT` (state root) and the ingress
/// sockets' runtime dir (see `infra::runtime_dir_env`, which documents the live
/// v0.34.1 regression from pinning only the first of the two). One list, used by both
/// re-exec sites, so a third one can't be added missing half of it. PURE.
fn reexec_env(id: &str, ip: &str) -> Vec<(String, std::ffi::OsString)> {
    let (rt_var, rt_dir) = infra::runtime_dir_env();
    vec![
        ("DELONIX_REEXEC_ID".into(), id.into()),
        ("DELONIX_REEXEC_IP".into(), ip.into()),
        (
            "DELONIX_ROOT".into(),
            super::util::state_root().into_os_string(),
        ),
        (rt_var.into(), rt_dir.into_os_string()),
    ]
}

/// The 1st pass of `start` with a custom network: re-executes itself inside the
/// netns (see `reexec_into_netns`, same mechanism, no spec — the container
/// already exists in the store, the id is enough).
/// `owns_netns` says whether the netns belongs to THIS container. It does for a
/// custom network (`netns == sanitize(id)`); it does not for a pod member, whose
/// netns is the pod's and is shared with its peers — tearing it down on one
/// member's failed start would take the whole pod's network with it. Same
/// contract `cmd_run`'s `--pod` branch already states.
fn reexec_start(id: &str, netns: &str, ip: &str, owns_netns: bool) -> Result<()> {
    // BUG FIXED HERE: this used `join_argv(id)` and never read its own `netns`
    // parameter. It worked only because the sole caller passed a netns equal to
    // the id (the custom-network case), so the two were the same string. A pod
    // member is the first caller where they differ — and with the old code it
    // would have tried to enter a netns named after the container, which does not
    // exist. Same family as the dead-but-public helpers this repo has had to
    // delete twice: an argument accepted and ignored, with the defect waiting on
    // the first caller that made the difference visible.
    let prefix = infra::join_argv(netns).ok_or_else(|| Error::Runtime {
        context: "join_argv",
        message: super::po::t("ingress infra is down — no holder to enter").into(),
    })?;
    let exe = std::env::current_exe().map_err(|e| Error::Runtime {
        context: "current_exe",
        message: e.to_string(),
    })?;
    let status = std::process::Command::new(&prefix[0])
        .args(&prefix[1..])
        .arg(&exe)
        .args(["container", "start", id])
        .envs(reexec_env(id, ip))
        .status()
        .map_err(|e| Error::Runtime {
            context: "re-exec nsenter",
            message: e.to_string(),
        })?;
    if !status.success() {
        // Only tear down a netns we OWN. For a pod member the netns is the pod's
        // and is shared with its peers — detaching it here would take their
        // network down too, and free an IPAM lease that is still in use, over one
        // member failing to come back.
        if owns_netns {
            infra::detach_container(id, ip);
        }
        return Err(Error::Invalid(super::po::tf(
            "the container did not restart inside the network '{netns}' (exit {code})",
            &[("netns", netns), ("code", &format!("{:?}", status.code()))],
        )));
    }
    Ok(())
}

pub(crate) fn cmd_stop(store: &Store, id: &str, time: u64) -> Result<()> {
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

/// Parses a `docker kill -s` style signal spec into a raw signal number: a
/// bare number (`9`) or a name, case-insensitive, with or without the `SIG`
/// prefix (`kill`, `KILL`, `SIGKILL` all mean the same thing). Covers the
/// standard POSIX signals a container is realistically sent — an unknown name
/// is a clear error, never a silent no-op.
fn parse_signal(spec: &str) -> Result<i32> {
    if let Ok(n) = spec.parse::<i32>() {
        return Ok(n);
    }
    let upper = spec.to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    Ok(match name {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "ILL" => libc::SIGILL,
        "TRAP" => libc::SIGTRAP,
        "ABRT" => libc::SIGABRT,
        "BUS" => libc::SIGBUS,
        "FPE" => libc::SIGFPE,
        "KILL" => libc::SIGKILL,
        "USR1" => libc::SIGUSR1,
        "SEGV" => libc::SIGSEGV,
        "USR2" => libc::SIGUSR2,
        "PIPE" => libc::SIGPIPE,
        "ALRM" => libc::SIGALRM,
        "TERM" => libc::SIGTERM,
        "CHLD" => libc::SIGCHLD,
        "CONT" => libc::SIGCONT,
        "STOP" => libc::SIGSTOP,
        "TSTP" => libc::SIGTSTP,
        "TTIN" => libc::SIGTTIN,
        "TTOU" => libc::SIGTTOU,
        "URG" => libc::SIGURG,
        "XCPU" => libc::SIGXCPU,
        "XFSZ" => libc::SIGXFSZ,
        "VTALRM" => libc::SIGVTALRM,
        "PROF" => libc::SIGPROF,
        "WINCH" => libc::SIGWINCH,
        "IO" => libc::SIGIO,
        "SYS" => libc::SIGSYS,
        _ => return Err(Error::Invalid(format!("unknown signal: '{spec}'"))),
    })
}

pub(crate) fn cmd_kill(store: &Store, id: &str, signal: &str) -> Result<()> {
    let sig = parse_signal(signal)?;
    let c = find(store, id)?;
    runtime::send_signal(&c, sig)?;
    delonix_runtime_core::events::emit(
        &super::util::state_root(),
        "container",
        "kill",
        &c.id,
        &c.name,
        Some(signal),
    );
    println!("{}", c.id);
    Ok(())
}

/// Blocks until the container is no longer `Running`/`Paused`, then returns its
/// exit code — same 3 values `Status::exit_code()` already models (0 = clean,
/// N = `Failed(N)`, 137 = `Crashed`, which covers both OOM and a violent signal
/// death; this engine has no captured-real-signal path, same documented
/// limitation as everywhere else `crash_reason` is used instead of a real
/// waitpid status). Extracted from `cmd_wait` so `cmd::dockerapi`'s `POST
/// /containers/{id}/wait` can reuse the exact same polling logic and get the
/// code back as a value instead of parsing it off stdout.
pub(crate) fn wait_for_exit(store: &Store, id: &str) -> Result<i32> {
    loop {
        let mut c = find(store, id)?;
        reconcile_with_diagnostics(store, &mut c);
        if c.status.is_terminal() {
            // FAIL LOUD instead of fabricating a code. `Status::exit_code()`
            // maps `Crashed` to 137, which is right when a signal really did
            // kill it — but a detached container that exited on its OWN is also
            // recorded as `Crashed` (nobody was its parent to `waitpid`), and
            // returning 137 for a clean `exit 43` is a lie in the one place an
            // orchestrator trusts absolutely. `on-failure`/CI cannot tell
            // success from failure from OOM if we invent the number.
            if exit_code_unknown(&c) {
                return Err(Error::Invalid(super::po::tf(
                    "exit code of '{name}' was not captured: this engine is not the process's \
                     parent when detached, so only THAT it died is known. To get the real code, \
                     run it in the FOREGROUND (the CLI is then the parent and exits with the \
                     container's own code), or detach it under a supervisor with `--restart \
                     on-failure` / `always` / `unless-stopped`.",
                    &[("name", &c.name)],
                )));
            }
            return Ok(c.status.exit_code());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

pub(crate) fn cmd_wait(store: &Store, id: &str) -> Result<()> {
    let code = wait_for_exit(store, id)?;
    println!("{code}");
    Ok(())
}

/// `stop` then `start` — reuses both wholesale rather than duplicating their
/// (fairly involved) network/namespace re-attach logic. Accepted trade-off:
/// prints 2 lines (one from each half) instead of docker's 1, since neither
/// helper is silence-able without touching either's own callers.
pub(crate) fn cmd_restart(images: &ImageStore, store: &Store, id: &str, time: u64) -> Result<()> {
    let c = find(store, id)?;
    if matches!(
        c.status,
        delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused
    ) {
        cmd_stop(store, id, time)?;
    }
    cmd_start(images, store, id)
}

pub(crate) fn cmd_rename(store: &Store, id: &str, new_name: &str) -> Result<()> {
    if new_name.trim().is_empty() {
        return Err(Error::Invalid("new name cannot be empty".into()));
    }
    if !valid_container_name(new_name) {
        return Err(Error::Invalid(format!(
            "invalid name: '{new_name}' (see the name restrictions on `container run --name`)"
        )));
    }
    let c = find(store, id)?;
    if store.load(new_name).is_ok() {
        return Err(Error::Invalid(format!(
            "a container named '{new_name}' already exists"
        )));
    }
    let old_name = c.name.clone();
    store.update(&c.id, |cur| {
        cur.name = new_name.to_string();
        true
    })?;
    delonix_runtime_core::events::emit(
        &super::util::state_root(),
        "container",
        "rename",
        &c.id,
        new_name,
        Some(&old_name),
    );
    println!("{new_name}");
    Ok(())
}

/// `docker port <container>` — published ports, `hostPort/proto -> containerPort`.
/// The address `container port` shows for a published port.
///
/// **The old answer was the literal `0.0.0.0` whenever the spec carried no
/// address, and this engine's default publish address is `127.0.0.1`.** So the
/// one command an operator reads to audit exposure claimed *every interface* for
/// a port bound to loopback only — measured 2026-09-10: `ss -tlnp` showed
/// `127.0.0.1:31060` for the same port `container port` printed as
/// `0.0.0.0:31060`. The justification on the line was "it is `docker port`'s
/// format, and scripts read it", but docker prints `0.0.0.0` because docker
/// really binds there; copying the FORM inverted the MEANING.
///
/// `fmt_ports`, forty lines up, already got this right for the `ps` column and
/// wrote down why — "inventing one would be an assertion of exposure that may be
/// false". Two readers of the same field with opposite rules, and the inventing
/// one was the auditing one.
///
/// The remaining caveat is stated rather than hidden: the effective address is
/// not persisted with the port, so this recomputes the same default the publish
/// path uses (`DELONIX_PUBLISH_ADDR`, else loopback). Changing that variable
/// between the `run` and this command would make it stale — a narrower gap than
/// a constant that is wrong by default.
pub(crate) fn published_addr_for_display(spec_addr: Option<String>) -> String {
    delonix_net::publish_bind_addr(spec_addr.as_deref())
}

pub(crate) fn cmd_port(store: &Store, id: &str) -> Result<()> {
    let c = find(store, id)?;
    if c.ports.is_empty() {
        return Ok(()); // docker prints nothing for a container with no published ports
    }
    for spec in &c.ports {
        // Mesma correcção do `fmt_ports`, e aqui a saída antiga era absurda em vez
        // de só enganadora: com `127.0.0.1:19555:80` o `split_once(':')` dava
        // `host_part = "127.0.0.1"` e imprimia-se **`19555:80/tcp -> 0.0.0.0:127.0.0.1`**
        // — um endereço que não existe, sobre um serviço restrito a loopback.
        if let Ok((addr, hp, cp, proto)) = delonix_net::parse_publish_addr(spec) {
            println!("{cp}/{proto} -> {}:{hp}", published_addr_for_display(addr));
        } else {
            println!("{spec}");
        }
    }
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
    purge_container_dir(images, &c.id);
    Ok(())
}

/// Deletes a container's directory, falling back to the mapped-userns remover
/// when the plain path cannot.
///
/// BUG FIXED HERE, found by the chaos harness and not by any test. `rm` left the
/// ENTIRE flat rootfs behind — measured at 39 MiB for a `redis:7-alpine`, ~1.2
/// GiB per 30 containers — while reporting success, with the record already
/// deleted so nothing pointed at the orphan any more. Root cause: EACCES.
/// A rootless container's tree contains directories the extraction left
/// read-only, and files written inside a mapped userns as SUBUIDs; the calling
/// uid cannot unlink through them, and `remove_dir_all`'s error was discarded.
///
/// This is the disk-pressure incident class this engine has already suffered
/// once (49 orphan rootfs directories, ~45 GiB, kubelet tainting the node).
///
/// The remedy already existed on both sides and was simply never wired:
/// `ImageStore::container_path` was added — its doc comment says "so `rm` can
/// remove it in a mapped userns (subuid files) via the runtime" — and
/// `remove_tree_mapped` is the runtime helper that does it, already used by
/// `volume rm` and `system prune`. `container_path` had **zero callers**: the
/// same "public API waiting for its first caller" trap this codebase has paid
/// for three times (`mount_live`, `set_net_rate`, `update_limits`), and here it
/// cost a silent disk leak.
///
/// Plain removal FIRST (cheap, no fork, and the only thing needed when there are
/// no subuids); the mapped re-exec only as the fallback.
fn purge_container_dir(images: &ImageStore, id: &str) {
    if images.remove_container_dir(id) {
        return;
    }
    let path = images.container_path(id);
    runtime::remove_tree_mapped(&path);
    if path.exists() {
        super::output::warn(&super::po::tf(
            "container directory {path} could not be removed — reclaim it with `delonix system prune`",
            &[("path", &path.display().to_string())],
        ));
    }
}

/// `container prune` — the container half of `system prune`, on its own.
///
/// It exists because the global prune was all-or-nothing: an operator who only
/// wanted the stopped containers gone also had every unused image and CAS blob
/// swept out from under them, with no way to say no to that half. The sweep
/// itself is not reimplemented here — it is the same `prune::sweep_containers`
/// that `system prune` calls, so the two can never disagree about what a
/// stopped container leaves behind.
fn cmd_prune(images: &ImageStore, store: &Store, force: bool) -> Result<()> {
    let doomed = super::prune::doomed_containers(store)?;
    let preview = (!doomed.is_empty()).then(|| {
        super::po::tf(
            "This will remove {n} stopped container(s): {list}",
            &[
                ("n", &doomed.len().to_string()),
                ("list", &doomed.join(", ")),
            ],
        )
    });
    if !super::prune::confirm(
        force,
        super::po::t(
            "`container prune` removes every stopped container — pass --force to confirm when not \
             on a terminal",
        ),
        preview,
        super::po::t(
            "Also removes orphan rootfs directories, empty cgroups and stale host ports. Continue? \
             [y/N]",
        ),
    )? {
        return Ok(());
    }

    let c = super::prune::sweep_containers(images, store)?;
    if c.slirps > 0 {
        println!(
            "{}",
            super::po::tf(
                "net: {n} orphan slirp(s) reaped",
                &[("n", &c.slirps.to_string())]
            )
        );
    }
    if c.refs > 0 {
        println!(
            "{}",
            super::po::tf(
                "net: {n} orphan ingress ref(s) reaped",
                &[("n", &c.refs.to_string())]
            )
        );
    }
    println!(
        "{}",
        super::po::tf(
            "removed: {c} container(s), {d} orphan dir(s), {g} cgroup(s), {p} orphan port(s) — \
             {size} freed",
            &[
                ("c", &c.containers.to_string()),
                ("d", &c.dirs.to_string()),
                ("g", &c.cgroups.to_string()),
                ("p", &c.ports.to_string()),
                ("size", &c.freed.fmt()),
            ]
        )
    );
    super::prune::note_partial(c.freed);
    Ok(())
}

pub(crate) fn cmd_rm(images: &ImageStore, store: &Store, id: &str, force: bool) -> Result<()> {
    let c = find(store, id)?;
    let pid = c.pid;
    runtime::remove(store, &c, force)?;
    unpublish_ports(&c, pid);
    // De-register from the L7 proxy (`--expose`) — removes the route + SIGHUP.
    //
    // UNCONDITIONAL on purpose. This used to be guarded by `c.expose.is_some()`, and the
    // guard made the call DEAD: `--expose` never reaches disk (measured — the re-exec spec
    // carries `expose: 8080` into the 2nd pass, but every persisted record has `expose:
    // null`; see `docs/discovery/46_GAPS_ENCONTRADOS.md` §4.1). So `rm` left the auto-route
    // behind, pointing at an SDN address that IPAM then hands to SOMEONE ELSE — the next
    // container to get that IP silently receives traffic addressed to the dead container's
    // `<name>.<namespace>.delonix.internal`.
    //
    // The guard was an optimization that bought nothing and could only cost correctness:
    // `auto_deregister` is already idempotent and its own contract is "if the container was
    // not registered, does nothing" — `with_auto_locked` short-circuits without writing (and
    // without rebuilding the proxy) when the route list is unchanged. Deriving a cleanup from
    // a record field is exactly the shape that failed here; the container's NAME is the key,
    // and we have it.
    super::ingress_proxy::auto_deregister(&c.name);
    let _ = images.unmount_rootfs(&c.id); // unmounts/cleans up the overlay scratch
                                          // Definitive DESTROY of the container's directory (including the flat `rootfs/`).
                                          // `unmount_rootfs` PRESERVES it on purpose (it's the container's state, for
                                          // `start` to reuse); only `rm` may delete it. Without this the rootfs was left
                                          // orphaned forever: 49 directories (45 GiB) piled up in a single test session,
                                          // and the kubelet marked the node with `disk-pressure`. The `purge_container_dir`
                                          // doc already said "called by `rm`" — but it wasn't.
    purge_container_dir(images, &c.id);
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_exec(
    images: &ImageStore,
    store: &Store,
    id: &str,
    interactive: bool,
    tty: bool,
    env: &[String],
    workdir: Option<&str>,
    user: Option<&str>,
    command: &[String],
) -> Result<()> {
    let c = find(store, id)?;
    // No command: the interactive-shell fallback. `container ssh` used to be a
    // separate verb for exactly this substitution — removed, folded in here
    // (docs/discovery/51_CLI_INVENTARIO.md §10.4: the name promised SSH and
    // delivered `exec -t` with a default command, which `exec` itself can do).
    //
    // `command -v bash`, never `exec bash 2>/dev/null || exec sh` (what
    // `container ssh` used to run): a failed `exec` in a non-interactive POSIX
    // shell terminates that shell on the spot instead of falling through to the
    // `||` — measured live against busybox and dash, not assumed. `container
    // ssh` carried this exact bug on any image without `bash` (alpine
    // included): the fallback it promised never ran. `command -v` only tests
    // for the binary, so the failure never reaches `exec`.
    let fallback;
    let command: &[String] = if command.is_empty() {
        fallback = vec![
            "/bin/sh".into(),
            "-c".into(),
            "command -v bash >/dev/null 2>&1 && exec bash || exec sh".into(),
        ];
        &fallback
    } else {
        command
    };
    let _ = interactive; // stdin is inherited; the flag keeps CLI parity
                         // `container_fs_root` and not a path built here: an `exec` targets a RUNNING
                         // container, so it resolves to `/proc/<pid>/root` — the merged tree as the
                         // container itself sees it, which is the only view that works whether the
                         // rootfs is an overlay or the legacy flat copy. A numeric `-u` never reaches
                         // this at all.
    let user_override = match user {
        Some(u) => {
            let root = container_fs_root(images, &c)?;
            let resolved = resolve_run_user(&root.path().to_string_lossy(), u)?;
            Some(resolved)
        }
        None => None,
    };
    // `--secret` (env mode) values are no longer baked into `c.env` — they are
    // resolved from the vault at spawn time and live only in the init process's
    // memory. An `exec` therefore has to resolve them too, or a debugging shell
    // would silently see a DIFFERENT environment from the container's own process
    // (docker's `exec` inherits the container's env). `extra_env` is exactly the
    // right channel: applied on top for this call only, never persisted.
    //
    // The explicit `-e` comes LAST so it still wins over a vault value, matching
    // the precedence the flag already had.
    let mut extra_env: Vec<String> = Vec::new();
    if !c.secret_files && !c.secrets.is_empty() {
        if let Ok(ss) = delonix_runtime_core::SecretStore::open(super::util::state_root()) {
            extra_env.extend(ss.resolve_env(&c.secrets));
        }
    }
    extra_env.extend(env.iter().cloned());
    let overrides = runtime::ExecOverrides {
        extra_env: &extra_env,
        workdir,
        user: user_override,
    };
    let code = runtime::exec_with(&c, command, tty, &overrides)?;
    std::process::exit(code);
}

/// `container inspect` — dumps the full spec stored in the Store (the runtime's
/// source of truth), as a docker-style JSON array.
fn cmd_inspect(store: &Store, ids: &[String]) -> Result<()> {
    let mut cs = Vec::new();
    for id in ids {
        let mut c = find(store, id)?;
        reconcile_with_diagnostics(store, &mut c);
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
/// An earlier version elsewhere only did the overlay path and, in rootless,
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
        // `container_fs_root` resolves the three layouts AND, for an overlay
        // container, insists it be running: packing a stopped one would read an
        // empty `merged/` and publish an EMPTY image while reporting success —
        // the same dishonest-report class this engine treats as its worst bug.
        // A running container packs from `/proc/<pid>/root`, which is the merged
        // tree including every write the container has made.
        let rootfs = container_fs_root(images, &c)?;
        images.commit_flat_rootfs(
            rootfs.path(),
            c.command.clone(),
            Vec::new(),
            c.env.clone(),
            c.workdir.clone().unwrap_or_default(),
            String::new(),
            tag,
            &base.config.architecture,
            // `container commit` herda o health check da base, tal como o
            // caminho overlay (`commit_container`) já fazia.
            base.config.healthcheck.clone(),
        )?
    } else {
        let layer = images.commit_upper(&c.id)?; // tar of the upperdir → CAS
        images.commit_container(&base, layer, c.command.clone(), c.env.clone(), tag)?
    };
    println!("{}  {}", img.short_id(), img.repo_tags.join(", "));
    Ok(())
}

/// `container healthcheck` — runs the image's `HEALTHCHECK` inside it.
/// Exits with 1 on `unhealthy`, to serve as a gate in scripts/CI.
fn cmd_healthcheck(images: &ImageStore, store: &Store, id: &str) -> Result<()> {
    let c = find(store, id)?;
    // `health_command` and not the image's `HEALTHCHECK` alone — the container's
    // own `--health-cmd` wins, exactly as it does for the monitor that writes the
    // `(healthy)`/`(unhealthy)` column. Reading only the image made this verb
    // refuse the one container that is DEFINITELY being health-checked: a
    // `run --health-cmd ... alpine` showed `Up (healthy)` in `ps` while
    // `container healthcheck <it>` answered `image 'alpine:3.20' defines no
    // HEALTHCHECK` and exited 1 — a permanent red in the CI script this verb
    // exists for. The resolution order was already written down one function
    // below; this caller was the one that never used it.
    let hc = health_command(images, &c).ok_or_else(|| {
        Error::Invalid(super::po::tf(
            "no health probe for '{name}': the container has no --health-cmd and image '{image}' defines no HEALTHCHECK",
            &[("name", &c.name), ("image", &c.image)],
        ))
    })?;
    if !c.is_live() {
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
    if !c.is_live() {
        return Err(Error::NotRunning(short_id(&c.id).to_string()));
    }
    // `Container::cgroup()` is the path the engine TRIED to use
    // (`<slice>/delonix-<id>`); in rootless without delegation the container isn't
    // there. We read init's REAL cgroup from `/proc/<pid>/cgroup` — the same
    // technique as the `cgroup_metric` that `stats` already uses, and which works
    // whatever the delegated base is. An earlier version used the guessed path and
    // gave "cgroup.procs: No such file or directory" on any host without delegation.
    let pid = c
        .pid
        .ok_or_else(|| Error::NotRunning(short_id(&c.id).to_string()))?;
    let procs = cgroup_metric(pid, "cgroup.procs").ok_or_else(|| {
        Error::Invalid(super::po::tf(
            "cannot read cgroup.procs of '{name}' — the container's cgroup is not accessible (rootless without delegation?)",
            &[("name", &c.name)],
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
        return Err(Error::Invalid(super::po::tf(
            "'{name}' has no overlay upperdir — `diff` compares the overlay with the image, and in rootless the rootfs is flat",
            &[("name", &c.name)],
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
/// A container's filesystem root, together with anything that has to stay alive
/// for that path to keep meaning something.
///
/// The `hold` is the `__ovlhold` process for a STOPPED overlay container: the
/// merged tree exists only inside its mount namespace, and we reach it through
/// `/proc/<pid>/root`. Dropping this unmounts the overlay by killing its holder,
/// which is why the value has to outlive every use of the path — hence a guard
/// and not a bare `PathBuf`. `HeldChild` kills and reaps in its own `Drop`, so
/// this struct needs none of its own.
struct FsRoot {
    path: std::path::PathBuf,
    /// Underscored because nothing ever READS it: it exists purely so its
    /// `Drop` runs when this struct dies, which is what unmounts the overlay.
    _hold: Option<runtime::HeldChild>,
}

impl FsRoot {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

fn container_fs_root(images: &ImageStore, c: &Container) -> Result<FsRoot> {
    if let Some(pid) = c.pid.filter(|_| c.is_live()) {
        return Ok(FsRoot {
            path: std::path::PathBuf::from(format!("/proc/{pid}/root")),
            _hold: None,
        });
    }
    let dir = images.root().join("containers").join(&c.id);
    // A STOPPED overlay container has no readable tree from out here: `merged/`
    // is an empty directory until something mounts the overlay, and an
    // unprivileged `mount(2)` on the host is EPERM. So we borrow the same door
    // the running case uses — a process that holds the mount in its own
    // namespace, read through `/proc/<pid>/root`.
    //
    // Returning the empty `merged/` would be the worst possible answer: `cp`
    // would copy nothing and report success.
    if dir.join(delonix_image::ImageStore::LOWERS_FILE).exists() {
        let hold = runtime::reexec_mapped_hold(&["__ovlhold", &dir.to_string_lossy()])
            .ok_or_else(|| {
                Error::Invalid(super::po::tf(
                    "could not mount the overlay of the stopped container '{name}' to read it — start it (`delonix container start {name}`)",
                    &[("name", &c.name)],
                ))
            })?;
        let pid = hold.pid();
        return Ok(FsRoot {
            path: std::path::PathBuf::from(format!(
                "/proc/{pid}/root{}",
                dir.join("merged").display()
            )),
            _hold: Some(hold),
        });
    }
    for cand in ["merged", "rootfs"] {
        let p = dir.join(cand);
        if p.exists() {
            return Ok(FsRoot {
                path: p,
                _hold: None,
            });
        }
    }
    Err(Error::Invalid(super::po::tf(
        "container '{name}' stopped and with no rootfs on disk — start it (`delonix container start {name}`)",
        &[("name", &c.name)],
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
pub(crate) fn split_cp_arg(s: &str) -> Option<(String, String)> {
    let colon = s.find(':')?;
    if s[..colon].is_empty() || s[..colon].contains('/') {
        return None;
    }
    Some((s[..colon].to_string(), s[colon + 1..].to_string()))
}

/// `container cp` — copies host↔container. Exactly one side is `container:/path`.
pub(crate) fn cmd_cp(images: &ImageStore, store: &Store, src: &str, dst: &str) -> Result<()> {
    let join_root = |root: &std::path::Path, p: &str| root.join(p.trim_start_matches('/'));
    match (split_cp_arg(src), split_cp_arg(dst)) {
        (Some((name, cpath)), None) => {
            let c = find(store, &name)?;
            let root = container_fs_root(images, &c)?;
            copy_recursive(&join_root(root.path(), &cpath), std::path::Path::new(dst))
                .map_err(|e| Error::Invalid(format!("cp: {e}")))?;
        }
        (None, Some((name, cpath))) => {
            let c = find(store, &name)?;
            let root = container_fs_root(images, &c)?;
            copy_recursive(std::path::Path::new(src), &join_root(root.path(), &cpath))
                .map_err(|e| Error::Invalid(format!("cp: {e}")))?;
        }
        _ => {
            return Err(Error::Invalid(
                super::po::t(
                    "usage: delonix container cp <SRC> <DST> — exactly one of the sides is `container:/path`",
                )
                .into(),
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
        reconcile_with_diagnostics(store, &mut c);
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
    // Always shown, `default` included. It decides who can reach this container (the
    // `@dlxns_<ns>` accept + the cross-namespace `ct new` drop), and `vm describe` has
    // printed it since namespaces landed — omitting it here made the isolation boundary
    // invisible on the resource that uses it most.
    d.field("Namespace", &c.namespace);
    d.field(k::IMAGE, &c.image);
    d.field("Command", c.command.join(" "));
    d.field_opt("Workdir", c.workdir.as_deref());
    d.field("Created", output::fmt_local(c.created_unix));

    let uptime = match c.status {
        Status::Running | Status::Paused => c.pid_starttime.and_then(output::uptime_from_starttime),
        _ => None,
    };
    d.field("Status", fmt_status_of(c, uptime));
    match c.pid {
        Some(p) => d.field("PID", p.to_string()),
        None => d.field("PID", "<none>"),
    };
    if let Some(reason) = &c.crash_reason {
        d.field("Crash reason", reason);
        if let Some(ts) = c.crashed_at {
            d.field("Crashed at", output::fmt_local(ts));
        }
    }
    d.field_opt(k::POD, c.pod.as_deref());

    d.section("Resources");
    d.sub("CPUs", &c.cpus);
    d.sub("Memory", &c.memory_max);
    d.sub_opt("CPU weight", c.cpu_weight.as_deref());
    d.sub_opt("Cpuset", c.cpuset.as_deref());
    d.sub_opt("IO weight", c.io_weight.as_deref());
    d.sub_opt("Nice", c.nice.map(|n| n.to_string()));

    d.section(k::NETWORK);
    // Diz a VERDADE sobre os três casos que `network: None` esconde: pedi
    // host, pedi none, ou pedi uma rede e fiquei sem ela. O terceiro é o que
    // interessa — um contentor assim está vivo, sem resolução de nomes e sem
    // rota para os pares, e antes disto aparecia aqui como um `host` normal.
    d.sub("Mode", net_mode_display(c));
    d.sub("IP", c.ip.as_deref().unwrap_or("<none>"));
    if !c.extra_networks.is_empty() {
        d.sub(
            "Extra",
            c.extra_networks
                .iter()
                .map(|n| format!("{} ({} on eth{})", n.network, n.ip, n.idx))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if !c.net_aliases.is_empty() {
        d.sub("Aliases", c.net_aliases.join(", "));
    }
    // Sendo a persistência o ponto desta funcionalidade, tem de haver forma de
    // confirmar o que ficou gravado sem ler o JSON à mão.
    if let Some(hc) = &c.health {
        let probe = if hc.cmd.is_empty() {
            "<image HEALTHCHECK>".to_string()
        } else {
            hc.cmd.clone()
        };
        d.sub("Health probe", probe);
        d.sub(
            "Probe policy",
            format!(
                "every {}s, timeout {}s, {} retries, {}s start period",
                hc.interval_secs, hc.timeout_secs, hc.retries, hc.start_period_secs
            ),
        );
    }
    if let Some(h) = &c.health_state {
        let when = if h.checked_unix > 0 {
            delonix_runtime_core::fmt_local_ts(h.checked_unix as u64)
        } else {
            "never".to_string()
        };
        d.sub(
            "Health",
            format!(
                "{} (last exit {}, {} consecutive failure(s), checked {when})",
                h.health, h.last_exit, h.failing_streak
            ),
        );
    }
    if !c.extra_hosts.is_empty() {
        d.sub("Extra hosts", c.extra_hosts.join(", "));
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
#[derive(Default)]
pub(crate) struct UpdateOpts {
    pub(crate) publish_add: Vec<String>,
    pub(crate) publish_rm: Vec<String>,
    pub(crate) volume_add: Vec<String>,
    pub(crate) volume_rm: Vec<String>,
    pub(crate) net_rate: Option<String>,
    pub(crate) net_burst: Option<String>,
    pub(crate) net_rate_clear: bool,
    pub(crate) memory: Option<String>,
    pub(crate) cpus: Option<String>,
}

impl UpdateOpts {
    fn is_empty(&self) -> bool {
        self.publish_add.is_empty()
            && self.publish_rm.is_empty()
            && self.volume_add.is_empty()
            && self.volume_rm.is_empty()
            && self.net_rate.is_none()
            && !self.net_rate_clear
            && self.memory.is_none()
            && self.cpus.is_none()
    }

    /// `--net-burst` alone: a burst without a rate configures nothing.
    ///
    /// It is NOT counted in `is_empty` on purpose — it is not a change by
    /// itself. But answering it with «nothing to do: pass at least one change»
    /// tells someone who DID pass a flag that they passed none, and the list of
    /// suggestions did not even mention `--net-burst`. `run` already answers
    /// this exact case to the point; the two should say the same thing.
    fn burst_without_rate(&self) -> bool {
        self.net_burst.is_some() && self.net_rate.is_none()
    }
}

/// Next free interface index for an additional network. `eth0` is always the
/// primary network, so the extras start at 1 — and we reuse holes left by a
/// `network disconnect` instead of always counting upward.
fn next_extra_idx(c: &Container) -> u32 {
    (1u32..)
        .find(|i| !c.extra_networks.iter().any(|n| n.idx == *i))
        .unwrap_or(1)
}

/// `network connect` — hot-attach a RUNNING container to an additional
/// network (multi-homing), keeping its primary network untouched.
///
/// Moved out of `container update --net-connect` (CLI Sprint 5): this is
/// Docker's own verb and argument order for network topology, kept separate
/// from `container update`'s ports/volumes/limits — which stay there because
/// Docker cannot reconfigure THOSE hot at all, so there is no upstream verb
/// to align with.
pub(crate) fn cmd_network_connect(store: &Store, id: &str, network: &str) -> Result<()> {
    let mut c = find(store, id)?;
    runtime::reconcile_status(&mut c);
    if !matches!(c.status, Status::Running | Status::Paused) {
        return Err(Error::Invalid(super::po::tf(
            "container '{name}' is not running ({status}) — connecting a network acts on the LIVE \
             process. Start it with `delonix container start {name}` first.",
            &[("name", &c.name), ("status", &c.status.to_string())],
        )));
    }
    if c.network.is_none() {
        return Err(Error::Invalid(super::po::tf(
            "'{name}' runs on the slirp-per-container path (--net host/none), which has no \
             holder-managed netns — connecting an additional network is only possible for \
             a container created with `--net <network>`",
            &[("name", &c.name)],
        )));
    }
    if c.extra_networks.iter().any(|n| n.network == network)
        || c.network.as_deref() == Some(network)
    {
        return Err(Error::Invalid(format!(
            "'{}' is already attached to network '{network}'",
            c.name
        )));
    }
    let idx = next_extra_idx(&c);
    let (ifname, ip) = infra::attach_extra_container(&c.id, idx, network, &c.namespace)?;
    let en = delonix_runtime_core::ExtraNet {
        network: network.to_string(),
        ip: ip.clone(),
        idx,
    };
    c = store.update(&c.id, |cur| {
        cur.extra_networks.push(en.clone());
        true
    })?;
    // The firewall has to be re-applied so the NEW IP is governed too — without this
    // the container gains an address that no `ingress`/`egress`/`Dependency` rule
    // reaches, which is exactly how a `policy deny` container stayed reachable over a
    // second network.
    if let Some(fw) = c.firewall.clone() {
        if let Err(e) = apply_firewall_everywhere(&c, &fw) {
            eprintln!("{}: firewall not extended to {ip}: {e}", c.name);
        }
    }
    println!(
        "{}: attached to network {network} — {ip} on {ifname}",
        c.name
    );
    Ok(())
}

/// `network disconnect` — the inverse of [`cmd_network_connect`]. Refuses a
/// network the container is not actually attached to as an EXTRA one — the
/// primary network (`c.network`) never shows up in `extra_networks`, so
/// naming it here lands in that same refusal, not a special case: it only
/// goes away with the container itself.
pub(crate) fn cmd_network_disconnect(store: &Store, id: &str, network: &str) -> Result<()> {
    let mut c = find(store, id)?;
    runtime::reconcile_status(&mut c);
    if !matches!(c.status, Status::Running | Status::Paused) {
        return Err(Error::Invalid(super::po::tf(
            "container '{name}' is not running ({status}) — disconnecting a network acts on the \
             LIVE process. Start it with `delonix container start {name}` first.",
            &[("name", &c.name), ("status", &c.status.to_string())],
        )));
    }
    let Some(en) = c
        .extra_networks
        .iter()
        .find(|n| n.network == network)
        .cloned()
    else {
        return Err(Error::Invalid(format!(
            "container '{}' is not attached to the extra network '{network}'",
            c.name
        )));
    };
    infra::detach_extra_container(&c.id, en.idx, &en.ip);
    c = store.update(&c.id, |cur| {
        let before = cur.extra_networks.len();
        cur.extra_networks.retain(|x| x.network != network);
        cur.extra_networks.len() != before
    })?;
    // Re-apply so the released IP loses its jumps: IPAM will hand that address to
    // another container, which must not inherit this one's firewall.
    if let Some(fw) = c.firewall.clone() {
        if let Err(e) = apply_firewall_everywhere(&c, &fw) {
            eprintln!("{}: firewall not re-applied after detach: {e}", c.name);
        }
    }
    println!(
        "{}: detached from network {network} (eth{})",
        c.name, en.idx
    );
    Ok(())
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
    // Checked BEFORE `is_empty`: someone who passed `--net-burst` alone did pass
    // a flag, and «pass at least one change» would be answering a question they
    // did not ask. Same sentence `run` gives for the same mistake.
    if o.burst_without_rate() {
        return Err(Error::Invalid(
            "--net-burst only makes sense together with --net-rate".into(),
        ));
    }
    if o.is_empty() {
        return Err(Error::Invalid("nothing to do: pass at least one change (--publish-add/--publish-rm/--volume-add/--volume-rm/--net-rate/--net-rate-clear/--memory/--cpus) — for network topology, see `network connect`/`network disconnect`".into()));
    }
    let mut c = find(store, id)?;
    runtime::reconcile_status(&mut c);
    if !matches!(c.status, Status::Running | Status::Paused) {
        return Err(Error::Invalid(super::po::tf(
            "container '{name}' is not running ({status}) — the hot update acts on the LIVE \
             process. Start it with `delonix container start {name}` first.",
            &[("name", &c.name), ("status", &c.status.to_string())],
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
    // --- additions ---
    // Ranges expand here too, so `update --publish-add 8000-8002:9000-9002` behaves
    // exactly like the same range on `run` — a flag that works in one place and not
    // the other is worse than one that exists nowhere.
    for spec in &o.publish_add {
        for one in delonix_net::expand_publish_range(spec)? {
            publish_live(store, &mut c, &one)?;
        }
    }
    for spec in &o.volume_add {
        let mounts = delonix_compute::ports::StorageProvider::resolve_mounts(
            &delonix_volume::HostVolumes {
                root: super::util::state_root(),
            },
            std::slice::from_ref(spec),
            &c.namespace,
        )?;
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
            return Err(Error::Invalid(super::po::tf(
                "'{name}' runs on the slirp-per-container path (--net host/none) — shaping is \
                 done on the ingress-side veth, which only exists for containers created with \
                 `--net <network>`",
                &[("name", &c.name)],
            )));
        }
        // The SAME parser `run` uses (`cmd_run`'s `--net-bps` path). It used to
        // be a private pair here, and the two had DRIFTED: this one read the
        // burst in decimal (`32kb` = 32 000) and `run` in binary (32 768), so
        // the same flag written the same way programmed a different bucket
        // depending on which command applied it. It also accepted a burst of
        // zero, which `run` refuses. One flag, one meaning.
        let parsed = delonix_net::parse_net_rate(rate, o.net_burst.as_deref())?;
        infra::set_net_rate(&c.id, parsed.rate_bit, parsed.burst_bytes)?;
        // Persist what the OPERATOR wrote, and `None` when they wrote nothing —
        // symmetric with `run`. Storing the computed number instead made the two
        // paths read differently for the same effective setting: `run` showed
        // `Rate limit: 10mbit` and `update` showed `10mbit (burst 125000)`, an
        // unitless number where the old default at least said `32kb`. Same
        // bucket in the kernel, two stories in `describe` — in the very commit
        // whose thesis is «one flag, one meaning».
        let burst_s = o.net_burst.clone();
        let (r, b) = (rate.clone(), burst_s.clone());
        store.update(&c.id, |cur| {
            cur.net_bps = Some(r.clone());
            cur.net_burst = b.clone();
            true
        })?;
        // What is REPORTED is the burst actually programmed — echoed verbatim
        // when the operator gave one, and formatted with a unit when it was
        // derived. A bare `125000` reads like a setting nobody chose.
        let burst_show = burst_s
            .clone()
            .unwrap_or_else(|| super::output::fmt_size(parsed.burst_bytes));
        println!(
            "{}",
            super::po::tf(
                "{name}: bandwidth limited to {rate} (burst {burst_s})",
                &[("name", &c.name), ("rate", rate), ("burst_s", &burst_show)],
            )
        );
    }

    // --- memory/CPU limits ---
    if o.memory.is_some() || o.cpus.is_some() {
        let aplicado = runtime::update_limits(&c, o.memory.as_deref(), o.cpus.as_deref())?;
        let (m, cp) = (o.memory.clone(), o.cpus.clone());
        store.update(&c.id, |cur| {
            if let Some(m) = &m {
                cur.memory_max = m.clone();
            }
            if let Some(cp) = &cp {
                cur.cpus = cp.clone();
            }
            true
        })?;
        // **Os três casos dizem três coisas diferentes.** Antes diziam a mesma, e a
        // pior delas: «updated» sobre um container a correr onde nada foi escrito,
        // com o registo já persistido — logo o `describe` a seguir confirmava um
        // limite que o kernel não conhece.
        let (memory, cpus) = (
            o.memory.as_deref().unwrap_or("unchanged"),
            o.cpus.as_deref().unwrap_or("unchanged"),
        );
        match aplicado {
            runtime::LimitUpdate::Applied => println!(
                "{}",
                super::po::tf(
                    "{name}: resource limits updated (memory={memory}, cpus={cpus})",
                    &[("name", &c.name), ("memory", memory), ("cpus", cpus)],
                )
            ),
            runtime::LimitUpdate::Deferred => println!(
                "{}",
                super::po::tf(
                    "{name}: recorded (memory={memory}, cpus={cpus}) — the container is \
                     stopped; they apply on the next `start`",
                    &[("name", &c.name), ("memory", memory), ("cpus", cpus)],
                )
            ),
            // Não é erro — o registo FOI actualizado e o próximo `start` aplica-o.
            // Mas não se chama «updated» a isto. E a mensagem NÃO nomeia uma causa:
            // a hipótese óbvia (falta de delegação) foi medida e está errada — sem
            // delegação a escrita é tentada e falha com `Permission denied`, que já
            // é honesto. Aqui o cgroup não existe de todo, tipicamente porque o
            // container está a morrer. Afirmar «configura a delegação» mandaria o
            // utilizador arranjar o que não está partido.
            // **Devolve ERRO, e a razão é o `converge`.** A primeira versão só
            // imprimia para stderr e devolvia `Ok(())` — mas o `store.update` acima
            // já persistiu o valor, e o `container::converge` do `stack apply`
            // delega neste mesmo caminho. Resultado: o apply carimbava o
            // `last-applied`, o `stack plan --detailed-exitcode` seguinte
            // respondia 0, e o gate de deriva do CI ficava verde por cima de uma
            // barreira de contenção que o kernel não conhece. Apanhado na
            // passagem `delonix-runtime-sec` a esta série.
            //
            // O registo FICA actualizado de propósito (o próximo `start`
            // reconstrói o cgroup a partir dele e aí o limite passa a valer); o
            // que não pode acontecer é o comando dizer que está feito.
            runtime::LimitUpdate::NotEnforced => {
                return Err(delonix_runtime_core::Error::Invalid(super::po::tf(
                    "{name}: recorded (memory={memory}, cpus={cpus}) but NOT enforced — \
                     the container is running and its cgroup no longer exists, so there \
                     was nowhere to write them. Check it is still healthy (`delonix \
                     container ps -a`); the limits apply on the next `start`.",
                    &[("name", &c.name), ("memory", memory), ("cpus", cpus)],
                )));
            }
        }
    }
    Ok(())
}

/// Publish a port on a LIVE container, by the right path for its network.
pub(crate) fn publish_live(store: &Store, c: &mut Container, spec: &str) -> Result<()> {
    let (host_addr, hp, cp, proto) = delonix_net::parse_publish_addr(spec)?;
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
                return Err(Error::Invalid(super::po::tf(
                    "'{name}' was created without `-p` and without `--net <network>`, so its \
                     slirp has no open api-socket — there's nowhere to publish a hot port. \
                     Publish at least one port in `run`, or use `--net <network>` (ingress \
                     always accepts hot publishes).",
                    &[("name", &c.name)],
                )));
            }
            delonix_net::slirp_add_hostfwd(&sock, &hp, &cp, &proto, host_addr.as_deref())?;
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
    // EVERY publication on this host port, not just the first: `-p 53:53/tcp -p
    // 53:53/udp` are two records sharing one host port. Removing one record while the
    // dataplane tore down both left the record claiming a port that answered nothing.
    let hits: Vec<String> = c
        .ports
        .iter()
        .filter(|p| {
            delonix_net::parse_publish(p)
                .map(|(h, _, _)| h == host_port)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if hits.is_empty() {
        return Err(Error::Invalid(format!(
            "'{}' does not publish host port {host_port}",
            c.name
        )));
    }
    for spec in &hits {
        let proto = delonix_net::parse_publish(spec).map(|(_, _, pr)| pr).ok();
        match c.network.as_deref() {
            Some(_) => infra::unpublish_port_proto(host_port, proto.as_deref()),
            None => {
                // Without a custom network, the hostfwd lives in the PER-container slirp —
                // which dies with it. On a stopped container there's no dataplane to clean up,
                // only the record (before: an error "container is not running" and the publish
                // stayed stuck in the record forever — a real bug report).
                if let Some(pid) = c.pid.filter(|_| c.is_live()) {
                    let sock = delonix_net::slirp_container_sock(pid);
                    if sock.exists() {
                        infra::slirp_remove_hostfwd_proto(&sock, host_port, proto.as_deref())?;
                    }
                }
            }
        }
    }
    *c = store.update(&c.id, |cur| {
        let before = cur.ports.len();
        cur.ports.retain(|p| !hits.contains(p));
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
        println!("{}", super::po::t("(no containers running)"));
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

/// Splits one CRI-format log line (`<rfc3339nano> stdout F <line>` — the
/// format `--log-cri` writes, see `delonix_runtime::log_shim`) into
/// `(timestamp, body)`. `None` for anything that doesn't match — a raw
/// (non-CRI) log, or an odd malformed record.
fn parse_cri_log_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.splitn(4, ' ');
    let ts = parts.next()?;
    let _stream = parts.next()?;
    let _tag = parts.next()?;
    let body = parts.next().unwrap_or("");
    if ts.len() < 20 || !ts.ends_with('Z') || ts.as_bytes().get(4) != Some(&b'-') {
        return None;
    }
    Some((ts, body))
}

/// Unix seconds -> the same `YYYY-MM-DDTHH:MM:SS` prefix a CRI log line's own
/// timestamp starts with — RFC3339 UTC timestamps compare lexicographically in
/// the same order as chronologically, so `--since` just needs this string
/// prefix, not a full parse back into a comparable integer. Small hand-written
/// civil-calendar conversion (mirrors `delonix_runtime`'s own private
/// `rfc3339` helper) rather than a `chrono` dependency, matching this
/// project's supply-chain-minimalism rule (see AGENTS.md's "Output" section).
fn unix_secs_to_rfc3339_prefix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}")
}

fn cri_format_error(name: &str) -> Error {
    Error::Invalid(format!(
        "--tail/--since/--timestamps require the container to have been run with --log-cri \
         (per-line timestamps) — '{name}' logs aren't in that format"
    ))
}

pub(crate) fn cmd_logs(
    images: &ImageStore,
    store: &Store,
    id: &str,
    follow: bool,
    tail: Option<usize>,
    since: Option<u64>,
    timestamps: bool,
) -> Result<()> {
    use std::io::{Read, Seek, Write};
    let c = find(store, id)?;
    // The recorded path wins: `--log-file` puts the log elsewhere, and the
    // fallback keeps records written before this field existed readable.
    let p = c
        .log_path
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| images.root().join("containers").join(&c.id).join("log"));
    let mut f = std::fs::File::open(&p)
        .map_err(|e| Error::Invalid(format!("no logs for {} at {}: {e}", c.name, p.display())))?;
    let mut out = std::io::stdout();
    let needs_cri = tail.is_some() || since.is_some() || timestamps;

    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    if needs_cri {
        let text = String::from_utf8_lossy(&buf).into_owned();
        let since_prefix = since.map(unix_secs_to_rfc3339_prefix);
        let mut lines: Vec<(&str, &str)> = Vec::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let (ts, body) = parse_cri_log_line(line).ok_or_else(|| cri_format_error(&c.name))?;
            lines.push((ts, body));
        }
        if let Some(threshold) = &since_prefix {
            lines.retain(|(ts, _)| *ts >= threshold.as_str());
        }
        if let Some(n) = tail {
            let start = lines.len().saturating_sub(n);
            lines = lines[start..].to_vec();
        }
        for (ts, body) in &lines {
            if timestamps {
                println!("{ts} {body}");
            } else {
                println!("{body}");
            }
        }
    } else {
        out.write_all(&buf)?;
    }
    if !follow {
        return Ok(());
    }
    // `-f`: follows the appends (reopens if the file shrinks — shim rotation);
    // ends when the container stops running and there's nothing left to read.
    // `--tail`/`--since` only filter the BACKLOG above — once live, every new
    // line is shown (same as docker). `--timestamps` keeps applying, buffering
    // a trailing incomplete line across reads (a line can arrive split across
    // two appends) since it needs a full line to parse the CRI record.
    let mut pos = f.stream_position()?;
    let mut pending_line = String::new();
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
            if !needs_cri {
                out.write_all(&buf)?;
                continue;
            }
            pending_line.push_str(&String::from_utf8_lossy(&buf));
            while let Some(nl) = pending_line.find('\n') {
                let line: String = pending_line.drain(..=nl).collect();
                let line = line.trim_end_matches('\n');
                if let Some((ts, body)) = parse_cri_log_line(line) {
                    if timestamps {
                        println!("{ts} {body}");
                    } else {
                        println!("{body}");
                    }
                } else if !line.is_empty() {
                    println!("{line}");
                }
            }
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

/// `docker attach` — OUTPUT-ONLY re-attach to a running container's log
/// stream (this engine keeps no live stdin conduit to an already-started
/// detached container, unlike a persistent per-container shim; see the
/// command's own `--help`). Reuses `cmd_logs`'s exact follow mechanism.
pub(crate) fn cmd_attach(
    images: &ImageStore,
    store: &Store,
    id: &str,
    interactive: bool,
) -> Result<()> {
    if interactive {
        return Err(Error::Invalid(
            "attach -i/--interactive: stdin forwarding isn't supported — this engine keeps no \
             live conduit to an already-started detached container's stdin (no persistent shim). \
             Use `container exec -it <id> <cmd>` for an interactive session instead."
                .into(),
        ));
    }
    let c = find(store, id)?;
    if !matches!(
        c.status,
        delonix_runtime_core::Status::Running | delonix_runtime_core::Status::Paused
    ) {
        return Err(Error::NotRunning(short_id(&c.id).to_string()));
    }
    cmd_logs(images, store, id, true, None, None, false)
}

/// Handles this group's `init` (see `cmd::scaffold`).
#[allow(clippy::too_many_arguments)] // mirrors the `ContainerCmd::Init` clap variant 1:1
fn cmd_init(
    target: super::scaffold::Target,
    dir: PathBuf,
    name: Option<String>,
    image: Option<String>,
    force: bool,
    template: Option<String>,
    template_version: Option<String>,
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
            template_version,
            up,
        },
    )
}

// ============================ health check contínuo ============================

/// Builds the [`HealthConfig`] from the `--health-*` flags, or `None` when the
/// user asked for no monitoring at all.
///
/// The trigger is DELIBERATELY "any `--health-*` flag was given", not just
/// `--health-cmd`: an image that already declares a `HEALTHCHECK` is the common
/// case, and `--health-interval 5` on it has an obvious meaning. Requiring a
/// `--health-cmd` that merely repeats the image would be ceremony.
///
/// It is not turned on by the mere presence of a `HEALTHCHECK` in the image
/// either: monitoring costs a probe process every interval, forever, and the
/// image cannot know whether this particular run wants that.
pub(crate) fn health_opts(
    cmd: Option<String>,
    interval: u64,
    timeout: u64,
    retries: u32,
    start_period: u64,
) -> Option<HealthConfig> {
    let d = HealthConfig::default();
    let asked = cmd.is_some()
        || interval != d.interval_secs
        || timeout != d.timeout_secs
        || retries != d.retries
        || start_period != d.start_period_secs;
    if !asked {
        return None;
    }
    Some(HealthConfig {
        cmd: cmd.unwrap_or_default(),
        interval_secs: interval.max(1),
        timeout_secs: timeout.max(1),
        retries: retries.max(1),
        start_period_secs: start_period,
    })
}

/// Wraps a probe so it enforces its OWN timeout, inside the container.
///
/// The engine cannot do it from outside: `runtime::exec` blocks in `waitpid`
/// on an intermediate process, and killing that intermediate leaves the actual
/// probe running in the container's pid namespace — a leak that repeats every
/// interval, forever. Making the probe kill itself has no such hole, needs no
/// new host-side plumbing, and works in any image that already has the
/// `/bin/sh` the probe requires anyway.
///
/// The exit code survives: a probe killed by the watchdog comes back as 137
/// (128+SIGKILL), which is a failure by the same rule as any non-zero.
///
/// **And the watchdog has to be reaped, or the leak comes back as zombies.**
/// The first version fired the watchdog off as `(sleep T; kill …) &` and later
/// sent it a plain `kill`, with no `wait` anywhere. Two processes survived every
/// single probe: the subshell (killed but never waited for) and the `sleep` it
/// was blocked on (never signalled at all, orphaned to the container's PID 1).
/// Nothing reaps them, because the container's PID 1 is the workload — a
/// `sleep`, a server, anything but an init.
///
/// Measured on this engine, 2026-09-10, on a container with `--health-interval
/// 2`: **+8 zombies every 10s**, growing without bound, invisible to `container
/// top` (zombies are not in `cgroup.procs`) and visible only as `pids.current`
/// climbing in `stats`. With the engine's default `pids.max=512` that is a
/// container which stops being able to fork — in ~10 minutes at a 2s interval,
/// in under three hours at the default 30s. A health check is a thing operators
/// add to be safe; this made it a slow fuse.
///
/// That fix trapped `TERM` in the watchdog so it could kill and reap its own
/// `sleep`, and it had a race of its own, measured 2026-09-16: when the probe ends
/// FAST (`true`, a `pg_isready` that answers at once) the parent's `kill` reaches
/// the watchdog before its `trap` is installed, the signal is lost, and the probe
/// waits out the WHOLE timeout — every probe of a healthy container took
/// `--health-timeout` (30s by default), in `dash` and busybox `ash` alike. A delay
/// before the `kill` only moves the race, and a `kill -9` of the watchdog orphans
/// its `sleep` onto the container's PID 1, which is exactly the zombie above.
///
/// So there is no watchdog and no signal between shells any more. The shell that
/// started the probe polls it itself: the probe's `/proc/<pid>/stat` disappears (the
/// shell already collected it) or shows `Z`, and the loop ends; past the deadline it
/// is killed. Every process is a DIRECT child of that shell and is reaped by it —
/// the `sleep` of each tick runs in the foreground, the probe by the final `wait` —
/// so nothing is orphaned and nothing is left to a PID 1 that reaps nothing. The
/// tick is 0.1s where `sleep` takes fractions and 1s where it does not; the deadline
/// is counted in tenths either way. The exit code survives, and a killed probe comes
/// back as 137 (128+SIGKILL).
pub(crate) fn health_probe_argv(cmd: &str, timeout_secs: u64) -> Vec<String> {
    let deadline_tenths = timeout_secs.saturating_mul(10);
    let script = format!(
        "{cmd} & __p=$!; \
         __d=1; sleep 0.01 2>/dev/null || __d=10; __i=0; \
         while {{ read -r __s < /proc/$__p/stat; }} 2>/dev/null; do \
           case \"$__s\" in *') Z '*) break ;; esac; \
           if [ $__i -ge {deadline_tenths} ]; then kill -9 $__p 2>/dev/null; break; fi; \
           if [ $__d = 1 ]; then sleep 0.1; else sleep 1; fi; \
           __i=$((__i + __d)); \
         done; \
         wait $__p"
    );
    vec!["/bin/sh".to_string(), "-c".to_string(), script]
}

/// The probe command line for a container: its own `--health-cmd`, else the
/// image's `HEALTHCHECK`. `None` when neither exists.
fn health_command(images: &ImageStore, c: &Container) -> Option<String> {
    if let Some(h) = &c.health {
        if !h.cmd.is_empty() {
            return Some(h.cmd.clone());
        }
    }
    // Resolved from the image EVERY time rather than frozen at create: a
    // rebuilt image is picked up on the next probe, and this is the same
    // resolution `--wait` and compose's `service_healthy` use.
    let argv = super::compose::image_health_argv(images, &c.image)?;
    // `image_health_argv` already returns `["/bin/sh","-c",<script>]` for the
    // shell form; anything else is an exec-form vector we re-join.
    if argv.len() == 3 && argv[0] == "/bin/sh" && argv[1] == "-c" {
        Some(argv[2].clone())
    } else {
        Some(argv.join(" "))
    }
}

/// Applies one probe result to the state machine. **Pure** — the whole of the
/// `starting`/`retries` semantics is tested here, with no container involved.
///
/// `age_secs` is how long the container has been up, which is what decides
/// whether we are still inside the start period. Docker's rule: a failure in
/// the grace window keeps the container `starting`, but a SUCCESS promotes it
/// to `healthy` immediately — the window is a licence to fail, not a delay.
pub(crate) fn apply_probe(
    prev: Option<&HealthState>,
    cfg: &HealthConfig,
    exit: i32,
    age_secs: u64,
    now_unix: i64,
) -> HealthState {
    let streak = if exit == 0 {
        0
    } else {
        prev.map(|p| p.failing_streak).unwrap_or(0) + 1
    };
    let health = if exit == 0 {
        Health::Healthy
    } else if age_secs < cfg.start_period_secs || streak < cfg.retries {
        Health::Starting
    } else {
        Health::Unhealthy
    };
    HealthState {
        health,
        failing_streak: streak,
        last_exit: exit,
        checked_unix: now_unix,
    }
}

/// Discards a container's on-disk state when `cmd_run` returns before handing the
/// container to `launch::start` — see [`discard_unstarted`] for why the record guards it.
struct UnstartedGuard<'a> {
    images: &'a ImageStore,
    store: &'a Store,
    id: &'a str,
    armed: bool,
}

impl Drop for UnstartedGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            discard_unstarted(self.images, self.store, self.id);
        }
    }
}

/// Removes the on-disk state of a container whose start FAILED before any record was
/// published — the overlay (`merged`/`upper`/`work`) and the log of an init that died.
///
/// Nothing else would: `container rm` and `system prune` find containers through their
/// records, and a start that failed never wrote one, so the directory was orphaned on
/// every failed `run -d` (measured 2026-09-16: one per attempt). Guarded by the record:
/// if one exists, this is a real container and its state is left alone.
fn discard_unstarted(images: &ImageStore, store: &Store, id: &str) {
    if store.load(id).is_ok() {
        return;
    }
    let _ = images.unmount_rootfs(id);
    // `purge_` and not the plain removal: once the init has mounted the overlay, the
    // kernel leaves `work/work` at mode 0000, which only the mapped removal takes.
    // Measured: every `run -d` whose command failed to exec left the directory.
    purge_container_dir(images, id);
}

/// Starts the health monitor as a CHILD PROCESS of the supervisor, not a thread.
///
/// It used to be a thread, and that made two forks unsafe at once: every probe runs
/// `runtime::exec`, which `fork`s and then allocates and prints in the child, and every
/// restart has the supervisor itself `clone()` the container again — both from a process
/// with a second thread alive. glibc's `fork` resets malloc's locks but not the locks the
/// Rust standard library holds (stderr, the environment), so a probe landing while the
/// other thread held one could hang the child for good. As a separate process forked
/// while the supervisor is still single-threaded, BOTH processes stay single-threaded.
///
/// `PR_SET_PDEATHSIG` ties the monitor to the supervisor: a thread died with it for
/// free, a process has to be told. The `getppid` check closes the window where the
/// supervisor died between the `fork` and the `prctl`.
fn spawn_health_monitor(id: String, cfg: HealthConfig) {
    // SAFETY: called by the supervisor, which is single-threaded at this point; the
    // child runs only the monitor loop and leaves through `_exit`.
    let supervisor = unsafe { libc::getpid() };
    // Monitoring is an addition to the container, never a condition for it: a failed
    // fork (-1) leaves the container running with no health column, and the parent
    // (> 0) has nothing to do — the monitor lives on its own.
    // SAFETY: as above — a fork from a single-threaded process.
    if unsafe { libc::fork() } == 0 {
        // SAFETY: `prctl` with integer arguments; `getppid`/`_exit` have no
        // preconditions.
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            if libc::getppid() != supervisor {
                libc::_exit(0);
            }
        }
        health_monitor_loop(id, cfg);
        // SAFETY: leaves the monitor without running the supervisor's destructors.
        unsafe { libc::_exit(0) };
    }
}

/// The monitoring loop, run by the detached container's supervisor.
///
/// **Who runs it was the design decision.** This engine is daemonless, so there
/// is nobody resident to poll. The supervisor is the honest answer: it already
/// exists, one per detached container, it already outlives the CLI, and it dies
/// with the container it watches — no fleet-wide process, no new lifecycle to
/// reason about. The cost is that a FOREGROUND container is not monitored, and
/// that is correct rather than a gap: you are looking at it.
fn health_monitor_loop(id: String, cfg: HealthConfig) {
    let Ok((images, store)) = open_stores() else {
        return;
    };
    loop {
        std::thread::sleep(std::time::Duration::from_secs(cfg.interval_secs));
        // The record disappearing (`rm`) is the signal to stop. Any other
        // error is transient (a concurrent write) and worth another round.
        let Ok(c) = store.load(&id) else {
            return;
        };
        // Not running: leave the last verdict alone rather than writing a
        // fresh "unhealthy" over a container the user stopped on purpose. The
        // supervisor's own restart loop is what brings it back, and the next
        // probe after that will speak for itself.
        if !c.is_live() {
            continue;
        }
        let Some(cmd) = health_command(&images, &c) else {
            // Asked to monitor something with no probe anywhere. Saying so once
            // and stopping beats an empty column nobody can explain.
            eprintln!("delonix: no health probe for '{}' — monitoring off", c.name);
            return;
        };
        let argv = health_probe_argv(&cmd, cfg.timeout_secs);
        let exit = runtime::exec(&c, &argv, false).unwrap_or(-1);
        let age = c
            .pid_starttime
            .and_then(output::uptime_from_starttime)
            .unwrap_or(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let next = apply_probe(c.health_state.as_ref(), &cfg, exit, age, now);
        let was = c.health_state.as_ref().map(|s| s.health);
        // `Store::update` and not save: the CLI writes to this same record
        // (`rename`, `update --publish-add`) and a read-modify-write without
        // the flock loses whichever change lands second.
        let _ = store.update(&id, |cur| {
            cur.health_state = Some(next.clone());
            true
        });
        if was != Some(next.health) {
            delonix_runtime_core::events::emit(
                &super::util::state_root(),
                "container",
                "health_status",
                &c.id,
                &c.name,
                Some(&next.health.to_string()),
            );
        }
    }
}

#[cfg(test)]
mod fmt_ports_tests {
    /// Uma spec com endereço de host tem DOIS dois-pontos, e cortar no primeiro
    /// troca as duas portas de sítio.
    ///
    /// Medido ao vivo antes da correcção, sobre um serviço publicado por compose
    /// em `127.0.0.1:19555:80`: a coluna PORTS dizia `127.0.0.1->19555:80/tcp` e
    /// o `container port` dizia **`19555:80/tcp -> 0.0.0.0:127.0.0.1`** — um
    /// endereço que não existe, na saída que se lê para decidir se um serviço
    /// está exposto. O bug era alcançável pelo CLI desde que o `-p` passou a
    /// aceitar host-IP; ficou visível quando o compose deixou de os recusar.
    #[test]
    fn uma_spec_com_endereco_de_host_nao_troca_as_portas() {
        assert_eq!(
            super::fmt_ports(&["127.0.0.1:19555:80".to_string()]),
            "127.0.0.1:19555->80/tcp"
        );
        assert_eq!(
            super::fmt_ports(&["0.0.0.0:8080:80/udp".to_string()]),
            "0.0.0.0:8080->80/udp"
        );
    }

    /// `container port` never claims an exposure it cannot back up.
    ///
    /// The regression this locks: printing the literal `0.0.0.0` for a spec with
    /// no address, while the engine publishes on `127.0.0.1` by default. An
    /// explicit address in the spec is a fact and passes through untouched.
    #[test]
    fn published_addr_never_widens_a_loopback_bind() {
        // No address in the spec → the default the publish path actually uses.
        assert_eq!(
            super::published_addr_for_display(None),
            "127.0.0.1",
            "an absent address must not read as every interface"
        );
        // An explicit one is a fact, whatever it is.
        assert_eq!(
            super::published_addr_for_display(Some("0.0.0.0".into())),
            "0.0.0.0"
        );
        assert_eq!(
            super::published_addr_for_display(Some("192.168.1.10".into())),
            "192.168.1.10"
        );
    }

    /// As formas SEM endereço mantêm-se byte a byte — incluindo a omissão
    /// deliberada do `0.0.0.0`, que existe para não afirmar uma exposição que
    /// depende do caminho de publicação e do `DELONIX_PUBLISH_ADDR`.
    #[test]
    fn as_formas_sem_endereco_ficam_como_estavam() {
        assert_eq!(super::fmt_ports(&["8080:80".to_string()]), "8080->80/tcp");
        assert_eq!(
            super::fmt_ports(&["8080:80/udp".to_string()]),
            "8080->80/udp"
        );
        assert_eq!(super::fmt_ports(&["80".to_string()]), "80/tcp");
    }
}

#[cfg(test)]
mod unconverged_container_tests {
    use super::*;

    fn doc(spec: &str) -> ManifestDoc {
        ManifestDoc {
            api_version: "delonix.io/v1".into(),
            kind: k::CONTAINER.into(),
            metadata: super::super::manifest::Metadata {
                name: "c".into(),
                namespace: None,
                labels: Default::default(),
                annotations: Default::default(),
            },
            spec: serde_yaml::from_str(spec).unwrap(),
        }
    }

    /// O aviso tem de NOMEAR o que não é comparado — é a diferença entre um plano
    /// honesto e um que diz «no changes» sobre um `env` mudado à mão.
    #[test]
    fn nomeia_os_campos_declarados_que_nao_sao_comparados() {
        let c = unconverged_fields_condition(&doc(
            "image: alpine\nenv: [A=1]\nuser: '1000'\ncapAdd: [NET_ADMIN]\n",
        ))
        .expect("um manifesto com env/user/capAdd tem de avisar");
        for esperado in ["env", "user", "capAdd"] {
            assert!(
                c.message.contains(esperado),
                "'{esperado}' não foi nomeado: {}",
                c.message
            );
        }
    }

    /// **Nenhum campo COMPARADO pode aparecer na lista** — senão o aviso diz que
    /// algo não é aplicado quando é, e mandava recriar um container à toa. É o
    /// mesmo par de asserções que o `Vm` tem, e é o que torna a derivação da
    /// constante uma garantia em vez de uma intenção.
    #[test]
    fn nunca_lista_um_campo_que_e_comparado() {
        let spec = format!(
            "{}\nenv: [A=1]\n",
            RECONCILED_CONTAINER_FIELDS
                .iter()
                .map(|f| format!("{f}: x"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let c = unconverged_fields_condition(&doc(&spec)).expect("o env tem de avisar");
        // Só a PRIMEIRA lista (a dos não-aplicados). A mensagem traz as duas de
        // propósito — a segunda diz o que É comparado — e olhar para a frase
        // inteira faria este teste falhar sobre o texto que existe para ajudar.
        let listados = c
            .message
            .split("container: ")
            .nth(1)
            .and_then(|s| s.split(" — ").next())
            .expect("a mensagem tem de trazer a lista antes do travessão");
        for comparado in RECONCILED_CONTAINER_FIELDS {
            assert!(
                !listados.split(", ").any(|f| f == *comparado),
                "'{comparado}' é comparado e não devia estar na lista: {listados}"
            );
        }
    }

    /// Um manifesto que declara SÓ o que é comparado não tem nada a avisar — e um
    /// aviso que dispara sobre um manifesto correcto é como as pessoas aprendem a
    /// não ler avisos. O `detach` conta como nada a dizer: é o modo de invocação
    /// de quem cria, não estado que um container a correr possa ter divergido.
    #[test]
    fn um_manifesto_so_com_campos_comparados_nao_avisa() {
        assert!(unconverged_fields_condition(&doc("image: alpine\nports: ['80:80']\n")).is_none());
        assert!(unconverged_fields_condition(&doc("image: alpine\ndetach: true\n")).is_none());
    }
}

#[cfg(test)]
mod runspec_single_builder_tests {
    /// **One builder of the engine's spawn specification, and this keeps it one.**
    ///
    /// `run` and `start` each had their own `RunSpec` literal, and a field filled
    /// in one and forgotten in the other was lost on a restart six times — the
    /// sixth time it was the container's confinement. The gate that replaced this
    /// one compared the two literals field by field; unifying them on
    /// `CliRunPorts::run_spec` found a seventh (`--net none` and `--log-file`
    /// ignored by `start`). With one builder the parity is structural, and this
    /// test only refuses a second one appearing in this module.
    #[test]
    fn container_has_one_runspec_builder() {
        // The spawn specification is built in ONE place,
        // `delonix_runtime::launch_spec::run_spec`, from a `Launch`; `run` and
        // `start` both go through it. A literal here would be a second builder.
        let src = include_str!("container.rs");
        let needle = concat!("RunSpec", " {");
        let builders = src.matches(needle).count();
        assert_eq!(
            builders, 0,
            "container.rs builds the spawn specification in {builders} places — \
             build every start from a `Launch` through `launch_spec::run_spec`"
        );
    }
}

#[cfg(test)]
mod tests {
    /// `--env-file0`: byte-exact values (multi-line, `=` inside the value),
    /// before `-e` (which wins), and an entry without `KEY=` is refused rather
    /// than silently dropped.
    #[test]
    fn env_file0_reads_exact_entries_and_dash_e_wins() {
        let dir = std::env::temp_dir().join(format!("dlx-envf0-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.env0");
        std::fs::write(&f, b"A=1\0CERT=x\n  y=z\0").unwrap();
        let fs = vec![f.to_string_lossy().into_owned()];
        let got = super::with_env_file0(&fs, vec!["A=2".into()]).unwrap();
        assert_eq!(got, ["A=1", "CERT=x\n  y=z", "A=2"]);
        std::fs::write(&f, b"NOEQUALS\0").unwrap();
        assert!(super::with_env_file0(&fs, vec![]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The contract of the whole reconciler**: an unchanged manifest must
    /// produce ZERO differences. Both sides of the diff are normalized by
    /// separate functions, and the moment one of them drifts, every plan starts
    /// reporting phantom drift — a plan that always says «changed» is worth less
    /// than no plan at all.
    ///
    /// So this test builds the record the way `cmd_run` builds it for exactly
    /// the compared fields and asserts the two maps are equal. It fails if
    /// either side changes alone, which is the point.
    #[test]
    fn manifesto_inalterado_nao_produz_diferenca_nenhuma() {
        let spec = super::ContainerSpec {
            image: "nginx:1.27".into(),
            ports: vec!["8080:80".into()],
            volumes: vec!["dados:/var/lib".into()],
            memory: Some("512M".into()),
            cpus: Some("0.5".into()),
            privileged: false,
            restart: "always".into(),
            network: "interna".into(),
            hostname: Some("web".into()),
            ..serde_yaml::from_str::<super::ContainerSpec>("image: nginx:1.27").unwrap()
        };

        let mut c = delonix_runtime_core::Container::new(
            "id".into(),
            "web".into(),
            "nginx:1.27".into(),
            vec!["nginx".into()],
            "512M".into(), // `cmd_run`: eff_memory = memory.unwrap_or("max")
        );
        c.ports = vec!["8080:80".into()];
        c.cpus = "0.5".into();
        c.privileged = false;
        c.restart_policy = Some("always".into());
        c.net_mode = Some("interna".into());
        c.network = Some("interna".into());
        c.hostname = Some("web".into());
        let root = std::path::Path::new("/var/lib/delonix/volumes");
        c.mounts = vec![delonix_runtime_core::Mount {
            source: root.join("dados/_data").to_string_lossy().into_owned(),
            target: "/var/lib".into(),
            readonly: false,
            propagation: None,
            optional: false,
        }];

        let desired = super::desired_container_fields(&spec);
        let actual = super::actual_container_fields(&c, root);
        assert_eq!(
            desired, actual,
            "unchanged manifest must diff to nothing\n desired={desired:#?}\n actual={actual:#?}"
        );
        // And the set really is the documented one — a field added to one map
        // and not the other would show up here before it shows up as drift.
        let keys: Vec<&str> = desired.keys().map(String::as_str).collect();
        for k in &keys {
            assert!(
                super::RECONCILED_CONTAINER_FIELDS.contains(k),
                "{k} is compared but undocumented"
            );
        }
    }

    /// A named volume must render back to the NAME the manifest used, never to
    /// the `_data` path — otherwise every container with a volume looks changed.
    /// A bind keeps its host path. `resolve_spec` cannot be used for this
    /// direction: it CREATES the volume, and computing a plan must not create
    /// anything.
    #[test]
    fn mount_to_spec_devolve_o_nome_do_volume_e_o_caminho_de_um_bind() {
        let root = std::path::Path::new("/var/lib/delonix/volumes");
        let named = delonix_runtime_core::Mount {
            source: "/var/lib/delonix/volumes/dados/_data".into(),
            target: "/var/lib".into(),
            readonly: false,
            propagation: None,
            optional: false,
        };
        assert_eq!(super::mount_to_spec(&named, root), "dados:/var/lib");
        let ro = delonix_runtime_core::Mount {
            readonly: true,
            ..named.clone()
        };
        assert_eq!(super::mount_to_spec(&ro, root), "dados:/var/lib:ro");
        let bind = delonix_runtime_core::Mount {
            source: "/etc/nginx".into(),
            target: "/etc/nginx".into(),
            readonly: true,
            propagation: None,
            optional: false,
        };
        assert_eq!(
            super::mount_to_spec(&bind, root),
            "/etc/nginx:/etc/nginx:ro"
        );
        // A bind that merely SITS under the volumes root is not a named volume:
        // only the exact `<name>/_data` shape is.
        let deep = delonix_runtime_core::Mount {
            source: "/var/lib/delonix/volumes/dados/_data/sub".into(),
            target: "/x".into(),
            readonly: false,
            propagation: None,
            optional: false,
        };
        assert_eq!(
            super::mount_to_spec(&deep, root),
            "/var/lib/delonix/volumes/dados/_data/sub:/x"
        );
    }

    /// REGRESSION: a host port held by a process this engine has never heard of
    /// (not a delonix container, so `port_owner` says `None`; not a privilege
    /// question either, so `can_bind_host_port` says `true`) used to sail
    /// through `cmd_run`'s whole preflight silently and only blow up deep inside
    /// the slirp handshake — measured as a 15s+ hang on `delonix container run -d
    /// -p 8080:8080 <image>` against a host process already listening on :8080.
    /// `host_port_conflict_error` is the check that closes that gap: it must
    /// fire the instant the port is taken, and clear the instant it's freed.
    #[test]
    fn host_port_conflict_error_names_a_foreign_holder_and_clears_once_freed() {
        use std::net::TcpListener;
        let held = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = held.local_addr().unwrap().port();
        let hp = port.to_string();

        let err = super::host_port_conflict_error(&hp, "80", None)
            .expect("a real listener holds this port right now");
        let msg = err.to_string();
        assert!(
            msg.contains("already in use on the host") && msg.contains(&hp),
            "expected a busy-port diagnosis naming port {hp}, got: {msg}"
        );
        // Ready-to-copy alternative, same convention as the sibling errors above.
        assert!(msg.contains(&(port as u32 + 10000).to_string()), "{msg}");

        drop(held);
        // The ephemeral port is back in the kernel's pool the moment it is dropped,
        // and the test battery runs in parallel: another test may be handed this very
        // port before the next line (measured — a full `cargo test --workspace` failed
        // here once while five isolated runs passed). The property is «a freed port is
        // not reported busy», so it is asked a few times and must hold at least once;
        // a diagnosis that NEVER clears would still fail every attempt.
        let cleared = (0..20).any(|_| {
            let free = super::host_port_conflict_error(&hp, "80", None).is_none();
            if !free {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            free
        });
        assert!(cleared, "a freed port must not still be reported as busy");
    }

    /// Ports and volumes are SETS — the order in the manifest carries no
    /// meaning. Without sorting, moving a line in the YAML would be reported as
    /// a change and, because `ports` is hot but `volumes` ordering would drag in
    /// a full-list diff, it would read far more alarming than it is.
    #[test]
    fn a_ordem_das_portas_no_manifesto_nao_e_uma_alteracao() {
        let mk = |ports: Vec<&str>| super::ContainerSpec {
            ports: ports.into_iter().map(String::from).collect(),
            ..serde_yaml::from_str::<super::ContainerSpec>("image: nginx").unwrap()
        };
        let a = super::desired_container_fields(&mk(vec!["8080:80", "8443:443"]));
        let b = super::desired_container_fields(&mk(vec!["8443:443", "8080:80"]));
        assert_eq!(a.get("ports"), b.get("ports"));
    }

    /// `hostAliases` do k8s (um IP, N nomes) tem de dar o mesmo resultado que
    /// N `--add-host`. Sem isto, o MESMO `kind: Container` tinha a
    /// funcionalidade na forma plana e não na forma k8s.
    #[test]
    fn host_alias_k8s_converte_para_add_host() {
        let a = super::HostAlias {
            ip: "10.0.0.9".into(),
            hostnames: vec!["meet.local".into(), "erp.local".into()],
        };
        assert_eq!(
            a.to_add_host().unwrap(),
            vec!["meet.local:10.0.0.9", "erp.local:10.0.0.9"]
        );
        // Passa pelo MESMO validador — um IP inválido falha aqui também.
        let mau = super::HostAlias {
            ip: "nao-e-ip".into(),
            hostnames: vec!["x.local".into()],
        };
        assert!(mau.to_add_host().is_err());
        // E a injecção também.
        let inj = super::HostAlias {
            ip: "10.0.0.9".into(),
            hostnames: vec!["a\nb 9.9.9.9 evil".into()],
        };
        assert!(inj.to_add_host().is_err());
    }

    /// O `\n` era o que armava a escrita de conteúdo arbitrário fora do
    /// rootfs (achado ALTO, provado ao vivo antes desta correcção): permitia
    /// injectar linhas no `/etc/hosts`, e um symlink plantado pela imagem
    /// levava-as para um caminho do host.
    #[test]
    fn parse_add_host_recusa_injeccao_e_exige_ip() {
        use super::parse_add_host as p;
        // O exploit exacto do revisor.
        assert!(p("x:1.2.3.4\nLINHA-ARBITRARIA\n#").is_err());
        assert!(p("x:1.2.3.4 evil\n9.9.9.9 deb.debian.org").is_err());
        // Espaço = alias no formato do /etc/hosts: uma entrada mapeava N nomes.
        assert!(p("registry-1.docker.io deb.debian.org:10.66.66.66").is_err());
        assert!(p("a\tb:1.2.3.4").is_err());
        // O endereço TEM de ser um IP — antes escrevia-se o que viesse.
        assert!(p("nome:isto-nao-e-ip").is_err());
        assert!(p("10.0.0.9:meet.local").is_err()); // ordem trocada
        assert!(p("semseparador").is_err());
        assert!(p(":10.0.0.1").is_err()); // nome vazio
                                          // Válidos, incluindo IPv6 (é por isso que se parte pelo ÚLTIMO `:`).
        assert_eq!(
            p("meet.kaeso.local:10.0.0.9").unwrap(),
            ("meet.kaeso.local".into(), "10.0.0.9".into())
        );
        assert_eq!(
            p("db:2001:db8::1").unwrap(),
            ("db".into(), "2001:db8::1".into())
        );
        // A forma `=` foi removida: partia IPv6 em silêncio.
        assert!(p("db=2001:db8::1").is_err());
    }

    /// A degradação de rede tem de ser DISTINGUÍVEL de uma escolha
    /// deliberada. Antes disto, os três casos abaixo diziam todos "host".
    #[test]
    fn net_mode_display_separa_degradacao_de_escolha() {
        let base = |net_mode: Option<&str>, network: Option<&str>| {
            let mut c = delonix_runtime_core::Container::new(
                "id".into(),
                "n".into(),
                "img".into(),
                vec!["sh".into()],
                "max".into(),
            );
            c.net_mode = net_mode.map(str::to_string);
            c.network = network.map(str::to_string);
            c
        };
        // Ligado: mostra a rede.
        assert_eq!(
            super::net_mode_display(&base(Some("dev"), Some("dev"))),
            "dev"
        );
        // Escolhas deliberadas: sem alarme.
        assert_eq!(super::net_mode_display(&base(Some("host"), None)), "host");
        assert_eq!(super::net_mode_display(&base(Some("none"), None)), "none");
        // O caso que estava mudo: pediu rede, não a tem.
        assert_eq!(
            super::net_mode_display(&base(Some("dev"), None)),
            "host (degraded: asked for 'dev')"
        );
        // Registo antigo (sem o campo): não inventa intenção.
        assert_eq!(super::net_mode_display(&base(None, None)), "host");
    }

    /// O supervisor é o que torna o exit code de um container `-d` conhecível:
    /// `waitpid` é a única fonte de um estado real e o kernel só o dá ao PAI.
    /// Sem supervisor o CLI saía, o container era reparentado ao `init`, e o
    /// código morria com ele — `Exited (unknown)`.
    #[test]
    fn supervisiona_todo_o_detached_excepto_quem_nao_pode_fazer_fork() {
        use super::should_supervise;
        // Detached, com ou sem política: supervisiona (a política só decide o
        // que o supervisor FAZ depois da morte, não se existe).
        for pol in ["", "no", "always", "on-failure:3", "unless-stopped"] {
            assert!(
                should_supervise(pol, true, true),
                "política {pol:?} devia ser supervisionada em -d"
            );
        }
        // Em primeiro plano o CLI JÁ é o pai — não há nada a resolver.
        assert!(!should_supervise("always", false, true));
        // Chamador que não pode fazer fork em segurança (o servidor docker-api,
        // multi-thread): mantém o comportamento antigo em vez de arriscar um
        // fork de um processo com threads.
        assert!(!should_supervise("", true, false));
        assert!(!should_supervise("always", true, false));
    }

    /// O tecto ABSOLUTO de I/O (`--device-read-bps` & família) — o que o
    /// `io.weight` não dá: sozinho na máquina, um container com peso continua a
    /// saturar o disco e a esfomear o journald/store/swap do host.
    #[test]
    fn compoe_o_io_max_a_partir_das_quatro_flags() {
        // Só leitura.
        assert_eq!(
            compose_io_max(Some("10mb"), None, None, None).unwrap(),
            Some("rbps=10485760".to_string())
        );
        // Os quatro, pela ordem canónica do cgroup-v2.
        assert_eq!(
            compose_io_max(Some("1m"), Some("2m"), Some("100"), Some("200")).unwrap(),
            Some("rbps=1048576 wbps=2097152 riops=100 wiops=200".to_string())
        );
        // Nenhuma flag → sem linha `io.max` nenhuma (não escrever é diferente de
        // escrever "max").
        assert_eq!(compose_io_max(None, None, None, None).unwrap(), None);
    }

    /// Sintaxe do Docker (`<device>:<rate>`) aceite; o device é IGNORADO de
    /// propósito — as escritas de um container só chegam ao disco do store, e
    /// aceitar outro seria aceitar uma instrução que o motor não pode honrar.
    #[test]
    fn parse_io_rate_aceita_a_sintaxe_docker_e_ignora_o_device() {
        assert_eq!(
            parse_io_rate("/dev/sda:10mb", true).unwrap(),
            10 * 1024 * 1024
        );
        assert_eq!(parse_io_rate("10mb", true).unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_io_rate("1g", true).unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_io_rate("1024", true).unwrap(), 1024);
        assert_eq!(parse_io_rate("/dev/nvme0n1:500", false).unwrap(), 500);
    }

    /// Fail-closed no input: um valor absurdo NUNCA pode virar `u64::MAX` — um
    /// limite que se lê como definido e é limite nenhum (a mesma armadilha do
    /// `as u64` saturante que a quota de volumes já pagou).
    #[test]
    fn parse_io_rate_recusa_input_invalido_em_vez_de_saturar() {
        for bad in ["", "abc", "0", "-5", "99999999999t", "10xb"] {
            assert!(
                parse_io_rate(bad, true).is_err(),
                "aceitou o valor inválido {bad:?}"
            );
        }
        // IOPS não leva sufixos de tamanho.
        assert!(parse_io_rate("10mb", false).is_err());
        // E o erro nomeia a flag, para o utilizador saber qual corrigir.
        let e = compose_io_max(None, Some("xpto"), None, None).unwrap_err();
        assert!(e.contains("--device-write-bps"), "{e}");
    }

    use super::super::util::compose_command;
    use super::infra;
    use super::{compose_io_max, parse_io_rate};
    use super::{
        container_ips, fmt_ports, fmt_status, next_extra_idx, normalize_container_spec,
        parse_cri_log_line, parse_signal, policy_supervised, reexec_env,
        unix_secs_to_rfc3339_prefix, valid_container_name, ContainerSpec,
    };
    use delonix_runtime_core::{Container, ExtraNet, Status};

    /// REGRESSION: the firewall used to be keyed on `c.ip` alone, so every additional
    /// network was ungoverned — reproduced live, an `ingress policy deny` container
    /// answered normally on its second address. The primary must come FIRST (the chain is
    /// named after it, and `do_unfirewall` finds it by that name).
    #[test]
    fn container_ips_covers_every_network_primary_first() {
        let mut c = Container::new(
            "id".into(),
            "t".into(),
            "img".into(),
            vec!["sh".to_string()],
            "max".into(),
        );
        c.ip = Some("10.209.0.5".into());
        c.extra_networks = vec![
            ExtraNet {
                network: "net2".into(),
                ip: "10.239.0.5".into(),
                idx: 1,
            },
            ExtraNet {
                network: "net3".into(),
                ip: "10.232.0.5".into(),
                idx: 2,
            },
        ];
        assert_eq!(
            container_ips(&c),
            vec!["10.209.0.5", "10.239.0.5", "10.232.0.5"]
        );
        // A `--net host` container has no SDN address at all — never an empty string,
        // which would reach the holder as a malformed IP.
        let mut host_net = c.clone();
        host_net.ip = None;
        host_net.extra_networks.clear();
        assert!(container_ips(&host_net).is_empty());
        // An extra network with a blank IP (a half-written record) is skipped, not
        // forwarded as an empty token in the comma-separated control line.
        let mut blank = c.clone();
        blank.extra_networks[0].ip = String::new();
        assert_eq!(container_ips(&blank), vec!["10.209.0.5", "10.232.0.5"]);
    }

    #[test]
    fn parse_signal_aceita_nome_numero_e_variantes_de_maiusculas() {
        assert_eq!(parse_signal("9").unwrap(), libc::SIGKILL);
        assert_eq!(parse_signal("KILL").unwrap(), libc::SIGKILL);
        assert_eq!(parse_signal("kill").unwrap(), libc::SIGKILL);
        assert_eq!(parse_signal("SIGKILL").unwrap(), libc::SIGKILL);
        assert_eq!(parse_signal("term").unwrap(), libc::SIGTERM);
        assert_eq!(parse_signal("HUP").unwrap(), libc::SIGHUP);
        assert!(parse_signal("NOTASIGNAL").is_err());
    }

    #[test]
    fn parse_cri_log_line_reconhece_o_formato_e_rejeita_texto_cru() {
        let (ts, body) =
            parse_cri_log_line("2026-07-26T15:30:00.123456789Z stdout F hello world").unwrap();
        assert_eq!(ts, "2026-07-26T15:30:00.123456789Z");
        assert_eq!(body, "hello world");
        // Partial-record marker ("P") also parses — only the timestamp shape gates it.
        assert!(parse_cri_log_line("2026-07-26T15:30:00.123456789Z stdout P partial").is_some());
        assert!(parse_cri_log_line("plain raw log line, no timestamp").is_none());
        assert!(parse_cri_log_line("").is_none());
    }

    /// Cross-checked against real `date -u -d @<secs>` output — the whole point
    /// of this function is that its output has to be lexicographically
    /// comparable against a REAL CRI log line's own timestamp text.
    #[test]
    fn unix_secs_to_rfc3339_prefix_bate_com_date_u() {
        assert_eq!(unix_secs_to_rfc3339_prefix(0), "1970-01-01T00:00:00");
        assert_eq!(
            unix_secs_to_rfc3339_prefix(1_700_000_000),
            "2023-11-14T22:13:20"
        );
        assert_eq!(
            unix_secs_to_rfc3339_prefix(1_753_547_686),
            "2025-07-26T16:34:46"
        );
    }

    #[test]
    fn since_prefix_e_menor_ou_igual_a_um_timestamp_real_do_mesmo_segundo() {
        // The actual property `--since` relies on: a bare-seconds prefix must
        // compare <= any real (fractional-nanosecond) CRI timestamp for that
        // same second, so `ts >= threshold` includes the whole second.
        let threshold = unix_secs_to_rfc3339_prefix(1_753_547_686);
        let real_line_ts = "2025-07-26T16:34:46.999999999Z";
        assert!(real_line_ts >= threshold.as_str());
        let earlier = "2025-07-26T16:34:45.999999999Z";
        assert!(earlier < threshold.as_str());
    }

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

    /// BUG REAL, apanhado a gerar o JSON Schema e presente num exemplo
    /// PUBLICADO (`examples/dependency.yaml`): `env: { K: v }` — a forma que
    /// qualquer pessoa vinda do compose ou do k8s escreve — era aceite e
    /// silenciosamente DESCARTADA, porque o `env` era removido antes de se
    /// verificar se a mapping era mesmo a forma agrupada. O Postgres do exemplo
    /// arrancava sem password nenhuma.
    #[test]
    fn env_como_mapa_simples_nao_e_descartado() {
        let v: serde_yaml::Value =
            serde_yaml::from_str("image: postgres:16\nenv: { POSTGRES_PASSWORD: dev, DEBUG: 1 }\n")
                .unwrap();
        let out = super::normalize_container_spec(v);
        let env = out.get("env").unwrap().as_sequence().unwrap();
        let mut got: Vec<String> = env
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec!["DEBUG=1".to_string(), "POSTGRES_PASSWORD=dev".to_string()]
        );
    }

    /// ...e a forma AGRUPADA continua a ser a forma agrupada. É a presença de
    /// uma das quatro chaves conhecidas que a identifica, não o facto de ser
    /// uma mapping — senão a correcção acima trocaria uma perda silenciosa por
    /// outra.
    #[test]
    fn env_agrupado_continua_a_ser_hoisteado() {
        let v: serde_yaml::Value = serde_yaml::from_str(
            "image: nginx\nenv:\n  vars: [K=v]\n  files: [.env]\n  secretFiles: true\n",
        )
        .unwrap();
        let out = super::normalize_container_spec(v);
        assert_eq!(
            out.get("env").unwrap().as_sequence().unwrap()[0]
                .as_str()
                .unwrap(),
            "K=v"
        );
        assert!(out.get("envFile").is_some());
        assert_eq!(out.get("secretFiles").unwrap().as_bool(), Some(true));
    }

    #[test]
    fn normalize_container_spec_deixa_a_forma_plana_intacta() {
        let flat: serde_yaml::Value =
            serde_yaml::from_str("image: nginx\nnetwork: host\nenv: [K=v]\nmemory: 1G\n").unwrap();
        assert_eq!(flat.clone(), normalize_container_spec(flat));
    }

    #[test]
    fn normalize_container_spec_hoisteia_todos_os_grupos() {
        let grouped: serde_yaml::Value = serde_yaml::from_str(
            "image: nginx\n\
             resources:\n\
             \x20 memory: 512M\n\
             \x20 cpus: \"2.0\"\n\
             network:\n\
             \x20 name: my-net\n\
             \x20 ports: [\"8080:80\"]\n\
             \x20 expose: 80\n\
             \x20 alias: [web]\n\
             security:\n\
             \x20 privileged: true\n\
             \x20 capAdd: [NET_ADMIN]\n\
             storage:\n\
             \x20 volumes: [\"data:/var/lib\"]\n\
             \x20 tmpfs: [\"/scratch\"]\n\
             env:\n\
             \x20 vars: [\"KEY=value\"]\n\
             \x20 secrets: [db-pass]\n\
             limits:\n\
             \x20 ulimit: [\"nofile=1024\"]\n\
             \x20 gpus: all\n",
        )
        .unwrap();
        let spec: ContainerSpec =
            serde_yaml::from_value(normalize_container_spec(grouped)).unwrap();
        assert_eq!(spec.image, "nginx");
        assert_eq!(spec.memory.as_deref(), Some("512M"));
        assert_eq!(spec.cpus.as_deref(), Some("2.0"));
        assert_eq!(spec.network, "my-net");
        assert_eq!(spec.ports, vec!["8080:80".to_string()]);
        assert_eq!(spec.expose, Some(80));
        assert_eq!(spec.network_alias, vec!["web".to_string()]);
        assert!(spec.privileged);
        assert_eq!(spec.cap_add, vec!["NET_ADMIN".to_string()]);
        assert_eq!(spec.volumes, vec!["data:/var/lib".to_string()]);
        assert_eq!(spec.tmpfs, vec!["/scratch".to_string()]);
        assert_eq!(spec.env, vec!["KEY=value".to_string()]);
        assert_eq!(spec.secret, vec!["db-pass".to_string()]);
        assert_eq!(spec.ulimit, vec!["nofile=1024".to_string()]);
        assert_eq!(spec.gpus.as_deref(), Some("all"));
    }

    #[test]
    fn sub_chave_desconhecida_no_grupo_e_reportada() {
        use super::unknown_group_keys;
        let v: serde_yaml::Value = serde_yaml::from_str(
            "image: nginx\nresources:\n  memoria: 128M\nsecurity:\n  privileged: true\n",
        )
        .unwrap();
        assert_eq!(unknown_group_keys(&v), vec!["resources.memoria"]);
    }

    /// `env: {KEY: value}` é a forma que quem vem do compose escreve, e cada
    /// chave ali é uma variável do utilizador — avisar sobre elas seria ruído em
    /// cima de um manifesto correcto. Só a forma AGRUPADA é verificada.
    #[test]
    fn env_plano_nao_gera_avisos() {
        use super::unknown_group_keys;
        let plano: serde_yaml::Value =
            serde_yaml::from_str("image: nginx\nenv:\n  POSTGRES_PASSWORD: dev\n").unwrap();
        assert!(unknown_group_keys(&plano).is_empty());
        let agrupado: serde_yaml::Value =
            serde_yaml::from_str("image: nginx\nenv:\n  vars: [A=1]\n  ficheiros: x\n").unwrap();
        assert_eq!(unknown_group_keys(&agrupado), vec!["env.ficheiros"]);
    }

    #[test]
    fn normalize_container_spec_forma_plana_explicita_ganha_ao_grupo() {
        let mixed: serde_yaml::Value = serde_yaml::from_str(
            "image: nginx\nmemory: 2G\nresources:\n  memory: 512M\n  cpus: \"4.0\"\n",
        )
        .unwrap();
        let spec: ContainerSpec = serde_yaml::from_value(normalize_container_spec(mixed)).unwrap();
        assert_eq!(
            spec.memory.as_deref(),
            Some("2G"),
            "o memory plano explícito devia ganhar"
        );
        assert_eq!(
            spec.cpus.as_deref(),
            Some("4.0"),
            "sem colisão, o do grupo aplica-se"
        );
    }

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
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
    fn valid_container_name_recusa_pontos_e_outros_exploits() {
        // HIGH fixed here: a dotted name is indistinguishable from an
        // external FQDN to the DNS resolver's whole-name match — reject it.
        assert!(!valid_container_name("registry.npmjs.org"));
        assert!(!valid_container_name("api.github.com"));
        assert!(!valid_container_name(""));
        assert!(!valid_container_name("-x"));
        // legitimate names (incl. the auto-derived "king-place-NN" pattern).
        assert!(valid_container_name("njinga-benguela-07"));
        assert!(valid_container_name("web"));
        assert!(valid_container_name("my_app-2"));
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
    fn supervised_policy_only_for_active_policies() {
        assert!(!policy_supervised("no"));
        assert!(!policy_supervised(""));
        assert!(policy_supervised("always"));
        assert!(policy_supervised("unless-stopped"));
        assert!(policy_supervised("on-failure"));
        assert!(policy_supervised("on-failure:5"));
    }

    #[test]
    fn pod_shape_normalizes_to_run_opts() {
        let yaml = r#"
containers:
  - name: web
    image: nginx:latest
    command: ["/bin/sh", "-c"]
    args: ["nginx -g 'daemon off;'"]
    workingDir: /srv/app
    ports:
      - containerPort: 80
        hostPort: 8080
        protocol: TCP
    env:
      - name: FOO
        value: bar
    volumeMounts:
      - name: data
        mountPath: /data
        readOnly: true
      - name: cache
        mountPath: /cache
    resources:
      limits:
        cpu: "500m"
        memory: "256Mi"
    securityContext:
      privileged: true
      runAsUser: 1000
      readOnlyRootFilesystem: true
      capabilities:
        add: ["NET_ADMIN"]
        drop: ["ALL"]
volumes:
  - name: data
    hostPath:
      path: /srv/data
  - name: cache
    emptyDir: {}
network: mynet
restartPolicy: OnFailure
"#;
        let pod: super::PodSpec = serde_yaml::from_str(yaml).unwrap();
        let opts = super::pod_to_run_opts("web", None, pod).unwrap();
        assert_eq!(opts.image, "nginx:latest");
        assert_eq!(opts.entrypoint.as_deref(), Some("/bin/sh"));
        assert_eq!(opts.command, vec!["-c", "nginx -g 'daemon off;'"]);
        assert_eq!(opts.workdir.as_deref(), Some("/srv/app"));
        assert_eq!(opts.ports, vec!["8080:80/tcp"]);
        assert_eq!(opts.env, vec!["FOO=bar"]);
        assert!(opts.volumes.contains(&"/srv/data:/data:ro".to_string()));
        assert_eq!(opts.tmpfs, vec!["/cache"]);
        assert_eq!(opts.cpus.as_deref(), Some("0.5"));
        assert_eq!(opts.memory.as_deref(), Some("256Mi"));
        assert!(opts.privileged);
        assert_eq!(opts.user.as_deref(), Some("1000"));
        assert!(opts.read_only);
        assert_eq!(opts.cap_add, vec!["NET_ADMIN"]);
        assert_eq!(opts.cap_drop, vec!["ALL"]);
        assert_eq!(opts.net, "mynet");
        assert_eq!(opts.restart, "on-failure");
    }

    /// `--restart talvez` used to be ACCEPTED (rc=0) and leave
    /// `restart_policy: null`: nothing recognised the value, so nothing ever
    /// restarted the container while the operator believed a supervisor was
    /// configured. A misspelling that costs availability, in silence.
    #[test]
    fn a_restart_policy_is_valid_or_it_is_refused() {
        for good in [
            "no",
            "always",
            "unless-stopped",
            "on-failure",
            "on-failure:3",
            "on-failure:99",
        ] {
            assert_eq!(
                super::parse_restart_policy(good).as_deref(),
                Ok(good),
                "{good}"
            );
            // Whatever passes here must be a value the supervisor understands —
            // otherwise the check is decorative. `no` is the one that
            // deliberately does not supervise.
            if good != "no" {
                assert!(
                    super::policy_supervised(good),
                    "{good} passed and the supervisor does not know it"
                );
            }
        }
        for bad in [
            "talvez",
            "alwyas",
            "on-failre",
            "",
            "ALWAYS",
            "always:2",
            "on-failure:0",
            "on-failure:abc",
            "on-failure:-1",
        ] {
            assert!(
                super::parse_restart_policy(bad).is_err(),
                "{bad} should be refused"
            );
        }
    }

    /// The flag documented `1–10000` and enforced nothing: `0`, `99999` and
    /// `abc` all landed on the kernel's default of 100, with rc=0. A range in
    /// the `--help` that the code does not check is a promise, not a fact.
    #[test]
    fn a_cgroup_weight_honours_the_range_its_help_promises() {
        for good in ["1", "100", "500", "10000"] {
            assert_eq!(
                super::parse_cgroup_weight(good).as_deref(),
                Ok(good),
                "{good}"
            );
        }
        for bad in ["0", "10001", "99999", "abc", "", "-1", "1.5", " 100"] {
            assert!(
                super::parse_cgroup_weight(bad).is_err(),
                "{bad} should be refused"
            );
        }
    }

    #[test]
    fn pod_shape_rejects_multi_container() {
        let yaml = "containers:\n  - image: a\n  - image: b\n";
        let pod: super::PodSpec = serde_yaml::from_str(yaml).unwrap();
        assert!(super::pod_to_run_opts("x", None, pod).is_err());
    }

    #[test]
    fn pod_member_run_opts_wires_shared_netns_and_labels() {
        // `kind: Pod` (multi-container): each member joins the SAME netns and is
        // labelled for membership.
        let yaml = "\
containers:
  - name: web
    image: nginx
    ports: [{ containerPort: 80, hostPort: 8080 }]
  - name: side
    image: busybox
";
        let pod: super::PodSpec = serde_yaml::from_str(yaml).unwrap();
        let opts = super::pod_member_run_opts("myapp", None, pod, "pod-myapp").unwrap();
        assert_eq!(opts.len(), 2);
        for o in &opts {
            assert_eq!(o.pod.as_deref(), Some("pod-myapp"));
            assert!(o.labels.iter().any(|l| l == "delonix.io/pod=myapp"));
            // All members share the pod hostname.
            assert_eq!(o.hostname.as_deref(), Some("myapp"));
        }
        assert_eq!(opts[0].name.as_deref(), Some("myapp-web"));
        assert_eq!(opts[1].name.as_deref(), Some("myapp-side"));
        assert_eq!(opts[0].ports, vec!["8080:80/tcp"]);
        // The declared position, stamped on each member. It is the ONLY record
        // of the manifest's order — the container keeps just the
        // `<pod>-<member>` name — and `pod::members_of` sorts by it so that
        // «the pod's first member» resolves to `web` and not to whatever
        // `read_dir` returned first (ACH-011).
        assert!(
            opts[0].labels.iter().any(|l| l == "delonix.io/pod-index=0"),
            "{:?}",
            opts[0].labels
        );
        assert!(
            opts[1].labels.iter().any(|l| l == "delonix.io/pod-index=1"),
            "{:?}",
            opts[1].labels
        );
    }

    #[test]
    fn reexec_env_fixa_o_root_e_o_runtime_dir_dos_sockets() {
        let env = reexec_env("abc123", "10.210.0.7");
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"DELONIX_REEXEC_ID"), "{names:?}");
        assert!(names.contains(&"DELONIX_REEXEC_IP"), "{names:?}");
        // Both uid-derived paths, not just the state root: the 2nd pass sees
        // `geteuid() == 0` inside the holder's userns, so leaving the ingress
        // sockets' dir unpinned resolved `/run/delonix-net` and broke
        // `run/start --net <custom> -p <port>` with a bare ENOENT (v0.34.1).
        assert!(names.contains(&"DELONIX_ROOT"), "{names:?}");
        let (rt_var, rt_dir) = infra::runtime_dir_env();
        let got = env
            .iter()
            .find(|(k, _)| k == rt_var)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{rt_var} missing from the re-exec env: {names:?}"));
        // Pinned to OUR value — the whole point is that the child must not
        // recompute it from its own (mapped) uid.
        assert_eq!(got, rt_dir.into_os_string());
    }

    // ---- health check contínuo ----

    use super::{apply_probe, fmt_status_of, health_opts, health_probe_argv};
    use delonix_runtime_core::{Health, HealthConfig, HealthState};

    fn hc(retries: u32, start_period: u64) -> HealthConfig {
        HealthConfig {
            retries,
            start_period_secs: start_period,
            ..HealthConfig::default()
        }
    }

    #[test]
    fn health_opts_so_liga_quando_o_utilizador_pede() {
        let d = HealthConfig::default();
        // Só os defaults: monitorização DESLIGADA. Uma imagem com HEALTHCHECK
        // não passa a custar um processo por intervalo só por existir.
        assert!(health_opts(
            None,
            d.interval_secs,
            d.timeout_secs,
            d.retries,
            d.start_period_secs
        )
        .is_none());
        // Um --health-interval sozinho basta para ligar sobre o HEALTHCHECK da
        // imagem, sem obrigar a repetir o comando.
        let on = health_opts(None, 5, d.timeout_secs, d.retries, d.start_period_secs).unwrap();
        assert_eq!(on.interval_secs, 5);
        assert!(on.cmd.is_empty());
    }

    #[test]
    fn health_opts_nunca_aceita_zero() {
        // Um intervalo de 0 seria um ciclo apertado a correr um processo dentro
        // do container para sempre — o clap aceita o número, nós não.
        let o = health_opts(Some("true".into()), 0, 0, 0, 0).unwrap();
        assert_eq!((o.interval_secs, o.timeout_secs, o.retries), (1, 1, 1));
    }

    #[test]
    fn apply_probe_precisa_de_retries_seguidos_para_ficar_unhealthy() {
        let cfg = hc(3, 0);
        let mut st = apply_probe(None, &cfg, 1, 100, 0);
        assert_eq!(
            st.health,
            Health::Starting,
            "1 falha de 3 ainda não é doente"
        );
        st = apply_probe(Some(&st), &cfg, 1, 100, 0);
        assert_eq!(st.health, Health::Starting);
        st = apply_probe(Some(&st), &cfg, 1, 100, 0);
        assert_eq!(st.health, Health::Unhealthy);
        assert_eq!(st.failing_streak, 3);
    }

    #[test]
    fn apply_probe_um_sucesso_zera_a_sequencia() {
        let cfg = hc(3, 0);
        // Duas falhas, um sucesso, duas falhas: NUNCA doente. `retries` é uma
        // sequência, não um total — um serviço que falha uma vez por hora não
        // pode acumular até ser declarado morto.
        let mut st = apply_probe(None, &cfg, 1, 100, 0);
        st = apply_probe(Some(&st), &cfg, 1, 100, 0);
        st = apply_probe(Some(&st), &cfg, 0, 100, 0);
        assert_eq!(st.health, Health::Healthy);
        assert_eq!(st.failing_streak, 0);
        st = apply_probe(Some(&st), &cfg, 1, 100, 0);
        st = apply_probe(Some(&st), &cfg, 1, 100, 0);
        assert_eq!(st.health, Health::Starting);
    }

    #[test]
    fn apply_probe_start_period_protege_e_o_sucesso_promove_de_imediato() {
        let cfg = hc(1, 60);
        // Dentro da janela, mesmo com retries=1, uma falha não condena.
        let st = apply_probe(None, &cfg, 1, 10, 0);
        assert_eq!(st.health, Health::Starting);
        // Mas um sucesso promove já — a janela é licença para falhar, não atraso.
        let st = apply_probe(Some(&st), &cfg, 0, 11, 0);
        assert_eq!(st.health, Health::Healthy);
        // Passada a janela, a mesma falha condena.
        let st = apply_probe(Some(&st), &cfg, 1, 61, 0);
        assert_eq!(st.health, Health::Unhealthy);
    }

    #[test]
    fn health_probe_argv_mata_se_a_si_proprio() {
        let a = health_probe_argv("curl -f localhost", 7);
        assert_eq!(a[0], "/bin/sh");
        assert_eq!(a[1], "-c");
        assert!(a[2].contains("curl -f localhost &"));
        // Deadline in tenths of a second, whatever the tick turns out to be.
        assert!(a[2].contains("-ge 70"), "{}", a[2]);
        assert!(a[2].contains("kill -9 $__p"));
        // The probe is reaped by the shell that started it, and its code is the
        // wrapper's code — `wait $__p` last, nothing after it.
        assert!(a[2].trim_end().ends_with("wait $__p"), "{}", a[2]);
        // No watchdog subshell and no trap: the signal between two shells is what
        // got lost when the probe finished fast.
        assert!(!a[2].contains("trap"), "{}", a[2]);
    }

    /// Runs the wrapper for real, in the host's `/bin/sh`. What the string test
    /// above cannot see and the E2E run measured: a probe that ends at once must
    /// return at once — it used to wait out the whole timeout — its exit code must
    /// survive, and one that overruns must be killed near the deadline.
    #[test]
    fn health_probe_wrapper_returns_fast_keeps_the_code_and_kills_at_the_deadline() {
        use std::time::{Duration, Instant};
        let run = |cmd: &str, timeout: u64| {
            let a = health_probe_argv(cmd, timeout);
            let t = Instant::now();
            let st = std::process::Command::new(&a[0])
                .arg(&a[1])
                .arg(&a[2])
                .status()
                .expect("/bin/sh runs");
            (st, t.elapsed())
        };
        for _ in 0..5 {
            let (st, took) = run("true", 5);
            assert!(st.success());
            assert!(
                took < Duration::from_secs(2),
                "a fast probe waited {took:?}"
            );
        }
        let (st, _) = run("exit 7", 5);
        assert_eq!(st.code(), Some(7));
        let (st, took) = run("sleep 30", 1);
        assert!(!st.success(), "an overrunning probe must fail");
        assert!(
            took >= Duration::from_millis(900) && took < Duration::from_secs(4),
            "killed at {took:?}, not near the 1s deadline"
        );
    }

    #[test]
    fn status_so_mostra_saude_de_um_container_a_correr() {
        let mut c = Container::new(
            "id".into(),
            "t".into(),
            "img".into(),
            v(&["sh"]),
            "max".into(),
        );
        c.status = Status::Running;
        c.health_state = Some(HealthState {
            health: Health::Unhealthy,
            failing_streak: 3,
            last_exit: 1,
            checked_unix: 0,
        });
        assert!(fmt_status_of(&c, Some(90)).contains("(unhealthy)"));
        // Parado, o último veredicto deixa de ser afirmado: seria dar por
        // presente uma observação que já não se está a fazer.
        c.status = Status::Stopped;
        assert_eq!(fmt_status_of(&c, None), "Exited (0)");
    }

    fn scratch_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dlx-restartcounts-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The off-by-one this column shipped with, found live: `cmd_run`'s
    /// initial launch emits `"create"`, never `"start"` — a naive
    /// `count("start") - 1` therefore undercounted every container by
    /// exactly one restart the moment it was ACTUALLY restarted once.
    #[test]
    fn restart_counts_nao_subtrai_um_porque_o_run_nunca_emite_start() {
        let root = scratch_root("basic");
        delonix_runtime_core::events::emit(&root, "container", "create", "c1", "c1", None);
        let counts = super::restart_counts(&root);
        assert_eq!(
            counts.get("c1").copied().unwrap_or(0),
            0,
            "create sozinho não é um restart"
        );

        delonix_runtime_core::events::emit(&root, "container", "stop", "c1", "c1", None);
        delonix_runtime_core::events::emit(&root, "container", "die", "c1", "c1", Some("exit=137"));
        delonix_runtime_core::events::emit(&root, "container", "start", "c1", "c1", None);
        let counts = super::restart_counts(&root);
        assert_eq!(counts.get("c1").copied().unwrap_or(0), 1);

        delonix_runtime_core::events::emit(&root, "container", "stop", "c1", "c1", None);
        delonix_runtime_core::events::emit(&root, "container", "start", "c1", "c1", None);
        let counts = super::restart_counts(&root);
        assert_eq!(counts.get("c1").copied().unwrap_or(0), 2);

        std::fs::remove_dir_all(&root).ok();
    }

    /// Each container's count is independent — a `start` on `c2` must not
    /// bleed into `c1`'s row.
    #[test]
    fn restart_counts_e_por_container() {
        let root = scratch_root("perid");
        delonix_runtime_core::events::emit(&root, "container", "create", "c1", "c1", None);
        delonix_runtime_core::events::emit(&root, "container", "create", "c2", "c2", None);
        delonix_runtime_core::events::emit(&root, "container", "start", "c2", "c2", None);
        let counts = super::restart_counts(&root);
        assert_eq!(counts.get("c1").copied().unwrap_or(0), 0);
        assert_eq!(counts.get("c2").copied().unwrap_or(0), 1);
        std::fs::remove_dir_all(&root).ok();
    }
}
