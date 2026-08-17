# ADR 0015 — An intermediate cgroup level with an aggregate ceiling

**Status:** Accepted (implemented)

## Context

Every limit this engine applies is **per container**: `memory.max`, `cpu.max` and `pids.max`
land on the container's own leaf, `<delegated base>/dlx-<id>`. That is the right unit for
"this workload may not exceed X", and it is structurally unable to answer a different
question: *what may these N workloads hold **together**?*

Ten containers of 1 GiB each are ten valid containers and 10 GiB of pressure. Nothing in a
per-leaf limit notices, because nothing is wrong per leaf. Whoever groups workloads — a PaaS
billing a customer, a CI runner fencing a job, an operator carving a box into shares — needs
the kernel to hold the aggregate, and the only construct that does that is a cgroup **above**
the leaves.

Measured on a host running this engine: eleven containers, each with `memory.max = max`,
holding 4.89 GiB between them. Every one of them individually within its (absent) limit.

The obvious way to fix this is also the wrong one: teach the engine what a tenant is. That
violates a guardrail this repo has held since the beginning — the engine is independent of
tenancy, licensing and billing (`delonix-runtime-core`'s own header says so, and
`delonix-core` depends on this engine, never the reverse).

## Decision

`Container` gains an optional `cgroup_parent: Option<CgroupParent>`: one **intermediate cgroup
level** between the delegated base and the leaf, with its own aggregate ceiling.

```
<delegated base>/<group>/dlx-<id>     instead of     <delegated base>/dlx-<id>
```

`CgroupParent` carries a `name` plus optional `memory_max`, `cpus` and `pids_max`. The engine:

1. creates `<base>/<name>` and nests the leaf under it;
2. writes the group's ceiling **before** the container's process enters, so the limit holds
   from the first allocation — the same reasoning as the per-leaf limits;
3. enables the controllers on the group so the leaves keep their own limits;
4. `None` changes nothing: the leaf hangs off the base exactly as before.

**The engine does not learn what the group means.** The name is opaque. `delonix-core` maps its
notion of tenant onto it; a CI runner could map a job id; an operator a department. This is the
same shape as ADR 0003's tenancy-free capability model, and for the same reason.

Three details are decisions, not implementation noise:

- **`memory.swap.max = 0` travels with the memory ceiling.** Measured: a group capped at 64 MiB
  let a single process allocate 200 MiB and finish, because the pages went to swap. Only with
  swap closed did the kernel enforce it (`memory.events` showed the ceiling hit 1466 times,
  then `oom_kill 1`). A memory quota that swap walks around is not a quota. The container leaf
  already had this fix; the group needs the same one.
- **An unsafe group name is dropped, not sanitised.** The name arrives from outside and is
  interpolated into a path; `..` climbs out of the delegated base into a cgroup this engine was
  never granted. `safe_cgroup_segment` rejects anything that is not a single `[a-z0-9._-]`
  segment. Silently *rewriting* a caller's name would be worse than refusing it: the caller
  would believe it had limited a group it never limited.
- **Best-effort application.** A group ceiling that cannot be written must not stop the
  container from starting. The corollary is that the caller cannot *trust* the ceiling without
  reading `memory.max` back off the group.

## Consequences

- A group of containers can be bounded by the kernel, which per-container limits cannot do.
  Proven live: two containers, leaves of 32 MiB each, group of 32 MiB; each wrote 20 MiB; the
  group's `memory.events` recorded 70 ceiling hits while **both** leaves recorded zero.
- The persisted `Container` grows a field. `#[serde(default)]` keeps every stored record
  deserializing, and `skip_serializing_if` keeps records without a group byte-identical.
- The manifest gains `cgroupParent` (camelCase, mirrored in `delonix-runtime-bin` so that
  `delonix-runtime-core` does not grow a `schemars` dependency). The published JSON Schema
  regenerates from the code, per ADR 0007.
- **`cgroupParent` is not a hot field.** Changing it means moving a running container to a
  different cgroup, which this engine does not do live. The reconciler reports it under
  `FieldsNotCompared` — the operator is told, and recreates. Silence would have been the bug.
- The group cgroup is **not** removed when its last container goes. An empty cgroup costs a
  directory; reaping it races with the next container of the same group. Left deliberately.

## Alternatives rejected

- **Teach the engine about tenants.** Breaks the guardrail, and would put billing semantics in
  a public engine that must stay usable by people who have no tenants at all.
- **Let the caller create the cgroup itself and pass a path.** The caller cannot: the leaf is
  created by the engine at clone time, and the ceiling has to exist *before* the first
  allocation. A caller writing limits afterwards is a caller writing them too late.
- **Enforce the aggregate in the control plane by arithmetic.** That is what `delonix-core`
  already does, and it is exactly the thing that fails the moment a workload escapes the
  accounting. Arithmetic in userspace is a budget; a cgroup is a ceiling.
