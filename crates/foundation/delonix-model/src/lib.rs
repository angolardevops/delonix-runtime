//! The shared model of the engine: what every layer can name without depending on
//! any mechanism (ADR-0040, foundation layer). Pure — no I/O, no process state.

pub use error::{Error, Result};

pub mod codes;
pub mod error;
pub mod exitcode;
pub mod names;
pub mod ports;
pub mod records;
pub mod secret;
pub mod typestate;
