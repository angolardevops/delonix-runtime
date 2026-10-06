#!/usr/bin/env python3
"""Capability cell-metric ratchet (`docs/discovery/65_PLANO_MATURIDADE.md`, item F0.1).

A **cell** is one (capability × provider) pair. The metric is how many of them
are at **N2** — `supported`, with evidence the evidence gate confirms exists —
over the cells that provider could fill at all. The number is computed by
`delonix provider matrix` from the catalog in CODE and written at the top of
`docs/providers/capability-matrix.md`.

Like `lang_ratchet.py` this is a ratchet in BOTH directions, for a different
reason in each:

* it **drops** → a regression: a cell stopped being proved and nothing said so.
  Fail.
* it **rises** without the baseline rising in the same commit → the gain was
  never recorded, and the next commit could undo it unnoticed. Fail, naming the
  command that records it.

**Why it reads the FILE instead of running the binary**: the file is generated,
and a test (`cmd::provider::tests::the_published_matrix_is_the_generated_one`)
already fails when it differs from the binary's output. With that guarantee,
reading one line is cheap and this gate does not need a `cargo build` in front
of it. Without it, this gate would be measuring a document that might be stale
— which is the very defect it replaces.

**The defect it replaces**, measured 2026-10-06: the baseline of plan 66 read
«111 supported, 96 partial, 73 not-implemented, 1 unavailable-on-host = 281
applicable, 39.5 %», and not one of those is a count of cells. They came from a
`grep -c` over the markdown, which also counted the file's own eight
per-provider summary lines, the intro prose, and — for `supported` — the word
inside `unsupported-by-provider` (339 - 228 = 111). The other three match the
grep exactly: 96, 73 and 1 (the declared view has no `unavailable-on-host` at
all; that 1 is the word in the introduction). Counted from the catalog: **100 of
252 = 39.7 %**. A number measured by grepping a document is not a metric — it is
the document talking about itself.

    python3 scripts/capability_ratchet.py            # check (exit 1 when out of line)
    python3 scripts/capability_ratchet.py --list     # the metric per domain
    python3 scripts/capability_ratchet.py --update   # record the current metric
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MATRIX = ROOT / "docs" / "providers" / "capability-matrix.md"
BASELINE = Path(__file__).resolve().parent / "capability_baseline.json"

# The line `metric_section` writes. Its shape is fixed on purpose: a gate that
# had to understand the TABLE would break on a cell holding an escaped `|`, and
# this file has several.
METRIC = re.compile(
    r"^\*\*Cell metric \(plan 65, level N2\): (\d+) of (\d+) applicable cells are proved"
)
ROW = re.compile(r"^\| ([a-z-]+) \| (\d+) \| (\d+) \| ([\d.]+) \|$")


def read_matrix() -> tuple[int, int, list[tuple[str, int, int]]]:
    """(proved, applicable, per domain), read from the published file."""
    if not MATRIX.exists():
        sys.exit(f"FAIL  {MATRIX.relative_to(ROOT)} does not exist")
    proved = applicable = None
    domains: list[tuple[str, int, int]] = []
    for line in MATRIX.read_text(encoding="utf-8").splitlines():
        m = METRIC.match(line)
        if m:
            if proved is not None:
                sys.exit("FAIL  the matrix carries more than one metric line")
            proved, applicable = int(m.group(1)), int(m.group(2))
            continue
        r = ROW.match(line)
        if r:
            domains.append((r.group(1), int(r.group(2)), int(r.group(3))))
    if proved is None:
        sys.exit(
            "FAIL  the published matrix has no metric line — regenerate it with\n"
            "      `delonix provider matrix > docs/providers/capability-matrix.md`"
        )
    # A gate that measures nothing reads as green.
    if applicable == 0:
        sys.exit("FAIL  the matrix says 0 applicable cells — this gate would guard nothing")
    return proved, applicable, domains


def percent(proved: int, applicable: int) -> str:
    """The engine's own arithmetic: integer tenths, rounded half up."""
    t = (proved * 1000 + applicable // 2) // applicable
    return f"{t // 10}.{t % 10}"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--list", action="store_true", help="print the metric per domain")
    ap.add_argument("--update", action="store_true", help="record the current metric")
    args = ap.parse_args()

    proved, applicable, domains = read_matrix()

    if args.list:
        print(f"{'domain':<20} {'N2':>4} {'applic.':>8}  %")
        for name, p, a in domains:
            print(f"{name:<20} {p:>4} {a:>8}  {percent(p, a)}")
        print(f"{'TOTAL':<20} {proved:>4} {applicable:>8}  {percent(proved, applicable)}")
        return 0

    if args.update:
        BASELINE.write_text(
            json.dumps({"proved": proved, "applicable": applicable}, indent=2) + "\n",
            encoding="utf-8",
        )
        print(f"baseline recorded: {proved}/{applicable} = {percent(proved, applicable)} %")
        return 0

    if not BASELINE.exists():
        sys.exit(f"FAIL  {BASELINE.relative_to(ROOT)} is missing — run with `--update`")
    base = json.loads(BASELINE.read_text(encoding="utf-8"))
    b_proved, b_applicable = base["proved"], base["applicable"]

    bad = []
    if proved < b_proved:
        bad.append(
            f"proved cells DROPPED: {b_proved} -> {proved}. A cell stopped being proved — "
            f"restore the evidence, or lower the baseline in the same commit with the reason"
        )
    elif proved > b_proved:
        bad.append(
            f"proved cells ROSE: {b_proved} -> {proved}, and the baseline did not. "
            f"Record the gain in the same commit: `python3 scripts/capability_ratchet.py --update`"
        )
    if applicable != b_applicable:
        bad.append(
            f"applicable cells changed: {b_applicable} -> {applicable}. The catalog grew or "
            f"shrank — record it in the same commit: "
            f"`python3 scripts/capability_ratchet.py --update`"
        )

    for b in bad:
        print(f"FAIL  {b}")
    if bad:
        return 1
    print(
        f"ok    cell metric: {proved} of {applicable} applicable "
        f"= {percent(proved, applicable)} % (N2)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
