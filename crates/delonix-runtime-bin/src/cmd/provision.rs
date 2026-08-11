//! `spec.provision` of a `kind: Volume` — create on the NAS what the share
//! block consumes, instead of requiring someone to have made it by hand.
//!
//! **Entirely optional.** A `kind: Volume` without this block behaves exactly
//! as it did before, byte for byte; an existing manifest does not change
//! meaning (ADR-0009).
//!
//! The shape follows `kind: Workload`'s: the block is NAMED for its target
//! (`provision.truenas`), so a target cannot contradict its own declaration.
//! A second vendor becomes a sibling key, not a `type:` field to keep in sync.

use super::manifest;
use super::po;
use super::util::state_root;
use delonix_runtime_core::{Error, Result};
use serde::{Deserialize, Serialize};

/// Accepted keys of `spec.provision` (drift-guard).
pub(crate) const PROVISION_FIELDS: &[&str] = &["truenas"];

/// Accepted keys of `spec.provision.truenas` (drift-guard).
pub(crate) const TRUENAS_FIELDS: &[&str] = &[
    "url",
    "apiKeySecret",
    "username",
    "password",
    "passwordSecret",
    "insecureTLS",
    "dataset",
    "quota",
    "owner",
    "share",
];

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct ProvisionSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) truenas: Option<TrueNasSpec>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct TrueNasSpec {
    /// `https://<nas>`. The scheme is the caller's: `http://` puts the API key
    /// on the wire in the clear, which is a decision to make deliberately.
    pub(crate) url: String,
    /// `kind: Secret` holding the API key under the key `apiKey` (or `token`).
    /// Preferred over an account: a key is revocable on the appliance without
    /// touching anyone's login.
    #[serde(
        default,
        rename = "apiKeySecret",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) api_key_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) username: Option<String>,
    /// Literal password. Accepted for a throwaway lab and nothing else — it
    /// ends up in whatever git repository the manifest lives in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) password: Option<String>,
    /// `kind: Secret` holding the account password under the key `password`.
    #[serde(
        default,
        rename = "passwordSecret",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) password_secret: Option<String>,
    /// Accept a certificate this host cannot verify — which a stock TrueNAS
    /// needs, since it serves a self-signed one. Explicit because turning it on
    /// means another machine can answer in the appliance's name, taking the API
    /// key with it.
    #[serde(default, rename = "insecureTLS")]
    pub(crate) insecure_tls: bool,
    /// Full ZFS path, `<pool>/<name>`. The pool has to exist: laying out
    /// physical disks is not something a volume manifest should trigger.
    pub(crate) dataset: String,
    /// Quota ON THE NAS (`"10G"`). Distinct from the volume's own `spec.quota`,
    /// which is a local accounting limit — for a network share the NAS is the
    /// only place a limit can actually be enforced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) quota: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner: Option<OwnerSpec>,
    /// NFS export rules. Omit to provision the dataset WITHOUT exporting it —
    /// useful when the export already exists and only the quota is managed here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) share: Option<ShareSpec>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct OwnerSpec {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    /// Octal, quoted (`"0770"`). A YAML `0770` unquoted is the decimal 770, and
    /// that is not the mode anyone means — so it is parsed from a string and a
    /// bare number is refused by serde rather than misread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mode: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct ShareSpec {
    /// CIDRs allowed to mount. **Required, and refused when empty**: the
    /// appliance reads an empty list as "every network", and a manifest that
    /// says nothing about who may mount a share must not end up exporting it to
    /// everything that can reach the NAS.
    #[serde(default)]
    pub(crate) networks: Vec<String>,
    #[serde(
        default,
        rename = "maprootUser",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) maproot_user: Option<String>,
    #[serde(
        default,
        rename = "maprootGroup",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) maproot_group: Option<String>,
    #[serde(default, rename = "readOnly")]
    pub(crate) read_only: bool,
}

/// What provisioning produced, for the caller to turn into a mount.
pub(crate) struct Provisioned {
    /// Host to mount from — the NAS, derived from `url` so it cannot disagree
    /// with the appliance that was actually provisioned.
    pub(crate) server: String,
    /// Export path — the mountpoint the NAS reported, not one we composed. A
    /// dataset's mountpoint is the appliance's to decide, and guessing
    /// `/mnt/<dataset>` would be right until someone sets a custom one.
    pub(crate) share: String,
    /// Bytes the NAS says it enforces (`None` = unlimited).
    pub(crate) quota: Option<u64>,
    pub(crate) available: Option<u64>,
}

/// The API key or account password, from a `kind: Secret` when named.
///
/// Same convention as `tunnel::resolve_token` and `storage::resolve_password`:
/// a literal wins, else the secret's named key.
fn resolve_secret_key(name: &str, keys: &[&str]) -> Result<String> {
    let store = delonix_runtime_core::SecretStore::open(state_root())?;
    let s = store.load(name)?;
    for k in keys {
        if let Some(v) = s.data.get(*k) {
            return Ok(v.clone());
        }
    }
    Err(Error::Invalid(po::tf(
        "secret '{name}' has none of the keys: {keys}",
        &[("name", name), ("keys", &keys.join(", "))],
    )))
}

fn auth_of(spec: &TrueNasSpec) -> Result<delonix_truenas::Auth> {
    if let Some(sref) = &spec.api_key_secret {
        return Ok(delonix_truenas::Auth::ApiKey(resolve_secret_key(
            sref,
            &["apiKey", "api_key", "token"],
        )?));
    }
    let username = spec.username.clone().ok_or_else(|| {
        Error::Invalid(
            po::t("provision.truenas needs either `apiKeySecret` or `username` with a password")
                .into(),
        )
    })?;
    let password = match (&spec.password, &spec.password_secret) {
        (Some(p), _) => p.clone(),
        (None, Some(sref)) => resolve_secret_key(sref, &["password"])?,
        (None, None) => {
            return Err(Error::Invalid(
                po::t("provision.truenas: `username` needs `passwordSecret` (or `password`)")
                    .into(),
            ))
        }
    };
    Ok(delonix_truenas::Auth::Password { username, password })
}

/// The host part of `https://nas.example:443` → `nas.example`.
///
/// Pure, and it is what the mount will actually contact — deriving it means the
/// share cannot end up pointing at a different machine from the one that was
/// provisioned, which is precisely the drift a second hand-written `server:`
/// field would allow.
pub(crate) fn host_of_url(url: &str) -> Result<String> {
    let rest = url.split_once("://").map(|(_, r)| r).ok_or_else(|| {
        Error::Invalid(format!(
            "provision.truenas: '{url}' has no scheme (https://…)"
        ))
    })?;
    let hostport = rest.split(['/', '?']).next().unwrap_or("");
    // An IPv6 literal is bracketed; keep it whole, and only strip a port that
    // follows the closing bracket.
    let host = if let Some(end) = hostport.strip_prefix('[').and_then(|r| r.find(']')) {
        &hostport[..end + 2]
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return Err(Error::Invalid(format!(
            "provision.truenas: '{url}' names no host"
        )));
    }
    Ok(host.to_string())
}

/// Parses an octal mode written as a string (`"0770"`, `"770"`).
pub(crate) fn parse_mode(s: &str) -> Result<u32> {
    let t = s.trim().trim_start_matches("0o");
    u32::from_str_radix(t, 8)
        .ok()
        .filter(|m| *m <= 0o7777)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "provision.truenas.owner.mode: '{s}' is not an octal mode (e.g. \"0770\")"
            ))
        })
}

/// Runs the provisioning described by the block and reports what now exists.
///
/// Nothing here is inferred from the volume's name: the dataset is named
/// explicitly in the manifest. A provisioner that derived `tank/<volume-name>`
/// would make renaming a volume silently point at a different dataset — and,
/// with the destructive path, at a different dataset to destroy.
pub(crate) fn run_truenas(spec: &TrueNasSpec) -> Result<Provisioned> {
    delonix_truenas::validate_dataset_name(&spec.dataset)?;
    let server = host_of_url(&spec.url)?;
    let quota = match &spec.quota {
        Some(q) => Some(
            delonix_volume::parse_size_bytes(q)
                .ok_or_else(|| Error::Invalid(po::tf("invalid quota: {q}", &[("q", q)])))?,
        ),
        None => None,
    };
    // Refuse an unacceptable quota BEFORE connecting: a manifest error should
    // not need a reachable NAS to be reported.
    delonix_truenas::validate_quota(quota)?;
    let owner = match &spec.owner {
        Some(o) => Some(delonix_truenas::Owner {
            uid: o.uid,
            gid: o.gid,
            mode: match &o.mode {
                Some(m) => Some(parse_mode(m)?),
                None => None,
            },
        }),
        None => None,
    };
    if let Some(sh) = &spec.share {
        if sh.networks.is_empty() {
            return Err(Error::Invalid(po::t(
                "provision.truenas.share.networks is empty — the NAS reads that as \"any network may mount this\". List the CIDRs that may, or drop the `share:` block to provision the dataset without exporting it",
            ).into()));
        }
    }

    let client = delonix_truenas::Client::connect(&delonix_truenas::Target {
        base_url: spec.url.clone(),
        auth: auth_of(spec)?,
        insecure_tls: spec.insecure_tls,
    })?;

    let p = client.ensure_dataset(&delonix_truenas::DatasetSpec {
        dataset: spec.dataset.clone(),
        quota,
        owner,
    })?;

    if let Some(sh) = &spec.share {
        client.ensure_nfs_share(
            &p.mountpoint,
            &delonix_truenas::NfsShareSpec {
                networks: sh.networks.clone(),
                maproot_user: sh.maproot_user.clone(),
                maproot_group: sh.maproot_group.clone(),
                read_only: sh.read_only,
            },
        )?;
    }

    Ok(Provisioned {
        server,
        share: p.mountpoint,
        quota: p.quota,
        available: p.available,
    })
}

/// Warns about unknown keys inside `spec.provision` and its vendor block. The
/// top-level guard only sees the spec's own keys, so a `datasett:` in here
/// would otherwise be swallowed into "no dataset" — the same reason the share
/// blocks get their own pass.
pub(crate) fn warn_unknown(doc: &manifest::ManifestDoc) {
    manifest::warn_unknown_fields_in(doc, "provision", PROVISION_FIELDS);
    manifest::warn_unknown_fields_in(doc, "provision.truenas", TRUENAS_FIELDS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o_host_vem_do_url_e_nao_de_um_campo_a_parte() {
        assert_eq!(host_of_url("https://nas.local").unwrap(), "nas.local");
        assert_eq!(host_of_url("https://nas.local:8443").unwrap(), "nas.local");
        assert_eq!(host_of_url("https://10.0.0.5/api/").unwrap(), "10.0.0.5");
        assert_eq!(host_of_url("http://[fd00::1]:8443/x").unwrap(), "[fd00::1]");
        assert!(host_of_url("nas.local").is_err(), "no scheme");
        assert!(host_of_url("https://").is_err(), "no host");
    }

    #[test]
    fn o_modo_e_octal_e_vem_de_uma_string() {
        assert_eq!(parse_mode("0770").unwrap(), 0o770);
        assert_eq!(parse_mode("770").unwrap(), 0o770);
        assert_eq!(parse_mode("0o755").unwrap(), 0o755);
        // 8 and 9 are not octal digits: `"0778"` is a typo, not a mode, and
        // taking the first two digits would apply 077 to somebody's data.
        assert!(parse_mode("0778").is_err());
        assert!(parse_mode("77777").is_err());
        assert!(parse_mode("").is_err());
    }
}
