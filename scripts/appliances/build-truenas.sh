#!/bin/bash
# Build a bootable TrueNAS SCALE appliance image from the installation ISO.
#
# TrueNAS ships no answer-file mechanism, so this drives the installer's own
# JSON-RPC API instead of screen-scraping its TUI. That is possible without
# touching the ISO at all: the stock `truenas-installer.service` inside the
# live image is literally `python3 -m truenas_installer --server`, which
# listens on :8080. We just forward the port and call `install`.
#
#   build-truenas.sh                  # the pinned version below, fetched+verified
#   build-truenas.sh 25.10.5          # another version, fetched+verified
#   build-truenas.sh /path/to.iso     # an ISO you already have
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
CACHE=${MEDIA_CACHE:-$HERE/.media}
DEFAULT_VER=${DEFAULT_VER:-25.10.5}

# The download path carries the RELEASE TRAIN's codename, and it is not
# derivable from the version number — so it is a table, and an unknown train
# stops the build instead of guessing a URL that would 404 halfway through.
# Each of these was confirmed to resolve before being written down.
train_of() {
  case "${1%%.*}.${1#*.}" in
    25.10*) echo Goldeye ;;
    25.04*) echo Fangtooth ;;
    24.10*) echo ElectricEel ;;
    *) return 1 ;;
  esac
}

ARG=${1:-$DEFAULT_VER}
if [ -f "$ARG" ]; then
  SRC_ISO=$ARG
  echo "############ truenas-scale (local ISO: $SRC_ISO)"
else
  VER=$ARG
  TRAIN=${TRUENAS_TRAIN:-$(train_of "$VER" || true)}
  if [ -z "$TRAIN" ]; then
    echo "!! unknown release train for TrueNAS SCALE $VER" >&2
    echo "   known: 25.10.x=Goldeye, 25.04.x=Fangtooth, 24.10.x=ElectricEel" >&2
    echo "   pass it explicitly: TRUENAS_TRAIN=<Name> build-truenas.sh $VER" >&2
    exit 1
  fi
  BASE=${TRUENAS_MIRROR:-https://download.sys.truenas.net}/TrueNAS-SCALE-$TRAIN/$VER
  SRC_ISO="$CACHE/TrueNAS-SCALE-$VER.iso"
  echo "############ truenas-scale $VER ($TRAIN)"
  # Unlike Proxmox's directory-wide SHA256SUMS, TrueNAS publishes a sidecar
  # holding the bare hash and nothing else — no filename to match on.
  WANT=$(curl -fsSL --retry 3 "$BASE/TrueNAS-SCALE-$VER.iso.sha256" | tr -d ' \n')
  [ -n "$WANT" ] || { echo "!! no checksum published for TrueNAS SCALE $VER" >&2; exit 1; }
  "$HERE/fetch-media.sh" "$BASE/TrueNAS-SCALE-$VER.iso" "$WANT" "$SRC_ISO"
fi
# The RPC client needs a WebSocket library. Debian/Ubuntu ship python3 as an
# externally-managed environment (PEP 668), so a throwaway venv is the way to
# get one without touching the system interpreter.
VENV=${VENV:-$HERE/.venv}
if [ ! -x "$VENV/bin/python" ]; then
  python3 -m venv "$VENV"
  "$VENV/bin/pip" install --quiet websockets
fi
DISK_GB=${DISK_GB:-20}
MEM=${MEM:-6144}
PORT=${PORT:-18080}
PASSWORD=${PASSWORD:-delonix-admin}

if [ -z "${VER:-}" ]; then
  VER=$(basename "$SRC_ISO" | sed -n 's/^TrueNAS-SCALE-\(.*\)\.iso$/\1/p')
fi
# The version belongs in the output name: without it, building a new release
# silently overwrites the image of the one already sitting there.
SLUG="truenas${VER:+-$VER}"
RAW="$OUT/$SLUG.raw.qcow2"
FINAL="$OUT/$SLUG.qcow2"
LOG="$OUT/$SLUG-install.log"
PIDFILE="$OUT/$SLUG-qemu.pid"

# KVM when the host has it, TCG when it does not (a CI runner may not expose
# /dev/kvm). Without acceleration an install takes far longer but still works,
# and `-cpu host` is meaningless under TCG.
if [ -w /dev/kvm ]; then
  ACCEL=(-enable-kvm -cpu host)
else
  echo "==> /dev/kvm not available: falling back to TCG (slow)"
  ACCEL=(-cpu max)
fi
rm -f "$RAW" "$LOG" "$PIDFILE"
qemu-img create -f qcow2 "$RAW" "${DISK_GB}G" >/dev/null

echo "==> booting installer ISO (RPC forwarded to 127.0.0.1:$PORT)"
qemu-system-x86_64 \
  "${ACCEL[@]}" -m "$MEM" -smp 4 \
  -drive file="$RAW",if=virtio,format=qcow2,cache=unsafe \
  -cdrom "$SRC_ISO" -boot d \
  -netdev "user,id=n0,hostfwd=tcp::$PORT-:8080" -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$LOG" -no-reboot -pidfile "$PIDFILE" &
QEMU_WRAP=$!

cleanup() {
  if [ -f "$PIDFILE" ]; then kill "$(cat "$PIDFILE")" 2>/dev/null || true; fi
}
trap cleanup EXIT

# No TCP pre-probe here: QEMU's hostfwd accepts a connection whether or not
# anything listens inside the guest, so /dev/tcp reports the port open from
# the first second of boot. The Python client retries the real WebSocket
# handshake instead, which is the only thing that proves the installer is up.
echo "==> driving the install (waits for the installer to come up)"
"$VENV/bin/python" "$HERE/truenas_install.py" "ws://127.0.0.1:$PORT/ws" vda "$PASSWORD"

echo "==> waiting for the guest to power off"
for i in $(seq 1 60); do
  kill -0 "$(cat "$PIDFILE" 2>/dev/null || echo 0)" 2>/dev/null || break
  sleep 5
done
cleanup
trap - EXIT
wait "$QEMU_WRAP" 2>/dev/null || true

echo "==> compressing"
rm -f "$FINAL"
qemu-img convert -O qcow2 -c -o compression_type=zstd "$RAW" "$FINAL"
rm -f "$RAW"
ls -lh "$FINAL"
qemu-img info "$FINAL" | grep -E "virtual size|disk size|compression"

# Close the loop: `--appliance` is what stops `vm create` from generating a
# cloud-init seed this guest cannot read.
echo
echo "Register it with:"
echo "  delonix image vm import $FINAL -t truenas-scale:${VER%.*} --appliance \\"
echo "      --distro truenas --release ${VER:-unknown} --default-vcpus 2 --default-memory 8G"
