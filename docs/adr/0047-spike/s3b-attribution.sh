#!/usr/bin/env bash
# S3b — which rule drops each DIRECT attempt of S3 case 1: one client at a time,
# counters reset in between. Through the proxy no forward-hook rule is hit at
# all (the proxy->backend leg is locally generated: output, not forward).
set -u
. "$(dirname "$0")/env.sh"
BE=$(ipof be-a)
CH=$(HN nft list map ip dlxing fwmap | tr ',' '\n' | grep -F "$BE " | sed 's/.*jump //; s/[ }].*//')
show() { HN nft list table ip dlxing | grep -E 'counter packets [1-9]' | grep -v -E 'masquerade|elements' | sed 's/^[[:space:]]*/    /'; }
for c in cl-b cl-a2 cl-b2 cl-m; do
  rst
  echo "## $c -> be-a direct: $(get $c "http://$BE:8080/")"; show
  rst
  echo "## $c -> proxy:       $(get $c "http://$(gwof $c):$PORT/" app.test)"; show
done
