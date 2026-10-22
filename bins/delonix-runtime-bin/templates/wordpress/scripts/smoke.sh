#!/bin/sh
# Smoke test against a RUNNING __NAME__ stack (after `delonix stack apply` or
# `delonix compose up`).
# Usage: scripts/smoke.sh [base-url]   (default http://127.0.0.1:__PORT__)
set -eu
BASE=${1:-http://127.0.0.1:__PORT__}
fail=0
check() { # check <description> <command...>
    desc=$1; shift
    if "$@" >/dev/null 2>&1; then echo "ok   $desc"; else echo "FAIL $desc"; fail=1; fi
}

# /wp-login.php is served whether or not the install wizard has run yet.
check "the login page is served" \
    sh -c "curl -fsSL '$BASE/wp-login.php' | grep -qi 'user_login\\|wordpress'"
# Before install, WordPress redirects the home page to /wp-admin/install.php;
# after install it serves the site. Either is a healthy sign the PHP app and
# its database are up.
check "WordPress answers on the home page" \
    sh -c "curl -fsSL -o /dev/null -w '%{http_code}' '$BASE/' | grep -qE '^(200|30[0-9])$'"

[ "$fail" -eq 0 ] && echo "smoke: all checks passed" || { echo "smoke: FAILED"; exit 1; }
