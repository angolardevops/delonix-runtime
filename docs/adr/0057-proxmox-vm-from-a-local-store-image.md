# ADR-0057: A Proxmox VM boots from an image in the engine's own store, uploaded and imported by the node

- **Status:** Accepted (2026-10-10, by the owner) — built and live-measured (#541)
- **Date:** 2026-09-27
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0008 (the remote backend and why its disk names something on the far
  side), ADR-0049 (the Proxmox transport, the task ledger, D3 on administering the provider),
  ADR-0054 (`providers.yaml`, where a target's settings live)

## Context

`vm create --backend proxmox --disk <x>` accepts two forms today, both naming something that
already exists on the node: `template:<vmid>` (clone a template) and `<storage>:<gib>` (a fresh
empty disk). A local path is refused, by name: "a local path has no meaning on a remote node".

So a VM with an OS in it needs a template that someone built on the node by hand. The images
the engine itself builds, pulls and verifies (`delonix-vm-base:*`, `delonix-vm-k8s:*`, imported
appliances) live in the local `VmImageStore`, and the CLI already resolves `--disk <name>` to
the qcow2 there (`resolve_image_ref`). A manual step in the customer's path is a blocker, not a
footnote.

Measured on the lab node (PVE 9.2.2, 2026-09-27):

- A dir storage accepts a qcow2 under `import/` only when its `content` includes `import`. The
  stock `local` storage does not list it.
- With `import` enabled, `qm create --scsi0 local-lvm:0,import-from=local:import/<file>.qcow2`
  created a 3 GiB disk (the image's virtual size) from the 276 MiB Debian base in 4 s, with
  `boot: order=scsi0`.
- The API has the two halves: `POST /nodes/{node}/storage/{storage}/upload` (multipart, with
  `content`, `filename`, `checksum` and `checksum-algorithm`, answered by an `imgcopy` task) and
  `POST /nodes/{node}/qemu` with `import-from` in the disk property.

## Decision

1. **A third disk form: a local image file.** When the disk the engine hands the backend is an
   existing local file (what `resolve_image_ref` returns for a store image), the backend uploads
   it to the node and creates the VM with `scsi0: <disk-storage>:0,import-from=<volid>`. The two
   existing forms are unchanged, and anything else is still refused naming all three.

2. **The upload is named by content, and is a cache.** The volume is
   `<import-storage>:import/delonix-<first 16 hex of the sha256>.<qcow2|raw>`. When the node
   already lists that volume, nothing is uploaded: two VMs from one image cost one upload. The
   upload carries `checksum`/`checksum-algorithm=sha256`, so the node verifies the bytes it
   received, and a mismatch fails the upload task instead of creating a VM from a damaged image.
   The engine never deletes an uploaded image on its own: other VMs may still be importing from
   it, and removing it is the operator's decision.

3. **Two storages, from the target's configuration.** The import storage (default `local`) and
   the disk storage (default `local-lvm`) come from `DELONIX_PROXMOX_IMPORT_STORAGE` and
   `DELONIX_PROXMOX_DISK_STORAGE`, which `providers.yaml` writes as `storage.import` and
   `storage.disk` (ADR-0054). They describe the node, not the VM, like the bridge and the VLAN.

4. **The engine never changes a storage's content types.** Enabling `import` on a storage is
   administering the provider, which ADR-0049 D3 keeps out. When the import storage does not list
   `import`, the create is refused before anything is uploaded, naming the storage and the
   command that enables it. When the storage has less free space than the image, the create is
   refused too, with both numbers: the node's own refusal would arrive inside a failed task,
   after the upload had already been paid for.

5. **`diskSize` grows the imported disk, never shrinks it**, the same rule as a template clone:
   an import lands at the image's virtual size, a larger `diskSize` is a resize after the create,
   and a smaller one is refused before the upload with both numbers.

6. **Everything after the VM exists is undone on failure**, as for the other two forms. A failed
   upload leaves no VM (the upload comes before `next_vmid`). A partial upload leaves no volume
   the cache would trust, because the node verifies the checksum before it keeps the file.

## Alternatives considered

- **`download-url` instead of an upload**: the node fetches the image itself. It needs a URL the
  node can reach and the engine to serve one; the store is a directory on this host, not a web
  server. Out of scope, but it is the natural path for images already published to a registry.
- **Build a template on the node from the image, then clone it**: one more object on the node
  that the engine would have to own and garbage-collect, for the same result as `import-from`.
- **Enable `import` on the storage automatically**: rejected by D4.

## Consequences

- `vm create --backend proxmox --disk delonix-vm-base:debian-bookworm` works with no template
  on the node, once the operator has enabled `import` on one dir storage.
- The upload costs the image's size once per node and image. The cache is keyed by content, so a
  rebuilt image with the same tag is a new upload, and the old one stays until the operator
  removes it.
- The Proxmox matrix gains the upload, the storage status read and the volume read.

## Not decided here

- Removing uploaded images (`vm image prune --backend proxmox` or similar).
- `download-url` and `oci-registry-pull` as ways to reach the node without uploading.
