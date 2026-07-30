# Architecture Decision Records

Structural decisions for the Delonix Runtime — one file per decision, `NNNN-title.md`, English
source (like the rest of the code documentation). Process and non-negotiable guardrails live in
the `delonix-adr` skill (`skills/delonix-adr/`). A decision that violates a guardrail is
wrong: daemonless by design · no tenant/licence/billing · no private-repo dependency · engine
crates dependency-clean · GO/NO-GO spike before any new privilege boundary · no silent failure.

An ADR decides; `martin` draws (C4/sequence diagrams in `ARCHITECTURE.md`). Accepted ADRs are
never rewritten — supersede them with a new one.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-workload-kind-schema.md) | Introduce `kind: Workload` as a lowering layer over existing Kinds | Accepted (implemented) |
| [0002](0002-compute-driver-trait.md) | Where a generic compute driver trait lives (and whether to extract one now) | Accepted (Phase 2a implemented) |

## Roadmap (from `AGENTS.md` "Universal Runtime" — each phase needs its own accepted ADR)

- **0001** — `kind: Workload` schema. *Accepted & implemented* (`cmd/workload.rs`, lowers in `manifest::load`).
- **0002** — Generic compute driver trait: 2a-now / 2b-on-trigger split. *Phase 2a implemented* —
  `ComputeDriver` trait + container/vm adapters in `cmd/workload.rs`, consumed by the
  `delonix workload {ls,stop,rm}` command group. Phase 2b (promote to `core` / a `delonix-compute`
  crate) stays deferred until a second consumer (cri/mgmt) needs it.
- **0003** — Tenancy-free capability model (VM_CREATE / NETWORK_ATTACH / …) at the API boundary,
  **without** identity/tenant/audit context (that half is out of bounds). *Not started.*
