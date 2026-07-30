# Delonix Runtime — Current State (Phase 1 discovery)

> **Method.** Ground-truthed from the code on 2026-07-30 (`crates/*/src`, `Cargo.toml`,
> `grep`/`wc`), not from prose. Where a claim could not be confirmed in a file it is marked
> *unverified*. This document describes what the runtime **is today** — not what it should
> become. See [runtime-architecture.md](runtime-architecture.md) for the target-vs-reality
> matrix and [dependency-map.md](dependency-map.md) for the crate graph.

## 1. Shape at a glance

- **10 crates** (the `CLAUDE.md` "8 crates" table undercounts — it omits `delonix-mgmt`
  and `delonix-scan`, which are real workspace members).
- **~71 000 LOC** of Rust under `crates/*/src`.
- **2 binaries**: `delonix` (the CLI, in `delonix-runtime-bin`) and `delonix-cri` (the
  kubelet-facing CRI server, in `delonix-cri`).
- **1 public trait** in the whole workspace: `VmBackend` (`delonix-vm/src/lib.rs:438`).
  The "trait-based, plugin-driven" architecture the vision calls for is, today, a single
  trait plus a `select_backend()` match — not a dynamic plugin system.
- **Zero private dependencies.** No crate depends on `delonix-core`/`delonix-api`/
  `delonix-orchestrator`/`delonix-overlay` (the private `delonix-paas` monorepo). The
  Regra de ouro holds — verified.

## 2. Crate inventory

| Crate | LOC | Bin | Role (confirmed in code) |
|---|---:|---|---|
| `delonix-runtime-core` | 3 663 | — | Shared types (`Container`, `Vm`, `Status`), `Store`/`JsonStore`, typestate, virt detection, Secret Manager (`secret`/`cred_vault`), **and the cross-cutting foundations below** (`events`, `telemetry`, `metrics`, `peer_cred`, `workload_net`). The dependency **sink** — depends on nothing internal. |
| `delonix-runtime` | 4 974 | — | Container engine: `clone`/namespaces/cgroups, create/stop/exec, `reconcile_status`. Contains `spawn()` (~405 lines, flagged as maintenance risk). |
| `delonix-runtime-bin` | **38 050** | `delonix` | The full CLI (44 `cmd/*` modules). Dominant crate by far — effectively the product surface. |
| `delonix-net` | 9 660 | — | Rootless SDN: holder netns + bridge + single slirp, DNAT/firewall (nft), CNI compat, IPAM, WireGuard overlay, eBPF device-cgroup. |
| `delonix-image` | 4 503 | — | OCI: pull/registry/build, CNB buildpacks, internal registry, CAS, overlay, OCI-archive save, signature verification. |
| `delonix-cri` | 3 842 | `delonix-cri` | CRI (`runtime.v1`) server for a kubelet. Full method surface (§5). |
| `delonix-vm` | 2 437 | — | microVMs: the `VmBackend` trait (Cloud Hypervisor / libvirt). |
| `delonix-mgmt` | 1 792 | — | Management HTTP server: `/metrics` (Prometheus) + `/v1/*` (dash, volumes, containers). |
| `delonix-volume` | 1 227 | — | Named volumes and bind mounts. |
| `delonix-scan` | 939 | — | Image/filesystem scanning (`pytree` language detection). |

## 3. What the runtime does today (confirmed)

- **Containers** — full Docker/Podman-verb lifecycle (run/ps/stop/rm/exec/logs/update/
  kill/wait/restart/…), rootless-first via userns re-exec, hot reconfig (`container update`
  changes ports/volumes/nets/bandwidth without restart), CDI GPU, labels.
- **microVMs** — create/start/stop/rm/status over Cloud Hypervisor **or** libvirt, cloud-init
  seed, golden-image build pipeline (`image --vm build`), `vm bridge` (experimental, privileged).
- **Images** — pull/push (OCI registry), build (Dockerfile/Delonixfile, multi-stage, secrets,
  cache), OCI-archive export/import (`cluster load`), signature verification.
- **Networking (SDN)** — rootless bridge + slirp, per-container firewall (ingress/egress),
  namespace isolation, `kind: Dependency` directed reachability, internal DNS
  (`<name>.<ns>.delonix.internal`), embedded L7 reverse-proxy (`kind: HTTPRoute`/`Ingress`),
  WireGuard-encrypted overlay between nodes.
- **Declarative surface** — Kubernetes-style manifests (`apiVersion: delonix.io/v1`, `kind:`)
  for Network/Volume/Image/Vm/Container/Pod/Stack/Storage/Secret/HTTPRoute/Ingress/…,
  plus native `docker-compose.yml` support (`delonix compose`).
- **Kubernetes** — serves a kubelet as its CRI (`delonix-cri`); bootstraps clusters
  (`cluster apply`/`cluster kubeadm`, HAProxy auto-LB for HA, external etcd).
- **Docker Engine API slice** — `delonix serve docker-api` (`DOCKER_HOST=unix://…`).

## 4. Cross-cutting foundations that ALREADY EXIST (the important discovery)

The target vision treats Event/Observability/Metrics as greenfield. They are not — they exist
and are wired, built in the **daemonless idiom** (file/registry, not a background daemon):

- **Event bus — `core/events.rs`.** An append-only `events.jsonl` ("the file IS the bus").
  Daemonless by design: each ephemeral process appends (`emit`), readers `tail` (`read`/
  `read_from`). Lock-free via `O_APPEND` < `PIPE_BUF` atomicity. **Wired** — emitters in
  `cmd/{container,image,volume,secret,storage,sharevolume,system}.rs`, `main.rs`, `delonix-cri`.
  This is already the "Event Engine consumable by other components" — the consumer tails a file.
- **Observability — `core/telemetry.rs`.** `tracing` structured logging **plus** OpenTelemetry/
  OTLP distributed spans (`BatchSpanProcessor` on a dedicated thread + blocking HTTP, so it works
  in both the tokio-less CLI and the tokio CRI server). This is the OTel/tracing layer the vision
  asks to "integrate" — it is implemented.
- **Metrics — `core/metrics.rs`.** A shared `prometheus-client` registry (counters + gauges:
  containers/vms running/total, memory, network rx/tx, storage by area), exposed at `/metrics`
  by both `delonix-cri` and `delonix-mgmt`. `Gauge` chosen deliberately over `Counter` for the
  dynamic-set byte sums.
- **Secret Manager — `core/secret.rs` + `core/cred_vault.rs`.** Docker-style `--secret`/
  `--secret-files`. (This is the *runtime's* secret store — **not** a platform/SSO vault, per
  the Regra de ouro.)
- **Workload IP space — `core/workload_net.rs`.** A single shared constant for the ingress
  workload address range (`10.200–10.254`), owned by core so `delonix-net` and the tunnel guard
  can't drift apart. A hint of the future `Workload` abstraction, but today just an address range.

## 5. CRI surface (kubelet-facing)

`delonix-cri` implements the full `runtime.v1` method set: pod sandbox lifecycle, container
lifecycle, `exec`/`attach`/`port_forward`, `container_stats`/`pod_sandbox_stats`,
`update_container_resources`, `get_container_events`, `list_metric_descriptors`, and
`checkpoint_container`. **Verified (2026-07-30):** `checkpoint_container` is a **stub** —
`todo("checkpoint_container")` → `Status::unimplemented` (`runtime_svc.rs:354`), and there is
**zero CRIU** anywhere in the repo. So there is **no Recovery-Engine foundation** today. The only
real building block is `set_frozen`/`is_frozen` (cgroup-freeze, `delonix-runtime/lib.rs:4498`, used
by `pause`/`unpause`) — that is *freeze*, not *checkpoint* (no memory image, no restore-from-disk).
Returning `Unimplemented` for this optional CRI method (forensic checkpointing, KEP-2008) is
standard and acceptable; it just means checkpoint/restore is unbuilt, not merely unverified. See
[ADR-0004](../adr/0004-container-checkpoint-restore.md) for the scoping decision.

## 6. Process & daemon model (the daemonless nuance)

"Daemonless" does **not** mean "no long-running process ever" — it means **no permanent system
daemon that must be alive for the CLI to work**. Reality:

- The `delonix` CLI is **ephemeral** — born per command, does its work, dies.
- Three processes are **long-running but on-demand / scoped**, started only when needed:
  `delonix-cri serve` (kubelet endpoint), `delonix-mgmt serve` (control-plane scrape), and the
  **network holder** (`delonix-net/infra.rs:control_loop` — `!` return, holds the SDN netns,
  authenticated by `SO_PEERCRED` + 0600). These are infrastructure, not a product daemon.

Any proposal that adds a *permanent* daemon (`delonixd`, an always-on event/plugin/recovery
engine) is a **philosophy change** and needs its own ADR (see `delonix-adr` skill), not an
incremental step.

## 7. Security posture today

- **Boundary auth = `SO_PEERCRED` uid check** (`core/peer_cred.rs::peer_uid`) at every control
  socket (holder, cri, mgmt, docker-api) + `0600` socket mode. Only the same uid can talk to
  the holder.
- **Linux isolation primitives** (in `delonix-runtime/lib.rs`): userns (`CLONE_NEWUSER` + uid/gid
  maps), mount/net/ipc/uts/pid namespaces, cgroup v2 (delegated in rootless), seccomp allowlist,
  capability masking. AppArmor/SELinux are **referenced/abstracted**, not enforced by the engine.
- **No application-level authorization model.** There is **no** capability/RBAC/identity/tenant
  layer (`enum Capability`, `authorize()`, `Identity`, RBAC — all absent; the "tenant" grep hits
  are comments/test names about NAS-share subdirectories, not a tenancy model). The vision's
  "Zero-Trust, every request carries identity+capability+tenant+audit" is genuinely **absent** —
  and the *tenant* half is **out of bounds** for this repo (belongs to `delonix-paas`).

## 8. What does NOT exist today (honest gap list)

Absent from the codebase (confirmed by grep, zero hits): dynamic **plugin loading** (only a
static `select_backend` match); a **generic compute driver trait** (only `VmBackend`); a
`kind: Workload` **unified model**; a **capability/RBAC/identity** model; **Ceph/CSI/ZFS/LVM/
Object-Storage** drivers; **OVS/SR-IOV** dataplane; VM **snapshot/clone/suspend/resume/live-
migrate** as first-class engine operations (some exist only as CLI-flag surface); a **multi-node
scheduler** (out of scope by design); `criterion`/`cargo-fuzz`/`cargo-bench` **infra** (only
`proptest`, in `delonix-net`); any **eBPF observability** (the one `bpf.rs` is device-cgroup, not
telemetry); a permanent **daemon**.
