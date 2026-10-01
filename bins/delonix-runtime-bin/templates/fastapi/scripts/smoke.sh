#!/bin/sh
# Smoke test against a RUNNING instance: health, the example capability, and
# the error shape. Exits non-zero on the first failure.
#   ./scripts/smoke.sh http://localhost:__PORT__
set -eu
BASE="${1:-http://localhost:__PORT__}"
fail() { echo "smoke: FAIL — $*" >&2; exit 1; }
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

[ "$(code "$BASE/api/v1/health/live")" = 200 ] || fail "live is not 200"
[ "$(code "$BASE/api/v1/health/ready")" = 200 ] || fail "ready is not 200"
created=$(curl -sf -X POST "$BASE/api/v1/notes" -H 'Content-Type: application/json' \
  -d '{"title":"smoke","body":"created by scripts/smoke.sh"}') || fail "create failed"
id=$(printf '%s' "$created" | sed -n 's/.*"id":"\([0-9a-f]*\)".*/\1/p')
[ -n "$id" ] || fail "no id in $created"
[ "$(code "$BASE/api/v1/notes/$id")" = 200 ] || fail "created note not found"
[ "$(code "$BASE/api/v1/notes?limit=1")" = 200 ] || fail "list failed"
[ "$(code -X POST "$BASE/api/v1/notes" -H 'Content-Type: application/json' -d '{"title":""}')" = 422 ] \
  || fail "invalid input not refused with 422"
[ "$(code -X POST "$BASE/api/v1/notes" -H 'Content-Type: application/json' -d '{nope')" = 400 ] \
  || fail "malformed JSON not refused with 400"
[ "$(code "$BASE/api/v1/notes/does-not-exist")" = 404 ] || fail "missing note not 404"
echo "smoke: OK ($BASE)"
