//! `delonix-node` — the node context (ADR-0040 D2.2): the node's event trail, its
//! virtualisation and host checks, the local-socket peer credentials, the dispatch
//! contract between `delonix` and its sibling servers, and the host/process
//! questions everything else asks.

pub mod dispatch;
pub mod events;
mod host;
pub mod peer_cred;
pub mod virt;

pub use host::{
    fmt_local_ts, generate_id, in_initial_userns, initial_uid_map, is_alive, is_rootless, now_unix,
    proc_starttime, safe_to_signal, self_bin,
};
