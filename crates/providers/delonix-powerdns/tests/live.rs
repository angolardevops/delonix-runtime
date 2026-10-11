//! The client against a REAL PowerDNS (ADR-0064 D6). `#[ignore]`d — `cargo
//! test` lists it as `ignored`, never as `passed`. Run with `--ignored` and
//! `DELONIX_POWERDNS_TEST_URL` (`http(s)://<host>:<port>/api/v1/servers/<id>`),
//! `DELONIX_POWERDNS_TEST_KEY_FILE` and `DELONIX_POWERDNS_TEST_ZONE` (a zone
//! the server already serves); one left unset fails the test naming it:
//!
//! ```text
//! DELONIX_POWERDNS_TEST_URL=http://192.168.122.55:8081/api/v1/servers/localhost \
//! DELONIX_POWERDNS_TEST_KEY_FILE=$HOME/.cache/dlx-slice6/pdns-lab.key \
//! DELONIX_POWERDNS_TEST_ZONE=f5c.lab \
//! cargo test -p delonix-powerdns --test live -- --ignored --nocapture
//! ```
//!
//! It writes one rrset of its own (named after the process id), with the
//! test's own `PATCH` — the client never creates a record — and removes it on
//! every exit.

use delonix_powerdns::{Client, Removal, Target};

struct Lab {
    url: String,
    key: String,
    zone: String,
}

/// A variable the test needs. It runs only under `--ignored`, so a missing
/// one FAILS naming it: returning instead would read as `passed`.
fn need(var: &str) -> String {
    std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| panic!("{var} must be set to run this --ignored test"))
}

fn lab() -> Lab {
    let url = need("DELONIX_POWERDNS_TEST_URL");
    let key_file = need("DELONIX_POWERDNS_TEST_KEY_FILE");
    let key = std::fs::read_to_string(&key_file)
        .unwrap_or_else(|e| panic!("DELONIX_POWERDNS_TEST_KEY_FILE={key_file}: {e}"))
        .trim()
        .to_string();
    let zone = need("DELONIX_POWERDNS_TEST_ZONE");
    Lab { url, key, zone }
}

fn patch(lab: &Lab, rrset: serde_json::Value) {
    reqwest::blocking::Client::new()
        .patch(format!(
            "{}/zones/{}.",
            lab.url,
            lab.zone.trim_end_matches('.')
        ))
        .header("X-API-Key", &lab.key)
        .json(&serde_json::json!({ "rrsets": [rrset] }))
        .send()
        .and_then(|r| r.error_for_status())
        .expect("the test's own PATCH");
}

/// Deletes the test's rrset on every exit.
struct Cleanup<'a>(&'a Lab, String);

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = reqwest::blocking::Client::new()
            .patch(format!(
                "{}/zones/{}.",
                self.0.url,
                self.0.zone.trim_end_matches('.')
            ))
            .header("X-API-Key", &self.0.key)
            .json(&serde_json::json!({ "rrsets": [
                { "name": self.1, "type": "A", "changetype": "DELETE", "records": [] }
            ]}))
            .send();
    }
}

#[ignore = "needs DELONIX_POWERDNS_TEST_URL, DELONIX_POWERDNS_TEST_KEY_FILE, DELONIX_POWERDNS_TEST_ZONE and a reachable PowerDNS: cargo test -p delonix-powerdns --test live -- --ignored"]
#[test]
fn one_record_is_removed_and_the_rest_of_its_rrset_kept() {
    let lab = lab();
    let zone = lab.zone.trim_end_matches('.').to_string();
    let name = format!("dlx-d6-{}.{zone}.", std::process::id());
    let _cleanup = Cleanup(&lab, name.clone());
    patch(
        &lab,
        serde_json::json!({
            "name": name, "type": "A", "ttl": 300, "changetype": "REPLACE",
            "records": [
                { "content": "10.99.0.1", "disabled": false },
                { "content": "10.99.0.2", "disabled": false },
            ],
        }),
    );
    let c = Client::connect(&Target {
        server_url: lab.url.clone(),
        key: lab.key.clone(),
        allow_plain_http: true,
        insecure_tls: false,
        ca_cert_pem: None,
    })
    .expect("connect with the engine's own key");
    let mut seen = c.record_contents(&zone, &name, "A").unwrap().unwrap();
    seen.sort();
    assert_eq!(seen, vec!["10.99.0.1", "10.99.0.2"]);

    assert_eq!(
        c.remove_record(&zone, &name, "A", "10.99.0.1").unwrap(),
        Removal::Removed
    );
    assert_eq!(
        c.record_contents(&zone, &name, "A").unwrap().unwrap(),
        vec!["10.99.0.2"],
        "the other record of the rrset was not kept"
    );
    assert_eq!(
        c.remove_record(&zone, &name, "A", "10.99.0.1").unwrap(),
        Removal::Absent,
        "a second removal is not idempotent"
    );
    assert_eq!(
        c.remove_record(&zone, &name, "A", "10.99.0.2").unwrap(),
        Removal::Removed
    );
    assert!(
        c.record_contents(&zone, &name, "A")
            .unwrap()
            .unwrap()
            .is_empty(),
        "the last record did not delete the rrset"
    );
    assert_eq!(
        c.remove_record(
            "no-such-zone.invalid",
            "x.no-such-zone.invalid",
            "A",
            "10.99.0.1"
        )
        .unwrap(),
        Removal::ZoneAbsent
    );

    // A wrong key fails at connect, as a refused credential (exit 77), and
    // the message does not carry the key.
    let e = delonix_model::Error::from(
        Client::connect(&Target {
            server_url: lab.url.clone(),
            key: "not-the-key".into(),
            allow_plain_http: true,
            insecure_tls: false,
            ca_cert_pem: None,
        })
        .unwrap_err(),
    );
    assert_eq!(delonix_model::exitcode::for_error(&e), 77, "{e}");
    assert!(!e.to_string().contains("not-the-key"));
}
