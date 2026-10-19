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
`cli_exec_ratchet.py --from-results` counts. Measured 2026-10-18 on
`manifest schema`.

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
KEPT_CHARS = 4000


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

check "huge output" ok bash -c 'head -c {OUTPUT_BYTES} /dev/zero | tr "\\0" x'
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
        # The writer keeps only the first 4000 characters; a huge output must be
        # cut to exactly that, not dropped and not kept whole.
        if len(rec["output"]) != KEPT_CHARS:
            print(
                f"FAIL: expected the output cut to {KEPT_CHARS} chars, "
                f"got {len(rec['output'])}"
            )
            return 1

    print(f"ok: a {OUTPUT_BYTES}-byte output still records one line, cut to {KEPT_CHARS}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
