//! What each Kind IS — one table, instead of six lists that have to agree.
//!
//! Every column here already existed, each as a separate constant living beside
//! the code that consumed it: `KINDS` (the apply order), `CONVERGING_KINDS`,
//! `TEARDOWN_KINDS`, `kind_honors_namespace`, the `DECLARATIVOS` of the wait
//! test, and the arms of `presence()`. Nothing tied them together, and they
//! drifted — with measured symptoms, all of the same shape: a Kind answered one
//! list and not the others, and the gap was invisible because the wrong answer
//! still looked like a working command.
//!
//! - `Vm`/`FirewallPolicy`/`ShareVolume` got a reconciler adapter and stayed OUT
//!   of `CONVERGING_KINDS`, so `converge_and_stamp` skipped them — hidden
//!   because the old per-Kind apply is idempotent and converged by the wrong path.
//! - `NetworkRoute` entered `KINDS` and was applied for versions with no arm in
//!   `presence()`, so `ls`/`describe` called a resource the apply creates an
//!   «unsupported kind».
//! - The declarative Kinds never say `"yes"`, and `stack wait` read that as
//!   «absent», burning the whole `--timeout` on a stack that was entirely up.
//!
//! So the lists are now DERIVED from this table and the table is the only place
//! a Kind is described. Adding one without answering every column does not
//! compile; answering a column wrong is what the tests at the bottom are for.
//!
//! What is deliberately NOT here: `Desired.ownable`. It is decided inside each
//! Kind's `desired()`, which needs a document to run, so a copy in this table
//! would be a seventh list with nothing forcing it to agree with the real
//! value — the exact arrangement this module exists to remove.

/// The area a Kind acts on. Printed as a column, so the names are short and the
/// three network ones are split: they answer different questions and a single
/// `network` label would hide that `NetworkRoute` opens a PATH while
/// `FirewallPolicy` decides whether traffic is ALLOWED along it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Domain {
    /// Runs code: containers, pods, VMs.
    Compute,
    /// Holds bytes: volumes and shares.
    Storage,
    /// Whether a path exists between two addresses.
    NetConnectivity,
    /// Whether traffic along an existing path is permitted.
    NetPolicy,
    /// How something outside the node gets in.
    NetExposure,
    /// Content consumed by a workload: images, secrets.
    Artifact,
    /// Composes or drives other Kinds; not a resource of its own.
    Composition,
}

impl Domain {
    /// Deliberately NOT translated: these are identifiers a script can grep for,
    /// like the Kind names beside them in the same table. The words that ARE
    /// prose (`primary`/`declarative`/…) go through `po::t` at the point of
    /// printing.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Domain::Compute => "compute",
            Domain::Storage => "storage",
            Domain::NetConnectivity => "net-conn",
            Domain::NetPolicy => "net-policy",
            Domain::NetExposure => "net-exposure",
            Domain::Artifact => "artifact",
            Domain::Composition => "composition",
        }
    }
}

/// What a document of this Kind becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Form {
    /// Has its own apply and survives the `load`.
    Primary,
    /// Rewritten into another Kind at load time, and the old spelling still
    /// works. Names the replacement, because a deprecation without one is just
    /// a complaint.
    Deprecated(&'static str),
    /// Rewritten into another Kind at load time as a convenience, not because it
    /// is going away.
    Sugar(&'static str),
    /// Expands into the documents it contains.
    Aggregate,
    /// A foreign schema this engine accepts verbatim, compiled onto another
    /// Kind's mechanism. It DOES survive the load — that is what separates it
    /// from [`Form::Deprecated`].
    Compat(&'static str),
}

impl Form {
    /// The Kind a document of this one ends up as, if it is not itself.
    pub(crate) fn lowers_to(self) -> Option<&'static str> {
        match self {
            Form::Deprecated(k) | Form::Sugar(k) | Form::Compat(k) => Some(k),
            Form::Primary | Form::Aggregate => None,
        }
    }
}

/// How `stack ls`/`wait` can tell whether the resource is there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Presence {
    /// A store of its own answers yes/no.
    Registry,
    /// Derived from something else — a Pod is its labelled members.
    Derived,
    /// Nothing to read back: the resource is a directive applied to a target,
    /// not state. `presence()` answers `-`, and **that is not «absent»** — the
    /// distinction `is_pending` exists to make.
    Declarative,
    /// Never reaches `presence()`: it does not survive the load, or is not a
    /// local resource at all.
    NotObservable,
}

/// Whether `metadata.namespace` means anything on a Kind.
///
/// Three states and not a bool, because `Volume` genuinely has three answers:
/// none for a plain volume, real for one with a `share:` block. Modelling that
/// as `true` would make the load stop warning about a namespace that does
/// nothing on every ordinary volume; as `false`, it would warn «namespace has no
/// effect» on a share, whose namespace decides which directory its data lives
/// in — a warning that is not merely useless but wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Namespaced {
    /// Never — the resource is global to the node.
    Never,
    /// Always.
    Always,
    /// Depends on the document. `honors_namespace` answers `true` so nothing is
    /// warned away wholesale, and the Kind's own apply decides what to do with it.
    PerDocument,
}

/// Everything the rest of the code asks about a Kind.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KindFacts {
    pub kind: &'static str,
    /// The lowercase plural a caller types (`delonix get pods`). Its own field
    /// and not derived by appending `s`: `Dependency`→`dependencies` and
    /// `Ingress`→`ingresses` do not follow that rule, and a resolver that
    /// guessed would answer «no such resource» for a Kind that is right there.
    pub plural: &'static str,
    /// Accepted abbreviations. Deliberately sparse — a shortname exists when it
    /// is unambiguous AND worth typing; inventing one per Kind only multiplies
    /// the ways two Kinds can collide. Uniqueness across the whole registry is
    /// a TEST, not a convention: a duplicate would silently shadow, which is
    /// the defect class this module exists to have removed.
    pub short: &'static [&'static str],
    /// The `apiVersion` a manifest writes for this Kind.
    ///
    /// A column and not a constant, even though every row says the same thing
    /// today. The CLI restructuring splits it per domain
    /// (`compute.delonix.io/…`, `networking.delonix.io/…`), and a Kind whose
    /// version lives in a shared `const` cannot be moved one at a time — which
    /// is the only way that migration can be reviewed. `delonix.io/v1` also has
    /// to keep LOADING afterwards (ADR-0020, and the promise in
    /// `docs/cli-stability.md`), so the old spelling stays reachable rather
    /// than being renamed away.
    pub api_version: &'static str,
    pub domain: Domain,
    pub form: Form,
    /// Applied by `stack apply`. **The order of the rows below is the order of
    /// the apply**, and `destroy` derives its own by reversing it.
    pub in_stack: bool,
    /// A changed field is really applied, rather than «ensure present».
    pub converges: bool,
    /// `destroy_one` removes it, so `--prune` and `destroy` can promise it.
    pub teardown: bool,
    /// Whether `metadata.namespace` does something on this Kind.
    pub namespaced: Namespaced,
    pub presence: Presence,
}

/// The table. Rows are in APPLY ORDER for everything with `in_stack: true` —
/// see [`KindFacts::in_stack`] — and the rest follow.
const FACTS: &[KindFacts] = &[
    KindFacts {
        kind: "Secret",
        plural: "secrets",
        short: &["sec"],
        api_version: "delonix.io/v1",
        domain: Domain::Artifact,
        form: Form::Primary,
        in_stack: true,
        // The state is the encrypted values, and a plan will not decrypt them to
        // compare. The only ensure-present Kind left, and `not_converged_reason`
        // says so in those words.
        converges: false,
        teardown: false,
        namespaced: Namespaced::Never,
        presence: Presence::Registry,
    },
    KindFacts {
        kind: "Network",
        plural: "networks",
        short: &["net"],
        api_version: "delonix.io/v1",
        domain: Domain::NetConnectivity,
        form: Form::Primary,
        in_stack: true,
        converges: true,
        teardown: true,
        namespaced: Namespaced::Never,
        presence: Presence::Registry,
    },
    KindFacts {
        kind: "NetworkRoute",
        plural: "networkroutes",
        short: &["nr"],
        api_version: "delonix.io/v1",
        domain: Domain::NetConnectivity,
        form: Form::Primary,
        in_stack: true,
        converges: true,
        teardown: true,
        namespaced: Namespaced::Never,
        presence: Presence::Registry,
    },
    KindFacts {
        kind: "Volume",
        plural: "volumes",
        short: &["vol"],
        api_version: "delonix.io/v1",
        domain: Domain::Storage,
        form: Form::Primary,
        in_stack: true,
        converges: true,
        teardown: true,
        // A plain volume is global and a `share:` one is scoped by namespace,
        // so this is the one Kind whose answer comes from the DOCUMENT.
        namespaced: Namespaced::PerDocument,
        presence: Presence::Registry,
    },
    KindFacts {
        kind: "Image",
        plural: "images",
        short: &["img"],
        api_version: "delonix.io/v1",
        domain: Domain::Artifact,
        form: Form::Primary,
        in_stack: true,
        converges: true,
        // Shared content-addressed cache: removing it because one stack stopped
        // declaring it would pull it from under the others.
        teardown: false,
        namespaced: Namespaced::Never,
        presence: Presence::Registry,
    },
    KindFacts {
        kind: "Vm",
        plural: "vms",
        short: &[],
        api_version: "delonix.io/v1",
        domain: Domain::Compute,
        form: Form::Primary,
        in_stack: true,
        converges: true,
        teardown: true,
        namespaced: Namespaced::Always,
        presence: Presence::Registry,
    },
    KindFacts {
        kind: "Container",
        plural: "containers",
        short: &[],
        api_version: "delonix.io/v1",
        domain: Domain::Compute,
        form: Form::Primary,
        in_stack: true,
        converges: true,
        teardown: true,
        namespaced: Namespaced::Always,
        presence: Presence::Registry,
    },
    KindFacts {
        kind: "Pod",
        plural: "pods",
        short: &["po"],
        api_version: "delonix.io/v1",
        domain: Domain::Compute,
        form: Form::Primary,
        in_stack: true,
        converges: true,
        teardown: true,
        namespaced: Namespaced::Always,
        presence: Presence::Derived,
    },
    KindFacts {
        kind: "Ingress",
        plural: "ingresses",
        short: &["ing"],
        api_version: "delonix.io/v1",
        domain: Domain::NetExposure,
        form: Form::Compat("HTTPRoute"),
        in_stack: true,
        converges: true,
        teardown: false,
        namespaced: Namespaced::Never,
        presence: Presence::Declarative,
    },
    KindFacts {
        kind: "FirewallPolicy",
        plural: "firewallpolicies",
        short: &["fwp"],
        api_version: "delonix.io/v1",
        domain: Domain::NetPolicy,
        form: Form::Primary,
        in_stack: true,
        converges: true,
        teardown: false,
        namespaced: Namespaced::Never,
        presence: Presence::Declarative,
    },
    KindFacts {
        kind: "HTTPRoute",
        plural: "httproutes",
        short: &["hr"],
        api_version: "delonix.io/v1",
        domain: Domain::NetExposure,
        form: Form::Primary,
        in_stack: true,
        converges: true,
        teardown: false,
        namespaced: Namespaced::Never,
        presence: Presence::Declarative,
    },
    KindFacts {
        kind: "Tunnel",
        plural: "tunnels",
        short: &["tun"],
        api_version: "delonix.io/v1",
        domain: Domain::NetExposure,
        form: Form::Primary,
        in_stack: true,
        converges: true,
        teardown: false,
        namespaced: Namespaced::Never,
        presence: Presence::Registry,
    },
    // --- Not resources of the stack: they become one of the above, or are not
    // local resources at all. ---
    KindFacts {
        kind: "Workload",
        plural: "workloads",
        short: &["wl"],
        api_version: "delonix.io/v1",
        domain: Domain::Compute,
        form: Form::Sugar("Container/Pod/Vm"),
        in_stack: false,
        converges: false,
        teardown: false,
        // Carried onto the child it lowers to.
        namespaced: Namespaced::Always,
        presence: Presence::NotObservable,
    },
    KindFacts {
        kind: "Dependency",
        plural: "dependencies",
        short: &["dep"],
        api_version: "delonix.io/v1",
        domain: Domain::NetPolicy,
        form: Form::Sugar("FirewallPolicy"),
        in_stack: false,
        converges: false,
        teardown: false,
        namespaced: Namespaced::Never,
        // It has an arm in `presence()` (answering `-`) even though the load
        // lowers it away, so a document that somehow reaches `ls` is described
        // rather than called unsupported.
        presence: Presence::Declarative,
    },
    KindFacts {
        kind: "ShareVolume",
        plural: "sharevolumes",
        short: &["sv"],
        api_version: "delonix.io/v1",
        domain: Domain::Storage,
        form: Form::Deprecated("Volume"),
        in_stack: false,
        converges: false,
        teardown: false,
        // It carried a namespace and still does — as a `kind: Volume` with a
        // `share:` block, which is what it lowers to.
        namespaced: Namespaced::Always,
        presence: Presence::NotObservable,
    },
    KindFacts {
        kind: "Storage",
        plural: "storages",
        short: &[],
        api_version: "delonix.io/v1",
        domain: Domain::Storage,
        form: Form::Deprecated("Volume"),
        in_stack: false,
        converges: false,
        teardown: false,
        namespaced: Namespaced::Never,
        presence: Presence::NotObservable,
    },
    KindFacts {
        kind: "Egress",
        plural: "egresses",
        short: &[],
        api_version: "delonix.io/v1",
        domain: Domain::NetPolicy,
        form: Form::Deprecated("FirewallPolicy"),
        in_stack: false,
        converges: false,
        teardown: false,
        namespaced: Namespaced::Never,
        presence: Presence::NotObservable,
    },
    KindFacts {
        kind: "Stack",
        plural: "stacks",
        short: &[],
        api_version: "delonix.io/v1",
        domain: Domain::Composition,
        form: Form::Aggregate,
        in_stack: false,
        converges: false,
        teardown: false,
        // Propagated to every child it expands into.
        namespaced: Namespaced::Always,
        presence: Presence::NotObservable,
    },
    KindFacts {
        kind: "Cluster",
        plural: "clusters",
        short: &[],
        api_version: "delonix.io/v1",
        domain: Domain::Composition,
        form: Form::Primary,
        // Deliberately outside the stack cycle: it is a remote procedure over
        // SSH against hosts that already exist, not a resource of this node.
        in_stack: false,
        converges: false,
        teardown: false,
        namespaced: Namespaced::Never,
        presence: Presence::NotObservable,
    },
];

/// Facts for a CANONICAL kind name (`canonical_kind` has already run at the call
/// site), or `None` for a name this engine does not know.
pub(crate) fn facts(kind: &str) -> Option<&'static KindFacts> {
    FACTS.iter().find(|f| f.kind == kind)
}

/// Every Kind in the table, in the order it is written.
pub(crate) fn all() -> impl Iterator<Item = &'static KindFacts> {
    FACTS.iter()
}

/// The Kinds `stack apply` handles, **in apply order**. Was the `KINDS`
/// constant.
pub(crate) fn stack_kinds() -> impl DoubleEndedIterator<Item = &'static str> {
    FACTS.iter().filter(|f| f.in_stack).map(|f| f.kind)
}

/// Whether the Kind belongs to the stack cycle at all.
pub(crate) fn in_stack(kind: &str) -> bool {
    facts(kind).is_some_and(|f| f.in_stack)
}

/// Whether a changed field is really applied. Was `CONVERGING_KINDS.contains`.
pub(crate) fn converges(kind: &str) -> bool {
    facts(kind).is_some_and(|f| f.converges)
}

/// Whether `destroy_one` removes it. Was `TEARDOWN_KINDS.contains`.
pub(crate) fn has_teardown(kind: &str) -> bool {
    facts(kind).is_some_and(|f| f.teardown)
}

/// Whether `metadata.namespace` does anything here.
pub(crate) fn honors_namespace(kind: &str) -> bool {
    facts(kind).is_some_and(|f| f.namespaced != Namespaced::Never)
}

// There is deliberately no `is_declarative(kind)` helper. `stack wait` decides
// on the presence MARKER the store actually returned (`-`), not on what the
// table says the Kind should return: a second path to the same answer would let
// the two disagree exactly when a Kind's presence changes — which is what
// happened to `NetworkRoute` when it gained a record of its own. The column is
// here to be DISPLAYED and to be asserted against `presence()` in the tests.

/// The domain label for display, or `-` for a Kind this engine does not know
/// (`stack ls` prints what the manifest says, including a typo).
pub(crate) fn domain_label(kind: &str) -> &'static str {
    facts(kind).map(|f| f.domain.label()).unwrap_or("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two rows for one Kind means half the code reads one and half the other.
    #[test]
    fn nenhum_kind_aparece_duas_vezes() {
        let mut seen = std::collections::BTreeSet::new();
        for f in all() {
            assert!(seen.insert(f.kind), "{} está duplicado na tabela", f.kind);
        }
    }

    /// The reversal `destroy` relies on is only meaningful if the stack Kinds
    /// are contiguous at the head of the table: a row inserted between them
    /// changes the apply order without anyone editing an «order» anywhere.
    #[test]
    fn os_kinds_do_stack_sao_um_prefixo_contiguo() {
        let first_out = FACTS.iter().position(|f| !f.in_stack).unwrap();
        assert!(
            FACTS[first_out..].iter().all(|f| !f.in_stack),
            "um Kind do stack está DEPOIS de um que não é — a ordem de apply mudou em silêncio"
        );
        assert_eq!(stack_kinds().count(), first_out);
    }

    /// A Kind that does not survive the load has no apply of its own, so it
    /// cannot be part of the stack cycle — and if it were, `apply` would look
    /// for documents that no longer exist.
    #[test]
    fn um_kind_que_baixa_para_outro_nao_pertence_ao_ciclo_do_stack() {
        for f in all() {
            if matches!(
                f.form,
                Form::Deprecated(_) | Form::Sugar(_) | Form::Aggregate
            ) {
                assert!(
                    !f.in_stack,
                    "{} é reescrito no load e está no ciclo do stack",
                    f.kind
                );
                assert!(
                    !f.converges,
                    "{} é reescrito no load e diz convergir",
                    f.kind
                );
            }
        }
        // A `Compat` DOES survive — that is the whole difference, and the
        // `Ingress` proves it by being applied like any other.
        assert!(in_stack("Ingress"));
    }

    /// The pairing that `TEARDOWN_KINDS` and `no_teardown_reason` used to keep
    /// by hand: promising to remove something the destroy refuses would fail
    /// HALFWAY, after the rest of the stack is already gone.
    #[test]
    fn so_um_kind_convergente_tem_teardown() {
        for f in all() {
            if f.teardown {
                assert!(
                    f.converges,
                    "{} tem teardown e não converge — o `--prune` promete o que o destroy recusa",
                    f.kind
                );
            }
        }
    }

    /// A Kind outside the stack cycle is never applied, so it can neither
    /// converge nor be torn down. Catches the copy-paste of a row.
    #[test]
    fn fora_do_ciclo_nao_converge_nem_e_removido() {
        for f in all().filter(|f| !f.in_stack) {
            assert!(!f.converges, "{} converge fora do ciclo", f.kind);
            assert!(!f.teardown, "{} tem teardown fora do ciclo", f.kind);
        }
    }

    /// A Kind the `apply` applies has to be observable SOMEHOW — `NotObservable`
    /// is the mark of a document that never reaches `presence()`, and a Kind in
    /// the cycle that carried it would fall through to `_ => ("?", "unsupported
    /// kind")`: printed by `ls`/`describe`, and counted as pending forever by
    /// `wait`. That is exactly what `NetworkRoute` did for several versions.
    ///
    /// The paired check lives in `stack.rs`, where `presence()` can be called:
    /// this one says the column is filled in, that one says it is TRUE.
    #[test]
    fn um_kind_do_ciclo_do_stack_tem_presenca_observavel() {
        for f in all().filter(|f| f.in_stack) {
            assert_ne!(
                f.presence,
                Presence::NotObservable,
                "{} é aplicado pelo stack e não tem presença — cairia no «unsupported kind»",
                f.kind
            );
        }
    }

    /// The lowering target has to be a Kind that exists — a typo here sends the
    /// reader looking for a Kind the engine never had.
    #[test]
    fn o_destino_de_uma_reducao_existe() {
        for f in all() {
            let Some(to) = f.form.lowers_to() else {
                continue;
            };
            // `Workload` names three, and the slash is the honest spelling.
            for k in to.split('/') {
                assert!(
                    facts(k).is_some(),
                    "{} baixa para {k}, que não existe",
                    f.kind
                );
            }
        }
    }
}
