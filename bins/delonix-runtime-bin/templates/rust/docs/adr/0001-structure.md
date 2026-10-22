# ADR 0001 — Hexagonal structure with the direction enforced as a test

## Status
Accepted.

## Context
A reference service should be easy to grow without the business rules slowly
acquiring dependencies on HTTP, a database driver, or a telemetry SDK. Comments
and good intentions do not hold that line over time.

## Decision
Put the use cases in `notes` with two ports (`Store`, `Publisher`) and nothing
else. Everything technological is an adapter at the edge, wired only in
`main.rs`. Caching and tracing are decorators that wrap the use cases from the
outside (`cache::CachedNotes`, `telemetry::TracedNotes`), so `notes` imports
neither. Encode the allowed directions in `tests/architecture.rs` so a wrong-way
import fails `cargo test`, not a review.

## Consequences
Swapping the in-memory store for a database, or adding a second transport, is a
new adapter and a line in `main.rs` — the use cases and their tests do not move.
The cost is a little ceremony (a trait per port); the architecture test keeps it
honest.
