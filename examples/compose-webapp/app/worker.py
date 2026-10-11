"""Background worker of the sample: proves Redis is reachable and writes a heartbeat."""
import os
import socket
import time

while True:
    with socket.create_connection((os.environ["REDIS_HOST"], 6379), timeout=3) as s:
        s.sendall(b"PING\r\n")
        reply = s.recv(16).decode().strip()
    with open("/data/heartbeat", "w") as f:
        f.write(f"{int(time.time())} {reply} {os.environ.get('APP_ENV')}\n")
    time.sleep(2)
