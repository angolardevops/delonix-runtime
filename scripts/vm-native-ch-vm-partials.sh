#!/usr/bin/env bash
# =============================================================================
# Live proof — Cloud Hypervisor VM compatibility partials.
# Boots ONE native CH microVM (pinned vCPUs, cloud-init SSH) and proves, live,
# four capabilities the CH provider reports as `partial` for lack of a battery:
#
#   vm.cpu.pinning   — the VMM's vCPU threads are pinned to the host CPUs the
#                      `--cpu-affinity` list named (read from /proc on the HOST).
#   vm.ip.observed   — `vm create --wait` confirms the guest answers ARP at the
#                      address the engine reports (the `sdn_reachable` path), and
#                      the guest's OWN address equals the reported one.
#   vm.console.serial— `vm console` bridges a tty to the VMM serial socket; the
#                      guest echoes/answers through it (driven under a pty).
#   vm.antispoof     — the guest reaches its gateway from its leased IP, and a
#                      FORGED source IP on the same link is dropped by `dlxspoof`.
#
# Usage:  ./vm-native-ch-vm-partials.sh [path-to-delonix]
# Env:    DLX_IMAGE  (default: first cloud-init golden via `vm image ls`)
#         KEEP=1     (keep the work dir and the VM for inspection)
# =============================================================================
set -u
B="${1:-$(command -v delonix || echo "$HOME/.local/bin/delonix")}"
WORK="$(mktemp -d /tmp/dlxvmp.XXXXXX)"
KEY="$WORK/key"
EVID="$WORK/evidence.txt"
PFX="dlxvmp-$$"
VM="$PFX-a"
PIN="0-1"           # host CPUs to pin the 2 vCPUs to
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
command -v python3 >/dev/null || die "python3 not in PATH (needed to drive the console pty)"
NCPU="$(nproc)"; [ "$NCPU" -ge 2 ] || die "need >= 2 host CPUs to pin to '$PIN'"
# SKIP_PIN=1 drops --cpu-affinity (and the cpu.pinning section) — used to
# validate the other three caps against a binary without the affinity fix.
AFFARGS=(--cpu-affinity "$PIN"); [ "${SKIP_PIN:-0}" = "1" ] && AFFARGS=()
IMG="${DLX_IMAGE:-$("$B" vm image ls 2>/dev/null | awk '/cloud-init/{print $1; exit}')}"
[ -n "$IMG" ] || die "no cloud-init golden (set DLX_IMAGE)"
ssh-keygen -t ed25519 -N '' -f "$KEY" -C dlxvmp >/dev/null 2>&1 || die "ssh-keygen failed"
log "delonix=$B  image=$IMG  ch=$(cloud-hypervisor --version 2>/dev/null)  ncpu=$NCPU  pin=$PIN"

# -----------------------------------------------------------------------------
log "=== A. create (pinned vCPUs) + boot, with --wait (vm.ip.observed path) ==="
# `--wait` drives wait_for_boot -> sdn_reachable: ARP-probe the SDN and read
# `ip neigh` REACHABLE for the (reported IP, guest MAC). Exit 0 means the guest
# answered AT the address the engine reports.
"$B" vm create "$VM" --backend cloud-hypervisor --disk "$IMG" --vcpus 2 --memory 2G \
     "${AFFARGS[@]}" --hostname "$VM" --ssh-key "@$KEY.pub" --wait >/dev/null 2>&1
WAITRC=$?
log "vm create --wait exit=$WAITRC"
[ "$(field "$VM" Backend)" = "cloud-hypervisor" ] && ok "backend is cloud-hypervisor" || bad "backend != cloud-hypervisor"
[ "$WAITRC" -eq 0 ] && ok "vm.ip.observed: --wait confirmed the guest answers ARP at the reported address" \
                    || bad "--wait did not confirm reachability (exit $WAITRC)"
wait_ssh || { bad "guest NOT reachable over ssh"; log "=== SUMMARY: PASS=$PASS FAIL=$((FAIL+1)) ==="; exit 1; }
RIP="$(field "$VM" IP)"
GIP="$(gx "ip -4 -o addr show scope global | awk '{print \$4}' | cut -d/ -f1 | head -1")"
log "reported IP='$RIP'  guest's own IP='$GIP'"
[ -n "$RIP" ] && [ "$RIP" = "$GIP" ] && ok "vm.ip.observed: reported IP equals the guest's observed address ($RIP)" \
                                     || bad "reported IP ($RIP) != guest address ($GIP)"

# -----------------------------------------------------------------------------
log "=== B. vm.cpu.pinning — HOST-side vCPU thread affinity ==="
if [ "${SKIP_PIN:-0}" = "1" ]; then
  log "SKIP: cpu.pinning section (SKIP_PIN=1)"
else
CHPID="$(pgrep -af cloud-hypervisor 2>/dev/null | grep -- "$VM.sock" | awk '{print $1}' | head -1)"
log "cloud-hypervisor pid for $VM: ${CHPID:-none}"
if [ -z "$CHPID" ] || [ ! -d "/proc/$CHPID/task" ]; then
  bad "could not locate the VMM process to read its vCPU threads"
else
  # Canonicalise the expected set: "0-1" -> "0,1" sorted list of allowed CPUs.
  want="$(python3 - "$PIN" <<'PY'
import sys
s=sys.argv[1]; out=set()
for p in s.split(','):
    if '-' in p: a,b=p.split('-'); out.update(range(int(a),int(b)+1))
    else: out.add(int(p))
print(','.join(str(x) for x in sorted(out)))
PY
)"
  nvcpu=0; nmatch=0; seen=""
  for t in /proc/"$CHPID"/task/*; do
    comm="$(cat "$t/comm" 2>/dev/null)"
    case "$comm" in
      vcpu*)
        nvcpu=$((nvcpu+1))
        allow="$(sed -n 's/^Cpus_allowed_list:\s*//p' "$t/status" 2>/dev/null)"
        got="$(python3 - "$allow" <<'PY'
import sys
s=sys.argv[1].strip(); out=set()
for p in s.split(','):
    p=p.strip()
    if not p: continue
    if '-' in p: a,b=p.split('-'); out.update(range(int(a),int(b)+1))
    else: out.add(int(p))
print(','.join(str(x) for x in sorted(out)))
PY
)"
        seen="$seen $comm=[$allow]"
        [ "$got" = "$want" ] && nmatch=$((nmatch+1))
        ;;
    esac
  done
  log "vCPU threads: $nvcpu; matching pin ($want):$seen"
  [ "$nvcpu" -ge 1 ] && [ "$nmatch" -eq "$nvcpu" ] \
    && ok "vm.cpu.pinning: all $nvcpu vCPU threads pinned to host CPUs {$want}" \
    || bad "vCPU thread affinity does not match the pin (matched $nmatch/$nvcpu)"
fi
fi

# -----------------------------------------------------------------------------
log "=== C. vm.console.serial — tty<->serial-socket bridge ==="
# Drive `vm console` under a pty: the bridge sends a \r on connect (nudges the
# getty), we type a marker and read the guest's echo/answer back off the serial
# line, then Ctrl-D to detach.
CONLOG="$WORK/console.out"
MARK="DLXSERIAL$$"
python3 - "$B" "$VM" "$MARK" "$CONLOG" <<'PY'
import os, pty, select, sys, time
b, vm, mark, out = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
pid, fd = pty.fork()
if pid == 0:
    os.environ["DELONIX_CONSOLE_ESCAPE"] = "^D"
    os.execvp(b, [b, "vm", "console", vm])
buf = b""
def pump(deadline):
    global buf
    while time.time() < deadline:
        r,_,_ = select.select([fd], [], [], 0.4)
        if r:
            try: d = os.read(fd, 4096)
            except OSError: break
            if not d: break
            buf += d
pump(time.time()+4)          # connect banner + getty prompt
try: os.write(fd, b"\r")     # nudge the line
except OSError: pass
pump(time.time()+2)
try: os.write(fd, mark.encode()+b"\r")   # type a marker the serial line echoes
except OSError: pass
pump(time.time()+3)
try: os.write(fd, b"\x04")   # Ctrl-D detaches the bridge
except OSError: pass
pump(time.time()+2)
with open(out, "wb") as f:
    f.write(buf)
try: os.kill(pid, 9)
except OSError: pass
PY
CSIZE="$(wc -c < "$CONLOG" 2>/dev/null || echo 0)"
log "console captured $CSIZE bytes"
if grep -qa -e "$MARK" -e "login:" -e "$VM" "$CONLOG" 2>/dev/null; then
  HIT="$(grep -aoE -e "$MARK" -e "login:" -e "$VM" "$CONLOG" 2>/dev/null | sort -u | tr '\n' ' ')"
  ok "vm.console.serial: guest answered over the serial socket (saw: $HIT)"
else
  bad "no guest output over the serial console (captured $CSIZE bytes)"
fi

# -----------------------------------------------------------------------------
log "=== D. vm.antispoof — forged source IP is dropped at the bridge ==="
NIC="$(gx "ls /sys/class/net | grep -v lo | head -1")"
GW="$(gx "ip route show default | awk '/via/{print \$3; exit}'")"
log "guest NIC=$NIC gateway=$GW"
if [ -z "$NIC" ] || [ -z "$GW" ]; then
  bad "could not determine the guest NIC/gateway to exercise anti-spoofing"
else
  # Baseline: the LEASED source reaches the gateway.
  if gx "ping -c2 -W2 $GW" >/dev/null 2>&1; then
    ok "antispoof baseline: guest reaches gateway $GW from its leased IP"
  else
    bad "guest cannot reach its gateway even from the leased IP (environment)"
  fi
  # Forge: a second, UNLEASED address on the same link, pinged with that source.
  # dlxspoof pins the port to the lease only, so this must be dropped.
  SPOOF="$(python3 - "$RIP" <<'PY'
import sys
ip=sys.argv[1].split('.')
if len(ip)==4:
    last=int(ip[3]); fake=250 if last!=250 else 240
    print('.'.join(ip[:3]+[str(fake)]))
PY
)"
  log "forged source IP=$SPOOF"
  gx "sudo -n ip addr add $SPOOF/24 dev $NIC" >/dev/null 2>&1
  if gx "ping -c2 -W2 -I $SPOOF $GW" >/dev/null 2>&1; then
    bad "antispoof: FORGED source $SPOOF reached the gateway (not dropped)"
  else
    ok "vm.antispoof: forged source $SPOOF was dropped at the bridge (no reply)"
  fi
  gx "sudo -n ip addr del $SPOOF/24 dev $NIC" >/dev/null 2>&1
fi

log "=== SUMMARY: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" -eq 0 ]
