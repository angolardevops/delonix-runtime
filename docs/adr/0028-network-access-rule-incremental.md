# ADR-0028: `kind: NetworkAccessRule` — an incremental firewall primitive

## Status

Accepted.

## Context

Block B4 of the CLI restructuring plan (`docs/discovery/52_CLI_PLANO_MIGRACAO.md`)
promised to collapse `net ingress`/`net egress` (17 leaves) into a declarative
Kind, for −41 leaves. Measured twice already (`docs/releases/v0.68.0.md`) and
confirmed a third time in this session: it only ever achieved 4 leaves.

The reason is structural, not an oversight. `net ingress allow`/`deny` and
`net egress allow`/`deny` are **incremental** — each call adds or replaces
exactly one rule, keeping every other rule on the container untouched
(`cmd/firewall.rs::add_rule`, the documented "last rule wins" ufw semantics).
The only existing declarative mechanism for firewall state, `kind:
FirewallPolicy` (also spelled `NetworkPolicy` — both strings alias the same
internal Kind, `cmd::kinds::FIREWALL_POLICY`), does the opposite: it
**replaces the whole state of one direction** on every apply
(`apply_fw_doc`). Two `FirewallPolicy` documents naming the same (target,
direction) are refused outright, precisely because replace semantics cannot
express "these two together" without one silently erasing the other while
both report success — a real bug this repo already found and fixed once.

`ADR-0024` measured this same tension from the other side — "the CLI verbs
and the Kind are not the same grain" — and left `net ingress`/`net egress` as
imperative verbs rather than trying to force them onto `FirewallPolicy`. That
was the right call given what existed. It does not mean the tension is
permanently unresolvable; it means resolving it needs a Kind with different
semantics, not a reinterpretation of `FirewallPolicy`.

The one existing accumulation mechanism, `kind: Dependency`, does not solve
this either: it merges every sibling `Dependency` document into ONE
`FirewallPolicy` document at manifest-**load** time
(`cmd/dependency.rs::lower_dependencies`). That only works because all
sibling documents are visible together in a single `stack apply` pass — the
module's own documentation states plainly that removing a single `Dependency`
does **not** retract its effect independently ("removing the Dependency does
NOT unprotect the `to`"). There has never been a Kind in this codebase that
is applied and removed **independently**, document by document, while still
correctly retracting only its own contribution from a resource shared with
other documents.

## Decision

Add `kind: NetworkAccessRule` **alongside** `FirewallPolicy`, not instead of
it. It expresses exactly one incremental rule per document:

```yaml
apiVersion: networking.delonix.io/v1alpha1
kind: NetworkAccessRule
metadata:
  name: allow-web-from-lan
spec:
  target: web
  direction: ingress   # or egress — both map to the same FwRule.dir the rest of the firewall already uses
  action: allow         # or deny
  proto: tcp
  port: "8080"
  from: "10.0.0.0/24"
```

**The mechanism — a contribution ledger, not a new dataplane primitive.**
`FwRule` gains an `origin: Option<String>` field: the name of the
`NetworkAccessRule` document that wrote it. A document's apply finds the rule
carrying its own `origin` on the target container and replaces just that one
(push if absent), leaving every other rule — from other `NetworkAccessRule`
documents, from imperative `net ingress`/`net egress` commands, or from a
`FirewallPolicy` — untouched. Removing the document retracts only that rule.

This required no change to the dataplane: the holder already rebuilds a
container's whole firewall chain from its full in-memory rule list on every
apply, in one atomic `nft -f` (`delonix-net/src/infra.rs::do_firewall`).
There is no per-rule nft primitive anywhere in this engine to build or
extend — incrementality at the CLI layer has always been a Rust-side list
mutation before a full rebuild. The only thing genuinely missing was
bookkeeping: nothing let an independently-applied-and-removed document find
and retract just its own contribution. `origin` is that bookkeeping.

**One companion fix, in the same change**: `apply_fw_doc`'s direction-wide
replace (`fw.rules.retain(|r| r.dir != dir)`) became
`retain(|r| r.dir != dir || r.origin.is_some())`. Left unfixed, applying a
`FirewallPolicy` on a target that also carries `NetworkAccessRule`-contributed
rules would silently erase them while reporting success — the exact class of
bug `FirewallPolicy`'s own duplicate-target refusal exists to prevent,
recreated across two different Kinds where that check cannot see it.

**No new CLI leaf.** `NetworkAccessRule` is reached the same way
`kind: Dependency` already is — through `delonix apply -f`/`delonix stack
apply`, which dispatch by Kind. This is deliberate: the whole point of this
Kind is to make it *possible* to eventually shrink `net ingress`/`net
egress`'s CLI surface, and adding a competing imperative wrapper for it now
would work against that.

## Consequences

- `FirewallPolicy`/`NetworkPolicy`'s semantics are unchanged. Nothing that
  worked before behaves differently, except that its replace no longer
  clobbers a `NetworkAccessRule`'s contribution.
- `kind: Dependency` is unaffected — it still lowers to `FirewallPolicy` at
  load time, unrelated to this Kind.
- `net ingress`/`net egress`/`net l4guard`'s CLI surface is **not** touched by
  this change. Re-measuring whether any of those 17+ leaves can now
  responsibly collapse onto `NetworkAccessRule` is separate, future work —
  the plan's own §5 already calls for "re-measure B4 once incremental
  NetworkPolicy exists" once this primitive existed to re-measure against.
- `l4guard` (rate limiting) is a different mechanism entirely and is not
  addressed here.
- This does not reopen `ADR-0024`. `ADR-0024`'s finding — CLI verbs and
  `FirewallPolicy` are not the same grain — still holds for `FirewallPolicy`
  specifically; `NetworkAccessRule` is a different Kind built at the grain
  the CLI verbs actually operate at (one rule).

## Not done here, and why

- **Collapsing `net ingress`/`net egress`'s CLI surface.** That's the actual
  B4 leaf-count goal, and it is a separate decision: whether removing 17
  imperative leaves in favor of authoring `NetworkAccessRule` manifests is a
  net UX improvement needs its own measurement, not an assumption that
  "declarative exists now, so imperative should go."
- **Cross-stack ownership disambiguation.** `stamp`/`actual`'s `owner` field
  is still a single container-wide label (`STACK_LABEL`), same as
  `container`/`pod`/`FirewallPolicy`'s own stamping. Two `NetworkAccessRule`
  documents from *different* stacks targeting the same container will
  overwrite each other's ownership record — an accepted, pre-existing
  limitation shared with every other Kind that stamps a container, not new
  here.
- **`egress`'s `scope: network` equivalent.** `NetworkAccessRule` only ever
  targets a container, mirroring `net ingress`/`net egress`'s own per-container
  scope — no per-network incremental policy exists or is proposed here.
