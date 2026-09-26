# Adding a Kind

**Before you read:** [Coding conventions](coding-conventions.md), [Architecture](architecture.md) and [The crates](crates.md#delonix-stack) — this page assumes you know what `delonix-stack` owns and why planning is pure.

A Kind is a declarative resource this engine understands — `Container`, `Volume`, `Service`,
and so on (`delonix api-resources` lists them all). Adding one touches more than a single
`match` arm: the table that describes what the Kind IS, the code that applies it, and the
reconciler wiring that lets `stack plan`/`apply` treat it like every other Kind. This page
walks through that in order, with a real Kind — **`Service`** (ADR-0032) — as the worked
example throughout. It is not the newest Kind (`IPPool`, `NetworkGateway`, `NetworkZone` and
`RuntimePolicy` came after it), but it is the one that exercises every path at once: primary,
converging, removable, namespaced, and backed by a registry of its own. The newer Kinds went
through the same steps; every file cited below is read from the tree, not from memory of an
older layout.

## The one table a Kind has to answer

`crates/contexts/delonix-stack/src/kinds.rs` is the single source of what a Kind is. Its own
module doc explains why it exists: before this table, the same facts about a Kind lived in six
separate lists (`KINDS`, `CONVERGING_KINDS`, `TEARDOWN_KINDS`, `kind_honors_namespace`, the
`DECLARATIVOS` of the wait test, and the arms of `presence()`), and they drifted — `Vm`,
`FirewallPolicy` and `ShareVolume` once gained a reconciler adapter and stayed *out* of
`CONVERGING_KINDS`, so the converge step silently skipped them while their own `apply` kept the
resource looking correct through the wrong path.

Adding a Kind starts with one `KindFacts` row:

```rust
pub const SERVICE: &str = "Service";
```

```rust
KindFacts {
    kind: SERVICE,
    plural: "services",
    short: &["svc"],
    api_version: "networking.delonix.io/v1alpha1",
    domain: Domain::NetConnectivity,
    form: Form::Primary,
    in_stack: true,
    stack_group: "services",
    converges: true,
    teardown: true,
    namespaced: Namespaced::Always,
    presence: Presence::Registry,
},
```

Each field is a decision, not a formality:

| Field | Question it answers | Getting it wrong |
|---|---|---|
| `kind` | The name, as a `const` — never a bare string literal. A rename touching every call site was measured at **106 occurrences across ten files** before this table existed; a `const` makes a typo a compile error instead of a silently unreachable `match` arm (a mistyped `&'static str` pattern degrades to a catch-all *binding*, which the crate's own doc-comment on `SECRET`/`NETWORK`/etc. calls out by name). | A second Kind answering to nobody, or a rename that misses a site with no build error. |
| `plural` | The word `delonix get <plural>` accepts. Its own field, not `kind` + `"s"` — `Dependency`→`dependencies` and `Ingress`→`ingresses` do not follow that rule. | `delonix get services` (correct) vs. a guessed `delonix get servicess`. |
| `short` | Accepted abbreviations. Sparse on purpose — a test enforces global uniqueness, so a shortname that collides with another Kind's fails the build rather than silently shadowing it. | Two Kinds answering to the same three letters. |
| `api_version` | The `apiVersion` a manifest writes, split by domain (`compute.delonix.io/…`, `networking.delonix.io/…`, …) since ADR-0020. A column, not a shared constant, precisely so Kinds can move to the split scheme one at a time. | A Kind stuck on the legacy `delonix.io/v1` string, or the wrong group. |
| `domain` | The area of action, shown as the `DOMAIN` column of `stack ls`/`plan --fields`. The three networking ones are split on purpose: `NetConnectivity` answers "does a path exist", `NetPolicy` answers "is traffic on it permitted" — merging them would hide that `NetworkRoute` opens a path while `FirewallPolicy` decides whether to let traffic through it. | A domain that answers a different question than the Kind actually acts on. |
| `form` | What a document of this Kind becomes: `Primary` (its own apply, survives the load), `Sugar(target)` (rewritten into another Kind at load time, disappears), `Aggregate` (expands into the documents it contains, like `Stack`), `Compat(target)` (a foreign schema — `Ingress` is `networking.k8s.io/v1` — compiled onto another Kind's mechanism, and unlike `Sugar` it *survives* the load), or `Sunset(target)` (still primary, but a successor is announced; used when rewriting would silently change what the engine *does* — `Container` cannot lower into a one-member `Pod` because a Pod always builds a shared netns, which is a different runtime shape). | Choosing `Sugar` for something that must keep its own apply, or vice versa. |
| `in_stack` | Whether `stack apply` handles it at all. **Rows with `in_stack: true` must stay a contiguous prefix of the table** — `destroy` derives its teardown order by reversing the stack order, so a row placed after a non-stack Kind changes the apply order with nobody editing an "order" anywhere. A test (`os_kinds_do_stack_sao_um_prefixo_contiguo`) enforces this. | A Kind applied out of dependency order, or a remote-procedure Kind like `KubernetesCluster` (SSH against hosts that already exist, not a local resource) mistakenly wired into the cycle. |
| `stack_group` | The key of a `kind: Stack` `spec` that holds documents of this Kind (`services:`), or `""` when it cannot be grouped. This column governs the expansion: `expand_stack`, the schema, the unknown-field warning and the generated docs all read it, and there is no second list of groups (see [The Stack group](#the-stack-group) below). Not the same question as `in_stack` — `Workload` and `Dependency` are lowered at load, so they are not applied as themselves, yet both are things a person writes inside a Stack. | A Kind `stack apply` handles that cannot be put inside a Stack — which is what happened to `NetworkRoute`, `NetworkAccessRule`, `Service` and `App` while the group list was written by hand. |
| `converges` | Whether a *changed* field is really applied, versus "ensure present" only. `false` is legitimate — `Secret`'s state is encrypted values a plan will not decrypt to compare — but it needs a reason (see `not_converged_reason` below); a generic excuse fails a test. | A Kind that reports `!` on every plan with a reason that reads as "nobody got to it" when the truth is a property of the resource. |
| `teardown` | Whether `destroy_one` can remove it, so `--prune` and `destroy` can promise to. A test (`so_um_kind_convergente_tem_teardown`) refuses a Kind with `teardown: true` and `converges: false` — promising to prune something the plan cannot even represent as changed. | `--prune` promising removal and `destroy_one` refusing mid-run, after earlier Kinds in the teardown order are already gone. |
| `namespaced` | `Never`, `Always`, or `PerDocument`. Not a `bool` — `Volume` genuinely has three answers: none for a plain volume, real for one with a `share:` block; modelling that as `true` would warn "namespace has no effect" on every ordinary volume, and as `false` it would warn the same, wrongly, on a share whose namespace decides which directory its data lives in. | A namespace warning that contradicts what the Kind's own `apply` does with the field. |
| `presence` | How `stack ls`/`wait` learn whether the resource is there: `Registry` (a store answers yes/no), `Derived` (computed from something else — a Pod is its labelled members), `Declarative` (nothing to read back; the resource is a directive applied to a target, and `presence()` answers `-`, which is *not* "absent"), or `NotObservable` (never reaches `presence()` — it does not survive the load, or is not a local resource). | `NetworkRoute` once had no arm in `presence()` at all and fell into `_ => ("?", "unsupported kind")` — printed by `ls`/`describe`, and read by `wait` as pending forever. |

`Service`'s row reads, in one line: applied by the stack, right after the compute Kinds it
selects; grouped under `services:` in a Stack; a real path exists (`NetConnectivity`); primary;
converges without recreating; can be torn down; always namespaced; and a real registry backs it.

## The Stack group

A Kind with `in_stack: true` must have a `stack_group`, and four tests in `kinds.rs` hold the
column in place (ADR-0045):

- **`every_kind_applied_by_the_stack_has_a_group`** — nothing the stack applies can be missing
  from `kind: Stack`.
- **`a_kind_without_a_group_says_why`** — a row with `stack_group: ""` needs an entry in
  `stack_group_absent_reason` (`Stack` itself, `KubernetesCluster`), and a row with a group must
  not have one.
- **`a_group_key_is_unique_and_no_alias_shadows_it`** — two Kinds under one key would merge their
  children and hand one of them the wrong spec.
- **`a_group_is_the_plural_of_its_kind`** — the key is the plural in lowerCamelCase
  (`networkRoutes` for `NetworkRoute`), so nobody has to look it up. Only three older keys are
  exempt (`ingress`, `vms`, `firewallPolicies`), because renaming a group breaks every published
  Stack.

The group then has to be proved end to end, in `bins/delonix-runtime-bin/src/cmd/`:

- `manifest.rs`, **`stack_group_sample`** — the minimal `spec` of one child in the group and the
  Kind it lands as; `every_stack_group_loads` walks every group and fails on one without a sample.
- **`examples/stack.yaml`** — must mention the group; `the_stack_example_names_every_group`
  fails otherwise. This file is also what the user site's Kinds page shows.
- `schema.rs`, **`every_stack_group_is_typed_against_its_kinds_own_spec`** — the group's items
  are typed against the Kind's own spec, so the Kind needs its `TYPED_KINDS` branch (next
  section) before its group can validate.

## The spec type and the schema

A Kind with a typed manifest spec (most of them) needs a `#[derive(Deserialize, Serialize,
JsonSchema)]` struct — `ServiceSpec` for this example, in `bins/delonix-runtime-bin/src/cmd/service.rs`
— and a branch in `TYPED_KINDS` in `bins/delonix-runtime-bin/src/cmd/schema.rs`, plus the matching
arm that names the struct for `manifest_schema`. That constant feeds
`delonix manifest schema` and `delonix explain <Kind>.<field>`, both generated from the same struct
(ADR-0007) so the published schema cannot drift from what the code actually accepts. Leaving a
Kind out is a real, allowed state — `Storage` and `ShareVolume` have no schema on purpose,
because they are rewritten into `Volume` at load and a second struct would just be a
hand-maintained copy of `Volume`'s own fields — but it has to be *said*: `untyped_hint(kind)`
gives the specific redirection, and `todo_kind_conhecido_tem_schema_ou_dica` (in `schema.rs`)
fails the build if a Kind the table knows about is neither in `TYPED_KINDS` nor has a hint. The
generic message ("no typed schema for X") reads as a manifest bug; the hint says it is a property
of the Kind.

Two more places read the spec, both in `bins/delonix-runtime-bin/src/cmd/manifest.rs`:

- **`filled_spec`** — an arm calling the Kind's `spec_with_defaults(doc)`, the round trip through
  the typed struct that `stack apply --dry-run` and `manifest render` print with every default
  filled in.
- **`spec_fields_for`** — an arm returning the Kind's `*_SPEC_FIELDS` list, which is what
  `warn_unknown_fields` checks a document against. The schema's `additionalProperties: false`
  takes its accepted keys from the same list, so a typo in a field name is caught in both places.

**Then regenerate the published schema**, because it is a file an editor fetches, not a copy
someone keeps up by hand:

```bash
delonix manifest schema > docs/schema/v1/delonix.json
```

`o_schema_publicado_esta_em_dia_com_o_codigo` (in `schema.rs`) fails until the file is exactly
what the binary generates. Use the binary built from your tree
(`target/release/delonix` or `cargo run -p delonix-runtime-bin --`), not the one on your `PATH`.

## Wiring the Kind into the reconciler

The reconciler's plan (`crates/contexts/delonix-stack/src/reconcile.rs::plan`) is pure — it never
opens a store or runs a command. Everything that touches the machine lives in
`bins/delonix-runtime-bin/src/cmd/stack.rs`, re-exported as `cmd::kinds`/`cmd::reconcile` from
`delonix-stack` (`pub use delonix_stack::kinds;` in `cmd/mod.rs`, so nothing that already called
`cmd::kinds::…` had to change when the table moved into its own crate). Four functions in
`stack.rs` need a branch for a new Kind that converges:

1. **`desired_of`** — one `reconcile::Desired` per document, with `fields` keyed by the
   **manifest** field name (`matchLabels`, `port`), never the internal record name, because the
   diff is read by whoever wrote the YAML. `Service::desired` builds this from `ServiceSpec`.
2. **`actual_of`** — every instance of the Kind that exists on the machine, so `--prune` has
   something to compare against. `Service::actual` reads `delonix_sdn::infra::service_list()`
   and fills the *same* field names `desired` used, or the diff compares apples to oranges.
3. **`converge_and_stamp`** — applies an `Action::Update` live. `Service` reuses its own
   `apply_one` here (`converge_doc`), because that function already fully overwrites the
   registry entry — the same shape `FirewallPolicy` and `NetworkAccessRule` use, and the same
   reasoning: a separate per-field path would be a second way to write the same record, and two
   ways start to disagree. A Kind with no live update path at all (`Pod` has no hot field)
   returns the explicit "no live update path" error rather than falling through silently.
4. **`stamp_all`** — records ownership (`delonix.io/stack` label) and the applied field map
   (`delonix.io/last-applied` annotation) after a successful apply, which is what turns the next
   plan into a three-way diff instead of a two-way one. `Service::stamp` writes these directly on
   the service's own registry entry — unlike `NetworkAccessRule`, whose rule lives on a
   *container* it does not own, a `Service` has a record entirely its own.

And `destroy_one` needs a branch calling `remove_for_replace` when `teardown: true` —
`Service::remove_for_replace` just calls `delonix_sdn::infra::service_remove`.

**None of this is invented per Kind.** `run_layers` (also in `stack.rs`) is where a document
actually gets *created* the first time, one layer per `in_stack` Kind in table order
(`layers.run(k::SERVICE, "🧭", || super::service::apply(docs))?`) — that `apply(docs)` function is
the same one the imperative CLI group already has, reused rather than duplicated.

## Fields that converge without recreating

If `converges: true`, add an entry to `hot_fields` in `reconcile.rs` naming exactly which
manifest fields can change *live*. This table is a promise the executor has to keep — listing a
field the converge step cannot actually apply turns a clean `Replace` into an `Update` that fails
halfway, which the module doc calls "strictly worse than declaring the replace up front".
`Service` lists `["matchLabels", "port"]`, because `service::apply_one` already fully overwrites
the record with no restart needed — nothing about a `Service` is cold. A field left out of
`hot_fields` still shows up in the plan; it just forces `Action::Replace` (refused without
`--replace <Kind>/<name>`) instead of `Action::Update`.

Three tests in `cmd/stack.rs` keep this table honest against the `kinds.rs` table and against
each other:

- **`as_tres_listas_de_kinds_convergentes_concordam`** — every Kind marked `converges: true` has
  to appear in the `--fields` output (`compared_fields_table`), and vice versa; and a converging
  Kind can never carry `not_converged_reason`'s generic excuse.
- **`todo_kind_nao_convergente_tem_razao_especifica`** — the inverse: every Kind that does *not*
  converge needs its own concrete sentence in `not_converged_reason`, not the shared
  `NOT_CONVERGED_GENERIC`.
- **`todo_kind_convergente_tem_teardown_ou_razao`** — a converging Kind is either removable
  (`kinds::has_teardown`) or has an entry in `no_teardown_reason`; never both, never neither.

## Presence, for `ls`/`describe`/`wait`

Add a branch to `presence(kind, doc, containers)` in `stack.rs` that says whether an instance
exists and, if so, its status string. `Service`'s branch checks
`delonix_sdn::infra::service_get(namespace, name)` and, if found, counts how many live containers
its selector currently matches (reading the *same* index the DNS resolver itself reads, so the
plan can never disagree with what a client asking for the name actually gets). Two tests guard
this column specifically:

- **`todo_o_kind_declarativo_de_kinds_tem_braco_no_presence`** — every Kind whose `presence` is
  `Declarative` really does answer `-` from `presence()` (not the fallback `"?"`/"unsupported
  kind" that `NetworkRoute` fell into before it gained a registry of its own).
- **`um_kind_declarativo_nao_fica_pendente_para_sempre`** — `is_pending("-", kind, status)` is
  `false` for every declarative Kind. `stack wait` used to read `present == "yes"` as the only
  sign of readiness, so a manifest with *any* declarative Kind burned its whole `--timeout`
  waiting for a marker that Kind can never produce.

## The generic verbs and `drift`

`delonix get`/`describe`/`delete <plural>` route by Kind through three lists in
`bins/delonix-runtime-bin/src/cmd/verbs.rs` — `GET_ROUTES`, `DESCRIBE_ROUTES` and
`DELETE_ROUTES` — plus one arm per verb that calls the Kind's own `cmd_ls`, `cmd_describe` and
`remove_for_replace`. A Kind that cannot answer them writes the obstacle in `no_verb_reason`
instead (`Stack` is read from a file, `Workload` lowers at load, …). Be aware of what the gate
does and does not do: `a_kind_never_both_routes_and_claims_it_cannot` **fails** only when a Kind
both routes and claims it cannot; a Kind that is in neither list only produces a
`not wired yet: …` line on stderr during the test run, and `delonix get <plural>` answers
"not wired yet" to the user. Read that line; the build will not stop you.

`delonix drift` compares the `last-applied` stamp with what the machine holds, and most Kinds can
be enumerated from their own store. If your `actual()` needs the parsed documents to answer — the
node keeps no registry that lists the Kind on its own, the way a `NetworkPolicy` lives as nft rules
on a target — add it to `DOC_SCOPED` in `bins/delonix-runtime-bin/src/cmd/drift.rs`.
`doc_scoped_matches_the_stack_wiring` reads `stack.rs` (the call site **and** the `actual`
signature) and fails if the list and the wiring disagree; a module that receives `docs` and ignores
them (`fn actual(_docs: …)`) does not belong in the list.

## Namespaces and completion

If the Kind is namespaced (`namespaced != Namespaced::Never`), it also needs an entry in
`NAMESPACE_SOURCES` (`bins/delonix-runtime-bin/src/cmd/complete.rs`) — either `NsSource::Store(fn)`
reading the Kind's own store, or `NsSource::Via("OtherKind — reason")` when the namespace travels
onto another Kind's record instead (a `Pod`'s namespace is on its member `Container`s, for
example). `every_namespaced_kind_declares_a_source` fails otherwise — the practical effect of
missing this is that shell completion for `--namespace` can never offer a tenant whose only
resource is the new Kind.

## The checklist

For a Kind that behaves like `Service` (primary, converges, has teardown, namespaced):

1. `kinds.rs` — a `pub const` name and a `KindFacts` row, including its `stack_group` (or an
   entry in `stack_group_absent_reason`).
2. A spec struct with `JsonSchema`; a branch in `TYPED_KINDS` and in `manifest_schema`
   (`schema.rs`) — or an entry in `untyped_hint` explaining why not.
3. `manifest.rs` — an arm in `filled_spec` (`spec_with_defaults`), an arm in `spec_fields_for`,
   and a sample in `stack_group_sample`; the group in `examples/stack.yaml`.
4. `stack.rs` — branches in `desired_of`, `actual_of`, `converge_and_stamp`, `stamp_all`,
   `destroy_one`, `presence`, and a layer in `run_layers` calling the Kind's own `apply(docs)`.
5. `reconcile.rs` — a `hot_fields` entry naming the fields that converge live.
6. If the Kind does not converge or cannot be torn down: a specific sentence in
   `not_converged_reason` / `no_teardown_reason` (`stack.rs`).
7. `verbs.rs` — the Kind in `GET_ROUTES`/`DESCRIBE_ROUTES`/`DELETE_ROUTES` with its arms, or a
   reason in `no_verb_reason`.
8. `drift.rs` — `DOC_SCOPED`, only if `actual()` genuinely needs the documents.
9. If namespaced: an entry in `NAMESPACE_SOURCES` (`complete.rs`).
10. Every new user-facing string in English in the code and translated in
    `bins/delonix-runtime-bin/data/pt.po`; a new error code in the `DX-CDNN` dictionary
    (`crates/foundation/delonix-model/src/codes.rs`) also needs its PT text
    (`every_dictionary_text_has_a_portuguese_translation`). Run `python3 scripts/lang_ratchet.py`.
11. `delonix manifest schema > docs/schema/v1/delonix.json` with the tree's binary, then
    `python3 docs/gen.py <that binary>` so the user site's Kinds page follows.
12. `cargo test -p delonix-runtime-bin -p delonix-stack` — the tests named above are what catch a
    skipped step, not a reviewer reading the diff by eye.

A Kind that is `Sugar`/`Aggregate` (rewritten or expanded at load, like `Workload` or `Stack`)
skips most of this: `um_kind_que_baixa_para_outro_nao_pertence_ao_ciclo_do_stack` requires
`in_stack: false` and `converges: false` for those forms, and the work instead lives in
`manifest::load`, where the rewrite happens.

---

**Next:** [Contribution workflow](contributing-workflow.md) — worktrees, version alignment, the language rule, when to write an ADR, and how to send the change.
