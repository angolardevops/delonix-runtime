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
            Action::Create | Action::Adopt | Action::Update | Action::Replace | Action::Delete
        )
    }

    /// The one-character marker in the rendered plan.
    pub fn marker(self) -> &'static str {
        match self {
            Action::Create => "+",
            Action::Adopt => "+~",
            Action::Update => "~",
            Action::Replace => "-/+",
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
    pub conditions: Vec<super::conditions::Condition>,
    pub diffs: Vec<FieldDiff>,
    /// Convenience for consumers: `action.is_change()`.
    pub changed: bool,
}

impl Change {
    fn new(kind: &str, name: &str, action: Action) -> Self {
        Change {
            kind: kind.to_string(),
            name: name.to_string(),
            action,
            reason: None,
            cold_fields: Vec::new(),
            owner: None,
            conditions: Vec::new(),
            diffs: Vec::new(),
            changed: action.is_change(),
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
        "Container" => &["ports", "volumes", "memory", "cpus", "netBps", "netBurst"],
        "Volume" => &["quota"],
        "Network" => &["peers"],
        // Fetching a ref destroys nothing — an image is shared cache, so its
        // whole comparable surface converges without recreating anything.
        "Image" => &["ref", "digest"],
        _ => &[],
    }
}

fn is_hot(kind: &str, field: &str) -> bool {
    hot_fields(kind).contains(&field)
}

/// Computes the three-way diff of one resource.
fn diff_fields(kind: &str, desired: &Desired, actual: &Actual) -> Vec<FieldDiff> {
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
            (Some(d), Some(a)) if d == a => {}
            // Present in the manifest — converge to it (whether or not the
            // machine has it today).
            (Some(d), a) => out.push(FieldDiff {
                field: key.clone(),
                from: a.cloned(),
                to: Some(d.clone()),
                hot: is_hot(kind, key),
            }),
            // Gone from the manifest. Revert it ONLY if we were the ones who
            // set it — otherwise it belongs to a human's `container update` or
            // to a default, and reverting it would be this tool fighting its
            // own user.
            (None, Some(a)) => {
                if last.contains_key(key) {
                    out.push(FieldDiff {
                        field: key.clone(),
                        from: Some(a.clone()),
                        to: None,
                        hot: is_hot(kind, key),
                    });
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
        let diffs = diff_fields(&d.kind, d, a);
        // A non-ownable resource is never «unowned and therefore to adopt»:
        // having no owner is its normal state, not something to fix.
        let unmanaged = d.ownable && a.owner.is_none();
        let action = if let Some(cold) = diffs.iter().find(|x| !x.hot) {
            let _ = cold;
            Action::Replace
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
            let cold: Vec<String> = diffs
                .iter()
                .filter(|x| !x.hot)
                .map(|x| x.field.clone())
                .collect();
            change = change.with_reason(format!("does not converge hot: {}", cold.join(", ")));
            change.cold_fields = cold;
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

    /// **Numa VM nada converge a quente, e isso não é uma lacuna.** Este motor
    /// não faz hotplug: mudar vCPUs, memória ou disco é arrancar outra máquina.
    /// Por isso qualquer alteração é `Replace` — e recriar uma VM deita fora o
    /// overlay, ou seja, tudo o que o convidado escreveu desde que existe.
    /// Recusar sem `--replace` é o comportamento certo, não um obstáculo.
    #[test]
    fn qualquer_alteracao_numa_vm_e_uma_recriacao() {
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
}
