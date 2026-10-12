use std::collections::BTreeMap;
use std::sync::Mutex;

use super::*;

/// A driver that records what it was asked and answers from fixed values.
struct Fake {
    id: &'static str,
    needs: Privilege,
    probe_local: PoolProbe,
    calls: Mutex<Vec<String>>,
}

impl Fake {
    fn new(needs: Privilege) -> Arc<Self> {
        Arc::new(Self {
            id: "lvm-thin",
            needs,
            probe_local: PoolProbe::Available,
            calls: Mutex::new(Vec::new()),
        })
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl StoragePoolDriver for Fake {
    fn id(&self) -> &'static str {
        self.id
    }
    fn probe(&self, _: &PoolRef) -> PoolProbe {
        self.calls.lock().unwrap().push("probe".into());
        self.probe_local.clone()
    }
    fn required(&self, _: PoolOp) -> Privilege {
        self.needs
    }
    fn allocate(&self, pool: &PoolRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation> {
        self.calls.lock().unwrap().push(format!(
            "allocate {} {} {}",
            req.name,
            req.size_bytes,
            owner.id()
        ));
        Ok(Allocation {
            pool: pool.name.into(),
            volume: req.name.clone(),
            shape: req.shape,
            path: PathBuf::from("/var/lib/delonix-storage/mounts/p/v/_data"),
            adopted: false,
        })
    }
    fn release(&self, _: &PoolRef, volume: &str, owner: &Owner) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("release {volume} {}", owner.id()));
        Ok(())
    }
    fn usage(&self, _: &PoolRef) -> PoolUsage {
        self.calls.lock().unwrap().push("usage".into());
        PoolUsage::default()
    }
}

/// A helper that records the requests it got and answers with a fixed outcome.
struct FakeHelper {
    seen: Mutex<Vec<Request>>,
    answer: Result<Outcome>,
}

impl HelperCall for FakeHelper {
    fn call(&self, req: &Request) -> Result<Outcome> {
        self.seen.lock().unwrap().push(req.clone());
        match &self.answer {
            Ok(o) => Ok(o.clone()),
            Err(e) => Err(Error::from_number(e.number(), e.to_string())),
        }
    }
}

fn helper_answering(o: Result<Outcome>) -> Arc<FakeHelper> {
    Arc::new(FakeHelper {
        seen: Mutex::new(Vec::new()),
        answer: o,
    })
}

fn entry(users: &[&str], max: Option<&str>) -> Entry {
    Entry {
        driver: "lvm-thin".into(),
        vg: Some("vg0".into()),
        thin_pool: Some("dlx".into()),
        allow_users: users.iter().map(|s| s.to_string()).collect(),
        max_volume_bytes: max.map(Into::into),
        ..Default::default()
    }
}

fn list_with(e: Entry) -> Allowlist {
    let mut pools = BTreeMap::new();
    pools.insert("fast".to_string(), e);
    Allowlist { mode: None, pools }
}

fn alloc_req(vol: &str, size: u64, root: &str) -> Request {
    let mut r = Request::new("allocate", "fast");
    r.volume = Some(vol.into());
    r.shape = Some("filesystem".into());
    r.size_bytes = Some(size);
    r.root = Some(root.into());
    r
}

fn refused(r: &Request) -> String {
    let e = r.check().unwrap_err();
    assert_eq!(e.number(), 1221, "{e}");
    e.to_string()
}

#[test]
fn every_field_of_a_request_is_checked_for_its_operation() {
    assert!(alloc_req("db", 1 << 30, "/home/u/.local/share/delonix")
        .check()
        .is_ok());
    // Version, pool name.
    let mut r = Request::new("probe", "fast");
    r.v = 2;
    assert!(refused(&r).contains("version 2"));
    for bad in ["", "../etc", "Fast", "-x", "a/b"] {
        assert!(
            refused(&Request::new("probe", bad)).contains("not a pool name"),
            "{bad}"
        );
    }
    // Volume names that would leave the pool, or read as an option.
    for bad in ["../x", "-rf", ".hidden", "a/b", "a b", "x\n"] {
        assert!(
            refused(&alloc_req(bad, 1, "/r")).contains("not a volume name"),
            "{bad:?}"
        );
    }
    // Shape and size.
    let mut r = alloc_req("db", 1, "/r");
    r.shape = Some("raw".into());
    assert!(refused(&r).contains("not filesystem or block"));
    for bad in [0, MAX_SIZE_BYTES + 1] {
        assert!(
            refused(&alloc_req("db", bad, "/r")).contains("outside"),
            "{bad}"
        );
    }
    // The state root: absolute, no '..', no control characters, bounded.
    for bad in ["rel/path", "/a/../b", "/a\nb", ""] {
        assert!(
            refused(&alloc_req("db", 1, bad)).contains("`root`"),
            "{bad:?}"
        );
    }
    assert!(refused(&alloc_req("db", 1, &format!("/{}", "x".repeat(1100)))).contains("`root`"));
    // A field the operation does not take is refused, not ignored.
    let mut r = Request::new("probe", "fast");
    r.volume = Some("db".into());
    assert!(refused(&r).contains("does not take `volume`"));
    let mut r = Request::new("release", "fast");
    r.volume = Some("db".into());
    r.root = Some("/r".into());
    r.size_bytes = Some(1);
    assert!(refused(&r).contains("does not take `size_bytes`"));
    // Missing fields.
    let mut r = alloc_req("db", 1, "/r");
    r.root = None;
    assert!(refused(&r).contains("`root` is missing"));
}

#[test]
fn pool_administration_and_unknown_operations_are_never_served_over_the_socket() {
    for op in ["create-pool", "destroy-pool"] {
        let e = refused(&Request::new(op, "fast"));
        assert!(e.contains("never served over the socket"), "{e}");
    }
    let e = refused(&Request::new("grant-device", "fast"));
    assert!(e.contains("not an operation"), "{e}");
    // A control character in an echoed value is neutralised.
    let e = refused(&Request::new("x\u{1b}[2J", "fast"));
    assert!(!e.contains('\u{1b}'), "{e:?}");
}

#[test]
fn a_device_a_path_or_a_command_cannot_be_smuggled_in_a_field_the_protocol_lacks() {
    for extra in [
        r#""device":"/dev/sda""#,
        r#""path":"/etc""#,
        r#""command":"rm -rf /""#,
        r#""uid":0"#,
    ] {
        let raw = format!(r#"{{"v":1,"op":"probe","pool":"fast",{extra}}}"#);
        let e = Request::decode(raw.as_bytes()).unwrap_err();
        assert_eq!(e.number(), 1221);
        assert!(e.to_string().contains("unknown field"), "{e}");
    }
    let big = vec![b' '; MAX_REQUEST_BYTES + 1];
    assert!(Request::decode(&big)
        .unwrap_err()
        .to_string()
        .contains("at most"));
}

fn ctx_for<'a>(
    list: &'a Allowlist,
    fake: &'a Arc<Fake>,
    driver: &'a dyn Fn(&str) -> Option<Arc<dyn StoragePoolDriver>>,
) -> ServeContext<'a> {
    let _ = fake;
    ServeContext {
        allowlist: list,
        driver,
        uid_of: &|name: &str| (name == "walter").then_some(1000),
        parse_size: &|s: &str| {
            s.strip_suffix('G')
                .and_then(|n| n.parse::<u64>().ok())
                .map(|n| n << 30)
        },
    }
}

#[test]
fn the_helper_serves_only_an_allowed_uid_and_stamps_the_uid_the_kernel_reported() {
    let fake = Fake::new(Privilege::Helper);
    let f2 = fake.clone();
    let drivers = move |id: &str| -> Option<Arc<dyn StoragePoolDriver>> {
        (id == "lvm-thin").then(|| f2.clone() as Arc<dyn StoragePoolDriver>)
    };
    let list = list_with(entry(&["walter", "2000"], Some("2G")));
    let ctx = ctx_for(&list, &fake, &drivers);
    let req = alloc_req("db", 1 << 30, "/home/w/.local/share/delonix").encode();

    // A uid neither by name nor by number: refused, the driver never asked.
    let e = serve(&req, 1001, &ctx).unwrap_err();
    assert_eq!(e.number(), 7202, "{e}");
    assert!(fake.calls().is_empty());

    // By name and by number.
    assert!(serve(&req, 1000, &ctx).is_ok());
    assert!(serve(&req, 2000, &ctx).is_ok());
    // The owner is the PEER's uid, never anything the request said.
    assert_eq!(
        fake.calls(),
        vec![
            "allocate db 1073741824 1000:/home/w/.local/share/delonix".to_string(),
            "allocate db 1073741824 2000:/home/w/.local/share/delonix".to_string(),
        ]
    );
}

#[test]
fn an_empty_allow_list_serves_nobody_and_a_pool_not_declared_is_refused() {
    let fake = Fake::new(Privilege::Helper);
    let f2 = fake.clone();
    let drivers =
        move |_: &str| -> Option<Arc<dyn StoragePoolDriver>> { Some(f2.clone() as Arc<_>) };
    let list = list_with(entry(&[], None));
    let ctx = ctx_for(&list, &fake, &drivers);
    let probe = Request::new("probe", "fast").encode();
    assert_eq!(serve(&probe, 0, &ctx).unwrap_err().number(), 7202);
    assert_eq!(serve(&probe, 1000, &ctx).unwrap_err().number(), 7202);
    let other = Request::new("probe", "other").encode();
    assert_eq!(serve(&other, 1000, &ctx).unwrap_err().number(), 7201);
    assert!(fake.calls().is_empty());
}

#[test]
fn the_helper_enforces_max_volume_bytes_itself() {
    let fake = Fake::new(Privilege::Helper);
    let f2 = fake.clone();
    let drivers =
        move |_: &str| -> Option<Arc<dyn StoragePoolDriver>> { Some(f2.clone() as Arc<_>) };
    let list = list_with(entry(&["walter"], Some("2G")));
    let ctx = ctx_for(&list, &fake, &drivers);
    let e = serve(&alloc_req("db", 3 << 30, "/r").encode(), 1000, &ctx).unwrap_err();
    assert_eq!(e.number(), 7201, "{e}");
    assert!(fake.calls().is_empty());
}

#[test]
fn a_reply_keeps_the_class_of_a_refusal_across_the_socket() {
    let r = Reply::of(Err(Error::PoolExhausted("97% full".into())));
    let bytes = r.encode();
    let back: Reply = serde_json::from_slice(&bytes).unwrap();
    let e = back.into_result().unwrap_err();
    assert_eq!(e.number(), 5202);
    assert_eq!(e.to_string(), "97% full");
    let ok = Reply::of(Ok(Outcome::Probe {
        probe: PoolProbe::Undetermined {
            reason: "r".into(),
            remedy: "m".into(),
        },
    }));
    let back: Reply = serde_json::from_slice(&ok.encode()).unwrap();
    assert_eq!(back, ok);
}

fn pool_ref(e: &Entry) -> PoolRef<'_> {
    PoolRef {
        name: "fast",
        entry: e,
    }
}

#[test]
fn an_operation_that_needs_root_goes_to_the_helper_and_never_to_the_local_driver() {
    let fake = Fake::new(Privilege::Helper);
    let helper = helper_answering(Ok(Outcome::Allocation {
        allocation: Allocation {
            pool: "fast".into(),
            volume: "db".into(),
            shape: VolumeShape::Filesystem,
            path: PathBuf::from("/var/lib/delonix-storage/mounts/fast/db/_data"),
            adopted: false,
        },
    }));
    let routed = Routed::new(fake.clone(), Privileged::Helper(helper.clone()));
    let e = entry(&[], None);
    let a = routed
        .allocate(
            &pool_ref(&e),
            &VolumeRequest {
                name: "db".into(),
                shape: VolumeShape::Filesystem,
                size_bytes: 5,
            },
            &Owner::of(1000, "/home/w/root"),
        )
        .unwrap();
    assert_eq!(a.volume, "db");
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    let seen = helper.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    // The request carries the root but NOT the uid: the helper takes that
    // from the socket.
    assert_eq!(seen[0].root.as_deref(), Some("/home/w/root"));
    assert!(!String::from_utf8(seen[0].encode())
        .unwrap()
        .contains("1000"));
}

#[test]
fn an_unprivileged_operation_runs_here_and_never_asks_the_helper() {
    let fake = Fake::new(Privilege::Unprivileged);
    let helper = helper_answering(Ok(Outcome::Released));
    let routed = Routed::new(fake.clone(), Privileged::Helper(helper.clone()));
    let e = entry(&[], None);
    routed
        .release(&pool_ref(&e), "db", &Owner::of(1, "/r"))
        .unwrap();
    assert_eq!(fake.calls(), vec!["release db 1:/r".to_string()]);
    assert!(helper.seen.lock().unwrap().is_empty());
}

#[test]
fn a_delegated_probe_that_cannot_look_here_asks_the_helper() {
    let mut f = Fake::new(Privilege::Delegated("zfs allow"));
    Arc::get_mut(&mut f).unwrap().probe_local = PoolProbe::Undetermined {
        reason: "/dev/zfs: Permission denied".into(),
        remedy: "x".into(),
    };
    let helper = helper_answering(Ok(Outcome::Probe {
        probe: PoolProbe::Available,
    }));
    let routed = Routed::new(f.clone(), Privileged::Helper(helper.clone()));
    let e = entry(&[], None);
    assert_eq!(routed.probe(&pool_ref(&e)), PoolProbe::Available);
    assert_eq!(f.calls(), vec!["probe".to_string()]);
    assert_eq!(helper.seen.lock().unwrap().len(), 1);
}

#[test]
fn a_helper_that_fails_a_probe_is_could_not_determine_never_available() {
    let fake = Fake::new(Privilege::Helper);
    let helper = helper_answering(Err(Error::HelperUnavailable("down".into())));
    let routed = Routed::new(fake.clone(), Privileged::Helper(helper));
    let e = entry(&[], None);
    assert!(matches!(
        routed.probe(&pool_ref(&e)),
        PoolProbe::Undetermined { .. }
    ));
    assert_eq!(routed.usage(&pool_ref(&e)), PoolUsage::default());
    assert!(fake.calls().is_empty());
}

#[test]
fn in_process_mode_calls_the_driver_directly() {
    let fake = Fake::new(Privilege::Helper);
    let routed = Routed::new(fake.clone(), Privileged::InProcess);
    let e = entry(&[], None);
    routed
        .release(&pool_ref(&e), "db", &Owner::of(0, "/var/lib/delonix"))
        .unwrap();
    assert_eq!(
        fake.calls(),
        vec!["release db 0:/var/lib/delonix".to_string()]
    );
}

/// ADR-0067 P1 proof «without the helper, lvm-thin answers UNKNOWN with exit
/// 69, and the driver is never called» — the unit-level half of it.
#[test]
fn without_a_helper_a_root_only_driver_is_never_called_and_answers_unknown_or_69() {
    let fake = Fake::new(Privilege::Helper);
    let list = Allowlist::default();
    let routed = route(fake.clone(), &list, None);
    let e = entry(&[], None);
    assert!(matches!(
        routed.probe(&pool_ref(&e)),
        PoolProbe::Undetermined { .. }
    ));
    let err = routed
        .allocate(
            &pool_ref(&e),
            &VolumeRequest {
                name: "db".into(),
                shape: VolumeShape::Filesystem,
                size_bytes: 1,
            },
            &Owner::of(1000, "/r"),
        )
        .unwrap_err();
    assert_eq!(err.number(), 6206);
    assert_eq!(
        delonix_model::exitcode::for_error(&delonix_model::Error::from(err)),
        69
    );
    assert_eq!(routed.usage(&pool_ref(&e)), PoolUsage::default());
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

#[test]
fn the_socket_client_speaks_the_protocol_and_says_when_nobody_listens() {
    // A short path: a unix socket's name is at most 107 bytes.
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let sock = dir.path().join("h.sock");
    let client = SocketClient::new(sock.clone());
    let e = client.call(&Request::new("probe", "fast")).unwrap_err();
    assert_eq!(e.number(), 6206);
    assert!(e.to_string().contains("cannot be reached"), "{e}");

    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let server = std::thread::spawn(move || {
        let (mut c, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        c.read_to_end(&mut buf).unwrap();
        let req = Request::decode(&buf).unwrap();
        assert_eq!(req.op, "usage");
        let reply = Reply::of(Ok(Outcome::Usage {
            usage: PoolUsage {
                capacity_bytes: Some(10),
                used_bytes: Some(4),
                metadata_pct: Some(3),
            },
        }));
        c.write_all(&reply.encode()).unwrap();
    });
    let out = client.call(&Request::new("usage", "fast")).unwrap();
    server.join().unwrap();
    assert_eq!(
        out,
        Outcome::Usage {
            usage: PoolUsage {
                capacity_bytes: Some(10),
                used_bytes: Some(4),
                metadata_pct: Some(3),
            }
        }
    );
}
