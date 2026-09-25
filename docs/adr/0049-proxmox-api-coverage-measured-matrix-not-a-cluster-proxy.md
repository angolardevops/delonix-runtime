# ADR-0049: Proxmox API coverage is a measured, versioned matrix — and the runtime is not a cluster proxy

- **Status:** Proposed — the endpoint matrix exists and is reproducible (D1, #474); slice 1
  (transport, typed errors, task ledger, lost-answer reconciliation, failure injection
  against a TLS mock) is built and tested (#475), and its lifecycle was run against a live
  PVE 9.2.2 node with the route trace on (2026-09-23, `docs/proxmox/trace-9.2.2.routes`):
  **15 of the 18 called routes are `supported+tested`** (the 18th, `DELETE …/snapshot/{snapname}`,
  entered with the same day's second run — see the addendum at the end); the 3 left are named
  with the reason in the trace header. Slice 2 has its first operation live (2026-09-24,
  same node): `disk_size_gib` on a template clone grows the boot disk through `PUT …/resize`,
  the clone source is made with `POST …/template`, and the trace now says **19 of 20 called
  routes are `supported+tested`** — only the lost-answer `GET /nodes/{node}/tasks` stays
  untested live. The rest of slice 2 and slice 3 have no live conformance, and the status does
  not change before they do
- **Date:** 2026-09-23
- **Deciders:** Walter Angolar
- **Related:** ADR-0008 (the Proxmox backend as ONE node behind `VmBackend`, and what it
  excludes on purpose); ADR-0010 (a remote management API is rejected — the ceiling any
  "administration gateway" sits under); ADR-0039 (the live proof against Proxmox VE 9.2 this
  ADR counts as "tested"); ADR-0040/ADR-0044 (the provider port, capability discovery and
  `ProviderExtensions` this ADR reuses rather than redefines); ADR-0043 (the `DX-CDNN` codes a
  provider error maps into).
- **Supersedes nothing.** This ADR arrived as a draft numbered 0045; that number was already
  taken by the stack-groups decision, and the draft is re-numbered here without changing
  what it decides.

## Context

### The claim this ADR exists to make measurable

The house already keeps a skill whose contract is «if Proxmox exposes it, this workspace
calls it — and proves it against a real node», with a baseline of «~10–15 %» coverage. That
number is honest for what it measures — ten AREAS, judged by hand — and useless for what a
reader takes it to mean, because nothing says what the 100 % is. The same trap was closed
for the Compose Specification the day before this ADR (`compatibility compose`, M02): a
third state, *missing*, needs a DENOMINATOR, and the denominator is the specification's own
list, never a count written from memory.

Proxmox VE publishes its own list. Every node serves the generated API schema without
authentication at `/pve-docs/api-viewer/apidoc.js`. That file is the denominator.

### Measured on 2026-09-23, against the real cluster

The target is the cluster the owner runs today — `ngola-lda`, three nodes (`delonix02`,
`delonix03`, `ngola`), **Proxmox VE 9.2.2**, with Ceph, SDN (zones, VNets, IPAM, fabrics)
and PBS storage configured, and the `delonix-lda-*`/`dks-mgmt-*` VMs of this workspace
running on it. The schema was fetched from `192.168.1.10:8006` (sha256 `f9b79fbbc29b14b8…`,
4 271 833 bytes) and read by `scripts/proxmox_api_inventory.py`, which also reads the
provider crate's source to find what it calls — never a list kept next to the code:

```
schema routes (method, path): 675
  called     16   2.4% of the schema
  excluded  364   with a written reason
  missing   295

area              called  excluded  missing  total
qemu                  12         0       97    109
lxc                    0        62        0     62
sdn                    0         0       90     90
storage                0         0       25     25
access                 1        42        2     45
pools                  0         0        7      7
cluster (other)        1       162       11    174
nodes (host)           2        98       62    162
version                0         0        1      1
```

The 16 routes `delonix-proxmox` calls (`crates/providers/delonix-proxmox/src/lib.rs`, 2 169
lines) are one purpose, the QEMU VM lifecycle: `POST /access/ticket`, `GET /cluster/nextid`,
`GET /nodes`, `POST /nodes/{node}/qemu`, `POST …/qemu/{vmid}/clone`, `GET`+`POST …/config`,
`POST …/status/start`, `POST …/status/stop`, `GET …/status/current`, `DELETE …/qemu/{vmid}`,
`GET`+`POST …/snapshot`, `POST …/snapshot/{snapname}/rollback`,
`GET …/agent/network-get-interfaces`, `GET /nodes/{node}/tasks/{upid}/status`. Every one
of them exists in the 9.2.2 schema (the script exits 1 if one does not). The «~10–15 %»
and the 2.4 % are both true; they have different denominators, and only the second one can
be reproduced by someone else. Both numbers say the same thing the draft said: the provider
cannot claim coverage of the Proxmox API, and today nothing stops a reader from believing
it does.

Of the 16, what has been **exercised against a live node** is recorded elsewhere and is
reused here, not re-run: ADR-0039's table (Proxmox VE 9.2, commit `a9c41cb9`, 51 checks /
0 failures — create, same-VM stop/start, back-to-back restart, snapshot, destroy, field
refusal, `kind: VirtualMachine`) and `tests/live.rs` (create, boot, destroy — skipped
unless `DELONIX_PROXMOX_TEST_URL` is set, on purpose). `docs/discovery/58_…` measured the two
fields the flat spec leaked: `--namespace` is refused before any API call, `--network` is
silently ignored — the finding ADR-0044's `VmSpec`/`Extensions` exists to close.

### What the transport already is, read from the code

- `reqwest` blocking, `rustls` with webpki roots; TLS **verified by default** and
  `danger_accept_invalid_certs` only behind an explicit `insecure_tls` on the target
  (`lib.rs:197`). Connect timeout 15 s, request timeout 120 s.
- API token (`PVEAPIToken=<id>=<secret>` header, the form the docs prefer) or a ticket
  that is renewed once on a 401 — **password only**: a token that gets a 401 was revoked,
  and retrying forever is how a credential gets locked.
- `wait_task` reads the task's `exitstatus`, never its `status`, and `task_verdict` is
  pure and tested — the trap the module header documents («a client that reads `status` as
  the result…»). Every write the crate makes goes through `task(...)`. The schema documents
  only 5 routes as returning a UPID by description, but 9 of the 16 called routes return
  `string`, and the crate treats every such string as a task to wait on.
- A `can't lock file … got timeout` verdict is retried (`is_lock_timeout`); a lock error
  without `got timeout` is not.
- **Not there, measured by absence:** no bound on response size (`grep -c redact` = 0,
  no `Content-Length` check), no explicit redaction of the secret in error/`Debug` output —
  whether a secret can reach a log has to be MEASURED in slice 1, not assumed either way.

### What the node contract already decided

`proto/delonix/node/v1/common.proto` already carries `Capability { name, supported, detail }`,
`ProviderInfo { id, kind, available, capabilities, health }`, `ProviderExtensions` (a map
namespaced by provider id, validated by the provider itself, failing closed on unknown
keys), and `VirtualMachineSpec.required_capabilities` with the rule that a request needing
a capability the selected provider lacks is `FAILED_PRECONDITION` with reason
`CapabilityNotSupported`. **The draft's «capability-negotiated, provider-neutral contract»
is not a new decision — it is ADR-0040 D3/ADR-0044 D2, and this ADR reuses it verbatim.**

### Guardrails this touches

- **#2 (no tenant, no fleet).** ADR-0008 addresses ONE node with ONE credential and
  excludes cluster inventory and scheduling. Cluster-dependent operations (storage,
  migration, HA, SDN) can be DISCOVERED as capabilities of that node's cluster; the engine
  never picks a node. Which node a workload lands on is a consumer's decision, expressed
  as an explicit provider target.
- **ADR-0010.** A remote, generic, authenticated HTTP gateway is a remote management
  API with another name. Rejected there; not reopened here.
- **#4 (engine crates dependency-clean).** The HTTP transport stays in
  `delonix-proxmox`; `delonix-vm`/`delonix-compute` do not grow a client.
- **#6 (no silent failure).** An operation a provider cannot honour is an explicit
  unsupported-capability error, never a no-op; an HTTP 200 on a task-producing route is
  never reported as success before the task's `exitstatus` says so.

## Decision

### D1. The denominator is a named schema, generated — never a hand count

Coverage is stated as **called routes / all (method, path) routes of the `apidoc.js` of a
named Proxmox VE release**, produced by `scripts/proxmox_api_inventory.py` from a schema
fetched from a node of that release. Each route is in exactly one of three states —
`called`, `excluded` (with the reason written in the script's `EXCLUDED` table), `missing`
— and an excluded route **stays in the denominator**: hiding it would make the percentage
grow without a single new route served. A route the crate calls that the schema lacks
fails the run (exit 1): that is a route the target release does not have, and the crate is
about to send it.

The baseline of record: **PVE 9.2.2, 16 / 675 (2.4 %), 12 / 109 of `qemu`** — measured
2026-09-23 as above. Slice 1 added `GET /nodes/{node}/tasks` (the lost-answer reconciliation)
and the snapshot delete added `DELETE …/snapshot/{snapname}`: the committed matrix says
**18 / 675 (2.7 %), 14 / 109 of `qemu`**, and it — not this paragraph — is the number of record
(`docs/proxmox/matrix-9.2.2.md`, regenerated by the gate). The schema is not vendored (4 MB of JavaScript that changes per
release); its sha256 and counts are, and the command that reproduces them is in the
script's docstring.

### D2. What a coverage claim may say, and what it may not

«Docker-compatible» without a number, a date and a version is already banned in this
repository; the same sentence holds for Proxmox. A claim names the release, the numerator,
the denominator and how many of the numerator were exercised live: *«16 of 675 routes of
the PVE 9.2.2 schema are called; K of them exercised against a live 9.2 node (cite the run)»*
— and K is counted from the run's log, route by route, not estimated.
**«100 %» is valid only for an explicitly defined subset and version, after that version's
live suite passes** — never for the Proxmox REST API as a whole. The workspace skill's
per-area verdict keeps its use (it says which AREAS a tenant can reach); it does not replace
the route matrix as the number.

### D3. Excluded on purpose: administration is not a VM operation

The `EXCLUDED` table names, prefix by prefix, what the engine does not call by decision:
identity administration (`/access/*` except the login), cluster membership, HA policy,
replication, backup schedules, cluster and node firewall, ACME, Ceph, metric servers,
notifications, resource mappings, jobs, datacenter options, and the host's packages,
certificates, services, subscription, hosts file, DNS, clock and logs. These are the
Proxmox operator's surface; the engine is a VM provider on that node, not its admin
console. `sdn`, `storage`, `pools` and `version` are **missing, not excluded**: the engine
has Kinds and provider ports (`NetworkProvider`, `StorageProvider`, ADR-0044) that a
Proxmox provider could serve, and the matrix must keep showing that it does not yet.

### D4. LXC needs its own boundary decision before a single route

62 routes, all excluded for now, with the reason on every line. This engine's container is
its own rootless runtime over the kernel; a Proxmox LXC container would be the first
container served by a REMOTE provider, and the questions it raises — is it a `Container`
Kind, which capabilities it can and cannot honour (namespaces, the SDN, `exec`, logs), what
`pct` semantics leak through the API — are a boundary, not a slice. It gets an ADR of its
own, after a spike, or it stays excluded.

### D5. Slices, each with an exit criterion, and the rules every slice keeps

| Slice | Deliverable | Exit criterion |
|---|---|---|
| 0 | Versioned endpoint inventory (this ADR, `scripts/proxmox_api_inventory.py`) | Schema source, release, method/path denominator, states with reasons, called routes read from the source; baseline reproduced by someone else with the docstring command — **done for 9.2.2**; the schema is committed with provenance (`docs/proxmox/api-9.2.2.routes.json`), the matrix is generated (`docs/proxmox/matrix-9.2.2.md`) and gated, and the five states of the brief (`supported+tested` from a route trace of a live run, `supported+untested`, `unsupported-by-design`, `not-yet-implemented`, `not-available-in-version`) replace the three |
| 1 | Hardened transport and task handle | **Built**: 16 MiB response bound; `Auth`/`Ticket` `Debug` redacted and a test that greps every rendered error, `Debug` and the trace file for the secret; typed status errors (`Unauthorized`/`Forbidden`/`NodeNotFound`/`NodeConflict`/`BadRequest`/`NodeUnavailable`/`ResponseTooLarge`, DX-1526/4504/5504/6506/9515–9517 — the `Node` prefix keeps `arch_fitness.py`'s raw-variant counter from reading a local variant as a raw match on the shared class); a task LEDGER per VM written before the wait and settled after, reconciled before the next operation; a lost answer reconciled through `GET /nodes/{node}/tasks?vmid=…&source=active` and an effect probe, never resent; failure injection against a TLS mock node (14 scenarios: TLS refused/accepted-by-CA, 401 renew-once/token-never, 403/404/409/400/5xx, truncated body, unexpected JSON, stalled answer, oversized body, task failed, task timed out, leftover task, lost answer ×3). **Live run done (2026-09-23)**: `tests/live.rs` walks create → snapshot (RAM) → rollback → stop → resume → stop → destroy against a real PVE 9.2.2 node with the trace on; the committed trace promotes 14 of 17 called routes to `supported+tested` and the gate regenerates the matrix WITH it. Measured there and not assumed: a `stop` submitted within ~30 s of a `start` or a RAM rollback fails on the node's 10 s config lock, the client's lock retry resubmits it (2 logical stops, 4 `qmstop` tasks, 2 failed on the node), and the ledger keeps every one. Still untested live: `clone`/`config` (need a template on the node) and `GET /nodes/{node}/tasks` (lost-answer path, failure injection only) |
| 2 | VM operations mapped to engine semantics | Resize (`…/resize`), disks and NICs beyond `config`, cloud-init through `config`, per-VM backup and restore (`…/vzdump`, `…/qemu` restore) — each behind a capability name from ADR-0044 D2; contract tests plus a `tests/live.rs` case per operation. **Resize done (2026-09-24)**: `VmConfig.disk_size_gib` was neither read nor refused by this backend (the ADR-0044 D1 class — honoured by the local overlays, dropped here with the command reporting success); it now sizes a template clone's boot disk through `PUT …/resize` after `configure_clone`, is the same number said twice or a refusal next to a fresh `<storage>:<gib>`, and a shrink is refused by name with both sizes BEFORE the clone exists (the node's own «shrinking disks is not supported» arrives inside a failed task). The clone source of the live case is made with `POST …/template` (`mark_template`, a client call — no engine verb yet), which is also what promoted `clone` and `POST …/config` from `supported+untested`. Catalogue: `vm.disk.resize` and `vm.template` are **partial** on purpose (no engine verb resizes an EXISTING VM or marks a template; the local backends declare the same capability `not-implemented` for the same reason), `vm.clone` is supported with the live case as evidence. Still open in this slice: disks/NICs beyond `config`, the rest of cloud-init through `config`, per-VM backup and restore **Proxmox SDN, the layer above zones/vnets/subnets, done (2026-09-25):** IPAM controllers (6 routes), DNS controllers (5), fabrics and fabric nodes (13 cluster routes + the 4 node-side reads), `GET /nodes/{node}/sdn/zones/{zone}/content`, a zone's `dhcp`/`ipam`/`dns` fields, a subnet's `dhcp-range`/`dhcp-dns-server`/`snat`, and IP reservations (`…/vnets/{vnet}/ips`, 3) — 30 routes, all live-tested in one case that applies and reads the vnet back as `available`. Matrix after it: **102 of 675 called (15.1 %), 99 seen live**. Three facts the schema does not state, measured: the node VERIFIES an IPAM/DNS controller by calling its URL before staging it (an unreachable one is a hung request, so the live case runs a stub the node reaches at `DELONIX_PROXMOX_TEST_CALLBACK_ADDR`); the fabrics API answers writes with an empty string, not `null`; `PUT …/ips` moves a MAC to a new IP, never an IP to a new MAC. And one defect in THIS repository found by the same run: the appliance's `proxmox_postinstall.py` rewrote `/etc/network/interfaces` without `source /etc/network/interfaces.d/*`, so every SDN apply on a published image ended `TASK OK` with a warning and realized nothing — fixed in the script; the lab node was patched by hand and given `dnsmasq` |
| 3 | Cluster-dependent operations | Capability discovery for storage, migration, HA and SDN on the node's cluster; **no implicit node selection**; integration on the supported topology — the three-node `ngola-lda` cluster above is the named target |
| 4 | Administration contract, if ever | Its own ADR, threat model, per-route permission, audit trail, deny by default — and it sits under ADR-0010: local socket, never remote |

Rules that every slice keeps, because each has already cost a bug in this repository:

1. **No new call skips `wait_task`.** A write route answers with a UPID; the verdict is
   the task's, and a test asserts every write site takes the `task(...)` path.
2. **No retry of a non-idempotent write because the connection failed AFTER submission.**
   `create`/`clone`/`snapshot` submitted and then lost must first reconcile the remote
   state (`GET …/config`, `GET …/snapshot`) before any resend. The lock-timeout retry that
   exists today is a retry of a task that REPORTED failure, which is a different case; slice
   1 writes down which is which.
3. **No `vmid` leaves the engine as a resource identity.** It is the provider's reference,
   stored in the record, never the name a caller addresses.
4. **A field the provider cannot honour is refused by name** (`refuse_unsupported` today,
   `VmSpec`/`Extensions` after ADR-0044), never accepted and dropped.

### D6. There is no generic passthrough — in `VmBackend`, in `VmProvider`, or in the node API

An unrestricted (method, path, body) RPC lets a caller bypass the engine's validation and
its privilege boundary, and reports a task-producing HTTP 200 as a result. It is refused as
a design, not deferred. The engine validates its own contract and never trusts a caller to
refuse what it does not refuse itself.

## Alternatives considered

- **A generic administration/passthrough RPC on the node API.** Rejected — guardrail #6
  and ADR-0010 together; also the one shape that makes «100 % of the API» trivially and
  falsely true.
- **Keep the per-area hand-written verdict as THE number.** Rejected — it is a useful
  map and an unreproducible percentage; without a denominator, «10–15 %» and «2.4 %» cannot
  even be compared.
- **Vendor `apidoc.js` into the repository per release.** Rejected for now — 4 MB of
  generated JavaScript per release, and a schema no test in CI can compare against a node.
  The sha256 and the counts are recorded; the fetch is one `curl` from any node.
- **Do nothing.** Rejected — the workspace already carries a skill whose contract is
  «100 % of Proxmox»; leaving that sentence without a denominator is the claim this
  repository refuses to make about Docker.

## Consequences

- The published coverage number **drops** from «~10–15 %» to **2.4 %** of routes. That is
  not a regression of the code; it is the first number with a denominator.
- `scripts/proxmox_api_inventory.py` becomes a gate-in-waiting: it cannot run in CI (it
  needs a schema from a node), but it fails on a called route the target release lacks,
  and its `EXCLUDED` table is the only hand-maintained part — the classic trap. Two things
  limit it: an excluded route still counts in the denominator, and moving a route from
  `excluded` to `called` changes nothing but the state, so the table can only make the
  number SMALLER, never larger. `scripts/test_proxmox_api_inventory.py` pins the reading
  discipline (verb read from the statement, not from the neighbouring function; a comment
  is not a call; a literal with no verb is not a call).
- Slices 1–3 are work, not bookkeeping: backup/restore, resize, migration and every
  cluster-dependent capability are **missing** today, and the draft's slice 2 listed them
  as if they were a mapping exercise.
- Whether the matrix is later served by `delonix compatibility proxmox` (the same three
  states `compatibility docker`/`compose` already print) is not decided here: it would need
  the schema at run time, and a command that prints a matrix without a node to fetch it
  from would print nothing honest.

## What this ADR does not decide

- LXC (D4) — its own ADR after a spike.
- An administration gateway (D6, slice 4) — its own ADR, under ADR-0010.
- Any Proxmox release other than 9.2.2. A second release enters the matrix the day a node
  of that release is reachable and its schema is fetched; a compatibility claim for a
  release nobody has measured is a claim without a denominator.
- Who consumes this. The engine does not know its consumers (`AGENTS.md`, «Identidade e
  fronteira do motor»); the draft's open question about «the external consumer's node API
  and deployment mode» is answered by the contract itself — `proto/delonix/node/v1`, served
  locally — and whoever consumes adapts to it.

## The draft's open questions, answered where they can be

1. **Target release and schema:** PVE 9.2.2, the `apidoc.js` of the `ngola-lda` cluster,
   fetched 2026-09-23 (D1). Nothing else is claimed.
2. **Administration operations a client needs beyond workload operations:** none named. A
   need is written as the engine capability it is, in the engine's vocabulary, and enters
   only if it serves any client; until one is named, everything in D3 stays excluded.
3. **The consumer's API and deployment mode:** not the engine's question (above).

## Proven vs not validated

**Proven (measured in this pass):** the 675-route denominator of PVE 9.2.2 from the real
cluster; the 16 called routes, read from the source and all present in the schema; the
three-state matrix reproduced by the script (16 / 364 / 295) and its six tests green;
TLS verified by default, timeouts, token/ticket handling and the `exitstatus` discipline,
read from the code at the cited lines.

**Not validated here:** nothing was run against the cluster beyond fetching the public
schema; the live results this ADR counts as «tested» are ADR-0039's and
`docs/discovery/58_…`'s, not re-run today; whether the credential can reach a log (slice 1
measures it); the three-node topology as a slice-3 target (nothing about Ceph, SDN or
migration has been called, ever — the matrix says 0).

**Added 2026-09-25 (slice 2, Proxmox SDN layer):** the live case for controllers, fabrics,
DHCP and IP reservations ran against the same lab node (PVE 9.2.2), 14 tests green, node left
empty; the trace is appended to `docs/proxmox/trace-9.2.2.routes` and the matrix regenerated
with it (102 called, 99 live). What it proved beyond the routes: a vnet reported `available`
by `GET /nodes/{node}/sdn/zones/{zone}/content` only after the node's `interfaces` file
carried the `source` directive — before that the same apply returned `TASK OK` and the zone
was `error: vnet is not generated`, which is why the case asserts `available` and not the
task's verdict. Not validated: a fabric with more than one node (no neighbour on a single
node, so `neighbors` is asserted empty), OSPF (only OpenFabric was staged and applied), a
real NetBox/PowerDNS behind the controller entries (the stub answers `200` to the node's
verification and nothing else), and DHCP leases actually handed to a guest (no guest was
attached to the vnet).

**Added 2026-09-23 (slice 1 live run):** `tests/live.rs` ran with `DELONIX_PROXMOX_TRACE_ROUTES`
against a PVE 9.2.2 node booted from this repository's appliance image (a libvirt VM, not the
`ngola-lda` cluster — the cluster is the slice-3 target, and nothing there was touched). The
trace is committed (`docs/proxmox/trace-9.2.2.routes`, 92 requests, header = provenance) and
the gate regenerates the matrix with it: **14 tested / 3 untested of 17 called**. The three
untested are named in the trace header with the reason. The credential did not reach the
trace file (the URL is the only thing written, and the test greps the mock's outputs for the
secret; the live file was read and has neither the password nor a ticket). Not measured:
the guest-agent `ip()` path (needs a prepared guest, `DELONIX_PROXMOX_TEST_AGENT_VMID`), and the
per-route behaviour against the real three-node cluster.

**Added 2026-09-23 (second live run, after the first review of this ADR against the code):**
three rule violations the review found are closed. (1) `cfg.network` was accepted and dropped —
rule 4 said "refused by name", the code admitted the gap in a comment; it is now refused by
`refuse_unsupported` when it names an engine network (the CLI's default `ingress` stays
accepted). (2) `POST …/config` was the one write outside `task(...)`, on the reading that it
"answers `data: null`"; the schema says `returns: string` and the node's own description says
"asynchronous API" — it answers a UPID when it forks a worker. It goes through `task_or_done`
now: a UPID is waited on and recorded, a `null` is done, anything else is an unexpected answer.
(3) Rule 1 was prose: `every_write_to_the_node_goes_through_the_task_path` reads the crate's
source and fails on any `post_form`/`delete` call outside a `task(...)` argument list, with the
login as the one named exception (verified to fail with a rogue write injected). And the
snapshot quadrant is complete — `DELETE …/snapshot/{snapname}` (`qmdelsnapshot`) is a task like
the others; the live run walks create → snapshot → rollback → **delete-snapshot** → stop → resume
→ stop → destroy, and the trace promotes the route to `supported+tested` (18 called, 15 tested).
Measured in that run: a delete submitted right after a RAM rollback hits the same 10 s config
lock as a stop does, and the lock retry resubmits it — the ledger keeps every attempt.

**Added 2026-09-24 (slice 2, first operation, live):** `tests/live.rs::a_template_clone_gets_the_disk_size_asked_for`
ran against the same appliance node (PVE 9.2.2, `pve-lab-475`), with the trace on, together with
the two earlier cases: **3 passed, 239 requests**, and the committed trace (`docs/proxmox/trace-9.2.2.routes`)
regenerates the matrix at **19 tested / 1 untested of 20 called** — `PUT …/resize` and
`POST …/template` entered as `supported+tested`, `POST …/clone` and `POST …/config` moved from
`supported+untested` to tested (the case makes its own template, so the "needs a template on the node"
reason is gone). Measured there and not assumed: the node registers the two workers as `resize`
(no `qm` prefix) and `qmtemplate`, and the client's `worker_type` table carries those names — the
lost-answer path finds a resize by that name; a clone asked for the template's own size submits
no resize task (the ledger has none); a 1 GiB clone of a 2 GiB template is refused before
`clone` is sent (the node's next free id is unchanged after the refusal); and the node's VM list
was empty after the run. Not measured: `diskSize` on a clone whose boot disk is not the
template's first sized drive (the boot-disk choice — `boot: order=…`, then `bootdisk`, then the
lowest-numbered sized drive — is unit-tested against captured configs, not exercised on the
node), and a resize of a RUNNING clone (the case resizes before the first start, which is where
`boot` does it).
