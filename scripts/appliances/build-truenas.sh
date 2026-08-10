#!/bin/bash
# Build a bootable TrueNAS SCALE appliance image from the installation ISO.
#
# TrueNAS ships no answer-file mechanism, so this drives the installer's own
# JSON-RPC API instead of screen-scraping its TUI. That is possible without
# touching the ISO at all: the stock `truenas-installer.service` inside the
# live image is literally `python3 -m truenas_installer --server`, which
# listens on :8080. We just forward the port and call `install`.
set -euo pipefail

SRC_ISO=${1:?usage: build-truenas.sh <TrueNAS-SCALE-*.iso>}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
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

RAW="$OUT/truenas.raw.qcow2"
FINAL="$OUT/truenas.qcow2"
LOG="$OUT/truenas-install.log"
PIDFILE="$OUT/truenas-qemu.pid"

echo "############ truenas-scale"
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
