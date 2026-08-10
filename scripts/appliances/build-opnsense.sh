#!/bin/bash
# Build the OPNsense appliance image.
#
# Nothing is installed here, and that is deliberate: OPNsense publishes a
# PRE-INSTALLED disk image (the `nano` variant), so the vendor's own artifact
# is the appliance. Converting it is the whole build.
#
# Do NOT reach for the `vga`/`serial`/`dvd` images instead — measured, those
# boot in LIVE mode off the installation media (`Root file system:
# /dev/ufs/OPNsense_Install`, "running in live mode from install media"). They
# are the installer in disk form, not an installed system. Only `nano` roots
# on `/dev/ufs/OPNsense_Nano`.
#
# `nano` is tuned for flash media: /var and /tmp live on RAM disks and
# configuration persists under /conf. That is the right trade-off for a
# firewall appliance and is what makes it bootable with no install step.
set -euo pipefail

VERSION=${1:-26.1.2}
BRANCH=${BRANCH:-${VERSION%.*}}
MIRROR=${MIRROR:-https://mirrors.dotsrc.org/opnsense}
OUT=${OUT_DIR:-$(pwd)}
WORK=${WORK_DIR:-$(mktemp -d)}

IMG="OPNsense-$VERSION-nano-amd64.img"
BZ2="$IMG.bz2"
SUMS="OPNsense-$VERSION-checksums-amd64.sha256"
FINAL="$OUT/opnsense-$VERSION.qcow2"

echo "############ opnsense $VERSION"
mkdir -p "$WORK"
cd "$WORK"

echo "==> downloading"
curl -fL --no-progress-meter -o "$SUMS" "$MIRROR/releases/$BRANCH/$SUMS"
[ -f "$BZ2" ] || curl -fL --no-progress-meter -o "$BZ2" "$MIRROR/releases/$BRANCH/$BZ2"

echo "==> verifying"
# The vendor publishes BSD-style `SHA256 (file) = hash`; sha256sum wants the
# GNU form. Never skip this: an unverified download is an unverified image.
want=$(sed -n "s/^SHA256 ($BZ2) = //p" "$SUMS")
test -n "$want" || { echo "!! no checksum for $BZ2 in $SUMS"; exit 1; }
echo "$want  $BZ2" | sha256sum -c -

echo "==> converting"
bunzip2 -kf "$BZ2"
rm -f "$FINAL"
# zstd, compressed: a store image is the read-only backing file of every VM
# created from it, so decompression speed is what matters at runtime.
qemu-img convert -O qcow2 -c -o compression_type=zstd "$IMG" "$FINAL"
rm -f "$IMG"

ls -lh "$FINAL"
echo
echo "Register it with:"
echo "  delonix image vm import $FINAL -t opnsense:$BRANCH --appliance \\"
echo "      --distro opnsense --release $VERSION --default-vcpus 2 --default-memory 2G"
