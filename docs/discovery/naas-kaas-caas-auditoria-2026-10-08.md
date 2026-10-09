# delonix-runtime as the technical base of NaaS / KaaS / CaaS — 2026-10-08

Scope: **delonix-runtime only**. Nothing in `delonix-paas` (or any other consumer) was
read, proposed, or changed — matching this repo's own "the engine knows no consumer"
doctrine (see `AGENTS.md`, "Identidade e fronteira do motor"). Measured against `main` at
`1cbe9639` (tag `v5.0.0`), in a dedicated worktree
(`.worktrees/delonix-runtime/naas-kaas-caas`), branch `auditoria/naas-kaas-caas`.

This document is the synthesis. The four detailed, evidence-cited matrices it draws on
are committed alongside it:

- [`naas-matrix-2026-10-08.md`](naas-matrix-2026-10-08.md) — networking, IPAM, firewall,
  OPNsense/Proxmox SDN.
- [`kaas-matrix-2026-10-08.md`](kaas-matrix-2026-10-08.md) — `cluster apply`/`cluster
  kubeadm`, CRI, cgroup hierarchy, node lifecycle.
- [`caas-matrix-2026-10-08.md`](caas-matrix-2026-10-08.md) — container runtime, OCI,
  volumes, limits, the `SystemContainer`/OCI boundary.
- [`contracts-boundary-2026-10-08.md`](contracts-boundary-2026-10-08.md) — the PaaS/Runtime
  boundary, tenant-neutrality, idempotency, async operations.

Each was produced by an independent pass that read the **current code** (grep, `Read`,
real function signatures, existing tests) and used `AGENTS.md`'s ~9,000 lines of history
only as a map of where to look — never as a substitute for checking the code, because this
repo's own history is full of entries correcting exactly that mistake. Every "PASS
medido" claim below cites the test or `e2e.sh` section that proves it; every "PASS por
leitura" is explicitly a weaker claim (code reads as correct, no dedicated test found in
this pass); every "GAP" names the file and function.

---

## 1. Architecture observed, and the responsibilities inside it

The separation the brief asks for (PaaS: product API, tenant auth, plans, quotas, billing
— Runtime: technical validation, isolation, execution, operation state, recovery,
observability) is **already the architecture**, not something this audit had to impose:

- `scripts/arch_fitness.py` fails the build if any `crates/`/`bins/`/`proto/` file names a
  consumer, and a manual search (grep for `tenant`/`plan`/`quota`/`billing`/`customer`/
  `account` across the whole tree) independent of that script found no matches that are not
  noise — cgroup/volume/NAS "quota", the reconciler's "plan" verb, or the Linux SSH "account".
  This is the strongest single piece of evidence for the boundary holding: an automated
  gate AND an independent manual check agree.
- The 29-crate layering (`foundation → contexts → adapters/providers → interfaces →
  composes`) is enforced by the same script, with declared, phase-numbered exceptions —
  never an open-ended one.
- **What is not yet true**: no request on the wire (CLI args aside) carries *who asked*.
  `delonix-node-api`'s `ListOperationsRequest` has no scope at all — any peer that can reach
  the socket (today: the same uid as the server, via `SO_PEERCRED` — "one fully-trusted
  agent per node", never written down as an explicit decision) sees every operation of every
  caller. This is not a bug to fix inside the Runtime today (there is exactly one trusted
  local agent by design), but it is the one place where "the Runtime validates boundaries
  even when the PaaS supplies the identity" is **not yet true**, because there is nowhere on
  the wire to supply it. Flagged, not fixed — see §8.

## 2. Capability matrix, condensed (full detail in the four linked files)

| Domain | Measured-pass | Read-pass | Partial | Gap | Not supported (by design) |
|---|---|---|---|---|---|
| NaaS | 31 | 12 | 1 | 6 | 4 |
| KaaS | ~8 | ~15 | 3 | 7 (2 confirmed-zero-code) | — |
| CaaS | ~14 | ~9 | 2 | 3 top-tier | — |
| Contracts | 1 full (capability discovery) | several designed-not-wired | — | 3 top-tier | — |

Headline facts, each with file:line evidence in the linked matrices:

- **NaaS**: the IPv6 anti-spoof hole this repo had in 2026 is confirmed closed in the
  current code (`disable_ipv6_argv` + `table ip6 dlxing`, tested). A VM on the Proxmox
  provider never joins the engine's native SDN (`net0_arg` always resolves `vmbr0`, zero
  `delonix_sdn` import in `delonix-proxmox`) — "compute on one provider + native engine
  networking" is not a combination that exists today; "compute + firewall on the SAME
  provider" (`scope: vm`/`scope: systemcontainer`) does exist and is live-validated.
- **KaaS**: `spec.cni` was validated by round-trip and never read again anywhere in the
  real bootstrap path — see §4 below, this was the single most consequential finding of
  the whole audit and is now partially fixed. `oom_score_adj`/`cpuset_mems`/`unified`/
  hugepages are confirmed silently dropped on the CRI path (zero references outside the
  `.proto` struct), directly contradicting ADR-0038's own text ("Honour or refuse, never
  ignore"). There is no teardown and no backup/restore for a VM/SSH-provisioned
  (`kubeadm`) cluster — the only topology worth selling — while kind-mode has both.
- **CaaS**: the confusion the brief explicitly asked to avoid — treating a Proxmox LXC as
  if it were an OCI container — **does not exist in the code**: `SystemContainerProvider`
  shares no type with `RunOpts`/`Container`, and `kinds.rs` gives the two Kinds different
  `Namespaced` values with privilege fields refused by name. Daemonless is preserved: every
  long-lived process (holder, slirp, supervisor, log shim, L7 proxy, `serve *`) is lazy and
  opt-in, confirmed by reading `main.rs` — nothing starts without an explicit trigger.
- **Contracts**: 39 of 59 `delonix-node-api` RPCs are `UNIMPLEMENTED` — Container, Pod, VM
  and Stack have zero served verbs, so a PaaS integrating against the contract today still
  needs the local CLI for almost everything. The async-operation machinery
  (`operations::begin/finish/replay`, `Interrupted`-on-dead-owner, 7-day retention) is well
  built and tested for exactly the "lost the connection, asked again" case the brief names
  in §9/§10 — but only 2 of the 22 mutating RPCs that should return an `Operation` do; the
  other 20 are not wired to it at all (they are simply `UNIMPLEMENTED`).

## 3. The 12 gaps judged most severe across all four domains

Ranked the way the brief's own §12 priority order asks (security/isolation/data-loss
first), independent of which domain each came from:

1. **CaaS — `--secret-files` could leak to the persistent rootfs in plaintext.** `FIXED`
   (§4.1). Every failure in the mount/write path was silently discarded; a tenant secret
   could survive a `container commit` or a filesystem backup.
2. **KaaS — every real cluster is permanently `NotReady`, and the CLI reports success
   anyway.** `FIXED for single-node, confirmed live` (§4.2, §4.6). `spec.cni` was
   validated and never applied; the final line printed "cluster ready" unconditionally,
   including on the historical `wait_ready: false` timing where readiness is never even
   checked. Single-node is now genuinely fixed and proven on real libvirt/KVM; multi-node
   is unchanged (no `--cni` escape hatch exists on `cluster kubeadm` yet to refuse it
   safely).
3. **CaaS — the ADR-0062 root-fallback is invisible to any external consumer.** `FIXED`
   (§4.3, §4.11). A Pod that declares `runAsNonRoot` can be running as root, with
   nothing in `ContainerStatus`/`inspect`/logs to show it. The CLI/`inspect` half closed
   first (`1e5562e6`); the CRI half — `ContainerStatusResponse.info`, the kubelet's own
   path to the fact — closed afterward (`46e3222a`, §4.11), gated on `verbose` exactly
   like `StatusResponse.info`'s `capabilityCeiling` already was.
4. **KaaS — `oom_score_adj`/`cpuset_mems`/`unified`/hugepages silently dropped on the CRI
   path.** `oom_score_adj` `FIXED` (§4.5); `cpuset_mems`/`unified`/hugepages
   **FIXED in a later session** (#752, `f72a4f55`/`61e901bf`: `CriResources` gained the
   three fields, `refuse_unenforceable_resources` runs before `StartContainer` reports
   success against whichever leaf the container actually lands in, and the write side
   mirrors `cpuset.cpus` across all three placement paths; `cpuset_cpus` itself — wired
   into `apply_resources` since before this gap was even filed, with no preflight of its
   own — was closed in the same pass). `UpdateContainerResources`, named alongside this
   gap in ADR-0038's own decision text as item 4, is **also fixed** (#755,
   `724925a6`, pending merge at the time of this note): `CriResources::merge_update`
   treats a zero/empty field as "leave unchanged" rather than "no limit" (the opposite
   of `CreateContainer`'s own reading of the same wire value), and
   `delonix_linux::update_kube_resources` dispatches to `SetUnitProperties` under a
   kubelet systemd scope or a direct write otherwise. Both validated live on the
   non-systemd leaf path (real `crictl`); the systemd `SetUnitProperties` branch for
   either fix still needs a stable kubelet to measure for real — unit/pure tests only,
   same boundary §8 already names for the rest of this ADR. ADR-0038's four decision
   items are now all implemented; what §8 below still calls open for this gap is
   superseded.
5. **CaaS — multi-tenant admission is open by default, with zero warning event.** `NOT
   FIXED` — deliberate for the engine's documented single-tenant use, but a CaaS layer that
   forgets to configure both `RuntimePolicy` and `DELONIX_CRI_CAP_CEILING` gets node
   compromise with no structural signal. See §8.
6. **NaaS — Proxmox-native vnet firewall is accepted and read back but does not actually
   filter** under the Proxmox default firewall backend (`iptables`/`pve-firewall` — not
   the datacenter enable/disable toggle; it is enforced only under `nftables`/
   `proxmox-firewall`). `INVESTIGATED, not closeable the way first proposed` (§4.9): the
   obvious write-time guard needs a node-side probe this engine's own prior decision
   (`docs/proxmox/matrix-9.2.2.md`, ADR-0049) already walls off as "host administration" —
   doc-comment hardening landed instead, naming the mismatch and requiring an owner
   decision before any future caller can reach it uninformed. Not yet reachable by any
   `kind:`/CLI, so still dormant — the same "public, dead, bug waiting for its first
   caller" pattern this repo has already catalogued five times.
7. **NaaS — stale DNS on Proxmox VM teardown/rename** can point to an address later
   reassigned to a different workload (ADR-0064 D6, decided, not implemented).
   `REVIEWED, still not attempted` (§4.10): confirmed there is zero existing scaffolding
   for it (`ProviderEntry`'s `type` enum has exactly `libvirt`/`proxmox`/`opnsense` — no
   `dns`/`powerdns` credential entry anywhere), the ADR's own text names a live DNS server
   as a precondition this session does not have, and it is already its own scheduled
   slice of work (Sprint 2, `docs/discovery/66_CONTINUITY_PLAN.md`) — not a bounded fix
   the way gap #6 turned out to be.
8. **NaaS — asymmetric rollback in `netops::remove`** can leave an orphaned `NetDef` that
   blocks future creates with a false `NetworkPrefixConflict`. `FIXED` (§4.4).
9. **Contracts — no request anywhere carries caller identity.** `ListOperationsRequest`
   is globally unscoped. Not an active exploit today (single trusted local agent per the
   `SO_PEERCRED` model) but a real gap the moment more than one caller shares a node.
   `REVIEWED, still not attempted` (§4.10): confirmed the socket's own accept path
   (`delonix-node-api/src/lib.rs:134`) refuses any peer whose uid differs from the
   server's own — by construction there is exactly one possible caller identity today,
   so recording it would record a constant, not information. Nothing small to add ahead
   of the real multi-caller design this gap is actually about.
10. **KaaS — no teardown for a VM/SSH-provisioned cluster.** `FIXED` (§4.8, §4.11).
    Destroying one meant manually `vm rm`-ing every VM, by hand editing `~/.kube/config`
    to drop the stale context — **confirmed live, the hard way** (§4.6): this audit's own
    test VM had to be torn down exactly that way before the fix existed. `cluster destroy`
    now detects a VM-provisioned cluster by its node-naming convention and removes the
    VMs, the cached kubeconfig, and the matching `~/.kube/config` entries in one call,
    leaving the operator's `--network` alone. The other half, `etcdctl snapshot`
    backup/restore, closed afterward (`7d529876`/`c7fe951d`, §4.11) — `cluster backup
    <name> [--to <path>]` and `cluster restore <name> --from <path>`, scoped to a single
    `etcd.mode: stacked` control-plane and stated as not validated against a real
    cluster in the session that wrote it (see §4.11 for the exact boundary).
11. **Contracts — 39 of 59 RPCs unimplemented**, Container/Pod/VM/Stack/Image with zero
    served verbs — the PaaS cannot delegate to the contract for almost anything yet.
    `REVIEWED, still not attempted` (§4.10): confirmed against the current tree (8 explicit
    `UNIMPLEMENTED` handlers; the rest never reach a handler at all, unserved by the
    router). Contract completion at this scale is squarely the P5-phase owner's call per
    §6's own stated reasoning, not something to grow by a few RPCs under an unrelated
    task.
12. **Contracts — structured errors (`ErrorDetail`) only exist inside `Operation.error`**,
    never on the synchronous error path, which is where most real errors actually surface.
    `REVIEWED, still not attempted` (§4.10): confirmed `ErrorDetail` appears in exactly
    `operations.rs`/`operations.proto`/`common.proto` and nowhere else — the claim holds
    precisely as stated. Threading it onto every synchronous RPC response is the same
    class of contract-wide decision as #11, same owner.

**Gap #13, found live and not in the original ranking.** `FIXED` (§4.7, found in §4.6):
`cluster kubeadm`/`cluster apply` upgraded `delonix-cri` on a node but never the `delonix`
CLI itself, and the CRI's `__apirun` re-exec target resolves whatever `delonix` is on the
node's `PATH` — so a node provisioned from an old golden image ran brand-new CRI protocol
handling on top of however old the baked-in CLI was, for every actual container spawn.
This was a real, security-relevant correctness gap (it silently defeated any fix landed in
the CLI/engine code after a golden image was built, ADR-0062's own root-fallback ordering
fix included — measured live, reproducing the exact crash that fix's own commit comment
describes as closed) that none of the four sandbox-only matrices could have found, because
finding it required a node old enough to be out of sync with its own CRI. `prepare_host`
now installs/replaces the on-node `delonix` CLI the same sha256-check-then-replace way it
already did for `delonix-cri`, on every `apply_ssh` pass — first provisioning and every
re-apply after.

## 4. Corrections implemented, with regression tests

Seven behavior fixes landed (§4.1–§4.5, §4.7, §4.8), in priority order (security/data-loss
first, per the brief's own §12), plus one live-validation pass (§4.6, not itself a fix but
where two of the later ones were found) and one documentation-hardening pass (§4.9, gap
#6 — the behavior fix it first proposed turned out to be blocked, see its own subsection).
Each fix is its own commit on
`auditoria/naas-kaas-caas`, each passed `cargo fmt --check`, `cargo clippy -D warnings`,
`python3 scripts/lang_ratchet.py`, `python3 scripts/arch_fitness.py`, and the pre-commit
hook's `cargo check --workspace --all-targets` before being committed. A final
`cargo test --workspace --lib` after the first three (see §5) passed with **zero**
failures across all 25 library crates; every fix after that was re-verified at the level
of the one crate (or two) it touched, per each subsection below.

### 4.1 `fix(container)` 3626ae70 — `--secret-files` fails closed

`crates/adapters/delonix-linux/src/lib.rs`, function `write_secret_files`. Every step
(`create_dir_all`, the tmpfs `mount(2)`, each `write`/`set_permissions`) was `let _ =
...`'d away. A tmpfs mount failure (a rejected flag on a given kernel, `/run/secrets`
already occupied, a tighter cgroup/userns than expected) meant the loop kept writing the
secret VALUES onto whatever `/run/secrets` already was — the container's own persistent
overlay, which a `container commit` or a filesystem backup can carry off the host in
plaintext. Zero test covered this path before.

Split into `mount_secrets_tmpfs` (the two `mount(2)` calls) and `write_secret_values`
(pure I/O, now unit-tested on a plain temp dir), both returning `Result`; the caller
aborts the container (exit 126, the same pattern `setup_rootfs` failure already uses)
instead of starting one whose secrets may be on disk.

New tests: `write_secret_values_writes_0600_files_with_the_exact_content`,
`write_secret_values_skips_unsafe_keys`,
`write_secret_values_fails_closed_instead_of_silently_dropping_values` (reverted and
re-verified: fails without the fix, in `tests::`). 174/174 `delonix-linux` tests pass.

**Not validated live**: a real tmpfs-mount-failure scenario needs `CAP_SYS_ADMIN`/a mount
namespace this sandbox has no root to exercise; the fail-closed *wiring* through the `?`
chain is enforced by the compiler, proven by the unit tests on the pure half, not by a
mocked `mount(2)`.

### 4.2 `fix(cluster)` 817946b8 — CNI for single-node clusters, honest readiness reporting

`bins/delonix-runtime-bin/src/cmd/cluster.rs`. `spec.cni` was validated by round-trip
(`cluster.rs:3892`) and never read again anywhere in `kubeadm_init`/`apply_ssh` — every
cluster bootstrapped via `cluster apply`/`cluster kubeadm` stayed `NotReady` forever, and
`apply_ssh` printed `cluster "x" ready` unconditionally regardless of whether a single
node had ever converged, including on the historical `wait_ready: false` timing where
readiness is never even checked.

Two changes, deliberately scoped to what is provably safe without live-validation
capability here:

1. `ensure_single_node_cni` wires a plain bridge + host-local IPAM conflist at
   `/etc/cni/net.d/10-bridge.conflist` — **not new mechanism**: it is the identical
   conflist `delonix-cri`'s own root-mode pod-sandbox path already requires and reads
   (ADR-0074), already live-validated against a real kubelet per `AGENTS.md`'s section on
   that fix. Applied only when the cluster has **exactly one node total** (removes the
   control-plane taint too, mirroring `kindmode.rs`'s own `workers == 0` branch — otherwise
   not even CoreDNS schedules). Multi-node clusters are deliberately left untouched: a
   plain bridge CNI has no cross-node routing, and `cluster kubeadm` has no `--cni` flag to
   opt out of `default` — refusing outright there would break the documented HA example
   with no escape hatch. A scope decision, recorded in the function's own doc comment, not
   an oversight.
2. `wait_for_cluster_ready` now distinguishes "zero nodes ever converged" (hard error —
   the exact signature of a CNI that was never applied) from "some did, still converging"
   (stays a warning, as before). `apply_ssh`'s final line is conditional on what was
   actually observed: "ready" only when confirmed, "bootstrapped — not all nodes Ready"
   when partial, "bootstrapped — readiness was not checked" when `wait_ready` was never
   requested at all.

New tests feed the generated conflist through **this engine's own** CNI parser
(`delonix_sdn::cni::parse_config`/`readiness`), not just assert it is valid JSON — a
conflist shaped wrong would otherwise compile and still leave the node `NotReady`,
undetectably. 47/47 pre-existing `cmd::cluster::` tests still pass.

**Not validated live**: no root/VM/hypervisor in this sandbox to run a real `kubeadm
init` against. The conflist content and the state-machine change are proven against this
engine's own parser and unit tests; a single-node `cluster kubeadm --control-plane 1`
reaching `kubectl get nodes` → `Ready` without a manual step still needs a host that can
actually run one.

### 4.3 `fix(container)` 1e5562e6 — the ADR-0062 root-fallback is now persisted

`crates/contexts/delonix-compute/src/{record,run}.rs`,
`bins/delonix-runtime-bin/src/cmd/container.rs`. The only trace of "this host could not
honour the image's declared non-root `USER`, so the container runs as root instead" was a
one-shot `Notice` — printed once to the CLI's stderr, and on the CRI path (the normal way
a kubelet creates a container) captured into a temp file read only on failure and
discarded, unread, on success. A Pod declaring `runAsNonRoot` could be running as root
with nothing in `inspect`, `describe`, or the CRI's `ContainerStatus` to show it.

Added `Container.user_fallback_to_root: bool` (`#[serde(default)]`), set by `run.rs`'s
existing fallback match arm and carried through `build_record` into the persisted
record. `container describe` now prints a `User` field naming ADR-0062 explicitly and
unconditionally in the fallback case; `container inspect` gets it for free (full-struct
JSON serialization).

Extended the existing `the_images_user_is_the_default_user` test rather than duplicating
it: asserts the flag is `false` on every successful-USER and declared-root path, `true`
on the fallback path, `true` across the re-exec pass even though the `Notice` itself only
prints once, and — the assertion that actually matters, since `inspect`/CRI read the
*persisted* `Container`, not the transient `ResolvedRun` — `true` on `build_record`'s
output. Reverted the fix locally and confirmed the test fails without it
(`assertion failed: r.record.user_fallback_to_root`), then restored it. 84/84
`delonix-compute` tests, 92/92 `cmd::container::` tests pass.

**Deliberately not done at the time, closed as its own follow-up (`46e3222a`, §4.11)**:
the CRI's own `ContainerStatus` could not show this. CRI containers are tracked in a
separate `ContainerRec`/store, distinct from `delonix-compute::Container`, and
`ContainerStatusResponse.info` was always empty, with `container_status` not even
accepting a `verbose` parameter to gate it on. The cross-reference turned out not to be
at `StartContainer` (the fallback decision happens inside the engine process it shells
out to, after that function has already returned) — it is read back at `ContainerStatus`
time, from the engine's own persisted record, through the SAME `load_reconciled` helper
`state`/`exit_code`/OOM-killed already use. See §4.11 for the full fix.

### 4.4 `fix(net)` 347cd598 — a network's record now outlives a failed dataplane removal

`crates/adapters/delonix-sdn/src/netops.rs::remove` erased the `NetworkStore` record
FIRST, then attempted the (best-effort, error-swallowing) dataplane teardown. If that
failed partway, the physical `NetDef` stayed on disk — read by `network_get`, and by the
prefix-conflict check a later `network create` runs — with nothing in `NetworkStore`
pointing at it: the network vanished from `network ls` while its prefix kept blocking a
replacement, with no command able to reach it. Exactly the inverse of `create_bridge`'s
own already-correct rollback in the same file. Reordered so the two dataplane calls run
first and the store record is removed last — neither depends on the other, so the reorder
changes no other behavior, and a dataplane failure now simply leaves the network visible
and its prefix honestly still-taken, instead of invisible-but-blocking. No new automated
test (neither `netops` function has ever had one — they talk to a live holder over a
control socket with no injectable seam); verified by reading, and by a clean 285/285
`delonix-sdn` test run with the reorder in place. Found by the NaaS audit's gap #3.

### 4.5 `fix(cri)` 225664a2 — `oom_score_adj` honoured instead of silently dropped

ADR 0038's own text is "honour or refuse, never ignore"; `LinuxContainerResources.
oom_score_adj` had neither — read off the CRI wire into `CriResources` and never
referenced again anywhere (confirmed: zero references outside the raw `.proto` struct).
Refusing it outright is not viable (every real kubelet sets it, on every pod, of every QoS
class — refusing would fail `CreateContainer` for all of them). Wired the value through
`CriResources` → `RunOpts` (new `oom_score_adj: Option<i32>` field, "0 means not set" —
the same convention this struct's sibling fields already use) → `Container` (persisted,
`#[serde(default)]`) → a new `apply_oom_score_adj`, called in `container_init` immediately
next to `apply_ulimits` and for the identical reason (lowering the value needs
`CAP_SYS_RESOURCE`, held at that exact point, before `drop_capabilities`). Also exposed as
a hidden `--oom-score-adj` flag on the native CLI (mirrors the existing
`--kube-cgroup-parent` hidden-flag pattern exactly), giving the CLI the same capability
for free. Extended the existing `pod_limits_become_run_flags` test with a real
Guaranteed-QoS value (`-998`) rather than adding a parallel test. No test for the actual
`/proc/self/oom_score_adj` write: that path is process-wide state, not per-thread, and
mutating it in a unit test would corrupt every other test running concurrently in the same
binary — the same class of hazard `arch_fitness.py`'s `env_writes` ratchet already
tracks for raw `std::env::set_var`. Found by the KaaS audit's gap #4 (the overall report's
ranked gap #4).

### 4.6 Live validation on a real host — what the sandbox analysis above could not do

All of the above was written assuming no root/KVM were available to validate live. That
assumption was wrong for the actual host this session ended up running on, and the user
asked for the KaaS fix specifically to be tested with a real `delonix vm`. This section is
that test, done with full isolation from the host's own state (see the warning below), and
its honest result — including a real, separate gap it surfaced that was not one of the
five fixes above.

**Isolation, because this host runs production workloads.** Before touching anything, a
read-only survey found this host running a live stack (Delonix Meet: PBX, web edge,
Kamailio, FreeSWITCH, Postgres, Redis, Coturn, several `kaeso-odoo` containers) under the
DEFAULT `DELONIX_ROOT`. None of it was touched. Testing used a from-scratch
`DELONIX_ROOT` **and** a from-scratch `DELONIX_NET_RUNTIME_DIR` — this repo's own history
(`AGENTS.md`, "Meia-isolação é pior que nenhuma") documents a real incident on this exact
host where isolating only the first and not the second caused one test session to tear
down another's live network holder, because the holder/slirp socket directory is keyed by
uid, not by `DELONIX_ROOT`.

**The golden VM image was imported from the host's own local copy, not downloaded.**
`image vm import` registered the host's already-present `delonix-vm-k8s:1.36` qcow2 (a
read-only operation on the source file) under the isolated root — 7.4s, no network egress,
no risk to the host's own copy.

**Result: the fix works.** `cluster kubeadm --control-plane 1 --workers 0 --network
<isolated> --copy-kubeconfig` against this binary (with a freshly-built matching
`delonix-cri` next to it) produced, for the first time, an honest and correct "cluster
ready": the "Installing the default CNI (bridge, single node)" step ran and the node
reached `Ready` with the control-plane taint removed, confirmed by `kubectl get nodes`
(`Ready`, 16s old) directly against the live kubeconfig — not by trusting the CLI's own
claim. CoreDNS's two pods were scheduled with real addresses from the pod subnet
(`10.244.0.2`/`.3`), proof the bridge CNI is actually handing out routable IPs, not just
reporting ready. `wait_for_cluster_ready` returned `Ok(true)` in 0.3s instead of the
historical silent-timeout warning.

**A real, separate gap this test found: `cluster kubeadm` upgrades `delonix-cri` on the
node but never the `delonix` CLI itself — and the CRI's own re-exec target resolves
whatever is on the node's `PATH`.** CoreDNS immediately crash-looped with `setuid(65532)
failed — the image USER is not mapped (subuid?)` — the exact symptom a comment at
`crates/adapters/delonix-linux/src/lib.rs:3493-3510` documents as fixed in this
repository's current source (moving the user switch to before `drop_capabilities`, so
`--cap-drop ALL` pods like CoreDNS do not lose `CAP_SETUID` first). Tracing it down: the
`__apirun` child processes `delonix-cri` re-execs to actually spawn a container were
running `/usr/local/bin/delonix` — and that binary reported **`delonix 0.66.0`, built
2026-08-27** — the CLI baked into this golden image two months before the fix the comment
describes, while the freshly-installed `delonix-cri` (sha256-verified as the one this
session built) is `5.0.0`. `cluster kubeadm`'s host-prep step installs/replaces
`delonix-cri` on the node (confirmed in the bootstrap output: `"delonix-cri differs from
the resolved one … replacing it"`) but has no equivalent step for the `delonix` CLI the
CRI re-execs into for the actual container-creation work — so a node provisioned today
from an old golden image runs new CRI protocol handling on top of month-old container
spawn logic, silently. **This is a real gap, found live, not one of the five ranked
originally** — filed here rather than guessed at from a sandbox. Replacing
`/usr/local/bin/delonix` on the test VM with this session's own build and recreating the
CoreDNS pods confirmed the diagnosis (the stale binary was the cause, not a regression in
this session's fixes), but also made the node's control-plane flap for several minutes —
most plausibly this VM's minimal size (2 vCPU/2G, this command's own defaults) under the
combined load of the kubeadm bootstrap, the binary swap, and concurrent `kubectl`/`ssh`
traffic from this session, rather than anything specific to the swap itself. That part is
reported as inconclusive, not as a finding, because it was not isolated from its own
confound.

**Teardown — by hand, because the gap is real.** `cluster kubeadm`/`cluster apply` has no
destroy verb for a VM-provisioned cluster (KaaS gap #10, §3 above) — this session's own
test is live proof of it: cleanup needed `vm rm` (confirmed: 3 artifacts, 679 MiB freed),
`network rm`, `net netns down` (isolated holder), and three `kubectl config delete-*` calls
to remove the context this test's own `--copy-kubeconfig` had merged into the user's real
`~/.kube/config` (confirmed afterward: the user's original contexts and current-context
were unchanged throughout — `--copy-kubeconfig` only adds, never switches). The isolated
`DELONIX_ROOT`/`DELONIX_NET_RUNTIME_DIR` directories were then deleted. A final read-only
survey confirmed the host's production containers, VMs and networks were exactly as they
were before this test began. (This gap is fixed by §4.8, written after this test — a
cluster torn down the same way today would need one `cluster destroy` call instead of the
four commands above, minus the network, which §4.8 deliberately still leaves alone.)

### 4.7 `fix(cluster)` 73eb5f52 — the node's `delonix` CLI now stays in step with `delonix-cri`

Gap #13 (§3), found by §4.6's own live test, not in the original ranking. `prepare_host`
upgraded `delonix-cri` on every host-prep pass — sha256-check against the resolved binary,
replace and restart on mismatch — but never touched `/usr/local/bin/delonix` itself. Every
actual container spawn goes through `delonix-cri`'s own `__apirun` re-exec, and that
re-exec target is the node's `delonix` CLI, found by name on `PATH` — not the
freshly-installed CRI sitting next to it. A node provisioned from a golden image therefore
ran brand-new CRI protocol handling on top of however old the baked-in CLI happened to be,
invisibly: the two binaries talk to each other fine, so nothing about the mismatch ever
surfaced as an error.

Measured live in §4.6: `cluster kubeadm` against `delonix-vm-k8s:1.36` (baked-in CLI
`0.66.0`, built 2026-08-27) crash-looped CoreDNS with `setuid(65532) failed — the image
USER is not mapped (subuid?)` — the precise symptom a 2026-09-15 fix in this repository's
own `container_init` documents as closed (moving the user switch to before
`drop_capabilities`, so `--cap-drop ALL` pods like CoreDNS do not lose `CAP_SETUID`
first). The fix never reached this node, because nothing had ever installed a newer CLI on
it. Replacing `/usr/local/bin/delonix` by hand with this session's build confirmed the
diagnosis before this fix existed.

Adds `install_cli`, called from `prepare_host` right after `install_cri`: same
sha256-check-then-replace shape as its sibling, deliberately not merged into one function
(a CLI binary has no systemd unit to restart — new `__apirun` invocations pick up the
replaced file on their next exec, no service bounce needed). The local binary resolved is
`std::env::current_exe()` of `apply_ssh`'s own caller, canonicalized — matching the
existing convention that a node ends up running the SAME version as whatever
`cluster kubeadm`/`cluster apply` invocation provisioned or re-applied it, the same rule
`install_cri` already follows for the CRI binary. Covers both first-time provisioning and
re-running `cluster apply` on an already-bootstrapped cluster (`prepare_host` runs on every
`apply_ssh` pass, not just the first), which is exactly the case where a stale node most
needs a refresh.

1161/1161 `delonix-runtime-bin` tests pass; clippy, fmt, lang_ratchet and arch_fitness all
clean. No new automated test: `install_cli`'s own logic is a single boolean branch (match
→ skip, mismatch → replace) with no decision table worth extracting as a pure function,
and the sha256-driven SSH commands it builds carry the exact same safety properties as
`install_cri`'s own already-accepted, untested-at-this-level pattern (a hex digest and a
hardcoded path, neither attacker-controlled). **Not re-validated live in this pass**: the
fix that found the bug already proved the mechanism works by hand (replacing the binary
and watching the symptom disappear); automating that exact sequence through this new code
path needs another live VM run, left for a follow-up.

### 4.8 `fix(cluster)` 2ff608f5 — `cluster destroy` now tears down a VM/SSH-provisioned cluster too

Gap #10 (§3), the one this same §4.6 test had to work around by hand. `ClusterCmd::Destroy`
only recognized the container-based `cluster create` (kind-mode) shape, found by its
`io.x-k8s.kind.cluster` label. A cluster made of VMs — `<name>-cp<N>`/`-w<N>`/`-etcd<N>`/
`-lb`, named by `cluster kubeadm` itself — had no destroy verb: the only way to remove one
was `vm rm` on every node plus hand-editing `~/.kube/config`.

`is_cluster_vm`/`cluster_vm_names` detect a VM cluster by that same naming convention
(checked against `delonix_vm::list`, since VMs carry no container-style label);
`ClusterCmd::Destroy` tries this match first and falls through to kind-mode's destroy only
when it finds none. `destroy_vm_cluster` mirrors `kindmode::destroy`'s own shape —
confirmation prompt, a progress step per VM removed via `vm::cmd_rm`, then the cached
kubeconfig and `<root>/clusters/<name>/`. It deliberately leaves the `--network` alone:
`cluster kubeadm --help` itself says that network is the operator's, not a default this
command invented.

The new piece is `remove_local_kubeconfig_entries`: removes the `clusters[]`/`contexts[]`
entries named `<name>` and the `users[]` entry named `<name>-admin` — the exact three names
`merge_into_local_kubeconfig` writes a VM/SSH cluster's kubeconfig under — and clears
`current-context` if it was pointing here. Takes `dest: &Path` rather than reading `$HOME`
itself, the same testable shape `merge_into_local_kubeconfig` already uses. A failure here
is a warning, never aborts the rest of the teardown.

New tests: `is_cluster_vm_matches_the_naming_convention_exactly` (including the negative
case this needed most — `lab-cp` without a node number must NOT match cluster `lab`),
`remove_local_kubeconfig_entries_removes_only_this_clusters_three_names`,
`remove_local_kubeconfig_entries_tolerates_no_match_and_no_file`. Reverted and
re-verified: weakening the digit-check in `is_cluster_vm` made the negative case fail
exactly as expected.

1164/1164 `delonix-runtime-bin` tests pass (1161 before, +3 new); clippy, fmt,
lang_ratchet and arch_fitness all clean, baselines unchanged. **Not validated live in this
pass**: §4.6's own manual teardown already exercised the mechanism this fix automates
(`vm::cmd_rm`, kubeconfig YAML surgery) by hand on a real cluster; repeating that exact
live setup a second time in the same session, now through this new code path, is left for
a follow-up rather than redone immediately. The etcd backup/restore half of gap #10 (no
`etcdctl snapshot` wiring) closed afterward — see §4.11.

### 4.9 `docs(proxmox)` 3f9def7f — gap #6 investigated; the obvious guard is itself blocked

Gap #6 (§3): a Proxmox vnet firewall rule, written through
`set_sdn_vnet_firewall_options`/`add_sdn_vnet_firewall_rule`/
`update_sdn_vnet_firewall_rule` (`crates/providers/delonix-proxmox/src/sdn_routing.rs`), is
accepted and read back whether or not it filters a single packet — it is enforced only
under the node's `nftables` (`proxmox-firewall`) backend, never under the Proxmox default
(`iptables`, `pve-firewall`). The module's own top-of-file doc comment already measured and
stated this in full (lines 16-44, from an earlier PR); what was missing was anything at the
three call sites pointing back to it, or anything standing between a future caller and
treating a successful write as a working rule — the "public, dead, bug waiting for its
first caller" shape this repo's own `AGENTS.md` already catalogues five times
(`mount_live`, `set_net_rate`, `update_limits`, `publish_port_allow`, `Net`). Zero callers
exist today (confirmed by grep across `bins/delonix-runtime-bin` and
`delonix-networking`), which is the only reason this is dormant rather than active.

This session's own capability-matrix write-up (`naas-matrix-2026-10-08.md`, "Gap 1")
proposed the obvious-looking fix first: probe the node's firewall backend before writing,
mirroring `vm_firewall.rs`'s already-shipped `Error::DatacenterFirewallDisabled` guard
(reads `GET /cluster/firewall/options` before letting a VM's own firewall rules through).
**That fix turned out to be blocked by a decision this same repository already made
elsewhere**: the field that would answer the probe, the per-node `nftables` option, lives
under `/nodes/{node}/firewall/options` — and this engine's own measured API coverage
(`docs/proxmox/matrix-9.2.2.md`, from ADR-0049) classifies the WHOLE
`/nodes/{node}/firewall/*` tree `unsupported-by-design`, reason "node firewall — host
administration", the same boundary ADR-0049 D3 already draws for cluster administration in
general. Reading it anyway — even read-only, even only to decide whether to refuse a write
— would reach past a line this engine drew on purpose for an unrelated reason, the same
"widened its own reach without anyone deciding so" ADR-0064 D6 refuses for a DNS
controller's credential.

What landed instead is the fix available without crossing that line: a new module-doc
section in `sdn_routing.rs` ("No caller may expose this without reading this first") naming
the mismatch precisely and requiring an explicit owner decision (a new ADR addendum to
cross that boundary for this one read-only probe, or the capability catalog reporting this
row permanently `unavailable-on-host`/`not-implemented` and refusing any write
unconditionally) before any `kind:`/CLI path reaches either function — plus a one-line
pointer on each of the three functions themselves. Zero behavior change. This closes the
gap the only way available today: not by making the first caller impossible (that needs a
decision this audit does not own, the same reasoning §6 already applies to adding RPCs to
`delonix-node-api`), but by making it impossible for that caller to arrive uninformed.

121/121 `delonix-proxmox` unit tests pass (unchanged — nothing here is behavior a test
could observe); clippy, fmt, lang_ratchet and arch_fitness all clean, no baseline change.
**Not validated live, and cannot be without the owner's decision above**: the matrix's own
prescribed acceptance test (a live case toggling the node's `nftables` option and measuring
packets with it on and off) needs exactly the probe this pass found is blocked — writing
that test is itself gated on the ADR addendum this subsection asks for.

### 4.10 The remaining candidates, reviewed — none attempted, each with a specific reason

After gap #6 turned out to need real investigation before its verdict was clear, the other
four gaps this report had provisionally deferred (§8's first draft) got the same scrutiny
rather than a second blanket "out of scope." None changed verdict; each now has the
specific evidence that confirms it, not just the earlier one-line guess:

- **Gap #7 (stale DNS on Proxmox teardown, ADR-0064 D6).** Confirmed zero existing
  scaffolding: `ProviderEntry`'s `type` enum in `providers_config.rs` has exactly
  `libvirt`/`proxmox`/`opnsense` — no `dns`/`powerdns` credential entry anywhere in the
  tree, and `delonix-proxmox`/`delonix-networking`'s existing `dns.rs` modules implement
  only D1-D5 (reading a zone's DNS settings THROUGH Proxmox), never a direct engine→DNS-
  server client. D6 needs that client built from nothing, a new `providers.yaml` entry
  type, and — the ADR's own words — "its own live case against a real DNS server," which
  this session does not have access to (unlike the Proxmox lab, confirmed unreachable
  from this sandbox in §4.6's own isolation notes). It is also already named as its own
  scheduled slice (Sprint 2, `docs/discovery/66_CONTINUITY_PLAN.md`), not a gap sized for
  an audit-session fix.
- **Gap #9 (contracts carry no caller identity).** Confirmed the actual mechanism that
  makes this "not an active exploit today" is stronger than the one-liner suggested:
  `delonix-node-api/src/lib.rs:134` refuses any peer whose uid differs from the server's
  own, so there is, by construction, exactly one possible caller on any socket this engine
  serves. Recording that uid in an operation's metadata today would record a value that
  never varies — not the audit trail the gap is actually worried about, which only exists
  once a second legitimate caller identity is designed in. Nothing to add ahead of that.
- **Gaps #11/#12 (RPC and error-contract completeness).** Re-measured rather than assumed:
  8 explicit `UNIMPLEMENTED` handlers exist today, the rest of the 59 never reach a handler
  at all (unmatched by the router); `ErrorDetail` appears in exactly `operations.rs`/
  `operations.proto`/`common.proto`, nowhere else. Both claims hold precisely. Both are
  the same class of contract-wide decision §6 already declines to improvise — growing the
  served surface or the error shape by a few RPCs under an unrelated audit would not close
  either gap, only make the next, real completion pass harder to reason about.

### 4.11 Two gaps named "still open" in §3/§8 closed in a later session — `7d529876`/`c7fe951d`, `46e3222a`

Both of this section's own earlier "still open" notes (§4.3's deliberately-not-done CRI
half of gap #3; §4.8's "the etcd backup/restore half of gap #10 remains open") were
closed in a later session on this same tree, each as its own scoped PR rather than
revisiting this audit's original commits.

**Gap #10, second half — `cluster backup`/`cluster restore` (`7d529876`, docs fix
`c7fe951d`, PR #747).** `delonix cluster backup <name> [--to <path>]` runs `etcdctl
snapshot save` on `<name>-cp1` and streams the result back over SSH, base64-encoded,
inside ONE privileged command (save → encode → delete the remote temp file) — the
cluster's entire keyspace, Secrets included, never sits on the node's disk outside that
one command's lifetime, and is never read back by a second, separate connection, the
same class of bug `fetch_kubeconfig`'s own doc comment documents having once had for
`admin.conf`. `delonix cluster restore <name> --from <path>` is the destructive half:
mirrors Kubernetes' own documented etcd disaster-recovery procedure (stop the static
pod, preserve the old data directory, `etcdctl snapshot restore` under the EXISTING
member's own identity — name/initial-cluster/peer-urls/data-dir, read back from its own
manifest and parsed with a pure, unit-tested function rather than a remote `grep -oP`
round trip depending on PCRE support no node is guaranteed to have), and refuses by name
above a single `-cpN` control-plane (a correct multi-member restore needs a working
`--initial-cluster` for every OTHER member too, not attempted). Scoped to `etcd.mode:
stacked` only — `external` has its own PKI layout and is refused by name, never silently
attempted. **Neither verb was validated against a real cluster**: no live
VM-provisioned cluster was available in that session without reusing another session's
stopped VMs, which it declined to touch — stated explicitly in both commands' own doc
comments, not folded into a caveat. 1169/1169 `delonix-runtime-bin` tests pass (1164
before this fix, +5 new unit tests on the pure manifest-flag parser backing both verbs);
clippy, fmt, lang_ratchet, arch_fitness, `cargo deny`, the CLI leaf-baseline gate and
the exec-coverage ratchet all clean, with the latter's denominator honestly bumped
272 → 274 (two new leaves neither the battery nor that session could exercise live, so
the fraction reads 58.4% rather than falsely flat) rather than silently ignored.

**Gap #3, second half — `ContainerStatusResponse.info` (`46e3222a`, PR #750).**
`container_status` now accepts a `verbose` parameter, threaded through from
`ContainerStatusRequest.verbose` at the gRPC boundary exactly the way `status()` already
threads it for `StatusResponse.info`'s `capabilityCeiling` (the only existing precedent
in this crate for the `info`-map convention: a flat key, a plain string value, never a
JSON blob). When `true`, it reads the engine's own persisted record for `cri-<id>` via
the EXISTING `load_reconciled` helper — the same one `state`/`exit_code`/OOM-killed
already go through — and reports `info["userFallbackToRoot"]` as `"true"`/`"false"`,
never omitted (an absent key and an explicit `"false"` read very differently to a
kubelet logging `.info` on an unexpected restart). The cross-reference this section's
own §4.3 flagged as needing investigation turned out not to be at `StartContainer`: the
fallback decision happens inside the engine process that RPC shells out to
(`delonix __apirun`), after the handler has already returned, so the fact has to be read
back at `ContainerStatus` time, never captured at creation time — no change needed to
`StartContainer` or `ContainerRec`. Two new unit tests (fallback reported under
`verbose: true`, a container whose `USER` was honoured reports `"false"`, a non-verbose
request gets an empty `info` map either way, per the CRI contract), reverted locally and
confirmed to fail without the fix. 63/63 `delonix-cri` lib tests plus the crate's full
test suite (lib + both existing `grpc_status.rs` integration tests + `version_flag.rs`)
pass. **No new gRPC-transport integration test added**, stated rather than silently
skipped: the only surface this adds at that boundary is a 3-line pass-through
(`container_id`/`verbose` extraction) identical in shape to `status()`'s own, which
already has gRPC-transport proof; the substantive logic is covered, revert-verified, at
the lib level — the same bar the CLI half of this exact gap (`1e5562e6`) was closed
against.

### 4.12 Gap #4, remaining half closed in two later sessions — `f72a4f55`/`61e901bf` (PR #752), `724925a6` (PR #755)

§3's gap #4 itself named four fields contradicting ADR-0038's own "honour or refuse,
never ignore" decision text: `oom_score_adj` was fixed here (§4.5); `cpuset_mems`,
`unified`, `hugepage_limits` and `UpdateContainerResources` (ADR-0038's own items 3 and
4) were not — and §8 counted the gap as only half-fixed. Both were closed afterward, on
this same tree, each its own PR.

**Item 3 (PR #752, `f72a4f55`, e2e coverage `61e901bf`).** The CRI had no preflight for
ANY resource field, not even `cpuset_cpus` — the fields simply did not reach
`CriResources` at all, so a kubelet asking for a controller the leaf does not delegate
got a container reporting `Running` with the limit silently absent, the exact "accept
and ignore" shape this section's gap #1 (`--secret-files`) was fixed for, on a different
field. `CriResources` gained the three proto fields; `refuse_unenforceable_resources`
runs in `start_container`, before `StartContainer` answers success, checking the
controller the container is ACTUALLY about to land in — the engine's own
`leaf_controllers()` without a kubelet `cgroup_parent`, that parent's own
`cgroup.controllers` with one (ADR-0038 item 1's placement decides which; asking the
wrong one would refuse legitimate requests under a well-delegated parent, or accept
requests the kubelet's own parent cannot honour at all). The write side mirrors
`cpuset.cpus` across the three placement paths, with `AllowedMemoryNodes` added as a
systemd unit property and raw writes for `hugepage_limits`/`unified` (neither has a
systemd property). `cpuset_cpus` itself — wired into `apply_resources` since before this
gap was filed, with no preflight of its own — was closed in the same preflight,
alongside its sibling `cpuset_mems`. Live `crictl` coverage (6 checks, `scripts/e2e.sh`,
no root needed) proves the refusal, the field+controller naming, the escape hatch
(`DELONIX_ALLOW_UNENFORCED_LIMITS`), and the path-traversal guard on
`hugepage_limits[].page_size`/a `unified` key end to end, against a real `serve cri`.

**Item 4 (PR #755, `724925a6`).** `UpdateContainerResources` always answered `todo` —
the RPC the kubelet's in-place pod resize (KEP-1287) and `crictl update` both call.
`CriResources::merge_update` treats the request's zero/empty fields as "leave
unchanged", the OPPOSITE of `CreateContainer`'s own reading of the identical wire value
(there, zero means "no limit" — a new container has nothing to inherit; here, an update
is a PARTIAL request, and replacing the record wholesale would erase every other field
the container was created with the moment any one is touched). `cpu_quota`/`cpu_period`
merge as a pair, by design, rather than letting one half of an incomplete request pair
with the OTHER half's old value. The same `refuse_unenforceable_resources` check from
item 3 gates it first. `delonix_linux::update_kube_resources` is `update_limits` widened
to every item-3 field and placement-aware: `SetUnitProperties` (only the properties the
request actually gives — the rest stay as systemd already has them) under a kubelet
systemd scope, a direct cgroup write otherwise; `hugepage_limits`/`unified` always write
straight to the scope/leaf's cgroup (no systemd property either way), and
`oom_score_adj` writes `/proc/<pid>/oom_score_adj` from outside this time (a new sibling
of the init's own self-write). Validated live, non-systemd leaf path, real `crictl
update` against an isolated root: `memory_limit_in_bytes` and `cpu_quota`/`cpu_period`
landed on the SAME pid's live cgroup, and a second, memory-only update left `cpu.max`
untouched — the partial-merge guarantee this item exists for.

**Both share the same stated boundary** this section already uses elsewhere: the
systemd `SetUnitProperties` branch, for item 3's write side and item 4's update path
alike, needs a stable kubelet to measure for real and has unit/pure test coverage only —
the control-plane crash-loop investigated separately (and never reached by this audit)
still blocks that. With item 4 closed, ADR-0038's four decision items are now all
implemented; what remains for the ADR as a whole is exactly that end-to-end kubelet
validation, not new behavior.

## 5. Test results, exact

Two environments ended up involved, and the results below say which is which.
**Sandbox** (fixes 4.1–4.5, written assuming no root/KVM): Rust toolchain and dependency
versions as pinned in `Cargo.lock` at `1cbe9639`, `CARGO_TARGET_DIR` isolated to this
worktree throughout. **Real host** (§4.6's live validation): the same worktree's release
build, run against real KVM/libvirt with both `DELONIX_ROOT` and
`DELONIX_NET_RUNTIME_DIR` isolated from the host's own production state.

```
cargo fmt --check -p delonix-linux -p delonix-runtime-bin -p delonix-compute -p delonix-cri -p delonix-sdn
                                                                               → clean (0 diffs)
cargo clippy -p delonix-linux -p delonix-runtime-bin -p delonix-compute -p delonix-cri -p delonix-sdn
  --all-targets -- -D warnings                                                → clean
python3 scripts/lang_ratchet.py                                               → ok (3241 comments, 1050 identifiers, 117 user_text)
python3 scripts/arch_fitness.py                                               → ok (library_prints baseline raised 89→90, see 4.1's commit message for why; all else unchanged)
cargo test -p delonix-linux --lib                                             → 174 passed; 0 failed
cargo test -p delonix-runtime-bin --bin delonix cmd::cluster::                 → 47 passed; 0 failed
cargo test -p delonix-runtime-bin --bin delonix cmd::container::               → 92 passed; 0 failed
cargo test -p delonix-runtime-bin --bin delonix (full)                        → 1161 passed; 0 failed
cargo test -p delonix-compute --lib                                           → 84 passed; 0 failed
cargo test -p delonix-cri --lib                                               → 61 passed; 0 failed
cargo test -p delonix-sdn --lib                                               → 285 passed; 0 failed
cargo check --workspace --all-targets (repeated after every fix)              → clean, every time
cargo test --workspace --lib (full pass, all crates, after fixes 4.1–4.3)      → 25/25 crates "test result: ok"; 0 "test result: FAILED"
cargo test -p delonix-runtime-bin --bin delonix (full, after 4.7)             → 1161 passed; 0 failed (no new test — see 4.7)
cargo test -p delonix-runtime-bin --bin delonix (full, after 4.8)             → 1164 passed; 0 failed (+3 new)
cargo clippy -p delonix-runtime-bin --bin delonix -- -D warnings (after 4.8)  → clean (1 pre-existing needless-borrow warning on an
                                                                                  unrelated line this fix's code sits next to, fixed
                                                                                  in the same commit — see 4.8's commit message)
python3 scripts/arch_fitness.py (after 4.7, 4.8)                              → ok, all baselines unchanged
```

Each fix's regression test was individually reverted and re-run to confirm it fails
without the fix (documented in the test's own comment and in §4 above), per this repo's
own "prova medida, não afirmada" discipline — never trusting that a new assertion catches
what it claims to without having watched it fail first.

**Live, on the real host** (§4.6): `cluster kubeadm --control-plane 1 --workers 0` with
this build → `kubectl get nodes` reports `Ready` (confirmed against the live kubeconfig,
not the CLI's own claim); CoreDNS pods carry real pod-subnet IPs
(`10.244.0.2`/`10.244.0.3`); `wait_for_cluster_ready` returns `Ok(true)` in 0.3s. This is
the one "not validated live" caveat from the first version of this report that is now
resolved. Gap #13 (§4.7) was itself found and diagnosed live, by hand, on this same test —
only the automated `install_cli` code path that now does that replacement is unvalidated
live. The remaining fixes (4.1, 4.3, 4.4, 4.5, 4.8) stay sandbox-proven only, each for the
reason its own subsection states.

## 6. Contracts prepared for future PaaS integration — status, not new work

No endpoint was added to `delonix-node-api` in this pass (adding RPCs to a contract whose
current coverage (`NetworkService` leaves/writes, `VolumeService` leaves, `OperationService`)
is the ONE thing this audit measured carefully is a decision for whoever owns the P5 phase
of ADR-0040, not something to improvise under an unrelated task). What this audit confirms
and leaves written down, for whoever does that work next:

- The async-operation contract (`operations::begin/finish/replay`) is production-quality
  and already handles exactly the case the brief's §9 asks for ("consulta do estado após
  timeout antes de repetir operações") — it just needs to be the mechanism EVERY mutating
  RPC uses, not 2 of 22.
- `GetHealth`/`GetCapacity`/`WatchEvents` on `NodeService` are `UNIMPLEMENTED`, with the
  ADR-0042 phase that is supposed to bring them named in the response itself — not a silent
  404.
- `ListProviders` (ADR-0050) is the one RPC that is genuinely done: capability discovery by
  provider, generated from `docs/providers/capability-matrix.md`, with a test tying the two
  together so they cannot diverge.

## 7. Operation, diagnosis, and recovery guide (for what this pass touched)

- **A `cluster apply`/`cluster kubeadm` run that reports "bootstrapped — not all nodes
  Ready"**: the cluster exists and `kubeadm join` succeeded everywhere; CNI did not fully
  converge (multi-node, where this pass did not wire an automatic CNI) or is still pulling
  images (single-node, transient). Run `delonix cluster <name> kubectl get nodes` to see
  which; for multi-node, apply a CNI manifest by hand
  (`KUBECONFIG=/etc/kubernetes/admin.conf kubectl apply -f <cni>.yaml` on the
  control-plane) — this is unchanged from before this pass, now just honestly reported
  instead of masked as "ready".
- **A `cluster kubeadm --control-plane 1` run that reports "cluster ready"**: for the
  first time, this is actually true for the single-node case — the bridge CNI was applied
  and observed converged, not just assumed.
- **A `container run`/CRI `CreateContainer` that fails with "failed to confine secret
  files to tmpfs"**: the tmpfs mount at `/run/secrets` could not be established on this
  host. The container intentionally never started — check `dmesg`/the kernel's mount
  rejection reason; this is new behavior (previously the container started anyway, with
  secrets silently on disk).
- **`container describe`/`inspect` showing `User: 0 (root) — FALLBACK: ...`**: the image
  declares a non-root `USER` that this host cannot honour (no subordinate uid/gid range —
  `ls -la /etc/subuid /etc/subgid` for the account running the engine). The container is
  running as root despite the image's intent; this was always true, it is now visible.
- **`cluster destroy <name>` on a VM-provisioned cluster**: now works in one call — it
  removes every `<name>-cp<N>`/`-w<N>`/`-etcd<N>`/`-lb` VM, the cached kubeconfig, and the
  matching `~/.kube/config` entries. It does NOT remove the `--network` the cluster used
  (`network rm <net>` by hand, once nothing else is using it); a repeated `cluster destroy`
  on a name with no matching VMs falls through to kind-mode's own destroy, which correctly
  reports "no such cluster kind" if there is no container-based cluster by that name either.
- **A node provisioned from an older golden image reporting a kubelet/CRI symptom a
  commit comment in this repo says is already fixed** (e.g. CoreDNS `setuid` failures): the
  on-node `delonix` CLI may predate the fix even though `delonix-cri` does not. Re-run
  `cluster kubeadm`/`cluster apply` against the node — `prepare_host` now replaces a stale
  CLI the same way it already replaced a stale CRI (§4.7). If the symptom was already
  present on a node this fix was never run against, replacing `/usr/local/bin/delonix` by
  hand and re-creating the affected pods is the same diagnostic step this session itself
  used to confirm the root cause.

## 8. Limitations, blockers, and remaining risk — stated plainly

- **Fixes 4.1, 4.3, 4.4, 4.5, 4.8 are sandbox-proven only — fix 4.2's single-node case is
  now confirmed on real KVM/libvirt** (§4.6), which is the one gap this originally said
  needed "a host that can run one," and fix 4.7's underlying mechanism (replacing the
  on-node CLI) was proven live, by hand, before the automated code path existed. Each
  remaining "not validated live" note in its own subsection above still stands, each for
  its own stated reason (a mount failure needing `CAP_SYS_ADMIN`; a process-wide procfs
  write unsafe to exercise in a parallel test runner; `netops` functions with no injectable
  seam to a live holder; a teardown already proven by hand, not yet automated through the
  new code path).
- **5 of the 12 originally-ranked gaps are not fixed by this pass — but all five were
  reviewed with evidence, not left on the earlier one-line guess (§4.10).** Of the twelve,
  six have a behavior fix: #1, #2 (single-node), #3, #4 (`oom_score_adj` half), #8, and
  #10 — **#3, #4 and #10 closed in full** in later sessions on this same tree (§4.11: #3's
  CRI/`ContainerStatus` half, `46e3222a`; #10's `etcdctl snapshot` backup/restore half,
  `7d529876`; #4's remaining `cpuset_mems`/`unified`/hugepages half and
  `UpdateContainerResources`, ADR-0038's own items 3 and 4, `f72a4f55`/`61e901bf`/
  `724925a6` — see the gap's own entry above), all stated as not validated against a
  real cluster/kubelet, same as this section's own earlier fixes. Gap #6 (§4.9) got real work — a precise diagnosis of
  exactly why the obvious fix cannot
  land without an owner decision, and doc-comment hardening in the meantime — but no
  behavior changed, so it stays counted as open. The live-only gap #13 (stale CLI on
  golden images) is also now fixed (§4.7). The five that remain untouched each have a
  specific, measured reason, not a restated assumption: the admission warning event (gap
  #5) requires adding a new case to `delonix-security-runtime`'s `Outcome`/`Category`
  taxonomy, an architectural decision inside a crate this audit does not own; gap #7 (DNS
  cleanup, ADR-0064 D6) has zero existing credential/client scaffolding and needs a live
  DNS server this session cannot reach, a precondition the ADR itself states; gap #9
  (caller identity) is structurally a non-issue today — the socket's own accept path
  refuses any peer whose uid differs from the server's own, confirmed at
  `delonix-node-api/src/lib.rs:134` — so there is nothing to record that would carry
  information yet; gaps #11/#12 (RPC and error-contract completeness) were re-measured
  (8 of 59 RPCs have an explicit `UNIMPLEMENTED` handler, the rest unmatched by the
  router; `ErrorDetail` appears in exactly three files, all on the async path) and both
  numbers hold precisely — both are still the P5-phase owner's call, per §6's own
  reasoning, not a gap sized for a few RPCs grown under an unrelated audit.
- **The multi-node KaaS path is unchanged and still silently `NotReady`.** The fix in §4.2
  deliberately does not touch it (no `--cni` escape hatch exists yet on `cluster kubeadm`
  to add a safe refusal without breaking the documented HA example) — this is the single
  largest remaining KaaS gap, and closing it for real needs either a vendored, tested CNI
  with cross-node routing (kindnet/Calico/Flannel) or a `--cni` flag plus an explicit
  refusal, neither of which this pass attempted blind.
- **The live validation's secondary observation (control-plane flapping after the CLI
  swap) is reported as inconclusive, not as a finding** (§4.6) — it was not isolated from
  the confound of a minimal 2 vCPU/2 GiB VM under concurrent `kubectl`/`ssh` load from this
  same session, and calling it a regression without isolating that would be exactly the
  kind of unmeasured claim this report tries not to make.
- **Nothing here was validated against the `delonix-paas` consumer**, by design — this
  audit's scope is the Runtime only, per the brief's own instruction and this repo's
  standing doctrine.
