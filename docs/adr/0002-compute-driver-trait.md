# ADR-0002: Where a generic compute driver trait lives (and whether to extract one now)

- **Status:** Accepted (Phase 2a; Phase 2b deferred on-trigger)
- **Date:** 2026-07-30 (accepted 2026-07-30)
- **Deciders:** Walter (owner) + Chief Runtime Architect review
- **Builds on:** ADR-0001 (the `kind: Workload` dispatcher). **Related:** `AGENTS.md` "Universal
  Runtime" Phase 2; `docs/runtime/dependency-map.md` §4; `docs/runtime/runtime-architecture.md` §4.

## Context

The North Star (ADR-0001) now has a declarative object that lowers `kind: Workload` to
`kind: Container`/`kind: Vm`. Phase 2 of the product vision asks to **"extract a general trait
from `VmBackend`, reusable by `delonix-runtime-core`, so the container engine can implement it
too"** — one `ComputeDriver` abstraction behind which both compute types sit.

Ground truth (confirmed in code, 2026-07-30):

- **`VmBackend`** (`delonix-vm/src/lib.rs:438`) is the only trait in the workspace:
  `id`/`available`/`boot`/`is_running`/`ip`/`stop`, selected by `select_backend()`. Its method
  signatures reference `VmConfig` (`delonix-vm:67`), `CreateStage` (`:372`), `Boot` (`:422`) —
  **all defined in `delonix-vm`** — plus `Vm` and `Result` (in `core`).
- **The container engine (`delonix-runtime`) has no backend object and no trait.** It is free
  functions: `create`/`create_with`/`create_networked`/`exec`/`stop` (`lib.rs:2996`+). There is
  no `ContainerBackend` to make implement a shared trait — it would have to be wrapped.
- **`delonix-runtime-core` is the dependency sink** (imports nothing internal — dependency-map §2).
  It **cannot** gain an edge to `delonix-vm`/`delonix-net`/`delonix-runtime`.
- The CLI dispatches to two different shapes today: `delonix_vm::create(&base, &cfg)` and
  `delonix_runtime::create*(...)`.

**The tension the vision's note underestimated:** `VmBackend` cannot be moved to `core` as-is —
its signatures name `VmConfig`/`CreateStage`/`Boot`, which live in `delonix-vm`, and `core` may not
depend on `delonix-vm` (guardrail: core is the sink). Any core-resident trait must therefore use
**associated types** (so no engine-specific concrete type leaks into core) — which makes the trait
`dyn`-unfriendly exactly where the Workload dispatcher wants a uniform boxed driver. Meanwhile the
two engines are genuinely different shapes (VM = an object with CH/libvirt backends and DHCP IPs;
container = free functions over namespaces/cgroups), so the honest common denominator is small.

**Guardrails touched:** core-is-sink / engine-clean (#4 — the whole point), PaaS boundary (#2 — a
driver is node-level, no tenant), daemonless (#1 — a trait is not a daemon), no-silent-failure (#6).

## Decision (proposed)

**Do the extraction in two steps, smallest first, and only promote to `core` when a second
consumer actually exists** — do not design for a consumer that may never appear.

- **Phase 2a (recommended now): define a minimal `ComputeDriver` trait in `delonix-runtime-bin`,
  next to the ADR-0001 dispatcher.** Two implementors wrap the existing engines
  (`VmComputeDriver` → `delonix_vm::create`; `ContainerComputeDriver` → `delonix_runtime::create*`).
  The trait surface is the small common denominator the Workload dispatcher needs, using `core`
  types only:

  ```rust
  // in delonix-runtime-bin — no engine crate changes, no core edge
  trait ComputeDriver {
      fn kind(&self) -> &'static str;                 // "container" | "vm"
      fn available(&self) -> bool;
      fn ensure(&self, name: &str, ns: Option<&str>, spec: &serde_yaml::Value) -> Result<String>; // idempotent create → id
      fn status(&self, name: &str) -> Result<Status>; // core::Status
      fn stop(&self, name: &str) -> Result<()>;
  }
  ```

  This delivers the unified dispatch immediately, with **zero structural risk**: no engine crate
  is touched, `core` stays the sink, and the rich VM types never enter the trait.

- **Phase 2b (deferred, its own ADR — only if triggered): promote the trait to shared code** when
  a **second** consumer beyond the CLI needs it (e.g. `delonix-cri` or `delonix-mgmt` wanting to
  dispatch by workload type). At that point choose between:
  - **(A) trait in `core` with associated types** (`type Config; type Handle;`) — keeps core the
    sink; cost: `dyn` erasure at the dispatch boundary; **or**
  - **(B) a new thin `delonix-compute` crate at L1** holding the trait + a neutral spec/handle,
    with `runtime`/`vm` depending on it — cleanest types, cost: a new crate (a supply-chain and
    structural addition, hence its own ADR).

  The trigger, not the calendar, decides 2b. Absent a second consumer, 2a is the whole feature.

## Alternatives considered

- **Move `VmBackend` verbatim into `core` (the literal vision note).** Rejected: impossible without
  giving `core` an edge to `delonix-vm` (its signatures name `VmConfig`/`Boot`/`CreateStage`) —
  violates guardrail #4. The note underestimated type-locality; this ADR records why.
- **Define the general trait in `core` now, with associated types, and make both engines implement
  it (full Phase 2 up front).** Rejected *for now*: it forces the container engine's free functions
  into a backend object and pays the `dyn`-erasure cost before any consumer beyond the CLI exists —
  designing for a consumer that may never appear. Kept available as Phase 2b option (A).
- **New `delonix-compute` crate now.** Rejected *for now*: adds a crate (structural + supply-chain
  cost) before the single-consumer case is proven insufficient. Kept as Phase 2b option (B).
- **Do nothing / keep two dispatch shapes.** Rejected: ADR-0001 already created the Workload
  dispatcher; leaving it as an ad-hoc `match doc.kind` in `-bin` (no trait) is workable but forgoes
  the one clean seam that makes adding a third compute type (WASM, a new `VmBackend`) a single
  `impl`. 2a buys that seam cheaply.

## Consequences

**Easier:** a single seam for the Workload dispatcher; adding a compute type becomes one
`impl ComputeDriver`; the VM's existing `VmBackend` (CH/libvirt selection) stays untouched *under*
`VmComputeDriver` — this ADR does not disturb working code (the brief's "never replace working
components" rule). Testable in isolation (`-bin` unit tests, the `delonix-testing` discipline).

**Harder / debt:** two trait layers for VMs (`ComputeDriver` → `VmComputeDriver` → `VmBackend`) —
justified only if the outer seam earns its keep; if Phase 2b never triggers, the trait stays a
thin CLI-local adapter (acceptable). The `ensure(spec: &serde_yaml::Value)` signature keeps the
untyped spec at the driver boundary (each driver re-deserializes to its typed spec, exactly as
`container::apply`/`vm::apply` do today) — deliberate, so the trait doesn't have to name both
`ContainerSpec` and `VmConfig`.

**Open decision for the owner:** accept the **2a-now / 2b-on-trigger** split, or mandate the full
core/`delonix-compute` extraction immediately (accepting the `dyn`/crate cost up front). This ADR
recommends the split; the alternative is a legitimate call if a second consumer is already known to
be imminent.

**Guardrail audit (2a):** core untouched ✅ · engine crates untouched ✅ (adapters live in `-bin`) ·
no new dependency ✅ · daemonless ✅ · PaaS boundary ✅ · no silent failure ✅ (driver selection by
explicit `type`, already fail-closed in ADR-0001). Phase 2b re-runs this audit for whichever of
(A)/(B) is chosen.

## Implementation note (Phase 2a, 2026-07-30)

Landed as `cmd/workload.rs`: `trait ComputeDriver { list, owns, stop, remove }` with
`ContainerDriver`/`VmDriver` adapters that delegate to `cmd::container::workload_*` /
`cmd::vm::workload_*` (thin wrappers over the engines' existing, tested list/stop/rm — no logic
duplicated, no engine crate touched). Its **real consumer** is the new `delonix workload {ls,stop,rm}`
command group — the trait was deliberately not built as bare scaffolding (that would be the repo's
documented "dead code awaiting a caller" anti-pattern). Every trait method has a caller (`ls`→`list`,
`stop`/`rm`→`owns`+`stop`/`remove`). Routing is by exact name and fail-closed: zero owners →
`no such workload`; a container AND a vm sharing a name → `ambiguous`, pointing at the type-specific
command. `owner()` is pure over the driver slice and unit-tested with fake drivers. `ensure`
(declarative create) stays out — creation is `kind: Workload` via `stack apply` (ADR-0001), so the
trait carries no create verb until an imperative `workload run` proves worth it. Validated live:
`workload ls` unifies containers+VMs in one table; `stop`/`rm` route and clean up via the existing
engine paths (single-line output mirroring the native subcommands).
