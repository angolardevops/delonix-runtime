#!/usr/bin/env python3
"""Tests for `scripts/folhas_gate.py`.

Run by the CI job `script tests`, which invokes every `scripts/test_*.py`.

Each case here exists because the gate can be wrong in that exact way and still
print a confident number — which is the failure this whole instrument was built
to stop. The first version of it went green over a table that was missing a
whole surface, because it only ever iterated the table and never the code.

The decisive ones are `a_leaf_the_code_has_and_the_table_lacks_fails` and
`a_cli_leaf_the_trace_proves_cannot_be_demoted_by_the_table`: the first is the
fail-closed property (a new MCP tool or node RPC cannot land unclassified), the
second is what stops the table from being the thing that decides whether the CLI
is covered. The trace decides; the table may only agree.
"""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load(root: Path):
    """Load the gate with every path it reads pointed into a temp tree."""
    spec = importlib.util.spec_from_file_location("fg", HERE / "folhas_gate.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)

    mod.REPO = root
    mod.TABLE = root / "folhas.tsv"
    mod.BASELINE = root / "folhas_baseline.json"
    mod.CLI_BASELINE = root / "cli_baseline.tsv"
    mod.CLI_TRACE = root / "cli_exec_trace.tsv"

    # Only two surfaces in the fixture: enough to prove every rule, and a
    # fixture that mirrored all six would be a second copy of the real tree.
    mod.SURFACES = {
        "cli": mod.cli_leaves,
        "mcp": lambda: ["runtime.info", "logs.query"],
    }
    return mod


def write(root: Path, *, table: str, trace: str = "container run\n", baseline=None):
    (root / "cli_baseline.tsv").write_text("=\tcontainer run\n~\tvm pause\n", encoding="utf-8")
    (root / "cli_exec_trace.tsv").write_text("# provenance\n" + trace, encoding="utf-8")
    (root / "folhas.tsv").write_text(table, encoding="utf-8")
    if baseline is not None:
        (root / "folhas_baseline.json").write_text(json.dumps(baseline), encoding="utf-8")


# A table that agrees with the fixture code and the fixture trace.
GOOD = (
    "cli\tcontainer run\tA\tthe trace\n"
    "cli\tvm pause\tE\tneeds a bootable VM image\n"
    "mcp\truntime.info\tB\tno test of any kind\n"
    "mcp\tlogs.query\tC\tname-presence in tools/list only\n"
)
BASE_OK = {"surfaces": {"cli": {"proved": 1, "leaves": 2}, "mcp": {"proved": 0, "leaves": 2}}}


class FolhasGate(unittest.TestCase):
    def check(self, *, table, trace="container run\n", baseline=BASE_OK):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            write(root, table=table, trace=trace, baseline=baseline)
            mod = load(root)
            per_surface, failures = mod.evaluate()
            return per_surface, failures

    def test_the_fixture_itself_is_clean(self):
        """Without this, every assertion below could pass for the wrong reason."""
        per_surface, failures = self.check(table=GOOD)
        self.assertEqual(failures, [])
        self.assertEqual(per_surface["cli"], {"proved": 1, "leaves": 2})
        self.assertEqual(per_surface["mcp"], {"proved": 0, "leaves": 2})

    def test_a_leaf_the_code_has_and_the_table_lacks_fails(self):
        """The fail-closed property: a new tool cannot land unclassified."""
        table = GOOD.replace("mcp\tlogs.query\tC\tname-presence in tools/list only\n", "")
        _, failures = self.check(table=table)
        self.assertTrue(
            any("logs.query" in f and "no row" in f for f in failures),
            f"expected the missing leaf to be named, got {failures}",
        )

    def test_a_row_for_a_leaf_the_code_lost_fails(self):
        table = GOOD + "mcp\tevents.list\tB\tADR-0025 says it exists; it does not\n"
        _, failures = self.check(table=table)
        self.assertTrue(
            any("events.list" in f and "no longer exists" in f for f in failures),
            f"expected the stale row to be named, got {failures}",
        )

    def test_a_class_a_with_no_evidence_fails(self):
        table = GOOD.replace("cli\tcontainer run\tA\tthe trace", "cli\tcontainer run\tA\t-")
        _, failures = self.check(table=table)
        self.assertTrue(
            any("container run" in f and "no evidence" in f for f in failures),
            f"expected the evidence-less claim to be named, got {failures}",
        )

    def test_a_non_a_with_no_reason_fails(self):
        """`not proved, and nobody said why` is the one state that must not be writable."""
        table = GOOD.replace("mcp\truntime.info\tB\tno test of any kind", "mcp\truntime.info\tB\t-")
        _, failures = self.check(table=table)
        self.assertTrue(
            any("runtime.info" in f and "no evidence" in f for f in failures),
            f"expected the reasonless B to be named, got {failures}",
        )

    def test_a_cli_leaf_the_trace_proves_cannot_be_demoted_by_the_table(self):
        """The trace decides the CLI's class A; the table may only agree."""
        table = GOOD.replace("cli\tcontainer run\tA\tthe trace", "cli\tcontainer run\tB\tnot run")
        _, failures = self.check(table=table)
        self.assertTrue(
            any("container run" in f and "promote it to A" in f for f in failures),
            f"expected the demotion to be refused, got {failures}",
        )

    def test_a_cli_leaf_the_trace_does_not_prove_cannot_be_promoted_by_the_table(self):
        table = GOOD.replace(
            "cli\tvm pause\tE\tneeds a bootable VM image", "cli\tvm pause\tA\twishful thinking"
        )
        _, failures = self.check(table=table)
        self.assertTrue(
            any("vm pause" in f and "does not record it executed" in f for f in failures),
            f"expected the unbacked promotion to be refused, got {failures}",
        )

    def test_a_malformed_row_fails_instead_of_being_skipped(self):
        for bad, why in [
            ("cli\tcontainer run\tA\n", "3 fields"),
            ("cli\tcontainer run\tZ\treason\n", "an unknown class"),
            ("storage\tsomething\tA\treason\n", "an unknown surface"),
        ]:
            with self.subTest(why=why):
                _, failures = self.check(table=GOOD + bad)
                self.assertTrue(failures, f"a row with {why} was silently skipped")

    def test_a_duplicate_row_fails(self):
        _, failures = self.check(table=GOOD + "mcp\truntime.info\tA\tsays otherwise twice\n")
        self.assertTrue(
            any("duplicate" in f for f in failures), f"expected a duplicate to fail, got {failures}"
        )

    def test_the_ratchet_bites_in_both_directions(self):
        """A drop is a regression; a rise without the baseline is an invisible win."""
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            # Recorded 2 proved for cli, measured 1 -> a regression.
            write(
                root,
                table=GOOD,
                baseline={
                    "surfaces": {"cli": {"proved": 2, "leaves": 2}, "mcp": {"proved": 0, "leaves": 2}}
                },
            )
            mod = load(root)
            self.assertEqual(mod.main(), 1, "a fall in proved leaves has to fail")

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            # Recorded 0, measured 1 -> a win nobody recorded.
            write(
                root,
                table=GOOD,
                baseline={
                    "surfaces": {"cli": {"proved": 0, "leaves": 2}, "mcp": {"proved": 0, "leaves": 2}}
                },
            )
            mod = load(root)
            self.assertEqual(mod.main(), 1, "a rise without the baseline moving has to fail")

    def test_a_surface_that_changed_size_fails(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            write(
                root,
                table=GOOD,
                baseline={
                    "surfaces": {"cli": {"proved": 1, "leaves": 2}, "mcp": {"proved": 0, "leaves": 9}}
                },
            )
            mod = load(root)
            self.assertEqual(mod.main(), 1, "a surface gaining or losing a leaf has to fail")

    def test_a_missing_baseline_refuses_instead_of_passing(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            write(root, table=GOOD, baseline=None)
            mod = load(root)
            self.assertEqual(mod.main(), 1, "no baseline must never read as green")

    def test_update_refuses_to_record_over_a_failing_table(self):
        """Otherwise `--update` becomes the way to make any failure go away."""
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            write(root, table=GOOD.replace("mcp\tlogs.query\tC\tname-presence in tools/list only\n", ""))
            mod = load(root)
            mod.BASELINE.unlink(missing_ok=True)
            import sys
            argv = sys.argv
            sys.argv = ["folhas_gate.py", "--update"]
            try:
                self.assertEqual(mod.main(), 1, "--update must not paper over a broken table")
            finally:
                sys.argv = argv
            self.assertFalse(mod.BASELINE.exists(), "--update wrote a baseline anyway")


class Extractors(unittest.TestCase):
    """The real extractors, against the real tree.

    These are the numbers `docs/testing/folhas-inventario.md` publishes. If an
    extractor silently starts returning nothing — the shape it is grepping for
    changed — the gate would report a surface of zero leaves and still pass its
    own table check. These pin the magnitudes measured on 2026-10-18.
    """

    def setUp(self):
        spec = importlib.util.spec_from_file_location("fg_real", HERE / "folhas_gate.py")
        self.mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.mod)

    def test_every_surface_derives_a_plausible_denominator(self):
        for surface, fn in self.mod.SURFACES.items():
            with self.subTest(surface=surface):
                leaves = fn()
                self.assertGreater(len(leaves), 10, f"{surface} derived almost nothing")
                self.assertEqual(len(leaves), len(set(leaves)), f"{surface} derived duplicates")
                self.assertTrue(all(l.strip() for l in leaves), f"{surface} derived a blank leaf")

    def test_the_kinds_struct_definition_is_not_counted_as_a_kind(self):
        """`grep -c 'KindFacts {'` gives 26; the table has 25 rows."""
        self.assertNotIn("", self.mod.kind_plurals())
        self.assertEqual(len(self.mod.kind_plurals()) * 5, len(self.mod.kind_leaves()))

    def test_mcp_tool_names_come_from_the_attribute_not_the_function(self):
        tools = self.mod.mcp_tools()
        self.assertIn("runtime.info", tools, "the published name is dotted, not snake_case")
        self.assertNotIn("runtime_info", tools)


if __name__ == "__main__":
    unittest.main(verbosity=2)
