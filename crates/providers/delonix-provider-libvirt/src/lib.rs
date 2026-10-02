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
    mem_mib, BackendRegistration, Boot, CreateStage, VmBackend, VmConfig,
};
use delonix_compute::vm_error::{Error, Result};
use delonix_compute::vm_registry::mac_for;
use delonix_compute::Vm;
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
            C::VmRestart => bin(S::Partial {
                detail: "stop-then-start, always a real reboot; no battery check names it",
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
            C::VmClone => S::NotImplemented,
            C::VmTemplate => S::NotImplemented,
            C::VmResizeCold => bin(S::Supported {
                evidence: "check:o domínio arranca com 2 vCPU",
            }),
            C::VmHotplug => S::NotImplemented,
            C::VmExtraDisks => bin(S::Partial {
                detail: "`extraDisks` reach the domain XML (unit-tested target letters); never booted in the battery",
            }),
            C::VmExtraNics => bin(S::Partial {
                detail: "`extraNics` (network/bridge/user) reach the domain XML; never booted in the battery",
            }),
            C::VmDiskResize => S::NotImplemented,
            C::VmPciPassthrough => bin(S::Partial {
                detail: "`<hostdev>` per validated PCI address; no IOMMU host in the battery",
            }),
            C::VmTpm => bin(S::Partial {
                detail: "`<tpm model='tpm-crb'>` emulator; swtpm presence is not probed",
            }),
            C::VmCpuModel => bin(S::Partial {
                detail: "`cpuModel`/`cpuTopology` in the XML, default host-passthrough; not booted in the battery",
            }),
            C::VmCpuPinning => bin(S::Partial {
                detail: "`<cputune>` quota + `<vcpupin>`; unit-tested XML, dropped on qemu:///session",
            }),
            C::VmHugepages => bin(S::Partial {
                detail: "`<memoryBacking><hugepages/>`; the host's hugepage pool is not probed",
            }),
            C::VmCloudInit => bin(S::Partial {
                detail: "NoCloud seed on a virtio disk (#435); the battery never logs into a guest",
            }),
            C::VmRestartPolicyNative => bin(S::Partial {
                detail: "`on_crash=restart` for always/on-failure; unit-tested, no guest crashed on purpose",
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
                evidence: "check:snapshot create com a VM parada",
            }),
            C::VmSnapshotMemory => bin(S::Supported {
                evidence: "check:vm snapshot create",
            }),
            C::VmSnapshotRestore => bin(S::Supported {
                evidence: "check:vm snapshot restore depois do start",
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
            C::VmMigrationCold => bin(S::Partial {
                detail: "`vm migrate`: stop, flatten, copy over SSH, import, start; no battery",
            }),
            C::VmMigrationLive => S::UnsupportedByProvider {
                reason: "ADR-0031: needs shared VM storage or a privileged libvirt daemon listening on the network; neither is a default of this engine",
            },
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
            C::VmGuestAgent => S::NotImplemented,
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
            | C::FirewallWorkloadPeer => S::UnsupportedByProvider {
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
        libvirt_filterref_xml(cfg.net_mode.as_deref(), cfg.allow_mac_spoofing)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // A declared "no" stays a "no" whatever the host looks like.
        assert_eq!(by(C::VmMigrationLive).label(), "unsupported-by-provider");
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
}
