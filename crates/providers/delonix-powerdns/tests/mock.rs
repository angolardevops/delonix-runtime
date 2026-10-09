//! The client against a scripted PowerDNS on loopback (plain HTTP, allowed
//! explicitly): what it sends, and how it reads each answer. The answers'
//! shapes are the ones measured on PowerDNS 4.9.17 (ADR-0064 D6): a bare
//! `Not Found` for a missing zone AND a wrong server id, `Unauthorized` for a
//! wrong key, and the rrset filter honoured.

use delonix_powerdns::{Client, Removal, Target};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// One request as the mock saw it.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    key: Option<String>,
    body: String,
}

/// A loopback server that answers each request with the first script entry
/// whose `(method, path prefix)` matches, and records every request.
struct Mock {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

type Script = Vec<(&'static str, String, u16, String)>;

impl Mock {
    fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.is_empty() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let mut len = 0usize;
                let mut key = None;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end().to_string();
                    if h.is_empty() {
                        break;
                    }
                    let (n, v) = h.split_once(':').unwrap_or((&h, ""));
                    if n.eq_ignore_ascii_case("content-length") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    if n.eq_ignore_ascii_case("x-api-key") {
                        key = Some(v.trim().to_string());
                    }
                }
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8_lossy(&body).into_owned();
                let (status, answer) = script
                    .iter()
                    .find(|(m, p, _, _)| *m == method && path.starts_with(p.as_str()))
                    .map(|(_, _, s, a)| (*s, a.clone()))
                    .unwrap_or((500, "{\"error\":\"unscripted\"}".into()));
                log.lock().unwrap().push(Seen {
                    method,
                    path,
                    key,
                    body,
                });
                let reason = match status {
                    200 => "OK",
                    204 => "No Content",
                    401 => "Unauthorized",
                    404 => "Not Found",
                    _ => "Other",
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                );
            }
        });
        Mock { port, seen }
    }

    fn target(&self, key: &str) -> Target {
        Target {
            server_url: format!("http://127.0.0.1:{}/api/v1/servers/localhost", self.port),
            key: key.into(),
            allow_plain_http: true,
            insecure_tls: false,
            ca_cert_pem: None,
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

const SERVER: &str = r#"{"type":"Server","id":"localhost","version":"4.9.17"}"#;
const BASE: &str = "/api/v1/servers/localhost";

fn zone(rrsets: &str) -> String {
    format!(r#"{{"id":"f5c.lab.","name":"f5c.lab.","rrsets":[{rrsets}]}}"#)
}

#[test]
fn a_record_shared_with_another_is_removed_and_the_other_kept() {
    let mock = Mock::start(vec![
        (
            "GET",
            format!("{BASE}/zones/"),
            200,
            zone(
                r#"{"name":"v-gw.f5c.lab.","type":"A","ttl":3600,"records":[{"content":"10.84.0.1","disabled":false},{"content":"10.84.0.254","disabled":false}]}"#,
            ),
        ),
        (
            "PATCH",
            format!("{BASE}/zones/f5c.lab."),
            204,
            String::new(),
        ),
        ("GET", BASE.into(), 200, SERVER.into()),
    ]);
    let c = Client::connect(&mock.target("lab-key")).expect("connect");
    assert_eq!(
        c.remove_record("f5c.lab", "v-gw.f5c.lab", "A", "10.84.0.1")
            .expect("remove"),
        Removal::Removed
    );
    let seen = mock.seen();
    assert!(seen.iter().all(|s| s.key.as_deref() == Some("lab-key")));
    let get = &seen[1];
    assert!(
        get.path.contains("rrset_name=v-gw.f5c.lab.") && get.path.contains("rrset_type=A"),
        "{get:?}"
    );
    let patch = &seen[2];
    assert_eq!(patch.method, "PATCH");
    let body: serde_json::Value = serde_json::from_str(&patch.body).unwrap();
    let rrset = &body["rrsets"][0];
    assert_eq!(rrset["changetype"], "REPLACE");
    assert_eq!(rrset["ttl"], 3600);
    assert_eq!(rrset["records"].as_array().unwrap().len(), 1);
    assert_eq!(rrset["records"][0]["content"], "10.84.0.254");
    // The key never travels in a body or a path.
    assert!(seen
        .iter()
        .all(|s| !s.body.contains("lab-key") && !s.path.contains("lab-key")));
}

#[test]
fn a_last_record_deletes_the_rrset() {
    let mock = Mock::start(vec![
        (
            "GET",
            format!("{BASE}/zones/"),
            200,
            zone(
                r#"{"name":"v-gw.f5c.lab.","type":"A","ttl":3600,"records":[{"content":"10.84.0.1","disabled":false}]}"#,
            ),
        ),
        ("PATCH", format!("{BASE}/zones/"), 204, String::new()),
        ("GET", BASE.into(), 200, SERVER.into()),
    ]);
    let c = Client::connect(&mock.target("k")).expect("connect");
    assert_eq!(
        c.remove_record("f5c.lab.", "v-gw.f5c.lab.", "A", "10.84.0.1")
            .unwrap(),
        Removal::Removed
    );
    let body: serde_json::Value = serde_json::from_str(&mock.seen()[2].body).unwrap();
    assert_eq!(body["rrsets"][0]["changetype"], "DELETE");
}

#[test]
fn an_absent_record_or_zone_writes_nothing() {
    let mock = Mock::start(vec![
        (
            "GET",
            format!("{BASE}/zones/nope.lab."),
            404,
            "Not Found".into(),
        ),
        (
            "GET",
            format!("{BASE}/zones/"),
            200,
            zone(
                r#"{"name":"v-gw.f5c.lab.","type":"A","ttl":3600,"records":[{"content":"10.84.0.254","disabled":false}]}"#,
            ),
        ),
        ("GET", BASE.into(), 200, SERVER.into()),
    ]);
    let c = Client::connect(&mock.target("k")).expect("connect");
    assert_eq!(
        c.remove_record("f5c.lab", "v-gw.f5c.lab", "A", "10.84.0.1")
            .unwrap(),
        Removal::Absent
    );
    assert_eq!(
        c.remove_record("f5c.lab", "w-gw.f5c.lab", "A", "10.84.0.1")
            .unwrap(),
        Removal::Absent,
        "an rrset the zone does not hold"
    );
    assert_eq!(
        c.remove_record("nope.lab", "v-gw.nope.lab", "A", "10.84.0.1")
            .unwrap(),
        Removal::ZoneAbsent
    );
    assert!(
        mock.seen().iter().all(|s| s.method == "GET"),
        "a write was sent"
    );
}

#[test]
fn a_wrong_key_and_a_wrong_server_id_fail_at_connect() {
    let mock = Mock::start(vec![("GET", BASE.into(), 401, "Unauthorized".into())]);
    let e = delonix_model::Error::from(Client::connect(&mock.target("wrong")).unwrap_err());
    assert_eq!(delonix_model::exitcode::for_error(&e), 77, "{e}");
    assert!(!e.to_string().contains("wrong"), "the key was quoted: {e}");

    let mock = Mock::start(vec![("GET", BASE.into(), 404, "Not Found".into())]);
    let e = Client::connect(&mock.target("k")).unwrap_err();
    assert!(e.to_string().contains("server id"), "{e}");
}

#[test]
fn a_server_error_on_the_write_is_a_failure_not_a_removal() {
    let mock = Mock::start(vec![
        (
            "GET",
            format!("{BASE}/zones/"),
            200,
            zone(
                r#"{"name":"v-gw.f5c.lab.","type":"A","ttl":3600,"records":[{"content":"10.84.0.1","disabled":false}]}"#,
            ),
        ),
        (
            "PATCH",
            format!("{BASE}/zones/"),
            422,
            r#"{"error":"RRset v-gw.f5c.lab. IN A: bad"}"#.into(),
        ),
        ("GET", BASE.into(), 200, SERVER.into()),
    ]);
    let c = Client::connect(&mock.target("k")).expect("connect");
    let e = c
        .remove_record("f5c.lab", "v-gw.f5c.lab", "A", "10.84.0.1")
        .unwrap_err();
    assert!(
        e.to_string().contains("422") && e.to_string().contains("bad"),
        "{e}"
    );
}

#[test]
fn a_server_that_does_not_answer_is_unavailable() {
    // A port nothing listens on.
    let free = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let t = Target {
        server_url: format!("http://127.0.0.1:{port}/api/v1/servers/localhost"),
        key: "k".into(),
        allow_plain_http: true,
        insecure_tls: false,
        ca_cert_pem: None,
    };
    let e = delonix_model::Error::from(Client::connect(&t).unwrap_err());
    assert_eq!(delonix_model::exitcode::for_error(&e), 69, "{e}");
}
