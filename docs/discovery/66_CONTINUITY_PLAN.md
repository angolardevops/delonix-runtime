# Continuity plan — one session owns the engine's open work

> **What this document is.** The consolidation of work that was spread over
> several parallel sessions into a single owner, with the order in which the open
> items are delivered. It names, for every programme already running, where it
> stopped and what the next step is.
>
> **What it is not.** New scope, and not a calendar. Every item here already has
> its decision taken (an accepted ADR, an owner decision in the maturity plan, or
> a `Pending` entry of an accepted ADR). Nothing below invents work, and no sprint
> promises a date — the sprints order the work by dependency, not by time.
>
> **What it replaces.** Nothing. `docs/discovery/65_PLANO_MATURIDADE.md` stays the
> plan by functional domain and `docs/roadmap/13-improvements-traceability.md`
> stays the matrix of record. This document is the *order of delivery* across the
> two, plus the items that live only in ADR `Pending` lists.

| Field | Value |
|---|---|
| Measured on | **2026-10-06** |
| Against | `origin/main` `21e1d0ed`, engine **4.5.0** |
| Latest published tag | **v4.5.0** — `main` carries **153 commits** nobody has released |
| Open pull requests | **0** |
| Sessions working on this repo | **0** (all stopped) |

---

## 1. Baseline, measured and not asserted

| Fact | Value | How it was measured |
|---|---|---|
| Cell metric of the capability catalogue | **111 / 281 applicable = 39.5 %** | counted in the committed `docs/providers/capability-matrix.md`: 111 `supported`, 96 `partial`, 73 `not-implemented`, 1 `unavailable-on-host` (228 `unsupported-by-provider` are out of the denominator) |
| The same metric in the maturity plan | 37 % (99/265), 2026-10-02, against `a0d683c6` | the plan's own baseline table |
| CLI battery | 800 `check` call sites in `scripts/e2e.sh` | `grep -c` |
| Last known FULL battery run | 1058 PASS / 2 FAIL / 12 SKIP on `48fa1365` | session record; the 2 failures were fixed by #687. **Stale**: `main` has moved far past that commit |
| Nightly lab | `.github/workflows/lab.yml` exists; the `delonix-lab` runner is **not registered** | the workflow has never run; this host is a DEV machine and the owner ruled it is not the lab |

**The battery number is the first thing Sprint 0 re-measures.** A plan whose
percentages come from a run against a commit that is dozens of merges behind is a
plan built on nothing.

---

## 2. What the parallel sessions left behind

Five sessions touched this repo and all of them have stopped. What each one
delivered is in `main`; what each one left open is listed here.

| Session | Delivered | Left open |
|---|---|---|
| **Kind catalog reassessment** | ADR-0069/0070/0071, strict manifests, the provider block as `type` + `spec`, policy hold for containers and Pods, versioned Secret rotation (#682…#700) | ADR-0069 `Pending` 1, 2 and 5; the two rows of the ADR-0070 table that have not moved; **uncommitted work in a worktree** (§3) |
| **Proxmox API interface assessment** | ADR-0049 slices, the Proxmox transport, the task ledger, ADR-0042 step C1/C2 (#659, #662) | ADR-0049 stays `Proposed` with 122/675 routes (15.1 %): the owner decides whether more slices are worth it |
| **PowerDNS integration** | ADR-0059 F5c, ADR-0063, ADR-0064 (#654, #656) | ADR-0063 D1 and D2; ADR-0064 D6 (decided, not implemented) |
| **Documentation reopening** | the contributor handbook (#601) | per-N3-cell user pages (maturity plan F5) |
| **Master initialization** | the seven application templates and the generator | PR #634 was closed as superseded; ADR-0061 stays `Proposed` although the templates shipped |

**Lost work, found while measuring.** The `auditoria-cobertura` branch — the CLI
execution coverage that went from 25 % to ~63 % — **no longer exists**: not local,
not on `origin`, not in any reflog. Item F0.2 of the maturity plan says to
«integrate that branch first», and that premise is void. F0.2 is now *rebuild*,
not *integrate*, and this is the one place in this plan where work has to be
redone rather than finished.

---

## 3. Uncommitted work, preserved

The worktree `.worktrees/delonix-runtime/vm-policy-hold` carries **271 insertions
across 7 files, never committed** (last written 2026-10-05 22:44, base
`320a5ee7`). It is ADR-0069 `Pending` item 1: the policy hold for VMs and system
containers — `VmConfig::policy_hold`, `VmBackend::holds_at_boot` (a backend that
cannot hold a guest closed at boot refuses by name instead of creating it open),
`guest_policy_targets`, `release_vm_holds` and `release_system_container_holds`,
with the annotation written LAST so a failure to open a direction leaves the guest
marked closed for the next apply.

It is good work and it was one `git checkout` away from being gone, so it is now
also a patch at
`.worktrees/_backup-nao-commitado-2026-10-06/vm-policy-hold-adr0069-d6-wip.patch`.
The working tree was left untouched. It has **not been compiled or gated** by this
session; Sprint 2 finishes it.

---

## 4. Sprints, in delivery order

The order follows three rules: measure before deciding, publish what is already
measured before building on top of it, and build the foundation of evidence before
claiming maturity.

### Sprint 0 — Re-measure and clean up (no new code)

| # | Item | Exit criterion |
|---|---|---|
| 0.1 | Full battery, chaos harness and `bench_gate.py` on `21e1d0ed`, with an isolated state root | PASS/FAIL/SKIP counts published with the SHA; every SKIP has a written reason |
| 0.2 | Re-measure the cell metric and record it next to the battery numbers | one number, one date, one SHA |
| 0.3 | Clean the lab leftovers: the paused `opnsense-f3d` appliance, the `pve-lab-475/476` VMs, the `audvm` VM, the orphan IPAM reservation `10.250.7.0/24`, the `dlxs6` lab token (revoke), the `naas/audit-fase0` remote branch (the seven NaaS sessions are closed) | `git ls-remote` shows only `main`; the token is revoked; the VMs are accounted for |
| 0.4 | Record that `auditoria-cobertura` is lost and rewrite F0.2 of the maturity plan from «integrate» to «rebuild» | the plan no longer depends on a branch that does not exist |

### Sprint 0 — delivered 2026-10-06, measured

| Measurement | Result |
|---|---|
| Chaos harness | **53 PASS / 0 FAIL / 1 SKIP**, rc=0. The skip is `truenas-destroy`, with no `DELONIX_CHAOS_TRUENAS_URL/USER/PASS` |
| Battery, clean single run on `2a62608d` | **1067 PASS / 13 FAIL / 13 SKIP** — the thirteen failures were one cascade (below) |
| Battery, same commit with the cascade's cause removed | **1081 PASS / 0 FAIL / 13 SKIP**, rc=0 |
| Cell metric of the capability catalogue | **111 / 281 applicable = 39.5 %**, identical to the committed `capability-matrix.md` — the matrix in git is current |
| Performance gate | **refused twice, then passed**: `delonix` **84 ms** against the machine's recorded 87 ms (×0.97, dispersion 1.9×), docker 216 ms (×0.96), podman 258 ms (×0.92). The two refusals were the gate working — the second came at load 1.70 with the line dispersed 27.3× against a ceiling of 3×, which is I/O contention that `load(1m)` does not show |
| Free disk | 98 % used (19 G free) → **90 % (90 G free)** |

**The thirteen failures were not the engine's.** The backup section's setup pulled
a *second* image (`alpine:latest`) that the battery's image guard does not cover,
with its output discarded. On this host the outbound link is slow: the image
reached the store only after the section, the container was never created, and
thirteen checks failed with `no such container` — thirteen steps past the
problem. The store of that run had `alpine:3.19` and no `alpine:latest`, which is
the whole cause. Fixed by using `$IMG` and making the creation a named check.

**And the first run of all was void, by my own hand.** It reported sixteen
failures, three of them in the node API. Two instances of the battery had run in
parallel over the same `DELONIX_ROOT` — visible as two distinct `PFX` in one log,
the same check appearing once PASS and once FAIL, and node-API checks under the
`=== image ===` section. The three node-API failures came from that: the
scenarios used a fixed idempotency key, so the second run was answered with the
first run's operation — exactly what the contract promises — over a network
`cleanup()` had just removed. Measured in isolation: 20 of 20 fail on a reused
root, and three consecutive runs pass after the fix. Each battery run now takes a
root of its own and aborts if another is live.

**Of the four items, three are closed and one is not:**

- 0.1 **done** — the numbers above, with the SHA, and every SKIP with a reason.
- 0.2 **done** — 39.5 %, and the committed matrix needed no correction.
- 0.3 **partly done, and the rest deliberately not**: `naas/audit-fase0` and the
  `audvm` VM were already gone; `opnsense-f3d`, `pve-lab-475/476` and `pve-lab`
  are **kept**, because Sprints 6 and 7 need them. The `dlxs6` lab token is still
  to revoke — it needs the Proxmox lab up, which Sprint 6 brings. `hadata`, `pbs`
  and `labdata` (53 G) are the live disks of existing VMs and were not touched.
- 0.4 **done** — F0.2 of the maturity plan corrected.

**What Sprint 0 says about Sprint 1**: on this commit the battery is green once
the environment's own failure is out of the way, the chaos harness is green, and
the engine is 3 % faster than this machine's recorded baseline. Sprint 1 may cut
the tag.

**And it took three attempts to get one performance verdict on this host**, which
is the argument for F0.3 in one line: a measurement that depends on the machine
being quiet cannot live on a developer's machine. The gate refusing is what kept
two unusable runs out of the record.

### Sprint 1 — Release v5.0.0

153 commits are unpublished, and carrying them is the largest risk in the repo
today. The next release **must be a major**: ADR-0062 P2 changed the default so a
container runs as the image's `USER`, which breaks the `container run` contract.

| # | Item | Exit criterion |
|---|---|---|
| 1.1 | `scripts/release_verify.py` green; regenerate `docs/codigos.html`, the dev docs site and the published schema | no gate red |
| 1.2 | Bump to `5.0.0` with `docs/releases/v5.0.0.md`, naming the breaking change | `version_gate.py` passes |
| 1.3 | Tag, then `gh workflow run release.yml -f tag=v5.0.0` — pushing the tag publishes nothing by design | the release exists with its assets |
| 1.4 | Validate the published assets as a real user would: signature, SBOM, `install.sh` from a clean host | an install from the release works |

### Sprint 2 — Finish what is already in flight

| # | Item | Depends on |
|---|---|---|
| 2.1 | ADR-0069 `Pending` 1: finish the VM/system-container policy hold (§3), add the holder-respawn test for a governed Pod, add `net ingress\|egress <pod>` to the CLI | §3 patch |
| 2.2 | ADR-0069 `Pending` 2: `ResourceKey` as `(kind, scope, name)` through `reconcile.rs` and the destroy path, so two homonyms in different namespaces stop being one plan entry | — |
| 2.3 | **DONE 2026-10-06, by measuring instead of moving.** Both rows are closed by **ADR-0072** (the perimeter filter stays on `NetworkGateway`: the `OwnerMark` is per declaring record and never rewritten, so a policy document would duplicate every rule on a live appliance and orphan the originals) and **ADR-0073** (`SystemContainer` stays its own Kind: `Namespaced::Never` against the VM's `Always`, an OCI image against a disk, and ~28 verbs to refuse). The D1 part of both rows was already satisfied by ADR-0071. The owner decides on the two ADRs; either way the row no longer reads as pending work | — |
| 2.4 | ADR-0064 D6: clean the DNS records the node leaves behind | — |
| 2.5 | ADR-0063 D1 and D2: external IPAM controllers, and subnets changed in place | — |
| 2.6 | **DONE 2026-10-06.** The owner accepted nine: ADR-0020 and ADR-0061 (work finished), ADR-0040 and ADR-0041 (structural — P0–P4 built, P5 is Sprint 7; this unblocks M01's `capability discovery`, whose only blocker was the ADR not being accepted), and ADR-0049, 0055, 0063, 0064, 0067 (phases built, phases named open). ADR-0065, 0066 and 0068 stay `Proposed`: nothing is implemented. The eleven with no work at all still need their own decision. Two status lines were not stale but WRONG and were corrected, and a gate now refuses both an ADR that contradicts itself and an index row that disagrees with its document — seven rows did, two of them for twelve days | — |

### Sprint 3 — F0 of the maturity plan: the foundation of evidence

Nothing after this sprint can be trusted without it.

| # | Item | Note |
|---|---|---|
| 3.1 | F0.1 — cell metric printed by `delonix provider matrix`, with a two-way ratchet | the `lang_ratchet` pattern |
| 3.2 | F0.2 — **rebuild** the CLI execution ratchet and the coverage (§2) | the branch is lost |
| 3.3 | F0.3 — register the `delonix-lab` self-hosted runner and make the nightly actually run | **owner's step**; blocks the evidence of F1 and F2 |
| 3.4 | F0.4 — minimal test guest image (< 100 MiB) that boots on libvirt **and** Cloud Hypervisor, runs cloud-init and answers on a port | blocks every guest-side cell |
| 3.5 | F0.5 — re-measure CRI with `critest` v1.36 against the current engine | the published 79/103 is from v0.63.1 |
| 3.6 | F0.6 — state upgrade test: a root written by tag N-1 opened by N with no loss | run it in every release from v5.0.0 on |

### Sprint 4 — F1: turn implemented into proved (39.5 % → ≥ 60 %)

No new functionality: checks, chaos scenarios and assertions only, per the F1
table of the maturity plan (VM compute, guest, containers, protection, mobility,
network, firewall on both linux and proxmox, inventory, access). Exit: no cell is
`supported` without a check that fails when the code is reverted. **Depends on
3.3 and 3.4.**

### Sprint 5 — F2: the failure paths (N2 → N3)

A chaos scenario or refusal check per N2 cell, exit-code classes on every failure
path, an event per failure, `WatchEvents` served on the node contract (ADR-0042
F2.4), and async operation handles generalised from the Proxmox task ledger.

### Sprint 6 — The four approved new capabilities, each with its ADR already written

Delivered in this order, cheapest and most-blocking first:

| Order | Item | ADR | Why here |
|---|---|---|---|
| 6.1 | Storage pools P1–P6 (helper, btrfs, ZFS, LVM-thin, VM disks in pools, destroy guards) | ADR-0067 (P0 shipped) | needs root and spare disks in a lab VM — so it needs 3.3 |
| 6.2 | IPv6 in the dataplane (`table inet`, IPAM v6, DNS AAAA; today's refusal becomes an opt-out) | ADR-0065 | nothing implemented |
| 6.3 | L4 load balancer (nftables vmap/numgen plus health check) | ADR-0066 | nothing implemented |
| 6.4 | VM hotplug of CPU, memory, disk and NIC | ADR-0068 | the most expensive; the plan says measure demand first |

### Sprint 7 — Structural: finish ADR-0040 and unblock the node contract

| # | Item | Unblocks |
|---|---|---|
| 7.1 | ADR-0040 P5: socket activation and the launcher | ADR-0059 F6 |
| 7.2 | ADR-0059 F6: the network RPCs on the node contract — **deferred by the owner on 2026-10-03** until P5 | the NaaS programme's last phase |
| 7.3 | ADR-0042: the rest of step E (`ConnectContainer`/`DisconnectContainer` with the container wave, `WatchOperation`, `CancelOperation`) | a client that drives the node without the CLI |

### Sprint 8 — F4: ecosystem compatibility

CRI failures classified as bugs in 3.5; the Compose keys that cover 95 % of a real
sample (49 are missing of 89); the 11 Docker API refusals re-read against what
Testcontainers and `docker compose` actually call.

### Sprint 9 — F5: the cross-cutting production pillars

Continuous fuzzing of the untrusted parsers; the engine's own SLIs (M08 is
`NOT_STARTED` — `slo` has zero occurrences in the CLI); `observe`/`diagnose`
(M07); splitting `spawn()` (~405 lines) by phase with a test per phase; a user
page per N3 cell.

---

## 5. Out of scope, with the reason written

A cell or an item removed by decision keeps its reason; it never silently
disappears.

| Item | Why it is not in a sprint |
|---|---|
| Fleet management (`node`/`cordon`/`drain`, M06) | ADR-0010 **rejected** the remote management API. It needs a successor ADR naming a concrete consumer — an owner decision, not work |
| OpenStack backend (ADR-0039, ADR-0069 `Pending` 7) | needs a lab cloud that does not exist here. `provider.type: openstack` is refused at load, and that is the honest state |
| Live VM migration (ADR-0031) | measured NO-GO: it needs shared VM image storage or a privileged libvirt daemon, neither of which is a named need |
| macOS and Windows (ADR-0036) | decided as a VM launcher, blocked on a real Mac for the GO/NO-GO spike |
| Conformance suites OCI/CRI/CNI/CSI (ADR-0069 `Pending` 9) | no conformance is claimed today; claiming it is a programme of its own |
| ADR-0003, 0004, 0012, 0021, 0026, 0033, 0034, 0055, 0057 | `Proposed` with no owner decision. Each needs an accept/reject before it can be scheduled |
| Pod standalone: `emptyDir`, `network: host\|none`, `requests`, probes, init containers (ADR-0069 `Pending` 5) | the strict load refuses the unsupported ones today, which is correct; implementing them is new scope and needs a decision |
| CNI `VERSION`/`GC` and persisted netconf for `DEL` (ADR-0069 `Pending` 8) | small, but it is new surface on a contract served to a kubelet; it waits for the container wave of Sprint 7 |

---

## 6. Other repositories in the workspace

A session opened in `delonix-runtime` works only on `delonix-runtime` (owner's
rule, 2026-09-16). What was found elsewhere is reported, not scheduled here:

- **`delonix-meet`** has **6 open pull requests** (#232, #235, #236, #238, #242,
  #243) and a session of its own running the same consolidation exercise. That is
  where its plan belongs.
- **`delonix-paas`, `delonix-console`, `delonix-admin`, `delonix-deploy`,
  `tunnel`** do not resolve under the `angolardevops` GitHub account from here, so
  their state could not be read. The NaaS programme's item **C1 lives in
  `delonix-paas`** and is still open there.

---

## 7. How this plan stays honest

1. **One owner.** From 2026-10-06 the open work of this repo is driven from a
   single session. A second session on this repo takes a worktree and a branch of
   its own, as always, but the order of delivery comes from here.
2. **Every sprint closes with a measurement**, recorded in
   `docs/roadmap/13-improvements-traceability.md` in the same commit that changes
   it — never in a transcript.
3. **An item without a measured exit criterion does not close.** A sprint that
   reports «done» without the number next to it has not been delivered.
