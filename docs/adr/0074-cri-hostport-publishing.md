# ADR-0074: A `hostPort` the node cannot publish is refused by name, and the CNI path has to publish them

- **Status:** Proposed (2026-10-07). **D1, D2 and D3 are all implemented** on
  `cri/hostport-proto`: the refusal is per path, and the CNI chain publishes `hostPort` through
  `portmap`'s `runtimeConfig`. Measured in a lab VM from this repo's own
  `delonix-vm-k8s:1.36` golden, against both binaries: `origin/main` created the sandbox with
  **0** DNAT rules and a port that answered nothing; with the change, **3** rules, `LAB-OK` over
  the host port, and 0 rules after `rmp`.
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

   It is the dataplane, not the spec parser — and it is the ROOTLESS dataplane only. **Corrected
   the same day, by measurement**: this ADR first claimed the CNI `portmap` plugin «takes tcp and
   udp as well», from its upstream spec and flagged as unmeasured. It is false. `portmap` from
   `kubernetes-cni` writes
   `-p sctp -m sctp --dport 31070 -j DNAT --to-destination 10.244.0.2:5070`, and with the `sctp`
   module loaded a real SCTP client on the node reached a server inside the pod netns through it.
   So the root/CNI path publishes SCTP end to end, and only the rootless slirp cannot.

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

**D1 — The engine refuses a `hostPort` it cannot publish, from `RunPodSandbox`, by name — and the
judgement is PER PATH.** A blanket answer was wrong, and measuring is what showed it: the rootless
slirp cannot carry SCTP, the CNI chain can, and `hostNetwork` publishes nothing at all. One
refusal for all three would have closed a door that is open in root mode.
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

1. **A refusal says «not implemented», never «impossible»** — and the first version of D1's message
   got that wrong, which is what sent us measuring. It said the node «has no sctp publish path»,
   reading as a permanent limit. Two measurements undid it: the kernel accepts the engine's own
   `sctp dport 5070 … dnat` in a throwaway netns, and `portmap` publishes SCTP for real (above). A
   message that says «impossible» teaches the next reader to stop looking.
2. **The CNI `hostPort` gap was implemented, not refused** — and it turned out to be ONE piece of
   work rather than two, because publishing through the chain delivers tcp, udp AND sctp at once.
   `portMappings` now travels as the plugin's `runtimeConfig`, which is what containerd does. The
   capability argument is injected into the CONFLIST, which is what makes both CNI paths need no
   new plumbing: the root path hands the list to `attach_named_netns`, the rootless path
   hex-encodes the same JSON onto the holder's control line (whose shape does not change), and
   storing the result as the sandbox's conflist gives the `DEL` the identical config back.
   A chain that declares no `portMappings` plugin is refused by name, with the fix — attaching
   anyway was the silence this removes.
3. **What stays refused, and where.** Only the rootless native ingress refuses a transport now,
   and only the one its `slirp4netns` cannot carry. `hostNetwork` judges nothing.

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

**No kubelet.** Everything here was driven by `crictl` — the Kubernetes project's own client, over
the same transport a kubelet uses — and by unit tests and a gRPC round-trip. A kubelet scheduling
these pods, with its retries and its own view of pod readiness, was not exercised.

**The SCTP `hostPort` is proved on the CNI path, not on the engine's own nftables.** The lab
measured `portmap` translating a real SCTP association. The other route — the engine writing its
own `sctp … dnat` in root mode, which ADR-0074 D3.1 names — was only loaded into a kernel as a
ruleset; no packet crossed it. Whoever builds that route starts by closing that gap.

**One node, one chain.** The lab ran a single-node `bridge` + `portmap` chain. A real cluster CNI
(Calico, Cilium) declaring the capability differently, and more than one node, were not measured.
