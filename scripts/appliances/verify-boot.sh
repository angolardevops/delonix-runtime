#!/bin/bash
# Boot each built appliance and prove it SERVES something.
#
# "The installer said finished" and "the disk boots" are different claims, and
# only the second is what a published image promises. Each product is probed
# on the port it is supposed to answer on — not merely watched for a login
# prompt, which a half-configured system also reaches.
set -uo pipefail
OUT=${OUT_DIR:-$(pwd)}

# product : filename stem : guest port : path
#
# A STEM and not a fixed filename: the builds name their output with the version
# (`pve-9.2-1.qcow2`), so that two releases of the same product can sit side by
# side — and every version present gets verified, because "the 9.1 image serves"
# says nothing about the 9.2 one. The bare `pve.qcow2` of older builds still
# matches.
CASES=(
  "pve:pve:8006:/"
  "pbs:pbs:8007:/"
  "pmg:pmg:8006:/"
  "pdm:pdm:8443:/"
  "truenas:truenas:80:/"
)

# KVM when the host has it, TCG when it does not (a CI runner may not expose
# /dev/kvm). Without acceleration an install takes far longer but still works,
# and `-cpu host` is meaningless under TCG.
if [ -w /dev/kvm ]; then
  ACCEL=(-enable-kvm -cpu host)
else
  echo "==> /dev/kvm not available: falling back to TCG (slow)"
  ACCEL=(-cpu max)
fi
# With no arguments, check every image that is present; with arguments, check
# exactly those (a CI run builds ONE product, and demanding the other three
# would fail every time).
WANT=("$@")
rc=0
checked=0
for spec in "${CASES[@]}"; do
  IFS=: read -r product stem port path <<< "$spec"
  if [ ${#WANT[@]} -gt 0 ]; then
    case " ${WANT[*]} " in *" $product "*) ;; *) continue ;; esac
  fi
  # Every version of this product that is present. `-bootcheck` is excluded:
  # it is this script's own scratch overlay, and a leftover one would be probed
  # as if it were an image to publish.
  imgs=()
  for cand in "$OUT/$stem".qcow2 "$OUT/$stem"-*.qcow2; do
    case "$cand" in *-bootcheck.qcow2) continue ;; esac
    [ -f "$cand" ] && imgs+=("$cand")
  done
  if [ ${#imgs[@]} -eq 0 ]; then
    # Named explicitly and absent is an error; not named at all just means it
    # was not built in this run.
    [ ${#WANT[@]} -gt 0 ] && { echo "!! $product: no image matching $OUT/$stem*.qcow2"; rc=1; }
    continue
  fi
  for img in "${imgs[@]}"; do
  name=$(basename "$img" .qcow2)
  checked=$((checked + 1))
  hostport=$((19000 + port % 1000))
  log="$OUT/$name-boot.log"
  pidfile="$OUT/$name-boot.pid"
  # An overlay, so probing never writes into the image we are about to publish.
  ovl="$OUT/$name-bootcheck.qcow2"
  rm -f "$ovl" "$log" "$pidfile"
  qemu-img create -f qcow2 -b "$img" -F qcow2 "$ovl" >/dev/null

  mem=4096; [ "$name" = truenas ] && mem=6144
  qemu-system-x86_64 "${ACCEL[@]}" -m $mem -smp 4 \
    -drive file="$ovl",if=virtio,format=qcow2 \
    -netdev "user,id=n0,hostfwd=tcp::$hostport-:$port" -device virtio-net-pci,netdev=n0 \
    -display none -serial "file:$log" -pidfile "$pidfile" &

  ok=0
  for i in $(seq 1 90); do
    sleep 10
    if curl -sk --max-time 5 "https://127.0.0.1:$hostport$path" -o /dev/null 2>/dev/null ||
       curl -s  --max-time 5 "http://127.0.0.1:$hostport$path"  -o /dev/null 2>/dev/null; then
      ok=1; break
    fi
    kill -0 "$(cat "$pidfile" 2>/dev/null)" 2>/dev/null || break   # guest died
  done

  if [ "$ok" = 1 ]; then
    echo "OK   $name — answers on port $port after ~$((i*10))s"
  else
    echo "FAIL $name — nothing on port $port. Last serial:"
    tail -6 "$log" | tr -d '\r'
    rc=1
  fi
  kill "$(cat "$pidfile" 2>/dev/null)" 2>/dev/null
  wait 2>/dev/null
  rm -f "$ovl" "$pidfile"
  done
done
if [ "$checked" = 0 ]; then
  echo "!! nothing to verify — no images found and none named"
  rc=1
fi
echo "==== boot verification rc=$rc ($checked checked)"
exit $rc
