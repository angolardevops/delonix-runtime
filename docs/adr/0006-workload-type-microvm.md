# ADR-0006: `type: microvm` forces the microVM hypervisor (Cloud Hypervisor)

- **Status:** Accepted
- **Date:** 2026-07-30
- **Deciders:** Walter (owner) + Chief Runtime Architect review
- **Builds on:** ADR-0001 (`kind: Workload` lowering). **Related:** `CLAUDE.md` "kind: Workload"
  (microvm reserved), `delonix-vm` `VmBackend` (CloudHypervisor | Libvirt).

## Context

ADR-0001 shipped `kind: Workload` with `spec.type: container | vm | pod`, and kept **`microvm`
reserved** with a fail-closed error, precisely because giving it meaning is a real semantic choice —
not a silent alias. This ADR makes that choice.

Ground truth: the engine has two `VmBackend`s — **Cloud Hypervisor** (a microVM VMM: fast boot,
minimal device model) and **libvirt/QEMU** (a full VM). `type: vm` today lowers to `kind: Vm`,
which auto-selects (CH if installed, else libvirt) or honors an explicit `backend`. So the only
honest distinction between "vm" and "microvm" on this runtime is **which hypervisor class runs it**.

Forces:
- A `type: microvm` that were a pure alias for `vm` would be meaningless (ADR-0001 rejected that).
- Forcing a backend is an **opinion with consequences**: on a host where libvirt works but CH is
  absent, a `microvm` fails where a `vm` would succeed. That is *correct* — the user explicitly
  asked for a microVM — but must be documented, not surprising.
- The golden k8s image boots **libvirt-only** (see `CLAUDE.md`); it is not a `microvm` workload.

## Decision

**`type: microvm` lowers to `kind: Vm` with the `backend` forced to `cloud-hypervisor`** (the
runtime's microVM VMM). Following the ADR-0001 contract, the block is named after the type:
`spec.microvm`, and it is **exactly a `VmSpec`** (same schema as `spec.vm`).

Rules:
- The lowered `VmSpec` gets `backend: cloud-hypervisor` injected when the block does not set one.
- A block that **explicitly sets a non-CH backend** (e.g. `backend: libvirt`) is a **contradiction**
  → fail-closed error pointing at `type: vm`. An explicit `cloud-hypervisor`/`ch` is accepted (redundant).
- `microvm` needs Cloud Hypervisor installed and a CH-bootable image; if CH is unavailable or the
  image is libvirt-only, boot fails closed (the VM backend surfaces it) — the same honesty as any
  other missing-backend path.
- **Firecracker is not a backend today** (only CH + libvirt exist). If it is added later, "which
  microVM backend does `microvm` prefer" is revisited *in this ADR's successor*, not silently.

## Alternatives considered

- **`type: microvm` = alias for `type: vm`** (no backend forcing). Rejected: meaningless — ADR-0001
  already rejected a silent alias.
- **Reuse the `spec.vm` block for `microvm`** (type: microvm + `spec.vm`). Rejected: breaks the
  ADR-0001 "block named after the type" contract; `spec.microvm` keeps the schema self-describing.
- **Force the backend but *ignore* a conflicting explicit `backend`.** Rejected: silently overriding
  the user's `backend: libvirt` violates no-silent-failure. A contradiction must error.
- **Keep `microvm` reserved.** Rejected: the distinction is well-defined (CH vs libvirt), so the
  reservation no longer buys anything — reserving a decidable thing is just an unfinished feature.

## Consequences

**Easier:** a paved-road name for "give me a microVM" without the user typing `backend:
cloud-hypervisor`; future-proof if a second microVM backend appears (the forcing logic centralizes
the choice). `kind: Workload` now covers `container | vm | pod | microvm` — the discriminator the
North Star named.

**Cost / debt:** `microvm` is host-dependent (needs CH) in a way `vm` is not — documented, and
fail-closed rather than surprising. The backend-injection touches the raw `serde_yaml::Value` before
`VmSpec` deserialization (a tiny, tested transform), not the engine.

**Guardrail audit:** daemonless ✅ · PaaS boundary ✅ (a hypervisor class, no tenant) · no new
dependency ✅ · engine crates untouched ✅ (lowering lives in `-bin`) · no silent failure ✅
(conflicting backend errors; unavailable CH fails closed at boot).

## Follow-up if accepted

Implementation is a `delonix-feature-dev` task in `cmd/workload.rs` (add the `microvm` block +
`force_microvm_backend` + tests + `examples/workload.yaml` + `CLAUDE.md`). This completes the
`kind: Workload` type discriminator; no reserved types remain.
