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

    /// The owner for `uid` and the engine state root `root`: `"<uid>:<root>"`,
    /// the one spelling the engine and the privileged helper both build, so a
    /// stamp written by either is recognised by the other.
    pub fn of(uid: u32, root: &str) -> Self {
        Self(format!("{uid}:{root}"))
    }

    pub fn id(&self) -> &str {
        &self.0
    }

    /// `(uid, root)`, for an owner built by [`Owner::of`].
    pub fn parts(&self) -> Option<(u32, &str)> {
        let (uid, root) = self.0.split_once(':')?;
        Some((uid.parse().ok()?, root))
    }
}

/// A short, stable digest of a state root (FNV-1a, 64 bits, hex) — for the
/// backends whose stamp cannot hold the path itself (an LVM tag allows only a
/// few characters). It is NOT a security boundary: the uid is stamped beside
/// it in clear, and only the uid separates one user from another. Two roots of
/// the same user that collided would be that user confusing their own volumes.
pub fn root_digest(root: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The primary group of `uid`, from the password database; `uid` itself when
/// the user has no entry (a uid with no name is still a uid).
pub fn primary_gid(uid: u32) -> u32 {
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: `passwd` is a C struct of pointers and integers; all-zero is a
    // valid value for an out-parameter.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the duration of the call and `buf`
    // is as long as the length passed.
    let rc =
        unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr().cast(), buf.len(), &mut out) };
    if rc == 0 && !out.is_null() {
        pw.pw_gid
    } else {
        uid
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
    fn an_owner_of_a_uid_and_a_root_gives_both_back() {
        let o = Owner::of(1000, "/home/u/.local/share/delonix");
        assert_eq!(o.id(), "1000:/home/u/.local/share/delonix");
        assert_eq!(o.parts(), Some((1000, "/home/u/.local/share/delonix")));
        assert_eq!(Owner::new("nonsense").parts(), None);
        assert_eq!(Owner::new("x:/r").parts(), None);
    }

    #[test]
    fn the_root_digest_is_stable_and_distinguishes_roots() {
        // FNV-1a 64 of the empty string is the offset basis.
        assert_eq!(root_digest(""), "cbf29ce484222325");
        assert_eq!(root_digest("/a"), root_digest("/a"));
        assert_ne!(root_digest("/a"), root_digest("/b"));
        assert_eq!(root_digest("/home/u").len(), 16);
    }

    #[test]
    fn root_has_group_zero_and_an_unknown_uid_falls_back_to_itself() {
        assert_eq!(primary_gid(0), 0);
        assert_eq!(primary_gid(3_999_999_999), 3_999_999_999);
    }

    #[test]
    fn what_is_not_this_engines_stamp_decodes_to_none() {
        assert!(Stamp::decode(b"").is_none());
        assert!(Stamp::decode(b"{\"engine\":\"other\",\"pool\":\"p\",\"volume\":\"v\",\"owner\":\"o\",\"created_unix\":1}").is_none());
        assert!(Stamp::decode(b"{\"engine\":\"delonix\"").is_none());
    }
}
