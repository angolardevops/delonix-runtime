# Building microVMs

This page takes a contributor from a bare Linux host to building, booting and testing VMs with
Delonix, and shows where the code lives when something needs changing. It assumes you have
already read [Preparing your environment](environment.md) and can build the tree
([Clone, build and test](build-and-test.md)).

> **What was verified for this page.** Every command and flag below was checked against
> `delonix <group> --help` of a binary built from this tree, and the read-only / configuration
> commands (`vm ls`, `vm reach`, `vm default-backend`, `image vm ls`, `manifest validate`,
> `stack apply --dry-run`, the backend-selection errors) were **run** with `DELONIX_ROOT` and
> `DELONIX_NET_RUNTIME_DIR` pointed at a scratch directory. **Booting a VM, building an image and
> pulling from the registry were not executed in this review** — behaviour for those comes from
> the code, the ADRs and the scripted tests referenced below.

Always use the binary you built (`./target/debug/delonix`), not one on your `PATH`, and always
isolate **both** state roots while experimenting — see [Isolation](#isolation-first).

---

## 1. Host prerequisites

### KVM

Both local backends need hardware virtualization:

```bash
ls -l /dev/kvm            # must exist; missing = VT-x/AMD-V off in firmware, or no nested virt
id -nG | tr ' ' '\n' | grep -x kvm   # your user must be in the kvm group
```

`scripts/install.sh` (default, i.e. without `--no-vm`) adds you to the `kvm` and `libvirt` groups
and warns when `/dev/kvm` is missing. Group changes need a new login session.

### Cloud Hypervisor and its firmware

The backend is available when `cloud-hypervisor` is on `PATH`
(`CloudHypervisorBackend::available` in `crates/adapters/delonix-vm/src/lib.rs`). Where the
distribution does not package it, the installer downloads the **static** upstream binary to
`/usr/local/bin/cloud-hypervisor`, pinned to a version **and** a SHA-256 in `scripts/install.sh`.

To boot a cloud image without `--kernel`, CH needs UEFI firmware. `default_ch_firmware` uses
`$DELONIX_HYPERVISOR_FW` if set, otherwise the first file that exists in `DEFAULT_CH_FIRMWARES`:

```text
/usr/local/share/delonix/CLOUDHV.fd       ← EDK2 build from cloud-hypervisor/edk2 (preferred)
/usr/share/delonix/CLOUDHV.fd
/usr/local/share/delonix/hypervisor-fw    ← rust-hypervisor-firmware (fallback)
/usr/share/delonix/hypervisor-fw
```

**The order matters.** Measured and recorded in the constant's doc comment: under
`rust-hypervisor-fw` none of the images this project builds boot in CH; with EDK2 `CLOUDHV.fd` they
do. `hypervisor-fw` stays as a fallback for hosts that only have it. The installer fetches both
(each pinned by tag and SHA-256). The unit test `o_edk2_vem_antes_do_hypervisor_fw_na_procura_de_firmware`
guards the order.

### libvirt / QEMU

The libvirt backend is available when both `virsh` and `qemu-system-x86_64` are on `PATH`
(`LibvirtBackend::available`). The installer installs QEMU and a libvirt daemon package and
enables `libvirtd` (socket-activated where supported).

Which libvirt connection is used matters more than it looks (`libvirt_uri_for`):

| Situation | Connection | Consequence |
|---|---|---|
| `--net-mode nat` or `bridge` | `qemu:///system` | reachable IP; needs the `libvirt` group (or root) |
| no `--net-mode`, system connection usable | `qemu:///system`, **`nat` chosen automatically** | IP by DHCP from the libvirt network (`virbr0`) |
| no `--net-mode`, system connection **not** usable, rootless | `qemu:///session`, user-mode | **no visible or reachable IP**; `vm create` warns |

On `qemu:///system` QEMU runs as the libvirt service user, which cannot read a disk under a 0700
home. For a rootless caller the domain XML gets a static DAC `seclabel` pinning QEMU to your
uid/gid with `relabel='no'`, so your own overlay boots without being chowned away.

### Tools the VM code shells out to

| Tool | Package (Debian / Fedora) | Used by |
|---|---|---|
| `qemu-img` | `qemu-utils` / `qemu-img` | per-VM overlays, `vm convert`, snapshots on CH, image builds |
| `cloud-localds` | `cloud-image-utils` / `cloud-utils` | NoCloud seed ISO — generated on **every** `vm create` of a cloud-init image unless `--seed` is given (`crates/adapters/delonix-vm/src/cloudinit.rs`) |
| `virsh` | `libvirt-clients` / `libvirt-client` | libvirt backend |
| `virt-customize`, `virt-sparsify`, `virt-copy-out` | `libguestfs-tools` / `guestfs-tools` | `image vm build` only |

`vmimage::tool_package` maps a missing binary to its package, so a missing tool is reported by
name rather than as a bare `No such file or directory`.

### Only for building images: `--with-image-build`

`image vm build` runs `virt-customize`, which builds a small appliance with supermin. Three host
problems break it in ways that do not look like host problems; `scripts/install.sh --with-image-build`
handles them, and `tool_failure_hint` (`cmd/vmimage.rs`) names them when a build fails:

1. **No DHCP client on the host.** supermin *copies* host packages into the appliance; without
   `isc-dhcp-client` the appliance has no network and the build dies on
   `Temporary failure resolving …`. The installer installs it.
2. **`/boot/vmlinuz-*` is 0600** (Debian/Ubuntu). supermin copies the host kernel and fails with
   `Permission denied`. The installer runs `chmod 0644 /boot/vmlinuz-*` — this **lowers a host
   security boundary** (any local user can read the kernel image), so it is opt-in and prints how to
   revert (`sudo chmod 0600 /boot/vmlinuz-*`). The failure hint also shows how to make it survive
   kernel updates.
3. **passt**, only with `image vm build --network`. libguestfs gives the appliance a network through
   passt. Its AppArmor profile (Debian/Ubuntu) forbids the runtime directory libguestfs uses, and the
   passt packaged in Ubuntu 24.04 starts but never hands out a lease — `dhclient` waits ~300 s and the
   build continues **without** network, failing later on a package mirror. Remedies:

   ```bash
   mkdir -p /tmp/delonix-run && chmod 700 /tmp/delonix-run
   XDG_RUNTIME_DIR=/tmp/delonix-run ./target/debug/delonix image vm build --network …
   ```

   and, if it still fails, a current passt **first on `PATH`** (the installer builds one into
   `/usr/local/bin`). Do not "disable" passt with a failing stub: libguestfs then uses the stub and
   dies on it.

The golden recipe's `--offline` mode avoids trap 3 entirely: it fetches and verifies packages on the
host and runs the guest with `--no-network`.

### Isolation first

This machine may run other workloads. Before any `vm` command beyond `--help`:

```bash
export DELONIX_ROOT=$HOME/dlx-dev/root
export DELONIX_NET_RUNTIME_DIR=/tmp/dlx-dev-run     # keep it SHORT (AF_UNIX sun_path is 108 bytes)
export TMPDIR=$HOME/dlx-dev/tmp                     # VMfile builds put whole disks here
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR" "$TMPDIR"
```

Both roots, always: isolating only `DELONIX_ROOT` leaves the network sockets shared with the real
state and has restarted real workloads before (see [Clone, build and test](build-and-test.md)). A Cloud Hypervisor
VM also refuses up front when `<root>/vms/<name>.sock` would not fit in `sun_path`
(`ch_socket_paths_fit`), so avoid very deep `DELONIX_ROOT` paths.

---

## 2. Backends and how one is chosen

### The port and the registry

`VmBackend` (`crates/adapters/delonix-vm/src/lib.rs`) is the port every hypervisor implements:
`id`, `available`, `boot`, `is_running`, `ip`, `stop`, plus defaulted methods (`destroy`, `pause`,
`unpause`, `resume`, `snapshot`/`restore`/`snapshots`/`delete_snapshot`, `preserve_snapshots`,
`ip_is_predicted`, `manages_own_storage`, `auto_selectable`, `disk_health`). A default that cannot
be honoured **fails closed** with a message, never a silent no-op.

Backends live in a registry (`BACKENDS`), not a `match`:

| Backend | Crate | Aliases | Auto-selectable |
|---|---|---|---|
| `cloud-hypervisor` | `delonix-vm` (built in) | `ch`, `cloudhypervisor` | yes |
| `libvirt` | `delonix-vm` (built in) | `kvm`, `qemu` | yes |
| `proxmox` | `delonix-proxmox` (registered by the CLI) | — | **no** — selected by name only |

`register_backend` refuses an id/alias that belongs to another backend, and refuses
`auto_selectable: true` for anything that is not built in: auto-detection asks every candidate
`available()`, and a remote backend can only answer that over the network. Registering does no I/O;
the factory runs the first time the backend is selected. `backend_for` on an existing VM resolves the
recorded backend and an unknown name is an **error** (it used to fall back to CH).

### Selection precedence for a new VM

From `delonix_vm::create_with` and `resolve_vm_defaults` (`cmd/vm.rs`), first match wins:

1. `--backend` (or `backend:` in the manifest).
2. The image's `HYPERVISOR` (recorded by a VMfile build), when `--disk` names a local image.
3. `DELONIX_VM_BACKEND` (session-wide).
4. `delonix vm default-backend --set <backend>` (machine-wide, stored in
   `<DELONIX_ROOT>/vm-default-backend`).
5. Capability heuristic: `volumes` present ⇒ `libvirt` (only libvirt does virtio-9p); a cloud image
   with no `--kernel` ⇒ `libvirt` **if libvirt is available**; otherwise auto-detection — the first
   auto-selectable registered backend that is installed (CH, then libvirt).

So on a host with both hypervisors, a plain `vm create` of a cloud image lands on **libvirt**; pass
`--backend cloud-hypervisor` (or set a default) to get a microVM on the SDN.

```text
$ delonix vm default-backend
none (auto-detection: cloud-hypervisor if installed, else libvirt)
$ delonix vm default-backend --set ch
default backend set to cloud-hypervisor
$ delonix vm default-backend --set bogus
error invalid argument: unknown VM backend: 'bogus' (use 'cloud-hypervisor', 'libvirt')
$ delonix vm default-backend --clear
default backend cleared (falls back to auto-detection)
```

### Proxmox VE (remote)

`bins/delonix-runtime-bin/src/cmd/vmbackends.rs::register_configured` registers the Proxmox backend
at startup when configured through the environment (a misconfiguration is a warning, never fatal
for unrelated commands):

| Variable | Meaning |
|---|---|
| `DELONIX_PROXMOX_URL` | API base URL, e.g. `https://pve.example:8006`. Unset = backend not registered. |
| `DELONIX_PROXMOX_NODE` | Required. The one node this backend addresses (as `GET /nodes` names it). |
| `DELONIX_PROXMOX_SECRET` | Preferred credential: name of a `kind: Secret` with `tokenId`+`tokenSecret` (or `username`+`password`). |
| `DELONIX_PROXMOX_TOKEN_ID` + `DELONIX_PROXMOX_TOKEN` | API token from the environment. |
| `DELONIX_PROXMOX_USER` + `DELONIX_PROXMOX_PASSWORD` | Password login (ticket, re-authenticated on 401). |
| `DELONIX_PROXMOX_INSECURE_TLS` | `1`/`true`/`yes` to skip certificate verification. Opt-in, never a fallback. |
| `DELONIX_PROXMOX_BRIDGE` | Default bridge on the node (a per-VM `bridge` wins). |
| `DELONIX_PROXMOX_VLAN` | Default VLAN tag, 1–4094; out of range is an error, not a silently untagged NIC. |

Without configuration, `--backend proxmox` answers that the backend "is not available in this build"
and says what to set (*run*). The backend owns its storage (`manages_own_storage`), so no local
overlay or NoCloud seed is made; `--hostname`/`--ssh-key` go to the node's cloud-init, and
`--user-data` is refused. Design and limits: [ADR-0008](../adr/0008-proxmox-vm-backend.md). An
OpenStack backend is **proposed only** ([ADR-0039](../adr/0039-openstack-vm-backend.md)); there is no
code for it.

---

## 3. Images

VM images live in `VmImageStore` (`cmd/vmimage.rs`) under `<DELONIX_ROOT>/vm-images/`: one qcow2 plus
a JSON metadata record (`VmImage`) per image. `delonix image vm ls` lists them with `TYPE`
(cloud-init / appliance) and `DEFAULTS` (recorded vCPU/memory).

### Official images

`OFFICIAL_REPOS` in `cmd/vmimage.rs`:

| Key | Repository | Content |
|---|---|---|
| `k8s` | `ghcr.io/angolardevops/delonix-vm-k8s` | Kubernetes node (kubeadm/kubelet/kubectl + `delonix-cri`) |
| `base` | `ghcr.io/angolardevops/delonix-vm-base` | base OS with the `delonix` engine, no Kubernetes (e.g. `ubuntu-24.04`) |
| `appliances` | `ghcr.io/angolardevops/delonix-vm-appliances` | vendor appliances, no cloud-init |

```bash
delonix vm ls-remote                 # tags of the Kubernetes golden repo
delonix vm ls-remote --no-k8s        # tags of the base repo
delonix vm pull                      # the official Kubernetes golden
delonix vm pull --no-k8s             # the official base image
delonix vm pull <oci-ref> --name <local-name>
```

Images are single-blob OCI artifacts; the pull verifies the manifest and blob digests and restores
the metadata from manifest annotations. The same verbs exist as `delonix image vm pull/ls-remote/push`.
**Note:** `vm create` with no `--disk` and no local image downloads the official golden image, so it
needs network.

### Building the golden recipe

`delonix image vm build -t <tag>` with no `VMfile` in the context runs the built-in recipe:

```bash
# Kubernetes node, packages fetched and verified on the HOST, guest offline
delonix image vm build --offline --k8s-version 1.34 -t delonix-vm-k8s:1.34
# no Kubernetes: just the engine, rootless-ready
delonix image vm build --no-k8s --distro debian --debian-release bookworm -t delonix-vm-base:debian-bookworm
```

Relevant flags (check `image vm build --help` for defaults): `--distro ubuntu|debian|rocky|fedora`,
`--ubuntu-release`, `--debian-release`, `--rocky-release`, `--fedora-release` (release **and** build,
e.g. `42-1.1`), `--k8s-version`, `--offline`, `--no-k8s`, `--extra-package`, `--extra-run`,
`--cri-bin`, `--delonix-bin`, `--root-password` (without it no account has a password),
`--node-exporter[=<addr>]`, `--no-compress`. Your own recipe is a `VMfile` — see
[Delonixfile and VMfile](delonixfile-and-vmfile.md). Image builds are amd64 only
([ADR-0018](../adr/0018-vm-images-stay-amd64.md)).

### Converting and importing

```bash
delonix vm convert <image-or-path> --to raw|qcow2|vmdk|vdi|vhdx|vhd [-o out] [--compress]
delonix image vm import disk.qcow2 -t opnsense:26.1 --appliance --default-vcpus 2 --default-memory 2G
```

`vm convert` flattens (no backing chain); `--compress` is accepted only for `qcow2` and `vmdk`.
`import --appliance` records `cloud_init: false`: `vm create` then attaches **no** seed and refuses
`--hostname`/`--ssh-key`/`--user-data` by name, because the guest would never read them. Appliance
build scripts live in `scripts/appliances/`.

---

## 4. Create and run

### `vm create`

```bash
delonix vm create dev --disk delonix-vm-base:ubuntu-24.04 \
  --backend cloud-hypervisor --vcpus 2 --memory 2G \
  --ssh-key @$HOME/.ssh/id_ed25519.pub --hostname dev --wait
```

What happens (`cmd/vm.rs` → `delonix_vm::create_with`):

1. **Node policy** is enforced before any image is resolved (`policy::enforce`).
2. **Disk resolution** (`resolve_image_ref`): `--url-img` wins (downloaded, cached, verified against
   `<url>.sha256` when offered); else `--disk` is looked up as a local image name, then as a path to a
   stored image's qcow2; else it is used as a plain path; with no `--disk`, the single local golden
   image, or the official one is pulled.
3. **Defaults** from the image metadata fill `--vcpus`/`--memory`/`--backend` only where unset.
4. **Seed**: a NoCloud ISO (network config by MAC, hostname, keys) is generated unless `--seed` is
   given, the image is an appliance, or the backend is remote. `--user-data` replaces the generated
   user-data.
5. **Backend** chosen (section 2); an admission check refuses when the host lacks RAM;
   `--namespace` other than `default` is refused on libvirt (the VM lives on `virbr0`, outside the
   Delonix SDN).
6. **Overlay**: `<root>/vms/<name>.qcow2`, a thin qcow2 over the base (`prepare_local_overlay`);
   `--disk-size <GiB>` grows it and cannot be smaller than the base.
7. **Boot**: the backend's `boot`. `create` is idempotent: an existing, running VM is returned as is.

The published images set no password on any account (see `--root-password` above), so pass
`--ssh-key` if you want to log in over SSH.

**`--wait` and the predicted IP.** On libvirt the IP comes from a real DHCP lease, so having one is
evidence the guest booted. On Cloud Hypervisor the IP is **computed from the MAC** before the guest
runs (`ip_is_predicted`), so `--wait` also probes the address by ARP from inside the network holder
(`delonix_sdn::infra::sdn_reachable`) until `--boot-timeout` (default 120 s). Three outcomes: up;
"which could not be verified from here" (the probe could not be asked); "is running but never
answered … computed from the MAC, not observed". Use `vm console` to see why.

### Day-2 verbs

| Command | Notes |
|---|---|
| `delonix vm ls [--namespace <ns>] [--ports] [-o json]` | lists VMs |
| `delonix describe vm <name>` / `delonix delete vm <name>` | there is no `vm describe`/`vm rm`; the generic verbs replace them |
| `delonix vm console <name> [-e ^X]` | serial console; detach with `Ctrl-]` by default (`$DELONIX_CONSOLE_ESCAPE`). libvirt: `virsh console` as a child process; CH: the console socket `<root>/vms/<name>.console` |
| `delonix vm ssh <name\|ip> [-l user] [-i key] [-- cmd]` | IP from the record; default user `delonix` on cloud-init images, `root` on appliances |
| `delonix vm vnc <name>` | only for libvirt VMs created with `--vnc` (CH has no display) |
| `delonix vm stop <name>` | keeps disk, record and snapshots. libvirt: the domain is undefined (snapshot metadata is preserved first) |
| `delonix vm start <name>` / `restart <name>` | rebuild the boot from the record and reuse the overlay; `start` on a running VM is a no-op, `restart` always reboots |
| `delonix vm pause` / `unpause <name>` | suspend vCPUs, memory kept in RAM; CH and libvirt |
| `delonix vm migrate <name> --host <h> --network <n>` | stop-copy-start over SSH to another host; real downtime ([ADR-0031](../adr/0031-live-vm-migration-no-go.md) explains why live migration is out of scope) |
| `delonix vm prune` | reclaims state no VM record accounts for |

### Snapshots

`delonix vm snapshot create|ls|rm|restore <vm> [<snapshot>]`:

| Backend | Running VM | Stopped VM |
|---|---|---|
| libvirt | `create` is a system checkpoint (memory + disk) | disk-only; the four verbs define the domain just for the command |
| cloud-hypervisor | `ls` only (`qemu-img info -U`); `create`/`restore`/`rm` **refused** — the running VMM locks the disk and CH has no live disk-snapshot API | all four, via `qemu-img snapshot` |
| proxmox | `create` (with VM state), `ls`, `restore` | same; `rm` not implemented (fails closed) |

libvirt snapshots survive `vm stop`/`vm start`: `undefine --snapshots-metadata` removes only
libvirt's bookkeeping, so `preserve_snapshots` dumps each snapshot's XML to
`<root>/vms/<vm>/snapshots/` before stopping and `boot` redefines them (rewriting the domain uuid,
which changes on every define).

### Declarative

`kind: VirtualMachine` (`compute.delonix.io/v1alpha1`) mirrors `vm create`; a full annotated example is
`examples/vm.yaml`. `kind: Workload` with `type: microvm` lowers to a `VirtualMachine` with the backend
**forced** to `cloud-hypervisor`; asking for another backend is an error
([ADR-0006](../adr/0006-workload-type-microvm.md)). Both *run* in a scratch root:

```yaml
apiVersion: compute.delonix.io/v1alpha1
kind: VirtualMachine
metadata: { name: dev }
spec:
  disk: delonix-vm-base:ubuntu-24.04
  resources: { vcpus: 2, memory: 2G }
  cloudInit:
    hostname: dev
    sshKeys: ["@~/.ssh/id_ed25519.pub"]
---
apiVersion: compute.delonix.io/v1alpha1
kind: Workload
metadata: { name: fast }
spec:
  type: microvm
  microvm: { disk: delonix-vm-base:ubuntu-24.04, vcpus: 1, memory: 1G }
```

```text
$ delonix manifest validate -f vm.yaml
stack validate: OK — 2 document(s), all references resolved
$ delonix stack apply -f vm.yaml --dry-run | grep backend
  backend: null
  backend: cloud-hypervisor
```

Apply with `delonix vm apply -f vm.yaml` or `delonix stack apply -f vm.yaml` (not executed here).

---

## 5. Networking for VMs

| | Cloud Hypervisor | libvirt |
|---|---|---|
| Where the NIC lives | a tap on a Delonix network (`--network`, default `ingress`) **inside the network holder** — the same SDN as containers | `virbr0` (nat) or a host bridge, in the host's network namespace |
| IP | leased by the holder's DHCP, deterministic from the MAC | libvirt DHCP; `--ip` reserves one (nat only) |
| Namespace isolation | yes (`--namespace`) | refused |
| VM ↔ container by IP | direct | container → VM works through the host; VM → container needs a published port or `vm bridge` |

**`vm reach`** (read-only, no privilege) lists the libvirt gateways and, for each port published by a
running container, whether a VM can reach it. A port published on the default `127.0.0.1` is
invisible to VMs; the command prints the exact re-publish, e.g.
`DELONIX_PUBLISH_ADDR=<gateway> delonix net ingress publish <c> <port>` — reachable from VMs on that
network, not from the external LAN.

**`vm bridge <network> [--vm-subnet <cidr>] [--apply]`** is **experimental and needs root**: a veth
from the host into the holder plus routes, giving libvirt VMs direct IP reachability to one container
network. Without `--apply` it only prints the plan. `vm unbridge <network>` tears it down (also a
dry-run without `--apply`). This is the one deliberate exception to rootless in the VM code
(`cmd/vmbridge.rs`).

---

## 6. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `/dev/kvm does not exist` / VM fails to start | virtualization disabled, or no nested virt | enable VT-x/AMD-V; in a VM, enable nested virtualization |
| `no VM backend available` | neither `cloud-hypervisor` nor `virsh`+`qemu-system-x86_64` on `PATH` | install one; `scripts/install.sh` does |
| CH VM "running" but never answers; overlay stays tiny | firmware cannot boot the image (e.g. only `hypervisor-fw` installed) | install EDK2 `CLOUDHV.fd` to `/usr/local/share/delonix/`, or set `DELONIX_HYPERVISOR_FW`, or use `--backend libvirt` |
| `vm ls` shows no IP on a libvirt VM | fell back to `qemu:///session` user-mode | join the `libvirt` group and re-login, or `--net-mode nat` |
| `warning: cannot reach qemu:///system for NAT networking` | not in the `libvirt` group | `sudo usermod -aG libvirt $USER`, new session |
| `vm console` on libvirt: "Active console session exists" | a previous session died uncleanly | current code passes `--force` to `virsh console`; update your binary |
| CH VM refused before boot mentioning the socket path | `<root>/vms/<name>.sock` exceeds 108 bytes | shorter `DELONIX_ROOT` or VM name |
| `namespace '…' is not enforceable on the 'libvirt' backend` | libvirt VMs are outside the SDN | `--backend cloud-hypervisor`, or drop `--namespace` |
| `vm snapshot create` refused on a running CH VM | CH has no live disk snapshot | `vm stop` first, or use libvirt |
| `--hostname`/`--ssh-key` refused | the image is an appliance (`cloud_init: false`) | configure the appliance through its own console/UI |
| `cloud-localds not found` | missing `cloud-image-utils` | install it (needed for every cloud-init `vm create`) |
| `image vm build`: `cp: cannot open '/boot/vmlinuz-…'` | host kernel is 0600 | `install.sh --with-image-build`, or `sudo chmod 0644 /boot/vmlinuz-*` (lowers a boundary) |
| `image vm build`: `Temporary failure resolving …` | appliance has no network: missing host DHCP client, or passt | install `isc-dhcp-client`; for `--network` use `XDG_RUNTIME_DIR=/tmp/delonix-run` and a current passt first on `PATH`; or build `--offline` |
| a step pauses ~300 s, then package installs fail | passt never leased; `dhclient` timed out | same as above |
| `--offline belong to the built-in golden recipe …` | golden-recipe flag used with a VMfile | remove it; the VMfile describes the build |
| `` `--network` is for VMfile builds `` | `--network` without a VMfile | use `--offline` for the golden recipe |
| VMfile build fills `/tmp` | each stage is a full flattened disk under `$TMPDIR` | set `TMPDIR` to a large filesystem; remove leftover `delonix-vmfile-*` dirs after failures |
| `VM backend 'proxmox' is not available in this build` | the backend is not configured | set the `DELONIX_PROXMOX_*` variables (section 2) |

---

## 7. For contributors

### Where the code lives

| Area | Path |
|---|---|
| Port, registry, CH and libvirt backends, `create_with`, snapshots, firmware lookup | `crates/adapters/delonix-vm/src/lib.rs` |
| NoCloud seed generation | `crates/adapters/delonix-vm/src/cloudinit.rs` |
| Proxmox backend | `crates/providers/delonix-proxmox/` |
| `vm` CLI, `kind: VirtualMachine`, `vm reach` | `bins/delonix-runtime-bin/src/cmd/vm.rs` |
| Image store, golden recipe, pull/push/import/convert, failure hints | `bins/delonix-runtime-bin/src/cmd/vmimage.rs` |
| VMfile | `bins/delonix-runtime-bin/src/cmd/vmfile.rs` |
| Backend registration from the environment | `bins/delonix-runtime-bin/src/cmd/vmbackends.rs` |
| `vm bridge` | `bins/delonix-runtime-bin/src/cmd/vmbridge.rs` |
| `kind: Workload` lowering | `bins/delonix-runtime-bin/src/cmd/workload.rs` |
| Appliance builds | `scripts/appliances/` |

### Adding a backend

Read [ADR-0008](../adr/0008-proxmox-vm-backend.md) first; it is the template. In short:

- Implement `VmBackend` in **its own crate** if it talks to a remote API (the engine crate stays free
  of HTTP clients), placed in the directory of its layer and listed in `scripts/arch_fitness.py`
  (see [Architecture](architecture.md)).
- Register it with `register_backend` from the process that knows its configuration, with
  `auto_selectable: false` unless it is a local, zero-configuration backend built into `delonix-vm`.
- Override `manages_own_storage`, `destroy`, `resume` and `ip_is_predicted` deliberately: for a remote
  backend `stop` and `destroy` are **not** the same operation, and `boot` on a stopped VM must not
  create a second one.
- Leave unsupported verbs on their fail-closed defaults; refuse unsupported `VmConfig` fields by name
  before creating anything.
- Do not publish a backend that has never been seen booting a VM. New backends and hypervisor
  boundaries go through an ADR ([docs/adr/](../adr/)).

### Testing

- **Pure unit tests** — argv and XML builders are pure functions so they test without a hypervisor:
  e.g. `libvirt_snapshot_argv_uses_flags_not_positional`, `snapshot_xml_with_uuid`,
  `libvirt_domain_xml`, the firmware-order test above, the registry and `auto_detect` tests, and the
  parser/scaffold tests in `cmd/vmfile.rs`. Run `cargo test -p delonix-vm` and
  `cargo test -p delonix-runtime-bin vmfile` (see [Clone, build and test](build-and-test.md) for `protoc` and the
  target directory).
- **`scripts/e2e.sh`** — the `vm` sections run without a hypervisor (listing, refusals) and, when
  available, exercise snapshots across stop/start on libvirt (needs `virsh`, `qemu-img` and a usable
  `qemu:///system`) and on Cloud Hypervisor. It isolates both state roots by default; the CH section
  refuses to run if only `DELONIX_ROOT` is isolated. Sections without their hypervisor are reported
  as skipped, not passed.
- **Proxmox live test** — `crates/providers/delonix-proxmox/tests/live.rs` creates and destroys one
  VM against a real node and is skipped unless `DELONIX_PROXMOX_TEST_URL` (plus `_NODE`, `_USER`,
  `_PASS`) is set: `cargo test -p delonix-proxmox --test live -- --nocapture`. Use a disposable node.
- **Provider lifecycle** — when changing `delonix-vm` or a provider, prove the whole lifecycle
  (create, stop, start, snapshot, destroy) **through the `delonix` CLI only**, observing the
  hypervisor read-only (`virsh -r`, API `GET`s), never repairing it by hand between steps.
- **Look, do not guess.** When a guest does not come up, the serial console (`vm console`) or a
  libvirt screenshot answers in seconds what hypotheses take hours to find — and validate with the
  command a user would type, not the flags that are convenient for debugging (`--vnc` once masked a
  boot failure that only happened without a video device).
