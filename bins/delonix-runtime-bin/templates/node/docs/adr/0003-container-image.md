# 0003 — Two-stage image, uid 0 inside the container

Status: accepted (template default)

## Context
The image should carry production dependencies and compiled JavaScript, not
the compiler and dev dependencies. Delonix runs containers rootless: uid 0
inside maps to the unprivileged user that runs Delonix. A non-root `USER`
inside the image needs a subordinate uid range on the host (`/etc/subuid`),
which not every host has.

## Decision
Build and prune in a `node:24-slim` stage, copy `node_modules`, `dist` and
`package.json` into a second `node:24-slim` stage, run as uid 0 inside the
container with a read-only root filesystem and a `/tmp` tmpfs in the manifest.
Dependencies install from `pnpm-lock.yaml` when it exists.

## Alternatives
- `USER node` in the image: better defence in depth on hosts with subuid
  ranges and on root-mode engines; fails to start where no range exists.
- A distroless runtime: smaller, but no shell for debugging.

## Trade-off
Rootless mapping bounds what uid 0 can do on the host, but it is not the same
as a non-root process inside the container. Add `USER node` once every target
host has subuid ranges.
