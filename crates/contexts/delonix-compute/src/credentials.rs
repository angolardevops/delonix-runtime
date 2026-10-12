//! Service credentials (ADR-0048 D4): what a workload's login is, and where it
//! comes from.
//!
//! The first thing anyone does with Grafana, Zabbix or an appliance is look up
//! the login, and the engine holds it — in a `kind: Secret`, in an env var, or as
//! the vendor's documented default — without ever saying so. Three sources, in
//! this order, and never a guess:
//!
//! 1. a declared `credentials:` block always wins;
//! 2. otherwise an embedded catalogue of PUBLIC images and their documented
//!    defaults — and when the container overrides the default through the
//!    vendor's own env var, the default is NOT printed, because it is now wrong;
//! 3. otherwise nothing: the listing prints `-`.
//!
//! The record keeps the REFERENCE to the secret, never the value: the vault
//! exists so that the record does not hold a password.
//!
//! PURE: the types, the validation, the catalogue and the resolution. Reading a
//! secret's value (`--reveal`) and printing are the CLI's.

use serde::{Deserialize, Serialize};

/// The `credentials:` block of a `Container` or a `VirtualMachine`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialsSpec {
    /// The login name. Not a secret: it is printed as is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Where the password lives: a key of a `kind: Secret`. Never the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<SecretKeyRef>,
    /// Free text shown next to the login (`created at first login`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A key of a `kind: Secret`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretKeyRef {
    /// The secret's name.
    pub secret: String,
    /// The key inside it.
    pub key: String,
}

/// Longest `user`/`note` accepted. A listing cell, not a document.
const MAX_TEXT: usize = 256;

fn printable(field: &str, v: &str) -> Result<(), String> {
    if v.trim().is_empty() {
        return Err(format!("credentials: `{field}` is empty"));
    }
    if v.len() > MAX_TEXT {
        return Err(format!(
            "credentials: `{field}` is longer than {MAX_TEXT} bytes"
        ));
    }
    // A control character would rewrite the terminal of whoever reads the
    // listing, and a newline would split one row into two.
    if v.chars().any(char::is_control) {
        return Err(format!(
            "credentials: `{field}` contains a control character"
        ));
    }
    Ok(())
}

impl CredentialsSpec {
    /// Refuses what would print wrong or name nothing. The secret is checked by
    /// NAME only: whether it exists is a question for the moment it is read, and
    /// a manifest may declare the Secret in the same apply.
    pub fn validate(&self) -> Result<(), String> {
        if self.user.is_none() && self.password.is_none() {
            return Err(
                "credentials: declare `user`, `password` or both — a block with neither names no login"
                    .into(),
            );
        }
        if let Some(u) = &self.user {
            printable("user", u)?;
        }
        if let Some(n) = &self.note {
            printable("note", n)?;
        }
        if let Some(p) = &self.password {
            if !delonix_model::secret::valid_name(&p.secret) {
                return Err(format!(
                    "credentials: `password.secret` '{}' is not a secret name ([a-z0-9._-], at most 64)",
                    p.secret
                ));
            }
            if !delonix_model::secret::valid_env_key(&p.key) {
                return Err(format!(
                    "credentials: `password.key` '{}' is not a secret key ([A-Za-z_][A-Za-z0-9_]*)",
                    p.key
                ));
            }
        }
        Ok(())
    }
}

/// What a catalogued image ships with.
#[derive(Debug, PartialEq, Eq)]
pub enum DefaultLogin {
    /// A documented account. `user_env`/`password_env` are the vendor's own
    /// variables for overriding it; when the container sets one, the default
    /// is no longer the truth.
    Account {
        user: &'static str,
        password: &'static str,
        user_env: Option<&'static str>,
        password_env: &'static [&'static str],
    },
    /// The image serves without a login at all.
    NoLogin,
}

/// One catalogue entry: the image repositories it covers and their login.
#[derive(Debug)]
pub struct VendorEntry {
    pub images: &'static [&'static str],
    pub login: DefaultLogin,
}

/// Public container images and the login their documentation gives. Names no
/// platform and no consumer: these are the vendors' own documented defaults.
pub const CONTAINER_CATALOGUE: &[VendorEntry] = &[
    VendorEntry {
        images: &[
            "grafana/grafana",
            "grafana/grafana-oss",
            "grafana/grafana-enterprise",
        ],
        login: DefaultLogin::Account {
            user: "admin",
            password: "admin",
            user_env: Some("GF_SECURITY_ADMIN_USER"),
            password_env: &[
                "GF_SECURITY_ADMIN_PASSWORD",
                "GF_SECURITY_ADMIN_PASSWORD__FILE",
            ],
        },
    },
    VendorEntry {
        images: &[
            "zabbix/zabbix-web-nginx-pgsql",
            "zabbix/zabbix-web-nginx-mysql",
            "zabbix/zabbix-web-apache-pgsql",
            "zabbix/zabbix-web-apache-mysql",
        ],
        // Zabbix's web login is changed in the UI, not by an env var: there is
        // no override to detect, which the printed hint says.
        login: DefaultLogin::Account {
            user: "Admin",
            password: "zabbix",
            user_env: None,
            password_env: &[],
        },
    },
    VendorEntry {
        images: &["prom/prometheus"],
        login: DefaultLogin::NoLogin,
    },
    VendorEntry {
        images: &["grafana/loki"],
        login: DefaultLogin::NoLogin,
    },
];

/// The VM images this repository builds (`scripts/appliances/README.md`, «known,
/// public» passwords), keyed by the name they are imported under.
pub const VM_CATALOGUE: &[VendorEntry] = &[
    VendorEntry {
        images: &["opnsense"],
        login: DefaultLogin::Account {
            user: "root",
            password: "opnsense",
            user_env: None,
            password_env: &[],
        },
    },
    VendorEntry {
        images: &[
            "proxmox-ve",
            "proxmox-backup-server",
            "proxmox-mail-gateway",
            "proxmox-datacenter-manager",
        ],
        login: DefaultLogin::Account {
            user: "root",
            password: "delonix-admin",
            user_env: None,
            password_env: &[],
        },
    },
    VendorEntry {
        images: &["truenas-scale"],
        login: DefaultLogin::Account {
            user: "truenas_admin",
            password: "delonix-admin",
            user_env: None,
            password_env: &[],
        },
    },
    VendorEntry {
        images: &["glpi"],
        login: DefaultLogin::Account {
            user: "glpi",
            password: "delonix-admin",
            user_env: None,
            password_env: &[],
        },
    },
];

/// The repository part of an image reference: no registry, no `library/`, no
/// tag, no digest. `docker.io/grafana/grafana:11.2@sha256:…` → `grafana/grafana`,
/// `nginx` → `nginx`, `localhost:5000/grafana/grafana` → `grafana/grafana`.
pub fn image_repository(image: &str) -> String {
    let no_digest = image.split('@').next().unwrap_or(image);
    // A tag is a `:` after the last `/`; a `:` before it is a registry port.
    let no_tag = match no_digest.rfind(':') {
        Some(i) if !no_digest[i..].contains('/') => &no_digest[..i],
        _ => no_digest,
    };
    let mut parts: Vec<&str> = no_tag.split('/').collect();
    if parts.len() > 1 {
        let first = parts[0];
        if first.contains('.') || first.contains(':') || first == "localhost" {
            parts.remove(0);
        }
    }
    if parts.len() > 1 && parts[0] == "library" {
        parts.remove(0);
    }
    parts.join("/")
}

/// The catalogue entry for a container image, if the image is catalogued.
pub fn container_default(image: &str) -> Option<&'static VendorEntry> {
    let repo = image_repository(image);
    CONTAINER_CATALOGUE
        .iter()
        .find(|e| e.images.contains(&repo.as_str()))
}

/// The catalogue entry for a VM image name as the image store keeps it
/// (`proxmox-ve:9.2`), if catalogued.
pub fn vm_default(image_name: &str) -> Option<&'static VendorEntry> {
    let stem = image_name.split(':').next().unwrap_or(image_name);
    VM_CATALOGUE.iter().find(|e| e.images.contains(&stem))
}

/// Where a workload's login comes from, after the three rules.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved<'a> {
    /// Rule 1: declared on the workload.
    Declared(&'a CredentialsSpec),
    /// Rule 2: the catalogue's default, not overridden.
    VendorDefault {
        user: String,
        password: &'static str,
    },
    /// Rule 2, overridden: the container sets the vendor's own variable, so the
    /// default would be wrong. `env` names the variable.
    VendorChanged { user: String, env: &'static str },
    /// The catalogued image serves without a login.
    VendorNoLogin,
    /// Rule 3: no source. Printed as `-`, never invented.
    Unknown,
}

/// Applies the three rules. `env` is the workload's `KEY=VALUE` environment
/// (empty for a VM).
pub fn resolve<'a>(
    declared: Option<&'a CredentialsSpec>,
    entry: Option<&'static VendorEntry>,
    env: &[String],
) -> Resolved<'a> {
    if let Some(d) = declared {
        return Resolved::Declared(d);
    }
    let Some(entry) = entry else {
        return Resolved::Unknown;
    };
    let value_of = |var: &str| {
        env.iter().find_map(|kv| {
            kv.split_once('=')
                .filter(|(k, _)| *k == var)
                .map(|(_, v)| v.to_string())
        })
    };
    match &entry.login {
        DefaultLogin::NoLogin => Resolved::VendorNoLogin,
        DefaultLogin::Account {
            user,
            password,
            user_env,
            password_env,
        } => {
            let user = user_env
                .and_then(value_of)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| user.to_string());
            match password_env.iter().find(|v| value_of(v).is_some()) {
                Some(var) => Resolved::VendorChanged { user, env: var },
                None => Resolved::VendorDefault { user, password },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(user: Option<&str>, secret: Option<(&str, &str)>) -> CredentialsSpec {
        CredentialsSpec {
            user: user.map(String::from),
            password: secret.map(|(s, k)| SecretKeyRef {
                secret: s.into(),
                key: k.into(),
            }),
            note: None,
        }
    }

    #[test]
    fn the_repository_is_found_whatever_the_reference_looks_like() {
        for r in [
            "grafana/grafana",
            "grafana/grafana:11.2.0",
            "docker.io/grafana/grafana:latest",
            "localhost:5000/grafana/grafana:1",
            "ghcr.io/grafana/grafana@sha256:abc",
            "grafana/grafana:11@sha256:abc",
        ] {
            assert_eq!(image_repository(r), "grafana/grafana", "{r}");
        }
        assert_eq!(image_repository("docker.io/library/nginx:1"), "nginx");
        assert_eq!(image_repository("alpine"), "alpine");
    }

    #[test]
    fn a_declared_login_wins_over_the_catalogue() {
        let d = decl(Some("ops"), Some(("grafana-admin", "password")));
        let entry = container_default("grafana/grafana");
        assert_eq!(resolve(Some(&d), entry, &[]), Resolved::Declared(&d));
    }

    #[test]
    fn a_catalogue_default_is_printed_only_while_it_is_true() {
        let entry = container_default("docker.io/grafana/grafana:11.2.0");
        assert_eq!(
            resolve(None, entry, &[]),
            Resolved::VendorDefault {
                user: "admin".into(),
                password: "admin"
            }
        );
        // The vendor's own variable set: the default is now wrong.
        let env = vec!["GF_SECURITY_ADMIN_PASSWORD=s3cret".to_string()];
        assert_eq!(
            resolve(None, entry, &env),
            Resolved::VendorChanged {
                user: "admin".into(),
                env: "GF_SECURITY_ADMIN_PASSWORD"
            }
        );
        // The user can be overridden too, and is then the one printed.
        let env = vec!["GF_SECURITY_ADMIN_USER=root".to_string()];
        assert!(matches!(
            resolve(None, entry, &env),
            Resolved::VendorDefault { user, .. } if user == "root"
        ));
        // A variable that merely starts with the name is not it.
        let env = vec!["GF_SECURITY_ADMIN_PASSWORD_X=1".to_string()];
        assert!(matches!(
            resolve(None, entry, &env),
            Resolved::VendorDefault { .. }
        ));
    }

    #[test]
    fn no_source_is_unknown_never_invented() {
        assert_eq!(
            resolve(None, container_default("alpine:3.20"), &[]),
            Resolved::Unknown
        );
        assert_eq!(
            resolve(None, container_default("prom/prometheus"), &[]),
            Resolved::VendorNoLogin
        );
        assert!(matches!(
            resolve(None, vm_default("proxmox-ve:9.2"), &[]),
            Resolved::VendorDefault { user, password: "delonix-admin" } if user == "root"
        ));
        assert_eq!(vm_default("ubuntu:24.04").map(|e| e.images), None);
    }

    #[test]
    fn a_block_that_names_no_login_or_could_rewrite_a_terminal_is_refused() {
        assert!(decl(Some("admin"), None).validate().is_ok());
        assert!(decl(None, Some(("grafana-admin", "password")))
            .validate()
            .is_ok());
        assert!(decl(None, None).validate().is_err());
        assert!(decl(Some(""), None).validate().is_err());
        assert!(decl(Some("a\nb"), None).validate().is_err());
        assert!(decl(Some("\x1b[2J"), None).validate().is_err());
        assert!(decl(None, Some(("Bad/Name", "password")))
            .validate()
            .is_err());
        assert!(decl(None, Some(("ok", "not a key"))).validate().is_err());
    }
}
