#!/usr/bin/env python3
"""Smoke test of `delonix serve docker-api` with a REAL Docker client.

Exercises the sequence third-party tooling actually uses. Testcontainers,
CI plugins and IDE integrations do NOT call `docker run`: they call
`create` → `start` → `inspect` → `stop` → `remove`, each as a separate API
request, and they read specific fields out of the responses.

That is the sequence here, driven by the official `docker` Python SDK — the
same HTTP wire protocol the Java/Go/Node clients speak. A layer that answers
`docker ps` on the command line and then dies on `inspect`'s `State.Running`
is exactly the failure that makes people give up on a compatibility shim.

Run:
    delonix serve docker-api --addr unix:///tmp/dlx-docker.sock &
    DOCKER_HOST=unix:///tmp/dlx-docker.sock python3 tests/compat/docker_api_smoke.py
"""
import os
import sys
import time

import docker

IMAGE = os.environ.get("SMOKE_IMAGE", "alpine:latest")
NAME = "dlx-compat-smoke"
fails = []


def check(label, cond, detail=""):
    print(f"{'  ok  ' if cond else ' FAIL '} {label}{'  ' + detail if detail else ''}")
    if not cond:
        fails.append(label)


c = docker.from_env()

check("ping", c.ping())
v = c.version()
check("version has ApiVersion", "ApiVersion" in v, v.get("ApiVersion", ""))
info = c.info()
check("info has ServerVersion", "ServerVersion" in info)
check("images list", isinstance(c.api.images(), list))
check("containers list", isinstance(c.api.containers(all=True), list))

# The lifecycle, one API call at a time — the way a test harness drives it.
try:
    c.api.remove_container(NAME, force=True)
except Exception:
    pass

cid = c.api.create_container(IMAGE, command=["sleep", "60"], name=NAME)["Id"]
check("create returns an Id", bool(cid), cid[:12])
c.api.start(cid)
# `start` on an already-running container must be idempotent: `create` here
# also starts, and a client that then calls `start` must not get an error.
c.api.start(cid)
check("start is idempotent", True)

d = c.api.inspect_container(cid)
check("inspect State.Running", d["State"]["Running"] is True)
check("inspect Name", d["Name"].lstrip("/") == NAME, d["Name"])
check("inspect Config.Image", d["Config"]["Image"] == IMAGE, d["Config"]["Image"])

listed = [x for x in c.api.containers() if x["Id"] == cid]
check("shows up in ps", len(listed) == 1)

c.api.rename(cid, NAME + "-2")
check("rename", c.api.inspect_container(cid)["Name"].lstrip("/") == NAME + "-2")

c.api.stop(cid, timeout=2)
for _ in range(20):
    if not c.api.inspect_container(cid)["State"]["Running"]:
        break
    time.sleep(0.5)
check("stop", c.api.inspect_container(cid)["State"]["Running"] is False)

c.api.remove_container(cid, force=True)
gone = not any(x["Id"] == cid for x in c.api.containers(all=True))
check("remove", gone)

print()
if fails:
    print(f"{len(fails)} failed: {', '.join(fails)}")
    sys.exit(1)
print("all good")
