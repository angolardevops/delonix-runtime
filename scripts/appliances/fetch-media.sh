#!/bin/bash
# Fetch vendor installation media and PROVE it is what the vendor published.
#
#   fetch-media.sh <url> <sha256> <dest>
#
# Why this exists: `build-opnsense.sh` has always downloaded and verified its
# own media, while `build-proxmox.sh` and `build-truenas.sh` took a path to an
# ISO that somebody had fetched by hand — and nothing, anywhere, checked it.
# The whole point of these images is that the vendor would recognise them; an
# ISO nobody verified makes that a hope. Same rule the rest of this repo already
# holds itself to (the golden VM build refuses a download it cannot check).
#
# Fail-closed, always: a missing or mismatching checksum aborts. There is no
# flag to skip it, deliberately — the moment there is one, it ends up in a
# script somewhere.
#
# The caller resolves the expected hash, because every vendor publishes it in a
# different shape: Proxmox a GNU `SHA256SUMS` for the whole directory, TrueNAS a
# bare hash in `<iso>.sha256`, OPNsense a BSD-style `SHA256 (file) = hash`.
# Keeping that knowledge in each build script is what lets this one stay honest
# about the single thing it does.
set -euo pipefail

URL=${1:?usage: fetch-media.sh <url> <sha256> <dest>}
WANT=${2:?missing expected sha256}
DEST=${3:?missing destination}

# A hash that is not a hash is a bug in the caller, and would otherwise only
# surface as a confusing mismatch after a 1.7 GB download.
case "$WANT" in
  [0-9a-fA-F]*) [ ${#WANT} -eq 64 ] || { echo "!! expected sha256 is ${#WANT} chars, not 64: $WANT" >&2; exit 1; } ;;
  *) echo "!! expected sha256 is not hex: $WANT" >&2; exit 1 ;;
esac

verify() { echo "$WANT  $1" | sha256sum -c --status - 2>/dev/null; }

# Already here and already right: say so and stop. These are 1.5-1.7 GB each,
# and a rebuild should not re-download them.
if [ -f "$DEST" ] && verify "$DEST"; then
  echo "==> cached and verified: $DEST"
  exit 0
fi
if [ -f "$DEST" ]; then
  echo "==> $DEST exists but does not match the expected checksum — refetching"
fi

mkdir -p "$(dirname "$DEST")"
echo "==> downloading $(basename "$DEST")"
# `-C -` resumes a partial file: on a slow link (this repo has measured 416 KB/s
# to some vendors) a dropped connection would otherwise start a 1.7 GB download
# from zero, which is how a fetch never finishes. Writing to `$DEST.part` keeps
# a half-file from ever being mistaken for a complete one.
curl -fL --retry 5 --retry-delay 2 --retry-connrefused -C - \
     --progress-bar -o "$DEST.part" "$URL"

echo "==> verifying"
if ! echo "$WANT  $DEST.part" | sha256sum -c --status -; then
  echo "!! checksum MISMATCH for $URL" >&2
  echo "   expected: $WANT" >&2
  echo "   actual:   $(sha256sum "$DEST.part" | cut -d' ' -f1)" >&2
  # Keep the bad file OUT of the cache path: leaving it as `$DEST` would make
  # the next run trust it after a resume.
  rm -f "$DEST.part"
  exit 1
fi
# Rename only after the hash matched — `$DEST` existing means `$DEST` is good.
mv "$DEST.part" "$DEST"
echo "==> ok: $DEST"
