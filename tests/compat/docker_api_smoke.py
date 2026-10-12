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

# What Testcontainers does on top of the lifecycle above (measured with
# testcontainers-python 4.8.2, 2026-10-12): it publishes the port on a host
# port the daemon picks, asks for that port, reads `Config.Tty` before any
# logs call, and removes the RUNNING container with `force=True` — Python's
# spelling of a boolean, which this layer once read as false.
tc = c.api.create_container(
    IMAGE,
    command=["sleep", "60"],
    name=NAME + "-tc",
    ports=[80],
    host_config=c.api.create_host_config(port_bindings={80: None}),
)["Id"]
c.api.start(tc)
d = c.api.inspect_container(tc)
check("inspect Config.Tty", "Tty" in d["Config"], str(d["Config"].get("Tty")))
published = c.api.port(tc, 80) or []
check(
    "a port with no host port is published on one",
    bool(published) and bool(published[0].get("HostPort")),
    str(published),
)
# A remove can wait on the host's writeback (DX-8101 answers 504 meanwhile);
# that is retried. A 409 «use --force» is the defect, and is not.
removed, why = False, ""
for _ in range(24):
    try:
        c.api.remove_container(tc, force=True)
        removed = True
        break
    except docker.errors.APIError as e:
        why = str(e)[:120]
        if e.status_code != 504:
            break
        time.sleep(5)
check("remove of a RUNNING container with force=True", removed, why)

print()
if fails:
    print(f"{len(fails)} failed: {', '.join(fails)}")
    sys.exit(1)
print("all good")
