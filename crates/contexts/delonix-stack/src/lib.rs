//! The Stack context (`core.delonix.io`): the table of Kinds, the reconciler that
//! plans a manifest against what exists, and the revision history of an apply
//! (ADR-0040, contexts layer). Planning is pure; nothing here opens a store of a
//! concrete resource or runs a command.

pub mod condition;
pub mod kinds;
pub mod reconcile;
pub mod revision;
