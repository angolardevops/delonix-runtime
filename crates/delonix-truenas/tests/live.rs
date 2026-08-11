//! Exercises the provisioner against a REAL TrueNAS appliance.
//!
//! Skipped unless `DELONIX_TRUENAS_TEST_URL` is set, because it needs one — and
//! a test that quietly passes when its target is absent proves nothing. Run it
//! against the appliance this repo builds (`scripts/appliances/build-truenas.sh`):
//!
//! ```text
//! DELONIX_TRUENAS_TEST_URL=https://192.168.122.83 \
//! DELONIX_TRUENAS_TEST_USER=truenas_admin \
//! DELONIX_TRUENAS_TEST_PASS=delonix-admin \
//! DELONIX_TRUENAS_TEST_POOL=tank \
//!   cargo test -p delonix-truenas --test live -- --nocapture
//! ```
//!
//! It creates and then destroys `<pool>/dlxlive-<pid>`, so it never touches a
//! dataset it did not make.

use delonix_truenas::{Auth, Client, DatasetSpec, NfsShareSpec, Owner, Target};

fn target() -> Option<(Target, String)> {
    let url = std::env::var("DELONIX_TRUENAS_TEST_URL").ok()?;
    let pool = std::env::var("DELONIX_TRUENAS_TEST_POOL").unwrap_or_else(|_| "tank".into());
    let auth = match std::env::var("DELONIX_TRUENAS_TEST_KEY") {
        Ok(k) => Auth::ApiKey(k),
        Err(_) => Auth::Password {
            username: std::env::var("DELONIX_TRUENAS_TEST_USER").ok()?,
            password: std::env::var("DELONIX_TRUENAS_TEST_PASS").ok()?,
        },
    };
    Some((
        Target {
            base_url: url,
            auth,
            // The appliance serves a self-signed certificate out of the box.
            insecure_tls: true,
        },
        pool,
    ))
}

#[test]
fn provisiona_partilha_e_destroi_contra_uma_appliance_real() {
    let Some((t, pool)) = target() else {
        eprintln!("SKIP: DELONIX_TRUENAS_TEST_URL is not set");
        return;
    };
    let c = Client::connect(&t).expect("connect");
    eprintln!("connected to TrueNAS {}", c.version());

    let ds = format!("{pool}/dlxlive-{}", std::process::id());

    // 1. Create, with a quota and an owner.
    let p = c
        .ensure_dataset(&DatasetSpec {
            dataset: ds.clone(),
            quota: Some(1024 * 1024 * 1024),
            owner: Some(Owner {
                uid: 1000,
                gid: 1000,
                mode: Some(0o770),
            }),
        })
        .expect("ensure_dataset");
    eprintln!("created {} at {}", p.dataset, p.mountpoint);
    assert_eq!(p.dataset, ds);
    assert!(p.mountpoint.starts_with("/mnt/"));
    // The quota comes back from the appliance, not from the request.
    assert_eq!(
        p.quota,
        Some(1024 * 1024 * 1024),
        "the NAS must report the quota it is enforcing"
    );
    assert!(p.available.is_some(), "available space must be reported");

    // 2. Idempotent: the same call twice changes nothing and still reports the
    //    same enforced quota.
    let again = c
        .ensure_dataset(&DatasetSpec {
            dataset: ds.clone(),
            quota: Some(1024 * 1024 * 1024),
            owner: None,
        })
        .expect("second ensure_dataset");
    assert_eq!(again.quota, p.quota);
    assert_eq!(again.mountpoint, p.mountpoint);

    // 3. Changing the quota is applied, and read back changed.
    let grown = c
        .ensure_dataset(&DatasetSpec {
            dataset: ds.clone(),
            quota: Some(2 * 1024 * 1024 * 1024),
            owner: None,
        })
        .expect("grow quota");
    assert_eq!(grown.quota, Some(2 * 1024 * 1024 * 1024));

    // 4. Share it, twice — the second must update rather than duplicate.
    let id = c
        .ensure_nfs_share(
            &p.mountpoint,
            &NfsShareSpec {
                networks: vec!["192.168.122.0/24".into()],
                maproot_user: Some("root".into()),
                maproot_group: Some("root".into()),
                read_only: false,
            },
        )
        .expect("ensure_nfs_share");
    let id2 = c
        .ensure_nfs_share(&p.mountpoint, &NfsShareSpec::default())
        .expect("re-ensure");
    assert_eq!(id, id2, "re-ensuring must reuse the share, not add one");

    // 5. Destroy, and prove it is gone — both the data and its export.
    c.remove_dataset(&ds, true).expect("remove_dataset");
    assert!(
        !c.remove_nfs_share(&p.mountpoint).expect("share lookup"),
        "the export must not outlive the dataset it points at"
    );
    // Idempotent: destroying what is already gone is the desired state.
    c.remove_dataset(&ds, true).expect("second remove");
}

#[test]
fn um_dataset_invalido_e_recusado_antes_de_qualquer_pedido() {
    // No target needed: the validation is what runs first, at every call site.
    for bad in ["tank/../etc", "tank", "/tank/x", "tank/x?a=b"] {
        assert!(
            delonix_truenas::validate_dataset_name(bad).is_err(),
            "{bad}"
        );
    }
}
