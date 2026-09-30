//! Failure injection against a TLS mock appliance.
//!
//! The client refuses `http://`, so the mock is a real listener with a
//! certificate `rcgen` mints per test and `rustls` serves — the same shape
//! `delonix-proxmox`'s own failure-injection suite already uses, because it
//! is what makes the two TLS cases provable: a certificate the client
//! cannot verify is refused, and the same certificate handed over as
//! `ca_cert_pem` is accepted WITHOUT `insecure_tls`.
//!
//! Every scenario reads from the mock's own request LOG (what was sent, how
//! many times), never just "the call returned Ok" — the property that
//! matters most here is `redirect::Policy::none()` actually holding: a 302
//! must show up as ONE logged request, never a second one to wherever it
//! points.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use delonix_networking::gateway::{AliasKind, EnsureOutcome, GatewayAlias, GatewayRule};
use delonix_networking::ownership::{Owner, OwnerMark, RemoveOutcome};
use delonix_opnsense::{Auth, Client, Error, Staging, Target, MAX_RESPONSE_BYTES};

// ===========================================================================
// The mock appliance
// ===========================================================================

#[derive(Clone)]
enum Reply {
    Json(u16, String),
    Redirect,
    Truncated { claimed: usize, body: String },
    Drop,
    Huge(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    method: String,
    path: String,
    body: String,
}

struct MockAppliance {
    base_url: String,
    cert_pem: String,
    log: Arc<Mutex<Vec<Seen>>>,
}

/// A script: the reply for each (method, path), consumed in order when
/// several are queued for the same route. A route with no script gets the
/// appliance's stock answer — an empty `rows` list for a search, `OK` for
/// `firmware/status`/`apply`/`reconfigure`.
type Script = Arc<Mutex<Vec<((String, String), VecDeque<Reply>)>>>;

fn stock(method: &str, path: &str) -> Reply {
    match (method, path) {
        ("GET", "core/firmware/status") => Reply::Json(
            200,
            r#"{"CORE_ABI":"26.1","CORE_NICKNAME":"Witty Woodpecker"}"#.into(),
        ),
        // Two owner labels (ours and another record's) and an operator's own
        // category, which says nothing about ownership.
        ("POST", "firewall/category/search_item") => Reply::Json(
            200,
            format!(
                r#"{{"rows":[{{"uuid":"{CAT_OURS}","name":"delonix-owner:dlx-0123456789abcdef"}},{{"uuid":"{CAT_OTHER}","name":"delonix-owner:dlx-ffffffffffffffff"}},{{"uuid":"{CAT_HAND}","name":"web servers"}}]}}"#
            ),
        ),
        ("POST", p) if p.ends_with("search_item") || p.ends_with("search_rule") => Reply::Json(
            200,
            r#"{"total":0,"rowCount":0,"current":1,"rows":[]}"#.into(),
        ),
        ("POST", p) if p.ends_with("add_item") || p.ends_with("add_rule") => Reply::Json(
            200,
            r#"{"result":"saved","uuid":"00000000-0000-0000-0000-000000000000"}"#.into(),
        ),
        ("POST", p) if p.ends_with("apply") || p.ends_with("reconfigure") => {
            Reply::Json(200, r#"{"status":"OK\n\n"}"#.into())
        }
        ("POST", p) if p.contains("/del_item/") || p.contains("/del_rule/") => {
            Reply::Json(200, r#"{"result":"deleted"}"#.into())
        }
        // The running state of a clean appliance: nothing loaded in pf that
        // the config does not also say.
        ("GET", "diagnostics/firewall/list_rule_ids") => Reply::Json(200, r#"{"items":[]}"#.into()),
        ("GET", "firewall/alias_util/aliases") => {
            Reply::Json(200, r#"["bogons","__lan_network"]"#.into())
        }
        ("GET", p) if p.starts_with("firewall/alias_util/list/") => Reply::Json(
            200,
            r#"{"total":0,"rowCount":0,"current":1,"rows":[]}"#.into(),
        ),
        _ => Reply::Json(
            500,
            r#"{"errorMessage":"mock: no script for this route"}"#.into(),
        ),
    }
}

impl MockAppliance {
    fn start(script: Script) -> Self {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_pem = ck.cert.pem();
        let cert_der = rustls::pki_types::CertificateDer::from(ck.cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()),
        );
        let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
        let cfg = Arc::new(cfg);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        std::thread::spawn(move || {
            for tcp in listener.incoming() {
                let Ok(tcp) = tcp else { break };
                let cfg = cfg.clone();
                let script = script.clone();
                let log = log2.clone();
                std::thread::spawn(move || serve_one(tcp, cfg, script, log));
            }
        });
        Self {
            base_url: format!("https://localhost:{port}"),
            cert_pem,
            log,
        }
    }

    fn log(&self) -> Vec<Seen> {
        self.log.lock().unwrap().clone()
    }

    /// The body of the LAST request to (method, path).
    fn body_of(&self, method: &str, path: &str) -> String {
        self.log()
            .iter()
            .rev()
            .find(|s| s.method == method && s.path == path)
            .map(|s| s.body.clone())
            .unwrap_or_default()
    }

    fn count(&self, method: &str, path: &str) -> usize {
        self.log()
            .iter()
            .filter(|s| s.method == method && s.path == path)
            .count()
    }
}

fn serve_one(
    mut tcp: std::net::TcpStream,
    cfg: Arc<rustls::ServerConfig>,
    script: Script,
    log: Arc<Mutex<Vec<Seen>>>,
) {
    let mut conn = rustls::ServerConnection::new(cfg).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match tls.read(&mut byte) {
            Ok(1) => {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            _ => return,
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default();
    let content_length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if content_length > 0 && tls.read_exact(&mut body).is_err() {
        return;
    }
    let path = target
        .strip_prefix("/api/")
        .unwrap_or(target)
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();
    log.lock().unwrap().push(Seen {
        method: method.clone(),
        path: path.clone(),
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    let reply = {
        let mut s = script.lock().unwrap();
        let queued = s
            .iter_mut()
            .find(|(k, _)| k.0 == method && k.1 == path)
            .and_then(|(_, q)| q.pop_front());
        queued.unwrap_or_else(|| stock(&method, &path))
    };
    let write_json = |tls: &mut rustls::Stream<
        '_,
        rustls::ServerConnection,
        std::net::TcpStream,
    >,
                      status: u16,
                      body: &[u8]| {
        let reason = match status {
            200 => "OK",
            302 => "Found",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Status",
        };
        let location = if status == 302 { "Location: /\r\n" } else { "" };
        let _ = write!(
            tls,
            "HTTP/1.1 {status} {reason}\r\n{location}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = tls.write_all(body);
        let _ = tls.flush();
    };
    match reply {
        Reply::Json(status, body) => write_json(&mut tls, status, body.as_bytes()),
        Reply::Redirect => write_json(&mut tls, 302, b"<html>login</html>"),
        Reply::Truncated { claimed, body } => {
            let _ = write!(
                tls,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {claimed}\r\nConnection: close\r\n\r\n"
            );
            let _ = tls.write_all(body.as_bytes());
            let _ = tls.flush();
        }
        Reply::Drop => {}
        Reply::Huge(n) => {
            let filler = n.saturating_sub(2);
            let mut body = Vec::with_capacity(n);
            body.push(b'"');
            body.resize(body.len() + filler, b'a');
            body.push(b'"');
            write_json(&mut tls, 200, &body);
        }
    }
    tls.conn.send_close_notify();
    let _ = tls.flush();
}

fn script(entries: &[(&str, &str, Reply)]) -> Script {
    let mut map: Vec<((String, String), VecDeque<Reply>)> = Vec::new();
    for (method, path, reply) in entries {
        let key = (method.to_string(), path.to_string());
        match map.iter_mut().find(|(k, _)| *k == key) {
            Some((_, q)) => q.push_back(reply.clone()),
            None => map.push((key, VecDeque::from([reply.clone()]))),
        }
    }
    Arc::new(Mutex::new(map))
}

fn target(appliance: &MockAppliance) -> Target {
    Target {
        base_url: appliance.base_url.clone(),
        auth: Auth {
            key: "test-key".into(),
            secret: "test-secret".into(),
        },
        insecure_tls: true,
        ca_cert_pem: None,
    }
}

// ===========================================================================
// TLS
// ===========================================================================

#[test]
fn a_certificate_the_client_cannot_verify_is_refused_and_the_same_one_as_ca_is_accepted() {
    let appliance = MockAppliance::start(script(&[]));

    let insecure_off = Target {
        insecure_tls: false,
        ca_cert_pem: None,
        ..target(&appliance)
    };
    let err = Client::connect(&insecure_off).unwrap_err();
    assert!(matches!(err, Error::Request(_)), "{err}");

    let pinned = Target {
        insecure_tls: false,
        ca_cert_pem: Some(appliance.cert_pem.clone().into_bytes()),
        ..target(&appliance)
    };
    Client::connect(&pinned).expect("the pinned CA must be accepted without insecure_tls");
}

// ===========================================================================
// Status classification (ADR-0051 Phase 0's own measurements)
// ===========================================================================

#[test]
fn a_302_with_no_body_is_unauthorized_and_never_followed() {
    let appliance =
        MockAppliance::start(script(&[("GET", "core/firmware/status", Reply::Redirect)]));
    let err = Client::connect(&target(&appliance)).unwrap_err();
    assert!(matches!(err, Error::Unauthorized(_)), "{err}");
    assert_eq!(
        appliance.count("GET", "core/firmware/status"),
        1,
        "a redirect must never be followed to a second request"
    );
}

#[test]
fn a_401_is_unauthorized() {
    let appliance = MockAppliance::start(script(&[(
        "GET",
        "core/firmware/status",
        Reply::Json(
            401,
            r#"{"status":401,"message":"Authentication Failed"}"#.into(),
        ),
    )]));
    let err = Client::connect(&target(&appliance)).unwrap_err();
    assert!(matches!(err, Error::Unauthorized(_)), "{err}");
}

#[test]
fn a_403_is_forbidden() {
    let appliance = MockAppliance::start(script(&[(
        "GET",
        "core/firmware/status",
        Reply::Json(403, "{}".into()),
    )]));
    let err = Client::connect(&target(&appliance)).unwrap_err();
    assert!(matches!(err, Error::Forbidden(_)), "{err}");
}

#[test]
fn a_404_is_route_not_found() {
    let appliance = MockAppliance::start(script(&[(
        "GET",
        "core/firmware/status",
        Reply::Json(404, r#"{"errorMessage":"Endpoint not found"}"#.into()),
    )]));
    let err = Client::connect(&target(&appliance)).unwrap_err();
    assert!(matches!(err, Error::RouteNotFound(_)), "{err}");
}

#[test]
fn a_validation_failure_at_http_200_is_typed_not_treated_as_success() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/filter/add_rule",
        Reply::Json(
            200,
            r#"{"result":"failed","validations":{"rule.protocol":"Option [] not in list."}}"#
                .into(),
        ),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let err = client
        .ensure_rule(
            &GatewayRule {
                description: "adr0051spike".into(),
                source: "any".into(),
                destination: "10.0.0.0/24".into(),
                protocol: Some("BOGUS".into()),
            },
            &mark(),
            &Staging::default(),
        )
        .unwrap_err();
    assert!(matches!(err, Error::Validation(_)), "{err}");
    assert!(err.to_string().contains("rule.protocol"));
}

#[test]
fn a_result_failed_without_validations_is_http_status_not_a_silent_success() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/filter/add_rule",
        Reply::Json(200, r#"{"result":"failed"}"#.into()),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let err = client
        .ensure_rule(
            &GatewayRule {
                description: "x".into(),
                source: "any".into(),
                destination: "10.0.0.0/24".into(),
                protocol: None,
            },
            &mark(),
            &Staging::default(),
        )
        .unwrap_err();
    assert!(matches!(err, Error::HttpStatus(_)), "{err}");
}

#[test]
fn a_truncated_body_is_a_transport_failure_not_a_decode_error() {
    let appliance = MockAppliance::start(script(&[(
        "GET",
        "core/firmware/status",
        Reply::Truncated {
            claimed: 1000,
            body: r#"{"CORE_"#.into(),
        },
    )]));
    let err = Client::connect(&target(&appliance)).unwrap_err();
    assert!(matches!(err, Error::Request(_)), "{err}");
}

#[test]
fn a_connection_dropped_outright_is_a_request_failure() {
    let appliance = MockAppliance::start(script(&[("GET", "core/firmware/status", Reply::Drop)]));
    let err = Client::connect(&target(&appliance)).unwrap_err();
    assert!(matches!(err, Error::Request(_)), "{err}");
}

#[test]
fn a_body_over_the_cap_is_refused_not_read() {
    let appliance = MockAppliance::start(script(&[(
        "GET",
        "core/firmware/status",
        Reply::Huge(MAX_RESPONSE_BYTES + 1024),
    )]));
    let err = Client::connect(&target(&appliance)).unwrap_err();
    assert!(matches!(err, Error::ResponseTooLarge(_)), "{err}");
}

#[test]
fn malformed_json_is_a_decode_error() {
    let appliance = MockAppliance::start(script(&[(
        "GET",
        "core/firmware/status",
        Reply::Json(200, "not json at all".into()),
    )]));
    let err = Client::connect(&target(&appliance)).unwrap_err();
    assert!(matches!(err, Error::Decode(_)), "{err}");
}

// ===========================================================================
// ensure_alias / ensure_rule / commit against the mock's stock answers
// ===========================================================================

#[test]
fn ensure_alias_creates_when_absent_and_reports_already_present_when_found() {
    let appliance = MockAppliance::start(script(&[]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let alias = GatewayAlias {
        name: "adr0051spike".into(),
        kind: AliasKind::Host,
        content: vec!["10.99.99.99".into()],
        description: "test".into(),
    };
    let staging = Staging::default();
    let outcome = client
        .ensure_alias(&alias, &mark(), &staging)
        .expect("stock add_item succeeds");
    assert_eq!(outcome, EnsureOutcome::Created);
    assert_eq!(appliance.count("POST", "firewall/alias/add_item"), 1);
    let sent = appliance.body_of("POST", "firewall/alias/add_item");
    assert!(
        sent.contains(&format!(r#""categories":"{CAT_OURS}""#)),
        "the created alias must carry the owner category: {sent}"
    );
    assert!(
        sent.contains(r#""description":"test""#),
        "the description stays the declared text: {sent}"
    );
    assert_eq!(
        staging.changes().len(),
        1,
        "the commit must know it is ours"
    );
}

#[test]
fn ensure_alias_already_present_never_calls_add_item() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/alias/search_item",
        Reply::Json(
            200,
            rows(&[serde_json::json!({
                "uuid": U1, "name": "adr0051spike", "type": "host", "enabled": "1",
                "content": "10.99.99.99", "description": "test", "categories": CAT_OURS,
            })]),
        ),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let alias = GatewayAlias {
        name: "adr0051spike".into(),
        kind: AliasKind::Host,
        content: vec!["10.99.99.99".into()],
        description: "test".into(),
    };
    let outcome = client
        .ensure_alias(&alias, &mark(), &Staging::default())
        .unwrap();
    assert_eq!(outcome, EnsureOutcome::AlreadyPresent);
    assert_eq!(
        appliance.count("POST", "firewall/alias/add_item"),
        0,
        "an already-present alias must never be re-created"
    );
}

#[test]
fn ensure_rule_creates_when_absent() {
    let appliance = MockAppliance::start(script(&[]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let rule = GatewayRule {
        description: "adr0051spike".into(),
        source: "adr0051spike".into(),
        destination: "10.0.0.0/24".into(),
        protocol: Some("TCP".into()),
    };
    let outcome = client
        .ensure_rule(&rule, &mark(), &Staging::default())
        .expect("stock add_rule succeeds");
    assert_eq!(outcome, EnsureOutcome::Created);
    assert_eq!(appliance.count("POST", "firewall/filter/add_rule"), 1);
}

#[test]
fn remove_alias_is_a_no_op_when_nothing_matches() {
    let appliance = MockAppliance::start(script(&[]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let outcome = client
        .remove_alias("does-not-exist", &mark(), &Staging::default())
        .expect("removing something absent is not an error");
    assert_eq!(outcome, RemoveOutcome::Absent);
    assert_eq!(appliance.count("POST", "firewall/alias/del_item"), 0);
}

#[test]
fn commit_calls_reconfigure_then_apply_in_order() {
    let appliance = MockAppliance::start(script(&[]));
    let client = Client::connect(&target(&appliance)).unwrap();
    client.reconfigure_aliases().unwrap();
    client.apply_filter().unwrap();
    let log = appliance.log();
    let reconfigure_at = log
        .iter()
        .position(|s| s.method == "POST" && s.path == "firewall/alias/reconfigure")
        .expect("reconfigure was called");
    let apply_at = log
        .iter()
        .position(|s| s.method == "POST" && s.path == "firewall/filter/apply")
        .expect("apply was called");
    assert!(
        reconfigure_at < apply_at,
        "aliases must be reconfigured before the filter is applied"
    );
}

// ===========================================================================
// Ownership (audit 62, §6 P1): found by name is not owned
// ===========================================================================

const U1: &str = "11111111-1111-1111-1111-111111111111";
const U2: &str = "22222222-2222-2222-2222-222222222222";
const CAT_OURS: &str = "c0000000-0000-0000-0000-00000000000a";
const CAT_OTHER: &str = "c0000000-0000-0000-0000-00000000000b";
const CAT_HAND: &str = "c0000000-0000-0000-0000-00000000000c";

fn mark() -> OwnerMark {
    OwnerMark::new("dlx-0123456789abcdef").unwrap()
}

/// A search answer with these rows.
fn rows(rows: &[serde_json::Value]) -> String {
    serde_json::json!({ "total": rows.len(), "rowCount": rows.len(), "current": 1, "rows": rows })
        .to_string()
}

fn rule_row(uuid: &str, description: &str) -> serde_json::Value {
    serde_json::json!({
        "uuid": uuid, "enabled": "1", "description": description,
        "source_net": "10.1.0.0/24", "destination_net": "10.0.0.0/24", "protocol": "TCP",
    })
}

/// A rule described "allow web" carrying OUR owner category — and an
/// operator's own category next to it, which must not confuse the reading.
fn ours_row(uuid: &str) -> serde_json::Value {
    let mut row = rule_row(uuid, "allow web");
    row["categories"] = format!("{CAT_HAND},{CAT_OURS}").into();
    row
}

fn web_rule() -> GatewayRule {
    GatewayRule {
        description: "allow web".into(),
        source: "10.1.0.0/24".into(),
        destination: "10.0.0.0/24".into(),
        protocol: Some("TCP".into()),
    }
}

fn host_alias() -> GatewayAlias {
    GatewayAlias {
        name: "web".into(),
        kind: AliasKind::Host,
        content: vec!["10.1.0.5".into()],
        description: "web hosts".into(),
    }
}

#[test]
fn a_hand_made_alias_with_the_same_name_is_refused_not_adopted() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/alias/search_item",
        Reply::Json(
            200,
            rows(&[serde_json::json!({
                "uuid": U1, "name": "web", "type": "host", "enabled": "1",
                "content": "10.1.0.5", "description": "made by hand",
            })]),
        ),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let err = client
        .ensure_alias(&host_alias(), &mark(), &Staging::default())
        .unwrap_err();
    assert!(matches!(err, Error::NotOwned(_)), "{err}");
    assert_eq!(err.number(), 5389);
    assert_eq!(appliance.count("POST", "firewall/alias/add_item"), 0);
}

#[test]
fn another_records_alias_is_refused_and_named_as_such() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/alias/search_item",
        Reply::Json(
            200,
            rows(&[serde_json::json!({
                "uuid": U1, "name": "web", "type": "host", "enabled": "1", "content": "10.1.0.5",
                "description": "web hosts", "categories": CAT_OTHER,
            })]),
        ),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let err = client
        .ensure_alias(&host_alias(), &mark(), &Staging::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("dlx-ffffffffffffffff"), "{err}");
}

#[test]
fn a_hand_made_rule_with_the_same_description_is_refused_not_adopted() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/filter/search_rule",
        Reply::Json(200, rows(&[rule_row(U1, "allow web")])),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let err = client
        .ensure_rule(&web_rule(), &mark(), &Staging::default())
        .unwrap_err();
    assert!(matches!(err, Error::NotOwned(_)), "{err}");
    assert_eq!(appliance.count("POST", "firewall/filter/add_rule"), 0);
}

#[test]
fn an_owned_rule_that_matches_is_already_present() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/filter/search_rule",
        Reply::Json(200, rows(&[ours_row(U1)])),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let outcome = client
        .ensure_rule(&web_rule(), &mark(), &Staging::default())
        .unwrap();
    assert_eq!(outcome, EnsureOutcome::AlreadyPresent);
    assert_eq!(appliance.count("POST", "firewall/filter/add_rule"), 0);
}

#[test]
fn an_owned_rule_edited_on_the_appliance_is_drift_not_present() {
    let mut row = ours_row(U1);
    row["destination_net"] = "any".into();
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/filter/search_rule",
        Reply::Json(200, rows(&[row])),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let err = client
        .ensure_rule(&web_rule(), &mark(), &Staging::default())
        .unwrap_err();
    assert!(matches!(err, Error::Drifted(_)), "{err}");
    assert_eq!(err.number(), 5389);
    assert!(
        err.to_string().contains("destination_net is 'any'"),
        "{err}"
    );
}

#[test]
fn an_owned_alias_whose_content_changed_is_drift() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/alias/search_item",
        Reply::Json(
            200,
            rows(&[serde_json::json!({
                "uuid": U1, "name": "web", "type": "host", "enabled": "1",
                "content": "10.1.0.5\n10.9.9.9", "description": "web hosts", "categories": CAT_OURS,
            })]),
        ),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let err = client
        .ensure_alias(&host_alias(), &mark(), &Staging::default())
        .unwrap_err();
    assert!(matches!(err, Error::Drifted(_)), "{err}");
    assert!(err.to_string().contains("10.9.9.9"), "{err}");
}

#[test]
fn remove_rule_deletes_ours_by_uuid_and_leaves_the_hand_made_one() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/filter/search_rule",
        Reply::Json(200, rows(&[rule_row(U1, "allow web"), ours_row(U2)])),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let staging = Staging::default();
    let outcome = client.remove_rule("allow web", &mark(), &staging).unwrap();
    assert_eq!(outcome, RemoveOutcome::NotOwned(Owner::Unmarked));
    assert_eq!(
        appliance.count("POST", &format!("firewall/filter/del_rule/{U2}")),
        1
    );
    assert_eq!(
        appliance.count("POST", &format!("firewall/filter/del_rule/{U1}")),
        0,
        "the hand-made rule must never be deleted"
    );
    assert_eq!(staging.changes().len(), 1);
}

#[test]
fn remove_alias_leaves_an_alias_it_does_not_own() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/alias/search_item",
        Reply::Json(
            200,
            rows(&[serde_json::json!({
                "uuid": U1, "name": "web", "type": "host", "content": "10.1.0.5",
                "description": "made by hand",
            })]),
        ),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let outcome = client
        .remove_alias("web", &mark(), &Staging::default())
        .unwrap();
    assert_eq!(outcome, RemoveOutcome::NotOwned(Owner::Unmarked));
    assert_eq!(
        appliance.count("POST", &format!("firewall/alias/del_item/{U1}")),
        0
    );
}

#[test]
fn an_add_rule_answer_without_a_uuid_is_refused_since_the_commit_could_not_claim_it() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/filter/add_rule",
        Reply::Json(200, r#"{"result":"saved"}"#.into()),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let err = client
        .ensure_rule(&web_rule(), &mark(), &Staging::default())
        .unwrap_err();
    assert!(matches!(err, Error::Decode(_)), "{err}");
}

// ===========================================================================
// Commit: only when everything staged is ours
// ===========================================================================

#[test]
fn a_foreign_rule_staged_and_not_applied_refuses_the_commit_before_any_apply() {
    // A rule configured and enabled that pf does not run: someone saved it
    // and did not apply. It is not in this caller's staging.
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/filter/search_rule",
        Reply::Json(200, rows(&[rule_row(U1, "operator's half-finished rule")])),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let err = client.commit(&Staging::default()).unwrap_err();
    assert!(matches!(err, Error::ForeignPending(_)), "{err}");
    assert_eq!(err.number(), 5389);
    assert!(err.to_string().contains(U1), "{err}");
    assert_eq!(appliance.count("POST", "firewall/alias/reconfigure"), 0);
    assert_eq!(appliance.count("POST", "firewall/filter/apply"), 0);
}

#[test]
fn a_foreign_deletion_not_applied_refuses_the_pre_check() {
    // pf still runs a rule the config no longer has.
    let appliance = MockAppliance::start(script(&[(
        "GET",
        "diagnostics/firewall/list_rule_ids",
        Reply::Json(
            200,
            format!(r#"{{"items":[{{"id":"{U2}","descr":"gone"}}]}}"#),
        ),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let err = client
        .check_no_foreign_pending(&Staging::default())
        .unwrap_err();
    assert!(err.to_string().contains("deleted, not applied"), "{err}");
}

#[test]
fn a_foreign_alias_edit_not_applied_is_seen_through_the_pf_table() {
    let appliance = MockAppliance::start(script(&[
        (
            "POST",
            "firewall/alias/search_item",
            Reply::Json(
                200,
                rows(&[serde_json::json!({
                    "uuid": U1, "name": "office", "type": "network", "enabled": "1",
                    "content": "10.5.0.0/24\n10.6.0.0/24", "description": "by hand",
                })]),
            ),
        ),
        (
            "GET",
            "firewall/alias_util/aliases",
            Reply::Json(200, r#"["office","bogons"]"#.into()),
        ),
        (
            "GET",
            "firewall/alias_util/list/office",
            Reply::Json(200, rows(&[serde_json::json!({ "ip": "10.5.0.0/24" })])),
        ),
    ]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let pending = client.pending_changes().unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].id, "office");
    assert_eq!(pending[0].what, "content changed, not applied");
}

#[test]
fn a_host_alias_matches_its_table_when_pf_shows_a_bare_address() {
    let appliance = MockAppliance::start(script(&[
        (
            "POST",
            "firewall/alias/search_item",
            Reply::Json(
                200,
                rows(&[serde_json::json!({
                    "uuid": U1, "name": "one", "type": "host", "enabled": "1",
                    "content": "10.5.0.7/32", "description": "",
                })]),
            ),
        ),
        (
            "GET",
            "firewall/alias_util/aliases",
            Reply::Json(200, r#"["one"]"#.into()),
        ),
        (
            "GET",
            "firewall/alias_util/list/one",
            Reply::Json(200, rows(&[serde_json::json!({ "ip": "10.5.0.7" })])),
        ),
    ]));
    let client = Client::connect(&target(&appliance)).unwrap();
    assert!(client.pending_changes().unwrap().is_empty());
}

#[test]
fn our_own_staged_rule_is_applied_and_proven_running_afterwards() {
    let created = "00000000-0000-0000-0000-000000000000";
    let appliance = MockAppliance::start(script(&[
        // ensure_rule: nothing yet.
        (
            "POST",
            "firewall/filter/search_rule",
            Reply::Json(200, rows(&[])),
        ),
        // commit's check: our rule configured, not running.
        (
            "POST",
            "firewall/filter/search_rule",
            Reply::Json(200, rows(&[ours_row(created)])),
        ),
        (
            "GET",
            "diagnostics/firewall/list_rule_ids",
            Reply::Json(200, r#"{"items":[]}"#.into()),
        ),
        // after the apply: running.
        (
            "POST",
            "firewall/filter/search_rule",
            Reply::Json(200, rows(&[ours_row(created)])),
        ),
        (
            "GET",
            "diagnostics/firewall/list_rule_ids",
            Reply::Json(
                200,
                format!(r#"{{"items":[{{"id":"{created}","descr":"allow web"}}]}}"#),
            ),
        ),
    ]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let staging = Staging::default();
    client.ensure_rule(&web_rule(), &mark(), &staging).unwrap();
    client.commit(&staging).expect("only our change is staged");
    assert_eq!(appliance.count("POST", "firewall/filter/apply"), 1);
    assert!(staging.changes().is_empty(), "a committed staging is empty");
}

#[test]
fn an_apply_that_leaves_our_rule_not_running_is_an_error_not_a_success() {
    let created = "00000000-0000-0000-0000-000000000000";
    let appliance = MockAppliance::start(script(&[
        (
            "POST",
            "firewall/filter/search_rule",
            Reply::Json(200, rows(&[])),
        ),
        // Every later search: our rule configured; pf never loads it.
        (
            "POST",
            "firewall/filter/search_rule",
            Reply::Json(200, rows(&[ours_row(created)])),
        ),
        (
            "POST",
            "firewall/filter/search_rule",
            Reply::Json(200, rows(&[ours_row(created)])),
        ),
    ]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let staging = Staging::default();
    client.ensure_rule(&web_rule(), &mark(), &staging).unwrap();
    let err = client.commit(&staging).unwrap_err();
    assert!(err.to_string().contains("still"), "{err}");
    assert!(err.to_string().contains(created), "{err}");
}

#[test]
fn a_foreign_change_staged_after_ours_refuses_and_takes_ours_back_out() {
    // The race the pre-check cannot close: between our writes and the
    // commit, someone else saves a rule. The commit refuses, and the rule
    // THIS caller created is deleted again, so the operator's next apply
    // does not push it either.
    let created = "00000000-0000-0000-0000-000000000000";
    let appliance = MockAppliance::start(script(&[
        (
            "POST",
            "firewall/filter/search_rule",
            Reply::Json(200, rows(&[])),
        ),
        (
            "POST",
            "firewall/filter/search_rule",
            Reply::Json(
                200,
                rows(&[ours_row(created), rule_row(U1, "someone else's")]),
            ),
        ),
    ]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let staging = Staging::default();
    client.ensure_rule(&web_rule(), &mark(), &staging).unwrap();
    let err = client.commit(&staging).unwrap_err();
    assert!(matches!(err, Error::ForeignPending(_)), "{err}");
    assert!(err.to_string().contains(U1), "{err}");
    assert!(
        !err.to_string().contains(created),
        "ours is not foreign: {err}"
    );
    assert_eq!(appliance.count("POST", "firewall/filter/apply"), 0);
    assert_eq!(
        appliance.count("POST", &format!("firewall/filter/del_rule/{created}")),
        1,
        "our staged rule must be taken back out"
    );
    assert_eq!(
        appliance.count("POST", &format!("firewall/filter/del_rule/{U1}")),
        0,
        "never someone else's"
    );
}

// ===========================================================================
// The owner category (ADR-0059 D1.5)
// ===========================================================================

#[test]
fn an_operators_own_category_is_not_an_owner_mark() {
    let mut row = rule_row(U1, "allow web");
    row["categories"] = CAT_HAND.into();
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/filter/search_rule",
        Reply::Json(200, rows(&[row])),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let err = client
        .ensure_rule(&web_rule(), &mark(), &Staging::default())
        .unwrap_err();
    assert!(matches!(err, Error::NotOwned(_)), "{err}");
}

#[test]
fn the_first_write_of_a_record_creates_its_category_and_uses_the_answered_uuid() {
    let fresh = "c0000000-0000-0000-0000-0000000000ff";
    let appliance = MockAppliance::start(script(&[
        (
            "POST",
            "firewall/category/search_item",
            Reply::Json(200, rows(&[])),
        ),
        (
            "POST",
            "firewall/category/add_item",
            Reply::Json(200, format!(r#"{{"result":"saved","uuid":"{fresh}"}}"#)),
        ),
    ]));
    let client = Client::connect(&target(&appliance)).unwrap();
    client
        .ensure_rule(&web_rule(), &mark(), &Staging::default())
        .unwrap();
    let category = appliance.body_of("POST", "firewall/category/add_item");
    assert!(
        category.contains("delonix-owner:dlx-0123456789abcdef"),
        "{category}"
    );
    let rule = appliance.body_of("POST", "firewall/filter/add_rule");
    assert!(
        rule.contains(&format!(r#""categories":"{fresh}""#)),
        "{rule}"
    );
    assert!(rule.contains(r#""description":"allow web""#), "{rule}");
}

#[test]
fn release_owner_deletes_our_category_only() {
    let appliance = MockAppliance::start(script(&[]));
    let client = Client::connect(&target(&appliance)).unwrap();
    assert_eq!(
        client.release_owner(&mark()).unwrap(),
        RemoveOutcome::Removed
    );
    assert_eq!(
        appliance.count("POST", &format!("firewall/category/del_item/{CAT_OURS}")),
        1
    );
    assert_eq!(
        appliance.count("POST", &format!("firewall/category/del_item/{CAT_OTHER}")),
        0
    );
    let stranger = OwnerMark::new("dlx-1111111111111111").unwrap();
    assert_eq!(
        client.release_owner(&stranger).unwrap(),
        RemoveOutcome::Absent
    );
}

#[test]
fn a_category_still_in_use_is_surfaced_not_forced() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        &format!("firewall/category/del_item/{CAT_OURS}"),
        Reply::Json(
            500,
            r#"{"errorMessage":"Cannot delete a category which is still in use.","errorTitle":"Category in use"}"#
                .into(),
        ),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let err = client.release_owner(&mark()).unwrap_err().to_string();
    assert!(err.contains("still in use"), "{err}");
}

// ===========================================================================
// Through the trait: the dictionary number survives the port
// ===========================================================================

#[test]
fn a_refusal_keeps_its_dx_number_through_the_gateway_provider_trait() {
    use delonix_networking::gateway::GatewayProvider;
    let appliance = MockAppliance::start(script(&[
        (
            "POST",
            "firewall/filter/search_rule",
            Reply::Json(200, rows(&[rule_row(U1, "allow web")])),
        ),
        (
            "POST",
            "firewall/filter/search_rule",
            Reply::Json(200, rows(&[rule_row(U2, "someone else's")])),
        ),
    ]));
    let provider = delonix_opnsense::OpnsenseGatewayProvider::connect(&target(&appliance)).unwrap();
    // One reason (provider_conflict, ADR-0059 D5) for both; the message still
    // says which of the two it was.
    let e = provider.ensure_rule(&web_rule(), &mark()).unwrap_err();
    assert_eq!(e.number(), 5389, "{e}");
    assert!(e.to_string().contains("refusing to adopt"), "{e}");
    let e = provider.check_no_foreign_pending().unwrap_err();
    assert_eq!(e.number(), 5389, "{e}");
    assert!(e.to_string().contains("that are not this engine's"), "{e}");
}
