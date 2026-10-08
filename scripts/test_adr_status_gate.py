#!/usr/bin/env python3
"""Tests for `scripts/adr_status_gate.py`, run by the CI job `script-tests`."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

GATE = Path(__file__).resolve().parent / "adr_status_gate.py"

CLEAN = """# ADR-0001: a decision

- **Status:** Accepted (2026-01-01)
- **Date:** 2026-01-01

## Context
"""

CONTRADICTS = """# ADR-0002: another

- **Status:** Proposed (2026-01-01). Nothing implemented. The owner decides
  whether this becomes Accepted.
- **Date:** 2026-01-01

## Addendum 2026-02-01 — P0 built: the port and the driver
"""

CONTRADICTS_PT = """# ADR-0003: outra

- **Estado:** Proposto (2026-01-01). Nada implementado.
- **Data:** 2026-01-01

## Addendum 2026-02-01 — P0 implementado
"""

# Says nothing is implemented, and the addendum does NOT claim anything was
# built: honest, and must pass.
NOTHING_BUT_HONEST = """# ADR-0004: a third

- **Status:** Proposed (2026-01-01). Nothing implemented.
- **Date:** 2026-01-01

## Addendum 2026-02-01 — the spike's raw numbers
"""


def run(files: dict[str, str], readme: str | None = None) -> subprocess.CompletedProcess:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        adr = root / "docs" / "adr"
        adr.mkdir(parents=True)
        (root / "scripts").mkdir()
        gate = root / "scripts" / "adr_status_gate.py"
        gate.write_text(GATE.read_text())
        for name, body in files.items():
            (adr / name).write_text(body)
        if readme is not None:
            (adr / "README.md").write_text(readme)
        return subprocess.run(
            [sys.executable, str(gate)], capture_output=True, text=True, cwd=root
        )


class AdrStatusGate(unittest.TestCase):
    def test_a_clean_adr_passes(self):
        r = run({"0001-a.md": CLEAN})
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("1 ADR(s)", r.stdout)

    def test_nothing_implemented_next_to_a_built_addendum_fails(self):
        r = run({"0002-b.md": CONTRADICTS})
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("0002-b.md", r.stdout)

    def test_the_portuguese_wording_is_caught_too(self):
        r = run({"0003-c.md": CONTRADICTS_PT})
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("0003-c.md", r.stdout)

    def test_nothing_implemented_with_an_honest_addendum_passes(self):
        r = run({"0004-d.md": NOTHING_BUT_HONEST})
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    def test_the_index_disagreeing_with_the_document_fails(self):
        """Seven rows disagreed on 2026-10-06, two of them for twelve days."""
        readme = (
            "| ADR | What | State |\n|---|---|---|\n"
            "| [0001](0001-a.md) | a decision | **Proposed** — some note |\n"
        )
        r = run({"0001-a.md": CLEAN}, readme=readme)
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("index is lying", r.stdout)

    def test_the_index_agreeing_passes_and_is_counted(self):
        readme = (
            "| ADR | What | State |\n|---|---|---|\n"
            "| [0001](0001-a.md) | a decision | **Accepted 2026-01-01** — note |\n"
        )
        r = run({"0001-a.md": CLEAN}, readme=readme)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("1 index row(s)", r.stdout)

    # --- the index's OTHER shape: the bullet list (measured 2026-10-08) --------
    # The table check covered none of the thirteen most recent ADRs, because the
    # table stops at 0062 and a bullet list carries 0061 and 0063 onwards. Four
    # of them were lying. Each test below fails with the bullet reader removed.

    def test_a_bullet_index_disagreeing_with_the_document_fails(self):
        readme = "- **0001** — a decision, explained at length. *Proposed*.\n"
        r = run({"0001-a.md": CLEAN}, readme=readme)
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("the bullet for 0001", r.stdout)
        self.assertIn("index is lying", r.stdout)

    def test_a_bullet_index_agreeing_passes_and_is_counted(self):
        readme = "- **0001** — a decision, explained at length. *Accepted* (2026-01-01).\n"
        r = run({"0001-a.md": CLEAN}, readme=readme)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("1 index row(s)", r.stdout)

    def test_a_bullet_that_names_no_state_fails(self):
        """ADR-0002's entry carried `*Phase 2a implemented*` and no state at all."""
        readme = "- **0001** — a decision. *Phase 2a implemented*, the rest deferred.\n"
        r = run({"0001-a.md": CLEAN}, readme=readme)
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("names no state", r.stdout)

    def test_a_status_word_in_the_prose_does_not_win_over_the_italic(self):
        """ADR-0072's entry says «rejected for the Kind move» before its *Accepted*."""
        readme = (
            "- **0001** — a decision; the Kind move was rejected for its own reasons. "
            "*Accepted* (2026-01-01).\n"
        )
        r = run({"0001-a.md": CLEAN}, readme=readme)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        # The row count, not just the exit code: a gate that ignores bullets
        # ALSO exits 0 here, so without this line the test passes on the broken
        # reader and proves nothing. Measured — it did.
        self.assertIn("1 index row(s)", r.stdout)

    def test_the_bold_number_does_not_swallow_the_italic_state(self):
        """Naive asterisk pairing read `**0001**` as an italic and hid the state."""
        readme = (
            "- **0001** — a decision with **bold** in the middle of it. "
            "*Accepted* (2026-01-01).\n"
        )
        r = run({"0001-a.md": CLEAN}, readme=readme)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("1 index row(s)", r.stdout)

    def test_a_state_on_a_continuation_line_is_read(self):
        """Ten of the eighteen entries carried their state on a wrapped line."""
        readme = (
            "- **0001** — a decision long enough that its entry wraps over\n"
            "  more than one line before it ever gets to the state.\n"
            "  *Proposed*.\n"
        )
        r = run({"0001-a.md": CLEAN}, readme=readme)
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("the bullet for 0001", r.stdout)

    def test_a_gate_that_read_nothing_fails(self):
        """A green by absence is the failure mode this repo has paid for twice."""
        r = run({"0005-e.md": "# ADR-0005\n\nNo status line at all.\n"})
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("read nothing", r.stdout)


if __name__ == "__main__":
    unittest.main()
