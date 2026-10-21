"""Tests for the release link install.sh prints after a successful install.

The reported symptom: on a default install (no `--version`) the installer
printed `https://github.com/<repo>/releases/tag/latest` first, and that page
is a 404 — `latest` is not a tag. Measured with curl on 2026-10-21.

`release_page_url` is extracted from `scripts/install.sh` itself and run
against fake binaries, so these tests exercise the code the installer runs,
not a copy of it, and need no network and no root.
"""

import re
import subprocess
import tempfile
import unittest
from pathlib import Path

INSTALL = Path(__file__).resolve().parent / "install.sh"
TEXT = INSTALL.read_text()
REPO = "angolardevops/delonix-runtime"


def function_source(name: str) -> str:
    m = re.search(rf"^{name}\(\) {{.*?\n}}\n", TEXT, re.S | re.M)
    if not m:
        raise AssertionError(
            f"{name}() is gone from install.sh — the final release link has to "
            "come from it, or a default install prints releases/tag/latest again"
        )
    return m.group(0)


def release_page_url(version: str, bin_script: str | None) -> str:
    """Run `release_page_url` with a fake installed binary (None = absent)."""
    with tempfile.TemporaryDirectory() as d:
        binary = Path(d) / "delonix"
        if bin_script is not None:
            binary.write_text("#!/bin/sh\n" + bin_script)
            binary.chmod(0o755)
        script = (
            "set -euo pipefail\n"
            + function_source("release_page_url")
            + f'release_page_url "{REPO}" "{version}" "{binary}"\n'
        )
        p = subprocess.run(
            ["bash", "-c", script], capture_output=True, text=True, timeout=30
        )
        if p.returncode != 0:
            raise AssertionError(f"release_page_url failed rc={p.returncode}: {p.stderr}")
        return p.stdout.strip()


class ReleasePageUrl(unittest.TestCase):
    def test_an_explicit_version_names_its_own_tag(self):
        self.assertEqual(
            release_page_url("v5.1.1", None),
            f"https://github.com/{REPO}/releases/tag/v5.1.1",
        )

    def test_latest_names_the_release_the_binary_reports(self):
        out = release_page_url(
            "latest",
            'echo "delonix 5.1.1"\necho "commit: a0da90f50 · built: 2026-10-15"\n',
        )
        self.assertEqual(out, f"https://github.com/{REPO}/releases/tag/v5.1.1")

    def test_latest_never_prints_a_tag_called_latest(self):
        for bin_script in (None, "exit 1\n", 'echo "no version here"\n'):
            out = release_page_url("latest", bin_script)
            self.assertNotIn("releases/tag/latest", out, bin_script)
            self.assertEqual(out, f"https://github.com/{REPO}/releases/latest", bin_script)

    def test_one_link_is_printed(self):
        out = release_page_url("latest", 'echo "delonix 5.1.1"\n')
        self.assertEqual(len(out.splitlines()), 1, out)


class FinalBlock(unittest.TestCase):
    def test_the_success_block_prints_the_link_through_the_function(self):
        block = TEXT[TEXT.index('msg "downloads and documentation:"'):]
        block = block[: block.index("\nelse\n")]
        self.assertIn("release_page_url", block)
        self.assertNotIn("releases/tag/$VERSION", block)


if __name__ == "__main__":
    unittest.main()
