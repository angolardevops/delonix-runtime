#!/bin/sh
# The CI gate, run IDENTICALLY by .github/workflows/ci.yml and .gitlab-ci.yml.
#
# It builds the IMAGE this project ships and probes it FROM OUTSIDE, which is
# the deployed shape — and the only shape in which this template's own smoke
# test can be honest:
#
#   * the status endpoint is restricted to the local machine. Probed from
#     INSIDE the container it answers 200 and the check written as "this must
#     NOT be public" fails against a configuration that is exactly right —
#     measured, for nginx (`allow 127.0.0.1`) and for httpd, whose
#     `Require local` counts the server's OWN addresses too, so not even
#     using the container's eth0 address gets around it;
#   * the published image runs as a non-root user (ADR-0062), so a script
#     running inside it cannot write the throwaway certificate — measured on
#     haproxy.
#
# Needs `docker`, `curl` and `openssl` on the RUNNER. A Delonixfile is
# Dockerfile-compatible (this template uses no Delonix-only instruction), so
# any runner builds it; GitLab needs the `docker:dind` service, which the
# .gitlab-ci.yml declares.
set -eu
cd "$(dirname "$0")/.."

# The certificate is NOT in git (tls/ is in .gitignore — a private key does not
# belong in a repository). A fresh clone has none, and that is the honest
# starting point: `scripts/tls.sh` is what a human runs.
if [ ! -s tls/tls.crt ] || [ ! -s tls/tls.key ]; then
    echo "--- generating a throwaway certificate for this run"
    mkdir -p tls
    openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
        -keyout tls/tls.key -out tls/tls.crt \
        -subj "/CN=localhost" \
        -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" >/dev/null 2>&1
    chmod 600 tls/tls.key
fi
# HAProxy wants certificate and key in ONE file (the manifest mounts ./tls).
    cat tls/tls.crt tls/tls.key > tls/tls.pem
    chmod 600 tls/tls.pem
mkdir -p acme

TAG=__NAME__:ci
NAME=__NAME__-ci-$$

echo "--- build (the Delonixfile's own config check runs inside it)"
docker build -q -f Delonixfile -t "$TAG" . >/dev/null

echo "--- run"
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT
# `--user 0` mirrors `user: "0"` in delonix-manifest.yaml, and for the same
# reason written there: the master reads the private key as root and
# haproxy.cfg's `user haproxy` drops the privilege before any traffic is
# handled. Without it the worker dies on `cannot open the file
# .../tls/tls.pem` — measured.
docker run -d --name "$NAME" --user 0 \
    -p 127.0.0.1:__PORT__:__PORT__ -p 127.0.0.1:__TLS_PORT__:__TLS_PORT__ \
    -v "$PWD/tls:/usr/local/etc/haproxy/tls:ro" \
    "$TAG" >/dev/null

# Wait for the port, never a fixed sleep: too short fails for the wrong reason
# and too long is paid on every run.
i=0
while [ $i -lt 100 ]; do
    curl -s -o /dev/null --max-time 2 "http://127.0.0.1:__PORT__/healthz" && break
    i=$((i + 1)); sleep 0.2
done

echo "--- smoke"
set +e
sh scripts/smoke.sh
rc=$?
set -e
[ $rc -eq 0 ] || docker logs "$NAME" 2>&1 | tail -30
exit $rc
