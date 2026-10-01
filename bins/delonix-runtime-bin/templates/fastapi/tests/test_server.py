"""The process entry point: signal handling and configuration errors."""

from __future__ import annotations

import signal

import pytest
import uvicorn

from __MODULE__.lifecycle import (
    Readiness,
)
from __MODULE__.server import (
    GracefulServer,
    _load_settings,
)

from .conftest import make_settings


def server() -> tuple[GracefulServer, Readiness]:
    readiness = Readiness()
    config = uvicorn.Config(lambda scope, receive, send: None, log_config=None)
    return GracefulServer(config, readiness, make_settings()), readiness


def test_uvicorn_still_has_the_hooks_the_server_overrides() -> None:
    # GracefulServer relies on these; a uvicorn release that renames them
    # must fail here, not in production.
    plain = uvicorn.Server(uvicorn.Config(lambda scope, receive, send: None, log_config=None))
    assert callable(plain.handle_exit)
    assert plain.should_exit is False
    assert plain.force_exit is False


def test_first_signal_drains_and_a_repeated_sigterm_changes_nothing() -> None:
    srv, readiness = server()
    srv.handle_exit(signal.SIGTERM, None)
    assert readiness.state == "draining"
    assert srv.should_exit  # no loop running in this test: stop at once
    srv.handle_exit(signal.SIGTERM, None)
    assert not srv.force_exit
    assert not readiness.interrupted


def test_a_second_sigint_stops_waiting_and_reports_an_incomplete_stop() -> None:
    srv, readiness = server()
    srv.handle_exit(signal.SIGINT, None)
    srv.handle_exit(signal.SIGINT, None)
    assert srv.force_exit
    assert readiness.interrupted


def test_invalid_configuration_lists_every_problem_and_exits_1(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], tmp_path: object
) -> None:
    monkeypatch.chdir(str(tmp_path))  # no .env here
    monkeypatch.setenv("APP_ENV", "staging")
    monkeypatch.setenv("PORT", "70000")
    monkeypatch.setenv("WEBHOOK_INBOUND_SECRET", "not-a-secret-value")
    with pytest.raises(SystemExit) as exit_info:
        _load_settings()
    assert exit_info.value.code == 1
    err = capsys.readouterr().err
    assert err.startswith("configuration:")
    assert "APP_ENV" in err
    assert "PORT" in err
    # The offending value of a secret is never echoed.
    assert "not-a-secret-value" not in err
