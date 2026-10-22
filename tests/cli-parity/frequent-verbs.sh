#!/usr/bin/env bash
# Acceptance suite — the central question of the CLI review:
# "Can a Docker/Podman user run their habitual flows on Delonix, within the
#  supported scope, changing ONLY the executable name?"
#
# This pass covers the most frequent flows (run, ps, logs, exec, stop, start,
# restart, rm, pull). It proves, WITHOUT a running engine or network, that each
# root verb reaches the SAME handler as its `container <verb>` / `image pull`
# counterpart: identical --help, and — where the verb touches the store/engine —
# byte-identical behaviour (output or error) between the two spellings.
#
# Usage: build the binary, then run this with its path:
#   cargo build -p delonix-runtime-bin --bin delonix
#   tests/cli-parity/frequent-verbs.sh target/debug/delonix
set -u
BIN="${1:-delonix}"
pass=0; fail=0
# same_help <verb> <group-prefix>: the FLAG/ARG surface of `delonix <verb>` must
# equal `delonix <group-prefix> <verb>`. Lines that merely NAME the command path
# (Usage, the examples, the footer) legitimately differ — `delonix run …` vs
# `delonix container run …` — and are stripped before the compare; what must
# match is the about text and every Option/Argument description.
# flag_surface: the help down to (but not including) the first editorial section
# (EXAMPLES/SEE ALSO/COMMAND MAP), minus the Usage line. That leaves the about
# text and every Option/Argument description — the part that MUST match. The
# editorial sections legitimately differ (each verb shows its own examples and
# cross-references) and are not part of the flag contract.
flag_surface() {
  "$BIN" $1 --help 2>&1 \
    | awk '/^(EXAMPLES|SEE ALSO|COMMAND MAP):/{exit} {print}' \
    | grep -v '^Usage:'
}
same_help() {
  local verb="$1" grp="$2"
  local a b
  a="$(flag_surface "$verb")"
  b="$(flag_surface "$grp $verb")"
  if [ "$a" = "$b" ]; then echo "PASS  flags: delonix $verb  ==  delonix $grp $verb"; pass=$((pass+1));
  else echo "FAIL  flags: delonix $verb  !=  delonix $grp $verb"; fail=$((fail+1));
    diff <(echo "$a") <(echo "$b") | head -8; fi
}
# same_run <root argv...> -- <group argv...>: stdout+stderr+exit identical.
same_behaviour() {
  local label="$1"; shift
  local root_cmd="$1"; shift
  local grp_cmd="$1"; shift
  local ra rb rc gc
  ra="$("$BIN" $root_cmd 2>&1)"; rc=$?
  rb="$("$BIN" $grp_cmd 2>&1)"; gc=$?
  if [ "$ra" = "$rb" ] && [ "$rc" = "$gc" ]; then
    echo "PASS  $label: '$root_cmd' == '$grp_cmd' (exit $rc)"; pass=$((pass+1));
  else
    echo "FAIL  $label: '$root_cmd' (exit $rc) != '$grp_cmd' (exit $gc)"; fail=$((fail+1));
  fi
}

echo "== --help parity (root verb vs group verb) =="
for v in run create ps start stop restart rm exec logs; do same_help "$v" container; done
same_help pull image

echo
echo "== behaviour parity (offline: store reads and no-such-container errors) =="
same_behaviour "ps -a"        "ps -a"                      "container ps -a"
same_behaviour "stop missing" "stop dlx-nope"              "container stop dlx-nope"
same_behaviour "start missing" "start dlx-nope"            "container start dlx-nope"
same_behaviour "restart missing" "restart dlx-nope"        "container restart dlx-nope"
same_behaviour "rm missing"   "rm dlx-nope"                "container rm dlx-nope"
same_behaviour "logs missing" "logs dlx-nope"              "container logs dlx-nope"
same_behaviour "exec missing" "exec dlx-nope -- echo hi"   "container exec dlx-nope -- echo hi"

echo
echo "total: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
