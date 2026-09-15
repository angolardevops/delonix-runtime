#!/bin/bash
# usage: g008.sh <label> <shim:0|1>
SP=/tmp/claude-1000/-home-walter-workspace-ngolacloud-delonix-runtime/796b5e1f-8974-461e-ae99-8ea71e6be20a/scratchpad
B=$SP/target/debug/delonix
export DELONIX_ROOT=/tmp/dxa-$1 DELONIX_NET_RUNTIME_DIR=/tmp/dxa-$1-n
[ "$2" = 1 ] && export PATH=$SP/shim:$PATH
mkdir -p $DELONIX_ROOT $DELONIX_NET_RUNTIME_DIR
echo "== $1 shim=$2 nft=$(command -v nft)"
$B network create audnet >/dev/null 2>&1; echo "network create rc=$?"
timeout 120 $B container run -d --name c1 --net audnet alpine sleep 300 >/dev/null 2>$SP/raw/g008-$1-run.err; echo "run rc=$?"
IP=$($B container inspect c1 2>/dev/null | grep -m1 '"ip"' | tr -dc '0-9.'); echo "container ip=$IP"
HP=$(cat $DELONIX_ROOT/ingress/holder.pid)
nsenter -t $HP -U -m -n --preserve-credentials /usr/sbin/nft list chain ip dlxing fwdeny 2>&1 | grep -E 'saddr != ' || echo "NO ANTISPOOF RULE IN fwdeny"
tail -2 $SP/raw/g008-$1-run.err
