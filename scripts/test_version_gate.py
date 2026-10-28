#!/usr/bin/env python3
"""Unit tests for `scripts/version_gate.py`'s `evaluate` — stdlib `unittest`, no
new dependency (dev tooling, like the other `test_*.py` here).

`evaluate` is the pure core: given the Cargo version, every `v*` tag, the tags
this commit contains, and whether release notes exist, it returns `(rc, msg)`.
The tests fix its decisions — the ordinary between-releases case, the release
commit, the three legacy failures, and the `delonix-runtime` (v1..v5) → Delonix
(0.x) line re-base, where `0.1.0` must NOT read as older than `v5.1.1`.

Run: python3 scripts/test_version_gate.py
"""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import version_gate as vg  # noqa: E402

LEGACY = ["v1.0.0", "v1.1.0", "v2.0.0", "v3.0.0", "v3.1.0", "v4.0.0", "v4.5.0",
          "v5.0.0", "v5.0.1", "v5.0.2", "v5.1.0", "v5.1.1"]


class LineOf(unittest.TestCase):
    def test_majors_1_through_5_are_the_frozen_runtime_line(self):
        for v in [(1, 0, 0), (4, 2, 0), (5, 1, 1)]:
            self.assertEqual(vg.line_of(v), "delonix-runtime")

    def test_major_0_is_the_delonix_line(self):
        self.assertEqual(vg.line_of((0, 1, 0)), "delonix")


class LegacyLine(unittest.TestCase):
    def test_equal_to_newest_contained_tag_passes(self):
        rc, msg = vg.evaluate("5.1.1", LEGACY, LEGACY, notes_exist=False)
        self.assertEqual(rc, 0, msg)
        self.assertIn("= v5.1.1", msg)

    def test_a_newer_published_tag_not_contained_fails(self):
        contained = [t for t in LEGACY if t != "v5.1.1"]  # commit predates v5.1.1
        rc, msg = vg.evaluate("5.1.0", LEGACY, contained, notes_exist=False)
        self.assertEqual(rc, 1, msg)
        self.assertIn("does not contain it", msg)

    def test_lower_than_the_contained_tag_fails(self):
        rc, msg = vg.evaluate("5.0.0", LEGACY, LEGACY, notes_exist=True)
        self.assertEqual(rc, 1, msg)
        self.assertIn("lower than v5.1.1", msg)

    def test_a_bump_with_notes_is_the_release_commit(self):
        rc, msg = vg.evaluate("5.2.0", LEGACY, LEGACY, notes_exist=True)
        self.assertEqual(rc, 0, msg)
        self.assertIn("release commit", msg)

    def test_a_bump_without_notes_fails(self):
        rc, msg = vg.evaluate("5.2.0", LEGACY, LEGACY, notes_exist=False)
        self.assertEqual(rc, 1, msg)
        self.assertIn("does not exist", msg)


class DelonixLineTransition(unittest.TestCase):
    def test_opening_the_delonix_line_at_0_1_0_with_notes_passes(self):
        # The crux: 0.1.0 with v5.1.1 contained used to fail "lower than v5.1.1".
        rc, msg = vg.evaluate("0.1.0", LEGACY, LEGACY, notes_exist=True)
        self.assertEqual(rc, 0, msg)
        self.assertIn("opens the delonix line at 0.1.0", msg)

    def test_opening_the_delonix_line_without_notes_fails(self):
        rc, msg = vg.evaluate("0.1.0", LEGACY, LEGACY, notes_exist=False)
        self.assertEqual(rc, 1, msg)
        self.assertIn("opening the delonix line", msg)

    def test_a_0_1_0_commit_that_drops_legacy_history_fails(self):
        # Does not contain v5.1.1 -> merging it would undo shipped work.
        contained = [t for t in LEGACY if t != "v5.1.1"]
        rc, msg = vg.evaluate("0.1.0", LEGACY, contained, notes_exist=True)
        self.assertEqual(rc, 1, msg)
        self.assertIn("does not contain it", msg)

    def test_ordinary_work_on_the_delonix_line_after_its_first_tag(self):
        tags = LEGACY + ["v0.1.0"]
        contained = LEGACY + ["v0.1.0"]
        rc, msg = vg.evaluate("0.1.0", tags, contained, notes_exist=True)
        self.assertEqual(rc, 0, msg)
        self.assertIn("= v0.1.0", msg)

    def test_lower_on_the_delonix_line_fails(self):
        tags = LEGACY + ["v0.1.0", "v0.2.0"]
        contained = LEGACY + ["v0.1.0", "v0.2.0"]
        rc, msg = vg.evaluate("0.1.0", tags, contained, notes_exist=True)
        self.assertEqual(rc, 1, msg)
        self.assertIn("lower than v0.2.0 on the delonix line", msg)

    def test_a_delonix_tag_not_contained_fails(self):
        tags = LEGACY + ["v0.1.0"]
        contained = list(LEGACY)  # branch predates v0.1.0
        rc, msg = vg.evaluate("0.1.0", tags, contained, notes_exist=True)
        self.assertEqual(rc, 1, msg)
        self.assertIn("does not contain it", msg)


class Guards(unittest.TestCase):
    def test_no_tags_at_all_refuses(self):
        rc, msg = vg.evaluate("5.1.1", [], [], notes_exist=False)
        self.assertEqual(rc, 1, msg)
        self.assertIn("no v* tags visible", msg)

    def test_a_non_semver_version_fails(self):
        rc, msg = vg.evaluate("5.1", LEGACY, LEGACY, notes_exist=False)
        self.assertEqual(rc, 1, msg)
        self.assertIn("not X.Y.Z", msg)


if __name__ == "__main__":
    unittest.main(verbosity=2)
