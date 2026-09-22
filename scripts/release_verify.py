#!/usr/bin/env python3
"""Release verify — refuses a release whose maturity table cites rotted evidence.

# What this closes (M13 of docs/roadmap/13-improvements-traceability.md)

`delonix system features` publishes a maturity level per capability with
EVIDENCE — a sentence a reader can go and check (a script, a doc, a test file).
`cmd/features.rs` already gates that the evidence is non-empty and not a weasel
word ("works well"); nothing ever gated that the evidence stays TRUE. A file it
names can be renamed or deleted later, and the row keeps claiming `stable`/
`production-ready` with nobody noticing until a reader follows the reference
and it is gone — the same rot `AUDITORIA-E2E.md` paid for once already (see
this repo's own rule: "a table that does not track the code lies in both
directions").

# What it does

Reads `system features -o json` from the release binary — the SAME source
`delonix system features` prints, never a second copy of the table — and, for
every capability at `stable` or above, extracts every file-shaped token in its
`evidence` string (a path this repo would actually contain: `scripts/*.py`,
`scripts/*.sh`, `docs/**/*.md`, `AGENTS.md`, `tests/**`, a crate's own
`tests/*.rs`) and confirms it exists in the working tree. A rotted reference
fails the gate BEFORE a tag is cut, not after a reader notices.

Capabilities below `stable` (`experimental`/`preview`) are exempt on purpose —
their whole point is that the contract, and so the evidence, may still be
moving; holding them to the same bar as `stable` would just teach people to
write vaguer evidence to dodge the gate.

# `--report`

Writes a conformance report (Markdown) instead of checking — the same table,
timestamped and tied to the git commit, meant to travel alongside a release's
SBOM/provenance (see `scripts/sbom.py`, which states the same "why not a
generic tool" reasoning this script follows: the source of truth already
exists in the binary, so this only reads and renders it).

Usage:
  scripts/release_verify.py                       # check (exit 1 on rot)
  scripts/release_verify.py --report > FILE.md     # render the report
  scripts/release_verify.py --bin PATH             # override the binary
"""
import argparse
import json
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# A capability below this level is exempt from the evidence-rot check — see
# the module doc for why. `stable` and up is exactly the set `cli-stability.md`
# already promises not to break without a major, so its evidence is the one
# that has to keep pointing somewhere real.
GATED_LEVELS = {"stable", "production-ready", "certified"}

# File-shaped tokens worth checking: this repo's own conventions for citing a
# piece of itself as evidence. Deliberately NOT a bare `\S+/\S+` — that would
# also match prose like "8 chaos scenarios" or a crate path mentioned without
# meaning "go open this exact file", producing false rot reports nobody could
# trust. Each pattern names a real, checkable class of reference.
PATH_PATTERNS = [
    re.compile(r"\bscripts/[\w.\-/]+\.(?:py|sh)\b"),
    re.compile(r"\bdocs/[\w.\-/]+\.md\b"),
    re.compile(r"\bAGENTS\.md\b"),
    re.compile(r"\btests/[\w.\-/]+\.(?:sh|py)\b"),
    re.compile(r"\bcrates/[\w.\-/]+/tests/[\w.\-/]+\.rs\b"),
    re.compile(r"\bdocs/schema/v1/delonix\.json\b"),
]


def binary() -> Path:
    for c in (ROOT / "target/release/delonix", ROOT / "target/debug/delonix"):
        if c.is_file():
            return c
    sys.exit("sem binário: corre `cargo build --release -p delonix-runtime-bin`")


def features(bin_path: Path) -> list[dict]:
    out = subprocess.run(
        [str(bin_path), "system", "features", "-o", "json"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    return json.loads(out)


def evidence_paths(evidence: str) -> list[str]:
    found: list[str] = []
    for pat in PATH_PATTERNS:
        found.extend(pat.findall(evidence))
    return found


def check(bin_path: Path) -> int:
    rot: list[str] = []
    checked = 0
    for f in features(bin_path):
        if f["level"] not in GATED_LEVELS:
            continue
        for p in evidence_paths(f["evidence"]):
            checked += 1
            if not (ROOT / p).exists():
                rot.append(f"{f['name']!r} ({f['level']}): cites {p!r}, which no longer exists")
    if rot:
        print(f"FALHA: {len(rot)} referência(s) de evidência apodrecida(s):", file=sys.stderr)
        for line in rot:
            print(f"  {line}", file=sys.stderr)
        return 1
    print(f"ok: {checked} referência(s) de evidência, todas presentes na árvore")
    return 0


def report(bin_path: Path) -> str:
    commit = subprocess.run(
        ["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    # `--version` prints a multi-line banner (get-started tips included); only
    # the first line ("delonix X.Y.Z") belongs in a one-line report header.
    version = (
        subprocess.run([str(bin_path), "--version"], capture_output=True, text=True, check=True)
        .stdout.strip()
        .splitlines()[0]
    )
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    lines = [
        "# Conformance report",
        "",
        f"Generated {now} from `{version}` (commit `{commit}`) by `scripts/release_verify.py`.",
        "This is the same table `delonix system features` prints — never a second copy.",
        "",
        "| Level | Capability | Evidence |",
        "|---|---|---|",
    ]
    for f in features(bin_path):
        lines.append(f"| {f['level']} | {f['name']} | {f['evidence']} |")
    lines.append("")
    lines.append(
        "Nothing here is `certified`: that level means a validated matrix of kernels, "
        "distros and providers, and this engine is measured on one host. Claiming it "
        "would be the failure this report exists to prevent."
    )
    return "\n".join(lines) + "\n"


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--report", action="store_true", help="write the conformance report instead of checking")
    p.add_argument("--bin", type=Path, default=None, help="override the binary path")
    args = p.parse_args()
    bin_path = args.bin or binary()
    if args.report:
        sys.stdout.write(report(bin_path))
        return 0
    return check(bin_path)


if __name__ == "__main__":
    sys.exit(main())
