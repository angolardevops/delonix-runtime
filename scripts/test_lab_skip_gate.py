import json
import tempfile
import unittest
from pathlib import Path

import lab_skip_gate as g


def results(*recs):
    d = tempfile.mkdtemp()
    p = Path(d) / "results.jsonl"
    p.write_text("\n".join(json.dumps(r) for r in recs) + "\n")
    return p


class LabSkipGate(unittest.TestCase):
    def test_an_undeclared_skip_fails(self):
        run = g.skips(results({"name": "a", "verdict": "PASS"}, {"name": "b", "verdict": "SKIP", "reason": "no x"}))
        self.assertEqual(g.verdict(run, {}), ["new SKIP: b — no x"])

    def test_a_declared_skip_passes(self):
        run = g.skips(results({"name": "b", "verdict": "SKIP", "reason": "no x"}))
        self.assertEqual(g.verdict(run, {"b": "no gpu"}), [])

    def test_a_declared_skip_that_ran_fails(self):
        run = g.skips(results({"name": "b", "verdict": "PASS"}))
        self.assertEqual(len(g.verdict(run, {"b": "no gpu"})), 1)

    def test_a_declared_line_without_a_reason_is_refused(self):
        d = Path(tempfile.mkdtemp()) / "allowed.txt"
        d.write_text("b\n")
        with self.assertRaises(SystemExit):
            g.allowed(d)

    def test_the_committed_list_parses(self):
        g.allowed()


if __name__ == "__main__":
    unittest.main()
