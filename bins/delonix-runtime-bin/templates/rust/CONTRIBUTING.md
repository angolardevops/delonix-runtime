# Contributing

## The loop

```sh
make check        # cargo fmt --check + cargo clippy -D warnings + cargo test
```

Everything runs offline: the tests use in-process fakes for Redis and RabbitMQ,
so no service has to be up. `make check` is exactly what CI runs.

## Rules that are tests

- **The contract** — add or remove a route and `tests/openapi.rs` fails until
  `api/openapi.yaml` matches. Update both together.
- **The dependency direction** — `tests/architecture.rs` fails if `notes`
  reaches for the transport, a framework, or a storage/cache/messaging adapter,
  or if an adapter calls the transport. See `ARCHITECTURE.md`.

## Commit messages

This project follows [Conventional Commits](https://www.conventionalcommits.org):
`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`, with a `!` or a
`BREAKING CHANGE:` footer for anything that breaks the API. The CI is the same
gate on GitHub and GitLab (`.github/workflows/ci.yml` and `.gitlab-ci.yml`):
`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`.

## Conventions

- Keep business rules in `notes`; keep HTTP concerns in `httpapi`.
- A new port is a trait in the module that needs it; its adapter lives in its
  own module and is wired only in `main.rs`.
- Secrets are read from the environment, never logged — log through
  `logging::redact` for anything that might be sensitive.
- The broker glue is behind the `amqp` feature; keep unit-testable logic out of
  it so the default test lane stays broker-free.
