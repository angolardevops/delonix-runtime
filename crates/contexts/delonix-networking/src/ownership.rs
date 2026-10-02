//! The engine's claim on an object it creates on a REMOTE provider — an
//! OPNsense alias or rule ([`crate::gateway`]), a vnet in a Proxmox
//! cluster's SDN ([`crate::segment`]).
//!
//! # Why a mark, and not the name
//!
//! Before this module the identity of a remote object was its name (an
//! alias, a zone, a vnet) or its description (an OPNsense rule). An
//! `ensure_*` that found the name already there answered `AlreadyPresent`,
//! and the teardown later deleted whatever carried that name: an operator's
//! hand-made rule with the same description was ADOPTED, then deleted
//! (audit 62, §3 and §6 P1). A name says what an object is called, not who
//! made it.
//!
//! [`OwnerMark`] is the "who": an opaque token the engine generates once per
//! declaring record (one `kind: NetworkGateway`, one `kind: NetworkZone`) and
//! persists BEFORE its first remote write. Where it goes depends on what the
//! provider has (ADR-0059 D1.5):
//!
//! * a **label** object the provider can attach — an OPNsense firewall
//!   category named `delonix-owner:<token>` ([`OwnerMark::label`]), put in
//!   every alias's and rule's `categories`, so the description stays the
//!   operator-readable text it was declared as;
//! * otherwise the object's own **free-text** field, as a trailing
//!   `[delonix-owner:<token>]` ([`OwnerMark::stamp`]) — a Proxmox SDN vnet's
//!   `alias`, its only one (PVE 9.2.2 has no comment on zones or vnets).
//!
//! It is
//!
//! * **immutable** — the token of a record never changes, and nothing in
//!   this engine rewrites the mark on an object;
//! * **distinguishable** — per record, not per engine: two stacks, or two
//!   engines on two hosts sharing one appliance, never read each other's
//!   objects as their own.
//!
//! The rule every provider follows with it: an object found under the
//! requested identity is this engine's only if it carries THIS mark; one
//! without it is refused ([`crate::Error`]'s `RemoteObjectNotOwned`) on
//! ensure, and left alone on remove ([`RemoveOutcome::NotOwned`]).
//!
//! # What a mark in a text field does not prove
//!
//! Anyone with write access to the provider can edit the field: delete the
//! mark (the engine then refuses to touch the object — fail closed), or copy
//! it onto another object (a deliberate hand-over; the engine cannot tell).
//! A provider object with no free-text field at all (a Proxmox SDN zone)
//! carries no mark: its ownership is the engine's own record that it created
//! it, which `cmd::network_zone` keeps and the provider cannot check.

use crate::error::{Error, Result};

/// The prefix of the tag written into an object's text field.
pub const MARK_PREFIX: &str = "[delonix-owner:";

/// The prefix of an owner LABEL (a category's name): the tag without its
/// brackets.
pub const LABEL_PREFIX: &str = "delonix-owner:";

/// The shortest and longest token accepted.
const TOKEN_LEN: std::ops::RangeInclusive<usize> = 8..=64;

/// A validated owner token (`[a-z0-9-]`, 8 to 64 characters) — the set every
/// provider's text field accepts (Proxmox's vnet `alias` pattern included).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerMark(String);

impl OwnerMark {
    /// Wraps a token read back from a record, refusing anything outside the
    /// alphabet — a token is written into a remote text field and must never
    /// carry a character that field would reinterpret.
    pub fn new(token: &str) -> Result<Self> {
        if TOKEN_LEN.contains(&token.len()) && token.bytes().all(is_token_byte) {
            Ok(Self(token.to_string()))
        } else {
            Err(Error::Engine(delonix_model::Error::Invalid(format!(
                "invalid owner token '{token}': 8 to 64 characters of [a-z0-9-]"
            ))))
        }
    }

    /// A fresh token from 16 random bytes (`dlx-` + 32 hex digits).
    pub fn from_random(bytes: &[u8; 16]) -> Self {
        let mut s = String::with_capacity(36);
        s.push_str("dlx-");
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        Self(s)
    }

    pub fn token(&self) -> &str {
        &self.0
    }

    /// `delonix-owner:<token>` — the name of the label object that carries
    /// this mark on a provider that has labels.
    pub fn label(&self) -> String {
        format!("{LABEL_PREFIX}{}", self.0)
    }

    /// Whose a label NAME says an object is; `None` when the name is not an
    /// owner label at all (an operator's own category).
    pub fn owner_of_label(&self, name: &str) -> Option<Owner> {
        let token = token_of_label(name)?;
        Some(if token == self.0 {
            Owner::Ours
        } else {
            Owner::Other(token)
        })
    }

    /// `[delonix-owner:<token>]`.
    pub fn tag(&self) -> String {
        format!("{MARK_PREFIX}{}]", self.0)
    }

    /// `text` with this mark appended — what the provider writes into the
    /// object's text field. Any mark already in `text` is dropped first, so a
    /// declared description that happens to carry one never stacks two.
    pub fn stamp(&self, text: &str) -> String {
        let (base, _) = split_mark(text);
        if base.is_empty() {
            self.tag()
        } else {
            format!("{base} {}", self.tag())
        }
    }

    /// Whose object `text` (an object's text field) says it is.
    pub fn owner_of(&self, text: &str) -> Owner {
        match split_mark(text).1 {
            Some(t) if t == self.0 => Owner::Ours,
            Some(t) => Owner::Other(t),
            None => Owner::Unmarked,
        }
    }
}

/// The token of an owner label name, if it is a well-formed one.
pub fn token_of_label(name: &str) -> Option<String> {
    let token = name.trim().strip_prefix(LABEL_PREFIX)?;
    (TOKEN_LEN.contains(&token.len()) && token.bytes().all(is_token_byte))
        .then(|| token.to_string())
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'
}

/// Who an object belongs to, read from its text field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// It carries this record's mark.
    Ours,
    /// It carries ANOTHER record's mark — another stack's, or another
    /// engine's sharing the provider.
    Other(String),
    /// No mark at all: made by hand, or by something else.
    Unmarked,
}

impl Owner {
    /// For a refusal message: whose it is, in words.
    pub fn describe(&self) -> String {
        match self {
            Owner::Ours => "this engine's".to_string(),
            Owner::Other(t) => format!("owned by another delonix record (mark {t})"),
            Owner::Unmarked => "not created by delonix (no owner mark)".to_string(),
        }
    }
}

/// `text` without its owner tag (trimmed), and the tag's token if there was
/// a well-formed one. A malformed tag is left in the text and reads as no
/// mark — never as someone's.
pub fn split_mark(text: &str) -> (String, Option<String>) {
    if let Some(start) = text.rfind(MARK_PREFIX) {
        let rest = &text[start + MARK_PREFIX.len()..];
        if let Some(end) = rest.find(']') {
            let token = &rest[..end];
            if TOKEN_LEN.contains(&token.len()) && token.bytes().all(is_token_byte) {
                let before = text[..start].trim_end();
                let after = rest[end + 1..].trim_start();
                let base = match (before.is_empty(), after.is_empty()) {
                    (_, true) => before.to_string(),
                    (true, false) => after.to_string(),
                    (false, false) => format!("{before} {after}"),
                };
                return (base.trim().to_string(), Some(token.to_string()));
            }
        }
    }
    (text.trim().to_string(), None)
}

/// What a `remove_*` on a remote provider did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// It was this engine's, and it is gone (staged, until the commit).
    Removed,
    /// Nothing under that identity — already gone.
    Absent,
    /// Something under that identity exists and is NOT this engine's; it was
    /// left untouched. The caller says so out loud: a teardown that skips an
    /// object is not a silent success.
    NotOwned(Owner),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark() -> OwnerMark {
        OwnerMark::new("dlx-0123456789abcdef").unwrap()
    }

    #[test]
    fn a_token_outside_the_alphabet_is_refused() {
        for bad in [
            "",
            "short",
            "UPPERCASE-token",
            "dlx-] injected",
            "dlx:colon12",
            &"a".repeat(65),
        ] {
            assert!(OwnerMark::new(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(OwnerMark::new("dlx-00000000").is_ok());
    }

    #[test]
    fn a_random_token_is_a_valid_one() {
        let m = OwnerMark::from_random(&[0xab; 16]);
        assert_eq!(m.token().len(), 36);
        assert_eq!(OwnerMark::new(m.token()).unwrap(), m);
    }

    #[test]
    fn stamp_then_owner_of_is_ours_and_the_base_text_survives() {
        let m = mark();
        let text = m.stamp("allow web");
        assert_eq!(text, "allow web [delonix-owner:dlx-0123456789abcdef]");
        assert_eq!(m.owner_of(&text), Owner::Ours);
        assert_eq!(split_mark(&text).0, "allow web");
        assert_eq!(m.stamp(""), m.tag());
    }

    #[test]
    fn stamping_twice_never_stacks_two_marks() {
        let m = mark();
        let other = OwnerMark::new("dlx-ffffffffffffffff").unwrap();
        let text = m.stamp(&other.stamp("x"));
        assert_eq!(text.matches(MARK_PREFIX).count(), 1, "{text}");
        assert_eq!(m.owner_of(&text), Owner::Ours);
    }

    #[test]
    fn a_hand_made_object_with_the_same_text_is_not_ours() {
        let m = mark();
        assert_eq!(m.owner_of("allow web"), Owner::Unmarked);
        let other = OwnerMark::new("dlx-ffffffffffffffff").unwrap();
        assert_eq!(
            m.owner_of(&other.stamp("allow web")),
            Owner::Other("dlx-ffffffffffffffff".into())
        );
    }

    #[test]
    fn a_malformed_tag_reads_as_no_mark_and_stays_in_the_text() {
        let m = mark();
        let text = "allow web [delonix-owner:BAD TOKEN]";
        assert_eq!(m.owner_of(text), Owner::Unmarked);
        assert_eq!(split_mark(text).0, text);
    }

    #[test]
    fn a_label_names_its_owner_and_an_operators_category_names_none() {
        let m = mark();
        assert_eq!(m.label(), "delonix-owner:dlx-0123456789abcdef");
        assert_eq!(m.owner_of_label(&m.label()), Some(Owner::Ours));
        assert_eq!(
            m.owner_of_label("delonix-owner:dlx-ffffffffffffffff"),
            Some(Owner::Other("dlx-ffffffffffffffff".into()))
        );
        assert_eq!(m.owner_of_label("web servers"), None);
        assert_eq!(m.owner_of_label("delonix-owner:BAD"), None);
    }

    #[test]
    fn a_tag_in_the_middle_is_found_and_removed() {
        let (base, token) = split_mark("a [delonix-owner:dlx-0123456789abcdef] b");
        assert_eq!(base, "a b");
        assert_eq!(token.as_deref(), Some("dlx-0123456789abcdef"));
    }
}
