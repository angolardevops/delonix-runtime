# ADR-0011: Namespace scoping for internal DNS, and per-namespace names

- **Status:** **Accepted** (2026-08-11)
- **Date:** 2026-08-11
- **Deciders:** Walter (owner)
- **Related:** `crates/delonix-net/src/infra.rs` (`dns_*`),
  `crates/delonix-runtime-bin/src/cmd/{container,util,stack}.rs`,
  `docs/discovery/48_REVISAO_REDE_DNS.md` (§A3, §A6, §A7, §A8, §A9),
  ADR-0003 (capability model — same "the node enforces locally" reasoning)

## Context

The network review measured a hole with no equivalent on the dataplane side: the
firewall isolates namespaces correctly in both directions, and the **name plane
isolates nothing**.

```
client(teamA) → ping   webb(teamB)                     → 100% packet loss   (correct)
client(teamA) → lookup webb                            → 10.250.198.79      (leak)
client(teamA) → lookup webb.teamB.delonix.internal     → 10.250.198.79      (leak)
```

A tenant enumerates the existence and the exact address of every workload of
every other tenant. `dns_server_main` **has** the client's address — it comes
out of `recv_from` — and drops it on the floor; `handle_dns(&q)` never receives
it.

Three findings hang off the same root and are decided here together:

- **A6** — container names are GLOBAL, not per-namespace: `--name web` in
  `teamA` refuses `web` in `teamB`. A namespace that is not a name space
  contradicts its own name, and it means the scoping decided below could never
  actually be exercised: two tenants cannot both own `db`.
- **A7** — a `Stack` is not an isolation boundary; two stacks with no declared
  namespace land in `default` and reach each other.
- **A9** — `default` is open and asymmetric, so the out-of-the-box state is the
  least safe one.

## Decision

### 1. The DNS answers what the dataplane would let through

The resolver takes the client's source address and answers **only what that
client could actually reach**. Not a second, parallel policy — a mirror of the
one that already exists:

| target | resolves? | why |
|---|---|---|
| same namespace | yes | the dataplane accepts (`@dlxns_<ns>`) |
| namespace `default` | yes | reachable from any namespace, by design |
| another namespace | **no → NXDOMAIN** | the dataplane drops it |
| another namespace, with an explicit inbound allow covering the client | yes | `kind: Dependency` opened it on purpose |

The last row is what keeps `kind: Dependency` usable. A dependency crosses the
namespace boundary in one direction, and a resolver that refused the name would
leave the feature working by IP only — the exact "accepted and then ignored"
shape this repo keeps having to remove.

**Deriving the answer from the firewall rules that are already persisted** is
the point. A separate allowlist for DNS would be a second source of truth about
reachability, and the two would drift the first time someone changed one.

**Rejected alternative — resolve everything, let the firewall drop it.** That is
today's behaviour, and it is what makes the leak: the address is the secret. It
also produces the worst possible symptom, a name that resolves and a connection
that hangs.

### 2. Unknown clients are treated as `default`, and the index is refreshed first

A query whose source is not in the index (the gateway, a VM the record does not
carry an address for, a container created inside the 2s index TTL) is scoped to
`default` — never given the old unrestricted behaviour, which is the leak.

Because "not in the index" is also what a **just-started container** looks like,
and the first thing a workload does is resolve, the resolver **forces one index
rebuild** before deciding for an unknown client (rate-limited to avoid turning
DNS traffic into a filesystem scan). Without that, the isolation would show up
as start-up flakiness, and flakiness gets features turned off.

### 3. Names are unique per (namespace, name)

The uniqueness check moves from the name alone to the pair. `util::find` — the
single resolver every container verb goes through — accepts `namespace/name`,
and when a bare name matches in several namespaces it **refuses and names them**
rather than picking one. That is `kubectl`'s behaviour and, more importantly,
picking one silently is how a `stop` or an `rm` hits another tenant's workload.

Not breaking in practice: a bare name that is unique on the node keeps working
exactly as before, which is every node that does not use namespaces yet.

### 4. A `Stack` does NOT derive a namespace automatically

Tempting, and rejected on evidence. Recursos of an existing stack live in
`default`; deriving `namespace: <stack>` on the next `apply` would make the
reconciler see nothing of its own in the new namespace and **create a second
copy of the whole stack**, leaving the running one orphaned and unmanaged. A
safety change whose failure mode is duplicating production is not a safety
change.

Instead, `stack apply` **warns** when a stack declares no namespace, saying what
it means (shared namespace, reachable by everything else in `default`) and how
to fix it.

**The scaffold is deliberately left alone**, and that is a correction to this
ADR's first draft, which said it would emit `namespace: <stack-name>`. Checked
before writing it: `stack init` scaffolds `network: host`, and a container with
no address on the SDN has no namespace isolation to speak of — the firewall
chains are keyed by address. Writing a namespace there would have produced a
manifest that *reads* isolated and is not, which is worse than the warning.
Making the scaffold isolated by default means changing its network model, and
that is its own decision, not a side effect of this one.

### 5. `default` stays open — deliberately, and it is not the leak

`default` remains reachable from every namespace. It is what "default" means in
this engine, the pre-namespace behaviour it exists to preserve, and flipping it
would break every existing node in a way that presents as "the network randomly
stopped working".

This is safe to keep **because the scoping above is what closes the leak**: a
`teamA` client cannot resolve `teamB` regardless of what `default` does. The
open namespace is a shared space you opt into by not naming one — not a hole in
the boundary between two tenants who did name theirs.

### 6. A pod resolves by its own name

`nslookup p1` returned SERVFAIL: only members (`p1-a`) resolved. A pod is the
unit that owns the address — every member shares it — so the pod name maps to
the pod's address, from the label the creator already writes.

## Consequences

- A tenant can no longer enumerate or address another tenant's workloads by
  name. `.delonix.internal` never leaves the node, in any form.
- Two teams can finally both own `db`, which is what makes namespaces worth
  declaring.
- **Breaking, narrowly**: a manifest or script that resolves a name across
  namespaces without a `Dependency` stops resolving. That is the fix, and it
  fails closed with NXDOMAIN rather than silently.
- The DNS index now carries namespace and inbound-allow data per entry, and an
  address→namespace map. Same one-rebuild-per-TTL cost as before.
- **Not addressed here**: cross-namespace resolution driven by anything other
  than a container-level inbound allow (a `FirewallPolicy` written at network
  level, for instance) is not consulted. Named so the next reader does not read
  its absence as an oversight.
