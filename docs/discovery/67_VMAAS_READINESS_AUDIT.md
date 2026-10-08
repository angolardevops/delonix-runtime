# 67 — VMaaS readiness audit: the VM base, measured

> **What this is.** A senior-level audit of the engine's VM subsystem against the
> question "can the PaaS build VM-as-a-Service on top of this?", done on
> `origin/main` at `1cbe9639e` (engine 5.0.0+32), in a dedicated worktree
> (`vmaas/auditoria`) on a host that genuinely has `/dev/kvm`, an active
> `libvirtd` and `cloud-hypervisor` — so most of it is measured against real
> boots, not read from code. Proxmox and OpenStack are **not reachable** from
> this environment (no credentials, no lab target); every claim about them is
> marked as such, not simulated.
>
> **What it replaces.** Nothing. It does not repeat what `docs/providers/
> capability-matrix.md` (ADR-0050) already measures and gates in CI — it reads
> that matrix, verifies it is current, and adds what that matrix does not
> cover: the Runtime→PaaS *contract* boundary, cloud-init/host-guest specifics,
> and two bugs found and fixed by actually booting VMs.

## 1. Inventory — what "the VM base" actually is

**Three `VmBackend` implementations, not four.** The request that triggered
this audit asked what "native KVM" means in the code. Measured
(`crates/contexts/delonix-node/src/virt.rs`, `cmd/system.rs::cmd_virt`): it is
**not a provider**. `/dev/kvm` is a kernel acceleration interface; "native KVM"
in this codebase is a label `delonix system virt` prints when the ENGINE'S OWN
HOST happens to be a VM with KVM acceleration available (nested virtualization
tuning) — unrelated to how a guest VM is provisioned. The engine has exactly
three backends behind one port (`delonix_compute::vm_backend::VmBackend`):

| Backend | Crate | VMM | Where the guest's network lives | Registered by |
|---|---|---|---|---|
| `cloud-hypervisor` | `delonix-provider-cloud-hypervisor` | Cloud Hypervisor (its own Rust VMM) on `/dev/kvm` | **inside the ingress holder's own netns** — the tap is a bridge port there | auto-detected, local |
| `libvirt` (aliases `kvm`, `qemu`) | `delonix-provider-libvirt` | QEMU, managed by `libvirtd`, driven through `virsh` | the HOST's own netns (`virbr0` in `nat` mode, or a host bridge in `bridge` mode) | auto-detected, local |
| `proxmox` | `delonix-proxmox` | the PVE node's own QEMU/KVM, over its REST API | the node's own network, never this host's | registered by the CLI from `DELONIX_PROXMOX_URL`/`_NODE` + a credential — **not** auto-detected, by design (it is a remote target, not a local capability) |

That network-location row is the root cause of §4's bug, and the reason the
capability catalog (ADR-0050) marks `vm.network.sdn` `supported` for CH and
`unsupported-by-provider` for libvirt: they are not two implementations of the
same feature, they are structurally different places for a guest to live.

**Images**: three repositories on `ghcr.io/angolardevops/` —
`delonix-vm-k8s` (golden, kubeadm-ready, Ubuntu 24.04, ~635 MiB),
`delonix-vm-base` (plain OS: Ubuntu 24.04/26.04, Debian bookworm, Rocky 9,
Fedora 42, 276 MiB–803 MiB), `delonix-vm-appliances` (OPNsense, Proxmox VE/PBS,
TrueNAS, Carbonio, GLPI, monitoring, OpenStack — no cloud-init, vendor
installers reproduced). Listing all of them took **47 s** on this host's link
(`image vm ls-remote`, measured) — slow but working; a 276 MiB pull took
**2 m 53 s** (~1.6 MB/s).

## 2. Cloud-init (§3 of the brief) — verified, not read

Pulled `delonix-vm-base:debian-bookworm` and booted it for real on
Cloud Hypervisor, isolated root (`/tmp/dlxvmaas-live`, removed afterwards),
with `--ssh-key @key.pub --hostname vmaaslive1 --wait`:

- cloud-init applied the **hostname** and the **injected key** correctly —
  `delonix vm ssh vmaaslive1 -- hostname` answered `vmaaslive1` over real SSH.
- The account the key lands on is `delonix` (the golden image's own build-time
  account), **not** the distro default (`ubuntu`/`debian`) — the engine's
  `cloudinit.rs` scopes the key under `users:` for exactly this reason (a
  previously-measured bug, see `cloudinit.rs`'s own doc-comment).
- Network config matches the guest's NIC by **MAC**, not by interface-name glob
  — the fix for a measured NetworkManager/netplan divergence (Fedora/Rocky vs.
  Ubuntu/Debian), already in place.
- The background agent's independent run additionally booted a **libvirt** VM
  from a real Ubuntu 24.04 golden image and confirmed the same: hostname,
  injected key, `vm ssh -- hostname` answering, IP **observed** (a real DHCP
  lease via `domifaddr`, not predicted).
- Multiple VMs from the same image do not collide: MAC/IP are derived
  deterministically from the VM's own name, and a second `vm create` on an
  interrupted name auto-heals instead of duplicating (see §6).

**Not verified in this pass**: partition/filesystem growth on first boot
(`growpart`/`resizefs`) — the capability catalog already records
`vm.disk.resize` as `not-implemented` on every backend, and no cloud-init
module in `cloudinit.rs` requests a resize. This is a real gap for VMaaS (a
tenant will ask for more disk), tracked in §7, not fixed in this pass.

## 3. Host-guest channel (§4 of the brief) — fixed one real gap, found one real bug

**QEMU Guest Agent only makes sense where QEMU is the VMM.** Checked before
writing anything: Cloud Hypervisor has no QGA-equivalent channel at all (no
vsock/virtio-console bridge implemented anywhere in the engine) — correctly
`not-implemented` for CH, not a bug. libvirt (QEMU) and Proxmox (QEMU) are
exactly the combinations that support it.

- **Proxmox already asks it** (`guest_info()`, over the PVE REST `/agent/...`
  proxy) — `get-osinfo`, `get-host-name`, `get-fsinfo`, `info`.
- **libvirt never did**, even though the golden image's own build recipe
  installs and **enables** `qemu-guest-agent`. **Fixed in this audit**: every
  domain XML this backend generates now carries the virtio-serial channel
  (`org.qemu.guest_agent.0`), and `guest_info()` queries it through `virsh
  qemu-agent-command`, mirroring Proxmox's field mapping exactly. Unit-tested
  (channel always present, the "agent unreachable" phrase classifier, the
  two pure JSON parsers); capability moved from `not-implemented` to
  `partial` for libvirt in the published matrix — **not yet `supported`**: no
  battery check boots a guest with a real agent and reads it back, and this
  pass did not either (the live pass went into the SSH bug below instead).
  Honest next step, not claimed here.

- **Found and FIXED a real, reachable bug**: `delonix vm ssh` against a
  Cloud Hypervisor VM failed with "No route to host"/"Connection timed out"
  even right after `--wait` correctly reported the VM up. Root cause: a CH
  VM's tap lives inside the **holder's own netns** (§1's table), and `cmd_ssh`
  (`bins/delonix-runtime-bin/src/cmd/vm.rs`) exec'd `ssh` directly in the
  CALLER's process — which has no route to the SDN's `10.x` range at all.
  libvirt's `nat`/`bridge` modes put the guest on `virbr0`/a host bridge,
  which **is** in the caller's netns, so the bug never shows there — that
  asymmetry is exactly why it went unnoticed.

  **Reproduced, fixed, and re-verified live, end to end, in this session**
  (not just unit-tested):
  ```
  $ ssh -i key delonix@10.200.254.226 hostname        # plain ssh, same host
  ssh: connect to host 10.200.254.226 port 22: Connection timed out

  $ delonix vm ssh vmaaslive1 -i key -- hostname       # FIXED path
  vmaaslive1                                           # rc=0, real guest answer
  ```
  The fix wraps the `ssh` exec with the same `nsenter` prefix
  (`delonix_sdn::infra::infra_join_argv`) the engine already uses to run a
  process inside the holder's netns elsewhere — no new mechanism, no new
  privilege boundary. The argv composition is extracted into a pure,
  unit-tested `ssh_argv` (wrap placement, identity, destination `--` guard,
  trailing command) so the logic that a live boot cannot cheaply re-exercise
  on every CI run is still regression-tested.

  **Impact if left unfixed**: the exact "next steps" text `vm create` itself
  prints (`ssh delonix@<ip>`) does not work for roughly half the engine's own
  VM backends, with an error that does not point at the real cause.

## 4. Runtime→PaaS boundary (§5 of the brief) — the main structural finding

The node API contract (`proto/delonix/node/v1/compute.proto`) already defines
a complete, well-shaped `VirtualMachineService`: create/get/list/delete,
start/stop/pause/resume, the full snapshot lifecycle, and a bidirectional
streaming `Console`. This is a good PaaS-consumable surface — identifiers,
typed errors, long-running `Operation`s with polling (ADR-0042), the same
pattern already serving `NetworkService`/`VolumeService`.

**Measured: zero of it is served.** `grep -rn VirtualMachine
crates/interfaces/delonix-node-api/src/` returns nothing; neither does
`ImageService`, `StackService`, `ContainerService` or `PodService`. The
server (`NodeApi` in `service.rs`) implements only `NodeService`,
`NetworkService`, `VolumeService` (reads) and `OperationService`. This is
**not an oversight** — `service.rs`'s own `not_yet()` helper and ADR-0040/
ADR-0050 D5 both name it explicitly: "lands with ADR-0040 P5". The engine's
own continuity plan (`66_CONTINUITY_PLAN.md`) has P5 as Sprint 7, after the
Kind-catalog and network-provider work in flight.

**What this means for VMaaS, stated plainly**: today, a PaaS that wants to
provision VMs through this engine has to shell out to the `delonix` CLI (or
drive `delonix-vm`/the providers as a library, which is also not how any
other consumer reaches this engine — see the "no private-repo dependency"
guardrail). There is no socket-level contract to call yet. This is the single
highest-leverage next step for VMaaS, and it is **already decided and staged**
— the right move is to follow P5's sequencing (network/volume/operations
first, because those are mid-flight; compute second), not to jump the queue
from this audit.

**What already respects the boundary well, measured in this pass**:
- cloud-init moved OUT of the CLI and into `delonix-vm::cloudinit` specifically
  so a backend that cannot read this host's filesystem (Proxmox) can still
  honour the same `hostname`/`ci_user`/`ssh_keys` *intent* natively — the
  CLI only resolves `@~/.ssh/id_ed25519.pub`-style conveniences before
  handing the engine a value, exactly the "no PaaS reading arbitrary files on
  a caller's behalf" line this crate's own doc-comment draws.
- the capability catalog (ADR-0050, 141 entries, six states) is a real,
  gated, machine-checked contract of *what a provider can be asked to do*,
  independent of whether the node-api server exists yet — a PaaS integration
  can already read `delonix provider ls -o json` today and will read the
  same shape from `GetCapabilities`/`ListProviders` once P5 lands them.
- unsupported fields are refused **by name, before any effect** (`refuse_
  unsupported`) on both local backends and Proxmox — a VM manifest that asks
  for `hugepages`/`tpm`/`cpuModel` on Proxmox, or `kernel`/`seed` on a backend
  that manages its own storage, is rejected up front, never silently ignored.
  This is exactly §5's "rejeita capacidades não suportadas... sem simular
  sucesso", already built.

## 5. Lifecycle (§6 of the brief) — the real matrix

Combines this session's own live tests (§2, §3) with an independent
background agent's run (same worktree binary, isolated `DELONIX_ROOT` **and**
`DELONIX_NET_RUNTIME_DIR`, cleaned up and verified by observation — `ps`,
`virsh -r`, `nsenter` into the holder's netns — not by trusting the CLI's own
exit code).

| Scenario | libvirt | cloud-hypervisor | Proxmox |
|---|---|---|---|
| create (empty disk) | PASS | PASS | **BLOCKED** — `DELONIX_PROXMOX_URL` absent, exit 3, documented precondition |
| create idempotent (same name twice) | PASS | PASS (same pid, no duplicate) | BLOCKED |
| stop preserves disk+record | PASS (undefine; disk/record stay) | PASS | BLOCKED |
| start after stop | PASS | PASS (new process, same overlay) | BLOCKED |
| restart ×2 back to back | PASS | PASS | BLOCKED |
| pause/unpause | PASS (`virsh domstate` confirms `paused`) | not exercised this pass | BLOCKED |
| snapshot create/ls/restore/rm | PASS | PASS (**offline only** — CH has no live-disk snapshot, and the refusal while running is correct and named) | BLOCKED |
| snapshot name reused → class 5 (Conflict) | PASS | PASS | BLOCKED |
| describe/delete of a name that does not exist → class 4 (NotFound) | PASS | PASS (`DX-4501`) | BLOCKED |
| delete cleans EVERYTHING (process, tap/netns, overlay, record) | PASS (confirmed via `virsh -r`) | PASS (confirmed via `nsenter` into the holder) | BLOCKED |
| `kind: VirtualMachine` (plan→apply→plan 0→drift→destroy) | PASS (12/12) | PASS (6/6, manual) | BLOCKED |
| real guest OS, cloud-init, SSH | **PASS** | **PASS** (after the §3 fix; **FAIL** before it) | BLOCKED |
| `kill -9` mid-`create`, 5× | PASS | PASS — zero orphan VMM process; an orphan overlay+dir is left (known class), `vm prune` reclaims it, a repeated `create` on the same name self-heals | not tested (no target) |
| cleanup confirmed by independent observation | PASS | PASS | BLOCKED |

**Proxmox, stated honestly**: the entire REST-API lifecycle (create, stop,
start, snapshot, destroy, the node lock gate, the field-refusal contract) is
**unvalidated in this pass**, for lack of a reachable node — not simulated,
not assumed. `docs/providers/capability-matrix.md` already carries real
`live:` evidence for most of Proxmox's rows from an earlier session's lab run
(two-node cluster with shared NFS, per ADR-0049/0053); this pass did not
re-run that lab.

**OpenStack**: out of scope for this pass (no backend exists in the engine —
confirmed by the repository's own architecture notes; not re-verified here).

## 6. Cloud-native principles (§7 of the brief) — what is verifiable today

- **Declarative, image separate from instance config**: `kind: VirtualMachine`
  plans/applies/diffs against a 3-way reconciler (ADR-0040); cloud-init intent
  is data on the record, never baked into the image.
- **No silent success on an unsupported request**: `refuse_unsupported` (§4),
  and the capability catalog's `UnavailableOnHost` state turns a declared
  "yes" into a probed "no" per host, never the reverse.
- **Idempotent + recoverable**: measured in §5 (`kill -9` self-heal, `create`
  idempotent, `delete`/`describe` on a missing name answer a stable error
  class).
- **Least privilege, explicit about what needs more**: libvirt/CH run
  rootless by default; the one privileged path this audit touched
  (`vm bridge`, VM↔container by IP) was explicitly NOT exercised — it needs
  root on a host this audit does not own alone.
- **What is NOT cloud-native yet, named plainly**: a VM is not disk-growable
  online (§2), and there is no socket-level API for a remote caller (§4) —
  cloud-init and a YAML manifest are necessary but not sufficient for "VMaaS",
  and this audit does not pretend otherwise.

## 7. What is fixed, what is open

### Fixed in this pass (both committed to `vmaas/auditoria`, pushed)

| # | Finding | Severity | Fix | Evidence |
|---|---|---|---|---|
| 1 | `vm.guest-agent` not asked on libvirt despite the golden image shipping the agent | Medium (observability/day-2 gap, not a correctness bug) | virtio-serial channel on every domain + `guest_info()` via `virsh qemu-agent-command` | 4 new unit tests, clippy clean, matrix regenerated and gate-verified (`the_published_matrix_is_the_generated_one`). **Not live-booted with a real agent in this pass** — marked `partial`, not `supported`, on purpose. |
| 2 | `vm ssh` unreachable on Cloud Hypervisor VMs (wrong netns) | **High** — breaks the engine's own documented "next step" for ~half its VM backends | wrap the `ssh` exec with the existing holder-netns `nsenter` prefix | 4 new unit tests + **live end-to-end reproduction and fix verification** (§3): "Connection timed out" → "Connection refused" (routing fixed) → `rc=0` real guest answer once cloud-init/sshd finished starting. Baseline re-confirmed broken (`Connection timed out`) immediately after, on a plain `ssh` from the same process, as a control. |

Both changes: `cargo fmt`/`clippy -D warnings`/`arch_fitness.py`/
`lang_ratchet.py` clean; the pt.po entry for the one new user-facing string
was added in the same commit (house rule). No crate boundary, dependency
direction or privilege model changed — `serde_json` is an existing workspace
dependency extended to one more crate that now needs to parse JSON, nothing
new pulled in.

### Open, in priority order (none attempted in this pass — scope and ADR sequencing)

1. **Node-API `VirtualMachineService`** (§4). Already specified, already
   staged behind ADR-0040 P5. The highest-leverage next step for actual
   PaaS consumption, and the one this audit explicitly did NOT jump ahead
   of — it is Sprint 7 of the engine's own continuity plan, after work
   already in flight.
2. **`vm.disk.resize`** (§2) — `not-implemented` on every backend. A tenant
   asking for a bigger disk is a VMaaS day-1 requirement; needs its own
   design (cold `qemu-img resize` + a cloud-init `growpart`/`resizefs` module
   at minimum; a live/online path is a separate, harder question per backend).
3. **Proxmox's full lifecycle, re-validated** — the matrix's `live:` evidence
   is from an earlier session; this audit could not refresh it (no reachable
   node). Re-run `delonix-proxmox/tests/live.rs` against the lab before
   trusting those rows for a VMaaS launch decision.
4. **`vm.guest-agent` on libvirt, promoted from `partial` to `supported`** —
   boot a real guest with the agent and add the battery/live evidence the
   matrix's evidence-gate already demands for that state.
5. **CH live-disk snapshot** stays correctly `unsupported-by-provider`
   (ADR already explains why: CH's own `vm.snapshot` API saves memory
   without the disk, which cannot be restored consistently) — not a gap,
   a documented limit worth repeating here so it is not re-discovered as
   a bug by the next VMaaS reviewer.

## 8. Reproducible guide (what this audit actually ran)

```bash
# Build, isolated target (never the shared checkout's):
export CARGO_TARGET_DIR=~/.cache/delonix-cargo-target/vmaas-auditoria
cargo build --release -p delonix-runtime-bin
BIN=$CARGO_TARGET_DIR/release/delonix

# Isolated state — BOTH vars, never only one (a half-isolated root shares
# this user's real network sockets with every other delonix instance on
# the host):
export DELONIX_ROOT=/tmp/dlxvmaas-live/root
export DELONIX_NET_RUNTIME_DIR=/tmp/dlxvmaas-live/net
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"

# Provision, verify, diagnose (the §3 reproduction):
$BIN image vm pull debian-bookworm
$BIN vm create t1 --backend cloud-hypervisor --disk delonix-vm-base:debian-bookworm \
  --memory 1G --ssh-key @key.pub --hostname t1 --wait --boot-timeout 90
$BIN vm ssh t1 -i key -- hostname          # the fixed path
$BIN provider matrix                        # the capability report

# Clean up — verify, don't assume:
$BIN vm rm -f t1
$BIN vm ls -A                               # must be empty
$BIN net netns down                         # tears down the isolated holder/pin
rm -rf /tmp/dlxvmaas-live
```

## 9. Contracts available to the PaaS today

- **`delonix provider ls|describe|matrix`** (ADR-0050): the capability
  catalog, per provider, per host — the closest thing to a stable contract
  for "what can I ask this node to do with VMs" that exists right now.
- **The node-API's `NetworkService`/`VolumeService`/`OperationService`**
  (ADR-0042): the PATTERN a future `VirtualMachineService` will follow —
  long-running `Operation`s, idempotency keys, typed errors with a numbered
  dictionary. A PaaS integrating today against network/volume operations is
  integrating against the shape VM operations will have tomorrow.
- **`kind: VirtualMachine`** manifests, plan/apply/diff/destroy: usable today
  by anything that can write YAML and shell out to `delonix stack apply` —
  not a socket contract, but a real declarative one.
- **Not yet available, and should not be built around**: any direct
  dependency on `delonix-vm`/the provider crates as a library from outside
  this repository — that would violate the "no private-repo dependency"
  guardrail and would have to be re-done once P5 ships the real contract.

## 10. What was not validated, stated plainly

- Proxmox and OpenStack: no reachable target in this environment.
- `vm.disk.resize`, `vm reach`/`vm bridge` (VM↔container by IP, needs root),
  physical/VLAN networking: out of scope for this pass or needing a
  privileged, shared-host action this audit chose not to take alone.
- `vm.guest-agent` on libvirt: code-complete and unit-tested, not live-booted
  against a real agent in this pass.
- A kubelet-driven workload on a VM-backed node: not attempted — this audit
  is about the VM base itself, not the Kubernetes layer above it.
