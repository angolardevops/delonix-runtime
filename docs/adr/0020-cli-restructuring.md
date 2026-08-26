# ADR-0020: The CLI splits into three surfaces, and the declarative CRUD is written once

- **Status:** Proposed
- **Date:** 2026-08-26
- **Deciders:** Walter Angolar
- **Related:** ADR-0005 (`-o json` is the stable contract), ADR-0007 (generated
  manifest schema), ADR-0019 (stack revision history), `docs/cli-stability.md`,
  `docs/discovery/51_CLI_INVENTARIO.md` (the measured inventory behind this).
- **Baseline:** `origin/main` `a3e7fa1` (v0.63.1) — 263 commands, 233 invocable
  leaves, 28 top-level groups. Reproduce with `scripts/cli-tree.sh --count`.

## Context

The CLI grew by addition. Every Kind that landed brought its own group with a
full CRUD, and the result is measured, not asserted: **`ls` written 12 times,
`apply` 10, `rm` 10, `describe` 10, `prune` 7, `init` 7, `dash` 6**. Nothing
forces the copies to agree.

That is the same defect `cmd/kinds.rs` already fixed on the reconciler side —
six lists that had to agree and drifted, with symptoms invisible because the
wrong answer still looked like a working command. Here it sits on the public
surface, where the user pays for it.

Three other shapes of the same problem:

- **Several doors to one object.** A VM image is reachable through `vm pull`,
  `image vm pull` and `image --vm pull`. A backup through `backup` or
  `system backup`, restored through `restore` or `system restore`.
- **A flag that switches domain.** `image --vm` swaps the whole store. This repo
  already catalogued that trap elsewhere; here a global flag decides which
  engine the command talks to.
- **Plumbing beside product.** `net netns` (9 leaves) sits at the same distance
  from the user as `container run`.

## Decision

The public tree represents **three explicit surfaces**:

```
declarative      apply · plan · diff · get · describe · delete · wait
                 api-resources · explain · manifest
day-2            pod · vm · image · volume · network · cluster · secret ·
                 backup · system · config · dashboard
compatibility    container · compose · serve
```

Four points this decision fixes:

1. **The declarative CRUD is written once.** No Kind gets its own group for
   `create`/`ls`/`rm`/`apply`/`describe`. A day-2 group exists only for what
   does **not** fit a generic CRUD — `vm console`, `pod port-forward`,
   `image build`, `volume snapshot`.
2. **`container` is the adoption bridge and stays isolated.** It keeps the verbs
   Docker and Podman taught, and does not reappear as a declarative Kind.
3. **A domain is never chosen by a flag.** `image --vm` gives way to
   `image … --type container|virtual-machine`, required whenever it cannot be
   inferred unambiguously.
4. **Plumbing leaves the public tree.** `net netns` becomes a hidden subcommand,
   as `ingress-proxy` already is.

## Consequences

### What improves

A new Kind costs **one row in the resource table** instead of a group of seven
commands. `api-resources`, the schema, the parser, completion and the reconciler
all read the same registry — the discipline `cmd/kinds.rs` already proved.

### What breaks, and this is where the ADR has to be honest

The specification says the new tree prevails over the old one. It does — but
`docs/cli-stability.md` exists to say what does not break, and **four** of these
changes break written promises. Either that document is revised in the same
commit, or it starts lying.

| # | published promise | what the restructuring does | recommendation |
|---|---|---|---|
| 1 | top-level shortcuts (`ps`/`run`/`exec`/`logs`/`rm`/`images`) are **stable** | removed (§3.4) | accept as a **major**, not as tidying |
| 2 | `delonix build` is **stable** | becomes `image build --type container` | same |
| 3 | exit codes `3`/`4`/`5`, with a published bash example | replaced by `64`–`77` | **DECIDED: keep `3`/`4`/`5`**, only ADD new classes |
| 4 | manifest schema — "stable, and it is what matters most" | `kind: Container` disappears; `v1` → `v1alpha1` | **DECIDED: a step down, not a clean cut** |

Point 3 is the one that breaks silently. `4` does not become invalid — it comes
to mean something else, and the `case $? in 4)` the published documentation
recommends stops matching, falling through to `*)`. Adding `69`/`75`/`77`/`124`
for classes that do not exist today meets the goal of §19 without invalidating
anything.

Point 4 is the most expensive. The promise is explicit: "a field is never
removed" and "`apiVersion: delonix.io/v1` only changes with a `v2`, and a `v2`
does not ship without `v1` still being accepted". `Container` is one of the four
Kinds the promise covers, and `compute.delonix.io/v1alpha1` is not a `v2` — it
is a step **down** in advertised maturity, in a format people keep in git and
point at with `$schema` in their editor.

**And the repo already has the right answer written**, about the three Kinds
that were merged earlier:

> the old names still load, with a deprecation warning — the "clean cut" rule
> applies to commands, and a manifest in git deserves a step rather than an
> error.

That is the distinction §3.4 of the specification does not draw, and this ADR
adopts it: **clean cut for commands, a step down for manifests.** `delonix.io/v1`
keeps loading and lowers to the new Kinds in `load`, through the
`Form::Deprecated` mechanism `cmd/kinds.rs` already models and that
`Egress`→`FirewallPolicy` and `Storage`→`Volume` already use. `kind: Container`
lowers to a one-container `kind: Pod` — which is literally what §3.3 says it is.

### Decisions taken on the two conflicts that fork the work

Both were the product owner's call, taken 2026-08-26, and both keep a published
promise rather than spend it:

**Exit codes — `3`/`4`/`5` stay.** The v0.49.0 table is unchanged, including the
`case $? in 4)` the documentation recommends. Phase CLI-1 only ADDS classes that
have no code today: `69` capability/provider unavailable, `75` temporary or
retryable, `77` permission denied, `124` timeout. `2` keeps its double duty
(clap usage error, and "there are changes" under `plan --detailed-exitcode`), so
the parser must be configured so a usage error cannot be mistaken for a plan
with changes — which is what §19 asked for in the first place.

**Manifests — `delonix.io/v1` keeps loading.** It lowers to the new Kinds in
`load`, through `Form::Deprecated`, with a deprecation warning. `kind: Container`
lowers to a one-container `kind: Pod`. `Vm` stays accepted as a spelling of
`VirtualMachine`. The clean cut applies to commands only, on the v0.30.0
precedent; a manifest in git gets a step.

This means `scripts/schema-diff.sh` must keep passing across the restructuring:
no field of `Container`, `Pod`, `Volume` or `Network` is removed — `Container`'s
move to a lowering path, and the gate stays the check that this ADR did not
quietly spend the promise.

### The dependency that orders the work

Phase CLI-2 **cannot start** before the 12 Kinds. Measured: `origin/main` serves
`delonix.io/v1` with 15 Kinds and has no `Pod`, `VirtualMachine`, `Service`,
`Gateway`, `NetworkPolicy` or `KubernetesCluster`. Building `get pods` against a
Kind that does not exist is writing a command that cannot have a caller — the
dead-code pattern this repo has already deleted four times
(`publish_port_allow`, `Net`, `mount_live`, `set_net_rate`).

Phase CLI-1 (ResourceRef, contexts, output, error envelope, exit codes,
cancellation) is independent of the Kinds and is what can start today.

## Alternatives considered

**Keep the groups and deduplicate internally.** Rejected: the cost is not in the
implementation — it is in the surface a user has to learn, and in the promise
that ten `apply` commands do the same thing. Deduplicating internally leaves the
ten doors and the obligation to keep them agreeing.

**Backwards-compatible aliases for the old commands.** Rejected for commands, on
the v0.30.0 precedent: a script invoking the old form should fail with
"unrecognized subcommand", not keep working through a path nobody tests.
**Accepted for manifests**, for the reason above.

**Adopt `64`–`77` wholesale.** Rejected as the default; available if the product
owner wants the major, with its own migration note.

## Guard-rails this decision does not touch

- **Daemonless by default** — no new command introduces a resident process.
- **Rootless-first** — `vm bridge` stays the single declared exception, still
  requiring root and still EXPERIMENTAL.
- **The PaaS boundary** — no notion of tenant, licence or billing enters the
  CLI. `config set-context` stores an endpoint and a namespace, not a customer
  identity.
