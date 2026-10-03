# ADR-0071: The provider block is `type` + `spec`; no `ref` until the node has more than one target per type

- **Status:** Accepted (2026-10-03, by the owner's canonical brief) — implemented for `VirtualMachine`, `Network` and the tunnel `Gateway`
- **Date:** 2026-10-03
- **Deciders:** Walter Angolar
- **Supersedes:** the spelling in ADR-0070 D1 (`provider: { name, <vendor>: {…} }`); keeps its rule (provider specifics live inline in the resource's own spec, never one Kind per provider resource)
- **Relates to:** ADR-0054 (the node's providers file), ADR-0059, ADR-0069

## Context

The canonical brief proposes one public provider block, `provider: { ref, type, spec }`, and asks
to adopt it only if the cost is justified and, if adopted, to make it the only recommended form.

Measured against the code at `17eb57d1`:

- The node's providers file (ADR-0054) is keyed by **type**: one entry per provider type, one default
  provider. There is no named target (`pve-lab`) anywhere, so a `ref` would select nothing. The
  brief's own rule — «a recognized field with no implementation is not a feature» — rules it out
  today; it is reserved, and the loader names it as unknown (`provider.ref`).
- ADR-0070's shape (`name` + a vendor-named sibling block) mixes the discriminator into the keys,
  so the schema cannot say «this spec belongs to this type»; `type` + `spec` can.
- It had landed one PR earlier and unreleased, so the move costs a conversion, not a migration.

## Decisions

**D1. The block is `provider: { type, spec }`.** `type` is one of the provider types the engine
knows (`libvirt`, `cloud-hypervisor`, `proxmox` for a VM; `proxmox` for a Network segment).
`spec` is a mapping, typed **per provider and per resource**: a VM's `libvirt` keys are not a
Volume's, and a key the named type does not have is refused (`provider.spec.machine (not a
'cloud-hypervisor' VM field)`). `spec` without `type` is refused. Cloud Hypervisor and Proxmox VMs
have `spec: {}` on purpose until a field is implemented and proven.

**D2. One normalization, no second executor.** `type` becomes the existing backend selector; `spec`
keys become the existing flat fields before anything runs. The older spellings — the ADR-0070
`name` + vendor block, the `libvirt:` group, the flat vendor fields, the top-level `backend` —
normalize to the same thing and are reported once per document. A `type` that contradicts a flat
`backend` is refused, not resolved by precedence.

**D3. Generic intent is never overridden by the provider.** A libvirt `cpuTopology` whose
sockets × cores × threads is not the declared `vcpus` is refused: the generic field is the
intent, the provider spec realizes it.

**D4. Reserved: `ref`.** Added together with named targets in the providers file, with a test
that a changed default never re-binds an existing resource.

## Pending

- The tunnel `Gateway` is done: `provider: { type: cloudflare, spec: { hostname, token, tokenSecretRef } }`; a bare `provider: pinggy` stays valid as the selection-only spelling. `insecureSkipTlsVerify` stays generic (it is about the local backend) and `localPort` is the intent. The eleven templates and `examples/` moved; the loader normalizes to the scalar + flat fields the executor reads, and a key written in both places is refused.
- `SystemContainer` (Proxmox LXC) **has the block** (2026-10-03): `spec.provider: { type: proxmox, spec: { swap, network } }`, folded at load into the flat fields the one executor reads (`lower_system_container_provider`); the flat spelling keeps working and means the same. The generic part stays at the top: image, entrypoint, env, memory, cores, rootfs. The ADR-0058 contract (hot/cold fields, no exec/logs) is unchanged.
- `NetworkGateway` (OPNsense) **accepts the block** (2026-10-03): `spec.provider: { type: opnsense }` lowers to the scalar the executor reads (`lower_network_gateway_provider`); the scalar still works. `provider.spec` is accepted empty and a key in it is refused by name. **The Kind stays a Kind, deliberately:** its vocabulary (aliases, rules, NAT, policies) is the engine's generic *perimeter* vocabulary, a different scope from the per-workload `NetworkPolicy`, and it is not one Kind per provider resource — a second gateway provider reuses it. The only appliance-only knobs (`nat[].interface`, `policies[].sequence`) are per list item and stay where they are until a second gateway provider shows what a per-item provider block must carry. **Not proven live**: the lab OPNsense VM does not boot, so the loading and lowering are covered by unit tests only.
- Per-disk and per-NIC options keyed by the generic item's stable name (the brief's §4.3).
