# ADR-0025: `delonix-mcp` is a local, tenancy-free AI control surface — not a remote management API

- **Status:** Accepted (2026-08-29)
- **Date:** 2026-08-29
- **Deciders:** Walter (owner)
- **Related:** ADR-0010 (remote management API, Rejected), ADR-0003 (tenancy-free capability
  model, Proposed), `crates/delonix-mgmt/src/lib.rs` (the existing local control socket),
  `docs/adr/README.md` guardrails.

## Context

A request came in to build a full Model Context Protocol (MCP) server so AI agents (Claude,
other MCP clients) can discover, inspect, diagnose and operate Delonix resources. The request as
written asked for OAuth/OIDC, `tenant`/`project`/`environment` scoping, IAM-style authorization
scopes (`runtime:admin`, `security:admin`, ...), and a remote HTTP listener with subscriptions.

That is the exact shape ADR-0010 already evaluated and rejected: "a fleet control-plane —
something managing many nodes, needing concurrency, identity and audit. This is `delonix-paas`.
Guardrail #2 says a notion of tenant does not live here." Reopening ADR-0010 requires, in its own
words, "a consumer that is neither the PaaS nor a local agent, named concretely" — an AI agent
running as a local MCP client (Claude Code, Claude Desktop, Cursor, VS Code) is squarely
ADR-0010's **candidate 3: a local agent on the same host**, already served in spirit by the
existing `delonix-mgmt` unix socket.

Separately, ADR-0003 (a tenancy-free `Capability`/risk model at the control-socket boundary) is
Proposed and explicitly recommends staying Proposed "until a lower-trust local socket consumer
exists." An MCP server driven by an LLM, which must not be trusted with the same blast radius as
the operator invoking `delonix` directly, is that consumer.

The `delonix-adr` guardrails that bind this decision: **#1 daemonless by design** (no permanent
background service), **#2 no tenant/licence/billing/IAM**, **#4 engine crates stay
dependency-clean** (heavy deps confined to non-engine crates, as `delonix-mgmt`/`delonix-truenas`/
`delonix-proxmox` already do).

## Decision

`delonix-mcp` ships as a new crate, in the same category as `delonix-mgmt`/`delonix-truenas`/
`delonix-proxmox` — outside the eight dependency-clean engine crates, carrying its external SDK
dependency (`rmcp`) and `tokio`.

1. **Transport: stdio is the supported path.** `delonix mcp serve` (default
   `--transport stdio`) is a foreground process, spawned as a child of the AI client for the
   duration of one session, and exits when its stdin closes — the same category as any other CLI
   invocation, not a persistent daemon. This does not trip guardrail #1.
2. **No tenant, no OAuth/OIDC, no IAM scopes.** The single principal is "the local uid running
   `delonix mcp serve`" — the same trust boundary `delonix-mgmt` already uses for its unix socket
   (`SO_PEERCRED` uid-equality, `crates/delonix-mgmt/src/lib.rs`). A `tenant`/`project`/
   `environment` field would put a notion of tenant in this repo, which guardrail #2 forbids.
3. **A remote, multi-tenant, OAuth-authenticated HTTP variant is explicitly out of scope for this
   repo.** If that surface is ever needed, it is a `delonix-paas` concern layered on top of this
   local server (or of `delonix-mgmt`), the same way `delonix-paas`'s `RemoteRuntime` already
   layers on top of `delonix-mgmt` without linking its crates. Building it here would repeat
   exactly the mistake ADR-0010 already named and rejected.
4. **Loopback-only HTTP is deferred, not built in this pass.** If a local IDE integration needs
   an HTTP transport instead of stdio, it binds `127.0.0.1` only, authenticates with a local
   bearer token file (`0600`, under `$DELONIX_ROOT/mcp/token` — the same trust model as a Jupyter
   notebook token), and carries no tenant concept. This is recorded here as the shape a future
   ADR amendment would take, not implemented now.
5. **Risk model, scoped to this new crate only.** `delonix-mcp` defines its own
   `RiskLevel { Read, SafeWrite, Disruptive, Destructive, Privileged }` and a per-tool table.
   Tools above `SafeWrite` require the call to carry `confirm: true`, or the server returns a
   structured `POLICY_DENIED` error. This is **not** wired into `delonix-runtime-core` or
   `delonix-mgmt` in this pass — that keeps the blast radius of this ADR to a new crate, not a
   change to already-shipped code. If `delonix-mgmt` later wants the same enforcement, promoting
   this table (or ADR-0003's `Capability` enum) to `delonix-runtime-core` is the natural next
   step, under its own ADR.
6. **Tools call domain APIs, never a constructed shell string.** Reads go straight to the same
   `Store`/domain-crate calls `delonix-mgmt` already uses (`delonix_runtime_core::Store`,
   `delonix_vm::list`/`status`, `delonix_volume::VolumeStore`, `delonix_net::infra::status`). The
   one MVP mutation (`container.restart`) reuses the exact pattern `delonix-mgmt` already ships
   for its own mutations: invoke the `delonix` binary itself (`std::env::current_exe()`) with a
   fixed, allowlisted argv (`Vec<String>`, never shell-interpolated) — not a new mechanism, the
   same one this repo already accepted in `delonix-mgmt::run_cli`.
7. **Tasks are in-process and session-scoped.** No persisted job queue (that would be
   daemon-shaped infrastructure outliving the session). A mutation spawns a `tokio::task`, tracked
   in an in-memory registry for the lifetime of the `delonix mcp serve` process.
8. **Audit is a local append-only log, not a fabricated "canonical pipeline."** No central
   Delonix audit pipeline exists in this repo (that is a `delonix-paas`/platform concept). One
   JSON line per tool call, appended to `$DELONIX_ROOT/mcp/audit.log` (`0600`), with secrets
   redacted before logging.

## Alternatives considered

- **Build the full spec as originally written** (OAuth/OIDC, tenant scoping, remote HTTP with
  subscriptions). Rejected: this is exactly ADR-0010's "candidate 1" (fleet control-plane), which
  that ADR already assigned to `delonix-paas`. Building it here would duplicate, and likely
  contradict, a decision this repo already made deliberately.
- **Do nothing until a new ADR explicitly reopens ADR-0010** with a concrete non-PaaS,
  non-local-agent consumer. Rejected for now: a local AI agent is a real, immediate consumer that
  ADR-0010 already anticipated (candidate 3) and ADR-0003 already anticipated (the "lower-trust
  local socket consumer"), so there is no need to wait for a hypothetical remote consumer to
  justify the local-only version.
- **Wire the risk/capability model directly into `delonix-runtime-core` and `delonix-mgmt` now**,
  promoting ADR-0003 to Accepted in the same pass. Deferred: it would touch already-shipped code
  in the same PR that introduces a brand-new crate and an external SDK dependency, widening the
  blast radius of a single change beyond what this ADR needs to decide. Left as an explicit
  follow-up.

## Consequences

**Easier**: an AI agent can now discover and read Delonix state (containers, VMs, volumes,
network topology, metrics) through a typed, schema-described interface instead of shelling out
to the CLI and scraping text — while staying inside the same trust boundary the CLI itself
already has (whoever can run `delonix mcp serve` could already run `delonix` directly).

**Harder / deferred, explicitly**: no remote access, no multi-agent/multi-tenant scoping, no
OAuth — an operator who wants a fleet-facing AI control plane needs `delonix-paas` to build it,
consuming this server or `delonix-mgmt` the way `RemoteRuntime` already does. `events.list` and
`security.explain` are not implemented in this pass (no backing event log or policy engine exists
in this repo yet) and return a clear "not implemented" error rather than fabricated data.
`gitops.*`, `backup.*`, `vm.snapshot`, `storage.snapshot`, `network.trace`,
`network.policy_check`, subscriptions, and loopback HTTP transport are follow-up work, each
warranting its own PR once the walking skeleton here is in.

**Guardrail audit**: daemonless ✅ (stdio is session-scoped, not a background service) · no
tenant/licence/billing ✅ (no such field exists anywhere in this crate) · engine crates
dependency-clean ✅ (`delonix-mcp` is outside the eight pure engine crates, same category as
`delonix-mgmt`) · no silent failure ✅ (unimplemented tools return a typed "not implemented"
error, never fabricated data; destructive/disruptive tools refuse without `confirm: true` rather
than executing quietly).
