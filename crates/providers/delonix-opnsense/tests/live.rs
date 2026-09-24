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
//! Runs the full cycle ADR-0051's Phase 0 spike ran by hand with `curl`:
//! create an alias, create a rule referencing it, commit (reconfigure +
//! apply), confirm both read back correctly, then remove both and commit
//! again — leaving the appliance exactly as it found it, the same way the
//! hand-run spike was cleaned up and confirmed via `firewall/filter/get`.

use delonix_opnsense::{Auth, OpnsenseGatewayProvider, Target};
use delonix_sdn::gateway::{AliasKind, EnsureOutcome, GatewayAlias, GatewayProvider, GatewayRule};

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

#[test]
fn ensures_and_removes_an_alias_and_a_rule_against_a_real_appliance() {
    let Some(t) = target() else {
        eprintln!("SKIP: DELONIX_OPNSENSE_TEST_URL is not set");
        return;
    };
    let provider = OpnsenseGatewayProvider::connect(&t).expect("connect and authenticate");

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
    };

    // Clean slate: a previous failed run may have left these behind.
    let _ = provider.remove_rule(&rule.description);
    let _ = provider.remove_alias(&alias.name);

    let outcome = provider.ensure_alias(&alias).expect("create the alias");
    assert_eq!(outcome, EnsureOutcome::Created);
    let outcome = provider
        .ensure_alias(&alias)
        .expect("a second ensure_alias must not fail");
    assert_eq!(
        outcome,
        EnsureOutcome::AlreadyPresent,
        "ensure_alias must be idempotent"
    );

    let outcome = provider.ensure_rule(&rule).expect("create the rule");
    assert_eq!(outcome, EnsureOutcome::Created);
    let outcome = provider
        .ensure_rule(&rule)
        .expect("a second ensure_rule must not fail");
    assert_eq!(
        outcome,
        EnsureOutcome::AlreadyPresent,
        "ensure_rule must be idempotent"
    );

    provider.commit().expect("reconfigure aliases and apply the filter");

    // Remove the RULE first, alias second — the appliance validates a
    // rule's source against the alias table on write (measured here: doing
    // this in the other order and re-proving removal with the same
    // alias-referencing rule fails with "not a valid source IP address or
    // alias", because the alias is already gone by then). A plain-CIDR
    // rule proves removal without that ordering dependency.
    provider
        .remove_rule(&rule.description)
        .expect("remove the rule");
    provider
        .remove_alias(&alias.name)
        .expect("remove the alias");
    provider.commit().expect("reconfigure and apply the removal");

    // Prove the rule was actually removed, not just unlisted: a plain-CIDR
    // rule (no alias dependency) that ensure_rule would report
    // AlreadyPresent for if the earlier removal had silently failed.
    let proof = GatewayRule {
        description: rule.description.clone(),
        source: "192.168.1.0/24".into(),
        destination: rule.destination.clone(),
        protocol: rule.protocol.clone(),
    };
    let outcome = provider
        .ensure_rule(&proof)
        .expect("recreate after removal to prove it was gone");
    assert_eq!(
        outcome,
        EnsureOutcome::Created,
        "the rule must have actually been removed, not just unlisted"
    );
    provider
        .remove_rule(&proof.description)
        .expect("remove the proof rule");
    provider.commit().expect("final commit");
}
