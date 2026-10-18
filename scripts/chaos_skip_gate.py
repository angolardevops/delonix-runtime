#!/usr/bin/env python3
"""The chaos workflow's SKIP gate (gap G-003).

`chaos.yml` gates its harness on a probe: `unshare -r -n true`. On a hosted
runner that fails, the `chaos harness` job is skipped, and GitHub renders a
skipped job apart from `success` — but it does not fail the workflow and does
not block a merge. So a run that measured NOTHING reads as a green PR. Both
PRs merged on 2026-10-18 (#872, #877) show exactly that: 18 gates green with
the harness `skipping`.

The fix is not "fail whenever the harness does not run": on this project's
hosted runners it never can, so that would paint every PR red every week, and
the workflow's own comment rejects that ("a blocked runner is not the engine's
fault"). What must not exist is an UNATTRIBUTED skip. So this gate asks one
question — *does something declare why the harness did not run?* — and fails
when the answer is no:

* the harness ran: nothing to judge, the job's own result stands;
* it was skipped and the probe's verdict is declared in
  `scripts/chaos_allowed_skips.txt`: the gate passes and PRINTS the declared
  reason, so the run says out loud that nothing was measured;
* it was skipped for anything else — a verdict nobody declared, or a probe
  that said the environment was fine and a skip happened anyway: the gate
  FAILS.

That last case is the one with teeth. A step-level `if` that quietly stops
exercising the harness, or a `needs` that disables it while the environment
could run it, now turns red instead of green.

    scripts/chaos_skip_gate.py --probe blocked --harness skipped
    scripts/chaos_skip_gate.py --print          # the declared verdicts
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

ALLOWED = Path(__file__).with_name("chaos_allowed_skips.txt")

# Results that mean the harness itself decided the run: the gate does not
# second-guess them. A `failure` already fails the workflow, and `cancelled`
# is either the operator or `concurrency: cancel-in-progress` superseding a
# run — making that red would be a false red, the defect this gate exists to
# avoid creating.
RAN_OR_SPOKE = ("success", "failure", "cancelled")


def declared(path: Path = ALLOWED) -> dict[str, str]:
    """The declared verdicts, `verdict -> reason`."""
    out: dict[str, str] = {}
    if not path.exists():
        return out
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        verdict, _, reason = line.partition("\t")
        out[verdict.strip()] = reason.strip()
    return out


def verdict(probe: str, harness: str, allowed: dict[str, str]) -> list[str]:
    """The problems found, empty when the run is acceptable."""
    harness = (harness or "").strip()
    probe = (probe or "").strip()
    if harness in RAN_OR_SPOKE:
        return []
    if harness != "skipped":
        return [
            f"the chaos harness reported an unknown result ({harness or 'empty'}), "
            "so whether anything was measured cannot be told"
        ]
    if probe in allowed:
        return []
    if not probe:
        return [
            "the chaos harness was skipped and the environment probe said nothing, "
            "so nothing attributes the skip"
        ]
    return [
        f"the chaos harness was skipped and the probe verdict `{probe}` is not declared "
        f"in {ALLOWED.name}: nothing was measured and nothing says why"
    ]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--probe", default="", help="the environment probe's verdict")
    ap.add_argument("--harness", default="", help="the chaos job's result")
    ap.add_argument("--print", action="store_true", help="print the declared verdicts")
    args = ap.parse_args()

    allowed = declared()
    if args.print:
        for v, reason in sorted(allowed.items()):
            print(f"{v}\t{reason}")
        return 0

    problems = verdict(args.probe, args.harness, allowed)
    if problems:
        for p in problems:
            print(f"FAIL  {p}")
        print(
            "      Declare the verdict in scripts/chaos_allowed_skips.txt if the skip is "
            "legitimate, or restore whatever stopped the harness from running."
        )
        return 1

    if args.harness == "skipped":
        print(f"ok    the harness did not run, and the reason is declared: {allowed[args.probe]}")
        print("      NOTHING was measured in this run. This is not a chaos result.")
    else:
        print(f"ok    the harness reported `{args.harness}`; its own result stands")
    return 0


if __name__ == "__main__":
    sys.exit(main())
