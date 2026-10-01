# 0003 — Two-stage image, uid 0 inside the container

Status: accepted (template default)

## Context
The image should carry the application and its dependencies, not the package
manager, its cache, the tests or the source checkout. Delonix runs containers
rootless: uid 0 inside maps to the unprivileged user that runs Delonix. A
non-root `USER` inside the image needs a subordinate uid range on the host
(`/etc/subuid`), which not every host has.

## Decision
Build in `python:3.13-slim` with a pinned uv: `uv sync --locked --no-dev
--no-editable` when the context has `uv.lock`, a plain resolve (with a
message) when it does not, so a freshly generated project still builds. The
runtime stage is the same base image plus `/app/.venv` — the project is
installed into the environment as a wheel, so no source tree is copied. It
runs as uid 0 inside the container, with a read-only root filesystem and a
`/tmp` tmpfs in the manifest.

## Alternatives
- `USER 65532` in the image: better defence in depth on hosts with subuid
  ranges and on root-mode engines; fails to start where no range exists.
- A distroless Python runtime: smaller, but the environment's interpreter path
  must match the build stage exactly, and there is no shell for debugging.
- Copying uv from its official image (`COPY --from=ghcr.io/astral-sh/uv`):
  the documented pattern; `pip install uv==<version>` gives the same pinned
  binary without depending on a second registry.

## Trade-off
Rootless mapping bounds what uid 0 can do on the host, but it is not the same
as a non-root process inside the container. Add `USER` once every target host
has subuid ranges. An image built without `uv.lock` is not reproducible; CI
refuses to run without the lock for that reason.
