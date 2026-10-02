"""The notes capability: rules and use cases (`domain`, `ports`, `service`),
the in-memory adapter (`memory`) and the HTTP router (`api`).

`domain`, `ports` and `service` import only the standard library and each
other — `tests/test_architecture.py` fails the build if they reach for the web
framework, an HTTP client or telemetry.
"""
