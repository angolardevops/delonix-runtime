# ADR-0052: The Proxmox node's own per-VM firewall answers the firewall domain for VMs on it

- **Status:** Accepted
- **Date:** 2026-09-25
- **Deciders:** Walter (owner)
- **Related:** ADR-0008 (the Proxmox `VmBackend`); ADR-0049 (the measured
  Proxmox API matrix, and its D3 exclusion of cluster-level firewall
  administration); ADR-0050 (the capability catalog this adds a column to);
  ADR-0051 (the `GatewayProvider` for perimeter appliances, and the reasoning
  this decision follows: per-workload rules are the `firewall` domain,
  perimeter policy is not)

## Context

The capability matrix had ONE provider in the `firewall` domain: the Linux
provider's nftables chain in the holder, which filters containers (and the
tap of a Cloud Hypervisor VM for anti-spoofing and namespace isolation, but
with no per-VM rule chain). A VM on a Proxmox node was outside every rule the
engine can write: its traffic never crosses this host.

The node has a firewall of its own for each VM, and the client already
reached it — `#492` and `#496` implemented and live-tested the per-VM
firewall routes (`…/qemu/{vmid}/firewall/{options,rules,aliases,ipset,…}`).
Nothing in the engine called them: 23 public functions, zero callers outside
the crate and its tests.

ADR-0051 already drew the line this decision needs. The four `firewall`
capabilities describe **per-workload** rules; a perimeter appliance
(OPNsense) answers the network domain through `GatewayProvider` instead,
because forcing it into per-workload rules is the wrong shape. A Proxmox
node's per-VM firewall is the opposite case: it IS per-workload, one rule
table per VM, bound to that VM's NIC. So it fits the `firewall` domain
directly.

Measured on the lab node (PVE 9.2.2) before the design was fixed:

- **A VM's rules filter only when three switches agree**: the cluster's
  DATACENTER firewall `enable`, the VM's own `enable`, and `firewall=1` on the
  VM's NIC. A cluster that never enabled the first answers
  `GET /cluster/firewall/options` with a bare `digest` — no `enable` key.
  This backend's `net0` never carried `firewall=1`.
- **Turning the datacenter switch on can cut the operator off.** On the lab
  it did: the appliance's `/etc/hosts` still names the build-time address
  (`10.0.2.15`), so the node detected only `127.0.0.0/8` as its local network,
  and its default input policy dropped the management traffic from
  `192.168.122.0/24`. The API and ping stopped answering, and the node was
  recovered from the serial console.
- **The node inserts a new rule at the top** (position 0), keeps rules by
  POSITION only, and refuses a `dport` without a `proto`.

## Decision

1. **A port on the VM backend, not a new registry.** `VmBackend` gains
   `apply_firewall(vmdir, vm, &Policy)` and `read_firewall(vmdir, vm,
   Direction)`. Their default implementations refuse by name
   (`UnsupportedByBackend`, DX-1501), as `pause` and `snapshot` do. The VM
   record is what knows the node-side id, so the port sits where the record
   is resolved; a parallel registry would have to find it again.
   `delonix_vm::firewall::Policy` is one direction of one VM, whole: the
   default verdict plus the ordered rules.

2. **`kind: NetworkPolicy` gains `scope: vm`**, with `target` naming a VM.
   It keeps the same contract a container policy has: applying replaces that
   direction and leaves the other alone, and it converges through
   `stack plan`/`apply` (`defaultPolicy` and `rules` are hot fields). Only for
   `scope: vm`, rule ORDER is compared, because the node evaluates rules first
   match wins. `fromWorkload`/`toWorkload` are refused, because they resolve to
   an address on this engine's SDN, and a VM filtered by its node is not on
   that SDN. The `scope: network` fields are refused too.

3. **The Proxmox backend implements it** (`delonix_proxmox::vm_firewall`):
   - check the datacenter switch, and refuse with **DX-6508**
     (`vm.proxmox_datacenter_firewall_disabled`) **before any write** when it
     is off. The engine never turns it on: that changes what the cluster's
     nodes accept, which is exactly the lock-out measured above;
   - ensure `firewall=1` on `net0` (the MAC is sent back unchanged) and the
     VM's `enable=1`;
   - delete this engine's rules in that direction, highest position first;
   - write the new rules in reverse, so they come out in policy order ABOVE
     any hand-made rule;
   - write the default verdict (`policy_in`/`policy_out`) LAST, so a deny
     default is never in force while the allow rules are still missing.

   **Ownership is a comment tag**, `delonix-managed:<n>`. The node has no
   other stable name for a rule, and an operator can add rules in the node's
   UI. A rule without the tag is never deleted and never reported as drift.
   `proto: any` with a port becomes two node rules (TCP and UDP) with the same
   tag, and the readback folds them back into one.

4. **The catalog gets a second network column.**
   `delonix_proxmox::network_capability_report` answers the network kind. The
   four `firewall.*` rows are `partial`: the live case proves the node HOLDS
   the policy (switches, verdict, rules in order, a hand-made rule untouched,
   re-apply replaces, directions independent), and the node's compiled
   `tap<vmid>i0-IN` chain was read once by hand. No guest traffic crosses it
   in any test, so `supported` would claim a measurement that was not made.
   `net.bridge` is `partial` for the SDN zone and VNets that `kind: NetworkZone`
   already drives. Every other network row says it is not implemented or not
   applicable.

## Alternatives considered

- **A separate `WorkloadFirewallProvider` registry in `delonix-sdn`, like
  `NetworkZoneProvider`.** Rejected: the operation needs the VM record's
  handle, which only the VM backend resolves. A second registry would need a
  second way to find the VM.
- **Datacenter-level (`/cluster/firewall/*`) rules as a second
  `GatewayProvider`, next to OPNsense.** Not done here, and not done in a
  follow-up by default. ADR-0049 D3 classifies the cluster firewall writes as
  `unsupported-by-design` (provider administration), and datacenter rules
  guard the cluster's NODES, not the VMs behind them. Doing it anyway needs
  its own ADR that supersedes that part of D3, with a concrete need named.
  This decision adds one read in that area, `GET /cluster/firewall/options`,
  and never writes there.
- **Turn the datacenter switch on automatically.** Rejected. It was measured
  to cut the operator off the node, and a firewall that locks out its own
  administrator on the first apply fails in the worst way a firewall can.
- **Own the VM's whole rule table** (delete every rule, as a container
  policy replaces its whole direction). Rejected. On a node shared with its
  operator's own rules, that deletes someone else's configuration without
  asking.

## Consequences

- The `firewall` domain has two providers: `linux` for containers and
  `proxmox` for VMs on a Proxmox node. `delonix provider ls --kind network`
  and the published matrix show both.
- A libvirt or Cloud Hypervisor VM with `scope: vm` fails closed (DX-1501)
  with the reason, instead of being accepted.
- **Limits, stated**:
  - rules are addressed by position, and the client sends no `digest`, so an
    operator editing the same VM's rules during an apply can race it;
  - a lost answer on a rule delete is checked by position, which a shift can
    misread (inherited from `delete_firewall_rule`);
  - aliases, IP sets, macros and security groups exist on the client but are
    not part of `scope: vm`;
  - `get`/`describe networkpolicies` list container policies only;
  - no packet crosses the VM in any test.
- **Found on the way, not fixed here**: the Proxmox appliances keep the
  build-time `10.0.2.15` in `/etc/hosts` (`scripts/appliances/`), which makes
  the node's firewall auto-detect a wrong local network.
