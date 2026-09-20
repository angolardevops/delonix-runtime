#!/bin/bash
# Boot the monitoring image and prove it does what it promises -- not merely
# that two ports answer, which is all verify-boot.sh can say.
#
#   verify-monitoring.sh [image.qcow2]
#
# Prometheus, Alertmanager, Loki and the exporters listen on 127.0.0.1 inside
# the guest, so they cannot be probed from the host. Grafana reaches them over
# loopback, which makes Grafana the honest witness: a datasource that answers
# through it is a datasource an operator will actually be able to use.
#
# Every check is a claim the README makes. A check that cannot fail is not one.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-$(pwd)}
IMG=${1:-$(ls -1t "$OUT"/monitoring-*.qcow2 2>/dev/null | grep -v -- '-check' | head -1)}
[ -f "${IMG:-}" ] || { echo "!! no monitoring image found (looked in $OUT)"; exit 1; }

ZP=${ZP:-18081}; GP=${GP:-13000}; SP=${SP:-11514}
OVL="$OUT/monitoring-verify-check.qcow2"; PIDF="$OUT/monitoring-verify.pid"
rm -f "$OVL" "$PIDF"
qemu-img create -q -f qcow2 -b "$IMG" -F qcow2 "$OVL"
if [ -w /dev/kvm ]; then ACCEL=(-enable-kvm -cpu host); else ACCEL=(-cpu max); fi
qemu-system-x86_64 "${ACCEL[@]}" -m 4096 -smp 2 \
  -drive file="$OVL",if=virtio,format=qcow2 \
  -netdev "user,id=n0,hostfwd=tcp::$ZP-:80,hostfwd=tcp::$GP-:3000,hostfwd=udp::$SP-:1514,hostfwd=tcp::$SP-:1514" -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$OUT/monitoring-verify-serial.log" -pidfile "$PIDF" &
trap 'kill "$(cat "$PIDF" 2>/dev/null)" 2>/dev/null; wait 2>/dev/null; rm -f "$OVL" "$PIDF"' EXIT

echo "==> waiting for Grafana"
for i in $(seq 1 60); do
  curl -sf -m 3 "http://127.0.0.1:$GP/api/health" >/dev/null && break
  sleep 5
  [ "$i" = 60 ] && { echo "!! Grafana never answered"; exit 1; }
done

rc=0
ok()   { echo "OK   $1"; }
bad()  { echo "FAIL $1"; rc=1; }
check() { if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi; }
G="curl -s -m 60 -u admin:delonix-admin http://127.0.0.1:$GP"
Z="http://127.0.0.1:$ZP/api_jsonrpc.php"; H='Content-Type: application/json-rpc'
J() { python3 -c "import sys,json; d=json.load(sys.stdin); $1"; }

# Give the first scrapes a moment: an empty result 10 s after boot is not a fault.
sleep 45

# --- credentials -------------------------------------------------------
check "Zabbix login Admin/delonix-admin" \
  "curl -s -m 20 $Z -H '$H' -d '{\"jsonrpc\":\"2.0\",\"method\":\"user.login\",\"params\":{\"username\":\"Admin\",\"password\":\"delonix-admin\"},\"id\":1}' | grep -q '\"result\"'"
check "Zabbix vendor default password is refused" \
  "curl -s -m 20 $Z -H '$H' -d '{\"jsonrpc\":\"2.0\",\"method\":\"user.login\",\"params\":{\"username\":\"Admin\",\"password\":\"zabbix\"},\"id\":1}' | grep -q 'Incorrect'"
check "Grafana vendor default admin/admin is refused" \
  "[ \"\$(curl -s -o /dev/null -w '%{http_code}' -u admin:admin http://127.0.0.1:$GP/api/datasources)\" = 401 ]"

# --- every provisioned datasource is healthy ---------------------------
# alertmanager is checked below through its own API: Grafana implements no
# health endpoint for that datasource type (measured: HTTP 500), so a health
# check here would fail on a perfectly working datasource.
for uid in zabbix zabbix-postgres prometheus loki; do
  check "datasource '$uid' health" \
    "$G/api/datasources/uid/$uid/health | grep -qE '\"status\": *\"(OK|success)\"|\"status\":\"OK\"'"
done

# --- and each one returns DATA, not just a green light -----------------
q() { # q <uid> <type> <json-target-fragment>
  $G/api/ds/query -H 'Content-Type: application/json' \
    -d "{\"queries\":[{\"refId\":\"A\",\"datasource\":{\"uid\":\"$1\",\"type\":\"$2\"},$3}],\"from\":\"now-15m\",\"to\":\"now\"}"
}
check "PostgreSQL: Zabbix hosts are readable through grafana_ro" \
  "q zabbix-postgres grafana-postgresql-datasource '\"format\":\"table\",\"rawQuery\":true,\"rawSql\":\"SELECT count(*) AS n FROM hosts WHERE status=0\"' | J 'sys.exit(0 if d[\"results\"][\"A\"][\"frames\"][0][\"data\"][\"values\"][0][0]>=1 else 1)'"
check "PostgreSQL: grafana_ro CANNOT read Zabbix users (password hashes)" \
  "q zabbix-postgres grafana-postgresql-datasource '\"format\":\"table\",\"rawQuery\":true,\"rawSql\":\"SELECT passwd FROM users\"' | grep -qi 'permission denied'"
check "Prometheus: node_exporter is scraped" \
  "q prometheus prometheus '\"expr\":\"up{job=\\\"node\\\"}\",\"instant\":true' | J 'sys.exit(0 if d[\"results\"][\"A\"][\"frames\"][0][\"data\"][\"values\"][1][0]==1 else 1)'"
check "Prometheus: blackbox probes the self targets (probe_success == 1)" \
  "q prometheus prometheus '\"expr\":\"min(probe_success)\",\"instant\":true' | J 'sys.exit(0 if d[\"results\"][\"A\"][\"frames\"][0][\"data\"][\"values\"][1][0]==1 else 1)'"
# Loki: a label VALUE that exists proves ingestion under that label. The earlier
# form of this check grepped the /api/ds/query reply for the word "values",
# which is in the schema of an EMPTY answer too -- a check that cannot fail.
LK="$G/api/datasources/proxy/uid/loki/loki/api/v1"
check "Loki: the host journal is ingested under job=journal" \
  "$LK/label/job/values | grep -q '\"journal\"'"
# Real syslog, as a switch or a workstation would send it (RFC 5424), over BOTH
# transports -- the promise is a receiver on :1514, not a config file.
python3 - "$SP" <<'PY' 2>/dev/null
import socket,sys
p=int(sys.argv[1])
socket.socket(socket.AF_INET,socket.SOCK_DGRAM).sendto(b'<134>1 2026-09-20T01:00:00Z verify-udp-host verify 1 ID1 - hello-from-udp',('127.0.0.1',p))
t=socket.create_connection(('127.0.0.1',p),timeout=5); t.sendall(b'<131>1 2026-09-20T01:00:01Z verify-tcp-host verify 2 ID2 - hello-from-tcp\n'); t.close()
PY
sleep 20
NOW=$(date +%s); START=$(( (NOW-3600) * 1000000000 ))
for h in verify-udp-host verify-tcp-host; do
  check "Loki: syslog from $h arrives with host/app/severity labels" \
    "$G/api/datasources/proxy/uid/loki/loki/api/v1/query_range -G --data-urlencode 'query={job=\"syslog\",host=\"$h\",app=\"verify\"}' --data-urlencode start=$START --data-urlencode limit=1 | J 'sys.exit(0 if d[\"data\"][\"result\"] and \"severity\" in d[\"data\"][\"result\"][0][\"stream\"] else 1)'"
done
check "Alertmanager datasource answers" \
  "$G/api/alertmanager/alertmanager/api/v2/status | grep -q 'versionInfo'"

# --- the starter dashboard is really there -----------------------------
check "starter dashboard 'delonix-overview' is provisioned" \
  "$G/api/dashboards/uid/delonix-overview | J 'sys.exit(0 if len(d[\"dashboard\"][\"panels\"])>=8 else 1)'"
check "Zabbix app plugin is enabled" \
  "$G/api/plugins/alexanderzobnin-zabbix-app/settings | J 'sys.exit(0 if d.get(\"enabled\") else 1)'"

# --- what the image says it carries ------------------------------------
echo "==== monitoring verification rc=$rc ($IMG)"
exit $rc
