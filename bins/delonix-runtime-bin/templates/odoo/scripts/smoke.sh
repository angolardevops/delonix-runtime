#!/bin/sh
# Smoke test against a RUNNING __NAME__ stack (after `delonix stack apply`).
# Usage: scripts/smoke.sh [base-url]   (default http://127.0.0.1:__PORT__)
#   SMOKE_PROFILE=dev  when the stack came from delonix-manifest.dev.yaml
#                      (the database manager is ON there, on purpose).
set -eu
BASE=${1:-http://127.0.0.1:__PORT__}
PROFILE=${SMOKE_PROFILE:-prod}
fail=0
check() { # check <description> <command...>
    desc=$1; shift
    if "$@" >/dev/null 2>&1; then echo "ok   $desc"; else echo "FAIL $desc"; fail=1; fi
}

check "/web/health answers pass" sh -c "curl -fsS '$BASE/web/health' | grep -q '\"pass\"'"
if [ "$PROFILE" = prod ]; then
    check "the login page is served (the database exists)" \
        sh -c "curl -fsS '$BASE/web/login' | grep -q 'name=\"login\"'"
    check "the database manager is disabled" \
        sh -c "curl -fsS '$BASE/web/database/manager' | grep -qi 'disabled'"
    check "the factory password admin/admin no longer works" sh -c "
        curl -fsS -H 'Content-Type: application/json' \
          -d '{\"jsonrpc\":\"2.0\",\"params\":{\"db\":\"__NAME__\",\"login\":\"admin\",\"password\":\"admin\"}}' \
          '$BASE/web/session/authenticate' | grep -q 'AccessDenied'"
else
    check "the database manager is reachable (dev)" \
        sh -c "curl -fsSL '$BASE/web/database/manager' | grep -qi 'master password\\|database'"
fi

[ "$fail" -eq 0 ] && echo "smoke: all checks passed" || { echo "smoke: FAILED"; exit 1; }
