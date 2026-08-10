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
| [0003](0003-capability-model.md) | A tenancy-free capability model at the control-socket boundary | Proposed |
| [0004](0004-container-checkpoint-restore.md) | Container checkpoint/restore is gated on a rootless-CRIU GO/NO-GO spike | Proposed |
| [0005](0005-structured-output-json.md) | Structured output (`-o json`) for listing commands | Accepted (contract + first slice) |
| [0006](0006-workload-type-microvm.md) | `type: microvm` forces the microVM hypervisor (Cloud Hypervisor) | Accepted |
| [0007](0007-generated-manifest-schema.md) | The manifest schema is GENERATED from the code (`schemars`) | Accepted (implemented) |
| [0008](0008-proxmox-vm-backend.md) | Add a Proxmox VE backend as a separate crate, and make backends registrable | **Accepted, in 2 phases** — the registry now, the Proxmox backend blocked on a real host |
| [0009](0009-truenas-storage-provisioner.md) | Provision TrueNAS datasets over its API, as a separate crate | **Accepted** — with a `runtime-sec` pass and a chaos scenario for the destructive path |
| [0010](0010-remote-management-api.md) | What it would take for the management API to be remote | **Rejected** — the API stays local; remoteness belongs to the PaaS |

## Roadmap (from `AGENTS.md` "Universal Runtime" — each phase needs its own accepted ADR)

- **0001** — `kind: Workload` schema. *Accepted & implemented* (`cmd/workload.rs`, lowers in `manifest::load`).
- **0002** — Generic compute driver trait: 2a-now / 2b-on-trigger split. *Phase 2a implemented* —
  `ComputeDriver` trait + container/vm adapters in `cmd/workload.rs`, consumed by the
  `delonix workload {ls,describe,stop,rm}` command group. Phase 2b (promote to `core` / a `delonix-compute`
  crate) stays deferred until a second consumer (cri/mgmt) needs it.
- **0003** — Tenancy-free capability model (`ContainerRun`/`NetworkAttach`/…) gated at the
  control-socket dispatch, **without** identity/tenant/audit context (that half is out of bounds).
  *Proposed* — recommends staying Proposed until a lower-trust local socket consumer exists (same
  "wait for the second consumer" discipline as ADR-0002 Phase 2b).
- **0004** — Recovery Engine (container checkpoint/restore). *Proposed* — verified there is **no
  foundation** (`checkpoint_container` is a stub, zero CRIU); real checkpoint needs CRIU, gated on a
  **rootless-CRIU GO/NO-GO spike** + security audit, run only behind a concrete need. Recommends
  keeping the stub and not scheduling the spike yet.
