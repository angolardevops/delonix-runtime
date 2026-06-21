//! `delonix-vm` — runtime de microVMs via **Cloud Hypervisor** (VMM em Rust sobre
//! `/dev/kvm`). É o mecanismo por trás do `kind: VM` do manifesto declarativo.
//!
//! Reaproveita o *plumbing* de rede do `delonix-net` (o mesmo que as VMs QEMU já
//! usavam): `infra::vm_attach` cria um `tap` na bridge do ingress (com DHCP),
//! `infra::infra_join_argv` dá o prefixo `nsenter` para correr o VMM DENTRO do
//! netns de infra (onde o `tap` vive), e `infra::dhcp_ip_for_mac` resolve o IP.
//!
//! Estado por-VM persistido como [`delonix_core::Vm`] via
//! [`delonix_core::JsonStore`] sob `<base>/vms/<name>.json`.
//!
//! Notas de arranque: ao contrário do QEMU (`-daemonize`), o Cloud Hypervisor
//! corre em *foreground*. Como o netns de infra **não** entra no *pid-namespace*
//! (`infra_join_argv` usa `-U -m -n`, sem `-p`), pomos o VMM em *background* com
//! `sh -c '… & echo $! > pidfile'` e lemos o **PID real** (visível no host) do
//! `pidfile` — o mesmo padrão de `pidfile` que o caminho QEMU usava.

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
}

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

/// Converte memória (`"2G"`/`"1024M"`/`"512"`) para MiB.
fn mem_mib(s: &str) -> u64 {
    let t = s.trim();
    let (num, mult) = if let Some(n) = t.strip_suffix(['G', 'g']) {
        (n, 1024)
    } else if let Some(n) = t.strip_suffix(['M', 'm']) {
        (n, 1)
    } else {
        (t, 1)
    };
    num.trim().parse::<u64>().unwrap_or(1024) * mult
}

/// Aspas para shell (single-quote, escapando `'`).
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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

/// Garante a microVM (idempotente): se já existe e está viva, não faz nada; se
/// existe mas morreu, re-arranca reutilizando o overlay/tap (auto-heal); senão,
/// cria overlay + tap e arranca. Persiste o estado e devolve a [`Vm`].
pub fn create(base: &Path, cfg: &VmConfig) -> Result<Vm> {
    let vmdir = vms_dir(base);
    std::fs::create_dir_all(&vmdir)?;
    let st = store(base)?;

    let restarting = st.load(&cfg.name).ok();
    if let Some(ref ex) = restarting {
        if ex.pid.map(is_alive).unwrap_or(false) {
            return Ok(ex.clone()); // já a correr — idempotente
        }
    }

    let disk_path = std::fs::canonicalize(&cfg.disk)
        .map_err(|_| Error::Invalid(format!("imagem não encontrada: {}", cfg.disk)))?;
    let overlay = vmdir.join(format!("{}.qcow2", cfg.name));
    if !overlay.exists() {
        let bf = if cfg.disk.ends_with(".qcow2") {
            "qcow2"
        } else {
            "raw"
        };
        run_tool(
            "qemu-img",
            &[
                "create",
                "-f",
                "qcow2",
                "-b",
                &disk_path.to_string_lossy(),
                "-F",
                bf,
                &overlay.to_string_lossy(),
            ],
        )?;
    }

    // Rede privada própria quando nomeada (≠ ingress partilhado): garante o seu
    // bridge isolado + DHCP antes do attach. O SDN das VMs vive aqui.
    if !matches!(cfg.network.as_str(), "" | "ingress" | "bridge" | "default") {
        let _ = infra::network_create(&cfg.network);
    }
    // tap na bridge da rede (sobe a infra + DHCP) + MAC determinístico.
    let tap = infra::vm_attach(&cfg.name, &cfg.network)?;
    let h = infra::name_hash(&cfg.name);
    let mac = format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        (h >> 16) & 0xff,
        (h >> 8) & 0xff,
        h & 0xff
    );

    let pid = match boot(&vmdir, cfg, &overlay.to_string_lossy(), &tap, &mac) {
        Ok(p) => p,
        Err(e) => {
            infra::vm_detach(&cfg.name);
            if restarting.is_none() {
                let _ = std::fs::remove_file(&overlay);
            }
            return Err(e);
        }
    };

    let sock = vmdir.join(format!("{}.sock", cfg.name));
    let mut vm = Vm::new(
        cfg.name.clone(),
        disk_path.to_string_lossy().into_owned(),
        overlay.to_string_lossy().into_owned(),
        cfg.vcpus.max(1),
        cfg.memory.clone(),
        cfg.network.clone(),
        tap,
        mac.clone(),
        sock.to_string_lossy().into_owned(),
    );
    vm.pid = Some(pid);
    vm.status = Status::Running;
    vm.restart_policy = cfg.restart_policy.clone();
    vm.ip = infra::dhcp_ip_for_mac(&cfg.network, &mac);
    st.save(&cfg.name, &vm)?;
    Ok(vm)
}

/// Arranca o `cloud-hypervisor` DENTRO do netns de infra, em background, e devolve
/// o PID (real, visível no host). Reutilizado por `create`.
fn boot(vmdir: &Path, cfg: &VmConfig, overlay: &str, tap: &str, mac: &str) -> Result<i32> {
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

/// Remove uma VM: pára o VMM, liberta o `tap`, e apaga overlay/socket/log/estado.
pub fn remove(base: &Path, name: &str) -> Result<()> {
    let vmdir = vms_dir(base);
    let st = store(base)?;
    if let Ok(vm) = st.load(name) {
        if let Some(pid) = vm.pid {
            if pid > 0 {
                // SAFETY: enviar SIGTERM a um PID é seguro; ignora-se o erro.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
    }
    infra::vm_detach(name);
    let _ = std::fs::remove_file(vmdir.join(format!("{name}.qcow2")));
    let _ = std::fs::remove_file(vmdir.join(format!("{name}.sock")));
    let _ = std::fs::remove_file(vmdir.join(format!("{name}.serial")));
    let _ = std::fs::remove_file(vmdir.join(format!("{name}.log")));
    let _ = std::fs::remove_file(vmdir.join(format!("{name}.pid")));
    st.remove(name)
}

/// Estado actual de uma VM, com `status`/`ip` reconciliados (PID vivo? IP via DHCP).
pub fn status(base: &Path, name: &str) -> Result<Vm> {
    let st = store(base)?;
    let mut vm = st.load(name)?;
    let alive = vm.pid.map(is_alive).unwrap_or(false);
    if !alive {
        vm.status = Status::Exited(0);
        vm.pid = None;
    } else {
        vm.status = Status::Running;
        vm.ip = infra::dhcp_ip_for_mac(&vm.network, &vm.mac).or(vm.ip);
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
        assert_eq!(mem_mib("lixo"), 1024); // fallback robusto
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
        assert_eq!(cpus_arg(&c), "boot=4,affinity=0@[8-15]:1@[8-15]:2@[8-15]:3@[8-15]");
    }

    #[test]
    fn shq_escapes_quotes() {
        assert_eq!(shq("a b"), "'a b'");
        assert_eq!(shq("a'b"), "'a'\\''b'");
    }
}
