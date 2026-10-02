#!/bin/sh
# Smoke test against a RUNNING __NAME__ (after `delonix stack apply`).
# Checks what this template promises: health, HTTPS and the redirect to it,
# the security headers, request-id correlation, a real 404, the ACME
# challenge path, and that the status page is not public.
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

# Nothing listening is one finding, not a dozen: without this, every check
# below fails for the same reason, and the two written as "this must NOT
# happen" pass.
if ! curl -s -o /dev/null --max-time 5 "$PLAIN/healthz"; then
    echo "FAIL nothing answers at $PLAIN — is __NAME__ up? (delonix stack apply)"
    echo "smoke: FAILED"
    exit 1
fi

check "/healthz answers ok over HTTP"   sh -c "curl -fsS '$PLAIN/healthz' | grep -qx ok"
check "/ answers 200 over HTTPS"        curl -kfsS -o /dev/null "$BASE/"
check "HTTP redirects to HTTPS (308)"   sh -c "curl -s -o /dev/null -D - '$PLAIN/some/page?x=1' | tr -d '\r' | grep -qix 'location: https://127.0.0.1:__TLS_PORT__/some/page?x=1'"
check "HTTP/2 is negotiated"            sh -c "[ \"\$(curl -ks -o /dev/null -w '%{http_version}' '$BASE/')\" = 2 ]"
check "TLS 1.1 is refused"              sh -c "curl -ks -o /dev/null --tlsv1.2 '$BASE/' && ! curl -ks -o /dev/null --tls-max 1.1 '$BASE/'"
check "the private key is not world-readable" sh -c "[ \"\$(stat -c %a tls/tls.key)\" = 600 ]"
check "X-Content-Type-Options: nosniff" sh -c "curl -kfsS -o /dev/null -D - '$BASE/' | tr -d '\r' | grep -qix 'x-content-type-options: nosniff'"
check "a request id is minted"          sh -c "curl -kfsS -o /dev/null -D - '$BASE/' | grep -qi '^x-request-id: .'"
check "a caller's request id is kept"   sh -c "curl -kfsS -o /dev/null -D - -H 'X-Request-ID: smoke-123' '$BASE/' | tr -d '\r' | grep -qix 'x-request-id: smoke-123'"
check "Content-Security-Policy is set" sh -c "curl -kfsS -o /dev/null -D - '$BASE/' | grep -qi '^content-security-policy:'"
check "an unknown path is a 404"       sh -c "[ \"\$(curl -ks -o /dev/null -w '%{http_code}' '$BASE/no-such-page')\" = 404 ]"
check "/server-status is not public"    sh -c "[ \"\$(curl -s -o /dev/null -w '%{http_code}' '$PLAIN/server-status')\" = 403 ]"
check "no version in the Server header" sh -c "curl -kfsS -o /dev/null -D - '$BASE/' | tr -d '\r' | grep -qix 'server: apache'"
# A name of its own per run, as a real challenge token is: never asked for
# before it exists.
token="smoke-$$"
mkdir -p acme/.well-known/acme-challenge && echo smoke-token > "acme/.well-known/acme-challenge/$token"
check "the ACME challenge path is served over HTTP, not redirected" sh -c "curl -fsS '$PLAIN/.well-known/acme-challenge/$token' | grep -qx smoke-token"
check "a challenge file written after a 404 for its path is served" sh -c "curl -s -o /dev/null '$PLAIN/.well-known/acme-challenge/$token-late'; echo late > 'acme/.well-known/acme-challenge/$token-late'; curl -fsS '$PLAIN/.well-known/acme-challenge/$token-late' | grep -qx late"
rm -f "acme/.well-known/acme-challenge/$token" "acme/.well-known/acme-challenge/$token-late"

if curl -fsS -o /dev/null "https://localhost:__TLS_PORT__/" 2>/dev/null; then
    echo "note this machine trusts the certificate"
else
    echo "note this machine does not trust the certificate (see README.md, TLS)"
fi
[ "$fail" -eq 0 ] && echo "smoke: all checks passed" || { echo "smoke: FAILED"; exit 1; }
