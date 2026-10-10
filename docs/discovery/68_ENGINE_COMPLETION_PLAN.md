# Engine completion plan — every open item, every Proposed ADR, one owner

> **What this document is.** The single list of everything still open in this
> repository on 2026-10-10, re-measured against `origin/main` `315cef6f`, and the
> order in which one session closes it. It extends
> [`66_CONTINUITY_PLAN.md`](66_CONTINUITY_PLAN.md): Sprints 0–2 of that plan are
> delivered and stay recorded there; **from Sprint 3 on, the order of delivery is
> this document's**. `65_PLANO_MATURIDADE.md` stays the plan by functional domain
> and `docs/roadmap/13-improvements-traceability.md` the matrix of record.
>
> **What changed since 66 was written.** The owner asked (2026-10-10) for three
> things together: treat every `Proposed` ADR, not only the ones with work
> scheduled; consolidate the work of all sessions into this one; and close every
> sprint with a **live** validation on the golden VMs or appliances that already
> exist on this host, under a resource budget that keeps the workstation usable.
>
> **What it is not.** A calendar. Sprints are ordered by dependency and by what
> each one unblocks.

| Field | Value |
|---|---|
| Measured on | **2026-10-10** |
| Against | `origin/main` `315cef6f`, engine **5.0.0** (+102 commits since the tag) |
| Cell metric | **103 / 273 = 37.7 %** (header of `docs/providers/capability-matrix.md`; the denominator grew from 252) |
| CLI exec coverage | **181 / 275 = 65.8 %** (`scripts/cli_exec_trace.tsv`, #761) |
| Open pull requests | 0 on `origin` when measured; the `vm image` migration (branch `vm-image-migration`) is pushed and waiting for its battery run |
| Sessions working on this repo | **0** besides this one — see §1 |
| `Proposed` ADRs | **16** (§3) |

---

## 1. Review: what the other sessions closed, and what they left

Every session that touched this repo is stopped, and its work is in `main`. The
review was done against git, not against what the sessions said about
themselves.

| Session | Delivered (merged) | Left open |
|---|---|---|
| VMaaS readiness audit | the audit (#741), gaps #3/#4/#5/#6/#7/#10/#13 fixed or measured, release v5.0.0 published and its assets validated (#770) | `install.sh` end to end on a clean host (66 §1.4); the items of §2 marked *67* |
| Design validation | operation-ledger race and idempotency fingerprint (#762), VM image defaults through the node API (#763), ADR-0076 accepted (#765) | ADR-0076 «Known limitations» (§2 item N3) |
| Net ingress publish binding | the bind address of a published port is recorded (#718) | — |
| Flaky temp leak in `delonix-sdn` | #719 | — |
| Runtime pending items | #712 | — |
| This session | the `vm image` CLI migration (Sprint 9 of the CLI restructuring), on branch `vm-image-migration` | ships with Sprint 10 |

**Worktrees left on disk**: `aprendizados-v5` is fully contained in `main` (0
patches outside it) and is removed in Sprint 10. `rota-multi-homed-v31` is **not**
lost work: it is the deliberate backport of #733 to the v3.1 line, already pushed
as `backport/v3.1-rota-multi-homed`; whether to cut a `v3.1.1` from it is an
owner decision (§3, row X1).

### 1.1 Statements in the documents that git contradicts

Each of these is a line a reader would act on and get wrong. All are fixed in
Sprint 10, in the document that carries them.

| # | Where | Says | Git says |
|---|---|---|---|
| C1 | ADR-0068 status; 66 §2.6 | «nothing implemented» | Phase 1 on Cloud Hypervisor shipped (bf6e548a, #749) |
| C2 | proxmox/CH live report §9 | ADR-0064 D6 «decided, not implemented» | implemented (#758) |
| C3 | ADR-0069 Pending 1 | holder-respawn test for a pod and `net ingress\|egress <pod>` still to do | done (#713) |
| C4 | ADR-0038, last section | live validation blocked by the control-plane crash-loop | the crash-loop was the cgroup driver, closed (AGENTS.md «RESOLVIDO»); what remains is a kubelet run |
| C5 | ADR index, row 0003 | «wait for a consumer» | ADR-0041 (Accepted 2026-10-06) says the trigger is met |
| C6 | ADR-0026, ADR-0057 | Proposed | both built (#163; #541) |
| C7 | ADR-0005 status | remaining commands are follow-up | 10/10 done (eaa41be1); its text still names the removed `storage ls`, `sharevolume ls`, `image --vm ls` |
| C8 | ADR-0070 status; 66 §2.3 | the table lists what is still to move; the owner decides 0072/0073 | both rows closed by ADR-0072/0073, accepted 2026-10-08 |
| C9 | 65 §2.4 and §6 | node contract serves 1 RPC; cell metric 39.7 %; CLI coverage 37 % / 58.8 % | Node, Network, Volume reads, VirtualMachine and Operations are served; 37.7 %; 65.8 % |
| C10 | 67 §4, §7.1 | `VirtualMachineService` «zero served» | served except `Console` (6d8606bc) |
| C11 | naas audit §3 #5 | admission warning not fixed | fixed (#767) |
| C12 | `scripts/e2e.sh` | `xfail ACH-034` | fixed in bbf2a972; the mark fails the battery by XPASS — turned into a `check` on the `vm-image-migration` branch |
| C13 | ADR numbering | two ADRs carry 0027 (pidfd, rootless sensor) | renumbering is the owner's call (§3, row X2) — done in 11.2: ADR-0077 |

---

## 2. The open items

Grouped by the sprint that closes them. «Needs» is what the item needs to RUN,
which decides where its live validation happens (§4).

| ID | Source | Item | Needs | Size |
|---|---|---|---|---|
| N1 | 66 §1.4 | `install.sh` end to end on a clean host | golden VM | S |
| N2 | 66 §0.3 | revoke the `dlxs6` lab token on `pve-lab-475` | proxmox-ve appliance | S |
| N3 | ADR-0076 | the node API never calls `delonix_vm::set_network`, so `CreateVirtualMachine` with a network fails | Cloud Hypervisor | S |
| N4 | 65 F0.4 / 66 3.4 | test guest image < 100 MiB booting on libvirt **and** CH with cloud-init, guest agent, serial console | golden VM, CH | M |
| N5 | 65 F0.6 / 66 3.6 | state-upgrade test: a root written by N-1 opened by N | nothing | M |
| N6 | 65 F0.5 / 66 3.5 | re-measure CRI with `critest` v1.36 (published 79/103 is from v0.63.1) | golden `delonix-vm-k8s:1.36` | M |
| N7 | ADR-0042 E–G, ADR-0040 P5 | node API: `WatchEvents`, `WatchOperation`, `CancelOperation`, `Create/DeleteVolume`, `Connect/DisconnectContainer`, VM `Console`, the Container/Pod/Image/Stack services; socket activation; the D4 conformance suite; step F (retire `delonix-mgmt`); step G (docs gate) | nothing; socket activation in a golden VM | L |
| N8 | ADR-0054 slices 5–6 | node API, MCP and CRI read `providers.yaml`; `provider-lifecycle.sh` driven by the file alone | libvirt, proxmox-ve | M |
| N9 | ADR-0059 F6 | network validate/plan/apply/observe RPCs (deferred by the owner until P5) | nothing | M |
| N10 | ADR-0040 P2/P6/P7 | CLI as a library; `ComputeDriver` out of `cmd/workload.rs` (ADR-0002 Ph 2b); in-process CRI (`self_exec_sites` 30); observability (`library_prints` 102) | nothing; kubelet node for P6's proof | L |
| N11 | 65 F0.2 / 66 3.2 | CLI exec coverage past 65.8 %: `systemcontainer` 0/6, `cluster`, the rest of `net` | root, golden VM, CH, proxmox-ve | M |
| N12 | 65 F1 / 66 S4 | `partial` → proved: VM compute, guest, OOM, backup, cold migration, net, firewall (linux and proxmox), inventory, access | golden VM, CH, proxmox-ve | L |
| N13 | 65 F2 / 66 S5 | one failure path per N2 cell (chaos scenario, exit class, event); async operations generalised | root, VM | L |
| N14 | ADR-0038; naas §3 #4 | live-validate the systemd `SetUnitProperties` branch against a real kubelet | golden k8s VM | S |
| N15 | naas §8 | multi-node kubeadm stays `NotReady`: no cross-node CNI, and no `--cni` refusal saying so | 2 golden k8s VMs | M |
| N16 | 67 §7.2–3, §10 | Proxmox lifecycle re-validated; `resize_disk` on an existing VM; `delonix-vm-base:debian-bookworm` rebuilt with the guest agent; online disk grow | proxmox-ve | M |
| N17 | proxmox/CH report §5.5 | six untested live cases: four migrations, guest-agent VMID, PowerDNS registration | 2 × proxmox-ve + NFS | M |
| N18 | ADR-0063 | plan does not read IPAM gateway entries; D3.3 has unit proof only | proxmox-ve | M |
| N19 | ADR-0063 D1.3/D1.4 | NetBox spike (called off on host resources) | NetBox + proxmox-ve | M |
| N20 | ADR-0067 P1–P6 | storage helper, btrfs, ZFS, LVM-thin, VM disks in pools, destroy guards | root VM with spare disks | L |
| N21 | ADR-0065 P1–P6 | IPv6 dataplane | golden VM; a v6-egress host for G3 | L |
| N22 | ADR-0066 F1–F3 | L4 VIP Service, `svc` chain, readiness | golden VM | L |
| N23 | ADR-0068 Ph1 rest, Ph2–6 | hotplug on libvirt and Proxmox; base images that online (D8); disk add, NIC add, removal | golden VM, proxmox-ve | L |
| N24 | ADR-0048 Ph3 | service credentials (`--credentials`/`--reveal`) | nothing | M |
| N25 | ADR-0069 Pending 5, 8 | Pod standalone (`emptyDir`, host/none, probes, init); CNI `VERSION`/`GC` and persisted netconf | golden k8s VM | M–L |
| N26 | 65 F4 / 66 S8 | CRI ≥ 95 %; the Compose keys of a real sample (49 of 89 missing); Docker API refusals re-read against Testcontainers | golden k8s VM, golden VM | L |
| N27 | 65 F5 / 66 S9; roadmap M03–M13 | continuous fuzzing; the engine's SLIs (M08 not started); `observe`/`diagnose`; split `spawn()`; a user page per N3 cell; `build type=ssh` + provenance; CVE feed; plugin SDK; gates per maturity level | nothing / golden VM | L |
| N28 | ADR-0049 slices 3–4 | 161/675 routes; agent exec routes untested live | proxmox-ve | M |
| N29 | proxmox/CH report §6.2 | write down the live-suite ordering: the datacenter firewall breaks DHCP on later SDN zones | nothing | S |
| N30 | `scripts/cli_exec_trace.tsv` header | the last battery run had FAIL=8 called «load flakiness»; a clean full battery and chaos run on the tip | quiet host | S |

---

## 3. The sixteen `Proposed` ADRs — decided by the owner on 2026-10-10

The recommendation was written so one reading was enough to say yes or no to
each row; the owner answered the same day. «Accept» on a built ADR records what
the code already does — it is not new work. Where the decision differs from the
recommendation, both are kept, so the reason for the difference is not lost.

| ADR | Decides | Evidence | Recommended | **Decided** | Sprint |
|---|---|---|---|---|---|
| 0026 | security runtime as a decision crate | built (#163), extended (#767) | accept (C6) | **Accepted** | — |
| 0057 | Proxmox VM from an image of the engine's store | built and live-measured (#541) | accept (C6) | **Accepted** | live re-check in 11 |
| 0068 | VM hotplug CPU/memory/disk/NIC | spike + Phase 1 on CH (#749) | accept | **Accepted** | 18 |
| 0012 | the reboot convergence class | gap measured; its «no hotplug» premise is stale | accept, merged with 0068 OQ1 | **Accepted**, merged with 0068 OQ1 | 18 |
| 0048 | service names and credentials | phases 1–2 built (#448, #449) | accept | **Accepted** | 20 |
| 0003 | default-off capability gate at socket dispatch | ADR-0041 says the trigger is met (C5) | accept | **Accepted** | 13 |
| 0047 | the L7 proxy authorises container sources | ADR-0046 Ph1b left the hole | accept the spike | **Accepted**, spike first | 17 |
| 0065 | IPv6 dataplane | spike done; nothing implemented | accept | **Accepted**; G3 (v6 egress) stays `partial` until a v6 uplink exists | 17 |
| 0066 | L4 load balancer | spike done | accept | **Accepted** | 17 |
| 0033 | OCI hooks stay unimplemented | no consumer | accept as not-to-build | **Accepted — not to build** | — |
| 0034 | no CSI daemon | daemonless principle | accept as not-to-build | **Accepted — not to build** | — |
| 0021 | `kind: GitOpsSource` pull reconciler | baseline only | defer until a consumer | **Accepted — to build** | 24 |
| 0027 (sensor) | seccomp user-notification sensor | GO/NO-GO closed; per-syscall cost not measured | measure, then decide | **Accepted — to build**, the cost measured first | 24 |
| 0004 | rootless CRIU checkpoint | no spike; CRIU absent | reject for now | **Accepted — to build**, behind its GO/NO-GO spike | 24 |
| 0036 | macOS/Windows as a VM launcher | design only | stays blocked | **Stays `Proposed`, blocked** on a Windows host and a real Mac (§6) | — |
| 0039 | OpenStack `VmBackend` | spike plan only | stays blocked | **Stays `Proposed`, blocked** on a live cloud; the `openstack:2026.1` appliance asks 8 vCPU / 16 GiB, past this host's budget (§4) | — |
| X1 | (not an ADR) cut `v3.1.1` from `backport/v3.1-rota-multi-homed`? | backport pushed | only if a consumer is pinned to v3.1 | **No** — the line is v5.0.0; the local worktree is removed, the remote branch stays as history | — |
| X2 | (not an ADR) renumber one of the two ADR-0027 | — | renumber the sensor ADR, stub at the old file | — | 11 |

The statuses and index rows were updated in the same change that recorded these
decisions, and `adr_status_gate.py` passes on them.

---

## 4. The lab budget — how a live validation runs on this workstation

Every sprint closes with a live validation, and this host is a development
workstation (32 threads, 30 GiB RAM; on 2026-10-10, **68–76 GiB free on `/`**, where the
isolated roots of a validation live, and 200 GiB on the separate disk of the real state root), with a self-hosted CI runner of another repository sharing it. A
validation that freezes the machine is a validation nobody runs twice. So:

1. **Preflight, refused rather than squeezed.** Before booting anything: free RAM
   that leaves ≥ 4 GiB after the guest, ≥ 30 GiB free on the filesystem of the
   `DELONIX_ROOT` in force, load(1m) below three quarters of the threads. If any fails, the validation
   waits and the sprint report says so — it is never run on a starved host and
   then blamed on the engine. `scripts/lab_budget.sh` makes the check and prints the
   three numbers (Sprint 10.3, branch `lab/budget-preflight`).
2. **One guest at a time**, two only where the item is about two (N15, N17).
   Golden images and Proxmox appliances are capped at **2 vCPU / 4 GiB**;
   `truenas-scale` (asks 4/8) only when ≥ 12 GiB are free; `openstack` (8/16)
   never on this host.
3. **Overlays, never the image.** A guest is created from a store image, which
   makes a per-VM overlay; nothing writes to the golden image.
4. **Isolated state.** `DELONIX_ROOT` and `DELONIX_NET_RUNTIME_DIR` both point
   at the sprint's own short directory (`/tmp/dlx<sprint>`); half-isolation is
   worse than none (AGENTS.md, 2026-08-12).
5. **Low priority.** Builds and tests run under `nice -n 15 … -j 16`, never the
   full 32 threads.
6. **Teardown in the same step, measured.** `virsh -c qemu:///system list --all`
   and free disk are recorded before and after; the sprint does not close with a
   domain or a disk it created still present. The fifteen shut-off domains that
   already exist (`pve-lab*`, `opnsense-*`, `k8scri`, `labvm`, …) belong to the
   owner's lab and are never touched.
7. **Evidence, not a status.** The report of each validation names the guest,
   the image, the commands, and the numbers read from the guest or the kernel —
   not what the CLI said it did.

Images available on 2026-10-10 (`delonix vm image ls`): `delonix-vm-base` for
Debian bookworm, Fedora 42, Rocky 9, Ubuntu 24.04 and 26.04; `delonix-vm-k8s:1.36`;
appliances `proxmox-ve:9.2-r2`, `proxmox-backup-server:4.2-r2`,
`proxmox-mail-gateway:9.1-r2`, `proxmox-datacenter-manager:1.1-r2`,
`opnsense:26.1`, `truenas-scale:25.10`, `freepbx:17`, `openstack:2026.1`.

---

## 5. Sprints

Each row's «Live validation» is the gate that closes the sprint.

### Sprint 10 — Truth, hygiene, and the work in flight

| # | Item |
|---|---|
| 10.1 | Merge the `vm image` migration — PR #772: battery PASS=1235 FAIL=0 SKIP=13 on its tree, `cli_exec_trace.tsv` re-recorded at 182/270 = 67.4 %, C12 included |
| 10.2 | **Done 2026-10-10**: C1–C11 fixed in the documents that carry them — ADR statuses in place, dated reports with a dated correction next to the original sentence |
| 10.3 | **Done 2026-10-10** (branch `lab/budget-preflight`): `scripts/lab_budget.sh` (§4 rule 1) with eight tests |
| 10.4 | Remove the `aprendizados-v5` worktree and branch (**done 2026-10-10**: no worktree left, the remote branch was fully merged by #734 and is deleted); N29 (in this PR: `crates/providers/delonix-proxmox/tests/live.rs` header); N30 |
| 10.5 | N1: `install.sh` end to end — **done 2026-10-10**, see the result below |

**Live validation**: N1 on a fresh `delonix-vm-base:ubuntu-24.04` guest (libvirt,
2 vCPU / 2 GiB): run the published `install.sh`, then `delonix --version` reports
the v5.0.0 commit and `delonix container run --rm alpine true` exits 0 inside the
guest.

**Result (2026-10-10)**: `lab_budget.sh --ram 2 --vcpus 2` said go (20 GiB available,
load 7.3 on 32 threads). The guest booted in 15 s; `install.sh` from `origin/main` installed
v5.0.0 (`2837f5863` = `v5.0.0^{commit}`) in 51 s; `container run --rm alpine:3.20` exited 0, and
`-m 128M` under a delegated scope read back as `memory.max` 134217728. **One defect**: the
installer's own `user namespaces` check failed on every Ubuntu 23.10+ host, because it ran a
bare `unshare` the delonix AppArmor profile does not cover — fixed in #775 and re-measured OK
on the same guest. Teardown: the owner's 15 libvirt domains unchanged, 578.7 MiB freed.

### Sprint 11 — The ADR decisions

| # | Item |
|---|---|
| 11.1 | **Done 2026-10-10**: the owner decided §3; statuses and index updated with the plan |
| 11.2 | X2 renumbering — **done 2026-10-10**: the sensor ADR is ADR-0077, the old file a stub, the spike directory moved with it |
| 11.3 | N2: revoke `dlxs6` |

**Live validation**: ADR-0057's path re-measured on a `proxmox-ve:9.2-r2`
appliance (2 vCPU / 4 GiB): a VM boots from an image of the engine's store,
uploaded and imported by the node; the appliance and the VM are destroyed after.

### Sprint 12 — The foundation of evidence

| # | Item |
|---|---|
| 12.1 | N4: the < 100 MiB test guest image |
| 12.2 | N5: state-upgrade test (a v5.0.0 root opened by the tip) — **done 2026-10-10**: `scripts/e2e_state_upgrade.sh`, wired into the battery behind `DELONIX_UPGRADE_FROM`; the published v5.0.0 → tip 14/14 twice, and a build that cannot find the old root fails all 14 |
| 12.3 | 65 F0.1: the cell metric ratchet — **already done** (`c539df7a`, 2026-10-06; re-measured by #771: 103/273 = 37.7 %) |

**Live validation**: the N4 image boots on libvirt **and** Cloud Hypervisor,
answers on its port, and its guest agent reports an address; the N5 test runs
against a root written by the published v5.0.0 binary.

### Sprint 13 — The node contract, part 1

N3, the rest of ADR-0042 step E (`WatchEvents`, `WatchOperation`,
`CancelOperation`, `Create/DeleteVolume`), ADR-0003's capability gate, N8 slice 5.

**Live validation**: through the socket only (no CLI), create a network, then a
VM attached to it on Cloud Hypervisor from the N4 image, watch the operation to
completion, and delete both; the guest's address is read from the guest.

**Release v5.1.0** after this sprint (minor: additive API).

### Sprint 14 — Socket activation and the network RPCs

ADR-0040 P5 (socket activation and the launcher), then N9 (ADR-0059 F6).

**Live validation**: inside a `delonix-vm-base:ubuntu-26.04` guest, a systemd
user socket starts `delonix-node-api` on first connection and the network RPCs
answer; nothing stays resident after the idle timeout.

### Sprint 15 — Implemented becomes proved (F1)

N11 and N12: checks and chaos scenarios only, no new functionality. Exit: no cell
is `supported` without a check that fails when the code is reverted.

**Live validation**: the promoted cells re-run against libvirt, CH and one
`proxmox-ve` appliance, one at a time; the cell metric is re-measured and
recorded.

### Sprint 16 — Failure paths (F2)

N13. **Live validation**: each new chaos scenario run on the tip with an isolated
root; each refusal check verified to fail with its fix reverted.

### Sprint 17 — Network capabilities

ADR-0065 P1–P6 (N21), ADR-0066 F1–F3 (N22), ADR-0047 spike.

**Live validation**: in one golden guest, two containers reach each other over
IPv6 with the namespace isolation and anti-spoof enforced on the wire (forged
packets counted dropped), and an L4 VIP spreads connections over two backends and
stops sending to one that fails its readiness check.

### Sprint 18 — VM hotplug and the reboot class

ADR-0068 phases 2–6 and ADR-0012 together (N23); N16.

**Live validation**: a libvirt guest and a Proxmox guest each gain a vCPU,
memory, a disk and a NIC while running, read from inside the guest; a change that
cannot be hot-plugged plans `Reboot`, not `Replace`.

**Release v5.2.0**.

### Sprint 19 — Kubernetes and the CRI

N6, N14, N15 (at least the `--cni` refusal by name), N25, the CRI half of N26.

**Live validation**: `critest` against a `delonix-vm-k8s:1.36` node, the number
published with its date; a pod resized live through the systemd branch; a
two-node cluster either `Ready` or refused by name.

### Sprint 20 — Ecosystem compatibility

The Compose and Docker API half of N26; N24 (ADR-0048 Ph3).

**Live validation**: a real Compose sample and a Testcontainers run against
`serve docker-api` inside a golden guest.

### Sprint 21 — Storage pools

ADR-0067 P1–P6 (N20). **Live validation**: in a golden guest with three extra
virtual disks, a btrfs, a ZFS and an LVM-thin pool each serve a container volume
and a VM disk, and the destroy guards refuse a pool with data.

### Sprint 22 — Proxmox depth

N17 (two appliances, 2 × 2 vCPU / 4 GiB, only when the preflight allows it),
N18, N28 if the owner keeps ADR-0049's slices, N19 only if the NetBox stack fits
the budget.

### Sprint 23 — Production pillars (F5) and the structural rest

N27, N10, ADR-0077 sensor measurement.

**Live validation**: the fuzzers run for a fixed budget with zero crashes; the
SLI exporter is scraped from a golden guest.

**Release v6.0.0** if N10 changes a public crate boundary; otherwise a minor.

### Sprint 24 — The three builds the owner chose over the recommendation

Each starts with the measurement its ADR names, and the build follows the
number; a spike that comes back NO-GO is recorded as such and the ADR's status
says so, rather than the build being forced.

| # | Item | First step | Live validation |
|---|---|---|---|
| 24.1 | ADR-0021 `kind: GitOpsSource` (opt-in, systemd timer, daemonless) | the security pass the ADR asks for (a pull source is untrusted input) | in a golden guest, a bare git repo served locally; a commit to it converges a stack on the next timer tick, and a commit that fails `stack plan` leaves the running stack untouched |
| 24.2 | ADR-0077 seccomp user-notification sensor | the per-syscall cost, measured with `bench.sh` against the same workload with and without the filter | a container under the sensor raises the decision event for a denied syscall and the cost stays inside the budget the ADR writes down |
| 24.3 | ADR-0004 rootless CRIU checkpoint/restore | GO/NO-GO in a golden guest with CRIU installed (rootless needs `CAP_CHECKPOINT_RESTORE`, kernel ≥ 5.9) | a container checkpointed and restored in the guest keeps its PID-1 state (a counter it holds in memory continues) |

---

## 6. Blocked on something this host cannot provide

| Item | Blocker |
|---|---|
| ADR-0039 OpenStack backend; ADR-0069 Pending 7 | a live cloud; the appliance asks 8 vCPU / 16 GiB |
| ADR-0036 phases 1–2 | a Windows host and a real Mac |
| 65 F0.3 nightly lab | the owner registers the `delonix-lab` runner |
| overlay VXLAN/WireGuard between hosts; multi-node CNI proved across hosts | two real nodes |
| GPU/CDI, TPM, hugepages, PCI passthrough cells | hardware |
| M06 fleet management; live migration; subnet-delete inside an IPAM rollback | decided out of scope (ADR-0010, ADR-0031, ADR-0063) |

---

## 7. How this plan stays honest

1. A sprint closes with its live validation **and** its numbers recorded in
   `docs/roadmap/13-improvements-traceability.md` in the same commit.
2. A validation that the lab budget refused is reported as refused, with the
   three preflight numbers — never as passed.
3. Every statement of this document that git later contradicts is fixed in the
   commit that contradicts it, the way §1.1 fixes the older ones.
