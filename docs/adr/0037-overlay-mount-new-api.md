# ADR-0037: the container overlay mount moves to the new mount API — the classic `mount(2)` silently truncates past ~4 KB of `lowerdir=`

- **Status:** Accepted, implemented and validated live 2026-09-06
- **Deciders:** Walter (owner)
- **Related:** ADR-0035 (`kind: App`) — where this was found; `crates/delonix-runtime/src/lib.rs::mount_overlay_if_marked`/`fsopen_overlay`; `crates/delonix-image/src/overlay.rs::prepare_overlay` (writes the `overlay-lowers` marker this reads).

## Context

Live validation of `kind: App`'s CNB build path (ADR-0035) failed to even start a
container from `paketobuildpacks/builder-jammy-base` — 91 layers — with `delonix:
failed to prepare the rootfs: ENOENT: No such file or directory`, and no App-kind
code in the call path at all: a plain `container run paketobuildpacks/builder-jammy-base
sleep 5` reproduced it identically.

`mount_overlay_if_marked` (`delonix-runtime/src/lib.rs`) built ONE `mount(2)` call
per container, with every lower layer joined into a single `lowerdir=a:b:c:...`
string passed as that syscall's `data` argument. That argument is copied by the
kernel's generic mount code via `strndup_user(data, PAGE_SIZE)` — capped at one
page (4096 bytes on this architecture) — and **silently truncates** past that
rather than erroring. A truncated `lowerdir=` cuts a path in half; overlayfs then
tries to open that half-path, which does not exist, and returns exactly the
`ENOENT` observed.

**Measured, not assumed — in the exact rootless `unshare --user --map-root-user
--mount` sandbox this engine already runs overlay mounts in:**

| real layer paths | `lowerdir=` string length | classic `mount(2)` |
|---|---|---|
| 3 | 837 B | succeeds |
| 20 | 4084 B | succeeds |
| 30 | 5994 B | **fails, `ENOENT` (errno=2)** |
| 42–50 | 8.3–9.8 KB | **fails, `ENOENT`** |

The threshold sits between 4084 B (success) and 5994 B (failure) — consistent
with the 4096-byte `PAGE_SIZE` ceiling on this host. The real bug's own numbers
(91 layers, this engine's actual layer-path length under this state root) come
to **9107 bytes** — over double the limit, confirmed by re-running the real
`paketobuildpacks/builder-jammy-base` pull against the fix (below).

This is not a corner case for a "big" image: any pulled or built image whose
layer count times its layer-path length exceeds ~4 KB hits this, silently,
however many layers that turns out to be on a given `DELONIX_ROOT`.

## Decision

Mount the container overlay through the **new mount API**
(`fsopen`/`fsconfig`/`fsmount`/`move_mount`, Linux 5.2+; overlayfs's
incremental `lowerdir+` append support since 6.5) instead of one joined-string
`mount(2)` call. Each lower directory gets its own `fsconfig(fs_fd,
FSCONFIG_SET_STRING, "lowerdir+", <path>, 0)` call — there is no longer a
single string for any length ceiling to apply to.

```rust
let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)?;
for lower in lowers {
    fsconfig_set_string(&fs, "lowerdir+", *lower)?;
}
fsconfig_set_string(&fs, "upperdir", upperdir)?;
fsconfig_set_string(&fs, "workdir", workdir)?;
fsconfig_create(&fs)?;
let mount_fd = fsmount(&fs, FsMountFlags::FSMOUNT_CLOEXEC, MountAttrFlags::empty())?;
move_mount(&mount_fd, "", CWD, target, MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH)?;
```

**Via `rustix` (1.1, features `mount`+`fs`), not raw syscalls.** `nix` (already a
dependency) has no wrapper for these four syscalls. `rustix` does, completely,
and is **already in this workspace's dependency tree at exactly this version**
— pulled transitively by `tempfile` (used by `delonix-mcp`/`delonix-mgmt`).
Enabling its `mount`/`fs` features on an already-resolved, already-building
crate adds zero new entries to the supply chain; it only compiles code already
vendored from a crate this workspace already trusts. `rustix::io::Errno`
converts to `nix::errno::Errno` at the single call boundary
(`Errno::from_i32(e.raw_os_error())`), so the rest of this file — and both
callers of `mount_overlay_if_marked` — never see the new error type.

**No length-based fallback to the classic call.** The new mount API has been in
every kernel this engine already requires (cgroup v2, `clone3`, pidfd) for
years; keeping two code paths for one operation is exactly the maintenance
burden this repo's own engineering discipline avoids, and a length-triggered
fallback would silently reintroduce the bug for anyone on a kernel where the
"new" path is unavailable, camouflaged as intermittent failure instead of a
clear one.

## Validated live

Three levels of evidence, in order of how close each is to the real bug:

1. **A standalone C harness** (`fsopen`+`fsconfig("lowerdir+", ...)` per path,
   `fsmount`, `move_mount`), run inside the same rootless sandbox: 100
   synthetic lower directories, 19000 bytes of raw path data — nearly 5× the
   classic ceiling — mounted correctly, full content visible, and a write
   inside the mount landed in `upper/` only (copy-up verified by inode/content
   inspection of the untouched lowers).
2. **The Rust implementation**, regression-checked with a plain `alpine`
   container (few layers, `exec`/write round-trip unchanged) and then run
   against a real 91-layer image (a re-pull of `paketobuildpacks/builder-jammy-base`,
   the exact image that started this ADR): container **starts** (`run_exit=0`,
   where it previously failed 100% of the time), `overlay-lowers` for that
   container is confirmed at **91 lines / 9107 bytes**, `cat /etc/os-release`
   inside it returns the real Ubuntu 22.04 Jammy content (not a corrupted or
   partial rootfs), and a write-then-read-back inside the running container
   confirms copy-up semantics are intact.
3. **Full workspace gate** — `cargo fmt --all --check`, `lang_ratchet.py`,
   `cargo clippy --all-targets -D warnings`, `cargo test --workspace`, `cargo
   deny check`.

## Consequences

- `crates/delonix-runtime/Cargo.toml` gains a direct `rustix` dependency
  (`mount`+`fs` features) — see "Decision" above for why this is not new
  supply-chain surface.
- `mount_overlay_if_marked`'s public signature (`fn(&str) -> nix::Result<()>`)
  is unchanged — both existing callers (`setup_rootfs`, `cmd::mapped::ovlhold`)
  needed no changes.
- Every container that uses the overlay rootfs path (the rootless default
  since v0.59.0) now mounts through this code — this is a correctness fix for
  the shared engine, not a `kind: App`-specific patch, even though `kind: App`
  is what found it.
- The bug this closes was previously invisible: nothing in this engine's own
  test suite or E2E battery builds/pulls an image anywhere near the layer
  count needed to trigger it, which is exactly why it survived until a real
  CNB builder image (91 layers) was pulled for the first time.

## Not done here, and why

- **A minimum-kernel-version preflight check.** The new mount API syscalls
  return a clear `ENOSYS` on a kernel that lacks them, which is already a
  legible error; adding a separate version probe would duplicate that
  information without adding anything actionable.
- **Migrating other `mount(2)` call sites in this file** (bind mounts, `/dev`,
  masked paths, etc.) to the new API. None of them build an option string
  anywhere near the page-size ceiling — a single bind mount's `data` argument
  is empty or a handful of flag words. Migrating them would be churn without a
  bug behind it.
