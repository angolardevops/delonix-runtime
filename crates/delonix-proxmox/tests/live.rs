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
//!   cargo test -p delonix-proxmox --test live -- --nocapture
//! ```
//!
//! It creates and then destroys one VM, and leaves the node's VM list as it
//! found it.

use delonix_proxmox::{Auth, ProxmoxBackend, Target};
use delonix_vm::{CreateStage, VmBackend, VmConfig};

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
    let b = ProxmoxBackend::connect(&t).expect("connect");

    let name = format!("dlxlive{}", std::process::id() % 10000);
    let cfg = VmConfig {
        name: name.clone(),
        disk: format!("{storage}:1"),
        vcpus: 1,
        memory: "512M".into(),
        ..Default::default()
    };

    let boot = b
        .boot(
            std::path::Path::new("/tmp"),
            &cfg,
            &cfg.disk,
            &|s: CreateStage| eprintln!("  stage: {s:?}"),
        )
        .expect("boot");
    eprintln!("created and started: handle={}", boot.api_socket);
    assert!(
        boot.api_socket.starts_with("proxmox:"),
        "the handle is what every later call addresses: {}",
        boot.api_socket
    );

    // The record the engine would keep.
    let vm = delonix_runtime_core::Vm::new(
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

    // Só parar: o `stop` PÁRA e não remove, desde que os dois verbos foram
    // separados (era o `vm stop` a apagar o disco). O domínio continua definido.
    b.stop(std::path::Path::new("/tmp"), &vm).expect("stop");
    assert!(
        !b.is_running(&vm),
        "a VM tem de ficar parada depois do stop"
    );
    assert!(
        b.client().config(vmid).is_ok(),
        "o `stop` REMOVEU a VM — parar e destruir são verbos diferentes"
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
    b.destroy(std::path::Path::new("/tmp"), &vm)
        .expect("destroy");
    assert!(
        b.client().config(vmid).is_err(),
        "a VM continua definida no nó depois do destroy — é um órfão"
    );

    // A record this backend did not create is refused rather than acted on.
    let mut alien = vm.clone();
    alien.api_socket = "/run/some.sock".into();
    assert!(
        b.stop(std::path::Path::new("/tmp"), &alien).is_err(),
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
    let b = ProxmoxBackend::connect(&t).expect("connect");
    let vm = delonix_runtime_core::Vm::new(
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
