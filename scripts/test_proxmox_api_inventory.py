#!/usr/bin/env python3
"""Tests for scripts/proxmox_api_inventory.py — a synthetic apidoc, never the real one.

The real schema is 4 MB of JavaScript fetched from a node; what these tests pin is
the reading discipline: the JS wrapper is cut correctly, a verb is read from the
call site on either side of the literal, a path with no verb is NOT counted, an
excluded route stays in the denominator, and a called route the schema lacks
fails the run.
"""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import proxmox_api_inventory as inv  # noqa: E402

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
'''


def write_apidoc(tmp: Path) -> Path:
    p = tmp / "apidoc.js"
    p.write_text("const apiSchema = " + json.dumps(SCHEMA) + ";\nfoo();\n", encoding="utf-8")
    return p


class Schema(unittest.TestCase):
    def test_one_row_per_method_and_path(self):
        with tempfile.TemporaryDirectory() as d:
            rows = inv.load_schema(write_apidoc(Path(d)))
        self.assertEqual(len(rows), 9)
        self.assertIn(("POST", "/nodes/{node}/qemu"), {(r["method"], r["path"]) for r in rows})


class ClientRoutes(unittest.TestCase):
    def test_verb_is_read_on_either_side_of_the_literal(self):
        called, unknown = inv.client_routes(SOURCE)
        self.assertEqual(
            called,
            {("GET", "/nodes"), ("POST", "/nodes/{node}/qemu"), ("DELETE", "/nodes/{node}/qemu/{vmid}")},
        )
        self.assertEqual(unknown, ["/nodes/{node}/lxc"], "a literal with no verb nearby is not a call")

    def test_a_comment_is_not_a_call(self):
        called, unknown = inv.client_routes(SOURCE)
        self.assertNotIn("/access/users", {p for _, p in called} | set(unknown))

    def test_query_and_placeholders_are_normalised(self):
        self.assertEqual(
            inv.normalise("/nodes/{}/qemu/{template}/snapshot/{name}/rollback?x=1"),
            "/nodes/{node}/qemu/{vmid}/snapshot/{snapname}/rollback",
        )


class Classification(unittest.TestCase):
    def rows(self):
        with tempfile.TemporaryDirectory() as d:
            rows = inv.load_schema(write_apidoc(Path(d)))
        called, _ = inv.client_routes(SOURCE)
        return inv.classify(rows, called)

    def test_three_states_and_the_excluded_stay_in_the_denominator(self):
        rows = self.rows()
        s = inv.summary(rows)
        self.assertEqual(s["total"], 9)
        self.assertEqual(s["states"], {"called": 3, "excluded": 4, "missing": 2})
        by = {(r["method"], r["path"]): r for r in rows}
        self.assertEqual(by[("POST", "/nodes/{node}/apt/update")]["state"], "excluded")
        self.assertTrue(by[("POST", "/nodes/{node}/apt/update")]["reason"])
        self.assertEqual(by[("GET", "/nodes/{node}/lxc")]["state"], "excluded", "no verb read → not called")
        self.assertEqual(by[("GET", "/nodes/{node}/qemu")]["state"], "missing")
        self.assertEqual(by[("POST", "/access/ticket")]["state"], "missing", "login is only 'called' when the source calls it")

    def test_a_called_route_absent_from_the_schema_fails_the_run(self):
        with tempfile.TemporaryDirectory() as d:
            apidoc = write_apidoc(Path(d))
            src = Path(d) / "lib.rs"
            src.write_text(SOURCE + '\nfn z(&self) { self.get("/nodes/{}/qemu/{vmid}/nope") }\n')
            rc = inv.main([str(apidoc), "--source", str(src), "--json"])
        self.assertEqual(rc, 1)


if __name__ == "__main__":
    unittest.main()
