#!/usr/bin/env python3
"""Unit tests for `scripts/tmp_roots_gate.py` — stdlib `unittest`, no new
dependency (same reasoning as `test_release_verify.py`).

Each test fixes a decision: the same leak has the same name on every run, a new
leak fails, a fixed leak fails until the baseline is lowered, a missing
TMPDIR is not "nothing leaked", and in a shared `/tmp` only what was not there
before the tests is theirs.

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

    # ---- `/tmp`, judged against a listing taken before the tests ----

    def before(self, *names):
        f = self.tmp / "before.txt"
        f.write_text("".join(f"{n}\n" for n in names))
        return str(f)

    def test_what_was_in_tmp_before_the_tests_is_not_theirs(self):
        self.leave("systemd-private-abc", "snap-private-tmp", "dlx-cgp-1")
        listing = self.before("systemd-private-abc", "snap-private-tmp", "dlx-cgp-1")
        rc, out, err = run(self.tmp, {}, "--before", listing)
        self.assertEqual(rc, 0, err)
        self.assertIn("0  total", out)

    def test_a_socket_a_failed_test_left_in_tmp_fails(self):
        # What the gRPC tests left when an assert failed before their last
        # line: a pid-named socket next to what the runner already had.
        self.leave("systemd-private-abc")
        (self.tmp / "t" / "dlx-grpc-t40496.sock").touch()
        rc, _, err = run(self.tmp, {}, "--before", self.before("systemd-private-abc"))
        self.assertEqual(rc, 1)
        self.assertIn("new leak: dlx-grpc-tN.sock", err)

    def test_before_matches_the_exact_name_not_the_normalized_one(self):
        # A pid-named entry from an earlier run does not excuse this run's.
        self.leave("dlx-node-t41.sock")
        rc, _, err = run(self.tmp, {}, "--before", self.before("dlx-node-t40.sock"))
        self.assertEqual(rc, 1)
        self.assertIn("new leak: dlx-node-tN.sock", err)

    def test_a_missing_before_listing_is_not_a_clean_run(self):
        rc, _, err = run(self.tmp, {}, "--before", str(self.tmp / "nope.txt"))
        self.assertEqual(rc, 2)
        self.assertIn("no listing", err)

    def test_update_does_not_take_before(self):
        with self.assertRaises(SystemExit) as e, redirect_stderr(io.StringIO()):
            run(self.tmp, {}, "--update", "--before", self.before())
        self.assertEqual(e.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
