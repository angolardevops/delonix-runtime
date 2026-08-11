#!/bin/bash
# Build a bootable Proxmox appliance image from an installation ISO.
#
#   build-proxmox.sh <product> <src.iso>
#
# Runs the vendor's own automated installer unattended against an empty disk
# in QEMU/KVM, then flattens and compresses the result. Nothing about the
# guest is hand-rolled: the ISO installs itself exactly as it would on metal.
set -euo pipefail

PRODUCT=$1
SRC_ISO=$2
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
DISK_GB=${DISK_GB:-20}
MEM=${MEM:-4096}

RAW="$OUT/$PRODUCT.raw.qcow2"
FINAL="$OUT/$PRODUCT.qcow2"
LOG="$OUT/$PRODUCT-install.log"
ISO="$HERE/$PRODUCT-auto.iso"
ANSWER=${ANSWER:-$HERE/answer-$PRODUCT.toml}

echo "############ $PRODUCT"
"$HERE/mkiso.sh" "$SRC_ISO" "$ANSWER" "$ISO" "$HERE/w-$PRODUCT"

# KVM when the host has it, TCG when it does not (a CI runner may not expose
# /dev/kvm). Without acceleration an install takes far longer but still works,
# and `-cpu host` is meaningless under TCG.
if [ -w /dev/kvm ]; then
  ACCEL=(-enable-kvm -cpu host)
else
  echo "==> /dev/kvm not available: falling back to TCG (slow)"
  ACCEL=(-cpu max)
fi
rm -f "$RAW" "$LOG"
qemu-img create -f qcow2 "$RAW" "${DISK_GB}G" >/dev/null

echo "==> installing (headless, serial log: $LOG)"
# -no-reboot: the installer reboots when it is done, which ends the run with
# the disk quiesced — exactly the moment to capture it.
# cache=unsafe: this is a throwaway build, and it roughly halves install time.
timeout "${INSTALL_TIMEOUT:-7200}" qemu-system-x86_64 \
  "${ACCEL[@]}" -m "$MEM" -smp 4 \
  -drive file="$RAW",if=virtio,format=qcow2,cache=unsafe \
  -cdrom "$ISO" -boot d \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$LOG" -no-reboot

# The exit status of QEMU says nothing about the install — it is 0 whether the
# installer finished or dropped to a rescue shell. Read the installer's own
# words instead.
if ! grep -aq "installation finished" "$LOG"; then
  echo "!! install did NOT finish. Last lines:"
  tail -20 "$LOG" | tr -d '\r'
  exit 1
fi
# Only the INSTALLER's own errors count. A plain /ERROR:/ also matches
# `modprobe: ERROR: could not insert 'amd_atl'` — the kernel shrugging at
# hardware this VM does not have — and failed three good builds that way.
if grep -aE "ERROR: Installation failed|Auto-installation failed|unable to continue" "$LOG"; then
  echo "!! installer reported errors"; exit 1
fi
echo "==> installer reported success"

# --------------------------------------------------------------------------
# Post-install: make the image bootable somewhere OTHER than this build.
#
# The installer writes down the address it had HERE as static, and names the
# NIC it saw HERE — neither is true in the next machine. Found by booting a
# published image and reading its own console, which announced
# `https://10.0.2.15:8006/`: the QEMU slirp address, on a libvirt network that
# has no such subnet. See the spike section of docs/adr/0008.
# --------------------------------------------------------------------------
ROOT_PW=${ROOT_PW:-$(grep -oP 'root-password\s*=\s*"\K[^"]+' "$ANSWER" 2>/dev/null || echo delonix-admin)}
SSH_PORT=${SSH_PORT:-$((22000 + RANDOM % 1000))}
echo "==> post-install: DHCP, eth0, serial console (ssh on :$SSH_PORT)"
qemu-system-x86_64 "${ACCEL[@]}" -m "$MEM" -smp 4 \
  -drive file="$RAW",if=virtio,format=qcow2,cache=unsafe \
  -netdev "user,id=n0,hostfwd=tcp::$SSH_PORT-:22" -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$OUT/$PRODUCT-postinstall.log" &
QEMU_PID=$!
# The port accepting is NOT sshd answering: the QEMU hostfwd accepts a
# connection whether or not anything listens inside. Wait for the banner.
for _ in $(seq 1 120); do
  if timeout 3 bash -c "exec 3<>/dev/tcp/127.0.0.1/$SSH_PORT && head -c 3 <&3" 2>/dev/null | grep -q SSH; then break; fi
  sleep 5
done
if ! python3 "$HERE/proxmox_postinstall.py" "$SSH_PORT" "$ROOT_PW"; then
  echo "!! post-install failed — the image would boot with a static IP from this build"
  kill $QEMU_PID 2>/dev/null || true
  exit 1
fi
wait $QEMU_PID 2>/dev/null || true

echo "==> compressing"
rm -f "$FINAL"
qemu-img convert -O qcow2 -c -o compression_type=zstd "$RAW" "$FINAL"
rm -f "$RAW"
ls -lh "$FINAL"
qemu-img info "$FINAL" | grep -E "virtual size|disk size|compression"
