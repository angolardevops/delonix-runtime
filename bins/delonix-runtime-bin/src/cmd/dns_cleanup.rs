//! ADR-0064 D6: removing the gateway records a segment provider's node writes
//! on a DNS server and never removes, with the credential the operator gives
//! THE ENGINE — a `type: powerdns` entry of `providers.yaml` — and never the
//! one the node returns (D5).
//!
//! The rule is D6's: only `<vnet>-gw.<domain>` A records whose address is the
//! gateway of a subnet this engine owned, and their PTR. Which records those
//! are, and their content, comes from the zone's own record
//! ([`delonix_networking::dns::gateway_records`]); a record whose content is
//! not that exact value is never touched, and the other records of the same
//! rrset stay (the read-filter-rewrite the node's plugin does itself).
//!
//! A record whose controller no `powerdns` entry names, or whose server
//! refuses or does not answer, is NOT removed and is said out loud — the
//! ADR-0064 D4 warning, now with the reason. A cleanup never fails the
//! teardown that called it: the cluster side is already gone, and the
//! record is reported for the operator to remove.

use super::po;
use super::providers_config::PowerdnsEntry;
use delonix_model::{Error, Result};
use delonix_networking::dns::DnsRecord;
use delonix_powerdns::{Client, Removal, Target};

/// The client target of a `powerdns` entry: the key read from `keyFile`
/// (refused unless only its owner reads it), the CA from `tls.caFile`.
/// Nothing is contacted.
pub(crate) fn target_of(d: &PowerdnsEntry) -> Result<Target> {
    let Some(path) = d.auth.key_file.as_deref() else {
        return Err(Error::Invalid(
            po::t("the powerdns entry has no `auth.keyFile` — the engine needs its own API key")
                .into(),
        ));
    };
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| Error::Invalid(format!("{path}: {e}")))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(Error::Invalid(po::tf(
            "{path} is readable by other users — chmod 600 it before using it as the PowerDNS keyFile",
            &[("path", path)],
        )));
    }
    let key = std::fs::read_to_string(path)
        .map_err(|e| Error::Invalid(format!("{path}: {e}")))?
        .trim()
        .to_string();
    if key.is_empty() {
        return Err(Error::Invalid(po::tf(
            "{path} is empty — it holds the PowerDNS API key",
            &[("path", path)],
        )));
    }
    let url = d.url.trim().to_ascii_lowercase();
    if url.starts_with("http://") && !d.allow_plain_http {
        return Err(Error::Invalid(po::tf(
            "the PowerDNS server '{url}' is plain http, which sends the API key in the clear — \
             put a TLS proxy in front of it, or set `allowPlainHttp: true`",
            &[("url", &d.url)],
        )));
    }
    let ca_cert_pem = match d.tls.ca_file.as_deref() {
        Some(f) => Some(std::fs::read(f).map_err(|e| Error::Invalid(format!("{f}: {e}")))?),
        None => None,
    };
    Ok(Target {
        server_url: d.url.clone(),
        key,
        allow_plain_http: d.allow_plain_http,
        insecure_tls: d.tls.insecure_skip_verify,
        ca_cert_pem,
    })
}

/// What happened to one record.
#[derive(Clone, Debug)]
pub(crate) enum Cleaned {
    Removed,
    /// The server does not hold it (already gone, or never written there).
    NotThere,
    /// No `powerdns` entry names the record's controller.
    NoCredential,
    /// The server refused, or did not answer.
    Failed(String),
}

/// Removes each of `records` from the server its controller names, with the
/// engine's own credential. One answer per record, in order. `entry` is the
/// providers file's `powerdns` entry, or `None` when it has none.
pub(crate) fn clean_with(
    entry: Option<&PowerdnsEntry>,
    records: &[DnsRecord],
) -> Vec<(DnsRecord, Cleaned)> {
    let serves = |r: &DnsRecord| entry.is_some_and(|d| d.controllers.contains(&r.controller));
    let mut client: Option<std::result::Result<Client, String>> = None;
    records
        .iter()
        .map(|r| {
            let outcome = if !serves(r) {
                Cleaned::NoCredential
            } else {
                let c = client.get_or_insert_with(|| {
                    let d = entry.expect("serves() checked the entry");
                    target_of(d)
                        .and_then(|t| Client::connect(&t).map_err(Error::from))
                        .map_err(|e| e.to_string())
                });
                match c {
                    Err(e) => Cleaned::Failed(e.clone()),
                    Ok(c) => match c.remove_record(&r.zone, &r.name, &r.kind, &r.content) {
                        Ok(Removal::Removed) => Cleaned::Removed,
                        Ok(Removal::Absent | Removal::ZoneAbsent) => Cleaned::NotThere,
                        Err(e) => Cleaned::Failed(Error::from(e).to_string()),
                    },
                }
            };
            (r.clone(), outcome)
        })
        .collect()
}

/// [`clean_with`] against this process's providers file. A file that cannot
/// be read cleans nothing, and every record says why.
pub(crate) fn clean(records: &[DnsRecord]) -> Vec<(DnsRecord, Cleaned)> {
    match super::providers_config::powerdns_entry() {
        Ok(entry) => clean_with(entry, records),
        Err(e) => records
            .iter()
            .map(|r| (r.clone(), Cleaned::Failed(e.to_string())))
            .collect(),
    }
}

/// The records of `outcomes` still on their server as far as this engine
/// knows (no credential, or the server refused or did not answer): what a
/// zone record keeps, to retry them.
pub(crate) fn still_left(outcomes: &[(DnsRecord, Cleaned)]) -> Vec<DnsRecord> {
    outcomes
        .iter()
        .filter(|(_, c)| matches!(c, Cleaned::NoCredential | Cleaned::Failed(_)))
        .map(|(r, _)| r.clone())
        .collect()
}

/// Says what happened to each record, one line each, prefixed with the
/// zone's document (`networkzone/<name>`).
pub(crate) fn report(document: &str, outcomes: &[(DnsRecord, Cleaned)]) {
    for (r, outcome) in outcomes {
        let record = format!("{} {} {}", r.name, r.kind, r.content);
        let line = match outcome {
            Cleaned::Removed => po::tf(
                "{doc}: dns record '{record}' removed from zone '{zone}' (controller '{server}')",
                &[
                    ("doc", document),
                    ("record", &record),
                    ("zone", &r.zone),
                    ("server", &r.controller),
                ],
            ),
            Cleaned::NotThere => po::tf(
                "{doc}: dns record '{record}' is not on zone '{zone}' (controller '{server}') — \
                 nothing to remove",
                &[
                    ("doc", document),
                    ("record", &record),
                    ("zone", &r.zone),
                    ("server", &r.controller),
                ],
            ),
            Cleaned::NoCredential => po::tf(
                "{doc}: dns record '{record}' left on zone '{zone}' (controller '{server}'): the \
                 provider writes a subnet gateway's records and never removes them — give the \
                 engine a `type: powerdns` entry naming '{server}' in providers.yaml to have it \
                 removed",
                &[
                    ("doc", document),
                    ("record", &record),
                    ("zone", &r.zone),
                    ("server", &r.controller),
                ],
            ),
            Cleaned::Failed(why) => po::tf(
                "{doc}: dns record '{record}' left on zone '{zone}' (controller '{server}'): {why}",
                &[
                    ("doc", document),
                    ("record", &record),
                    ("zone", &r.zone),
                    ("server", &r.controller),
                    ("why", why),
                ],
            ),
        };
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(controller: &str) -> DnsRecord {
        DnsRecord {
            controller: controller.into(),
            zone: "f5c.lab.".into(),
            name: "v-gw.f5c.lab.".into(),
            kind: "A".into(),
            content: "10.84.0.1".into(),
        }
    }

    fn entry(key_file: &str, controllers: &[&str]) -> PowerdnsEntry {
        let yaml = format!(
            "apiVersion: config.delonix.io/v1\nproviders:\n  - type: powerdns\n    url: http://127.0.0.1:9/api/v1/servers/localhost\n    controllers: [{}]\n    allowPlainHttp: true\n    auth:\n      keyFile: {key_file}\n",
            controllers.join(", ")
        );
        let cfg = super::super::providers_config::parse(&yaml, std::path::Path::new("p.yaml"))
            .expect("parse");
        match cfg.providers.into_iter().next() {
            Some(super::super::providers_config::ProviderEntry::Powerdns(d)) => *d,
            _ => panic!("not a powerdns entry"),
        }
    }

    /// No entry, or an entry for another controller: nothing is contacted
    /// and the record is reported left — never a silent skip.
    #[test]
    fn a_record_without_a_credential_is_left_and_said() {
        let got = clean_with(None, &[record("pdnslab")]);
        assert!(matches!(got[0].1, Cleaned::NoCredential));
        let e = entry("/nonexistent/key", &["other"]);
        let got = clean_with(Some(&e), &[record("pdnslab")]);
        assert!(matches!(got[0].1, Cleaned::NoCredential));
    }

    /// A key file others can read is refused before any request.
    #[test]
    fn a_key_file_others_read_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("k");
        std::fs::write(&key, "secret\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = entry(key.to_str().unwrap(), &["pdnslab"]);
        assert!(target_of(&e).unwrap_err().to_string().contains("chmod 600"));
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let t = target_of(&e).expect("target");
        assert_eq!(t.key, "secret");
        // A server that does not answer fails the record, with the reason.
        let got = clean_with(Some(&e), &[record("pdnslab")]);
        match &got[0].1 {
            Cleaned::Failed(why) => assert!(!why.contains("secret"), "{why}"),
            other => panic!("{other:?}"),
        }
    }
}
