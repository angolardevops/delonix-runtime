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

use delonix_proxmox::{Auth, ProxmoxBackend, Target};
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
    for want in ["create", "start", "snapshot", "rollback", "stop"] {
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
