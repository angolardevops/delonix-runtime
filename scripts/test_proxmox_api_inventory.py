#!/usr/bin/env python3
"""Tests for scripts/proxmox_api_inventory.py — a synthetic apidoc, never the real one.

The real schema is 4 MB of JavaScript fetched from a node; what these tests pin is
the reading discipline: the JS wrapper is cut correctly, a verb is read from the
statement the literal sits in (never from the neighbouring function), a comment
is not a call, a test-module literal is not a call, an excluded route stays in
the denominator, a trace of a live run promotes a route to tested, and a called
route the schema lacks fails the run.

The last test is a GATE on the repository: the committed matrix for 9.2.2 must
be byte for byte what the script regenerates from the committed schema and the
crate's source — a route added to the crate without regenerating the matrix
fails here.
"""

import io
import json
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import proxmox_api_inventory as inv  # noqa: E402

REPO = Path(__file__).resolve().parent.parent

SCHEMA = [
    {
        "path": "/nodes",
        "info": {"GET": {"returns": {"type": "array"}}},
        "children": [
            {
                "path": "/nodes/{node}",
                "children": [
                    {
                        "path": "/nodes/{node}/qemu",
                        "info": {"GET": {"returns": {"type": "array"}}, "POST": {"returns": {"type": "string"}}},
                        "children": [
                            {
                                "path": "/nodes/{node}/qemu/{vmid}",
                                "info": {"DELETE": {"returns": {"type": "string"}}},
                                "children": [
                                    {
                                        "path": "/nodes/{node}/qemu/{vmid}/config",
                                        "info": {"GET": {"returns": {"type": "object"}}},
                                    }
                                ],
                            }
                        ],
                    },
                    {"path": "/nodes/{node}/apt/update", "info": {"POST": {}, "GET": {}}},
                    {"path": "/nodes/{node}/lxc", "info": {"GET": {}}},
                ],
            }
        ],
    },
    {"path": "/access/ticket", "info": {"POST": {"allowtoken": 0}}},
    {"path": "/access/users", "info": {"GET": {}}},
]

SOURCE = '''
fn a(&self) {
    let nodes = parse(&me.get("/nodes")?, "/nodes")?;
    self.task("create", || {
        self.post_form(&format!("/nodes/{}/qemu", self.node), &form, true)
    })
}
fn b(&self) {
    let url = self.url(&format!(
        "/nodes/{}/qemu/{vmid}?purge=1",
        self.node
    ));
    self.task("destroy", || self.send_authed(|| self.http.delete(&url)))
}
// a comment quoting "/access/users" is not a call
fn c(&self) {
    let orphan = "/nodes/{}/lxc";
    let _ = orphan;
}

#[cfg(test)]
mod tests {
    fn t() {
        let x = self.get("/nodes/pve/qemu/100/config");
    }
}
'''


def write_apidoc(tmp: Path) -> Path:
    p = tmp / "apidoc.js"
    p.write_text("const apiSchema = " + json.dumps(SCHEMA) + ";\nfoo();\n", encoding="utf-8")
    return p


class Schema(unittest.TestCase):
    def test_one_row_per_method_and_path(self):
        with tempfile.TemporaryDirectory() as d:
            rows = inv.extract_apidoc(write_apidoc(Path(d)))
        self.assertEqual(len(rows), 10)
        self.assertIn(("POST", "/nodes/{node}/qemu"), {(r["method"], r["path"]) for r in rows})

    def test_the_committed_form_carries_provenance_and_the_same_rows(self):
        with tempfile.TemporaryDirectory() as d:
            apidoc = write_apidoc(Path(d))
            buf = io.StringIO()
            with redirect_stdout(buf):
                inv.main([str(apidoc), "--extract"])
            committed = Path(d) / "api.routes.json"
            committed.write_text(buf.getvalue())
            prov, rows = inv.load_schema(committed)
        self.assertEqual(len(rows), 10)
        self.assertIn("source", prov)


class ClientRoutes(unittest.TestCase):
    def test_verb_is_read_from_the_statement_on_either_side_of_the_literal(self):
        called, unknown = inv.client_routes(SOURCE)
        self.assertEqual(
            called,
            {("GET", "/nodes"), ("POST", "/nodes/{node}/qemu"), ("DELETE", "/nodes/{node}/qemu/{vmid}")},
        )
        self.assertEqual(unknown, ["/nodes/{node}/lxc"], "a literal with no verb nearby is not a call")

    def test_a_comment_and_a_test_module_literal_are_not_calls(self):
        called, unknown = inv.client_routes(SOURCE)
        paths = {p for _, p in called} | set(unknown)
        self.assertNotIn("/access/users", paths)
        self.assertNotIn("/nodes/pve/qemu/100/config", paths)

    def test_query_and_placeholders_are_normalised(self):
        self.assertEqual(
            inv.normalise("/nodes/{}/qemu/{template}/snapshot/{name}/rollback?x=1"),
            "/nodes/{node}/qemu/{vmid}/snapshot/{snapname}/rollback",
        )


class Trace(unittest.TestCase):
    def test_a_concrete_route_maps_to_the_longest_template(self):
        with tempfile.TemporaryDirectory() as d:
            rows = inv.extract_apidoc(write_apidoc(Path(d)))
        hit = inv.traced_routes(
            "GET /nodes\nGET /nodes/pve/qemu/100/config\nDELETE /nodes/pve/qemu/100?purge=1\nPOST /nowhere\n",
            rows,
        )
        self.assertEqual(
            hit,
            {("GET", "/nodes"), ("GET", "/nodes/{node}/qemu/{vmid}/config"), ("DELETE", "/nodes/{node}/qemu/{vmid}")},
        )


    def test_header_lines_are_provenance_not_requests(self):
        with tempfile.TemporaryDirectory() as d:
            rows = inv.extract_apidoc(write_apidoc(Path(d)))
        trace = "# node: pve\n# version: 9.2.2\n# run: 2026-09-23\n#\nGET /nodes\n# GET /nodes/pve/qemu\n"
        hit = inv.traced_routes(trace, rows)
        self.assertEqual(hit, {("GET", "/nodes")}, "a commented-out request is not a request")
        self.assertEqual(inv.trace_provenance(trace), {"node": "pve", "version": "9.2.2", "run": "2026-09-23"})
        self.assertEqual(inv.trace_requests(trace), 1)

    def test_the_markdown_names_the_run_behind_the_tested_column(self):
        with tempfile.TemporaryDirectory() as d:
            rows = inv.extract_apidoc(write_apidoc(Path(d)))
        called, _ = inv.client_routes(SOURCE)
        classified = inv.classify(rows, called, {("GET", "/nodes")})
        md = inv.render_markdown({"version": "9.2.2"}, classified, [], {"file": "t.routes", "run": "2026-09-23"})
        self.assertIn("### Live trace", md)
        self.assertIn("- **run**: `2026-09-23`", md)
        # And without a trace the matrix says so, instead of a tested column nobody can date.
        md2 = inv.render_markdown({"version": "9.2.2"}, classified, [], None)
        self.assertIn("None given", md2)


class Classification(unittest.TestCase):
    def rows(self, trace=None):
        with tempfile.TemporaryDirectory() as d:
            rows = inv.extract_apidoc(write_apidoc(Path(d)))
        called, _ = inv.client_routes(SOURCE)
        tested = inv.traced_routes(trace, rows) if trace else set()
        return inv.classify(rows, called, tested)

    def test_five_states_and_the_excluded_stay_in_the_denominator(self):
        rows = self.rows()
        s = inv.summary(rows)
        self.assertEqual(s["denominator"], 10)
        self.assertEqual(
            s["states"],
            {inv.SUPPORTED_UNTESTED: 3, inv.UNSUPPORTED: 4, inv.NOT_YET: 3},
        )
        by = {(r["method"], r["path"]): r for r in rows}
        self.assertEqual(by[("POST", "/nodes/{node}/apt/update")]["state"], inv.UNSUPPORTED)
        self.assertTrue(by[("POST", "/nodes/{node}/apt/update")]["reason"])
        self.assertEqual(by[("GET", "/nodes/{node}/lxc")]["state"], inv.UNSUPPORTED, "no verb read → not called")
        self.assertEqual(by[("GET", "/nodes/{node}/qemu")]["state"], inv.NOT_YET)
        self.assertEqual(by[("POST", "/access/ticket")]["state"], inv.NOT_YET, "login is only called when the source calls it")

    def test_a_trace_promotes_a_called_route_to_tested_and_nothing_else(self):
        rows = self.rows(trace="GET /nodes\nGET /nodes/pve/qemu\n")
        by = {(r["method"], r["path"]): r for r in rows}
        self.assertEqual(by[("GET", "/nodes")]["state"], inv.SUPPORTED_TESTED)
        # Traced but not called by the crate: a trace cannot invent a call.
        self.assertEqual(by[("GET", "/nodes/{node}/qemu")]["state"], inv.NOT_YET)
        self.assertEqual(by[("POST", "/nodes/{node}/qemu")]["state"], inv.SUPPORTED_UNTESTED)

    def test_a_called_route_absent_from_the_schema_fails_the_run(self):
        with tempfile.TemporaryDirectory() as d:
            apidoc = write_apidoc(Path(d))
            src = Path(d) / "lib.rs"
            # Before the test module: a literal after `#[cfg(test)]` is not a call.
            extra = 'fn z(&self) { self.get("/nodes/{}/qemu/{vmid}/nope") }\n'
            src.write_text(SOURCE.replace("#[cfg(test)]", extra + "#[cfg(test)]"))
            buf = io.StringIO()
            with redirect_stdout(buf):
                rc = inv.main([str(apidoc), "--source", str(src), "--json"])
        self.assertEqual(rc, 1)
        out = json.loads(buf.getvalue())
        self.assertEqual(out["summary"]["states"].get(inv.NOT_IN_VERSION), 1)
        self.assertEqual(out["summary"]["denominator"], 10, "a route the schema lacks is not in the denominator")


class CommittedMatrix(unittest.TestCase):
    """The gate: docs/proxmox/matrix-9.2.2.md is what the script regenerates."""

    def test_the_committed_matrix_is_up_to_date(self):
        schema = REPO / "docs/proxmox/api-9.2.2.routes.json"
        matrix = REPO / "docs/proxmox/matrix-9.2.2.md"
        trace = REPO / "docs/proxmox/trace-9.2.2.routes"
        if not schema.exists() or not matrix.exists():
            self.skipTest("no committed 9.2.2 schema/matrix in this tree")
        # The committed trace is what puts routes in the `tested` column; the
        # gate regenerates with it, exactly as the docstring command does.
        argv = [sys.executable, str(REPO / "scripts/proxmox_api_inventory.py"), str(schema), "--markdown"]
        if trace.exists():
            # Relative on purpose: the path is rendered into the matrix's provenance,
            # and the committed file says `docs/proxmox/trace-9.2.2.routes`.
            argv += ["--trace", str(trace.relative_to(REPO))]
        out = subprocess.run(
            argv,
            cwd=REPO,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        self.assertEqual(
            out.stdout,
            matrix.read_text(encoding="utf-8"),
            "docs/proxmox/matrix-9.2.2.md is stale — regenerate it: "
            "python3 scripts/proxmox_api_inventory.py docs/proxmox/api-9.2.2.routes.json "
            "--trace docs/proxmox/trace-9.2.2.routes --markdown > docs/proxmox/matrix-9.2.2.md",
        )


if __name__ == "__main__":
    unittest.main()
