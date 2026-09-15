#!/bin/sh
case "$*" in
  "insert rule ip dlxing fwdeny iifname "*" ip saddr != "*" drop") echo "shim: injected failure" >&2; exit 1;;
esac
exec /usr/sbin/nft "$@"
