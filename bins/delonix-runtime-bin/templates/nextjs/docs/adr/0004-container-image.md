# 0004 — Two-stage image from the standalone output, an unprivileged user

Status: accepted (template default)

## Context
The image should carry what the server needs to run, not the package manager,
dev dependencies or sources.
Delonix runs containers rootless: uid 0 inside maps to the unprivileged user
that runs Delonix. That bounds a compromise on the host, but a process running
as uid 0 can still rewrite anything in its own container. A second user inside
needs a subordinate uid range on the host (`/etc/subuid`; the Delonix
installer sets one up).

## Decision
`next.config.ts` sets `output: "standalone"`. The build stage
(`node:24-slim`) installs from `pnpm-lock.yaml` when it exists and runs
`pnpm build`, which also copies `.next/static` into `.next/standalone`; the
runtime stage (`node:24-slim`) copies that one directory and runs
`node server.js` as `node` (uid 1000, shipped by the base image, owning
nothing under `/app`), with a read-only root filesystem and a `/tmp` tmpfs in
the manifest.

## Alternatives
- `next start` with the full `node_modules`: no copy step, a much larger image.
- uid 0 inside the container: needs no subordinate uid range, but the process
  could rewrite its own binaries and `/etc`. A host without a range cannot run
  a second user; there, remove `user:` from the manifest.
- Serving `.next/static` from a CDN: what Next.js recommends at scale; one
  container serving its own assets is simpler to start with.

## Trade-off
The engine applies an image's `USER` only when the manifest names the user, so
`USER` in the `Delonixfile` and `user:` in `delonix-manifest.yaml` must agree.
The user cannot bind a port below 1024 and writes only to the `/tmp` tmpfs.
With a read-only filesystem Next.js cannot write its on-disk caches; this
application uses none (every route is dynamic).
