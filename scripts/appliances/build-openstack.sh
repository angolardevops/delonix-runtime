#!/bin/bash
# Build the OpenStack host image: a cloud image with Kolla Ansible and every
# container image of one OpenStack release already inside it.
#
#   build-openstack.sh [release]
#
#   build-openstack.sh                   # the pinned release below
#   build-openstack.sh 2025.2            # another release, from its stable branch
#
# This is NOT an appliance. The other four products in this directory install
# themselves from vendor media and configure themselves through a web UI;
# OpenStack has no such media, and a deployed OpenStack is bound to the
# addresses of the machine it was deployed on. So the image carries what is
# machine-INDEPENDENT -- the deployment tool, pinned, and the ~20 GiB of
# container images -- and the deployment itself is `openstack_aio` in
# delonix-deploy, which writes the real globals.yml on the real host.
#
# Splitting it there is not a compromise, it is where the seam actually is:
# `kolla_internal_vip_address` and the two interface names cannot be known here
# and must not be guessed. Proxmox VE taught this repository what happens when
# an image ships the build machine's address as if it were its own.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
CACHE=${MEDIA_CACHE:-$HERE/.media}

# Pinned. Read off the vendor on 2026-08-26, not remembered:
#   * OpenStack 2026.1 "Gazpacho" is the current SLURP release -- the one whose
#     upgrade path skips a cycle, which is the only sane choice for something
#     meant to be operated rather than demonstrated.
#   * kolla-ansible 22.x is that release (docs.openstack.org/kolla-ansible/2026.1
#     reports itself as 22.1.1.dev5); 22.1.0 was published 2026-08-06.
#   * Ubuntu Noble 24.04 is one of the three host OSes on the kolla support
#     matrix (the others being Debian 13 and Rocky 10).
DEFAULT_RELEASE=2026.1
BASE_DISTRO=${BASE_DISTRO:-ubuntu}
UBUNTU_SERIES=${UBUNTU_SERIES:-noble}
UBUNTU_VER=${UBUNTU_VER:-24.04}

# 60 GiB and the number is not decoration: the pulled container images occupy
# roughly 20 GiB under /var/lib/docker, on top of a ~2.5 GiB stock rootfs, and
# a deployed all-in-one then wants room for logs and for the MariaDB it runs.
# Thin qcow2 means this is a ceiling and not an allocation -- the compressed
# artefact this script emits is far smaller.
DISK_GB=${DISK_GB:-60}
MEM=${MEM:-4096}
SMP=${SMP:-4}
# Four hours. The pull is ~20 GiB and this workspace measures 3.3 MB/s to the
# mirrors, which is over an hour on its own before anything is installed.
BUILD_TIMEOUT=${BUILD_TIMEOUT:-14400}

RELEASE=${1:-$DEFAULT_RELEASE}
case "$RELEASE" in
  [0-9][0-9][0-9][0-9].[12]) ;;
  *) echo "!! '$RELEASE' is not an OpenStack release name (want e.g. 2026.1)" >&2; exit 1 ;;
esac

SLUG="openstack-$RELEASE-$BASE_DISTRO-$UBUNTU_VER"
RAW="$OUT/$SLUG.raw.qcow2"
FINAL="$OUT/$SLUG.qcow2"
LOG="$OUT/$SLUG-build.log"
SEED="$HERE/.$SLUG-seed.iso"
BASE_IMG="ubuntu-$UBUNTU_VER-server-cloudimg-amd64.img"
MIRROR=${UBUNTU_MIRROR:-https://cloud-images.ubuntu.com/releases/$UBUNTU_SERIES/release}

echo "############ OpenStack $RELEASE on $BASE_DISTRO $UBUNTU_VER"

# --------------------------------------------------------------------------
#  The base image, and proof it is the one Canonical published
# --------------------------------------------------------------------------
# Same fail-closed contract as every other build here. Canonical's SHA256SUMS
# is GNU format with the binary marker (`hash *file`), so the filename column
# carries a leading `*` that has to come off before comparing -- getting that
# wrong yields "no checksum for ..." for a file that is right there.
# `--max-time` and `--connect-timeout` are not decoration. Without them this
# exact call hung for six minutes on a 3 KiB file (measured 2026-08-26, the
# same flaky link that made pip report `ansible-core (from versions: none)`
# inside the guest). `--retry` never fires on a stall, because curl does not
# give up on a connection it still considers open -- and a build that blocks
# forever on its first step is worse than one that fails.
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
# `instance-id` changes per build on purpose: cloud-init only re-runs when it
# sees an instance it does not recognise, and a fixed one turns a rebuild into
# a boot that does nothing and reports success.
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
sed -e "s/@RELEASE@/$RELEASE/g" -e "s/@BASE_DISTRO@/$BASE_DISTRO/g" \
    "$HERE/openstack-build.yaml" > "$TMP/user-data"
cat > "$TMP/meta-data" <<META
instance-id: delonix-osbuild-$RELEASE-$$
local-hostname: openstack-build
META
cloud-localds "$SEED" "$TMP/user-data" "$TMP/meta-data"

# --------------------------------------------------------------------------
#  Build
# --------------------------------------------------------------------------
rm -f "$RAW" "$LOG"
cp --reflink=auto "$CACHE/$BASE_IMG" "$RAW"
# GROW FIRST. The stock Ubuntu cloud image root is ~2.4 GiB with ~139 MiB free
# (measured on ngola, 2026-08-22, and written down in the golden template
# role). Twenty gigabytes of container images do not fit in that, and a pull
# that runs out of space halfway leaves an image that boots and lies.
qemu-img resize "$RAW" "${DISK_GB}G" >/dev/null

# KVM when the host has it, TCG when it does not. Same rule as the other
# builds, and here it matters more: under TCG a 20 GiB pull plus an apt install
# is a working day, not a coffee.
if [ -w /dev/kvm ]; then
  ACCEL=(-enable-kvm -cpu host)
else
  echo "==> /dev/kvm not available: falling back to TCG (very slow for this build)"
  ACCEL=(-cpu max)
fi

echo "==> building (headless, serial log: $LOG)"
echo "    this pulls ~20 GiB of container images; budget an hour or more"
# Two NICs, the shape the target has: kolla wants a management interface with
# an address and a second one for Neutron's external network with none. The
# pull does not need the second, but building with a shape the deploy does not
# have is how a precheck fails for the first time on the real host.
timeout "$BUILD_TIMEOUT" qemu-system-x86_64 \
  "${ACCEL[@]}" -m "$MEM" -smp "$SMP" \
  -drive file="$RAW",if=virtio,format=qcow2,cache=unsafe \
  -drive file="$SEED",if=virtio,format=raw,readonly=on \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
  -netdev user,id=n1 -device virtio-net-pci,netdev=n1 \
  -display none -serial "file:$LOG" -no-reboot || true

# QEMU's exit status says nothing: it is 0 when the guest powered off cleanly
# AND 0 when `timeout` killed it mid-pull. The guest's own marker is the only
# thing that distinguishes those.
if grep -aq "DELONIX-OPENSTACK-BUILD-OK" "$LOG"; then
  echo "==> guest reported success"
elif grep -aq "DELONIX-OPENSTACK-BUILD-FAIL" "$LOG"; then
  echo "!! the build script failed inside the guest. Its own last lines:"
  sed -n '/DELONIX-OPENSTACK-BUILD-FAIL/,$p' "$LOG" | tr -d '\r' | head -50
  exit 1
else
  echo "!! no marker on the console -- the guest never finished (timeout, panic," >&2
  echo "   or cloud-init never ran). Last lines of $LOG:" >&2
  tail -30 "$LOG" | tr -d '\r' >&2
  exit 1
fi

# --------------------------------------------------------------------------
#  Read back what is actually inside
# --------------------------------------------------------------------------
# The marker says the script reached its end. This says what the script left
# behind, read out of the disk itself -- the same distinction the golden
# template role draws between what a node believes it built and what a clone
# actually gets.
export LIBGUESTFS_BACKEND=${LIBGUESTFS_BACKEND:-direct}
echo "==> what the image carries:"
if ! virt-cat -a "$RAW" /etc/delonix/openstack-image.json; then
  echo "!! /etc/delonix/openstack-image.json is not in the image -- the build" >&2
  echo "   marked success without leaving its own record. Refusing to publish." >&2
  exit 1
fi
IMAGES=$(virt-cat -a "$RAW" /etc/delonix/openstack-image.json | sed -n 's/.*"images": *\([0-9]*\).*/\1/p')
if [ "${IMAGES:-0}" -lt 20 ]; then
  echo "!! only ${IMAGES:-0} container images in the guest's docker. A full" >&2
  echo "   all-in-one pull is dozens; this image would deploy by downloading" >&2
  echo "   the rest at deploy time, which is the one thing it exists to avoid." >&2
  exit 1
fi
echo "==> $IMAGES container images baked in"

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

# No `--appliance`. This IS a cloud image: it wants the NoCloud seed that
# `vm create` builds, because that seed is how the target's hostname, SSH key
# and network reach it. Marking it an appliance would make `vm create` refuse
# `--ssh-key` and hand over a VM nobody can log into.
echo
echo "Register it with:"
echo "  delonix image vm import $FINAL -t openstack:$RELEASE \\"
echo "      --distro ubuntu --release $UBUNTU_VER \\"
echo "      --default-vcpus 6 --default-memory 16G"
echo
echo "PROVEN by this script: the release is pinned, the base image matched the"
echo "  checksum Canonical published, the guest's own script ran to the end, and"
echo "  $IMAGES container images were read back out of the finished disk."
echo "NOT PROVEN by this script: that OpenStack deploys from it. That needs a"
echo "  host with a real VIP and real interfaces -- playbooks/openstack.yml in"
echo "  delonix-deploy, and then verify-boot.sh openstack."
