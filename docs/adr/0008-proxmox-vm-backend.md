# ADR-0008: Add a Proxmox VE backend as a separate crate, and make backends registrable

- **Status:** **Accepted, fully implemented** (phase 1 2026-08-10; phase 2
  2026-08-11; decision 2's populable registry and its first caller 2026-08-11,
  the whole lifecycle watched running through the CLI against a real node)
- **Date:** 2026-08-10
- **Deciders:** Walter Angolar

## Decision taken

**Accepted, and split**, because the two halves of this ADR have very different
evidence behind them.

**Phase 1 — the registry, now.** It is worth doing on its own merits, and it is
testable here. Today `backend_for` ends in `_ => CloudHypervisorBackend`: an
unknown backend name falls through to a default instead of failing, which is
guardrail #6 (no silent failure) broken in the one place a user is most likely
to typo. The registry closes that with a named error, and it is the change that
makes any third backend possible — Firecracker included. Small, pure, and
provable without a Proxmox host.

**Phase 2 — the Proxmox backend. UNBLOCKED (2026-08-11): the spike ran, and it
is a GO.** See "Spike result" at the end. What follows is the original text.

**Phase 2 — the Proxmox backend, deferred.** Not rejected: **blocked on a real
target**, the same way the kind spike was blocked and said so. This ADR itself
admits "the backend is not testable end-to-end here", and this repository does
not ship a compute backend it has never watched boot a VM. The appliance built
in this series (`proxmox-ve:9.1`) is the intended test target; when one runs
somewhere reachable, phase 2 starts with a GO/NO-GO spike against it, not with
a merge.

**What would flip this to Rejected:** if the registry turns out to cost more
than a `match` a reader takes in at a glance — the ADR's own condition. It does
not: the default registration stays inside `delonix-vm` and existing callers see
no change.

## Context

The engine can now build and run a Proxmox VE appliance image
(`proxmox-ve:9.1`, see the appliance work in `CLAUDE.md`), which makes the next
question concrete: a `delonix vm create` should be able to place the VM **on**
a Proxmox host through its API, not only on the local libvirt/Cloud Hypervisor.

Measured before proposing anything:

- **There is no Proxmox or TrueNAS API code anywhere in the workspace** (grep
  over `crates/` for `/api2/json`, `pve.?api`, `proxmox.*api`: no hits). This is
  greenfield, not a wiring job.
- **`delonix-vm` has four dependencies**: `delonix-runtime-core`,
  `delonix-net`, `tracing`, `libc` (`crates/delonix-vm/Cargo.toml`). It is one
  of the cleanest crates in the tree.
- **`VmBackend` is public and implementable from outside**
  (`crates/delonix-vm/src/lib.rs:499`), but **backend selection is not**:
  `fn backend_for(vm: &Vm) -> Box<dyn VmBackend>` (line 644) is a private
  `match` over two string literals with `_ => CloudHypervisorBackend`. Nothing
  outside the crate can add a third.

Guardrails in play (see the `delonix-adr` skill):

- **#2 (boundary with the private PaaS).** A Proxmox *driver* with
  multi-cluster inventory, a scheduler, or tenant↔resource mapping belongs to
  `delonix-paas`. A **single-node backend with no notion of tenant** is
  explicitly named as something that can live here.
- **#4 (engine crates stay dependency-clean).** Talking to a REST API needs an
  HTTP client. `reqwest` pulls in tokio, hyper and a TLS stack; putting that
  into a four-dependency engine crate is a supply-chain decision for a
  container/VM runtime, not a detail.

## Decision

1. **A new crate `delonix-proxmox`** implements `delonix_vm::VmBackend` against
   the Proxmox VE REST API (`/api2/json`). It depends on `delonix-vm` (for the
   trait and `VmConfig`), `delonix-runtime-core` and an HTTP client.
   `delonix-vm` gains **no** new dependency.

2. **Backend selection becomes registrable.** `backend_for` is replaced by a
   registry the caller populates, so a backend can be added without editing
   `delonix-vm`. The default registration (libvirt, Cloud Hypervisor) stays
   inside `delonix-vm`, so nothing changes for existing callers.

3. **Scope is one Proxmox node, addressed explicitly** — `--backend proxmox`
   plus an endpoint and an API token. No inventory, no scheduling, no choosing
   a node for the user. If a decision would need to know *who the customer is*,
   it is out of scope by guardrail #2.

4. **Credentials come from `kind: Secret`**, never from a flag or a manifest
   literal (the `Tunnel` `tokenSecretRef` precedent).

5. **An unknown backend name becomes an error.** Today `_ =>` silently falls
   through to Cloud Hypervisor; `valid_backend_name` makes that hard to reach
   from the CLI, but the registry must not re-create a silent default
   (guardrail #6).

## Alternatives considered

- **Put `reqwest` in `delonix-vm` and write the backend there.** Simplest
  diff, and rejected: it takes an engine crate from 4 dependencies to a tokio +
  hyper + TLS tree, for a feature most users never enable. Guardrail #4 exists
  for exactly this.
- **Implement the backend in `delonix-runtime-bin`**, where `reqwest` already
  lives (via `delonix-image`). Tempting, and rejected because `backend_for` is
  private: the bin cannot inject a backend without the registry change anyway,
  and putting a compute backend in the CLI crate puts it out of reach of
  `delonix-cri` and `delonix-mgmt`.
- **Shell out to `qm`/`pvesh` over SSH**, the way `cluster apply` drives
  `kubeadm`. Rejected: it needs shell access to the Proxmox host rather than an
  API token, and this repo has already paid for command injection through
  interpolated remote shell commands (audit finding #1, `CLAUDE.md`).
- **Do nothing** — keep Proxmox as an appliance you run, not a place you run
  things. Legitimate, and the right answer if the registry change proves to
  cost more than the backend is worth; this ADR should be rejected rather than
  half-implemented in that case.

## Consequences

**Easier.** A third backend stops being a special case: Firecracker, or a
single-node Proxmox, become "implement the trait and register it". The
`ComputeDriver` direction of ADR-0002 gets its second real consumer, which is
the bar that ADR set for promoting the trait.

**Harder.** Backend selection stops being a two-line `match` a reader can take
in at a glance. The registry must not become a plugin system — it is a map
populated at startup, nothing more.

**Debt accepted.** A REST backend cannot honour the whole of `VmConfig`:
hugepages, CPU affinity, 9p volumes and the raw libvirt XML escape hatches have
no Proxmox equivalent. Every one of those must be **refused with a message
naming the field**, never accepted and dropped — the failure mode this repo
treats as its worst.

**Testing.** There is no Proxmox host in this sandbox. The API client is
testable against recorded responses; the backend is not testable end-to-end
here, and that limit must be stated in the release notes rather than implied
away. The appliance image built in this same series (`proxmox-ve:9.1`) is the
obvious test target and should be used as one.

## Spike result (2026-08-11) — GO

Run against the `proxmox-ve:9.1` appliance this repo builds, booted the same way
`scripts/appliances/verify-boot.sh` boots the others (QEMU with a hostfwd), and
driven through the real REST API as `root@pam`. **Every method the `VmBackend`
trait declares has a Proxmox operation behind it, and the lifecycle was
exercised end to end** — a VM created, started (`status: running`, `pid: 1425`),
snapshotted, stopped and destroyed, with the node's VM list empty at the end.

| `VmBackend` | Proxmox | shape |
|---|---|---|
| `boot` | `POST /nodes/{n}/qemu` then `.../status/start` | UPID |
| `is_running` | `GET .../status/current` → `status`/`qmpstatus`/`pid` | sync |
| `stop` | `POST .../status/stop` then `DELETE .../qemu/{vmid}` | UPID |
| `snapshot` | `POST .../snapshot` | UPID |
| `snapshots` | `GET .../snapshot` | sync |
| `restore` | `POST .../snapshot/{name}/rollback` | UPID — **not exercised** |
| `ip` | `GET .../agent/network-get-interfaces` | needs the guest agent |

**The finding that matters most for whoever writes this backend: almost
everything is an asynchronous task.** A create, a start, a snapshot and a
destroy each answer with a bare `UPID:pve:…` string, not a result. The outcome
is read separately at `/nodes/{n}/tasks/{upid}/status`, and its shape is a trap:

```json
{"status": "stopped", "exitstatus": "OK", "type": "qmsnapshot"}
```

`status: stopped` means **the task finished**, not that it failed — the verdict
lives in `exitstatus`. A client that reads `status` as the result concludes the
exact opposite of the truth. This is the same class of trap the TrueNAS
provisioner already handles (`delonix-truenas`'s `wait_job`), and the two should
share the discipline, not the code: the payloads have nothing in common.

Two smaller findings, both worth knowing before writing the first request:

* **`net0=virtio,bridge=vmbr0` is refused** — `duplicate key in comma-separated
  list property: model`, and spelling it `model=virtio,…` is refused too. The
  NIC syntax needs its own pass; the spike proceeded without a network, which
  the lifecycle does not need.
* **`GET .../snapshot` includes a pseudo-entry named `current`**, which is not a
  snapshot. Listing it as one would report a snapshot nobody took.

**Two defects in the appliance itself, found on the way** (they belong to
`scripts/appliances/`, not to the backend):

1. **The published Proxmox images carry a STATIC IP from the build environment.**
   `source = "from-dhcp"` in the answer file means "get the configuration by
   DHCP *during installation* and write it down as static" — not "use DHCP at
   boot". A VM booted by `delonix vm create --backend libvirt` therefore comes up
   with `10.0.2.15`, the QEMU slirp address, and is unreachable on a libvirt NAT
   network. Confirmed by screenshot of the guest's own console.
2. **No serial console**: the Proxmox installer does not add `console=ttyS0` to
   grub, so a guest that fails to reach the network cannot be observed at all
   without a graphics device. `delonix vm create --vnc` was what made the
   diagnosis possible — worth remembering as the tool for this.

Both affect all four Proxmox appliances.

## What the spike did NOT settle: the trait assumes a local disk

The API side is a GO — every method has an operation behind it, and the
lifecycle ran. **The engine side is not, and this is the honest blocker now.**
Reading `create_with` before writing the first line of the backend:

```rust
let disk_path = std::fs::canonicalize(&cfg.disk)      // must exist HERE
    .map_err(|_| Error::Invalid(format!("image not found: {}", cfg.disk)))?;
let overlay = vmdir.join(format!("{}.qcow2", cfg.name));
qemu-img create -f qcow2 -b <disk_path> … <overlay>   // a LOCAL overlay
backend.boot(&vmdir, cfg, &overlay.to_string_lossy(), on)
```

Every one of those three steps runs **before** any backend is consulted, and
every one is meaningless for a hypervisor on another machine:

* the base image has to be on the Proxmox node, not on this filesystem — and
  `canonicalize` fails here first, so the backend never even gets asked;
* the local overlay is waste: Proxmox manages its own disks (LVM-thin, ZFS, a
  storage of its own), and a qcow2 made here backs nothing there;
* `boot(…, overlay: &str, …)` hands over a **local path** as the thing to boot.

So the blocker moved rather than disappeared: it is no longer "no target to
spike against", it is "**`VmBackend` is not implementable by a remote backend
without a change to the engine**". Pretending otherwise would mean writing a
backend that uploads a local overlay to a remote node on every create — a second
disk model, and slow — purely to satisfy a signature.

A second, smaller instance of the same thing: **`available()` is called during
auto-detection** (`select_backend` walks the registry asking each). For a local
backend that is a `which`; for a remote one the only honest answer needs a
network round trip, so auto-detection would start making HTTP requests to a node
that may not be configured at all.

**The minimal fix, and it is additive**: a method on the trait with a default —
something like `fn manages_own_storage(&self) -> bool { false }` — that
`create_with` consults before the canonicalize/overlay block, plus never
including a remote backend in auto-detection (it is selected explicitly or not
at all). No existing signature changes, no existing implementation breaks, and
the skill's "do not touch the trait" holds in substance: nothing that exists
today is altered.

**That decision belongs to this ADR and has not been taken.** Phase 2 is
therefore: (a) decide and land the storage/detection split above; (b) then the
backend. Writing (b) first would bake the wrong assumption into a crate.

## Where this actually stands (2026-08-11, code review)

Read against the code rather than against this document, three of those four
things are already done and one is not:

* `VmBackend::manages_own_storage()` and `auto_selectable()` **exist**, with
  defaults, and `create_with`/`select_backend` **consult them** — the
  storage/detection split of (a) landed.
* `delonix-proxmox` **exists and implements the trait**, with the async-task
  handling this ADR's spike identified.
* `ProxmoxBackend::boot` is **deliberately unimplemented** and says so in its
  own error: the engine does not ship a create path nobody has watched run.
  *(Superseded the same day — see "Phase 2, done" below: `boot` was written and
  watched running against a real node.)*

What is genuinely missing is the half of decision 2 that never landed: **the
registry is a `static` table, so "a registry the caller populates" is not
possible today.** A crate that depends on `delonix-vm` (as `delonix-proxmox`
must, for the trait) cannot add itself to a `static` inside `delonix-vm`. Phase 2
therefore needs, in order: a populatable registry whose entries can carry
configuration (a Proxmox backend needs an endpoint and a token, and
`fn() -> Box<dyn VmBackend>` cannot receive either), credentials resolved from a
`kind: Secret` per decision 4, and only then `boot`, written against a live node.

Until then `--backend proxmox` no longer answers «unknown backend» — which told
the operator the crate does not exist, when it is sitting in the workspace. It
now names the actual state and points here (`KNOWN_UNREGISTERED`, in
`delonix-vm`). It remains **refused**: nothing about this makes an unfinished
backend selectable.

## Decision 2 landed (2026-08-11) — the registry takes a closure

The paragraph above was right that the `static` table was the blocker, and one
sentence of it explains the whole shape of the fix: `fn() -> Box<dyn VmBackend>`
has nowhere to receive an endpoint, a node name and a credential. So a
registration carries a **closure**, and the closure is where a caller's
configuration lives:

```rust
pub type BackendFactory = Box<dyn Fn() -> Result<Box<dyn VmBackend>> + Send + Sync>;
pub fn register_backend(reg: BackendRegistration) -> Result<()>;
```

`Send + Sync` constrains the CLOSURE, not the trait — no backend implementation
changed, and the skill's "do not touch the trait" holds literally.

Four properties, each one a refusal or an ordering that a plainer map would not
have:

* **Registering does no I/O.** The factory is not called, so a node that is
  unreachable — or simply not there — costs nothing until somebody selects the
  backend by name.
* **`auto_selectable` is a field of the REGISTRATION, not just a method on the
  built backend.** Auto-detection has to answer it *without building*, because
  construction is where a remote backend authenticates. The old
  `select_backend` did `.map(build).filter(auto_selectable)` — it built every
  candidate and threw the wrong ones away, which for a remote backend is
  precisely the network round trip the flag exists to prevent. Free for the two
  local backends, which is why nothing noticed.
* **A third-party registration may not be auto-selectable at all**, and asking
  for it is refused by name. Auto-detection asks `available()`, and a backend
  from outside this crate may only be able to answer that over the network.
* **Names are owned.** An id or alias already belonging to a different backend
  is refused: the loser would become unreachable by name, silently, which is the
  same class of failure as the `_ => CloudHypervisorBackend` default this ADR
  removed. Re-registering the *same* id replaces it, so reconfiguring a target
  does not leave two entries shadowing each other.

**The first caller is `delonix-runtime-bin`** (`cmd/vmbackends.rs`), called once
from `run()`. It reads `DELONIX_PROXMOX_URL` / `_NODE` and a credential — a
`kind: Secret` named by `DELONIX_PROXMOX_SECRET` first, per decision 4 — and
registers the backend. Environment and not a manifest field because `create_with`
resolves the backend itself and never receives a target: the registration has to
be in place *before* the engine is asked.

A misconfigured target is **reported and skipped, not fatal**: a typo in
`DELONIX_PROXMOX_TOKEN` must not stop `delonix container ls` from running. The
name then stays unregistered and `--backend proxmox` says how to configure it,
which is the state the operator is actually in.

`ProxmoxBackend` now holds an `Arc<Client>`, and the registered factory clones
it. Without that, every `backend_for` — which is once per VM in `vm ls` — meant
a fresh authenticate plus `GET /nodes`: listing ten VMs was thirty round trips
where twelve do.

**Watched running end to end through the CLI**, against the `proxmox-ve:9.1`
appliance this repo builds:

```
$ delonix vm create pvedemo --backend proxmox --disk local-lvm:1 --memory 512M
 ✓ defining the domain 📋 0.4s      ✓ starting the VM ▶ 0.8s
$ delonix vm ls    → pvedemo  1  512M  Running
$ delonix vm rm pvedemo
$ GET /nodes/pve/qemu → {"data":[]}
```

### Three defects the same pass found and fixed

1. **`create_with` deleted `cfg.disk` when a boot failed.** The cleanup was
   `remove_file(&overlay)`, and with `manages_own_storage` the overlay IS
   `cfg.disk` verbatim — the name the caller wrote for something on the far
   node. For today's backend that name is `local-lvm:8` and the unlink simply
   fails, but the rule cannot rest on the spelling a backend happens to use: a
   remote backend whose disk reference is a local path would lose the user's
   base image. **This branch had never been exercised by anything** — no
   registered backend reached it, which is what a registry nobody can add to
   costs.
2. **`mem_mib` was re-implemented in `delonix-proxmox`, and the copy was
   wrong.** It did not know the k8s `Gi`/`Mi` suffix the engine tolerates, so
   `memory: 2Gi` meant 2 GiB on libvirt and Cloud Hypervisor and **1 GiB** on
   Proxmox — silently, and the copy did not warn on an unparseable value either.
   The engine's `mem_mib` is now `pub` and shared: one definition for everyone
   who reads the field, the same discipline as `fw_rule_tail`.
3. **`urlencode` encoded code points, not bytes.** `other as u32` is right only
   below 0x80: a `ç` in an account name became `%E7` instead of its two UTF-8
   bytes, and anything above 0xFF produced `%1F600`, which is not
   percent-encoding at all. A UPID comes back from the node with the account
   name inside it, so the input was never ours to assume ASCII.

**A method note worth more than any of the three.** The first test written for
defect (1)'s sibling — the auto-detection ordering — registered the remote
candidate in the global registry and called `select_backend(None)`. It passed
with the bug still in: this host has a local backend installed, the walk stops
at the first entry, and the remote one is never reached either way. The fix was
to extract `auto_detect(&[BackendRegistration])` and hand the test a table where
the skipped candidate is actually reached. Each of the three fixes was then
verified by reverting it and watching its test fail.

### The guest agent (2026-08-12)

`ip()` no longer returns a flat `None`. Two halves, and only one of them is
this side's to control:

* **The channel**, which is the host side: `create_vm` sends `agent=1`, adding
  the virtio-serial port the agent talks over. Without it the node does not even
  try — every `/agent/…` call answers "not running" no matter what the guest
  has installed. Measured: the setting comes back in the VM's config, and the
  live test now asserts it on the VM the backend itself created.
  `clone_template` deliberately does NOT force it: a clone inherits the
  template's configuration, and overriding would contradict a choice somebody
  made about that template.
* **The agent**, which is the image's: `parse_agent_ip` reads
  `data.result[].ip-addresses[]` and takes the first IPv4 that is not loopback,
  not IPv6 and **not `169.254.0.0/16`** — that last one is what an interface has
  when DHCP *failed*, so reporting it would say "the VM has an address" when the
  truth is the opposite. `lo` is skipped by name *and* 127/8 by value, because
  it is the first entry the agent returns and "the first IPv4" would be
  loopback for every guest there is.

**`None` is a first-class answer, not a failure.** A guest with no agent makes
the node reply HTTP 500 `"QEMU guest agent is not running"` (measured), which is
the ordinary case for a plain cloud image; `vm ls` calls `ip()` for every VM on
every listing, so it costs a `None` and a `debug` line, never a warning. The
`debug` line is there because "no agent" and "the token lost its permissions"
both show up as an empty IP column, and only one of those is fine.

**What was NOT validated live, and why.** The parser was driven with recorded
answers, not with an agent talking. Getting one needed a nested guest with
`qemu-guest-agent` installed: the node boots and has egress, and a Debian
genericcloud image was downloaded onto it and booted as VM 901 — but that image
does not ship the agent, and installing it into a guest two levels down was
costing more than the evidence was worth. What IS live: the channel on a
backend-created VM, and `ip()` answering `None` quietly for a guest with no
agent. What is not: an actual address coming back.

**A correction this pass owes the reader.** The note above saying
`net0=virtio,bridge=vmbr0` is *refused* by the API was wrong, and wrong in a way
worth naming: it was an artefact of the spike's `curl -d`, which does not
URL-encode. `reqwest`'s `.form()` does, and the node accepts the same string —
measured, the resulting config reads `net0 = virtio=BC:24:11:F4:F9:9C,bridge=vmbr0`.
The backend had been sending it correctly all along; only the ADR was wrong.

### Closing the rest (2026-08-12)

The list above was the open one; this closes it. Two of the items turned out to
be data-loss bugs rather than gaps.

**`vm stop` was destroying the guest's disk.** `stop` and `destroy` are the same
operation on a local backend — libvirt undefines the domain and
`<root>/vms/<name>.qcow2`, which the *engine* owns, is untouched — and they are
not the same remotely, where the only call that frees the VM also frees its
disk. This backend implemented `stop` as "stop and destroy" for a good reason (a
VM left behind after `vm rm` is an orphan) wired to the wrong verb: the engine
calls `stop` for `vm stop` too, and the CLI's own next-steps block promises
`stop it (keeps the disk)`. `VmBackend::destroy` is new, defaults to `stop` so
both local backends are byte-for-byte unchanged, and `remove_inner` calls it.

**`vm start` on a stopped remote VM built a second one.** `boot` asks the node
for the next free id, so the record was rewritten to a fresh VM with an empty
disk and the original was orphaned — data still on the node, nothing pointing at
it. `VmBackend::resume` is new, defaults to `Ok(None)` (both local backends'
`boot` is already idempotent through the per-VM overlay), and the Proxmox one
starts the vmid its record names, answering `None` only when the node no longer
has it.

**Fields are refused by name.** `refuse_unsupported` runs before anything is
created and names every field this backend cannot honour, grouped by why: the
guest is on another machine (`kernel`, `initrd`, `firmware`, `cmdline`, `seed`,
`devices`, `volumes`), Proxmox owns the QEMU knobs itself (`hugepages`,
`cpuAffinity`, `machine`, `cpuModel`, `cpuTopology`, `tpm`, `video`,
`bootOrder`), or there is no domain XML here at all (`libvirtXml*`). Several are
reported together — fixing them one error at a time is one create attempt per
field.

This immediately caught a real case: **the CLI generates a NoCloud seed ISO
unconditionally**, which is a file on this host and unreadable from a node
elsewhere, so a plain `vm create --backend proxmox` failed on a seed nobody
asked for. The CLI now skips it for a backend that owns its storage
(`delonix_vm::backend_manages_own_storage`) and refuses `--hostname`/`--ssh-key`/
`--user-data` there with the reason — the same shape the appliance-image refusal
right above it already uses.

**Snapshots** are implemented (`snapshot`/`restore`/`snapshots`), with
`vmstate=1` so a snapshot of a running VM is a system checkpoint like the
libvirt backend's, and with the API's pseudo-entry `current` filtered out of the
listing *and* refused as a name — otherwise `vm restore <vm> current` would look
like a supported thing to do.

**Networking** takes a bridge (`VmConfig.bridge`, falling back to the target's,
falling back to `vmbr0`) and a VLAN tag from the target. The tag lives with the
target because it describes how the node is cabled, not the VM; an out-of-range
value is an **error**, never a `None`, because dropping it would put the VM on
the untagged network while the operator believes it is isolated.

**The ticket expires** (2 h) and this client is shared for the life of the
process, so `send_authed` re-authenticates once on a 401 — only for password
auth, since an API token that gets a 401 was revoked and retrying it forever is
how a credential gets locked out.

Watched running through the CLI against the real node: `create` → `stop` (vmid
100 **still on the node**) → `start` (same handle, no second VM) → `snapshot` →
`snapshots` → `restore` → `rm` (node empty). Each of the three bug fixes was
reverted one at a time and its test fails.

### Still open, deliberately

A second NIC, and an address from an agent (see above — the parser runs against
recorded answers).

## Phase 2, done (2026-08-11)

The storage/detection split landed first (`manages_own_storage`,
`auto_selectable` — see the section above on why the trait blocked this), and
then the backend itself. Watched running against a real Proxmox VE 9.1:

```
stage: Define · stage: Start
created and started: handle=proxmox:pve:100
is_running -> true · stop -> destroyed · is_running -> false
VMs on the node afterwards: none
```

`cfg.disk` names something on the FAR side, in two forms — `template:<vmid>` to
clone, `<storage>:<gib>` for a fresh disk — and anything else is refused naming
both. The likeliest mistake is a local path, the habit every other backend
teaches, and treating it as a storage name would create a disk nobody asked for.

The vmid lives in the record (`api_socket` = `proxmox:<node>:<vmid>`), never the
name: two VMs on a node may share one and every `qm` call takes the id. A record
without that handle was not created by this backend and is refused rather than
acted on.

**Still open, and deliberately**: `ip()` returns `None` — reaching a guest's
address needs the QEMU guest agent inside it, and inventing one from the config
would be worse than saying nothing. Snapshots use the trait's fail-closed
default; the API supports them (the spike exercised one) but nothing here has
called it.
