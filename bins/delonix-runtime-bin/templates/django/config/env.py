"""Typed configuration read from the environment.

Pure Python on purpose: `config/settings.py` (Django) and `gunicorn.conf.py`
(the server, which runs before Django is imported) both call `load()`, so the
two never disagree about a value. Every invalid variable is reported at once
and the process stops before it listens.
"""

from __future__ import annotations

import base64
import contextlib
import os
import re
import secrets
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlsplit

ENVIRONMENTS = ("development", "test", "production")
LOG_LEVELS = ("debug", "info", "warning", "error")

# Used only outside production, and named so that Django's own deploy check
# (security.W009) and our production refusal both recognise it.
DEV_SECRET_KEY = "django-insecure-development-only-this-is-not-a-secret"  # noqa: S105
DEV_ALLOWED_HOSTS = ("localhost", "127.0.0.1", "[::1]")

_DURATION = re.compile(r"^(\d+(?:\.\d+)?)(ms|s|m)?$")


class ConfigError(Exception):
    """One or more variables are invalid; `errors` lists each of them."""

    def __init__(self, errors: list[str]) -> None:
        self.errors = errors
        super().__init__("configuration: " + "; ".join(errors))


@dataclass(frozen=True)
class Config:
    app_env: str
    log_level: str
    service_name: str
    service_version: str
    host: str
    port: int
    web_concurrency: int
    threads: int
    shutdown_timeout: float
    drain_delay: float
    max_body_bytes: int
    secret_key: str
    allowed_hosts: tuple[str, ...]
    database_url: str
    trusted_proxy: tuple[str, ...]
    ssl_redirect: bool
    hsts_seconds: int
    hsts_include_subdomains: bool
    hsts_preload: bool
    webhook_inbound_key: bytes | None
    webhook_target_url: str
    webhook_target_key: bytes | None
    otlp_endpoint: str
    otel_disabled: bool
    warnings: tuple[str, ...] = field(default=())

    @property
    def production(self) -> bool:
        return self.app_env == "production"


def parse_duration(raw: str) -> float:
    """`15s`, `500ms`, `2m` or a bare number of seconds → seconds."""
    m = _DURATION.match(raw.strip())
    if not m:
        raise ValueError(f"{raw!r} is not a duration like 15s, 500ms or 2m")
    value, unit = float(m.group(1)), m.group(2) or "s"
    return value / 1000 if unit == "ms" else value * 60 if unit == "m" else value


def parse_webhook_secret(raw: str) -> bytes:
    """Decode a Standard Webhooks `whsec_<base64>` secret (≥ 24 bytes)."""
    if not raw.startswith("whsec_"):
        raise ValueError('must look like "whsec_<base64>"')
    try:
        key = base64.b64decode(raw[len("whsec_") :], validate=True)
    except ValueError as exc:
        raise ValueError("is not valid base64") from exc
    if len(key) < 24:
        raise ValueError("must decode to at least 24 bytes")
    return key


def insecure_secret_key(key: str) -> bool:
    """The same test as Django's security.W009, plus our development key."""
    return key == DEV_SECRET_KEY or key.startswith("django-insecure-") or len(key) < 50 or len(set(key)) < 5


def read_or_create_secret_key(path: str) -> str:
    """Read the key from `path`; if the file does not exist, create it with a
    random key first.

    Creation is atomic and never overwrites: the key is written to a private
    temporary file and hard-linked into place, so two processes starting at
    once both end up reading the one key that won. The file is mode 0600.
    """
    target = Path(path)
    if not target.exists():
        tmp = target.with_name(f".{target.name}.{os.getpid()}.tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            with os.fdopen(fd, "w") as fh:
                fh.write(secrets.token_urlsafe(64))
            with contextlib.suppress(FileExistsError):  # another process won: use its key
                os.link(tmp, target)
        finally:
            tmp.unlink(missing_ok=True)
    return target.read_text().strip()


def load(environ: dict[str, str] | None = None, base_dir: Path | None = None) -> Config:
    """Parse and validate the configuration. Raises ConfigError listing every
    problem found."""
    env = os.environ if environ is None else environ
    base = base_dir or Path(__file__).resolve().parent.parent
    errors: list[str] = []
    warnings: list[str] = []

    def get(name: str, default: str = "") -> str:
        return env.get(name, default).strip()

    def boolean(name: str, default: bool) -> bool:
        raw = get(name).lower()
        if raw == "":
            return default
        if raw in ("1", "true", "yes", "on"):
            return True
        if raw in ("0", "false", "no", "off"):
            return False
        errors.append(f"{name}: {raw!r} is not a boolean (true/false)")
        return default

    def integer(name: str, default: int, low: int, high: int) -> int:
        raw = get(name)
        if raw == "":
            return default
        try:
            value = int(raw)
        except ValueError:
            errors.append(f"{name}: {raw!r} is not an integer")
            return default
        if not low <= value <= high:
            errors.append(f"{name}: must be between {low} and {high}")
            return default
        return value

    def duration(name: str, default: float) -> float:
        raw = get(name)
        if raw == "":
            return default
        try:
            return parse_duration(raw)
        except ValueError as exc:
            errors.append(f"{name}: {exc}")
            return default

    def csv(name: str) -> tuple[str, ...]:
        return tuple(p.strip() for p in get(name).split(",") if p.strip())

    app_env = get("APP_ENV", "development").lower()
    if app_env not in ENVIRONMENTS:
        errors.append(f"APP_ENV: must be one of {', '.join(ENVIRONMENTS)}")
        app_env = "development"
    production = app_env == "production"

    log_level = get("LOG_LEVEL", "info").lower()
    if log_level == "warn":
        log_level = "warning"
    if log_level not in LOG_LEVELS:
        errors.append(f"LOG_LEVEL: must be one of {', '.join(LOG_LEVELS)}")
        log_level = "info"

    shutdown_timeout = duration("SHUTDOWN_TIMEOUT", 15.0)
    drain_delay = duration("DRAIN_DELAY", 0.0)
    if drain_delay >= shutdown_timeout:
        errors.append("DRAIN_DELAY: must be shorter than SHUTDOWN_TIMEOUT")

    # SECRET_KEY: the environment, or a file (created on first use). In
    # production a missing or guessable key is refused.
    secret_key = get("SECRET_KEY")
    key_file = get("SECRET_KEY_FILE")
    if not secret_key and key_file:
        try:
            secret_key = read_or_create_secret_key(key_file)
        except OSError as exc:
            errors.append(f"SECRET_KEY_FILE: {exc.strerror or exc}: {key_file}")
    if production:
        if not secret_key:
            errors.append("SECRET_KEY: required in production (or set SECRET_KEY_FILE)")
        elif insecure_secret_key(secret_key):
            errors.append(
                "SECRET_KEY: insecure in production — use at least 50 random characters"
                " (python -c 'import secrets; print(secrets.token_urlsafe(50))')"
            )
    elif not secret_key:
        secret_key = DEV_SECRET_KEY

    allowed_hosts = csv("ALLOWED_HOSTS")
    if production:
        if not allowed_hosts:
            errors.append("ALLOWED_HOSTS: required in production (comma-separated host names)")
        elif "*" in allowed_hosts:
            errors.append("ALLOWED_HOSTS: '*' disables host validation; list the host names")
    elif not allowed_hosts:
        allowed_hosts = DEV_ALLOWED_HOSTS + (("testserver",) if app_env == "test" else ())

    database_url = get("DATABASE_URL")
    if not database_url:
        if production:
            errors.append("DATABASE_URL: required in production, e.g. sqlite:////data/db.sqlite3")
        database_url = f"sqlite:///{base / 'db.sqlite3'}"
    elif urlsplit(database_url).scheme not in ("sqlite", "postgres", "postgresql", "pgsql"):
        errors.append("DATABASE_URL: scheme must be sqlite:// or postgres://")

    trusted_proxy = csv("TRUSTED_PROXY")
    ssl_redirect = boolean("SECURE_SSL_REDIRECT", production)
    hsts_seconds = integer("SECURE_HSTS_SECONDS", 31_536_000 if production else 0, 0, 10**9)
    hsts_subdomains = boolean("SECURE_HSTS_INCLUDE_SUBDOMAINS", production)
    hsts_preload = boolean("SECURE_HSTS_PRELOAD", False)
    if production and ssl_redirect and not trusted_proxy:
        warnings.append(
            "SECURE_SSL_REDIRECT is on and TRUSTED_PROXY is unset: every plain-HTTP request"
            " (except health probes) is redirected to https"
        )

    def webhook_key(name: str) -> bytes | None:
        raw = get(name)
        if not raw:
            return None
        try:
            return parse_webhook_secret(raw)
        except ValueError as exc:
            errors.append(f"{name}: {exc}")
            return None

    inbound_key = webhook_key("WEBHOOK_INBOUND_SECRET")
    target_url = get("WEBHOOK_TARGET_URL")
    target_key = webhook_key("WEBHOOK_TARGET_SECRET")
    if target_url:
        parts = urlsplit(target_url)
        if parts.scheme not in ("http", "https") or not parts.netloc:
            errors.append("WEBHOOK_TARGET_URL: must be an absolute http(s) URL")
        elif production and parts.scheme != "https":
            errors.append("WEBHOOK_TARGET_URL: must be https in production")
        if target_key is None and not get("WEBHOOK_TARGET_SECRET"):
            errors.append("WEBHOOK_TARGET_SECRET: required when WEBHOOK_TARGET_URL is set")

    otlp = get("OTEL_EXPORTER_OTLP_ENDPOINT")
    if otlp and urlsplit(otlp).scheme not in ("http", "https"):
        errors.append("OTEL_EXPORTER_OTLP_ENDPOINT: must be an http(s) URL of the OTLP/HTTP port")

    config = Config(
        app_env=app_env,
        log_level=log_level,
        service_name=get("SERVICE_NAME", "__NAME__"),
        service_version=get("APP_VERSION", "dev"),
        host=get("HTTP_HOST", "0.0.0.0"),
        port=integer("PORT", __PORT__, 1, 65535),
        web_concurrency=integer("WEB_CONCURRENCY", 2, 1, 64),
        threads=integer("GUNICORN_THREADS", 4, 1, 64),
        shutdown_timeout=shutdown_timeout,
        drain_delay=drain_delay,
        max_body_bytes=integer("HTTP_MAX_BODY_BYTES", 1_048_576, 1024, 1 << 30),
        secret_key=secret_key,
        allowed_hosts=allowed_hosts,
        database_url=database_url,
        trusted_proxy=trusted_proxy,
        ssl_redirect=ssl_redirect,
        hsts_seconds=hsts_seconds,
        hsts_include_subdomains=hsts_subdomains,
        hsts_preload=hsts_preload,
        webhook_inbound_key=inbound_key,
        webhook_target_url=target_url,
        webhook_target_key=target_key,
        otlp_endpoint=otlp,
        otel_disabled=get("OTEL_SDK_DISABLED").lower() == "true",
        warnings=tuple(warnings),
    )
    if errors:
        raise ConfigError(errors)
    return config
