"""Tests for the providers.yaml that install.sh writes (ADR-0054).

The function under test is extracted from `scripts/install.sh` itself and run in
a temporary directory, so these tests exercise the code the installer runs — not
a copy of it — without packages, root or a real host.
"""

import os
import re
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

INSTALL = Path(__file__).resolve().parent / "install.sh"


def extract_function(name: str) -> str:
    text = INSTALL.read_text()
    m = re.search(rf"^{name}\(\) \{{\n.*?^\}}\n", text, re.S | re.M)
    if not m:
        raise AssertionError(f"{name} not found in install.sh")
    return m.group(0)


WRITER = extract_function("write_providers_config")


def write(path: Path, provider: str = "libvirt") -> int:
    script = WRITER + f'\nwrite_providers_config "{path}" "{provider}" ""\n'
    return subprocess.run(["bash", "-c", script], check=False).returncode


class WriteProvidersConfig(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.root = Path(self.dir.name)
        self.path = self.root / "etc" / "delonix" / "providers.yaml"

    def tearDown(self):
        self.dir.cleanup()

    def test_writes_the_default_provider_and_is_world_readable(self):
        self.assertEqual(write(self.path), 0)
        text = self.path.read_text()
        self.assertIn("apiVersion: config.delonix.io/v1\n", text)
        self.assertIn("defaultProvider: libvirt\n", text)
        self.assertIn("  - type: libvirt\n", text)
        mode = stat.S_IMODE(self.path.stat().st_mode)
        self.assertEqual(mode, 0o644)

    def test_the_chosen_provider_is_the_one_written(self):
        self.assertEqual(write(self.path, "cloud-hypervisor"), 0)
        self.assertIn("defaultProvider: cloud-hypervisor\n", self.path.read_text())

    def test_a_second_run_leaves_an_edited_file_byte_equal(self):
        self.assertEqual(write(self.path), 0)
        edited = self.path.read_text() + "  - type: proxmox\n    url: https://pve:8006\n"
        self.path.write_text(edited)
        before = self.path.read_bytes()
        self.assertEqual(write(self.path, "cloud-hypervisor"), 3)
        self.assertEqual(self.path.read_bytes(), before)

    def test_an_existing_symlink_is_neither_followed_nor_replaced(self):
        target = self.root / "elsewhere.yaml"
        target.write_text("not ours\n")
        self.path.parent.mkdir(parents=True)
        self.path.symlink_to(target)
        self.assertEqual(write(self.path), 3)
        self.assertEqual(target.read_text(), "not ours\n")
        self.assertTrue(self.path.is_symlink())

    def test_a_dangling_symlink_is_not_written_through(self):
        target = self.root / "would-be-created.yaml"
        self.path.parent.mkdir(parents=True)
        self.path.symlink_to(target)
        self.assertEqual(write(self.path), 3)
        self.assertFalse(target.exists())


class VmProviderFlag(unittest.TestCase):
    def run_install(self, *args):
        # The value is refused right after argument parsing, before any check
        # that touches the host, so this is safe to run anywhere.
        return subprocess.run(
            ["bash", str(INSTALL), *args],
            capture_output=True,
            text=True,
            check=False,
            env={**os.environ, "NO_COLOR": "1"},
        )

    def test_an_unknown_provider_is_refused_before_anything_runs(self):
        r = self.run_install("--vm-provider", "hyperv")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("--vm-provider must be libvirt or cloud-hypervisor", r.stderr)
        self.assertNotIn("[host]", r.stdout)

    def test_the_flag_needs_a_value(self):
        r = self.run_install("--vm-provider")
        self.assertNotEqual(r.returncode, 0)


if __name__ == "__main__":
    unittest.main()
