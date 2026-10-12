#!/usr/bin/env python3
"""ADR-0047 spike probe: a TCP listener for the holder netns.

It answers every HTTP request with the two addresses the L7 proxy would have at
`accept()`: the PEER (who is asking) and the LOCAL address the connection was
delivered to (which holder interface it came in on). It also appends one line
per connection to a log, so a measurement does not depend on the client
printing the body.

Usage (inside the holder netns): peer_probe.py <port> <logfile>
"""
import socket
import sys
import time

port = int(sys.argv[1])
log = sys.argv[2]

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", port))
srv.listen(64)
while True:
    conn, peer = srv.accept()
    local = conn.getsockname()
    try:
        conn.settimeout(3)
        req = b""
        while b"\r\n\r\n" not in req and len(req) < 8192:
            chunk = conn.recv(4096)
            if not chunk:
                break
            req += chunk
        host = ""
        for line in req.split(b"\r\n"):
            if line.lower().startswith(b"host:"):
                host = line.split(b":", 1)[1].strip().decode(errors="replace")
        body = f"peer={peer[0]}:{peer[1]} local={local[0]}:{local[1]} host={host}\n"
        with open(log, "a") as f:
            f.write(f"{time.time():.3f} {body}")
        resp = (
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n"
            f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n{body}"
        )
        conn.sendall(resp.encode())
    except Exception as e:  # noqa: BLE001 - a probe logs and keeps serving
        with open(log, "a") as f:
            f.write(f"{time.time():.3f} error peer={peer} {e}\n")
    finally:
        conn.close()
