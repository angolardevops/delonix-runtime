#!/usr/bin/env bash
# S6 — can a container be mistaken for the host? ADR-0047 D1 keeps the host's
# behaviour for «sources that are NOT a known workload». S2 showed the host
# arrives with the slirp gateway (10.0.2.2) OR its own real address as peer,
# always on local 10.0.2.100 (tap0); a container always arrives with its own
# SDN address. Two questions: (1) does a container that dials 10.0.2.100 look
# like the host by local address? (2) does a guard on the destination address
# in the D2 rule close that?
set -u
. "$(dirname "$0")/env.sh"
: >"$DELONIX_ROOT/probe.log"
echo "## cl-b dials the tap0 address 10.0.2.100:$PROBE (D2 rule as in S2: iifname @dlxbr, any daddr)"
get cl-b "http://10.0.2.100:$PROBE/"
echo "## the D2 rule with a destination guard: only the bridge-side addresses"
H=$(HN nft -a list chain ip dlxing dlxinput | grep 'adr0047-spike-d2' | sed 's/.*# handle //')
HN nft replace rule ip dlxing dlxinput handle "$H" iifname @dlxbr ip daddr != 10.0.2.100 tcp dport "{ $PORT, $PROBE }" counter accept comment '"adr0047-spike-d2"'
HN nft list chain ip dlxing dlxinput | grep -E 'adr0047|drop'
rst
echo "cl-b -> 10.0.2.100:$PROBE: $(get cl-b "http://10.0.2.100:$PROBE/")"
echo "cl-b -> 10.251.0.1:$PROBE:  $(get cl-b "http://10.251.0.1:$PROBE/")"
echo "host -> 127.0.0.1:$PROBE:   $(curl -s -m 4 "http://127.0.0.1:$PROBE/")"
HN nft list chain ip dlxing dlxinput | grep -E 'adr0047|drop'
echo "## probe log"; cat "$DELONIX_ROOT/probe.log"
