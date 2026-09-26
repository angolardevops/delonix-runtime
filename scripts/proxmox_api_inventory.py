#!/usr/bin/env python3
"""Proxmox VE API inventory — the denominator behind any coverage claim (ADR-0049).

A route count written by hand is the number this script exists to replace. The
denominator is the schema Proxmox VE itself publishes on every node, without
authentication:

    curl -k https://<node>:8006/pve-docs/api-viewer/apidoc.js -o apidoc.js
    python3 scripts/proxmox_api_inventory.py --extract apidoc.js > docs/proxmox/api-<ver>.routes.json
    python3 scripts/proxmox_api_inventory.py docs/proxmox/api-<ver>.routes.json

The extracted form (one row per method and path, with the privilege, the
return type and the provenance of the fetch) is what the repository commits —
the 4 MB of generated JavaScript is not — and it is what the matrix is
regenerated from. The numerator is what the provider crate actually CALLS,
read from its source (`crates/providers/delonix-proxmox/src/{lib,sdn}.rs` — an
`impl Client` block spans both files, and `--source` scans every file given),
never from a list kept next to it.

Each (method, path) of the schema lands in exactly one of five states:

  * supported+tested    — called by the crate AND seen in a route trace of a
                          run against a real node (`--trace <file>`, written by
                          the client when `DELONIX_PROXMOX_TRACE_ROUTES` is
                          set). The repository commits one such run next to the
                          schema (`docs/proxmox/trace-<ver>.routes`): its `#`
                          header lines carry the provenance of the run (node,
                          version, date, command) and are rendered into the
                          matrix, so a «tested» claim always says WHEN and
                          against WHAT (ADR-0049 D2);
  * supported+untested  — called by the crate, no trace of a live run given;
  * unsupported-by-design — not called ON PURPOSE, reason written in EXCLUDED
                          below (cluster administration, identities, host);
  * not-yet-implemented — real surface the engine does not reach yet;
  * not-available-in-version — a route the crate calls that the named schema
                          does not have (the run fails: the crate would send it).

A route in `unsupported-by-design` is still counted in the denominator. Hiding
it would make the percentage grow without a single new route served — the
same trap the `compatibility compose` matrix was built to avoid.

The method of a call is read from the STATEMENT the path literal sits in
(then the next statement, for a `url()` built first and sent by `.delete(`
right after it); a path whose method cannot be determined is reported as
unclassified and counted as NOT called, so a reading failure never inflates
the number.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter
from pathlib import Path

CRATE_SOURCE = [
    Path("crates/providers/delonix-proxmox/src/lib.rs"),
    Path("crates/providers/delonix-proxmox/src/sdn.rs"),
    Path("crates/providers/delonix-proxmox/src/cluster.rs"),
]

SUPPORTED_TESTED = "supported+tested"
SUPPORTED_UNTESTED = "supported+untested"
UNSUPPORTED = "unsupported-by-design"
NOT_YET = "not-yet-implemented"
NOT_IN_VERSION = "not-available-in-version"

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
    # The guest agent: what serves an engine verb is called (describe's guest block,
    # the quiesced backup, exec, the IP); the rest is excluded one by one, then the
    # remainder, because an agent passthrough is the proxy ADR-0049 D3 rules out.
    ("/nodes/{node}/qemu/{vmid}/agent/set-user-password", "sets a password inside the guest — the engine gives a guest SSH keys through cloud-init (`vm cloud-init`) and never handles a password"),
    ("/nodes/{node}/qemu/{vmid}/agent/file-", "reads or writes an arbitrary file inside the guest — outside the engine's VM contract; `agent/exec` is the one guest command channel it keeps"),
    ("/nodes/{node}/qemu/{vmid}/agent/suspend-", "guest-driven hibernation — the engine's pause is the node's vCPU suspend (`vm pause`), which keeps the guest's memory"),
    ("/nodes/{node}/qemu/{vmid}/agent/fsfreeze-freeze", "the node's snapshot-mode backup freezes and thaws the guest itself (the quiesced backup proves it from the task log); a separate freeze from the engine would open a window where a crash leaves the guest frozen"),
    ("/nodes/{node}/qemu/{vmid}/agent/fsfreeze-thaw", "same as fsfreeze-freeze: the backup thaws the guest itself, and the engine reads `fsfreeze-status` afterwards to prove it"),
    ("/nodes/{node}/qemu/{vmid}/agent/shutdown", "the node's own `status/shutdown` already goes through the agent when `agent=1` (`vm stop` of a guest that answers)"),
    ("/nodes/{node}/qemu/{vmid}/agent/fstrim", "discards free blocks — storage housekeeping with no engine verb"),
    ("/nodes/{node}/qemu/{vmid}/agent", "raw agent passthrough and guest inventory (users, time, timezone, vCPUs, memory blocks) — no engine verb reads them, and a passthrough is the proxy ADR-0049 D3 rules out"),
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




def trace_provenance(trace: str) -> dict[str, str]:
    """The `# key: value` header of a committed trace — who ran it, against what.

    A trace without a header still promotes routes; it just cannot say when or
    against which node, and the matrix then prints only the request count. The
    keys are free-form: the render prints whatever the run wrote down.
    """
    prov: dict[str, str] = {}
    for line in trace.splitlines():
        line = line.strip()
        if not line.startswith("#"):
            continue
        body = line.lstrip("#").strip()
        if ":" in body:
            k, v = body.split(":", 1)
            if k.strip() and v.strip():
                prov[k.strip()] = v.strip()
    return prov


def trace_requests(trace: str) -> int:
    """Request lines in a trace (every non-comment `METHOD /path`)."""
    return sum(1 for ln in trace.splitlines() if ln.strip() and not ln.strip().startswith("#") and " " in ln.strip())


# ---------------------------------------------------------------------------
# The schema
# ---------------------------------------------------------------------------


def extract_apidoc(path: Path) -> list[dict]:
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
                    "protected": info.get("protected"),
                    "perm": info.get("permissions"),
                }
            )
        for child in node.get("children") or []:
            walk(child)

    for node in data:
        walk(node)
    return rows


def load_schema(path: Path) -> tuple[dict, list[dict]]:
    """Either the committed `*.routes.json` (provenance + routes) or a raw `apidoc.js`."""
    if path.suffix == ".json":
        doc = json.loads(path.read_text(encoding="utf-8"))
        return doc.get("provenance", {}), doc["routes"]
    return {"source": str(path)}, extract_apidoc(path)


# ---------------------------------------------------------------------------
# The crate's calls
# ---------------------------------------------------------------------------

_PATH_RE = re.compile(r'"(/(?:nodes|cluster|access|storage|pools|version)[^"]*)"')
# Ordered: a write verb wins over the `.get(` a serde lookup on the same lines would
# match, and a path literal never sits next to two different HTTP verbs.
_METHOD_HINTS = (
    (".delete(", "DELETE"),
    ("put_form(", "PUT"),
    (".put(", "PUT"),
    ("post_form(", "POST"),
    (".post(", "POST"),
    (".get(", "GET"),
)
_STMT_END = (";", "{", "}")


def normalise(raw: str) -> str:
    """Maps the crate's `format!` template to the schema's placeholder names."""
    p = raw.split("?", 1)[0]
    p = p.replace("/nodes/{}/", "/nodes/{node}/")
    p = p.replace("/tasks/{}/", "/tasks/{upid}/")
    p = p.replace("/qemu/{template}/", "/qemu/{vmid}/")
    p = p.replace("/snapshot/{name}/", "/snapshot/{snapname}/")
    p = p.replace("/content/{}", "/content/{volume}")
    p = p.replace("/firewall/ipset/{name}/{}", "/firewall/ipset/{name}/{cidr}")
    return p


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
    # The unit-test module at the end of the file is not the crate: a literal
    # `/nodes/pve/qemu/100/config` in a test is not a call the crate makes.
    source = source.split("#[cfg(test)]", 1)[0]
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


# ---------------------------------------------------------------------------
# The trace of a live run
# ---------------------------------------------------------------------------


def _template_re(path: str) -> re.Pattern:
    parts = [re.escape(seg) if not seg.startswith("{") else "[^/]+" for seg in path.split("/")]
    return re.compile("^" + "/".join(parts) + "$")


def traced_routes(trace: str, rows: list[dict]) -> set[tuple[str, str]]:
    """Maps `METHOD /concrete/path` lines onto the schema's templates.

    A concrete `/nodes/pve/qemu/100/config` matches `/nodes/{node}/qemu/{vmid}/config`;
    the longest template wins when several match, so `/nodes/{node}/qemu/{vmid}`
    never absorbs its own children.
    """
    templates = sorted(
        ((r["method"], r["path"], _template_re(r["path"])) for r in rows),
        key=lambda t: -len(t[1]),
    )
    hit: set[tuple[str, str]] = set()
    for line in trace.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or " " not in line:
            continue
        method, concrete = line.split(" ", 1)
        concrete = concrete.split("?", 1)[0]
        for m, p, rx in templates:
            if m == method and rx.match(concrete):
                hit.add((m, p))
                break
    return hit


# ---------------------------------------------------------------------------
# Classification
# ---------------------------------------------------------------------------


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


def classify(
    rows: list[dict],
    called: set[tuple[str, str]],
    tested: set[tuple[str, str]] | None = None,
) -> list[dict]:
    tested = tested or set()
    out = []
    for r in rows:
        key = (r["method"], r["path"])
        if key in called:
            state = SUPPORTED_TESTED if key in tested else SUPPORTED_UNTESTED
            reason = ""
        else:
            reason = excluded_reason(r["path"]) or ""
            state = UNSUPPORTED if reason else NOT_YET
        out.append({**r, "state": state, "reason": reason, "area": area_of(r["path"])})
    in_schema = {(r["method"], r["path"]) for r in rows}
    for m, p in sorted(c for c in called if c not in in_schema):
        out.append(
            {
                "method": m,
                "path": p,
                "returns": None,
                "allowtoken": None,
                "proxyto": None,
                "protected": None,
                "perm": None,
                "state": NOT_IN_VERSION,
                "reason": "the crate calls it; this schema does not have it",
                "area": area_of(p),
            }
        )
    return out


def summary(rows: list[dict]) -> dict:
    by_area: dict[str, Counter] = {}
    for r in rows:
        by_area.setdefault(r["area"], Counter())[r["state"]] += 1
    total = Counter(r["state"] for r in rows)
    denominator = sum(1 for r in rows if r["state"] != NOT_IN_VERSION)
    return {
        "denominator": denominator,
        "states": dict(total),
        "areas": {a: dict(c) for a, c in by_area.items()},
    }


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

STATE_ORDER = [SUPPORTED_TESTED, SUPPORTED_UNTESTED, UNSUPPORTED, NOT_YET, NOT_IN_VERSION]


def render_markdown(
    provenance: dict,
    rows: list[dict],
    unknown: list[str],
    trace: dict[str, str] | None = None,
) -> str:
    s = summary(rows)
    st = s["states"]
    called = st.get(SUPPORTED_TESTED, 0) + st.get(SUPPORTED_UNTESTED, 0)
    d = s["denominator"]
    out = []
    out.append(f"# Proxmox VE API coverage matrix — {provenance.get('version', '?')}\n")
    out.append("Generated by `scripts/proxmox_api_inventory.py` (ADR-0049). Do not edit; regenerate.\n")
    out.append("## Provenance\n")
    for k in ("product", "version", "source", "fetched", "apidoc_sha256", "apidoc_bytes"):
        if k in provenance:
            out.append(f"- **{k}**: `{provenance[k]}`")
    out.append("")
    if trace is not None:
        out.append("### Live trace\n")
        out.append("The `tested` column comes from ONE run against a real node, recorded with "
                   "`DELONIX_PROXMOX_TRACE_ROUTES` (the header of the trace file, verbatim):\n")
        for k, v in trace.items():
            out.append(f"- **{k}**: `{v}`")
        out.append("")
    else:
        out.append("### Live trace\n")
        out.append("None given — no route is `tested`; every called route is `untested`.\n")
    out.append("## Summary\n")
    out.append(f"- **denominator**: {d} routes (method, path)")
    out.append(f"- **called**: {called} ({100 * called / d:.1f} % of the schema) — "
               f"{st.get(SUPPORTED_TESTED, 0)} seen in a live trace, {st.get(SUPPORTED_UNTESTED, 0)} not")
    out.append(f"- **unsupported by design**: {st.get(UNSUPPORTED, 0)} (each with a written reason)")
    out.append(f"- **not yet implemented**: {st.get(NOT_YET, 0)}")
    out.append(f"- **not available in this version**: {st.get(NOT_IN_VERSION, 0)}")
    if unknown:
        out.append(f"- **unclassified paths in the source** (counted as NOT called): {unknown}")
    out.append("")
    out.append("| area | tested | untested | unsupported | not yet | total |")
    out.append("|---|---:|---:|---:|---:|---:|")
    for area, _ in AREAS:
        ac = s["areas"].get(area, {})
        tot = sum(v for k, v in ac.items() if k != NOT_IN_VERSION)
        if tot == 0:
            continue
        out.append(
            f"| {area} | {ac.get(SUPPORTED_TESTED, 0)} | {ac.get(SUPPORTED_UNTESTED, 0)} | "
            f"{ac.get(UNSUPPORTED, 0)} | {ac.get(NOT_YET, 0)} | {tot} |"
        )
    out.append("")
    out.append("## Routes\n")
    out.append("`returns: string` on a write route is a UPID — the caller waits on the task, never on the answer.\n")
    out.append("| method | path | state | returns | token | reason |")
    out.append("|---|---|---|---|---|---|")
    order = {s_: i for i, s_ in enumerate(STATE_ORDER)}
    for r in sorted(rows, key=lambda r: (order[r["state"]], r["path"], r["method"])):
        token = "no" if r.get("allowtoken") == 0 else ("yes" if r.get("allowtoken") is not None else "")
        out.append(
            f"| {r['method']} | `{r['path']}` | {r['state']} | {r.get('returns') or ''} | {token} | {r.get('reason', '')} |"
        )
    out.append("")
    return "\n".join(out)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("schema", type=Path, help="docs/proxmox/api-<ver>.routes.json, or a raw apidoc.js")
    ap.add_argument("--extract", action="store_true", help="print the routes JSON of an apidoc.js and exit")
    ap.add_argument(
        "--source",
        type=Path,
        nargs="+",
        default=CRATE_SOURCE,
        help="provider crate source file(s) to scan — impl Client spans more than one file",
    )
    ap.add_argument("--trace", type=Path, help="route trace of a live run (DELONIX_PROXMOX_TRACE_ROUTES)")
    ap.add_argument("--json", action="store_true", help="machine-readable output")
    ap.add_argument("--markdown", action="store_true", help="the matrix as Markdown (docs/proxmox/matrix-<ver>.md)")
    args = ap.parse_args(argv)

    provenance, rows = load_schema(args.schema)
    if args.extract:
        json.dump({"provenance": provenance, "routes": rows}, sys.stdout, indent=0, sort_keys=True)
        print()
        return 0

    # Each file's own `#[cfg(test)]` module is stripped BEFORE joining, not
    # after: `client_routes` truncates at the first `#[cfg(test)]` it finds
    # (the unit-test module is not the crate), and a naive join would let
    # `lib.rs`'s own test module — near its end — truncate away every file
    # joined after it, discarding a whole module's routes in silence.
    source_text = "\n".join(
        p.read_text(encoding="utf-8").split("#[cfg(test)]", 1)[0] for p in args.source
    )
    called, unknown = client_routes(source_text)
    trace_text = args.trace.read_text(encoding="utf-8") if args.trace else None
    tested = traced_routes(trace_text, rows) if trace_text is not None else set()
    trace_prov = None
    if trace_text is not None:
        trace_prov = {"file": str(args.trace), **trace_provenance(trace_text)}
        trace_prov["requests"] = str(trace_requests(trace_text))
    classified = classify(rows, called, tested)
    s = summary(classified)
    not_in_version = [(r["method"], r["path"]) for r in classified if r["state"] == NOT_IN_VERSION]

    if args.json:
        json.dump(
            {"provenance": provenance, "trace": trace_prov, "summary": s, "unclassified_paths": unknown, "routes": classified},
            sys.stdout,
            indent=1,
        )
        print()
    elif args.markdown:
        sys.stdout.write(render_markdown(provenance, classified, unknown, trace_prov))
    else:
        st = s["states"]
        called_n = st.get(SUPPORTED_TESTED, 0) + st.get(SUPPORTED_UNTESTED, 0)
        d = s["denominator"]
        print(f"schema routes (method, path): {d}   [{provenance.get('version', '?')}]")
        print(f"  called          {called_n:4}   {100 * called_n / d:.1f}% of the schema "
              f"({st.get(SUPPORTED_TESTED, 0)} tested live, {st.get(SUPPORTED_UNTESTED, 0)} untested)")
        print(f"  unsupported     {st.get(UNSUPPORTED, 0):4}   by design, with a written reason")
        print(f"  not yet         {st.get(NOT_YET, 0):4}")
        print()
        print(f"{'area':16} {'tested':>7} {'untested':>9} {'unsupp.':>8} {'not yet':>8} {'total':>6}")
        for area, _ in AREAS:
            ac = s["areas"].get(area, {})
            tot = sum(v for k, v in ac.items() if k != NOT_IN_VERSION)
            if tot == 0:
                continue
            print(
                f"{area:16} {ac.get(SUPPORTED_TESTED, 0):7} {ac.get(SUPPORTED_UNTESTED, 0):9} "
                f"{ac.get(UNSUPPORTED, 0):8} {ac.get(NOT_YET, 0):8} {tot:6}"
            )
        if unknown:
            print(f"\npaths whose method could not be read from the call site (counted as NOT called): {unknown}")
        if not_in_version:
            print(f"\ncalled but absent from this schema (a route the target version does not have): {not_in_version}")
    # A route the crate calls that the schema lacks is a real finding, not a parse detail.
    return 1 if not_in_version else 0


if __name__ == "__main__":
    sys.exit(main())
