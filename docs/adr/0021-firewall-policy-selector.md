# ADR-0021: A `FirewallPolicy` gains a label selector, and the default it implies stays OPEN

**Status: Accepted (2026-08-25).** Nothing here is implemented yet. The GO/NO-GO question this ADR
had to answer was not "can we build a selector" — it was "what does a selector that matches nothing
mean", and getting that wrong turns a convenience into an outage.

## Decision taken

| | Decision |
|---|---|
| `spec.target` | **stays**, unchanged, and stays the common case |
| `spec.selector` | **added** — `matchLabels`, over the labels a container already carries |
| the two together | **refused**, like two policies for the same (target, direction) already are |
| a selector matching **zero** workloads | **applies to nothing, warns, and succeeds** — see below |
| a workload created **later** that matches | **not** retroactively governed in v1; the `apply` that creates it is what governs it |
| `namespace:` as a selector | **not** in this ADR — see ADR-0019 |

## Context — measured on `origin/main` (b465300), 2026-08-25

**`spec.target` is a `String`.** One name, resolved to one container (or, with `scope: network`,
to one network). There is no list, no glob, no selector.

**The consequence is arithmetic.** A tenant with twenty containers writes twenty documents. The
twenty-first container is created with **no policy at all** until somebody remembers to write the
twenty-first document — and "default open until someone remembers" is the failure mode that made
Kubernetes NetworkPolicy select on labels rather than on names in the first place.

**The thing to select on already exists.** `Container.labels` is a `BTreeMap<String, String>`,
persisted in the record, settable with `--label KEY=VAL` and from the manifest. Nothing new has to
be stored for a selector to work.

**There is already a grain ladder, and this is the rung that is missing:**

| grain | today |
|---|---|
| one workload → one workload | `kind: Dependency` |
| network → network | `kind: NetworkRoute` |
| one workload | `kind: FirewallPolicy` (`target`) |
| one network's egress | `kind: FirewallPolicy` (`scope: network`) |
| **a SET of workloads** | **nothing** |

## The hard part: what an empty selector match means

This is the decision, and it goes against the instinct.

A selector that matches nothing looks like it should be an error — the user clearly meant
something. But a `FirewallPolicy` is applied by `apply_fw_doc`, which **replaces the whole state of
one direction** on its target. So "matches nothing" has to answer: is that a mistake to refuse, or
a state to converge to?

**It applies to nothing, warns loudly, and succeeds.** Because:

- **it is the normal state of a correct manifest.** A stack that declares its policies before its
  workloads — which is the order `KINDS` applies them in for every other Kind — has a selector
  matching zero at the moment the policy is applied. Refusing would make the declarative order
  illegal.
- **the alternative fails the wrong way.** Refusing means a typo in a label key takes down the
  whole `apply`, including the policies that DID match. `apply` is fail-fast without rollback: the
  ones already applied stay applied, and the stack is left half-governed with an error. That is
  worse than a warning.
- **a firewall that governs nothing is not a firewall that allows everything.** The workloads it
  did not match keep whatever policy they already had. Nothing is opened by a selector missing.

The warning is not optional and it is the whole safety story here: `selector matched no workloads —
this policy governs nothing`. A policy silently governing nothing is the exact shape of the bug
this repo keeps finding (`--network-alias` recorded and never consulted; `slo:` written and
ignored; `create_with_base` with zero callers). It gets a sentence.

## Why not retroactive in v1

A container created after the policy would have to be matched against every stored selector at
attach time — which means the policy documents become a thing the RUNTIME consults, not just a
thing `apply` reads. That is a live-reconciliation loop, and this engine is daemonless by design:
a loop like that is either a daemon or a `systemd` timer, and `delonix-engine`'s own ADR-0010 says
a resident process needs its own decision with evidence behind it.

So v1 is: the `apply` that creates a workload is the `apply` that governs it. That is the same
contract the rest of the declarative surface already has, and it does not smuggle in a daemon.

The cost is stated rather than hidden: `container run` outside a manifest creates a workload the
selector will not reach until the next `stack apply`. The warning above is what makes that
visible.

## Why `matchLabels` only, and no `matchExpressions`

`matchLabels` is an AND of equalities and it is what the overwhelming majority of real policies
use. `matchExpressions` (`In`, `NotIn`, `Exists`, `DoesNotExist`) is a small language, and a small
language in a firewall selector is a place where "I thought this matched" becomes a hole. It can be
added later against a concrete need; it cannot be removed.

## Consequences

- `FwDocSpec` gains one optional field; every existing manifest keeps working byte for byte.
- `target` and `selector` together are refused at `validate_graph`, with both spellings named —
  two answers to "which workloads" is a contradiction, and on a firewall a contradiction is not
  something to resolve by precedence. (Same rule, same reason, as a rule naming both an address
  and a workload.)
- The `--dry-run` round-trip has to preserve `selector`, or a plan will show drift on a document
  nobody touched — the `direction` field already had to be captured for exactly this reason.
- The plan must show WHICH workloads a selector resolved to at plan time. A plan that says
  `FirewallPolicy/p` without saying it will govern four containers is a plan that hides its blast
  radius.

## How we will know it was right

The trigger to revisit is a measured one: if the warning above fires in a normal, correct workflow
often enough that people learn to ignore it, then the empty-match decision is wrong and the
retroactive question has to be reopened — with the daemon question answered first, not by accident.
