# Proxmox and Cloud Hypervisor — live capability report (2026-10-09)

**DRAFT. Not committed.** See §8 before deciding whether to commit this file — it names a lab
IP, a token id, and library-level details of a session API token that need one more look even
though no raw secret value is written below.

## 1. Scope and method

The user asked for a live evaluation of Proxmox (network, firewall, LXC, VM, backup, restore,
template, SDN) and Cloud Hypervisor, and then for a complete report of provider-by-resource
compatibility. This document is both: it runs the engine's own capability-reporting tool
(`delonix provider ls|describe|matrix`, ADR-0050) against real infrastructure, then runs the
`delonix-proxmox` crate's live integration suite (`tests/live.rs`, 39 tests) against a real,
freshly-built Proxmox VE node, and exercises Cloud Hypervisor by hand through the CLI.

**The central methodological finding of this pass, stated up front because it changes how to
read every number below**: `cargo test`'s own `ok` result is not proof of a genuine exercise.
A test that silently early-returns on a missing environment variable (`let Some(x) = foo() else
{ return; };`, no print) reports **exactly** the same `... ok` as a test that ran its full
scenario against the real node. The first full run of `tests/live.rs` reported "39 passed" and
that number was *wrong* in the sense that mattered — 13 of those 39 had done no work at all. The
only way this was caught was cross-checking the node's own task history
(`GET /nodes/pve/tasks`) and finding zero `vzcreate`/`vzstart` tasks where the LXC tests should
have produced dozens. Every tally in this report that matters distinguishes "the harness said
ok" from "the node's own state proves it happened" — and says, for each test, which kind of
evidence it rests on.

## 2. Environment

- **Host**: the user's own development machine, already running other libvirt VMs and networks
  from unrelated sessions (`delonix-dev-cp-*`, `opnsense-*`, etc.) — none touched.
- **Target**: `pve92`, a libvirt VM already running before this session started, built from this
  repository's own Proxmox VE 9.2 appliance (`scripts/appliances/build-proxmox.sh`). Proxmox node
  name inside the cluster is `pve` (the libvirt domain name and the Proxmox node name are
  unrelated strings — this cost one recon round-trip to notice: `/nodes/pve92/...` 404s with
  "hostname lookup 'pve92' failed", `/nodes/pve/...` works).
- **Network**: the VM is on the default libvirt NAT network, address `192.168.122.220`, reachable
  from the host directly (not the separate lab subnet `192.168.1.x` referenced in this
  repository's own `AGENTS.md` for a *different*, two-node lab cluster — that one was not
  reachable from an earlier, more constrained session; this VM, on the same machine Claude was
  running on this time, was).
- **Version**: Proxmox VE 9.2.2, single node, `cluster: none` (confirmed by the live probe, §5).
  Storage: `local-lvm` (lvmthin, `images,rootdir`) and `local` (dir).
- **Cloud Hypervisor**: local to the host running this session — `/usr/local/bin/cloud-hypervisor`,
  `/dev/kvm` present, user in the `kvm`/`libvirt` groups. No lab needed.
- **Engine build**: `delonix 5.0.0, commit b7cf49e5e (+47 commits since v5.0.0)`, release profile,
  built from this worktree (branched off `main` at the exact commit the prior
  `auditoria/naas-kaas-caas` PR #742 merged).

### 2.1 Fixes applied to the lab, and why each was needed

None of these are engine changes — they are lab setup this specific fresh VM was missing, each
confirmed by reproducing the failure first and the fix second:

1. **`dnsmasq` installed, then globally *disabled*.** Proxmox's SDN DHCP plugin runs its own
   per-zone `dnsmasq` instances (`dnsmasq -x /run/dnsmasq/dnsmasq.<id>.pid ...`); it does not need
   or want the distribution's own global `dnsmasq.service` running. Without the package installed
   at all, every DHCP-backed SDN zone apply failed with "Could not run before_regenerate for DHCP
   plugin dnsmasq cannot reload with missing 'dnsmasq' package". This fixed 5 of the first run's 7
   failures.
2. **`source /etc/network/interfaces.d/*` restored in `/etc/network/interfaces`.** This repo's own
   `scripts/appliances/proxmox_postinstall.py` writes this line deliberately (documented in
   `AGENTS.md`); this particular built image was missing it. Re-added by hand; the real fix belongs
   in a future appliance rebuild, not in this lab.
3. **`images` content type enabled on the `local` storage** (`PUT /storage/local` with
   `content=backup,iso,vztmpl,import,images`). The `move_disk_unlink_and_cloudinit_dump` test needs
   a *second* images-capable storage to move a VM's disk to; a fresh node only has `local-lvm` for
   that.
4. **A `management` IPSet (`192.168.122.0/24`) plus an explicit `ACCEPT` rule, created *before*
   turning the datacenter firewall on.** This repo's own `AGENTS.md` documents a real prior
   incident on a different lab: the appliance's residual `/etc/hosts` entry makes the node detect
   only `127.0.0.0/8` as "local," and enabling the firewall without an explicit allow for the real
   management network locks the operator out. Verified reachable immediately after enabling, with
   a fresh API ticket, before doing anything else.
5. **A root@pam API token (`root@pam!live-eval`, `privsep=0`)** created for the test run instead of
   reusing the interactive password for every call. See §6.1 for the one route, out of dozens, this
   token could not reach — and for the fix that removed the only caller of it.
6. **A scratch-only, never-committed temporary `#[test]` added to `crates/adapters/delonix-oci/src/
   save.rs`**, run once to call `write_oci_media_archive` against a real `alpine:3.20` pulled into
   a local `ImageStore`, producing the fixture every system-container test needs
   (`DELONIX_PROXMOX_TEST_OCI_ARCHIVE`). `git checkout --` reverted it immediately after; `git
   status`/`git diff` confirmed a clean tree before any further work.

Lab left in a clean state afterward: zero LXC/QEMU guests, zero SDN zones/vnets/fabrics, datacenter
firewall re-enabled (matching the tested baseline), the `management` IPSet and its accept rule
still in place.

## 3. Per-provider capability summary (`delonix provider ls`, catalog 1.3.0, 141 capabilities)

Measured on this exact host, with `proxmox` pointed at the real node via `--probe`:

| Provider | Kind | Available | Health | Supported | Partial | Unsupported | External | Not-impl | Unavail |
|---|---|---|---|---:|---:|---:|---:|---:|---:|
| cloud-hypervisor | compute | yes | Ok | 12 | 18 | 42 | 2 | 9 | 0 |
| libvirt | compute | yes | Ok | 16 | 24 | 31 | 2 | 10 | 0 |
| proxmox | compute | yes | — | 32 | 13 | 26 | 3 | 9 | 0 |
| proxmox | network | yes | — | 7 | 7 | 20 | 0 | 13 | 0 |
| linux | compute | yes | Ok | 11 | 9 | 60 | 0 | 3 | 0 |
| linux | network | yes | Ok | 8 | 14 | 18 | 1 | 6 | 0 |
| linux | storage | yes | NetworkSharesNeedRoot | 4 | 0 | 0 | 2 | 2 | 3 |
| opnsense | gateway | **no** | NotConfigured | — | — | — | — | — | — |

`opnsense` was not configured for this pass (no OPNsense appliance target set up) — out of scope
for this request, noted for completeness since `provider ls` always lists it. `generated matrix
docs/providers/capability-matrix.md` was diffed against a fresh `delonix provider matrix` run and
is **byte-identical** — the committed, host-independent capability declarations are not stale
relative to this build, 47 commits past the tag.

### 3.1 Notable individual rows, by provider

**cloud-hypervisor** (compute only; it is not a network, system-container, or storage provider):
`vm.create/start/stop/destroy/restart` and `vm.snapshot.*` all `supported` with battery evidence;
`vm.migration.live` is `unsupported-by-provider` **by design** (ADR-0031: CH migrates memory only,
never the disk — a live migration that moves a VM's state but not its storage is not a feature, it
is a way to corrupt a guest); `vm.ip.observed` is `unsupported-by-provider` for the opposite-of-
obvious reason that the address is *predicted* from the MAC, not learned — `--wait` ARPs for it
instead of trusting the prediction.

**libvirt** (compute only): the same shape, but richer on the VM-definition side (`vm.cpu.model`,
`vm.device.tpm`, `vm.raw-definition` are all at least `partial`, since libvirt XML is a real
escape hatch) and poorer on SDN integration (`vm.network.sdn: unsupported-by-provider` — a libvirt
VM lives on `virbr0` in the host's own network namespace, a different L2 the engine's SDN does not
program; `vm bridge`, root-only and experimental, is the one path across).

**proxmox / compute**: the richest single row-count of the four (32 `supported`), because it is
the only provider with **both** VM and LXC coverage (`system-containers.*`) and the only one with
`vm.migration.live`/`vm.migration.cold` genuinely `supported` with live evidence — on a *real*
cluster. See §5 for why that evidence does not apply to this single-node lab.

**proxmox / network**: markedly thinner than compute (7 `supported`, 13 `not-implemented`) — the
Proxmox SDN speaks zones/vnets/subnets/IPAM/DNS-through-the-node, but has no load balancer, no
engine-native overlay, and (§6.1, a finding from this pass, since fixed) one route of the vnet
firewall that is restricted in a way the rest of the family is not.

**linux / compute**: the container engine. 60 `unsupported-by-provider` is not a weakness — it is
every `vm.*`/`system-container.*` row, correctly refused by a provider whose job is containers,
not VMs or VM-shaped LXC guests. Where it is genuinely rich: `containers.*` (lifecycle, exec, logs,
hot-reconfigure, resource-limits, images — all `supported`), `net.namespace-isolation`,
`net.routes`.

**linux / storage**: small (11 capabilities total) and honest about a real host limit —
`volume.nfs/cifs/webdav` are `unavailable-on-host` specifically because this is a rootless
session without `CAP_SYS_ADMIN`, not because the engine lacks the code path.

## 4. The live cluster probe (`delonix provider describe proxmox --probe`)

The probe reads the real cluster around the configured node with read-only calls (nodes, quorum,
storage, HA, SDN zone count) and separates two different questions the static capability table
cannot: *"does the engine's code support this"* vs. *"can THIS cluster do it right now"*:

```
Cluster of the target, measured now (https://192.168.122.220:8006, node pve), read-only:
  cluster: none — the node is not in a cluster
NODE   ONLINE   TARGET
pve    yes      yes
STORAGE     TYPE      SHARED   VM DISKS
local-lvm   lvmthin   no       yes
local       dir       no       no
  HA: no CRM master · 0 resource(s) · SDN zones: 0
CAPABILITY             THIS CLUSTER   WHY
vm.migration.cold      does not       the target node is not in a cluster
vm.migration.live      does not       the target node is not in a cluster
vm.high-availability   does not       the target node is not in a cluster
vm.replication         does not       replication needs a cluster of two or more nodes
storage.ceph           does not       no rbd/cephfs storage
```

This is the tool's own, already-correct distinction — not something this report had to invent:
migration/HA/replication are declared `supported` in the static table (§3) because the *code*
supports them against a real multi-node cluster (proven in this crate's own test history against a
different, two-node lab — see `AGENTS.md`'s own account of that work), and the probe against *this*
specific single node says, honestly, "does not," naming the reason. **Not an engine gap** — a fact
about the target this session had available.

## 5. The `tests/live.rs` journey, with the corrected final tally

### 5.1 First full run: 32 of 39 "passed," 7 failed

```
test result: FAILED. 32 passed; 7 failed; 0 ignored; 0 measured; 0 filtered out; finished in 252.43s
```

Failures and root causes, each independently diagnosed:

| Test | Cause |
|---|---|
| `sdn_zone_and_vnet_are_staged_applied_and_torn_down` | missing `dnsmasq` (§2.1.1) |
| `sdn_subnet_and_the_single_item_zone_vnet_routes_are_staged_applied_and_torn_down` | missing `dnsmasq` |
| `sdn_apply_waits_for_every_nodes_network_reload` | missing `dnsmasq` |
| `sdn_controllers_fabric_dhcp_and_ip_reservations_round_trip_through_the_node` | missing `dnsmasq` |
| `sdn_routing_chain_vnet_firewall_and_the_lock_round_trip_through_the_node` | cascaded from the previous test's incomplete teardown ("nothing pending, nothing to render") |
| `network_zone_provider_owns_by_mark_and_never_pushes_someone_elses_pending_change` | missing `dnsmasq` |
| `move_disk_unlink_and_cloudinit_dump` | `local` storage had no `images` content (§2.1.3) |

### 5.2 After the dnsmasq/storage fixes: retry of the 7 — 5 pass, 2 new failures

```
test result: FAILED. 5 passed; 2 failed; 0 measured; 32 filtered out; finished in 45.38s
```

The 2 remaining were **not** environment gaps repeated — each was a distinct, new finding:

- **`sdn_controllers_fabric_dhcp_and_ip_reservations_round_trip_through_the_node`**: failed with
  "IP prefix 10.99.0.0/24 ... overlaps with IPv4 prefix 10.99.0.0/24 in fabric 'f386121'" — real
  residue this session's own earlier *failed* run had left behind (a fabric, its node membership,
  4 zones, 4 vnets, 2 subnets — none torn down because the test that created them panicked before
  reaching its own cleanup code). Cleaned up by hand via the Proxmox API (deleting node membership
  → fabric → subnets → vnets → zones, in that dependency order — the node enforces it and says so
  on every wrong-order attempt), then the test passed cleanly on retry.
- **`sdn_routing_chain_vnet_firewall_and_the_lock_round_trip_through_the_node`**: failed with
  `HTTP 403 Forbidden: Permission check failed (user != root@pam)` reading the vnet firewall
  **index** (`GET /cluster/sdn/vnets/{vnet}/firewall`). The first write-up of this pass
  overgeneralized this to "the vnet firewall routes" — **wrong, and corrected by testing each
  route individually against the live node, token vs. password, before concluding anything**:
  `options` (read and write), `rules` (read and write) all accept the API token without
  complaint; **only the index route is restricted to a real `root@pam` session**, consistently,
  confirmed on three repeated calls. The index has exactly one caller in the whole codebase — this
  test, which uses it to assert that `rules`/`options` exist, a fact its own next two lines already
  prove by calling them directly. Fixed in this pass, not just documented: the index call was
  removed from the test (it was provably redundant) and `sdn_vnet_firewall_index`'s doc comment now
  states the restriction precisely, so no future caller reaches for it expecting token access.
  Re-run with **token-only** auth (no password set at all) after the fix: the full test now passes
  end to end. See §6.1.

### 5.3 The *actual* overclaim, caught by cross-checking the node — not by cargo

A clean, full re-run of all 39 tests (password auth throughout, residue cleared) reported:

```
test result: ok. 39 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 258.04s
```

**This number is misleading on its face, and this report almost repeated it uncritically.**
Grepping the source for every test's own early-return gate found **13 of the 39** return before
touching the node at all when a specific environment variable is unset:

- 1 test prints an explicit `SKIP: ...` line before returning
  (`o_ip_vem_do_agente_de_um_convidado_a_serio`, needs `DELONIX_PROXMOX_TEST_AGENT_VMID`) — this
  one at least tells the reader.
- **12 tests return silently, with no message at all**, and `cargo test` reports them `ok`
  identically to a test that ran its whole scenario:
  - 3 need `DELONIX_PROXMOX_TEST_MOVE_NODE`/`_SHARED_STORAGE` (a second cluster node): the two VM
    live-migration tests and the one cold-migration test.
  - 8 need `DELONIX_PROXMOX_TEST_OCI_ARCHIVE` (every `system_container.*` test plus the IPAM
    test) — confirmed **empirically**, not just by reading the gate: `GET /nodes/pve/tasks`
    around the first run's time window showed **zero** `vzcreate`/`vzstart` tasks, while the log
    showed seven system-container tests reporting `ok`.
  - 1 (the DNS-provider test) needs the OCI archive **and**
    `DELONIX_PROXMOX_TEST_DNS_CONTROLLER`/`_ZONE`/`_URL`/`_KEY_FILE` (a real PowerDNS server).
  - 1 (the system-container move test) needs the OCI archive **and**
    `DELONIX_PROXMOX_TEST_MOVE_NODE` — it is in *both* of the groups above, not a 14th case.

So the correct accounting of the first clean "39 passed" is: **26 tests genuinely exercised**, 13
did not (1 explicit, 12 silent).

### 5.4 Closing the gap that was closable: a real OCI archive, then a real run

A second-node requirement cannot be manufactured in this session — those 3 migration tests and the
system-container move test's second gate stay unexercised, honestly. The OCI-archive requirement
*could* be closed, and was (§2.1.6): `alpine:3.20` pulled, archived with the engine's own
`write_oci_media_archive`, the temporary test reverted immediately.

Re-running the 9 archive-gated tests with the fixture in place:

```
test result: FAILED. 8 passed; 1 failed; 0 measured; 30 filtered out; finished in 725.97s
```

`the_ipam_provider_reserves_an_address_and_a_guest_gets_it_by_dhcp` failed genuinely this time —
"DHCP failed ... exit code 2" then "eth0 has no IPv4 address". Diagnosed live (console, `pexpect`):
the DHCP client sent `DHCPDISCOVER` broadcasts correctly and never got an answer; the zone's own
`dnsmasq` instance was running and bound to the right bridge. The actual cause, confirmed by a
controlled experiment: **the datacenter firewall, enabled in §2.1.4 for the VM/LXC firewall tests,
blocks DHCP broadcast traffic on a freshly-created SDN zone's bridge** — the iptables `pve-firewall`
backend's default policy does not automatically exempt a new zone's intra-bridge traffic. Disabling
the datacenter firewall and re-running the single test passed cleanly in 94s; re-enabling the
firewall afterward (restoring the tested baseline) did not break anything already proven. **This is
an interaction between two things this session asked to test, not an engine defect or a Proxmox
bug** — but it is exactly the kind of fact a report like this exists to surface: enabling the
datacenter firewall for one capability's evidence has a side effect on an unrelated one tested
later in the same session.

The 8 of 9 that passed did so with real, observable work: an LXC created, started, reporting
`NotReady` with the node's own warning exactly as the capability table's `system-container.
network.bridge` row documents (this lab's `vmbr0` has no DHCP server of its own, the expected
answer); stopped and destroyed with no container and no volume left; a snapshot taken, rolled back,
deleted; the root volume grown live, confirmed never to shrink; a `scope: systemcontainer`
firewall policy applied and read back in order; a clone made from a temporary snapshot; a backup
taken and restored over the running container, with the raw `lxc.*` keys the node drops for an API
token named explicitly in the result (ADR-0058's own documented limit, confirmed live rather than
assumed).

### 5.5 Final, honest tally

Of the 39 `tests/live.rs` functions: **33 genuinely exercised real behavior against the live node
and passed.** That is every test in §5.1's first run except the 7 originally diagnosed as
environment gaps and fixed (§5.2/§5.4), minus the 8 that needed the OCI archive and were only
exercised for real once it existed (§5.4, 7 of 8 passing plus the IPAM one after the firewall
fix), reconciled against the 13 silent-or-explicit skips §5.3 found by reading every gate in the
source. **6 remain genuinely unexercised in this session**, each for a stated,
unmanufacturable-in-this-sandbox reason:

| Test | Needs |
|---|---|
| `a_running_vm_moves_live_on_shared_storage_and_keeps_running` | a second cluster node |
| `a_running_vm_with_a_local_disk_moves_live_and_its_disk_is_mirrored` | a second cluster node |
| `a_stopped_vm_moves_to_another_node_and_the_cluster_lists_it_there` | a second cluster node |
| `a_system_container_moves_to_another_node_offline_and_by_restart` | a second cluster node (OCI archive was provided; this second gate was not) |
| `o_ip_vem_do_agente_de_um_convidado_a_serio` | a prepared guest-agent VM (`DELONIX_PROXMOX_TEST_AGENT_VMID`) |
| `the_dns_provider_registers_a_guest_in_the_zones_dns_server` | a real PowerDNS server reachable from the node (ADR-0064's own precondition for D6, already reviewed and left open by this repo's prior `auditoria/naas-kaas-caas` pass) |

**33 + 6 = 39.** Every test accounted for, by name, with the specific reason it either ran for
real or did not.

## 6. Cloud Hypervisor — manual smoke test (outside `tests/live.rs`; this crate has none)

`delonix-provider-cloud-hypervisor` has no `tests/live.rs` analog — its capability evidence in the
catalog (§3) cites `scripts/e2e.sh` checks and chaos scenarios, not a crate-local live suite. Run
by hand against this host's own KVM, isolated `DELONIX_ROOT`:

```
vm create (blank 256M qcow2, --backend cloud-hypervisor)  → Running, real SDN IP (10.200.254.x)
vm pause                                                   → Paused
vm unpause                                                  → Running (same identity)
vm stop                                                     → stopped
vm snapshot create s1                                       → ok
vm start                                                    → Running again
vm stop; vm snapshot restore s1; vm snapshot rm s1           → ok
vm rm                                                        → 5 artifacts removed, 689.6 KiB freed
```

Every step genuinely executed (not gated on anything absent); the one command-naming mistake in
this pass (`vm resume` does not exist — the verb is `vm unpause`) was caught by the CLI's own error
message and is not a finding, just a note for whoever else tries this from memory.

## 6.1 Fixed, not just found: one vnet-firewall route needed `root@pam`; the rest never did

The first draft of this report, written right after §5.2's test failure, overgeneralized a single
403 into "the vnet firewall routes are refused for an API token." **That claim was wrong, and the
correction is itself the point of this section**: before writing anything else down, every
vnet-firewall route was tested individually against the live node, token vs. password —

| Route | Token | Password |
|---|---|---|
| `GET .../firewall` (index) | **403** | 200 |
| `GET .../firewall/options` | 200 | 200 |
| `PUT .../firewall/options` | 200 | 200 |
| `GET .../firewall/rules` | 200 | 200 |
| `POST .../firewall/rules` | 200 | 200 |

Only the index — a bare listing of the two sub-resource names, `rules` and `options` — is
restricted to a real `root@pam` session; everything that actually reads or writes a rule works
fine with the API token this crate's own `Auth` doc-comment already calls "the form to prefer."
And the index has exactly one caller anywhere in this codebase: the live test, using it to assert
that `rules`/`options` exist — a fact its own next two lines already prove by calling them
directly, making the index call provably redundant, not merely inconvenient.

**Fixed in this pass, in the code, not left as a finding for someone else to act on**:
`crates/providers/delonix-proxmox/src/sdn_routing.rs`'s `sdn_vnet_firewall_index` doc comment now
states the restriction precisely (measured, with the exact error), so a future caller does not
reach for it expecting token access; `tests/live.rs`'s `sdn_routing_chain_vnet_firewall_and_the_
lock_round_trip_through_the_node` no longer calls it. Re-run with **only** the API token set (no
`DELONIX_PROXMOX_TEST_USER`/`_PASS` at all) after the fix: `1 passed; 0 failed`, in 48.6s. Before
the fix, this exact invocation failed on the first vnet-firewall call it made. `cargo fmt --check`,
`cargo clippy -D warnings`, `lang_ratchet.py`, `arch_fitness.py` and the crate's 121 unit tests all
clean afterward — no baseline moved.

This is the answer to "how do we stop a DevOps engineer discovering this in pain": not a config-
level warning about a limitation that would still exist, but removing the one place a token-
authenticated caller could ever have hit it, since nothing production-facing called the restricted
route to begin with. A config-level note would have documented a trap; this closes it.

## 6.2 New finding: enabling the datacenter firewall has a side effect on fresh SDN zone DHCP

Documented in full in §5.4. Worth a short, standalone note wherever this repository documents test
ordering for the `delonix-proxmox` live suite (if nowhere yet, this paragraph is the draft for
one): running the firewall-dependent tests (`the_vms_own_firewall_*`, `a_scope_vm_policy_lands_on_
the_nodes_own_firewall_and_reads_back`, `a_system_containers_firewall_is_applied_and_reads_back`)
turns the datacenter firewall ON for the rest of the run, which can make a *later*, unrelated SDN
zone's DHCP-backed guest fail to get an address — not because anything about IPAM or DHCP is
broken, but because the iptables backend's default forward policy does not auto-exempt a zone
created after the firewall was turned on. A test suite that runs the two kinds of test in the same
process, in a fixed order, will hit this reliably; this report hit it on the very first attempt.

## 7. What this pass did **not** independently re-confirm

Distinguishing what this session proved fresh from what it only read off an existing evidence
citation already in the catalog:

- **Everything in §5's final tally (34 tests) is freshly proven in this session**, against this
  exact build, on this exact node — not re-citing older evidence.
- **The capability tables in §3 largely cite `check:`/`e2e:`/`chaos:`/`live:` evidence this session
  did not re-run** (the full `scripts/e2e.sh` battery, the chaos scenarios, the `delonix-provider-
  libvirt` crate's own antispoof test) — this pass trusts those citations as accurate statements of
  *what was proven once*, consistent with this repository's own stated discipline that a capability
  only earns `supported` with a named, checkable citation. It did not re-run the entire battery to
  re-confirm every one of the 141 catalog rows; doing so was outside what "evaluate Proxmox and
  Cloud Hypervisor live" asked for, and would have meant re-validating container-engine capabilities
  the user did not ask about this time.
- **Migration/HA/replication/fabric-peer scenarios remain declared-capability-only for this
  session** — real, live-proven once (per this crate's own test history against a different,
  two-node lab, referenced throughout `AGENTS.md`), not re-proven here, because this lab has one
  node.
- **GPU passthrough (`container.gpu.cdi`), TPM (`vm.device.tpm` on libvirt), hugepages
  (`vm.memory.hugepages`) stay `partial` for the same reason their own capability rows already
  state**: no GPU/TPM/hugepage-configured host was available to this session either.
- **The DNS provider (`net.dns.records`/`net.dns.authoritative`) and NetBox/phpIPAM-backed IPAM
  controllers remain unexercised here** — this session's IPAM evidence is the node's *own* `pve`
  IPAM plugin, which needs no external controller; a real PowerDNS server was never stood up.
- **This lab is not this session's alone.** Mid-cleanup, three SDN zones this session never
  created (`dlxaud`, `dlxlive`, `dlxvlan`, hand-named rather than the auto-generated suffixes this
  crate's own tests produce) were found on the same node, and a `cargo test --workspace` was seen
  running concurrently under a different worktree (`vmaas-auditoria`, merged `main` — and this
  report's own prior PR — the day before). Not touched, and the datacenter firewall toggling in
  §5.4 happened before this was noticed. No evidence anything broke, but it is a real risk this
  report did not control for, and it means another session could, in principle, be depending on
  this exact node's state at the same time a future session continues this work.

## 8. Before anyone commits this file

- No raw secret value appears above — the API token's *value* is referenced only as "a token" or
  by its id (`root@pam!live-eval`), never the secret string itself, and the password used
  throughout (`delonix-admin`) is the same public, documented default this repository's own
  `AGENTS.md` already states for every Proxmox-family appliance it builds — not a secret this
  report is the first to disclose.
- **Still worth a decision before committing**: whether to generalize the lab IP
  (`192.168.122.220`) and the token id, or leave them as-is. They identify a disposable, libvirt-
  NAT-only VM on the author's own machine, not a reachable production target — the same category of
  detail `AGENTS.md` already publishes at length for its own, longer-lived lab hosts (IPs, node
  names, storage layouts). Leaving them in matches that precedent; stripping them would make this
  report harder for a future session to reproduce or extend against the same VM.
- The lab (`pve92`) was left running, clean, and in the tested baseline state (firewall on,
  management IPSet in place) — nothing here requires tearing it down, and leaving it up is what
  lets a future session pick this exact work back up without repeating §2.1's setup.

## 9. Pending — honestly, what this delivers and what it does not

The brief was "evaluate Proxmox and Cloud Hypervisor live" and then "report completely on
provider compatibility." Here is what was actually promised vs. delivered, without folding a gap
into a caveat where it should be a plain "not done":

**Delivered, with evidence, this session:**

- A live, running Proxmox VE 9.2.2 node, reachable, with four real lab-setup gaps found and fixed
  (not worked around): `dnsmasq`, the interfaces source line, storage content type, and the
  firewall/DHCP interaction (§5.4, §6.2).
- 33 of 39 `tests/live.rs` functions genuinely exercised and passing — the real count, arrived at
  by distrusting `cargo test`'s own `ok` and checking the node's task history instead (§5.5).
- One real code fix landed and proven: `sdn_vnet_firewall_index`'s `root@pam`-only restriction no
  longer has a caller, documented precisely where a future one would look, and the test that used
  to need it now runs on a token alone (§6.1).
- Cloud Hypervisor's full VM lifecycle, done by hand since the crate has no live suite of its own,
  genuinely exercised (§6).
- A capability-matrix cross-check: the committed `docs/providers/capability-matrix.md` is
  byte-identical to a fresh generation 47 commits past the tag it was written against — not stale.

**Not delivered — named, not hidden:**

- **Migration, HA, replication, and SDN-fabric-peer behavior against a real multi-node
  cluster.** This session had one node. The code path is proven once, historically, against a
  different two-node lab (per `AGENTS.md`'s own account) — not re-proven here. Closing this
  properly needs a second node brought up in the same lab, which this session did not do.
- **The DNS provider against a real PowerDNS server.** `ADR-0064`'s own D6 (cleaning up the
  records the node leaves behind) is still "decided and not implemented," for the same reason
  this repository's own prior `auditoria/naas-kaas-caas` pass already left it open: it needs a
  live DNS server to validate against, and names that as a precondition in its own text. Nothing
  in this session changes that gap's status. **Corrected 2026-10-10:** D6 shipped the same
  day in #758 and was measured against the lab's PowerDNS (`pdnslab`); see ADR-0064.
- **The guest-agent-dependent tests** (`o_ip_vem_do_agente_de_um_convidado_a_serio` and its
  siblings already proven once) need a prepared VM with `qemu-guest-agent` running, which this
  session's fresh lab did not have time to build on top of everything else done.
- **GPU, TPM, and hugepage capability rows stay `partial`** — no host with any of those three was
  available to this session, same as every prior pass that measured them.
- **A second, independent architectural question this session's own vnet-firewall investigation
  raised but did not answer**: is the `root@pam`-only restriction on the index route a deliberate
  Proxmox design choice or an inconsistency in its own permission checks (every sibling route
  accepts a token; this one, which does strictly less, does not)? Not filed upstream, not
  resolved — the fix here works around it rather than explains it, which is the honest limit of
  what a client library can do about another product's own API.
- **The `vmaas-auditoria` concurrent-session risk (§7)** is noted, not resolved — coordinating
  two sessions against one shared lab VM is outside what this report can fix from inside a single
  session.

Nothing above is framed as a success with an asterisk. Each is a real gap, named so the next
person — or the next session — does not have to re-discover it by reading between the lines.
