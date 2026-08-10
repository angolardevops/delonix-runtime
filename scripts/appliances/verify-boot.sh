#!/bin/bash
# Boot each built appliance and prove it SERVES something.
#
# "The installer said finished" and "the disk boots" are different claims, and
# only the second is what a published image promises. Each product is probed
# on the port it is supposed to answer on — not merely watched for a login
# prompt, which a half-configured system also reaches.
set -uo pipefail
OUT=${OUT_DIR:-$(pwd)}

# product : image : guest port : path
CASES=(
  "pbs:$OUT/pbs.qcow2:8007:/"
  "pmg:$OUT/pmg.qcow2:8006:/"
  "pdm:$OUT/pdm.qcow2:8443:/"
  "truenas:$OUT/truenas.qcow2:80:/"
)

rc=0
for spec in "${CASES[@]}"; do
  IFS=: read -r name img port path <<< "$spec"
  [ -f "$img" ] || { echo "!! $name: no image at $img"; rc=1; continue; }
  hostport=$((19000 + port % 1000))
  log="$OUT/$name-boot.log"
  pidfile="$OUT/$name-boot.pid"
  # An overlay, so probing never writes into the image we are about to publish.
  ovl="$OUT/$name-bootcheck.qcow2"
  rm -f "$ovl" "$log" "$pidfile"
  qemu-img create -f qcow2 -b "$img" -F qcow2 "$ovl" >/dev/null

  mem=4096; [ "$name" = truenas ] && mem=6144
  qemu-system-x86_64 -enable-kvm -m $mem -smp 4 -cpu host \
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
echo "==== boot verification rc=$rc"
exit $rc
