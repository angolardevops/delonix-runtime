//! `kind: Service` — a selector-matched workload SET, load-balanced by DNS (ADR-0032).
//!
//! `ADR-0020` named `Service` as one of the six Kinds the CLI restructuring's
//! Phase CLI-2 needed before it could even start; five of the six shipped, this
//! is the sixth. It selects containers by label (`spec.selector.matchLabels`,
//! reusing `delonix_sdn::matches_labels` — the same pure function
//! `FirewallPolicy`'s own planned selector, ADR-0024, is meant to share rather
//! than grow a second implementation of) and publishes the matched set as
//! MULTIPLE DNS `A` records under `<name>.<namespace>.delonix.internal`,
//! round-robin rotated per query.
//!
//! **No VIP, no L4 dataplane, no new daemon.** The membership computation and
//! the DNS answer itself live in `delonix_sdn::infra` (`ServiceDef` registry +
//! `build_dns_index`'s Service pass + `dns_resolve_multi_for`) — this module is
//! the thin Kind-dispatch layer: parse the spec, write the registry entry,
//! answer the reconciler's questions. See `docs/adr/0032-service-kind-dns-round-robin.md`
//! for the full design and the deliberately-deferred pieces (a real L4 VIP,
//! `type: LoadBalancer`/`NodePort`/`ExternalName`).
//!
//! **Membership is readiness-aware.** Only a `Running` container is a backend,
//! and one that declares a health check is a backend only while `healthy`
//! (`Starting`/`Unhealthy` are out). A container with no health check is ready
//! when it runs. This is discovery, not draining: a client holding a resolved
//! address keeps it.
//!
//! **`type: VirtualIP` (ADR-0066).** The default `type: DNS` above is
//! unchanged byte for byte. A `VirtualIP` Service holds ONE address from the
//! VIP pool the operator configures (`delonix network vip-pool set`); its name
//! resolves to that address, and the holder DNATs each new connection to one of
//! the ready backends (`numgen random`). Ready means the declared port accepts a
//! TCP connect from the holder AND, when the container declares a health check,
//! `healthy`. With no backend ready the VIP refuses at once. The dataplane, the
//! readiness prober and the pool live in `delonix_sdn::service_lb`.
//!
//! **No new CLI leaf.** Reached the same way `kind: Dependency`/
//! `kind: NetworkAccessRule` already are — through `delonix apply -f`/
//! `delonix stack apply`, which dispatch by Kind — plus the generic `get`/
//! `describe`/`delete services` verbs (`cmd/verbs.rs`).

use std::collections::BTreeMap;

use super::kinds as k;
use super::manifest::{self, ManifestDoc};
use super::output::OutputFormat;
use super::util::open_stores;
use delonix_model::{Error, Result};
use delonix_sdn::service_lb::{self, ServiceType, TcpReadiness};

/// `spec` of `kind: Service`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct ServiceSpec {
    pub selector: ServiceSelector,
    /// The CONTAINER port every matched workload is expected to listen on —
    /// mirrors the "always container-side, post-DNAT" convention `net ingress
    /// allow`'s port already uses. With `type: VirtualIP` it is also the VIP's
    /// port: one port in v1 (ADR-0066 open question 1), and no host-side port.
    pub port: u16,
    /// `DNS` (default, ADR-0032) or `VirtualIP` (ADR-0066).
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub service_type: Option<ServiceTypeSpec>,
    /// `type: VirtualIP` only: the address to hold, inside the node's VIP
    /// pool. Without it the IPAM picks one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vip: Option<String>,
    /// `type: VirtualIP` only: the knobs of the TCP readiness check that
    /// always runs for each backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<ServiceReadiness>,
}

/// `spec.type` of a `kind: Service`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
pub enum ServiceTypeSpec {
    #[serde(rename = "DNS")]
    Dns,
    #[serde(rename = "VirtualIP")]
    VirtualIp,
}

/// `spec.readiness`.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceReadiness {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp: Option<TcpReadinessSpec>,
}

/// `spec.readiness.tcp` — every field optional, the defaults being 2 s,
/// 500 ms, 1 success to admit and 2 failures to eject.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TcpReadinessSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_milliseconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_threshold: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_threshold: Option<u32>,
}

impl ServiceSpec {
    fn is_virtual_ip(&self) -> bool {
        self.service_type == Some(ServiceTypeSpec::VirtualIp)
    }

    /// The readiness knobs with every default materialised.
    fn tcp_readiness(&self) -> TcpReadiness {
        let d = TcpReadiness::default();
        let t = self
            .readiness
            .as_ref()
            .and_then(|r| r.tcp.clone())
            .unwrap_or_default();
        TcpReadiness {
            interval_seconds: t.interval_seconds.unwrap_or(d.interval_seconds),
            timeout_milliseconds: t.timeout_milliseconds.unwrap_or(d.timeout_milliseconds),
            success_threshold: t.success_threshold.unwrap_or(d.success_threshold),
            failure_threshold: t.failure_threshold.unwrap_or(d.failure_threshold),
        }
    }

    /// What does not depend on the node: `vip`/`readiness` belong to
    /// `VirtualIP`, and a malformed address or readiness knob is refused before
    /// anything is written.
    fn validate(&self) -> Result<()> {
        if !self.is_virtual_ip() {
            if self.vip.is_some() || self.readiness.is_some() {
                return Err(Error::Invalid(
                    "spec.vip and spec.readiness apply to `type: VirtualIP` only".into(),
                ));
            }
            return Ok(());
        }
        if let Some(v) = &self.vip {
            if v.parse::<std::net::Ipv4Addr>().map(|a| a.to_string()) != Ok(v.clone()) {
                return Err(Error::coded(
                    1361,
                    Error::Invalid(format!("spec.vip {v:?} is not a dotted IPv4 address")),
                ));
            }
        }
        self.tcp_readiness().validate().map_err(Error::Invalid)
    }
}

/// How the readiness knobs read in the plan: one comparable string.
fn readiness_field(r: &TcpReadiness) -> String {
    format!(
        "interval={}s,timeout={}ms,success={},failure={}",
        r.interval_seconds, r.timeout_milliseconds, r.success_threshold, r.failure_threshold
    )
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct ServiceSelector {
    #[serde(default, rename = "matchLabels")]
    pub match_labels: BTreeMap<String, String>,
}

/// Known fields of the `spec` (drift-guard, same pattern every other Kind's
/// spec uses).
pub const SERVICE_SPEC_FIELDS: &[&str] = &["selector", "port", "type", "vip", "readiness"];

/// Fields the reconciler compares.
/// `type`, `vip` and `readiness` appear only for a `type: VirtualIP` Service,
/// so a `type: DNS` one compares exactly what it always did.
pub const RECONCILED_SERVICE_FIELDS: &[&str] = &["matchLabels", "port", "type", "vip", "readiness"];

fn service_fields(spec: &ServiceSpec) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    // A `BTreeMap` renders its keys in sorted order deterministically, so this
    // comparable form never spuriously drifts because two applies happened to
    // write the same labels in a different order.
    f.insert(
        "matchLabels".into(),
        spec.selector
            .match_labels
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(","),
    );
    f.insert("port".into(), spec.port.to_string());
    if spec.is_virtual_ip() {
        f.insert("type".into(), ServiceType::VirtualIp.as_str().into());
        f.insert(
            "vip".into(),
            spec.vip.clone().unwrap_or_else(|| "auto".into()),
        );
        f.insert("readiness".into(), readiness_field(&spec.tcp_readiness()));
    }
    f
}

/// What the manifest declares, for the reconciler. `ownable: true` — a
/// `Service` has its own durable identity (name + namespace), so a stack CAN
/// own and prune one, same as `NetworkAccessRule`/`NetworkRoute`.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: ServiceSpec = manifest::spec_of(doc)?;
    Ok(super::reconcile::Desired {
        kind: k::SERVICE.into(),
        name: manifest::plan_name(doc),
        fields: service_fields(&spec),
        converges: true,
        ownable: true,
    })
}

/// Every `Service` this node has declared — the enumeration `--prune` needs,
/// same reasoning as `netroute::actual`/`network_access_rule::actual`.
pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    Ok(delonix_sdn::infra::service_list()
        .into_iter()
        .map(|def| {
            let mut f = BTreeMap::new();
            f.insert(
                "matchLabels".into(),
                def.match_labels
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            f.insert("port".into(), def.port.to_string());
            if def.service_type == ServiceType::VirtualIp {
                f.insert("type".into(), ServiceType::VirtualIp.as_str().into());
                f.insert(
                    "vip".into(),
                    def.vip_requested.clone().unwrap_or_else(|| "auto".into()),
                );
                f.insert(
                    "readiness".into(),
                    readiness_field(&def.readiness.unwrap_or_default()),
                );
            }
            super::reconcile::Actual {
                kind: k::SERVICE.into(),
                name: manifest::scoped_plan_name(&def.namespace, &def.name),
                fields: f,
                owner: def.labels.get(super::reconcile::STACK_LABEL).cloned(),
                last_applied: def
                    .annotations
                    .get(super::reconcile::LAST_APPLIED)
                    .and_then(|raw| super::reconcile::decode_last_applied(raw)),
            }
        })
        .collect())
}

/// Records ownership + last-applied for `name` — mirrors
/// `netroute::stamp`/`network_access_rule::stamp`, on the Service's OWN
/// registry entry (unlike `NetworkAccessRule`, a `Service` does not target a
/// container it does not own — its record is entirely its own, so stamping it
/// directly carries none of that Kind's risk).
pub(crate) fn stamp(plan: &str, stack: &str, fields: &BTreeMap<String, String>) -> Result<()> {
    let (namespace, name) = manifest::split_plan_name(plan);
    delonix_sdn::infra::service_set_metadata(
        namespace,
        name,
        &[
            (
                super::reconcile::STACK_LABEL.to_string(),
                Some(stack.to_string()),
            ),
            (
                super::reconcile::MANAGED_BY.to_string(),
                Some("delonix".to_string()),
            ),
        ],
        &[(
            super::reconcile::LAST_APPLIED.to_string(),
            Some(super::reconcile::encode_last_applied(fields)),
        )],
    )
    .map_err(Into::into)
}

/// `--prune`/`stack destroy`'s teardown.
pub(crate) fn remove_for_replace(plan: &str) -> Result<()> {
    let (namespace, name) = manifest::split_plan_name(plan);
    delonix_sdn::infra::service_remove(namespace, name)?;
    sync_dataplane();
    Ok(())
}

/// Tells the holder to rewrite the VIP dataplane now. Best effort: with the
/// holder down there is nothing to rewrite, and its next start syncs from the
/// records; a holder too old to know the verb says so, and that is printed.
fn sync_dataplane() {
    if let Err(e) = service_lb::sync() {
        eprintln!(
            "{}",
            super::po::tf(
                "warning: the VIP dataplane was not rewritten now ({why}); the holder syncs on its next round",
                &[("why", &e.to_string())],
            )
        );
    }
}

/// The holder must run bridged traffic through netfilter, or a client on its
/// backends' own network times out on the VIP (ADR-0066 D5). «Could not ask»
/// (holder down) falls back to whether the module exists on the host: a fresh
/// holder netns starts with the sysctl at 1 when it does.
fn require_hairpin() -> Result<()> {
    let ok = match delonix_sdn::infra::br_netfilter_active() {
        Ok(v) => v,
        Err(_) => std::path::Path::new("/proc/sys/net/bridge/bridge-nf-call-iptables").exists(),
    };
    if ok {
        return Ok(());
    }
    Err(delonix_sdn::Error::VipHairpinUnavailable(
        "type: VirtualIP needs bridged traffic to go through netfilter in the holder \
         (net.bridge.bridge-nf-call-iptables=1); without it a client on its backends' own \
         network times out on the VIP. Load br_netfilter on the host (`modprobe br_netfilter`)"
            .into(),
    )
    .into())
}

/// The VIP half of an apply: the pool in force, its overlap check, the
/// hairpin precondition, then the lease. Returns the VIP.
fn ensure_vip(namespace: &str, name: &str, spec: &ServiceSpec) -> Result<String> {
    let (pools, _) = service_lb::configured_pools()?;
    if pools.is_empty() {
        return Err(delonix_sdn::Error::VipPoolUnconfigured(format!(
            "Service/{name}: type VirtualIP needs a VIP pool, and this node has none; set one \
             with `delonix network vip-pool set <cidr>` (or {} for one process)",
            service_lb::VIP_POOL_ENV
        ))
        .into());
    }
    service_lb::check_pools_against(&pools, &service_lb::collect_sources())?;
    require_hairpin()?;
    Ok(service_lb::allocate_vip(
        namespace,
        name,
        spec.vip.as_deref(),
        &pools,
    )?)
}

/// For `stack ls`/`describe`: declared vs. how many live backends it resolves
/// to right now. Reads the SAME container store the DNS index itself reads,
/// so this never disagrees with what a client asking for the name actually
/// gets.
pub(crate) fn presence_of(doc: &ManifestDoc) -> (String, String) {
    let Ok(spec) = manifest::spec_of::<ServiceSpec>(doc) else {
        return ("?".into(), super::po::t("invalid spec").into());
    };
    let namespace = doc
        .metadata
        .namespace
        .as_deref()
        .unwrap_or("default")
        .to_string();
    if delonix_sdn::infra::service_get(&namespace, &doc.metadata.name).is_none() {
        return ("no".into(), "-".into());
    }
    let count = match_count(&namespace, &spec.selector.match_labels);
    if count == 0 {
        return (
            "yes".into(),
            super::po::t("selector matches no workloads").into(),
        );
    }
    (
        "yes".into(),
        super::po::tf("{count} backend(s)", &[("count", &count.to_string())]),
    )
}

/// Live count of containers this selector currently matches, in `namespace` —
/// the same match `build_dns_index`'s Service pass computes, done here on the
/// CLI's OWN container store read (not a second implementation of the match:
/// `delonix_sdn::matches_labels` is the one function, this is just a second
/// CALLER of it, over a store this process already has open).
fn match_count(namespace: &str, match_labels: &BTreeMap<String, String>) -> usize {
    let Ok((_images, store)) = open_stores() else {
        return 0;
    };
    let Ok(containers) = store.list() else {
        return 0;
    };
    containers
        .iter()
        .filter(|c| c.namespace.eq_ignore_ascii_case(namespace))
        .filter(|c| delonix_sdn::matches_labels(&c.labels, match_labels))
        .count()
}

/// Applies one document — writes the selector+port to the registry
/// (`delonix_sdn::infra::service_set`), preserving any existing ownership
/// stamp (the reconciler's `stamp` runs separately, after `apply`, same
/// two-step order every other ownable Kind here follows).
fn apply_one(containers: &[delonix_compute::Container], doc: &ManifestDoc) -> Result<()> {
    let spec: ServiceSpec = manifest::spec_of(doc)?;
    spec.validate()?;
    let namespace = doc
        .metadata
        .namespace
        .as_deref()
        .unwrap_or("default")
        .to_string();
    let name = doc.metadata.name.as_str();
    let before = delonix_sdn::infra::service_get(&namespace, name);
    // The VIP first: a refusal (no pool, overlap, hairpin, a taken address)
    // must leave the record exactly as it was.
    let vip = if spec.is_virtual_ip() {
        Some(ensure_vip(&namespace, name, &spec)?)
    } else {
        None
    };
    delonix_sdn::infra::service_set(&namespace, name, &spec.selector.match_labels, spec.port)?;
    if let Some(vip) = &vip {
        delonix_sdn::infra::service_set_lb(
            &namespace,
            name,
            ServiceType::VirtualIp,
            Some(vip.clone()),
            spec.vip.clone(),
            Some(spec.tcp_readiness()),
        )?;
        if let Some(old) = before.as_ref().and_then(|b| b.vip.clone()) {
            if &old != vip {
                // Whoever kept the old address stops reaching it (ADR-0066 D1).
                eprintln!(
                    "{}",
                    super::po::tf(
                        "Service/{name}: VIP moved {old} -> {new}; clients that kept {old} no longer reach it",
                        &[("name", name), ("old", &old), ("new", vip)],
                    )
                );
            }
        }
    } else if before
        .as_ref()
        .is_some_and(|b| b.service_type == ServiceType::VirtualIp || b.vip.is_some())
    {
        service_lb::release_vip(&namespace, name);
        delonix_sdn::infra::service_set_lb(&namespace, name, ServiceType::Dns, None, None, None)?;
    }
    if vip.is_some() || before.as_ref().is_some_and(|b| b.vip.is_some()) {
        sync_dataplane();
    }
    let matched = containers
        .iter()
        .filter(|c| c.namespace.eq_ignore_ascii_case(&namespace))
        .filter(|c| delonix_sdn::matches_labels(&c.labels, &spec.selector.match_labels))
        .count();
    if matched == 0 {
        // Deliberately a warning, not an error (ADR-0032): refusing would
        // make the declarative create-policies-before-workloads order
        // illegal, and `apply` has no rollback to undo a half-applied
        // stack over an ordering problem that resolves itself on the next
        // apply once the workload exists.
        eprintln!(
            "{}",
            super::po::tf(
                "Service/{name}: selector matched no workloads — this service resolves to nothing",
                &[("name", &doc.metadata.name)],
            )
        );
    }
    match &vip {
        Some(vip) => println!(
            "{}",
            super::po::tf(
                "service/{name}: {namespace}.{count} matching workload(s), VIP {vip}:{port}",
                &[
                    ("name", name),
                    ("namespace", &namespace),
                    ("count", &matched.to_string()),
                    ("vip", vip),
                    ("port", &spec.port.to_string()),
                ],
            )
        ),
        None => println!(
            "{}",
            super::po::tf(
                "service/{name}: {namespace}.{count} matching workload(s), port {port}",
                &[
                    ("name", name),
                    ("namespace", &namespace),
                    ("count", &matched.to_string()),
                    ("port", &spec.port.to_string()),
                ],
            )
        ),
    }
    Ok(())
}

/// Applies every `kind: Service` document.
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (_images, store) = open_stores()?;
    let containers = store.list().unwrap_or_default();
    for doc in manifest::of_kind(docs, k::SERVICE) {
        apply_one(&containers, doc)?;
    }
    Ok(())
}

/// `converge_and_stamp`'s live-update path (`stack.rs`) — same rationale as
/// `NetworkAccessRule`/`FirewallPolicy`: `apply_one` already fully overwrites
/// the registry entry, so converging IS applying, and a per-field path would
/// just be a second way to write the same record.
pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    let (_images, store) = open_stores()?;
    let containers = store.list().unwrap_or_default();
    apply_one(&containers, doc)
}

/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: ServiceSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

/// `delonix get services` — every declared `Service`, its live backend count
/// and DNS name.
#[derive(serde::Serialize)]
struct ServiceLsRow {
    name: String,
    namespace: String,
    #[serde(rename = "type")]
    service_type: String,
    vip: Option<String>,
    port: u16,
    backends: usize,
    /// `VirtualIP` only: the backends in rotation right now, as the holder
    /// serves them. `None` when the holder could not be asked — never read as
    /// zero.
    ready: Option<usize>,
    dns: String,
    stack: Option<String>,
}

/// The holder's view of every `VirtualIP` Service, or `None` when it cannot be
/// asked (down, or older than this build).
fn holder_states() -> Option<Vec<service_lb::ServiceState>> {
    service_lb::state().ok()
}

fn state_of<'a>(
    states: &'a Option<Vec<service_lb::ServiceState>>,
    def: &delonix_sdn::infra::ServiceDef,
) -> Option<&'a service_lb::ServiceState> {
    states
        .as_ref()?
        .iter()
        .find(|s| s.namespace == def.namespace && s.name == def.name)
}

pub(crate) fn cmd_ls(format: OutputFormat) -> Result<()> {
    let format = super::config::resolve_output(&super::util::state_root(), format);
    let mut defs = delonix_sdn::infra::service_list();
    defs.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));

    let states = if defs
        .iter()
        .any(|d| d.service_type == ServiceType::VirtualIp)
    {
        holder_states()
    } else {
        None
    };
    let rows: Vec<ServiceLsRow> = defs
        .iter()
        .map(|def| ServiceLsRow {
            name: def.name.clone(),
            namespace: def.namespace.clone(),
            service_type: def.service_type.as_str().to_string(),
            vip: def.vip.clone(),
            port: def.port,
            backends: match_count(&def.namespace, &def.match_labels),
            ready: (def.service_type == ServiceType::VirtualIp)
                .then(|| state_of(&states, def).map(|s| s.ready()))
                .flatten(),
            dns: delonix_sdn::infra::service_fqdn(&def.name, &def.namespace),
            stack: def.labels.get(super::reconcile::STACK_LABEL).cloned(),
        })
        .collect();

    if format == OutputFormat::Json {
        return super::output::print_json(&rows);
    }

    let mut t = super::output::Table::new(&[
        "NAME",
        "NAMESPACE",
        "TYPE",
        "VIP",
        "PORT",
        "READY/TOTAL",
        "DNS",
        "STACK",
    ]);
    for r in &rows {
        let ready = if r.service_type == ServiceType::VirtualIp.as_str() {
            format!(
                "{}/{}",
                r.ready.map_or_else(|| "?".to_string(), |n| n.to_string()),
                r.backends
            )
        } else {
            r.backends.to_string()
        };
        t.row(vec![
            r.name.clone(),
            r.namespace.clone(),
            r.service_type.clone(),
            r.vip.clone().unwrap_or_else(|| "-".to_string()),
            r.port.to_string(),
            ready,
            r.dns.clone(),
            r.stack.clone().unwrap_or_else(|| "-".to_string()),
        ]);
    }
    t.drop_uninformative().print();
    Ok(())
}

/// `delonix describe services <name>` — the generic verb's target.
pub(crate) fn cmd_describe(names: &[String]) -> Result<()> {
    for name in names {
        let Some(def) = delonix_sdn::infra::service_list()
            .into_iter()
            .find(|d| d.name == *name)
        else {
            return Err(Error::NotFound(format!("service: {name}")));
        };
        let backends = match_count(&def.namespace, &def.match_labels);
        let mut d = super::output::Describe::new();
        d.field("Name", &def.name);
        d.field("Namespace", &def.namespace);
        d.field(
            "Selector",
            def.match_labels
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(","),
        );
        d.field("Type", def.service_type.as_str());
        d.field("Port", def.port.to_string());
        d.field(
            "DNS",
            delonix_sdn::infra::service_fqdn(&def.name, &def.namespace),
        );
        d.field("Backends", backends.to_string());
        if def.service_type == ServiceType::VirtualIp {
            d.field("VIP", def.vip.clone().unwrap_or_else(|| "-".into()));
            d.field(
                "Readiness",
                readiness_field(&def.readiness.unwrap_or_default()),
            );
            let states = holder_states();
            match state_of(&states, &def) {
                Some(st) => {
                    d.field("Ready", format!("{}/{}", st.ready(), st.backends.len()));
                    if st.ready() == 0 {
                        d.field(
                            "State",
                            super::po::t("no backend ready: the VIP refuses connections"),
                        );
                    }
                    for b in &st.backends {
                        d.field(&format!("  {}", b.name), format!("{} {}", b.ip, b.state));
                    }
                }
                None => {
                    d.field(
                        "Ready",
                        super::po::t("unknown (the holder could not be asked)"),
                    );
                }
            }
        }
        d.field_opt("Stack", def.labels.get(super::reconcile::STACK_LABEL));
        d.field_opt("Managed by", def.labels.get(super::reconcile::MANAGED_BY));
        d.print();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::reconcile::{plan, Action, Actual};

    fn doc(nome: &str, ns: &str, labels: &[(&str, &str)], port: u16) -> ManifestDoc {
        let sel = labels
            .iter()
            .map(|(k, v)| format!("      {k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        serde_yaml::from_str(&format!(
            "apiVersion: delonix.io/v1\nkind: Service\nmetadata:\n  name: {nome}\n  namespace: {ns}\n\
             spec:\n  selector:\n    matchLabels:\n{sel}\n  port: {port}\n"
        ))
        .unwrap()
    }

    fn actual_de(nome: &str, labels: &[(&str, &str)], port: u16, dono: Option<&str>) -> Actual {
        let mut f = BTreeMap::new();
        f.insert(
            "matchLabels".into(),
            labels
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(","),
        );
        f.insert("port".into(), port.to_string());
        Actual {
            kind: "Service".into(),
            name: nome.into(),
            fields: f,
            owner: dono.map(String::from),
            last_applied: None,
        }
    }

    #[test]
    fn um_servico_declarado_e_desejado_com_a_selector_ordenada() {
        let d = doc("web", "teamA", &[("app", "web")], 8080);
        let des = desired(&d).unwrap();
        assert_eq!(des.kind, "Service");
        assert_eq!(des.fields.get("matchLabels").unwrap(), "app=web");
        assert_eq!(des.fields.get("port").unwrap(), "8080");
        assert!(des.ownable);
        assert!(des.converges);
    }

    /// Um `Service` retirado do manifesto é candidato a remoção — o mesmo
    /// contrato que `NetworkRoute`/`NetworkAccessRule` já garantem, aqui pela
    /// primeira vez para este Kind.
    #[test]
    fn um_servico_tirado_do_manifesto_e_candidato_a_remocao() {
        let p = plan(
            &[],
            &[actual_de("web", &[("app", "web")], 8080, Some("s"))],
            "s",
        );
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].action, Action::Delete);
        assert_eq!(p[0].name, "web");
    }

    /// `ownable: true` é o que faz um `Service` sem dono passar a `Adopt` em
    /// vez de `NoOp` — sem isto nunca ganha carimbo e nunca é podável.
    #[test]
    fn um_servico_ja_existente_e_adoptado_pela_stack_que_o_declara() {
        let p = plan(
            &[desired(&doc("web", "teamA", &[("app", "web")], 8080)).unwrap()],
            &[actual_de("teamA/web", &[("app", "web")], 8080, None)],
            "s",
        );
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].action, Action::Adopt, "{p:?}");
    }

    fn vip_doc(vip: Option<&str>) -> ManifestDoc {
        let vip = vip.map(|v| format!("  vip: {v}\n")).unwrap_or_default();
        serde_yaml::from_str(&format!(
            "apiVersion: delonix.io/v1\nkind: Service\nmetadata:\n  name: web\n  namespace: teamA\n\
             spec:\n  selector:\n    matchLabels:\n      app: web\n  port: 8080\n  type: VirtualIP\n{vip}"
        ))
        .unwrap()
    }

    /// `type: DNS` (the default) compares exactly the two fields it always did
    /// (ADR-0066 D1: «byte for byte») — a third field would make every Service
    /// stamped before this release plan a change.
    #[test]
    fn a_dns_service_compares_only_what_it_always_did() {
        let des = desired(&doc("web", "teamA", &[("app", "web")], 8080)).unwrap();
        assert_eq!(
            des.fields.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["matchLabels", "port"]
        );
    }

    #[test]
    fn a_virtual_ip_service_compares_type_vip_and_readiness() {
        let des = desired(&vip_doc(None)).unwrap();
        assert_eq!(des.fields["type"], "VirtualIP");
        assert_eq!(des.fields["vip"], "auto");
        assert_eq!(
            des.fields["readiness"],
            "interval=2s,timeout=500ms,success=1,failure=2"
        );
        assert_eq!(
            desired(&vip_doc(Some("10.90.4.20"))).unwrap().fields["vip"],
            "10.90.4.20"
        );
    }

    /// Moving the VIP or turning a DNS Service into a VirtualIP one converges
    /// in place (ADR-0066 D1: hot fields) — a `Replace` would drop the record
    /// and its lease for nothing.
    #[test]
    fn changing_the_vip_or_the_type_plans_an_update_not_a_replace() {
        let mut current = actual_de("teamA/web", &[("app", "web")], 8080, Some("s"));
        let p = plan(&[desired(&vip_doc(None)).unwrap()], &[current.clone()], "s");
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].action, Action::Update, "{p:?}");
        current.fields = desired(&vip_doc(None)).unwrap().fields;
        let p = plan(
            &[desired(&vip_doc(Some("10.90.4.20"))).unwrap()],
            &[current],
            "s",
        );
        assert_eq!(p[0].action, Action::Update, "{p:?}");
    }

    #[test]
    fn vip_and_readiness_belong_to_virtual_ip_only() {
        let mut spec: ServiceSpec = manifest::spec_of(&vip_doc(Some("10.90.4.20"))).unwrap();
        assert!(spec.validate().is_ok());
        spec.service_type = None;
        assert!(
            spec.validate().is_err(),
            "a DNS Service with a vip is refused"
        );
        let bad: ServiceSpec = manifest::spec_of(&vip_doc(Some("10.90.4.300"))).unwrap();
        assert_eq!(bad.validate().unwrap_err().number(), 1361);
    }

    /// Mudar a porta é uma alteração REAL — `converges: true` tem de a fazer
    /// planear `Update`, não `NoOp`.
    #[test]
    fn mudar_a_porta_planeia_um_update() {
        let p = plan(
            &[desired(&doc("web", "teamA", &[("app", "web")], 9090)).unwrap()],
            &[actual_de("teamA/web", &[("app", "web")], 8080, Some("s"))],
            "s",
        );
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].action, Action::Update, "{p:?}");
    }
}
