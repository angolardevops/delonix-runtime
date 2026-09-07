# ADR-0032: `kind: Service` — a selector-matched workload SET, load-balanced by DNS, not a new dataplane

## Status

Accepted. Implemented 2026-09-05.

## Context

`ADR-0020` named `Service` as one of the 6 Kinds the CLI restructuring's Phase CLI-2 needed
before it could even start (`Pod`, `VirtualMachine`, `Service`, `Gateway`, `NetworkPolicy`,
`KubernetesCluster`). Five of the six shipped; `Service` never did.
`docs/discovery/52_CLI_PLANO_MIGRACAO.md` §5 still names it as the one Kind blocking the
"12 operable Kinds" count from closing — a design gap, not a CLI-surface cut, and explicitly
out of scope for the restructuring plan itself.

This is new capability, not a corrected estimate. It gets an ADR before any code, per this
repo's own Rule 0 (discover, map, classify, compare, plan — before building).

## What already exists that this can reuse

**A selector mechanism is already designed, just not built.** `ADR-0024` (accepted, still
unimplemented as of this writing — confirmed: no `selector`/`matchLabels` field exists in
`crates/delonix-runtime-bin/src/cmd/firewall.rs`'s `FwDocSpec` today) worked through the hard
parts of "select a SET of workloads by label" for `FirewallPolicy`: `matchLabels` over
`Container.labels` (already a persisted `BTreeMap<String, String>`, nothing new to store), an
empty match warns and succeeds rather than erroring, and matching is NOT retroactive to a
runtime event — the `apply` that runs is the `apply` that governs the current match set.
`Service` needs the exact same selector primitive. Building it once, shared, is the point of
sequencing this after `ADR-0024` rather than inventing a second `matchLabels` implementation
that can drift from the first.

**The internal DNS resolver already re-derives its answers from live state, on a short
TTL, with no daemon.** `delonix-net::infra::build_dns_index`/`dns_index_within` (a `<root>`-
wide index of container/VM names → IPs) is rebuilt from the container/VM stores whenever the
cached copy is older than `DNS_INDEX_TTL` — a lazy, pull-based refresh triggered by the next
query, not a push-based reconciliation loop. This is a materially different (and better)
situation than `FirewallPolicy`'s selector: a `FirewallPolicy` only re-evaluates its selector
when something explicitly re-applies the manifest, so its match can go stale between applies.
A `Service`'s backend set, if plugged into this SAME index-rebuild path, is freshened on
every DNS lookup that outlives the TTL — no daemon, no timer, no new loop, and LESS staleness
than the selector precedent it's borrowing from.

**`dns_resolve_for` (`delonix-net/src/infra.rs`) returns exactly one IP per query today.**
Every container/VM name maps to a single `DnsEntry`. A `Service` name needs to map to a SET.

## Decision

**`kind: Service` selects a set of containers by label and publishes it as MULTIPLE DNS `A`
records under `<service>.<namespace>.delonix.internal` — round-robin resolved, order rotated
per query. No VIP, no L4 dataplane, no new daemon.**

```yaml
apiVersion: delonix.io/v1
kind: Service
metadata:
  name: web
  namespace: teamA
spec:
  selector:
    matchLabels:
      app: web
  port: 8080          # the CONTAINER port every matched workload listens on
```

- **Resolution**: `build_dns_index` gains a pass over every `kind: Service` document's live
  match set (same selector-matching helper `FirewallPolicy` will eventually share), storing
  `Vec<[u8;4]>` under the service's DNS key instead of the single `[u8;4]` a container/VM
  entry stores. `dns_resolve_for` (or a sibling `dns_resolve_multi_for`, kept separate so the
  existing one-IP callers — the container/VM path — do not have to change shape) returns the
  full set, rotated by a counter so consecutive queries do not always favor the same backend.
  A client that resolves once and holds a long-lived TCP connection does not get rebalanced
  mid-connection — the same limitation any DNS-based load balancing has, k8s "headless
  Service" included, and an explicit, named trade-off rather than a silent one.
- **Membership refresh**: identical to any other name in the index — the next query past
  `DNS_INDEX_TTL` re-evaluates the selector against the current container list. A container
  added, relabeled, or removed shows up within one TTL window, no `apply` required to notice
  it (unlike `FirewallPolicy`'s selector, which only updates on the next explicit apply — a
  meaningfully different and better freshness story, inherited for free from reusing the DNS
  index rather than the firewall's own apply-triggered convergence).
- **Ownership/stamping**: `ownable: true`, same as `NetworkAccessRule` — a `Service`
  document has its own name and survives independently; `stack apply --prune` can remove one
  whose document disappears. It does NOT stamp the matched containers' own `STACK_LABEL` —
  same guard `NetworkAccessRule` already needed (a rule/service targeting a container it did
  not create must not be able to hand that container to its own stack on `--prune`).
- **An empty selector match**: applies to nothing, warns loudly
  (`Service/<name>: selector matched no workloads — this service resolves to nothing`), and
  succeeds — identical reasoning to `ADR-0024`: refusing would make the declarative
  create-policies-before-workloads order illegal, and `apply` has no rollback to undo a
  half-applied stack.
- **`spec.port`** names the CONTAINER port every matched workload is expected to listen on
  (mirroring how `net ingress allow`'s port is always the container side, post-DNAT, per the
  documented convention this whole engine already uses) — not a host-side or VIP-side port,
  because there is no VIP. A client resolves the Service name to an IP and connects to
  `spec.port` directly on whichever backend it landed on.

## Why not a real L4 VIP (ClusterIP-style) in v1

A stable virtual IP with round-robin DNAT to a backend set is a real, well-understood
nftables pattern (`numgen random mod N vmap`) and this codebase already uses verdict maps
extensively (`@fwmap`, `@netpair`) for comparable dispatch. It was seriously considered and
set aside for v1, not ruled out forever:

- It is a new dataplane primitive with its own membership-refresh problem to solve
  (`NetworkRoute`'s own history in this codebase — `@netpair` add/remove ordering,
  dataplane-first-registry-last — is the direct precedent for how much care a verdict-map
  membership change needs to get right; a `Service` VIP map inherits the same class of bug
  surface for free by NOT building it).
- The DNS approach needs zero new dataplane code, reuses an index-rebuild path already
  proven correct, and covers the common case (a client that resolves per-request or
  reconnects periodically) without inventing anything.
- Building it now would be designing for a need nobody has named yet — the same anti-pattern
  this repo's own audits keep finding and removing (`publish_port_allow`, dead code with zero
  callers). A concrete workload that needs mid-connection rebalancing (long-lived
  connections, e.g. a database pool) is the trigger to revisit this, not a hypothetical.

## Consequences

- `cmd/kinds.rs`'s registry gains a 19th Kind, `Service`, closing `docs/discovery/
  52_CLI_PLANO_MIGRACAO.md` §5's "12 operable Kinds" count.
- `ADR-0024`'s selector, once built, and `Service`'s selector should share ONE
  `matchLabels`-resolution function — sequencing `Service`'s implementation after (or
  alongside) `FirewallPolicy`'s selector avoids two parallel, divergence-prone
  implementations of the same primitive.
- `delonix-net::infra`'s DNS index format changes shape (single IP → IP or IP-set per entry)
  — every existing caller of `dns_resolve_for` needs to keep working unchanged for
  container/VM entries; only Service entries take the new path.

## Not done here, and why

- **A real L4 VIP/ClusterIP** — deliberately deferred above, not part of v1.
- **`FirewallPolicy`'s own selector (`ADR-0024`)** — still unimplemented; this ADR assumes
  it will exist and share code with `Service`'s, but does not build it.
- **`type: LoadBalancer`/`type: NodePort`/`type: ExternalName`** (Kubernetes Service's other
  types) — no concrete consumer named for any of them yet; `Service` here is the equivalent
  of a k8s "headless Service" only.
- **Health-checking backends before including them in the DNS answer** — v1 returns every
  selector-matched container regardless of its own liveness/readiness; a container that is
  `Running` but not actually accepting connections on `spec.port` is indistinguishable from
  a healthy one here. A concrete need for readiness-gated membership is its own follow-up.
