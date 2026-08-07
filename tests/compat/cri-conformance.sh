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
