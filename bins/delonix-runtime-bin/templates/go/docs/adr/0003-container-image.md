# 0003 — Two-stage image, uid 0 inside the container

Status: accepted (template default)

## Context
The image should carry the binary and CA roots, not the toolchain. Delonix
runs containers rootless: uid 0 inside maps to the unprivileged user that runs
Delonix. A non-root `USER` inside the image needs a subordinate uid range on
the host (`/etc/subuid`), which not every host has.

## Decision
Build in `golang:<version>-alpine`, run on `alpine`, as uid 0 inside the
container, with a read-only root filesystem and a `/tmp` tmpfs in the manifest.

## Alternatives
- `USER 65532` in the image: better defence in depth on hosts with subuid
  ranges and on root-mode engines; fails to start where no range exists.
- distroless/scratch runtime: smaller, but no shell or `wget` for the
  HEALTHCHECK.

## Trade-off
Rootless mapping bounds what uid 0 can do on the host, but it is not the same
as a non-root process inside the container (a compromised process can still
write anything the container's root can). Add `USER` once every target host
has subuid ranges.
