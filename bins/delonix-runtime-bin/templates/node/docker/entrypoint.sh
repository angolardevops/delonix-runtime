#!/bin/sh
# Container entrypoint: a seam for release-time work (a schema migration, a
# cache warm-up) before the server starts. The example has none, so it just
# execs the command (the OTel-instrumented node process by default). A failure
# here stops the container before anything listens.
set -eu
cd /app
exec "$@"
