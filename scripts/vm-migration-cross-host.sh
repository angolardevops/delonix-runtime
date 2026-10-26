#!/usr/bin/env bash
# Cross-host cold VM migration proof (libvirt) — `vm.migration.cold`.
#
# The single-host battery in `scripts/e2e.sh` proves the SUBSTANTIVE steps of a
# cold migration on one host (stop, flatten, import --appliance, create+boot
# with state intact) and deliberately leaves out "the generic scp transport
# between two hosts (not a VM-provider capability)". THIS harness closes that
# last gap: it stands up a genuine SECOND delonix node on the same machine and
# drives the real `vm migrate` command — SSH transport included — A -> B.
#
# The second node is a separate state root (`DELONIX_ROOT`) reached over a real,
# user-space `sshd` on loopback, exactly the way `vm migrate` reaches any remote
# host: `scp` the flattened disk, then `delonix vm image import`/`vm create` over
# SSH as the connecting user. See cmd::vm::cmd_migrate / cmd::remote.
#
# SINGLE-HOST LIMIT — `--remove-source` is NOT exercised here, on purpose. The
# two nodes share ONE `qemu:///system` daemon, and a libvirt domain is keyed by
# NAME. The migration recreates the VM under its own name on node B, so the live
# domain in the shared daemon is now node B's. `--remove-source` would then run
# node A's `remove_force(name)`, whose `virsh undefine/destroy` is also keyed by
# that name — it would tear down node B's freshly-started domain (demonstrated:
# with `--remove-source`, `virsh domstate <name>` becomes "failed to get domain"
# and node B's `vm ls` is empty while its state files remain). On TWO REAL hosts
# each has its own libvirtd and the removal is local — a non-issue there. This
# harness therefore proves everything that is provable on one host: the disk
# crosses intact over SSH and the target boots it. The `--remove-source` final
# step is a two-host concern, documented, not faked.
#
# Proof of fidelity: a UNIQUE marker is written into the SOURCE VM's own overlay
# (guestfish) before the migration; after A -> B the marker is read back from
# the TARGET node's overlay (guestfish) AND the target domain is RUNNING. State
# crossed intact over SSH and the target booted the migrated disk.
#
# Not run by CI: it needs libvirt (`qemu:///system`), guestfish, a cloud image
# and a user-space sshd. Driven by hand / a live session; cited as the
# cross-host evidence behind `vm.migration.cold`.
#
# Required environment:
#   DELONIX_BIN     absolute path to the delonix binary (source AND, via the
#                   sshd PATH, the target must resolve the SAME binary)
#   NODEB_ROOT      DELONIX_ROOT for node B (a dir this user owns)
#   SSH_PORT        loopback sshd port for node B
#   SSH_KEY         private key accepted by that sshd
#   BASE_IMG        a qcow2/raw cloud image for the source VM
# Optional:
#   SSH_USER        default: the current user
#   KNOWN_HOSTS     default: a throwaway file next to SSH_KEY
set -uo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
step() { echo; echo "== $* =="; }

: "${DELONIX_BIN:?set DELONIX_BIN}"
: "${NODEB_ROOT:?set NODEB_ROOT}"
: "${SSH_PORT:?set SSH_PORT}"
: "${SSH_KEY:?set SSH_KEY}"
: "${BASE_IMG:?set BASE_IMG}"
SSH_USER="${SSH_USER:-$USER}"
KNOWN_HOSTS="${KNOWN_HOSTS:-${SSH_KEY}.known_hosts}"

command -v guestfish >/dev/null 2>&1 || fail "guestfish not found"
virsh -c qemu:///system uri >/dev/null 2>&1 || fail "qemu:///system does not answer (join the libvirt group)"
[[ -r "$BASE_IMG" ]] || fail "BASE_IMG not readable: $BASE_IMG"

PFX="xmig-$$"
SRC="$PFX-a"
MARKER_FILE="/etc/dlx-migration-marker"
MARKER="dlx-migration-$(cat /proc/sys/kernel/random/uuid)"
SROOT="${DELONIX_ROOT:-$HOME/.local/share/delonix}"   # node A
SRC_OVL="$SROOT/vms/$SRC.qcow2"

SSH_OPTS=(-i "$SSH_KEY" -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
          -o UserKnownHostsFile="$KNOWN_HOSTS" -p "$SSH_PORT")
remote() { ssh "${SSH_OPTS[@]}" "$SSH_USER@127.0.0.1" "$@"; }

cleanup() {
  "$DELONIX_BIN" vm rm -f "$SRC" >/dev/null 2>&1 || true
  remote "delonix vm rm -f $SRC >/dev/null 2>&1; delonix vm image rm migrated-$SRC >/dev/null 2>&1" || true
}
trap cleanup EXIT

step "node B is a SEPARATE, empty state root reachable over SSH"
remote "echo ROOT=\$DELONIX_ROOT; delonix --version | sed -n 3p" \
  || fail "cannot reach node B over SSH"
[[ "$(remote 'echo $DELONIX_ROOT')" == "$NODEB_ROOT" ]] \
  || fail "node B DELONIX_ROOT is not $NODEB_ROOT"
remote "delonix vm ls" | grep -q "$SRC" \
  && fail "node B already knows '$SRC' before migration (roots not isolated)"

step "create the source VM on node A ($SRC)"
"$DELONIX_BIN" vm create "$SRC" --disk "$BASE_IMG" --vcpus 2 --memory 1G \
  --network ingress --backend libvirt --disk-size 5 \
  || fail "vm create on node A"
"$DELONIX_BIN" vm stop "$SRC" >/dev/null 2>&1 || true
for _ in $(seq 1 30); do
  st="$(virsh -c qemu:///system domstate "$SRC" 2>/dev/null)"
  [[ "$st" == *"shut off"* || -z "$st" ]] && break
  sleep 2
done
[[ -f "$SRC_OVL" ]] || fail "source overlay missing: $SRC_OVL"

step "bake a unique marker into the SOURCE overlay (guestfish): $MARKER"
guestfish --rw -a "$SRC_OVL" -i <<EOF || fail "guestfish write marker"
write $MARKER_FILE "$MARKER"
EOF
got="$(guestfish --ro -a "$SRC_OVL" -i cat "$MARKER_FILE" 2>/dev/null)"
[[ "$got" == "$MARKER" ]] || fail "marker not readable back on source (got: '$got')"

step "REAL cold migration A -> B over SSH (vm migrate)"
# No --remove-source on a single shared libvirtd (see the SINGLE-HOST LIMIT note
# in the header): it would undefine node B's identically-named domain.
"$DELONIX_BIN" vm migrate "$SRC" \
  --host 127.0.0.1 --user "$SSH_USER" --ssh-key "$SSH_KEY" --ssh-port "$SSH_PORT" \
  --network ingress --backend libvirt \
  || fail "vm migrate A -> B"

step "the TARGET node (B) now runs '$SRC'"
remote "delonix vm ls" | grep -q "$SRC" || fail "target node B does not list '$SRC'"
tgt_state="$(virsh -c qemu:///system domstate "$SRC" 2>/dev/null)"
echo "target domstate: $tgt_state"
[[ "$tgt_state" == *running* ]] || fail "target domain '$SRC' is not running (got: '$tgt_state')"

step "the marker crossed intact — read it from the TARGET node's own overlay"
TGT_OVL="$NODEB_ROOT/vms/$SRC.qcow2"
[[ -f "$TGT_OVL" ]] || fail "target overlay missing: $TGT_OVL"
# Stop the target so guestfish can open its disk (no VMM holding it).
remote "delonix vm stop $SRC >/dev/null 2>&1" || true
for _ in $(seq 1 30); do
  st="$(virsh -c qemu:///system domstate "$SRC" 2>/dev/null)"
  [[ "$st" == *"shut off"* || -z "$st" ]] && break
  sleep 2
done
crossed="$(guestfish --ro -a "$TGT_OVL" -i cat "$MARKER_FILE" 2>/dev/null)"
[[ "$crossed" == "$MARKER" ]] \
  || fail "marker did NOT cross intact (target has: '$crossed', expected: '$MARKER')"

echo
echo "PASS: cold migration A -> B proved end-to-end over SSH"
echo "  marker '$MARKER' written on A, read back from B's own overlay"
echo "  target domain booted (running) on B (--remove-source is a two-host step; see header)"
