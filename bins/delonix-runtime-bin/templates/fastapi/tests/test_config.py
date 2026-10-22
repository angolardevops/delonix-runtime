"""Configuration is validated once, and every problem is reported."""

from __future__ import annotations

from pathlib import Path

import pytest
from pydantic import ValidationError

from .conftest import SECRET, make_settings


def test_defaults() -> None:
    s = make_settings()
    assert (s.port, s.shutdown_timeout, s.drain_delay, s.http_max_body_bytes) == (
        __PORT__,
        15.0,
        0.0,
        1_048_576,
    )
    assert s.inbound_key is None
    assert s.api_docs


@pytest.mark.parametrize(
    ("raw", "seconds"), [("15", 15.0), ("15s", 15.0), ("500ms", 0.5), ("1m", 60.0)]
)
def test_durations_accept_units(raw: str, seconds: float) -> None:
    assert make_settings(shutdown_timeout=raw).shutdown_timeout == seconds


def test_every_bad_field_is_listed_at_once() -> None:
    with pytest.raises(ValidationError) as err:
        make_settings(app_env="staging", port=0, shutdown_timeout="soon")
    fields = {e["loc"][0] for e in err.value.errors()}
    assert fields == {"app_env", "port", "shutdown_timeout"}


def test_production_refuses_plain_http_webhooks() -> None:
    with pytest.raises(ValidationError, match="https only"):
        make_settings(
            app_env="production",
            webhook_target_url="http://receiver.local/hook",
            webhook_target_secret=SECRET,
        )
    assert make_settings(
        app_env="development",
        webhook_target_url="http://receiver.local/hook",
        webhook_target_secret=SECRET,
    ).target_key


def test_cross_field_rules() -> None:
    with pytest.raises(ValidationError) as err:
        make_settings(
            drain_delay="20s",
            webhook_inbound_secret="not-a-secret",
            webhook_target_url="ftp://x",
        )
    message = str(err.value)
    for expected in (
        "DRAIN_DELAY",
        "WEBHOOK_INBOUND_SECRET",
        "WEBHOOK_TARGET_URL",
        "WEBHOOK_TARGET_SECRET",
    ):
        assert expected in message


def test_secrets_are_not_printed() -> None:
    s = make_settings(webhook_inbound_secret=SECRET)
    assert SECRET not in repr(s)


def test_tls_is_off_by_default() -> None:
    assert make_settings().tls is None


def test_tls_pair_serves_https(tmp_path: Path) -> None:
    crt, key = tmp_path / "tls.crt", tmp_path / "tls.key"
    crt.write_text("cert"), key.write_text("key")
    assert make_settings(tls_cert_file=crt, tls_key_file=key).tls == (crt, key)


def test_tls_needs_both_files(tmp_path: Path) -> None:
    crt = tmp_path / "tls.crt"
    crt.write_text("cert")
    with pytest.raises(ValidationError, match="must be set together"):
        make_settings(tls_cert_file=crt)


def test_tls_file_must_exist(tmp_path: Path) -> None:
    with pytest.raises(ValidationError, match="no readable file"):
        make_settings(
            tls_cert_file=tmp_path / "missing.crt",
            tls_key_file=tmp_path / "missing.key",
        )
