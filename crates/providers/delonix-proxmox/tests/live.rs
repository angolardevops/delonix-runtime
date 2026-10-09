//! Exercises the backend against a REAL Proxmox VE node.
//!
//! Skipped unless `DELONIX_PROXMOX_TEST_URL` is set, because it needs one — and
//! a test that quietly passes when its target is absent proves nothing:
//!
//! ```text
//! DELONIX_PROXMOX_TEST_URL=https://192.168.122.54:8006 \
//! DELONIX_PROXMOX_TEST_NODE=pve \
//! DELONIX_PROXMOX_TEST_USER=root@pam \
//! DELONIX_PROXMOX_TEST_PASS=delonix-admin \
//! DELONIX_PROXMOX_TRACE_ROUTES=/path/to/trace.routes \
//!   cargo test -p delonix-proxmox --test live -- --nocapture --test-threads=1
//! ```
//!
//! An API token can stand in for the account: `DELONIX_PROXMOX_TEST_TOKEN_FILE`
//! names a file holding `<user>@<realm>!<tokenid>=<secret>` on one line, and
//! then `DELONIX_PROXMOX_TEST_USER`/`_PASS` are not read. The secret stays in
//! the file, out of the environment and the shell history.
//!
//! It creates one VM, walks it through snapshot, rollback, stop, resume and
//! destroy, and leaves the node's VM list as it found it. The trace file is the
//! numerator of the coverage matrix: `scripts/proxmox_api_inventory.py --trace`
//! promotes every route it records to `supported+tested` (ADR-0049 D2), and the
//! committed `docs/proxmox/trace-<ver>.routes` is one such run.

use delonix_compute::vm_backend::{CreateStage, VmBackend, VmConfig};
use delonix_proxmox::{AgentExecStatus, Auth, ProxmoxBackend, Target};

/// The backend over a client that honours the route trace: with
/// `DELONIX_PROXMOX_TRACE_ROUTES=<file>` every request of this run lands in
/// the file, which is what promotes a route to `supported+tested` in the
/// coverage matrix (`scripts/proxmox_api_inventory.py --trace`, ADR-0049).
fn backend(t: &Target) -> Result<ProxmoxBackend, delonix_proxmox::Error> {
    let opts = delonix_proxmox::ClientOptions {
        trace_routes: std::env::var_os(delonix_proxmox::TRACE_ROUTES_ENV)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from),
        ..Default::default()
    };
    let client = delonix_proxmox::Client::connect_with(t, opts)?;
    Ok(ProxmoxBackend::sharing(std::sync::Arc::new(client)))
}

/// The client's own `tracing` on stderr when `DELONIX_LOG` is set (for
/// example `DELONIX_LOG=warn`): a task's warnings are logged there, not
/// returned, so this is how a live run shows them.
fn init_log() {
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_env("DELONIX_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
    }
}

fn target() -> Option<Target> {
    init_log();
    Some(Target {
        base_url: std::env::var("DELONIX_PROXMOX_TEST_URL").ok()?,
        node: std::env::var("DELONIX_PROXMOX_TEST_NODE").unwrap_or_else(|_| "pve".into()),
        auth: auth_from_env()?,
        insecure_tls: true,
        bridge: None,
        vlan: None,
        ca_cert_pem: None,
        import_storage: None,
        disk_storage: None,
    })
}

/// A token from `DELONIX_PROXMOX_TEST_TOKEN_FILE`, else one from
/// `DELONIX_PROXMOX_TEST_TOKEN_ID`/`_TOKEN`, else the account from
/// `DELONIX_PROXMOX_TEST_USER`/`_PASS`.
fn auth_from_env() -> Option<Auth> {
    if let Some(path) = std::env::var_os("DELONIX_PROXMOX_TEST_TOKEN_FILE") {
        let text = std::fs::read_to_string(path).ok()?;
        let (id, secret) = text.trim().split_once('=')?;
        return Some(Auth::ApiToken {
            id: id.to_string(),
            secret: secret.to_string(),
        });
    }
    // Or an API token (`DELONIX_PROXMOX_TEST_TOKEN_ID` + `_TOKEN`) — revocable
    // on the node, and the form a lab run can mint without anybody's password.
    if let (Ok(id), Ok(secret)) = (
        std::env::var("DELONIX_PROXMOX_TEST_TOKEN_ID"),
        std::env::var("DELONIX_PROXMOX_TEST_TOKEN"),
    ) {
        return Some(Auth::ApiToken { id, secret });
    }
    Some(Auth::Password {
        username: std::env::var("DELONIX_PROXMOX_TEST_USER").ok()?,
        password: std::env::var("DELONIX_PROXMOX_TEST_PASS").ok()?,
    })
}

#[test]
fn cria_arranca_e_destroi_contra_um_no_real() {
    let Some(t) = target() else {
        eprintln!("SKIP: DELONIX_PROXMOX_TEST_URL is not set");
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    // The VM's directory on this side: the task ledger lands in
    // `<vmdir>/proxmox-tasks.json`, and a shared `/tmp` handed one run's ledger
    // to the next (a leftover `submitted` entry is WAITED on before operating).
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();

    let name = format!("dlxlive{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };

    let boot = b
        .boot(vmdir, &cfg, &cfg.disk, &|s: CreateStage| {
            eprintln!("  stage: {s:?}")
        })
        .expect("boot");
    eprintln!("created and started: handle={}", boot.api_socket);
    assert!(
        boot.api_socket.starts_with("proxmox:"),
        "the handle is what every later call addresses: {}",
        boot.api_socket
    );

    // The record the engine would keep.
    let vm = delonix_compute::Vm::new(
        name.clone(),
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );

    assert!(b.is_running(&vm), "the VM must be running after boot");

    // The agent CHANNEL has to be on the VM this backend created: without it
    // the node never even tries, and `ip()` could not work no matter what the
    // guest has installed.
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let node_cfg = b.client().config(vmid).expect("read the config back");
    assert!(
        node_cfg.get("agent").is_some(),
        "the VM was created without the guest-agent channel: {node_cfg}"
    );

    // And `ip()` on a guest with no agent answers None — quietly. This is the
    // ORDINARY case (a plain cloud image has no agent), and `vm ls` calls it
    // for every VM on every listing, so it must not be an error.
    assert_eq!(
        b.ip(&vm),
        None,
        "a guest with no agent must yield no address, not a failure"
    );

    // Snapshot of a RUNNING VM: `vmstate=1`, so the node has to save RAM and
    // the worker is `qmsnapshot` — until this run only the TLS mock had ever
    // answered to that name. The list is read back from the node, never from
    // what the call said it did.
    b.snapshot(vmdir, &vm, "live1").expect("snapshot");
    let listed = b.snapshots(vmdir, &vm).expect("snapshots");
    assert!(
        listed.iter().any(|s| s == "live1"),
        "the snapshot is not in the node's list: {listed:?}"
    );
    assert!(
        !listed.iter().any(|s| s == "current"),
        "`current` is the node's pseudo-entry for the live state, not a snapshot: {listed:?}"
    );
    // A second snapshot by the same name is a conflict the CLIENT refuses
    // before the node sees a request — exit class 5, not a raw node error.
    let again = b.snapshot(vmdir, &vm, "live1");
    assert!(
        again.is_err(),
        "a snapshot name already taken was accepted twice"
    );

    // Rollback to a snapshot that carries RAM leaves the guest RUNNING (the
    // node resumes it), so `is_running` is the observable that says the
    // rollback happened and did not just stop the machine.
    b.restore(vmdir, &vm, "live1").expect("rollback");
    assert!(
        b.is_running(&vm),
        "after a rollback to a RAM snapshot the VM must be running again"
    );
    assert!(
        b.restore(vmdir, &vm, "nope").is_err(),
        "a rollback to a snapshot the VM does not have must be refused"
    );

    // Delete the snapshot — the fourth verb of the quadrant, and the one this
    // backend did not have (`vm snapshot rm` fell through to "not supported"
    // while the other three worked). The proof is the node's list, not the
    // call's answer; and a name the VM does not have is refused before any
    // request, as on libvirt.
    b.delete_snapshot(vmdir, &vm, "live1")
        .expect("delete snapshot");
    let listed = b.snapshots(vmdir, &vm).expect("snapshots after delete");
    assert!(
        !listed.iter().any(|s| s == "live1"),
        "the snapshot is still in the node's list after delete: {listed:?}"
    );
    assert!(
        b.delete_snapshot(vmdir, &vm, "live1").is_err(),
        "deleting a snapshot the VM no longer has must be refused"
    );

    // Só parar: o `stop` PÁRA e não remove, desde que os dois verbos foram
    // separados (era o `vm stop` a apagar o disco). O domínio continua definido.
    b.stop(vmdir, &vm).expect("stop");
    assert!(
        !b.is_running(&vm),
        "a VM tem de ficar parada depois do stop"
    );
    assert!(
        b.client().config(vmid).is_ok(),
        "o `stop` REMOVEU a VM — parar e destruir são verbos diferentes"
    );

    // `resume` is the path `vm start` takes for a record with a handle: it
    // starts the vmid the record names instead of creating a second VM (the
    // ADR-0008 orphan). It answers `Some` because it acted, and the handle it
    // returns is the one the record already had.
    let resumed = b
        .resume(vmdir, &vm)
        .expect("resume")
        .expect("a record with a handle on an existing VM must be resumed, not recreated");
    assert_eq!(
        resumed.api_socket, vm.api_socket,
        "resume changed the handle"
    );
    assert!(b.is_running(&vm), "the VM must be running after resume");
    b.stop(vmdir, &vm).expect("second stop");
    assert!(!b.is_running(&vm));

    // The task ledger is durable state, not process memory: every UPID this
    // run submitted is on disk, and none is left `submitted` or `timedout` — a
    // leftover would make the next operation on this VM wait for it first.
    //
    // `failed` entries are LEGITIMATE here, and the first version of this
    // assertion (every entry `ok`) was wrong. Measured against PVE 9.2.2: a
    // `stop` submitted within ~30 s of a `start` or a RAM `rollback` fails on
    // the node with «can't lock file '/var/lock/qemu-server/lock-<vmid>.conf' -
    // got timeout» (the node's own 10 s lock timeout), and the client's lock
    // retry resubmits it — two logical stops took five `qmstop` tasks, four of
    // them failed on the node. The ledger records what the node said, task by
    // task; what has to hold is that the LAST task of each action succeeded.
    let ledger_path = vmdir.join("proxmox-tasks.json");
    let ledger: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&ledger_path).expect("the ledger was written next to the VM"),
    )
    .expect("the ledger is JSON");
    let entries = ledger.as_array().expect("the ledger is a list");
    let state_of = |e: &serde_json::Value| -> String {
        e.pointer("/state/state")
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string()
    };
    let unsettled: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| !matches!(state_of(e).as_str(), "ok" | "failed"))
        .collect();
    assert!(
        unsettled.is_empty(),
        "tasks left unsettled in the ledger: {unsettled:?}"
    );
    for want in [
        "create",
        "start",
        "snapshot",
        "rollback",
        "delete-snapshot",
        "stop",
    ] {
        let last = entries
            .iter()
            .rev()
            .find(|e| e.get("action").and_then(|a| a.as_str()) == Some(want))
            .unwrap_or_else(|| panic!("no `{want}` task in the ledger: {entries:?}"));
        assert_eq!(
            state_of(last),
            "ok",
            "the last `{want}` task did not succeed: {last}"
        );
    }
    let lock_failures = entries
        .iter()
        .filter(|e| {
            state_of(e) == "failed"
                && e.pointer("/state/reason")
                    .and_then(|r| r.as_str())
                    .is_some_and(|r| r.contains("can't lock file"))
        })
        .count();
    let other_failures = entries.iter().filter(|e| state_of(e) == "failed").count() - lock_failures;
    assert_eq!(
        other_failures, 0,
        "a task failed for a reason other than the node's lock: {entries:?}"
    );

    // E agora destruir, provando que o nó deixou de a ter: uma VM deixada para
    // trás depois de um `delonix vm rm` é um órfão que ninguém procura.
    //
    // A asserção é a CONFIG deixar de existir, e não `!is_running`, que era o
    // que estava aqui: `is_running` é falso para uma VM destruída E para uma
    // apenas parada, por isso não conseguia distinguir as duas — exactamente o
    // que este bloco diz que prova. Com o `stop` a deixar de destruir, o teste
    // passou a deixar a VM no nó e a passar na mesma; medido, duas corridas
    // deixaram dois órfãos (`dlxlive*`, stopped) no nó real.
    b.destroy(vmdir, &vm).expect("destroy");
    assert!(
        b.client().config(vmid).is_err(),
        "a VM continua definida no nó depois do destroy — é um órfão"
    );

    // A record this backend did not create is refused rather than acted on.
    let mut alien = vm.clone();
    alien.api_socket = "/run/some.sock".into();
    assert!(
        b.stop(vmdir, &alien).is_err(),
        "a record with no Proxmox handle must be refused"
    );
}

/// A backup lands on the storage as a crash-consistent archive, and comes
/// OFF it: `backup_vm` (`POST …/vzdump`) never stops the VM, `list_backups`
/// reads back what the node actually has (never what the call said), and
/// `delete_backup` removes it — the fourth verb this backend's backup
/// primitive needs, proved the same way the snapshot quadrant was.
///
/// The VM stays RUNNING through the whole backup — that is the assertion
/// that matters, because a VM this backend cannot copy a disk FOR locally
/// (`manages_own_storage`) has no other honest way to prove "nothing had to
/// stop".
#[test]
fn a_backup_lands_on_the_storage_and_comes_off_it() {
    // No SKIP line: a print in a library crate's tests is counted debt, and
    // the sibling cases already say it.
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let name = format!("dlxbkp{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b
        .boot(vmdir, &cfg, &cfg.disk, &|_: CreateStage| {})
        .expect("boot");
    let vm = delonix_compute::Vm::new(
        name.clone(),
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        String::new(),
        String::new(),
        boot.api_socket.clone(),
    );
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();

    // The archive storage — not necessarily the same as the VM's disk
    // storage, but the lab node has only the one, so it doubles as both here.
    let backup_storage =
        std::env::var("DELONIX_PROXMOX_TEST_BACKUP_STORAGE").unwrap_or_else(|_| storage.clone());

    let before = b
        .client()
        .list_backups(&backup_storage, vmid)
        .expect("list before");
    assert!(
        before.is_empty(),
        "a freshly created VM already has a backup: {before:?}"
    );

    let ledger = delonix_proxmox::Ledger::at(vmdir);
    b.client()
        .backup_vm(&ledger, vmid, &backup_storage)
        .expect("backup");
    assert!(
        b.is_running(&vm),
        "the VM must still be running after a snapshot-mode backup"
    );

    let after = b
        .client()
        .list_backups(&backup_storage, vmid)
        .expect("list after");
    assert_eq!(
        after.len(),
        1,
        "expected exactly one backup after one call: {after:?}"
    );
    let (volid, size) = &after[0];
    assert!(
        volid.contains(&vmid.to_string()),
        "the volid does not name this VM: {volid}"
    );
    assert!(
        *size > 0,
        "a backup archive of 0 bytes is not a backup: {after:?}"
    );

    // A second backup adds a second archive — `remove=0` keeps the first.
    b.client()
        .backup_vm(&ledger, vmid, &backup_storage)
        .expect("second backup");
    let two = b
        .client()
        .list_backups(&backup_storage, vmid)
        .expect("list after second");
    assert_eq!(
        two.len(),
        2,
        "the first backup did not survive a second one: {two:?}"
    );

    // Delete both — the proof is the node's list, not the call's answer.
    for (volid, _) in &two {
        b.client()
            .delete_backup(&ledger, vmid, &backup_storage, volid)
            .unwrap_or_else(|e| panic!("delete {volid}: {e}"));
    }
    let gone = b
        .client()
        .list_backups(&backup_storage, vmid)
        .expect("list after delete");
    assert!(
        gone.is_empty(),
        "backups are still on the storage after delete: {gone:?}"
    );

    // A volid this VM's storage does not have is refused, not silently no-op'd.
    assert!(
        b.client()
            .delete_backup(&ledger, vmid, &backup_storage, "does-not-exist")
            .is_err(),
        "deleting a backup that was never there must be refused"
    );

    b.stop(vmdir, &vm).expect("stop");
    b.destroy(vmdir, &vm).expect("destroy");
    assert!(
        b.client().config(vmid).is_err(),
        "the VM is still on the node after destroy"
    );

    let ledger: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(vmdir.join("proxmox-tasks.json")).expect("ledger"),
    )
    .expect("json");
    let entries = ledger.as_array().unwrap();
    for want in ["backup", "delete-backup"] {
        let count = entries
            .iter()
            .filter(|e| e.get("action").and_then(|a| a.as_str()) == Some(want))
            .count();
        assert!(count >= 1, "no `{want}` task in the ledger: {entries:?}");
        assert!(
            entries
                .iter()
                .rev()
                .find(|e| e.get("action").and_then(|a| a.as_str()) == Some(want))
                .and_then(|e| e.pointer("/state/state"))
                .and_then(|s| s.as_str())
                == Some("ok"),
            "the last `{want}` task did not succeed: {entries:?}"
        );
    }
}

/// A VM destroyed after being backed up comes back from that same backup —
/// `restore_vm` (`POST …/qemu` with `archive=`), the other half of the
/// pair `backup_vm` started: a backup nobody can restore from is not a
/// backup, it is a write nobody ever reads back.
///
/// Restores into the vmid the original VM just vacated — the ordinary "I
/// deleted it and want it back" story — and proves the restored VM is the
/// SAME shape (same boot disk size) as what was destroyed, read from the
/// node's own config, never from what the call said.
#[test]
fn a_deleted_vm_comes_back_from_its_own_backup() {
    // No SKIP line: a print in a library crate's tests is counted debt, and
    // the sibling cases already say it.
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let backup_storage =
        std::env::var("DELONIX_PROXMOX_TEST_BACKUP_STORAGE").unwrap_or_else(|_| storage.clone());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let name = format!("dlxrst{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b
        .boot(vmdir, &cfg, &cfg.disk, &|_: CreateStage| {})
        .expect("boot");
    let vm = delonix_compute::Vm::new(
        name.clone(),
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        String::new(),
        String::new(),
        boot.api_socket.clone(),
    );
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();

    let ledger = delonix_proxmox::Ledger::at(vmdir);
    b.client()
        .backup_vm(&ledger, vmid, &backup_storage)
        .expect("backup");
    let backups = b
        .client()
        .list_backups(&backup_storage, vmid)
        .expect("list");
    assert_eq!(backups.len(), 1, "expected one backup: {backups:?}");
    let (archive, _) = &backups[0];

    // Destroy the original — the archive outlives it.
    b.stop(vmdir, &vm).expect("stop");
    b.destroy(vmdir, &vm).expect("destroy");
    assert!(
        b.client().config(vmid).is_err(),
        "the VM is still on the node after destroy"
    );

    b.client()
        .restore_vm(&ledger, vmid, archive, &storage)
        .expect("restore");
    assert!(
        b.client().vm_exists(vmid).expect("vm_exists after restore"),
        "the node has no VM {vmid} after restoring {archive}"
    );
    let (key, bytes) = b.client().boot_disk(vmid).expect("restored boot disk");
    assert_eq!(
        bytes,
        1 << 30,
        "the restored `{key}` is not the 1 GiB the original was created with"
    );

    // Restoring into a vmid that is NOT free is refused, never a silent
    // overwrite — this call never sets `force`.
    assert!(
        b.client()
            .restore_vm(&ledger, vmid, archive, &storage)
            .is_err(),
        "restoring over a VM that already exists must be refused"
    );

    b.client()
        .delete_backup(&ledger, vmid, &backup_storage, archive)
        .expect("delete backup");
    let restored_vm = delonix_compute::Vm::new(
        name,
        cfg.disk.clone(),
        cfg.disk,
        1,
        "512M".into(),
        String::new(),
        String::new(),
        String::new(),
        format!("proxmox:{}:{vmid}", t.node),
    );
    b.stop(vmdir, &restored_vm).expect("stop the restored VM");
    b.destroy(vmdir, &restored_vm)
        .expect("destroy the restored VM");
    assert!(
        b.client().config(vmid).is_err(),
        "the restored VM is still on the node after destroy"
    );

    let ledger: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(vmdir.join("proxmox-tasks.json")).expect("ledger"),
    )
    .expect("json");
    let entries = ledger.as_array().unwrap();
    let count = entries
        .iter()
        .filter(|e| e.get("action").and_then(|a| a.as_str()) == Some("restore"))
        .count();
    assert!(count >= 1, "no `restore` task in the ledger: {entries:?}");
    assert!(
        entries
            .iter()
            .rev()
            .find(|e| e.get("action").and_then(|a| a.as_str()) == Some("restore"))
            .and_then(|e| e.pointer("/state/state"))
            .and_then(|s| s.as_str())
            == Some("ok"),
        "the last `restore` task did not succeed: {entries:?}"
    );
}

/// `ip()` ATRAVÉS do backend, contra um convidado com o agente REAL a correr.
///
/// O teste acima cria uma VM sem sistema operativo, logo sem agente: ali `ip()`
/// devolver `None` é o comportamento certo e não prova nada sobre o caminho
/// feliz — que era, até aqui, o único por exercitar do trait inteiro.
///
/// Aponta para uma VM preparada à parte (`DELONIX_PROXMOX_TEST_AGENT_VMID`) com
/// o `qemu-guest-agent` instalado e DUAS NICs, porque é o segundo NIC que
/// levanta a única pergunta que uma NIC só não levanta: qual dos endereços sai.
///
/// Como se prepara uma, se for preciso repetir: arrancar a appliance
/// `proxmox-ve:9.1` deste repo sobre um overlay, importar uma cloud image
/// (`download-url` com `content=import` + `qm set --scsi0 …,import-from=…`),
/// dar-lhe `--net0`/`--net1` e um drive de cloud-init, e lá dentro
/// `apt install qemu-guest-agent`.
#[test]
fn o_ip_vem_do_agente_de_um_convidado_a_serio() {
    let Some(t) = target() else {
        eprintln!("SKIP: DELONIX_PROXMOX_TEST_URL is not set");
        return;
    };
    let Ok(vmid) = std::env::var("DELONIX_PROXMOX_TEST_AGENT_VMID") else {
        eprintln!("SKIP: DELONIX_PROXMOX_TEST_AGENT_VMID is not set");
        return;
    };
    let b = backend(&t).expect("connect");
    let vm = delonix_compute::Vm::new(
        "agenttest".into(),
        String::new(),
        String::new(),
        1,
        "2G".into(),
        String::new(),
        String::new(),
        String::new(),
        format!("proxmox:{}:{vmid}", t.node),
    );
    let ip = b
        .ip(&vm)
        .expect("o agente está vivo — `ip()` tinha de devolver um endereço");
    eprintln!("  ip() = {ip}");
    assert!(!ip.starts_with("127."), "loopback: {ip}");
    assert!(
        !ip.starts_with("169.254."),
        "link-local (DHCP falhado): {ip}"
    );
    assert!(ip.parse::<std::net::Ipv4Addr>().is_ok(), "não é IPv4: {ip}");
}

/// `agent_ping` against a guest that has none — the ordinary case, and the
/// one every throwaway VM in this suite already is (no OS, so no agent can
/// possibly answer). Proven against a REAL node and not only the TLS mock:
/// `Ok(false)`, never an `Err`, matching the same "no agent is not a
/// failure" rule `ip()` already established for this backend.
#[test]
fn agent_ping_answers_false_without_an_agent() {
    // No SKIP line: a print in a library crate's tests is counted debt, and
    // the sibling cases already say it.
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let name = format!("dlxping{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b
        .boot(vmdir, &cfg, &cfg.disk, &|_: CreateStage| {})
        .expect("boot");
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let vm = delonix_compute::Vm::new(
        name,
        cfg.disk.clone(),
        cfg.disk,
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );

    assert!(
        !b.client()
            .agent_ping(vmid)
            .expect("agent_ping must not error"),
        "a guest with no agent must yield Ok(false), never an Err"
    );

    b.stop(vmdir, &vm).expect("stop");
    b.destroy(vmdir, &vm).expect("destroy");
}

/// `agent_ping`/`agent_exec`/`agent_exec_status` against the SAME prepared
/// guest the sibling case just above needs (`DELONIX_PROXMOX_TEST_AGENT_VMID`)
/// — the one VM in this suite already known to run a real `qemu-guest-agent`.
/// That case proves the agent answers a QUERY; this one proves it runs a
/// COMMAND and reports back a real exit code and stdout, through
/// `agent_exec_wait`'s poll loop and not just the pure translation the unit
/// tests already cover.
#[test]
fn agent_exec_wait_runs_a_real_command_in_the_guest() {
    // No SKIP line: a print in a library crate's tests is counted debt, and
    // the sibling case already says it.
    let Some(t) = target() else {
        return;
    };
    let Ok(vmid) = std::env::var("DELONIX_PROXMOX_TEST_AGENT_VMID") else {
        return;
    };
    let vmid: u32 = vmid
        .parse()
        .expect("DELONIX_PROXMOX_TEST_AGENT_VMID is not a number");
    let b = backend(&t).expect("connect");
    let client = b.client();

    assert!(
        client.agent_ping(vmid).expect("agent ping"),
        "the prepared guest must have a real agent running"
    );

    let outcome = client
        .agent_exec_wait(
            vmid,
            &["/bin/cat", "/etc/hostname"],
            std::time::Duration::from_secs(30),
        )
        .expect("agent exec");
    let AgentExecStatus::Finished {
        exit_code,
        stdout,
        stderr,
        signal,
    } = outcome
    else {
        panic!("agent_exec_wait reported Running past its own deadline");
    };
    assert_eq!(exit_code, 0, "stderr: {stderr}");
    assert!(signal.is_none(), "killed by a signal: {signal:?}");
    assert!(!stdout.trim().is_empty(), "/etc/hostname read back empty");
}

/// A clone of a template comes up with the DISK SIZE asked for, not the
/// template's — `diskSize` (`VmConfig.disk_size_gib`) was neither read nor
/// refused by this backend before this case, the ADR-0044 D1 class.
///
/// The case makes its own clone source: a fresh 1 GiB VM, stopped and turned
/// into a template (`POST …/template`). That is also what promotes `clone`
/// and `POST …/config` from `supported+untested` — until this run the only
/// route trace had no template on the node to clone.
///
/// What is asserted is read back from the node (`GET …/config`, the boot
/// disk's `size=`), never taken from the call's answer; and a SHRINK is
/// refused before anything exists on the node, which the VM count proves.
#[test]
fn a_template_clone_gets_the_disk_size_asked_for() {
    // No SKIP line: a print in a library crate's tests is counted debt, and
    // the sibling cases already say it.
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let stage = |_: CreateStage| {};
    let record = |name: &str, disk: &str, handle: &str| {
        delonix_compute::Vm::new(
            name.to_string(),
            disk.to_string(),
            disk.to_string(),
            1,
            "512M".into(),
            String::new(),
            String::new(),
            String::new(),
            handle.to_string(),
        )
    };
    let vmid_of = |handle: &str| -> u32 { handle.rsplit(':').next().unwrap().parse().unwrap() };

    // The clone source: a fresh 1 GiB disk, stopped, marked as a template.
    let src_name = format!("dlxtpl{}", std::process::id() % 10000);
    let src_cfg = VmConfig {
        name: src_name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let src_boot = b
        .boot(vmdir, &src_cfg, &src_cfg.disk, &stage)
        .expect("boot the source");
    let src = record(&src_name, &src_cfg.disk, &src_boot.api_socket);
    let src_id = vmid_of(&src_boot.api_socket);
    b.stop(vmdir, &src).expect("stop the source");
    b.client()
        .mark_template(&delonix_proxmox::Ledger::at(vmdir), src_id)
        .expect("template");
    let tpl_cfg = b.client().config(src_id).expect("template config");
    assert_eq!(
        tpl_cfg.get("template").and_then(|t| t.as_u64()),
        Some(1),
        "the node does not flag the source as a template: {tpl_cfg}"
    );
    let (key, tpl_bytes) = b
        .client()
        .boot_disk(src_id)
        .expect("the template's boot disk");
    assert_eq!(
        tpl_bytes,
        1 << 30,
        "the source was created with a 1 GiB `{key}`"
    );

    // A shrink is refused BEFORE a clone exists: the next free id is the same
    // after the refusal as before it.
    let vms_before = b.client().next_vmid().ok();
    let shrink_cfg = VmConfig {
        name: format!("{src_name}s"),
        disk: format!("template:{src_id}"),
        disk_size_gib: None,
        ..src_cfg.clone()
    };
    // Grow the template itself to 2 GiB first, so that asking a clone for 1
    // GiB is a genuine shrink.
    b.client()
        .resize_disk(&delonix_proxmox::Ledger::at(vmdir), src_id, &key, 2)
        .expect("grow the template to 2 GiB");
    let (_, tpl_bytes) = b.client().boot_disk(src_id).expect("re-read");
    assert_eq!(
        tpl_bytes,
        2 << 30,
        "the template's `{key}` is not 2 GiB after the resize"
    );
    let shrink = b.boot(
        vmdir,
        &VmConfig {
            disk_size_gib: Some(1),
            ..shrink_cfg.clone()
        },
        &shrink_cfg.disk,
        &stage,
    );
    let Err(e) = shrink else {
        panic!("a 1 GiB clone of a 2 GiB template must be refused");
    };
    let msg = e.to_string();
    assert!(
        msg.contains("smaller")
            && msg.contains(&key)
            && msg.contains("2 GiB")
            && msg.contains("1 GiB"),
        "the refusal must name the disk and both sizes: {msg}"
    );
    assert_eq!(
        b.client().next_vmid().ok(),
        vms_before,
        "the refused clone left a VM on the node"
    );

    // The clone, asked for 4 GiB: the node has to show 4 GiB on the boot disk.
    let clone_name = format!("{src_name}c");
    let clone_cfg = VmConfig {
        name: clone_name.clone(),
        disk: format!("template:{src_id}"),
        disk_size_gib: Some(4),
        ..src_cfg.clone()
    };
    let clone_boot = b
        .boot(vmdir, &clone_cfg, &clone_cfg.disk, &stage)
        .expect("boot the clone");
    let clone = record(&clone_name, &clone_cfg.disk, &clone_boot.api_socket);
    let clone_id = vmid_of(&clone_boot.api_socket);
    assert_ne!(clone_id, src_id);
    assert!(b.is_running(&clone), "the clone must be running after boot");
    let (clone_key, clone_bytes) = b
        .client()
        .boot_disk(clone_id)
        .expect("the clone's boot disk");
    assert_eq!(
        clone_key, key,
        "the clone's boot disk is not the template's"
    );
    assert_eq!(
        clone_bytes,
        4 << 30,
        "the clone's `{clone_key}` is not the 4 GiB asked for"
    );
    let clone_cfg_node = b.client().config(clone_id).expect("clone config");
    assert_eq!(
        clone_cfg_node.get("template").and_then(|t| t.as_u64()),
        None,
        "the clone must not itself be a template: {clone_cfg_node}"
    );

    // Same size as the template: no resize task at all — the ledger says so.
    let same_name = format!("{src_name}e");
    let same_cfg = VmConfig {
        name: same_name.clone(),
        disk: format!("template:{src_id}"),
        disk_size_gib: Some(2),
        ..src_cfg.clone()
    };
    let same_dir = tempfile::tempdir().expect("tempdir");
    let same_boot = b
        .boot(same_dir.path(), &same_cfg, &same_cfg.disk, &stage)
        .expect("boot the same-size clone");
    let same = record(&same_name, &same_cfg.disk, &same_boot.api_socket);
    let same_ledger: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(same_dir.path().join("proxmox-tasks.json")).expect("ledger"),
    )
    .expect("json");
    assert!(
        !same_ledger
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.get("action").and_then(|a| a.as_str()) == Some("resize")),
        "a clone at the template's size submitted a resize: {same_ledger}"
    );

    // Tear down: clones, then the template. The proof is the node's answer.
    for (vm, id) in [(&clone, clone_id), (&same, vmid_of(&same_boot.api_socket))] {
        b.stop(vmdir, vm).expect("stop clone");
        b.destroy(vmdir, vm).expect("destroy clone");
        assert!(
            b.client().config(id).is_err(),
            "clone {id} is still on the node"
        );
    }
    b.destroy(vmdir, &src).expect("destroy the template");
    assert!(
        b.client().config(src_id).is_err(),
        "the template is still on the node"
    );

    // The ledger settled every task, and the last `resize`/`template` succeeded.
    let ledger: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(vmdir.join("proxmox-tasks.json")).expect("ledger"),
    )
    .expect("json");
    let entries = ledger.as_array().unwrap();
    for want in ["template", "resize", "clone", "configure"] {
        let last = entries
            .iter()
            .rev()
            .find(|e| e.get("action").and_then(|a| a.as_str()) == Some(want))
            .unwrap_or_else(|| panic!("no `{want}` task in the ledger"));
        assert_eq!(
            last.pointer("/state/state").and_then(|s| s.as_str()),
            Some("ok"),
            "the last `{want}` task did not succeed: {last}"
        );
    }
}

/// `move_disk`/`unlink`/`cloudinit_dump` — the client-level primitives this
/// backend exposes BEYOND what `boot`/`config` already cover: moving a disk
/// to another storage (or re-formatting it in place), detaching one from a
/// VM, and reading back the RENDERED cloud-init file the node would inject —
/// as opposed to `config`/`configure_clone`, which only read or write the
/// cloud-init INPUTS.
///
/// The VM is stopped before any of the three. Neither `move_disk` nor
/// `unlink` needs it running, and testing them against a stopped VM keeps
/// this case independent of the node's config-lock contention window
/// (`Client::task`'s doc comment) that a `start` opens for about 30 s.
///
/// **`move_disk` moves to a SECOND storage** (`DELONIX_PROXMOX_TEST_MOVE_STORAGE`,
/// default `local`), with the source reference dropped. Measured against the
/// real lab node first: the node refuses `move_disk` to the SAME storage with
/// the SAME format outright ("you can't move to the same storage with same
/// format", HTTP 500) — Proxmox's own GUI offer to "move disk" within one
/// storage is a *format* change (e.g. raw → qcow2), not a same-format no-op,
/// and this crate has no format-conversion parameter wired up in this pass.
/// A genuine cross-storage move is what the primitive is for in the first
/// place, so that's what this proves instead. `local` doesn't accept VM disk
/// images by default on a stock node — the live run enables `content=images`
/// on it once, out of band, the same way the backup case's own storage
/// (`DELONIX_PROXMOX_TEST_BACKUP_STORAGE`) needs `content=backup` enabled.
#[test]
fn move_disk_unlink_and_cloudinit_dump() {
    // No SKIP line: a print in a library crate's tests is counted debt, and
    // the sibling cases already say it.
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let stage = |_: CreateStage| {};

    let name = format!("dlxdisk{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let vm = delonix_compute::Vm::new(
        name.clone(),
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );
    b.stop(vmdir, &vm)
        .expect("stop before the disk operations below");
    assert!(!b.is_running(&vm), "the VM must be stopped");

    let ledger = delonix_proxmox::Ledger::at(vmdir);

    // cloudinit_dump: the RENDERED file, not the inputs `config` reads. Every
    // VM this backend creates gets a cloud-init drive unless `cloud_init:
    // false` was asked for (`create_form`'s `has_ci`, always true for the
    // plain `VmConfig` above), so a fresh `boot` like this one always has
    // something to dump — an empty answer here would be this backend's own
    // bug, not the node declining to render one.
    let user_dump = b
        .client()
        .cloudinit_dump(vmid, "user")
        .expect("cloudinit dump: user");
    assert!(
        !user_dump.is_empty(),
        "a VM with a cloud-init drive rendered an empty user-data file"
    );
    // An unknown `type` is refused before any request — proven here against a
    // REAL node, not only against the client's own validation (see the `lib.rs`
    // unit test for that half).
    assert!(
        b.client().cloudinit_dump(vmid, "bogus").is_err(),
        "an unknown cloud-init dump type must be refused"
    );

    // move_disk: the boot disk, moved to a SECOND storage with the source
    // reference dropped — see the doc comment above for why this has to be a
    // genuine cross-storage move rather than a same-storage no-op. The effect
    // probe is read back from `config`, never taken from the call's answer,
    // and the SIZE is asserted unchanged — a move alone must not also resize.
    let move_storage =
        std::env::var("DELONIX_PROXMOX_TEST_MOVE_STORAGE").unwrap_or_else(|_| "local".into());
    let (key, size_before) = b.client().boot_disk(vmid).expect("boot disk before move");
    b.client()
        .move_disk(&ledger, vmid, &key, Some(&move_storage), true, None)
        .expect("move_disk to a second storage, dropping the source reference");
    let cfg_after_move = b.client().config(vmid).expect("config after move_disk");
    let disk_val = cfg_after_move
        .get(&key)
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        disk_val.starts_with(&format!("{move_storage}:")),
        "the disk is not on '{move_storage}' after the move: {disk_val}"
    );
    let (_, size_after) = b.client().boot_disk(vmid).expect("boot disk after move");
    assert_eq!(
        size_after, size_before,
        "a move alone must not resize the disk"
    );

    // unlink: the same `ide2` cloud-init drive `cloudinit_dump` just read —
    // a real disk to detach without needing to attach one first, which this
    // crate has no verb for on an existing VM.
    assert!(
        cfg_after_move.get("ide2").is_some(),
        "the VM has no cloud-init drive to unlink: {cfg_after_move}"
    );
    b.client()
        .unlink(&ledger, vmid, &["ide2"], true)
        .expect("unlink the cloud-init drive");
    let cfg_after_unlink = b.client().config(vmid).expect("config after unlink");
    assert!(
        cfg_after_unlink.get("ide2").is_none(),
        "ide2 is still in the config after unlink: {cfg_after_unlink}"
    );

    b.destroy(vmdir, &vm).expect("destroy");
    assert!(
        b.client().config(vmid).is_err(),
        "the VM is still defined on the node after destroy"
    );
}

/// which lives entirely in `delonix-sdn` and has no relationship to any of
/// this. Proves the trap [`delonix_proxmox::Client::firewall_options`]'s doc
/// comment names — a rule means nothing until the firewall itself is turned
/// on — and the full loop of a rule (add, read back two ways, update,
/// delete), every assertion against what the node reports, never against
/// what a call merely claimed.
#[test]
fn the_vms_own_firewall_rule_round_trips_through_the_node() {
    // No SKIP line: a print in a library crate's tests is counted debt, and
    // the sibling cases already say it.
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let ledger = delonix_proxmox::Ledger::at(vmdir);
    let stage = |_: CreateStage| {};

    let name = format!("dlxfw{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let client = b.client();

    // Turned on explicitly, and read back: nothing below would have any
    // effect on the node without this, which is the whole reason
    // `firewall_options`'s doc comment calls it out.
    client
        .set_firewall_enabled(&ledger, vmid, true)
        .expect("enable the VM's own firewall");
    let opts_on = client.firewall_options(vmid).expect("firewall options");
    assert_eq!(
        opts_on.get("enable").and_then(|v| v.as_u64()),
        Some(1),
        "the node does not report the firewall as enabled: {opts_on}"
    );

    // Add one rule, and read it back TWO ways — the list, and the
    // single-rule route — never trusting what `add_firewall_rule` claimed.
    let comment = "delonix-live-test";
    client
        .add_firewall_rule(
            &ledger,
            vmid,
            "in",
            "DROP",
            &delonix_proxmox::FirewallRuleOpts {
                dest: Some("10.0.0.0/8"),
                dport: Some("12345"),
                proto: Some("tcp"),
                comment: Some(comment),
                ..Default::default()
            },
        )
        .expect("add a rule");
    let rules = client.firewall_rules(vmid).expect("list the rules");
    let added = rules
        .iter()
        .find(|r| r.get("comment").and_then(|c| c.as_str()) == Some(comment))
        .unwrap_or_else(|| panic!("the added rule is not in the node's list: {rules:?}"));
    assert_eq!(added.get("type").and_then(|v| v.as_str()), Some("in"));
    assert_eq!(added.get("action").and_then(|v| v.as_str()), Some("DROP"));
    assert_eq!(
        added.get("dest").and_then(|v| v.as_str()),
        Some("10.0.0.0/8")
    );
    assert_eq!(added.get("dport").and_then(|v| v.as_str()), Some("12345"));
    assert_eq!(
        added.get("enable").and_then(|v| v.as_u64()),
        Some(1),
        "`add_firewall_rule` must send `enable` explicitly rather than trust \
         the node's own default: {added}"
    );
    let pos = added
        .get("pos")
        .and_then(|p| p.as_u64())
        .expect("the node's own rule carries no `pos`") as u32;
    let single = client
        .firewall_rule(vmid, pos)
        .expect("read the rule by position");
    assert_eq!(
        single.get("comment").and_then(|c| c.as_str()),
        Some(comment),
        "firewall_rule(pos) disagrees with the node's own list: {single}"
    );

    // Update it — flip the verdict, and confirm a field the update did NOT
    // name (`dest`) survives it untouched.
    client
        .update_firewall_rule(
            &ledger,
            vmid,
            pos,
            &delonix_proxmox::FirewallRuleOpts {
                action: Some("ACCEPT"),
                ..Default::default()
            },
        )
        .expect("update the rule");
    let updated = client
        .firewall_rule(vmid, pos)
        .expect("read the rule back after the update");
    assert_eq!(
        updated.get("action").and_then(|v| v.as_str()),
        Some("ACCEPT"),
        "the update did not stick: {updated}"
    );
    assert_eq!(
        updated.get("dest").and_then(|v| v.as_str()),
        Some("10.0.0.0/8"),
        "a field the update did not name must survive it: {updated}"
    );

    // Delete it — the proof is the node's own list, empty again.
    client
        .delete_firewall_rule(&ledger, vmid, pos)
        .expect("delete the rule");
    let after_delete = client
        .firewall_rules(vmid)
        .expect("list rules after delete");
    assert!(
        after_delete.is_empty(),
        "the rule is still on the node after delete: {after_delete:?}"
    );

    // Turn the firewall back off, and confirm it on the node before cleanup.
    client
        .set_firewall_enabled(&ledger, vmid, false)
        .expect("disable the VM's own firewall");
    let opts_off = client.firewall_options(vmid).expect("firewall options");
    assert_eq!(
        opts_off.get("enable").and_then(|v| v.as_u64()),
        Some(0),
        "the node still reports the firewall as enabled: {opts_off}"
    );

    let vm = delonix_compute::Vm::new(
        name,
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );
    b.stop(vmdir, &vm).expect("stop");
    b.destroy(vmdir, &vm).expect("destroy");
    assert!(
        client.config(vmid).is_err(),
        "the VM is still defined on the node after destroy — an orphan"
    );
}

/// An SDN apply with nothing staged still reloads every node's network, and
/// the apply must wait for each of those reloads, not only for its own task:
/// `reloadnetworkall` starts them in the background and does not follow them
/// (plan 63, slice 0b). With `DELONIX_LOG=warn` the warnings of each node's
/// reload are printed with the node's name — measured on 2026-09-27 with
/// `source /etc/network/interfaces.d/*` removed from the second node, whose
/// reload then warned `missing 'source /etc/network/interfaces.d/sdn'
/// directive`, while the apply's own task said `OK`.
#[test]
fn sdn_apply_waits_for_every_nodes_network_reload() {
    let Some(t) = target() else {
        return;
    };
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = delonix_proxmox::Ledger::at(dir.path());
    b.client()
        .apply_sdn(&ledger)
        .expect("an apply with nothing staged, every node's reload followed");
}

/// Stages a Proxmox SDN zone and a vnet inside it, applies the PENDING
/// configuration to the cluster, and tears both back down — the whole
/// create→vnet→apply cycle `delonix_proxmox::sdn`'s module doc comment warns
/// about: every write up to `apply_sdn` only edits a STAGED config, and this
/// case exists to prove nothing is left half-applied when it is done.
///
/// **Not the SDN this engine has of its own** (netns/nftables on this host,
/// `delonix-sdn`) — see that same module doc comment. This is Proxmox VE's
/// own cluster-wide Zones/VNets subsystem, provisioned by the node itself.
///
/// Only the `simple` zone type is exercised, deliberately: it needs no
/// VLAN-capable hardware on the lab node to prove the cycle end to end.
/// `vlan`/`vxlan`/`qinq` are real Proxmox zone types this crate does not
/// implement.
#[test]
fn sdn_zone_and_vnet_are_staged_applied_and_torn_down() {
    // No SKIP line: a print in a library crate's tests is counted debt, and
    // the sibling cases already say it.
    let Some(t) = target() else {
        return;
    };
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = delonix_proxmox::Ledger::at(dir.path());

    // Derived from this process's id, the same convention every VM name in
    // this file uses, so a run never collides with another one on the
    // shared lab node. Proxmox's own zone/vnet id format (a lowercase letter
    // then up to 7 more lowercase letters or digits, 8 characters total,
    // enforced client-side by `delonix_proxmox::sdn::validate_sdn_id`) is
    // why the prefix is a single letter and the pid is reduced to 6 digits.
    let suffix = std::process::id() % 1_000_000;
    let zone = format!("z{suffix}");
    let vnet = format!("v{suffix}");

    // Create the zone. Answers before anything is REAL — see the module
    // doc comment — which is exactly why the very next line asks the node
    // itself, not the create call's `Ok(())`.
    client
        .create_sdn_zone(&ledger, &zone)
        .expect("create the zone");
    let zones = client.sdn_zones().expect("list zones");
    assert!(
        zones
            .iter()
            .any(|z| z.get("zone").and_then(|v| v.as_str()) == Some(zone.as_str())),
        "the zone is not in the pending list: {zones:?}"
    );

    // A vnet inside that zone, with an alias — proving the field reaches
    // the node and not just that SOME vnet was created.
    client
        .create_sdn_vnet(&ledger, &vnet, &zone, Some("live sdn test"))
        .expect("create the vnet");
    let vnets = client.sdn_vnets().expect("list vnets");
    let listed_vnet = vnets
        .iter()
        .find(|v| v.get("vnet").and_then(|s| s.as_str()) == Some(vnet.as_str()))
        .unwrap_or_else(|| panic!("the vnet is not in the pending list: {vnets:?}"));
    assert_eq!(
        listed_vnet.get("zone").and_then(|z| z.as_str()),
        Some(zone.as_str()),
        "the vnet is not attached to the zone it was created in: {listed_vnet}"
    );
    assert_eq!(
        listed_vnet.get("alias").and_then(|a| a.as_str()),
        Some("live sdn test"),
        "the alias did not reach the node: {listed_vnet}"
    );

    // The one call in this cycle that genuinely forks a task and makes the
    // staged zone/vnet REAL on every node (see `delonix_proxmox::sdn`'s doc
    // comment). Read back from the ledger, not just the call's `Ok(())` —
    // the same discipline every other write in this file follows.
    client.apply_sdn(&ledger).expect("apply the pending config");
    let read_ledger = || -> serde_json::Value {
        serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("proxmox-tasks.json"))
                .expect("the ledger was written"),
        )
        .expect("the ledger is JSON")
    };
    let last_task = |entries: &[serde_json::Value], action: &str| -> serde_json::Value {
        entries
            .iter()
            .rev()
            .find(|e| e.get("action").and_then(|a| a.as_str()) == Some(action))
            .unwrap_or_else(|| panic!("no `{action}` task in the ledger: {entries:?}"))
            .clone()
    };
    let ledger_json = read_ledger();
    let entries = ledger_json.as_array().expect("the ledger is a list");
    let applied = last_task(entries, "apply-sdn");
    assert_eq!(
        applied.pointer("/state/state").and_then(|s| s.as_str()),
        Some("ok"),
        "the apply did not succeed: {applied}"
    );

    // Cleanup: the vnet before the zone (the node refuses to remove a zone
    // a vnet still references — its own business logic, not pre-empted
    // here), then apply AGAIN. Leaving a pending delete unapplied would be
    // the same trap as leaving a pending create unapplied: `sdn_zones`/
    // `sdn_vnets` would agree it is gone while a node that already
    // realized it keeps running it until the next reload.
    client
        .delete_sdn_vnet(&ledger, &vnet)
        .expect("delete the vnet");
    client
        .delete_sdn_zone(&ledger, &zone)
        .expect("delete the zone");
    client
        .apply_sdn(&ledger)
        .expect("apply the pending deletion");

    // Measured against a live node, not assumed: `create_sdn_zone`/
    // `create_sdn_vnet`/`delete_sdn_vnet`/`delete_sdn_zone` fork NO task at
    // all — they apply inline, the same as `unlink` and every per-VM
    // firewall write. `task_or_done`'s own `task_inner` returns `Ok(())`
    // BEFORE ever calling `ledger.record(...)` on that path (see its doc
    // comment), so there is no ledger entry to assert on for any of the
    // four — asserting one here would be checking for something the
    // machinery structurally cannot produce. Only `apply_sdn` genuinely
    // forks a task, and it is the only write this cycle can hold the
    // ledger accountable for.
    let ledger_json = read_ledger();
    let entries = ledger_json.as_array().expect("the ledger is a list");
    // Two `apply-sdn` tasks by now (the create and the delete); the LAST one
    // is what has to have succeeded — the same "last task of each action"
    // rule the ledger assertions above this test already use.
    let last_apply = last_task(entries, "apply-sdn");
    assert_eq!(
        last_apply.pointer("/state/state").and_then(|s| s.as_str()),
        Some("ok"),
        "the final apply (the deletion) did not succeed: {last_apply}"
    );

    let zones_after = client.sdn_zones().expect("list zones after cleanup");
    assert!(
        !zones_after
            .iter()
            .any(|z| z.get("zone").and_then(|v| v.as_str()) == Some(zone.as_str())),
        "the zone is still listed after delete+apply: {zones_after:?}"
    );
    let vnets_after = client.sdn_vnets().expect("list vnets after cleanup");
    assert!(
        !vnets_after
            .iter()
            .any(|v| v.get("vnet").and_then(|s| s.as_str()) == Some(vnet.as_str())),
        "the vnet is still listed after delete+apply: {vnets_after:?}"
    );
}

/// The other half of `cloudinit_dump`: what `GET …/cloudinit` reports for a
/// key just after a config write, and what `PUT …/cloudinit` (regenerate)
/// actually reaches. This case is what MEASURED
/// [`delonix_proxmox::Client::cloudinit_pending`]'s doc comment — it
/// originally asserted the same premise that comment now says is false
/// (that an `ipconfig0` write shows up here as pending): a live PVE 9.2.2
/// run of this exact test, VM stopped then again running, found the list
/// empty before AND after the write, before AND after regenerate. That is
/// not this test failing to prove something — it IS the measurement, and
/// it is why the assertions below stop at what a real answer can confirm:
/// the list is well-formed (an empty list is success, not a probe failure),
/// `regenerate` does not error, and — the part that matters to a caller —
/// the RENDERED file genuinely carries the write once `regenerate` has run.
#[test]
fn cloudinit_pending_answers_and_regenerate_reaches_the_rendered_file() {
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let stage = |_: CreateStage| {};

    let name = format!("dlxci{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let vm = delonix_compute::Vm::new(
        name.clone(),
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );
    b.stop(vmdir, &vm)
        .expect("stop before touching the cloud-init config");
    assert!(!b.is_running(&vm), "the VM must be stopped");

    let ledger = delonix_proxmox::Ledger::at(vmdir);
    let client = b.client();

    // Baseline: a plain read, before anything is touched. The route
    // answering at all — never an error — is the assertion; whether the
    // list is empty or not is not something this crate claims to predict
    // (see `cloudinit_pending`'s doc comment for what was measured).
    client
        .cloudinit_pending(vmid)
        .expect("cloudinit pending: before");

    // Stages a change: the same `POST …/config` path `configure_clone`
    // already uses for every clone, now with a static address —
    // `cloud_init_form` turns that into `ipconfig0=ip=<addr>`.
    let restaged = VmConfig {
        name: name.clone(),
        disk: cfg.disk.clone(),
        vcpus: 1,
        memory: "512M".into(),
        static_ip: Some("10.99.0.5/24".into()),
        ..Default::default()
    };
    client
        .configure_clone(&ledger, vmid, &restaged)
        .expect("stage a new ipconfig0 via a config write");

    // Still just a well-formed read — measured on a live node to stay empty
    // right here, for this key, and that is not an error to assert against.
    client
        .cloudinit_pending(vmid)
        .expect("cloudinit pending: after the config write");

    client
        .cloudinit_regenerate(&ledger, vmid)
        .expect("regenerate the cloud-init drive");

    client
        .cloudinit_pending(vmid)
        .expect("cloudinit pending: after regenerate");

    // The rendered network file must now carry the applied address — the
    // one effect of this whole sequence a live answer CAN confirm.
    let network_dump = client
        .cloudinit_dump(vmid, "network")
        .expect("cloudinit dump: network, after regenerate");
    assert!(
        network_dump.contains("10.99.0.5"),
        "the regenerated cloud-init network file does not carry the new address: \
         {network_dump}"
    );

    b.destroy(vmdir, &vm).expect("destroy");
}

/// The VM's own firewall's aliases, IP sets, log and refs — the rest of the
/// per-VM firewall surface [`the_vms_own_firewall_rule_round_trips_through_the_node`]
/// (above) does not cover (that one exercises `options`/`rules` only).
///
/// Round-trips an alias and an IP-set entry through the node, reading each
/// back TWO ways (the list, and the single-item route) the same way the
/// rule test does, never trusting what the write call itself claimed —
/// and confirms a field an update did not name survives it, the same
/// property the rule test's update case proves for a rule.
#[test]
fn the_vms_own_firewall_aliases_ipsets_log_and_refs_round_trip_through_the_node() {
    // No SKIP line: a print in a library crate's tests is counted debt, and
    // the sibling cases already say it.
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let ledger = delonix_proxmox::Ledger::at(vmdir);
    let stage = |_: CreateStage| {};

    let name = format!("dlxfwa{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let client = b.client();

    // A caller confirming the sub-path exists before touching anything
    // under it — see `Client::firewall_index`'s own doc comment for why
    // this test asserts nothing about the SHAPE of what comes back, only
    // that the route answers at all.
    client.firewall_index(vmid).expect("firewall index");

    // Proxmox's own alias/ipset name format (a letter then one or more
    // letters/digits/`-`/`_`) is checked client-side by
    // `validate_firewall_object_name`; the suffix keeps this run from
    // colliding with another one on the shared lab node.
    let suffix = std::process::id() % 1_000_000;
    let alias = format!("a{suffix}");
    let ipset = format!("s{suffix}");

    // Alias: add, read back TWO ways, update one field and confirm the
    // other survives untouched, delete.
    client
        .add_firewall_alias(&ledger, vmid, &alias, "10.10.0.0/16", Some("delonix-live"))
        .expect("add an alias");
    let aliases = client.firewall_aliases(vmid).expect("list aliases");
    let added = aliases
        .iter()
        .find(|a| a.get("name").and_then(|v| v.as_str()) == Some(alias.as_str()))
        .unwrap_or_else(|| panic!("the added alias is not in the node's list: {aliases:?}"));
    assert_eq!(
        added.get("cidr").and_then(|v| v.as_str()),
        Some("10.10.0.0/16")
    );
    let single = client
        .firewall_alias(vmid, &alias)
        .expect("read the alias by name");
    assert_eq!(
        single.get("comment").and_then(|c| c.as_str()),
        Some("delonix-live"),
        "firewall_alias(name) disagrees with the node's own list: {single}"
    );

    client
        .update_firewall_alias(&ledger, vmid, &alias, Some("10.11.0.0/16"), None)
        .expect("update the alias");
    let updated = client
        .firewall_alias(vmid, &alias)
        .expect("read the alias back after the update");
    assert_eq!(
        updated.get("cidr").and_then(|v| v.as_str()),
        Some("10.11.0.0/16"),
        "the update did not stick: {updated}"
    );
    assert_eq!(
        updated.get("comment").and_then(|c| c.as_str()),
        Some("delonix-live"),
        "a field the update did not name must survive it: {updated}"
    );

    client
        .delete_firewall_alias(&ledger, vmid, &alias)
        .expect("delete the alias");
    let aliases_after = client
        .firewall_aliases(vmid)
        .expect("list aliases after delete");
    assert!(
        !aliases_after
            .iter()
            .any(|a| a.get("name").and_then(|v| v.as_str()) == Some(alias.as_str())),
        "the alias is still on the node after delete: {aliases_after:?}"
    );

    // IP set: create the (empty) set, add one CIDR entry, read it back TWO
    // ways, update its `nomatch` and confirm `comment` — a field that
    // update did not name — survives untouched, remove the entry, remove
    // the set.
    client
        .create_firewall_ipset(&ledger, vmid, &ipset, Some("delonix-live-set"))
        .expect("create an ipset");
    let ipsets = client.firewall_ipsets(vmid).expect("list ipsets");
    assert!(
        ipsets
            .iter()
            .any(|s| s.get("name").and_then(|v| v.as_str()) == Some(ipset.as_str())),
        "the created ipset is not in the node's list: {ipsets:?}"
    );

    let entry_cidr = "10.30.0.0/24";
    client
        .add_firewall_ipset_cidr(
            &ledger,
            vmid,
            &ipset,
            entry_cidr,
            &delonix_proxmox::IpsetCidrOpts {
                comment: Some("entry"),
                nomatch: None,
            },
        )
        .expect("add an ipset entry");
    let entries = client
        .firewall_ipset_entries(vmid, &ipset)
        .expect("list ipset entries");
    let added_entry = entries
        .iter()
        .find(|e| e.get("cidr").and_then(|v| v.as_str()) == Some(entry_cidr))
        .unwrap_or_else(|| panic!("the added entry is not in the set: {entries:?}"));
    assert_eq!(
        added_entry.get("comment").and_then(|c| c.as_str()),
        Some("entry")
    );
    let single_entry = client
        .firewall_ipset_cidr(vmid, &ipset, entry_cidr)
        .expect("read the entry by cidr — proves the '/' in the path segment round-trips");
    assert_eq!(
        single_entry.get("comment").and_then(|c| c.as_str()),
        Some("entry"),
        "firewall_ipset_cidr(name, cidr) disagrees with the node's own list: {single_entry}"
    );

    client
        .update_firewall_ipset_cidr(
            &ledger,
            vmid,
            &ipset,
            entry_cidr,
            &delonix_proxmox::IpsetCidrOpts {
                comment: None,
                nomatch: Some(true),
            },
        )
        .expect("update the ipset entry");
    let updated_entry = client
        .firewall_ipset_cidr(vmid, &ipset, entry_cidr)
        .expect("read the entry back after the update");
    assert_eq!(
        updated_entry.get("nomatch").and_then(|v| v.as_u64()),
        Some(1),
        "the update did not stick: {updated_entry}"
    );
    assert_eq!(
        updated_entry.get("comment").and_then(|c| c.as_str()),
        Some("entry"),
        "a field the update did not name must survive it: {updated_entry}"
    );

    // What refers to the alias/ipset just built — read while they still
    // exist, the point at which a real caller would consult this before
    // deciding whether a delete is safe.
    client.firewall_refs(vmid).expect("firewall refs");

    // The tail of the firewall's own log — never enabled or exercised by
    // this test, so this only proves the route answers, not that it is
    // non-empty.
    client.firewall_log(vmid).expect("firewall log");

    client
        .delete_firewall_ipset_cidr(&ledger, vmid, &ipset, entry_cidr)
        .expect("delete the ipset entry");
    let entries_after = client
        .firewall_ipset_entries(vmid, &ipset)
        .expect("list ipset entries after delete");
    assert!(
        entries_after.is_empty(),
        "the entry is still in the set after delete: {entries_after:?}"
    );

    client
        .delete_firewall_ipset(&ledger, vmid, &ipset)
        .expect("delete the ipset");
    let ipsets_after = client
        .firewall_ipsets(vmid)
        .expect("list ipsets after delete");
    assert!(
        !ipsets_after
            .iter()
            .any(|s| s.get("name").and_then(|v| v.as_str()) == Some(ipset.as_str())),
        "the ipset is still on the node after delete: {ipsets_after:?}"
    );

    let vm = delonix_compute::Vm::new(
        name,
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );
    b.stop(vmdir, &vm).expect("stop");
    b.destroy(vmdir, &vm).expect("destroy");
    assert!(
        client.config(vmid).is_err(),
        "the VM is still defined on the node after destroy — an orphan"
    );
}

/// Stages a subnet inside a zone/vnet pair, exercises the single-item
/// zone/vnet read/edit routes and the subnet's own read/edit/delete, applies
/// the PENDING configuration, confirms via the NODE's own directory-index
/// routes that the zone/vnet answer at all (see `delonix_proxmox::sdn`'s
/// module doc comment, "The two node-side routes here answer a directory
/// index, not 'is this real'" — those two calls do NOT prove the subnet's
/// dataplane is live; they prove the route exists against a real node and
/// that the id reached the URL path unmangled), and tears everything back
/// down.
///
/// Sibling of [`sdn_zone_and_vnet_are_staged_applied_and_torn_down`] — same
/// zone/vnet, same cleanup order, same "read the ledger, not just `Ok(())`"
/// discipline — extended with the one thing that test does not cover: a
/// subnet, and the single-item `GET`/`PUT` for a zone and a vnet.
#[test]
fn sdn_subnet_and_the_single_item_zone_vnet_routes_are_staged_applied_and_torn_down() {
    let Some(t) = target() else {
        return;
    };
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = delonix_proxmox::Ledger::at(dir.path());

    // Same naming convention as the sibling test, and a distinct letter
    // prefix so the two tests never collide on the same lab node even if
    // run in the same second.
    let suffix = std::process::id() % 1_000_000;
    let zone = format!("s{suffix}");
    let vnet = format!("t{suffix}");
    let cidr = "10.77.0.0/24";

    // --- Zone: create, then the single-item GET and the PUT this slice adds ---
    client
        .create_sdn_zone(&ledger, &zone)
        .expect("create the zone");
    let zone_obj = client.sdn_zone(&zone).expect("read the zone back");
    assert_eq!(
        zone_obj.get("zone").and_then(|v| v.as_str()),
        Some(zone.as_str()),
        "GET /cluster/sdn/zones/{{zone}} did not echo the zone it was asked for: {zone_obj}"
    );
    client
        .update_sdn_zone(&ledger, &zone, Some(1400))
        .expect("stage the mtu change");
    let zone_obj = client
        .sdn_zone(&zone)
        .expect("read the zone after the mtu update");
    assert_eq!(
        zone_obj.get("mtu").and_then(serde_json::Value::as_u64),
        Some(1400),
        "the mtu did not reach the node: {zone_obj}"
    );

    // --- Vnet: create with an alias, then the single-item GET and PUT ---
    client
        .create_sdn_vnet(&ledger, &vnet, &zone, Some("live subnet test"))
        .expect("create the vnet");
    let vnet_obj = client.sdn_vnet(&vnet).expect("read the vnet back");
    assert_eq!(
        vnet_obj.get("alias").and_then(|v| v.as_str()),
        Some("live subnet test"),
        "GET /cluster/sdn/vnets/{{vnet}} did not echo the alias it was created with: {vnet_obj}"
    );
    client
        .update_sdn_vnet(&ledger, &vnet, Some("renamed by the live test"))
        .expect("stage the alias change");
    let vnet_obj = client
        .sdn_vnet(&vnet)
        .expect("read the vnet after the alias update");
    assert_eq!(
        vnet_obj.get("alias").and_then(|v| v.as_str()),
        Some("renamed by the live test"),
        "the renamed alias did not reach the node: {vnet_obj}"
    );

    // --- Subnet: create (the id comes back — POST answers null, see the
    // module doc comment's "A subnet's id is not something a caller ever
    // writes"), read, and edit its gateway ---
    let subnet_id = client
        .create_sdn_subnet(&ledger, &vnet, &zone, cidr, Some("10.77.0.1"))
        .expect("create the subnet");
    assert_eq!(
        subnet_id,
        format!("{zone}-10.77.0.0-24"),
        "the computed subnet id does not match Proxmox's own <zone>-<network>-<mask> \
         construction"
    );
    let subnets = client
        .sdn_vnet_subnets(&vnet)
        .expect("list the vnet's subnets");
    assert!(
        subnets
            .iter()
            .any(|s| s.get("subnet").and_then(|v| v.as_str()) == Some(subnet_id.as_str())),
        "the subnet is not in the pending list: {subnets:?}"
    );
    let subnet_obj = client
        .sdn_vnet_subnet(&vnet, &zone, cidr)
        .expect("read the subnet back by (vnet, zone, cidr)");
    assert_eq!(
        subnet_obj.get("gateway").and_then(|v| v.as_str()),
        Some("10.77.0.1"),
        "the gateway did not reach the node: {subnet_obj}"
    );
    client
        .update_sdn_subnet(&ledger, &vnet, &zone, cidr, Some("10.77.0.254"))
        .expect("stage the gateway change");
    let subnet_obj = client
        .sdn_vnet_subnet(&vnet, &zone, cidr)
        .expect("read the subnet after the gateway update");
    assert_eq!(
        subnet_obj.get("gateway").and_then(|v| v.as_str()),
        Some("10.77.0.254"),
        "the changed gateway did not reach the node: {subnet_obj}"
    );

    // --- Apply: the one call in this cycle that genuinely forks a task and
    // makes all three staged objects real on every node ---
    client.apply_sdn(&ledger).expect("apply the pending config");
    let read_ledger = || -> serde_json::Value {
        serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("proxmox-tasks.json"))
                .expect("the ledger was written"),
        )
        .expect("the ledger is JSON")
    };
    let last_task = |entries: &[serde_json::Value], action: &str| -> serde_json::Value {
        entries
            .iter()
            .rev()
            .find(|e| e.get("action").and_then(|a| a.as_str()) == Some(action))
            .unwrap_or_else(|| panic!("no `{action}` task in the ledger: {entries:?}"))
            .clone()
    };
    let ledger_json = read_ledger();
    let entries = ledger_json.as_array().expect("the ledger is a list");
    let applied = last_task(entries, "apply-sdn");
    assert_eq!(
        applied.pointer("/state/state").and_then(|s| s.as_str()),
        Some("ok"),
        "the apply did not succeed: {applied}"
    );

    // --- The node's own directory index for the zone/vnet — proof the route
    // answers against a real node, NOT proof of dataplane realization (see
    // the module doc comment; a diridx answers the same fixed list whether
    // or not `apply_sdn` was ever called, so this is deliberately checked
    // AFTER the apply above rather than instead of it). ---
    let zone_index = client
        .sdn_zone_node_index(&zone)
        .expect("the node answers its own directory index for the zone");
    assert!(
        zone_index
            .iter()
            .any(|e| e.get("subdir").and_then(|v| v.as_str()) == Some("content")),
        "GET /nodes/{{node}}/sdn/zones/{{zone}} did not list its 'content' subdir: {zone_index:?}"
    );
    let vnet_index = client
        .sdn_vnet_node_index(&vnet)
        .expect("the node answers its own directory index for the vnet");
    assert!(
        vnet_index
            .iter()
            .any(|e| e.get("subdir").and_then(|v| v.as_str()) == Some("mac-vrf")),
        "GET /nodes/{{node}}/sdn/vnets/{{vnet}} did not list its 'mac-vrf' subdir: {vnet_index:?}"
    );

    // --- Cleanup: subnet before vnet before zone (the node refuses to
    // remove a vnet that still has a subnet, and a zone a vnet still
    // references — its own business logic, not pre-empted here). The
    // subnet's own absence is checked right here, from the PENDING list —
    // the same "a GET reads the same pending state a POST/DELETE just
    // wrote" the module doc comment already relies on — and NOT after the
    // vnet itself is gone: `GET .../vnets/{vnet}/subnets` needs the vnet to
    // still exist to answer at all.
    client
        .delete_sdn_subnet(&ledger, &vnet, &zone, cidr)
        .expect("delete the subnet");
    let subnets_after_delete = client
        .sdn_vnet_subnets(&vnet)
        .expect("list the vnet's subnets after deleting the subnet");
    assert!(
        !subnets_after_delete
            .iter()
            .any(|s| s.get("subnet").and_then(|v| v.as_str()) == Some(subnet_id.as_str())),
        "the subnet is still in the pending list right after its own delete: \
         {subnets_after_delete:?}"
    );

    // Then the vnet and the zone, and apply AGAIN so nothing is left
    // half-applied.
    client
        .delete_sdn_vnet(&ledger, &vnet)
        .expect("delete the vnet");
    client
        .delete_sdn_zone(&ledger, &zone)
        .expect("delete the zone");
    client
        .apply_sdn(&ledger)
        .expect("apply the pending deletion");

    let ledger_json = read_ledger();
    let entries = ledger_json.as_array().expect("the ledger is a list");
    let last_apply = last_task(entries, "apply-sdn");
    assert_eq!(
        last_apply.pointer("/state/state").and_then(|s| s.as_str()),
        Some("ok"),
        "the final apply (the deletion) did not succeed: {last_apply}"
    );

    let vnets_after = client.sdn_vnets().expect("list vnets after cleanup");
    assert!(
        !vnets_after
            .iter()
            .any(|v| v.get("vnet").and_then(|s| s.as_str()) == Some(vnet.as_str())),
        "the vnet is still listed after delete+apply: {vnets_after:?}"
    );
    let zones_after = client.sdn_zones().expect("list zones after cleanup");
    assert!(
        !zones_after
            .iter()
            .any(|z| z.get("zone").and_then(|v| v.as_str()) == Some(zone.as_str())),
        "the zone is still listed after delete+apply: {zones_after:?}"
    );
}

/// A stand-in for the external controller an IPAM or DNS entry names: the
/// node VERIFIES both on create and on update by calling the URL, so a live
/// case for those routes needs something at the other end that answers. This
/// answers `200` with an empty JSON collection to anything and keeps the
/// request lines and the two auth headers the node is known to send, which
/// is what the test asserts on. Bound on every interface at an ephemeral
/// port; the node reaches it at `DELONIX_PROXMOX_TEST_CALLBACK_ADDR`.
struct ControllerStub {
    port: u16,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl ControllerStub {
    fn start() -> ControllerStub {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("0.0.0.0:0").expect("bind the stub");
        let port = listener.local_addr().expect("stub addr").port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut line = head.lines().next().unwrap_or("").to_string();
                for h in head.lines() {
                    let lower = h.to_ascii_lowercase();
                    if lower.starts_with("authorization:") || lower.starts_with("x-api-key:") {
                        line.push_str(" | ");
                        line.push_str(h.trim());
                    }
                }
                log.lock().expect("stub log").push(line);
                let body = br#"{"results":[],"count":0}"#;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(body);
            }
        });
        ControllerStub { port, seen }
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().expect("stub log").clone()
    }
}

/// The layer above zones/vnets/subnets, against a real node: IPAM and DNS
/// controllers (with a stub at the other end of their URL, because the node
/// calls it), a fabric and its node member, a DHCP-serving zone with a
/// DHCP range on its subnet, the apply that makes the zone and the fabric
/// real, the node-side fabric reads that only answer once it is, and IP
/// reservations in the `pve` IPAM — which act on the RUNNING subnet, so
/// they come after the apply. Everything staged here is torn down and a
/// second apply proves the node ends as it started.
///
/// The controller half needs `DELONIX_PROXMOX_TEST_CALLBACK_ADDR` (the
/// address the NODE can reach this host at); without it that half is
/// skipped and the rest still runs. No SKIP line: a print in a library
/// crate's tests is counted debt.
#[test]
fn sdn_controllers_fabric_dhcp_and_ip_reservations_round_trip_through_the_node() {
    use delonix_proxmox::{DhcpRange, FabricProtocol, IpamKind, SubnetOptions, ZoneOptions};
    let Some(t) = target() else {
        return;
    };
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = delonix_proxmox::Ledger::at(dir.path());
    let node = t.node.clone();

    let suffix = std::process::id() % 1_000_000;
    let ipam = format!("i{suffix}");
    let dns = format!("n{suffix}");
    let fabric = format!("f{suffix}");
    let zone = format!("d{suffix}");
    let vnet = format!("e{suffix}");
    let cidr = "10.88.0.0/24";

    // --- IPAM and DNS controllers, verified by the node against the stub ---
    if let Ok(callback) = std::env::var("DELONIX_PROXMOX_TEST_CALLBACK_ADDR") {
        let stub = ControllerStub::start();
        let base = format!("http://{callback}:{}", stub.port);

        client
            .create_sdn_ipam(
                &ledger,
                &ipam,
                IpamKind::Netbox,
                &format!("{base}/api"),
                "tok-one",
                None,
            )
            .expect("create the NetBox IPAM entry");
        let obj = client.sdn_ipam(&ipam).expect("read the IPAM entry back");
        assert_eq!(
            obj.get("type").and_then(|v| v.as_str()),
            Some("netbox"),
            "{obj}"
        );
        assert_eq!(
            obj.get("token").and_then(|v| v.as_str()),
            Some("tok-one"),
            "{obj}"
        );
        client
            .update_sdn_ipam(&ledger, &ipam, None, Some("tok-two"), None)
            .expect("update the IPAM token");
        let obj = client
            .sdn_ipam(&ipam)
            .expect("read the IPAM entry after the update");
        assert_eq!(
            obj.get("token").and_then(|v| v.as_str()),
            Some("tok-two"),
            "{obj}"
        );
        assert!(
            client
                .sdn_ipams()
                .expect("list IPAMs")
                .iter()
                .any(|i| { i.get("ipam").and_then(|v| v.as_str()) == Some("pve") }),
            "the built-in pve IPAM is always listed"
        );
        client
            .delete_sdn_ipam(&ledger, &ipam)
            .expect("delete the IPAM entry");
        assert!(
            !client
                .sdn_ipams()
                .expect("list IPAMs after delete")
                .iter()
                .any(|i| { i.get("ipam").and_then(|v| v.as_str()) == Some(ipam.as_str()) }),
            "the IPAM entry is still listed after its delete"
        );

        client
            .create_sdn_dns(
                &ledger,
                &dns,
                &format!("{base}/api/v1/servers/localhost"),
                "key-one",
                Some(300),
            )
            .expect("create the PowerDNS entry");
        let obj = client.sdn_dns(&dns).expect("read the DNS entry back");
        assert_eq!(
            obj.get("ttl").and_then(serde_json::Value::as_u64),
            Some(300),
            "{obj}"
        );
        client
            .update_sdn_dns(&ledger, &dns, None, None, Some(600))
            .expect("update the DNS ttl");
        let obj = client
            .sdn_dns(&dns)
            .expect("read the DNS entry after the update");
        assert_eq!(
            obj.get("ttl").and_then(serde_json::Value::as_u64),
            Some(600),
            "{obj}"
        );
        client
            .delete_sdn_dns(&ledger, &dns)
            .expect("delete the DNS entry");
        assert!(
            !client
                .sdn_dns_controllers()
                .expect("list DNS after delete")
                .iter()
                .any(|d| { d.get("dns").and_then(|v| v.as_str()) == Some(dns.as_str()) }),
            "the DNS entry is still listed after its delete"
        );

        // The node did call the controllers: that is the fact this half exists
        // to prove, and the one a caller has to know (an unreachable URL is a
        // hung request, not a staged entry).
        let seen = stub.requests();
        assert!(
            seen.iter()
                .any(|l| l.contains("/api/ipam/aggregates/") && l.contains("token tok-one")),
            "the node did not verify the NetBox entry on create: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|l| l.contains("/api/ipam/aggregates/") && l.contains("token tok-two")),
            "the node did not verify the NetBox entry on update: {seen:?}"
        );
        assert!(
            seen.iter().any(|l| l.contains("/api/v1/servers/localhost")
                && l.to_ascii_lowercase().contains("x-api-key: key-one")),
            "the node did not verify the PowerDNS entry: {seen:?}"
        );
    }

    // --- a fabric and its node member, staged ---
    client
        .create_sdn_fabric(
            &ledger,
            &fabric,
            FabricProtocol::OpenFabric,
            Some("10.99.0.0/24"),
            Some(3),
            None,
        )
        .expect("create the fabric");
    let f = client.sdn_fabric(&fabric).expect("read the fabric back");
    assert_eq!(
        f.get("protocol").and_then(|v| v.as_str()),
        Some("openfabric"),
        "{f}"
    );
    assert_eq!(
        f.get("hello_interval").and_then(serde_json::Value::as_u64),
        Some(3),
        "{f}"
    );
    client
        .update_sdn_fabric(&ledger, &fabric, FabricProtocol::OpenFabric, Some(5), None)
        .expect("update the fabric's hello interval");
    let f = client
        .sdn_fabric(&fabric)
        .expect("read the fabric after the update");
    assert_eq!(
        f.get("hello_interval").and_then(serde_json::Value::as_u64),
        Some(5),
        "{f}"
    );
    client
        .create_sdn_fabric_node(
            &ledger,
            &fabric,
            &node,
            FabricProtocol::OpenFabric,
            Some("10.99.0.1"),
            &[],
        )
        .expect("add this node to the fabric");
    let n = client
        .sdn_fabric_node(&fabric, &node)
        .expect("read the fabric node back");
    assert_eq!(
        n.get("ip").and_then(|v| v.as_str()),
        Some("10.99.0.1"),
        "{n}"
    );
    client
        .update_sdn_fabric_node(
            &ledger,
            &fabric,
            &node,
            FabricProtocol::OpenFabric,
            Some("10.99.0.2"),
        )
        .expect("update the fabric node's address");
    let n = client
        .sdn_fabric_node(&fabric, &node)
        .expect("read the fabric node after the update");
    assert_eq!(
        n.get("ip").and_then(|v| v.as_str()),
        Some("10.99.0.2"),
        "{n}"
    );
    let nodes = client
        .sdn_fabric_nodes(&fabric)
        .expect("list the fabric's nodes");
    assert!(
        nodes
            .iter()
            .any(|x| x.get("node_id").and_then(|v| v.as_str()) == Some(node.as_str())),
        "{nodes:?}"
    );
    let all = client.sdn_fabrics_all().expect("fabrics/all");
    assert!(
        all.get("fabrics")
            .and_then(|v| v.as_array())
            .is_some_and(|fs| {
                fs.iter()
                    .any(|x| x.get("id").and_then(|v| v.as_str()) == Some(fabric.as_str()))
            }),
        "fabrics/all does not list the fabric: {all}"
    );
    let index = client
        .sdn_fabric_node_index(&fabric)
        .expect("node-side fabric index");
    assert!(
        index
            .iter()
            .any(|e| e.get("subdir").and_then(|v| v.as_str()) == Some("routes")),
        "{index:?}"
    );

    // --- a DHCP-serving zone with a ranged subnet, staged ---
    client
        .create_sdn_zone_with(
            &ledger,
            &zone,
            &ZoneOptions {
                dhcp_dnsmasq: true,
                ipam: Some("pve"),
                ..Default::default()
            },
        )
        .expect("create the DHCP zone");
    let z = client.sdn_zone(&zone).expect("read the zone back");
    assert_eq!(
        z.get("dhcp").and_then(|v| v.as_str()),
        Some("dnsmasq"),
        "{z}"
    );
    client
        .create_sdn_vnet(&ledger, &vnet, &zone, None)
        .expect("create the vnet");
    let range1 = [DhcpRange {
        start: "10.88.0.100".into(),
        end: "10.88.0.150".into(),
    }];
    client
        .create_sdn_subnet_with(
            &ledger,
            &vnet,
            &zone,
            cidr,
            &SubnetOptions {
                gateway: Some("10.88.0.1"),
                dhcp_ranges: &range1,
                dhcp_dns_server: Some("10.88.0.1"),
                snat: None,
            },
        )
        .expect("create the subnet with a DHCP range");
    let sub = client
        .sdn_vnet_subnet(&vnet, &zone, cidr)
        .expect("read the subnet back");
    assert_eq!(
        sub.pointer("/dhcp-range/0/start-address")
            .and_then(|v| v.as_str()),
        Some("10.88.0.100"),
        "the DHCP range did not reach the node: {sub}"
    );
    assert_eq!(
        sub.get("dhcp-dns-server").and_then(|v| v.as_str()),
        Some("10.88.0.1"),
        "{sub}"
    );
    let range2 = [DhcpRange {
        start: "10.88.0.110".into(),
        end: "10.88.0.160".into(),
    }];
    client
        .update_sdn_subnet_with(
            &ledger,
            &vnet,
            &zone,
            cidr,
            &SubnetOptions {
                dhcp_ranges: &range2,
                ..Default::default()
            },
        )
        .expect("change the DHCP range");
    let sub = client
        .sdn_vnet_subnet(&vnet, &zone, cidr)
        .expect("read the subnet after the range update");
    assert_eq!(
        sub.pointer("/dhcp-range/0/end-address")
            .and_then(|v| v.as_str()),
        Some("10.88.0.160"),
        "the changed DHCP range did not reach the node: {sub}"
    );

    // --- apply: the zone, the subnet's dnsmasq and the fabric's FRR become real ---
    client.apply_sdn(&ledger).expect("apply the pending config");
    let read_ledger = || -> Vec<serde_json::Value> {
        serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(dir.path().join("proxmox-tasks.json"))
                .expect("the ledger was written"),
        )
        .expect("the ledger is JSON")
        .as_array()
        .expect("the ledger is a list")
        .clone()
    };
    let last_apply = |entries: &[serde_json::Value]| -> serde_json::Value {
        entries
            .iter()
            .rev()
            .find(|e| e.get("action").and_then(|a| a.as_str()) == Some("apply-sdn"))
            .expect("an apply-sdn task in the ledger")
            .clone()
    };
    let applied = last_apply(&read_ledger());
    assert_eq!(
        applied.pointer("/state/state").and_then(|s| s.as_str()),
        Some("ok"),
        "{applied}"
    );

    // Node-side fabric reads answer only for a RUNNING fabric — this is that,
    // and the interface list is the proof the fabric is real on this node
    // (its dummy loopback), not just present in the running config.
    let ifaces = client
        .sdn_fabric_interfaces(&fabric)
        .expect("fabric interfaces after apply");
    let dummy = format!("dummy_{fabric}");
    assert!(
        ifaces
            .iter()
            .any(|i| i.get("name").and_then(|v| v.as_str()) == Some(dummy.as_str())),
        "the fabric's own interface is not up on the node: {ifaces:?}"
    );
    let neigh = client
        .sdn_fabric_neighbors(&fabric)
        .expect("fabric neighbours after apply");
    assert!(
        neigh.is_empty(),
        "a single node has no neighbour: {neigh:?}"
    );
    client
        .sdn_fabric_routes(&fabric)
        .expect("fabric routes after apply");
    // `status: "available"` is the assertion that matters: an apply can end
    // `TASK OK` and realize nothing (see the module doc comment of `sdn.rs`
    // for the case this repository's own appliance image hit), and only this
    // route says which of the two happened.
    let content = client
        .sdn_zone_content(&zone)
        .expect("the zone's content on this node");
    let entry = content
        .iter()
        .find(|c| c.get("vnet").and_then(|v| v.as_str()) == Some(vnet.as_str()))
        .unwrap_or_else(|| {
            panic!("the applied zone does not list its vnet on the node: {content:?}")
        });
    assert_eq!(
        entry.get("status").and_then(|v| v.as_str()),
        Some("available"),
        "the vnet is in the zone but not realized on the node: {entry}"
    );

    // --- IP reservations, against the now-running subnet ---
    client
        .sdn_vnet_ip_add(
            &ledger,
            &vnet,
            &zone,
            "10.88.0.50",
            Some("BC:24:11:00:00:01"),
        )
        .expect("reserve an address");
    let held = client.sdn_ipam_status("pve").expect("pve IPAM status");
    let entry = held
        .iter()
        .find(|e| e.get("ip").and_then(|v| v.as_str()) == Some("10.88.0.50"))
        .unwrap_or_else(|| panic!("the reservation is not in the pve IPAM: {held:?}"));
    assert!(
        entry
            .get("mac")
            .and_then(|v| v.as_str())
            .is_some_and(|m| m.eq_ignore_ascii_case("BC:24:11:00:00:01")),
        "{entry}"
    );
    client
        .sdn_vnet_ip_update(
            &ledger,
            &vnet,
            &zone,
            "BC:24:11:00:00:01",
            "10.88.0.51",
            None,
        )
        .expect("move the MAC's reservation to another address");
    let held = client
        .sdn_ipam_status("pve")
        .expect("pve IPAM status after the update");
    assert!(
        held.iter().any(|e| {
            e.get("ip").and_then(|v| v.as_str()) == Some("10.88.0.51")
                && e.get("mac")
                    .and_then(|v| v.as_str())
                    .is_some_and(|m| m.eq_ignore_ascii_case("BC:24:11:00:00:01"))
        }),
        "the new address did not reach the IPAM: {held:?}"
    );
    assert!(
        !held
            .iter()
            .any(|e| e.get("ip").and_then(|v| v.as_str()) == Some("10.88.0.50")),
        "the old address survived the move: {held:?}"
    );
    client
        .sdn_vnet_ip_delete(
            &ledger,
            &vnet,
            &zone,
            "10.88.0.51",
            Some("BC:24:11:00:00:01"),
        )
        .expect("release the address");
    let held = client
        .sdn_ipam_status("pve")
        .expect("pve IPAM status after the delete");
    assert!(
        !held
            .iter()
            .any(|e| e.get("ip").and_then(|v| v.as_str()) == Some("10.88.0.51")),
        "the reservation survived its delete: {held:?}"
    );

    // --- teardown, and the apply that makes the node forget all of it ---
    client
        .delete_sdn_subnet(&ledger, &vnet, &zone, cidr)
        .expect("delete the subnet");
    client
        .delete_sdn_vnet(&ledger, &vnet)
        .expect("delete the vnet");
    client
        .delete_sdn_zone(&ledger, &zone)
        .expect("delete the zone");
    client
        .delete_sdn_fabric_node(&ledger, &fabric, &node)
        .expect("remove the node from the fabric");
    client
        .delete_sdn_fabric(&ledger, &fabric)
        .expect("delete the fabric");
    client
        .apply_sdn(&ledger)
        .expect("apply the pending deletions");
    let applied = last_apply(&read_ledger());
    assert_eq!(
        applied.pointer("/state/state").and_then(|s| s.as_str()),
        Some("ok"),
        "{applied}"
    );
    assert!(
        !client
            .sdn_zones()
            .expect("zones after cleanup")
            .iter()
            .any(|z| z.get("zone").and_then(|v| v.as_str()) == Some(zone.as_str())),
        "the zone is still listed after delete+apply"
    );
    assert!(
        !client
            .sdn_fabrics()
            .expect("fabrics after cleanup")
            .iter()
            .any(|f| f.get("id").and_then(|v| v.as_str()) == Some(fabric.as_str())),
        "the fabric is still listed after delete+apply"
    );
}

/// `NetworkPolicy` `scope: vm` through the backend port (ADR-0052): the
/// engine's policy lands on the node's own per-VM firewall and reads back as
/// the same policy.
///
/// Runs one of two branches, decided by the CLUSTER, never flipped here:
/// with the datacenter firewall off, `apply_firewall` must refuse with
/// DX-6508 and leave the VM's firewall exactly as it was; with it on, the
/// whole cycle runs — the three switches, the rules in policy order above a
/// hand-made rule that must survive, a re-apply that replaces instead of
/// appending, and the other direction left alone.
#[test]
fn a_scope_vm_policy_lands_on_the_nodes_own_firewall_and_reads_back() {
    use delonix_compute::vm_firewall::{Direction, Policy, Proto, Rule};
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let ledger = delonix_proxmox::Ledger::at(vmdir);
    let stage = |_: CreateStage| {};

    let name = format!("dlxpol{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let client = b.client();
    let vm = delonix_compute::Vm::new(
        name,
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );
    let rule = |allow: bool, proto: Proto, port: Option<&str>, peer: Option<&str>| Rule {
        allow,
        proto,
        port: port.map(str::to_string),
        peer: peer.map(str::to_string),
    };
    let inbound = Policy {
        direction: Direction::In,
        default_allow: false,
        rules: vec![
            rule(true, Proto::Any, Some("53"), Some("10.0.0.0/8")),
            rule(true, Proto::Tcp, Some("22"), None),
            rule(false, Proto::Any, None, Some("192.168.1.0/24")),
        ],
    };

    let dc = client
        .cluster_firewall_options()
        .expect("datacenter firewall options");
    if !delonix_proxmox::vm_firewall::datacenter_enabled(&dc) {
        let err = b
            .apply_firewall(vmdir, &vm, &inbound)
            .expect_err("a datacenter firewall that is off must refuse the policy");
        assert!(
            matches!(err, delonix_model::Error::Coded { number: 6508, .. }),
            "expected DX-6508, got: {err}"
        );
        let opts = client.firewall_options(vmid).expect("firewall options");
        assert!(
            opts.get("enable").is_none() && opts.get("policy_in").is_none(),
            "the refusal touched the VM's firewall anyway: {opts}"
        );
        assert!(
            client.firewall_rules(vmid).expect("rules").is_empty(),
            "the refusal wrote rules anyway"
        );
    } else {
        // A rule the engine did not write: it must survive every apply.
        client
            .add_firewall_rule(
                &ledger,
                vmid,
                "in",
                "ACCEPT",
                &delonix_proxmox::FirewallRuleOpts {
                    enable: Some(true),
                    comment: Some("hand-made"),
                    source: None,
                    dest: None,
                    proto: Some("tcp"),
                    dport: Some("8006"),
                    sport: None,
                    iface: None,
                    macro_name: None,
                    rule_type: None,
                    action: None,
                },
            )
            .expect("a hand-made rule");

        b.apply_firewall(vmdir, &vm, &inbound)
            .expect("apply the inbound policy");
        assert_eq!(
            b.read_firewall(vmdir, &vm, Direction::In)
                .expect("read back"),
            inbound,
            "the node does not hold the policy that was applied"
        );

        // The three switches, read from the node, not from what apply said.
        let opts = client.firewall_options(vmid).expect("firewall options");
        assert_eq!(
            opts.get("enable").and_then(|v| v.as_u64()),
            Some(1),
            "{opts}"
        );
        assert_eq!(
            opts.get("policy_in").and_then(|v| v.as_str()),
            Some("DROP"),
            "{opts}"
        );
        let net0 = client.config(vmid).expect("config")["net0"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            net0.contains("firewall=1"),
            "net0 has no firewall switch: {net0}"
        );
        assert!(
            net0.contains(&boot.mac),
            "turning the NIC's firewall on changed its MAC: {net0} (was {})",
            boot.mac
        );

        // The engine's four node rules sit ABOVE the hand-made one (the first
        // match wins), and the hand-made one is still there.
        let rules = client.firewall_rules(vmid).expect("rules");
        let pos_of = |c: &str| {
            rules
                .iter()
                .filter(|r| r.get("comment").and_then(|x| x.as_str()) == Some(c))
                .filter_map(|r| r.get("pos").and_then(|p| p.as_u64()))
                .collect::<Vec<_>>()
        };
        let hand = pos_of("hand-made");
        assert_eq!(hand.len(), 1, "the hand-made rule is gone: {rules:?}");
        let managed: Vec<u64> = [
            "delonix-managed:0",
            "delonix-managed:1",
            "delonix-managed:2",
        ]
        .iter()
        .flat_map(|c| pos_of(c))
        .collect();
        assert_eq!(managed.len(), 4, "any+port is two node rules: {rules:?}");
        assert!(
            managed.iter().all(|p| *p < hand[0]),
            "a managed rule sits below the hand-made one: {rules:?}"
        );

        // Re-apply a different policy: it REPLACES the engine's rules.
        let narrower = Policy {
            direction: Direction::In,
            default_allow: false,
            rules: vec![rule(true, Proto::Tcp, Some("443"), None)],
        };
        b.apply_firewall(vmdir, &vm, &narrower)
            .expect("re-apply the inbound policy");
        assert_eq!(
            b.read_firewall(vmdir, &vm, Direction::In)
                .expect("read back"),
            narrower
        );
        let rules = client.firewall_rules(vmid).expect("rules");
        assert_eq!(
            rules.len(),
            2,
            "a re-apply must leave one managed rule and the hand-made one: {rules:?}"
        );

        // The other direction, and the first one untouched by it.
        let outbound = Policy {
            direction: Direction::Out,
            default_allow: true,
            rules: vec![rule(false, Proto::Tcp, Some("25"), None)],
        };
        b.apply_firewall(vmdir, &vm, &outbound)
            .expect("apply the outbound policy");
        assert_eq!(
            b.read_firewall(vmdir, &vm, Direction::Out)
                .expect("read back"),
            outbound
        );
        assert_eq!(
            b.read_firewall(vmdir, &vm, Direction::In)
                .expect("read back"),
            narrower,
            "an egress policy changed the ingress one"
        );
    }

    b.stop(vmdir, &vm).expect("stop");
    b.destroy(vmdir, &vm).expect("destroy");
    assert!(
        client.config(vmid).is_err(),
        "the VM is still defined on the node after destroy — an orphan"
    );
}

/// The power operations beyond start/stop, each asserted from what the node
/// reports afterwards (`GET …/status/current`), never from the call's answer:
///
/// - `pause`/`unpause` (the backend's, i.e. `vm pause`) are the node's
///   `…/status/suspend`/`…/status/resume`. A suspended VM still answers
///   `status: running` — only `qmpstatus` says `paused`, so that is what is
///   asserted, and `is_running` has to keep answering true for it (the
///   engine's record says `Paused`, not gone).
/// - `reset` leaves the VM running.
/// - `reboot` and a plain `shutdown` of a guest with NO operating system —
///   this case's 1 GiB empty disk ignores ACPI — FAIL after their timeout,
///   and the VM is still running: «asked and it did not go down» is an
///   error, never a success.
/// - `shutdown` with `force_stop` stops it anyway.
///
/// The ledger is read at the end: every task that was meant to succeed did,
/// and the only failures are the two the guest caused.
#[test]
fn power_operations_round_trip_through_the_node() {
    // No SKIP line: a print in a library crate's tests is counted debt, and
    // the sibling cases already say it.
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let stage = |_: CreateStage| {};

    let name = format!("dlxpower{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let vm = delonix_compute::Vm::new(
        name.clone(),
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );
    let client = b.client();
    let ledger = delonix_proxmox::Ledger::at(vmdir);
    let state = || client.power_state(vmid).expect("status/current");

    b.pause(vmdir, &vm).expect("pause (…/status/suspend)");
    let s = state();
    assert_eq!(
        s.status, "running",
        "a suspended VM still reads running: {s:?}"
    );
    assert!(
        s.is_paused(),
        "qmpstatus must say paused after a suspend: {s:?}"
    );
    assert!(
        b.is_running(&vm),
        "a paused VM is not gone — the engine keeps its record as Paused"
    );

    b.unpause(vmdir, &vm).expect("unpause (…/status/resume)");
    let s = state();
    assert!(
        s.status == "running" && !s.is_paused(),
        "running again after a resume: {s:?}"
    );

    client.reset(&ledger, vmid).expect("reset");
    assert_eq!(state().status, "running", "a reset leaves the VM running");

    let short = Some(std::time::Duration::from_secs(5));
    let err = client
        .reboot(&ledger, vmid, short)
        .expect_err("a guest with no OS ignores ACPI: the reboot must fail");
    assert!(
        matches!(err, delonix_proxmox::Error::TaskFailed(_)),
        "a task failure, not a transport error: {err:?}"
    );
    assert_eq!(
        state().status,
        "running",
        "a failed reboot leaves it running"
    );

    let err = client
        .shutdown(&ledger, vmid, short, false)
        .expect_err("a guest with no OS ignores ACPI: the shutdown must fail");
    assert!(
        err.to_string().contains("powerdown failed"),
        "the node's own reason is carried: {err}"
    );
    assert_eq!(
        state().status,
        "running",
        "a failed shutdown leaves it running"
    );

    client
        .shutdown(&ledger, vmid, short, true)
        .expect("forceStop pulls the plug after the timeout");
    let s = state();
    assert_eq!(s.status, "stopped", "{s:?}");
    assert!(!b.is_running(&vm));

    let entries: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(vmdir.join("proxmox-tasks.json")).expect("the ledger"),
    )
    .expect("ledger JSON");
    let state_of = |e: &serde_json::Value| {
        e.pointer("/state/state")
            .or_else(|| e.get("state"))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string()
    };
    for want in ["suspend", "resume", "reset"] {
        let last = entries
            .iter()
            .rev()
            .find(|e| e.get("action").and_then(|a| a.as_str()) == Some(want))
            .unwrap_or_else(|| panic!("no `{want}` task in the ledger: {entries:?}"));
        assert_eq!(state_of(last), "ok", "`{want}` did not succeed: {last}");
    }
    let failed: Vec<&str> = entries
        .iter()
        .filter(|e| state_of(e) == "failed")
        .filter(|e| {
            !e.pointer("/state/reason")
                .and_then(|r| r.as_str())
                .is_some_and(|r| r.contains("can't lock file"))
        })
        .filter_map(|e| e.get("action").and_then(|a| a.as_str()))
        .collect();
    assert_eq!(
        failed,
        ["reboot", "shutdown"],
        "only the two the guest caused may fail: {entries:?}"
    );

    b.destroy(vmdir, &vm).expect("destroy");
    assert!(
        client.config(vmid).is_err(),
        "the VM is still defined on the node after destroy — an orphan"
    );
}

/// `vm resize` on Proxmox (`vm.resize.cold`), asserted from what the node
/// records afterwards (`GET …/config`, `GET …/pending`), never from the
/// call's answer:
///
/// - a VM the NODE runs is refused with the engine's own
///   `ResizeNeedsStopped` (DX-5505), even though a record could say
///   `Stopped` — nothing is sent, the config keeps its old numbers;
/// - stopped, `resize_cold` sets cores, sockets and memory, and `pending`
///   is empty: the numbers are the ones the VM boots with;
/// - it boots with them: after `resume` the config still says so and the
///   node reports the VM's `cpus`/`maxmem` from the new definition.
#[test]
fn a_stopped_vm_is_resized_and_the_node_reads_back_the_new_size() {
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let stage = |_: CreateStage| {};

    let name = format!("dlxresize{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let vm = delonix_compute::Vm::new(
        name.clone(),
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );
    let client = b.client();
    let number_at = |v: &serde_json::Value, k: &str| -> Option<u64> {
        let x = v.get(k)?;
        x.as_u64()
            .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
    };

    let err = b
        .resize_cold(vmdir, &vm, 2, 768)
        .expect_err("the node runs it: a cold resize must be refused");
    assert_eq!(err.number(), 5505, "{err}");
    let c = client.config(vmid).expect("config");
    assert_eq!(
        (number_at(&c, "cores"), number_at(&c, "memory")),
        (Some(1), Some(512)),
        "a refused resize changed the config: {c}"
    );

    b.stop(vmdir, &vm).expect("stop");
    b.resize_cold(vmdir, &vm, 2, 768)
        .expect("resize a stopped VM");
    let c = client.config(vmid).expect("config");
    assert_eq!(
        (
            number_at(&c, "cores"),
            number_at(&c, "sockets"),
            number_at(&c, "memory")
        ),
        (Some(2), Some(1), Some(768)),
        "{c}"
    );
    let pending = client.pending(vmid).expect("pending");
    assert!(
        pending
            .iter()
            .all(|e| e.get("pending").is_none() && e.get("delete").is_none()),
        "a stopped VM's resize was left pending: {pending:?}"
    );

    b.resume(vmdir, &vm).expect("resume").expect("a started VM");
    let st = client.current(vmid).expect("status/current");
    assert_eq!(st.get("status").and_then(|s| s.as_str()), Some("running"));
    assert_eq!(
        number_at(&st, "cpus"),
        Some(2),
        "it booted with 2 vCPUs: {st}"
    );
    assert_eq!(
        number_at(&st, "maxmem"),
        Some(768 * 1024 * 1024),
        "it booted with 768 MiB: {st}"
    );

    b.destroy(vmdir, &vm).expect("destroy");
    assert!(
        client.config(vmid).is_err(),
        "the VM is still defined on the node after destroy — an orphan"
    );
}

/// `extraDisks`/`extraNics` on Proxmox (`vm.disks.extra`/`vm.nics.extra`),
/// asserted from the node's own config and storage, never from the call's
/// answer: the extra disks exist on the storage under this VM's id, in the
/// slots asked for and with the sizes asked for; the extra NICs carry the
/// model, the fixed MAC and the bridge asked for; and a destroy takes every
/// disk with it (`storage/.../content?content=images`), not only the boot one.
#[test]
fn extra_disks_and_nics_are_created_with_the_vm_and_go_with_it() {
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let stage = |_: CreateStage| {};

    let name = format!("dlxextra{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        extra_disks: vec![
            delonix_compute::ExtraDisk {
                source: format!("{storage}:1"),
                ..Default::default()
            },
            delonix_compute::ExtraDisk {
                source: format!("{storage}:2"),
                bus: "scsi".into(),
                ..Default::default()
            },
        ],
        extra_nics: vec![
            delonix_compute::ExtraNic::default(),
            delonix_compute::ExtraNic {
                kind: "bridge".into(),
                source: Some("vmbr0".into()),
                model: "e1000".into(),
                mac: Some("BC:24:11:0A:0B:0C".into()),
            },
        ],
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let vm = delonix_compute::Vm::new(
        name.clone(),
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );
    let client = b.client();

    let c = client.config(vmid).expect("config");
    let key = |k: &str| {
        c.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let owned = format!("{storage}:vm-{vmid}-disk-");
    assert!(
        key("virtio0").starts_with(&owned) && key("virtio0").contains("size=1G"),
        "virtio0: {c}"
    );
    assert!(
        key("scsi1").starts_with(&owned) && key("scsi1").contains("size=2G"),
        "scsi1: {c}"
    );
    assert!(
        key("net1").starts_with("virtio=") && key("net1").contains("bridge=vmbr0"),
        "net1: {c}"
    );
    assert!(
        key("net2").starts_with("e1000=BC:24:11:0A:0B:0C") && key("net2").contains("bridge=vmbr0"),
        "net2: {c}"
    );
    // The cloud-init drive (`vm-<id>-cloudinit`) is on the storage too —
    // every VM gets one for `ipconfig0` — so the disks are counted by name.
    let images = client.list_images(&storage, vmid).expect("storage content");
    let disks: Vec<&String> = images.iter().filter(|v| v.contains("-disk-")).collect();
    assert_eq!(disks.len(), 3, "boot + two extra disks: {images:?}");

    b.destroy(vmdir, &vm).expect("destroy");
    assert!(
        client.config(vmid).is_err(),
        "the VM is still defined on the node after destroy — an orphan"
    );
    let left = client.list_images(&storage, vmid).expect("storage content");
    assert!(
        left.is_empty(),
        "a destroy left disks on the storage: {left:?}"
    );
}

/// `vm cloud-init` on Proxmox (`vm.cloud-init`), asserted from the node's
/// OWN rendering of the drive (`…/cloudinit/dump?type=user`), never from the
/// call's answer:
///
/// - refused with the engine's DX-5506 while the node runs the VM, and the
///   rendering keeps the key the VM was created with;
/// - stopped, the change reaches the drive: the new hostname, the new user
///   and the new key are in the user-data, the old key is not, and
///   `…/cloudinit` has nothing pending.
#[test]
fn a_stopped_vms_cloud_init_is_changed_and_the_node_renders_it() {
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let stage = |_: CreateStage| {};

    // Real ed25519 public keys, generated for this test only (no private half
    // is kept): the node validates the key format and refuses an invented one.
    let old_key =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDoho0AhSdfKD3pWW/u4a2o3709J6q0Pl4kSE2rZ/B5Z dlx-old";
    let new_key =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFjdrrc1qEToezK50JsCHNdww+KDK9e0S4YQon8PsUys dlx-new";
    let name = format!("dlxci{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ssh_keys: vec![old_key.into()],
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let vm = delonix_compute::Vm::new(
        name.clone(),
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );
    let client = b.client();
    let intent = delonix_compute::vm_backend::CloudInitIntent {
        hostname: Some("dlx-renamed".into()),
        ci_user: Some("ops".into()),
        ssh_keys: vec![new_key.into()],
    };

    let err = b
        .update_cloud_init(vmdir, &vm, &intent)
        .expect_err("the node runs it: a cloud-init change must be refused");
    assert_eq!(err.number(), 5506, "{err}");
    let before = client.cloudinit_dump(vmid, "user").expect("dump user");
    assert!(
        before.contains(old_key),
        "a refused change touched the drive: {before}"
    );

    b.stop(vmdir, &vm).expect("stop");
    b.update_cloud_init(vmdir, &vm, &intent)
        .expect("cloud-init change on a stopped VM");
    let after = client.cloudinit_dump(vmid, "user").expect("dump user");
    assert!(after.contains("hostname: dlx-renamed"), "{after}");
    assert!(after.contains("user: ops"), "{after}");
    assert!(after.contains(new_key), "{after}");
    assert!(!after.contains(old_key), "the old key survived: {after}");
    let pending = client.cloudinit_pending(vmid).expect("cloudinit pending");
    assert!(
        pending.iter().all(|k| !k.is_pending()),
        "keys left pending after the change: {pending:?}"
    );

    b.destroy(vmdir, &vm).expect("destroy");
    assert!(
        client.config(vmid).is_err(),
        "the VM is still defined on the node after destroy — an orphan"
    );
}

/// `locate_vm` (ADR-0053 decision 3) against a real node: the cluster's
/// resource list places a VM this backend created on its node, and an id
/// nobody has is not placed anywhere. On a single node this proves the READ;
/// following a VM to ANOTHER node needs a second node and is not measured here.
#[test]
fn the_cluster_resource_list_places_a_vm_on_its_node() {
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let stage = |_: CreateStage| {};
    let name = format!("dlxloc{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let vm = delonix_compute::Vm::new(
        name.clone(),
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );
    let client = b.client();
    assert_eq!(
        client
            .locate_vm(vmid)
            .expect("cluster resources")
            .as_deref(),
        Some(t.node.as_str()),
        "the VM is listed on the node that created it"
    );
    assert_eq!(
        client.locate_vm(999_999).expect("cluster resources"),
        None,
        "an id nobody has is placed nowhere"
    );
    assert_eq!(b.current_handle(&vm), None, "nothing moved");
    b.destroy(vmdir, &vm).expect("destroy");
    assert!(
        client.config(vmid).is_err(),
        "the VM is still defined on the node after destroy — an orphan"
    );
}

/// The cluster half of the live cases: the node a VM moves to and a storage
/// every node shares (ADR-0053 decision 6 — a lab cluster, never production).
///
/// Both unset skips the two move cases, silently like the other cases here
/// (a print in a test is library-print debt, `scripts/arch_fitness.py`); the
/// run that promotes `vm.migration.*` sets both.
fn move_env() -> Option<(String, String)> {
    let to = std::env::var("DELONIX_PROXMOX_TEST_MOVE_NODE").ok()?;
    let shared = std::env::var("DELONIX_PROXMOX_TEST_SHARED_STORAGE").ok()?;
    Some((to, shared))
}

/// A record for what `boot` just made — the handle is what the engine keeps.
fn record_of(
    name: &str,
    cfg: &VmConfig,
    boot: &delonix_compute::vm_backend::Boot,
) -> delonix_compute::Vm {
    delonix_compute::Vm::new(
        name.to_string(),
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    )
}

/// `vm move --node` offline (`vm.migration.cold`, ADR-0053), asserted from the
/// cluster and not from the call's answer:
/// - refused before anything moves, with the class the ADR names: the node
///   the VM is on (DX-1538), a node that is not a member (DX-1538), a VM
///   running on the node when the move is offline (DX-5507), and a VM whose
///   disk is on a storage the target does not share (DX-5507, the volume
///   named) — and after each, the cluster still lists the VM where it was;
/// - a stopped VM on the shared storage moves: `/cluster/resources` lists it
///   on the target, its config is readable there, and the returned handle
///   names the target;
/// - the moved VM is then driven through its NEW handle — started and
///   stopped on the target, through the configured node's API (the
///   behaviour ADR-0053 marked "not measured": a request for
///   `/nodes/<other>/…` served for a cluster member).
#[test]
fn a_stopped_vm_moves_to_another_node_and_the_cluster_lists_it_there() {
    let Some(t) = target() else {
        return;
    };
    let Some((to, shared)) = move_env() else {
        return;
    };
    let local =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let stage = |_: CreateStage| {};
    let pid = std::process::id() % 10000;

    // A VM on LOCAL storage: the node's precheck lists its disk, and the move
    // is refused by name before the node is asked to copy anything.
    let lname = format!("dlxmvlocal{pid}");
    let lcfg = VmConfig {
        name: lname.clone(),
        disk: format!("{local}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let lboot = b
        .boot(vmdir, &lcfg, &lcfg.disk, &stage)
        .expect("boot local");
    let lvm = record_of(&lname, &lcfg, &lboot);
    b.stop(vmdir, &lvm).expect("stop local");
    let lvmid: u32 = lboot
        .api_socket
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let err = b
        .move_to_node(vmdir, &lvm, &to, &mv(false))
        .expect_err("a VM with a local disk must not move");
    assert_eq!(err.number(), 5507, "{err}");
    assert!(
        err.to_string().contains(&local),
        "the refusal names the volume: {err}"
    );
    assert!(
        err.to_string().contains("--with-local-disks"),
        "the refusal names the flag: {err}"
    );
    assert_eq!(
        client.locate_vm(lvmid).unwrap().as_deref(),
        Some(t.node.as_str())
    );

    // Asked for, the same move copies the disk — onto the shared storage,
    // named with `--target-storage` because the target may lack the source
    // one — and the VM's disk on the target lives there.
    let copy = delonix_compute::vm_backend::MoveOptions {
        with_local_disks: true,
        target_storage: Some(shared.clone()),
        ..Default::default()
    };
    let lhandle = b
        .move_to_node(vmdir, &lvm, &to, &copy)
        .expect("move copying the local disk");
    assert_eq!(lhandle, format!("proxmox:{to}:{lvmid}"));
    assert_eq!(
        client.locate_vm(lvmid).unwrap().as_deref(),
        Some(to.as_str()),
        "the cluster does not list the copied VM on the target"
    );
    let moved = client
        .for_node(&to)
        .unwrap()
        .config(lvmid)
        .expect("the config is readable on the target");
    let moved = serde_json::to_string(&moved).unwrap();
    assert!(
        moved.contains(&format!("{shared}:")) && !moved.contains(&format!("{local}:")),
        "the disk was not copied onto {shared}: {moved}"
    );
    let mut lvm = lvm;
    lvm.api_socket = lhandle;
    b.destroy(vmdir, &lvm).expect("destroy local");
    assert_eq!(client.locate_vm(lvmid).unwrap(), None, "an orphan was left");

    let name = format!("dlxmvcold{pid}");
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{shared}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let mut vm = record_of(&name, &cfg, &boot);
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();

    let err = b
        .move_to_node(vmdir, &vm, &t.node, &mv(false))
        .expect_err("same node");
    assert_eq!(err.number(), 1538, "{err}");
    let err = b
        .move_to_node(vmdir, &vm, "nosuchnode", &mv(false))
        .expect_err("not a member");
    assert_eq!(err.number(), 1538, "{err}");
    let err = b
        .move_to_node(vmdir, &vm, &to, &mv(false))
        .expect_err("it runs on the node: an offline move must be refused");
    assert_eq!(err.number(), 5507, "{err}");
    assert_eq!(
        client.locate_vm(vmid).unwrap().as_deref(),
        Some(t.node.as_str())
    );

    b.stop(vmdir, &vm).expect("stop");
    let handle = b
        .move_to_node(vmdir, &vm, &to, &mv(false))
        .expect("move offline");
    assert_eq!(handle, format!("proxmox:{to}:{vmid}"));
    assert_eq!(
        client.locate_vm(vmid).unwrap().as_deref(),
        Some(to.as_str()),
        "the cluster does not list the VM on the target"
    );
    client
        .for_node(&to)
        .unwrap()
        .config(vmid)
        .expect("the config is readable on the target");
    vm.api_socket = handle;

    b.resume(vmdir, &vm)
        .expect("start on the target")
        .expect("a started VM");
    assert!(b.is_running(&vm), "not running on the target after start");
    b.stop(vmdir, &vm).expect("stop on the target");
    assert!(!b.is_running(&vm));

    b.destroy(vmdir, &vm).expect("destroy");
    assert_eq!(client.locate_vm(vmid).unwrap(), None, "an orphan was left");
}

/// `vm move --node --live` (`vm.migration.live`, ADR-0053) on a disk every
/// node shares: the VM moves while running and is running on the target;
/// `--live` on a stopped VM is refused (DX-5507); and the VM moves BACK live
/// — the second move settles the ledger's first `qmigrate`, whose worker is
/// on the other node, through the task's own node (a status read aimed at
/// the wrong node answered "no such task").
#[test]
fn a_running_vm_moves_live_on_shared_storage_and_keeps_running() {
    let Some(t) = target() else {
        return;
    };
    let Some((to, shared)) = move_env() else {
        return;
    };
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let stage = |_: CreateStage| {};

    let name = format!("dlxmvlive{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{shared}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let mut vm = record_of(&name, &cfg, &boot);
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();

    b.stop(vmdir, &vm).expect("stop");
    let err = b
        .move_to_node(vmdir, &vm, &to, &mv(true))
        .expect_err("--live on a stopped VM");
    assert_eq!(err.number(), 5507, "{err}");
    b.resume(vmdir, &vm).expect("start").expect("a started VM");

    let handle = b
        .move_to_node(vmdir, &vm, &to, &mv(true))
        .expect("move live");
    assert_eq!(handle, format!("proxmox:{to}:{vmid}"));
    assert_eq!(
        client.locate_vm(vmid).unwrap().as_deref(),
        Some(to.as_str())
    );
    let st = client
        .for_node(&to)
        .unwrap()
        .status_current(vmid)
        .expect("status");
    assert_eq!(st, "running", "a live move must leave it running");
    vm.api_socket = handle;

    let back = b
        .move_to_node(vmdir, &vm, &t.node, &mv(true))
        .expect("move back live");
    assert_eq!(back, format!("proxmox:{}:{vmid}", t.node));
    assert_eq!(
        client.locate_vm(vmid).unwrap().as_deref(),
        Some(t.node.as_str())
    );
    assert_eq!(client.status_current(vmid).unwrap(), "running");
    vm.api_socket = back;

    let ledger = std::fs::read_to_string(vmdir.join("proxmox-tasks.json")).expect("the ledger");
    // At least two: a move the node's config lock turned away is retried,
    // and the ledger keeps both attempts (see `Client::task`).
    assert!(
        ledger.matches("\"migrate\"").count() >= 2,
        "two moves, at least two ledger entries: {ledger}"
    );
    assert!(
        !ledger.contains("\"submitted\""),
        "a move was left unsettled in the ledger: {ledger}"
    );

    b.destroy(vmdir, &vm).expect("destroy");
    assert_eq!(client.locate_vm(vmid).unwrap(), None, "an orphan was left");
}

/// `vm move --node --live --with-local-disks` (ADR-0053): a RUNNING VM whose
/// disk is on the source's local storage moves while it runs, the node
/// mirroring the disk over NBD onto `--target-storage`. Without the flag the
/// live move is refused by name (DX-5507) and the VM stays where it was;
/// with it, the VM is running on the target and its config names the target
/// storage, not the source one.
#[test]
fn a_running_vm_with_a_local_disk_moves_live_and_its_disk_is_mirrored() {
    let Some(t) = target() else {
        return;
    };
    let Some((to, shared)) = move_env() else {
        return;
    };
    let local =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();
    let stage = |_: CreateStage| {};

    let name = format!("dlxmvmirror{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{local}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b.boot(vmdir, &cfg, &cfg.disk, &stage).expect("boot");
    let mut vm = record_of(&name, &cfg, &boot);
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    assert!(b.is_running(&vm), "the VM must be running for a live move");

    let err = b
        .move_to_node(vmdir, &vm, &to, &mv(true))
        .expect_err("a live move of a local disk without the flag");
    assert_eq!(err.number(), 5507, "{err}");
    assert!(err.to_string().contains("--with-local-disks"), "{err}");
    assert_eq!(
        client.locate_vm(vmid).unwrap().as_deref(),
        Some(t.node.as_str())
    );

    let mirror = delonix_compute::vm_backend::MoveOptions {
        live: true,
        with_local_disks: true,
        target_storage: Some(shared.clone()),
    };
    let handle = b
        .move_to_node(vmdir, &vm, &to, &mirror)
        .expect("live move mirroring the local disk");
    assert_eq!(handle, format!("proxmox:{to}:{vmid}"));
    assert_eq!(
        client.locate_vm(vmid).unwrap().as_deref(),
        Some(to.as_str()),
        "the cluster does not list the VM on the target"
    );
    let on_target = client.for_node(&to).unwrap();
    assert_eq!(
        on_target.status_current(vmid).expect("status"),
        "running",
        "a live move must leave it running"
    );
    let moved = serde_json::to_string(&on_target.config(vmid).expect("config")).unwrap();
    assert!(
        moved.contains(&format!("{shared}:"))
            && !moved.contains(&format!("{local}:vm-{vmid}-disk")),
        "the disk was not mirrored onto {shared}: {moved}"
    );
    vm.api_socket = handle;

    b.destroy(vmdir, &vm).expect("destroy");
    assert_eq!(client.locate_vm(vmid).unwrap(), None, "an orphan was left");
}

/// The prepared agent guest (`DELONIX_PROXMOX_TEST_AGENT_VMID`) as a record
/// the backend can address: running, on the configured node.
fn agent_guest(t: &Target) -> Option<(u32, delonix_compute::Vm)> {
    let vmid: u32 = std::env::var("DELONIX_PROXMOX_TEST_AGENT_VMID")
        .ok()?
        .parse()
        .expect("DELONIX_PROXMOX_TEST_AGENT_VMID is not a number");
    let mut vm = delonix_compute::Vm::new(
        "agent-guest".into(),
        String::new(),
        String::new(),
        1,
        "768M".into(),
        String::new(),
        String::new(),
        String::new(),
        format!("proxmox:{}:{vmid}", t.node),
    );
    vm.status = delonix_model::records::Status::Running;
    Some((vmid, vm))
}

/// `vm describe`'s guest block (`vm.guest-agent`), from a real agent: the OS
/// and kernel, the hostname, the agent's version and the mounted filesystems
/// come back, and the hostname is the SAME one the guest's own
/// `/etc/hostname` holds, read through `agent/exec` — the block is the
/// guest's answer, not a field filled from anywhere else.
#[test]
fn the_guest_agent_reports_the_os_hostname_and_filesystems() {
    let Some(t) = target() else {
        return;
    };
    let Some((vmid, vm)) = agent_guest(&t) else {
        return;
    };
    let b = backend(&t).expect("connect");
    let g = b
        .guest_info(&vm)
        .expect("guest info")
        .expect("the prepared guest runs an agent");
    assert!(g.os.as_deref().is_some_and(|o| !o.is_empty()), "{g:?}");
    assert!(g.kernel.is_some() && g.agent_version.is_some(), "{g:?}");
    let root = g
        .filesystems
        .iter()
        .find(|f| f.mountpoint == "/")
        .unwrap_or_else(|| panic!("no root filesystem reported: {g:?}"));
    assert!(
        matches!((root.used_bytes, root.total_bytes), (Some(u), Some(t)) if u > 0 && u < t),
        "{root:?}"
    );
    let AgentExecStatus::Finished {
        exit_code, stdout, ..
    } = b
        .client()
        .agent_exec_wait(
            vmid,
            &["/bin/cat", "/etc/hostname"],
            std::time::Duration::from_secs(30),
        )
        .expect("agent exec")
    else {
        panic!("agent exec still running past its deadline");
    };
    assert_eq!(exit_code, 0);
    assert_eq!(g.hostname.as_deref(), Some(stdout.trim()), "{g:?}");
}

/// `vm.backup.quiesced`: a backup of the running agent guest is taken with
/// its filesystem frozen — proved from the node's own task log (the freeze
/// and the thaw) and the guest reporting `thawed` afterwards, and the archive
/// is on the storage. A running VM with no agent is refused with DX-6509
/// BEFORE any backup runs: its archive count does not change.
#[test]
fn a_backup_of_a_running_vm_is_taken_with_its_filesystem_frozen() {
    let Some(t) = target() else {
        return;
    };
    let Some((vmid, _)) = agent_guest(&t) else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let backups =
        std::env::var("DELONIX_PROXMOX_TEST_BACKUP_STORAGE").unwrap_or_else(|_| "local".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = delonix_proxmox::Ledger::at(dir.path());

    // A running VM without an agent: refused, nothing archived.
    let name = format!("dlxnoagent{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };
    let boot = b
        .boot(dir.path(), &cfg, &cfg.disk, &|_: CreateStage| {})
        .expect("boot");
    let bare: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    let before = client.list_backups(&backups, bare).expect("list").len();
    let err = client
        .backup_vm_quiesced(&ledger, bare, &backups)
        .expect_err("no agent: the backup cannot be quiesced");
    let err = delonix_model::Error::from(err);
    assert_eq!(err.number(), 6509, "{err}");
    assert_eq!(
        client.list_backups(&backups, bare).expect("list").len(),
        before
    );
    let vm = delonix_compute::Vm::new(
        name.clone(),
        cfg.disk.clone(),
        cfg.disk.clone(),
        1,
        "512M".into(),
        String::new(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );
    b.destroy(dir.path(), &vm)
        .expect("destroy the agentless VM");

    let before: Vec<String> = client
        .list_backups(&backups, vmid)
        .expect("list")
        .into_iter()
        .map(|(v, _)| v)
        .collect();
    client
        .backup_vm_quiesced(&ledger, vmid, &backups)
        .expect("a quiesced backup of the agent guest");
    let after: Vec<String> = client
        .list_backups(&backups, vmid)
        .expect("list")
        .into_iter()
        .map(|(v, _)| v)
        .collect();
    let new: Vec<&String> = after.iter().filter(|v| !before.contains(v)).collect();
    assert_eq!(new.len(), 1, "exactly one new archive: {after:?}");
    assert_eq!(
        client
            .fsfreeze_status(vmid)
            .expect("fsfreeze-status")
            .as_deref(),
        Some("thawed")
    );
    client
        .delete_backup(&ledger, vmid, &backups, new[0])
        .expect("delete the test archive");
}

/// A plain move: live or offline, no disk copy.
fn mv(live: bool) -> delonix_compute::vm_backend::MoveOptions {
    delonix_compute::vm_backend::MoveOptions {
        live,
        ..Default::default()
    }
}

/// Best-effort teardown for the SDN routing case when an assertion fails
/// halfway: the lab is shared, and a pending change or a held lock left by a
/// failed run would make every later run (and anyone else's SDN apply) fail
/// on it. Forcing the lock away is acceptable HERE only — a lab node this
/// run owns for its duration; nothing in the crate does it on its own.
struct SdnLabCleanup<'a> {
    client: &'a delonix_proxmox::Client,
    ledger: &'a delonix_proxmox::Ledger,
    controllers: [String; 2],
    route_map: String,
    prefix_list: String,
    vnet: String,
    zone: String,
    armed: bool,
}

impl Drop for SdnLabCleanup<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (c, l) = (self.client, self.ledger);
        let _ = c.force_release_sdn_lock(l);
        let _ = c.rollback_sdn(l, None);
        for rule in (0..4).rev() {
            let _ = c.delete_sdn_vnet_firewall_rule(l, &self.vnet, rule);
        }
        for ctl in &self.controllers {
            let _ = c.delete_sdn_controller(l, ctl);
        }
        let _ = c.delete_sdn_route_map_entry(l, &self.route_map, 10);
        let _ = c.delete_sdn_prefix_list(l, &self.prefix_list);
        let _ = c.delete_sdn_vnet(l, &self.vnet);
        let _ = c.delete_sdn_zone(l, &self.zone);
        let _ = c.apply_sdn(l);
    }
}

/// The routing layer of the cluster's own SDN, the vnet firewall, and the
/// global lock that makes an SDN change a transaction — every read back from
/// the node, never from what a call returned.
///
/// 1. The lock on its own: a write without the token is refused (DX-5515); a
///    rollback with the token releases it only because `release-lock=1` is
///    sent; staged work nobody applied makes the lock refuse (DX-5516), and a
///    rollback discards it.
/// 2. A transaction whose change fails (an EVPN controller naming a route map
///    that does not exist) is rolled back: nothing pending, the lock free.
/// 3. The chain prefix list → route-map entry → EVPN controller (+ a BGP
///    controller, a zone and a vnet) staged under the lock, the dry-run's FRR
///    diff carrying the chain, applied; then the RUNNING configuration holds
///    it, nothing is pending, the dry-run is empty, and the vnet is available.
/// 4. The vnet firewall: the three writes (options, add, update) refused
///    by the client with DX-1552 whatever their fields (`sdn_routing.rs`,
///    module doc), and the node read back unchanged — no rule, no forward
///    policy. The writes' own round trip was measured live on 2026-09-27
///    (`docs/proxmox/trace-9.2.2.routes`) before the refusal was decided.
/// 5. The teardown, under the lock again, and the node back as it was found.
#[test]
fn sdn_routing_chain_vnet_firewall_and_the_lock_round_trip_through_the_node() {
    use delonix_proxmox::{
        ControllerKind, ControllerOptions, FirewallRuleOpts, PrefixListEntry,
        PrefixListEntryUpdate, RouteMapClause, RouteMapEntry, RouteMapEntryUpdate, RoutingAction,
        VnetFirewallOptions,
    };
    let Some(t) = target() else {
        return;
    };
    let b = backend(&t).expect("connect");
    let shared = b.client();
    let client: &delonix_proxmox::Client = &shared;
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = delonix_proxmox::Ledger::at(dir.path());

    let s = std::process::id() % 1_000_000;
    let pl = format!("dlxsdnpl{s}");
    let rm = format!("dlxsdnrm{s}");
    let ev = format!("dlxsdnev{s}");
    let bg = format!("dlxsdnbg{s}");
    let zone = format!("ds{s}");
    let vnet = format!("dv{s}");
    // Peers: the entry node's own address (which the node skips when it
    // renders its own session) and TEST-NET-1 addresses, which no router
    // answers — FRR tries them and nothing else happens.
    let host = t
        .base_url
        .trim_start_matches("https://")
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string();
    let evpn_peers: Vec<&str> = if host.parse::<std::net::Ipv4Addr>().is_ok() {
        vec![host.as_str(), "192.0.2.10"]
    } else {
        vec!["192.0.2.10", "192.0.2.11"]
    };
    let bgp_peers = ["192.0.2.10"];

    assert!(
        client.sdn_pending_changes().expect("pending").is_empty(),
        "the node carries someone else's staged SDN changes; this run would apply them"
    );
    let mut cleanup = SdnLabCleanup {
        client,
        ledger: &ledger,
        controllers: [bg.clone(), ev.clone()],
        route_map: rm.clone(),
        prefix_list: pl.clone(),
        vnet: vnet.clone(),
        zone: zone.clone(),
        armed: true,
    };

    // --- the indexes ---------------------------------------------------------
    let ids: Vec<String> = client
        .sdn_index()
        .expect("GET /cluster/sdn")
        .iter()
        .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    for want in [
        "zones",
        "vnets",
        "controllers",
        "prefix-lists",
        "route-maps",
        "fabrics",
    ] {
        assert!(ids.iter().any(|i| i == want), "{want} missing from {ids:?}");
    }
    let subdirs: Vec<String> = client
        .sdn_fabrics_index()
        .expect("GET /cluster/sdn/fabrics")
        .iter()
        .filter_map(|e| e.get("subdir").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    assert_eq!(subdirs, ["fabric", "node", "all"], "{subdirs:?}");
    client
        .sdn_fabric_nodes_all(false)
        .expect("GET /cluster/sdn/fabrics/node");
    assert!(
        client.sdn_dry_run(None).expect("dry-run").is_empty(),
        "nothing pending, nothing to render"
    );

    // --- 1. the lock on its own ------------------------------------------------
    let tok = client.acquire_sdn_lock(false).expect("lock");
    let refused = client.create_sdn_zone(&ledger, &zone).unwrap_err();
    assert_eq!(
        refused.number(),
        5515,
        "a write without the token: {refused}"
    );
    client.release_sdn_lock(&ledger, &tok).expect("release");
    let tok = client
        .acquire_sdn_lock(false)
        .expect("the lock is free again");
    client
        .rollback_sdn(&ledger, Some(&tok))
        .expect("rollback with the token");
    let stale = client
        .acquire_sdn_lock(false)
        .expect("the rollback released the lock (release-lock=1)");
    client.release_sdn_lock(&ledger, &stale).expect("release");
    // Measured: releasing a lock nobody holds succeeds whatever the token — so
    // the stale token is tried while ANOTHER holder has the lock.
    let held = client.acquire_sdn_lock(false).expect("lock");
    let wrong = client
        .release_sdn_lock(&ledger, &stale)
        .expect_err("a token that is no longer the lock's");
    assert_eq!(wrong.number(), 5515, "{wrong}");
    client.release_sdn_lock(&ledger, &held).expect("release");

    client
        .create_sdn_zone(&ledger, &zone)
        .expect("a staged zone, outside any lock");
    let pending = client.sdn_pending_changes().expect("pending");
    assert!(
        pending
            .iter()
            .any(|p| p.kind == "zone" && p.id == zone && p.state == "new"),
        "{pending:?}"
    );
    let busy = client.acquire_sdn_lock(false).unwrap_err();
    assert_eq!(busy.number(), 5516, "pending work refuses the lock: {busy}");
    client
        .rollback_sdn(&ledger, None)
        .expect("rollback without a lock");
    assert!(
        client.sdn_pending_changes().expect("pending").is_empty(),
        "the rollback discarded the staged zone"
    );

    // --- 2. a failed transaction is rolled back ---------------------------------
    let failed = client
        .sdn_transaction(&ledger, || {
            client.create_sdn_prefix_list(&ledger, &pl, &[])?;
            client.create_sdn_controller(
                &ledger,
                &ev,
                ControllerKind::Evpn,
                &ControllerOptions {
                    asn: Some(65077),
                    peers: &evpn_peers,
                    route_map_in: Some(&rm),
                    ..Default::default()
                },
            )
        })
        .unwrap_err();
    assert!(
        failed.to_string().contains(&rm),
        "the node's own reason names the missing route map: {failed}"
    );
    assert!(
        client.sdn_pending_changes().expect("pending").is_empty(),
        "the staged prefix list was rolled back"
    );
    assert!(client.sdn_prefix_list(&pl).is_err(), "no prefix list left");
    let tok = client
        .acquire_sdn_lock(false)
        .expect("the rollback left the lock free");
    client.release_sdn_lock(&ledger, &tok).expect("release");

    // --- 3. the chain, staged and applied under the lock ------------------------
    let dry = client
        .sdn_transaction(&ledger, || {
            client.create_sdn_prefix_list(
                &ledger,
                &pl,
                &[
                    PrefixListEntry {
                        action: RoutingAction::Permit,
                        prefix: "10.77.0.0/16",
                        ge: None,
                        le: Some(24),
                        seq: Some(10),
                    },
                    PrefixListEntry {
                        action: RoutingAction::Deny,
                        prefix: "0.0.0.0/0",
                        ge: None,
                        le: None,
                        seq: Some(100),
                    },
                ],
            )?;
            client.add_sdn_prefix_list_entry(
                &ledger,
                &pl,
                &PrefixListEntry {
                    action: RoutingAction::Permit,
                    prefix: "10.78.0.0/16",
                    ge: None,
                    le: None,
                    seq: Some(20),
                },
            )?;
            client.update_sdn_prefix_list_entry(
                &ledger,
                &pl,
                20,
                &PrefixListEntryUpdate {
                    le: Some(24),
                    ..Default::default()
                },
            )?;
            let e20 = client.sdn_prefix_list_entry(&pl, 20)?;
            assert_eq!(e20.get("le").and_then(|v| v.as_u64()), Some(24), "{e20}");
            let entries = client.sdn_prefix_list_entries(&pl)?;
            assert_eq!(entries.len(), 3, "{entries:?}");

            client.create_sdn_route_map_entry(
                &ledger,
                &rm,
                10,
                &RouteMapEntry {
                    action: RoutingAction::Permit,
                    matches: &[RouteMapClause {
                        key: "ip-address-prefix-list",
                        value: Some(&pl),
                    }],
                    sets: &[RouteMapClause {
                        key: "local-preference",
                        value: Some("200"),
                    }],
                    call: None,
                    exit_action: None,
                },
            )?;
            client.update_sdn_route_map_entry(
                &ledger,
                &rm,
                10,
                &RouteMapEntryUpdate {
                    sets: Some(&[RouteMapClause {
                        key: "local-preference",
                        value: Some("300"),
                    }]),
                    ..Default::default()
                },
            )?;
            let e10 = client.sdn_route_map_entry(&rm, 10)?;
            assert_eq!(
                e10.pointer("/set/0").and_then(|v| v.as_str()),
                Some("key=local-preference,value=300"),
                "{e10}"
            );
            assert!(
                client
                    .sdn_route_map_entries(&rm)?
                    .iter()
                    .any(|e| e.get("order").and_then(|v| v.as_u64()) == Some(10)),
                "the map lists its entry"
            );
            assert!(
                client
                    .sdn_route_maps()?
                    .iter()
                    .any(|m| m.get("id").and_then(|v| v.as_str()) == Some(rm.as_str())),
                "the map exists while it has an entry"
            );

            client.create_sdn_controller(
                &ledger,
                &ev,
                ControllerKind::Evpn,
                &ControllerOptions {
                    asn: Some(65077),
                    peers: &evpn_peers,
                    route_map_in: Some(&rm),
                    ..Default::default()
                },
            )?;
            client.create_sdn_controller(
                &ledger,
                &bg,
                ControllerKind::Bgp,
                &ControllerOptions {
                    asn: Some(65077),
                    peers: &bgp_peers,
                    node: Some(&t.node),
                    ..Default::default()
                },
            )?;
            client.update_sdn_controller(
                &ledger,
                &ev,
                &ControllerOptions {
                    ebgp_multihop: Some(3),
                    ..Default::default()
                },
                &[],
            )?;
            // Stored and read back — and, measured, NOT rendered: both
            // controllers share one ASN, so the sessions are iBGP and FRR's
            // `ebgp-multihop` has nothing to apply to.
            let evc = client.sdn_controller(&ev)?;
            assert_eq!(
                evc.get("ebgp-multihop").and_then(|v| v.as_u64()),
                Some(3),
                "{evc}"
            );
            assert!(
                client
                    .sdn_controllers(false)?
                    .iter()
                    .any(|c| c.get("controller").and_then(|v| v.as_str()) == Some(bg.as_str())),
                "the bgp controller is staged"
            );

            client.create_sdn_zone(&ledger, &zone)?;
            client.create_sdn_vnet(&ledger, &vnet, &zone, None)?;

            let pending = client.sdn_pending_changes()?;
            for (kind, id) in [
                ("prefix-list", pl.as_str()),
                ("route-map-entry", rm.as_str()),
                ("controller", ev.as_str()),
                ("controller", bg.as_str()),
                ("zone", zone.as_str()),
                ("vnet", vnet.as_str()),
            ] {
                assert!(
                    pending.iter().any(|p| p.kind == kind && p.id == id),
                    "{kind} {id} not pending: {pending:?}"
                );
            }
            client.sdn_dry_run(None)
        })
        .expect("the staged chain, applied under the lock");
    let frr = dry.frr_diff.clone().unwrap_or_default();
    for want in [
        "+router bgp 65077".to_string(),
        format!("+ip prefix-list {pl} seq 10 permit 10.77.0.0/16 le 24"),
        format!("+ip prefix-list {pl} seq 20 permit 10.78.0.0/16 le 24"),
        format!("+ip prefix-list {pl} seq 100 deny 0.0.0.0/0"),
        format!("+route-map {rm} permit 10"),
        format!("+ match ip address prefix-list {pl}"),
        "+ set local-preference 300".to_string(),
        format!("+ call {rm}"),
        "+ neighbor 192.0.2.10 peer-group BGP".to_string(),
    ] {
        assert!(frr.contains(&want), "the dry-run misses `{want}`:\n{frr}");
    }

    // Applied: nothing pending, the rendered files are the pending config, and
    // the RUNNING configuration holds the chain.
    assert!(
        client.sdn_pending_changes().expect("pending").is_empty(),
        "nothing pending after the apply"
    );
    let after = client.sdn_dry_run(None).expect("dry-run");
    assert!(
        after.is_empty(),
        "the node's files already match: {after:?}"
    );
    let running: Vec<String> = client
        .sdn_controllers(true)
        .expect("running controllers")
        .iter()
        .filter_map(|c| {
            c.get("controller")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        running.contains(&ev) && running.contains(&bg),
        "{running:?}"
    );
    assert!(
        client
            .sdn_route_map_entries_all(true)
            .expect("running route maps")
            .iter()
            .any(|e| e.get("route-map-id").and_then(|v| v.as_str()) == Some(rm.as_str())),
        "the route map is running"
    );
    let content = client.sdn_zone_content(&zone).expect("zone content");
    assert!(
        content.iter().any(|v| {
            v.get("vnet").and_then(|x| x.as_str()) == Some(vnet.as_str())
                && v.get("status").and_then(|x| x.as_str()) == Some("available")
        }),
        "the vnet is realized: {content:?}"
    );
    let tok = client
        .acquire_sdn_lock(false)
        .expect("the apply released the lock (release-lock=1)");
    client.release_sdn_lock(&ledger, &tok).expect("release");

    // --- 4. the vnet firewall ----------------------------------------------------
    // Every write is refused by the client before it is sent (DX-1552): whether
    // a vnet rule filters anything is the per-node `nftables` option, under the
    // `/nodes/{node}/firewall` tree this engine does not read (ADR-0049 D3).
    // `sdn_vnet_firewall_index` is deliberately NOT called: measured live
    // 2026-10-09, that one route refuses an API token (403, `user != root@pam`)
    // — see its own doc comment.
    let ssh = FirewallRuleOpts {
        comment: Some("dlxsdn-ssh"),
        proto: Some("tcp"),
        dport: Some("22"),
        ..Default::default()
    };
    let refused = [
        client.set_sdn_vnet_firewall_options(
            &ledger,
            &vnet,
            &VnetFirewallOptions {
                enable: Some(true),
                policy_forward: Some("DROP"),
                ..Default::default()
            },
        ),
        client.add_sdn_vnet_firewall_rule(&ledger, &vnet, "ACCEPT", &ssh),
        client.update_sdn_vnet_firewall_rule(&ledger, &vnet, 0, &ssh, None),
    ];
    for r in refused {
        let e = r.unwrap_err();
        assert_eq!(e.number(), 1552, "{e}");
    }
    let rules = client.sdn_vnet_firewall_rules(&vnet).expect("rules");
    assert!(
        rules.is_empty(),
        "a refused write reached the node: {rules:?}"
    );
    let opts = client.sdn_vnet_firewall_options(&vnet).expect("options");
    assert!(opts.get("policy_forward").is_none(), "{opts}");

    // --- 5. the teardown, under the lock -----------------------------------------
    client
        .sdn_transaction(&ledger, || {
            client.update_sdn_prefix_list(
                &ledger,
                &pl,
                &[PrefixListEntry {
                    action: RoutingAction::Permit,
                    prefix: "10.79.0.0/16",
                    ge: None,
                    le: None,
                    seq: Some(30),
                }],
            )?;
            let left = client.sdn_prefix_list_entries(&pl)?;
            assert_eq!(
                left.len(),
                1,
                "a list update replaces the entries: {left:?}"
            );
            client.delete_sdn_prefix_list_entry(&ledger, &pl, 30)?;
            client.delete_sdn_controller(&ledger, &bg)?;
            client.delete_sdn_controller(&ledger, &ev)?;
            client.delete_sdn_route_map_entry(&ledger, &rm, 10)?;
            client.delete_sdn_prefix_list(&ledger, &pl)?;
            client.delete_sdn_vnet(&ledger, &vnet)?;
            client.delete_sdn_zone(&ledger, &zone)
        })
        .expect("the teardown, applied under the lock");
    assert!(
        client.sdn_pending_changes().expect("pending").is_empty(),
        "nothing pending after the teardown"
    );
    assert!(
        client.sdn_dry_run(None).expect("dry-run").is_empty(),
        "the node's files match the running config"
    );
    let running = client.sdn_controllers(true).expect("running controllers");
    assert!(
        !running.iter().any(|c| {
            let id = c.get("controller").and_then(|v| v.as_str());
            id == Some(ev.as_str()) || id == Some(bg.as_str())
        }),
        "{running:?}"
    );
    assert!(
        !client
            .sdn_prefix_lists()
            .expect("prefix lists")
            .iter()
            .any(|p| p.get("id").and_then(|v| v.as_str()) == Some(pl.as_str())),
        "the prefix list is gone"
    );
    let tok = client.acquire_sdn_lock(false).expect("the lock is free");
    client.release_sdn_lock(&ledger, &tok).expect("release");

    // Every staged write and every vnet firewall write answered inline: the
    // ledger holds the applies and nothing else.
    let ledger_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("proxmox-tasks.json")).expect("the ledger"),
    )
    .expect("json");
    let actions: std::collections::BTreeSet<&str> = ledger_json
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|e| e.get("action").and_then(|a| a.as_str()))
        .collect();
    assert_eq!(
        actions,
        std::collections::BTreeSet::from(["apply-sdn"]),
        "only the applies forked a task"
    );
    cleanup.armed = false;
}

/// ADR-0057: a VM boots from a LOCAL image (`DELONIX_PROXMOX_TEST_IMAGE`, a
/// qcow2 of the engine's store) with no template on the node. The image is
/// uploaded to the import storage named by its content (or found there from
/// an earlier run), the VM's boot disk is imported onto the disk storage and
/// grown to `diskSize`, and a second staging of the same image uploads
/// nothing. The node must have `import` enabled on the import storage.
#[test]
fn a_vm_boots_from_a_local_store_image_uploaded_and_imported() {
    let Some(t) = target() else {
        return;
    };
    let Ok(image) = std::env::var("DELONIX_PROXMOX_TEST_IMAGE") else {
        return;
    };
    let import = t.import_storage.clone().unwrap_or_else(|| "local".into());
    let disk = t.disk_storage.clone().unwrap_or_else(|| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let vmdir = dir.path();

    let name = format!("dlximport{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: image.clone(),
        vcpus: 1,
        memory: "512M".into(),
        disk_size_gib: Some(5),
        ..Default::default()
    };
    let boot = b
        .boot(vmdir, &cfg, &cfg.disk, &|_: CreateStage| {})
        .expect("boot from the local image");
    let vm = record_of(&name, &cfg, &boot);
    let vmid: u32 = boot.api_socket.rsplit(':').next().unwrap().parse().unwrap();
    assert!(b.is_running(&vm), "the imported VM is not running");

    let after = client.list_import_volumes(&import).expect("list");
    let ours: Vec<_> = after
        .iter()
        .filter(|v| v.starts_with(&format!("{import}:import/delonix-")))
        .collect();
    assert!(!ours.is_empty(), "no delonix image on {import}: {after:?}");
    let config = serde_json::to_string(&client.config(vmid).expect("config")).unwrap();
    assert!(
        config.contains(&format!("{disk}:vm-{vmid}-disk-0")) && config.contains("size=5G"),
        "the boot disk was not imported onto {disk} and grown to 5G: {config}"
    );

    // The same image again: nothing is uploaded.
    let again = client
        .stage_import(std::path::Path::new(&image), None)
        .expect("stage again");
    assert!(!again.uploaded, "the same image was uploaded a second time");
    assert!(after.contains(&again.volid), "{again:?} not in {after:?}");

    // The node verifies what it received: the same bytes announced with a
    // wrong sha256 fail the upload, and nothing is kept under that name.
    let bogus = format!("delonix-badsum{}.qcow2", std::process::id() % 10000);
    let err = client
        .upload(
            &import,
            delonix_proxmox::UploadContent::Import,
            std::path::Path::new(&image),
            &bogus,
            &"0".repeat(64),
        )
        .expect_err("a wrong checksum must fail the upload");
    let listed = client.list_import_volumes(&import).expect("list");
    assert!(
        !listed.contains(&format!("{import}:import/{bogus}")),
        "the node kept a file whose checksum did not match ({err}): {listed:?}"
    );

    b.destroy(vmdir, &vm).expect("destroy");
    assert_eq!(client.locate_vm(vmid).unwrap(), None, "an orphan was left");
}

/// ADR-0058 / plan 63 slice 2: a container archive is staged as `vztmpl`,
/// named by a manifest digest, with its sha256 for the node to check. The
/// node's `imgcopy` task must end OK and list it; a second staging uploads
/// nothing; and the same bytes announced with a wrong sha256 fail with the
/// node's «checksum mismatch», keeping nothing. The archive is a stand-in
/// (the node does not parse it on upload; slice 1 proved it parses the
/// engine's OCI archive on create). It stays on the storage, as the cache;
/// its bytes are fixed, so a later run takes the cached path.
#[test]
fn a_container_archive_is_staged_as_vztmpl_and_a_wrong_checksum_is_refused() {
    let Some(t) = target() else {
        return;
    };
    init_log();
    let storage = t.import_storage.clone().unwrap_or_else(|| "local".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = dir.path().join("archive.tar");
    // Fixed bytes, so every run names the same file: the first run uploads
    // it, later runs find it (the cache), and nothing piles up on the node.
    let payload = "delonix plan 63 slice 2 live archive".to_string();
    std::fs::write(&archive, payload.as_bytes()).unwrap();
    let digest = {
        use sha2::Digest;
        let hex: String = sha2::Sha256::digest(payload.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("sha256:{hex}")
    };

    let before = client
        .list_content(&storage, delonix_proxmox::UploadContent::Vztmpl)
        .expect("list");
    let staged = client
        .stage_template(&storage, &archive, &digest)
        .expect("stage the archive");
    assert_eq!(
        staged.uploaded,
        !before.contains(&staged.volid),
        "uploaded must mean «was not there before»: {staged:?}"
    );
    let listed = client
        .list_content(&storage, delonix_proxmox::UploadContent::Vztmpl)
        .expect("list");
    assert!(
        listed.contains(&staged.volid),
        "{staged:?} not in {listed:?}"
    );

    let again = client
        .stage_template(&storage, &archive, &digest)
        .expect("stage again");
    assert!(
        !again.uploaded,
        "the same archive was uploaded a second time"
    );
    assert_eq!(again.volid, staged.volid);

    let bogus = format!("dlx-badsum{}.tar", std::process::id() % 10000);
    let err = client
        .upload(
            &storage,
            delonix_proxmox::UploadContent::Vztmpl,
            &archive,
            &bogus,
            &"0".repeat(64),
        )
        .expect_err("a wrong checksum must fail the upload");
    assert!(
        err.to_string().contains("checksum mismatch"),
        "the refusal does not say why: {err}"
    );
    let listed = client
        .list_content(&storage, delonix_proxmox::UploadContent::Vztmpl)
        .expect("list");
    assert!(
        !listed.contains(&format!("{storage}:vztmpl/{bogus}")),
        "the node kept a file whose checksum did not match: {listed:?}"
    );
}

/// The manifest digest an OCI image layout archive's `index.json` points
/// at. A minimal ustar walk: this crate carries no tar reader, and the
/// archive is the engine's own (`write_oci_media_archive`).
fn oci_archive_manifest_digest(path: &std::path::Path) -> String {
    let bytes = std::fs::read(path).expect("read the OCI archive");
    let mut at = 0;
    while at + 512 <= bytes.len() {
        let header = &bytes[at..at + 512];
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let name_end = header[..100].iter().position(|b| *b == 0).unwrap_or(100);
        let name = std::str::from_utf8(&header[..name_end]).unwrap_or_default();
        let size_field = std::str::from_utf8(&header[124..136]).unwrap_or_default();
        let size =
            usize::from_str_radix(size_field.trim_matches(|c: char| c == '\0' || c == ' '), 8)
                .expect("tar size");
        let data = &bytes[at + 512..at + 512 + size];
        if name == "index.json" {
            let index: serde_json::Value = serde_json::from_slice(data).expect("index.json");
            return index["manifests"][0]["digest"]
                .as_str()
                .expect("a manifest digest")
                .to_string();
        }
        at += 512 + size.div_ceil(512) * 512;
    }
    panic!("no index.json in {}", path.display());
}

/// ADR-0058 / plan 63 slice 3: a system container from the engine's OCI
/// archive (`DELONIX_PROXMOX_TEST_OCI_ARCHIVE`, written by
/// `write_oci_media_archive` — e.g. `alpine:3.20`) runs its whole lifecycle
/// through the node. The entrypoint and environment asked for are the ones
/// the node kept; the start's network verdict is read from the interfaces
/// (the lab's `vmbr0` has no DHCP server, so `NotReady` with the node's
/// warning is the expected answer there); stop and destroy leave no container
/// and no volume; and the task ledger is read at the end.
#[test]
fn a_system_container_runs_its_lifecycle_through_the_node() {
    use delonix_compute::system_container::{
        NetworkState, SystemContainerNet, SystemContainerProvider, SystemContainerSpec,
    };
    let Some(t) = target() else {
        return;
    };
    let Ok(archive) = std::env::var("DELONIX_PROXMOX_TEST_OCI_ARCHIVE") else {
        return;
    };
    init_log();
    let archive = std::path::PathBuf::from(archive);
    let digest = oci_archive_manifest_digest(&archive);
    let template = t.import_storage.clone().unwrap_or_else(|| "local".into());
    let rootfs = t.disk_storage.clone().unwrap_or_else(|| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let provider =
        delonix_proxmox::ProxmoxSystemContainerProvider::new(client.clone(), &template, &rootfs);
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = SystemContainerSpec {
        name: format!("dlxct{}", std::process::id() % 10000),
        archive,
        manifest_digest: digest,
        entrypoint: vec!["/bin/sleep".into(), "3600".into()],
        env: vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("DLX_TEST".into(), "one two".into()),
        ],
        memory_mib: 256,
        swap_mib: 0,
        cores: 1,
        rootfs_gib: 1,
        network: Some(SystemContainerNet {
            bridge: t.bridge.clone().unwrap_or_else(|| "vmbr0".into()),
            vlan: None,
            dhcp: true,
        }),
        unprivileged: true,
    };

    let h = provider.create(dir.path(), &spec).expect("create");
    let vmid: u32 = h.locator.rsplit(':').next().unwrap().parse().unwrap();
    let config = client.lxc_config(vmid).expect("config");
    assert_eq!(config["entrypoint"], "/bin/sleep 3600", "{config}");
    assert_eq!(
        config["env"], "PATH=/usr/bin:/bin\u{0}DLX_TEST=one two",
        "{config}"
    );
    assert_eq!(config["unprivileged"], 1, "{config}");
    assert_eq!(
        client.lxc_status(vmid).unwrap(),
        "stopped",
        "create must not start it"
    );

    let obs = provider.start(dir.path(), &h, &spec).expect("start");
    assert!(obs.running, "{obs:?}");
    match &obs.network {
        NetworkState::Ready { ipv4 } => assert!(!ipv4.is_empty()),
        NetworkState::NotReady { reason } => assert!(!reason.is_empty(), "{obs:?}"),
        other => panic!("a DHCP network must be judged, got {other:?}"),
    }
    let again = provider.observe(dir.path(), &h, &spec).expect("observe");
    assert!(again.running);

    provider.stop(dir.path(), &h).expect("stop");
    assert_eq!(client.lxc_status(vmid).unwrap(), "stopped");
    provider.destroy(dir.path(), &h).expect("destroy");
    assert!(
        matches!(
            client.lxc_config(vmid),
            Err(delonix_proxmox::Error::NodeNotFound(_))
        ),
        "the container is still there"
    );
    let left: Vec<_> = client
        .list_ct_volumes(&rootfs, vmid)
        .expect("list")
        .into_iter()
        .collect();
    assert!(left.is_empty(), "a volume was left behind: {left:?}");

    // The ledger: every action's LAST task succeeded (a shutdown that timed
    // out against a `sleep` init may fail before the stop), nothing is still
    // submitted, and the start is there.
    let recs = delonix_proxmox::Ledger::at(dir.path()).records();
    assert!(
        !recs
            .iter()
            .any(|r| matches!(r.state, delonix_proxmox::TaskState::Submitted)),
        "{recs:?}"
    );
    let stopped_ok = recs
        .iter()
        .rev()
        .find(|r| r.action == "ct-stop")
        .map_or_else(
            || {
                recs.iter()
                    .rev()
                    .find(|r| r.action == "ct-shutdown")
                    .is_some_and(|r| r.state == delonix_proxmox::TaskState::Ok)
            },
            |r| r.state == delonix_proxmox::TaskState::Ok,
        );
    assert!(
        stopped_ok,
        "neither a shutdown nor a stop ended OK: {recs:?}"
    );
    for action in ["ct-create", "ct-start", "ct-destroy"] {
        let last = recs
            .iter()
            .rev()
            .find(|r| r.action == action)
            .unwrap_or_else(|| panic!("no {action} in the ledger: {recs:?}"));
        assert!(
            !matches!(last.state, delonix_proxmox::TaskState::Failed { .. }),
            "{action} failed: {last:?}"
        );
    }
}

/// Plan 63 slice 5, snapshots: a snapshot of a running container, a change
/// made after it (memory), and the rollback that undoes the change and
/// leaves the container running as it was. A taken name is a conflict
/// (DX-5503), a missing one not found (DX-4503), and `current` — the API's
/// pseudo-entry for the live state — is neither listed nor accepted.
#[test]
fn a_system_containers_snapshot_is_rolled_back_and_deleted() {
    use delonix_compute::system_container::{
        SystemContainerProvider, SystemContainerResources, SystemContainerSpec,
    };
    let Some(t) = target() else {
        return;
    };
    let Ok(archive) = std::env::var("DELONIX_PROXMOX_TEST_OCI_ARCHIVE") else {
        return;
    };
    init_log();
    let archive = std::path::PathBuf::from(archive);
    let digest = oci_archive_manifest_digest(&archive);
    let template = t.import_storage.clone().unwrap_or_else(|| "local".into());
    let rootfs = t.disk_storage.clone().unwrap_or_else(|| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let provider =
        delonix_proxmox::ProxmoxSystemContainerProvider::new(client.clone(), &template, &rootfs);
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = SystemContainerSpec {
        name: format!("dlxsnap{}", std::process::id() % 10000),
        archive,
        manifest_digest: digest,
        entrypoint: vec!["/bin/sleep".into(), "3600".into()],
        env: vec![],
        memory_mib: 256,
        swap_mib: 0,
        cores: 1,
        rootfs_gib: 1,
        network: None,
        unprivileged: true,
    };
    let h = provider.create(dir.path(), &spec).expect("create");
    let vmid: u32 = h.locator.rsplit(':').next().unwrap().parse().unwrap();
    provider.start(dir.path(), &h, &spec).expect("start");

    assert!(
        provider.snapshots(dir.path(), &h).unwrap().is_empty(),
        "`current` must not be listed"
    );
    provider.snapshot(dir.path(), &h, "s1").expect("snapshot");
    assert_eq!(
        provider.snapshots(dir.path(), &h).unwrap(),
        vec!["s1".to_string()]
    );
    let taken = provider.snapshot(dir.path(), &h, "s1").unwrap_err();
    assert_eq!(taken.number(), 5503, "{taken}");
    let current = provider.snapshot(dir.path(), &h, "current").unwrap_err();
    assert!(current.to_string().contains("current"), "{current}");

    provider
        .resize(
            dir.path(),
            &h,
            SystemContainerResources {
                memory_mib: 384,
                swap_mib: 0,
                cores: 1,
            },
        )
        .expect("resize");
    assert_eq!(client.lxc_config(vmid).unwrap()["memory"], 384);
    provider.restore(dir.path(), &h, "s1").expect("restore");
    assert_eq!(
        client.lxc_config(vmid).unwrap()["memory"],
        256,
        "the rollback undid the change"
    );
    assert_eq!(
        client.lxc_status(vmid).unwrap(),
        "running",
        "running before, running after"
    );
    let missing = provider.restore(dir.path(), &h, "nope").unwrap_err();
    assert_eq!(missing.number(), 4503, "{missing}");

    provider.stop(dir.path(), &h).expect("stop");
    provider
        .restore(dir.path(), &h, "s1")
        .expect("restore stopped");
    assert_eq!(
        client.lxc_status(vmid).unwrap(),
        "stopped",
        "stopped before, stopped after"
    );

    provider
        .delete_snapshot(dir.path(), &h, "s1")
        .expect("delete snapshot");
    assert!(provider.snapshots(dir.path(), &h).unwrap().is_empty());
    let gone = provider.delete_snapshot(dir.path(), &h, "s1").unwrap_err();
    assert_eq!(gone.number(), 4503, "{gone}");

    provider.destroy(dir.path(), &h).expect("destroy");
    let left = client.list_ct_volumes(&rootfs, vmid).expect("list");
    assert!(left.is_empty(), "a volume was left behind: {left:?}");
    let recs = delonix_proxmox::Ledger::at(dir.path()).records();
    for action in ["ct-snapshot", "ct-rollback", "ct-delete-snapshot"] {
        let last = recs
            .iter()
            .rev()
            .find(|r| r.action == action)
            .unwrap_or_else(|| panic!("no {action} in the ledger: {recs:?}"));
        assert_eq!(
            last.state,
            delonix_proxmox::TaskState::Ok,
            "{action}: {last:?}"
        );
    }
}

/// Plan 63 slice 5, resize: memory and the root volume of a RUNNING
/// container change without recreating it — the node grows the volume
/// (`PUT …/resize`) and never shrinks it, and a smaller size is refused
/// before any request (DX-1540), leaving the volume as it was.
#[test]
fn a_system_containers_root_volume_grows_live_and_never_shrinks() {
    use delonix_compute::system_container::{
        SystemContainerProvider, SystemContainerResources, SystemContainerSpec,
    };
    let Some(t) = target() else {
        return;
    };
    let Ok(archive) = std::env::var("DELONIX_PROXMOX_TEST_OCI_ARCHIVE") else {
        return;
    };
    init_log();
    let archive = std::path::PathBuf::from(archive);
    let digest = oci_archive_manifest_digest(&archive);
    let template = t.import_storage.clone().unwrap_or_else(|| "local".into());
    let rootfs = t.disk_storage.clone().unwrap_or_else(|| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let provider =
        delonix_proxmox::ProxmoxSystemContainerProvider::new(client.clone(), &template, &rootfs);
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = SystemContainerSpec {
        name: format!("dlxgrow{}", std::process::id() % 10000),
        archive,
        manifest_digest: digest,
        entrypoint: vec!["/bin/sleep".into(), "3600".into()],
        env: vec![],
        memory_mib: 256,
        swap_mib: 0,
        cores: 1,
        rootfs_gib: 1,
        network: None,
        unprivileged: true,
    };
    let h = provider.create(dir.path(), &spec).expect("create");
    let vmid: u32 = h.locator.rsplit(':').next().unwrap().parse().unwrap();
    provider.start(dir.path(), &h, &spec).expect("start");
    let cfg = provider.configuration(dir.path(), &h).unwrap().unwrap();
    assert_eq!(cfg.rootfs_gib, 1, "the create's size read back");

    provider
        .resize(
            dir.path(),
            &h,
            SystemContainerResources {
                memory_mib: 384,
                swap_mib: 128,
                cores: 2,
            },
        )
        .expect("resize");
    provider.grow_rootfs(dir.path(), &h, 2).expect("grow");
    let cfg = provider.configuration(dir.path(), &h).unwrap().unwrap();
    assert_eq!(
        (cfg.memory_mib, cfg.swap_mib, cfg.cores, cfg.rootfs_gib),
        (384, 128, 2, 2)
    );
    assert_eq!(
        client.lxc_status(vmid).unwrap(),
        "running",
        "still the same running container"
    );

    let shrink = provider.grow_rootfs(dir.path(), &h, 1).unwrap_err();
    assert_eq!(shrink.number(), 1540, "{shrink}");
    let cfg = provider.configuration(dir.path(), &h).unwrap().unwrap();
    assert_eq!(cfg.rootfs_gib, 2, "the refusal left the volume as it was");
    provider
        .grow_rootfs(dir.path(), &h, 2)
        .expect("the same size is a no-op");

    provider.stop(dir.path(), &h).expect("stop");
    provider.destroy(dir.path(), &h).expect("destroy");
    let left = client.list_ct_volumes(&rootfs, vmid).expect("list");
    assert!(left.is_empty(), "a volume was left behind: {left:?}");
    let recs = delonix_proxmox::Ledger::at(dir.path()).records();
    let grow = recs
        .iter()
        .rev()
        .find(|r| r.action == "resize")
        .unwrap_or_else(|| panic!("no resize in the ledger: {recs:?}"));
    assert_eq!(grow.state, delonix_proxmox::TaskState::Ok, "{grow:?}");
}

/// Plan 63 slice 5, firewall: a `scope: systemcontainer` policy lands on the
/// container's own firewall on the node — the NIC switch (`firewall=1` on
/// `net0`) and the container's `enable` turned on, the rules in order with the
/// default verdict last — reads back as it was written, and a second apply
/// leaves the same rules, not twice as many. A container with no network has
/// nothing to filter and is refused before any rule is written.
#[test]
fn a_system_containers_firewall_is_applied_and_reads_back() {
    use delonix_compute::system_container::{SystemContainerProvider, SystemContainerSpec};
    use delonix_compute::vm_firewall::{Direction, Policy, Proto, Rule};
    let Some(t) = target() else {
        return;
    };
    let Ok(archive) = std::env::var("DELONIX_PROXMOX_TEST_OCI_ARCHIVE") else {
        return;
    };
    init_log();
    let archive = std::path::PathBuf::from(archive);
    let digest = oci_archive_manifest_digest(&archive);
    let template = t.import_storage.clone().unwrap_or_else(|| "local".into());
    let rootfs = t.disk_storage.clone().unwrap_or_else(|| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let provider =
        delonix_proxmox::ProxmoxSystemContainerProvider::new(client.clone(), &template, &rootfs);
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = SystemContainerSpec {
        name: format!("dlxfw{}", std::process::id() % 10000),
        archive,
        manifest_digest: digest,
        entrypoint: vec!["/bin/sleep".into(), "3600".into()],
        env: vec![],
        memory_mib: 256,
        swap_mib: 0,
        cores: 1,
        rootfs_gib: 1,
        network: Some(delonix_compute::system_container::SystemContainerNet {
            bridge: std::env::var("DELONIX_PROXMOX_TEST_BRIDGE").unwrap_or_else(|_| "vmbr0".into()),
            vlan: None,
            dhcp: true,
        }),
        unprivileged: true,
    };
    let h = provider.create(dir.path(), &spec).expect("create");
    let vmid: u32 = h.locator.rsplit(':').next().unwrap().parse().unwrap();
    provider.start(dir.path(), &h, &spec).expect("start");
    let policy = Policy {
        direction: Direction::In,
        default_allow: false,
        rules: vec![
            Rule {
                allow: true,
                proto: Proto::Tcp,
                port: Some("22".into()),
                peer: Some("10.0.0.0/8".into()),
            },
            Rule {
                allow: false,
                proto: Proto::Udp,
                port: Some("53".into()),
                peer: None,
            },
        ],
    };
    provider
        .apply_firewall(dir.path(), &h, &policy)
        .expect("apply");
    let back = provider
        .read_firewall(dir.path(), &h, Direction::In)
        .expect("read");
    assert_eq!(
        back, policy,
        "the policy reads back as it was written, in order"
    );
    let opts = client.lxc_firewall_options(vmid).unwrap();
    assert_eq!(
        opts.get("enable").and_then(|v| v.as_u64()),
        Some(1),
        "{opts}"
    );
    let raw = client.lxc_config(vmid).unwrap();
    let net0 = raw.get("net0").and_then(|v| v.as_str()).unwrap_or_default();
    assert!(net0.split(',').any(|p| p == "firewall=1"), "{net0}");
    let managed = client.lxc_firewall_rules(vmid).unwrap().len();
    provider
        .apply_firewall(dir.path(), &h, &policy)
        .expect("apply again");
    assert_eq!(
        client.lxc_firewall_rules(vmid).unwrap().len(),
        managed,
        "a second apply doubled the rules"
    );

    let mut bare_spec = spec.clone();
    bare_spec.name = format!("{}n", spec.name);
    bare_spec.network = None;
    let bare_dir = tempfile::tempdir().expect("tempdir");
    let bare = provider
        .create(bare_dir.path(), &bare_spec)
        .expect("create bare");
    let e = provider
        .apply_firewall(bare_dir.path(), &bare, &policy)
        .unwrap_err();
    assert_eq!(e.number(), 1540, "{e}");
    let bare_vmid: u32 = bare.locator.rsplit(':').next().unwrap().parse().unwrap();
    assert!(
        client.lxc_firewall_rules(bare_vmid).unwrap().is_empty(),
        "rules written on a container with no network"
    );
    provider
        .destroy(bare_dir.path(), &bare)
        .expect("destroy bare");
    assert!(client
        .list_ct_volumes(&rootfs, bare_vmid)
        .unwrap()
        .is_empty());

    provider.stop(dir.path(), &h).expect("stop");
    provider.destroy(dir.path(), &h).expect("destroy");
    let left = client.list_ct_volumes(&rootfs, vmid).expect("list");
    assert!(left.is_empty(), "a volume was left behind: {left:?}");
    let recs = delonix_proxmox::Ledger::at(dir.path()).records();
    // The firewall writes answer inline (no UPID), so they leave nothing in
    // the ledger; what must hold there is that no task was left unfinished.
    assert!(
        recs.iter()
            .all(|r| !matches!(r.state, delonix_proxmox::TaskState::Submitted)),
        "{recs:?}"
    );
}

/// Plan 63 slice 5, clone: a RUNNING container is copied in full under a new
/// name — the node refuses a full copy of a running container without a
/// snapshot, so the provider takes a temporary one and deletes it. The copy
/// is created stopped with the source's configuration, the source keeps
/// running, and a name the node cannot take is refused before any request.
#[test]
fn a_running_system_container_is_cloned_from_a_temporary_snapshot() {
    use delonix_compute::system_container::{SystemContainerProvider, SystemContainerSpec};
    let Some(t) = target() else {
        return;
    };
    let Ok(archive) = std::env::var("DELONIX_PROXMOX_TEST_OCI_ARCHIVE") else {
        return;
    };
    init_log();
    let archive = std::path::PathBuf::from(archive);
    let digest = oci_archive_manifest_digest(&archive);
    let template = t.import_storage.clone().unwrap_or_else(|| "local".into());
    let rootfs = t.disk_storage.clone().unwrap_or_else(|| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let provider =
        delonix_proxmox::ProxmoxSystemContainerProvider::new(client.clone(), &template, &rootfs);
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = SystemContainerSpec {
        name: format!("dlxcln{}", std::process::id() % 10000),
        archive,
        manifest_digest: digest,
        entrypoint: vec!["/bin/sleep".into(), "3600".into()],
        env: vec![],
        memory_mib: 256,
        swap_mib: 0,
        cores: 1,
        rootfs_gib: 1,
        network: None,
        unprivileged: true,
    };
    let h = provider.create(dir.path(), &spec).expect("create");
    let vmid: u32 = h.locator.rsplit(':').next().unwrap().parse().unwrap();
    provider.start(dir.path(), &h, &spec).expect("start");
    let bad = provider
        .clone_as(dir.path(), &h, "not a name", None)
        .unwrap_err();
    assert_eq!(bad.number(), 1540, "{bad}");

    let copy_name = format!("{}c", spec.name);
    let copy = provider
        .clone_as(dir.path(), &h, &copy_name, None)
        .expect("clone");
    let copy_vmid: u32 = copy.locator.rsplit(':').next().unwrap().parse().unwrap();
    assert_ne!(copy_vmid, vmid);
    assert_eq!(
        client.lxc_status(vmid).unwrap(),
        "running",
        "the source keeps running"
    );
    assert_eq!(
        client.lxc_status(copy_vmid).unwrap(),
        "stopped",
        "the copy is created stopped"
    );
    assert!(
        provider.snapshots(dir.path(), &h).unwrap().is_empty(),
        "the temporary snapshot was left on the source"
    );
    let src_cfg = provider.configuration(dir.path(), &h).unwrap().unwrap();
    let copy_dir = tempfile::tempdir().expect("tempdir");
    let copy_cfg = provider
        .configuration(copy_dir.path(), &copy)
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            copy_cfg.memory_mib,
            copy_cfg.cores,
            copy_cfg.rootfs_gib,
            &copy_cfg.entrypoint
        ),
        (
            src_cfg.memory_mib,
            src_cfg.cores,
            src_cfg.rootfs_gib,
            &src_cfg.entrypoint
        )
    );
    let raw = client.lxc_config(copy_vmid).unwrap();
    assert_eq!(
        raw.get("hostname").and_then(|v| v.as_str()),
        Some(copy_name.as_str())
    );

    provider
        .destroy(copy_dir.path(), &copy)
        .expect("destroy the copy");
    let left = client.list_ct_volumes(&rootfs, copy_vmid).expect("list");
    assert!(left.is_empty(), "the copy left a volume behind: {left:?}");

    provider.stop(dir.path(), &h).expect("stop");
    provider.destroy(dir.path(), &h).expect("destroy");
    let left = client.list_ct_volumes(&rootfs, vmid).expect("list");
    assert!(left.is_empty(), "a volume was left behind: {left:?}");
    let recs = delonix_proxmox::Ledger::at(dir.path()).records();
    for action in ["ct-snapshot", "ct-clone", "ct-delete-snapshot"] {
        let r = recs
            .iter()
            .rev()
            .find(|r| r.action == action)
            .unwrap_or_else(|| panic!("no {action} in the ledger: {recs:?}"));
        assert_eq!(r.state, delonix_proxmox::TaskState::Ok, "{r:?}");
    }
}

/// Plan 63 slice 5, move: a container moves to another node of the cluster
/// (`DELONIX_PROXMOX_TEST_MOVE_NODE`). The node cannot move a running
/// container live, and its own restart move aborts when the init ignores
/// SIGTERM — the test's init is a bare `sleep`, which does — so a running one
/// is refused without `restart`, and with it the provider stops it, moves it
/// offline and starts it on the target. A root volume on `local-lvm`, which
/// the cluster does not share, is refused without `with_local_disks`; every
/// refusal leaves the container where and as it was.
#[test]
fn a_system_container_moves_to_another_node_offline_and_by_restart() {
    use delonix_compute::system_container::{
        SystemContainerMoveOptions, SystemContainerProvider, SystemContainerSpec,
    };
    let Some(t) = target() else {
        return;
    };
    let Ok(archive) = std::env::var("DELONIX_PROXMOX_TEST_OCI_ARCHIVE") else {
        return;
    };
    let Ok(to) = std::env::var("DELONIX_PROXMOX_TEST_MOVE_NODE") else {
        return;
    };
    init_log();
    let archive = std::path::PathBuf::from(archive);
    let digest = oci_archive_manifest_digest(&archive);
    let template = t.import_storage.clone().unwrap_or_else(|| "local".into());
    let rootfs = t.disk_storage.clone().unwrap_or_else(|| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let there = client.for_node(&to).expect("client for the target node");
    let provider =
        delonix_proxmox::ProxmoxSystemContainerProvider::new(client.clone(), &template, &rootfs);
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = SystemContainerSpec {
        name: format!("dlxmov{}", std::process::id() % 10000),
        archive,
        manifest_digest: digest,
        entrypoint: vec!["/bin/sleep".into(), "3600".into()],
        env: vec![],
        memory_mib: 256,
        swap_mib: 0,
        cores: 1,
        rootfs_gib: 1,
        network: None,
        unprivileged: true,
    };
    let h = provider.create(dir.path(), &spec).expect("create");
    let vmid: u32 = h.locator.rsplit(':').next().unwrap().parse().unwrap();
    let source = t.node.clone();
    provider.start(dir.path(), &h, &spec).expect("start");
    let here = |why: &str| {
        assert_eq!(
            client.locate_ct(vmid).unwrap().as_deref(),
            Some(source.as_str()),
            "{why}: the container left the source"
        );
        assert_eq!(client.lxc_status(vmid).unwrap(), "running", "{why}");
    };

    let full = SystemContainerMoveOptions {
        restart: true,
        with_local_disks: true,
        target_storage: None,
    };
    let e = provider
        .move_to(dir.path(), &h, &source, &full)
        .unwrap_err();
    assert_eq!(e.number(), 1540, "the node it is on: {e}");
    let e = provider
        .move_to(dir.path(), &h, "nosuchnode", &full)
        .unwrap_err();
    assert_eq!(e.number(), 1540, "a node outside the cluster: {e}");
    let e = provider
        .move_to(
            dir.path(),
            &h,
            &to,
            &SystemContainerMoveOptions {
                restart: false,
                ..full.clone()
            },
        )
        .unwrap_err();
    assert_eq!(e.number(), 5517, "running without restart: {e}");
    here("refused without restart");
    let e = provider
        .move_to(
            dir.path(),
            &h,
            &to,
            &SystemContainerMoveOptions {
                with_local_disks: false,
                ..full.clone()
            },
        )
        .unwrap_err();
    assert_eq!(
        e.number(),
        5517,
        "a local volume without with_local_disks: {e}"
    );
    assert!(
        e.to_string().contains("rootfs="),
        "the volume is named: {e}"
    );
    here("refused without with_local_disks");

    let moved = provider
        .move_to(dir.path(), &h, &to, &full)
        .expect("move by restart");
    assert_eq!(moved.locator, format!("proxmox:{to}:{vmid}"));
    assert_eq!(
        client.locate_ct(vmid).unwrap().as_deref(),
        Some(to.as_str())
    );
    assert_eq!(
        there.lxc_status(vmid).unwrap(),
        "running",
        "started on the target"
    );
    assert!(
        client.list_ct_volumes(&rootfs, vmid).unwrap().is_empty(),
        "the source kept a volume after the move"
    );
    // A record never updated after the move (a process that died between
    // the node's move and the record) still names the source: the provider
    // follows the container to where the cluster lists it instead of reading
    // it as gone — which would plan a second container.
    let stale = provider
        .configuration(dir.path(), &h)
        .expect("read through the stale locator");
    assert!(
        stale.is_some(),
        "the stale locator read the container as gone"
    );
    assert!(
        provider
            .observe(dir.path(), &h, &spec)
            .expect("observe through the stale locator")
            .running,
        "observed through the stale locator"
    );

    // Back, offline: a stopped container needs no restart.
    provider
        .stop(dir.path(), &moved)
        .expect("stop on the target");
    let back = provider
        .move_to(
            dir.path(),
            &moved,
            &source,
            &SystemContainerMoveOptions {
                restart: false,
                ..full.clone()
            },
        )
        .expect("move back offline");
    assert_eq!(back.locator, format!("proxmox:{source}:{vmid}"));
    assert_eq!(client.lxc_status(vmid).unwrap(), "stopped", "stays stopped");
    assert!(
        there.list_ct_volumes(&rootfs, vmid).unwrap().is_empty(),
        "the target kept a volume after the move back"
    );

    provider.destroy(dir.path(), &back).expect("destroy");
    let left = client.list_ct_volumes(&rootfs, vmid).expect("list");
    assert!(left.is_empty(), "a volume was left behind: {left:?}");
    let recs = delonix_proxmox::Ledger::at(dir.path()).records();
    let migrations: Vec<_> = recs.iter().filter(|r| r.action == "ct-migrate").collect();
    assert_eq!(migrations.len(), 2, "{recs:?}");
    assert!(
        migrations
            .iter()
            .all(|r| r.state == delonix_proxmox::TaskState::Ok),
        "{migrations:?}"
    );
}

/// Plan 63 slice 5, backup: an archive of a RUNNING container lands on the
/// node's backup storage without stopping it, a restore puts the container
/// back over itself and leaves it running, another container's archive is
/// refused before any request, and a delete removes only this archive.
#[test]
fn a_system_containers_backup_is_restored_over_it_and_deleted() {
    use delonix_compute::system_container::{
        SystemContainerProvider, SystemContainerResources, SystemContainerSpec,
    };
    let Some(t) = target() else {
        return;
    };
    let Ok(archive) = std::env::var("DELONIX_PROXMOX_TEST_OCI_ARCHIVE") else {
        return;
    };
    init_log();
    let archive = std::path::PathBuf::from(archive);
    let digest = oci_archive_manifest_digest(&archive);
    let template = t.import_storage.clone().unwrap_or_else(|| "local".into());
    let rootfs = t.disk_storage.clone().unwrap_or_else(|| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let provider =
        delonix_proxmox::ProxmoxSystemContainerProvider::new(client.clone(), &template, &rootfs);
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = SystemContainerSpec {
        name: format!("dlxbak{}", std::process::id() % 10000),
        archive,
        manifest_digest: digest,
        entrypoint: vec!["/bin/sleep".into(), "3600".into()],
        env: vec![],
        memory_mib: 256,
        swap_mib: 0,
        cores: 1,
        rootfs_gib: 1,
        network: None,
        unprivileged: true,
    };
    let h = provider.create(dir.path(), &spec).expect("create");
    let vmid: u32 = h.locator.rsplit(':').next().unwrap().parse().unwrap();
    provider.start(dir.path(), &h, &spec).expect("start");
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_BACKUP_STORAGE").unwrap_or_else(|_| "local".into());
    assert!(provider
        .backups(dir.path(), &h, &storage)
        .unwrap()
        .is_empty());
    let archive = provider
        .backup(dir.path(), &h, &storage, false)
        .expect("backup");
    assert!(
        archive.starts_with(&format!("{storage}:backup/vzdump-lxc-{vmid}-")),
        "{archive}"
    );
    assert_eq!(
        client.lxc_status(vmid).unwrap(),
        "running",
        "a snapshot backup does not stop it"
    );
    let listed = provider.backups(dir.path(), &h, &storage).unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].0, archive);
    assert!(listed[0].1 > 0, "an archive with no bytes");

    provider
        .resize(
            dir.path(),
            &h,
            SystemContainerResources {
                memory_mib: 384,
                swap_mib: 0,
                cores: 1,
            },
        )
        .expect("resize");
    let before_raw = client.lxc_config(vmid).unwrap();
    let dropped = provider
        .restore_backup(dir.path(), &h, &archive)
        .expect("restore");
    // By an API token the node drops the raw lxc.* keys (root@pam only); the
    // provider names each one it had before and not after.
    let had: Vec<String> = before_raw
        .get("lxc")
        .and_then(|l| l.as_array())
        .map(|l| {
            l.iter()
                .filter_map(|kv| Some(format!("{}: {}", kv[0].as_str()?, kv[1].as_str()?)))
                .collect()
        })
        .unwrap_or_default();
    let after_raw = client.lxc_config(vmid).unwrap();
    let kept = after_raw
        .get("lxc")
        .and_then(|l| l.as_array())
        .map(|l| l.len())
        .unwrap_or(0);
    assert_eq!(
        dropped.len() + kept,
        had.len(),
        "dropped {dropped:?}, had {had:?}"
    );
    for d in &dropped {
        assert!(had.contains(d), "{d} was never there");
    }
    let cfg = provider.configuration(dir.path(), &h).unwrap().unwrap();
    assert_eq!(
        cfg.memory_mib, 256,
        "the restore put the archived config back"
    );
    assert_eq!(
        cfg.entrypoint,
        vec!["/bin/sleep", "3600"],
        "the entrypoint came back with it"
    );
    assert_eq!(
        client.lxc_status(vmid).unwrap(),
        "running",
        "running before, running after"
    );

    let other = format!(
        "{storage}:backup/vzdump-lxc-{}-2026_01_01-00_00_00.tar.zst",
        vmid + 1
    );
    let refused = provider.restore_backup(dir.path(), &h, &other).unwrap_err();
    assert_eq!(refused.number(), 1540, "{refused}");
    let missing = format!("{storage}:backup/vzdump-lxc-{vmid}-2026_01_01-00_00_00.tar.zst");
    let e = provider
        .delete_backup(dir.path(), &h, &missing)
        .unwrap_err();
    assert_eq!(e.number(), 4504, "{e}");

    provider
        .delete_backup(dir.path(), &h, &archive)
        .expect("delete backup");
    assert!(provider
        .backups(dir.path(), &h, &storage)
        .unwrap()
        .is_empty());

    provider.stop(dir.path(), &h).expect("stop");
    provider.destroy(dir.path(), &h).expect("destroy");
    let left = client.list_ct_volumes(&rootfs, vmid).expect("list");
    assert!(left.is_empty(), "a volume was left behind: {left:?}");
    let recs = delonix_proxmox::Ledger::at(dir.path()).records();
    for action in ["backup", "ct-restore", "delete-backup"] {
        let r = recs
            .iter()
            .rev()
            .find(|r| r.action == action)
            .unwrap_or_else(|| panic!("no {action} in the ledger: {recs:?}"));
        // The restore warns when it drops the raw lxc.* keys, and that is a
        // finished task, not a failed one.
        assert!(
            matches!(
                r.state,
                delonix_proxmox::TaskState::Ok | delonix_proxmox::TaskState::OkWithWarnings { .. }
            ),
            "{r:?}"
        );
    }
}

/// Audit 62 §6 P1 / ADR-0059 D1.5 against the real cluster, through the
/// `SegmentProvider` the `kind: NetworkZone` apply uses: a vnet carries
/// the owner mark in its alias; another record's mark, or none, is refused
/// and never deleted; a vnet edited on the cluster is drift; and someone
/// else's staged change refuses the whole transaction before it writes,
/// leaving that change pending and not applied.
#[test]
fn network_zone_provider_owns_by_mark_and_never_pushes_someone_elses_pending_change() {
    use delonix_networking::ownership::{Owner, OwnerMark, RemoveOutcome};
    use delonix_networking::segment::{EnsureOutcome, NetworkZoneSpec, SegmentProvider, VNetSpec};

    let Some(t) = target() else {
        return;
    };
    let opts = delonix_proxmox::ClientOptions {
        trace_routes: std::env::var_os(delonix_proxmox::TRACE_ROUTES_ENV)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from),
        ..Default::default()
    };
    let client =
        std::sync::Arc::new(delonix_proxmox::Client::connect_with(&t, opts).expect("connect"));
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = delonix_proxmox::Ledger::at(dir.path());
    let provider = delonix_proxmox::ProxmoxSegmentProvider::new(
        client.clone(),
        delonix_proxmox::Ledger::at(dir.path()),
    );
    assert!(
        client.sdn_pending_changes().expect("pending").is_empty(),
        "the lab cluster must start with no pending SDN change"
    );

    let suffix = std::process::id() % 1_000_000;
    let zone = format!("o{suffix}");
    let vnet = format!("w{suffix}");
    let foreign = format!("f{suffix}");
    let owner = OwnerMark::new(&format!("dlx-live-{suffix:08}")).unwrap();
    let stranger = OwnerMark::new("dlx-stranger-0000").unwrap();
    let spec = VNetSpec {
        name: vnet.clone(),
        zone: zone.clone(),
        alias: Some("s6 live".into()),
    };
    let apply = |mark: &OwnerMark| {
        provider.transaction(&mut || {
            provider.ensure_zone(&NetworkZoneSpec { name: zone.clone() })?;
            provider.ensure_vnet(&spec, mark)?;
            Ok(())
        })
    };

    // 1. Created, marked, applied (running, not only pending).
    apply(&owner).expect("create and apply the zone and the vnet");
    let running = client.sdn_vnet(&vnet).expect("read the vnet");
    let alias = running["alias"].as_str().unwrap_or_default().to_string();
    assert!(
        alias.contains(&owner.tag()),
        "the vnet alias must carry the mark: {alias}"
    );
    assert!(
        client.sdn_pending_changes().unwrap().is_empty(),
        "applied, nothing pending"
    );

    // 2. The same record again: already present. (Nothing asserts INSIDE a
    // transaction: a panic there would leave the cluster's lock held.)
    let mut again = None;
    provider
        .transaction(&mut || {
            again = Some(provider.ensure_vnet(&spec, &owner)?);
            Ok(())
        })
        .expect("an unchanged second apply");
    assert_eq!(again, Some(EnsureOutcome::AlreadyPresent));

    // 3. Another record's mark: refused inside the transaction, rolled back.
    let e = apply(&stranger).unwrap_err();
    assert_eq!(e.number(), 5389, "{e}");
    assert!(
        e.to_string().contains("refusing to adopt it by name"),
        "{e}"
    );
    assert!(
        client.sdn_pending_changes().unwrap().is_empty(),
        "the refusal left nothing staged"
    );
    let mut left = None;
    provider
        .transaction(&mut || {
            left = Some(provider.remove_vnet(&vnet, &stranger)?);
            Ok(())
        })
        .expect("a stranger's teardown runs");
    assert_eq!(
        left,
        Some(RemoveOutcome::NotOwned(Owner::Other(
            owner.token().to_string()
        )))
    );
    assert!(
        client.sdn_vnet(&vnet).is_ok(),
        "a stranger's teardown must not delete it"
    );

    // 4. Someone else stages a zone and does not apply it: the next apply is
    //    refused before any write, and their change is still pending.
    client
        .create_sdn_zone(&ledger, &foreign)
        .expect("stage someone else's zone");
    let e = apply(&owner).unwrap_err();
    assert_eq!(e.number(), 5516, "{e}");
    let pending = client.sdn_pending_changes().unwrap();
    assert!(
        pending.iter().any(|p| p.id == foreign && p.state == "new"),
        "their zone must still be pending, not applied: {pending:?}"
    );
    client
        .rollback_sdn(&ledger, None)
        .expect("discard their staged zone");

    // 5. Edited on the cluster (the alias, keeping the mark): drift.
    client
        .sdn_transaction(&ledger, || {
            client.update_sdn_vnet(&ledger, &vnet, Some(&owner.stamp("edited by hand")))
        })
        .expect("edit the vnet by hand");
    let e = apply(&owner).unwrap_err();
    assert_eq!(e.number(), 5389, "{e}");
    assert!(e.to_string().contains("was changed on the cluster"), "{e}");

    // 6. Teardown by the owner: vnet then zone, one transaction.
    let mut removed = None;
    provider
        .transaction(&mut || {
            removed = Some(provider.remove_vnet(&vnet, &owner)?);
            provider.remove_zone(&zone)
        })
        .expect("tear down");
    assert_eq!(removed, Some(RemoveOutcome::Removed));
    assert!(
        !client
            .sdn_zones()
            .unwrap()
            .iter()
            .any(|z| z["zone"].as_str() == Some(zone.as_str())),
        "the zone must be gone"
    );
    assert!(client.sdn_pending_changes().unwrap().is_empty());
}

/// ADR-0059 F5b: the IPAM role, end to end through the node. A zone the
/// segment provider creates gets `ipam=pve` and `dhcp=dnsmasq` inside the
/// same transaction as its vnet and subnet (with a DHCP range); a reservation
/// for a system container's MAC is made once the subnet is running, read back,
/// and refused for another MAC; and the container, started on the vnet, gets
/// the RESERVED address by DHCP — the per-zone `dnsmasq` serves only the MACs
/// in its `ethers` file, and the node writes the address the IPAM holds for
/// that MAC there when the guest starts. The teardown releases the
/// reservation before the subnet (the node refuses a subnet that still holds
/// one), and leaves no zone and no IPAM entry.
#[test]
fn the_ipam_provider_reserves_an_address_and_a_guest_gets_it_by_dhcp() {
    use delonix_compute::system_container::{
        NetworkState, SystemContainerNet, SystemContainerProvider, SystemContainerSpec,
    };
    use delonix_networking::ipam::{
        ipam_drift, DhcpRange, IpamProvider, IpamReservation, IpamSubnet,
    };
    use delonix_networking::ownership::OwnerMark;
    use delonix_networking::segment::{EnsureOutcome, NetworkZoneSpec, SegmentProvider, VNetSpec};
    let Some(t) = target() else {
        return;
    };
    let Ok(archive) = std::env::var("DELONIX_PROXMOX_TEST_OCI_ARCHIVE") else {
        return;
    };
    let archive = std::path::PathBuf::from(archive);
    let digest = oci_archive_manifest_digest(&archive);
    let template = t.import_storage.clone().unwrap_or_else(|| "local".into());
    let rootfs = t.disk_storage.clone().unwrap_or_else(|| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let segment = delonix_proxmox::ProxmoxSegmentProvider::new(
        client.clone(),
        delonix_proxmox::Ledger::at(dir.path()),
    );
    let ipam = delonix_proxmox::ProxmoxIpamProvider::new(
        client.clone(),
        delonix_proxmox::Ledger::at(dir.path()),
    );
    let owner = OwnerMark::from_random(
        &std::process::id()
            .to_be_bytes()
            .repeat(4)
            .try_into()
            .unwrap(),
    );

    let suffix = std::process::id() % 1_000_000;
    let zone = format!("i{suffix}");
    let vnet = format!("j{suffix}");
    let octet = suffix % 200 + 20;
    let subnet = IpamSubnet {
        vnet: vnet.clone(),
        cidr: format!("10.79.{octet}.0/24"),
        gateway: Some(format!("10.79.{octet}.1")),
        dhcp_ranges: vec![DhcpRange {
            start: format!("10.79.{octet}.100"),
            end: format!("10.79.{octet}.150"),
        }],
    };

    segment
        .transaction(&mut || {
            assert_eq!(
                segment.ensure_zone(&NetworkZoneSpec { name: zone.clone() })?,
                EnsureOutcome::Created
            );
            segment.ensure_vnet(
                &VNetSpec {
                    name: vnet.clone(),
                    zone: zone.clone(),
                    alias: Some("f5b".into()),
                },
                &owner,
            )?;
            ipam.prepare_zone(&zone, "pve", true)?;
            assert_eq!(
                ipam.ensure_subnet(&zone, &subnet, &owner)?,
                EnsureOutcome::Created
            );
            Ok(())
        })
        .expect("the zone, its vnet and its subnet in one transaction");

    let observed = ipam
        .observe(&zone, std::slice::from_ref(&vnet))
        .expect("observe");
    assert!(observed.zone_ipam && observed.zone_dhcp, "{observed:?}");
    assert_eq!(observed.subnets, vec![subnet.clone()], "{observed:?}");

    let spec = SystemContainerSpec {
        name: format!("dlxip{}", suffix % 10000),
        archive,
        manifest_digest: digest,
        entrypoint: vec!["/bin/sleep".into(), "3600".into()],
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        memory_mib: 256,
        swap_mib: 0,
        cores: 1,
        rootfs_gib: 1,
        network: Some(SystemContainerNet {
            bridge: vnet.clone(),
            vlan: None,
            dhcp: true,
        }),
        unprivileged: true,
    };
    let ct =
        delonix_proxmox::ProxmoxSystemContainerProvider::new(client.clone(), &template, &rootfs);
    let ctdir = tempfile::tempdir().expect("tempdir");
    let h = ct.create(ctdir.path(), &spec).expect("create");
    let vmid: u32 = h.locator.rsplit(':').next().unwrap().parse().unwrap();
    let net0 = client.lxc_config(vmid).expect("config")["net0"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mac = net0
        .split(',')
        .find_map(|kv| kv.strip_prefix("hwaddr="))
        .unwrap_or_else(|| panic!("no hwaddr in {net0}"))
        .to_ascii_uppercase();

    // The node allocated an address of the range for the MAC at create.
    let at_create: Vec<String> = client
        .sdn_ipam_status("pve")
        .unwrap()
        .iter()
        .filter(|e| {
            e["zone"] == zone.as_str()
                && e["mac"]
                    .as_str()
                    .is_some_and(|m| m.eq_ignore_ascii_case(&mac))
        })
        .map(|e| e["ip"].as_str().unwrap_or_default().to_string())
        .collect();
    let reserved = IpamReservation {
        vnet: vnet.clone(),
        ip: format!("10.79.{octet}.20"),
        mac: mac.clone(),
    };
    assert_eq!(
        ipam.ensure_reservation(&zone, &reserved).expect("reserve"),
        EnsureOutcome::Created
    );
    assert_eq!(
        ipam.ensure_reservation(&zone, &reserved).expect("again"),
        EnsureOutcome::AlreadyPresent
    );
    let foreign = IpamReservation {
        mac: "BC:24:11:00:00:01".into(),
        ..reserved.clone()
    };
    let e = ipam
        .ensure_reservation(&zone, &foreign)
        .expect_err("another MAC on a held address");
    assert!(e.to_string().contains("already held"), "{e}");
    let of_mac: Vec<String> = client
        .sdn_ipam_status("pve")
        .unwrap()
        .iter()
        .filter(|e| {
            e["zone"] == zone.as_str()
                && e["mac"]
                    .as_str()
                    .is_some_and(|m| m.eq_ignore_ascii_case(&mac))
        })
        .map(|e| e["ip"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        of_mac,
        vec![reserved.ip.clone()],
        "the MAC holds exactly the reserved address (it held {at_create:?} at create)"
    );
    let observed = ipam
        .observe(&zone, std::slice::from_ref(&vnet))
        .expect("observe");
    assert!(
        ipam_drift(
            std::slice::from_ref(&subnet),
            std::slice::from_ref(&reserved),
            true,
            &observed
        )
        .is_empty(),
        "{observed:?}"
    );

    // The DHCP server is the node's own dnsmasq. With the datacenter
    // firewall on (the lab has it, ADR-0052) the node drops the requests
    // unless a rule lets udp/67 in: a lab precondition, like the
    // `management` IPSet (`IN ACCEPT -p udp -dport 67` on each node) — the
    // engine does not write the node's firewall (ADR-0049 D3) and only warns.
    let obs = ct.start(ctdir.path(), &h, &spec).expect("start");
    let got = match &obs.network {
        NetworkState::Ready { ipv4 } => ipv4.clone(),
        other => {
            // Read the lease once more: dnsmasq may answer after the start's
            // own wait.
            std::thread::sleep(std::time::Duration::from_secs(10));
            let again = ct.observe(ctdir.path(), &h, &spec).expect("observe");
            match again.network {
                NetworkState::Ready { ipv4 } => ipv4,
                _ => panic!("the guest got no address by DHCP: {other:?} then {again:?}"),
            }
        }
    };
    assert_eq!(got, reserved.ip, "the guest must get the reserved address");

    ct.stop(ctdir.path(), &h).expect("stop");
    ct.destroy(ctdir.path(), &h).expect("destroy");
    // Destroying the guest releases the address its MAC holds — the
    // reservation goes with the guest, which a plan then reads as missing.
    let after_destroy = ipam
        .observe(&zone, std::slice::from_ref(&vnet))
        .expect("observe");
    assert!(
        !after_destroy
            .entries
            .iter()
            .any(|e| e.mac.as_deref() == Some(mac.as_str())),
        "the guest's MAC still holds an address after its destroy: {after_destroy:?}"
    );
    assert_eq!(
        ipam.remove_reservation(&zone, &reserved).expect("release"),
        delonix_networking::ownership::RemoveOutcome::Absent
    );
    segment
        .transaction(&mut || {
            ipam.remove_subnet(&zone, &subnet, &owner)?;
            segment.remove_vnet(&vnet, &owner)?;
            segment.remove_zone(&zone)
        })
        .expect("teardown");
    assert!(
        !client
            .sdn_zones_running()
            .unwrap()
            .iter()
            .any(|z| z["zone"] == zone.as_str()),
        "the zone is still running"
    );
    assert!(
        !client
            .sdn_ipam_status("pve")
            .unwrap()
            .iter()
            .any(|e| e["zone"] == zone.as_str()),
        "an IPAM entry of the zone was left behind"
    );
}

/// ADR-0063 D1 and D2 against a real node, with no guest: the cluster's IPAM
/// controllers are listed and the built-in one is served (D1); a subnet's
/// gateway and DHCP ranges change in place, a range is cleared, and a new
/// gateway an entry holds is refused before any write (D2.1, D2.2, D2.4);
/// and the failure ADR-0063 measured — a gateway change staged, the
/// transaction failed and rolled back, the IPAM's gateway entry left on the
/// new address — is injected and then repaired (D2.3).
#[test]
fn a_subnet_changes_in_place_and_a_rolled_back_gateway_entry_is_repaired() {
    use delonix_networking::ipam::{DhcpRange, IpamProvider, IpamReservation, IpamSubnet};
    use delonix_networking::ownership::OwnerMark;
    use delonix_networking::segment::{EnsureOutcome, NetworkZoneSpec, SegmentProvider, VNetSpec};
    let Some(t) = target() else {
        return;
    };
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let segment = delonix_proxmox::ProxmoxSegmentProvider::new(
        client.clone(),
        delonix_proxmox::Ledger::at(dir.path()),
    );
    let ipam = delonix_proxmox::ProxmoxIpamProvider::new(
        client.clone(),
        delonix_proxmox::Ledger::at(dir.path()),
    );
    let owner = OwnerMark::from_random(
        &(std::process::id() ^ 0x0063_d200)
            .to_be_bytes()
            .repeat(4)
            .try_into()
            .unwrap(),
    );
    let suffix = std::process::id() % 1_000_000;
    let zone = format!("k{suffix}");
    let vnet = format!("m{suffix}");
    let octet = suffix % 200 + 20;
    let at = |last: u8| format!("10.83.{octet}.{last}");
    let subnet = |gateway: u8, ranges: &[(u8, u8)]| IpamSubnet {
        vnet: vnet.clone(),
        cidr: format!("10.83.{octet}.0/24"),
        gateway: Some(at(gateway)),
        dhcp_ranges: ranges
            .iter()
            .map(|(s, e)| DhcpRange {
                start: at(*s),
                end: at(*e),
            })
            .collect(),
    };
    // The IPAM's gateway entries in the vnet, as the node lists them.
    let gateway_entries = || -> Vec<String> {
        client
            .sdn_ipam_status("pve")
            .unwrap()
            .iter()
            .filter(|e| e["zone"] == zone.as_str() && e["vnet"] == vnet.as_str())
            .filter(|e| match &e["gateway"] {
                serde_json::Value::Number(n) => n.as_u64() == Some(1),
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::String(s) => s == "1",
                _ => false,
            })
            .map(|e| e["ip"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    let running = || {
        ipam.observe(&zone, std::slice::from_ref(&vnet))
            .expect("observe")
            .subnets
    };

    // D1: the cluster's controllers; the built-in one is there and served.
    let controllers = ipam.controllers().expect("controllers");
    eprintln!("IPAM controllers on the node: {controllers:?}");
    let pve = controllers
        .iter()
        .find(|c| c.id == "pve")
        .expect("the built-in controller");
    assert_eq!(pve.kind, "pve");
    ipam.refuse_unsupported(pve)
        .expect("the built-in controller is served");

    let first = subnet(1, &[(100, 150)]);
    segment
        .transaction(&mut || {
            segment.ensure_zone(&NetworkZoneSpec { name: zone.clone() })?;
            segment.ensure_vnet(
                &VNetSpec {
                    name: vnet.clone(),
                    zone: zone.clone(),
                    alias: Some("d2".into()),
                },
                &owner,
            )?;
            ipam.prepare_zone(&zone, "pve", true)?;
            ipam.ensure_subnet(&zone, &first, &owner)?;
            Ok(())
        })
        .expect("zone, vnet and subnet");
    assert_eq!(running(), vec![first.clone()]);
    assert_eq!(gateway_entries(), vec![at(1)]);
    let reserved = IpamReservation {
        vnet: vnet.clone(),
        ip: at(20),
        mac: "BC:24:11:83:00:20".into(),
    };
    assert_eq!(
        ipam.ensure_reservation(&zone, &reserved).expect("reserve"),
        EnsureOutcome::Created
    );

    // D2.4: a gateway onto the reserved address is refused before any write.
    let onto_reserved = subnet(20, &[(100, 150)]);
    let e = segment
        .transaction(&mut || {
            ipam.ensure_subnet(&zone, &onto_reserved, &owner)
                .map(|_| ())
        })
        .expect_err("a gateway onto a held address");
    assert!(e.to_string().contains("already held"), "{e}");
    assert_eq!(
        gateway_entries(),
        vec![at(1)],
        "the refusal moved the entry"
    );

    // D2.1: gateway and ranges in place, in one transaction (a range covering
    // the reservation is accepted, ADR-0063 D2.5).
    let moved = subnet(254, &[(10, 30)]);
    segment
        .transaction(&mut || {
            assert_eq!(
                ipam.ensure_subnet(&zone, &moved, &owner)?,
                EnsureOutcome::Created
            );
            Ok(())
        })
        .expect("the gateway and the ranges in place");
    assert_eq!(running(), vec![moved.clone()]);
    assert_eq!(gateway_entries(), vec![at(254)]);
    segment
        .transaction(&mut || {
            assert_eq!(
                ipam.ensure_subnet(&zone, &moved, &owner)?,
                EnsureOutcome::AlreadyPresent
            );
            Ok(())
        })
        .expect("unchanged");

    // D2.2: every range cleared (`delete=dhcp-range`), then one put back.
    let cleared = subnet(254, &[]);
    segment
        .transaction(&mut || ipam.ensure_subnet(&zone, &cleared, &owner).map(|_| ()))
        .expect("the ranges cleared");
    assert_eq!(running(), vec![cleared.clone()]);
    let settled = subnet(254, &[(100, 150)]);
    segment
        .transaction(&mut || ipam.ensure_subnet(&zone, &settled, &owner).map(|_| ()))
        .expect("a range back");
    assert_eq!(running(), vec![settled.clone()]);

    // D2.3: inject the measured failure — a gateway change staged, then the
    // transaction fails and is rolled back.
    let staged = subnet(200, &[(100, 150)]);
    let e = segment
        .transaction(&mut || {
            ipam.ensure_subnet(&zone, &staged, &owner)?;
            Err(delonix_model::Error::Invalid(
                "injected failure after the gateway was staged".into(),
            ))
        })
        .expect_err("the injected failure");
    assert!(e.to_string().contains("injected failure"), "{e}");
    assert_eq!(
        running(),
        vec![settled.clone()],
        "the rollback kept the subnet"
    );
    let left = gateway_entries();
    eprintln!("after the rollback the IPAM gateway entries are {left:?}");
    assert!(
        !left.contains(&at(254)),
        "ADR-0063 measured the entry left on the staged address; the node now restores it: \
         {left:?}"
    );

    // Another owner's repair touches nothing: the vnet is not its own.
    let stranger = OwnerMark::new("dlx-ffffffffffffffff").unwrap();
    assert!(ipam
        .repair_gateways(&zone, std::slice::from_ref(&vnet), &stranger)
        .expect("a stranger's repair")
        .is_empty());
    assert_eq!(
        gateway_entries(),
        left,
        "a stranger's repair changed the IPAM"
    );

    let repaired = ipam
        .repair_gateways(&zone, std::slice::from_ref(&vnet), &owner)
        .expect("repair");
    eprintln!("repair: {repaired:?}");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    let after = gateway_entries();
    eprintln!("after the repair the IPAM gateway entries are {after:?}");
    assert_eq!(
        after,
        vec![at(254)],
        "one gateway entry, on the running gateway"
    );
    assert_eq!(
        running(),
        vec![settled.clone()],
        "the repair changed the subnet"
    );
    assert!(
        client.sdn_pending_changes().expect("pending").is_empty(),
        "the repair left staged changes"
    );
    assert!(
        ipam.repair_gateways(&zone, std::slice::from_ref(&vnet), &owner)
            .expect("again")
            .is_empty(),
        "a second repair found something to do"
    );

    assert_eq!(
        ipam.remove_reservation(&zone, &reserved).expect("release"),
        delonix_networking::ownership::RemoveOutcome::Removed
    );
    segment
        .transaction(&mut || {
            ipam.remove_subnet(&zone, &settled, &owner)?;
            segment.remove_vnet(&vnet, &owner)?;
            segment.remove_zone(&zone)
        })
        .expect("teardown");
    assert!(
        !client
            .sdn_ipam_status("pve")
            .unwrap()
            .iter()
            .any(|e| e["zone"] == zone.as_str()),
        "an IPAM entry of the zone was left behind"
    );
}

/// ADR-0063 D1.2/D1.3 against a real node: a NetBox and a phpIPAM controller
/// registered on the cluster — real entries of `GET /cluster/sdn/ipams`, the
/// node having verified each URL against a stub — are listed by the IPAM port
/// with their plugin type and refused BY NAME by the provider (DX-6381),
/// while the built-in `pve` one stays served. The stub sees only the node's
/// own verification: the engine never talks to an external IPAM. Needs
/// `DELONIX_PROXMOX_TEST_CALLBACK_ADDR`, like the controller half above.
#[test]
fn an_external_ipam_controller_is_listed_and_refused_by_name() {
    use delonix_networking::ipam::IpamProvider;
    use delonix_proxmox::IpamKind;
    let Some(t) = target() else {
        return;
    };
    let Ok(callback) = std::env::var("DELONIX_PROXMOX_TEST_CALLBACK_ADDR") else {
        return;
    };
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = delonix_proxmox::Ledger::at(dir.path());
    let ipam = delonix_proxmox::ProxmoxIpamProvider::new(
        client.clone(),
        delonix_proxmox::Ledger::at(dir.path()),
    );
    let stub = ControllerStub::start();
    let base = format!("http://{callback}:{}", stub.port);
    let suffix = std::process::id() % 1_000_000;
    let netbox = format!("nb{suffix}");
    let phpipam = format!("ph{suffix}");

    /// Deletes the controllers this test registered, on every exit.
    struct Registered<'a> {
        client: &'a delonix_proxmox::Client,
        ledger: &'a delonix_proxmox::Ledger,
        ids: Vec<String>,
    }
    impl Drop for Registered<'_> {
        fn drop(&mut self) {
            for id in &self.ids {
                let _ = self.client.delete_sdn_ipam(self.ledger, id);
            }
        }
    }
    let registered = Registered {
        client: &client,
        ledger: &ledger,
        ids: vec![netbox.clone(), phpipam.clone()],
    };
    client
        .create_sdn_ipam(
            &ledger,
            &netbox,
            IpamKind::Netbox,
            &format!("{base}/api"),
            "tok-nb",
            None,
        )
        .expect("register a NetBox controller");
    client
        .create_sdn_ipam(
            &ledger,
            &phpipam,
            IpamKind::PhpIpam,
            &format!("{base}/api/app"),
            "tok-ph",
            Some(1),
        )
        .expect("register a phpIPAM controller");

    let listed = ipam.controllers().expect("controllers");
    for (id, kind, why) in [
        (&netbox, "netbox", "ADR-0063 D1.3"),
        (&phpipam, "phpipam", "parsing of result not yet implemented"),
    ] {
        let c = listed
            .iter()
            .find(|c| c.id == *id)
            .unwrap_or_else(|| panic!("controller '{id}' is not listed: {listed:?}"));
        assert_eq!(c.kind, kind, "{c:?}");
        let e = ipam
            .refuse_unsupported(c)
            .expect_err("an external controller is refused by name");
        assert_eq!(e.number(), 6381, "{e}");
        let text = e.to_string();
        assert!(
            text.contains(id.as_str()) && text.contains(kind) && text.contains(why),
            "{text}"
        );
    }
    let pve = listed
        .iter()
        .find(|c| c.id == "pve")
        .expect("the built-in controller");
    ipam.refuse_unsupported(pve)
        .expect("the built-in controller is still served");

    // phpIPAM's token rides a `token:` header the stub does not record.
    let seen = stub.requests();
    assert!(
        seen.iter().all(|l| {
            (l.contains("/api/ipam/aggregates/") && l.contains("tok-nb"))
                || l.contains("/api/app/sections/1")
        }),
        "something other than the node's own verification reached the controller: {seen:?}"
    );

    drop(registered);
    let after = ipam.controllers().expect("controllers after the teardown");
    assert!(
        !after.iter().any(|c| c.id == netbox || c.id == phpipam),
        "a controller of this test was left on the cluster: {after:?}"
    );
}

/// ADR-0063 D2 against a real node, the cases the first live case left out:
///
/// * D2.3 for a gateway REMOVED: `delete=gateway` staged and rolled back
///   leaves the subnet running with its gateway and the IPAM with NO gateway
///   entry — the router's address free for a guest (measured: the node then
///   accepts a reservation of it). The repair puts the entry back.
/// * D2.2 `delete=gateway` for real: the node clears the gateway and its
///   IPAM entry.
/// * D2.3 for a gateway ADDED: staged and rolled back, the subnet runs
///   without a gateway and the IPAM holds the new one's entry, which then
///   makes D2.4 refuse that very gateway. The repair releases it, and the
///   gateway is then added.
/// * D2.5: a guest's allocation (made by the node at the VM's create), then
///   the subnet's range narrowed so the allocation falls outside it — the
///   node accepts the change and leaves the allocation where it is.
#[test]
fn a_gateway_is_removed_and_added_in_place_and_a_range_narrows_under_a_guest() {
    use delonix_networking::ipam::{DhcpRange, IpamProvider, IpamSubnet};
    use delonix_networking::ownership::OwnerMark;
    use delonix_networking::segment::{EnsureOutcome, NetworkZoneSpec, SegmentProvider, VNetSpec};
    let Some(t) = target() else {
        return;
    };
    let storage =
        std::env::var("DELONIX_PROXMOX_TEST_STORAGE").unwrap_or_else(|_| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = delonix_proxmox::Ledger::at(dir.path());
    let segment = delonix_proxmox::ProxmoxSegmentProvider::new(
        client.clone(),
        delonix_proxmox::Ledger::at(dir.path()),
    );
    let ipam = delonix_proxmox::ProxmoxIpamProvider::new(
        client.clone(),
        delonix_proxmox::Ledger::at(dir.path()),
    );
    let owner = OwnerMark::from_random(
        &(std::process::id() ^ 0x0063_d225)
            .to_be_bytes()
            .repeat(4)
            .try_into()
            .unwrap(),
    );
    let suffix = std::process::id() % 1_000_000;
    let zone = format!("g{suffix}");
    let vnet = format!("h{suffix}");
    let octet = suffix % 200 + 20;
    let at = |last: u8| format!("10.85.{octet}.{last}");
    let subnet = |gateway: Option<u8>, ranges: &[(u8, u8)]| IpamSubnet {
        vnet: vnet.clone(),
        cidr: format!("10.85.{octet}.0/24"),
        gateway: gateway.map(at),
        dhcp_ranges: ranges
            .iter()
            .map(|(s, e)| DhcpRange {
                start: at(*s),
                end: at(*e),
            })
            .collect(),
    };
    let gateway_entries = || -> Vec<String> {
        client
            .sdn_ipam_status("pve")
            .unwrap()
            .iter()
            .filter(|e| e["zone"] == zone.as_str() && e["vnet"] == vnet.as_str())
            .filter(|e| match &e["gateway"] {
                serde_json::Value::Number(n) => n.as_u64() == Some(1),
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::String(s) => s == "1",
                _ => false,
            })
            .map(|e| e["ip"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    let running = || {
        ipam.observe(&zone, std::slice::from_ref(&vnet))
            .expect("observe")
            .subnets
    };
    let repair = || {
        ipam.repair_gateways(&zone, std::slice::from_ref(&vnet), &owner)
            .expect("repair")
    };
    let failed = |s: &IpamSubnet| {
        let e = segment
            .transaction(&mut || {
                ipam.ensure_subnet(&zone, s, &owner)?;
                Err(delonix_model::Error::Invalid(
                    "injected failure after the subnet was staged".into(),
                ))
            })
            .expect_err("the injected failure");
        assert!(e.to_string().contains("injected failure"), "{e}");
    };
    let converge = |s: &IpamSubnet| {
        segment
            .transaction(&mut || ipam.ensure_subnet(&zone, s, &owner).map(|_| ()))
            .expect("the subnet in place");
    };

    let with_gateway = subnet(Some(1), &[(100, 150)]);
    let without_gateway = subnet(None, &[(100, 150)]);
    segment
        .transaction(&mut || {
            segment.ensure_zone(&NetworkZoneSpec { name: zone.clone() })?;
            segment.ensure_vnet(
                &VNetSpec {
                    name: vnet.clone(),
                    zone: zone.clone(),
                    alias: Some("d25".into()),
                },
                &owner,
            )?;
            ipam.prepare_zone(&zone, "pve", true)?;
            ipam.ensure_subnet(&zone, &with_gateway, &owner)?;
            Ok(())
        })
        .expect("zone, vnet and subnet");
    assert_eq!(running(), vec![with_gateway.clone()]);
    assert_eq!(gateway_entries(), vec![at(1)]);

    // D2.3, a gateway REMOVED and rolled back.
    failed(&without_gateway);
    assert_eq!(
        running(),
        vec![with_gateway.clone()],
        "the rollback kept the gateway"
    );
    assert_eq!(
        gateway_entries(),
        Vec::<String>::new(),
        "measured: the staged delete=gateway released the entry and the rollback did not \
         bring it back"
    );
    let repaired = repair();
    tracing::info!(?repaired, "after a rolled-back gateway removal");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert!(
        repaired[0].contains("held no gateway entry"),
        "{repaired:?}"
    );
    assert_eq!(gateway_entries(), vec![at(1)], "the entry is back");
    assert_eq!(running(), vec![with_gateway.clone()]);
    assert!(client.sdn_pending_changes().expect("pending").is_empty());
    assert!(repair().is_empty(), "a second repair found something to do");

    // D2.2, `delete=gateway` for real.
    converge(&without_gateway);
    assert_eq!(running(), vec![without_gateway.clone()]);
    assert_eq!(
        gateway_entries(),
        Vec::<String>::new(),
        "the node cleared the gateway's IPAM entry with the gateway"
    );
    segment
        .transaction(&mut || {
            assert_eq!(
                ipam.ensure_subnet(&zone, &without_gateway, &owner)?,
                EnsureOutcome::AlreadyPresent
            );
            Ok(())
        })
        .expect("unchanged");

    // D2.3, a gateway ADDED and rolled back.
    failed(&with_gateway);
    assert_eq!(
        running(),
        vec![without_gateway.clone()],
        "the rollback kept the subnet without a gateway"
    );
    assert_eq!(
        gateway_entries(),
        vec![at(1)],
        "measured: the staged gateway's entry survived the rollback"
    );
    let e = segment
        .transaction(&mut || ipam.ensure_subnet(&zone, &with_gateway, &owner).map(|_| ()))
        .expect_err("D2.4 refuses the gateway the stale entry holds");
    assert!(e.to_string().contains("already held"), "{e}");
    let repaired = repair();
    tracing::info!(?repaired, "after a rolled-back gateway addition");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert!(
        repaired[0].contains("runs without a gateway"),
        "{repaired:?}"
    );
    assert_eq!(gateway_entries(), Vec::<String>::new());
    assert!(client.sdn_pending_changes().expect("pending").is_empty());
    converge(&with_gateway);
    assert_eq!(running(), vec![with_gateway.clone()]);
    assert_eq!(gateway_entries(), vec![at(1)]);

    // D2.5: a guest's allocation, then the range narrowed under it.
    /// Destroys the guest this test created, on every exit.
    struct Guest<'a> {
        client: &'a delonix_proxmox::Client,
        ledger: &'a delonix_proxmox::Ledger,
        vmid: u32,
    }
    impl Drop for Guest<'_> {
        fn drop(&mut self) {
            if self.client.vm_exists(self.vmid).unwrap_or(false) {
                let _ = self.client.destroy(self.ledger, self.vmid);
            }
        }
    }
    let vmid = client.next_vmid().expect("next vmid");
    let cfg = delonix_compute::vm_backend::VmConfig {
        name: format!("dlxd25{}", suffix % 10000),
        vcpus: 1,
        memory: "128M".into(),
        bridge: Some(vnet.clone()),
        ..Default::default()
    };
    client
        .create_vm(&ledger, vmid, &cfg.name, &cfg, &storage, 1)
        .expect("a guest on the vnet");
    let guest = Guest {
        client: &client,
        ledger: &ledger,
        vmid,
    };
    let allocation = |vmid: u32| -> Vec<String> {
        ipam.observe(&zone, std::slice::from_ref(&vnet))
            .expect("observe")
            .entries
            .iter()
            .filter(|e| e.vmid == Some(vmid))
            .map(|e| e.ip.clone())
            .collect()
    };
    let held = allocation(vmid);
    let in_old_range = |ip: &str| {
        ip.rsplit('.')
            .next()
            .and_then(|o| o.parse::<u8>().ok())
            .is_some_and(|o| (100..=150).contains(&o))
    };
    assert!(
        held.len() == 1 && in_old_range(&held[0]),
        "the node allocated the guest a range address at create: {held:?}"
    );
    let narrowed = subnet(Some(1), &[(10, 30)]);
    segment
        .transaction(&mut || {
            assert_eq!(
                ipam.ensure_subnet(&zone, &narrowed, &owner)?,
                EnsureOutcome::Created
            );
            Ok(())
        })
        .expect("the node accepts a range narrowed under a guest's allocation");
    assert_eq!(running(), vec![narrowed.clone()]);
    assert_eq!(
        allocation(vmid),
        held,
        "the node leaves the guest's allocation where it is, outside the new range"
    );
    tracing::info!(?held, "a guest allocation outside the narrowed range");
    drop(guest);
    assert!(
        allocation(vmid).is_empty(),
        "destroying the guest released its allocation"
    );

    segment
        .transaction(&mut || {
            ipam.remove_subnet(&zone, &narrowed, &owner)?;
            segment.remove_vnet(&vnet, &owner)?;
            segment.remove_zone(&zone)
        })
        .expect("teardown");
    assert!(
        !client
            .sdn_ipam_status("pve")
            .unwrap()
            .iter()
            .any(|e| e["zone"] == zone.as_str()),
        "an IPAM entry of the zone was left behind"
    );
}

/// Reads a PowerDNS zone's records through the server's own API (the TEST's
/// read: the engine never talks to the DNS server). Returns `(name, type,
/// contents)` without the SOA and NS sets.
fn powerdns_records(url: &str, key: &str, zone: &str) -> Vec<(String, String, Vec<String>)> {
    let body: serde_json::Value = reqwest::blocking::Client::new()
        .get(format!("{url}/zones/{zone}"))
        .header("X-API-Key", key)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .unwrap_or_else(|e| panic!("read the PowerDNS zone {zone}: {e}"));
    body["rrsets"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|r| !matches!(r["type"].as_str(), Some("SOA") | Some("NS")))
        .map(|r| {
            (
                r["name"].as_str().unwrap_or_default().to_string(),
                r["type"].as_str().unwrap_or_default().to_string(),
                r["records"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|x| x["content"].as_str().unwrap_or_default().to_string())
                    .collect(),
            )
        })
        .collect()
}

/// Removes one rrset from a PowerDNS zone (the test's own cleanup of what the
/// node leaves behind).
fn powerdns_delete(url: &str, key: &str, zone: &str, name: &str, kind: &str) {
    reqwest::blocking::Client::new()
        .patch(format!("{url}/zones/{zone}"))
        .header("X-API-Key", key)
        .json(&serde_json::json!({
            "rrsets": [{ "name": name, "type": kind, "changetype": "DELETE" }]
        }))
        .send()
        .and_then(|r| r.error_for_status())
        .unwrap_or_else(|e| panic!("delete {name} {kind} in {zone}: {e}"));
}

/// Undoes what the DNS live case made, on EVERY exit — a failed assertion
/// included — so the shared lab keeps no guest, zone, vnet or DNS record
/// (the lesson of #587: a teardown written as the test's last lines only runs
/// when everything before it passed). Best effort: each step ignores what is
/// already gone.
struct DnsLabGuard {
    client: std::sync::Arc<delonix_proxmox::Client>,
    ledger_dir: std::path::PathBuf,
    zone: String,
    vnet: String,
    cidr: String,
    vmid: std::cell::Cell<Option<u32>>,
    url: String,
    key: String,
    /// `(DNS zone, name, type)` of every record the node may have written.
    records: std::cell::RefCell<Vec<(String, String, String)>>,
}

impl Drop for DnsLabGuard {
    fn drop(&mut self) {
        let ledger = delonix_proxmox::Ledger::at(&self.ledger_dir);
        if let Some(vmid) = self.vmid.get() {
            let _ = self.client.lxc_destroy(&ledger, vmid);
        }
        let present = self
            .client
            .sdn_zones()
            .unwrap_or_default()
            .iter()
            .any(|z| z["zone"] == self.zone.as_str());
        if present {
            let _ = self.client.sdn_transaction(&ledger, || {
                let _ = self
                    .client
                    .delete_sdn_subnet(&ledger, &self.vnet, &self.zone, &self.cidr);
                let _ = self.client.delete_sdn_vnet(&ledger, &self.vnet);
                self.client.delete_sdn_zone(&ledger, &self.zone)
            });
        }
        for (zone, name, kind) in self.records.borrow().iter() {
            let _ = reqwest::blocking::Client::new()
                .patch(format!("{}/zones/{zone}", self.url))
                .header("X-API-Key", &self.key)
                .json(&serde_json::json!({
                    "rrsets": [{ "name": name, "type": kind, "changetype": "DELETE" }]
                }))
                .send();
        }
    }
}

/// ADR-0059 F5c (ADR-0064): the DNS role, end to end through the node and a
/// real PowerDNS. The zone gets `dns`/`dnszone`/`reversedns` in the same
/// transaction as its IPAM, vnet and subnet; the node writes the subnet
/// gateway's records (`<vnet>-gw`) when the subnet is created, and a guest's
/// A and PTR when it gets an address from the DHCP range; destroying the guest
/// removes them. Tearing the zone down leaves the gateway's records on the
/// server — no node API removes them (measured on PVE 9.2.2) — and this test
/// asserts that, then removes them itself so the lab stays clean.
///
/// Preconditions set by the cluster's administrator (the engine never creates
/// them): a DNS controller registered on the cluster
/// (`DELONIX_PROXMOX_TEST_DNS_CONTROLLER`), the domain
/// (`DELONIX_PROXMOX_TEST_DNS_ZONE`) and `10.in-addr.arpa.` on that server,
/// and the server's API reachable from the test (`DELONIX_PROXMOX_TEST_DNS_URL`,
/// key in `DELONIX_PROXMOX_TEST_DNS_KEY_FILE`) to read the records back.
#[test]
fn the_dns_provider_registers_a_guest_in_the_zones_dns_server() {
    use delonix_compute::system_container::{
        SystemContainerNet, SystemContainerProvider, SystemContainerSpec,
    };
    use delonix_networking::dns::{dns_drift, DnsProvider, ZoneDns};
    use delonix_networking::ipam::{DhcpRange, IpamProvider, IpamSubnet};
    use delonix_networking::ownership::OwnerMark;
    use delonix_networking::segment::{EnsureOutcome, NetworkZoneSpec, SegmentProvider, VNetSpec};
    let Some(t) = target() else {
        return;
    };
    let (Ok(archive), Ok(controller), Ok(domain), Ok(url), Ok(key_file)) = (
        std::env::var("DELONIX_PROXMOX_TEST_OCI_ARCHIVE"),
        std::env::var("DELONIX_PROXMOX_TEST_DNS_CONTROLLER"),
        std::env::var("DELONIX_PROXMOX_TEST_DNS_ZONE"),
        std::env::var("DELONIX_PROXMOX_TEST_DNS_URL"),
        std::env::var("DELONIX_PROXMOX_TEST_DNS_KEY_FILE"),
    ) else {
        return;
    };
    let key = std::fs::read_to_string(&key_file)
        .expect("read the DNS server's key")
        .trim()
        .to_string();
    let archive = std::path::PathBuf::from(archive);
    let digest = oci_archive_manifest_digest(&archive);
    let template = t.import_storage.clone().unwrap_or_else(|| "local".into());
    let rootfs = t.disk_storage.clone().unwrap_or_else(|| "local-lvm".into());
    let b = backend(&t).expect("connect");
    let client = b.client();
    let dir = tempfile::tempdir().expect("tempdir");
    let segment = delonix_proxmox::ProxmoxSegmentProvider::new(
        client.clone(),
        delonix_proxmox::Ledger::at(dir.path()),
    );
    let ipam = delonix_proxmox::ProxmoxIpamProvider::new(
        client.clone(),
        delonix_proxmox::Ledger::at(dir.path()),
    );
    let dns = delonix_proxmox::ProxmoxDnsProvider::new(
        client.clone(),
        delonix_proxmox::Ledger::at(dir.path()),
    );
    assert!(
        dns.controllers()
            .expect("list the DNS controllers")
            .iter()
            .any(|c| c.id == controller && c.kind == "powerdns"),
        "the lab needs the DNS controller '{controller}' registered on the cluster"
    );
    let owner = OwnerMark::from_random(
        &std::process::id()
            .to_be_bytes()
            .repeat(4)
            .try_into()
            .unwrap(),
    );
    let suffix = std::process::id() % 1_000_000;
    let zone = format!("d{suffix}");
    let vnet = format!("e{suffix}");
    let octet = suffix % 200 + 20;
    let gateway = format!("10.85.{octet}.1");
    let subnet = IpamSubnet {
        vnet: vnet.clone(),
        cidr: format!("10.85.{octet}.0/24"),
        gateway: Some(gateway.clone()),
        dhcp_ranges: vec![DhcpRange {
            start: format!("10.85.{octet}.100"),
            end: format!("10.85.{octet}.150"),
        }],
    };
    let declared = ZoneDns {
        server: controller.clone(),
        zone: domain.clone(),
        reverse_server: Some(controller.clone()),
    };
    let gw_ptr = {
        let o: Vec<&str> = gateway.split('.').collect();
        format!("{}.{}.{}.{}.in-addr.arpa.", o[3], o[2], o[1], o[0])
    };
    let guard = DnsLabGuard {
        client: client.clone(),
        ledger_dir: dir.path().to_path_buf(),
        zone: zone.clone(),
        vnet: vnet.clone(),
        cidr: subnet.cidr.clone(),
        vmid: std::cell::Cell::new(None),
        url: url.clone(),
        key: key.clone(),
        records: std::cell::RefCell::new(vec![
            (
                format!("{domain}."),
                format!("{vnet}-gw.{domain}."),
                "A".into(),
            ),
            ("10.in-addr.arpa.".into(), gw_ptr.clone(), "PTR".into()),
        ]),
    };

    segment
        .transaction(&mut || {
            assert_eq!(
                segment.ensure_zone(&NetworkZoneSpec { name: zone.clone() })?,
                EnsureOutcome::Created
            );
            segment.ensure_vnet(
                &VNetSpec {
                    name: vnet.clone(),
                    zone: zone.clone(),
                    alias: Some("f5c".into()),
                },
                &owner,
            )?;
            ipam.prepare_zone(&zone, "pve", true)?;
            dns.prepare_zone(&zone, Some(&declared))?;
            ipam.ensure_subnet(&zone, &subnet, &owner)?;
            Ok(())
        })
        .expect("zone, vnet, DNS and subnet in one transaction");
    let observed = dns.observe(&zone).expect("observe");
    assert!(
        dns_drift(Some(&declared), observed.as_ref()).is_empty(),
        "{observed:?}"
    );
    // Preparing the same settings again writes nothing (and does not fail).
    segment
        .transaction(&mut || dns.prepare_zone(&zone, Some(&declared)))
        .expect("idempotent");

    let fwd = format!("{domain}.");
    let gw_name = format!("{vnet}-gw.{domain}.");
    let has = |zone_name: &str, name: &str, kind: &str, content: &str| {
        powerdns_records(&url, &key, zone_name)
            .iter()
            .any(|(n, k, c)| n == name && k == kind && c.iter().any(|x| x == content))
    };
    assert!(
        has(&fwd, &gw_name, "A", &gateway),
        "the node did not register the gateway: {:?}",
        powerdns_records(&url, &key, &fwd)
    );

    let name = format!("dlxdn{}", suffix % 10000);
    let spec = SystemContainerSpec {
        name: name.clone(),
        archive,
        manifest_digest: digest,
        entrypoint: vec!["/bin/sleep".into(), "3600".into()],
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        memory_mib: 256,
        swap_mib: 0,
        cores: 1,
        rootfs_gib: 1,
        network: Some(SystemContainerNet {
            bridge: vnet.clone(),
            vlan: None,
            dhcp: true,
        }),
        unprivileged: true,
    };
    let ct =
        delonix_proxmox::ProxmoxSystemContainerProvider::new(client.clone(), &template, &rootfs);
    let ctdir = tempfile::tempdir().expect("tempdir");
    let h = ct.create(ctdir.path(), &spec).expect("create");
    guard
        .vmid
        .set(h.locator.rsplit(':').next().and_then(|v| v.parse().ok()));
    let ip = client
        .sdn_ipam_status("pve")
        .unwrap()
        .iter()
        .find(|e| e["zone"] == zone.as_str() && e["hostname"] == name.as_str())
        .and_then(|e| e["ip"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("the guest '{name}' got no IPAM entry"));
    let guest = format!("{name}.{domain}.");
    let ptr = {
        let o: Vec<&str> = ip.split('.').collect();
        format!("{}.{}.{}.{}.in-addr.arpa.", o[3], o[2], o[1], o[0])
    };
    guard.records.borrow_mut().extend([
        (fwd.clone(), guest.clone(), "A".into()),
        ("10.in-addr.arpa.".into(), ptr.clone(), "PTR".into()),
    ]);
    assert!(
        has(&fwd, &guest, "A", &ip),
        "no A for the guest: {:?}",
        powerdns_records(&url, &key, &fwd)
    );
    assert!(
        has("10.in-addr.arpa.", &ptr, "PTR", &guest),
        "no PTR for the guest: {:?}",
        powerdns_records(&url, &key, "10.in-addr.arpa.")
    );

    ct.destroy(ctdir.path(), &h).expect("destroy");
    guard.vmid.set(None);
    assert!(
        !powerdns_records(&url, &key, &fwd)
            .iter()
            .any(|(n, _, _)| n == &guest),
        "the guest's A outlived it"
    );
    assert!(
        !powerdns_records(&url, &key, "10.in-addr.arpa.")
            .iter()
            .any(|(n, _, _)| n == &ptr),
        "the guest's PTR outlived it"
    );

    segment
        .transaction(&mut || {
            ipam.remove_subnet(&zone, &subnet, &owner)?;
            segment.remove_vnet(&vnet, &owner)?;
            segment.remove_zone(&zone)
        })
        .expect("teardown");
    assert_eq!(dns.observe(&zone).expect("observe"), None);
    // The node leaves the gateway's records: no node API removes them. The
    // engine says so on a teardown; this test removes them itself.
    let left_a = has(&fwd, &gw_name, "A", &gateway);
    let left_ptr = has("10.in-addr.arpa.", &gw_ptr, "PTR", &gw_name);
    powerdns_delete(&url, &key, &fwd, &gw_name, "A");
    powerdns_delete(&url, &key, "10.in-addr.arpa.", &gw_ptr, "PTR");
    assert!(
        left_a && left_ptr,
        "the node now removes a deleted subnet's gateway records (A left: {left_a}, PTR left: \
         {left_ptr}) — drop the teardown's warning (ADR-0064)"
    );
}
