//! The Compute context (`compute.delonix.io`, ADR-0040): Container, Pod,
//! VirtualMachine and Workload. Today it holds the one run specification that the
//! entry points translate into; the use cases that execute it follow in later P2
//! slices.

mod notice;
pub mod pod;
pub mod preflight;
mod run_opts;

pub use notice::Notice;
pub use run_opts::RunOpts;
