#!/usr/bin/env bash
# S4 — can the source address be mapped back to a workload, by the index D1
# would reuse? The holder DNS already places a client by its source address
# (`dns_client_ns` over `ns_of_ip`), so its answers ARE that mapping, live:
# a client the index cannot place is scoped to `default` and stops seeing the
# names of its own namespace.
#
# Also: is the source address trustworthy? A teamB container WITH NET_ADMIN
# claims cl-a's address and dials the probe (S2) — the bridge anti-spoof
# (`table bridge dlxspoof`) is the only thing between that and impersonation.
set -u
. "$(dirname "$0")/env.sh"
A=10.251.0.1; Bgw=10.230.0.1
q() { "$B" container exec "$1" nslookup "$2" "$3" 2>&1 | grep -E 'Address: |NXDOMAIN|can.t find' | tail -1; }
echo "## cl-b2 (teamB, netB) is $(ipof cl-b2); cl-m is teamB, primary $(ipof cl-m) on netB, extra on netA"
echo "cl-m asks netB gw (source = primary): $(q cl-m cl-b2.teamB.delonix.internal $Bgw)"
echo "cl-m asks netA gw (source = extra):   $(q cl-m cl-b2.teamB.delonix.internal $A)"
echo "po-c0 (teamB pod, $(ipof po-c0)) asks netA gw:  $(q po-c0 cl-b2.teamB.delonix.internal $A)"
echo "cl-a (teamA) asks netA gw (control, must not see teamB): $(q cl-a cl-b2.teamB.delonix.internal $A)"
echo "## the dataplane DOES map the extra address (fwmap):"
HN nft list map ip dlxing fwmap | tr ',' '\n' | grep -E "$(ipof cl-m) |10\.251\.108\.135 " | sed 's/^[[:space:]{]*/    /'
echo "## anti-spoof: cl-x (teamB, netA, NET_ADMIN) sends with cl-a's address $(ipof cl-a)"
"$B" container run -d --name cl-x --namespace teamB --net netA --cap-add NET_ADMIN alpine:3.20 sleep 3600 >/dev/null 2>&1 || true
CLA=$(ipof cl-a)
rst; ( HN nft reset rules table bridge dlxspoof >/dev/null 2>&1 ) 2>/dev/null
: >"$DELONIX_ROOT/probe.log"
# Own address first, so the gateway's neighbour entry is resolved before the
# forged address exists (otherwise the ARP sender is what gets dropped).
echo "cl-x, own address:    $("$B" container exec cl-x sh -c "printf 'GET / HTTP/1.0\r\n\r\n' | nc -w 3 $A $PROBE | tail -1")"
echo "cl-x, forged $CLA:  $("$B" container exec cl-x sh -c "ip addr add $CLA/32 dev eth0 && printf 'GET / HTTP/1.0\r\n\r\n' | nc -w 3 -s $CLA $A $PROBE | tail -1; echo rc=\$?; ip addr del $CLA/32 dev eth0")"
echo "## probe log (what reached the holder):"; cat "$DELONIX_ROOT/probe.log"
echo "## dlxspoof drops:"
HN nft list table bridge dlxspoof | grep -E 'counter packets [1-9]' | sed 's/^[[:space:]]*/    /'
