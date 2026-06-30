//! `delonix-vm` — runtime de microVMs com **backend selecionável**:
//!
//! * **Cloud Hypervisor** (VMM em Rust sobre `/dev/kvm`, corre rootless DENTRO do
//!   netns de infra do ingress — o `tap` vive lá) — o backend histórico.
//! * **libvirt/KVM** (QEMU gerido pelo `libvirtd` via `virsh`) — 2.º backend, para
//!   hosts onde o libvirt já é o padrão de virtualização.
//!
//! O backend é escolhido por VM: explícito (`VmConfig.backend`) ou **auto-deteção**
//! (prefere o `cloud-hypervisor` se instalado; senão `libvirt`). O estado por-VM
//! ([`delonix_core::Vm`], persistido em `<base>/vms/<name>.json`) regista o backend
//! que a arrancou, para reconciliar liveness/paragem com o backend certo.
//!
//! Rede: o Cloud Hypervisor reaproveita o *plumbing* do `delonix-net`
//! (`infra::vm_attach` cria um `tap` na bridge do ingress + DHCP). O libvirt corre o
//! QEMU sob o `libvirtd` (netns do host), por isso usa, no MVP, **rede user-mode**
//! (SLIRP/passt: egress sem `tap`); a integração com a bridge do ingress (inbound
//! pelo SDN) é um follow-up.

use std::path::Path;
use std::process::Command;

use delonix_core::{Error, JsonStore, Result, Status, Vm};
use delonix_net::infra;

/// Configuração para arrancar uma microVM (campos planos, independentes do
/// `orchestrator` — a CLI traduz o `VmSpec` para isto).
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Nome (chave de persistência e do `tap`/MAC determinísticos).
    pub name: String,
    /// Disco base (qcow2/raw) — torna-se overlay por-VM.
    pub disk: String,
    /// vCPUs.
    pub vcpus: u32,
    /// Memória (ex.: `"2G"`, `"1024M"`).
    pub memory: String,
    /// Rede do ingress para o `tap`.
    pub network: String,
    /// Kernel para *direct boot* (vmlinux/bzImage).
    pub kernel: Option<String>,
    /// Initrd/initramfs (com `kernel`).
    pub initrd: Option<String>,
    /// Firmware (alternativa ao kernel: rust-hypervisor-fw/EDK2 — para cloud images).
    pub firmware: Option<String>,
    /// Linha de comando do kernel (com `kernel`).
    pub cmdline: Option<String>,
    /// ISO de *seed* cloud-init (NoCloud) — disco secundário.
    pub seed: Option<String>,
    /// Política de reinício normalizada (`"no"`|`"on-failure"`|`"always"`).
    pub restart_policy: Option<String>,
    // --- HPC (S4) ---------------------------------------------------------
    /// Suporta a memória da VM em *hugepages* (`--memory …,hugepages=on`). Reduz
    /// TLB misses e jitter em cargas HPC. Requer hugepages reservadas no host.
    pub hugepages: bool,
    /// Afinidade de CPU (NUMA/pinning): lista de CPUs do host (ex.: `"8-15"`) a que
    /// TODAS as vCPUs são fixadas (`--cpus …,affinity=<vcpu>@[<lista>]`). Evita a
    /// migração de vCPUs entre cores/nós NUMA — determinismo de latência.
    pub cpu_affinity: Option<String>,
    /// Passagem de dispositivos PCI (SR-IOV VF, GPU, …) por VFIO: caminhos sysfs
    /// (ex.: `/sys/bus/pci/devices/0000:65:00.1`). O VF tem de estar pré-ligado ao
    /// `vfio-pci` no host. Cada um vira `--device path=…`.
    pub devices: Vec<String>,
    /// Backend de virtualização: `Some("cloud-hypervisor")`, `Some("libvirt")` ou
    /// `None` (auto-deteção). Default histórico = cloud-hypervisor.
    pub backend: Option<String>,
    /// Modo de rede do backend **libvirt** (o Cloud Hypervisor usa sempre o `tap` do
    /// ingress). Abstrai o `<interface>` do domínio — o utilizador NUNCA escreve XML:
    ///   * `None`/`"user"` — rede user-mode (SLIRP/passt): egress, sem IP de entrada.
    ///   * `"nat"`         — rede NAT gerida pelo libvirt (`<source network=…>`, DHCP +
    ///                       IP via `virsh domifaddr`). Requer `qemu:///system` (root).
    ///   * `"bridge"`      — liga a uma bridge do host (`bridge` abaixo).
    pub net_mode: Option<String>,
    /// Nome da bridge do host (modo `net_mode = "bridge"`) ou da rede libvirt (modo
    /// `"nat"`; default `"default"`).
    pub bridge: Option<String>,
}

// ===========================================================================
// Helpers partilhados
// ===========================================================================

fn vms_dir(base: &Path) -> std::path::PathBuf {
    base.join("vms")
}

fn store(base: &Path) -> Result<JsonStore<Vm>> {
    JsonStore::open(vms_dir(base))
}

/// `true` se o PID está vivo (existe `/proc/<pid>`).
fn is_alive(pid: i32) -> bool {
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
}

/// `true` se já existe uma VM com este nome.
pub fn exists(base: &Path, name: &str) -> bool {
    store(base).map(|s| s.exists(name)).unwrap_or(false)
}

/// Converte memória (`"2G"`/`"1024M"`/`"512"`/`"2Gi"`) para MiB.
fn mem_mib(s: &str) -> u64 {
    let t = s.trim();
    // Tolera o sufixo `i` do estilo k8s (Gi/Mi): "2Gi" == "2G", "512Mi" == "512M".
    let t = t.strip_suffix(['i', 'I']).unwrap_or(t);
    let (num, mult) = if let Some(n) = t.strip_suffix(['G', 'g']) {
        (n, 1024)
    } else if let Some(n) = t.strip_suffix(['M', 'm']) {
        (n, 1)
    } else {
        (t, 1)
    };
    match num.trim().parse::<u64>() {
        Ok(v) => v * mult,
        // Não degradar em silêncio: um valor mal-escrito ("2GB", "2 Gi") daria
        // metade-ish da RAM pedida sem aviso. Avisa e usa um default seguro.
        Err(_) => {
            eprintln!("delonix: valor de memória inválido {s:?}; a usar 1024 MiB por omissão");
            1024
        }
    }
}

/// Aspas para shell (single-quote, escapando `'`).
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// MAC determinístico (prefixo QEMU/KVM `52:54:00`) derivado do nome.
fn mac_for(name: &str) -> String {
    let h = infra::name_hash(name);
    format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        (h >> 16) & 0xff,
        (h >> 8) & 0xff,
        h & 0xff
    )
}

/// `true` se a correr sem privilégios de root (euid ≠ 0).
fn is_rootless() -> bool {
    // SAFETY: geteuid não tem efeitos colaterais.
    unsafe { libc::geteuid() != 0 }
}

/// `true` se um binário existe no `PATH`.
fn binary_in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(name).is_file()))
        .unwrap_or(false)
}

/// Extrai o campo `"format"` de um `qemu-img info --output=json`. Função pura
/// (testável sem `qemu-img`). Ignora a chave distinta `"format-specific"` (o
/// padrão `"format"` com aspa-a-fechar não casa o prefixo `"format-`).
fn parse_qemu_format(json: &str) -> Option<String> {
    let i = json.find("\"format\"")?;
    let rest = &json[i + "\"format\"".len()..];
    let colon = rest.find(':')?;
    let rest = &rest[colon + 1..];
    let q1 = rest.find('"')?;
    let q2 = rest[q1 + 1..].find('"')?;
    let fmt = &rest[q1 + 1..q1 + 1 + q2];
    (!fmt.is_empty()).then(|| fmt.to_string())
}

/// Formato REAL do disco base via `qemu-img info` — NÃO confia na extensão. As
/// cloud images Ubuntu/Debian distribuem-se como `*.img` mas são **qcow2**
/// internamente; um overlay criado com `-F raw` sobre um backing qcow2 faz o
/// guest ler o qcow2 como raw → disco corrompido / não-booting, em silêncio.
/// Cai para a heurística da extensão se o `qemu-img info` não estiver disponível.
pub fn disk_backing_format(disk: &Path) -> String {
    if let Ok(out) = Command::new("qemu-img")
        .args(["info", "--output=json"])
        .arg(disk)
        .output()
    {
        if out.status.success() {
            if let Some(fmt) = std::str::from_utf8(&out.stdout).ok().and_then(parse_qemu_format) {
                return fmt;
            }
        }
    }
    if disk.extension().and_then(|e| e.to_str()) == Some("qcow2") {
        "qcow2".into()
    } else {
        "raw".into()
    }
}

/// Corre uma ferramenta externa (ex.: `qemu-img`), erro se falhar.
fn run_tool(prog: &str, args: &[&str]) -> Result<()> {
    let st = Command::new(prog)
        .args(args)
        .status()
        .map_err(|e| Error::Runtime {
            context: "vm-tool",
            message: format!("{prog}: {e}"),
        })?;
    if !st.success() {
        return Err(Error::Runtime {
            context: "vm-tool",
            message: format!("{prog} falhou"),
        });
    }
    Ok(())
}

/// Corre um comando e captura o stdout (trimmed), ou `None` em falha.
fn capture(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(prog).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ===========================================================================
// Backend trait
// ===========================================================================

/// O que um backend produziu ao arrancar uma VM — persistido na [`Vm`].
pub struct Boot {
    /// PID do VMM no host (Cloud Hypervisor). `None` quando gerido por um daemon
    /// (libvirt) — aí a liveness vem de `is_running`.
    pub pid: Option<i32>,
    /// Interface `tap` (ou `"user"` para rede user-mode do libvirt).
    pub tap: String,
    /// MAC da NIC.
    pub mac: String,
    /// Socket de controlo (API do Cloud Hypervisor; vazio no libvirt).
    pub api_socket: String,
    /// IP da VM, se conhecido no arranque.
    pub ip: Option<String>,
}

/// Mecanismo de virtualização por trás de uma microVM. Permite ter o Cloud
/// Hypervisor e o libvirt/KVM lado a lado (escolhido por VM).
pub trait VmBackend {
    /// Identificador estável persistido na [`Vm`].
    fn id(&self) -> &'static str;
    /// `true` se o backend tem as ferramentas necessárias instaladas.
    fn available(&self) -> bool;
    /// Cria a rede (se aplicável) e arranca a VM a partir do `overlay`. A criação
    /// do overlay e a idempotência são tratadas por [`create`].
    fn boot(&self, vmdir: &Path, cfg: &VmConfig, overlay: &str) -> Result<Boot>;
    /// A VM ainda está viva?
    fn is_running(&self, vm: &Vm) -> bool;
    /// IP atual da VM (pode mudar/resolver-se mais tarde por DHCP).
    fn ip(&self, vm: &Vm) -> Option<String>;
    /// Pára a VM e liberta os recursos de rede.
    fn stop(&self, vmdir: &Path, vm: &Vm);
}

/// Seleciona um backend a partir de um pedido explícito ou por auto-deteção
/// (prefere cloud-hypervisor se instalado; senão libvirt).
pub fn select_backend(want: Option<&str>) -> Result<Box<dyn VmBackend>> {
    match want.map(|s| s.trim().to_lowercase()).as_deref() {
        Some("cloud-hypervisor") | Some("ch") | Some("cloudhypervisor") => {
            Ok(Box::new(CloudHypervisorBackend))
        }
        Some("libvirt") | Some("kvm") | Some("qemu") => Ok(Box::new(LibvirtBackend)),
        Some(other) if !other.is_empty() => Err(Error::Invalid(format!(
            "backend de VM desconhecido: '{other}' (usa 'cloud-hypervisor' ou 'libvirt')"
        ))),
        _ => {
            let ch = CloudHypervisorBackend;
            if ch.available() {
                return Ok(Box::new(ch));
            }
            let lv = LibvirtBackend;
            if lv.available() {
                return Ok(Box::new(lv));
            }
            Err(Error::Invalid(
                "nenhum backend de VM disponível: instala 'cloud-hypervisor' ou 'libvirt'+'qemu'".into(),
            ))
        }
    }
}

/// O backend que arrancou uma VM já persistida (para liveness/stop).
fn backend_for(vm: &Vm) -> Box<dyn VmBackend> {
    match vm.backend.as_str() {
        "libvirt" => Box::new(LibvirtBackend),
        _ => Box::new(CloudHypervisorBackend),
    }
}

// ===========================================================================
// Backend: Cloud Hypervisor
// ===========================================================================

/// Constrói o argumento `--memory` do Cloud Hypervisor (com `hugepages=on` se
/// pedido). Função pura — testada sem hardware.
fn memory_arg(cfg: &VmConfig) -> String {
    let mut a = format!("size={}M", mem_mib(&cfg.memory));
    if cfg.hugepages {
        a.push_str(",hugepages=on");
    }
    a
}

/// Constrói o argumento `--cpus` do Cloud Hypervisor. Com `cpu_affinity`, fixa
/// cada vCPU à mesma lista de CPUs do host (`affinity=0@[lista],1@[lista],…`).
/// Função pura — testada sem hardware.
fn cpus_arg(cfg: &VmConfig) -> String {
    let n = cfg.vcpus.max(1);
    let mut a = format!("boot={n}");
    if let Some(list) = &cfg.cpu_affinity {
        let aff: Vec<String> = (0..n).map(|v| format!("{v}@[{list}]")).collect();
        a.push_str(&format!(",affinity={}", aff.join(":")));
    }
    a
}

/// Backend histórico: Cloud Hypervisor dentro do netns de infra (rootless).
pub struct CloudHypervisorBackend;

impl VmBackend for CloudHypervisorBackend {
    fn id(&self) -> &'static str {
        "cloud-hypervisor"
    }

    fn available(&self) -> bool {
        binary_in_path("cloud-hypervisor")
    }

    fn boot(&self, vmdir: &Path, cfg: &VmConfig, overlay: &str) -> Result<Boot> {
        // Rede privada própria quando nomeada (≠ ingress partilhado): garante o seu
        // bridge isolado + DHCP antes do attach. O SDN das VMs vive aqui.
        if !matches!(cfg.network.as_str(), "" | "ingress" | "bridge" | "default") {
            let _ = infra::network_create(&cfg.network);
        }
        let tap = infra::vm_attach(&cfg.name, &cfg.network)?;
        let mac = mac_for(&cfg.name);
        let pid = match boot_ch(vmdir, cfg, overlay, &tap, &mac) {
            Ok(p) => p,
            Err(e) => {
                infra::vm_detach(&cfg.name);
                return Err(e);
            }
        };
        let sock = vmdir.join(format!("{}.sock", cfg.name));
        Ok(Boot {
            pid: Some(pid),
            ip: infra::dhcp_ip_for_mac(&cfg.network, &mac),
            tap,
            mac,
            api_socket: sock.to_string_lossy().into_owned(),
        })
    }

    fn is_running(&self, vm: &Vm) -> bool {
        vm.pid.map(is_alive).unwrap_or(false)
    }

    fn ip(&self, vm: &Vm) -> Option<String> {
        infra::dhcp_ip_for_mac(&vm.network, &vm.mac)
    }

    fn stop(&self, _vmdir: &Path, vm: &Vm) {
        if let Some(pid) = vm.pid {
            if pid > 0 {
                // SAFETY: enviar SIGTERM a um PID é seguro; ignora-se o erro.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
        infra::vm_detach(&vm.name);
    }
}

/// Arranca o `cloud-hypervisor` DENTRO do netns de infra, em background, e devolve
/// o PID (real, visível no host).
fn boot_ch(vmdir: &Path, cfg: &VmConfig, overlay: &str, tap: &str, mac: &str) -> Result<i32> {
    let join = infra::infra_join_argv().ok_or_else(|| Error::Runtime {
        context: "vm",
        message: "o ingress (infra rootless) não está de pé".into(),
    })?;
    let sock = vmdir.join(format!("{}.sock", cfg.name));
    let serial = vmdir.join(format!("{}.serial", cfg.name));
    let log = vmdir.join(format!("{}.log", cfg.name));
    let pidfile = vmdir.join(format!("{}.pid", cfg.name));
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&pidfile);

    let mut ch: Vec<String> = vec![
        "cloud-hypervisor".into(),
        "--api-socket".into(),
        sock.to_string_lossy().into_owned(),
    ];
    // Arranque: kernel (direct boot) OU firmware (cloud images com bootloader).
    if let Some(k) = &cfg.kernel {
        ch.push("--kernel".into());
        ch.push(k.clone());
        if let Some(i) = &cfg.initrd {
            ch.push("--initramfs".into());
            ch.push(i.clone());
        }
        ch.push("--cmdline".into());
        ch.push(
            cfg.cmdline
                .clone()
                .unwrap_or_else(|| "console=ttyS0 root=/dev/vda1 rw".into()),
        );
    } else if let Some(fw) = &cfg.firmware {
        ch.push("--firmware".into());
        ch.push(fw.clone());
    } else {
        return Err(Error::Invalid(
            "VM sem 'kernel' nem 'firmware' — o Cloud Hypervisor precisa de um para arrancar (ex.: firmware: rust-hypervisor-fw para cloud images)".into(),
        ));
    }
    ch.push("--disk".into());
    ch.push(format!("path={overlay}"));
    if let Some(seed) = &cfg.seed {
        ch.push("--disk".into());
        ch.push(format!("path={seed}"));
    }
    ch.push("--cpus".into());
    ch.push(cpus_arg(cfg)); // boot=N [+ affinity para NUMA/CPU pinning]
    ch.push("--memory".into());
    ch.push(memory_arg(cfg)); // size=XM [+ hugepages=on]
    // SR-IOV / VFIO: passa cada dispositivo PCI pré-ligado ao vfio-pci.
    for dev in &cfg.devices {
        ch.push("--device".into());
        ch.push(format!("path={dev}"));
    }
    ch.push("--net".into());
    ch.push(format!("tap={tap},mac={mac}"));
    ch.push("--serial".into());
    ch.push(format!("file={}", serial.display()));
    ch.push("--console".into());
    ch.push("off".into());

    // background dentro do netns; sem pid-ns ⇒ o $! é o PID real no host.
    let ch_str = ch.iter().map(|a| shq(a)).collect::<Vec<_>>().join(" ");
    let script = format!(
        "{ch_str} </dev/null >>{log} 2>&1 & echo $! > {pid}",
        log = shq(&log.to_string_lossy()),
        pid = shq(&pidfile.to_string_lossy())
    );

    let st = Command::new(&join[0])
        .args(&join[1..])
        .args(["sh", "-c", &script])
        .env("DELONIX_INTERNAL", "1")
        .status()
        .map_err(|e| Error::Runtime {
            context: "cloud-hypervisor",
            message: e.to_string(),
        })?;
    if !st.success() {
        return Err(Error::Runtime {
            context: "vm",
            message: "falha a lançar o cloud-hypervisor (KVM/binário disponível?)".into(),
        });
    }
    // espera curta pelo pidfile.
    for _ in 0..20 {
        if pidfile.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let pid = std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(0);
    if pid <= 0 {
        return Err(Error::Runtime {
            context: "vm",
            message: "o cloud-hypervisor não reportou PID (verifica o log da VM)".into(),
        });
    }
    Ok(pid)
}

// ===========================================================================
// Backend: libvirt / KVM (QEMU sob libvirtd, via virsh)
// ===========================================================================

/// 2.º backend: QEMU/KVM gerido pelo `libvirtd`, controlado por `virsh`.
pub struct LibvirtBackend;

/// URI de ligação do libvirt: sessão do utilizador (rootless) ou sistema (root).
fn libvirt_uri() -> &'static str {
    if is_rootless() {
        "qemu:///session"
    } else {
        "qemu:///system"
    }
}

/// Gera o XML do domínio libvirt (KVM). **Função pura** — testada sem daemon.
///
/// Cobre: vCPUs (+ pinning via `<cputune>`), memória (+ hugepages via
/// `<memoryBacking>`), disco virtio (overlay qcow2), seed cloud-init (cdrom),
/// rede user-mode virtio (egress rootless), consola série, e passagem VFIO de
/// dispositivos PCI (`<hostdev>`).
pub fn libvirt_domain_xml(cfg: &VmConfig, overlay: &str, mac: &str) -> String {
    let mib = mem_mib(&cfg.memory);
    let kib = mib * 1024;
    let vcpus = cfg.vcpus.max(1);
    let name = xml_escape(&cfg.name);

    let mut s = String::new();
    s.push_str("<domain type='kvm'>\n");
    s.push_str(&format!("  <name>{name}</name>\n"));
    s.push_str(&format!("  <memory unit='KiB'>{kib}</memory>\n"));
    s.push_str(&format!("  <currentMemory unit='KiB'>{kib}</currentMemory>\n"));
    // hugepages (HPC): suporta a RAM do domínio em hugepages do host.
    if cfg.hugepages {
        s.push_str("  <memoryBacking>\n    <hugepages/>\n  </memoryBacking>\n");
    }
    s.push_str(&format!("  <vcpu placement='static'>{vcpus}</vcpu>\n"));
    // CPU pinning (NUMA/determinismo): fixa cada vCPU à lista de CPUs do host.
    if let Some(list) = &cfg.cpu_affinity {
        let list = xml_escape(list);
        s.push_str("  <cputune>\n");
        for v in 0..vcpus {
            s.push_str(&format!("    <vcpupin vcpu='{v}' cpuset='{list}'/>\n"));
        }
        s.push_str("  </cputune>\n");
    }
    // Boot: firmware (cloud images) ou kernel direto.
    s.push_str("  <os>\n    <type arch='x86_64' machine='q35'>hvm</type>\n");
    if let Some(k) = &cfg.kernel {
        s.push_str(&format!("    <kernel>{}</kernel>\n", xml_escape(k)));
        if let Some(i) = &cfg.initrd {
            s.push_str(&format!("    <initrd>{}</initrd>\n", xml_escape(i)));
        }
        let cmdline = cfg
            .cmdline
            .clone()
            .unwrap_or_else(|| "console=ttyS0 root=/dev/vda1 rw".into());
        s.push_str(&format!("    <cmdline>{}</cmdline>\n", xml_escape(&cmdline)));
    } else if let Some(fw) = &cfg.firmware {
        s.push_str(&format!(
            "    <loader readonly='yes' type='pflash'>{}</loader>\n",
            xml_escape(fw)
        ));
    } else {
        s.push_str("    <boot dev='hd'/>\n");
    }
    s.push_str("  </os>\n");
    s.push_str("  <features>\n    <acpi/>\n    <apic/>\n  </features>\n");
    s.push_str("  <cpu mode='host-passthrough' check='none'/>\n");
    s.push_str("  <clock offset='utc'/>\n");
    s.push_str("  <on_poweroff>destroy</on_poweroff>\n");
    // política de reinício: 'always'/'on-failure' → restart no crash.
    let on_crash = match cfg.restart_policy.as_deref() {
        Some("always") | Some("on-failure") => "restart",
        _ => "destroy",
    };
    s.push_str(&format!(
        "  <on_reboot>restart</on_reboot>\n  <on_crash>{on_crash}</on_crash>\n"
    ));
    s.push_str("  <devices>\n");
    s.push_str("    <emulator>/usr/bin/qemu-system-x86_64</emulator>\n");
    // disco principal: overlay qcow2 via virtio (vda).
    s.push_str("    <disk type='file' device='disk'>\n");
    s.push_str("      <driver name='qemu' type='qcow2'/>\n");
    s.push_str(&format!("      <source file='{}'/>\n", xml_escape(overlay)));
    s.push_str("      <target dev='vda' bus='virtio'/>\n");
    s.push_str("    </disk>\n");
    // seed cloud-init (NoCloud) como cdrom.
    if let Some(seed) = &cfg.seed {
        s.push_str("    <disk type='file' device='cdrom'>\n");
        s.push_str("      <driver name='qemu' type='raw'/>\n");
        s.push_str(&format!("      <source file='{}'/>\n", xml_escape(seed)));
        s.push_str("      <target dev='sda' bus='sata'/>\n");
        s.push_str("      <readonly/>\n    </disk>\n");
    }
    // rede: abstraída pelo YAML (net_mode) → `<interface>` virtio. Sem XML à mão.
    s.push_str(&libvirt_interface_xml(cfg, mac));
    // consola série (logs de boot).
    s.push_str("    <serial type='pty'><target type='isa-serial' port='0'/></serial>\n");
    s.push_str("    <console type='pty'><target type='serial' port='0'/></console>\n");
    // VFIO: passagem de dispositivos PCI (SR-IOV VF, GPU).
    for dev in &cfg.devices {
        if let Some((dom, bus, slot, func)) = parse_pci_addr(dev) {
            s.push_str("    <hostdev mode='subsystem' type='pci' managed='yes'>\n      <source>\n");
            s.push_str(&format!(
                "        <address domain='0x{dom}' bus='0x{bus}' slot='0x{slot}' function='0x{func}'/>\n"
            ));
            s.push_str("      </source>\n    </hostdev>\n");
        }
    }
    s.push_str("  </devices>\n");
    s.push_str("</domain>\n");
    s
}

/// Gera o `<interface>` do domínio libvirt a partir do `net_mode` do YAML — assim a
/// rede fica 100% abstraída (sem XML à mão). **Função pura** — testada sem daemon.
fn libvirt_interface_xml(cfg: &VmConfig, mac: &str) -> String {
    let mac = xml_escape(mac);
    let model = "      <model type='virtio'/>\n    </interface>\n";
    match cfg.net_mode.as_deref().unwrap_or("user") {
        "nat" | "network" => {
            // rede NAT gerida pelo libvirt (DHCP + IP via domifaddr). `bridge` = nome
            // da rede libvirt (default "default").
            let net = cfg.bridge.as_deref().unwrap_or("default");
            format!(
                "    <interface type='network'>\n      <source network='{}'/>\n      <mac address='{mac}'/>\n{model}",
                xml_escape(net)
            )
        }
        "bridge" => {
            // liga a uma bridge do host pré-existente.
            let br = cfg.bridge.as_deref().unwrap_or("virbr0");
            format!(
                "    <interface type='bridge'>\n      <source bridge='{}'/>\n      <mac address='{mac}'/>\n{model}",
                xml_escape(br)
            )
        }
        _ => {
            // user-mode (SLIRP/passt): egress sem tap — rootless-friendly (default).
            format!("    <interface type='user'>\n      <mac address='{mac}'/>\n{model}")
        }
    }
}

/// Escapa os 5 caracteres especiais de XML.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Extrai `(domain, bus, slot, func)` de um caminho/endereço PCI
/// (`/sys/bus/pci/devices/0000:65:00.1` ou `0000:65:00.1`). Função pura.
fn parse_pci_addr(dev: &str) -> Option<(String, String, String, String)> {
    let bdf = dev.rsplit('/').next().unwrap_or(dev); // 0000:65:00.1
    let (rest, func) = bdf.rsplit_once('.')?;
    let mut it = rest.split(':');
    let dom = it.next()?;
    let bus = it.next()?;
    let slot = it.next()?;
    if it.next().is_some() {
        return None;
    }
    Some((
        dom.to_string(),
        bus.to_string(),
        slot.to_string(),
        func.to_string(),
    ))
}

impl VmBackend for LibvirtBackend {
    fn id(&self) -> &'static str {
        "libvirt"
    }

    fn available(&self) -> bool {
        binary_in_path("virsh") && binary_in_path("qemu-system-x86_64")
    }

    fn boot(&self, vmdir: &Path, cfg: &VmConfig, overlay: &str) -> Result<Boot> {
        let mac = mac_for(&cfg.name);
        let uri = libvirt_uri();
        // overlay como caminho absoluto (o libvirtd pode correr noutro cwd).
        let overlay_abs = std::fs::canonicalize(overlay)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| overlay.to_string());
        // Modo NAT: garante a rede libvirt ativa (best-effort; ignora se já o está).
        if matches!(cfg.net_mode.as_deref(), Some("nat") | Some("network")) {
            let net = cfg.bridge.as_deref().unwrap_or("default");
            let _ = Command::new("virsh").args(["-c", uri, "net-start", net]).status();
        }
        let xml = libvirt_domain_xml(cfg, &overlay_abs, &mac);
        let xml_path = vmdir.join(format!("{}.xml", cfg.name));
        std::fs::write(&xml_path, &xml)?;

        // Idempotente: se o domínio já existe (auto-heal), só (re)arranca; senão
        // define + arranca. `virsh start` num domínio já a correr é no-op benigno.
        let defined = capture("virsh", &["-c", uri, "domstate", &cfg.name]).is_some();
        if !defined {
            run_tool("virsh", &["-c", uri, "define", &xml_path.to_string_lossy()])?;
        }
        let st = Command::new("virsh")
            .args(["-c", uri, "start", &cfg.name])
            .status()
            .map_err(|e| Error::Runtime {
                context: "libvirt",
                message: format!("virsh start: {e}"),
            })?;
        // 'start' falha se já estiver a correr — toleramos isso (auto-heal).
        if !st.success() && !self.is_running_uri(uri, &cfg.name) {
            return Err(Error::Runtime {
                context: "vm",
                message: "falha a arrancar o domínio libvirt (KVM/permissões/imagem?)".into(),
            });
        }
        Ok(Boot {
            pid: None, // gerido pelo libvirtd — liveness via virsh domstate
            ip: self.ip_uri(uri, &cfg.name),
            tap: "user".into(),
            mac,
            api_socket: String::new(),
        })
    }

    fn is_running(&self, vm: &Vm) -> bool {
        self.is_running_uri(libvirt_uri(), &vm.name)
    }

    fn ip(&self, vm: &Vm) -> Option<String> {
        self.ip_uri(libvirt_uri(), &vm.name)
    }

    fn stop(&self, vmdir: &Path, vm: &Vm) {
        let uri = libvirt_uri();
        let _ = Command::new("virsh")
            .args(["-c", uri, "destroy", &vm.name])
            .status();
        let _ = Command::new("virsh")
            .args(["-c", uri, "undefine", &vm.name])
            .status();
        let _ = std::fs::remove_file(vmdir.join(format!("{}.xml", vm.name)));
    }
}

impl LibvirtBackend {
    fn is_running_uri(&self, uri: &str, name: &str) -> bool {
        capture("virsh", &["-c", uri, "domstate", name])
            .map(|s| s == "running")
            .unwrap_or(false)
    }

    /// IP via `virsh domifaddr` (pode estar vazio em rede user-mode sem agente).
    fn ip_uri(&self, uri: &str, name: &str) -> Option<String> {
        let out = capture("virsh", &["-c", uri, "domifaddr", name])?;
        // formato: "Name  MAC  Protocol  Address"; pega o 1.º IPv4 (a.b.c.d/p).
        for line in out.lines() {
            if let Some(field) = line.split_whitespace().last() {
                if let Some((ip, _)) = field.split_once('/') {
                    if ip.parse::<std::net::Ipv4Addr>().is_ok() {
                        return Some(ip.to_string());
                    }
                }
            }
        }
        None
    }
}

// ===========================================================================
// Ciclo de vida (genérico, delega no backend)
// ===========================================================================

/// Garante a microVM (idempotente): se já existe e está viva, não faz nada; se
/// existe mas morreu, re-arranca reutilizando o overlay (auto-heal) com o MESMO
/// backend; senão, escolhe o backend (explícito/auto), cria o overlay e arranca.
pub fn create(base: &Path, cfg: &VmConfig) -> Result<Vm> {
    let vmdir = vms_dir(base);
    std::fs::create_dir_all(&vmdir)?;
    let st = store(base)?;

    let restarting = st.load(&cfg.name).ok();
    // No restart, honra o backend que a VM já usava; senão escolhe agora.
    let backend: Box<dyn VmBackend> = match &restarting {
        Some(ex) => {
            if backend_for(ex).is_running(ex) {
                return Ok(ex.clone()); // já a correr — idempotente
            }
            backend_for(ex)
        }
        None => select_backend(cfg.backend.as_deref())?,
    };

    let disk_path = std::fs::canonicalize(&cfg.disk)
        .map_err(|_| Error::Invalid(format!("imagem não encontrada: {}", cfg.disk)))?;
    let overlay = vmdir.join(format!("{}.qcow2", cfg.name));
    if !overlay.exists() {
        let bf = disk_backing_format(&disk_path);
        run_tool(
            "qemu-img",
            &[
                "create",
                "-f",
                "qcow2",
                "-b",
                &disk_path.to_string_lossy(),
                "-F",
                &bf,
                &overlay.to_string_lossy(),
            ],
        )?;
    }

    let boot = match backend.boot(&vmdir, cfg, &overlay.to_string_lossy()) {
        Ok(b) => b,
        Err(e) => {
            if restarting.is_none() {
                let _ = std::fs::remove_file(&overlay);
            }
            return Err(e);
        }
    };

    let mut vm = Vm::new(
        cfg.name.clone(),
        disk_path.to_string_lossy().into_owned(),
        overlay.to_string_lossy().into_owned(),
        cfg.vcpus.max(1),
        cfg.memory.clone(),
        cfg.network.clone(),
        boot.tap,
        boot.mac,
        boot.api_socket,
    );
    vm.pid = boot.pid;
    vm.status = Status::Running;
    vm.restart_policy = cfg.restart_policy.clone();
    vm.ip = boot.ip;
    vm.backend = backend.id().to_string();
    st.save(&cfg.name, &vm)?;
    Ok(vm)
}

/// Remove uma VM: pára o VMM (via o seu backend), e apaga overlay/estado.
pub fn remove(base: &Path, name: &str) -> Result<()> {
    let vmdir = vms_dir(base);
    let st = store(base)?;
    if let Ok(vm) = st.load(name) {
        backend_for(&vm).stop(&vmdir, &vm);
    } else {
        // sem registo: tenta limpar o tap do ingress, por segurança.
        infra::vm_detach(name);
    }
    let _ = std::fs::remove_file(vmdir.join(format!("{name}.qcow2")));
    let _ = std::fs::remove_file(vmdir.join(format!("{name}.sock")));
    let _ = std::fs::remove_file(vmdir.join(format!("{name}.serial")));
    let _ = std::fs::remove_file(vmdir.join(format!("{name}.log")));
    let _ = std::fs::remove_file(vmdir.join(format!("{name}.pid")));
    let _ = std::fs::remove_file(vmdir.join(format!("{name}.xml")));
    st.remove(name)
}

/// Estado actual de uma VM, com `status`/`ip` reconciliados pelo seu backend.
pub fn status(base: &Path, name: &str) -> Result<Vm> {
    let st = store(base)?;
    let mut vm = st.load(name)?;
    let backend = backend_for(&vm);
    if backend.is_running(&vm) {
        vm.status = Status::Running;
        vm.ip = backend.ip(&vm).or(vm.ip);
    } else {
        // Uma VM desligada = Stopped (o guest pode ter feito shutdown limpo; ao
        // contrário dos containers, a VM é autónoma — não se assume crash).
        vm.status = Status::Stopped;
        vm.pid = None;
    }
    Ok(vm)
}

/// Lista todas as VMs, com estado reconciliado.
pub fn list(base: &Path) -> Result<Vec<Vm>> {
    let st = store(base)?;
    let mut out = Vec::new();
    for vm in st.list()? {
        out.push(status(base, &vm.name).unwrap_or(vm));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_mib_parses_units() {
        assert_eq!(mem_mib("2G"), 2048);
        assert_eq!(mem_mib("1024M"), 1024);
        assert_eq!(mem_mib("512"), 512);
        assert_eq!(mem_mib("2Gi"), 2048); // sufixo k8s tolerado (antes dava 1024)
        assert_eq!(mem_mib("512Mi"), 512);
        assert_eq!(mem_mib("lixo"), 1024); // fallback robusto
    }

    #[test]
    fn parse_qemu_format_extrai_formato_real() {
        // `.img` que é qcow2 por dentro — o cerne do bug do backing-format.
        let j = r#"{"virtual-size":2361393152,"filename":"jammy.img","format":"qcow2","actual-size":643825664,"format-specific":{"type":"qcow2","data":{}}}"#;
        assert_eq!(parse_qemu_format(j).as_deref(), Some("qcow2"));
        let raw = r#"{"filename":"disco.raw","format":"raw","virtual-size":10}"#;
        assert_eq!(parse_qemu_format(raw).as_deref(), Some("raw"));
        // não confunde com a chave "format-specific".
        let only_spec = r#"{"format-specific":{"type":"qcow2"}}"#;
        assert_eq!(parse_qemu_format(only_spec), None);
        assert_eq!(parse_qemu_format("{}"), None);
    }

    /// VmConfig mínima para exercitar os helpers de args HPC (S4).
    fn hpc_cfg() -> VmConfig {
        VmConfig {
            name: "v".into(),
            disk: "/d.qcow2".into(),
            vcpus: 4,
            memory: "2G".into(),
            network: "ingress".into(),
            kernel: None,
            initrd: None,
            firmware: None,
            cmdline: None,
            seed: None,
            restart_policy: None,
            hugepages: false,
            cpu_affinity: None,
            devices: vec![],
            backend: None,
            net_mode: None,
            bridge: None,
        }
    }

    #[test]
    fn memory_arg_plain_and_hugepages() {
        let mut c = hpc_cfg();
        assert_eq!(memory_arg(&c), "size=2048M");
        c.hugepages = true;
        assert_eq!(memory_arg(&c), "size=2048M,hugepages=on");
    }

    #[test]
    fn cpus_arg_plain_and_affinity() {
        let mut c = hpc_cfg();
        assert_eq!(cpus_arg(&c), "boot=4");
        c.cpu_affinity = Some("8-15".into());
        // cada uma das 4 vCPUs fixada à lista 8-15 do host.
        assert_eq!(
            cpus_arg(&c),
            "boot=4,affinity=0@[8-15]:1@[8-15]:2@[8-15]:3@[8-15]"
        );
    }

    #[test]
    fn shq_escapes_quotes() {
        assert_eq!(shq("a b"), "'a b'");
        assert_eq!(shq("a'b"), "'a'\\''b'");
    }

    #[test]
    fn backend_selection() {
        assert_eq!(select_backend(Some("libvirt")).unwrap().id(), "libvirt");
        assert_eq!(select_backend(Some("kvm")).unwrap().id(), "libvirt");
        assert_eq!(
            select_backend(Some("cloud-hypervisor")).unwrap().id(),
            "cloud-hypervisor"
        );
        assert!(select_backend(Some("xpto")).is_err());
    }

    #[test]
    fn pci_addr_parsing() {
        assert_eq!(
            parse_pci_addr("/sys/bus/pci/devices/0000:65:00.1"),
            Some(("0000".into(), "65".into(), "00".into(), "1".into()))
        );
        assert_eq!(
            parse_pci_addr("0000:03:00.0"),
            Some(("0000".into(), "03".into(), "00".into(), "0".into()))
        );
        assert_eq!(parse_pci_addr("lixo"), None);
    }

    #[test]
    fn libvirt_xml_has_core_devices() {
        let mut c = hpc_cfg();
        c.firmware = Some("/usr/share/fw.fd".into());
        c.seed = Some("/seed.iso".into());
        let xml = libvirt_domain_xml(&c, "/var/lib/delonix/vms/v.qcow2", "52:54:00:ab:cd:ef");
        assert!(xml.contains("<domain type='kvm'>"));
        assert!(xml.contains("<name>v</name>"));
        assert!(xml.contains("<vcpu placement='static'>4</vcpu>"));
        assert!(xml.contains("<memory unit='KiB'>2097152</memory>")); // 2G
        assert!(xml.contains("type='qcow2'"));
        assert!(xml.contains("dev='vda' bus='virtio'"));
        assert!(xml.contains("device='cdrom'")); // seed
        assert!(xml.contains("<interface type='user'>"));
        assert!(xml.contains("52:54:00:ab:cd:ef"));
        assert!(xml.contains("host-passthrough"));
    }

    #[test]
    fn libvirt_interface_modes_from_yaml() {
        let mut c = hpc_cfg();
        // default = user-mode (egress, rootless).
        assert!(libvirt_interface_xml(&c, "52:54:00:00:00:01").contains("type='user'"));
        // nat → rede libvirt (default "default") com IP via domifaddr.
        c.net_mode = Some("nat".into());
        let nat = libvirt_interface_xml(&c, "52:54:00:00:00:01");
        assert!(nat.contains("type='network'") && nat.contains("source network='default'"));
        c.bridge = Some("dlxnat".into());
        assert!(libvirt_interface_xml(&c, "52:54:00:00:00:01").contains("source network='dlxnat'"));
        // bridge → bridge do host.
        c.net_mode = Some("bridge".into());
        c.bridge = Some("br0".into());
        let br = libvirt_interface_xml(&c, "52:54:00:00:00:01");
        assert!(br.contains("type='bridge'") && br.contains("source bridge='br0'"));
    }

    #[test]
    fn libvirt_xml_hugepages_and_pinning_and_vfio() {
        let mut c = hpc_cfg();
        c.firmware = Some("/fw.fd".into());
        c.hugepages = true;
        c.cpu_affinity = Some("8-15".into());
        c.devices = vec!["0000:65:00.1".into()];
        let xml = libvirt_domain_xml(&c, "/v.qcow2", "52:54:00:00:00:01");
        assert!(xml.contains("<hugepages/>"));
        assert!(xml.contains("<vcpupin vcpu='0' cpuset='8-15'/>"));
        assert!(xml.contains("<vcpupin vcpu='3' cpuset='8-15'/>"));
        assert!(xml.contains("<hostdev mode='subsystem' type='pci'"));
        assert!(xml.contains("bus='0x65' slot='0x00' function='0x1'"));
    }
}
