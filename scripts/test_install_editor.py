"""Tests for the editor-extension step of install.sh.

The reported symptom was «installing/updating the .vsix opens the editor and
the delonix-runtime install hangs». A launcher that does not recognise a CLI
argument opens a WINDOW with it and stays in the foreground, and the installer
waits for that process — a convenience holding the engine install hostage.

`editor_cli` is the one door every editor invocation goes through, so it is
extracted from `scripts/install.sh` itself and exercised here against fake
editors: these tests run the code the installer runs, not a copy of it, and
need no editor, no network and no root.
"""

import os
import re
import subprocess
import tempfile
import textwrap
import time
import unittest
from pathlib import Path

INSTALL = Path(__file__).resolve().parent / "install.sh"
TEXT = INSTALL.read_text()


def function_source(name: str) -> str:
    """The function as written in install.sh (indented), or a clear failure.

    A missing `editor_cli` is itself the regression this file guards: without
    it every editor call is unbounded again.
    """
    m = re.search(rf"^(?P<i> *){name}\(\) \{{\n.*?^(?P=i)\}}\n", TEXT, re.S | re.M)
    if not m:
        raise AssertionError(
            f"{name}() is gone from install.sh — every editor invocation must go "
            "through it, or a launcher that opens a window hangs the install"
        )
    return m.group(0)


def run_editor_cli(fake: str, timeout_s: str = "2", stdin_text: str = "", wait: int = 30):
    """Run `editor_cli <fake> <out>` with EDITOR_TIMEOUT set, and time it."""
    with tempfile.TemporaryDirectory() as d:
        ed = Path(d) / "fake-editor"
        ed.write_text("#!/bin/sh\n" + fake)
        ed.chmod(0o755)
        out = Path(d) / "out.txt"
        script = (
            "set -euo pipefail\n"
            f"EDITOR_TIMEOUT={timeout_s}\n"
            + textwrap.dedent(function_source("editor_cli"))
            + f'rc=0; editor_cli "{ed}" "{out}" --version || rc=$?\n'
            'printf "rc=%s\\n" "$rc"\n'
        )
        start = time.monotonic()
        p = subprocess.run(
            ["bash", "-c", script],
            input=stdin_text,
            capture_output=True,
            text=True,
            timeout=wait,
        )
        elapsed = time.monotonic() - start
        rc = int(re.search(r"rc=(\d+)", p.stdout).group(1))
        return rc, elapsed, out.read_text()


class EditorCli(unittest.TestCase):
    def test_an_editor_that_never_returns_is_given_up_on(self):
        # The hang itself: a launcher that opened a window and waits for it.
        rc, elapsed, _ = run_editor_cli("sleep 60\n", timeout_s="2")
        self.assertEqual(rc, 124, "a hanging editor must be given up on, not waited for")
        self.assertLess(elapsed, 20, f"gave up only after {elapsed:.1f}s")

    def test_the_editors_stdin_is_closed_so_it_cannot_eat_the_script(self):
        # `curl … | bash` puts the script on stdin, and the VS Code launcher
        # reads from it (`read -r YN`) on WSL.
        rc, _, out = run_editor_cli(
            'if read -r line; then echo "ATE:$line"; else echo "EOF"; fi\n',
            stdin_text="THE-NEXT-LINE-OF-THE-SCRIPT\n",
        )
        self.assertEqual(rc, 0)
        self.assertIn("EOF", out)
        self.assertNotIn("ATE:", out, "the editor read from the installer's stdin")

    def test_both_streams_are_captured_so_a_failure_can_be_quoted(self):
        rc, _, out = run_editor_cli('echo to-stdout; echo to-stderr >&2; exit 9\n')
        self.assertEqual(rc, 9)
        self.assertIn("to-stdout", out)
        self.assertIn("to-stderr", out)

    def test_a_well_behaved_editor_is_not_slowed_down(self):
        rc, elapsed, out = run_editor_cli("echo 1.0.0\n", timeout_s="120")
        self.assertEqual(rc, 0)
        self.assertIn("1.0.0", out)
        self.assertLess(elapsed, 10)


class EveryEditorCallGoesThroughIt(unittest.TestCase):
    """A ceiling only holds while nothing walks around it."""

    def test_no_editor_is_invoked_outside_editor_cli(self):
        block = re.search(
            r"# ---- Extensão de editor.*?\n(?=  ACTIVE=)", TEXT, re.S
        )
        self.assertIsNotNone(block, "editor block not found in install.sh")
        body = block.group(0).replace(function_source("editor_cli"), "")
        # An invocation is `"$ed"` in COMMAND position; `"$ed"` as an argument
        # of spin/warn/stepok is just the editor's name being printed.
        as_command = re.compile(
            r'(^|[;&|(]|\$\(|\b(if|then|else|elif|do|while|until|!)\b)\s*"\$ed"'
        )
        offenders = [
            line.strip()
            for line in body.splitlines()
            if as_command.search(line) and not line.lstrip().startswith("#")
        ]
        self.assertEqual(offenders, [], "editor invoked without the time ceiling")

    def test_the_timeout_is_adjustable_and_has_a_default(self):
        self.assertIn("EDITOR_TIMEOUT=${DELONIX_EDITOR_TIMEOUT:-", TEXT)

    def test_a_timed_out_editor_is_reported_as_such(self):
        # 124 is `timeout`'s verdict, 137 the SIGKILL of `-k`. Both must be
        # told apart from a real failure, or the message sends people looking
        # for a broken editor when the editor simply opened a window.
        self.assertGreaterEqual(len(re.findall(r"124\|137\)", TEXT)), 2)


if __name__ == "__main__":
    unittest.main()
