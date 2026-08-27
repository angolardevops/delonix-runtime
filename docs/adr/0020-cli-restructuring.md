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

**Two of the four new codes, not four.** *(Superseded by the correction
below: three of the four shipped, plus `74`. Kept as written — what the
decision was at the time is the point of the record.)* §19 proposed
`69`/`75`/`77`/`124`.
`69` (capability unavailable) and `124` (timeout) shipped: both had **real
producers being misclassified** — `stack wait` answered 1 on a timeout, the same
number as a broken apply, on the command whose entire job is to be read by CI;
a missing `wg`/`virt-customize`/`ngrok`/`cloudflared`/`systemd-run` answered 1
too, indistinguishable from a typo in a flag.

`75` (retryable) did **not** ship, and the reason is the one `Error::Conflict`
already documents in its own doc-comment: it sat in the enum with **zero
producers** while `delonix-mgmt` matched on it for a 409 and the real refusals
said `Invalid`. The retrying that exists (`publish_with_retry`) happens inside
the engine and never reaches a caller. Publishing it would be a number that can
never be observed — decoration, the same shape as the digest-pinning that audit
#3 found. It reopens the day something constructs it.

**Correction (2026-08-27): `77` DID ship, and `74` came with it.** This ADR put
`77` in the same bucket as `75` and refused both on one argument — that
permission failures "arrive wrapped as `Error::Io(EACCES)` from inside a syscall
path", so nothing constructs them. The observation was right and the conclusion
was wrong: a code does not need a variant of its own, it needs somewhere honest
to be read from. `for_error` inspects the **kind inside the wrapper**
(`Error::Io(e) if e.kind() == PermissionDenied`), and that is measured rather
than assumed — against a state root with the write bit cleared, both
`volumes create` and `secret create` come back exactly that way. `74`
(`EX_IOERR`) follows from it: carving `77` out of the I/O bucket is what leaves
the rest of that bucket a class worth naming.

Neither spends a published promise, and that is the whole test. Both are carved
out of `1`, which the published table describes as "a failure with no class of
its own", so a script whose last arm is `*) exit 1` keeps matching. `3`/`4`/`5`
are untouched, `case $? in 4)` still means what it meant.

`65` (invalid data) stays out, for a reason of its own rather than the one
above: `Error::Invalid` covers both "your manifest is wrong" (`65` in the
specification) and "your flag is wrong" (`64` in the same one), and it is
constructed in too many places to retag wholesale — **924 across the workspace,
635 of them in the CLI crate**
(`grep -rIn 'Error::Invalid(' --include=*.rs crates/ | wc -l`; the slice that
shipped `74`/`77` quoted 643, which is the CLI crate alone). Mapping the whole
variant would hand ONE number to two classes the specification is careful to
separate. It needs the variant split first.

The table in force after this correction is `1` · `3` · `4` · `5` · `69` · `74`
· `77` · `124`. `cmd/exitcode.rs` is the single place that decides it, and
`docs/cli-stability.md` publishes it — the two new rows land there with the
CLI-2 slice, not with this correction.

**Manifests — `delonix.io/v1` keeps loading.** It lowers to the new Kinds in
`load`, through `Form::Deprecated`, with a deprecation warning. `kind: Container`
lowers to a one-container `kind: Pod`. `Vm` stays accepted as a spelling of
`VirtualMachine`. The clean cut applies to commands only, on the v0.30.0
precedent; a manifest in git gets a step.

**Correction (2026-08-27): `kind: Container` is NOT lowered.** This ADR first
said it would become a one-container `kind: Pod`, taking §3.3 at its word. That
was written without checking what this repo had already measured, and the repo
was right: a Pod always builds a shared netns and its members join it through
the `--pod` re-exec, so lowering would give every declarative container an extra
netns holder and a different network path. The NAME half was solvable —
`pod.rs` honours a member's own name instead of appending `-c0` — but the netns
half is not. That is not a step down in spelling, it is a change of runtime
shape, applied silently to manifests already running.

So `Container` is **announced, not rewritten**: a new `Form::Sunset(Pod)` in the
Kind registry, a `sunset → Pod` in `api-resources`, and one warning per load
naming the count. It keeps its own apply and survives the load. A future major
removes it, once manifests have moved. This meets what §3.3 is FOR — `Pod` is
the canonical way to declare containers — without changing what anybody's
engine does today.

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
