#!/usr/bin/env python3
"""Unit tests for `scripts/tmp_roots_gate.py` — stdlib `unittest`, no new
dependency (same reasoning as `test_release_verify.py`).

Each test fixes a decision: the same leak has the same name on every run, a new
leak fails, a fixed leak fails until the baseline is lowered, and a missing
TMPDIR is not "nothing leaked".

Run: python3 scripts/test_tmp_roots_gate.py
"""
import io
import json
import sys
import tempfile
import unittest
from collections import Counter
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import tmp_roots_gate as g  # noqa: E402


def run(tmp: Path, baseline: dict, *extra):
    base = tmp / "baseline.json"
    base.write_text(json.dumps(baseline))
    out, err = io.StringIO(), io.StringIO()
    with redirect_stdout(out), redirect_stderr(err):
        rc = g.main(["--dir", str(tmp / "t"), "--baseline", str(base), *extra])
    return rc, out.getvalue(), err.getvalue()


class TmpRootsGate(unittest.TestCase):
    def setUp(self):
        self._td = tempfile.TemporaryDirectory()
        self.tmp = Path(self._td.name)
        (self.tmp / "t").mkdir()

    def tearDown(self):
        self._td.cleanup()

    def leave(self, *names):
        for n in names:
            (self.tmp / "t" / n).mkdir()

    def test_pids_and_thread_ids_do_not_make_a_new_name(self):
        self.assertEqual(g.normalize("delonix-net-race-1605459"), "delonix-net-race-N")
        self.assertEqual(
            g.normalize("dlx-vmresolve-contexto-1592406-ThreadId(887)"),
            "dlx-vmresolve-contexto-N-ThreadId(N)",
        )

    def test_exactly_the_known_debt_passes(self):
        self.leave("dlx-low-1-2", "dlx-low-3-4")
        rc, _, err = run(self.tmp, {"dlx-low-N-N": 2})
        self.assertEqual(rc, 0, err)

    def test_the_567_leak_coming_back_fails(self):
        self.leave("delonix-net-race-4242")
        rc, _, err = run(self.tmp, {})
        self.assertEqual(rc, 1)
        self.assertIn("new leak: delonix-net-race-N", err)

    def test_more_of_a_known_leak_fails(self):
        self.leave("dlx-low-1-2", "dlx-low-3-4")
        rc, _, err = run(self.tmp, {"dlx-low-N-N": 1})
        self.assertEqual(rc, 1)
        self.assertIn("more of a known leak", err)

    def test_a_fixed_leak_fails_until_the_baseline_is_lowered(self):
        rc, _, err = run(self.tmp, {"dlx-detect-go": 1})
        self.assertEqual(rc, 1)
        self.assertIn("--update", err)

    def test_update_writes_what_was_found(self):
        self.leave("dlx-detect-go", "dlx-low-1-2")
        rc, _, _ = run(self.tmp, {"stale": 3}, "--update")
        self.assertEqual(rc, 0)
        written = json.loads((self.tmp / "baseline.json").read_text())
        self.assertEqual(Counter(written), Counter({"dlx-detect-go": 1, "dlx-low-N-N": 1}))

    def test_a_missing_tmpdir_is_not_a_clean_run(self):
        (self.tmp / "t").rmdir()
        rc, _, err = run(self.tmp, {})
        self.assertEqual(rc, 2)
        self.assertIn("not a directory", err)


if __name__ == "__main__":
    unittest.main()
