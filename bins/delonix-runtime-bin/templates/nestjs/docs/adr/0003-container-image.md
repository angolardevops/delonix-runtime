# 0003 — Two-stage image, production dependencies only, an unprivileged user

Status: accepted (template default)

## Context
The image should carry the compiled service and its production dependencies,
not TypeScript, the Nest CLI, the test tooling or a package manager.
Delonix runs containers rootless: uid 0 inside maps to the unprivileged user
that runs Delonix. That bounds a compromise on the host, but a process running
as uid 0 can still rewrite anything in its own container. A second user inside
needs a subordinate uid range on the host (`/etc/subuid`; the Delonix
installer sets one up).

## Decision
Build in `node:24-slim`: install every dependency, `nest build`, then
`pnpm prune --prod`. The runtime stage is a second `node:24-slim` that takes
`package.json`, the pruned `node_modules` and `dist/` — nothing is installed
there. It runs as `node` (uid 1000, shipped by the base image, owning nothing
under `/app`), with a read-only root filesystem and a `/tmp` tmpfs in the
manifest. The build installs from `pnpm-lock.yaml` when the context has one,
and resolves otherwise so a freshly generated project still builds.

## Alternatives
- `pnpm install --prod` in the runtime stage: installs the dependencies a
  second time and leaves corepack and pnpm in the image.
- uid 0 inside the container: needs no subordinate uid range, but the process
  could rewrite its own binaries and `/etc`. A host without a range cannot run
  a second user; there, remove `user:` from the manifest.
- A distroless Node image: smaller, no shell for debugging.

## Trade-off
pnpm's `node_modules` is a tree of symlinks into `.pnpm`; the copy between
stages relies on the builder keeping them as links (`delonix build` and Docker
both do). The engine applies an image's `USER` only when the manifest names
the user, so `USER` in the `Delonixfile` and `user:` in
`delonix-manifest.yaml` must agree. The user cannot bind a port below 1024 and
writes only to the `/tmp` tmpfs.
