"""Smoke test for the health endpoints — run with `pytest`."""

from __future__ import annotations

from fastapi.testclient import TestClient

from __MODULE__.main import app

client = TestClient(app)


def test_live() -> None:
    resp = client.get("/api/v1/health/live")
    assert resp.status_code == 200
    assert resp.json() == {"status": "alive"}


def test_ready() -> None:
    resp = client.get("/api/v1/health/ready")
    assert resp.status_code == 200
    assert resp.json()["status"] == "ready"
