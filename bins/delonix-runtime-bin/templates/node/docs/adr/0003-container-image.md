# 0003 — Two-stage image, an unprivileged user inside the container

Status: accepted (template default)

## Context
The image should carry production dependencies and compiled JavaScript, not
the compiler and dev dependencies.
Delonix runs containers rootless: uid 0 inside maps to the unprivileged user
that runs Delonix. That bounds a compromise on the host, but a process running
as uid 0 can still rewrite anything in its own container. A second user inside
needs a subordinate uid range on the host (`/etc/subuid`; the Delonix
installer sets one up).

## Decision
Build and prune in a `node:24-slim` stage, copy `node_modules`, `dist` and
`package.json` into a second `node:24-slim` stage, and run as `node`
(uid 1000, shipped by the base image, owning nothing under `/app`), with a
read-only root filesystem and a `/tmp` tmpfs in the manifest. Dependencies
install from `pnpm-lock.yaml` when it exists.

## Alternatives
- uid 0 inside the container: needs no subordinate uid range, but the process
  could rewrite its own binaries and `/etc`. A host without a range cannot run
  a second user; there, remove `user:` from the manifest.
- A distroless runtime: smaller, but no shell for debugging.

## Trade-off
The engine applies an image's `USER` only when the manifest names the user, so
`USER` in the `Delonixfile` and `user:` in `delonix-manifest.yaml` must agree.
The user cannot bind a port below 1024 and writes only to the `/tmp` tmpfs.
