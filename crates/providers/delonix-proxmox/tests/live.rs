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
            delonix_vm::ExtraDisk {
                source: format!("{storage}:1"),
                ..Default::default()
            },
            delonix_vm::ExtraDisk {
                source: format!("{storage}:2"),
                bus: "scsi".into(),
                ..Default::default()
            },
        ],
        extra_nics: vec![
            delonix_vm::ExtraNic::default(),
            delonix_vm::ExtraNic {
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
