# ADR-0014: The network runtime dir is scoped to the state root, not just to the uid

**Status: Accepted (2026-08-15).** Implemented in the same change. This ADR exists because the
decision moves a path that a running holder resolves, which is the exact shape of the v0.34.1
regression this repo already paid for once — so the reasoning has to survive the commit.

## Decision taken

`infra::runtime_dir()` gains a fixed-width suffix derived from `DELONIX_ROOT`, **for
non-default roots only**:

| State root | Runtime dir |
|---|---|
| the default one (no `DELONIX_ROOT`, or set to the default path) | `/tmp/delonix-net-<uid>` — unchanged, byte for byte |
| any other | `/tmp/delonix-net-<uid>-<fnv32(root):08x>` |

`DELONIX_NET_RUNTIME_DIR` keeps overriding both, unchanged.

Alongside it, `ensure_up()` and `teardown()` now take the `FileLock` that already existed in
`infra.rs` — they were the only mutators of this state that took no lock at all.

## Context — measured 2026-08-15, not assumed

The user reported `/tmp/delonix-net-1000/` being wiped twice in one day and attributed it to
`systemd-tmpfiles`. It was not:

- the only rule matching `/tmp` is `D /tmp 1777 root root 30d` — **age 30 days**, and the
  directory was created that morning;
- `systemd-tmpfiles-clean.service` ran **once** that day, at 15:37:41;
- `/etc/tmpfiles.d/` is empty, so nothing shortens the age.

What was actually happening is described by a comment that has been sitting in `ensure_up`
since 2026-08-10: **the sockets are per-UID while the pidfiles are per-ROOT.** Two roots on one
login each read their own (absent) pidfile, each conclude "no infra", and the second calls
`teardown()` — deleting the first one's sockets and unplugging every workload on that netns.

Measured on the host, live: **four `slirp4netns` bound to the same
`/tmp/delonix-net-1000/slirp.sock`**, from four different roots (three ephemeral
`/tmp/delonix-itest-<pid>-0` from parallel test runs, one default). Only the newest owned the
path; the other three were listening on unlinked inodes. Three stomps, not two.

The guard that was supposed to prevent this — `control_reachable()` — is a bare
`UnixStream::connect(...).is_ok()`. It only fires while the socket file is present AND
accepting: once anything has already unlinked it, `connect` gets `ENOENT`, the guard answers
"no owner", and the next root tears down freely. **The first collision unlocks the rest.**

## Decision, and why this shape

**Separate the resource rather than serialise access to it.** Two designs answer the same
question:

- **A — separate:** derive the runtime dir from the root. Roots stop sharing sockets, so they
  stop being able to stomp each other. The per-root lock that already exists lands in the right
  scope by construction.
- **B — serialise:** add a per-UID lock around `ensure_up`/`teardown` and keep the sharing.

A is chosen. B manages the sharing instead of removing it, and it would have meant building a
*second* locking mechanism next to the per-root `FileLock` already in this file — two answers to
one question is how they start to disagree, the argument this repo already used to delete
`publish_port_allow`. B survives only in reduced form: the existing lock now also covers
`ensure_up`/`teardown`, which protects two concurrent invocations of the *same* root.

**The default root keeps the bare name, and that is the load-bearing part of the decision.** A
holder already running from an older binary resolves exactly the path it always did, so there is
no in-place-upgrade trap of the kind that forced `stale_holder_message` and
`legacy_control_sock_path` into existence in v0.34.2. Only the roots that *cause* the collision
move, and those are by definition short-lived (test roots) or secondary (a second engine).

**The comparison is against the computed `default_root()`, not against "is `DELONIX_ROOT`
set".** Someone who points `DELONIX_ROOT` at the default path by hand would otherwise get its
own socket directory while sharing everyone else's pidfiles — the same split-brain, inverted.

**Fixed width, not the root's own path.** `runtime_dir` was moved out of `DELONIX_ROOT` in the
first place because `AF_UNIX` caps `sun_path` at 108 bytes and a deep test root pushed the socket
past it. An eight-hex-digit suffix cannot reintroduce that; there is an explicit length assertion
in the test, with a deliberately deep root.

**`fnv32` and not a new hash.** It already exists in `delonix-net-rules`, is already used to
derive bridge names, and already has a reference test pinning its values. `DefaultHasher` was
rejected: it is not stable across Rust versions, so a rebuild could silently orphan a live
holder's directory.

## Guard-rails checked

- **Daemonless:** unchanged — no process is added, and the dir is still created lazily.
- **Boundary with the private PaaS:** untouched. No new dependency; `fnv32` is already in-tree.
- **No private dependency:** none added.
- **New boundary needing a GO/NO-GO spike:** none. This moves a path derivation inside an
  existing boundary; it does not create one.

## Consequences

- A test root and the normal engine can now run concurrently without destroying each other.
  This is what makes `scripts/e2e.sh` safe to isolate with `DELONIX_ROOT` alone — though the
  battery's header still tells callers to export both, and should keep doing so until this has
  been exercised for a while.
- **A non-default root that had a live holder before this change will not be found after it.**
  Its sockets stay at the old path with nothing pointing at them. That is accepted: such roots
  are test/secondary by construction, and `delonix net netns down` from the old binary — or a
  reboot — clears them. The default root, where a long-lived holder actually lives, does not move.
- **Two self-deadlocks were introduced by wiring the lock, and only one was caught by reading.**
  `flock` is held per open file description, so any function already inside the critical section
  that calls the public, locking variant blocks the process against itself — hanging **every
  `container run` on the host, forever**.
  - The first was found before running anything: `ensure_up` (holding the lock) calling the
    public `teardown`. Fixed with `teardown_locked()` for the four internal callers (`release`,
    `reap_orphan_refs`, `ensure_up`, `start_pin`).
  - **The second shipped and was caught only by `scripts/e2e.sh`**: `acquire()` takes the lock
    and then brings the infra up on the first user, so it called the now-locking `ensure_up`.
    The battery stalled for 31 minutes on a `container run --net`, with the kernel showing the
    process in `locks_lock_inode_wait` holding two lock fds. Fixed with `ensure_up_locked()`.
  - The method failure is the part worth keeping: when the `teardown` half was written, the
    callers *of `teardown`* were enumerated and the callers *of `ensure_up`* were not. **Adding a
    lock to a function that already has callers requires walking both directions**, plus the
    transitive callees inside the section. All 1086 unit tests passed with the second deadlock
    in place — nothing short of running the CLI would have found it.

## What this ADR does NOT decide

- **`control_reachable()` stays a bare `connect`.** Scoping the dir removes the cross-root race
  that made its weakness matter, but it is still a TOCTOU probe rather than a lock, and it is
  still the thing standing between a second process of the *same* root and a live holder. The
  `FileLock` now covers that path; whether the probe should become an ownership assertion of its
  own is a separate question.
- **Nothing about `reap_orphan_slirp`**, which is the same bug class one layer down (a
  destructive sweep over a shared resource with no ownership test) and is fixed in the same
  change by a different mechanism — the `--api-socket` path as an ownership token.
- **Whether the ephemeral `delonix-itest-<pid>-0` roots should exist at all.** They are created
  outside this repo; this ADR only stops them from being destructive.
