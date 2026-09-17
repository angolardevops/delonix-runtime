//! The engine's observability (ADR-0040 P3): structured logging, OpenTelemetry
//! spans exported over OTLP, and the Prometheus metrics the servers share.
//!
//! It left `delonix-runtime-core`, the foundation crate, because a foundation
//! must not carry a runtime or an exporter: every crate that needed a `Container`
//! type was compiling an OTLP client with it.

pub mod metrics;
pub mod telemetry;
