# ADR 0003 — A two-stage image with only the binary at runtime

## Status
Accepted.

## Context
A Rust toolchain image is ~1.5 GB; shipping it to production is wasteful and
widens the attack surface. The runtime needs only the compiled binary, CA roots
for outbound HTTPS, and a way to answer the healthcheck.

## Decision
Build in `rust:<toolchain>-slim`, copying `Cargo.toml` first so the dependency
build layer is cached until it changes, then the source. Run on
`debian:bookworm-slim` with just `ca-certificates` and `curl` (the healthcheck),
as an unprivileged user (uid 10001), read-only root with a `/tmp` tmpfs. The
same two stages back the `Delonixfile` (for `delonix build`) and the
`Dockerfile` (for `docker build` / compose) — keep them in step.

## Consequences
The runtime image is tens of MB, not gigabytes, and contains no compiler or
source. The build stays glibc-based (slim → slim) so the dynamically linked
binary runs without surprises; a fully static musl build is a later option if a
distroless/scratch runtime is wanted.
