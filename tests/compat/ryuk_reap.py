#!/usr/bin/env python3
"""Ryuk — Testcontainers' reaper — runs AND reaps on `delonix serve docker-api`.

Not a workaround (`TESTCONTAINERS_RYUK_DISABLED`), the real thing: Testcontainers
pulls and starts `testcontainers/ryuk`, binds the control socket into it, and
Ryuk removes the labelled resources when the session's connection drops.

Why it works here, and why it is SAFER than Docker:
  * The socket accepts only the engine's own uid (`SO_PEERCRED`). A rootless
    container's `root` (uid 0) maps to the engine's uid on the host, so Ryuk —
    which runs as root in its image — is recognised as the engine itself and
    accepted. No other user's process is.
  * On Docker the daemon is root, so a container with the socket bound in is
    host-root — the canonical container escape. On delonix the socket grants
    only the UNPRIVILEGED engine user's scope: no host root, by construction.
  * Ryuk reaps BY LABEL, and the engine honours the `label=` filter server-side,
    so Ryuk removes exactly the session's resources — never everything.

Run:
    delonix serve docker-api --addr unix:///tmp/dlx-docker.sock &
    DOCKER_HOST=unix:///tmp/dlx-docker.sock python3 tests/compat/ryuk_reap.py
"""
import os
import subprocess
import sys
import time

os.environ["TESTCONTAINERS_RYUK_DISABLED"] = "false"
os.environ.setdefault("RYUK_RECONNECTION_TIMEOUT", "4s")

import docker  # noqa: E402

IMAGE = os.environ.get("SMOKE_IMAGE", "alpine:3.20")
HOST = os.environ["DOCKER_HOST"]
fails = []


def check(label, cond, detail=""):
    print(f"{'  ok  ' if cond else ' FAIL '} {label}{'  ' + detail if detail else ''}")
    if not cond:
        fails.append(label)


# A child process starts a container with Ryuk enabled and then CRASHES
# (`os._exit`) without stopping it — exactly the orphan Ryuk exists to clean up.
child = r"""
import os
os.environ["TESTCONTAINERS_RYUK_DISABLED"] = "false"
os.environ.setdefault("RYUK_RECONNECTION_TIMEOUT", "4s")
from testcontainers.core.container import DockerContainer
from testcontainers.core.waiting_utils import wait_for_logs
c = DockerContainer(os.environ["SMOKE_IMAGE"]).with_command("sh -c 'echo ready; sleep 300'")
c.start()
wait_for_logs(c, "ready", timeout=30)
print("ORPHAN_CID=" + c.get_wrapped_container().id, flush=True)
os._exit(0)
"""
out = subprocess.run([sys.executable, "-c", child], capture_output=True, text=True, timeout=120)
cid = ""
for line in out.stdout.splitlines():
    if line.startswith("ORPHAN_CID="):
        cid = line[len("ORPHAN_CID=") :]
check("Testcontainers started the orphan with Ryuk enabled", bool(cid), out.stderr[-200:] if not cid else cid[:12])
if not cid:
    sys.exit(1)

c = docker.from_env()
ryuk_running = any("ryuk" in x.get("Image", "") for x in c.api.containers(all=True))
check("a real Ryuk container was pulled and run", ryuk_running)

# Ryuk reaps after the dropped connection. Poll for the orphan to vanish.
reaped = False
for _ in range(20):
    time.sleep(2)
    if not any(x["Id"] == cid for x in c.api.containers(all=True)):
        reaped = True
        break
check("Ryuk reaped the orphan container", reaped)
check(
    "Ryuk reaped itself and all testcontainers-labelled resources",
    not any("ryuk" in x.get("Image", "") for x in c.api.containers(all=True))
    and not c.api.containers(all=True, filters={"label": "org.testcontainers=true"}),
)

print()
if fails:
    print(f"{len(fails)} failed: {', '.join(fails)}")
    sys.exit(1)
print("Ryuk runs and reaps on delonix — rootless, label-scoped, no host root")
