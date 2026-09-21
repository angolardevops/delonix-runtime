#!/bin/bash
# Boot the Wazuh image, let its first boot run, and prove what the README claims.
#
#   verify-wazuh.sh [image.qcow2]
#
# The checks run inside the guest, from a cloud-init seed made here, and report
# over the serial console. Every check is a claim the README makes; a check that
# cannot fail is not one.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
IMG=${1:-$(ls -1t "$OUT"/wazuh*.qcow2 2>/dev/null | grep -v -- '-check' | head -1)}
[ -f "${IMG:-}" ] || { echo "!! no wazuh image found (looked in $OUT)"; exit 1; }
WAZUH_VERSION=$(sed -n 's/^WAZUH_VERSION=${WAZUH_VERSION:-\(.*\)}$/\1/p' "$HERE/build-wazuh.sh")
[ -n "$WAZUH_VERSION" ] || { echo "!! could not read the pinned version from build-wazuh.sh"; exit 1; }

OVL="$OUT/wazuh-verify-check.qcow2"; SEED="$OUT/.wazuh-verify-seed.iso"
LOG="$OUT/wazuh-verify-serial.log"
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

# Read from the disk BEFORE boot: inside the guest the first boot has already
# run, so an in-guest check would pass against an image that shipped the secrets.
PRE_FAILS=0
IMG_HOST=$(printf 'cat /etc/hostname\n' | guestfish --ro -a "$OVL" -i 2>/dev/null | tr -d '[:space:]')
if [ -n "$IMG_HOST" ] && [ "$IMG_HOST" != wazuh-build ]; then
  echo "WAZUH-VERIFY OK   the image's /etc/hostname is neutral ($IMG_HOST), not the build VM's"
else
  echo "WAZUH-VERIFY FAIL the image's /etc/hostname is '${IMG_HOST:-unreadable}'"; PRE_FAILS=$((PRE_FAILS+1))
fi
for f in /etc/wazuh-indexer/certs/root-ca.pem /root/wazuh-passwords.txt /var/ossec/etc/authd.pass /etc/delonix/wazuh/initialised; do
  if printf 'exists %s\n' "$f" | guestfish --ro -a "$OVL" -i 2>/dev/null | grep -q true; then
    echo "WAZUH-VERIFY FAIL the image already contains $f -- every clone would share it"; PRE_FAILS=$((PRE_FAILS+1))
  else
    echo "WAZUH-VERIFY OK   the image does not carry $f"
  fi
done

cat > "$TMP/user-data" <<'EOF'
#cloud-config
runcmd:
  - |
    say() { echo "$1" > /dev/console; }
    ok()  { say "WAZUH-VERIFY OK   $1"; }
    bad() { say "WAZUH-VERIFY FAIL $1"; }
    chk() { if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi; }
    code() { curl -sk -o /dev/null -m 20 -w '%{http_code}' "$@"; }

    chk "the image record says secrets are generated at first boot" "grep -q secrets-generated-at-first-boot /etc/delonix/wazuh-image.json"
    chk "wazuh-manager is @WAZUH_VERSION@" "[ \"\$(dpkg-query -W -f='\${Version}' wazuh-manager)\" = @WAZUH_VERSION@ ]"
    for p in wazuh-manager wazuh-indexer wazuh-dashboard filebeat; do
      chk "$p is on hold" "apt-mark showhold | grep -qx $p"
    done

    # the first boot runs by itself; give it time
    for i in $(seq 1 300); do [ -f /etc/delonix/wazuh/initialised ] && break; sleep 5; done
    chk "first boot completed" "[ -f /etc/delonix/wazuh/initialised ]"
    if [ ! -f /etc/delonix/wazuh/initialised ]; then
      tail -20 /var/log/delonix-wazuh-first-boot.log 2>/dev/null | while read l; do say "first-boot: $l"; done
    fi

    for u in wazuh-indexer wazuh-manager filebeat wazuh-dashboard; do chk "$u is active" "systemctl is-active --quiet $u"; done
    chk "the indexer listens on loopback only" "ss -ltnH | awk '{print \$4}' | grep -q ':9200\$' && ! ss -ltnH | awk '{print \$4}' | grep ':9200\$' | grep -Ev '^(127\\.0\\.0\\.1|\\[::ffff:127\\.0\\.0\\.1\\]):'"
    chk "the manager listens for agents on :1514 and :1515" "ss -ltnH | grep -q ':1514 ' && ss -ltnH | grep -q ':1515 '"

    # credentials: the vendor defaults are refused, the generated ones work
    P=/root/wazuh-passwords.txt
    APASS=$(grep -i "user admin " $P | awk '{print $NF}' | head -1)
    WPASS=$(grep -i "user wazuh " $P | awk '{print $NF}' | head -1)
    chk "generated passwords were written to $P (mode 600)" "[ -n '$APASS' ] && [ -n '$WPASS' ] && [ \"\$(stat -c %a $P)\" = 600 ]"
    chk "the indexer refuses the vendor default admin/admin" "[ \"\$(code -u admin:admin https://127.0.0.1:9200)\" = 401 ]"
    chk "the indexer accepts the generated admin password" "[ \"\$(code -u admin:'$APASS' https://127.0.0.1:9200)\" = 200 ]"
    chk "the manager API refuses the vendor default wazuh/wazuh" "[ \"\$(code -u wazuh:wazuh -X POST https://127.0.0.1:55000/security/user/authenticate)\" = 401 ]"
    chk "the manager API accepts the generated wazuh password" "[ \"\$(code -u wazuh:'$WPASS' -X POST https://127.0.0.1:55000/security/user/authenticate)\" = 200 ]"

    for i in $(seq 1 30); do curl -sk -o /dev/null -m 5 https://127.0.0.1/ && break; sleep 3; done
    chk "the dashboard answers on :443" "curl -sk -m 20 -L https://127.0.0.1/ | grep -qi -e wazuh -e opensearch"

    # enrolment needs the password
    chk "ossec.conf requires a password for agent enrolment" "grep -q '<use_password>yes</use_password>' /var/ossec/etc/ossec.conf"
    chk "enrolment with a wrong password is refused" "printf \"OSSEC A:'probe'\\n\" | timeout 20 openssl s_client -quiet -connect 127.0.0.1:1515 2>&1 | grep -qi 'invalid password'"
    chk "sudoers drop-in gives delonix passwordless sudo" "grep -q 'delonix ALL=(ALL) NOPASSWD:ALL' /etc/sudoers.d/90-delonix"

    # the point of a SIEM: an event goes in, an alert comes out of the indexer
    logger -p auth.info -t sshd "Failed password for invalid user delonixprobe from 203.0.113.9 port 4242 ssh2"
    N=0
    for i in $(seq 1 60); do
      N=$(curl -sk -u admin:"$APASS" 'https://127.0.0.1:9200/wazuh-alerts-*/_count' | python3 -c 'import sys,json; print(json.load(sys.stdin).get("count",0))' 2>/dev/null || echo 0)
      [ "${N:-0}" -ge 1 ] && break; sleep 5
    done
    chk "an event became an alert in the indexer ($N alerts)" "[ ${N:-0} -ge 1 ]"

    say "WAZUH-VERIFY DONE"
    poweroff
EOF
sed -i -e "s|@WAZUH_VERSION@|$WAZUH_VERSION|g" "$TMP/user-data"
printf 'instance-id: wazuh-verify-%s\nlocal-hostname: wazuh-verify\n' "$$" > "$TMP/meta-data"
cloud-localds "$SEED" "$TMP/user-data" "$TMP/meta-data"

if [ -w /dev/kvm ]; then ACCEL=(-enable-kvm -cpu host); else ACCEL=(-cpu max); fi
echo "==> booting $IMG"
timeout 2700 qemu-system-x86_64 "${ACCEL[@]}" -m "${MEM:-8192}" -smp 4 \
  -drive file="$OVL",if=virtio,format=qcow2 -drive file="$SEED",if=virtio,format=raw,readonly=on \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$LOG" -no-reboot || true

grep -a "WAZUH-VERIFY\|first-boot:" "$LOG" | tr -d '\r' | sed 's/^.*\(WAZUH-VERIFY\|first-boot:\)/\1/'
grep -aq "WAZUH-VERIFY DONE" "$LOG" || { echo "!! the guest never finished its checks (last lines of $LOG):"; tail -15 "$LOG" | tr -d '\r'; exit 1; }
FAILS=$(( $(grep -ac "WAZUH-VERIFY FAIL" "$LOG") + PRE_FAILS )); OKS=$(grep -ac "WAZUH-VERIFY OK" "$LOG")
echo "==== wazuh verification: $OKS ok, $FAILS failed ($IMG)"
echo
echo "NOT PROVEN by this script: a real agent enrolling and reporting, the web UI in a"
echo "  browser, vulnerability-detection feeds (they need internet), or cluster mode."
[ "$FAILS" -eq 0 ]
