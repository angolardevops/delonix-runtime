"""config.env: typed, validated, every error at once; production refuses
the development shortcuts."""

from __future__ import annotations

import base64
import stat

import pytest

from config.env import DEV_SECRET_KEY, ConfigError, load, parse_duration, parse_webhook_secret

GOOD_KEY = "k" * 30 + "0123456789abcdefghij"  # 50 characters, many distinct
WHSEC = "whsec_" + base64.b64encode(b"x" * 32).decode()


def production(**extra: str) -> dict[str, str]:
    env = {
        "APP_ENV": "production",
        "SECRET_KEY": GOOD_KEY,
        "ALLOWED_HOSTS": "api.example.com",
        "DATABASE_URL": "sqlite:////data/db.sqlite3",
    }
    env.update(extra)
    return env


def errors_of(env: dict[str, str]) -> list[str]:
    with pytest.raises(ConfigError) as info:
        load(env)
    return info.value.errors


def test_development_defaults(tmp_path):
    cfg = load({}, base_dir=tmp_path)
    assert cfg.app_env == "development"
    assert cfg.secret_key == DEV_SECRET_KEY
    assert "localhost" in cfg.allowed_hosts
    assert cfg.database_url == f"sqlite:///{tmp_path / 'db.sqlite3'}"
    assert cfg.port == __PORT__
    assert cfg.shutdown_timeout == 15.0
    assert not cfg.ssl_redirect and cfg.hsts_seconds == 0


def test_production_with_everything_set_loads():
    cfg = load(production(TRUSTED_PROXY="10.0.0.1"))
    assert cfg.production and cfg.ssl_redirect and cfg.hsts_seconds == 31_536_000
    assert cfg.warnings == ()


def test_production_refuses_the_development_shortcuts():
    errs = errors_of({"APP_ENV": "production"})
    joined = " ".join(errs)
    assert "SECRET_KEY: required" in joined
    assert "ALLOWED_HOSTS: required" in joined
    assert "DATABASE_URL: required" in joined


@pytest.mark.parametrize("key", [DEV_SECRET_KEY, "short", "django-insecure-" + "x" * 60, "ab" * 40])
def test_production_refuses_an_insecure_secret_key(key):
    assert any("SECRET_KEY: insecure" in e for e in errors_of(production(SECRET_KEY=key)))


def test_production_refuses_a_wildcard_host():
    assert any("'*'" in e for e in errors_of(production(ALLOWED_HOSTS="*")))


def test_every_error_is_reported_at_once():
    errs = errors_of({"APP_ENV": "nope", "PORT": "x", "SHUTDOWN_TIMEOUT": "soon", "LOG_LEVEL": "loud"})
    assert len(errs) == 4


def test_webhook_target_needs_its_secret_and_https_in_production():
    assert any("WEBHOOK_TARGET_SECRET" in e for e in errors_of({"WEBHOOK_TARGET_URL": "http://x.test/h"}))
    errs = errors_of(production(WEBHOOK_TARGET_URL="http://x.test/h", WEBHOOK_TARGET_SECRET=WHSEC))
    assert any("https in production" in e for e in errs)


def test_ssl_redirect_without_a_trusted_proxy_is_a_warning():
    assert load(production()).warnings


def test_drain_delay_must_fit_in_the_shutdown_timeout():
    assert any("DRAIN_DELAY" in e for e in errors_of({"DRAIN_DELAY": "20s", "SHUTDOWN_TIMEOUT": "10s"}))


@pytest.mark.parametrize(("raw", "seconds"), [("15s", 15), ("500ms", 0.5), ("2m", 120), ("3", 3)])
def test_durations(raw, seconds):
    assert parse_duration(raw) == seconds


@pytest.mark.parametrize("raw", ["", "whsec_", "whsec_!!!", "whsec_" + base64.b64encode(b"short").decode(), "abc"])
def test_bad_webhook_secrets_are_refused(raw):
    with pytest.raises(ValueError):
        parse_webhook_secret(raw)


def test_secret_key_file_is_created_private_and_then_reused(tmp_path):
    path = tmp_path / "secret_key"
    env = production(SECRET_KEY="", SECRET_KEY_FILE=str(path))
    first = load(env).secret_key
    assert len(first) >= 50
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    assert load(env).secret_key == first
