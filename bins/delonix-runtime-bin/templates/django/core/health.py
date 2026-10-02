"""Readiness: `starting` → `ready` → `draining`, plus the database.

The state belongs to one process (each gunicorn worker has its own). It moves
to `ready` once the WSGI application is loaded (config/wsgi.py) and to
`draining` when the worker receives SIGTERM (gunicorn.conf.py). While ready,
the probe also checks that the database answers and that no migration is
pending — a replica that cannot store a note should not receive traffic.
"""

from __future__ import annotations

import logging
import threading

from django.db import DatabaseError, connection
from django.db.migrations.executor import MigrationExecutor

log = logging.getLogger("core.health")

STARTING, READY, DRAINING = "starting", "ready", "draining"


class Readiness:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._state = STARTING
        self._migrations_applied = False

    @property
    def status(self) -> str:
        with self._lock:
            return self._state

    def mark_ready(self) -> None:
        with self._lock:
            if self._state == STARTING:
                self._state = READY

    def begin_draining(self) -> None:
        with self._lock:
            self._state = DRAINING

    def reset(self) -> None:
        """Back to `starting` — for tests."""
        with self._lock:
            self._state = STARTING
            self._migrations_applied = False

    def evaluate(self) -> tuple[int, dict[str, str]]:
        """HTTP status and body of the readiness probe."""
        status = self.status
        if status != READY:
            return 503, {"status": status}
        failing = self._failing_dependency()
        if failing:
            log.warning("readiness dependency failing", extra={"dependency": failing})
            return 503, {"status": "unavailable", "dependency": failing}
        return 200, {"status": READY}

    def _failing_dependency(self) -> str:
        try:
            with connection.cursor() as cursor:
                cursor.execute("SELECT 1")
        except DatabaseError:
            return "database"
        if not self._migrations_applied:
            # Once every migration is applied it stays so for this process;
            # the (small) cost of reading the migration graph is paid only
            # until then.
            try:
                executor = MigrationExecutor(connection)
                pending = executor.migration_plan(executor.loader.graph.leaf_nodes())
            except DatabaseError:
                return "database"
            if pending:
                return "migrations"
            self._migrations_applied = True
        return ""


state = Readiness()
