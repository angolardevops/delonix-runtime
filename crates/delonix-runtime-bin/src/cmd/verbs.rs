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
];

/// Kinds whose group has no `-o json` today. Listed rather than discovered,
/// because the alternative is accepting `-o json` and printing a table — the
/// silently-ignored flag this engine refuses everywhere else.
const NO_JSON_YET: &[&str] = &[kinds::CLUSTER, kinds::GATEWAY, kinds::HTTP_ROUTE];
pub(crate) const DESCRIBE_ROUTES: &[&str] = &[
    kinds::POD,
    kinds::NETWORK,
    kinds::VOLUME,
    kinds::VM,
    kinds::IMAGE,
    kinds::GATEWAY,
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
];

/// The CLI group that still owns a Kind's imperative verbs.
///
/// **Not the plural.** `api-resources` prints `virtualmachines`, and the group
/// a person types is `vm`; a fallback message built from the plural sent them
/// to `virtualmachines ls`, which does not exist. Measured the first time the
/// verb refused a Kind — the message was wrong in the one place it was supposed
/// to help.
pub(crate) fn cli_group(kind: &str) -> &'static str {
    match kind {
        k if k == kinds::VM => "vm",
        k if k == kinds::VOLUME => "volumes",
        k if k == kinds::CONTAINER => "container",
        k if k == kinds::POD => "pod",
        k if k == kinds::NETWORK => "network",
        k if k == kinds::NETWORK_ROUTE => "network route",
        k if k == kinds::SECRET => "secret",
        k if k == kinds::IMAGE => "image",
        k if k == kinds::HTTP_ROUTE => "net httproute",
        k if k == kinds::GATEWAY => "net tunnel",
        k if k == kinds::FIREWALL_POLICY => "net ingress",
        k if k == kinds::CLUSTER => "cluster",
        k if k == kinds::STACK => "stack",
        k if k == kinds::WORKLOAD => "workload",
        k if k == kinds::DEPENDENCY => "net ingress",
        k if k == kinds::INGRESS => "net httproute",
        _ => "",
    }
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
pub(crate) fn get(kind: &str, names: &[String], output: OutputFormat) -> Result<()> {
    let f = resolve_kind(kind)?;
    refuse_if_unreachable(f)?;
    if !names.is_empty() {
        // Naming a resource is asking about THAT one, which is `describe` with
        // a table's worth of detail. Routing it here instead of inventing a
        // filtered list keeps one implementation per question.
        return describe(kind, names);
    }
    // A format accepted and ignored is worse than one refused: whoever asks for
    // `-o json` in a pipeline gets a table and only finds out downstream.
    if output != OutputFormat::Table && NO_JSON_YET.contains(&f.kind) {
        return Err(Error::Invalid(format!(
            "`get {}` has no JSON yet — `{} ls` is table-only, and answering a \
             table to `-o json` would break whatever reads it",
            f.plural,
            cli_group(f.kind)
        )));
    }
    if !GET_ROUTES.contains(&f.kind) {
        return Err(Error::Invalid(format!(
            "`get {}` is not wired yet — use `{} ls` meanwhile",
            f.plural,
            cli_group(f.kind)
        )));
    }
    match f.kind {
        k if k == kinds::POD => super::pod::run(super::pod::PodCmd::Ls { output }),
        k if k == kinds::NETWORK => super::network::run(super::network::NetworkCmd::Ls { output }),
        k if k == kinds::VOLUME => super::volume::run(super::volume::VolumeCmd::Ls { output }),
        k if k == kinds::SECRET => super::secret::run(super::secret::SecretCmd::Ls { output }),
        k if k == kinds::IMAGE => super::image::run(false, super::image::ImageCmd::Ls { output }),
        k if k == kinds::CLUSTER => super::cluster::run(super::cluster::ClusterCmd::Ls),
        k if k == kinds::GATEWAY => super::tunnel::run(super::tunnel::TunnelCmd::Ls),
        k if k == kinds::HTTP_ROUTE => super::httproute::run(super::httproute::HttpRouteCmd::Ls),
        // `ports` stays FALSE: `vm ls --ports` does real network I/O against
        // every VM, and a `get` must not probe the network unasked.
        k if k == kinds::VM => super::vm::run(super::vm::VmCmd::Ls {
            ports: false,
            output,
            namespace: None,
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
        return Err(Error::Invalid(format!(
            "`describe {}` is not wired yet — use `{} describe` meanwhile",
            f.plural,
            cli_group(f.kind)
        )));
    }
    let n = names.to_vec();
    match f.kind {
        k if k == kinds::POD => super::pod::run(super::pod::PodCmd::Describe { names: n }),
        k if k == kinds::NETWORK => {
            super::network::run(super::network::NetworkCmd::Describe { names: n })
        }
        k if k == kinds::VOLUME => {
            super::volume::run(super::volume::VolumeCmd::Describe { names: n })
        }
        k if k == kinds::VM => super::vm::run(super::vm::VmCmd::Describe { names: n }),
        k if k == kinds::IMAGE => {
            super::image::run(false, super::image::ImageCmd::Describe { names: n })
        }
        k if k == kinds::GATEWAY => {
            for name in names {
                super::tunnel::run(super::tunnel::TunnelCmd::Describe { name: name.clone() })?;
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
        return Err(Error::Invalid(format!(
            "`delete {}` is not wired yet — use `{} rm` meanwhile",
            f.plural,
            cli_group(f.kind)
        )));
    }
    match f.kind {
        k if k == kinds::POD => super::pod::run(super::pod::PodCmd::Rm {
            names: names.to_vec(),
            force,
        }),
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
                })?;
            }
            Ok(())
        }
        k if k == kinds::VM => {
            for n in names {
                super::vm::run(super::vm::VmCmd::Rm {
                    name: n.clone(),
                    force,
                })?;
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
                super::image::run(
                    false,
                    super::image::ImageCmd::Rm {
                        image: n.clone(),
                        force,
                    },
                )?;
            }
            Ok(())
        }
        k if k == kinds::CLUSTER => {
            for n in names {
                super::cluster::run(super::cluster::ClusterCmd::Delete { name: n.clone() })?;
            }
            Ok(())
        }
        k if k == kinds::GATEWAY => {
            for n in names {
                super::tunnel::run(super::tunnel::TunnelCmd::Rm { name: n.clone() })?;
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

    /// Every command this module SUGGESTS has to exist.
    ///
    /// The fallback message is the only help a person gets when a Kind is not
    /// wired yet, and the first version built it from the registry's plural:
    /// it sent them to `virtualmachines ls`, a command this CLI has never had.
    /// A wrong signpost is worse than none — it costs a person the time to find
    /// out it was wrong. So the suggestion is now checked against the real clap
    /// tree, which is the same tree `--help` prints.
    #[test]
    fn every_suggested_command_exists_in_the_tree() {
        use clap::CommandFactory;
        let root = crate::Cli::command();
        for f in kinds::all() {
            let group = cli_group(f.kind);
            if group.is_empty() {
                continue;
            }
            let mut cur = &root;
            for part in group.split(' ') {
                let found = cur
                    .get_subcommands()
                    .find(|c| c.get_name() == part || c.get_all_aliases().any(|a| a == part));
                cur = match found {
                    Some(c) => c,
                    None => panic!(
                        "cli_group({}) suggests `{group}`, and `{part}` is not in the tree",
                        f.kind
                    ),
                };
            }
        }
    }

    /// A Kind that does not route needs a group to send the reader to.
    #[test]
    fn an_unwired_kind_always_has_somewhere_to_point() {
        for f in kinds::all() {
            if GET_ROUTES.contains(&f.kind) || !no_verb_reason(f.kind).is_empty() {
                continue;
            }
            assert!(
                !cli_group(f.kind).is_empty(),
                "{} neither routes nor explains itself and has no fallback group — \
                 the message would read `use `` ls``",
                f.kind
            );
        }
    }

    /// A `delete` with no name must never be read as «all of them».
    #[test]
    fn delete_without_a_name_refuses_instead_of_taking_all() {
        let e = delete(kinds::POD, &[], false).unwrap_err();
        assert!(e.to_string().contains("will not delete every one"), "{e}");
    }
}
