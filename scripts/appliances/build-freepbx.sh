#!/bin/bash
# Build the FreePBX 17 image: stock FreePBX 17 + Asterisk 22 (LTS) on Debian 12,
# on top of a Debian 12 genericcloud image.
#
#   build-freepbx.sh
#
# Same shape as build-glpi.sh: a cloud image, one provisioning script run once
# inside QEMU, a guest-reported verdict, a read-back before publishing.
#
# FreePBX has no release tarball: the vendor's supported path is its own
# installer, `sng_freepbx_debian_install.sh`, which adds deb.freepbx.org and
# installs Asterisk and FreePBX from it. That script is pinned by commit AND
# by sha256 here, run as the vendor ships it (`--skipversion` only; why not
# `--opensourceonly` is in freepbx-build.yaml), and the apt key it
# installs is checked against a pinned fingerprint afterwards -- the installer
# fetches that key over plain http, so trusting it unchecked would trust
# whoever answered that request.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
CACHE=${MEDIA_CACHE:-$HERE/.media}

# Pinned. Read off the vendors on 2026-09-30, not remembered:
#   * the installer is github.com/FreePBX/sng_freepbx_debian_install at the
#     commit that was HEAD that day (SCRIPTVER 1.15); its default Asterisk is 22,
#     the LTS line with security fixes to 2029 (docs.asterisk.org, Asterisk
#     Versions) -- the reason this image exists instead of an Issabel one,
#     whose repository stops at Asterisk 18.19.0, out of security support.
#   * the key is deb.freepbx.org/gpg/aptly-pubkey.asc, identical over http and
#     https that day; it expires 2028-02-12.
#   * the Debian build is the dated cloud.debian.org directory, not `latest`,
#     so two builds a month apart start from the same disk.
FREEPBX_INSTALLER_COMMIT=${FREEPBX_INSTALLER_COMMIT:-b023b8426786a518d25dfc0d11ba0c0314fcd927}
FREEPBX_INSTALLER_SHA256=${FREEPBX_INSTALLER_SHA256:-afef5e4b480cf545b2035f92068dc2fdd32452989d170a16a84acb6e37b7d564}
FREEPBX_KEY_FPR=${FREEPBX_KEY_FPR:-991C357C8A359D0382BC6E87C4DFE68FCE6DE186}
ASTERISK_MAJOR=${ASTERISK_MAJOR:-22}
DEBIAN_BUILD=${DEBIAN_BUILD:-20260923-2610}
# Bumped when the image changes without any pinned version changing.
IMAGE_REV=${IMAGE_REV:-1}

DEBIAN_VER=12
DISK_GB=${DISK_GB:-20}
MEM=${MEM:-4096}
SMP=${SMP:-2}
# The installer compiles nothing, but `fwconsole ma upgradeall` walks every
# module and the sound packages are large.
BUILD_TIMEOUT=${BUILD_TIMEOUT:-3600}

SLUG="freepbx17-asterisk$ASTERISK_MAJOR-r$IMAGE_REV"
RAW="$OUT/$SLUG.raw.qcow2"
FINAL="$OUT/$SLUG.qcow2"
LOG="$OUT/$SLUG-build.log"
SEED="$HERE/.$SLUG-seed.iso"
BASE_IMG="debian-$DEBIAN_VER-genericcloud-amd64-$DEBIAN_BUILD.qcow2"
MIRROR=${DEBIAN_MIRROR:-https://cloud.debian.org/images/cloud/bookworm/$DEBIAN_BUILD}

echo "############ FreePBX 17 + Asterisk $ASTERISK_MAJOR on Debian $DEBIAN_VER ($DEBIAN_BUILD)"

# --------------------------------------------------------------------------
#  The base image, and proof it is the one Debian published
# --------------------------------------------------------------------------
SUMS=$(curl -fsSL --retry 5 --retry-delay 3 --retry-connrefused \
            --connect-timeout 20 --max-time 120 "$MIRROR/SHA512SUMS") || {
  echo "!! could not fetch $MIRROR/SHA512SUMS -- the base image cannot be" >&2
  echo "   verified, so the build stops here rather than trusting a cache." >&2
  exit 1
}
WANT=$(echo "$SUMS" | awk -v f="$BASE_IMG" '{ n=$2; sub(/^\*/, "", n); if (n == f) print $1 }')
if [ -z "$WANT" ]; then
  echo "!! no checksum for $BASE_IMG in $MIRROR/SHA512SUMS" >&2
  echo "   what that directory publishes:" >&2
  echo "$SUMS" | awk '{ n=$2; sub(/^\*/,"",n); print "     " n }' >&2
  exit 1
fi
# fetch-media.sh checks sha256; Debian publishes sha512, so check it here.
if [ ! -f "$CACHE/$BASE_IMG" ] || ! echo "$WANT  $CACHE/$BASE_IMG" | sha512sum -c --status -; then
  mkdir -p "$CACHE"
  curl -fL --retry 5 --retry-delay 3 --connect-timeout 20 --no-progress-meter \
       -o "$CACHE/$BASE_IMG.part" "$MIRROR/$BASE_IMG"
  echo "$WANT  $CACHE/$BASE_IMG.part" | sha512sum -c --status - || {
    echo "!! $BASE_IMG does not match Debian's SHA512SUMS" >&2; rm -f "$CACHE/$BASE_IMG.part"; exit 1; }
  mv "$CACHE/$BASE_IMG.part" "$CACHE/$BASE_IMG"
fi
echo "==> base image matches Debian's SHA512SUMS"

# --------------------------------------------------------------------------
#  The seed
# --------------------------------------------------------------------------
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
sed -e "s/@FREEPBX_INSTALLER_COMMIT@/$FREEPBX_INSTALLER_COMMIT/g" \
    -e "s/@FREEPBX_INSTALLER_SHA256@/$FREEPBX_INSTALLER_SHA256/g" \
    -e "s/@FREEPBX_KEY_FPR@/$FREEPBX_KEY_FPR/g" \
    -e "s/@ASTERISK_MAJOR@/$ASTERISK_MAJOR/g" \
    -e "s/@DEBIAN_BUILD@/$DEBIAN_BUILD/g" \
    -e "s/@IMAGE_REV@/$IMAGE_REV/g" \
    "$HERE/freepbx-build.yaml" > "$TMP/user-data"
# The first-boot unit travels inside the seed, indented under its write_files entry.
sed 's/^/      /' "$HERE/freepbx-first-boot" > "$TMP/first-boot.indented"
# Anchored: the marker is also named in the seed's header comment.
sed -i -e "/^@FREEPBX_FIRST_BOOT@$/{r $TMP/first-boot.indented" -e 'd}' "$TMP/user-data"
cat > "$TMP/meta-data" <<META
instance-id: delonix-freepbxbuild-$IMAGE_REV-$$
local-hostname: freepbx-build
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

if grep -aq "DELONIX-FREEPBX-BUILD-OK" "$LOG"; then
  echo "==> guest reported success"
elif grep -aq "DELONIX-FREEPBX-BUILD-FAIL" "$LOG"; then
  echo "!! the build script failed inside the guest. Its own last lines:"
  # `head` closes the pipe early: 141 under pipefail, which would hide the exit 1.
  { sed -n '/DELONIX-FREEPBX-BUILD-FAIL/,$p' "$LOG" | tr -d '\r' | head -60; } || true
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
# See build-glpi.sh: supermin needs a readable host kernel.
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
if ! RECORD=$(virt-cat -a "$RAW" /etc/delonix/freepbx-image.json); then
  echo "!! /etc/delonix/freepbx-image.json is not in the image -- the build" >&2
  echo "   marked success without leaving its own record. Refusing to publish." >&2
  exit 1
fi
echo "$RECORD"
# The whole point of this image is a supported Asterisk: a different major
# than the one pinned is a failed build, not a detail.
if ! echo "$RECORD" | grep -Eq "\"asterisk_version\": \"$ASTERISK_MAJOR\.[0-9]"; then
  echo "!! the image does not carry Asterisk $ASTERISK_MAJOR.x. Refusing to publish." >&2
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
echo "  delonix image vm import $FINAL -t freepbx:17-asterisk$ASTERISK_MAJOR-r$IMAGE_REV \\"
echo "      --distro debian --release $DEBIAN_VER \\"
echo "      --default-vcpus 2 --default-memory 4G"
echo
echo "Publish it with:"
echo "  delonix image vm push freepbx:17-asterisk$ASTERISK_MAJOR-r$IMAGE_REV \\"
echo "      ghcr.io/angolardevops/delonix-vm-appliances:freepbx-17-asterisk$ASTERISK_MAJOR-r$IMAGE_REV"
echo
echo "PROVEN by this script: the base image matched Debian's SHA512SUMS, the"
echo "  installer matched its pinned commit's sha256, the FreePBX apt key matched"
echo "  its pinned fingerprint, the guest's own script ran to the end, and"
echo "  /etc/delonix/freepbx-image.json -- with Asterisk $ASTERISK_MAJOR.x -- was read"
echo "  back out of the finished disk."
echo "NOT PROVEN by this script: that FreePBX and Asterisk answer after a real"
echo "  boot, or that the first boot rotates the secrets -- that is"
echo "  verify-freepbx.sh's job, against a running clone."
echo "NOT PINNED: the FreePBX modules. The vendor's installer runs"
echo "  \`fwconsole ma upgradeall\`, so two builds can differ in module versions;"
echo "  the record above lists the ones this build got."
