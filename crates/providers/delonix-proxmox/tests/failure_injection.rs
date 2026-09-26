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
        ..token_target(&node)
    };
    Client::connect_with(&with_ca, fast()).expect("verified against the CA given");
    assert_eq!(node.count("GET", "/nodes"), 1);
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
