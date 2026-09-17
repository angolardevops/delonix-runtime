//! `delonix-state` — the engine's persisted state (ADR-0040 D2.3): JSON records
//! behind `flock`, atomic writes, and the secret vault encrypted at rest.
//!
//! It came out of `delonix-runtime-core`, which was the sink of everything: the
//! record TYPES stay in the foundation, the files that hold them live here.

pub mod cred_vault;
mod error;
pub mod secret;
mod store;

pub use error::{Error, Result};
pub use secret::{Secret, SecretStore};
pub use store::{write_atomic, write_atomic_mode, write_private_temp, JsonStore, Store};
