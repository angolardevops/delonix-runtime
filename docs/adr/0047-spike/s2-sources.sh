#!/usr/bin/env bash
# S2 — what a listener in the holder netns sees at accept(): the PEER address
# (who asks) and the LOCAL address (which holder address the connection was
# delivered to). The probe is the same accept() the proxy does and discards
# (`let (stream, _peer) = listener.accept()`), in the same netns.
#
# The hole is opened BY HAND and only for the two spike ports, in the shape D2
# proposes (an accept on `iifname @dlxbr` for named TCP ports, before the drop),
# inside the isolated holder. No engine code changes.
set -u
. "$(dirname "$0")/env.sh"
HN nft insert rule ip dlxing dlxinput iifname @dlxbr tcp dport "{ $PORT, $PROBE }" counter accept comment '"adr0047-spike-d2"'
HN nft list chain ip dlxing dlxinput | sed -n '/chain/,/}/p'
LOG="$DELONIX_ROOT/probe.log"; : >"$LOG"
HN setsid python3 "$P/peer_probe.py" "$PROBE" "$LOG" </dev/null >/dev/null 2>&1 &
sleep 1
# The host path: what the ingress slirp delivers for a published port. Two host
# addresses: loopback (the default publish address) and a non-loopback one
# (virbr0's, not reachable from the LAN).
for spec in "127.0.0.1 $PROBE" "192.168.122.1 18473"; do
  set -- $spec
  printf '{"execute":"add_hostfwd","arguments":{"proto":"tcp","host_addr":"%s","host_port":%s,"guest_addr":"10.0.2.100","guest_port":%s}}' "$1" "$2" "$PROBE" \
    | socat - UNIX-CONNECT:"$DELONIX_NET_RUNTIME_DIR/slirp.sock"; echo
done
echo "## host -> 127.0.0.1:$PROBE";   curl -s -m 4 "http://127.0.0.1:$PROBE/"
echo "## host -> 192.168.122.1:18473"; curl -s -m 4 "http://192.168.122.1:18473/"
A=10.251.0.1; Bgw=10.230.0.1
for c in cl-a cl-b po-c0; do echo "## $c ($(ipof $c)) -> netA gw $A";  get $c "http://$A:$PROBE/"; done
for c in cl-a2 cl-b2;      do echo "## $c ($(ipof $c)) -> netB gw $Bgw"; get $c "http://$Bgw:$PROBE/"; done
echo "## cl-a2 (netB) -> netA gw $A (another bridge's holder address)"; get cl-a2 "http://$A:$PROBE/"
echo "## cl-m (primary $(ipof cl-m) on netB, extra 10.251.x on netA) -> netB gw"; get cl-m "http://$Bgw:$PROBE/"
echo "## cl-m -> netA gw (the extra network's gateway)"; get cl-m "http://$A:$PROBE/"
echo "## dlxinput counters"
HN nft list chain ip dlxing dlxinput | grep counter
echo "## probe log"; cat "$LOG"
