//! The record-store port an adapter is handed instead of opening its own
//! `delonix-state::{Store,JsonStore}` directly (ADR-0044 D6).
//!
//! Lives in the foundation, not in a context's `ports.rs` (`delonix-compute::ports`),
//! because the trait names nothing but `T`: no `Container`, no `Vm`, no I/O type. An
//! adapter is already allowed to depend on the foundation (`arch_fitness.py`'s
//! `ALLOWED` table), which is what lets it stop depending on `delonix-state` — an
//! adapter itself — for this one thing.
//!
//! The shape below is not a first draft: it is what a spike proved against real
//! callers (`docs/discovery/56_P4_D6_STATE_REPOSITORY_SPIKE.md`), after the ADR's own
//! first sketch (`update(&self, id, f) -> Result<()>`) failed to compile against
//! `delonix-linux::wait_and_record`/`persist_stop`. Two corrections came out of that
//! spike, both load-bearing:
//!
//! 1. `update`'s closure returns `bool` (commit/abort), not `Result<()>` — those two
//!    callers use "the record does not describe this incarnation" as a normal abort,
//!    not an error, and the method returns the final `T` so the caller never needs a
//!    second, unlocked read to see what was written (which would reopen the race the
//!    port exists to close).
//! 2. `set` is mandatory. `update` alone 404s on a record that does not exist yet —
//!    the single biggest thing an adapter does with its store (first-time creation).
//!
//! Deliberately **not** object-safe (`update`'s `F` is generic): every consumer takes
//! `impl StateRepository<T>`, never `dyn`. Confirmed against every existing port this
//! codebase already threads through a use case (`WorkloadRuntime`, `RunHost`) — none
//! of them is consumed as `dyn` either.

use crate::Result;

/// A collection of `T`, keyed by id, with the flock-guarded read-modify-write a
/// daemonless multi-process engine needs (the CLI and `delonix-cri` mutate the same
/// record store concurrently by design, not by accident).
pub trait StateRepository<T> {
    /// The record named `id`.
    fn get(&self, id: &str) -> Result<T>;

    /// Every record in the collection.
    fn list(&self) -> Result<Vec<T>>;

    /// Persists `value` under `id`, creating the record if it does not exist yet.
    fn set(&self, id: &str, value: &T) -> Result<()>;

    /// Locks, re-reads, applies `f`, and writes back only if `f` returns `true` —
    /// `false` means "abort, and it is not an error" (a caller decided the record no
    /// longer describes what it expected to find). Returns the record as written (or
    /// as found, if `f` aborted), so the caller never needs a second, unlocked read.
    fn update<F>(&self, id: &str, f: F) -> Result<T>
    where
        F: FnOnce(&mut T) -> bool;

    /// Removes the record named `id`. Whether a missing `id` is an error is an
    /// implementation's own contract, not this trait's — `Store`'s and `JsonStore`'s
    /// existing behaviors disagree on this today, and callers already depend on
    /// which one they get.
    fn remove(&self, id: &str) -> Result<()>;
}
