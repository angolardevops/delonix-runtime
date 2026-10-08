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
3. **CaaS — the ADR-0062 root-fallback is invisible to any external consumer.** `FIXED,
   CLI half` (§4.3). A Pod that declares `runAsNonRoot` can be running as root, with
   nothing in `ContainerStatus`/`inspect`/logs to show it.
4. **KaaS — `oom_score_adj`/`cpuset_mems`/`unified`/hugepages silently dropped on the CRI
   path.** `oom_score_adj` `FIXED` (§4.5); `cpuset_mems`/`unified`/hugepages still not —
   contradicts ADR-0038's own decision text. See §8.
5. **CaaS — multi-tenant admission is open by default, with zero warning event.** `NOT
   FIXED` — deliberate for the engine's documented single-tenant use, but a CaaS layer that
   forgets to configure both `RuntimePolicy` and `DELONIX_CRI_CAP_CEILING` gets node
   compromise with no structural signal. See §8.
6. **NaaS — Proxmox-native vnet firewall is accepted and read back but does not actually
   filter** under the Proxmox default (datacenter firewall off). Not yet reachable by any
   `kind:`/CLI, so dormant — the same "public, dead, bug waiting for its first caller"
   pattern this repo has already catalogued five times.
7. **NaaS — stale DNS on Proxmox VM teardown/rename** can point to an address later
   reassigned to a different workload (ADR-0064 D6, decided, not implemented).
8. **NaaS — asymmetric rollback in `netops::remove`** can leave an orphaned `NetDef` that
   blocks future creates with a false `NetworkPrefixConflict`. `FIXED` (§4.4).
9. **Contracts — no request anywhere carries caller identity.** `ListOperationsRequest`
   is globally unscoped. Not an active exploit today (single trusted local agent per the
   `SO_PEERCRED` model) but a real gap the moment more than one caller shares a node.
10. **KaaS — no teardown, no backup/restore for a VM/SSH-provisioned cluster.** Destroying
    one today means manually `vm rm`-ing every VM; there is no `etcdctl snapshot` wiring
    anywhere. **Confirmed live, the hard way** (§4.6): this audit's own test VM had to be
    torn down by hand (`vm rm`, `network rm`, three `kubectl config delete-*` calls) for
    exactly this reason.
11. **Contracts — 39 of 59 RPCs unimplemented**, Container/Pod/VM/Stack/Image with zero
    served verbs — the PaaS cannot delegate to the contract for almost anything yet.
12. **Contracts — structured errors (`ErrorDetail`) only exist inside `Operation.error`**,
    never on the synchronous error path, which is where most real errors actually surface.

**Gap #13, found live and not in the original ranking** (§4.6): `cluster kubeadm`/
`cluster apply` upgrade `delonix-cri` on a node but never the `delonix` CLI itself, and the
CRI's `__apirun` re-exec target resolves whatever `delonix` is on the node's `PATH` — so a
node provisioned from an old golden image runs brand-new CRI protocol handling on top of
however old the baked-in CLI is, for every actual container spawn. This is a real,
security-relevant correctness gap (it silently defeats any fix landed in the CLI/engine
code after a golden image was built, ADR-0062's own root-fallback ordering fix included —
measured live, reproducing the exact crash that fix's own commit comment describes as
closed) that none of the four sandbox-only matrices could have found, because finding it
required a node old enough to be out of sync with its own CRI.

## 4. Corrections implemented, with regression tests

Three fixes landed, in priority order (security/data-loss first, per the brief's own
§12). Each is its own commit on `auditoria/naas-kaas-caas`, each passed
`cargo fmt --check`, `cargo clippy -D warnings`, `python3 scripts/lang_ratchet.py`,
`python3 scripts/arch_fitness.py`, and the pre-commit hook's
`cargo check --workspace --all-targets` before being committed. A final
`cargo test --workspace --lib` after all three (see §5) passed with **zero** failures
across all 25 library crates.

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

**Deliberately not done**: the CRI's own `ContainerStatus` still cannot show this. CRI
containers are tracked in a separate `ContainerRec`/store
(`crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs`), distinct from
`delonix-compute::Container`, and `ContainerStatusResponse.info` (the CRI spec's
sanctioned free-form debug extension, requested via `verbose: true`) is currently always
empty. Wiring the flag through needs understanding exactly how `StartContainer`
cross-references the two stores for a given id — real work, not something to guess at
without a kubelet in this sandbox to validate the result against. Named as a scoped
follow-up rather than attempted half-blind.

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
were before this test began.

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
resolved — the other four fixes (4.1, 4.3, 4.4, 4.5) remain sandbox-proven only, for the
reasons each one's subsection states.

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

## 8. Limitations, blockers, and remaining risk — stated plainly

- **Fixes 4.1, 4.3, 4.4, 4.5 are sandbox-proven only — fix 4.2's single-node case is now
  confirmed on real KVM/libvirt** (§4.6), which is the one gap this originally said needed
  "a host that can run one." The other four's "not validated live" notes in their own
  subsections above still stand, each for its own stated reason (a mount failure needing
  `CAP_SYS_ADMIN`; a process-wide procfs write unsafe to exercise in a parallel test
  runner; `netops` functions with no injectable seam to a live holder).
- **8 of the 12 originally-ranked gaps, plus the live-only gap #13, are not fixed by this
  pass.** Landed: #1, #2 (single-node), #3 (CLI half), #4 (`oom_score_adj` half), #8. Not
  landed, each for a stated reason: the admission warning event (gap #5) requires adding a
  new case to `delonix-security-runtime`'s `Outcome`/`Category` taxonomy, which is an
  architectural decision inside a crate this audit does not own — flagged for its actual
  owner rather than improvised. Gaps #6, #7, #9–#12 are each real engineering work (a
  second provider-firewall path, DNS lifecycle tied to VM teardown, a contract-wide
  identity field, 39 RPCs, a sync-path error model, a destroy verb for VM-provisioned
  clusters) that did not fit a single session on top of the four-domain inventory, the five
  landed fixes, and the live validation. Gap #13 (stale CLI on golden images) was found,
  not fixed — the right fix (does `cluster kubeadm`'s host-prep upgrade the CLI the same
  way it already upgrades `delonix-cri`? does the CRI's re-exec need to pin an explicit
  path instead of trusting `PATH`?) deserves its own session, not a reaction under time
  pressure to something just discovered.
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
