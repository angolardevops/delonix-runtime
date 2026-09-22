#!/usr/bin/env python3
"""Unit tests for `scripts/release_verify.py` — stdlib `unittest`, zero new
dependency (the same reasoning `sbom.py`'s own doc gives for not reaching for
a library: the script is dev tooling, not something this engine ships).

Run: python3 scripts/test_release_verify.py
"""
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release_verify as rv  # noqa: E402


class EvidencePathsTests(unittest.TestCase):
    def test_extracts_a_scripts_reference(self):
        self.assertEqual(
            rv.evidence_paths("see scripts/e2e.sh for the checks"),
            ["scripts/e2e.sh"],
        )

    def test_extracts_a_docs_reference(self):
        self.assertEqual(
            rv.evidence_paths("pinned in docs/cli-stability.md since v0.49.0"),
            ["docs/cli-stability.md"],
        )

    def test_extracts_agents_md_bare(self):
        self.assertIn("AGENTS.md", rv.evidence_paths("validated live (AGENTS.md, v0.40.0)"))

    def test_extracts_multiple_references_in_one_sentence(self):
        found = rv.evidence_paths(
            "docs/schema/v1/delonix.json IS the generated one; scripts/schema-diff.sh compares"
        )
        self.assertEqual(
            sorted(found), sorted(["docs/schema/v1/delonix.json", "scripts/schema-diff.sh"])
        )

    def test_ignores_prose_that_is_not_a_checkable_path(self):
        # "cmd/exitcode.rs" and a bare "e2e.sh" (no `scripts/` prefix) are
        # real prose this table uses, and deliberately NOT matched — see the
        # module doc's "not a bare \S+/\S+" note. A false rot report on prose
        # nobody meant as a path would be worse than not checking it.
        found = rv.evidence_paths(
            "cmd/exitcode.rs maps from the error TYPE; e2e.sh asserts numeric classes"
        )
        self.assertEqual(found, [])

    def test_ignores_a_crate_path_with_no_test_file(self):
        self.assertEqual(rv.evidence_paths("8 chaos scenarios kill the holder"), [])


class CheckTests(unittest.TestCase):
    """`check()` reads ROOT at call time (module-level global) — patched per
    test rather than passed as a parameter, matching how the script is
    actually invoked (a single process, one repository root)."""

    def _features(self, evidence: str, level: str = "stable"):
        return [{"name": "widget", "level": level, "evidence": evidence}]

    def test_passes_when_every_cited_path_exists(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "scripts").mkdir()
            (root / "scripts" / "e2e.sh").write_text("")
            with mock.patch.object(rv, "ROOT", root), mock.patch.object(
                rv, "features", return_value=self._features("see scripts/e2e.sh")
            ):
                self.assertEqual(rv.check(Path("unused")), 0)

    def test_fails_when_a_cited_path_is_missing(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            with mock.patch.object(rv, "ROOT", root), mock.patch.object(
                rv, "features", return_value=self._features("see scripts/missing.sh")
            ):
                self.assertEqual(rv.check(Path("unused")), 1)

    def test_a_preview_level_is_exempt_even_when_its_evidence_is_rotted(self):
        # Below `stable` on purpose — see the module doc: holding
        # experimental/preview evidence to the same bar just teaches people
        # to write vaguer evidence to dodge the gate.
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            with mock.patch.object(rv, "ROOT", root), mock.patch.object(
                rv,
                "features",
                return_value=self._features("scripts/missing.sh", level="preview"),
            ):
                self.assertEqual(rv.check(Path("unused")), 0)

    def test_multiple_features_each_contribute_their_own_rot(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            feats = [
                {"name": "a", "level": "stable", "evidence": "scripts/missing-a.sh"},
                {"name": "b", "level": "production-ready", "evidence": "scripts/missing-b.sh"},
            ]
            with mock.patch.object(rv, "ROOT", root), mock.patch.object(
                rv, "features", return_value=feats
            ):
                self.assertEqual(rv.check(Path("unused")), 1)


if __name__ == "__main__":
    unittest.main()
