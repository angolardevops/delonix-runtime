//! Signing and verification (the Standard Webhooks format), and the in-memory
//! dedup set.

use base64::Engine;
use __MODULE__::webhook::{self, Dedup, VerifyError};

fn key() -> Vec<u8> {
    vec![7u8; 32]
}

#[test]
fn parse_secret_requires_the_standard_form() {
    let good = format!("whsec_{}", base64::engine::general_purpose::STANDARD.encode(key()));
    assert!(webhook::parse_secret(&good).is_ok());
    for bad in [
        "",
        "plain",
        "whsec_!!",
        &format!("whsec_{}", base64::engine::general_purpose::STANDARD.encode(b"short")),
    ] {
        assert!(webhook::parse_secret(bad).is_err(), "accepted {bad:?}");
    }
}

#[test]
fn verify_accepts_a_good_signature_and_refuses_the_rest() {
    let key = key();
    let body = br#"{"type":"ping"}"#;
    let now = 1_000_000;
    let sig = webhook::sign(&key, "m1", now, body);

    assert_eq!(webhook::verify(&key, "m1", &now.to_string(), &sig, body, now), Ok(()));
    // Missing headers.
    assert_eq!(
        webhook::verify(&key, "", "", "", body, now),
        Err(VerifyError::MissingHeaders)
    );
    // Stale timestamp (replay window).
    assert_eq!(
        webhook::verify(&key, "m1", &(now - 600).to_string(), &sig, body, now),
        Err(VerifyError::BadTimestamp)
    );
    // Tampered body.
    assert_eq!(
        webhook::verify(&key, "m1", &now.to_string(), &sig, br#"{"type":"pong"}"#, now),
        Err(VerifyError::BadSignature)
    );
    // Wrong key.
    let other = vec![9u8; 32];
    assert_eq!(
        webhook::verify(&other, "m1", &now.to_string(), &sig, body, now),
        Err(VerifyError::BadSignature)
    );
    // Several space-separated signatures (key rotation): one matches.
    let rotated = format!("v1,invalid {sig}");
    assert_eq!(webhook::verify(&key, "m1", &now.to_string(), &rotated, body, now), Ok(()));
}

#[test]
fn dedup_remembers_until_forgotten() {
    let d = Dedup::new(4);
    let now = 1_000;
    assert!(d.first_seen("a", now), "first time is new");
    assert!(!d.first_seen("a", now), "second time is a duplicate");
    d.forget("a");
    assert!(d.first_seen("a", now), "forgotten id is new again");
}

#[test]
fn dedup_evicts_the_oldest_when_full() {
    let d = Dedup::new(2);
    assert!(d.first_seen("a", 1));
    assert!(d.first_seen("b", 2));
    assert!(d.first_seen("c", 3)); // evicts "a"
    assert!(d.first_seen("a", 4), "the oldest was evicted, so it is new again");
    assert!(!d.first_seen("c", 5), "a recent id is still remembered");
}
