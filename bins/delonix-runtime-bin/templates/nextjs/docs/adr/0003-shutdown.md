# 0003 — The service handles SIGTERM itself (NEXT_MANUAL_SIG_HANDLE)

Status: accepted (template default)

## Context
A deployment needs: readiness turning 503 before the process stops taking
work, requests in flight finished, pending webhooks delivered, telemetry
flushed, all within a bound, and an exit code that says whether that
happened.

Measured on Next.js 15.5.27 and 16.3.8 (`next start` and the standalone
`server.js` share the code, `next/dist/server/lib/start-server.js`): on
SIGTERM or SIGINT the built-in handler closes the listener at once (a new
connection is refused), waits for requests in flight and for `after()`
callbacks with no deadline, and exits 143 (130 for SIGINT, from the source; SIGTERM measured). There is no
window in which readiness can report `draining`, no deadline, and nothing
flushes the OpenTelemetry exporters.

Next.js skips that handler when `NEXT_MANUAL_SIG_HANDLE=true` is in the
environment of the server process.

## Decision
`pnpm start` and the image set `NEXT_MANUAL_SIG_HANDLE=true`, and
`lib/server/shutdown.ts` installs the handler: readiness `draining` →
`DRAIN_DELAY` (still serving) → new API requests answered 503
`shutting_down` → wait for the in-flight count to reach zero → drain the
dispatcher → flush telemetry (at most 3 s) → exit 0, or 1 when
`SHUTDOWN_TIMEOUT` cut a step short. A request counts as in flight from the
start of `route()` until an `after()` callback reports that its response was
sent; page renders are counted the same way. With `APP_ENV=production` the
server refuses to start without the variable.

Measured with this template: readiness 503 and the API 200 during the drain
delay; a 4-second upload in flight at SIGTERM completed with 201 and the
process exited 0; with `SHUTDOWN_TIMEOUT=1s` the same upload was cut and the
exit code was 1.

## Alternatives
- Keep the built-in handler: correct for in-flight requests, but no drain
  window, no bound, no flush, exit 143.
- A custom server (`http.createServer` + `next()`): `server.close()` would
  stop accepting connections exactly; it cannot be used with
  `output: "standalone"` and gives up the small image.

## Trade-off
Next.js does not expose its HTTP server, so the listener stays open until the
process exits: a connection arriving after the drain delay gets a 503 answer
instead of a refused connection, and a request to a page (not an API route)
in that phase is still rendered. Requests Next.js answers without calling
this project's code (static assets) are not counted. The `after()` callbacks
of other code are not awaited by Next.js in this mode; only what the runtime
tracks is.
