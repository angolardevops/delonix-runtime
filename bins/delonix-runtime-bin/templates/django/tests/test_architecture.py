"""Dependency direction, as a test. Each rule names a module set and the
imports it must not contain; a violation fails naming the file, the line and
the import. See ARCHITECTURE.md for why each rule exists."""

from __future__ import annotations

import ast
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent.parent

# (modules the rule applies to, forbidden import prefixes, reason)
RULES = [
    (
        ["notes/models.py", "notes/services.py", "notes/signals.py", "notes/apps.py"],
        [
            "django.http",
            "django.views",
            "django.urls",
            "django.shortcuts",
            "core",
            "webhooks",
            "notes.views",
            "httpx",
            "opentelemetry.sdk",
            "opentelemetry.instrumentation",
        ],
        "the capability knows nothing about HTTP, adapters or the telemetry SDK",
    ),
    (
        ["notes/views.py", "notes/urls.py"],
        ["django.db", "notes.models"],
        "views reach storage only through notes.services",
    ),
    (
        ["core/*.py"],
        ["notes", "webhooks"],
        "cross-cutting code does not depend on a capability",
    ),
    (
        ["webhooks/*.py"],
        ["notes.views", "notes.models", "notes.urls"],
        "the webhooks adapter uses the notes use cases and events, not their internals",
    ),
    (
        ["config/env.py"],
        ["django", "core", "notes", "webhooks"],
        "configuration parsing is plain Python (gunicorn reads it before Django)",
    ),
]


def imports_of(path: Path) -> list[tuple[int, str]]:
    found = []
    for node in ast.walk(ast.parse(path.read_text(), filename=str(path))):
        if isinstance(node, ast.Import):
            found += [(node.lineno, alias.name) for alias in node.names]
        elif isinstance(node, ast.ImportFrom) and node.module and node.level == 0:
            found.append((node.lineno, node.module))
            # `from notes import views` imports notes.views
            found += [(node.lineno, f"{node.module}.{alias.name}") for alias in node.names]
    return found


def forbidden(name: str, prefixes: list[str]) -> bool:
    return any(name == p or name.startswith(p + ".") for p in prefixes)


@pytest.mark.parametrize(("patterns", "prefixes", "reason"), RULES, ids=[r[2] for r in RULES])
def test_dependency_rule(patterns, prefixes, reason):
    files = sorted({f for pattern in patterns for f in ROOT.glob(pattern)})
    assert files, f"no file matches {patterns}"
    violations = [
        f"{f.relative_to(ROOT)}:{line} imports {name}"
        for f in files
        for line, name in imports_of(f)
        if forbidden(name, prefixes)
    ]
    assert not violations, f"{reason}:\n" + "\n".join(violations)
