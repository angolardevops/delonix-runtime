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
//! It creates one VM, walks it through snapshot, rollback, stop, resume and
//! destroy, and leaves the node's VM list as it found it. The trace file is the
//! numerator of the coverage matrix: `scripts/proxmox_api_inventory.py --trace`
//! promotes every route it records to `supported+tested` (ADR-0049 D2), and the
//! committed `docs/proxmox/trace-<ver>.routes` is one such run.

use delonix_proxmox::{AgentExecStatus, Auth, ProxmoxBackend, Target};
use delonix_vm::{CreateStage, VmBackend, VmConfig};

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

fn target() -> Option<Target> {
    Some(Target {
        base_url: std::env::var("DELONIX_PROXMOX_TEST_URL").ok()?,
        node: std::env::var("DELONIX_PROXMOX_TEST_NODE").unwrap_or_else(|_| "pve".into()),
        auth: Auth::Password {
            username: std::env::var("DELONIX_PROXMOX_TEST_USER").ok()?,
            password: std::env::var("DELONIX_PROXMOX_TEST_PASS").ok()?,
        },
        insecure_tls: true,
        bridge: None,
        vlan: None,
        ca_cert_pem: None,
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
