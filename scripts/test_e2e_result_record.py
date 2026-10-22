#!/usr/bin/env python3
"""`check` must record its line even when the command output is huge.

Regression test for `scripts/e2e.sh`. The result writer used to pass the
captured output straight through ARGV:

    python3 - "$name" "$verdict" "$rc" "$out" "$*" >>"$OUT/results.jsonl"

Linux caps a SINGLE argv string at MAX_ARG_STRLEN (128 KiB), independently of
how much room the total has. A check whose output exceeded that killed
`python3` with "Argument list too long" and the line was never appended — while
the console still printed PASS. The run therefore read as complete and
`results.jsonl` had silently lost the leaf, which is exactly what
`cli_exec_ratchet.py --from-results` counts. Measured on `manifest schema`.

The writer now hands the output and the command to `python3` through FILES,
and a long output keeps its first 1000 characters, a marker naming how many
were cut, and its last 3000 — the end is where a failing step says why. This
test asserts that shape exactly: one line, the head and the tail present
verbatim, the marker with the right count, and nothing else.

The test drives the REAL `check` out of the script (not a restatement of the
pattern), so pointing it at a pre-fix `e2e.sh` fails it:

    python3 scripts/test_e2e_result_record.py <path-to-e2e.sh>
"""

import json
import pathlib
import subprocess
import sys
import tempfile

REPO = pathlib.Path(__file__).resolve().parent.parent
DEFAULT = REPO / "scripts" / "e2e.sh"

# Comfortably past MAX_ARG_STRLEN (128 KiB) so the pre-fix shape cannot survive.
OUTPUT_BYTES = 200_000
# What the writer keeps of a long output: this much of the head, this much of
# the tail, and a marker between them.
HEAD_CHARS = 1000
TAIL_CHARS = 3000
KEPT_CHARS = HEAD_CHARS + TAIL_CHARS


def expected_output() -> str:
    """The exact bytes the check's command prints.

    Distinct head and tail (`H`, `T`) around a body of `x`, so a writer that
    keeps only the head, only the tail, or the wrong amount of either cannot
    pass by printing `x` in the right length.
    """
    body = OUTPUT_BYTES - KEPT_CHARS
    return "H" * HEAD_CHARS + "x" * body + "T" * TAIL_CHARS


def extract_check(script: pathlib.Path) -> str:
    """The `check()` function, verbatim, from `check() {` to its closing brace."""
    lines = script.read_text().splitlines()
    start = next(
        (i for i, ln in enumerate(lines) if ln.startswith("check() {")), None
    )
    if start is None:
        raise SystemExit(f"FAIL: no `check() {{` in {script}")
    end = next((i for i in range(start + 1, len(lines)) if lines[i] == "}"), None)
    if end is None:
        raise SystemExit(f"FAIL: `check()` in {script} is not closed by a bare `}}`")
    return "\n".join(lines[start : end + 1])


def run_one_check(script: pathlib.Path, out_dir: pathlib.Path) -> subprocess.CompletedProcess:
    """Run a single `check` whose command prints OUTPUT_BYTES bytes."""
    harness = f"""
set -uo pipefail
OUT={out_dir}
PASS=0; FAIL=0
declare -a FAILED_NAMES=()
log() {{ printf '%s\\n' "$*"; }}

{extract_check(script)}

check "huge output" ok python3 -c 'import sys; sys.stdout.write("H"*{HEAD_CHARS} + "x"*{OUTPUT_BYTES - KEPT_CHARS} + "T"*{TAIL_CHARS})'
"""
    return subprocess.run(
        ["bash", "-c", harness], capture_output=True, text=True, timeout=120
    )


def main() -> int:
    script = pathlib.Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else DEFAULT
    if not script.is_file():
        print(f"FAIL: {script} does not exist")
        return 1

    with tempfile.TemporaryDirectory(prefix="e2e-record-") as tmp:
        out_dir = pathlib.Path(tmp)
        proc = run_one_check(script, out_dir)
        results = out_dir / "results.jsonl"

        if "Argument list too long" in proc.stderr:
            print("FAIL: the writer still passes the output through argv")
            print(f"      stderr: {proc.stderr.strip().splitlines()[:1]}")
            return 1

        if not results.is_file():
            print(f"FAIL: no results.jsonl written at all\n      stderr: {proc.stderr.strip()}")
            return 1

        lines = [ln for ln in results.read_text().splitlines() if ln.strip()]
        if len(lines) != 1:
            print(f"FAIL: expected exactly 1 recorded line, got {len(lines)}")
            return 1

        try:
            rec = json.loads(lines[0])
        except json.JSONDecodeError as exc:
            print(f"FAIL: the recorded line is not valid JSON: {exc}")
            return 1

        for field in ("name", "verdict", "rc", "cmd", "output"):
            if field not in rec:
                print(f"FAIL: the record is missing `{field}`")
                return 1

        if rec["name"] != "huge output":
            print(f"FAIL: wrong name recorded: {rec['name']!r}")
            return 1
        if rec["verdict"] != "PASS":
            print(f"FAIL: expected PASS, got {rec['verdict']!r}")
            return 1
        # A long output keeps its head and its tail verbatim, with a marker
        # between them naming how many characters were cut — never dropped,
        # never kept whole, never only the head.
        full = expected_output()
        marker = "\n[... %d chars cut ...]\n" % (len(full) - KEPT_CHARS)
        want = full[:HEAD_CHARS] + marker + full[-TAIL_CHARS:]
        got = rec["output"]
        if not got.startswith(full[:HEAD_CHARS]):
            print(f"FAIL: the first {HEAD_CHARS} chars of the output were not kept")
            return 1
        if not got.endswith(full[-TAIL_CHARS:]):
            print(f"FAIL: the last {TAIL_CHARS} chars of the output were not kept")
            return 1
        if marker not in got:
            print(f"FAIL: no cut marker {marker.strip()!r} in the recorded output")
            return 1
        if got != want:
            print(
                f"FAIL: expected {len(want)} chars (head + marker + tail), "
                f"got {len(got)}"
            )
            return 1
        if "python3 -c" not in rec["cmd"]:
            print(f"FAIL: the command line was not recorded: {rec['cmd'][:80]!r}")
            return 1

    print(
        f"ok: a {OUTPUT_BYTES}-byte output still records one line, "
        f"head {HEAD_CHARS} + marker + tail {TAIL_CHARS}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
