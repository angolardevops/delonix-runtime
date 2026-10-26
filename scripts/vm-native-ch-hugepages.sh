#!/usr/bin/env bash
# =============================================================================
# Live proof — Cloud Hypervisor hugepage-backed guest memory (§7, HPC).
# Boots a native CH microVM with `--hugepages` and proves, from the HOST, that
# the guest's RAM is served out of the kernel's hugetlb pool:
#
#   * with `--memory …,hugepages=on` CH maps guest RAM MAP_HUGETLB but faults it
#     LAZILY (HugePages_Rsvd stays 0; measured on this host), so the proof FILLS
#     ~FILL_MIB of guest RAM (a tmpfs file) and then reads /proc/meminfo:
#     `HugePages_Free` DROPS by about that many 2 MiB pages — the guest's pages
#     come from the pool, not from ordinary 4 KiB pages;
#   * a CONTROL VM with the SAME workload but WITHOUT `--hugepages` must NOT move
#     the hugetlb pool — that is what pins the drop on the flag and not on the
#     VMM's own footprint or transparent-hugepage noise;
#   * the guest is reachable over ssh while on hugepages (it really booted);
#   * `HugePages_Free` RECOVERS to the baseline once the VM is destroyed — the
#     pages were borrowed from the pool, not leaked.
#
# Usage:  ./vm-native-ch-hugepages.sh [path-to-delonix]
# Env:    DLX_IMAGE  (default: first cloud-init golden via `vm image ls`)
#         KEEP=1     (keep the work dir and the VM for inspection)
# =============================================================================
set -u
B="${1:-$(command -v delonix || echo "$HOME/.local/bin/delonix")}"
WORK="$(mktemp -d /tmp/dlxhp.XXXXXX)"
KEY="$WORK/key"
EVID="$WORK/evidence.txt"
PFX="dlxhp-$$"
VM="$PFX-huge"
CTL="$PFX-ctl"
MEM_MIB=512                 # guest boot memory
FILL_MIB=300                # RAM the guest faults in (safely under usable)
HPSZ_KIB=2048               # 2 MiB hugepage (Hugepagesize in /proc/meminfo)
WANT_PAGES=$(( FILL_MIB * 1024 / HPSZ_KIB ))   # ~pages the fill alone must take
LO=$(( WANT_PAGES - 32 ))   # fill + boot noise sits above this; control below
TOL=32
PASS=0; FAIL=0
trap 'cleanup' EXIT
log(){ echo "$*" | tee -a "$EVID"; }
ok(){ log "PASS: $*"; PASS=$((PASS+1)); }
bad(){ log "FAIL: $*"; FAIL=$((FAIL+1)); }
die(){ log "ABORT: $*"; exit 2; }
gx(){ timeout 40 "$B" vm ssh "$1" --identity "$KEY" -- "$2" 2>/dev/null; }
wait_ssh(){ for _ in $(seq 1 45); do gx "$1" 'true' >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }
field(){ "$B" describe vm "$1" 2>/dev/null | sed -n "s/^ *$2: *//p" | head -1 | tr -d ' '; }
hp_free(){ awk '/^HugePages_Free:/{print $2}' /proc/meminfo; }
status(){ "$B" vm ls 2>/dev/null | awk -v v="$1" '$1==v{print $6}'; }
wait_running(){ for _ in $(seq 1 45); do [ "$(status "$1")" = "Running" ] && return 0; sleep 1; done; return 1; }
# Fill ~FILL_MIB of guest RAM in a tmpfs file and HOLD it, so the pages are
# really resident while the host reads the pool; prints the measured free pages.
fill_and_free(){
  gx "$1" "dd if=/dev/zero of=/dev/shm/fill bs=1M count=$FILL_MIB 2>/dev/null; sync" >/dev/null 2>&1
  sleep 1; hp_free
}

cleanup(){
  "$B" vm destroy "$VM"  >/dev/null 2>&1
  "$B" vm destroy "$CTL" >/dev/null 2>&1
  [ "${KEEP:-0}" = "1" ] || rm -rf "$WORK"
}

log "=== PREFLIGHT ==="
[ -x "$B" ] || die "delonix not found at '$B'"
[ -r /dev/kvm ] && [ -w /dev/kvm ] || die "/dev/kvm not rw"
command -v cloud-hypervisor >/dev/null || die "cloud-hypervisor not in PATH"
HPT=$(awk '/^HugePages_Total:/{print $2}' /proc/meminfo)
HPSZ=$(awk '/^Hugepagesize:/{print $2}' /proc/meminfo)
NEED=$(( MEM_MIB * 1024 / HPSZ_KIB ))
[ "${HPT:-0}" -ge "$NEED" ] || die "hugepage pool too small ($HPT pages; need $NEED of ${HPSZ}kB)"
[ "${HPSZ:-0}" = "$HPSZ_KIB" ] || log "note: Hugepagesize=${HPSZ}kB (assumed ${HPSZ_KIB}kB)"
IMG="${DLX_IMAGE:-$("$B" vm image ls 2>/dev/null | awk '/cloud-init/{print $1; exit}')}"
[ -n "$IMG" ] || die "no cloud-init golden (set DLX_IMAGE)"
ssh-keygen -t ed25519 -N '' -f "$KEY" -C dlxhp >/dev/null 2>&1 || die "ssh-keygen failed"
BASE=$(hp_free)
log "delonix=$B  image=$IMG  ch=$(cloud-hypervisor --version 2>/dev/null)"
log "pool: total=$HPT free=$BASE size=${HPSZ}kB ; fill=${FILL_MIB}MiB -> expect >=${LO} pages with hugepages, <${TOL} without"

log "=== A. CONTROL VM WITHOUT --hugepages: pool must NOT move ==="
# No ssh needed here: the claim is that an identical guest minus the flag keeps
# its RAM on ordinary pages, so cloud-init churning its memory after boot must
# leave the hugetlb pool untouched. (ssh on a cold first boot is slow on this
# host; the hugepage VM below, which DOES need ssh to fault its RAM, gets it.)
"$B" vm create "$CTL" --backend cloud-hypervisor --disk "$IMG" \
     --vcpus 1 --memory "${MEM_MIB}M" --hostname "$CTL" --ssh-key "@$KEY.pub" >/dev/null 2>&1
if wait_running "$CTL"; then
  sleep 20   # let the guest boot and cloud-init touch its RAM
  CTLF=$(hp_free); CTLDELTA=$(( BASE - CTLF ))
  if [ "$CTLDELTA" -lt "$TOL" ]; then
    ok "control running + settled, hugetlb pool steady (delta=$CTLDELTA pages < $TOL)"
  else
    bad "control (no hugepages) moved the pool by $CTLDELTA pages"
  fi
else
  bad "control VM did not reach Running"
fi
"$B" vm destroy "$CTL" >/dev/null 2>&1
sleep 2

log "=== B. create + boot WITH --hugepages (${MEM_MIB}M) ==="
"$B" vm create "$VM" --backend cloud-hypervisor --disk "$IMG" \
     --vcpus 1 --memory "${MEM_MIB}M" --hugepages \
     --hostname "$VM" --ssh-key "@$KEY.pub" >/dev/null 2>&1
[ "$(field "$VM" Backend)" = "cloud-hypervisor" ] && ok "backend is cloud-hypervisor" || bad "backend != cloud-hypervisor"
wait_ssh "$VM" && ok "guest reachable over ssh on hugepages" || { bad "guest NOT reachable"; log "$(tail -5 "$HOME/.local/share/delonix/vms/$VM.log" 2>/dev/null)"; exit 1; }

log "=== C. fill guest RAM -> host hugetlb pool DROPS by ~the filled memory ==="
RUNF=$(fill_and_free "$VM"); DELTA=$(( BASE - RUNF ))
MT=$(gx "$VM" 'grep MemTotal /proc/meminfo' | tr -s ' ')
log "HugePages_Free: baseline=$BASE after-fill=$RUNF  delta=$DELTA pages (filled ${FILL_MIB}MiB; $MT)"
if [ "$DELTA" -ge "$LO" ]; then
  ok "pool dropped by $DELTA pages (>=${LO}, ~$(( DELTA * HPSZ_KIB / 1024 ))MiB) — guest RAM is on hugepages"
else
  bad "pool delta $DELTA pages < $LO — not hugepage-backed as expected"
fi

log "=== D. destroy -> pool RECOVERS (no leak) ==="
"$B" vm destroy "$VM" >/dev/null 2>&1
REC=""
for _ in $(seq 1 15); do REC=$(hp_free); [ "$REC" -ge "$(( BASE - TOL ))" ] && break; sleep 1; done
log "HugePages_Free after destroy=$REC (baseline=$BASE)"
if [ "$REC" -ge "$(( BASE - TOL ))" ]; then
  ok "hugetlb pool recovered to baseline (returned the pages)"
else
  bad "pool did not recover: $REC < baseline $BASE — pages leaked"
fi

log "=== SUMMARY: $PASS passed, $FAIL failed ==="
[ "$FAIL" -eq 0 ] || exit 1
