# ADR-0078: A container group is honoured as root, and a VMM starts in a cgroup leaf with the VM's ceilings

- **Status:** Proposed — slice 1 written on a branch (root-mode group, the process leaf, the Cloud
  Hypervisor VMM in it); not yet built or measured live. libvirt session, disk/network rate
  limiters and queue counts are slice 2 (see «What this does not do»).
- **Date:** 2026-10-12
- **Deciders:** Walter Angolar (owner)
- **Relates to:** ADR-0015 (the intermediate cgroup level — this supersedes two of its details,
  named below), ADR-0038 (the CRI's kubelet hierarchy, the other place a container leaves
  `delonix.slice`), ADR-0068 (VM hotplug: the ceilings a leaf has to leave room for), ADR-0040
  (layer direction: why the leaf is reached through a port), ADR-0043 (the dictionary entries).

## Context

Two ceilings the engine accepted were not enforced, and nothing said so.

**1. A container group did not exist as root.** ADR-0015 gave `Container` an optional
`cgroup_parent`: a group level with an aggregate ceiling, so N containers can be held to a total
that no per-container limit expresses. It was implemented on the rootless delegated path only
(`try_delegated_base`). In root mode `setup_cgroup` used `Container::cgroup()`, the fixed
`delonix.slice/delonix-<id>`, and never read the group. A caller that runs every step of a job as
a container under one group, to cap the job, got no cap at all as root — with `rc=0`, a record
that carried the group, and nothing in the output.

On the same path, ADR-0015's two «details» were silent failures by design: an unsafe group name
was **dropped** (the container started with no group at all), and the group ceiling was written
**best-effort** with the caller's raw string (`64Mi`, which the kernel refuses, left no ceiling).

**2. A Cloud Hypervisor VM had no cgroup.** The VMM was launched (through the infra's `join` argv
and a `sh -c … &`) into whatever cgroup the launcher was in. The only protection was the
admission check on `MemAvailable`. Neither the VM's vCPU count nor its memory size was a
`cpu.max`/`memory.max` on the process that holds the vCPU threads and the guest's RAM, so a VM's
VMM threads, I/O threads and RAM overhead were bounded by nothing but the host.

Measured for the margin (Cloud Hypervisor v53.0, firmware boot, 2 vCPUs, no disk): the VMM's
resident set was ~7 MiB (2.8 MiB anonymous), the same with 256 MiB and 2 GiB of guest RAM, with
7 threads.

The guardrail at stake is «no silent failure: refuse before any effect». The other one is the
layer direction: a provider crate may depend only on the foundation and the contexts, and the
cgroup knowledge (which base is delegable, the root slice, which controllers to hand down) lives
in the `delonix-linux` adapter.

## Decision

**D1 — Root mode honours the group.** `Container::cgroup()` is `delonix.slice/<group>/delonix-<id>`
for a container of a group (`root_cgroup_path`, pure). Before the leaf, `setup_cgroup` creates
`delonix.slice/<group>`, writes its ceiling and hands `memory`, `cpu` and `pids` down to it
(`ensure_root_group`). Each of those is an ERROR when it fails — `DX-6102`
`container.cgroup_parent_unavailable` (class unavailable, exit 69) — because a group without its
ceiling holds nothing. Idempotent: the next container of the group rewrites the same files.
Because `cgroup()` itself changed, everything that reads the static root path (removal, the CRI's
statistics, `container_pids`) follows without a second edit.

**D2 — An invalid group is refused, in both modes, before any effect.** The name rule is
ADR-0015's (`safe_cgroup_segment`); the ceilings must parse (memory as a size, normalised to
bytes; CPU as cores; processes as a positive integer or `max`). Refused with `DX-1116`
`container.invalid_cgroup_parent`, at the top of `spawn` (nothing cloned, nothing written) and
again in `setup_cgroup`. **This supersedes ADR-0015's «an unsafe group name is dropped».** The
rootless group ceiling stays best-effort per write (ADR-0015's third detail is kept there), but
now writes the normalised values.

**D3 — A process leaf, for a process tree that is not a container.** The compute context gains
`cgroup_leaf`: `LeafLimits` (CPU quota, memory bytes, pids, optional `io.max` line), the pure
`limit_files` (what is written, and which writes are required), the name rule, and the handle
`ProcessLeaf` (`enter(pid)`, `enter_self()`, `events()` for `memory.events oom_kill` and
`pids.events max`, `of_pid`, `remove`). The adapter implements
`create_process_leaf(name, &LeafLimits) -> Result<Option<ProcessLeaf>>`: in root mode under
`delonix.slice`, rootless under the escape base `<user@uid.service>/dlx-containers` (the same base,
with the same aggregate ceiling, the containers use); `Ok(None)` when neither exists; a required
ceiling that does not land removes the leaf and is an error. A provider reaches it through the
`ProcessLeaves` port, registered by the composition roots (the CLI's `main` and the node API's
`compose`), the same shape as `VmNetwork`.

**D4 — The Cloud Hypervisor VMM starts inside its leaf, and a VM whose ceilings cannot be held is
refused.** The leaf `vm-<name>` gets `cpu.max` = the vCPU ceiling (`vcpus_max` when declared) in
whole cores, `memory.max` = the memory ceiling (`memory_max_mib` when declared) + 128 MiB for the
VMM, `pids.max` = the container default or `2 × vCPUs + 64`. The launcher's child enters the leaf
between `fork` and `exec` (`pre_exec`, a single `write(2)` on a descriptor opened by the parent),
so the VMM is born inside it and nothing it allocates is charged elsewhere. With no leaf — no port
registered, no delegated cgroup, a ceiling that did not land — the start is refused before the
network is touched: `DX-6515` `vm.limits_not_enforceable` (exit 69). The escape hatch is the one
the container limits already use, `DELONIX_ALLOW_UNENFORCED_LIMITS`, which starts the VM without a
leaf and logs a warning. `stop` removes the VM's own leaf (recognised by name from
`/proc/<pid>/cgroup` before the VMM goes).

**D5 — `DEFAULT_PIDS_MAX` is public** (`delonix_compute::DEFAULT_PIDS_MAX`, re-exported by
`delonix-linux`), so a caller that sizes a group against the per-container default names the
engine's number.

## Alternatives considered

- **Keep root mode flat and document it.** The record would keep claiming a group the kernel does
  not have. Rejected by the no-silent-failure guardrail.
- **Treat only an explicit VM limit as a request, and leave a VM without one unlimited.** A VM
  always declares its vCPUs and memory, and those are what a consumer sells and plans against;
  «no leaf» would have stayed the normal case on every host where the engine can create one.
  Rejected; the escape hatch covers the host that deliberately runs without.
- **Move the VMM into its leaf after it starts.** Memory a process already touched stays charged
  where it was (cgroup v2 does not migrate charges), and the VMM is a grandchild behind a `sh`.
  Entering in the launcher's child is earlier and cheaper.
- **Put the leaf code in the provider, or let the provider depend on `delonix-linux`.** The first
  duplicates the delegation knowledge (the escape base, the aggregate ceiling, the root slice);
  the second breaks the layer direction (`scripts/arch_fitness.py`). The port keeps one
  implementation.
- **A separate `delonix-vm.slice` beside `delonix.slice`.** VMs would escape the engine's
  aggregate budget that the slice exists to hold. Rejected; a VM counts against the same budget as
  a container.

## Consequences

- **Behaviour change, rootless:** a Cloud Hypervisor VM started where the engine has no delegated
  cgroup (an SSH session scope, say) is now REFUSED (exit 69) instead of starting unlimited. The
  remedies are in the message: root, a `Delegate=yes` scope, or the escape hatch.
- **Behaviour change, root:** a container whose group cannot be created or limited is not
  started (`DX-6102`); before, it started without the group.
- **Behaviour change, both:** an invalid group name or ceiling is refused (`DX-1116`) instead of
  dropped or written raw.
- A VM's VMM now counts against `delonix.slice` (root) or the rootless escape base: the container
  admission check, which reads the slice's `memory.current`, sees running VMs.
- `cpu.max` caps the VMM's own threads together with the vCPUs, so a VM saturating all its vCPUs
  and doing heavy I/O is throttled at its vCPU count, as a container is at `--cpus`.
- The group directory is still not removed when its last container goes (ADR-0015, kept).
- Not yet measured live: the root path with a group, the leaf under a real VMM, and the OOM of a
  VM that exceeds its ceiling. The `#[ignore]` tests in `delonix-linux/tests/cgroup_parent.rs`
  (root) and `tests/process_leaf.rs` (root or a delegated scope) are the proofs to run.

## What this does not do (slice 2)

- **libvirt session.** The QEMU process is forked by the session libvirt daemon, not by the
  engine, so there is no `fork`/`exec` window to enter a leaf in. The clean placement found so
  far is `virsh start --paused` → move the QEMU pid into the leaf → `virsh resume`: the guest has
  run nothing, but QEMU has already loaded the firmware into guest RAM charged elsewhere. Until
  that is measured, `strip_cgroup_tuning` keeps removing `<memtune>`/`<cputune>` with a warning.
- **Disk and network rate limiters, queue counts.** Cloud Hypervisor v53.0 accepts
  `bw_size`/`bw_one_time_burst`/`bw_refill_time` and `ops_size`/`ops_one_time_burst`/
  `ops_refill_time` on `--disk` and `--net`, a shared `--rate-limit-group`, and
  `num_queues`/`queue_size` on both. Adding them is a new VM field that `vm start` must rebuild
  from the record (a limit lost on restart is the same failure as one never applied), a manifest
  field (a spec change) and a CLI flag — not a contained change. Rootless has no `io` controller,
  so the VMM's rate limiter is the only disk ceiling a rootless VM can have.
- **OOM reporting for a VM.** `ProcessLeaf::events()` reads the counters; recording `OOMKilled`
  on the VM's record, the way the container supervisor does, needs a reader that outlives the VMM.
