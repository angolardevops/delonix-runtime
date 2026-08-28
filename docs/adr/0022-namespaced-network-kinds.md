# ADR-0022: Network Kinds stay node-scoped; the collision is a NAME collision, and it gets a qualified form

**Status: Accepted (2026-08-25).** Nothing here is implemented yet. It exists so the first line of
code is written against a decided boundary rather than discovered halfway.

## Decision taken

**Do not make the network Kinds namespaced objects.** The isolation they were suspected of missing
is already enforced, one layer down, in nftables — and it was measured, not assumed. What they are
actually missing is a way for two tenants to want the same NAME, which is a naming problem with a
naming answer:

| | Decision |
|---|---|
| `Network`, `NetworkRoute`, `Ingress`, `FirewallPolicy`, `HTTPRoute`, `Tunnel`, `Dependency` | stay `Namespaced::Never` |
| a name that two tenants want | gains the **qualified `<namespace>/<name>` form**, the one `Store::load` already speaks for containers |
| `metadata.namespace` on those Kinds | keeps warning, and the warning stops naming a stale list |
| the conflict message | stops naming the OTHER tenant's stack |

The alternative — a namespace field on every network Kind — buys nothing the nftables sets do not
already give, and costs a migration of every registry on disk. That trade is the whole ADR.

## Context — measured on `origin/main` (b465300), 2026-08-25

Everything below was reproduced against the binary in an isolated `DELONIX_ROOT`, not read off the
source. Two of the four contradicted the audit note that opened this ADR, which is why they are
here in the order they were found.

**1. Seven Kinds honor `metadata.namespace`, twelve do not.** From the `KindFacts` table
(`cmd/kinds.rs`), which is the single authority since the six lists were folded into it:

| honors it | `Volume` (per-document), `Vm`, `Container`, `Pod`, `Workload`, `ShareVolume`, `Stack` |
|---|---|
| **does not** | `Secret`, `Network`, `NetworkRoute`, `Image`, `Ingress`, `FirewallPolicy`, `HTTPRoute`, `Tunnel`, `Dependency`, `Storage`, `Egress`, `Cluster` |

So every network Kind is in the second row. That much the audit had right.

**2. It is NOT an isolation hole, and this is the fact that decided the ADR.** Cross-namespace
isolation does not live in the resource records at all — it lives in the per-workload nftables
chain, keyed on the `@dlxall` / `@dlxns_<hash>` sets that `do_attach` maintains. Two containers on
the SAME network in different namespaces are already blocked, and that has been validated live
since v0.40.0 for containers, pods and VMs alike. A namespace field on `kind: Network` would not
add one rule to that.

**3. What actually breaks is the NAME, and it fails closed.** Measured with two stacks, each
declaring a network called `web`:

```
$ delonix stack plan -f teamB.yaml
  ✗   Network/web  — owned by the stack 'pilha-a'
Summary: 1 in conflict
```

`teamB` cannot take over `teamA`'s network and cannot create its own. That is the correct failure —
ownership by the `delonix.io/stack` label doing its job — but the capability it denies is
legitimate: two tenants each wanting a network called `web` is not a mistake to refuse, it is the
thing namespaces exist to allow.

**4. The refusal leaks a name across the boundary.** `owned by the stack 'pilha-a'` tells `teamB`
what `teamA` called its stack. Small, and real: it is the one place where the current design lets
one tenant learn something about another.

## Why the qualified form and not a namespace field

`Store::load` already resolves `<namespace>/<name>` for containers, refuses a bare name that two
namespaces share, and names both candidates when it does. That machinery exists, is tested, and was
just extended to one more caller (a rule's far end — see the fix that came out of this measurement).
Reusing it means:

- **no migration.** A network's registry entry is a handful of `key=value` lines and the octet;
  adding a namespace column means rewriting every record on every node, and a record that is read
  by both an old and a new binary during an upgrade is exactly where this repo has been bitten
  before (the holder split, the `runtime_dir` move).
- **one resolver, not two.** The alternative gives the engine a second notion of "which one did
  you mean", and two notions of identity is how they start to disagree — the reason `ShareVolume`
  was folded into `Volume`, and the reason two firewall policies for the same (target, direction)
  are refused rather than merged.
- **the bridge name stays derived.** `delonix_net::bridge_name` is the single formula and the
  physical plane is the authority. A namespace in the name changes the hash input; that is fine.
  A namespace in a NEW field would have to be threaded into the formula everywhere it is called,
  and a second formula printing a device that does not exist is a bug this repo has already paid
  for once.

## What this ADR does NOT decide

- **Whether `Secret` should be namespaced.** It is in the same `Never` row and it is a stronger
  case than any network Kind — a secret is exactly the kind of object two tenants both want called
  `db-password`. It is out of scope here because the answer may well be different, and bundling it
  would let a network decision settle a credential question by momentum.
- **The per-network `Egress` grain.** `scope: network` targets a network name; whatever happens to
  network naming happens to it. No separate decision needed.
- **Any change to the isolation dataplane.** Fact 2 is why: there is nothing to change.

## Consequences

- Two tenants can name a network `web` once the qualified form lands. Until then, they cannot, and
  the refusal at least says why.
- The conflict message must stop naming the other stack. It can say that the name is taken without
  saying by whom — the tenant needs to know to pick another name, not who has it.
- The warning on `metadata.namespace` must derive its list from the table. It said "only Container,
  Pod and Vm" while the table had seven, which is a warning about namespaces misinforming someone
  about namespaces.

## How we will know it was right

If someone comes back needing a namespace FIELD on a network Kind, the trigger will be concrete and
this ADR is superseded, not argued with. The two that would do it:

1. a resource whose namespace has to be read WITHOUT parsing its name — an authorization check, a
   quota, a metric label;
2. a second isolation mechanism that keys on the record rather than on the nftables sets.

Neither exists today, and inventing the field before the consumer is the mistake ADR-0010 rejected.
