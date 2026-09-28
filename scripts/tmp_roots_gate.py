#!/usr/bin/env python3
"""What the test suite leaves in the temp dir — a ratchet, both ways.

`network_alloc_race.rs` created `$TMPDIR/delonix-net-race-<pid>` once per test
process and never removed it: ~2400 runs left ~2400 directories (#567). Its
sibling `netdef_naming.rs` had been given a sweep for the SAME leak a day after
both were written (2026-08-15), and the fix never reached the file next door.
Nothing failed in either case, because nothing looked at the temp dir after the
tests. This does.

CI runs `cargo test --workspace` with `TMPDIR` pointing at an EMPTY directory,
so everything in it afterwards was left by the tests. Names are normalized
(every run of digits becomes `N`: pids, thread ids, timestamps), so the same
leak has the same name on every run, and compared with
`scripts/tmp_roots_baseline.json`:

- a name that is not in the baseline FAILS: a test started leaking (or a fixed
  one, like `delonix-net-race-N`, came back);
- a name that leaves MORE entries than the baseline allows FAILS;
- a name in the baseline that no longer appears, or leaves fewer, FAILS too:
  someone fixed a leak and must lower the baseline in the same commit
  (`--update`). Counted with `<=`, the debt would read green forever.

The baseline is what the HOSTED runner leaves, and a test's leak can depend on
the host: `uma_imagem_em_uso_por_uma_vm_e_detectada_pelo_disco` returns early
when `qemu-img` is missing and skips its cleanup, so the runner leaves one entry
a workstation with `qemu-img` does not (measured 2026-09-28: 30 on the runner,
29 locally, the other 29 identical). Compare a local run with `--list`, not
against the baseline.

    python3 scripts/tmp_roots_gate.py --dir "$TMPDIR"            # judge
    python3 scripts/tmp_roots_gate.py --dir "$TMPDIR" --list     # show only
    python3 scripts/tmp_roots_gate.py --dir "$TMPDIR" --update   # lower it
"""
import argparse
import json
import re
import sys
from collections import Counter
from pathlib import Path

BASELINE = Path(__file__).resolve().parent / "tmp_roots_baseline.json"


def normalize(name: str) -> str:
    return re.sub(r"\d+", "N", name)


def census(entries) -> Counter:
    return Counter(normalize(e) for e in entries)


def judge(found: Counter, allowed: Counter) -> list:
    """Every line is one reason to fail; an empty list passes."""
    problems = []
    for name in sorted(set(found) | set(allowed)):
        f, a = found.get(name, 0), allowed.get(name, 0)
        if a == 0:
            problems.append(f"new leak: {name} ({f} left) — a test no longer cleans up after itself")
        elif f > a:
            problems.append(f"more of a known leak: {name} ({f} left, baseline {a})")
        elif f < a:
            problems.append(
                f"fixed or reduced: {name} ({f} left, baseline {a}) — lower the baseline "
                "with --update in the same commit"
            )
    return problems


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--dir", required=True, help="the TMPDIR the tests ran with")
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--list", action="store_true")
    mode.add_argument("--update", action="store_true")
    ap.add_argument("--baseline", default=str(BASELINE))
    args = ap.parse_args(argv)

    d = Path(args.dir)
    if not d.is_dir():
        # A missing dir would read as "nothing leaked": a green that measured nothing.
        print(f"FAIL  {d} is not a directory — the tests did not run with this TMPDIR", file=sys.stderr)
        return 2
    found = census(p.name for p in d.iterdir())

    for name, n in sorted(found.items()):
        print(f"{n:4}  {name}")
    print(f"{sum(found.values()):4}  total")
    if args.list:
        return 0

    base = Path(args.baseline)
    if args.update:
        base.write_text(json.dumps(dict(sorted(found.items())), indent=2) + "\n")
        print(f"baseline written: {base}")
        return 0

    allowed = Counter(json.loads(base.read_text()))
    problems = judge(found, allowed)
    for p in problems:
        print(f"FAIL  {p}", file=sys.stderr)
    if not problems:
        print(f"ok    the tests left exactly the known debt ({sum(allowed.values())})")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
