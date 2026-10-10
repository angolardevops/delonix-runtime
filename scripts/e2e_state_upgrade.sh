#!/usr/bin/env bash
# A state root written by the previous release is opened by this build, and
# nothing in it changes meaning (plan 65 F0.6, completion plan N5).
#
#   e2e_state_upgrade.sh <old-bin-dir> <new-bin-dir> <work-dir> [image]
#
# <old-bin-dir> holds the PUBLISHED binaries of the previous release (delonix
# and its four siblings, verified against SHA256SUMS and the minisign key of
# the repository — not built here, or the test would compare a build with
# itself). <new-bin-dir> holds this build. <work-dir> gets its own state root
# and its own network runtime dir: both, or two holders would share the
# per-user sockets (AGENTS.md, «Meia-isolação é pior que nenhuma»).
#
# The upgrade is the in-place one an operator does: the old binary leaves a
# stack with a running container, a stopped one, a volume with data, a
# network and a secret; the new binary is then pointed at the same root,
# WITHOUT restarting the network infra the old one started. What must hold:
# the stack plans no change (`--detailed-exitcode` 0), every resource is still
# listed, the volume still has its bytes, the secret its value AND its version,
# the stopped container starts, the running one still answers `exec`.
#
# Exit: 0 every check held; 1 a check failed (named on the output); 77 the
# host cannot run it (no user namespaces, no image pull).
set -u
OLD="${1:?old binary dir}"; NEW="${2:?new binary dir}"; WORK="${3:?work dir}"
IMG="${4:-alpine:3.20}"
OLDB="$OLD/delonix"; NEWB="$NEW/delonix"
[[ -x "$OLDB" && -x "$NEWB" ]] || { echo "missing binary: $OLDB or $NEWB"; exit 77; }

export DELONIX_ROOT="$WORK/root" DELONIX_NET_RUNTIME_DIR="$WORK/net"
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"
M="$WORK/stack.yaml"
fails=0
ok() { echo "ok    $1"; }
bad() { echo "FAIL  $1: $2"; fails=$((fails + 1)); }

cleanup() {
  local b n
  for b in "$NEWB" "$OLDB"; do
    "$b" stack destroy -f "$M" >/dev/null 2>&1
    for n in 1 2 3 4; do
      [[ -z "$("$b" container ps -aq 2>/dev/null)" ]] && break
      "$b" container ps -aq 2>/dev/null | xargs -r "$b" container rm -f >/dev/null 2>&1
      sleep 3
    done
  done
  # Both binaries: the infra was started by the old one, and a new build that
  # cannot read the root (the case this test exists to catch) would leave it up.
  "$NEWB" net netns down >/dev/null 2>&1
  "$OLDB" net netns down >/dev/null 2>&1
  # The root goes only when neither binary sees anything left in it.
  if [[ -z "$("$NEWB" container ps -aq 2>/dev/null)$("$OLDB" container ps -aq 2>/dev/null)" ]]; then
    rm -rf "$WORK"
  else
    echo "left $WORK in place: containers are still recorded in it"
  fi
}
trap cleanup EXIT

cat >"$M" <<EOF
apiVersion: networking.delonix.io/v1alpha1
kind: Network
metadata: { name: upg-net }
spec: { driver: bridge, subnet: 10.246.0.0/16 }
---
apiVersion: storage.delonix.io/v1alpha1
kind: Volume
metadata: { name: upg-vol, labels: { app: upgrade } }
spec: { driver: local }
---
apiVersion: core.delonix.io/v1alpha1
kind: Secret
metadata: { name: upg-sec }
spec:
  stringData: { token: kept-across-the-upgrade }
---
apiVersion: compute.delonix.io/v1alpha1
kind: Container
metadata: { name: upg-run, labels: { tier: kept } }
spec:
  image: $IMG
  command: ["sleep", "100000"]
  network: upg-net
  volumes: ["upg-vol:/data"]
  env: ["MARK=env-kept"]
  secret: ["upg-sec"]
---
apiVersion: compute.delonix.io/v1alpha1
kind: Container
metadata: { name: upg-stopped }
spec:
  image: $IMG
  command: ["sleep", "100000"]
  network: upg-net
EOF

# --- the previous release writes the root -----------------------------------
out=$(timeout 300 "$OLDB" stack apply -f "$M" 2>&1) || { echo "$out" | tail -5; exit 77; }
# Write and re-read: an exec right after a start used to land before the mounts
# (AGENTS.md, «Um `exec` logo a seguir ao `run -d` corria no HOST»).
for _ in $(seq 1 20); do
  "$OLDB" container exec upg-run sh -c 'echo data-kept > /data/f' >/dev/null 2>&1
  [[ "$("$OLDB" container exec upg-run cat /data/f 2>/dev/null)" == data-kept ]] && break
  sleep 0.5
done
[[ "$("$OLDB" container exec upg-run cat /data/f 2>/dev/null)" == data-kept ]] ||
  { echo "the previous release could not write its own volume"; exit 77; }
# A stop can give up (DX-8101) while the kernel writes back a saturated disk
# (ADR-0056 D4): repeat it, and if the container never stops the host cannot
# give this test its precondition — that is 77, not a defect of either build.
stopped=""
for _ in 1 2 3 4 5 6; do
  timeout 120 "$OLDB" container stop upg-stopped >/dev/null 2>&1
  st=$("$OLDB" container inspect upg-stopped 2>/dev/null |
    python3 -c 'import json,sys;print(json.load(sys.stdin)[0].get("status") or "")' 2>/dev/null)
  [[ "$st" != Running ]] && { stopped=1; break; }
  sleep 10
done
[[ -n "$stopped" ]] || { echo "the previous release could not stop upg-stopped on this host (status $st)"; exit 77; }
"$OLDB" stack plan -f "$M" --detailed-exitcode >/dev/null 2>&1; rc=$?
[[ $rc -eq 0 ]] || { echo "the previous release plans a change on its own root (rc=$rc)"; exit 77; }
# The version is on the text view only (`-o json` carries keys and data).
sec_ver() { "$1" secret inspect upg-sec -o table 2>/dev/null | sed -n 's/^Version: *//p'; }
v_before=$(sec_ver "$OLDB")
run_pid=$("$OLDB" container inspect upg-run 2>/dev/null |
  python3 -c 'import json,sys;print(json.load(sys.stdin)[0].get("pid") or "")' 2>/dev/null)

# --- this build opens it -----------------------------------------------------
"$NEWB" stack plan -f "$M" --detailed-exitcode >"$WORK/plan.out" 2>&1; rc=$?
[[ $rc -eq 0 ]] && ok "stack plan: no change on a root the previous release wrote" ||
  bad "stack plan" "rc=$rc — $(tail -5 "$WORK/plan.out" | tr '\n' ' ')"

listed=$("$NEWB" container ps -a --format '{{.Names}}' 2>/dev/null || "$NEWB" container ps -a 2>/dev/null)
for c in upg-run upg-stopped; do
  grep -qw "$c" <<<"$listed" && ok "container $c is listed" || bad "container $c" "not in container ps -a"
done
"$NEWB" network inspect upg-net 2>/dev/null | grep -q '10\.246\.' && ok "network keeps its subnet" ||
  bad "network" "upg-net missing or lost 10.246.0.0/16"
"$NEWB" volume ls 2>/dev/null | grep -qw upg-vol && ok "volume is listed" || bad "volume" "upg-vol not in volume ls"
"$NEWB" image ls 2>/dev/null | grep -q "${IMG%%:*}" && ok "image is in the store" || bad "image" "${IMG} not in image ls"

now_pid=$("$NEWB" container inspect upg-run 2>/dev/null |
  python3 -c 'import json,sys;print(json.load(sys.stdin)[0].get("pid") or "")' 2>/dev/null)
[[ -n "$run_pid" && "$now_pid" == "$run_pid" ]] && ok "the running container keeps its pid ($run_pid)" ||
  bad "running container" "pid $run_pid before, $now_pid after"
[[ "$("$NEWB" container exec upg-run cat /data/f 2>/dev/null)" == data-kept ]] &&
  ok "the volume keeps its bytes (read through exec)" || bad "volume data" "/data/f is not data-kept"
[[ "$("$NEWB" container exec upg-run sh -c 'echo $MARK $token' 2>/dev/null)" == "env-kept kept-across-the-upgrade" ]] &&
  ok "the running container keeps its env and secret" || bad "env/secret" "MARK/token not as written"

"$NEWB" secret inspect upg-sec --reveal 2>/dev/null | grep -q kept-across-the-upgrade &&
  ok "the secret decrypts with the key the previous release wrote" || bad "secret" "value not readable"
v_after=$(sec_ver "$NEWB")
[[ -n "$v_before" && "$v_after" == "$v_before" ]] && ok "the secret keeps its version ($v_before)" ||
  bad "secret version" "$v_before before, $v_after after"

st_out=$(timeout 120 "$NEWB" container start upg-stopped 2>&1); st_rc=$?
[[ $st_rc -eq 0 && "$("$NEWB" container exec upg-stopped sh -c 'echo up' 2>/dev/null)" == up ]] &&
  ok "the stopped container starts and answers" ||
  bad "start" "rc=$st_rc, $(tail -3 <<<"$st_out" | tr '\n' ' ')status=$("$NEWB" container inspect upg-stopped 2>/dev/null |
    python3 -c 'import json,sys;print(json.load(sys.stdin)[0].get("status"))' 2>/dev/null)"
"$NEWB" container exec upg-stopped ip addr 2>/dev/null | grep -q '10\.246\.' &&
  ok "the restarted container is back on its network" || bad "network attach" "upg-stopped has no 10.246 address"

"$NEWB" stack plan -f "$M" --detailed-exitcode >/dev/null 2>&1; rc=$?
[[ $rc -eq 0 ]] && ok "stack plan: still no change after the new build touched it" || bad "second plan" "rc=$rc"

[[ $fails -eq 0 ]] || { echo "$fails check(s) failed"; exit 1; }
