#!/bin/bash
# Boot the GLPI image and prove what its README will claim.
#
#   verify-glpi.sh [image.qcow2]
#
# The checks run inside the guest, from a cloud-init seed made here, and report
# over the serial console -- the same channel the build uses. Every check is a
# claim the README makes; a check that cannot fail is not one.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
IMG=${1:-$(ls -1t "$OUT"/glpi*.qcow2 2>/dev/null | grep -v -- '-check' | head -1)}
[ -f "${IMG:-}" ] || { echo "!! no glpi image found (looked in $OUT)"; exit 1; }
GLPI_VERSION=$(sed -n 's/^GLPI_VERSION=${GLPI_VERSION:-\(.*\)}$/\1/p' "$HERE/build-glpi.sh")
AGENT_VERSION=$(sed -n 's/^GLPI_AGENT_VERSION=${GLPI_AGENT_VERSION:-\(.*\)}$/\1/p' "$HERE/build-glpi.sh")
[ -n "$GLPI_VERSION" ] && [ -n "$AGENT_VERSION" ] || { echo "!! could not read the pinned versions from build-glpi.sh"; exit 1; }

OVL="$OUT/glpi-verify-check.qcow2"; SEED="$OUT/.glpi-verify-seed.iso"
LOG="$OUT/glpi-verify-serial.log"
TMP=$(mktemp -d); rm -f "$OVL" "$SEED" "$LOG"
trap 'rm -rf "$TMP" "$OVL" "$SEED"' EXIT
qemu-img create -q -f qcow2 -b "$IMG" -F qcow2 "$OVL"

export LIBGUESTFS_BACKEND=${LIBGUESTFS_BACKEND:-direct}
if [ -z "${SUPERMIN_KERNEL:-}" ] && [ ! -r "/boot/vmlinuz-$(uname -r)" ]; then
  for k in $(ls -1r /boot/vmlinuz-* 2>/dev/null); do
    kv=${k#/boot/vmlinuz-}
    if [ -r "$k" ] && [ -d "/lib/modules/$kv" ]; then export SUPERMIN_KERNEL=$k SUPERMIN_MODULES=/lib/modules/$kv; break; fi
  done
fi

# Read from the disk BEFORE boot: inside the guest cloud-init has already
# replaced the hostname, so an in-guest check would pass against an image that
# still carried the build VM's name.
PRE_FAILS=0
IMG_HOST=$(printf 'cat /etc/hostname\n' | guestfish --ro -a "$OVL" -i 2>/dev/null | tr -d '[:space:]')
if [ -n "$IMG_HOST" ] && [ "$IMG_HOST" != glpi-build ]; then
  echo "GLPI-VERIFY OK   the image's /etc/hostname is neutral ($IMG_HOST), not the build VM's"
else
  echo "GLPI-VERIFY FAIL the image's /etc/hostname is '${IMG_HOST:-unreadable}'"
  PRE_FAILS=1
fi

cat > "$TMP/user-data" <<'EOF'
#cloud-config
runcmd:
  - |
    say() { echo "$1" > /dev/console; }
    ok()  { say "GLPI-VERIFY OK   $1"; }
    bad() { say "GLPI-VERIFY FAIL $1"; }
    chk() { if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi; }
    Q() { mysql -N glpi -e "$1"; }
    G=/var/www/glpi

    for u in mariadb nginx php8.3-fpm cron; do chk "$u is active" "systemctl is-active --quiet $u"; done
    for i in $(seq 1 30); do curl -s -o /dev/null -m 3 http://127.0.0.1/ && break; sleep 2; done

    chk "GLPI schema version is @GLPI_VERSION@" "[ \"\$(Q \"SELECT value FROM glpi_configs WHERE name='version' AND context='core'\")\" = @GLPI_VERSION@ ]"
    chk "the login page answers and says GLPI" "curl -s -m 30 -L http://127.0.0.1/ | grep -qi glpi"
    # the document root is public/: the config directory must NOT be servable
    chk "config/config_db.php is not served over HTTP" "[ \"\$(curl -s -o /dev/null -w '%{http_code}' -m 20 http://127.0.0.1/config/config_db.php)\" != 200 ]"
    chk "the database listens on loopback only" "! ss -ltnH | awk '{print \$4}' | grep ':3306\$' | grep -v '^127.0.0.1:'"
    chk "GLPI's own requirements check passes" "sudo -u www-data php $G/bin/console system:check_requirements --no-interaction"

    # credentials
    HASH=$(Q "SELECT password FROM glpi_users WHERE name='glpi'")
    chk "login glpi/delonix-admin verifies" "php -r 'exit(password_verify(\"delonix-admin\", \$argv[1]) ? 0 : 1);' '$HASH'"
    chk "the vendor default glpi/glpi is refused" "! php -r 'exit(password_verify(\"glpi\", \$argv[1]) ? 0 : 1);' '$HASH'"
    chk "the sample accounts tech/normal/post-only are inactive" "[ \"\$(Q \"SELECT COUNT(*) FROM glpi_users WHERE name IN ('tech','normal','post-only') AND is_active=1\")\" = 0 ]"

    # the agent
    chk "glpi-agent is @AGENT_VERSION@ and on hold" "dpkg-query -W -f='\${Version}' glpi-agent | grep -Eq '^([0-9]+:)?@AGENT_VERSION@' && apt-mark showhold | grep -qx glpi-agent"
    chk "the agent's status page listens on loopback only" "! ss -ltnH | awk '{print \$4}' | grep ':62354\$' | grep -v '^127.0.0.1:'"
    chk "GLPI's cron is installed" "[ -f /etc/cron.d/glpi ]"
    chk "native inventory is enabled in GLPI" "[ \"\$(Q \"SELECT value FROM glpi_configs WHERE context='inventory' AND name='enabled_inventory'\")\" = 1 ]"
    chk "sudoers drop-in gives delonix passwordless sudo" "grep -q 'delonix ALL=(ALL) NOPASSWD:ALL' /etc/sudoers.d/90-delonix"

    # the point of shipping the agent: the machine shows up in its own GLPI
    systemctl restart glpi-agent >/dev/null 2>&1; glpi-agent --force >/dev/null 2>&1 &
    N=0
    for i in $(seq 1 48); do N=$(Q "SELECT COUNT(*) FROM glpi_computers" 2>/dev/null || echo 0); [ "${N:-0}" -ge 1 ] && break; sleep 5; done
    chk "the agent inventoried this machine into GLPI ($N computer)" "[ ${N:-0} -ge 1 ]"

    say "GLPI-VERIFY DONE"
    poweroff
EOF
sed -i -e "s|@GLPI_VERSION@|$GLPI_VERSION|g" -e "s|@AGENT_VERSION@|$AGENT_VERSION|g" "$TMP/user-data"
printf 'instance-id: glpi-verify-%s\nlocal-hostname: glpi-verify\n' "$$" > "$TMP/meta-data"
cloud-localds "$SEED" "$TMP/user-data" "$TMP/meta-data"

if [ -w /dev/kvm ]; then ACCEL=(-enable-kvm -cpu host); else ACCEL=(-cpu max); fi
echo "==> booting $IMG"
timeout 900 qemu-system-x86_64 "${ACCEL[@]}" -m "${MEM:-4096}" -smp 2 \
  -drive file="$OVL",if=virtio,format=qcow2 -drive file="$SEED",if=virtio,format=raw,readonly=on \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$LOG" -no-reboot || true

grep -a "GLPI-VERIFY" "$LOG" | tr -d '\r' | sed 's/^.*GLPI-VERIFY /GLPI-VERIFY /'
grep -aq "GLPI-VERIFY DONE" "$LOG" || { echo "!! the guest never finished its checks (last lines of $LOG):"; tail -15 "$LOG" | tr -d '\r'; exit 1; }
FAILS=$(( $(grep -ac "GLPI-VERIFY FAIL" "$LOG") + PRE_FAILS )); OKS=$(grep -ac "GLPI-VERIFY OK" "$LOG")
echo "==== glpi verification: $OKS ok, $FAILS failed ($IMG)"
echo
echo "NOT PROVEN by this script: the web UI in a browser, LDAP/mail-collector"
echo "  integrations, or a second machine reporting to this GLPI."
[ "$FAILS" -eq 0 ]
