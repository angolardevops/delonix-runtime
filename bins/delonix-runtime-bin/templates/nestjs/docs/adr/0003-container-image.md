# 0003 — Two-stage image, production dependencies only, uid 0 inside the container

Status: accepted (template default)

## Context
The image should carry the compiled service and its production dependencies,
not TypeScript, the Nest CLI or the test tooling. Delonix runs containers
rootless: uid 0 inside maps to the unprivileged user that runs Delonix. A
non-root `USER` inside the image needs a subordinate uid range on the host
(`/etc/subuid`), which not every host has.

## Decision
Build in `node:24-slim` (all dependencies, `nest build`). The runtime stage is
a second `node:24-slim` that takes `package.json`, the lock and `dist/`, and
runs `pnpm install --prod --frozen-lockfile`. It runs as uid 0 inside the
container, with a read-only root filesystem and a `/tmp` tmpfs in the
manifest. The build installs from `pnpm-lock.yaml` when the context has one,
and resolves otherwise so a freshly generated project still builds.

## Alternatives
- Copy `node_modules` from the build stage after `pnpm prune --prod`: pnpm's
  `node_modules` is a tree of symlinks into `.pnpm`, which a plain file copy
  between stages does not preserve.
- `USER node` in the image: better defence in depth on hosts with subuid
  ranges and on root-mode engines; fails to start where no range exists.
- A distroless Node image: smaller, no shell for debugging and no corepack.

## Trade-off
Dependencies are installed twice (the second time from the pnpm store cache
when the builder keeps cache mounts). Rootless mapping bounds what uid 0 can
do on the host, but it is not the same as a non-root process inside the
container. Add `USER` once every target host has subuid ranges.
