# ADR-0053: A Proxmox VM moves between the nodes of its cluster with `vm move --node`, and each VM is addressed on its own node

- **Status:** Accepted (2026-09-26, by the owner) — all six decisions implemented and tested:
  decisions 2 and 3 in the first addendum, decision 1 (`vm move`) on the lab cluster of decision 6
  in the second
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

## Addendum 2026-09-26 — decisions 2 and 3 implemented

**Decision 2.** The backend reads the node from the handle (`handle_parts`, replacing
`vmid_from_handle`, which took only the id) and every per-VM operation — status, IP, stop,
destroy, pause/unpause, cold resize, cloud-init change, resume, snapshots, the VM firewall —
goes through one helper, `on_vm`, that runs it on a client for that node. The client gained
`for_node(node)`: the same HTTP pool and credential with another node in its paths, no I/O,
the node name validated; none of the client's existing per-VM calls changed. Creation stays on
the configured node.

**Decision 3.** When the handle's node answers `NodeNotFound`, `on_vm` reads
`GET /cluster/resources?type=vm` once (`Client::locate_vm`); exactly one QEMU entry with the
same id on another node → a warning naming both nodes, the move remembered in the shared client
(so the next call goes straight there), and the operation retried once there; zero or two
entries → the original error stands. The engine gained `VmBackend::current_handle` (default
`None`, no I/O): `status()` — what `vm ls` runs — writes the handle the backend reports into the
record.

**Tested:** failure injection (VM addressed on its handle's node with the configured node never
asked; a moved VM found with ONE search, remembered, and its new handle reported; zero or two
matches keeping the not-found) — verified to fail with decision 2 reverted; an engine test for
`status()` persisting the reported handle; and live on the single-node lab: the five VM cases
re-run as a regression (no `/cluster/resources` request, every handle right) plus
`the_cluster_resource_list_places_a_vm_on_its_node` for the read itself. Matrix: 114/675 called,
111 in a live trace.

**Not measured:** following a VM to ANOTHER node, and a request for `/nodes/<other>/…` served
through the configured node's API — both need a second node, which is the lab of decision 6.

## Addendum 2026-09-26 — decision 1 implemented, on the lab cluster of decision 6

**The lab.** Two libvirt VMs on the developer host, both from this repository's appliance image
`proxmox-ve_9.2.qcow2` (PVE 9.2.2): `pve` (192.168.122.91, the API entry point) and `pve2`
(192.168.122.55), joined in a cluster `lab`, quorate, with a shared NFS storage `nfs-lab`
exported by `pve2`. Two things the appliance brought had to be fixed first, and both would bite
any cluster built from it: both disks carried the SAME `machine-id`, and `/etc/hosts` named the
build-time `10.0.2.15`. `ngola-lda` was not touched.

**The verb.** `delonix vm move <name> --node <target> [--live]`, behind
`VmBackend::move_to_node`, whose default refuses by name and names `vm migrate` (libvirt and
Cloud Hypervisor keep it, DX-1501). The engine refuses an empty target (DX-1538) and a power state
that does not match `--live` as the record says it — a paused VM either way (DX-5507) — before the
backend is asked, and writes the returned handle only after the backend's `Ok`.

**Refused before anything moves** (decision 4), each checked in failure injection against what
the node RECEIVED: the node the VM is on, a node that is not a member, an offline node (DX-1538,
and the precheck is not even sent); the power state as the NODE reports it (DX-5507); and what
`GET …/migrate?target=` says — local disks (the volumes named), local resources, a target outside
`allowed_nodes` (the storages it lacks named when the node says so). Measured on PVE 9.2.2: an
allowed target is ALSO a key of `not_allowed_nodes`, with an empty object, so only
`allowed_nodes` decides.

**Proved on the node** (decision 5): `POST …/migrate` goes through the task path and the ledger;
after it, `/cluster/resources` lists the VM on the target, its config is readable on the target
and not on the source, and a live move left it running. A lost answer is settled by the source
node's task list (`qmigrate`, which runs on the SOURCE; a live move also forks a `qmstart` on the
target) and by where the cluster lists the VM — never resent.

**The two behaviours this ADR marked "not measured"**, now measured: a request for
`/nodes/pve2/qemu/<vmid>/…` sent to `pve`'s API is served — the moved VM was started and stopped
on `pve2` through the entry point; and the precheck's answer shape, above.

**Two defects the live cases found**, both fixed here:

- **A task's status was read on the client's node.** A `qmigrate` stays on the source, so the
  next operation on the moved VM, settling the ledger through a client for the target, asked the
  wrong node. The status read now goes to the node the UPID names (validated, since the ledger is
  a file whose content goes into a URL). The live case moves a VM there and BACK, which is what
  exercises it.
- **The lock retry never fired for a move.** Right after a start the node's `qm cleanup` holds
  the VM's config lock (see `Client::task`), and a `qmigrate` turned away by it ends with the exit
  status `migration aborted` — the lock line is only in the task's log. A failed task's error now
  carries the last `ERROR:` line of its log (`GET …/tasks/{upid}/log`), skipping the node's
  `TASK ERROR:` summary, which only repeats the exit status. The first version took the summary,
  and its test asserted exactly that; the test now requires the lock line from the whole log.

**Tested:** engine (`move_refuses_before_the_backend_and_writes_the_handle_only_on_success`),
failure injection (`a_refused_move_never_sends_the_migrate`,
`a_move_is_sent_once_waited_on_and_proved_on_the_node`), unit (the measured precheck shape, the
node out of a UPID, the error line of a failed log), live against the lab cluster
(`a_stopped_vm_moves_to_another_node_and_the_cluster_lists_it_there`,
`a_running_vm_moves_live_on_shared_storage_and_keeps_running` — 2 passed; the lock retry fired
twice: 5 `POST …/migrate` for 3 completed moves), and battery checks for the CLI refusals.
`vm.migration.cold` and `vm.migration.live` are `supported` on Proxmox, citing the two live cases.
Matrix: 117/675 called, 114 in a live trace.

**Still out, as decided:** a VM with local disks is refused, not copied (the lab measured that
`--with-local-disks` works — an NBD mirror, 21 s for 1 GiB — so a later flag is feasible); no
creation on another node; HA excluded; no cross-cluster move.
