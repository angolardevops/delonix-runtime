"""api/openapi.yaml and the URLconf must describe the same routes and
methods; adding a route without documenting it (or the other way round)
fails here."""

from __future__ import annotations

import re
from pathlib import Path

import yaml
from django.urls import URLPattern, URLResolver, get_resolver

CONTRACT = Path(__file__).resolve().parent.parent / "api" / "openapi.yaml"
METHODS = {"get", "post", "put", "patch", "delete"}


def routes_in_code() -> set[tuple[str, str]]:
    found: set[tuple[str, str]] = set()

    def walk(patterns, prefix: str) -> None:
        for p in patterns:
            route = prefix + str(p.pattern)
            if isinstance(p, URLResolver):
                walk(p.url_patterns, route)
            elif isinstance(p, URLPattern):
                path = "/" + re.sub(r"<(?:\w+:)?(\w+)>", r"{\1}", route)
                methods = getattr(p.callback, "allowed_methods", None)
                assert methods, f"{path}: the view does not declare its methods (use core.http.api_view)"
                found.update((m.upper(), path) for m in methods)

    walk(get_resolver().url_patterns, "")
    return found


def routes_in_contract() -> set[tuple[str, str]]:
    spec = yaml.safe_load(CONTRACT.read_text())
    return {(m.upper(), path) for path, ops in spec["paths"].items() for m in ops if m in METHODS}


def test_openapi_matches_routes():
    code, contract = routes_in_code(), routes_in_contract()
    assert code - contract == set(), f"routes missing from api/openapi.yaml: {sorted(code - contract)}"
    assert contract - code == set(), f"documented routes the code does not serve: {sorted(contract - code)}"
