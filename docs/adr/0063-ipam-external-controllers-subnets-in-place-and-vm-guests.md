# ADR-0063: IPAM beyond Proxmox's own: external controllers, subnets changed in place, VMs as DHCP guests

- **Status:** Accepted (2026-10-06, by the owner). D3's defect fix is implemented in the F5b PR (#654). D1.1, D1.2 and
  D2 are implemented (2026-10-09, see «Implementation» below). **D1.3/D1.4 (NetBox, after its spike) and D3.3 (a
  reservation that names a VM) are not implemented.**
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

## Implementation (2026-10-09)

D1.1, D1.2 and D2 are built. Every fact below was measured on the lab cluster (PVE 9.2.2, two
nodes) by `crates/providers/delonix-proxmox/tests/live.rs::a_subnet_changes_in_place_and_a_rolled_back_gateway_entry_is_repaired`
and through the CLI (`stack plan`/`apply`/`destroy` of a `NetworkZone`), unless it says otherwise.

- **D1.1.** `spec.ipam` names the controller (default `pve`; also `provider.spec.ipam` of a
  `kind: Network`, a zone setting every network of the zone must agree on). A controller the
  cluster does not list (`GET /cluster/sdn/ipams`) is refused before any write — measured: the
  zone was not created. `ipam` is a cold field: the node refuses to change it once a subnet exists.
- **D1.2, D1.3.** The provider serves only a controller of plugin `pve`. `phpipam` is refused by
  name with the node's `die`; `netbox` is refused by name until the D1.3 spike; any other plugin
  as never measured. Measured (2026-10-09, follow-up run): a NetBox and a phpIPAM controller
  registered on the cluster against a stub (the node verifies the URL: `GET <url>/ipam/aggregates/`
  for NetBox, `GET <url>/sections/<section>` with a `token:` header for phpIPAM) are listed by the
  port with their plugin type and refused by name, DX-6381 (exit 69), through `stack apply` and
  `live.rs::an_external_ipam_controller_is_listed_and_refused_by_name`. The refusal comes before
  any write: no zone, vnet or record was created, and the stub saw only the node's own
  verification. `stack plan` does not refuse (it plans `+`, exit 2): the controller is read at
  apply.
- **D2.1, D2.2.** `subnets` (vnet and CIDR) stays cold; a new hot field, `subnetSettings`
  (gateway and ranges), is read from the provider, so a gateway changed by hand on the node
  converges back too (measured). The write is `PUT …/subnets/<id>` with the exact target:
  **`delete=dhcp-range` clears every range** (measured here for the first time). **`delete=gateway`
  clears the gateway and its IPAM entry** (measured in the follow-up run, through the provider
  and through `stack apply`): the staged `PUT` releases the entry at once, like a moved gateway,
  and after the apply the running subnet has no gateway, the IPAM no gateway entry, and the plan
  is `=`.
- **D2.4.** A new gateway that an IPAM entry holds is refused before the `PUT` (measured: the
  entry did not move).
- **D2.3 — the measured sequence was incomplete.** Injected exactly as the Context describes
  (a gateway staged, the transaction failed and rolled back): the IPAM's gateway entry stayed on
  the staged address. The move-and-back then put a gateway entry on the running gateway **and
  left the stale one, still flagged gateway** — two gateway entries for one subnet, the stale one
  holding its address, and the node refusing the subnet's delete (`cannot delete subnet …, not
  empty`). The repair therefore also releases every other gateway entry of the subnet with
  `DELETE …/ips` (the delete measured to work in «Alternatives»), and reads back exactly one
  entry, on the running gateway. It runs after a failed transaction and before the transaction
  of an apply that follows an interrupted run (a dead process's staged change is discarded by
  the next lock), as the ledger step `repair_gateways`, only on vnets carrying the record's mark.
  Measured through the CLI with a natural failure: two subnets, the first moved its gateway, the
  second was refused by D2.4 inside the same transaction; the repair put the first one's entry
  back, and the ledger reads `transaction: failed`, `repair_gateways: done`.
- **D2.3 — two more states the first repair missed** (follow-up run, 2026-10-09, measured on the
  node by hand and then through the engine). The rollback restores the staged configuration and
  never the IPAM, whichever way the gateway changed:
  - a gateway **removed** (`delete=gateway`) and rolled back: the subnet runs with its gateway and
    the IPAM holds **no** gateway entry. The router's address is free: the node then accepted a
    reservation of it for a guest's MAC (`POST …/ips`, rc 0). The first repair skipped a subnet
    with no gateway entry ("never measured"). Moving through a free address and back restores the
    entry (measured: the node warns `IP '<gw>' does not exist in IPAM DB` on the first `PUT` and
    goes on), so the repair now does that too.
  - a gateway **added** to a subnet without one and rolled back: the subnet runs without a
    gateway and the IPAM keeps the new gateway's entry. **It wedges the next apply**: D2.4 refuses
    that very gateway because the stale entry holds the address (DX-5389, measured with the PR's
    binary after a `kill -9`, and repeated on every apply). The repair now releases every gateway
    entry of a subnet that runs without a gateway.
  - A right entry next to a stale one (a repair cut short between its move and its release) only
    releases the stale one. The releases run inside the discarded change, under the SDN lock.
- **D2.3 after a real `kill -9`** (follow-up run, through `stack apply`). The CLI's route trace was
  pointed at a FIFO, so each request waits for the reader; the process was SIGKILLed right after
  the subnet's `PUT` was answered, blocked before its next request. The node then held: the SDN
  lock with the dead process's token, the staged gateway change, the IPAM gateway entry moved (or
  released, or added), and the record's ledger read `transaction: submitted`. The next apply said
  the run was interrupted, discarded the dead lock (`died holding the lock`), repaired, and ended
  with one gateway entry on the running gateway and no lock, for all three changes (moved to
  `.254`, removed, added). With the PR's binary as the control, the removed case ended `rc=0` with
  **no** gateway entry and a plan of `=`, and the added case was refused by D2.4 on every apply.
- **Widened (2026-10-09), owner decision:** the repair ran only after a failed or interrupted
  run, so an inconsistency made by hand on the node, or left by an older binary that never had
  this repair, stayed until the next failure happened to trip it. `repair_gateways` is idempotent
  — a subnet whose entry already matches its running gateway is a no-op — so the call site in
  `network_zone.rs::apply_one` now runs it on **every** apply, not only an interrupted one. This
  widens D2.3 from "called only after a failure" to "called every time", at the cost of one extra
  read per apply. The plan still does not read the IPAM's gateway entries on its own (a subnet
  whose entry is missing still plans `=`, exit 0) — that is a separate, larger change (the plan
  would need to fingerprint the IPAM side the way `uses_ipam`/`ipam_fingerprint` already do for
  drift detection, not just for the repair step), and is left open.
- **Not fixed, by owner decision — a known limitation, not an engine bug to patch around.** A
  subnet **deleted** inside a transaction that is then rolled back comes back in the
  configuration and not in the IPAM's database (`pve-ipam-state.json`). The node then refuses
  every update and every delete of that subnet (`subnet '<cidr>' doesn't exist in IPAM DB`), so
  neither the engine nor `pvesh` can remove the vnet or the zone. The only way out measured was to
  edit that file on the node by hand — it is an undocumented, internal Proxmox state file, not
  something exposed through any API route this engine talks to. Having the engine reach around
  its own provider boundary to patch a vendor's internal file is a fragile, unsupported workaround
  (the file's format and location are not a contract Proxmox publishes, and could change under a
  future PVE release without notice) — the risk of building that is judged higher than the risk of
  leaving this documented. It applies to a teardown (`remove_subnet` then `remove_vnet`/
  `remove_zone`) or a CIDR replace that fails after the subnet's delete was staged. Operational
  mitigation until a real fix exists: avoid interrupting (`kill -9`, a crashed host) a
  `NetworkZone` apply or destroy while a subnet delete is in flight; if it happens, recovery is a
  manual Proxmox-side operation, not an automated `delonix` path.
- **D2.5 — measured** (follow-up run, by hand and through
  `live.rs::a_gateway_is_removed_and_added_in_place_and_a_range_narrows_under_a_guest`). A VM
  created on the vnet got an allocation in the range (`.100`); the range narrowed to `.10–.30` is
  accepted and applied, and the allocation stays where it is, with its vmid. The zone's `dnsmasq`
  serves the whole subnet as `static` (`dhcp-range=…,<network>,static,<mask>,infinite`), so the
  range only steers the IPAM's allocator: a VM created after the change got `.10`, and the first
  VM, started after the change, was handed `.100` (`ethers` written at `qmstart`, `DHCPACK
  10.86.2.100`). The engine's warning ("the node leaves it where it is") is therefore what the node
  does; the guest keeps an address outside the declared range until it is destroyed.
- The F5b and F5c live cases (`the_ipam_provider_reserves_an_address_and_a_guest_gets_it_by_dhcp`,
  `the_dns_provider_registers_a_guest_in_the_zones_dns_server`), whose `prepare_zone` calls now
  name the controller, pass again against the lab (an `alpine:3.20` OCI archive, the lab's
  PowerDNS on the second node), and leave no zone, guest or DNS record behind.
