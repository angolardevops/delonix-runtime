#!/usr/bin/env python3
"""The nightly lab's SKIP gate (decision D1 of docs/discovery/65_PLANO_MATURIDADE.md).

`scripts/e2e.sh` exits 0 on a SKIP, on purpose: a measurement that could not be
taken is not a negative result. On a developer's machine that is right. On the
lab it is the whole point of the lab that the measurement CAN be taken, so a
SKIP there is either a missing host piece (the preflight should have said) or a
section that silently stopped exercising anything.

The gate compares the SKIPs of a run (`$OUT/results.jsonl`) with the declared
list `scripts/lab_allowed_skips.txt`, one `name<TAB>reason` per line, and fails
in both directions, the way `lang_ratchet.py` does:

* a SKIP that is not declared fails: something stopped running;
* a declared SKIP that ran fails too: the line must go in the same commit,
  or the list becomes permission for the next regression to hide behind.

    scripts/lab_skip_gate.py /tmp/delonix-e2e/results.jsonl
    scripts/lab_skip_gate.py --print results.jsonl   # the run's SKIPs, in the list format
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

ALLOWED = Path(__file__).with_name("lab_allowed_skips.txt")


def skips(results: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    for line in results.read_text().splitlines():
        if not line.strip():
            continue
        rec = json.loads(line)
        if rec.get("verdict") == "SKIP":
            out[rec["name"]] = rec.get("reason", "")
    return out


def allowed(path: Path = ALLOWED) -> dict[str, str]:
    out: dict[str, str] = {}
    if not path.exists():
        return out
    for n, raw in enumerate(path.read_text().splitlines(), 1):
        if not raw.strip() or raw.startswith("#"):
            continue
        name, sep, reason = raw.partition("\t")
        if not sep or not reason.strip():
            raise SystemExit(f"{path.name}:{n}: a declared SKIP needs `name<TAB>reason`")
        out[name] = reason
    return out


def verdict(run: dict[str, str], declared: dict[str, str]) -> list[str]:
    problems = []
    for name in sorted(set(run) - set(declared)):
        problems.append(f"new SKIP: {name} — {run[name]}")
    for name in sorted(set(declared) - set(run)):
        problems.append(f"declared SKIP ran: {name} — remove it from {ALLOWED.name}")
    return problems


def main(argv: list[str]) -> int:
    if len(argv) == 3 and argv[1] == "--print":
        for name, reason in sorted(skips(Path(argv[2])).items()):
            print(f"{name}\t{reason}")
        return 0
    if len(argv) != 2:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        return 2
    results = Path(argv[1])
    if not results.exists():
        print(f"lab skip gate: {results} does not exist — the battery did not run", file=sys.stderr)
        return 1
    run = skips(results)
    problems = verdict(run, allowed())
    for p in problems:
        print(p)
    if problems:
        return 1
    print(f"lab skip gate: ok — {len(run)} SKIP(s), all declared")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
