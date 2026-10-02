"""Configuration is validated once, and every problem is reported."""

from __future__ import annotations

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
