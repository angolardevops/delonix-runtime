#!/usr/bin/env bash
# S5 — what D1 would add per request, measured where it can be: (a) the holder
# index that maps a source address to a workload is rebuilt from disk at most
# once per 2 s (`DNS_INDEX_TTL`) — timed through the real DNS server, a query
# after 2.5 s idle (rebuild) against one right after (cached); (b) the proxy's
# own request latency today, host -> proxy -> backend, for scale.
set -u
. "$(dirname "$0")/env.sh"
echo "## records in the index: $(ls "$DELONIX_ROOT"/containers/*.json | wc -l) containers, $(ls "$DELONIX_ROOT"/vms/*.json 2>/dev/null | wc -l) VMs; load $(cut -d' ' -f1-3 /proc/loadavg)"
HN python3 - <<'PY'
import socket, struct, time, statistics
def query(name):
    q = struct.pack(">HHHHHH", 0x1234, 0x0100, 1, 0, 0, 0)
    for p in name.split("."):
        q += bytes([len(p)]) + p.encode()
    q += b"\x00" + struct.pack(">HH", 1, 1)
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(3)
    t = time.perf_counter(); s.sendto(q, ("10.251.0.1", 53)); s.recv(512); d = time.perf_counter() - t
    s.close(); return d * 1000
cold, warm = [], []
for _ in range(12):
    time.sleep(2.5); cold.append(query("be-a.teama.delonix.internal"))
    warm.append(query("be-a.teama.delonix.internal"))
f = lambda v: f"median {statistics.median(v):.2f} ms, max {max(v):.2f} ms"
print("DNS after 2.5 s idle (index rebuilt):", f(cold))
print("DNS right after (index cached):      ", f(warm))
PY
echo "## host -> proxy -> backend, 50 sequential requests"
for i in $(seq 50); do curl -s -o /dev/null -w '%{time_total}\n' -H 'Host: app.test' "http://127.0.0.1:$PORT/"; done \
  | python3 -c 'import sys,statistics; v=sorted(float(x)*1000 for x in sys.stdin); print(f"median {statistics.median(v):.2f} ms, p90 {v[int(len(v)*0.9)-1]:.2f} ms, max {v[-1]:.2f} ms")'
