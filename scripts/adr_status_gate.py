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
2. **The index and the document agree, in BOTH shapes the index uses.**
   `docs/adr/README.md` carries a status per entry; when it and the ADR disagree,
   one of them is lying to every reader who stops at the index. Measured
   2026-10-06: SEVEN rows disagreed, and two of them (`0044`, `0050`) had been
   wrong for twelve days — accepted in the document, `Proposed` in the index.

   That check read only the TABLE, and the index has two shapes. Measured
   2026-10-08: the table stops at `0062`, while a bullet list carries `0061` and
   `0063` onwards — so the guarantee covered none of the thirteen most recent
   ADRs, and four of them were lying (`0061`, `0063`, `0064`, `0067`: `Proposed`
   in the index, Accepted in the document). A guard that does not cover the case
   it exists for is the shape this repo keeps paying for; it was found because an
   edit to a bullet FAILED and the gate stayed green over the disagreement.

   Since 2026-10-08 the index has ONE shape: a table row per ADR. The bullet list
   was migrated into it and a bullet-shaped entry is now refused, because two
   shapes meant two places to update and four ADRs (`0001`-`0004`) were in both.
   A status cell BEGINS with the state, which is what makes reading the first
   match correct: the prose that follows says «rejected for the Kind move»
   (`0072`) and «rejected on measurement» (`0075`) long before it is done.

3. **Every ADR has a status, and an index row.** Both are absences the earlier
   gate could not see, because it only judged what it found. Measured 2026-10-08:
   `0011`, `0045` and `0051` had no index entry at all, so nothing compared them
   to anything.

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
WORD = re.compile(r"(Accepted|Proposed|Rejected|Superseded|Aceite|Proposto|Recusado)", re.I)
ROW = re.compile(r"^\|\s*\[(\d{4})\]\(([^)]+)\)\s*\|.*\|\s*(.*)\|\s*$")
# The index's other shape: `- **NNNN** — …text… *Accepted*.` The number is the
# key here, because a bullet carries no link to the file the way a row does.
BULLET = re.compile(r"^-\s+\*\*(\d{4})\*\*")
# An italic span, and NOT a bold one: the lookarounds are what keep `**0074**`
# from being read as the opening of an italic and shifting every pair after it.
ITALIC = re.compile(r"(?<!\*)\*([^*]+)\*(?!\*)")
SAME = {"aceite": "accepted", "proposto": "proposed", "recusado": "rejected"}


def word_of(text: str) -> str | None:
    """The status word, normalised to English.

    The map above exists because the pt-AO review copies of some ADRs write the
    status in Portuguese while the index is in English; without it, an agreeing
    pair would read as a disagreement. Naming those words in a `#` comment is
    what the language ratchet counts as new Portuguese debt (measured: it took
    the count from 3257 to 3258 and the push was refused), so they are named
    here instead — the repo's own remedy for a reference a comment cannot carry.
    """
    m = WORD.search(text)
    if not m:
        return None
    w = m.group(1).lower()
    return SAME.get(w, w)
# The status line, in every shape the ADRs actually use. Measured 2026-10-08:
# requiring a LIST ITEM (`- **Status:**`) read 60 of 76 documents and skipped
# SIXTEEN — and `main()` skips a document with no status silently, so for those
# sixteen the gate checked neither self-contradiction nor the index. A gate that
# reads nothing is a green by absence, which this file's own docstring calls the
# failure this repo has paid for twice; it was doing it to a fifth of the ADRs.
#
# The three shapes it was missing, all of them carrying a real status:
#   `**Status: Accepted (2026-08-12).**`  — bold with the colon INSIDE (0013, 0022)
#   `**Status:** Accepted 2026-09-15 — …` — a paragraph, not a list item (0038)
#   `## Status` + the prose below it       — a heading (0048)
STATUS = re.compile(r"^-?\s*\*\*(status|estado):?\*\*:?\s*(.*)$", re.I)
STATUS_INNER = re.compile(r"^\*\*(status|estado):\s*(.*)$", re.I)
STATUS_HEAD = re.compile(r"^#{2,3}\s+(status|estado)\s*$", re.I)


def status_of(lines: list[str]) -> tuple[int, str]:
    """The status line's index and its text, continuation lines included."""
    for i, line in enumerate(lines):
        text = None
        m = STATUS.match(line)
        if m:
            text = m.group(2)
        else:
            m = STATUS_INNER.match(line)
            if m:
                # `**Status: Accepted (…).** the rest` — the closing `**` sits
                # mid-line, so the status text is the whole remainder.
                text = m.group(2)
            elif STATUS_HEAD.match(line):
                # A heading: the status is the paragraph under it.
                body = []
                for cont in lines[i + 1 :]:
                    if not cont.strip():
                        if body:
                            break
                        continue
                    if cont.startswith("#"):
                        break
                    body.append(cont.strip())
                text = " ".join(body)
        if text is None:
            continue
        if not STATUS_HEAD.match(line):
            for cont in lines[i + 1 :]:
                if cont.startswith("- ") or cont.startswith("#") or not cont.strip():
                    break
                text += " " + cont.strip()
        return i, text
    return -1, ""



def bullet_entries(lines: list[str]) -> dict[str, str]:
    """Each bullet of the index, number -> its text with continuations joined.

    Bullets wrap over several lines, and a single-line read is what made this
    check miss them: measured 2026-10-08, ten of the eighteen entries carried
    their status on a continuation line.
    """
    out: dict[str, str] = {}
    i = 0
    while i < len(lines):
        m = BULLET.match(lines[i])
        if not m:
            i += 1
            continue
        text = lines[i]
        j = i + 1
        while j < len(lines) and lines[j].strip() and not BULLET.match(lines[j]) and not lines[j].startswith("#"):
            text += " " + lines[j].strip()
            j += 1
        out[m.group(1)] = text
        i = j
    return out


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
    # The index may not disagree with the document.
    own: dict[str, str] = {}
    for path in sorted(ADR.glob("0*.md")):
        if ".pt-AO" in path.name:
            continue
        _, status = status_of(path.read_text(encoding="utf-8").splitlines())
        w = word_of(status)
        if w:
            own[path.name] = w
    rows = 0
    readme = ADR / "README.md"
    if readme.exists():
        for line in readme.read_text(encoding="utf-8").splitlines():
            m = ROW.match(line)
            if not m:
                continue
            w = word_of(m.group(3))
            if w is None or m.group(2) not in own:
                continue
            rows += 1
            if w != own[m.group(2)]:
                problems.append(
                    f"README.md: {m.group(2)} says «{w}» and the ADR says "
                    f"«{own[m.group(2)]}» — the index is lying to whoever stops there"
                )
        # One shape for the index (2026-10-08). The reader stays as the
        # REFUSAL, not as dead code: a bullet added tomorrow is caught here
        # instead of silently escaping the table check.
        for num in bullet_entries(readme.read_text(encoding="utf-8").splitlines()):
            problems.append(
                f"README.md: {num} is indexed as a bullet — the index is a table, "
                "one row per ADR (migrated 2026-10-08; two shapes meant two places to update)"
            )
        # An ADR with no row is compared to nothing, which reads as agreement.
        indexed = {
            m.group(2)
            for m in (ROW.match(l) for l in readme.read_text(encoding="utf-8").splitlines())
            if m
        }
        for name in sorted(own):
            if name not in indexed:
                problems.append(
                    f"README.md: {name} has no index row — a reader who stops at the "
                    f"index never learns it says «{own[name]}»"
                )

    # A status nobody can read is the same absence, one file further in.
    for path in sorted(ADR.glob("0*.md")):
        _, status = status_of(path.read_text(encoding="utf-8").splitlines())
        if not word_of(status):
            problems.append(
                f"{path.name}: no status line names a state — a reader cannot tell "
                "whether the decision is taken (see STATUS for the shapes accepted)"
            )

    if problems:
        print(f"FAIL  adr status: {len(problems)} contradiction(s) in {checked} ADR(s)")
        for p in problems:
            print(f"  - {p}")
        return 1
    print(
        f"ok    adr status: {checked} ADR(s) and {rows} index row(s), none contradicts itself"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
