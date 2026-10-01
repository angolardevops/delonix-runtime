# ADR-0059: Network providers answer by role, through small ports negotiated by capability, and every change goes through validate → plan → apply → observe → verify

- **Status:** Accepted (2026-09-27, by the owner, on the text of PR #547) — from here the ADR
  is not rewritten: what F1–F6 find goes into dated addenda with evidence, and a change of
  decision is a new ADR
- **Date:** 2026-09-27
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0050 (the capability catalog this extends to 1.1.0 and to a fifth
  `ProviderKind`); ADR-0051 (`GatewayProvider` — moves, loses its refusing default methods,
  and its native builtin); ADR-0049 (the `kind: NetworkZone` addendum of 2026-09-25, whose
  resolution **by count** this ADR supersedes; D3, which keeps cluster-level administration
  out); ADR-0052 (the per-VM firewall of a Proxmox node — one of the lowerings of D6);
  ADR-0054 (`providers.yaml` — gains `type: opnsense` and `networkDefaults`); ADR-0044 D3/D4
  (the `Provider` skeleton and the registry rules this reuses verbatim); ADR-0040 D2.2 (the
  `delonix-networking` context this creates); ADR-0042 (problem+json, `ETag`/`If-Match`,
  `request_id` — the envelope D5 extends); ADR-0043 (the `DX-CDNN` dictionary); ADR-0024,
  ADR-0028, ADR-0055 (selector, origin bookkeeping and the per-VM anti-spoof opt-out — fields
  the IR of D6 keeps)
- **Evidence:** `docs/discovery/62_NAAS_FASE0_AUDITORIA.md` (PR #544: §4 capability matrix,
  §6 P2, §7 session S5, §10 question 3) and the spike of this ADR,
  `docs/discovery/64_NET_PROVIDER_CONTRATO_SPIKE.md`

## Context

### What the engine has today, measured on `origin/main` `d3d6f394`

Four network ports exist, each shaped by the provider that arrived first, and none of them
shares a lifecycle, an error envelope or a selection rule with another:

| Port | Where | Selection | Lifecycle | "Not supported" |
|---|---|---|---|---|
| `NetworkProvider` (attach, publish, per-container firewall) | `crates/contexts/delonix-compute/src/ports.rs:128` | one implementation | apply, no plan | n/a |
| `VmBackend::apply_firewall`/`read_firewall` | compute context (ADR-0052) | the VM's backend | apply + readback | default method refuses (DX-1501) |
| `GatewayProvider` | `crates/adapters/delonix-sdn/src/gateway.rs:126` | **by name** (`gateway_provider_for`, `spec.provider` required) | `ensure_*` + `commit` | four default methods refuse, `commit` defaults to `Ok(())`; the builtin `native` refuses everything |
| `NetworkZoneProvider` | `crates/adapters/delonix-sdn/src/network_zone.rs:66` | **by count** (`active_network_zone_provider`: 0 refused, 1 used, >1 refused) | `ensure_*` + `commit` | n/a |

The catalog (ADR-0050, 1.0.0) has 20 `network`-kind entries (16 `net.*`, 4 `firewall.*`) and
**no** entry for a perimeter gateway, NAT, a load balancer, a provider's DNS or a provider's
IPAM beyond the engine's own (`net.ipam`, `net.dns`). `ProviderKind` has four words. OPNsense
is registered from `DELONIX_OPNSENSE_*` only (`cmd/gatewayproviders.rs`), has **no report**,
so `delonix provider ls` and the node contract's `ListProviders` do not show it, and
`providers.yaml` (ADR-0054) does not know its type. The published matrix
(`docs/providers/capability-matrix.md`) gives `linux` 7 of 20 network entries supported and
`proxmox` 0 of 20 — the Proxmox SDN client has live-tested IPAM, DNS, fabric and lock routes
(ADR-0049 slice 2, PR #542) that no port reaches, so the report is right to say
`not-implemented`.

`kind: NetworkZone` resolves its provider by count (ADR-0049 addendum, 2026-09-25): it works
because this build registers one, and it becomes a refusal the day a second segment provider is
linked — the one situation a by-name rule exists for. Its record does not even keep which
provider served it (`NetworkZoneRecord`, `bins/delonix-runtime-bin/src/cmd/network_zone.rs:71`),
although the port's doc comment says it does.

There are **three** representations of a firewall rule: `delonix_model::records::FwRule`
(stringly typed, persisted, containers), `delonix_compute::vm_firewall::{Policy, Rule}` (typed,
refuses an unknown protocol, VMs) and `delonix_sdn::gateway::GatewayRule` (stringly typed,
OPNsense). Nothing checks that the same intent means the same thing on each, and audit 62 found
the nft one fail-open in five ways (P0-1…P0-5), which session S1 corrects.

### What the brief asks, translated to this repository

The canonical prompt (§10–§12, §18, §25, §35) describes a NaaS with tenants, network classes,
quotas, provider scoring and persisted bindings. `AGENTS.md` forbids the engine to know any of
that (guardrail 2). Audit 62 §0 already translated it; this ADR keeps to the engine's half:

| Brief | This engine | Not this engine |
|---|---|---|
| role contracts, capability document, `unsupported_capability` | D1, D2 | — |
| validate / plan (digest) / apply / observe / verify | D4 | cancel of a long-running operation, import/adopt (later) |
| stable error envelope | D5 | tenant-facing redaction policy |
| policy IR and compiler | D6 | tenant/admin ownership, expiry, NetworkClass guardrails |
| selection | **a named target** or the node's configured default (D3) | scoring, candidates, `ProviderBinding` persistence, migration between providers (§10.1–10.5) |

### Guardrails this decision touches

1. **Daemonless** — the lifecycle runs inside the invoking process; `observe` is on demand;
   nothing watches.
2. **No consumer** — no tenant, class, quota or binding; a provider is chosen by a name the
   node's operator configured or the caller passed.
4. **Engine crates dependency-clean** — the new context crate takes `serde`/`serde_json`
   (already in every context crate) and `sha2` for the plan digest. `sha2` is already in the
   lockfile (`delonix-oci`, `delonix-mcp`, the `-bin`), so no new package enters the tree,
   but it enters a context crate for the first time — this ADR is the record of that.
6. **No silent failure** — the whole point of D1 (no no-op defaults), D3 (no fall-through
   default), D4 (`stale_plan`, `partial_apply`, control-plane-only verification said out loud)
   and D6 (a rule a lowering cannot represent is refused, never widened).

## Decision

### D1. One small port per role, born only with its first implementation, with no default bodies

The engine's network roles are: **segment, port, ipam, route, gateway, firewall, nat, lb,
dns**. A role gets a port when — and only when — a provider implements it:

| Role | Port | First implementation | Phase |
|---|---|---|---|
| segment | `SegmentProvider` (replaces `NetworkZoneProvider`: zone + VNets) | Proxmox SDN | F2 |
| gateway | `GatewayProvider` (moved; filter rules + aliases) | OPNsense | F2 |
| nat | `NatProvider` | OPNsense `firewall/d_nat`, `source_nat`, `one_to_one`; Proxmox subnet `snat` | F5 |
| ipam | `IpamProvider` | Proxmox SDN IPAM (`pve`/`netbox`/`phpipam` plugins) | F5 |
| dns | `DnsProvider` | Proxmox SDN DNS (`powerdns`) | F5 |
| lb | **none** — catalog rows only | none exists; OPNsense 26.1.2 ships no LB API (spike §4) | excluded |
| port, firewall (per workload) | **unchanged**: `NetworkProvider::attach`/`apply_firewall`, `VmNetwork`, `VmBackend::apply_firewall` | native, libvirt, Proxmox | unified by D6's IR, not by a new port |
| route, fabric, telemetry | none | no remote implementation reachable | excluded |

Rules every role port keeps:

1. **Every method is required.** No default body — neither a refusal nor an `Ok(())`. A
   provider that cannot do a role does not implement its trait.
2. **A provider serves a role by registering in that role's registry.** One registry per
   role, keyed by the provider's registry id, with ADR-0008's four rules (no I/O at
   registration, `auto_selectable` a fact of the registration, third-party opt-out, owned
   names). One `providers.yaml` entry may register in several registries — as the Proxmox
   target already registers a `VmBackend` and a `NetworkZoneProvider` from one configuration.
   Asking a role of a provider that is not in that registry is **`unsupported_capability`**,
   raised by the resolver and naming role and provider.
3. **Inside a role, optional behaviour is a catalog entry, checked before the call.** The use
   case compares the capabilities the intent needs with the provider's report (ADR-0050 D6's
   comparison, extended to network Kinds) and refuses by name before any effect. An adapter
   reached anyway returns the typed `UnsupportedCapability { capability }` — never success.
4. **Every role port extends ADR-0044 D3's `Provider`** (`id`, `capabilities() ->
   ProviderReport`, `health`) — the skeleton `delonix_compute::vm_provider::Provider` already
   has; no second report type.
5. **Identity is an ownership marker, not a name.** Every object a role port creates on a
   remote provider carries an immutable marker the port reads back before it updates or
   deletes; an object with the right name and no marker is `provider_conflict`, never adopted.
   Session S6 (audit 62 §7) builds the markers — OPNsense `categories`, Proxmox SDN comments —
   and this ADR makes them a precondition of F2's writes.

The builtin `native` gateway provider is **removed**: it refused every alias and rule and its
`commit` did nothing. The native masquerade and DNAT publish stay where they are
(`delonix-sdn`), and the Linux network report answers the new `net.nat.*` rows for them.
`kind: NetworkGateway` with `provider: native` becomes `unsupported_capability` (see D5 for
the class change).

### D2. The catalog grows to 1.1.0, and `ProviderKind` gains `gateway`

This answers audit 62 §10 question 3: **the existing catalog**, not a second document. A
second document would be a second denominator and would lose the property ADR-0050 D1 is
built on — a provider cannot skip a row because the compiler walks the enum. Adding entries is
a minor bump (ADR-0050 D1): **1.0.0 → 1.1.0**. New names keep the `net.` prefix so that
`net.ipam` and `net.dns` keep their published meaning (the engine's own IPAM and resolver):

| Group | Entries |
|---|---|
| gateway | `net.gateway.filter`, `net.gateway.alias`, `net.gateway.update-in-place`, `net.gateway.rule-order`, `net.gateway.multi-wan`, `net.gateway.vpn` |
| nat | `net.nat.snat`, `net.nat.dnat`, `net.nat.one-to-one`, `net.nat.npt` |
| lb | `net.lb.l4`, `net.lb.health-check` |
| dns | `net.dns.records` (records in a provider's DNS), `net.dns.authoritative` |
| ipam | `net.ipam.provider` (allocation in a provider's IPAM), `net.ipam.reservation` (fixed address per MAC), `net.ipam.dhcp` (a provider-served range) |
| segment | `net.segment.remote` (a segment realized by a remote provider) |
| lifecycle | `net.apply.staged` (stage, then activate), `net.apply.rollback` (discard staged changes before activation), `net.observe` (read the actual state back), `net.verify.dataplane` (a traffic probe, not a readback), `net.ownership-marker` |
| firewall | `firewall.stateless`, `firewall.logging`, `firewall.icmp-type`, `firewall.workload-peer` (a peer named by namespace or selector, not a CIDR) |

Every existing provider answers every new row in the same PR (the `match` has no wildcard
arm); each `supported` cites evidence the existing gate greps for.

**`ProviderKind::Gateway`** (serialized `gateway`) is a fifth word for a provider reached over
the network that enforces policy at a boundary the workloads' traffic crosses, not on the node
— OPNsense today. Its report walks the same `network`-kind entries, so `linux/network`,
`proxmox/network` and `opnsense/gateway` compare row by row. It is additive on the wire:
`ProviderInfo.kind` is a string (`proto/delonix/node/v1/common.proto:82`), and its comment
gains the fifth value. The word collides with the `gateway.delonix.io` API group (L7 routes,
ADR-0040 D2.2); it is kept because the port is already `GatewayProvider` and the Kind
`NetworkGateway`, and the L7 group's Kinds are served by the engine's own proxy, which has no
provider report.

**The OPNsense report is declared, never probed**, like Proxmox's compute report (ADR-0050
D3): building it contacts nothing; `provider describe opnsense --probe` reads
`GET /api/core/firmware/status` and the installed plugin list. It is composed in both places
the provider list is composed today (`cmd/provider.rs:117` and
`delonix-node-api/src/providers.rs:18`), and the existing equality test keeps them together.
Capabilities are discovered, not assumed from the name: the spike found the Proxmox lab node
running `proxmox-firewall` with the host's `nftables` option unset, which makes the VNet
firewall **stored but not enforced** (PR #542) — that row is `unavailable-on-host` there, from
a probe, not `supported` because the provider is Proxmox.

### D3. A provider is chosen by name — the caller's, the record's, or the node's

`providers.yaml` (ADR-0054) gains two things:

```yaml
apiVersion: config.delonix.io/v1
defaultProvider: libvirt            # compute, unchanged (ADR-0054 D3)
networkDefaults:                    # NEW: which registry id answers a role when nothing names one
  segment: proxmox
  gateway: opnsense
providers:
  - type: proxmox
    # … as ADR-0054 D1
  - type: opnsense                  # NEW
    url: https://fw.example
    auth:
      keyFile: /etc/delonix/opnsense.key          # 0600, like tokenSecretFile
      secretFile: /etc/delonix/opnsense.secret    # or secretRef: <kind: Secret>
    tls:
      caFile: /etc/delonix/fw-ca.pem              # insecureSkipVerify only as opt-in
```

- The same rules as the Proxmox entry: no inline secret (refused by name at parse time), key
  files owner-only, one entry per type while `name:` stays reserved (so "by name" is "by
  registry id" until ADR-0054 lifts it). `DELONIX_OPNSENSE_URL` in the environment replaces
  the whole entry (ADR-0054 D4). `defaultProvider: opnsense` is refused — it is compute's key.
- **Resolution for a network Kind**, highest first: the provider the document names
  (`NetworkGateway.spec.provider`, now optional; `NetworkZone` keeps no field — the owner's
  transparency rule of the ADR-0049 addendum stands) → **the provider on the record** (an
  existing resource never moves when a default changes) → `networkDefaults.<role>` → only when
  **no** `providers.yaml` exists, the single registered provider of the role (today's count
  rule, kept for a development node byte for byte) → otherwise refused. A default that names a
  provider not registered for that role is an error, never a fall-through (ADR-0054 D3's rule).
- **This supersedes the "resolution is by COUNT" paragraph of ADR-0049's 2026-09-25 addendum**
  and the doc comment at `crates/adapters/delonix-sdn/src/network_zone.rs:20-31`.
- **The record keeps the provider that served it** (`NetworkZoneRecord` gains `provider`), as
  a VM record keeps `backend`. That is the engine's own record of its own resource.
- **The engine persists no binding for anyone else.** Over the node contract a network request
  carries an optional provider name; the answer carries the provider that served it, the
  catalog version and the states of the capabilities the plan used. A caller that wants to pin
  a resource to a provider stores that answer itself and sends the name back.

### D4. Every network change is validate → plan (with digest) → apply(plan, digest) → observe → verify

The use case lives in `delonix-networking` (D7). Its types — `Observed`, `NetPlan { steps,
digest }`, `Step { id, role, op, target, reversible, compensation, pre, post }`, the step
ledger — are plain data.

1. **validate** — pure, no I/O: the document's syntax, then the capabilities it needs
   (`required_capabilities(intent)`) against the resolved provider's report on this host.
   Unknown capability name: `invalid_intent`; unmet: `unsupported_capability`, listing every
   unmet row with its state and detail (ADR-0050 D6's message shape).
2. **plan** — observes first, then `plan = f(intent, observed, report)`, deterministic. Steps
   are ordered by dependency (segment before its VNets; aliases before the rules that name
   them; rules removed before their aliases — the orders ADR-0051 phase 3 and the ADR-0049
   addendum already measured). The **digest** is SHA-256 over the canonical JSON of: the
   normalized intent, the observed fingerprint (the provider's own per-object digests where it
   has them — Proxmox SDN returns one per IPAM/firewall object, spike §3 — otherwise the
   normalized readback), the provider id, the catalog version, the states of the capabilities
   the plan uses, and the plan-format version. A digest over the intent alone would never go
   stale, so it is not one.
3. **apply(plan, digest)** — re-observes and recomputes; a different digest is
   **`stale_plan`** and nothing is written (the ADR-0042 `If-Match` rule, applied to a plan).
   Where the provider has `net.apply.staged`, the steps run inside its transaction: the
   Proxmox SDN global lock, which refuses while someone else's changes are pending (DX-5516)
   and applies with the token or rolls back (PR #542, measured); OPNsense's stage-then-`apply`
   (ADR-0051 phase 0), whose `savepoint`/`apply(rollback_revision)`/`cancelRollback` actions
   exist in 26.1.2 (spike §4, not yet exercised). Each step is written to a ledger (through
   `StateRepository`, ADR-0044 D6) **before** it runs and settled after — the shape of
   ADR-0049 slice 1's task ledger — so a crashed apply is reconciled by the next plan, never
   blindly resent. On failure: staged and not activated → the provider's rollback; activated →
   the compensations of the reversible steps done, in reverse; an irreversible step reached →
   **`partial_apply`** naming the steps done; a compensation that fails → **`rollback_failed`**
   with the ledger.
4. **observe** — read-only, on demand. `stack plan` shows a difference between the record and
   the provider as drift (`drift_detected`, exit 2 under `--detailed-exitcode`). No loop, no
   watcher.
5. **verify** — each step's postcondition read back from **every node that realizes it** where
   the provider reports per node (the spike and PR #542 measured that a Proxmox SDN apply's
   verdict covers the entry node only: pve2's reload failed while the task said OK), plus a
   traffic probe only when `net.verify.dataplane` is supported. Otherwise the result says
   `verified: control-plane-only`. A green without the semantics is not a result.

CLI (F4): `stack plan -o json` carries `planDigest` per network document; `stack apply
--plan-digest <d>` refuses a stale plan. Without the flag, `stack apply` plans and applies in
one invocation as it does today, with the ledger and verification added.

### D5. One error envelope: a stable reason, a `DX-C3NN` code, and the context

The envelope is ADR-0042's problem+json (`type`, `title`, `status`, `detail`, `instance`,
`code`) plus `reason` (the stable slug below), and when they apply `capability`, `provider`,
`role`, `step`, `planDigest`, and `cause` — the provider's message, redacted (the ADR-0049
redaction test pattern: every rendered error is grepped for the secret). The CLI prints the
same fields. The block **`NN` = 80–99 of the network domain** (`DX-C380`…`DX-C399`) is
reserved for these reasons, so the parallel sessions S1–S4 cannot collide with them:

| reason | class (ADR-0043) | exit | note |
|---|---|---|---|
| `invalid_intent` | 1 invalid argument | 1 | includes an unknown capability name |
| `unsupported_capability` | 6 unavailable | 69 | the remedy is another provider, not the argument (ADR-0050 D6) |
| `incompatible_provider` | 6 | 69 | the provider's version lacks what the catalog row needs |
| `provider_unavailable` | 6 | 69 | |
| `provider_rate_limited` | 6 | 69 | carries the retry hint when the provider gives one |
| `provider_auth_failed` | 7 permission denied | 77 | OPNsense answers 401 **and** 302 for this (ADR-0051 phase 0) |
| `policy_denied` | 7 | 77 | only the engine's own guardrails (D6) |
| `address_conflict`, `address_exhausted` | 5 conflict | 5 | |
| `provider_conflict` | 5 | 5 | pending changes of someone else; an object without our marker |
| `stale_plan` | 5 | 5 | |
| `drift_detected` | 2 changes pending | 2 | |
| `operation_timeout` | 8 deadline | 124 | |
| `partial_apply`, `verification_failed`, `rollback_failed`, `dependency_failed` | 9 system failure | 1 | the ledger is attached |

`quota_exceeded` is not an engine reason (guardrail 2). **DX-1342
`network.unsupported_by_gateway_provider` is replaced by `unsupported_capability`** — class 1
→ 6, exit 1 → 69; the release notes say so in those words. DX-1345/1346 (no/ambiguous zone
provider) stay configuration errors and are re-worded by role; DX-6507
`vm.capability_not_supported` is unchanged.

### D6. One typed policy IR, and a lowering per enforcement point that refuses what it cannot represent

`PolicyIr` lives in `crates/foundation/delonix-net-rules` — hand-written, zero dependencies,
as that crate is today — and moves with it into `delonix-networking`'s domain layer when
ADR-0040 P2 absorbs it. `vm_firewall::{Policy, Rule}` is its seed (it already refuses an
unknown protocol). One direction of one target:

- `default`: allow | deny; `rules` in order, **first match wins — the order is the
  priority**, there is no separate number to disagree with it;
- per rule: `action` allow | deny; `family` IPv4 (IPv6 refused, audit 62 P3); `proto` tcp |
  udp | icmp | any; `ports` (one or a range); `icmp_type`; `peer` = `Cidr` | `Any` |
  `Namespace(name)` | `Selector(labels)` (ADR-0024); `stateful` (true; `false` needs
  `firewall.stateless`); `log`; `origin` (ADR-0028's contribution ledger); `guardrail` (an
  immutable, engine-owned rule).

**Invariants it inherits from S1** (audit 62 §7; S1 must be merged before F3):

1. a source's egress is evaluated whatever the destination's ingress says — an `accept` is
   never terminal across the two (P0-1);
2. `FwRule` → IR is a total parse: one rule the IR cannot hold refuses the **whole** set,
   never skips the rule (P0-2);
3. namespace isolation is a guardrail rule — its absence is an error, not a warning (P0-3,
   P0-5), and removing every user rule keeps it (P0-4);
4. guardrails come first and a later `allow` cannot reach past them (anti-spoof keeps its
   explicit per-VM opt-out, ADR-0055).

**Lowerings** — each declares the IR features it can represent with the same semantics and
refuses the rest with `unsupported_capability` naming the row:

| Enforcement point | What it cannot hold (measured or read) |
|---|---|
| nft in the holder (containers, `delonix-sdn`) | the reference lowering |
| Proxmox per-VM firewall (ADR-0052) | `Namespace`/`Selector` peers (refused today, `fromWorkload`); the node inserts at position 0, so the lowering writes in reverse |
| Proxmox VNet firewall | only `forward` rules; not enforced unless the host runs the nftables `proxmox-firewall` (PR #542; spike §3: unset on the lab) |
| OPNsense filter | no engine namespace: a `Namespace`/`Selector` peer is **refused, not expanded into a CIDR snapshot** that goes stale; `stateful: false` maps to `statetype: none`, `log` to `log`, order to `sequence` + `moveRuleBefore` (spike §4) |

**Golden equivalence tests**: one table of IR policies × a fixed set of packets × the expected
verdict, run against every lowering's rendering (nft by `nft --check` plus an evaluator over
the rendered set; the remote ones by their pure renderers, and live where a lab exists). A
lowering that renders a different verdict for any cell fails.

`FwRule` stays the persisted form (no record migration); `GatewayRule` and `vm_firewall::Rule`
become lowering outputs.

### D7. Where it lives: `delonix-networking`, and the direction of the dependency

A new context crate, `crates/contexts/delonix-networking` — the name ADR-0040 D2.2 fixed
(`networking.delonix.io`) — holds the role ports, the per-role registries, the lifecycle use
case and the envelope. `GatewayProvider` and `NetworkZoneProvider` move there from
`delonix-sdn` in F2. The condition ADR-0051 set for that move — "wherever `VmBackend`
eventually moves, in the same commit shape" — is met: `VmBackend`'s port is in
`delonix-compute` since #517. The two phase-tagged exceptions
`("dep", "delonix-opnsense", "delonix-sdn")` and `("dep", "delonix-proxmox", "delonix-sdn")`
in `scripts/arch_fitness.py` are **deleted**, not added to.

`delonix-networking` depends on `delonix-compute` (the catalog and the `Provider` skeleton),
**never the reverse**. `NetworkProvider` and `VmNetwork` therefore stay in `delonix-compute`:
moving them into networking while the catalog is in compute would make the two contexts depend
on each other. That move waits for the catalog to go down to the foundation — its own decision.

### D8. What the engine does not do

Scoring or ranking providers; candidates and "explain" of a choice; a `ProviderBinding` per
anyone's resource; migration between providers; tenant, network class, quota, approval; a
long-running reconciler or watcher; administration of the Proxmox cluster (ADR-0049 D3 — the
cluster firewall and HA stay excluded); new routes on `delonix-mgmt` (frozen, ADR-0041 D4);
any raw provider rule, XML, shell or endpoint arriving over the node contract (brief §25's
"never accept" list holds).

## Phases

| Phase | Deliverable | Exit criterion | Depends on |
|---|---|---|---|
| **F0** | This ADR and its spike | owner accepts | — |
| **F1** | catalog 1.1.0 (every provider answers every new row), `ProviderKind::Gateway`, the declared OPNsense report in `provider ls/describe/matrix` and in `ListProviders`; `providers.yaml` `type: opnsense` + `networkDefaults` parsed and shown by `provider config show/validate` | matrix regenerated and equal; evidence gate green; the E2E diff of the socket against `provider ls -o json` passes with `opnsense` in it; zero behaviour change for any Kind | ADR-0054 accepted |
| **F2** | `delonix-networking`; `SegmentProvider`/`GatewayProvider` moved with no default bodies; per-role registries; D3 resolution (count only without a file); `NetworkZoneRecord.provider`; `native` removed; the D5 envelope and the DX-C380 block | the two `arch_fitness` exceptions gone; a battery check with two segment providers registered and `networkDefaults.segment` naming one; a check that a default naming an unregistered provider fails with exit 69 | F1; S6 markers for the remote writes |
| **F3** | `PolicyIr`, the total parse from `FwRule`, lowerings nft / Proxmox VM / OPNsense, the golden table | golden table green on all three; S1's regression tests still green | **S1 merged** |
| **F4** | plan + digest + ledger + observe + verify for `NetworkZone` and `NetworkGateway`; `planDigest` / `--plan-digest` | live: a stale plan refused against the lab (a VNet added out of band between plan and apply); an apply killed mid-way reconciled by the next plan; per-node verification on the two-node lab | F2 |
| **F5** | `NatProvider`, `IpamProvider`, `DnsProvider`, one slice each, each with a live case | each row it touches cites its live test | F4 |
| **F6** | node contract RPCs (validate/plan/apply/observe for network documents) | contract conformance suite | the ADR-0042 steps that bring mutations |

## Excluded

OpenStack Neutron (ADR-0039 stays Proposed; no environment); IPv6 (refused, audit 62 P3); a
load-balancer port and any LB implementation; authoritative DNS as a product; fabric, route and
telemetry ports; HA gateway; the Proxmox zone types beyond `simple` (evpn, qinq, vlan, vxlan,
faucet are listed by the node, spike §3, and each needs its own slice); `cancel` of a
long-running operation and `import/adopt` (the ownership marker comes first); everything in D8.

## Alternatives considered

- **One `NetworkProvider` with every role's methods.** Rejected: most providers would carry
  default bodies — exactly the refusing and no-op defaults `GatewayProvider` has today, and the
  shape ADR-0044 D3 already ruled out.
- **A separate capability document per role** (audit 62 §10 question 3, second option).
  Rejected: two denominators, and the compile-time walk of ADR-0050 D1 does not reach a second
  document.
- **Keep selection by count.** Rejected: it is correct only while one provider is linked, and
  a second registration turns every `NetworkZone` into a refusal; the choice is invisible to
  the operator either way.
- **A `provider` field on `NetworkZone`.** Rejected: it contradicts the owner's transparency
  rule; the node's file names it, the document does not.
- **A digest over the intent alone.** Rejected: it never goes stale; the observed state has to
  be inside it.
- **Persist a binding and score candidates in the engine.** Rejected by guardrail 2.
- **A reconciler that watches the providers.** Rejected by guardrail 1; observe on demand.
- **Keep the ports in `delonix-sdn` with more exceptions.** Rejected: the condition ADR-0051
  set for moving them is met, and each new role would add an exception instead of removing two.
- **Do nothing.** Rejected: OPNsense stays invisible to `provider ls`, the second segment
  provider breaks `NetworkZone`, and three rule types keep drifting.

## Consequences

- A caller can ask the node what each network provider can do, by role, with the reason for
  every "no" — OPNsense included — before it sends anything.
- `unsupported_capability` becomes one error with one class across VMs and networks.
- A remote network change is refusable before it runs (validate), refusable when its world
  changed (stale plan), recoverable when it dies mid-way (ledger), and honest about what was
  checked (verify).
- **Cost:** a new context crate, a catalog minor, a fifth `ProviderKind` word every consumer
  of `ListProviders` has to accept, a new `providers.yaml` key, and the DX-1342 class change.
- **Cost:** three lowerings to keep equivalent; the golden table is the price.
- **Risk:** this ADR rests on two decisions still `Proposed` — ADR-0040 (the crate layout,
  already enforced by `arch_fitness.py`) and ADR-0054 (`providers.yaml`, slices 2–4 built on
  `main`). F1 does not start before ADR-0054 is accepted.

## Proven vs not validated (the spike, `docs/discovery/64_…`)

**Proven:** the Proxmox lab node (PVE 9.2.2, two-node cluster `lab`, read-only calls) lists
the SDN sections, zone types (`evpn faucet qinq simple vlan vxlan`), IPAM plugins
(`netbox phpipam pve`), DNS plugin (`powerdns`), DHCP backend (`dnsmasq`), the subnet `snat`
flag, the global lock and rollback parameters, per-object digests, the datacenter firewall on,
the host `nftables` option unset, and an **orphan IPAM entry** (`10.250.7.0/24`, zone `prfz`,
VNet `prfv`) whose zone and VNet no longer exist — observed drift of the kind D4's `observe` and
D1's marker are for. The OPNsense 26.1.2 image (offline, read-only) carries API controllers for
filter, alias, category, D-NAT, source NAT, one-to-one, NPT, gateways and routes, Unbound, Kea
and Dnsmasq, and none for a load balancer; `FilterBase` has `savepoint`, `apply` with a
rollback revision, `cancelRollback` and `revert`; a filter rule has `sequence`, `log`,
`statetype` (including `none`), `categories` and `ipprotocol`.

**Not validated:** any write, lock or rollback by this spike (PR #542 measured the Proxmox
lock; nothing measured OPNsense's rollback timer); OPNsense's API answering live (the appliance
was not started and this session has no key); the drafted reports as code; the golden table;
the digest's stability across engine versions; per-node verification beyond PR #542's
observation.

## Addendum 2026-09-27 — F1 built

What F1 delivered, against its exit criterion:

- **Catalog 1.1.0** (`delonix_compute::capability`): the 27 entries of D2 (23 `net.*`, 4
  `firewall.*`), all of kind `network`; the catalog has 127 entries. Every provider answers
  every new row in the same change — the `match` of each report has no wildcard arm, so a
  missing answer does not compile.
- **`ProviderKind::Gateway`** (`gateway`), with `ProviderKind::catalog_kind()`: a gateway
  report walks the `network` rows and keeps its own kind. In `provider matrix` it is a column
  of the network table, labelled `opnsense (gateway)`; the node-api's equality test reads that
  label back as `(gateway, opnsense)`. `--kind gateway` works on `provider ls/describe` and on
  `GET /v1/providers?kind=gateway`; the proto comment of `ProviderInfo.kind` lists the fifth
  word.
- **The OPNsense report** (`delonix_opnsense::capability_report`), declared, never probed:
  3 supported (`net.gateway.filter`, `net.gateway.alias`, `net.apply.staged`, all citing the
  live test of ADR-0051 phase 2), 1 partial, 23 unsupported, 2 requiring an external
  component (the load balancer), 18 not implemented. It is composed in both provider lists
  (`cmd/provider.rs` and `delonix-node-api/src/providers.rs`). The declared network table:
  `linux` 8 of 47 supported, `proxmox` 0 of 47, `opnsense` 3 of 47.
- **`providers.yaml`**: `type: opnsense` (`url`, `auth.key`/`keyFile`, `auth.secretFile` or
  `secretRef`, `tls`), an inline `secret` refused by name, the entry translated into the
  `DELONIX_OPNSENSE_*` keys the one registration reads, the environment replacing the entry as
  a whole. `networkDefaults` with the five roles: `segment` accepts only `proxmox`, `gateway`
  only `opnsense`, `nat`/`ipam`/`dns` are refused naming F5, `defaultProvider: opnsense` is
  refused as compute's key. `provider config validate` checks what the entry points at (the
  secret file owner-only, the CA readable) without registering; `provider config show` prints
  the entry and the network defaults, the secret only by where it comes from. The published
  schema (`docs/schema/v1/providers.json`) is regenerated.

**One behaviour change, on purpose:** a node whose `providers.yaml` has a `type: opnsense`
entry now REGISTERS the appliance from the file (ADR-0054 D4's rule, applied to the gateway),
where before only `DELONIX_OPNSENSE_*` did. Registering contacts nothing (ADR-0008). No Kind
resolves through `networkDefaults` yet — that is F2.

**Measured with the tree's binary** (isolated `DELONIX_ROOT`, a providers file pointing at
`https://fw.invalid`): `provider config validate` passes; `provider ls --kind gateway` lists
`opnsense gateway yes NotProbed 3/1/23/2/18`; every report carries `catalog_version 1.1.0`;
without the file the row is `NotConfigured`; `defaultProvider: opnsense`, `networkDefaults.nat`
and a secret file others can read each exit 1 with the reason; `--kind firewall` is refused
naming the five kinds.

## Addendum 2026-09-30 — F2a: the crate and the move, nothing else

F2 is split. **F2a** is the move D7 names, in the shape `VmBackend` left `delonix-vm` for the
compute context (ADR-0044 P4b.2), and it changes no behaviour:

- **`crates/contexts/delonix-networking`**, a context depending only on `delonix-model`.
  It holds `gateway` (`GatewayProvider`, its registry, the `native` provider), `network_zone`
  (`NetworkZoneProvider`, its registry) and `ownership` (the owner marks), moved from
  `delonix-sdn` with their tests. `delonix-sdn` re-exports the three modules under their old
  paths, so the CLI and `delonix-node-api` do not change a line.
- **The eight failures those modules raise move with them** (`DX-1341`, `1342`, `1344`–`1346`,
  `5340`–`5342`), each with the same text, class and number, so the CLI prints and exits as
  before. The DX-C380 block of D5 is a later slice.
- **`delonix-opnsense` and `delonix-proxmox` depend on the context**, not on the native
  dataplane. The two exceptions `("dep", "delonix-opnsense", "delonix-sdn")` and
  `("dep", "delonix-proxmox", "delonix-sdn")` are deleted: `arch_fitness.py` now sees 28 crates
  and 9 exceptions. This is also the network half of ADR-0044's P4c row.

**What F2 still owes**, unchanged from its row: the ports without default bodies, `native`
removed, `SegmentProvider` in place of `NetworkZoneProvider`, the per-role registries with D3's
resolution by name, `NetworkZoneRecord.provider`, and the D5 envelope with the DX-C380 block.

## Addendum 2026-09-30 — F2b: `native` removed, and `GatewayProvider` without default bodies

- **The seven `GatewayProvider` operations are required.** The four that refused by default,
  and the three that answered `Ok`/`Absent` by default, are declarations now (D1). The one
  implementation, `delonix-opnsense`, already had every method, so nothing else changes.
  `NetworkZoneProvider` never had default bodies.
- **The `native` gateway provider is gone, and the registry starts empty.** It refused every
  `ensure_*`, so no document naming it could ever be applied. `provider: native` now resolves
  to no provider and is refused before the record is written, as any unregistered name is.
  The message says `known: none` rather than an empty list.
- **A record left by an earlier build that names `native` is deleted without a provider.**
  That provider refused every write, so nothing remote carries the record's mark. A battery
  check writes such a record by hand in an isolated root and deletes it.
- **`DX-1342` has no producer now** (`network.unsupported_by_gateway_provider`). The variant and
  the number stay, because the dictionary is published (`delonix explain codes`), until D5's
  DX-C380 block renumbers the envelope.

**Still owed by F2:** `SegmentProvider` in place of `NetworkZoneProvider`, the per-role
registries with D3's resolution by name, `NetworkZoneRecord.provider`, and the D5 envelope.

## Addendum 2026-09-30 — F2c: `SegmentProvider`, and a provider chosen by name

- **`NetworkZoneProvider` is `SegmentProvider`** (D1), in `delonix_networking::segment`; the
  registry, its registration type and its functions follow the name. The three failures keep
  their numbers (`DX-1344`–`1346`) and the dictionary now names the new trait.
- **D3's resolution is one pure function, `delonix_networking::resolve::choose`**, shared by
  both roles. Highest first: the document's `spec.provider` (NetworkGateway only), the record's
  provider, `networkDefaults.<role>`, and only without a `providers.yaml` the single registered
  provider. A name that resolves to nothing is an error at every step, never a fall-through:
  - a document naming an unregistered provider: `DX-1348` (`network.provider_not_registered`);
  - a record naming an unregistered provider: `DX-6304`, exit 69 — the resource never moves;
  - a default naming an unregistered provider: `DX-6303`, exit 69;
  - a `providers.yaml` without the role's default, or a gateway with none named and zero or
    several registered: `DX-1347` (`network.no_provider_for_role`);
  - with no `providers.yaml`, zero or several segment providers keep `DX-1345`/`DX-1346`.
- **`NetworkZoneRecord` keeps `provider`**, written on the first apply; a record from before
  the field has it empty and resolves through the default. `describe` shows it.
- **`NetworkGateway.spec.provider` is optional.** The reconciler compares it only when the
  document names one, so an unnamed gateway does not drift against its record.
- **Resolution runs before the record is written**, so every refusal above leaves no record.

**The exit criterion changed, by the owner's decision (2026-09-30).** The F2 row asks for a
battery check with two segment providers registered and `networkDefaults.segment` naming one.
The battery cannot build that: only Proxmox serves the segment role, and `providers.yaml`
takes one entry per type while `name:` stays reserved (ADR-0054). So the choice between two
providers is proven in-process, by `resolve::tests` and `segment::tests` with fake providers
registered in the test; the battery proves with the real binary what it can build — a default
naming an unregistered provider exits 69, for a zone and for an unnamed gateway, and a
`providers.yaml` without the default turns the count rule off (exit 1), with no record left
behind. Lifting ADR-0054's `name:` reservation would let the battery register two real
providers; that is its own decision.

**Still owed by F2:** every role port extending `Provider` (D1 rule 4: `capabilities()`,
`health`), and the D5 envelope with the DX-C380 block.

## Addendum 2026-09-30 — F2d, part 1: the role ports extend `Provider`

- **`GatewayProvider` and `SegmentProvider` extend `delonix_compute::vm_provider::Provider`**
  (D1 rule 4): `id() -> ProviderId`, `capabilities() -> ProviderReport`, and `health`. The role
  ports' own `id() -> &'static str` is gone, so there is one identity per provider.
- **OPNsense answers with its declared report** (`capability_report(true)`), **Proxmox with its
  declared network report** (`network_capability_report(true)`). Neither contacts anything to
  answer, and the value exists only once its registration was configured.
- **`delonix-networking` depends on `delonix-compute`**, the direction D7 allows (compute never
  depends on networking); the C4 page shows the edge.

**Part 2, the D5 envelope and the DX-C380 block, is a separate change**: it renumbers published
codes, which D5 asks the release notes to name, and it has to be reconciled with the four codes
F2c introduced.

## Addendum 2026-09-30 — F2d, part 2a: the DX-C380 block

**The owner decided (2026-09-30) to renumber every published network failure that is a D5
reason into the block**, and that the old DX-1348 exits 69 as D1 rule 2 says. That goes one
step past D5's own text, which kept DX-1345/1346 as configuration errors: they are
`invalid_intent` now, still class 1 and exit 1.

- **Each reason owns one `NN`**, whatever its class: `delonix_model::codes::Reason`, in D5's
  table order from `80` (`invalid_intent`) to `96` (`dependency_failed`), and
  `Reason::number()` builds `DX-C3NN` from the reason's class. A test holds the block to the
  reasons: an entry in `80`–`99` of the network domain that is no reason, or is in another
  class, fails. A reason gets an entry when something raises it; the others keep their
  number reserved and no entry yet.
- **The mapping**, and the exit code each failure answers now:

  | new | reason | replaces | exit |
  |---|---|---|---|
  | DX-1380 | `invalid_intent` | 1343, 1345, 1346, 1347 | 1 (unchanged) |
  | DX-5389 | `provider_conflict` | 5340, 5341, 5342 | 5 (unchanged) |
  | DX-6381 | `unsupported_capability` | 1342, 1348, 6303, 6304 | 69 (1342 and 1348 were 1) |
  | DX-6383 | `provider_unavailable` | 9303 (OPNsense transport) | 69 (was 1) |
  | DX-7385 | `provider_auth_failed` | 9305, 9306 (OPNsense 401/302/403) | 77 (was 1) |

  The release notes must carry this table: a script that matched `DX-5340` or read exit 1 for
  a refused OPNsense key sees a different answer.
- **A number never changes meaning and is never reused**, so the old ones are not deleted:
  `codes::RETIRED` keeps each with its published texts, the number that replaced it and the
  last release that emitted it (`None` for 1347, 1348, 6303 and 6304, which only ever lived on
  the main branch — retired anyway, so a later entry cannot take them). `delonix explain
  DX-5340` still answers, with `Retired: replaced by DX-5389`; `explain codes --json` lists the
  retired ones with `replaced_by`, and the generated `codigos.html` has a «Retired codes» table.
  A test refuses a retired number or id back in the dictionary.
- **Several variants share a reason, and the variant still says which.** `Error::reason()` in
  `delonix-networking` and in `delonix-opnsense` is the exhaustive match; `number()` asks it
  first. The tests that used the number to tell «not ours» from «someone else's staged
  changes» now also match the variant or the message.
- **`provider_auth_failed` needed a class-7 carrier.** The only one was `Error::Io` with
  `PermissionDenied`, which prints «I/O error» for a refused API key; the model gained
  `Error::PermissionDenied(String)` («permission denied: …», `DX_PERMISSION_DENIED`, exit 77).
- **Left as they are**: the registration refusals (1341, 1344 — a programming error in the
  process that registered, not a provider failure), OPNsense's 404, unclassified status,
  oversized or unparseable body and unbuildable client (no D5 reason names them), DX-6301/6302,
  and the Proxmox errors, which live in the VM domain.

**Still owed by F2, part 2b:** the envelope's context fields (`provider`, `role`, `step`,
`cause` redacted, with ADR-0049's grep-for-the-secret test) on the CLI and the node API.

## Addendum 2026-09-30 — F2d, part 2b: the envelope's context, and the redaction

- **The context travels inside the error.** `delonix_model::ErrorContext` (`provider`, `role`,
  `capability`, `step`, `plan_digest`, `cause`) rides on the `Error::Coded` carrier the number
  already uses (ADR-0043 D4). `Error::with_context` adds it without touching the number, the
  class, the message or the exit code; a field set closer to the failure wins over one added
  further up, so a caller can add the role without overwriting the step the provider named.
- **One problem document, built from the error**: `delonix_model::codes::problem(&e, instance)`
  gives the RFC 9457 members (`type` points at the dictionary entry,
  `codigos.html#DX-6381`; `title`, `status`, `detail`, `instance`) plus `code` (the `DX_*`
  class), `dx`, `exit`, `reason` for a code of the network block, and the context fields that
  are set. An unset field is left out. `Class::http_status` gives each class its HTTP word, the
  ones the management API already used for the classes it mapped.
- **The CLI prints the same fields** under the error line: `reason:`, `provider:`, `role:`,
  `step:` (translated labels; the values stay as the machine reads them). The network Kinds add
  the context: `resolve_provider` with the role and the provider the document, the record or
  the default named, and every provider call with its step (`ensure_alias`, `commit`,
  `ensure_vnet`, `remove_zone`…).
- **Redaction lives in the provider, because only it holds the credential.** Both remote clients
  read every answer in one place, and it now passes through `delonix_model::redact_known`
  before it can reach an error: OPNsense removes the API secret and the Basic header value it
  travels in (a proxy or an error page can echo it); Proxmox removes the token secret or the
  password, and the ticket and CSRF token a password login holds. **This closed a real leak**:
  each client put the answer's body into its error message as it came, and a node that echoed
  the request put the secret on the operator's terminal. The ADR-0049 test pattern now guards
  it: `no_rendered_error_carries_the_credential_the_answer_echoed`, in both crates, renders
  every such error as its message and as its problem document and greps for each secret; with
  the redaction removed it fails and prints the secret.
- **Not in this slice**: the node API keeps the `google.rpc.Status` body the published OpenAPI
  declares for its one route; moving it to problem+json changes the contract and is ADR-0042's
  step D (and F6 brings the network RPCs that would carry the context). `cause` has no producer
  yet: the provider's text is already in the message, redacted, and splitting it out is a
  change to each client's error variants. `plan_digest` waits for F4.

**F2 is closed** with this slice.

## Addendum 2026-09-30 — F3a: the policy IR, its reference evaluator, and the golden table

F3 is sliced by lowering: **F3a** the IR and its reference semantics (this addendum), **F3b** the
nft lowering, **F3c** the Proxmox per-VM lowering, **F3d** the OPNsense lowering. Each later slice
is checked against the same golden table.

- **`delonix_net_rules::policy`** — `Policy` (direction, default, ordered rules), `Rule` (action,
  proto, ports, icmp type, peer, stateful, log, origin, guardrail), `Peer` (`Any`, `Cidr`,
  `Namespace`, `OtherNamespaces`, `Selector`), `Packet`, and **`evaluate`**, the reference verdict:
  the return of an admitted flow passes when every rule is stateful; otherwise the first rule that
  matches decides and the default decides the rest. `any` with a port matches TCP and UDP only.
  Still zero dependencies. `OtherNamespaces` is a peer this ADR's list did not name: the namespace
  guardrail cuts «a workload of any namespace but this one», which neither `Namespace` nor
  `Selector` can say, and traffic from outside the engine's workloads never matches it.
- **`delonix_networking::policy::from_container_fw`** — the total parse from the persisted
  `ContainerFw` (no record migration). One rule the IR cannot hold refuses the whole set with
  DX-1380 `invalid_intent`, naming the rule; nothing is skipped. It rebuilds the holder chain's
  order: user rules, then the namespace guardrail (`Allow Namespace(ns)` only without an explicit
  inbound intent, then `Deny OtherNamespaces(ns)`, both marked `guardrail`), then the defaults.
- **`delonix_networking::policy::golden::cases()`** — the golden table, public so a lowering in
  another crate runs the same cells: 24 cells of record × packet × verdict, covering the open
  record, the S1 C1 case (one inbound deny keeps the isolation), a Dependency-style allow across
  namespaces, the `any`+port widening case, first match, egress, and a bare host address.
- **The verdicts were written from reading `fw_chain_body`, not measured against a kernel.** F3b
  closes that: it renders the IR as nft, checks the ruleset with `nft --check`, and evaluates the
  rendered rules over the same cells.
- **Two differences from today's code, found by writing the parse and left as they are for now:**
  a reversed port range (`90-80`) passes `validate_container_fw` (and would fail inside `nft`)
  but is refused by the parse; and a disabled record gives an empty chain, so a namespaced
  workload with `enabled: false` has no isolation guardrail. The IR reproduces that (two open
  policies). ADR invariant 3 says the guardrail's absence is an error; making it one changes what
  a disabled firewall means and is its own decision.

## Addendum 2026-09-30 — F3b: the nft lowering, and the holder chain built from it

- **`delonix_sdn::policy_nft::chain_body`** renders a `TargetPolicy` as the lines of one address's
  part of the workload chain. `infra::fw_chain_body` is now parse, then render: the holder chain is
  built from the IR and nothing else. `validate_container_fw` runs the same translation last, so a
  record the IR or the lowering refuses is refused on the host and in the holder, before `nft -f`,
  with the previous ruleset kept.
- **What the holder chain cannot hold is refused by name**: ICMP (`proto` or type), a selector or
  namespace peer on a user rule, a logged rule, a stateless rule, an egress guardrail. None of them
  can come from a `ContainerFw` today; the refusals exist so a later IR producer cannot reach the
  chain with a meaning the text would drop.
- **Three proofs, each verified to go red under a mutation**:
  - the lines equal the old generator's, kept verbatim as a test oracle, for every golden record
    and five shapes the table does not cover;
  - an evaluator over the rendered TEXT (prologue plus body, every token parsed, an unknown token
    fails the test) gives each of the 24 golden cells the reference verdict. Anchoring the egress
    default on `daddr` turns it red;
  - `nft --check` accepts every rendered chain (via `unshare -rn`; `counter acept` turns it red).
    The hosted CI runner blocks unprivileged user namespaces, so there this check returns without
    running; it ran here with nftables 1.0.9.
- **Two differences from the old text, and neither changes a verdict.** (1) Inbound and outbound
  rules are no longer interleaved in record order; each direction keeps its own order, which is the
  one that decides — an inbound line anchors on `ip daddr <workload>`, an outbound one on
  `ip saddr <workload>`, and a forwarded packet never carries the workload's address at both ends.
  (2) A record with `namespace: ""` names the `default` set, as the attach side always did; the old
  generator hashed `""`, a set no workload joins. Serde never produces that record.
- **The reversed port range is now refused by `validate_container_fw`** (F3a's first open
  difference), because validation goes through the parse. Measured before deciding: nft refuses
  `90-80` itself («Range has zero or negative size»), so the only change is where and how clearly
  the refusal is reported. The disabled-record guardrail (F3a's second difference) is unchanged.

## Addendum 2026-09-30 — F3d: the perimeter filter is a lowering of the IR, measured on a live appliance

Measured on a fresh OPNsense 26.1.2_5, built from the published `opnsense:26.1` image with
`delonix vm create`, with a lab API key kept in a 0600 file:

- `firewall/filter/add_rule` takes `action`, `destination_port`, `log`, `statetype`, `sequence` in
  the flat form, and `search_rule` answers them in the same form.
- pf loads filter rules in `sequence` order, whatever order they were created in. So order needs
  neither `move_rule_before` nor an update in place.
- `protocol: TCP/UDP` loads as a TCP and a UDP pf rule under one label. That is the IR's
  «`any` with a port».
- `block` loads as `block drop`, `log` as `log`, and a range `8000-8080` as `port 8000:8080`.

What F3d adds:

- **`GatewayRule`** gains `action`, `destination_port`, `log`, `stateful`, `sequence`. The
  defaults are what every earlier rule was: pass, any port, keep state, no log, the appliance's
  sequence. So an existing rule does not read as drift. The client sends the new fields, and
  `rule_drift` compares them.
- **`delonix_networking::policy::gateway_rules`** lowers one direction of the IR for a target (an
  alias, a prefix or an address):
  - each rule is `<name>#<n>` at `sequence = first + n`;
  - the default verdict is one more rule, `<name>#default`, at the end.

  It refuses by name (DX-1380) a namespace, other-namespaces or selector peer, an engine
  guardrail, and an ICMP type (the field exists on the appliance; the lowering was not measured
  against it).
- **Proofs**:
  - An appliance evaluator runs over the rules. It models pf quick rules in `sequence` order and
    state kept by a keep-state rule. It gives each of the 24 golden cells the reference verdict,
    with the guardrails removed and their refusal checked. «`any` with a port» sent as TCP only
    turns it red.
  - Live, `a_lowered_policy_lands_on_the_appliance_in_its_order_with_its_fields` ensures and
    commits a lowered policy through the provider. It reads every field back from `search_rule`,
    and reads pf's load order from `pf_statistics`. It then removes everything and retires the
    owner.
- **Found on the way, and fixed first (#624)**: the client's pending-change check read two views
  of the running state that go stale on this appliance:
  - a deleted alias's pf table;
  - `list_rule_ids`'s label cache, which keeps the pf lines past the end of a shorter ruleset.

  Each made the engine's own deletion fail its commit, and refused every commit after it.
- **Catalog**:
  - `net.gateway.rule-order`, `firewall.stateless` and `firewall.logging` become `partial`. The
    port carries them and they are live-tested, but no field of `kind: NetworkGateway` reaches
    them yet.
  - `net.ownership-marker` becomes `supported`: its live test ran against this appliance.
- **Not in this slice**: a manifest surface that declares a policy for a gateway. That is new
  schema, with a record and a teardown by rule count, and it is its own decision.

## Addendum 2026-09-30 — F3e: `kind: NetworkGateway` declares policies

- **`spec.policies`** declares one direction of one target's policy in the shape a
  `NetworkPolicy` has. The target is an alias, a prefix or an address. The fields are:
  - `name`, `target`, `direction` (`ingress` or `egress`), `defaultPolicy` (`deny` when omitted);
  - `sequence`, the first rule's position;
  - `rules`, each with `proto`, `port`, `from` or `to`, `action`, `log` and `stateful`.
- **Lowering.** The rules are built as IR through `delonix_networking::policy::rule_of`, which is
  now public: the same parse a container's record uses. `gateway_rules` then lowers the IR.
- **Refused before anything is sent**:
  - a direction that is neither `ingress` nor `egress`;
  - a peer named on the wrong side (`from` on an egress rule, or `to` on an ingress rule);
  - a reversed range;
  - a name that cannot be an identity;
  - two policies with one name;
  - two policies whose positions overlap.
- **The record keeps the policies last declared.** A teardown recomputes their rule identities
  from it. The reconciler compares them: a change plans a Replace, as every `NetworkGateway`
  change already does, since there is no update in place.
- **Live, by the CLI, against the OPNsense 26.1.2_5 lab appliance.** The manifest had two
  policies, one ingress with three rules and one egress with one rule. The run, with an isolated
  root:
  - `stack apply` put 6 rules on the appliance, and every field read back as declared;
  - pf loaded the rules in `sequence` order, with `TCP/UDP` as two pf rules;
  - `stack plan --detailed-exitcode` answered 0;
  - a second `apply` was idempotent;
  - `delete networkgateways` left no rule, no pf line and no owner category of its own.

  The rules carry no interface, so they are floating (`in quick inet`), as every rule this client
  wrote before.
- **Catalog.** `net.gateway.rule-order`, `firewall.stateless` and `firewall.logging` become
  `supported`. The manifest now reaches them, and the live provider test exercises them.

## Addendum 2026-10-01 — F4a: a `NetworkGateway` plan observes the appliance

F4 is sliced like F3:
- **F4a** (this addendum): observe for `NetworkGateway`;
- **F4b**: `planDigest` and `--plan-digest`;
- **F4c**: the step ledger and the reconciliation of an apply killed mid-way;
- **F4d**: the same for `NetworkZone`, with per-node verification on the two-node Proxmox lab.

What F4a adds:

- **Observe.** `GatewayProvider::observe(owner)` is a required method, because the port has no
  default bodies. It is read-only and returns `GatewayObserved`: the aliases and rules carrying
  the mark, as the appliance holds them, plus the rules it has disabled. OPNsense implements it
  from `search_rule` and `alias/search_item`, keeping the rows whose categories carry the mark.
- **Compare.** `gateway_drift` is pure. It names every difference between what a record declared
  and what the provider holds:
  - an object that is missing, or owned and not declared;
  - a field that differs, where a `sequence` counts only when declared and the protocol is
    compared case-insensitively;
  - alias content, compared as a set;
  - a rule that is disabled.
- **Plan.** A `NetworkGateway` record's `remote` field is observed on every plan. The manifest
  always wants `in sync`. So a change made on the appliance by hand is drift:
  - `stack plan --detailed-exitcode` answers 2;
  - `delonix drift` names it;
  - `stack apply` refuses without `--replace`, as every change to this Kind does (there is no
    update in place);
  - `--replace` converges it.

  A record that cannot be observed says so instead of claiming to be in sync. That covers a
  record without an owner mark, and one from the retired `native` provider.
- **Live against the OPNsense 26.1.2_5 lab appliance.**
  - The provider test reads back exactly the lowered rules. A rule disabled by hand and applied
    is drift, and it is in sync again once re-enabled.
  - By the CLI, with an isolated root:
    - plan 0;
    - a rule disabled by hand gives plan 2, with `remote: rule 'f3e-web-in#2' is disabled on the
      appliance`, and the same line in `delonix drift`;
    - `apply` refuses without `--replace`, and `--replace` converges (plan 0);
    - `delete` leaves no rule and no category.
- **Catalog.** OPNsense `net.observe` becomes `supported`.

## Addendum 2026-10-01 — F4b: the plan digest, and a stale plan refused

- **`delonix_networking::plan::plan_digest`** is SHA-256 over the canonical JSON of what a plan is
  decided from. Object keys are written in sorted order at every depth. The inputs are:
  - the normalized intent, which is the document's compared fields;
  - what the provider holds under the record's mark (`gateway_fingerprint`): aliases by name,
    rules by description, every observed field, and whether a rule is disabled;
  - the provider id and the catalog version;
  - the states of the capabilities the document uses;
  - `PLAN_FORMAT`.

  Pure, with a test that every input moves the digest and that order does not.
- **`stack plan -o json`** carries `planDigest` on each `NetworkGateway` change. A document no
  provider resolves for has none.
- **`stack apply --plan-digest <d>`** is repeatable, one per network document, and the top-level
  `apply` takes it too. Before the first write it recomputes each network document's digest. One
  that is not among those given is refused as **DX-5390 `network.stale_plan`** (exit 5, reason
  `stale_plan`), and nothing is written. The flag is also refused when the manifest has no
  network document to check. Without the flag nothing is checked: `apply` plans and applies in
  one invocation, as before.
- **Live against the OPNsense 26.1.2_5 lab appliance**, by the CLI with an isolated root:
  - the digest planned before the first apply is accepted and creates the 6 rules;
  - the digest changes after the apply, and planning twice gives the same one;
  - a wrong digest is refused with DX-5390 and exit 5;
  - with a rule disabled on the appliance by hand between plan and apply, the apply with the old
    digest is refused and the appliance is untouched (5 of 6 rules enabled before and after);
  - the apply with the new digest converges (6 of 6).
- **Not in this slice**: `NetworkZone` has no digest yet (F4d). The digest is computed with a
  second observation of the appliance per plan, separate from the one behind the `remote` field.

## Addendum 2026-10-01 — F4c: the step ledger, and an apply killed mid-way

**Measured first, on the lab appliance with the F4b binary.** A `stack apply` of 6 rules was
killed (`kill -9`) after 2 were staged and before the commit. The appliance was left with 2 rules
configured and none loaded in pf. Afterwards:
- the next `stack apply` planned a replace, because 4 rules were missing;
- `--replace` was refused with DX-5389, naming this engine's own two staged rules as «not this
  engine's».

The cause is that what a provider value staged lives only in that process's memory. The document
was stuck until someone went to the appliance by hand.

What F4c adds:

- **`delonix_networking::ledger::StepLedger`** is plain data, kept in the record. Each step
  (`ensure_alias`, `ensure_rule`, `remove_rule`, `remove_alias`, `commit`) is opened and the
  record saved before it runs, then settled and saved after. `finish()` marks the end of a run.
  `is_interrupted()` is true when a step did not end well, or when steps are done and the run
  never finished.

  The second case was found live. The first version called a run complete when no step was open.
  A kill landed after step 2 was settled and before step 3 was opened: every step read `done`,
  with 2 of 6 rules created.
- **`GatewayProvider::adopt_pending(owner, removed_ids)`** takes over what a dead run staged:
  - every pending change of an object carrying the owner's mark;
  - every pending deletion whose id is in `removed_ids`.

  Anything else pending stays foreign, and the pre-check still refuses it.
- **`GatewayProvider::owned_rule_ids(owner)`** gives the provider's ids of the owned rules. A
  teardown saves them in the ledger before the first deletion. A deleted rule carries no mark,
  so its id is the only way to recognize it later.
- **The plan.** A record's `applied` field is `complete`, or where its last run stopped. The
  manifest wants `complete`, and the field converges live. So a plain `stack apply`, with no
  `--replace`, resumes: it adopts what was staged, ensures what is missing, and commits. While a
  run is interrupted the `remote` field is not compared, because what is missing is what the run
  had not reached.
- **Live, the same kill with the F4c binary:**
  - after the kill, 2 rules were configured and none loaded;
  - the next `stack apply` answered `the last run was interrupted: step 2 (ensure_rule
    'f3e-web-in#2') did not finish, after 1 step(s) done — resuming, with 2 staged change(s) of
    it adopted`;
  - it left 6 rules configured and 7 pf lines, and `stack plan --detailed-exitcode` answered 0;
  - the ledger on disk read 7 steps done and `finished: true`.
- **Not in this slice**:
  - `partial_apply` and `rollback_failed` as reasons, and compensations. An apply that fails
    still leaves what it did, named in the ledger.
  - A teardown killed mid-way was exercised on the TLS mock only, not live.
  - A record written before the ledger existed cannot be resumed: it has no steps to read.
