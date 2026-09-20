#!/bin/bash
# Build the monitoring host image: Zabbix Server + Grafana, wired together and
# ready to answer, on top of an Ubuntu 24.04 cloud image.
#
#   build-monitoring.sh
#
# This is NOT an appliance in the sense the other four products in this
# directory are: there is no vendor installer to drive. It follows
# build-openstack.sh's shape instead -- a cloud image, one provisioning
# script run once inside QEMU, a guest-reported verdict, a read-back before
# publishing -- because that is what building "Ubuntu plus two pinned
# packages, wired together" actually is.
#
# What "wired together" means concretely: the Zabbix frontend never shows its
# setup wizard (zabbix.conf.php is pre-seeded), and Grafana already has
# Zabbix configured as a data source the moment it boots. What this image
# deliberately does NOT decide is WHICH remote network to monitor -- that is
# a per-deployment choice (SNMP/agent targets added in Zabbix, reachability
# via whatever `--net`/`kind: NetworkRoute`/VPN gateway the VM is attached
# to), not something a golden image can bake in for every clone at once. A
# tenant-facing "point this at my network" self-service flow is a platform
# feature built ON TOP of this VM, in delonix-paas -- not a Kind this engine
# can own, because the engine has no concept of tenant to hang it off (see
# AGENTS.md, "Identidade e fronteira do motor").
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
CACHE=${MEDIA_CACHE:-$HERE/.media}

# Pinned. Read off the vendors on 2026-09-19, not remembered:
#   * Zabbix 7.0 is the released LTS in full support until 2027-06-30 (5.0
#     years total) -- zabbix.com/life_cycle_and_release_policy. 7.4 is a
#     standard release with ~12 months of support, and 8.0 was still beta on
#     that page. Same reasoning as pinning OpenStack's SLURP release over a
#     dev branch: this image is meant to be operated, not demonstrated.
#   * 7.0.30-1+ubuntu24.04 is the newest package build in
#     repo.zabbix.com/zabbix/7.0/ubuntu/dists/noble/main/binary-amd64/
#     on that date; the release bootstrap .deb is pinned to build 7.0-5,
#     the newest of the five listed in .../pool/main/z/zabbix-release/.
#   * Grafana 13.2.2 is the version grafana.com/grafana/download reports as
#     current stable on that date.
#   * alexanderzobnin-zabbix-app 6.7.0 is grafana.com/api/plugins' current
#     version for that plugin, with signatureType "grafana" -- checked before
#     writing this, so the build does not also need
#     allow_loading_unsigned_plugins.
ZABBIX_SERIES=${ZABBIX_SERIES:-7.0}
ZABBIX_VERSION=${ZABBIX_VERSION:-7.0.30-1}
GRAFANA_VERSION=${GRAFANA_VERSION:-13.2.2}
ZABBIX_GRAFANA_APP_VERSION=${ZABBIX_GRAFANA_APP_VERSION:-6.7.0}
# Observability layer, read off the vendors on 2026-09-20. Prometheus is the
# 3.13 LTS line (its release notes say so; 3.14 is not LTS), for the same
# reason Zabbix is 7.0. Loki and Alloy are the exact versions apt.grafana.com
# lists; the four prometheus/* tarballs are checked against the sha256sums.txt
# each release publishes.
PROMETHEUS_VERSION=${PROMETHEUS_VERSION:-3.13.1}
ALERTMANAGER_VERSION=${ALERTMANAGER_VERSION:-0.34.1}
BLACKBOX_VERSION=${BLACKBOX_VERSION:-0.28.0}
NODE_EXPORTER_VERSION=${NODE_EXPORTER_VERSION:-1.12.1}
LOKI_VERSION=${LOKI_VERSION:-3.7.8}
# goflow2 publishes no checksum file, but GitHub computes a sha256 digest for every
# release asset (`gh api repos/netsampler/goflow2/releases/latest`); this is the
# one for the linux-amd64 binary, and the build stops if the download differs.
GOFLOW2_VERSION=${GOFLOW2_VERSION:-2.2.6}
GOFLOW2_SHA256=${GOFLOW2_SHA256:-0b8b8b081ff810d01431bfe6f1d10ab0be9de8bbe3da1cb700317fe06b2a0f14}
ALLOY_VERSION=${ALLOY_VERSION:-1.19.2-1}
# Bumped when the image changes without any pinned version changing.
IMAGE_REV=${IMAGE_REV:-3}

BASE_DISTRO=ubuntu
UBUNTU_SERIES=noble
UBUNTU_VER=24.04

# A single-host Zabbix+Grafana stack is far lighter than the OpenStack
# image's 20 GiB container pull: two packages, a schema import, no bulk data.
DISK_GB=${DISK_GB:-20}
MEM=${MEM:-4096}
SMP=${SMP:-2}
BUILD_TIMEOUT=${BUILD_TIMEOUT:-1800}

SLUG="monitoring-zabbix$ZABBIX_SERIES-grafana$GRAFANA_VERSION-r$IMAGE_REV"
RAW="$OUT/$SLUG.raw.qcow2"
FINAL="$OUT/$SLUG.qcow2"
LOG="$OUT/$SLUG-build.log"
SEED="$HERE/.$SLUG-seed.iso"
BASE_IMG="ubuntu-$UBUNTU_VER-server-cloudimg-amd64.img"
MIRROR=${UBUNTU_MIRROR:-https://cloud-images.ubuntu.com/releases/$UBUNTU_SERIES/release}

echo "############ monitoring: Zabbix $ZABBIX_VERSION + Grafana $GRAFANA_VERSION on $BASE_DISTRO $UBUNTU_VER"

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
sed -e "s/@ZABBIX_SERIES@/$ZABBIX_SERIES/g" \
    -e "s/@ZABBIX_VERSION@/$ZABBIX_VERSION/g" \
    -e "s/@GRAFANA_VERSION@/$GRAFANA_VERSION/g" \
    -e "s/@ZABBIX_GRAFANA_APP_VERSION@/$ZABBIX_GRAFANA_APP_VERSION/g" \
    -e "s/@PROMETHEUS_VERSION@/$PROMETHEUS_VERSION/g" \
    -e "s/@ALERTMANAGER_VERSION@/$ALERTMANAGER_VERSION/g" \
    -e "s/@BLACKBOX_VERSION@/$BLACKBOX_VERSION/g" \
    -e "s/@NODE_EXPORTER_VERSION@/$NODE_EXPORTER_VERSION/g" \
    -e "s/@LOKI_VERSION@/$LOKI_VERSION/g" \
    -e "s/@GOFLOW2_VERSION@/$GOFLOW2_VERSION/g" \
    -e "s/@GOFLOW2_SHA256@/$GOFLOW2_SHA256/g" \
    -e "s/@ALLOY_VERSION@/$ALLOY_VERSION/g" \
    -e "s/@IMAGE_REV@/$IMAGE_REV/g" \
    "$HERE/monitoring-build.yaml" > "$TMP/user-data"
cat > "$TMP/meta-data" <<META
instance-id: delonix-monbuild-$ZABBIX_SERIES-$$
local-hostname: monitoring-build
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

if grep -aq "DELONIX-MONITORING-BUILD-OK" "$LOG"; then
  echo "==> guest reported success"
elif grep -aq "DELONIX-MONITORING-BUILD-FAIL" "$LOG"; then
  echo "!! the build script failed inside the guest. Its own last lines:"
  sed -n '/DELONIX-MONITORING-BUILD-FAIL/,$p' "$LOG" | tr -d '\r' | head -60
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
if ! virt-cat -a "$RAW" /etc/delonix/monitoring-image.json; then
  echo "!! /etc/delonix/monitoring-image.json is not in the image -- the build" >&2
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

# No `--appliance`: this IS a cloud image (it wants the NoCloud seed
# `vm create` builds, for hostname/SSH key/network), it just happens to have
# Zabbix and Grafana pre-installed and pre-wired.
echo
echo "Register it with:"
echo "  delonix image vm import $FINAL -t monitoring:$ZABBIX_SERIES-r$IMAGE_REV \\"
echo "      --distro ubuntu --release $UBUNTU_VER \\"
echo "      --default-vcpus 2 --default-memory 4G"
echo
echo "Publish it with:"
echo "  delonix image vm push monitoring:$ZABBIX_SERIES-r$IMAGE_REV \\"
echo "      ghcr.io/angolardevops/delonix-vm-appliances:monitoring-$ZABBIX_SERIES-r$IMAGE_REV"
echo
echo "PROVEN by this script: the versions are pinned, the base image matched"
echo "  the checksum Canonical published, the guest's own script ran to the"
echo "  end, and /etc/delonix/monitoring-image.json was read back out of the"
echo "  finished disk."
echo "NOT PROVEN by this script: that the Zabbix web UI and the Grafana"
echo "  dashboard actually answer over HTTP after a real boot -- that is"
echo "  verify-boot.sh's job, against a running clone."
