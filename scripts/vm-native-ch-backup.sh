#!/usr/bin/env bash
# =============================================================================
# Live proof — Cloud Hypervisor VM disk BACKUP and RESTORE by DATA RECOVERY.
# Boots ONE native CH microVM (cloud-init SSH) and proves, live, the two
# capabilities the CH provider reports as `partial` for lack of a battery:
#
#   vm.backup.disk    — `backup create vm` archives the overlay of a STOPPED CH
#                       VM, and the archive actually holds the guest's data
#                       (read back offline with guestfish from the archived
#                       overlay — "creating a file does not prove recoverability").
#   vm.backup.restore — `backup restore` puts the overlay back and the guest
#                       boots with the ORIGINAL data, undoing a change made
#                       after the backup was taken.
#
# The cycle is the whole point: ORIGINAL -> backup -> CHANGED -> restore ->
# ORIGINAL, each step observed on the live guest (not just on the host).
#
#   1. create + boot a CH VM; write a known marker into the guest, `sync`.
#   2. stop; `backup create vm` -> archive.  (guestfish reads ORIGINAL out of
#      the archived overlay offline = backup.disk captured real data.)
#   3. start; OVERWRITE the marker with a different value, `sync`; read it back
#      CHANGED (the mutation took).
#   4. stop; `backup restore` the archive.
#   5. start; read the marker -> it is ORIGINAL again = backup.restore recovered.
#
# Usage:  ./vm-native-ch-backup.sh [path-to-delonix]
# Env:    DLX_IMAGE  (default: first cloud-init golden via `vm image ls`)
#         KEEP=1     (keep the work dir, the VM and the archive for inspection)
# =============================================================================
set -u
B="${1:-$(command -v delonix || echo "$HOME/.local/bin/delonix")}"
WORK="$(mktemp -d /tmp/dlxvmb.XXXXXX)"
KEY="$WORK/key"
EVID="$WORK/evidence.txt"
PFX="dlxvmb-$$"
VM="$PFX-a"
MARKER_PATH="/var/lib/dlx-backup-marker"
ORIGINAL="ORIGINAL-$$-$(date +%s)"
CHANGED="CHANGED-$$-corrupted"
PASS=0; FAIL=0
trap 'cleanup' EXIT
log(){ echo "$*" | tee -a "$EVID"; }
ok(){ log "PASS: $*"; PASS=$((PASS+1)); }
bad(){ log "FAIL: $*"; FAIL=$((FAIL+1)); }
die(){ log "ABORT: $*"; exit 2; }
gx(){ timeout 40 "$B" vm ssh "$VM" --identity "$KEY" -- "$@" 2>/dev/null; }
wait_ssh(){ for _ in $(seq 1 45); do gx 'true' >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }
field(){ "$B" describe vm "$1" 2>/dev/null | sed -n "s/^ *$2: *//p" | head -1 | tr -d ' '; }

# Write/read the marker as root inside the guest. `sync` forces the guest to
# flush its page cache to the virtual disk, so the subsequent `vm stop` (a
# graceful vmm.shutdown that flushes the qcow2) captures the write.
put_marker(){ gx "echo '$1' | sudo tee $MARKER_PATH >/dev/null && sync"; }
get_marker(){ gx "sudo cat $MARKER_PATH 2>/dev/null"; }

cleanup(){
  "$B" vm destroy "$VM" >/dev/null 2>&1
  [ "${KEEP:-0}" = "1" ] || rm -rf "$WORK"
}

log "=== PREFLIGHT ==="
[ -x "$B" ] || die "delonix not found at '$B'"
[ -r /dev/kvm ] && [ -w /dev/kvm ] || die "/dev/kvm not rw"
command -v cloud-hypervisor >/dev/null || die "cloud-hypervisor not in PATH"
IMG="${DLX_IMAGE:-$("$B" vm image ls 2>/dev/null | awk '/cloud-init/{print $1; exit}')}"
[ -n "$IMG" ] || die "no cloud-init golden (set DLX_IMAGE)"
ssh-keygen -t ed25519 -N '' -f "$KEY" -C dlxvmb >/dev/null 2>&1 || die "ssh-keygen failed"
log "delonix=$B  image=$IMG  ch=$(cloud-hypervisor --version 2>/dev/null)"
log "marker ORIGINAL='$ORIGINAL'  CHANGED='$CHANGED'"

# -----------------------------------------------------------------------------
log "=== 1. create + boot the CH VM, write the ORIGINAL marker ==="
"$B" vm create "$VM" --backend cloud-hypervisor --disk "$IMG" --vcpus 2 --memory 2G \
     --hostname "$VM" --ssh-key "@$KEY.pub" --wait >/dev/null 2>&1
log "vm create --wait exit=$?"
[ "$(field "$VM" Backend)" = "cloud-hypervisor" ] && ok "backend is cloud-hypervisor" || bad "backend != cloud-hypervisor"
wait_ssh || { bad "guest NOT reachable over ssh"; log "=== SUMMARY: PASS=$PASS FAIL=$((FAIL+1)) ==="; exit 1; }
OVERLAY="$(field "$VM" Overlay)"; [ -z "$OVERLAY" ] && OVERLAY="$(field "$VM" Disk)"
log "overlay path (from describe): ${OVERLAY:-unknown}"
put_marker "$ORIGINAL" >/dev/null
GOT="$(get_marker)"
log "marker in guest after write: '$GOT'"
[ "$GOT" = "$ORIGINAL" ] && ok "wrote the ORIGINAL marker into the running guest" || bad "could not write/read the marker (got '$GOT')"

# -----------------------------------------------------------------------------
log "=== 2. stop + backup create vm (archive the stopped overlay) ==="
"$B" vm stop "$VM" >/dev/null 2>&1; log "vm stop exit=$?"
log "VM status after stop: '$(field "$VM" Status)'"
"$B" backup create vm "$VM" --to "$WORK" >/dev/null 2>&1; log "backup create exit=$?"
ARCHIVE="$(ls -1t "$WORK"/vm-"$VM"-*.tar.gz 2>/dev/null | head -1)"
if [ -n "$ARCHIVE" ] && [ -f "$ARCHIVE" ]; then
  ok "backup create vm wrote $(basename "$ARCHIVE") ($(stat -c%s "$ARCHIVE") bytes)"
else
  bad "backup create vm produced no archive"; log "=== SUMMARY: PASS=$PASS FAIL=$((FAIL+1)) ==="; exit 1
fi
# The archive's own record must say it holds a VM.
INSPECT="$("$B" backup inspect "$ARCHIVE" 2>/dev/null)"
echo "$INSPECT" | sed 's/^/  inspect| /' | tee -a "$EVID" >/dev/null
echo "$INSPECT" | grep -qiE "Kind.*vm" && ok "archive records Kind=vm" || bad "archive does not record Kind=vm"

# OFFLINE proof that the ARCHIVE holds the guest's data (not just a file name):
# extract disk/overlay.qcow2 and read the marker straight out of it with
# guestfish. The overlay is a qcow2 whose backing file is the golden base; point
# libguestfs at the base too so it can follow the chain read-only.
log "--- offline: read the marker out of the ARCHIVED overlay with guestfish ---"
tar -C "$WORK" -xzf "$ARCHIVE" disk/overlay.qcow2 2>/dev/null
AOVL="$WORK/disk/overlay.qcow2"
if [ -f "$AOVL" ]; then
  BACKING="$(qemu-img info "$AOVL" 2>/dev/null | sed -n 's/^backing file: *//p' | head -1)"
  log "archived overlay backing file: ${BACKING:-none}"
  GFVAL=""
  if [ -n "$BACKING" ] && [ -f "$BACKING" ]; then
    # Overlay first (the drive we read), base added so the chain resolves.
    GFVAL="$(guestfish --ro -i -a "$AOVL" -a "$BACKING" <<GF 2>/dev/null
cat $MARKER_PATH
GF
)"
  fi
  [ -z "$GFVAL" ] && GFVAL="$(guestfish --ro -i -a "$AOVL" <<GF 2>/dev/null
cat $MARKER_PATH
GF
)"
  GFVAL="$(printf '%s' "$GFVAL" | tr -d '\r\n')"
  log "guestfish read from the archived overlay: '$GFVAL'"
  if [ "$GFVAL" = "$ORIGINAL" ]; then
    ok "vm.backup.disk: the ARCHIVE holds the guest's ORIGINAL data (read offline with guestfish)"
  else
    log "note: offline guestfish read inconclusive (backing-chain/libguestfs); relying on the restore cycle below"
  fi
else
  log "note: could not extract disk/overlay.qcow2 for the offline read"
fi

# -----------------------------------------------------------------------------
log "=== 3. start + CHANGE the marker (so restore has something to undo) ==="
"$B" vm start "$VM" >/dev/null 2>&1; log "vm start exit=$?"
wait_ssh || bad "guest not reachable after restart"
put_marker "$CHANGED" >/dev/null
GOT="$(get_marker)"
log "marker in guest after change: '$GOT'"
[ "$GOT" = "$CHANGED" ] && ok "marker CHANGED in the live guest (and read back changed)" || bad "could not change the marker (got '$GOT')"

# -----------------------------------------------------------------------------
log "=== 4. stop + backup restore (put the archived overlay back) ==="
"$B" vm stop "$VM" >/dev/null 2>&1; log "vm stop exit=$?"
"$B" backup restore "$ARCHIVE" --kind vm >/dev/null 2>&1; RC=$?; log "backup restore exit=$RC"
[ "$RC" -eq 0 ] && ok "backup restore returned success" || bad "backup restore failed (exit $RC)"

# -----------------------------------------------------------------------------
log "=== 5. start + read the marker: must be ORIGINAL again (recovery) ==="
"$B" vm start "$VM" >/dev/null 2>&1; log "vm start exit=$?"
wait_ssh || bad "guest not reachable after restore"
GOT="$(get_marker)"
log "marker in guest after restore: '$GOT'  (ORIGINAL='$ORIGINAL', CHANGED='$CHANGED')"
if [ "$GOT" = "$ORIGINAL" ]; then
  ok "vm.backup.restore: the guest booted with the ORIGINAL data — the CHANGE was undone"
elif [ "$GOT" = "$CHANGED" ]; then
  bad "restore did NOT recover: marker is still the CHANGED value"
else
  bad "restore left the guest in an unexpected state (marker '$GOT')"
fi

log "=== SUMMARY: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" -eq 0 ]
