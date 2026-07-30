# ADR-0001: Introduce `kind: Workload` as a lowering layer over existing Kinds

- **Status:** Accepted
- **Date:** 2026-07-30 (accepted 2026-07-30)
- **Deciders:** Walter (owner) + Chief Runtime Architect review
- **Related:** `docs/runtime/runtime-architecture.md` §4 (target matrix, North Star row),
  `CLAUDE.md` "Visão de produto: Universal Runtime" (Phase 1).

## Context

The stated product North Star is a **Runtime Abstraction Layer**: one declarative object,
`kind: Workload` with `spec.type: container | vm | …`, that dispatches to the right engine — so
that `container`/`vm`/`image` stop being three disconnected CLIs and become one workload API.

Today (see `docs/runtime/current-state.md`) there is **no** unified model. The declarative surface
is per-Kind: `manifest.rs::load()` validates `apiVersion: delonix.io/v1`, expands `kind: Stack`
into per-Kind children, and dispatches each Kind to its own typed spec + `apply()`
(`cmd::container::apply`, `cmd::vm::apply`, `cmd::pod::apply`, …). The specs are already
convention-aligned: k8s-style camelCase, a shared canonical `restartPolicy` across `Container`
and `Vm`.

Forces:
- The North Star is high value but the space of "what a Workload can be" is wide; over-designing
  the schema now (a merged mega-spec, a new apiVersion group, a scheduler) is the trap.
- **Guardrails touched:** daemonless (#1) — a Workload must not imply a controller/daemon;
  PaaS boundary (#2) — `type` is a dispatch discriminator, never a tenant/plan; engine-clean (#4)
  — this must not touch any engine crate or add a dependency; no-silent-failure (#6) — an
  unsupported `type` must fail closed, never be ignored.
- **Evidence / precedent:** `kind: Stack` already proves the cheapest mechanism — it does **not**
  survive `load()`; it is expanded into individual per-Kind docs (inheriting `metadata.namespace`)
  before `apply`/`ls`/`describe`/`--dry-run` see anything. Everything downstream already handles
  the lowered children with zero per-Kind changes.

## Decision

Introduce `kind: Workload` as **sugar that lowers to an existing Kind at load time**, reusing the
`Stack` expansion mechanism. No new engine, no new backend, no daemon, no new apiVersion.

**Schema (v1):**

```yaml
apiVersion: delonix.io/v1
kind: Workload
metadata: { name: web, namespace: default }
spec:
  type: container            # discriminator: container | vm  (v1)
  container:                 # exactly the existing ContainerSpec, verbatim
    image: nginx:alpine
    ports: ["8080:80"]
    restartPolicy: always
```

```yaml
kind: Workload
metadata: { name: db-vm }
spec:
  type: vm
  vm:                        # exactly the existing VmSpec, verbatim
    disk: delonix-vm-k8s:1.34
    vcpus: 2
    memory: 4G
```

Rules:
1. **`spec.type` selects a single nested block named after the type** (`spec.container` /
   `spec.vm`), whose value is deserialized by the **existing** `ContainerSpec` / `VmSpec` structs
   — no field is redefined, so the Workload spec cannot drift from the underlying spec.
2. **Lowering happens in `manifest::load()`**, right where `Stack` is already expanded: a
   `kind: Workload` doc is rewritten into a synthetic `kind: Container` / `kind: Vm` doc carrying
   the nested block as its `spec` and inheriting `metadata` (name/namespace). The Workload does not
   survive `load()`; everything after it (apply, per-Kind `apply -f`, `stack apply`, `--dry-run`,
   `ls`, `describe`) sees the child and needs **zero** new wiring.
3. **`spec.type` is validated fail-closed.** v1 accepts `container` and `vm`. `pod`, `microvm`,
   and any other value are **reserved** and rejected with a clear, actionable error (e.g.
   `type: pod not yet supported — use kind: Pod`); never silently ignored, never defaulted.
4. **A Workload block must match its type.** `type: container` with a `vm:` block present (or the
   `container:` block absent) is an error, not a best-effort guess.
5. All changes live in `crates/delonix-runtime-bin` (`cmd/manifest.rs` + a small `cmd/workload.rs`
   for the lowering + validation). No engine crate is touched; `serde_yaml` is already a `-bin`
   dependency. `apiVersion` stays `delonix.io/v1`.

Deferred to a follow-up (documented, not silent): a dedicated `delonix workload ls/describe`
command group (the lowered child is shown under its real Kind for now, exactly as `Stack` children
are); `type: pod` (maps to `kind: Pod`) and `type: microvm` (a backend-forcing variant of `vm`).

## Alternatives considered

- **Do nothing / keep three Kinds.** Rejected: abandons the North Star; the disconnected-CLI
  problem the vision names stays unsolved.
- **Flat merge — `spec: { type, <all container AND vm fields inline> }`.** Rejected: forces a
  merged mega-struct, invites field collisions between container and vm semantics, and duplicates
  every field (drift risk). The nested-block form reuses the typed specs verbatim.
- **New apiVersion `runtime.delonix.io/v1`** (as the vision sketch suggested). Rejected for v1: a
  second API group adds a parallel validation path in `load()` for zero present benefit; the
  existing `delonix.io/v1` already carries every other Kind. Revisit only if/when Workload gains
  semantics the other Kinds cannot express.
- **Lower at apply-time instead of load-time** (a new `cmd::workload::apply`). Rejected: it would
  re-implement dispatch and miss `ls`/`describe`/`--dry-run`, which iterate the *loaded* docs. The
  `Stack` precedent (lower in `load()`) is strictly cheaper and already battle-tested.
- **Make Workload a controller/reconciler** (drift detection, continuous dispatch). Rejected hard:
  that is a daemon (guardrail #1) and an orchestrator concern (guardrail #2) — out of scope, and
  `apply` semantics are "ensure present", not reconcile, by design.

## Consequences

**Easier:** one object for both compute types; a natural home for the future generic compute
driver (ADR-0002) — the dispatcher already exists once this lands; the schema is a thin, testable
pure function (`lower_workload(doc) -> ManifestDoc`) with obvious property tests (type↔block
agreement, reserved-type rejection).

**Harder / debt assumed:** `describe`/`ls` show the lowered Kind, not `Workload` (same limitation
`Stack` has — acceptable, and documented, not hidden). Dependency ordering relies on the existing
per-Kind apply order (Network → Volume → Image → Vm → Container): a lowered Workload slots into
that order as its child Kind, so a `Workload(vm)` and a `Workload(container)` in the same file
order correctly, but two Workloads with a cross-dependency get no more ordering guarantees than
two raw Kinds do today.

**New maintenance surface:** `cmd/workload.rs` (lowering + validation) and the reserved-type list —
each new `type` value is a future ADR (`pod`, `microvm`), never a silent addition.

**Guardrail audit:** daemonless ✅ (load-time lowering, ephemeral) · PaaS boundary ✅ (`type` is a
discriminator, no tenancy) · no private dep ✅ · engine crates untouched ✅ (all in `-bin`, no new
dep) · privilege spike N/A (lowers to paths that already passed their spikes) · fail-closed ✅
(reserved types and type/block mismatch error explicitly).

## Follow-up if accepted

Implementation is a `delonix-feature-dev` task (pure lowering function + `load()` wiring + tests +
`examples/workload.yaml` + `CLAUDE.md` note), not further architecture. ADR-0002 (extract a generic
compute driver trait from `VmBackend`) builds directly on the dispatcher this ADR introduces.
