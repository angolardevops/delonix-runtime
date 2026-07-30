# ADR-0004: Container checkpoint/restore is gated on a rootless-CRIU GO/NO-GO spike

- **Status:** Proposed
- **Date:** 2026-07-30
- **Deciders:** Walter (owner) + Chief Runtime Architect review
- **Related:** `docs/runtime/current-state.md` §5 (verified stub), `docs/runtime/runtime-architecture.md`
  §4 (Recovery row), `CLAUDE.md` "Universal Runtime" (Recovery Engine), the kind/`--privileged`
  GO/NO-GO spike precedent, `delonix-runtime-sec` skill.

## Context

The product vision names a **Recovery Engine** with checkpoint/restore. Verified in code
(2026-07-30, this investigation):

- `delonix-cri`'s `checkpoint_container` is a **stub** — `todo("checkpoint_container")` →
  `Status::unimplemented` (`runtime_svc.rs:354`). Returning `Unimplemented` for this *optional* CRI
  method (forensic container checkpointing, KEP-2008) is standard and correct; it is not a bug.
- There is **zero CRIU** anywhere in the repo (`grep -i criu` → nothing).
- The only adjacent building block is `set_frozen`/`is_frozen` — a cgroup **freeze**
  (`delonix-runtime/lib.rs:4498`, backing `pause`/`unpause`). Freeze suspends scheduling; it is
  **not** a checkpoint (no memory image is written, nothing can be restored from disk after the
  process dies).

So there is **no Recovery-Engine foundation today.** The honest question is not "how do we finish
checkpoint" but "should we build it at all, and if so, is it even possible under this runtime's
rootless model?" — which is a spike, not a coding task.

Forces / guardrails touched:
- **Real checkpoint means CRIU** — a large external dependency (checkpoint/restore of process
  memory, fds, sockets, namespaces). Adding it is a supply-chain + privilege surface expansion
  (guardrail #4: engine-clean; guardrail #5: new privilege boundary).
- **Rootless CRIU is uncertain.** CRIU can run rootless but needs specific capabilities/config, and
  this runtime's userns + delegated-cgroup + seccomp model may or may not support a full
  dump/restore. This is genuinely unknown — exactly the kind of thing the kind/`--privileged` spike
  taught us to *measure*, not assume.
- **Daemonless OK:** checkpoint/restore is a one-shot operation, not a daemon — no philosophy issue.
- **PaaS boundary:** fleet DR / cyber-recovery orchestration stays 🚧 (control-plane). This ADR is
  only about *node-local* container checkpoint/restore.

## Decision (proposed)

**Do not build checkpoint/restore now. Gate it behind a `delonix-runtime-sec`-audited GO/NO-GO
spike on rootless CRIU, run only when a concrete consumer need appears.** Keep the
`checkpoint_container` stub as-is (correct for an optional CRI method).

When triggered, the spike must answer, empirically, on a real host (not by assumption):

1. Does `criu dump` succeed against a container in this runtime's userns + delegated cgroup +
   seccomp profile, **rootless**? (The likely blockers: `ptrace` across the userns boundary,
   cgroup freezer access, `/proc` visibility, socket/fd restore.)
2. Does `criu restore` bring it back to a working state, with the SDN veth / slirp reattached
   (the network is not part of the process image — restore has to re-run the existing attach path,
   like `container start` does)?
3. What privilege does it actually require, and does that stay within the rootless model or force a
   `--privileged`/root path (like `vm bridge` did)? If it needs root, that is a **NO-GO for the
   rootless-first default** and, at most, an opt-in privileged subcommand.
4. Supply-chain: is CRIU a runtime dependency of a container engine we are willing to carry, or a
   host prerequisite the operator installs (the `nvidia-ctk`/CDI model — the engine consumes a tool
   it does not vendor)? **Strong preference: host prerequisite, not a vendored dep.**

Only a GO on (1)–(3) with an acceptable answer to (4) justifies an implementation ADR. The engine
would then wire `checkpoint_container` (CRI) + a `container checkpoint`/`restore` CLI verb to a
CRIU shell-out, reusing the existing attach path for the network on restore.

## Alternatives considered

- **Cold "checkpoint" (freeze → tar rootfs + save config → restore = recreate).** Rejected as a
  Recovery-Engine answer: it does **not** preserve running memory/state, so it is not a checkpoint —
  it is a filesystem snapshot, which `delonix-image` (commit / `container commit`) already does. It
  would restore a *fresh* process, not a *resumed* one, so it buys nothing over `stop`+`start` for a
  stateless workload and loses the whole point (live state) for a stateful one.
- **Build CRIU checkpoint now, assume rootless works.** Rejected hard: assuming a privilege
  boundary works is exactly the mistake the kind/`--privileged` spike exists to prevent. No
  measurement, no build.
- **Vendor CRIU as a crate dependency.** Rejected (for now): a large privilege/supply-chain
  addition to a container runtime; if CRIU is used at all, prefer the host-prerequisite model.
- **Do nothing / leave the stub, no ADR.** Rejected only in the sense that the *unknown* deserved
  recording: this ADR IS the "do nothing until measured" decision, made explicit so checkpoint is
  never bolted on under pressure without the spike.

## Consequences

**Now:** the discovery docs state the verified fact (stub, no CRIU, no foundation); the vision's
Recovery item is honestly mapped to ⛔-with-a-gated-path instead of a vague 🟡; no code changes, no
new dependency, no privilege surface added. The `checkpoint_container` stub keeps behaving
correctly (Unimplemented) for any kubelet that probes it.

**If/when the spike runs:** a bounded, measurable investigation with a clear GO/NO-GO, a
`delonix-runtime-sec` audit of the ptrace/cgroup/namespace surface before any merge, and a strong
prior that CRIU is a host prerequisite (not vendored). A NO-GO is a perfectly good outcome —
documented, like the kind spike's original NO-GO — not a failure.

**Guardrail audit (this ADR):** daemonless ✅ (checkpoint is one-shot) · PaaS boundary ✅ (node-local
only; fleet DR stays out) · no new dependency ✅ (nothing added; CRIU deferred and preferred as a
host prereq) · **new privilege boundary → spike + `delonix-runtime-sec` audit required before any
implementation** (#5) · no silent failure ✅ (the stub returns explicit `Unimplemented`).

## Open decision for the owner

**Recommendation: keep Proposed and do not schedule the spike yet.** Checkpoint/restore earns the
spike only behind a concrete need — live migration, fast stateful restart, or forensic
checkpointing a security workflow actually requires. Absent that, the stub is the right state and
this ADR is the record of *why we are not building it*. When a need appears, promote to a spike
(its own session, GO/NO-GO discipline) before any implementation ADR.
