# ADR-0040: Engine restructuring — layers, provider ports, and the node contract

- **Status:** Proposed (2026-09-15)
- **Deciders:** Walter (owner)
- **Related:** ADR-0002 (ComputeDriver, Phase 2b trigger), ADR-0008 (VM backend registry),
  ADR-0009 (TrueNAS), ADR-0010 (remote management API — **stands**), ADR-0025 (MCP),
  ADR-0038 (CRI follows the kubelet resource model); the `delonix-paas` restructuring plan
  «Saneamento do Control Plane» (2026-09-15), requests R1–R5; `proto/delonix/node/v1/`.

> **Amendment — the engine knows no consumer (owner, 2026-09-16).** The canonical rule is
> «Identidade e fronteira do motor» in `AGENTS.md`. Where this ADR describes the node
> contract through one consumer (a control-plane agent, a platform plan and its requests),
> read it as the origin of a requirement, not as its shape: the contract serves **any local
> client** and names none (D4 below), phases are ordered by the engine's own capability
> coverage, and nothing in `crates/`, `bins/` or `proto/` may name a consumer — enforced by
> `scripts/arch_fitness.py` from P0 on. The decisions D1–D7 are unchanged.

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
interfaces ─► contexts (domain + app) ◄─ adapters / providers
     │                                        ▲
     └──────────── composes (bins/) ──────────┘
```

- **Foundation** — `delonix-model`: identifiers, `ResourceMeta`, `Status`, `Condition`,
  quantities and the `DX_*` error codes every context maps to. Pure.
- **Contexts** — one crate per bounded context, each with two internal modules:
  `domain/` (types, invariants, pure rules — no I/O, no `tokio`, `libc`, `nix`,
  `std::fs`) and `app/` (use cases such as `RunContainer`, `ApplyStack`,
  `CreateVirtualMachine`, and the **ports** they need). A context knows no kernel, no
  HTTP and no provider.
- **Adapters and providers** — implement ports: kernel, SDN, filesystem state, OCI
  registry, each VM and storage provider.
- **Interfaces** — CLI, node API, CRI, MCP, Docker API shim, as **libraries**: parse,
  call a use case, present.
- **Binaries** — `bins/<name>/src/main.rs` only: the composition root (which adapter backs
  which port) plus the call into one interface library. No business logic in a binary.

### D2. Naming convention, crate map and binaries

#### D2.1 The convention

| Role | Name | Example |
|---|---|---|
| Shared foundation (pure) | `delonix-model` | ids, `ResourceMeta`, `Status`, `DX_*` |
| Bounded context | `delonix-<context>` | `delonix-compute` |
| Technology adapter | `delonix-<technology>` | `delonix-linux`, `delonix-sdn` |
| Pluggable provider | `delonix-provider-<technology>` | `delonix-provider-proxmox` |
| Interface library | `delonix-<protocol>` | `delonix-cri`, `delonix-node-api` |
| Binary | same name as the interface it serves | `bins/delonix-cri` |

A crate exists only if it (a) is a bounded context, (b) isolates a heavy or privileged
dependency (kernel, HTTP, gRPC, guestfs), or (c) is a separately installed binary.
No `-core`, `-common`, `-utils` or `-types`: that is how `delonix-runtime-core` became the
sink of everything. Not `delonix-api`/`delonix-core` either: those names belong to the
private monorepo (guardrail #3 confusion). Crates are renamed **when they are
restructured, never twice**.

#### D2.2 Contexts are named after the API groups already published

The manifest `apiVersion` groups have been public since v0.64.0 (ADR-0020). Using them as
context boundaries gives **one vocabulary** across YAML, proto, crates and docs.

| Published group | Context crate | Holds |
|---|---|---|
| `compute.delonix.io` | `delonix-compute` | Container, Pod, VirtualMachine, Workload; the **one** run specification that replaces the four translators; ports `WorkloadRuntime`, `SandboxProvider`, `VmProvider`; `ComputeDriver` (ADR-0002 Phase 2b, option B) |
| `networking.delonix.io` | `delonix-networking` | Network, NetworkRoute, NetworkPolicy, NetworkAccessRule, Service, Dependency; absorbs `delonix-net-rules`; port `NetworkProvider` |
| `gateway.delonix.io` | `delonix-gateway` | Gateway, HTTPRoute, Ingress — route tables and tunnels; ports `L7Dataplane`, `TunnelProvider` |
| `storage.delonix.io` | `delonix-storage` | Volume, shares, quota, snapshots and resource backups; ports `StorageProvider`, `Provisioner` |
| `artifact.delonix.io` | `delonix-artifact` | Image, App — pull, build plan (Dockerfile/Delonixfile), CNB plan, signing policy, VM image recipes; ports `ImageRegistry`, `ImageStore`, `Scanner`, `ImageBuilder` |
| `infrastructure.delonix.io` | `delonix-cluster` | KubernetesCluster — kubeadm and kind bootstrap, etcd, PKI, load balancer plan; port `RemoteExecutor` |
| `core.delonix.io` (Stack) | `delonix-stack` | manifest loading, the Kind table, the planner, apply/destroy, compose translation, revisions, the generated schema |
| `core.delonix.io` (Secret) | `delonix-security` | secrets and the credential vault model, policy, admission, capability and seccomp profiles, redaction; absorbs `delonix-security-runtime` |
| (node) | `delonix-node` | boot units, prune/GC, events, health, capacity, node snapshot; ports `ServiceManager`, `EventSink` |

#### D2.3 Adapters and providers

| Crate | Comes from | Implements |
|---|---|---|
| `delonix-linux` | `delonix-runtime` | `WorkloadRuntime`, `SandboxProvider`, `DeviceResolver` (CDI) — namespaces, cgroups, mounts, capabilities, seccomp, devices |
| `delonix-sdn` | `delonix-net` | `NetworkProvider` — netns holder, nftables, slirp, DNS, DHCP, overlay, WireGuard, CNI client |
| `delonix-oci` | `delonix-image` | `ImageRegistry`, `ImageStore` — registry client, CAS, layers, overlay, image layout |
| `delonix-scanner` | `delonix-scan` | `Scanner` — SBOM, CVE |
| `delonix-state` | stores of `delonix-runtime-core` | `StateRepository<T>`, `SecretVault` — JSON records with flock, encryption at rest |
| `delonix-l7proxy` | `cmd/ingress_proxy.rs` | `L7Dataplane` — the HTTP proxy |
| `delonix-ssh` | `cmd/remote.rs` | `RemoteExecutor` |
| `delonix-guestfs` | the build half of `cmd/vmimage.rs` | `ImageBuilder` for VM images (`virt-customize`, `qemu-img`) |
| `delonix-telemetry` | `telemetry`/`metrics` of `delonix-runtime-core` | tracing, OpenTelemetry, Prometheus registry |
| `delonix-provider-cloud-hypervisor` | split out of `delonix-vm` | `VmProvider` |
| `delonix-provider-libvirt` | split out of `delonix-vm` | `VmProvider` |
| `delonix-provider-proxmox` | `delonix-proxmox` | `VmProvider` |
| `delonix-provider-openstack` | ADR-0039 (in review) | `VmProvider` |
| `delonix-provider-truenas` | `delonix-truenas` | `Provisioner` |
| `delonix-provider-mount` | `delonix-volume` | `StorageProvider` — local, NFS, SMB, WebDAV |

#### D2.4 Interfaces and binaries — one long-running role per executable

Measured on `origin/main`: the `delonix` executable holds **seven** long-running programs
(CLI, `serve cri`, `serve api`, `serve docker-api`, `mcp`, the hidden `ingress-proxy`,
`netns pin`/`control`) and **nine** internal re-exec entry points (`__apirun`, `__ovlhold`,
`__ovlmigrate`, `__rmtree`, `__duusage`, `__volsnap`, `__buildtar`, `__netnsconnect`, and
the container init). `delonix-cri` additionally has its own `[[bin]]`, so the CRI server
exists twice. The CLI links the servers and the servers exec the CLI back.

| Interface library | Binary | Replaces |
|---|---|---|
| `delonix-cli` (clap tree, presenters, i18n catalog, TUI) | `delonix` | the CLI, with no server inside |
| `delonix-cri` | `delonix-cri` | `delonix serve cri` and the duplicate `[[bin]]` |
| `delonix-node-api` + `delonix-node-proto` | `delonix-node-api` | `delonix serve api` and `delonix-mgmt` |
| `delonix-mcp` | `delonix-mcp` | `delonix mcp` |
| `delonix-docker-api` | `delonix-docker-api` | `delonix serve docker-api` |
| `delonix-l7proxy` | `delonix-gateway-proxy` | `delonix ingress-proxy` |
| `delonix-sdn` | `delonix-netns-holder` | `delonix netns pin` / `control` |
| `delonix-linux` | `delonix-launcher` | the nine `__*` re-execs and the container init |

- ~~**Clean cut, no alias** for `delonix serve …` and `delonix mcp`: they are declared not
  stable in `cli-stability.md`, and the systemd units call the binaries directly. Keeping
  an exec shortcut would put back a second door to the same server.~~
- **Amended 2026-09-17 — `delonix` stays the one door.** The owner's objection: a user
  should only ever need to know `delonix`, and a split that made them learn
  `delonix-cri`, `delonix-node-api`, `delonix-mcp` and `delonix-docker-api` moves the
  restructuring's cost onto the person it is not for. So each server still becomes its own
  executable — the architectural goal, **no binary links another interface's server**, is
  unchanged — and `delonix serve <x>` / `delonix mcp` **`exec` the sibling binary**, the
  way `git lfs` runs `git-lfs`. It is not the "second door" the original bullet refused:
  there is one server, in one executable; `delonix` holds no server code and only finds
  and runs it. Three rules make the shortcut safe rather than a new failure mode:
  - the sibling next to `delonix` wins over the `PATH`;
  - `delonix` tells it the version it must be (`DELONIX_DISPATCH_VERSION`) and a server
    from another release refuses to start — a server from before this rule cannot check,
    so the flags also travel as the environment variables it reads;
  - a missing server exits `69` (unavailable) naming the install step, never a bare
    `No such file or directory`.
  `exec`, not a child process: the server takes the pid, so units, signals and `kill`
  reach it. Units may keep calling the binary directly. First slice: `delonix serve cri`
  → `delonix-cri` (which already existed as a duplicate `[[bin]]`). Second: `delonix mcp`
  → `delonix-mcp` (`bins/delonix-mcp-bin`, installed by default). A server that runs the
  CLI back resolves it through `delonix_runtime_core::dispatch::cli_bin` —
  `DELONIX_BIN`, which `delonix` sets to itself, then the sibling, then the `PATH` —
  never its own executable, which is the server.
- **`delonix-launcher` owns every spawn that creates namespaces.** It is the
  `ProcessLauncher` adapter of D5: it receives a typed spec over an inherited fd, never an
  argv built by another program. The CLI, the CRI and the node API stop creating user
  namespaces themselves; only the launcher and the netns holder do.

#### D2.5 Directory layout

```
crates/
  foundation/   model, telemetry
  contexts/     compute, networking, gateway, storage, artifact, stack, cluster, security, node
  adapters/     linux, sdn, oci, scanner, state, l7proxy, ssh, guestfs
  providers/    provider-cloud-hypervisor, provider-libvirt, provider-proxmox,
                provider-openstack, provider-truenas, provider-mount
  interfaces/   cli, cri, node-api, node-proto, mcp, docker-api
bins/           delonix, delonix-cri, delonix-node-api, delonix-mcp, delonix-docker-api,
                delonix-gateway-proxy, delonix-netns-holder, delonix-launcher
```

The layer is readable from the path, and the D7 fitness test uses it: a crate under
`contexts/` may depend only on `foundation/`; `adapters/` and `providers/` on
`foundation/` and `contexts/`; `interfaces/` on everything but `bins/`; `bins/` composes.

#### D2.6 Where the 80 CLI modules go

| Destination | Modules today (`bins/delonix-runtime-bin/src/cmd/`) |
|---|---|
| `delonix-model` | `names` (generated names, used by compute and cluster) |
| `delonix-compute` | `container`, `pod`, `workload`, `vm` (use cases) — `cdi` is an adapter, not domain: it reads `/etc/cdi` and parses YAML specs, so it lives in `delonix-linux` (today `delonix-runtime`) as the `DeviceResolver` |
| `delonix-networking` | `network`, `netroute`, `firewall`, `network_access_rule`, `service`, `dependency`, `namespace`, `vlan`, `netns`, `flow`, `capture`, `vmbridge` (use cases; their dataplane halves are already in `delonix-sdn`) |
| `delonix-gateway` | `httproute`, `tunnel` |
| `delonix-storage` | `volume`, `storage`, `sharevolume`, `provision`, `backup`, `rbackup` |
| `delonix-artifact` | `image`, `build`, `scan`, `app`, `vmfile`, `vmimage` (recipes) |
| `delonix-cluster` | `cluster`, `kindmode`, `kubeadm_config`, `k8s_recipes`, `etcd`, `pki`, `lb`, `kube` |
| `delonix-stack` | `manifest`, `kinds`, `reconcile`, `stack`, `compose`, `revision`, `schema`, `diff`, `resource`, `verbs`, `conditions` |
| `delonix-security` | `secret`, `policy` (node runtime policy ceiling) |
| `delonix-node` | `boot`, `prune`, `system` |
| `delonix-linux` (launcher side) | `mapped` (handlers of the `__rmtree`/`__volsnap` re-execs) |
| `delonix-cli` | `output`, `po`, `man`, `manual`, `manual_entries`, `complete`, `dash`, `exitcode`, `init`, `scaffold`, `compatibility`, `features`, `config`, `net`, `manifestcmd`, `mod` |
| `delonix-docker-api` / `delonix-mcp` / `delonix-l7proxy` | `dockerapi` / `mcp` / `ingress_proxy` |
| `delonix-ssh` | `remote` |
| composition root (`bins/delonix`) | `vmbackends`, `util` (state root and store opening) |
| removed | `serve` (replaced by the binaries) |

The split is by the business rule a module owns, not by its file name: where a module
mixes a use case with printing (most of `container`, `vm`, `system`), the use case moves to
the context and the printing stays in `delonix-cli`.

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
                                     delonix-node-api  (this engine)
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
- **No daemon (guardrail #1).** `delonix-node-api` is started by systemd socket
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
  `ProcessLauncher` port, implemented by the `delonix-launcher` binary (D2.4) receiving a
  typed spec over an fd — the pattern `__apirun` already uses for the Docker API. The CRI
  maps `runtime.v1` onto the `delonix-compute` use cases in-process; only the final spawn crosses a
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
| **P0 rails** | D7 fitness tests and ratchets at today's numbers; `[workspace.lints]` (`undocumented_unsafe_blocks = deny`); workspace-level dependency versions; the `crates/{foundation,contexts,adapters,providers,interfaces}` + `bins/` directories with the current crates moved into their layer **without renaming** | CI red on any new layer violation, new subprocess call or new library `println!`; `cargo build --workspace` and the e2e battery unchanged after the move |
| **P1 contract** | Vendor `google/api/http.proto`, add REST annotations; generated OpenAPI in `docs/api/`; `buf` in CI. **The `delonix-node-proto` crate moves to P5**, where the server is its first consumer: a crate of generated code nobody calls is the dead scaffolding the engine refuses — the risk that the contract does not compile under `prost`/`tonic` is recorded and closed there | contract compiles, lint clean, breaking-check wired; OpenAPI regenerated = committed |
| **P1b launcher spike** | GO/NO-GO (guardrail #5): a `delonix-launcher` executable spawning a rootless container and an overlay hold, on the golden `delonix-vm-base:ubuntu-24.04` with `kernel.apparmor_restrict_unprivileged_userns=1`, with the AppArmor profile naming the launcher and the holder but not the CLI; plus an in-place upgrade where a holder started as `delonix netns pin` is recognised by the new `delonix-netns-holder` | both measured on the golden; a written NO-GO keeps the re-execs inside each binary and amends D2.4 |
| **P2 extract** | `delonix-model` and `delonix-stack` first (already nearly pure), then the one run specification in `delonix-compute` replacing the four translators, then `networking`, `storage`, `artifact`, `gateway`, `cluster`, `security`, `node` — one context per PR, no behaviour change; `delonix-cli` becomes a library and `bins/delonix` its `main.rs` | CLI crate lines ratchet down each PR; e2e battery unchanged; each migrated use case has zero subprocess paths in CRI/mgmt/mcp |
| **P3 adapters** | rename and split: `delonix-linux`, `delonix-sdn`, `delonix-oci`, `delonix-scanner`, `delonix-state`, `delonix-telemetry`, `delonix-l7proxy`, `delonix-ssh`, `delonix-guestfs`; errors per crate mapped to `DX_*`; the `vm → net::infra` calls moved behind `NetworkProvider`; the servers become their own binaries and `delonix serve`/`delonix mcp` are removed | fitness test green; `delonix-runtime-core` gone; no binary links another interface's server |
| **P4 providers** | D3: neutral `VmSpec` + `Extensions`; `delonix-provider-*` crates; `NetworkProvider`, `StorageProvider`, `ImageRegistry` ports; `delonix-launcher` lands if P1b said GO | substitution test green (CH ↔ libvirt); zero provider-name matching outside composition roots |
| **P5 node API** | `delonix-node-api` serving gRPC + REST on the socket, socket activation, persisted operations, `WatchEvents`, health, capacity | contract conformance suite against the generated client; `delonix-mgmt` routes all mapped, then removed |
| **P6 CRI** | in-process CRI over `delonix-compute`; stats/eviction, events, `UpdateContainerResources`, record locking, digests, RuntimeConfig | critest ≥ current in rootless **and** a root run published; real-kubelet e2e on a node |
| **P7 observability** | D6 in full: semantic conventions, propagation, metric renames, JSON logs | one trace spans kubelet → CRI → launcher in a recorded run; zero library `println!` |

**P1b result (2026-09-16) — GO, with one condition.** Measured on the golden
`ubuntu-24.04` with `apparmor_restrict_unprivileged_userns=1`
(`docs/discovery/53_P1B_LAUNCHER_SPIKE.md`, raw output alongside): the same binary under a
launcher path with an `unconfined + userns` profile runs `run`, `run -d`, `exec`, and the
`__ovlhold`/`__rmtree` re-execs, while the unprofiled CLI path is refused — D2.4's split
works. **The netns holder does not, under that split:** it starts the pin through
`/usr/bin/unshare`, and a profile flagged `unconfined` is inherited across `exec` — so
`unshare(1)` passes when its caller carries the profile (today's single binary) and is moved
into `unprivileged_userns`, with its `mount` denied, when the caller does not (the CLI in
D2.4). The spike first attributed its own pin failure to this; that run actually failed on
a runtime dir under `/run`, which the control plane hides behind its tmpfs — corrected and
re-measured in the same document. D2.4 is amended: **`delonix-netns-holder` creates its
user, net and mount namespaces in-process** (`unshare(2)` + `newuidmap`, as
`reexec_mapped` already does), never through `unshare(1)`
— granting `userns` to `/usr/bin/unshare` would open it to every user and defeat the
restriction. In-place upgrade stays safe: the pin is recognised by its argv pair and
environment, never by the binary name (`argv_matches`, with the pre-split precedent).

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
- **One `delonix-app` crate for the whole application layer** (this ADR's first draft).
  Rejected on review: a single application crate becomes the next junk drawer, and it
  hides the bounded contexts the API groups already name. Each context crate owns its own
  use cases and ports.
- **Keep the servers and helpers inside `delonix`.** Rejected: seven long-running programs
  and nine re-execs in one executable are what let the CLI link the servers while the
  servers exec the CLI. One role per executable removes the cycle by construction.
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

**Harder / cost, in order of risk:**

1. **AppArmor on Ubuntu 23.10+.** The profile `install.sh` writes authorises user
   namespaces for the path `…/delonix` only (`scripts/install.sh:591`). Any new executable
   that creates a user namespace fails with `EPERM`, which reads as an engine bug (it cost an
   hour on 2026-08-12). With every such spawn in `delonix-launcher`, the profile names the
   launcher and the netns holder — measured in P1b before anything merges.
2. **In-place upgrade of the network holder.** Recovery recognises the live holder by its
   argv (`netns pin` / `netns holder`, ACH-016). The new `delonix-netns-holder` must accept
   the old form for one release cycle, or an upgrade rebuilds the network of every running
   container.
3. **Distribution.** The release goes from two executables to eight; `install.sh`, the
   systemd units and `delonix-deploy` change with it.

Also: a long migration with coordination windows; crate renames touch
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
- Whether the VM image factory (`delonix-guestfs` + the recipes in `delonix-artifact`)
  eventually leaves this repository — it is a build tool, not a node operation.
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
