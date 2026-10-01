//! Exercises the client against a REAL OPNsense appliance.
//!
//! Skipped unless `DELONIX_OPNSENSE_TEST_URL` is set, because it needs one —
//! and a test that quietly passes when its target is absent proves nothing:
//!
//! ```text
//! DELONIX_OPNSENSE_TEST_URL=https://192.168.122.103 \
//! DELONIX_OPNSENSE_TEST_KEY=<key> \
//! DELONIX_OPNSENSE_TEST_SECRET=<secret> \
//!   cargo test -p delonix-opnsense --test live -- --nocapture --test-threads=1
//! ```
//!
//! Two scenarios, each leaving the appliance as it found it:
//!
//! * the full cycle ADR-0051's Phase 0 spike ran by hand with `curl` —
//!   create an alias and a rule referencing it, commit, read back, remove,
//!   commit — now under an owner mark;
//! * the ownership rules of audit 62 (§6 P1) and ADR-0059 D1.5 (the owner
//!   mark is a firewall category) against the real thing: a rule
//!   made by hand with the same description is refused, never adopted and
//!   never deleted; a hand-made change staged and not applied refuses the
//!   commit before anything is applied; an owned rule disabled on the
//!   appliance is drift.
//!
//! The appliance must start CLEAN — nothing staged and not applied — or the
//! first pre-check refuses, which is the behaviour under test, not a flake.

use delonix_networking::gateway::{
    AliasKind, EnsureOutcome, GatewayAlias, GatewayProvider, GatewayRule,
};
use delonix_networking::ownership::{Owner, OwnerMark, RemoveOutcome};
use delonix_opnsense::{Auth, OpnsenseGatewayProvider, Target};

fn target() -> Option<Target> {
    Some(Target {
        base_url: std::env::var("DELONIX_OPNSENSE_TEST_URL").ok()?,
        auth: Auth {
            key: std::env::var("DELONIX_OPNSENSE_TEST_KEY").ok()?,
            secret: std::env::var("DELONIX_OPNSENSE_TEST_SECRET").ok()?,
        },
        insecure_tls: true,
        ca_cert_pem: None,
    })
}

/// The target, or `None` after saying the test was skipped — the one place
/// a skipped live case speaks, so a run without an appliance never reads as
/// a pass.
fn target_or_skip() -> Option<Target> {
    let t = target();
    if t.is_none() {
        eprintln!("SKIP: DELONIX_OPNSENSE_TEST_URL is not set");
    }
    t
}

/// A fresh mark per run — `RandomState` is seeded from the OS, which is all
/// a test needs to keep two runs (or two scenarios) apart.
fn fresh_mark() -> OwnerMark {
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = [0u8; 16];
    for half in bytes.chunks_mut(8) {
        let n = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        half.copy_from_slice(&n.to_le_bytes());
    }
    OwnerMark::from_random(&bytes)
}

/// The appliance's API WITHOUT this crate — the operator's hand, making a
/// rule with no owner mark.
struct Hand {
    http: reqwest::blocking::Client,
    t: Target,
}

impl Hand {
    fn new(t: &Target) -> Self {
        let http = reqwest::blocking::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        Self { http, t: t.clone() }
    }

    fn post(&self, path: &str, body: serde_json::Value) -> serde_json::Value {
        self.http
            .post(format!("{}/api/{path}", self.t.base_url))
            .basic_auth(&self.t.auth.key, Some(&self.t.auth.secret))
            .json(&body)
            .send()
            .and_then(|r| r.json())
            .unwrap_or_else(|e| panic!("POST {path}: {e}"))
    }

    fn get(&self, path: &str) -> serde_json::Value {
        self.http
            .get(format!("{}/api/{path}", self.t.base_url))
            .basic_auth(&self.t.auth.key, Some(&self.t.auth.secret))
            .send()
            .and_then(|r| r.json())
            .unwrap_or_else(|e| panic!("GET {path}: {e}"))
    }

    fn add_rule(&self, description: &str) -> String {
        let answer = self.post(
            "firewall/filter/add_rule",
            serde_json::json!({ "rule": {
                "description": description,
                "source_net": "any",
                "destination_net": "10.77.0.0/24",
            }}),
        );
        answer["uuid"]
            .as_str()
            .unwrap_or_else(|| panic!("add_rule answered no uuid: {answer}"))
            .to_string()
    }

    fn del_rule(&self, uuid: &str) {
        self.post(
            &format!("firewall/filter/del_rule/{uuid}"),
            serde_json::json!({}),
        );
    }

    fn disable_rule(&self, uuid: &str) {
        self.post(
            &format!("firewall/filter/toggle_rule/{uuid}/0"),
            serde_json::json!({}),
        );
    }

    fn rule_uuid(&self, description_prefix: &str) -> Option<String> {
        let rows = self.post(
            "firewall/filter/search_rule",
            serde_json::json!({ "current": 1, "rowCount": -1 }),
        );
        rows["rows"].as_array()?.iter().find_map(|r| {
            r["description"]
                .as_str()?
                .starts_with(description_prefix)
                .then(|| r["uuid"].as_str().map(str::to_string))?
        })
    }
}

#[test]
fn ensures_and_removes_an_alias_and_a_rule_against_a_real_appliance() {
    let Some(t) = target_or_skip() else {
        return;
    };
    let provider = OpnsenseGatewayProvider::connect(&t).expect("connect and authenticate");
    let owner = fresh_mark();

    let alias = GatewayAlias {
        name: "delonix_opnsense_live_test".into(),
        kind: AliasKind::Host,
        content: vec!["10.99.99.99".into()],
        description: "delonix-opnsense live test - safe to delete".into(),
    };
    let rule = GatewayRule {
        description: "delonix-opnsense live test - safe to delete".into(),
        source: alias.name.clone(),
        destination: "10.0.0.0/24".into(),
        protocol: Some("TCP".into()),
        ..Default::default()
    };

    provider
        .check_no_foreign_pending()
        .expect("the appliance must start with nothing staged");

    assert_eq!(
        provider
            .ensure_alias(&alias, &owner)
            .expect("create the alias"),
        EnsureOutcome::Created
    );
    assert_eq!(
        provider
            .ensure_alias(&alias, &owner)
            .expect("a second ensure_alias"),
        EnsureOutcome::AlreadyPresent,
        "ensure_alias must be idempotent for its owner"
    );
    assert_eq!(
        provider
            .ensure_rule(&rule, &owner)
            .expect("create the rule"),
        EnsureOutcome::Created
    );
    assert_eq!(
        provider
            .ensure_rule(&rule, &owner)
            .expect("a second ensure_rule"),
        EnsureOutcome::AlreadyPresent,
        "ensure_rule must be idempotent for its owner"
    );
    provider
        .commit()
        .expect("reconfigure aliases and apply the filter — and prove both running");

    // Another record's mark finds the same names and must not own them.
    let stranger = fresh_mark();
    let err = GatewayProvider::ensure_alias(&provider, &alias, &stranger).unwrap_err();
    assert_eq!(err.number(), 5389, "{err}");
    assert!(err.to_string().contains("refusing to adopt"), "{err}");
    assert!(matches!(
        provider.remove_rule(&rule.description, &stranger).unwrap(),
        RemoveOutcome::NotOwned(Owner::Other(_))
    ));

    // Rule first, alias second (the appliance validates a rule's source
    // against the alias table on write).
    assert_eq!(
        provider.remove_rule(&rule.description, &owner).unwrap(),
        RemoveOutcome::Removed
    );
    assert_eq!(
        provider.remove_alias(&alias.name, &owner).unwrap(),
        RemoveOutcome::Removed
    );
    provider.commit().expect("apply the removal");
    assert_eq!(
        provider.remove_rule(&rule.description, &owner).unwrap(),
        RemoveOutcome::Absent,
        "the rule must be gone, not just unlisted"
    );
    assert_eq!(
        provider
            .release_owner(&owner)
            .expect("retire the owner category"),
        RemoveOutcome::Removed
    );
    provider
        .check_no_foreign_pending()
        .expect("the appliance is left with nothing staged");
}

#[test]
fn a_hand_made_rule_is_never_adopted_and_a_hand_made_pending_change_blocks_the_commit() {
    // No SKIP line: a print in a library crate's tests is counted debt, and
    // the first case already says it.
    let Some(t) = target() else {
        return;
    };
    let hand = Hand::new(&t);
    let provider = OpnsenseGatewayProvider::connect(&t).expect("connect");
    let owner = fresh_mark();
    let description = "delonix-opnsense live ownership - safe to delete";
    let rule = GatewayRule {
        description: description.into(),
        source: "any".into(),
        destination: "10.77.0.0/24".into(),
        protocol: None,
        ..Default::default()
    };
    provider
        .check_no_foreign_pending()
        .expect("the appliance must start with nothing staged");

    // 1. The operator saves a rule with the same description, not applied.
    let theirs = hand.add_rule(description);

    // 2. Ours with that description: refused, not adopted.
    let err = provider.ensure_rule(&rule, &owner).unwrap_err();
    assert_eq!(err.number(), 5389, "{err}");
    assert!(err.to_string().contains("refusing to adopt"), "{err}");

    // 3. The commit sees the operator's unapplied rule and refuses.
    let err = provider.commit().unwrap_err();
    assert_eq!(err.number(), 5389, "{err}");
    assert!(
        err.to_string().contains("that are not this engine's"),
        "{err}"
    );
    assert!(err.to_string().contains(&theirs), "{err}");

    // 4. A teardown of ours leaves theirs alone.
    assert_eq!(
        provider.remove_rule(description, &owner).unwrap(),
        RemoveOutcome::NotOwned(Owner::Unmarked)
    );
    assert_eq!(
        hand.rule_uuid(description).as_deref(),
        Some(theirs.as_str())
    );

    // The operator takes it back out; created-then-deleted is not pending.
    hand.del_rule(&theirs);
    provider
        .check_no_foreign_pending()
        .expect("a rule created and deleted before any apply leaves nothing staged");

    // 5. Ours, then disabled by hand on the appliance: drift, not present.
    assert_eq!(
        provider.ensure_rule(&rule, &owner).unwrap(),
        EnsureOutcome::Created
    );
    provider.commit().expect("apply ours");
    let ours = hand
        .rule_uuid(description)
        .expect("our rule is on the appliance");
    hand.disable_rule(&ours);
    let err = provider.ensure_rule(&rule, &owner).unwrap_err();
    assert_eq!(err.number(), 5389, "{err}");
    assert!(
        err.to_string().contains("was changed on the appliance"),
        "{err}"
    );
    assert!(err.to_string().contains("disabled"), "{err}");

    // Clean up: our removal covers our own disabled-not-applied change.
    assert_eq!(
        provider.remove_rule(description, &owner).unwrap(),
        RemoveOutcome::Removed
    );
    provider.commit().expect("apply the removal");
    assert!(hand.rule_uuid(description).is_none());
    assert_eq!(
        provider
            .release_owner(&owner)
            .expect("retire the owner category"),
        RemoveOutcome::Removed
    );
    provider
        .check_no_foreign_pending()
        .expect("the appliance is left with nothing staged");
}

/// ADR-0059 F3d against the real appliance: one direction of the policy IR,
/// lowered by `gateway_rules`, ensured and committed through the provider.
/// What is checked is what the APPLIANCE holds — each field as `search_rule`
/// returns it, and the order pf loaded the rules in (`pf_statistics`, one
/// line per pf rule with the rule's uuid as its label) — never the `Ok` of a
/// call. Then it is all removed and the owner retired.
#[test]
fn a_lowered_policy_lands_on_the_appliance_in_its_order_with_its_fields() {
    let Some(t) = target_or_skip() else {
        return;
    };
    use delonix_networking::policy::{from_container_fw, gateway_rules, golden};
    let provider = OpnsenseGatewayProvider::connect(&t).expect("connect and authenticate");
    let hand = Hand::new(&t);
    let owner = fresh_mark();

    let record = golden::fw(
        "deny",
        "",
        &[
            ("in", "tcp", "22", "10.9.0.0/24", "allow"),
            ("in", "any", "8000-8080", "", "deny"),
        ],
    );
    let mut ir = from_container_fw(&record).unwrap().ingress;
    ir.rules.retain(|r| !r.guardrail);
    ir.rules[0].log = true;
    ir.rules[1].stateful = false;
    let rules = gateway_rules("10.200.0.5", &ir, "delonix-f3d-live", 30000).expect("lowered");
    assert_eq!(rules.len(), 3);

    provider
        .check_no_foreign_pending()
        .expect("the appliance must start with nothing staged");
    for r in &rules {
        assert_eq!(
            provider.ensure_rule(r, &owner).expect("ensure"),
            EnsureOutcome::Created,
            "{}",
            r.description
        );
    }
    provider.commit().expect("apply");
    for r in &rules {
        assert_eq!(
            provider.ensure_rule(r, &owner).expect("ensure again"),
            EnsureOutcome::AlreadyPresent,
            "{}: the new fields read back as drift",
            r.description
        );
    }

    let rows = hand.post(
        "firewall/filter/search_rule",
        serde_json::json!({ "current": 1, "rowCount": -1, "searchPhrase": "delonix-f3d-live" }),
    );
    let row = |d: &str| {
        rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["description"] == d)
            .unwrap_or_else(|| panic!("{d} is not on the appliance: {rows}"))
            .clone()
    };
    let field = |r: &serde_json::Value, k: &str| r[k].as_str().unwrap_or("").to_string();
    let one = row("delonix-f3d-live#1");
    for (k, v) in [
        ("action", "pass"),
        ("protocol", "TCP"),
        ("source_net", "10.9.0.0/24"),
        ("destination_net", "10.200.0.5"),
        ("destination_port", "22"),
        ("log", "1"),
        ("statetype", "keep"),
        ("sequence", "30000"),
    ] {
        assert_eq!(field(&one, k), v, "#1 {k}");
    }
    let two = row("delonix-f3d-live#2");
    for (k, v) in [
        ("action", "block"),
        ("protocol", "TCP/UDP"),
        ("destination_port", "8000-8080"),
        ("log", "0"),
        ("statetype", "none"),
        ("sequence", "30001"),
    ] {
        assert_eq!(field(&two, k), v, "#2 {k}");
    }
    let default = row("delonix-f3d-live#default");
    for (k, v) in [
        ("action", "block"),
        ("source_net", "any"),
        ("destination_net", "10.200.0.5"),
        ("sequence", "30002"),
    ] {
        assert_eq!(field(&default, k), v, "#default {k}");
    }

    // The order pf loaded them in: the first line carrying each uuid.
    let stats = hand.get("diagnostics/firewall/pf_statistics/rules");
    let lines: Vec<&String> = stats["rules"]["filter rules"]
        .as_object()
        .expect("pf_statistics answered its filter rules")
        .keys()
        .collect();
    let position = |uuid: &str| {
        lines
            .iter()
            .position(|l| l.contains(&format!("label \"{uuid}\"")))
            .unwrap_or_else(|| panic!("{uuid} is not loaded in pf"))
    };
    let order: Vec<usize> = [&one, &two, &default]
        .iter()
        .map(|r| position(&field(r, "uuid")))
        .collect();
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "pf did not load the rules in sequence order: {order:?}"
    );
    let two_uuid = field(&two, "uuid");
    let two_lines = lines
        .iter()
        .filter(|l| l.contains(&format!("label \"{two_uuid}\"")))
        .count();
    assert_eq!(two_lines, 2, "TCP/UDP is one pf rule per protocol");

    // ADR-0059 D4, observe: what the appliance holds under the mark reads
    // back as exactly the rules that were lowered…
    use delonix_networking::gateway::gateway_drift;
    let observed = provider.observe(&owner).expect("observe");
    assert_eq!(
        gateway_drift(&[], &rules, &observed),
        Vec::<String>::new(),
        "{observed:?}"
    );
    // …and a rule disabled on the appliance by hand, and applied, is drift.
    hand.disable_rule(&two_uuid);
    hand.post("firewall/filter/apply", serde_json::json!({}));
    let drift = gateway_drift(&[], &rules, &provider.observe(&owner).expect("observe"));
    assert_eq!(
        drift,
        ["rule 'delonix-f3d-live#2' is disabled on the appliance"]
    );
    hand.post(
        &format!("firewall/filter/toggle_rule/{two_uuid}/1"),
        serde_json::json!({}),
    );
    hand.post("firewall/filter/apply", serde_json::json!({}));
    assert!(gateway_drift(&[], &rules, &provider.observe(&owner).unwrap()).is_empty());

    for r in &rules {
        assert_eq!(
            provider.remove_rule(&r.description, &owner).unwrap(),
            RemoveOutcome::Removed
        );
    }
    provider.commit().expect("apply the removal");
    assert_eq!(
        provider.release_owner(&owner).expect("retire the owner"),
        RemoveOutcome::Removed
    );
}
