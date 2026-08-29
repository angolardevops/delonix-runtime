# ADR-0026: The security runtime is a decision crate, not a sensor platform — and it has no tenants

- **Status:** Proposed
- **Date:** 2026-08-29
- **Deciders:** Walter (owner)
- **Related:** ADR-0010 (remote management API, **Rejected**), ADR-0003 (tenancy-free capability
  model, Proposed), ADR-0025 (local MCP control surface, Accepted), `docs/adr/README.md`
  guardrails, `crates/delonix-runtime-bin/src/cmd/policy.rs`, `crates/delonix-net/src/bpf.rs`.

## Context

A canonical brief asked for a production-grade `delonix-security-runtime` crate providing the
engine's full "Runtime Security Fabric": admission, eBPF sensors, malware and behavioural
ransomware detection, file-integrity monitoring, network runtime detection, storage and backup
protection, an automated response engine, evidence preservation, AI/MCP action policy, a security
score, SIEM export, and multi-tenant scoping of every event, incident and policy.

Measured against `origin/main` at **v0.69.0 (`68da3b8`)** before deciding anything:

**What already exists**, and would have been duplicated:

| Brief section | Already in the tree |
|---|---|
| Admission | `cmd/policy.rs` — fail-closed node policy, 4 fields, 301 lines |
| Supply chain | `delonix_image::verify_signature` (cosign) + `delonix-scan` SBOM/CVE (939 lines), wired as a `scan-on-pull` gate |
| Security profiles | `delonix-runtime/src/seccomp_profile.rs`, 756 lines, OCI format |
| Capability ceiling | `delonix-cri/src/cap_ceiling.rs` (`DELONIX_CRI_CAP_CEILING`) |
| Event pipeline | `delonix-runtime-core/src/events.rs` — append-only, lock-free, daemonless |
| Secrets | `core/secret.rs` + `core/cred_vault.rs` (XChaCha20 at rest) |
| eBPF | `delonix-net/src/bpf.rs` — tc/clsact loader, pinned map |

**The measured gap**, which nobody asked about and matters more than most of the brief:
`policy::enforce` has **exactly one caller**, `cmd/container.rs:2944`. `cmd/vm.rs` has none — its
thirteen matches for "policy" are all `restartPolicy`. So a node that set `denyPrivileged: true`
refused `container run --privileged` and accepted `vm create --device 0000:01:00.0`, which gives
the guest DMA to host hardware. It also accepted `vm create --url-img https://anywhere/x.qcow2`,
whose own CLI help admits that without a sibling `.sha256` the download is "trusted on TLS alone".
Whoever admits was not one point.

**Three parts of the brief are already adjudicated in this repo**, twice against, once today:

- **Tenancy.** ADR-0010 (Rejected, 2026-08-10) put a fleet control-plane in `delonix-paas`.
  ADR-0003 states "identity + tenant + audit-context-per-request is out of bounds for this repo."
  ADR-0025 (Accepted, 2026-08-29) says a `tenant`/`project`/`environment` field "would put a
  notion of tenant in this repo, which guardrail #2 forbids." The brief's §41 requires exactly
  that field on every event, incident, policy, scan and response.
- **The AI/MCP action policy.** ADR-0025 shipped the local half — `RiskLevel { Read, SafeWrite,
  Disruptive, Destructive, Privileged }` and `confirm: true` above `SafeWrite`. The approval half
  needs an approver, which needs an identity, which is the same boundary.
- **Continuous detection.** Guardrail #1 is daemonless by design. eBPF sensors, FIM watchers and
  network anomaly detection are resident processes.

And the eBPF half has a measured problem, not only a doctrinal one: `delonix-net/src/bpf.rs`
documents that loading a program needs `CAP_BPF` + `CAP_NET_ADMIN` in the init namespace, which a
rootless runtime does not have, so `available()` returns false and the module no-ops. The
engine's primary mode is rootless (`AGENTS.md` §2). A sensor layer on that footing would be inert
exactly where this engine usually runs.

## Decision

**Ship `delonix-security-runtime` as a pure decision crate: what the node refuses, why, how bad
it is, and what event that produces. No sensors, no residency, no tenants.**

1. **The crate is the single admission point, and the VM path joins it.** `admission::evaluate`
   takes a `Request` that names its `Workload`, so a rule can say "container only" and mean it.
   `cmd/vm.rs`'s `Create` arm now calls `policy::enforce` before the image is resolved, fetched or
   written — the same placement, and the same reason, as `cmd_run`.
2. **The VM rules are NEW fields, every one default-off.** The shortcut — let `denyPrivileged`
   also refuse `--device` — was rejected for the reason `cmd/policy.rs` states in its own doc:
   turning a setting into something that refuses more breaks existing hosts on upgrade. An
   operator who wrote `denyPrivileged: true` last month consented to a rule about containers.
   The new fields are `denyDevicePassthrough`, `denyLatestVmImage` and `allowedImageUrlHosts`.
3. **`SecurityPolicy::lint()` reports the half the operator left open**, by name, with a stable
   id, on the path they are using, once per process, silenceable with `DELONIX_POLICY_LINT=0`.
   This is what turns three default-off fields from an option nobody discovers into a gap the node
   reports on itself. Lints **warn, never reject**: a contradiction that refused to load would
   break hosts on upgrade, which is the failure mode the whole module avoids.
4. **No tenant, and a test rather than an intention.** `boundary_tests` fails the build if a
   `tenant`/`project`/`environment` field appears in any file of the crate, and the event
   round-trip test asserts the words are absent from the serialised JSON.
5. **No new bus.** A `SecurityEvent` rides `core::events` under `kind = "security"`. The file is
   the bus, as it already is for lifecycle events.
6. **No `RequireApproval` decision.** Approval needs an approver; approvers need identity;
   identity is `delonix-paas`. A variant this repo cannot produce would be a promise in a type
   signature.
7. **Dependencies: three** — `delonix-runtime-core`, `serde`, `serde_json`. No YARA, no ClamAV, no
   sigstore SDK, no `aya`/`libbpf`. The crate decides; it does not scan.
8. **The layer with tenants wraps this one.** `delonix-paas` already consumes
   `delonix_runtime_core` (`crates/delonix-core/src/policy_store.rs`) without linking anything
   private back into this repo. Its operator-facing policy store is documented as store-only
   (`enforced: false`, "enforcement in the apply path is PHASE 2"); this crate is what that phase
   can evaluate with, once it adds the tenant envelope on its own side.

## Alternatives considered

- **Build the brief as written, here.** Rejected: it requires a tenant field that three ADRs put
  in `delonix-paas`, a resident sensor process that guardrail #1 forbids without its own ADR, and
  an eBPF layer measured to be inert in this engine's primary mode. It would also have rewritten
  `cmd/policy.rs`, `delonix-scan`, `seccomp_profile.rs` and `cap_ceiling.rs` — 2 000+ lines of
  working, tested code — which is the "start by deleting what works" proposal the architecture
  rule rejects at review.
- **Put the whole thing in `delonix-paas` instead.** Rejected: the engine would keep no admission
  of its own, and a `crictl` talking straight to the socket, or somebody typing `delonix vm create`
  on the host, would bypass a control that lives on another machine. That is the exact argument
  `cmd/policy.rs` already makes for why the ceiling is local.
- **Fold the VM rules into the existing container fields.** Rejected: see Decision 2. It closes the
  gap by breaking hosts on upgrade, and buries a behaviour change inside a bug fix.
- **Do nothing; the VM path has always been unguarded.** Rejected: the asymmetry is not a missing
  feature but an incoherent one — the node refuses the narrower hole and permits the wider.
- **Add a `delonix security` command group now.** Deferred: PR #161 is restructuring the CLI's
  top-level surface. A new command group landing in the middle of that is a merge conflict with no
  upside; the crate is reachable through `policy.json` today.

## Consequences

**Easier.** One evaluation for containers and VMs, with typed violations carrying stable rule ids
(`ADM-DEVICE-PASSTHROUGH`) that survive a change of wording and can be alerted on. A node can now
refuse VFIO passthrough and unvetted boot-image hosts, which it could not before. Redaction,
scoring and severity are pure functions with tests, so the layer above gets them for free.

**Harder, and stated.** Three fields default off means an operator who upgrades and reads nothing
gets the lint, not the protection — the deliberate trade against breaking their fleet. The event
log is `events::emit`, which is best-effort and infallible by design: it detects nothing about its
own gaps and anyone with write access to the state root can edit it. **This is operational signal,
not evidence.** The tamper-evident trail is the hash-chained Ed25519-anchored audit log, and that
lives in `delonix-paas`.

**Explicitly not built, and why**, so no reader mistakes the crate's name for its scope: eBPF
sensors, file-integrity monitoring, malware scanning, behavioural ransomware detection and scoring,
network anomaly detection, the automated response engine (freeze/isolate/quarantine/rollback),
evidence preservation, storage and backup protection, SIEM/syslog/webhook export, the security
score's supply-chain and network inputs, `RequireApproval`, incidents, and every tenant-scoped
concern in the brief. The first six need a resident process; that is a change of product
philosophy and it needs its own ADR carrying a GO/NO-GO spike that measures what such a sensor can
actually observe **rootless** — the number `bpf.rs` implies is zero, and a design should not be
built on an implication.

**Guardrail audit.** Daemonless ✅ (pure functions; the only I/O is one append to an existing log).
No tenant/licence/billing ✅ (asserted by a test, not by review). No private dependency ✅. Engine
crates dependency-clean ✅ (three deps, two of them already universal here). No silent failure ✅
(an unreadable policy is an error; an unparseable boot URL is refused, not ignored; `mode: warn`
says out loud that it did not stop the request). Spike before a new privilege boundary ✅ by
avoidance — this ADR adds no privileged code path, and defers the one that would to its own ADR.
