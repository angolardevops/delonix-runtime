# ADR-0040: Engine restructuring — layers, provider ports, and the node contract

- **Status:** Proposed (2026-09-15)
- **Deciders:** Walter (owner)
- **Related:** ADR-0002 (ComputeDriver, Phase 2b trigger), ADR-0008 (VM backend registry),
  ADR-0009 (TrueNAS), ADR-0010 (remote management API — **stands**), ADR-0025 (MCP),
  ADR-0038 (CRI follows the kubelet resource model); the `delonix-paas` restructuring plan
  «Saneamento do Control Plane» (2026-09-15), requests R1–R5; `proto/delonix/node/v1/`.

## Context

The engine is going to be driven by three kinds of consumer at once: a human through the
CLI, the kubelet through the CRI, and the control plane's **node agent** (`ngc-agent`,
one per node, from the `delonix-paas` plan). The owner asked for a restructuring that
gives each of them a clean boundary, extensible providers, a complete API and
production-grade observability — before production, so renames and moves are acceptable.

**Measured on `origin/main` (`58fdc4e3`, 2026-09-15). Nothing below is assumed.**

### 1. The application layer lives inside a binary that nothing can link

- `delonix-runtime-bin` is **91 223** lines; `src/cmd/` alone is 89 815 (18 370 of them
  tests). It has **no `[lib]` target** (`Cargo.toml:10`).
- Business logic sits there, not argument parsing: `cmd_run` is 1 044 lines
  (`container.rs:3052-4096`); the manifest planner (`reconcile.rs`, `kinds.rs`,
  `manifest.rs`), `compose.rs`, `stack.rs`, `cluster.rs` and `vmimage.rs` are 60–90 %
  business logic by a line-pattern estimate.
- **Consequence — a dependency cycle hidden behind a process boundary.** The binary
  depends on `delonix-cri`, `delonix-mgmt` and `delonix-mcp`; all three call the binary
  back as a subprocess:
  - the CRI builds a full `container run -d` argv (`start_argv`,
    `delonix-cri/src/lifecycle.rs:1037-1231`) and forks it for every lifecycle call
    (10 call sites, plus streaming);
  - `delonix-mgmt` rebuilds CLI flags from an 11-field `RunSpecBody` (`lib.rs:633`) against
    the CLI's 67-field `RunOpts`, runs it (15 call sites) and returns the printed text;
  - `delonix-mcp` does the same (`run_cli_blocking`, `lib.rs:821`).
- **Four translators** into a run specification exist (Pod, compose, Docker API, and the
  two argv builders). Only three are compiler-checked against `RunOpts`.

The `delonix-paas` audit measured the mirror image on its side: ~7 000 lines copied from
`delonix-runtime-bin` and 88 subprocess calls, **for the same reason** — the logic is in
a crate that cannot be linked.

### 2. Ports are few, and the one that exists leaks its providers

- There are **two** non-test traits in the workspace: `VmBackend` (`delonix-vm/src/lib.rs:700`)
  and `ComputeDriver` (bin, `workload.rs:254`). Network drivers, storage provisioners, the
  image registry and the scanner have no port: adding a provider means editing a `match`
  (`network.rs:1046`) or a call site (`provision.rs:224`).
- `VmBackend::boot` takes `VmConfig`, which carries `libvirt_xml`, `libvirt_xml_overlay`,
  `machine`, `tpm`, `video`, `cpu_model`, `boot_order`, `net_mode`, `bridge`, `firmware`
  (`lib.rs:53-194`). The shared `Vm` record reuses `tap` to store libvirt's network mode
  (`lib.rs:4419`). Every provider quirk so far became a new trait method
  (`ip_is_predicted`, `manages_own_storage`, `auto_selectable`, `preserve_snapshots`).
- The CLI still branches on provider names as strings (`vm.backend.contains("libvirt")`,
  `vm.rs:1934, 2673, 3023, 3177`).

### 3. Dependencies point the wrong way

- `delonix-vm` calls 13 internals of `delonix-net::infra` (`vm_attach`, `dhcp_ip_for_mac`, …).
- `delonix-runtime-core` carries OTLP, Prometheus, a hard-coded root cgroup path
  (`Container::cgroup()`, `lib.rs:1047`) and **one error enum for every context**
  (`error.rs:7`, with registry- and VM-specific variants).
- "runtime" names three different things: the kernel adapter (`delonix-runtime`), shared
  types + stores + telemetry (`delonix-runtime-core`), and the CLI + application layer
  (`delonix-runtime-bin`).

### 4. The interfaces are partial and uncontracted

| Surface | Today |
|---|---|
| gRPC | Only the CRI. `UpdateContainerResources`, `GetContainerEvents`, `CheckpointContainer` unimplemented; `RuntimeConfig` empty; no health/reflection service |
| HTTP (`delonix-mgmt`) | 40-odd routes on a unix socket, same-uid only; no OpenAPI, no async operations, no pagination, no idempotency, mutations return CLI stdout |
| Metrics | 14 series, no labels; `_total` on gauges, units not suffixed |
| Tracing | Spans only in the CRI; no context propagation across processes; reads `DELONIX_OTLP_ENDPOINT`, not the standard `OTEL_*` |
| Logs | The CLI crate has 623 `println!`/`eprintln!` and no `tracing`; the `delonix-runtime` library has 47 bare `eprintln!` |

### 5. The kubelet boundary is not production-grade yet

Beyond ADR-0038: stats read the wrong cgroup in rootless and report a writable-layer path
that does not exist (eviction is blind); every lifecycle call forks the CLI and every list
scans a directory; no container events, so the kubelet polls that expensive path; CRI
records are written without a lock and with a temp-file name shared across threads
(`lifecycle.rs:316`); `runtime_handlers` is empty (no RuntimeClass); conformance is
**79/103, rootless only**.

### Guardrails this decision touches

#1 daemonless · #2 PaaS boundary (no tenant) · #4 engine crates dependency-clean ·
#6 no silent failure. ADR-0010 (the API stays local). ADR-0002's Phase 2b trigger — *a
second consumer beyond the CLI* — is now met three times over (CRI, node agent, MCP).

## Decision

### D1. Four layers, one dependency direction

```
delivery   ─►  application  ─►  domain
   │               ▲
   └── composes ── adapters (implement application ports)
```

- **Domain** — types, invariants and pure rules. No I/O, no `tokio`, no `libc`, no `nix`,
  no `std::fs`. Errors per bounded context.
- **Application** — use cases (`RunContainer`, `ApplyStack`, `CreateVirtualMachine`, …)
  and the **ports** they need. Knows no kernel, no HTTP, no provider.
- **Adapters** — implement ports: kernel, SDN, filesystem stores, OCI registry, each VM
  and storage provider.
- **Delivery** — CLI, node API (gRPC + REST), CRI, MCP, Docker API shim. Parse, call a use
  case, present. The **composition root** (which adapters back which ports) lives here and
  only here.

### D2. Target crate map

Crates are renamed **when they are restructured, never twice** (the same rule as the PaaS
plan). Names describe the role; "runtime" stops meaning three things.

| Layer | Target crate | Comes from |
|---|---|---|
| domain | `delonix-domain` | types and `Status` from `runtime-core`; `delonix-net-rules` |
| domain | `delonix-manifest` | `cmd/{kinds,reconcile,manifest,schema}.rs` — the Kind table, planner, generated schema |
| domain | `delonix-security` | `delonix-security-runtime` (already clean) |
| application | `delonix-app` | use cases out of `cmd/*`; the ports; `ComputeDriver` (ADR-0002 Phase 2b, option B) |
| adapter | `delonix-linux` | `delonix-runtime` (namespaces, cgroups, mounts, seccomp) |
| adapter | `delonix-sdn` | `delonix-net` (holder, nftables, slirp, DNS, overlay) |
| adapter | `delonix-store` | `Store`/`JsonStore`/secret vault out of `runtime-core` |
| adapter | `delonix-oci` | `delonix-image` + `delonix-scan` |
| adapter | `delonix-volume` | unchanged name |
| adapter | `delonix-provider-cloud-hypervisor`, `-libvirt`, `-proxmox`, `-truenas` | split out of `delonix-vm`; `delonix-proxmox`, `delonix-truenas` |
| cross-cutting | `delonix-telemetry` | `telemetry`/`metrics` out of `runtime-core` |
| delivery | `delonix-cli` (binary `delonix`) | `delonix-runtime-bin`, thin |
| delivery | `delonix-node-api` | new; replaces `delonix-mgmt` |
| delivery | `delonix-node-proto` | new; generated from `proto/delonix/node/v1` — the crate the node agent links |
| delivery | `delonix-cri`, `delonix-mcp` | unchanged names |

Not `delonix-api`/`delonix-core`: those names belong to the private monorepo, and a
public crate with the same name is a guardrail #3 confusion waiting to happen.

### D3. Provider ports with capability discovery

Every provider port has the same skeleton — **identity, availability, health,
capabilities, lifecycle, error mapping** — as `ngolacloud-arch` §4 requires:

```rust
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn capabilities(&self) -> &CapabilitySet;   // named, optional features
    fn health(&self) -> Condition;               // never a bare bool
}

pub trait VmProvider: Provider {
    fn create(&self, spec: &VmSpec, ext: &Extensions) -> Result<VmHandle, VmError>;
    fn start(&self, h: &VmHandle) -> Result<(), VmError>;
    fn stop(&self, h: &VmHandle) -> Result<(), VmError>;     // keeps the disk
    fn destroy(&self, h: &VmHandle) -> Result<(), VmError>;  // releases it
    fn observe(&self, h: &VmHandle) -> Result<VmObservation, VmError>;
    // optional operations are capabilities, not default methods returning "unsupported"
}
```

The ports: `WorkloadRuntime` (container lifecycle), `SandboxProvider` (pod namespaces),
`VmProvider`, `NetworkProvider`, `StorageProvider` (mount + optional provisioning),
`ImageRegistry`, `ImageStore`, `StateRepository<T>`, `SecretVault`, `EventSink`,
`ProcessLauncher`, `Clock`.

Three rules make the port closed to modification and open to extension:

1. **Neutral spec + namespaced extensions.** `VmSpec` holds what every VM has. Provider
   knobs (`libvirt_xml`, `tpm`, a Proxmox storage pool) travel in `Extensions`, keyed by
   provider id; **each provider validates its own entry and rejects unknown keys**
   (guardrail #6). Adding a provider never edits `VmSpec`.
2. **Quirks become capabilities or observations, not trait methods.** `ip_is_predicted`
   becomes a field of `VmObservation`; `snapshot`, `pause`, `live-migration` become
   capabilities. A request needing a capability the provider lacks fails closed with
   reason `CapabilityNotSupported`.
3. **No string matching on provider names outside the composition root.** A fitness test
   (D7) enforces it.

The substitution test (`ngolacloud-arch` §7) becomes a CI check: the same `VmSpec`
converges on Cloud Hypervisor and libvirt with **only** composition-root files changing.

### D4. The node contract: gRPC + REST, local, socket-activated

Aligned with the PaaS plan (request R1) and **without reopening ADR-0010**:

```
ngc-api (control plane) ──gRPC, network, mTLS──► ngc-agent (one per node, PaaS repo)
                                                     │
                                         unix socket, SO_PEERCRED
                                                     ▼
                                     delonix serve api  (this engine)
```

- **One contract, two encodings.** `proto/delonix/node/v1/*.proto` is the source of truth
  (drafted with this ADR: 5 files, 58 RPCs, compiles with `protoc`). The same server
  answers gRPC and HTTP/JSON on the same unix socket; the REST mapping is generated from
  `google.api.http` annotations (vendored in P1), and **OpenAPI 3 is generated from the
  proto**, never written by hand. DevOps tooling on the node (Ansible, Terraform over SSH,
  scripts, Prometheus) uses the REST encoding; the agent uses gRPC.
- **Local only.** The transport stays a unix socket with `SO_PEERCRED`. Remote access,
  identity, tenant mapping and audit of remote callers live in the agent and the control
  plane. A DevOps team integrating from **off** the node goes through the control plane's
  public API, not through this socket. Opening TCP here is still ADR-0010's question and
  still answered no.
- **No daemon (guardrail #1).** `delonix serve api` is started by systemd socket
  activation (`LISTEN_FDS`) and may exit when idle, exactly as the CRI unit does today.
  Long-running work is an `Operation` **persisted under the state root before it is
  acknowledged**; an operation cut by a restart ends `FAILED/Interrupted`, never `RUNNING`
  forever.
- **API conventions** (Google AIP style): resource-oriented `Get/List/Create/Update/Delete`;
  `request_id` idempotency on every mutation; `etag` optimistic concurrency; `FieldMask`
  updates (the hot reconfiguration the CLI already has); page tokens; `label_selector`;
  errors as gRPC status + `ErrorDetail.reason` carrying the same `DX_*` code the CLI exits
  with. **No tenant field anywhere** (the `namespace` is the engine's isolation namespace).
- **Streaming**: `Exec`, `Logs`, `Console` and `WatchEvents` as gRPC streams; WebSocket on
  the REST side.
- **Health and capacity** (R5): `NodeService.GetHealth` returns one condition per
  dependency (network, store, cgroup delegation, providers) and `GetCapacity` lists what it
  could **not** measure instead of reporting zero.
- `delonix-mgmt` is **deprecated on arrival** of the node API and removed once the agent
  has migrated; it was never declared stable (`cli-stability.md`).

### D5. The CRI calls the application layer, not the CLI

- The reason the CRI forks today is real: `clone` is unsafe inside a multi-threaded tokio
  process. The fix is **not** to keep forking the CLI but to put that constraint behind the
  `ProcessLauncher` port: a small re-exec helper (`delonix __launch`) receiving a typed spec
  over an fd — the pattern `__apirun` already uses for the Docker API. The CRI maps
  `runtime.v1` onto `delonix-app` use cases in-process; only the final spawn crosses a
  process boundary, and it carries a typed spec, not an argv.
- The CRI gaps in §5 become the CRI track of the plan (P6), with ADR-0038 as its resource
  policy. RuntimeClass handlers (`container`, `microvm`) get their own ADR when the
  microVM path is ready.

### D6. Observability conventions

- **Tracing everywhere, printing nowhere in libraries.** Library crates use `tracing` only;
  the CLI prints user output through a presenter module and logs through `tracing`.
  Fitness test: zero `println!`/`eprintln!` in non-delivery crates.
- **OpenTelemetry semantic conventions.** Read the standard `OTEL_EXPORTER_OTLP_*`,
  `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` (keep `DELONIX_OTLP_ENDPOINT` as an
  alias for one release); resource attributes `service.name`, `service.version`,
  `host.name`, `os.type`; OTLP over gRPC and HTTP; flush on CLI exit.
- **W3C trace context propagated** through gRPC metadata, HTTP headers, the re-exec
  boundary (`TRACEPARENT` env) and persisted `Operation.trace_id`, so a kubelet call, an
  agent call and the spawn they cause are one trace.
- **Metrics follow Prometheus/OpenMetrics naming**: unit suffix last, `_total` only on
  counters, state in labels (`delonix_containers{state="running"}`,
  `delonix_storage_used_bytes{area="volumes"}`), bounded label cardinality (no container
  ids on node-level series). Exposed at `/metrics` on the API socket and via OTLP. A
  renaming table ships with P7.
- **Logs**: JSON with `trace_id`/`span_id` when `DELONIX_LOG_FORMAT=json`; stable `event`
  field names; secrets redacted by `delonix-security`'s redactor before any sink.
- **Events**: the persisted event log stays the single source; `WatchEvents`, CRI
  `GetContainerEvents` and the Docker `/events` shim all read it.

### D7. Architecture fitness functions in CI

Written **before** code moves (P0), so the restructuring cannot regress while it happens:

- a `cargo metadata` test: domain crates depend on no `tokio`, `nix`, `libc`, `reqwest`,
  `axum`; application depends on no adapter; only delivery crates compose adapters;
- no `Command::new(current_exe())` or `delonix` subprocess outside `ProcessLauncher`;
- no provider-name string matching outside composition roots;
- ratchets (fail on increase **and** on unrecorded decrease, like `lang_ratchet.py`):
  lines in the CLI crate, `println!` in libraries, subprocess call sites;
- `buf lint` + `buf breaking` on `proto/` against the last tag.

## Plan — phases with gates

Every phase lands as its own PR from an integration worktree, full test battery green
**there**. Moves of whole modules (P2, P3) need a **coordination window**: announce the
module, merge open PRs that touch it first, then move — several sessions work in the same
crates.

| Phase | Work | Gate (measured, in CI) |
|---|---|---|
| **P0 rails** | D7 fitness tests and ratchets at today's numbers; `[workspace.lints]` (`undocumented_unsafe_blocks = deny`); workspace-level dependency versions | CI red on any new layer violation, new subprocess call or new library `println!` |
| **P1 contract** | Vendor `google/api/http.proto`, add REST annotations; `delonix-node-proto` crate; generated OpenAPI in `docs/api/`; `buf` in CI | contract compiles, lint clean, breaking-check wired; OpenAPI regenerated = committed |
| **P2 extract** | `delonix-manifest` first (already pure), then one `ContainerSpec` replacing the four translators, then container/pod/vm/network/volume/image/stack use cases into `delonix-app` — one context per PR, no behaviour change | CLI crate lines ratchet down each PR; e2e battery unchanged; each migrated use case has zero subprocess paths in CRI/mgmt/mcp |
| **P3 split core** | `delonix-domain` / `delonix-store` / `delonix-telemetry`; errors per context with a mapping to `DX_*`; `vm → net::infra` calls moved behind `NetworkProvider` | fitness test green with domain free of I/O; `delonix-vm` no longer depends on `delonix-net` |
| **P4 ports** | D3: neutral `VmSpec` + `Extensions`; provider crates split; `NetworkProvider`, `StorageProvider`, `ImageRegistry` ports | substitution test green (CH ↔ libvirt); zero provider-name matching outside composition |
| **P5 node API** | `delonix-node-api` serving gRPC + REST on the socket, socket activation, persisted operations, `WatchEvents`, health, capacity | contract conformance suite against the generated client; `delonix-mgmt` routes all mapped |
| **P6 CRI** | in-process CRI over `delonix-app`; stats/eviction, events, `UpdateContainerResources`, record locking, digests, RuntimeConfig | critest ≥ current in rootless **and** a root run published; real-kubelet e2e on a node |
| **P7 observability** | D6 in full: semantic conventions, propagation, metric renames, JSON logs | one trace spans kubelet → CRI → spawn in a recorded run; zero library `println!` |

P0 and P1 do not move code and can start immediately. P2 unlocks the node agent's
parity work on the PaaS side (their F3) and is the long pole.

## Alternatives considered

- **Big-bang rewrite into the new layout.** Rejected: 170 k lines, several concurrent
  sessions, and working paths validated live (rootless networking, overlay, CRI). The
  strangler order above keeps `main` releasable after every PR.
- **Expose gRPC/REST over TCP with mTLS in the engine.** Rejected, and ADR-0010 already
  argued why: identity, authorization, certificate lifecycle and audit of remote callers
  would move into a daemonless node engine that has no notion of who the caller is. The
  node agent carries them, on the side of the boundary that knows the tenant.
- **Keep `delonix-mgmt` and widen it.** Rejected: hand-written routes, no contract,
  mutations implemented by shelling out. Widening it multiplies exactly the translation
  layer this ADR removes.
- **A `delonixd` daemon to serve the API and a continuous event bus.** Rejected here
  (guardrail #1). Socket activation plus a persisted event log and persisted operations
  cover the node agent's needs; a resident process would need its own ADR with evidence
  of what activation cannot do.
- **Put ports in `delonix-runtime-core`.** Rejected: core is already the sink of every
  crate and carries infrastructure; ports belong to the application that needs them, and
  a separate crate keeps the domain free of I/O.
- **One enum of providers instead of traits.** Rejected: a closed enum means every new
  provider edits the contract, which is what the owner asked to stop.

## Consequences

**Easier:** one run specification and one set of use cases for CLI, CRI, API and MCP; a
new VM/storage/network provider is a new crate plus a composition-root line; the node
agent links a generated client instead of copying engine code; failures classify the same
way in every interface; one trace across the kubelet boundary.

**Harder / cost:** a long migration with coordination windows; crate renames touch
`delonix-paas` (pin), `delonix-deploy` and docs (the PaaS plan measured ~74 references to
the runtime); two API servers coexist until `delonix-mgmt` is removed; the proto becomes a
stability promise that `cli-stability.md` must record; `tonic`/`prost` move from the CRI
crate to the node API as well (already in the tree — no new supply-chain surface), while
`buf` enters CI tooling only.

**Guardrail audit:** #1 socket-activated server, no resident process ✅ · #2 no tenant
field in the contract, tenant mapping in the agent ✅ · #3 no private dependency ✅ ·
#4 engine adapters stay dependency-clean; gRPC/HTTP deps confined to delivery crates ✅ ·
#5 no new privilege boundary in this ADR; the `ProcessLauncher` re-exec reuses the
`__apirun` pattern and gets a `delonix-runtime-sec` pass before P6 merges ✅ · #6 unknown
extension keys and unsupported capabilities fail closed; unmeasured values are reported as
unmeasured ✅.

## What this ADR does not decide

- The OCI runtime-spec/containerd-shim-v2 interop and RuntimeClass `microvm` handler —
  separate ADR once P6 lands.
- Moving the VM image factory (`vmimage.rs`, 7 482 lines) out of the engine — it is a
  build tool, not a node operation; flagged for P2 review.
- The exact metric rename table (P7) and the API stability tier of each service (P5).

## Proven vs not validated

**Proven** (read or measured on `origin/main`, with the references above): crate
dependency graph and sizes; the binary has no lib target; the subprocess call sites in
CRI/mgmt/mcp; the leaking `VmConfig` fields; the missing ports; the telemetry and logging
counts; the CRI gaps; the draft proto compiles with `protoc` (58 RPCs).

**Not validated:** the business-logic percentages are a line-pattern estimate; no build or
test was run for this ADR (it moves no code); the latency cost of the agent hop and of
in-process CRI dispatch is unmeasured and is a P5/P6 gate; socket activation of the API
server has not been spiked; the `google.api.http` REST mapping is designed but not yet
annotated.
