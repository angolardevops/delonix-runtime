# ADR-0056: An ephemeral container's overlay is mounted `volatile`, so its exit stops waiting for the host's writeback

- **Status:** Accepted (2026-09-27) — implemented for `--rm`; see «Implementation» below
- **Date:** 2026-09-27
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0016 (ext4 under the state root), ADR-0037 (the overlay is mounted with
  the new mount API, one `fsconfig` call per layer), #529 and #533 (where the wait was found)

## Context

A container's exit waits for **everything that is dirty on the host filesystem**, not only for
what the container wrote. This was found twice while measuring image performance:

- **#529 (build):** `stop -t 0` of a `sleep infinity` container took ~37 s on a busy host. The
  killed process sat in state `D` in `wb_wait_for_completion`, with the SIGKILL pending, for 1 to
  6 minutes. `stop` gave up at its 30 s cap (`KILL_EXIT_WAIT_TICKS`) and returned 0 over a process
  that was still alive. The comment on `stop` had already recorded this wait at 1.5–6.8 s on
  2026-09-17.
- **#533 (first run of `node:22`):** the layers were extracted in 2.1 s, but the command returned
  after 15–25 s. Measured separately:

  | step | time |
  |---|---|
  | `run -d` with freshly extracted layers (no exit) | 1.8 s, leaving 1.2 GB dirty |
  | a plain `sync` right after | 11.8 s |
  | `stop -t 0` after that `sync` | 0.17 s |
  | `stop -t 0` with fresh layers and no `sync` (2 runs) | 14.2 s, 18.1 s |

**Why.** When the last process of the container's mount namespace exits, the kernel tears down
the overlay, and unmounting an overlay syncs its upper filesystem. The upper filesystem is the
host's (ext4 under the state root, ADR-0016). The dying process waits for that sync.

**Spike (2026-09-27, kernel 7.0.0, ext4 on NVMe, rootless `unshare -rm`, no engine code).** An
overlay was mounted inside a user namespace, then the namespace exited. The table is the time
between the last write and the exit returning, two rounds each:

| case | plain overlay | `volatile` overlay |
|---|---|---|
| 1 GB written **inside** the overlay | 25.1 s · 18.5 s · 43.0 s | 0.001 s · 0.003 s · 0.001 s |
| nothing written inside; 1 GB written to **another file** on the same filesystem | 29.0 s · 12.1 s | 0.001 s · 0.001 s |
| nothing dirty anywhere | 0.2 s · 0.02 s | — |

Three things follow:

1. **The wait is host-wide.** An overlay with no writes at all waited 29 s for someone else's
   gigabyte. Any container exit on a node pays for every other writer on that disk. That is
   why the stops took minutes on a host where other sessions were compiling.
2. **`volatile` removes the wait.** The option (Linux 5.10+) makes the overlay skip every sync
   of its upper filesystem. It is accepted by an unprivileged mount inside a user namespace.
3. **`volatile` leaves a mark.** After a `volatile` mount, the kernel refuses to mount the same
   `workdir` again — `overlay with incompat feature 'volatile' cannot be mounted` — even after a
   clean unmount, until `work/work/incompat/volatile` is removed. This is by design: after a
   crash, the upper dir may be missing writes, and the kernel will not let it be reused silently.

**What others do.** Podman 4.9.3 on this host mounts a `--rm` container's overlay with
`volatile` (shown in the mount options as `fsync=volatile`) and a kept container's without it.
Measured with `podman run [--rm] alpine:3.20 grep ' / ' /proc/self/mountinfo`.

**Guardrails touched.** No daemon, no new dependency (`rustix` already exposes `fsconfig`). This
is a mount option on a boundary the engine already crosses (ADR-0037), inside the same user
namespace, with the spike above run first.

## Decision

**D1 — An ephemeral container's overlay is mounted `volatile`.** An ephemeral container is one
whose upper dir is deleted when it exits, so no write in it can outlive the container:

- a container run with `--rm`.

*Correction at implementation:* this list also named the `build` work containers. They do not
use an overlay at all — `build` prepares a flat rootfs (`prepare_rootfs_flat`), so there is no
overlay unmount for their exit to wait on. `--rm` is the only case.

For it, `volatile` costs nothing. Durability of the upper does not matter because the upper
is discarded, and the `incompat` mark is never seen because that overlay is never mounted again.

**D2 — How it is carried.** The overlay is mounted inside the container's init, from a contract
on disk (`overlay-lowers`, written by `ImageStore::prepare_overlay`), because the mount has to
survive the re-exec of `--net <custom>` and `--pod`. The volatile choice travels the same way: a
second marker, `overlay-volatile`, next to `overlay-lowers`. `mount_overlay_if_marked` adds
`fsconfig_set_flag("volatile")` when the marker is there.

**D3 — A kernel without `volatile` still starts the container.** If the mount with `volatile`
fails with `EINVAL`, it is retried once without it.
- The container starts, and its exit waits as it does today.
- This is not a silent failure in the sense of guardrail 6: `volatile` is the engine's own
  optimisation, not an option the operator set. The fallback is logged at `debug`.

**D4 — Kept containers stay as they are.** A container without `--rm`, a pod member and a CRI
container keep a plain overlay. Two facts are recorded for a later decision:
- `stop` tries to delete `work/` (`unmount_rootfs`, `let _ = remove_dir_all`). If that
  succeeded, the `incompat` mark would not block a `start` after a clean stop. But the kernel
  creates `work/work` with mode `000`, and in the spike a plain `rm -rf` of it by the invoking
  user failed. Whether `unmount_rootfs` actually clears it was not measured.
- After a host crash, the mount would be refused. That refusal is loud and correct, but it
  changes what a crash means for a container whose data lives in its upper dir.

That is a different trade-off from D1, and it needs its own measurement and its own ADR.

## Alternatives considered

- **Do nothing.** Every `run --rm` and every build step keeps paying for the host's writeback.
  - It costs 14–18 s on the first run after an image is unpacked, and minutes on a busy node.
  - `stop` keeps returning 0 at its 30 s cap over a process that is still exiting.
- **Start writeback of the layers as they are published** (`sync_file_range` or a `syncfs`
  after extraction), so the flush overlaps the run instead of landing on the exit. Rejected as
  the fix. The spike's second row shows the exit also waits for writes that are not ours: an
  overlay with no writes waited 29 s. Flushing our layers early would shorten the first run and
  leave the general case untouched. It may still be worth doing on its own, measured.
- **Make every overlay `volatile`** (D1 plus D4 in one step). This gives the largest gain, but
  it changes crash semantics for kept containers, including the `incompat` refusal after a
  crash. It should not ride along with a change whose cost is zero.
- **Stop waiting for the exit in `stop`.** Waiting on the exit is what fixed the second
  incarnation running next to a dying first one (see the comment on `stop`). Giving that up
  moves the problem instead of removing it.

## Consequences

- Easier:
  - `run --rm` and build steps return without waiting for the host's writeback;
  - on a busy node a build stops paying minutes per step.
- Harder:
  - a second on-disk marker to keep in step with `overlay-lowers`;
  - a test that proves an ephemeral container mounts with `volatile` and a kept one does not.
    The overlay's options are visible in `/proc/self/mountinfo` inside the container, where
    Podman's show `fsync=volatile`.
- Inside an ephemeral container, `fsync`/`syncfs` on its root filesystem stop reaching the
  disk. This is intended: the data is discarded at exit. Volumes are bind mounts, not the
  overlay, so writes to a volume are not affected.
- Known limit: a kept container keeps the wait (D4). The host-wide nature of the wait stays
  true for it, and `stop`'s 30 s cap stays the only bound.
- Before merging an implementation: a `delonix-runtime-sec` pass on the mount path (guardrail 5
  asks for one on any change at a namespace boundary), and the measurement of #533 repeated with
  the implementation.

## Implementation

Measured with the implementation, isolated state root, rootless, same host as the spike:

| 1 GB written to another file on the same disk | `run … alpine true` |
|---|---|
| `--rm` (mount options inside show `fsync=volatile`) | **0.16 s · 0.14 s** |
| kept container (no `volatile` in its mount options) | 9.5 s · 14.0 s |

- **The marker.** `ImageStore::prepare_overlay(…, volatile)` writes `overlay-volatile`, and
  REMOVES a stale one when the container is kept. Test:
  `the_volatile_marker_follows_the_last_preparation`.
- **The mount.** `mount_overlay_if_marked` sets the `volatile` flag through `fsconfig`. The
  rootful path (`mount_rootfs_with`) adds `,volatile` to its options.
  - Both retry without the flag on `EINVAL` (D3).
  - Both empty `work/` first. A `--rm` container can be mounted twice, because `--restart` is
    not refused alongside `--rm`, and the kernel would refuse the marked workdir.
  - Measured: `run -d --rm --restart always` restarted twice in 12 s with no mount error, and a
    file in its write layer kept accumulating across the restarts.
- **Not validated:** the rootful path (`mount_rootfs_with`), which cannot be run on this
  production host; and a kernel older than 5.10 (the `EINVAL` fallback).
- **Seen on the way, not changed here:** a `--rm --restart always` container stopped with
  `stop` is not removed.
