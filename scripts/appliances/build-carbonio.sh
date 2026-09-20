#!/bin/bash
# Build the Carbonio CE mail image: the vendor's pinned packages and PostgreSQL 16
# on an Ubuntu 24.04 cloud image, stopped BEFORE `carbonio-bootstrap`.
#
#   build-carbonio.sh
#
# Carbonio is the open-source successor of Zimbra OSE, maintained by Zextras
# (AGPL, official signed repository, Ubuntu 24.04 supported). It replaces the
# Zimbra image that was first asked for: Zimbra OSE has no official binary for
# 24.04 -- the only one is a third-party build that asks for registration, is
# beta on that release, and whose binary EULA could not be confirmed for
# redistribution in a public registry. Carbonio has none of those problems.
#
# PRE-bootstrap on purpose. The bootstrap needs the machine's FQDN, address,
# mail domain and admin password; baking a guess would ship the build VM's
# identity in every clone. See the README section "Carbonio CE" for the
# commands that finish the job on the target.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
CACHE=${MEDIA_CACHE:-$HERE/.media}

# Pinned. Read off repo.zextras.io/release/ubuntu/dists/noble on 2026-09-20 --
# every one of the 28 packages the vendor's manual-installation guide names
# exists there, and these are the versions it served that day. The guide's page
# reports Carbonio CE 26.6.0.
CARBONIO_RELEASE=${CARBONIO_RELEASE:-26.6.0}
CARBONIO_PINS=${CARBONIO_PINS:-"service-discover-server=0.7.4-1noble carbonio-directory-server=5.0.2-1noble carbonio-proxy=4.14.4-1noble carbonio-webui=4.5.6-1noble carbonio-files-ui=2.17.0-1ubuntu carbonio-mta=4.2.8-1noble carbonio-appserver=4.5.1-1noble carbonio-user-management=1.2.1-1noble carbonio-files-ce=1.2.4-1noble carbonio-files-public-folder-ui=0.0.9-1ubuntu carbonio-files-db=0.4.2-1noble carbonio-tasks-ce=1.1.4-1noble carbonio-tasks-db=0.2.2-1noble carbonio-tasks-ui=0.1.0-1ubuntu carbonio-storages-ce=1.0.17-1ubuntu carbonio-preview-ce=1.3.1-1noble carbonio-docs-connector-ce=1.1.3-1noble carbonio-docs-editor=25.04.8-6noble carbonio-prometheus=3.9.1-1noble carbonio-message-broker=0.3.3-1noble carbonio-message-dispatcher-ce=1.2.5-1noble carbonio-message-dispatcher-db=0.4.4-1noble carbonio-ws-collaboration-ce=1.7.1-1noble carbonio-ws-collaboration-db=0.5.2-1noble carbonio-ws-collaboration-ui=0.9.20-1ubuntu carbonio-videoserver-ce=1.2.6-1noble carbonio-catalog=0.5.3-1noble carbonio-memcached=1.6.42-1noble"}
IMAGE_REV=${IMAGE_REV:-2}

BASE_DISTRO=ubuntu
UBUNTU_SERIES=noble
UBUNTU_VER=24.04

# 50 GiB is the vendor's stated minimum for OS + Carbonio (thin qcow2, so a
# ceiling and not an allocation). The BUILD only installs packages, so it does
# not need the 16 GiB the RUNNING system does; running it is a different matter.
DISK_GB=${DISK_GB:-50}
MEM=${MEM:-6144}
SMP=${SMP:-4}
BUILD_TIMEOUT=${BUILD_TIMEOUT:-5400}

SLUG="carbonio-ce$CARBONIO_RELEASE-r$IMAGE_REV"
RAW="$OUT/$SLUG.raw.qcow2"
FINAL="$OUT/$SLUG.qcow2"
LOG="$OUT/$SLUG-build.log"
SEED="$HERE/.$SLUG-seed.iso"
BASE_IMG="ubuntu-$UBUNTU_VER-server-cloudimg-amd64.img"
MIRROR=${UBUNTU_MIRROR:-https://cloud-images.ubuntu.com/releases/$UBUNTU_SERIES/release}

echo "############ carbonio CE $CARBONIO_RELEASE (pre-bootstrap) on $BASE_DISTRO $UBUNTU_VER"

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
# The finishing helper is its own file (one source of truth, testable on its own)
# and travels into the image base64-encoded so no quoting can alter it.
FINISH_INSTALL_B64=$(base64 -w0 "$HERE/carbonio-finish-install")
sed -e "s|@CARBONIO_RELEASE@|$CARBONIO_RELEASE|g" \
    -e "s|@CARBONIO_PINS@|$CARBONIO_PINS|g" \
    -e "s|@FINISH_INSTALL_B64@|$FINISH_INSTALL_B64|g" \
    "$HERE/carbonio-build.yaml" > "$TMP/user-data"
cat > "$TMP/meta-data" <<META
instance-id: delonix-cbbuild-$CARBONIO_RELEASE-$$
local-hostname: carbonio-build
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

if grep -aq "DELONIX-CARBONIO-BUILD-OK" "$LOG"; then
  echo "==> guest reported success"
elif grep -aq "DELONIX-CARBONIO-BUILD-FAIL" "$LOG"; then
  echo "!! the build script failed inside the guest. Its own last lines:"
  sed -n '/DELONIX-CARBONIO-BUILD-FAIL/,$p' "$LOG" | tr -d '\r' | head -60
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
if ! virt-cat -a "$RAW" /etc/delonix/carbonio-image.json; then
  echo "!! /etc/delonix/carbonio-image.json is not in the image -- the build" >&2
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
echo "  delonix image vm import $FINAL -t carbonio:$CARBONIO_RELEASE-r$IMAGE_REV \\"
echo "      --distro ubuntu --release $UBUNTU_VER \\"
echo "      --default-vcpus 4 --default-memory 16G"
echo
echo "PROVEN by this script: the vendor key matched its documented fingerprint,"
echo "  the base image matched Canonical's checksum, all 28 packages installed at"
echo "  the pinned versions, and the record was read back out of the finished disk."
echo "NOT PROVEN by this script: that carbonio-bootstrap completes. It needs an FQDN,"
echo "  DNS and the vendor's 16 GiB -- verify-carbonio.sh proves what a build can,"
echo "  and the README lists what only a real deployment can."
