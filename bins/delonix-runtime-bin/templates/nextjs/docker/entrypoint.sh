#!/bin/sh
# Container entrypoint. The one place for pre-start work — wait for a
# dependency, run a migration — before the server takes over. The final `exec`
# replaces this shell with `node` so the server is PID 1: SIGTERM reaches it
# and the ordered shutdown (docs/adr/0003-shutdown.md) runs. Add setup ABOVE
# the exec line; never background the server.
set -e

exec "$@"
