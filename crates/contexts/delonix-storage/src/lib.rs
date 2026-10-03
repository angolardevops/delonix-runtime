//! The storage context (`storage.delonix.io`, ADR-0040 D2.2; ADR-0067): the
//! port a storage-pool driver implements, the registry of drivers by name,
//! the administrator's allowlist and the owner stamp a pool volume carries.
//!
//! In the shape `delonix-networking` has had since ADR-0059 D7: the drivers
//! depend on this context, never on each other, and the composition root
//! registers them. The `dir` driver lives in `delonix-volume`; the block
//! drivers (btrfs, ZFS, LVM-thin) arrive as provider crates, one per backend.
//!
//! What the port carries today is what has a caller (ADR-0067 P0): probe,
//! allocate, release and usage. Resize, snapshot, rollback and clone enter
//! with the first driver that serves them, not before.

pub mod allowlist;
mod error;
pub mod ownership;
pub mod pool;
pub mod registry;

pub use error::{Error, Result};
