"""Liveness and readiness: starting → ready → draining, and the database."""

from __future__ import annotations

import pytest
from django.db import DatabaseError

from core import health

pytestmark = pytest.mark.django_db
LIVE, READY = "/api/v1/health/live", "/api/v1/health/ready"


def test_live_answers_while_the_process_runs(client):
    assert client.get(LIVE).json() == {"status": "alive"}


def test_readiness_follows_the_lifecycle(client):
    health.state.reset()
    assert (client.get(READY).status_code, client.get(READY).json()) == (503, {"status": "starting"})
    health.state.mark_ready()
    assert (client.get(READY).status_code, client.get(READY).json()) == (200, {"status": "ready"})
    health.state.begin_draining()
    assert (client.get(READY).status_code, client.get(READY).json()) == (503, {"status": "draining"})
    assert client.get(LIVE).status_code == 200  # still alive while draining


def test_probes_ignore_host_validation_and_the_https_redirect(client, settings):
    settings.SECURE_SSL_REDIRECT = True
    for path in (LIVE, READY):
        assert client.get(path, headers={"Host": "10.1.2.3:8000"}).status_code == 200
    assert client.get("/api/v1/notes").status_code == 301  # everything else is redirected


def test_a_failing_database_makes_the_process_unready(client, monkeypatch):
    def broken(*args, **kwargs):
        raise DatabaseError("down")

    monkeypatch.setattr("core.health.connection.cursor", broken)
    response = client.get(READY)
    assert (response.status_code, response.json()) == (503, {"status": "unavailable", "dependency": "database"})


def test_pending_migrations_make_the_process_unready(client, monkeypatch):
    class Pending:
        def __init__(self, connection):
            self.loader = type("L", (), {"graph": type("G", (), {"leaf_nodes": lambda self: []})()})()

        def migration_plan(self, targets):
            return [("notes.0002_future", False)]

    monkeypatch.setattr("core.health.MigrationExecutor", Pending)
    response = client.get(READY)
    assert (response.status_code, response.json()["dependency"]) == (503, "migrations")
