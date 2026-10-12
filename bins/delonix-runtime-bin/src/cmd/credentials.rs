//! Service credentials on the CLI (ADR-0048 D4): the record's reference, the
//! `--credentials`/`--reveal` columns, and the JSON a script reads.
//!
//! The rules and the catalogue are `delonix_compute::credentials` (pure). This
//! module is where the reference is stored (an annotation on the record, so the
//! record holds the NAME of a secret and never its value), and where a value is
//! read — only under `--reveal`, the `secret inspect --reveal` precedent: a
//! listing copied into a ticket must not carry a working password.

use std::collections::BTreeMap;

use delonix_compute::credentials::{self as cr, CredentialsSpec, Resolved};
use serde::Serialize;

/// Where a workload's declared `credentials:` reference lives on its record.
/// The reconciler's comparable field has the same name and the same value.
pub(crate) const ANNOTATION: &str = "delonix.io/credentials";

/// The comparable field name in a plan.
pub(crate) const FIELD: &str = "credentials";

/// The one encoding of a declared block, used by the record AND by the plan —
/// two encodings would read as drift on every plan. JSON with the struct's
/// field order, so equal blocks encode equal.
pub(crate) fn encode(spec: &CredentialsSpec) -> String {
    serde_json::to_string(spec).unwrap_or_default()
}

/// Reads the block back from a record. An annotation that does not parse is
/// treated as absent: it can only come from a hand edit, and printing a login
/// from it would be guessing.
pub(crate) fn decode(raw: &str) -> Option<CredentialsSpec> {
    serde_json::from_str(raw).ok()
}

/// The declared block from a record's annotations.
pub(crate) fn declared_on(annotations: &BTreeMap<String, String>) -> Option<CredentialsSpec> {
    annotations.get(ANNOTATION).and_then(|raw| decode(raw))
}

/// Writes (or clears) the reference on a record from a plan's fields. Called
/// where the reconciler stamps ownership, so a create, an adoption and an
/// update all land it the same way, and a block removed from the manifest is
/// removed from the record.
pub(crate) fn record(
    annotations: &mut BTreeMap<String, String>,
    fields: &BTreeMap<String, String>,
) {
    match fields.get(FIELD) {
        Some(v) => {
            annotations.insert(ANNOTATION.into(), v.clone());
        }
        None => {
            annotations.remove(ANNOTATION);
        }
    }
}

/// What the listing and the JSON say about one workload.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct View {
    /// `declared`, `vendor-default`, `vendor-changed`, `no-login` or `none`.
    pub(crate) source: &'static str,
    pub(crate) user: Option<String>,
    /// The cell as printed: `(secret NAME)`, a hint, or — with `--reveal` —
    /// the value.
    pub(crate) password: Option<String>,
    /// The reference, for a script that reads the secret itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) secret: Option<cr::SecretKeyRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) note: Option<String>,
}

impl View {
    /// The two table cells. `-` for what no source gives.
    pub(crate) fn cells(&self) -> (String, String) {
        (
            self.user.clone().unwrap_or_else(|| "-".into()),
            self.password.clone().unwrap_or_else(|| "-".into()),
        )
    }
}

/// Builds the view. `read` returns a secret key's value; it is called only when
/// `reveal` is set, so a listing without `--reveal` never opens the vault.
pub(crate) fn view(
    resolved: Resolved<'_>,
    reveal: bool,
    read: &dyn Fn(&str, &str) -> std::result::Result<String, String>,
) -> View {
    let vendor_default = super::po::t("(vendor default — change it)");
    match resolved {
        Resolved::Declared(d) => View {
            source: "declared",
            user: d.user.clone(),
            password: d.password.as_ref().map(|p| {
                if reveal {
                    read(&p.secret, &p.key).unwrap_or_else(|e| {
                        super::po::tf(
                            "(secret {name}: not readable — {err})",
                            &[("name", &p.secret), ("err", &e)],
                        )
                    })
                } else {
                    super::po::tf("(secret {name})", &[("name", &p.secret)])
                }
            }),
            secret: d.password.clone(),
            note: d.note.clone(),
        },
        // A default that has not been changed IS the live password, so it is
        // treated like one: shown only under `--reveal`.
        Resolved::VendorDefault { user, password } => View {
            source: "vendor-default",
            user: Some(user),
            password: Some(if reveal {
                format!("{password} {vendor_default}")
            } else {
                vendor_default.to_string()
            }),
            secret: None,
            note: None,
        },
        Resolved::VendorChanged { user, env } => View {
            source: "vendor-changed",
            user: Some(user),
            password: Some(super::po::tf(
                "(changed — see the env: {var})",
                &[("var", env)],
            )),
            secret: None,
            note: None,
        },
        Resolved::VendorNoLogin => View {
            source: "no-login",
            user: None,
            password: Some(super::po::t("(no login)").to_string()),
            secret: None,
            note: None,
        },
        Resolved::Unknown => View {
            source: "none",
            user: None,
            password: None,
            secret: None,
            note: None,
        },
    }
}

/// Reads one key of a secret from this node's vault.
pub(crate) fn read_secret(name: &str, key: &str) -> std::result::Result<String, String> {
    let store = delonix_state::SecretStore::open_existing(super::util::state_root())
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "this node holds no secrets".to_string())?;
    let s = store.load(name).map_err(|e| e.to_string())?;
    s.data
        .get(key)
        .cloned()
        .ok_or_else(|| format!("no key '{key}' in secret '{name}'"))
}

/// The view of a container: its declared block, else the catalogue entry of its
/// image, judged against its environment.
pub(crate) fn of_container(c: &delonix_compute::Container, reveal: bool) -> View {
    let declared = declared_on(&c.annotations);
    view(
        cr::resolve(declared.as_ref(), cr::container_default(&c.image), &c.env),
        reveal,
        &read_secret,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cr::SecretKeyRef;

    fn declared() -> CredentialsSpec {
        CredentialsSpec {
            user: Some("ops".into()),
            password: Some(SecretKeyRef {
                secret: "grafana-admin".into(),
                key: "password".into(),
            }),
            note: Some("created at first login".into()),
        }
    }

    #[test]
    fn the_record_and_the_plan_share_one_encoding() {
        let d = declared();
        let raw = encode(&d);
        assert_eq!(decode(&raw), Some(d.clone()));
        // A hand-edited annotation that does not parse prints nothing.
        assert_eq!(decode("{not json"), None);
        // The record follows the plan, both ways.
        let mut ann = BTreeMap::new();
        let mut fields = BTreeMap::new();
        fields.insert(FIELD.to_string(), raw.clone());
        record(&mut ann, &fields);
        assert_eq!(ann.get(ANNOTATION), Some(&raw));
        record(&mut ann, &BTreeMap::new());
        assert!(!ann.contains_key(ANNOTATION));
    }

    #[test]
    fn a_password_is_read_only_under_reveal() {
        let d = declared();
        let calls = std::cell::Cell::new(0);
        let read = |s: &str, k: &str| {
            calls.set(calls.get() + 1);
            Ok(format!("value-of-{s}-{k}"))
        };
        let v = view(Resolved::Declared(&d), false, &read);
        assert_eq!(
            calls.get(),
            0,
            "a listing without --reveal opened the vault"
        );
        assert_eq!(v.cells(), ("ops".into(), "(secret grafana-admin)".into()));
        assert_eq!(v.source, "declared");
        let v = view(Resolved::Declared(&d), true, &read);
        assert_eq!(calls.get(), 1);
        assert_eq!(v.cells().1, "value-of-grafana-admin-password");
        // An unreadable secret is said, not a failed listing.
        let v = view(Resolved::Declared(&d), true, &|_, _| Err("gone".into()));
        assert!(v.cells().1.contains("gone"), "{:?}", v.cells());
    }

    #[test]
    fn the_three_sources_print_as_the_adr_says() {
        let never = |_: &str, _: &str| -> std::result::Result<String, String> {
            panic!("no secret to read")
        };
        let d = view(
            Resolved::VendorDefault {
                user: "admin".into(),
                password: "admin",
            },
            false,
            &never,
        );
        assert_eq!(
            d.cells(),
            ("admin".into(), "(vendor default — change it)".into())
        );
        let d = view(
            Resolved::VendorDefault {
                user: "admin".into(),
                password: "admin",
            },
            true,
            &never,
        );
        assert_eq!(d.cells().1, "admin (vendor default — change it)");
        let c = view(
            Resolved::VendorChanged {
                user: "admin".into(),
                env: "GF_SECURITY_ADMIN_PASSWORD",
            },
            true,
            &never,
        );
        assert!(c.cells().1.starts_with("(changed — see the env"), "{:?}", c);
        assert!(
            !c.cells().1.contains("admin"),
            "a changed default is not printed"
        );
        let n = view(Resolved::Unknown, true, &never);
        assert_eq!(n.cells(), ("-".into(), "-".into()));
        assert_eq!(n.source, "none");
    }
}
