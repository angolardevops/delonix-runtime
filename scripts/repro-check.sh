#!/usr/bin/env bash
# repro-check.sh — prove that the release binary does not depend on where the tree was built.
#
# Copies the tracked files of this checkout into two directories of different path
# lengths, builds `delonix` from each with the reproducible-build settings (the same
# ones `make build REPRODUCIBLE=1` uses) and compares the two binaries byte for byte.
# Exit 0 when identical, 1 when they differ, 2 when a build fails.
#
# Costs two release builds, run in parallel (about 4 minutes, 2 x 2.5 GiB of RAM).
# Work happens under $REPRO_DIR (default ~/.cache/delonix-repro), never /tmp, and is
# removed on exit.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=${REPRO_DIR:-$HOME/.cache/delonix-repro}
JOBS=${REPRO_JOBS:-6}
EPOCH=${SOURCE_DATE_EPOCH:-1700000000}
A=$WORK/a
B=$WORK/bravo-a-longer-checkout-path
trap 'rm -rf "$WORK"' EXIT

rm -rf "$WORK"; mkdir -p "$A" "$B"
(cd "$ROOT" && git ls-files -z | xargs -0 tar -c) | tar -x -C "$A"
(cd "$ROOT" && git ls-files -z | xargs -0 tar -c) | tar -x -C "$B"

build() { # <dir>
  local d=$1
  ( cd "$d" && SOURCE_DATE_EPOCH=$EPOCH CARGO_BUILD_JOBS=$JOBS CARGO_TARGET_DIR="$d/target" \
      RUSTFLAGS="--remap-path-prefix=$d=/build --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup" \
      nice -n 19 cargo build --release --locked -p delonix-runtime-bin >"$d/build.log" 2>&1 )
}
build "$A" & PA=$!
build "$B" & PB=$!
wait $PA; RA=$?
wait $PB; RB=$?
if [ $RA -ne 0 ] || [ $RB -ne 0 ]; then
  echo "a build failed (a=$RA b=$RB); last lines:" >&2
  tail -5 "$A/build.log" "$B/build.log" >&2
  exit 2
fi

SA=$(sha256sum "$A/target/release/delonix" | cut -d' ' -f1)
SB=$(sha256sum "$B/target/release/delonix" | cut -d' ' -f1)
echo "a: $SA"
echo "b: $SB"
if [ "$SA" = "$SB" ]; then
  echo "reproducible: the binary does not depend on the checkout path"
  exit 0
fi
echo "NOT reproducible: the two binaries differ. Embedded paths that survived:" >&2
for d in "$A" "$B"; do strings -n 8 "$d/target/release/delonix" | grep -F "$d" | head -3 >&2; done
exit 1
