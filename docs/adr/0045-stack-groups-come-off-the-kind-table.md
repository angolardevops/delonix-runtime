# ADR-0045: The groups of a `kind: Stack` come off the Kind table, and a Stack has no state of its own

- **Status:** Accepted (2026-09-19)
- **Date:** 2026-09-19
- **Deciders:** Walter (owner)
- **Builds on, does not reopen:** the Kind table `delonix_stack::kinds` (one row per Kind, the
  classifier that ended the six hand-kept lists); ADR-0011 §4 (the namespace of a Stack is never
  derived from its name); the «no state file» decision of the IaC section of `AGENTS.md`.

## Context

`kind: Stack` is the one document a person copies. It expanded through a hand-written `StackSpec`
struct with one field per group and a hand-written list in `expand_stack`, next to a Kind table
that already knew which Kinds the stack applies. The two drifted, and the drift was measured
(2026-09-19, `origin/main`):

- **Four Kinds `stack apply` applies could not be written inside a Stack** — `NetworkRoute`,
  `NetworkAccessRule`, `Service`, `App`. A manifest had to leave the Stack for them.
- **The Gateway group was still called `tunnels:`**, the name of the Kind before v0.64.0.
- **`storage:`/`shareVolumes:`/`egress:` were accepted** while their children were hard-removed
  Kinds, so a Stack that used one failed whole (PR #430 removed them).
- **`Workload` had no group**, although `Dependency` did — the two Kinds that lower into another
  Kind were treated differently for no reason anyone wrote down.
- **The schema typed the group names and nothing inside them**: `imgae:` in a container of a Stack
  validated clean and showed only at apply.
- **A child could not carry `labels:`**, which a `Service` inside a Stack needs to select it.
- The header of `examples/stack.yaml` named Kinds that no longer exist.

## Decision

**1. One column governs everything.** `KindFacts.stack_group` holds the key a Stack uses for the
Kind, or `""`. `expand_stack`, the unknown-field warning, the published schema, and the example are
read from it (`stack_groups()`); there is no second list. Tests fail when a Kind the stack applies
has no group, when a group has no sample that survives `load`, when two Kinds share a key, and when
the published example does not name a group.

**2. Groups are named after the plural of the Kind** (`networkRoutes`, `networkAccessRules`,
`services`, `apps`, `gateways`, `workloads`). Three predate the rename of their Kind and keep the key
manifests already write: `vms`, `ingress`, `firewallPolicies`. Renaming them would break every
published Stack for a spelling nobody asked to change. `tunnels:` is the old spelling of `gateways:`
and loads **in silence** (a rename does not change what the document means — the rule
`KIND_ALIASES` follows), and both may appear at once: they add up.

**3. `Workload` and `Dependency` are groups; they are still not applied as themselves.** Both are
things a person writes in a Stack, and both are lowered at load (a `Workload` into the Kind its
`type` names; a `Dependency` into the `NetworkPolicy` that carries its allow, merged by target across
the whole list). A Stack's children never pass through the top-level loop, so the `Workload` child is
lowered explicitly on the way out — a `workloads:` group that left a `kind: Workload` no handler
claims would be the same silent drop the `Egress` lowering once was. `in_stack` keeps its meaning
(«applied by `stack apply` as itself»); `stack_group` is the new, separate question («can be written
inside a Stack»).

**4. `Stack` and `KubernetesCluster` are not groups, and say why.** A Stack does not nest (the
reconciler identifies a resource by `(kind, name)`; two stacks reaching one child through a third is
a second writer), and a `KubernetesCluster` is a remote procedure over SSH, not a resource of the
node, so grouping it would promise an apply that never happens. The reason is a function
(`stack_group_absent_reason`) that a test requires for exactly the Kinds without a group, and
`stack plan --fields` prints it — the discipline `not_converged_reason` and `no_teardown_reason`
already follow.

**5. A Stack has no state of its own, on purpose.** It is `NotObservable`: it dissolves at load into
its children, and `stack ls`/`describe`/`destroy` derive from those children and from the label
`delonix.io/stack`. There is no `get stacks`, no `delete stacks`, and a whole group removed from a
manifest is seen by `--prune`, not by a Stack record. This is the same decision as «no state file»:
a record of the Stack would be a second source of truth beside the resources, and the day they
disagree the record wins in the reader's head and loses in the kernel. Reopening it needs a
consumer that cannot be served by the label, named in a successor ADR — not a verb.

**6. Cross-reference validation runs on the expanded list**, so a reference between two groups is
checked wherever it is written. Two additions: a `Pod` naming a `network:` that is neither declared
nor existing is refused (the field is honoured since v0.47.0, and was unchecked). A `Service` whose
selector matches nothing is **not** refused: it selects by label against workloads that are alive
when the DNS answers (ADR-0032), so the same manifest is valid when the backends come from another
stack or a later apply, and blocking it would refuse a legitimate ordering. `apply` already warns
about an empty selector, the case that is certainly a mistake. A test pins that decision.

**7. The schema types the inside of every group.** Each group's items are the same schema the Kind
has at the top level (compared for equality in a test, not copied), and the items are closed
(`additionalProperties: false`), so `imgae:` in a child and `nmae:` next to `name:` are both caught in
the editor. The definition is built from `stack_groups()`, and the strictness of the children is also
registered when only the Stack is printed (`schema print --kind Stack`, `explain Stack…`), the one
path that never visits them. A child gains `labels:` and `annotations:` next to `name:`.

## Consequences

- A Kind added to the table has to say where it goes in a Stack, or why it cannot — the omission
  that hid four Kinds is now a failing test.
- The unknown-field warning and the schema list `tunnels` next to the groups; `stack_spec_fields()`
  is a function, not a constant.
- The published schema grows (every group carries the Kind's whole spec): `docs/schema/v1/delonix.json`
  went from 94 476 to 106 404 bytes, +12%. It is a document editors fetch once, not a per-apply cost.

## Not decided here

- A typed check that a `Service` selector matches a workload **declared in the same Stack** when
  the Stack declares any. It would be advisory only (see 6) and needs a warning channel that
  `validate_graph_with`, being pure, does not have.
