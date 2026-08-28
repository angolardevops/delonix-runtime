# ADR-0019: A stack keeps a revision history — a record, never a source of truth

- **Status:** Accepted
- **Date:** 2026-08-25
- **Deciders:** Walter Angolar
- **Related:** ADR-0007 (generated manifest schema), ADR-0016 (the state root),
  `docs/gitops.md`, `AGENTS.md` §«IaC nativo».

## Context

`stack apply` is fail-fast without rollback. That is a decision, not a gap:
undoing halfway is worse than stopping, and the code says so in three places. A
recent fix closed the part that WAS a defect — a failed apply left the resources
it had created without an owner, invisible to `plan`, `prune` and `destroy`.

What is still missing is the other half: **a way back**. Today the engine cannot
answer two questions that anyone operating a stack asks within the first week:

1. *What did this stack apply, and when?*
2. *Put it back the way it was on Tuesday.*

Both need something the engine does not have: a record of past applies.

### Why the existing event log does not serve

`delonix-runtime-core::events` already writes `<root>/events.jsonl`, and reusing
it was the first idea. Reading it ruled it out, and the module itself says why:

- **The fields are fixed and short on purpose.** Atomicity without a lock comes
  from every append staying under `PIPE_BUF` (4 KiB). A manifest does not fit,
  and making it fit would break the guarantee for every other producer.
- **Rotation keeps a single generation**, and the doc-comment states outright
  that «history is not the point of this».

So a revision needs its own mechanism. That means **new state**, and new state
under a runtime that has publicly refused a state file needs the distinction
below to hold — otherwise this ADR is just reinventing `terraform.tfstate`.

## The distinction this rests on

A `.tfstate` is the **source of truth for what exists**. That is what makes it
dangerous: it is consulted to decide reality, so when it drifts from the machine
the tool acts on a lie, and «state got out of sync» becomes a support category.

A revision here is a **record of what was asked for**. It is written after the
fact and read only by a human (or by `rollback`, which re-applies it as if it
were a manifest). It is never consulted to decide what exists.

The consequence is the testable property, and it is the one that keeps this
honest:

> **Delete `<root>/stacks/` entirely and `plan`, `apply`, `prune` and `destroy`
> keep working, byte for byte. Only the history is lost.**

Ownership and the three-way diff continue to come from where they come from
today — the `delonix.io/stack` label and the `delonix.io/last-applied`
annotation, both stamped on the resource itself. Nothing moves into the new
directory. A gate asserts this rather than the ADR asserting it.

## Decision

**A stack records one revision per apply, under
`<root>/stacks/<stack>/revisions/`.**

> **Amended by [ADR-0021](0021-gitops-pull-reconciler.md), implemented:** «per
> apply» became «per apply that ASKED FOR SOMETHING». An apply whose plan is
> entirely `NoOp` no longer spends a revision — it asked for nothing the previous
> revision does not already say, and with retention at 20 those recordings were
> destroying the history this ADR exists to keep. A FAILED apply is still
> recorded whatever its plan said. The decision below is unchanged; only its
> granularity is.

### What a revision holds

The **rendered manifest** — the same YAML `stack apply --dry-run` prints, with
every `#[serde(default)]` materialised, Stacks expanded and Kinds canonicalised.

Not the plan, and not the desired fields. Two reasons:

- **The manifest is the only thing that can be re-applied.** A plan is a diff
  against a machine state that no longer exists; replaying it means nothing a
  week later. `rollback` becomes «apply this manifest», which is one creation
  path, not a second one that drifts away from the first.
- **The rendered form, not the file as written.** A file can reference things
  that moved (`fromEnvFile`, a relative path, a `kind: Stack` that has since been
  edited). The rendered document is what the engine actually acted on, and it is
  self-contained.

Alongside it, a small header: the revision number, the unix instant, the
manifest path, whether the apply **succeeded**, and the counts from the plan.

### Failed applies are recorded too, and marked as failed

The temptation is to record only successes. That is wrong here for the same
reason `stack plan` never hides a resource: **the interesting question after an
incident is what the machine was asked to do, not what it managed to do.** A
failed apply that created half a stack is precisely the revision someone needs
to look at. It is written with `ok: false` and the error, and `rollback` refuses
to target one.

### Numbering, and why not a timestamp

Sequential, zero-padded (`0001.yaml`). A human says «go back two revisions», not
«go back to 1756118400». The instant is inside the header for when it matters.

The next number is derived from what is on disk, under the same `flock` idiom the
stores already use — two applies of the same stack in parallel must not both
claim `0007`.

### Retention: 20, pruned by the writer

There is no daemon, so nothing cleans up in the background. The writer prunes on
its way out, same as the event log rotates opportunistically. Twenty is enough to
cover «what changed this week» and small enough that nobody has to think about it
— a rendered manifest is kilobytes.

Not configurable in this ADR. A knob invites the question of what happens when
someone sets it to zero, and no measured need for it exists yet.

### Best-effort, and never fatal

A revision that cannot be written **must not fail an apply that worked**. Losing
the record is bad; refusing to run a workload because a log directory is
read-only is worse. It warns, loudly, and the apply keeps its exit code.

This is the same rule the stamp already follows, and the same rule `events::emit`
follows — stated here because it is the one an implementer is most likely to get
wrong by making the write `?`-propagate.

## What is NOT decided here

- ~~**`stack rollback`.**~~ **Shipped in the slice right after this one**, and
  the questions it left open were answered there: a rollback replays the recorded
  manifest through `apply_docs` — the same path a normal apply takes — and gets a
  revision of its own, marked with the one it replayed. What it cannot undo is
  printed BEFORE anything runs, counted from this rollback's own plan rather than
  as a general disclaimer: resources created after the target stay unless
  `--prune`, a recreated resource comes back EMPTY (the record holds a manifest,
  never the bytes), and a cold field still needs `--replace`. A revision recorded
  as FAILED is refused as a target — it is on record so it can be READ.
- **A revision as a rollback GUARANTEE.** Re-applying an old manifest is not a
  time machine, and this ADR does not let anything claim it is.
- **Cross-node or remote history.** Out of scope by guardrail — that is the
  control plane's, not this engine's (ADR-0010).

## Consequences

- `<root>/stacks/` is new, and it is the first directory under the state root
  that holds no resource. ADR-0016's inventory gains a row that is measured in
  kilobytes.
- `stack history` becomes answerable, and after it `stack rollback`.
- The engine still has **no state file** in the sense that matters: nothing under
  `stacks/` is read to decide what exists. If that ever stops being true, this
  ADR needs a successor, because the property the whole design rests on will have
  been given away.
