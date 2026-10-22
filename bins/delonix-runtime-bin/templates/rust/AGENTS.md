# Notes for automated agents

This is a Rust (axum + tokio) JSON-API service. Build and verify with:

```sh
cargo build
make check            # fmt --check, clippy -D warnings, cargo test
```

Everything is offline: tests use in-process fakes for Redis and RabbitMQ, so no
container or broker is required.

Ground rules before you change anything:

- **The route contract is a test.** If you add/remove an HTTP route, update
  `api/openapi.yaml` to match or `tests/openapi.rs` fails.
- **The dependency direction is a test.** `notes` must not import the transport,
  a framework, or a storage/cache/messaging adapter; `tests/architecture.rs`
  enforces it. See `ARCHITECTURE.md`.
- **Secrets never get logged.** Read them from the environment; log sensitive
  values through `logging::redact`.
- **The broker glue is behind the `amqp` feature.** Keep testable logic out of
  the feature so `cargo test` stays broker-free.
- **Composition happens only in `main.rs`.** Add a port as a trait next to the
  code that needs it, put its adapter in its own module, wire it in `main`.
