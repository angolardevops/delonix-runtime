#!/usr/bin/env python3
"""Tests for `scripts/capability_ratchet.py`.

Run by the CI job `script tests`, which invokes every `scripts/test_*.py`.
"""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load():
    spec = importlib.util.spec_from_file_location("cr", HERE / "capability_ratchet.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


MATRIX = """# Provider capability matrix

prose mentioning `unsupported-by-provider`, which a grep would count.

**Cell metric (plan 65, level N2): {proved} of {applicable} applicable cells are proved — {pct} %.** A cell is one capability.

| domain | proved (N2) | applicable | % |
|---|---|---|---|
| inventory | 4 | 16 | 25.0 |
| vm-compute | {proved_vm} | {applicable_vm} | 44.1 |

## compute

- **libvirt**: N2 16 of 50 applicable (32.0 %) — 16 supported, 24 partial

| domain | capability | libvirt |
|---|---|---|
| vm-compute | `vm.create` | supported — e2e:something with an escaped \\| pipe |
"""


class Ratchet(unittest.TestCase):
    def setUp(self):
        self.mod = load()
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        d = Path(self.tmp.name)
        self.mod.MATRIX = d / "capability-matrix.md"
        self.mod.BASELINE = d / "capability_baseline.json"
        self.mod.ROOT = d

    def write(self, proved=100, applicable=252, pct="39.7", proved_vm=26, applicable_vm=59):
        self.mod.MATRIX.write_text(
            MATRIX.format(
                proved=proved,
                applicable=applicable,
                pct=pct,
                proved_vm=proved_vm,
                applicable_vm=applicable_vm,
            ),
            encoding="utf-8",
        )

    def baseline(self, proved, applicable):
        self.mod.BASELINE.write_text(
            json.dumps({"proved": proved, "applicable": applicable}), encoding="utf-8"
        )

    def run_gate(self, argv=()):
        import sys

        old = sys.argv
        sys.argv = ["capability_ratchet.py", *argv]
        try:
            return self.mod.main()
        finally:
            sys.argv = old

    def test_a_pipe_in_a_cell_does_not_confuse_the_reader(self):
        """Why the metric is a LINE and not the table: cells carry an escaped `\\|`."""
        self.write()
        proved, applicable, domains = self.mod.read_matrix()
        self.assertEqual((proved, applicable), (100, 252))
        self.assertEqual(domains[0], ("inventory", 4, 16))

    def test_the_prose_is_not_counted_as_cells(self):
        """The defect this gate replaces: the grep counted the prose itself."""
        self.write()
        text = self.mod.MATRIX.read_text(encoding="utf-8")
        self.assertIn("`unsupported-by-provider`", text, "the prose is there on purpose")
        proved, _applicable, _ = self.mod.read_matrix()
        self.assertEqual(proved, 100, "the number comes from the line, not from counting words")

    def test_a_drop_fails(self):
        self.write(proved=99)
        self.baseline(100, 252)
        self.assertEqual(self.run_gate(), 1)

    def test_a_rise_without_recording_it_fails(self):
        self.write(proved=101)
        self.baseline(100, 252)
        self.assertEqual(self.run_gate(), 1)

    def test_a_rise_recorded_in_the_same_commit_passes(self):
        self.write(proved=101)
        self.baseline(101, 252)
        self.assertEqual(self.run_gate(), 0)

    def test_a_catalog_that_grew_has_to_be_recorded(self):
        """A new capability moves the denominator: the change has to be stated."""
        self.write(applicable=253)
        self.baseline(100, 252)
        self.assertEqual(self.run_gate(), 1)

    def test_a_matrix_without_the_metric_line_refuses(self):
        self.mod.MATRIX.write_text("# sem métrica\n", encoding="utf-8")
        self.baseline(100, 252)
        with self.assertRaises(SystemExit):
            self.run_gate()

    def test_zero_applicable_refuses_instead_of_passing(self):
        """A gate that measures nothing reads as green."""
        self.write(proved=0, applicable=0)
        self.baseline(0, 0)
        with self.assertRaises(SystemExit):
            self.run_gate()

    def test_update_records_what_the_matrix_says(self):
        self.write(proved=123, applicable=456)
        self.assertEqual(self.run_gate(["--update"]), 0)
        self.assertEqual(
            json.loads(self.mod.BASELINE.read_text(encoding="utf-8")),
            {"proved": 123, "applicable": 456},
        )

    def test_percent_matches_the_engines_integer_rounding(self):
        """The same arithmetic as Rust's `CellMetric::percent`: half up, no float."""
        for (p, a), want in {
            (1, 3): "33.3",
            (2, 3): "66.7",
            (100, 252): "39.7",
            (1, 16): "6.3",
            (1, 2): "50.0",
        }.items():
            self.assertEqual(self.mod.percent(p, a), want, f"{p}/{a}")


if __name__ == "__main__":
    unittest.main(verbosity=1)
