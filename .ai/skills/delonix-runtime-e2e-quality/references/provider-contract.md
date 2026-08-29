# Provider Contract — Delonix Runtime E2E

Reference companion to `SKILL.md` §6.D and §6.K. Defines the canonical
contract every VM/compute provider is expected to honor, so a provider-specific
suite (`LIBVIRT-###`, `PROXMOX-###`, `OPENSTACK-###`) tests the SAME
behavioral contract instead of drifting into testing each provider's own
idea of what "create a VM" means.

## Why a shared contract

Delonix's VM layer already routes through a single backend trait
(`VmBackend` in `crates/delonix-vm`) — the whole point of that abstraction
is that `delonix vm create` means the same thing regardless of which
provider is configured underneath. An E2E suite that tests each provider
with a different set of assertions would let that abstraction silently leak
provider-specific behavior without anyone noticing. Test the contract once,
run it against every configured provider, and treat any divergence between
providers as a finding — either a `BUG` (the abstraction leaks) or a `GAP`
(a capability one provider has that the trait doesn't expose to the others).

## The canonical contract

For every provider under test, the following must hold — this is the
"same command, same effect" bar from `SKILL.md` §3 applied specifically to
compute provisioning:

| Operation | Contract |
|---|---|
| `create` | Idempotent: calling it twice with the same manifest either converges to the same state or explicitly refuses as a conflict — never silently creates a duplicate. |
| `create` | The resulting resource has *exactly* the declared CPU, RAM, disk, and network — not "close enough," and no silently-applied host default overriding an explicit request. |
| `start`/`stop` | State transitions are observable through `inspect`/`status` before the command returns, or the command documents that it's asynchronous and gives a way to wait for completion. |
| `delete` | Removes every resource the provider allocated for it (disk, network attachment, any provider-side registration) — a partial delete that leaves an orphan on the provider side is a `BUG`, not a `GAP`, since the create path proves the provider can be asked to clean up what it created. |
| Failure | A provider-side failure (timeout, auth failure, resource exhaustion) surfaces as a distinguishable error — not folded into the same generic failure a local misconfiguration would produce. |
| Failure | A `create` that fails partway through does not leave a resource visible to `ls`/`inspect` that doesn't actually work — either it's fully there or it's cleaned up. |

## Provider-specific test additions

The shared contract is the floor, not the ceiling. Each provider suite adds
tests for capabilities that are genuinely provider-specific and should be
labeled as such rather than silently assumed universal:

- **libvirt** — snapshot/restore semantics, live migration if configured,
  the specific network modes Delonix wires up (NAT vs. bridged vs. isolated
  namespace), VNC/console access.
- **Proxmox** — whatever Proxmox-specific resource model exists on top of
  the shared contract (storage pools, node placement) — verify against the
  actual crate (`crates/delonix-proxmox`) rather than assumed Proxmox API
  behavior, since the contract Delonix presents may differ from raw Proxmox
  semantics by design.
- **OpenStack** — only test what's actually implemented; do not assume
  feature parity with libvirt/Proxmox exists until the provider registry
  (`delonix api-resources`, or the equivalent provider listing) confirms it.

## Do not assume providers before checking

Per `SKILL.md` §5's "do not assume Kinds exist" — the same discipline
applies to providers. Before writing a `PROXMOX-###` or `OPENSTACK-###`
test, confirm the provider is actually wired into the build under test
(feature flags, compiled-in support, configured credentials) rather than
assuming it from documentation or from what an earlier version supported.
A provider that isn't configured in this environment is `SKIPPED` or
`BLOCKED`, never silently omitted from the report without a line saying why.
