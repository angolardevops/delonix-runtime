# ADR-0074: A `hostPort` the node cannot publish is refused by name, and the CNI path has to publish them

- **Status:** Proposed (2026-10-07). D1 and D2 are **implemented** (PR on `cri/hostport-proto`:
  `publishable_port_specs` refuses from `RunPodSandbox`). D3 is **decided and not built**: the
  owner stated on 2026-10-07 that the engine is rootless-first and not rootless-only, so the CNI
  `hostPort` path is to be implemented rather than refused, and SCTP publishing in root mode is a
  path and not a closed door. Both are sequenced after this PR, each needing a real kubeadm node.
- **Date:** 2026-10-07
- **Deciders:** Walter Angolar
- **Relates to:** the `delonix_sdn::Proto` authority and the recorded bind address (PR #718),
  ADR-0038 (the CRI puts containers in the kubelet's hierarchy; the `cgroup_parent` refusal this
  one is placed beside), ADR-0005 (`-o json` and the refusal classes), the engine's identity
  section in `AGENTS.md` («the engine validates its own contract: it never trusts a caller to
  refuse what it cannot itself do»).

## Context

A Kubernetes `hostPort` asks the node to publish a container port on the node's own address. The
engine has three network paths behind the CRI and they do not agree on whether that happens:

| path | what happens to a `hostPort` today |
|---|---|
| `hostNetwork` | nothing to publish; the container is in the host's REAL netns and binds the node's ports itself |
| rootless / native ingress | published — `add_hostfwd` on the shared `slirp4netns` plus DNAT in the holder |
| root / CNI | **not published at all, for any transport**, and nothing says so |

The third row is the guard in `run_opts_of`:

```rust
if !sb.host_network && sb.cni_netns.is_empty() {
    o.ports = sb.port_mappings.clone();
}
```

A CNI sandbox leaves host ports to the `portmap` plugin, and the `portMappings` never reach it as
`runtimeConfig`. So the pod reports `Running` with a port that does not exist.

Two facts measured on 2026-10-07, both against the real thing rather than read:

1. **The rootless datapath cannot carry SCTP.** `slirp4netns` 1.2.1 (libslirp 4.7.0), probed
   through its api-socket against a throwaway netns:

   ```
   proto=tcp   -> {"return":{"id":1}}
   proto=udp   -> {"return":{"id":2}}
   proto=sctp  -> {"error":{"desc":"bad request: add_hostfwd: bad arguments.proto"}}
   ```

   It is the dataplane, not the spec parser. The CNI `portmap` plugin takes `tcp` and `udp` as
   well, so no path publishes SCTP.

2. **`hostNetwork` really is the host's network.** A `--net host` container reports the same
   `net:[4026531833]` inode as the host and sees the node's own interfaces. So it binds the node's
   ports itself, whatever the transport — which is why refusing a `hostPort` there would break a
   case that works.

What the engine did with an SCTP `hostPort` before this ADR: `cri_port_specs` emitted the string
`"sctp"`, the sandbox was created, the spec reached the container, and `parse_publish_addr` refused
it at `StartContainer` with `invalid protocol in '8080:80/sctp'`. Late, after the sandbox existed,
naming the spec instead of the cause, on a manifest that is legal Kubernetes — and retried by the
kubelet forever on a condition that can never clear. The same function turned an unknown protocol
NUMBER into `"tcp"`: a guess about the one field whose job is to say what the traffic is.

## Decision

**D1 — The engine refuses a `hostPort` it cannot publish, from `RunPodSandbox`, by name.**
`failed_precondition`, beside the `cgroup_parent` refusal and for the identical reason: a refusal
the kubelet shows on the pod, not a sandbox left behind. The pod sits in `Pending` with the reason
as an event, which is where an operator looks.

Not a silent drop. A declared `hostPort` that is not published leaves the pod `Running` and the
service unanswerable — the failure class this engine has already had to remove three times
(`--network-alias`, `--security-opt seccomp=`, `-v …:z`: accepted and ignored, now refused). And a
warning would go to the `delonix-cri` journal, while the person who could act reads
`kubectl get pod`.

Not «leave it failing at `StartContainer`» either: that is the state described above, and a
refusal that can never be satisfied has to be stated once, at the earliest point, instead of
retried.

**D2 — The transport is asked of `delonix_sdn::Proto`, never decided in the CRI.** That enum is
the engine's single statement of what it can publish (`tcp|udp`, anything else refused at the
boundary). The CRI holding its own opinion is what produced both defects in one function. An
unknown protocol number is refused by number rather than guessed.

**Blast radius, and it is what makes D1 cheap:** a `containerPort` with no `hostPort` is already
dropped (`m.host_port > 0`), which is correct Kubernetes semantics — it is informational. An SCTP
**Service** goes through kube-proxy and never through a `hostPort`, so SCTP services are untouched.
Only `hostPort` + SCTP is refused, and no pod that works today starts failing: that combination
already failed, later and worse.

**D3 — the engine is rootless-FIRST, not rootless-only, so «it needs root» is not a reason to drop
a capability.** Stated by the owner on 2026-10-07: when a capability needs the privilege, it is
built behind **root mode** or **cgroup delegation**, as an explicit opt-in, the way `vm bridge` and
the `--device-*` limits already are. Three consequences for this ADR:

1. **A refusal says «not implemented», never «impossible».** Measured the same day, in a throwaway
   netns, the kernel accepts the engine's own DNAT for SCTP:

   ```
   sctp dport 5070 counter packets 0 bytes 0 dnat to 10.0.0.5:5070
   ```

   So SCTP publishing is reachable in root mode through the nftables the engine already writes for
   its ingress DNAT — neither `slirp4netns` nor `portmap` is in that path. D1 still refuses,
   because none of it is built; what changes is that the message, the doc comment and this ADR must
   not read as a closed door. A message that says «impossible» teaches the next reader to stop
   looking.
2. **The CNI `hostPort` gap is to be implemented, not refused.** Pass `portMappings` to the
   `portmap` plugin as `runtimeConfig`, which is what containerd does, and then refuse only what
   `portmap` itself cannot do. The alternative — refusing a `hostPort` in the CNI path until that
   exists — would make a pod that today runs-with-a-missing-port fail to start, on a node that
   could have working TCP `hostPort`s, and it buys nothing. Leaving it silent stays rejected for
   D1's reason: it is the same silence, only wider, because it affects every transport.
3. **Neither is bundled into the PR that implements D1 and D2.** Both need a real kubeadm node to
   measure — `portmap` is not even installed on the machine where this was written, so its
   `tcp|udp` limit is still stated from its upstream spec and not probed. They are sequenced after,
   each with its own measurement, not deferred for lack of privilege.

## Consequences

- An SCTP `hostPort` now fails at admission with a message naming the port, the reason, and the two
  ways out (a Service, or `hostNetwork`). It used to fail at container start, misattributed.
- An unknown protocol number fails instead of being published as TCP.
- `hostNetwork` sandboxes no longer store port specs they never read. The field is consumed in one
  place only (the guard quoted above), so storing a spec the engine cannot parse was a landmine for
  whoever removes that guard.
- The CNI `hostPort` silence stays open until D3 is decided. It is named here so the next reader
  finds it as a known gap rather than discovering it the way this one was discovered — by measuring
  a guard while fixing something else.

## What was not measured

A real kubeadm node was not available for this change: the refusal is proved by unit tests and by a
gRPC round-trip against the real server over a unix socket (the kubelet's own transport), not by a
kubelet scheduling an SCTP pod. `portmap` is not installed on the machine where this was written, so
its `tcp|udp` limit is stated from its upstream spec and not probed — D3.2 must measure it before
it is built.

The SCTP DNAT of D3.1 was loaded into a kernel, in a throwaway netns, which proves the ruleset is
accepted. It does **not** prove a packet is translated: that needs an SCTP client and server across
the rule, and the `sctp` module is not even loaded on this host. So «reachable in root mode» is a
measured ruleset, not a measured datapath, and whoever builds it starts by closing that gap.
