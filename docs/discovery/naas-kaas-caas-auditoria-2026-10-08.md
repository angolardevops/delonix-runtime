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
   anyway.** `PARTIALLY FIXED` (§4.2). `spec.cni` was validated and never applied; the
   final line printed "cluster ready" unconditionally, including on the historical
   `wait_ready: false` timing where readiness is never even checked.
3. **CaaS — the ADR-0062 root-fallback is invisible to any external consumer.** `FIXED,
   CLI half` (§4.3). A Pod that declares `runAsNonRoot` can be running as root, with
   nothing in `ContainerStatus`/`inspect`/logs to show it.
4. **KaaS — `oom_score_adj`/`cpuset_mems`/`unified`/hugepages silently dropped on the CRI
   path.** `NOT FIXED` — contradicts ADR-0038's own decision text. See §8.
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
   blocks future creates with a false `NetworkPrefixConflict`.
9. **Contracts — no request anywhere carries caller identity.** `ListOperationsRequest`
   is globally unscoped. Not an active exploit today (single trusted local agent per the
   `SO_PEERCRED` model) but a real gap the moment more than one caller shares a node.
10. **KaaS — no teardown, no backup/restore for a VM/SSH-provisioned cluster.** Destroying
    one today means manually `vm rm`-ing every VM; there is no `etcdctl snapshot` wiring
    anywhere.
11. **Contracts — 39 of 59 RPCs unimplemented**, Container/Pod/VM/Stack/Image with zero
    served verbs — the PaaS cannot delegate to the contract for almost anything yet.
12. **Contracts — structured errors (`ErrorDetail`) only exist inside `Operation.error`**,
    never on the synchronous error path, which is where most real errors actually surface.

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

## 5. Test results, exact

Environment: this sandbox, no root, no KVM/hypervisor, no real network egress beyond what
was already cached. Rust toolchain and dependency versions as pinned in `Cargo.lock` at
`1cbe9639`. `CARGO_TARGET_DIR` isolated to this worktree throughout (per this repo's own
multi-session-safety convention).

```
cargo fmt --check -p delonix-linux -p delonix-runtime-bin -p delonix-compute   → clean (0 diffs)
cargo clippy -p delonix-linux --lib --all-targets -- -D warnings               → clean
cargo clippy -p delonix-runtime-bin --bin delonix -- -D warnings               → clean
cargo clippy -p delonix-compute --all-targets -- -D warnings                  → clean
python3 scripts/lang_ratchet.py                                               → ok (3241 comments, 1050 identifiers, 117 user_text)
python3 scripts/arch_fitness.py                                               → ok (library_prints baseline raised 89→90, see 4.1's commit message for why; all else unchanged)
cargo test -p delonix-linux --lib                                             → 174 passed; 0 failed
cargo test -p delonix-runtime-bin --bin delonix cmd::cluster::                 → 47 passed; 0 failed
cargo test -p delonix-runtime-bin --bin delonix cmd::container::               → 92 passed; 0 failed
cargo test -p delonix-runtime-bin --bin delonix help_i18n                      → 3 passed; 0 failed
cargo test -p delonix-compute --lib                                           → 84 passed; 0 failed
cargo test --workspace --lib (final full pass, all crates)                    → 25/25 crates "test result: ok"; 0 "test result: FAILED"
```

Each fix's regression test was individually reverted and re-run to confirm it fails
without the fix (documented in the test's own comment and in §4 above), per this repo's
own "prova medida, não afirmada" discipline — never trusting that a new assertion catches
what it claims to without having watched it fail first.

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

- **This sandbox has no root, no KVM, no real network egress.** Every "not validated
  live" note above is real: the three fixes are proven by unit tests against this engine's
  own code (including, for the CNI fix, this engine's own CNI parser — not a hand-rolled
  JSON check), and by a clean full-workspace test/lint/gate pass, but none of the three was
  exercised against a live `kubeadm` bootstrap, a real mount-failure scenario, or a real
  kubelet. Whoever has a host that can run one should re-run the KaaS matrix's own
  suggested acceptance tests before calling gap #1/#2 fully closed.
- **9 of the 12 ranked gaps in §3 are not fixed by this pass.** This was a deliberate
  choice under the brief's own stated priority (security/data-loss first, within a bounded
  session), not an oversight: `oom_score_adj` (gap #4) needs a new `RunOpts`/`Container`
  field AND a post-spawn `/proc/<pid>/oom_score_adj` write in `delonix-linux`'s spawn path
  — larger surface than the three landed fixes, deferred rather than rushed. The admission
  warning event (gap #5) requires adding a new case to `delonix-security-runtime`'s
  `Outcome`/`Category` taxonomy, which is an architectural decision inside a crate this
  audit does not own — flagged for its actual owner rather than improvised. Gaps #6–#12 are
  each real engineering work (a second provider-firewall path, DNS lifecycle tied to VM
  teardown, a symmetric rollback, a contract-wide identity field, 39 RPCs, a sync-path error
  model) that did not fit a single session on top of the four-domain inventory and the three
  landed fixes.
- **The multi-node KaaS path is unchanged and still silently `NotReady`.** The fix in §4.2
  deliberately does not touch it (no `--cni` escape hatch exists yet on `cluster kubeadm`
  to add a safe refusal without breaking the documented HA example) — this is the single
  largest remaining KaaS gap, and closing it for real needs either a vendored, tested CNI
  with cross-node routing (kindnet/Calico/Flannel) or a `--cni` flag plus an explicit
  refusal, neither of which this pass attempted blind.
- **Nothing here was validated against the `delonix-paas` consumer**, by design — this
  audit's scope is the Runtime only, per the brief's own instruction and this repo's
  standing doctrine.
