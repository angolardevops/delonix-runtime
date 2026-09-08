# ADR-0034: A real CSI driver needs a persistent daemon — that's a philosophy decision, not a storage-driver decision

- **Status:** Proposed
- **Date:** 2026-09-06
- **Deciders:** Walter (owner)
- **Related:** `docs/runtime/runtime-architecture.md` §"Storage Engine" ("CSI specifically edges
  toward orchestrator concerns — scope carefully in an ADR" — this is that ADR), ADR-0009
  (TrueNAS provisioner, the network-storage precedent this repo already shipped), ADR-0010
  (remote management API — Rejected, same "belongs to a different layer" shape), the "daemonless
  by design" guardrail this project treats as non-negotiable.

## Context

The Container Storage Interface (CSI) is the standard Kubernetes uses to let a runtime/node
provide dynamically-provisioned, mountable storage to pods, replacing in-tree volume plugins.
Grep confirms: **zero occurrences of CSI in this codebase beyond comments** — it is a real,
named gap in `docs/runtime/runtime-architecture.md`'s own Storage Engine row.

**What CSI actually requires, structurally, and why it's not a small addition:**

- A CSI driver ships (at minimum) a **Node plugin**: a gRPC server listening on a Unix socket that
  kubelet registers with via the **kubelet plugin registration mechanism** — kubelet watches a
  well-known directory (`/var/lib/kubelet/plugins_registry`) for a registration socket and expects
  the driver's OWN socket to be reachable for the lifetime of every pod that might mount a volume
  through it. This is not a request-response call this engine can serve on demand — it is a
  standing service kubelet assumes is always there.
- A **Controller plugin** (`CreateVolume`/`DeleteVolume`/`ControllerPublishVolume`) is typically a
  separate process (often the only replica, via a `StatefulSet` with leader election in real
  clusters) — again a persistent service, not a one-shot CLI invocation.
- Both are near-universally deployed as **DaemonSet/Deployment pods with a sidecar stack**
  (`node-driver-registrar`, `external-provisioner`, `external-attacher`, `livenessprobe`) — the
  ecosystem's own tooling assumes a long-running process model.

**This is the same conflict `docs/adr/0031-live-vm-migration-no-go.md` found for libvirt's NBD
migration path** — "needs a persistently-listening daemon this engine's rootless identity
rejects." CSI is not a NEW instance of an old problem; it is the SAME problem, in the storage
domain instead of the migration domain. The engine's own architecture doc names the tension
directly: CSI "edges toward orchestrator concerns."

**What already covers the practical need, without a daemon.** `kind: Volume` already provisions
network storage — NFS/CIFS/WebDAV mounts (`storage.rs`, `share_mount`/`ensure_mounted`) — and
ADR-0009 shipped a real TrueNAS provisioner (dataset + quota + export over the appliance's own
API, no CSI, no new daemon). For a **node-local runtime** — not a multi-node orchestrator — this
is the same practical outcome a CSI NFS/dynamic-provisioning driver gives a cluster: a pod (here,
a container) gets a mountable, provisioned, network-backed volume. The gap is not "storage
capability," it is specifically "the CSI wire protocol," and nothing in this codebase's own
product surface currently asks for that protocol by name — every consumer described in `AGENTS.md`
reaches storage through `kind: Volume`, not through kubelet's CSI registration path.

## Decision (proposed)

**Do not build a CSI driver.** The daemon it requires is a philosophy-level decision this ADR is
not the right place to make casually — the project's own "daemonless by design" guardrail is
listed first among the non-negotiables in `docs/adr/README.md`, and CSI cannot be satisfied without
crossing it. `kind: Volume`'s existing network-storage support (NFS/CIFS/WebDAV + the TrueNAS
provisioner) stays the answer for "a pod/container needs dynamically-provisioned network storage
on this node."

This reopens only when BOTH of the following are true, not either alone:

1. **A concrete consumer names the CSI protocol specifically** — not "we want dynamic storage"
   (already served) but "this Kubernetes cluster's storage class MUST speak CSI because `X`" (e.g.,
   a workload that assumes CSI's snapshot/clone/resize verbs, which `kind: Volume` does not expose
   today, or an operator that only knows how to consume a `StorageClass` backed by a CSI
   `provisioner:` field).
2. **The daemon question has its own accepted ADR first** — the same discipline `AGENTS.md`'s
   "Próximas fases" section already applies to `delonixd`: "é uma mudança de filosofia... precisa
   da sua própria sessão de planeamento." A CSI ADR that quietly accepts a persistent Node-plugin
   daemon as a side effect would be deciding the bigger question by the back door.

## Alternatives considered

- **A CSI Controller plugin only (no Node plugin), delegating mount to `kind: Volume`.** Rejected:
  kubelet's contract still expects a Node plugin registered and reachable to actually stage/publish
  the volume into the pod's mount namespace — a Controller-only driver does not satisfy real
  cluster usage, it only manages backend resources, which `kind: Volume`'s `provision:` block (ADR-
  0009) already does without CSI at all.
- **A CSI driver that only runs while `delonix-cri` is serving, piggybacking its lifetime.**
  Rejected: `delonix-cri` itself is meant to be startable/stoppable around workload lifecycle
  (no permanent daemon requirement today), and tying CSI's socket lifetime to it would make
  volume mount/unmount reliability depend on whether the CRI socket happens to be up — a new,
  unaudited failure mode for exactly the guarantee CSI exists to make reliable.
- **Expose `kind: Volume`'s NFS/CIFS/TrueNAS capability AS a StorageClass via the in-tree NFS
  provisioner pattern** (a well-known Kubernetes pattern that doesn't require a custom CSI driver
  at all, using an existing community `nfs-subdir-external-provisioner`-style Deployment against
  the SAME NFS/CIFS server `kind: Volume` already provisions). Not rejected — this is the actual
  practical path if a real cluster consumer appears, and it needs zero code changes here: the
  server-side provisioning ADR-0009 already does is reusable by that Deployment as-is. Recorded
  here so it is not confused with "we need to build CSI" the next time someone asks for
  Kubernetes-native storage.

## Consequences

- No code changes. No new dependency. No new daemon.
- `docs/runtime/runtime-architecture.md`'s Storage Engine row gains a citation to this ADR.
- The one path recorded above (community NFS provisioner against the existing TrueNAS/NFS backend)
  is now written down, so a future request for "Kubernetes-native dynamic storage" has a concrete
  answer that does not require reopening this decision.

## Not done here, and why

- **Any CSI code.** No concrete consumer named; see Decision above.
- **The daemon-philosophy ADR** this would need first even if a consumer appeared — that is a
  separate, larger decision (`delonixd`'s own unresolved question in `AGENTS.md`) and does not
  belong inside a storage-specific ADR.
- **Designing the community-NFS-provisioner integration in detail** (which StorageClass parameters,
  which existing TrueNAS credential/secret it would reuse) — sketched as the answer's SHAPE, not
  built, because no cluster has asked for it yet.