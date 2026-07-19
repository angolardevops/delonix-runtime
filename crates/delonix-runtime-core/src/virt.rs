//! Deteção de **virtualização** (KVM/QEMU/Xen/VMware/…) e de dispositivos
//! **virtio**, para o Delonix tirar o máximo desempenho quando corre dentro de
//! uma VM Linux (#1 — "integração nativa com KVM").
//!
//! Tudo é lido de `/sys` e `/proc` (sem dependências externas → funciona no
//! binário musl estático). A deteção nunca altera nada; as afinações de
//! desempenho são explícitas (ver [`set_blk_scheduler_none`]).

use std::path::Path;

/// Resumo do ambiente de virtualização do host onde o Delonix corre.
#[derive(Debug, Clone, Default)]
pub struct VirtInfo {
    /// `true` se estamos dentro de uma VM (qualquer hipervisor).
    pub virtualized: bool,
    /// Nome do hipervisor: `kvm`, `qemu`, `xen`, `vmware`, `virtualbox`,
    /// `hyper-v`, `unknown` ou `none` (bare-metal).
    pub hypervisor: String,
    /// `true` quando o guest é acelerado por KVM (caminho de máximo desempenho).
    pub is_kvm: bool,
    /// `/dev/kvm` presente (aceleração KVM disponível — p.ex. virtualização aninhada).
    pub kvm_accel: bool,
    /// Interfaces de rede servidas por `virtio_net` (p.ex. `enp1s0`).
    pub virtio_net: Vec<String>,
    /// Discos servidos por `virtio_blk` (p.ex. `vda`).
    pub virtio_blk: Vec<String>,
    /// Nº total de dispositivos no bus virtio (`/sys/bus/virtio/devices`).
    pub virtio_count: usize,
}

fn read_trim(p: &str) -> String {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Decide o nome do hipervisor a partir dos sinais do DMI/CPU. Função pura
/// (separada de [`detect`]) para ser testável sem uma VM real.
fn classify(
    product: &str,
    sys_vendor: &str,
    xen_type: &str,
    hv_flag: bool,
    has_virtio: bool,
) -> String {
    let pl = product.to_lowercase();
    let vl = sys_vendor.to_lowercase();
    if xen_type == "xen" {
        "xen".into()
    } else if pl.contains("kvm") {
        "kvm".into()
    } else if vl.contains("qemu") || vl.contains("red hat") {
        // Vendor QEMU/Red Hat com virtio é, na prática, sempre acelerado por KVM.
        if has_virtio {
            "kvm".into()
        } else {
            "qemu".into()
        }
    } else if vl.contains("vmware") {
        "vmware".into()
    } else if pl.contains("virtualbox") || vl.contains("innotek") || vl.contains("oracle") {
        "virtualbox".into()
    } else if vl.contains("microsoft") && pl.contains("virtual") {
        "hyper-v".into()
    } else if hv_flag || has_virtio {
        "unknown".into()
    } else {
        "none".into()
    }
}

/// Lê o `device/driver` de uma entrada de `/sys` e devolve o basename do driver.
fn driver_of(dir: &Path) -> String {
    std::fs::read_link(dir.join("device/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// Deteta o ambiente de virtualização atual (barato; pode ser chamado à vontade).
pub fn detect() -> VirtInfo {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let hv_flag = cpuinfo
        .lines()
        .any(|l| l.starts_with("flags") && l.contains(" hypervisor"));

    // virtio-net
    let mut virtio_net = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
        for e in rd.flatten() {
            if driver_of(&e.path()) == "virtio_net" {
                virtio_net.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    // virtio-blk
    let mut virtio_blk = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/block") {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if driver_of(&e.path()) == "virtio_blk" || name.starts_with("vd") {
                virtio_blk.push(name);
            }
        }
    }
    virtio_net.sort();
    virtio_blk.sort();
    virtio_blk.dedup();
    let virtio_count = std::fs::read_dir("/sys/bus/virtio/devices")
        .map(|r| r.flatten().count())
        .unwrap_or(0);
    let has_virtio = !virtio_net.is_empty() || !virtio_blk.is_empty() || virtio_count > 0;

    let product = read_trim("/sys/class/dmi/id/product_name");
    let sys_vendor = read_trim("/sys/class/dmi/id/sys_vendor");
    let xen_type = read_trim("/sys/hypervisor/type");
    let hypervisor = classify(&product, &sys_vendor, &xen_type, hv_flag, has_virtio);

    let virtualized = hypervisor != "none";
    let is_kvm = hypervisor == "kvm" || hypervisor == "qemu";
    let kvm_accel = Path::new("/dev/kvm").exists();

    VirtInfo {
        virtualized,
        hypervisor,
        is_kvm,
        kvm_accel,
        virtio_net,
        virtio_blk,
        virtio_count,
    }
}

/// Lê o escalonador de I/O atual de um disco e diz se convém pôr `none`.
///
/// Devolve `(atual, deve_pôr_none)`. Num guest KVM o host já escalona o I/O do
/// disco físico — manter um escalonador no guest só duplica trabalho e latência,
/// por isso `none` é o recomendado para `virtio-blk`.
pub fn blk_scheduler(dev: &str) -> Option<(String, bool)> {
    let raw = read_trim(&format!("/sys/block/{dev}/queue/scheduler"));
    if raw.is_empty() {
        return None;
    }
    // Formato do kernel: "[none] mq-deadline" — o ativo está entre [].
    let current = raw
        .split_whitespace()
        .find(|t| t.starts_with('['))
        .map(|t| t.trim_matches(|c| c == '[' || c == ']').to_string())
        .unwrap_or_else(|| raw.clone());
    let wants_none = current != "none" && raw.contains("none");
    Some((current, wants_none))
}

/// Aplica `none` ao escalonador de I/O de um disco virtio-blk (precisa de root).
pub fn set_blk_scheduler_none(dev: &str) -> std::io::Result<()> {
    std::fs::write(format!("/sys/block/{dev}/queue/scheduler"), "none")
}

#[cfg(test)]
mod tests {
    use super::{classify, driver_of};

    #[test]
    fn driver_of_resolves_symlink() {
        // Monta `<tmp>/eth0/device -> ../drivers/virtio_net` como o sysfs real
        // (`/sys/class/net/eth0/device/driver` aponta para o driver virtio).
        let root = std::env::temp_dir().join(format!("delonix-virt-{}", std::process::id()));
        let dev = root.join("eth0");
        let drvdir = root.join("drivers/virtio_net");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::create_dir_all(&drvdir).unwrap();
        // device/ é um dir; device/driver é o symlink para o driver.
        std::fs::create_dir_all(dev.join("device")).unwrap();
        std::os::unix::fs::symlink(&drvdir, dev.join("device/driver")).unwrap();
        assert_eq!(driver_of(&dev), "virtio_net");
        // sem symlink → string vazia (não-virtio).
        assert_eq!(driver_of(&root.join("nope")), "");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kvm_via_product_name() {
        assert_eq!(classify("KVM", "Red Hat", "", true, true), "kvm");
    }

    #[test]
    fn kvm_via_qemu_vendor_with_virtio() {
        assert_eq!(
            classify("Standard PC (Q35 + ICH9, 2009)", "QEMU", "", true, true),
            "kvm"
        );
    }

    #[test]
    fn qemu_tcg_without_virtio() {
        assert_eq!(classify("Standard PC", "QEMU", "", true, false), "qemu");
    }

    #[test]
    fn vmware_and_vbox_and_hyperv() {
        assert_eq!(
            classify("VMware Virtual Platform", "VMware, Inc.", "", true, false),
            "vmware"
        );
        assert_eq!(
            classify("VirtualBox", "innotek GmbH", "", true, false),
            "virtualbox"
        );
        assert_eq!(
            classify("Virtual Machine", "Microsoft Corporation", "", true, false),
            "hyper-v"
        );
    }

    #[test]
    fn xen_takes_priority() {
        assert_eq!(classify("HVM domU", "Xen", "xen", true, false), "xen");
    }

    #[test]
    fn bare_metal_is_none() {
        assert_eq!(
            classify("HP ZBook Power 15.6", "HP", "", false, false),
            "none"
        );
    }

    #[test]
    fn unknown_hypervisor_flag_only() {
        assert_eq!(
            classify("Some Cloud Instance", "Cloud Co", "", true, false),
            "unknown"
        );
    }
}
