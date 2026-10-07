#!/usr/bin/env python3
"""Tests for `scripts/cli_exec_ratchet.py`.

Run by the CI job `script tests`, which invokes every `scripts/test_*.py`.

Each case here exists because the gate can be wrong in that exact way and still
print a number: a shortest-prefix match files `vm snapshot create` under another
leaf's group, a global option before the subcommand reads as a subcommand, a
results file with nothing asserted reports a confident 0, and a ratchet that
only looks one way lets coverage fall.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent

# The canonical leaves have to include every target of the alias map, or the
# script refuses — which is the point of the guard and is how this fixture first
# failed. `container ls`/`vm rm`/`vm down` are NOT leaves: they are aliases.
LEAF_SET = {
    "api-resources",
    "container run",
    "container ps",
    "vm start",
    "vm stop",
    "vm destroy",
    "vm snapshot create",
    "get",
    "net ingress allow",
}


def load():
    spec = importlib.util.spec_from_file_location("cer", HERE / "cli_exec_ratchet.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def results(records) -> str:
    return "".join(json.dumps(r) + "\n" for r in records)


class LeafResolution(unittest.TestCase):
    def setUp(self):
        self.m = load()

    def test_the_longest_matching_prefix_wins(self):
        # `vm start` and `vm snapshot create` are both leaves. A shortest match
        # would resolve the three-word one to nothing (`vm snapshot` is a node)
        # and a greedy-but-unchecked one would stop at the first hit.
        self.assertEqual(
            self.m.leaves_of("/t/target/debug/delonix vm snapshot create s1 --vm x", LEAF_SET),
            {"vm snapshot create"},
        )
        self.assertEqual(self.m.leaves_of("/t/delonix vm start x", LEAF_SET), {"vm start"})

    def test_a_global_option_before_the_subcommand_is_not_the_subcommand(self):
        # `--l18n` is the only global option of the CLI and arrives in two
        # shapes; the two-word shape also hides its value from the match.
        self.assertEqual(self.m.leaves_of("/t/delonix --l18n=pt container ps", LEAF_SET), {"container ps"})
        self.assertEqual(self.m.leaves_of("/t/delonix --l18n pt container ps", LEAF_SET), {"container ps"})

    def test_the_binary_is_found_behind_a_wrapper_and_inside_a_shell_body(self):
        self.assertEqual(self.m.leaves_of("timeout 30 /t/delonix container run alpine", LEAF_SET), {"container run"})
        # `cmd` is a space-joined argv, so a `bash -c` body arrives unquoted.
        self.assertEqual(self.m.leaves_of("bash -c /t/delonix container ps | wc -l", LEAF_SET), {"container ps"})

    def test_a_command_that_never_invokes_the_engine_resolves_to_nothing(self):
        self.assertEqual(self.m.leaves_of("curl -fsS http://127.0.0.1:8080/healthz", LEAF_SET), set())
        self.assertEqual(self.m.leaves_of("nft list table ip dlxing", LEAF_SET), set())

    def test_a_path_argument_is_not_mistaken_for_the_binary(self):
        # The basename has to be exactly `delonix`: the battery passes paths like
        # `/tmp/delonix-e2e/x.yaml` and a substring match would read the next
        # word as a subcommand.
        self.assertEqual(self.m.leaves_of("cat /tmp/delonix-e2e/container run.yaml", LEAF_SET), set())

    def test_a_leaf_invoked_with_a_flag_first_still_resolves(self):
        self.assertEqual(self.m.leaves_of("/t/delonix get -o json networks", LEAF_SET), {"get"})

    def test_a_group_on_its_own_is_not_a_leaf(self):
        self.assertEqual(self.m.leaves_of("/t/delonix container --help", LEAF_SET), set())


class Numerator(unittest.TestCase):
    def setUp(self):
        self.m = load()

    def _file(self, text):
        d = Path(self.enterContext(tempfile.TemporaryDirectory()))
        p = d / "results.jsonl"
        p.write_text(text)
        return p

    def test_only_asserted_commands_count(self):
        # A SKIP carries no command at all, and a teardown line never reaches
        # results.jsonl — the numerator is what the battery JUDGED.
        p = self._file(
            results(
                [
                    {"name": "a", "verdict": "PASS", "rc": 0, "cmd": "/t/delonix container ps"},
                    {"name": "b", "verdict": "SKIP", "reason": "no kvm"},
                    {"name": "c", "verdict": "XFAIL", "rc": 1, "cmd": "/t/delonix vm start x"},
                ]
            )
        )
        executed, unmatched = self.m.executed_from_results([p], LEAF_SET)
        self.assertEqual(executed, {"container ps", "vm start"})
        self.assertEqual(unmatched, [])

    def test_a_run_with_nothing_asserted_refuses_instead_of_reporting_zero(self):
        p = self._file(results([{"name": "a", "verdict": "SKIP", "reason": "no kvm"}]))
        with self.assertRaises(SystemExit) as cm:
            self.m.executed_from_results([p], LEAF_SET)
        self.assertIn("nothing was measured", str(cm.exception))

    def test_a_help_check_is_neither_counted_nor_listed_as_unmatched(self):
        # Otherwise the audit list is 272 contract checks deep and nobody reads
        # it — and the list is the only way to see the number is not wrong.
        p = self._file(
            results(
                [
                    {"name": "a", "verdict": "PASS", "rc": 0, "cmd": "/t/delonix vm start --help"},
                    {"name": "b", "verdict": "PASS", "rc": 0, "cmd": "curl -fsS http://x/"},
                ]
            )
        )
        executed, unmatched = self.m.executed_from_results([p], LEAF_SET)
        self.assertEqual(executed, set())
        self.assertEqual(unmatched, ["curl -fsS http://x/"])

    def test_a_command_that_resolves_to_no_leaf_is_listed_not_counted(self):
        p = self._file(
            results(
                [
                    {"name": "a", "verdict": "PASS", "rc": 0, "cmd": "/t/delonix container ps"},
                    {"name": "b", "verdict": "PASS", "rc": 0, "cmd": "curl -fsS http://x/"},
                ]
            )
        )
        executed, unmatched = self.m.executed_from_results([p], LEAF_SET)
        self.assertEqual(executed, {"container ps"})
        self.assertEqual(unmatched, ["curl -fsS http://x/"])


class Gate(unittest.TestCase):
    def setUp(self):
        self.m = load()
        self.dir = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.m.LEAVES = self.dir / "cli_baseline.tsv"
        self.m.TRACE = self.dir / "trace.tsv"
        self.m.BASELINE = self.dir / "baseline.json"
        self.m.LEAVES.write_text("".join(f"=\t{leaf}\n" for leaf in sorted(LEAF_SET)))

    def _run(self, argv):
        import sys

        old = sys.argv
        sys.argv = ["cli_exec_ratchet.py"] + argv
        out, err = io.StringIO(), io.StringIO()
        try:
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                rc = self.m.main()
        finally:
            sys.argv = old
        return rc, out.getvalue() + err.getvalue()

    def _record(self, executed):
        self.m.write_trace(self.m.TRACE, set(executed), LEAF_SET, "test")
        self.m.BASELINE.write_text(json.dumps({"executed": len(executed), "leaves": len(LEAF_SET)}))

    def test_a_trace_that_matches_the_baseline_passes(self):
        self._record(["container ps", "vm start"])
        rc, out = self._run([])
        self.assertEqual(rc, 0, out)
        self.assertIn("2 of 9", out)
        self.assertIn("22.2 %", out)

    def test_coverage_that_dropped_fails(self):
        self._record(["container ps", "vm start"])
        self.m.write_trace(self.m.TRACE, {"container ps"}, LEAF_SET, "test")
        rc, out = self._run([])
        self.assertEqual(rc, 1)
        self.assertIn("coverage dropped", out)

    def test_coverage_that_rose_without_the_baseline_fails(self):
        self._record(["container ps"])
        self.m.write_trace(self.m.TRACE, {"container ps", "vm start"}, LEAF_SET, "test")
        rc, out = self._run([])
        self.assertEqual(rc, 1)
        self.assertIn("coverage rose", out)

    def test_a_leaf_added_to_the_cli_fails_until_someone_decides(self):
        # The numerator is unchanged and the fraction fell. Passing here would
        # let CLI surface grow with nobody exercising it.
        self._record(["container ps"])
        self.m.LEAVES.write_text(self.m.LEAVES.read_text() + "=\tsystem doctor\n")
        rc, out = self._run([])
        self.assertEqual(rc, 1)
        self.assertIn("the CLI grew", out)

    def test_a_trace_naming_a_leaf_the_cli_no_longer_has_fails(self):
        self._record(["container ps"])
        self.m.write_trace(self.m.TRACE, {"container ps", "volumes ls"}, LEAF_SET, "test")
        rc, out = self._run([])
        self.assertEqual(rc, 1)
        self.assertIn("no longer has", out)
        self.assertIn("volumes ls", out)

    def test_update_refuses_without_a_run_to_record_from(self):
        self._record(["container ps"])
        with self.assertRaises(SystemExit) as cm:
            self._run(["--update"])
        self.assertIn("--from-results", str(cm.exception))

    def test_a_missing_trace_says_how_to_record_one(self):
        self.m.BASELINE.write_text(json.dumps({"executed": 0, "leaves": len(LEAF_SET)}))
        with self.assertRaises(SystemExit) as cm:
            self._run([])
        self.assertIn("--from-results", str(cm.exception))


class Aliases(unittest.TestCase):
    def setUp(self):
        self.m = load()

    def test_an_alias_resolves_to_the_canonical_leaf(self):
        # The battery runs `container ls` more than any other command, and the
        # inventory only ever carries `container ps` — `--help` prints the
        # canonical name. Measured against a real run: without this the most
        # exercised leaf of the whole CLI counted as never executed.
        self.assertEqual(self.m.leaves_of("/t/delonix container ls -a", LEAF_SET), {"container ps"})
        self.assertEqual(self.m.leaves_of("/t/delonix vm rm x -f", LEAF_SET), {"vm destroy"})
        self.assertEqual(self.m.leaves_of("/t/delonix vm down x", LEAF_SET), {"vm stop"})

    def test_a_quoted_binary_path_is_still_the_binary(self):
        # A `bash -c` body keeps the quotes around the path.
        self.assertEqual(
            self.m.leaves_of("bash -c '/t/delonix' container ps -a | head -1", LEAF_SET),
            {"container ps"},
        )

    def test_the_literal_BIN_of_a_shell_body_is_the_binary(self):
        # The battery `export`s BIN, so a single-quoted `bash -c` body reaches
        # the trace with the name unexpanded — and the engine still ran.
        self.assertEqual(
            self.m.leaves_of('bash -c "$BIN" container ps -a | wc -l', LEAF_SET),
            {"container ps"},
        )

    def test_every_invocation_of_a_shell_body_counts_not_just_the_first(self):
        # A `check` asserts the outcome of the whole command, so all three were
        # judged. Counting only the first undercounted every composite check.
        body = 'bash -c "$BIN" container ps; "$BIN" vm start x; "$BIN" get pods'
        self.assertEqual(self.m.leaves_of(body, LEAF_SET), {"container ps", "vm start", "get"})

    def test_a_help_invocation_is_not_an_execution(self):
        # THE test of this gate. The battery checks the `--help` of every leaf
        # through `check`, so counting those gave 100% in every group on the
        # first real measurement — a number that would have read as full
        # coverage of a CLI that executes a third of itself.
        self.assertEqual(self.m.leaves_of("/t/delonix container run --help", LEAF_SET), set())
        self.assertEqual(self.m.leaves_of("/t/delonix vm snapshot create -h", LEAF_SET), set())
        self.assertEqual(self.m.leaves_of("/t/delonix container run --rm alpine true", LEAF_SET), {"container run"})

    def test_an_alias_pointing_at_nothing_fails_loudly(self):
        self.m.ALIASES = dict(self.m.ALIASES, **{"container list": "container gone"})
        with self.assertRaises(SystemExit) as cm:
            self.m.lookup_table(LEAF_SET)
        self.assertIn("is not a leaf", str(cm.exception))

    def test_the_map_covers_every_alias_the_source_declares(self):
        # A fourth `#[command(alias…)]` added to the CLI would silently
        # undercount; this is what stops the map going stale.
        src = HERE.parent / "bins" / "delonix-runtime-bin" / "src"
        found = set()
        for f in src.rglob("*.rs"):
            for line in f.read_text().splitlines():
                stripped = line.strip()
                if stripped.startswith("#[command(") and "alias" in stripped:
                    # `#[command(alias = "rm")]` / `#[command(visible_alias = "ls")]`
                    found.add(stripped.split('"')[1])
        declared = {alias.split()[-1] for alias in self.m.ALIASES}
        self.assertEqual(
            found,
            declared,
            f"the CLI declares {sorted(found)} as command aliases and the map carries {sorted(declared)}",
        )


class Formatting(unittest.TestCase):
    def setUp(self):
        self.m = load()

    def test_the_percent_rounds_half_up_without_a_float(self):
        self.assertEqual(self.m.percent(1, 8), "12.5")
        self.assertEqual(self.m.percent(1, 16), "6.3")  # 6.25 rounds up
        self.assertEqual(self.m.percent(91, 244), "37.3")
        self.assertEqual(self.m.percent(0, 10), "0.0")
        self.assertEqual(self.m.percent(0, 0), "0.0")

    def test_the_denominator_refuses_an_empty_inventory(self):
        d = Path(self.enterContext(tempfile.TemporaryDirectory()))
        p = d / "empty.tsv"
        p.write_text("")
        with self.assertRaises(SystemExit) as cm:
            self.m.read_leaves(p)
        self.assertIn("refusing to measure", str(cm.exception))

    def test_the_trace_carries_its_provenance(self):
        d = Path(self.enterContext(tempfile.TemporaryDirectory()))
        p = d / "t.tsv"
        self.m.write_trace(p, {"container ps"}, LEAF_SET, "host X, 2026-10-07")
        executed, meta = self.m.read_trace(p)
        self.assertEqual(executed, {"container ps"})
        self.assertEqual(meta["recorded"], "host X, 2026-10-07")
        self.assertEqual(meta["executed"], "1")
        self.assertEqual(meta["leaves"], "9")


if __name__ == "__main__":
    unittest.main(verbosity=2)
