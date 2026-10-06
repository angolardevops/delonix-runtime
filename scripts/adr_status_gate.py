#!/usr/bin/env python3
"""ADR status gate — an ADR may not contradict itself about its own state.

# What this closes

An ADR's `Status:` line is what a reader trusts to know whether the decision has
been built. `docs/adr/0067-storage-pools.md` said «Nothing implemented» from
2026-10-02 to 2026-10-06 while the SAME document carried an addendum titled «P0
built: the port, the allowlist, `kind: StoragePool` and the `dir` driver» — the
work was in `main` the whole time. And `0061-init-template-contract.md` reported
its state off a branch (`init/templates-master`) that no longer exists.

Both are the class this repo keeps paying for: a record that is not updated with
the work starts lying in BOTH directions — it hides what shipped and it promises
what did not. The `AUDITORIA-E2E.md` table did it with findings, the maturity
plan did it with a lost branch, and these did it with a status line.

# What it checks

1. **Self-contradiction.** A status line claiming nothing is implemented, in
   either language, in a document that also has an addendum saying something was
   built. Nothing here judges WHETHER an ADR should be Accepted — that is the
   owner's decision and no script can take it.
What it deliberately does NOT check: a status line that names a branch. ADR-0061
did exactly that and it was a real defect — but `0049`'s status names the API
routes `agent/exec` and `agent/exec-status`, which have a branch's shape, so the
check produced one true positive against two false ones. A detector with false
positives is noise with a number in front (the same measurement that kept `num`
in the language lexicon and threw `nas` out). The rule stands as a convention:
a status line states the STATE, never where to go and read it.

Run: `python3 scripts/adr_status_gate.py` (exit 1 on a contradiction).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ADR = ROOT / "docs" / "adr"

# «nothing implemented» in the two languages the ADRs are written in.
NOTHING = re.compile(r"\b(nothing implemented|nada implementado)\b", re.I)
# An addendum (or a status) that says something WAS built.
BUILT = re.compile(
    r"\b(built|implemented|implementado|construído|construido)\b", re.I
)
HEADING = re.compile(r"^#{2,3}\s+(addendum|adenda|anexo)\b", re.I)
STATUS = re.compile(r"^-\s+\*\*(status|estado):?\*\*:?\s*(.*)$", re.I)


def status_of(lines: list[str]) -> tuple[int, str]:
    """The status line's index and its text, continuation lines included."""
    for i, line in enumerate(lines):
        m = STATUS.match(line)
        if not m:
            continue
        text = m.group(2)
        for cont in lines[i + 1 :]:
            if cont.startswith("- ") or not cont.strip():
                break
            text += " " + cont.strip()
        return i, text
    return -1, ""



def main() -> int:
    problems: list[str] = []
    checked = 0
    for path in sorted(ADR.glob("0*.md")):
        lines = path.read_text(encoding="utf-8").splitlines()
        idx, status = status_of(lines)
        if idx < 0:
            continue
        checked += 1
        if NOTHING.search(status):
            for line in lines[idx + 1 :]:
                if HEADING.match(line) and BUILT.search(line):
                    problems.append(
                        f"{path.name}: the status says nothing is implemented, and "
                        f"the document says «{line.strip()[:90]}»"
                    )
                    break
    # A gate that read nothing is a green by absence — the exact failure this
    # repo has paid for twice (a CI step that skipped, a check that never ran).
    if checked == 0:
        print("FAIL  adr status: no ADR had a status line — the gate read nothing")
        return 1
    if problems:
        print(f"FAIL  adr status: {len(problems)} contradiction(s) in {checked} ADR(s)")
        for p in problems:
            print(f"  - {p}")
        return 1
    print(f"ok    adr status: {checked} ADR(s), none contradicts itself")
    return 0


if __name__ == "__main__":
    sys.exit(main())
