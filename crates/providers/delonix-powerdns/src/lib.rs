//! A minimal client for a PowerDNS authoritative server's HTTP API, holding
//! the credential the operator gave THE ENGINE (ADR-0064 D6).
//!
//! # Why it exists
//!
//! A segment provider with a DNS role (Proxmox SDN's `powerdns` plugin) writes
//! a subnet gateway's A and PTR records itself, and leaves some of them
//! behind: a deleted subnet keeps its gateway's records, and a gateway changed
//! in place keeps the old address in `<vnet>-gw` (ADR-0064, measured on PVE
//! 9.2.2). No node API removes them, so removing them takes the DNS server's
//! own API — with a credential the operator gives the engine in
//! `providers.yaml`, never the one the node returns (ADR-0064 D5, D6).
//!
//! # What it does
//!
//! Only what the cleanup needs, and nothing that creates a record:
//!
//! * [`Client::connect`] proves the server URL and the key with
//!   `GET <server>` (the server object PowerDNS answers at
//!   `/api/v1/servers/<id>`). That one call is what makes a later 404 mean
//!   «no such zone» and not «wrong server id» — PowerDNS answers both with
//!   the same bare `Not Found` (measured on 4.9.17);
//! * [`Client::remove_record`] removes ONE record — a `(name, type,
//!   content)` triple — from its rrset, keeping every other record of the
//!   rrset: the same read-filter-`REPLACE`/`DELETE` the node's own plugin does
//!   (`del_a_record`, `Dns/PowerdnsPlugin.pm`, PVE 9.2.2). A record that is not
//!   there is [`Removal::Absent`], never an error: the cleanup is idempotent.
//!
//! # Limits, said here and not discovered later
//!
//! * PowerDNS's API has no conditional write: between the read and the
//!   `PATCH`, another writer's change to the same rrset can be lost. The node's
//!   own plugin has the same window.
//! * PowerDNS's built-in webserver speaks no TLS. `http://` sends the key in
//!   the clear, so the caller has to allow it explicitly
//!   ([`Target::allow_plain_http`]); `https://` (a TLS proxy in front) is the
//!   default.

use delonix_model::codes::Reason;
use serde_json::{json, Value};
use std::time::Duration;

/// The most a response body may be before this client refuses to read the
/// rest. A zone this client reads is filtered to one rrset
/// (`rrset_name`/`rrset_type`, measured on 4.9.17); a body past this is not
/// one of those.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Where the DNS server is and how to get in.
#[derive(Clone)]
pub struct Target {
    /// The server's API URL, `<scheme>://<host>:<port>/api/v1/servers/<id>`
    /// — the same form a Proxmox `powerdns` controller takes.
    pub server_url: String,
    /// The `X-API-Key`. Never printed: [`std::fmt::Debug`] redacts it.
    pub key: String,
    /// `http://` is refused unless this is set: it sends the key in the
    /// clear. PowerDNS's own webserver has no TLS, so a lab or a server on a
    /// private management network needs it — the operator's choice to make.
    pub allow_plain_http: bool,
    /// Accept a certificate this host cannot verify (a TLS proxy with a
    /// self-signed certificate). Opt-in.
    pub insecure_tls: bool,
    /// A CA certificate (PEM) to trust in addition to the system roots.
    pub ca_cert_pem: Option<Vec<u8>>,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("server_url", &self.server_url)
            .field("key", &"<redacted>")
            .field("allow_plain_http", &self.allow_plain_http)
            .field("insecure_tls", &self.insecure_tls)
            .field("ca_cert_pem", &self.ca_cert_pem.as_ref().map(|_| "<pem>"))
            .finish()
    }
}

/// What [`Client::remove_record`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// The record was there and is gone; the rrset's other records, if any,
    /// were kept.
    Removed,
    /// The zone holds no such record (or no such rrset). Nothing written.
    Absent,
    /// The server has no such zone. Nothing written.
    ZoneAbsent,
}

/// One PowerDNS server's API, proven reachable with its key.
pub struct Client {
    http: reqwest::blocking::Client,
    base: String,
    key: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// A failure of this client, by what the operator does next (ADR-0043: a
/// provider's own error, converted into the shared class with its number).
/// No variant ever carries the API key.
#[derive(Debug)]
pub enum Error {
    /// Fixable in the providers file: the URL, the key's form, the TLS
    /// settings, or a request the server refused as malformed (400/422).
    /// The ADR-0059 D5 reason `invalid_intent`.
    Invalid(String),
    /// The server did not answer, or answered 5xx. `provider_unavailable`.
    Unavailable(String),
    /// The server refused the key (401/403). `provider_auth_failed`.
    Denied(String),
    /// An answer this client cannot read: a status it has no class for, a
    /// body that is not the JSON it expected, or one past the size limit.
    Unexpected(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(m) | Error::Unavailable(m) | Error::Denied(m) | Error::Unexpected(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// The ADR-0059 D5 reason of this failure; `None` for an answer this
    /// client cannot classify.
    pub fn reason(&self) -> Option<Reason> {
        match self {
            Error::Invalid(_) => Some(Reason::InvalidIntent),
            Error::Unavailable(_) => Some(Reason::ProviderUnavailable),
            Error::Denied(_) => Some(Reason::ProviderAuthFailed),
            Error::Unexpected(_) => None,
        }
    }
}

impl From<Error> for delonix_model::Error {
    fn from(e: Error) -> Self {
        type M = delonix_model::Error;
        let reason = e.reason();
        let class = match e {
            Error::Invalid(m) => M::Invalid(m),
            Error::Unavailable(m) => M::Unavailable(m),
            Error::Denied(m) => M::PermissionDenied(m),
            Error::Unexpected(m) => {
                return M::Runtime {
                    context: "powerdns api",
                    message: m,
                }
            }
        };
        match reason {
            Some(r) => M::coded(r.number(), class),
            None => class,
        }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

fn invalid(msg: String) -> Error {
    Error::Invalid(msg)
}

fn unavailable(msg: String) -> Error {
    Error::Unavailable(msg)
}

fn denied(msg: String) -> Error {
    Error::Denied(msg)
}

fn runtime(msg: String) -> Error {
    Error::Unexpected(msg)
}

/// `s`, cut to at most `max` bytes at a character boundary.
fn truncate_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `name` as a fully qualified DNS name, with its trailing dot.
pub fn fqdn(name: &str) -> String {
    let n = name.trim();
    if n.ends_with('.') {
        n.to_string()
    } else {
        format!("{n}.")
    }
}

/// A zone's id in the API path: its fully qualified name, with every byte
/// outside `[A-Za-z0-9.-_]` written `=XX` — PowerDNS's own encoding of a zone
/// name into an id (`apiZoneNameToId`).
pub fn zone_id(zone: &str) -> String {
    let mut out = String::new();
    for b in fqdn(zone).bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_') {
            out.push(b as char);
        } else {
            out.push_str(&format!("={b:02X}"));
        }
    }
    out
}

/// The rrset change that removes `content` from `rrset` (one rrset object as
/// PowerDNS returns it), or `None` when the rrset does not hold it. The same
/// rule the node's plugin follows (`del_a_record`): the other records are
/// rewritten with `REPLACE` and their own TTL; an rrset left empty is
/// `DELETE`d.
pub fn removal_change(rrset: &Value, name: &str, kind: &str, content: &str) -> Option<Value> {
    let records = rrset["records"].as_array()?;
    let kept: Vec<Value> = records
        .iter()
        .filter(|r| r["content"].as_str() != Some(content))
        .cloned()
        .collect();
    if kept.len() == records.len() {
        return None;
    }
    Some(if kept.is_empty() {
        json!({ "name": name, "type": kind, "changetype": "DELETE", "records": [] })
    } else {
        json!({
            "name": name,
            "type": kind,
            "ttl": rrset["ttl"].clone(),
            "changetype": "REPLACE",
            "records": kept,
        })
    })
}

impl Client {
    /// Builds the client and proves the URL and the key with `GET <server>`.
    pub fn connect(target: &Target) -> Result<Self> {
        let url = target.server_url.trim().trim_end_matches('/').to_string();
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("http://") {
            if !target.allow_plain_http {
                return Err(invalid(format!(
                    "the PowerDNS server '{url}' is plain http, which sends the API key in the \
                     clear — put a TLS proxy in front of it, or set `allowPlainHttp: true` for a \
                     server on a network you trust"
                )));
            }
        } else if !lower.starts_with("https://") {
            return Err(invalid(format!(
                "the PowerDNS server URL '{url}' is not an http(s) URL"
            )));
        }
        if url.contains('@') {
            return Err(invalid(format!(
                "the PowerDNS server URL '{}' carries a user — the key goes in `keyFile`, never \
                 in the URL",
                url.split('@').next_back().unwrap_or_default()
            )));
        }
        if !url.contains("/api/v1/servers/") {
            return Err(invalid(format!(
                "the PowerDNS server URL '{url}' does not name a server — it ends in \
                 `/api/v1/servers/<id>` (usually `localhost`)"
            )));
        }
        if target.key.trim().is_empty() {
            return Err(invalid(format!(
                "the API key for the PowerDNS server '{url}' is empty"
            )));
        }
        let mut builder = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(2)
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(target.insecure_tls);
        if let Some(pem) = &target.ca_cert_pem {
            let cert = reqwest::Certificate::from_pem(pem).map_err(|e| {
                invalid(format!(
                    "the CA certificate given for {url} is not a PEM certificate: {e}"
                ))
            })?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder
            .build()
            .map_err(|e| runtime(format!("could not build the HTTP client: {e}")))?;
        let me = Self {
            http,
            base: url,
            key: target.key.trim().to_string(),
        };
        let (status, body) = me.send(reqwest::Method::GET, "", None)?;
        if status == 404 {
            return Err(invalid(format!(
                "'{}' is not a PowerDNS server (404) — check the server id at the end of the URL",
                me.base
            )));
        }
        let v = me.expect_json(status, &body, "")?;
        if v["type"].as_str() != Some("Server") {
            return Err(runtime(format!(
                "'{}' answered, but not with a PowerDNS server object",
                me.base
            )));
        }
        tracing::debug!(
            server = %me.base,
            version = v["version"].as_str().unwrap_or_default(),
            "powerdns: connected"
        );
        Ok(me)
    }

    /// One request. The key rides `X-API-Key` and is never part of any
    /// message this client builds.
    fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, String)> {
        let url = format!("{}{path}", self.base);
        let mut req = self
            .http
            .request(method.clone(), &url)
            .header("X-API-Key", &self.key)
            .header("Accept", "application/json");
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().map_err(|e| {
            unavailable(format!(
                "the PowerDNS server at {} did not answer {method} {path}: {e}",
                self.base
            ))
        })?;
        let status = resp.status().as_u16();
        use std::io::Read;
        let mut buf = Vec::new();
        resp.take(MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(|e| {
                unavailable(format!(
                    "reading the answer to {method} {path} from {} failed: {e}",
                    self.base
                ))
            })?;
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(runtime(format!(
                "the answer to {method} {path} from {} exceeded {} MiB — refusing to read it",
                self.base,
                MAX_RESPONSE_BYTES / (1024 * 1024)
            )));
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        match status {
            401 | 403 => Err(denied(format!(
                "the PowerDNS server at {} refused the API key ({status}) — check `keyFile`",
                self.base
            ))),
            _ => Ok((status, text)),
        }
    }

    /// The JSON of a 2xx answer, or the failure a non-2xx one is.
    fn expect_json(&self, status: u16, body: &str, path: &str) -> Result<Value> {
        if !(200..300).contains(&status) {
            return Err(self.status_error(status, body, path));
        }
        serde_json::from_str(body)
            .map_err(|_| runtime(format!("the answer of {} to {path} is not JSON", self.base)))
    }

    /// A non-2xx answer as a failure. PowerDNS's error body is
    /// `{"error": "<text>"}`, which carries no credential; it is quoted, cut.
    fn status_error(&self, status: u16, body: &str, path: &str) -> Error {
        let why = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| body.trim().to_string());
        let why = truncate_chars(&why, 200);
        let msg = format!(
            "the PowerDNS server at {} answered {status} to {path}: {why}",
            self.base
        );
        match status {
            400 | 422 => invalid(msg),
            500..=599 => unavailable(msg),
            _ => runtime(msg),
        }
    }

    /// The rrset `(name, kind)` of `zone` as PowerDNS returns it:
    /// `Ok(None)` when the server has no such zone (a 404 — after
    /// [`Self::connect`] proved the server id, that is what it means),
    /// `Ok(Some(None))` when the zone holds no such rrset.
    fn rrset(&self, zone: &str, name: &str, kind: &str) -> Result<Option<Option<Value>>> {
        let path = format!(
            "/zones/{}?rrset_name={}&rrset_type={}",
            zone_id(zone),
            fqdn(name),
            kind
        );
        let (status, body) = self.send(reqwest::Method::GET, &path, None)?;
        if status == 404 {
            return Ok(None);
        }
        let v = self.expect_json(status, &body, &path)?;
        let name = fqdn(name);
        // Filtered on the server (measured on 4.9.17); filtered again here,
        // so a server that ignores the parameters gives the same answer.
        Ok(Some(
            v["rrsets"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| {
                    r["name"]
                        .as_str()
                        .is_some_and(|n| n.eq_ignore_ascii_case(&name))
                        && r["type"].as_str() == Some(kind)
                })
                .cloned(),
        ))
    }

    /// The contents of the rrset `(name, kind)` in `zone`: empty when the
    /// zone holds no such rrset, `None` when the server has no such zone.
    pub fn record_contents(
        &self,
        zone: &str,
        name: &str,
        kind: &str,
    ) -> Result<Option<Vec<String>>> {
        Ok(self.rrset(zone, name, kind)?.map(|r| {
            r.map(|r| {
                r["records"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x["content"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
        }))
    }

    /// Removes the ONE record `(name, kind, content)` from `zone`, keeping
    /// the rrset's other records. Idempotent: a record that is not there is
    /// [`Removal::Absent`], a zone the server does not have
    /// [`Removal::ZoneAbsent`].
    pub fn remove_record(
        &self,
        zone: &str,
        name: &str,
        kind: &str,
        content: &str,
    ) -> Result<Removal> {
        let Some(rrset) = self.rrset(zone, name, kind)? else {
            return Ok(Removal::ZoneAbsent);
        };
        let Some(rrset) = rrset else {
            return Ok(Removal::Absent);
        };
        let Some(change) = removal_change(&rrset, &fqdn(name), kind, content) else {
            return Ok(Removal::Absent);
        };
        let path = format!("/zones/{}", zone_id(zone));
        let (status, body) = self.send(
            reqwest::Method::PATCH,
            &path,
            Some(&json!({ "rrsets": [change] })),
        )?;
        if !(200..300).contains(&status) {
            return Err(self.status_error(status, &body, &path));
        }
        Ok(Removal::Removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0043/ADR-0059 D5: every failure converts into the class of its
    /// reason with the reason's number, which is in the dictionary.
    #[test]
    fn every_failure_converts_with_its_reasons_number() {
        for (e, exit) in [
            (Error::Invalid("x".into()), 1),
            (Error::Unavailable("x".into()), 69),
            (Error::Denied("x".into()), 77),
        ] {
            let r = e.reason().expect("a reason");
            let m = delonix_model::Error::from(e);
            assert_eq!(m.number(), r.number());
            assert!(delonix_model::codes::lookup(m.number()).is_some());
            assert_eq!(delonix_model::exitcode::for_error(&m), exit);
        }
        let m = delonix_model::Error::from(Error::Unexpected("x".into()));
        assert!(m.to_string().contains("powerdns api"), "{m}");
    }

    #[test]
    fn a_zone_id_is_the_fqdn_with_specials_escaped() {
        assert_eq!(zone_id("f5c.lab"), "f5c.lab.");
        assert_eq!(
            zone_id("16-31.172.in-addr.arpa."),
            "16-31.172.in-addr.arpa."
        );
        assert_eq!(
            zone_id("0/26.1.168.192.in-addr.arpa"),
            "0=2F26.1.168.192.in-addr.arpa."
        );
    }

    #[test]
    fn removal_keeps_the_other_records_of_the_rrset() {
        let rrset = json!({
            "name": "v-gw.f5c.lab.", "type": "A", "ttl": 3600,
            "records": [
                { "content": "10.84.0.1", "disabled": false },
                { "content": "10.84.0.254", "disabled": false },
            ],
        });
        let change = removal_change(&rrset, "v-gw.f5c.lab.", "A", "10.84.0.1").unwrap();
        assert_eq!(change["changetype"], "REPLACE");
        assert_eq!(change["ttl"], 3600);
        assert_eq!(change["records"].as_array().unwrap().len(), 1);
        assert_eq!(change["records"][0]["content"], "10.84.0.254");
    }

    #[test]
    fn removal_of_the_last_record_deletes_the_rrset() {
        let rrset = json!({
            "name": "v-gw.f5c.lab.", "type": "A", "ttl": 3600,
            "records": [{ "content": "10.84.0.1", "disabled": false }],
        });
        let change = removal_change(&rrset, "v-gw.f5c.lab.", "A", "10.84.0.1").unwrap();
        assert_eq!(change["changetype"], "DELETE");
        assert!(change["records"].as_array().unwrap().is_empty());
    }

    #[test]
    fn removal_of_a_record_that_is_not_there_writes_nothing() {
        let rrset = json!({
            "name": "v-gw.f5c.lab.", "type": "A", "ttl": 3600,
            "records": [{ "content": "10.84.0.254", "disabled": false }],
        });
        assert!(removal_change(&rrset, "v-gw.f5c.lab.", "A", "10.84.0.1").is_none());
    }

    #[test]
    fn the_key_is_never_in_debug() {
        let t = Target {
            server_url: "https://dns.example/api/v1/servers/localhost".into(),
            key: "s3cr3t-key".into(),
            allow_plain_http: false,
            insecure_tls: false,
            ca_cert_pem: None,
        };
        assert!(!format!("{t:?}").contains("s3cr3t-key"));
    }

    #[test]
    fn plain_http_is_refused_unless_allowed_and_no_request_is_sent() {
        let t = Target {
            server_url: "http://127.0.0.1:9/api/v1/servers/localhost".into(),
            key: "k".into(),
            allow_plain_http: false,
            insecure_tls: false,
            ca_cert_pem: None,
        };
        let e = Client::connect(&t).unwrap_err();
        assert!(e.to_string().contains("allowPlainHttp"), "{e}");
        let e = delonix_model::Error::from(e);
        assert_eq!(e.number(), Reason::InvalidIntent.number());
        assert_eq!(delonix_model::exitcode::for_error(&e), 1);
    }

    #[test]
    fn a_url_without_a_server_or_with_a_user_is_refused() {
        let mut t = Target {
            server_url: "https://dns.example/api/v1".into(),
            key: "k".into(),
            allow_plain_http: false,
            insecure_tls: false,
            ca_cert_pem: None,
        };
        assert!(Client::connect(&t)
            .unwrap_err()
            .to_string()
            .contains("/api/v1/servers/<id>"));
        t.server_url = "https://u:p@dns.example/api/v1/servers/localhost".into();
        let e = Client::connect(&t).unwrap_err().to_string();
        assert!(e.contains("keyFile") && !e.contains("u:p"), "{e}");
    }
}
