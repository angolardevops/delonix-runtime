# ADR 0038 — On the CRI path, the kubelet owns resource policy; the engine is the mechanism

**Status:** Accepted 2026-09-15 — implementation gated on the placement spike (Decision, «Order»)

## Context

Measured on 2026-09-15, on a real kubeadm node (`delonix-vm-k8s:1.36`, Kubernetes 1.36.4,
`delonix-cri` as the kubelet's runtime), and with `crictl` against `delonix serve cri`:

1. **`linux.resources` was read by nobody** until #309. A pod asking for 48 MiB ran with the
   engine's default ceiling (6646 MiB on that host). Fixed: explicit limits now reach the
   container and were verified in the kernel (`cpu.max = 400000 100000`,
   `memory.max = 3221225472` on the control-plane containers).
2. **A pod with NO limits still gets the engine's house ceiling** — a quarter of the engine's
   memory budget and at most one core (`default_memory_max`/`default_cpus`). On the 4-vCPU,
   4 GiB node every control-plane container ran at `cpus 0.85`, `memory_max 830M`; etcd and the
   apiserver were measurably throttled at startup (12 of 72 CFS periods, 1.67 s in ~7 s).
3. **The sandbox's `cgroup_parent` is ignored.** The kubelet sends one on every
   `RunPodSandbox` (`kubepods/burstable/pod<uid>` or the systemd-slice form), and the engine
   puts every container under its own `delonix.slice` instead. Everything Kubernetes builds on
   that hierarchy — Node Allocatable enforcement (`--enforce-node-allocatable=pods`,
   `system-reserved`, `kube-reserved`), the QoS classes, pod-level limits and pod overhead — does
   not apply to the containers this runtime starts.
4. **The kubelet's eviction manager cannot get pod stats** (`stat failed on
   /var/lib/delonix/containers/cri-<id>`), so node-pressure eviction — the last line of node
   protection — is not working either.
5. `UpdateContainerResources` is `todo`, and `cpuset_cpus` is silently not applied where the
   `cpuset` controller is not delegated (measured in a rootless session: `nproc` 32 inside a
   container that asked for `0-1`).

Points 2 and 3 are one problem seen from two sides. The engine's house ceiling exists because,
standing alone, nothing else stops one unlimited container from taking the host
(`default_memory_max`'s own comment). On a Kubernetes node something else does — the kubelet —
but only if the runtime places containers where the kubelet enforces its policy. Today the
runtime does neither half: it ignores the kubelet's hierarchy and substitutes its own ceiling,
which is wrong in both directions (too tight for etcd, and not the node budget the operator
configured).

What the reference is. The CRI's `LinuxContainerResources` defines `0`/empty as «not
specified». containerd and CRI-O translate an unspecified limit to no limit in the container's
cgroup and place the container under the sandbox's `cgroup_parent`; node protection is the
kubelet's job (Node Allocatable on the `kubepods` cgroup, QoS `oom_score_adj`, eviction). In-place
pod resize (KEP-1287) drives `UpdateContainerResources`, and the CPU and memory managers use it
for `cpuset`.

Guardrails this touches: **no silent failure** (points 1, 4, 5 are exactly that); **spike before
a new privilege boundary** (writing into a cgroup hierarchy the engine does not own is one);
**no tenancy** (the parent path is opaque — the engine does not learn what a pod or a QoS class
is, the same shape as ADR 0015). Daemonless is unaffected: `delonix-cri` is already the long-lived
endpoint the kubelet talks to, and nothing here adds a process.

## Decision

On the **CRI path only**, the engine stops being a policy engine and becomes the mechanism for
the kubelet's policy. The standalone CLI keeps its house ceiling unchanged.

1. **Place containers under the sandbox's `cgroup_parent`.** Both kubelet cgroup drivers:
   `cgroupfs` (`/kubepods/burstable/pod<uid>`) and `systemd` (`kubepods-burstable-pod<uid>.slice`,
   expanded to its slice path). The path is a privilege boundary: it must resolve under the
   cgroup2 mount, contain no `..`, and must NOT be the root or the engine's own base. A parent
   that fails validation is **refused at `RunPodSandbox`**, never silently replaced by
   `delonix.slice`.
2. **An unspecified limit is no limit — only when (1) holds.** Inside the kubelet's hierarchy the
   node is protected by Node Allocatable and eviction, so `memory.max`/`cpu.max` are `max` when
   the pod did not ask. Without a `cgroup_parent` (a bare `crictl`, a misconfigured kubelet), the
   engine keeps its house ceiling — removing it there would remove the only ceiling that exists.
3. **Honour or refuse, never ignore**: `oom_score_adj` (the QoS OOM ordering), `cpuset_cpus`/
   `cpuset_mems`, `unified`, `hugepage_limits`. Where a controller is not available, the
   container is refused with the reason, as #307 does for memory and CPU.
4. **Implement `UpdateContainerResources`** on top of the existing `update_limits`.
5. **Fix the stats the eviction manager reads**, so node-pressure eviction works.

Order: a spike first for (1) — create a container under a kubelet-shaped parent with `crictl`,
and read back `cgroup.procs` and the limits in the kernel — then (1)+(2) together (never (2)
alone), then (5), (3), (4).

## Alternatives considered

- **Keep the house ceiling for CRI pods.** Rejected: contradicts the CRI's own definition of an
  unspecified limit, throttles control-plane components measurably, and is not the node budget
  the operator configured in the kubelet.
- **Remove the ceiling without honouring `cgroup_parent`.** Rejected: on today's code it removes
  the only ceiling a BestEffort pod has, with Node Allocatable still not covering it. This is
  why (2) is gated on (1).
- **A Delonix-side node budget knob for CRI pods.** Rejected: a second policy engine next to the
  kubelet's, which would disagree with it the first time an operator changes `system-reserved`.
- **Do nothing until a kubelet stays up on this image.** Rejected as a reason to wait on the
  decision (the control-plane crash-loop predates this work and is investigated separately), but
  it IS why end-to-end validation of (1)-(2) with a stable cluster is not claimed here.

## Consequences

- Kubernetes QoS, Node Allocatable, pod overhead and in-place resize start to mean on a Delonix
  node what they mean elsewhere.
- The CRI path gains a validated write into a cgroup hierarchy the engine does not own — needs
  the spike and a `delonix-runtime-sec` pass before merge.
- Two semantics for «no limit» coexist, deliberately: CLI = house ceiling; CRI under a kubelet
  parent = unlimited. Documented where each is decided, not discovered.
- Known limitation until the crash-loop is fixed: validation of the placement and eviction
  behaviour under a real, stable kubelet cannot be claimed.
