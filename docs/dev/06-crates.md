# 6. The crates

This page is the map you keep open while reading the code. The table below is
generated from `Cargo.toml` and `scripts/arch_fitness.py`; everything after it is
written by hand, and every pointer (`path:symbol`) was read in the tree before it
was written down. Where a claim could not be confirmed it is not here.

How to use it:

- Find the crate that owns what you want to change (layer first, see
  [05 — Architecture](05-architecture.md) for why the layers exist and which
  direction a dependency may point).
- Read its **Start reading at** list in order, then its **Gotchas**: each one is a
  trap this code base has already paid for, and the comment that records it is
  still in the file.
- Before you add a `use delonix_…` line, check the table: an edge that is not in
  the "Depends on" column will fail `scripts/arch_fitness.py` unless it goes in the
  allowed direction.

Two conventions you will meet everywhere:

- **Pure versus effect.** Contexts and foundation crates decide; adapters touch the
  kernel, the disk, a subprocess or the network. When a use case in a context needs
  an effect, it declares a *port* (a trait) and an adapter implements it. The
  composition root that wires ports to adapters is the `delonix` binary.
- **"Talks to" means the mechanism, not just the dependency.** A crate may depend on
  another and still reach it by running the `delonix` binary as a subprocess (the
  CRI and the local management API do this for anything that forks), or by writing
  a line on a unix control socket (the network holder).

## Reference table

<!-- dev-docs:begin crates-table -->
| Crate | Layer | Path | Binaries | Depends on (engine crates) | Used by |
|---|---|---|---|---|---|
| `delonix-model` | Foundation | `crates/foundation/delonix-model` | — | `delonix-runtime-core` | `delonix-runtime-bin` |
| `delonix-net-rules` | Foundation | `crates/foundation/delonix-net-rules` | — | — | `delonix-net`, `delonix-vm` |
| `delonix-runtime-core` | Foundation | `crates/foundation/delonix-runtime-core` | — | — | `delonix-compute`, `delonix-cri`, `delonix-image`, `delonix-mcp`, `delonix-mcp-bin`, `delonix-mgmt`, `delonix-mgmt-bin`, `delonix-model`, `delonix-net`, `delonix-proxmox`, `delonix-runtime`, `delonix-runtime-bin`, `delonix-scan`, `delonix-security-runtime`, `delonix-stack`, `delonix-truenas`, `delonix-vm`, `delonix-volume` |
| `delonix-compute` | Contexts | `crates/contexts/delonix-compute` | — | `delonix-runtime-core` | `delonix-cri`, `delonix-image`, `delonix-net`, `delonix-runtime`, `delonix-runtime-bin`, `delonix-vm`, `delonix-volume` |
| `delonix-security-runtime` | Contexts | `crates/contexts/delonix-security-runtime` | — | `delonix-runtime-core` | `delonix-runtime-bin` |
| `delonix-stack` | Contexts | `crates/contexts/delonix-stack` | — | `delonix-runtime-core` | `delonix-runtime-bin` |
| `delonix-image` | Adapters | `crates/adapters/delonix-image` | — | `delonix-compute`, `delonix-runtime-core` | `delonix-cri`, `delonix-mgmt`, `delonix-runtime-bin`, `delonix-scan` |
| `delonix-net` | Adapters | `crates/adapters/delonix-net` | — | `delonix-compute`, `delonix-net-rules`, `delonix-runtime-core` | `delonix-cri`, `delonix-mcp`, `delonix-mgmt`, `delonix-runtime-bin` |
| `delonix-runtime` | Adapters | `crates/adapters/delonix-runtime` | — | `delonix-compute`, `delonix-runtime-core` | `delonix-cri`, `delonix-mcp`, `delonix-mgmt`, `delonix-runtime-bin` |
| `delonix-scan` | Adapters | `crates/adapters/delonix-scan` | — | `delonix-image`, `delonix-runtime-core` | `delonix-mgmt`, `delonix-runtime-bin` |
| `delonix-telemetry` | Adapters | `crates/adapters/delonix-telemetry` | — | — | `delonix-cri`, `delonix-mcp-bin`, `delonix-mgmt`, `delonix-mgmt-bin`, `delonix-runtime-bin` |
| `delonix-vm` | Adapters | `crates/adapters/delonix-vm` | — | `delonix-compute`, `delonix-net-rules`, `delonix-runtime-core` | `delonix-mcp`, `delonix-mgmt`, `delonix-proxmox`, `delonix-runtime-bin` |
| `delonix-volume` | Adapters | `crates/adapters/delonix-volume` | — | `delonix-compute`, `delonix-runtime-core` | `delonix-mcp`, `delonix-mgmt`, `delonix-runtime-bin` |
| `delonix-proxmox` | Providers | `crates/providers/delonix-proxmox` | — | `delonix-runtime-core`, `delonix-vm` | `delonix-runtime-bin` |
| `delonix-truenas` | Providers | `crates/providers/delonix-truenas` | — | `delonix-runtime-core` | `delonix-runtime-bin` |
| `delonix-cri` | Interfaces | `crates/interfaces/delonix-cri` | `delonix-cri` | `delonix-compute`, `delonix-image`, `delonix-net`, `delonix-runtime`, `delonix-runtime-core`, `delonix-telemetry` | — |
| `delonix-mcp` | Interfaces | `crates/interfaces/delonix-mcp` | — | `delonix-mgmt`, `delonix-net`, `delonix-runtime`, `delonix-runtime-core`, `delonix-vm`, `delonix-volume` | `delonix-mcp-bin` |
| `delonix-mgmt` | Interfaces | `crates/interfaces/delonix-mgmt` | — | `delonix-image`, `delonix-net`, `delonix-runtime`, `delonix-runtime-core`, `delonix-scan`, `delonix-telemetry`, `delonix-vm`, `delonix-volume` | `delonix-mcp`, `delonix-mgmt-bin`, `delonix-runtime-bin` |
| `delonix-mcp-bin` | Binaries | `bins/delonix-mcp-bin` | `delonix-mcp` | `delonix-mcp`, `delonix-runtime-core`, `delonix-telemetry` | — |
| `delonix-mgmt-bin` | Binaries | `bins/delonix-mgmt-bin` | `delonix-mgmt` | `delonix-mgmt`, `delonix-runtime-core`, `delonix-telemetry` | — |
| `delonix-runtime-bin` | Binaries | `bins/delonix-runtime-bin` | `delonix` | `delonix-compute`, `delonix-image`, `delonix-mgmt`, `delonix-model`, `delonix-net`, `delonix-proxmox`, `delonix-runtime`, `delonix-runtime-core`, `delonix-scan`, `delonix-security-runtime`, `delonix-stack`, `delonix-telemetry`, `delonix-truenas`, `delonix-vm`, `delonix-volume` | — |
<!-- dev-docs:end crates-table -->

## Foundation

Foundation crates carry no mechanism: types, pure rules and the on-disk state
format. They may depend only on other foundation crates.

### `delonix-runtime-core`

**Purpose.** The shared vocabulary of the engine: the persisted records
(`Container`, `Vm`), their `Status`, the error type every crate returns, and the
JSON-file stores with atomic writes. It also holds the small cross-cutting pieces
that more than one crate needs and that would otherwise be copied: the
`SO_PEERCRED` check for local sockets, the append-only event log, the encrypted
secret store, and the rule a server binary follows when `delonix` runs it. It does
**not** create processes, mount, or configure the network, and it has no notion of
tenant, plan or billing (its crate doc says so, and the rest of the workspace
relies on it).

**Key modules**

| Module | Responsibility |
|---|---|
| `lib.rs` | `Container`, `Vm`, `Status`, `Mount`, `ContainerFw`/`FwRule`, health config, cgroup-parent parsing, `generate_id`, pid liveness helpers |
| `store` | `Store` (one JSON file per container) and `JsonStore<T>`; atomic write helpers |
| `error` | `Error` and `Result` |
| `events` | append-only `events.jsonl` event log (`emit`, `read`) |
| `secret`, `cred_vault` | named secrets encrypted at rest (XChaCha20-Poly1305) |
| `dispatch` | version check and CLI resolution for server binaries run by `delonix` |
| `peer_cred` | `peer_uid` from `SO_PEERCRED` |
| `typestate` | compile-time lifecycle phases (`Phase<Created/Running/Stopped>`) |
| `virt` | virtualization/virtio detection from `/sys` and `/proc` |
| `workload_net` | the workload IPv4 range, defined once |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `Container` | the container record everything reads and writes | `crates/foundation/delonix-runtime-core/src/lib.rs:Container` |
| `Vm` | the VM record | `crates/foundation/delonix-runtime-core/src/lib.rs:Vm` |
| `Status` | lifecycle state of a workload | `crates/foundation/delonix-runtime-core/src/lib.rs:Status` |
| `Store` | container store; `load`/`save`/`list`, and `update` for read-modify-write | `crates/foundation/delonix-runtime-core/src/store.rs:Store` |
| `JsonStore<T>` | the same pattern for other records (VMs, …), with `update` | `crates/foundation/delonix-runtime-core/src/store.rs:JsonStore` |
| `write_atomic`, `write_atomic_mode`, `write_private_temp` | temp-file + rename writes; a private temp file for handing content to a tool | `crates/foundation/delonix-runtime-core/src/store.rs` |
| `Error`, `Result` | the error every engine crate returns | `crates/foundation/delonix-runtime-core/src/error.rs:Error` |
| `events::emit` | append one event line | `crates/foundation/delonix-runtime-core/src/events.rs:emit` |
| `SecretStore`, `CredVault` | encrypted secrets and credentials | `crates/foundation/delonix-runtime-core/src/secret.rs:SecretStore` |
| `dispatch::check_version`, `dispatch::cli_bin` | how `delonix-cri`/`-mgmt`/`-mcp` refuse a mismatched release and find the `delonix` CLI to run back | `crates/foundation/delonix-runtime-core/src/dispatch.rs` |
| `is_alive`, `proc_starttime`, `safe_to_signal` | pid checks that survive pid recycling | `crates/foundation/delonix-runtime-core/src/lib.rs` |

**Talks to.** No other engine crate (it is the root of the graph). No
subprocesses: detection reads `/sys` and `/proc` directly.

**Notable external dependencies.** `serde`/`serde_json` (records on disk),
`thiserror`, `libc`, `chacha20poly1305` + `getrandom` (the Cargo.toml comment:
at-rest encryption for the secret manager, pure Rust so it builds on musl/aarch64),
`tracing`.

**Tests.** Inline `#[cfg(test)]` modules in the source files; no `tests/` directory.

**Start reading at.** `src/lib.rs` (the `Container` and `Vm` structs), then
`src/store.rs`, then `src/dispatch.rs`.

**Gotchas.**

- `Container.userns` says whether the container **created** its own user
  namespace, not whether it runs in a different one. Workloads that join the
  network holder's user namespace have `userns = false` and are still in a
  different user namespace than the caller. `mount_live` in `delonix-runtime`
  records this and always opens the `user` namespace instead of trusting the field
  (`crates/adapters/delonix-runtime/src/lib.rs:mount_live`).
- `Container.ip` is the address on the **primary** network only; a multi-homed
  container has more (see the `NetPlan` doc comment in
  `crates/adapters/delonix-net/src/infra.rs` and `apply_firewall_all`, which exists
  because firewalling only the primary IP was bypassable).
- `Container::cgroup()` is the static root-mode path. For a running rootless
  container the real cgroup is read from `/proc/<pid>/cgroup` by
  `delonix_runtime::live_cgroup`.

### `delonix-model`

**Purpose.** The part of the model any layer can name without depending on a
mechanism: the generated workload names and the mapping from an engine `Error` to a
process exit code. Pure — no I/O, no process state (crate doc). It does not hold
records; those are in `delonix-runtime-core`.

**Key modules**

| Module | Responsibility |
|---|---|
| `exitcode` | exit-code classes (`NOT_RUNNING`, `NOT_FOUND`, `CONFLICT`, …) and `for_error` |
| `names` | default names (`derived_name`, `random_name`) |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `exitcode::for_error` | the one place an `Error` becomes an exit code | `crates/foundation/delonix-model/src/exitcode.rs:for_error` |
| `exitcode::merge` | the code for a batch of results | `crates/foundation/delonix-model/src/exitcode.rs:merge` |
| `names::derived_name` | deterministic name from an id | `crates/foundation/delonix-model/src/names.rs:derived_name` |

**Talks to.** `delonix-runtime-core` (for `Error`), by direct Rust call. The CLI
re-exports both modules as `cmd::exitcode` and `cmd::names`
(`bins/delonix-runtime-bin/src/cmd/mod.rs`), so older call sites did not change.

**Notable external dependencies.** None beyond the workspace (`serde_json` is
dev-only).

**Tests.** Inline unit tests.

**Start reading at.** `src/exitcode.rs` (its module doc explains why classes exist),
then `src/names.rs`.

**Gotchas.** The `match` in `for_error` is exhaustive on purpose: a new `Error`
variant must be classified here or the build fails.

### `delonix-net-rules`

**Purpose.** Network rules that can be computed without touching the kernel:
bridge names, IP derivation inside a prefix, the `Cidr` value type, label
matching, parsing of `iptables-save` output. It has **zero dependencies**, so any
caller can compile the same rules the engine uses. It deliberately excludes
anything that reads shared state (IP allocation reads the IPAM registry, so it
stays in `delonix-net`).

**Key modules.** A single `lib.rs`.

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `Cidr` | IPv4 prefix type, no external crate | `crates/foundation/delonix-net-rules/src/lib.rs:Cidr` |
| `bridge_name` | the one formula for a network's bridge device name | `crates/foundation/delonix-net-rules/src/lib.rs:bridge_name` |
| `derive_ip_in`, `valid_ip_in_subnet` | preferred address for an id, and membership check | `crates/foundation/delonix-net-rules/src/lib.rs` |
| `matches_labels` | label selector matching (used by `kind: Service`) | `crates/foundation/delonix-net-rules/src/lib.rs:matches_labels` |
| `parse_overlay_peer` | parse an overlay peer spec | `crates/foundation/delonix-net-rules/src/lib.rs:parse_overlay_peer` |

**Talks to.** Nothing. `delonix-net` re-exports its items, so callers of
`delonix_net::Cidr` etc. keep compiling.

**Notable external dependencies.** None.

**Tests.** Inline unit tests.

**Start reading at.** `src/lib.rs` — the module doc lists what was left out and why.

**Gotchas.** Parts of the module doc are still in Portuguese (LANG-01 debt); the
code is the reference.

## Contexts

A context owns a domain's decisions and the ports its use cases need. No context
touches the kernel.

### `delonix-compute`

**Purpose.** The Compute context (`compute.delonix.io`): the run specification
every entry point translates into (`RunOpts`), and the `container run` use case as
pure steps over ports — preflight, resolve, build the record, wire the network,
start. It also holds the Pod specification types and their translation to
`RunOpts`. It does **not** spawn processes, pull images or configure networks; it
calls traits that adapters implement.

**Key modules**

| Module | Responsibility |
|---|---|
| `run_opts` | `RunOpts`, the one run specification |
| `preflight` | refuse flag combinations that mean nothing, before any effect |
| `run` | `resolve_run` (through ports) and `build_record` (pure) |
| `network` | the network phase: `attach_custom_network`, `wire_network` |
| `launch` | `Launch` intent, `WorkloadRuntime` port, `start` use case, restart policy |
| `ports` | `ImageStore`, `StorageProvider`, `DeviceResolver`, `RunHost`, `NetworkProvider`, `VmNetwork` |
| `pod` | Pod spec types and `pod_to_run_opts`/`container_to_run_opts` |
| `notice` | `Notice`, a warning returned as data instead of printed |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `RunOpts` | the run specification | `crates/contexts/delonix-compute/src/run_opts.rs:RunOpts` |
| `preflight::check_run_opts` | pure refusal of impossible combinations | `crates/contexts/delonix-compute/src/preflight.rs:check_run_opts` |
| `run::resolve_run` | resolve image, volumes, devices, user, defaults through ports | `crates/contexts/delonix-compute/src/run.rs:resolve_run` |
| `run::build_record` | turn spec + resolution into a `Container` (pure) | `crates/contexts/delonix-compute/src/run.rs:build_record` |
| `network::wire_network` | publish ports, record network/IP, namespace isolation, shaping — before start | `crates/contexts/delonix-compute/src/network.rs:wire_network` |
| `launch::start` | supervised or direct start, and cleanup of a start that never happened | `crates/contexts/delonix-compute/src/launch.rs:start` |
| `launch::WorkloadRuntime` | port that turns a `Launch` into a process | `crates/contexts/delonix-compute/src/launch.rs:WorkloadRuntime` |
| `ports::NetworkProvider` | port for attach/publish/firewall/shaping | `crates/contexts/delonix-compute/src/ports.rs:NetworkProvider` |
| `ports::VmNetwork` | port for a VM tap on the rootless network | `crates/contexts/delonix-compute/src/ports.rs:VmNetwork` |

**Talks to.** Only `delonix-runtime-core`, by direct call. Everything else arrives
through its ports, implemented in adapters:

| Port | Implemented by |
|---|---|
| `ImageStore` | `crates/adapters/delonix-image/src/run_images.rs:HostImages` |
| `StorageProvider` | `crates/adapters/delonix-volume/src/lib.rs:HostVolumes` |
| `DeviceResolver` | `crates/adapters/delonix-runtime/src/cdi.rs:HostDevices` |
| `RunHost` | `crates/adapters/delonix-runtime/src/run_host.rs:HostRuntime` |
| `WorkloadRuntime` | `crates/adapters/delonix-runtime/src/workload.rs:HostWorkload` |
| `NetworkProvider` | `crates/adapters/delonix-net/src/run_network.rs:HostNetwork` |
| `VmNetwork` | `crates/adapters/delonix-net/src/vm_network.rs:HostVmNetwork` |

**Notable external dependencies.** `serde`, `schemars` (Cargo.toml comment: the
spec types derive their JSON Schema next to their definition, so the published
schema cannot drift from the types).

**Tests.** Inline unit tests with fake port implementations (`FakeNet`,
`FakeRuntime`, `Fake` in `network.rs`, `launch.rs`, `run.rs`) — the use case is
tested without a kernel.

**Start reading at.** `src/ports.rs`, then `src/run.rs`, then `src/launch.rs`.

**Gotchas.**

- Two items named `ImageStore` exist: the port trait
  `delonix_compute::ports::ImageStore` and the concrete store
  `delonix_image::ImageStore` (a struct). `HostImages` adapts the second to the
  first. Import paths matter.
- `wire_network` must run **before** `launch::start`; its module doc records that a
  supervised `-d` otherwise missed the network settings.

### `delonix-stack`

**Purpose.** The Stack context (`core.delonix.io`): the table of Kinds and their
facts, the three-way reconciler that plans a manifest against what exists, and the
revision history of an apply. Planning is pure — nothing here opens a store of a
concrete resource or runs a command (crate doc). Loading manifests and applying
each Kind stay in the CLI.

**Key modules**

| Module | Responsibility |
|---|---|
| `kinds` | Kind name constants and `KindFacts` (domain, form, converges, teardown, namespaced, presence) |
| `reconcile` | `Desired`/`Actual`/`Change`, `plan`, the ownership label and last-applied annotation |
| `revision` | record and list apply revisions (for rollback) |
| `condition` | the `Condition` type |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `kinds::facts`, `kinds::stack_kinds`, `kinds::converges` | the single table the CLI consults per Kind | `crates/contexts/delonix-stack/src/kinds.rs` |
| `reconcile::plan` | desired vs actual → `Vec<Change>` | `crates/contexts/delonix-stack/src/reconcile.rs:plan` |
| `reconcile::STACK_LABEL`, `LAST_APPLIED` | ownership label and three-way diff annotation | `crates/contexts/delonix-stack/src/reconcile.rs` |
| `reconcile::hot_fields_for` | which field changes can be applied live | `crates/contexts/delonix-stack/src/reconcile.rs:hot_fields_for` |
| `revision::record`, `revision::list` | apply history | `crates/contexts/delonix-stack/src/revision.rs` |

**Talks to.** `delonix-runtime-core` only. The CLI re-exports `kinds`, `reconcile`
and `revision` as `cmd::kinds` etc. (`bins/delonix-runtime-bin/src/cmd/mod.rs`).

**Notable external dependencies.** `serde`, `serde_json`.

**Tests.** Inline unit tests (plans as data).

**Start reading at.** `src/kinds.rs`, then `src/reconcile.rs`, then
`bins/delonix-runtime-bin/src/cmd/stack.rs` to see it consumed.

**Gotchas.** Adding a Kind is not only a row in `kinds.rs`: the CLI has per-Kind
code (`desired_of`/`actual_of`, `converge_and_stamp`, `destroy_one` in
`cmd/stack.rs`) and schema/completion tables with their own tests. Run the full
test suite of `delonix-runtime-bin` after touching the table.

### `delonix-security-runtime`

**Purpose.** The node's security **decisions**: the policy file, the single
admission evaluation for containers and VMs, the security event, an explainable
posture score, and redaction of secrets in text. Pure functions of their arguments.
It deliberately has no sensors, watchers or resident process (crate doc: daemonless
by design), and no tenant, project or environment field anywhere.

**Key modules**

| Module | Responsibility |
|---|---|
| `policy` | `SecurityPolicy`, `Mode`, lints |
| `admission` | `Request`, `evaluate`, `Decision`, `Violation` |
| `event` | `SecurityEvent` on the engine's event log |
| `score` | `Score` with deductions and reasons |
| `redact` | redaction of sensitive keys/values in hostile input |
| `severity` | `Severity`, `ActionRisk`, `Confidence` |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `SecurityPolicy::parse` | load a policy | `crates/contexts/delonix-security-runtime/src/policy.rs:SecurityPolicy` |
| `admission::evaluate` | decide one request | `crates/contexts/delonix-security-runtime/src/admission.rs:evaluate` |
| `admission::Request` | container or VM admission input | `crates/contexts/delonix-security-runtime/src/admission.rs:Request` |
| `redact::redact_text` | mask secrets in text | `crates/contexts/delonix-security-runtime/src/redact.rs:redact_text` |

**Talks to.** `delonix-runtime-core` (`events`, `now_unix`). Consumed by the CLI
through `bins/delonix-runtime-bin/src/cmd/policy.rs`, which `cmd_run` calls before
any image is resolved.

**Notable external dependencies.** `serde`, `serde_json`.

**Tests.** Inline unit tests, including a `boundary_tests` module in `lib.rs` and a
doc-test in the crate doc.

**Start reading at.** `src/lib.rs` (crate doc), `src/admission.rs`, `src/policy.rs`.

**Gotchas.** None beyond the crate doc: do not add a background sensor here — the
doc explains why an inert control in rootless mode is worse than none.

## Adapters

Adapters are where the engine meets the kernel, the disk, host tools and remote
registries. They depend on foundation and contexts, never on each other (the
declared exceptions are listed in [05](05-architecture.md)).

### `delonix-runtime`

**Purpose.** The low-level container runtime: `clone` with namespaces,
`pivot_root`, cgroups v2, capabilities and seccomp, `exec` via `setns`, stop and
remove, and the detached supervisor behind `run -d`. Its crate doc states the rule:
the syscall boundary for containers lives here. It does not resolve images, parse
CLI flags or configure the network; network effects arrive as hooks from the
caller.

**Key modules**

| Module | Responsibility |
|---|---|
| `lib.rs` | `RunSpec`, `create_with`/`spawn`, `container_init`, rootfs setup and overlay mount, `exec`, `stop`, `remove`, live mounts, cgroups, `reconcile_status` |
| `workload` | `HostWorkload`, the `WorkloadRuntime` port implementation |
| `launch_spec` | `run_spec`, the one builder of `RunSpec` from a `Launch` |
| `supervise` | `run_supervised`, the forked parent of a detached container |
| `capabilities` | capability name↔number table and default set |
| `seccomp_profile` | OCI seccomp profile loading |
| `cdi` | CDI device spec consumer (`HostDevices`) |
| `run_host` | `HostRuntime`, the `RunHost` port implementation |
| `regulate`, `resource_advice`, `workload_view` | resource pressure, host advice, requested-vs-enforced view |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `RunSpec` | everything a spawn needs | `crates/adapters/delonix-runtime/src/lib.rs:RunSpec` |
| `create_with` | start a container (calls `spawn`) | `crates/adapters/delonix-runtime/src/lib.rs:create_with` |
| `exec` | run a command inside a running container | `crates/adapters/delonix-runtime/src/lib.rs:exec` |
| `stop`, `remove` | lifecycle | `crates/adapters/delonix-runtime/src/lib.rs` |
| `reconcile_status` | refresh a record against the live process | `crates/adapters/delonix-runtime/src/lib.rs:reconcile_status` |
| `mount_live`, `update_limits`, `set_frozen` | hot changes to a running container | `crates/adapters/delonix-runtime/src/lib.rs` |
| `mount_overlay_if_marked` | overlay mount with the new mount API | `crates/adapters/delonix-runtime/src/lib.rs:mount_overlay_if_marked` |
| `supervise::run_supervised` | detached supervisor | `crates/adapters/delonix-runtime/src/supervise.rs:run_supervised` |
| `workload::HostWorkload` | `WorkloadRuntime` adapter | `crates/adapters/delonix-runtime/src/workload.rs:HostWorkload` |

**Talks to.** `delonix-runtime-core` and `delonix-compute`, by direct call.
Syscalls through `nix`, `libc` and `rustix`. Host tools it runs: `busctl` (systemd
scopes for the kubelet cgroup parent), `apparmor_parser`, `ldconfig`,
`nvidia-smi`. The slirp for `-p` is not started here: `HostWorkload` takes an
`attach_slirp` hook that the CLI fills with `delonix_net::slirp_attach`
(`bins/delonix-runtime-bin/src/cmd/container.rs:with_host_workload`).

**Notable external dependencies.** `nix`, `libc`, `seccompiler`; `rustix` with
`mount`/`fs` (Cargo.toml comment: `nix` has no wrapper for
`fsopen`/`fsconfig`/`fsmount`/`move_mount`, needed to avoid the page-size limit of
the classic `mount(2)` data argument); `serde_yaml` for CDI specs.

**Tests.** Inline unit test modules in `lib.rs` and the module files;
integration tests in `crates/adapters/delonix-runtime/tests/` (`cgroup_parent.rs`,
`advisor_fixtures.rs`).

**Start reading at.** `src/workload.rs`, then `src/launch_spec.rs`, then
`src/lib.rs` from `RunSpec` through `spawn` and `container_init`.

**Gotchas.**

- `spawn` does not return, and the record is not saved with a `pid`, until the init
  has finished its mounts; the comment before `store.save` in `spawn` explains the
  host-root race this closes. Do not move that save earlier.
- `supervise::run_supervised` and the rootless handshake assume a single-threaded
  caller (`fork`). That is why multi-threaded servers (the CRI, the management API,
  the Docker API shim) run the `delonix` binary instead of calling this crate to
  start containers.
- Use `live_cgroup(container)`, not `container.cgroup()`, for a running rootless
  container.

### `delonix-image`

**Purpose.** OCI images: a content-addressed blob store, the image store and its
metadata, registry pull/push with auth, per-container rootfs preparation (shared
overlay layers), Dockerfile/Delonixfile parsing and build helpers, Cloud Native
Buildpacks planning, archive load/save, and signature sign/verify. It does not run
containers; a build runs its steps through the CLI.

**Key modules**

| Module | Responsibility |
|---|---|
| `cas` | `Cas`, sha256-addressed blobs |
| `image` | `Image`, `ImageConfig`, `ImageStore` |
| `registry` | reference parsing, `resolve_or_pull`, pull/push, OCI artifacts |
| `overlay` | `prepare_container_rootfs`, `prepare_overlay`, `existing_rootfs_path` |
| `build` | Dockerfile parser (`parse_dockerfile`), stages, `commit_flat_rootfs` |
| `run_images` | `HostImages`, the compute `ImageStore` port |
| `auth` | registry credentials (`login`/`lookup`) |
| `load`, `save` | Docker archive in, OCI archive out |
| `sign` | `sign_image`, `verify_signature` (ECDSA P-256) |
| `buildpack`, `detect`, `internal_registry` | CNB plan, language detection, throwaway registry |
| `rootfs_user` | `--user` resolution against a rootfs |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `ImageStore` | open, resolve, list, remove images | `crates/adapters/delonix-image/src/image.rs:ImageStore` |
| `registry::resolve_or_pull` | local image or pull | `crates/adapters/delonix-image/src/registry.rs:resolve_or_pull` |
| `pull_from_registry_with_creds` | pull with credentials (used by the CRI) | `crates/adapters/delonix-image/src/registry.rs` |
| `ImageStore::prepare_container_rootfs` | rootfs for a container id | `crates/adapters/delonix-image/src/overlay.rs` |
| `build::parse_dockerfile` | Dockerfile/Delonixfile grammar | `crates/adapters/delonix-image/src/build.rs:parse_dockerfile` |
| `Cas` | blob store | `crates/adapters/delonix-image/src/cas.rs:Cas` |
| `verify_signature` | cosign-style verification | `crates/adapters/delonix-image/src/sign.rs:verify_signature` |

**Talks to.** `delonix-runtime-core`, and `delonix-compute` (it implements the
`ImageStore` port). Registries over HTTPS with a blocking `reqwest` client. No host
subprocesses in its source.

**Notable external dependencies.** `reqwest` (blocking, rustls), `oci-spec`
(canonical OCI image types), `sha2`, `tar`, `flate2`, `zstd`, `base64`, `ring`
(signature verification); dev-only `proptest` (parser robustness on stable Rust)
and `criterion`.

**Tests.** Inline unit tests; a benchmark in
`crates/adapters/delonix-image/benches/parse_reference.rs`.

**Start reading at.** `src/image.rs`, then `src/registry.rs` (`resolve_or_pull`),
then `src/overlay.rs`.

**Gotchas.**

- `delonix_image::ImageStore` (struct) is not `delonix_compute::ports::ImageStore`
  (trait); see `run_images.rs`.
- The rootfs a container starts from is an overlay over shared layers with a marker
  file; the mount itself happens inside the container's init
  (`delonix_runtime::mount_overlay_if_marked`), not here.

### `delonix-net`

**Purpose.** The rootless SDN and the firewall. A long-lived *pin* process holds a
user+network namespace; a restartable *control* process inside it serves a unix
control socket and owns the bridges, nftables rules, DHCP and internal DNS; one
`slirp4netns` bridges that namespace to the host. It also covers the
slirp-per-container path for `-p` without a custom network, IPAM, CNI plugin
execution, WireGuard overlay, and optional eBPF flow accounting. It re-exports
`delonix-net-rules`. It does not spawn containers.

**Key modules**

| Module | Responsibility |
|---|---|
| `lib.rs` | `NetworkStore`, publish spec parsing, `slirp_attach`, slirp orphan reaping |
| `infra` | the holder: `ensure_up`, `acquire`, `attach_container`, `publish_port`, `apply_firewall_all`, `network_route`, `vm_attach`, the control socket |
| `run_network` | `HostNetwork` (`NetworkProvider` port), `publish_with_retry` |
| `vm_network` | `HostVmNetwork` (`VmNetwork` port) |
| `ipam` | lease registry for addresses |
| `cni` | CNI conformance: run plugin binaries |
| `wg` | WireGuard over the overlay |
| `bpf` | optional eBPF flow accounting |
| `discover` | listening ports of a workload from `/proc/<pid>/net` |
| `pin_userns` | the pin's own namespaces and id maps |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `NetworkStore` | declarative network registry | `crates/adapters/delonix-net/src/lib.rs:NetworkStore` |
| `parse_publish`, `parse_publish_addr` | `-p` grammar | `crates/adapters/delonix-net/src/lib.rs` |
| `slirp_attach` | a container's own slirp, with host forwards | `crates/adapters/delonix-net/src/lib.rs:slirp_attach` |
| `infra::ensure_up` | bring the holder up (pin + control + slirp) | `crates/adapters/delonix-net/src/infra.rs:ensure_up` |
| `infra::attach_container` | veth on a network, IP lease | `crates/adapters/delonix-net/src/infra.rs:attach_container` |
| `infra::apply_firewall_all` | per-container chain for every IP it holds | `crates/adapters/delonix-net/src/infra.rs:apply_firewall_all` |
| `run_network::HostNetwork` | `NetworkProvider` adapter | `crates/adapters/delonix-net/src/run_network.rs:HostNetwork` |

**Talks to.** `delonix-runtime-core`, `delonix-net-rules`, `delonix-compute`
(ports). Host tools: `ip`, `nft`, `nsenter`, `slirp4netns`, `conntrack`, `wg`, CNI
plugin binaries. The holder is started by re-executing the engine binary
(`netns pin`, `netns control`, intercepted in the CLI's `main` before argument
parsing — `bins/delonix-runtime-bin/src/main.rs`). Everything that must happen
inside the namespace is a line written to the control socket
(`infra.rs:control_query`), served by `handle_control`. Port forwards go to
`slirp4netns` over its API socket (`slirp_add_hostfwd`). `build.rs` compiles the
eBPF object only if `clang` and the headers exist; eBPF is never required.

**Notable external dependencies.** `libc`, `serde`, `serde_json`, `tracing`;
dev-only `proptest` for IP allocation invariants.

**Tests.** Inline unit test modules; integration tests in
`crates/adapters/delonix-net/tests/`.

**Start reading at.** `src/infra.rs` module doc and `ensure_up`, then
`attach_container`, then `src/run_network.rs`.

**Gotchas.**

- The private `capture()` helper in `src/lib.rs` returns stdout **without checking
  the exit status**. Read its output; never treat its `Ok` as "the command
  succeeded". (The helper of the same name in `delonix-vm` is different: it returns
  `None` on failure.)
- The control socket path is derived from the uid **and**, when `DELONIX_ROOT` is not
  the default, from a hash of it (`runtime_dir` + `root_suffix`, ADR-0014);
  `DELONIX_NET_RUNTIME_DIR` overrides both. Anything re-executed across a user namespace must carry
  `runtime_dir_env()` as well as `DELONIX_ROOT`; see how the pin is spawned in
  `infra.rs`. When isolating a test run, set both `DELONIX_ROOT` and
  `DELONIX_NET_RUNTIME_DIR`.
- A firewall that only knows `Container.ip` misses additional networks; use
  `apply_firewall_all`.

### `delonix-vm`

**Purpose.** MicroVMs and VMs behind the `VmBackend` trait and a runtime
**registry** of backends. Cloud Hypervisor and libvirt are the local backends; a
remote backend registers itself from outside the crate. It owns VM records, boot
and lifecycle, snapshots, cloud-init seed generation, and backend selection
(explicit, default file, or auto-detection). It does not hold an HTTP client or
provider credentials.

**Key modules**

| Module | Responsibility |
|---|---|
| `lib.rs` | `VmConfig`, `VmBackend`, registry, `CloudHypervisorBackend`, `LibvirtBackend`, `create_with`, `start`/`stop`/`remove`, snapshots, `status`/`list` |
| `cloudinit` | `build_user_data`, `build_network_config`, `generate_seed_iso` |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `VmBackend` | the backend port (`boot`, `stop`, `destroy`, `resume`, `snapshot`, `ip`, `manages_own_storage`, `auto_selectable`, …) | `crates/adapters/delonix-vm/src/lib.rs:VmBackend` |
| `register_backend`, `BackendRegistration` | add a backend by factory | `crates/adapters/delonix-vm/src/lib.rs` |
| `set_network` | register the `VmNetwork` port once per process | `crates/adapters/delonix-vm/src/lib.rs:set_network` |
| `VmConfig` | what to create | `crates/adapters/delonix-vm/src/lib.rs:VmConfig` |
| `create_with`, `start`, `stop`, `remove`, `status`, `list` | lifecycle | `crates/adapters/delonix-vm/src/lib.rs` |
| `snapshot`, `restore`, `snapshots`, `delete_snapshot` | checkpoints | `crates/adapters/delonix-vm/src/lib.rs` |
| `valid_vm_name` | name validation at the engine boundary | `crates/adapters/delonix-vm/src/lib.rs:valid_vm_name` |

**Talks to.** `delonix-runtime-core`, `delonix-compute` (the `VmNetwork` port),
`delonix-net-rules`. Host tools: `cloud-hypervisor` (and its HTTP API on a unix
socket, e.g. `PUT /api/v1/vm.pause`), `virsh`, `qemu-img`, `cloud-localds`, `sh`.
The network is reached only through the registered `VmNetwork`; the CLI registers
`delonix_net::vm_network::HostVmNetwork` at startup
(`bins/delonix-runtime-bin/src/main.rs`).

**Notable external dependencies.** `libc`, `tracing` — deliberately few.

**Tests.** Inline unit test modules in `lib.rs`.

**Start reading at.** `VmBackend` and the registry in `src/lib.rs`, then
`create_with`, then one backend (`CloudHypervisorBackend`).

**Gotchas.**

- For Cloud Hypervisor the IP is **computed** from the MAC, not observed
  (`VmNetwork::lease_ip`, `ip_is_predicted`). A predicted IP is not proof the guest
  booted.
- Tool output is parsed with a pinned `C` locale (`stable_cmd`); use it for any new
  host tool call whose output you parse.
- `stop` and `destroy` are distinct trait methods: for a remote backend, destroying
  also removes the disk.

### `delonix-volume`

**Purpose.** Named volumes (`<root>/volumes/<name>/_data`) and bind mounts,
including the `-v` grammar, quotas and usage measurement, network-backed volumes
(NFS/CIFS/WebDAV mounted by host tools), shares under a parent volume, and
snapshots. It implements the compute `StorageProvider` port. It does not create a
NAS dataset (that is `delonix-truenas`).

**Key modules.** A single `lib.rs`.

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `VolumeStore` | create, list, remove, quota, mount | `crates/adapters/delonix-volume/src/lib.rs:VolumeStore` |
| `VolumeStore::resolve_spec` | `-v` spec → `Mount` | `crates/adapters/delonix-volume/src/lib.rs` |
| `Volume` | the volume record | `crates/adapters/delonix-volume/src/lib.rs:Volume` |
| `measure`, `Usage` | disk usage with an unreadable counter | `crates/adapters/delonix-volume/src/lib.rs` |
| `HostVolumes` | `StorageProvider` adapter | `crates/adapters/delonix-volume/src/lib.rs:HostVolumes` |

**Talks to.** `delonix-runtime-core`, `delonix-compute`. Host tools: `mount`,
`umount`, `losetup`. Removal of trees owned by mapped uids is injected by the
caller (`remove_with` takes an `rmtree` closure; the CLI passes
`delonix_runtime::remove_tree_mapped`).

**Notable external dependencies.** `serde`, `serde_json`.

**Tests.** Inline unit tests.

**Start reading at.** `VolumeStore` in `src/lib.rs`, then `resolve_spec`, then
`ensure_mounted`.

**Gotchas.** An unreadable directory is not an empty one: `Usage.unreadable > 0`
means `bytes` is a lower bound. In rootless mode a database volume made `0700` by a
mapped uid is the normal case.

### `delonix-scan`

**Purpose.** Image vulnerability scanning without root and without running the
image: extract an SBOM (Alpine `apk`, Debian/Ubuntu `dpkg`) by reading layers from
the CAS, and match it against an advisory database. A `pytree` module scans Python
module trees (manifest and dependency checks). It does not download the advisory
database itself (no HTTP client in its dependencies).

**Key modules**

| Module | Responsibility |
|---|---|
| `lib.rs` | `extract_sbom`, `AdvisoryDb`, `Finding`, `advisories_from_osv`, version comparison |
| `pytree` | Python module-tree scanning |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `extract_sbom` | packages of an image | `crates/adapters/delonix-scan/src/lib.rs:extract_sbom` |
| `AdvisoryDb` | advisories to match against | `crates/adapters/delonix-scan/src/lib.rs:AdvisoryDb` |
| `advisories_from_osv` | load OSV-format advisories | `crates/adapters/delonix-scan/src/lib.rs:advisories_from_osv` |

**Talks to.** `delonix-image` (`ImageStore`, `Image`) and `delonix-runtime-core`,
by direct call — a declared layering exception (see [05](05-architecture.md)).

**Notable external dependencies.** `tar`, `flate2`, `serde`, `serde_json`.

**Tests.** Inline unit tests.

**Start reading at.** `src/lib.rs` from `extract_sbom`.

**Gotchas.** None recorded in code beyond the crate doc.

### `delonix-telemetry`

**Purpose.** Observability for the engine's binaries: structured `tracing`
logging, optional OpenTelemetry span export over OTLP, and the shared Prometheus
registry the servers expose. It left `delonix-runtime-core` so that a crate needing
a `Container` type does not compile an OTLP client (crate doc).

**Key modules**

| Module | Responsibility |
|---|---|
| `telemetry` | `init` — `fmt` subscriber, plus OTLP when configured |
| `metrics` | Prometheus counters/gauges and `encode` |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `telemetry::init` | call once at the start of each binary | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` |
| `metrics::encode` | text exposition for `/metrics` | `crates/adapters/delonix-telemetry/src/metrics.rs:encode` |

**Talks to.** No engine crate. OTLP export to a collector when configured.

**Notable external dependencies.** `tracing-subscriber`, `opentelemetry`,
`opentelemetry_sdk`, `opentelemetry-otlp`, `tracing-opentelemetry`,
`prometheus-client`.

**Tests.** Inline unit tests.

**Start reading at.** `src/telemetry.rs`, then `src/metrics.rs`.

**Gotchas.** The OTLP exporter batches. The short-lived `delonix` CLI does not
flush on exit, so spans from a quick CLI invocation can be lost; long-running
servers deliver reliably (module doc of `telemetry.rs`).

## Providers

Providers are backends that speak to an external system's management API. They
live outside the adapters so that talking to a remote management API stays out of
the engine adapters (Cargo.toml comments of both crates). This is not "no HTTP in
adapters": `delonix-image` has its own OCI registry client, and `delonix-telemetry`
exports OTLP over HTTP.

### `delonix-proxmox`

**Purpose.** A `VmBackend` backed by the REST API of **one** Proxmox VE node,
named explicitly. No inventory and no node selection. It never touches a local
disk (`manages_own_storage` is `true`) and is never auto-detected
(`auto_selectable` is `false`, because answering "available?" would cost a network
round trip).

**Key modules.** A single `lib.rs`.

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `Target`, `Auth` | node endpoint, node name, credentials | `crates/providers/delonix-proxmox/src/lib.rs` |
| `Client` | API client (`connect`, `create_vm`, `start`, `stop`, `destroy`, `snapshot`, `wait_task`, …) | `crates/providers/delonix-proxmox/src/lib.rs:Client` |
| `ProxmoxBackend` | the `VmBackend` implementation | `crates/providers/delonix-proxmox/src/lib.rs:ProxmoxBackend` |
| `register` | register the backend in `delonix-vm`'s registry | `crates/providers/delonix-proxmox/src/lib.rs:register` |

**Talks to.** `delonix-vm` (the trait and `register_backend`; a declared layering
exception) and `delonix-runtime-core`. The node over HTTPS with blocking `reqwest`.
The CLI registers it from environment configuration at startup
(`bins/delonix-runtime-bin/src/cmd/vmbackends.rs:register_configured`).

**Notable external dependencies.** `reqwest` (blocking, rustls), `serde`,
`serde_json`.

**Tests.** Inline unit tests; `crates/providers/delonix-proxmox/tests/live.rs`
runs against a real node and skips with a printed line unless
`DELONIX_PROXMOX_TEST_URL` is set.

**Start reading at.** Crate doc in `src/lib.rs`, then `Client::wait_task`, then
`impl VmBackend for ProxmoxBackend`.

**Gotchas.** Most operations return a task id, not a result. A finished task
reports `status: stopped` whether it succeeded or not; the verdict is
`exitstatus` (`task_verdict`, crate doc).

### `delonix-truenas`

**Purpose.** Provisioning on a TrueNAS SCALE appliance: dataset, quota, NFS share
and permissions, so a `kind: Volume` does not require them made by hand. It only
**creates** what lives on the NAS; mounting stays in `delonix-volume` through the
same path as a hand-made share.

**Key modules.** A single `lib.rs`.

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `Client::connect` | connect and pin a supported major version | `crates/providers/delonix-truenas/src/lib.rs:Client` |
| `Client::ensure_dataset`, `set_permissions`, `ensure_nfs_share` | idempotent provisioning | `crates/providers/delonix-truenas/src/lib.rs` |
| `Client::remove_nfs_share`, `remove_dataset` | teardown | `crates/providers/delonix-truenas/src/lib.rs` |
| `validate_quota`, `validate_target_url`, `validate_dataset_name` | input checks before any request | `crates/providers/delonix-truenas/src/lib.rs` |

**Talks to.** `delonix-runtime-core` only; the appliance over HTTPS. Used by
`bins/delonix-runtime-bin/src/cmd/provision.rs`.

**Notable external dependencies.** `reqwest` (blocking, rustls), `serde`,
`serde_json`.

**Tests.** Inline unit tests; `crates/providers/delonix-truenas/tests/live.rs`
against a real appliance, skipped when unconfigured.

**Start reading at.** Crate doc in `src/lib.rs` (four measured findings), then
`Client::connect`, then `ensure_dataset`.

**Gotchas.** Some calls return a job id that must be polled (`wait_job`). Numeric
properties can be `null`; "no quota" is not the number 0 (crate doc).

## Interfaces

Interfaces expose the engine over a protocol. Each server runs as its own binary;
`delonix serve <x>` and `delonix mcp` `exec` it
(`bins/delonix-runtime-bin/src/cmd/serve.rs:exec_server`).

### `delonix-cri`

**Purpose.** A Kubernetes CRI server (`runtime.v1` RuntimeService and ImageService
over gRPC on a unix socket), so a kubelet or `crictl` can use the engine as the
node runtime. It also serves the streaming endpoints for exec/attach/port-forward
(WebSocket and SPDY). It keeps its own sandbox and container records under
`<root>/cri/` and does **not** start containers in-process.

**Key modules**

| Module | Responsibility |
|---|---|
| `lib.rs` | generated `cri` stubs, `DelonixImage` (ImageService), `serve_blocking` |
| `runtime_svc` | RuntimeService: `version`, `status`, runtime config, dispatch to lifecycle |
| `runtime_svc/lifecycle` | pod sandboxes and containers |
| `streaming`, `spdy` | exec/attach/port-forward streaming servers |
| `cap_ceiling` | node-level upper bound on capabilities |
| `child_handle` | a spawned child handle safe against pid reuse |
| `bin/delonix-cri.rs` | the `delonix-cri` executable |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `serve_blocking` | run the gRPC server on a socket | `crates/interfaces/delonix-cri/src/lib.rs:serve_blocking` |
| `CapCeiling`, `CeilingMode` | capability ceiling config | `crates/interfaces/delonix-cri/src/cap_ceiling.rs` |
| `lifecycle::run_pod_sandbox`, `create_container`, `start_container` | the lifecycle entry points | `crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs` |

**Talks to.**

- Clients: gRPC over a unix socket (`tonic`); stubs generated by `build.rs` from
  `crates/interfaces/delonix-cri/proto/api.proto`.
- Images: `delonix-image` in-process (`pull_from_registry_with_creds`,
  `ImageStore`).
- State: reads `delonix_runtime_core::Store` directly and calls
  `delonix_runtime::reconcile_status`.
- Starting, stopping and removing: runs the `delonix` CLI
  (`dispatch::cli_bin`) with `DELONIX_ROOT` and `DELONIX_INTERNAL=1`.
  `start_container` writes the `RunOpts` as a JSON file and runs
  `delonix __apirun <file>`, under `nsenter --net=<netns>` when the sandbox has a
  CNI namespace (`delonix_detached_why_in`). The module doc gives the reason: the
  server is multi-threaded and `clone`/`fork` are not safe there.
- Pod network: `delonix_net::cni` / `delonix_net::infra::cni_attach_container`
  in-process, or `delonix net netns attach` as a subprocess (rootless without CNI).

**Notable external dependencies.** `tonic`, `prost` (+ `tonic-build`), `tokio`,
`tokio-stream`, `axum` (WebSocket), `hyper`, `hyper-util`, `futures-util`,
`flate2`; dev-only `tower` for the gRPC round-trip test.

**Tests.** Inline unit tests; `crates/interfaces/delonix-cri/tests/grpc_status.rs`
does a real gRPC round trip over a unix socket.

**Start reading at.** `src/bin/delonix-cri.rs`, then `src/runtime_svc.rs`, then
`src/runtime_svc/lifecycle.rs` (`run_pod_sandbox`, `start_container`).

**Gotchas.**

- stderr of a detached engine run goes to a **file**, never a pipe: the
  container inherits the descriptor and a pipe would never reach EOF
  (`delonix_detached_why` doc).
- The runtime config must answer `Cgroupfs` (`engine_cgroup_driver`); the proto's
  zero value is `SYSTEMD`, so a default brings back the pod-kill loop the comment
  measured (ADR 0038).
- Never re-run the server's own executable to run a command; `cli_bin` exists
  because doing so bound the socket again.

### `delonix-mgmt`

**Purpose.** The local management API: HTTP+JSON over a unix socket, accepted only
for the calling uid (`SO_PEERCRED`). Reads (volumes, containers, images, networks,
VMs) are library calls; container mutations run the `delonix` CLI so they take the
engine's real path. It also collects the dashboard summary and publishes
Prometheus gauges. Its crate doc says new local clients belong on the node contract
of ADR-0040/0041 rather than these routes.

**Key modules**

| Module | Responsibility |
|---|---|
| `lib.rs` | `serve_blocking`, the `axum` router, handlers, `run_cli` |
| `dashstats` | `DashSummary`, `collect` (counts, memory, network, disk), timeouts, metrics publishing |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `serve_blocking` | run the server | `crates/interfaces/delonix-mgmt/src/lib.rs:serve_blocking` |
| `dashstats::collect` | summary shared by `delonix dashboard` and `/metrics` | `crates/interfaces/delonix-mgmt/src/dashstats.rs:collect` |

**Talks to.** Direct calls into `delonix-runtime-core` (`Store`),
`delonix-volume`, `delonix-image`, `delonix-scan`, `delonix-vm`, `delonix-net`
(`infra`, `NetworkStore`), `delonix-runtime`, `delonix-telemetry`. Mutations: the
`delonix` CLI as a subprocess (`run_cli`).

**Notable external dependencies.** `axum`, `tokio`, `hyper`, `hyper-util`,
`tower`.

**Tests.** Inline unit tests using `tower` against the router.

**Start reading at.** The router in `src/lib.rs` (`.route(` calls), then
`run_cli`, then `src/dashstats.rs`.

**Gotchas.** Arguments passed to the CLI are validated to reject a leading `-`
(`valid_arg`), otherwise an id could be parsed as a flag.

### `delonix-mcp`

**Purpose.** A Model Context Protocol server: a local AI control surface. Stdio
transport only; a child process of one client session, never a daemon. The single
principal is the local uid. Tool inputs are typed and schema-validated; outputs are
JSON text. It keeps a local audit log and an in-process task registry.

**Key modules**

| Module | Responsibility |
|---|---|
| `lib.rs` | `DelonixMcp` tools (`runtime.info`, `resource.list`, `container.restart`, …), `serve_stdio`, `doctor_checks` |
| `risk` | risk level per tool |
| `audit` | append-only `mcp/audit.log` |
| `tasks` | session-scoped task registry |

**Main public API**

| Item | What it is | Where |
|---|---|---|
| `serve_stdio` | run the server | `crates/interfaces/delonix-mcp/src/lib.rs:serve_stdio` |
| `DelonixMcp` | the tool handler | `crates/interfaces/delonix-mcp/src/lib.rs:DelonixMcp` |
| `capabilities_table`, `doctor_checks` | `delonix mcp capabilities` / `doctor` | `crates/interfaces/delonix-mcp/src/lib.rs` |

**Talks to.** Direct calls for reads: `delonix-runtime-core`, `delonix-vm`,
`delonix-volume`, `delonix-net`, `delonix-runtime` (`resource_advice`), and
`delonix-mgmt` (`dashstats`, a declared layering exception). Mutations run the
`delonix` CLI (`run_cli_blocking`, via `dispatch::cli_bin`).

**Notable external dependencies.** `rmcp` (server, stdio transport), `schemars`,
`tokio`, `sha2` (argument hashes in the audit log).

**Tests.** Inline unit tests (`tempfile` dev-dependency).

**Start reading at.** Crate doc in `src/lib.rs`, the `#[tool(` handlers, then
`src/risk.rs`.

**Gotchas.** Same as the CRI: mutations go through `cli_bin`, never
`current_exe()` (that is the server itself).

## Binaries

### `delonix-runtime-bin` (binary `delonix`)

**Purpose.** The CLI and the composition root. It parses commands (`clap`),
translates every entry point (flags, manifests, compose files, the Docker Engine
API slice, kind clusters) into engine calls, wires adapters to context ports, loads
manifests and applies each Kind, and prints (with the `po` translation catalog).
Hidden internal verbs (`netns pin`, `netns control`, `__apirun`, `__rmtree`,
`__ovlhold`, …) are intercepted in `main` before argument parsing so re-executed
processes land in the right code. It also hosts the Docker API slice and the L7
ingress proxy in-process.

**Key modules** (a selection; one module per command group in `src/cmd/`)

| Module | Responsibility |
|---|---|
| `main.rs` | internal-verb interception, `run`, backend and network registration |
| `cmd/container.rs` | `container` group; `cmd_run` composes the run use case |
| `cmd/manifest.rs` | manifest loading (`load`), lowering of `Stack`/`Workload` |
| `cmd/stack.rs` | `stack plan/apply/destroy`: plan, per-Kind apply, converge, prune, revisions |
| `cmd/vm.rs`, `cmd/vmimage.rs`, `cmd/vmfile.rs` | VMs, VM images, VMfile |
| `cmd/network.rs`, `cmd/firewall.rs`, `cmd/netns.rs` | networks, ingress/egress, holder commands |
| `cmd/image.rs`, `cmd/build.rs` | images and builds |
| `cmd/serve.rs`, `cmd/mcp.rs` | `exec` into the server binaries; `serve docker-api` in-process |
| `cmd/dockerapi.rs` | Docker Engine API slice; `run_from_spec_file` for `__apirun` |
| `cmd/policy.rs` | node runtime policy via `delonix-security-runtime` |
| `cmd/vmbackends.rs` | registers configured remote VM backends |
| `cmd/output.rs`, `cmd/po.rs` | tables/describe output, translation catalog |

**Main public API.** Not a library. The entry points a contributor meets first:
`bins/delonix-runtime-bin/src/main.rs:run`,
`bins/delonix-runtime-bin/src/cmd/container.rs:cmd_run`,
`bins/delonix-runtime-bin/src/cmd/stack.rs:build_plan`.

**Talks to.** Every engine crate except `delonix-cri` and `delonix-mcp` by direct
call (see the table). Server binaries by `exec`. Host tools directly from some
commands: `ssh`/`scp` (cluster bootstrap), `virsh`, `qemu-img`, `virt-ls`/`virt-cat`
(VM images), `systemctl`/`loginctl`/`systemd-run` (boot units, cgroup scopes),
`tcpdump`, `ip`, `ss`, `kubectl`. Re-executes itself for namespace entry
(`reexec_into_netns`) and mapped-uid operations.

**Notable external dependencies.** `clap`, `clap_complete`; `hyper`,
`hyper-util`, `tokio`, `tokio-rustls`, `rustls-pemfile`, `rcgen` (the embedded L7
proxy; Cargo.toml comment: already in the tree via other crates); `ratatui` (the
interactive dashboard, confined to this binary); `serde_yaml` (manifests);
`schemars` (schema generation); `oci-spec` (runtime); `reqwest`.

**Tests.** Many inline `#[cfg(test)]` modules, including CLI-shape tests in
`main.rs` (help translations, stability classification, dead command references);
`bins/delonix-runtime-bin/tests/architecture.rs` checks that the documented
architecture matches the code. `build.rs` embeds the project templates.

**Start reading at.** `src/main.rs` (`main`, then `run`), then
`src/cmd/container.rs:cmd_run`, then `src/cmd/stack.rs`.

**Gotchas.**

- Adding a command means updating every entry point that duplicates it, the `pt.po`
  catalog and the help tests; follow the feature checklist in
  [10](10-contributing-workflow.md).
- The engine binary's hidden verbs are matched on raw `argv` before `clap`;
  renaming a public command does not rename them.

### `delonix-mgmt-bin` (binary `delonix-mgmt`)

**Purpose.** The executable of the local management API. Checks the dispatch
version, reads `--addr` / `DELONIX_API_ADDR` (default
`unix:///run/delonix-mgmt.sock`) and `DELONIX_ROOT` (default `/var/lib/delonix`),
then calls `delonix_mgmt::serve_blocking`.

**Talks to.** `delonix-mgmt`, `delonix-runtime-core` (`dispatch`),
`delonix-telemetry` (`init`). Run by `delonix serve api`.

**Tests.** None of its own.

**Start reading at.** `bins/delonix-mgmt-bin/src/main.rs`.

### `delonix-mcp-bin` (binary `delonix-mcp`)

**Purpose.** The executable of the MCP server. Verbs `serve [--transport stdio]`,
`doctor`, `capabilities`.

**Talks to.** `delonix-mcp` (`serve_stdio`, `doctor_checks`,
`capabilities_table`), `delonix-runtime-core` (`dispatch`), `delonix-telemetry`.
Run by `delonix mcp <verb>`.

**Notable external dependencies.** `tokio`.

**Tests.** None of its own.

**Start reading at.** `bins/delonix-mcp-bin/src/main.rs`.

## How a request crosses the crates

Three flows, each arrow traced to a call in the tree. Function names are the ones
you can grep for.

### 1. `delonix container run -d -p 8080:80 nginx`

Default network (`--net host`), so the port is published by the container's own
`slirp4netns`, not by the holder. With `--net <custom>` the flow differs: the first
pass attaches through the holder and re-executes into the network namespace
(`attach_custom_network`, `reexec_into_netns`), and ports are published on the
holder by `HostNetwork::publish`.

```mermaid
sequenceDiagram
  actor Op as Operator
  participant CLI as delonix (cmd/container.rs)
  participant Pol as delonix-security-runtime
  participant Cmp as delonix-compute
  participant Img as delonix-image (HostImages)
  participant RT as delonix-runtime (HostWorkload)
  participant Net as delonix-net
  participant Slirp as slirp4netns (host tool)
  Op->>CLI: container run -d -p 8080:80 nginx
  CLI->>Pol: policy::enforce (admission::evaluate)
  CLI->>Cmp: preflight::check_run_opts(RunOpts)
  CLI->>Cmp: run::resolve_run(...)
  Cmp->>Img: ImageStore::resolve (resolve_or_pull)
  Cmp->>Img: ImageStore::prepare_rootfs
  CLI->>Cmp: run::build_record -> Container
  CLI->>Cmp: network::wire_network (no custom network)
  CLI->>Cmp: launch::start(Launch with slirp_ports)
  Cmp->>RT: WorkloadRuntime::supervise
  RT->>RT: supervise::run_supervised (fork), create_with, spawn (clone)
  RT->>Net: on_started hook: slirp_attach(pid, ports)
  Net->>Slirp: spawn with --api-socket, then slirp_add_hostfwd 8080 to 80
  RT->>RT: store.save(Container) after the init finished its mounts
  CLI-->>Op: container id
```

### 2. `delonix stack apply -f manifest.yaml`

```mermaid
sequenceDiagram
  actor Op as Operator
  participant Stk as delonix (cmd/stack.rs)
  participant Man as cmd/manifest.rs
  participant Rec as delonix-stack
  participant Kind as cmd per Kind (network.rs, volume.rs, container.rs, ...)
  participant Eng as adapters (delonix-net, delonix-volume, delonix-runtime, ...)
  Op->>Stk: stack apply -f manifest.yaml
  Stk->>Man: manifest::load (lowers Stack and Workload documents)
  Stk->>Stk: build_plan: desired_of, actual_of
  Stk->>Rec: reconcile::plan(desired, actual, stack) -> Vec of Change
  Stk->>Stk: refuse_unallowed (replacements need --replace)
  loop run_layers, in Kind order (kinds constants)
    Stk->>Kind: KIND::apply(docs)
    Kind->>Eng: create or ensure (e.g. container::apply calls cmd_run)
  end
  Stk->>Kind: converge_and_stamp: live updates (e.g. container::converge) and ownership label
  opt --prune
    Stk->>Kind: prune -> destroy_one
  end
  Stk->>Rec: revision::record
```

### 3. kubelet → `delonix-cri` → engine

```mermaid
sequenceDiagram
  participant K as kubelet
  participant CRI as delonix-cri (tonic server)
  participant Img as delonix-image
  participant NetC as delonix-net (cni / infra)
  participant CLI as delonix CLI (subprocess)
  participant Store as delonix-runtime-core Store
  K->>CRI: PullImage (gRPC over unix socket)
  CRI->>Img: pull_from_registry_with_creds
  K->>CRI: RunPodSandbox
  alt root, or rootless with DELONIX_CNI=1
    CRI->>NetC: CNI chain (cni_attach_container / named netns)
  else rootless without CNI
    CRI->>CLI: delonix net netns attach cri-id
  end
  CRI->>CRI: write sandbox record under root/cri/sandboxes
  K->>CRI: CreateContainer
  CRI->>CRI: write container record under root/cri/containers
  K->>CRI: StartContainer
  CRI->>CLI: [nsenter --net=netns] delonix __apirun spec.json
  CLI->>CLI: dockerapi::run_from_spec_file -> container::cmd_run
  K->>CRI: ContainerStatus
  CRI->>Store: load_reconciled (Store::open, reconcile_status)
```
