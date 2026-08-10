# ADR-0010: What it would take for the management API to be remote

- **Status:** Proposed — this ADR frames the decision, it does not take it
- **Date:** 2026-08-10
- **Deciders:** Walter (owner)
- **Related:** `crates/delonix-mgmt/src/lib.rs`, `docs/cli-stability.md`
  (`serve api` declared not stable), `docs/discovery/47_IAC_REVISAO.md` §F4/§F5,
  ADR-0008 (a *VM backend* that talks to a remote host — a different question,
  see below).

## Context

The IaC review named two findings that are really one: the management API is a
small slice of the engine (**F4**), and it has no contract and no remote access
(**F5**). F4 is not worth closing before F5 is decided — widening an API whose
shape and audience are undecided produces surface that then has to be kept.

**Measured, not assumed** (`delonix-mgmt/src/lib.rs`):

- **Transport**: a unix socket only. `serve api --addr` accepts a path or
  `unix://<path>`; there is no TCP option anywhere in the crate.
- **Authentication**: `SO_PEERCRED`, and the peer must be **the same euid** as
  the server. That is not a weak check — it is the strongest one available for a
  local socket, and it is there because this is the highest-privilege surface in
  the runtime: `POST /v1/containers/:id/exec` is arbitrary code execution inside
  any container.
- **Coverage**: volumes CRUD; containers list/get/run/rm/action/logs/exec and a
  partial reconfig; images list/rmi/pull/build/scan/sbom; networks create/rm;
  VMs `action` only. `RunSpecBody` has 11 fields against `container run`'s 71.
  No route applies a manifest.
- **Contract**: no OpenAPI, no JSON Schema (grepped: no hits). `cli-stability.md`
  classifies it as *not stable*.

So the API is a **local control-plane socket for one process to drive another on
the same host**, which is exactly what its module doc says it is (the PaaS's
`RemoteRuntime` consuming it without linking the crates). It is not a
half-finished remote API; it is a finished local one.

## The question this ADR exists to frame

Terraform and Ansible normally run **off** the machine they manage. The review's
conclusion was that the shortest honest path for both is **SSH + the CLI** — no
remote API needed, because `stack plan`/`apply` with `-o json` and
`--detailed-exitcode` already give a provider everything it needs. That path is
now built.

So the question is not "how do we finish the API". It is:

> **Is there a consumer that the CLI-over-SSH path does not serve?**

Three candidate answers, and they are not equally good:

1. **A fleet control-plane** — something managing many nodes, needing
   concurrency, identity and audit. This is `delonix-paas`. Guardrail #2 says a
   notion of tenant does not live here.
2. **A single remote operator** — one person, one node, no SSH available.
   Rare, and SSH is the answer everywhere else in this engine (`cluster apply`
   shells out to `ssh`).
3. **A local agent on the same host** — already served, by the socket as it is.

**If the honest answer is (1), then this API should not become remote at all**
— it should stay the local socket the PaaS consumes, and the remoteness belongs
one layer up. That is the outcome this ADR expects, and writing it down is worth
more than the code it avoids.

## What a decision to go remote would have to settle

Recorded so that a future session does not rediscover them:

1. **Identity.** `SO_PEERCRED` has no remote analogue. mTLS, or a bearer token
   from `kind: Secret`? A token means a revocation story and an expiry story.
2. **Authorization.** Today there is one privilege level: the owning uid. A
   remote API needs at least "can read" vs "can exec", and the moment it needs
   "can exec **as this tenant**" it has crossed into the PaaS.
3. **Transport.** TCP means TLS, which means certificate lifecycle on a runtime
   that deliberately has no daemon.
4. **The contract.** F4's widening only makes sense with an OpenAPI document and
   a stability promise; without them, a consumer is building on sand — the same
   argument that made `docs/schema/v1/delonix.json` generated rather than
   written.
5. **Audit.** A local socket's caller is a uid on this host. A remote caller is
   not, and "who ran `exec` in that container" becomes a question the engine
   cannot currently answer.

## Not to be confused with ADR-0008

ADR-0008 adds a **VM backend that talks to a remote Proxmox host**. That is the
engine acting as a CLIENT of somebody else's API, addressed explicitly, with
credentials from a `kind: Secret`, and no notion of tenant. This ADR is about
the engine being a **SERVER** for remote callers, which is where identity,
authorization and audit all appear at once. The first does not imply the second.

## Consequences of leaving it Proposed

**Nothing breaks.** The API keeps working for its real consumer, and
`cli-stability.md` already tells everyone else not to build on it. The cost is
that F4 stays open — which is correct, because widening it is only worth doing
once (2) or (3) above is the answer, and today the evidence says it is (1).

**Guardrail audit:** daemonless ⚠️ — a remote API is a listening service, and
the socket is already one; going remote makes it a *network* service, which is a
different promise · PaaS boundary ⚠️ — this is precisely where the line is, and
the ADR's expected outcome is that the line holds · no silent failure ✅ — the
current state is documented as local and not stable, rather than left to be
discovered.
