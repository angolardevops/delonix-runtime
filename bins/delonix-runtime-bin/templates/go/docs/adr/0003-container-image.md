# 0003 — Two-stage image, an unprivileged user inside the container

Status: accepted (template default)

## Context
The image should carry the binary and CA roots, not the toolchain.
Delonix runs containers rootless: uid 0 inside maps to the unprivileged user
that runs Delonix. That bounds a compromise on the host, but a process running
as uid 0 can still rewrite anything in its own container. A second user inside
needs a subordinate uid range on the host (`/etc/subuid`; the Delonix
installer sets one up).

## Decision
Build in `golang:<version>-alpine`, run on `alpine` as the user `app`
(uid 10001), created in the image and owning nothing in it, with a read-only
root filesystem and a `/tmp` tmpfs in the manifest.

## Alternatives
- uid 0 inside the container: needs no subordinate uid range, but the process
  could rewrite its own binaries and `/etc`. A host without a range cannot run
  a second user; there, remove `user:` from the manifest.
- distroless/scratch runtime: smaller, but no shell or `wget` for the
  HEALTHCHECK.

## Trade-off
The engine applies an image's `USER` only when the manifest names the user, so
`USER` in the `Delonixfile` and `user:` in `delonix-manifest.yaml` must agree.
The user cannot bind a port below 1024 and writes only to the `/tmp` tmpfs.
