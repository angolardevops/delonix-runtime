#!/bin/sh
# The TLS certificate of __NAME__. Everything lands in ./tls, which the
# container mounts read-only; nothing here ever enters the image.
#
#   sh scripts/tls.sh [name...]                   a local certificate (default: localhost 127.0.0.1 ::1)
#   sh scripts/tls.sh install <fullchain> <key>   use a certificate you already have
#   sh scripts/tls.sh letsencrypt <domain> <email>
#       a publicly trusted certificate from Let's Encrypt (HTTP-01). Needs
#       certbot on this host, <domain> pointing at it, and port 80 of the
#       host reaching this container: publish "0.0.0.0:80:__PORT__" in
#       delonix-manifest.yaml first. Running it accepts the Let's Encrypt
#       subscriber agreement on your behalf (--agree-tos).
#       <email> may be `-` to register without one (no expiry notices).
#       LETSENCRYPT_STAGING=1 uses Let's Encrypt's staging environment: the
#       whole path is exercised, nothing counts against the domain's rate
#       limits, and the certificate is NOT trusted by browsers.
#       ACME_SERVER=<directory-url> uses another ACME CA instead (an
#       internal step-ca, ZeroSSL, a Pebble test server).
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
    if delonix container exec __NAME__ httpd -k graceful >/dev/null 2>&1; then
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
letsencrypt)
    [ $# -eq 3 ] || { echo "usage: sh scripts/tls.sh letsencrypt <domain> <email>" >&2; exit 2; }
    command -v certbot >/dev/null 2>&1 || { echo "certbot is not installed (apt install certbot / dnf install certbot)" >&2; exit 1; }
    mkdir -p acme letsencrypt
    # Everything certbot keeps stays inside the project: no root, no /etc.
    if [ "$3" = "-" ]; then contact="--register-unsafely-without-email"; else contact="-m $3"; fi
    staging=""
    [ "${LETSENCRYPT_STAGING:-0}" = 1 ] && staging="--test-cert"
    [ -n "${ACME_SERVER:-}" ] && staging="--server $ACME_SERVER"
    # $contact and $staging are split on purpose: each is zero or more options.
    # shellcheck disable=SC2086
    certbot certonly --webroot -w acme -d "$2" $contact $staging --agree-tos --non-interactive \
        --config-dir letsencrypt --work-dir letsencrypt/work --logs-dir letsencrypt/logs
    cp "letsencrypt/live/$2/fullchain.pem" tls/tls.crt
    cp "letsencrypt/live/$2/privkey.pem" tls/tls.key
    echo "renew (cron, twice a day):  certbot renew --config-dir $PWD/letsencrypt --work-dir $PWD/letsencrypt/work --logs-dir $PWD/letsencrypt/logs --deploy-hook 'sh $PWD/scripts/tls.sh install $PWD/letsencrypt/live/$2/fullchain.pem $PWD/letsencrypt/live/$2/privkey.pem'"
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
