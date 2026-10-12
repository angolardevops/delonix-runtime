#!/usr/bin/env bash
# S7 — the third control the proxy bypasses: the path BETWEEN networks. cl-a2 is
# in be-a's namespace but on netB; the dataplane drops it at the inter-bridge
# rule (S3b) until a `NetworkRoute` netB -> netA opens the pair. The authorisation
# D1 computes therefore needs the route registry too, not only namespace and
# the backend firewall.
set -u
. "$(dirname "$0")/env.sh"
BE=$(ipof be-a)
echo "no route:       cl-a2 -> be-a direct: $(get cl-a2 "http://$BE:8080/")"
"$B" network route netB netA 2>&1 | tail -1
echo "route netB->netA: cl-a2 -> be-a direct: $(get cl-a2 "http://$BE:8080/")"
echo "                  cl-b2 -> be-a direct: $(get cl-b2 "http://$BE:8080/")   (route opens the path, the namespace still decides)"
ls "$DELONIX_ROOT/ingress/routes/"
"$B" network route netB netA --rm 2>&1 | tail -1
echo "route removed:  cl-a2 -> be-a direct: $(get cl-a2 "http://$BE:8080/")"
