"""Dependency rules as a test (the rules are explained in ARCHITECTURE.md).

A small `ast` walk instead of import-linter: the rule is "these modules
import only the standard library and these few modules of ours", which fits
in a page, needs no extra tool or config format, and runs with the rest of
`pytest`. Imports are read from the source, so a forbidden import fails the
test even if nothing executes it."""

from __future__ import annotations

import ast
import sys
from pathlib import Path

import pytest

PACKAGE = "__MODULE__"
SRC = Path(__file__).resolve().parent.parent / "src" / PACKAGE

# Module → modules of this package it may import. Anything else of ours, and
# any third-party package, is forbidden: the rules and use cases know nothing
# about HTTP, clients, telemetry, settings or storage technology.
CORE: dict[str, set[str]] = {
    "notes.domain": set(),
    "notes.ports": {"notes.domain"},
    "notes.service": {"notes.domain", "notes.ports"},
    "notes.memory": {"notes.domain"},
    "webhooks.signature": set(),
    "webhooks.dedup": {"webhooks.signature"},
    "lifecycle": set(),
}
# Only the composition root may name the storage adapter.
ADAPTER_USERS = {"notes.memory": {"app"}}


def imports_of(source: str, module: str) -> set[str]:
    """Absolute module names imported by `source` (relative imports resolved)."""
    found: set[str] = set()
    package_parts = [PACKAGE, *module.split(".")[:-1]]
    for node in ast.walk(ast.parse(source)):
        if isinstance(node, ast.Import):
            found.update(alias.name for alias in node.names)
        elif isinstance(node, ast.ImportFrom):
            base = node.module or ""
            if node.level:
                anchor = package_parts[: len(package_parts) - node.level + 1]
                base = ".".join([*anchor, base] if base else anchor)
            found.add(base)
    return found


def violations(module: str, imported: set[str], allowed_ours: set[str]) -> list[str]:
    bad = []
    for name in sorted(imported):
        top = name.split(".")[0]
        if top == "__future__" or top in sys.stdlib_module_names:
            continue
        ours = name.removeprefix(PACKAGE + ".")
        if top == PACKAGE and ours in allowed_ours:
            continue
        bad.append(f"{module} imports {name}")
    return bad


def _source(module: str) -> str:
    return (SRC / (module.replace(".", "/") + ".py")).read_text(encoding="utf-8")


@pytest.mark.parametrize("module", sorted(CORE))
def test_core_modules_import_only_the_standard_library(module: str) -> None:
    assert violations(module, imports_of(_source(module), module), CORE[module]) == []


def test_only_the_composition_root_names_the_storage_adapter() -> None:
    for path in SRC.rglob("*.py"):
        module = ".".join(path.relative_to(SRC).with_suffix("").parts)
        imported = imports_of(path.read_text(encoding="utf-8"), module)
        for adapter, users in ADAPTER_USERS.items():
            if f"{PACKAGE}.{adapter}" in imported and module != adapter:
                assert module in users, f"{module} imports the adapter {adapter}"


def test_the_checker_catches_an_injected_import() -> None:
    injected = _source("notes.service") + "\nimport fastapi\nfrom httpx import AsyncClient\n"
    found = violations(
        "notes.service", imports_of(injected, "notes.service"), CORE["notes.service"]
    )
    assert found == ["notes.service imports fastapi", "notes.service imports httpx"]
