#!/usr/bin/env python3
"""Unit tests for the `[workspace.package]` and the silent-test-skip rules of
`scripts/arch_fitness.py` —
stdlib `unittest`, no new dependency (same reasoning as `test_release_verify.py`).

The rule exists because of what the gate did NOT catch: `delonix-mgmt` said
`version = "0.1.0"` for two and a half months while the workspace walked to
4.5.0 (#704), and `inline_versions` only ever looked at DEPENDENCY versions.
These tests fix the decisions that make the rule worth having: a hand-written
value fails whichever key it is, `.workspace = true` passes, an OMITTED key is
not a failure, a member that writes a key the root does NOT declare is not this
gate's business, and a root with no `[workspace.package]` fails instead of
passing over every manifest.

The gate itself reads the real manifests through `cargo metadata`; these tests
feed it temporary ones, so they need no toolchain.

Run: python3 scripts/test_arch_fitness.py
"""
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import arch_fitness as g  # noqa: E402

SHARED = ["edition", "license", "repository", "version"]


class WorkspacePackageFields(unittest.TestCase):
    def setUp(self):
        self._td = tempfile.TemporaryDirectory()
        self.tmp = Path(self._td.name)

    def tearDown(self):
        self._td.cleanup()

    def pkgs(self, **manifests: str) -> dict[str, dict]:
        """A fake `cargo metadata` over manifests written to a temp directory."""
        out = {}
        for name, body in manifests.items():
            crate = self.tmp / name
            crate.mkdir(exist_ok=True)
            manifest = crate / "Cargo.toml"
            manifest.write_text(body, encoding="utf-8")
            out[name] = {"manifest_path": str(manifest)}
        return out

    def test_a_hand_written_value_fails_for_every_shared_key(self):
        for key, value in (
            ("version", '"0.1.0"'),
            ("edition", '"2021"'),
            ("license", '"Apache-2.0"'),
            ("repository", '"https://example.invalid/x"'),
        ):
            with self.subTest(key=key):
                pkgs = self.pkgs(crate=f'[package]\nname = "crate"\n{key} = {value}\n')
                (only,) = g.inline_package_fields(pkgs, SHARED)
                self.assertIn(f"[package] {key} =", only)
                self.assertIn(f"`{key}.workspace = true`", only, "the failure has to say the fix")

    def test_a_value_that_matches_the_root_today_still_fails(self):
        """The drift is silent by construction: a copy is the root's value FROZEN.

        `delonix-mgmt` was born with `version = "0.1.0"` on the very day the
        workspace was at 0.1.0 — passing it because it happens to agree is how
        the next two and a half months of drift start.
        """
        pkgs = self.pkgs(crate='[package]\nname = "crate"\nedition = "2021"\n')
        self.assertTrue(g.inline_package_fields(pkgs, ["edition"]))

    def test_workspace_true_passes(self):
        body = (
            '[package]\nname = "crate"\nversion.workspace = true\n'
            "edition.workspace = true\nlicense.workspace = true\nrepository.workspace = true\n"
        )
        self.assertEqual(g.inline_package_fields(self.pkgs(crate=body), SHARED), [])

    def test_an_omitted_key_is_not_a_failure(self):
        """9 of the 29 crates do not declare `repository`, and that stays fine.

        The rule is about WHERE a value lives, not about forcing every manifest
        to carry every field.
        """
        pkgs = self.pkgs(crate='[package]\nname = "crate"\nversion.workspace = true\n')
        self.assertEqual(g.inline_package_fields(pkgs, SHARED), [])

    def test_a_key_the_root_does_not_share_is_not_this_gates_business(self):
        pkgs = self.pkgs(crate='[package]\nname = "crate"\ndescription = "mine alone"\n')
        self.assertEqual(g.inline_package_fields(pkgs, SHARED), [])

    def test_every_member_is_looked_at_not_just_the_first(self):
        pkgs = self.pkgs(
            good='[package]\nname = "good"\nversion.workspace = true\n',
            bad='[package]\nname = "bad"\nversion = "9.9.9"\n',
        )
        (only,) = g.inline_package_fields(pkgs, SHARED)
        self.assertTrue(only.startswith("bad: "), only)

    def test_a_root_with_no_shared_keys_fails_instead_of_guarding_nothing(self):
        pkgs = self.pkgs(crate='[package]\nname = "crate"\nversion = "0.1.0"\n')
        (only,) = g.inline_package_fields(pkgs, [])
        self.assertIn("guards nothing", only)

    def test_the_real_root_still_declares_the_four_keys(self):
        """If the root stops sharing one, this gate quietly stops guarding it."""
        self.assertEqual(g.shared_package_keys(), SHARED)


class SilentTestSkips(unittest.TestCase):
    """The B-66 rule: an integration test that returns is a `passed` that ran nothing.

    Measured before the rule: 33 early returns in 23 live tests (Proxmox,
    PowerDNS, libvirt, Cloud Hypervisor, the VM providers, the cgroup parent),
    each reported as `passed` by every `cargo test --workspace` that had no
    target. The fix is `#[ignore = "needs ..."]` plus a failure naming what is
    missing; these tests fix what the gate must and must not count.
    """

    def setUp(self):
        self._td = tempfile.TemporaryDirectory()
        self.root = Path(self._td.name)

    def tearDown(self):
        self._td.cleanup()

    def judge(self, body: str, name: str = "live.rs") -> list[str]:
        f = self.root / "crates" / "x" / "tests" / name
        f.parent.mkdir(parents=True, exist_ok=True)
        f.write_text(body, encoding="utf-8")
        return g.silent_test_skips([f], root=self.root)

    def test_a_let_else_return_on_a_missing_variable_fails(self):
        (only,) = self.judge(
            '#[ignore = "needs X: cargo test -- --ignored"]\n#[test]\nfn t() {\n'
            '    let Ok(url) = std::env::var("X") else {\n        return;\n    };\n'
            "    assert!(!url.is_empty());\n}\n"
        )
        self.assertIn("crates/x/tests/live.rs:3: t has 1 early return", only)

    def test_an_early_ok_return_fails_too(self):
        (only,) = self.judge(
            "#[test]\nfn t() -> Result<(), String> {\n    if true {\n        return Ok(());\n    }\n"
            "    Ok(())\n}\n",
            name="other.rs",
        )
        self.assertIn("early return", only)

    def test_a_failure_naming_the_variable_passes(self):
        self.assertEqual(
            self.judge(
                '#[test]\n#[ignore = "needs X: cargo test -- --ignored"]\nfn t() {\n'
                '    let url = std::env::var("X").expect("X must be set");\n'
                "    assert!(!url.is_empty());\n}\n"
            ),
            [],
        )

    def test_a_return_outside_a_test_body_is_not_counted(self):
        """A `Drop` or a helper may return; only a TEST that returns passes empty."""
        self.assertEqual(
            self.judge(
                "struct G(bool);\nimpl Drop for G {\n    fn drop(&mut self) {\n"
                "        if !self.0 {\n            return;\n        }\n    }\n}\n"
                "fn helper() -> Option<u8> {\n    return None;\n}\n"
                '#[ignore = "needs X: cmd"]\n#[test]\nfn t() {\n    let _ = helper();\n}\n'
            ),
            [],
        )

    def test_a_return_in_a_comment_or_a_string_is_not_counted(self):
        self.assertEqual(
            self.judge(
                '#[ignore = "needs X: cmd"]\n#[test]\nfn t() {\n'
                "    // return; was the old shape\n"
                '    let s = "} return; {";\n'
                '    let r = r#"return;"#;\n'
                "    let c = '}';\n"
                "    /* return; */\n"
                "    assert!(!s.is_empty() && !r.is_empty() && c == '}');\n}\n"
            ),
            [],
        )

    def test_the_body_ends_at_its_own_closing_brace(self):
        """A test after the bad one is judged on its own, and a good one is not blamed."""
        problems = self.judge(
            '#[ignore = "needs X: cmd"]\n#[test]\nfn good() {\n    if true {\n        let _ = 1;\n    }\n}\n'
            '#[ignore = "needs X: cmd"]\n#[test]\nfn bad() {\n    return;\n}\n'
        )
        self.assertEqual(len(problems), 1)
        self.assertIn(": bad has 1 early return", problems[0])

    def test_a_bare_ignore_in_a_live_file_fails(self):
        (only,) = self.judge("#[test]\n#[ignore]\nfn t() {\n    assert!(true);\n}\n")
        self.assertIn("t is #[ignore] without a reason", only)

    def test_an_empty_reason_is_no_reason(self):
        (only,) = self.judge('#[ignore = " "]\n#[test]\nfn t() {\n    assert!(true);\n}\n')
        self.assertIn("without a reason", only)

    def test_a_bare_ignore_outside_a_live_file_is_not_this_rules_business(self):
        self.assertEqual(
            self.judge("#[test]\n#[ignore]\nfn t() {\n    assert!(true);\n}\n", name="slow.rs"),
            [],
        )

    def test_the_gate_reads_the_live_files_of_the_real_tree(self):
        """If the walk stops finding them, the rule guards nothing and reads as green."""
        found = {f.relative_to(g.ROOT).as_posix() for f in g.integration_test_files()}
        for live in (
            "crates/providers/delonix-proxmox/tests/live.rs",
            "crates/providers/delonix-powerdns/tests/live.rs",
            "crates/adapters/delonix-linux/tests/cgroup_parent.rs",
        ):
            self.assertIn(live, found)
        proxmox = (g.ROOT / "crates/providers/delonix-proxmox/tests/live.rs").read_text(encoding="utf-8")
        self.assertGreaterEqual(len(g.test_functions(proxmox)), 42)


if __name__ == "__main__":
    unittest.main()
