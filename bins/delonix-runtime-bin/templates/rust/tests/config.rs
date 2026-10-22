//! Configuration: the defaults, that every bad value is reported at once, the
//! duration parser, and the outbound-webhook rules.

use std::collections::HashMap;
use std::time::Duration;

use __MODULE__::config::{self, Environment};

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> =
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |k: &str| map.get(k).cloned()
}

#[test]
fn defaults_are_sane() {
    let cfg = config::load(env(&[]), "1.2.3").unwrap();
    assert_eq!(cfg.env, Environment::Development);
    assert_eq!(cfg.log_level, "info");
    assert_eq!(cfg.version, "1.2.3");
    assert_eq!(cfg.port, 8000);
    assert_eq!(cfg.addr(), "0.0.0.0:8000");
    assert_eq!(cfg.max_body_bytes, 1 << 20);
    assert_eq!(cfg.shutdown_timeout, Duration::from_secs(15));
    assert!(cfg.redis_url.is_none());
    assert!(cfg.inbound_webhook_secret.is_none());
}

#[test]
fn every_bad_value_is_reported_at_once() {
    let errs = config::load(
        env(&[
            ("APP_ENV", "staging"),
            ("LOG_LEVEL", "silly"),
            ("PORT", "no"),
            ("SHUTDOWN_TIMEOUT", "-1"),
            ("HTTP_MAX_BODY_BYTES", "0"),
        ]),
        "dev",
    )
    .unwrap_err();
    for needle in ["APP_ENV", "LOG_LEVEL", "PORT", "SHUTDOWN_TIMEOUT", "HTTP_MAX_BODY_BYTES"] {
        assert!(errs.iter().any(|e| e.contains(needle)), "missing {needle} in {errs:?}");
    }
}

#[test]
fn durations_parse_their_units() {
    assert_eq!(config::parse_duration("15"), Some(Duration::from_secs(15)));
    assert_eq!(config::parse_duration("15s"), Some(Duration::from_secs(15)));
    assert_eq!(config::parse_duration("500ms"), Some(Duration::from_millis(500)));
    assert_eq!(config::parse_duration("2m"), Some(Duration::from_secs(120)));
    assert_eq!(config::parse_duration("1h"), Some(Duration::from_secs(3600)));
    assert_eq!(config::parse_duration("-1s"), None);
    assert_eq!(config::parse_duration("abc"), None);
}

#[test]
fn outbound_webhook_requires_a_secret_and_https_in_production() {
    // URL without a secret is refused.
    let errs = config::load(env(&[("WEBHOOK_TARGET_URL", "https://h/x")]), "dev").unwrap_err();
    assert!(errs.iter().any(|e| e.contains("WEBHOOK_TARGET_SECRET")));

    // Plain http in production is refused.
    let errs = config::load(
        env(&[
            ("APP_ENV", "production"),
            ("WEBHOOK_TARGET_URL", "http://h/x"),
            ("WEBHOOK_TARGET_SECRET", "whsec_x"),
        ]),
        "dev",
    )
    .unwrap_err();
    assert!(errs.iter().any(|e| e.contains("https only")), "{errs:?}");

    // https + secret is accepted.
    let cfg = config::load(
        env(&[
            ("WEBHOOK_TARGET_URL", "https://h/x"),
            ("WEBHOOK_TARGET_SECRET", "whsec_x"),
        ]),
        "dev",
    )
    .unwrap();
    assert_eq!(cfg.outbound_webhook_url.as_deref(), Some("https://h/x"));
}
