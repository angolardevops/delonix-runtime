//! The Compute context (`compute.delonix.io`, ADR-0040): Container, Pod,
//! VirtualMachine and Workload. Today it holds the one run specification that the
//! entry points translate into; the use cases that execute it follow in later P2
//! slices.

pub mod capability;
pub mod capability_host;
pub mod launch;
pub mod network;
mod notice;
pub mod pod;
pub mod ports;
pub mod preflight;
mod record;
pub mod run;
mod run_opts;
pub mod workload_net;

pub use notice::Notice;
pub use record::*;
pub use run_opts::RunOpts;
