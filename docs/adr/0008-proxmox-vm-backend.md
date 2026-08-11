# ADR-0008: Add a Proxmox VE backend as a separate crate, and make backends registrable

- **Status:** **Accepted, in two phases** (2026-08-10) — phase 1 landed;
  **phase 2 UNBLOCKED 2026-08-11 by a GO spike against a real appliance**
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
