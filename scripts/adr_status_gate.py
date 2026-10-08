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

   A bullet's status is the last ITALIC span that names a state, not any status
   word in its prose: `0072`'s entry says «rejected for the Kind move» before its
   `*Accepted*`, so reading the whole text would have filed it as rejected. And
   the span must not swallow `**bold**` — `**0074**` sits at the start of every
   bullet, and naive pairing of asterisks hid that entry's own `*Accepted*`.

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


def bullet_status(text: str) -> str | None:
    """The state a bullet declares: the LAST italic span that names one."""
    named = [s for s in ITALIC.findall(text) if word_of(s)]
    return word_of(named[-1]) if named else None


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
        by_number = {name[:4]: name for name in own}
        for num, text in bullet_entries(readme.read_text(encoding="utf-8").splitlines()).items():
            name = by_number.get(num)
            if name is None:
                continue
            w = bullet_status(text)
            if w is None:
                problems.append(
                    f"README.md: the bullet for {num} names no state — the ADR says "
                    f"«{own[name]}» and a reader who stops at the index learns nothing"
                )
                continue
            rows += 1
            if w != own[name]:
                problems.append(
                    f"README.md: the bullet for {num} says «{w}» and the ADR says "
                    f"«{own[name]}» — the index is lying to whoever stops there"
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
