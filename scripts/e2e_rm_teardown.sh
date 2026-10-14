#!/usr/bin/env bash
# The battery's teardown (`e2e_rm`, `e2e_net_rm` in `e2e_teardown.sh`) proved
# against what it exists for: a `container rm -f` that answers DX-8101.
#
#   e2e_rm_teardown.sh <paths|converges|gives-up> <binary> <image> [prefix]
#
# Called by `scripts/e2e.sh`; runs on its own against any binary, with
# `DELONIX_ROOT` and `DELONIX_NET_RUNTIME_DIR` already isolated by the caller.
# Exits 0 (right), 1 (defect, with the measurement on stdout) or 77 (this host
# does not let it measure).
#
#   paths      the helpers against a fake binary: DX-8101 repeated until it
#              clears, class 4 silent, any other failure logged, a permanent
#              DX-8101 given up and said, a failed network removal said
#   converges  a real container whose exit is held past the first `rm -f`:
#              the teardown repeats, and removes it once the exit happens
#   gives-up   the same hold with a 1 s deadline: the teardown gives up, says
#              so, and the record is still there for the next `rm -f`
#
# The exit is held the way `e2e_rm_force_gave_up.sh` holds it (its header has
# the why): a `container exec` leaves a process in the container's pidns whose
# parent is outside it, and with that parent stopped the PID 1 cannot finish
# leaving, so `rm -f` waits its 30 s and answers DX-8101.
set -u
SCENARIO="${1:?scenario}"; REAL="${2:?binary}"; IMG="${3:?image}"; PFX="${4:-rmt$$}"
name="$PFX-rmt"
ep=""; execpid=""
work=$(mktemp -d) || exit 77
log() { printf '%s\n' "$*"; }

# Every `container rm -f` the helper makes goes through this shim, so the
# number of attempts is counted instead of inferred from the elapsed time.
cat >"$work/bin" <<SHIM
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$work/calls"
exec "$REAL" "\$@"
SHIM
chmod +x "$work/bin"
BIN="$work/bin"
# shellcheck source=scripts/e2e_teardown.sh
source "$(dirname "$0")/e2e_teardown.sh"

cleanup() {
  [[ -n "$ep" ]] && kill -CONT "$ep" 2>/dev/null
  [[ -n "$execpid" ]] && kill "$execpid" 2>/dev/null
  if [[ "$SCENARIO" != paths ]]; then
    local _
    for _ in 1 2 3 4; do
      timeout 120 "$REAL" container rm -f "$name" >/dev/null 2>&1
      "$REAL" container inspect "$name" >/dev/null 2>&1 || break
      sleep 5
    done
  fi
  rm -rf -- "$work"
}
die() { echo "$*"; cleanup; exit 1; }
cannot() { echo "$*"; cleanup; exit 77; }
rm_calls() { grep -c '^container rm -f' "$work/calls" 2>/dev/null || echo 0; }
exists() { "$REAL" container inspect "$name" >/dev/null 2>&1; }

if [[ "$SCENARIO" == paths ]]; then
  # A fake binary: `stuck` answers DX-8101 twice then clears, `forever` never
  # clears, `gone` is class 4, `bad` is any other failure.
  cat >"$work/fake" <<'FAKE'
#!/usr/bin/env bash
c="$FAKE_DIR/count-${*: -1}"; n=$(cat "$c" 2>/dev/null || echo 0); echo $((n+1)) >"$c"
case "${*: -1}" in
  stuck) (( n < 2 )) && { echo "error[DX-8101] still exiting"; exit 1; }; exit 0 ;;
  forever) echo "error[DX-8101] still exiting"; exit 1 ;;
  gone) echo "error[DX-4101] no such container"; exit 4 ;;
  bad) printf 'first line\nerror: something else broke\n'; exit 1 ;;
esac
FAKE
  chmod +x "$work/fake"
  export FAKE_DIR="$work"
  BIN="$work/fake"
  count() { cat "$work/count-$1" 2>/dev/null || echo 0; }
  out=$(E2E_RM_PAUSE=0 e2e_rm stuck gone)
  [[ -z "$out" ]] || die "a DX-8101 that clears, or a class 4, logged something: $out"
  [[ $(count stuck) -eq 3 ]] || die "DX-8101 twice then success took $(count stuck) calls, expected 3"
  out=$(E2E_RM_PAUSE=0 e2e_rm bad)
  [[ "$out" == *"rc=1: error: something else broke"* ]] || die "another failure was not logged with its last line: '$out'"
  [[ $(count bad) -eq 1 ]] || die "another failure was retried ($(count bad) calls)"
  out=$(E2E_RM_DEADLINE=0 E2E_RM_PAUSE=0 e2e_rm forever)
  [[ "$out" == *"forever continua a sair ao fim de 0 s"* ]] || die "a permanent DX-8101 was not given up and said: '$out'"
  out=$(e2e_net_rm gone bad)
  [[ "$out" != *" gone "* && "$out" == *"network rm bad → rc=1: error: something else broke"* ]] ||
    die "network removal: class 4 must be silent, another failure logged: '$out'"
  cleanup; exit 0
fi

[[ "$SCENARIO" == converges || "$SCENARIO" == gives-up ]] || die "unknown scenario: $SCENARIO"
"$REAL" container rm -f "$name" >/dev/null 2>&1
timeout 180 "$REAL" container run -d --name "$name" "$IMG" sleep 1000 >/dev/null 2>&1 ||
  cannot "the container did not start on this host"
init=$("$REAL" container inspect "$name" 2>/dev/null |
  python3 -c 'import json,sys;print(json.load(sys.stdin)[0].get("pid") or "")' 2>/dev/null)
[[ -n "$init" ]] || die "started with no pid in the record"

"$REAL" container exec "$name" sleep 902 >/dev/null 2>&1 &
execpid=$!
inside=""
for _ in $(seq 1 50); do
  for p in $(pgrep -x sleep); do
    [[ "$({ tr '\0' ' ' <"/proc/$p/cmdline"; } 2>/dev/null)" == "sleep 902 " ]] || continue
    [[ "$(readlink "/proc/$p/ns/pid")" == "$(readlink "/proc/$init/ns/pid")" ]] && inside=$p
  done
  [[ -n "$inside" ]] && break
  sleep 0.2
done
[[ -n "$inside" ]] || die "the exec left no process inside the container's pidns"
ep=$(awk '{print $4}' "/proc/$inside/stat")
[[ "$(readlink "/proc/$ep/ns/pid")" != "$(readlink "/proc/$init/ns/pid")" ]] ||
  cannot "the exec's parent is inside the pidns: this binary does not leave the exit held"
kill -STOP "$ep"

if [[ "$SCENARIO" == gives-up ]]; then
  out=$(E2E_RM_DEADLINE=1 e2e_rm "$name")
  [[ $(rm_calls) -ge 1 ]] || die "the teardown never called rm -f"
  exists || cannot "rm -f removed it despite the held exit: the exit was not held on this host"
  [[ "$out" == *"$name continua a sair ao fim de 1 s — DX-8101"* ]] ||
    die "the teardown gave up without saying so (calls=$(rm_calls)): '$out'"
  cleanup; exit 0
fi

# converges: the first rm -f waits its 30 s and answers DX-8101; by 40 s the
# teardown must still be at it, with a second attempt under way.
( e2e_rm "$name" ) >"$work/out" 2>&1 &
td=$!
sleep 40
kill -0 "$td" 2>/dev/null || {
  exists && die "the teardown returned while the exit was held (calls=$(rm_calls)): $(cat "$work/out")"
  cannot "rm -f removed it despite the held exit: the exit was not held on this host"
}
[[ $(rm_calls) -ge 2 ]] || die "40 s into a held exit the teardown made $(rm_calls) rm -f call(s), expected a retry"
kill -CONT "$ep"; ep=""
for _ in $(seq 1 90); do kill -0 "$td" 2>/dev/null || break; sleep 1; done
kill -0 "$td" 2>/dev/null && die "the teardown did not return within 90 s of the exit being released"
wait "$td"
exists && die "the teardown returned and the record is still there: $(cat "$work/out")"
grep -q '(limpeza:' "$work/out" && die "a teardown that converged logged a failure: $(cat "$work/out")"
calls=$(rm_calls)
cleanup
echo "removed after $calls rm -f call(s)"
exit 0
