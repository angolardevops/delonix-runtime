#!/usr/bin/env bash
# S1 — today's tree, nothing changed: the host reaches the route, a container
# reaches neither the proxy (dlxinput drops it) nor, across namespaces, the
# backend directly.
set -u
. "$(dirname "$0")/env.sh"
BE=$(ipof be-a)
rst
echo "## host -> 127.0.0.1:$PORT (Host: app.test)"
curl -s -m 4 -H 'Host: app.test' "http://127.0.0.1:$PORT/" || echo "curl rc=$?"
for c in cl-a cl-b cl-a2 cl-b2 cl-m po-c0; do
  echo "## $c -> proxy at its own gateway $(gwof $c):$PORT"; get "$c" "http://$(gwof $c):$PORT/" app.test
done
for c in cl-a cl-b cl-a2 cl-b2 po-c0; do
  echo "## $c -> be-a directly ($BE:8080)"; get "$c" "http://$BE:8080/"
done
echo "## dlxinput"
HN nft list chain ip dlxing dlxinput | grep -E 'counter'
