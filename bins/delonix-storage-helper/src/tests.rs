//! The helper's own plumbing over a real socket pair: the caller's uid comes
//! from the kernel, a request is bounded, and every connection gets a reply.
//! The request grammar and its refusals are proven in `delonix_storage::helper`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use delonix_storage::allowlist::Entry;
use delonix_storage::helper::Request;
use delonix_storage::ownership::Owner;
use delonix_storage::pool::{
    Allocation, PoolOp, PoolProbe, PoolUsage, Privilege, VolumeRequest, VolumeShape,
};

use super::*;

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn me() -> u32 {
    // SAFETY: no preconditions.
    unsafe { libc::geteuid() }
}

#[derive(Default)]
struct Fake(Mutex<Vec<String>>);

impl StoragePoolDriver for Fake {
    fn id(&self) -> &'static str {
        "lvm-thin"
    }
    fn probe(&self, _: &PoolRef) -> PoolProbe {
        PoolProbe::Available
    }
    fn required(&self, _: PoolOp) -> Privilege {
        Privilege::Helper
    }
    fn allocate(&self, pool: &PoolRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation> {
        self.0.lock().unwrap().push(owner.id().to_string());
        Ok(Allocation {
            pool: pool.name.into(),
            volume: req.name.clone(),
            shape: req.shape,
            path: "/var/lib/delonix-storage/mounts/fast/db/_data".into(),
            adopted: false,
        })
    }
    fn release(&self, _: &PoolRef, _: &str, _: &Owner) -> Result<()> {
        Ok(())
    }
    fn usage(&self, _: &PoolRef) -> PoolUsage {
        PoolUsage::default()
    }
}

fn list(users: &[String]) -> Allowlist {
    let mut pools = BTreeMap::new();
    pools.insert(
        "fast".to_string(),
        Entry {
            driver: "lvm-thin".into(),
            vg: Some("vg0".into()),
            thin_pool: Some("dlx".into()),
            allow_users: users.to_vec(),
            ..Default::default()
        },
    );
    Allowlist { mode: None, pools }
}

fn allocate_request() -> Vec<u8> {
    let mut r = Request::new("allocate", "fast");
    r.volume = Some("db".into());
    r.shape = Some(VolumeShape::Filesystem.label().into());
    r.size_bytes = Some(1 << 30);
    r.root = Some("/home/w/.local/share/delonix".into());
    r.encode()
}

/// Sends `bytes` from one end, serves the other, returns the reply and the log.
fn exchange(bytes: &[u8], list: Result<Allowlist>, fake: &Arc<Fake>) -> (Reply, String) {
    let (mut client, mut server) = UnixStream::pair().unwrap();
    client.write_all(bytes).unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let f = fake.clone();
    let driver = move |_: &str| -> Option<Arc<dyn StoragePoolDriver>> { Some(f.clone()) };
    let line = serve_connection(&mut server, list, &driver, &|_| None);
    drop(server);
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).unwrap();
    (serde_json::from_slice(&reply).unwrap(), line)
}

#[test]
fn the_arguments_are_one_connection_or_an_administrators_command() {
    assert_eq!(parse_args(&[]).unwrap(), Command::Serve);
    assert_eq!(parse_args(&args(&["--version"])).unwrap(), Command::Version);
    assert_eq!(
        parse_args(&args(&["create-pool", "fast", "--wipe-devices"])).unwrap(),
        Command::CreatePool {
            name: "fast".into(),
            wipe: true
        }
    );
    assert_eq!(
        parse_args(&args(&["destroy-pool", "fast"])).unwrap(),
        Command::DestroyPool {
            name: "fast".into()
        }
    );
    for bad in [
        &["serve", "--socket", "/tmp/x"][..],
        &["create-pool"],
        &["destroy-pool", "a", "b"],
        &["create-pool", "fast", "--force"],
    ] {
        assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
    }
}

#[test]
fn the_owner_is_the_uid_the_kernel_reports_for_the_other_end() {
    let fake = Arc::new(Fake::default());
    let (reply, line) = exchange(&allocate_request(), Ok(list(&[me().to_string()])), &fake);
    assert!(matches!(reply, Reply::Ok { .. }), "{reply:?}");
    assert_eq!(
        *fake.0.lock().unwrap(),
        vec![format!("{}:/home/w/.local/share/delonix", me())]
    );
    assert!(
        line.starts_with(&format!("uid={} ok allocate", me())),
        "{line}"
    );
}

#[test]
fn a_caller_outside_allow_users_gets_a_reply_and_nothing_runs() {
    let fake = Arc::new(Fake::default());
    let other = (me() + 1).to_string();
    let (reply, line) = exchange(&allocate_request(), Ok(list(&[other])), &fake);
    match reply {
        Reply::Err { number, .. } => assert_eq!(number, 7202),
        other => panic!("{other:?}"),
    }
    assert!(fake.0.lock().unwrap().is_empty());
    assert!(line.contains("refused DX-7202"), "{line}");
}

#[test]
fn a_request_above_the_limit_is_refused_not_truncated() {
    let fake = Arc::new(Fake::default());
    let mut big = allocate_request();
    big.resize(MAX_REQUEST_BYTES + 1, b' ');
    let (reply, _) = exchange(&big, Ok(list(&[me().to_string()])), &fake);
    match reply {
        Reply::Err { number, .. } => assert_eq!(number, 1221),
        other => panic!("{other:?}"),
    }
    assert!(fake.0.lock().unwrap().is_empty());
}

#[test]
fn without_an_allowlist_the_caller_is_told_and_nothing_runs() {
    let fake = Arc::new(Fake::default());
    let (reply, _) = exchange(
        &allocate_request(),
        Err(Error::PoolUnavailable("no allowlist".into())),
        &fake,
    );
    assert!(
        matches!(reply, Reply::Err { number: 6203, .. }),
        "{reply:?}"
    );
    assert!(fake.0.lock().unwrap().is_empty());
}

#[test]
fn standard_input_that_is_not_a_unix_stream_socket_is_refused() {
    let f = tempfile::tempfile().unwrap();
    let e = check_socket(f.as_raw_fd()).unwrap_err();
    assert_eq!(e.number(), 6206, "{e}");
    let (a, _b) = UnixStream::pair().unwrap();
    check_socket(a.as_raw_fd()).unwrap();
    let d = std::os::unix::net::UnixDatagram::unbound().unwrap();
    assert!(check_socket(d.as_raw_fd()).is_err());
}

#[test]
fn the_administrators_commands_need_root() {
    if me() == 0 {
        return;
    }
    for create in [Some(false), None] {
        let e = admin_command("fast", create).unwrap_err();
        assert_eq!(e.number(), 7203, "{e}");
    }
}

#[test]
fn the_helper_runs_the_privileged_drivers_and_not_dir() {
    for id in ["lvm-thin", "zfs", "btrfs"] {
        assert_eq!(driver_for(id).unwrap().id(), id);
        assert_eq!(admin_for(id).unwrap().id(), id);
    }
    assert!(driver_for("dir").is_none());
    assert!(admin_for("dir").is_none());
}

/// The units shipped in `dist/` are the ones this binary assumes: one
/// connection on standard input, the group-only socket at the path the
/// engine dials, and no mount namespace that would hide the helper's mounts.
#[test]
fn the_shipped_units_hand_one_connection_to_one_process() {
    let socket = include_str!("../../../dist/delonix-storage-helper.socket");
    let service = include_str!("../../../dist/delonix-storage-helper@.service");
    let has = |text: &str, line: &str| text.lines().any(|l| l.trim() == line);
    assert!(has(
        socket,
        &format!("ListenStream={}", helper::DEFAULT_SOCKET)
    ));
    for line in [
        "Accept=yes",
        "SocketMode=0660",
        "SocketGroup=delonix-storage",
    ] {
        assert!(has(socket, line), "{line}");
    }
    for line in [
        "StandardInput=socket",
        "NoNewPrivileges=yes",
        "ExecStart=/usr/local/sbin/delonix-storage-helper",
    ] {
        assert!(has(service, line), "{line}");
    }
    let directives: Vec<&str> = service
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();
    for ns in [
        "ProtectSystem=",
        "ProtectHome=",
        "PrivateTmp=",
        "PrivateDevices=",
        "MountFlags=",
        "ReadWritePaths=",
    ] {
        assert!(
            !directives.iter().any(|l| l.starts_with(ns)),
            "{ns} would put the helper's mounts in a private namespace"
        );
    }
    assert!(!directives.contains(&"[Install]"));
}
