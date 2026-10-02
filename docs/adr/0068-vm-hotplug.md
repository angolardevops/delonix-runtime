# ADR-0068: VM hotplug — CPU, memory, disk and NIC while the VM runs

- **Status:** Proposed (2026-10-02). Nothing implemented; the spike is in Annex A. The owner's
  answers of 2026-10-02 are recorded as decisions (D2, D4, D8, D10); the questions he left open
  stay open, with a recommendation.
- **Date:** 2026-10-02
- **Deciders:** Walter Angolar
- **Relates to:** decision D8 of the maturity plan (`docs/discovery/65_PLANO_MATURIDADE.md`,
  PR #658), ADR-0008 (VM backend registry), ADR-0043 (the `DX-CDNN` dictionary), ADR-0050
  (capability catalogue, evidence required for `supported`), ADR-0053 (`vm move`), ADR-0055
  (per-VM anti-spoof), the three-way reconciler (`crates/contexts/delonix-stack/src/reconcile.rs`),
  and guard-rail 6 of `delonix-adr` (no silent failure).
- **Copies:** this file is canonical. `0068-vm-hotplug.pt-AO.md` is an internal review copy in
  Portuguese; the two carry the same ID, status, decisions and references.

## Context

Today a VM changes shape only while stopped: `vm resize` is the cold resize (`vm.resize.cold`;
`VmEngine::resize` refuses `Running`/`Paused` with DX-5505), and `kind: VirtualMachine` has no hot
field — `hot_fields("Vm")` is empty, so changing `vcpus` or `memory` in a manifest plans a
`Replace`, which is refused without `--replace` because it throws away the overlay disk. The
capability matrix says `vm.hotplug: not-implemented` on all three backends and `vm.disk.resize`
only `partial` on Proxmox. The owner approved hotplug (D8 of plan 65): CPU, memory, disk and NIC
added or removed while the VM runs, on the backends that support it.

The container side already has the shape to copy: `container update` reconfigures ports, volumes,
networks and limits **live, with the PID unchanged**, writes each operation to the record as soon
as the dataplane confirms it, and the reconciler has `memory`/`cpus`/`ports`/`volumes` in
`hot_fields(CONTAINER)`.

### What was measured (Annex A, 2026-10-02, on this host)

Cloud Hypervisor v53.0 with the EDK2 `CLOUDHV.fd` and an EMPTY qcow2 (no guest OS); libvirt
10.0.0 on `qemu:///session`, q35, a throwaway domain. Without a guest, what is measured is the
hypervisor side; what the guest does with the change (onlining CPUs, plugging memory, acking an
eject) **is not observable without a guest**, and the annex says so on each line.

1. **The hotplug ceiling is fixed at boot and cannot change live.** CH: `--cpus boot=1,max=4` and
   `--memory size=256M,hotplug_size=512M`; asking for 5 vCPUs gives `Requested vCPUs exceed
   maximum`, asking for 1 GiB gives `Not enough space in the hotplug RAM region` (ACPI) or `new size
   … is bigger than region_size` (virtio-mem). libvirt: `<vcpu current='1'>4</vcpu>` and
   `<maxMemory slots='4'>`; `setvcpus 5` gives `5 > 4`. A VM created without headroom takes no
   hotplug at all.
2. **Adding CPUs works on the hypervisor side without the guest.** CH: 1→3 gives 204 and threads
   `vcpu1`/`vcpu2` appear. libvirt: `setvcpus 3 --live` gives `current live 3`.
3. **Removing CPUs depends on the guest, and the hypervisor's answer does not prove it.** CH: 3→2
   gives 204 and `vm.info` says `boot_vcpus 2`, but thread `vcpu2` is still alive (the guest did not
   eject), and the next request gets **HTTP 429 "Too Many Requests"** while the removal is pending.
   libvirt: `setvcpus 1 --live` is **refused** (`device unplug request for not supported device
   type: host-x86_64-cpu`) because the vCPUs were not declared `hotpluggable='yes'`.
4. **Memory: the request is not the result.** CH with virtio-mem: 256→512 MiB gives 204,
   `config.memory.hotplugged_size` becomes 256 MiB and `memory_actual_size` **stays at 256 MiB** —
   with no virtio-mem driver in the guest nothing is plugged. CH with ACPI: 512→256 gives **204 and
   `vm.info` says 256 MiB**, although ACPI cannot remove memory — a report the engine must not
   repeat. libvirt: `setmem 128M --live` (balloon) gives **rc=0 and nothing changes**
   (`dommemstat actual` stays at the maximum — no balloon driver); a `<memory model='dimm'>` via
   `attach-device --live` works on the hypervisor side (`Max memory` 256→512 MiB).
5. **Removing a disk depends on the guest, and the identifier stays taken.** CH: `vm.add-disk`
   gives 200 with the `bdf`; `vm.remove-device` gives 204 and the disk leaves `config.disks`, but
   **stays in `device_tree`**, and adding the same id again gives `Invalid identifier as it is not
   unique`. libvirt: `detach-disk --live --config` says **"Disk detached successfully", rc=0, and
   `vdb` is still in `domblklist`** 3 s later (it left only the persistent definition).
6. **A q35 without spare PCIe ports takes ONE hot device.** After `vdb`, `attach-device` of a NIC
   gave `No more available PCI slots`. `pcie-root-port`s have to be reserved when the domain is
   defined.
7. **A hot NIC on CH needs a tap**, and the VMs' tap lives in the holder's netns: a `vm.add-net`
   without a prepared tap gave `Unable to configure tap interface: Operation not permitted`. It is
   holder work (the `vmtap` control line), not a call to the VMM's API.
8. **The record keeps no extra disks or NICs.** `VmConfig.extra_disks`/`extra_nics` exist and the
   stored `Vm` lacks them: a `vm start` today already loses the declared `extraDisks` (the
   documented limitation of `vm start`), and a hot-added disk would be lost at the first restart —
   the trap this repository has paid for four times ("state needed to rebuild the resource must be
   persisted").

Proxmox was **not measured** in this spike (from the PVE 9 documentation): the VM's `hotplug`
option defaults to `network,disk,usb`; hot CPU and memory need `cpu`/`memory` in that list and
`numa: 1`; the CPU ceiling is `sockets × cores` and the current value is `vcpus`; a change the node
cannot apply live goes to the config's `pending` section instead of failing.

What the guest needs is in the kernel's memory-hotplug admin guide
(`Documentation/admin-guide/mm/memory-hotplug.rst`): hot-added memory blocks are onlined only if
the auto-online policy says so — the `memhp_default_state=online|online_kernel|online_movable`
command-line parameter, `CONFIG_MEMHP_DEFAULT_ONLINE_TYPE`, or a write to
`/sys/devices/system/memory/auto_online_blocks` — otherwise a udev rule (or an agent) has to write
`online` to each new `memoryN/state`. Hot-added x86 CPUs come up offline unless a udev rule writes
`1` to `cpuN/online`. virtio-mem plugs through its own driver (Linux ≥ 5.8 on x86_64) and onlines by
the same policy.

## Decision

### D1 — A new verb, `vm update`, like `container update`; `vm resize` stays cold

`delonix vm update <name> [--vcpus N] [--memory M] [--disk-add …] [--disk-rm …] [--nic-add …]
[--nic-rm …]`. On a running VM it applies live; on a stopped one it writes the record (and the
backend's `resize_cold`, for CPU and memory) and the next boot uses it.

Why a verb and not `vm resize --live`:

- **One verb, one meaning.** `vm resize` is documented and tested as cold ("a guest that only sees
  the change after its next reboot has not been resized yet"); a flag that inverts its contract
  makes the same command mean two things, and an old script with a forgotten `--live` would ask for
  another operation.
- **Disk and NIC are not a "resize".** `update` is the verb the CLI already uses for "change this
  on a live resource, keeping its identity" (`container update`), and the one a user coming from
  Docker (`docker update`) or Proxmox (`qm set`) looks for.
- **The reconciler needs ONE path that works in both states**, and that is `update`.

`vm resize` stays as it is, with no alias. On a running VM, the DX-5505 message names
`vm update`. Removals run before additions, as in `container update` — and removals themselves are
gated by D10.

### D2 — Three quantities, kept apart: initial, maximum, assigned (owner, 2026-10-02)

The model and every output separate three quantities for CPU and for memory:

- **initial** — what the VM booted with (recorded at boot; informational);
- **maximum** — the hotplug ceiling, fixed at boot (D3);
- **assigned** — what the VM has now, read back from the live VM (D5), with the guest-active part
  shown next to it.

`vm describe`, `vm ls -o json`, the node API and the dashboard print the three, e.g.
`CPUs: initial 2 · maximum 4 · assigned 3 (guest online 3)`. **The maximum is not consumption**:
it reserves address space or slots, never host RAM or CPU, and nothing that measures or reports
use (`dashstats`, the Prometheus gauges, a showback) may count it — only the assigned quantity
counts. The record gains `vcpus_max` and `memory_max`; `vcpus`/`memory` keep meaning "what the next
boot starts with" and follow the confirmed assigned value (D6).

### D3 — The ceiling is declared at create, and without it there is no hotplug (owner, 2026-10-02)

`kind: VirtualMachine` gains `vcpusMax` and `memoryMax` (and `vm create --vcpus-max/--memory-max`).
**By default the maximum is the boot size** (zero headroom) — which is, byte for byte, what every
existing VM is today. Hotplug of CPU or memory needs BOTH a declared maximum above the boot size
AND the backend declaring the capability (D9); missing either, `vm update` above the boot size is
**refused, naming the ceiling and the command that changes it**, never clamped in silence.

Per backend, at create:

- **cloud-hypervisor:** `--cpus boot=N,max=M`; `--memory size=X,hotplug_method=virtio-mem,
  hotplug_size=(Max−X)`.
- **libvirt:** `<vcpu current='N'>M</vcpu>` with per-vCPU `<vcpus>` (`hotpluggable='yes'` on all
  but vCPU 0), `<maxMemory slots='S'>Max</maxMemory>` and a NUMA cell (libvirt needs one for
  DIMMs), and **four spare `pcie-root-port`s** when headroom is declared (finding 6).
- **Proxmox:** `hotplug: network,disk,usb,cpu,memory`, `numa: 1`, `sockets×cores = M`, `vcpus = N`.

The ceiling is **cold**: it changes with the VM stopped and holds from the next boot. It is not
given by default because it changes the domain's shape (NUMA, slots, PCIe ports) for someone who
did not ask for hotplug.

### D4 — An operation is complete only when the GUEST uses the resource; otherwise it is PARTIAL (owner, 2026-10-02)

A hot operation has three outcomes, and only the first is success:

- **complete** — the hypervisor assigned the resource AND the guest is using it (CPUs online,
  memory blocks online, the disk/NIC present in the guest);
- **partial** — the hypervisor assigned it and the guest has not activated it within the deadline
  (`--wait`, default 60 s), or the backend cannot see into the guest. Reported as the state
  `Partial`, with what is missing in the guest, under a new code **`DX-8503
  vm.hotplug_partial`** (class *timeout*, domain *vm*, **exit 124**: "the deadline passed with the
  work unfinished"; the next free number after DX-8501/8502, assigned at implementation through the
  dictionary gate). It is **never** reported as success, and `-o json` carries `"state":
  "partial"` with the assigned and the guest-active quantities;
- **refused** — an error with its class, nothing changed.

How the guest's side is read: on libvirt and Proxmox through the guest agent the base images
already ship (`guest-get-vcpus`, `guest-get-memory-blocks`, `guest-get-disks`,
`guest-network-get-interfaces`). With no agent answering, the result is `partial` with the reason
"guest activation not observable", never assumed. On Cloud Hypervisor virtio-mem's
`memory_actual_size` shows the guest driver plugging; CPU activation has no channel today, so a CPU
add on CH ends `partial` until one exists (Question 5).

### D5 — The truth is what is read back from the live VM, not the request's answer

Every hot operation is **request → read-back (hypervisor, then guest) → record**:

| | cloud-hypervisor | libvirt | Proxmox |
|---|---|---|---|
| vCPUs | VMM threads `vcpuN` (`/proc/<pid>/task`), not `config.boot_vcpus` | `vcpucount --live` | `GET …/status/current` `cpus` |
| memory | `vm.info` `memory_actual_size`, not `config.memory.*` | `dominfo` / `dommemstat actual` | `GET …/config?current=1` + empty `pending` |
| disk | `vm.info` `device_tree` | `domblklist` | `GET …/config?current=1` |
| NIC | `vm.info` `device_tree` | `domiflist` | `GET …/config?current=1` |
| guest | virtio-mem plugged size only | guest agent | guest agent |

### D6 — Persistence: what changes live reaches the record, or is lost at restart

- The stored `Vm` gains `vcpus_max`, `memory_max`, `extra_disks` and `extra_nics` (with
  `#[serde(default)]`: an old record reads as "maximum = boot, no extras", which is what it was).
  This also closes the `vm start` that today loses declared `extraDisks`.
- The order is the `resize`'s: **hypervisor first, read-back, record last**, one operation at a
  time (`JsonStore::update` under flock), as in `container update` — no transaction.
- What the record takes is the **hypervisor-assigned** value, also on a `partial` (the host holds
  it, and a restart must not drop it silently; the guest sees it at the next boot), together with
  a `HotplugPartial` condition naming what the guest has not activated. A refused operation leaves
  the record as it was.
- libvirt and CH: the record is the whole definition and the next boot rebuilds from it, so
  `--live` is enough (libvirt's `--config` is moot while `stop` undefines the domain).
- Proxmox: the node's config is persistent by nature; a change that lands in `pending` is **not
  applied** (read and reported as refused), never a success.
- **Mandatory gate per field:** change it live, `vm stop`, `vm start`, read the VM again — the
  value has to survive.

### D7 — Disk and NIC

- **Disk add:** CH `vm.add-disk` with a deterministic `id` (`dlx-<target>`); libvirt
  `attach-disk --live`; Proxmox `PUT /config` with `scsiN`/`virtioN`. The file path goes through the
  same name/confinement validation `extraDisks` already has. **Resizing** an existing disk
  (`vm.disk.resize`) is not part of this decision.
- **NIC add:** libvirt `attach-device` of an `<interface>` (needs a free PCIe port, D3); Proxmox
  `netN`. **On CH the hot NIC is a slice of its own**: the holder has to create the tap and attach
  it to the bridge (a new control line, with the same holder compatibility as `vmtap`), and with
  it the new tap's anti-spoof and namespace isolation (ADR-0055) — not a call to the VMM's API.
  Until then `--nic-add` on CH is refused by name (DX-1501).
- Disk and NIC removal follow D10.

### D8 — Delonix-managed base images online what is added (owner, 2026-10-02)

The images built by `vm-image` (`delonix-vm-base` for the four distros — Ubuntu, Debian, Rocky,
Fedora — and the golden k8s image) must online added CPUs and memory where the kernel supports
it. A dedicated phase changes the build recipes (`rootless_customization_steps` /
`shared_account_steps` and the k8s recipe), writing files, not runtime commands, because
`virt-customize` runs against an offline guest:

- memory: the auto-online policy, by kernel command line (`memhp_default_state=online_movable`,
  movable so a later remove can succeed) or, where the distro's bootloader makes that awkward, a
  udev rule `SUBSYSTEM=="memory", ACTION=="add", ATTR{state}=="offline", ATTR{state}="online_movable"`
  — per the kernel memory-hotplug admin guide;
- CPU: a udev rule `SUBSYSTEM=="cpu", ACTION=="add", ATTR{online}=="0", ATTR{online}="1"`, unless
  the distro already ships an equivalent (to be measured per distro, not assumed);
- the guest agent stays installed and enabled (D4 reads through it);
- the image's provenance file (`/etc/delonix-image-release`) records `hotplug-online: yes`.

**The check:** a test per distro asserting the recipe writes the policy, and a battery case that
boots each published image with headroom, adds one vCPU and 256 MiB, and reads `nproc` and
`/proc/meminfo` inside the guest — `complete`, not `partial`. Images not built by Delonix are
the user's: there the result is whatever D4 observes.

### D9 — In `kind: VirtualMachine`, `vcpus`/`memory` grow live, extras are hot

- `grow_only_fields(Vm)` = `vcpus`, `memory` (memory compared in normalised MiB, because
  `is_hot_change` compares numbers and `2G` against `2048M` is the same thing). Growth plans
  `Update`; a decrease still plans `Replace` (Question 1).
- `hot_fields(Vm)` gains `extraDisks` and `extraNics` for **additions**, converged item by item;
  an item removed from the manifest is not removed live while removal is out of scope (D10) — the
  plan says so instead of planning a removal it will not do.
- `vcpusMax`/`memoryMax` are cold.
- `is_hot_change` is the same function in `plan` and `apply` (as for `SystemContainer`).

### D10 — Removal is a separate capability, out of the first phases (owner, 2026-10-02)

Hot-add support does not prove safe hot-remove: findings 3, 4 and 5 show the hypervisor answering
"done" for a removal the guest has not performed, a CH removal blocking every later request with
429, and an identifier left taken. So:

- every capability is split into **`.add`** and **`.remove`** (D11);
- removal (CPU, memory, disk, NIC) is **out of the first phases** and refused live by name, with
  the cold path (`vm stop` + `vm update`/`vm resize`);
- it enters per backend only once proven there, with a guest that ejects, by a battery check that
  reads both the hypervisor and the guest after the removal and after a restart.

### D11 — Catalogue: `vm.hotplug` splits into eight cells

`vm.hotplug` gives way to `vm.hotplug.{cpu,memory,disk,nic}.{add,remove}` (catalogue minor+1).
Each backend declares each one with its reason; `supported` only with evidence (ADR-0050).
Declaration when the first phase ships:

| | cloud-hypervisor | libvirt | proxmox |
|---|---|---|---|
| `cpu.add` | partial — no guest channel for CPU activation (D4) | partial — until the D8 check | partial — no live case |
| `memory.add` | partial — virtio-mem; until the D8 check | partial — DIMM; until the D8 check | partial — no live case |
| `disk.add` | partial | partial | partial |
| `nic.add` | not-implemented — tap in the holder (D7) | partial | partial |
| `*.remove` | not-implemented — D10 | not-implemented — D10 | not-implemented — D10 |

A cell moves to `supported` only with the battery check of D12 run against a guest that onlines.

### D12 — The battery reads the live VM, and a real guest

The checks in `scripts/e2e.sh` (and the Proxmox `live.rs` case) read the hypervisor through the D5
table, **never** what `delonix` printed — and, for `supported` and for `complete`, also read INSIDE
the guest (`nproc`, `/proc/meminfo`, `lsblk`, `ip link`) over the serial console or the agent,
using a D8 image. Each field has its persistence check (D6), and a `partial` case asserts exit 124
and `DX-8503` on an image without the D8 policy.

## Phases

1. Record fields + ceiling at create + `vm update` for CPU/memory **add** on the three backends,
   with D2's three quantities in the output and D4's `partial` (DX-8503).
2. Base images that online (D8), with their per-distro check.
3. Disk add.
4. NIC add on libvirt/Proxmox.
5. NIC add on CH, with the holder.
6. Removal, per backend, only when proven (D10).

## Alternatives considered

- **`vm resize --live`.** Rejected by D1: inverts a published verb's contract and does not cover
  disk/NIC.
- **A generous default ceiling (e.g. `max = 2× boot`).** Rejected by the owner (D3).
- **Balloon as "hot memory" on libvirt.** Rejected: measured returning 0 with no effect without a
  driver; and it cannot go above `<memory>`, only below.
- **ACPI on CH.** Rejected: it does not remove, and answered 204 to a removal (finding 4).
- **Success as soon as the hypervisor accepts.** Rejected by the owner (D4): the hypervisor answers
  before the guest acts.
- **Writing the requested value to the record and reconciling later.** Rejected: the dishonest
  report this repository chases — a `vm describe` saying 4 vCPUs of a VM with 2.

## Consequences

- The `Vm` record grows four fields; old records stay valid.
- A new VM with declared headroom has a different domain shape on libvirt (NUMA, `<vcpus>`, PCIe
  ports) — the existing XML checks have to cover it.
- `vm start` stops losing the declared `extraDisks`/`extraNics` (side effect of D6).
- The reconciler gets the first Kind whose grow-only fields are not raw numbers (normalised memory).
- A new code, DX-8503, and a new `Partial` state in the VM output.
- The vm-image recipes change for the four distros (D8), and published images are rebuilt.

## Open questions (not answered by the owner; recommendation given)

1. **A decrease in the manifest** plans `Replace` today (refused without `--replace`). *Recommend:*
   a new plan action, "restart" (stop + apply + start, keeping the overlay), so a decrease and a
   ceiling change converge without destroying the disk.
2. **Changing the ceiling** of an existing VM. *Recommend:* only with the VM stopped, via
   `vm resize --vcpus-max/--memory-max` and, if Question 1 is accepted, through the "restart"
   action.
3. **Hot NIC on CH** (D7). *Recommend:* after the NaaS programme's current slice, because it
   touches the holder control line and the anti-spoof the network work is also changing.
4. **Proxmox lab window** (`pve`/`pve2`). *Recommend:* one window before phase 1 ships, to measure
   `hotplug` + `numa` + `pending` + `guest-get-vcpus` in a `live.rs` case before declaring.
5. **CPU activation on CH** has no guest channel. *Recommend:* stay `partial` on CH and evaluate a
   vsock agent in a later ADR, rather than declare a CH CPU add complete on the hypervisor's word.

## Annex A — Spike (2026-10-02, isolated)

Temporary directory in `/tmp/dlxhp.*`, removed at the end; no existing VM, no `DELONIX_ROOT`, only
`qemu:///session`. Domain `dlx-hp-spike-<pid>` destroyed and undefined; `virsh list --all | grep
dlx-hp` → 0; no spike `cloud-hypervisor` alive at the end.

### A.1 Cloud Hypervisor v53.0, ACPI

```
cloud-hypervisor --api-socket path=$D/api.sock --firmware /usr/local/share/delonix/CLOUDHV.fd \
  --disk path=$D/boot.qcow2,image_type=qcow2 --cpus boot=1,max=4 \
  --memory size=256M,hotplug_method=acpi,hotplug_size=512M --serial file=$D/serial.log --console off
curl --unix-socket $D/api.sock -X PUT -d '<json>' http://localhost/api/v1/<verb>
```

```
  boot_vcpus 1 mem.size 268435456 actual 268435456 disks ['_disk0']
== vm.resize {"desired_vcpus":5}         → HTTP500 "Requested vCPUs exceed maximum"
== vm.resize {"desired_vcpus":3}         → HTTP204   boot_vcpus 3   (threads vcpu0..vcpu2)
== vm.resize {"desired_vcpus":2}         → HTTP204   boot_vcpus 2   (threads: vcpu0, vcpu1, vcpu2 — vcpu2 did NOT go)
== vm.resize {"desired_vcpus":4}         → HTTP429 "Too Many Requests"   (removal pending)
== vm.resize {"desired_ram":1073741824}  → HTTP500 "Not enough space in the hotplug RAM region"
== vm.resize {"desired_ram":536870912}   → HTTP204   mem.size 536870912 actual 536870912
== vm.resize {"desired_ram":268435456}   → HTTP204   mem.size 268435456 actual 268435456   (ACPI cannot remove)
== vm.add-disk {"path":".../extra.raw","id":"hp0"} → HTTP200 {"id":"hp0","bdf":"0000:00:03.0"}
                                           disks ['_disk0','hp0'], device_tree has hp0
== vm.remove-device {"id":"hp0"}         → HTTP204   disks ['_disk0'], device_tree STILL has hp0 (also 5 s later)
== vm.add-disk ... id hp0                → HTTP500 "Invalid identifier as it is not unique: hp0"
== vm.add-net {"id":"net9"}              → HTTP500 "Unable to configure tap interface … Operation not permitted"
```

### A.2 Cloud Hypervisor v53.0, virtio-mem

`--memory size=256M,hotplug_method=virtio-mem,hotplug_size=512M`

```
  mem.size 268435456 hotplugged_size None actual 268435456
== vm.resize {"desired_ram":536870912}   → HTTP204   hotplugged_size 268435456 actual 268435456 (also 3 s later)
== vm.resize {"desired_ram":268435456}   → HTTP204   hotplugged_size None      actual 268435456
== vm.resize {"desired_ram":1073741824}  → HTTP500 "new size 0x30000000 is bigger than region_size 0x20000000"
```

Without a guest, `memory_actual_size` does not move: the guest plugs the blocks.

### A.3 libvirt 10.0.0, `qemu:///session`, q35

Domain with `<maxMemory slots='4' unit='MiB'>2048</maxMemory>`, `<memory>256</memory>`,
`<vcpu placement='static' current='1'>4</vcpu>`, one 256 MiB NUMA cell, one virtio disk,
`<memballoon model='virtio'/>`.

```
== setvcpus 3 --live        rc=0   current live 3, current config 1
== setvcpus 1 --live        rc=1   "acpi: device unplug request for not supported device type: host-x86_64-cpu"
== setvcpus 5 --live        rc=1   "requested vcpus is greater than max allowable vcpus for the live domain: 5 > 4"
== setmem 128M --live       rc=0   dominfo Used memory 262144 KiB (unchanged); dommemstat actual = maximum
== setmem 512M --live       rc=1   "cannot set memory higher than max memory"
== attach-device <memory model='dimm'> 256 MiB --live   rc=0   Max/Used memory 524288 KiB, 1 dimm in the live XML
== attach-disk vdb --live --config   rc=0   domblklist: vda vdb
== detach-disk vdb --live --config   rc=0   "Disk detached successfully"; domblklist: vda vdb (3 s later);
                                            inactive XML: 1 disk, live XML: 2
== attach-device <interface type='user'> --live --config   rc=1   "No more available PCI slots"
```

### A.4 Not measured

- Nothing inside a guest (CPU/memory onlining, eject ack): the spike's qcow2 is empty.
- Proxmox: no call; the facts in "Context" are from reading.
- virtio-mem on libvirt, `hotpluggable='yes'` vCPUs on libvirt, hot NIC on CH with a tap.
- Whether each distro already ships a CPU/memory onlining rule (D8 measures it).
