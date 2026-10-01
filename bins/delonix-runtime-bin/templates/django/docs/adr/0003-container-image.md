# 0003 — Two-stage image, an unprivileged user, migrations at start

Status: accepted (template default)

## Context
The image should carry the project and its virtualenv, not uv or its cache.
Delonix runs containers rootless: uid 0 inside maps to the unprivileged user
that runs Delonix. That bounds a compromise on the host, but a process running
as uid 0 can still rewrite anything in its own container. A second user inside
needs a subordinate uid range on the host (`/etc/subuid`; the Delonix
installer sets one up).
A fresh deployment has no schema, and `delonix init --up` should end with a
service that answers ready.

## Decision
- Build in `python:3.13-slim` with uv (`uv sync --locked` when `uv.lock` is
  in the context, a plain `uv sync` otherwise, with a message), run on
  `python:3.13-slim` with the same interpreter path, as the user `app`
  (uid 10001), with a read-only root filesystem, a `/tmp` tmpfs and a volume
  for SQLite and the generated secret key. `app` owns `/data` in the image and
  nothing else; the volume takes that owner on its first mount.
- `docker/entrypoint.sh` runs `manage.py migrate` before gunicorn only when
  `MIGRATE_ON_START=true`. The manifest sets it, because its default is one
  replica on SQLite. gunicorn itself never migrates.

## Alternatives
- uid 0 inside the container: needs no subordinate uid range, but the process
  could rewrite its own binaries and `/etc`. A host without a range cannot run
  a second user; there, remove `user:` from the manifest.
- Always migrating at start: with several replicas each would race to
  migrate, and a schema change would arrive with whichever replica restarts
  first.
- Never migrating at start: a fresh deployment answers 503 on readiness
  (`"dependency": "migrations"`) until someone runs the command — what
  happens here once `MIGRATE_ON_START` is removed.
- distroless: smaller, but no shell for the entrypoint or for
  `delonix container exec … migrate`, and no simple HEALTHCHECK.

## Trade-off
The engine applies an image's `USER` only when the manifest names the user, so
`USER` in the `Delonixfile` and `user:` in `delonix-manifest.yaml` must agree.
The user cannot bind a port below 1024 and writes only to the `/tmp` tmpfs and
the `/data` volume. With `MIGRATE_ON_START` a failed migration stops the
container before it listens; with several replicas the variable must be
removed and the migration run as a release step.
