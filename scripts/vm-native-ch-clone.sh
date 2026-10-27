#!/usr/bin/env bash
# =============================================================================
# Live proof — Cloud Hypervisor `vm clone` + `vm template` (capability catalog:
# VmClone, VmTemplate), mirroring the libvirt path (commit 2b4d4494).
#
# vm.clone — clones a RUNNING native CH microVM into a second, INDEPENDENT VM
# and proves the clone:
#   * owns its disk — a standalone qcow2 with NO backing file, so it survives
#     the source being destroyed (not a thin overlay of the source);
#   * owns its network identity — a MAC distinct from the source's;
#   * owns its cloud-init seed — its own `meta-data` names ITS hostname, and the
#     booted guest reports that hostname (a fresh seed built for the clone, not
#     the source's);
#   * boots as its own domain alongside the still-running source.
# Cloning a RUNNING source is the hard case: CH holds the overlay's qcow2 lock
# in a mode that rejects even a shared reader, so the copy opens the source with
# `file.locking=off` (see `clone_disk_independent`) — this script is the proof
# that path works end to end on the CH backend.
#
# vm.template — marks a CH VM as a template and proves:
#   * `vm start` on the template is REFUSED with DX-1501 (it does not boot);
#   * `vm clone` of the template instantiates a runnable VM that boots.
#
# Usage:  ./vm-native-ch-clone.sh [path-to-delonix]
# Env:    DLX_IMAGE  (default: first cloud-init golden via `vm image ls`)
#         KEEP=1     (keep the work dir and the VMs for inspection)
# =============================================================================
set -u
B="${1:-$(command -v delonix || echo "$HOME/.local/bin/delonix")}"
WORK="$(mktemp -d /tmp/dlxcl.XXXXXX)"
KEY="$WORK/key"
EVID="$WORK/evidence.txt"
PFX="dlxcl-$$"
SRC="$PFX-src"; CLONE="$PFX-clone"; TPL="$PFX-tpl"; INST="$PFX-inst"
PASS=0; FAIL=0
trap 'cleanup' EXIT
log(){ echo "$*" | tee -a "$EVID"; }
ok(){ log "PASS: $*"; PASS=$((PASS+1)); }
bad(){ log "FAIL: $*"; FAIL=$((FAIL+1)); }
die(){ log "ABORT: $*"; exit 2; }
field(){ "$B" describe vm "$1" 2>/dev/null | sed -n "s/.*$2: *//p" | head -1 | tr -d ' '; }
gx(){ timeout 30 "$B" vm ssh "$1" --identity "$KEY" -- "${@:2}" 2>/dev/null; }
wait_ssh(){ for _ in $(seq 1 45); do gx "$1" true >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }

cleanup(){
  for v in "$SRC" "$CLONE" "$TPL" "$INST"; do "$B" vm destroy "$v" >/dev/null 2>&1; done
  [ "${KEEP:-0}" = "1" ] || rm -rf "$WORK"
}

log "=== PREFLIGHT ==="
[ -x "$B" ] || die "delonix not found at '$B'"
[ -r /dev/kvm ] && [ -w /dev/kvm ] || die "/dev/kvm not rw"
command -v cloud-hypervisor >/dev/null || die "cloud-hypervisor not in PATH"
command -v qemu-img >/dev/null || die "qemu-img not in PATH"
IMG="${DLX_IMAGE:-$("$B" vm image ls 2>/dev/null | awk '/cloud-init/{print $1; exit}')}"
[ -n "$IMG" ] || die "no cloud-init golden (set DLX_IMAGE)"
ssh-keygen -t ed25519 -N '' -f "$KEY" -C dlxcl >/dev/null 2>&1 || die "ssh-keygen failed"
log "delonix=$B  image=$IMG  ch=$(cloud-hypervisor --version 2>/dev/null)"

log "=== A. create + boot a native CH source VM ==="
"$B" vm create "$SRC" --backend cloud-hypervisor --disk "$IMG" \
     --vcpus 1 --vcpus-max 4 --memory 512M --memory-max 1536M --hostname "$SRC" --ssh-key "@$KEY.pub" >/dev/null 2>&1
[ "$(field "$SRC" Backend)" = "cloud-hypervisor" ] && ok "source backend is cloud-hypervisor" || bad "source backend != cloud-hypervisor"
wait_ssh "$SRC" && ok "source guest reachable over ssh" || { bad "source guest NOT reachable"; exit 1; }
SRC_MAC="$(field "$SRC" MAC)"
SRC_OVL="$("$B" describe vm "$SRC" 2>/dev/null | sed -n 's/.*Overlay: *//p' | head -1 | tr -d ' ')"
VMS_DIR="$(dirname "$SRC_OVL")"
log "source MAC=$SRC_MAC overlay=$SRC_OVL"

log "=== B. clone the RUNNING source into an independent VM ==="
"$B" vm clone "$SRC" "$CLONE" >/dev/null 2>&1
RC=$?; [ "$RC" -eq 0 ] && ok "clone of a RUNNING CH source succeeded (lock bypassed)" || { bad "clone failed (exit=$RC)"; }
# Source stays up — the clone is a copy, not a move.
[ "$(field "$SRC" Status)" = "Running" ] && ok "source still Running after the clone" || bad "source no longer Running after clone"
# B1 — own disk: a standalone qcow2 with NO backing file.
CL_OVL="$("$B" describe vm "$CLONE" 2>/dev/null | sed -n 's/.*Overlay: *//p' | head -1 | tr -d ' ')"
if qemu-img info -U "$CL_OVL" 2>/dev/null | grep -qi '^backing file'; then
  bad "clone disk still has a backing file (not standalone)"
else
  ok "clone disk is a standalone qcow2 (no backing file): $CL_OVL"
fi
# B2 — own MAC.
CL_MAC="$(field "$CLONE" MAC)"
[ -n "$CL_MAC" ] && [ "$CL_MAC" != "$SRC_MAC" ] && ok "clone MAC distinct from source ($SRC_MAC -> $CL_MAC)" || bad "clone MAC not distinct ($SRC_MAC / $CL_MAC)"
# B3 — own seed: its own meta-data names ITS hostname.
CL_META="$VMS_DIR/$CLONE/meta-data"
if grep -q "local-hostname: $CLONE" "$CL_META" 2>/dev/null; then
  ok "clone has its own cloud-init seed (meta-data names '$CLONE')"
else
  bad "clone seed does not name '$CLONE' (meta-data at $CL_META)"
fi
# B4 — boots as its own domain with its own identity (guest reports clone name).
wait_ssh "$CLONE" && ok "clone guest reachable over ssh (booted independently)" || bad "clone guest NOT reachable"
GH="$(gx "$CLONE" hostname)"
[ "$GH" = "$CLONE" ] && ok "clone guest reports its own hostname '$GH'" || bad "clone guest hostname '$GH' != '$CLONE'"

log "=== C. mark a CH VM as a template; start refused; clone instantiates ==="
"$B" vm create "$TPL" --backend cloud-hypervisor --disk "$IMG" \
     --vcpus 1 --vcpus-max 4 --memory 512M --memory-max 1536M --hostname "$TPL" --ssh-key "@$KEY.pub" >/dev/null 2>&1
wait_ssh "$TPL" >/dev/null 2>&1
"$B" vm template "$TPL" >/dev/null 2>&1
RC=$?; [ "$RC" -eq 0 ] && ok "VM marked as template" || bad "vm template failed (exit=$RC)"
[ "$(field "$TPL" Status)" = "Stopped" ] && ok "template is Stopped (frozen base)" || bad "template not Stopped"
# C1 — start refused with DX-1501.
OUT="$("$B" vm start "$TPL" 2>&1)"; RC=$?
if [ "$RC" -ne 0 ] && printf '%s' "$OUT" | grep -q "DX-1501"; then
  ok "vm start on the template refused with DX-1501"
else
  bad "vm start on the template was not refused with DX-1501 (exit=$RC): $OUT"
fi
# C2 — clone the template into a runnable instance that boots.
"$B" vm clone "$TPL" "$INST" >/dev/null 2>&1
RC=$?; [ "$RC" -eq 0 ] && ok "template cloned into an instance (exit=0)" || bad "clone of template failed (exit=$RC)"
wait_ssh "$INST" && ok "instance from template booted and is reachable" || bad "instance from template NOT reachable"
# The instance is runnable (NOT a template): its own start is accepted idempotently.
"$B" vm start "$INST" >/dev/null 2>&1
[ "$?" -eq 0 ] && ok "instance is runnable (vm start accepted, not a template)" || bad "instance start refused (inherited the template mark?)"

log "=== SUMMARY: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" -eq 0 ]
