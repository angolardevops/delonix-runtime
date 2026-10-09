//! Failure injection against a TLS mock node.
//!
//! The client refuses `http://`, so the mock is a real listener with a
//! certificate `rcgen` mints per test and `rustls` serves — which is also what
//! makes the two TLS cases provable: a certificate the client cannot verify is
//! refused, and the same certificate handed over as `ca_cert_pem` is accepted
//! WITHOUT `insecure_tls`.
//!
//! What every scenario asserts is read from two places the client cannot
//! fake: the request LOG the mock keeps (what was sent, in what order, how
//! many times) and the task LEDGER on disk. "The call returned Ok" is never the
//! whole assertion.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use delonix_compute::vm_backend::VmConfig;
use delonix_proxmox::{
    AgentExecStatus, Auth, Client, ClientOptions, Error, Ledger, Target, TaskState,
    MAX_RESPONSE_BYTES,
};

// ===========================================================================
// The mock node
// ===========================================================================

/// What the mock does with one request.
#[derive(Clone)]
enum Reply {
    /// A JSON answer with this status.
    Json(u16, String),
    /// Announce `claimed` bytes, send only `body`, then close: a body cut by
    /// the network, as the client sees it.
    Truncated { claimed: usize, body: String },
    /// Close the socket without answering: the request may or may not have
    /// been acted on — the client cannot tell.
    Drop,
    /// Sleep this long, then answer 200 `{"data":null}` — past the client's
    /// request timeout, an answer that never comes.
    Stall(Duration),
    /// A 200 with a body of exactly this many bytes of JSON.
    Huge(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    method: String,
    path: String,
    /// The decoded request body, form-encoded requests included — captured
    /// so a scenario can check WHAT was sent, not just that something was.
    body: String,
    /// The decoded query string — where a DELETE carries its parameters.
    query: String,
}

struct MockNode {
    base_url: String,
    cert_pem: String,
    log: Arc<Mutex<Vec<Seen>>>,
}

/// A script: the reply for each (method, path), consumed in order when several
/// are queued for the same route. A route with no script gets the node's
/// stock answers (login, node list, an OK task) — the boring parts every
/// scenario needs.
type Script = Arc<Mutex<Vec<((String, String), VecDeque<Reply>)>>>;

fn stock(method: &str, path: &str) -> Reply {
    match (method, path) {
        ("POST", "/access/ticket") => Reply::Json(
            200,
            r#"{"data":{"ticket":"PVE:root@pam:TICKET-1","CSRFPreventionToken":"CSRF-1"}}"#.into(),
        ),
        ("GET", "/nodes") => Reply::Json(200, r#"{"data":[{"node":"pve"}]}"#.into()),
        // An SDN apply reads the zones back to check them on every node; a
        // cluster with none has nothing to check.
        ("GET", "/cluster/sdn/zones") => Reply::Json(200, r#"{"data":[]}"#.into()),
        (_, p) if p.contains("/tasks/") && p.ends_with("/status") => Reply::Json(
            200,
            r#"{"data":{"status":"stopped","exitstatus":"OK"}}"#.into(),
        ),
        _ => Reply::Json(
            500,
            r#"{"data":null,"message":"mock: no script for this route"}"#.into(),
        ),
    }
}

impl MockNode {
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
    // Head: up to the blank line.
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
    // Strip the API prefix and the query, and undo the percent-encoding the
    // client applies to a UPID (`root@pam` travels as `root%40pam`): the
    // script is keyed by the route as a test writes it.
    let path = percent_decode(
        target
            .strip_prefix("/api2/json")
            .unwrap_or(target)
            .split('?')
            .next()
            .unwrap_or_default(),
    );
    log.lock().unwrap().push(Seen {
        method: method.clone(),
        path: path.clone(),
        body: percent_decode(&String::from_utf8_lossy(&body)),
        query: percent_decode(target.split_once('?').map(|(_, q)| q).unwrap_or_default()),
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
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            409 => "Conflict",
            422 => "Unprocessable Entity",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Status",
        };
        let _ = write!(
            tls,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = tls.write_all(body);
        let _ = tls.flush();
    };
    match reply {
        Reply::Json(status, body) => write_json(&mut tls, status, body.as_bytes()),
        Reply::Truncated { claimed, body } => {
            let _ = write!(
                tls,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {claimed}\r\nConnection: close\r\n\r\n"
            );
            let _ = tls.write_all(body.as_bytes());
            let _ = tls.flush();
        }
        Reply::Drop => {}
        Reply::Stall(d) => {
            std::thread::sleep(d);
            write_json(&mut tls, 200, br#"{"data":null}"#);
        }
        Reply::Huge(n) => {
            // `{"data":"aaaa…"}` of exactly n bytes.
            let filler = n.saturating_sub(11);
            let mut body = Vec::with_capacity(n);
            body.extend_from_slice(br#"{"data":""#);
            body.resize(body.len() + filler, b'a');
            body.extend_from_slice(br#""}"#);
            write_json(&mut tls, 200, &body);
        }
    }
    tls.conn.send_close_notify();
    let _ = tls.flush();
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ===========================================================================
// Scenario plumbing
// ===========================================================================

fn script(entries: &[(&str, &str, Reply)]) -> Script {
    let mut v: Vec<((String, String), VecDeque<Reply>)> = Vec::new();
    for (m, p, r) in entries {
        let key = (m.to_string(), p.to_string());
        match v.iter_mut().find(|(k, _)| *k == key) {
            Some((_, q)) => q.push_back(r.clone()),
            None => v.push((key, VecDeque::from([r.clone()]))),
        }
    }
    Arc::new(Mutex::new(v))
}

fn token_target(node: &MockNode) -> Target {
    Target {
        base_url: node.base_url.clone(),
        node: "pve".into(),
        auth: Auth::ApiToken {
            id: "root@pam!delonix".into(),
            secret: "the-token-secret-value".into(),
        },
        insecure_tls: true,
        bridge: None,
        vlan: None,
        ca_cert_pem: None,
        import_storage: None,
        disk_storage: None,
    }
}

fn password_target(node: &MockNode) -> Target {
    Target {
        auth: Auth::Password {
            username: "root@pam".into(),
            password: "the-password-value".into(),
        },
        ..token_target(node)
    }
}

fn fast() -> ClientOptions {
    ClientOptions {
        request_timeout: Duration::from_secs(3),
        task_timeout: Duration::from_millis(600),
        trace_routes: None,
    }
}

const UPID: &str = "UPID:pve:0001A2B3:0000C4D5:66F0:qmstart:100:root@pam:";
const CONFIG: &str = "/nodes/pve/qemu/100/config";

fn ok_data(v: &str) -> Reply {
    Reply::Json(200, format!(r#"{{"data":{v}}}"#))
}

// ===========================================================================
// TLS
// ===========================================================================

#[test]
fn a_certificate_the_client_cannot_verify_is_refused_and_the_same_one_as_ca_is_accepted() {
    let node = MockNode::start(script(&[]));
    let strict = Target {
        insecure_tls: false,
        ..token_target(&node)
    };
    let err = Client::connect_with(&strict, fast())
        .err()
        .expect("no CA, no trust");
    assert!(matches!(err, Error::Request(_)), "{err}");
    assert_eq!(
        node.log().len(),
        0,
        "nothing reached the node over a handshake that failed"
    );

    let with_ca = Target {
        insecure_tls: false,
        ca_cert_pem: Some(node.cert_pem.clone().into_bytes()),
        import_storage: None,
        disk_storage: None,
        ..token_target(&node)
    };
    Client::connect_with(&with_ca, fast()).expect("verified against the CA given");
    assert_eq!(node.count("GET", "/nodes"), 1);
}

// ===========================================================================
// Redaction (ADR-0059 D5; ADR-0049's grep-for-the-secret rule)
// ===========================================================================

/// A node, a proxy or an error page can echo what it was sent. Every error
/// that carries the answer is rendered as its message and as its problem
/// document, and grepped for the token secret, the password and the ticket.
#[test]
fn no_rendered_error_carries_the_credential_the_answer_echoed() {
    let echo = "PVEAPIToken=root@pam!delonix=the-token-secret-value";
    let node = MockNode::start(script(&[
        (
            "GET",
            CONFIG,
            Reply::Json(500, format!(r#"{{"data":null,"message":"{echo}"}}"#)),
        ),
        (
            "GET",
            CONFIG,
            Reply::Json(403, format!(r#"{{"data":null,"message":"{echo}"}}"#)),
        ),
        (
            "GET",
            CONFIG,
            Reply::Json(400, format!(r#"{{"data":null,"errors":{{"x":"{echo}"}}}}"#)),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    for _ in 0..3 {
        let e = client.config(100).unwrap_err();
        let shown = e.to_string();
        let doc = delonix_model::codes::problem(&delonix_model::Error::from(e), None).to_string();
        for rendered in [&shown, &doc] {
            assert!(
                !rendered.contains("the-token-secret-value"),
                "secret leaked: {rendered}"
            );
        }
        assert!(
            shown.contains("<redacted>"),
            "the answer should still be shown: {shown}"
        );
    }

    // A password login: the password, the ticket and the CSRF token.
    let echo = "password=the-password-value ticket=PVE:root@pam:TICKET-1 csrf=CSRF-1";
    let node = MockNode::start(script(&[(
        "GET",
        CONFIG,
        Reply::Json(500, format!(r#"{{"data":null,"message":"{echo}"}}"#)),
    )]));
    let client = Client::connect_with(&password_target(&node), fast()).unwrap();
    let e = client.config(100).unwrap_err();
    let shown = e.to_string();
    let doc = delonix_model::codes::problem(&delonix_model::Error::from(e), None).to_string();
    for rendered in [&shown, &doc] {
        for secret in ["the-password-value", "PVE:root@pam:TICKET-1", "CSRF-1"] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }
    }
}

// ===========================================================================
// Status classes
// ===========================================================================

#[test]
fn every_status_class_arrives_typed_and_nothing_is_resent() {
    let node = MockNode::start(script(&[
        ("GET", CONFIG, Reply::Json(404, r#"{"data":null}"#.into())),
        ("GET", CONFIG, Reply::Json(403, r#"{"data":null,"message":"Permission check failed (/vms/100, VM.Audit)"}"#.into())),
        ("GET", CONFIG, Reply::Json(409, r#"{"data":null}"#.into())),
        ("GET", CONFIG, Reply::Json(400, r#"{"data":null,"errors":{"memory":"invalid"}}"#.into())),
        ("GET", CONFIG, Reply::Json(503, r#"{"data":null}"#.into())),
        ("GET", CONFIG, Reply::Json(500, r#"{"data":null,"message":"Configuration file 'nodes/pve/qemu-server/100.conf' does not exist\n"}"#.into())),
        ("GET", CONFIG, Reply::Json(500, r#"{"data":null,"message":"unable to open file"}"#.into())),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let e = client.config(100).unwrap_err();
    assert!(matches!(e, Error::NodeNotFound(_)), "{e}");
    let e = client.config(100).unwrap_err();
    assert!(matches!(e, Error::Forbidden(_)), "{e}");
    assert!(
        e.to_string().contains("VM.Audit"),
        "the node's reason survives: {e}"
    );
    assert!(matches!(
        client.config(100).unwrap_err(),
        Error::NodeConflict(_)
    ));
    assert!(matches!(
        client.config(100).unwrap_err(),
        Error::BadRequest(_)
    ));
    assert!(matches!(
        client.config(100).unwrap_err(),
        Error::NodeUnavailable(_)
    ));
    let e = client.config(100).unwrap_err();
    assert!(
        matches!(e, Error::NodeNotFound(_)),
        "a 500 'does not exist' is a missing VM: {e}"
    );
    assert!(matches!(
        client.config(100).unwrap_err(),
        Error::HttpStatus(_)
    ));
    assert_eq!(
        node.count("GET", CONFIG),
        7,
        "one request per answer — no retry on a status"
    );
    assert!(client
        .vm_exists(100)
        .unwrap_err()
        .to_string()
        .contains("mock: no script"));
}

#[test]
fn a_password_ticket_is_renewed_once_on_401_and_a_token_never_is() {
    let node = MockNode::start(script(&[
        (
            "GET",
            CONFIG,
            Reply::Json(401, r#"{"data":null,"message":"invalid ticket"}"#.into()),
        ),
        ("GET", CONFIG, ok_data(r#"{"name":"vm100"}"#)),
    ]));
    let client = Client::connect_with(&password_target(&node), fast()).unwrap();
    let cfg = client.config(100).expect("renewed and retried once");
    assert_eq!(cfg["name"], "vm100");
    let log = node.log();
    let routes: Vec<(&str, &str)> = log
        .iter()
        .map(|s| (s.method.as_str(), s.path.as_str()))
        .collect();
    assert_eq!(
        routes,
        vec![
            ("POST", "/access/ticket"),
            ("GET", "/nodes"),
            ("GET", CONFIG),
            ("POST", "/access/ticket"),
            ("GET", CONFIG),
        ]
    );

    let node = MockNode::start(script(&[
        (
            "GET",
            CONFIG,
            Reply::Json(401, r#"{"data":null,"message":"invalid token"}"#.into()),
        ),
        ("GET", CONFIG, ok_data(r#"{"name":"vm100"}"#)),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let e = client.config(100).unwrap_err();
    assert!(matches!(e, Error::Unauthorized(_)), "{e}");
    assert_eq!(
        node.count("POST", "/access/ticket"),
        0,
        "a token is never exchanged for a ticket"
    );
    assert_eq!(
        node.count("GET", CONFIG),
        1,
        "a revoked token is not retried"
    );
}

// ===========================================================================
// Transport
// ===========================================================================

#[test]
fn a_truncated_body_is_a_transport_failure_not_a_malformed_node() {
    let node = MockNode::start(script(&[(
        "GET",
        CONFIG,
        Reply::Truncated {
            claimed: 4096,
            body: r#"{"data":{"na"#.into(),
        },
    )]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let e = client.config(100).unwrap_err();
    assert!(matches!(e, Error::Request(_)), "{e}");
    assert!(e.to_string().contains("reading the answer"), "{e}");
}

#[test]
fn unexpected_json_is_named_for_what_it_is() {
    let node = MockNode::start(script(&[
        ("GET", CONFIG, Reply::Json(200, r#"{"data":"#.into())),
        (
            "POST",
            "/nodes/pve/qemu/100/status/start",
            ok_data(r#""not a task id""#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    assert!(matches!(client.config(100).unwrap_err(), Error::Decode(_)));
    let dir = tempfile::tempdir().unwrap();
    let e = client.start(&Ledger::at(dir.path()), 100).unwrap_err();
    assert!(matches!(e, Error::UnexpectedAnswer(_)), "{e}");
    assert!(
        Ledger::at(dir.path()).records().is_empty(),
        "nothing to wait on, nothing recorded"
    );
}

#[test]
fn a_stalled_answer_hits_the_request_timeout() {
    let node = MockNode::start(script(&[(
        "GET",
        CONFIG,
        Reply::Stall(Duration::from_secs(6)),
    )]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let started = std::time::Instant::now();
    let e = client.config(100).unwrap_err();
    assert!(matches!(e, Error::Request(_)), "{e}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the 3 s request ceiling held"
    );
}

#[test]
fn a_body_past_the_bound_is_refused_not_read() {
    let node = MockNode::start(script(&[(
        "GET",
        CONFIG,
        Reply::Huge(MAX_RESPONSE_BYTES + 1),
    )]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let e = client.config(100).unwrap_err();
    assert!(matches!(e, Error::ResponseTooLarge(_)), "{e}");
}

// ===========================================================================
// Tasks and the ledger
// ===========================================================================

#[test]
fn a_task_verdict_is_read_from_exitstatus_and_the_ledger_records_it() {
    let status = "/nodes/pve/tasks/UPID:pve:0001A2B3:0000C4D5:66F0:qmstart:100:root@pam:/status";
    let node = MockNode::start(script(&[
        (
            "POST",
            "/nodes/pve/qemu/100/status/start",
            ok_data(&format!(r#""{UPID}""#)),
        ),
        ("GET", status, ok_data(r#"{"status":"running"}"#)),
        (
            "GET",
            status,
            ok_data(r#"{"status":"stopped","exitstatus":"OK"}"#),
        ),
        (
            "POST",
            "/nodes/pve/qemu/100/status/stop",
            ok_data(&format!(r#""{UPID}""#)),
        ),
        (
            "GET",
            status,
            ok_data(r#"{"status":"stopped","exitstatus":"VM quit/powerdown failed"}"#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    client.start(&ledger, 100).expect("stopped + OK is success");
    let recs = ledger.records();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].upid, UPID);
    assert_eq!(recs[0].action, "start");
    assert_eq!(recs[0].state, TaskState::Ok);
    assert_eq!(
        node.count("GET", status),
        2,
        "polled until terminal, no more"
    );

    let e = client.stop(&ledger, 100).unwrap_err();
    assert!(matches!(e, Error::TaskFailed(_)), "{e}");
    assert!(
        e.to_string().contains("powerdown failed"),
        "the node's reason: {e}"
    );
    let recs = ledger.records();
    assert_eq!(recs.len(), 2);
    assert!(matches!(&recs[1].state, TaskState::Failed { reason } if reason.contains("powerdown")));
}

/// `WARNINGS: <n>` is the third outcome: the worker finished its work and
/// logged warnings. Measured on PVE 9.2.2 with a container start whose DHCP got
/// no offer — the container ran, without an address. Read as a failure, the
/// start would be retried on top of a running guest; read as a plain `ok`,
/// the missing address would be nobody's news. It is a success with its
/// `WARN:` lines kept in the ledger.
#[test]
fn a_task_that_ends_with_warnings_succeeds_and_the_ledger_keeps_them() {
    let status = "/nodes/pve/tasks/UPID:pve:0001A2B3:0000C4D5:66F0:qmstart:100:root@pam:/status";
    let log = "/nodes/pve/tasks/UPID:pve:0001A2B3:0000C4D5:66F0:qmstart:100:root@pam:/log";
    let node = MockNode::start(script(&[
        (
            "POST",
            "/nodes/pve/qemu/100/status/start",
            ok_data(&format!(r#""{UPID}""#)),
        ),
        (
            "GET",
            status,
            ok_data(r#"{"status":"stopped","exitstatus":"WARNINGS: 1"}"#),
        ),
        (
            "GET",
            log,
            ok_data(
                r#"[{"n":1,"t":"WARN: DHCP failed - command 'dhclient' failed: exit code 2"},{"n":2,"t":""},{"n":3,"t":"TASK WARNINGS: 1"}]"#,
            ),
        ),
        (
            "POST",
            "/nodes/pve/qemu/100/status/start",
            ok_data(&format!(r#""{UPID}""#)),
        ),
        (
            "GET",
            status,
            ok_data(r#"{"status":"stopped","exitstatus":"WARNINGS: 2"}"#),
        ),
        ("GET", log, Reply::Json(500, r#"{"data":null}"#.into())),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());

    client
        .start(&ledger, 100)
        .expect("WARNINGS is not a failure");
    let recs = ledger.records();
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].state,
        TaskState::OkWithWarnings {
            warnings: vec!["DHCP failed - command 'dhclient' failed: exit code 2".into()]
        },
        "the WARN line is kept, the count line is not"
    );
    assert_eq!(
        node.count("POST", "/nodes/pve/qemu/100/status/start"),
        1,
        "not resent"
    );

    // A log that cannot be read still leaves the warnings on record: "no
    // warnings" would be the one false answer.
    client
        .start(&ledger, 100)
        .expect("an unreadable log does not turn warnings into a failure");
    let recs = ledger.records();
    match &recs[1].state {
        TaskState::OkWithWarnings { warnings } => assert!(
            warnings.len() == 1 && warnings[0].contains("2 warning(s)"),
            "{warnings:?}"
        ),
        other => panic!("expected ok_with_warnings, got {other:?}"),
    }
    let on_disk = std::fs::read_to_string(dir.path().join("proxmox-tasks.json")).unwrap();
    assert!(
        on_disk.contains(r#""state": "ok_with_warnings""#),
        "{on_disk}"
    );
}

#[test]
fn a_wait_that_gives_up_is_timed_out_in_the_ledger_and_not_failed() {
    let status = "/nodes/pve/tasks/UPID:pve:0001A2B3:0000C4D5:66F0:qmstart:100:root@pam:/status";
    let mut entries = vec![(
        "POST",
        "/nodes/pve/qemu/100/status/start",
        ok_data(&format!(r#""{UPID}""#)),
    )];
    for _ in 0..40 {
        entries.push(("GET", status, ok_data(r#"{"status":"running"}"#)));
    }
    let node = MockNode::start(script(&entries));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    let e = client.start(&ledger, 100).unwrap_err();
    assert!(matches!(e, Error::TaskTimeout(_)), "{e}");
    assert!(e.to_string().contains("may still be running"), "{e}");
    assert_eq!(ledger.records()[0].state, TaskState::TimedOut);
    // And the record is still the thing to reconcile: a later status read
    // settles it without resending anything.
    assert_eq!(
        ledger.pending().len(),
        0,
        "timed out is a verdict of THIS client, not pending"
    );
}

#[test]
fn a_leftover_task_in_the_ledger_is_waited_for_before_a_new_operation() {
    let status = "/nodes/pve/tasks/UPID:pve:0001A2B3:0000C4D5:66F0:qmstart:100:root@pam:/status";
    let node = MockNode::start(script(&[
        ("GET", status, ok_data(r#"{"status":"running"}"#)),
        (
            "GET",
            status,
            ok_data(r#"{"status":"stopped","exitstatus":"OK"}"#),
        ),
        (
            "GET",
            "/nodes/pve/qemu/100/status/current",
            ok_data(r#"{"status":"stopped"}"#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    // A record an earlier, killed process left behind.
    std::fs::write(
        dir.path().join("proxmox-tasks.json"),
        format!(
            r#"[{{"upid":"{UPID}","node":"pve","action":"start","vmid":100,"started_unix":1,"state":{{"state":"submitted"}}}}]"#
        ),
    )
    .unwrap();
    let ledger = Ledger::at(dir.path());
    assert_eq!(ledger.pending().len(), 1);
    client
        .settle_pending(&ledger, 100)
        .expect("waited for the leftover task");
    assert_eq!(ledger.pending().len(), 0);
    assert_eq!(ledger.records()[0].state, TaskState::Ok);
    let log = node.log();
    let routes: Vec<&str> = log.iter().map(|s| s.path.as_str()).collect();
    // A token logs in with no ticket: the log is `GET /nodes` and then the two
    // status reads — the reconcile's single look, and the wait's.
    assert_eq!(
        &routes[1..],
        &[status, status],
        "settled first, then nothing else was sent"
    );
}

// ===========================================================================
// A lost answer is not a lost request
// ===========================================================================

#[test]
fn a_lost_answer_finds_the_running_task_instead_of_resending() {
    let create_upid = "UPID:pve:0001A2B3:0000C4D5:66F0:qmcreate:100:root@pam:";
    let status = format!("/nodes/pve/tasks/{create_upid}/status");
    let node = MockNode::start(script(&[
        ("POST", "/nodes/pve/qemu", Reply::Drop),
        (
            "GET",
            "/nodes/pve/tasks",
            ok_data(&format!(
                r#"[{{"upid":"{create_upid}","type":"qmcreate","status":"running"}}]"#
            )),
        ),
        (
            "GET",
            &status,
            ok_data(r#"{"status":"stopped","exitstatus":"OK"}"#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    let cfg = delonix_compute::vm_backend::VmConfig {
        name: "delonix-test-lost".into(),
        ..Default::default()
    };
    client
        .create_vm(&ledger, 100, "delonix-test-lost", &cfg, "local-lvm", 2)
        .expect("the task the node was running finished OK");
    assert_eq!(node.count("POST", "/nodes/pve/qemu"), 1, "NEVER resent");
    assert_eq!(node.count("GET", "/nodes/pve/tasks"), 1);
    let recs = ledger.records();
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].upid, create_upid,
        "the node's task, recorded as ours"
    );
    assert_eq!(recs[0].state, TaskState::Ok);
}

#[test]
fn a_lost_answer_with_the_effect_already_there_is_accepted_without_a_task() {
    let node = MockNode::start(script(&[
        ("POST", "/nodes/pve/qemu/100/status/start", Reply::Drop),
        ("GET", "/nodes/pve/tasks", ok_data("[]")),
        (
            "GET",
            "/nodes/pve/qemu/100/status/current",
            ok_data(r#"{"status":"running"}"#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    client
        .start(&Ledger::at(dir.path()), 100)
        .expect("running already: done");
    assert_eq!(node.count("POST", "/nodes/pve/qemu/100/status/start"), 1);
}

/// `POST …/config` is the node's asynchronous config API: `null` means it
/// applied the change inline, a UPID means a worker — and the first version
/// of `configure_clone` read every answer as `null` and never waited. Both
/// shapes here, and the UPID one has to reach the ledger like any task.
#[test]
fn a_config_change_is_done_on_null_and_waited_on_when_it_answers_a_task() {
    let cfg_upid = "UPID:pve:0001A2B3:0000C4D5:66F0:qmconfig:100:root@pam:";
    let status = format!("/nodes/pve/tasks/{cfg_upid}/status");
    let node = MockNode::start(script(&[
        ("POST", CONFIG, ok_data("null")),
        ("POST", CONFIG, ok_data(&format!(r#""{cfg_upid}""#))),
        (
            "GET",
            &status,
            ok_data(r#"{"status":"stopped","exitstatus":"OK"}"#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    let cfg = VmConfig {
        name: "v".into(),
        disk: "template:9000".into(),
        vcpus: 2,
        memory: "1G".into(),
        ..Default::default()
    };
    client
        .configure_clone(&ledger, 100, &cfg)
        .expect("null: applied inline");
    assert!(ledger.records().is_empty(), "no task, nothing to record");
    client
        .configure_clone(&ledger, 100, &cfg)
        .expect("a UPID: waited on");
    let recs = ledger.records();
    assert_eq!(recs.len(), 1, "the forked config task is in the ledger");
    assert_eq!(recs[0].action, "configure");
    assert_eq!(recs[0].state, TaskState::Ok);
    assert_eq!(node.count("POST", CONFIG), 2, "nothing was resent");
    assert_eq!(node.count("GET", &status), 1, "polled to its verdict");
}

/// `DELETE …/snapshot/{snapname}` is a task (`qmdelsnapshot`) like every
/// other write; a name the VM does not have is refused before any request.
#[test]
fn a_snapshot_delete_is_a_task_and_a_missing_name_is_not_found() {
    let del_upid = "UPID:pve:0001A2B3:0000C4D5:66F0:qmdelsnapshot:100:root@pam:";
    let status = format!("/nodes/pve/tasks/{del_upid}/status");
    let list = "/nodes/pve/qemu/100/snapshot";
    let node = MockNode::start(script(&[
        (
            "GET",
            list,
            ok_data(r#"[{"name":"s1"},{"name":"current"}]"#),
        ),
        (
            "DELETE",
            "/nodes/pve/qemu/100/snapshot/s1",
            ok_data(&format!(r#""{del_upid}""#)),
        ),
        (
            "GET",
            &status,
            ok_data(r#"{"status":"stopped","exitstatus":"OK"}"#),
        ),
        ("GET", list, ok_data(r#"[{"name":"current"}]"#)),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    client.delete_snapshot(&ledger, 100, "s1").expect("deleted");
    let recs = ledger.records();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].action, "delete-snapshot");
    assert_eq!(recs[0].state, TaskState::Ok);
    let e = client.delete_snapshot(&ledger, 100, "s1").unwrap_err();
    assert!(matches!(e, Error::SnapshotNotFound(_)), "{e}");
    assert_eq!(
        node.count("DELETE", "/nodes/pve/qemu/100/snapshot/s1"),
        1,
        "a missing name never reaches the node"
    );
}

#[test]
fn a_lost_answer_with_nothing_on_the_node_stays_a_transport_error() {
    let node = MockNode::start(script(&[
        ("POST", "/nodes/pve/qemu", Reply::Drop),
        ("GET", "/nodes/pve/tasks", ok_data("[]")),
        (
            "GET",
            CONFIG,
            Reply::Json(500, r#"{"data":null,"message":"Configuration file 'nodes/pve/qemu-server/100.conf' does not exist\n"}"#.into()),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cfg = delonix_compute::vm_backend::VmConfig {
        name: "delonix-test-lost".into(),
        ..Default::default()
    };
    let e = client
        .create_vm(
            &Ledger::at(dir.path()),
            100,
            "delonix-test-lost",
            &cfg,
            "local-lvm",
            2,
        )
        .unwrap_err();
    assert!(matches!(e, Error::Request(_)), "{e}");
    assert_eq!(
        node.count("POST", "/nodes/pve/qemu"),
        1,
        "still never resent — the caller decides"
    );
    assert!(Ledger::at(dir.path()).records().is_empty());
}

// ===========================================================================
// The guest agent: ping, exec, exec-status
// ===========================================================================

const AGENT_NOT_RUNNING: &str = r#"{"data":null,"message":"QEMU guest agent is not running\n"}"#;

/// `agent_ping` is the one call in this file where a failure is not the
/// whole story: the node's OWN "no agent" answer has to read as `Ok(false)`,
/// never as an [`Error`], while a real failure on the exact same route still
/// propagates. And none of it is a node task — no UPID, no ledger entry.
#[test]
fn agent_ping_tells_no_agent_apart_from_a_real_failure() {
    let ping = "/nodes/pve/qemu/100/agent/ping";
    let node = MockNode::start(script(&[
        ("POST", ping, ok_data("{}")),
        ("POST", ping, Reply::Json(500, AGENT_NOT_RUNNING.into())),
        (
            "POST",
            ping,
            Reply::Json(
                500,
                r#"{"data":null,"message":"unable to open file"}"#.into(),
            ),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    assert!(
        matches!(client.agent_ping(100), Ok(true)),
        "the agent answered"
    );
    assert!(
        matches!(client.agent_ping(100), Ok(false)),
        "no agent is not a failure"
    );
    let e = client.agent_ping(100).unwrap_err();
    assert!(matches!(e, Error::HttpStatus(_)), "{e}");
    assert_eq!(
        node.count("POST", ping),
        3,
        "each ping sent once, never resent"
    );
}

/// `command` is a REPEATED form field, one per argv element — the agent
/// takes an argv array, and joining `["ls", "-la", "/tmp"]` into one string
/// would hand it a single argument that happens to contain spaces.
#[test]
fn agent_exec_sends_argv_as_repeated_command_fields() {
    let exec = "/nodes/pve/qemu/100/agent/exec";
    let node = MockNode::start(script(&[("POST", exec, ok_data(r#"{"pid":4711}"#))]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let pid = client.agent_exec(100, &["ls", "-la", "/tmp"]).unwrap();
    assert_eq!(pid, 4711);
    let sent = node
        .log()
        .into_iter()
        .find(|s| s.method == "POST" && s.path == exec)
        .expect("the exec request reached the node")
        .body;
    // `Seen.body` is already percent-DECODED (`serve_one` does that for every
    // captured request, the same as it does for the path), so a literal `/`
    // reads back as `/` here even though the wire form encoded it `%2F`.
    assert_eq!(sent, "command=ls&command=-la&command=/tmp");
}

/// `exited == 0` while the guest process is still running — `exitcode`/
/// `out-data`/`err-data` are absent then, not zero/empty, and reading them
/// anyway would report a stale result while the command is still going.
#[test]
fn agent_exec_status_distinguishes_running_from_finished() {
    // No `?pid=…`: `serve_one` strips the query before matching a script
    // entry (the matrix is keyed by route, not by arguments — the same
    // reason `trace_route` drops it), so the key here has to be the bare path.
    let status = "/nodes/pve/qemu/100/agent/exec-status";
    let node = MockNode::start(script(&[
        ("GET", status, ok_data(r#"{"exited":0}"#)),
        (
            "GET",
            status,
            ok_data(r#"{"exited":1,"exitcode":0,"out-data":"hi\n","err-data":""}"#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    assert_eq!(
        client.agent_exec_status(100, 4711).unwrap(),
        AgentExecStatus::Running
    );
    assert_eq!(
        client.agent_exec_status(100, 4711).unwrap(),
        AgentExecStatus::Finished {
            exit_code: 0,
            stdout: "hi\n".into(),
            stderr: String::new(),
            signal: None,
        }
    );
}

/// `agent_exec_wait` issues ONE `exec` and polls `exec-status` until it
/// reports finished — proving the poll loop drives real HTTP responses, not
/// just the pure translation [`agent_exec_status_of`] already covers.
#[test]
fn agent_exec_wait_polls_until_finished_and_issues_exec_once() {
    let exec = "/nodes/pve/qemu/100/agent/exec";
    // No `?pid=…`: `serve_one` strips the query before matching a script
    // entry, so the key has to be the bare path — the single pid the mock
    // ever hands back (99) makes that unambiguous here.
    let status = "/nodes/pve/qemu/100/agent/exec-status";
    let node = MockNode::start(script(&[
        ("POST", exec, ok_data(r#"{"pid":99}"#)),
        ("GET", status, ok_data(r#"{"exited":0}"#)),
        ("GET", status, ok_data(r#"{"exited":0}"#)),
        (
            "GET",
            status,
            ok_data(r#"{"exited":1,"exitcode":3,"out-data":"","err-data":"nope"}"#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let outcome = client
        .agent_exec_wait(100, &["false"], Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        outcome,
        AgentExecStatus::Finished {
            exit_code: 3,
            stdout: String::new(),
            stderr: "nope".into(),
            signal: None,
        }
    );
    assert_eq!(node.count("POST", exec), 1, "issued exactly once");
    assert_eq!(node.count("GET", status), 3, "polled until finished");
}

/// A command still running when `timeout` elapses is a [`Error::TaskTimeout`]
/// — not proof the guest command failed; it may still be running there.
#[test]
fn agent_exec_wait_gives_up_at_its_timeout() {
    let exec = "/nodes/pve/qemu/100/agent/exec";
    // No `?pid=…`: `serve_one` strips the query before matching a script
    // entry, so the key has to be the bare path.
    let status = "/nodes/pve/qemu/100/agent/exec-status";
    // A generous supply of "still running" — enough to outlast the 500ms
    // window at the fixed 200ms poll interval with room for scheduling
    // jitter. A script that ran dry would fall back to the mock's stock 500
    // ("no script for this route"), which would fail the wait for the wrong
    // reason before the timeout ever had a chance to fire.
    let mut entries = vec![("POST", exec, ok_data(r#"{"pid":7}"#))];
    for _ in 0..20 {
        entries.push(("GET", status, ok_data(r#"{"exited":0}"#)));
    }
    let node = MockNode::start(script(&entries));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let e = client
        .agent_exec_wait(100, &["sleep", "999"], Duration::from_millis(500))
        .unwrap_err();
    assert!(matches!(e, Error::TaskTimeout(_)), "{e}");
    assert!(e.to_string().contains("pid 7"), "{e}");
    assert_eq!(
        node.count("POST", exec),
        1,
        "the guest process is never re-started"
    );
}

// ===========================================================================
// Secrets and the route trace
// ===========================================================================

#[test]
fn the_secret_reaches_no_error_no_debug_output_and_no_trace_file() {
    let node = MockNode::start(script(&[
        (
            "GET",
            CONFIG,
            Reply::Json(401, r#"{"data":null,"message":"invalid token"}"#.into()),
        ),
        (
            "GET",
            CONFIG,
            Reply::Json(500, r#"{"data":null,"message":"boom"}"#.into()),
        ),
        ("GET", CONFIG, Reply::Drop),
    ]));
    let target = token_target(&node);
    let trace = tempfile::NamedTempFile::new().unwrap();
    let opts = ClientOptions {
        trace_routes: Some(trace.path().to_path_buf()),
        ..fast()
    };
    let client = Client::connect_with(&target, opts).unwrap();
    let mut texts = vec![format!("{target:?}")];
    for _ in 0..3 {
        let e = client.config(100).unwrap_err();
        texts.push(e.to_string());
        texts.push(format!("{e:?}"));
        texts.push(delonix_model::Error::from(e).to_string());
    }
    for t in &texts {
        assert!(!t.contains("the-token-secret-value"), "leaked: {t}");
    }
    let traced = std::fs::read_to_string(trace.path()).unwrap();
    assert!(!traced.contains("the-token-secret-value"));
    assert!(traced.contains("GET /nodes\n"), "{traced}");
    assert!(traced.contains(&format!("GET {CONFIG}\n")), "{traced}");
}

/// ADR-0052: a `scope: vm` policy on a cluster whose DATACENTER firewall is
/// off is refused with DX-6508 — and refused BEFORE anything is written. The
/// node's answer here is the one measured on PVE 9.2.2 for a cluster that
/// never had it on: a bare `digest`, no `enable` key at all.
#[test]
fn a_vm_policy_on_a_cluster_with_its_firewall_off_is_refused_before_any_write() {
    let node = MockNode::start(script(&[(
        "GET",
        "/cluster/firewall/options",
        ok_data(r#"{"digest":"da39a3ee5e6b4b0d3255bfef95601890afd80709"}"#),
    )]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let policy = delonix_compute::vm_firewall::Policy {
        direction: delonix_compute::vm_firewall::Direction::In,
        default_allow: false,
        rules: vec![],
    };
    let err = delonix_proxmox::vm_firewall::apply(&client, &Ledger::at(dir.path()), 100, &policy)
        .expect_err("a datacenter firewall that is off must refuse");
    assert_eq!(err.number(), 6508, "{err}");
    let writes: Vec<Seen> = node
        .log()
        .into_iter()
        .filter(|s| s.method != "GET" && !s.path.ends_with("/access/ticket"))
        .collect();
    assert!(
        writes.is_empty(),
        "the refusal wrote to the node: {writes:?}"
    );
}

// ===========================================================================
// Power operations: shutdown, reboot, reset, suspend, resume
// ===========================================================================

/// `…/status/suspend` forks a task the node lists as `qmpause` — measured on
/// PVE 9.2.2, and NOT the `qmsuspend` the path suggests. A lost answer has to
/// be found under the name the node actually uses, or the recovery falls
/// through to its probe while the suspend is still in flight.
#[test]
fn a_lost_suspend_is_found_as_the_qmpause_task_the_node_runs() {
    let upid = "UPID:pve:0001A2B3:0000C4D5:66F0:qmpause:100:root@pam:";
    let status = format!("/nodes/pve/tasks/{upid}/status");
    let node = MockNode::start(script(&[
        ("POST", "/nodes/pve/qemu/100/status/suspend", Reply::Drop),
        (
            "GET",
            "/nodes/pve/tasks",
            ok_data(&format!(
                r#"[{{"upid":"{upid}","type":"qmpause","status":"running"}}]"#
            )),
        ),
        (
            "GET",
            &status,
            ok_data(r#"{"status":"stopped","exitstatus":"OK"}"#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    client
        .suspend(&ledger, 100)
        .expect("the qmpause task the node was running finished OK");
    assert_eq!(
        node.count("POST", "/nodes/pve/qemu/100/status/suspend"),
        1,
        "NEVER resent"
    );
    let recs = ledger.records();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].upid, upid, "the node's task, recorded as ours");
    assert_eq!(recs[0].state, TaskState::Ok);
}

/// A suspended VM still answers `status: running`; only `qmpstatus` says
/// `paused`. The lost-answer probe has to read the second field, or a
/// suspend that never happened would be accepted.
#[test]
fn a_lost_suspend_is_accepted_only_when_qmpstatus_says_paused() {
    let paused = MockNode::start(script(&[
        ("POST", "/nodes/pve/qemu/100/status/suspend", Reply::Drop),
        ("GET", "/nodes/pve/tasks", ok_data("[]")),
        (
            "GET",
            "/nodes/pve/qemu/100/status/current",
            ok_data(r#"{"status":"running","qmpstatus":"paused"}"#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&paused), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    client
        .suspend(&Ledger::at(dir.path()), 100)
        .expect("paused already: done");

    let running = MockNode::start(script(&[
        ("POST", "/nodes/pve/qemu/100/status/suspend", Reply::Drop),
        ("GET", "/nodes/pve/tasks", ok_data("[]")),
        (
            "GET",
            "/nodes/pve/qemu/100/status/current",
            ok_data(r#"{"status":"running","qmpstatus":"running"}"#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&running), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = client
        .suspend(&Ledger::at(dir.path()), 100)
        .expect_err("`status: running` alone is not a suspended VM");
    assert!(matches!(err, Error::Request(_)), "{err:?}");
    assert_eq!(
        running.count("POST", "/nodes/pve/qemu/100/status/suspend"),
        1,
        "a lost answer is never resent"
    );
}

/// A guest that ignores ACPI makes the node's shutdown task FAIL after the
/// timeout (measured: «VM quit/powerdown failed - got timeout»). That is an
/// error here, recorded as `failed` — never read as «it went down».
#[test]
fn a_shutdown_the_guest_ignores_is_a_failure_and_the_form_carries_the_timeout() {
    let upid = "UPID:pve:0001A2B3:0000C4D5:66F0:qmshutdown:100:root@pam:";
    let status = format!("/nodes/pve/tasks/{upid}/status");
    let node = MockNode::start(script(&[
        (
            "POST",
            "/nodes/pve/qemu/100/status/shutdown",
            ok_data(&format!(r#""{upid}""#)),
        ),
        (
            "GET",
            &status,
            ok_data(
                r#"{"status":"stopped","exitstatus":"VM quit/powerdown failed - got timeout"}"#,
            ),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), slow_task()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    let err = client
        .shutdown(&ledger, 100, Some(Duration::from_secs(5)), false)
        .expect_err("a guest that did not go down is not a successful shutdown");
    assert!(matches!(err, Error::TaskFailed(_)), "{err:?}");
    assert!(err.to_string().contains("powerdown failed"), "{err}");
    let sent = node
        .log()
        .into_iter()
        .find(|s| s.path == "/nodes/pve/qemu/100/status/shutdown")
        .unwrap();
    assert_eq!(sent.body, "timeout=5", "no forceStop unless asked");
    assert!(matches!(
        ledger.records()[0].state,
        TaskState::Failed { .. }
    ));
}

/// `force_stop` and the timeout both reach the node, and a timeout this
/// client could not wait out is refused before any request.
#[test]
fn a_forced_shutdown_sends_force_stop_and_an_unwaitable_timeout_is_refused_first() {
    let node = MockNode::start(script(&[(
        "POST",
        "/nodes/pve/qemu/100/status/shutdown",
        ok_data(r#""UPID:pve:0001A2B3:0000C4D5:66F0:qmshutdown:100:root@pam:""#),
    )]));
    let client = Client::connect_with(&token_target(&node), slow_task()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    client
        .shutdown(&ledger, 100, Some(Duration::from_secs(5)), true)
        .expect("forced shutdown");
    let sent = node
        .log()
        .into_iter()
        .find(|s| s.path == "/nodes/pve/qemu/100/status/shutdown")
        .unwrap();
    assert_eq!(sent.body, "timeout=5&forceStop=1");

    let before = node.count("POST", "/nodes/pve/qemu/100/status/shutdown");
    let err = client
        .shutdown(&ledger, 100, Some(Duration::from_secs(100_000)), true)
        .expect_err("a timeout past the client's task deadline");
    assert!(matches!(err, Error::InvalidPowerTimeout(_)), "{err:?}");
    let err = client
        .reboot(&ledger, 100, Some(Duration::from_secs(100_000)))
        .expect_err("the same bound applies to a reboot");
    assert!(matches!(err, Error::InvalidPowerTimeout(_)), "{err:?}");
    assert_eq!(
        node.count("POST", "/nodes/pve/qemu/100/status/shutdown"),
        before,
        "refused before any request"
    );
    assert_eq!(node.count("POST", "/nodes/pve/qemu/100/status/reboot"), 0);
}

/// [`fast`] with room for a 5 s guest timeout inside the task deadline.
fn slow_task() -> ClientOptions {
    ClientOptions {
        task_timeout: Duration::from_secs(30),
        ..fast()
    }
}

// ===========================================================================
// Cold resize: POST …/config, then read back config and pending
// ===========================================================================

const RCONFIG: &str = "/nodes/pve/qemu/100/config";
const RPENDING: &str = "/nodes/pve/qemu/100/pending";

/// Applied inline (`null`), config carries the numbers sent (memory as the
/// string PVE 8+ uses), nothing pending: done — and the form says
/// `sockets=1`, so a two-socket template clone does not get twice the vCPUs.
#[test]
fn a_resize_is_done_only_when_config_and_pending_agree() {
    let node = MockNode::start(script(&[
        ("POST", RCONFIG, ok_data("null")),
        (
            "GET",
            RCONFIG,
            ok_data(r#"{"cores":2,"sockets":1,"memory":"768"}"#),
        ),
        ("GET", RPENDING, ok_data(r#"[{"key":"cores","value":2}]"#)),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    client
        .resize_hardware(&Ledger::at(dir.path()), 100, 2, 768)
        .expect("config and pending agree");
    let sent = node
        .log()
        .into_iter()
        .find(|s| s.method == "POST" && s.path == RCONFIG)
        .unwrap();
    assert_eq!(sent.body, "cores=2&sockets=1&memory=768");
}

/// A config that does not carry what was sent is an unexpected answer, not
/// a resize — whatever the POST said.
#[test]
fn a_resize_whose_config_does_not_read_back_is_an_error() {
    let node = MockNode::start(script(&[
        ("POST", RCONFIG, ok_data("null")),
        (
            "GET",
            RCONFIG,
            ok_data(r#"{"cores":1,"sockets":2,"memory":"512"}"#),
        ),
        ("GET", RPENDING, ok_data("[]")),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = client
        .resize_hardware(&Ledger::at(dir.path()), 100, 2, 768)
        .expect_err("the node did not record the new size");
    assert!(matches!(err, Error::UnexpectedAnswer(_)), "{err:?}");
}

/// A VM the node still runs holds the change as PENDING until its next
/// boot. That is not a resize, and it is said as one: the key is named.
#[test]
fn a_resize_left_pending_is_an_error_that_names_the_key() {
    let node = MockNode::start(script(&[
        ("POST", RCONFIG, ok_data("null")),
        (
            "GET",
            RCONFIG,
            ok_data(r#"{"cores":2,"sockets":1,"memory":"768"}"#),
        ),
        (
            "GET",
            RPENDING,
            ok_data(r#"[{"key":"memory","value":"512","pending":"768"}]"#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = client
        .resize_hardware(&Ledger::at(dir.path()), 100, 2, 768)
        .expect_err("a pending change is not applied");
    assert!(matches!(err, Error::UnexpectedAnswer(_)), "{err:?}");
    assert!(err.to_string().contains("memory"), "{err}");
    assert!(err.to_string().contains("PENDING"), "{err}");
}

// ===========================================================================
// Extra disks/NICs: refused on a template clone BEFORE anything is asked
// ===========================================================================

/// A template clone with `extraDisks` is refused before `next_vmid`: the
/// template may already hold the slot, and writing it would detach the
/// template's own device. The node sees no request past the login.
#[test]
fn extra_devices_on_a_template_clone_are_refused_before_any_request() {
    use delonix_compute::vm_backend::{CreateStage, VmBackend};
    use delonix_compute::ExtraDisk;
    let node = MockNode::start(script(&[]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let before = node.log().len();
    let b = delonix_proxmox::ProxmoxBackend::sharing(std::sync::Arc::new(client));
    let dir = tempfile::tempdir().unwrap();
    let cfg = VmConfig {
        name: "x".into(),
        disk: "template:9000".into(),
        vcpus: 1,
        memory: "512M".into(),
        extra_disks: vec![ExtraDisk {
            source: "local-lvm:1".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let stage = |_: CreateStage| {};
    let Err(err) = b.boot(dir.path(), &cfg, &cfg.disk, &stage) else {
        panic!("a template clone cannot take extra devices");
    };
    assert_eq!(err.number(), 1524, "{err}");
    assert!(err.to_string().contains("template clone"), "{err}");
    assert_eq!(
        node.log().len(),
        before,
        "the refusal reached the node: {:?}",
        node.log()
    );
}

// ===========================================================================
// Cloud-init change: the node's rendering is the proof, not the writes
// ===========================================================================

/// The config write and the regenerate both answer `null` (done), nothing is
/// pending — and the node's rendered user-data does not carry the new key.
/// That is an unexpected answer that names what is missing, never a success.
#[test]
fn a_cloud_init_change_the_rendering_does_not_carry_is_an_error() {
    let node = MockNode::start(script(&[
        ("POST", "/nodes/pve/qemu/100/config", ok_data("null")),
        ("PUT", "/nodes/pve/qemu/100/cloudinit", ok_data("null")),
        ("GET", "/nodes/pve/qemu/100/cloudinit", ok_data("[]")),
        (
            "GET",
            "/nodes/pve/qemu/100/cloudinit/dump",
            ok_data(r##""#cloud-config\nhostname: web-1\nuser: ops\n""##),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let intent = delonix_compute::vm_backend::CloudInitIntent {
        hostname: Some("web-1".into()),
        ci_user: Some("ops".into()),
        ssh_keys: vec!["ssh-ed25519 AAAA k1".into()],
    };
    let err = client
        .update_cloud_init(&Ledger::at(dir.path()), 100, "web-1", &intent)
        .expect_err("a key the rendering lacks is not applied");
    assert!(matches!(err, Error::UnexpectedAnswer(_)), "{err:?}");
    assert!(err.to_string().contains("ssh key #1"), "{err}");
    let sent = node
        .log()
        .into_iter()
        .find(|s| s.method == "POST" && s.path == "/nodes/pve/qemu/100/config")
        .unwrap();
    assert!(
        sent.body.starts_with("name=web-1&ciuser=ops&sshkeys="),
        "{}",
        sent.body
    );
}

// ===========================================================================
// ADR-0053 decisions 2 and 3: each VM on its own node, and a moved VM found
// ===========================================================================

fn vm_with_handle(handle: &str) -> delonix_compute::Vm {
    delonix_compute::Vm::new(
        "v".into(),
        "local-lvm:1".into(),
        "local-lvm:1".into(),
        1,
        "512M".into(),
        String::new(),
        String::new(),
        String::new(),
        handle.into(),
    )
}

fn backend_on(node: &MockNode) -> delonix_proxmox::ProxmoxBackend {
    let client = Client::connect_with(&token_target(node), fast()).unwrap();
    delonix_proxmox::ProxmoxBackend::sharing(Arc::new(client))
}

const PVE_STATUS: &str = "/nodes/pve/qemu/100/status/current";
const PVE2_STATUS: &str = "/nodes/pve2/qemu/100/status/current";
const RESOURCES: &str = "/cluster/resources";

/// Decision 2: the handle's node is the node the VM is addressed on. The
/// configured node (`pve`, the API entry point) is never asked about it.
#[test]
fn a_vm_is_addressed_on_the_node_its_handle_names() {
    use delonix_compute::vm_backend::VmBackend;
    let node = MockNode::start(script(&[(
        "GET",
        PVE2_STATUS,
        ok_data(r#"{"status":"running"}"#),
    )]));
    let b = backend_on(&node);
    let vm = vm_with_handle("proxmox:pve2:100");
    assert!(b.is_running(&vm), "the VM on pve2 is running");
    assert_eq!(node.count("GET", PVE2_STATUS), 1);
    assert_eq!(
        node.count("GET", PVE_STATUS),
        0,
        "the configured node was asked"
    );
    assert_eq!(
        node.count("GET", RESOURCES),
        0,
        "no search when the handle is right"
    );
    assert_eq!(b.current_handle(&vm), None, "nothing moved");
}

/// Decision 3: the handle's node says the VM does not exist; ONE
/// `/cluster/resources` read finds it on `pve2`; the call is retried there,
/// the move is remembered — the next call goes straight to `pve2` — and
/// `current_handle` gives the engine the new handle to persist.
#[test]
fn a_vm_moved_outside_the_engine_is_found_once_and_remembered() {
    use delonix_compute::vm_backend::VmBackend;
    let node = MockNode::start(script(&[
        (
            "GET",
            PVE_STATUS,
            Reply::Json(404, r#"{"data":null}"#.into()),
        ),
        (
            "GET",
            RESOURCES,
            ok_data(
                r#"[{"type":"qemu","vmid":100,"node":"pve2"},{"type":"qemu","vmid":101,"node":"pve"}]"#,
            ),
        ),
        ("GET", PVE2_STATUS, ok_data(r#"{"status":"running"}"#)),
        ("GET", PVE2_STATUS, ok_data(r#"{"status":"stopped"}"#)),
    ]));
    let b = backend_on(&node);
    let vm = vm_with_handle("proxmox:pve:100");
    assert!(b.is_running(&vm), "found on pve2, running there");
    assert_eq!(b.current_handle(&vm).as_deref(), Some("proxmox:pve2:100"));
    assert!(!b.is_running(&vm), "the second answer from pve2");
    assert_eq!(
        node.count("GET", PVE_STATUS),
        1,
        "the old node is asked once"
    );
    assert_eq!(
        node.count("GET", RESOURCES),
        1,
        "one search, then remembered"
    );
    assert_eq!(node.count("GET", PVE2_STATUS), 2);
}

/// A VM the cluster does not list, or lists on two nodes, is not followed:
/// the original not-found stands (class 4), and nothing is remembered.
#[test]
fn a_vm_the_cluster_cannot_place_keeps_its_not_found() {
    use delonix_compute::vm_backend::VmBackend;
    for listing in [
        "[]",
        r#"[{"type":"qemu","vmid":100,"node":"pve2"},{"type":"qemu","vmid":100,"node":"pve3"}]"#,
    ] {
        let node = MockNode::start(script(&[
            (
                "GET",
                PVE_STATUS,
                Reply::Json(404, r#"{"data":null}"#.into()),
            ),
            ("GET", RESOURCES, ok_data(listing)),
        ]));
        let b = backend_on(&node);
        let vm = vm_with_handle("proxmox:pve:100");
        let dir = tempfile::tempdir().unwrap();
        let err = b
            .stop(dir.path(), &vm)
            .expect_err("a VM nobody can place is not found");
        assert!(err.is_not_found(), "{listing}: {err}");
        assert_eq!(b.current_handle(&vm), None, "{listing}: nothing remembered");
        assert_eq!(node.count("GET", RESOURCES), 1);
    }
}

// ===========================================================================
// vm move --node (ADR-0053 decisions 1, 4 and 5)
// ===========================================================================

const MIGRATE: &str = "/nodes/pve/qemu/100/migrate";
const TWO_NODES: &str = r#"[{"node":"pve","status":"online"},{"node":"pve2","status":"online"}]"#;

/// The node answers for a scenario: the node list (read at connect and again
/// by the move), the VM's power state, and whatever else the case needs.
fn move_node(power: &str, nodes: &str, extra: Vec<(&str, &str, Reply)>) -> MockNode {
    let mut entries = vec![
        ("GET", "/nodes", ok_data(nodes)),
        ("GET", "/nodes", ok_data(nodes)),
        ("GET", PVE_STATUS, ok_data(power)),
    ];
    entries.extend(extra);
    MockNode::start(script(&entries))
}

/// Every refusal is decided before the node is asked to move anything —
/// and one on the target itself before the precheck is even sent. Checked
/// against what the node RECEIVED, never against the error alone.
#[test]
fn a_refused_move_never_sends_the_migrate() {
    use delonix_compute::vm_backend::VmBackend;
    let stopped = r#"{"status":"stopped","qmpstatus":"stopped"}"#;
    let running = r#"{"status":"running","qmpstatus":"running"}"#;
    let local = ok_data(
        r#"{"allowed_nodes":["pve2"],"local_disks":[{"volid":"local-lvm:vm-100-disk-0"}],
            "local_resources":[],"not_allowed_nodes":{"pve2":{}},"running":0}"#,
    );
    let offline = r#"[{"node":"pve","status":"online"},{"node":"pve2","status":"offline"}]"#;
    let cases: Vec<(&str, MockNode, &str, bool, u16, bool)> = vec![
        (
            "the node it is on",
            move_node(stopped, TWO_NODES, vec![]),
            "pve",
            false,
            1538,
            false,
        ),
        (
            "not a member",
            move_node(stopped, TWO_NODES, vec![]),
            "pve3",
            false,
            1538,
            false,
        ),
        (
            "offline",
            move_node(stopped, offline, vec![]),
            "pve2",
            false,
            1538,
            false,
        ),
        (
            "running, offline move",
            move_node(running, TWO_NODES, vec![]),
            "pve2",
            false,
            5507,
            false,
        ),
        (
            "stopped, --live",
            move_node(stopped, TWO_NODES, vec![]),
            "pve2",
            true,
            5507,
            false,
        ),
        (
            "a local disk",
            move_node(stopped, TWO_NODES, vec![("GET", MIGRATE, local)]),
            "pve2",
            false,
            5507,
            true,
        ),
    ];
    for (what, node, target, live, number, prechecked) in cases {
        let b = backend_on(&node);
        let dir = tempfile::tempdir().unwrap();
        let vm = vm_with_handle("proxmox:pve:100");
        let err = b
            .move_to_node(dir.path(), &vm, target, &mv(live))
            .expect_err(what);
        assert_eq!(err.number(), number, "{what}: {err}");
        assert_eq!(node.count("POST", MIGRATE), 0, "{what}: the move was sent");
        assert_eq!(
            node.count("GET", MIGRATE),
            usize::from(prechecked),
            "{what}: precheck count"
        );
        if what == "a local disk" {
            let msg = err.to_string();
            assert!(msg.contains("local-lvm:vm-100-disk-0"), "{msg}");
            assert!(
                msg.contains("--with-local-disks"),
                "the refusal names the flag: {msg}"
            );
        }
    }
}

/// A local CD-ROM is refused even when disk copying is asked for — the node
/// never copies one — and so is a malformed `--target-storage`, both before
/// the node is asked to move anything.
#[test]
fn a_disk_copying_move_refuses_a_cdrom_and_a_bad_storage_id() {
    use delonix_compute::vm_backend::{MoveOptions, VmBackend};
    let stopped = r#"{"status":"stopped","qmpstatus":"stopped"}"#;
    let cdrom = ok_data(
        r#"{"allowed_nodes":["pve2"],"local_disks":[
              {"volid":"local:iso/debian.iso","cdrom":1},
              {"volid":"local-lvm:vm-100-disk-0","cdrom":0}],
            "local_resources":[],"not_allowed_nodes":{"pve2":{}},"running":0}"#,
    );
    let copy = MoveOptions {
        with_local_disks: true,
        ..Default::default()
    };
    let node = move_node(stopped, TWO_NODES, vec![("GET", MIGRATE, cdrom)]);
    let b = backend_on(&node);
    let dir = tempfile::tempdir().unwrap();
    let vm = vm_with_handle("proxmox:pve:100");
    let err = b
        .move_to_node(dir.path(), &vm, "pve2", &copy)
        .expect_err("a local CD-ROM is refused");
    assert_eq!(err.number(), 5507, "{err}");
    let msg = err.to_string();
    assert!(msg.contains("local:iso/debian.iso"), "{msg}");
    assert!(
        !msg.contains("vm-100-disk-0"),
        "a disk is not a CD-ROM: {msg}"
    );
    assert_eq!(node.count("POST", MIGRATE), 0, "the move was sent");

    let node = move_node(stopped, TWO_NODES, vec![]);
    let b = backend_on(&node);
    let bad = MoveOptions {
        with_local_disks: true,
        target_storage: Some("-oops".into()),
        ..Default::default()
    };
    let err = b
        .move_to_node(dir.path(), &vm, "pve2", &bad)
        .expect_err("a malformed storage id is refused");
    assert_eq!(err.number(), 1538, "{err}");
    assert_eq!(node.count("GET", MIGRATE), 0, "refused before the precheck");
    assert_eq!(node.count("POST", MIGRATE), 0, "the move was sent");
}

/// With disk copying asked and a target storage named, the move goes ahead
/// even though the precheck says the target lacks the SOURCE storage — the
/// mapping is what `targetstorage` is for, and the node checks it — and the
/// request carries both parameters.
#[test]
fn a_disk_copying_move_sends_the_copy_and_the_target_storage() {
    use delonix_compute::vm_backend::{MoveOptions, VmBackend};
    const MIGRATE_UPID: &str = "UPID:pve:00000972:00004698:6AB793FF:qmigrate:100:root@pam:";
    let node = move_node(
        r#"{"status":"stopped","qmpstatus":"stopped"}"#,
        TWO_NODES,
        vec![
            (
                "GET",
                MIGRATE,
                ok_data(
                    r#"{"allowed_nodes":[],"local_disks":[{"volid":"local-lvm:vm-100-disk-0"}],
                        "local_resources":[],
                        "not_allowed_nodes":{"pve2":{"unavailable_storages":["local-lvm"]}},
                        "running":0}"#,
                ),
            ),
            ("POST", MIGRATE, ok_data(&format!("\"{MIGRATE_UPID}\""))),
            (
                "GET",
                RESOURCES,
                ok_data(r#"[{"type":"qemu","vmid":100,"node":"pve2"}]"#),
            ),
            ("GET", "/nodes/pve2/qemu/100/config", ok_data(r#"{"name":"v"}"#)),
            (
                "GET",
                CONFIG,
                Reply::Json(
                    500,
                    r#"{"data":null,"message":"Configuration file 'nodes/pve/qemu-server/100.conf' does not exist"}"#
                        .into(),
                ),
            ),
        ],
    );
    let b = backend_on(&node);
    let dir = tempfile::tempdir().unwrap();
    let vm = vm_with_handle("proxmox:pve:100");
    let opts = MoveOptions {
        with_local_disks: true,
        target_storage: Some("nfs-lab".into()),
        ..Default::default()
    };
    let handle = b
        .move_to_node(dir.path(), &vm, "pve2", &opts)
        .expect("move");
    assert_eq!(handle, "proxmox:pve2:100");
    let sent: Vec<_> = node
        .log()
        .into_iter()
        .filter(|s| s.method == "POST" && s.path == MIGRATE)
        .collect();
    assert_eq!(sent.len(), 1, "the move is sent exactly once");
    let body = &sent[0].body;
    assert!(body.contains("with-local-disks=1"), "{body}");
    assert!(body.contains("targetstorage=nfs-lab"), "{body}");

    // Without a target storage the same precheck is a refusal that says
    // how to map the disks.
    let node = move_node(
        r#"{"status":"stopped","qmpstatus":"stopped"}"#,
        TWO_NODES,
        vec![(
            "GET",
            MIGRATE,
            ok_data(
                r#"{"allowed_nodes":[],"local_disks":[{"volid":"local-lvm:vm-100-disk-0"}],
                    "local_resources":[],
                    "not_allowed_nodes":{"pve2":{"unavailable_storages":["local-lvm"]}},
                    "running":0}"#,
            ),
        )],
    );
    let b = backend_on(&node);
    let copy = MoveOptions {
        with_local_disks: true,
        ..Default::default()
    };
    let err = b
        .move_to_node(dir.path(), &vm, "pve2", &copy)
        .expect_err("an unmapped target is refused");
    assert_eq!(err.number(), 5507, "{err}");
    assert!(err.to_string().contains("--target-storage"), "{err}");
    assert_eq!(node.count("POST", MIGRATE), 0, "the move was sent");
}

/// The move is sent once, with the target and `online` asked; its UPID is
/// waited on and kept in the ledger; and it is PROVED on the node before the
/// new handle is returned — the cluster lists the VM on the target, its
/// config is on the target and gone from the source.
#[test]
fn a_move_is_sent_once_waited_on_and_proved_on_the_node() {
    use delonix_compute::vm_backend::VmBackend;
    const MIGRATE_UPID: &str = "UPID:pve:00000971:00004697:6AB793FE:qmigrate:100:root@pam:";
    let node = move_node(
        r#"{"status":"stopped","qmpstatus":"stopped"}"#,
        TWO_NODES,
        vec![
            (
                "GET",
                MIGRATE,
                ok_data(
                    r#"{"allowed_nodes":["pve2"],"local_disks":[],"local_resources":[],
                        "not_allowed_nodes":{"pve2":{}},"running":0}"#,
                ),
            ),
            ("POST", MIGRATE, ok_data(&format!("\"{MIGRATE_UPID}\""))),
            (
                "GET",
                RESOURCES,
                ok_data(r#"[{"type":"qemu","vmid":100,"node":"pve2"}]"#),
            ),
            ("GET", "/nodes/pve2/qemu/100/config", ok_data(r#"{"name":"v"}"#)),
            (
                "GET",
                CONFIG,
                Reply::Json(
                    500,
                    r#"{"data":null,"message":"Configuration file 'nodes/pve/qemu-server/100.conf' does not exist"}"#
                        .into(),
                ),
            ),
        ],
    );
    let b = backend_on(&node);
    let dir = tempfile::tempdir().unwrap();
    let vm = vm_with_handle("proxmox:pve:100");
    let handle = b
        .move_to_node(dir.path(), &vm, "pve2", &mv(false))
        .expect("move");
    assert_eq!(handle, "proxmox:pve2:100");
    let sent: Vec<_> = node
        .log()
        .into_iter()
        .filter(|s| s.method == "POST" && s.path == MIGRATE)
        .collect();
    assert_eq!(sent.len(), 1, "the move is sent exactly once");
    assert!(
        sent[0].body.contains("target=pve2") && sent[0].body.contains("online=0"),
        "{}",
        sent[0].body
    );
    assert_eq!(
        node.count("GET", RESOURCES),
        1,
        "the cluster is asked where it is"
    );
    let ledger = std::fs::read_to_string(dir.path().join("proxmox-tasks.json")).unwrap();
    assert!(
        ledger.contains(MIGRATE_UPID) && ledger.contains("\"ok\""),
        "{ledger}"
    );
    assert_eq!(b.current_handle(&vm_with_handle(&handle)), None);
}

// ===========================================================================
// Quiesced backup (vm.backup.quiesced)
// ===========================================================================

const VZDUMP: &str = "/nodes/pve/vzdump";
const AGENT_PING: &str = "/nodes/pve/qemu/100/agent/ping";
const BACKUPS: &str = "/nodes/pve/storage/local/content";

/// No agent answering: refused before the backup — the node never sees a
/// `vzdump`, and the refusal is DX-6509.
#[test]
fn a_quiesced_backup_without_an_agent_never_starts() {
    let node = MockNode::start(script(&[
        ("GET", PVE_STATUS, ok_data(r#"{"status":"running"}"#)),
        (
            "POST",
            AGENT_PING,
            Reply::Json(
                500,
                r#"{"message":"QEMU guest agent is not running\n","data":null}"#.into(),
            ),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = client
        .backup_vm_quiesced(&delonix_proxmox::Ledger::at(dir.path()), 100, "local")
        .expect_err("no agent");
    assert_eq!(delonix_model::Error::from(err).number(), 6509);
    assert_eq!(node.count("POST", VZDUMP), 0, "the backup was started");
}

/// The backup ran, but its log shows no freeze: that is NOT a quiesced
/// backup, whatever `agent=1` says — DX-6509, with the task named.
#[test]
fn a_backup_whose_log_shows_no_freeze_is_not_reported_quiesced() {
    const BACKUP_UPID: &str = "UPID:pve:00001111:00002222:6AB70000:vzdump:100:root@pam:";
    let node = MockNode::start(script(&[
        ("GET", PVE_STATUS, ok_data(r#"{"status":"running"}"#)),
        ("POST", AGENT_PING, ok_data("{}")),
        ("GET", BACKUPS, ok_data("[]")),
        ("POST", VZDUMP, ok_data(&format!("\"{BACKUP_UPID}\""))),
        (
            "GET",
            &format!("/nodes/pve/tasks/{BACKUP_UPID}/log"),
            ok_data(
                r#"[{"n":1,"t":"INFO: backup mode: snapshot"},{"n":2,"t":"INFO: Finished Backup of VM 100"}]"#,
            ),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = client
        .backup_vm_quiesced(&delonix_proxmox::Ledger::at(dir.path()), 100, "local")
        .expect_err("no freeze in the log");
    let shown = err.to_string();
    assert_eq!(delonix_model::Error::from(err).number(), 6509, "{shown}");
    assert!(
        shown.contains("no filesystem freeze") && shown.contains(BACKUP_UPID),
        "{shown}"
    );
    assert_eq!(node.count("POST", VZDUMP), 1);
}

/// A plain move: live or offline, no disk copy.
fn mv(live: bool) -> delonix_compute::vm_backend::MoveOptions {
    delonix_compute::vm_backend::MoveOptions {
        live,
        ..Default::default()
    }
}

// ===========================================================================
// The cluster's own SDN: the global lock, controllers, prefix lists, route
// maps, the vnet firewall
// ===========================================================================

const SDN_LOCK: &str = "/cluster/sdn/lock";
const SDN_APPLY: &str = "/cluster/sdn";
const SDN_ROLLBACK: &str = "/cluster/sdn/rollback";
const RELOAD_UPID: &str = "UPID:pve:0000243A:00014550:6AB8CB63:reloadnetworkall::root@pam:";

fn sdn_client(node: &MockNode) -> Client {
    Client::connect_with(&token_target(node), fast()).expect("connect")
}

/// A transaction killed while it held the lock left its token on disk. The
/// next one rolls back with that token (which also releases the lock) BEFORE
/// it asks for the lock itself — measured on PVE 9.2.2, nothing else gets the
/// lock back, and every later transaction was refused.
#[test]
fn a_transaction_that_died_holding_the_lock_is_rolled_back_by_the_next() {
    let node = MockNode::start(script(&[
        ("POST", "/cluster/sdn/rollback", ok_data("null")),
        ("POST", SDN_LOCK, ok_data(r#""tok-2""#)),
        ("DELETE", "/cluster/sdn/zones/z1", ok_data("null")),
        ("PUT", SDN_APPLY, ok_data(&format!("\"{RELOAD_UPID}\""))),
    ]));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    let held = dir.path().join("proxmox-sdn-lock.json");
    // pid 0 is never a process of ours: a dead holder.
    std::fs::write(&held, r#"{"token":"tok-dead","pid":0,"starttime":1}"#).unwrap();

    cli.sdn_transaction(&ledger, || cli.delete_sdn_zone(&ledger, "z1"))
        .expect("transaction");

    let writes: Vec<Seen> = node
        .log()
        .into_iter()
        .filter(|s| s.path.starts_with("/cluster/sdn") && s.method != "GET")
        .collect();
    assert_eq!(writes[0].path, "/cluster/sdn/rollback", "{writes:?}");
    assert!(
        writes[0].body.contains("lock-token=tok-dead") && writes[0].body.contains("release-lock=1"),
        "the dead run's token, and the lock released: {:?}",
        writes[0]
    );
    assert_eq!(writes[1].path, SDN_LOCK, "{writes:?}");
    assert!(
        !held.exists(),
        "the record is cleared when the transaction ends"
    );
}

/// A recorded holder that is still alive is another apply of this engine:
/// refused, and nothing is sent — rolling back under it would discard what it
/// is staging.
#[test]
fn a_live_holder_of_the_lock_is_never_rolled_back() {
    let node = MockNode::start(script(&[]));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    let me = std::process::id();
    let stat = std::fs::read_to_string(format!("/proc/{me}/stat")).unwrap();
    let starttime = stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_string();
    std::fs::write(
        dir.path().join("proxmox-sdn-lock.json"),
        format!(r#"{{"token":"tok-live","pid":{me},"starttime":{starttime}}}"#),
    )
    .unwrap();

    let e = cli
        .sdn_transaction(&ledger, || cli.delete_sdn_zone(&ledger, "z1"))
        .unwrap_err();
    assert!(matches!(e, Error::SdnLocked(_)), "{e:?}");
    let writes = node.log().into_iter().filter(|s| s.method != "GET").count();
    assert_eq!(writes, 0, "nothing is written under a live holder");
    assert!(
        dir.path().join("proxmox-sdn-lock.json").exists(),
        "the live holder's record is left alone"
    );
}

/// The whole transaction on the wire: the lock first, the token on every
/// staged write (the DELETE's in its query), then ONE apply that carries the
/// token AND `release-lock=1` — the node's handler does not apply the
/// schema's default, so without it the lock outlives the apply.
#[test]
fn an_sdn_transaction_carries_the_lock_token_and_applies_with_it() {
    let node = MockNode::start(script(&[
        ("POST", SDN_LOCK, ok_data(r#""tok-1""#)),
        ("POST", "/cluster/sdn/prefix-lists", ok_data("null")),
        ("DELETE", "/cluster/sdn/zones/z1", ok_data("null")),
        ("PUT", SDN_APPLY, ok_data(&format!("\"{RELOAD_UPID}\""))),
    ]));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    let entries = [delonix_proxmox::PrefixListEntry {
        action: delonix_proxmox::RoutingAction::Permit,
        prefix: "10.77.0.0/16",
        ge: None,
        le: Some(24),
        seq: None,
    }];
    cli.sdn_transaction(&ledger, || {
        cli.create_sdn_prefix_list(&ledger, "pl1", &entries)?;
        cli.delete_sdn_zone(&ledger, "z1")
    })
    .expect("transaction");

    let log: Vec<Seen> = node
        .log()
        .into_iter()
        .filter(|s| s.path.starts_with("/cluster/sdn") && s.method != "GET")
        .collect();
    let order: Vec<(&str, &str)> = log
        .iter()
        .map(|s| (s.method.as_str(), s.path.as_str()))
        .collect();
    assert_eq!(
        order,
        vec![
            ("POST", SDN_LOCK),
            ("POST", "/cluster/sdn/prefix-lists"),
            ("DELETE", "/cluster/sdn/zones/z1"),
            ("PUT", SDN_APPLY),
        ],
        "lock, the two writes, one apply — no rollback"
    );
    assert!(
        log[1].body.contains("lock-token=tok-1")
            && log[1]
                .body
                .contains("entries=action=permit,prefix=10.77.0.0/16,le=24"),
        "{}",
        log[1].body
    );
    assert!(log[2].query.contains("lock-token=tok-1"), "{:?}", log[2]);
    assert!(
        log[3].body.contains("lock-token=tok-1") && log[3].body.contains("release-lock=1"),
        "{}",
        log[3].body
    );
    let ledger_text = std::fs::read_to_string(dir.path().join("proxmox-tasks.json")).unwrap();
    assert!(
        ledger_text.contains(RELOAD_UPID) && ledger_text.contains("\"ok\""),
        "{ledger_text}"
    );
    // The token is the transaction's: a write after it carries none.
    let _ = cli.create_sdn_zone(&ledger, "z2");
    let late = node
        .log()
        .into_iter()
        .rfind(|s| s.method == "POST" && s.path == "/cluster/sdn/zones")
        .expect("the late write reached the node");
    assert!(!late.body.contains("lock-token"), "{}", late.body);
}

/// A change that fails is rolled back under the lock — with `release-lock=1`
/// — and nothing is applied: the apply would have pushed the half that did
/// get staged.
#[test]
fn a_failed_sdn_change_is_rolled_back_and_never_applied() {
    let node = MockNode::start(script(&[
        ("POST", SDN_LOCK, ok_data(r#""tok-2""#)),
        ("POST", "/cluster/sdn/prefix-lists", ok_data("null")),
        (
            "POST",
            "/cluster/sdn/controllers",
            Reply::Json(
                500,
                r#"{"data":null,"message":"create sdn controller object failed: route map rm1 does not exist!\n"}"#
                    .into(),
            ),
        ),
        ("POST", SDN_ROLLBACK, ok_data("null")),
    ]));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    let peers = ["10.0.0.2"];
    let err = cli
        .sdn_transaction(&ledger, || {
            cli.create_sdn_prefix_list(&ledger, "pl1", &[])?;
            cli.create_sdn_controller(
                &ledger,
                "ev1",
                delonix_proxmox::ControllerKind::Evpn,
                &delonix_proxmox::ControllerOptions {
                    asn: Some(65000),
                    peers: &peers,
                    route_map_in: Some("rm1"),
                    ..Default::default()
                },
            )
        })
        .unwrap_err();
    assert!(err.to_string().contains("route map rm1"), "{err}");
    assert_eq!(node.count("PUT", SDN_APPLY), 0, "nothing is applied");
    let rb: Vec<Seen> = node
        .log()
        .into_iter()
        .filter(|s| s.path == SDN_ROLLBACK)
        .collect();
    assert_eq!(rb.len(), 1, "one rollback");
    assert!(
        rb[0].body.contains("lock-token=tok-2") && rb[0].body.contains("release-lock=1"),
        "{}",
        rb[0].body
    );
}

/// Someone else's staged changes are waiting: the lock is refused with its own
/// class (DX-5516), and the change never runs.
#[test]
fn the_lock_refused_for_pending_changes_runs_nothing() {
    let node = MockNode::start(script(&[(
        "POST",
        SDN_LOCK,
        Reply::Json(
            500,
            r#"{"data":null,"message":"could not acquire lock for SDN config: configuration has pending changes\n"}"#
                .into(),
        ),
    )]));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    let mut ran = false;
    let err = cli
        .sdn_transaction(&ledger, || {
            ran = true;
            Ok(())
        })
        .unwrap_err();
    assert!(!ran, "the change must not run without the lock");
    assert_eq!(err.number(), 5516, "{err}");
    assert!(err.is_conflict(), "{err}");
    let sdn: Vec<Seen> = node
        .log()
        .into_iter()
        .filter(|s| s.path.starts_with("/cluster/sdn"))
        .collect();
    assert_eq!(sdn.len(), 1, "only the lock request: {sdn:?}");
}

const THREE_NODES: &str = r#"[{"node":"pve","status":"online"},{"node":"pve2","status":"online"},{"node":"pve3","status":"offline"}]"#;

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// A node's `srvreload`/`networking` task, as its task list shows it.
fn reload_task(node: &str, tag: &str, start: u64) -> String {
    format!(
        r#"{{"upid":"UPID:{node}:0000{tag}:00000BBB:6AB9{tag}:srvreload:networking:root@pam:","type":"srvreload","id":"networking","starttime":{start},"status":"OK"}}"#
    )
}

fn reload_upid(node: &str, tag: &str) -> String {
    format!("UPID:{node}:0000{tag}:00000BBB:6AB9{tag}:srvreload:networking:root@pam:")
}

/// One online node's network reload as an SDN apply sees it: the task list
/// read BEFORE the apply (an older reload only) and AFTER it (a fresh one,
/// `1111`, on top). The fresh reload's status is the mock's stock `OK`.
fn reload_lists(node: &str) -> [(&'static str, &'static str, Reply); 2] {
    let path = leak(format!("/nodes/{node}/tasks"));
    let old = reload_task(node, "0AAA", 100);
    let fresh = reload_task(node, "1111", 200);
    [
        ("GET", path, ok_data(&format!("[{old}]"))),
        ("GET", path, ok_data(&format!("[{fresh},{old}]"))),
    ]
}

/// The apply's task ends OK when only the ENTRY node's reload succeeded —
/// measured on a two-node cluster, 2026-09-27: `reloadnetworkall` OK, the
/// second node's `srvreload` failed on a missing `dnsmasq`, its vnet `error`.
/// So the apply reads every online node's zone content and refuses with
/// DX-6512 naming node, zone and vnet; an offline node is not asked.
#[test]
fn an_sdn_apply_the_second_node_did_not_realize_is_refused() {
    let content_ok = r#"{"data":[{"vnet":"v1","status":"available","statusmsg":null}]}"#;
    let content_err = r#"{"data":[{"vnet":"v1","status":"error","statusmsg":"vnet is not generated. Please check the 'reload network' task log."}]}"#;
    let node = MockNode::start(script(
        &[
            ("PUT", SDN_APPLY, ok_data(&format!("\"{RELOAD_UPID}\""))),
            (
                "GET",
                "/cluster/sdn/zones",
                ok_data(r#"[{"zone":"z1","type":"simple"}]"#),
            ),
            // Three times: `connect`, the apply's reload follow-up and the
            // realization check each read the node list.
            ("GET", "/nodes", ok_data(THREE_NODES)),
            ("GET", "/nodes", ok_data(THREE_NODES)),
            ("GET", "/nodes", ok_data(THREE_NODES)),
            (
                "GET",
                "/nodes/pve/sdn/zones/z1/content",
                Reply::Json(200, content_ok.into()),
            ),
            (
                "GET",
                "/nodes/pve2/sdn/zones/z1/content",
                Reply::Json(200, content_err.into()),
            ),
        ]
        .into_iter()
        .chain(reload_lists("pve"))
        .chain(reload_lists("pve2"))
        .collect::<Vec<_>>(),
    ));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    let err = cli.apply_sdn(&Ledger::at(dir.path())).unwrap_err();
    let shown = err.to_string();
    assert_eq!(delonix_model::Error::from(err).number(), 6512, "{shown}");
    assert!(shown.contains("pve2/z1/v1: error"), "{shown}");
    assert!(
        !shown.contains("pve/z1"),
        "the realized node is not named: {shown}"
    );
    let asked: Vec<String> = node
        .log()
        .into_iter()
        .filter(|s| s.path.ends_with("/content"))
        .map(|s| s.path)
        .collect();
    assert_eq!(
        asked,
        vec![
            "/nodes/pve/sdn/zones/z1/content",
            "/nodes/pve2/sdn/zones/z1/content"
        ],
        "every online node, and not the offline one"
    );
}

/// The same apply with every online node realizing the vnet is plain success.
#[test]
fn an_sdn_apply_every_online_node_realized_succeeds() {
    let content_ok = r#"{"data":[{"vnet":"v1","status":"available","statusmsg":null}]}"#;
    let node = MockNode::start(script(
        &[
            ("PUT", SDN_APPLY, ok_data(&format!("\"{RELOAD_UPID}\""))),
            (
                "GET",
                "/cluster/sdn/zones",
                ok_data(r#"[{"zone":"z1","type":"simple"}]"#),
            ),
            ("GET", "/nodes", ok_data(TWO_NODES)),
            ("GET", "/nodes", ok_data(TWO_NODES)),
            ("GET", "/nodes", ok_data(TWO_NODES)),
            (
                "GET",
                "/nodes/pve/sdn/zones/z1/content",
                Reply::Json(200, content_ok.into()),
            ),
            (
                "GET",
                "/nodes/pve2/sdn/zones/z1/content",
                Reply::Json(200, content_ok.into()),
            ),
        ]
        .into_iter()
        .chain(reload_lists("pve"))
        .chain(reload_lists("pve2"))
        .collect::<Vec<_>>(),
    ));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    cli.apply_sdn(&Ledger::at(dir.path()))
        .expect("both nodes realized it");
    let asked = node
        .log()
        .into_iter()
        .filter(|s| s.path.ends_with("/content"))
        .count();
    assert_eq!(asked, 2, "both online nodes were read");
}

/// The apply's own task only STARTS each node's `srvreload networking`
/// (PVE's `SDN.pm`, with an upstream FIXME saying so): the apply finds each
/// online node's fresh reload by what its task list did not show before, and
/// waits for it. A reload that ends `WARNINGS` succeeds; one that appears late
/// is waited for; an older reload is never taken for this apply's.
#[test]
fn an_sdn_apply_follows_each_nodes_reload_and_takes_only_the_fresh_one() {
    let pve2_path = "/nodes/pve2/tasks";
    let old2 = reload_task("pve2", "0AAA", 100);
    let fresh2 = reload_task("pve2", "1111", 200);
    let mut entries: Vec<(&str, &str, Reply)> = vec![
        ("PUT", SDN_APPLY, ok_data(&format!("\"{RELOAD_UPID}\""))),
        ("GET", "/nodes", ok_data(TWO_NODES)),
        ("GET", "/nodes", ok_data(TWO_NODES)),
        // pve2: before, then twice without the fresh reload, then with it.
        ("GET", pve2_path, ok_data(&format!("[{old2}]"))),
        ("GET", pve2_path, ok_data(&format!("[{old2}]"))),
        ("GET", pve2_path, ok_data(&format!("[{old2}]"))),
        ("GET", pve2_path, ok_data(&format!("[{fresh2},{old2}]"))),
        (
            "GET",
            leak(format!(
                "/nodes/pve/tasks/{}/status",
                reload_upid("pve", "1111")
            )),
            ok_data(r#"{"status":"stopped","exitstatus":"WARNINGS: 1"}"#),
        ),
        (
            "GET",
            leak(format!(
                "/nodes/pve/tasks/{}/log",
                reload_upid("pve", "1111")
            )),
            ok_data(
                r#"[{"n":1,"t":"WARN: missing 'source /etc/network/interfaces.d/sdn' directive for SDN support!"},{"n":2,"t":"TASK WARNINGS: 1"}]"#,
            ),
        ),
    ];
    entries.extend(reload_lists("pve"));
    let node = MockNode::start(script(&entries));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    cli.apply_sdn(&Ledger::at(dir.path()))
        .expect("a reload with warnings, and one that came late, are both a success");

    let statuses: Vec<String> = node
        .log()
        .into_iter()
        .filter(|s| s.path.contains("srvreload") && s.path.ends_with("/status"))
        .map(|s| s.path)
        .collect();
    assert!(
        statuses
            .iter()
            .any(|p| p.contains(&reload_upid("pve", "1111")))
            && statuses
                .iter()
                .any(|p| p.contains(&reload_upid("pve2", "1111"))),
        "each node's fresh reload was waited on: {statuses:?}"
    );
    assert!(
        !statuses.iter().any(|p| p.contains("0AAA")),
        "an older reload is never taken for this apply's: {statuses:?}"
    );
    let lists: Vec<Seen> = node
        .log()
        .into_iter()
        .filter(|s| s.path == pve2_path)
        .collect();
    assert_eq!(lists.len(), 4, "pve2 polled until its reload appeared");
    assert!(
        lists[0].query.contains("typefilter=srvreload") && lists[0].query.contains("source=all"),
        "{:?}",
        lists[0]
    );
}

/// The parent ended OK and a node's reload failed: the apply fails with
/// DX-6512, naming that node and its reason.
#[test]
fn an_sdn_apply_whose_node_reload_failed_is_refused_despite_the_parent_ok() {
    let mut entries: Vec<(&str, &str, Reply)> = vec![
        ("PUT", SDN_APPLY, ok_data(&format!("\"{RELOAD_UPID}\""))),
        ("GET", "/nodes", ok_data(TWO_NODES)),
        ("GET", "/nodes", ok_data(TWO_NODES)),
        (
            "GET",
            leak(format!(
                "/nodes/pve2/tasks/{}/status",
                reload_upid("pve2", "1111")
            )),
            ok_data(
                r#"{"status":"stopped","exitstatus":"command 'ifreload -a' failed: exit code 1"}"#,
            ),
        ),
    ];
    entries.extend(reload_lists("pve"));
    entries.extend(reload_lists("pve2"));
    let node = MockNode::start(script(&entries));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    let err = cli.apply_sdn(&Ledger::at(dir.path())).unwrap_err();
    let shown = err.to_string();
    assert_eq!(delonix_model::Error::from(err).number(), 6512, "{shown}");
    assert!(
        shown.contains("pve2:") && shown.contains("ifreload -a"),
        "{shown}"
    );
    assert!(
        !shown.contains("pve: "),
        "the node that reloaded is not named: {shown}"
    );
}

/// A node that never starts a reload within the task timeout fails the apply
/// by name — never read as "nothing to do".
#[test]
fn an_sdn_apply_with_no_reload_on_a_node_is_refused() {
    let old2 = reload_task("pve2", "0AAA", 100);
    let mut entries: Vec<(&str, &str, Reply)> = vec![
        ("PUT", SDN_APPLY, ok_data(&format!("\"{RELOAD_UPID}\""))),
        ("GET", "/nodes", ok_data(TWO_NODES)),
        ("GET", "/nodes", ok_data(TWO_NODES)),
    ];
    for _ in 0..30 {
        entries.push(("GET", "/nodes/pve2/tasks", ok_data(&format!("[{old2}]"))));
    }
    entries.extend(reload_lists("pve"));
    let node = MockNode::start(script(&entries));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    let err = cli.apply_sdn(&Ledger::at(dir.path())).unwrap_err();
    let shown = err.to_string();
    assert_eq!(delonix_model::Error::from(err).number(), 6512, "{shown}");
    assert!(
        shown.contains("pve2: no network reload appeared"),
        "{shown}"
    );
}

/// A staged write while another holder has the lock is DX-5515, not a generic
/// HTTP error.
#[test]
fn a_write_under_someone_elses_lock_is_sdn_locked() {
    let node = MockNode::start(script(&[(
        "POST",
        "/cluster/sdn/zones",
        Reply::Json(
            500,
            r#"{"data":null,"message":"create sdn zone object failed: invalid lock token provided! at /usr/share/perl5/PVE/Network/SDN.pm line 305.\n"}"#
                .into(),
        ),
    )]));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    let err = cli
        .create_sdn_zone(&Ledger::at(dir.path()), "z1")
        .unwrap_err();
    assert_eq!(err.number(), 5515, "{err}");
    assert!(
        err.to_string().contains("locked by another holder"),
        "{err}"
    );
}

/// The change failed and the rollback failed too: both are named, with the
/// token that releases what is left.
#[test]
fn a_failed_rollback_names_both_failures_and_the_token() {
    let node = MockNode::start(script(&[
        ("POST", SDN_LOCK, ok_data(r#""tok-3""#)),
        (
            "DELETE",
            "/cluster/sdn/zones/z1",
            Reply::Json(400, r#"{"data":null,"message":"bad"}"#.into()),
        ),
        (
            "POST",
            SDN_ROLLBACK,
            Reply::Json(503, r#"{"data":null,"message":"pmxcfs busy"}"#.into()),
        ),
    ]));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    let err = cli
        .sdn_transaction(&ledger, || cli.delete_sdn_zone(&ledger, "z1"))
        .unwrap_err();
    assert_eq!(err.number(), 9525, "{err}");
    let text = err.to_string();
    assert!(
        text.contains("tok-3") && text.contains("bad") && text.contains("pmxcfs busy"),
        "{text}"
    );
}

/// The three writes to a vnet's own firewall — options, a new rule, a
/// changed rule — are refused unconditionally (DX-1552, `sdn_routing.rs`
/// module doc): a well-formed request and a malformed one get the same
/// answer, and NOTHING reaches the node, not even a login or a lock. Reads
/// stay: a staged vnet ("invalid vnet specified") is a not-found that says
/// to apply first.
#[test]
fn a_vnet_firewall_write_is_refused_before_anything_reaches_the_node() {
    let node = MockNode::start(script(&[(
        "GET",
        "/cluster/sdn/vnets/v2/firewall/rules",
        Reply::Json(
            500,
            r#"{"data":null,"message":"invalid vnet specified at /usr/share/perl5/PVE/API2/Firewall/Helpers.pm line 54.\n"}"#
                .into(),
        ),
    )]));
    let cli = sdn_client(&node);
    // Connecting proves the node (`GET /nodes`); everything after it counts.
    let connected = node.log().len();
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());

    let ssh = delonix_proxmox::FirewallRuleOpts {
        rule_type: Some("forward"),
        proto: Some("tcp"),
        dport: Some("22"),
        ..Default::default()
    };
    // Malformed on purpose: an `in` rule, a vnet id the node could not hold.
    // The refusal comes first, so it is the same answer as the valid one.
    let inbound = delonix_proxmox::FirewallRuleOpts {
        rule_type: Some("in"),
        ..Default::default()
    };
    let opts = delonix_proxmox::VnetFirewallOptions {
        enable: Some(true),
        policy_forward: Some("DROP"),
        ..Default::default()
    };
    let refused = [
        cli.set_sdn_vnet_firewall_options(&ledger, "v1", &opts),
        cli.add_sdn_vnet_firewall_rule(&ledger, "v1", "ACCEPT", &ssh),
        cli.add_sdn_vnet_firewall_rule(&ledger, "Not A Vnet", "+group", &inbound),
        cli.update_sdn_vnet_firewall_rule(&ledger, "v1", 0, &ssh, None),
        cli.update_sdn_vnet_firewall_rule(&ledger, "v1", 0, &ssh, Some(2)),
    ];
    for r in refused {
        let e = r.unwrap_err();
        assert_eq!(e.number(), 1552, "{e}");
        assert!(e.to_string().contains("ADR-0049 D3"), "{e}");
    }
    let sent: Vec<String> = node.log()[connected..]
        .iter()
        .map(|s| format!("{} {}", s.method, s.path))
        .collect();
    assert!(
        sent.is_empty(),
        "a refused vnet firewall write sent {sent:?}"
    );
    assert!(
        std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "a refused write left a ledger entry"
    );

    let staged = cli.sdn_vnet_firewall_rules("v2").unwrap_err();
    assert_eq!(staged.number(), 4504, "{staged}");
    assert!(
        staged.to_string().contains("running SDN configuration"),
        "{staged}"
    );
}

/// A route-map entry goes to its own path (`{route-map-id}` travels as the
/// id), its clauses as `key=…,value=…` property strings, and the dry-run's
/// `null` diffs read as "nothing would change".
#[test]
fn a_route_map_entry_and_a_dry_run_read_the_nodes_shapes() {
    let entry_path = "/cluster/sdn/route-maps/entries/rm1/entry/10";
    let node = MockNode::start(script(&[
        ("POST", "/cluster/sdn/route-maps/entries", ok_data("null")),
        (
            "GET",
            entry_path,
            ok_data(r#"{"route-map-id":"rm1","order":10,"action":"permit"}"#),
        ),
        (
            "GET",
            "/cluster/sdn/dry-run",
            ok_data(r#"{"interfaces-diff":null,"frr-diff":null}"#),
        ),
    ]));
    let cli = sdn_client(&node);
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path());
    let matches = [delonix_proxmox::RouteMapClause {
        key: "ip-address-prefix-list",
        value: Some("pl1"),
    }];
    let sets = [delonix_proxmox::RouteMapClause {
        key: "local-preference",
        value: Some("200"),
    }];
    cli.create_sdn_route_map_entry(
        &ledger,
        "rm1",
        10,
        &delonix_proxmox::RouteMapEntry {
            action: delonix_proxmox::RoutingAction::Permit,
            matches: &matches,
            sets: &sets,
            call: None,
            exit_action: None,
        },
    )
    .expect("entry");
    let sent = node
        .log()
        .into_iter()
        .find(|s| s.path == "/cluster/sdn/route-maps/entries")
        .unwrap();
    assert!(
        sent.body.contains("route-map-id=rm1")
            && sent.body.contains("order=10")
            && sent
                .body
                .contains("match=key=ip-address-prefix-list,value=pl1")
            && sent.body.contains("set=key=local-preference,value=200"),
        "{}",
        sent.body
    );
    let got = cli.sdn_route_map_entry("rm1", 10).expect("read back");
    assert_eq!(got.get("action").and_then(|v| v.as_str()), Some("permit"));
    let dry = cli.sdn_dry_run(None).expect("dry run");
    assert!(dry.is_empty(), "{dry:?}");
    let asked = node
        .log()
        .into_iter()
        .find(|s| s.path == "/cluster/sdn/dry-run")
        .unwrap();
    assert_eq!(asked.query, "node=pve");
    let e = cli
        .create_sdn_route_map_entry(
            &ledger,
            "pve_x",
            1,
            &delonix_proxmox::RouteMapEntry {
                action: delonix_proxmox::RoutingAction::Deny,
                matches: &[],
                sets: &[],
                call: None,
                exit_action: None,
            },
        )
        .unwrap_err();
    assert_eq!(e.number(), 1550, "a reserved id is refused: {e}");
}

// ===========================================================================
// ADR-0057: a VM from a local image, uploaded and imported by the node
// ===========================================================================

const LOCAL_STATUS: &str = "/nodes/pve/storage/local/status";
const LOCAL_CONTENT: &str = "/nodes/pve/storage/local/content";
const LOCAL_UPLOAD: &str = "/nodes/pve/storage/local/upload";
const NEXTID: &str = "/cluster/nextid";
const CREATE: &str = "/nodes/pve/qemu";

/// A small qcow2: a real header (magic, version 3, virtual size `gib` GiB at
/// bytes 24..32) and a payload, which is all the client reads.
fn tiny_qcow2(dir: &std::path::Path, gib: u64) -> std::path::PathBuf {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"QFI\xfb");
    bytes.extend_from_slice(&3u32.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    bytes.extend_from_slice(&(gib * 1024 * 1024 * 1024).to_be_bytes());
    bytes.extend_from_slice(b"delonix test image payload");
    let path = dir.join("image.qcow2");
    std::fs::write(&path, &bytes).unwrap();
    path
}

fn sha256_hex(path: &std::path::Path) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(std::fs::read(path).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn image_cfg(disk: &std::path::Path, size: Option<u32>) -> VmConfig {
    VmConfig {
        name: "img".into(),
        disk: disk.to_string_lossy().into_owned(),
        vcpus: 1,
        memory: "512M".into(),
        disk_size_gib: size,
        ..Default::default()
    }
}

fn status_reply(content: &str, avail: u64) -> Reply {
    ok_data(&format!(
        r#"{{"active":1,"avail":{avail},"content":"{content}","enabled":1,"type":"dir"}}"#
    ))
}

/// The import storage does not list `import`: refused (DX-6510) naming the
/// storage and the command, before any upload and before a vmid is asked for.
#[test]
fn an_image_on_a_storage_without_import_is_refused_before_anything_is_uploaded() {
    use delonix_compute::vm_backend::{CreateStage, VmBackend};
    let node = MockNode::start(script(&[(
        "GET",
        LOCAL_STATUS,
        status_reply("images,iso,backup", 1 << 40),
    )]));
    let dir = tempfile::tempdir().unwrap();
    let img = tiny_qcow2(dir.path(), 2);
    let b = backend_on(&node);
    let cfg = image_cfg(&img, None);
    let Err(err) = b.boot(dir.path(), &cfg, &cfg.disk, &|_: CreateStage| {}) else {
        panic!("a storage without `import` cannot take the image");
    };
    assert_eq!(err.number(), 6510, "{err}");
    let shown = err.to_string();
    assert!(
        shown.contains("'local'") && shown.contains("pvesm set local"),
        "{shown}"
    );
    assert_eq!(
        node.count("POST", LOCAL_UPLOAD),
        0,
        "the image was uploaded"
    );
    assert_eq!(node.count("GET", NEXTID), 0, "a vmid was asked for");
}

/// The node does not have the image: it is uploaded ONCE, named by its
/// content, with its sha256 for the node to verify, the text fields before
/// the file; then the VM is created importing it onto the disk storage.
#[test]
fn an_image_is_uploaded_with_its_checksum_and_imported_onto_the_disk_storage() {
    use delonix_compute::vm_backend::{CreateStage, VmBackend};
    const UPLOAD_UPID: &str = "UPID:pve:00000100:00000200:6AB90000:imgcopy::root@pam:";
    const CREATE_UPID: &str = "UPID:pve:00000101:00000201:6AB90001:qmcreate:100:root@pam:";
    const START_UPID: &str = "UPID:pve:00000102:00000202:6AB90002:qmstart:100:root@pam:";
    let node = MockNode::start(script(&[
        ("GET", LOCAL_STATUS, status_reply("images,import", 1 << 40)),
        ("GET", LOCAL_CONTENT, ok_data("[]")),
        ("POST", LOCAL_UPLOAD, ok_data(&format!("\"{UPLOAD_UPID}\""))),
        ("GET", NEXTID, ok_data("\"100\"")),
        ("POST", CREATE, ok_data(&format!("\"{CREATE_UPID}\""))),
        (
            "POST",
            "/nodes/pve/qemu/100/status/start",
            ok_data(&format!("\"{START_UPID}\"")),
        ),
    ]));
    let dir = tempfile::tempdir().unwrap();
    let img = tiny_qcow2(dir.path(), 2);
    let sha = sha256_hex(&img);
    let b = backend_on(&node);
    let cfg = image_cfg(&img, None);
    let boot = b
        .boot(dir.path(), &cfg, &cfg.disk, &|_: CreateStage| {})
        .expect("boot from a local image");
    assert_eq!(boot.api_socket, "proxmox:pve:100");

    let uploads: Vec<_> = node
        .log()
        .into_iter()
        .filter(|s| s.method == "POST" && s.path == LOCAL_UPLOAD)
        .collect();
    assert_eq!(uploads.len(), 1, "the image is uploaded exactly once");
    let body = &uploads[0].body;
    let volume = format!("delonix-{}.qcow2", &sha[..16]);
    for want in [
        "name=\"content\"\r\n\r\nimport\r\n".to_string(),
        format!("name=\"checksum\"\r\n\r\n{sha}\r\n"),
        "name=\"checksum-algorithm\"\r\n\r\nsha256\r\n".to_string(),
        format!("filename=\"{volume}\""),
        "delonix test image payload".to_string(),
    ] {
        assert!(body.contains(&want), "upload body lacks {want:?}: {body:?}");
    }
    assert!(
        body.find("name=\"content\"").unwrap() < body.find("filename=").unwrap(),
        "the fields must come before the file: {body:?}"
    );

    let create = node
        .log()
        .into_iter()
        .find(|s| s.method == "POST" && s.path == CREATE)
        .expect("the create");
    assert!(
        create.body.contains(&format!(
            "scsi0=local-lvm:0,import-from=local:import/{volume}"
        )),
        "{}",
        create.body
    );
}

/// The node already has the image (same content, same name): nothing is
/// uploaded, and the create imports the volume that is there.
#[test]
fn an_image_the_node_already_has_is_not_uploaded_again() {
    use delonix_compute::vm_backend::{CreateStage, VmBackend};
    let dir = tempfile::tempdir().unwrap();
    let img = tiny_qcow2(dir.path(), 2);
    let sha = sha256_hex(&img);
    let volid = format!("local:import/delonix-{}.qcow2", &sha[..16]);
    let node = MockNode::start(script(&[
        ("GET", LOCAL_STATUS, status_reply("images,import", 1 << 40)),
        (
            "GET",
            LOCAL_CONTENT,
            ok_data(&format!(r#"[{{"volid":"{volid}","content":"import"}}]"#)),
        ),
        ("GET", NEXTID, ok_data("\"100\"")),
        (
            "POST",
            CREATE,
            ok_data("\"UPID:pve:1:2:3:qmcreate:100:root@pam:\""),
        ),
        (
            "POST",
            "/nodes/pve/qemu/100/status/start",
            ok_data("\"UPID:pve:1:2:4:qmstart:100:root@pam:\""),
        ),
    ]));
    let b = backend_on(&node);
    let cfg = image_cfg(&img, None);
    b.boot(dir.path(), &cfg, &cfg.disk, &|_: CreateStage| {})
        .expect("boot from a cached image");
    assert_eq!(
        node.count("POST", LOCAL_UPLOAD),
        0,
        "the image was uploaded again"
    );
    let create = node
        .log()
        .into_iter()
        .find(|s| s.method == "POST" && s.path == CREATE)
        .unwrap();
    assert!(
        create.body.contains(&format!("import-from={volid}")),
        "{}",
        create.body
    );
}

/// Less free space than the image (DX-6511), and a `diskSize` smaller than
/// the image's virtual size (DX-1522): both refused before anything is sent —
/// the second before the storage is even asked about.
#[test]
fn an_image_without_room_or_asked_to_shrink_is_refused_before_the_upload() {
    use delonix_compute::vm_backend::{CreateStage, VmBackend};
    let dir = tempfile::tempdir().unwrap();
    let img = tiny_qcow2(dir.path(), 2);

    let node = MockNode::start(script(&[
        ("GET", LOCAL_STATUS, status_reply("images,import", 10)),
        ("GET", LOCAL_CONTENT, ok_data("[]")),
    ]));
    let b = backend_on(&node);
    let cfg = image_cfg(&img, None);
    let Err(err) = b.boot(dir.path(), &cfg, &cfg.disk, &|_: CreateStage| {}) else {
        panic!("no room for the image");
    };
    assert_eq!(err.number(), 6511, "{err}");
    assert_eq!(
        node.count("POST", LOCAL_UPLOAD),
        0,
        "the image was uploaded"
    );
    assert_eq!(node.count("GET", NEXTID), 0, "a vmid was asked for");

    let node = MockNode::start(script(&[]));
    let b = backend_on(&node);
    let before = node.log().len();
    let cfg = image_cfg(&img, Some(1));
    let Err(err) = b.boot(dir.path(), &cfg, &cfg.disk, &|_: CreateStage| {}) else {
        panic!("a diskSize smaller than the image cannot shrink it");
    };
    assert_eq!(err.number(), 1522, "{err}");
    assert!(err.to_string().contains("never shrink"), "{err}");
    assert_eq!(node.log().len(), before, "the refusal reached the node");
}

// ===========================================================================
// ADR-0058 / plan 63 slice 2: a container archive staged as `vztmpl`
// ===========================================================================

const ARCHIVE_DIGEST: &str =
    "sha256:a14b44155e4b8cefde8433bcf0f6fb5f460169571f5bff684d06572a98759035";
const ARCHIVE_VOLID: &str =
    "local:vztmpl/dlx-a14b44155e4b8cefde8433bcf0f6fb5f460169571f5bff684d06572a98759035.tar";

fn tiny_archive(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("image.tar");
    std::fs::write(&path, b"delonix test archive payload").unwrap();
    path
}

fn uploads_of(node: &MockNode) -> Vec<String> {
    node.log()
        .into_iter()
        .filter(|s| s.method == "POST" && s.path == LOCAL_UPLOAD)
        .map(|s| s.body)
        .collect()
}

/// A storage without `vztmpl`, or without room: refused before a byte is sent.
#[test]
fn an_archive_is_refused_before_the_upload_on_a_storage_without_vztmpl_or_room() {
    let dir = tempfile::tempdir().unwrap();
    let archive = tiny_archive(dir.path());

    let node = MockNode::start(script(&[(
        "GET",
        LOCAL_STATUS,
        status_reply("images,import", 1 << 40),
    )]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let err = client
        .stage_template("local", &archive, ARCHIVE_DIGEST)
        .unwrap_err();
    assert_eq!(err.number(), 6513, "{err}");
    assert!(err.to_string().contains("pvesm set local"), "{err}");
    assert!(uploads_of(&node).is_empty(), "the archive was uploaded");

    let node = MockNode::start(script(&[
        ("GET", LOCAL_STATUS, status_reply("vztmpl", 4)),
        ("GET", LOCAL_CONTENT, ok_data("[]")),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let err = client
        .stage_template("local", &archive, ARCHIVE_DIGEST)
        .unwrap_err();
    assert_eq!(err.number(), 6514, "{err}");
    assert!(uploads_of(&node).is_empty(), "the archive was uploaded");
}

/// The node already has the archive under its digest name: not sent again.
#[test]
fn an_archive_the_node_already_has_is_not_uploaded_again() {
    let dir = tempfile::tempdir().unwrap();
    let archive = tiny_archive(dir.path());
    let node = MockNode::start(script(&[
        ("GET", LOCAL_STATUS, status_reply("vztmpl,iso", 1 << 40)),
        (
            "GET",
            LOCAL_CONTENT,
            ok_data(&format!(
                r#"[{{"volid":"{ARCHIVE_VOLID}","content":"vztmpl"}}]"#
            )),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let staged = client
        .stage_template("local", &archive, ARCHIVE_DIGEST)
        .expect("stage");
    assert_eq!(staged.volid, ARCHIVE_VOLID);
    assert!(!staged.uploaded);
    assert!(
        uploads_of(&node).is_empty(),
        "the archive was uploaded again"
    );
    let listed = node
        .log()
        .into_iter()
        .find(|s| s.path == LOCAL_CONTENT)
        .expect("the listing");
    assert!(listed.query.contains("content=vztmpl"), "{}", listed.query);
}

/// Uploaded once as `vztmpl`, named by the manifest digest, with the FILE's
/// sha256; a node that answers «checksum mismatch» fails the stage.
#[test]
fn an_archive_is_uploaded_as_vztmpl_with_its_checksum_and_a_mismatch_fails() {
    const UPID: &str = "UPID:pve:00000300:00000400:6AB90010:imgcopy::root@pam:";
    let dir = tempfile::tempdir().unwrap();
    let archive = tiny_archive(dir.path());
    let sha = sha256_hex(&archive);
    let status = format!("/nodes/pve/tasks/{UPID}/status");

    let node = MockNode::start(script(&[
        ("GET", LOCAL_STATUS, status_reply("vztmpl", 1 << 40)),
        ("GET", LOCAL_CONTENT, ok_data("[]")),
        ("POST", LOCAL_UPLOAD, ok_data(&format!("\"{UPID}\""))),
        (
            "GET",
            leak(status.clone()),
            ok_data(r#"{"status":"stopped","exitstatus":"OK"}"#),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let staged = client
        .stage_template("local", &archive, ARCHIVE_DIGEST)
        .expect("stage");
    assert!(staged.uploaded);
    assert_eq!(staged.volid, ARCHIVE_VOLID);
    let bodies = uploads_of(&node);
    assert_eq!(bodies.len(), 1, "uploaded exactly once");
    for want in [
        "name=\"content\"\r\n\r\nvztmpl\r\n".to_string(),
        format!("name=\"checksum\"\r\n\r\n{sha}\r\n"),
        format!(
            "filename=\"{}\"",
            ARCHIVE_VOLID.trim_start_matches("local:vztmpl/")
        ),
        "delonix test archive payload".to_string(),
    ] {
        assert!(bodies[0].contains(&want), "upload body lacks {want:?}");
    }

    let mismatch = format!(
        "checksum mismatch: got '{sha}' != expect '{}'",
        "0".repeat(64)
    );
    let node = MockNode::start(script(&[
        ("GET", LOCAL_STATUS, status_reply("vztmpl", 1 << 40)),
        ("GET", LOCAL_CONTENT, ok_data("[]")),
        ("POST", LOCAL_UPLOAD, ok_data(&format!("\"{UPID}\""))),
        (
            "GET",
            leak(status),
            ok_data(&format!(
                r#"{{"status":"stopped","exitstatus":"{mismatch}"}}"#
            )),
        ),
    ]));
    let client = Client::connect_with(&token_target(&node), fast()).unwrap();
    let err = client
        .stage_template("local", &archive, ARCHIVE_DIGEST)
        .unwrap_err();
    assert!(err.to_string().contains("checksum mismatch"), "{err}");
}

// ===========================================================================
// ADR-0058 / plan 63 slice 3: a system container
// ===========================================================================

const CT_VOLID: &str =
    "local:vztmpl/dlx-a14b44155e4b8cefde8433bcf0f6fb5f460169571f5bff684d06572a98759035.tar";
const CT_CREATE: &str = "/nodes/pve/lxc";
const CT_CONFIG: &str = "/nodes/pve/lxc/100/config";

fn ct_spec(dir: &std::path::Path) -> delonix_compute::system_container::SystemContainerSpec {
    delonix_compute::system_container::SystemContainerSpec {
        name: "dlx-ct".into(),
        archive: dir.join("image.tar"),
        manifest_digest: ARCHIVE_DIGEST.into(),
        entrypoint: vec!["/bin/sleep".into(), "3600".into()],
        env: vec![("DLX_TEST".into(), "one".into())],
        memory_mib: 256,
        swap_mib: 0,
        cores: 1,
        rootfs_gib: 1,
        network: Some(delonix_compute::system_container::SystemContainerNet {
            bridge: "vmbr0".into(),
            vlan: None,
            dhcp: true,
        }),
        unprivileged: true,
    }
}

fn ct_provider(node: &MockNode) -> delonix_proxmox::ProxmoxSystemContainerProvider {
    let client = Client::connect_with(&token_target(node), fast()).unwrap();
    delonix_proxmox::ProxmoxSystemContainerProvider::new(Arc::new(client), "local", "local-lvm")
}

fn ct_cached_template() -> Vec<(&'static str, &'static str, Reply)> {
    vec![
        ("GET", LOCAL_STATUS, status_reply("vztmpl", 1 << 40)),
        (
            "GET",
            LOCAL_CONTENT,
            ok_data(&format!(r#"[{{"volid":"{CT_VOLID}","content":"vztmpl"}}]"#)),
        ),
        ("GET", NEXTID, ok_data("\"100\"")),
        (
            "POST",
            CT_CREATE,
            ok_data(r#""UPID:pve:00000500:00000600:6AB90020:vzcreate:100:root@pam:""#),
        ),
    ]
}

/// T2: the node keeps the image's entrypoint after the `PUT …/config`. The
/// create fails naming the field, and the container is destroyed — nothing is
/// left running with a configuration nobody asked for.
#[test]
fn a_system_container_whose_config_reads_back_different_is_destroyed() {
    use delonix_compute::system_container::SystemContainerProvider;
    let mut steps = ct_cached_template();
    steps.push(("PUT", CT_CONFIG, ok_data("null")));
    steps.push((
        "GET",
        CT_CONFIG,
        ok_data(r#"{"entrypoint":"/bin/sh","env":"DLX_TEST=one","unprivileged":1}"#),
    ));
    steps.push((
        "GET",
        "/nodes/pve/lxc/100/status/current",
        ok_data(r#"{"status":"stopped"}"#),
    ));
    steps.push((
        "DELETE",
        "/nodes/pve/lxc/100",
        ok_data(r#""UPID:pve:00000501:00000601:6AB90021:vzdestroy:100:root@pam:""#),
    ));
    let node = MockNode::start(script(&steps));
    let dir = tempfile::tempdir().unwrap();
    let p = ct_provider(&node);
    let err = p.create(dir.path(), &ct_spec(dir.path())).unwrap_err();
    let shown = err.to_string();
    assert!(
        shown.contains("entrypoint") && shown.contains("/bin/sh"),
        "{shown}"
    );
    assert!(shown.contains("destroyed"), "{shown}");

    let create = node
        .log()
        .into_iter()
        .find(|s| s.method == "POST" && s.path == CT_CREATE)
        .expect("the create");
    assert!(create.body.contains("unprivileged=1"), "{}", create.body);
    assert!(
        !create.body.contains("entrypoint") && !create.body.contains("env="),
        "the create must not carry entrypoint/env, the node replaces them: {}",
        create.body
    );
    let put = node
        .log()
        .into_iter()
        .find(|s| s.method == "PUT" && s.path == CT_CONFIG)
        .expect("the configure");
    assert!(
        put.body.contains("entrypoint=/bin/sleep+3600"),
        "{}",
        put.body
    );
    let delete = node
        .log()
        .into_iter()
        .find(|s| s.method == "DELETE")
        .expect("the destroy");
    assert!(
        delete.query.contains("purge=1") && delete.query.contains("destroy-unreferenced-disks=1"),
        "{}",
        delete.query
    );
}

/// A privileged container is refused by name, before anything reaches the node.
#[test]
fn a_privileged_system_container_is_refused_before_any_call() {
    use delonix_compute::system_container::SystemContainerProvider;
    let node = MockNode::start(script(&[]));
    let dir = tempfile::tempdir().unwrap();
    let mut spec = ct_spec(dir.path());
    spec.unprivileged = false;
    let err = ct_provider(&node).create(dir.path(), &spec).unwrap_err();
    assert_eq!(err.number(), 1540, "{err}");
    // `connect_with` reads `/nodes` once; nothing else may reach the node.
    let calls: Vec<_> = node
        .log()
        .into_iter()
        .filter(|s| s.path != "/nodes")
        .collect();
    assert!(calls.is_empty(), "the node was called: {calls:?}");
}

/// T3: the start ends `WARNINGS: 1` because DHCP got no answer. The start
/// succeeds, the container is running, and the network is `NotReady` with the
/// node's warning — not "running, all good".
#[test]
fn a_system_container_whose_dhcp_failed_runs_with_the_network_not_ready() {
    use delonix_compute::system_container::{NetworkState, SystemContainerProvider};
    const START: &str = "UPID:pve:00000502:00000602:6AB90022:vzstart:100:root@pam:";
    let mut steps = ct_cached_template();
    steps.push(("PUT", CT_CONFIG, ok_data("null")));
    steps.push((
        "GET",
        CT_CONFIG,
        ok_data(r#"{"entrypoint":"/bin/sleep 3600","env":"DLX_TEST=one","unprivileged":1}"#),
    ));
    // The start resolves the container first: it is on the node its
    // locator names (`located`), so no cluster read follows.
    steps.push((
        "GET",
        CT_CONFIG,
        ok_data(r#"{"entrypoint":"/bin/sleep 3600","env":"DLX_TEST=one","unprivileged":1}"#),
    ));
    steps.push((
        "POST",
        "/nodes/pve/lxc/100/status/start",
        ok_data(&format!("\"{START}\"")),
    ));
    steps.push((
        "GET",
        leak(format!("/nodes/pve/tasks/{START}/status")),
        ok_data(r#"{"status":"stopped","exitstatus":"WARNINGS: 1"}"#),
    ));
    steps.push((
        "GET",
        leak(format!("/nodes/pve/tasks/{START}/log")),
        ok_data(
            r#"[{"n":1,"t":"WARN: DHCP failed - command 'lxc-attach -n 100 -- dhclient eth0' failed: exit code 2"},{"n":2,"t":"TASK WARNINGS: 1"}]"#,
        ),
    ));
    steps.push((
        "GET",
        "/nodes/pve/lxc/100/status/current",
        ok_data(r#"{"status":"running"}"#),
    ));
    steps.push((
        "GET",
        "/nodes/pve/lxc/100/interfaces",
        ok_data(r#"[{"name":"lo","inet":"127.0.0.1/8"},{"name":"eth0","inet6":"fe80::1/64"}]"#),
    ));
    let node = MockNode::start(script(&steps));
    let dir = tempfile::tempdir().unwrap();
    let p = ct_provider(&node);
    let spec = ct_spec(dir.path());
    let h = p.create(dir.path(), &spec).expect("create");
    assert_eq!(h.locator, "proxmox:pve:100");
    let obs = p
        .start(dir.path(), &h, &spec)
        .expect("a start with warnings succeeds");
    assert!(obs.running);
    match obs.network {
        NetworkState::NotReady { reason } => {
            assert!(reason.contains("DHCP failed"), "{reason}")
        }
        other => panic!("expected NotReady, got {other:?}"),
    }
    let ledger = Ledger::at(dir.path());
    assert!(
        ledger
            .records()
            .iter()
            .any(|r| r.action == "ct-start" && matches!(r.state, TaskState::OkWithWarnings { .. })),
        "{:?}",
        ledger.records()
    );
}
