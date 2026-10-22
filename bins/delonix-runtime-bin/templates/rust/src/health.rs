//! Tracks whether the process should receive traffic.
//!
//! Liveness only says the process answers. Readiness moves through three
//! states: starting (not listening yet), ready, and draining (a shutdown
//! signal arrived — finish what is in flight, take nothing new). A load
//! balancer that polls readiness stops routing here before the listener closes.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Starting,
    Ready,
    Draining,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Ready => "ready",
            State::Draining => "draining",
        }
    }
}

/// Safe for concurrent use. Starts in `Starting`.
#[derive(Clone, Default)]
pub struct Readiness {
    state: Arc<AtomicU8>,
}

const STARTING: u8 = 0;
const READY: u8 = 1;
const DRAINING: u8 = 2;

impl Readiness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the service as able to take traffic (only from `Starting`).
    pub fn set_ready(&self) {
        let _ = self
            .state
            .compare_exchange(STARTING, READY, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// One-way: nothing brings a draining process back.
    pub fn set_draining(&self) {
        self.state.store(DRAINING, Ordering::SeqCst);
    }

    pub fn state(&self) -> State {
        match self.state.load(Ordering::SeqCst) {
            READY => State::Ready,
            DRAINING => State::Draining,
            _ => State::Starting,
        }
    }
}
