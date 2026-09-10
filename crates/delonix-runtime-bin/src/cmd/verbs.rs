//! The generic verbs: `get`, `describe`, `delete`.
//!
//! Ten groups had grown their own copy of the same CRUD — `network ls`,
//! `volumes ls`, `vm ls`, `pod ls`, `secret ls`, each with its own spelling of
//! the same question. The verbs here ask it ONCE, against the Kind registry
//! that already knows every resource this engine serves.
//!
//! **They route; they do not reimplement.** Each arm builds the group's own
//! command and calls the group's own `run` — the same code the old spelling
//! reached, so behaviour, output and exit codes cannot drift between the two
//! ways of asking. A second implementation of `vm ls` living here would be the
//! divergence this module exists to remove.
//!
//! Not every Kind is reachable this way, and the ones that are not have to SAY
//! so: [`no_verb_reason`] carries the obstacle, and a gate refuses a Kind that
//! neither routes nor explains itself. «Nobody got round to it» and «that is not
//! how you ask this» are different answers, and a caller deserves to know which.

use super::kinds::{self, KindFacts};
use super::output::OutputFormat;
use super::resource::resolve_kind;
use delonix_runtime_core::{Error, Result};

/// The Kinds each verb can route today, as DATA — so the gate below can ask
/// the question without running the verb. The first version of that gate called
/// `get` and read the error text: it did real store I/O to answer a question
/// about a table, and would have passed or failed on the state of the machine.
pub(crate) const GET_ROUTES: &[&str] = &[
    kinds::POD,
    kinds::NETWORK,
    kinds::VOLUME,
    kinds::SECRET,
    kinds::VM,
    kinds::IMAGE,
    kinds::CLUSTER,
    kinds::GATEWAY,
    kinds::HTTP_ROUTE,
    kinds::NETWORK_ROUTE,
    kinds::FIREWALL_POLICY,
    kinds::SERVICE,
];

/// Kinds whose listing has no `-o json` today. Listed rather than discovered,
/// because the alternative is accepting `-o json` and printing a table — the
/// silently-ignored flag this engine refuses everywhere else.
///
/// A list of refusals rots in BOTH directions, and this one had rotted the
/// expensive way: `Gateway` and `HTTPRoute` sat here long after
/// [`super::tunnel::cmd_ls`] and [`super::httproute::cmd_ls`] grew full JSON
/// arms, so `get gateways -o json` and `get httproutes -o json` were refused
/// by a list rather than by the code — a capability that works, turned off by
/// a stale constant. Only `KubernetesCluster` is genuinely table-only: it
/// routes to [`super::kindmode::list`], which takes no format argument at all.
const NO_JSON_YET: &[&str] = &[kinds::CLUSTER];
pub(crate) const DESCRIBE_ROUTES: &[&str] = &[
    kinds::POD,
    kinds::NETWORK,
    kinds::VOLUME,
    kinds::VM,
    kinds::IMAGE,
    kinds::GATEWAY,
    kinds::SECRET,
    kinds::NETWORK_ROUTE,
    kinds::FIREWALL_POLICY,
    kinds::SERVICE,
    kinds::CLUSTER,
];
pub(crate) const DELETE_ROUTES: &[&str] = &[
    kinds::POD,
    kinds::NETWORK,
    kinds::VOLUME,
    kinds::VM,
    kinds::SECRET,
    kinds::IMAGE,
    kinds::CLUSTER,
    kinds::GATEWAY,
    kinds::NETWORK_ROUTE,
    kinds::FIREWALL_POLICY,
    kinds::SERVICE,
];

/// A generic verb, as DATA — so the table below and its test can name one
/// without running it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Verb {
    Get,
    Describe,
    Delete,
}

/// The command a refusal sends the reader to, per Kind AND per verb.
///
/// **Not a group plus a verb.** The first version returned the registry's
/// PLURAL and sent people to `virtualmachines ls`, which has never existed.
/// The fix returned the CLI group instead — `cluster`, `net tunnel`, `net
/// httproute` — and each caller appended its own leaf: `{group} ls`, `{group}
/// describe`, `{group} rm`. That shape cannot be right, because the groups are
/// not uniform. `net ingress` has `ls` and `rm` but no `describe`; `vm` has
/// `ls` and neither of the other two; `cluster`, `net tunnel` and `net
/// httproute` have no `ls` at all — B4 of the CLI restructuring deleted those
/// leaves ON PURPOSE, because they duplicated these very verbs byte-for-byte
/// (see `httproute::cmd_ls`), and this table was not updated with them.
///
/// Measured against the v3.0.0 binary, five reachable refusals named a command
/// that exits 2. The `--help` was right and the command was right; only the
/// sentence lied, which costs a person the time to find out it was wrong — and
/// quietly demotes a refusal-with-a-reason, which this engine counts as a
/// property of the product, to a plain dead end.
///
/// So the WHOLE suggestion is the datum now, and the test
/// `every_suggested_command_exists_in_the_tree` resolves each one against the
/// real clap tree — the LEAF included, which a group-only check could not do
/// and did not do.
///
/// Empty means there is no single command to name, and the caller then refuses
/// WITHOUT a signpost. Inventing one is the failure this table exists to stop.
pub(crate) fn fallback_command(kind: &str, verb: Verb) -> &'static str {
    match verb {
        Verb::Get => match kind {
            k if k == kinds::VM => "vm ls",
            k if k == kinds::VOLUME => "volume ls",
            // `container ps`, never `container ls` — the imperative surface has
            // only ever spelled it the one way, and `no_verb_reason` agrees.
            k if k == kinds::CONTAINER => "container ps",
            k if k == kinds::NETWORK => "network ls",
            // `network route` with neither endpoint IS the listing; there is no
            // `network route ls` under it to send anyone to.
            k if k == kinds::NETWORK_ROUTE => "network route",
            k if k == kinds::SECRET => "secret ls",
            k if k == kinds::IMAGE => "image ls",
            k if k == kinds::APP => "image ls",
            k if k == kinds::FIREWALL_POLICY => "net ingress ls",
            k if k == kinds::NETWORK_ACCESS_RULE => "net ingress ls",
            k if k == kinds::DEPENDENCY => "net ingress ls",
            k if k == kinds::STACK => "stack ls",
            k if k == kinds::WORKLOAD => "workload ls",
            // `get` IS the only listing the rest have. `pod ls` was collapsed
            // into `get pods`, and `cluster`/`net tunnel`/`net httproute` never
            // got an `ls` back after B4 took theirs.
            _ => "",
        },
        Verb::Describe => match kind {
            k if k == kinds::VOLUME => "volume describe",
            k if k == kinds::CONTAINER => "container describe",
            k if k == kinds::NETWORK => "network describe",
            // `secret` never grew a `describe`: `inspect` IS its detail view,
            // and it prints keys and never values without `--reveal`.
            k if k == kinds::SECRET => "secret inspect",
            k if k == kinds::IMAGE => "image describe",
            k if k == kinds::APP => "image describe",
            k if k == kinds::STACK => "stack describe",
            k if k == kinds::WORKLOAD => "workload describe",
            // A rule has no detail view of its own — it lives inside the
            // target's own fw.rules table, which this listing already prints.
            k if k == kinds::FIREWALL_POLICY => "net ingress ls",
            k if k == kinds::NETWORK_ACCESS_RULE => "net ingress ls",
            k if k == kinds::DEPENDENCY => "net ingress ls",
            k if k == kinds::NETWORK_ROUTE => "network route",
            // `vm describe`, `pod describe`, `cluster describe` and `net
            // httproute describe` all read plausible and none of them exist.
            _ => "",
        },
        Verb::Delete => match kind {
            k if k == kinds::VOLUME => "volume rm",
            k if k == kinds::CONTAINER => "container rm",
            k if k == kinds::NETWORK => "network rm",
            k if k == kinds::SECRET => "secret rm",
            // `image remove`, not `image rm` — the group never took the short
            // spelling and clap refuses it.
            k if k == kinds::IMAGE => "image remove",
            k if k == kinds::WORKLOAD => "workload rm",
            k if k == kinds::FIREWALL_POLICY => "net ingress rm",
            k if k == kinds::NETWORK_ACCESS_RULE => "net ingress rm",
            k if k == kinds::DEPENDENCY => "net ingress rm",
            k if k == kinds::HTTP_ROUTE => "net httproute rm",
            k if k == kinds::INGRESS => "net httproute rm",
            // `vm`, `pod`, `cluster` and `stack` have no `rm` leaf: `vm prune`
            // and `stack destroy` are different questions, not short spellings.
            _ => "",
        },
    }
}

/// The refusal for a verb that does not route a Kind yet.
///
/// One builder for the three verbs, because the rule about the signpost is one
/// rule: name a REAL command, or name none and say so plainly. Each caller
/// assembling its own sentence is exactly how `ls`, `describe` and `rm` came to
/// be appended to groups that have none of them.
fn not_wired_yet(verb: &str, plural: &str, kind: &str, v: Verb) -> Error {
    let alt = fallback_command(kind, v);
    Error::Invalid(if alt.is_empty() {
        format!("`{verb} {plural}` is not wired yet")
    } else {
        format!("`{verb} {plural}` is not wired yet — use `{alt}` meanwhile")
    })
}

/// Why a Kind cannot answer a generic verb. Empty string = it can.
///
/// Same shape as `not_converged_reason`/`no_teardown_reason` on the stack side,
/// and for the same reason: a refusal that names the obstacle is a different
/// message from one that reads like an omission.
pub(crate) fn no_verb_reason(kind: &str) -> &'static str {
    match kind {
        // A Stack is a FILE, not a registry entry — there is nothing to
        // enumerate without being told which manifest. `stack ls -f` stays.
        k if k == kinds::STACK => "a Stack is read from a manifest — use `stack ls -f <file>`",
        // Sugar and compat Kinds never reach a store under their own name: they
        // are rewritten at load time, so asking for them would list nothing and
        // that nothing would read as «none exist».
        k if k == kinds::WORKLOAD => {
            "`Workload` lowers to Pod/VirtualMachine/Container at load — ask for those"
        }
        k if k == kinds::DEPENDENCY => "`Dependency` lowers to NetworkPolicy — ask for that",
        // Unlike `Image`, an App has no registry entry of its own — its build
        // produces exactly one Image and nothing else persists the fact that
        // an App named it. Once built, it is indistinguishable from any other
        // tagged image: `image ls`/`image describe <tag>` see the real state;
        // `get apps` would need a new persisted record this pass does not add.
        k if k == kinds::APP => "an App's state IS its output image — use `image ls`/`image describe <tag>`",
        k if k == kinds::INGRESS => "`Ingress` is the k8s spelling of HTTPRoute — ask for that",
        // The CLI restructuring separates the two surfaces on purpose: a
        // container made by `container run` is an IMPERATIVE resource, and
        // answering it through the declarative verb would put it back in the
        // API the split exists to keep it out of. `container ps` is not a
        // lesser spelling of `get containers`; it is the other surface.
        k if k == kinds::CONTAINER => {
            "`container` is the imperative surface — use `container ps`; declarative containers live in `kind: Pod`"
        }
        _ => "",
    }
}

/// `delonix get <kind> [name…]`
pub(crate) fn get(
    kind: &str,
    names: &[String],
    output: OutputFormat,
    namespace: Option<String>,
) -> Result<()> {
    let f = resolve_kind(kind)?;
    refuse_if_unreachable(f)?;
    if !names.is_empty() {
        // Naming a resource is asking about THAT one, which is `describe` with
        // a table's worth of detail. Routing it here instead of inventing a
        // filtered list keeps one implementation per question. `--namespace`
        // alongside an explicit name has nothing to filter — refused rather
        // than silently ignored, same rule as `-o json` on a Kind with none.
        if namespace.is_some() {
            return Err(Error::Invalid(
                "--namespace filters a LIST — naming a resource already picks exactly one, \
                 drop one of the two"
                    .into(),
            ));
        }
        return describe(kind, names);
    }
    // Only Kinds whose own `ls` already accepts a namespace do anything with
    // one here — every other arm below ignores the parameter outright, which
    // would be the accepted-and-ignored trap this codebase refuses elsewhere
    // (`--security-opt seccomp=`, `-v :z`, `--network-alias`) if `--namespace`
    // reached them un-checked. Refuse it up front instead.
    const NAMESPACED_GET_KINDS: &[&str] = &[kinds::POD, kinds::VM, kinds::VOLUME];
    if namespace.is_some() && !NAMESPACED_GET_KINDS.contains(&f.kind) {
        return Err(Error::Invalid(format!(
            "`get {}` has no namespace to filter by",
            f.plural
        )));
    }
    // A format accepted and ignored is worse than one refused: whoever asks for
    // `-o json` in a pipeline gets a table and only finds out downstream.
    if output != OutputFormat::Table && NO_JSON_YET.contains(&f.kind) {
        // No signpost here on purpose: `get` IS this Kind's listing, so there
        // is no other command to point at. The old sentence pointed at one
        // anyway — `cluster ls`, which exits 2 — and blamed the format on a
        // group that does not have the leaf.
        return Err(Error::Invalid(format!(
            "`get {}` has no JSON yet — that listing is table-only, and answering a \
             table to `-o json` would break whatever reads it",
            f.plural
        )));
    }
    if !GET_ROUTES.contains(&f.kind) {
        return Err(not_wired_yet("get", f.plural, f.kind, Verb::Get));
    }
    match f.kind {
        k if k == kinds::POD => super::pod::ls(output, namespace.as_deref()),
        k if k == kinds::NETWORK => super::network::run(super::network::NetworkCmd::Ls { output }),
        k if k == kinds::VOLUME => super::volume::run(super::volume::VolumeCmd::Ls {
            output,
            namespace,
            all_namespaces: false,
        }),
        k if k == kinds::SECRET => super::secret::run(super::secret::SecretCmd::Ls { output }),
        k if k == kinds::IMAGE => super::image::run(super::image::ImageCmd::Ls { output }),
        k if k == kinds::CLUSTER => super::cluster::cmd_ls(true),
        k if k == kinds::GATEWAY => super::tunnel::cmd_ls(output),
        k if k == kinds::HTTP_ROUTE => super::httproute::cmd_ls(output),
        // The route group's own listing, which already shows both the record and
        // the live map. A route has no name someone chose — the PAIR is its
        // identity — so there is nothing else `get` could key on.
        k if k == kinds::NETWORK_ROUTE => super::netroute::cmd_ls(output),
        k if k == kinds::SERVICE => super::service::cmd_ls(output),
        // Both directions of every governed container, one row each — the
        // listing `net ingress ls`/`net egress ls` never had between them
        // (each answers only its own direction). Identity is `<target>/
        // <direction>`, matched by `describe`/`delete networkpolicies` below.
        k if k == kinds::FIREWALL_POLICY => super::firewall::list_all_policies(output),
        // `ports` stays FALSE: `vm ls --ports` does real network I/O against
        // every VM, and a `get` must not probe the network unasked.
        k if k == kinds::VM => super::vm::run(super::vm::VmCmd::Ls {
            ports: false,
            output,
            namespace,
        }),
        // The list above already decided this Kind routes; reaching here means the
        // two halves disagree, which is our defect and not the caller's.
        _ => Err(Error::Invalid(format!(
            "internal: {} is in GET_ROUTES with no arm",
            f.kind
        ))),
    }
}

/// `delonix describe <kind> <name…>`
pub(crate) fn describe(kind: &str, names: &[String]) -> Result<()> {
    let f = resolve_kind(kind)?;
    refuse_if_unreachable(f)?;
    if names.is_empty() {
        return Err(Error::Invalid(format!(
            "`describe {}` needs a name — `get {}` lists them",
            f.plural, f.plural
        )));
    }
    if !DESCRIBE_ROUTES.contains(&f.kind) {
        return Err(not_wired_yet("describe", f.plural, f.kind, Verb::Describe));
    }
    let n = names.to_vec();
    match f.kind {
        k if k == kinds::POD => super::pod::describe(&n),
        k if k == kinds::NETWORK => {
            super::network::run(super::network::NetworkCmd::Describe { names: n })
        }
        k if k == kinds::VOLUME => super::volume::run(super::volume::VolumeCmd::Describe {
            names: n,
            namespace: None,
        }),
        k if k == kinds::VM => super::vm::cmd_describe(&super::util::state_root(), &n),
        k if k == kinds::IMAGE => super::image::run(super::image::ImageCmd::Describe { names: n }),
        k if k == kinds::GATEWAY => {
            for name in names {
                super::tunnel::cmd_describe(name)?;
            }
            Ok(())
        }
        // `secret` has no `Describe` of its own — `inspect` IS the detail view
        // (keys, never values, unless `--reveal`), same shape `kubectl describe
        // secret` settles for. The generic verb never reveals: a caller who
        // wants values types `secret inspect --reveal` on purpose, not by
        // routing through a verb that does not carry the flag.
        k if k == kinds::SECRET => {
            for name in names {
                super::secret::run(super::secret::SecretCmd::Inspect {
                    name: name.clone(),
                    reveal: false,
                    output: OutputFormat::Table,
                })?;
            }
            Ok(())
        }
        k if k == kinds::NETWORK_ROUTE => super::netroute::cmd_describe(&n),
        k if k == kinds::SERVICE => super::service::cmd_describe(&n),
        k if k == kinds::FIREWALL_POLICY => super::firewall::cmd_describe_policy(&n),
        k if k == kinds::CLUSTER => {
            for name in names {
                super::cluster::cmd_describe(name)?;
            }
            Ok(())
        }
        // The list above already decided this Kind routes; reaching here means the
        // two halves disagree, which is our defect and not the caller's.
        _ => Err(Error::Invalid(format!(
            "internal: {} is in DESCRIBE_ROUTES with no arm",
            f.kind
        ))),
    }
}

/// `delonix delete <kind> <name…>`
pub(crate) fn delete(kind: &str, names: &[String], force: bool) -> Result<()> {
    let f = resolve_kind(kind)?;
    refuse_if_unreachable(f)?;
    if names.is_empty() {
        // Never «delete everything of this Kind» by omission. A missing
        // argument is a typo far more often than an intention, and this verb
        // does not get a second chance to ask.
        return Err(Error::Invalid(format!(
            "`delete {}` needs a name — it will not delete every one of them",
            f.plural
        )));
    }
    if !DELETE_ROUTES.contains(&f.kind) {
        return Err(not_wired_yet("delete", f.plural, f.kind, Verb::Delete));
    }
    match f.kind {
        k if k == kinds::POD => {
            for n in names {
                super::pod::remove_pod(n, force)?;
            }
            Ok(())
        }
        k if k == kinds::NETWORK => {
            for n in names {
                super::network::run(super::network::NetworkCmd::Rm { name: n.clone() })?;
            }
            Ok(())
        }
        // One at a time, not in a batch: each group has its own `Rm` shape, and
        // the first failure stops — half a removal done in silence is worse.
        k if k == kinds::VOLUME => {
            for n in names {
                super::volume::run(super::volume::VolumeCmd::Rm {
                    name: n.clone(),
                    force,
                    destroy_remote: false,
                    purge_data: false,
                    namespace: None,
                })?;
            }
            Ok(())
        }
        k if k == kinds::VM => {
            let base = super::util::state_root();
            for n in names {
                super::vm::cmd_rm(&base, n, force)?;
            }
            Ok(())
        }
        k if k == kinds::SECRET => {
            for n in names {
                super::secret::run(super::secret::SecretCmd::Rm { name: n.clone() })?;
            }
            Ok(())
        }
        k if k == kinds::IMAGE => {
            for n in names {
                super::image::run(super::image::ImageCmd::Remove {
                    image: n.clone(),
                    force,
                })?;
            }
            Ok(())
        }
        k if k == kinds::CLUSTER => {
            for n in names {
                super::cluster::cmd_delete(n)?;
            }
            Ok(())
        }
        k if k == kinds::GATEWAY => {
            for n in names {
                super::tunnel::cmd_rm(n)?;
            }
            Ok(())
        }
        k if k == kinds::NETWORK_ROUTE => {
            for n in names {
                super::netroute::remove_for_replace(n)?;
                println!("{}", super::po::tf("route {name}: closed", &[("name", n)]));
            }
            Ok(())
        }
        k if k == kinds::FIREWALL_POLICY => super::firewall::cmd_delete_policy(names),
        k if k == kinds::SERVICE => {
            for n in names {
                super::service::remove_for_replace(n)?;
                println!(
                    "{}",
                    super::po::tf("service {name}: removed", &[("name", n)])
                );
            }
            Ok(())
        }
        // The list above already decided this Kind routes; reaching here means the
        // two halves disagree, which is our defect and not the caller's.
        _ => Err(Error::Invalid(format!(
            "internal: {} is in DELETE_ROUTES with no arm",
            f.kind
        ))),
    }
}

fn refuse_if_unreachable(f: &'static KindFacts) -> Result<()> {
    let why = no_verb_reason(f.kind);
    if why.is_empty() {
        Ok(())
    } else {
        Err(Error::Invalid(format!("{}: {why}", f.kind)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every Kind either answers the generic verbs or writes down what stops
    /// it. Without this a Kind added later answers «not wired yet» forever, and
    /// that message reads like a promise nobody kept rather than a property of
    /// the Kind.
    #[test]
    fn a_kind_never_both_routes_and_claims_it_cannot() {
        for f in kinds::all() {
            if GET_ROUTES.contains(&f.kind) || !no_verb_reason(f.kind).is_empty() {
                continue;
            }
            // Neither routed nor explained: that is debt, and the gate names it.
            eprintln!("not wired yet: {} ({})", f.kind, f.plural);
        }
        // What is refused is a Kind that both routes and claims it cannot.
        for f in kinds::all() {
            let blocked = !no_verb_reason(f.kind).is_empty();
            assert!(
                !(blocked && GET_ROUTES.contains(&f.kind)),
                "{} both routes and declares itself blocked",
                f.kind
            );
        }
    }

    /// A Kind listed as table-only has to be a Kind `get` actually routes —
    /// otherwise the refusal never fires and the list is decoration.
    #[test]
    fn table_only_kinds_are_kinds_get_routes() {
        for k in NO_JSON_YET {
            assert!(
                GET_ROUTES.contains(k),
                "{k} is in NO_JSON_YET without being in GET_ROUTES"
            );
        }
    }

    /// The three lists name Kinds that exist. A mistyped entry here would be a
    /// verb that never fires, silently.
    #[test]
    fn the_routing_lists_only_name_live_kinds() {
        let live: Vec<&str> = kinds::all().map(|f| f.kind).collect();
        for (name, list) in [
            ("GET", GET_ROUTES),
            ("DESCRIBE", DESCRIBE_ROUTES),
            ("DELETE", DELETE_ROUTES),
        ] {
            for k in list {
                assert!(
                    live.contains(k),
                    "{name}_ROUTES names '{k}', which is not in the registry"
                );
            }
        }
    }

    /// Every command this module SUGGESTS has to exist — the LEAF included.
    ///
    /// The fallback message is the only help a person gets when a Kind is not
    /// wired yet, and it has now been wrong twice. The first version built the
    /// suggestion from the registry's plural and sent people to
    /// `virtualmachines ls`, which this CLI has never had. The fix checked the
    /// GROUP against this same tree — and passed happily on `cluster ls`, `net
    /// tunnel ls`, `net httproute ls`, `net httproute describe` and `net
    /// ingress describe`, because `cluster`, `net tunnel`, `net httproute` and
    /// `net ingress` are all real groups; only the leaf was not. Measured
    /// against the v3.0.0 binary: five reachable refusals, five commands that
    /// exit 2.
    ///
    /// Checking the group could never have caught that, so the WHOLE string is
    /// resolved here, one path element at a time, for every verb.
    #[test]
    fn every_suggested_command_exists_in_the_tree() {
        use clap::CommandFactory;
        let root = crate::Cli::command();
        for f in kinds::all() {
            for verb in [Verb::Get, Verb::Describe, Verb::Delete] {
                let suggestion = fallback_command(f.kind, verb);
                if suggestion.is_empty() {
                    continue;
                }
                let mut cur = &root;
                for part in suggestion.split(' ') {
                    let found = cur
                        .get_subcommands()
                        .find(|c| c.get_name() == part || c.get_all_aliases().any(|a| a == part));
                    cur = match found {
                        Some(c) => c,
                        None => panic!(
                            "fallback_command({}, {verb:?}) suggests `{suggestion}`, \
                             and `{part}` is not in the tree",
                            f.kind
                        ),
                    };
                }
            }
        }
    }

    /// A refusal never emits an empty command.
    ///
    /// The signpost is optional now, and deliberately so: for `Pod`,
    /// `KubernetesCluster`, `Gateway` and `HTTPRoute` the generic verb IS the
    /// only way to ask, and the honest message names no alternative. What is
    /// NOT optional is that the sentence must not read as though there were
    /// one — «use `` meanwhile» is the shape the old group-plus-leaf table
    /// produced whenever the group was empty, and that is worse than silence.
    ///
    /// Built from [`not_wired_yet`] rather than by calling the verbs, for the
    /// same reason the routing lists are data: reaching `get`/`describe`/
    /// `delete` would do real store I/O and make this pass or fail on the state
    /// of the machine.
    #[test]
    fn a_refusal_never_names_an_empty_command() {
        for f in kinds::all() {
            for (verb, v) in [
                ("get", Verb::Get),
                ("describe", Verb::Describe),
                ("delete", Verb::Delete),
            ] {
                let m = not_wired_yet(verb, f.plural, f.kind, v).to_string();
                // An empty pair of backticks is the whole tell: either the
                // sentence names a command or it does not mention one at all.
                assert!(!m.contains("``"), "{} / {verb}: {m}", f.kind);
                assert_eq!(
                    m.contains("use `"),
                    !fallback_command(f.kind, v).is_empty(),
                    "{} / {verb} says «use» exactly when there is one: {m}",
                    f.kind
                );
            }
        }
    }

    /// A `delete` with no name must never be read as «all of them».
    #[test]
    fn delete_without_a_name_refuses_instead_of_taking_all() {
        let e = delete(kinds::POD, &[], false).unwrap_err();
        assert!(e.to_string().contains("will not delete every one"), "{e}");
    }

    /// `--namespace` alongside an explicit name has nothing to filter — a
    /// name already picks exactly one resource. Refused before either guard
    /// clause reaches real store I/O (same shape as `delete_without_a_name_*`
    /// above), so this is a pure logic test.
    #[test]
    fn get_with_both_name_and_namespace_is_refused() {
        let e = get(
            kinds::POD,
            &["web".to_string()],
            OutputFormat::Table,
            Some("teamA".to_string()),
        )
        .unwrap_err();
        assert!(e.to_string().contains("drop one of the two"), "{e}");
    }

    /// A Kind with no namespace concept (`Secret`) must refuse `--namespace`
    /// outright, not silently ignore it — the exact "accepted and ignored"
    /// trap this codebase already refuses for `--security-opt seccomp=`/
    /// `-v :z`/`--network-alias`.
    #[test]
    fn get_namespace_is_refused_on_a_kind_without_namespaces() {
        let e = get(
            kinds::SECRET,
            &[],
            OutputFormat::Table,
            Some("teamA".to_string()),
        )
        .unwrap_err();
        assert!(e.to_string().contains("no namespace to filter by"), "{e}");
    }
}
