//! The stamp a pool volume carries ON the object itself (ADR-0067 D5).
//!
//! A pool is shared ground: the administrator made it, other tools and other
//! engine roots may use it. So a driver never decides that an object is this
//! engine's by its name. It reads the stamp — a file beside the data in a
//! directory pool, a tag or a user property in the block drivers — and only an
//! object stamped for the same owner is adopted or released. The stamp holds
//! references, never a credential.

use serde::{Deserialize, Serialize};

/// The stamp's file name in the pools that are directory trees.
pub const STAMP_FILE: &str = ".delonix-volume.json";

/// Who an allocation is for: one engine state root of one user.
///
/// Two roots of the same user may use the same pool (a test root beside the
/// real one), and a volume name means nothing across them. The composition
/// root builds this; the context does not know what a state root is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner(String);

impl Owner {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn id(&self) -> &str {
        &self.0
    }
}

/// What is written on the object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    /// Always `"delonix"`: what tells this stamp from another tool's file.
    pub engine: String,
    pub pool: String,
    pub volume: String,
    pub owner: String,
    pub created_unix: u64,
}

impl Stamp {
    pub fn new(pool: &str, volume: &str, owner: &Owner, created_unix: u64) -> Self {
        Self {
            engine: "delonix".into(),
            pool: pool.into(),
            volume: volume.into(),
            owner: owner.id().into(),
            created_unix,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        // A struct of strings and one integer always serialises.
        serde_json::to_vec_pretty(self).unwrap_or_default()
    }

    /// `None` for anything that is not a stamp of this engine — another tool's
    /// file, a truncated write. Never an error: «not ours» is the answer.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let s: Self = serde_json::from_slice(bytes).ok()?;
        (s.engine == "delonix").then_some(s)
    }

    /// Is this the stamp of `volume` in `pool`, for `owner`?
    pub fn is_for(&self, pool: &str, volume: &str, owner: &Owner) -> bool {
        self.pool == pool && self.volume == volume && self.owner == owner.id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_round_trips_and_names_its_owner() {
        let me = Owner::new("1000:/home/u/.local/share/delonix");
        let s = Stamp::new("media", "db", &me, 7);
        let back = Stamp::decode(&s.encode()).unwrap();
        assert_eq!(back, s);
        assert!(back.is_for("media", "db", &me));
        assert!(!back.is_for("media", "db", &Owner::new("1000:/tmp/other")));
        assert!(!back.is_for("media", "web", &me));
        assert!(!back.is_for("fast", "db", &me));
    }

    #[test]
    fn what_is_not_this_engines_stamp_decodes_to_none() {
        assert!(Stamp::decode(b"").is_none());
        assert!(Stamp::decode(b"{\"engine\":\"other\",\"pool\":\"p\",\"volume\":\"v\",\"owner\":\"o\",\"created_unix\":1}").is_none());
        assert!(Stamp::decode(b"{\"engine\":\"delonix\"").is_none());
    }
}
