# Proxmox VM migration on NFS shared storage — a mount-option prerequisite

**Status:** production prerequisite, root-caused live on a two-node PVE 9.2.2
cluster (2026-10-22). Not a delonix defect — a Proxmox + NFS storage behaviour
that affects `qm start` identically.

## Symptom

`delonix vm move <name> --node <target>` (cold, intra-cluster) succeeds — the
cluster lists the VM on the target and its config is readable there — but a
`delonix vm start <name>` issued **immediately afterwards** fails:

```
error[DX-9513] registry error: proxmox: task failed:
  mkdir /mnt/pve/sharedstore/images/<vmid>: File exists at
  /usr/share/perl5/PVE/Storage/Plugin.pm line 1055.
```

The node's own `qmstart` task log shows the real sequence:

```
failed to stat '/mnt/pve/sharedstore/images/<vmid>/vm-<vmid>-cloudinit.qcow2'
TASK ERROR: mkdir /mnt/pve/sharedstore/images/<vmid>: File exists
```

It shows up only for VMs with a cloud-init drive (every VM delonix creates on
Proxmox carries one), only on NFS `dir`-layout storage, and only when the start
closely follows the migrate.

## Root cause — NFS attribute-cache staleness, not a logic bug

The migrate writes the VM's per-VM directory (`images/<vmid>/`) and its
`vm-<vmid>-cloudinit.qcow2` from the **source** node. On the **target** node,
the NFS client's attribute cache is briefly stale: it already sees the
directory (so PVE's `mkdir` returns `EEXIST`) but cannot yet `stat` the
cloud-init file inside it, so PVE's cloud-init regeneration on `qmstart` aborts
instead of overwriting the file in place.

Two measurements make the attribution definitive:

1. The failing task is a plain `qmstart` — the **same** task `qm start` runs. A
   manual `qm start <vmid>` on the target node fails identically when run
   immediately after the migrate, and **succeeds** once the cache settles. The
   caller (delonix vs. `qm`) makes no difference.
2. Re-issuing `delonix vm start` after ~20 s (cache refreshed) succeeds with no
   code change, and the cloud-init file `stat`s fine from a shell throughout —
   the file was always there; only the target node's cached view was stale.

The engine's default ad-hoc mount was `nfs4 vers=4.2` with attribute caching
on. A storage whose mount shortens or disables attribute caching does not race.

## Fix — tune the shared NFS storage's mount options

Disable attribute caching (or shorten it) on the NFS storage used for VM disks:

```bash
pvesm set <storage> --options vers=3,actimeo=0,lookupcache=none
# then remount on every node (umount /mnt/pve/<storage>; PVE remounts on access)
```

`actimeo=0` + `lookupcache=none` make the target node read directory and file
attributes from the server rather than its cache, so the cloud-init file is
visible the moment the migrate finishes.

## Live evidence

On a two-node PVE 9.2.2 cluster (`pve` + `pveb`) with this NFS storage tuned,
both gated migration tests pass against the real node:

| Test (`delonix-proxmox/tests/live.rs`) | Result |
|---|---|
| `a_running_vm_moves_live_on_shared_storage_and_keeps_running` | ok (45 s) — guest OS uptime continuous, never rebooted |
| `a_stopped_vm_moves_to_another_node_and_the_cluster_lists_it_there` | ok (11 s) — after the mount options above |

With the default caching mount, only the first (live, no stop/start) passed;
the second flaked at the start-after-move step described here.

## Scope

This is storage configuration, not a provider change — forcing a start retry
into delonix would mask a misconfigured storage rather than fix it. HA and
replication remain `requires-external-component` (ADR-0049 D3: cluster
administration, outside the engine). See `docs/providers/capability-matrix.md`
for the `vm.migration.cold` / `vm.migration.live` rows this validates.
