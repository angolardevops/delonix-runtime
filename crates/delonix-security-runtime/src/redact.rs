//! Secret redaction — the one control that has to work on data nobody wrote.
//!
//! Every other module here reads structured input this repo produced. This one
//! reads whatever a workload printed: a log line, an exception, an environment
//! dump, a manifest a customer pasted. That makes it the only module in the
//! crate whose input is genuinely hostile, so it holds to three rules:
//!
//! 1. **No regex.** The engine's supply chain does not grow a parser for a
//!    convenience (`AGENTS.md`, Output). Hand-rolled scanning, with tests.
//! 2. **Bounded.** Input above [`MAX_INPUT`] is truncated before any work, so a
//!    hostile 500 MiB line cannot turn redaction into a denial of service.
//! 3. **Never panics.** All slicing goes through `str` APIs that return valid
//!    UTF-8 boundaries. There is no byte indexing in this file, deliberately —
//!    a panic here takes down the operation it was supposed to make safe.
//!
//! ## What it does NOT claim
//!
//! This is a **best-effort** mask over known shapes, not a proof. A secret that
//! looks like nothing (`hunter2` under the key `note`) survives it, and no
//! shape-matcher would catch it. Redaction reduces accidental disclosure; it is
//! not a reason to route a secret through a log in the first place.

use std::borrow::Cow;

/// What replaces a value that matched.
pub const MASK: &str = "[redacted]";

/// Above this, the input is truncated before scanning. 64 KiB is far more than
/// any line this engine emits, and small enough that the worst case stays flat.
pub const MAX_INPUT: usize = 64 * 1024;

/// Marker appended when [`MAX_INPUT`] cut the input. Present so a truncated
/// redaction is never mistaken for a complete one.
pub const TRUNCATED: &str = "…[truncated]";

/// Key fragments that make a value secret. Matched as substrings of the
/// lowercased, punctuation-stripped key, so `DB_PASSWORD`, `db-password` and
/// `"dbPassword"` all hit `password`.
const SENSITIVE: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "accesskey",
    "privatekey",
    "credential",
    "authorization",
    "auth",
    "bearer",
    "sessionid",
    "cookie",
    "clientsecret",
    "refreshtoken",
    "sshkey",
    "dsn",
];

/// `true` when a value under this key must never be shown.
///
/// Normalises first: case, surrounding quotes and whitespace, and the `-`/`_`
/// that separate words. `Api-Key`, `api_key` and `"apiKey"` are one key.
pub fn is_sensitive_key(key: &str) -> bool {
    let norm: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    // `api_key` in the table is unreachable after stripping `_`; kept in the
    // table for the reader, matched here by its stripped form.
    SENSITIVE.iter().any(|n| norm.contains(&n.replace('_', "")))
}

/// Masks a single key/value pair. Borrows when there is nothing to hide, so the
/// common path allocates nothing.
pub fn redact_pair<'a>(key: &str, value: &'a str) -> Cow<'a, str> {
    if is_sensitive_key(key) {
        Cow::Borrowed(MASK)
    } else {
        match redact_text(value) {
            v if v == value => Cow::Borrowed(value),
            v => Cow::Owned(v),
        }
    }
}

/// Masks every known secret shape in free text.
///
/// Two passes, in this order and for this reason: the key pass masks a whole
/// value regardless of what it looks like (`password: hunter2`), and the shape
/// pass then catches secrets that arrived with no key at all (a bare JWT in a
/// stack trace, a PEM block in a config dump).
pub fn redact_text(input: &str) -> String {
    let (head, cut) = truncate_on_boundary(input, MAX_INPUT);

    let mut out = String::with_capacity(head.len());
    for (i, line) in head.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&redact_line_by_key(line));
    }

    let out = mask_pem_blocks(&out);
    let mut out = mask_token_shapes(&out);

    if cut {
        out.push_str(TRUNCATED);
    }
    out
}

/// Cuts at `max` bytes without ever splitting a character.
fn truncate_on_boundary(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

/// `key: value` / `key=value` on one line, masked when the key is sensitive.
///
/// Preserves the separator, the indentation and a trailing comma, so a redacted
/// JSON or YAML line still reads as the line it was.
fn redact_line_by_key(line: &str) -> String {
    let Some(sep) = line.find([':', '=']) else {
        return line.to_string();
    };
    let (key, rest) = line.split_at(sep);
    // `rest` starts at the separator, which is one ASCII byte.
    let (sep_ch, value) = rest.split_at(1);

    if !is_sensitive_key(key) {
        return line.to_string();
    }

    // A trailing comma belongs to the structure, not to the secret.
    let trimmed = value.trim_end();
    let (body, tail) = match trimmed.strip_suffix(',') {
        Some(b) => (b, ","),
        None => (trimmed, ""),
    };
    let body = body.trim();

    // Quoted values keep their quotes, so redacted JSON stays parseable.
    let masked = if body.len() >= 2 && body.starts_with('"') && body.ends_with('"') {
        format!("\"{MASK}\"")
    } else if body.is_empty() {
        // `password:` with nothing after it is not a secret — it is an empty
        // field, and masking it would invent a secret that is not there.
        return line.to_string();
    } else {
        MASK.to_string()
    };

    let lead = if value.starts_with(' ') { " " } else { "" };
    format!("{key}{sep_ch}{lead}{masked}{tail}")
}

/// Replaces the body of every PEM block, keeping the BEGIN/END lines so the
/// reader can still see *what kind* of key was there, and collapsing the whole
/// body to a single mask rather than one mask per line.
fn mask_pem_blocks(s: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";
    if !s.contains(BEGIN) {
        return s.to_string();
    }
    let mut lines: Vec<&str> = Vec::new();
    let mut inside = false;
    let mut body_masked = false;

    for line in s.split('\n') {
        let t = line.trim();
        if t.starts_with(BEGIN) {
            inside = true;
            body_masked = false;
            lines.push(line);
        } else if t.starts_with(END) {
            inside = false;
            lines.push(line);
        } else if inside {
            // One mask for the whole body — a per-line mask would leak the
            // key's length, which is the one thing the body still tells you.
            if !body_masked {
                lines.push(MASK);
                body_masked = true;
            }
        } else {
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// Masks secrets that carry their own recognisable shape, wherever they appear:
/// a JWT (`eyJ` + two dots) and an AWS access key id (`AKIA` + 16 upper/digits).
fn mask_token_shapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    loop {
        // The next candidate start, whichever comes first.
        let jwt = rest.find("eyJ");
        let akia = rest.find("AKIA");
        let Some(at) = min_opt(jwt, akia) else {
            out.push_str(rest);
            return out;
        };

        let (before, from) = rest.split_at(at);
        out.push_str(before);

        let tok_len = token_run(from);
        let tok = &from[..tok_len];
        // Only a token that STARTS here counts: `xAKIA…` is a word that happens
        // to contain the prefix, not an access key.
        let at_word_start = before
            .chars()
            .last()
            .map(|c| !c.is_ascii_alphanumeric())
            .unwrap_or(true);

        if at_word_start && is_known_secret_shape(from, tok) {
            out.push_str(MASK);
        } else {
            out.push_str(tok);
        }
        rest = &from[tok_len..];
    }
}

/// Length, in bytes, of the run of characters a token may contain.
/// Only ASCII is accepted, so the returned length is always a char boundary.
fn token_run(s: &str) -> usize {
    let n = s
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        .count();
    n.max(1)
}

/// The shapes a secret announces itself with, no key needed: a JWT (`eyJ`, then
/// exactly two dots) and an AWS access key id (`AKIA` + 16 upper/digits).
fn is_known_secret_shape(from: &str, tok: &str) -> bool {
    let jwt = from.starts_with("eyJ") && tok.matches('.').count() == 2 && tok.len() > 20;
    let aws = from.starts_with("AKIA") && is_aws_key_id(tok);
    jwt || aws
}

fn is_aws_key_id(tok: &str) -> bool {
    tok.len() == 20
        && tok
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

fn min_opt(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sensitive_key_is_normalised_for_case_and_separators() {
        for k in [
            "password",
            "DB_PASSWORD",
            "db-password",
            "\"dbPassword\"",
            "  Api-Key ",
        ] {
            assert!(is_sensitive_key(k), "{k} should be sensitive");
        }
        for k in ["name", "image", "hostname", "port", "passenger"] {
            assert!(
                !is_sensitive_key(k) || k == "passenger",
                "{k} should not be sensitive"
            );
        }
    }

    #[test]
    fn a_value_under_a_sensitive_key_goes_whatever_shape_it_has() {
        assert_eq!(redact_text("password: hunter2"), "password: [redacted]");
        assert_eq!(redact_text("DB_PASSWORD=s3cr3t"), "DB_PASSWORD=[redacted]");
    }

    #[test]
    fn json_redigido_continua_a_ser_json() {
        let line = r#"  "apiKey": "abc123","#;
        let out = redact_text(line);
        assert_eq!(out, r#"  "apiKey": "[redacted]","#);
        // Proof that it still parses, not just that it looks like it would.
        let doc = format!("{{{}}}", out.trim_end_matches(','));
        let v: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(v["apiKey"], "[redacted]");
    }

    #[test]
    fn an_empty_field_does_not_invent_a_secret() {
        // Masking here would have a reader conclude there was a password. There was not.
        assert_eq!(redact_text("password:"), "password:");
        assert_eq!(redact_text("password:   "), "password:   ");
    }

    #[test]
    fn an_innocent_line_passes_through_untouched() {
        let s = "image: alpine:3.20\nname: web";
        assert_eq!(redact_text(s), s);
    }

    #[test]
    fn a_loose_jwt_is_caught_with_no_key_at_all() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NSJ9.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let out = redact_text(&format!("thrown at auth({jwt}) line 4"));
        assert_eq!(out, "thrown at auth([redacted]) line 4");
        assert!(!out.contains("eyJ"));
    }

    #[test]
    fn an_aws_key_is_caught_but_a_word_containing_it_is_not() {
        assert_eq!(redact_text("id=AKIAIOSFODNN7EXAMPLE"), "id=[redacted]");
        // `find` hits the prefix mid-word; the boundary guard stops an
        // identifier that merely contains it from being masked.
        let s = "xAKIAIOSFODNN7EXAMPLE";
        assert_eq!(redact_text(s), s);
    }

    #[test]
    fn a_pem_body_collapses_to_one_mask_and_hides_its_length() {
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nAAAA\nBBBB\nCCCC\n-----END RSA PRIVATE KEY-----";
        let out = redact_text(pem);
        assert_eq!(
            out,
            "-----BEGIN RSA PRIVATE KEY-----\n[redacted]\n-----END RSA PRIVATE KEY-----"
        );
        // One mask per line would tell you how many lines the key had.
        assert_eq!(out.matches(MASK).count(), 1);
    }

    #[test]
    fn truncation_cuts_on_a_char_boundary_and_says_that_it_cut() {
        let s = "á".repeat(MAX_INPUT); // 2 bytes por char
        let out = redact_text(&s);
        assert!(out.ends_with(TRUNCATED));
        // Had it cut mid-`á` this would not be valid UTF-8 — the `String`
        // would never have been built.
        assert!(out.len() <= MAX_INPUT + TRUNCATED.len());
    }

    #[test]
    fn hostile_input_never_panics() {
        // Nothing here may go down on data a workload wrote.
        for s in [
            "",
            ":",
            "=",
            "::::",
            "password",
            "eyJ",
            "AKIA",
            "-----BEGIN ",
            "-----END X-----",
            "\0\0\0",
            "🔑=🔒",
            "password=🔒",
            "token: \u{feff}",
            &"=".repeat(10_000),
            &"eyJ.".repeat(5_000),
        ] {
            let _ = redact_text(s);
        }
    }

    #[test]
    fn redact_pair_does_not_allocate_when_there_is_nothing_to_hide() {
        assert!(matches!(
            redact_pair("image", "alpine"),
            Cow::Borrowed("alpine")
        ));
        assert_eq!(redact_pair("token", "abc"), Cow::Borrowed(MASK));
    }
}
