#!/usr/bin/env bash
# Runs the upstream CRI validation suite (cri-tools `critest`) against
# `delonix-cri`, and prints the score.
#
# The point is the NUMBER, published and reproducible. "Serves the kubelet" is a
# claim; 65/103 named specs is a fact someone else can check, and it is the only
# form in which a conformance statement means anything.
set -euo pipefail

VER="${CRI_TOOLS_VERSION:-v1.36.0}"
WORK="${WORK:-$(mktemp -d)}"
SOCK="${SOCK:-unix://$WORK/cri.sock}"
# A root of our own: the default is /var/lib/delonix, which a normal user
# cannot write — and the failure surfaces as `Permission denied (os error 13)`
# on every single image pull, which reads like 103 conformance failures rather
# than one wrong path. It cost a full run to notice.
export DELONIX_ROOT="${DELONIX_ROOT:-$WORK/state}"
mkdir -p "$DELONIX_ROOT"

# The suite runs under a state root of its own, but the netns infra (control
# socket, slirp) is keyed by UID and therefore SHARED with every other delonix
# on this user. Two roots each build their own holder, and the second one wins:
# measured 2026-08-10, a session that started a container 2.5 minutes into a run
# took the socket, and every RunPodSandbox after that hung for the full 600s spec
# timeout — the run reported 79 failures with nothing to do with conformance.
#
# The engine now refuses that instead of clobbering it (`foreign_holder_message`),
# so the run would fail anyway — but it would fail 105 times with a message about
# sockets. Say it once, here, before spending twenty minutes.
# A real connect, not `[ -S ... ]`: a socket FILE outlives the process that
# created it, which is a mistake this repo has already made three times.
# `python3` and not `socat` because socat is not installed here — and a guard
# that quietly passes when its tool is missing is worse than no guard.
_holder_sock="/tmp/delonix-net-$(id -u)/control.sock"
if ! command -v python3 >/dev/null 2>&1; then
  echo "WARNING: no python3 — cannot check for a foreign holder; if the run" >&2
  echo "         fails on 'control socket', that is why." >&2
elif python3 -c "import socket,sys
s=socket.socket(socket.AF_UNIX)
try: s.connect(sys.argv[1])
except OSError: sys.exit(1)
sys.exit(0)" "$_holder_sock" 2>/dev/null; then
  echo "ERROR: this user already has a delonix network holder up." >&2
  echo "       The conformance run needs the infra to itself: stop it with" >&2
  echo "       \`delonix net netns down\` (this kills the SDN of any running" >&2
  echo "       container), or run the suite on a host with nothing else on it." >&2
  exit 1
fi

cd "$WORK"
if [ ! -x ./critest ]; then
  curl -sSL -o crit.tgz \
    "https://github.com/kubernetes-sigs/cri-tools/releases/download/$VER/critest-$VER-linux-amd64.tar.gz"
  tar xzf crit.tgz
fi

BIN="${DELONIX_CRI_BIN:-$(command -v delonix-cri || echo ./target/release/delonix-cri)}"
DELONIX_CRI_ADDR="$SOCK" setsid "$BIN" >"$WORK/server.log" 2>&1 </dev/null &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
sleep 3

./critest -runtime-endpoint "$SOCK" -image-endpoint "$SOCK" -ginkgo.timeout=20m || true
echo
echo "server log: $WORK/server.log"
