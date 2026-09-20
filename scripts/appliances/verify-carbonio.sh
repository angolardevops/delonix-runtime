#!/bin/bash
# Boot the Carbonio CE image and prove what a BUILD can prove about it.
#
#   verify-carbonio.sh [image.qcow2]
#
# The image is pre-bootstrap, so there is no web UI to probe. The checks run
# inside the guest, from a cloud-init seed made here, and report over the
# serial console -- the same channel the build itself uses.
#
# What this does NOT prove is listed at the end of the run, on purpose: a green
# result that hid it would read as "the mail server works".
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
IMG=${1:-$(ls -1t "$OUT"/carbonio-*.qcow2 2>/dev/null | grep -v -- '-check' | head -1)}
[ -f "${IMG:-}" ] || { echo "!! no carbonio image found (looked in $OUT)"; exit 1; }
PINS=$(sed -n 's/^CARBONIO_PINS=${CARBONIO_PINS:-"\(.*\)"}$/\1/p' "$HERE/build-carbonio.sh")
[ -n "$PINS" ] || { echo "!! could not read the pinned versions from build-carbonio.sh"; exit 1; }

OVL="$OUT/carbonio-verify-check.qcow2"; SEED="$OUT/.carbonio-verify-seed.iso"
LOG="$OUT/carbonio-verify-serial.log"
TMP=$(mktemp -d); rm -f "$OVL" "$SEED" "$LOG"
trap 'rm -rf "$TMP" "$OVL" "$SEED"' EXIT
qemu-img create -q -f qcow2 -b "$IMG" -F qcow2 "$OVL"

# Carbonio enables ~22 services that start at boot, and a 4 GiB test VM cannot
# carry them: cloud-init's final stage starved and the checks never ran (measured).
# They are disabled in this THROWAWAY overlay only -- the image is untouched -- so
# the database checks can run without the vendor's stated 16 GiB.
export LIBGUESTFS_BACKEND=${LIBGUESTFS_BACKEND:-direct}
if [ -z "${SUPERMIN_KERNEL:-}" ] && [ ! -r "/boot/vmlinuz-$(uname -r)" ]; then
  for k in $(ls -1r /boot/vmlinuz-* 2>/dev/null); do
    kv=${k#/boot/vmlinuz-}
    if [ -r "$k" ] && [ -d "/lib/modules/$kv" ]; then export SUPERMIN_KERNEL=$k SUPERMIN_MODULES=/lib/modules/$kv; break; fi
  done
fi
echo "==> disabling the Carbonio units in the throwaway overlay"
printf '%s\n' 'command "sh -c '"'"'rm -f /etc/systemd/system/*.wants/carbonio-* /etc/systemd/system/*.wants/service-discover* /etc/systemd/system/*.wants/carbonio*.timer'"'"'"' \
  | guestfish -a "$OVL" -i >/dev/null 2>&1 || echo "!! could not edit the overlay"

# The build VM's name must not be what the image carries. This is read from the
# disk BEFORE boot: inside the guest cloud-init has already replaced the hostname
# with this test's own by then, so an in-guest check passed against an image that
# still said `carbonio-build` -- a check that could not fail.
PRE_FAILS=0
IMG_HOST=$(printf 'cat /etc/hostname\n' | guestfish --ro -a "$OVL" -i 2>/dev/null | tr -d '[:space:]')
if [ -n "$IMG_HOST" ] && [ "$IMG_HOST" != carbonio-build ]; then
  echo "CB-VERIFY OK   the image's /etc/hostname is neutral ($IMG_HOST), not the build VM's"
else
  echo "CB-VERIFY FAIL the image's /etc/hostname is '${IMG_HOST:-unreadable}' -- the build VM's name would reach every deployment"
  PRE_FAILS=1
fi

cat > "$TMP/user-data" <<'EOF'
#cloud-config
runcmd:
  - |
    say() { echo "$1" > /dev/console; }
    ok()  { say "CB-VERIFY OK   $1"; }
    bad() { say "CB-VERIFY FAIL $1"; }
    chk() { if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi; }

    # every package is at the pinned version
    for pin in @PINS@; do
      p=${pin%%=*}; v=${pin#*=}
      chk "package $p is $v" "[ \"\$(dpkg-query -W -f='\${Version}' $p)\" = \"$v\" ]"
    done
    HELD=$(apt-mark showhold | wc -l)
    chk "the 28 Carbonio packages are on hold ($HELD held)" "[ $HELD -ge 28 ]"
    chk "carbonio-bootstrap is installed" "command -v carbonio-bootstrap || ls /opt/zextras/bin/carbonio-bootstrap"
    chk "the image record is valid JSON, pre-bootstrap, 28 packages" "python3 -c \"import json;d=json.load(open('/etc/delonix/carbonio-image.json'));assert d['state']=='pre-bootstrap' and len(d['packages'])==28\""

    # the machine identity of the BUILD did not travel
    chk "no database password baked into the image" "[ ! -e /root/.carbonio-db-password ]"

    # PostgreSQL stays on loopback: the guide's 0.0.0.0/0 superuser line is NOT applied
    systemctl start postgresql; sleep 5
    LISTEN=$(ss -H -ltn 'sport = :5432' | awk '{print $4}' | tr '\n' ' ')
    chk "PostgreSQL listens only on loopback ($LISTEN)" "[ -n \"$LISTEN\" ] && ! echo \"$LISTEN\" | grep -Eq '(^| )(0\.0\.0\.0|\*|\[::\]):5432'"
    chk "pg_hba has no world-open line" "! grep -Eq '0\.0\.0\.0/0' /etc/postgresql/16/main/pg_hba.conf"

    # the helper creates the role with a random password, and is idempotent
    chk "carbonio-prepare-db runs" "/usr/local/sbin/carbonio-prepare-db"
    chk "password file is mode 0600" "[ \"\$(stat -c %a /root/.carbonio-db-password)\" = 600 ]"
    chk "carbonio_adm logs in over 127.0.0.1 with it" "PGPASSWORD=\$(cat /root/.carbonio-db-password) psql -h 127.0.0.1 -U carbonio_adm -d carbonio_adm -tAc 'select 1' | grep -q 1"
    H1=$(sha256sum /root/.carbonio-db-password | cut -d' ' -f1)
    /usr/local/sbin/carbonio-prepare-db >/dev/null 2>&1
    chk "a second run does not change the password" "[ \"\$(sha256sum /root/.carbonio-db-password | cut -d' ' -f1)\" = \"$H1\" ]"
    chk "the vendor's parameters were applied" "[ \"\$(su - postgres -c \"psql -tAc 'show max_connections'\")\" = 500 ]"

    say "CB-VERIFY DONE"
    poweroff
EOF
sed -i "s|@PINS@|$PINS|" "$TMP/user-data"
printf 'instance-id: cb-verify-%s\nlocal-hostname: cb-verify\n' "$$" > "$TMP/meta-data"
cloud-localds "$SEED" "$TMP/user-data" "$TMP/meta-data"

if [ -w /dev/kvm ]; then ACCEL=(-enable-kvm -cpu host); else ACCEL=(-cpu max); fi
echo "==> booting $IMG"
timeout 900 qemu-system-x86_64 "${ACCEL[@]}" -m "${MEM:-4096}" -smp 2 \
  -drive file="$OVL",if=virtio,format=qcow2 -drive file="$SEED",if=virtio,format=raw,readonly=on \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$LOG" -no-reboot || true

grep -a "CB-VERIFY" "$LOG" | tr -d '\r' | sed 's/^.*CB-VERIFY /CB-VERIFY /'
grep -aq "CB-VERIFY DONE" "$LOG" || { echo "!! the guest never finished its checks (last lines of $LOG):"; tail -15 "$LOG" | tr -d '\r'; exit 1; }
FAILS=$(( $(grep -ac "CB-VERIFY FAIL" "$LOG") + PRE_FAILS )); OKS=$(grep -ac "CB-VERIFY OK" "$LOG")
echo "==== carbonio verification: $OKS ok, $FAILS failed ($IMG)"
cat <<'NOTE'

NOT proven here, because it cannot be proven without a real deployment:
  * that `carbonio-bootstrap` completes -- it needs an FQDN, working DNS/MX and
    the vendor's stated 16 GiB of RAM;
  * mail flow, the web client, or the admin panel;
  * the video server, which needs the host's public IP in /etc/janus/janus.jcfg.
NOTE
[ "$FAILS" = 0 ]
