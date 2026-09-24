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

use delonix_opnsense::{Auth, Client, Error, Target, MAX_RESPONSE_BYTES};
use delonix_sdn::gateway::{AliasKind, EnsureOutcome, GatewayAlias, GatewayRule};

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
        .ensure_rule(&GatewayRule {
            description: "adr0051spike".into(),
            source: "any".into(),
            destination: "10.0.0.0/24".into(),
            protocol: Some("BOGUS".into()),
        })
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
        .ensure_rule(&GatewayRule {
            description: "x".into(),
            source: "any".into(),
            destination: "10.0.0.0/24".into(),
            protocol: None,
        })
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
    let outcome = client
        .ensure_alias(&alias)
        .expect("stock add_item succeeds");
    assert_eq!(outcome, EnsureOutcome::Created);
    assert_eq!(appliance.count("POST", "firewall/alias/add_item"), 1);
}

#[test]
fn ensure_alias_already_present_never_calls_add_item() {
    let appliance = MockAppliance::start(script(&[(
        "POST",
        "firewall/alias/search_item",
        Reply::Json(
            200,
            r#"{"total":1,"rowCount":1,"current":1,"rows":[{"uuid":"11111111-1111-1111-1111-111111111111","name":"adr0051spike"}]}"#
                .into(),
        ),
    )]));
    let client = Client::connect(&target(&appliance)).unwrap();
    let alias = GatewayAlias {
        name: "adr0051spike".into(),
        kind: AliasKind::Host,
        content: vec!["10.99.99.99".into()],
        description: "test".into(),
    };
    let outcome = client.ensure_alias(&alias).unwrap();
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
    let outcome = client.ensure_rule(&rule).expect("stock add_rule succeeds");
    assert_eq!(outcome, EnsureOutcome::Created);
    assert_eq!(appliance.count("POST", "firewall/filter/add_rule"), 1);
}

#[test]
fn remove_alias_is_a_no_op_when_nothing_matches() {
    let appliance = MockAppliance::start(script(&[]));
    let client = Client::connect(&target(&appliance)).unwrap();
    client
        .remove_alias("does-not-exist")
        .expect("removing something absent is not an error");
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
