//! The libvirt/KVM VM backend (ADR-0044 P4b.4b): QEMU under libvirtd, driven
//! through `virsh`, behind the `VmBackend` port of `delonix-compute`. It moved
//! out of `delonix-vm` so the provider depends only on the foundation and the
//! contexts; the composition root registers it with [`registration`].
//!
//! The process helpers at the top (`stable_cmd`, `capture`, `run_quiet`,
//! `binary_in_path`) are this crate's own copy: a provider may not depend on
//! another provider or on an adapter, and a context may not run programs. What
//! the copies must never diverge on is the fixed locale, and the test that
//! pins it lives here too.

#![allow(clippy::too_many_arguments)]

use delonix_compute::capability::{
    Capability as C, CapabilityState as S, HealthStatus, ProviderHealth, ProviderKind,
    ProviderReport,
};
use delonix_compute::vm::{serial_log_path, valid_vm_name};
use delonix_compute::vm_backend::{
    guest_cpu_outcome as cpu_outcome, guest_memory_outcome as memory_outcome, guest_memory_state,
    guest_online_vcpus, libvirt_nic, mem_mib, names_a_declared_network, BackendRegistration, Boot,
    CreateStage, GuestFilesystem, GuestInfo, HotplugOutcome, LibvirtNic, VmBackend, VmConfig,
};
use delonix_compute::vm_error::{Error, Result};
use delonix_compute::vm_registry::mac_for;
use delonix_compute::{ExtraDisk, ExtraNic, Vm};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The registration the composition root seeds the registry with: auto-
/// selectable (it answers `available()` from this host alone), `kvm` and
/// `qemu` as aliases.
pub fn registration() -> BackendRegistration {
    BackendRegistration {
        id: "libvirt",
        aliases: &["kvm", "qemu"],
        auto_selectable: true,
        new: Box::new(|| Ok(Box::new(LibvirtBackend))),
        report: Box::new(|| libvirt_report(&LibvirtHost::probe())),
    }
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

/// `true` if a binary exists in `PATH`.
fn binary_in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(name).is_file()))
        .unwrap_or(false)
}

/// `true` if running without root privileges (euid ≠ 0).
fn is_rootless() -> bool {
    // SAFETY: geteuid has no side effects.
    unsafe { libc::geteuid() != 0 }
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
        return LeasePick::NoLeaseForMac;
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

/// What the lease table says about THIS boot's address.
#[derive(Debug, PartialEq, Eq)]
enum LeasePick {
    /// A lease issued during this boot — the address to report.
    Current(String),
    /// The MAC has leases, but every one of them predates this boot. Reporting
    /// any of them is reporting a dead VM's address; the caller must NOT fall
    /// back to another source that reads the same table (`domifaddr` does).
    OnlyStale,
    /// The table WAS read and holds no entry for this MAC: this VM does not
    /// have an address. Same rule as [`Self::OnlyStale`] — the caller must NOT
    /// fall back to `domifaddr`, which reads the SAME table and so can only
    /// ever agree or add noise.
    ///
    /// BUG FIXED HERE, diagnosed live on 2026-10-15 (`cluster kubeadm --name
    /// lab`): this case and [`Self::TableUnreadable`] used to be one variant,
    /// so «the table says this VM has no address» was treated as «nobody could
    /// tell me» and fell through to the fallback. A VM with no lease of its own
    /// must report no address, so the `--boot-timeout` wait is what trips —
    /// anything else hands the caller an address this VM was never given, and
    /// `cluster kubeadm` then runs every host's prep against ONE machine.
    NoLeaseForMac,
    /// The lease table could not be read AT ALL (the domain is not on a
    /// libvirt-managed network, so there is no `net-dhcp-leases` to ask). This
    /// is the ONLY case where the `domifaddr` fallback adds information.
    TableUnreadable,
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
        LeasePick::OnlyStale | LeasePick::NoLeaseForMac | LeasePick::TableUnreadable => None,
    }
}

/// What the libvirt backend needs from the host, each probed separately so the
/// message names the missing piece instead of a generic "not available".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibvirtHost {
    pub virsh: bool,
    pub qemu: bool,
    pub kvm: bool,
    /// `qemu:///system` answers — needed for `nat`/`bridge` (an observed IP).
    pub system_uri: bool,
}

impl LibvirtHost {
    /// Everything present: the declaration as published, host-independent.
    pub const ASSUMED: LibvirtHost = LibvirtHost {
        virsh: true,
        qemu: true,
        kvm: true,
        system_uri: true,
    };

    /// Reads this machine. Costs three `which` and one `virsh uri` round trip;
    /// never creates anything.
    pub fn probe() -> LibvirtHost {
        let virsh = binary_in_path("virsh");
        LibvirtHost {
            virsh,
            qemu: binary_in_path("qemu-system-x86_64"),
            kvm: std::path::Path::new("/dev/kvm").exists(),
            system_uri: virsh && system_libvirt_usable(),
        }
    }
}

/// The libvirt backend's report against `host`.
pub fn libvirt_report(host: &LibvirtHost) -> ProviderReport {
    let available = host.virsh && host.qemu;
    let (reason, message) = if !host.virsh {
        (
            "VirshMissing",
            "virsh is not installed (libvirt-clients)".to_string(),
        )
    } else if !host.qemu {
        (
            "QemuMissing",
            "qemu-system-x86_64 is not installed".to_string(),
        )
    } else if !host.kvm {
        (
            "NoKvm",
            "/dev/kvm is absent: guests would run without acceleration".to_string(),
        )
    } else if !host.system_uri {
        (
            "SessionOnly",
            "qemu:///system does not answer (join the `libvirt` group): only user-mode networking, no observed IP".to_string(),
        )
    } else {
        ("Ok", String::new())
    };
    // The system connection is what most of the network-facing rows need; a
    // session-only host keeps the lifecycle and loses the observed address.
    let bin = |s: S| s.on_host(available, "virsh + qemu-system-x86_64 not both installed");
    // Narrowed AFTER `bin`: a host without virsh has no system connection
    // either, whatever the probe struct says — `probe()` derives one from the
    // other, but the declaration must not depend on that coupling.
    let sys = |s: S| bin(s).on_host(host.system_uri, "needs qemu:///system (libvirt group)");
    ProviderReport::build(
        "libvirt",
        ProviderKind::Compute,
        available,
        health(available, reason, message),
        |c| {
            match c {
            C::ProviderAvailability => bin(S::Partial {
                detail: "`available()` checks the two binaries; `provider ls` additionally probes qemu:///system",
            }),
            C::ResourceReadback => bin(S::Supported {
                evidence: "check:o vm ls diz Paused (não Stopped)",
            }),
            C::Events => S::NotImplemented,
            C::AsyncOperations => S::NotImplemented,
            C::VmCreate => bin(S::Supported {
                evidence: "e2e:vm: o snapshot sobrevive a um stop/start (precisa de hipervisor)",
            }),
            C::VmStart => bin(S::Supported {
                evidence: "check:vm start",
            }),
            C::VmStop => bin(S::Supported {
                evidence: "check:vm stop",
            }),
            C::VmDestroy => bin(S::Partial {
                detail: "`vm destroy` undefines the domain, releases the DHCP reservation and removes the overlay; the battery tears down without a named check",
            }),
            C::VmRestart => bin(S::Supported {
                evidence: "e2e:vm: stop-then-start is a real reboot — the guest boot id changes (libvirt, cloud-init, guestfish)",
            }),
            C::VmPause => bin(S::Supported {
                evidence: "check:vm pause",
            }),
            C::VmResume => bin(S::Supported {
                evidence: "check:vm unpause",
            }),
            C::VmResumeSameIdentity => bin(S::Supported {
                evidence: "check:vm start depois do stop de uma VM pausada",
            }),
            C::VmClone => bin(S::Supported {
                evidence: "test:crates/providers/delonix-provider-libvirt/tests/live.rs::vm_clone_boots_an_independent_domain_from_a_copied_disk",
            }),
            C::VmTemplate => bin(S::Supported {
                evidence: "test:crates/contexts/delonix-compute/src/vm.rs::mark_template_freezes_the_vm_and_start_refuses_it",
            }),
            C::VmResizeCold => bin(S::Supported {
                evidence: "check:o domínio arranca com 2 vCPU",
            }),
            C::VmHotplugCpuAdd => sys(S::Supported {
                evidence: "e2e:vm: um vCPU a QUENTE e ONLINE DENTRO do guest (libvirt, cloud-init, guestfish)",
            }),
            C::VmHotplugCpuRemove => S::NotImplemented,
            C::VmHotplugMemoryAdd => sys(S::Supported {
                evidence: "e2e:vm: memoria a QUENTE e ONLINE DENTRO do guest (libvirt, cloud-init, guestfish)",
            }),
            C::VmHotplugMemoryRemove => S::NotImplemented,
            C::VmHotplugDiskAdd => sys(S::Supported {
                evidence: "e2e:vm: um disco a QUENTE e detectado e fica R/W DENTRO do guest (libvirt, cloud-init, guestfish)",
            }),
            C::VmHotplugDiskRemove => S::NotImplemented,
            C::VmHotplugNicAdd => sys(S::Supported {
                evidence: "e2e:vm: uma NIC a QUENTE e detectada e passa TRAFEGO REAL DENTRO do guest (libvirt, cloud-init, guestfish)",
            }),
            C::VmHotplugNicRemove => S::NotImplemented,
            C::VmExtraDisks => bin(S::Supported {
                evidence: "e2e:vm: an extra disk declared at create is seen and R/W inside the guest (libvirt, cloud-init, guestfish)",
            }),
            C::VmExtraNics => bin(S::Supported {
                evidence: "e2e:vm: an extra NIC declared at create is seen and passes real traffic inside the guest (libvirt, cloud-init, guestfish)",
            }),
            C::VmDiskResize => bin(S::Supported {
                evidence: "check:o overlay cresceu para exactamente 1 GiB",
            }),
            C::VmPciPassthrough => bin(S::Partial {
                detail: "`<hostdev>` per validated PCI address; no IOMMU host in the battery",
            }),
            C::VmTpm => bin(S::Partial {
                detail: "`<tpm model='tpm-crb'>` emulator; swtpm presence is not probed",
            }),
            C::VmCpuModel => bin(S::Supported {
                evidence: "e2e:vm: the requested CPU topology is seen inside the guest (libvirt, cloud-init, guestfish)",
            }),
            C::VmCpuPinning => bin(S::Partial {
                detail: "`<cputune>` quota + `<vcpupin>`; unit-tested XML, dropped on qemu:///session",
            }),
            C::VmHugepages => bin(S::Partial {
                detail: "`<memoryBacking><hugepages/>`; the host's hugepage pool is not probed",
            }),
            C::VmCloudInit => sys(S::Supported {
                evidence: "e2e:vm: o cloud-init do guest CONSOME o seed NoCloud (libvirt, guestfish)",
            }),
            C::VmRestartPolicyNative => bin(S::Partial {
                detail: "`on_crash=restart` for always/on-failure reaches the LIVE domain XML (validated against a booted guest); the crash→restart EFFECT stays unproven because the domain carries no `<panic>` device, so a guest kernel panic is not reported to libvirt and on_crash never fires — honest until a pvpanic path exists",
            }),
            C::VmNamespaceIsolation => S::UnsupportedByProvider {
                reason: "a libvirt VM lives on virbr0 in the host netns, a different L2 the engine does not program; `--namespace` is refused by name",
            },
            C::VmAntispoof => sys(S::Supported {
                evidence: "test:crates/providers/delonix-provider-libvirt/src/lib.rs::antispoof_live_defines_and_survives_the_domain",
            }),
            C::VmRawDefinition => bin(S::Partial {
                detail: "`libvirtXml`/`libvirtXmlOverlay` are UNVALIDATED, trusted manifests only, local CLI only — never reachable from the node contract (ADR-0050)",
            }),
            C::SystemContainerLifecycle
            | C::SystemContainerOciImage
            | C::SystemContainerEntrypointEnv
            | C::SystemContainerExec
            | C::SystemContainerLogs
            | C::SystemContainerExitStatus
            | C::SystemContainerNetworkBridge
            | C::SystemContainerUnprivileged
            | C::SystemContainerSnapshot
            | C::SystemContainerResize
            | C::SystemContainerBackup
            | C::SystemContainerClone
            | C::SystemContainerFirewall
            | C::SystemContainerMove => S::UnsupportedByProvider {
                reason: "a VM provider; a system container is the Proxmox provider's (ADR-0058)",
            },
            C::ContainerLifecycle
            | C::ContainerExec
            | C::ContainerLogs
            | C::ContainerHotReconfigure
            | C::ContainerResourceLimits
            | C::ContainerGpuCdi
            | C::ContainerSeccompCustomProfile
            | C::ContainerOomDetection
            | C::PodSharedNetwork
            | C::PodSharedIpcUts
            | C::PodSharedPid
            | C::ContainerImages
            | C::ContainerBackupRestore => S::UnsupportedByProvider {
                reason: "a VM provider; containers are the Linux provider's",
            },
            C::VmNetworkNat => sys(S::Supported {
                evidence: "e2e:vm: o snapshot sobrevive a um stop/start (precisa de hipervisor)",
            }),
            C::VmNetworkBridge => sys(S::Partial {
                detail: "`netMode: bridge` enslaves the NIC to a host bridge; validated live for ADR-0046 phase 2, no battery check",
            }),
            C::VmNetworkSdn => S::UnsupportedByProvider {
                reason: "the NIC is on virbr0 (host netns), not on the engine's SDN; `vm bridge` (root, experimental) is the only path across",
            },
            C::VmStaticIp => sys(S::Partial {
                detail: "`--ip` reserves a DHCP host entry (`net-update`), released on destroy; argv unit-tested, no battery",
            }),
            C::StoragePools => S::NotImplemented,
            C::VmSnapshotDisk => bin(S::Supported {
                // #16: the offline disk snapshot is proved by DATA RECOVERY — a
                // datum is written to the disk, the snapshot is taken, the datum is
                // corrupted, and the restore reads the ORIGINAL back (guestfish).
                evidence: "check:o snapshot de disco captura o dado para recuperacao",
            }),
            C::VmSnapshotMemory => bin(S::Partial {
                // `snapshot-create-as` on a running VM captures RAM + disk and a
                // revert resumes the VM (the e2e proves a live snapshot brings it
                // back to Running). What is NOT observed is recovery of the guest's
                // in-memory CONTENT: the harness has no guest that boots an OS to
                // read RAM state before/after the revert. Only DISK recoverability
                // is proved (vm.snapshot-restore).
                detail: "running-domain snapshot captures RAM + disk and a revert resumes the VM; recovery of in-guest MEMORY CONTENT is not observed (no bootable guest on the harness) — only disk-data recovery is proved",
            }),
            C::VmSnapshotRestore => bin(S::Supported {
                // #16: the restore is proved by DATA recoverability, not by an
                // exit code nor by creating a file. See the cycle
                // ORIGINAL->snapshot->CORRUPTED->restore->ORIGINAL in e2e.sh.
                evidence: "check:o restore RECUPERA o dado de disco (nao so um codigo de saida)",
            }),
            C::VmSnapshotDelete => bin(S::Supported {
                evidence: "check:vm snapshot rm",
            }),
            C::VmSnapshotPersistent => bin(S::Supported {
                evidence: "check:o libvirt volta a conhecer o snapshot",
            }),
            C::VmBackupDisk => bin(S::Partial {
                detail: "`backup create vm` takes a live external snapshot and block-commits it back; the battery backs up a container, not a VM",
            }),
            C::VmBackupQuiesced => bin(S::Partial {
                detail: "`--quiesce` asks the guest agent to freeze; whether the guest has one is not probed",
            }),
            C::VmBackupRestore => bin(S::Partial {
                detail: "`backup restore` re-imports the disk; battery covers the container kind only",
            }),
            C::VmMigrationCold => bin(S::Supported {
                // The battery proves the mechanism live on one host — the
                // substantive steps (stop, `qemu-img convert` flatten to a
                // standalone disk, `--appliance` import without a seed reset,
                // create+boot on the target) with the guest's baked state
                // crossing intact. Only the generic scp transport between two
                // hosts is left to the cross-host run (not a provider capability).
                evidence: "e2e:vm: a stopped VM cold-migrates — flatten, import, boot on the target with state intact (libvirt, guestfish)",
            }),
            // libvirt/QEMU supports live migration without shared storage via
            // `--copy-storage-all` (libvirt >= 1.0.3); the capability is the
            // provider's, the engine just has not wired it — hence not-implemented,
            // not unsupported (ADR-0031). If the product intent is to require shared
            // storage instead of building the copy-storage-all path, move this to
            // RequiresExternalComponent (like C::VmReplication below).
            C::VmMigrationLive => S::NotImplemented,
            C::VmReplication => S::RequiresExternalComponent {
                component: "shared or replicated VM storage outside the engine (ADR-0031)",
            },
            C::VmHighAvailability => S::RequiresExternalComponent {
                component: "a cluster manager with quorum and fencing; a single libvirt host has none",
            },
            C::VmConsoleSerial => bin(S::Partial {
                detail: "`vm console` runs `virsh console --force`; interactive, no battery",
            }),
            C::VmConsoleVnc => bin(S::Partial {
                detail: "`vm vnc` reads `vncdisplay` of a `--vnc` domain; no battery",
            }),
            C::VmGuestAgent => bin(S::Supported {
                evidence: "live:crates/providers/delonix-provider-libvirt/tests/live.rs::libvirt_guest_agent_answers_on_a_real_cloud_init_boot",
            }),
            C::VmIpObserved => sys(S::Partial {
                detail: "DHCP lease with a lease floor, then `domifaddr`; pure test only, the battery does not read the IP",
            }),
            C::MetricsPrometheus => S::Partial {
                detail: "`delonix_vms_running/total` only; no per-domain stats",
            },
            C::MetricsPerWorkloadNetwork => S::NotImplemented,
            C::HostHealth => S::Supported {
                evidence: "check:system info",
            },
            C::HostCapacity => S::NotImplemented,
            C::TransportVerified => S::Partial {
                detail: "local URIs only (qemu:///system|session); a remote libvirt URI is never built",
            },
            C::CredentialInVault => S::UnsupportedByProvider {
                reason: "no credential exists: access is membership of the `libvirt` group",
            },
            C::NetBridge
            | C::NetMacvlanIpvlan
            | C::NetVlan
            | C::NetOverlayVxlan
            | C::NetOverlayEncrypted
            | C::NetIpam
            | C::NetStaticIp
            | C::NetDns
            | C::NetPublishPorts
            | C::NetRoutesBetweenNetworks
            | C::NetNamespaceIsolation
            | C::NetTunnelEgress
            | C::NetRateLimit
            | C::NetPacketCapture
            | C::NetL7Proxy
            | C::NetIpv6
            | C::VolumeLocal
            | C::VolumeBind
            | C::VolumeNfs
            | C::VolumeCifs
            | C::VolumeWebdav
            | C::VolumeQuota
            | C::VolumeSnapshot
            | C::VolumeProvisionNas
            | C::StorageLvmThin
            | C::StorageZfsBtrfs
            | C::StorageCeph
            | C::FirewallPerWorkload
            | C::FirewallDefaultDeny
            | C::FirewallSourceFiltering
            | C::FirewallEgressPolicy
            | C::NetGatewayFilter
            | C::NetGatewayAlias
            | C::NetGatewayUpdateInPlace
            | C::NetGatewayRuleOrder
            | C::NetGatewayMultiWan
            | C::NetGatewayVpn
            | C::NetNatSnat
            | C::NetNatDnat
            | C::NetNatOneToOne
            | C::NetNatNpt
            | C::NetLbL4
            | C::NetLbHealthCheck
            | C::NetDnsRecords
            | C::NetDnsAuthoritative
            | C::NetIpamProvider
            | C::NetIpamReservation
            | C::NetIpamDhcp
            | C::NetSegmentRemote
            | C::NetApplyStaged
            | C::NetApplyRollback
            | C::NetObserve
            | C::NetVerifyDataplane
            | C::NetOwnershipMarker
            | C::FirewallStateless
            | C::FirewallLogging
            | C::FirewallIcmpType
            | C::FirewallWorkloadPeer
            | C::FirewallIntraSegment => S::UnsupportedByProvider {
                reason: "not a compute capability: answered by the network/storage provider",
            },
        }
        },
    )
}

fn health(available: bool, reason: &'static str, message: String) -> ProviderHealth {
    ProviderHealth {
        status: if available {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unavailable
        },
        reason,
        message,
    }
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
pub fn system_libvirt_usable() -> bool {
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
pub fn libvirt_domain_uri(name: &str) -> Option<&'static str> {
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

/// Runs one QEMU guest agent command through `virsh qemu-agent-command` and
/// returns the QMP `"return"` payload — `Ok(None)` when the agent is not
/// reachable (not configured on this domain, not connected, domain not
/// running), the SAME distinction [`VmBackend::guest_info`]'s doc-comment
/// makes: a missing agent is an answer, not a failure. Any other `virsh`
/// failure (the domain does not exist at all, a malformed reply) propagates.
///
/// `--timeout 5` BEFORE the `--`: a flag after it is read as a positional
/// (the same ordering bug class `net-update`'s `--live --config` already
/// paid for in this file).
fn qemu_agent_query(
    uri: &str,
    domain: &str,
    qmp_command: &str,
) -> delonix_model::Result<Option<serde_json::Value>> {
    match quiet(
        "virsh",
        &[
            "-c",
            uri,
            "qemu-agent-command",
            "--timeout",
            "5",
            "--",
            domain,
            qmp_command,
        ],
    ) {
        Ok(out) => {
            let v: serde_json::Value = serde_json::from_str(&out).map_err(|e| {
                delonix_model::Error::from(Error::Command {
                    context: "virsh qemu-agent-command",
                    message: format!("could not parse the agent's reply: {e} ({out})"),
                })
            })?;
            Ok(v.get("return").cloned())
        }
        Err(e) if agent_unreachable(&e) => Ok(None),
        Err(e) => Err(Error::Command {
            context: "virsh qemu-agent-command",
            message: e,
        }
        .into()),
    }
}

/// `qemu-img info --output=json`'s `virtual-size`, in bytes — the ONLY
/// place this backend reads a disk's current size from (never the record,
/// which carries none). The tool call is a one-liner; the parsing it needs
/// is [`parse_virtual_size`], pure and tested without a real `qemu-img`.
fn qcow2_virtual_size(path: &Path) -> delonix_model::Result<u64> {
    let out = quiet(
        "qemu-img",
        &["info", "--output=json", "--", &path.to_string_lossy()],
    )
    .map_err(|e| {
        delonix_model::Error::from(Error::Command {
            context: "qemu-img info",
            message: e,
        })
    })?;
    parse_virtual_size(&out).ok_or_else(|| {
        delonix_model::Error::from(Error::Command {
            context: "qemu-img info",
            message: format!("no 'virtual-size' in: {out}"),
        })
    })
}

/// Pure: `qemu-img info --output=json`'s `"virtual-size"` field, in bytes.
fn parse_virtual_size(json: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()?
        .get("virtual-size")?
        .as_u64()
}

// ---- hotplug (ADR-0068 Phases 2–5) -------------------------------------

/// Writes `xml` to the VM's state directory and attaches it live; the file is
/// removed whatever the answer.
fn attach_xml(
    vmdir: &Path,
    vm: &Vm,
    uri: &str,
    xml: &str,
    what: &str,
) -> delonix_model::Result<()> {
    let path = vmdir.join(format!("{}.{what}.xml", vm.name));
    std::fs::write(&path, xml).map_err(|e| {
        delonix_model::Error::from(Error::Command {
            context: "vm hotplug",
            message: format!("{}: {e}", path.display()),
        })
    })?;
    let file = path.to_string_lossy().into_owned();
    let out = virsh_hotplug(
        &attach_device_argv(uri, &vm.name, &file),
        "virsh attach-device",
    );
    let _ = std::fs::remove_file(&path);
    out.map(|_| ())
}

/// The targets `virsh domblklist` lists. Pure.
fn parse_domblklist(out: &str) -> Vec<String> {
    out.lines()
        .skip_while(|l| !l.trim_start().starts_with('-'))
        .skip(1)
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect()
}

/// The MACs `virsh domiflist` lists (its last column). Pure.
fn parse_domiflist_macs(out: &str) -> Vec<String> {
    out.lines()
        .skip_while(|l| !l.trim_start().starts_with('-'))
        .skip(1)
        .filter_map(|l| l.split_whitespace().last().map(str::to_string))
        .filter(|m| m.contains(':'))
        .collect()
}

/// The first target letter of `bus` (`vd` for virtio, `sd` otherwise) the
/// live domain does not use; `a` is the boot disk's. Pure.
fn next_free_target(bus: &str, live: &[String]) -> Option<String> {
    let prefix = if bus == "virtio" { "vd" } else { "sd" };
    (b'b'..=b'z')
        .map(|c| format!("{prefix}{}", c as char))
        .find(|t| !live.contains(t))
}

/// The MAC of the VM's next extra NIC: derived from its name and the slot,
/// like the primary's, and never one it already carries. Pure.
fn extra_nic_mac(vm: &Vm) -> String {
    let taken = |m: &str| {
        m.eq_ignore_ascii_case(&vm.mac)
            || vm
                .boot
                .extra_nics
                .iter()
                .any(|n| n.mac.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(m)))
    };
    (1..)
        .map(|i| mac_for(&format!("{}#nic{i}", vm.name)))
        .find(|m| !taken(m))
        .unwrap_or_default()
}

/// How many disks the guest's agent lists — `None` without an agent.
fn guest_disk_count(uri: &str, domain: &str) -> delonix_model::Result<Option<usize>> {
    Ok(
        qemu_agent_query(uri, domain, r#"{"execute":"guest-get-disks"}"#)?
            .and_then(|v| v.as_array().map(Vec::len)),
    )
}

/// D4 for a disk: complete when the guest lists one more disk than before
/// the attach. Pure.
fn disk_outcome(before: Option<usize>, after: Option<usize>) -> HotplugOutcome {
    match (before, after) {
        (Some(b), Some(a)) if a > b => HotplugOutcome::Complete,
        (Some(b), Some(a)) => HotplugOutcome::Partial {
            reason: format!(
                "the domain has the disk but the guest still lists {a} disk(s), as before ({b})"
            ),
        },
        _ => HotplugOutcome::Partial {
            reason: "the domain took the disk, but no guest agent answers to say the guest \
                     sees it (ADR-0068 D4)"
                .into(),
        },
    }
}

/// D4 for a NIC: complete when the guest's agent lists an interface with
/// that MAC. Pure.
fn nic_outcome(mac: &str, interfaces: Option<&serde_json::Value>) -> HotplugOutcome {
    let Some(list) = interfaces.and_then(|v| v.as_array()) else {
        return HotplugOutcome::Partial {
            reason: "the domain took the NIC, but no guest agent answers to say the guest \
                     sees it (ADR-0068 D4)"
                .into(),
        };
    };
    let seen = list.iter().any(|i| {
        i.get("hardware-address")
            .and_then(|m| m.as_str())
            .is_some_and(|m| m.eq_ignore_ascii_case(mac))
    });
    if seen {
        HotplugOutcome::Complete
    } else {
        HotplugOutcome::Partial {
            reason: format!("the domain has the NIC {mac} but the guest does not list it yet"),
        }
    }
}

/// The connection a RUNNING domain lives on, or an error naming it.
fn live_domain_uri(vm: &Vm) -> delonix_model::Result<&'static str> {
    libvirt_domain_uri(&vm.name).ok_or_else(|| {
        delonix_model::Error::from(Error::Command {
            context: "vm hotplug",
            message: format!("libvirt does not know a domain named '{}'", vm.name),
        })
    })
}

/// `virsh`, with EVERY line of its stderr in the error: an `attach-device`
/// says «Failed to attach device from …» first and the reason («No more
/// available PCI slots») on the next line, which [`quiet`] drops.
fn virsh_hotplug<S: AsRef<str>>(
    args: &[S],
    context: &'static str,
) -> delonix_model::Result<String> {
    let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    match stable_cmd("virsh").args(&args).output() {
        Ok(out) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Ok(out) => Err(Error::Command {
            context,
            message: virsh_reason(&String::from_utf8_lossy(&out.stderr)),
        }
        .into()),
        Err(e) => Err(Error::Command {
            context,
            message: format!("virsh: {e}"),
        }
        .into()),
    }
}

/// Every non-empty stderr line of `virsh`, without its `error: ` prefix,
/// joined. Pure.
fn virsh_reason(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .map(|l| l.strip_prefix("error: ").unwrap_or(l))
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        "unknown error".into()
    } else {
        lines.join(": ")
    }
}

/// `virsh setvcpus`, named options only (a positional after a flag is the
/// ordering bug this file already paid for). `--live`: the next boot rebuilds
/// the domain from the record, which the engine updates (D6). Pure.
fn setvcpus_argv(uri: &str, name: &str, count: u32) -> Vec<String> {
    vec![
        "-c".into(),
        uri.into(),
        "setvcpus".into(),
        "--domain".into(),
        name.into(),
        "--count".into(),
        count.to_string(),
        "--live".into(),
    ]
}

/// `virsh vcpucount --live --active`: the vCPUs the domain HAS now. Pure.
fn vcpucount_argv(uri: &str, name: &str) -> Vec<String> {
    vec![
        "-c".into(),
        uri.into(),
        "vcpucount".into(),
        "--domain".into(),
        name.into(),
        "--live".into(),
        "--active".into(),
    ]
}

fn attach_device_argv(uri: &str, name: &str, file: &str) -> Vec<String> {
    vec![
        "-c".into(),
        uri.into(),
        "attach-device".into(),
        "--domain".into(),
        name.into(),
        "--file".into(),
        file.into(),
        "--live".into(),
    ]
}

/// One hot-added DIMM of `kib`, in guest node 0 (the only cell the domain is
/// booted with). Pure.
fn dimm_xml(kib: u64) -> String {
    format!(
        "<memory model='dimm'>\n  <target>\n    <size unit='KiB'>{kib}</size>\n    \
         <node>0</node>\n  </target>\n</memory>\n"
    )
}

fn parse_vcpucount(out: &str) -> Option<u32> {
    out.trim().parse().ok()
}

/// `virsh dominfo`'s `Max memory:` — the domain's memory INCLUDING plugged
/// DIMMs (`Used memory:` is the balloon target, not what is plugged). Pure.
fn parse_dominfo_memory_kib(out: &str) -> Option<u64> {
    out.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("Max memory:")?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// `true` when a `virsh qemu-agent-command` failure MEANS "nothing is
/// listening", not "something else broke". Pure, extracted out of the match
/// guard above so it is testable without a real `virsh`. `virsh` phrases
/// this in more than one way depending on version and on WHY the agent is
/// unreachable ("Guest agent is not responding: QEMU guest agent is not
/// connected", "domain is not running", "QEMU guest agent is not
/// configured") — all of them name either the agent or the domain's power
/// state, never anything else a real `virsh` failure (bad domain name,
/// `virsh` itself missing) would say.
fn agent_unreachable(message: &str) -> bool {
    let l = message.to_lowercase();
    l.contains("agent") || l.contains("not running")
}

/// `guest-get-osinfo`'s `(pretty-name, kernel-release)`, the same two fields
/// [`crate::capability`]'s `vm.guest-agent` entry promises. Pure.
fn parse_guest_osinfo(v: &serde_json::Value) -> (Option<String>, Option<String>) {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    (s("pretty-name").or_else(|| s("name")), s("kernel-release"))
}

/// `guest-get-fsinfo`'s mounted filesystems, in the guest's own order. An
/// entry without a mountpoint is skipped — the agent reports internal
/// devices (e.g. a bind mount's source) that way on some kernels. Pure.
fn parse_guest_fsinfo(v: &serde_json::Value) -> Vec<GuestFilesystem> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| {
                    Some(GuestFilesystem {
                        mountpoint: f.get("mountpoint")?.as_str()?.to_string(),
                        fstype: f
                            .get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                        used_bytes: f.get("used-bytes").and_then(|b| b.as_u64()),
                        total_bytes: f.get("total-bytes").and_then(|b| b.as_u64()),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
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

pub fn libvirt_poweroff(uri: &str, name: &str) -> Result<()> {
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
pub fn libvirt_cleanup(name: &str) -> Result<()> {
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

/// DIMM slots reserved for memory hotplug (ADR-0068 D3): each hot-add plugs
/// one DIMM, so this is how many separate memory additions a boot allows.
const MEMORY_HOTPLUG_SLOTS: u32 = 16;

/// Spare PCIe root ports reserved at boot for hot-added disks and NICs
/// (ADR-0068 D3/D7).
const SPARE_PCIE_ROOT_PORTS: u32 = 4;

/// The spare PCIe root ports a q35 domain with hotplug headroom reserves
/// (ADR-0068 D3/D7). **The `pcie-root` at index 0 is written explicitly**:
/// a port with no index takes the first free one, and with no root declared
/// that is index 0 — which libvirt refuses on q35 («The PCI controller with
/// index='0' must be model='pcie-root' for this machine type»), measured on
/// libvirt 10.0.0, so the domain could never be defined. Any other machine
/// type (i440fx `pc`) gets nothing: its PCI bus is hot-pluggable as it is,
/// and a PCIe port is not valid there. Pure.
///
/// **libvirt puts the boot devices on the declared ports FIRST** (measured:
/// with four bare ports declared, all of them carried a boot device and a
/// live attach answered «No more available PCI slots»). So the count is the
/// devices the domain places on root ports, plus the spare ones —
/// [`pcie_devices_at_boot`] errs on the side of one port too many, which
/// costs a free slot on bus 0; one too few costs the hotplug.
fn spare_pcie_ports_xml(cfg: &VmConfig, machine: &str, headroom: bool) -> String {
    if !headroom || !(machine.starts_with("q35") || machine.contains("-q35")) {
        return String::new();
    }
    let mut s = String::from("    <controller type='pci' index='0' model='pcie-root'/>\n");
    for _ in 0..pcie_devices_at_boot(cfg) + SPARE_PCIE_ROOT_PORTS {
        s.push_str("    <controller type='pci' model='pcie-root-port'/>\n");
    }
    s
}

/// The devices a q35 domain from `cfg` places on PCIe root ports at boot:
/// every virtio disk (the boot disk, virtio extras, the seed), a SCSI
/// controller when an extra uses it, every 9p share, every NIC, every
/// passed-through PCI device, a virtio video, and the three libvirt always
/// adds (virtio-serial for the agent channel, the memory balloon, the USB
/// controller). SATA hangs off the integrated ICH9 controller and a `vga`
/// video is integrated on bus 0: neither takes a port. Pure.
fn pcie_devices_at_boot(cfg: &VmConfig) -> u32 {
    let virtio_disks = cfg
        .extra_disks
        .iter()
        .filter(|d| matches!(d.bus.as_str(), "" | "virtio"))
        .count();
    let scsi = cfg.extra_disks.iter().any(|d| d.bus == "scsi") as usize;
    let video =
        matches!(cfg.video.as_deref(), Some("virtio" | "qxl")) || (cfg.video.is_none() && cfg.vnc);
    let n = 1 // boot disk
        + virtio_disks
        + scsi
        + cfg.seed.is_some() as usize
        + cfg.volumes.len()
        + 1 // primary NIC
        + cfg.extra_nics.len()
        + cfg.devices.len()
        + video as usize
        + 3; // virtio-serial, memballoon, USB
    n as u32
}

/// The hotplug ceilings that open headroom: `(Some(max vCPUs), Some(max KiB))`
/// only where the ceiling is ABOVE the boot size — a ceiling equal to it is no
/// headroom and changes nothing in the domain. Pure.
fn hotplug_ceilings(cfg: &VmConfig, vcpus: u32, mib: u64) -> (Option<u32>, Option<u64>) {
    let max_vcpus = cfg.vcpus_max.filter(|m| *m > vcpus);
    let max_kib = cfg.memory_max_mib.filter(|m| *m > mib).map(|m| m * 1024);
    (max_vcpus, max_kib)
}

/// `<vcpu>`, and with headroom the per-vCPU `<vcpus>` table libvirt needs to
/// hot-add them (`virsh setvcpus --live`). vCPU 0 is never hot-pluggable: the
/// guest cannot run without it. Pure.
fn libvirt_vcpu_xml(vcpus: u32, max: Option<u32>) -> String {
    let Some(max) = max else {
        return format!("  <vcpu placement='static'>{vcpus}</vcpu>\n");
    };
    let mut s = format!("  <vcpu placement='static' current='{vcpus}'>{max}</vcpu>\n");
    s.push_str("  <vcpus>\n");
    for i in 0..max {
        let enabled = if i < vcpus { "yes" } else { "no" };
        let hotpluggable = if i == 0 { "no" } else { "yes" };
        s.push_str(&format!(
            "    <vcpu id='{i}' enabled='{enabled}' hotpluggable='{hotpluggable}'/>\n"
        ));
    }
    s.push_str("  </vcpus>\n");
    s
}

/// The target of every extra disk at boot: a pinned one keeps its own; the
/// others take, in order, the next letter of their bus (`vd` for virtio,
/// `sd` otherwise) that no other disk holds. `vda` is the boot disk. Pure.
fn extra_disk_targets(disks: &[ExtraDisk]) -> Vec<String> {
    let pinned: Vec<&str> = disks.iter().filter_map(|d| d.target.as_deref()).collect();
    let mut taken: Vec<String> = vec!["vda".into()];
    taken.extend(pinned.iter().map(|t| t.to_string()));
    disks
        .iter()
        .map(|d| match &d.target {
            Some(t) => t.clone(),
            None => {
                let bus = if d.bus.is_empty() { "virtio" } else { &d.bus };
                let t = next_free_target(bus, &taken).unwrap_or_default();
                taken.push(t.clone());
                t
            }
        })
        .collect()
}

/// The cloud-init seed's target: the first virtio letter after `vda` that no
/// extra disk holds. Before, it was "the letter after the unpinned extras",
/// which a PINNED extra could already hold. Pure.
fn seed_target(extra_targets: &[String]) -> String {
    let mut taken = vec!["vda".to_string()];
    taken.extend(extra_targets.iter().cloned());
    next_free_target("virtio", &taken).unwrap_or_else(|| "vdz".into())
}

/// Pins every disk `new` adds over `old` to a letter the OLD layout leaves
/// free — its extras and its seed included — so adding a disk to a stopped VM
/// never moves the seed or a disk the guest already knows. Pure.
fn pin_added_disks(old: &[ExtraDisk], new: &mut [ExtraDisk]) {
    let old_targets = extra_disk_targets(old);
    let mut taken = vec!["vda".to_string(), seed_target(&old_targets)];
    taken.extend(old_targets);
    taken.extend(new.iter().filter_map(|d| d.target.clone()));
    for d in new.iter_mut() {
        if d.target.is_some() || old.iter().any(|o| o.source == d.source) {
            continue;
        }
        let bus = if d.bus.is_empty() {
            "virtio"
        } else {
            d.bus.as_str()
        };
        if let Some(t) = next_free_target(bus, &taken) {
            taken.push(t.clone());
            d.target = Some(t);
        }
    }
}

/// One extra disk's `<disk>` element at `target` — the SAME text the boot XML
/// and a live `attach-device` use, so a hot-added disk boots next time as the
/// guest saw it. Pure.
fn libvirt_extra_disk_xml(d: &ExtraDisk, target: &str) -> String {
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
    let mut s = format!("    <disk type='file' device='{}'>\n", xml_escape(device));
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
        xml_escape(target),
        xml_escape(bus)
    ));
    if d.read_only {
        s.push_str("      <readonly/>\n");
    }
    s.push_str("    </disk>\n");
    s
}

/// One extra NIC's `<interface>` — the same text for the boot XML and a live
/// `attach-device`. Pure.
fn libvirt_extra_nic_xml(n: &ExtraNic) -> String {
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
    let mut s = format!("    <interface type='{itype}'>\n");
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
    s
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
    // ADR-0068 D3: the hotplug CEILINGS are fixed here, at boot. Without them
    // there is no headroom, and every limit below is sized from the boot size
    // exactly as before; with them, the CONTAINMENT limits are sized from the
    // ceiling — a hard_limit computed from the boot memory would OOM-kill the
    // domain the moment the guest used the memory it was hot-added.
    let (max_vcpus, max_kib) = hotplug_ceilings(cfg, vcpus, mib);

    let mut s = String::new();
    s.push_str("<domain type='kvm'>\n");
    s.push_str(&format!("  <name>{name}</name>\n"));
    if let Some(max_kib) = max_kib {
        s.push_str(&format!(
            "  <maxMemory slots='{MEMORY_HOTPLUG_SLOTS}' unit='KiB'>{max_kib}</maxMemory>\n"
        ));
    }
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
    if let Some(limit_kib) = mem_hard_limit_kib(max_kib.unwrap_or(kib)) {
        s.push_str(&format!(
            "  <memtune>\n    <hard_limit unit='KiB'>{limit_kib}</hard_limit>\n  </memtune>\n"
        ));
    }
    s.push_str(&libvirt_vcpu_xml(vcpus, max_vcpus));
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
    let all_vcpus = max_vcpus.unwrap_or(vcpus);
    let cputune_quota = cpu_quota_micros(all_vcpus);
    if cfg.cpu_affinity.is_some() || cputune_quota.is_some() {
        s.push_str("  <cputune>\n");
        if let Some(q) = cputune_quota {
            s.push_str("    <period>100000</period>\n");
            s.push_str(&format!("    <quota>{q}</quota>\n"));
        }
        if let Some(list) = &cfg.cpu_affinity {
            let list = xml_escape(list);
            for v in 0..all_vcpus {
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
    // Memory hotplug needs a guest node topology: a DIMM is plugged into a
    // node. One cell, every vCPU, the boot memory.
    let node_cell = max_kib.map(|_| {
        format!(
            "    <numa>\n      <cell id='0' cpus='0-{}' memory='{kib}' unit='KiB'/>\n    </numa>\n",
            all_vcpus - 1
        )
    });
    s.push_str(&libvirt_cpu_xml(cfg, node_cell.as_deref()));
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
    // The backing file (the base image) is declared only when it is a
    // DIFFERENT file from the overlay. A `vm clone` hands the backend a
    // standalone qcow2 as both `disk` and overlay (the flattened copy IS the
    // disk — no chain): declaring it as its own `<backingStore>` opened the one
    // file twice and QEMU refused the second open ("Failed to get write
    // lock"), so the clone never booted. The comparison is on the canonical
    // paths because `cfg.disk` and `overlay` reach here spelled differently
    // (one relative, one absolute) for the very same file.
    let overlay_canon = std::fs::canonicalize(overlay)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| overlay.to_string());
    let base = (!cfg.disk.is_empty())
        .then(|| {
            std::fs::canonicalize(&cfg.disk)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| cfg.disk.clone())
        })
        .filter(|base| base != &overlay_canon);
    if let Some(base) = base {
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
    let targets = extra_disk_targets(&cfg.extra_disks);
    for (d, target) in cfg.extra_disks.iter().zip(&targets) {
        s.push_str(&libvirt_extra_disk_xml(d, target));
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
        let target = seed_target(&targets);
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
        s.push_str(&libvirt_extra_nic_xml(n));
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
    // QEMU guest agent channel (`vm.guest-agent`), always present: it is a
    // no-op unless something listens on `org.qemu.guest_agent.0` inside the
    // guest (the golden image's build recipe installs and enables it —
    // `qemu-guest-agent` — but nothing here can see whether a GIVEN guest
    // did), and `guest_info` already answers `Ok(None)` when it finds
    // nothing on the other end. No path, no source: libvirt manages the
    // unix socket under its own run directory.
    s.push_str("    <channel type='unix'>\n");
    s.push_str("      <target type='virtio' name='org.qemu.guest_agent.0'/>\n");
    s.push_str("    </channel>\n");
    // ADR-0068 D3/D7: on q35 a hot-added disk or NIC needs a FREE PCIe root
    // port, and libvirt only creates the ports the boot devices use. Spare
    // ports are reserved here, at boot — they cannot be added live.
    // Only with declared headroom (ADR-0068 D3): spare ports change the
    // domain's shape, which someone who did not ask for hotplug never gets.
    let headroom = max_vcpus.is_some() || max_kib.is_some();
    s.push_str(&spare_pcie_ports_xml(cfg, machine, headroom));
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
fn libvirt_cpu_xml(cfg: &VmConfig, node_cell: Option<&str>) -> String {
    let topo = cfg.cpu_topology.as_ref().map(|t| {
        format!(
            "    <topology sockets='{}' cores='{}' threads='{}'/>\n",
            t.sockets.max(1),
            t.cores.max(1),
            t.threads.max(1)
        )
    });
    // The node cell rides inside `<cpu>`, after the topology.
    let topo = match (topo, node_cell) {
        (None, None) => None,
        (t, n) => Some(format!("{}{}", t.unwrap_or_default(), n.unwrap_or(""))),
    };
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

/// Puts the network the operator NAMED (`--network <name>`) into `bridge`, the
/// field `libvirt_interface_xml` and `ensure_libvirt_network` read. An explicit
/// `bridge` wins, and the values that mean "the default network" map to
/// nothing — the same set Cloud Hypervisor and Proxmox use, via
/// [`names_a_declared_network`]. Excluding only `"default"` here (#866) sent
/// `ingress` — what every `vm create` without `--network` carries — to libvirt
/// as a network name, and the domain failed to start. **Pure.**
fn map_declared_network(cfg: &mut VmConfig) {
    if cfg.bridge.is_none() && names_a_declared_network(&cfg.network) {
        cfg.bridge = Some(cfg.network.clone());
    }
}

/// Generates the libvirt domain's `<interface>` from the YAML `net_mode` — so the
/// network is 100% abstracted (no hand-written XML). **Pure function** — tested without a daemon.
fn libvirt_interface_xml(cfg: &VmConfig, mac: &str) -> String {
    let mac = xml_escape(mac);
    let model = &format!(
        "      <model type='virtio'/>\n{}    </interface>\n",
        libvirt_filterref_xml(cfg.net_mode.as_deref(), cfg.allow_mac_spoofing)
    )[..];
    // The same function `vm describe` and the node API read the record back
    // with (`delonix_compute::vm::attached_network`), so what is reported is
    // what this XML says.
    match libvirt_nic(cfg.net_mode.as_deref(), cfg.bridge.as_deref(), &cfg.network) {
        LibvirtNic::Network(net) => {
            // NAT network managed by libvirt (DHCP + IP via domifaddr). `bridge` = name
            // of the libvirt network (default "default").
            format!(
                "    <interface type='network'>\n      <source network='{}'/>\n      <mac address='{mac}'/>\n{model}",
                xml_escape(&net)
            )
        }
        LibvirtNic::Bridge(br) => {
            // attaches to a pre-existing host bridge.
            format!(
                "    <interface type='bridge'>\n      <source bridge='{}'/>\n      <mac address='{mac}'/>\n{model}",
                xml_escape(&br)
            )
        }
        LibvirtNic::User => {
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

/// Whether this VM's NIC gets the filter: it can attach ([`antispoof_applies`])
/// and the VM did not opt out (`VmConfig::allow_mac_spoofing`, ADR-0055).
/// **Pure.** The one predicate both the XML and `boot` read, so "the filterref
/// is emitted" and "the filter is ensured first" cannot disagree.
fn antispoof_wanted(net_mode: Option<&str>, allow_mac_spoofing: bool) -> bool {
    antispoof_applies(net_mode) && !allow_mac_spoofing
}

/// Refuses `allow_mac_spoofing` where there is no filter to opt out of.
/// **Pure.**
///
/// The filter only exists on a libvirt NIC with a tap. Anywhere else the flag
/// would be accepted and change nothing — and an opt-out of a security
/// control that silently does nothing is the same lie as an opt-in that does
/// nothing: the operator reads the record, sees «spoofing allowed», and
/// diagnoses the wrong thing.
pub fn check_allow_mac_spoofing(cfg: &VmConfig, backend_id: &str) -> Result<()> {
    if !cfg.allow_mac_spoofing {
        return Ok(());
    }
    if backend_id != "libvirt" {
        return Err(Error::RequiresLibvirtBackend(format!(
            "--allow-mac-spoofing opts out of the libvirt anti-spoofing filter ('{ANTISPOOF_FILTER}'), \
             and the '{backend_id}' backend has none to opt out of — refusing rather than record an \
             opt-out that changes nothing. Use `--backend libvirt`, or drop the flag"
        )));
    }
    if !antispoof_applies(cfg.net_mode.as_deref()) {
        return Err(Error::RequiresLibvirtBackend(format!(
            "--allow-mac-spoofing needs a NIC with a tap (`--net-mode nat|bridge`): in '{}' mode \
             there is no filter to opt out of, so the flag would change nothing",
            cfg.net_mode.as_deref().unwrap_or("user")
        )));
    }
    Ok(())
}

/// The `<filterref>` block for this NIC, or empty. **Pure** — tested without a
/// daemon.
fn libvirt_filterref_xml(net_mode: Option<&str>, allow_mac_spoofing: bool) -> &'static str {
    if antispoof_wanted(net_mode, allow_mac_spoofing) {
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
/// The `<network>` XML of a NAT network called `name` over `cidr` — the one
/// place that turns an operator's `--network <name>` into a libvirt object.
///
/// **Pure**, so the address arithmetic is tested without a daemon. No
/// `<bridge>` element on purpose: libvirt then picks a free `virbrN` itself,
/// which is the only way to avoid colliding with the engine's OWN bridge for
/// the same network (`dlxn…`, in a rootless netns that `qemu:///system`
/// cannot see anyway).
///
/// Returns `None` for anything it cannot place an address range inside —
/// a malformed CIDR, or a prefix of /31 or /32, which has no room for a
/// gateway plus a DHCP range. The caller then leaves the network alone rather
/// than defining something broken.
///
/// The `<metadata>` element is the engine's mark of POSSESSION
/// ([`NET_OWNER_NS`]): the only networks [`ensure_libvirt_network`] may
/// redefine and [`release_owned_network`] may remove are the ones carrying it.
/// libvirt keeps a foreign-namespace metadata element verbatim (measured on
/// libvirt 10.0: `net-dumpxml` returns it, re-quoted with `"`).
fn libvirt_network_xml(name: &str, cidr: &str) -> Option<String> {
    let (base, prefix) = parse_cidr(cidr)?;
    if prefix > 30 {
        return None;
    }
    let mask: u32 = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    let base = u32::from(base);
    let gateway = std::net::Ipv4Addr::from(base + 1);
    let first = std::net::Ipv4Addr::from(base + 2);
    // `| !mask` is the broadcast; the last usable host is one below it.
    let last = std::net::Ipv4Addr::from((base | !mask) - 1);
    let netmask = std::net::Ipv4Addr::from(mask);
    let n = xml_escape(name);
    let subnet = std::net::Ipv4Addr::from(base);
    Some(format!(
        "<network>\n  <name>{n}</name>\n  \
         <metadata>\n    <delonix:network xmlns:delonix='{NET_OWNER_NS}' subnet='{subnet}/{prefix}'/>\n  </metadata>\n  \
         <forward mode='nat'/>\n  \
         <ip address='{gateway}' netmask='{netmask}'>\n    \
         <dhcp><range start='{first}' end='{last}'/></dhcp>\n  </ip>\n</network>\n"
    ))
}

/// The XML namespace of the `<metadata>` element that marks a libvirt network
/// as DEFINED BY THIS ENGINE. Possession is read from the object itself, never
/// inferred from its name: the host's own networks (`default`, a lab bridge,
/// another tool's) can carry any name a delonix network also has.
const NET_OWNER_NS: &str = "https://delonix.io/xmlns/libvirt/network/1";

/// `a.b.c.d/p` → the network address (host bits cleared) and the prefix.
/// **Pure.** `None` for a malformed CIDR or a prefix above 32.
fn parse_cidr(cidr: &str) -> Option<(std::net::Ipv4Addr, u32)> {
    let (addr, prefix) = cidr.trim().split_once('/')?;
    let prefix: u32 = prefix.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let ip: std::net::Ipv4Addr = addr.parse().ok()?;
    let mask: u32 = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    Some((std::net::Ipv4Addr::from(u32::from(ip) & mask), prefix))
}

/// The value of attribute `name` in one XML start tag, either quote style
/// (libvirt writes `'` for its own attributes and `"` for metadata ones).
/// **Pure.**
fn xml_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    while let Some(i) = rest.find(name) {
        let before_ok = i == 0 || rest[..i].ends_with(char::is_whitespace);
        let after = &rest[i + name.len()..];
        if before_ok {
            if let Some(v) = after.strip_prefix('=') {
                let q = v.chars().next()?;
                if q == '\'' || q == '"' {
                    let v = &v[1..];
                    return v.find(q).map(|end| &v[..end]);
                }
            }
        }
        rest = after;
    }
    None
}

/// What a libvirt network IS, read from its `net-dumpxml`: whether this engine
/// defined it, and the IPv4 subnet its first `<ip>` element puts it on.
#[derive(Debug, PartialEq, Eq)]
struct LiveNetwork {
    owned: bool,
    subnet: Option<(std::net::Ipv4Addr, u32)>,
}

/// **Pure** reader of `virsh net-dumpxml`. The subnet comes from the `<ip>`
/// element libvirt actually serves (`netmask=` or `prefix=`), not from the
/// engine's own metadata: an operator's `net-edit` changes the first and not
/// the second, and the question is where the VM would land.
fn parse_live_network(xml: &str) -> LiveNetwork {
    let owned = xml.contains(NET_OWNER_NS);
    let subnet = xml.match_indices("<ip ").find_map(|(i, _)| {
        let tag = &xml[i..i + xml[i..].find('>')?];
        if xml_attr(tag, "family") == Some("ipv6") {
            return None;
        }
        let addr = xml_attr(tag, "address")?;
        let prefix = match (xml_attr(tag, "prefix"), xml_attr(tag, "netmask")) {
            (Some(p), _) => p.parse().ok()?,
            (None, Some(m)) => {
                let m = u32::from(m.parse::<std::net::Ipv4Addr>().ok()?);
                // A non-contiguous mask is no subnet at all.
                if m.leading_ones() + m.trailing_zeros() != 32 {
                    return None;
                }
                m.leading_ones()
            }
            (None, None) => return None,
        };
        parse_cidr(&format!("{addr}/{prefix}"))
    });
    LiveNetwork { owned, subnet }
}

/// What [`ensure_libvirt_network`] does with a named network.
#[derive(Debug, PartialEq, Eq)]
enum NetPlan {
    /// The network is there and nothing says it is wrong.
    Use,
    /// It is not there: define it from this XML.
    Define(String),
    /// It is ours, on another subnet, and no domain is on it: replace it.
    Redefine(String),
    /// It is on another subnet and may not be replaced. The message says why.
    Refuse(String),
    /// It is not there and there is no subnet to define it with.
    Unresolved,
}

/// The decision, **pure**: the network `net` the VM asks for, the subnet
/// `declared` for the delonix network of that name, what libvirt has under that
/// name (`live`), and — asked only when a replacement is on the table — the
/// domains attached to it (`None` when they could not be listed).
///
/// The rule the test below pins: a network is used only when it is on the
/// declared subnet (or nothing was declared to compare with); otherwise it is
/// redefined when it carries the engine's mark AND no domain is on it, and
/// refused in every other case. Before (#866), an existing name was trusted:
/// redeclaring `cf-snet` on 10.231.80.0/24 and creating a VM in it gave rc 0
/// and a guest on the old 10.231.79.0/24, with `stack plan`/`drift` silent.
fn libvirt_network_plan(
    net: &str,
    declared: Option<&str>,
    live: Option<&LiveNetwork>,
    users: &dyn Fn() -> Option<Vec<String>>,
) -> NetPlan {
    let want = declared.and_then(parse_cidr);
    let Some(live) = live else {
        return match declared.and_then(|d| libvirt_network_xml(net, d)) {
            Some(xml) => NetPlan::Define(xml),
            None => NetPlan::Unresolved,
        };
    };
    let Some(want) = want else {
        return NetPlan::Use; // nothing declared to hold it against
    };
    if live.subnet == Some(want) {
        return NetPlan::Use;
    }
    let want_s = format!("{}/{}", want.0, want.1);
    let have = live
        .subnet
        .map_or_else(|| "no IPv4 subnet".to_string(), |(a, p)| format!("{a}/{p}"));
    if !live.owned {
        return NetPlan::Refuse(format!(
            "libvirt network '{net}' is on {have}, not on {want_s} as declared for the delonix \
             network '{net}', and delonix did not define it, so it is left as it is. Give the \
             delonix network another name, or change the libvirt one (`virsh net-edit {net}`)"
        ));
    }
    match users() {
        Some(u) if u.is_empty() => match declared.and_then(|d| libvirt_network_xml(net, d)) {
            Some(xml) => NetPlan::Redefine(xml),
            None => NetPlan::Use,
        },
        Some(u) => NetPlan::Refuse(format!(
            "libvirt network '{net}' is on {have}, not on {want_s} as declared for the delonix \
             network '{net}'. delonix defined it, but {} still on it ({}), so it is not redefined \
             under them. Remove them first, then create this VM again",
            if u.len() == 1 { "a VM is" } else { "VMs are" },
            u.join(", ")
        )),
        None => NetPlan::Refuse(format!(
            "libvirt network '{net}' is on {have}, not on {want_s} as declared for the delonix \
             network '{net}', and the domains on it could not be listed, so it is not redefined"
        )),
    }
}

/// The libvirt networks a domain's interfaces source from, from `virsh
/// domiflist` (the `Source` column of the `network` rows). **Pure.**
fn domiflist_networks(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            (cols.len() >= 3 && cols[1] == "network").then(|| cols[2].to_string())
        })
        .collect()
}

/// Every domain on `uri`, running or not, with an interface on network `net`.
/// `None` when the domains could not be listed — a caller about to remove or
/// replace the network reads that as "in use", never as "free".
fn domains_on_network(uri: &str, net: &str) -> Option<Vec<String>> {
    let names = capture("virsh", &["-c", uri, "list", "--all", "--name"])?;
    let mut on = Vec::new();
    for d in names.lines().map(str::trim).filter(|d| !d.is_empty()) {
        let ifs = capture("virsh", &["-c", uri, "domiflist", "--", d])?;
        if domiflist_networks(&ifs).iter().any(|n| n == net) {
            on.push(d.to_string());
        }
    }
    Some(on)
}

/// Removes the libvirt network `net` — only when it carries the engine's mark
/// and no domain is on it. Returns whether it removed it. A network the
/// engine did not define is never touched, whatever its name.
fn release_owned_network(uri: &str, net: &str) -> Result<bool> {
    let Some(xml) = capture("virsh", &["-c", uri, "net-dumpxml", "--", net]) else {
        return Ok(false); // already gone
    };
    if !parse_live_network(&xml).owned {
        return Ok(false);
    }
    if !domains_on_network(uri, net).is_some_and(|u| u.is_empty()) {
        return Ok(false);
    }
    // An inactive network refuses `net-destroy`; the undefine is what counts.
    let _ = stable_cmd("virsh")
        .args(["-c", uri, "net-destroy", "--", net])
        .output();
    run_quiet("virsh", &["-c", uri, "net-undefine", "--", net])?;
    Ok(true)
}

/// The libvirt network a recorded VM is on: an explicit `bridge`, else the
/// network it was created with when that names one — the same mapping `boot`
/// makes with `map_declared_network`. `None` for libvirt's own `default`.
fn libvirt_network_of(vm: &Vm) -> Option<&str> {
    recorded_libvirt_network(vm.boot.bridge.as_deref(), &vm.network)
}

/// Whether any of `records` — `(boot.bridge, network)` of each VM record left
/// — is on the libvirt network `net`, by the same mapping as
/// [`libvirt_network_of`]. **Pure.**
fn still_on_network<'a>(
    net: &str,
    records: impl IntoIterator<Item = (Option<&'a str>, &'a str)>,
) -> bool {
    records
        .into_iter()
        .any(|(bridge, network)| recorded_libvirt_network(bridge, network) == Some(net))
}

/// [`libvirt_network_of`] over the two record fields it reads. **Pure.**
fn recorded_libvirt_network<'a>(bridge: Option<&'a str>, network: &'a str) -> Option<&'a str> {
    bridge.or_else(|| names_a_declared_network(network).then_some(network))
}

/// Returns whether it DEFINED the network in this call, so a `boot` that fails
/// afterwards can take it back with it.
fn ensure_libvirt_network(uri: &str, net: &str, subnet: Option<&str>) -> Result<bool> {
    // System connection reachable? (NAT lives in qemu:///system.)
    if capture("virsh", &["-c", uri, "net-list", "--all"]).is_none() {
        eprintln!(
            "warning: cannot reach {uri} for NAT networking — add yourself to the \
'libvirt' group (`sudo usermod -aG libvirt $USER && newgrp libvirt`) and retry"
        );
        return Ok(false);
    }
    let live =
        capture("virsh", &["-c", uri, "net-dumpxml", "--", net]).map(|x| parse_live_network(&x));
    let exists = live.is_some();
    let mut defined_here = false;
    // A named network the operator asked for, with a subnet resolved from the
    // registry: define it — or, when the name is taken on another subnet,
    // redefine or refuse (`libvirt_network_plan`). Without the define the name
    // was accepted and dropped, and the VM silently used `default` instead;
    // without the check, an existing name was trusted whatever its subnet.
    if net != "default" {
        let plan =
            libvirt_network_plan(net, subnet, live.as_ref(), &|| domains_on_network(uri, net));
        match plan {
            NetPlan::Use => {}
            NetPlan::Refuse(why) => return Err(Error::NetworkSubnetConflict(why)),
            NetPlan::Redefine(xml) => {
                let _ = stable_cmd("virsh")
                    .args(["-c", uri, "net-destroy", "--", net])
                    .output();
                run_quiet("virsh", &["-c", uri, "net-undefine", "--", net])?;
                if !virsh_define_xml(uri, "net-define", &format!("libvirt-{net}"), &xml) {
                    return Err(Error::Command {
                        context: "libvirt",
                        message: format!(
                            "removed libvirt network '{net}' to put it on its declared subnet, \
                             and could not define it again (`virsh net-define`)"
                        ),
                    });
                }
                tracing::debug!(network = %net, "libvirt network redefined on its declared subnet");
                defined_here = true;
            }
            NetPlan::Define(xml) => {
                // A failed define is an ERROR, not a warning. It used to be a
                // `tracing::warn!` promising that «the VM will fall back to
                // libvirt's 'default' network» — there is no such fallback: the
                // domain names `net`, and `virsh define`/`start` then failed on
                // «no network with matching name». Stopping here stops BEFORE
                // the domain is defined.
                if !virsh_define_xml(uri, "net-define", &format!("libvirt-{net}"), &xml) {
                    return Err(Error::Command {
                        context: "libvirt",
                        message: format!(
                            "could not define the libvirt network '{net}' on {uri} \
                             (`virsh net-define` failed)"
                        ),
                    });
                }
                defined_here = true;
            }
            NetPlan::Unresolved => {
                // Reached only past the engine's «is it declared?» refusal, so
                // the network exists on this node — but no usable subnet came
                // with it (a caller that did not resolve `network_subnet`, or a
                // /31–/32). Same honesty as above: the domain cannot get it.
                return Err(Error::Command {
                    context: "libvirt",
                    message: format!(
                        "network '{net}' is not defined in libvirt and no usable subnet was \
                         resolved for it, so it cannot be defined there (check \
                         `delonix network inspect {net}`)"
                    ),
                });
            }
        }
    }
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
    Ok(defined_here)
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
fn libvirt_backup_disk_live(name: &str, dest: &Path, quiesce: bool) -> Result<()> {
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

impl VmBackend for LibvirtBackend {
    fn backup_disk_live(
        &self,
        _vmdir: &Path,
        vm: &Vm,
        dest: &Path,
        quiesce: bool,
    ) -> delonix_model::Result<()> {
        Ok(libvirt_backup_disk_live(&vm.name, dest, quiesce)?)
    }
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
        // `--network <name>` now REACHES the domain. This provider used to have
        // zero readers of `cfg.network`: the name was accepted, recorded and
        // shown by `vm ls`, while `bridge` stayed `None` and the `nat` arm below
        // fell through to `unwrap_or("default")` — so a cluster asked for on
        // `--network lab-net` had every node on libvirt's `default` instead,
        // which is the behaviour `network_of`'s doc-comment records as found
        // live. Mapping it here (and not in `libvirt_interface_xml`) keeps the
        // XML generator pure and leaves ONE place that decides the network.
        map_declared_network(&mut cfg);
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
        // A network this call defined goes again if the boot does not get as
        // far as a domain on it — else a failed `vm create` left it behind.
        let mut net_defined_here: Option<&str> = None;
        let undo_net = |n: Option<&str>| {
            if let Some(n) = n {
                let _ = release_owned_network(uri, n);
            }
        };
        if matches!(cfg.net_mode.as_deref(), Some("nat") | Some("network")) {
            on(CreateStage::Network);
            let net = cfg.bridge.as_deref().unwrap_or("default");
            if ensure_libvirt_network(uri, net, cfg.network_subnet.as_deref())? {
                net_defined_here = Some(net);
            }
            // Static IP: DHCP reservation MAC→IP on the libvirt network, BEFORE
            // the domain boots (the guest's DHCP request must already find it).
            if let Some(ip) = cfg.static_ip.as_deref() {
                if let Err(e) = libvirt_reserve_ip(uri, net, &mac, ip) {
                    undo_net(net_defined_here);
                    return Err(e.into());
                }
            }
        }
        // The NIC's `<filterref>` is emitted by `libvirt_domain_xml` below, and
        // `virsh define` REFUSES a domain naming a filter the daemon does not
        // have. So the filter has to exist first, and a failure here has to
        // abort — see `ensure_antispoof_filter` for why the alternative (drop
        // the filterref and boot anyway) is the one thing not on the table.
        //
        // A VM that opted out (ADR-0055) emits no filterref, so the filter is
        // not needed — and not ensured, so an opt-out VM does not fail on a
        // daemon that cannot define it. It still says so, every boot: the
        // record is the durable trace, this is the one in the journal.
        if antispoof_wanted(cfg.net_mode.as_deref(), cfg.allow_mac_spoofing) {
            ensure_antispoof_filter(uri)?;
        } else if cfg.allow_mac_spoofing {
            tracing::warn!(
                vm = %cfg.name,
                "anti-spoofing filter '{ANTISPOOF_FILTER}' NOT attached (allow_mac_spoofing, ADR-0055): this guest may emit frames with any source MAC"
            );
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
        delonix_node::write_atomic_mode(&xml_path, xml.as_bytes(), None)?;

        // Idempotent: if the domain already exists (auto-heal), it just (re)starts; otherwise
        // define + start. `virsh start` on an already-running domain is a benign no-op.
        let defined = capture("virsh", &["-c", uri, "domstate", "--", &cfg.name]).is_some();
        if !defined {
            on(CreateStage::Define);
            if let Err(e) = run_quiet("virsh", &["-c", uri, "define", &xml_path.to_string_lossy()])
            {
                let _ = std::fs::remove_file(&xml_path);
                undo_net(net_defined_here);
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
                undo_net(net_defined_here);
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
                LeasePick::OnlyStale | LeasePick::NoLeaseForMac => None,
                LeasePick::TableUnreadable => self.ip_uri(uri, &cfg.name),
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
            // it would hand back the very address just rejected, or (for a MAC
            // with no row at all) an answer the table has already given: none.
            LeasePick::OnlyStale | LeasePick::NoLeaseForMac => None,
            LeasePick::TableUnreadable => self.ip_uri(uri, &vm.name),
        }
    }

    /// What the guest says about itself through `qemu-guest-agent`
    /// (`vm.guest-agent`), via `virsh qemu-agent-command` — the SAME agent
    /// the golden image's own build recipe already installs and enables
    /// (`vmimage.rs`, "`qemu-guest-agent` nas três receitas"). `Ok(None)`
    /// when the VM is not running, the domain was defined before this
    /// backend started adding the guest-agent channel (an old record is
    /// never redefined except by `vm start`/`vm create`), or nothing
    /// answers on the other end of the channel — the SAME distinction the
    /// trait's own doc-comment draws: a question the guest does not answer
    /// is not a failure of this call.
    fn guest_info(&self, vm: &Vm) -> delonix_model::Result<Option<GuestInfo>> {
        if !self.is_running(vm) {
            return Ok(None);
        }
        let Some(uri) = libvirt_domain_uri(&vm.name) else {
            return Ok(None);
        };
        let Some(os_raw) = qemu_agent_query(uri, &vm.name, r#"{"execute":"guest-get-osinfo"}"#)?
        else {
            return Ok(None);
        };
        let (os, kernel) = parse_guest_osinfo(&os_raw);
        let hostname = qemu_agent_query(uri, &vm.name, r#"{"execute":"guest-get-host-name"}"#)?
            .and_then(|h| h.get("host-name")?.as_str().map(str::to_string));
        let filesystems = qemu_agent_query(uri, &vm.name, r#"{"execute":"guest-get-fsinfo"}"#)?
            .map(|v| parse_guest_fsinfo(&v))
            .unwrap_or_default();
        let agent_version = qemu_agent_query(uri, &vm.name, r#"{"execute":"guest-info"}"#)?
            .and_then(|i| i.get("version")?.as_str().map(str::to_string));
        Ok(Some(GuestInfo {
            os,
            kernel,
            hostname,
            agent_version,
            filesystems,
        }))
    }

    /// `virsh setvcpus --live` (ADR-0068 Phase 2), up to the `<vcpu>` maximum
    /// the domain was booted with (D3). The count read back is the domain's
    /// live `vcpucount` (D5); whether the GUEST onlined them is asked of its
    /// agent (`guest-get-vcpus`), polled for `wait` (D4). No agent, or vCPUs
    /// the guest leaves offline (an image without the udev rule, D8), is
    /// `Partial` — never success.
    fn hotplug_add_cpu(
        &self,
        _vmdir: &Path,
        vm: &Vm,
        target_vcpus: u32,
        wait: std::time::Duration,
    ) -> delonix_model::Result<(u32, HotplugOutcome)> {
        let ceiling = vm.vcpus_max.unwrap_or(vm.vcpus);
        if target_vcpus > ceiling {
            return Err(Error::InvalidResize(format!(
                "VM '{}' has no headroom for {target_vcpus} vCPUs — the declared maximum is \
                 {ceiling} (see `vm resize --vcpus-max`, ADR-0068 D3)",
                vm.name
            ))
            .into());
        }
        let uri = live_domain_uri(vm)?;
        virsh_hotplug(
            &setvcpus_argv(uri, &vm.name, target_vcpus),
            "virsh setvcpus",
        )?;
        let out = virsh_hotplug(&vcpucount_argv(uri, &vm.name), "virsh vcpucount")?;
        let assigned = parse_vcpucount(&out).ok_or_else(|| {
            delonix_model::Error::from(Error::Command {
                context: "virsh vcpucount",
                message: format!("not a count: {out}"),
            })
        })?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            let guest = qemu_agent_query(uri, &vm.name, r#"{"execute":"guest-get-vcpus"}"#)?;
            let online = guest.as_ref().map(guest_online_vcpus);
            match cpu_outcome(assigned, online) {
                HotplugOutcome::Complete => return Ok((assigned, HotplugOutcome::Complete)),
                partial if std::time::Instant::now() >= deadline => return Ok((assigned, partial)),
                _ => std::thread::sleep(std::time::Duration::from_millis(500)),
            }
        }
    }

    /// A DIMM through `virsh attach-device --live` (ADR-0068 Phase 3), into
    /// the `<maxMemory>` slots reserved at boot (D3). The size read back is
    /// the domain's live memory from `dominfo` (D5); the guest side is its
    /// agent's memory blocks — an image that does not online hot-added
    /// memory (D8: no `memhp_default_state=online` nor udev rule) leaves them
    /// offline, which is `Partial`.
    fn hotplug_add_memory(
        &self,
        vmdir: &Path,
        vm: &Vm,
        target_mib: u64,
        wait: std::time::Duration,
    ) -> delonix_model::Result<(u64, HotplugOutcome)> {
        let ceiling = vm.memory_max_mib.unwrap_or_else(|| mem_mib(&vm.memory));
        if target_mib > ceiling {
            return Err(Error::InvalidResize(format!(
                "VM '{}' has no headroom for {target_mib} MiB — the declared maximum is \
                 {ceiling} MiB (see `vm resize --memory-max`, ADR-0068 D3)",
                vm.name
            ))
            .into());
        }
        let uri = live_domain_uri(vm)?;
        let current_mib = parse_dominfo_memory_kib(&virsh_hotplug(
            &["-c", uri, "dominfo", "--domain", &vm.name],
            "virsh dominfo",
        )?)
        .map(|k| k / 1024)
        .unwrap_or_else(|| mem_mib(&vm.memory));
        if target_mib <= current_mib {
            return Err(Error::InvalidResize(format!(
                "VM '{}' already has {current_mib} MiB live (asked for {target_mib} MiB)",
                vm.name
            ))
            .into());
        }
        let xml_path = vmdir.join(format!("{}.dimm.xml", vm.name));
        std::fs::write(&xml_path, dimm_xml((target_mib - current_mib) * 1024)).map_err(|e| {
            delonix_model::Error::from(Error::Command {
                context: "vm hotplug memory",
                message: format!("{}: {e}", xml_path.display()),
            })
        })?;
        let xml = xml_path.to_string_lossy().into_owned();
        let attached = virsh_hotplug(
            &attach_device_argv(uri, &vm.name, &xml),
            "virsh attach-device",
        );
        let _ = std::fs::remove_file(&xml_path);
        attached?;
        let assigned = parse_dominfo_memory_kib(&virsh_hotplug(
            &["-c", uri, "dominfo", "--domain", &vm.name],
            "virsh dominfo",
        )?)
        .map(|k| k / 1024)
        .ok_or_else(|| {
            delonix_model::Error::from(Error::Command {
                context: "virsh dominfo",
                message: "no 'Max memory' line — the live memory size is unknown".into(),
            })
        })?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            let blocks =
                qemu_agent_query(uri, &vm.name, r#"{"execute":"guest-get-memory-blocks"}"#)?;
            let info = qemu_agent_query(
                uri,
                &vm.name,
                r#"{"execute":"guest-get-memory-block-info"}"#,
            )?;
            let state = match (blocks, info) {
                (Some(b), Some(i)) => guest_memory_state(&b, &i),
                _ => None,
            };
            match memory_outcome(assigned, state) {
                HotplugOutcome::Complete => return Ok((assigned, HotplugOutcome::Complete)),
                partial if std::time::Instant::now() >= deadline => return Ok((assigned, partial)),
                _ => std::thread::sleep(std::time::Duration::from_millis(500)),
            }
        }
    }

    /// `vm update --disk-add` on a running domain (ADR-0068 Phase 4): the
    /// SAME `<disk>` the boot XML writes, at the next target letter free in
    /// the LIVE domain (pinned, so the next boot keeps it), through
    /// `attach-device --live` — which needs a spare PCIe port on q35 (D3).
    /// Read back with `domblklist`; the guest agent's disk list growing is
    /// what makes it complete (D4).
    fn hotplug_add_disk(
        &self,
        vmdir: &Path,
        vm: &Vm,
        disk: &ExtraDisk,
        wait: std::time::Duration,
        attached: &mut dyn FnMut(&ExtraDisk) -> delonix_model::Result<()>,
    ) -> delonix_model::Result<(ExtraDisk, HotplugOutcome)> {
        if disk.device == "cdrom" {
            return Err(Error::InvalidResize(format!(
                "VM '{}': a cdrom is not hot-plugged — stop the VM and add it",
                vm.name
            ))
            .into());
        }
        if !Path::new(&disk.source).is_file() {
            return Err(Error::InvalidResize(format!(
                "VM '{}': the disk to add '{}' is not a file on this host",
                vm.name, disk.source
            ))
            .into());
        }
        let uri = live_domain_uri(vm)?;
        let live = parse_domblklist(&virsh_hotplug(
            &["-c", uri, "domblklist", "--domain", &vm.name],
            "virsh domblklist",
        )?);
        let bus = if disk.bus.is_empty() {
            "virtio"
        } else {
            &disk.bus
        };
        let target = match &disk.target {
            Some(t) if live.contains(t) => {
                return Err(Error::InvalidResize(format!(
                    "VM '{}' already has a disk at '{t}'",
                    vm.name
                ))
                .into())
            }
            Some(t) => t.clone(),
            None => next_free_target(bus, &live).ok_or_else(|| {
                delonix_model::Error::from(Error::InvalidResize(format!(
                    "VM '{}' has no free {bus} target letter left",
                    vm.name
                )))
            })?,
        };
        let mut pinned = disk.clone();
        pinned.target = Some(target.clone());
        let before = guest_disk_count(uri, &vm.name)?;
        attach_xml(
            vmdir,
            vm,
            uri,
            &libvirt_extra_disk_xml(&pinned, &target),
            "disk",
        )?;
        let after_live = parse_domblklist(&virsh_hotplug(
            &["-c", uri, "domblklist", "--domain", &vm.name],
            "virsh domblklist",
        )?);
        if !after_live.contains(&target) {
            return Err(Error::Command {
                context: "virsh attach-device",
                message: format!(
                    "libvirt accepted the disk but '{target}' is not in the domain's disk list"
                ),
            }
            .into());
        }
        // Recorded now (D6), before the guest wait a kill could interrupt.
        attached(&pinned)?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            let after = guest_disk_count(uri, &vm.name)?;
            let outcome = disk_outcome(before, after);
            match outcome {
                HotplugOutcome::Complete => return Ok((pinned, outcome)),
                partial if std::time::Instant::now() >= deadline => return Ok((pinned, partial)),
                _ => std::thread::sleep(std::time::Duration::from_millis(500)),
            }
        }
    }

    /// `vm update --nic-add` on a running domain (ADR-0068 Phase 5): the
    /// SAME `<interface>` the boot XML writes, with a MAC derived from the VM
    /// and pinned, through `attach-device --live`. Read back with
    /// `domiflist`; the guest agent listing that MAC makes it complete (D4).
    fn hotplug_add_nic(
        &self,
        vmdir: &Path,
        vm: &Vm,
        nic: &ExtraNic,
        wait: std::time::Duration,
        attached: &mut dyn FnMut(&ExtraNic) -> delonix_model::Result<()>,
    ) -> delonix_model::Result<(ExtraNic, HotplugOutcome)> {
        let uri = live_domain_uri(vm)?;
        let mut pinned = nic.clone();
        let mac = match &nic.mac {
            Some(m) => m.clone(),
            None => extra_nic_mac(vm),
        };
        pinned.mac = Some(mac.clone());
        attach_xml(vmdir, vm, uri, &libvirt_extra_nic_xml(&pinned), "nic")?;
        let macs = parse_domiflist_macs(&virsh_hotplug(
            &["-c", uri, "domiflist", "--domain", &vm.name],
            "virsh domiflist",
        )?);
        if !macs.iter().any(|m| m.eq_ignore_ascii_case(&mac)) {
            return Err(Error::Command {
                context: "virsh attach-device",
                message: format!(
                    "libvirt accepted the NIC but {mac} is not in the domain's interface list"
                ),
            }
            .into());
        }
        attached(&pinned)?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            let guest = qemu_agent_query(
                uri,
                &vm.name,
                r#"{"execute":"guest-network-get-interfaces"}"#,
            )?;
            let outcome = nic_outcome(&mac, guest.as_ref());
            match outcome {
                HotplugOutcome::Complete => return Ok((pinned, outcome)),
                partial if std::time::Instant::now() >= deadline => return Ok((pinned, partial)),
                _ => std::thread::sleep(std::time::Duration::from_millis(500)),
            }
        }
    }

    /// Extra disks/NICs of a STOPPED VM: the record is the whole definition
    /// and the next boot rebuilds the domain from it, so nothing else changes.
    fn change_devices_cold(
        &self,
        _vmdir: &Path,
        old: &Vm,
        new: &mut Vm,
    ) -> delonix_model::Result<()> {
        pin_added_disks(&old.boot.extra_disks, &mut new.boot.extra_disks);
        Ok(())
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

    /// `vm resize --disk-size` (`vm.disk.resize`): grows the overlay
    /// `boot`/`start` always build at `vmdir/<name>.qcow2` — this backend
    /// keeps no OTHER record of where the disk is; the domain XML just
    /// names this same path. Re-checks `is_running` itself (the engine
    /// already did, against the record) because this is the one resize
    /// path that touches a FILE: a stale record saying "stopped" while
    /// something else started the domain would otherwise let `qemu-img
    /// resize` run against a disk QEMU holds open.
    fn resize_disk(&self, vmdir: &Path, vm: &Vm, new_bytes: u64) -> delonix_model::Result<()> {
        if self.is_running(vm) {
            return Err(Error::ResizeNeedsStopped(format!(
                "VM '{}' is running: stop it first (`delonix vm stop {}`)",
                vm.name, vm.name
            ))
            .into());
        }
        let overlay = vmdir.join(format!("{}.qcow2", vm.name));
        let current = qcow2_virtual_size(&overlay)?;
        if new_bytes <= current {
            return Err(Error::InvalidResize(format!(
                "VM '{}' disk is already {} bytes — a disk can grow, never shrink (asked for {} bytes)",
                vm.name, current, new_bytes
            ))
            .into());
        }
        quiet(
            "qemu-img",
            &[
                "resize",
                "--",
                &overlay.to_string_lossy(),
                &new_bytes.to_string(),
            ],
        )
        .map(|_| ())
        .map_err(|e| {
            delonix_model::Error::from(Error::Command {
                context: "qemu-img resize",
                message: e,
            })
        })
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
            // The network the reservation went to: `boot` maps a declared
            // `--network` into `bridge`, which the record does not keep, so
            // `boot.bridge` alone sent this release to `default`.
            let net = libvirt_network_of(vm).unwrap_or("default");
            libvirt_release_ip(uri, net, &vm.mac, ip);
        }
        Ok(())
    }

    fn release_network(&self, vm: &Vm, remaining: &[Vm]) -> delonix_model::Result<()> {
        let Some(net) = libvirt_network_of(vm) else {
            return Ok(()); // libvirt's own `default`: never ours to remove
        };
        // A record still on it — by `--network` or by `--bridge`, running or
        // stopped — keeps it. Comparing `network` fields alone missed a VM put
        // on it with `--bridge <name>` (recorded as `network: ingress`): the
        // `rm` of the other one removed the network and its `start` failed.
        if still_on_network(
            net,
            remaining
                .iter()
                .map(|o| (o.boot.bridge.as_deref(), o.network.as_str())),
        ) {
            return Ok(());
        }
        let uri = libvirt_uri_for(Some(vm.tap.as_str()));
        if release_owned_network(uri, net)? {
            tracing::debug!(network = %net, "libvirt network removed with its last VM");
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
            delonix_node::write_atomic_mode(&dir.join(format!("{n}.xml")), xml.as_bytes(), None)?;
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
                    delonix_node::write_atomic_mode(&tmp, patched.as_bytes(), None)
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
        domiflist_networks(&out).into_iter().next()
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
    /// not a heuristic. Falls back to [`Self::ip_uri`] (`domifaddr`) only when
    /// there is no such table to read at all (a non-libvirt-managed network) —
    /// NOT when the table was read and says this VM has no address.
    /// `floor` is this boot's [`Vm::dhcp_lease_floor`]: leases at or below it
    /// are an earlier boot's (see [`pick_lease_ip`]). A table that cannot be
    /// read is [`LeasePick::TableUnreadable`], which keeps the `domifaddr`
    /// fallback; a table that CAN be read and holds nothing for this MAC is
    /// [`LeasePick::NoLeaseForMac`], which does not (see that variant).
    fn ip_from_leases(uri: &str, name: &str, mac: &str, floor: Option<&str>) -> LeasePick {
        Self::leases_of(uri, name).map_or(LeasePick::TableUnreadable, |out| {
            pick_lease_ip(&out, mac, floor)
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The address arithmetic of a NAT network, for the two prefixes that
    /// actually show up: a /16 (what `network create` hands out) and a /24.
    /// Gateway is the first host, the DHCP range runs from the second host to
    /// the last one BELOW the broadcast — getting that off by one is how you
    /// ship a network that leases its own broadcast address.
    #[test]
    fn libvirt_network_xml_places_gateway_and_range_inside_the_subnet() {
        let x = libvirt_network_xml("k8s-net", "10.229.0.0/16").expect("valid /16");
        assert!(x.contains("<name>k8s-net</name>"), "{x}");
        assert!(x.contains("address='10.229.0.1'"), "{x}");
        assert!(x.contains("netmask='255.255.0.0'"), "{x}");
        assert!(x.contains("start='10.229.0.2' end='10.229.255.254'"), "{x}");

        let x = libvirt_network_xml("lab", "192.168.50.0/24").expect("valid /24");
        assert!(x.contains("address='192.168.50.1'"), "{x}");
        assert!(x.contains("netmask='255.255.255.0'"), "{x}");
        assert!(
            x.contains("start='192.168.50.2' end='192.168.50.254'"),
            "{x}"
        );
    }

    /// A subnet whose address is not the network address still yields the
    /// network's own gateway — the operator writing `10.229.5.7/16` means that
    /// /16, and the XML must not inherit the stray host part.
    #[test]
    fn libvirt_network_xml_masks_the_address_down_to_the_network() {
        let x = libvirt_network_xml("n", "10.229.5.7/16").expect("valid");
        assert!(x.contains("address='10.229.0.1'"), "{x}");
    }

    /// NO `<bridge>` element, deliberately: libvirt then picks a free
    /// `virbrN`. Naming one here is how you collide with the engine's OWN
    /// bridge for the same network (`dlxn…`), which lives in a rootless netns
    /// that `qemu:///system` cannot see anyway.
    #[test]
    fn libvirt_network_xml_lets_libvirt_choose_the_bridge() {
        let x = libvirt_network_xml("k8s-net", "10.229.0.0/16").expect("valid");
        assert!(!x.contains("<bridge"), "must not pin a bridge name: {x}");
        assert!(x.contains("<forward mode='nat'/>"), "{x}");
    }

    /// Refuses what it cannot place a range inside, instead of emitting XML
    /// that libvirt would reject (or worse, accept). /31 and /32 have no room
    /// for a gateway plus a range; a malformed CIDR has no meaning at all.
    #[test]
    fn libvirt_network_xml_refuses_what_has_no_room() {
        assert!(libvirt_network_xml("n", "10.0.0.0/31").is_none());
        assert!(libvirt_network_xml("n", "10.0.0.0/32").is_none());
        assert!(libvirt_network_xml("n", "10.0.0.0").is_none());
        assert!(libvirt_network_xml("n", "not-a-cidr/16").is_none());
        assert!(libvirt_network_xml("n", "10.0.0.0/abc").is_none());
    }

    /// What `net-dumpxml` returned for a network defined from
    /// `libvirt_network_xml`, measured 2026-10-21 on libvirt 10.0: the
    /// metadata comes back re-quoted with `"`, and libvirt adds the bridge,
    /// the MAC and the uuid.
    const LIVE_OWNED: &str = r#"<network>
  <name>cf-snet</name>
  <uuid>eb08f0ea-3f71-43fd-81ef-9052b0aa122f</uuid>
  <metadata>
    <delonix:network xmlns:delonix="https://delonix.io/xmlns/libvirt/network/1" subnet="10.231.79.0/24"/>
  </metadata>
  <forward mode='nat'/>
  <bridge name='virbr2' stp='on' delay='0'/>
  <mac address='52:54:00:66:64:6c'/>
  <ip address='10.231.79.1' netmask='255.255.255.0'>
    <dhcp>
      <range start='10.231.79.2' end='10.231.79.254'/>
    </dhcp>
  </ip>
</network>
"#;

    /// The host's own `default`, as distros ship it: no mark.
    const LIVE_FOREIGN: &str = "<network>\n  <name>default</name>\n  <forward mode='nat'/>\n  \
        <bridge name='virbr0' stp='on' delay='0'/>\n  \
        <ip address='192.168.122.1' netmask='255.255.255.0'>\n  </ip>\n</network>\n";

    fn cidr(s: &str) -> Option<(std::net::Ipv4Addr, u32)> {
        parse_cidr(s)
    }

    /// The network the engine defines carries its mark, and reading it back
    /// (in either quote style) gives the subnet it was defined on.
    #[test]
    fn a_network_the_engine_defines_reads_back_as_its_own() {
        let x = libvirt_network_xml("cf-snet", "10.231.80.7/24").expect("valid");
        assert!(x.contains(NET_OWNER_NS), "{x}");
        assert!(x.contains("subnet='10.231.80.0/24'"), "{x}");
        let live = parse_live_network(&x);
        assert_eq!(
            live,
            LiveNetwork {
                owned: true,
                subnet: cidr("10.231.80.0/24")
            }
        );
        assert_eq!(
            parse_live_network(LIVE_OWNED),
            LiveNetwork {
                owned: true,
                subnet: cidr("10.231.79.0/24")
            }
        );
        assert_eq!(
            parse_live_network(LIVE_FOREIGN),
            LiveNetwork {
                owned: false,
                subnet: cidr("192.168.122.0/24")
            }
        );
    }

    /// `prefix=` instead of `netmask=`, an IPv6 element before the IPv4 one,
    /// a network with no address at all (a host-bridge forward), and a mask
    /// that is no subnet.
    #[test]
    fn the_live_subnet_is_the_first_ipv4_element() {
        let x = "<network><ip family='ipv6' address='fd00::1' prefix='64'/>\
                 <ip address='10.9.8.1' prefix='16'/></network>";
        assert_eq!(parse_live_network(x).subnet, cidr("10.9.0.0/16"));
        let x = "<network><forward mode='bridge'/><bridge name='br0'/></network>";
        assert_eq!(parse_live_network(x).subnet, None);
        let x = "<network><ip address='10.0.0.1' netmask='255.0.255.0'/></network>";
        assert_eq!(parse_live_network(x).subnet, None);
    }

    /// The decision table of `libvirt_network_plan`. `users` panics where the
    /// plan must not need to ask (listing domains costs a `virsh` per domain).
    #[test]
    fn a_network_name_is_used_only_on_its_declared_subnet() {
        let never = || -> Option<Vec<String>> { panic!("users asked for no reason") };
        let owned = |c: &str| LiveNetwork {
            owned: true,
            subnet: cidr(c),
        };
        let foreign = |c: &str| LiveNetwork {
            owned: false,
            subnet: cidr(c),
        };

        // Absent: defined with the mark, or left to libvirt when no subnet.
        match libvirt_network_plan("cf-snet", Some("10.231.80.0/24"), None, &never) {
            NetPlan::Define(x) => {
                assert!(x.contains(NET_OWNER_NS) && x.contains("10.231.80.1"), "{x}")
            }
            p => panic!("{p:?}"),
        }
        assert_eq!(
            libvirt_network_plan("cf-snet", None, None, &never),
            NetPlan::Unresolved
        );

        // Present on the declared subnet — ours or not — or nothing to compare.
        let want = Some("10.231.80.0/24");
        for live in [owned("10.231.80.0/24"), foreign("10.231.80.0/24")] {
            assert_eq!(
                libvirt_network_plan("cf-snet", want, Some(&live), &never),
                NetPlan::Use
            );
        }
        assert_eq!(
            libvirt_network_plan("cf-snet", None, Some(&owned("10.231.79.0/24")), &never),
            NetPlan::Use
        );

        // THE MEASURED CASE: the name redeclared on 10.231.80.0/24 while
        // libvirt still had 10.231.79.0/24. It was `Use`, rc 0, VM on .79.
        let stale = owned("10.231.79.0/24");
        match libvirt_network_plan("cf-snet", want, Some(&stale), &|| Some(vec![])) {
            NetPlan::Redefine(x) => assert!(x.contains("10.231.80.1"), "{x}"),
            p => panic!("ours and unused must be redefined: {p:?}"),
        }
        match libvirt_network_plan("cf-snet", want, Some(&stale), &|| {
            Some(vec!["cf-vm1".into()])
        }) {
            NetPlan::Refuse(m) => assert!(
                m.contains("10.231.79.0/24")
                    && m.contains("10.231.80.0/24")
                    && m.contains("cf-vm1"),
                "{m}"
            ),
            p => panic!("a VM on it must block the redefine: {p:?}"),
        }
        assert!(matches!(
            libvirt_network_plan("cf-snet", want, Some(&stale), &|| None),
            NetPlan::Refuse(_)
        ));

        // Not ours: refused without asking who is on it — never redefined.
        match libvirt_network_plan("labnet", want, Some(&foreign("10.0.0.0/24")), &never) {
            NetPlan::Refuse(m) => assert!(m.contains("did not define"), "{m}"),
            p => panic!("{p:?}"),
        }
        let no_ip = LiveNetwork {
            owned: false,
            subnet: None,
        };
        match libvirt_network_plan("labnet", want, Some(&no_ip), &never) {
            NetPlan::Refuse(m) => assert!(m.contains("no IPv4 subnet"), "{m}"),
            p => panic!("{p:?}"),
        }
    }

    /// `domiflist` rows of type `network` only; a `bridge` row names a host
    /// bridge, not a libvirt network.
    #[test]
    fn domiflist_networks_reads_only_network_rows() {
        let out = " Interface   Type      Source    Model    MAC\n\
                   ---------------------------------------------------------\n \
                   vnet0       network   cf-snet   virtio   52:54:00:aa:bb:cc\n \
                   vnet1       bridge    br0       virtio   52:54:00:aa:bb:cd\n \
                   vnet2       network   default   virtio   52:54:00:aa:bb:ce\n";
        assert_eq!(domiflist_networks(out), ["cf-snet", "default"]);
    }

    /// The network a RECORD is on, for `destroy`'s DHCP release and for
    /// `release_network`: the declared one when the record names one, never
    /// libvirt's `default`. `boot.bridge` alone was `None` for a VM created
    /// with `--network`, so the static-IP release went to `default`.
    #[test]
    fn a_record_is_on_its_declared_network() {
        assert_eq!(recorded_libvirt_network(None, "cf-snet"), Some("cf-snet"));
        assert_eq!(recorded_libvirt_network(None, "ingress"), None);
        assert_eq!(recorded_libvirt_network(None, "default"), None);
        assert_eq!(
            recorded_libvirt_network(Some("br-lab"), "ingress"),
            Some("br-lab")
        );
    }

    /// BUG-1 of the cli-tester's run on 2026-10-21: VM A on `--network ct-x`,
    /// VM B on `--net-mode nat --bridge ct-x` (recorded as `network: ingress,
    /// bridge: ct-x`), B stopped, `vm rm A` — and the network went, because
    /// the count compared `network` fields only; `vm start B` then failed
    /// with «Network not found». B's record keeps it now.
    #[test]
    fn a_vm_on_the_network_by_bridge_keeps_it() {
        let b = (Some("ct-x"), "ingress");
        assert!(still_on_network("ct-x", [b]));
        assert!(still_on_network("ct-x", [(None, "ct-x")]));
        assert!(!still_on_network(
            "ct-x",
            [(None, "ingress"), (None, "ct-y")]
        ));
        assert!(!still_on_network("ct-x", []));
    }

    /// REGRESSION (the whole point of the `network_subnet` field): with the
    /// libvirt network resolved into `bridge`, the domain's NIC sources THAT
    /// network. Before, `bridge` stayed `None` on the `cluster kubeadm` path
    /// and this arm fell through to `unwrap_or("default")`, so every node of a
    /// cluster asked for on `--network k8s-net` ran on libvirt's `default` —
    /// measured live via `virsh domiflist`.
    #[test]
    fn interface_xml_sources_the_requested_network_not_default() {
        let cfg = VmConfig {
            name: "lab-cp1".into(),
            net_mode: Some("nat".into()),
            bridge: Some("k8s-net".into()),
            ..Default::default()
        };
        let x = libvirt_interface_xml(&cfg, "52:54:00:aa:bb:cc");
        assert!(x.contains("<source network='k8s-net'/>"), "{x}");
        assert!(!x.contains("network='default'"), "{x}");
    }

    /// REGRESSION: every tool whose OUTPUT this crate parses runs with a
    /// pinned locale. `virsh` is a gettext program, and with its catalogues
    /// installed and `LANG=pt_PT` a running domain would report as stopped.
    /// This crate carries its own copy of `stable_cmd` (a provider may not
    /// depend on another provider or on an adapter, docs/discovery/61), so
    /// the copy carries its own guard. Dropping the `LC_ALL` from
    /// `stable_cmd` makes this test fail.
    #[test]
    fn stable_cmd_pins_the_locale_so_the_output_is_machine_stable() {
        let cmd = stable_cmd("virsh");
        let envs: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(envs.get("LC_ALL").and_then(|v| v.as_deref()), Some("C"));
        assert_eq!(envs.get("LANG").and_then(|v| v.as_deref()), Some("C"));
    }

    /// The liveness decisions this locale pin protects are still made against
    /// the English `virsh` strings. The check reads this file's CODE only
    /// (comments and the test module cut away): the same assertion over the
    /// whole file would be satisfied by its own string literals and by the
    /// doc comment on `stable_cmd`, and so would never fail.
    #[test]
    fn liveness_is_decided_against_the_english_virsh_states() {
        let src = include_str!("lib.rs");
        let code: String = src
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap()
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            code.contains(r#"state == "shut off""#),
            "the power-off comparison changed shape; re-check that stable_cmd still covers it"
        );
        assert!(
            code.contains(r#"s == "running""#),
            "the liveness comparison changed shape; re-check that stable_cmd still covers it"
        );
    }
    use delonix_compute::{VmBootSpec, VmVolume};

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
            allow_mac_spoofing: false,
            ..Default::default()
        }
    }

    #[test]
    fn every_domain_carries_the_guest_agent_channel() {
        let cfg = test_vm_cfg("128M");
        let xml = super::libvirt_domain_xml(&cfg, "/vms/t.qcow2", &super::mac_for(&cfg.name));
        assert!(
            xml.contains("org.qemu.guest_agent.0"),
            "no guest-agent channel in:\n{xml}"
        );
    }

    #[test]
    fn agent_unreachable_recognizes_the_virsh_phrasings() {
        assert!(super::agent_unreachable(
            "Guest agent is not responding: QEMU guest agent is not connected"
        ));
        assert!(super::agent_unreachable(
            "Requested operation is not valid: domain is not running"
        ));
        assert!(super::agent_unreachable(
            "internal error: unable to execute QEMU agent command: QEMU guest agent is not configured"
        ));
        // A real virsh failure unrelated to the agent must NOT be swallowed.
        assert!(!super::agent_unreachable("failed to get domain 'ghost'"));
        assert!(!super::agent_unreachable("virsh: command not found"));
    }

    #[test]
    fn parse_guest_osinfo_prefers_pretty_name_and_falls_back_to_name() {
        let v = serde_json::json!({"pretty-name": "Debian GNU/Linux 12 (bookworm)", "name": "debian", "kernel-release": "6.1.0-amd64"});
        assert_eq!(
            super::parse_guest_osinfo(&v),
            (
                Some("Debian GNU/Linux 12 (bookworm)".to_string()),
                Some("6.1.0-amd64".to_string())
            )
        );
        let no_pretty = serde_json::json!({"name": "alpine"});
        assert_eq!(
            super::parse_guest_osinfo(&no_pretty),
            (Some("alpine".to_string()), None)
        );
    }

    #[test]
    fn parse_guest_fsinfo_skips_entries_without_a_mountpoint() {
        let v = serde_json::json!([
            {"mountpoint": "/", "type": "ext4", "used-bytes": 1000, "total-bytes": 2000},
            {"name": "sda1"},
            {"mountpoint": "/boot", "type": "vfat"},
        ]);
        let fs = super::parse_guest_fsinfo(&v);
        assert_eq!(fs.len(), 2);
        assert_eq!(fs[0].mountpoint, "/");
        assert_eq!(fs[0].used_bytes, Some(1000));
        assert_eq!(fs[1].mountpoint, "/boot");
        assert_eq!(fs[1].used_bytes, None);
    }

    #[test]
    fn parse_virtual_size_reads_the_real_qemu_img_json_shape() {
        // Captured live on this host (`qemu-img info --output=json`), not
        // guessed — the field this parser depends on is real.
        let real = r#"{
            "virtual-size": 10737418240,
            "filename": "/vms/t.qcow2",
            "format": "qcow2",
            "actual-size": 200704,
            "format-specific": {"type": "qcow2", "data": {}}
        }"#;
        assert_eq!(super::parse_virtual_size(real), Some(10737418240));
        assert_eq!(super::parse_virtual_size("not json"), None);
        assert_eq!(super::parse_virtual_size(r#"{"format":"qcow2"}"#), None);
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
            LeasePick::NoLeaseForMac
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
    /// REGRESSION, diagnosed live on 2026-10-15: `cluster kubeadm --name lab
    /// --control-plane 1 --workers 2` ran every host's prep against ONE machine
    /// because all three node VMs resolved to `192.168.122.176`. The lease table
    /// below is the one captured at the time — a SINGLE row, for `lab-w2`'s MAC;
    /// `lab-cp1` and `lab-w1` had no row of their own at all.
    ///
    /// A MAC with no row of its own must resolve to NO address, and it must say
    /// so as [`LeasePick::NoLeaseForMac`] and not as
    /// [`LeasePick::TableUnreadable`]: the two used to be one variant, and the
    /// shared variant let the resolution fall through to `domifaddr` — another
    /// reader of this same table — instead of answering "no address" and letting
    /// the `--boot-timeout` wait trip. Collapsing them back makes this fail.
    #[test]
    fn a_vm_with_no_lease_of_its_own_never_resolves_to_a_siblings_address() {
        let (cp1, w1, w2) = (
            "52:54:00:e2:55:fb",
            "52:54:00:b7:c8:ef",
            "52:54:00:b7:ca:82",
        );
        let out = "\
 Expiry Time           MAC address         Protocol   IP address           Hostname   Client ID or DUID
------------------------------------------------------------------------------------------------------------------------------------------------
 2026-10-15 12:05:00   52:54:00:b7:ca:82   ipv4       192.168.122.176/24   lab-w2     ff:56:50:4d:98:00:02:00:00:ab:11:1c:6b:f5:9e:94:dd:d5:2e";

        // The one MAC that HAS the lease is the only one that gets the address.
        assert_eq!(
            pick_lease_ip(out, w2, None),
            LeasePick::Current("192.168.122.176".into())
        );
        // The parser never attributes a row to another MAC...
        for mac in [cp1, w1] {
            assert!(
                lease_entries(out, mac).is_empty(),
                "{mac} has no row in this table"
            );
            assert_eq!(parse_leases_latest_ip(out, mac), None);
            // ...and resolution says WHY there is no address, so the caller
            // cannot re-ask a second reader of this same table.
            assert_eq!(pick_lease_ip(out, mac, None), LeasePick::NoLeaseForMac);
        }
        // The floor is orthogonal: a MAC with no row has nothing to floor, and
        // the answer stays "no address" rather than becoming "cannot tell".
        assert_eq!(leases_max_expiry(out, cp1), None);
        assert_eq!(
            pick_lease_ip(out, cp1, Some("2026-10-15 11:00:00")),
            LeasePick::NoLeaseForMac
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
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp = tmp_dir.path();
        let dir = super::snapshot_meta_dir(tmp, "dev");
        // No directory at all is the normal case (a VM that was never stopped,
        // or never had a snapshot) — not an error, and never a panic.
        assert!(super::preserved_snapshot_names(tmp, "dev").is_empty());
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["s2.xml", "s1.xml", "s1.xml.redefine", "notes.txt"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        assert_eq!(
            super::preserved_snapshot_names(tmp, "dev"),
            vec!["s1".to_string(), "s2".to_string()]
        );
        tmp_dir.close().unwrap();
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
    fn the_filter_stays_on_unless_asked() {
        // The default of the struct every caller builds with
        // `..Default::default()` must be the FILTERED one.
        assert!(!VmConfig::default().allow_mac_spoofing);
        assert!(!VmBootSpec::default().allow_mac_spoofing);
        assert!(super::antispoof_wanted(Some("bridge"), false));
        assert!(!super::antispoof_wanted(Some("bridge"), true));
    }
    #[test]
    fn the_opt_out_is_refused_where_there_is_no_filter() {
        let mut cfg = test_vm_cfg("128M");
        cfg.allow_mac_spoofing = true;
        cfg.net_mode = Some("bridge".into());
        assert!(super::check_allow_mac_spoofing(&cfg, "libvirt").is_ok());
        // Another backend has no nwfilter to opt out of.
        for b in ["cloud-hypervisor", "proxmox"] {
            let e = super::check_allow_mac_spoofing(&cfg, b).unwrap_err();
            assert!(matches!(e, Error::RequiresLibvirtBackend(_)), "{b}");
        }
        // Nor does a user-mode NIC, which has no tap.
        for mode in [None, Some("user")] {
            cfg.net_mode = mode.map(Into::into);
            assert!(super::check_allow_mac_spoofing(&cfg, "libvirt").is_err());
        }
        // Without the flag nothing is checked, whatever the backend.
        cfg.allow_mac_spoofing = false;
        assert!(super::check_allow_mac_spoofing(&cfg, "proxmox").is_ok());
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
    #[test]
    fn a_session_only_host_keeps_the_lifecycle_and_loses_the_observed_address() {
        let host = LibvirtHost {
            system_uri: false,
            ..LibvirtHost::ASSUMED
        };
        let r = libvirt_report(&host);
        assert!(r.available);
        assert_eq!(r.health.reason, "SessionOnly");
        let by = |c: C| {
            r.capabilities
                .iter()
                .find(|x| x.capability == c)
                .unwrap()
                .state
                .clone()
        };
        assert_eq!(by(C::VmStart).label(), "supported");
        assert_eq!(by(C::VmNetworkNat).label(), "unavailable-on-host");
        // A declared negative stays a negative whatever the host looks like.
        assert_eq!(by(C::VmMigrationLive).label(), "not-implemented");
    }
    #[test]
    fn without_virsh_nothing_that_needs_it_reads_as_supported() {
        let host = LibvirtHost {
            virsh: false,
            ..LibvirtHost::ASSUMED
        };
        let r = libvirt_report(&host);
        assert!(!r.available);
        assert_eq!(r.health.reason, "VirshMissing");
        assert!(r
            .capabilities
            .iter()
            .all(|c| { c.state.label() != "supported" || c.capability == C::HostHealth }));
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
            allow_mac_spoofing: false,
            ..Default::default()
        }
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
            super::libvirt_filterref_xml(Some("nat"), false),
            super::ANTISPOOF_FILTERREF
        );
        assert_eq!(
            super::libvirt_filterref_xml(Some("network"), false),
            super::ANTISPOOF_FILTERREF
        );
        assert_eq!(
            super::libvirt_filterref_xml(Some("bridge"), false),
            super::ANTISPOOF_FILTERREF
        );
        // `user` (SLIRP/passt) has no tap. Emitting there would be accepted and
        // ignored — exactly what this repo has already corrected three times.
        assert_eq!(super::libvirt_filterref_xml(Some("user"), false), "");
        assert_eq!(super::libvirt_filterref_xml(None, false), "");
    }
    #[test]
    fn an_opted_out_nic_carries_no_filterref() {
        // ADR-0055: the opt-out removes the ONE line and nothing else.
        for mode in ["nat", "network", "bridge", "user"] {
            assert_eq!(super::libvirt_filterref_xml(Some(mode), true), "", "{mode}");
        }
        let mut cfg = test_vm_cfg("128M");
        cfg.net_mode = Some("bridge".into());
        cfg.bridge = Some("br0".into());
        let mac = super::mac_for("t");
        let on = super::libvirt_interface_xml(&cfg, &mac);
        cfg.allow_mac_spoofing = true;
        let off = super::libvirt_interface_xml(&cfg, &mac);
        assert!(on.contains("filterref"));
        assert!(!off.contains("filterref"));
        assert_eq!(on.replace(super::ANTISPOOF_FILTERREF, ""), off);
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
    fn the_default_network_values_never_reach_libvirt_as_a_name() {
        // Measured 2026-10-21 on main: `vm create --backend libvirt` without
        // `--network` failed with "no network with matching name 'ingress'",
        // because only "default" was excluded and `ingress` is what the bin
        // puts in `network` when the flag is absent.
        for default in ["", "ingress", "bridge", "default"] {
            let mut c = hpc_cfg();
            c.network = default.into();
            c.net_mode = Some("nat".into());
            super::map_declared_network(&mut c);
            assert_eq!(c.bridge, None, "{default:?}");
            let xml = libvirt_interface_xml(&c, "52:54:00:00:00:01");
            assert!(
                xml.contains("source network='default'"),
                "{default:?}: {xml}"
            );
        }
        // A name the operator asked for still reaches the domain (#866).
        let mut c = hpc_cfg();
        c.network = "lab-net".into();
        c.net_mode = Some("nat".into());
        super::map_declared_network(&mut c);
        assert_eq!(c.bridge.as_deref(), Some("lab-net"));
        // And an explicit bridge is never overwritten by it.
        c.bridge = Some("br0".into());
        super::map_declared_network(&mut c);
        assert_eq!(c.bridge.as_deref(), Some("br0"));
    }
    /// What `vm describe`, `vm ls -o json` and the node API report for a libvirt
    /// VM is read back from its RECORD; the domain is built from the config.
    /// Both go through `libvirt_nic`, and this holds them to each other for
    /// every net mode: the record as `boot` writes it (`tap` = the net mode, the
    /// boot spec from the config before `map_declared_network`) reports the
    /// network the generated `<interface>` names.
    #[test]
    fn the_reported_network_is_the_one_in_the_domain_xml() {
        for mode in [
            None,
            Some("user"),
            Some("nat"),
            Some("network"),
            Some("bridge"),
        ] {
            for network in ["", "ingress", "default", "lab-net"] {
                for bridge in [None, Some("br-x")] {
                    let mut c = hpc_cfg();
                    c.net_mode = mode.map(str::to_string);
                    c.network = network.into();
                    c.bridge = bridge.map(str::to_string);
                    let record: delonix_compute::Vm = serde_json::from_value(serde_json::json!({
                        "name": c.name, "disk": c.disk, "overlay": "/o.qcow2",
                        "vcpus": c.vcpus, "memory": c.memory, "network": c.network,
                        "tap": mode.unwrap_or("user"), "mac": "52:54:00:00:00:01",
                        "pid": null, "api_socket": "", "status": "Running",
                        "created_unix": 1, "backend": "libvirt",
                        "boot": delonix_compute::vm::boot_spec_of(&c),
                    }))
                    .unwrap();
                    super::map_declared_network(&mut c);
                    let xml = libvirt_interface_xml(&c, "52:54:00:00:00:01");
                    let case = format!("{mode:?} {network:?} {bridge:?}: {xml}");
                    match delonix_compute::vm::attached_network(&record) {
                        None => assert!(xml.contains("type='user'"), "{case}"),
                        Some(n) => assert!(
                            xml.contains(&format!("source network='{n}'"))
                                || xml.contains(&format!("source bridge='{n}'")),
                            "reported {n:?} — {case}"
                        ),
                    }
                }
            }
        }
    }
}

/// ADR-0068 Phases 2–3 on libvirt: the ceilings fixed at boot (D3), the argv
/// that adds live, and the outcome the guest's agent decides (D4, D8).
#[cfg(test)]
mod hotplug_tests {
    use super::*;

    fn cfg(vcpus: u32, memory: &str, vmax: Option<u32>, mmax: Option<u64>) -> VmConfig {
        VmConfig {
            name: "hp".into(),
            disk: String::new(),
            vcpus,
            memory: memory.into(),
            vcpus_max: vmax,
            memory_max_mib: mmax,
            ..Default::default()
        }
    }

    /// The defect this closes: `vm create --vcpus-max/--memory-max` recorded
    /// a ceiling the domain never had — no `current=`, no `<maxMemory>` — so
    /// every live add failed in libvirt after the engine had accepted it.
    #[test]
    fn a_declared_ceiling_opens_headroom_in_the_domain() {
        let xml = libvirt_domain_xml(
            &cfg(2, "2048M", Some(8), Some(8192)),
            "/o.qcow2",
            "52:54:00:00:00:01",
        );
        assert!(
            xml.contains("<vcpu placement='static' current='2'>8</vcpu>"),
            "{xml}"
        );
        assert!(
            xml.contains("<vcpu id='0' enabled='yes' hotpluggable='no'/>"),
            "{xml}"
        );
        assert!(
            xml.contains("<vcpu id='1' enabled='yes' hotpluggable='yes'/>"),
            "{xml}"
        );
        assert!(
            xml.contains("<vcpu id='7' enabled='no' hotpluggable='yes'/>"),
            "{xml}"
        );
        assert!(
            xml.contains("<maxMemory slots='16' unit='KiB'>8388608</maxMemory>"),
            "{xml}"
        );
        assert!(xml.contains("<memory unit='KiB'>2097152</memory>"), "{xml}");
        assert!(
            xml.contains("<cell id='0' cpus='0-7' memory='2097152' unit='KiB'/>"),
            "a DIMM needs a NUMA node to plug into: {xml}"
        );
        // The boot devices take ports first: boot disk, NIC, virtio-serial,
        // balloon and USB here — five — and four stay spare.
        assert_eq!(
            xml.matches("model='pcie-root-port'").count(),
            5 + 4,
            "{xml}"
        );
        // libvirt refuses a q35 domain whose index 0 is a port: the root has
        // to be declared, at index 0, BEFORE any port takes that index.
        let root = xml
            .find("<controller type='pci' index='0' model='pcie-root'/>")
            .expect("no pcie-root at index 0");
        assert!(root < xml.find("pcie-root-port").unwrap(), "{xml}");
    }

    /// The containment limits follow the ceiling: a hard_limit sized from the
    /// boot memory OOM-kills the domain once the guest uses a hot-added DIMM.
    #[test]
    fn the_containment_limits_are_sized_from_the_ceiling() {
        let with = libvirt_domain_xml(&cfg(2, "2048M", Some(8), Some(8192)), "/o", "m");
        let without = libvirt_domain_xml(&cfg(2, "2048M", None, None), "/o", "m");
        if let Some(limit) = mem_hard_limit_kib(8192 * 1024) {
            assert!(with.contains(&format!("<hard_limit unit='KiB'>{limit}</hard_limit>")));
        }
        if let Some(q) = cpu_quota_micros(8) {
            assert!(with.contains(&format!("<quota>{q}</quota>")), "{with}");
        }
        assert!(without.contains("<vcpu placement='static'>2</vcpu>"));
        assert!(!without.contains("maxMemory") && !without.contains("<numa>"));
        assert!(
            !without.contains("pcie-root-port"),
            "no headroom, no spare ports"
        );
    }

    /// i440fx (`pc`) has a hot-pluggable PCI bus of its own and no PCIe:
    /// a ceiling there emits no port, and no `pcie-root` either.
    #[test]
    fn a_pc_machine_gets_no_pcie_controller() {
        let mut c = cfg(2, "2048M", Some(8), Some(8192));
        c.machine = Some("pc".into());
        let xml = libvirt_domain_xml(&c, "/o.qcow2", "m");
        assert!(!xml.contains("pcie-root"), "{xml}");
        assert!(
            xml.contains("<maxMemory"),
            "the memory headroom stays: {xml}"
        );
    }

    /// The domain is DEFINED by the real libvirt, never booted: `virsh define`
    /// on `qemu:///session`, then `undefine`. Run with `--ignored` on a host
    /// with libvirt; this is what caught the missing `pcie-root`.
    #[test]
    #[ignore = "needs virsh and qemu:///session; defines and undefines a domain, boots nothing"]
    fn libvirt_accepts_the_headroom_domain() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("d.qcow2");
        std::fs::write(&disk, b"").unwrap();
        let mut c = cfg(2, "2048M", Some(4), Some(4096));
        c.name = format!("dlx18-define-{}", std::process::id());
        let xml = libvirt_domain_xml(&c, &disk.to_string_lossy(), "52:54:00:00:18:01");
        let file = dir.path().join("d.xml");
        std::fs::write(&file, &xml).unwrap();
        let uri = "qemu:///session";
        let def = quiet("virsh", &["-c", uri, "define", &file.to_string_lossy()]);
        let _ = quiet("virsh", &["-c", uri, "undefine", "--", &c.name]);
        def.unwrap_or_else(|e| panic!("libvirt refused the domain: {e}\n{xml}"));
    }

    /// How many `pcie-root-port` controllers of a DEFINED domain's XML carry
    /// no device: a port's `index` is the PCI bus a device behind it names.
    fn free_root_ports(xml: &str) -> usize {
        let attr = |l: &str, k: &str| -> Option<String> {
            let i = l.find(&format!("{k}='"))? + k.len() + 2;
            Some(l[i..].split('\'').next()?.to_string())
        };
        let ports: Vec<u32> = xml
            .lines()
            .filter(|l| l.contains("model='pcie-root-port'") && l.contains("<controller"))
            .filter_map(|l| attr(l, "index")?.parse().ok())
            .collect();
        let busy: Vec<u32> = xml
            .lines()
            .filter(|l| l.contains("<address type='pci'"))
            .filter_map(|l| u32::from_str_radix(attr(l, "bus")?.trim_start_matches("0x"), 16).ok())
            .collect();
        ports.iter().filter(|p| !busy.contains(p)).count()
    }

    /// Every device that takes a root port is counted; SATA and a plain
    /// `vga` (integrated on bus 0) are not.
    #[test]
    fn the_port_count_follows_the_devices() {
        let base = cfg(2, "2048M", Some(4), None);
        assert_eq!(pcie_devices_at_boot(&base), 5);
        let mut more = base.clone();
        more.seed = Some("/s.iso".into());
        more.extra_disks = vec![
            ExtraDisk {
                source: "/a".into(),
                ..Default::default()
            },
            ExtraDisk {
                source: "/b".into(),
                bus: "sata".into(),
                ..Default::default()
            },
        ];
        more.extra_nics = vec![ExtraNic::default()];
        more.vnc = true;
        assert_eq!(pcie_devices_at_boot(&more), 5 + 1 + 1 + 1 + 1);
    }

    #[test]
    fn free_root_ports_reads_a_defined_domain() {
        let xml = "<controller type='pci' index='1' model='pcie-root-port'>\n                   <address type='pci' domain='0x0000' bus='0x00' slot='0x02' function='0x0'/>\n                   <controller type='pci' index='2' model='pcie-root-port'>\n                   <address type='pci' domain='0x0000' bus='0x01' slot='0x00' function='0x0'/>\n";
        assert_eq!(free_root_ports(xml), 1);
    }

    /// A headroom domain as libvirt DEFINES it (addresses assigned): at least
    /// four root ports must carry nothing, or a live disk/NIC add has no slot
    /// — the review measured all six taken by the boot devices.
    #[test]
    #[ignore = "needs virsh and qemu:///session; defines and undefines a domain, boots nothing"]
    fn the_spare_ports_are_still_free_once_libvirt_places_the_devices() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("d.qcow2");
        std::fs::write(&disk, b"").unwrap();
        let seed = dir.path().join("seed.iso");
        std::fs::write(&seed, b"").unwrap();
        let mut c = cfg(2, "2048M", Some(4), Some(4096));
        c.name = format!("dlx18-ports-{}", std::process::id());
        c.seed = Some(seed.to_string_lossy().into_owned());
        c.extra_disks = vec![ExtraDisk {
            source: disk.to_string_lossy().into_owned(),
            ..Default::default()
        }];
        let xml = libvirt_domain_xml(&c, &disk.to_string_lossy(), "52:54:00:00:18:02");
        let file = dir.path().join("d.xml");
        std::fs::write(&file, &xml).unwrap();
        let uri = "qemu:///session";
        let def = quiet("virsh", &["-c", uri, "define", &file.to_string_lossy()]);
        let dumped = quiet("virsh", &["-c", uri, "dumpxml", "--", &c.name]);
        let _ = quiet("virsh", &["-c", uri, "undefine", "--", &c.name]);
        def.unwrap_or_else(|e| panic!("libvirt refused the domain: {e}\n{xml}"));
        let dumped = dumped.expect("dumpxml");
        let free = free_root_ports(&dumped);
        assert!(free >= 4, "only {free} free root port(s):\n{dumped}");
    }

    /// A real `attach-device --live` of a disk into a RUNNING headroom domain.
    /// The domain has an empty disk, so the VMM runs with no OS; it is
    /// destroyed and undefined at the end.
    #[test]
    #[ignore = "needs virsh, qemu:///session and /dev/kvm; runs a VMM with no OS"]
    fn a_disk_attaches_live_to_a_running_headroom_domain() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |n: &str| {
            let p = dir.path().join(n);
            let out = std::process::Command::new("qemu-img")
                .args(["create", "-q", "-f", "qcow2"])
                .arg(&p)
                .arg("64M")
                .status()
                .unwrap();
            assert!(out.success());
            p.to_string_lossy().into_owned()
        };
        let root = mk("root.qcow2");
        let extra = mk("extra.qcow2");
        let mut c = cfg(1, "256M", Some(2), Some(512));
        c.name = format!("dlx18-attach-{}", std::process::id());
        // A session libvirt cannot apply cgroup tuning: the containment
        // blocks go, the rest of the domain is the one this code writes.
        let strip = |x: String, tag: &str| -> String {
            match (x.find(&format!("<{tag}>")), x.find(&format!("</{tag}>"))) {
                (Some(a), Some(b)) => format!("{}{}", &x[..a], &x[b + tag.len() + 3..]),
                _ => x,
            }
        };
        let xml = libvirt_domain_xml(&c, &root, "52:54:00:00:18:03");
        let xml = strip(strip(xml, "memtune"), "cputune");
        let file = dir.path().join("d.xml");
        std::fs::write(&file, &xml).unwrap();
        let uri = "qemu:///session";
        let cleanup = |name: &str| {
            let _ = quiet("virsh", &["-c", uri, "destroy", "--", name]);
            let _ = quiet("virsh", &["-c", uri, "undefine", "--", name]);
        };
        let result = (|| -> std::result::Result<String, String> {
            quiet("virsh", &["-c", uri, "define", &file.to_string_lossy()])?;
            virsh_hotplug(&["-c", uri, "start", "--", &c.name], "virsh start")
                .map_err(|e| e.to_string())?;
            let d = ExtraDisk {
                source: extra.clone(),
                ..Default::default()
            };
            let dx = dir.path().join("disk.xml");
            std::fs::write(&dx, libvirt_extra_disk_xml(&d, "vdc")).unwrap();
            let out = virsh_hotplug(
                &attach_device_argv(uri, &c.name, &dx.to_string_lossy()),
                "virsh attach-device",
            )
            .map_err(|e| e.to_string())?;
            let blk = quiet("virsh", &["-c", uri, "domblklist", "--domain", &c.name])?;
            Ok(format!("{out}\n{blk}"))
        })();
        cleanup(&c.name);
        let out = result.unwrap_or_else(|e| panic!("live attach failed: {e}\n{xml}"));
        assert!(parse_domblklist(&out).contains(&"vdc".to_string()), "{out}");
    }

    /// A ceiling equal to the boot size is no headroom: the domain is the
    /// one it always was.
    #[test]
    fn a_ceiling_equal_to_the_boot_size_changes_nothing() {
        let a = libvirt_domain_xml(&cfg(2, "2048M", Some(2), Some(2048)), "/o", "m");
        let b = libvirt_domain_xml(&cfg(2, "2048M", None, None), "/o", "m");
        assert_eq!(a, b);
    }

    #[test]
    fn the_live_add_argv_uses_named_options_and_live_only() {
        assert_eq!(
            setvcpus_argv("qemu:///system", "hp", 4).join(" "),
            "-c qemu:///system setvcpus --domain hp --count 4 --live"
        );
        assert_eq!(
            vcpucount_argv("qemu:///system", "hp").join(" "),
            "-c qemu:///system vcpucount --domain hp --live --active"
        );
        assert_eq!(
            attach_device_argv("qemu:///system", "-hp", "/f.xml").join(" "),
            "-c qemu:///system attach-device --domain -hp --file /f.xml --live"
        );
        let x = dimm_xml(1048576);
        assert!(
            x.contains("<memory model='dimm'>") && x.contains("<size unit='KiB'>1048576</size>")
        );
        assert!(x.contains("<node>0</node>"));
    }

    /// Phases 4–5: what a live disk/NIC add reads, and how it picks a target
    /// the live domain does not use (pinned, so the next boot agrees).
    #[test]
    fn a_hot_disk_takes_a_free_target_and_the_reads_parse_virsh() {
        let blk = " Target   Source\n------------------------------------\n vda      /o.qcow2\n vdb      /seed.iso\n";
        let live = parse_domblklist(blk);
        assert_eq!(live, ["vda", "vdb"]);
        assert_eq!(next_free_target("virtio", &live).as_deref(), Some("vdc"));
        assert_eq!(next_free_target("sata", &live).as_deref(), Some("sdb"));
        let ifl = " Interface   Type      Source    Model    MAC\n-------------------------------------------------------\n vnet0       network   default   virtio   52:54:00:12:34:56\n";
        assert_eq!(parse_domiflist_macs(ifl), ["52:54:00:12:34:56"]);
    }

    /// The boot XML and the live attach use the SAME text — a hot-added disk
    /// boots next time as the guest saw it.
    #[test]
    fn the_attach_xml_is_the_boot_xml() {
        let d = ExtraDisk {
            source: "/d.qcow2".into(),
            target: Some("vdc".into()),
            ..Default::default()
        };
        let mut c = cfg(1, "512M", None, None);
        c.extra_disks = vec![d.clone()];
        let boot = libvirt_domain_xml(&c, "/o", "m");
        assert!(boot.contains(&libvirt_extra_disk_xml(&d, "vdc")), "{boot}");
        let n = ExtraNic {
            kind: "bridge".into(),
            source: Some("br0".into()),
            mac: Some("52:54:00:00:00:02".into()),
            ..Default::default()
        };
        c.extra_nics = vec![n.clone()];
        let boot = libvirt_domain_xml(&c, "/o", "m");
        assert!(boot.contains(&libvirt_extra_nic_xml(&n)), "{boot}");
    }

    /// A disk added to a STOPPED VM is pinned to a letter the old layout
    /// leaves free, so the seed (vdb with no extras) does not move to vdc at
    /// the next boot — measured by the review: it did.
    #[test]
    fn a_cold_added_disk_does_not_move_the_seed() {
        let old: Vec<ExtraDisk> = vec![];
        assert_eq!(seed_target(&extra_disk_targets(&old)), "vdb");
        let mut new = vec![ExtraDisk {
            source: "/d.qcow2".into(),
            ..Default::default()
        }];
        pin_added_disks(&old, &mut new);
        assert_eq!(new[0].target.as_deref(), Some("vdc"));
        assert_eq!(
            seed_target(&extra_disk_targets(&new)),
            "vdb",
            "the seed stays"
        );
        // A pinned extra never shares a letter with the seed.
        let pinned = vec![ExtraDisk {
            source: "/p".into(),
            target: Some("vdb".into()),
            ..Default::default()
        }];
        assert_eq!(seed_target(&extra_disk_targets(&pinned)), "vdc");
        // Unpinned extras keep the letters they always had.
        let two = vec![
            ExtraDisk {
                source: "/a".into(),
                ..Default::default()
            },
            ExtraDisk {
                source: "/b".into(),
                ..Default::default()
            },
        ];
        assert_eq!(extra_disk_targets(&two), ["vdb", "vdc"]);
        assert_eq!(seed_target(&extra_disk_targets(&two)), "vdd");
    }

    /// The reason libvirt gives on its SECOND line reaches the error.
    #[test]
    fn the_whole_virsh_reason_is_kept() {
        let e = virsh_reason(
            "error: Failed to attach device from /x.xml\nerror: internal error: No more available PCI slots\n",
        );
        assert!(e.contains("No more available PCI slots"), "{e}");
        assert!(e.starts_with("Failed to attach device"), "{e}");
    }

    #[test]
    fn a_hot_nic_gets_a_mac_the_vm_does_not_carry() {
        let mut vm = Vm::new(
            "hp".into(),
            String::new(),
            String::new(),
            1,
            "512M".into(),
            "default".into(),
            String::new(),
            mac_for("hp"),
            String::new(),
        );
        let first = extra_nic_mac(&vm);
        assert_ne!(first, vm.mac);
        vm.boot.extra_nics.push(ExtraNic {
            mac: Some(first.clone()),
            ..Default::default()
        });
        assert_ne!(extra_nic_mac(&vm), first);
    }

    #[test]
    fn a_disk_or_nic_is_complete_only_when_the_guest_sees_it() {
        assert_eq!(disk_outcome(Some(2), Some(3)), HotplugOutcome::Complete);
        assert!(matches!(
            disk_outcome(Some(2), Some(2)),
            HotplugOutcome::Partial { .. }
        ));
        assert!(matches!(
            disk_outcome(None, None),
            HotplugOutcome::Partial { .. }
        ));
        let ifs = serde_json::json!([{"name": "eth1", "hardware-address": "52:54:00:AA:00:01"}]);
        assert_eq!(
            nic_outcome("52:54:00:aa:00:01", Some(&ifs)),
            HotplugOutcome::Complete
        );
        assert!(matches!(
            nic_outcome("52:54:00:aa:00:02", Some(&ifs)),
            HotplugOutcome::Partial { .. }
        ));
        assert!(matches!(
            nic_outcome("x", None),
            HotplugOutcome::Partial { .. }
        ));
    }

    #[test]
    fn the_read_backs_parse_what_virsh_prints() {
        assert_eq!(parse_vcpucount("4\n"), Some(4));
        let dominfo = "Id:             3\nName:           hp\nMax memory:     3145728 KiB\nUsed memory:    2097152 KiB\n";
        assert_eq!(parse_dominfo_memory_kib(dominfo), Some(3145728));
        assert_eq!(parse_dominfo_memory_kib("nothing"), None);
    }

    /// D4/D8: success only when the GUEST is using it. No agent, or vCPUs
    /// and memory the image leaves offline, is `Partial`, and says why.
    #[test]
    fn the_guest_agent_decides_complete_or_partial() {
        let vcpus = serde_json::json!([
            {"logical-id": 0, "online": true}, {"logical-id": 1, "online": true},
            {"logical-id": 2, "online": false}, {"logical-id": 3, "online": false}
        ]);
        assert_eq!(guest_online_vcpus(&vcpus), 2);
        assert_eq!(cpu_outcome(2, Some(2)), HotplugOutcome::Complete);
        match cpu_outcome(4, Some(2)) {
            HotplugOutcome::Partial { reason } => assert!(reason.contains("D8"), "{reason}"),
            o => panic!("{o:?}"),
        }
        assert!(matches!(
            cpu_outcome(4, None),
            HotplugOutcome::Partial { .. }
        ));

        let info = serde_json::json!({"size": 134217728u64});
        let blocks = |online: usize, offline: usize| {
            let mut v: Vec<serde_json::Value> = (0..online)
                .map(|_| serde_json::json!({"online": true}))
                .collect();
            v.extend((0..offline).map(|_| serde_json::json!({"online": false})));
            serde_json::Value::Array(v)
        };
        let st = guest_memory_state(&blocks(24, 0), &info).unwrap();
        assert_eq!(st, (3072, 0));
        assert_eq!(memory_outcome(3072, Some(st)), HotplugOutcome::Complete);
        let st = guest_memory_state(&blocks(16, 8), &info).unwrap();
        match memory_outcome(3072, Some(st)) {
            HotplugOutcome::Partial { reason } => {
                assert!(
                    reason.contains("8 block(s) offline") && reason.contains("D8"),
                    "{reason}"
                )
            }
            o => panic!("{o:?}"),
        }
        assert!(matches!(
            memory_outcome(3072, None),
            HotplugOutcome::Partial { .. }
        ));
    }
}
