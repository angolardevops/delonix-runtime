# ADR-0053: A Proxmox VM moves between the nodes of its cluster with `vm move --node`, and each VM is addressed on its own node

- **Status:** Proposed
- **Date:** 2026-09-25
- **Deciders:** Walter Angolar
- **Numbering:** drafted as ADR-0052 (#511); that number was taken first by
  `0052-proxmox-vm-firewall-as-a-network-provider.md` (#512), so this record is ADR-0053 without
  changing what it decides.
- **Related:** ADR-0008 (the Proxmox backend addresses ONE node, explicitly, and never picks
  one — decision 3 is amended here, see Consequences); ADR-0031 (`vm migrate` is stop-copy-start
  between two `delonix` hosts, and live migration stays out of scope for the local backends);
  ADR-0049 (slice 3, "Slice 3 writes: what they need before they start", which asks for this
  decision before any code); ADR-0050 (the capability catalogue: `vm.migration.cold`,
  `vm.migration.live`).

## Context

ADR-0049's slice 3 has its read-only half: `delonix provider describe proxmox --probe` (#505)
reads the target's cluster and says, per capability, whether that cluster could migrate a VM
(quorate, two online nodes, a shared storage that holds VM disks). The writes — actually moving a
VM — were stopped on purpose until two questions had a written answer, because each has a wrong
answer that is cheap to code.

**1. Which verb.** `vm migrate` exists (ADR-0031, PR #220): it stops the VM, flattens the
overlay, copies it over SSH to ANOTHER `delonix` host, imports it there and creates a NEW VM
from it — a new record on another machine, `--remove-source` optional. Moving a Proxmox VM to
another node of the SAME cluster is a different operation with a different outcome: the same
VM, the same `vmid`, the same engine record, the same backend; only the node changes, and the
node does the copying (or nothing at all, on shared storage). `vm migrate`'s required flags
(`--host`, `--network`) and optional ones (`--ssh-key`, `--ssh-port`, `--vcpus`, `--memory`,
`--remove-source`) mean nothing for a node move, and a node move's only input — the target node
— means nothing for a host move.

**2. Which node the backend talks to about a VM.** ADR-0008 decision 3: "Scope is one Proxmox
node, addressed explicitly — no choosing a node for the user". The code reads that literally:
the client builds every per-VM path from the configured node (`self.node`, 63 uses in
`crates/providers/delonix-proxmox/src/lib.rs`, most of them per-VM paths), and the handle a VM
is recorded with, `proxmox:<node>:<vmid>`, is written but its node is never read back
(`vmid_from_handle` takes only the id). A VM on another node is therefore addressed on the wrong one.

That is **not hypothetical today**, which is what makes it the central point of this decision
rather than the verb: by the code above, a VM migrated from the Proxmox UI — the operator's
surface, which the engine does not control — makes every later `vm stop`/`vm start`/`vm destroy`
of it ask the configured node, which answers "does not exist" (`NodeNotFound`, DX-4504) while the
VM runs one node over. This follows from reading the client; it has **not been reproduced**,
because it needs a second node.

What was measured, and what was not:

- `ngola-lda` (2026-09-25, read-only): quorate, one of three nodes offline, no shared storage
  that holds VM disks — it could not move a VM without copying its disk, and it is production.
- The lab node `pve-lab-475` is a single node: no second node to move to.
- **Not measured:** that a request for `/nodes/<other>/qemu/<vmid>/…` sent to the configured
  node's API is served for a cluster member (Proxmox proxies it; this is the documented
  behaviour, not yet a measurement here), and the exact answer shape of
  `GET /nodes/{node}/qemu/{vmid}/migrate` (the precheck) on PVE 9.2.2. Both are the first thing
  the lab cluster measures.

Guardrails this touches: **#2** — choosing where a VM runs is scheduling, which belongs to a
consumer; the engine may move a VM only where the caller says. **#6** — a move the node would
refuse, or complete differently than asked (copying local disks the caller did not know about),
must be refused before it starts. No daemon (#1), no new dependency (#4): the client already
speaks the API.

## Decision

1. **A new verb, `delonix vm move <name> --node <target> [--live]`.** The target node is always
   the caller's; there is no default and no selection. `--live` asks for an online move (the VM
   keeps running); without it the move is offline and the VM must be stopped. It is a
   `VmBackend` method whose default refuses by name, so libvirt and Cloud Hypervisor — which
   have no cluster — refuse it with the verb that fits them (`vm migrate`) named in the message.
   `vm migrate` keeps its meaning (ADR-0031) and gains nothing.

2. **Each VM is addressed on its own node.** The node in the handle becomes the node every
   per-VM operation uses; the configured node stays what it already is for everything else —
   the API entry point and the node NEW VMs are created on. Creating on another node stays out
   of scope (that is where a scheduler would start). A successful move rewrites the handle to
   `proxmox:<target>:<vmid>` before the command returns.

3. **A VM moved outside the engine is found, not guessed.** When a per-VM call answers
   `NodeNotFound` on the handle's node, the backend reads `GET /cluster/resources?type=vm` once
   and looks for the SAME `vmid` in the same cluster. Found on one node: the handle is
   rewritten, the operation retried once on that node, and the move is said out loud (a line on
   stderr naming both nodes). Not found, or found twice: the original `NodeNotFound` stands.
   This reads where the VM already is; it never chooses where a VM should be, which is what
   guardrail #2 forbids.

4. **Refused before the move starts**, from the node's own precheck
   (`GET /nodes/{node}/qemu/{vmid}/migrate?target=<target>`) and the engine's record:
   - the target is not a member of the VM's cluster, is offline, or is the node the VM is on;
   - the precheck lists the target among the nodes that cannot host the VM (a storage the
     target does not have), and the refusal names the storages;
   - the VM has LOCAL disks (the precheck's `local_disks`): copying them is a different cost
     and a different failure window, and it is not what a caller asking for a move on a
     cluster expects. Allowing it is a later, explicit flag, not this decision;
   - `--live` on a stopped VM, or no `--live` on a running one.
   Classes: an invalid target is an invalid argument; a VM in the wrong power state is a
   conflict (the same class as `vm resize` on a running VM, DX-5505's family).

5. **The proof is the node's, per ADR-0049's rules.** `POST …/migrate` answers a UPID and goes
   through `wait_task` and the ledger. After it: `/cluster/resources` reports the VM on the
   target, its config is readable on the target and not on the source, and for `--live` its
   status stayed `running` throughout. A lost answer is reconciled by the task list (the node's
   worker type, measured in the lab) and the same `/cluster/resources` read, never resent.

6. **Proved on a lab cluster, never on `ngola-lda`.** Two nodes, quorate, one shared storage
   holding VM disks. Until that lab exists the verb is not merged: code without its live case
   would be the kind of `supported` this repository refuses (ADR-0050: `supported` names its
   evidence).

## Alternatives considered

- **`--node` as a flag of `vm migrate`.** One verb for "relocate a VM". Rejected: the two
  mechanisms produce different identities (a new VM on another host vs. the same VM on another
  node), and every flag of one is meaningless for the other — `clap` can forbid the
  combinations, but the help text would describe two commands in one, and a script reading
  `vm migrate` could not know which identity it gets back.
- **Keep one node per backend; register a second backend per node.** Each node its own target,
  a VM moved between them by the caller. Rejected: the VM's record belongs to one backend
  instance, so a move would be a record hand-over between registrations that the engine has no
  owner for, and it multiplies credentials for what the cluster already exposes through one
  API endpoint.
- **Let the engine pick the target** (the least loaded node, the first allowed by the
  precheck). Rejected by guardrail #2: that is scheduling.
- **Do nothing, and document that the engine loses a VM moved from the UI.** Rejected: that is
  the silent failure guardrail #6 names — the record says a VM exists and every command about it
  says it does not.
- **Search `/cluster/resources` on every call instead of only after `NodeNotFound`.** Rejected:
  a round trip on every `vm ls` row for a case that is rare, where the cheap trigger (the node
  said "not here") already exists.

## Consequences

- **ADR-0008 decision 3 is amended, not superseded.** "One Proxmox node, addressed explicitly"
  becomes "one API entry point and one node for creation, addressed explicitly; an existing VM
  is addressed on the node its handle names". Everything else in ADR-0008 stands, including "no
  choosing a node for the user".
- The client grows a node parameter on its per-VM calls (or a per-VM client view); the 63
  `self.node` uses bound the size of that change. The ledger keeps working per VM directory.
- `vm.migration.cold` and `vm.migration.live` become reachable on Proxmox; they stay what they
  are in the catalogue until the lab's live cases name their evidence, and the probe's
  cluster verdicts (#505) keep saying whether a given cluster could do it at all.
- Decision 3 fixes a real defect of today's backend independently of `vm move`: a VM moved from
  the Proxmox UI is found again. It can land before the verb, with a failure-injection case
  (NodeNotFound, then `/cluster/resources`), and a live case once the lab exists.
- **Known limits, stated:** no move of a VM with local disks; no creation on a node other than
  the configured one; HA stays excluded (ADR-0049 D3); cross-cluster moves (`remote_migrate`)
  are out of scope; and nothing here is measured yet — the two unmeasured behaviours in
  Context are the first lab task.
