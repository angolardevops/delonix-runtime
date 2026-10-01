#!/bin/sh
# Starts the production server (`pnpm start`, i.e. .next/standalone/server.js)
# on a port, runs scripts/smoke.sh against it, then sends SIGTERM and checks
# that the ordered shutdown exits 0. Needs `pnpm build` first. Used by CI.
#   SMOKE_PORT=18080 sh scripts/smoke-production.sh
set -eu
PORT="${SMOKE_PORT:-18080}"
BASE="http://127.0.0.1:$PORT"
[ -f .next/standalone/server.js ] || { echo "smoke: run 'pnpm build' first" >&2; exit 1; }

NEXT_MANUAL_SIG_HANDLE=true APP_ENV=production PORT="$PORT" HOSTNAME=127.0.0.1 \
  node .next/standalone/server.js &
pid=$!
trap 'kill "$pid" 2>/dev/null || true' EXIT

i=0
until curl -sf "$BASE/api/v1/health/ready" >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -lt 60 ] || { echo "smoke: FAIL — not ready after 30s" >&2; exit 1; }
  kill -0 "$pid" 2>/dev/null || { echo "smoke: FAIL — the server exited at startup" >&2; exit 1; }
  sleep 0.5
done

sh scripts/smoke.sh "$BASE"

kill -TERM "$pid"
code=0
wait "$pid" || code=$?
trap - EXIT
[ "$code" = 0 ] || { echo "smoke: FAIL — exit code $code after SIGTERM, expected 0" >&2; exit 1; }
echo "smoke: shutdown OK (exit 0)"
