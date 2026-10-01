# 0003 — Two-stage image, an unprivileged user inside the container

Status: accepted (template default)

## Context
The image should carry the application and its dependencies, not the package
manager, its cache, the tests or the source checkout.
Delonix runs containers rootless: uid 0 inside maps to the unprivileged user
that runs Delonix. That bounds a compromise on the host, but a process running
as uid 0 can still rewrite anything in its own container. A second user inside
needs a subordinate uid range on the host (`/etc/subuid`; the Delonix
installer sets one up).

## Decision
Build in `python:3.13-slim` with a pinned uv: `uv sync --locked --no-dev
--no-editable` when the context has `uv.lock`, a plain resolve (with a
message) when it does not, so a freshly generated project still builds. The
runtime stage is the same base image plus `/app/.venv` — the project is
installed into the environment as a wheel, so no source tree is copied. It
runs as the user `app` (uid 10001), created in the image and owning nothing in
it, with a read-only root filesystem and a `/tmp` tmpfs in the manifest.

## Alternatives
- uid 0 inside the container: needs no subordinate uid range, but the process
  could rewrite its own binaries and `/etc`. A host without a range cannot run
  a second user; there, remove `user:` from the manifest.
- A distroless Python runtime: smaller, but the environment's interpreter path
  must match the build stage exactly, and there is no shell for debugging.
- Copying uv from its official image (`COPY --from=ghcr.io/astral-sh/uv`):
  the documented pattern; `pip install uv==<version>` gives the same pinned
  binary without depending on a second registry.

## Trade-off
The engine applies an image's `USER` only when the manifest names the user, so
`USER` in the `Delonixfile` and `user:` in `delonix-manifest.yaml` must agree.
The user cannot bind a port below 1024 and writes only to the `/tmp` tmpfs. An
image built without `uv.lock` is not reproducible; CI refuses to run without
the lock for that reason.
