//! **The declarative reconciler** — decides what has to change for the machine
//! to match the manifest, and decides it WITHOUT touching the machine.
//!
//! Until this module existed, `stack apply` only ever created: a resource that
//! already existed printed `already exists, nothing to do` and the command
//! returned 0. Changing the image in the manifest and re-applying did nothing
//! and reported success — the declarative twin of the dishonest reporting the
//! v0.37.0 audit removed from the imperative CLI, and worse here, because the
//! user changed the file on purpose and that intent was thrown away.
//!
//! # Why this is a pure function
//!
//! [`plan`] takes an already-read snapshot of both sides and returns a list of
//! [`Change`]. It never opens a store, never runs a command, never needs a
//! namespace or a privilege. That is what makes the interesting cases — a field
//! removed from the manifest, a resource owned by another stack, a change that
//! cannot be applied hot — testable as plain data, in milliseconds, with no
//! host state. Same discipline as `conditions::conditions_for`,
//! `vmbridge::bridge_plan` and `vm::resolve_vm_defaults`.
//!
//! # Field names are the ones the user typed
//!
//! [`Desired::fields`] and [`Actual::fields`] are keyed by the **manifest**
//! field name (`image`, `ports`, `restartPolicy`), never by the internal record
//! name (`memory_max`, `restart_policy`). The plan is read by whoever wrote the
//! YAML; naming a field they cannot find in their own file makes the diff
//! useless. Each Kind's module owns the translation.
//!
//! # Three-way, not two-way
//!
//! Comparing desired against actual cannot distinguish «the user deleted this
//! field from the manifest» from «this field was never ours». A two-way diff
//! has to pick one and is wrong in half the cases: either it keeps reverting
//! settings a human made with `container update`, or it never honours a removal.
//!
//! So the last spec we applied is kept on the resource itself (the
//! `delonix.io/last-applied` annotation — the mechanism `kubectl` uses, in the
//! place `kubectl` puts it) and the rule becomes:
//!
//! | in manifest | on machine | in last-applied | verdict |
//! |---|---|---|---|
//! | yes | differs | — | converge to the manifest |
//! | no | present | yes | **we** set it and it is gone from the file → revert |
//! | no | present | no | never ours → leave it alone |
//!
//! A resource with no `last-applied` at all (adopted, or created by hand) falls
//! into the last row for every field: the first apply only ever adds.

use crate::kinds as k;
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

/// Label carrying the stack that owns a resource. Membership derived from a
/// label, with no registry of its own, is the idiom this repo already uses for
/// pods (`pod::POD_LABEL`) and compose projects
/// (`compose::COMPOSE_PROJECT_LABEL`) — and the reason is the same: a second
/// registry drifts out of sync with the thing it describes.
pub const STACK_LABEL: &str = "delonix.io/stack";
/// Annotation carrying the last applied field map (compact JSON). An
/// annotation and not a label: it is not identifying and it is large — a whole
/// spec inside a label would wreck every listing that prints labels.
pub const LAST_APPLIED: &str = "delonix.io/last-applied";
/// Label marking a resource as created by a declarative apply.
pub const MANAGED_BY: &str = "delonix.io/managed-by";

/// What a resource in the manifest asks for.
#[derive(Debug, Clone)]
pub struct Desired {
    pub kind: String,
    pub name: String,
    /// Manifest-named field → normalized value. Only fields the Kind knows how
    /// to compare belong here; a field nobody can read back off the machine
    /// would show as a permanent difference.
    pub fields: BTreeMap<String, String>,
    /// Whether this Kind converges in this version. `false` means «this Kind is
    /// still ensure-present» — the plan SAYS so rather than omitting the
    /// resource, because a plan that hides a resource reads as «no changes».
    pub converges: bool,
    /// Whether a stack can OWN this resource.
    ///
    /// `false` for shared, content-addressed things — an `Image` is the clear
    /// case: the same `nginx:alpine` backs every stack on the host, so stamping
    /// it for one and pruning it when that one stops declaring it would pull the
    /// image out from under the others. Docker treats images as shared cache
    /// rather than owned resources, and so does this.
    ///
    /// A non-ownable resource is never adopted (it has no owner by design, so
    /// «unowned» is not a state to fix) and never a deletion candidate.
    pub ownable: bool,
}

/// What is actually on the machine.
#[derive(Debug, Clone)]
pub struct Actual {
    pub kind: String,
    pub name: String,
    pub fields: BTreeMap<String, String>,
    /// Value of [`STACK_LABEL`], if any.
    pub owner: Option<String>,
    /// Decoded [`LAST_APPLIED`], if any.
    pub last_applied: Option<BTreeMap<String, String>>,
}

/// What will happen to one resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Does not exist yet.
    Create,
    /// Exists, is not owned by any stack, and will be taken over (stamped and
    /// converged). This is what removes the need for a separate `import`
    /// command — the resource is identified by name, which is the only identity
    /// a manifest ever gives it.
    Adopt,
    /// Converges without recreating and without changing the PID.
    Update,
    /// Needs to be destroyed and recreated. Refused unless explicitly asked for.
    Replace,
    /// Converges by a STOP and a START of the same resource, with nothing
    /// destroyed (ADR-0012): the PID changes, so it is not an [`Action::Update`],
    /// and the disk survives, so it is not an [`Action::Replace`]. Disruptive,
    /// not destructive — refused unless the operator authorises the downtime.
    Reboot,
    /// Already matches.
    NoOp,
    /// Owned by this stack, gone from the manifest.
    Delete,
    /// Owned by ANOTHER stack — never touched.
    Conflict,
    /// Present, and this Kind does not converge yet in this version.
    NotConverged,
}

impl Action {
    /// Whether this action changes anything on the machine. Drives the
    /// `--detailed-exitcode` contract and the `changed` field of `-o json`.
    pub fn is_change(self) -> bool {
        matches!(
            self,
            Action::Create
                | Action::Adopt
                | Action::Update
                | Action::Replace
                | Action::Reboot
                | Action::Delete
        )
    }

    /// The one-character marker in the rendered plan.
    pub fn marker(self) -> &'static str {
        match self {
            Action::Create => "+",
            Action::Adopt => "+~",
            Action::Update => "~",
            Action::Replace => "-/+",
            Action::Reboot => "~↻",
            Action::NoOp => "=",
            Action::Delete => "-",
            Action::Conflict => "✗",
            Action::NotConverged => "!",
        }
    }
}

/// One field that differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldDiff {
    pub field: String,
    /// Value on the machine (`None` = absent).
    pub from: Option<String>,
    /// Value the manifest asks for (`None` = the manifest dropped it).
    pub to: Option<String>,
    /// Whether this field converges without recreating the resource.
    pub hot: bool,
    /// Whether this field converges by a stop and a start of the resource,
    /// keeping its disk (ADR-0012). Never `true` together with `hot`.
    /// Omitted from the JSON when `false`, so a reader of an older payload
    /// sees exactly what it saw before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reboot: bool,
}

/// The decision for one resource.
#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub kind: String,
    pub name: String,
    pub action: Action,
    /// Set when the action needs justifying (which field forces a replace, who
    /// owns a conflicting resource). Never a generic sentence — the point is to
    /// name the cause.
    ///
    /// **Stays in English, always.** It is part of the `-o json` payload, and
    /// ADR-0005's whole point is that machine-readable output does not change
    /// with the locale. The human rendering is composed separately, from the
    /// structured fields below — which is exactly why they exist as data and not
    /// only as a baked sentence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// On `Replace`: every field that cannot converge live. All of them, not
    /// just the first — someone editing the manifest to avoid a recreation needs
    /// the whole list, and learning them one run at a time is the kind of
    /// drip-feed that makes people stop reading output.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cold_fields: Vec<String>,
    /// On `Reboot`: every field that needs the stop and the start. Same
    /// «all of them, not just the first» rule as `cold_fields`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reboot_fields: Vec<String>,
    /// On `Conflict`: the stack that already owns this resource.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Prerequisites this resource needs and the host does not have — the
    /// difference between «exists» and «exists and works».
    ///
    /// Until this field, that difference was unsayable in a plan: `conditions.rs`
    /// computed them and only the END of an `apply` printed them, so a user
    /// learned that their NFS volume does not actually mount AFTER creating it,
    /// and only if the apply got that far. A plan that says `+ Volume/media`
    /// and nothing else is not wrong, it is just not the whole truth.
    ///
    /// Only the FAILING ones are carried: a list of satisfied prerequisites is
    /// noise, and noise in a plan is what makes people stop reading it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<crate::condition::Condition>,
    pub diffs: Vec<FieldDiff>,
    /// Convenience for consumers: `action.is_change()`.
    pub changed: bool,
    /// The digest of this document's plan, for the Kinds whose plan is
    /// decided from a remote provider's state (ADR-0059 D4). `stack apply
    /// --plan-digest` refuses when it no longer matches.
    #[serde(rename = "planDigest", skip_serializing_if = "Option::is_none")]
    pub plan_digest: Option<String>,
}

impl Change {
    fn new(kind: &str, name: &str, action: Action) -> Self {
        Change {
            kind: kind.to_string(),
            name: name.to_string(),
            action,
            reason: None,
            cold_fields: Vec::new(),
            reboot_fields: Vec::new(),
            owner: None,
            conditions: Vec::new(),
            diffs: Vec::new(),
            changed: action.is_change(),
            plan_digest: None,
        }
    }
    fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// Fields that converge WITHOUT recreating the resource, per Kind.
///
/// **This table is a promise, and it must match what the converge step can
/// actually do.** Listing a field here that the executor cannot apply hot turns
/// a clean `Replace` into an `Update` that fails halfway — strictly worse than
/// declaring the replace up front. Every entry below is backed by a real code
/// path that was already there and already tested:
///
/// - `Container`: `cmd::container::cmd_update` (`--publish-add/-rm`,
///   `--volume-add/-rm`, `--memory`, `--cpus`, `--net-rate/-burst`). It
///   reconfigures the live dataplane and the live cgroup, so the PID does not
///   change — the difference of substance between this engine and Docker.
///   `network` is deliberately NOT here: `cmd_update` only connects and
///   disconnects ADDITIONAL networks, it cannot move a container's primary one.
/// - `Volume`: `VolumeStore::set_quota`.
/// - `Network`: `NetworkStore::add_overlay_peer`.
fn hot_fields(kind: &str) -> &'static [&'static str] {
    match kind {
        // `credentials` is the declared login REFERENCE (ADR-0048 D4), bookkeeping
        // on the record: changing it never touches the process.
        k::CONTAINER => &[
            "ports",
            "volumes",
            "memory",
            "cpus",
            "netBps",
            "netBurst",
            "credentials",
        ],
        // `alertPct` joins `quota` as bookkeeping on the record. `parent` does
        // NOT: a share is a subdirectory of the volume it names, so repointing
        // it means a different directory and the bytes already written stay in
        // the old one — cold, which turns it into a refused `Replace` rather
        // than a silent move.
        k::VOLUME => &["quota", "alertPct", "labels"],
        // Measured on PVE 9.2.2 against a running container before being
        // declared: `memory` and `swap` land in its cgroup at once, `cores` in
        // its cpuset a few seconds later, and nothing is left pending. `image`,
        // `entrypoint`, `env` and `network` stay cold (ADR-0058, plan 63
        // slice 4). `rootfs` is grow-only, see `grow_only_fields`.
        k::SYSTEM_CONTAINER => &["memory", "swap", "cores"],
        // The only things about a VM that change without recreating it: its
        // labels and its declared login reference, both bookkeeping on the
        // record. Every other compared field
        // defines the machine, and stays a refused `Replace` — which discards
        // the disk.
        k::VM => &["labels", "credentials"],
        k::NETWORK => &["peers", "labels"],
        // Fetching a ref destroys nothing — an image is shared cache, so its
        // whole comparable surface converges without recreating anything.
        k::IMAGE => &["ref", "digest"],
        // `apply_fw_doc` replaces the whole direction in place, with no restart.
        // `target`/`direction` stay cold: they identify WHICH direction of WHICH
        // container the policy governs, so changing one leaves the old target's
        // rules exactly where they were.
        k::FIREWALL_POLICY => &["defaultPolicy", "rules", "scope"],
        // The proxy's config is COLLECTIVE and `httproute::converge_all`
        // recomposes ALL of it, so every comparable field converges without
        // destroying anything — `rules` by SIGHUP (same PID, no downtime),
        // `entrypoints`/`tls` by restarting the proxy, which `converge_all` now
        // does instead of warning that it did not.
        //
        // **This arm was MISSING, and its absence made three things false at
        // once**: no field was hot, so every change planned as `Replace`; the
        // `Replace` needs a teardown a collective config cannot give per
        // document; and the `converge_all` arm in `stack.rs` was therefore
        // unreachable. Meanwhile `RECONCILED_HTTPROUTE_FIELDS` documented
        // «everything converges hot». The table is a promise — this is it being
        // kept.
        k::HTTP_ROUTE | k::INGRESS => &["entrypoints", "tls", "rules", "hosts", "pool"],
        // `service::apply_one` fully overwrites the registry entry on every
        // apply — same "converges without recreating anything" shape as
        // `Image`'s ref/digest just above.
        // The apply layer writes a Secret in place; nothing is recreated.
        k::SECRET => &["keys", "fingerprint"],
        k::SERVICE => &["matchLabels", "port"],
        // An apply that died mid-way is finished by applying again: the
        // record's ledger says where it stopped, and the provider adopts what
        // the dead process staged (ADR-0059 D4). Every other field of a
        // gateway stays cold — the appliance has no update in place.
        k::NETWORK_GATEWAY => &["applied"],
        // A subnet's gateway and DHCP ranges are written in place
        // (ADR-0063 D2), and an IPAM gateway entry left away from its subnet's
        // gateway is put back by the apply's repair (`gatewayEntries`, N18);
        // which subnets exist (`subnets`, their CIDRs) and the IPAM
        // controller (`ipam`) stay cold.
        k::NETWORK_ZONE => &[
            "applied",
            "reservations",
            "dns",
            "subnetSettings",
            "gatewayEntries",
        ],
        // `ippool::apply_one` overwrites the definition and keeps the leases.
        k::IPPOOL => &["addresses", "announce", "interface"],
        // The use of a pool is two numbers in a record; the pool itself is
        // never recreated by the engine.
        k::STORAGE_POOL => &["maxRatio", "alertPct"],
        // Every comparable field of `policy.json` converges hot: `apply_one`
        // (in `cmd::policy`) rewrites the whole file, which needs no restart
        // of anything — the ceiling just takes effect on the next admission.
        k::RUNTIME_POLICY => &[
            "mode",
            "denyPrivileged",
            "denyHostNetwork",
            "denyLatestTag",
            "allowedRegistries",
            "denyDevicePassthrough",
            "denyLatestVmImage",
            "allowedImageUrlHosts",
            // The four below were compared and NOT listed here, so turning
            // the private-egress guard on in an applied manifest planned a
            // `Replace` of a singleton that has no teardown — refused, and
            // the guard stayed off. `cmd::policy::RECONCILED_RUNTIME_POLICY_
            // FIELDS` must equal this list; a test there keeps it so.
            "allowedSourcePrefixes",
            "allowSourceCheckOptOut",
            "denyPrivateEgress",
            "allowedEgressPrefixes",
            "workloadSecurity",
        ],
        _ => &[],
    }
}

fn is_hot(kind: &str, field: &str) -> bool {
    hot_fields(kind).contains(&field)
}

/// Fields that converge live only when they GROW, per Kind: a number the
/// provider can raise in place and never lower.
///
/// - `SystemContainer.rootfs`: the Proxmox node grows a container's root
///   volume on a running container (`PUT …/resize`, measured on PVE 9.2.2)
///   and refuses to shrink it. A smaller size is a recreate, so it plans as a
///   `Replace` that `--replace` has to authorise, like any other cold field.
fn grow_only_fields(kind: &str) -> &'static [&'static str] {
    match kind {
        k::SYSTEM_CONTAINER => &["rootfs"],
        _ => &[],
    }
}

/// Whether one field change converges live: a hot field always does, a
/// grow-only field when both sides are numbers and it does not shrink.
pub fn is_hot_change(kind: &str, field: &str, from: Option<&str>, to: Option<&str>) -> bool {
    if is_hot(kind, field) {
        return true;
    }
    if !grow_only_fields(kind).contains(&field) {
        return false;
    }
    match (
        from.and_then(|v| v.parse::<u64>().ok()),
        to.and_then(|v| v.parse::<u64>().ok()),
    ) {
        (Some(f), Some(t)) => t >= f,
        _ => false,
    }
}

/// The hot table, for the per-Kind tests that have to assert their own half of
/// the promise. Reading it is the only way a module can check that what it
/// declares comparable is also something the executor can apply — and those
/// tests live in the crates that own the executors, so this cannot be
/// `#[cfg(test)]` here.
pub fn hot_fields_for(kind: &str) -> &'static [&'static str] {
    hot_fields(kind)
}

/// What the plan knows about a resource that is NOT one of its fields — read
/// off the machine by the module that owns the Kind, and handed in as data so
/// [`plan_with_facts`] stays a pure function.
///
/// It exists for the VM (ADR-0012, ADR-0068): whether a vCPU change is free, a
/// reboot or a recreate depends on whether the VM runs, on the ceiling it was
/// created with and on what its backend can redefine across a reboot — none of
/// which is something a manifest declares, so none of it can be a compared
/// field without becoming a permanent difference.
///
/// An empty `Facts` (the [`Default`]) changes nothing: every Kind without facts
/// plans exactly as [`plan`] always did.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// `Some(true)` = running, `Some(false)` = stopped (a reboot costs nothing:
    /// the change lands in the record and the next start uses it — ADR-0012
    /// question 4), `None` = unknown or paused, which is planned as running: a
    /// gate asked for in vain is cheaper than downtime nobody authorised.
    pub running: Option<bool>,
    /// Fields that converge by a stop and a start without losing the disk. Per
    /// RESOURCE and not per Kind, because it is the backend that decides: a
    /// Cloud Hypervisor VM's `network` is rebuilt from its record at boot,
    /// while a Proxmox node keeps its own NIC config and would boot the old one.
    pub reboot: BTreeSet<String>,
    /// Fields that converge LIVE when they grow up to a ceiling (ADR-0068 D9) —
    /// vCPUs and memory with hotplug headroom. Only honoured while running.
    pub live_growth: BTreeMap<String, LiveGrowth>,
    /// Fields compared by VALUE through a parser rather than as strings, so
    /// `2G` and `2048M` are the same memory and never a difference.
    pub numeric: BTreeMap<String, fn(&str) -> Option<u64>>,
}

/// A field that grows live up to `ceiling`, in the units `parse` returns.
#[derive(Debug, Clone, Copy)]
pub struct LiveGrowth {
    pub ceiling: u64,
    pub parse: fn(&str) -> Option<u64>,
}

/// How one field change converges — the cost model ADR-0012 adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convergence {
    /// In place, same PID.
    Hot,
    /// Stop + start, disk kept.
    Reboot,
    /// Destroy + recreate.
    Cold,
}

/// Classifies one field change. The SAME function decides in `plan` and in
/// `apply`, so the two can never disagree about what costs downtime.
pub fn classify(
    kind: &str,
    field: &str,
    from: Option<&str>,
    to: Option<&str>,
    facts: Option<&Facts>,
) -> Convergence {
    if is_hot_change(kind, field, from, to) {
        return Convergence::Hot;
    }
    if let Some(f) = facts {
        if f.running == Some(true) {
            if let Some(g) = f.live_growth.get(field) {
                if let (Some(a), Some(b)) = (
                    from.and_then(|v| (g.parse)(v)),
                    to.and_then(|v| (g.parse)(v)),
                ) {
                    if b > a && b <= g.ceiling {
                        return Convergence::Hot;
                    }
                }
            }
        }
        if f.reboot.contains(field) {
            return Convergence::Reboot;
        }
    }
    Convergence::Cold
}

/// Whether two values of one field are the same thing. Strings, unless the
/// facts name a parser for the field. Public so `drift` compares with the
/// SAME rule the plan uses: `1G` and `1024M` are one memory in both, or the
/// plan says «nothing to do» while drift stays red forever.
pub fn same_value(field: &str, a: &str, b: &str, facts: Option<&Facts>) -> bool {
    if a == b {
        return true;
    }
    match facts.and_then(|f| f.numeric.get(field)) {
        Some(parse) => matches!((parse(a), parse(b)), (Some(x), Some(y)) if x == y),
        None => false,
    }
}

fn field_diff(
    kind: &str,
    field: &str,
    from: Option<&String>,
    to: Option<&String>,
    facts: Option<&Facts>,
) -> FieldDiff {
    let class = classify(
        kind,
        field,
        from.map(String::as_str),
        to.map(String::as_str),
        facts,
    );
    FieldDiff {
        field: field.to_string(),
        from: from.cloned(),
        to: to.cloned(),
        hot: class == Convergence::Hot,
        reboot: class == Convergence::Reboot,
    }
}

/// Computes the three-way diff of one resource.
fn diff_fields(
    kind: &str,
    desired: &Desired,
    actual: &Actual,
    facts: Option<&Facts>,
) -> Vec<FieldDiff> {
    let empty = BTreeMap::new();
    let last = actual.last_applied.as_ref().unwrap_or(&empty);
    let keys: BTreeSet<&String> = desired
        .fields
        .keys()
        .chain(actual.fields.keys())
        .chain(last.keys())
        .collect();
    let mut out = Vec::new();
    for key in keys {
        let d = desired.fields.get(key);
        let a = actual.fields.get(key);
        match (d, a) {
            // Present in the manifest and equal on the machine.
            (Some(d), Some(a)) if same_value(key, d, a, facts) => {}
            // Present in the manifest — converge to it (whether or not the
            // machine has it today).
            (Some(d), a) => out.push(field_diff(kind, key, a, Some(d), facts)),
            // Gone from the manifest. Revert it ONLY if we were the ones who
            // set it — otherwise it belongs to a human's `container update` or
            // to a default, and reverting it would be this tool fighting its
            // own user.
            (None, Some(a)) => {
                if last.contains_key(key) {
                    out.push(field_diff(kind, key, Some(a), None, facts));
                }
            }
            (None, None) => {}
        }
    }
    out
}

/// Decides what has to change. Pure: both sides come in already read.
///
/// `stack` is the name that owns this apply; it is what separates «mine, gone
/// from the file, so remove it» from «someone else's, never touch it».
pub fn plan(desired: &[Desired], actual: &[Actual], stack: &str) -> Vec<Change> {
    plan_with_facts(desired, actual, stack, &BTreeMap::new())
}

/// [`plan`], with what the owning modules read off the machine about each
/// resource, keyed by `(kind, name)` (see [`Facts`]). Still pure.
pub fn plan_with_facts(
    desired: &[Desired],
    actual: &[Actual],
    stack: &str,
    facts: &BTreeMap<(String, String), Facts>,
) -> Vec<Change> {
    let mut out = Vec::new();
    for d in desired {
        let found = actual.iter().find(|a| a.kind == d.kind && a.name == d.name);
        let Some(a) = found else {
            out.push(Change::new(&d.kind, &d.name, Action::Create));
            continue;
        };
        // Owned by another stack: refuse before computing anything. Two stacks
        // converging the same resource would flap it between two shapes on
        // every apply, and neither owner would understand why.
        if let Some(owner) = &a.owner {
            if owner != stack {
                let mut c = Change::new(&d.kind, &d.name, Action::Conflict)
                    .with_reason(format!("owned by stack '{owner}'"));
                c.owner = Some(owner.clone());
                out.push(c);
                continue;
            }
        }
        if !d.converges {
            out.push(
                Change::new(&d.kind, &d.name, Action::NotConverged)
                    .with_reason("this Kind is ensure-present in this version"),
            );
            continue;
        }
        let f = facts.get(&(d.kind.clone(), d.name.clone()));
        let diffs = diff_fields(&d.kind, d, a, f);
        // A non-ownable resource is never «unowned and therefore to adopt»:
        // having no owner is its normal state, not something to fix.
        let unmanaged = d.ownable && a.owner.is_none();
        let cold: Vec<String> = diffs
            .iter()
            .filter(|x| !x.hot && !x.reboot)
            .map(|x| x.field.clone())
            .collect();
        let reboot: Vec<String> = diffs
            .iter()
            .filter(|x| x.reboot)
            .map(|x| x.field.clone())
            .collect();
        // A reboot of something that is not running costs nothing: the change
        // lands where the next start reads it, so it converges as an `Update`
        // (ADR-0012 question 4). The plan shows the difference, because the
        // action itself is what tells the operator the cost.
        let stopped = f.and_then(|f| f.running) == Some(false);
        let action = if !cold.is_empty() {
            Action::Replace
        } else if !reboot.is_empty() && !stopped {
            Action::Reboot
        } else if !diffs.is_empty() {
            Action::Update
        } else if unmanaged {
            Action::Adopt
        } else {
            Action::NoOp
        };
        let mut change = Change::new(&d.kind, &d.name, action);
        if action == Action::Replace {
            // Name every cold field, not just the first: the user fixing the
            // manifest to avoid a replace needs the whole list, and finding out
            // one field at a time across several runs is the kind of drip-feed
            // that makes people stop reading output.
            change = change.with_reason(format!("does not converge hot: {}", cold.join(", ")));
            change.cold_fields = cold;
        } else if action == Action::Reboot {
            change = change.with_reason(format!(
                "converges by a stop and a start, the disk kept: {}",
                reboot.join(", ")
            ));
            change.reboot_fields = reboot;
        } else if action == Action::Adopt {
            change = change.with_reason("exists and belongs to no stack — will be taken over");
        }
        change.diffs = diffs;
        out.push(change);
    }
    // Resources this stack owns that the manifest no longer declares. Anything
    // without our stamp is invisible here on purpose — an apply must never
    // consider deleting something it did not create.
    for a in actual {
        if a.owner.as_deref() != Some(stack) {
            continue;
        }
        // (A non-ownable resource never carries the label, so it never reaches
        // here — the guard above is what keeps shared content out of `--prune`.)
        if desired.iter().any(|d| d.kind == a.kind && d.name == a.name) {
            continue;
        }
        out.push(
            Change::new(&a.kind, &a.name, Action::Delete)
                .with_reason("no longer declared in the manifest"),
        );
    }
    out
}

/// Counts per action, for the summary line and the exit code.
pub fn summary(changes: &[Change]) -> BTreeMap<&'static str, usize> {
    let mut m = BTreeMap::new();
    for c in changes {
        let key = match c.action {
            Action::Create => "create",
            Action::Adopt => "adopt",
            Action::Update => "update",
            Action::Replace => "replace",
            Action::Reboot => "reboot",
            Action::NoOp => "unchanged",
            Action::Delete => "delete",
            Action::Conflict => "conflict",
            Action::NotConverged => "not_converged",
        };
        *m.entry(key).or_insert(0) += 1;
    }
    m
}

/// Splits a comma-joined list field into what has to be ADDED and what has to be
/// REMOVED to get from `from` to `to`.
///
/// The converge step needs a delta, not two lists: `container update` speaks
/// `--publish-add`/`--publish-rm`, not «here is the new set». `None` on either
/// side means «no such field», which is an empty list.
///
/// Removals come out first for the caller to apply first — `cmd_update` already
/// orders its own operations that way, so that `--publish-rm 8080
/// --publish-add 8080:9000` works in a single call instead of colliding on a
/// port that is still taken.
pub fn list_delta(from: Option<&str>, to: Option<&str>) -> (Vec<String>, Vec<String>) {
    let split = |s: Option<&str>| -> BTreeSet<String> {
        s.unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect()
    };
    let (a, b) = (split(from), split(to));
    let removed = a.difference(&b).cloned().collect();
    let added = b.difference(&a).cloned().collect();
    (removed, added)
}

/// Serializes a field map for the [`LAST_APPLIED`] annotation. Compact JSON, so
/// it survives the `key=value` line format of the network record (see
/// `NetworkStore::set_metadata`) — `serde_json` escapes a newline inside a
/// string as two characters, never a literal one.
pub fn encode_last_applied(fields: &BTreeMap<String, String>) -> String {
    serde_json::to_string(fields).unwrap_or_else(|_| "{}".to_string())
}

/// The plan field that carries a document's own `metadata.labels`.
pub const LABELS_FIELD: &str = "labels";

/// Label keys the engine writes itself (ownership, management). A document
/// cannot set them and the `labels` field never reports them: they are how the
/// reconciler knows whose a resource is, not something a manifest declares.
pub const RESERVED_LABEL_PREFIX: &str = "delonix.io/";

/// `metadata.labels` as a plan field: the labels a document may own, as one
/// canonical JSON object (keys ordered). `None` when there are none — a
/// document that declares no labels says nothing about them, so labels put on
/// the resource by hand are left alone (three-way diff).
///
/// Both sides of the comparison come through here — the document's labels and
/// the record's — which is what keeps an unchanged manifest at zero
/// differences.
pub fn user_labels_field(labels: &BTreeMap<String, String>) -> Option<String> {
    let own: BTreeMap<&String, &String> = labels
        .iter()
        .filter(|(k, _)| !k.starts_with(RESERVED_LABEL_PREFIX))
        .collect();
    if own.is_empty() {
        return None;
    }
    serde_json::to_string(&own).ok()
}

/// What moving the `labels` field from `from` to `to` means for the record: a
/// key in `to` is set, a key only in `from` is removed (`None`). The shape the
/// stores' `set_metadata` takes — which merges, so the engine's own labels and
/// anything else on the record stay where they are.
pub fn labels_delta(from: Option<&str>, to: Option<&str>) -> Vec<(String, Option<String>)> {
    let parse = |raw: Option<&str>| -> BTreeMap<String, String> {
        raw.and_then(|r| serde_json::from_str(r).ok())
            .unwrap_or_default()
    };
    let (old, new) = (parse(from), parse(to));
    let mut out: Vec<(String, Option<String>)> = old
        .keys()
        .filter(|k| !new.contains_key(*k))
        .map(|k| (k.clone(), None))
        .collect();
    out.extend(new.into_iter().map(|(k, v)| (k, Some(v))));
    out.retain(|(k, _)| !k.starts_with(RESERVED_LABEL_PREFIX));
    out
}

/// Reads back what [`encode_last_applied`] wrote. A corrupt or absent value is
/// `None`, which degrades to «never applied by us» — the conservative side:
/// the reconciler then only ever adds, and never reverts a field on the basis
/// of a value it could not read.
pub fn decode_last_applied(raw: &str) -> Option<BTreeMap<String, String>> {
    serde_json::from_str(raw).ok()
}

#[cfg(test)]
mod tests {

    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn desired(name: &str, fields: &[(&str, &str)]) -> Desired {
        Desired {
            kind: "Container".into(),
            name: name.into(),
            fields: map(fields),
            converges: true,
            ownable: true,
        }
    }

    fn actual(name: &str, fields: &[(&str, &str)], owner: Option<&str>) -> Actual {
        Actual {
            kind: "Container".into(),
            name: name.into(),
            fields: map(fields),
            owner: owner.map(String::from),
            last_applied: None,
        }
    }

    /// A grow-only field converges live when it grows and plans a replace
    /// when it shrinks, and nothing else of the Kind changes behaviour.
    #[test]
    fn a_grow_only_field_is_hot_growing_and_cold_shrinking() {
        let sc = k::SYSTEM_CONTAINER;
        assert!(is_hot_change(sc, "rootfs", Some("1"), Some("2")));
        assert!(is_hot_change(sc, "rootfs", Some("2"), Some("2")));
        assert!(!is_hot_change(sc, "rootfs", Some("2"), Some("1")));
        assert!(!is_hot_change(sc, "rootfs", None, Some("2")));
        assert!(is_hot_change(sc, "memory", Some("512"), Some("256")));
        assert!(!is_hot_change(sc, "image", Some("a"), Some("b")));
        assert!(!is_hot_change(k::VM, "rootfs", Some("1"), Some("2")));
    }

    #[test]
    fn recurso_ausente_e_criado() {
        let p = plan(&[desired("web", &[("image", "nginx")])], &[], "s");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].action, Action::Create);
        assert!(p[0].changed);
    }

    #[test]
    fn recurso_igual_e_noop() {
        let p = plan(
            &[desired("web", &[("image", "nginx")])],
            &[actual("web", &[("image", "nginx")], Some("s"))],
            "s",
        );
        assert_eq!(p[0].action, Action::NoOp);
        assert!(!p[0].changed);
        assert!(p[0].diffs.is_empty());
    }

    /// The whole point of the module: a manifest that changed must produce
    /// work. Before this, `apply` printed «already exists, nothing to do».
    #[test]
    fn campo_quente_produz_update_e_nao_replace() {
        let p = plan(
            &[desired("web", &[("image", "nginx"), ("memory", "512M")])],
            &[actual(
                "web",
                &[("image", "nginx"), ("memory", "256M")],
                Some("s"),
            )],
            "s",
        );
        assert_eq!(p[0].action, Action::Update);
        assert_eq!(p[0].diffs.len(), 1);
        assert_eq!(p[0].diffs[0].field, "memory");
        assert_eq!(p[0].diffs[0].from.as_deref(), Some("256M"));
        assert_eq!(p[0].diffs[0].to.as_deref(), Some("512M"));
        assert!(p[0].diffs[0].hot);
    }

    #[test]
    fn campo_frio_produz_replace_e_nomeia_todos_os_campos() {
        let p = plan(
            &[desired(
                "web",
                &[("image", "nginx:1.27"), ("user", "app"), ("memory", "512M")],
            )],
            &[actual(
                "web",
                &[
                    ("image", "nginx:1.24"),
                    ("user", "root"),
                    ("memory", "256M"),
                ],
                Some("s"),
            )],
            "s",
        );
        assert_eq!(p[0].action, Action::Replace);
        let reason = p[0].reason.as_deref().unwrap();
        assert!(reason.contains("image"), "{reason}");
        assert!(reason.contains("user"), "{reason}");
        // The hot field is still listed in the diff — the user must see
        // everything that changes, not only what forced the replace.
        assert!(p[0].diffs.iter().any(|d| d.field == "memory"));
    }

    /// Three-way, the row that a two-way diff cannot get right: a field the
    /// user set BY HAND (never ours) must survive an apply untouched.
    #[test]
    fn campo_que_nunca_foi_nosso_nao_e_revertido() {
        let a = Actual {
            last_applied: Some(map(&[("image", "nginx")])),
            ..actual(
                "web",
                &[("image", "nginx"), ("ports", "9999:80")],
                Some("s"),
            )
        };
        let p = plan(&[desired("web", &[("image", "nginx")])], &[a], "s");
        assert_eq!(
            p[0].action,
            Action::NoOp,
            "a hand-set field is not ours to revert: {:?}",
            p[0].diffs
        );
    }

    /// The other row: a field WE set, now gone from the manifest, is reverted.
    #[test]
    fn campo_retirado_do_manifesto_e_revertido() {
        let a = Actual {
            last_applied: Some(map(&[("image", "nginx"), ("ports", "8080:80")])),
            ..actual(
                "web",
                &[("image", "nginx"), ("ports", "8080:80")],
                Some("s"),
            )
        };
        let p = plan(&[desired("web", &[("image", "nginx")])], &[a], "s");
        assert_eq!(p[0].action, Action::Update);
        assert_eq!(p[0].diffs[0].field, "ports");
        assert_eq!(p[0].diffs[0].to, None, "reverting means removing");
    }

    #[test]
    fn recurso_sem_dono_e_adoptado_e_nao_duplicado() {
        let p = plan(
            &[desired("web", &[("image", "nginx")])],
            &[actual("web", &[("image", "nginx")], None)],
            "s",
        );
        assert_eq!(p[0].action, Action::Adopt);
        assert!(p[0].changed, "adopting stamps ownership, so it IS a change");
    }

    /// Two stacks converging the same resource would flap it on every apply.
    #[test]
    fn recurso_de_outra_stack_e_conflito_e_nunca_tocado() {
        let p = plan(
            &[desired("web", &[("image", "nginx:1.27")])],
            &[actual("web", &[("image", "nginx:1.24")], Some("outra"))],
            "s",
        );
        assert_eq!(p[0].action, Action::Conflict);
        assert!(p[0].reason.as_deref().unwrap().contains("outra"));
        assert!(
            p[0].diffs.is_empty(),
            "a conflict must not even compute a diff — there is nothing to offer"
        );
    }

    #[test]
    fn so_o_que_a_stack_possui_e_candidato_a_remocao() {
        let p = plan(
            &[],
            &[
                actual("meu", &[("image", "nginx")], Some("s")),
                actual("alheio", &[("image", "nginx")], Some("outra")),
                actual("a-mao", &[("image", "nginx")], None),
            ],
            "s",
        );
        assert_eq!(p.len(), 1, "only ours is a deletion candidate: {p:?}");
        assert_eq!(p[0].name, "meu");
        assert_eq!(p[0].action, Action::Delete);
    }

    /// A Kind outside the converging scope must be DECLARED, never omitted — a
    /// plan that hides a resource reads as «no changes», which is the exact
    /// dishonesty this module exists to remove.
    #[test]
    fn kind_nao_convergente_aparece_no_plano() {
        let d = Desired {
            kind: "Vm".into(),
            name: "db".into(),
            fields: map(&[("memory", "4G")]),
            converges: false,
            ownable: true,
        };
        let a = Actual {
            kind: "Vm".into(),
            name: "db".into(),
            fields: map(&[("memory", "2G")]),
            owner: Some("s".into()),
            last_applied: None,
        };
        let p = plan(&[d], &[a], "s");
        assert_eq!(p[0].action, Action::NotConverged);
        assert!(!p[0].changed);
    }

    #[test]
    fn last_applied_faz_round_trip_e_um_valor_corrompido_degrada_para_none() {
        let m = map(&[("image", "nginx"), ("cmd", "sh -c \"echo\nolá\"")]);
        let raw = encode_last_applied(&m);
        assert!(
            !raw.contains('\n'),
            "must survive a line-based record: {raw}"
        );
        assert_eq!(decode_last_applied(&raw).unwrap(), m);
        assert!(decode_last_applied("not json").is_none());
    }

    #[test]
    fn list_delta_da_o_que_acrescentar_e_o_que_tirar() {
        let (rm, add) = list_delta(Some("8080:80,9090:90"), Some("8080:80,7070:70"));
        assert_eq!(rm, vec!["9090:90".to_string()]);
        assert_eq!(add, vec!["7070:70".to_string()]);
        // A field that did not exist, and one that stopped existing.
        assert_eq!(list_delta(None, Some("a")), (vec![], vec!["a".to_string()]));
        assert_eq!(list_delta(Some("a"), None), (vec!["a".to_string()], vec![]));
        // Equal sets in a different order are not a delta — the same reason the
        // field values are sorted when they are built.
        assert_eq!(list_delta(Some("a,b"), Some("a,b")), (vec![], vec![]));
    }

    #[test]
    fn o_resumo_conta_por_accao() {
        let p = plan(
            &[
                desired("a", &[("image", "x")]),
                desired("b", &[("image", "y")]),
            ],
            &[actual("b", &[("image", "y")], Some("s"))],
            "s",
        );
        let s = summary(&p);
        assert_eq!(s.get("create"), Some(&1));
        assert_eq!(s.get("unchanged"), Some(&1));
    }

    /// Conteúdo PARTILHADO não se adopta nem se poda. A mesma imagem serve
    /// todas as stacks do host; carimbá-la para uma e removê-la quando essa
    /// deixasse de a declarar tirava-a debaixo das outras.
    #[test]
    fn um_recurso_nao_possuivel_nunca_e_adoptado_nem_removido() {
        let d = Desired {
            kind: "Image".into(),
            name: "base".into(),
            fields: map(&[("ref", "alpine:latest")]),
            converges: true,
            ownable: false,
        };
        let a = Actual {
            kind: "Image".into(),
            name: "base".into(),
            fields: map(&[("ref", "alpine:latest")]),
            owner: None,
            last_applied: None,
        };
        let p = plan(&[d], std::slice::from_ref(&a), "s");
        assert_eq!(
            p[0].action,
            Action::NoOp,
            "sem dono é o estado NORMAL de conteúdo partilhado, não algo a corrigir"
        );
        // E fora do manifesto continua a não ser candidato a remoção.
        assert!(
            plan(&[], &[a], "s").is_empty(),
            "conteúdo partilhado nunca entra no --prune"
        );
    }

    /// Without facts about the machine, a VM change is still a `Replace`: the
    /// reboot and hotplug classes depend on the backend and on whether the VM
    /// runs, and a plan that cannot see them must not guess the cheaper one.
    #[test]
    fn without_facts_a_vm_change_is_still_a_recreate() {
        assert!(
            hot_fields("Vm").is_empty(),
            "um campo quente numa VM prometeria um update que o motor não sabe fazer"
        );
        let d = Desired {
            kind: "Vm".into(),
            name: "db".into(),
            fields: map(&[("memory", "4G")]),
            converges: true,
            ownable: true,
        };
        let a = Actual {
            kind: "Vm".into(),
            name: "db".into(),
            fields: map(&[("memory", "2G")]),
            owner: Some("s".into()),
            last_applied: None,
        };
        let p = plan(&[d], &[a], "s");
        assert_eq!(p[0].action, Action::Replace);
        assert_eq!(p[0].cold_fields, vec!["memory".to_string()]);
    }

    /// `policy.json` is a node-wide singleton with no teardown: a cold field
    /// would plan a `Replace` that `--replace` cannot carry out. Turning the
    /// private-egress guard on in an applied manifest must converge in place.
    #[test]
    fn a_runtime_policy_change_converges_in_place() {
        let map = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        for field in [
            "denyPrivateEgress",
            "allowSourceCheckOptOut",
            "allowedSourcePrefixes",
            "allowedEgressPrefixes",
            "workloadSecurity",
        ] {
            let d = Desired {
                kind: k::RUNTIME_POLICY.into(),
                name: "node".into(),
                fields: map(&[(field, "true")]),
                converges: true,
                ownable: false,
            };
            let a = Actual {
                kind: k::RUNTIME_POLICY.into(),
                name: "node".into(),
                fields: map(&[(field, "false")]),
                owner: None,
                last_applied: None,
            };
            let p = plan(&[d], &[a], "s");
            assert_eq!(
                p[0].action,
                Action::Update,
                "{field}: {:?}",
                p[0].cold_fields
            );
        }
    }

    /// A VM's hot fields are bookkeeping on its record — `labels` and the
    /// declared login reference (ADR-0048 D4) — and nothing that defines the
    /// machine: everything else stays a refused `Replace`.
    #[test]
    fn only_bookkeeping_changes_a_vm_without_recreating_it() {
        assert_eq!(hot_fields(k::VM), ["labels", "credentials"]);
        assert!(hot_fields(k::NETWORK).contains(&"labels"));
        assert!(hot_fields(k::VOLUME).contains(&"labels"));
    }

    /// A document's own labels are one plan field, the same string from the
    /// manifest and from the record; the engine's ownership labels never are.
    #[test]
    fn the_labels_field_is_canonical_and_leaves_the_engines_labels_out() {
        let map = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let declared = map(&[("tier", "front"), ("app", "web")]);
        let on_record = map(&[
            ("app", "web"),
            ("delonix.io/managed-by", "delonix"),
            ("delonix.io/stack", "shop"),
            ("tier", "front"),
        ]);
        assert_eq!(
            user_labels_field(&declared).as_deref(),
            Some(r#"{"app":"web","tier":"front"}"#)
        );
        assert_eq!(user_labels_field(&declared), user_labels_field(&on_record));
        // Nothing declared says nothing: no field, so labels put on by hand
        // are not this document's to revert.
        assert_eq!(user_labels_field(&map(&[])), None);
        assert_eq!(
            user_labels_field(&map(&[("delonix.io/stack", "shop")])),
            None
        );
    }

    /// Moving the field sets what is declared and removes what no longer is —
    /// and never touches a key the engine owns, whatever a manifest says.
    #[test]
    fn a_labels_change_sets_removes_and_never_touches_reserved_keys() {
        let from = r#"{"app":"web","tier":"front"}"#;
        let to = r#"{"app":"api","delonix.io/stack":"mine"}"#;
        let mut delta = labels_delta(Some(from), Some(to));
        delta.sort();
        assert_eq!(
            delta,
            vec![
                ("app".to_string(), Some("api".to_string())),
                ("tier".to_string(), None),
            ]
        );
        assert_eq!(
            labels_delta(Some(from), None),
            vec![("app".to_string(), None), ("tier".to_string(), None)]
        );
        assert!(labels_delta(None, None).is_empty());
    }

    // ---- the reboot class (ADR-0012) and live growth (ADR-0068 D9) ---------

    fn mib(s: &str) -> Option<u64> {
        let t = s.trim();
        if let Some(n) = t.strip_suffix('G') {
            return n.parse::<u64>().ok().map(|v| v * 1024);
        }
        t.strip_suffix('M').unwrap_or(t).parse().ok()
    }

    fn count(s: &str) -> Option<u64> {
        s.parse().ok()
    }

    fn vm_plan(
        desired_fields: &[(&str, &str)],
        actual_fields: &[(&str, &str)],
        facts: Facts,
    ) -> Change {
        let d = Desired {
            kind: k::VM.into(),
            name: "db".into(),
            fields: map(desired_fields),
            converges: true,
            ownable: true,
        };
        let a = Actual {
            kind: k::VM.into(),
            name: "db".into(),
            fields: map(actual_fields),
            owner: Some("s".into()),
            last_applied: None,
        };
        let mut all = BTreeMap::new();
        all.insert((k::VM.to_string(), "db".to_string()), facts);
        plan_with_facts(&[d], &[a], "s", &all).remove(0)
    }

    fn vm_facts(running: Option<bool>) -> Facts {
        let mut f = Facts {
            running,
            ..Facts::default()
        };
        for x in ["vcpus", "memory", "network"] {
            f.reboot.insert(x.into());
        }
        f.numeric.insert("memory".into(), mib);
        f.numeric.insert("vcpus".into(), count);
        f
    }

    /// The point of ADR-0012: a field that is neither hot nor hot-pluggable
    /// plans a REBOOT on a running VM, never a `Replace` that throws the disk
    /// away — and the change names every field that needs it.
    #[test]
    fn a_reboot_field_on_a_running_vm_plans_a_reboot_not_a_replace() {
        let c = vm_plan(
            &[("vcpus", "4"), ("memory", "4G"), ("disk", "/i.qcow2")],
            &[("vcpus", "2"), ("memory", "2G"), ("disk", "/i.qcow2")],
            vm_facts(Some(true)),
        );
        assert_eq!(c.action, Action::Reboot);
        assert!(c.changed);
        assert!(c.cold_fields.is_empty(), "{:?}", c.cold_fields);
        assert_eq!(c.reboot_fields, vec!["memory".to_string(), "vcpus".into()]);
        assert!(c.diffs.iter().all(|d| d.reboot && !d.hot), "{:?}", c.diffs);
        assert!(c.reason.as_deref().unwrap().contains("stop and a start"));
    }

    /// Unknown (or paused) is planned as running: the gate is asked for.
    #[test]
    fn an_unknown_power_state_plans_the_reboot_too() {
        let c = vm_plan(&[("vcpus", "4")], &[("vcpus", "2")], vm_facts(None));
        assert_eq!(c.action, Action::Reboot);
    }

    /// On a STOPPED VM the same change costs nothing: it is an `Update`, and
    /// the diff still says which class it is, for the executor.
    #[test]
    fn a_reboot_field_on_a_stopped_vm_is_a_plain_update() {
        let c = vm_plan(&[("vcpus", "4")], &[("vcpus", "2")], vm_facts(Some(false)));
        assert_eq!(c.action, Action::Update);
        assert!(c.reboot_fields.is_empty());
        assert!(c.diffs[0].reboot);
    }

    /// The disk image is identity: it stays a `Replace`, and only it is named
    /// cold — the reboot field that came with it is not.
    #[test]
    fn a_disk_change_still_recreates_and_names_only_the_cold_field() {
        let c = vm_plan(
            &[("vcpus", "4"), ("disk", "/b.qcow2")],
            &[("vcpus", "2"), ("disk", "/a.qcow2")],
            vm_facts(Some(true)),
        );
        assert_eq!(c.action, Action::Replace);
        assert_eq!(c.cold_fields, vec!["disk".to_string()]);
    }

    /// Growth inside the hotplug ceiling on a running VM is live; beyond the
    /// ceiling, or a decrease, is a reboot.
    #[test]
    fn growth_inside_the_ceiling_is_live_and_everything_else_reboots() {
        let mut f = vm_facts(Some(true));
        f.live_growth.insert(
            "vcpus".into(),
            LiveGrowth {
                ceiling: 4,
                parse: count,
            },
        );
        f.live_growth.insert(
            "memory".into(),
            LiveGrowth {
                ceiling: 4096,
                parse: mib,
            },
        );
        let c = vm_plan(
            &[("vcpus", "4"), ("memory", "4G")],
            &[("vcpus", "2"), ("memory", "2048M")],
            f.clone(),
        );
        assert_eq!(c.action, Action::Update, "{c:?}");
        assert!(c.diffs.iter().all(|d| d.hot && !d.reboot), "{:?}", c.diffs);
        let over = vm_plan(&[("vcpus", "5")], &[("vcpus", "2")], f.clone());
        assert_eq!(over.action, Action::Reboot);
        let down = vm_plan(&[("vcpus", "1")], &[("vcpus", "2")], f.clone());
        assert_eq!(down.action, Action::Reboot);
        // Not running: no live growth, and a reboot of a stopped VM is free.
        let mut stopped = f;
        stopped.running = Some(false);
        let c = vm_plan(&[("vcpus", "3")], &[("vcpus", "2")], stopped);
        assert_eq!(c.action, Action::Update);
        assert!(c.diffs[0].reboot);
    }

    /// `2G` and `2048M` are the same memory: no difference, ever.
    #[test]
    fn a_numeric_field_is_compared_by_value() {
        let c = vm_plan(
            &[("memory", "2G")],
            &[("memory", "2048M")],
            vm_facts(Some(true)),
        );
        assert_eq!(c.action, Action::NoOp, "{c:?}");
        assert!(c.diffs.is_empty());
    }

    /// The new action is a change for `--detailed-exitcode`, has its own
    /// marker and its own summary key, and the reboot flag is absent from the
    /// JSON of a diff that does not reboot (an older reader sees what it saw).
    #[test]
    fn the_reboot_action_is_a_change_with_its_own_marker_and_key() {
        assert!(Action::Reboot.is_change());
        assert_eq!(Action::Reboot.marker(), "~↻");
        let c = vm_plan(&[("vcpus", "4")], &[("vcpus", "2")], vm_facts(Some(true)));
        assert_eq!(summary(std::slice::from_ref(&c)).get("reboot"), Some(&1));
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""action":"reboot""#), "{json}");
        assert!(json.contains(r#""reboot_fields":["vcpus"]"#), "{json}");
        let plain = vm_plan(&[("labels", "x")], &[("labels", "y")], vm_facts(Some(true)));
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("reboot"), "{json}");
    }
}
