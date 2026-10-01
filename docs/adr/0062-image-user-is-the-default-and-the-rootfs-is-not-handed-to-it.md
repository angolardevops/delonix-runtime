# ADR-0062: An image's USER is the default user, and the root filesystem stops being handed to it

- **Status:** Proposed (2026-10-01)
- **Date:** 2026-10-01
- **Deciders:** Walter Angolar
- **Relates to:** the engine's third principle (rootless-first: privilege is an explicit opt-in,
  told to the operator, never a silent default), `docs/cli-stability.md` (`container run` is a
  stable group), the init template contract (ADR-0061, in review; its templates run as uid 0 today),
  the `odoo` template (#625, #631), where this was first measured.

## Context

An OCI image config can name the user its process runs as (`USER` in a Dockerfile, `config.User`
in the image). Docker, Podman and containerd apply it when the caller names no user. This engine
does not, and the path that does switch users has two defects of its own. All of it was measured
on 2026-10-01 on one host (Ubuntu 24.04, rootless with subuid, engine built from `362b37d9`; the
code path is the one on `main` at `9eb2a14d`), with `haproxy:3.4-alpine`, a 15 MB image that
declares `USER haproxy` (uid 99).

**M1 — the image's USER is read by nothing.** `container run haproxy:3.4-alpine id` answers
`uid=0(root)`. `odoo:20.0` (config `user = odoo`) does the same and logs `Running as user 'root'
is a security risk`. `resolve_run` turns `--user` into a uid and leaves the user unset when the
flag is absent (`run_user = None`); the image config's `user` is not consulted. It has been so
since v1.0.0 (`git show v1.0.0`, `v3.1.0`, `v4.0.0`: no reader of `config.user` on the run
path). No error, no warning.

**M2 — with `--user`, the application user becomes the owner of the whole root filesystem.**
Before switching, the container's init runs `chown_tree_once("/", uid, gid)`: a recursive
`lchown` of everything under `/` except `proc`, `sys` and `dev`. Measured inside a container
started with `-u haproxy`: 940 of 986 entries are owned by `haproxy`, `/etc/passwd` and
`/usr/local/sbin/haproxy` among them. As `haproxy`, `echo … >> /etc/passwd` succeeded and
`cp /bin/busybox /usr/local/sbin/haproxy` replaced the binary. A non-root user that owns the
system files is not the confinement a non-root user is chosen for.

**M3 — the same walk crosses mount points, and a bind mount is the host.** `container run -u
haproxy -v <host-dir>:/app …` changed the owner of the host directory and the files in it from
`1000:1000` to `100098:100098` (the subuid that maps uid 99). The original owner could no longer
write his own file (`Permission denied`). A named volume is re-owned the same way, which is the
only reason the user can write to it today (`-v vol:/data`, `stat` → `99:99`, write succeeds).

**M4 — the cost grows with the image.** Each `lchown` of a file that lives in a shared read-only
layer copies it up into the container's writable layer. For the 15 MB image the writable layer
grew to 13 MB. For `odoo:20.0` (about 2 GB unpacked) a first start with `-u odoo` was still
walking the tree after 16 minutes on a host whose disk was saturated; it was not timed on an
idle disk.

**M5 — why the walk exists.** Layers are stored with every entry owned by the invoking user
(the rootless map makes that uid 0 inside the container), so the ownership the image recorded is
gone: `/var/lib/haproxy` is `haproxy:haproxy` in the image and `root:root` in a container run
without `-u`. A non-root user could not write its own directories, and the walk was the remedy.

**M6 — every entry point reaches the same code.** `--user` on the CLI, `user:` in a manifest,
`User` in the Docker API, and the kubelet's `run_as_user`/`run_as_username` through the CRI all
end in the same `run_user`. The kubelet sends the image's user name when the pod sets none, so a
pod from an image with a `USER` gets M2 and M4 today. This last sentence is from reading the CRI
and the kubelet's contract; it was not measured on a node.

## Decision

**D1 — the image's USER is the default.** With no `--user` (and no `user:`), the process runs as
the user the image declares. `--user 0` (or `root`) is the explicit way to run as root in the
container. An image that declares none runs as uid 0, as now.

**D2 — no user is ever given the root filesystem.** The recursive `lchown` of `/` is removed.
What a non-root user may write is what the image says it owns, nothing more. The ownership the
image recorded is preserved when a layer is unpacked, so no per-container walk is needed:

- rootless with subuid: layers are unpacked inside the mapped user namespace (the mechanism
  `reexec_mapped` already provides for `__buildtar`, `__rmtree` and `__duusage`), so an entry
  owned by uid 99 in the image is owned by the subuid for 99 on disk;
- root: unpacking as root already can preserve ownership;
- rootless without subuid: a second uid cannot exist. An image USER cannot be applied there, and
  the run says so and continues as uid 0 (the notice of P0), rather than refusing every image
  that declares a user.

If the P1 spike shows that preserving ownership in the layer store is not viable (every reader
of the store has to cope with files it does not own: `image save`, `push`, `commit`, `du`,
`rm`), the fallback is an ownership index written at unpack time and applied at start only to
the entries the image gives to a non-root owner. Either way the walk over `/` goes.

**D3 — mounts are not re-owned.** A bind mount keeps the ownership the host gave it, always. A
named volume mounted EMPTY over a directory of the image takes that directory's owner and mode
once, at its first mount (Docker's rule, and the reason `-v data:/var/lib/app` works there);
a volume that already has content is left alone.

**D4 — one resolution for every entry point.** The CLI, manifests, compose, the Docker API and
the CRI resolve the user through the same function in the Compute context, so the default
cannot differ between them.

**D5 — it lands in three steps, and the default changes last.**

| Step | What | Breaking |
|---|---|---|
| P0 | The run SAYS when an image's USER is not applied, and names `-u`. No behaviour changes. | no |
| P1 | D2 and D3: the walk is removed, ownership is preserved, mounts are left alone. Fixes M2, M3, M4 for everyone who already passes `--user`. | yes, for a caller who relied on a bind mount being re-owned |
| P2 | D1: the default flips. | yes: a container that ran as uid 0 runs as the image's user |

P1 is a security and data-ownership fix and does not wait for a major version; its release note
names the bind-mount change. P2 goes in the next major, with `--user 0` documented as the way
back, and `docs/cli-stability.md` updated in the same change.

## Alternatives considered

- **Leave it as it is.** The engine keeps accepting a declaration and ignoring it (M1), which is
  the class of defect it refuses elsewhere (`--security-opt seccomp=`, `-v …:z`,
  `--network-alias`), and M2/M3 stay.
- **Warn forever (P0 only).** Honest about M1, but M2 and M3 are defects in a path people already
  use, and a warning does not fix them.
- **Flip the default now, with the current mechanism.** Every image with a USER would get M2, M3
  and M4 without asking for them. Strictly worse than today for those images.
- **Keep the walk but stop it at mount points.** Fixes M3 only. The user still owns `/etc/passwd`
  and the binaries (M2), and the cost (M4) stays.
- **idmapped mounts.** The kernel can present a mount with shifted ownership without touching the
  disk. Not evaluated here: whether an unprivileged user may create one over the layers this
  engine uses was not measured, and D2 does not depend on it. If the P1 spike finds it works
  rootless, it is a third way to implement D2, not a different decision.

## Consequences

- An image that declares a non-root user runs as that user by default (after P2), as it does
  under Docker and Podman. The `init` templates that carry a note about running as uid 0
  (`odoo`, and the application templates of ADR-0061) lose it.
- A non-root user can no longer rewrite system files of its own container (after P1).
- A bind mount is never re-owned (after P1). A development flow that mounts a source tree and
  runs as a non-root user reads it as before (files are normally world-readable) and can no
  longer write into it unless the host permissions allow it. That is the same as Docker rootless,
  and it is the honest state: the alternative is the engine changing who owns the operator's
  files.
- Layers hold files owned by subuids (if D2's first option holds). Every reader of the layer
  store has to go through the mapped helpers, as volumes written by containers already require.
- First start with a non-root user stops costing a copy of the image (M4).

## Validation required before merge of an implementation

P1, each as a check in `scripts/e2e.sh` that fails with the change reverted:

1. As a non-root user, writing `/etc/passwd` and replacing a binary under `/usr` are refused.
2. The image's own directory for that user (`/var/lib/haproxy` in the test image) is owned by it
   and writable.
3. A host directory bind-mounted into a container run with `--user` keeps its owner and mode on
   the host.
4. A named volume mounted empty over an image directory is writable by the image's user; a
   volume with content keeps its ownership.
5. The container's writable layer after a first start is not a copy of the image (size bound).
6. `image save`, `image push`, `container commit`, `image rm` and `system df` work on an image
   whose layers hold non-root owners.

P2:

7. `container run <image with USER> id -u` answers the image's uid; `--user 0` answers 0.
8. The same through a manifest, compose, the Docker API and the CRI (the last on a real node).
9. Rootless without subuid: the run continues as uid 0 and says why.

## Implementation

P0 is in the change that adds this ADR: `resolve_run` reads the image's `user` and returns a
notice when it is not applied (first pass only, never with `--user`, never for a root USER).
Measured: `haproxy:3.4-alpine` warns once in English and in Portuguese, on the default network
and on a custom one (two passes, one warning), with stdout unchanged; `-u haproxy` and
`alpine:3.20` do not warn. P1 and P2 are not implemented.
