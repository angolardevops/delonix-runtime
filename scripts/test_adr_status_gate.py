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

    # --- the status cell is read at its HEAD (migration of 2026-10-08) --------
    # The bullet list became table rows, so the prose that used to follow an
    # italic state now sits in the status cell. The convention that makes the
    # first match correct is that the cell BEGINS with the state.

    def test_prose_after_the_state_does_not_change_it(self):
        """0072's cell says «rejected for the Kind move» after its **Accepted**."""
        readme = (
            "| ADR | What | State |\n|---|---|---|\n"
            "| [0001](0001-a.md) | a decision | **Accepted** (2026-01-01) — the Kind move "
            "was rejected for its own reasons, and a second option was superseded |\n"
        )
        r = run({"0001-a.md": CLEAN}, readme=readme)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("1 index row(s)", r.stdout)

    def test_a_bullet_entry_is_refused_now_that_the_index_is_a_table(self):
        readme = (
            "| ADR | What | State |\n|---|---|---|\n"
            "| [0001](0001-a.md) | a decision | **Accepted 2026-01-01** |\n"
            "- **0001** — the same decision, a second time. *Accepted*.\n"
        )
        r = run({"0001-a.md": CLEAN}, readme=readme)
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("indexed as a bullet", r.stdout)

    def test_an_adr_with_no_index_row_fails(self):
        """0011, 0045, 0051 had none, and 0027 had a row pointing elsewhere."""
        readme = "| ADR | What | State |\n|---|---|---|\n"
        r = run({"0001-a.md": CLEAN}, readme=readme)
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("has no index row", r.stdout)

    def test_a_row_for_another_file_is_not_a_row_for_this_one(self):
        # Two ADRs shared the number 0027; the index linked one and the other
        # was invisible. The comparison is by FILE, not by number.
        readme = (
            "| ADR | What | State |\n|---|---|---|\n"
            "| [0001](0001-other.md) | another file | **Accepted 2026-01-01** |\n"
        )
        r = run({"0001-a.md": CLEAN}, readme=readme)
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("0001-a.md has no index row", r.stdout)

    def test_a_status_no_shape_can_read_fails(self):
        body = "# ADR-0002: a decision\n\nIt was decided, somehow.\n\n## Context\n"
        # A second ADR WITH a status, so this does not fall into the older
        # «the gate read nothing» guard, which covers the all-empty case.
        readme = (
            "| ADR | What | State |\n|---|---|---|\n"
            "| [0001](0001-a.md) | a decision | **Accepted 2026-01-01** |\n"
        )
        r = run({"0001-a.md": CLEAN, "0002-b.md": body}, readme=readme)
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("no status line names a state", r.stdout)

    # --- the three status shapes the reader was missing ------------------------
    # Measured 2026-10-08: requiring a list item read 60 of 76 documents and
    # skipped SIXTEEN, silently. Each of these fails with the widened reader
    # reverted, because the document then has no readable status at all.

    def test_a_status_bold_with_the_colon_inside_is_read(self):
        """`**Status: Accepted (2026-08-12).**` — ADR-0013 and ADR-0022."""
        body = "# ADR-0001: a decision\n\n**Status: Accepted (2026-01-01).** And a sentence.\n"
        readme = (
            "| ADR | What | State |\n|---|---|---|\n"
            "| [0001](0001-a.md) | a decision | **Proposed** |\n"
        )
        r = run({"0001-a.md": body}, readme=readme)
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("index is lying", r.stdout)

    def test_a_status_paragraph_without_a_bullet_is_read(self):
        """`**Status:** Accepted 2026-09-15 — …` — ADR-0038."""
        body = "# ADR-0001: a decision\n\n**Status:** Accepted 2026-01-01 — gated on a spike\n"
        readme = (
            "| ADR | What | State |\n|---|---|---|\n"
            "| [0001](0001-a.md) | a decision | **Proposed** |\n"
        )
        r = run({"0001-a.md": body}, readme=readme)
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("index is lying", r.stdout)

    def test_a_status_under_a_heading_is_read(self):
        """`## Status` with the state in the paragraph below — ADR-0048."""
        body = "# ADR-0001: a decision\n\n## Status\n\nProposed 2026-01-01. Scope decided.\n\n## Context\n"
        readme = (
            "| ADR | What | State |\n|---|---|---|\n"
            "| [0001](0001-a.md) | a decision | **Accepted** |\n"
        )
        r = run({"0001-a.md": body}, readme=readme)
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("index is lying", r.stdout)

    def test_a_gate_that_read_nothing_fails(self):
        """A green by absence is the failure mode this repo has paid for twice."""
        r = run({"0005-e.md": "# ADR-0005\n\nNo status line at all.\n"})
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("read nothing", r.stdout)


if __name__ == "__main__":
    unittest.main()
