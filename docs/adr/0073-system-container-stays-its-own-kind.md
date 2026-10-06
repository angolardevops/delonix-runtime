# ADR-0073: `SystemContainer` stays its own Kind, and the ADR-0070 row closes with the reason

- **Status:** Proposed (2026-10-06)
- **Date:** 2026-10-06
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0070's table (the `SystemContainer` row), ADR-0058 (a
  Proxmox LXC is not a container provider), ADR-0071 (the provider block),
  `crates/contexts/delonix-stack/src/kinds.rs`, item 2.3 of
  `docs/discovery/66_CONTINUITY_PLAN.md`

## Context

ADR-0070's table has a row saying `SystemContainer` should become «a guest `kind`
with `provider.proxmox`», on the grounds that ADR-0058 found its semantics closer
to a VM than to a Pod, and it names what blocks the move: «the ADR-0058 contract
(`exec`/logs unsupported, hot vs cold fields) has to be re-expressed on the new
Kind and its snapshot/backup/move verbs repointed».

Half of that row is already done. ADR-0071 (#690) moved the provider-specific
fields into `spec.provider: { type, spec }`, typed per provider **and per
resource** — so the inline-provider rule of ADR-0070 D1 is satisfied for this
Kind. What the row still asks for is the Kind MERGE.

Measured on 2026-10-06, against `main`, three things that merge badly:

**1. The two Kinds disagree on namespace, and a Kind has one answer.** In
`kinds.rs`, `SYSTEM_CONTAINER` is `Namespaced::Never`, with the reason written
next to it: «the provider's node has no namespace of this engine's to put it in:
the engine's isolation does not reach it (ADR-0058 T7)». `VM` is
`Namespaced::Always`. A merged Kind forces one of the two to lie, and the table
is what the loader, the plan name and the namespace warnings all read.

**2. The source of the two is not the same thing.** A `SystemContainer` is
created from an **OCI image** (`spec.image`, which the engine pulls and stages as
a template the node accepts); a `VirtualMachine` is created from a **disk**
(`spec.disk`). A merged spec carries both and has to refuse the wrong one per
provider — and «which of these two did you mean» is the question the two Kinds
answer by existing.

**3. The verb surface is 30 against 4.** `delonix vm` has cloud-init, create,
destroy, migrate, move, pause, resize, restart, start, stop, unpause, ls,
console, ssh, vnc, bridge, reach, unbridge, build, convert, ls-remote, pull,
push, apply, init, prune, snapshot and default-backend. `delonix systemcontainer`
has clone, move and snapshot. Merging means the LXC path has to refuse, by name,
roughly 28 verbs it does not have — including `console`, `ssh` and `vnc`, which
ADR-0058 already recorded as absent from the node's API. Twenty-eight new
refusals on a Kind that works today is a lot of surface to get wrong, and the
failure mode of getting one wrong is the one this engine persecutes: a verb that
answers instead of refusing.

## Decision

**`SystemContainer` stays its own Kind.** The `SystemContainer` row of
ADR-0070's table closes as **satisfied by ADR-0071 for its D1 part, and rejected
for the Kind merge**, with the measured reason recorded.

The work is to write that down where it will be read:

1. ADR-0070's table row says this, so the next reader does not start the merge.
2. `AGENTS.md` keeps one line on why there are two guest Kinds: the engine's
   isolation reaches one and not the other, and they are created from different
   things.

What ADR-0058's contract keeps, unchanged: `exec`, logs and the exit code stay
`unsupported-by-provider` with the reason written; `memory`, `swap` and `cores`
are hot; `rootfs` grows only; and the day-2 verbs stay where they are, so nothing
is repointed.

## Alternatives considered

- **Fold `SystemContainer` into `VirtualMachine` with `provider.type:
  proxmox-lxc`.** Rejected on the three measurements above. The namespace one is
  decisive by itself: `Namespaced` is one value per Kind and both answers are
  load-bearing.
- **A third Kind, `kind: Guest`, that both lower into.** Rejected: it adds a
  Kind to remove a Kind, and every reader would then have to learn which of the
  three to write. The engine already has one Kind that does not survive the load
  (`Dependency`), and that is justified by folding MANY documents into one, not
  by renaming two into a third.
- **Rename `SystemContainer` to something that reads as a guest** (`kind:
  Lxc`, `kind: NodeContainer`). Rejected: a rename of a Kind with day-2 verbs and
  a registry costs a breaking change and an alias, and buys a word. ADR-0058
  already explains the name.
- **Do nothing and leave the row open.** Rejected for the same reason as in
  ADR-0072: an open row reads as pending work, and the next reader would
  re-derive the three costs.

## Consequences

- **Easier:** both Kinds keep one honest answer to «is this namespaced», «what is
  it made from» and «what verbs does it have». No refusal has to be written for a
  verb that was never offered.
- **Harder:** the catalogue keeps two guest Kinds, and a reader has to know which
  one a Proxmox LXC is. `AGENTS.md` and `delonix api-resources` both say it.
- **Debt assumed:** none new. If a second provider ever offers system containers,
  it joins `SystemContainer` through its own `provider.type` — which is exactly
  what ADR-0071's per-resource typing is for, and needs no Kind change.
- **What would reopen this:** a provider whose system containers DO sit inside the
  engine's isolation, which would remove measurement 1. Nothing on the roadmap
  offers one.
