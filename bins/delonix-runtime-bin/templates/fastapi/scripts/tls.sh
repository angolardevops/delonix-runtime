#!/bin/sh
# Local development certificate for HTTPS, into ./tls — the same files
# `delonix init` writes, regenerated here when they are missing or when you
# want new ones. NEVER a production certificate: these cover localhost only and
# are trusted by nothing outside this machine (mkcert) or nobody at all
# (self-signed fallback).
#
#   sh scripts/tls.sh                 # keep an existing pair; create one if absent
#   sh scripts/tls.sh force           # replace the pair
#   sh scripts/tls.sh host app.test   # add an extra name (repeatable)
#
# The app reads tls/tls.crt and tls/tls.key (see .env.example, TLS_CERT_FILE).
set -eu

dir="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
tls="$dir/tls"
crt="$tls/tls.crt"
key="$tls/tls.key"
pem="$tls/tls.pem"

force=0
hosts="localhost 127.0.0.1 ::1"
while [ $# -gt 0 ]; do
	case "$1" in
	force) force=1 ;;
	host) shift; hosts="$hosts ${1:?usage: host <name>}" ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
	shift
done

mkdir -p "$tls"
if [ "$force" -eq 0 ] && [ -f "$crt" ] && [ -f "$key" ]; then
	echo "tls/: keeping the existing certificate (pass 'force' to replace it)"
else
	# shellcheck disable=SC2086 — $hosts is a deliberate word list.
	if command -v mkcert >/dev/null 2>&1 && mkcert -cert-file "$crt" -key-file "$key" -- $hosts; then
		echo "tls/: mkcert certificate for: $hosts"
		command -v mkcert >/dev/null 2>&1 && mkcert -install >/dev/null 2>&1 || true
	else
		san=""
		for h in $hosts; do
			case "$h" in
			*[!0-9.]*) san="$san,DNS:$h" ;;
			*) san="$san,IP:$h" ;;
			esac
		done
		san="${san#,}"
		openssl req -x509 -newkey rsa:2048 -nodes -days 825 \
			-subj "/CN=localhost" -addext "subjectAltName=$san" \
			-keyout "$key" -out "$crt" >/dev/null 2>&1
		echo "tls/: self-signed certificate for: $hosts (no mkcert — browsers will warn)"
	fi
fi

# A single file for servers that want both (not used by the app, handy to have).
cat "$crt" "$key" > "$pem"
chmod 600 "$key" "$pem"
echo "tls/: tls.crt, tls.key, tls.pem"
