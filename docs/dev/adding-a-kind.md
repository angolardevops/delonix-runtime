# Adding a Kind

**Before you read:** [Coding conventions](coding-conventions.md), [Architecture](architecture.md) and [The crates](crates.md#delonix-stack) — this page assumes you know what `delonix-stack` owns and why planning is pure.

A Kind is a declarative resource this engine understands — `Container`, `Volume`, `Service`,
and so on (`delonix api-resources` lists them all). Adding one touches more than a single
`match` arm: the table that describes what the Kind IS, the code that applies it, and the
reconciler wiring that lets `stack plan`/`apply` treat it like every other Kind. This page
walks through that in order, with a real Kind — **`Service`** (ADR-0032) — as the worked
example throughout, because it is the most recently added Kind and every file cited below is
read from the tree, not from memory of an older layout.

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
| `converges` | Whether a *changed* field is really applied, versus "ensure present" only. `false` is legitimate — `Secret`'s state is encrypted values a plan will not decrypt to compare — but it needs a reason (see `not_converged_reason` below); a generic excuse fails a test. | A Kind that reports `!` on every plan with a reason that reads as "nobody got to it" when the truth is a property of the resource. |
| `teardown` | Whether `destroy_one` can remove it, so `--prune` and `destroy` can promise to. A test (`so_um_kind_convergente_tem_teardown`) refuses a Kind with `teardown: true` and `converges: false` — promising to prune something the plan cannot even represent as changed. | `--prune` promising removal and `destroy_one` refusing mid-run, after earlier Kinds in the teardown order are already gone. |
| `namespaced` | `Never`, `Always`, or `PerDocument`. Not a `bool` — `Volume` genuinely has three answers: none for a plain volume, real for one with a `share:` block; modelling that as `true` would warn "namespace has no effect" on every ordinary volume, and as `false` it would warn the same, wrongly, on a share whose namespace decides which directory its data lives in. | A namespace warning that contradicts what the Kind's own `apply` does with the field. |
| `presence` | How `stack ls`/`wait` learn whether the resource is there: `Registry` (a store answers yes/no), `Derived` (computed from something else — a Pod is its labelled members), `Declarative` (nothing to read back; the resource is a directive applied to a target, and `presence()` answers `-`, which is *not* "absent"), or `NotObservable` (never reaches `presence()` — it does not survive the load, or is not a local resource). | `NetworkRoute` once had no arm in `presence()` at all and fell into `_ => ("?", "unsupported kind")` — printed by `ls`/`describe`, and read by `wait` as pending forever. |

`Service`'s row reads, in one line: applied by the stack, right after the compute Kinds it
selects; a real path exists (`NetConnectivity`); primary; converges without recreating; can be
torn down; always namespaced; and a real registry backs it.

## The spec type and the schema

A Kind with a typed manifest spec (most of them) needs a `#[derive(Deserialize, Serialize,
JsonSchema)]` struct — `ServiceSpec` for this example, in `bins/delonix-runtime-bin/src/cmd/service.rs`
— and a branch in `TYPED_KINDS` in `bins/delonix-runtime-bin/src/cmd/schema.rs`. That constant feeds
`delonix manifest schema` and `delonix explain <Kind>.<field>`, both generated from the same struct
(ADR-0007) so the published schema cannot drift from what the code actually accepts. Leaving a
Kind out is a real, allowed state — `Storage` and `ShareVolume` have no schema on purpose,
because they are rewritten into `Volume` at load and a second struct would just be a
hand-maintained copy of `Volume`'s own fields — but it has to be *said*: `untyped_hint(kind)`
gives the specific redirection, and `todo_kind_conhecido_tem_schema_ou_dica` (in `schema.rs`)
fails the build if a Kind the table knows about is neither in `TYPED_KINDS` nor has a hint. The
generic message ("no typed schema for X") reads as a manifest bug; the hint says it is a property
of the Kind.

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

If the Kind is namespaced (`namespaced != Namespaced::Never`), it also needs an entry in
`NAMESPACE_SOURCES` (`bins/delonix-runtime-bin/src/cmd/complete.rs`) — either `NsSource::Store(fn)`
reading the Kind's own store, or `NsSource::Via("OtherKind — reason")` when the namespace travels
onto another Kind's record instead (a `Pod`'s namespace is on its member `Container`s, for
example). `every_namespaced_kind_declares_a_source` fails otherwise — the practical effect of
missing this is that shell completion for `--namespace` can never offer a tenant whose only
resource is the new Kind.

## The checklist

For a Kind that behaves like `Service` (primary, converges, has teardown, namespaced):

1. `kinds.rs` — a `pub const` name and a `KindFacts` row.
2. A spec struct with `JsonSchema`, and a branch in `TYPED_KINDS` (`schema.rs`) — or an entry in
   `untyped_hint` explaining why not.
3. `stack.rs` — branches in `desired_of`, `actual_of`, `converge_and_stamp`, `stamp_all`,
   `destroy_one`, `presence`, and a layer in `run_layers` calling the Kind's own `apply(docs)`.
4. `reconcile.rs` — a `hot_fields` entry naming the fields that converge live.
5. If the Kind does not converge or cannot be torn down: a specific sentence in
   `not_converged_reason` / `no_teardown_reason` (`stack.rs`).
6. If namespaced: an entry in `NAMESPACE_SOURCES` (`complete.rs`).
7. `cargo test -p delonix-runtime-bin -p delonix-stack` — the tests named above are what catch a
   skipped step, not a reviewer reading the diff by eye.

A Kind that is `Sugar`/`Aggregate` (rewritten or expanded at load, like `Workload` or `Stack`)
skips most of this: `um_kind_que_baixa_para_outro_nao_pertence_ao_ciclo_do_stack` requires
`in_stack: false` and `converges: false` for those forms, and the work instead lives in
`manifest::load`, where the rewrite happens.

---

**Next:** [Contribution workflow](contributing-workflow.md) — worktrees, version alignment, the language rule, when to write an ADR, and how to send the change.
