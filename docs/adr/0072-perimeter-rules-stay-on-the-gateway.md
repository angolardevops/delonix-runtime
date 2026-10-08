# ADR-0072: the perimeter filter stays on `NetworkGateway`, and the ADR-0070 row closes with the reason

- **Status:** Accepted (2026-10-08, by the owner)
- **Date:** 2026-10-06
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0070's table (the `NetworkGateway` row), ADR-0059 D1.5
  (`crates/contexts/delonix-networking/src/ownership.rs`), ADR-0071 (the provider
  block), item 2.3 of `docs/discovery/66_CONTINUITY_PLAN.md`

## Context

ADR-0070's table has a row saying the perimeter rules of a `NetworkGateway`
«belong to `NetworkPolicy` with a `provider` scope», and names what blocks the
move: «a policy has no perimeter scope yet; the OPNsense ownership marks must
carry over». The goal is vocabulary: one Kind for policy, whatever enforces it.

Measured on 2026-10-06, against `main`:

**The scope half is small.** `apply_fw_doc`
(`bins/delonix-runtime-bin/src/cmd/firewall.rs`) validates the scope against a
closed list — `container | network | vm | systemcontainer` — and refuses a typo
rather than falling into the container path. A fifth value is one arm. The rule
shape is already shared: `GatewayPolicyRuleSpec`'s doc-comment says it is «the
`NetworkPolicy` rule shape, plus the two fields a perimeter appliance holds and
the node's chain does not» (`log`, `stateful`), and both descend through one
`PolicyIr` (ADR-0059 F3). The one appliance-only field on the policy itself,
`sequence`, has a home under ADR-0071: `provider.spec`.

**The ownership half is not small, and it is the reason this ADR exists.** A
remote object's owner is an `OwnerMark`: an opaque token the engine generates
**once per declaring record** — one `kind: NetworkGateway`, one
`kind: NetworkZone` — and persists before its first remote write. On OPNsense it
is a firewall category named `delonix-owner:<token>` attached to every alias and
rule. The module states two properties as invariants: it is **immutable** («the
token of a record never changes, and nothing in this engine rewrites the mark on
an object») and **distinguishable per record**, so «two stacks, or two engines on
two hosts sharing one appliance, never read each other's objects as their own».
That property is not decoration: audit 62 §3/§6 P1 records an operator's
hand-made rule being adopted by name and then deleted, which is why identity
stopped being the name.

A `NetworkPolicy` document is a **different record**. Moving the rules there
gives them a new token, and the appliance keeps the old one, so the first apply
after the move:

1. creates a second copy of every rule, under the new token; and
2. orphans the first copy — owned by a record that no longer declares it.

On a perimeter, that is duplicated filter rules in `sequence` order on a live
appliance. None of the three ways out is cheap: re-stamping breaks the
immutability invariant on the object the invariant protects; keying the token by
the targeted gateway instead of by the declaring record breaks
per-record distinguishability, which is what keeps two stacks apart; and asking
the operator to delete and re-apply is a destructive step on a perimeter.

And the goal ADR-0070 D1 states — provider specifics inline, vendor-keyed — was
already delivered for this Kind by ADR-0071 (#692): `NetworkGateway.spec.provider`
takes the `{ type }` block, and the appliance-only knobs live per item
(`nat[].interface`, `policies[].sequence`). What the row still asks for is
vocabulary unification, not the inline-provider rule.

## Decision

**The perimeter filter stays on `kind: NetworkGateway`.** The `NetworkGateway`
row of ADR-0070's table closes as **satisfied by ADR-0071 for its D1 part, and
rejected for the Kind move**, with the measured reason recorded: the move buys
one word of vocabulary and costs either an ownership invariant or a destructive
step on a live perimeter.

Two things follow, and they are the whole of the work:

1. `docs/adr/0070-…md`'s table row is rewritten to say this, so the next reader
   does not start the move believing only a scope arm is missing.
2. `AGENTS.md` gains the rule in one line: a policy that the NODE enforces is a
   `NetworkPolicy`; a policy an APPLIANCE enforces is declared on the
   `NetworkGateway` that owns the appliance, because ownership of a remote object
   belongs to the record that declared it.

## Alternatives considered

- **Add `scope: gateway` and migrate the marks by re-stamping.** Rejected: it
  rewrites the mark on the object, which the ownership module names as the thing
  it never does. The invariant exists because identity-by-name already caused an
  operator's rule to be adopted and deleted.
- **Key the `OwnerMark` by the targeted gateway, not by the declaring record.**
  Rejected: per-record distinguishability is what keeps two stacks (or two
  engines sharing one appliance) from reading each other's objects. Trading it
  for vocabulary is the wrong side of the trade.
- **Add `scope: gateway` and accept the duplication**, documenting a delete-and-
  re-apply migration. Rejected: a destructive step on a live perimeter, for a
  rename.
- **Add `scope: gateway` as a second spelling that lowers into the gateway
  document at load**, the way `Dependency` folds into `NetworkPolicy`. Not
  rejected on principle, and it is the cheapest path if the owner wants the
  vocabulary: the declaring record stays the gateway, so the mark is untouched.
  It is not decided here because it adds a second way to write the same thing
  with no measured demand for it — and this repo has paid for two spellings of
  one resource before.
- **Do nothing and leave the row open.** Rejected: an open row reads as «someone
  will get to it», and the next reader would re-derive the ownership cost from
  scratch. A closed row with the reason is the honest record.

## Consequences

- **Easier:** the ownership model keeps one rule with no exception — the record
  that declared a remote object owns it, and the mark is never rewritten.
- **Harder:** the vocabulary stays split. Someone writing policy has to know
  that a perimeter policy is declared on the gateway. That is the cost, and
  `AGENTS.md` states it rather than leaving it to be discovered.
- **Debt assumed:** none new. The lowering through `PolicyIr` already means the
  two paths cannot drift in what a rule MEANS; only where it is declared differs.
- **If the owner wants the vocabulary anyway**, the fourth alternative (a second
  spelling lowered at load) is the one to take, and it needs its own ADR — the
  cost there is a Kind that does not survive the load, which this repo already
  does for `Dependency` and `NetworkZone`.
