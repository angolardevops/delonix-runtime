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

t0 = time.monotonic()
cid = c.api.create_container(IMAGE, command=["sleep", "60"], name=NAME)["Id"]
create_took = time.monotonic() - t0
check("create returns an Id", bool(cid), cid[:12])
# G-027: the answer must come back while the workload is still running. A create
# that waits for the container tracks its lifetime instead — that is how `docker
# run -d` came back rc=124 at 60 s (measured 2026-09-16) with the container up.
# The budget is 5 s against a 60 s workload, so a regression cannot pass by being
# fast: it would have to wait ~60 s.
check(f"create answers in under 5s (took {create_took:.3f}s)", create_took < 5.0)
check(
    "the container is still running when the create answers",
    c.api.inspect_container(cid)["State"]["Running"] is True,
)
c.api.start(cid)
# `start` on an already-running container must be idempotent: `create` here
# also starts, and a client that then calls `start` must not get an error.
c.api.start(cid)
check("start is idempotent", True)

d = c.api.inspect_container(cid)
check("inspect State.Running", d["State"]["Running"] is True)
check("inspect Name", d["Name"].lstrip("/") == NAME, d["Name"])
check("inspect Config.Image", d["Config"]["Image"] == IMAGE, d["Config"]["Image"])
# HostConfig — the last of the three §8 Testcontainers gaps; clients read
# NetworkMode/Binds/Privileged off it.
check("inspect carries HostConfig", isinstance(d.get("HostConfig"), dict), str(type(d.get("HostConfig"))))
check("HostConfig.NetworkMode is set", bool(d.get("HostConfig", {}).get("NetworkMode")), str(d.get("HostConfig", {}).get("NetworkMode")))

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

# Logs and exec — the two calls that used to 404 and stopped most Testcontainers
# suites (measured §8). `logs()` is `wait_for_logs`' backbone; `exec_*` is
# `.exec()`. docker-py demuxes the stdcopy frames using `Config.Tty` (false
# here), so a correctly framed stream comes back as the bytes the command wrote.
lg = c.api.create_container(
    IMAGE, command=["sh", "-c", "echo ready-marker; sleep 60"], name=NAME + "-lg"
)["Id"]
c.api.start(lg)
logged = b""
for _ in range(40):  # the write lands shortly after start
    logged = c.api.logs(lg, stdout=True, stderr=True)
    if b"ready-marker" in logged:
        break
    time.sleep(0.25)
check("logs stream carries the container's stdout", b"ready-marker" in logged, repr(logged[:60]))

e_ok = c.api.exec_create(lg, cmd=["echo", "exec-marker"])["Id"]
out = c.api.exec_start(e_ok)
check("exec stdout comes back", b"exec-marker" in out, repr(out[:60]))
check("exec ExitCode is 0", c.api.exec_inspect(e_ok)["ExitCode"] == 0)

e_err = c.api.exec_create(lg, cmd=["sh", "-c", "echo boom 1>&2; exit 7"])["Id"]
err_out = c.api.exec_start(e_err)
check("exec stderr comes back", b"boom" in err_out, repr(err_out[:60]))
check("exec ExitCode reflects a non-zero exit", c.api.exec_inspect(e_err)["ExitCode"] == 7)

c.api.remove_container(lg, force=True)

# stdout and stderr come back on SEPARATE streams (API-created containers log in
# CRI format), so a client that asks for only one gets only that one — the demux
# docker-py does off the stdcopy header, and what a stderr-only wait relies on.
sp = c.api.create_container(
    IMAGE, command=["sh", "-c", "echo OUT-line; echo ERR-line 1>&2; sleep 60"], name=NAME + "-sp"
)["Id"]
c.api.start(sp)
both = b""
for _ in range(40):
    both = c.api.logs(sp, stdout=True, stderr=True)
    if b"OUT-line" in both and b"ERR-line" in both:
        break
    time.sleep(0.25)
only_out = c.api.logs(sp, stdout=True, stderr=False)
only_err = c.api.logs(sp, stdout=False, stderr=True)
check("stdout stream has only stdout", b"OUT-line" in only_out and b"ERR-line" not in only_out, repr(only_out[:60]))
check("stderr stream has only stderr", b"ERR-line" in only_err and b"OUT-line" not in only_err, repr(only_err[:60]))
c.api.remove_container(sp, force=True)

# A one-shot stats sample (what Dev Containers / GitLab Runner read): the cpu and
# memory numbers come from the container's cgroup, two reads apart for a real %.
stc = c.api.create_container(IMAGE, command=["sleep", "60"], name=NAME + "-st")["Id"]
c.api.start(stc)
st = c.api.stats(stc, stream=False)
check(
    "stats carries cpu_stats.cpu_usage.total_usage",
    st.get("cpu_stats", {}).get("cpu_usage", {}).get("total_usage") is not None,
    str(st.get("cpu_stats", {}).get("cpu_usage", {}).get("total_usage")),
)
check(
    "stats carries memory usage and limit",
    st.get("memory_stats", {}).get("usage") is not None and st.get("memory_stats", {}).get("limit"),
    str(st.get("memory_stats", {})),
)
c.api.remove_container(stc, force=True)

# Label filtering — the server-side filter label-based reaping relies on (what
# Testcontainers would otherwise hand to Ryuk). A client lists `label=…` and gets
# only the matching containers.
lab = c.api.create_container(
    IMAGE, command=["sleep", "60"], name=NAME + "-lab", labels={"org.testcontainers": "true"}
)["Id"]
c.api.start(lab)
tagged = c.api.containers(all=True, filters={"label": "org.testcontainers=true"})
check("label filter returns the tagged container", any(x["Id"] == lab for x in tagged))
check(
    "label filter excludes the untagged ones",
    all(x["Labels"].get("org.testcontainers") == "true" for x in tagged),
    f"{len(tagged)} returned",
)
c.api.remove_container(lab, force=True)

print()
if fails:
    print(f"{len(fails)} failed: {', '.join(fails)}")
    sys.exit(1)
print("all good")
