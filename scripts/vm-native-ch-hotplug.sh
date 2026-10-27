#!/usr/bin/env bash
# =============================================================================
# Live proof — Cloud Hypervisor hot DISK/NIC add AND remove (ADR-0068 Ph. 4/5/6).
# Boots a native CH microVM, hot-adds a disk and a NIC over the api-socket, proves
# INSIDE the guest (over `vm ssh`) that the guest sees and uses each, then hot-
# removes both (`vm.remove-device`) and proves the guest LOST each.
#
# The `vm update --disk-add/--nic-add` call itself exits non-zero on this
# backend (honest `partial`: CH has no in-band guest-confirm channel), so the
# verdict is NEVER the CLI exit code — it is what the guest reports.
#
# Usage:  ./vm-native-ch-hotplug.sh [path-to-delonix]
# Env:    DLX_IMAGE  (default: first cloud-init golden via `vm image ls`)
#         KEEP=1     (keep the work dir and the VM for inspection)
# =============================================================================
set -u
B="${1:-$(command -v delonix || echo "$HOME/.local/bin/delonix")}"
WORK="$(mktemp -d /tmp/dlxhp.XXXXXX)"
KEY="$WORK/key"
EVID="$WORK/evidence.txt"
PFX="dlxhp-$$"
VM="$PFX-a"
PASS=0; FAIL=0
trap 'cleanup' EXIT
log(){ echo "$*" | tee -a "$EVID"; }
ok(){ log "PASS: $*"; PASS=$((PASS+1)); }
bad(){ log "FAIL: $*"; FAIL=$((FAIL+1)); }
die(){ log "ABORT: $*"; exit 2; }
gx(){ timeout 30 "$B" vm ssh "$VM" --identity "$KEY" -- "$@" 2>/dev/null; }
wait_ssh(){ for _ in $(seq 1 45); do gx 'true' >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }
field(){ "$B" describe vm "$1" 2>/dev/null | sed -n "s/^ *$2: *//p" | head -1 | tr -d ' '; }

cleanup(){
  "$B" vm destroy "$VM" >/dev/null 2>&1
  rm -f "$WORK/extra.raw"
  [ "${KEEP:-0}" = "1" ] || rm -rf "$WORK"
}

log "=== PREFLIGHT ==="
[ -x "$B" ] || die "delonix not found at '$B'"
[ -r /dev/kvm ] && [ -w /dev/kvm ] || die "/dev/kvm not rw"
command -v cloud-hypervisor >/dev/null || die "cloud-hypervisor not in PATH"
IMG="${DLX_IMAGE:-$("$B" vm image ls 2>/dev/null | awk '/cloud-init/{print $1; exit}')}"
[ -n "$IMG" ] || die "no cloud-init golden (set DLX_IMAGE)"
ssh-keygen -t ed25519 -N '' -f "$KEY" -C dlxhp >/dev/null 2>&1 || die "ssh-keygen failed"
log "delonix=$B  image=$IMG  ch=$(cloud-hypervisor --version 2>/dev/null)"

log "=== A. create + boot ==="
"$B" vm create "$VM" --backend cloud-hypervisor --disk "$IMG" --vcpus 2 --memory 2G \
     --hostname "$VM" --ssh-key "@$KEY.pub" >/dev/null 2>&1
[ "$(field "$VM" Backend)" = "cloud-hypervisor" ] && ok "backend is cloud-hypervisor" || bad "backend != cloud-hypervisor"
wait_ssh && ok "guest reachable over ssh" || { bad "guest NOT reachable"; exit 1; }
NET="$(field "$VM" Network)"; [ -n "$NET" ] || NET=default
log "network=$NET"

log "=== B. HOT DISK ADD (vm.add-disk) ==="
BLK0="$(gx 'ls /sys/block | grep -c .')"
log "guest block devices before: $BLK0 ($(gx 'ls /sys/block | tr "\n" " "'))"
truncate -s 256M "$WORK/extra.raw" || die "truncate failed"
# The update call exits non-zero (honest partial) — verdict is the guest.
# `format=raw`: the file is raw, and Cloud Hypervisor needs the type explicit
# (autodetected raw disables sector-0 writes — mkfs would fail ReadOnly).
"$B" vm update "$VM" --disk-add "$WORK/extra.raw,format=raw" >/dev/null 2>&1
RC=$?; log "vm update --disk-add exit=$RC (partial expected)"
sleep 3
BLK1="$(gx 'ls /sys/block | grep -c .')"
log "guest block devices after: $BLK1 ($(gx 'ls /sys/block | tr "\n" " "'))"
[ "${BLK1:-0}" -gt "${BLK0:-0}" ] && ok "guest sees a NEW block device ($BLK0 -> $BLK1)" || bad "guest sees NO new block device"
# Find the new virtio-blk device (the one not mounted as root), format, mount, R/W.
NEWDEV="$(gx 'for d in /sys/block/vd*; do n=$(basename $d); mount | grep -q "/dev/${n}" || echo $n; done | tail -1')"
log "candidate new device: ${NEWDEV:-none}"
if [ -n "$NEWDEV" ]; then
  gx "sudo mkfs.ext4 -q -F /dev/$NEWDEV" >/dev/null 2>&1 && ok "mkfs.ext4 on /dev/$NEWDEV" || bad "mkfs on /dev/$NEWDEV failed"
  gx "sudo mkdir -p /mnt/hp && sudo mount /dev/$NEWDEV /mnt/hp && echo dlx-hotplug-disk | sudo tee /mnt/hp/marker >/dev/null && sync" >/dev/null 2>&1
  RB="$(gx 'sudo cat /mnt/hp/marker 2>/dev/null')"
  [ "$RB" = "dlx-hotplug-disk" ] && ok "R/W on hot-added disk (read back '$RB')" || bad "R/W on hot-added disk failed (got '$RB')"
  gx "sudo umount /mnt/hp" >/dev/null 2>&1
else
  bad "no new virtio-blk device found to exercise"
fi

log "=== D. HOT DISK REMOVE (vm.remove-device) ==="
# `--disk-rm <PATH>` detaches the SAME extra disk by its source path. The CLI
# exits non-zero on the honest `partial` (no guest-confirm channel), so the
# verdict is the guest: the block device count drops back.
"$B" vm update "$VM" --disk-rm "$WORK/extra.raw" >/dev/null 2>&1
RC=$?; log "vm update --disk-rm exit=$RC (partial expected)"
sleep 3
BLK2="$(gx 'ls /sys/block | grep -c .')"
log "guest block devices after remove: $BLK2 ($(gx 'ls /sys/block | tr "\n" " "'))"
[ "${BLK2:-9}" -lt "${BLK1:-0}" ] && ok "guest LOST the hot-removed disk ($BLK1 -> $BLK2)" || bad "guest still shows the removed disk"

log "=== C. HOT NIC ADD (vm.add-net) ==="
IF0="$(gx 'ls /sys/class/net | grep -vc lo')"
log "guest NICs before: $IF0 ($(gx 'ls /sys/class/net | tr "\n" " "'))"
"$B" vm update "$VM" --nic-add "network:$NET" >/dev/null 2>&1
RC=$?; log "vm update --nic-add exit=$RC (partial expected)"
sleep 3
IF1="$(gx 'ls /sys/class/net | grep -vc lo')"
log "guest NICs after: $IF1 ($(gx 'ls /sys/class/net | tr "\n" " "'))"
[ "${IF1:-0}" -gt "${IF0:-0}" ] && ok "guest sees a NEW NIC ($IF0 -> $IF1)" || bad "guest sees NO new NIC"
# Bring the new NIC up + DHCP + prove traffic to its gateway.
NEWIF="$(gx 'for i in $(ls /sys/class/net | grep -v lo); do ip -o -4 addr show $i | grep -q inet || echo $i; done | tail -1')"
log "candidate new NIC: ${NEWIF:-none}"
if [ -n "$NEWIF" ]; then
  # The golden runs systemd-networkd (no dhclient): a DHCP .network for the new
  # link, reload, and read back the lease.
  gx "printf '[Match]\nName=$NEWIF\n[Network]\nDHCP=yes\n' | sudo -n tee /etc/systemd/network/10-$NEWIF.network >/dev/null && sudo -n networkctl reload" >/dev/null 2>&1
  IP2=""; for _ in $(seq 1 10); do
    IP2="$(gx "ip -o -4 addr show $NEWIF | awk '{print \$4}'")"
    [ -n "$IP2" ] && break; sleep 2
  done
  [ -n "$IP2" ] && ok "hot-added NIC $NEWIF got address $IP2 via DHCP" || bad "hot-added NIC got no DHCP lease"
  GW2="$(gx "ip route show dev $NEWIF | awk '/via/{print \$3; exit}'")"
  if [ -n "$GW2" ]; then
    if gx "ping -c3 -W2 -I $NEWIF $GW2" >/dev/null 2>&1; then
      ok "traffic on hot-added NIC: ping $GW2 via $NEWIF"
    else
      bad "no traffic on hot-added NIC (ping $GW2)"
    fi
  else
    bad "no gateway found for hot-added NIC"
  fi
else
  bad "no new NIC found to exercise"
fi

log "=== E. HOT NIC REMOVE (vm.remove-device) ==="
# `--nic-rm <MAC>` detaches the extra NIC by its pinned MAC; read the MAC off
# the guest's own new link so the proof never trusts a record we are testing.
RMMAC="$(gx "cat /sys/class/net/$NEWIF/address" 2>/dev/null)"
log "removing NIC with MAC ${RMMAC:-unknown}"
if [ -n "$RMMAC" ]; then
  "$B" vm update "$VM" --nic-rm "$RMMAC" >/dev/null 2>&1
  RC=$?; log "vm update --nic-rm exit=$RC (partial expected)"
  sleep 3
  IF2="$(gx 'ls /sys/class/net | grep -vc lo')"
  log "guest NICs after remove: $IF2 ($(gx 'ls /sys/class/net | tr "\n" " "'))"
  [ "${IF2:-9}" -lt "${IF1:-0}" ] && ok "guest LOST the hot-removed NIC ($IF1 -> $IF2)" || bad "guest still shows the removed NIC"
else
  bad "could not read the hot-added NIC's MAC to remove it"
fi

log "=== SUMMARY: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" -eq 0 ]
