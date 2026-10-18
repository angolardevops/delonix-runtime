import tempfile
import unittest
from pathlib import Path

import chaos_skip_gate as g


def allowed_file(*lines):
    d = tempfile.mkdtemp()
    p = Path(d) / "chaos_allowed_skips.txt"
    p.write_text("# a comment\n\n" + "\n".join(lines) + "\n")
    return p


class ChaosSkipGate(unittest.TestCase):
    def test_a_declared_skip_passes(self):
        allowed = {"blocked": "no userns on a hosted runner"}
        self.assertEqual(g.verdict("blocked", "skipped", allowed), [])

    def test_an_undeclared_verdict_fails(self):
        allowed = {"blocked": "no userns on a hosted runner"}
        problems = g.verdict("some-new-verdict", "skipped", allowed)
        self.assertEqual(len(problems), 1)
        self.assertIn("some-new-verdict", problems[0])

    def test_a_skip_while_the_environment_was_fine_fails(self):
        """The case with teeth: the probe could run it, and it was skipped anyway."""
        allowed = {"blocked": "no userns on a hosted runner"}
        problems = g.verdict("ok", "skipped", allowed)
        self.assertEqual(len(problems), 1)
        self.assertIn("not declared", problems[0])

    def test_a_skip_with_no_probe_verdict_fails(self):
        problems = g.verdict("", "skipped", {"blocked": "x"})
        self.assertEqual(len(problems), 1)
        self.assertIn("said nothing", problems[0])

    def test_a_harness_that_ran_is_not_judged(self):
        for result in ("success", "failure", "cancelled"):
            self.assertEqual(g.verdict("ok", result, {}), [], result)

    def test_an_unknown_result_fails(self):
        problems = g.verdict("ok", "neutral", {})
        self.assertEqual(len(problems), 1)
        self.assertIn("unknown result", problems[0])

    def test_the_declared_list_is_parsed_as_verdict_tab_reason(self):
        p = allowed_file("blocked\tno userns here", "weird\treason with  spaces")
        self.assertEqual(
            g.declared(p),
            {"blocked": "no userns here", "weird": "reason with  spaces"},
        )

    def test_removing_a_line_makes_that_verdict_fail(self):
        """The A/B the session has to show: the gate bites when the line is gone."""
        p = allowed_file("blocked\tno userns here")
        self.assertEqual(g.verdict("blocked", "skipped", g.declared(p)), [])
        empty = allowed_file("# nothing declared")
        self.assertEqual(len(g.verdict("blocked", "skipped", g.declared(empty))), 1)

    def test_the_shipped_list_declares_the_hosted_runner(self):
        """The real file: a hosted runner's `blocked` must stay declared."""
        self.assertIn("blocked", g.declared())


if __name__ == "__main__":
    unittest.main(verbosity=2)
