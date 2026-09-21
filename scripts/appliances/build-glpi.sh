#!/bin/bash
# Build the GLPI (ITSM / asset inventory) image: GLPI 11 with its own database,
# web server and cron, plus the GLPI Agent that inventories the machine itself,
# on top of an Ubuntu 24.04 cloud image.
#
#   build-glpi.sh
#
# Same shape as build-monitoring.sh: a cloud image, one provisioning script run
# once inside QEMU, a guest-reported verdict, a read-back before publishing.
# GLPI is installed from the vendor's release tarball (not an apt repo), which
# is checked against the sha256 GitHub publishes for that release asset.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
CACHE=${MEDIA_CACHE:-$HERE/.media}

# Pinned. Read off the vendors on 2026-09-20, not remembered:
#   * GLPI 11.0.9 is the newest non-prerelease on github.com/glpi-project/glpi
#     (12.0.0-rc2 is a release candidate; 10.0.27 is the previous major).
#   * GLPI Agent 1.19 is the newest release of glpi-project/glpi-agent.
#   * Both sha256 values are the `digest` GitHub itself reports for the asset.
GLPI_VERSION=${GLPI_VERSION:-11.0.9}
GLPI_SHA256=${GLPI_SHA256:-5f0b52fee2661d14ff9ed6e106d98c6f28c2fc91c4aa89c852e4400c219775b9}
GLPI_AGENT_VERSION=${GLPI_AGENT_VERSION:-1.19}
GLPI_AGENT_SHA256=${GLPI_AGENT_SHA256:-86e3ff59d27af43849601e8eba86d79324e452d011a5d25954eca322b5e73d36}
# Bumped when the image changes without any pinned version changing.
IMAGE_REV=${IMAGE_REV:-1}

BASE_DISTRO=ubuntu
UBUNTU_SERIES=noble
UBUNTU_VER=24.04

# A single-host Zabbix+Grafana stack is far lighter than the OpenStack
# image's 20 GiB container pull: two packages, a schema import, no bulk data.
DISK_GB=${DISK_GB:-12}
MEM=${MEM:-4096}
SMP=${SMP:-2}
BUILD_TIMEOUT=${BUILD_TIMEOUT:-1800}

SLUG="glpi$GLPI_VERSION-agent$GLPI_AGENT_VERSION-r$IMAGE_REV"
RAW="$OUT/$SLUG.raw.qcow2"
FINAL="$OUT/$SLUG.qcow2"
LOG="$OUT/$SLUG-build.log"
SEED="$HERE/.$SLUG-seed.iso"
BASE_IMG="ubuntu-$UBUNTU_VER-server-cloudimg-amd64.img"
MIRROR=${UBUNTU_MIRROR:-https://cloud-images.ubuntu.com/releases/$UBUNTU_SERIES/release}

echo "############ GLPI $GLPI_VERSION + agent $GLPI_AGENT_VERSION on $BASE_DISTRO $UBUNTU_VER"

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
sed -e "s/@GLPI_VERSION@/$GLPI_VERSION/g" \
    -e "s/@GLPI_SHA256@/$GLPI_SHA256/g" \
    -e "s/@GLPI_AGENT_VERSION@/$GLPI_AGENT_VERSION/g" \
    -e "s/@GLPI_AGENT_SHA256@/$GLPI_AGENT_SHA256/g" \
    -e "s/@IMAGE_REV@/$IMAGE_REV/g" \
    "$HERE/glpi-build.yaml" > "$TMP/user-data"
cat > "$TMP/meta-data" <<META
instance-id: delonix-glpibuild-$GLPI_VERSION-$$
local-hostname: glpi-build
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

if grep -aq "DELONIX-GLPI-BUILD-OK" "$LOG"; then
  echo "==> guest reported success"
elif grep -aq "DELONIX-GLPI-BUILD-FAIL" "$LOG"; then
  echo "!! the build script failed inside the guest. Its own last lines:"
  sed -n '/DELONIX-GLPI-BUILD-FAIL/,$p' "$LOG" | tr -d '\r' | head -60
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
if ! virt-cat -a "$RAW" /etc/delonix/glpi-image.json; then
  echo "!! /etc/delonix/glpi-image.json is not in the image -- the build" >&2
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
echo "  delonix image vm import $FINAL -t glpi:$GLPI_VERSION-r$IMAGE_REV \\"
echo "      --distro ubuntu --release $UBUNTU_VER \\"
echo "      --default-vcpus 2 --default-memory 4G"
echo
echo "Publish it with:"
echo "  delonix image vm push glpi:$GLPI_VERSION-r$IMAGE_REV \\"
echo "      ghcr.io/angolardevops/delonix-vm-appliances:glpi-$GLPI_VERSION-r$IMAGE_REV"
echo
echo "PROVEN by this script: the versions are pinned, both downloads matched the"
echo "  sha256 GitHub publishes, the base image matched Canonical's checksum, the"
echo "  guest's own script ran to the end, and /etc/delonix/glpi-image.json was"
echo "  read back out of the finished disk."
echo "NOT PROVEN by this script: that GLPI answers after a real boot -- that is"
echo "  verify-glpi.sh's job, against a running clone."
