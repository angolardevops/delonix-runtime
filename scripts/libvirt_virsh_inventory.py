#!/usr/bin/env python3
"""libvirt (`virsh`) command inventory — the denominator behind any "the engine
covers libvirt" claim (ADR-0050, the libvirt counterpart of ADR-0049's Proxmox
matrix).

libvirt publishes no HTTP schema, but its client publishes the command list
of the version installed, grouped by object family:

    virsh help > virsh-help.txt          # of the libvirt version under test
    python3 scripts/libvirt_virsh_inventory.py virsh-help.txt

and the numerator is what the engine actually INVOKES, read from the Rust
sources that spawn `virsh` (never from a list kept next to them):

    crates/adapters/delonix-vm/src/lib.rs
    bins/delonix-runtime-bin/src/cmd/vm.rs

Each subcommand of the help lands in exactly one of three states:

  * called   — a source line builds a `virsh` argv naming it;
  * excluded — the engine does not call it ON PURPOSE, with the reason written
               in EXCLUDED below;
  * missing  — neither: real libvirt surface the engine does not reach.

An excluded command stays in the denominator. A called name the help does not
list fails the run (exit 1): that is a command this libvirt version does not
have, and the engine is about to spawn it.

**What this number is and is not.** `virsh` is the transport the engine uses
today, so this is a faithful count of the libvirt surface reached; it is NOT
the capability matrix. A capability (ADR-0050) is answered by
`delonix provider describe libvirt`; this script answers the narrower question
"of the 276 things virsh 10.0 can do, how many does the engine ever ask for".
Both numbers are published with the version they were measured against.

Detection is by CONTEXT, not by the bare literal: `"start"`, `"list"` or
`"create"` are ordinary words in a Rust file. A literal counts only when the
same line also carries the `"-c"` URI flag, or when it is the subcommand
argument of `virsh_define_xml(...)`, or when it is a `.into()` element inside a
function whose name says it builds a virsh argv (`*_argv`). Test modules
(`#[cfg(test)] mod … { … }`) are skipped. A line the reader cannot classify
counts as NOT called, so a reading failure never inflates the number.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter
from pathlib import Path

SOURCES = [
    Path("crates/adapters/delonix-vm/src/lib.rs"),
    Path("bins/delonix-runtime-bin/src/cmd/vm.rs"),
]

# Groups (by the `help keyword` virsh prints) the engine does not call by
# decision. A group not listed here is either called or MISSING — never hidden.
EXCLUDED_GROUPS: dict[str, str] = {
    "virsh": "the client's own shell (cd, pwd, echo, connect, help, quit) — not a libvirt operation",
    "interface": "host NIC administration (`iface-*`) — the host's, not a VM operation; the engine's VLAN verb is root-only and says so",
}

# Individual commands excluded with a reason, when the group as a whole is not.
EXCLUDED_COMMANDS: dict[str, str] = {
    "domxml-from-native": "conversion of foreign definitions — no consumer",
    "domxml-to-native": "conversion to a raw QEMU command line — no consumer",
    "cpu-baseline": "cluster-wide CPU baseline — a multi-host decision the engine does not make (ADR-0031)",
    "cpu-compare": "same",
    "hypervisor-cpu-baseline": "same",
    "hypervisor-cpu-compare": "same",
    "migrate": "live migration is a written no-go (ADR-0031); `vm migrate` is stop-copy-start over SSH, without virsh",
    "migrate-compcache": "same",
    "migrate-getmaxdowntime": "same",
    "migrate-getspeed": "same",
    "migrate-postcopy": "same",
    "migrate-setmaxdowntime": "same",
    "migrate-setspeed": "same",
    "migrate-getmaxdowntime": "same",
}

GENERIC_WORDS = {"start", "list", "create", "restore", "hostname", "uri", "console", "define", "destroy", "resume", "suspend", "event", "version", "help", "echo", "connect"}


def parse_help(text: str) -> tuple[dict[str, str], dict[str, str]]:
    """Returns ({command: group}, {group: title}) from `virsh help` output."""
    cmd_group: dict[str, str] = {}
    titles: dict[str, str] = {}
    group = None
    for line in text.splitlines():
        m = re.match(r"^ (.+?) \(help keyword '([a-z]+)'\):\s*$", line)
        if m:
            group = m.group(2)
            titles[group] = m.group(1)
            continue
        m = re.match(r"^    ([a-z][a-z0-9-]*)\s", line + " ")
        if m and group:
            cmd_group[m.group(1)] = group
    return cmd_group, titles


def strip_test_modules(text: str) -> str:
    """Blanks every `#[cfg(test)]` module so a test's argv never counts."""
    out: list[str] = []
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        line = lines[i]
        if line.strip() == "#[cfg(test)]" and i + 1 < len(lines) and re.match(r"^\s*mod\s+\w+\s*\{", lines[i + 1]):
            depth = 0
            j = i + 1
            while j < len(lines):
                depth += lines[j].count("{") - lines[j].count("}")
                out.append("")
                j += 1
                if depth <= 0:
                    break
            out.append("")
            i = j
            continue
        out.append(line)
        i += 1
    return "\n".join(out)


def called_commands(text: str, known: set[str]) -> dict[str, list[int]]:
    """{command: [line numbers]} for every virsh subcommand the source invokes."""
    text = strip_test_modules(text)
    found: dict[str, list[int]] = {}
    current_fn = ""
    lines = text.splitlines()
    # A virsh argv often spans lines (`vec![\n "-c",\n uri,\n "net-update", …]`,
    # or `virsh_define_xml(\n uri,\n "nwfilter-define", …)`). Join such a block
    # onto its first line so the context rules below see the whole argv.
    joined: list[tuple[int, str]] = []
    i = 0
    while i < len(lines):
        line = lines[i]
        opens = ('"-c",' in line and "]" not in line) or (
            "virsh_define_xml(" in line and ")" not in line
        )
        if opens:
            block = [line]
            j = i + 1
            while j < len(lines) and not re.search(r"[\])]", lines[j]):
                block.append(lines[j])
                j += 1
            if j < len(lines):
                block.append(lines[j])
            joined.append((i + 1, " ".join(b.strip() for b in block)))
            i = j + 1
            continue
        joined.append((i + 1, line))
        i += 1
    for no, line in joined:
        m = re.search(r"\bfn\s+(\w+)\s*\(", line)
        if m:
            current_fn = m.group(1)
        literals = re.findall(r'"([a-z][a-z0-9-]*)"', line)
        for lit in literals:
            if lit not in known:
                continue
            ok = False
            if '"-c"' in line:
                ok = True
            elif re.search(r'virsh_define_xml\([^)]*"' + re.escape(lit) + '"', line):
                ok = True
            elif current_fn.endswith("_argv") and (f'"{lit}".into()' in line or f'"{lit}",' in line):
                ok = True
            elif lit not in GENERIC_WORDS and ("virsh" in line or "virsh" in current_fn):
                ok = True
            if ok:
                found.setdefault(lit, []).append(no)
    return found


def inventory(help_text: str, sources: dict[str, str]) -> dict:
    cmd_group, titles = parse_help(help_text)
    known = set(cmd_group)
    called: dict[str, list[str]] = {}
    unknown: list[str] = []
    for path, text in sources.items():
        for cmd, lines in called_commands(text, known | set()).items():
            called.setdefault(cmd, []).extend(f"{path}:{n}" for n in lines)
    rows = []
    for cmd, group in cmd_group.items():
        if cmd in called:
            state, reason = "called", ""
        elif group in EXCLUDED_GROUPS:
            state, reason = "excluded", EXCLUDED_GROUPS[group]
        elif cmd in EXCLUDED_COMMANDS:
            state, reason = "excluded", EXCLUDED_COMMANDS[cmd]
        else:
            state, reason = "missing", ""
        rows.append({"command": cmd, "group": group, "state": state, "reason": reason, "sites": called.get(cmd, [])})
    for cmd in called:
        if cmd not in cmd_group:
            unknown.append(cmd)
    by_group: dict[str, Counter] = {}
    for r in rows:
        by_group.setdefault(r["group"], Counter())[r["state"]] += 1
    totals = Counter(r["state"] for r in rows)
    return {
        "denominator": len(rows),
        "totals": dict(totals),
        "groups": {g: {"title": titles.get(g, g), **dict(c), "total": sum(c.values())} for g, c in by_group.items()},
        "rows": rows,
        "unknown_called": unknown,
    }


def render(inv: dict) -> str:
    t = inv["totals"]
    n = inv["denominator"]
    out = [
        f"virsh subcommands: {n}",
        f"  called    {t.get('called', 0):4d}  {100.0 * t.get('called', 0) / n:5.1f}% of the help",
        f"  excluded  {t.get('excluded', 0):4d}  with a written reason",
        f"  missing   {t.get('missing', 0):4d}",
        "",
        f"{'group':<12} {'called':>6} {'excluded':>8} {'missing':>7} {'total':>5}",
    ]
    for g, c in sorted(inv["groups"].items(), key=lambda kv: -kv[1]["total"]):
        out.append(f"{g:<12} {c.get('called', 0):>6} {c.get('excluded', 0):>8} {c.get('missing', 0):>7} {c['total']:>5}")
    out.append("")
    out.append("called: " + ", ".join(sorted(r["command"] for r in inv["rows"] if r["state"] == "called")))
    if inv["unknown_called"]:
        out.append("")
        out.append("CALLED BUT NOT IN THIS VIRSH: " + ", ".join(inv["unknown_called"]))
    return "\n".join(out)


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("help_file", help="output of `virsh help` for the libvirt version under test")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--root", default=".", help="repository root (default: cwd)")
    args = ap.parse_args(argv)
    root = Path(args.root)
    sources = {}
    for p in SOURCES:
        f = root / p
        if not f.is_file():
            print(f"missing source {f}", file=sys.stderr)
            return 2
        sources[str(p)] = f.read_text()
    inv = inventory(Path(args.help_file).read_text(), sources)
    print(json.dumps(inv, indent=2) if args.json else render(inv))
    return 1 if inv["unknown_called"] else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
