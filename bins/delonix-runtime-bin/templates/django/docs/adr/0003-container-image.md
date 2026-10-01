# 0003 — Two-stage image, uid 0 inside the container, no migrations at start

Status: accepted (template default)

## Context
The image should carry the project and its virtualenv, not uv or its cache.
Delonix runs containers rootless: uid 0 inside maps to the unprivileged user
that runs Delonix. A non-root `USER` inside the image needs a subordinate uid
range on the host (`/etc/subuid`), which not every host has.

## Decision
- Build in `python:3.13-slim` with uv (`uv sync --locked` when `uv.lock` is
  in the context, a plain `uv sync` otherwise, with a message), run on
  `python:3.13-slim` with the same interpreter path, as uid 0 inside the
  container, with a read-only root filesystem, a `/tmp` tmpfs and a volume
  for SQLite and the generated secret key.
- The image's command is gunicorn only. Migrations are a separate, explicit
  step (`manage.py migrate`), run once per release — not by every replica at
  start.

## Alternatives
- `USER 65532` in the image: better defence in depth on hosts with subuid
  ranges and on root-mode engines; fails to start where no range exists.
- Running `migrate` in the entrypoint: convenient for one replica, a race
  (and a surprise schema change) with several.
- distroless: smaller, but no shell for `delonix container exec … migrate`
  debugging and no simple HEALTHCHECK.

## Trade-off
Rootless mapping bounds what uid 0 can do on the host, but it is not the same
as a non-root process inside the container. A fresh deployment answers 503 on
readiness until someone runs the migrations; that is deliberate and visible
(`"dependency": "migrations"`).
