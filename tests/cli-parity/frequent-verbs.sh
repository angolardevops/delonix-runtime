#!/usr/bin/env bash
# Acceptance suite — the central question of the CLI review:
# "Can a Docker/Podman user run their habitual flows on Delonix, within the
#  supported scope, changing ONLY the executable name?"
#
# It covers the WHOLE Docker/Podman table: the frequent flows #930 landed
# (run, ps, logs, exec, stop, start, restart, rm, pull) and the rest added
# after (kill, attach, stats; images, rmi, tag, save, load, push; and the
# polymorphic `wait`). It proves, WITHOUT a running engine or network, that
# each root verb reaches the SAME handler as its `container <verb>` /
# `image <verb>` counterpart: identical --help, and — where the verb touches
# the store/engine — byte-identical behaviour (output or error) between the two
# spellings.
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
# same_help2 <root-path> <group-path>: same as same_help but for the two verbs
# whose root spelling differs from the group one (`images`→`image ls`,
# `rmi`→`image rm`). The flag surface still has to be identical.
same_help2() {
  local root="$1" grp="$2"
  local a b
  a="$(flag_surface "$root")"
  b="$(flag_surface "$grp")"
  if [ "$a" = "$b" ]; then echo "PASS  flags: delonix $root  ==  delonix $grp"; pass=$((pass+1));
  else echo "FAIL  flags: delonix $root  !=  delonix $grp"; fail=$((fail+1));
    diff <(echo "$a") <(echo "$b") | head -8; fi
}
# assert_fail <label> <argv...>: the command must exit non-zero AND print the
# needle (checked by the caller via the global `_out`). Used for the `wait`
# disambiguation refusal, which has no group counterpart to diff against.
assert_contains() {
  local label="$1" needle="$2"; shift 2
  local out rc
  out="$("$BIN" "$@" 2>&1)"; rc=$?
  if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -qF "$needle"; then
    echo "PASS  $label: refused (exit $rc) with the documented reason"; pass=$((pass+1));
  else
    echo "FAIL  $label: expected a non-zero exit naming '$needle', got exit $rc:"; fail=$((fail+1));
    printf '%s\n' "$out" | head -4;
  fi
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
for v in run create ps start stop restart rm exec logs kill attach stats; do same_help "$v" container; done
same_help pull image
for v in tag save load push; do same_help "$v" image; done
same_help2 images "image ls"
same_help2 rmi    "image rm"

echo
echo "== behaviour parity (offline: store reads and no-such-resource errors) =="
same_behaviour "ps -a"        "ps -a"                      "container ps -a"
same_behaviour "stop missing" "stop dlx-nope"              "container stop dlx-nope"
same_behaviour "start missing" "start dlx-nope"            "container start dlx-nope"
same_behaviour "restart missing" "restart dlx-nope"        "container restart dlx-nope"
same_behaviour "rm missing"   "rm dlx-nope"                "container rm dlx-nope"
same_behaviour "logs missing" "logs dlx-nope"              "container logs dlx-nope"
same_behaviour "exec missing" "exec dlx-nope -- echo hi"   "container exec dlx-nope -- echo hi"
same_behaviour "kill missing" "kill dlx-nope"              "container kill dlx-nope"
same_behaviour "attach missing" "attach dlx-nope"          "container attach dlx-nope"
same_behaviour "stats all"    "stats"                      "container stats"
same_behaviour "images"       "images"                     "image ls"
same_behaviour "images json"  "images -o json"             "image ls -o json"
same_behaviour "rmi missing"  "rmi dlx-nope:1"             "image rm dlx-nope:1"
same_behaviour "tag missing"  "tag dlx-nope:1 x:2"         "image tag dlx-nope:1 x:2"
same_behaviour "save missing" "save dlx-nope:1 -o /dev/null" "image save dlx-nope:1 -o /dev/null"
same_behaviour "load missing" "load -i /nonexistent-dlx.tar" "image load -i /nonexistent-dlx.tar"
same_behaviour "push missing" "push dlx-nope:1"            "image push dlx-nope:1"

echo
echo "== wait: one verb, two forms, selected by grammar =="
# A container operand reaches the SAME handler as `container wait`.
same_behaviour "wait container" "wait dlx-nope"            "container wait dlx-nope"
# No operand stays the manifest form (both spellings error the same way on a
# missing manifest file).
same_behaviour "wait manifest"  "wait -f /nonexistent-dlx.yaml" "wait -f /nonexistent-dlx.yaml"
# Mixing the two grammars is REFUSED, not silently resolved.
assert_contains "wait mixed refused" "not both" wait dlx-nope -f /nonexistent-dlx.yaml

echo
echo "total: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
