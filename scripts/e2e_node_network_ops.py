#!/usr/bin/env python3
"""E2E scenarios for NetworkService.CreateNetwork / DeleteNetwork and the
operation record, against a running `delonix serve node-api`.

Called by scripts/e2e.sh, one scenario per check:

    BIN=<delonix> NODESOCK=<socket> DELONIX_ROOT=<root> \
        e2e_node_network_ops.py <scenario>

Each scenario reads what the socket answered AND what the engine has
afterwards (the CLI, the files under the state root), and exits non-zero with
the reason on the first thing that is not as stated.
"""

import json
import os
import subprocess
import sys
import time

BIN = os.environ["BIN"]
SOCK = os.environ["NODESOCK"]
ROOT = os.environ["DELONIX_ROOT"]
NETS = "/v1/namespaces/default/networks"


def call(method, path, body=None, headers=()):
    """(status, headers lower-cased, parsed JSON body or None)."""
    cmd = ["curl", "-s", "-D", "-", "-X", method, "--unix-socket", SOCK]
    for h in headers:
        cmd += ["-H", h]
    if body is not None:
        cmd += ["-H", "content-type: application/json", "-d", json.dumps(body)]
    out = subprocess.run(cmd + ["http://localhost" + path], capture_output=True, text=True).stdout
    # `text=True` already turned the header's CRLF into LF.
    head, _, payload = out.replace("\r\n", "\n").partition("\n\n")
    lines = head.splitlines()
    hdrs = {}
    for line in lines[1:]:
        k, _, v = line.partition(":")
        hdrs[k.strip().lower()] = v.strip()
    try:
        doc = json.loads(payload) if payload.strip() else None
    except json.JSONDecodeError:
        doc = payload
    return int(lines[0].split()[1]), hdrs, doc


def cli(*args):
    p = subprocess.run([BIN, *args], capture_output=True, text=True)
    return p.returncode, p.stdout + p.stderr


def ok(cond, what):
    if not cond:
        print("FAILED:", what, file=sys.stderr)
        sys.exit(1)


def create(name, key=None, **extra):
    body = {"name": name, **extra}
    headers = [f"Idempotency-Key: {key}"] if key else []
    return call("POST", NETS, body, headers)


def cleanup(*names):
    for n in names:
        cli("network", "rm", n)


def scenario_create():
    """POST creates the network: the operation ended SUCCEEDED, Location names
    it, and the CLI and the dataplane record both have the network."""
    cleanup("e2op-a")
    code, hdrs, op = create("e2op-a", key="e2op-create-a", labels={"app": "web"})
    ok(code == 200, f"create answered {code}: {op}")
    ok(op["state"] == "OPERATION_STATE_SUCCEEDED", op)
    ok((op["verb"], op["target"]) == ("create", "Network/e2op-a"), op)
    ok(hdrs.get("location") == "/v1/operations/" + op["id"], hdrs)
    ok(op["id"] == "r-e2op-create-a" and op["request_id"] == "e2op-create-a", op)
    target = [l["href"] for l in op["links"] if l["rel"] == "target"]
    ok(target == [NETS + "/e2op-a"], op["links"])
    # The link answers, realized, with the label it was asked with.
    code, _, net = call("GET", target[0])
    ok(code == 200, f"the target link answered {code}")
    ok(net["meta"]["labels"] == {"app": "web"}, net["meta"])
    cond = net["conditions"][0]
    ok((cond["type"], cond["status"]) == ("Realized", "CONDITION_STATUS_TRUE"), cond)
    # And the engine has it: the CLI lists it, and a second CLI create is a conflict.
    rc, out = cli("network", "inspect", "e2op-a")
    ok(rc == 0, f"network inspect rc={rc}: {out}")
    rc, _ = cli("network", "create", "e2op-a")
    ok(rc == 5, f"a CLI create of the same name answered rc={rc}, expected 5 (conflict)")


def scenario_replay():
    """The same request sent again is answered with the first operation; the
    same name without the key is 409; the key on another name is 400."""
    code, _, first = create("e2op-a", key="e2op-create-a", labels={"app": "web"})
    ok(code == 200 and first["id"] == "r-e2op-create-a", (code, first))
    ok(first["state"] == "OPERATION_STATE_SUCCEEDED", first)
    code, _, doc = create("e2op-a")
    ok(code == 409 and doc["grpc_status"] == 6, (code, doc))
    code, _, doc = create("e2op-other", key="e2op-create-a")
    ok(code == 400, (code, doc))
    rc, _ = cli("network", "inspect", "e2op-other")
    ok(rc != 0, "a refused create left a network behind")


def scenario_operations():
    """The operation is readable by id and listed; an unknown id is 404; a
    record whose owner process is gone reads FAILED/Interrupted."""
    code, _, op = call("GET", "/v1/operations/r-e2op-create-a")
    ok(code == 200 and op["state"] == "OPERATION_STATE_SUCCEEDED", (code, op))
    code, hdrs, listed = call("GET", "/v1/operations")
    ok(code == 200, code)
    ok("r-e2op-create-a" in [o["id"] for o in listed["operations"]], listed)
    ok('rel="self"' in hdrs.get("link", ""), hdrs)
    code, _, doc = call("GET", "/v1/operations/nao-existe")
    ok(code == 404 and doc["dx"] == "DX-4000", (code, doc))
    # An operation left RUNNING by a process that no longer exists.
    dead = subprocess.Popen(["true"])
    dead.wait()
    rec = {
        "id": "op-e2interrupted",
        "state": "running",
        "target": "Network/e2op-ghost",
        "verb": "create",
        "created_ms": 1,
        "owner_pid": dead.pid,
        "owner_starttime": 1,
    }
    path = os.path.join(ROOT, "operations", "op-e2interrupted.json")
    with open(path, "w") as f:
        json.dump(rec, f)
    code, _, op = call("GET", "/v1/operations/op-e2interrupted")
    ok(code == 200 and op["state"] == "OPERATION_STATE_FAILED", (code, op))
    ok(op["error"]["reason"] == "Interrupted" and op["error"]["retryable"], op)
    with open(path) as f:
        ok(json.load(f)["state"] == "failed", "the interrupted state was not written back")
    code, _, active = call("GET", "/v1/operations?active_only=true")
    ok("op-e2interrupted" not in [o["id"] for o in active["operations"]], active)
    os.remove(path)


def scenario_refused():
    """What the node API does not create is refused by name and creates
    nothing: the overlay topology (501), another namespace (400), a subnet the
    engine refuses and a name that would leave the store (400, and nothing
    written)."""
    code, _, doc = create("e2op-ov", spec={"topology": "NETWORK_TOPOLOGY_OVERLAY"})
    ok(code == 501 and doc["dx"] == "DX-6001", (code, doc))
    code, _, doc = call("POST", "/v1/namespaces/outro/networks", {"name": "e2op-ns"})
    ok(code == 400, (code, doc))
    code, _, doc = create("e2op-bad", spec={"ipv4_cidr": "not-a-cidr"})
    ok(code == 400 and doc["dx"].startswith("DX-1"), (code, doc))
    code, _, doc = create("../e2op-evil", spec={"ipv4_cidr": "10.231.0.0/24"})
    ok(code == 400, (code, doc))
    ok(not os.path.exists(os.path.join(ROOT, "e2op-evil")), "a name with '..' wrote outside networks/")
    ok(not os.path.isdir(os.path.join(ROOT, "operations")) or all(
        "e2op-bad" not in open(os.path.join(ROOT, "operations", f)).read()
        for f in os.listdir(os.path.join(ROOT, "operations"))), "a refused request wrote an operation")
    for name in ("e2op-ov", "e2op-ns", "e2op-bad"):
        rc, _ = cli("network", "inspect", name)
        ok(rc != 0, f"a refused create left '{name}' behind")


def scenario_delete():
    """DELETE with a stale If-Match is 412 and the network stays; with the
    current ETag it removes the network from the engine and the dataplane
    record; sent again with the same key it is the same answer, and without a
    key it is 404."""
    code, _, doc = call("DELETE", NETS + "/e2op-a", headers=['If-Match: "0000000000000000"'])
    ok(code == 412 and doc["grpc_status"] == 9, (code, doc))
    rc, _ = cli("network", "inspect", "e2op-a")
    ok(rc == 0, "a refused delete removed the network")
    code, hdrs, _ = call("GET", NETS + "/e2op-a")
    etag = hdrs["etag"]
    key = "Idempotency-Key: e2op-delete-a"
    code, hdrs, op = call("DELETE", NETS + "/e2op-a", headers=[f"If-Match: {etag}", key])
    ok(code == 200 and op["state"] == "OPERATION_STATE_SUCCEEDED", (code, op))
    ok((op["verb"], op["target"]) == ("delete", "Network/e2op-a"), op)
    ok(hdrs.get("location") == "/v1/operations/r-e2op-delete-a", hdrs)
    rc, _ = cli("network", "inspect", "e2op-a")
    ok(rc == 4, f"after the delete, network inspect answered rc={rc}, expected 4")
    defs = os.path.join(ROOT, "ingress", "networks")
    left = [f for f in os.listdir(defs) if f.startswith("e2op-a-")] if os.path.isdir(defs) else []
    ok(not left, f"the dataplane record stayed: {left}")
    code, _, again = call("DELETE", NETS + "/e2op-a", headers=[key])
    ok(code == 200 and again["id"] == op["id"], (code, again))
    code, _, doc = call("DELETE", NETS + "/e2op-a")
    ok(code == 404, (code, doc))


def scenario_inuse():
    """A network a container is attached to is refused with 409 / DX-5307,
    naming the container, and stays; once the container is gone it deletes.
    Needs IMG (an image in the store)."""
    img = os.environ["IMG"]
    cleanup("e2op-u")
    code, _, op = create("e2op-u")
    ok(code == 200 and op["state"] == "OPERATION_STATE_SUCCEEDED", (code, op))
    rc, out = cli("container", "run", "-d", "--name", "e2op-user", "--net", "e2op-u", img, "sleep", "300")
    ok(rc == 0, f"container run on the API-created network rc={rc}: {out}")
    try:
        before = len(os.listdir(os.path.join(ROOT, "operations")))
        code, _, doc = call("DELETE", NETS + "/e2op-u")
        ok(code == 409 and doc["dx"] == "DX-5307", (code, doc))
        ok("container e2op-user" in doc["detail"], doc)
        ok(len(os.listdir(os.path.join(ROOT, "operations"))) == before, "a refused delete wrote an operation")
        rc, _ = cli("network", "inspect", "e2op-u")
        ok(rc == 0, "a refused delete removed the network")
        ok(doc["detail"].count("conflict:") == 1, doc)
    finally:
        # With the host's disk saturated a forced remove can give up while the
        # process is still exiting (DX-8101) and keep the record: ask again.
        deadline = time.time() + 180
        while cli("container", "rm", "-f", "e2op-user")[0] not in (0, 4) and time.time() < deadline:
            time.sleep(2)
    code, _, op = call("DELETE", NETS + "/e2op-u")
    ok(code == 200 and op["state"] == "OPERATION_STATE_SUCCEEDED", (code, op))


if __name__ == "__main__":
    globals()["scenario_" + sys.argv[1]]()
