# ADR-0067: The engine manages storage pools — LVM-thin, ZFS, btrfs and directory, behind a port

- **Status:** Accepted (2026-10-06, by the owner), **P0 implemented** 2026-10-03 (#689: the port, the
  `delonix-storage` context, the administrator's allowlist, `kind: StoragePool` and the `dir`
  driver — see the addendum at the end). P1–P6 need root and spare disks in a lab VM and are not
  built. The owner's decisions of 2026-10-02 are recorded in D3 and D5; three questions remain
  open (end of the document).
- **Date:** 2026-10-02
- **Deciders:** Walter Angolar
- **Relates to:** decision D2 of the maturity plan (`docs/discovery/65_PLANO_MATURIDADE.md`,
  PR #658: «the engine manages pools»), ADR-0009 (provisioning on a NAS: ownership by stamp, the
  remote first and the record last), ADR-0008/ADR-0044 (backend registry seeded by the composition
  root), ADR-0050 (capability catalogue and evidence), ADR-0059 D7 (role ports in their own
  context), ADR-0031 (what stays out: storage shared between nodes).
- **Review copy:** `docs/adr/0067-storage-pools.pt-AO.md` (Portuguese, internal). This English
  file is canonical; the two carry the same ID, status, decisions and references.

## Context

Today the engine **consumes** storage and does not manage it. A `kind: Volume` is a directory in
`<root>/volumes/<name>/_data`, or a mounted network share; the quota is **hard** only in the root
model (a sparse ext4 image mounted through a loop device, `delonix-volume/src/lib.rs`) and
**monitored** in rootless (measured usage, an alert near the limit). A local VM's disk is always a
qcow2 overlay over the golden image, in a file under `<root>/vms/`. In the capability matrix
(`docs/providers/capability-matrix.md`) that reads:

| cell | libvirt | cloud-hypervisor | proxmox | linux |
|---|---|---|---|---|
| `storage.pools` | not-implemented | not-implemented | partial (`disk: <storage>:<gib>`) | unsupported-by-provider |
| `storage.lvm-thin` | — | — | — | not-implemented |
| `storage.zfs-btrfs` | — | — | — | not-implemented |
| `storage.ceph` | — | — | — | requires-external-component |

The owner decided (D2) that the engine now **manages** pools: allocating volumes and disks inside
them, with native quotas, thin provisioning and snapshots. It is a boundary change — the engine
starts writing to block devices and kernel metadata it never touches today — so it goes through an
ADR, with the measurement below.

### What the tested environment allows without root (measured, 2026-10-02)

Spike run as `walter` (uid 1000, no `sudo`, not in the `disk` group) on **one host**: kernel
7.0.0-34-generic, Ubuntu 24.04, root filesystem ext4. Files only in a scratch directory of the
worktree (256 MiB + 128 MiB sparse), deleted at the end. **These results describe this tested
environment, not every Linux host**: device-node permissions, group membership, udev rules, the
packages installed and the kernel configuration all change them. They are the reason the design
probes each host instead of assuming.

| Question | Command | Result on this host |
|---|---|---|
| Tools present | `command -v …` | `lvm2` 2.03.16, `thin-provisioning-tools` 0.9.0, `btrfs-progs` 6.6.3, `dmsetup`, `losetup`, `qemu-img`, `qemu-nbd`. **No `zfs`/`zpool`** (`zfsutils-linux` not installed) |
| Modules | `lsmod`, `modinfo zfs` | `btrfs` loaded; `zfs` present in the kernel (2.4.1-1ubuntu5.1) but **not loaded**; `/dev/zfs` exists as `crw------- root root` |
| LVM as a user | `lvs` | `WARNING: Running as a non-root user` and `/run/lock/lvm/P_global:aux: open failed: Permission denied` — **exits 0** with an empty list |
| device-mapper | `dmsetup ls` | `/dev/mapper/control: open failed: Permission denied` |
| Loop as a user | `losetup --find --show img` | `/dev/loop-control` is `root:disk 0660` → `Permission denied` |
| `mkfs.btrfs` on a file | `mkfs.btrfs -q -L dlxspike img` (256 MiB) | **works**, 0.26 s; 4.6 MiB used of 256 MiB apparent; `btrfs inspect-internal dump-super`, `btrfs filesystem show` and `btrfs check --readonly` read the image without mounting it |
| Userns + mount ns | `unshare --user --map-root-user --mount` | `tmpfs` and `ramfs` mount; `mount -o loop`, `losetup` and `mount -t btrfs <file>` fail (loop is denied even as the userns root); `lvs`, `dmsetup` and `/dev/zfs` give `Permission denied` |
| VM disk without a pool | `qemu-img create -f raw` / `-f qcow2` | work; `qemu-nbd --connect=/dev/nbd0` fails (no `/dev/nbd0` on this host) |
| libvirt pools | `virsh -c qemu:///system pool-capabilities` | `dir`, `fs`, `netfs`, `logical`, `disk`, `iscsi`, `scsi`, `mpath` supported; **`zfs` and `rbd` not** |

Three traps the measurement showed, which the code must respect:

1. **Unprivileged `lvs` exits 0 with an empty list.** A driver that reads the exit status reads
   «no VG» on a host that has VGs. This is the class already catalogued as «a `read` that fails is
   not an empty answer». The rule (owner decision, D3.5): without sufficient privilege the answer
   is **«could not determine»**, never «no pools».
2. **On this host, without loop or device-mapper access, a btrfs/ext4 image in a file is of no use
   rootless**: it is created, but cannot be mounted — not even inside a userns. Another host may
   differ (a user in the `disk` group, a different kernel); the probe decides per host.
3. **`/dev/zfs` at `0600` here is the absence of `zfsutils-linux`**, not an OpenZFS rule: the
   package installs the udev rule that sets it to `0666`, which is what makes `zfs allow`
   delegation usable by a non-root user. On a host without the package, the probe says «ZFS
   unavailable», never «ZFS without delegation».

### What was read and not measured (no root, no real pool)

- **btrfs**: `btrfs subvolume create` needs only write access to the parent directory; an
  unprivileged `subvolume delete` needs the `user_subvol_rm_allowed` mount option (or an empty
  subvolume, kernel ≥ 4.18); a snapshot of a subvolume the user owns is allowed; **quotas
  (`quota enable`, `qgroup limit`) are administrative ioctls — root**. btrfs cannot be mounted
  inside a userns (it lacks `FS_USERNS_MOUNT`), so the pool has to be mounted by the administrator.
- **ZFS**: `zfs allow <user> create,destroy,snapshot,rollback,clone,quota,refquota,volsize,mount`
  delegates operations on a dataset; on Linux a delegated `mount` permission is not enough by
  itself (`mount(2)` needs `CAP_SYS_ADMIN`), except through OpenZFS ≥ 2.2's user-namespace
  delegation (`zfs zone` / the `zoned` property). A zvol appears as `/dev/zd*` (`root:disk 0660`): a
  rootless VMM opens it only with a udev rule or ACL granted by the administrator.
- **LVM-thin**: everything goes through device-mapper — **root, no delegation possible**. A full
  thin pool puts its LVs in I/O error; `thin_pool_autoextend_threshold` in `lvm.conf` is host
  configuration, not the engine's.

## Decision

### D1 — A `StoragePoolDriver` port in a new context, `delonix-storage`

The **storage** context (`storage.delonix.io`, ADR-0040 D2.2) gets its crate,
`crates/contexts/delonix-storage`, in the shape `delonix-networking` has had since ADR-0059 D7: the
port, the registry by name, the ownership marks and the pure request types. It enters the `LAYERS`
table of `scripts/arch_fitness.py` as `CONTEXT` in the same commit the directory is born.

```rust
pub trait StoragePoolDriver: Send + Sync {
    fn id(&self) -> &'static str;                        // "dir" | "btrfs" | "zfs" | "lvm-thin"
    fn probe(&self, pool: &PoolRef) -> PoolProbe;        // tools, module, privilege, health — never writes
    fn required(&self, op: PoolOp) -> Privilege;         // Unprivileged | Delegated(&'static str) | Helper
    fn adopt(&self, pool: &PoolRef) -> Result<PoolState>;
    fn allocate(&self, pool: &PoolState, req: &VolumeRequest, owner: &Owner) -> Result<Allocation>;
    fn resize(&self, a: &Allocation, bytes: u64) -> Result<()>;   // grow only (like ADR-0058's rootfs)
    fn snapshot(&self, a: &Allocation, name: &str) -> Result<()>;
    fn rollback(&self, a: &Allocation, name: &str) -> Result<()>;
    fn clone_from(&self, snap: &SnapshotRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation>;
    fn release(&self, a: &Allocation, owner: &Owner) -> Result<()>;
    fn usage(&self, pool: &PoolState) -> PoolUsage;      // data% and metadata% of a thin pool
}
```

- `PoolProbe` is **three-valued**: `Available`, `Unavailable { missing }`, and
  `Undetermined { reason, remedy }`. An empty listing obtained without the privilege to list is
  `Undetermined` (D3.5), never `Available` with zero pools.
- `VolumeRequest` has one shape: `Filesystem` (a mounted directory — a container volume) or `Block`
  (a raw device or file path — a VM disk). A driver that does not serve a shape refuses it by name,
  never approximates it.
- **Pool creation and destruction are not on the port.** They are administrator operations
  (D3.3, D5) performed by the helper's administrative command, never by the engine on behalf of a
  manifest.
- **The drivers are providers, one crate per backend** (`crates/providers/delonix-provider-btrfs`,
  `-zfs`, `-lvm`), like libvirt and Cloud Hypervisor since P4b.4: they depend only on the
  foundation and the context, and the composition root seeds them with `registration()`. The
  **`dir`** driver is today's behaviour expressed as a pool, and lives in `delonix-volume`
  (adapter).
- **No `if driver == …` outside the registry.** The CLI, the reconciler and compute ask for an
  `Allocation` by pool name.
- Registering does no I/O (ADR-0008); the probe runs only when the pool is used or listed.

### D2 — `kind: StoragePool`, and how `Volume` and VMs refer to it

```yaml
apiVersion: storage.delonix.io/v1alpha1
kind: StoragePool
metadata: { name: fast }        # must name a pool in the administrator's allowlist (D3.2)
spec:
  overcommit: { maxRatio: 1.0 } # thin provisioning ceiling; above it, allocation is refused
  alertPct: 80
```

- **A manifest never carries a device, a VG, a dataset or a mount path, and never a command.** The
  Kind *uses* a pool the administrator declared (D3.2): its name is looked up in the allowlist, and
  the driver and backing object come from there. A name not in the allowlist is refused before any
  probe. Fields such as `devices`, `vg`, `dataset`, `path` or `mode: create` are refused by name,
  never ignored.
- **It has no namespace** (`Namespaced::Never`): a pool is a node resource. The engine knows no
  tenants; whoever divides a pool among customers is whoever consumes the engine, with volumes and
  quotas.
- `kind: Volume` gets `spec.pool: <name>` and `spec.size`. With `pool`, the quota becomes the
  backend's (hard where it gives one). `pool` is exclusive with `nfs:`/`cifs:`/`webdav:`/
  `provision:` — refused together, never one silently winning. Without `pool`, today's behaviour.
- `kind: VirtualMachine` gets `spec.storage.pool` (the root disk) and `extraDisks[].pool`; in the
  CLI, `vm create --pool` and `volume create --pool <name> --size <s>`. **No new top-level group**:
  the pool is addressed through the generic verbs `get|describe|delete storagepools` and through
  `apply -f`, as the CLI restructuring (B1, B5) already decided for Kinds without a verb of their
  own. `delete storagepools` unregisters the use; it never destroys the pool (D5).
- `stack plan` compares what the pool **is** (read from the backend) and not what the manifest
  says. Hot fields: `overcommit`, `alertPct`, a growing `size`.

### D3 — Privilege, the helper, and the administrator's allowlist (owner decisions, 2026-10-02)

The owner decided, on 2026-10-02:

1. **Privileged operations for LVM-thin are accepted, but the engine as a whole never requires
   root. Rootless stays the default.** Every operation that can run unprivileged (D3.4) runs in the
   engine's own process; only the operations that need root cross into the helper.
2. **Devices and pools are limited to an administrator allowlist**, and a manifest can never name a
   device, a path or a command beyond it.
3. **Pools are created only through explicit administrator configuration.**
4. **Pool management runs through a restricted helper, or in an explicitly configured rootful node
   service.** The choice and its reasons are below.
5. **An unprivileged `lvs` exit 0 with empty output is never read as «no pools»**; the answer is
   «could not determine», with an actionable diagnostic.

#### The allowlist

`/etc/delonix/storage-pools.yaml`, owned by root, mode `0644`, read by the engine and by the
helper; **only root can write it**, and that write is the administrator's explicit configuration.

```yaml
pools:
  fast:
    driver: lvm-thin
    vg: vg0
    thinPool: dlx
    create: { devices: [/dev/disk/by-id/nvme-SAMSUNG_X_1234] }   # optional; only `delonix-storage-helper create-pool` reads it
    allowUsers: [walter]           # uids/groups allowed to allocate through the helper
    maxVolumeBytes: 500G
  tank:
    driver: zfs
    dataset: tank/dlx
  media:
    driver: btrfs
    path: /srv/dlx                 # mounted by the administrator
```

- The helper re-reads the file on every operation; the engine never passes it a path, a device or
  an option — only a pool name, a volume name, a size and an operation.
- Devices are named by stable `by-id`/`by-path` links and are canonicalised and compared after
  resolution; a link that resolves to a different block device than at creation is refused.

#### The helper: per-operation, socket-activated, no resident process (chosen)

Three mechanisms were evaluated against the daemonless principle (AGENTS.md: «what must persist
belongs to systemd — unit, timer, socket activation»):

| Option | Resident process | Input surface | Who checks the caller | Verdict |
|---|---|---|---|---|
| **A. Socket-activated helper** — `delonix-storage-helper.socket` with `Accept=yes` and a template `delonix-storage-helper@.service` (root, hardened unit), one process per connection, exits after one request | **None** (systemd holds the socket) | One typed request (JSON) per connection, validated against the allowlist | `SO_PEERCRED` uid/gid against `allowUsers`; socket mode `0660`, group `delonix-storage` | **Chosen** |
| B. `sudoers` rule for a command (`walter ALL=(root) NOPASSWD: /usr/libexec/delonix-storage-helper *`) | None | An argv, with sudo's wildcard semantics | sudo | Rejected: argv wildcards in sudoers are a known injection surface; the policy ends up split between sudoers and the allowlist; prompts/TTY behaviour differs per host |
| C. A resident root daemon | **Yes** | A long-lived socket server | itself | Rejected by the daemonless principle; would need its own ADR with evidence that A cannot work |

In option A the helper process lives only for one request. The unit runs with `ProtectSystem=strict`,
`ProtectHome=yes`, `PrivateTmp=yes`, `NoNewPrivileges=yes`, `CapabilityBoundingSet` limited to
`CAP_SYS_ADMIN CAP_MKNOD CAP_DAC_OVERRIDE CAP_CHOWN CAP_FOWNER`, `DeviceAllow=` restricted to
`/dev/mapper/control`, `/dev/zfs` and the allowlisted devices, and `ReadWritePaths=` limited to the
allowlisted pool paths. No setuid binary is installed. The helper:

- accepts only the operations `probe`, `allocate`, `resize`, `snapshot`, `rollback`, `clone`,
  `release`, `usage`, `grant-device` (an ACL on the device node of a volume it allocated, for the
  calling uid — what a rootless VMM needs to open a zvol or thin LV);
- never accepts `create-pool` or `destroy-pool` over the socket: those are the administrative
  command `delonix-storage-helper create-pool <name>` / `destroy-pool <name>`, run by root at a
  terminal, which reads the `create:` block of the allowlist (D3.3, D5);
- logs every operation to the journal with the caller's uid.

**The explicitly configured rootful node service** is the second accepted mode: an engine started
by the administrator as root (the way `delonix-cri.service` runs on a Kubernetes node) calls the
same helper code **in-process**, against the same allowlist — no socket, but the same validation and
the same refusals. It is selected by the administrator's configuration
(`/etc/delonix/storage-pools.yaml: mode: in-process`), never inferred from `geteuid() == 0`.

When neither is configured, every operation that needs the helper is refused before any command,
with exit class **69** (`Unavailable`) and a storage `DX-` code chosen against the dictionary at
implementation time, naming what is missing and the remedy («enable
`delonix-storage-helper.socket`, add your user to `allowUsers` of pool `fast`»).

#### What each driver needs

| driver | volume `Filesystem` | volume `Block` (VM disk) | hard quota | snapshot | pool creation |
|---|---|---|---|---|---|
| `dir` | unprivileged (directory) | unprivileged (qcow2/raw file) | helper (loop ext4); otherwise monitored | unprivileged (copy) | administrator (`mkdir` + allowlist entry) |
| `btrfs` | unprivileged (subvolume) on a btrfs **mounted by the administrator** where the user can write | unprivileged (raw file with `chattr +C`) | helper (qgroups); otherwise monitored | unprivileged (snapshot of an own subvolume); delete needs `user_subvol_rm_allowed` or the helper | administrator (`create-pool`: `mkfs.btrfs` + mount) |
| `zfs` | delegated (`zfs allow` + readable `/dev/zfs`); mounting needs the helper or `zoned` (OpenZFS ≥ 2.2) — **to be measured** | delegated (`zfs create -s -V`) + device access via `grant-device` | delegated (`refquota`/`volsize`) | delegated | administrator (`create-pool`: `zpool create`) |
| `lvm-thin` | helper (LV + `mkfs` + mount) | helper (thin LV) + `grant-device` | helper (LV size) | helper (thin snapshot) | administrator (`create-pool`: `pvcreate`/`vgcreate`/`lvcreate --type thin-pool`) |

- `create-pool` refuses a device with a filesystem signature (`blkid`) unless the administrator
  passes `--wipe-devices` at the terminal.
- A `Block` VM disk whose device the VMM cannot open read-write is refused at `create`, naming the
  device and the missing ACL — before starting a VMM that would die on `Permission denied`.

#### «Could not determine» is an answer

Every listing that can be partial without privilege is classified, not trusted:

- `lvs`/`vgs` run unprivileged are never interpreted. The LVM driver asks the helper; with no
  helper, `probe` returns `Undetermined { reason: "LVM state needs root (lvs printed: P_global:aux:
  open failed: Permission denied)", remedy: "enable delonix-storage-helper.socket" }`, and
  `get storagepools` shows `UNKNOWN` in the state column, exit class 69 under
  `--detailed-exitcode`.
- The same rule for ZFS without a readable `/dev/zfs` and for btrfs qgroups.
- A gate (unit test over captured output) fails if the LVM driver ever turns the warning-plus-empty
  output into `Available`.

### D4 — How container volumes and VM disks map onto each backend

| | `Filesystem` (container) | `Block` (VM) | shared golden image |
|---|---|---|---|
| `dir` | directory | qcow2 overlay over the image (today) | qcow2 backing file (today) |
| `btrfs` | subvolume | raw file in the VM's subvolume, `+C` (no data CoW) | base volume per image, snapshot per VM |
| `zfs` | dataset | sparse zvol (`-s`) | base zvol per image + snapshot + `zfs clone` per VM |
| `lvm-thin` | thin LV + ext4/xfs | thin LV | base thin LV + thin snapshot per VM (`--setactivationskip n`) |

- The image enters the pool **once** (`qemu-img convert` from the `VmImageStore` qcow2 into the base
  volume, stamped with the digest), and each VM is a thin clone of it: the backend-native equivalent
  of the qcow2 backing file. A base volume with live clones is not deleted (ZFS refuses on its own;
  in the others the engine counts the clones by stamp).
- A `Block` in a pool is **raw**: qcow2 over a zvol or LV would duplicate thin provisioning and
  snapshots. Both local backends accept a raw device (`--disk path=` in Cloud Hypervisor,
  `<disk type='block'>` in libvirt). VM snapshots (`vm snapshot`) become the pool's when the disk is
  in a pool; libvirt's memory snapshot is limited to qcow2 disks and is refused by name on a pool
  disk.
- Containers' `rootfs` (the overlay `upper`) does **not** move to pools in this decision.
- **Proxmox stays as it is**: `disk: <storage>:<gib>` names the node's storage; Proxmox's pool is
  Proxmox's, and the port does not administer it (ADR-0049 D3).
- **libvirt is not used to manage pools** (`virsh pool-define`): the engine would gain a second
  notion of pool that Cloud Hypervisor does not have, and on the tested host libvirt does not
  support `zfs`. The libvirt backend consumes the `Allocation` like any other.

### D5 — The destructive path

- **Ownership by stamp, on the object itself**: LVM tags (`delonix.io_owner=<stack>/<name>`), ZFS
  user properties (`delonix.io:owner`), and in btrfs/dir a `.delonix-volume.json` per volume. The
  stamp carries only references, never credentials (ADR-0009).
- **The engine never destroys a pool.** `delete storagepools <name>` unregisters the use;
  `stack destroy`/`--prune` likewise. Destroying a pool is `delonix-storage-helper destroy-pool
  <name>` run by root at a terminal, and it is refused while the pool holds a volume without this
  engine's stamp, or a stamped volume still referenced by an existing container or VM; a pool the
  allowlist does not mark `create:` (one that existed before the engine) is never destroyed, only
  removed from the file by the administrator.
- Volumes: `volume rm` releases a stamped volume through the driver (or the helper); volumes
  first, the record last. An `apply` that dies between «LV created» and «record written» finds the
  object again by its stamp on the next apply and adopts it, instead of creating a second.

### D6 — Thin provisioning: the engine refuses before the pool fills

`overcommit.maxRatio` (default 1.0 — no over-allocation; see open question 2) bounds the sum of
allocated sizes against capacity; above it `allocate` refuses. In a thin pool, `usage` reads
`data_percent` and `metadata_percent` (`lvs` via the helper, `zpool list`, `btrfs filesystem
usage`); above `alertPct` the engine warns, and above 95 % it refuses new allocations. Autoextend
stays the administrator's.

### D7 — What stays out

- **Ceph/RBD and any storage shared between nodes**: `requires-external-component` (a Ceph cluster
  the engine does not ship), as today; live migration stays blocked by ADR-0031.
- Mounting btrfs inside a userns: the kernel does not allow it.
- vdev layouts beyond the declared, native encryption (ZFS/LUKS), deduplication, replication
  (`zfs send`), shrinking.
- A resident storage daemon (option C above).

## Phased plan

| Phase | What | Files | Proof (battery / chaos) | Cells |
|---|---|---|---|---|
| **P0** | Port, registry, allowlist reader, `kind: StoragePool` with the `dir` driver, three-valued probe, refusal by class 69, `volume create --pool` | `crates/contexts/delonix-storage/{lib,pool,registry,allowlist,ownership,error}.rs`; `Cargo.toml`; `scripts/arch_fitness.py` (LAYERS); `crates/adapters/delonix-volume` (`dir` driver); `crates/contexts/delonix-stack/src/kinds.rs`; `bins/delonix-runtime-bin/src/cmd/{storage_pool,volume,schema}.rs`; `data/pt.po`; `docs/schema/v1/delonix.json`; `crates/contexts/delonix-compute/src/capability.rs` | check «storagepool dir: apply, volume with pool, plan without differences, delete unregisters without deleting data»; check «a pool name not in the allowlist is refused; a manifest with `devices:` is refused by name» | none yet |
| **P1** | The helper: `bins/delonix-storage-helper` (socket-activated), `dist/delonix-storage-helper.{socket,service}`, `create-pool`/`destroy-pool` admin commands, `install.sh --with-storage-helper` | new bin crate (BIN layer); `dist/`; `scripts/install.sh` | in a lab VM: socket refuses a uid not in `allowUsers` (`SO_PEERCRED`); a request naming a device is refused; no helper process remains after a request; check «without the helper, lvm-thin answers UNKNOWN with exit 69, and a fake `lvcreate` in PATH is never called» | — |
| **P2** | btrfs (subvolumes, snapshots; qgroups through the helper) | `crates/providers/delonix-provider-btrfs` | in a lab VM with an extra disk: rootless on an admin-mounted btrfs; chaos `storagepool_apply_dies_midway` | `storage.btrfs` → supported (open question 1) |
| **P3** | ZFS (delegated; datasets, zvols, clones) | `crates/providers/delonix-provider-zfs` | lab VM with `zfsutils-linux`; check of `zfs allow` delegation and of the refusal without it; measure delegated mount and `zoned` | `storage.zfs` → supported |
| **P4** | LVM-thin (through the helper) | `crates/providers/delonix-provider-lvm` | lab VM; chaos `storagepool_thin_full` (filling to the threshold: allocation refused before 100 %); gate on the captured `lvs` warning-plus-empty output | `storage.lvm-thin` → supported |
| **P5** | VM disks in pools (base volume + clone) for libvirt and Cloud Hypervisor, with `grant-device` for rootless VMMs | `crates/contexts/delonix-compute/src/ports.rs` (`LocalDiskImages` takes a pool); `delonix-vm/src/local_ports.rs`; both provider crates | check per backend: VM created in a pool of each driver boots, pool snapshot and restore, `vm rm` frees the clone and leaves the base | `storage.pools` libvirt and CH → supported |
| **P6** | `destroy-pool` and the destructive guards | the helper; the three providers | chaos `storagepool_destroy_owned_only`: a pool without `create:` is never destroyed (read a file afterwards); an unstamped volume blocks the destroy | — |

P1–P6 need root and disks: they run **in a disposable lab VM** (plan decision D1), never on a
production host. A cell becomes `supported` only with the evidence cited in the provider report
(ADR-0050). A `delonix-runtime-sec` pass over the helper's request surface is required before P1
merges.

## Alternatives considered

- **Requiring root for the whole engine when pools are used**: rejected by the owner (D3.1).
- **`sudoers` or a resident daemon for the privileged part**: evaluated in D3 and rejected.
- **libvirt's pools** (`virsh pool-*`): rejected (D4) — they serve only one of the two local
  backends, and on the tested host they do not support ZFS.
- **One crate for the three drivers**: fewer crates, but a ZFS driver would pull the LVM build onto
  a host with neither, and one provider per port is the rule the engine already follows.
- **Pool creation from a manifest** (`mode: create` with `devices:`): rejected by the owner
  (D3.2, D3.3): a manifest is not where raw devices are chosen.
- **A rootless pool in a file** (btrfs/ext4 in an image): not possible on the tested host without
  loop access; not offered.

## Consequences

- The engine never writes to a raw device on a manifest's word: only the administrator's
  `create-pool`, on devices in the root-owned allowlist, with no signature unless
  `--wipe-devices`. That is the boundary this decision moves.
- A rootless node gains pools only where the administrator prepared the ground (allowlist, helper
  socket, a mounted btrfs, ZFS delegation). The refusal says which one is missing.
- The engine keeps running without root; the helper exists only for the duration of a request.
- The root lab (plan D1) becomes a prerequisite of phases P1–P6.

## Open questions (with a recommendation)

1. **Split `storage.zfs-btrfs`?** *Recommendation:* split into `storage.zfs` and `storage.btrfs`
   (catalogue 1.3.0, the old name kept as an alias in the report). The two backends have different
   privilege models and different evidence; one cell would stay `partial` until the slower one is
   proven, hiding a finished backend.
2. **Over-allocation default?** *Recommendation:* keep `overcommit.maxRatio: 1.0` (no
   over-allocation). A full LVM thin pool puts every LV into I/O errors; over-allocating by default
   would make the worst failure mode the default, and an operator who wants Proxmox-style
   over-allocation sets the ratio explicitly.
3. **Provider ids?** *Recommendation:* report pools under the node's `linux` storage provider, one
   capability row per driver (with question 1's split), instead of one provider per driver. The
   drivers share the node, the allowlist and the helper; separate provider ids would multiply matrix
   columns full of `unsupported-by-provider` for the other domains.

## Addendum 2026-10-03 — P0 built: the port, the allowlist, `kind: StoragePool` and the `dir` driver

> **The status line lied for three days** (2026-10-02 to 2026-10-06): it read
> «Nothing implemented» while this addendum, in the same document, said P0 was
> built and the code was in `main`. Corrected on 2026-10-06, and
> `scripts/adr_status_gate.py` now refuses the combination. A record that is not
> updated with the work lies in both directions — it hides what shipped and it
> promises what did not.


- **What was built**: the `delonix-storage` context (`StoragePoolDriver` with probe, allocate,
  release and usage; the registry of drivers by id; the administrator's allowlist; the owner
  stamp), the `dir` driver in `delonix-volume` (`pool_dir.rs`), `kind: StoragePool`
  (`cmd/storage_pool.rs`, also the composition root that registers the drivers), `spec.pool` +
  `spec.size` on `kind: Volume`, and `volume create --pool --size`. Codes DX-1217, DX-1218,
  DX-1219, DX-4203, DX-5201, DX-5202, DX-6203, DX-6204 and DX-7201.
- **The context carries no YAML parser.** The allowlist is YAML, and a context may not depend on
  `serde_yaml` (`arch_fitness.py`). `allowlist::load` takes the decoder from the composition
  root (`allowlist::Decode`); the file checks (owner, mode) stay in the context.
- **A field that is the administrator's is refused when the manifest is loaded.** With strict
  manifests (ADR-0069) the unknown-field check spoke first: `spec: { driver: dir, path: … }`
  answered DX-1000 «unknown field — check the spelling». The refusal by name (DX-1217) now runs
  in `manifest::refused_by_name`, before that check, as the `SystemContainer` privilege fields do.
- **Measured live** (isolated root, a `dir` pool on `/dev/shm`, the section «storage: kind
  StoragePool e volumes num pool» of `scripts/e2e.sh`, 12 checks): no allowlist → 69 (DX-6203);
  an administrator's field in the manifest → DX-1217; a pool the allowlist lacks → 77 (DX-7201);
  an allowlist others can write → DX-1218; apply, then a plan with no difference; a volume's
  data inside the pool with the engine's stamp; a volume above `maxVolumeBytes` → 77; one above
  the over-allocation ceiling → 5 (DX-5202), nothing created; a directory with data and no stamp
  is never adopted (5) and stays intact; `delete storagepools` with volumes → 5 naming them;
  `volume rm` releases the volume and the pool's directory stays; by manifest, pool and volume
  apply, the plan sees no drift, and `stack destroy` takes the volume and stops using the pool.
- **The 95 % refusal was met by accident**: the first live run put the pool on the host's own
  disk, which was 95 % full, and every allocation was refused with DX-5202 — D6 working. The
  battery's pool lives on `/dev/shm` for that reason.
- **An empty directory with the volume's name and no stamp IS taken** (an apply that died between
  `mkdir` and the stamp leaves one); a directory with anything in it is not. Written in the
  driver, with a unit test; the battery covers the second case.
- **Not validated**: a pool on a filesystem other than tmpfs and the host's ext4; the helper and
  every block driver (P1–P6, which need root and a lab VM); a container writing into a pool
  volume (the checks read the directories, not a workload's writes); the capability cells stay
  as they were (the P0 row says «none yet»).

