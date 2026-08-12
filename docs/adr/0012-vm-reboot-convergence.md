# ADR-0012: A third convergence class for VMs — reboot, between update and replace

- **Status:** **Proposed** (2026-08-12)
- **Date:** 2026-08-12
- **Deciders:** Walter Angolar

## Context

A `kind: Vm` accepts 36 spec fields and `RECONCILED_VM_FIELDS` compares five:
`disk`, `vcpus`, `memory`, `network`, `backend`. Measured on a real host, a plan
for an EXISTING VM declaring `cpuTopology`, `tpm`, `vnc`, `machine`, `bootOrder`,
an `extraDisks` and an `extraNics` the machine did not have printed
`Summary: 1 to adopt` and proposed nothing. The control case — a genuinely
unknown field — *does* warn, so those seven were recognised, parsed and
discarded.

Two of the three parts of that defect are closed:

- **Persistence** (`43b3a85`): the `Vm` record now carries the boot shape
  (`VmBootSpec`), so `vm start`/`restart` reboot the machine that was created
  instead of a defaulted approximation of it.
- **Honesty** (`94f8ee1`): the plan now NAMES the declared fields it will not
  apply to an existing VM, instead of dropping them silently.

What is still missing is convergence itself, and it does not fit the model.

## The problem: the set of "safe to compare" fields is empty

`Action` has exactly two shapes for changing an existing resource:

- `Update` — "converges without recreating **and without changing the PID**".
  This engine does not hotplug: no VM field applies to a running machine.
- `Replace` — destroy and recreate. Recreating a VM discards its overlay, which
  is everything the guest wrote. That is why it is refused without `--replace`.

So extending `RECONCILED_VM_FIELDS` under today's model turns every added field
into a **destructive** `Replace`. Comparing more would mean comparing in order to
propose data loss. `create_with` also returns early on a running VM
(`return Ok(ex.clone())`), so an `apply` over a live VM does nothing even now
that the shape is persisted.

## What persistence unlocked

There is a third class, and it only became reachable once the boot shape was
persisted. Changing `tpm`, `machine`, `cpuModel`, `cpuTopology`, `vnc`, `video`,
`bootOrder`, `extraDisks` or `extraNics` is applicable by **stop + start**:

- the PID changes, so it is not an `Update`;
- the overlay survives, so it is not a `Replace`;
- and the next boot genuinely uses the new shape, which was not true before.

It is a reboot: **disruptive, not destructive.** The distinction is the whole
ADR — today the reconciler can express "free" and "costs you your data", and has
no way to say "costs you your uptime".

## Decision to take

Add a fourth action (working name `Action::Reboot`) that stops and starts the VM
with the new boot spec, preserving the overlay.

### Open questions, which are why this is Proposed and not Accepted

1. **May an `apply` reboot a VM without being asked?** A reboot is service
   interruption. `--replace` exists because destroying data must be explicit;
   the parallel argument says downtime should be too. The counter-argument is
   that a declarative apply that refuses to converge anything is not
   declarative. A `--reboot` flag alongside `--replace` is the obvious middle,
   at the cost of a second gate to explain.
2. **What does `--detailed-exitcode` return** for a change the user has not yet
   authorised? Today `2` means "there are changes". A reboot-pending change is a
   change, so `2` — but a CI drift gate would then go red on a VM nobody intends
   to reboot.
3. **Does `Action::Reboot` break the `-o json` contract?** ADR-0005 exists to keep
   machine-readable output stable. Adding an enum variant is additive for a
   reader that tolerates unknown values and breaking for one that matches
   exhaustively. This needs a stated compatibility rule, not an assumption.
4. **What about a VM that is stopped?** Applying the new shape then costs no
   downtime at all — arguably it should just converge, with no gate. That makes
   the action's cost depend on runtime state, which the plan would have to show.

## Consequences if accepted

- `kind: Vm` stops being the Kind that accepts thirty-six fields and honours
  five, which is the single largest gap between this engine's IaC and the
  "self-sustaining, no Terraform" goal (see
  `docs/discovery/50_VIRTUALIZACAO_FASE0.md`).
- The reconciler gains a cost model with three levels instead of two, which is
  closer to what infrastructure actually looks like and will likely be reused
  (a container `update` that needs a restart has the same shape today: it is
  either hot or it is nothing).

## Consequences if rejected

The honest fallback is what shipped in `94f8ee1`: name the unconverged fields
and tell the operator to apply them with `vm create`/`vm stop`+`start` by hand.
That is not silent, and silence was the actual defect.

## Deliberately not decided here

Whether the same class applies to `kind: Container`. A container restart is
cheaper and `container update` already converges ports, volumes, networks and
limits hot — the pressure that produced this ADR does not exist there yet.

## Timing

**Not before the production launch.** Written on 2026-08-12 with the launch ten
days out. Persistence and honesty are safe to ship now; a new action class that
can reboot a production VM during an `apply` is not something to introduce days
before a launch, and the fallback above is a defensible steady state.
