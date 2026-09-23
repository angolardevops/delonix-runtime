#!/usr/bin/env python3
"""Proxmox VE API inventory — the denominator behind any coverage claim (ADR-0049).

A route count written by hand is the number this script exists to replace. The
denominator is the schema Proxmox VE itself publishes on every node, without
authentication:

    curl -k https://<node>:8006/pve-docs/api-viewer/apidoc.js -o apidoc.js
    python3 scripts/proxmox_api_inventory.py apidoc.js

and the numerator is what the provider crate actually CALLS, read from its source
(`crates/providers/delonix-proxmox/src/lib.rs`), never from a list kept next to it.

Each (method, path) of the schema lands in exactly one of three states:

  * called   — the crate sends this method to this path;
  * excluded — the engine does not call it ON PURPOSE, and the reason is written
               in EXCLUDED below (cluster administration, users, host config);
  * missing  — neither: real surface the engine does not reach yet.

A route in `excluded` is still counted in the denominator. Hiding it would make
the percentage grow without a single new route served — the same trap the
`compatibility compose` matrix was built to avoid.

The method of a call is read from the enclosing call site (`self.get(`,
`post_form(`, `http.delete(`); a path whose method cannot be determined is
reported as `?` and counted as NOT called, so a reading failure never inflates
the number.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter
from pathlib import Path

CRATE_SOURCE = Path("crates/providers/delonix-proxmox/src/lib.rs")

# Prefixes the engine does not call by decision (ADR-0049 D3): administration of the
# cluster, of identities and of the host is not a VM operation. Order matters: the
# first matching prefix wins, and a more specific prefix goes before a wider one.
EXCLUDED: list[tuple[str, str]] = [
    ("/access/ticket", "called: the one access route the engine needs (login)"),
    ("/access/", "identity administration (users, groups, roles, ACLs, TFA) — not a VM operation"),
    ("/cluster/config", "cluster membership — provider administration"),
    ("/cluster/ha", "HA policy — cluster-dependent, slice 3 capability discovery first"),
    ("/cluster/replication", "replication jobs — cluster-dependent, slice 3"),
    ("/cluster/backup", "backup schedules — provider administration (slice 2 covers per-VM backup only)"),
    ("/cluster/firewall", "cluster firewall — provider administration"),
    ("/cluster/acme", "ACME accounts/plugins — host certificate administration"),
    ("/cluster/ceph", "Ceph administration — storage provider, not a VM operation"),
    ("/cluster/metrics", "metric servers — host observability administration"),
    ("/cluster/notifications", "notification targets — host administration"),
    ("/cluster/mapping", "resource mappings — host administration"),
    ("/cluster/jobs", "job scheduling — host administration"),
    ("/cluster/options", "datacenter options — provider administration"),
    ("/nodes/{node}/apt", "package management — host administration"),
    ("/nodes/{node}/certificates", "node certificates — host administration"),
    ("/nodes/{node}/ceph", "Ceph node administration"),
    ("/nodes/{node}/firewall", "node firewall — host administration"),
    ("/nodes/{node}/hardware", "host hardware inventory — read-only host administration"),
    ("/nodes/{node}/services", "host services — host administration"),
    ("/nodes/{node}/subscription", "subscription — host administration"),
    ("/nodes/{node}/hosts", "host /etc/hosts — host administration"),
    ("/nodes/{node}/dns", "host DNS — host administration"),
    ("/nodes/{node}/time", "host clock — host administration"),
    ("/nodes/{node}/syslog", "host logs — host administration"),
    ("/nodes/{node}/journal", "host logs — host administration"),
    ("/nodes/{node}/replication", "replication status — cluster-dependent, slice 3"),
    ("/nodes/{node}/vzdump", "node-level dump — slice 2 covers per-VM backup through it, not yet called"),
    ("/nodes/{node}/lxc", "LXC — needs its own container-boundary decision before any route (ADR-0049 D4)"),
]

AREAS: list[tuple[str, str]] = [
    ("qemu", "/qemu"),
    ("lxc", "/lxc"),
    ("sdn", "/sdn"),
    ("storage", "/storage"),
    ("access", "/access"),
    ("pools", "/pools"),
    ("cluster (other)", "/cluster"),
    ("nodes (host)", "/nodes"),
    ("version", "/version"),
]


def load_schema(path: Path) -> list[dict]:
    """Returns one row per (method, path) of `apidoc.js`.

    The file is JavaScript (`const apiSchema = [ … ];`), not JSON: the array is cut
    out by bracket depth and parsed as JSON, which the Proxmox generator emits.
    """
    txt = path.read_text(encoding="utf-8")
    start = txt.index("[")
    depth = 0
    end = None
    for i in range(start, len(txt)):
        c = txt[i]
        if c == "[":
            depth += 1
        elif c == "]":
            depth -= 1
            if depth == 0:
                end = i + 1
                break
    if end is None:
        raise SystemExit("apidoc.js: unbalanced schema array")
    data = json.loads(txt[start:end])
    rows: list[dict] = []

    def walk(node: dict) -> None:
        for method, info in (node.get("info") or {}).items():
            returns = info.get("returns") or {}
            rows.append(
                {
                    "method": method,
                    "path": node.get("path"),
                    "returns": returns.get("type"),
                    "allowtoken": info.get("allowtoken", 1),
                    "proxyto": info.get("proxyto"),
                }
            )
        for child in node.get("children") or []:
            walk(child)

    for node in data:
        walk(node)
    return rows


_PATH_RE = re.compile(r'"(/(?:nodes|cluster|access|storage|pools|version)[^"]*)"')
# Ordered: a write verb wins over the `.get(` a serde lookup on the same lines would
# match, and a path literal never sits next to two different HTTP verbs.
_METHOD_HINTS = (
    (".delete(", "DELETE"),
    (".put(", "PUT"),
    ("post_form(", "POST"),
    (".post(", "POST"),
    (".get(", "GET"),
)


def normalise(raw: str) -> str:
    """Maps the crate's `format!` template to the schema's placeholder names."""
    p = raw.split("?", 1)[0]
    p = p.replace("/nodes/{}/", "/nodes/{node}/")
    p = p.replace("/tasks/{}/", "/tasks/{upid}/")
    p = p.replace("/qemu/{template}/", "/qemu/{vmid}/")
    p = p.replace("/snapshot/{name}/", "/snapshot/{snapname}/")
    return p


_STMT_END = (";", "{", "}")


def _statement(lines: list[str], i: int) -> tuple[int, int]:
    """The line span [start, end] of the statement line `i` belongs to."""
    start = i
    while start > 0 and not lines[start - 1].rstrip().endswith(_STMT_END):
        start -= 1
    end = i
    while end < len(lines) - 1 and not lines[end].rstrip().endswith(_STMT_END):
        end += 1
    return start, end


def client_routes(source: str) -> tuple[set[tuple[str, str]], list[str]]:
    """The (method, path) pairs the crate calls, read from its source.

    The verb is read from the STATEMENT the literal sits in (a `format!` argument
    is inside its `self.post_form(...)` call), and failing that from the next
    statement only — a `url(...)` built first is sent by the `.delete(` right
    after it. A wider window read the verb of the neighbouring function. A path
    with no verb in either is returned in the second list, unclassified, unless
    the same path was already classified at another site.
    """
    lines = source.splitlines()
    called: set[tuple[str, str]] = set()
    unknown: list[str] = []
    for i, line in enumerate(lines):
        if line.lstrip().startswith("//"):
            continue
        for m in _PATH_RE.finditer(line):
            path = normalise(m.group(1))
            start, end = _statement(lines, i)
            candidates = ["\n".join(lines[start : end + 1])]
            if end + 1 < len(lines):
                nstart, nend = _statement(lines, end + 1)
                candidates.append("\n".join(lines[nstart : nend + 1]))
            method = None
            for ctx in candidates:
                method = next((meth for hint, meth in _METHOD_HINTS if hint in ctx), None)
                if method:
                    break
            if method is None:
                unknown.append(path)
            else:
                called.add((method, path))
    known = {p for _, p in called}
    unknown = sorted({p for p in unknown if p not in known})
    return called, unknown


def excluded_reason(path: str) -> str | None:
    for prefix, reason in EXCLUDED:
        if path.startswith(prefix):
            return None if reason.startswith("called:") else reason
    return None


def area_of(path: str) -> str:
    for name, needle in AREAS:
        if needle in path:
            return name
    return "other"


def classify(rows: list[dict], called: set[tuple[str, str]]) -> list[dict]:
    out = []
    for r in rows:
        key = (r["method"], r["path"])
        if key in called:
            state, reason = "called", ""
        else:
            reason = excluded_reason(r["path"]) or ""
            state = "excluded" if reason else "missing"
        out.append({**r, "state": state, "reason": reason, "area": area_of(r["path"])})
    return out


def summary(rows: list[dict]) -> dict:
    by_area: dict[str, Counter] = {}
    for r in rows:
        by_area.setdefault(r["area"], Counter())[r["state"]] += 1
    total = Counter(r["state"] for r in rows)
    return {"total": len(rows), "states": dict(total), "areas": {a: dict(c) for a, c in by_area.items()}}


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("apidoc", type=Path, help="apidoc.js fetched from a Proxmox VE node")
    ap.add_argument("--source", type=Path, default=CRATE_SOURCE, help="provider crate source to scan")
    ap.add_argument("--json", action="store_true", help="machine-readable output")
    args = ap.parse_args(argv)

    rows = load_schema(args.apidoc)
    called, unknown = client_routes(args.source.read_text(encoding="utf-8"))
    not_in_schema = sorted(c for c in called if c not in {(r["method"], r["path"]) for r in rows})
    classified = classify(rows, called)
    s = summary(classified)

    if args.json:
        json.dump(
            {"summary": s, "unclassified_paths": unknown, "called_but_not_in_schema": not_in_schema, "routes": classified},
            sys.stdout,
            indent=1,
        )
        print()
    else:
        c, e, m = s["states"].get("called", 0), s["states"].get("excluded", 0), s["states"].get("missing", 0)
        print(f"schema routes (method, path): {s['total']}")
        print(f"  called   {c:4}   {100 * c / s['total']:.1f}% of the schema")
        print(f"  excluded {e:4}   with a written reason")
        print(f"  missing  {m:4}")
        print()
        print(f"{'area':16} {'called':>7} {'excluded':>9} {'missing':>8} {'total':>6}")
        for area, _ in AREAS:
            ac = s["areas"].get(area, {})
            tot = sum(ac.values())
            if tot == 0:
                continue
            print(f"{area:16} {ac.get('called', 0):7} {ac.get('excluded', 0):9} {ac.get('missing', 0):8} {tot:6}")
        if unknown:
            print(f"\npaths whose method could not be read from the call site (counted as NOT called): {unknown}")
        if not_in_schema:
            print(f"\ncalled but absent from this schema (a route the target version does not have): {not_in_schema}")
    # A route the crate calls that the schema lacks is a real finding, not a parse detail.
    return 1 if not_in_schema else 0


if __name__ == "__main__":
    sys.exit(main())
