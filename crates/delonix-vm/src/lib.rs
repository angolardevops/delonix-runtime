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

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use delonix_net::infra;
use delonix_runtime_core::{Error, JsonStore, Result, Status, Vm, VmBootSpec};

/// The VM shapes that [`Vm`] persists. They are DEFINED in
/// `delonix-runtime-core` — the record lives there and the dependency cannot
/// run the other way — and re-exported here so `delonix_vm::CpuTopology` and
/// friends keep resolving for every existing caller.
pub use delonix_runtime_core::{CpuTopology, ExtraDisk, ExtraNic, VmVolume};

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
    /// Logical isolation namespace (`None`/`"default"` = the open SDN), the same
    /// notion `container run --namespace` uses. Only meaningful for a VM that
    /// actually lives on the holder's SDN — see [`vm_namespace_supported`].
    pub namespace: Option<String>,
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

// `VmVolume` — what connects `kind: Volume`/`kind: Storage` to a VM without the
// user writing cloud-init or XML: the bin resolves the name → `source` (the
// volume's `_data`, or a network Storage's mountpoint) and the engine generates
// both the domain's `<filesystem>` and the guest-side `mount`. Defined in
// `delonix-runtime-core` with the other persisted shapes; re-exported above.

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
/// `"2G"`/`"512M"`/`"2Gi"`/`"2048"` → MiB.
///
/// **Public because every backend has to read the SAME field the same way.**
/// It was private, so `delonix-proxmox` grew its own copy — and the copy did
/// not know the k8s `Gi`/`Mi` suffix this one tolerates, so `memory: 2Gi` meant
/// 2 GiB on libvirt and Cloud Hypervisor and 1 GiB on Proxmox, silently. Same
/// discipline as `fw_rule_tail` on the network side: one definition, shared by
/// everyone who reads the format.
pub fn mem_mib(s: &str) -> u64 {
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
/// The VM's requested isolation namespace, normalized (`None`/empty = `default`).
fn vm_namespace_of(cfg: &VmConfig) -> String {
    match cfg.namespace.as_deref() {
        None | Some("") => "default".to_string(),
        Some(ns) => ns.to_string(),
    }
}

/// Whether `backend` puts its VMs on the holder's SDN, where namespace isolation
/// is enforceable at all.
///
/// **Only Cloud Hypervisor does.** A libvirt VM lives on `virbr0`, in the HOST's
/// network namespace — a different L2 entirely, governed by libvirt's own
/// filtering, which this engine does not program. Accepting `--namespace` there
/// and quietly doing nothing would be the exact anti-pattern this codebase has
/// already had to correct three times over (`--security-opt seccomp=`,
/// `-v …:z`, `--network-alias`): an option accepted, ignored, and believed.
pub fn vm_namespace_supported(backend_id: &str) -> bool {
    backend_id == "cloud-hypervisor"
}

/// The primary NIC's MAC, DERIVED from the VM name — the same value both
/// backends stamp on the interface they create, and therefore the one thing
/// about the guest's network that is knowable before the guest exists.
///
/// `pub` because the seed generator needs it: a NoCloud `network-config` that
/// matches the NIC by name has to guess a name (`eth0`? `ens3`? `enp1s0`?),
/// and guessing wrong is silent. Matching by MAC is exact. One formula, one
/// caller-visible function — a second copy of this arithmetic would diverge the
/// day the vendor prefix changed, and the symptom would be a guest configuring
/// a NIC that is not there.
pub fn mac_for(name: &str) -> String {
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
    // `qemu-img info` is PARSED (`parse_qemu_format`) — same locale exposure
    // as the `virsh` state strings; see `stable_cmd`.
    if let Ok(out) = stable_cmd("qemu-img").arg("info").arg(disk).output() {
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
    let out = stable_cmd(prog)
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

/// Builds a `Command` whose output this crate PARSES, pinned to the `C` locale.
///
/// BUG FIXED HERE (latent, and it bites precisely in this product's home
/// market). `virsh` is a gettext program — confirmed on this host: its binary
/// exports `bindtextdomain`/`dcgettext` and carries `"shut off"` as a
/// translatable msgid. Meanwhile this crate decides a domain's liveness by
/// comparing that output against ENGLISH literals:
///
/// ```text
/// libvirt_poweroff:      state == "shut off"
/// LibvirtBackend::is_running:  s == "running"
/// ```
///
/// On a host with libvirt's l10n catalogues installed and `LANG=pt_PT` — an
/// ordinary Angolan/Portuguese production host — `virsh domstate` answers in
/// Portuguese and BOTH comparisons silently go false. A running VM reports as
/// stopped (`vm ls` lies, `wait_for_boot` never converges) and
/// `libvirt_poweroff` fires `destroy` at an already-off domain, which is exactly
/// the raw-stderr failure v0.11 fixed from the other end.
///
/// Pinning the locale is the right layer: it makes the tool's output a stable
/// MACHINE interface, rather than teaching every call site to recognise N
/// translations. `LANG` is set too — `LC_ALL` alone is enough for glibc, but
/// belt-and-braces costs nothing and covers tools that read `LANG` directly.
///
/// This is also why it lives on the shared helpers rather than on the `virsh`
/// call sites: `qemu-img`, `losetup` and friends are parsed the same way and
/// have the same exposure.
fn stable_cmd(prog: &str) -> Command {
    let mut c = Command::new(prog);
    c.env("LC_ALL", "C").env("LANG", "C");
    c
}

/// Runs a command and captures stdout (trimmed), or `None` on failure.
fn capture(prog: &str, args: &[&str]) -> Option<String> {
    let out = stable_cmd(prog).args(args).output().ok()?;
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

    /// Is [`VmBackend::ip`] a PREDICTION rather than an OBSERVATION?
    ///
    /// Default `false`: libvirt reads a real DHCP lease, so an address there is
    /// evidence that the guest booted far enough to ask for one. Cloud
    /// Hypervisor overrides it — its address is computed from the MAC before
    /// the guest runs at all, so it is evidence of nothing.
    ///
    /// Whoever waits for a boot needs this to know when "it has an IP" is an
    /// answer and when it is only an arithmetic identity. It lives on the
    /// backend rather than in a `backend.contains("cloud-hypervisor")` at the
    /// call site for the reason ADR-0008 gives: the knowledge belongs to the
    /// backend that does the predicting.
    fn ip_is_predicted(&self) -> bool {
        false
    }
    /// Stops the VM and frees the network resources. Returns `Err` when the backend
    /// REFUSED the cleanup (e.g. libvirt) — the caller decides whether to abort (so as not to
    /// delete the local record of a VM that is still defined in the hypervisor) or
    /// to ignore it (`vm rm --force`).
    fn stop(&self, vmdir: &Path, vm: &Vm) -> Result<()>;

    /// Releases everything the VM owns, because its record is going away
    /// (`vm rm`). Default: [`Self::stop`] — which is exactly right for the two
    /// local backends and is why nothing existing changes.
    ///
    /// **The two are the same operation locally and NOT the same remotely**,
    /// and conflating them destroyed data. Locally the disk is the engine's: a
    /// libvirt `undefine` leaves `<root>/vms/<name>.qcow2` untouched, so `stop`
    /// can free the hypervisor's side and `rm` deletes the file afterwards. On
    /// a remote node the disk belongs to the node, and the only call that frees
    /// the VM also frees its disk — so a backend that implemented `stop` as
    /// "stop and destroy" made `delonix vm stop` erase the guest, while the
    /// CLI's own next-steps block promises `stop it (keeps the disk)`.
    ///
    /// A backend that owns nothing beyond what `stop` releases should leave
    /// this alone.
    fn destroy(&self, vmdir: &Path, vm: &Vm) -> Result<()> {
        self.stop(vmdir, vm)
    }

    /// Brings an already-created VM back up, instead of creating one.
    ///
    /// `Ok(None)` — the default — means "I have no way to resume; create it the
    /// usual way", which is the truth for both local backends: their `boot` is
    /// idempotent because the per-VM overlay is on this filesystem and gets
    /// reused.
    ///
    /// A remote backend has no such luck. Its `boot` asks the node for the next
    /// free id, so a `vm start` on a stopped VM would build a SECOND one and
    /// leave the first orphaned on the node with nothing pointing at it —
    /// silently, since the record is then rewritten to the new handle. Here it
    /// can start the VM its record already names.
    ///
    /// Called only when a record exists and the VM is not running.
    fn resume(&self, _vmdir: &Path, _vm: &Vm) -> Result<Option<Boot>> {
        Ok(None)
    }

    /// Takes a named snapshot of the VM. On libvirt this is a **system checkpoint**
    /// (`virsh snapshot-create-as`): for a running domain it captures memory + disk
    /// state; `restore` reverts to it. Default: unsupported — a backend that does not
    /// override this fails closed with a clear message (never a silent no-op).
    fn snapshot(&self, _vmdir: &Path, _vm: &Vm, _name: &str) -> Result<()> {
        Err(unsupported_snapshot(self.id(), "snapshot"))
    }
    /// Reverts the VM to a named snapshot (libvirt: `virsh snapshot-revert`).
    /// Default: unsupported (fail closed).
    fn restore(&self, _vmdir: &Path, _vm: &Vm, _name: &str) -> Result<()> {
        Err(unsupported_snapshot(self.id(), "restore"))
    }
    /// Lists the VM's snapshot names. Default: unsupported (fail closed).
    ///
    /// Takes `vmdir` because a stopped VM's snapshots may live only on OUR
    /// side: libvirt's metadata does not survive the undefine that [`stop`]
    /// does, so the list of a stopped VM is read from what
    /// [`VmBackend::preserve_snapshots`] wrote there.
    fn snapshots(&self, _vmdir: &Path, _vm: &Vm) -> Result<Vec<String>> {
        Err(unsupported_snapshot(self.id(), "snapshots"))
    }
    /// Deletes a named snapshot — the state in the disk AND whatever metadata
    /// points at it. Default: unsupported (fail closed).
    fn delete_snapshot(&self, _vmdir: &Path, _vm: &Vm, _name: &str) -> Result<()> {
        Err(unsupported_snapshot(self.id(), "snapshot rm"))
    }

    /// Saves whatever snapshot state STOPPING this VM would otherwise destroy,
    /// and returns the names saved. Called by [`stop`] BEFORE
    /// [`VmBackend::stop`], so a failure here aborts the stop with nothing lost
    /// yet. Default: nothing to preserve (a backend whose snapshots survive a
    /// stop, or which has none, keeps its behaviour byte for byte).
    ///
    /// This exists because of what libvirt's `undefine --snapshots-metadata`
    /// does: the snapshot DATA stays in the qcow2 (measured), only libvirt's
    /// bookkeeping is deleted — so a `vm stop`/`vm start` left `vm snapshots`
    /// empty with rc=0 and `vm restore` answering "Domain snapshot not found",
    /// for snapshots that were still there on the disk the whole time.
    fn preserve_snapshots(&self, _vmdir: &Path, _vm: &Vm) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// `true` when the backend owns its own disks and [`create`] must NOT
    /// prepare one.
    ///
    /// The default is `false`, which is what both local backends are and what
    /// every existing implementation keeps without changing a line: `create`
    /// resolves `cfg.disk` on THIS filesystem and builds a thin qcow2 overlay
    /// for the VM, and `boot` receives that overlay's path.
    ///
    /// A backend whose hypervisor is on another machine cannot use any of it —
    /// the base image lives on that node, and a local overlay backs nothing
    /// there. Worse, `create` would fail on the local `canonicalize` before the
    /// backend was ever asked. With `true`, `boot` receives `cfg.disk`
    /// unchanged and decides for itself what it names on the far side.
    ///
    /// This exists because the alternative was uploading a local overlay on
    /// every create — a second disk model, and slow — purely to satisfy a
    /// signature (ADR-0008).
    fn manages_own_storage(&self) -> bool {
        false
    }

    /// `true` when auto-detection may pick this backend with nobody asking for
    /// it by name.
    ///
    /// Local backends answer `available()` with a `which`, which is cheap and
    /// truthful. A REMOTE backend cannot: the only honest answer needs a
    /// network round trip to a node that may not even be configured, and
    /// auto-detection is not a place to make HTTP requests. So a remote backend
    /// returns `false` here and is chosen explicitly (`--backend`,
    /// `DELONIX_VM_BACKEND`, `vm default-backend`) or not at all.
    fn auto_selectable(&self) -> bool {
        true
    }
}

/// Fail-closed error for a backend that does not implement snapshot/restore
/// (today: cloud-hypervisor — its restore relaunches a fresh vmm, a different
/// lifecycle than libvirt's in-place revert, and needs `ch-remote`; deferred).
fn unsupported_snapshot(backend: &str, op: &str) -> Error {
    Error::Invalid(format!(
        "{op} is not supported on the '{backend}' backend yet — use the libvirt backend"
    ))
}

/// How a registered backend is built when somebody selects it.
///
/// A closure and not a `fn` pointer because a REMOTE backend needs
/// configuration — an endpoint, a node name, a credential — and
/// `fn() -> Box<dyn VmBackend>` has nowhere to receive it. That gap is
/// precisely what kept ADR-0008's decision 2 from landing: a crate that
/// depends on `delonix-vm` (as any backend must, for the trait) could not put
/// itself into a `static` table here.
///
/// `Send + Sync` because the table is process-wide. It constrains the CLOSURE,
/// not the trait: a backend implementation is untouched by this.
///
/// It returns `Result` so a backend whose construction can fail (a remote one
/// authenticating) reports why, instead of a factory that must panic or lie.
pub type BackendFactory = Box<dyn Fn() -> Result<Box<dyn VmBackend>> + Send + Sync>;

/// One backend this build knows about: its canonical id (the value persisted in
/// [`Vm::backend`]), the aliases accepted on input, and how to build one.
pub struct BackendRegistration {
    /// Canonical id. Must equal what the built backend's [`VmBackend::id`]
    /// returns — it is what gets persisted in the record and looked up later.
    pub id: &'static str,
    /// Extra spellings accepted from a user; never repeats `id`.
    pub aliases: &'static [&'static str],
    /// Whether auto-detection may pick this backend with nobody naming it.
    ///
    /// **A copy of [`VmBackend::auto_selectable`], and deliberately so**:
    /// auto-detection has to answer this WITHOUT building the backend.
    /// Construction is where a remote backend authenticates, so asking the
    /// built object would make the walk do the network round trip the flag
    /// exists to prevent. [`register_backend`] checks the two agree.
    pub auto_selectable: bool,
    pub new: BackendFactory,
}

impl std::fmt::Debug for BackendRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendRegistration")
            .field("id", &self.id)
            .field("aliases", &self.aliases)
            .field("auto_selectable", &self.auto_selectable)
            .finish_non_exhaustive()
    }
}

fn builtin_backends() -> Vec<BackendRegistration> {
    vec![
        BackendRegistration {
            id: "cloud-hypervisor",
            aliases: &["ch", "cloudhypervisor"],
            auto_selectable: true,
            new: Box::new(|| Ok(Box::new(CloudHypervisorBackend))),
        },
        BackendRegistration {
            id: "libvirt",
            aliases: &["kvm", "qemu"],
            auto_selectable: true,
            new: Box::new(|| Ok(Box::new(LibvirtBackend))),
        },
    ]
}

/// Every registered backend. **Order matters**: it is the preference order of
/// the auto-detection in [`select_backend`] (first one installed wins), and the
/// two local ones are seeded first so registering a third never changes what an
/// existing host picks.
///
/// This is a map populated at startup, **not a plugin system** (ADR-0008):
/// nothing loads a `.so`, and the only way in is [`register_backend`], called
/// by a process that already linked the backend's crate. On the day somebody
/// proposes loading code at runtime, that is a new ADR.
static BACKENDS: std::sync::OnceLock<std::sync::RwLock<Vec<BackendRegistration>>> =
    std::sync::OnceLock::new();

fn backends() -> &'static std::sync::RwLock<Vec<BackendRegistration>> {
    BACKENDS.get_or_init(|| std::sync::RwLock::new(builtin_backends()))
}

/// Runs `f` over the registered backends. A helper because every reader needs
/// the same lock, and a poisoned lock here must not take the process down: a
/// panic in an unrelated thread is not a reason for `vm ls` to abort.
fn with_backends<T>(f: impl FnOnce(&[BackendRegistration]) -> T) -> T {
    let guard = backends().read().unwrap_or_else(|e| e.into_inner());
    f(&guard)
}

/// Adds a backend to the registry. Idempotent by id: registering the same id
/// twice REPLACES the entry, so a process that configures a target twice ends
/// up with the last one rather than two that shadow each other.
///
/// The caller is a process that linked the backend's crate and knows its
/// configuration — for a remote backend, that is where the endpoint and the
/// credential come from. **Nothing here does I/O**: the factory is not called,
/// so registering a node that is unreachable costs nothing until someone
/// actually selects it.
///
/// Refused, rather than accepted and left to surprise someone later:
///
/// * an id or alias that collides with a DIFFERENT backend already registered —
///   the loser would become unreachable by name, silently;
/// * a `auto_selectable: true` on a backend that is not one of this crate's
///   own. Auto-detection walks the table asking `available()`, and a remote
///   backend cannot answer that without a network round trip (ADR-0008). A
///   third-party backend is selected by name or not at all.
pub fn register_backend(reg: BackendRegistration) -> Result<()> {
    if reg.id.trim().is_empty() {
        return Err(Error::Invalid("a backend registration needs an id".into()));
    }
    let builtin_ids: Vec<&str> = builtin_backends().iter().map(|b| b.id).collect();
    if reg.auto_selectable && !builtin_ids.contains(&reg.id) {
        return Err(Error::Invalid(format!(
            "backend '{}' cannot be auto-selectable: auto-detection asks every candidate \
             `available()`, and a backend registered from outside this crate may only be able to \
             answer that over the network. Register it with `auto_selectable: false` and select it \
             by name (`--backend {}`)",
            reg.id, reg.id
        )));
    }
    let mut guard = backends().write().unwrap_or_else(|e| e.into_inner());
    // A name that already belongs to somebody else. Checked against every
    // OTHER entry, so re-registering the same id (a reconfigured target) is
    // fine while stealing another's alias is not.
    for name in std::iter::once(&reg.id).chain(reg.aliases.iter()) {
        let want = name.trim().to_lowercase();
        if let Some(clash) = guard
            .iter()
            .find(|b| b.id != reg.id && (b.id == want || b.aliases.contains(&want.as_str())))
        {
            return Err(Error::Invalid(format!(
                "backend '{}' cannot claim the name '{}': it already belongs to '{}'",
                reg.id, name, clash.id
            )));
        }
    }
    guard.retain(|b| b.id != reg.id);
    guard.push(reg);
    Ok(())
}

/// `true` when `name` resolves to a registered backend (canonical id or alias),
/// case- and whitespace-insensitive.
///
/// `#[cfg(test)]` on purpose: no production path needs "is it registered?"
/// without also wanting the backend, and this repo does not keep a public
/// helper waiting for its first caller (`publish_port_allow`, `Net`).
#[cfg(test)]
fn backend_is_registered(name: &str) -> bool {
    let want = name.trim().to_lowercase();
    with_backends(|bs| {
        bs.iter()
            .any(|b| b.id == want || b.aliases.contains(&want.as_str()))
    })
}

/// Builds the backend `name` resolves to, or `None` if nothing does.
fn make_backend(name: &str) -> Option<Result<Box<dyn VmBackend>>> {
    let want = name.trim().to_lowercase();
    with_backends(|bs| {
        bs.iter()
            .find(|b| b.id == want || b.aliases.contains(&want.as_str()))
            .map(|b| (b.new)())
    })
}

/// The registered ids, for an error that names what IS accepted. Derived from
/// the table so it cannot drift from it.
fn registered_backend_ids() -> String {
    with_backends(|bs| {
        bs.iter()
            .map(|b| format!("'{}'", b.id))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

/// Backend names this engine KNOWS but does not register itself, and why.
///
/// The distinction is not pedantry. `delonix-proxmox` exists in this workspace
/// and implements the trait — answering `--backend proxmox` with «unknown
/// backend» told the operator the opposite of the truth: that the thing does
/// not exist, rather than that this process did not configure it.
///
/// Because it CAN be configured now, the text says what to do rather than what
/// is missing. It is still not a registration: reaching this table means
/// nothing registered the name, and the only way in is [`register_backend`].
const KNOWN_UNREGISTERED: &[(&str, &str)] = &[(
    "proxmox",
    "the Proxmox backend (crate `delonix-proxmox`) needs a node to talk to, so it is only \
     available once one is configured. Set `DELONIX_PROXMOX_URL`, `DELONIX_PROXMOX_NODE` and a \
     credential (`DELONIX_PROXMOX_TOKEN`, or a `kind: Secret` named by \
     `DELONIX_PROXMOX_SECRET`) — see docs/adr/0008-proxmox-vm-backend.md",
)];

fn unknown_backend(name: &str) -> Error {
    let want = name.trim().to_lowercase();
    if let Some((_, why)) = KNOWN_UNREGISTERED.iter().find(|(id, _)| *id == want) {
        return Error::Invalid(format!(
            "VM backend '{}' is not available in this build: {why}",
            name.trim()
        ));
    }
    Error::Invalid(format!(
        "unknown VM backend: '{}' (use {})",
        name.trim(),
        registered_backend_ids()
    ))
}

/// Normalizes any accepted alias (`ch`, `cloudhypervisor`, `kvm`, `qemu`, …)
/// to the canonical backend id. `None` for an empty string (the "no opinion"
/// case, distinct from an unknown name — callers that need to reject unknown
/// names do so themselves, since an empty string is valid here but not in
/// [`select_backend`]'s explicit-request arm).
fn canonical_backend_name(s: &str) -> Option<&'static str> {
    let want = s.trim().to_lowercase();
    with_backends(|bs| {
        bs.iter()
            .find(|b| b.id == want || b.aliases.contains(&want.as_str()))
            .map(|b| b.id)
    })
}

/// Selects a backend from an explicit request or by auto-detection (the first
/// registered entry that is actually installed — today cloud-hypervisor, then
/// libvirt).
pub fn select_backend(want: Option<&str>) -> Result<Box<dyn VmBackend>> {
    match want.map(str::trim) {
        Some(other) if !other.is_empty() => {
            make_backend(other).unwrap_or_else(|| Err(unknown_backend(other)))
        }
        _ => with_backends(auto_detect),
    }
}

/// Auto-detection: the first registered entry that is auto-selectable AND
/// installed.
///
/// **The `auto_selectable` filter reads the REGISTRATION and runs before
/// anything is built**, and that order is the whole point. It used to be
/// `.map(|b| (b.new)()).filter(|b| b.auto_selectable())` — every candidate was
/// constructed and the wrong ones thrown away. For a local backend that is free
/// (both are unit structs), which is why nothing ever noticed; for a remote one
/// CONSTRUCTION is where authentication happens, so auto-detection made exactly
/// the network round trip the flag exists to prevent.
///
/// Takes the entries rather than reading the registry, so a test can hand it a
/// table where the skipped candidate is actually REACHED. Against the global
/// registry it never is: this host has a local backend installed, the walk
/// stops at the first one, and a test written that way passes whether the order
/// is right or wrong.
fn auto_detect(entries: &[BackendRegistration]) -> Result<Box<dyn VmBackend>> {
    for e in entries.iter().filter(|b| b.auto_selectable) {
        // A built-in constructor is infallible; a registered one that fails is
        // not a reason to abort a walk whose next candidate may serve fine.
        if let Ok(b) = (e.new)() {
            if b.available() {
                return Ok(b);
            }
        }
    }
    Err(Error::Invalid(
        "no VM backend available: install 'cloud-hypervisor' or 'libvirt'+'qemu'".into(),
    ))
}

/// Does the backend that would run this VM own its own storage?
///
/// For a caller that has to decide something BEFORE `create_with` — the CLI
/// generating a NoCloud seed ISO, which is a file on this filesystem and
/// therefore meaningless to a hypervisor on another machine. Without this the
/// CLI built one anyway and handed over a path the node cannot read.
///
/// Same resolution as [`select_backend`], so the answer is about the backend
/// that will actually be used. `false` when nothing resolves: the caller then
/// keeps its old behaviour and the real error comes from `create_with`, which
/// is where it reads properly.
pub fn backend_manages_own_storage(want: Option<&str>) -> bool {
    select_backend(want)
        .map(|b| b.manages_own_storage())
        .unwrap_or(false)
}

/// Validates and normalizes a backend name for external callers (the CLI's
/// `HYPERVISOR` VMfile instruction, `vm default-backend --set`) — same
/// acceptance rules as [`select_backend`]'s explicit-request arm, without
/// needing to construct a [`VmBackend`] just to validate a string.
pub fn valid_backend_name(s: &str) -> Result<&'static str> {
    canonical_backend_name(s).ok_or_else(|| unknown_backend(s))
}

/// File that persists the machine-wide default backend (`<base>/vm-default-backend`,
/// a bare canonical name, no JSON — this repo avoids new parsing surface for a
/// single string). Sibling of `vms_dir(base)`/`store(base)`'s root.
fn default_backend_file(base: &Path) -> PathBuf {
    base.join("vm-default-backend")
}

/// The persisted default backend, if one was set with [`set_default_backend`].
/// Best-effort: a missing or unreadable file is `None`, never an error — this
/// is a convenience default, not a requirement, and a corrupt/stale file must
/// not block `vm create` (fall through to auto-detection instead).
pub fn get_default_backend(base: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(default_backend_file(base)).ok()?;
    canonical_backend_name(raw.trim()).map(str::to_string)
}

/// Persists the default backend used when neither `--backend` nor
/// `DELONIX_VM_BACKEND` is given (see the precedence documented on
/// [`create_with`]). Validated before writing — refusing an unknown name here
/// is cheap; discovering it at the next `vm create` is not.
pub fn set_default_backend(base: &Path, backend: &str) -> Result<()> {
    let canon = valid_backend_name(backend)?;
    std::fs::create_dir_all(base)?;
    // Atomic: a torn write leaves a truncated backend name, and the reader has no way to
    // tell "libvir" from a value someone meant to write.
    delonix_runtime_core::write_atomic(&default_backend_file(base), canon.as_bytes())?;
    Ok(())
}

/// Removes the persisted default (falls back to `DELONIX_VM_BACKEND`/auto-detection).
/// Idempotent: a default that was never set is not an error.
pub fn clear_default_backend(base: &Path) -> Result<()> {
    match std::fs::remove_file(default_backend_file(base)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// The backend that started an already-persisted VM (for liveness/stop).
///
/// Resolved through [`BACKENDS`], and **fail-closed**: it used to end in
/// `_ => CloudHypervisorBackend`, which is the worst place in this crate for a
/// silent default. Unlike [`select_backend`], which answers "what should run
/// this?", this one answers "what IS running this?" — and getting that wrong
/// does not fail, it LIES: `is_running` on a live libvirt VM through the Cloud
/// Hypervisor backend reports it stopped, and `stop` then tears down the record
/// of a guest that is still up.
///
/// `vm.backend` is written by `create` from `valid_backend_name`, so a record
/// this cannot resolve means the file was hand-edited or written by a build
/// that knew a backend this one does not. Both deserve a sentence naming the
/// VM and the value, not a guess.
fn backend_for(vm: &Vm) -> Result<Box<dyn VmBackend>> {
    make_backend(&vm.backend).unwrap_or_else(|| {
        Err(Error::Invalid(format!(
            "vm '{}': its record names backend '{}', which this process does not have registered \
             (it has {})",
            vm.name,
            vm.backend,
            registered_backend_ids()
        )))
    })
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
        // The MAC is needed BEFORE the attach now, not after: it is what makes
        // the guest's future DHCP address computable, and that address is what
        // the attach registers in the namespace sets.
        let mac = mac_for(&cfg.name);
        let ns = vm_namespace_of(cfg);
        let tap = infra::vm_attach(&cfg.name, &cfg.network, &mac, &ns)?;
        let lease = infra::dhcp_ip_for_mac(&cfg.network, &mac);
        on(CreateStage::Start);
        let pid = match boot_ch(vmdir, cfg, overlay, &tap, &mac) {
            Ok(p) => p,
            Err(e) => {
                infra::vm_detach(&cfg.name, lease.as_deref());
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

    /// Computed from the MAC, and available before the guest has booted — see
    /// [`infra::dhcp_lease_ip`], and [`VmBackend::ip_is_predicted`] for why
    /// anyone waiting on a boot needs to be told.
    fn ip_is_predicted(&self) -> bool {
        true
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
        // The record's own address if it learned one; otherwise the lease its MAC
        // maps to — so a VM stopped before it ever DHCP'd still gives up its chain.
        let ip = vm
            .ip
            .clone()
            .or_else(|| infra::dhcp_ip_for_mac(&vm.network, &vm.mac));
        infra::vm_detach(&vm.name, ip.as_deref());
        Ok(())
    }

    // ---- snapshots -------------------------------------------------------
    //
    // OFFLINE, in the VM's own qcow2 (`qemu-img snapshot`) — the same kind of
    // artifact libvirt makes for a shut-off domain, so a checkpoint means the
    // same thing on both backends.
    //
    // **Why not Cloud Hypervisor's own `vm.snapshot`**, which exists and works
    // (measured on a live VM: pause → `PUT /api/v1/vm.snapshot` → resume writes
    // `config.json` + `state.json` + a `memory-ranges` the size of the guest's
    // whole RAM): it captures memory and devices and **NOT the disk**, and CH
    // has no live disk-snapshot API at all — while the vmm runs it holds the
    // qcow2 under an exclusive lock, so nothing else can checkpoint it either
    // (`qemu-img` answers "Failed to lock byte 100"). Restoring that later,
    // against a disk that kept being written, is not a rollback: it is a guest
    // whose memory believes in a filesystem that has moved on. Exposing it as
    // `snapshot` would make the SAME command mean «go back in time» on libvirt
    // and «resume this exact moment, if nothing touched the disk» here — the
    // kind of quiet divergence between backends this engine refuses to ship.
    // A `vm suspend`/`vm resume` pair is where that capability belongs.

    fn snapshots(&self, vmdir: &Path, vm: &Vm) -> Result<Vec<String>> {
        // `-U` (force-share) because this has to answer while the VM RUNS, and
        // the vmm holds the write lock: `qemu-img info` opens read-only, which
        // is the only mode force-share allows. Plain `snapshot -l` opens
        // read-write and fails on a running VM.
        let out = capture(
            "qemu-img",
            &["info", "-U", "--", &ch_overlay(vmdir, vm).to_string_lossy()],
        )
        .ok_or_else(|| Error::Runtime {
            context: "qemu-img info",
            message: format!("could not read the disk of VM '{}'", vm.name),
        })?;
        Ok(parse_qemu_snapshot_list(&out))
    }

    fn snapshot(&self, vmdir: &Path, vm: &Vm, name: &str) -> Result<()> {
        self.offline_snapshot_op(vmdir, vm, "take a snapshot of")?;
        if self.snapshots(vmdir, vm)?.iter().any(|s| s == name) {
            return Err(taken_snapshot(&vm.name, name));
        }
        qemu_img_snapshot(vmdir, vm, "-c", name)
    }

    fn restore(&self, vmdir: &Path, vm: &Vm, name: &str) -> Result<()> {
        self.offline_snapshot_op(vmdir, vm, "restore")?;
        if !self.snapshots(vmdir, vm)?.iter().any(|s| s == name) {
            return Err(missing_snapshot(&vm.name, name));
        }
        qemu_img_snapshot(vmdir, vm, "-a", name)
    }

    fn delete_snapshot(&self, vmdir: &Path, vm: &Vm, name: &str) -> Result<()> {
        self.offline_snapshot_op(vmdir, vm, "delete a snapshot of")?;
        if !self.snapshots(vmdir, vm)?.iter().any(|s| s == name) {
            return Err(missing_snapshot(&vm.name, name));
        }
        qemu_img_snapshot(vmdir, vm, "-d", name)
    }
}

impl CloudHypervisorBackend {
    /// Refuses a snapshot verb while the VM runs, saying WHY and what to do —
    /// this is a limit of the hypervisor, not a missing feature: the running
    /// vmm holds the qcow2 exclusively, so there is no way to checkpoint the
    /// disk under it. Silence here would be worse than the refusal: the write
    /// simply would not happen.
    fn offline_snapshot_op(&self, _vmdir: &Path, vm: &Vm, what: &str) -> Result<()> {
        if !self.is_running(vm) {
            return Ok(());
        }
        Err(Error::Runtime {
            context: "vm",
            message: format!(
                "cloud-hypervisor cannot {what} a RUNNING VM: the vmm holds its disk exclusively \
                 and CH has no live disk-snapshot API. Stop it first (`delonix vm stop {}`) — the \
                 snapshot is then taken in the disk itself and survives everything. A VM that \
                 needs checkpoints while it runs belongs on `--backend libvirt`.",
                vm.name
            ),
        })
    }
}

/// The per-VM overlay `create` builds — the file the checkpoints live in.
fn ch_overlay(vmdir: &Path, vm: &Vm) -> PathBuf {
    vmdir.join(format!("{}.qcow2", vm.name))
}

fn qemu_img_snapshot(vmdir: &Path, vm: &Vm, flag: &str, name: &str) -> Result<()> {
    let disk = ch_overlay(vmdir, vm);
    // `--` before the path: a name is already validated, the path is ours, and
    // this keeps the habit that has bitten this repo before with `ssh`/`virsh`.
    quiet(
        "qemu-img",
        &["snapshot", flag, name, "--", &disk.to_string_lossy()],
    )
    .map(|_| ())
    .map_err(|e| Error::Runtime {
        context: "qemu-img snapshot",
        message: e,
    })
}

/// Pure parser for the `Snapshot list:` block of `qemu-img info`. Written
/// against the REAL output captured on this host, not from the man page:
///
/// ```text
/// Snapshot list:
/// ID        TAG               VM SIZE                DATE     VM CLOCK     ICOUNT
/// 1         manual1               0 B 2026-08-12 16:44:30 00:00:00.000          0
/// ```
///
/// The block ends at the next unindented section (`Format specific
/// information:`), and an image with no snapshots has no block at all — which
/// is an empty list, not an error.
fn parse_qemu_snapshot_list(out: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut inside = false;
    for line in out.lines() {
        if line.starts_with("Snapshot list:") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        match cols.as_slice() {
            // A row always starts with a numeric ID; the TAG is the name.
            [id, tag, ..] if id.parse::<u64>().is_ok() => names.push((*tag).to_string()),
            // The column header sits between the marker and the first row —
            // treating it as a row invented a snapshot called "TAG", and
            // breaking on it (the first version) returned nothing at all.
            ["ID", ..] => continue,
            // Anything else is the next section: the block is over.
            _ => break,
        }
    }
    names
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
    for p in DEFAULT_CH_FIRMWARES {
        if Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    None
}

/// Where [`default_ch_firmware`] looks, in order. **The EDK2 build comes
/// first, and the order is the whole point.**
///
/// Measured 2026-08-12, not assumed: under `rust-hypervisor-fw` NO image this
/// project builds boots in Cloud Hypervisor. The `delonix-vm-base:*` ones leave
/// the overlay at 448 KiB — the guest never wrote a byte — and the k8s golden
/// dies in the Secure Boot shim (`import_mok_state() failed: Unsupported`, read
/// off the serial console) without ever reaching a kernel. With the EDK2
/// `CLOUDHV.fd` the same images boot and get an address on the SDN:
/// ubuntu-24.04 in 7.8s, ubuntu-26.04 and debian-bookworm in 5s, rocky-9 in
/// 32s, the golden in 7s (fedora-42 does not, and does not under libvirt
/// either — a separate problem, see AGENTS.md).
///
/// `hypervisor-fw` stays in the list rather than being dropped: it is ~150 KB,
/// it starts faster where it works, and a VM on a host that only has it must
/// keep booting the way it did.
const DEFAULT_CH_FIRMWARES: [&str; 4] = [
    "/usr/local/share/delonix/CLOUDHV.fd",
    "/usr/share/delonix/CLOUDHV.fd",
    "/usr/local/share/delonix/hypervisor-fw",
    "/usr/share/delonix/hypervisor-fw",
];

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

/// Pure argv for `virsh snapshot-create-as`. A running domain's snapshot is a
/// system checkpoint (memory + disk); `--atomic` fails cleanly instead of leaving
/// a half-made snapshot. `--domain`/`--name` are flags (not positional), so the
/// already-validated names can never be read as options — no `--` needed.
fn libvirt_snapshot_argv(uri: &str, domain: &str, snap: &str) -> Vec<String> {
    vec![
        "-c".into(),
        uri.into(),
        "snapshot-create-as".into(),
        "--domain".into(),
        domain.into(),
        "--name".into(),
        snap.into(),
        "--atomic".into(),
    ]
}

/// Pure argv for `virsh snapshot-revert`.
fn libvirt_revert_argv(uri: &str, domain: &str, snap: &str) -> Vec<String> {
    vec![
        "-c".into(),
        uri.into(),
        "snapshot-revert".into(),
        "--domain".into(),
        domain.into(),
        "--snapshotname".into(),
        snap.into(),
    ]
}

/// "This VM has no snapshot by that name" — `NotFound`, not `Runtime`, because
/// the exit code is the part a script reads: 4 means «it is not there», 1 means
/// «something broke», and a caller that has to tell them apart cannot parse the
/// message (it is translated).
fn missing_snapshot(vm: &str, snap: &str) -> Error {
    Error::NotFound(format!("snapshot of VM '{vm}': {snap}"))
}

/// The name is TAKEN — `Conflict` (exit 5), the class whose next move is «pick
/// another name or remove that one», as opposed to «create it» (4) or
/// «something broke» (1).
fn taken_snapshot(vm: &str, snap: &str) -> Error {
    Error::Conflict(format!(
        "VM '{vm}' already has a snapshot named '{snap}' (see `delonix vm snapshot ls {vm}`)"
    ))
}

/// Where a VM's snapshot metadata is kept on OUR side, under the per-VM
/// directory that `remove` already deletes wholesale (so an `rm` takes the
/// preserved metadata with it, and nothing is left pointing at a disk that
/// no longer exists).
fn snapshot_meta_dir(vmdir: &Path, name: &str) -> PathBuf {
    vmdir.join(name).join("snapshots")
}

/// The snapshot names preserved for `name`, sorted. Absent directory = none —
/// this is the answer for a VM that never had a snapshot AND for one that has
/// never been stopped, which is why the caller only consults it when libvirt
/// itself does not know the domain.
fn preserved_snapshot_names(vmdir: &Path, name: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(snapshot_meta_dir(vmdir, name)) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("xml") {
                return None;
            }
            p.file_stem().and_then(|s| s.to_str()).map(String::from)
        })
        .collect();
    out.sort();
    out
}

/// Rewrites the `<uuid>` of a snapshot's embedded `<domain>` description.
/// **Pure** — this is the one thing that stands between a preserved snapshot
/// and libvirt taking it back.
///
/// `snapshot-create --redefine` REFUSES an XML whose domain uuid is not the
/// current one ("definition for snapshot s must use uuid …"), and the uuid is
/// assigned by libvirt at `define` time — so the domain this engine re-defines
/// on `vm start` never has the uuid it had when the snapshot was taken.
/// Rewriting it is the honest translation of "this snapshot belongs to this
/// VM": the disk, the name and the memory image are the same; only libvirt's
/// handle for the domain changed.
fn snapshot_xml_with_uuid(xml: &str, uuid: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(open) = rest.find("<uuid>") {
        let after = &rest[open + "<uuid>".len()..];
        let Some(close) = after.find("</uuid>") else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str("<uuid>");
        out.push_str(uuid);
        out.push_str("</uuid>");
        rest = &after[close + "</uuid>".len()..];
    }
    out.push_str(rest);
    out
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
    match stable_cmd(prog).args(args).output() {
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
/// The `<iotune>` block for the root disk, or an empty string when no ceiling is
/// configured. See the call site in [`libvirt_domain_xml`] for why this one is
/// opt-in while the memory and CPU ceilings are not.
fn vm_iotune_xml() -> String {
    let num = |var: &str| {
        std::env::var(var)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
    };
    iotune_xml_from(num("DELONIX_VM_IO_MAX_BPS"), num("DELONIX_VM_IO_MAX_IOPS"))
}

/// Composes the `<iotune>` block. **Pure**, and that is the point.
///
/// The composition used to be tested by SETTING the environment variables, in a
/// test binary where every test runs in the same process, in parallel. The
/// sibling test that asserts iotune is opt-in reads the same variables — so it
/// saw whatever this one had just set, and failed with "o iotune tem de ser
/// opt-in" depending on scheduling. The test's own comment warned about exactly
/// this ("mexer em env vars num teste paralelo é uma corrida com todos os
/// outros") while its neighbour did it anyway.
///
/// Reading the environment is now the ONLY thing `vm_iotune_xml` does that is
/// not testable in isolation, and nothing tests it.
fn iotune_xml_from(bps: Option<u64>, iops: Option<u64>) -> String {
    if bps.is_none() && iops.is_none() {
        return String::new();
    }
    let mut s = String::from("      <iotune>\n");
    if let Some(b) = bps {
        // `total_bytes_sec` rather than a read/write pair: the resource being
        // protected is the DEVICE's throughput, and a guest can exhaust it from
        // either direction.
        s.push_str(&format!("        <total_bytes_sec>{b}</total_bytes_sec>\n"));
    }
    if let Some(i) = iops {
        s.push_str(&format!("        <total_iops_sec>{i}</total_iops_sec>\n"));
    }
    s.push_str("      </iotune>\n");
    s
}

/// The `<memtune><hard_limit>` for a guest of `guest_kib`, in KiB — the host-side
/// ceiling on the whole QEMU process. `None` disables the element
/// (`DELONIX_VM_MEM_HARD_LIMIT=off`).
///
/// See the call site in [`libvirt_domain_xml`] for why the margin is generous
/// rather than tight.
fn mem_hard_limit_kib(guest_kib: u64) -> Option<u64> {
    if std::env::var("DELONIX_VM_MEM_HARD_LIMIT").as_deref() == Ok("off") {
        return None;
    }
    let pct = std::env::var("DELONIX_VM_MEM_OVERHEAD_PCT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|p| (5..=200).contains(p))
        .unwrap_or(25);
    const MIN_OVERHEAD_KIB: u64 = 1024 * 1024; // 1 GiB
                                               // Multiply BEFORE dividing: `x / 100 * pct` truncates twice and loses up to
                                               // ~100 KiB of the margin. Irrelevant in practice, but this is a ceiling that
                                               // decides whether the host OOM-kills the domain — it should be the number it
                                               // claims to be. No overflow concern: a 1 PiB guest is ~1e12 KiB, and ×200 is
                                               // still four orders of magnitude inside u64.
    let overhead = guest_kib
        .saturating_mul(pct)
        .saturating_div(100)
        .max(MIN_OVERHEAD_KIB);
    Some(guest_kib.saturating_add(overhead))
}

/// The `<cputune><quota>` in microseconds per 100 ms period, or `None` to omit
/// the ceiling (`DELONIX_VM_CPU_QUOTA_CORES=off`).
///
/// Defaults to `vcpus + 1` cores — see the call site for why the extra core is
/// not slack but a correctness requirement for QEMU's emulator/IO threads.
fn cpu_quota_micros(vcpus: u32) -> Option<u64> {
    const PERIOD: u64 = 100_000;
    match std::env::var("DELONIX_VM_CPU_QUOTA_CORES").as_deref() {
        Ok("off") => None,
        Ok(v) => v
            .parse::<f64>()
            .ok()
            .filter(|c| *c > 0.0)
            .map(|cores| ((cores * PERIOD as f64).round() as u64).max(1000)),
        Err(_) => Some((vcpus as u64 + 1) * PERIOD),
    }
}

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
    // CONTAINMENT (1/2): a ceiling on what the QEMU process may take from the
    // HOST, as opposed to what the guest is told it has.
    //
    // BUG FIXED HERE. `<memory>` is an ALLOCATION — it sizes the guest's view.
    // It is not a limit the host enforces: QEMU's real RSS is guest RAM *plus*
    // device models, video buffers, migration buffers and its own heap, and a
    // leak or a hostile guest driver pushes that arbitrarily far with nothing to
    // stop it. `<memtune><hard_limit>` is the cgroup ceiling libvirt applies to
    // the domain, and without it a single VM can take the host down — the exact
    // failure the container path guards against with `memory.max`.
    //
    // The margin is deliberately GENEROUS. libvirt's own documentation warns
    // that a hard_limit set too tight gets the domain OOM-killed by the host,
    // and a VM that dies at random is worse than a VM that is merely unbounded.
    // `guest + max(1 GiB, 25 %)` bounds a runaway while leaving real headroom
    // for legitimate overhead. `DELONIX_VM_MEM_OVERHEAD_PCT` tunes the
    // percentage; `DELONIX_VM_MEM_HARD_LIMIT=off` disables the element entirely
    // for anyone who measured their workload and wants the old behaviour.
    if let Some(limit_kib) = mem_hard_limit_kib(kib) {
        s.push_str(&format!(
            "  <memtune>\n    <hard_limit unit='KiB'>{limit_kib}</hard_limit>\n  </memtune>\n"
        ));
    }
    s.push_str(&format!("  <vcpu placement='static'>{vcpus}</vcpu>\n"));
    // CPU pinning (NUMA/determinism) and CONTAINMENT (2/2).
    //
    // `<vcpu>N` bounds the vCPU THREADS to N cores, but a domain is more than
    // its vCPUs: the emulator thread and QEMU's I/O threads run outside that
    // count and are, without a quota, unbounded. `<cputune><period>/<quota>`
    // is the domain-wide CPU ceiling.
    //
    // `(vcpus + 1) × period` on purpose: exactly `vcpus × period` would make the
    // vCPUs and the emulator compete for the same budget, so a VM with every
    // vCPU busy would starve its own I/O thread — a performance cliff that looks
    // like a disk problem. One core of headroom keeps normal operation intact
    // while still bounding a runaway. `DELONIX_VM_CPU_QUOTA_CORES` overrides the
    // ceiling outright (fractional allowed — this is how you give a tenant 8
    // vCPUs for parallelism but only 2 cores of throughput); `off` disables it.
    let cputune_quota = cpu_quota_micros(vcpus);
    if cfg.cpu_affinity.is_some() || cputune_quota.is_some() {
        s.push_str("  <cputune>\n");
        if let Some(q) = cputune_quota {
            s.push_str("    <period>100000</period>\n");
            s.push_str(&format!("    <quota>{q}</quota>\n"));
        }
        if let Some(list) = &cfg.cpu_affinity {
            let list = xml_escape(list);
            for v in 0..vcpus {
                s.push_str(&format!("    <vcpupin vcpu='{v}' cpuset='{list}'/>\n"));
            }
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
    // CONTAINMENT (3/3): a per-disk I/O ceiling — the VM-side analogue of the
    // container path's `io.max`, and the last of the three resources a guest can
    // exhaust on its host.
    //
    // Without it a VM writing flat out saturates the disk that also carries the
    // host's journald, the engine's own store and swap — with CPU and memory
    // already capped, this was the remaining way for one guest to make the whole
    // box unresponsive. Applied to the ROOT disk only: that is the one the guest
    // can drive arbitrarily hard, and the cdrom seed is read once at boot.
    //
    // OFF by default, unlike the memory/CPU ceilings. Throttling disk on VMs
    // that already exist would be a silent performance change on upgrade, and
    // unlike a memory cap there is no safe "generous" value — the right number
    // depends on the device. `DELONIX_VM_IO_MAX_BPS` (and the `_IOPS` twin) turn
    // it on; both accept plain byte/op counts.
    s.push_str(&vm_iotune_xml());
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
    // Video: a display adapter is ALWAYS present unless explicitly suppressed
    // with `video: none`.
    //
    // It used to appear only alongside `--vnc`, and that conflated two
    // different things: **VNC is remote access to a screen; VGA is the machine
    // HAVING one.** A domain with no display adapter at all is unusual — a
    // plain `virt-install` always gives one — and guests exist that simply do
    // not boot without it.
    //
    // Measured, and it cost hours: every Proxmox appliance image (the vendor's
    // own installer output, before this repo touched it) boots into a
    // `SeaBIOS → GRUB → reset` loop under `qemu -vga none`, never printing a
    // single kernel line. With an adapter present, the same image boots and
    // gets a DHCP lease. So `delonix vm create <appliance>` worked with `--vnc`
    // and produced a machine that silently reset without it — the flag people
    // reach for to LOOK at a guest was the thing making it work.
    //
    // The default model is `virtio` for a VNC domain (as before) and the plain
    // `vga` otherwise: no guest driver needed, which is the point when nobody
    // is going to connect and the adapter exists only so firmware and kernel
    // find a console.
    match cfg.video.as_deref() {
        Some("none") => {}
        Some(m) => s.push_str(&format!(
            "    <video><model type='{}' heads='1'/></video>\n",
            xml_escape(m)
        )),
        None if cfg.vnc => s.push_str("    <video><model type='virtio' heads='1'/></video>\n"),
        None => s.push_str("    <video><model type='vga' heads='1'/></video>\n"),
    }
    // VFIO: PCI device passthrough (SR-IOV VF, GPU).
    for dev in &cfg.devices {
        if let Some((dom, bus, slot, func)) = parse_pci_addr(dev) {
            // `parse_pci_addr` already restricts these to fixed-width hex, so
            // `xml_escape` here is defense-in-depth, not the primary guard —
            // matches the discipline every other field in this function follows.
            let (dom, bus, slot, func) = (
                xml_escape(&dom),
                xml_escape(&bus),
                xml_escape(&slot),
                xml_escape(&func),
            );
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
///
/// BUG FOUND: this used to return the four raw split substrings with no
/// validation that they're actually hex — every OTHER user-influenced value
/// in `libvirt_domain_xml` goes through `xml_escape` except these four,
/// which get interpolated straight into `<address domain='0x{dom}' .../>`.
/// `cfg.devices` is manifest-reachable (`spec.devices`), so a value like
/// `0' foo='bar:00:00.0` split fine (no `:`/`.`/`/` in `dom`) and produced
/// injected-attribute XML. Fixed at the source: each component must be
/// valid hex of the expected width (domain 4, bus 2, slot 2, func 1) or the
/// whole address is rejected — `xml_escape` is also applied at the call
/// site as defense-in-depth, matching every other field in this function.
fn parse_pci_addr(dev: &str) -> Option<(String, String, String, String)> {
    fn is_hex_of_len(s: &str, len: usize) -> bool {
        s.len() == len && s.chars().all(|c| c.is_ascii_hexdigit())
    }
    let bdf = dev.rsplit('/').next().unwrap_or(dev); // 0000:65:00.1
    let (rest, func) = bdf.rsplit_once('.')?;
    let mut it = rest.split(':');
    let dom = it.next()?;
    let bus = it.next()?;
    let slot = it.next()?;
    if it.next().is_some() {
        return None;
    }
    if !(is_hex_of_len(dom, 4)
        && is_hex_of_len(bus, 2)
        && is_hex_of_len(slot, 2)
        && is_hex_of_len(func, 1))
    {
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
                let _ = stable_cmd("virsh")
                    .args(["-c", uri, "net-define", &path.to_string_lossy()])
                    .output();
            }
            let _ = std::fs::remove_file(&path);
        }
    }
    // `.output()` (not `.status()`) so the "Network default started / marked as
    // autostarted" chatter does not leak into the clean `vm create` progress.
    let _ = stable_cmd("virsh")
        .args(["-c", uri, "net-start", "--", net])
        .output();
    let _ = stable_cmd("virsh")
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
        delonix_runtime_core::write_atomic(&xml_path, xml.as_bytes())?;

        // Idempotent: if the domain already exists (auto-heal), it just (re)starts; otherwise
        // define + start. `virsh start` on an already-running domain is a benign no-op.
        let defined = capture("virsh", &["-c", uri, "domstate", "--", &cfg.name]).is_some();
        if !defined {
            on(CreateStage::Define);
            run_quiet("virsh", &["-c", uri, "define", &xml_path.to_string_lossy()])?;
            // A domain defined here is a domain libvirt has never seen before,
            // so any snapshot this VM had is metadata WE are holding — from the
            // `stop` that undefined it. Hand it back now, before the guest
            // runs: `vm snapshots`/`vm restore` must answer about the VM, not
            // about how many times it has been stopped.
            Self::redefine_preserved_snapshots(uri, &cfg.name, vmdir);
        }
        on(CreateStage::Start);
        let out = stable_cmd("virsh")
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

    fn stop(&self, _vmdir: &Path, vm: &Vm) -> Result<()> {
        libvirt_cleanup(&vm.name)?;
        // The domain XML that `boot` wrote STAYS. It used to be deleted here,
        // which is tidy right up to the moment something needs the domain back:
        // `snapshot`/`restore` on a stopped VM define it again from this exact
        // file — the one libvirt itself had, DAC seclabel and all — instead of
        // re-deriving a description that would only have to agree with `boot`
        // by hand. `remove` still deletes it, with everything else.
        Ok(())
    }

    fn snapshot(&self, vmdir: &Path, vm: &Vm, name: &str) -> Result<()> {
        let take = |uri: &str| -> Result<()> {
            let argv = libvirt_snapshot_argv(uri, &vm.name, name);
            quiet(
                "virsh",
                &argv.iter().map(String::as_str).collect::<Vec<_>>(),
            )
            .map(|_| ())
            .map_err(|e| Error::Runtime {
                context: "virsh snapshot-create-as",
                message: e,
            })
        };
        match libvirt_domain_uri(&vm.name) {
            Some(uri) => {
                // A name already taken is a CONFLICT (exit 5, «pick another or
                // remove it»), not a generic 1 — and virsh answers it in its
                // own vocabulary ("domain moment off1 already exists"), where
                // `moment` is a word this CLI never uses.
                if Self::live_snapshots(uri, &vm.name)
                    .iter()
                    .any(|s| s == name)
                {
                    return Err(taken_snapshot(&vm.name, name));
                }
                take(uri)
            }
            // Stopped: virsh needs a domain to snapshot, and this engine's stop
            // undefines it. Defining it back for the length of the command
            // gives a DISK-ONLY checkpoint (`state=shutoff`) — which is the
            // honest thing for a VM with no memory to capture, and exactly what
            // `virsh` itself does for a shut-off domain.
            None => {
                if preserved_snapshot_names(vmdir, &vm.name)
                    .iter()
                    .any(|s| s == name)
                {
                    return Err(taken_snapshot(&vm.name, name));
                }
                self.with_stopped_domain(vmdir, vm, &take).map(|_| ())
            }
        }
    }

    fn restore(&self, vmdir: &Path, vm: &Vm, name: &str) -> Result<()> {
        let revert = |uri: &str| -> Result<()> {
            let argv = libvirt_revert_argv(uri, &vm.name, name);
            quiet(
                "virsh",
                &argv.iter().map(String::as_str).collect::<Vec<_>>(),
            )
            .map(|_| ())
            .map_err(|e| Error::Runtime {
                context: "virsh snapshot-revert",
                message: e,
            })
        };
        let Some(uri) = libvirt_domain_uri(&vm.name) else {
            // Stopped. Name the missing snapshot BEFORE defining a domain to
            // revert nothing to — and never with the raw virsh answer ("failed
            // to get domain"), which sends the reader looking for a VM that is
            // sitting right there in `vm ls`.
            if !preserved_snapshot_names(vmdir, &vm.name)
                .iter()
                .any(|s| s == name)
            {
                return Err(missing_snapshot(&vm.name, name));
            }
            return self.with_stopped_domain(vmdir, vm, &revert).map(|running| {
                if running {
                    // Reverting to a checkpoint taken while the VM ran means the
                    // VM runs again — say so, because the command was given to a
                    // stopped VM and nobody asked for a boot.
                    eprintln!(
                        "note: VM '{}' is RUNNING again — the snapshot '{name}' was taken with \
                         it running, and a revert restores the memory state too",
                        vm.name
                    );
                }
            });
        };
        // Running, and the snapshot has to exist for the same reason it does
        // above: `Error::NotFound` is the exit code 4 that says «create it»,
        // and virsh's own "Domain snapshot not found" came out as a generic 1,
        // so the SAME question got two different answers depending on whether
        // the VM happened to be up (see docs/cli-stability.md).
        if !Self::live_snapshots(uri, &vm.name)
            .iter()
            .any(|s| s == name)
        {
            return Err(missing_snapshot(&vm.name, name));
        }
        revert(uri)
    }

    fn delete_snapshot(&self, vmdir: &Path, vm: &Vm, name: &str) -> Result<()> {
        let del = |uri: &str| -> Result<()> {
            quiet(
                "virsh",
                &[
                    "-c",
                    uri,
                    "snapshot-delete",
                    "--domain",
                    &vm.name,
                    "--snapshotname",
                    name,
                ],
            )
            .map(|_| ())
            .map_err(|e| Error::Runtime {
                context: "virsh snapshot-delete",
                message: e,
            })
        };
        let done = match libvirt_domain_uri(&vm.name) {
            Some(uri) => {
                if !Self::live_snapshots(uri, &vm.name)
                    .iter()
                    .any(|s| s == name)
                {
                    return Err(missing_snapshot(&vm.name, name));
                }
                del(uri)
            }
            None => {
                if !preserved_snapshot_names(vmdir, &vm.name)
                    .iter()
                    .any(|s| s == name)
                {
                    return Err(missing_snapshot(&vm.name, name));
                }
                self.with_stopped_domain(vmdir, vm, &del).map(|_| ())
            }
        };
        // The preserved copy goes even when the VM is running and the dump was
        // therefore not refreshed: leaving it would let the next `start`
        // redefine metadata for a snapshot whose state has just been deleted
        // from the disk — a name that lists fine and fails on revert.
        let _ =
            std::fs::remove_file(snapshot_meta_dir(vmdir, &vm.name).join(format!("{name}.xml")));
        done
    }

    fn snapshots(&self, vmdir: &Path, vm: &Vm) -> Result<Vec<String>> {
        // No domain in libvirt = the VM is stopped, and libvirt knows nothing
        // about its snapshots (the undefine took the metadata). Answering from
        // the live query alone printed an EMPTY list with rc=0 for a VM whose
        // snapshots were intact on disk — indistinguishable from a VM that
        // never had one.
        match libvirt_domain_uri(&vm.name) {
            None => Ok(preserved_snapshot_names(vmdir, &vm.name)),
            Some(uri) => Ok(Self::live_snapshots(uri, &vm.name)),
        }
    }

    fn preserve_snapshots(&self, vmdir: &Path, vm: &Vm) -> Result<Vec<String>> {
        let Some(uri) = libvirt_domain_uri(&vm.name) else {
            return Ok(Vec::new()); // no domain: nothing for the undefine to destroy
        };
        let names = Self::live_snapshots(uri, &vm.name);
        let dir = snapshot_meta_dir(vmdir, &vm.name);
        // Rewritten from scratch, never merged: a snapshot deleted while the VM
        // ran must not be resurrected by a leftover file on the next start.
        let _ = std::fs::remove_dir_all(&dir);
        if names.is_empty() {
            return Ok(names);
        }
        std::fs::create_dir_all(&dir)?;
        for n in &names {
            // The name becomes a file name. Everything this engine creates
            // passes `valid_vm_name`, so this only ever trips on a snapshot
            // made directly with virsh — refuse rather than write outside the
            // directory, and rather than let the undefine eat it in silence.
            if !valid_vm_name(n) {
                return Err(Error::Runtime {
                    context: "vm",
                    message: format!(
                        "VM '{}': cannot preserve the snapshot '{n}' across a stop (its name is \
                         not usable as a file name). Remove it first with `virsh -c {uri} \
                         snapshot-delete --domain {} --snapshotname '{n}'`",
                        vm.name, vm.name
                    ),
                });
            }
            let xml = capture(
                "virsh",
                &[
                    "-c",
                    uri,
                    "snapshot-dumpxml",
                    "--domain",
                    &vm.name,
                    "--snapshotname",
                    n,
                ],
            )
            .ok_or_else(|| Error::Runtime {
                context: "virsh snapshot-dumpxml",
                message: format!(
                    "VM '{}': could not read the snapshot '{n}' to preserve it across the stop \
                     (nothing was stopped — the metadata would be lost by the undefine)",
                    vm.name
                ),
            })?;
            delonix_runtime_core::write_atomic(&dir.join(format!("{n}.xml")), xml.as_bytes())?;
        }
        Ok(names)
    }
}

impl LibvirtBackend {
    /// Snapshot names libvirt itself knows for `name` (`--name` → one per
    /// line). Empty when the domain has none — or when it is not defined at
    /// all, which is why the callers decide FIRST whether libvirt is the right
    /// place to ask.
    fn live_snapshots(uri: &str, name: &str) -> Vec<String> {
        capture(
            "virsh",
            &["-c", uri, "snapshot-list", "--domain", name, "--name"],
        )
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
    }

    /// Runs a snapshot verb against a STOPPED VM, whose libvirt domain does
    /// not exist (this engine's `stop` undefines it). Returns whether the VM is
    /// RUNNING when it is done.
    ///
    /// Defines the domain again from the XML the last `boot` wrote — the same
    /// file libvirt itself had, DAC seclabel included, rather than re-deriving
    /// a description that would then have to agree with `boot` by hand — hands
    /// back the preserved metadata, runs `op`, and puts the host back as it
    /// found it: the metadata is dumped again (it now includes whatever `op`
    /// created) and the domain undefined.
    ///
    /// The one case where the domain is deliberately LEFT defined is a revert
    /// to a checkpoint taken while the VM ran: `snapshot-revert` restores the
    /// memory state, so the guest is running — undefining it from under a
    /// running VM is not a cleanup, it is a kill.
    fn with_stopped_domain(
        &self,
        vmdir: &Path,
        vm: &Vm,
        op: &dyn Fn(&str) -> Result<()>,
    ) -> Result<bool> {
        let xml = vmdir.join(format!("{}.xml", vm.name));
        if !xml.exists() {
            return Err(Error::Runtime {
                context: "vm",
                message: format!(
                    "VM '{}' is stopped and its libvirt domain description is not on disk ({}) \
                     — start it once (`delonix vm start {}`) and it stays there from then on",
                    vm.name,
                    xml.display(),
                    vm.name
                ),
            });
        }
        // The connection `boot` would pick: `vm.tap` holds the EFFECTIVE net
        // mode of the last boot (see the `tap: cfg.net_mode…` assignment), and
        // nat/bridge live only on the system connection.
        let uri = libvirt_uri_for(Some(&vm.tap));
        run_quiet("virsh", &["-c", uri, "define", &xml.to_string_lossy()])?;
        Self::redefine_preserved_snapshots(uri, &vm.name, vmdir);
        let done = op(uri);
        // Runs whether `op` failed or not: what it managed to create still has
        // to survive the undefine below, and the error to report is `op`'s.
        let preserved = self.preserve_snapshots(vmdir, vm);
        let running = self.is_running_uri(uri, &vm.name);
        if !running {
            let _ = libvirt_cleanup(&vm.name);
        }
        done.and(preserved.map(|_| running))
    }

    /// Gives libvirt back the snapshots that the previous `stop` preserved,
    /// for a domain that was just re-defined. The snapshot DATA never left the
    /// qcow2 that this VM reuses; this restores the bookkeeping that points at
    /// it.
    ///
    /// **Best effort, but never silent.** A snapshot that cannot be redefined
    /// is a snapshot the operator thinks they have — refusing to boot the VM
    /// over it would be worse (the VM is the thing they asked for), so it warns
    /// and names both the snapshot and the file it is still in.
    fn redefine_preserved_snapshots(uri: &str, name: &str, vmdir: &Path) {
        let names = preserved_snapshot_names(vmdir, name);
        if names.is_empty() {
            return;
        }
        let Some(uuid) = capture("virsh", &["-c", uri, "domuuid", "--", name]) else {
            eprintln!(
                "warning: VM '{name}': could not read the domain uuid — the {} preserved \
                 snapshot(s) stay in {} and are NOT known to libvirt yet",
                names.len(),
                snapshot_meta_dir(vmdir, name).display()
            );
            return;
        };
        for n in &names {
            let path = snapshot_meta_dir(vmdir, name).join(format!("{n}.xml"));
            let redefined = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|xml| {
                    let patched = snapshot_xml_with_uuid(&xml, uuid.trim());
                    // Written next to the original: the redefine reads a FILE,
                    // and this one carries the current domain's uuid.
                    let tmp = path.with_extension("xml.redefine");
                    delonix_runtime_core::write_atomic(&tmp, patched.as_bytes())
                        .map_err(|e| e.to_string())?;
                    let out = quiet(
                        "virsh",
                        &[
                            "-c",
                            uri,
                            "snapshot-create",
                            "--domain",
                            name,
                            "--redefine",
                            "--xmlfile",
                            &tmp.to_string_lossy(),
                        ],
                    );
                    let _ = std::fs::remove_file(&tmp);
                    out.map(|_| ())
                });
            if let Err(e) = redefined {
                eprintln!(
                    "warning: VM '{name}': snapshot '{n}' could not be given back to libvirt \
                     ({e}) — its state is still in the disk and its metadata in {}",
                    path.display()
                );
            }
        }
    }

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
///
/// Backend precedence when `cfg.backend` is `None`: `DELONIX_VM_BACKEND` env
/// var, then [`get_default_backend`] (persisted, [`set_default_backend`]),
/// then the capability heuristic (volumes ⇒ libvirt; cloud image without a
/// kernel ⇒ libvirt if available). Lives here (not just in the CLI) so every
/// consumer of this API — `stack apply`/`cluster kubeadm` included — inherits
/// it for free.
/// Resolves `cfg.disk` on THIS filesystem and builds the VM's thin qcow2
/// overlay from it. Extracted from `create_with` so a backend that owns its
/// storage can skip the whole thing (`manages_own_storage`) instead of the
/// engine doing local disk work for a hypervisor on another machine.
fn prepare_local_overlay(
    vmdir: &Path,
    cfg: &VmConfig,
    on: &dyn Fn(CreateStage),
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
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
    Ok((disk_path, overlay))
}

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
            // Resolved ONCE. It used to be built twice, which is free for a
            // local backend and a second authentication for a remote one.
            let b = backend_for(ex)?;
            if b.is_running(ex) {
                return Ok(ex.clone()); // already running — idempotent
            }
            b
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
            //
            // Precedence for "no opinion from the caller" (`cfg.backend` is
            // `None`): `DELONIX_VM_BACKEND` (session-wide), then the
            // persisted default (`set_default_backend`, machine-wide), then
            // the capability heuristic below. Both env/persisted act exactly
            // like an explicit `cfg.backend` — including bypassing the
            // heuristic and its warning — because they ARE an explicit
            // choice, just made once instead of per-command; a backend
            // requested this way that can't actually boot the VM (e.g. the
            // volumes/9p case above) still fails loud at boot, never silently.
            let standing_choice = std::env::var("DELONIX_VM_BACKEND")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| get_default_backend(base));
            let want = match cfg.backend.as_deref().or(standing_choice.as_deref()) {
                Some(b) => Some(b.to_string()),
                None if !cfg.volumes.is_empty() => Some("libvirt".to_string()),
                None if cfg.kernel.is_none() && LibvirtBackend.available() => {
                    Some("libvirt".to_string())
                }
                None if cfg.kernel.is_none() => {
                    eprintln!(
                        "warning: booting a cloud image on Cloud Hypervisor (libvirt not found) —                          if it panics on 'unable to mount root fs', install libvirt+qemu"
                    );
                    None
                }
                None => None,
            };
            select_backend(want.as_deref())?
        }
    };

    // Admission: refuses to boot if there is no RAM on the host (anti-overcommit).
    // Only the VMs that will REALLY boot (not the idempotent already-running one above).
    vm_admission_check(cfg)?;

    // Namespace isolation is enforceable only where the VM is on OUR dataplane.
    // Refuse rather than accept-and-ignore — see `vm_namespace_supported`.
    let ns = vm_namespace_of(cfg);
    if ns != "default" && !vm_namespace_supported(backend.id()) {
        return Err(Error::Invalid(format!(
            "namespace '{ns}' is not enforceable on the '{}' backend: its VMs live on the host's \
             libvirt bridge, outside the Delonix SDN, so nothing here can isolate them. Use \
             `--backend cloud-hypervisor` (its VMs share the containers' SDN), or drop \
             `--namespace`",
            backend.id()
        )));
    }

    // A backend that owns its storage gets `cfg.disk` verbatim and nothing is
    // prepared here: the local canonicalize would fail on an image that lives
    // on the remote node, before the backend was ever asked (ADR-0008).
    // `disk_path` is what the record keeps as the VM's base image; `overlay` is
    // what `boot` is handed. For a backend that owns its storage they are the
    // same string the caller wrote — this engine does not get to reinterpret a
    // name that means something on the far node.
    let own_storage = backend.manages_own_storage();
    let (disk_path, overlay) = if own_storage {
        (
            std::path::PathBuf::from(&cfg.disk),
            std::path::PathBuf::from(&cfg.disk),
        )
    } else {
        prepare_local_overlay(&vmdir, cfg, on)?
    };

    // An EXISTING, stopped VM gets a chance to be resumed before anything is
    // created. Both local backends answer `None` here (their `boot` is already
    // idempotent — it reuses the per-VM overlay on this filesystem), so this is
    // invisible to them. A remote backend's `boot` asks the node for the next
    // free id, so without this a `vm start` built a SECOND VM and orphaned the
    // first, with the record rewritten to the new handle and nothing left
    // pointing at the old one.
    let resumed = match &restarting {
        Some(ex) => backend.resume(&vmdir, ex)?,
        None => None,
    };

    let boot = match resumed {
        Some(b) => b,
        None => match backend.boot(&vmdir, cfg, &overlay.to_string_lossy(), on) {
            Ok(b) => b,
            Err(e) => {
                // Clean up the overlay only when WE made it. With
                // `manages_own_storage`, `overlay` IS `cfg.disk` verbatim — the name
                // the caller wrote for something on the far node — and removing it
                // means this engine deleting a file it did not create. For today's
                // Proxmox backend that name is `local-lvm:8` and the unlink simply
                // fails, but the rule cannot rest on the spelling a backend happens
                // to use: a remote backend whose disk reference IS a local path
                // would lose the user's base image on a failed boot.
                if restarting.is_none() && !own_storage {
                    let _ = std::fs::remove_file(&overlay);
                }
                return Err(e);
            }
        },
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
    vm.namespace = ns.clone();
    vm.ip = boot.ip;
    vm.backend = backend.id().to_string();
    vm.devices = cfg.devices.clone();
    vm.boot = boot_spec_of(cfg);
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
            // `destroy`, not `stop`: the record is going away, so whatever the
            // backend still owns has to go with it. They are the same call for
            // the local backends (the default), and deliberately not for a
            // remote one, whose disk lives on the node.
            if let Err(e) = backend_for(&vm).and_then(|b| b.destroy(&vmdir, &vm)) {
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
            // No record, so no address to trust — the tap goes, the firewall
            // teardown is skipped rather than guessed at.
            infra::vm_detach(name, None);
            orphan
        }
    };
    // CHECK BEFORE DELETING. This block used to run BEFORE the `existed` test,
    // so `vm rm <name>` on a VM that has no record and no libvirt domain deleted
    // `<name>.qcow2` and the whole seed directory and THEN returned "no such VM"
    // with a non-zero exit — an operator reading that error reasonably concludes
    // nothing happened, while a multi-gigabyte disk image has just been removed.
    // Reproduced live: a stray `auditghost.qcow2` was destroyed by a command that
    // reported failure. Same shape as the volume `rm` that unlinked its metadata
    // before failing: destroy nothing until we know the object is really ours to
    // destroy.
    if !existed {
        // Neither a local record nor a domain in libvirt — the `st.remove` below is
        // idempotent (absence is not an error) and would say Ok; an `rm` of something that
        // does not exist should say so, like docker.
        return Err(Error::VmNotFound(name.to_string()));
    }
    for ext in ["qcow2", "sock", "sock.lock", "serial", "log", "pid", "xml"] {
        let _ = std::fs::remove_file(vmdir.join(format!("{name}.{ext}")));
    }
    // The cloud-init seed directory (`vms/<name>/`, from `generate_seed_iso`)
    // also belongs to the VM — it was left behind and accumulated junk per name.
    let _ = std::fs::remove_dir_all(vmdir.join(name));
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
    let backend = backend_for(&vm)?;
    // BEFORE the stop, and its failure aborts the stop: on libvirt the stop
    // undefines the domain, and the undefine deletes the snapshot metadata.
    // `remove` deliberately does NOT come through here — there the whole
    // per-VM directory goes anyway.
    backend.preserve_snapshots(&vmdir, &vm)?;
    backend.stop(&vmdir, &vm)?;
    vm.status = Status::Stopped;
    vm.pid = None;
    vm.started_unix = None;
    st.save(name, &vm)
}

/// Loads a VM record, mapping the shared `NotFound` to the VM-specific
/// `VmNotFound` ("no such VM: …") — same idiom as `stop`/`status`.
fn load_vm(base: &Path, name: &str) -> Result<Vm> {
    store(base)?.load(name).map_err(|e| match e {
        Error::NotFound(_) => Error::VmNotFound(name.to_string()),
        e => e,
    })
}

/// Takes a named snapshot of VM `name` (see [`VmBackend::snapshot`]). On libvirt a
/// running VM's snapshot is a system checkpoint (memory + disk).
pub fn snapshot(base: &Path, name: &str, snap: &str) -> Result<()> {
    if !valid_vm_name(snap) {
        return Err(Error::Invalid(format!("invalid snapshot name: {snap}")));
    }
    let vmdir = vms_dir(base);
    let vm = load_vm(base, name)?;
    backend_for(&vm)?.snapshot(&vmdir, &vm, snap)
}

/// Reverts VM `name` to the named snapshot (see [`VmBackend::restore`]).
pub fn restore(base: &Path, name: &str, snap: &str) -> Result<()> {
    if !valid_vm_name(snap) {
        return Err(Error::Invalid(format!("invalid snapshot name: {snap}")));
    }
    let vmdir = vms_dir(base);
    let vm = load_vm(base, name)?;
    backend_for(&vm)?.restore(&vmdir, &vm, snap)?;
    // A revert changes what the VM IS: a checkpoint taken running brings a
    // stopped VM back up, one taken offline puts a running VM down. `status` is
    // the reconciler this engine already has, under the store lock — calling it
    // beats a second one here that could disagree with it.
    status(base, name).map(|_| ())
}

/// The `blockcommit` that puts a VM back on its own disk after a live backup.
///
/// Pure, and separate, because of what the wrong version does: a bare
/// `blockcommit --active --pivot` (no `--top`, no `--base`) commits the WHOLE
/// backing chain and pivots the guest onto the BOTTOM of it — for every VM this
/// engine creates, the shared golden image that every other VM uses as its
/// backing file. It reports `Successfully pivoted`, the PID does not change, and
/// the domain is now writing into an image other VMs read. Measured on a real
/// VM, which is how it was found. Naming top and base merges only the temporary
/// overlay, into this VM's own disk.
fn blockcommit_argv(uri: &str, name: &str, dev: &str, top: &str, base: &str) -> Vec<String> {
    [
        "-c",
        uri,
        "blockcommit",
        "--domain",
        name,
        "--path",
        dev,
        "--top",
        top,
        "--base",
        base,
        "--active",
        "--pivot",
        "--wait",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Copies a RUNNING VM's disk to `dest` without stopping it.
///
/// A VM that has to be stopped to be backed up is a VM nobody backs up, so this
/// does the same thing every hypervisor-level backup tool does, using libvirt's
/// own primitives:
///
/// 1. an **external snapshot** (`--disk-only --atomic`) redirects new writes to a
///    temporary overlay and leaves the real disk read-only and quiet;
/// 2. the now-quiet disk is copied; and
/// 3. **`blockcommit --active --pivot`** merges what the guest wrote during the
///    copy back into the real disk and puts the VM back on it.
///
/// The guest never pauses and its PID never changes.
///
/// **The temporary overlay is deleted only after the pivot succeeds.** Between
/// steps 1 and 3 that file holds every write the guest has made, so removing it
/// on the error path — the reflex, since it is "our" temp file — would destroy
/// live data. If the pivot fails, the file stays and the error says where the VM
/// is now running from.
///
/// `quiesce` asks the guest agent to flush and freeze its filesystems first,
/// which upgrades the copy from crash-consistent to filesystem-consistent. It is
/// opt-in because it FAILS on a guest without `qemu-guest-agent`, and failing a
/// backup over a guest-side package that may not be installable is the wrong
/// default.
pub fn backup_disk_live(base: &Path, name: &str, dest: &Path, quiesce: bool) -> Result<()> {
    let vm = load_vm(base, name)?;
    if vm.backend != "libvirt" {
        return Err(Error::Invalid(format!(
            "live disk backup needs the libvirt backend (this VM runs on {}); stop it first, or \
             use `delonix vm snapshot create {name} <label>`",
            vm.backend
        )));
    }
    let uri = libvirt_domain_uri(name).ok_or_else(|| Error::VmNotFound(name.to_string()))?;

    // The disk's TARGET (vda/sda), read from libvirt rather than assumed: it is
    // what `snapshot-create-as` and `blockcommit` both address, and a wrong guess
    // would act on a different disk of the same domain.
    let blklist =
        quiet("virsh", &["-c", uri, "domblklist", "--details", "--", name]).map_err(|e| {
            Error::Invalid(format!("live backup: cannot list the disks of {name}: {e}"))
        })?;
    let target = blklist
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // type device target source
            (f.len() >= 4 && f[0] == "file" && f[1] == "disk")
                .then(|| (f[2].to_string(), f[3].to_string()))
        })
        .next()
        .ok_or_else(|| {
            Error::Invalid(format!(
                "live backup: {name} has no file-backed disk to copy"
            ))
        })?;
    let (dev, source) = target;

    let tmp = PathBuf::from(format!("{source}.delonix-backup-{}", std::process::id()));
    let tmp_s = tmp.to_string_lossy().to_string();
    let snapname = format!("delonix-backup-{}", std::process::id());
    let diskspec = format!("{dev},file={tmp_s}");

    // The overlay is created HERE and handed to libvirt with `--reuse-external`,
    // instead of letting `snapshot-create-as` create it. Measured on Ubuntu: the
    // per-domain AppArmor profile (virt-aa-helper) only whitelists paths already
    // in the domain XML, so QEMU asked to create a brand-new file gets
    // `Permission denied` — even though the file would be in the user's own
    // directory and QEMU runs as that user. Pre-creating it makes the path known
    // before QEMU is asked to open it.
    let fmt = quiet("qemu-img", &["info", "--output=json", "--", &source])
        .ok()
        .and_then(|j| {
            j.split("\"format\":").nth(1).map(|t| {
                t.trim_start()
                    .trim_start_matches('"')
                    .split('"')
                    .next()
                    .unwrap_or("qcow2")
                    .to_string()
            })
        })
        .unwrap_or_else(|| "qcow2".to_string());
    quiet(
        "qemu-img",
        &[
            "create", "-q", "-f", "qcow2", "-b", &source, "-F", &fmt, "--", &tmp_s,
        ],
    )
    .map_err(|e| Error::Invalid(format!("live backup: cannot stage the overlay: {e}")))?;

    let mut args = vec![
        "-c",
        uri,
        "snapshot-create-as",
        "--domain",
        name,
        "--name",
        &snapname,
        "--disk-only",
        "--atomic",
        "--no-metadata",
        "--reuse-external",
        "--diskspec",
        &diskspec,
    ];
    if quiesce {
        args.push("--quiesce");
    }
    quiet("virsh", &args).map_err(|e| {
        Error::Invalid(format!(
            "live backup: could not snapshot {name}: {e}{}",
            if quiesce {
                " (--quiesce needs qemu-guest-agent running INSIDE the guest)"
            } else {
                ""
            }
        ))
    })?;

    // From here on the guest writes to `tmp`, and `source` is quiet. Copy it, but
    // do NOT return early on failure: the pivot has to happen either way, or the
    // VM is left running on a temporary file.
    let copied = std::fs::copy(&source, dest)
        .map_err(|e| Error::Invalid(format!("live backup: copying {source}: {e}")));

    // `--top` and `--base` are NOT optional here, and leaving them out is a
    // disaster that reports success. A bare `blockcommit --active --pivot`
    // commits the WHOLE chain and pivots the guest onto the bottom of it — which
    // for every VM this engine creates is the shared golden image that every
    // other VM uses as its backing file. Measured on a real VM: `Successfully
    // pivoted`, PID unchanged, and the domain now writing straight into
    // `vm-images/delonix-vm-base_*.qcow2`. Naming top and base merges only the
    // temporary overlay, back into this VM's own disk.
    let args = blockcommit_argv(uri, name, &dev, &tmp_s, &source);
    let pivot = quiet(
        "virsh",
        &args.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
    );

    match pivot {
        Ok(_) => {
            // Do not take "pivoted" for an answer: ASK where the domain writes.
            // This check is what turns the failure above from silent corruption
            // into a refusal, and it costs one `domblklist`.
            let now = quiet("virsh", &["-c", uri, "domblklist", "--", name]).unwrap_or_default();
            if !now.contains(source.as_str()) {
                return Err(Error::Invalid(format!(
                    "live backup: {name} pivoted onto the WRONG disk — it should be writing to \
                     {source}. Stop it NOW (virsh -c {uri} destroy {name}) and check the chain \
                     with qemu-img info before starting it again; its backing image may be \
                     taking writes"
                )));
            }
            // Only now is `tmp` genuinely spare.
            let _ = std::fs::remove_file(&tmp);
        }
        Err(e) => {
            let _ = std::fs::remove_file(dest); // the archive would be half a story
            return Err(Error::Invalid(format!(
                "live backup: {name} could NOT be put back on its own disk ({e}). It is still \
                 running, but writing to {tmp_s}, which must not be deleted. Recover with: \
                 virsh -c {uri} blockcommit --domain {name} --path {dev} --active --pivot --wait"
            )));
        }
    }
    copied.map(|_| ())
}

/// Lists VM `name`'s snapshot names (see [`VmBackend::snapshots`]).
pub fn snapshots(base: &Path, name: &str) -> Result<Vec<String>> {
    let vm = load_vm(base, name)?;
    backend_for(&vm)?.snapshots(&vms_dir(base), &vm)
}

/// Deletes VM `name`'s snapshot `snap` (see [`VmBackend::delete_snapshot`]).
pub fn delete_snapshot(base: &Path, name: &str, snap: &str) -> Result<()> {
    if !valid_vm_name(snap) {
        return Err(Error::Invalid(format!("invalid snapshot name: {snap}")));
    }
    let vmdir = vms_dir(base);
    let vm = load_vm(base, name)?;
    backend_for(&vm)?.delete_snapshot(&vmdir, &vm, snap)
}

/// Reconstructs the subset of [`VmConfig`] reliably recoverable from a
/// persisted [`Vm`] record, for [`start`]/[`restart`]. `Vm` does NOT persist
/// everything `VmConfig` needs to boot — only what survives past the initial
/// `create`: base disk, vcpus, memory, network, restart policy, passthrough
/// devices, and (libvirt only) the net mode, smuggled into `Vm.tap` at boot
/// (see the `LibvirtBackend::boot` `tap: cfg.net_mode…` assignment). Fields
/// that only ever existed as `vm create` flags — custom kernel/initrd,
/// cloud-init seed, 9p volumes, static IP, VNC, and the advanced libvirt
/// knobs (machine/CPU model/topology/TPM/video/boot order/extra disks or
/// NICs/raw XML) — are lost once `create` returns, so they are NOT restored.
/// The half of a [`VmConfig`] that the flat [`Vm`] fields do NOT already carry,
/// ready to persist.
///
/// **Destructures `VmConfig` exhaustively on purpose.** Adding a field to
/// `VmConfig` breaks the build right here, which forces whoever adds it to
/// decide whether it has to survive a `vm start` — instead of it being
/// forgotten in silence, which is precisely how the record came to persist ten
/// fields out of thirty. Same discipline as the exhaustive `match` in
/// `cmd::exitcode::for_error`: the compiler asks the question so a human does
/// not have to remember to.
fn boot_spec_of(cfg: &VmConfig) -> VmBootSpec {
    let VmConfig {
        // Already carried by a flat `Vm` field, or derived at boot time — the
        // record round-trips these without help (see `config_from`).
        name: _,
        disk: _,
        vcpus: _,
        memory: _,
        network: _,
        namespace: _,
        restart_policy: _,
        devices: _,
        backend: _,
        net_mode: _,
        // Everything below used to exist only for the duration of `vm create`.
        kernel,
        initrd,
        firmware,
        cmdline,
        seed,
        hugepages,
        cpu_affinity,
        bridge,
        volumes,
        vnc,
        static_ip,
        machine,
        cpu_model,
        cpu_topology,
        tpm,
        video,
        boot_order,
        extra_disks,
        extra_nics,
        libvirt_xml_overlay,
        libvirt_xml,
    } = cfg;
    VmBootSpec {
        kernel: kernel.clone(),
        initrd: initrd.clone(),
        firmware: firmware.clone(),
        cmdline: cmdline.clone(),
        seed: seed.clone(),
        hugepages: *hugepages,
        cpu_affinity: cpu_affinity.clone(),
        bridge: bridge.clone(),
        volumes: volumes.clone(),
        vnc: *vnc,
        static_ip: static_ip.clone(),
        machine: machine.clone(),
        cpu_model: cpu_model.clone(),
        cpu_topology: cpu_topology.clone(),
        tpm: *tpm,
        video: video.clone(),
        boot_order: boot_order.clone(),
        extra_disks: extra_disks.clone(),
        extra_nics: extra_nics.clone(),
        libvirt_xml_overlay: libvirt_xml_overlay.clone(),
        libvirt_xml: libvirt_xml.clone(),
    }
}

/// Rebuilds the `VmConfig` of an existing VM from its record — what
/// `start`/`restart` reboot with.
///
/// Written WITHOUT `..Default::default()` for the same reason `boot_spec_of`
/// destructures: the fallback was how twenty-one fields quietly became
/// defaults on every restart. Spelling every field out means a new one cannot
/// be silently dropped here either.
fn config_from(vm: &Vm) -> VmConfig {
    let b = &vm.boot;
    VmConfig {
        name: vm.name.clone(),
        disk: vm.disk.clone(),
        vcpus: vm.vcpus,
        memory: vm.memory.clone(),
        network: vm.network.clone(),
        namespace: Some(vm.namespace.clone()),
        restart_policy: vm.restart_policy.clone(),
        devices: vm.devices.clone(),
        backend: Some(vm.backend.clone()),
        // For libvirt, `Vm.tap` is not a real tap: `LibvirtBackend::boot` stores
        // the net mode string there. For Cloud Hypervisor it IS a device name
        // and must not be misread as one.
        net_mode: (vm.backend == "libvirt").then(|| vm.tap.clone()),
        kernel: b.kernel.clone(),
        initrd: b.initrd.clone(),
        firmware: b.firmware.clone(),
        cmdline: b.cmdline.clone(),
        seed: b.seed.clone(),
        hugepages: b.hugepages,
        cpu_affinity: b.cpu_affinity.clone(),
        bridge: b.bridge.clone(),
        volumes: b.volumes.clone(),
        vnc: b.vnc,
        static_ip: b.static_ip.clone(),
        machine: b.machine.clone(),
        cpu_model: b.cpu_model.clone(),
        cpu_topology: b.cpu_topology.clone(),
        tpm: b.tpm,
        video: b.video.clone(),
        boot_order: b.boot_order.clone(),
        extra_disks: b.extra_disks.clone(),
        extra_nics: b.extra_nics.clone(),
        libvirt_xml_overlay: b.libvirt_xml_overlay.clone(),
        libvirt_xml: b.libvirt_xml.clone(),
    }
}

/// Starts an existing, stopped VM — idempotent (already running = no-op,
/// same as `create`'s auto-heal, which this delegates to). Reboots reusing
/// the SAME per-VM overlay (disk state preserved) with the base
/// disk/vcpus/memory/network/backend recorded at its last `create`/`start`,
/// PLUS the boot shape ([`VmBootSpec`]: kernel/seed/volumes/static IP/VNC/TPM/
/// CPU topology/extra disks and NICs/…). Until that block was persisted this
/// rebooted a materially different machine and said nothing — see
/// [`VmBootSpec`] for the measurement.
///
/// The one thing still not recovered is a VM whose record predates the block:
/// `boot` is empty there, and empty means *unknown*, not *none*. Such a VM
/// keeps its old behaviour until the next `vm create` (idempotent) stamps the
/// real shape.
pub fn start(base: &Path, name: &str) -> Result<Vm> {
    let st = store(base)?;
    let vm = st.load(name).map_err(|e| match e {
        Error::NotFound(n) => Error::VmNotFound(n),
        e => e,
    })?;
    create(base, &config_from(&vm))
}

/// Stops (if running) then starts — always a real reboot, unlike `start`
/// (which no-ops when already running). Same recovered-fields caveat as
/// `start`/[`config_from`].
pub fn restart(base: &Path, name: &str) -> Result<Vm> {
    let st = store(base)?;
    let vm = st.load(name).map_err(|e| match e {
        Error::NotFound(n) => Error::VmNotFound(n),
        e => e,
    })?;
    if backend_for(&vm)?.is_running(&vm) {
        stop(base, name)?;
    }
    create(base, &config_from(&vm))
}

/// Current state of a VM, with `status`/`ip` reconciled by its backend.
pub fn status(base: &Path, name: &str) -> Result<Vm> {
    let st = store(base)?;
    // load() first just to resolve the NotFound->VmNotFound mapping before
    // taking the lock (update() would otherwise surface the generic NotFound).
    st.load(name).map_err(|e| match e {
        Error::NotFound(n) => Error::VmNotFound(n),
        e => e,
    })?;
    // Everything from the backend query to the decision runs INSIDE the
    // locked read-modify-write (`JsonStore::update`) — this used to be a bare
    // load->mutate->save with no lock, racing the background metrics refresh
    // (dash/delonix-mgmt) against a concurrent `vm start/stop/create` on the
    // same VM: a narrow but real lost-update window on the IP/status field.
    // The backend is resolved BEFORE the lock: the closure returns `bool`
    // (changed / unchanged) and has nowhere to put an error, and a record this
    // build cannot resolve is not something to discover halfway through a
    // read-modify-write. `load()` above already read the record, so this costs
    // nothing extra.
    let named = st.load(name)?;
    let backend = backend_for(&named)?;
    st.update(name, |vm| {
        let old_ip = vm.ip.clone();
        let was_running = vm.status == Status::Running;
        if backend.is_running(vm) {
            vm.status = Status::Running;
            vm.ip = backend.ip(vm).or_else(|| vm.ip.clone());
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
        // the name never resolves. Only writes when something actually changed.
        vm.ip != old_ip || was_running != (vm.status == Status::Running)
    })
}

/// Does this VM's recorded IP come from a PREDICTION rather than an
/// observation? See [`VmBackend::ip_is_predicted`].
///
/// `false` for a record whose backend this build cannot resolve: the caller is
/// a boot wait, and the useful default there is the one that does not go and
/// probe an address nobody can vouch for.
pub fn ip_is_predicted(vm: &Vm) -> bool {
    backend_for(vm)
        .map(|b| b.ip_is_predicted())
        .unwrap_or(false)
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
    fn o_edk2_vem_antes_do_hypervisor_fw_na_procura_de_firmware() {
        // The order IS the fix. A host that has both (the installer fetches
        // both) and picks `hypervisor-fw` boots none of this project's images
        // in Cloud Hypervisor — measured, see DEFAULT_CH_FIRMWARES. The failure
        // is also the quietest kind: the VMM process runs, the record says
        // Running, and the guest never executed an instruction.
        let edk2 = DEFAULT_CH_FIRMWARES
            .iter()
            .position(|p| p.ends_with("CLOUDHV.fd"))
            .expect("the EDK2 firmware must be in the search path");
        let rhf = DEFAULT_CH_FIRMWARES
            .iter()
            .position(|p| p.ends_with("hypervisor-fw"))
            .expect("rust-hypervisor-fw stays as a fallback");
        assert!(
            edk2 < rhf,
            "EDK2 must be preferred: {DEFAULT_CH_FIRMWARES:?}"
        );
    }

    #[test]
    fn so_o_cloud_hypervisor_preve_o_ip_em_vez_de_o_observar() {
        // The whole point of the flag, and the reason it is checked here rather
        // than trusted: a backend that predicts an address without declaring it
        // makes `vm create --wait` announce "is up" in 60ms over a guest that
        // may never boot (MEASURED, 2026-08-12, on an image whose firmware
        // fails before the kernel). libvirt reads a real DHCP lease, so there
        // an address IS evidence; Cloud Hypervisor computes one from the MAC
        // before the guest runs, so there it is evidence of nothing.
        assert!(
            CloudHypervisorBackend.ip_is_predicted(),
            "CH computes the lease from the MAC — it must say so"
        );
        assert!(
            !LibvirtBackend.ip_is_predicted(),
            "libvirt observes a real lease — declaring it predicted would send `--wait` probing for no reason"
        );
    }

    #[test]
    fn o_blockcommit_nomeia_sempre_o_topo_e_a_base() {
        // MEASURED disaster, not a hypothetical. Without `--top`/`--base`, virsh
        // committed the whole chain and left a live VM writing straight into
        // `vm-images/delonix-vm-base_debian-bookworm.qcow2` — the image every
        // other VM on the node uses as its backing file. It printed
        // "Successfully pivoted" and the guest PID never changed.
        let a = blockcommit_argv(
            "qemu:///system",
            "dev",
            "vda",
            "/vms/dev.qcow2.delonix-backup-1",
            "/vms/dev.qcow2",
        );
        let top = a
            .iter()
            .position(|x| x == "--top")
            .expect("--top is not optional");
        let base = a
            .iter()
            .position(|x| x == "--base")
            .expect("--base is not optional");
        assert_eq!(a[top + 1], "/vms/dev.qcow2.delonix-backup-1");
        // The base is the VM's OWN disk: that is what stops the commit from
        // reaching the shared golden image underneath it.
        assert_eq!(a[base + 1], "/vms/dev.qcow2");
        assert!(a.contains(&"--active".to_string()) && a.contains(&"--pivot".to_string()));
        // `--wait` too: without it virsh returns while the job is still running
        // and the caller would delete the overlay out from under it.
        assert!(a.contains(&"--wait".to_string()));
    }

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
    fn libvirt_snapshot_argv_uses_flags_not_positional() {
        // Names go via --domain/--name (flags), never positional — so a
        // (validated) name can't be read as an option, and a reorder is caught.
        let a = super::libvirt_snapshot_argv("qemu:///system", "dev", "before-upgrade");
        assert_eq!(
            a,
            vec![
                "-c",
                "qemu:///system",
                "snapshot-create-as",
                "--domain",
                "dev",
                "--name",
                "before-upgrade",
                "--atomic",
            ]
        );
        let r = super::libvirt_revert_argv("qemu:///session", "dev", "before-upgrade");
        assert_eq!(
            r,
            vec![
                "-c",
                "qemu:///session",
                "snapshot-revert",
                "--domain",
                "dev",
                "--snapshotname",
                "before-upgrade",
            ]
        );
    }

    #[test]
    fn o_uuid_do_snapshot_e_reescrito_em_todas_as_ocorrencias() {
        // `snapshot-create --redefine` REFUSES an XML whose domain uuid is not
        // the CURRENT one, and the uuid of a re-defined domain is new every
        // time. The dumped XML carries it more than once (the snapshot and the
        // embedded <domain>), so replacing only the first one gets the file
        // refused for the occurrence left behind — measured live before this
        // was written as a loop.
        let xml = "<domainsnapshot>\n  <name>s1</name>\n  <domain>\n    <uuid>old-1</uuid>\n    \
                   <memory>x</memory>\n  </domain>\n  <uuid>old-2</uuid>\n</domainsnapshot>\n";
        let out = super::snapshot_xml_with_uuid(xml, "new-uuid");
        assert_eq!(out.matches("<uuid>new-uuid</uuid>").count(), 2, "{out}");
        assert!(!out.contains("old-"), "{out}");
        assert!(out.contains("<name>s1</name>"), "{out}");
        assert!(out.contains("<memory>x</memory>"), "{out}");
        // Nothing to replace = byte-for-byte the same file.
        assert_eq!(super::snapshot_xml_with_uuid("<a/>", "u"), "<a/>");
    }

    #[test]
    fn le_a_lista_de_snapshots_do_qemu_img_info() {
        // Output REAL capturado neste host (`qemu-img info -U` de um overlay de
        // uma VM CH a correr) — não a forma do manual. O bloco acaba na secção
        // seguinte, e a linha de cabeçalho não é um snapshot chamado "TAG".
        let out = "\
image: /x/dev.qcow2
file format: qcow2
Snapshot list:
ID        TAG               VM SIZE                DATE     VM CLOCK     ICOUNT
1         manual1               0 B 2026-08-12 16:44:30 00:00:00.000          0
2         antes-do-upgrade    2 MiB 2026-08-12 16:45:00 00:00:09.012
Format specific information:
    compat: 1.1
";
        assert_eq!(
            super::parse_qemu_snapshot_list(out),
            vec!["manual1".to_string(), "antes-do-upgrade".to_string()]
        );
        // Sem snapshots não há bloco nenhum — lista vazia, nunca um erro.
        assert!(super::parse_qemu_snapshot_list("image: /x\nfile format: qcow2\n").is_empty());
    }

    #[test]
    fn os_snapshots_preservados_moram_onde_o_rm_ja_apaga() {
        // The preserved metadata MUST live under the per-VM directory that
        // `remove` deletes wholesale (`remove_dir_all(vmdir/<name>)`). Anywhere
        // else and a `vm rm` leaves metadata behind pointing at a disk that no
        // longer exists — and the next VM with the same name inherits it.
        let vmdir = std::path::Path::new("/state/vms");
        let dir = super::snapshot_meta_dir(vmdir, "dev");
        assert!(
            dir.starts_with(vmdir.join("dev")),
            "{} is outside the directory that `rm` deletes",
            dir.display()
        );
    }

    #[test]
    fn a_lista_preservada_le_so_xml_e_ordena() {
        let tmp = std::env::temp_dir().join(format!("dlx-snapmeta-{}", std::process::id()));
        let dir = super::snapshot_meta_dir(&tmp, "dev");
        // No directory at all is the normal case (a VM that was never stopped,
        // or never had a snapshot) — not an error, and never a panic.
        assert!(super::preserved_snapshot_names(&tmp, "dev").is_empty());
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["s2.xml", "s1.xml", "s1.xml.redefine", "notes.txt"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        assert_eq!(
            super::preserved_snapshot_names(&tmp, "dev"),
            vec!["s1".to_string(), "s2".to_string()]
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn unsupported_snapshot_names_the_backend_and_op() {
        let e = super::unsupported_snapshot("cloud-hypervisor", "restore").to_string();
        assert!(e.contains("restore"), "{e}");
        assert!(e.contains("cloud-hypervisor"), "{e}");
        assert!(e.contains("libvirt"), "{e}");
    }

    /// REGRESSION: toda a ferramenta cujo OUTPUT este crate parseia tem de
    /// correr com locale fixo.
    ///
    /// `virsh` é um programa gettext (confirmado neste host: exporta
    /// `bindtextdomain`/`dcgettext` e carrega `"shut off"` como msgid
    /// traduzível), e o crate decide se um domínio está vivo comparando
    /// `virsh domstate` com literais ingleses. Num host com os catálogos
    /// instalados e `LANG=pt_PT`, uma VM a correr passa a reportar-se como
    /// parada. Tirar o `.env("LC_ALL", "C")` do `stable_cmd` faz este teste
    /// falhar.
    #[test]
    fn stable_cmd_fixa_o_locale_para_o_output_ser_estavel() {
        let cmd = super::stable_cmd("virsh");
        let envs: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            envs.get("LC_ALL").and_then(|v| v.as_deref()),
            Some("C"),
            "sem LC_ALL=C o `virsh domstate` responde traduzido e a comparação com \
             \"running\"/\"shut off\" falha em silêncio"
        );
        assert_eq!(envs.get("LANG").and_then(|v| v.as_deref()), Some("C"));
    }

    /// E as comparações de estado que dependem disso continuam a ser feitas
    /// contra as formas EN — se alguém as mudar, o `stable_cmd` deixa de as
    /// proteger e o teste acima passa a estar a guardar a coisa errada.
    #[test]
    fn os_estados_comparados_sao_os_literais_en_do_virsh() {
        let src = include_str!("lib.rs");
        assert!(
            src.contains(r#"state == "shut off""#),
            "a comparação de estado mudou de forma — reavaliar se o stable_cmd ainda a cobre"
        );
        assert!(
            src.contains(r#"s == "running""#),
            "a comparação de liveness mudou de forma — idem"
        );
    }

    /// REGRESSION (protecção do host): o XML tem de levar um tecto que o HOST
    /// impõe, não só a alocação que o guest vê.
    ///
    /// `<memory>` dimensiona a visão do guest; o RSS real do QEMU é isso MAIS
    /// device models, buffers de vídeo/migração e o heap dele. Sem
    /// `<memtune><hard_limit>` uma fuga leva o host — a mesma falha que o
    /// caminho de container fecha com `memory.max`. E `<vcpu>N` limita as
    /// threads de vCPU a N cores, mas as threads de emulador/IO do QEMU correm
    /// fora dessa conta e ficavam sem tecto nenhum.
    #[test]
    fn o_dominio_leva_tecto_de_memoria_e_de_cpu_imposto_pelo_host() {
        let cfg = VmConfig {
            name: "v".into(),
            vcpus: 4,
            memory: "2G".into(),
            ..Default::default()
        };
        let xml = super::libvirt_domain_xml(&cfg, "/x.qcow2", "52:54:00:aa:bb:cc");

        // 2 GiB de guest = 2097152 KiB; margem de 25% = 524288, abaixo do mínimo
        // de 1 GiB, por isso vale o mínimo → 2097152 + 1048576 = 3145728.
        assert!(
            xml.contains("<hard_limit unit='KiB'>3145728</hard_limit>"),
            "faltou o tecto de memória imposto pelo host:\n{xml}"
        );
        // O tecto TEM de ficar acima da RAM do guest, senão o host mata a VM.
        assert!(
            xml.contains("<memory unit='KiB'>2097152</memory>"),
            "a alocação do guest não pode ter mudado"
        );
        // 4 vCPUs + 1 core de folga para emulador/IO = 5 × 100000.
        assert!(
            xml.contains("<period>100000</period>") && xml.contains("<quota>500000</quota>"),
            "faltou o tecto de CPU do domínio inteiro:\n{xml}"
        );
    }

    /// O tecto de CPU tem de conviver com o pinning de vCPUs — os dois vivem no
    /// MESMO `<cputune>`, e emitir dois blocos produziria XML que o libvirt
    /// recusa.
    #[test]
    fn cputune_junta_quota_e_pinning_num_so_bloco() {
        let cfg = VmConfig {
            name: "v".into(),
            vcpus: 2,
            memory: "1G".into(),
            cpu_affinity: Some("8-15".into()),
            ..Default::default()
        };
        let xml = super::libvirt_domain_xml(&cfg, "/x.qcow2", "52:54:00:aa:bb:cc");
        assert_eq!(
            xml.matches("<cputune>").count(),
            1,
            "só pode haver UM <cputune>:\n{xml}"
        );
        assert!(xml.contains("<quota>300000</quota>"), "{xml}");
        assert!(xml.contains("<vcpupin vcpu='0' cpuset='8-15'/>"), "{xml}");
        assert!(xml.contains("<vcpupin vcpu='1' cpuset='8-15'/>"), "{xml}");
    }

    /// As fórmulas puras, incluindo as escapatórias — um operador que meça o seu
    /// workload tem de conseguir voltar ao comportamento antigo sem editar código.
    #[test]
    fn formulas_de_tecto_e_escapatorias() {
        // margem = max(25%, 1 GiB)
        assert_eq!(
            super::mem_hard_limit_kib(1024 * 1024),
            Some(2 * 1024 * 1024)
        );
        // guest grande: os 25% ultrapassam o mínimo
        let big = 64 * 1024 * 1024; // 64 GiB em KiB
        assert_eq!(super::mem_hard_limit_kib(big), Some(big + big / 4));
        // o tecto é SEMPRE maior que o guest — o contrário mataria a VM
        for g in [1024u64, 1024 * 1024, 8 * 1024 * 1024] {
            assert!(super::mem_hard_limit_kib(g).unwrap() > g);
        }
        // quota = (vcpus + 1) cores
        assert_eq!(super::cpu_quota_micros(1), Some(200_000));
        assert_eq!(super::cpu_quota_micros(8), Some(900_000));
    }

    /// REGRESSION: o tecto de I/O por-disco das VMs — o último recurso que um
    /// guest podia esgotar no host depois de CPU e memória ficarem limitadas.
    /// Opt-in de propósito: ligar throttling de disco a VMs já existentes seria
    /// uma mudança silenciosa de desempenho num upgrade, e ao contrário da
    /// memória não há valor "generoso" seguro — depende do dispositivo.
    #[test]
    fn iotune_das_vms_e_opt_in_e_so_no_disco_raiz() {
        let cfg = VmConfig {
            name: "v".into(),
            vcpus: 1,
            memory: "1G".into(),
            seed: Some("/seed.iso".into()),
            ..Default::default()
        };
        // Sem env: nenhum <iotune> — o XML fica byte-a-byte como antes.
        assert!(
            !super::libvirt_domain_xml(&cfg, "/x.qcow2", "52:54:00:aa:bb:cc").contains("<iotune>"),
            "o iotune tem de ser opt-in"
        );

        // A função pura é o que se testa: mexer em env vars num teste paralelo
        // é uma corrida com todos os outros.
        assert_eq!(super::vm_iotune_xml(), "");
    }

    /// A composição do bloco, em cada combinação — `total_*` e não um par
    /// read/write, porque o recurso protegido é o DÉBITO do dispositivo e um
    /// guest esgota-o por qualquer das direcções.
    #[test]
    fn iotune_compoe_bytes_e_iops() {
        // Sem env vars: a composição é uma função pura, e testá-la assim é o que
        // impede a corrida que fazia o teste irmão (`iotune ... opt-in`) falhar
        // por ordem de escalonamento.
        let x = super::iotune_xml_from(Some(104_857_600), None);
        assert!(
            x.contains("<total_bytes_sec>104857600</total_bytes_sec>"),
            "{x}"
        );
        assert!(!x.contains("iops"), "{x}");

        let x = super::iotune_xml_from(Some(104_857_600), Some(2000));
        assert!(
            x.contains("<total_bytes_sec>104857600</total_bytes_sec>"),
            "{x}"
        );
        assert!(x.contains("<total_iops_sec>2000</total_iops_sec>"), "{x}");
        assert!(
            x.starts_with("      <iotune>") && x.trim_end().ends_with("</iotune>"),
            "{x}"
        );

        // Nenhum valor = nenhum tecto. O filtro de lixo/zero vive no leitor de
        // ambiente (`vm_iotune_xml`), que só sabe transformar "0"/"abc" em
        // `None` — o que esta função recebe já é o resultado disso.
        assert_eq!(super::iotune_xml_from(None, None), "");
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

    /// A name this engine knows but does not register must NOT be reported as
    /// unknown. `delonix-proxmox` is in this workspace and implements the trait;
    /// telling an operator «unknown backend, use cloud-hypervisor or libvirt»
    /// says the opposite of what is true, and sends them looking for a crate
    /// that is right there. Two assertions, because both halves matter: the
    /// name is recognised, AND it is still refused (fail-closed — nothing about
    /// this makes an unfinished backend selectable).
    #[test]
    fn um_backend_conhecido_mas_nao_registado_nao_se_reporta_como_desconhecido() {
        let e = unknown_backend("proxmox").to_string();
        assert!(
            !e.contains("unknown VM backend"),
            "o proxmox existe neste workspace — nao pode sair como desconhecido: {e}"
        );
        assert!(e.contains("not available in this build"), "{e}");
        assert!(e.contains("0008"), "a mensagem tem de apontar o ADR: {e}");
        // A mensagem tem de dizer o que FAZER, e nao so o que falta: o backend
        // pode ser configurado, e um operador que leia isto quer o passo
        // seguinte, nao um relatorio de estado.
        assert!(
            e.contains("DELONIX_PROXMOX_URL"),
            "tem de dizer como o configurar: {e}"
        );
        // E continua a NAO estar registado por omissao — quem nao configurou um
        // no nao pode seleccionar um.
        assert!(!backend_is_registered("proxmox"));
        // E um nome que nao existe mesmo continua a dizer que nao existe.
        let o = unknown_backend("naoexiste").to_string();
        assert!(o.contains("unknown VM backend"), "{o}");
        assert!(o.contains("libvirt"), "nomeia os que servem: {o}");
    }

    /// `backend_for` answers "what IS running this?", and a wrong answer does
    /// not fail — it LIES. It used to end in `_ => CloudHypervisorBackend`, so a
    /// record naming anything else got the wrong backend silently: `is_running`
    /// on a live libvirt VM would report it stopped.
    #[test]
    fn backend_for_recusa_um_registo_com_backend_desconhecido() {
        let mut vm = Vm::new(
            "db".into(),
            "/base.qcow2".into(),
            "/overlay.qcow2".into(),
            1,
            "1G".into(),
            "ingress".into(),
            "nat".into(),
            "52:54:00:aa:bb:cc".into(),
            String::new(),
        );

        for known in ["libvirt", "cloud-hypervisor", "kvm", "CH", " libvirt "] {
            vm.backend = known.into();
            assert!(
                backend_for(&vm).is_ok(),
                "'{known}' está registado e tem de resolver"
            );
        }

        // O que ANTES caía em cloud-hypervisor em silêncio.
        for unknown in ["hyperv", "proxmox", "", "cloud-hypervisr"] {
            vm.backend = unknown.into();
            let msg = match backend_for(&vm) {
                Ok(_) => panic!("'{unknown}' não está registado e resolveu na mesma"),
                Err(e) => e.to_string(),
            };
            assert!(msg.contains("db"), "a mensagem tem de nomear a VM: {msg}");
            assert!(
                msg.contains("libvirt") && msg.contains("cloud-hypervisor"),
                "e tem de dizer o que É aceite: {msg}"
            );
        }
    }

    #[test]
    fn valid_backend_name_normalizes_aliases_and_rejects_unknown() {
        assert_eq!(valid_backend_name("ch").unwrap(), "cloud-hypervisor");
        assert_eq!(
            valid_backend_name("CloudHypervisor").unwrap(),
            "cloud-hypervisor"
        );
        assert_eq!(valid_backend_name("KVM").unwrap(), "libvirt");
        assert_eq!(valid_backend_name(" libvirt ").unwrap(), "libvirt");
        assert!(valid_backend_name("hyperv").is_err());
        assert!(valid_backend_name("").is_err());
    }

    #[test]
    fn default_backend_persistence_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "delonix-vm-default-backend-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Nothing set yet.
        assert_eq!(get_default_backend(&dir), None);

        set_default_backend(&dir, "KVM").unwrap();
        assert_eq!(get_default_backend(&dir).as_deref(), Some("libvirt"));

        set_default_backend(&dir, "ch").unwrap();
        assert_eq!(
            get_default_backend(&dir).as_deref(),
            Some("cloud-hypervisor")
        );

        // Unknown name refused, previous value untouched.
        assert!(set_default_backend(&dir, "hyperv").is_err());
        assert_eq!(
            get_default_backend(&dir).as_deref(),
            Some("cloud-hypervisor")
        );

        clear_default_backend(&dir).unwrap();
        assert_eq!(get_default_backend(&dir), None);
        // Clearing an already-cleared default is not an error.
        clear_default_backend(&dir).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
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
    fn pci_addr_recusa_injeccao_de_atributos_xml() {
        // BUG regression guard: the parser used to accept ANY non-`:`/`.`/`/`
        // characters for each component, with no hex check — a manifest
        // `spec.devices` entry like `0' foo='bar:00:00.0` produced an
        // injected XML attribute in `libvirt_domain_xml`'s `<address>` tag.
        assert_eq!(parse_pci_addr("0' foo='bar:00:00.0"), None);
        assert_eq!(parse_pci_addr("a':00:00.0"), None);
        // Wrong width per component is also rejected (not just non-hex chars).
        assert_eq!(parse_pci_addr("00000:65:00.1"), None); // domain too long
        assert_eq!(parse_pci_addr("0000:6:00.1"), None); // bus too short
        assert_eq!(parse_pci_addr("0000:65:0.1"), None); // slot too short
        assert_eq!(parse_pci_addr("0000:65:00.12"), None); // func too long
        assert_eq!(parse_pci_addr("000g:65:00.1"), None); // non-hex digit
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

    #[test]
    fn config_from_recovers_libvirt_net_mode_from_the_tap_field() {
        // For libvirt, `Vm.tap` is not a real host tap — `LibvirtBackend::boot`
        // stores the net mode string there (`cfg.net_mode.unwrap_or("user")`,
        // see the assignment above). `config_from`/`start`/`restart` depend on
        // being able to read it back out the same way.
        let mut vm = Vm::new(
            "dev".into(),
            "/base.qcow2".into(),
            "/overlay.qcow2".into(),
            2,
            "2G".into(),
            "ingress".into(),
            "nat".into(),
            "52:54:00:aa:bb:cc".into(),
            String::new(),
        );
        vm.backend = "libvirt".into();
        vm.restart_policy = Some("on-failure".into());
        vm.devices = vec!["/sys/bus/pci/devices/0000:65:00.1".into()];

        let cfg = config_from(&vm);
        assert_eq!(cfg.name, "dev");
        assert_eq!(cfg.disk, "/base.qcow2");
        assert_eq!(cfg.vcpus, 2);
        assert_eq!(cfg.memory, "2G");
        assert_eq!(cfg.network, "ingress");
        assert_eq!(cfg.backend.as_deref(), Some("libvirt"));
        assert_eq!(cfg.net_mode.as_deref(), Some("nat"));
        assert_eq!(cfg.restart_policy.as_deref(), Some("on-failure"));
        assert_eq!(cfg.devices, vec!["/sys/bus/pci/devices/0000:65:00.1"]);
        // A record with an EMPTY boot block (every record written before it
        // existed) recovers nothing extra — and that is honest: empty means
        // unknown, not "this VM had none".
        assert!(cfg.kernel.is_none());
        assert!(cfg.seed.is_none());
        assert!(cfg.static_ip.is_none());
    }

    /// The whole point of persisting the boot shape: what a VM was created
    /// WITH is what it is restarted with. Before this, `vm start` rebooted a
    /// machine with no TPM, no CPU topology and no extra disks and reported
    /// success — twenty-one fields silently replaced by their defaults.
    #[test]
    fn a_forma_de_arranque_sobrevive_a_um_start() {
        let cfg = VmConfig {
            name: "dev".into(),
            disk: "/base.qcow2".into(),
            vcpus: 4,
            memory: "8G".into(),
            network: "ingress".into(),
            backend: Some("libvirt".into()),
            net_mode: Some("nat".into()),
            kernel: Some("/boot/vmlinuz".into()),
            seed: Some("/seed.iso".into()),
            hugepages: true,
            static_ip: Some("192.168.122.50".into()),
            vnc: true,
            tpm: true,
            machine: Some("q35".into()),
            cpu_model: Some("host-passthrough".into()),
            cpu_topology: Some(CpuTopology {
                sockets: 2,
                cores: 4,
                threads: 2,
            }),
            boot_order: vec!["hd".into(), "cdrom".into()],
            extra_disks: vec![ExtraDisk {
                source: "/data.qcow2".into(),
                bus: "virtio".into(),
                ..Default::default()
            }],
            extra_nics: vec![ExtraNic {
                kind: "bridge".into(),
                source: Some("br0".into()),
                ..Default::default()
            }],
            volumes: vec![VmVolume {
                tag: "dados".into(),
                source: "/srv/dados".into(),
                mount_path: "/mnt/dados".into(),
                read_only: false,
            }],
            libvirt_xml_overlay: vec!["<serial type='pty'/>".into()],
            ..Default::default()
        };

        // What `create_with` stamps on the record…
        let mut vm = Vm::new(
            cfg.name.clone(),
            cfg.disk.clone(),
            "/overlay.qcow2".into(),
            cfg.vcpus,
            cfg.memory.clone(),
            cfg.network.clone(),
            "nat".into(),
            "52:54:00:aa:bb:cc".into(),
            String::new(),
        );
        vm.backend = "libvirt".into();
        vm.boot = boot_spec_of(&cfg);

        // …has to come back out intact on the next `start`.
        let back = config_from(&vm);
        assert_eq!(back.kernel.as_deref(), Some("/boot/vmlinuz"));
        assert_eq!(back.seed.as_deref(), Some("/seed.iso"));
        assert!(back.hugepages);
        assert_eq!(back.static_ip.as_deref(), Some("192.168.122.50"));
        assert!(back.vnc);
        assert!(back.tpm);
        assert_eq!(back.machine.as_deref(), Some("q35"));
        assert_eq!(back.cpu_model.as_deref(), Some("host-passthrough"));
        assert_eq!(back.cpu_topology.as_ref().map(|t| t.cores), Some(4));
        assert_eq!(back.boot_order, vec!["hd", "cdrom"]);
        assert_eq!(back.extra_disks.len(), 1);
        assert_eq!(back.extra_nics[0].source.as_deref(), Some("br0"));
        assert_eq!(back.volumes[0].mount_path, "/mnt/dados");
        assert_eq!(back.libvirt_xml_overlay.len(), 1);
        // And the flat fields keep round-tripping as they always did.
        assert_eq!(back.net_mode.as_deref(), Some("nat"));
        assert_eq!(back.vcpus, 4);
    }

    /// The wire-compatibility half of this (a record written before the block
    /// existed must keep deserializing) lives in `delonix-runtime-core`, where
    /// `Vm` and `serde_json` both are — this crate has no JSON dependency and
    /// is not gaining one for a test.

    #[test]
    fn config_from_leaves_net_mode_none_for_cloud_hypervisor() {
        // Cloud Hypervisor's `Vm.tap` IS a real host tap device name — must
        // NOT be misread as a libvirt net mode.
        let mut vm = Vm::new(
            "ch1".into(),
            "/base.qcow2".into(),
            "/overlay.qcow2".into(),
            1,
            "1G".into(),
            "ingress".into(),
            "tap-ch1".into(),
            "52:54:00:11:22:33".into(),
            "/run/ch1.sock".into(),
        );
        vm.backend = "cloud-hypervisor".into();

        let cfg = config_from(&vm);
        assert_eq!(cfg.backend.as_deref(), Some("cloud-hypervisor"));
        assert!(cfg.net_mode.is_none());
    }

    // ---- namespace isolation for VMs ----------------------------------------

    #[test]
    fn vm_namespace_of_normaliza_ausencia_e_vazio() {
        let mut cfg = VmConfig {
            name: "v".into(),
            ..Default::default()
        };
        assert_eq!(vm_namespace_of(&cfg), "default");
        cfg.namespace = Some(String::new());
        assert_eq!(vm_namespace_of(&cfg), "default");
        cfg.namespace = Some("teamA".into());
        assert_eq!(vm_namespace_of(&cfg), "teamA");
    }

    /// libvirt VMs live on `virbr0`, in the HOST netns — a different L2 that this
    /// engine does not program. Reporting that honestly (a refusal) instead of
    /// accepting `--namespace` and doing nothing is the whole point: an isolation
    /// option that silently does nothing is worse than not having one.
    #[test]
    fn so_o_cloud_hypervisor_suporta_namespace() {
        assert!(vm_namespace_supported("cloud-hypervisor"));
        assert!(!vm_namespace_supported("libvirt"));
        assert!(!vm_namespace_supported("qualquer-outro"));
    }

    /// `start`/`restart` rebuild the `VmConfig` from the record — a namespace that
    /// did not survive that round-trip would silently drop the VM's isolation on
    /// the first restart. Exactly the family of bug this repo has already been
    /// bitten by three times (`-v` not persisted, `-p` on a custom net, extra
    /// networks lost on restart).
    #[test]
    fn config_from_preserva_a_namespace() {
        let mut vm = Vm::new(
            "v".into(),
            "d".into(),
            "o".into(),
            1,
            "1G".into(),
            "ingress".into(),
            "t".into(),
            "52:54:00:00:00:01".into(),
            "s".into(),
        );
        vm.namespace = "teamA".into();
        assert_eq!(config_from(&vm).namespace.as_deref(), Some("teamA"));
        assert_eq!(vm_namespace_of(&config_from(&vm)), "teamA");
    }

    /// A backend that exists only to be asked questions — no hypervisor, no
    /// process, nothing on disk.
    struct FakeBackend {
        id: &'static str,
        available: bool,
        own_storage: bool,
        auto: bool,
    }

    impl VmBackend for FakeBackend {
        fn id(&self) -> &'static str {
            self.id
        }
        fn available(&self) -> bool {
            self.available
        }
        fn boot(
            &self,
            _vmdir: &Path,
            _cfg: &VmConfig,
            _overlay: &str,
            _on: &dyn Fn(CreateStage),
        ) -> Result<Boot> {
            unreachable!("these tests never boot")
        }
        fn is_running(&self, _vm: &Vm) -> bool {
            false
        }
        fn ip(&self, _vm: &Vm) -> Option<String> {
            None
        }
        fn stop(&self, _vmdir: &Path, _vm: &Vm) -> Result<()> {
            Ok(())
        }
        fn manages_own_storage(&self) -> bool {
            self.own_storage
        }
        fn auto_selectable(&self) -> bool {
            self.auto
        }
    }

    #[test]
    fn os_dois_backends_de_hoje_mantem_o_comportamento_de_sempre() {
        // The defaults are what makes this addition invisible to everything
        // that already exists: both local backends prepare a local overlay and
        // both may be auto-detected, exactly as before.
        for b in [
            Box::new(CloudHypervisorBackend) as Box<dyn VmBackend>,
            Box::new(LibvirtBackend),
        ] {
            assert!(
                !b.manages_own_storage(),
                "{} must let the engine prepare the disk",
                b.id()
            );
            assert!(b.auto_selectable(), "{} must stay auto-detectable", b.id());
        }
    }

    /// A registry nobody can add to is a `match` with extra steps. This is the
    /// half of ADR-0008's decision 2 that never landed: a crate that depends on
    /// `delonix-vm` (as any backend must, for the trait) could not put itself
    /// into a `static` table here.
    ///
    /// Registration is by CLOSURE and not by `fn` pointer for one concrete
    /// reason: a remote backend needs an endpoint and a credential, and
    /// `fn() -> Box<dyn VmBackend>` has nowhere to receive them.
    #[test]
    fn um_backend_de_fora_pode_registar_se_e_passa_a_resolver_por_nome() {
        // A name no other test uses: the registry is process-wide and the test
        // harness is threaded.
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = seen.clone();
        assert!(
            select_backend(Some("fakeremote")).is_err(),
            "antes de registar nao existe"
        );
        register_backend(BackendRegistration {
            id: "fakeremote",
            aliases: &["fr"],
            auto_selectable: false,
            new: Box::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Box::new(FakeBackend {
                    id: "fakeremote",
                    available: true,
                    own_storage: true,
                    auto: false,
                }))
            }),
        })
        .expect("registar");

        // Registar NAO constroi: um no inalcancavel nao pode custar nada ate
        // alguem o escolher.
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 0);

        assert_eq!(
            select_backend(Some("fakeremote")).unwrap().id(),
            "fakeremote"
        );
        assert_eq!(select_backend(Some("FR ")).unwrap().id(), "fakeremote");
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 2);

        // E um registo que o nomeia resolve — que e o que faltava para
        // `is_running`/`stop` de uma VM criada por ele.
        let mut vm = Vm::new(
            "x".into(),
            "local-lvm:8".into(),
            "local-lvm:8".into(),
            1,
            "1G".into(),
            String::new(),
            String::new(),
            String::new(),
            "proxmox:pve:100".into(),
        );
        vm.backend = "fakeremote".into();
        assert!(backend_for(&vm).is_ok());

        // Idempotente por id: reconfigurar um alvo substitui, nunca duplica.
        register_backend(BackendRegistration {
            id: "fakeremote",
            aliases: &["fr"],
            auto_selectable: false,
            new: Box::new(|| {
                Ok(Box::new(FakeBackend {
                    id: "fakeremote",
                    available: true,
                    own_storage: true,
                    auto: false,
                }))
            }),
        })
        .expect("re-registar");
        assert_eq!(
            with_backends(|bs| bs.iter().filter(|b| b.id == "fakeremote").count()),
            1
        );
        // Limpeza: o registo e do processo inteiro.
        backends().write().unwrap().retain(|b| b.id != "fakeremote");
    }

    /// Two refusals, and each one is a name that would otherwise go missing in
    /// silence.
    #[test]
    fn o_registo_recusa_roubar_um_nome_e_recusa_auto_deteccao_de_fora() {
        // Stealing an alias would make the loser unreachable BY NAME, which is
        // the same silent failure the `_ => CloudHypervisorBackend` default was.
        let e = register_backend(BackendRegistration {
            id: "impostor",
            aliases: &["kvm"],
            auto_selectable: false,
            new: Box::new(|| Ok(Box::new(LibvirtBackend))),
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("kvm") && e.contains("libvirt"), "{e}");
        assert!(!backend_is_registered("impostor"), "nao pode ter entrado");

        // Auto-detection asks `available()`, and a backend from outside may only
        // be able to answer that over the network (ADR-0008).
        let e = register_backend(BackendRegistration {
            id: "remoto",
            aliases: &[],
            auto_selectable: true,
            new: Box::new(|| Ok(Box::new(LibvirtBackend))),
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("auto-selectable"), "{e}");
        assert!(!backend_is_registered("remoto"));
    }

    /// The order used to be `.map(build).filter(auto_selectable)`: every
    /// candidate was BUILT and the wrong ones thrown away. Free for a local
    /// backend, which is why nothing noticed — and for a remote one,
    /// construction is where authentication happens, so auto-detection made
    /// exactly the network round trip the flag exists to prevent.
    ///
    /// **Written against `auto_detect` with its own table, and the first
    /// version was not.** Registering the remote candidate in the GLOBAL
    /// registry and calling `select_backend(None)` passed with the bug still
    /// in: this host has a local backend installed, the walk stops at the first
    /// entry, and the remote one is never reached either way. A test that
    /// cannot reach the line it is about proves nothing.
    #[test]
    fn a_auto_deteccao_nao_constroi_um_backend_que_vai_descartar() {
        let built = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = built.clone();
        // The remote one FIRST, and no local backend after it that is available
        // — so a walk that builds before filtering has to touch it.
        let tabela = vec![
            BackendRegistration {
                id: "remoto",
                aliases: &[],
                auto_selectable: false,
                new: Box::new(move || {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(Box::new(FakeBackend {
                        id: "remoto",
                        available: true,
                        own_storage: true,
                        auto: false,
                    }))
                }),
            },
            BackendRegistration {
                id: "local",
                aliases: &[],
                auto_selectable: true,
                new: Box::new(|| {
                    Ok(Box::new(FakeBackend {
                        id: "local",
                        available: true,
                        own_storage: false,
                        auto: true,
                    }))
                }),
            },
        ];

        assert_eq!(
            auto_detect(&tabela).unwrap().id(),
            "local",
            "a auto-deteccao tem de escolher o local"
        );
        assert_eq!(
            built.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a auto-deteccao construiu um backend que o filtro ia descartar — \
             num backend remoto isso e uma ligacao HTTP a um no que ninguem pediu"
        );
    }

    // A test that used to live here (`a_auto_deteccao_salta_um_backend_nao_
    // auto_selecionavel`) is gone rather than kept: it re-implemented the
    // filter inside the assertion — `[remote, local].filter(auto_selectable)` —
    // so it asserted that an iterator chain written in the test does what the
    // test says. It could not have caught the ordering bug in `select_backend`
    // because it never called it. `a_auto_deteccao_nao_constroi_um_backend_que_
    // vai_descartar` above now drives the real `auto_detect`.

    /// A failed boot must not delete a file this engine did not create.
    ///
    /// With `manages_own_storage`, `overlay` IS `cfg.disk` verbatim — the name
    /// the caller wrote for something on the far node. The cleanup path removed
    /// it unconditionally. For today's Proxmox backend that name is
    /// `local-lvm:8` and the unlink simply fails, but the rule cannot rest on
    /// the spelling a backend happens to use: a remote backend whose disk
    /// reference IS a local path would lose the user's base image.
    ///
    /// **This test is only writable because the registry became populable** —
    /// the `manages_own_storage` branch of `create_with` had no registered
    /// backend that reached it, so it was never exercised at all.
    #[test]
    fn um_boot_falhado_nao_apaga_o_disco_de_um_backend_com_storage_propria() {
        struct FailingRemote;
        impl VmBackend for FailingRemote {
            fn id(&self) -> &'static str {
                "falharemoto"
            }
            fn available(&self) -> bool {
                true
            }
            fn manages_own_storage(&self) -> bool {
                true
            }
            fn auto_selectable(&self) -> bool {
                false
            }
            fn boot(
                &self,
                _: &Path,
                _: &VmConfig,
                _: &str,
                _: &dyn Fn(CreateStage),
            ) -> Result<Boot> {
                Err(Error::Invalid("the node refused".into()))
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> Result<()> {
                Ok(())
            }
        }
        register_backend(BackendRegistration {
            id: "falharemoto",
            aliases: &[],
            auto_selectable: false,
            new: Box::new(|| Ok(Box::new(FailingRemote))),
        })
        .expect("registar");

        let base = std::env::temp_dir().join(format!(
            "delonix-own-storage-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        // The victim: a real file whose NAME is what the backend was handed.
        // A remote backend is free to accept a path — this engine does not get
        // to reinterpret, nor to delete, a name that means something elsewhere.
        let vitima = base.join("imagem-base.qcow2");
        std::fs::write(&vitima, b"a imagem base do utilizador").unwrap();

        let cfg = VmConfig {
            name: "vremota".into(),
            disk: vitima.to_string_lossy().into_owned(),
            backend: Some("falharemoto".into()),
            memory: "256M".into(),
            ..Default::default()
        };
        let e = create_with(&base, &cfg, &|_| {}).unwrap_err();
        assert!(e.to_string().contains("refused"), "{e}");
        assert!(
            vitima.exists(),
            "o boot falhou e o motor apagou um ficheiro que nao criou"
        );

        let _ = std::fs::remove_dir_all(&base);
        backends()
            .write()
            .unwrap()
            .retain(|b| b.id != "falharemoto");
    }

    /// `stop` and `destroy` are the SAME call locally and NOT remotely, and
    /// conflating them destroyed data: a backend that read `stop` as "stop and
    /// destroy" made `delonix vm stop` erase the guest's disk, while the CLI's
    /// own next-steps block promises `stop it (keeps the disk)`.
    ///
    /// Two halves, and both matter: the local backends must keep the old
    /// behaviour exactly (the default), and `vm rm` must call `destroy`.
    #[test]
    fn o_rm_destroi_e_o_stop_so_para() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static STOPS: AtomicUsize = AtomicUsize::new(0);
        static DESTROYS: AtomicUsize = AtomicUsize::new(0);

        struct Counting;
        impl VmBackend for Counting {
            fn id(&self) -> &'static str {
                "contador"
            }
            fn available(&self) -> bool {
                true
            }
            fn manages_own_storage(&self) -> bool {
                true
            }
            fn auto_selectable(&self) -> bool {
                false
            }
            fn boot(
                &self,
                _: &Path,
                _: &VmConfig,
                _: &str,
                _: &dyn Fn(CreateStage),
            ) -> Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> Result<()> {
                STOPS.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn destroy(&self, _: &Path, _: &Vm) -> Result<()> {
                DESTROYS.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        register_backend(BackendRegistration {
            id: "contador",
            aliases: &[],
            auto_selectable: false,
            new: Box::new(|| Ok(Box::new(Counting))),
        })
        .expect("registar");

        let base = std::env::temp_dir().join(format!(
            "delonix-stop-destroy-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(vms_dir(&base)).unwrap();
        let st = store(&base).unwrap();
        let mut vm = Vm::new(
            "r".into(),
            "local-lvm:8".into(),
            "local-lvm:8".into(),
            1,
            "1G".into(),
            String::new(),
            String::new(),
            String::new(),
            "proxmox:pve:100".into(),
        );
        vm.backend = "contador".into();
        st.save("r", &vm).unwrap();

        stop(&base, "r").expect("stop");
        assert_eq!(STOPS.load(Ordering::SeqCst), 1);
        assert_eq!(
            DESTROYS.load(Ordering::SeqCst),
            0,
            "`vm stop` destruiu a VM — o disco de um backend remoto vai com ela"
        );

        remove(&base, "r").expect("rm");
        assert_eq!(
            DESTROYS.load(Ordering::SeqCst),
            1,
            "`vm rm` tem de libertar tudo, senao fica um orfao no no"
        );

        // The local backends must be untouched: `destroy` defaults to `stop`.
        struct OnlyStop;
        impl VmBackend for OnlyStop {
            fn id(&self) -> &'static str {
                "so-stop"
            }
            fn available(&self) -> bool {
                true
            }
            fn boot(
                &self,
                _: &Path,
                _: &VmConfig,
                _: &str,
                _: &dyn Fn(CreateStage),
            ) -> Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> Result<()> {
                STOPS.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        let before = STOPS.load(Ordering::SeqCst);
        OnlyStop.destroy(Path::new("/tmp"), &vm).unwrap();
        assert_eq!(
            STOPS.load(Ordering::SeqCst),
            before + 1,
            "sem override, destroy TEM de ser stop — e o que mantem os locais iguais"
        );

        let _ = std::fs::remove_dir_all(&base);
        backends().write().unwrap().retain(|b| b.id != "contador");
    }

    /// A `vm start` on a stopped remote VM must resume the one the record names,
    /// not build a second. Without `resume`, `boot` asked the node for the next
    /// free id and the first VM was orphaned with nothing pointing at it — and
    /// with a fresh empty disk on the new one, so the data was still there and
    /// unreachable.
    #[test]
    fn um_start_retoma_a_vm_do_registo_em_vez_de_criar_outra() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static BOOTS: AtomicUsize = AtomicUsize::new(0);
        static RESUMES: AtomicUsize = AtomicUsize::new(0);

        struct Resumable;
        impl VmBackend for Resumable {
            fn id(&self) -> &'static str {
                "retomavel"
            }
            fn available(&self) -> bool {
                true
            }
            fn manages_own_storage(&self) -> bool {
                true
            }
            fn auto_selectable(&self) -> bool {
                false
            }
            fn boot(
                &self,
                _: &Path,
                _: &VmConfig,
                _: &str,
                _: &dyn Fn(CreateStage),
            ) -> Result<Boot> {
                BOOTS.fetch_add(1, Ordering::SeqCst);
                Ok(Boot {
                    pid: None,
                    tap: String::new(),
                    mac: String::new(),
                    api_socket: "remoto:novo".into(),
                    ip: None,
                })
            }
            fn resume(&self, _: &Path, vm: &Vm) -> Result<Option<Boot>> {
                RESUMES.fetch_add(1, Ordering::SeqCst);
                Ok(Some(Boot {
                    pid: None,
                    tap: String::new(),
                    mac: String::new(),
                    api_socket: vm.api_socket.clone(),
                    ip: None,
                }))
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> Result<()> {
                Ok(())
            }
        }
        register_backend(BackendRegistration {
            id: "retomavel",
            aliases: &[],
            auto_selectable: false,
            new: Box::new(|| Ok(Box::new(Resumable))),
        })
        .expect("registar");

        let base = std::env::temp_dir().join(format!(
            "delonix-resume-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(vms_dir(&base)).unwrap();
        let st = store(&base).unwrap();
        let mut vm = Vm::new(
            "s".into(),
            "local-lvm:8".into(),
            "local-lvm:8".into(),
            1,
            "1G".into(),
            String::new(),
            String::new(),
            String::new(),
            "remoto:original".into(),
        );
        vm.backend = "retomavel".into();
        vm.status = Status::Stopped;
        st.save("s", &vm).unwrap();

        let out = start(&base, "s").expect("start");
        assert_eq!(RESUMES.load(Ordering::SeqCst), 1);
        assert_eq!(
            BOOTS.load(Ordering::SeqCst),
            0,
            "criou uma VM nova — a antiga fica orfa no no e o registo passa a apontar para a nova"
        );
        assert_eq!(
            out.api_socket, "remoto:original",
            "o registo tem de continuar a apontar para a MESMA VM"
        );

        let _ = std::fs::remove_dir_all(&base);
        backends().write().unwrap().retain(|b| b.id != "retomavel");
    }

    #[test]
    fn um_backend_remoto_recebe_o_disco_tal_como_foi_escrito() {
        // The point of `manages_own_storage`: `cfg.disk` names something on the
        // FAR node, so the engine must not canonicalize it here (it would fail
        // before the backend was asked) nor build an overlay from it.
        let remote = FakeBackend {
            id: "remote",
            available: true,
            own_storage: true,
            auto: false,
        };
        assert!(remote.manages_own_storage());
        // And a name that does not exist locally is exactly the normal case.
        assert!(
            !std::path::Path::new("local-lvm:vm-100-disk-0").exists(),
            "the test's premise is that this is not a local path"
        );
    }

    #[test]
    fn um_dominio_tem_sempre_ecra_a_nao_ser_que_o_tirem() {
        // The bug this closes: a display adapter only appeared with `--vnc`,
        // and every Proxmox appliance image — the vendor's own, untouched —
        // boots into a SeaBIOS→GRUB→reset loop with no adapter at all. So
        // `vm create` worked with the flag people use to LOOK at a guest and
        // silently produced a dead machine without it.
        let base = VmConfig {
            name: "v".into(),
            disk: "/tmp/x.qcow2".into(),
            ..Default::default()
        };
        let xml = libvirt_domain_xml(&base, "/tmp/x.qcow2", "");
        assert!(
            xml.contains("<video>"),
            "a domain with no --vnc must still have a display adapter:\n{xml}"
        );
        assert!(
            !xml.contains("<graphics"),
            "…but no VNC server, which is what --vnc is for"
        );

        // With --vnc: both, and the virtio model as before.
        let vnc = VmConfig {
            vnc: true,
            ..base.clone()
        };
        let xml = libvirt_domain_xml(&vnc, "/tmp/x.qcow2", "");
        assert!(xml.contains("<graphics type='vnc'"));
        assert!(xml.contains("<video><model type='virtio'"));

        // `video: none` still suppresses it — an explicit choice stays honoured.
        let none = VmConfig {
            video: Some("none".into()),
            ..base.clone()
        };
        assert!(!libvirt_domain_xml(&none, "/tmp/x.qcow2", "").contains("<video>"));

        // An explicit model still wins over both defaults.
        let qxl = VmConfig {
            video: Some("qxl".into()),
            ..base
        };
        assert!(libvirt_domain_xml(&qxl, "/tmp/x.qcow2", "").contains("type='qxl'"));
    }
}
