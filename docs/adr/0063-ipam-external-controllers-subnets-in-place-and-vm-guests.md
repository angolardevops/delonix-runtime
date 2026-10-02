# ADR-0063: IPAM beyond Proxmox's own: external controllers, subnets changed in place, VMs as DHCP guests

- **Status:** Proposed (2026-10-02). D3's defect fix is implemented in the F5b PR (#654); D1 and D2
  are not implemented.
- **Date:** 2026-10-02
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0059 (network providers by role; F5b is its IPAM slice), ADR-0049 D3 (the
  cluster and node firewalls are administration the engine does not do), ADR-0054 (one
  `providers.yaml` entry per provider), guard-rail 6 of `delonix-adr` (no silent failure).

## Context

ADR-0059 F5b gave `kind: NetworkZone` subnets, DHCP ranges and reservations, served by the
Proxmox SDN's built-in `pve` IPAM. Three things were left out of that PR: other IPAM plugins
(NetBox, phpIPAM), changing a subnet without recreating the zone, and a VM as the DHCP guest (the
live case used a system container). This ADR decides the three. Everything below was measured on
the lab cluster (PVE 9.2.2, two nodes, 2026-10-02) or read in the node's own code
(`/usr/share/perl5/PVE/…`, PVE 9.2.2). Each fact says which.

### What the node does with an external IPAM (read in the node's code)

- `GET /cluster/sdn/ipams/{ipam}/status` refuses every IPAM but `pve`:
  `die "Currently only PVE IPAM is supported!" if $id ne 'pve'` (`API2/Network/SDN/Ipams.pm`).
  The engine reads reservations back through this route (F5b), so with NetBox or phpIPAM the
  engine cannot observe the IPAM through the node at all.
- The phpIPAM plugin's `get_ips_from_mac` ends in `die "parsing of result not yet implemented"`
  (`Network/SDN/Ipams/PhpIpamPlugin.pm`). That function is how the node writes a starting guest's
  MAC and address into the zone's `dnsmasq` `ethers` file (the F5b live case relies on it). So on
  9.2.2 a phpIPAM reservation would be stored in phpIPAM and never served by DHCP. The phpIPAM
  plugin also has no `add_dhcp_range`.
- The NetBox plugin implements `get_ips_from_mac`, `add_dhcp_range` and `del_dhcp_range` (read,
  not run against a NetBox).
- Measured earlier (ADR-0049 slice 2): the node calls the controller's URL when the controller is
  created. An unreachable URL is a hanging request, not an entry.

### What the node does when a subnet changes (measured)

On a running zone (`ipam=pve`, `dhcp=dnsmasq`) with a subnet `10.82.0.0/24`, gateway `.1`, range
`.100–.150`, and a reservation `.20`:

1. `PUT …/subnets/<id>` with `gateway=.254` is accepted with the reservation present. The change is
   staged (the running subnet still says `.1`), **but the IPAM's gateway entry moves to `.254`
   immediately**, before any apply.
2. A new `dhcp-range` is accepted, and so is one that covers the reservation (`.10–.30`).
3. **`POST /cluster/sdn/rollback` restores the subnet to `.1` and leaves the IPAM with `.254` as
   the gateway**: `.1` is no longer held, so the IPAM can hand it to a guest. A transaction that
   changes a gateway and fails leaves the IPAM inconsistent.
4. A `PUT` that repeats the current gateway does nothing to the IPAM. Moving the gateway to another
   address and back (two `PUT`s), then rolling back, restored `.1` as the gateway entry.

The CIDR is the subnet's identity (its id is `<zone>-<network>-<len>`); it cannot change in place.

### What happens with a VM as the guest (measured)

- A VM with no disk and `boot=order=net0`, created on the vnet with a known MAC, boots iPXE from
  SeaBIOS, which is a real DHCP client. With the MAC holding only the reservation, `dnsmasq`
  logged `DHCPACK 10.82.0.20`: the same node path as the container (`ethers` written at
  `qmstart`).
- **The order defect.** With the reservation made BEFORE the VM existed, the VM's create added an
  allocation of a range address for the same MAC (`10.83.0.100`, with the vmid). The node wrote
  that one to `ethers` and the VM got `DHCPACK 10.83.0.100`. F5b's plan said «in sync» because the
  reserved entry was still there. This is a defect of F5b, not a new feature: the reservation was
  held in the IPAM and served to nobody.
- Destroying the VM released every address the MAC held, the reservation included.
- `dnsmasq` hands static leases with an infinite lease time, so a running guest never renews.
- When a zone has `ipam` but no `dhcp`, the IPAM listing skips it
  (`next if … !$zone_config->{dhcp}`, read in the code, and measured: a reservation was stored and
  `status` listed nothing). F5b turned DHCP on only when a range was declared, so a zone with
  reservations and no range would have failed its own read-back.

## Decision

### D1 — External IPAM controllers

1. A zone may name its IPAM: `spec.ipam: <controller id>`, defaulting to `pve`. The id names a
   controller **registered on the cluster by its administrator**. The engine does not create
   IPAM controllers: a controller carries a credential to a third-party system, and a tenant-side
   manifest is not where that credential lives. An id the cluster does not have is refused before
   any write.
2. **phpIPAM is refused by name** while the node cannot map a MAC to an address
   (`unsupported-by-provider`, with the node's `die` as the reason). Accepting it would store
   reservations that DHCP never serves — the silent failure guard-rail 6 forbids. The refusal is
   re-measured on each PVE version the capability matrix names.
3. **NetBox is accepted only after a live spike** against a real NetBox in the lab. Until then the
   three `net.ipam.*` rows for a NetBox-backed zone stay `partial`. Because the node's `status`
   route refuses NetBox, the engine observes a NetBox-backed zone by reading NetBox's own API with
   a **read-only** token, from the provider's entry in `providers.yaml` (a `tokenFile`, as the
   other secrets there). Writes still go through the node (reservations through `…/ips`), so the
   node and NetBox never disagree about who wrote what.
4. The spike must answer, before code: whether `…/ips` on a NetBox zone stores the MAC; what
   NetBox returns for the gateway entry; and whether the node's `ethers` gets the reserved address
   for a guest started on the vnet (the D3 path).

### D2 — Subnets changed in place

1. `gateway` and `dhcpRange` become hot; `cidr` stays cold (it is the identity).
2. A changed subnet is written with `PUT …/subnets/<id>` inside the segment transaction, like
   F5b's create.
3. **After a failed transaction, the provider repairs the IPAM's gateway entry.** It reads the
   running subnet; when the IPAM's gateway entry is not the running gateway, it moves the gateway
   to a free address and back (two `PUT`s) while holding the SDN lock, then rolls back. That is the
   sequence measured to restore the entry. The ledger records the repair as a step, so a process
   that dies during it is resumed like any other step.
4. A new gateway that is held by any IPAM entry (a reservation or a guest's allocation) is refused
   before the `PUT`. The node's behaviour in that case was not measured, and moving a gateway onto
   a guest's address is never the intent.
5. A range may cover a reservation (measured accepted; `dnsmasq` serves only known MACs). A range
   narrowed below a guest's allocation leaves that allocation in place; this was not measured and
   is part of the implementation's live case.

### D3 — VMs as DHCP guests

1. **A reservation owns its MAC in its vnet** (implemented in #654). It counts as held only when the
   MAC holds exactly the reserved address there. Any other address the MAC holds — the allocation a
   guest created after the reservation gets — makes the plan show `~ reservations`, and the apply
   releases it. A running guest keeps its infinite lease until it restarts, so the apply's warning
   names the released address and the vmid. Measured end to end: reservation first, VM created
   after, `DHCPACK .100`; plan `~`; apply released `.100`; plan 0; the restarted VM got
   `DHCPACK .20`.
2. **A zone with reservations serves DHCP even without a range** (implemented in #654). The IPAM
   listing skips zones without `dhcp`, and `dnsmasq` serves each subnet in `static` mode anyway.
   Measured: a zone with one reservation and no range applied with `dhcp=dnsmasq`, plan 0.
3. **Reserving for an engine VM** (`kind: VirtualMachine` on the Proxmox backend) needs the MAC
   before the VM exists, and the node assigns it at create. The decided shape is a reservation that
   names the workload instead of a MAC — `{ ip, workload: VirtualMachine/<name> }` — resolved at
   apply to the VM's `net0` MAC. Because zones are applied before VMs (`run_layers`), such a
   reservation is ensured in a pass after the VM layer. Not implemented; it needs its own slice.

## Alternatives considered

- **An engine `IpamProvider` that writes NetBox or phpIPAM directly.** Rejected for a Proxmox zone:
  the node already writes its IPAM plugin, and a second writer to the same IPAM makes ownership
  ambiguous. It may make sense later for a segment provider with no IPAM of its own; there is no
  consumer for that today.
- **Letting the engine create IPAM controllers from the manifest.** Rejected: the controller's
  credential would live in a manifest, and the controller is shared by every zone on the cluster —
  cluster administration, the ADR-0049 D3 boundary.
- **Accepting phpIPAM and documenting the gap.** Rejected: a reservation that is stored and never
  served is the accept-and-ignore failure the engine refuses elsewhere.
- **Keeping subnets cold (replace on any change).** Simpler and correct, but a replace of a zone
  with guests on it takes their network away. The in-place path is cheap once the gateway repair
  exists, and the repair is needed anyway for a transaction that fails.
- **Repairing the gateway entry with `DELETE` + `POST …/ips`.** Measured to work for the delete,
  but the re-added address is a plain entry, not a gateway entry. Rejected for the move-and-back
  sequence, which restores the gateway flag.
- **Restarting a running guest after releasing its allocation.** Rejected: the engine does not own
  every guest on a vnet. It says which guest has to restart.

## Consequences

- Easier: an engine VM or container on a reserved address works whatever the order of creation;
  a reservation-only zone works; gateways and ranges change without recreating the zone (D2, when
  implemented).
- Harder: D1 adds a read path to a third-party API, with its own credential and version range;
  D2 adds a repair step that must be live-tested with an injected failure; D3.3 adds a pass after
  the VM layer.
- Known limits: phpIPAM stays refused until a PVE release implements the MAC lookup; the NetBox
  facts above are read in code, not run; a range narrowed under a guest's allocation is not
  measured; IPv6 is out of scope (the engine's SDN is IPv4).
- Seen while measuring, not part of this ADR: `stack apply` applies a `NetworkZone` once in its
  layer and again in the converge of a hot change, so one apply runs two SDN transactions. The
  same pattern exists for `NetworkGateway`. It costs a second reload and should be fixed on its own.
