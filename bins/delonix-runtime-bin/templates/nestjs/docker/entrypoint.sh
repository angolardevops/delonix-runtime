#!/bin/sh
# The container entry point. It starts the compiled service with the
# OpenTelemetry instrumentation loaded BEFORE the application (the HTTP
# instrumentation only patches the runtime when it is required first), then
# `exec`s so node becomes PID 1 and receives SIGTERM directly for the ordered,
# bounded shutdown in dist/main.js. Any extra arguments are passed through.
set -e
exec node --require ./dist/instrumentation.js dist/main.js "$@"
