#!/bin/sh
# The container entry point. It keeps PID 1 as the service itself (`exec`), so
# SIGTERM reaches it directly and the ordered shutdown in main.rs runs — no
# shell swallowing the signal. Put any one-off pre-flight (a migration, a
# config check) above the exec; keep it fast, the healthcheck starts at boot.
set -eu

exec /usr/local/bin/__NAME__ "$@"
