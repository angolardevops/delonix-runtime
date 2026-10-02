"""Cross-cutting concerns shared by every capability: the HTTP error shape
and strict JSON parsing, request context, readiness, logging and telemetry.

`core` knows nothing about notes or webhooks (tests/test_architecture.py
fails if it ever imports them).
"""
