//! The Cloud Hypervisor VM backend (ADR-0044 P4b.4c): a microVM per process,
//! driven over its api-socket, behind the `VmBackend` port of `delonix-compute`.
//! It moved out of `delonix-vm` so the provider depends only on the foundation
//! and the contexts; the composition root registers it with [`registration`].
//! Its VMs sit on the holder's SDN through the node's `VmNetwork`, reached
//! through `delonix_compute::vm_registry::network()`.
//!
//! The process helpers (`stable_cmd`, `capture`, `binary_in_path`) are this
//! crate's own copy, as in `delonix-provider-libvirt`: a provider may not depend
//! on another provider or an adapter, and a context runs no programs.

#![allow(clippy::too_many_arguments)]

use delonix_compute::capability::{
    Capability as C, CapabilityState as S, HealthStatus, ProviderHealth, ProviderKind,
    ProviderReport,
};
use delonix_compute::vm::{console_socket, serial_log_path, vm_namespace_of};
use delonix_compute::vm_backend::{
    mem_mib, BackendRegistration, Boot, CreateStage, VmBackend, VmConfig,
};
use delonix_compute::vm_error::{Error, Result};
use delonix_compute::vm_registry::mac_for;
use delonix_compute::Vm;
use delonix_node::{proc_starttime, safe_to_signal};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// How long `stop` waits for the VMM to leave after the `SIGTERM`, before
/// escalating to `SIGKILL` — the same ten seconds `container stop` grants by
/// default, so both halves of the engine mean the same thing by "stop".
const VMM_TERM_GRACE: Duration = Duration::from_secs(10);
/// And after the `SIGKILL`. Only the kernel is left to do here, so it is
/// short; it is not zero because the exit still has to be observed.
const VMM_KILL_GRACE: Duration = Duration::from_secs(2);

/// The registration the composition root seeds the registry with: auto-
/// selectable (it answers `available()` from this host alone), `ch` and
/// `cloudhypervisor` as aliases.
pub fn registration() -> BackendRegistration {
    BackendRegistration {
        id: "cloud-hypervisor",
        aliases: &["ch", "cloudhypervisor"],
        auto_selectable: true,
        new: Box::new(|| Ok(Box::new(CloudHypervisorBackend))),
        report: Box::new(|| cloud_hypervisor_report(&CloudHypervisorHost::probe())),
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

/// Shell quoting (single-quote, escaping `'`).
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

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

/// The pid of this VM's VMM, **only when it is safe to signal it**: alive AND
/// still the same process we started.
///
/// Split out of `stop` so the recycled-pid case is covered by a test that fails
/// if the guard is dropped — a test on `safe_to_signal` alone would keep passing.
fn vmm_to_signal(vm: &Vm) -> Option<i32> {
    vm.pid.filter(|&p| safe_to_signal(p, vm.pid_starttime))
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

/// What the Cloud Hypervisor backend needs from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudHypervisorHost {
    pub binary: bool,
    pub kvm: bool,
    /// One of the firmwares the engine knows how to boot with is installed.
    pub firmware: bool,
}

impl CloudHypervisorHost {
    pub const ASSUMED: CloudHypervisorHost = CloudHypervisorHost {
        binary: true,
        kvm: true,
        firmware: true,
    };

    pub fn probe() -> CloudHypervisorHost {
        CloudHypervisorHost {
            binary: binary_in_path("cloud-hypervisor"),
            kvm: std::path::Path::new("/dev/kvm").exists(),
            firmware: DEFAULT_CH_FIRMWARES
                .iter()
                .any(|f| std::path::Path::new(f).exists()),
        }
    }
}

/// The Cloud Hypervisor backend's report against `host`.
pub fn cloud_hypervisor_report(host: &CloudHypervisorHost) -> ProviderReport {
    let available = host.binary;
    let (reason, message) = if !host.binary {
        (
            "BinaryMissing",
            "cloud-hypervisor is not installed".to_string(),
        )
    } else if !host.kvm {
        ("NoKvm", "/dev/kvm is absent".to_string())
    } else if !host.firmware {
        (
            "FirmwareMissing",
            format!("none of {} is installed", DEFAULT_CH_FIRMWARES.join(", ")),
        )
    } else {
        ("Ok", String::new())
    };
    let boots = available && host.kvm && host.firmware;
    let bin = |s: S| {
        s.on_host(
            boots,
            "cloud-hypervisor + /dev/kvm + a known firmware are all needed to boot",
        )
    };
    ProviderReport::build(
        "cloud-hypervisor",
        ProviderKind::Compute,
        available,
        health(available, reason, message),
        |c| {
            match c {
            C::ProviderAvailability => S::Partial {
                detail: "`available()` checks the binary; `provider ls` additionally probes /dev/kvm and the firmware",
            },
            C::ResourceReadback => bin(S::Supported {
                evidence: "check:CH: ls nomeia-o",
            }),
            C::Events => S::NotImplemented,
            C::AsyncOperations => S::NotImplemented,
            C::VmCreate => bin(S::Supported {
                evidence: "e2e:vm: os mesmos snapshots no backend cloud-hypervisor",
            }),
            C::VmStart => bin(S::Supported {
                evidence: "check:CH: vm start",
            }),
            C::VmStop => bin(S::Supported {
                evidence: "check:CH: vm stop",
            }),
            C::VmDestroy => bin(S::Partial {
                detail: "terminates the VMM (pid + starttime guard) and removes the overlay; no named battery check",
            }),
            C::VmRestart => bin(S::Partial {
                detail: "stop-then-start; no battery check names it",
            }),
            C::VmPause => bin(S::Partial {
                detail: "`PUT /api/v1/vm.pause` on the VMM socket; no battery check",
            }),
            C::VmResume => bin(S::Partial {
                detail: "`vm.resume` on the VMM socket; no battery check",
            }),
            C::VmResumeSameIdentity => bin(S::Supported {
                evidence: "check:CH: vm start",
            }),
            C::VmClone => S::NotImplemented,
            C::VmTemplate => S::NotImplemented,
            C::VmResizeCold => bin(S::Partial {
                detail: "`vm resize` rewrites vcpus/memory in the record and `vm start` rebuilds the vmm command line from it; no battery check yet",
            }),
            C::VmHotplug => S::NotImplemented,
            C::VmExtraDisks => S::UnsupportedByProvider {
                reason: "the CH command line carries one root disk and the seed; `extraDisks` are refused for this backend",
            },
            C::VmExtraNics => S::UnsupportedByProvider {
                reason: "one tap on the SDN; `extraNics` are refused for this backend",
            },
            C::VmDiskResize => bin(S::Supported {
                evidence: "check:CH: o overlay cresceu para exactamente 1 GiB",
            }),
            C::VmPciPassthrough => bin(S::Partial {
                detail: "`--device path=<sysfs>` per validated address; no IOMMU host in the battery",
            }),
            C::VmTpm => S::UnsupportedByProvider {
                reason: "no vTPM device is emitted for Cloud Hypervisor",
            },
            C::VmCpuModel => S::UnsupportedByProvider {
                reason: "CH exposes the host CPU; no model selection is passed",
            },
            C::VmCpuPinning => bin(S::Partial {
                detail: "`cpuAffinity` becomes `--cpus affinity=`; not booted in the battery",
            }),
            C::VmHugepages => bin(S::Partial {
                detail: "`--memory hugepages=on`; unit-tested argument, pool not probed",
            }),
            C::VmCloudInit => bin(S::Partial {
                detail: "NoCloud seed ISO attached as a second disk; the battery never logs into a guest",
            }),
            C::VmRestartPolicyNative => S::UnsupportedByProvider {
                reason: "the VMM has no crash policy; the engine's supervisor restarts it (`restart_policy_unsupervised`)",
            },
            C::VmNamespaceIsolation => bin(S::Supported {
                evidence: "chaos:scen_namespace_isolation",
            }),
            C::VmAntispoof => bin(S::Partial {
                detail: "the tap is pinned to its DHCP lease and the guest MAC in the bridge anti-spoofing table (`dlxspoof`, 2026-10-02; the old `ip` rule never matched); a booted guest got its lease and answered ARP through it, but no battery check forges from a guest yet",
            }),
            C::VmRawDefinition => S::UnsupportedByProvider {
                reason: "no raw definition format exists for the CH command line",
            },
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
            C::VmNetworkNat => S::UnsupportedByProvider {
                reason: "no hypervisor NAT network: the tap joins the engine's SDN, which does the NAT",
            },
            C::VmNetworkBridge => S::UnsupportedByProvider {
                reason: "the NIC is a tap on the SDN bridge inside the holder, never a host bridge",
            },
            C::VmNetworkSdn => bin(S::Supported {
                evidence: "check:CH: vm start",
            }),
            C::VmStaticIp => S::UnsupportedByProvider {
                reason: "the address is derived from the MAC by the engine's DHCP; a fixed IP is not honoured",
            },
            C::StoragePools => S::NotImplemented,
            C::VmSnapshotDisk => bin(S::Supported {
                evidence: "check:CH: create com a VM parada",
            }),
            C::VmSnapshotMemory => S::UnsupportedByProvider {
                reason: "`vm.snapshot` of the VMM saves memory without the disk, which cannot be restored consistently; refused by design",
            },
            C::VmSnapshotRestore => bin(S::Supported {
                evidence: "check:CH: restore",
            }),
            C::VmSnapshotDelete => bin(S::Supported {
                evidence: "check:CH: rm com a VM parada",
            }),
            C::VmSnapshotPersistent => bin(S::Supported {
                evidence: "check:CH: ls nomeia-o",
            }),
            C::VmBackupDisk => bin(S::Partial {
                detail: "`backup create vm` copies the overlay of a STOPPED VM; a running CH VM holds the qcow2 exclusively",
            }),
            C::VmBackupQuiesced => S::UnsupportedByProvider {
                reason: "no guest agent channel; only an offline copy is consistent",
            },
            C::VmBackupRestore => bin(S::Partial {
                detail: "`backup restore` re-imports the disk; battery covers the container kind only",
            }),
            C::VmMigrationCold => bin(S::Partial {
                detail: "`vm migrate`: stop, copy over SSH, import, start; no battery",
            }),
            C::VmMigrationLive => S::UnsupportedByProvider {
                reason: "ADR-0031: CH migrates memory only, never the disk",
            },
            C::VmReplication => S::RequiresExternalComponent {
                component: "shared or replicated VM storage outside the engine (ADR-0031)",
            },
            C::VmHighAvailability => S::RequiresExternalComponent {
                component: "a cluster manager with quorum and fencing",
            },
            C::VmConsoleSerial => bin(S::Partial {
                detail: "`vm console` bridges the tty to the VMM's serial socket; no battery",
            }),
            C::VmConsoleVnc => S::UnsupportedByProvider {
                reason: "no VNC device on the CH command line",
            },
            C::VmGuestAgent => S::NotImplemented,
            C::VmIpObserved => S::UnsupportedByProvider {
                reason: "the address is PREDICTED from the MAC (`ip_is_predicted`); `--wait` verifies it by ARP instead",
            },
            C::MetricsPrometheus => S::Partial {
                detail: "`delonix_vms_running/total` only",
            },
            C::MetricsPerWorkloadNetwork => S::Partial {
                detail: "the tap's counters are readable in the holder; not exported per VM",
            },
            C::HostHealth => S::Supported {
                evidence: "check:system info",
            },
            C::HostCapacity => S::NotImplemented,
            C::TransportVerified => S::Partial {
                detail: "a unix socket per VMM, owned by the engine's uid",
            },
            C::CredentialInVault => S::UnsupportedByProvider {
                reason: "no credential exists: the VMM is a child of the engine",
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
            // Not `let _`: a network that could not be created (or recorded) used to
            // surface one step later as a bare "no such network" from the attach.
            delonix_compute::vm_registry::network()?.ensure_network(&cfg.network)?;
        }
        // The MAC is needed BEFORE the attach now, not after: it is what makes
        // the guest's future DHCP address computable, and that address is what
        // the attach registers in the namespace sets.
        let mac = mac_for(&cfg.name);
        let ns = vm_namespace_of(cfg);
        let net = delonix_compute::vm_registry::network()?;
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
        delonix_compute::vm_registry::network()
            .ok()?
            .lease_ip(&vm.network, &vm.mac)
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

    /// `vm resize --disk-size` (`vm.disk.resize`): grows the overlay
    /// (`ch_overlay`, the same path `boot`/`snapshots` already read). The
    /// vmm holds a RUNNING VM's qcow2 exclusively — the same fact that
    /// makes live snapshotting impossible on this backend
    /// (`offline_snapshot_op`) — so a grow has to be refused there too, not
    /// just when the record says "stopped".
    fn resize_disk(&self, vmdir: &Path, vm: &Vm, new_bytes: u64) -> delonix_model::Result<()> {
        self.offline_snapshot_op(vmdir, vm, "resize the disk of")?;
        let overlay = ch_overlay(vmdir, vm);
        let info = capture(
            "qemu-img",
            &["info", "-U", "--", &overlay.to_string_lossy()],
        )
        .ok_or_else(|| {
            delonix_model::Error::from(Error::Command {
                context: "qemu-img info",
                message: format!("could not read the disk of VM '{}'", vm.name),
            })
        })?;
        let current = parse_virtual_size_bytes(&info).ok_or_else(|| {
            delonix_model::Error::from(Error::Command {
                context: "qemu-img info",
                message: format!("no 'virtual size' in: {info}"),
            })
        })?;
        if new_bytes <= current {
            return Err(delonix_model::Error::from(Error::InvalidResize(format!(
                "VM '{}' disk is already {current} bytes — a disk can grow, never shrink (asked \
                 for {new_bytes} bytes)",
                vm.name
            ))));
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
        let net = delonix_compute::vm_registry::network()?;
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

/// Pure: the current size of a qcow2 disk from `qemu-img info`'s (plain
/// text, not `--output=json`) `"virtual size: ... (N bytes)"` line, in
/// bytes — the ONLY place this backend reads a disk's size from (never the
/// record, which carries none).
fn parse_virtual_size_bytes(info: &str) -> Option<u64> {
    let line = info
        .lines()
        .find(|l| l.trim_start().starts_with("virtual size:"))?;
    let after_paren = line.split('(').nth(1)?;
    after_paren
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
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
pub const DEFAULT_CH_FIRMWARES: [&str; 4] = [
    "/usr/local/share/delonix/CLOUDHV.fd",
    "/usr/share/delonix/CLOUDHV.fd",
    "/usr/local/share/delonix/hypervisor-fw",
    "/usr/share/delonix/hypervisor-fw",
];

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
    let join = delonix_compute::vm_registry::network()?
        .join_argv()
        .ok_or_else(|| Error::Command {
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
        let cmd = stable_cmd("cloud-hypervisor");
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

    #[test]
    fn parse_virtual_size_bytes_reads_the_real_qemu_img_text_shape() {
        // Captured live on this host (`qemu-img info -U`, plain text).
        let real = "image: /vms/t.qcow2\nfile format: qcow2\nvirtual size: 10 GiB (10737418240 bytes)\ndisk size: 196 KiB\n";
        assert_eq!(super::parse_virtual_size_bytes(real), Some(10737418240));
        assert_eq!(
            super::parse_virtual_size_bytes("image: /vms/t.qcow2\n"),
            None
        );
        assert_eq!(super::parse_virtual_size_bytes(""), None);
    }

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
    fn shq_escapes_quotes() {
        assert_eq!(shq("a b"), "'a b'");
        assert_eq!(shq("a'b"), "'a'\\''b'");
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

    fn run_ok(prog: &str, args: &[&str]) -> bool {
        std::process::Command::new(prog)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
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
    /// BUG-VM-001: a clean, freshly-created qcow2 must never read as
    /// corrupted — `disk_looks_corrupt` exists to add a diagnosis on top of a
    /// REAL positive, and a false positive here would fail every `vm stop`.
    #[test]
    fn disk_looks_corrupt_says_no_for_a_clean_image() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("clean.qcow2");
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
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("truncated.qcow2");
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
    }
    /// A live disk backup is a backend's mechanism (P4b.3a): a backend that
    /// cannot copy a disk under a running guest refuses by name, with the code
    /// the orchestration used to answer with itself (DX-1514) — never touching
    /// the destination.
    #[test]
    fn a_backend_without_a_live_backup_refuses_it_by_name() {
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
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("x.qcow2");
        let e = CloudHypervisorBackend
            .backup_disk_live(dir.path(), &vm, &dest, false)
            .unwrap_err();
        assert_eq!(e.number(), 1514, "{e}");
        assert!(e.to_string().contains("cloud-hypervisor"), "{e}");
        assert!(!dest.exists(), "the refusal wrote the destination");
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
}

/// Identidade do PID antes de matar o VMM — a assimetria que faltava fechar.
///
/// O lado dos containers guarda TODOS os sinais com `safe_to_signal` (dezasseis
/// pontos de chamada); o das VMs matava por `pid > 0`, e o registo nem sequer
/// levava o `starttime` contra o qual comparar.
#[cfg(test)]
mod tests_identidade_do_vmm {
    use super::*;
    use delonix_compute::vm::{adopt_pid_starttime, argv_is_vmm_for};

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
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
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
    }

    /// The answer of an api that is up is not enough when the process is not:
    /// the api answered and the VMM left — a zombie included.
    #[test]
    fn an_answering_api_does_not_rescue_a_dead_vmm() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
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
    }

    #[test]
    fn a_vmm_whose_vm_is_running_is_returned() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let (log, pidf, sock) = (d.join("v.log"), d.join("v.pid"), d.join("v.sock"));
        fake_api(&sock, "Running");
        let s = script("sleep 30", &log, &pidf);
        let pid = launch_vmm(&no_join(), &s, &pidf, &sock, &log, Duration::from_secs(5))
            .expect("a live VMM reporting Running has to be accepted");
        assert!(matches!(proc_state(pid), Some(st) if st != 'Z'));
        kill(pid);
    }

    /// Alive but never `Running` (a silent or stuck VMM): an error once the
    /// grace is up, and the process does not stay behind holding the disk.
    #[test]
    fn a_vmm_that_never_reports_running_is_terminated() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
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
