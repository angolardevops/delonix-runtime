#!/usr/bin/env python3
"""Tests for scripts/lab_budget.sh: each limit refuses on its own, and a host with room says go."""
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("lab_budget.sh")


class LabBudget(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.meminfo = self.dir / "meminfo"
        self.loadavg = self.dir / "loadavg"

    def tearDown(self):
        self.tmp.cleanup()

    def run_budget(self, mem_gib=16, load="1.00", args=(), **env):
        self.meminfo.write_text(f"MemTotal: 33554432 kB\nMemAvailable: {mem_gib * 1024 * 1024} kB\n")
        self.loadavg.write_text(f"{load} 1.00 1.00 1/100 1\n")
        e = dict(os.environ)
        e.update(
            LAB_MEMINFO=str(self.meminfo),
            LAB_LOADAVG=str(self.loadavg),
            LAB_DISK_PATH=str(self.dir),
            LAB_NPROC="32",
            # The temp dir's real free space is unknown here; tests that do
            # not exercise the disk limit set its threshold to zero.
            LAB_MIN_FREE_DISK_GIB="0",
        )
        e.update(env)
        return subprocess.run(["bash", str(SCRIPT), *args], env=e, capture_output=True, text=True)

    def test_a_host_with_room_says_go(self):
        r = self.run_budget()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("lab_budget: go", r.stdout)
        self.assertIn("16 GiB available", r.stdout)

    def test_too_little_ram_after_the_guest_refuses(self):
        r = self.run_budget(mem_gib=7)
        self.assertEqual(r.returncode, 3)
        self.assertIn("leaves less than 4 GiB", r.stderr)

    def test_a_loaded_host_refuses(self):
        r = self.run_budget(load="30.5")
        self.assertEqual(r.returncode, 3)
        self.assertIn("load 30.5 is not below 24", r.stderr)

    def test_a_guest_over_the_vcpu_budget_refuses(self):
        r = self.run_budget(args=("--vcpus", "8"))
        self.assertEqual(r.returncode, 3)
        self.assertIn("asks 8 vCPUs", r.stderr)

    def test_a_guest_over_the_ram_budget_refuses(self):
        r = self.run_budget(args=("--ram", "16"))
        self.assertEqual(r.returncode, 3)
        self.assertIn("asks 16 GiB", r.stderr)

    def test_too_little_disk_refuses(self):
        r = self.run_budget(LAB_MIN_FREE_DISK_GIB="999999")
        self.assertEqual(r.returncode, 3)
        self.assertIn("free on disk", r.stderr)

    def test_every_shortage_is_named_not_just_the_first(self):
        r = self.run_budget(mem_gib=2, load="99", LAB_MIN_FREE_DISK_GIB="999999")
        self.assertEqual(r.returncode, 3)
        self.assertEqual(r.stderr.count("REFUSED"), 3, r.stderr)

    def test_a_bad_argument_is_a_usage_error(self):
        r = self.run_budget(args=("--ram", "lots"))
        self.assertEqual(r.returncode, 2)


if __name__ == "__main__":
    unittest.main()
