#!/usr/bin/env bash
# =============================================================================
# Live proof — Cloud Hypervisor hot CPU/MEMORY add (ADR-0068 Phase 1, §7).
# Boots a native CH microVM with hotplug headroom declared at create
# (`--vcpus-max`/`--memory-max`), then over `vm update` grows the live VM and
# proves INSIDE the guest (over `vm ssh`) that:
#   * the hot-added vCPU comes online — `nproc` rises, and the new CPU runs work;
#   * the hot-added memory lands — `MemTotal` in /proc/meminfo rises.
#
# `vm update --vcpus` exits 124 on this backend (honest `partial`: CH has no
# in-band channel to confirm the guest onlined the vCPU — ADR-0068 D4/Q5), so
# the CPU verdict is NEVER the CLI exit code — it is what the guest reports.
# Memory add CAN reach `Complete` (the virtio-mem driver claims the region).
#
# REMOVE is deliberately NOT exercised: hot CPU/memory removal from a running
# guest is unsafe to force and is routed through cold `vm resize` on a stopped
# VM (not-implemented here; see the capability catalog).
#
# Usage:  ./vm-native-ch-cpumem.sh [path-to-delonix]
# Env:    DLX_IMAGE  (default: first cloud-init golden via `vm image ls`)
#         KEEP=1     (keep the work dir and the VM for inspection)
# =============================================================================
set -u
B="${1:-$(command -v delonix || echo "$HOME/.local/bin/delonix")}"
WORK="$(mktemp -d /tmp/dlxcm.XXXXXX)"
KEY="$WORK/key"
EVID="$WORK/evidence.txt"
PFX="dlxcm-$$"
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
  [ "${KEEP:-0}" = "1" ] || rm -rf "$WORK"
}

log "=== PREFLIGHT ==="
[ -x "$B" ] || die "delonix not found at '$B'"
[ -r /dev/kvm ] && [ -w /dev/kvm ] || die "/dev/kvm not rw"
command -v cloud-hypervisor >/dev/null || die "cloud-hypervisor not in PATH"
IMG="${DLX_IMAGE:-$("$B" vm image ls 2>/dev/null | awk '/cloud-init/{print $1; exit}')}"
[ -n "$IMG" ] || die "no cloud-init golden (set DLX_IMAGE)"
ssh-keygen -t ed25519 -N '' -f "$KEY" -C dlxcm >/dev/null 2>&1 || die "ssh-keygen failed"
log "delonix=$B  image=$IMG  ch=$(cloud-hypervisor --version 2>/dev/null)"

log "=== A. create + boot (1 vCPU / 512M, headroom to 4 vCPU / 1536M) ==="
"$B" vm create "$VM" --backend cloud-hypervisor --disk "$IMG" \
     --vcpus 1 --vcpus-max 4 --memory 512M --memory-max 1536M \
     --hostname "$VM" --ssh-key "@$KEY.pub" >/dev/null 2>&1
[ "$(field "$VM" Backend)" = "cloud-hypervisor" ] && ok "backend is cloud-hypervisor" || bad "backend != cloud-hypervisor"
wait_ssh && ok "guest reachable over ssh" || { bad "guest NOT reachable"; exit 1; }

log "=== B. HOT CPU ADD (vm.resize desired_vcpus 1 -> 2) ==="
NPROC0="$(gx 'nproc')"
CPUS0="$(gx 'ls -d /sys/devices/system/cpu/cpu[0-9]* | wc -l')"
log "guest before: nproc=$NPROC0  cpu dirs=$CPUS0"
# The update call exits 124 (honest partial) — the verdict is the guest.
"$B" vm update "$VM" --vcpus 2 >/dev/null 2>&1
RC=$?; log "vm update --vcpus 2 exit=$RC (124 partial expected on CH)"
# Give udev a moment; if the guest did not auto-online the new vCPU, online it.
sleep 3
NPROC1="$(gx 'nproc')"
if [ "${NPROC1:-0}" -le "${NPROC0:-0}" ]; then
  log "new vCPU not auto-onlined; onlining offline CPUs in guest"
  gx 'for c in /sys/devices/system/cpu/cpu[0-9]*/online; do [ "$(cat $c)" = 0 ] && echo 1 | sudo tee $c >/dev/null; done' >/dev/null 2>&1
  sleep 1
  NPROC1="$(gx 'nproc')"
fi
log "guest after: nproc=$NPROC1"
[ "${NPROC1:-0}" -gt "${NPROC0:-0}" ] && ok "guest onlined the hot-added vCPU (nproc $NPROC0 -> $NPROC1)" || bad "guest nproc did not rise ($NPROC0 -> $NPROC1)"
# Prove the new CPU actually runs work: pin a busy loop to the last CPU id.
LAST=$(( NPROC1 - 1 ))
RAN="$(gx "taskset -c $LAST sh -c 'for i in \$(seq 1 200000); do :; done; echo ran-on-cpu-$LAST'")"
[ "$RAN" = "ran-on-cpu-$LAST" ] && ok "work ran pinned to the hot-added CPU $LAST" || bad "could not run work on CPU $LAST (got '$RAN')"
# The engine refuses a DECREASE on a running VM (hot-remove is not wired).
"$B" vm update "$VM" --vcpus 1 >/dev/null 2>&1
RC=$?; [ "$RC" -ne 0 ] && ok "hot CPU DECREASE refused on a running VM (exit=$RC, routed to cold resize)" || bad "hot CPU decrease was NOT refused"

log "=== C. HOT MEMORY ADD (vm.resize desired_ram 512M -> 1024M) ==="
MEM0="$(gx 'grep MemTotal /proc/meminfo | awk "{print \$2}"')"
log "guest before: MemTotal=${MEM0} kB"
"$B" vm update "$VM" --memory 1024M >/dev/null 2>&1
RC=$?; log "vm update --memory 1024M exit=$RC"
sleep 3
MEM1="$(gx 'grep MemTotal /proc/meminfo | awk "{print \$2}"')"
if [ "${MEM1:-0}" -le "${MEM0:-0}" ]; then
  log "virtio-mem blocks not auto-onlined; onlining offline memory blocks in guest"
  gx 'for s in /sys/devices/system/memory/memory*/state; do [ "$(cat $s)" = offline ] && echo online | sudo tee $s >/dev/null; done' >/dev/null 2>&1
  sleep 2
  MEM1="$(gx 'grep MemTotal /proc/meminfo | awk "{print \$2}"')"
fi
log "guest after: MemTotal=${MEM1} kB"
[ "${MEM1:-0}" -gt "${MEM0:-0}" ] && ok "guest MemTotal rose with the hot-added memory (${MEM0} -> ${MEM1} kB)" || bad "guest MemTotal did not rise (${MEM0} -> ${MEM1} kB)"
# The reported record memory should also reflect the grow.
MEMREC="$(field "$VM" Memory)"
log "record memory now: ${MEMREC}"
# Hot memory DECREASE on a running VM is refused (routed to cold resize).
"$B" vm update "$VM" --memory 512M >/dev/null 2>&1
RC=$?; [ "$RC" -ne 0 ] && ok "hot MEMORY DECREASE refused on a running VM (exit=$RC, routed to cold resize)" || bad "hot memory decrease was NOT refused"

log "=== SUMMARY: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" -eq 0 ]
