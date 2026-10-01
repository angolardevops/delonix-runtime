#!/bin/sh
# Smoke test against a RUNNING __NAME__ (after `delonix stack apply`).
# Checks what this template promises: health, the page, the security headers,
# request-id correlation, a real 404, and that the status page is not public.
# Usage: scripts/smoke.sh [base-url]   (default http://127.0.0.1:__PORT__)
set -eu
BASE=${1:-http://127.0.0.1:__PORT__}
fail=0
check() { # check <description> <command...>
    desc=$1; shift
    if "$@" >/dev/null 2>&1; then echo "ok   $desc"; else echo "FAIL $desc"; fail=1; fi
}

check "/healthz answers ok"            sh -c "curl -fsS '$BASE/healthz' | grep -qx ok"
check "/ answers 200"                  curl -fsS -o /dev/null "$BASE/"
check "X-Content-Type-Options: nosniff" sh -c "curl -fsS -o /dev/null -D - '$BASE/' | tr -d '\r' | grep -qix 'x-content-type-options: nosniff'"
check "Content-Security-Policy is set" sh -c "curl -fsS -o /dev/null -D - '$BASE/' | grep -qi '^content-security-policy:'"
check "a request id is minted"         sh -c "curl -fsS -o /dev/null -D - '$BASE/' | grep -qi '^x-request-id: .'"
check "a caller's request id is kept"  sh -c "curl -fsS -o /dev/null -D - -H 'X-Request-ID: smoke-123' '$BASE/' | tr -d '\r' | grep -qix 'x-request-id: smoke-123'"
check "an unknown path is a 404"       sh -c "[ \"\$(curl -s -o /dev/null -w '%{http_code}' '$BASE/no-such-page')\" = 404 ]"
check "/server-status is not public"    sh -c "[ \"\$(curl -s -o /dev/null -w '%{http_code}' '$BASE/server-status')\" = 403 ]"
check "no version in the Server header" sh -c "curl -fsS -o /dev/null -D - '$BASE/' | tr -d '\r' | grep -qix 'server: apache'"

[ "$fail" -eq 0 ] && echo "smoke: all checks passed" || { echo "smoke: FAILED"; exit 1; }
