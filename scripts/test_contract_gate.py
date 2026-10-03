#!/usr/bin/env python3
"""Unit tests for `scripts/contract_gate.py`'s `problem_json` transform — stdlib
`unittest`, no new dependency (dev tooling, like the other `test_*.py` here).

The transform is the one place the published OpenAPI differs from the plugin's
output (ADR-0042 D2), so the tests fix its decisions: every error response
becomes `application/problem+json` with the `Problem` schema, `google.rpc.Status`
leaves the document, `google.protobuf.Any` (still referenced elsewhere) stays,
and output the transform does not recognise stops the gate instead of passing
half-transformed.

Run: python3 scripts/test_contract_gate.py
"""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import contract_gate as cg  # noqa: E402

SAMPLE = """openapi: 3.0.3
paths:
    /v1/node:
        get:
            operationId: NodeService_GetNodeInfo
            responses:
                "200":
                    description: OK
""" + cg.STATUS_DEFAULT + """    /v1/providers:
        get:
            operationId: NodeService_ListProviders
            responses:
                "200":
                    description: OK
""" + cg.STATUS_DEFAULT + """components:
    schemas:
        delonix.node.v1.Operation:
            type: object
            properties:
                result:
                    allOf:
                        - $ref: '#/components/schemas/google.protobuf.Any'
        google.protobuf.Any:
            type: object
            properties:
                '@type':
                    type: string
        google.rpc.Status:
            type: object
            properties:
                code:
                    type: integer
                details:
                    type: array
                    items:
                        $ref: '#/components/schemas/google.protobuf.Any'
tags:
    - name: NodeService
"""


class ProblemJson(unittest.TestCase):
    def test_every_error_response_becomes_a_problem_document(self):
        out = cg.problem_json(SAMPLE)
        self.assertEqual(out.count("\n                        application/problem+json:\n"), 2)
        self.assertEqual(out.count("$ref: '#/components/schemas/Problem'"), 2)
        self.assertNotIn("Default error response", out)
        self.assertIn("\n        Problem:\n", out)

    def test_status_leaves_and_any_stays(self):
        out = cg.problem_json(SAMPLE)
        self.assertNotIn("google.rpc.Status", out)
        self.assertIn("\n        google.protobuf.Any:\n", out)
        # The schema after the removed block is intact.
        self.assertTrue(out.rstrip().endswith("- name: NodeService"))

    def test_unrecognised_output_stops_the_gate(self):
        changed = SAMPLE.replace("Default error response", "Some other wording", 1)
        with self.assertRaises(SystemExit):
            cg.problem_json(changed)
        with self.assertRaises(SystemExit):
            cg.problem_json(SAMPLE.replace(cg.STATUS_DEFAULT, ""))

    def test_the_problem_schema_declares_what_the_engine_sends(self):
        # `delonix_model::codes::problem` + `grpc_status` (crates/interfaces/delonix-node-api).
        for field in ("type", "title", "status", "detail", "instance", "code", "dx",
                      "exit", "reason", "provider", "role", "capability", "step",
                      "planDigest", "cause", "grpc_status"):
            self.assertIn(f"\n                {field}:\n", cg.PROBLEM_SCHEMA, field)


if __name__ == "__main__":
    unittest.main()
