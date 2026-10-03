# ADR-0070: Provider specifics live in a `provider` block of the resource's own spec

- **Status:** Accepted (2026-10-03, owner's direction) — D1–D2 implemented for `VirtualMachine`, D3 for `NetworkZone` (now `Network` + `provider.proxmox`); the table lists what is still to move
- **Date:** 2026-10-03
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0008 (VM backend registry), ADR-0044 (provider ports), ADR-0049/0051/0059 (Proxmox, OPNsense, network providers by role), ADR-0058 (system containers), ADR-0069

## Context

The owner's rule: a provider's own resources belong inside the provider part of the spec, in the
same YAML object, and not as one Kind per provider resource; and the general open-interface
vocabulary (OCI images, CRI pod semantics, CNI networks, CSI-style volume claims) must not be
mixed with what only one provider understands.

At `681d3de8` the `VirtualMachine` spec mixed both layers at the top level: `disk`, `vcpus`,
`memory`, `network`, `hostname` next to `machine`, `cpuModel`, `tpm`, `video`, `libvirtXml`,
`extraNics`, plus a `backend` selector, plus a `libvirt:` group that itself carried `backend`.
Three spellings of the same fact.

## Decisions

**D1. One canonical shape for the provider part of a spec.**

```yaml
spec:
  disk: …              # provider-neutral
  resources: { vcpus: 2, memory: 2G }
  network: { name: lan }
  cloudInit: { hostname: web }
  provider:
    name: libvirt      # the target; omitted = the configured default / auto-detection
    libvirt:           # vendor block: typed, validated, only for its own `name`
      machine: q35
      xml: "<domain>…"
```

- `provider.name` replaces the top-level `backend`.
- A vendor block under a `name` that is not its own is an error (`provider.libvirt (provider.name is 'cloud-hypervisor', …)`), not a field the other backend ignores. An unknown vendor key or sub-key is reported by the same unknown-field gate as everything else (ADR-0069 D1), so it refuses the manifest.
- With a vendor block and no `name`, the block selects the provider.
- `type: microvm` in a `Workload` rejects a `provider` that names anything but Cloud Hypervisor (the same contradiction `backend: libvirt` already raised).
- The flat vendor fields, the `libvirt:` group and the top-level `backend` are still accepted and lowered to the same fields, with one warning naming the keys to move. Nothing else changes in execution: there is one executor, and the lowering happens before it.

**D2. Layering.** A provider-neutral field never names a vendor and a vendor block never carries a
neutral concept. Open-interface concepts (OCI image reference and digest, CRI pod/sandbox
semantics, CNI network config, volume claims) stay in the neutral part of the spec and are
translated by the adapter; nothing in them is described as a provider capability, and no provider
block redefines them. Conformance to those interfaces is claimed per interface and version, with
the suite that proves it, never through a provider block.

**D3. The remaining provider-specific Kinds, and what moving each costs.** They are named here
because the owner's rule says they should not be Kinds, and they are not moved in this change
because each needs more than a re-spelling:

| Kind today | Provider part | Target shape | What blocks the move |
|---|---|---|---|
| `NetworkZone` | **Moved.** Proxmox SDN zone + vnets, DNS/IPAM settings | `kind: Network` with `spec.provider.proxmox: { zone, alias, dhcpRange, reservations, dns }`; `subnet`/`gateway` stay neutral | Done at load: the networks that name a zone are folded into the one `NetworkZone` document the executor already reconciles (the way `Dependency` folds into `NetworkPolicy`), so the zone is created with the first vnet and removed with the last without a reference count. `kind: NetworkZone` still loads, announced as superseded; the synthesized document carries `delonix.io/lowered-from` so the load does not announce a Kind the author never wrote. `dns` is a zone setting: networks of one zone that disagree on it are refused. Native-only fields (`driver`, `vni`, `peers`, …) with a provider block are refused. |
| `NetworkGateway` | OPNsense aliases and perimeter rules | the perimeter rules belong to `NetworkPolicy` with a `provider` scope; the aliases to the provider block | A policy has no perimeter scope yet; the OPNsense ownership marks must carry over. |
| `SystemContainer` | Proxmox LXC (cores, swap, rootfs, template staging) | a guest `kind` with `provider.proxmox` (ADR-0058 says its semantics are closer to a VM than to a Pod) | The ADR-0058 contract (`exec`/logs unsupported, hot vs cold fields) has to be re-expressed on the new Kind and its snapshot/backup/move verbs repointed. |
| `Volume` `provision.truenas`, `nfs`/`cifs`/`webdav` | already a vendor-keyed block (`provision.<vendor>`) | unchanged | — it already follows D1. |

D1's validation (name ↔ block, unknown keys) is written once for `VirtualMachine`; the other
moves reuse it by declaring their vendor tables.

## Consequences

- `examples/vm.yaml`, `examples/full-virtualmachine.yaml` and `examples/full-workload.yaml` use `provider:`; the flat spellings still load, with a warning.
- A manifest that puts a libvirt-only field under a Cloud Hypervisor or Proxmox target is refused at load time, before any provider is contacted.
- No new Kind, trait or crate.
