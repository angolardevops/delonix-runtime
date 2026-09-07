# ADR-0033: OCI runtime hooks stay unimplemented — no concrete consumer, and the security class is the one this repo already gates behind a spike

- **Status:** Proposed
- **Date:** 2026-09-06
- **Deciders:** Walter (owner)
- **Related:** `docs/runtime/runtime-architecture.md` §"Plugin Engine" (dynamic loading flagged as
  "large security & supply-chain surface... its own ADR + spike"), `crates/delonix-runtime/src/
  lib.rs` (`StartedHook`, the CDI `ldconfig -r` comment that first named this gap), ADR-0003
  (tenancy-free capability model — same "wait for a real consumer" discipline), the kind/
  `--privileged` GO/NO-GO spike precedent.

## Context

Comparing this engine's OCI conformance against the spec surfaces one real gap: the **OCI Runtime
Spec hook protocol** — `prestart` (deprecated alias `createRuntime`/`createContainer`/
`startContainer`/`poststart`/`poststop`), each a host-side binary a `config.json` names, invoked by
the runtime at the matching lifecycle point with the container's state piped as JSON on stdin.

**This is not undiscovered territory — it was already found and worked around once.** The CDI
(`--gpus nvidia`) code says so directly, in a comment written before this ADR existed:

> a deliberately simpler substitute for executing a real CDI spec's own `createContainer` hook
> (`nvidia-ctk hook update-ldcache`, which needs the OCI-hook-stdin-state protocol this engine
> doesn't implement).

So `nvidia-ctk`'s own hook already goes unexecuted, papered over by a direct `ldconfig -r` call.
That is the ONE place today where the absence has a visible, named cost.

**What already exists, and is a different thing.** `StartedHook<'a> = dyn Fn(i32) -> Result<()>`
(`lib.rs`) is an in-process Rust closure invoked with the init PID, right after start — used today
to configure CNI-style networking before the container's own `waitpid`. It solves the same *shape*
of problem (do something at a lifecycle point) but is not the OCI protocol: no external binary, no
JSON-over-stdin state, no operator-configurable hook list, and — critically — no new privilege
boundary, because it is code this engine already trusts, not a third party's binary chosen by
whoever authored the container spec.

**Why this is the same security class the architecture doc already gates.** `docs/runtime/
runtime-architecture.md`'s Plugin Engine row calls dynamic plugin loading "a large security &
supply-chain surface for a container runtime" and requires "its own ADR + spike" before building
it. A `prestart`/`poststop` hook is a lighter-weight version of the identical concern: it hands
execution of an operator- or image-author-named host binary to a lifecycle event this engine
controls, with no code review, no supply-chain provenance check, and (for `prestart`) a moment
BEFORE the container's own namespaces/cgroups/seccomp are fully in place — exactly the kind of
early, high-trust execution point this repo's own security audits (delonix-runtime-sec) have found
real bugs in before (command injection via unsanitized manifest fields, path traversal in `COPY`).
A hook is arbitrary command execution tied to OUR lifecycle, not the workload's.

**What actually needs it, concretely, today: nothing named.** `nvidia-ctk`'s hook is already
worked around (`ldconfig -r`, documented as deliberately simpler). CNI is invoked through
`delonix-net::cni` directly (ADD/DEL/CHECK over its own protocol, not via an OCI hook — see
`delonix-net/src/cni.rs`). No CRI conformance suite failure names hooks (confirmed: `docs/
cri-conformance.md` has zero occurrences of "hook", and CRI does not go through OCI hooks anyway —
that is a runc/crun-level concept the CRI shim below the kubelet is agnostic to).

## Decision (proposed)

**Do not implement the OCI hook protocol now.** Keep the CDI/`nvidia-ctk` workaround as-is (already
documented, already correct for the one case that needed it). Revisit only when a CONCRETE
consumer needs a real third-party OCI hook binary that has no native equivalent already built —
not "CNCF completeness" as a goal in itself.

When a real trigger appears, the implementation has a known shape, so it's worth recording:

1. **Parse `config.json`'s `hooks` object** (already round-tripped through `oci_spec::runtime::Spec`
   for `image export`'s bundle — the type exists, just unused for hook execution on the native
   path). No new dependency: `oci-spec` is already in the tree.
2. **Serialize the OCI `State` JSON** (`ociVersion`, `id`, `status`, `pid`, `bundle`) and pipe it to
   the hook's stdin — the wire format the protocol requires, so a third-party hook binary (written
   against the real spec) works unmodified.
3. **Order matters and is spec-fixed**: `createRuntime` before the container's mount namespace is
   set up, `createContainer` after mounts but before `pivot_root`, `startContainer` right before
   `execve`, `poststart` after start (this engine's `StartedHook` slot), `poststop` after full
   teardown. Each is a NEW point in `spawn()` — already flagged in `AGENTS.md` as a ~405-line
   function that is "a real risk of maintenance," and hooks would be five more insertion points in
   it, not a bolt-on.
4. **Timeout and failure semantics are spec-defined** (a hook that doesn't exit is a runtime error;
   a non-zero exit fails the operation) and have to be enforced — an unbounded `Command::spawn` +
   `wait` here would be the same class of bug this repo's own history has paid for elsewhere (the
   `run -d` mount-wait race, the DNS control-socket timeout that had no failure path).
5. **A CAPABILITY gate, not just a config flag**, given the ADR-0026 (`delonix-security-runtime`)
   precedent: a node that wants to allow zero, some, or all hooks should be able to say so
   node-locally, the same shape as `DELONIX_CRI_CAP_CEILING`. Without this, hooks become the
   quietest possible way to get arbitrary host execution past every other guard this engine has.

## Alternatives considered

- **Implement now, ungated, as a compatibility checkbox.** Rejected: this is exactly the "we did it
  to say we did it" failure this repo's audits keep finding elsewhere (`publish_port_allow`,
  `reap_orphan_hostfwds`) — code with no real caller and no threat model, sitting there as a footgun
  for the day someone does call it.
- **Extend `StartedHook` (the in-process Rust closure) to also mean "OCI hook."** Rejected: they
  solve different problems. The in-process hook is code THIS ENGINE wrote and trusts; an OCI hook
  is a THIRD PARTY's binary named in someone else's manifest. Conflating them would make it
  impossible to reason about which lifecycle callbacks are "ours" vs. "operator-supplied,
  untrusted."
- **Wait for a GO/NO-GO spike like `--privileged`/kind or rootless-CRIU (ADR-0004).** Considered
  and rejected as the WRONG shape for this specific gap: those spikes exist because the technical
  feasibility was genuinely unknown (does rootless CRIU even work here?). Hook execution has no
  such unknown — `Command::spawn` + stdin JSON is ordinary, proven code in this very repo. What's
  missing is not feasibility, it's a NAMED CONSUMER whose need justifies opening the privilege
  surface. That is this ADR's actual gate, not a spike.

## Consequences

- The CDI/`nvidia-ctk` comment (`lib.rs`) stays accurate and does not need correcting — it already
  says the right thing.
- `docs/runtime/current-state.md`/`runtime-architecture.md` gain a citation to this ADR wherever
  OCI conformance is discussed, so "hooks are unimplemented" reads as a decision, not an oversight.
- No code changes. No new dependency. No new privilege boundary.

## Not done here, and why

- **Implementation.** No concrete consumer is named; building it now would be exactly the
  "accepted and ignored is worse than missing" failure this repo refuses by policy, aimed at a
  DIFFERENT axis (a hook nobody calls is not "ignored input," but it is unaudited attack surface
  installed for nothing).
- **A capability-gate design for hooks specifically.** Sketched above as the shape a real
  implementation should take, not designed in full — that is real work for whenever this reopens.
- **Auditing whether any *other* CNCF tool this project might integrate with (service mesh
  sidecars, admission-time mutators) specifically requires OCI hooks vs. a CRI/CNI-level
  integration point this engine already has.** That survey is the concrete-consumer search that
  would trigger reopening this ADR, and it has not been done.
