# 0004 — Two-stage image from the standalone output, uid 0 inside the container

Status: accepted (template default)

## Context
The image should carry what the server needs to run, not the package manager,
dev dependencies or sources. Delonix runs containers rootless: uid 0 inside
maps to the unprivileged user that runs Delonix. A non-root `USER` inside the
image needs a subordinate uid range on the host (`/etc/subuid`), which not
every host has.

## Decision
`next.config.ts` sets `output: "standalone"`. The build stage
(`node:24-slim`) installs from `pnpm-lock.yaml` when it exists and runs
`pnpm build`, which also copies `.next/static` into `.next/standalone`; the
runtime stage (`node:24-slim`) copies that one directory and runs
`node server.js` as uid 0 inside the container, with a read-only root
filesystem and a `/tmp` tmpfs in the manifest.

## Alternatives
- `next start` with the full `node_modules`: no copy step, a much larger image.
- `USER node` in the image: better defence in depth on hosts with subuid
  ranges and on root-mode engines; fails to start where no range exists.
- Serving `.next/static` from a CDN: what Next.js recommends at scale; one
  container serving its own assets is simpler to start with.

## Trade-off
Rootless mapping bounds what uid 0 can do on the host, but it is not the same
as a non-root process inside the container. Add `USER node` once every target
host has subuid ranges. With a read-only filesystem Next.js cannot write its
on-disk caches; this application uses none (every route is dynamic).
