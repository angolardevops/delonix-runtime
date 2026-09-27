# ADR-0055: A VM may opt out of the libvirt anti-spoofing filter — explicitly, per VM, and never by default

- **Status:** Proposed
- **Date:** 2026-09-26
- **Deciders:** Walter Angolar
- **Relates to:** `bd7ffd9b` (the `delonix-antispoof` nwfilter, which had no ADR of its own —
  its reasoning lives in that commit and in the doc comments of `ANTISPOOF_FILTER`),
  ADR-0049 and ADR-0053 (the nested Proxmox lab that hits this), ADR-0050 (`vm.antispoof`
  in the capability catalog)

## Context

Since `bd7ffd9b` (2026-08-25) the libvirt backend attaches ONE nwfilter to the primary NIC
of every VM in `nat`/`network`/`bridge` mode: `delonix-antispoof`, composed of libvirt's
`no-mac-spoofing` and `no-arp-mac-spoofing` (`crates/adapters/delonix-vm/src/lib.rs`,
`ANTISPOOF_FILTER`, `antispoof_applies`, `libvirt_filterref_xml`,
`ensure_antispoof_filter`). It closes a real hole: the MAC is derived from the VM NAME
(`mac_for`), so two VMs with the same name on one L2 would otherwise be able to impersonate
each other at L2 and in ARP. The filter fails closed — if it cannot be defined, the VM is
not created.

There was no way to turn it off, and that is correct for a workload. It is wrong for one
class of VM: **a VM whose job is to forward frames it did not originate at L2** — a
hypervisor-in-a-VM whose own guests are bridged onto its NIC (Proxmox VE's `vmbr0`, a
libvirt host's `br0`, an OpenStack compute node with a flat provider network). Those
guests' frames carry the GUESTS' MACs, which is exactly what `no-mac-spoofing` exists to
drop.

**Measured 2026-09-26.** `pve-lab-475`, created with `delonix vm create --backend libvirt`
from this repo's own `proxmox-ve_9.2` appliance (`vmbr0` bridges `eth0` and takes DHCP from
the libvirt `default` network), could not give a nested guest on `vmbr0` any network: the
guest's DHCP DISCOVER left the PVE node with the guest's MAC, the filter dropped it on the
tap, and the libvirt network never saw it. The lab worked around it with a NAT bridge
(`vmbr1`, masqueraded out of `vmbr0`) inside the node — which hides the nested guests from
the outer network and is not the topology ADR-0049/ADR-0053 want to exercise (a guest
migrated between nodes should keep its address on the shared L2). Every hypervisor-in-a-VM
use of this repo's own appliances hits the same wall.

The guard-rails this touches:

- **No silent failure (guard-rail 6).** An opt-out that is accepted where there is no
  filter would record «spoofing allowed» for a VM whose behaviour did not change — the
  mirror image of the accepted-ignored-believed option this repo has corrected before
  (`--security-opt seccomp=`, `-v …:z`, `--network-alias`).
- **State needed to rebuild is persisted** (the class catalogued in `AGENTS.md`, paid by
  `-v`, `-p` on a custom network, `Container.pod`, `serial_capture`): `vm start` rebuilds
  the domain from the record (`config_from`), so a flag kept only in the request would
  come back filtered on the first restart — or, worse, a record that invented it would
  come back unfiltered.

## Decision

1. **An explicit per-VM opt-out: `vm create --allow-mac-spoofing`, and
   `allowMacSpoofing: true` in `kind: VirtualMachine`** (flat, `allow_mac_spoofing`
   accepted as the legacy spelling; grouped form `network: {allowMacSpoofing: true}`).
   The name says what it COSTS, not what it enables: someone reading a manifest review
   sees «allow MAC spoofing», not «nested networking». The flag is a `bool` that defaults
   to `false` everywhere — `VmConfig`, `VmBootSpec`, `LibvirtExt` — so every caller that
   builds a config with `..Default::default()` keeps the filter.

2. **What it removes, exactly:** the single `<filterref filter='delonix-antispoof'/>` on
   the primary NIC, and with it the `ensure_antispoof_filter` call in `boot` (a VM that
   emits no reference does not need the filter defined, and should not fail on a daemon
   that cannot define it). Nothing else in the domain changes (pinned by
   `an_opted_out_nic_carries_no_filterref`). Every other VM on the host keeps its filter;
   the filter's definition is shared and untouched.

3. **Refused where there is no filter** (`check_allow_mac_spoofing`, in `create_with`,
   after the backend is chosen, so every caller of the engine inherits it): on any backend
   other than `libvirt` (Cloud Hypervisor's tap is guarded by nft rules on the SDN, Proxmox
   by its own firewall — neither is this filter), and on a libvirt NIC without a tap
   (`user` mode). Error `RequiresLibvirtBackend`.

4. **Persisted and visible.** `VmBootSpec.allow_mac_spoofing` (serialized only when
   `true`, so records written before this read as filtered and a filtered VM grows no key)
   is restored by `config_from` on every `vm start`/`restart`. `vm describe` prints an
   `Antispoof` line on every libvirt VM with a tap — `on (delonix-antispoof: MAC +
   ARP)` or `OFF — allowMacSpoofing …` — and none where the filter never applies, so the
   line is never a claim about a control that is not there. `vm create`/`apply` print a
   warning on stderr, read from the RECORD, and `boot` logs one on every start.

5. **Not reconciled on an existing VM.** Like `tpm`, `vnc` and `machine`, adding or removing
   `allowMacSpoofing` on a VM that already exists is not applied by `apply`; the existing
   `FieldsNotCompared` condition names it. Changing it means recreating the VM — the
   honest cost of a field that changes the NIC's security posture.

## Alternatives considered

- **Do nothing; keep the NAT bridge inside the nested hypervisor.** Works for egress, but
  the nested guests are invisible to the outer L2 (no lease from the outer network, no
  address that survives a move between nodes), and every lab has to re-invent it inside
  each appliance. Rejected: it moves a host-side decision into every guest.
- **Turn the filter off automatically for appliances whose metadata says «hypervisor».**
  Rejected: a security control that disappears because of what an image SAYS about itself
  is a control an image author can switch off for every VM made from it. The operator of
  THIS VM has to ask for it.
- **A global switch (node-wide, `providers.yaml` or env).** Rejected: it weakens every VM
  on the node to serve one, and a node-wide default is exactly what this ADR forbids.
- **A weaker filter instead of none (keep `no-arp-mac-spoofing`, drop only
  `no-mac-spoofing`).** Rejected: a bridged guest answers ARP for its OWN address with its
  OWN MAC, which `no-arp-mac-spoofing` drops just the same; the half-filter breaks the same
  use case and adds a third state to reason about.
- **An allow-list of extra MACs per VM (libvirt `MAC` parameters on the filter).** More
  precise, but a nested hypervisor mints its guests' MACs at runtime, so the list is
  unknowable at `vm create`. Left as a possible future refinement, not a substitute.
- **A field name like `nestedNetworking` or `trustedL2`.** Rejected: it names the benefit
  and hides the cost; the reviewer of a manifest must see what is being given up.

## Consequences

- A hypervisor-in-a-VM (Proxmox appliance, a libvirt host, an OpenStack compute node) can
  bridge its guests onto the outer network. Measured below.
- A VM created with the flag CAN impersonate any MAC on its L2 and answer ARP for any
  address — including another VM's. The flag is therefore a trust decision about that
  guest, and it is visible in three places (the record, `vm describe`, the warnings) so it
  can be audited. It is the operator's decision per VM, never the engine's.
- Unchanged, and still not guaranteed (as before): IPv4 source pinning, IPv6/NDP
  anti-spoofing, and any filtering on `extraNics` — the extra NICs never carried the filter
  (`libvirt_domain_xml`, the `extra_nics` loop). That gap predates this ADR and is not
  widened by it.
- The capability catalog's `vm.antispoof` row stays `supported` for libvirt: the default is
  still filtered, and the opt-out is a per-VM exception, not a missing capability.

## Evidence

Measured on this host (libvirt 10.0.0, `qemu:///system`, `default` network
192.168.122.0/24), two VMs from the same `proxmox-ve_9.2` appliance with the same
`--net-mode nat`, differing only in `--allow-mac-spoofing`. In each, a nested QEMU guest
with no disk (`qm create … --net0 virtio,bridge=vmbr0`, booted into the NIC's iPXE ROM) asks
for DHCP through `vmbr0`:

| | `dlx-as-on` (default) | `dlx-as-off` (`--allow-mac-spoofing`) |
|---|---|---|
| `filterref` in the domain after `vm start` (`virsh dumpxml`) | 1 | 0 |
| `describe vm` → `Antispoof` | `on (delonix-antispoof: MAC + ARP)` | `OFF — allowMacSpoofing (any source MAC; ADR-0055)` |
| record `boot.allow_mac_spoofing` | absent | `true` |
| nested guest's DHCP on `vmbr0` (`tcpdump` in the node) | 6 DISCOVERs in ~25 s, **no reply** | DISCOVER → OFFER from `192.168.122.1` in **2 s** |
| host `virsh net-dhcp-leases default` | no lease for `bc:24:11:aa:00:01` | `bc:24:11:aa:00:02` → `192.168.122.231` |

Both nodes kept their own lease and SSH throughout: the filter never blocked the node's own
MAC, only its guests'. Each VM went through two `vm stop` + `vm start` cycles before the
measurement, so the rows above were read from domains REBUILT from the record, not from the
first `create`. `vm start dlx-as-off` logged `anti-spoofing filter 'delonix-antispoof' NOT
attached (allow_mac_spoofing, ADR-0055)`.

The refusals, measured with the same binary: `--net-mode user --allow-mac-spoofing` gives
`DX-1505 … needs a NIC with a tap`, and `--backend cloud-hypervisor --allow-mac-spoofing`
gives `DX-1505 … the 'cloud-hypervisor' backend has none to opt out of`. In both cases no
record, overlay or seed was left behind.

The host's `dnsmasq` log for the filtered node's guest (`bc:24:11:aa:00:01`) has **zero**
lines: its DISCOVER never got past the tap. The opted-out guest's has DISCOVER → OFFER →
REQUEST → ACK.

**`kind: VirtualMachine` and `bridge` mode, measured the same way.** `delonix apply -f` of
a manifest with the grouped form (`network: {mode: bridge, bridge: virbr0,
allowMacSpoofing: true}`) printed the stderr warning, gave a domain with
`<interface type='bridge'>` and no `filterref`, a record with `allow_mac_spoofing: true`,
and `Antispoof: OFF …`. After a `vm stop` + `vm start`, a nested guest on that node's
`vmbr0` (`bc:24:11:aa:00:03`) got `192.168.122.232`: DISCOVER → OFFER → REQUEST → ACK in
the host's `dnsmasq` log.
