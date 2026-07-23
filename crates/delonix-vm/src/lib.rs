//! `delonix-vm` — microVM runtime with a **selectable backend**:
//!
//! * **Cloud Hypervisor** (Rust VMM on top of `/dev/kvm`, runs rootless INSIDE the
//!   ingress infra netns — the `tap` lives there) — the historical backend.
//! * **libvirt/KVM** (QEMU managed by `libvirtd` via `virsh`) — 2nd backend, for
//!   hosts where libvirt is already the virtualization standard.
//!
//! The backend is chosen per VM: explicit (`VmConfig.backend`) or **auto-detection**
//! (prefers `cloud-hypervisor` if installed; otherwise `libvirt`). The per-VM state
//! ([`delonix_runtime_core::Vm`], persisted in `<base>/vms/<name>.json`) records the backend
//! that started it, in order to reconcile liveness/shutdown with the right backend.
//!
//! Networking: Cloud Hypervisor reuses the `delonix-net` *plumbing*
//! (`infra::vm_attach` creates a `tap` on the ingress bridge + DHCP). libvirt runs
//! QEMU under `libvirtd` (host netns), so it uses, in the MVP, **user-mode networking**
//! (SLIRP/passt: egress without a `tap`); integration with the ingress bridge (inbound
//! via the SDN) is a follow-up.

use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use delonix_net::infra;
use delonix_runtime_core::{Error, JsonStore, Result, Status, Vm};

/// CPU topology (`<cpu><topology sockets cores threads/></cpu>`).
#[derive(Debug, Clone, Default)]
pub struct CpuTopology {
    pub sockets: u32,
    pub cores: u32,
    pub threads: u32,
}

/// An extra disk attached to the VM (beyond the main overlay + cloud-init seed).
#[derive(Debug, Clone)]
pub struct ExtraDisk {
    /// Host path of the disk image.
    pub source: String,
    /// `"disk"` (default) or `"cdrom"`.
    pub device: String,
    /// Bus: `"virtio"` (default), `"sata"`, `"scsi"`, `"ide"`.
    pub bus: String,
    /// Image format: `"qcow2"` (default) or `"raw"`.
    pub format: String,
    /// Mount read-only.
    pub read_only: bool,
    /// Explicit target dev (e.g. `"vdb"`); auto-assigned when `None`.
    pub target: Option<String>,
}

/// An extra network interface (beyond the primary one derived from `net_mode`).
#[derive(Debug, Clone)]
pub struct ExtraNic {
    /// `"network"` (libvirt network), `"bridge"` (host bridge) or `"user"`.
    pub kind: String,
    /// Network/bridge name (for `network`/`bridge`).
    pub source: Option<String>,
    /// NIC model: `"virtio"` (default), `"e1000"`, `"rtl8139"`, …
    pub model: String,
    /// Fixed MAC (auto/random when `None`).
    pub mac: Option<String>,
}

/// Configuration to boot a microVM (flat fields, independent of the
/// `orchestrator` — the CLI translates the `VmSpec` into this).
#[derive(Debug, Clone, Default)]
pub struct VmConfig {
    /// Name (persistence key and of the deterministic `tap`/MAC).
    pub name: String,
    /// Base disk (qcow2/raw) — becomes a per-VM overlay.
    pub disk: String,
    /// vCPUs.
    pub vcpus: u32,
    /// Memory (e.g. `"2G"`, `"1024M"`).
    pub memory: String,
    /// Ingress network for the `tap`.
    pub network: String,
    /// Kernel for *direct boot* (vmlinux/bzImage).
    pub kernel: Option<String>,
    /// Initrd/initramfs (with `kernel`).
    pub initrd: Option<String>,
    /// Firmware (alternative to the kernel: rust-hypervisor-fw/EDK2 — for cloud images).
    pub firmware: Option<String>,
    /// Kernel command line (with `kernel`).
    pub cmdline: Option<String>,
    /// cloud-init *seed* ISO (NoCloud) — secondary disk.
    pub seed: Option<String>,
    /// Normalized restart policy (`"no"`|`"on-failure"`|`"always"`).
    pub restart_policy: Option<String>,
    // --- HPC (S4) ---------------------------------------------------------
    /// Backs the VM memory with *hugepages* (`--memory …,hugepages=on`). Reduces
    /// TLB misses and jitter in HPC workloads. Requires hugepages reserved on the host.
    pub hugepages: bool,
    /// CPU affinity (NUMA/pinning): list of host CPUs (e.g. `"8-15"`) to which
    /// ALL vCPUs are pinned (`--cpus …,affinity=<vcpu>@[<list>]`). Avoids
    /// vCPU migration between cores/NUMA nodes — latency determinism.
    pub cpu_affinity: Option<String>,
    /// PCI device passthrough (SR-IOV VF, GPU, …) via VFIO: sysfs paths
    /// (e.g. `/sys/bus/pci/devices/0000:65:00.1`). The VF must be pre-bound to
    /// `vfio-pci` on the host. Each one becomes a `--device path=…`.
    pub devices: Vec<String>,
    /// Virtualization backend: `Some("cloud-hypervisor")`, `Some("libvirt")` or
    /// `None` (auto-detection). Historical default = cloud-hypervisor.
    pub backend: Option<String>,
    /// Network mode of the **libvirt** backend (Cloud Hypervisor always uses the
    /// ingress `tap`). Abstracts the domain's `<interface>` — the user NEVER writes XML:
    ///   * `None`/`"user"` — user-mode network (SLIRP/passt): egress, no inbound IP.
    ///   * `"nat"`         — NAT network managed by libvirt (`<source network=…>`, DHCP +
    ///     IP via `virsh domifaddr`). Requires `qemu:///system` (root).
    ///   * `"bridge"`      — attaches to a host bridge (`bridge` below).
    pub net_mode: Option<String>,
    /// Name of the host bridge (mode `net_mode = "bridge"`) or of the libvirt network (mode
    /// `"nat"`; default `"default"`).
    pub bridge: Option<String>,
    /// Volumes/Storage shared into the VM (via **virtio-9p**). Each one
    /// comes already RESOLVED by the bin (the `Volume`/`Storage` name → host
    /// directory). Only the **libvirt** backend materializes them (Cloud Hypervisor does not do
    /// 9p) — see `create`. Closes the gap "mount a NAS into a VM without cloud-init/XML".
    pub volumes: Vec<VmVolume>,
    /// VNC graphical console (`--vnc`) — **libvirt backend only** (Cloud Hypervisor
    /// has no display). Binds to `127.0.0.1` on an auto port; see `vm vnc`.
    pub vnc: bool,
    /// Static IP (`--ip`) — libvirt `nat` mode only: materialized as a DHCP
    /// reservation (`<host mac=… ip=…/>`) on the libvirt network, so the guest
    /// needs NO cloud-init network config. Must belong to the network's subnet.
    pub static_ip: Option<String>,

    // --- Advanced libvirt knobs (libvirt backend only) ------------------------
    // Declarative `kind: Vm` parity with hand-written libvirt XML: typed fields
    // for the common cases + two raw-XML escape hatches for the long tail.
    /// Machine type (`<os><type machine=…>`), default `q35`.
    pub machine: Option<String>,
    /// CPU mode/model: `"host-passthrough"` (default), `"host-model"`, or a named
    /// model (e.g. `"Skylake-Server"`) → `<cpu mode='custom'>`.
    pub cpu_model: Option<String>,
    /// CPU topology (`<topology sockets cores threads/>`).
    pub cpu_topology: Option<CpuTopology>,
    /// Emulated TPM 2.0 (`<tpm>`) — needed by some guests (Windows/Secure Boot).
    pub tpm: bool,
    /// Video model (`<video><model type=…>`): `"virtio"`, `"qxl"`, `"vga"`,
    /// `"none"`. Overrides the default (virtio when `vnc`).
    pub video: Option<String>,
    /// OS boot device order (`<os><boot dev=…/>`): e.g. `["hd","cdrom","network"]`
    /// (ignored on direct-kernel boot).
    pub boot_order: Vec<String>,
    /// Extra disks beyond the main overlay + cloud-init seed.
    pub extra_disks: Vec<ExtraDisk>,
    /// Extra network interfaces beyond the primary one.
    pub extra_nics: Vec<ExtraNic>,
    /// Raw libvirt XML FRAGMENTS injected verbatim just before `</devices>` — the
    /// escape hatch for device knobs with no typed field. **UNVALIDATED**: a
    /// fragment can reference arbitrary host paths/devices, so only for TRUSTED
    /// manifests (same trust model as running an arbitrary disk image).
    pub libvirt_xml_overlay: Vec<String>,
    /// FULL `<domain>` override used VERBATIM (ignores everything generated from
    /// the fields above except the rootless seclabel injected at boot). The
    /// ultimate escape hatch — the author owns the entire XML. **UNVALIDATED**.
    pub libvirt_xml: Option<String>,
}

/// A host directory shared into the VM via virtio-9p. This is what
/// connects `kind: Volume`/`kind: Storage` to a VM without the user writing
/// cloud-init or XML: the bin resolves the name → `source` (the volume's `_data`, or the
/// mountpoint of a network Storage), and the engine generates the domain's `<filesystem>`
/// **and** the `mount` in the guest (via cloud-init) from these flat fields.
#[derive(Debug, Clone)]
pub struct VmVolume {
    /// 9p tag (short, unique in the VM) — the guest mounts by this tag.
    pub tag: String,
    /// Directory ON THE HOST to share (resolved by the bin).
    pub source: String,
    /// Mount point INSIDE the guest (e.g. `/mnt/dados`).
    pub mount_path: String,
    /// Mount read-only.
    pub read_only: bool,
}

// ===========================================================================
// Shared helpers
// ===========================================================================

fn vms_dir(base: &Path) -> std::path::PathBuf {
    base.join("vms")
}

fn store(base: &Path) -> Result<JsonStore<Vm>> {
    JsonStore::open(vms_dir(base))
}

/// `true` if the PID is alive (`/proc/<pid>` exists).
fn is_alive(pid: i32) -> bool {
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
}

/// `true` if a VM with this name already exists.
pub fn exists(base: &Path, name: &str) -> bool {
    store(base).map(|s| s.exists(name)).unwrap_or(false)
}

/// Converts memory (`"2G"`/`"1024M"`/`"512"`/`"2Gi"`) to MiB.
fn mem_mib(s: &str) -> u64 {
    let t = s.trim();
    // Tolerates the k8s-style `i` suffix (Gi/Mi): "2Gi" == "2G", "512Mi" == "512M".
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
        // Do not degrade silently: a mistyped value ("2GB", "2 Gi") would give
        // roughly half of the requested RAM without warning. Warn and use a safe default.
        Err(_) => {
            tracing::warn!(value = ?s, "invalid memory value; defaulting to 1024 MiB");
            1024
        }
    }
}

/// The host's `MemAvailable` in MiB (from `/proc/meminfo`) — memory that can be
/// given to new processes without swapping. `None` if unreadable.
fn host_mem_available_mib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib: u64 = s
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kib / 1024)
}

/// VM ADMISSION control: refuses to boot a VM if the requested memory does not
/// fit in the host's `MemAvailable` minus a safety reserve. Unlike
/// containers (with a budget in `delonix.slice`), a VM is a process
/// (cloud-hypervisor/qemu) that consumes host RAM DIRECTLY; without this
/// check, scheduling 30×2GB on a 32GB host would drown/OOM-kill the host. Since
/// `MemAvailable` already discounts the running VMs, the Nth VM that does not fit is
/// refused naturally. Reserve tunable via `DELONIX_VM_RESERVE_MIB`
/// (default 2048). Best-effort: if `/proc/meminfo` is unreadable, it does not block.
fn vm_admission_check(cfg: &VmConfig) -> Result<()> {
    let avail = match host_mem_available_mib() {
        Some(a) => a,
        None => return Ok(()),
    };
    let reserve = std::env::var("DELONIX_VM_RESERVE_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2048u64);
    let want = mem_mib(&cfg.memory);
    if want.saturating_add(reserve) > avail {
        return Err(Error::Runtime {
            context: "VM admission",
            message: format!(
                "host protection: VM '{}' asks for {want} MiB but the host only has {avail} MiB \
                 available (reserve {reserve} MiB). Stop VMs/containers, reduce the memory, \
                 or lower DELONIX_VM_RESERVE_MIB (at your own risk).",
                cfg.name
            ),
        });
    }
    Ok(())
}

/// Shell quoting (single-quote, escaping `'`).
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Deterministic MAC (QEMU/KVM prefix `52:54:00`) derived from the name.
fn mac_for(name: &str) -> String {
    let h = infra::name_hash(name);
    format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        (h >> 16) & 0xff,
        (h >> 8) & 0xff,
        h & 0xff
    )
}

/// `true` if running without root privileges (euid ≠ 0).
fn is_rootless() -> bool {
    // SAFETY: geteuid has no side effects.
    unsafe { libc::geteuid() != 0 }
}

/// `true` if a binary exists in `PATH`.
fn binary_in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(name).is_file()))
        .unwrap_or(false)
}

/// Extracts the format from the `file format: <fmt>` line of the HUMAN output of
/// `qemu-img info`. Pure function (testable without `qemu-img`).
///
/// NB: the human output is used on purpose — the modern `--output=json` nests a
/// `children` node with the protocol layer's `"format": "file"` BEFORE the
/// top-level `"format"`, and a naive parse would catch "file" instead of "qcow2". The
/// human output has a single `file format:` line (the top-level one).
fn parse_qemu_format(info: &str) -> Option<String> {
    for line in info.lines() {
        if let Some(rest) = line.trim().strip_prefix("file format:") {
            let f = rest.trim();
            if !f.is_empty() {
                return Some(f.to_string());
            }
        }
    }
    None
}

/// The REAL format of the base disk via `qemu-img info` — does NOT trust the extension.
/// Ubuntu/Debian cloud images are distributed as `*.img` but are **qcow2**
/// internally; an overlay created with `-F raw` over a qcow2 backing makes the
/// guest read the qcow2 as raw → corrupted / non-booting disk, silently.
/// Falls back to the extension heuristic if `qemu-img info` is not available.
pub fn disk_backing_format(disk: &Path) -> String {
    if let Ok(out) = Command::new("qemu-img").arg("info").arg(disk).output() {
        if out.status.success() {
            if let Some(fmt) = std::str::from_utf8(&out.stdout)
                .ok()
                .and_then(parse_qemu_format)
            {
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

/// Runs an external tool (e.g. `qemu-img`/`virsh`) CAPTURING stdout+stderr
/// (nothing leaks raw to the terminal) — surfacing the captured stderr in the
/// error. The `create` progress UI wants clean staged lines, not the raw
/// `Formatting '...qcow2'` / `Domain 'x' defined` chatter of `qemu-img`/`virsh`.
fn run_quiet(prog: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| Error::Runtime {
            context: "vm-tool",
            message: format!("{prog}: {e}"),
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim().trim_start_matches("error: ").trim();
        return Err(Error::Runtime {
            context: "vm-tool",
            message: if err.is_empty() {
                format!("{prog} failed")
            } else {
                format!("{prog}: {err}")
            },
        });
    }
    Ok(())
}

/// Stages emitted by [`create_with`] so a caller can render step-by-step
/// progress. The engine emits ONLY the enum — the user-facing text and its
/// translation stay in `delonix-runtime-bin` (project rule: UI strings live in
/// the bin, not in the mechanism crates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateStage {
    /// Preparing the per-VM overlay disk (`qemu-img create`).
    Disk,
    /// Ensuring/attaching the network (libvirt NAT net, or the SDN tap).
    Network,
    /// Defining the domain in the hypervisor.
    Define,
    /// Starting the domain.
    Start,
}

/// Runs a command and captures stdout (trimmed), or `None` on failure.
fn capture(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(prog).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Pure parser for `virsh net-dhcp-leases` output: among the entries matching
/// `mac` (case-insensitive), returns the address of the one with the LATEST
/// `Expiry Time`. See [`LibvirtBackend::ip_from_leases`] for why this is the
/// only reliable signal (`domifaddr` can list several stale entries for the
/// same MAC in no useful order). The expiry format (`YYYY-MM-DD HH:MM:SS`) is
/// zero-padded and lexicographically sortable — plain string `max` is exact,
/// no date parsing needed.
fn parse_leases_latest_ip(out: &str, mac: &str) -> Option<String> {
    let mac_lower = mac.to_ascii_lowercase();
    out.lines()
        .filter_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            // "<date> <time> <mac> ipv4 <addr>/<prefix> ..." — at least 5 cols.
            if cols.len() < 5 || cols[2].to_ascii_lowercase() != mac_lower {
                return None;
            }
            let expiry = format!("{} {}", cols[0], cols[1]);
            let ip = cols[4].split_once('/').map(|(ip, _)| ip)?;
            ip.parse::<std::net::Ipv4Addr>().ok()?;
            Some((expiry, ip.to_string()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, ip)| ip)
}

// ===========================================================================
// Backend trait
// ===========================================================================

/// What a backend produced when booting a VM — persisted in the [`Vm`].
pub struct Boot {
    /// PID of the VMM on the host (Cloud Hypervisor). `None` when managed by a daemon
    /// (libvirt) — there the liveness comes from `is_running`.
    pub pid: Option<i32>,
    /// `tap` interface (or `"user"` for libvirt user-mode networking).
    pub tap: String,
    /// NIC MAC.
    pub mac: String,
    /// Control socket (Cloud Hypervisor API; empty on libvirt).
    pub api_socket: String,
    /// The VM's IP, if known at boot.
    pub ip: Option<String>,
}

/// The virtualization mechanism behind a microVM. Allows having Cloud
/// Hypervisor and libvirt/KVM side by side (chosen per VM).
pub trait VmBackend {
    /// Stable identifier persisted in the [`Vm`].
    fn id(&self) -> &'static str;
    /// `true` if the backend has the required tools installed.
    fn available(&self) -> bool;
    /// Creates the network (if applicable) and boots the VM from the `overlay`. The overlay
    /// creation and idempotency are handled by [`create`]. `on` receives the
    /// sub-stages (network/define/start) for a progress UI.
    fn boot(
        &self,
        vmdir: &Path,
        cfg: &VmConfig,
        overlay: &str,
        on: &dyn Fn(CreateStage),
    ) -> Result<Boot>;
    /// Is the VM still alive?
    fn is_running(&self, vm: &Vm) -> bool;
    /// Current IP of the VM (may change/resolve later via DHCP).
    fn ip(&self, vm: &Vm) -> Option<String>;
    /// Stops the VM and frees the network resources. Returns `Err` when the backend
    /// REFUSED the cleanup (e.g. libvirt) — the caller decides whether to abort (so as not to
    /// delete the local record of a VM that is still defined in the hypervisor) or
    /// to ignore it (`vm rm --force`).
    fn stop(&self, vmdir: &Path, vm: &Vm) -> Result<()>;
}

/// Selects a backend from an explicit request or by auto-detection
/// (prefers cloud-hypervisor if installed; otherwise libvirt).
pub fn select_backend(want: Option<&str>) -> Result<Box<dyn VmBackend>> {
    match want.map(|s| s.trim().to_lowercase()).as_deref() {
        Some("cloud-hypervisor") | Some("ch") | Some("cloudhypervisor") => {
            Ok(Box::new(CloudHypervisorBackend))
        }
        Some("libvirt") | Some("kvm") | Some("qemu") => Ok(Box::new(LibvirtBackend)),
        Some(other) if !other.is_empty() => Err(Error::Invalid(format!(
            "unknown VM backend: '{other}' (use 'cloud-hypervisor' or 'libvirt')"
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
                "no VM backend available: install 'cloud-hypervisor' or 'libvirt'+'qemu'".into(),
            ))
        }
    }
}

/// The backend that started an already-persisted VM (for liveness/stop).
fn backend_for(vm: &Vm) -> Box<dyn VmBackend> {
    match vm.backend.as_str() {
        "libvirt" => Box::new(LibvirtBackend),
        _ => Box::new(CloudHypervisorBackend),
    }
}

// ===========================================================================
// Backend: Cloud Hypervisor
// ===========================================================================

/// Builds the Cloud Hypervisor `--memory` argument (with `hugepages=on` if
/// requested). Pure function — tested without hardware.
fn memory_arg(cfg: &VmConfig) -> String {
    let mut a = format!("size={}M", mem_mib(&cfg.memory));
    if cfg.hugepages {
        a.push_str(",hugepages=on");
    }
    a
}

/// Builds the Cloud Hypervisor `--cpus` argument. With `cpu_affinity`, pins
/// each vCPU to the same list of host CPUs (`affinity=0@[list],1@[list],…`).
/// Pure function — tested without hardware.
fn cpus_arg(cfg: &VmConfig) -> String {
    let n = cfg.vcpus.max(1);
    let mut a = format!("boot={n}");
    if let Some(list) = &cfg.cpu_affinity {
        let aff: Vec<String> = (0..n).map(|v| format!("{v}@[{list}]")).collect();
        a.push_str(&format!(",affinity={}", aff.join(":")));
    }
    a
}

/// Historical backend: Cloud Hypervisor inside the infra netns (rootless).
pub struct CloudHypervisorBackend;

impl VmBackend for CloudHypervisorBackend {
    fn id(&self) -> &'static str {
        "cloud-hypervisor"
    }

    fn available(&self) -> bool {
        binary_in_path("cloud-hypervisor")
    }

    fn boot(
        &self,
        vmdir: &Path,
        cfg: &VmConfig,
        overlay: &str,
        on: &dyn Fn(CreateStage),
    ) -> Result<Boot> {
        // Cloud Hypervisor does not support virtio-9p (only virtio-fs, which requires the
        // virtiofsd daemon, not yet wired up). `spec.volumes` on a CH VM is a
        // clear error instead of a silently ignored mount — the bin
        // auto-selects libvirt when there are volumes, so this only fires
        // if the user FORCES `backend: cloud-hypervisor` with volumes.
        if !cfg.volumes.is_empty() {
            return Err(Error::Invalid(format!(
                "VM '{}': spec.volumes requires the libvirt backend (Cloud Hypervisor does not do virtio-9p) — remove `backend: cloud-hypervisor` or the volumes",
                cfg.name
            )));
        }
        // Own private network when named (≠ shared ingress): ensures its
        // isolated bridge + DHCP before the attach. The VMs' SDN lives here.
        on(CreateStage::Network);
        if !matches!(cfg.network.as_str(), "" | "ingress" | "bridge" | "default") {
            let _ = infra::network_create(&cfg.network);
        }
        let tap = infra::vm_attach(&cfg.name, &cfg.network)?;
        let mac = mac_for(&cfg.name);
        on(CreateStage::Start);
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

    fn stop(&self, _vmdir: &Path, vm: &Vm) -> Result<()> {
        if let Some(pid) = vm.pid {
            if pid > 0 {
                // SAFETY: sending SIGTERM to a PID is safe; the error is ignored.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
        infra::vm_detach(&vm.name);
        Ok(())
    }
}

/// Boots `cloud-hypervisor` INSIDE the infra netns, in the background, and returns
/// the PID (real, visible on the host).
/// Locates the `rust-hypervisor-fw` that the installer places (or one pointed to by
/// `$DELONIX_HYPERVISOR_FW`), so Cloud Hypervisor can boot cloud images without an
/// explicit `--firmware`. Returns the 1st existing path, or `None`.
fn default_ch_firmware() -> Option<String> {
    if let Ok(p) = std::env::var("DELONIX_HYPERVISOR_FW") {
        if !p.is_empty() && Path::new(&p).exists() {
            return Some(p);
        }
    }
    for p in [
        "/usr/local/share/delonix/hypervisor-fw",
        "/usr/share/delonix/hypervisor-fw",
    ] {
        if Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    None
}

/// Path of the UNIX socket of the serial console of a Cloud Hypervisor VM
/// (`<base>/vms/<name>.console`). `delonix vm console` connects here.
pub fn console_socket(base: &Path, name: &str) -> std::path::PathBuf {
    base.join("vms").join(format!("{name}.console"))
}

fn boot_ch(vmdir: &Path, cfg: &VmConfig, overlay: &str, tap: &str, mac: &str) -> Result<i32> {
    let join = infra::infra_join_argv().ok_or_else(|| Error::Runtime {
        context: "vm",
        message: "the ingress (rootless infra) is not up".into(),
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
    // Boot: kernel (direct boot) OR firmware (cloud images with a bootloader).
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
    } else if let Some(fw) = cfg.firmware.clone().or_else(default_ch_firmware) {
        // Without an explicit kernel or firmware: a cloud image (the golden) needs
        // firmware for CH to boot (unlike libvirt, which falls back to
        // BIOS). The `rust-hypervisor-fw` that the installer provides is resolved —
        // so `vm create` with the golden boots without flags.
        ch.push("--firmware".into());
        ch.push(fw);
    } else {
        return Err(Error::Invalid(
            "VM without 'kernel' or 'firmware' and no rust-hypervisor-fw found — reinstall (curl install.sh) to fetch it, pass `--firmware <path>`, or use `--backend libvirt`".into(),
        ));
    }
    ch.push("--disk".into());
    // `image_type=qcow2,backing_files=on` is MANDATORY: recent versions of
    // Cloud Hypervisor (real finding via `validate-rootless`, v52) refuse by
    // default any qcow2 with a `backing_file` (the per-VM overlay that `create`
    // always generates) with the misleading error "Maximum disk nesting depth exceeded"
    // — it is not about real nesting depth, it is CH's new security opt-in
    // for backing file chains. Without this, NO VM with an overlay
    // boots.
    ch.push(format!("path={overlay},image_type=qcow2,backing_files=on"));
    if let Some(seed) = &cfg.seed {
        ch.push("--disk".into());
        ch.push(format!("path={seed}"));
    }
    ch.push("--cpus".into());
    ch.push(cpus_arg(cfg)); // boot=N [+ affinity for NUMA/CPU pinning]
    ch.push("--memory".into());
    ch.push(memory_arg(cfg)); // size=XM [+ hugepages=on]
                              // SR-IOV / VFIO: passes each PCI device pre-bound to vfio-pci.
    for dev in &cfg.devices {
        ch.push("--device".into());
        ch.push(format!("path={dev}"));
    }
    ch.push("--net".into());
    ch.push(format!("tap={tap},mac={mac}"));
    // Serial on a UNIX SOCKET (not a log file): this is what enables an
    // INTERACTIVE console (`delonix vm console`) — CH accepts bytes in both
    // directions over the socket. The boot and the getty (ttyS0) appear here.
    let console = console_socket(vmdir.parent().unwrap_or(vmdir), &cfg.name);
    let _ = std::fs::remove_file(&console);
    ch.push("--serial".into());
    ch.push(format!("socket={}", console.display()));
    ch.push("--console".into());
    ch.push("off".into());
    let _ = &serial; // (the serial log file gave way to the socket)

    // background inside the netns; no pid-ns ⇒ $! is the real PID on the host.
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
            message: "failed to launch cloud-hypervisor (KVM/binary available?)".into(),
        });
    }
    // short wait for the pidfile.
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
            message: "cloud-hypervisor did not report a PID (check the VM log)".into(),
        });
    }
    Ok(pid)
}

// ===========================================================================
// Backend: libvirt / KVM (QEMU sob libvirtd, via virsh)
// ===========================================================================

/// 2nd backend: QEMU/KVM managed by `libvirtd`, controlled via `virsh`.
pub struct LibvirtBackend;

/// libvirt connection URI: user session (rootless) or system (root).
/// Which libvirt connection to use for a *new* domain, given its `net_mode`.
///
/// `qemu:///session` (per-user libvirt) can ONLY do user-mode networking
/// (SLIRP/passt): its 10.0.2.x address is invisible to `virsh domifaddr` and
/// unreachable from the host. NAT and host-bridge networks — the ones that
/// yield a discoverable, reachable IP — live ONLY in `qemu:///system`. So a VM
/// that asks for `net_mode: nat|bridge` must go to the system connection even
/// when we're otherwise rootless (the invoking user needs to be in the
/// `libvirt` group; otherwise `virsh` fails loudly, which is the honest signal).
fn libvirt_uri_for(net_mode: Option<&str>) -> &'static str {
    match net_mode {
        Some("nat") | Some("network") | Some("bridge") => "qemu:///system",
        _ if is_rootless() => "qemu:///session",
        _ => "qemu:///system",
    }
}

/// Which connection a *already-defined* domain lives on. `net_mode` isn't
/// persisted in the `Vm` record, so we discover it: whichever of system/session
/// knows the domain. Prefer `system` (reachable-IP modes) and fall back to
/// `session` (user-mode). Returns `session` if neither defines it (harmless —
/// the caller's virsh op is then a no-op).
/// The URI of the libvirt connection (`qemu:///system` or `.../session`) where the domain
/// `name` lives — so the bin (`vm console`/`vm vnc`) talks to virsh on the
/// RIGHT connection (otherwise `virsh console` without `-c` uses the default and gives "failed to
/// get domain" when the domain is on the other one).
pub fn libvirt_uri(name: &str) -> String {
    libvirt_uri_of(name).to_string()
}

fn libvirt_uri_of(name: &str) -> &'static str {
    if let Some(uri) = libvirt_domain_uri(name) {
        return uri;
    }
    if is_rootless() {
        "qemu:///session"
    } else {
        "qemu:///system"
    }
}

/// `true` if this user can use the libvirt SYSTEM connection (the `libvirt`
/// group, or root). This is what decides the default network mode: `nat`
/// (reachable DHCP IP) instead of user-mode (no visible IP at all).
fn system_libvirt_usable() -> bool {
    capture("virsh", &["-c", "qemu:///system", "uri"]).is_some()
}

/// DHCP reservation MAC→IP on the libvirt network `net` (nat mode) — the
/// static `--ip` path with NO cloud-init network config. Idempotent: if an
/// entry for this MAC exists, modify it; clear error when the IP does not
/// belong to the network's subnet (virsh itself validates that).
fn libvirt_reserve_ip(uri: &str, net: &str, mac: &str, ip: &str) -> Result<()> {
    let entry = format!("<host mac='{mac}' ip='{ip}'/>");
    let args = |verb: &'static str| {
        // Flags BEFORE `--`: after the terminator virsh reads everything as
        // positional data ("unexpected data '--config'", real error).
        vec![
            "-c",
            uri,
            "net-update",
            "--live",
            "--config",
            "--",
            net,
            verb,
            "ip-dhcp-host",
            &entry,
        ]
    };
    if quiet("virsh", &args("add-last")).is_ok() || quiet("virsh", &args("modify")).is_ok() {
        return Ok(());
    }
    // Report with virsh's reason (retrying add-last), never raw stderr.
    let msg = quiet("virsh", &args("add-last"))
        .err()
        .unwrap_or_else(|| "unknown error".into());
    Err(Error::Invalid(format!(
        "could not reserve static IP {ip} on libvirt network '{net}': {msg}"
    )))
}

/// The connection where the domain `name` is DEFINED, if any — unlike
/// [`libvirt_uri_of`], **without** a fallback. `None` = libvirt does not know the VM.
fn libvirt_domain_uri(name: &str) -> Option<&'static str> {
    ["qemu:///system", "qemu:///session"]
        .into_iter()
        .find(|uri| capture("virsh", &["-c", uri, "domstate", "--", name]).is_some())
}

/// Runs a command capturing stdout AND stderr — nothing from `virsh` leaks raw to
/// the terminal (it was the `error: Failed to destroy domain …` that appeared in the middle
/// of the `vm rm` output). On failure it returns the 1st useful stderr line, without the
/// virsh `error: ` prefix, to compose clear messages.
fn quiet(prog: &str, args: &[&str]) -> std::result::Result<String, String> {
    match Command::new(prog).args(args).output() {
        Ok(out) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let line = err
                .lines()
                .map(|l| l.trim())
                .map(|l| l.strip_prefix("error: ").unwrap_or(l))
                .find(|l| !l.is_empty())
                .unwrap_or("unknown error");
            Err(line.to_string())
        }
        Err(e) => Err(format!("{prog}: {e}")),
    }
}

/// Powers off the domain (`virsh destroy`) only if it is NOT already "shut off" —
/// idempotent and silent (destroy on a stopped domain is an error in virsh, and was
/// one of the raw messages that `vm rm` let escape).
fn libvirt_poweroff(uri: &str, name: &str) -> Result<()> {
    let state = capture("virsh", &["-c", uri, "domstate", "--", name]).unwrap_or_default();
    if state.is_empty() || state == "shut off" {
        return Ok(());
    }
    quiet("virsh", &["-c", uri, "destroy", "--", name])
        .map(|_| ())
        .map_err(|msg| Error::Runtime {
            context: "vm",
            message: format!("could not power off VM '{name}': {msg}"),
        })
}

/// Completely removes the libvirt domain `name`, if it exists: powers it off and does
/// `undefine` with the flags that clean up state attached to the domain (managed
/// save, snapshot metadata, NVRAM). Without `--managed-save`, a domain
/// suspended by the host (`virsh managedsave`/libvirt-guests at shutdown) makes
/// virsh REFUSE the undefine — and the old version ignored that refusal, deleted
/// the local record anyway and left the VM orphaned in libvirt. Idempotent:
/// non-existent domain → `Ok`.
fn libvirt_cleanup(name: &str) -> Result<()> {
    let Some(uri) = libvirt_domain_uri(name) else {
        return Ok(());
    };
    libvirt_poweroff(uri, name)?;
    if quiet(
        "virsh",
        &[
            "-c",
            uri,
            "undefine",
            "--managed-save",
            "--snapshots-metadata",
            "--nvram",
            "--",
            name,
        ],
    )
    .is_ok()
    {
        return Ok(());
    }
    // old virsh without some of the flags above: the plain undefine still covers the
    // common case (without managed save).
    quiet("virsh", &["-c", uri, "undefine", "--", name])
        .map(|_| ())
        .map_err(|msg| Error::Runtime {
            context: "vm",
            message: format!("could not remove VM '{name}' from libvirt ({uri}): {msg}"),
        })
}

/// Generates the libvirt (KVM) domain XML. **Pure function** — tested without a daemon.
///
/// Covers: vCPUs (+ pinning via `<cputune>`), memory (+ hugepages via
/// `<memoryBacking>`), virtio disk (qcow2 overlay), cloud-init seed (cdrom),
/// virtio user-mode network (rootless egress), serial console, and VFIO passthrough of
/// PCI devices (`<hostdev>`).
pub fn libvirt_domain_xml(cfg: &VmConfig, overlay: &str, mac: &str) -> String {
    // Full-domain escape hatch: the manifest author owns the entire XML. The
    // rootless seclabel is still injected at boot (`create`, via the </domain>
    // replace), so a full override keeps working under system libvirt.
    if let Some(raw) = &cfg.libvirt_xml {
        return raw.clone();
    }
    let mib = mem_mib(&cfg.memory);
    let kib = mib * 1024;
    let vcpus = cfg.vcpus.max(1);
    let name = xml_escape(&cfg.name);

    let mut s = String::new();
    s.push_str("<domain type='kvm'>\n");
    s.push_str(&format!("  <name>{name}</name>\n"));
    s.push_str(&format!("  <memory unit='KiB'>{kib}</memory>\n"));
    s.push_str(&format!(
        "  <currentMemory unit='KiB'>{kib}</currentMemory>\n"
    ));
    // hugepages (HPC): backs the domain's RAM with host hugepages.
    if cfg.hugepages {
        s.push_str("  <memoryBacking>\n    <hugepages/>\n  </memoryBacking>\n");
    }
    s.push_str(&format!("  <vcpu placement='static'>{vcpus}</vcpu>\n"));
    // CPU pinning (NUMA/determinism): pins each vCPU to the list of host CPUs.
    if let Some(list) = &cfg.cpu_affinity {
        let list = xml_escape(list);
        s.push_str("  <cputune>\n");
        for v in 0..vcpus {
            s.push_str(&format!("    <vcpupin vcpu='{v}' cpuset='{list}'/>\n"));
        }
        s.push_str("  </cputune>\n");
    }
    // Boot: firmware (cloud images) or direct kernel.
    let machine = cfg.machine.as_deref().unwrap_or("q35");
    s.push_str(&format!(
        "  <os>\n    <type arch='x86_64' machine='{}'>hvm</type>\n",
        xml_escape(machine)
    ));
    if let Some(k) = &cfg.kernel {
        s.push_str(&format!("    <kernel>{}</kernel>\n", xml_escape(k)));
        if let Some(i) = &cfg.initrd {
            s.push_str(&format!("    <initrd>{}</initrd>\n", xml_escape(i)));
        }
        let cmdline = cfg
            .cmdline
            .clone()
            .unwrap_or_else(|| "console=ttyS0 root=/dev/vda1 rw".into());
        s.push_str(&format!(
            "    <cmdline>{}</cmdline>\n",
            xml_escape(&cmdline)
        ));
    } else if let Some(fw) = &cfg.firmware {
        s.push_str(&format!(
            "    <loader readonly='yes' type='pflash'>{}</loader>\n",
            xml_escape(fw)
        ));
    }
    // Boot device order (firmware/disk boot only — irrelevant with a direct
    // kernel). Explicit `boot_order` wins; otherwise the default is `hd`.
    if cfg.kernel.is_none() {
        if cfg.boot_order.is_empty() {
            s.push_str("    <boot dev='hd'/>\n");
        } else {
            for d in &cfg.boot_order {
                s.push_str(&format!("    <boot dev='{}'/>\n", xml_escape(d)));
            }
        }
    }
    s.push_str("  </os>\n");
    s.push_str("  <features>\n    <acpi/>\n    <apic/>\n  </features>\n");
    s.push_str(&libvirt_cpu_xml(cfg));
    s.push_str("  <clock offset='utc'/>\n");
    s.push_str("  <on_poweroff>destroy</on_poweroff>\n");
    // restart policy: 'always'/'on-failure' → restart on crash.
    let on_crash = match cfg.restart_policy.as_deref() {
        Some("always") | Some("on-failure") => "restart",
        _ => "destroy",
    };
    s.push_str(&format!(
        "  <on_reboot>restart</on_reboot>\n  <on_crash>{on_crash}</on_crash>\n"
    ));
    s.push_str("  <devices>\n");
    s.push_str("    <emulator>/usr/bin/qemu-system-x86_64</emulator>\n");
    // main disk: qcow2 overlay via virtio (vda). The backing file (the base
    // image) is declared EXPLICITLY: on Ubuntu the per-domain AppArmor profile
    // (virt-aa-helper) only whitelists paths present in the XML — without
    // <backingStore>, QEMU opened the overlay but got EPERM on the backing
    // qcow2 ("Could not open …vm-images/…: Permission denied", real report).
    s.push_str("    <disk type='file' device='disk'>\n");
    s.push_str("      <driver name='qemu' type='qcow2'/>\n");
    s.push_str(&format!("      <source file='{}'/>\n", xml_escape(overlay)));
    if !cfg.disk.is_empty() {
        let base = std::fs::canonicalize(&cfg.disk)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| cfg.disk.clone());
        let fmt = disk_backing_format(Path::new(&base));
        s.push_str("      <backingStore type='file'>\n");
        s.push_str(&format!("        <format type='{}'/>\n", xml_escape(&fmt)));
        s.push_str(&format!("        <source file='{}'/>\n", xml_escape(&base)));
        s.push_str("      </backingStore>\n");
    }
    s.push_str("      <target dev='vda' bus='virtio'/>\n");
    s.push_str("    </disk>\n");
    // cloud-init seed (NoCloud) as cdrom.
    if let Some(seed) = &cfg.seed {
        s.push_str("    <disk type='file' device='cdrom'>\n");
        s.push_str("      <driver name='qemu' type='raw'/>\n");
        s.push_str(&format!("      <source file='{}'/>\n", xml_escape(seed)));
        s.push_str("      <target dev='sda' bus='sata'/>\n");
        s.push_str("      <readonly/>\n    </disk>\n");
    }
    // Extra disks (typed): additional images beyond the main overlay + seed.
    // Target devs are auto-assigned per bus (vdb, vdc… for virtio; sdb… for
    // sata/scsi) unless the user pinned one — `vda`/`sda` stay reserved above.
    let mut vd = b'b'; // next virtio letter (vda taken by the main disk)
    let mut sd = b'b'; // next sata/scsi letter (sda taken by the seed cdrom)
    for d in &cfg.extra_disks {
        let bus = if d.bus.is_empty() { "virtio" } else { &d.bus };
        let device = if d.device.is_empty() {
            "disk"
        } else {
            &d.device
        };
        let fmt = if d.format.is_empty() {
            "qcow2"
        } else {
            &d.format
        };
        let target = match &d.target {
            Some(t) => t.clone(),
            None if bus == "virtio" => {
                let t = format!("vd{}", vd as char);
                vd += 1;
                t
            }
            None => {
                let t = format!("sd{}", sd as char);
                sd += 1;
                t
            }
        };
        s.push_str(&format!(
            "    <disk type='file' device='{}'>\n",
            xml_escape(device)
        ));
        s.push_str(&format!(
            "      <driver name='qemu' type='{}'/>\n",
            xml_escape(fmt)
        ));
        s.push_str(&format!(
            "      <source file='{}'/>\n",
            xml_escape(&d.source)
        ));
        s.push_str(&format!(
            "      <target dev='{}' bus='{}'/>\n",
            xml_escape(&target),
            xml_escape(bus)
        ));
        if d.read_only {
            s.push_str("      <readonly/>\n");
        }
        s.push_str("    </disk>\n");
    }
    // volumes/Storage shared via virtio-9p — the user does NOT write this
    // XML: it comes from `spec.volumes` already resolved. The guest mounts by `<target dir=tag>`
    // (the mount is injected into cloud-init, see `cmd::vm::build_user_data`).
    for v in &cfg.volumes {
        s.push_str("    <filesystem type='mount' accessmode='passthrough'>\n");
        s.push_str(&format!(
            "      <source dir='{}'/>\n",
            xml_escape(&v.source)
        ));
        s.push_str(&format!("      <target dir='{}'/>\n", xml_escape(&v.tag)));
        if v.read_only {
            s.push_str("      <readonly/>\n");
        }
        s.push_str("    </filesystem>\n");
    }
    // network: abstracted by the YAML (net_mode) → virtio `<interface>`. No hand-written XML.
    s.push_str(&libvirt_interface_xml(cfg, mac));
    // Extra NICs (typed): additional interfaces beyond the primary one.
    for n in &cfg.extra_nics {
        let model = if n.model.is_empty() {
            "virtio"
        } else {
            &n.model
        };
        let (itype, src) = match n.kind.as_str() {
            "bridge" => (
                "bridge",
                n.source
                    .as_deref()
                    .map(|b| format!("      <source bridge='{}'/>\n", xml_escape(b))),
            ),
            "user" => ("user", None),
            _ => (
                "network",
                Some(format!(
                    "      <source network='{}'/>\n",
                    xml_escape(n.source.as_deref().unwrap_or("default"))
                )),
            ),
        };
        s.push_str(&format!("    <interface type='{itype}'>\n"));
        if let Some(src) = src {
            s.push_str(&src);
        }
        if let Some(m) = &n.mac {
            s.push_str(&format!("      <mac address='{}'/>\n", xml_escape(m)));
        }
        s.push_str(&format!(
            "      <model type='{}'/>\n    </interface>\n",
            xml_escape(model)
        ));
    }
    // serial console (boot logs).
    s.push_str("    <serial type='pty'><target type='isa-serial' port='0'/></serial>\n");
    s.push_str("    <console type='pty'><target type='serial' port='0'/></console>\n");
    // Emulated TPM 2.0 (opt-in) — some guests (Windows, Secure Boot) require it.
    if cfg.tpm {
        s.push_str("    <tpm model='tpm-crb'>\n      <backend type='emulator' version='2.0'/>\n    </tpm>\n");
    }
    // VNC (opt-in): auto port, loopback only (`vm vnc` reports host:port).
    if cfg.vnc {
        s.push_str("    <graphics type='vnc' port='-1' autoport='yes' listen='127.0.0.1'/>\n");
    }
    // Video: explicit model overrides; else the default virtio head when VNC is
    // on. `"none"` suppresses the device entirely.
    match cfg.video.as_deref() {
        Some("none") => {}
        Some(m) => s.push_str(&format!(
            "    <video><model type='{}' heads='1'/></video>\n",
            xml_escape(m)
        )),
        None if cfg.vnc => s.push_str("    <video><model type='virtio' heads='1'/></video>\n"),
        None => {}
    }
    // VFIO: PCI device passthrough (SR-IOV VF, GPU).
    for dev in &cfg.devices {
        if let Some((dom, bus, slot, func)) = parse_pci_addr(dev) {
            s.push_str("    <hostdev mode='subsystem' type='pci' managed='yes'>\n      <source>\n");
            s.push_str(&format!(
                "        <address domain='0x{dom}' bus='0x{bus}' slot='0x{slot}' function='0x{func}'/>\n"
            ));
            s.push_str("      </source>\n    </hostdev>\n");
        }
    }
    // Raw XML fragments (escape hatch) injected verbatim before </devices> — the
    // long tail of libvirt device knobs with no typed field. UNVALIDATED: trusted
    // manifests only (a fragment can name arbitrary host paths/devices).
    for frag in &cfg.libvirt_xml_overlay {
        s.push_str(frag);
        if !frag.ends_with('\n') {
            s.push('\n');
        }
    }
    s.push_str("  </devices>\n");
    s.push_str("</domain>\n");
    s
}

/// The domain's `<cpu>` element from `cpu_model` + `cpu_topology`. **Pure**.
/// `host-passthrough` (default) exposes the host CPU exactly; `host-model`
/// asks libvirt for the closest named model; anything else is a custom model.
fn libvirt_cpu_xml(cfg: &VmConfig) -> String {
    let topo = cfg.cpu_topology.as_ref().map(|t| {
        format!(
            "    <topology sockets='{}' cores='{}' threads='{}'/>\n",
            t.sockets.max(1),
            t.cores.max(1),
            t.threads.max(1)
        )
    });
    match cfg.cpu_model.as_deref().unwrap_or("host-passthrough") {
        "host-passthrough" => match topo {
            Some(t) => format!("  <cpu mode='host-passthrough' check='none'>\n{t}  </cpu>\n"),
            None => "  <cpu mode='host-passthrough' check='none'/>\n".into(),
        },
        "host-model" => match topo {
            Some(t) => format!("  <cpu mode='host-model' check='partial'>\n{t}  </cpu>\n"),
            None => "  <cpu mode='host-model' check='partial'/>\n".into(),
        },
        named => format!(
            "  <cpu mode='custom' match='exact' check='partial'>\n    <model fallback='allow'>{}</model>\n{}  </cpu>\n",
            xml_escape(named),
            topo.unwrap_or_default()
        ),
    }
}

/// Generates the libvirt domain's `<interface>` from the YAML `net_mode` — so the
/// network is 100% abstracted (no hand-written XML). **Pure function** — tested without a daemon.
fn libvirt_interface_xml(cfg: &VmConfig, mac: &str) -> String {
    let mac = xml_escape(mac);
    let model = "      <model type='virtio'/>\n    </interface>\n";
    match cfg.net_mode.as_deref().unwrap_or("user") {
        "nat" | "network" => {
            // NAT network managed by libvirt (DHCP + IP via domifaddr). `bridge` = name
            // of the libvirt network (default "default").
            let net = cfg.bridge.as_deref().unwrap_or("default");
            format!(
                "    <interface type='network'>\n      <source network='{}'/>\n      <mac address='{mac}'/>\n{model}",
                xml_escape(net)
            )
        }
        "bridge" => {
            // attaches to a pre-existing host bridge.
            let br = cfg.bridge.as_deref().unwrap_or("virbr0");
            format!(
                "    <interface type='bridge'>\n      <source bridge='{}'/>\n      <mac address='{mac}'/>\n{model}",
                xml_escape(br)
            )
        }
        _ => {
            // user-mode (SLIRP/passt): egress without a tap — rootless-friendly (default).
            format!("    <interface type='user'>\n      <mac address='{mac}'/>\n{model}")
        }
    }
}

/// Escapes the 5 special XML characters.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Extracts `(domain, bus, slot, func)` from a PCI path/address
/// (`/sys/bus/pci/devices/0000:65:00.1` or `0000:65:00.1`). Pure function.
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

/// Ensures a ready NAT libvirt network (`--net-mode nat` → host-pingable IP).
/// Best-effort: if `net` does not exist and is the `default`, it defines the standard NAT
/// network (virbr0, 192.168.122.0/24, DHCP); then `net-start` + `net-autostart`. A
/// clear warning if the system connection is unreachable (missing the libvirt group).
fn ensure_libvirt_network(uri: &str, net: &str) {
    // System connection reachable? (NAT lives in qemu:///system.)
    if capture("virsh", &["-c", uri, "net-list", "--all"]).is_none() {
        eprintln!(
            "warning: cannot reach {uri} for NAT networking — add yourself to the 'libvirt' group              (`sudo usermod -aG libvirt $USER && newgrp libvirt`) and retry"
        );
        return;
    }
    let exists = capture("virsh", &["-c", uri, "net-info", "--", net]).is_some();
    if !exists && net == "default" {
        // XML of the standard libvirt NAT network (the one most distros ship).
        let xml = "<network>\n  <name>default</name>\n  <forward mode='nat'/>\n                     <bridge name='virbr0' stp='on' delay='0'/>\n                     <ip address='192.168.122.1' netmask='255.255.255.0'>\n                       <dhcp><range start='192.168.122.2' end='192.168.122.254'/></dhcp>\n                     </ip>\n</network>\n";
        // Audit finding: a PREDICTABLE name in /tmp (world-writable)
        // allowed another local user to pre-create a symlink and divert the
        // write. `create_new` (O_EXCL) fails if the path already exists — without
        // following symlinks — and 0600 closes reading by others.
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let path = std::env::temp_dir().join(format!(
            "delonix-libvirt-default-{}.xml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path); // cleans up a leftover OF OURS from a previous run
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            if f.write_all(xml.as_bytes()).is_ok() {
                let _ = Command::new("virsh")
                    .args(["-c", uri, "net-define", &path.to_string_lossy()])
                    .output();
            }
            let _ = std::fs::remove_file(&path);
        }
    }
    // `.output()` (not `.status()`) so the "Network default started / marked as
    // autostarted" chatter does not leak into the clean `vm create` progress.
    let _ = Command::new("virsh")
        .args(["-c", uri, "net-start", "--", net])
        .output();
    let _ = Command::new("virsh")
        .args(["-c", uri, "net-autostart", "--", net])
        .output();
}

impl VmBackend for LibvirtBackend {
    fn id(&self) -> &'static str {
        "libvirt"
    }

    fn available(&self) -> bool {
        binary_in_path("virsh") && binary_in_path("qemu-system-x86_64")
    }

    fn boot(
        &self,
        vmdir: &Path,
        cfg: &VmConfig,
        overlay: &str,
        on: &dyn Fn(CreateStage),
    ) -> Result<Boot> {
        // Effective net mode: with no explicit `--net-mode`, prefer `nat`
        // whenever the SYSTEM connection is usable (libvirt group) — user-mode
        // (session) NEVER yields a reachable/visible IP, and silently landing
        // there was the real-world "vm ls shows IP <none>" report. Only when
        // the system connection is unusable do we keep user-mode (egress-only).
        let mut cfg = cfg.clone();
        if cfg.net_mode.is_none() && system_libvirt_usable() {
            cfg.net_mode = Some("nat".into());
        }
        let cfg = &cfg;
        if let Some(ip) = cfg.static_ip.as_deref() {
            if !matches!(cfg.net_mode.as_deref(), Some("nat") | Some("network")) {
                return Err(Error::Invalid(format!(
                    "VM '{}': --ip (static IP) requires the libvirt `nat` mode — this VM resolved to '{}' (on a host bridge, reserve the IP on your LAN's DHCP instead)",
                    cfg.name,
                    cfg.net_mode.as_deref().unwrap_or("user")
                )));
            }
            if ip.parse::<std::net::Ipv4Addr>().is_err() {
                return Err(Error::Invalid(format!(
                    "VM '{}': invalid static IP '{ip}'",
                    cfg.name
                )));
            }
        }
        let mac = mac_for(&cfg.name);
        let uri = libvirt_uri_for(cfg.net_mode.as_deref());
        // overlay as an absolute path (libvirtd may run in another cwd).
        let overlay_abs = std::fs::canonicalize(overlay)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| overlay.to_string());
        // NAT mode: ensures the libvirt network is DEFINED + active + autostart. Without
        // this, `vm create --net-mode nat` failed on installations where the
        // `default` network is not created (minimalist libvirt) or is stopped — the
        // path to a host-pingable IP + SSH.
        if matches!(cfg.net_mode.as_deref(), Some("nat") | Some("network")) {
            on(CreateStage::Network);
            let net = cfg.bridge.as_deref().unwrap_or("default");
            ensure_libvirt_network(uri, net);
            // Static IP: DHCP reservation MAC→IP on the libvirt network, BEFORE
            // the domain boots (the guest's DHCP request must already find it).
            if let Some(ip) = cfg.static_ip.as_deref() {
                libvirt_reserve_ip(uri, net, &mac, ip)?;
            }
        }
        let mut xml = libvirt_domain_xml(cfg, &overlay_abs, &mac);
        // On `qemu:///system` the QEMU process runs as the `libvirt-qemu` user,
        // which cannot read the overlay under a 0700 `$HOME`. A static DAC label
        // pins QEMU to the invoking uid/gid (the disk owner) and `relabel='no'`
        // keeps it from chown-ing the disk away from the user. This is what lets
        // a rootless-owned disk boot under system libvirt (needed for NAT/bridge,
        // the only modes with a host-reachable IP).
        if uri == "qemu:///system" && is_rootless() {
            let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
            let sec = format!(
                "  <seclabel type='static' model='dac' relabel='no'>\n    <label>+{uid}:+{gid}</label>\n  </seclabel>\n"
            );
            xml = xml.replace("</domain>\n", &format!("{sec}</domain>\n"));
        }
        let xml_path = vmdir.join(format!("{}.xml", cfg.name));
        std::fs::write(&xml_path, &xml)?;

        // Idempotent: if the domain already exists (auto-heal), it just (re)starts; otherwise
        // define + start. `virsh start` on an already-running domain is a benign no-op.
        let defined = capture("virsh", &["-c", uri, "domstate", "--", &cfg.name]).is_some();
        if !defined {
            on(CreateStage::Define);
            run_quiet("virsh", &["-c", uri, "define", &xml_path.to_string_lossy()])?;
        }
        on(CreateStage::Start);
        let out = Command::new("virsh")
            .args(["-c", uri, "start", "--", &cfg.name])
            .output()
            .map_err(|e| Error::Runtime {
                context: "libvirt",
                message: format!("virsh start: {e}"),
            })?;
        // 'start' fails if it is already running — we tolerate that (auto-heal).
        if !out.status.success() && !self.is_running_uri(uri, &cfg.name) {
            return Err(Error::Runtime {
                context: "vm",
                message: "failed to start the libvirt domain (KVM/permissions/image?)".into(),
            });
        }
        Ok(Boot {
            pid: None, // managed by libvirtd — liveness via virsh domstate
            ip: Self::ip_from_leases(uri, &cfg.name, &mac)
                .or_else(|| self.ip_uri(uri, &cfg.name))
                .or_else(|| cfg.static_ip.clone()),
            // The EFFECTIVE mode (not the requested one): lets `vm describe`
            // and the bin tell a reachable VM (nat/bridge) from an egress-only
            // one (user) — the basis of the "no reachable IP" warning.
            tap: cfg.net_mode.clone().unwrap_or_else(|| "user".into()),
            mac,
            api_socket: String::new(),
        })
    }

    fn is_running(&self, vm: &Vm) -> bool {
        self.is_running_uri(libvirt_uri_of(&vm.name), &vm.name)
    }

    fn ip(&self, vm: &Vm) -> Option<String> {
        let uri = libvirt_uri_of(&vm.name);
        Self::ip_from_leases(uri, &vm.name, &vm.mac).or_else(|| self.ip_uri(uri, &vm.name))
    }

    fn stop(&self, vmdir: &Path, vm: &Vm) -> Result<()> {
        libvirt_cleanup(&vm.name)?;
        let _ = std::fs::remove_file(vmdir.join(format!("{}.xml", vm.name)));
        Ok(())
    }
}

impl LibvirtBackend {
    fn is_running_uri(&self, uri: &str, name: &str) -> bool {
        capture("virsh", &["-c", uri, "domstate", "--", name])
            .map(|s| s == "running")
            .unwrap_or(false)
    }

    /// The libvirt network a domain's interface actually sources from (the
    /// `Source` column of `domiflist`) — NOT necessarily the delonix
    /// `--network` name given at `vm create`/`cluster kubeadm` time. Found
    /// live: a VM created with `--network lab-net` still lands on libvirt's
    /// own `default` NAT network; `lab-net` never becomes a real libvirt
    /// network object for the VM backend. `net-dhcp-leases` needs the REAL
    /// one, so this is queried rather than assumed.
    fn network_of(uri: &str, name: &str) -> Option<String> {
        let out = capture("virsh", &["-c", uri, "domiflist", "--", name])?;
        out.lines().find_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            (cols.len() >= 3 && cols[1] == "network").then(|| cols[2].to_string())
        })
    }

    /// IP via `virsh net-dhcp-leases`, scoped to this VM's OWN mac and
    /// resolved to the MOST RECENT lease.
    ///
    /// BUG FIXED HERE, found live (`cluster kubeadm`, repeatedly): a VM's
    /// guest can renegotiate DHCP several times during a single boot (each
    /// getting a DIFFERENT IP — observed live, e.g. one VM cycling through 3
    /// distinct addresses in under 20 minutes with a STABLE machine-id/DUID,
    /// so this isn't the machine-id-collision bug already fixed elsewhere —
    /// dnsmasq's lease list simply accumulates every past negotiation for
    /// that MAC instead of the guest releasing the old ones). `domifaddr`'s
    /// "lease" source dumps ALL of them, in neither chronological nor any
    /// other USEFUL order — taking its first (or last) line is a coin flip;
    /// confirmed live picking the WRONG, no-longer-valid entry from BOTH
    /// ends while the true current IP sat in the middle. `net-dhcp-leases`
    /// carries a real `Expiry Time` per entry (`YYYY-MM-DD HH:MM:SS`, so
    /// plain string comparison sorts it correctly) — filtering by MAC and
    /// taking the MAX expiry is the only actually-correct signal available,
    /// not a heuristic. Falls back to [`Self::ip_uri`] (`domifaddr`) when
    /// this doesn't resolve (non-libvirt-managed network, no lease yet, ...).
    fn ip_from_leases(uri: &str, name: &str, mac: &str) -> Option<String> {
        let network = Self::network_of(uri, name)?;
        let out = capture("virsh", &["-c", uri, "net-dhcp-leases", "--", &network])?;
        parse_leases_latest_ip(&out, mac)
    }

    /// IP via `virsh domifaddr` (may be empty in user-mode networking without an agent).
    /// Fallback of [`Self::ip_from_leases`] — see its doc for why that one is
    /// preferred whenever it resolves.
    fn ip_uri(&self, uri: &str, name: &str) -> Option<String> {
        let out = capture("virsh", &["-c", uri, "domifaddr", "--", name])?;
        // format: "Name  MAC  Protocol  Address"; take the 1st IPv4 (a.b.c.d/p).
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
// Lifecycle (generic, delegates to the backend)
// ===========================================================================

/// Ensures the microVM (idempotent): if it already exists and is alive, does nothing; if
/// it exists but died, re-boots reusing the overlay (auto-heal) with the SAME
/// backend; otherwise, chooses the backend (explicit/auto), creates the overlay and boots.
/// Validates a VM's NAME before using it in file PATHS, in the
/// cloud-init `hostname` and in the `virsh` argv. Audit finding: the name
/// (coming from the CLI OR from `metadata.name` of an UNTRUSTED manifest via
/// `stack apply -f`) flowed raw into `state_root/vms/<name>` (seed) and into the
/// overlay `<name>.qcow2` — a `metadata.name: "../../.ssh/authorized_keys"`
/// wrote/overwrote files OUTSIDE the state directory, as the
/// user. It also prevents a name starting with `-` (which `virsh` would read
/// as an option) and control characters (injection in the cloud-init YAML).
/// Strict whitelist: `[A-Za-z0-9._-]`, non-empty, does not start with `-`/`.`,
/// no `..`. Same spirit as the `valid_*` of the `cluster` audit.
pub fn valid_vm_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.starts_with('.')
        && name != ".."
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub fn create(base: &Path, cfg: &VmConfig) -> Result<Vm> {
    create_with(base, cfg, &|_| {})
}

/// [`create`] with a progress callback: `on` fires once per [`CreateStage`] as
/// the VM is built (disk → network → define → start), so the CLI can render
/// step-by-step progress. The engine emits only the enum; the text lives in the bin.
pub fn create_with(base: &Path, cfg: &VmConfig, on: &dyn Fn(CreateStage)) -> Result<Vm> {
    if !valid_vm_name(&cfg.name) {
        return Err(Error::Invalid(format!(
            "invalid VM name '{}' — use letters, digits, '.', '_' or '-' (no '/', '..', or leading '-')",
            cfg.name
        )));
    }
    let vmdir = vms_dir(base);
    std::fs::create_dir_all(&vmdir)?;
    let st = store(base)?;

    let restarting = st.load(&cfg.name).ok();
    // Anti-clobber (two VM subsystems in the SAME folder): if a
    // `<name>.json` that does NOT parse as a declarative Vm already exists, it is a direct-QEMU
    // record (`vm run`) — refuse instead of overwriting it and leaving that VM orphaned.
    if restarting.is_none() && vmdir.join(format!("{}.json", cfg.name)).exists() {
        return Err(Error::Invalid(format!(
            "a VM '{}' created by `vm run` (direct-QEMU) already exists. Remove it first \
             (`vm rm {}`) or use another name — the two subsystems share the vms/ folder.",
            cfg.name, cfg.name
        )));
    }
    // On restart, honor the backend the VM already used; otherwise choose now.
    let backend: Box<dyn VmBackend> = match &restarting {
        Some(ex) => {
            if backend_for(ex).is_running(ex) {
                return Ok(ex.clone()); // already running — idempotent
            }
            backend_for(ex)
        }
        None => {
            // Volumes ⇒ libvirt: only it materializes virtio-9p (Cloud Hypervisor
            // does not do 9p and would refuse in `boot`). The rule lives HERE (in the engine) and not
            // only in the bin, so any consumer of the API inherits it. Without volumes,
            // the normal auto-detection is kept.
            //
            // Cloud image (boot via FIRMWARE, without an explicit kernel) ⇒ prefer
            // libvirt. Cloud Hypervisor's `rust-hypervisor-fw` does not load the
            // initrd of Ubuntu cloud images (the initrd via EFI LoadFile2 is
            // not implemented in the minimalist firmware) → the kernel boots but
            // panics "Unable to mount root fs" (LABEL=cloudimg-rootfs
            // does not resolve without the initrd's udev). libvirt (full UEFI/SeaBIOS)
            // boots them. CH is left for DIRECT-KERNEL boot (k8s nodes with their own
            // kernel), where it is the best. Only if libvirt exists; otherwise CH with
            // a warning (better to try than to refuse).
            let want = match cfg.backend.as_deref() {
                Some(b) => Some(b),
                None if !cfg.volumes.is_empty() => Some("libvirt"),
                None if cfg.kernel.is_none() && LibvirtBackend.available() => Some("libvirt"),
                None if cfg.kernel.is_none() => {
                    eprintln!(
                        "warning: booting a cloud image on Cloud Hypervisor (libvirt not found) —                          if it panics on 'unable to mount root fs', install libvirt+qemu"
                    );
                    None
                }
                None => None,
            };
            select_backend(want)?
        }
    };

    // Admission: refuses to boot if there is no RAM on the host (anti-overcommit).
    // Only the VMs that will REALLY boot (not the idempotent already-running one above).
    vm_admission_check(cfg)?;

    let disk_path = std::fs::canonicalize(&cfg.disk)
        .map_err(|_| Error::Invalid(format!("image not found: {}", cfg.disk)))?;
    let overlay = vmdir.join(format!("{}.qcow2", cfg.name));
    if !overlay.exists() {
        on(CreateStage::Disk);
        let bf = disk_backing_format(&disk_path);
        run_quiet(
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

    let boot = match backend.boot(&vmdir, cfg, &overlay.to_string_lossy(), on) {
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
    vm.devices = cfg.devices.clone();
    vm.started_unix = Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    // restart_policy HONESTY: only libvirt materializes it (`<on_crash>restart`
    // in the XML). On Cloud Hypervisor there is no supervisor on the host — warn instead of
    // silently accepting a policy that is not enforced instantly.
    if restart_policy_unsupervised(backend.id(), vm.restart_policy.as_deref()) {
        tracing::warn!(
            vm = %cfg.name,
            backend = %backend.id(),
            restart_policy = %vm.restart_policy.as_deref().unwrap_or(""),
            "restart_policy '{}' on VM '{}' (backend {}) is NOT supervised on the host: the restart \
             happens on the next `delonix apply`/reconcile (auto-heal), not instantly on \
             crash. For immediate restart use `--backend libvirt`.",
            vm.restart_policy.as_deref().unwrap_or(""),
            cfg.name,
            backend.id()
        );
    }
    st.save(&cfg.name, &vm)?;
    Ok(vm)
}

/// `true` if the `restart_policy` requests automatic restart (`always`/`on-failure`)
/// but the backend does NOT supervise it on the host (only libvirt materializes it via XML).
/// On the others the restart depends on reconcile/apply — the caller should warn.
/// Pure function — testable.
pub fn restart_policy_unsupervised(backend_id: &str, policy: Option<&str>) -> bool {
    backend_id != "libvirt" && matches!(policy, Some("always") | Some("on-failure"))
}

/// Removes a VM: stops the VMM (via its backend), and deletes overlay/state.
///
/// If the backend cleanup fails (e.g. libvirt refuses the undefine), the local
/// record stays **INTACT** and the error propagates — the old version deleted the record
/// anyway and the VM was orphaned in libvirt, invisible to `vm ls`/`vm stop`. It also covers
/// the reverse: no local record but with an orphaned domain in libvirt (an
/// old interrupted `rm`), `remove` cleans up the domain anyway.
pub fn remove(base: &Path, name: &str) -> Result<()> {
    remove_inner(base, name, false)
}

/// Like [`remove`], but deletes the local state EVEN if the backend cleanup
/// fails (the `vm rm --force`) — the user takes on resolving the rest in libvirt.
pub fn remove_force(base: &Path, name: &str) -> Result<()> {
    remove_inner(base, name, true)
}

fn remove_inner(base: &Path, name: &str, force: bool) -> Result<()> {
    // A name that `create` would refuse cannot exist — and above all, it cannot
    // flow into the paths deleted below (the seed dir's `remove_dir_all`).
    if !valid_vm_name(name) {
        return Err(Error::VmNotFound(name.to_string()));
    }
    let vmdir = vms_dir(base);
    let st = store(base)?;
    let existed = match st.load(name) {
        Ok(vm) => {
            if let Err(e) = backend_for(&vm).stop(&vmdir, &vm) {
                if !force {
                    return Err(e); // record intact — the rm can be retried
                }
            }
            true
        }
        Err(_) => {
            // No record: there may be an orphaned libvirt domain with this name —
            // clean it up, and the ingress tap for safety.
            let orphan = libvirt_domain_uri(name).is_some();
            if let Err(e) = libvirt_cleanup(name) {
                if !force {
                    return Err(e);
                }
            }
            infra::vm_detach(name);
            orphan
        }
    };
    for ext in ["qcow2", "sock", "sock.lock", "serial", "log", "pid", "xml"] {
        let _ = std::fs::remove_file(vmdir.join(format!("{name}.{ext}")));
    }
    // The cloud-init seed directory (`vms/<name>/`, from `generate_seed_iso`)
    // also belongs to the VM — it was left behind and accumulated junk per name.
    let _ = std::fs::remove_dir_all(vmdir.join(name));
    if !existed {
        // Neither a local record nor a domain in libvirt — the `st.remove` below is
        // idempotent (absence is not an error) and would say Ok; an `rm` of something that
        // does not exist should say so, like docker.
        return Err(Error::VmNotFound(name.to_string()));
    }
    st.remove(name)
}

/// Stops the VM via ITS backend (CH/libvirt) but **preserves** the record and disk
/// (resumable). Unlike `remove`, it deletes nothing. Fixes the case where
/// the CLI's `vm stop` (direct-QEMU scheme) did not know how to stop a declarative
/// libvirt VM (pid null → the domain stayed alive, orphaned).
pub fn stop(base: &Path, name: &str) -> Result<()> {
    let vmdir = vms_dir(base);
    let st = store(base)?;
    let mut vm = match st.load(name) {
        Ok(vm) => vm,
        // No local record, but with a domain in libvirt (orphaned from an old
        // `rm`): power it off anyway — the intent is unambiguous and answering
        // "no such VM" for a VM that libvirt lists would be a lie.
        Err(Error::NotFound(_)) => {
            return match libvirt_domain_uri(name) {
                Some(uri) => libvirt_poweroff(uri, name),
                None => Err(Error::VmNotFound(name.to_string())),
            };
        }
        Err(e) => return Err(e),
    };
    backend_for(&vm).stop(&vmdir, &vm)?;
    vm.status = Status::Stopped;
    vm.pid = None;
    vm.started_unix = None;
    st.save(name, &vm)
}

/// Current state of a VM, with `status`/`ip` reconciled by its backend.
pub fn status(base: &Path, name: &str) -> Result<Vm> {
    let st = store(base)?;
    let mut vm = st.load(name).map_err(|e| match e {
        Error::NotFound(n) => Error::VmNotFound(n),
        e => e,
    })?;
    let backend = backend_for(&vm);
    let old_ip = vm.ip.clone();
    let was_running = vm.status == Status::Running;
    if backend.is_running(&vm) {
        vm.status = Status::Running;
        vm.ip = backend.ip(&vm).or(vm.ip);
    } else {
        // A powered-off VM = Stopped (the guest may have done a clean shutdown;
        // unlike containers, the VM is autonomous — a crash is not assumed).
        vm.status = Status::Stopped;
        vm.pid = None;
        // The guest powered itself off outside our own `stop()` (e.g. `shutdown
        // now` from inside) — reconcile `started_unix` the same way `stop()`
        // does, so UPTIME doesn't keep counting a boot that already ended.
        vm.started_unix = None;
    }
    // Persist a freshly-learnt IP (a nat VM only gets its DHCP lease well after
    // `create` saved the record): the record is what the holder's internal DNS
    // reads to resolve `<vm-name>` for containers — a stale null IP there means
    // the name never resolves. Best-effort: status() stays read-mostly.
    if vm.ip != old_ip || was_running != (vm.status == Status::Running) {
        let _ = st.save(name, &vm);
    }
    Ok(vm)
}

/// Lists all VMs, with reconciled state.
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
    fn parse_leases_latest_ip_escolhe_o_expiry_mais_recente() {
        // Real `virsh net-dhcp-leases default` output captured live while
        // diagnosing this exact bug — 3 leases for the SAME MAC (a VM whose
        // guest renegotiated DHCP repeatedly during one boot), not in
        // chronological order in the listing. Only .179 (the last NEGOTIATED,
        // NOT the last LISTED) was actually reachable at the time.
        let out = "\
 Expiry Time           MAC address         Protocol   IP address           Hostname   Client ID or DUID
------------------------------------------------------------------------------------------------------------------------------------------------
 2026-07-23 19:45:54   52:54:00:e2:55:fb   ipv4       192.168.122.177/24   -          ff:56:50:4d:98:00:02:00:00:ab:11:f8:13:8b:f9:b6:a0:58:03
 2026-07-23 20:04:08   52:54:00:e2:55:fb   ipv4       192.168.122.179/24   lab-cp1    ff:56:50:4d:98:00:02:00:00:ab:11:1a:71:81:66:74:ab:24:eb
 2026-07-23 19:55:23   52:54:00:e2:55:fb   ipv4       192.168.122.178/24   -          ff:56:50:4d:98:00:02:00:00:ab:11:7c:bb:67:24:f1:93:4b:b8
 2026-07-23 19:46:10   52:54:00:b7:c8:ef   ipv4       192.168.122.17/24    -          ff:56:50:4d:98:00:02:00:00:ab:11:a1:60:5a:13:80:91:cf:b8";
        assert_eq!(
            parse_leases_latest_ip(out, "52:54:00:e2:55:fb"),
            Some("192.168.122.179".to_string())
        );
        // Case-insensitive MAC match (virsh output is lowercase, callers may not be).
        assert_eq!(
            parse_leases_latest_ip(out, "52:54:00:E2:55:FB"),
            Some("192.168.122.179".to_string())
        );
        // A different MAC only ever had one lease.
        assert_eq!(
            parse_leases_latest_ip(out, "52:54:00:b7:c8:ef"),
            Some("192.168.122.17".to_string())
        );
        // No lease at all for this MAC.
        assert_eq!(parse_leases_latest_ip(out, "aa:bb:cc:dd:ee:ff"), None);
    }

    #[test]
    fn parse_leases_latest_ip_tolera_saida_vazia_ou_so_cabecalho() {
        assert_eq!(parse_leases_latest_ip("", "52:54:00:e2:55:fb"), None);
        assert_eq!(
            parse_leases_latest_ip(
                " Expiry Time  MAC address  Protocol  IP address  Hostname  Client ID or DUID\n---",
                "52:54:00:e2:55:fb"
            ),
            None
        );
    }

    #[test]
    fn mem_mib_parses_units() {
        assert_eq!(mem_mib("2G"), 2048);
        assert_eq!(mem_mib("1024M"), 1024);
        assert_eq!(mem_mib("512"), 512);
        assert_eq!(mem_mib("2Gi"), 2048); // k8s suffix tolerated (before it gave 1024)
        assert_eq!(mem_mib("512Mi"), 512);
        assert_eq!(mem_mib("lixo"), 1024); // robust fallback
    }

    #[test]
    fn valid_vm_name_recusa_exploits() {
        // Path traversal (seed/overlay outside the state dir), via CLI or manifest.
        assert!(!super::valid_vm_name("../../.ssh/authorized_keys"));
        assert!(!super::valid_vm_name("a/b"));
        assert!(!super::valid_vm_name(".."));
        assert!(!super::valid_vm_name("a..b"));
        // virsh argv: a name starting with '-' becomes an option.
        assert!(!super::valid_vm_name("-c"));
        // Injection in the cloud-init YAML (hostname) / control.
        assert!(!super::valid_vm_name("x\nruncmd:\n  - evil"));
        assert!(!super::valid_vm_name(""));
        // Legitimate names pass through intact (no regression).
        assert!(super::valid_vm_name("dev"));
        assert!(super::valid_vm_name("kadm-cp1"));
        assert!(super::valid_vm_name("my.vm_02"));
    }

    #[test]
    fn quiet_captura_o_stderr_sem_o_prefixo_error() {
        // `virsh` prefixes each line with `error: ` — the composed message must
        // not repeat that, nor leak the raw stderr to the terminal.
        let err = super::quiet("sh", &["-c", "echo 'error: boom' >&2; exit 1"]).unwrap_err();
        assert_eq!(err, "boom");
        let ok = super::quiet("sh", &["-c", "echo out"]).unwrap();
        assert_eq!(ok, "out");
    }

    #[test]
    fn stop_e_remove_de_vm_inexistente_dizem_no_such_vm() {
        // Regression from the bug report: `vm stop dev` without a record answered
        // "no such container: dev" — wrong noun for a VM — and
        // `vm rm` of a non-existent name returned silent success.
        let base = std::env::temp_dir().join(format!("delonix-vm-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        for res in [super::stop(&base, "nope"), super::remove(&base, "nope")] {
            match res {
                Err(Error::VmNotFound(n)) => assert_eq!(n, "nope"),
                other => panic!("expected VmNotFound, got {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    fn test_vm_cfg(mem: &str) -> VmConfig {
        VmConfig {
            name: "t".into(),
            disk: String::new(),
            vcpus: 1,
            memory: mem.into(),
            network: String::new(),
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
            volumes: vec![],
            vnc: false,
            static_ip: None,
            ..Default::default()
        }
    }

    #[test]
    fn libvirt_xml_partilha_volumes_por_9p() {
        let mut cfg = test_vm_cfg("1G");
        cfg.volumes = vec![
            VmVolume {
                tag: "dados".into(),
                source: "/srv/dados".into(),
                mount_path: "/mnt/dados".into(),
                read_only: false,
            },
            VmVolume {
                tag: "ro".into(),
                source: "/srv/ro".into(),
                mount_path: "/mnt/ro".into(),
                read_only: true,
            },
        ];
        let xml = libvirt_domain_xml(&cfg, "/tmp/overlay.qcow2", "52:54:00:00:00:01");
        assert!(
            xml.contains("<filesystem type='mount' accessmode='passthrough'>"),
            "{xml}"
        );
        assert!(xml.contains("<source dir='/srv/dados'/>"), "{xml}");
        assert!(xml.contains("<target dir='dados'/>"), "{xml}");
        // The read-only one (2nd volume) carries `<readonly/>` in its block.
        let ro_idx = xml.find("<target dir='ro'/>").unwrap();
        assert!(
            xml[ro_idx..].starts_with("<target dir='ro'/>\n      <readonly/>"),
            "{xml}"
        );
        // Without volumes → no <filesystem>.
        assert!(
            !libvirt_domain_xml(&test_vm_cfg("1G"), "/tmp/o.qcow2", "52:54:00:00:00:02")
                .contains("<filesystem")
        );
    }

    #[test]
    fn vm_admission_recusa_quando_nao_cabe() {
        std::env::set_var("DELONIX_VM_RESERVE_MIB", "0");
        // Only validates if the host has a readable MemAvailable (otherwise it is a best-effort no-op).
        if host_mem_available_mib().is_some() {
            assert!(
                vm_admission_check(&test_vm_cfg("1000000G")).is_err(), // 1 PB — never fits
                "giant VM must be refused"
            );
        }
        assert!(
            vm_admission_check(&test_vm_cfg("1M")).is_ok(), // tiny — always fits
            "tiny VM must be admitted"
        );
        std::env::remove_var("DELONIX_VM_RESERVE_MIB");
    }

    #[test]
    fn restart_policy_unsupervised_deteta() {
        // CH/QEMU do not supervise always/on-failure → warns.
        assert!(restart_policy_unsupervised(
            "cloud-hypervisor",
            Some("always")
        ));
        assert!(restart_policy_unsupervised(
            "cloud-hypervisor",
            Some("on-failure")
        ));
        // libvirt materializes it in the XML → does not warn.
        assert!(!restart_policy_unsupervised("libvirt", Some("always")));
        // no policy or `no` → nothing to warn about.
        assert!(!restart_policy_unsupervised("cloud-hypervisor", Some("no")));
        assert!(!restart_policy_unsupervised("cloud-hypervisor", None));
    }

    #[test]
    fn create_recusa_clobber_de_vm_run() {
        let tmp = std::env::temp_dir().join(format!("dlx-vmclob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let vmdir = vms_dir(&tmp);
        std::fs::create_dir_all(&vmdir).unwrap();
        // direct-QEMU record (raw scheme, WITHOUT `backend`) — as `vm run` writes it.
        std::fs::write(
            vmdir.join("myvm.json"),
            br#"{"name":"myvm","pid":1234,"memory":1024,"cpus":1}"#,
        )
        .unwrap();
        let mut cfg = hpc_cfg();
        cfg.name = "myvm".into();
        let err = create(&tmp, &cfg).unwrap_err();
        assert!(
            format!("{err}").contains("vm run"),
            "create should refuse the clobber of a direct-QEMU record: {err}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_qemu_format_extrai_formato_real() {
        // Human output of `qemu-img info` for a `.img` that is qcow2 inside
        // (the core of the backing-format bug).
        let info = "image: jammy.img\nfile format: qcow2\nvirtual size: 2.2 GiB (2361393152 bytes)\ndisk size: 614 MiB\n";
        assert_eq!(parse_qemu_format(info).as_deref(), Some("qcow2"));
        let raw = "image: disco.img\nfile format: raw\nvirtual size: 8 MiB\n";
        assert_eq!(parse_qemu_format(raw).as_deref(), Some("raw"));
        assert_eq!(parse_qemu_format("image: x\nvirtual size: 8 MiB\n"), None);
    }

    /// Minimal VmConfig to exercise the HPC args helpers (S4).
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
            volumes: vec![],
            vnc: false,
            static_ip: None,
            ..Default::default()
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
        // each of the 4 vCPUs pinned to the host's 8-15 list.
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
        // nat → libvirt network (default "default") with IP via domifaddr.
        c.net_mode = Some("nat".into());
        let nat = libvirt_interface_xml(&c, "52:54:00:00:00:01");
        assert!(nat.contains("type='network'") && nat.contains("source network='default'"));
        c.bridge = Some("dlxnat".into());
        assert!(libvirt_interface_xml(&c, "52:54:00:00:00:01").contains("source network='dlxnat'"));
        // bridge → host bridge.
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

    #[test]
    fn libvirt_xml_advanced_knobs() {
        let mut c = hpc_cfg();
        c.machine = Some("pc-q35-6.2".into());
        c.cpu_model = Some("Skylake-Server".into());
        c.cpu_topology = Some(CpuTopology {
            sockets: 2,
            cores: 4,
            threads: 2,
        });
        c.tpm = true;
        c.video = Some("qxl".into());
        c.boot_order = vec!["cdrom".into(), "hd".into()];
        c.extra_disks = vec![ExtraDisk {
            source: "/data/extra.qcow2".into(),
            device: "disk".into(),
            bus: "virtio".into(),
            format: "qcow2".into(),
            read_only: true,
            target: None,
        }];
        c.extra_nics = vec![ExtraNic {
            kind: "bridge".into(),
            source: Some("br0".into()),
            model: "e1000".into(),
            mac: None,
        }];
        c.libvirt_xml_overlay = vec!["    <watchdog model='i6300esb' action='reset'/>".into()];
        let xml = libvirt_domain_xml(&c, "/o.qcow2", "52:54:00:aa:bb:cc");
        assert!(xml.contains("machine='pc-q35-6.2'"));
        assert!(xml.contains("<cpu mode='custom'"));
        assert!(xml.contains("<model fallback='allow'>Skylake-Server</model>"));
        assert!(xml.contains("sockets='2' cores='4' threads='2'"));
        assert!(xml.contains("<boot dev='cdrom'/>"));
        assert!(xml.contains("<boot dev='hd'/>"));
        assert!(xml.contains("<source file='/data/extra.qcow2'/>"));
        // main disk keeps vda; the extra virtio disk auto-assigns vdb.
        assert!(xml.contains("<target dev='vdb' bus='virtio'/>"));
        assert!(xml.contains("<interface type='bridge'>"));
        assert!(xml.contains("<source bridge='br0'/>"));
        assert!(xml.contains("<model type='e1000'/>"));
        assert!(xml.contains("<tpm model='tpm-crb'>"));
        assert!(xml.contains("<video><model type='qxl' heads='1'/></video>"));
        assert!(xml.contains("<watchdog model='i6300esb' action='reset'/>"));
    }

    #[test]
    fn libvirt_xml_full_override_is_verbatim() {
        let mut c = hpc_cfg();
        c.libvirt_xml = Some("<domain type='kvm'><name>custom</name></domain>\n".into());
        let xml = libvirt_domain_xml(&c, "/o.qcow2", "52:54:00:aa:bb:cc");
        assert_eq!(xml, "<domain type='kvm'><name>custom</name></domain>\n");
    }
}
