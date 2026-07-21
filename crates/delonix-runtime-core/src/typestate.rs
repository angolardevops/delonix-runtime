//! Typestate of a container's lifecycle (Sprint 5 — Damas: *correctness by
//! construction*). The states are **types**, and the illegal transitions **do not
//! compile** — instead of being caught (or not) at runtime by a `match` over
//! a [`Status`](crate::Status).
//!
//! The model: `Created → Running → Stopped → (restart) Created`. Each transition
//! **consumes** the previous phase, so an obsolete phase cannot be reused.
//!
//! ```
//! use delonix_runtime_core::typestate::Phase;
//! use delonix_runtime_core::Status;
//!
//! let created = Phase::new("abc123");            // Phase<Created>
//! assert_eq!(created.status(), Status::Created);
//! let running = created.start(4242);             // Created → Running
//! assert_eq!(running.pid(), 4242);
//! let stopped = running.stop(0);                 // Running → Stopped
//! assert_eq!(stopped.status(), Status::Stopped);
//! let _again = stopped.restart();                // Stopped → Created (reuses the id)
//! ```
//!
//! The invalid transitions are **compilation errors**, not runtime bugs:
//!
//! ```compile_fail
//! use delonix_runtime_core::typestate::Phase;
//! let created = Phase::new("abc123"); // Phase<Created>
//! created.stop(0);                    // ERROR: `stop` only exists on Phase<Running>
//! ```
//!
//! ```compile_fail
//! use delonix_runtime_core::typestate::Phase;
//! let running = Phase::new("abc123").start(1); // Phase<Running>
//! running.start(2);                            // ERROR: `start` only exists on Phase<Created>
//! ```
//!
//! ```compile_fail
//! use delonix_runtime_core::typestate::Phase;
//! let created = Phase::new("abc123");
//! let _running = created.start(1);
//! created.start(2);                  // ERROR: `created` was consumed by the 1st transition
//! ```

use crate::Status;
use std::marker::PhantomData;

/// State: created, still without a `pid`.
pub struct Created;
/// State: running, with a live init `pid`.
pub struct Running;
/// State: terminated, with an exit code.
pub struct Stopped;

/// A **typed** phase of the lifecycle. The `S` parameter is the current state; the
/// transition methods only exist in the phases where they are valid.
pub struct Phase<S> {
    id: String,
    pid: Option<i32>,
    code: Option<i32>,
    _state: PhantomData<S>,
}

impl<S> Phase<S> {
    /// The container's identifier (stable across transitions).
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Phase<Created> {
    /// Creates a new phase in the `Created` state.
    pub fn new(id: impl Into<String>) -> Self {
        Phase {
            id: id.into(),
            pid: None,
            code: None,
            _state: PhantomData,
        }
    }
    /// `Created → Running`. Consumes the phase (the previous one becomes unusable).
    pub fn start(self, pid: i32) -> Phase<Running> {
        Phase {
            id: self.id,
            pid: Some(pid),
            code: None,
            _state: PhantomData,
        }
    }
    /// The corresponding [`Status`](crate::Status).
    pub fn status(&self) -> Status {
        Status::Created
    }
}

impl Phase<Running> {
    /// The init's `pid` (exists only in the `Running` state).
    pub fn pid(&self) -> i32 {
        self.pid.expect("Phase<Running> always has a pid")
    }
    /// `Running → Stopped`, storing the exit code.
    pub fn stop(self, code: i32) -> Phase<Stopped> {
        Phase {
            id: self.id,
            pid: None,
            code: Some(code),
            _state: PhantomData,
        }
    }
    /// The corresponding [`Status`](crate::Status).
    pub fn status(&self) -> Status {
        Status::Running
    }
}

impl Phase<Stopped> {
    /// The exit code (exists only in the `Stopped` state).
    pub fn exit_code(&self) -> i32 {
        self.code.expect("Phase<Stopped> always has an exit code")
    }
    /// `Stopped → Created` (restart): reuses the id, clears pid/code.
    pub fn restart(self) -> Phase<Created> {
        Phase {
            id: self.id,
            pid: None,
            code: None,
            _state: PhantomData,
        }
    }
    /// The corresponding [`Status`](crate::Status): code 0 → Stopped, ≠0 → Failed.
    pub fn status(&self) -> Status {
        match self.code.unwrap_or(0) {
            0 => Status::Stopped,
            n => Status::Failed(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_lifecycle_preserves_id_and_maps_status() {
        let c = Phase::new("deadbeef");
        assert_eq!(c.id(), "deadbeef");
        assert_eq!(c.status(), Status::Created);

        let r = c.start(99);
        assert_eq!(r.id(), "deadbeef");
        assert_eq!(r.pid(), 99);
        assert_eq!(r.status(), Status::Running);

        let s = r.stop(137);
        assert_eq!(s.id(), "deadbeef");
        assert_eq!(s.exit_code(), 137);
        assert_eq!(s.status(), Status::Failed(137));

        let again = s.restart();
        assert_eq!(again.id(), "deadbeef");
        assert_eq!(again.status(), Status::Created);
    }
}
