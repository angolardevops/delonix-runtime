#!/bin/bash
# Boot the FreePBX image and prove what its README will claim.
#
#   verify-freepbx.sh [image.qcow2]
#   VERIFY_ADMIN=random verify-freepbx.sh    # the other first-boot path
#
# The checks run inside the guest, from a cloud-init seed made here, and report
# over the serial console -- the same channel the build uses. Every check is a
# claim the README makes; a check that cannot fail is not one.
#
# The first boot has two paths for the admin password, and one boot proves one:
#   given  (default) -- the seed writes /etc/delonix/freepbx-admin-password, as
#                       a platform creating the VM would;
#   random           -- nothing given, the first boot generates one.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
IMG=${1:-$(ls -1t "$OUT"/freepbx17*.qcow2 2>/dev/null | grep -v -- '-check' | grep -v '\.raw\.' | head -1)}
[ -f "${IMG:-}" ] || { echo "!! no freepbx image found (looked in $OUT)"; exit 1; }
AST_MAJOR=$(sed -n 's/^ASTERISK_MAJOR=${ASTERISK_MAJOR:-\(.*\)}$/\1/p' "$HERE/build-freepbx.sh")
KEY_FPR=$(sed -n 's/^FREEPBX_KEY_FPR=${FREEPBX_KEY_FPR:-\(.*\)}$/\1/p' "$HERE/build-freepbx.sh")
[ -n "$AST_MAJOR" ] && [ -n "$KEY_FPR" ] || { echo "!! could not read the pinned values from build-freepbx.sh"; exit 1; }
MODE=${VERIFY_ADMIN:-given}
case "$MODE" in given|random) ;; *) echo "!! VERIFY_ADMIN is given or random"; exit 2 ;; esac

OVL="$OUT/freepbx-verify-check.qcow2"; SEED="$OUT/.freepbx-verify-seed.iso"
LOG="$OUT/freepbx-verify-serial.log"
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

# Read from the disk BEFORE boot: the build identity must be gone, and the
# first boot must not have happened yet -- a stamp baked into the image would
# mean every clone skips the rotation and keeps the build's secrets.
PRE_FAILS=0
pre() { if [ "$2" = ok ]; then echo "FREEPBX-VERIFY OK   $1"; else echo "FREEPBX-VERIFY FAIL $1"; PRE_FAILS=$((PRE_FAILS+1)); fi; }
IMG_HOST=$(printf 'cat /etc/hostname\n' | guestfish --ro -a "$OVL" -i 2>/dev/null | tr -d '[:space:]')
[ -n "$IMG_HOST" ] && [ "$IMG_HOST" != freepbx-build ] && r=ok || r=fail
pre "the image's /etc/hostname is neutral (${IMG_HOST:-unreadable}), not the build VM's" $r
STAMPED=$(printf 'is-file /var/lib/delonix/freepbx-first-boot.done\n' | guestfish --ro -a "$OVL" -i 2>/dev/null | tr -d '[:space:]')
[ "$STAMPED" = false ] && r=ok || r=fail
pre "the image carries no first-boot stamp, so every clone rotates its own secrets" $r
ADMIN_TXT=$(printf 'is-file /root/freepbx-admin.txt\n' | guestfish --ro -a "$OVL" -i 2>/dev/null | tr -d '[:space:]')
[ "$ADMIN_TXT" = false ] && r=ok || r=fail
pre "the image carries no admin password file" $r
AMI_BUILD_SHA=$(virt-cat -a "$OVL" /etc/delonix/freepbx-image.json 2>/dev/null | sed -n 's/.*"ami_build_secret_sha256": "\([0-9a-f]*\)".*/\1/p')
[ -n "$AMI_BUILD_SHA" ] && r=ok || r=fail
pre "the image records the hash of its build-time AMI secret" $r

GIVEN_PASS=""
if [ "$MODE" = given ]; then
  GIVEN_PASS=$(openssl rand -base64 24 | tr -d '/+=\n' | head -c 20)
fi

cat > "$TMP/user-data" <<EOF
#cloud-config
$( [ "$MODE" = given ] && printf 'write_files:\n  - path: /etc/delonix/freepbx-admin-password\n    permissions: "0600"\n    content: |\n      %s\n' "$GIVEN_PASS" )
runcmd:
  - |
    say() { echo "\$1" > /dev/console; }
    ok()  { say "FREEPBX-VERIFY OK   \$1"; }
    bad() { say "FREEPBX-VERIFY FAIL \$1"; }
    chk() { if eval "\$2" >/dev/null 2>&1; then ok "\$1"; else bad "\$1"; fi; }
    Q() { mysql -N asterisk -e "\$1"; }
    listens_lo_only() { ! ss -ltnH | awk '{print \$4}' | grep ":\$1\\\$" | grep -Ev '^(127\\.0\\.0\\.1|\\[::1\\]):'; }

    for i in \$(seq 1 150); do [ -f /var/lib/delonix/freepbx-first-boot.done ] && break; sleep 2; done
    chk "the first-boot unit ran to the end" "[ -f /var/lib/delonix/freepbx-first-boot.done ]"
    for u in mariadb apache2 fail2ban; do chk "\$u is active" "systemctl is-active --quiet \$u"; done
    chk "Asterisk answers and is @AST_MAJOR@.x" "asterisk -rx 'core show version' | grep -Eq '^Asterisk @AST_MAJOR@\\.'"
    chk "PJSIP is loaded" "asterisk -rx 'module show like res_pjsip.so' | grep -q Running"
    chk "the FreePBX apt key is still the pinned one" "gpg --show-keys --with-colons /etc/apt/trusted.gpg.d/freepbx.gpg | grep -q '^fpr:.*:@KEY_FPR@:'"
    for i in \$(seq 1 30); do curl -s -o /dev/null -m 3 http://127.0.0.1/admin/config.php && break; sleep 2; done
    chk "the admin UI answers and says FreePBX" "curl -s -m 30 -L http://127.0.0.1/admin/config.php | grep -qi freepbx"

    # the admin login: one row, the password from /root/freepbx-admin.txt
    P=\$(sed -n 's/^password: //p' /root/freepbx-admin.txt 2>/dev/null)
    chk "/root/freepbx-admin.txt exists and is 0600" "[ \"\$(stat -c %a /root/freepbx-admin.txt)\" = 600 ]"
    chk "the admin password in it is at least 12 characters" "[ \${#P} -ge 12 ]"
    chk "ampusers holds exactly one admin, with that password" "[ \"\$(Q \"SELECT COUNT(*) FROM ampusers WHERE username='admin' AND password_sha1=SHA1('\$P')\")\" = 1 ] && [ \"\$(Q \"SELECT COUNT(*) FROM ampusers\")\" = 1 ]"
    chk "the given-password file was removed after use" "[ ! -e /etc/delonix/freepbx-admin-password ]"
    if [ "@MODE@" = given ]; then
      chk "the password is the one given at creation" "[ \"\$P\" = '@GIVEN_PASS@' ]"
    else
      chk "the password was generated on first boot" "grep -q 'generated on first boot' /root/freepbx-admin.txt"
    fi

    # the AMI secret
    AMI=\$(Q "SELECT value FROM freepbx_settings WHERE keyword='AMPMGRPASS'" | tr -d '[:space:]')
    chk "the AMI secret is not the build's" "[ \"\$(printf %s \"\$AMI\" | sha256sum | cut -d' ' -f1)\" != @AMI_BUILD_SHA@ ]"
    chk "manager.conf carries the rotated AMI secret" "grep -Eq \"^secret *= *\$AMI\\\$\" /etc/asterisk/manager.conf"
    chk "the AMI (5038) listens on loopback only" "listens_lo_only 5038"
    chk "the database (3306) listens on loopback only" "listens_lo_only 3306"

    # once means once: a second run must not rotate again
    H1=\$(Q "SELECT password_sha1 FROM ampusers WHERE username='admin'")
    systemctl restart delonix-freepbx-first-boot.service >/dev/null 2>&1 || true
    /usr/local/sbin/delonix-freepbx-first-boot >/dev/null 2>&1 || true
    chk "a second first-boot run changes nothing" "[ \"\$(Q \"SELECT password_sha1 FROM ampusers WHERE username='admin'\")\" = \"\$H1\" ]"
    chk "sudoers drop-in gives delonix passwordless sudo" "grep -q 'delonix ALL=(ALL) NOPASSWD:ALL' /etc/sudoers.d/90-delonix"

    say "FREEPBX-VERIFY DONE"
    poweroff
EOF
sed -i -e "s|@AST_MAJOR@|$AST_MAJOR|g" -e "s|@KEY_FPR@|$KEY_FPR|g" -e "s|@MODE@|$MODE|g" \
       -e "s|@GIVEN_PASS@|$GIVEN_PASS|g" -e "s|@AMI_BUILD_SHA@|${AMI_BUILD_SHA:-none}|g" "$TMP/user-data"
printf 'instance-id: freepbx-verify-%s\nlocal-hostname: freepbx-verify\n' "$$" > "$TMP/meta-data"
cloud-localds "$SEED" "$TMP/user-data" "$TMP/meta-data"

if [ -w /dev/kvm ]; then ACCEL=(-enable-kvm -cpu host); else ACCEL=(-cpu max); fi
echo "==> booting $IMG (admin password: $MODE)"
timeout 1200 qemu-system-x86_64 "${ACCEL[@]}" -m "${MEM:-4096}" -smp 2 \
  -drive file="$OVL",if=virtio,format=qcow2 -drive file="$SEED",if=virtio,format=raw,readonly=on \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$LOG" -no-reboot || true

grep -a "FREEPBX-VERIFY" "$LOG" | tr -d '\r' | sed 's/^.*FREEPBX-VERIFY /FREEPBX-VERIFY /'
grep -aq "FREEPBX-VERIFY DONE" "$LOG" || { echo "!! the guest never finished its checks (last lines of $LOG):"; tail -15 "$LOG" | tr -d '\r'; exit 1; }
FAILS=$(( $(grep -ac "FREEPBX-VERIFY FAIL" "$LOG") + PRE_FAILS )); OKS=$(grep -ac "FREEPBX-VERIFY OK" "$LOG")
echo "==== freepbx verification ($MODE): $OKS ok, $FAILS failed ($IMG)"
echo
echo "NOT PROVEN by this script: a SIP registration or a call through the PBX,"
echo "  a login through the web form in a browser, TLS on 5061, or the image"
echo "  reached from outside the VM."
[ "$FAILS" -eq 0 ]
