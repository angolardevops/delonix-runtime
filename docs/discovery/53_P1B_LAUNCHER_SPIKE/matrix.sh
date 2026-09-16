#!/bin/bash
# P1b spike: which engine operations need a user namespace, by binary path.
BIN=$1; TAG=$2
export DELONIX_ROOT=$HOME/p1b-$TAG DELONIX_NET_RUNTIME_DIR=/run/user/$(id -u)/p1b-$TAG
mkdir -p $DELONIX_ROOT; rm -rf $DELONIX_NET_RUNTIME_DIR
r(){ local name=$1; shift; out=$("$@" 2>&1); rc=$?; printf '%-34s rc=%-3s %s\n' "$name" "$rc" "$(echo "$out" | grep -iE 'error|eperm|permission|denied|fail' | head -1 | cut -c1-90)"; }
r "image load" $BIN image load -i /tmp/bb.tar
r "container run --net none" $BIN container run --rm --net none busybox:latest true
r "container run -d (supervisor)" $BIN container run -d --name keep --net none busybox:latest sleep 60
r "container exec" $BIN container exec keep true
r "container stop" $BIN container stop -t 0 keep
r "container cp (stopped: __ovlhold)" $BIN container cp keep:/etc/hostname /tmp/p1b-$TAG-hostname
r "container rm (__rmtree)" $BIN container rm -f keep
r "net netns up (pin)" $BIN net netns up
r "net netns down" $BIN net netns down
