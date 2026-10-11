#!/usr/bin/env bash
# S9 — tear the bench down. Lists what it removes first, only ever inside the
# isolated root env.sh pins (it refuses anything else).
set -u
. "$(dirname "$0")/env.sh"
echo "## containers in $DELONIX_ROOT"
"$B" container ps -a | awk "NR>1{print \$NF, \$(NF-1)}"
# The S2 probe: matched by its script path AND this bench's root, never by name.
for p in /proc/[0-9]*; do
  { tr "\0" "\n" <"$p/environ"; } 2>/dev/null | grep -qx "DELONIX_ROOT=$DELONIX_ROOT" || continue
  tr '\0' ' ' <"$p/cmdline" 2>/dev/null | grep -q "peer_probe.py $PROBE " && kill "${p#/proc/}" && echo "stopped probe ${p#/proc/}"
done
"$B" net httproute rm 2>&1 | tail -1
for c in be-a cl-a cl-b cl-a2 cl-b2 cl-m cl-x; do "$B" container rm -f "$c" >/dev/null 2>&1 && echo "removed $c"; done
"$B" pod rm -f po 2>/dev/null | tail -1
for n in netA netB; do "$B" network rm "$n" >/dev/null 2>&1 && echo "removed $n"; done
"$B" net netns down 2>&1 | tail -1
echo "## left with DELONIX_ROOT under /tmp/dlx17:"
for p in /proc/[0-9]*; do
  { tr "\0" "\n" <"$p/environ"; } 2>/dev/null | grep -q '^DELONIX_ROOT=/tmp/dlx17' && echo "  ${p#/proc/} $(tr '\0' ' ' <"$p/cmdline" | cut -c1-90)"
done
