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
What a non-root user may write is what the image says it owns, nothing more.

The ownership the image recorded is kept in an **owners index**: when a layer is unpacked, the
entries its tar headers give to a user or group other than root are written to a sidecar beside
the layer directory, and a container gets the image's merged index (per path, the topmost layer
holding it decides) next to its `overlay-lowers`. The container's init applies it once, after
`pivot_root`, only when the process runs as a non-root user, and never onto another filesystem.

The other way to keep ownership — writing the real owners to disk in the layer store, by
unpacking inside a mapped user namespace — was the first choice and was set aside by the P1
spike. It makes every reader of the store cope with files it does not own: the flat export
`build` copies from, the flat→overlay migration, the scanner, `system df`, and every removal.
That is a wide change for a security fix. Its advantage is real and is recorded here: it costs
nothing per container, whereas the index copies up, into each container's write layer, the
entries the image gives to a non-root owner. For most images that is a handful of directories
(two entries for `haproxy:3.4-alpine`); for an image that ships a large tree owned by its user
(`COPY --chown`) it is that tree, once per container. It is still bounded by what the image
gave away, where the walk it replaces was bounded by the whole image.

- rootless without subuid: a second uid cannot exist. An image USER cannot be applied there, and
  the run says so and continues as uid 0 (the notice of P0).

**D3 — mounts are not re-owned.** A bind mount keeps the ownership the host gave it, always. A
named volume that is still EMPTY belongs to whoever the image gives its mount point to (Docker's
rule, and the reason `-v data:/var/lib/app` works there) and, when the image names nobody, to
the container's user: a volume created for this container, with nothing in it, at a path the
image does not own, has no other owner to infer, and leaving it to root would make the user's
own volume unwritable. A volume that already has content is left alone. A named volume is
recognised by its shape in the volume store, never by its name.

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
- The layer store stays as it is: every entry owned by whoever runs the engine. Beside each
  layer directory there is one more file, its owners.
- First start with a non-root user stops costing a copy of the image (M4); it costs a copy of
  what the image gives to non-root owners.

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

**P1 is implemented** (2026-10-01), in three changes:

- the image store records each layer's non-root owners while unpacking it (the unpack loop is
  `tar`'s own, repeated because `Archive::unpack` gives no access to the headers), and writes
  the merged index beside a container's overlay; a layer unpacked by an older engine has its
  headers read once;
- the init applies the index and owns empty named volumes as D3 says; `chown_tree_once` is gone;
- `container commit` writes the owners the CONTAINER sees. This was found by validation step 6:
  a rootless commit packs from the host side and wrote host numbers into the tar headers —
  measured, every entry `1000:1000` and the directory of the container's uid 1000
  `100999:100999`. With the index, such an image would have given its whole filesystem to uid
  1000. The packer now passes each owner through the rootless map. A commit also dropped the
  base image's USER; it is kept. A layer committed by an older engine still carries the wrong
  numbers.

Measured after P1 on `haproxy:3.4-alpine` with `-u haproxy`: writing `/etc/passwd` and replacing
the binary are refused; 2 of 987 entries are owned by the user (its directory and its volume),
down from 940; a bind-mounted host directory keeps `1000:1000` and its owner still writes to it;
the write layer is 56 K, down from 13 MB; the image's own directory and two empty named volumes
are writable; a volume that already held data is untouched; the same holds on a custom network;
nothing changes without `--user`. `scripts/e2e.sh` checks each of validation steps 1–4 and the
commit; run against the engine before P1, four of the six ownership checks fail.

Not validated: validation step 5 as a check in the battery (the size was measured by hand), a
real root (non-rootless) host, the CRI on a node, and `odoo:20.0`'s first start with a user on
an idle disk. A container created before P1 keeps what the old walk did to it.

P2 is not implemented.

P0 is in the change that adds this ADR: `resolve_run` reads the image's `user` and returns a
notice when it is not applied (first pass only, never with `--user`, never for a root USER).
Measured: `haproxy:3.4-alpine` warns once in English and in Portuguese, on the default network
and on a custom one (two passes, one warning), with stdout unchanged; `-u haproxy` and
`alpine:3.20` do not warn.
