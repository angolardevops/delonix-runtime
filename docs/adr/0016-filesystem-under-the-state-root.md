# ADR-0016: Keep ext4 under the state root; revisit btrfs only for a measured need

- **Status:** Accepted
- **Date:** 2026-08-17
- **Deciders:** Walter Angolar

## Context

A bug report opened this: containers were filling the disk. The host was at
**93%** with `~/.local/share/delonix` at 183 GiB, and the request was to add
compression to how volumes are stored, with fast decompression, to buy a large
saving without paying much latency.

The measurement did not agree with the premise, and that is the whole reason
this ADR exists. Two findings, both from the disk rather than from reading code:

1. **The disk was not full of compressible data — it was full of duplicates.**
   21 containers of the same `kaeso-odoo:16` image each held a full physical copy
   of the same 2.1 GiB tree, every file at `nlink == 1`. ~39 GiB of the 47 GiB
   under `containers/` were byte-identical. That was a defect in how rootfs was
   materialised, not a storage-format problem, and it is now fixed — rootless
   containers share the extracted layers through an overlay (see
   `ImageStore::prepare_overlay` and `mount_overlay_if_marked`).
2. **The single biggest consumer is incompressible.** The 38 GiB volume that
   triggered the report is an Odoo `filestore`: 86 588 files, sampled as JPEG,
   PNG and PDF. Already-compressed formats.

So the question this ADR answers is no longer "how do we compress volumes" but
"is the filesystem under the state root the right one", which is a substrate
decision and touches no guardrail in the skill: no daemon, no tenant awareness,
no new dependency, no privilege boundary. It is about what `/` is formatted as.

### What ext4 does not give us

Measured on this host, not assumed:

- **No reflink.** `cp --reflink=always` returns `Operation not supported`.
- **No transparent compression.** There is no such feature in ext4.
- **No snapshots.**

btrfs is in this kernel (`/proc/filesystems`) and `btrfs-progs` is installed. ZFS
has a module (`zfs.ko.zst`) but no userspace tools.

### What compression would actually buy, per area

`zstd:1` (btrfs's default level) over the REAL data, measured 2026-08-17 after
recovering 32 GiB of stopped containers:

| area | size | zstd-1 | why |
|---|---|---|---|
| `volumes/` | 46 GiB | **1.12×** on the 38 GiB filestore | JPEG/PNG/PDF |
| `containers/` | 36 GiB | — | legacy flat copies; shrinks on its own now |
| `vm-images/` | 22 GiB | **1.00×** | the golden already ships `qemu-img -c zstd` |
| `vms/` | 13 GiB | 2.3–2.7× | qcow2 overlays, guest data |
| `blobs/` | 12 GiB | 3.0× | OCI blobs |
| `layers/` | 3.2 GiB | **7.4×** | extracted image trees |

Extrapolating over the areas that survive the overlay change, transparent
compression is worth roughly **20 GiB of ~132 GiB — about 15%**. Real, but not
the answer to a disk at 92%, and an order of magnitude below what the duplication
fix already returned.

### What the overlay change did to the CoW argument

This matters more than the compression numbers and is the reason this ADR does
not simply say "move to btrfs". The strongest case for a CoW filesystem here used
to be that **rootless had to copy the whole image per container**, and reflink
would have made that copy nearly free. That copy no longer happens: the overlay
shares one extracted tree across every container of an image, which is strictly
better than a cheap copy — there is no second tree to keep coherent at all.

The remaining CoW use is the qcow2 overlay per VM, and qcow2 already implements
backing files itself. So the decision arrived with its main justification already
spent by a change made the same week.

## Decision

**Stay on ext4. Do not migrate the state root to btrfs or ZFS now.**

The measured problem — duplication — is fixed at the layer that caused it. The
remaining wins are ~15% of space and snapshots, neither of which justifies
reformatting the root filesystem of a host that runs production workloads.

Revisit this with a written trigger, not a feeling. Any ONE of these reopens it:

- the state root passes **200 GiB** with `containers/` no longer dominated by
  pre-overlay legacy copies (i.e. the growth is real, not the old duplication);
- a need for **snapshot-based backup** of the state root that `delonix backup`
  cannot serve (it already archives registry + volume data per resource);
- a **second measurement** showing compression would return materially more than
  the ~15% measured here — most plausibly if the workload mix shifts from media
  and pre-compressed images toward databases and text.

When reopened, it needs a spike that **could not be run here**: mounting a
loopback btrfs requires `CAP_SYS_ADMIN` and this host has no sudo. Compression
ratios and the absence of reflink were measurable without privilege; latency
under load, real `compress=zstd:1` behaviour and the migration of ~132 GiB were
not. That spike is a precondition, in the same shape as ADR-0008's phase 2 being
blocked on a real Proxmox node — we do not adopt a substrate we have never seen
running.

## Alternatives considered

**Compress in the application layer** (the original proposal — compress volumes
on write, decompress on read). Rejected, and it is worth writing down why,
because the instinct behind it was sound. Per-file userspace compression forces a
whole block to be decompressed for a random read; the Postgres volumes in
`kaeso-pgdata*` read 8K pages and would pay that on every one. A filesystem
compresses per extent and can read part of one. If compression is ever worth it,
it belongs in the filesystem, not here. The latency worry attached to the
proposal is also real but points the other way: `zstd:1` decompresses at
~1.5–2 GB/s per core, so on compressible data reading compressed can be *faster*
than reading raw, because fewer bytes leave the disk. The problem was never the
CPU cost — it was that the biggest consumer does not compress.

**Migrate to btrfs now.** Rejected as premature, not as wrong. It is the option
this ADR would choose if the triggers above fire: transparent `zstd:1`, reflink
and snapshots in one substrate, with the engine unchanged. Rejected today because
the number that justified it (39 GiB of duplication) was fixed elsewhere, the
remaining ~15% does not pay for reformatting a production host, and we cannot
even test it here.

**Migrate to ZFS.** Rejected before btrfs on availability alone: no userspace
tools installed, and it is an out-of-tree module — a kernel upgrade can leave a
host unable to mount its own state root. For a runtime whose whole posture is to
survive on someone else's production host, that is the wrong kind of coupling.
btrfs being in-tree is the deciding property, not a feature comparison.

**Do nothing at all** — no ADR, no triggers. Rejected: the question will come
back the next time a disk fills, and without this record the same measurement
gets redone from scratch, most likely landing on the same wrong premise.

## Consequences

- **Nothing changes today**, which is the point: no migration risk on a host
  running production workloads, and no new failure mode in the storage path.
- **The engine stays substrate-agnostic.** Nothing in `delonix-image` or
  `delonix-runtime` assumes a filesystem feature. If btrfs arrives later, it
  arrives underneath and the engine does not learn about it — which is also why
  this decision is cheap to reverse.
- **The ~15% is left on the table, knowingly.** That is a documented cost, not an
  oversight. Anyone proposing compression again should read the per-area table
  first: `vm-images/` at 1.00× and the filestore at 1.12× are where the intuition
  breaks.
- **Legacy flat `containers/` (36 GiB) still shrinks by hand**, and only for
  containers created before the overlay change. `delonix system prune` on stopped
  containers is the lever; new containers never grow it. Nothing reclaims it
  automatically.
- **The spike stays owed.** If this is reopened, the loopback-btrfs measurement
  must happen on a host with privilege, before any migration. Reopening without
  it repeats exactly the mistake this ADR was written to stop: deciding a storage
  question from a plausible premise instead of a measured one.
