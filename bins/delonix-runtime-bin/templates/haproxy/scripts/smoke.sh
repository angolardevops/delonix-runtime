#!/bin/sh
# Smoke test against a RUNNING __NAME__ (after `delonix stack apply`).
# Checks what this template promises: health, HTTPS and the redirect to it,
# the security headers, request-id correlation, that no Server
# header leaks, and that the proxy does not handle traffic as root.
# Usage: scripts/smoke.sh [https-base-url] [http-base-url]
#        (defaults https://127.0.0.1:__TLS_PORT__ and http://127.0.0.1:__PORT__)
#
# `-k`: the checks are about what the server answers, not about whether THIS
# machine trusts the certificate — the last line reports that separately.
set -eu
BASE=${1:-https://127.0.0.1:__TLS_PORT__}
PLAIN=${2:-http://127.0.0.1:__PORT__}
cd "$(dirname "$0")/.."
fail=0
check() { # check <description> <command...>
    desc=$1; shift
    if "$@" >/dev/null 2>&1; then echo "ok   $desc"; else echo "FAIL $desc"; fail=1; fi
}

check "/healthz answers ok over HTTP"   sh -c "curl -fsS '$PLAIN/healthz' | grep -qx ok"
check "/ answers 200 over HTTPS"        curl -kfsS -o /dev/null "$BASE/"
check "HTTP redirects to HTTPS (308)"   sh -c "curl -s -o /dev/null -D - '$PLAIN/some/page?x=1' | tr -d '\r' | grep -qix 'location: https://127.0.0.1:__TLS_PORT__/some/page?x=1'"
check "HTTP/2 is negotiated"            sh -c "[ \"\$(curl -ks -o /dev/null -w '%{http_version}' '$BASE/')\" = 2 ]"
check "TLS 1.1 is refused"              sh -c "! curl -ks -o /dev/null --tls-max 1.1 '$BASE/'"
check "the private key is not world-readable" sh -c "[ \"\$(stat -c %a tls/tls.key)\" = 600 ]"
check "X-Content-Type-Options: nosniff" sh -c "curl -kfsS -o /dev/null -D - '$BASE/' | tr -d '\r' | grep -qix 'x-content-type-options: nosniff'"
check "a request id is minted"          sh -c "curl -kfsS -o /dev/null -D - '$BASE/' | grep -qi '^x-request-id: .'"
check "a caller's request id is kept"   sh -c "curl -kfsS -o /dev/null -D - -H 'X-Request-ID: smoke-123' '$BASE/' | tr -d '\r' | grep -qix 'x-request-id: smoke-123'"
check "the /healthz answer carries the headers too" sh -c "curl -fsS -o /dev/null -D - '$PLAIN/healthz' | grep -qi '^x-content-type-options:'"
check "no Server header"                sh -c "! curl -kfsS -o /dev/null -D - '$BASE/' | grep -qi '^server:'"
check "the worker runs as haproxy, not root" sh -c "delonix container exec __NAME__ sh -c 'for p in /proc/[0-9]*; do grep -qs ^haproxy \$p/comm && awk \"/^Uid:/{print \\\$2}\" \$p/status; done' | grep -qvx 0"

if curl -fsS -o /dev/null "https://localhost:__TLS_PORT__/" 2>/dev/null; then
    echo "note this machine trusts the certificate"
else
    echo "note this machine does not trust the certificate (see README.md, TLS)"
fi
[ "$fail" -eq 0 ] && echo "smoke: all checks passed" || { echo "smoke: FAILED"; exit 1; }
