#!/usr/bin/env python3
"""Unit tests for `scripts/gitflow.py` — stdlib `unittest`, no new dependency
(same reasoning as `test_tmp_roots_gate.py`); discovered by the CI
`script-tests` job.

Each test fixes a decision of the engineering-process harness: a fix is
classified above a feature, every prefix maps to the right base, a bad prefix
is refused, the worktree-list parser is exact, `--dry-run` mutates nothing, and
the destructive paths refuse to lose work.

Run: python3 scripts/test_gitflow.py
"""
import io
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gitflow as g  # noqa: E402


def _git(args, cwd):
    subprocess.run(["git", *args], cwd=cwd, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def _capture(argv):
    out = io.StringIO()
    with redirect_stdout(out):
        rc = g.main(argv)
    return rc, out.getvalue()


class Classification(unittest.TestCase):
    def test_fix_beats_feature(self):
        rc, out = _capture(["classify", "add", "a", "fix", "for", "the", "crash"])
        self.assertEqual(rc, 0)
        self.assertIn("prefix: fix", out)
        self.assertIn("origin/main", out)

    def test_plain_feature_defaults_to_develop(self):
        rc, out = _capture(["classify", "support", "a", "new", "manifest", "kind"])
        self.assertEqual(rc, 0)
        self.assertIn("prefix: feat", out)
        self.assertIn("origin/develop", out)

    def test_docs_goes_to_develop(self):
        rc, out = _capture(["classify", "document", "the", "handbook"])
        self.assertIn("prefix: docs", out)
        self.assertIn("origin/develop", out)


class BaseMapping(unittest.TestCase):
    def test_every_prefix_has_a_base(self):
        for prefix, base in g.PREFIX_BASE.items():
            self.assertIn(base, (g.INTEGRATION, g.RELEASED))

    def test_fix_family_from_main(self):
        for b in ("fix/x", "bug/y", "hotfix/z"):
            self.assertEqual(g.base_for_branch(b), g.RELEASED)

    def test_feature_family_from_develop(self):
        for b in ("feat/x", "chore/y", "docs/z", "ci/w", "refactor/r"):
            self.assertEqual(g.base_for_branch(b), g.INTEGRATION)

    def test_unknown_prefix_refused(self):
        with self.assertRaises(g.GitFlowError):
            g.base_for_branch("wip/nope")


class WorktreeParser(unittest.TestCase):
    def test_parses_and_filters(self):
        # Build a temp git repo with develop + a task worktree.
        with tempfile.TemporaryDirectory() as td:
            root = Path(td) / "repo"
            root.mkdir()
            _git(["init", "-q", "-b", "main"], root)
            _git(["config", "user.email", "t@example.com"], root)
            _git(["config", "user.name", "t"], root)
            (root / "f").write_text("x")
            _git(["add", "f"], root)
            _git(["commit", "-qm", "init"], root)
            _git(["branch", "develop"], root)
            wt = Path(td) / "wt-task"
            _git(["worktree", "add", "-q", "-b", "feat/task", str(wt), "develop"], root)

            allwt = g.list_worktrees(cwd=root)
            branches = {b for _, b in allwt}
            self.assertIn("main", branches)
            self.assertIn("feat/task", branches)

            tasks = g.task_worktrees(cwd=root)
            task_branches = {b for _, b in tasks}
            self.assertEqual(task_branches, {"feat/task"})  # main/develop excluded


class DryRunAndSafety(unittest.TestCase):
    def setUp(self):
        self._td = tempfile.TemporaryDirectory()
        td = Path(self._td.name)
        self.root = td / "repo"
        self.root.mkdir()
        _git(["init", "-q", "-b", "main"], self.root)
        _git(["config", "user.email", "t@example.com"], self.root)
        _git(["config", "user.name", "t"], self.root)
        # a fake origin so validate_remote passes
        _git(["remote", "add", "origin",
              "https://example.com/angolardevops/delonix-runtime.git"], self.root)
        (self.root / "f").write_text("x")
        _git(["add", "f"], self.root)
        _git(["commit", "-qm", "init"], self.root)
        _git(["branch", "develop"], self.root)
        self._cwd = os.getcwd()
        os.chdir(self.root)
        os.environ["DELONIX_WORKTREES_DIR"] = str(td / ".worktrees")

    def tearDown(self):
        os.chdir(self._cwd)
        os.environ.pop("DELONIX_WORKTREES_DIR", None)
        self._td.cleanup()

    def test_start_dry_run_creates_nothing(self):
        before = g.list_worktrees(cwd=self.root)
        rc, out = _capture(["start", "chore/x", "--dry-run"])
        self.assertEqual(rc, 0)
        self.assertIn("DRY-RUN", out)
        self.assertEqual(before, g.list_worktrees(cwd=self.root))

    def test_validate_remote_rejects_wrong_repo(self):
        _git(["remote", "set-url", "origin", "https://example.com/someone/other.git"], self.root)
        with self.assertRaises(g.GitFlowError):
            g.validate_remote(cwd=self.root)

    def _mk_worktree(self, branch, base, *, commit=False):
        wt = Path(self._td.name) / f"wt-{branch.replace('/', '-')}"
        _git(["worktree", "add", "-q", "-b", branch, str(wt), base], self.root)
        if commit:
            (wt / "g").write_text("new")
            _git(["add", "g"], wt)
            _git(["commit", "-qm", "work"], wt)
        return wt

    def _cleanup_args(self, branch, **kw):
        import argparse
        base = dict(branch=branch, remote=False, execute=True, dry_run=False,
                    force_unmerged=False)
        base.update(kw)
        return argparse.Namespace(**base)

    def test_cleanup_fails_closed_without_integration_ref(self):
        # The data-loss bug the reviewer found: no local develop and no
        # origin/develop must make cleanup REFUSE, not sail past the guard.
        self._mk_worktree("feat/noref", "main", commit=True)
        _git(["branch", "-D", "develop"], self.root)  # remove the integration ref
        with self.assertRaises(g.GitFlowError):
            g.cmd_cleanup(self._cleanup_args("feat/noref"))
        self.assertIn("feat/noref", {b for _, b in g.list_worktrees(cwd=self.root)})

    def test_pending_commits_raises_on_bad_target(self):
        self._mk_worktree("feat/bt", "develop")
        with self.assertRaises(g.GitFlowError):
            g.pending_commits(self.root, "feat/bt", target="origin/does-not-exist")

    def test_cleanup_noexecute_on_integrated_leaves_worktree(self):
        # A branch with no commits beyond develop is integrated; cleanup without
        # --execute prints a plan and removes nothing.
        self._mk_worktree("chore/clean", "develop")
        rc = g.cmd_cleanup(self._cleanup_args("chore/clean", execute=False))
        self.assertEqual(rc, 0)
        self.assertIn("chore/clean", {b for _, b in g.list_worktrees(cwd=self.root)})

    def test_cleanup_refuses_unmerged(self):
        # a branch with a commit not in develop must not be removable
        _git(["worktree", "add", "-q", "-b", "feat/unmerged",
              str(Path(self._td.name) / "wt-u"), "develop"], self.root)
        wt = Path(self._td.name) / "wt-u"
        (wt / "g").write_text("new")
        _git(["add", "g"], wt)
        _git(["commit", "-qm", "work"], wt)
        import argparse
        args = argparse.Namespace(branch="feat/unmerged", remote=False,
                                  execute=True, dry_run=False, force_unmerged=False)
        with self.assertRaises(g.GitFlowError):
            g.cmd_cleanup(args)
        # worktree + branch still there
        self.assertIn("feat/unmerged", {b for _, b in g.list_worktrees(cwd=self.root)})


if __name__ == "__main__":
    unittest.main(verbosity=2)
