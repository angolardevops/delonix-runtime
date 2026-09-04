# ADR-0030: No further collapse of `net ingress`/`net egress` — measured, not assumed

## Status

Accepted.

## Context

`ADR-0029` (decision 1) unified how `net ingress`/`net egress allow`/`deny` persist their
rules — routing them through the same `origin` bookkeeping `kind: NetworkAccessRule` uses —
but explicitly left the actual CLI-surface question open: "whether removing 17 imperative
leaves in favor of authoring `NetworkAccessRule` manifests is a net UX improvement needs its
own measurement, not an assumption that 'declarative exists now, so imperative should go.'"

This ADR is that measurement, done leaf by leaf against the current code
(`crates/delonix-runtime-bin/src/cmd/firewall.rs`), not against the plan document's
seven-week-old estimate.

## The measurement

**The current leaf count is still 17**, split `IngressCmd` (`allow`/`deny`/`policy`/
`publish`/`unpublish`/`ls`/`rm`/`clear`, 8) and `EgressCmd` (`allow`/`deny`/`policy`/`net`/
`host`/`ls`/`rm`/`show`/`clear`, 9). `l4guard`'s 3 leaves stay excluded — a different
mechanism entirely, per `ADR-0028`. Removing the 3 read-only leaves (`ingress ls`/
`egress ls`/`egress show`, already covered by the `get`/`describe` generics from B1) leaves a
**write-verb surface of 14**.

Of those 14, sorted by what a manifest can actually express today:

- **`allow`/`deny` (4 leaves)** are the ONLY verbs `kind: NetworkAccessRule` was ever built
  for — a direct 1:1 mapping (`examples/network-access-rule.yaml`).
- **`policy` (2) and `egress net`/`egress host` (2)** already had a manifest equivalent
  *before* `NetworkAccessRule` existed, via `kind: FirewallPolicy`/`NetworkPolicy`
  (`defaultPolicy`, `scope: network`, `allowCidrs`, `fqdnAllowlist`). `NetworkAccessRule`
  adds nothing here — it is a different Kind's job (whole-direction replace, not
  incremental), and conflating the two would repeat exactly the "two grains, one Kind"
  mistake `ADR-0024` already flagged for `FirewallPolicy` itself.
- **`publish`/`unpublish` (2)** are permanently excluded per `ADR-0029` decision 2 — a DNAT
  port mapping has no allow/deny axis to fit `NetworkAccessRuleSpec`.
- **`rm`/`clear` (4)** have no manifest equivalent at all, and inventing one (a
  "declarative removal") would be a category error — removal is what NOT declaring a
  document already means.

**So of the "17 imperative leaves" the plan's original estimate counted, only 4 (`allow`/
`deny`) were ever actual collapse candidates.** The other 13 were either already reachable
declaratively through a different Kind, structurally incompatible with `NetworkAccessRule`'s
grain, or a removal verb with no declarative shape to take.

**What the codebase's own usage says.** `scripts/e2e.sh` invokes `net ingress`/`net egress`
8 times and `kind: NetworkAccessRule` zero — the only place that Kind appears is its own
dedicated example, written to demonstrate the incremental-accumulation mechanism, not as a
replacement for the imperative form in a real scenario. `docs/gitops.md` takes no position on
imperative-vs-declarative for firewall rules specifically; it is a generic plan/apply/drift
story for any manifest.

**Concrete cost of the trade, for the common case:**

```
delonix net ingress allow web 8080 --from 10.0.0.0/24
```

against a ~10-line `NetworkAccessRule` document plus a separate `delonix apply -f rule.yaml`
(or `stack apply`) invocation, plus a file to create, track, and version. For `publish`,
there is no manifest form to compare against at all.

## Decision

**No further collapse.** `net ingress`/`net egress`'s CLI surface stays exactly as it is,
permanently — this is not a deferral, it is the closed answer to the question `ADR-0029` left
open. Every other resource this engine has (`container run` vs. `kind: Container`, `vm
create` vs. `kind: Vm`, `volume create` vs. `kind: Volume`) keeps its imperative CLI form and
its declarative Kind side by side forever; firewall rules follow the same pattern instead of
being singled out for a manifest-only future.

The one genuinely open question the measurement surfaced is **not** a leaf-count question:
`delete networkaccessrules <origin>` already works, generically, on the synthetic `cli:...`
origin `ADR-0029` gives an imperative rule — but it is not documented anywhere as the
supported way to remove a specific CLI-added rule by identity, distinct from `net ingress
rm`'s match-based removal. Whether that is worth documenting is a small follow-up, not a
restructuring decision, and is not resolved here.

## Consequences

- `docs/discovery/52_CLI_PLANO_MIGRACAO.md`'s B4 section's "re-measure once incremental
  NetworkPolicy exists" now has its answer: measured, and the answer is no.
- Nothing in `firewall.rs`/`network_access_rule.rs` changes as a result of this ADR — it
  ratifies the status quo `ADR-0029` already established, closing the open question rather
  than reopening any code.
- The CLI restructuring plan's B4 block is now fully closed: both open design questions
  (`ADR-0029`) and this measurement question have written, accepted answers.

## Not done here, and why

- **Documenting `delete networkaccessrules <cli-origin>` as a supported removal path** — a
  small, separate piece of work (naming/discoverability), not a restructuring decision.
- **Reopening `l4guard`'s exclusion from `NetworkAccessRule`** — `ADR-0028` already settled
  this as a different mechanism; nothing measured here bears on it.
