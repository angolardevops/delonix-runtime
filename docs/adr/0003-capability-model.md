# ADR-0003: A tenancy-free capability model at the control-socket boundary

- **Status:** Proposed
- **Date:** 2026-07-30
- **Deciders:** Walter (owner) + Chief Runtime Architect review
- **Related:** `docs/runtime/current-state.md` §7 (security posture), `docs/runtime/runtime-architecture.md`
  §4 (Security Engine row), `CLAUDE.md` "Regra de ouro" (#2 PaaS boundary), `delonix-runtime-sec` skill.

## Context

The product vision asks for "Zero Trust internally — every runtime request carries identity,
capability, tenant, resource, audit; no operation runs just because the caller is admin," with an
explicit capability list (`VM_CREATE`, `NETWORK_ATTACH`, `VOLUME_CREATE`, `LIVE_MIGRATION`, …).

Ground truth (confirmed in code):

- The **only** authorization today is `SO_PEERCRED` uid-equality at each control socket
  (`core::peer_cred::peer_uid`, `0600` socket) — the holder, `delonix-cri`, `delonix-mgmt`, and
  the docker-api slice all gate on "same uid as the server." Below that, Linux primitives
  (userns/seccomp/caps) enforce isolation. There is **no** application-level authorization: no
  `enum Capability`, no `authorize()`, no roles.
- The runtime is **single-uid, single-tenant by construction** (rootless, one user's daemonless
  processes). The "tenant" strings in the tree are NAS-share subdirectory comments, not a tenancy.

**The hard boundary (guardrail #2):** *identity + tenant + audit-context-per-request is out of
bounds for this repo* — it belongs to `delonix-paas`. So the vision's Zero-Trust-with-tenancy
cannot land here in full. What *can* fit is the **capability half without the identity/tenancy
half**: a way to restrict *which operations* a given control connection may invoke, decided by
node-local policy, not by "who the customer is."

The open question this ADR answers: **is a tenancy-free capability layer worth adding to a
single-uid rootless runtime, and if so, where does it sit without inventing a tenant?**

## Decision (proposed)

**Add an optional, default-off capability gate at the control-socket dispatch boundary — scoped
to the operation, never to an identity or tenant.** Keep `SO_PEERCRED` as the primary
authentication; capabilities are a second, coarser *authorization* filter layered on top, opt-in
via node-local config.

Shape (to be refined if accepted):

- **`enum Capability`** in `core` (the sink — it already holds the security primitives
  `peer_cred`/`secret`): a fixed, node-level verb set, e.g. `ContainerRun`, `ContainerRemove`,
  `VmCreate`, `VmRemove`, `NetworkCreate`, `NetworkAttach`, `VolumeCreate`, `VolumeRemove`,
  `ImagePull`, `Exec`. **No** `LiveMigration`/tenant/quota verbs (those are absent features or
  PaaS concerns).
- **A `CapabilitySet`** (default = **all**, so existing behaviour is byte-identical when the
  feature is unused) loaded from node-local config (`$DELONIX_ROOT/policy.toml` or an env var),
  never from the wire — the caller cannot grant itself a capability.
- **A single `require(cap)` checkpoint** in each server's dispatch (`delonix-cri`,
  `delonix-mgmt`, docker-api, the holder control-loop), right after the existing `peer_uid`
  check. Deny → a clear, fail-closed error naming the missing capability. This is the *only*
  new enforcement point; the CLI path (a human at their own uid) stays unrestricted by default.
- **Audit as the existing event log, not a new subsystem:** a denied (or capability-restricted)
  operation emits a `core::events` line (`kind=capability, action=denied`). The daemonless event
  bus (ADR-adjacent, already wired) *is* the audit trail — no new daemon, no per-request identity
  context.

Explicitly **excluded** (out of bounds / not this ADR): identity/principals, tenant scoping,
per-tenant quotas, mTLS/SSO, RBAC roles, `LiveMigration` (no such feature). If any of those is
ever needed, it lives in `delonix-paas`, above this runtime — this ADR does not open that door.

## Alternatives considered

- **Do nothing (status quo: `SO_PEERCRED` only).** Legitimate and possibly correct: on a
  single-uid rootless node, "same uid = full control" is already the Unix trust model, and a
  capability layer a single operator can edit adds ceremony without a threat it closes. **This is
  the null hypothesis the owner must reject before we build anything** — see "Open decision."
- **Full Zero-Trust with identity + tenant + audit context (the literal vision).** Rejected:
  violates guardrail #2 (tenancy is PaaS). Recorded here so the vision item is explicitly mapped
  to "out of bounds," not silently dropped.
- **Capabilities enforced in the CLI (`-bin`) instead of the servers.** Rejected: the CLI runs as
  the operator's own uid — restricting a human from their own runtime is theatre. The real
  boundary worth gating is the *socket* (where `delonix-cri`/`delonix-mgmt`/docker-api accept
  connections that may outlive an interactive session).
- **Capability set delivered per-connection over the wire.** Rejected hard: a caller that names
  its own capabilities is not a gate (fail-open by design). Policy must be node-local.

## Consequences

**If accepted:** a node operator can run, e.g., a `delonix-mgmt` socket that may pull images and
list state but may **not** remove volumes or exec into containers — useful when a
lower-trust control-plane component talks to the runtime. Enforcement is one `require()` per
dispatch arm; `core` gains an `enum` + a set type (no new dependency, no engine edge). Default-off
means zero behaviour change for every existing user until they write a policy.

**Cost / debt:** a new enforcement point in four dispatchers that must be kept in sync as verbs
are added (a `require()` forgotten on a new arm is a silent hole — mitigated by making the
dispatch table exhaustive over `Capability` where possible, and a `delonix-runtime-sec` audit of
the gate before merge). The capability↔operation mapping is a maintenance surface.

**Guardrail audit (proposed):** core gets the enum but no engine edge ✅ · **no tenant/identity**
✅ (the whole point — operation-scoped only) · no new dependency ✅ · daemonless ✅ (policy is a
file, audit is the existing event log) · no silent failure ✅ (deny is explicit; default-all keeps
current behaviour). A new privilege-adjacent boundary → **requires a `delonix-runtime-sec` spike
before implementation** (guardrail #5): can a caller bypass `require()` via a path that skips the
dispatch checkpoint? are all four sockets covered? does default-all truly reproduce today's bytes?

## Open decision for the owner

**Is this worth building at all on a single-uid rootless runtime?** The honest recommendation is:
**not yet — keep it Proposed.** `SO_PEERCRED` already matches the node's real trust model, and a
capability layer earns its keep only when a *lower-trust local consumer* of a runtime socket
actually exists (the same "wait for the second consumer" discipline as ADR-0002 Phase 2b). If/when
that consumer appears (e.g. a constrained `delonix-mgmt` exposed to a control-plane component),
accept this ADR, run the security spike, and implement the default-off gate. Until then, this ADR
stands as the *decided shape* so the feature is never bolted on ad-hoc under pressure.
