# ADR-0021: A pull reconciler (`delonix gitops`) — opt-in, and still daemonless

- **Status:** Proposed
- **Date:** 2026-08-27
- **Deciders:** Walter Angolar
- **Related:** ADR-0010 (remote management API, **rejected**), ADR-0019 (stack
  revision history), ADR-0007 (generated manifest schema), ADR-0020 (CLI
  surfaces), `docs/gitops.md`, `AGENTS.md` §«IaC nativo» and §«Sobreviver a um
  reboot».

## Context

`docs/gitops.md` describes a complete GitOps flow and then says, in its own
closing section, what it does not do:

> **Não reconcilia continuamente.** Converge quando lhe chamas. Um loop de
> controlo é trabalho de orquestrador, fora de escopo por desenho — o que aqui
> existe é um gate de deriva, que é o mesmo resultado sem um daemon.

That is honest and it is also incomplete. Measured against the four
[OpenGitOps](https://opengitops.dev/) principles, the engine satisfies
*declarative* and *versioned & immutable*, and satisfies *pulled automatically*
and *continuously reconciled* **only through a CI system that runs `apply` on
the engine's behalf**. The manifest is the source of truth; who reads it is not
the engine.

The gap is real, and it is one specific thing: **nothing on the node ever looks
at the repository.** Everything downstream of that already exists.

### Baseline, measured 2026-08-27 against `origin/main` (v0.66.1)

What the proposal assumes, and is there:

- `stack validate` / `plan` / `apply` / `prune` / `wait`, idempotent, with
  `--detailed-exitcode` (0/2/1) already shaped for a drift gate in CI.
- Three-way diff without a state file — `delonix.io/stack` (ownership) and
  `delonix.io/last-applied` (the applied spec), both stamped on the resource.
- `events.jsonl`, the shared Prometheus registry, OpenTelemetry spans.
- `delonix net boot`, which already writes `systemd --user` units for rootless
  and system units for root — the timer machinery is not new work.

What the proposal states and the measurement corrects — each of these changes
the design, which is why they are here and not in a footnote:

1. **`rollback` and `history` already exist.** ADR-0019 is accepted and
   implemented: `delonix stack history` and `delonix stack rollback`, one
   rendered revision per apply under `<root>/stacks/<stack>/revisions/`. The
   proposal designs rollback as if the engine had none. Its *conclusion* — the
   canonical rollback is `git revert`, not a CLI verb — survives, and this ADR
   keeps it. What does not survive is ignoring the mechanism: see «Interaction
   with ADR-0019», which is a defect, not a detail.
2. **The Kind counts and names are stale.** `delonix stack plan --fields`
   prints the authoritative list: 16 Kinds, of which **11 converge**, one
   (`Secret`) is ensure-present with a written reason, and the rest are
   sugar/aggregate. The v0.65.0 cut renamed `Vm`→`VirtualMachine`,
   `FirewallPolicy`→`NetworkPolicy`, `Tunnel`→`Gateway`. `docs/gitops.md` still
   says «doze dos treze» with the old spellings, and is itself stale.
3. **The reconciler's actions are `Create`, `Adopt`, `Update`, `Replace`,
   `Delete`, `NoOp`, `Conflict`** (`cmd/reconcile.rs::Action`). There is no
   `HotUpdate`, no `Unsupported`, no `PrerequisiteMissing`. `Update` *is* the
   live convergence; a missing prerequisite is the plan's `!`, a **condition**
   and not an action. A safety allowlist written against invented names would be
   accepted-and-ignored — the failure class this repository has corrected three
   times (`--security-opt seccomp=`, `-v …:z`, `--network-alias`).
4. **There is no git dependency in the workspace.** Neither `git2` nor `gix`.
   The proposal never names this choice, and it is the largest one in the
   design; see the decision below.

## The three objections this has to answer first

**Is this a daemon?** No, and the distinction is already written in `AGENTS.md`:
continuous reconciliation «entra por unit/timer `systemd` a invocar o `stack
apply` que já existe, e um processo residente precisa do seu próprio ADR com a
evidência do que o timer não resolveu». This ADR takes exactly that path. A
timer that fires a short-lived process leaves nothing running between fires;
the engine with GitOps unconfigured is byte-for-byte the engine of today. The
`watch` mode the proposal mentions as an alternative for hosts without systemd
**is** a resident process and is therefore *out of scope here* — it needs the
evidence the guardrail asks for, which nobody has yet.

**Is this ADR-0010 again?** No, and the difference is the whole point. ADR-0010
rejected an API that **accepts commands from outside**: it needs identity,
authorization and audit, and without them remoteness is not worth having. Here
nothing outside speaks to the node. The node reads a repository, over a
read-only deploy key, and decides for itself. The attack surface is the
opposite one, and the trust anchor is a signed commit rather than a session.

**Is this the PaaS's job?** Fleet, tenants, RBAC, approvals, environment
promotion — yes, and none of that is here. What is here is one host converging
one manifest, which is what a node runtime is for. The line is the same as the
CRI's: the engine serves a kubelet without knowing whose kubelet it is.

## Decision (proposed)

### 1. `kind: GitOpsSource` — a Kind, like everything else declarative

An earlier draft of this ADR put the configuration in a separate registry fed by
`delonix gitops register -f`, on the argument that the Kinds are resources the
engine *runs* while this is configuration of the reconciler that runs them. That
argument does not survive contact with the product: **this is Infrastructure as
Code, and a second file format with its own verb, sitting beside a manifest that
already declares networks, volumes and VMs, is the incoherence.** Nobody asks why
a `Volume` is declarative and the thing that applies it is not.

It also does not survive contact with the table. The draft claimed a GitOps
object «has no honest answer» to the columns of `cmd/kinds.rs`. Measured against
the struct, it answers every one:

| Column | Answer | Why |
|---|---|---|
| `api_version` | `gitops.delonix.io/v1alpha1` | per-Kind groups already exist (`core.delonix.io/v1alpha1`) |
| `domain` | `composition` | it composes other resources, like `Stack` and `KubernetesCluster` |
| `form` | `primary` | not sugar for anything |
| `converges` | **yes** | `url`, `ref`, `path`, `interval` and the policy are all comparable — this is what earns it a `plan` |
| `teardown` | **yes** | deregister and remove the timer |
| `namespaced` | `always` | a source applies into a namespace |
| `presence` | `registry` | it has a record of its own |
| `in_stack` | yes | a `kind: Stack` can carry one |

A Kind is the *right* shape, not a tolerated one: it inherits `plan`, `apply`,
`prune`, `destroy`, three-way diff, ownership and the generated schema (ADR-0007)
— all of which the separate registry would have had to reimplement or go
without. `delonix gitops register` disappears; the verb is `stack apply`.

Naming: **`GitOpsSource`**, not `Application`. Argo CD's noun is `Application`,
and here that word already means something else in the PaaS above — a Kind whose
name means one thing in the engine and another one layer up is a defect waiting
for its first support ticket.

What stays imperative is only what an operator does *to* a source that already
exists: `status`, `history`, `reconcile --once`, `suspend`/`resume`.

```yaml
apiVersion: gitops.delonix.io/v1alpha1
kind: GitOpsSource
metadata:
  name: production
  namespace: production
spec:
  source:
    url: ssh://git@github.com/ngolacloud/infrastructure.git
    ref: refs/heads/main            # resolved to a commit before anything reads it
    path: environments/production
    interval: 60s
    secretRef: github-production-deploy-key
    knownHostsRef: github-known-hosts
    signedCommitsRequired: true
    trustedKeysRef: production-git-signers
  sync:
    automated: true                 # installs the timer; false is fetch-and-report only
    selfHeal: true
    wait: true
    healthTimeout: 5m
  safety:
    allow: [Create, Update]         # the real `Action` names, nothing invented
    prune: false
    maximumDeletes: 5
    protectedResources:
      - Volume/*
      - VirtualMachine/database
```

Two things in there are load-bearing and easy to get wrong. `sync.automated`
**is** the timer — installing it is an apply, not a second verb, which is what
keeps «what is running on this node» answerable from the manifest alone. And
credentials are named, never carried: `secretRef` points at a `kind: Secret`
that already exists on the node.

### 2. Git by shell-out, not by library.

`git(1)` is invoked as a subprocess. `gix` and `git2` are both large additions
to the dependency graph of a container runtime, and this repository has refused
smaller ones for that reason (no `comfy-table` for column alignment). The
precedent for shelling out is broad and load-bearing: `ssh`/`scp` in
`cmd/remote.rs`, `virsh`, `qemu-img`, `nft`, `ip`. The cost is that `git` becomes
a host prerequisite for this feature — which the failure must **name**, the way
`vmimage::tool_package` learned to: a bare `ENOENT` reads as a missing file
path, not as a missing tool.

Non-negotiable flags, each closing a known vector rather than a hypothetical:
`core.hooksPath=/dev/null` (a repository must not execute code on the node
merely by being cloned), no submodule recursion, `--depth` bounded, a fixed
`GIT_SSH_COMMAND` with a pinned `known_hosts` and `StrictHostKeyChecking=yes`,
`GIT_TERMINAL_PROMPT=0`, and a timeout on every invocation.

### 3. The revision is resolved to an immutable artifact before anything reads it.

A ref is a moving target; a commit is not. Fetch, resolve to a commit SHA and a
tree digest, verify the signature if required, and only then render. Nothing
downstream ever reads a mutable working tree. This is what makes «the same
commit reconciled twice is the same reconciliation» true rather than hoped for.

### 4. The safety policy is written against the real `Action` enum.

Automatic reconciliation allows `Create`, `Update` and `NoOp`. It blocks
`Adopt`, `Replace`, `Delete` and `Conflict` unless the registration opts in,
per action. The reasoning is not symmetric and should not be: `Update` is live
convergence that keeps the PID; `Replace` tears the resource down and rebuilds
it, and for a `VirtualMachine` that throws away the overlay disk. A timer must
not be able to do that because someone edited a cold field.

`prune` stays off by default, runs last, and honours a maximum-deletes ceiling
and a protected-resource list. Argo CD keeps prune, self-heal and allow-empty as
independent switches for the same operational reason.

### 5. One lock per target, and the existing lock idiom.

Two fires must not overlap. The `flock` idiom the stores already use, keyed on
the target, and a fire that cannot take the lock **skips and says so** rather
than queuing — a queue of applies is how a slow apply turns into an apply storm.

### 6. Units are prefixed `delonix-gitops-`.

Not `delonix-`. That prefix once matched `delonix-cri.service` and a
`net boot disable` deleted the kubelet's CRI endpoint on a Kubernetes node. The
prefix is decided here so it is not decided by accident later.

## What being a Kind opens, and how each is closed

Making the source declarative means it can appear in the very repository it
pulls — self-management, which is the point and also where Argo CD's known
failure modes live. Four of them, each with the answer this ADR proposes:

**Bootstrap is circular, and that is fine.** Who applies the manifest that
declares the source? The first apply is local — `stack apply -f` by hand, or
`delonix gitops bootstrap <url>`, which is the same thing with the clone in
front. From there the source may declare itself and take over. This is Argo's
«app of apps», and it works because the circle is only closed *after* the first
turn.

**A source must never prune itself.** A commit that deletes the
`kind: GitOpsSource` from the repository, with prune enabled, would switch off
the very reconciliation that would later pull the revert — a node that goes
silent and cannot be talked back. So: **the source that owns the current run is
excluded from its own prune**, and says so. Removing one is a deliberate local
act (`stack destroy`, `gitops rm`), never a side effect of an edit.

**A source may declare itself; it may not declare others.** Same identity
(`metadata.name`) is self-management. A *different* `GitOpsSource` in the applied
tree is refused, because that is how one repository silently acquires the node's
other repositories — and how a reconciliation loop becomes a tree nobody can
read from a single manifest. Depth one, enforced by name comparison.

**The credential is always local.** `secretRef` names a `kind: Secret` that must
already exist on the node — it cannot come from the repository it unlocks. That
egg-and-chicken has one honest answer, and it is the same one every GitOps tool
gives: the bootstrap secret is created out of band.

### `suspend` is local state, and deliberately not a spec field

An emergency stop must not require a round-trip through Git: the moment a
reconciliation is actively damaging production is the worst possible time to
demand a commit, a review and a merge. But a `suspended: true` in the spec and a
local override would be two answers to one question, and the plan would show
eternal drift between them.

So `suspend` writes a **local marker outside the spec** — the same shape as
`last-applied` being an annotation rather than a field — the three-way diff does
not see it, and `gitops status` shows it in bold. `resume` clears it. A pause
that is invisible is worse than no pause at all.

## Interaction with ADR-0019 — a defect, found by composing the two

`stack apply` records a revision on **every** apply, unconditionally, including
one whose plan is entirely `NoOp` (`cmd/stack.rs`, both the success and failure
paths). ADR-0019 keeps `KEEP = 20`, pruned by the writer.

A GitOps target with a 60-second interval therefore **destroys its own useful
history in twenty minutes**: twenty no-op revisions of the same unchanged
manifest, and the apply that actually changed something scrolled off the end.
The mechanism written to answer «what did this stack apply, and when?» would
answer «the same thing, twenty times, in the last twenty minutes».

Two candidate fixes, and this ADR proposes the first:

- **Do not record a revision when the plan changed nothing.** A revision is a
  record of what was *asked for*; a no-op apply asked for nothing new, and the
  previous revision already says what the desired state is. This is a change to
  ADR-0019's implementation, not to its decision, and it makes the history
  useful for hand-run applies too. **Done** — landed ahead of the rest of this
  ADR, because it improves `stack history` today for anyone applying by hand,
  with no GitOps involved. The predicate is `Action::is_change`, the same one
  behind `--detailed-exitcode`, so «a revision was written» and «the plan
  reported changes» cannot drift apart. ADR-0019 carries the amendment note.
- Make `KEEP` configurable. Rejected for the reason ADR-0019 already gives: a
  knob invites the question of what zero means, and the underlying problem is
  that no-ops are recorded at all.

A gate asserts it: N reconciliations of an unchanged commit leave the revision
count unchanged.

## What this does not do

- **No `delonix gitops rollback`.** The canonical rollback is `git revert`,
  because the next fire would re-apply `HEAD` and undo any local rollback. The
  emergency path is `suspend` → act → record in git → `resume`, and `suspend`
  exists for exactly that.
- **No fleet.** One host, one target set. Several nodes are several
  registrations.
- **No Kubernetes-internal reconciliation.** A `KubernetesCluster` can be
  created and kept by this; what runs *inside* it is Flux or Argo CD.
- **No secrets in the checkout.** The registration references a `kind: Secret`
  by name; the checkout is scanned and refused if it carries private key
  material.

## Phases, each with its gate

1. **This ADR + a `delonix-runtime-sec` pass.** The feature holds a credential
   that reads a private repository and executes what it finds — the same bar
   ADR-0009 set for holding a credential that destroys data elsewhere.
2. **The Kind itself** — one row in `cmd/kinds.rs`, the spec, the generated
   schema, `desired`/`actual`, teardown. No fetching yet: a `GitOpsSource` that
   registers, plans and prunes like every other Kind, and reconciles nothing.
   Gate: the three-list test and `stack plan --fields` accept it without a
   special case, and a manifest unchanged plans zero differences.
3. **Source and immutable artifact.** Fetch, verify, resolve, report status. No
   apply. Gate: a tampered signature and an unreachable remote both fail closed,
   with the tool named.
4. **`gitops reconcile --once`,** wired to the same internal path as
   `plan`/`apply`/`wait`. Not a second creation path. Gate: self-management —
   a source that declares itself converges; one that declares a *different*
   source is refused.
5. **Safety policy.** Gate: a commit that plans a `Delete` leaves the
   infrastructure intact and prints which action was blocked and why.
6. **Timer, lock, backoff with jitter, reboot recovery.**
7. **Observability** — events, gauges on the existing registry, redaction.
8. **E2E and failure injection**, in `scripts/e2e.sh`: invalid commit, remote
   down, two concurrent fires, crash mid-apply, drift, blocked prune, secret in
   the tree, reboot, **a commit that deletes the source itself**, and a source
   that tries to declare another one. Verified by the repo's rule — each gate must fail with its
   fix reverted.

## The acceptance criterion

A valid merge converges without anyone touching the node. An invalid or
destructive commit leaves the previous infrastructure **byte for byte intact**
and says exactly which action was blocked, by which rule, at which commit.

Neither half alone is the feature.
