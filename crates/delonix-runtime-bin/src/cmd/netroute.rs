//! `kind: NetworkRoute` — a DIRECTED path between two networks (ADR-0013 tier B).
//!
//! Networks are isolated from each other by default. This declares that one may
//! reach another, and it compiles to ONE element in the holder's `@netpair`
//! verdict map — the spike behind the ADR proved the forwarding already exists
//! and that an explicit pairwise drop is what closes it, so opening a path is an
//! exemption and not a dataplane.
//!
//! **A route says a packet MAY cross; it never says it is allowed.** The
//! per-workload `fwcont` chains still decide, and a namespace boundary crossed
//! by a route still needs its own `kind: Dependency` or policy. That is what
//! makes this compose with the isolation model instead of undermining it.
//!
//! Its own document type rather than a `routes:` field inside `kind: Network`,
//! because a route is a RELATIONSHIP and belongs to neither end: expressible
//! from both sides is how two documents come to disagree about one route — the
//! bug `FirewallPolicy` already pays for by REFUSING two policies for the same
//! target and direction.

use super::kinds as k;
use serde::{Deserialize, Serialize};

use super::manifest::{self, ManifestDoc};
use delonix_runtime_core::Result;

/// `spec` of `kind: NetworkRoute`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct NetworkRouteSpec {
    /// Network that may INITIATE. Its workloads reach `to`.
    pub from: String,
    /// Network reached. Its workloads do NOT get to reach `from` back — the
    /// return traffic of a conversation `from` started flows (established), a
    /// new one from this side does not. Same asymmetry `kind: Dependency` gives
    /// between containers.
    pub to: String,
}

/// Known fields of the `spec` (drift-guard).
pub const NETWORK_ROUTE_SPEC_FIELDS: &[&str] = &["from", "to"];

/// Fields the plan compares.
///
/// Both, and both COLD — which here means the `Update` arm is unreachable rather
/// than unimplemented: the resource's identity IS the pair (see [`route_name`]),
/// so a document whose `from`/`to` changed is a different route, and the plan
/// says `+`/`-`, never `~`. Compared anyway because a field nobody compares is a
/// field the `--fields` table cannot honestly list.
pub const RECONCILED_ROUTE_FIELDS: &[&str] = &["from", "to"];

/// The name a route is known by in the plan: the ORDERED PAIR.
///
/// **Not `metadata.name`, and this is the decision the whole Kind hangs on.** The
/// reconciler matches resources by `(kind, name)`. With the document's name as
/// identity, renaming the document yields `Create(new)` + `Delete(old)` for the
/// SAME nft element — and since `--prune` runs last, it would close the very path
/// the same apply had just opened. Silent, and a security boundary.
///
/// Precedent: an `Image`'s identity is its ref, not the document's name. Here it
/// is stronger still, because this module already says a route belongs to neither
/// end. The cost is that the plan prints `NetworkRoute web->db` instead of the
/// document's name, which is more informative anyway.
pub fn route_name(from: &str, to: &str) -> String {
    format!("{from}->{to}")
}

fn route_fields(from: &str, to: &str) -> std::collections::BTreeMap<String, String> {
    let mut f = std::collections::BTreeMap::new();
    f.insert("from".into(), from.to_string());
    f.insert("to".into(), to.to_string());
    f
}

pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: NetworkRouteSpec = manifest::spec_of(doc)?;
    Ok(super::reconcile::Desired {
        kind: k::NETWORK_ROUTE.into(),
        name: route_name(&spec.from, &spec.to),
        fields: route_fields(&spec.from, &spec.to),
        converges: true,
        // A route CAN be owned, and it has to be: without a stamp it is never a
        // `Delete` candidate, so the manifest that opened an exemption to the
        // isolation model could never close it. `infra::RouteDef` exists to hold
        // this label.
        ownable: true,
    })
}

/// Every route this node has declared — the enumeration `--prune` needs.
///
/// Reads the RECORD and not the live map, deliberately. See `infra::RouteDef`:
/// the `@netpair` lives in the holder's ephemeral netns, so planning against it
/// would report drift forever on an idle node.
pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    Ok(delonix_net::infra::route_list()
        .into_iter()
        .map(|r| super::reconcile::Actual {
            kind: k::NETWORK_ROUTE.into(),
            name: route_name(&r.from, &r.to),
            fields: route_fields(&r.from, &r.to),
            owner: r.labels.get(super::reconcile::STACK_LABEL).cloned(),
            last_applied: r
                .annotations
                .get(super::reconcile::LAST_APPLIED)
                .and_then(|raw| super::reconcile::decode_last_applied(raw)),
        })
        .collect())
}

/// Splits a plan name back into its pair. Total, because `route_name` is the only
/// thing that builds these.
fn split_route_name(name: &str) -> Result<(&str, &str)> {
    name.split_once("->").ok_or_else(|| {
        delonix_runtime_core::Error::Invalid(format!(
            "'{name}' is not a route name (expected `<from>-><to>`)"
        ))
    })
}

/// Records that this stack owns the route, and what it last applied.
pub(crate) fn stamp(
    name: &str,
    stack: &str,
    fields: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let (from, to) = split_route_name(name)?;
    delonix_net::infra::route_set_metadata(
        from,
        to,
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
}

/// Closes the path. This is what `--prune` and `stack destroy` call, and its
/// absence was the defect: a route removed from the manifest stayed open.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    let (from, to) = split_route_name(name)?;
    delonix_net::infra::network_route(from, to, false)
}

/// What the dataplane is doing about a route that IS declared.
///
/// Three states and not two, and the third is the one that matters: «I could
/// not ask the holder» is not «the route is not live». The `@netpair` lives in
/// the holder's EPHEMERAL netns — on an idle node it does not exist at all —
/// and reading that as «the path closed» is the mistake this Kind already paid
/// for once, with the drift gate red every day over a manifest nobody touched.
pub(crate) enum LiveState {
    /// The counters answer **«did this route ever carry traffic?»** — the
    /// question ADR-0013 left marked as unanswerable, because the exemptions
    /// were the one family of rules in this engine without a `counter`. An open
    /// path that never passed a packet and one passing thousands read exactly
    /// alike until they existed.
    Open { packets: u64, bytes: u64 },
    /// Declared, absent from the map. The holder puts it back from the record
    /// when the bridge is reborn.
    NotLive,
    /// The holder did not answer. Says nothing about the route.
    Unreachable,
}

/// ONE query to the holder, to be reused for every route in a listing.
///
/// `handle_control` is the serialization point of the whole ingress: a caller
/// queues behind every attach in front of it, and the measured tail is not
/// short. Asking once per route would turn a listing of N routes into N trips
/// through that queue for an answer that does not change between them.
pub(crate) fn live_snapshot() -> Option<Vec<(String, String, u64, u64)>> {
    delonix_net::infra::network_routes_live_counted().ok()
}

/// Resolves one route against a snapshot taken by [`live_snapshot`].
pub(crate) fn live_state(
    snapshot: &Option<Vec<(String, String, u64, u64)>>,
    from: &str,
    to: &str,
) -> LiveState {
    let Some(live) = snapshot else {
        return LiveState::Unreachable;
    };
    match live.iter().find(|(a, b, _, _)| a == from && b == to) {
        Some((_, _, packets, bytes)) => LiveState::Open {
            packets: *packets,
            bytes: *bytes,
        },
        None => LiveState::NotLive,
    }
}

/// The sentence for a state — SHARED by `stack ls` and `network route`.
///
/// One owner for the wording, for the reason `fw_rule_tail` has one: two
/// readings of the same state that phrase it separately drift the day one of
/// them is edited, and then the same route reads as two different things
/// depending on which command was typed.
pub(crate) fn live_label(state: &LiveState) -> String {
    match state {
        LiveState::Open { packets, .. } if *packets == 0 => {
            super::po::t("open, no traffic yet").into()
        }
        LiveState::Open { packets, .. } => super::po::tf(
            "open ({packets} packets)",
            &[("packets", &packets.to_string())],
        ),
        LiveState::NotLive => super::po::t("declared, not live").into(),
        LiveState::Unreachable => super::po::t("declared, holder unreachable").into(),
    }
}

/// One row of `delonix network route` with no arguments.
#[derive(Serialize)]
struct RouteLsRow {
    from: String,
    to: String,
    state: String,
    /// `null` when the holder could not be asked or the route is not live —
    /// never `0`, which would read as «open and silent».
    packets: Option<u64>,
    bytes: Option<u64>,
    /// Owning stack (`delonix.io/stack`), when the route was applied by one.
    stack: Option<String>,
}

/// `delonix network route` with no arguments — every route this node declares.
///
/// Shows BOTH sides on purpose: the record (what was asked for) and the live
/// map (what the kernel is doing). They disagree routinely and legitimately,
/// and a listing that showed only one of them would be the dishonest half.
pub(crate) fn cmd_ls(format: super::output::OutputFormat) -> Result<()> {
    let mut routes = delonix_net::infra::route_list();
    // The pair IS the identity of a route (there is no name someone chose), so
    // it is also the only stable sort key.
    routes.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));
    let snapshot = live_snapshot();

    if format == super::output::OutputFormat::Json {
        let rows: Vec<RouteLsRow> = routes
            .iter()
            .map(|r| {
                let st = live_state(&snapshot, &r.from, &r.to);
                let (packets, bytes) = match st {
                    LiveState::Open { packets, bytes } => (Some(packets), Some(bytes)),
                    _ => (None, None),
                };
                RouteLsRow {
                    from: r.from.clone(),
                    to: r.to.clone(),
                    state: live_label(&st),
                    packets,
                    bytes,
                    stack: r.labels.get(super::reconcile::STACK_LABEL).cloned(),
                }
            })
            .collect();
        return super::output::print_json(&rows);
    }

    let mut t = super::output::Table::new(&["FROM", "TO", "STATE", "PACKETS", "BYTES", "STACK"]);
    for r in &routes {
        let st = live_state(&snapshot, &r.from, &r.to);
        let (packets, bytes) = match st {
            LiveState::Open { packets, bytes } => {
                (packets.to_string(), super::output::fmt_size(bytes))
            }
            _ => ("-".to_string(), "-".to_string()),
        };
        t.row(vec![
            r.from.clone(),
            r.to.clone(),
            live_label(&st),
            packets,
            bytes,
            r.labels
                .get(super::reconcile::STACK_LABEL)
                .cloned()
                .unwrap_or_else(|| "-".to_string()),
        ]);
    }
    // STACK disappears on a node where no route was applied by a manifest —
    // the same reason `vm ls` hides NAMESPACE when every row says `default`.
    t.drop_uninformative().print();
    Ok(())
}

/// For `stack ls`: whether the route is declared, and what the dataplane is
/// actually doing about it.
///
/// The two can disagree, and saying which is which is the point: a record with no
/// live element means the holder is down (or somebody deleted it by hand), and
/// reporting that as «present» would be the dishonesty this Kind was fixed to
/// remove.
pub(crate) fn presence_of(doc: &ManifestDoc) -> (String, String) {
    let Ok(spec) = manifest::spec_of::<NetworkRouteSpec>(doc) else {
        return ("?".into(), super::po::t("invalid spec").into());
    };
    if delonix_net::infra::route_get(&spec.from, &spec.to).is_none() {
        return ("no".into(), "-".into());
    }
    // The wording lives in `live_label`, shared with `network route`. Two
    // readings of one state phrased separately drift the day one of them is
    // edited, and then the same route reads as two different things depending
    // on which command was typed.
    let state = live_state(&live_snapshot(), &spec.from, &spec.to);
    ("yes".into(), live_label(&state))
}

/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: NetworkRouteSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec)
        .map_err(|e| delonix_runtime_core::Error::Invalid(format!("dry-run: {e}")))
}

/// Applies the `kind: NetworkRoute` documents.
///
/// Idempotent, like every other apply here: `nft add element` on an element that
/// is already there is a no-op, so re-applying a manifest opens nothing twice.
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, k::NETWORK_ROUTE) {
        let spec: NetworkRouteSpec = manifest::spec_of(doc)?;
        delonix_net::infra::network_route(&spec.from, &spec.to, true)?;
        println!(
            "{}",
            super::po::tf(
                "networkroute/{name}: {from} -> {to} open",
                &[
                    ("name", &doc.metadata.name),
                    ("from", &spec.from),
                    ("to", &spec.to),
                ],
            )
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::reconcile::{plan, Action, Actual};

    fn doc(nome: &str, from: &str, to: &str) -> ManifestDoc {
        serde_yaml::from_str(&format!(
            "apiVersion: delonix.io/v1\nkind: NetworkRoute\nmetadata:\n  name: {nome}\n\
             spec:\n  from: {from}\n  to: {to}\n"
        ))
        .unwrap()
    }

    fn actual_de(from: &str, to: &str, dono: Option<&str>) -> Actual {
        Actual {
            kind: "NetworkRoute".into(),
            name: route_name(from, to),
            fields: route_fields(from, to),
            owner: dono.map(String::from),
            last_applied: None,
        }
    }

    /// **O defeito que esta série fecha, como dados.**
    ///
    /// Uma rota é a EXCEPÇÃO ao isolamento entre redes. Tirá-la do manifesto tem
    /// de a propor para remoção — antes, `apply --prune` e `stack destroy`
    /// deixavam o caminho aberto e o plano dizia «sem alterações».
    ///
    /// **Alcance deste teste, para não valer mais do que vale**: prova a decisão
    /// do reconciliador dado um `Actual` com dono. O que o torna alcançável na
    /// vida real são três ligações que um teste puro não vê — o `actual()` a
    /// enumerar o registo, o `stamp` a escrever o dono, e o braço do
    /// `destroy_one`. Essas provam-se no cenário de caos, com o ciclo completo.
    #[test]
    fn uma_rota_tirada_do_manifesto_e_candidata_a_remocao() {
        let p = plan(&[], &[actual_de("web", "db", Some("s"))], "s");
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].action, Action::Delete);
        assert_eq!(p[0].name, "web->db");
    }

    /// **É este que é sensível ao `ownable`**, e a distinção custou-me uma
    /// reversão a passar: o `Delete` decide-se pelo `owner` do lado ACTUAL, por
    /// isso o teste acima passa mesmo com `ownable: false`. Quem depende do
    /// `ownable` é a ADOPÇÃO — uma rota que já existe e não pertence a stack
    /// nenhuma tem de ser tomada por esta, senão nunca ganha carimbo, e sem
    /// carimbo nunca é podada.
    ///
    /// Com `ownable: false` isto dá `NoOp` e o defeito volta inteiro, em
    /// silêncio.
    #[test]
    fn uma_rota_ja_existente_e_adoptada_pela_stack_que_a_declara() {
        let p = plan(
            &[desired(&doc("r", "web", "db")).unwrap()],
            &[actual_de("web", "db", None)],
            "s",
        );
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(
            p[0].action,
            Action::Adopt,
            "uma rota sem dono declarada por esta stack tem de ser adoptada, \
             senão nunca é carimbada e nunca pode ser podada: {p:?}"
        );
    }

    /// E o CONTROLO que impede a correcção de ir longe demais: uma rota criada à
    /// mão (`delonix network route`, sem carimbo) não pertence a stack nenhuma e
    /// **não** pode ser apagada por um `--prune`. Um apply que remove o que não
    /// criou é a falha que destrói a confiança na ferramenta.
    #[test]
    fn uma_rota_sem_dono_nao_e_apagada_por_um_prune() {
        let p = plan(&[], &[actual_de("web", "db", None)], "s");
        assert!(
            p.is_empty(),
            "uma rota imperativa foi proposta para remoção: {p:?}"
        );
    }

    /// **A identidade é o PAR, não o nome do documento.**
    ///
    /// Com `metadata.name` como identidade, renomear o documento dava
    /// `Create(novo)` + `Delete(antigo)` para o MESMO elemento nft — e como o
    /// `--prune` corre em último lugar, fecharia o caminho que o próprio apply
    /// acabara de abrir.
    #[test]
    fn renomear_o_documento_de_uma_rota_nao_muda_nada() {
        let antes = desired(&doc("rota-antiga", "web", "db")).unwrap();
        let depois = desired(&doc("rota-nova", "web", "db")).unwrap();
        assert_eq!(antes.name, depois.name);
        let p = plan(&[depois], &[actual_de("web", "db", Some("s"))], "s");
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].action, Action::NoOp, "{p:?}");
    }

    /// Trocar as pontas é outra rota, não uma alteração desta — a assimetria é o
    /// que o Kind existe para exprimir.
    #[test]
    fn inverter_o_sentido_e_outra_rota() {
        let ida = desired(&doc("r", "web", "db")).unwrap();
        assert_ne!(ida.name, route_name("db", "web"));
        let p = plan(&[ida], &[actual_de("db", "web", Some("s"))], "s");
        // Uma a criar, a outra a remover — nunca um update silencioso do sentido.
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p
            .iter()
            .any(|c| c.action == Action::Create && c.name == "web->db"));
        assert!(p
            .iter()
            .any(|c| c.action == Action::Delete && c.name == "db->web"));
    }

    /// O `stamp` tem de saber voltar do nome ao par, senão a posse nunca é
    /// escrita e a rota deixa de ser podável — o defeito de novo, por outra via.
    #[test]
    fn o_nome_do_plano_volta_ao_par() {
        assert_eq!(split_route_name("web->db").unwrap(), ("web", "db"));
        assert!(split_route_name("web").is_err());
    }
}
