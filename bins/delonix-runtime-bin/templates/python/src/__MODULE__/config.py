"""Typed settings, loaded from the environment (12-factor)."""

from __future__ import annotations

from functools import lru_cache

from pydantic_settings import BaseSettings, SettingsConfigDict


class Settings(BaseSettings):
    model_config = SettingsConfigDict(env_prefix="APP_", env_file=".env")

    app_name: str = "__NAME__"
    env: str = "development"


@lru_cache
def get_settings() -> Settings:
    return Settings()
