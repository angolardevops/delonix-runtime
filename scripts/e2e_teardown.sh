# shellcheck shell=bash
# Teardown helpers for the e2e battery. Sourced by `scripts/e2e.sh` and by
# `scripts/e2e_rm_teardown.sh`, which proves them against a real DX-8101; the
# caller defines `BIN` and `log`.

# Teardown that a section relies on, and that a loaded disk can break.
#
# A `container rm -f` whose process is still leaving (state D, waiting on the
# disk's writeback) answers DX-8101 and KEEPS the record, as designed: "run
# `rm -f` again once it is gone". Silencing that left the container behind, its
# network could not be removed, and the next section failed on a collision that
# had nothing to do with what it tests (measured 2026-10-12: 28 FAIL with the
# disk at 3 GB dirty, none of them reproducible with the disk calm). So the
# teardown repeats on DX-8101 for up to two minutes, says so when it gives up,
# and says any other failure instead of swallowing it. A container that is
# already gone (class 4) is not news. `E2E_RM_DEADLINE` (seconds) shortens the
# wait, for the scenario that proves the give-up.
e2e_rm() {
  local c out rc deadline wait=${E2E_RM_DEADLINE:-120}
  for c in "$@"; do
    deadline=$((SECONDS + wait))
    while :; do
      out=$("$BIN" container rm -f "$c" 2>&1); rc=$?
      (( rc == 0 || rc == 4 )) && break
      if [[ "$out" != *DX-8101* ]]; then log "  (limpeza: container rm -f $c → rc=$rc: ${out##*$'\n'})"; break; fi
      (( SECONDS >= deadline )) && { log "  (limpeza: $c continua a sair ao fim de $wait s — DX-8101)"; break; }
      sleep "${E2E_RM_PAUSE:-2}"
    done
  done
}

# An image removal judged by its effect, for the caller to run under `check`:
# each name must answer 0 (or 4, already gone — a build that failed never made
# it) AND be absent from `image ls` afterwards; otherwise it says why and
# returns 1. Measured 2026-10-22: three teardown lines called `image rm`, which
# is not a subcommand (`image remove`, no alias — docs/cli-stability.md), with
# the output sent to /dev/null; clap answered 2, nothing looked, and every run
# left its test images in the root.
e2e_image_rm() {
  local i out rc ls bad=0
  for i in "$@"; do
    out=$("$BIN" image remove "$i" 2>&1); rc=$?
    if (( rc != 0 && rc != 4 )); then
      echo "image remove $i → rc=$rc: ${out##*$'\n'}"; bad=1; continue
    fi
    # JSON, never the table: the table cuts REPOSITORY:TAG near 44 characters
    # with «…», so a long name still in the store read as gone (measured).
    # stderr stays out of the JSON, and a listing that does not parse is its
    # own answer (2), never «absent».
    ls=$("$BIN" image ls -o json 2>/dev/null) || { echo "image ls -o json → rc=$?"; bad=1; continue; }
    # A name shows as given or behind a registry prefix (`…/name:tag`).
    python3 -I -c '
import json, sys
n = sys.argv[1]
try:
    tags = [t for img in json.load(sys.stdin) for t in img.get("repo_tags") or []]
except (ValueError, TypeError, AttributeError) as e:
    print("image ls -o json did not parse: %s" % e); sys.exit(2)
sys.exit(0 if any(t == n or t.endswith("/" + n) for t in tags) else 1)' "$i" <<<"$ls"
    case $? in
      0) echo "image remove $i → rc=$rc, but image ls still lists it"; bad=1 ;;
      1) ;;
      *) bad=1 ;;
    esac
  done
  return "$bad"
}

# A network removal that fails says why, so a leftover is never silent. Class 4
# (already gone) is not news.
e2e_net_rm() {
  local n out rc
  for n in "$@"; do
    out=$("$BIN" network rm "$n" 2>&1); rc=$?
    (( rc == 0 || rc == 4 )) || log "  (limpeza: network rm $n → rc=$rc: ${out##*$'\n'})"
  done
}
