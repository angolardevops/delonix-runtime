#!/bin/bash
# Build a bootable Proxmox appliance image from an installation ISO.
#
#   build-proxmox.sh <product> [version|src.iso]
#
#   build-proxmox.sh pve                 # the pinned version below, fetched+verified
#   build-proxmox.sh pve 9.2-1           # another version, fetched+verified
#   build-proxmox.sh pve /path/to.iso    # an ISO you already have
#
# Runs the vendor's own automated installer unattended against an empty disk
# in QEMU/KVM, then flattens and compresses the result. Nothing about the
# guest is hand-rolled: the ISO installs itself exactly as it would on metal.
set -euo pipefail

PRODUCT=$1
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
DISK_GB=${DISK_GB:-20}
MEM=${MEM:-4096}
# Where fetched media is kept between builds. These are ~1.5 GiB each and the
# checksum makes re-use safe, so a rebuild costs nothing.
CACHE=${MEDIA_CACHE:-$HERE/.media}
MIRROR=${PROXMOX_MIRROR:-https://enterprise.proxmox.com/iso}

# The ISO file stem, per product. Not derivable from the short name — and
# guessing it would produce a 404 after the checksum had already been fetched.
case "$PRODUCT" in
  pve) STEM=proxmox-ve;                 DEFAULT_VER=9.2-1 ;;
  pbs) STEM=proxmox-backup-server;      DEFAULT_VER=4.2-1 ;;
  pmg) STEM=proxmox-mail-gateway;       DEFAULT_VER=9.1-1 ;;
  pdm) STEM=proxmox-datacenter-manager; DEFAULT_VER=1.1-1 ;;
  *) echo "!! unknown product '$PRODUCT' (want: pve, pbs, pmg or pdm)" >&2; exit 1 ;;
esac

# Second argument: an existing FILE is used as-is (the original contract, kept);
# anything else is a version to fetch. Deciding by "does this path exist" and
# not by shape means a typo in a path fetches rather than failing with a
# confusing "no such ISO" — and the fetch then fails loudly on a real 404.
ARG=${2:-$DEFAULT_VER}
if [ -f "$ARG" ]; then
  SRC_ISO=$ARG
  echo "############ $PRODUCT (local ISO: $SRC_ISO)"
else
  VER=$ARG
  SRC_ISO="$CACHE/${STEM}_${VER}.iso"
  echo "############ $PRODUCT $VER"
  # The vendor publishes one GNU-format SHA256SUMS for the whole directory.
  # Pull the line for THIS file: an absent entry means the version does not
  # exist (or was withdrawn), and that has to stop the build rather than
  # download something nobody vouched for.
  SUMS=$(curl -fsSL --retry 3 "$MIRROR/SHA256SUMS")
  WANT=$(echo "$SUMS" | awk -v f="${STEM}_${VER}.iso" '$2 == f {print $1}')
  if [ -z "$WANT" ]; then
    echo "!! no checksum for ${STEM}_${VER}.iso in $MIRROR/SHA256SUMS" >&2
    echo "   published versions of this product:" >&2
    echo "$SUMS" | awk -v s="$STEM" '$2 ~ "^"s"_" {print "     " $2}' >&2
    exit 1
  fi
  "$HERE/fetch-media.sh" "$MIRROR/${STEM}_${VER}.iso" "$WANT" "$SRC_ISO"
fi

# With a local ISO the version is not known from an argument, so read it off the
# vendor's own filename. It is only ever used to NAME the output.
if [ -z "${VER:-}" ]; then
  VER=$(basename "$SRC_ISO" | sed -n "s/^${STEM}_\(.*\)\.iso$/\1/p")
fi
# The version belongs in the output name. Without it, building 9.2 silently
# overwrites the 9.1 image sitting in the same directory — and the whole point
# of keeping both tags is that both exist.
SLUG="$PRODUCT${VER:+-$VER}"
RAW="$OUT/$SLUG.raw.qcow2"
FINAL="$OUT/$SLUG.qcow2"
LOG="$OUT/$SLUG-install.log"
ISO="$HERE/$SLUG-auto.iso"
ANSWER=${ANSWER:-$HERE/answer-$PRODUCT.toml}

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
  -display none -serial "file:$OUT/$SLUG-postinstall.log" &
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

# Close the loop: the tag is what makes the image reachable, and `--appliance`
# is what stops `vm create` from generating a cloud-init seed this guest cannot
# read. Getting either wrong is only noticed at first boot.
TAG=${VER%%-*}
echo
echo "Register it with:"
echo "  delonix image vm import $FINAL -t $PRODUCT:${TAG:-latest} --appliance \\"
echo "      --distro $PRODUCT --release ${VER:-unknown} --default-vcpus 2 --default-memory 4G"
