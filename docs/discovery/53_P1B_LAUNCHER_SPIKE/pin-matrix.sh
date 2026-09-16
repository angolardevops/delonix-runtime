#!/bin/bash
# Pin under kernel.apparmor_restrict_unprivileged_userns=1: base (unshare(1)) vs new
# (in-process unshare), with and without a `userns` profile on the binary path, and
# with the runtime dir under /tmp or under /run.
for bin in /opt/p1b/prof/base /opt/p1b/prof/delonix /opt/p1b/noprof/base /opt/p1b/noprof/delonix; do
  for rt in tmp run; do
    tag=$(echo "$bin" | tr / _)-$rt
    export DELONIX_ROOT=$HOME/m$tag
    if [ "$rt" = tmp ]; then export DELONIX_NET_RUNTIME_DIR=/tmp/m$tag; else export DELONIX_NET_RUNTIME_DIR=/run/user/$(id -u)/m$tag; fi
    rm -rf "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"; mkdir -p "$DELONIX_ROOT"
    s=$(date +%s%N)
    out=$(timeout 40 "$bin" net netns up </dev/null 2>&1 | tail -1)
    rc=$?
    ms=$(( ($(date +%s%N) - s) / 1000000 ))
    printf '%-22s %-4s %6sms  %s\n' "$bin" "$rt" "$ms" "$(echo "$out" | cut -c1-120)"
    timeout 20 "$bin" net netns down </dev/null >/dev/null 2>&1
    pkill -f "[m]$tag" 2>/dev/null
  done
done
