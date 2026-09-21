#!/bin/bash
# Build the Wazuh (SIEM / XDR) all-in-one image: manager + indexer + dashboard +
# filebeat on one host, on top of an Ubuntu 24.04 cloud image.
#
#   build-wazuh.sh
#
# Same shape as build-monitoring.sh. What differs is what the image does NOT
# contain: no certificate, no password, no agent-enrolment secret. Wazuh's own
# installer generates a private CA and a set of passwords, and a golden image
# that baked them would hand every clone the same CA and the same admin login --
# for a security product that is the wrong default. The packages and the vendor's
# helper assets are installed here; `wazuh-first-boot` (embedded below) generates
# the secrets on the VM the first time it boots.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
CACHE=${MEDIA_CACHE:-$HERE/.media}

# Pinned. Read off packages.wazuh.com on 2026-09-20, not remembered:
#   * 4.14.7 is the newest non-prerelease 4.x (4.14.8 is an rc, 5.0.0 a beta).
#     wazuh-manager, wazuh-indexer and wazuh-dashboard all publish 4.14.7-1.
#   * filebeat 7.10.2-2 is what the Wazuh repository ships for the indexer link.
#   * The helper assets below are the vendor's own, from packages.wazuh.com/4.14
#     (and the alerts template from the v4.14.7 tag); each is checked by sha256.
WAZUH_VERSION=${WAZUH_VERSION:-4.14.7-1}
FILEBEAT_VERSION=${FILEBEAT_VERSION:-7.10.2-2}
WAZUH_KEY_FPR=${WAZUH_KEY_FPR:-0DCFCA5547B19D2A6099506096B3EE5F29111145}
CERTS_TOOL_SHA256=${CERTS_TOOL_SHA256:-8c93ed36d7b956a6e97a906aed6b6bc636d7cb55a15a41fd6e9c2aa825164216}
PASSWORDS_TOOL_SHA256=${PASSWORDS_TOOL_SHA256:-29ce567ce1bcb4629a34f3ccfcaec7463a5418bcdd0ee96db5e25dbc0340f8eb}
FILEBEAT_YML_SHA256=${FILEBEAT_YML_SHA256:-233c82cdbec2a159ac33a8e5faff4adf7517add92d4f927b33ff4fbd3fcbde37}
ALERTS_TEMPLATE_SHA256=${ALERTS_TEMPLATE_SHA256:-c6e30822c67c10f7e777cb51926e261d8b2c3a941c4ffcf83325f700c1c8802f}
FILEBEAT_MODULE_SHA256=${FILEBEAT_MODULE_SHA256:-7a8c67c47b22f89ab271b7e35f108f18b2215b7aa411cdd23c9994393070d38d}
# Bumped when the image changes without any pinned version changing.
IMAGE_REV=${IMAGE_REV:-1}

BASE_DISTRO=ubuntu
UBUNTU_SERIES=noble
UBUNTU_VER=24.04

# A single-host Zabbix+Grafana stack is far lighter than the OpenStack
# image's 20 GiB container pull: two packages, a schema import, no bulk data.
DISK_GB=${DISK_GB:-30}
MEM=${MEM:-6144}
SMP=${SMP:-4}
BUILD_TIMEOUT=${BUILD_TIMEOUT:-1800}

SLUG="wazuh$WAZUH_VERSION-r$IMAGE_REV"
RAW="$OUT/$SLUG.raw.qcow2"
FINAL="$OUT/$SLUG.qcow2"
LOG="$OUT/$SLUG-build.log"
SEED="$HERE/.$SLUG-seed.iso"
BASE_IMG="ubuntu-$UBUNTU_VER-server-cloudimg-amd64.img"
MIRROR=${UBUNTU_MIRROR:-https://cloud-images.ubuntu.com/releases/$UBUNTU_SERIES/release}

echo "############ Wazuh $WAZUH_VERSION on $BASE_DISTRO $UBUNTU_VER"

# --------------------------------------------------------------------------
#  The base image, and proof it is the one Canonical published
# --------------------------------------------------------------------------
SUMS=$(curl -fsSL --retry 5 --retry-delay 3 --retry-connrefused \
            --connect-timeout 20 --max-time 120 "$MIRROR/SHA256SUMS") || {
  echo "!! could not fetch $MIRROR/SHA256SUMS -- the base image cannot be" >&2
  echo "   verified, so the build stops here rather than trusting a cache." >&2
  exit 1
}
WANT=$(echo "$SUMS" | awk -v f="$BASE_IMG" '{ n=$2; sub(/^\*/, "", n); if (n == f) print $1 }')
if [ -z "$WANT" ]; then
  echo "!! no checksum for $BASE_IMG in $MIRROR/SHA256SUMS" >&2
  echo "   what that directory publishes:" >&2
  echo "$SUMS" | awk '{ n=$2; sub(/^\*/,"",n); print "     " n }' >&2
  exit 1
fi
"$HERE/fetch-media.sh" "$MIRROR/$BASE_IMG" "$WANT" "$CACHE/$BASE_IMG"

# --------------------------------------------------------------------------
#  The seed
# --------------------------------------------------------------------------
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
FIRST_BOOT_B64=$(base64 -w0 "$HERE/wazuh-first-boot")
sed -e "s/@WAZUH_VERSION@/$WAZUH_VERSION/g" \
    -e "s/@FILEBEAT_VERSION@/$FILEBEAT_VERSION/g" \
    -e "s/@WAZUH_KEY_FPR@/$WAZUH_KEY_FPR/g" \
    -e "s/@CERTS_TOOL_SHA256@/$CERTS_TOOL_SHA256/g" \
    -e "s/@PASSWORDS_TOOL_SHA256@/$PASSWORDS_TOOL_SHA256/g" \
    -e "s/@FILEBEAT_YML_SHA256@/$FILEBEAT_YML_SHA256/g" \
    -e "s/@ALERTS_TEMPLATE_SHA256@/$ALERTS_TEMPLATE_SHA256/g" \
    -e "s/@FILEBEAT_MODULE_SHA256@/$FILEBEAT_MODULE_SHA256/g" \
    -e "s|@FIRST_BOOT_B64@|$FIRST_BOOT_B64|g" \
    -e "s/@IMAGE_REV@/$IMAGE_REV/g" \
    "$HERE/wazuh-build.yaml" > "$TMP/user-data"
cat > "$TMP/meta-data" <<META
instance-id: delonix-wazuhbuild-$WAZUH_VERSION-$$
local-hostname: wazuh-build
META
cloud-localds "$SEED" "$TMP/user-data" "$TMP/meta-data"

# --------------------------------------------------------------------------
#  Build
# --------------------------------------------------------------------------
rm -f "$RAW" "$LOG"
cp --reflink=auto "$CACHE/$BASE_IMG" "$RAW"
qemu-img resize "$RAW" "${DISK_GB}G" >/dev/null

if [ -w /dev/kvm ]; then
  ACCEL=(-enable-kvm -cpu host)
else
  echo "==> /dev/kvm not available: falling back to TCG (slow)"
  ACCEL=(-cpu max)
fi

echo "==> building (headless, serial log: $LOG)"
timeout "$BUILD_TIMEOUT" qemu-system-x86_64 \
  "${ACCEL[@]}" -m "$MEM" -smp "$SMP" \
  -drive file="$RAW",if=virtio,format=qcow2,cache=unsafe \
  -drive file="$SEED",if=virtio,format=raw,readonly=on \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$LOG" -no-reboot || true

if grep -aq "DELONIX-WAZUH-BUILD-OK" "$LOG"; then
  echo "==> guest reported success"
elif grep -aq "DELONIX-WAZUH-BUILD-FAIL" "$LOG"; then
  echo "!! the build script failed inside the guest. Its own last lines:"
  sed -n '/DELONIX-WAZUH-BUILD-FAIL/,$p' "$LOG" | tr -d '\r' | head -60
  exit 1
else
  echo "!! no marker on the console -- the guest never finished (timeout, panic," >&2
  echo "   or cloud-init never ran). Last lines of $LOG:" >&2
  tail -40 "$LOG" | tr -d '\r' >&2
  exit 1
fi

# --------------------------------------------------------------------------
#  Read back what is actually inside
# --------------------------------------------------------------------------
export LIBGUESTFS_BACKEND=${LIBGUESTFS_BACKEND:-direct}
# Ubuntu installs /boot/vmlinuz-* as 0600, and supermin (which every libguestfs
# tool below needs) copies the host kernel into its appliance -- so as a normal
# user it dies with "Permission denied". Chmod-ing /boot would lower a host
# boundary, which is not this script's call; use the newest kernel that IS
# readable instead. Only when the caller has not chosen one.
if [ -z "${SUPERMIN_KERNEL:-}" ] && [ ! -r "/boot/vmlinuz-$(uname -r)" ]; then
  for k in $(ls -1r /boot/vmlinuz-* 2>/dev/null); do
    v=${k#/boot/vmlinuz-}
    if [ -r "$k" ] && [ -d "/lib/modules/$v" ]; then
      export SUPERMIN_KERNEL=$k SUPERMIN_MODULES=/lib/modules/$v
      echo "==> libguestfs: running kernel not readable, using $v"
      break
    fi
  done
fi
echo "==> what the image carries:"
if ! virt-cat -a "$RAW" /etc/delonix/wazuh-image.json; then
  echo "!! /etc/delonix/wazuh-image.json is not in the image -- the build" >&2
  echo "   marked success without leaving its own record. Refusing to publish." >&2
  exit 1
fi

# --------------------------------------------------------------------------
#  Finish
# --------------------------------------------------------------------------
echo "==> sparsifying"
virt-sparsify --in-place "$RAW"
echo "==> compressing"
rm -f "$FINAL"
qemu-img convert -O qcow2 -c -o compression_type=zstd "$RAW" "$FINAL"
rm -f "$RAW" "$SEED"
ls -lh "$FINAL"
qemu-img info "$FINAL" | grep -E "virtual size|disk size|compression"

echo
echo "Register it with:"
echo "  delonix image vm import $FINAL -t wazuh:${WAZUH_VERSION%-*}-r$IMAGE_REV \\"
echo "      --distro ubuntu --release $UBUNTU_VER \\"
echo "      --default-vcpus 4 --default-memory 8G"
echo
echo "Publish it with:"
echo "  delonix image vm push wazuh:${WAZUH_VERSION%-*}-r$IMAGE_REV \\"
echo "      ghcr.io/angolardevops/delonix-vm-appliances:wazuh-${WAZUH_VERSION%-*}-r$IMAGE_REV"
echo
echo "PROVEN by this script: the versions are pinned, the vendor's signing key matched"
echo "  its documented fingerprint, every downloaded asset matched its pinned sha256,"
echo "  the guest's own script ran to the end, and /etc/delonix/wazuh-image.json was"
echo "  read back out of the finished disk."
echo "NOT PROVEN by this script: that the stack comes up -- that is verify-wazuh.sh's job,"
echo "  against a booted clone, because the certificates are generated at first boot."
