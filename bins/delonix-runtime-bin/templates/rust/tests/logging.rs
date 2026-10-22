//! The redaction helper: a value whose key names a secret never reaches a log.

use __MODULE__::logging::{is_sensitive, redact, REDACTED};

#[test]
fn secret_keys_are_recognised_case_insensitively() {
    for key in ["authorization", "Cookie", "PASSWORD", "api_key", "webhook-signature", "token"] {
        assert!(is_sensitive(key), "{key} should be sensitive");
    }
    for key in ["status", "method", "note_id", "duration_ms"] {
        assert!(!is_sensitive(key), "{key} should not be sensitive");
    }
}

#[test]
fn redact_replaces_a_secret_value_and_keeps_a_plain_one() {
    assert_eq!(redact("authorization", "Bearer abc"), REDACTED);
    assert_eq!(redact("status", "200"), "200");
}
