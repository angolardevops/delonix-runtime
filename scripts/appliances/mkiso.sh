#!/bin/bash
# Rebuild a Proxmox installation ISO with an embedded answer file, so the
# automated installer runs unattended.
#
# The ISO cannot be edited in place: `xorriso -boot_image any replay` dies on
# the hybrid GPT ("partitions 1 and 2 overlap") and `keep` produces an image
# SeaBIOS refuses to boot past "Booting from DVD/CD..." (both measured, not
# assumed). So the tree is extracted and the ISO authored again.
#
# The boot layout is NOT hardcoded: `xorriso -report_el_torito as_mkisofs`
# prints the exact mkisofs arguments that reproduce the source ISO, and those
# are what we replay. That is what makes one script correct for all four
# Proxmox products (the volume id alone differs: PVE/PBS/PMG/PDM).
set -euo pipefail

SRC_ISO=$1          # original Proxmox ISO
ANSWER=$2           # answer.toml for this product
OUT_ISO=$3          # ISO to write
WORK=${4:-$(mktemp -d)}

test -f "$SRC_ISO"
test -f "$ANSWER"
rm -rf "$WORK/iso"
mkdir -p "$WORK/iso"

echo "==> reading boot layout of $(basename "$SRC_ISO")"
mapfile -d '' -t MKISOFS_ARGS < <(
  xorriso -indev "$SRC_ISO" -report_el_torito as_mkisofs 2>/dev/null |
    python3 -c '
import shlex, sys
# The report is one option (or option + quoted value) per line, in mkisofs
# syntax. shlex handles the quoting; a naive sed does not — `-V '"'"'PVE'"'"'`
# has to split into two argv entries, not one.
for line in sys.stdin:
    for tok in shlex.split(line.strip()):
        sys.stdout.write(tok + "\0")
'
)
test "${#MKISOFS_ARGS[@]}" -gt 5 || { echo "could not read boot layout"; exit 1; }

echo "==> extracting"
xorriso -osirrox on -indev "$SRC_ISO" -extract / "$WORK/iso" >/dev/null 2>&1
chmod -R u+w "$WORK/iso"

echo "==> injecting answer file"
cp "$ANSWER" "$WORK/iso/answer.toml"
# `proxmox-fetch-answer` reads /cdrom/auto-installer-mode.toml (confirmed by
# reading the binary shipped inside the ISO). `mode` selects iso|http|partition.
printf 'mode = "iso"\n' > "$WORK/iso/auto-installer-mode.toml"

echo "==> patching grub.cfg"
# Three edits, each for a reason:
#  - `if true`: the stock menu only offers the automated entry when GRUB finds
#    auto-installer-mode.toml, and GRUB resolves that relative path against its
#    own prefix, not the ISO root. Forcing it removes the dependency entirely.
#  - default/timeout: boot it without a keystroke.
#  - console=ttyS0: the stock entry is `quiet splash=silent` with no serial, so
#    a headless run would have no output at all to diagnose.
python3 - "$WORK/iso/boot/grub/grub.cfg" <<'PY'
import sys
p = sys.argv[1]
t = open(p).read()
t = t.replace("if [ -f auto-installer-mode.toml ]; then", "if true; then", 1)
t = t.replace("rw quiet splash=silent proxmox-start-auto-installer",
              "rw splash=verbose proxmox-start-auto-installer console=tty0 console=ttyS0,115200")
t = t.replace("set timeout=10", "set default=0\n    set timeout=1", 1)
open(p, "w").write(t)
PY
grep -q "console=ttyS0" "$WORK/iso/boot/grub/grub.cfg"
grep -q "if true; then" "$WORK/iso/boot/grub/grub.cfg"

echo "==> authoring $(basename "$OUT_ISO")"
rm -f "$OUT_ISO"
xorriso -as mkisofs "${MKISOFS_ARGS[@]}" -o "$OUT_ISO" "$WORK/iso" 2>&1 |
  grep -iE "FAILURE|SORRY|Written to medium" || true

test -s "$OUT_ISO"
ls -lh "$OUT_ISO"
