//! `delonix-vm` — microVM runtime with a **selectable backend**:
//!
//! * **Cloud Hypervisor** (Rust VMM on top of `/dev/kvm`, runs rootless INSIDE the
//!   ingress infra netns — the `tap` lives there) — the historical backend.
//! * **libvirt/KVM** (QEMU managed by `libvirtd` via `virsh`) — 2nd backend, for
//!   hosts where libvirt is already the virtualization standard.
//!
//! The backend is chosen per VM: explicit (`VmConfig.backend`) or **auto-detection**
//! (prefers `cloud-hypervisor` if installed; otherwise `libvirt`). The per-VM state
//! ([`delonix_compute::Vm`], persisted in `<base>/vms/<name>.json`) records the backend
//! that started it, in order to reconcile liveness/shutdown with the right backend.
//!
//! Networking: Cloud Hypervisor reuses the `delonix-sdn` *plumbing*
//! (`infra::vm_attach` creates a `tap` on the ingress bridge + DHCP). libvirt runs
//! QEMU under `libvirtd` (host netns), so it uses, in the MVP, **user-mode networking**
//! (SLIRP/passt: egress without a `tap`); integration with the ingress bridge (inbound
//! via the SDN) is a follow-up.

use delonix_compute::capability::Capability;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use delonix_compute::ports::VmNetwork;

/// The network a Cloud Hypervisor VM is attached through, registered once by the
/// composition root ([`set_network`]).
static NETWORK: std::sync::OnceLock<Box<dyn VmNetwork>> = std::sync::OnceLock::new();

/// Registers the node's [`VmNetwork`]. The first registration wins: the network
/// is a fact of the process, not something to swap between two VMs.
pub fn set_network(network: Box<dyn VmNetwork>) {
    let _ = NETWORK.set(network);
}

/// The registered network, or the reason there is none — a VM on the SDN cannot
/// be attached without one, and saying so beats a VM with no network.
fn network() -> Result<&'static dyn VmNetwork> {
    NETWORK
        .get()
        .map(|n| n.as_ref())
        .ok_or_else(|| Error::Command {
            context: "vm",
            message: "no VM network provider is registered in this process".into(),
        })
}
use delonix_compute::{Vm, VmBootSpec};
use delonix_model::records::Status;
use delonix_state::JsonStore;

mod error;
pub use error::{Error, Result};

/// The VM shapes that [`Vm`] persists. They are DEFINED in
/// `delonix-compute` — the record lives there and the dependency cannot
/// run the other way — and re-exported here so `delonix_vm::CpuTopology` and
/// friends keep resolving for every existing caller.
pub use delonix_compute::{CpuTopology, ExtraDisk, ExtraNic, VmVolume};

/// The `VmBackend` port and its companions moved to `delonix-compute` in P4b.2
/// (the P4b plan, `docs/discovery/61`) so a provider crate can implement
/// it without depending on this adapter. Re-exported: no caller changes.
pub use delonix_compute::vm_backend::{
    mem_mib, parse_mem_mib, BackendFactory, BackendRegistration, Boot, CloudInitIntent,
    CreateStage, DestroyStage, MoveOptions, ReportFactory, VmBackend, VmConfig,
};

pub mod capabilities;
pub mod cloudinit;
pub mod firewall;
pub mod provider;

// `VmVolume` — what connects `kind: Volume`/`kind: Storage` to a VM without the
// user writing cloud-init or XML: the bin resolves the name → `source` (the
// volume's `_data`, or a network Storage's mountpoint) and the engine generates
// both the domain's `<filesystem>` and the guest-side `mount`. Defined in
// `delonix-compute` with the other persisted shapes; re-exported above.

// ===========================================================================
// Shared helpers
// ===========================================================================

fn vms_dir(base: &Path) -> std::path::PathBuf {
    base.join("vms")
}

/// A record-store failure, as this crate's error. A free function and not a
/// `From` impl since P4b.2: both types now live in other crates, so the orphan
/// rule forbids the impl; the conversion is the one the impl used to do.
fn state_err(e: delonix_state::Error) -> Error {
    Error::Engine(e.into())
}

fn store(base: &Path) -> Result<JsonStore<Vm>> {
    JsonStore::open(vms_dir(base)).map_err(state_err)
}

// `is_alive` era uma TERCEIRA cópia da mesma pergunta (a do motor usa o
// `kill(pid, 0)`, esta lia `/proc`). Usa-se agora a do `delonix-node`,
// que é também onde vive o `safe_to_signal` que fecha a reciclagem de PID.
use delonix_node::{proc_starttime, safe_to_signal};

/// Is `argv` the `cloud-hypervisor` serving THIS VM? PURE.
///
/// The api-socket path is the ownership token: we choose it, it is unique per
/// VM, and the VMM carries it in its own argv. Same idiom the slirp reaper uses
/// — the tool's name alone identifies a TOOL, never an instance.
fn argv_is_vmm_for(argv: &[String], api_socket: &str) -> bool {
    if api_socket.is_empty() {
        return false;
    }
    argv.first()
        .is_some_and(|a| a.ends_with("cloud-hypervisor"))
        && argv.iter().any(|a| a == api_socket)
}

/// Adopts `pid_starttime` for a record written before the field existed.
///
/// **Only when the pid is PROVABLY this VM's VMM**, by a means independent of
/// the starttime itself: the process's argv has to name this VM's api-socket.
/// Stamping on liveness alone would be worse than the gap it closes — it would
/// carve a recycled pid's starttime into the record and make every later check
/// agree with it, turning a missing guard into a confidently wrong one.
///
/// Returns `true` when it stamped (the caller persists).
fn adopt_pid_starttime(vm: &mut Vm) -> bool {
    if vm.pid_starttime.is_some() {
        return false;
    }
    let Some(pid) = vm.pid.filter(|&p| p > 0) else {
        return false;
    };
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let argv: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|c| !c.is_empty())
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    if !argv_is_vmm_for(&argv, &vm.api_socket) {
        return false;
    }
    vm.pid_starttime = proc_starttime(pid);
    vm.pid_starttime.is_some()
}

/// The pid of this VM's VMM, **only when it is safe to signal it**: alive AND
/// still the same process we started.
///
/// Split out of `stop` so the recycled-pid case is covered by a test that fails
/// if the guard is dropped — a test on `safe_to_signal` alone would keep passing.
fn vmm_to_signal(vm: &Vm) -> Option<i32> {
    vm.pid.filter(|&p| safe_to_signal(p, vm.pid_starttime))
}

/// How long `stop` waits for the VMM to leave after the `SIGTERM`, before
/// escalating to `SIGKILL` — the same ten seconds `container stop` grants by
/// default, so both halves of the engine mean the same thing by "stop".
const VMM_TERM_GRACE: Duration = Duration::from_secs(10);
/// And after the `SIGKILL`. Only the kernel is left to do here, so it is
/// short; it is not zero because the exit still has to be observed.
const VMM_KILL_GRACE: Duration = Duration::from_secs(2);

/// State letter (field 3 of `/proc/<pid>/stat`) — `R`, `S`, `D`, `Z`, …
/// `None` when the process is gone or unreadable.
fn proc_state(pid: i32) -> Option<char> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm (field 2) may contain spaces and parentheses — same cut as
    // `proc_starttime`: everything after the LAST ')'.
    s[s.rfind(')')? + 1..]
        .split_whitespace()
        .next()?
        .chars()
        .next()
}

/// `true` once `pid` is no longer RUNNING: gone, or a zombie.
///
/// The zombie counts, and that is why this is not written as
/// `!safe_to_signal(...)`: `kill(pid, 0)` succeeds on a zombie, but a zombie
/// has already closed every descriptor it held — including the qcow2's, which
/// is the only thing the caller is waiting for. `boot_ch` launches the VMM
/// orphaned (it backgrounds it and the `sh` exits), so init reaps it and the
/// window is normally invisible; making the wait depend on that timing anyway
/// would trade a race for a stall.
fn vmm_left(pid: i32, starttime: Option<u64>) -> bool {
    !safe_to_signal(pid, starttime) || proc_state(pid) == Some('Z')
}

/// Polls [`vmm_left`] until it says yes or `limit` runs out.
fn wait_vmm_left(pid: i32, starttime: Option<u64>, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if vmm_left(pid, starttime) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `SIGTERM`, wait, `SIGKILL`, wait. `true` if the VMM really left.
///
/// **The wait is the point.** `SIGTERM` and an immediate `Ok(())` — which is
/// what this used to be — let `stop` return while the vmm still held the
/// qcow2's write lock, so `vm stop && vm snapshot rm` failed with `qemu-img:
/// Failed to lock byte 100` whenever the next process reached `qemu-img`
/// first. That sequence is not one a user invented: it is the one
/// `offline_snapshot_op` prints when it refuses a snapshot of a running VM.
/// Measured on this host at 2026-09-09, against a VM booting a real image.
/// The lock outlived the `SIGTERM` in **35 of 35** samples — never zero —
/// 26 ms at the median and 954 ms in the tail, against the ~70 ms `vm stop`
/// still spends after the signal on `vm_detach`. Whether that shows depends
/// on the DISK, not the CPU: with the host's disk busy, `vm stop` returned
/// with the vmm still in `R`/`S` in **14 of 25** runs and `vm stop && vm
/// snapshot rm` failed in **4 of 25**; with the disk idle, both are 0 of 25,
/// which is why the failure read as unreproducible. With this wait, both are
/// 0 of 25 under the same busy disk.
///
/// Split out of `stop` for the same reason `vmm_to_signal` was: so a test can
/// hold it to its contract without a hypervisor.
fn terminate_vmm(pid: i32, starttime: Option<u64>, grace: Duration, kill_grace: Duration) -> bool {
    // SAFETY: `pid` confirmed alive and ours by the caller's `vmm_to_signal`.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    if wait_vmm_left(pid, starttime, grace) {
        return true;
    }
    // SAFETY: same pid, and `vmm_left` says it is still the process we started.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    wait_vmm_left(pid, starttime, kill_grace)
}

/// `true` if a VM with this name already exists.
pub fn exists(base: &Path, name: &str) -> bool {
    store(base).map(|s| s.exists(name)).unwrap_or(false)
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
    admission_verdict(
        cfg,
        host_mem_available_mib(),
        std::env::var("DELONIX_VM_RESERVE_MIB").ok().as_deref(),
    )
}

/// The admission rule, with the host's available memory and the reserve passed in —
/// testable without reading `/proc/meminfo` (which made the old test depend on the
/// machine) or writing the process environment (which raced parallel tests).
fn admission_verdict(
    cfg: &VmConfig,
    available_mib: Option<u64>,
    reserve_raw: Option<&str>,
) -> Result<()> {
    let avail = match available_mib {
        Some(a) => a,
        None => return Ok(()),
    };
    let reserve = reserve_raw.and_then(|v| v.parse().ok()).unwrap_or(2048u64);
    let want = mem_mib(&cfg.memory);
    if want.saturating_add(reserve) > avail {
        return Err(Error::AdmissionRefused(format!(
            "host protection: VM '{}' asks for {want} MiB but the host only has {avail} MiB \
             available (reserve {reserve} MiB). Stop VMs/containers, reduce the memory, \
             or lower DELONIX_VM_RESERVE_MIB (at your own risk).",
            cfg.name
        )));
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
/// **Today only Cloud Hypervisor does**, and it is the backend's own report that
/// says so (`vm.namespace-isolation`, ADR-0050) — not this function comparing
/// the id against a literal, which is the provider-name matching ADR-0044 D3
/// rule 3 forbids outside a composition root. A libvirt VM lives on `virbr0`,
/// in the HOST's network namespace — a different L2 entirely, governed by
/// libvirt's own filtering, which this engine does not program. Accepting
/// `--namespace` there and quietly doing nothing would be the exact
/// anti-pattern this codebase has already had to correct three times over
/// (`--security-opt seccomp=`, `-v …:z`, `--network-alias`): an option
/// accepted, ignored, and believed. An id no registration knows is a "no".
pub fn vm_namespace_supported(backend_id: &str) -> bool {
    backend_declares(backend_id, Capability::VmNamespaceIsolation)
}

/// Whether the backend registered as `backend_id` DECLARES `cap` (ADR-0050),
/// regardless of what this host has installed: the question about a provider's
/// nature, which is what the two predicates above ask. The registration's
/// report factory probes the host (three `which`, a `/dev/kvm` stat and one
/// `virsh uri` — ~10 ms measured), and `declared_usable` puts a declared "yes"
/// the probe narrowed to `unavailable-on-host` back; a `--require` keeps
/// asking `is_usable`, because a request has to run HERE. Unknown id: `false`,
/// never a guess — a backend nobody registered cannot supervise anything.
fn backend_declares(backend_id: &str, cap: Capability) -> bool {
    with_backends(|regs| report_of(regs, backend_id))
        .map(|report| {
            report
                .capabilities
                .iter()
                .any(|c| c.capability == cap && c.state.declared_usable())
        })
        .unwrap_or(false)
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
    let h = delonix_net_rules::fnv32(name);
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

/// Extracts the virtual size IN BYTES from the `virtual size: … (N bytes)` line
/// of `qemu-img info`. Pure function (testable without `qemu-img`).
///
/// Reads the parenthesised byte count and not the human figure: `2.2 GiB` is
/// rounded, and a per-node disk quota compared against a rounded number is a
/// quota that lets through what it meant to refuse.
fn parse_qemu_virtual_size_bytes(info: &str) -> Option<u64> {
    for line in info.lines() {
        if let Some(rest) = line.trim().strip_prefix("virtual size:") {
            let inside = rest.split('(').nth(1)?;
            let digits: String = inside.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
    }
    None
}

/// Virtual size of a disk, in bytes, via `qemu-img info`.
fn disk_virtual_size_bytes(disk: &Path) -> Option<u64> {
    let out = stable_cmd("qemu-img").arg("info").arg(disk).output().ok()?;
    parse_qemu_virtual_size_bytes(&String::from_utf8_lossy(&out.stdout))
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
        .map_err(|e| Error::Command {
            context: "vm-tool",
            message: format!("{prog}: {e}"),
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim().trim_start_matches("error: ").trim();
        return Err(Error::Command {
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

/// The `(expiry, ip)` of every `virsh net-dhcp-leases` entry for `mac`
/// (case-insensitive). The expiry format (`YYYY-MM-DD HH:MM:SS`) is zero-padded
/// and lexicographically sortable, so plain string comparison orders it — no
/// date parsing, and no time zone to get wrong (every value compared comes from
/// the same `virsh` on the same host).
fn lease_entries(out: &str, mac: &str) -> Vec<(String, String)> {
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
        .collect()
}

/// Pure parser for `virsh net-dhcp-leases` output: among the entries matching
/// `mac`, returns the address of the one with the LATEST `Expiry Time`. See
/// [`LibvirtBackend::ip_from_leases`] for why this is the only reliable signal
/// (`domifaddr` can list several stale entries for the same MAC in no useful
/// order).
#[cfg(test)]
fn parse_leases_latest_ip(out: &str, mac: &str) -> Option<String> {
    match pick_lease_ip(out, mac, None) {
        LeasePick::Current(ip) => Some(ip),
        LeasePick::OnlyStale | LeasePick::NoLease => None,
    }
}

/// What the lease table says about THIS boot's address.
#[derive(Debug, PartialEq, Eq)]
enum LeasePick {
    /// A lease issued during this boot — the address to report.
    Current(String),
    /// The MAC has leases, but every one of them predates this boot. Reporting
    /// any of them is reporting a dead VM's address; the caller must NOT fall
    /// back to another source that reads the same table (`domifaddr` does).
    OnlyStale,
    /// No lease for this MAC at all.
    NoLease,
}

/// Picks this boot's address from `virsh net-dhcp-leases` output: the latest
/// lease for `mac` whose expiry is strictly after `floor` (see
/// [`leases_max_expiry`], taken before the domain started).
///
/// BUG FIXED HERE, measured 2026-09-15: the MAC is derived from the VM's name
/// ([`mac_for`]), and `delete vm` does not — cannot, through libvirt — release
/// the dnsmasq lease. A VM deleted and re-created under the same name therefore
/// found its predecessor's unexpired lease under its own MAC, and before the
/// new guest had asked for an address that lease was the ONLY one: `vm create
/// --wait` announced `ip 192.168.122.223` while the guest came up on `.224`.
/// Once the new guest leases, its expiry is always the later one (same network,
/// same lease time, issued later), so the stale entry only ever wins in that
/// window — which is exactly when the banner is printed.
///
/// A guest that renews the SAME address after a `vm stop`/`vm start` gets a new
/// expiry, above the floor, so it is still reported.
fn pick_lease_ip(out: &str, mac: &str, floor: Option<&str>) -> LeasePick {
    let entries = lease_entries(out, mac);
    if entries.is_empty() {
        return LeasePick::NoLease;
    }
    entries
        .into_iter()
        .filter(|(expiry, _)| floor.is_none_or(|f| expiry.as_str() > f))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map_or(LeasePick::OnlyStale, |(_, ip)| LeasePick::Current(ip))
}

/// The latest expiry already in the lease table for `mac` — the floor a boot
/// records before its domain starts. `None` when the MAC has no lease yet.
fn leases_max_expiry(out: &str, mac: &str) -> Option<String> {
    lease_entries(out, mac).into_iter().map(|(e, _)| e).max()
}

// ===========================================================================
// Backend trait
// ===========================================================================

fn builtin_backends() -> Vec<BackendRegistration> {
    vec![
        BackendRegistration {
            id: "cloud-hypervisor",
            aliases: &["ch", "cloudhypervisor"],
            auto_selectable: true,
            new: Box::new(|| Ok(Box::new(CloudHypervisorBackend))),
            report: Box::new(|| {
                capabilities::cloud_hypervisor_report(&capabilities::CloudHypervisorHost::probe())
            }),
        },
        BackendRegistration {
            id: "libvirt",
            aliases: &["kvm", "qemu"],
            auto_selectable: true,
            new: Box::new(|| Ok(Box::new(LibvirtBackend))),
            report: Box::new(|| capabilities::libvirt_report(&capabilities::LibvirtHost::probe())),
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

/// Every registered backend's capability report (ADR-0050), in registry
/// order, which is the order auto-detection prefers. Each report is built
/// NOW, so a local backend probes this host and a remote one declares without
/// connecting (its own `report` factory is responsible for that).
pub fn provider_reports() -> Vec<delonix_compute::capability::ProviderReport> {
    with_backends(|regs| regs.iter().map(|r| (r.report)()).collect())
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
        return Err(Error::BackendRegistrationRefused(
            "a backend registration needs an id".into(),
        ));
    }
    let builtin_ids: Vec<&str> = builtin_backends().iter().map(|b| b.id).collect();
    if reg.auto_selectable && !builtin_ids.contains(&reg.id) {
        return Err(Error::BackendRegistrationRefused(format!(
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
            return Err(Error::BackendRegistrationRefused(format!(
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
     `DELONIX_PROXMOX_SECRET`), or a `type: proxmox` entry in the node's providers file \
     (`/etc/delonix/providers.yaml`, ADR-0054) — see docs/adr/0008-proxmox-vm-backend.md",
)];

fn unknown_backend(name: &str) -> Error {
    let want = name.trim().to_lowercase();
    if let Some((_, why)) = KNOWN_UNREGISTERED.iter().find(|(id, _)| *id == want) {
        return Error::BackendNotConfigured(format!(
            "VM backend '{}' is not available in this build: {why}",
            name.trim()
        ));
    }
    Error::UnknownBackend(format!(
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
    select_backend_requiring(want, &[])
}

/// [`select_backend`] with the caller's requirements (ADR-0050 D6): a named
/// backend is refused unless its report on this host marks every entry of
/// `required` usable, and auto-detection skips a candidate that does not.
/// With `required` empty this is exactly [`select_backend`].
pub fn select_backend_requiring(
    want: Option<&str>,
    required: &[Capability],
) -> Result<Box<dyn VmBackend>> {
    match want.map(str::trim) {
        Some(other) if !other.is_empty() => {
            let b = make_backend(other).unwrap_or_else(|| Err(unknown_backend(other)))?;
            require_capabilities(b.id(), required)?;
            Ok(b)
        }
        _ => with_backends(|regs| auto_detect(regs, required)),
    }
}

/// The names in `VmConfig::required_capabilities`, resolved against the
/// catalog. **Before any backend is touched**: an unknown name is an invalid
/// argument, and reading it as "no provider supports it" would send the
/// caller looking for a provider instead of for the typo.
pub fn resolve_required_capabilities(names: &[String]) -> Result<Vec<Capability>> {
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        let n = n.trim();
        match Capability::from_name(n) {
            Some(c) => {
                if !out.contains(&c) {
                    out.push(c);
                }
            }
            None => {
                return Err(Error::UnknownCapability(format!(
                    "unknown capability '{n}': the catalog (version {}) has no entry by that \
                     name — `delonix provider ls` lists the names",
                    delonix_compute::capability::CATALOG_VERSION
                )))
            }
        }
    }
    Ok(out)
}

/// Refuses `backend_id` unless its report on THIS host marks every entry of
/// `required` usable (the contract's `Capability.supported`: `supported` or
/// `partial`). The refusal names each unmet entry with the provider's own
/// state and reason — the same words `provider describe` prints — so the
/// caller knows whether to pick another provider, install something on this
/// host, or drop the requirement.
pub fn require_capabilities(backend_id: &str, required: &[Capability]) -> Result<()> {
    if required.is_empty() {
        return Ok(());
    }
    let report = with_backends(|regs| report_of(regs, backend_id));
    let Some(report) = report else {
        return Err(unknown_backend(backend_id));
    };
    let missing = unmet(&report, required);
    if missing.is_empty() {
        return Ok(());
    }
    Err(capability_not_supported(&report.id, &missing))
}

fn report_of(
    regs: &[BackendRegistration],
    id: &str,
) -> Option<delonix_compute::capability::ProviderReport> {
    regs.iter()
        .find(|r| r.id == id || r.aliases.contains(&id))
        .map(|r| (r.report)())
}

/// `name: state — detail` for every required entry the report does not mark
/// usable. An entry the report lacks (a kind mismatch) is "not in this
/// provider's report", which is a "no" too.
fn unmet(
    report: &delonix_compute::capability::ProviderReport,
    required: &[Capability],
) -> Vec<String> {
    required
        .iter()
        .filter_map(
            |c| match report.capabilities.iter().find(|r| r.capability == *c) {
                Some(r) if r.state.is_usable() => None,
                Some(r) => {
                    let detail = r.state.detail();
                    Some(if detail.is_empty() {
                        format!("{}: {}", c.name(), r.state.label())
                    } else {
                        format!("{}: {} — {}", c.name(), r.state.label(), detail)
                    })
                }
                None => Some(format!("{}: not in this provider's report", c.name())),
            },
        )
        .collect()
}

fn capability_not_supported(id: &str, missing: &[String]) -> Error {
    Error::CapabilityNotSupported(format!(
        "the '{id}' backend does not support what this VM requires on this host:\n  {}\nPick a \
         backend that does (`delonix provider ls`), or drop the requirement",
        missing.join("\n  ")
    ))
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
///
/// With `required` non-empty (ADR-0050 D6) the walk also asks each candidate's
/// REPORT — built here, which for a local backend probes this host and for a
/// remote one declares without connecting — and skips one that does not mark
/// every required entry usable. When every available candidate is skipped that
/// way, the refusal names what each one lacks: "no backend" would send the
/// caller to install a hypervisor it already has.
fn auto_detect(
    entries: &[BackendRegistration],
    required: &[Capability],
) -> Result<Box<dyn VmBackend>> {
    let mut skipped: Vec<String> = Vec::new();
    for e in entries.iter().filter(|b| b.auto_selectable) {
        // A built-in constructor is infallible; a registered one that fails is
        // not a reason to abort a walk whose next candidate may serve fine.
        if let Ok(b) = (e.new)() {
            if b.available() {
                if required.is_empty() {
                    return Ok(b);
                }
                let missing = unmet(&(e.report)(), required);
                if missing.is_empty() {
                    return Ok(b);
                }
                skipped.push(format!("{}:\n  {}", e.id, missing.join("\n  ")));
            }
        }
    }
    if skipped.is_empty() {
        return Err(Error::NoBackendAvailable(
            "no VM backend available: install 'cloud-hypervisor' or 'libvirt'+'qemu'".into(),
        ));
    }
    Err(Error::CapabilityNotSupported(format!(
        "no available VM backend supports what this VM requires on this host:\n{}\nInstall or \
         configure one that does (`delonix provider ls`), or drop the requirement",
        skipped.join("\n")
    )))
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

/// The default provider the NODE declares, set once at startup by whoever
/// read the node's providers file (ADR-0054 D1-D3) — the composition root, not
/// this crate, which never reads a configuration file. `Ok(None)`: the file
/// names no default. `Err`: the file exists and could not be read, which makes
/// every choice that would have used it fail instead of guessing.
static CONFIGURED_DEFAULT: std::sync::OnceLock<std::result::Result<Option<String>, String>> =
    std::sync::OnceLock::new();

/// Records the node's configured default provider (ADR-0054). The first call
/// wins; a process reads its providers file once.
pub fn set_configured_default_backend(value: std::result::Result<Option<String>, String>) {
    let _ = CONFIGURED_DEFAULT.set(value.map(|o| o.map(|n| n.trim().to_lowercase())));
}

/// What [`set_configured_default_backend`] recorded, if anything — for a
/// caller that reports the default (`vm default-backend`) and has to say where
/// it came from.
pub fn configured_default_backend() -> Option<&'static std::result::Result<Option<String>, String>>
{
    CONFIGURED_DEFAULT.get()
}

/// The backend chosen ONCE instead of per-command, in the order ADR-0054 D3
/// fixes: `DELONIX_VM_BACKEND` (session-wide), then the node's configured
/// default ([`set_configured_default_backend`]), then the persisted legacy
/// default ([`set_default_backend`]). `Ok(None)` when none is set.
///
/// **A default the process cannot serve is not dropped.** The name is
/// returned as written, so selecting it fails with the reason
/// (`BackendNotConfigured` for a provider this process has no target for)
/// instead of falling through to auto-detection and creating the VM on a
/// LOCAL hypervisor — measured before this existed: a default of `proxmox`
/// read by a process without the target created locally, rc=0 (ADR-0054 §3).
///
/// Public because [`create_with`] is no longer the only place that needs the
/// answer, and two copies of a precedence rule is how they start to disagree.
/// `backend_manages_own_storage(None)` resolves by AUTO-DETECTION, so a caller
/// that asks it without threading this through gets the local backend even on a
/// machine standing-configured for Proxmox — and then goes on to prepare a
/// local overlay for a guest that will run somewhere else entirely.
pub fn standing_backend_choice(base: &Path) -> Result<Option<String>> {
    if let Some(v) = std::env::var("DELONIX_VM_BACKEND")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return Ok(Some(v));
    }
    match CONFIGURED_DEFAULT.get() {
        Some(Err(why)) => return Err(Error::BackendNotConfigured(why.clone())),
        Some(Ok(Some(name))) => return Ok(Some(name.clone())),
        _ => {}
    }
    Ok(get_default_backend(base))
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
/// A missing or unreadable file is `None`. A name this process has not
/// registered is returned AS WRITTEN, never dropped: dropping it is what made
/// a `proxmox` default fall through to a local hypervisor in a process without
/// the target (ADR-0054 §3). Selecting it then fails with the reason.
pub fn get_default_backend(base: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(default_backend_file(base)).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(
        canonical_backend_name(raw)
            .map(str::to_string)
            .unwrap_or_else(|| raw.to_lowercase()),
    )
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
    delonix_state::write_atomic(&default_backend_file(base), canon.as_bytes())
        .map_err(state_err)?;
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
        Err(Error::UnregisteredBackendInRecord(format!(
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
    ) -> delonix_model::Result<Boot> {
        // Cloud Hypervisor does not support virtio-9p (only virtio-fs, which requires the
        // virtiofsd daemon, not yet wired up). `spec.volumes` on a CH VM is a
        // clear error instead of a silently ignored mount — the bin
        // auto-selects libvirt when there are volumes, so this only fires
        // if the user FORCES `backend: cloud-hypervisor` with volumes.
        if !cfg.volumes.is_empty() {
            return Err(Error::RequiresLibvirtBackend(format!(
                "VM '{}': spec.volumes requires the libvirt backend (Cloud Hypervisor does not do virtio-9p) — remove `backend: cloud-hypervisor` or the volumes",
                cfg.name
            )).into());
        }
        // Before the network is touched: a socket path the kernel will refuse
        // kills the VMM at startup, so it is a clear error here instead.
        ch_socket_paths_fit(vmdir, cfg)?;
        // Own private network when named (≠ shared ingress): ensures its
        // isolated bridge + DHCP before the attach. The VMs' SDN lives here.
        on(CreateStage::Network);
        if !matches!(cfg.network.as_str(), "" | "ingress" | "bridge" | "default") {
            let _ = network()?.ensure_network(&cfg.network);
        }
        // The MAC is needed BEFORE the attach now, not after: it is what makes
        // the guest's future DHCP address computable, and that address is what
        // the attach registers in the namespace sets.
        let mac = mac_for(&cfg.name);
        let ns = vm_namespace_of(cfg);
        let net = network()?;
        let tap = net.attach_tap(&cfg.name, &cfg.network, &mac, &ns)?;
        let lease = net.lease_ip(&cfg.network, &mac);
        on(CreateStage::Start);
        let pid = match boot_ch(vmdir, cfg, overlay, &tap, &mac) {
            Ok(p) => p,
            Err(e) => {
                net.detach_tap(&cfg.name, lease.as_deref());
                return Err(e.into());
            }
        };
        let sock = vmdir.join(format!("{}.sock", cfg.name));
        Ok(Boot {
            pid: Some(pid),
            ip: net.lease_ip(&cfg.network, &mac),
            tap,
            mac,
            api_socket: sock.to_string_lossy().into_owned(),
            lease_floor: None,
        })
    }

    fn is_running(&self, vm: &Vm) -> bool {
        // Mesma pergunta que o `stop`: vivo E ainda o nosso.
        vm.pid.is_some_and(|p| safe_to_signal(p, vm.pid_starttime))
    }

    fn ip(&self, vm: &Vm) -> Option<String> {
        network().ok()?.lease_ip(&vm.network, &vm.mac)
    }

    /// Computed from the MAC, and available before the guest has booted — see
    /// `delonix_sdn::infra::dhcp_lease_ip`, and [`VmBackend::ip_is_predicted`] for why
    /// anyone waiting on a boot needs to be told.
    fn ip_is_predicted(&self) -> bool {
        true
    }

    /// `vm resize` (`vm.resize.cold`): nothing to change outside the record.
    /// This backend keeps no definition of its own between boots — `vm start`
    /// rebuilds the vmm's command line from the record (`start` → `create(config_from(..))`),
    /// so the engine rewriting `vcpus`/`memory` there is the whole resize.
    fn resize_cold(
        &self,
        _vmdir: &Path,
        _vm: &Vm,
        _vcpus: u32,
        _memory_mib: u64,
    ) -> delonix_model::Result<()> {
        Ok(())
    }

    fn stop(&self, _vmdir: &Path, vm: &Vm) -> delonix_model::Result<()> {
        // `pid > 0` was the ONLY condition here, which is not an identity: a
        // record left behind by a VMM that died — or by a reboot — names a
        // number the kernel is free to hand to anything, and this path then
        // SIGTERMs a stranger. The container side of this engine has guarded
        // every signal with `safe_to_signal` for a long time (sixteen call
        // sites); the VM side simply never got it, and its record did not even
        // carry the `starttime` to check against.
        //
        // Signal AND WAIT — see `terminate_vmm`. A `stop` that returns while
        // the vmm runs is not a stop: the record says `Stopped` and `pid:
        // null`, so `is_running` answers no to everyone who asks next, while
        // the process is still there holding the disk. The detach below is
        // downstream of the same fact — pulling the tap out from under a live
        // guest is not a teardown either — so a VMM that will not leave is an
        // error here, not something to keep walking past.
        if let Some(pid) = vmm_to_signal(vm) {
            // `PUT /api/v1/vmm.shutdown` FIRST, `SIGTERM`/`SIGKILL` only as a
            // fallback — this is BUG-VM-001 (measured live, 2026-09-14): a real
            // guest write in flight at the moment of `SIGTERM` left the qcow2
            // refcount table corrupted, even though `terminate_vmm` faithfully
            // waited for the process to be fully, confirmably gone before the
            // next command ran (the OS-level exit was clean; the disk was not).
            // A signal is delivered asynchronously — Cloud Hypervisor has to
            // catch it and unwind whatever I/O was mid-flight from inside a
            // handler. `vmm.shutdown` runs on its own ordinary async path
            // instead: the block backend drops through its normal Rust
            // destructor chain, the same way `pause`/`resume` above already
            // talk to this VM over its own api-socket rather than a signal.
            // Confirmed live against this exact binary (CH v53.0): the call
            // answers `200` and the process is gone by the time it returns.
            let graceful = ch_api_put(&vm.api_socket, "/api/v1/vmm.shutdown").is_ok()
                && wait_vmm_left(pid, vm.pid_starttime, VMM_TERM_GRACE);
            if !graceful && !terminate_vmm(pid, vm.pid_starttime, VMM_TERM_GRACE, VMM_KILL_GRACE) {
                return Err(Error::Command {
                    context: "vm",
                    message: format!(
                        "cloud-hypervisor (pid {pid}) of VM '{}' did not exit after SIGTERM and \
                         SIGKILL ({}s) — it still holds the VM's disk, so a snapshot would fail; \
                         the VM is left as it is instead of being recorded as stopped",
                        vm.name,
                        (VMM_TERM_GRACE + VMM_KILL_GRACE).as_secs()
                    ),
                }
                .into());
            }
        }
        // The record's own address if it learned one; otherwise the lease its MAC
        // maps to — so a VM stopped before it ever DHCP'd still gives up its chain.
        let net = network()?;
        let ip = vm.ip.clone().or_else(|| net.lease_ip(&vm.network, &vm.mac));
        net.detach_tap(&vm.name, ip.as_deref());
        Ok(())
    }

    fn disk_health(&self, vmdir: &Path, vm: &Vm) -> delonix_model::Result<()> {
        let disk = ch_overlay(vmdir, vm);
        if !disk_looks_corrupt(&disk) {
            return Ok(());
        }
        Err(Error::DiskCorrupted(format!(
            "VM '{}' stopped, but `qemu-img check` now finds its disk corrupted \
             (BUG-VM-001) — measured live to happen even through a graceful \
             `vmm.shutdown`, with a real guest write in flight at the moment of \
             `stop`, which points at Cloud Hypervisor's own qcow2 writer under host \
             memory pressure, not at anything `stop` controls. Do NOT run `snapshot \
             restore` against it — try `qemu-img check -r all {}` first, and consider \
             `--backend libvirt` for VMs that need reliable disk snapshots.",
            vm.name,
            disk.display()
        ))
        .into())
    }

    // ---- pause/unpause -----------------------------------------------------
    //
    // Cloud Hypervisor's own `PUT /api/v1/vm.pause`/`vm.resume`, over the
    // per-VM api-socket `boot` already opens (`vm.api_socket`). Unlike
    // `vm.snapshot` (see the comment above the snapshot methods below), this
    // never touches the disk — it only suspends/resumes vCPUs — so it has
    // none of the "who holds the qcow2 lock" conflict that keeps snapshot
    // offline instead.

    fn pause(&self, _vmdir: &Path, vm: &Vm) -> delonix_model::Result<()> {
        Ok(ch_api_put(&vm.api_socket, "/api/v1/vm.pause")?)
    }

    fn unpause(&self, _vmdir: &Path, vm: &Vm) -> delonix_model::Result<()> {
        Ok(ch_api_put(&vm.api_socket, "/api/v1/vm.resume")?)
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

    fn snapshots(&self, vmdir: &Path, vm: &Vm) -> delonix_model::Result<Vec<String>> {
        // `-U` (force-share) because this has to answer while the VM RUNS, and
        // the vmm holds the write lock: `qemu-img info` opens read-only, which
        // is the only mode force-share allows. Plain `snapshot -l` opens
        // read-write and fails on a running VM.
        let out = capture(
            "qemu-img",
            &["info", "-U", "--", &ch_overlay(vmdir, vm).to_string_lossy()],
        )
        .ok_or_else(|| {
            delonix_model::Error::from(Error::Command {
                context: "qemu-img info",
                message: format!("could not read the disk of VM '{}'", vm.name),
            })
        })?;
        Ok(parse_qemu_snapshot_list(&out))
    }

    fn snapshot(&self, vmdir: &Path, vm: &Vm, name: &str) -> delonix_model::Result<()> {
        self.offline_snapshot_op(vmdir, vm, "take a snapshot of")?;
        if self.snapshots(vmdir, vm)?.iter().any(|s| s == name) {
            return Err(taken_snapshot(&vm.name, name));
        }
        Ok(qemu_img_snapshot(vmdir, vm, "-c", name)?)
    }

    fn restore(&self, vmdir: &Path, vm: &Vm, name: &str) -> delonix_model::Result<()> {
        self.offline_snapshot_op(vmdir, vm, "restore")?;
        if !self.snapshots(vmdir, vm)?.iter().any(|s| s == name) {
            return Err(missing_snapshot(&vm.name, name));
        }
        Ok(qemu_img_snapshot(vmdir, vm, "-a", name)?)
    }

    fn delete_snapshot(&self, vmdir: &Path, vm: &Vm, name: &str) -> delonix_model::Result<()> {
        self.offline_snapshot_op(vmdir, vm, "delete a snapshot of")?;
        if !self.snapshots(vmdir, vm)?.iter().any(|s| s == name) {
            return Err(missing_snapshot(&vm.name, name));
        }
        Ok(qemu_img_snapshot(vmdir, vm, "-d", name)?)
    }
}

impl CloudHypervisorBackend {
    /// Refuses a snapshot verb while the VM runs, saying WHY and what to do —
    /// this is a limit of the hypervisor, not a missing feature: the running
    /// vmm holds the qcow2 exclusively, so there is no way to checkpoint the
    /// disk under it. Silence here would be worse than the refusal: the write
    /// simply would not happen.
    fn offline_snapshot_op(&self, _vmdir: &Path, vm: &Vm, what: &str) -> delonix_model::Result<()> {
        if !self.is_running(vm) {
            return Ok(());
        }
        Err(Error::SnapshotNeedsStopped(format!(
            "cloud-hypervisor cannot {what} a RUNNING VM: the vmm holds its disk exclusively \
             and CH has no live disk-snapshot API. Stop it first (`delonix vm stop {}`) — the \
             snapshot is then taken in the disk itself and survives everything. A VM that \
             needs checkpoints while it runs belongs on `--backend libvirt`.",
            vm.name
        ))
        .into())
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
    .map_err(|e| Error::Command {
        context: "qemu-img snapshot",
        message: e,
    })
}

/// `true` only when `qemu-img check` confirms the image IS corrupted — its
/// documented exit code 2, "check completed, image is corrupted". Exit 0
/// (clean), 3 (leaked-but-not-corrupt clusters — wasted space, not damage) and
/// anything unreachable (binary missing, disk gone) all answer `false`: this
/// exists to add a loud diagnosis on top of a REAL positive, never to block
/// `stop` on a guess — same reasoning as `preflight_cgroup_controllers`'s
/// "cannot tell — do not block" elsewhere in this engine.
fn disk_looks_corrupt(disk: &Path) -> bool {
    stable_cmd("qemu-img")
        .args(["check", "--"])
        .arg(disk)
        .status()
        .is_ok_and(|st| st.code() == Some(2))
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

/// Where a capture-mode VM's serial console is written (`<base>/vms/<name>.serial`).
///
/// THE formula, with one owner. It used to be spelled out separately in `boot_ch`
/// and in a reader in another program, which is exactly how the two came to
/// disagree: the interactive console moved the writer to a socket and the reader
/// went on opening a file nobody wrote. Same discipline as `fw_rule_tail` — the
/// writer and the reader share the format or they drift.
pub fn serial_log_path(base: &Path, name: &str) -> std::path::PathBuf {
    base.join("vms").join(format!("{name}.serial"))
}

/// A minimal HTTP/1.1 `PUT` with no request body, used only for Cloud
/// Hypervisor's `vm.pause`/`vm.resume` (which have no response body either —
/// both answer `204 No Content`). There is no HTTP client anywhere in this
/// crate — ADR-0008 keeps `delonix-vm` free of network dependencies, and a
/// REMOTE backend gets its own crate instead (`delonix-proxmox`) — so this is
/// the smallest thing that talks the one endpoint these two verbs need, over
/// `UnixStream`, the same primitive `delonix-sdn`'s `slirp_api` already uses
/// for a different (line-delimited JSON) protocol.
fn ch_api_put(sock: &str, path: &str) -> Result<()> {
    let (status_line, _) = ch_api_call(sock, "PUT", path, Duration::from_secs(10))?;
    if http_status_is_2xx(&status_line) {
        Ok(())
    } else {
        Err(Error::CloudHypervisorApi(format!(
            "cloud-hypervisor api {path}: {}",
            if status_line.is_empty() {
                "no response"
            } else {
                &status_line
            }
        )))
    }
}

/// One request with no body over the api-socket; returns the status line and
/// the response body (read up to `Content-Length`, which Cloud Hypervisor
/// always sends on a body it has).
///
/// Waiting for EOF instead would hang on a keep-alive connection the server
/// never closes, so the read stops at the end of the headers when there is no
/// `Content-Length`, and at the end of the declared body when there is.
fn ch_api_call(
    sock: &str,
    method: &str,
    path: &str,
    timeout: Duration,
) -> Result<(String, Vec<u8>)> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(sock)
        .map_err(|e| Error::CloudHypervisorApi(format!("cloud-hypervisor api socket: {e}")))?;
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
    s.write_all(req.as_bytes())
        .map_err(|e| Error::CloudHypervisorApi(format!("cloud-hypervisor api write: {e}")))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    let mut want: Option<usize> = None;
    loop {
        if let Some(total) = want {
            if buf.len() >= total {
                break;
            }
        } else if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let len = http_content_length(&String::from_utf8_lossy(&buf[..end])).unwrap_or(0);
            want = Some(end + 4 + len);
            continue;
        }
        if buf.len() > 65536 {
            break;
        }
        let n = s
            .read(&mut chunk)
            .map_err(|e| Error::CloudHypervisorApi(format!("cloud-hypervisor api read: {e}")))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf);
    let status_line = text.lines().next().unwrap_or_default().trim().to_string();
    let body = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|end| buf[end + 4..].to_vec())
        .unwrap_or_default();
    Ok((status_line, body))
}

/// Pure: the `Content-Length` of a response header block, if it declares one.
fn http_content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| v.trim().parse().ok())
            .flatten()
    })
}

/// Pure: `true` for any HTTP status line in the 2xx range. Tested against the
/// exact shape Cloud Hypervisor returns (`HTTP/1.1 204 No Content`).
fn http_status_is_2xx(status_line: &str) -> bool {
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|code| (200..300).contains(&code))
}

/// Cloud Hypervisor's `--serial` destination: a capture FILE or an interactive
/// SOCKET.
///
/// **Pure, and one or the other — never both.** `--serial` takes a single
/// destination (`off|null|pty|tty|file=<path>|socket=<path>`, read off the
/// binary's own `--help`) and the guest writes to a single `/dev/console`
/// (`ttyS0`), so "capture *and* stay interactive" is not expressible. Splitting
/// the decision out is what lets it be tested without a hypervisor: a live VM was
/// never going to be the thing standing between this and a regression.
fn ch_serial_dest(capture: bool, serial: &Path, console: &Path) -> String {
    if capture {
        format!("file={}", serial.display())
    } else {
        format!("socket={}", console.display())
    }
}

fn boot_ch(vmdir: &Path, cfg: &VmConfig, overlay: &str, tap: &str, mac: &str) -> Result<i32> {
    let join = network()?.join_argv().ok_or_else(|| Error::Command {
        context: "vm",
        message: "the ingress (rootless infra) is not up".into(),
    })?;
    let sock = vmdir.join(format!("{}.sock", cfg.name));
    let serial = serial_log_path(vmdir.parent().unwrap_or(vmdir), &cfg.name);
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
        return Err(Error::NoFirmware(
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
    // Serial: EITHER an interactive socket OR a capture file — see
    // `ch_serial_dest` for why there is no "both".
    let console = console_socket(vmdir.parent().unwrap_or(vmdir), &cfg.name);
    // Drop the stale endpoint of the mode we are entering. For capture this is
    // not tidying: readers take the LAST marker they find, so a log left from a
    // previous boot would serve a join token that expired with it.
    let _ = std::fs::remove_file(if cfg.serial_capture {
        &serial
    } else {
        &console
    });
    ch.push("--serial".into());
    ch.push(ch_serial_dest(cfg.serial_capture, &serial, &console));
    ch.push("--console".into());
    ch.push("off".into());

    // background inside the netns; no pid-ns ⇒ $! is the real PID on the host.
    let ch_str = ch.iter().map(|a| shq(a)).collect::<Vec<_>>().join(" ");
    let script = format!(
        "{ch_str} </dev/null >>{log} 2>&1 & echo $! > {pid}",
        log = shq(&log.to_string_lossy()),
        pid = shq(&pidfile.to_string_lossy())
    );

    launch_vmm(&join, &script, &pidfile, &sock, &log, VMM_READY_GRACE)
}

/// How long `boot_ch` waits for a freshly launched VMM to report its VM as
/// `Running`. Measured on this host (CH v53.0, 2026-09-16): a good boot answers
/// on the first poll and a failed one is gone in under 50 ms, so this only
/// bounds a VMM that is alive and silent.
const VMM_READY_GRACE: Duration = Duration::from_secs(10);

/// Runs the launch `script` behind `join`, reads the pid it writes, and
/// returns it **only once the VMM confirms its VM is running**.
///
/// Returning on the pidfile alone was the defect: the `sh` backgrounds the VMM
/// and exits 0 whether the VMM comes up or dies a millisecond later, so `vm
/// create` recorded `Running` for a process that had already exited (measured
/// 2026-09-16: an api-socket path past `SUN_LEN` exits at 0.0005 s, an invalid
/// firmware at 0.044 s — both with `vm create` at rc=0). And `is_running` is
/// `kill(pid, 0)`, true for a zombie, so the first check after boot could
/// still agree and the next one not.
///
/// Split out of `boot_ch` (with `join` as a parameter) so a test can run it
/// with a no-op join and a fake VMM; that test fails if the confirmation is
/// dropped from this path.
fn launch_vmm(
    join: &[String],
    script: &str,
    pidfile: &Path,
    sock: &Path,
    log: &Path,
    ready: Duration,
) -> Result<i32> {
    let st = Command::new(&join[0])
        .args(&join[1..])
        .args(["sh", "-c", script])
        .env("DELONIX_INTERNAL", "1")
        .status()
        .map_err(|e| Error::Command {
            context: "cloud-hypervisor",
            message: e.to_string(),
        })?;
    if !st.success() {
        return Err(Error::Command {
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
    let pid = std::fs::read_to_string(pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(0);
    if pid <= 0 {
        return Err(Error::Command {
            context: "vm",
            message: format!(
                "cloud-hypervisor did not report a PID{}",
                log_tail_suffix(log)
            ),
        });
    }
    wait_vmm_ready(pid, sock, log, ready)?;
    Ok(pid)
}

/// Waits until the VMM at `pid` answers `GET /api/v1/vm.info` with the VM in
/// state `Running`. Errors — with the tail of the VM log — if the process
/// leaves (a zombie counts, see [`vmm_left`]) or the grace runs out; in the
/// second case the silent VMM is terminated so no orphan holds the disk.
///
/// **Why `vm.info` and not `vmm.ping`:** measured against CH v53.0 with an
/// invalid firmware, `vmm.ping` ANSWERS (500, and 200 is possible in the same
/// window) while the boot is failing, and the process is gone 44 ms later. The
/// VMM's own statement that the VM is `Running` only exists after the kernel
/// or firmware was loaded and the vCPUs started — which is what `vm create`
/// reports.
fn wait_vmm_ready(pid: i32, sock: &Path, log: &Path, ready: Duration) -> Result<()> {
    let starttime = proc_starttime(pid);
    let sock = sock.to_string_lossy();
    let deadline = Instant::now() + ready;
    loop {
        if starttime.is_none() || vmm_left(pid, starttime) {
            return Err(Error::Command {
                context: "vm",
                message: format!(
                    "cloud-hypervisor exited during startup{}",
                    log_tail_suffix(log)
                ),
            });
        }
        if let Ok((status, body)) =
            ch_api_call(&sock, "GET", "/api/v1/vm.info", Duration::from_secs(1))
        {
            if http_status_is_2xx(&status) && vm_info_says_running(&body) {
                // Re-checked AFTER the answer: the answer and the exit can
                // cross, and a dead VMM must not be returned as up.
                if !vmm_left(pid, starttime) {
                    return Ok(());
                }
                continue;
            }
        }
        if Instant::now() >= deadline {
            let _ = terminate_vmm(pid, starttime, VMM_KILL_GRACE, VMM_KILL_GRACE);
            return Err(Error::Command {
                context: "vm",
                message: format!(
                    "cloud-hypervisor did not report the VM running within {}s (terminated){}",
                    ready.as_secs(),
                    log_tail_suffix(log)
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Pure: does a `vm.info` body say `"state": "Running"`? Tolerates the
/// whitespace a pretty-printer would add; no JSON parser needed for one field.
fn vm_info_says_running(body: &[u8]) -> bool {
    let compact: String = String::from_utf8_lossy(body)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    compact.contains("\"state\":\"Running\"")
}

/// `" — VM log: <last lines>"`, or `""` when the log is empty/unreadable, so
/// the cause (e.g. `path must be shorter than SUN_LEN`) reaches the user
/// instead of staying in a file they were never told about.
fn log_tail_suffix(log: &Path) -> String {
    let Ok(raw) = std::fs::read(log) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&raw);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return String::new();
    }
    let tail = lines[lines.len().saturating_sub(8)..].join("\n  ");
    format!(" — {}:\n  {tail}", log.display())
}

/// The largest path a UNIX socket can bind: `sun_path` is 108 bytes on Linux
/// and the kernel wants the terminating NUL inside it.
const SUN_PATH_MAX: usize = 107;

/// Pure: refuses up front a Cloud Hypervisor VM whose api-socket (always) or
/// console socket (when interactive) would not fit in `sun_path`. Without this
/// the VMM dies at 0.0005 s with `path must be shorter than SUN_LEN` — measured
/// with a `DELONIX_ROOT` deep enough that `<root>/vms/<name>.sock` is 108 bytes.
fn ch_socket_paths_fit(vmdir: &Path, cfg: &VmConfig) -> Result<()> {
    let mut socks = vec![vmdir.join(format!("{}.sock", cfg.name))];
    if !cfg.serial_capture {
        socks.push(console_socket(vmdir.parent().unwrap_or(vmdir), &cfg.name));
    }
    for s in socks {
        let len = s.as_os_str().len();
        if len > SUN_PATH_MAX {
            return Err(Error::SocketPathTooLong(format!(
                "VM '{}': socket path {} is {len} bytes, and a UNIX socket path is limited to {SUN_PATH_MAX} — use a shorter VM name or a shorter DELONIX_ROOT",
                cfg.name,
                s.display()
            )));
        }
    }
    Ok(())
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
///
/// Returns the SHARED type directly, like [`unsupported_pause`]: every caller
/// is a `VmBackend` trait method body, and that trait stays on
/// `delonix_model::Result`.
fn missing_snapshot(vm: &str, snap: &str) -> delonix_model::Error {
    Error::SnapshotNotFound(format!("snapshot of VM '{vm}': {snap}")).into()
}

/// The name is TAKEN — `Conflict` (exit 5), the class whose next move is «pick
/// another name or remove that one», as opposed to «create it» (4) or
/// «something broke» (1).
fn taken_snapshot(vm: &str, snap: &str) -> delonix_model::Error {
    Error::SnapshotTaken(format!(
        "VM '{vm}' already has a snapshot named '{snap}' (see `delonix vm snapshot ls {vm}`)"
    ))
    .into()
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
    Err(Error::StaticIpReservationFailed(format!(
        "could not reserve static IP {ip} on libvirt network '{net}': {msg}"
    )))
}

/// Drops the DHCP reservation [`libvirt_reserve_ip`] added. Best effort by
/// design: the network may be gone or the entry already removed, and neither
/// is a reason to keep a VM that is being destroyed.
fn libvirt_release_ip(uri: &str, net: &str, mac: &str, ip: &str) {
    let entry = format!("<host mac='{mac}' ip='{ip}'/>");
    let _ = quiet(
        "virsh",
        &[
            "-c",
            uri,
            "net-update",
            "--live",
            "--config",
            "--",
            net,
            "delete",
            "ip-dhcp-host",
            &entry,
        ],
    );
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
/// Does a `virsh domstate` answer mean the domain is ALIVE — the question
/// [`VmBackend::is_running`] asks, and the same one Cloud Hypervisor answers
/// with "the VMM process is there"?
///
/// `paused` counts. BUG FIXED HERE, reproduced on libvirt: `vm pause` left the
/// domain `paused` (confirmed by `virsh domstate`), but only `running` was
/// accepted, so the next `vm ls` took the reconciliation's "powered off" branch
/// and reported `Stopped` with `pid=None` for a VM whose guest memory was
/// intact. The `Paused` guard in [`status`] lives inside the alive branch, so it
/// only ever worked on Cloud Hypervisor. Treating `paused` as alive also stops
/// the offline snapshot path from undefining a domain a restore left paused.
fn libvirt_domstate_is_alive(s: &str) -> bool {
    s == "running" || s == "paused"
}

fn libvirt_poweroff(uri: &str, name: &str) -> Result<()> {
    let state = capture("virsh", &["-c", uri, "domstate", "--", name]).unwrap_or_default();
    if state.is_empty() || state == "shut off" {
        return Ok(());
    }
    quiet("virsh", &["-c", uri, "destroy", "--", name])
        .map(|_| ())
        .map_err(|msg| Error::Command {
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
        .map_err(|msg| Error::Command {
            context: "vm",
            message: format!("could not remove VM '{name}' from libvirt ({uri}): {msg}"),
        })
}

/// The domain XML without the settings a session (unprivileged) libvirt cannot apply:
/// the `<memtune>` hard limit and the `<cputune>` period/quota. CPU pinning stays — it
/// is an affinity, not a cgroup limit. Pure.
fn strip_cgroup_tuning(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut in_memtune = false;
    for line in xml.split_inclusive('\n') {
        let t = line.trim();
        if t == "<memtune>" {
            in_memtune = true;
            continue;
        }
        if in_memtune {
            if t == "</memtune>" {
                in_memtune = false;
            }
            continue;
        }
        if t.starts_with("<period>") || t.starts_with("<quota>") {
            continue;
        }
        out.push_str(line);
    }
    // A `<cputune>` left with nothing inside is not valid to define.
    out.replace("  <cputune>\n  </cputune>\n", "")
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

/// Where this VM's serial console is captured, or `None` for the interactive pty.
///
/// The path is DERIVED — `<vmdir>/<name>.serial`, the same formula `boot_ch` uses —
/// and never comes from a caller: `cfg.name` is already validated by
/// [`valid_vm_name`], so nothing caller-controlled reaches the domain XML. The
/// overlay always lives in the VM directory (`create` builds it as
/// `vmdir/<name>.qcow2`), which is what makes the parent recoverable here without
/// widening this pure function's signature.
fn serial_capture_path(cfg: &VmConfig, overlay: &str) -> Option<String> {
    if !cfg.serial_capture {
        return None;
    }
    let vmdir = Path::new(overlay).parent()?;
    let base = vmdir.parent().unwrap_or(vmdir);
    Some(
        serial_log_path(base, &cfg.name)
            .to_string_lossy()
            .into_owned(),
    )
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
    // Extra disks (typed): additional images beyond the main overlay. Target
    // devs are auto-assigned per bus (vdb, vdc… for virtio; sdb… for sata/scsi)
    // unless the user pinned one — `vda` stays reserved above, and the cloud-init
    // seed takes the virtio letter AFTER the extras, so adding a seed never
    // renames a disk the guest already knows.
    let mut vd = b'b'; // next virtio letter (vda taken by the main disk)
    let mut sd = b'b'; // next sata/scsi letter (kept from when the seed sat on sda)
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
    // cloud-init seed (NoCloud), as a read-only VIRTIO disk.
    //
    // It used to be a SATA cdrom, and that made cloud-init invisible on every
    // image whose kernel has no SATA: the Debian `-cloud` kernel (the
    // `genericcloud` image) enumerates the q35 AHCI controller on PCI and binds
    // no driver — no libata, no `sr` — so the seed never appears as a block
    // device, no datasource is found, and the guest boots with hostname
    // `localhost`, no network config and no user-data (measured on
    // `delonix-vm-base:debian-bookworm`, kernel 6.1.0-52-cloud-amd64). Every
    // guest that runs on a hypervisor has virtio-blk, and NoCloud finds the
    // seed by its `cidata` label on any block device. `snapshot='no'` keeps it
    // out of libvirt's disk snapshots and blockcommit, as the cdrom was.
    if let Some(seed) = &cfg.seed {
        let target = format!("vd{}", vd as char);
        s.push_str("    <disk type='file' device='disk' snapshot='no'>\n");
        s.push_str("      <driver name='qemu' type='raw'/>\n");
        s.push_str(&format!("      <source file='{}'/>\n", xml_escape(seed)));
        s.push_str(&format!("      <target dev='{target}' bus='virtio'/>\n"));
        s.push_str("      <readonly/>\n    </disk>\n");
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
    // serial console (boot logs) — a capture FILE or an interactive pty, never
    // both: one guest `/dev/console` maps to one serial port. See
    // `VmConfig::serial_capture`.
    match serial_capture_path(cfg, overlay) {
        Some(path) => {
            let path = xml_escape(&path);
            s.push_str(&format!(
                "    <serial type='file'><source path='{path}'/><target type='isa-serial' port='0'/></serial>\n"
            ));
            s.push_str(&format!(
                "    <console type='file'><source path='{path}'/><target type='serial' port='0'/></console>\n"
            ));
        }
        None => {
            s.push_str("    <serial type='pty'><target type='isa-serial' port='0'/></serial>\n");
            s.push_str("    <console type='pty'><target type='serial' port='0'/></console>\n");
        }
    }
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
    let model = &format!(
        "      <model type='virtio'/>\n{}    </interface>\n",
        libvirt_filterref_xml(cfg.net_mode.as_deref())
    )[..];
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

/// The nwfilter every engine-created libvirt VM's NIC references.
///
/// # Why a filter of ours and not `clean-traffic`
///
/// libvirt ships `clean-traffic`, and it is the obvious choice until you read
/// it. Measured on libvirt 10.0.0, its last two members are
/// `no-other-l2-traffic` — a bare `<rule action='drop' direction='inout'
/// priority='1000'/>` — after rules that accept only IPv4 and ARP. Referencing
/// it would therefore drop **all IPv6** out of every VM, silently, as a side
/// effect of asking for anti-spoofing. That is a functional regression wearing
/// a security label, so this composes the anti-spoofing members and stops
/// there.
///
/// # Why `no-ip-spoofing` is deliberately NOT in here
///
/// It pins the source IPv4 to the one address libvirt knows for the NIC. That
/// is correct for a plain workload and **wrong for a router-shaped one**: a DKS
/// worker running a CNI emits pod traffic with POD source addresses, none of
/// which is the node's, so pinning the source IP would black-hole every pod's
/// egress. The two members below have no such failure mode — pods share the
/// node's MAC — which is why they are safe as an unconditional floor while IP
/// pinning is not. Pinning belongs to a per-VM opt-in, with a name that says
/// what it costs; it is not smuggled into the default.
///
/// So what this DOES guarantee is exactly: the guest cannot emit a frame with a
/// source MAC that is not its own, and cannot answer ARP for one. What it does
/// NOT guarantee — and no caller should be told otherwise — is IPv4 source
/// pinning, IPv6/NDP anti-spoofing, or any L2/L3 filtering beyond those two.
const ANTISPOOF_FILTER: &str = "delonix-antispoof";

/// The uuid used when NOTHING of this name exists yet. Never assume it is the
/// one in the daemon — see [`antispoof_filter_xml`].
const ANTISPOOF_FILTER_UUID: &str = "de102000-0000-4000-8000-000000000001";

/// The filter's definition, carrying `uuid`. **Pure.**
///
/// # Why the uuid is a parameter and not baked in
///
/// `nwfilter-define` is "define **or update**", and which of the two it does
/// turns entirely on the uuid: given a name that already exists under a
/// DIFFERENT uuid it refuses outright — `filter 'delonix-antispoof' already
/// exists with uuid …`. So a hard-coded uuid is idempotent only on a host that
/// has never seen this filter under another one, and a host that HAS (an older
/// engine, an operator, a leftover) would have every `vm create` fail, because
/// [`ensure_antispoof_filter`] fails closed.
///
/// That is not hypothetical: it is how this function came to take a parameter.
/// The first cut pinned the uuid, passed every unit test, and then failed on
/// the first live run against a daemon that already had the name. Hence
/// [`ensure_antispoof_filter`] reads the uuid in place and hands it back here —
/// the define then UPDATES, which is also what makes it self-healing if someone
/// weakened the members out of band (measured, libvirt 10.0.0).
fn antispoof_filter_xml(uuid: &str) -> String {
    format!(
        "<filter name='{ANTISPOOF_FILTER}' chain='root'>\n  \
         <uuid>{uuid}</uuid>\n  \
         <filterref filter='no-mac-spoofing'/>\n  \
         <filterref filter='no-arp-mac-spoofing'/>\n</filter>\n"
    )
}

/// The `<uuid>` out of a `nwfilter-dumpxml`. **Pure** — there is no
/// `nwfilter-info` in virsh, so the XML is the only place the uuid is legible.
fn parse_nwfilter_uuid(xml: &str) -> Option<String> {
    let rest = xml.split_once("<uuid>")?.1;
    let uuid = rest.split_once("</uuid>")?.0.trim();
    (!uuid.is_empty()).then(|| uuid.to_string())
}

/// The `<filterref>` line injected into `<interface>`. Kept as one literal so
/// the emitted name cannot drift from [`ANTISPOOF_FILTER`] (a test pins them
/// together).
const ANTISPOOF_FILTERREF: &str = "      <filterref filter='delonix-antispoof'/>\n";

/// Whether a nwfilter can attach to this NIC at all. **Pure.**
///
/// `nat`/`network` and `bridge` give the guest a tap device on the host, which
/// is what libvirt applies a filter to. `user` mode (SLIRP/passt) has no tap —
/// the traffic never reaches a host interface libvirt can program — so a
/// `filterref` there would be accepted and do nothing. Emitting one anyway is
/// the anti-pattern this codebase has already had to correct three times over
/// (`--security-opt seccomp=`, `-v …:z`, `--network-alias`): an option
/// accepted, ignored, and believed.
fn antispoof_applies(net_mode: Option<&str>) -> bool {
    matches!(net_mode, Some("nat") | Some("network") | Some("bridge"))
}

/// The `<filterref>` block for this NIC, or empty. **Pure** — tested without a
/// daemon.
fn libvirt_filterref_xml(net_mode: Option<&str>) -> &'static str {
    if antispoof_applies(net_mode) {
        ANTISPOOF_FILTERREF
    } else {
        ""
    }
}

/// Defines [`ANTISPOOF_FILTER`] on `uri`, idempotently.
///
/// **Fails closed, and that is the whole point.** A domain whose NIC references
/// a filter the daemon does not have is refused by `virsh define`, so the only
/// alternatives are "abort with a reason" and "quietly drop the filterref and
/// boot an unfiltered VM the operator believes is filtered". The second is how
/// a security control becomes a comment, so this returns the error and lets
/// `boot` carry it up.
///
/// Only reached for the modes [`antispoof_applies`] accepts, which already
/// require the system connection — so in practice the daemon is reachable by
/// the time this runs.
fn ensure_antispoof_filter(uri: &str) -> Result<()> {
    // Adopt the uuid already in the daemon when the name is taken; only mint
    // ours when nothing is there. See `antispoof_filter_xml` for what a pinned
    // uuid costs on a host that has seen this name before.
    let uuid = capture(
        "virsh",
        &["-c", uri, "nwfilter-dumpxml", "--", ANTISPOOF_FILTER],
    )
    .as_deref()
    .and_then(parse_nwfilter_uuid)
    .unwrap_or_else(|| ANTISPOOF_FILTER_UUID.to_string());
    if virsh_define_xml(
        uri,
        "nwfilter-define",
        "nwfilter",
        &antispoof_filter_xml(&uuid),
    ) {
        return Ok(());
    }
    Err(Error::Command {
        context: "libvirt",
        message: format!(
            "could not define the '{ANTISPOOF_FILTER}' network filter on {uri} — refusing to \
             create the VM rather than boot it without the anti-spoofing it is supposed to have. \
             Check that the connection is reachable (`virsh -c {uri} nwfilter-list`)."
        ),
    })
}

/// Writes `xml` to a private temp file and runs `virsh <subcommand> <path>`,
/// returning whether it succeeded.
///
/// The temp-file discipline is not incidental and must not be re-derived by the
/// next caller: a PREDICTABLE name in the world-writable `/tmp` let another
/// local user pre-create a symlink and divert the write (audit finding).
/// `create_new` (`O_EXCL`) fails if the path already exists — without following
/// symlinks — and `0600` closes reading by others. One copy, so a second
/// definition site cannot get it subtly wrong.
fn virsh_define_xml(uri: &str, subcommand: &str, tag: &str, xml: &str) -> bool {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = std::env::temp_dir().join(format!("delonix-{tag}-{}.xml", std::process::id()));
    let _ = std::fs::remove_file(&path); // cleans up a leftover OF OURS from a previous run
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    else {
        return false;
    };
    let ok = f.write_all(xml.as_bytes()).is_ok()
        && stable_cmd("virsh")
            .args(["-c", uri, subcommand, &path.to_string_lossy()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    drop(f);
    let _ = std::fs::remove_file(&path);
    ok
}

/// Ensures a ready NAT libvirt network (`--net-mode nat` → host-pingable IP).
/// Best-effort: if `net` does not exist and is the `default`, it defines the standard NAT
/// network (virbr0, 192.168.122.0/24, DHCP); then `net-start` + `net-autostart`. A
/// clear warning if the system connection is unreachable (missing the libvirt group).
fn ensure_libvirt_network(uri: &str, net: &str) {
    // System connection reachable? (NAT lives in qemu:///system.)
    if capture("virsh", &["-c", uri, "net-list", "--all"]).is_none() {
        eprintln!(
            "warning: cannot reach {uri} for NAT networking — add yourself to the \
'libvirt' group (`sudo usermod -aG libvirt $USER && newgrp libvirt`) and retry"
        );
        return;
    }
    let exists = capture("virsh", &["-c", uri, "net-info", "--", net]).is_some();
    if !exists && net == "default" {
        // XML of the standard libvirt NAT network (the one most distros ship).
        let xml = "<network>\n  <name>default</name>\n  <forward mode='nat'/>\n                     <bridge name='virbr0' stp='on' delay='0'/>\n                     <ip address='192.168.122.1' netmask='255.255.255.0'>\n                       <dhcp><range start='192.168.122.2' end='192.168.122.254'/></dhcp>\n                     </ip>\n</network>\n";
        // The symlink-in-/tmp audit finding this used to carry inline now lives
        // once, in `virsh_define_xml` — see the note there.
        let _ = virsh_define_xml(uri, "net-define", "libvirt-default", xml);
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
    ) -> delonix_model::Result<Boot> {
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
                return Err(Error::StaticIpRequiresNat(format!(
                    "VM '{}': --ip (static IP) requires the libvirt `nat` mode — this VM resolved to '{}' (on a host bridge, reserve the IP on your LAN's DHCP instead)",
                    cfg.name,
                    cfg.net_mode.as_deref().unwrap_or("user")
                )).into());
            }
            if ip.parse::<std::net::Ipv4Addr>().is_err() {
                return Err(Error::InvalidStaticIp(format!(
                    "VM '{}': invalid static IP '{ip}'",
                    cfg.name
                ))
                .into());
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
        // The NIC's `<filterref>` is emitted by `libvirt_domain_xml` below, and
        // `virsh define` REFUSES a domain naming a filter the daemon does not
        // have. So the filter has to exist first, and a failure here has to
        // abort — see `ensure_antispoof_filter` for why the alternative (drop
        // the filterref and boot anyway) is the one thing not on the table.
        if antispoof_applies(cfg.net_mode.as_deref()) {
            ensure_antispoof_filter(uri)?;
        }
        let mut xml = libvirt_domain_xml(cfg, &overlay_abs, &mac);
        if uri == "qemu:///session" {
            // A session daemon has no cgroup controller to hand out: it REFUSES a domain
            // with a memory or CPU-quota ceiling («Memory tuning is not available in
            // session mode»), so the guest never started. The ceilings are protections for
            // a shared host; without them the guest still runs, and this says so.
            let bare = strip_cgroup_tuning(&xml);
            if bare != xml {
                tracing::warn!(
                    vm = %cfg.name,
                    "libvirt session mode cannot enforce the memory/CPU ceilings — this VM runs without them (use `--net-mode nat` on qemu:///system to keep them)"
                );
                xml = bare;
            }
        }
        // On `qemu:///system` the QEMU process runs as the `libvirt-qemu` user,
        // which cannot read the overlay under a 0700 `$HOME`. A static DAC label
        // pins QEMU to the invoking uid/gid (the disk owner) and `relabel='no'`
        // keeps it from chown-ing the disk away from the user. This is what lets
        // a rootless-owned disk boot under system libvirt (needed for NAT/bridge,
        // the only modes with a host-reachable IP).
        if uri == "qemu:///system" && is_rootless() {
            // SAFETY: `getuid` and `getgid` take no arguments and have no preconditions.
            let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
            let sec = format!(
                "  <seclabel type='static' model='dac' relabel='no'>\n    <label>+{uid}:+{gid}</label>\n  </seclabel>\n"
            );
            xml = xml.replace("</domain>\n", &format!("{sec}</domain>\n"));
        }
        let xml_path = vmdir.join(format!("{}.xml", cfg.name));
        delonix_state::write_atomic(&xml_path, xml.as_bytes())?;

        // Idempotent: if the domain already exists (auto-heal), it just (re)starts; otherwise
        // define + start. `virsh start` on an already-running domain is a benign no-op.
        let defined = capture("virsh", &["-c", uri, "domstate", "--", &cfg.name]).is_some();
        if !defined {
            on(CreateStage::Define);
            if let Err(e) = run_quiet("virsh", &["-c", uri, "define", &xml_path.to_string_lossy()])
            {
                let _ = std::fs::remove_file(&xml_path);
                return Err(e.into());
            }
            // A domain defined here is a domain libvirt has never seen before,
            // so any snapshot this VM had is metadata WE are holding — from the
            // `stop` that undefined it. Hand it back now, before the guest
            // runs: `vm snapshots`/`vm restore` must answer about the VM, not
            // about how many times it has been stopped.
            Self::redefine_preserved_snapshots(uri, &cfg.name, vmdir);
        }
        // BEFORE the domain starts: whatever the lease table holds for this MAC
        // now belongs to an earlier boot (see `pick_lease_ip`).
        let lease_floor = Self::leases_of(uri, &cfg.name).and_then(|o| leases_max_expiry(&o, &mac));
        on(CreateStage::Start);
        let out = stable_cmd("virsh")
            .args(["-c", uri, "start", "--", &cfg.name])
            .output()
            .map_err(|e| {
                delonix_model::Error::from(Error::Command {
                    context: "libvirt",
                    message: format!("virsh start: {e}"),
                })
            })?;
        // 'start' fails if it is already running — we tolerate that (auto-heal).
        if !out.status.success() && !self.is_running_uri(uri, &cfg.name) {
            // What libvirt said is the only diagnosis there is: without it a failed
            // start reads as «KVM, permissions or image?» and nothing else.
            let why = String::from_utf8_lossy(&out.stderr);
            let why = why.trim().trim_start_matches("error:").trim().to_string();
            if !defined {
                // We defined this domain a moment ago and it never ran: leave nothing
                // of it behind — not the definition, not the rendered XML, and (for the
                // session connection) not libvirt's per-domain log, whose tail is in
                // the error below.
                let _ = libvirt_cleanup(&cfg.name);
                let _ = std::fs::remove_file(&xml_path);
                if uri == "qemu:///session" {
                    let cache = std::env::var_os("XDG_CACHE_HOME")
                        .map(PathBuf::from)
                        .or_else(|| {
                            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache"))
                        });
                    if let Some(c) = cache {
                        let _ = std::fs::remove_file(
                            c.join("libvirt/qemu/log").join(format!("{}.log", cfg.name)),
                        );
                    }
                }
            }
            return Err(Error::Command {
                context: "vm",
                message: if why.is_empty() {
                    "failed to start the libvirt domain (KVM/permissions/image?)".into()
                } else {
                    format!("failed to start the libvirt domain: {why}")
                },
            }
            .into());
        }
        Ok(Boot {
            pid: None, // managed by libvirtd — liveness via virsh domstate
            ip: match Self::ip_from_leases(uri, &cfg.name, &mac, lease_floor.as_deref()) {
                LeasePick::Current(ip) => Some(ip),
                LeasePick::OnlyStale => None,
                LeasePick::NoLease => self.ip_uri(uri, &cfg.name),
            }
            .or_else(|| cfg.static_ip.clone()),
            // The EFFECTIVE mode (not the requested one): lets `vm describe`
            // and the bin tell a reachable VM (nat/bridge) from an egress-only
            // one (user) — the basis of the "no reachable IP" warning.
            tap: cfg.net_mode.clone().unwrap_or_else(|| "user".into()),
            mac,
            api_socket: String::new(),
            lease_floor,
        })
    }

    fn is_running(&self, vm: &Vm) -> bool {
        self.is_running_uri(libvirt_uri_of(&vm.name), &vm.name)
    }

    fn ip(&self, vm: &Vm) -> Option<String> {
        let uri = libvirt_uri_of(&vm.name);
        match Self::ip_from_leases(uri, &vm.name, &vm.mac, vm.dhcp_lease_floor.as_deref()) {
            LeasePick::Current(ip) => Some(ip),
            // `domifaddr` reads the same lease table without the floor — asking
            // it would hand back the very address just rejected.
            LeasePick::OnlyStale => None,
            LeasePick::NoLease => self.ip_uri(uri, &vm.name),
        }
    }

    /// `vm resize` (`vm.resize.cold`): nothing to change outside the record.
    /// This backend keeps no definition of its own between boots — `vm start`
    /// rebuilds the domain XML from the record (`start` → `create(config_from(..))`),
    /// so the engine rewriting `vcpus`/`memory` there is the whole resize.
    fn resize_cold(
        &self,
        _vmdir: &Path,
        _vm: &Vm,
        _vcpus: u32,
        _memory_mib: u64,
    ) -> delonix_model::Result<()> {
        Ok(())
    }

    fn stop(&self, _vmdir: &Path, vm: &Vm) -> delonix_model::Result<()> {
        libvirt_cleanup(&vm.name)?;
        // The domain XML that `boot` wrote STAYS. It used to be deleted here,
        // which is tidy right up to the moment something needs the domain back:
        // `snapshot`/`restore` on a stopped VM define it again from this exact
        // file — the one libvirt itself had, DAC seclabel and all — instead of
        // re-deriving a description that would only have to agree with `boot`
        // by hand. `remove` still deletes it, with everything else.
        Ok(())
    }

    /// `stop` plus the libvirt-side state that outlives the domain: the DHCP
    /// reservation a `--ip` created on the network. Without this the address
    /// stayed bound to a MAC nothing uses, and a re-created VM with the same
    /// name (same derived MAC) inherited it silently.
    fn destroy(&self, vmdir: &Path, vm: &Vm) -> delonix_model::Result<()> {
        self.stop(vmdir, vm)?;
        if let Some(ip) = vm.boot.static_ip.as_deref() {
            let uri = libvirt_uri_for(Some(vm.tap.as_str()));
            let net = vm.boot.bridge.as_deref().unwrap_or("default");
            libvirt_release_ip(uri, net, &vm.mac, ip);
        }
        Ok(())
    }

    fn pause(&self, _vmdir: &Path, vm: &Vm) -> delonix_model::Result<()> {
        let uri = libvirt_domain_uri(&vm.name)
            .ok_or_else(|| delonix_model::Error::from(Error::VmNotFound(vm.name.clone())))?;
        quiet("virsh", &["-c", uri, "suspend", "--", &vm.name])
            .map(|_| ())
            .map_err(|e| {
                Error::Command {
                    context: "virsh suspend",
                    message: e,
                }
                .into()
            })
    }

    fn unpause(&self, _vmdir: &Path, vm: &Vm) -> delonix_model::Result<()> {
        let uri = libvirt_domain_uri(&vm.name)
            .ok_or_else(|| delonix_model::Error::from(Error::VmNotFound(vm.name.clone())))?;
        quiet("virsh", &["-c", uri, "resume", "--", &vm.name])
            .map(|_| ())
            .map_err(|e| {
                Error::Command {
                    context: "virsh resume",
                    message: e,
                }
                .into()
            })
    }

    fn snapshot(&self, vmdir: &Path, vm: &Vm, name: &str) -> delonix_model::Result<()> {
        let take = |uri: &str| -> delonix_model::Result<()> {
            let argv = libvirt_snapshot_argv(uri, &vm.name, name);
            quiet(
                "virsh",
                &argv.iter().map(String::as_str).collect::<Vec<_>>(),
            )
            .map(|_| ())
            .map_err(|e| {
                Error::Command {
                    context: "virsh snapshot-create-as",
                    message: e,
                }
                .into()
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

    fn restore(&self, vmdir: &Path, vm: &Vm, name: &str) -> delonix_model::Result<()> {
        let revert = |uri: &str| -> delonix_model::Result<()> {
            let argv = libvirt_revert_argv(uri, &vm.name, name);
            quiet(
                "virsh",
                &argv.iter().map(String::as_str).collect::<Vec<_>>(),
            )
            .map(|_| ())
            .map_err(|e| {
                Error::Command {
                    context: "virsh snapshot-revert",
                    message: e,
                }
                .into()
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

    fn delete_snapshot(&self, vmdir: &Path, vm: &Vm, name: &str) -> delonix_model::Result<()> {
        let del = |uri: &str| -> delonix_model::Result<()> {
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
            .map_err(|e| {
                Error::Command {
                    context: "virsh snapshot-delete",
                    message: e,
                }
                .into()
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

    fn snapshots(&self, vmdir: &Path, vm: &Vm) -> delonix_model::Result<Vec<String>> {
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

    fn preserve_snapshots(&self, vmdir: &Path, vm: &Vm) -> delonix_model::Result<Vec<String>> {
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
                return Err(Error::Command {
                    context: "vm",
                    message: format!(
                        "VM '{}': cannot preserve the snapshot '{n}' across a stop (its name is \
                         not usable as a file name). Remove it first with `virsh -c {uri} \
                         snapshot-delete --domain {} --snapshotname '{n}'`",
                        vm.name, vm.name
                    ),
                }
                .into());
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
            .ok_or_else(|| {
                delonix_model::Error::from(Error::Command {
                    context: "virsh snapshot-dumpxml",
                    message: format!(
                        "VM '{}': could not read the snapshot '{n}' to preserve it across the \
                         stop (nothing was stopped — the metadata would be lost by the undefine)",
                        vm.name
                    ),
                })
            })?;
            delonix_state::write_atomic(&dir.join(format!("{n}.xml")), xml.as_bytes())?;
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
        op: &dyn Fn(&str) -> delonix_model::Result<()>,
    ) -> delonix_model::Result<bool> {
        let xml = vmdir.join(format!("{}.xml", vm.name));
        if !xml.exists() {
            return Err(Error::NoStoppedDomainXml(format!(
                "VM '{}' is stopped and its libvirt domain description is not on disk ({}) \
                 — start it once (`delonix vm start {}`) and it stays there from then on",
                vm.name,
                xml.display(),
                vm.name
            ))
            .into());
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
                    delonix_state::write_atomic(&tmp, patched.as_bytes())
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
            .is_some_and(|s| libvirt_domstate_is_alive(&s))
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
    /// `floor` is this boot's [`Vm::dhcp_lease_floor`]: leases at or below it
    /// are an earlier boot's (see [`pick_lease_ip`]). A table that cannot be
    /// read is [`LeasePick::NoLease`], which keeps the `domifaddr` fallback.
    fn ip_from_leases(uri: &str, name: &str, mac: &str, floor: Option<&str>) -> LeasePick {
        Self::leases_of(uri, name).map_or(LeasePick::NoLease, |out| pick_lease_ip(&out, mac, floor))
    }

    /// Raw `virsh net-dhcp-leases` of the network the domain is attached to.
    fn leases_of(uri: &str, name: &str) -> Option<String> {
        let network = Self::network_of(uri, name)?;
        capture("virsh", &["-c", uri, "net-dhcp-leases", "--", &network])
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
    let disk_path = std::fs::canonicalize(&cfg.disk).map_err(|e| {
        Error::from(delonix_model::Error::not_found_or_io(e, || {
            format!("VM image {}", cfg.disk)
        }))
    })?;
    let overlay = vmdir.join(format!("{}.qcow2", cfg.name));
    if !overlay.exists() {
        on(CreateStage::Disk);
        let bf = disk_backing_format(&disk_path);
        let mut argv: Vec<String> = vec![
            "create".into(),
            "-f".into(),
            "qcow2".into(),
            "-b".into(),
            disk_path.to_string_lossy().into_owned(),
            "-F".into(),
            bf,
            overlay.to_string_lossy().into_owned(),
        ];
        // Tamanho pedido para o nó. O `qemu-img create` aceita-o depois do
        // ficheiro e o guest cresce a raiz no arranque (growpart do cloud-init,
        // medido). Sem isto o overlay herda o tamanho da base — que é
        // deliberadamente o PISO da golden.
        if let Some(gib) = cfg.disk_size_gib {
            let pedido = u64::from(gib) * 1024 * 1024 * 1024;
            // Um overlay não pode ser menor que o seu backing file: o
            // `qemu-img` aceita-o em algumas versões e o resultado é uma VM que
            // arranca e corrompe o filesystem. Recusar por nome e com os dois
            // números é a diferença entre um erro e um mistério.
            if let Some(base_bytes) = disk_virtual_size_bytes(&disk_path) {
                if pedido < base_bytes {
                    return Err(Error::DiskTooSmall(format!(
                        "--disk-size {gib}G é menor que a imagem base ({} GiB): um overlay qcow2 \
                         não encolhe o seu backing file",
                        base_bytes / (1024 * 1024 * 1024)
                    )));
                }
            }
            argv.push(format!("{gib}G"));
        }
        run_quiet(
            "qemu-img",
            &argv.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
    }
    Ok((disk_path, overlay))
}

pub fn create_with(base: &Path, cfg: &VmConfig, on: &dyn Fn(CreateStage)) -> Result<Vm> {
    if !valid_vm_name(&cfg.name) {
        return Err(Error::InvalidName(format!(
            "invalid VM name '{}' — use letters, digits, '.', '_' or '-' (no '/', '..', or leading '-')",
            cfg.name
        )));
    }
    // Requirements are NAMES until here; an unknown one is refused before
    // any store is opened or any backend asked (ADR-0050 D6).
    let required = resolve_required_capabilities(&cfg.required_capabilities)?;
    let vmdir = vms_dir(base);
    std::fs::create_dir_all(&vmdir)?;
    let st = store(base)?;

    let restarting = st.load(&cfg.name).ok();
    // Anti-clobber (two VM subsystems in the SAME folder): if a
    // `<name>.json` that does NOT parse as a declarative Vm already exists, it is a direct-QEMU
    // record (`vm run`) — refuse instead of overwriting it and leaving that VM orphaned.
    if restarting.is_none() && vmdir.join(format!("{}.json", cfg.name)).exists() {
        return Err(Error::RecordConflict(format!(
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
            // A requirement holds on a restart too: the backend is the record's,
            // and if it cannot do what the caller now requires the answer is
            // the same refusal, not a VM that came back without it.
            require_capabilities(b.id(), &required)?;
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
            let standing_choice = standing_backend_choice(base)?;
            let want = match cfg.backend.as_deref().or(standing_choice.as_deref()) {
                Some(b) => Some(b.to_string()),
                None if !cfg.volumes.is_empty() => Some("libvirt".to_string()),
                None if cfg.kernel.is_none() && LibvirtBackend.available() => {
                    Some("libvirt".to_string())
                }
                None if cfg.kernel.is_none() => {
                    eprintln!(
                        "warning: booting a cloud image on Cloud Hypervisor \
(libvirt not found) — if it panics on 'unable to mount root fs', install \
libvirt+qemu"
                    );
                    None
                }
                None => None,
            };
            select_backend_requiring(want.as_deref(), &required)?
        }
    };

    // Admission: refuses to boot if there is no RAM on the host (anti-overcommit).
    // Only the VMs that will REALLY boot (not the idempotent already-running one above).
    vm_admission_check(cfg)?;

    // Namespace isolation is enforceable only where the VM is on OUR dataplane.
    // Refuse rather than accept-and-ignore — see `vm_namespace_supported`.
    let ns = vm_namespace_of(cfg);
    if ns != "default" && !vm_namespace_supported(backend.id()) {
        return Err(Error::NamespaceUnsupported(format!(
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

    // REALIZE the cloud-init intent, for the backends that need it realized as a
    // file. A local backend has no vocabulary for "this VM should have this
    // hostname and these keys" other than a NoCloud ISO, so the engine builds
    // one here; a backend that owns its storage is handed the intent verbatim
    // and maps it to whatever the far node speaks (Proxmox: `--ciuser`/
    // `--sshkeys`), which is the entire reason these fields exist.
    //
    // This used to be the CALLER's job, and that is what kept cloud-init local:
    // every consumer built its own ISO — the CLI, `cluster kubeadm`, and other
    // programs in copies of their own — so a remote backend could only ever
    // receive a path it could not open.
    //
    // An explicit `seed` always wins: someone who built their own is not asking
    // us to guess. An appliance (`cloud_init: Some(false)`) gets nothing — an
    // ISO nobody reads, on a drive that changes the guest's device list.
    let realized;
    let cfg = if cfg.seed.is_none() && cfg.cloud_init != Some(false) && !own_storage {
        let iso = cloudinit::generate_seed_iso(
            base,
            &cfg.name,
            cfg.hostname.as_deref(),
            cfg.ci_user.as_deref(),
            &cfg.ssh_keys,
            None,
            &cfg.volumes,
        )?;
        realized = VmConfig {
            seed: Some(iso.to_string_lossy().into_owned()),
            ..cfg.clone()
        };
        &realized
    } else {
        cfg
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
                return Err(e.into());
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
    // Registado JUNTO do pid, e a partir do MESMO pid: é o par que torna o
    // `stop` capaz de distinguir este VMM de um pid reciclado mais tarde.
    vm.pid_starttime = boot.pid.and_then(proc_starttime);
    vm.status = Status::Running;
    vm.restart_policy = cfg.restart_policy.clone();
    vm.namespace = ns.clone();
    vm.ip = boot.ip;
    vm.dhcp_lease_floor = boot.lease_floor;
    vm.backend = backend.id().to_string();
    vm.devices = cfg.devices.clone();
    vm.boot = boot_spec_of(cfg);
    vm.started_unix = Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    // restart_policy HONESTY: only a backend that declares
    // `vm.restart-policy.native` materializes it (libvirt: `<on_crash>restart`
    // in the XML). On Cloud Hypervisor there is no supervisor on the host — warn
    // instead of silently accepting a policy that is not enforced instantly.
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
    st.save(&cfg.name, &vm).map_err(state_err)?;
    Ok(vm)
}

/// `true` if the `restart_policy` requests automatic restart (`always`/`on-failure`)
/// but the backend does NOT declare `vm.restart-policy.native` (ADR-0050) — today
/// only libvirt does, via `<on_crash>restart` in the XML. On the others the
/// restart depends on reconcile/apply — the caller should warn.
///
/// Used to be `backend_id != "libvirt"`: the name match ADR-0044's Context
/// names as the second instance of the leak `VmSpec`/`Extensions` exist to
/// close. A backend that supervises natively under any other name was told it
/// did not; now it says so in its own report and is believed. The policy check
/// comes first so a VM without a policy never costs a report.
pub fn restart_policy_unsupervised(backend_id: &str, policy: Option<&str>) -> bool {
    matches!(policy, Some("always") | Some("on-failure"))
        && !backend_declares(backend_id, Capability::VmRestartPolicyNative)
}

/// Removes a VM: stops the VMM (via its backend), and deletes overlay/state.
///
/// If the backend cleanup fails (e.g. libvirt refuses the undefine), the local
/// record stays **INTACT** and the error propagates — the old version deleted the record
/// anyway and the VM was orphaned in libvirt, invisible to `vm ls`/`vm stop`. It also covers
/// the reverse: no local record but with an orphaned domain in libvirt (an
/// old interrupted `rm`), `remove` cleans up the domain anyway.
pub fn remove(base: &Path, name: &str) -> Result<()> {
    remove_inner(base, name, false, false, &|_| {}).map(|_| ())
}

/// Like [`remove`], but deletes the local state EVEN if the backend cleanup
/// fails (the `vm rm --force`) — the user takes on resolving the rest in libvirt.
pub fn remove_force(base: &Path, name: &str) -> Result<()> {
    remove_inner(base, name, true, false, &|_| {}).map(|_| ())
}

/// What a [`destroy`] took away, and what it deliberately left.
#[derive(Debug, Default, Clone)]
pub struct Destroyed {
    /// Bytes of local files and directories removed (a remote disk is freed by
    /// the node, which does not report a size).
    pub freed_bytes: u64,
    /// Every local artifact removed: overlay, seed, snapshots, sockets, extra disks.
    pub removed: Vec<String>,
    /// The backend id when its storage lives on the provider (a remote node),
    /// so the disks it released are NOT in `removed`/`freed_bytes` — those count
    /// local files only, and reporting «0 B freed» for a VM whose disks and
    /// snapshots went away on the node reads as «nothing was freed».
    pub provider_released: Option<String>,
    /// Things the record points to that are NOT the VM's to delete (a 9p share
    /// is somebody's data; an extra disk outside the state directory may be an
    /// image the operator supplied). Named, never silently left.
    pub kept: Vec<String>,
}

/// The full teardown of a VM: the provider's own destroy (libvirt domain with
/// managed-save/snapshot metadata/NVRAM and the DHCP reservation, or a Proxmox
/// VM purged from the node with its disks), then every local artifact —
/// overlay, seed, sockets, serial log, pid, domain XML, the per-VM directory
/// with its preserved snapshots, and extra disks in the state directory.
///
/// `purge_disks` widens the last step to extra disks OUTSIDE the state
/// directory; without it they are listed in [`Destroyed::kept`]. `force`
/// deletes the local state even when the provider refuses.
pub fn destroy(base: &Path, name: &str, force: bool, purge_disks: bool) -> Result<Destroyed> {
    remove_inner(base, name, force, purge_disks, &|_| {})
}

/// [`destroy`] that reports each [`DestroyStage`] as it starts.
pub fn destroy_with(
    base: &Path,
    name: &str,
    force: bool,
    purge_disks: bool,
    on: &dyn Fn(DestroyStage),
) -> Result<Destroyed> {
    remove_inner(base, name, force, purge_disks, on)
}

fn path_size(p: &Path) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    if md.is_dir() {
        std::fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| path_size(&e.path())).sum())
            .unwrap_or(0)
    } else {
        md.len()
    }
}

fn remove_inner(
    base: &Path,
    name: &str,
    force: bool,
    purge_disks: bool,
    on: &dyn Fn(DestroyStage),
) -> Result<Destroyed> {
    // A name that `create` would refuse cannot exist — and above all, it cannot
    // flow into the paths deleted below (the seed dir's `remove_dir_all`).
    if !valid_vm_name(name) {
        return Err(Error::VmNotFound(name.to_string()));
    }
    let vmdir = vms_dir(base);
    let st = store(base)?;
    let mut record: Option<Vm> = None;
    let mut provider_released: Option<String> = None;
    let existed = match st.load(name) {
        Ok(vm) => {
            // `destroy`, not `stop`: the record is going away, so whatever the
            // backend still owns has to go with it. They are the same call for
            // the local backends (the default), and deliberately not for a
            // remote one, whose disk lives on the node.
            on(DestroyStage::Provider(&vm.backend));
            let backend = backend_for(&vm);
            provider_released = backend
                .as_ref()
                .ok()
                .filter(|b| b.manages_own_storage())
                .map(|b| b.id().to_string());
            if let Err(e) = backend.and_then(|b| b.destroy(&vmdir, &vm).map_err(Error::from)) {
                if !force {
                    return Err(e); // record intact — the rm can be retried
                }
            }
            record = Some(vm);
            true
        }
        Err(_) => {
            // No record: there may be an orphaned libvirt domain with this name —
            // clean it up, and the ingress tap for safety.
            let orphan = libvirt_domain_uri(name).is_some();
            if orphan {
                on(DestroyStage::Provider("libvirt"));
            }
            if let Err(e) = libvirt_cleanup(name) {
                if !force {
                    return Err(e);
                }
            }
            // No record, so no address to trust — the tap goes, the firewall
            // teardown is skipped rather than guessed at. Best effort, as it
            // always was: a process with no network registered has no tap to
            // remove, and the answer to an `rm` of a name that does not exist
            // stays «no such VM», not a complaint about the network.
            if let Ok(net) = network() {
                net.detach_tap(name, None);
            }
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
    let mut out = Destroyed {
        provider_released,
        ..Destroyed::default()
    };
    let rm_file = |out: &mut Destroyed, p: &Path, label: String| {
        let sz = path_size(p);
        if std::fs::remove_file(p).is_ok() {
            out.freed_bytes += sz;
            out.removed.push(label);
        }
    };
    // The overlay first: it is the bulk of what is freed.
    if vmdir.join(format!("{name}.qcow2")).exists() {
        on(DestroyStage::Overlay);
        rm_file(
            &mut out,
            &vmdir.join(format!("{name}.qcow2")),
            format!("{name}.qcow2"),
        );
    }
    // Extra disks and shares named by the record. Only what lives inside the
    // state directory is provably ours; the rest is reported, and removed only
    // when the operator said so.
    if let Some(vm) = &record {
        let root = std::fs::canonicalize(&vmdir).unwrap_or_else(|_| vmdir.clone());
        let mut doomed: Vec<&str> = Vec::new();
        for d in &vm.boot.extra_disks {
            let p = Path::new(&d.source);
            if d.device == "cdrom" || !p.exists() {
                continue;
            }
            let inside = std::fs::canonicalize(p)
                .map(|c| c.starts_with(&root))
                .unwrap_or(false);
            if inside || purge_disks {
                doomed.push(&d.source);
            } else {
                out.kept.push(format!(
                    "{} (extra disk outside the state directory; --purge-disks removes it)",
                    d.source
                ));
            }
        }
        if !doomed.is_empty() {
            on(DestroyStage::ExtraDisks(doomed.len()));
            for src in doomed {
                rm_file(&mut out, Path::new(src), src.to_string());
            }
        }
        for v in &vm.boot.volumes {
            out.kept.push(format!(
                "{} (shared volume — remove it with `volume rm`)",
                v.source
            ));
        }
    }
    // The cloud-init seed directory (`vms/<name>/`, from `generate_seed_iso`)
    // and the preserved snapshots inside it also belong to the VM.
    let dir = vmdir.join(name);
    if dir.exists() {
        on(DestroyStage::SeedAndSnapshots);
        let sz = path_size(&dir);
        if std::fs::remove_dir_all(&dir).is_ok() && sz > 0 {
            out.freed_bytes += sz;
            out.removed.push(format!("{name}/"));
        }
    }
    let leftovers = ["sock", "sock.lock", "serial", "log", "pid", "xml"];
    if leftovers
        .iter()
        .any(|e| vmdir.join(format!("{name}.{e}")).exists())
    {
        on(DestroyStage::RuntimeState);
        for ext in leftovers {
            rm_file(
                &mut out,
                &vmdir.join(format!("{name}.{ext}")),
                format!("{name}.{ext}"),
            );
        }
    }
    on(DestroyStage::Record);
    st.remove(name).map_err(state_err)?;
    // The store's per-record lock file (`.<name>.lock`) is the last trace: it
    // outlived every destroy, so «removes everything» was not quite true.
    // Nothing holds it once the record is gone.
    let _ = std::fs::remove_file(vmdir.join(format!(".{name}.lock")));
    Ok(out)
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
        Err(e) if e.is_not_found() => {
            return match libvirt_domain_uri(name) {
                Some(uri) => libvirt_poweroff(uri, name),
                None => Err(Error::VmNotFound(name.to_string())),
            };
        }
        Err(e) => return Err(state_err(e)),
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
    st.save(name, &vm).map_err(state_err)?;
    // AFTER the save: the vmm is confirmed gone and the record already says
    // so correctly either way — an `Err` from here is a diagnosis on top of a
    // stop that already happened, never a reason to leave the record lying
    // about a dead vmm being `Running`. See `VmBackend::disk_health`.
    Ok(backend.disk_health(&vmdir, &vm)?)
}

/// Loads a VM record, mapping the shared `NotFound` to the VM-specific
/// `VmNotFound` ("no such VM: …") — same idiom as `stop`/`status`.
fn load_vm(base: &Path, name: &str) -> Result<Vm> {
    store(base)?.load(name).map_err(|e| match e.into_root() {
        delonix_model::Error::NotFound(_) => Error::VmNotFound(name.to_string()),
        e => e.into(),
    })
}

/// Suspends a RUNNING VM's vCPUs (see [`VmBackend::pause`]). Refuses a VM
/// that is not currently `Running`, rather than a silent no-op — the caller
/// would otherwise have no way to tell "already paused" from "just paused".
pub fn pause(base: &Path, name: &str) -> Result<()> {
    let vmdir = vms_dir(base);
    let st = store(base)?;
    let mut vm = load_vm(base, name)?;
    if vm.status != Status::Running {
        return Err(Error::NotRunningForOp(format!(
            "VM '{name}' is not running (status: {:?}) — nothing to pause",
            vm.status
        )));
    }
    backend_for(&vm)?.pause(&vmdir, &vm)?;
    vm.status = Status::Paused;
    st.save(name, &vm).map_err(state_err)
}

/// Resumes a VM suspended with [`pause`]. Refuses a VM that is not currently
/// `Paused`.
pub fn unpause(base: &Path, name: &str) -> Result<()> {
    let vmdir = vms_dir(base);
    let st = store(base)?;
    let mut vm = load_vm(base, name)?;
    if vm.status != Status::Paused {
        return Err(Error::NotRunningForOp(format!(
            "VM '{name}' is not paused (status: {:?}) — nothing to resume",
            vm.status
        )));
    }
    backend_for(&vm)?.unpause(&vmdir, &vm)?;
    vm.status = Status::Running;
    st.save(name, &vm).map_err(state_err)
}

/// Whether `h` is a DNS label a guest accepts as its hostname: letters,
/// digits and '-', not at either end, 1 to 63 characters. Pure.
fn valid_hostname(h: &str) -> bool {
    (1..=63).contains(&h.len())
        && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !h.starts_with('-')
        && !h.ends_with('-')
}

/// Whether `u` is a login name cloud-init can create: a lowercase letter or
/// '_' first, then lowercase letters, digits, '_' or '-', at most 32. Pure.
fn valid_login(u: &str) -> bool {
    let mut b = u.bytes();
    matches!(b.next(), Some(c) if c.is_ascii_lowercase() || c == b'_')
        && u.len() <= 32
        && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

/// Changes a STOPPED VM's cloud-init — hostname, user and/or SSH keys — for
/// its next boot (see [`VmBackend::update_cloud_init`]).
///
/// Everything refusable is refused before a backend is asked, with the record
/// untouched: nothing to change, a hostname that is not a DNS label, a user
/// that is not a login name, a key that is empty or spans lines, an appliance
/// (which does not run cloud-init), and a VM that is running or paused. `keys`
/// REPLACE the record's when given; a field not given keeps its value, and the
/// backend receives the whole merged intent. The record is rewritten only
/// after the backend returns `Ok`.
pub fn set_cloud_init(
    base: &Path,
    name: &str,
    hostname: Option<&str>,
    ci_user: Option<&str>,
    keys: Option<Vec<String>>,
) -> Result<Vm> {
    let bad = |m: String| Error::InvalidCloudInitChange(m);
    if hostname.is_none() && ci_user.is_none() && keys.is_none() {
        return Err(bad(format!(
            "nothing to change in VM '{name}''s cloud-init: give --hostname, --user and/or --ssh-key"
        )));
    }
    if let Some(h) = hostname.filter(|h| !valid_hostname(h)) {
        return Err(bad(format!(
            "hostname '{h}' is not a DNS label (letters, digits, '-', not at either end, at most 63)"
        )));
    }
    if let Some(u) = ci_user.filter(|u| !valid_login(u)) {
        return Err(bad(format!("user '{u}' is not a login name")));
    }
    if let Some(k) = &keys {
        if k.is_empty() {
            return Err(bad("give at least one --ssh-key".to_string()));
        }
        if let Some((i, _)) = k
            .iter()
            .enumerate()
            .find(|(_, key)| key.trim().is_empty() || key.contains('\n'))
        {
            return Err(bad(format!("ssh key #{} is empty or spans lines", i + 1)));
        }
    }
    let vmdir = vms_dir(base);
    let st = store(base)?;
    let mut vm = load_vm(base, name)?;
    if vm.boot.cloud_init == Some(false) {
        return Err(bad(format!(
            "VM '{name}' runs an appliance image, which does not run cloud-init"
        )));
    }
    if matches!(vm.status, Status::Running | Status::Paused) {
        return Err(Error::CloudInitNeedsStopped(format!(
            "VM '{name}' is {:?}: the guest reads cloud-init at boot — stop it first (`delonix vm stop {name}`)",
            vm.status
        )));
    }
    let intent = CloudInitIntent {
        hostname: hostname
            .map(str::to_string)
            .or_else(|| vm.boot.hostname.clone()),
        ci_user: ci_user
            .map(str::to_string)
            .or_else(|| vm.boot.ci_user.clone()),
        ssh_keys: keys
            .map(|k| k.into_iter().map(|s| s.trim().to_string()).collect())
            .unwrap_or_else(|| vm.boot.ssh_keys.clone()),
    };
    backend_for(&vm)?.update_cloud_init(&vmdir, &vm, &intent)?;
    vm.boot.hostname = intent.hostname;
    vm.boot.ci_user = intent.ci_user;
    vm.boot.ssh_keys = intent.ssh_keys;
    st.save(name, &vm).map_err(state_err)?;
    Ok(vm)
}

/// Changes a STOPPED VM's vCPUs and/or memory for its next boot — the cold
/// resize (`vm.resize.cold`, see [`VmBackend::resize_cold`]).
///
/// Everything that can be refused is refused before the backend is asked:
/// nothing to change, zero vCPUs, a memory value that does not parse (the
/// lenient [`mem_mib`] would read `2GB` as 1 GiB and this would report it
/// done), and a VM that is running or paused — a guest that only sees the
/// change after its next reboot has not been resized yet. The record is
/// rewritten only after the backend returns `Ok`, so a refused or failed
/// resize leaves it saying what the VM actually has.
///
/// Returns the updated record.
pub fn resize(base: &Path, name: &str, vcpus: Option<u32>, memory: Option<&str>) -> Result<Vm> {
    if vcpus.is_none() && memory.is_none() {
        return Err(Error::InvalidResize(format!(
            "nothing to resize on VM '{name}': give --vcpus and/or --memory"
        )));
    }
    if vcpus == Some(0) {
        return Err(Error::InvalidResize(format!(
            "VM '{name}' cannot have 0 vCPUs"
        )));
    }
    let new_mib = match memory {
        Some(m) => Some(parse_mem_mib(m).ok_or_else(|| {
            Error::InvalidResize(format!(
                "memory '{m}' is not a size: use a number with an optional M/G suffix (512M, 4G, 4Gi)"
            ))
        })?),
        None => None,
    };
    let vmdir = vms_dir(base);
    let st = store(base)?;
    let mut vm = load_vm(base, name)?;
    if matches!(vm.status, Status::Running | Status::Paused) {
        return Err(Error::ResizeNeedsStopped(format!(
            "VM '{name}' is {:?}: `vm resize` is a cold resize — stop it first (`delonix vm stop {name}`)",
            vm.status
        )));
    }
    let target_vcpus = vcpus.unwrap_or(vm.vcpus.max(1));
    let target_mib = new_mib.unwrap_or_else(|| mem_mib(&vm.memory));
    backend_for(&vm)?.resize_cold(&vmdir, &vm, target_vcpus, target_mib)?;
    vm.vcpus = target_vcpus;
    if let Some(m) = memory {
        vm.memory = m.trim().to_string();
    }
    st.save(name, &vm).map_err(state_err)?;
    Ok(vm)
}

/// Moves VM `name` to `target`, another node of its cluster (`vm move --node`,
/// ADR-0053 decision 1; see [`VmBackend::move_to_node`]).
///
/// The target is always the caller's: there is no default and no selection.
/// Refused before the backend is asked: an empty target, and a power state
/// that does not match `live` as the record says it — `--live` on a stopped
/// VM, no `--live` on a running one, and a paused VM either way (unpause it or
/// stop it first). The backend asks its node the same question again, because
/// a record can be out of date. A target storage without `--with-local-disks`
/// is refused too: it names where COPIED disks land, and without the flag
/// nothing is copied. The record takes the handle the backend returns only
/// after the move is proved, so a refused or failed move leaves it naming the
/// node the VM is still on.
///
/// Returns the updated record.
pub fn move_to_node(base: &Path, name: &str, target: &str, opts: &MoveOptions) -> Result<Vm> {
    let target = target.trim();
    if target.is_empty() {
        return Err(Error::InvalidMoveTarget(format!(
            "no node to move VM '{name}' to: give `--node <node>`"
        )));
    }
    match opts.target_storage.as_deref().map(str::trim) {
        Some("") => {
            return Err(Error::InvalidMoveTarget(format!(
                "an empty target storage for VM '{name}': name a storage of node '{target}'"
            )))
        }
        Some(_) if !opts.with_local_disks => {
            return Err(Error::InvalidMoveTarget(format!(
                "`--target-storage` names where copied disks land, and without \
                 `--with-local-disks` VM '{name}' has none copied"
            )))
        }
        _ => {}
    }
    let live = opts.live;
    let vmdir = vms_dir(base);
    let st = store(base)?;
    let mut vm = load_vm(base, name)?;
    if let Some(why) = move_power_refusal(&vm.status, live) {
        return Err(Error::MoveRefused(format!("VM '{name}' {why}")));
    }
    let handle = backend_for(&vm)?.move_to_node(&vmdir, &vm, target, opts)?;
    vm.api_socket = handle;
    st.save(name, &vm).map_err(state_err)?;
    Ok(vm)
}

/// Why a move of a VM in `status` is refused for `live`, or `None`. Pure.
fn move_power_refusal(status: &Status, live: bool) -> Option<String> {
    match (status, live) {
        (Status::Paused, _) => Some(
            "is paused: a move needs it running (`--live`) or stopped — unpause it or stop it first"
                .into(),
        ),
        (Status::Running, false) => Some(
            "is running: move it with `--live`, or stop it first for an offline move".into(),
        ),
        (Status::Running, true) => None,
        (_, true) => Some(
            "is not running: `--live` moves a running VM — drop `--live` for an offline move".into(),
        ),
        (_, false) => None,
    }
}

/// Takes a named snapshot of VM `name` (see [`VmBackend::snapshot`]). On libvirt a
/// running VM's snapshot is a system checkpoint (memory + disk).
pub fn snapshot(base: &Path, name: &str, snap: &str) -> Result<()> {
    if !valid_vm_name(snap) {
        return Err(Error::InvalidSnapshotName(format!(
            "invalid snapshot name: {snap}"
        )));
    }
    let vmdir = vms_dir(base);
    let vm = load_vm(base, name)?;
    Ok(backend_for(&vm)?.snapshot(&vmdir, &vm, snap)?)
}

/// Reverts VM `name` to the named snapshot (see [`VmBackend::restore`]).
pub fn restore(base: &Path, name: &str, snap: &str) -> Result<()> {
    if !valid_vm_name(snap) {
        return Err(Error::InvalidSnapshotName(format!(
            "invalid snapshot name: {snap}"
        )));
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
        return Err(Error::LiveBackupNeedsLibvirt(format!(
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
            Error::LiveBackupFailed(format!("live backup: cannot list the disks of {name}: {e}"))
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
            Error::LiveBackupFailed(format!(
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
    .map_err(|e| Error::LiveBackupFailed(format!("live backup: cannot stage the overlay: {e}")))?;

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
        Error::LiveBackupFailed(format!(
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
        .map_err(|e| Error::LiveBackupFailed(format!("live backup: copying {source}: {e}")));

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
                return Err(Error::LiveBackupFailed(format!(
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
            return Err(Error::LiveBackupFailed(format!(
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
    Ok(backend_for(&vm)?.snapshots(&vms_dir(base), &vm)?)
}

/// Deletes VM `name`'s snapshot `snap` (see [`VmBackend::delete_snapshot`]).
pub fn delete_snapshot(base: &Path, name: &str, snap: &str) -> Result<()> {
    if !valid_vm_name(snap) {
        return Err(Error::InvalidSnapshotName(format!(
            "invalid snapshot name: {snap}"
        )));
    }
    let vmdir = vms_dir(base);
    let vm = load_vm(base, name)?;
    Ok(backend_for(&vm)?.delete_snapshot(&vmdir, &vm, snap)?)
}

/// Applies one direction of VM `name`'s own firewall (see
/// [`VmBackend::apply_firewall`]).
pub fn apply_firewall(base: &Path, name: &str, policy: &firewall::Policy) -> Result<()> {
    let vmdir = vms_dir(base);
    let vm = load_vm(base, name)?;
    Ok(backend_for(&vm)?.apply_firewall(&vmdir, &vm, policy)?)
}

/// Reads one direction of VM `name`'s own firewall back (see
/// [`VmBackend::read_firewall`]).
pub fn read_firewall(
    base: &Path,
    name: &str,
    direction: firewall::Direction,
) -> Result<firewall::Policy> {
    let vmdir = vms_dir(base);
    let vm = load_vm(base, name)?;
    Ok(backend_for(&vm)?.read_firewall(&vmdir, &vm, direction)?)
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
        // Consumido em `prepare_local_overlay` no momento da criação: depois de
        // o overlay existir, o tamanho é uma propriedade DELE e não há nada a
        // reaplicar no arranque (e `prepare_local_overlay` salta um overlay que
        // já existe). Por isso não entra no `VmBootSpec`.
        disk_size_gib: _,
        // A request-time gate (ADR-0050 D6), consumed in `create_with` BEFORE
        // the backend is chosen: once the record names a backend that passed
        // it, there is nothing to reapply on `vm start`, and the record does
        // not keep it.
        required_capabilities: _,
        // Everything below used to exist only for the duration of `vm create`.
        kernel,
        initrd,
        firmware,
        cmdline,
        seed,
        hostname,
        ci_user,
        ssh_keys,
        cloud_init,
        hugepages,
        cpu_affinity,
        bridge,
        volumes,
        vnc,
        serial_capture,
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
        hostname: hostname.clone(),
        ci_user: ci_user.clone(),
        ssh_keys: ssh_keys.clone(),
        cloud_init: *cloud_init,
        hugepages: *hugepages,
        cpu_affinity: cpu_affinity.clone(),
        bridge: bridge.clone(),
        volumes: volumes.clone(),
        vnc: *vnc,
        serial_capture: *serial_capture,
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
        // Not persisted: the requirement was checked against the backend the
        // record names when the VM was created (see `boot_spec_of`).
        required_capabilities: Vec::new(),
        // For libvirt, `Vm.tap` is not a real tap: `LibvirtBackend::boot` stores
        // the net mode string there. For Cloud Hypervisor it IS a device name
        // and must not be misread as one.
        net_mode: (vm.backend == "libvirt").then(|| vm.tap.clone()),
        // `None` de propósito: o overlay já tem o tamanho com que nasceu, e um
        // restart não o redimensiona. Reafirmar aqui um número seria convidar um
        // resize acidental em cada arranque.
        disk_size_gib: None,
        kernel: b.kernel.clone(),
        initrd: b.initrd.clone(),
        firmware: b.firmware.clone(),
        cmdline: b.cmdline.clone(),
        seed: b.seed.clone(),
        hostname: b.hostname.clone(),
        ci_user: b.ci_user.clone(),
        ssh_keys: b.ssh_keys.clone(),
        cloud_init: b.cloud_init,
        hugepages: b.hugepages,
        cpu_affinity: b.cpu_affinity.clone(),
        bridge: b.bridge.clone(),
        volumes: b.volumes.clone(),
        vnc: b.vnc,
        serial_capture: b.serial_capture,
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
    let vm = st.load(name).map_err(|e| match e.into_root() {
        delonix_model::Error::NotFound(n) => Error::VmNotFound(n),
        e => e.into(),
    })?;
    create(base, &config_from(&vm))
}

/// Stops (if running) then starts — always a real reboot, unlike `start`
/// (which no-ops when already running). Same recovered-fields caveat as
/// `start`/[`config_from`].
pub fn restart(base: &Path, name: &str) -> Result<Vm> {
    let st = store(base)?;
    let vm = st.load(name).map_err(|e| match e.into_root() {
        delonix_model::Error::NotFound(n) => Error::VmNotFound(n),
        e => e.into(),
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
    st.load(name).map_err(|e| match e.into_root() {
        delonix_model::Error::NotFound(n) => Error::VmNotFound(n),
        e => e.into(),
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
    let named = st.load(name).map_err(state_err)?;
    let backend = backend_for(&named)?;
    st.update(name, |vm| {
        let old_ip = vm.ip.clone();
        let old_status = vm.status.clone();
        // Records written before `pid_starttime` existed carry `None`, which
        // `safe_to_signal` treats as the old behaviour — so the guard is inert
        // for every VM already on disk until it is booted again. Adopt it here,
        // where we are already reading the process, and only on proof.
        let adopted = adopt_pid_starttime(vm);
        if backend.is_running(vm) {
            // A PAUSED VMM is still "alive" to `is_running` (the process is
            // there, only its vCPUs are frozen) — a routine `vm ls`/`status()`
            // must not silently thaw the record back to `Running` just
            // because the process answers. Only `unpause`/`stop` move it out
            // of `Paused`; measured live: without this guard, `vm ls` right
            // after `vm pause` reported `Running` again, and a second `vm
            // pause` failed against a VMM that was never actually paused.
            if vm.status != Status::Paused {
                vm.status = Status::Running;
            }
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
        //
        // The status comparison is on the WHOLE status, not on "was it
        // Running": `Paused -> Stopped` (a paused VMM that really died) is a
        // change too, and the old `was_running != is_running` saw both sides as
        // "not running" and left `Paused` on disk while `vm ls` said `Stopped`
        // — so `vm unpause` went on to aim at a VM that no longer existed.
        // A remote backend that found the VM on another node of its cluster
        // (ADR-0053 decision 3) says so here; the record takes the new handle,
        // or every later command would ask the old node again.
        let relocated = match backend.current_handle(vm) {
            Some(h) if h != vm.api_socket => {
                vm.api_socket = h;
                true
            }
            _ => false,
        };
        adopted || relocated || vm.ip != old_ip || vm.status != old_status
    })
    .map_err(state_err)
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
    for vm in st.list().map_err(state_err)? {
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

    /// The re-created VM. Real `virsh net-dhcp-leases default` rows captured
    /// on 2026-09-15 for `procsys-fix`, deleted and re-created twice under the
    /// same name (so the same `mac_for` MAC) — one lease per incarnation, each
    /// with its own client id, none released by `delete vm`.
    #[test]
    fn a_recreated_vm_never_reports_the_previous_incarnations_lease() {
        let mac = "52:54:00:d5:15:98";
        let before_boot = "\
 Expiry Time           MAC address         Protocol   IP address           Hostname          Client ID or DUID
-------------------------------------------------------------------------------------------------------------------------------------------------------
 2026-09-15 21:53:05   52:54:00:d5:15:98   ipv4       192.168.122.223/24   -                 ff:56:50:4d:98:00:02:00:00:ab:11:66:6c:2c:6a:54:f1:70:e4
 2026-09-15 22:08:07   52:54:00:83:78:cb   ipv4       192.168.122.81/24    nrcni             ff:56:50:4d:98:00:02:00:00:ab:11:8d:4c:f5:42:eb:1a:77:f4";
        let floor = leases_max_expiry(before_boot, mac);
        assert_eq!(floor.as_deref(), Some("2026-09-15 21:53:05"));

        // The new guest has not asked for an address yet: the only lease is the
        // dead VM's. This is what `vm create --wait` announced as the new IP.
        assert_eq!(
            pick_lease_ip(before_boot, mac, None),
            LeasePick::Current("192.168.122.223".into()),
            "without a floor the stale lease is indistinguishable — the bug"
        );
        assert_eq!(
            pick_lease_ip(before_boot, mac, floor.as_deref()),
            LeasePick::OnlyStale
        );

        // The new guest leases `.224`: reported, and the stale row still ignored.
        let after_lease = format!(
            "{before_boot}\n 2026-09-15 22:01:37   52:54:00:d5:15:98   ipv4       192.168.122.224/24   -                 ff:56:50:4d:98:00:02:00:00:ab:11:ab:5f:89:ae:2a:5f:bb:ea"
        );
        assert_eq!(
            pick_lease_ip(&after_lease, mac, floor.as_deref()),
            LeasePick::Current("192.168.122.224".into())
        );
        // Another VM's leases are never borrowed.
        assert_eq!(
            pick_lease_ip(before_boot, "52:54:00:aa:bb:cc", floor.as_deref()),
            LeasePick::NoLease
        );
    }

    /// `vm stop` + `vm start`: the guest keeps its address and renews it. The
    /// renewal carries a new expiry, above the floor, so it is reported.
    #[test]
    fn a_restarted_vm_that_renews_the_same_address_is_still_reported() {
        let mac = "52:54:00:d5:15:98";
        let old = "2026-09-15 21:53:05 52:54:00:d5:15:98 ipv4 192.168.122.223/24 host -";
        let floor = leases_max_expiry(old, mac);
        let renewed = "2026-09-15 22:40:11 52:54:00:d5:15:98 ipv4 192.168.122.223/24 host -";
        assert_eq!(
            pick_lease_ip(old, mac, floor.as_deref()),
            LeasePick::OnlyStale
        );
        assert_eq!(
            pick_lease_ip(renewed, mac, floor.as_deref()),
            LeasePick::Current("192.168.122.223".into())
        );
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
    fn a_session_domain_loses_the_ceilings_it_cannot_have_and_keeps_pinning() {
        let xml = "  <memtune>\n    <hard_limit unit='KiB'>1</hard_limit>\n  </memtune>\n  <cputune>\n    <period>100000</period>\n    <quota>50000</quota>\n  </cputune>\n  <cputune>\n    <period>100000</period>\n    <vcpupin vcpu='0' cpuset='1'/>\n  </cputune>\n";
        let out = strip_cgroup_tuning(xml);
        assert!(!out.contains("memtune") && !out.contains("quota") && !out.contains("period"));
        assert!(out.contains("vcpupin"));
        assert_eq!(
            out.matches("<cputune>").count(),
            1,
            "the empty one is gone: {out}"
        );
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
    fn http_status_is_2xx_reads_the_real_shapes_cloud_hypervisor_sends() {
        assert!(super::http_status_is_2xx("HTTP/1.1 204 No Content"));
        assert!(super::http_status_is_2xx("HTTP/1.1 200 OK"));
        assert!(!super::http_status_is_2xx(
            "HTTP/1.1 500 Internal Server Error"
        ));
        assert!(!super::http_status_is_2xx("HTTP/1.1 400 Bad Request"));
        // No response at all (connection reset before a full status line).
        assert!(!super::http_status_is_2xx(""));
        assert!(!super::http_status_is_2xx("garbage"));
    }

    /// `qemu-img`/`qemu-io` are not guaranteed on every CI runner (confirmed
    /// absent there, not just some subcommand failing) — same guard the
    /// `delonix-runtime-bin` `vmimage` tests already use for the same tool:
    /// `.status()` mapped to `false` on any error, never `.expect(...)`, so a
    /// host without the tool skips silently instead of failing the suite.
    fn run_ok(prog: &str, args: &[&str]) -> bool {
        std::process::Command::new(prog)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// BUG-VM-001: a clean, freshly-created qcow2 must never read as
    /// corrupted — `disk_looks_corrupt` exists to add a diagnosis on top of a
    /// REAL positive, and a false positive here would fail every `vm stop`.
    #[test]
    fn disk_looks_corrupt_says_no_for_a_clean_image() {
        let path =
            std::env::temp_dir().join(format!("dlx-diskhealth-clean-{}.qcow2", std::process::id()));
        let _ = std::fs::remove_file(&path);
        if !run_ok(
            "qemu-img",
            &["create", "-f", "qcow2", &path.to_string_lossy(), "16M"],
        ) {
            return; // qemu-img absent: this host cannot run the check either
        }
        assert!(
            !super::disk_looks_corrupt(&path),
            "a fresh image is not corrupt"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A genuinely corrupted qcow2 has to read as corrupt — this is the exact
    /// signal `VmBackend::disk_health` relies on to surface BUG-VM-001
    /// immediately instead of silently. Real data is written first (so
    /// clusters and L2 entries actually exist), then the file is truncated
    /// short — reproducing, without hand-crafting qcow2 internals, the exact
    /// error class measured live on this host ("counting reference for a
    /// region exceeding the end of the file"), not a stand-in for it.
    #[test]
    fn disk_looks_corrupt_says_yes_for_a_truncated_image() {
        let path = std::env::temp_dir().join(format!(
            "dlx-diskhealth-truncated-{}.qcow2",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        if !run_ok(
            "qemu-img",
            &["create", "-f", "qcow2", &path.to_string_lossy(), "16M"],
        ) {
            return; // qemu-img absent: this host cannot run the check either
        }
        if !run_ok(
            "qemu-io",
            &["-c", "write -P 0x5a 0 1M", &path.to_string_lossy()],
        ) {
            std::fs::remove_file(&path).ok();
            return; // qemu-io absent: same reasoning
        }
        let len = std::fs::metadata(&path).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(len - 65536).unwrap(); // one cluster short — the same
                                         // shape as a write cut off mid-flight
        drop(f);
        assert!(
            super::disk_looks_corrupt(&path),
            "a truncated image with real allocated data has to read as corrupt"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A path that does not exist must not panic, and must not read as
    /// corrupt — `disk_looks_corrupt` only answers `true` on a CONFIRMED
    /// positive (`qemu-img check` exit code 2), never on "could not tell".
    #[test]
    fn disk_looks_corrupt_tolerates_a_missing_file() {
        let path =
            std::env::temp_dir().join(format!("dlx-diskhealth-missing-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(!super::disk_looks_corrupt(&path));
    }

    /// The default `VmBackend::disk_health` (libvirt, and any future backend
    /// that never overrides it) has nothing to check and must never fail
    /// `stop` on a made-up diagnosis.
    #[test]
    fn disk_health_default_is_a_no_op() {
        struct Nothing;
        impl VmBackend for Nothing {
            fn id(&self) -> &'static str {
                "nothing"
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
            ) -> delonix_model::Result<Boot> {
                unimplemented!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                Ok(())
            }
        }
        let vm = Vm::new(
            "x".into(),
            "d".into(),
            "o".into(),
            1,
            "1G".into(),
            "n".into(),
            "tap".into(),
            "mac".into(),
            "sock".into(),
        );
        assert!(Nothing.disk_health(Path::new("/tmp"), &vm).is_ok());
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

    /// REGRESSION (reproduced on libvirt): `vm pause` left the domain
    /// `paused` and the next `vm ls` said `Stopped`, because only `running`
    /// counted as alive and the `Paused` guard in `status()` lives inside the
    /// alive branch. Removing the `|| s == "paused"` makes this test fail.
    #[test]
    fn a_paused_libvirt_domain_is_alive() {
        assert!(libvirt_domstate_is_alive("running"));
        assert!(
            libvirt_domstate_is_alive("paused"),
            "a paused domain keeps the guest memory intact — it is not a powered-off VM"
        );
        for dead in ["shut off", "crashed", ""] {
            assert!(!libvirt_domstate_is_alive(dead), "{dead:?} is not alive");
        }
    }

    /// `status()` reconciling a `Paused` record, both halves: with the VMM
    /// alive `Paused` stays (no silent thaw of the record), and with the VMM
    /// dead `Stopped` is WRITTEN to disk — it used to be only returned, because
    /// change detection compared "was it Running?" and `Paused` and `Stopped`
    /// both answer "no". The disk kept `Paused` while `vm ls` said `Stopped`,
    /// and `vm unpause` then aimed at a VM that no longer existed.
    #[test]
    fn status_of_a_paused_vm_keeps_paused_and_persists_its_death() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static ALIVE: AtomicBool = AtomicBool::new(true);

        struct Pausable;
        impl VmBackend for Pausable {
            fn id(&self) -> &'static str {
                "pausavel"
            }
            fn available(&self) -> bool {
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
            ) -> delonix_model::Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                ALIVE.load(Ordering::SeqCst)
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                Ok(())
            }
        }
        register_backend(BackendRegistration {
            id: "pausavel",
            aliases: &[],
            auto_selectable: false,
            report: crate::capabilities::undeclared("fake"),
            new: Box::new(|| Ok(Box::new(Pausable))),
        })
        .expect("registar");

        let base = std::env::temp_dir().join(format!(
            "delonix-paused-status-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(vms_dir(&base)).unwrap();
        let st = store(&base).unwrap();
        let mut vm = Vm::new(
            "p".into(),
            "d".into(),
            "o".into(),
            1,
            "1G".into(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        );
        vm.backend = "pausavel".into();
        vm.status = Status::Paused;
        st.save("p", &vm).unwrap();

        ALIVE.store(true, Ordering::SeqCst);
        assert_eq!(status(&base, "p").unwrap().status, Status::Paused);
        assert_eq!(st.load("p").unwrap().status, Status::Paused);

        ALIVE.store(false, Ordering::SeqCst);
        assert_eq!(status(&base, "p").unwrap().status, Status::Stopped);
        assert_eq!(
            st.load("p").unwrap().status,
            Status::Stopped,
            "`vm ls` said Stopped while the record on disk stayed Paused"
        );

        let _ = std::fs::remove_dir_all(&base);
        backends().write().unwrap().retain(|b| b.id != "pausavel");
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
                // DX-4501 is «no such VM» — the variant this test pins, asked by
                // its dictionary number instead of by a pattern a wrapped error
                // would stop matching.
                Err(e) => {
                    assert_eq!(e.number(), 4501, "{e}");
                    assert!(e.to_string().contains("nope"), "{e}");
                }
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
        // 8 GiB available, no reserve: a 1 PB VM never fits, a 1 MiB VM always does —
        // on any machine, because the numbers are the test's and not the host's.
        assert!(
            admission_verdict(&test_vm_cfg("1000000G"), Some(8192), Some("0")).is_err(),
            "giant VM must be refused"
        );
        assert!(
            admission_verdict(&test_vm_cfg("1M"), Some(8192), Some("0")).is_ok(),
            "tiny VM must be admitted"
        );
        // The default reserve (2 GiB) applies when the variable is absent or garbage.
        assert!(admission_verdict(&test_vm_cfg("7G"), Some(8192), None).is_err());
        assert!(admission_verdict(&test_vm_cfg("7G"), Some(8192), Some("não")).is_err());
        // No readable MemAvailable: best-effort no-op, as before.
        assert!(admission_verdict(&test_vm_cfg("1000000G"), None, None).is_ok());
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

    /// O tamanho virtual lê-se dos BYTES entre parênteses, não do número
    /// humano: `2.2 GiB` é arredondado, e uma quota comparada com um número
    /// arredondado é uma quota que deixa passar o que queria recusar.
    #[test]
    fn tamanho_virtual_le_se_dos_bytes_e_nao_do_numero_humano() {
        let info = "image: g.qcow2\nfile format: qcow2\nvirtual size: 10 GiB (10737418240 bytes)\ndisk size: 690 MiB\n";
        assert_eq!(parse_qemu_virtual_size_bytes(info), Some(10_737_418_240));

        // 2.2 GiB arredondado esconde 161 MiB — daí ler os bytes.
        let arred = "virtual size: 2.2 GiB (2361393152 bytes)\n";
        assert_eq!(parse_qemu_virtual_size_bytes(arred), Some(2_361_393_152));

        // Sem os parênteses (formatos antigos), não se inventa um número.
        assert_eq!(parse_qemu_virtual_size_bytes("virtual size: 8 MiB\n"), None);
        assert_eq!(parse_qemu_virtual_size_bytes("file format: qcow2\n"), None);
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

        // ADR-0054 §3: a name this process has not registered is kept, not
        // dropped — so selecting it fails instead of falling through to a
        // local hypervisor.
        std::fs::write(default_backend_file(&dir), "Nave-Remota\n").unwrap();
        assert_eq!(get_default_backend(&dir).as_deref(), Some("nave-remota"));
        assert!(select_backend(get_default_backend(&dir).as_deref()).is_err());

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

    /// Live proof, against this machine's `qemu:///system`. `#[ignore]` — it
    /// needs a running libvirt and permission to define, so it stays out of the
    /// normal battery; run it with
    /// `cargo test -p delonix-vm -- --ignored antispoof_live`.
    ///
    /// It exists because both design decisions came from MEASURED daemon
    /// behaviour (the uuid-dependent define, and the second `filterref` dropped
    /// in silence), and an assertion about strings proves neither.
    #[test]
    #[ignore]
    fn antispoof_live_defines_and_survives_the_domain() {
        let uri = "qemu:///system";
        if super::capture("virsh", &["-c", uri, "nwfilter-list"]).is_none() {
            eprintln!("{uri} unreachable — nothing to prove here");
            return;
        }
        // 1. The define is idempotent: three times running, no error.
        for _ in 0..3 {
            super::ensure_antispoof_filter(uri).expect("nwfilter-define");
        }
        // 2. The domain the production code generates really does define...
        let mut cfg = test_vm_cfg("128M");
        cfg.name = "dlx-itest-antispoof".into();
        cfg.net_mode = Some("nat".into());
        let xml =
            super::libvirt_domain_xml(&cfg, "/var/tmp/dlx-itest.qcow2", &super::mac_for(&cfg.name));
        assert!(super::virsh_define_xml(
            uri,
            "define",
            "itest-antispoof",
            &xml
        ));
        // 3. ...and libvirt KEPT the reference to the filter.
        let dumped =
            super::capture("virsh", &["-c", uri, "dumpxml", "--", &cfg.name]).expect("dumpxml");
        let _ = super::quiet("virsh", &["-c", uri, "undefine", "--", &cfg.name]);
        assert!(
            dumped.contains(super::ANTISPOOF_FILTER),
            "the filter did not survive the define: {dumped}"
        );
    }

    #[test]
    fn filterref_only_where_libvirt_can_apply_it() {
        // nat/network/bridge give the guest a tap — libvirt has something to
        // apply the filter to.
        assert_eq!(
            super::libvirt_filterref_xml(Some("nat")),
            super::ANTISPOOF_FILTERREF
        );
        assert_eq!(
            super::libvirt_filterref_xml(Some("network")),
            super::ANTISPOOF_FILTERREF
        );
        assert_eq!(
            super::libvirt_filterref_xml(Some("bridge")),
            super::ANTISPOOF_FILTERREF
        );
        // `user` (SLIRP/passt) has no tap. Emitting there would be accepted and
        // ignored — exactly what this repo has already corrected three times.
        assert_eq!(super::libvirt_filterref_xml(Some("user")), "");
        assert_eq!(super::libvirt_filterref_xml(None), "");
    }

    #[test]
    fn the_emitted_name_cannot_drift_from_the_defined_one() {
        // Two literals, one name: rename one and every domain references a
        // filter `nwfilter-define` never created, so `virsh define` refuses
        // EVERY VM. Cheap to pin here.
        assert!(super::ANTISPOOF_FILTERREF.contains(super::ANTISPOOF_FILTER));
        assert!(super::antispoof_filter_xml(super::ANTISPOOF_FILTER_UUID)
            .contains(super::ANTISPOOF_FILTER));
    }

    #[test]
    fn the_uuid_round_trips_or_the_define_is_not_idempotent() {
        // Measured: `nwfilter-define` refuses a name that already exists under
        // a DIFFERENT uuid. Without carrying the right one, the second VM on
        // the machine failed to create.
        let xml = super::antispoof_filter_xml(super::ANTISPOOF_FILTER_UUID);
        assert!(xml.contains(&format!("<uuid>{}</uuid>", super::ANTISPOOF_FILTER_UUID)));
        // And the uuid MUST come out as it went in — that is what turns the
        // define into an update instead of a collision.
        let other = "abcdef01-2345-4678-89ab-cdef01234567";
        assert!(super::antispoof_filter_xml(other).contains(other));
        // Round trip: what we write is what we know how to read back.
        assert_eq!(
            super::parse_nwfilter_uuid(&super::antispoof_filter_xml(other)).as_deref(),
            Some(other)
        );
        assert_eq!(super::parse_nwfilter_uuid("<filter/>"), None);
    }

    #[test]
    fn the_filter_is_anti_spoofing_and_not_an_l2_firewall() {
        let xml = super::antispoof_filter_xml(super::ANTISPOOF_FILTER_UUID);
        // What it promises:
        assert!(xml.contains("no-mac-spoofing"));
        assert!(xml.contains("no-arp-mac-spoofing"));
        // What it must not gain by accident — each with the measured cost:
        //  · clean-traffic / no-other-l2-traffic end in a blanket drop and
        //    would throw away ALL of the guest's IPv6;
        //  · no-ip-spoofing pins the source IPv4 and would black-hole the
        //    egress of every pod on a DKS node running a CNI.
        assert!(!xml.contains("clean-traffic"), "clean-traffic drops IPv6");
        assert!(
            !xml.contains("no-other-l2-traffic"),
            "blanket drop at priority 1000"
        );
        assert!(!xml.contains("no-ip-spoofing"), "breaks a DKS node's CNI");
        assert!(
            !xml.contains("no-ipv6-spoofing"),
            "the ipv6-ip chain ends in a drop"
        );
    }

    #[test]
    fn the_domain_carries_exactly_one_filterref() {
        // Measured against libvirt 10.0.0: an `<interface>` with TWO
        // `filterref` is accepted by `define` with success and the second is
        // DISCARDED in silence. That is why the filter is composed on the
        // libvirt side and only one reference leaves here — if two ever did,
        // half the protection would stop existing with nobody noticing.
        let mut cfg = test_vm_cfg("1G");
        cfg.net_mode = Some("nat".into());
        let xml = super::libvirt_domain_xml(&cfg, "/x.qcow2", "52:54:00:aa:bb:cc");
        assert_eq!(xml.matches("<filterref").count(), 1, "{xml}");
        assert!(xml.contains("</interface>"));
    }

    #[test]
    fn user_mode_carries_no_filterref_at_all() {
        let mut cfg = test_vm_cfg("1G");
        cfg.net_mode = Some("user".into());
        let xml = super::libvirt_domain_xml(&cfg, "/x.qcow2", "52:54:00:aa:bb:cc");
        assert_eq!(xml.matches("<filterref").count(), 0, "{xml}");
    }

    /// The seed takes the virtio letter AFTER the extra disks: adding cloud-init
    /// to a VM must never rename a disk the guest already has.
    #[test]
    fn the_seed_takes_the_virtio_letter_after_the_extra_disks() {
        let mut c = hpc_cfg();
        c.seed = Some("/seed.iso".into());
        let xml = libvirt_domain_xml(&c, "/v.qcow2", "52:54:00:ab:cd:ef");
        assert!(xml.contains("dev='vdb' bus='virtio'"), "no extras: {xml}");

        c.extra_disks = vec![ExtraDisk {
            source: "/data.qcow2".into(),
            ..Default::default()
        }];
        let xml = libvirt_domain_xml(&c, "/v.qcow2", "52:54:00:ab:cd:ef");
        let extra = xml.find("/data.qcow2").expect("extra disk");
        let seed = xml.find("/seed.iso").expect("seed");
        assert!(
            xml[extra..seed].contains("dev='vdb'") || xml[..seed].contains("dev='vdb'"),
            "{xml}"
        );
        assert!(
            xml.contains("dev='vdc' bus='virtio'"),
            "seed after extra: {xml}"
        );
        assert_eq!(xml.matches("dev='vdb'").count(), 1, "{xml}");
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
        // The seed is a read-only virtio disk, NOT a SATA cdrom: the Debian
        // `-cloud` kernel has no SATA driver and would never see it.
        assert!(!xml.contains("device='cdrom'"), "{xml}");
        assert!(!xml.contains("bus='sata'"), "{xml}");
        assert!(
            xml.contains("<disk type='file' device='disk' snapshot='no'>"),
            "{xml}"
        );
        assert!(xml.contains("/seed.iso"), "{xml}");
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
    /// existed must keep deserializing) lives in `delonix-compute`, where
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

    /// ADR-0044 D3 rule 3: the two predicates ask the backend's REPORT, never
    /// its name. A fake registered under a name that is neither `libvirt` nor
    /// `cloud-hypervisor`, declaring both entries, gets both answers — and the
    /// same fake with an empty declaration gets neither. With the old
    /// `backend_id == "…"` matches this test fails on the first assertion.
    #[test]
    fn the_predicates_read_the_report_not_the_backend_name() {
        register_backend(fake(
            "declares-both",
            false,
            &[
                Capability::VmRestartPolicyNative,
                Capability::VmNamespaceIsolation,
            ],
        ))
        .expect("register");
        register_backend(fake("declares-neither", false, &[])).expect("register");

        assert!(vm_namespace_supported("declares-both"));
        assert!(!restart_policy_unsupervised(
            "declares-both",
            Some("always")
        ));

        assert!(!vm_namespace_supported("declares-neither"));
        assert!(restart_policy_unsupervised(
            "declares-neither",
            Some("always")
        ));
        // No policy: nothing to supervise, whatever the backend declares.
        assert!(!restart_policy_unsupervised("declares-neither", None));
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

    /// Capture and the interactive console are MUTUALLY EXCLUSIVE, and the XML has
    /// to say which. Revert the fix (unconditional pty) and this fails — it is the
    /// defect that left `<vmdir>/<name>.serial` unwritten and blocked every
    /// multi-node DKS cluster.
    #[test]
    fn libvirt_xml_captures_the_serial_to_a_file_when_asked() {
        let mut cfg = test_vm_cfg("1G");
        cfg.name = "dks-cp1".into();
        cfg.serial_capture = true;
        let xml = libvirt_domain_xml(
            &cfg,
            "/var/lib/delonix/vms/dks-cp1.qcow2",
            "52:54:00:aa:bb:cc",
        );
        assert!(
            xml.contains(
                "<serial type='file'><source path='/var/lib/delonix/vms/dks-cp1.serial'/>"
            ),
            "the serial had to be captured to the file the reader opens; XML:\n{xml}"
        );
        assert!(
            !xml.contains("<serial type='pty'>"),
            "one guest /dev/console maps to one port — never both destinations"
        );
    }

    /// The default must stay byte-for-byte what it was: `delonix vm console` is a
    /// shipped feature, and capture is opt-in per VM precisely so it survives.
    #[test]
    fn without_capture_the_libvirt_xml_keeps_the_interactive_console() {
        let cfg = test_vm_cfg("1G");
        let xml = libvirt_domain_xml(&cfg, "/var/lib/delonix/vms/t.qcow2", "52:54:00:aa:bb:cc");
        assert!(
            xml.contains("<serial type='pty'>"),
            "the default has to stay interactive"
        );
        assert!(
            !xml.contains(".serial"),
            "nothing should point at a capture file"
        );
    }

    /// The Cloud Hypervisor half of the same rule. Revert the fix and this fails:
    /// capture silently produced `socket=`, and `<name>.serial` stayed unwritten.
    #[test]
    fn the_ch_serial_destination_is_file_or_socket_never_both() {
        let serial = Path::new("/var/lib/delonix/vms/n1.serial");
        let console = Path::new("/var/lib/delonix/vms/n1.console");
        assert_eq!(
            ch_serial_dest(true, serial, console),
            "file=/var/lib/delonix/vms/n1.serial"
        );
        assert_eq!(
            ch_serial_dest(false, serial, console),
            "socket=/var/lib/delonix/vms/n1.console"
        );
    }

    /// The path is DERIVED from the same formula `boot_ch` uses, so the writer and
    /// the reader cannot drift apart — the discipline `fw_rule_tail` already
    /// enforces on the firewall side.
    #[test]
    fn the_capture_path_comes_from_the_vmdir_and_the_name() {
        let mut cfg = test_vm_cfg("1G");
        cfg.name = "n1".into();
        cfg.serial_capture = true;
        assert_eq!(
            serial_capture_path(&cfg, "/var/lib/delonix/vms/n1.qcow2").as_deref(),
            Some("/var/lib/delonix/vms/n1.serial")
        );
        cfg.serial_capture = false;
        assert_eq!(
            serial_capture_path(&cfg, "/var/lib/delonix/vms/n1.qcow2"),
            None
        );
    }

    /// The trap this repo has already paid four times (`-v`, `-p` on a custom
    /// network, extra networks, `Container.pod`): state needed to REBUILD the
    /// resource has to be persisted. `vm start` rebuilds the `VmConfig` from the
    /// record — without this round-trip a restarted DKS node came back interactive
    /// and the unattended reader went quiet again.
    #[test]
    fn serial_capture_survives_a_restart() {
        let mut cfg = test_vm_cfg("1G");
        cfg.serial_capture = true;
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
        vm.boot = boot_spec_of(&cfg);
        assert!(
            config_from(&vm).serial_capture,
            "a capture-mode VM that loses the flag on restart goes silent"
        );
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
        ) -> delonix_model::Result<Boot> {
            unreachable!("these tests never boot")
        }
        fn is_running(&self, _vm: &Vm) -> bool {
            false
        }
        fn ip(&self, _vm: &Vm) -> Option<String> {
            None
        }
        fn stop(&self, _vmdir: &Path, _vm: &Vm) -> delonix_model::Result<()> {
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
            report: crate::capabilities::undeclared("fake"),
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
            report: crate::capabilities::undeclared("fake"),
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
            report: crate::capabilities::undeclared("fake"),
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
            report: crate::capabilities::undeclared("fake"),
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
                report: crate::capabilities::undeclared("fake"),
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
                report: crate::capabilities::undeclared("fake"),
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
            auto_detect(&tabela, &[]).unwrap().id(),
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

    /// A report factory that marks `yes` usable and everything else a "no" —
    /// the table a requirement is compared against, without a host probe.
    fn reporting(id: &'static str, yes: &'static [Capability]) -> ReportFactory {
        use delonix_compute::capability::{
            CapabilityState as S, HealthStatus, ProviderHealth, ProviderKind, ProviderReport,
        };
        Box::new(move || {
            ProviderReport::build(
                id,
                ProviderKind::Compute,
                true,
                ProviderHealth {
                    status: HealthStatus::Healthy,
                    reason: "Ok",
                    message: String::new(),
                },
                |c| {
                    if yes.contains(&c) {
                        S::Partial {
                            detail: "declared for the test",
                        }
                    } else {
                        S::UnsupportedByProvider {
                            reason: "the test says no",
                        }
                    }
                },
            )
        })
    }

    fn fake(id: &'static str, auto: bool, yes: &'static [Capability]) -> BackendRegistration {
        BackendRegistration {
            id,
            aliases: &[],
            auto_selectable: auto,
            report: reporting(id, yes),
            new: Box::new(move || {
                Ok(Box::new(FakeBackend {
                    id,
                    available: true,
                    own_storage: false,
                    auto,
                }))
            }),
        }
    }

    /// ADR-0050 D6: a requirement FILTERS auto-detection — the first candidate
    /// is skipped when its report lacks the entry, the next that has it is
    /// chosen, and when none has it the refusal names what each lacked, never
    /// "no backend available" (there is one; it cannot do this).
    #[test]
    fn a_requirement_filters_auto_detection_and_the_refusal_names_what_each_lacked() {
        let mem = Capability::VmSnapshotMemory;
        let table = vec![
            fake("first", true, &[Capability::VmCreate]),
            fake(
                "second",
                true,
                &[Capability::VmCreate, Capability::VmSnapshotMemory],
            ),
        ];
        assert_eq!(auto_detect(&table, &[]).unwrap().id(), "first");
        assert_eq!(
            auto_detect(&table, &[mem]).unwrap().id(),
            "second",
            "the first candidate lacks the requirement and must be skipped"
        );
        // `Box<dyn VmBackend>` has no `Debug`, so `unwrap_err` cannot be used here.
        let e = match auto_detect(&table, &[mem, Capability::VmPause]) {
            Ok(b) => panic!("expected a refusal, got backend {}", b.id()),
            Err(e) => e,
        };
        assert!(
            matches!(e, Error::CapabilityNotSupported(_)),
            "not NoBackendAvailable: {e}"
        );
        let text = e.to_string();
        assert!(
            text.contains("first:") && text.contains("second:"),
            "{text}"
        );
        assert!(
            text.contains("vm.pause: unsupported-by-provider — the test says no"),
            "the provider's own state and reason: {text}"
        );
        assert_eq!(
            delonix_model::Error::from(e).number(),
            6507,
            "the contract's FAILED_PRECONDITION lands in the unavailable class"
        );
    }

    /// The name is resolved BEFORE any backend is asked: a typo is an invalid
    /// argument, never "no provider supports it".
    #[test]
    fn an_unknown_capability_name_is_an_invalid_argument_not_an_unsupported_one() {
        let e = resolve_required_capabilities(&["vm.snapshot.memry".into()]).unwrap_err();
        assert!(matches!(e, Error::UnknownCapability(_)), "{e}");
        assert!(e.to_string().contains("vm.snapshot.memry"), "{e}");
        assert_eq!(delonix_model::Error::from(e).number(), 1527);
        // Trimmed, deduplicated, in the caller's order.
        let ok = resolve_required_capabilities(&[
            " vm.create ".into(),
            "vm.pause".into(),
            "vm.create".into(),
        ])
        .unwrap();
        assert_eq!(ok, vec![Capability::VmCreate, Capability::VmPause]);
        assert!(resolve_required_capabilities(&[]).unwrap().is_empty());
    }

    /// `unmet` reads the report the way `provider describe` prints it; an
    /// entry the report does not carry is a "no" too, never a silent pass.
    #[test]
    fn unmet_lists_only_what_the_report_does_not_mark_usable() {
        let report = (reporting("x", &[Capability::VmCreate]))();
        assert!(unmet(&report, &[Capability::VmCreate]).is_empty());
        let m = unmet(&report, &[Capability::VmCreate, Capability::VmStop]);
        assert_eq!(
            m,
            vec!["vm.stop: unsupported-by-provider — the test says no"]
        );
        // A network entry is not in a compute report at all.
        let m = unmet(&report, &[Capability::NetBridge]);
        assert_eq!(m, vec!["net.bridge: not in this provider's report"]);
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
            ) -> delonix_model::Result<Boot> {
                Err(delonix_model::Error::Invalid("the node refused".into()))
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                Ok(())
            }
        }
        register_backend(BackendRegistration {
            id: "falharemoto",
            aliases: &[],
            auto_selectable: false,
            report: crate::capabilities::undeclared("fake"),
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

    /// `destroy` returns what it took, and takes exactly what is the VM's:
    /// overlay, seed dir, sockets and an extra disk inside the state directory
    /// go; an extra disk elsewhere is KEPT and named until `purge_disks`, and a
    /// 9p share is never touched.
    #[test]
    fn destroy_reports_and_respects_what_is_not_the_vms() {
        struct Nop;
        impl VmBackend for Nop {
            fn id(&self) -> &'static str {
                "nop-destroy"
            }
            fn available(&self) -> bool {
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
            ) -> delonix_model::Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                Ok(())
            }
        }
        register_backend(BackendRegistration {
            id: "nop-destroy",
            aliases: &[],
            auto_selectable: false,
            report: crate::capabilities::undeclared("fake"),
            new: Box::new(|| Ok(Box::new(Nop))),
        })
        .expect("registar");

        let base = std::env::temp_dir().join(format!(
            "delonix-destroy-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let vmdir = vms_dir(&base);
        std::fs::create_dir_all(vmdir.join("d")).unwrap();
        let outside = base.join("outside.qcow2");
        std::fs::write(vmdir.join("d.qcow2"), vec![0u8; 4096]).unwrap();
        std::fs::write(vmdir.join("d").join("seed.iso"), vec![0u8; 1024]).unwrap();
        std::fs::write(vmdir.join("d-data.qcow2"), vec![0u8; 2048]).unwrap();
        std::fs::write(&outside, b"operator image").unwrap();
        let st = store(&base).unwrap();
        let mut vm = Vm::new(
            "d".into(),
            "b".into(),
            "b".into(),
            1,
            "1G".into(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        );
        vm.backend = "nop-destroy".into();
        vm.boot.extra_disks = vec![
            ExtraDisk {
                source: vmdir.join("d-data.qcow2").to_string_lossy().into(),
                ..Default::default()
            },
            ExtraDisk {
                source: outside.to_string_lossy().into(),
                ..Default::default()
            },
        ];
        vm.boot.volumes = vec![VmVolume {
            tag: "t".into(),
            source: "/srv/shared".into(),
            mount_path: "/m".into(),
            read_only: false,
        }];
        st.save("d", &vm).unwrap();

        let d = destroy(&base, "d", false, false).expect("destroy");
        assert!(!vmdir.join("d.qcow2").exists());
        assert!(!vmdir.join("d").exists());
        assert!(!vmdir.join("d-data.qcow2").exists(), "extra disk owned");
        assert!(outside.exists(), "a disk outside the state dir is not ours");
        assert!(d.freed_bytes >= 4096 + 1024 + 2048);
        assert_eq!(d.kept.len(), 2, "{:?}", d.kept);
        assert!(st.load("d").is_err(), "the record is gone");
        assert!(!vmdir.join(".d.lock").exists(), "the lock file is gone too");

        // Second incarnation: `purge_disks` takes the outside disk too.
        st.save("d", &vm).unwrap();
        std::fs::write(vmdir.join("d.qcow2"), b"x").unwrap();
        destroy(&base, "d", false, true).expect("destroy purge");
        assert!(!outside.exists());
        let _ = std::fs::remove_dir_all(&base);
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
            ) -> delonix_model::Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                STOPS.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn destroy(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                DESTROYS.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        register_backend(BackendRegistration {
            id: "contador",
            aliases: &[],
            auto_selectable: false,
            report: crate::capabilities::undeclared("fake"),
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
            ) -> delonix_model::Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
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
            ) -> delonix_model::Result<Boot> {
                BOOTS.fetch_add(1, Ordering::SeqCst);
                Ok(Boot {
                    pid: None,
                    tap: String::new(),
                    mac: String::new(),
                    api_socket: "remoto:novo".into(),
                    ip: None,
                    lease_floor: None,
                })
            }
            fn resume(&self, _: &Path, vm: &Vm) -> delonix_model::Result<Option<Boot>> {
                RESUMES.fetch_add(1, Ordering::SeqCst);
                Ok(Some(Boot {
                    pid: None,
                    tap: String::new(),
                    mac: String::new(),
                    api_socket: vm.api_socket.clone(),
                    ip: None,
                    lease_floor: None,
                }))
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                Ok(())
            }
        }
        register_backend(BackendRegistration {
            id: "retomavel",
            aliases: &[],
            auto_selectable: false,
            report: crate::capabilities::undeclared("fake"),
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

    /// `vm move --node`: an empty target and a power state that does not
    /// match `--live` are refused before the backend is asked; a backend
    /// failure leaves the record naming the node the VM is still on; only an
    /// `Ok` writes the handle the backend returns. A backend with no cluster
    /// refuses by name and names `vm migrate`.
    #[test]
    fn move_refuses_before_the_backend_and_writes_the_handle_only_on_success() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Mutex;
        static CALLS: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());
        static FAIL: AtomicBool = AtomicBool::new(false);

        struct Movable;
        impl VmBackend for Movable {
            fn id(&self) -> &'static str {
                "movivel"
            }
            fn available(&self) -> bool {
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
            ) -> delonix_model::Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                Ok(())
            }
            fn move_to_node(
                &self,
                _: &Path,
                _: &Vm,
                target: &str,
                opts: &MoveOptions,
            ) -> delonix_model::Result<String> {
                CALLS.lock().unwrap().push((target.to_string(), opts.live));
                if FAIL.load(Ordering::SeqCst) {
                    return Err(delonix_model::Error::Invalid("node said no".into()));
                }
                Ok(format!("fake:{target}:7"))
            }
        }
        struct NoCluster;
        impl VmBackend for NoCluster {
            fn id(&self) -> &'static str {
                "sem-cluster"
            }
            fn available(&self) -> bool {
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
            ) -> delonix_model::Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                Ok(())
            }
        }
        for (id, new) in [
            (
                "movivel",
                Box::new(|| Ok(Box::new(Movable) as Box<dyn VmBackend>)) as BackendFactory,
            ),
            (
                "sem-cluster",
                Box::new(|| Ok(Box::new(NoCluster) as Box<dyn VmBackend>)) as BackendFactory,
            ),
        ] {
            register_backend(BackendRegistration {
                id,
                aliases: &[],
                auto_selectable: false,
                report: crate::capabilities::undeclared("fake"),
                new,
            })
            .expect("registar");
        }

        let base = std::env::temp_dir().join(format!(
            "delonix-move-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(vms_dir(&base)).unwrap();
        let st = store(&base).unwrap();
        let save = |name: &str, backend: &str, status: Status| {
            let mut vm = Vm::new(
                name.into(),
                "d".into(),
                "o".into(),
                1,
                "1G".into(),
                String::new(),
                String::new(),
                String::new(),
                "fake:a:7".into(),
            );
            vm.backend = backend.into();
            vm.status = status;
            st.save(name, &vm).unwrap();
        };
        save("parada", "movivel", Status::Stopped);
        save("a-correr", "movivel", Status::Running);
        save("pausada", "movivel", Status::Paused);
        save("n", "sem-cluster", Status::Stopped);
        let handle = |name: &str| st.load(name).unwrap().api_socket;

        let offline = MoveOptions::default();
        let online = MoveOptions {
            live: true,
            ..Default::default()
        };
        let code_of = |e: Error| e.number();
        // A target storage names where COPIED disks land: without
        // `--with-local-disks` nothing is copied, and an empty one names nothing.
        let storage_only = MoveOptions {
            target_storage: Some("fast".into()),
            ..Default::default()
        };
        assert_eq!(
            code_of(move_to_node(&base, "parada", "b", &storage_only).unwrap_err()),
            1538
        );
        let empty_storage = MoveOptions {
            with_local_disks: true,
            target_storage: Some(" ".into()),
            ..Default::default()
        };
        assert_eq!(
            code_of(move_to_node(&base, "parada", "b", &empty_storage).unwrap_err()),
            1538
        );
        assert_eq!(
            code_of(move_to_node(&base, "parada", " ", &offline).unwrap_err()),
            1538
        );
        assert_eq!(
            code_of(move_to_node(&base, "parada", "b", &online).unwrap_err()),
            5507
        );
        assert_eq!(
            code_of(move_to_node(&base, "a-correr", "b", &offline).unwrap_err()),
            5507
        );
        assert_eq!(
            code_of(move_to_node(&base, "pausada", "b", &online).unwrap_err()),
            5507
        );
        assert_eq!(
            code_of(move_to_node(&base, "pausada", "b", &offline).unwrap_err()),
            5507
        );
        assert!(move_to_node(&base, "nao-existe", "b", &offline)
            .unwrap_err()
            .is_not_found());
        assert!(
            CALLS.lock().unwrap().is_empty(),
            "a refusal reached the backend"
        );
        for n in ["parada", "a-correr", "pausada"] {
            assert_eq!(handle(n), "fake:a:7", "{n}: record changed");
        }

        FAIL.store(true, Ordering::SeqCst);
        assert!(move_to_node(&base, "parada", "b", &offline).is_err());
        assert_eq!(
            handle("parada"),
            "fake:a:7",
            "a failed move rewrote the handle"
        );
        FAIL.store(false, Ordering::SeqCst);

        let vm = move_to_node(&base, "parada", "b", &offline).unwrap();
        assert_eq!(vm.api_socket, "fake:b:7");
        assert_eq!(handle("parada"), "fake:b:7");
        let vm = move_to_node(&base, "a-correr", "b", &online).unwrap();
        assert_eq!(vm.api_socket, "fake:b:7");
        assert_eq!(
            *CALLS.lock().unwrap(),
            vec![
                ("b".to_string(), false),
                ("b".to_string(), false),
                ("b".to_string(), true)
            ]
        );

        let e = move_to_node(&base, "n", "b", &offline).unwrap_err();
        assert_eq!(e.number(), 1501, "{e}");
        let e = e.to_string();
        assert!(e.contains("sem-cluster") && e.contains("vm migrate"), "{e}");
        assert_eq!(handle("n"), "fake:a:7");

        let _ = std::fs::remove_dir_all(&base);
        backends()
            .write()
            .unwrap()
            .retain(|b| b.id != "movivel" && b.id != "sem-cluster");
    }

    /// `vm resize`: every refusal happens before the backend is asked and
    /// leaves the record untouched; a backend failure leaves it untouched too;
    /// only an `Ok` from the backend rewrites `vcpus`/`memory`. A backend with
    /// no override refuses by name instead of doing nothing.
    #[test]
    fn resize_refuses_before_the_backend_and_writes_the_record_only_on_success() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Mutex;
        static CALLS: Mutex<Vec<(u32, u64)>> = Mutex::new(Vec::new());
        static FAIL: AtomicBool = AtomicBool::new(false);

        struct Resizable;
        impl VmBackend for Resizable {
            fn id(&self) -> &'static str {
                "redimensionavel"
            }
            fn available(&self) -> bool {
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
            ) -> delonix_model::Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                Ok(())
            }
            fn resize_cold(
                &self,
                _: &Path,
                _: &Vm,
                vcpus: u32,
                memory_mib: u64,
            ) -> delonix_model::Result<()> {
                CALLS.lock().unwrap().push((vcpus, memory_mib));
                if FAIL.load(Ordering::SeqCst) {
                    return Err(delonix_model::Error::Invalid("node said no".into()));
                }
                Ok(())
            }
        }
        struct NoOverride;
        impl VmBackend for NoOverride {
            fn id(&self) -> &'static str {
                "sem-resize"
            }
            fn available(&self) -> bool {
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
            ) -> delonix_model::Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                Ok(())
            }
        }
        register_backend(BackendRegistration {
            id: "redimensionavel",
            aliases: &[],
            auto_selectable: false,
            report: crate::capabilities::undeclared("fake"),
            new: Box::new(|| Ok(Box::new(Resizable))),
        })
        .expect("registar");
        register_backend(BackendRegistration {
            id: "sem-resize",
            aliases: &[],
            auto_selectable: false,
            report: crate::capabilities::undeclared("fake"),
            new: Box::new(|| Ok(Box::new(NoOverride))),
        })
        .expect("registar");

        let base = std::env::temp_dir().join(format!(
            "delonix-resize-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(vms_dir(&base)).unwrap();
        let st = store(&base).unwrap();
        let save = |name: &str, backend: &str, status: Status| {
            let mut vm = Vm::new(
                name.into(),
                "d".into(),
                "o".into(),
                1,
                "1G".into(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            );
            vm.backend = backend.into();
            vm.status = status;
            st.save(name, &vm).unwrap();
        };
        save("r", "redimensionavel", Status::Stopped);
        save("a-correr", "redimensionavel", Status::Running);
        save("pausada", "redimensionavel", Status::Paused);
        save("n", "sem-resize", Status::Stopped);
        let unchanged = |name: &str| {
            let vm = st.load(name).unwrap();
            assert_eq!(
                (vm.vcpus, vm.memory.as_str()),
                (1, "1G"),
                "{name}: record changed"
            );
        };

        let code_of = |e: Error| e.number();
        assert_eq!(code_of(resize(&base, "r", None, None).unwrap_err()), 1536);
        assert_eq!(
            code_of(resize(&base, "r", Some(0), None).unwrap_err()),
            1536
        );
        assert_eq!(
            code_of(resize(&base, "r", None, Some("2GB")).unwrap_err()),
            1536
        );
        assert_eq!(
            code_of(resize(&base, "r", None, Some("0")).unwrap_err()),
            1536
        );
        assert_eq!(
            code_of(resize(&base, "a-correr", Some(2), None).unwrap_err()),
            5505
        );
        assert_eq!(
            code_of(resize(&base, "pausada", Some(2), None).unwrap_err()),
            5505
        );
        assert!(resize(&base, "nao-existe", Some(2), None)
            .unwrap_err()
            .is_not_found());
        assert!(
            CALLS.lock().unwrap().is_empty(),
            "a refusal reached the backend"
        );
        unchanged("r");
        unchanged("a-correr");

        FAIL.store(true, Ordering::SeqCst);
        assert!(resize(&base, "r", Some(4), None).is_err());
        unchanged("r");
        FAIL.store(false, Ordering::SeqCst);

        // Only memory: vCPUs keep the record's value, and the backend is told both.
        let vm = resize(&base, "r", None, Some("4Gi")).unwrap();
        assert_eq!((vm.vcpus, vm.memory.as_str()), (1, "4Gi"));
        let vm = resize(&base, "r", Some(3), None).unwrap();
        assert_eq!((vm.vcpus, vm.memory.as_str()), (3, "4Gi"));
        assert_eq!(st.load("r").unwrap().vcpus, 3);
        assert_eq!(
            *CALLS.lock().unwrap(),
            vec![(4, 1024), (1, 4096), (3, 4096)]
        );

        let e = resize(&base, "n", Some(2), None).unwrap_err().to_string();
        assert!(e.contains("resize") && e.contains("sem-resize"), "{e}");
        unchanged("n");

        let _ = std::fs::remove_dir_all(&base);
        backends()
            .write()
            .unwrap()
            .retain(|b| b.id != "redimensionavel" && b.id != "sem-resize");
    }

    #[test]
    fn parse_mem_mib_refuses_what_mem_mib_would_have_guessed() {
        assert_eq!(parse_mem_mib("512M"), Some(512));
        assert_eq!(parse_mem_mib("4G"), Some(4096));
        assert_eq!(parse_mem_mib("4Gi"), Some(4096));
        assert_eq!(parse_mem_mib(" 2048 "), Some(2048));
        for bad in [
            "2GB",
            "2 Gi x",
            "",
            "G",
            "0",
            "0G",
            "-1G",
            "99999999999999999999G",
        ] {
            assert_eq!(parse_mem_mib(bad), None, "{bad:?}");
        }
        assert_eq!(
            mem_mib("2GB"),
            1024,
            "the lenient reader keeps its fallback"
        );
    }

    /// `vm cloud-init`: every refusal happens before the backend and leaves
    /// the record untouched; the backend receives the MERGED intent (a field
    /// not given keeps the record's value, keys replace); the record changes
    /// only on `Ok`; a backend with no override refuses by name.
    #[test]
    fn set_cloud_init_refuses_first_and_hands_the_backend_the_merged_intent() {
        use std::sync::Mutex;
        static GOT: Mutex<Vec<CloudInitIntent>> = Mutex::new(Vec::new());

        struct Ci;
        impl VmBackend for Ci {
            fn id(&self) -> &'static str {
                "com-ci"
            }
            fn available(&self) -> bool {
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
            ) -> delonix_model::Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                Ok(())
            }
            fn update_cloud_init(
                &self,
                _: &Path,
                _: &Vm,
                intent: &CloudInitIntent,
            ) -> delonix_model::Result<()> {
                GOT.lock().unwrap().push(intent.clone());
                Ok(())
            }
        }
        register_backend(BackendRegistration {
            id: "com-ci",
            aliases: &[],
            auto_selectable: false,
            report: crate::capabilities::undeclared("fake"),
            new: Box::new(|| Ok(Box::new(Ci))),
        })
        .expect("registar");

        let base = std::env::temp_dir().join(format!(
            "delonix-cloudinit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(vms_dir(&base)).unwrap();
        let st = store(&base).unwrap();
        let save = |name: &str, backend: &str, status: Status, appliance: bool| {
            let mut vm = Vm::new(
                name.into(),
                "d".into(),
                "o".into(),
                1,
                "1G".into(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            );
            vm.backend = backend.into();
            vm.status = status;
            vm.boot.hostname = Some("velho".into());
            vm.boot.ci_user = Some("delonix".into());
            vm.boot.ssh_keys = vec!["ssh-ed25519 AAAA velha".into()];
            if appliance {
                vm.boot.cloud_init = Some(false);
            }
            st.save(name, &vm).unwrap();
        };
        save("c", "com-ci", Status::Stopped, false);
        save("viva", "com-ci", Status::Running, false);
        save("app", "com-ci", Status::Stopped, true);
        save("sem", "libvirt", Status::Stopped, false);

        let code_of = |e: Error| e.number();
        let key = |k: &str| Some(vec![k.to_string()]);
        assert_eq!(
            code_of(set_cloud_init(&base, "c", None, None, None).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(&base, "c", Some("-x"), None, None).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(&base, "c", Some("a.b"), None, None).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(&base, "c", None, Some("Root"), None).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(&base, "c", None, None, Some(vec![])).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(&base, "c", None, None, key("a\nb")).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(&base, "app", Some("h"), None, None).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(&base, "viva", Some("h"), None, None).unwrap_err()),
            5506
        );
        assert!(set_cloud_init(&base, "nada", Some("h"), None, None)
            .unwrap_err()
            .is_not_found());
        assert!(
            GOT.lock().unwrap().is_empty(),
            "a refusal reached the backend"
        );
        assert_eq!(
            st.load("c").unwrap().boot.hostname.as_deref(),
            Some("velho")
        );

        let vm = set_cloud_init(&base, "c", Some("novo"), None, None).unwrap();
        assert_eq!(vm.boot.hostname.as_deref(), Some("novo"));
        assert_eq!(
            vm.boot.ssh_keys,
            vec!["ssh-ed25519 AAAA velha".to_string()],
            "keys kept"
        );
        let vm = set_cloud_init(
            &base,
            "c",
            None,
            Some("ops"),
            key(" ssh-ed25519 AAAA nova "),
        )
        .unwrap();
        assert_eq!(
            vm.boot.ssh_keys,
            vec!["ssh-ed25519 AAAA nova".to_string()],
            "keys replaced, trimmed"
        );
        assert_eq!(st.load("c").unwrap().boot.ci_user.as_deref(), Some("ops"));
        let got = GOT.lock().unwrap().clone();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].hostname.as_deref(), Some("novo"));
        assert_eq!(
            got[0].ci_user.as_deref(),
            Some("delonix"),
            "merged with the record"
        );
        assert_eq!(got[1].hostname.as_deref(), Some("novo"));
        assert_eq!(got[1].ci_user.as_deref(), Some("ops"));

        let e = set_cloud_init(&base, "sem", Some("h"), None, None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("cloud-init") && e.contains("libvirt"), "{e}");
        assert_eq!(
            st.load("sem").unwrap().boot.hostname.as_deref(),
            Some("velho")
        );

        let _ = std::fs::remove_dir_all(&base);
        backends().write().unwrap().retain(|b| b.id != "com-ci");
    }

    /// ADR-0053 decision 3, the engine half: when the backend reports that it
    /// now knows the VM by another handle (it found it on another node of its
    /// cluster), `status()` — what `vm ls` runs — writes that handle to the
    /// record, so later commands stop asking the old node. A backend that
    /// reports nothing leaves the record alone.
    #[test]
    fn status_persists_the_handle_a_backend_relocated_the_vm_to() {
        struct Moved;
        impl VmBackend for Moved {
            fn id(&self) -> &'static str {
                "movido"
            }
            fn available(&self) -> bool {
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
            ) -> delonix_model::Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                Ok(())
            }
            fn current_handle(&self, vm: &Vm) -> Option<String> {
                (vm.name == "m").then(|| "remote:novo:7".to_string())
            }
        }
        register_backend(BackendRegistration {
            id: "movido",
            aliases: &[],
            auto_selectable: false,
            report: crate::capabilities::undeclared("fake"),
            new: Box::new(|| Ok(Box::new(Moved))),
        })
        .expect("registar");
        let base = std::env::temp_dir().join(format!(
            "delonix-relocated-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(vms_dir(&base)).unwrap();
        let st = store(&base).unwrap();
        for name in ["m", "fica"] {
            let mut vm = Vm::new(
                name.into(),
                "d".into(),
                "o".into(),
                1,
                "1G".into(),
                String::new(),
                String::new(),
                String::new(),
                "remote:velho:7".into(),
            );
            vm.backend = "movido".into();
            vm.status = Status::Stopped;
            st.save(name, &vm).unwrap();
        }
        assert_eq!(status(&base, "m").unwrap().api_socket, "remote:novo:7");
        assert_eq!(
            st.load("m").unwrap().api_socket,
            "remote:novo:7",
            "persisted"
        );
        assert_eq!(status(&base, "fica").unwrap().api_socket, "remote:velho:7");
        assert_eq!(st.load("fica").unwrap().api_socket, "remote:velho:7");
        let _ = std::fs::remove_dir_all(&base);
        backends().write().unwrap().retain(|b| b.id != "movido");
    }
}

/// Identidade do PID antes de matar o VMM — a assimetria que faltava fechar.
///
/// O lado dos containers guarda TODOS os sinais com `safe_to_signal` (dezasseis
/// pontos de chamada); o das VMs matava por `pid > 0`, e o registo nem sequer
/// levava o `starttime` contra o qual comparar.
#[cfg(test)]
mod tests_identidade_do_vmm {
    use super::*;

    fn vm_com(pid: Option<i32>, st: Option<u64>) -> Vm {
        let mut vm = Vm::new(
            "t".into(),
            "d".into(),
            "o".into(),
            1,
            "1G".into(),
            "n".into(),
            "tap".into(),
            "mac".into(),
            "sock".into(),
        );
        vm.pid = pid;
        vm.pid_starttime = st;
        vm
    }

    /// O achado: um registo que nomeia um pid RECICLADO não pode disparar.
    #[test]
    fn um_pid_reciclado_nao_e_sinalizavel() {
        let eu = std::process::id() as i32;
        // vivo, mas o starttime não é o nosso — é o que um pid reciclado parece.
        assert_eq!(vmm_to_signal(&vm_com(Some(eu), Some(1))), None);
    }

    #[test]
    fn o_nosso_vmm_continua_a_ser_sinalizavel() {
        let eu = std::process::id() as i32;
        let st = proc_starttime(eu);
        assert!(st.is_some(), "o /proc deste processo tem de ser legível");
        assert_eq!(vmm_to_signal(&vm_com(Some(eu), st)), Some(eu));
    }

    /// Registos anteriores a este campo não podem deixar de parar.
    #[test]
    fn um_registo_antigo_sem_starttime_mantem_o_comportamento() {
        let eu = std::process::id() as i32;
        assert_eq!(vmm_to_signal(&vm_com(Some(eu), None)), Some(eu));
    }

    /// A adopção só pode acontecer com a identidade provada por OUTRO meio.
    #[test]
    fn a_adopcao_exige_o_api_socket_no_argv() {
        let sock = "/tmp/dlx/v.sock".to_string();
        let vmm = vec![
            "/usr/bin/cloud-hypervisor".to_string(),
            "--api-socket".into(),
            sock.clone(),
        ];
        assert!(argv_is_vmm_for(&vmm, &sock));

        // O MESMO binário, a servir OUTRA VM — é o caso que um pid reciclado
        // dentro da mesma frota produz, e é o que não pode ser adoptado.
        assert!(!argv_is_vmm_for(&vmm, "/tmp/dlx/outra.sock"));
        // Um processo qualquer que por acaso nomeie o socket.
        assert!(!argv_is_vmm_for(
            &["/bin/cat".to_string(), sock.clone()],
            &sock
        ));
        // Um registo sem api-socket não tem token nenhum: nunca adopta.
        assert!(!argv_is_vmm_for(&vmm, ""));
    }

    /// Um registo que JÁ tem starttime nunca é reescrito — carimbar por cima
    /// seria trocar uma prova por um palpite.
    #[test]
    fn a_adopcao_nao_toca_num_registo_ja_carimbado() {
        let mut vm = vm_com(Some(std::process::id() as i32), Some(12345));
        assert!(!adopt_pid_starttime(&mut vm));
        assert_eq!(vm.pid_starttime, Some(12345));
    }

    /// Este processo de teste não é um cloud-hypervisor: a adopção recusa.
    #[test]
    fn nao_adopta_um_pid_vivo_que_nao_e_o_vmm() {
        let mut vm = vm_com(Some(std::process::id() as i32), None);
        vm.api_socket = "/tmp/dlx/v.sock".into();
        assert!(!adopt_pid_starttime(&mut vm));
        assert_eq!(vm.pid_starttime, None, "não carimbar sem prova");
    }

    #[test]
    fn sem_pid_nao_ha_nada_a_sinalizar() {
        assert_eq!(vmm_to_signal(&vm_com(None, None)), None);
        // pid morto (o 0 nunca é sinalizável por este caminho)
        assert_eq!(vmm_to_signal(&vm_com(Some(0), None)), None);
    }

    /// O campo é `#[serde(default)]`: um `.json` escrito antes disto existir
    /// tem de continuar a carregar, senão a correcção tranca VMs em disco.
    /// Escrito com o MESMO `JsonStore` que o motor usa, e sem o campo novo.
    #[test]
    fn um_registo_em_disco_sem_o_campo_continua_a_carregar() {
        let dir = std::env::temp_dir().join(format!("dlx-vmpid-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let antigo = concat!(
            r#"{"name":"v","disk":"d","overlay":"o","vcpus":1,"#,
            r#""memory":"1G","network":"n","tap":"t","mac":"m","pid":42,"#,
            r#""api_socket":"s","status":"Running","created_unix":0}"#
        );
        std::fs::write(dir.join("v.json"), antigo).unwrap();
        let st: JsonStore<Vm> = JsonStore::open(&dir).unwrap();
        let vm = st.load("v").expect("um registo antigo tem de carregar");
        assert_eq!(vm.pid, Some(42));
        assert_eq!(vm.pid_starttime, None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// ACH-014: `stop` used to SIGTERM the vmm and return, so the disk it holds was
/// still locked when the next command opened it. These hold `terminate_vmm` to
/// the only contract that matters — it does not return while the process runs.
///
/// The subject is a real orphaned process, launched the way `boot_ch` launches
/// the vmm (backgrounded from a `sh` that then exits), so its pid behaves like
/// the vmm's: reaped by init, not by this test.
#[cfg(test)]
mod tests_the_stop_waits_for_the_vmm {
    use super::*;

    /// Backgrounds `cmd` from a shell that exits, and returns the orphan's pid.
    ///
    /// The `</dev/null >/dev/null 2>&1` is not tidiness: without it the orphan
    /// inherits this call's stdout pipe and `output()` blocks until the orphan
    /// itself exits — the subject would be dead before the test began. It is
    /// also exactly what `boot_ch` writes when it launches the vmm.
    fn spawn_orphan(cmd: &str) -> i32 {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("{cmd} </dev/null >/dev/null 2>&1 & echo $!"))
            .output()
            .expect("sh has to run");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("the shell has to print the pid")
    }

    /// `true` while the process still EXECUTES — the property `stop` was
    /// returning in spite of. A zombie is not running: it has closed its
    /// descriptors, and the qcow2 lock with them.
    fn still_running(pid: i32) -> bool {
        matches!(proc_state(pid), Some(st) if st != 'Z')
    }

    #[test]
    fn it_does_not_return_while_the_vmm_is_still_running() {
        let pid = spawn_orphan("sleep 30");
        let starttime = proc_starttime(pid);
        assert!(still_running(pid), "the subject has to be up to be stopped");

        assert!(terminate_vmm(
            pid,
            starttime,
            Duration::from_secs(5),
            Duration::from_secs(2)
        ));
        assert!(
            !still_running(pid),
            "terminate_vmm returned with the process still running — this is ACH-014"
        );
    }

    /// A vmm that ignores the `SIGTERM` must not turn `stop` into a lie either:
    /// the grace runs out, the `SIGKILL` goes, and only then does it return.
    #[test]
    fn it_escalates_to_sigkill_when_the_sigterm_is_ignored() {
        let pid = spawn_orphan("trap '' TERM; sleep 30");
        let starttime = proc_starttime(pid);
        // Give the shell a moment to install the trap, or the SIGTERM lands
        // first and the test proves nothing.
        std::thread::sleep(Duration::from_millis(200));
        assert!(still_running(pid), "the subject has to be up to be stopped");

        let began = Instant::now();
        assert!(terminate_vmm(
            pid,
            starttime,
            Duration::from_millis(300),
            Duration::from_secs(2)
        ));
        assert!(!still_running(pid), "the SIGKILL did not land");
        assert!(
            began.elapsed() >= Duration::from_millis(300),
            "it returned before the grace was up: the SIGTERM was never waited on"
        );
    }

    /// The wait is on the process LEAVING, not on it being reaped: a zombie
    /// has already released the disk, and blocking on the reaper — which is
    /// init, not us — would trade the race for a stall.
    #[test]
    fn a_zombie_counts_as_gone() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("sh has to run");
        let pid = child.id() as i32;
        let starttime = proc_starttime(pid);
        // Nobody calls `wait` here, so it stays a zombie: alive to `kill(pid, 0)`
        // and with a readable `/proc`, which is exactly the shape that would
        // hang a wait written as `!safe_to_signal(...)`.
        let deadline = Instant::now() + Duration::from_secs(5);
        while proc_state(pid) != Some('Z') && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(proc_state(pid), Some('Z'), "the subject has to be a zombie");
        assert!(vmm_left(pid, starttime));
        assert!(wait_vmm_left(pid, starttime, Duration::from_millis(50)));
        let _ = child.wait();
    }

    /// And the timeout is a timeout: a process that will not leave makes the
    /// wait say so instead of waiting forever.
    #[test]
    fn the_wait_gives_up_when_the_process_stays() {
        let pid = spawn_orphan("sleep 30");
        let starttime = proc_starttime(pid);
        let began = Instant::now();
        assert!(!wait_vmm_left(pid, starttime, Duration::from_millis(200)));
        assert!(began.elapsed() >= Duration::from_millis(200));
        // SAFETY: our own subject, spawned above.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }

    /// `proc_state` has to survive a comm with spaces and parentheses in it —
    /// the same trap `proc_starttime` documents.
    #[test]
    fn the_state_is_read_after_the_comm() {
        let me = std::process::id() as i32;
        assert!(
            matches!(proc_state(me), Some('R') | Some('S')),
            "this very process has to read as running"
        );
        assert_eq!(proc_state(-1), None, "a pid with no /proc reads as None");
    }
}

#[cfg(test)]
mod tests_boot_confirms_the_vmm {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    /// A short, per-test directory: the api-socket inside it has to fit in
    /// `sun_path`, which is the very limit under test elsewhere.
    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dlxboot-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The same shape `boot_ch` writes, around a fake VMM `cmd`.
    fn script(cmd: &str, log: &Path, pidfile: &Path) -> String {
        format!(
            "{cmd} </dev/null >>{log} 2>&1 & echo $! > {pid}",
            log = shq(&log.to_string_lossy()),
            pid = shq(&pidfile.to_string_lossy())
        )
    }

    /// A no-op `join`: `env sh -c …` runs the script in this namespace.
    fn no_join() -> Vec<String> {
        vec!["env".into()]
    }

    /// Serves `vm.info` on `sock` with `state` for every connection, keeping
    /// each connection open after the answer (keep-alive, as CH does) so a
    /// client that waits for EOF would hang.
    fn fake_api(sock: &Path, state: &'static str) {
        let l = UnixListener::bind(sock).unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for mut c in l.incoming().flatten() {
                let mut b = [0u8; 1024];
                let _ = c.read(&mut b);
                let body = format!("{{\"config\":{{}},\"state\":\"{state}\"}}");
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                held.push(c);
            }
        });
    }

    fn kill(pid: i32) {
        // SAFETY: our own subject.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }

    /// THE defect: a VMM that dies right after launch used to come back as a
    /// pid, and `vm create` recorded it `Running`. It has to be an error, and
    /// the error has to carry the cause from the VM log.
    #[test]
    fn a_vmm_that_dies_at_startup_is_an_error_with_the_log() {
        let d = dir("dies");
        let (log, pid, sock) = (d.join("v.log"), d.join("v.pid"), d.join("v.sock"));
        let s = script(
            "sh -c 'echo \"Fatal error: path must be shorter than SUN_LEN\" >&2; exit 1'",
            &log,
            &pid,
        );
        let began = Instant::now();
        let r = launch_vmm(&no_join(), &s, &pid, &sock, &log, Duration::from_secs(5));
        let err = r
            .expect_err("a VMM that exited at startup was reported as started")
            .to_string();
        assert!(err.contains("exited during startup"), "{err}");
        assert!(err.contains("SUN_LEN"), "the log tail is missing: {err}");
        assert!(
            began.elapsed() < Duration::from_secs(4),
            "the exit was not noticed; it waited for the grace instead"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The answer of an api that is up is not enough when the process is not:
    /// the api answered and the VMM left — a zombie included.
    #[test]
    fn an_answering_api_does_not_rescue_a_dead_vmm() {
        let d = dir("zombie");
        let (log, sock) = (d.join("v.log"), d.join("v.sock"));
        fake_api(&sock, "Running");
        let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
        let pid = child.id() as i32;
        let deadline = Instant::now() + Duration::from_secs(5);
        while proc_state(pid) != Some('Z') && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(proc_state(pid), Some('Z'), "the subject has to be a zombie");
        assert!(wait_vmm_ready(pid, &sock, &log, Duration::from_secs(2)).is_err());
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_vmm_whose_vm_is_running_is_returned() {
        let d = dir("ok");
        let (log, pidf, sock) = (d.join("v.log"), d.join("v.pid"), d.join("v.sock"));
        fake_api(&sock, "Running");
        let s = script("sleep 30", &log, &pidf);
        let pid = launch_vmm(&no_join(), &s, &pidf, &sock, &log, Duration::from_secs(5))
            .expect("a live VMM reporting Running has to be accepted");
        assert!(matches!(proc_state(pid), Some(st) if st != 'Z'));
        kill(pid);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Alive but never `Running` (a silent or stuck VMM): an error once the
    /// grace is up, and the process does not stay behind holding the disk.
    #[test]
    fn a_vmm_that_never_reports_running_is_terminated() {
        let d = dir("silent");
        let (log, pidf, sock) = (d.join("v.log"), d.join("v.pid"), d.join("v.sock"));
        fake_api(&sock, "Created");
        let s = script("sleep 30", &log, &pidf);
        let r = launch_vmm(
            &no_join(),
            &s,
            &pidf,
            &sock,
            &log,
            Duration::from_millis(400),
        );
        let err = r
            .expect_err("a VM that never ran was reported as started")
            .to_string();
        assert!(err.contains("did not report the VM running"), "{err}");
        let pid: i32 = std::fs::read_to_string(&pidf)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            wait_vmm_left(pid, None, Duration::from_secs(3)),
            "the silent VMM was left running"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn socket_paths_past_sun_path_are_refused_up_front() {
        let fits = |root_len: usize, capture: bool| {
            // `<vmdir>/<name>.sock` with name "vmx": vmdir + 9 bytes.
            let vmdir = std::path::PathBuf::from(format!("/{}", "p".repeat(root_len - 1)));
            let cfg = VmConfig {
                name: "vmx".into(),
                serial_capture: capture,
                ..Default::default()
            };
            ch_socket_paths_fit(&vmdir, &cfg)
        };
        assert!(fits(98, true).is_ok(), "107 bytes fits");
        let err = fits(99, true)
            .expect_err("108 bytes does not fit")
            .to_string();
        assert!(err.contains("108 bytes") && err.contains("107"), "{err}");
        // The console socket (`<base>/vms/<name>.console`) only exists when
        // the serial is interactive, and it is the longer of the two.
        let cfg = VmConfig {
            name: "vmx".into(),
            serial_capture: false,
            ..Default::default()
        };
        let vmdir = std::path::PathBuf::from(format!("/{}/vms", "p".repeat(91)));
        assert_eq!(
            console_socket(vmdir.parent().unwrap(), "vmx")
                .as_os_str()
                .len(),
            108
        );
        assert!(ch_socket_paths_fit(&vmdir, &cfg).is_err());
        let cfg = VmConfig {
            serial_capture: true,
            ..cfg
        };
        assert!(ch_socket_paths_fit(&vmdir, &cfg).is_ok());
    }

    #[test]
    fn vm_info_state_and_content_length_are_read_from_the_real_shapes() {
        assert!(vm_info_says_running(
            br#"{"config":{},"state":"Running","memory_actual_size":0}"#
        ));
        assert!(vm_info_says_running(b"{\n  \"state\": \"Running\"\n}"));
        assert!(!vm_info_says_running(br#"{"state":"Created"}"#));
        assert!(!vm_info_says_running(b""));
        assert_eq!(
            http_content_length("HTTP/1.1 200 OK\r\ncontent-length: 42"),
            Some(42)
        );
        assert_eq!(http_content_length("HTTP/1.1 204 No Content"), None);
    }
}
