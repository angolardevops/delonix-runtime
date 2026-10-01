"""Configuration from the environment, typed and validated once at startup.

A bad value stops the process before it listens, with every problem listed at
once (`server.main` prints them and exits 1). Variable names are the fields in
upper case (`APP_ENV`, `SHUTDOWN_TIMEOUT`, ...). The OpenTelemetry SDK reads
its own standard `OTEL_*` variables; they are not duplicated here.
"""

from __future__ import annotations

import re
from typing import Annotated, Literal
from urllib.parse import urlsplit

from pydantic import BeforeValidator, Field, SecretStr, model_validator
from pydantic_settings import BaseSettings, SettingsConfigDict

from .webhooks.signature import parse_secret

_DURATION = re.compile(r"^\s*(\d+(?:\.\d+)?)\s*(ms|s|m)?\s*$")
_UNIT_SECONDS = {"ms": 0.001, "s": 1.0, "m": 60.0, None: 1.0}


def _parse_duration(value: object) -> object:
    """`15`, `15s`, `500ms` and `1m` are all accepted; the result is seconds."""
    if isinstance(value, str):
        match = _DURATION.match(value)
        if match is None:
            raise ValueError("must be a duration such as 15s, 500ms or 1m")
        return float(match.group(1)) * _UNIT_SECONDS[match.group(2)]
    return value


Seconds = Annotated[float, BeforeValidator(_parse_duration), Field(ge=0)]


class Settings(BaseSettings):
    model_config = SettingsConfigDict(
        env_file=".env",
        env_file_encoding="utf-8",
        env_ignore_empty=True,  # `WEBHOOK_TARGET_URL=` in .env means "not set"
        extra="ignore",
        frozen=True,
    )

    app_env: Literal["development", "test", "production"] = "development"
    log_level: Literal["debug", "info", "warning", "error"] = "info"
    service_name: str = Field(
        default="__NAME__",
        min_length=1,
    )

    http_host: str = "0.0.0.0"  # noqa: S104 — a container listens on every interface
    port: int = Field(default=__PORT__, ge=1, le=65535)
    http_max_body_bytes: int = Field(default=1_048_576, ge=1)
    http_keepalive_timeout: Seconds = 5.0
    shutdown_timeout: Seconds = 15.0
    # Serve with readiness at 503 before closing the listener, so a load
    # balancer has time to stop routing here.
    drain_delay: Seconds = 0.0

    webhook_inbound_secret: SecretStr | None = None
    webhook_target_url: str | None = None
    webhook_target_secret: SecretStr | None = None

    @model_validator(mode="after")
    def _cross_field_rules(self) -> Settings:
        problems: list[str] = []
        if self.drain_delay >= self.shutdown_timeout > 0:
            problems.append("DRAIN_DELAY must be shorter than SHUTDOWN_TIMEOUT")
        if self.webhook_inbound_secret is not None:
            try:
                parse_secret(self.webhook_inbound_secret.get_secret_value())
            except ValueError as exc:
                problems.append(f"WEBHOOK_INBOUND_SECRET: {exc}")
        if self.webhook_target_url:
            parts = urlsplit(self.webhook_target_url)
            if parts.scheme not in ("http", "https") or not parts.netloc:
                problems.append("WEBHOOK_TARGET_URL: not an http(s) URL")
            elif self.app_env == "production" and parts.scheme != "https":
                # The development shortcut (plain http to a local receiver) is
                # refused where it would send signed events in clear text.
                problems.append("WEBHOOK_TARGET_URL: production sends events over https only")
            if self.webhook_target_secret is None:
                problems.append("WEBHOOK_TARGET_SECRET: required when WEBHOOK_TARGET_URL is set")
            else:
                try:
                    parse_secret(self.webhook_target_secret.get_secret_value())
                except ValueError as exc:
                    problems.append(f"WEBHOOK_TARGET_SECRET: {exc}")
        if problems:
            raise ValueError("; ".join(problems))
        return self

    @property
    def api_docs(self) -> bool:
        """Swagger UI and ReDoc are a development convenience, off in production."""
        return self.app_env != "production"

    @property
    def inbound_key(self) -> bytes | None:
        s = self.webhook_inbound_secret
        return parse_secret(s.get_secret_value()) if s is not None else None

    @property
    def target_key(self) -> bytes | None:
        s = self.webhook_target_secret
        return (
            parse_secret(s.get_secret_value())
            if s is not None and self.webhook_target_url
            else None
        )
