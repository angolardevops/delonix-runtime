#!/usr/bin/env python3
"""Fail-closed gate on the leaves of ALL SEVEN public surfaces, not just the CLI.

# What this closes

`scripts/cli_exec_ratchet.py` keeps the CLI honest: 273 leaves, 254 of them
invoked under an assertion, and a number derived from a real run instead of a
comment. Nothing did that for the other six surfaces, and the first measurement
of them (2026-10-18, `docs/testing/folhas-inventario.md`) says why it matters:

  * Mgmt API: 1 of 40 operations is exercised over real HTTP.
  * MCP: 0 of 15 tools has ever run through the protocol, and there is NO
    `tools/call` anywhere in the repository.
  * Node API: 24 of 59 RPCs are never invoked by any test.
  * CRI: 5 RPCs have no coverage from critest or from any repo test.
  * Kinds: `Container` is the only one of 25 with all five verbs exercised.

None of those numbers could regress visibly, because none of them was recorded.
Worse, nothing guarded the DENOMINATORS: a new MCP tool, a new mgmt route or a
new node RPC could land with no test and no gate noticing. `cli_baseline.tsv`
only ever covered the CLI.

# The denominator is DERIVED FROM CODE, never from the table

That is the fail-closed property, and it is the whole point. For each surface
this module reads the leaf names out of the source of truth:

  CLI       `scripts/cli_baseline.tsv`          (itself gated by cli-tree.sh --gate)
  node-api  `proto/delonix/node/v1/*.proto`     (`rpc <Name>`)
  mgmt-api  `delonix-mgmt/src/lib.rs`           (`.route("<path>"`)
  mcp       `delonix-mcp/src/lib.rs`            (`#[tool(` → `name = "<tool>"`)
  cri       `delonix-cri/proto/api.proto`       (`rpc <Name>`)
  kinds     `delonix-stack/src/kinds.rs`        (`KindFacts {` → `plural: "<name>"`)

A name the code has and `docs/testing/folhas.tsv` does not is a FAILURE that
names it. So is the reverse: a row for a leaf that no longer exists. A table
that drifts from the code is a table that lies, and this repo has paid for that
twice already (the CLI inventory measured against an installed binary six
commands stale; `libvirt_virsh_inventory.py` still scanning the adapter the
libvirt code moved out of three refactors ago).

Providers are deliberately NOT in this gate. Their denominator is a published
HTTP schema or a `virsh help` of a named version, and it already has its own
instrument (`scripts/proxmox_api_inventory.py`, ADR-0049). Duplicating it here
with a worse extractor would manufacture a second, disagreeing number — which
is exactly the failure mode `docs/cri-conformance.md` is still living with.

# What counts as proof, per class

The class in the table is the claim; the evidence column is what backs it:

  A  proved in a real run      evidence MUST name the check/trace that did it
  B  exists, nothing runs it   evidence MUST say what would run it
  C  only mocks, or only one   evidence MUST say which layer is missing
     encoding
  D  declared UNIMPLEMENTED,   evidence MUST cite the decision (an ADR) or the
     or a known defect         finding
  E  needs lab infrastructure  evidence MUST name the exact missing piece

An `A` with no evidence is a contradiction and fails. A `B`/`C`/`D`/`E` with an
empty reason fails too: "not proved, and nobody said why" is the one state that
must never be writable. This is the same shape as `lab_allowed_skips.txt` —
a skip is allowed only with the reason it cannot be measured.

# The ratchet

`scripts/folhas_baseline.json` records, per surface, how many leaves are class
`A`. It fails when the number goes DOWN (a regression), and when it goes UP
without the baseline moving in the same commit (an unrecorded win, which makes
the next regression invisible). Same two-way rule as `cli_exec_ratchet.py`.

    scripts/folhas_gate.py                  # the gate
    scripts/folhas_gate.py --list           # per-surface breakdown
    scripts/folhas_gate.py --list-missing   # leaves that are not class A
    scripts/folhas_gate.py --skeleton       # emit rows for leaves the table lacks
    scripts/folhas_gate.py --update         # adopt today's counts as the baseline
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent
TABLE = REPO / "docs" / "testing" / "folhas.tsv"
BASELINE = REPO / "scripts" / "folhas_baseline.json"

CLI_BASELINE = REPO / "scripts" / "cli_baseline.tsv"
CLI_TRACE = REPO / "scripts" / "cli_exec_trace.tsv"
NODE_PROTO_DIR = REPO / "proto" / "delonix" / "node" / "v1"
MGMT_SRC = REPO / "crates" / "interfaces" / "delonix-mgmt" / "src" / "lib.rs"
MCP_SRC = REPO / "crates" / "interfaces" / "delonix-mcp" / "src" / "lib.rs"
CRI_PROTO = REPO / "crates" / "interfaces" / "delonix-cri" / "proto" / "api.proto"
KINDS_SRC = REPO / "crates" / "contexts" / "delonix-stack" / "src" / "kinds.rs"
INVENTORY = REPO / "docs" / "testing" / "folhas-inventario.md"

CLASSES = ("A", "B", "C", "D", "E")

# The five verbs a Kind is exercised through. A Kind is one leaf PER VERB: the
# measurement that mattered was per verb, not per Kind — `Container` is the only
# one of 25 with all five, and a per-Kind count would have hidden that.
KIND_VERBS = ("apply", "diff", "plan", "delete", "drift")


def _read(path: pathlib.Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as e:  # a moved file must fail loudly, never silently empty
        sys.exit(f"FAIL  folhas_gate: cannot read {path.relative_to(REPO)}: {e}")


def cli_leaves() -> list[str]:
    """The CLI leaves, from the inventory `cli-tree.sh --gate` already guards."""
    out = []
    for line in _read(CLI_BASELINE).splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        # The baseline is `<impact-class><TAB><leaf>`; the leaf is the last field.
        out.append(line.split("\t")[-1].strip())
    return sorted(set(out))


def cli_executed() -> set[str]:
    """Leaves a real battery run invoked under an assertion.

    Derived from the committed trace, never stored in the table: the trace is
    regenerated from a run (`cli_exec_ratchet.py --from-results … --update`),
    and a copy of it in a second file is a copy that goes stale.
    """
    out = set()
    for line in _read(CLI_TRACE).splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            out.add(line)
    return out


def proto_rpcs(path: pathlib.Path) -> list[str]:
    return sorted(set(re.findall(r"^\s*rpc\s+([A-Za-z0-9_]+)", _read(path), re.M)))


def node_rpcs() -> list[str]:
    names: set[str] = set()
    protos = sorted(NODE_PROTO_DIR.glob("*.proto"))
    if not protos:
        sys.exit(f"FAIL  folhas_gate: no .proto under {NODE_PROTO_DIR.relative_to(REPO)}")
    for p in protos:
        names.update(proto_rpcs(p))
    return sorted(names)


def mgmt_routes() -> list[str]:
    return sorted(set(re.findall(r'\.route\(\s*"([^"]+)"', _read(MGMT_SRC))))


def mcp_tools() -> list[str]:
    """Tool names out of the `#[tool(name = "...")]` attributes.

    Counted from the attribute and not from the handler function name: the
    attribute is what the protocol publishes, and the two differ (`resources.get`
    is served by `resources_get`, `resource.list` by `resource_list`).
    """
    src = _read(MCP_SRC)
    # `#[tool(` and its `name = "..."` can sit on different lines.
    return sorted(set(re.findall(r'#\[tool\(\s*name\s*=\s*"([^"]+)"', src, re.S)))


def cri_rpcs() -> list[str]:
    return proto_rpcs(CRI_PROTO)


def kind_plurals() -> list[str]:
    """The catalogue's plural names, from the `FACTS` table.

    `plural:` and not `kind:` on purpose: `kind:` is a Rust const (`RUNTIME_POLICY`)
    whose value lives elsewhere, while `plural` is the literal string the CLI
    prints in `api-resources` and the one an operator types.
    """
    src = _read(KINDS_SRC)
    # Only literals in the FACTS table, never the `struct KindFacts {` definition.
    body = src.split("struct KindFacts {", 1)[-1]
    return sorted(set(re.findall(r'^\s*plural:\s*"([^"]+)"', body, re.M)))


def kind_leaves() -> list[str]:
    return [f"{p} {v}" for p in kind_plurals() for v in KIND_VERBS]


# Each surface: how to derive its leaf names from code.
SURFACES: dict[str, callable] = {
    "cli": cli_leaves,
    "node-api": node_rpcs,
    "mgmt-api": mgmt_routes,
    "mcp": mcp_tools,
    "cri": cri_rpcs,
    "kinds": kind_leaves,
}


def inventory_counts() -> dict[str, dict[str, int]]:
    """The counts table published in `docs/testing/folhas-inventario.md`.

    Exists because that table was written by hand once and had drifted by the
    first revision: it claimed the CLI had 7 class `B` and 12 class `E` where
    the measurement says 9 and 10. A surface's state living in two files with
    nothing comparing them is the defect this repository keeps paying for — the
    published CRI number disagreeing with the one that has raw logs (G-025) is
    the same shape.

    Returns `{surface: {"A": n, ..., "leaves": n}}` for the rows between the
    `folhas:counts` markers. A missing marker pair is itself a failure: the
    check cannot silently cover nothing.
    """
    text = _read(INVENTORY)
    try:
        block = text.split("<!-- folhas:counts:begin", 1)[1].split("<!-- folhas:counts:end", 1)[0]
    except IndexError:
        return {}
    out: dict[str, dict[str, int]] = {}
    for line in block.splitlines():
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) < 7:
            continue
        surface = cells[0].strip("*` ")
        if surface not in SURFACES:
            continue
        nums = []
        for c in cells[1:7]:
            digits = re.sub(r"[^0-9]", "", c)
            if not digits:
                break
            nums.append(int(digits))
        if len(nums) != 6:
            continue
        out[surface] = dict(zip(("leaves", "A", "B", "C", "D", "E"), nums))
    return out


def read_table() -> tuple[dict[tuple[str, str], tuple[str, str]], list[str]]:
    """`(surface, leaf) -> (class, evidence)`, plus the malformed lines."""
    rows: dict[tuple[str, str], tuple[str, str]] = {}
    bad: list[str] = []
    if not TABLE.exists():
        return rows, bad
    for n, line in enumerate(_read(TABLE).splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) != 4:
            bad.append(f"{TABLE.name}:{n}: expected 4 tab-separated fields, got {len(parts)}")
            continue
        surface, leaf, cls, evidence = (p.strip() for p in parts)
        if surface not in SURFACES:
            bad.append(f"{TABLE.name}:{n}: unknown surface {surface!r}")
            continue
        if cls not in CLASSES:
            bad.append(f"{TABLE.name}:{n}: class must be one of {'/'.join(CLASSES)}, got {cls!r}")
            continue
        key = (surface, leaf)
        if key in rows:
            bad.append(f"{TABLE.name}:{n}: duplicate row for {surface} {leaf!r}")
            continue
        rows[key] = (cls, evidence)
    return rows, bad


def evaluate() -> tuple[dict[str, dict], list[str]]:
    """Per-surface counts, and every failure found, each naming its leaf."""
    rows, failures = read_table()
    derived = {name: fn() for name, fn in SURFACES.items()}
    executed = cli_executed()
    per_surface: dict[str, dict] = {}

    for surface, leaves in derived.items():
        table_leaves = {leaf for (s, leaf) in rows if s == surface}

        for leaf in leaves:
            if leaf not in table_leaves:
                failures.append(
                    f"{surface}: {leaf!r} exists in the code and has no row in "
                    f"docs/testing/folhas.tsv — classify it"
                )
        for leaf in sorted(table_leaves - set(leaves)):
            failures.append(
                f"{surface}: {leaf!r} has a row in docs/testing/folhas.tsv and no "
                f"longer exists in the code — remove the row"
            )

        proved = 0
        by_class = dict.fromkeys(CLASSES, 0)
        for leaf in leaves:
            got = rows.get((surface, leaf))
            if got is None:
                continue  # already reported above
            cls, evidence = got
            by_class[cls] += 1

            # The CLI's class A is not a claim in the table: it is derived from
            # the committed trace, so the table cannot promote a leaf by itself.
            if surface == "cli":
                really_executed = leaf in executed
                if cls == "A" and not really_executed:
                    failures.append(
                        f"cli: {leaf!r} is class A but the trace "
                        f"(scripts/cli_exec_trace.tsv) does not record it executed"
                    )
                if cls != "A" and really_executed:
                    failures.append(
                        f"cli: {leaf!r} IS recorded executed in the trace but the "
                        f"table calls it class {cls} — promote it to A"
                    )

            if not evidence or evidence == "-":
                failures.append(
                    f"{surface}: {leaf!r} is class {cls} with no evidence — "
                    f"class A needs the check that proves it, B/C/D/E need the "
                    f"reason it is not proved"
                )
            elif cls == "A":
                proved += 1

        per_surface[surface] = {"proved": proved, "leaves": len(leaves)}
        per_surface[surface].update(by_class)

    # The published counts table has to agree with what was just measured.
    published = inventory_counts()
    if not published:
        failures.append(
            "docs/testing/folhas-inventario.md has no folhas:counts block — the "
            "published table cannot be checked against docs/testing/folhas.tsv"
        )
    else:
        for surface in SURFACES:
            said = published.get(surface)
            if said is None:
                failures.append(
                    f"{surface}: absent from the counts table in "
                    f"docs/testing/folhas-inventario.md"
                )
                continue
            measured = per_surface[surface]
            if said["leaves"] != measured["leaves"]:
                failures.append(
                    f"{surface}: the inventario table says {said['leaves']} leaves, "
                    f"folhas.tsv has {measured['leaves']}"
                )
            for cls in CLASSES:
                if said[cls] != measured[cls]:
                    failures.append(
                        f"{surface}: the inventario table says {said[cls]} of class "
                        f"{cls}, folhas.tsv has {measured[cls]}"
                    )

    return per_surface, failures


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--list", action="store_true", help="per-surface breakdown")
    ap.add_argument("--list-missing", action="store_true", help="leaves that are not class A")
    ap.add_argument("--skeleton", action="store_true", help="emit rows for leaves the table lacks")
    ap.add_argument("--update", action="store_true", help="adopt today's counts as the baseline")
    args = ap.parse_args()

    if args.skeleton:
        rows, _ = read_table()
        for surface, fn in SURFACES.items():
            for leaf in fn():
                if (surface, leaf) not in rows:
                    print(f"{surface}\t{leaf}\tB\t-")
        return 0

    per_surface, failures = evaluate()
    total_proved = sum(s["proved"] for s in per_surface.values())
    total_leaves = sum(s["leaves"] for s in per_surface.values())

    if args.list:
        for surface in SURFACES:
            s = per_surface[surface]
            pct = 100.0 * s["proved"] / s["leaves"] if s["leaves"] else 0.0
            print(f"{surface:10s} {s['proved']:4d} of {s['leaves']:4d}  {pct:5.1f} %")
        print(f"{'TOTAL':10s} {total_proved:4d} of {total_leaves:4d}")

    if args.list_missing:
        rows, _ = read_table()
        for surface, fn in SURFACES.items():
            for leaf in fn():
                cls, evidence = rows.get((surface, leaf), ("?", "-"))
                if cls != "A":
                    print(f"{surface}\t{leaf}\t{cls}\t{evidence}")

    if args.update:
        if failures:
            for f in failures:
                print(f"FAIL  {f}")
            print("refusing to record a baseline over a failing table", file=sys.stderr)
            return 1
        BASELINE.write_text(
            json.dumps({"schema": 1, "surfaces": per_surface}, indent=2) + "\n",
            encoding="utf-8",
        )
        print(f"recorded: {total_proved} of {total_leaves} leaves class A")
        return 0

    if not BASELINE.exists():
        failures.append(
            "no scripts/folhas_baseline.json — record one with "
            "scripts/folhas_gate.py --update"
        )
        for f in failures:
            print(f"FAIL  {f}")
        return 1

    base = json.loads(_read(BASELINE)).get("surfaces", {})
    for surface in SURFACES:
        now = per_surface[surface]
        was = base.get(surface)
        if was is None:
            failures.append(f"{surface}: absent from the baseline — re-record it")
            continue
        if now["proved"] < was["proved"]:
            failures.append(
                f"{surface}: {was['proved']} leaves were proved and now {now['proved']} "
                f"are — a regression, not a measurement"
            )
        elif now["proved"] > was["proved"]:
            failures.append(
                f"{surface}: {now['proved']} leaves are proved and the baseline says "
                f"{was['proved']} — raise it in this commit "
                f"(scripts/folhas_gate.py --update)"
            )
        if now["leaves"] != was["leaves"]:
            failures.append(
                f"{surface}: the surface had {was['leaves']} leaves and now has "
                f"{now['leaves']} — a leaf entered or left, decide and re-record"
            )

    if failures:
        printed: set[str] = set()
        for f in failures:
            if f not in printed:
                print(f"FAIL  {f}")
                printed.add(f)
        return 1

    print(f"ok    {total_proved} of {total_leaves} leaves proved in a real run")
    return 0


if __name__ == "__main__":
    sys.exit(main())
