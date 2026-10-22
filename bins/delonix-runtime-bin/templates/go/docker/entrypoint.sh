#!/bin/sh
# Container entrypoint: the one place to run pre-flight before the server.
#
# Kept deliberately thin — the Go binary needs no migrations or asset build, so
# this just execs the command it is given (the server by default). Add readiness
# waits here if your deploy needs them, e.g. block until REDIS_URL / RABBITMQ_URL
# answer; the app itself already degrades gracefully when they are absent (the
# cache falls back to memory, messaging stays off), so a wait is optional.
set -eu
exec "$@"
