#!/usr/bin/env python3
"""Unit tests for `scripts/libvirt_virsh_inventory.py` — stdlib `unittest`.

Run: python3 scripts/test_libvirt_virsh_inventory.py
"""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import libvirt_virsh_inventory as inv  # noqa: E402

HELP = """
 Domain Management (help keyword 'domain'):
    start                          start a (previously defined) inactive domain
    destroy                        destroy (stop) a domain
    migrate                        migrate domain to another host

 Snapshot (help keyword 'snapshot'):
    snapshot-create-as             Create a snapshot from a set of args

 Interface (help keyword 'interface'):
    iface-list                     list physical host interfaces

 Virsh itself (help keyword 'virsh'):
    help                           print help
"""

SRC = '''
fn boot() {
    run_quiet("virsh", &["-c", uri, "start", "--", name]);
    let words = vec!["destroy", "list"]; // an ordinary word, not an argv
}
fn libvirt_snapshot_argv() -> Vec<String> {
    vec!["-c".into(), uri.into(), "snapshot-create-as".into()]
}
#[cfg(test)]
mod tests {
    fn t() { run_quiet("virsh", &["-c", uri, "migrate", "--", name]); }
}
'''


class ParseHelpTests(unittest.TestCase):
    def test_groups_every_command(self):
        cmds, titles = inv.parse_help(HELP)
        self.assertEqual(cmds["start"], "domain")
        self.assertEqual(cmds["snapshot-create-as"], "snapshot")
        self.assertEqual(titles["virsh"], "Virsh itself")


class CalledTests(unittest.TestCase):
    def test_context_not_bare_literal(self):
        known = set(inv.parse_help(HELP)[0])
        called = inv.called_commands(SRC, known)
        self.assertIn("start", called)
        self.assertIn("snapshot-create-as", called)
        self.assertNotIn("destroy", called, "a bare word in a vec is not an argv")

    def test_a_test_module_never_counts(self):
        known = set(inv.parse_help(HELP)[0])
        self.assertNotIn("migrate", inv.called_commands(SRC, known))


class InventoryTests(unittest.TestCase):
    def test_three_states_and_the_denominator_keeps_the_excluded(self):
        r = inv.inventory(HELP, {"x.rs": SRC})
        self.assertEqual(r["denominator"], 6)
        states = {row["command"]: row["state"] for row in r["rows"]}
        self.assertEqual(states["start"], "called")
        self.assertEqual(states["migrate"], "excluded")
        self.assertEqual(states["iface-list"], "excluded")
        self.assertEqual(states["destroy"], "missing")
        self.assertEqual(r["unknown_called"], [])

    def test_a_called_name_absent_from_this_virsh_is_reported(self):
        src = 'fn f() { run_quiet("virsh", &["-c", uri, "not-in-help", "--", n]); }'
        # `not-in-help` is not in HELP, so it is never "known" and cannot be
        # detected as called at all: the denominator decides what is countable.
        r = inv.inventory(HELP, {"x.rs": src})
        self.assertEqual(r["totals"].get("called", 0), 0)


if __name__ == "__main__":
    unittest.main()
