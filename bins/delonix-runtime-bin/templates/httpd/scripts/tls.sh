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
#   sh scripts/tls.sh letsencrypt-dns <domain> <email>
#       the same certificate by DNS-01: nothing has to reach this host, so it
#       works behind NAT or CGNAT, and <domain> may be a wildcard (*.example.com).
#       The script prints one TXT record (also written to
#       letsencrypt/dns-challenge.txt), you create it at your DNS provider,
#       and it carries on by itself once the zone's own name servers answer
#       with it (DNS_WAIT seconds at most, default 1800). Needs certbot and
#       one of dig, host or nslookup.
#       DNS_AUTH_HOOK=<command> creates the record instead of you (certbot
#       passes CERTBOT_DOMAIN and CERTBOT_VALIDATION; DNS_CLEANUP_HOOK removes
#       it afterwards) — with it, renewal needs nobody.
#       <email>, LETSENCRYPT_STAGING and ACME_SERVER mean what they mean above.
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

certbot_dirs="--config-dir letsencrypt --work-dir letsencrypt/work --logs-dir letsencrypt/logs"
acme_server() { # the ACME directory this run talks to
    if [ -n "${ACME_SERVER:-}" ]; then echo "$ACME_SERVER"
    elif [ "${LETSENCRYPT_STAGING:-0}" = 1 ]; then echo "https://acme-staging-v02.api.letsencrypt.org/directory"
    else echo "https://acme-v02.api.letsencrypt.org/directory"; fi
}
acme_options() { # acme_options <email> <domain>: the contact and the CA, as certbot options
    if [ "$1" = "-" ]; then printf '%s' "--register-unsafely-without-email"; else printf '%s' "-m $1"; fi
    printf ' %s' "--server $(acme_server)"
    # A certificate already issued for this name by ANOTHER CA (staging first,
    # then production) is replaced; certbot alone would call it "not yet due
    # for renewal" and keep the old one.
    conf="letsencrypt/renewal/${2#\*.}.conf"
    if [ -f "$conf" ] && ! grep -qxF "server = $(acme_server)" "$conf"; then printf ' %s' "--force-renewal"; fi
}
say() { # to the terminal when there is one: certbot keeps a hook's output to itself
    if (: >/dev/tty) 2>/dev/null; then echo "$*" >/dev/tty; else echo "$*" >&2; fi
}
dns_tool() {
    for t in dig host nslookup; do command -v "$t" >/dev/null 2>&1 && { echo "$t"; return 0; }; done
    return 1
}
dns_ns() { # dns_ns <name>: the name servers of the zone that holds <name>
    zone=$1
    while :; do
        case "$(dns_tool)" in
            dig) ns=$(dig +short NS "$zone" 2>/dev/null | grep -v '^;' || true) ;;
            host) ns=$(host -t NS "$zone" 2>/dev/null | sed -n 's/.* name server //p') ;;
            *) ns=$(nslookup -type=NS "$zone" 2>/dev/null | sed -n 's/.*nameserver = //p') ;;
        esac
        [ -n "$ns" ] && { echo "$ns"; return 0; }
        case "$zone" in *.*) zone=${zone#*.} ;; *) return 1 ;; esac
    done
}
dns_txt() { # dns_txt <name> <server>: the TXT values <server> gives for <name>
    case "$(dns_tool)" in
        dig) dig +short +norecurse TXT "$1" "@$2" 2>/dev/null | tr -d '"' ;;
        host) host -t TXT "$1" "$2" 2>/dev/null | sed -n 's/.*descriptive text "\(.*\)"$/\1/p' ;;
        *) nslookup -type=TXT "$1" "$2" 2>/dev/null | sed -n 's/.*text = "\(.*\)"$/\1/p' ;;
    esac
}
dns_wait() { # the manual DNS-01 step: show the record, then wait for the zone to serve it
    record="_acme-challenge.$CERTBOT_DOMAIN"
    mkdir -p letsencrypt
    printf 'name:  %s\ntype:  TXT\nvalue: %s\n' "$record" "$CERTBOT_VALIDATION" > letsencrypt/dns-challenge.txt
    say ""
    say "Create this record at the DNS provider of $CERTBOT_DOMAIN:"
    say "  name:  $record"
    say "  type:  TXT"
    say "  value: $CERTBOT_VALIDATION"
    servers=$(dns_ns "$record") || { say "no name server found for $record"; return 1; }
    say "waiting until $(echo $servers | tr '\n' ' ')answer with it (up to ${DNS_WAIT:-1800} s)..."
    waited=0
    while :; do
        missing=""
        for s in $servers; do
            dns_txt "$record" "$s" | grep -qxF "$CERTBOT_VALIDATION" || missing="$missing $s"
        done
        [ -z "$missing" ] && break
        [ "$waited" -ge "${DNS_WAIT:-1800}" ] && { say "gave up after $waited s: not served by$missing"; return 1; }
        sleep 10
        waited=$((waited + 10))
    done
    say "the record is served by every name server after $waited s; asking the CA to check it"
    sleep 5
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
    # $(acme_options) and $certbot_dirs are split on purpose: each is several options.
    # shellcheck disable=SC2046,SC2086
    certbot certonly --webroot -w acme -d "$2" $(acme_options "$3" "$2") --agree-tos --non-interactive $certbot_dirs
    cp "letsencrypt/live/$2/fullchain.pem" tls/tls.crt
    cp "letsencrypt/live/$2/privkey.pem" tls/tls.key
    echo "renew (cron, twice a day):  certbot renew --config-dir $PWD/letsencrypt --work-dir $PWD/letsencrypt/work --logs-dir $PWD/letsencrypt/logs --deploy-hook 'sh $PWD/scripts/tls.sh install $PWD/letsencrypt/live/$2/fullchain.pem $PWD/letsencrypt/live/$2/privkey.pem'"
    ;;
letsencrypt-dns)
    [ $# -eq 3 ] || { echo "usage: sh scripts/tls.sh letsencrypt-dns <domain> <email>" >&2; exit 2; }
    command -v certbot >/dev/null 2>&1 || { echo "certbot is not installed (apt install certbot / dnf install certbot)" >&2; exit 1; }
    mkdir -p letsencrypt
    live="letsencrypt/live/${2#\*.}"
    renew="certbot renew --config-dir $PWD/letsencrypt --work-dir $PWD/letsencrypt/work --logs-dir $PWD/letsencrypt/logs --deploy-hook 'sh $PWD/scripts/tls.sh install $PWD/$live/fullchain.pem $PWD/$live/privkey.pem'"
    if [ -n "${DNS_AUTH_HOOK:-}" ]; then
        # $(acme_options) and $certbot_dirs are split on purpose: each is several options.
        # shellcheck disable=SC2046,SC2086
        certbot certonly --manual --preferred-challenges dns -d "$2" $(acme_options "$3" "$2") \
            --manual-auth-hook "$DNS_AUTH_HOOK" ${DNS_CLEANUP_HOOK:+--manual-cleanup-hook "$DNS_CLEANUP_HOOK"} \
            --agree-tos --non-interactive $certbot_dirs
        after="renew (cron, twice a day):  $renew"
    else
        dns_tool >/dev/null || { echo "none of dig, host or nslookup is installed (apt install dnsutils / dnf install bind-utils)" >&2; exit 1; }
        # shellcheck disable=SC2046,SC2086
        certbot certonly --manual --preferred-challenges dns -d "$2" $(acme_options "$3" "$2") \
            --manual-auth-hook "sh '$PWD/scripts/tls.sh' __dns-wait" \
            --agree-tos --non-interactive $certbot_dirs
        rm -f letsencrypt/dns-challenge.txt
        after="the TXT record can be deleted now. To renew, run this command again before the certificate expires: each time the CA asks for a new record (set DNS_AUTH_HOOK to have it created for you)"
    fi
    cp "$live/fullchain.pem" tls/tls.crt
    cp "$live/privkey.pem" tls/tls.key
    echo "$after"
    ;;
__dns-wait)
    dns_wait
    exit $?
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
