#!/usr/bin/env bash
# A container whose init passed the start's mount ceiling is never entered
# until its mounts are seen done, and comes up on its own once they are.
#
#   e2e_mount_ceiling.sh <binary> <image> [prefix]
#
# Called by `scripts/e2e.sh`; runs alone against any binary, with
# `DELONIX_ROOT` and `DELONIX_NET_RUNTIME_DIR` already isolated by the caller.
# Exits 0 (right), 1 (defect, with the measurement in the output) or 77 (this
# host does not let it measure).
#
# Issue #793, measured on a kubeadm node with the disk saturated: two
# control-plane containers missed the 60 s ceiling of the «mounted» handshake,
# the engine warned and published a plain `Running` record, and the CRI
# answered `StartContainer` with success. An `exec` entering such a container
# joins a mount namespace whose `/` may still be the host's.
#
# How the init is held before its mounts without saturating the host's disk:
# the start moves the init into its cgroup leaf BEFORE releasing it to mount,
# and a leaf created under a FROZEN parent group is frozen too. The container is
# declared with `cgroupParent` naming a group this script created and froze, so
# the init stops right there. It is then SIGSTOPped and the group thawed: the
# init stays put (an `exec` joins the same leaf, so it must not be frozen), the
# start passes its 60 s ceiling, and a SIGCONT lets the mounts happen.
set -u
BIN="${1:?binary}"; IMG="${2:?image}"; PFX="${3:-mc$$}"
name="$PFX-mc"
grp="${PFX}mc"
grp_dir=""
init=""
apply_pid=""
probe='test -e /etc/alpine-release && echo CONTAINER || echo HOST'

cleanup() {
  [[ -n "$init" ]] && kill -CONT "$init" 2>/dev/null
  [[ -n "$grp_dir" && -f "$grp_dir/cgroup.freeze" ]] && echo 0 >"$grp_dir/cgroup.freeze" 2>/dev/null
  [[ -n "$apply_pid" ]] && wait "$apply_pid" 2>/dev/null
  local _
  for _ in 1 2 3 4; do
    timeout 120 "$BIN" container rm -f "$name" >/dev/null 2>&1
    "$BIN" container inspect "$name" >/dev/null 2>&1 || break
    sleep 5
  done
  [[ -n "$grp_dir" ]] && rmdir "$grp_dir" 2>/dev/null
  rm -f "$manifest"
}
die() { echo "$*"; cleanup; exit 1; }
cannot() { echo "$*"; cleanup; exit 77; }
run_probe() { "$BIN" container exec "$name" sh -c "$probe" 2>&1; }

# The engine's rootless cgroup base: `<user@uid.service>/dlx-containers`, found
# the way the engine finds it — the delegation boundary above this process.
cur=$(sed -n 's/^0:://p' /proc/self/cgroup)
svc=$(printf '%s\n' "$cur" | grep -o '^.*/user@[0-9]*\.service' || true)
manifest=$(mktemp)
[[ -n "$svc" ]] || cannot "this session is not under a user@<uid>.service cgroup to freeze a group in"
base="/sys/fs/cgroup$svc/dlx-containers"
mkdir -p "$base/$grp" 2>/dev/null || cannot "cannot create a cgroup group under $base"
grp_dir="$base/$grp"
echo 1 >"$grp_dir/cgroup.freeze" 2>/dev/null || cannot "cannot freeze $grp_dir"

cat >"$manifest" <<EOF
apiVersion: compute.delonix.io/v1alpha1
kind: Container
metadata: { name: $name }
spec:
  image: $IMG
  command: ["sleep", "1000"]
  network: none
  cgroupParent: { name: $grp }
EOF
out_file=$(mktemp)
"$BIN" stack apply -f "$manifest" >"$out_file" 2>&1 &
apply_pid=$!

# The init lands frozen in a leaf of the group; take it from there.
while kill -0 "$apply_pid" 2>/dev/null; do
  for leaf in "$grp_dir"/dlx-*; do
    [[ -f "$leaf/cgroup.procs" ]] || continue
    init=$(head -1 "$leaf/cgroup.procs")
    [[ -n "$init" ]] && break 2
  done
  sleep 0.1
done
if [[ -z "$init" ]]; then
  apply_out=$(cat "$out_file"); rm -f "$out_file"
  cannot "the init never landed in the frozen group (no cgroup delegation here?): $apply_out"
fi
kill -STOP "$init"
echo 0 >"$grp_dir/cgroup.freeze"

wait "$apply_pid"; apply_rc=$?; apply_pid=""
apply_out=$(cat "$out_file"); rm -f "$out_file"
# A container that may still come up is not a failed start: the apply succeeds,
# and kills nothing.
[[ $apply_rc -eq 0 ]] || die "the apply failed (rc=$apply_rc) for a container that can still come up: $apply_out"
kill -0 "$init" 2>/dev/null || die "the start killed the init it could not see mounted"

# While the init is held before its mounts, nothing may enter it.
out=$(run_probe); rc=$?
[[ "$out" == *HOST* ]] && die "exec ran on the HOST's filesystem of a container not seen mounted (rc=$rc): $out"
[[ $rc -ne 0 && "$out" == *"has not finished mounting"* ]] ||
  die "exec into a container not seen mounted was not refused as such (rc=$rc): $out"
cp_dst=$(mktemp -d)
"$BIN" container cp "$name:/etc/os-release" "$cp_dst/" >/dev/null 2>&1 && {
  got=$(head -1 "$cp_dst/os-release" 2>/dev/null); rm -rf "$cp_dst"
  die "cp copied out of a container not seen mounted: $got"
}
rm -rf "$cp_dst"
status=$("$BIN" container ls -a 2>/dev/null | grep -w "$name")
[[ "$status" == *"(mounting)"* ]] || die "ls shows it as plain Up while entering it is refused: $status"

# Released, it comes up on its own: the proof arriving late lifts the refusal.
kill -CONT "$init"
init_held="$init"; init=""
ok=""
for _ in $(seq 1 600); do
  out=$(run_probe); rc=$?
  [[ $rc -eq 0 && "$out" == *CONTAINER* ]] && { ok=1; break; }
  [[ "$out" == *HOST* ]] && die "exec ran on the HOST's filesystem after the release: $out"
  sleep 0.2
done
[[ -n "$ok" ]] || die "the container never became enterable after its mounts (rc=$rc): $out"
kill -0 "$init_held" 2>/dev/null || die "the init died instead of coming up"
status=$("$BIN" container ls -a 2>/dev/null | grep -w "$name")
[[ "$status" == *"(mounting)"* ]] && die "ls still shows (mounting) after the mounts: $status"

echo "held before its mounts: apply rc=0, exec and cp refused, ls «(mounting)»; released: came up, exec reaches the container"
cleanup
exit 0
