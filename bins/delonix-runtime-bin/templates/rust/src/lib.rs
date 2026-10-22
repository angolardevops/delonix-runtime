//! __NAME__ — a JSON-API reference service.
//!
//! The crate is a library plus a thin binary (`src/main.rs` is the composition
//! root). Keeping the logic in the library is what lets the integration tests
//! under `tests/` drive the same code the binary runs.
//!
//! # The dependency direction (enforced by `tests/architecture.rs`)
//!
//! The use cases (`notes`) know their ports (`Store`, `Publisher`) and nothing
//! else — not HTTP, not telemetry, not a storage or cache technology. Adapters
//! (`memstore`, `webhook`, `cache`, `messaging`) implement the ports. The
//! transport (`httpapi`) reaches the use cases through a trait, never a
//! concrete adapter. The cache and tracing wrap the use cases from the OUTSIDE
//! (`cache::CachedNotes`, `telemetry::TracedNotes`), so `notes` imports neither.

pub mod cache;
pub mod config;
pub mod health;
pub mod httpapi;
pub mod logging;
pub mod memstore;
pub mod messaging;
pub mod notes;
pub mod telemetry;
pub mod webhook;
