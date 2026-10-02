"""The checked-in contract (api/openapi.json) is what the app serves.

FastAPI generates the OpenAPI document from the routes; this test fails when
a route, parameter or schema changes without the file being regenerated with
`make openapi` — so a contract change is always visible in review."""

from __future__ import annotations

import json
from pathlib import Path

from __MODULE__.contract import (
    CONTRACT_PATH,
    generate,
)


def test_openapi_matches_the_routes() -> None:
    committed = json.loads(Path(CONTRACT_PATH).read_text(encoding="utf-8"))
    assert generate() == committed, (
        "api/openapi.json is stale: run `make openapi` and review the diff"
    )


def test_every_route_is_versioned() -> None:
    paths = generate()["paths"]
    assert paths
    assert all(p.startswith("/api/v1/") for p in paths), sorted(paths)
