# Sourced by every step of the ADR-0047 spike. An isolated engine state and an
# isolated network runtime dir, so the spike never touches the node's real
# holder, containers or ports. Refuses to go on with anything else.
: "${B:=$HOME/.cache/delonix-target/s17/debug/delonix}"
export DELONIX_ROOT=${S17_BENCH:-/tmp/dlx17b}/root
export DELONIX_NET_RUNTIME_DIR=${S17_BENCH:-/tmp/dlx17b}/net
case "$DELONIX_ROOT" in /tmp/dlx17*) ;; *) echo "REFUSE: DELONIX_ROOT=$DELONIX_ROOT"; exit 1 ;; esac
[ -n "$DELONIX_NET_RUNTIME_DIR" ] || { echo "REFUSE: DELONIX_NET_RUNTIME_DIR empty"; exit 1; }
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"
P=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PORT=18471    # the HTTPRoute entrypoint (the real proxy)
PROBE=18472   # the peer probe, in the same holder netns
HN() { # run a command inside the isolated holder (user+mount+net namespaces)
  nsenter -t "$(cat "$DELONIX_ROOT/ingress/holder.pid")" -U -m -n --preserve-credentials -- "$@"
}
ipof() { "$B" container inspect "$1" | python3 -c 'import json,sys; d=json.load(sys.stdin); d=d[0] if isinstance(d,list) else d; print(d.get("ip") or "")'; }
gwof() { local ip; ip=$(ipof "$1"); echo "${ip%.*.*}.0.1"; }
get() { # get <container> <url> [Host] — prints the body, or the failure
  "$B" container exec "$1" wget -q -T 4 -O - ${3:+--header "Host: $3"} "$2" 2>&1 | head -2 | tr '\n' ' '; echo
}
# Zero the RULE counters of the engine table. `nft reset counters` only resets
# named counter objects (none here); `nft reset rules` does the rules — and on
# nftables 1.0.9 it segfaults right after resetting (the counters do read 0),
# so the crash is swallowed here.
rst() { ( HN nft reset rules table ip dlxing >/dev/null 2>&1 ) 2>/dev/null; true; }
