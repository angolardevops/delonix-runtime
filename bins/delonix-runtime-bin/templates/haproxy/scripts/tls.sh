#!/bin/sh
# The TLS certificate of __NAME__. Everything lands in ./tls, which the
# container mounts read-only; nothing here ever enters the image.
#
#   sh scripts/tls.sh [name...]                   a local certificate (default: localhost 127.0.0.1 ::1)
#   sh scripts/tls.sh install <fullchain> <key>   use a certificate you already have
#
# Let's Encrypt: HAProxy does not serve files, so the HTTP-01 webroot challenge
# is not wired here. Get the certificate with a DNS-01 client (certbot with
# your DNS provider's plugin, acme.sh, lego) and hand it over with `install`.
#
# A local certificate comes from mkcert when it is installed (trusted by this
# machine's browsers after `mkcert -install`), otherwise from openssl
# (self-signed: HTTPS works, browsers warn).
set -eu
cd "$(dirname "$0")/.."
mkdir -p tls
umask 077

bundle() { # tls.pem = certificate + key, for servers that want a single file
    cat tls/tls.crt tls/tls.key > tls/tls.pem
    chmod 600 tls/tls.key tls/tls.pem
    chmod 644 tls/tls.crt
}
reload() {
    if delonix container kill -s USR2 __NAME__ >/dev/null 2>&1; then
        echo "reloaded __NAME__ with the new certificate"
    else
        echo "__NAME__ is not running — it picks the certificate up when it starts"
    fi
}

case "${1:-}" in
install)
    [ $# -eq 3 ] || { echo "usage: sh scripts/tls.sh install <fullchain.pem> <privkey.pem>" >&2; exit 2; }
    cp "$2" tls/tls.crt
    cp "$3" tls/tls.key
    ;;
*)
    [ $# -gt 0 ] || set -- localhost 127.0.0.1 ::1
    if command -v mkcert >/dev/null 2>&1; then
        mkcert -cert-file tls/tls.crt -key-file tls/tls.key -- "$@" 2>/dev/null
        echo "certificate from this machine's mkcert CA for: $*  (run \`mkcert -install\` once so browsers trust it)"
    elif command -v openssl >/dev/null 2>&1; then
        san=""
        for n in "$@"; do
            case "$n" in
                *:*|*[!0-9.]*) case "$n" in *:*) kind=IP ;; *) kind=DNS ;; esac ;;
                *) kind=IP ;;
            esac
            san="${san:+$san,}$kind:$n"
        done
        openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
            -days 397 -subj "/CN=$1" -addext "subjectAltName=$san" \
            -keyout tls/tls.key -out tls/tls.crt 2>/dev/null
        echo "self-signed certificate for: $*  (install mkcert for one browsers trust)"
    else
        echo "neither mkcert nor openssl is installed — install one of them" >&2
        exit 1
    fi
    ;;
esac
bundle
reload
