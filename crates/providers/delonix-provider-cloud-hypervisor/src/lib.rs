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
use delonix_compute::cgroup_leaf::{leaf_dir_name, LeafLimits, ProcessLeaf};
use delonix_compute::vm::{console_socket, serial_log_path, vm_namespace_of};
use delonix_compute::vm_backend::{
    mem_mib, names_a_declared_network, BackendRegistration, Boot, CreateStage, HotplugOutcome,
    VmBackend, VmConfig,
};
use delonix_compute::vm_error::{Error, Result};
use delonix_compute::vm_registry::mac_for;
use delonix_compute::{ExtraDisk, ExtraNic, Vm};
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
            C::VmDestroy => bin(S::Supported {
                evidence: "check:CH: vm destroy",
            }),
            C::VmRestart => bin(S::Supported {
                evidence: "check:CH: vm restart",
            }),
            C::VmPause => bin(S::Supported {
                evidence: "check:CH: vm pause",
            }),
            C::VmResume => bin(S::Supported {
                evidence: "check:CH: vm resume",
            }),
            C::VmResumeSameIdentity => bin(S::Supported {
                evidence: "check:CH: vm start",
            }),
            C::VmClone => S::NotImplemented,
            C::VmTemplate => S::NotImplemented,
            C::VmResizeCold => bin(S::Supported {
                evidence: "check:CH: vm resize --vcpus 2 com a VM parada",
            }),
            // CPU add is proven live end to end (the guest's `nproc` rises
            // after `vm update --vcpus`): the hypervisor `vm.resize` succeeds
            // and the guest onlines the hot-added vCPU. The RUNTIME still
            // reports the operation `partial` (CH has no in-band channel to
            // confirm the guest side — ADR-0068 D4/Q5), exactly as the proven
            // disk/NIC adds do; the capability is `supported` because the
            // feature is exercised and the evidence names the proof.
            C::VmHotplugCpuAdd => bin(S::Supported {
                evidence: "live:scripts/vm-native-ch-cpumem.sh",
            }),
            // Hot CPU REMOVAL from a running guest is not wired and not safe to
            // wire here: a vCPU the guest has not offlined first cannot be torn
            // out without risking the tasks on it, and CH gives no channel to
            // coordinate "guest offlined → now remove" (the same D4/Q5 gap). The
            // engine routes every CPU decrease through cold `vm resize` on a
            // stopped VM (`VmResizeCold`, supported); libvirt and Proxmox leave
            // this `not-implemented` for the same reason.
            C::VmHotplugCpuRemove => S::NotImplemented,
            // Memory add is proven live end to end (the guest's `MemTotal`
            // rises after `vm update --memory`): the virtio-mem region declared
            // at create (ADR-0068 D3) is grown by `vm.resize` and the guest's
            // driver plugs and onlines the blocks, so `memory_actual_size`
            // reaches the target and the runtime reports `Complete`.
            C::VmHotplugMemoryAdd => bin(S::Supported {
                evidence: "live:scripts/vm-native-ch-cpumem.sh",
            }),
            // Hot memory REMOVAL (virtio-mem shrink) is cooperative and unsafe
            // to force: the guest driver only releases blocks it can spare, and
            // pulling memory in use would OOM the guest. Not wired; every memory
            // decrease goes through cold `vm resize` on a stopped VM
            // (`VmResizeCold`, supported), as on libvirt and Proxmox.
            C::VmHotplugMemoryRemove => S::NotImplemented,
            C::VmHotplugDiskAdd => bin(S::Supported {
                evidence: "live:scripts/vm-native-ch-hotplug.sh",
            }),
            C::VmHotplugDiskRemove => S::NotImplemented,
            C::VmHotplugNicAdd => bin(S::Supported {
                evidence: "live:scripts/vm-native-ch-hotplug.sh",
            }),
            C::VmHotplugNicRemove => S::NotImplemented,
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
                detail: "each `cfg.devices` path becomes `--device path=<sysfs>` (VFIO); not proved live — binding a device to vfio-pci needs root and a host device spare to detach, and none is on the test host",
            }),
            // Cloud Hypervisor DOES carry a vTPM: `--tpm socket=<swtpm socket>`
            // (confirmed on v52 `--help`), backed by an external `swtpm` the VMM
            // connects to over a UNIX socket. It is NOT unsupported-by-provider —
            // the backend can express it — it is simply not wired: `boot_ch`
            // emits no `--tpm` and nothing spawns or reaps a per-VM `swtpm`
            // (socket, state dir, pidfile, teardown on stop/destroy), so the arm
            // stays honest at `not-implemented` until that lifecycle exists.
            C::VmTpm => S::NotImplemented,
            C::VmCpuModel => S::UnsupportedByProvider {
                reason: "CH exposes the host CPU; no model selection is passed",
            },
            C::VmCpuPinning => bin(S::Supported {
                evidence: "live:scripts/vm-native-ch-vm-partials.sh",
            }),
            C::VmHugepages => bin(S::Supported {
                evidence: "live:scripts/vm-native-ch-hugepages.sh",
            }),
            C::VmCloudInit => bin(S::Supported {
                evidence: "live:scripts/vm-native-ch-lifecycle.sh",
            }),
            C::VmRestartPolicyNative => S::UnsupportedByProvider {
                reason: "the VMM has no crash policy; the engine's supervisor restarts it (`restart_policy_unsupervised`)",
            },
            C::VmNamespaceIsolation => bin(S::Supported {
                evidence: "chaos:scen_namespace_isolation",
            }),
            C::VmAntispoof => bin(S::Supported {
                evidence: "live:scripts/vm-native-ch-vm-partials.sh",
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
                reason: "the VMM is indifferent to the bridge, but this rootless provider puts its tap on the engine's SDN in the holder netns; a host bridge needs the host netns (root), which is libvirt's `netMode: bridge`",
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
            C::VmBackupDisk => bin(S::Supported {
                // #16: proved by DATA recovery, not an exit code. `backup create
                // vm` archives the overlay of a STOPPED CH VM (a running one holds
                // the qcow2 exclusively, so `backup_disk_live` refuses by name,
                // DX-1514); the live battery writes a known marker into the guest,
                // stops, archives, and then reads that marker back OUT of the
                // archived overlay OFFLINE with guestfish — so the archive is shown
                // to carry the guest's own data, not merely a file that exists.
                evidence: "live:scripts/vm-native-ch-backup.sh",
            }),
            C::VmBackupQuiesced => S::UnsupportedByProvider {
                reason: "no guest agent channel; only an offline copy is consistent",
            },
            C::VmBackupRestore => bin(S::Supported {
                // #16: the restore is proved by DATA recovery on a live CH guest.
                // The battery runs the full cycle ORIGINAL -> backup -> CHANGED ->
                // restore -> ORIGINAL: it writes a marker, backs up, boots the VM
                // and OVERWRITES the marker, then `backup restore`s and boots again
                // — and the guest reads back the ORIGINAL value, so the change was
                // actually undone. See scripts/vm-native-ch-backup.sh.
                evidence: "live:scripts/vm-native-ch-backup.sh",
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
            C::VmConsoleSerial => bin(S::Supported {
                evidence: "live:scripts/vm-native-ch-vm-partials.sh",
            }),
            C::VmConsoleVnc => S::UnsupportedByProvider {
                reason: "no VNC device on the CH command line",
            },
            C::VmGuestAgent => S::UnsupportedByProvider {
                // The qemu-guest-agent the catalog models speaks over a
                // virtio-serial channel named `org.qemu.guest_agent.0` (the
                // exact device libvirt puts on every domain and reads with
                // `virsh qemu-agent-command`). Cloud Hypervisor exposes no
                // virtio-serial device at all: its command line carries only a
                // single `--console` (one virtio-console), a legacy `--serial`,
                // and `--vsock` — there is no port to run the agent on and no
                // host transport to query it, so the engine cannot build the
                // channel even in principle. A vsock-based agent
                // (`qemu-ga -m vsock-listen` over CH `--vsock`) is a different,
                // non-adopted mechanism: it is unwired here and no guest image
                // ships or configures it, so it is not a claim this provider can
                // make. Verified against cloud-hypervisor v52.0 `--help`.
                reason: "Cloud Hypervisor exposes no virtio-serial channel (org.qemu.guest_agent.0) for the qemu-guest-agent — only a single virtio-console, legacy serial and vsock; a vsock-based agent is a different, unadopted mechanism",
            },
            C::VmIpObserved => bin(S::Partial {
                detail: "stays PARTIAL by provenance, not for lack of a proof: the reported address is COMPUTED from the MAC (`dhcp_lease_ip`, deterministic; `ip_is_predicted` -> `Predicted`), never read back from a lease file or a guest agent, which is what `vm.ip.observed` asks for. `vm create --wait` (`wait_for_boot` -> `delonix_sdn::infra::sdn_reachable`) ARP-probes the SDN and reads `ip neigh` REACHABLE for the (computed IP, guest MAC), and `scripts/vm-native-ch-vm-partials.sh` confirms live that the reported address equals the guest's OWN address — but confirming a computed address is reachable is not observing it, so it is not promoted",
            }),
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
// Backend: Cloud Hypervisor
// ===========================================================================

/// Builds the Cloud Hypervisor `--memory` argument (with `hugepages=on` if
/// requested, and `hotplug_method=virtio-mem,hotplug_size=…` when
/// `cfg.memory_max_mib` declares headroom above the boot size — ADR-0068 D3;
/// `acpi` was measured and rejected, Alternatives, "it does not remove, and
/// answered 204 to a removal"). Pure function — tested without hardware.
fn memory_arg(cfg: &VmConfig) -> String {
    let boot_mib = mem_mib(&cfg.memory);
    let mut a = format!("size={boot_mib}M");
    if cfg.hugepages {
        a.push_str(",hugepages=on");
    }
    if let Some(max_mib) = cfg.memory_max_mib {
        if max_mib > boot_mib {
            a.push_str(&format!(
                ",hotplug_method=virtio-mem,hotplug_size={}M",
                max_mib - boot_mib
            ));
        }
    }
    a
}

/// Builds the Cloud Hypervisor `--cpus` argument. `max=` is the hotplug
/// ceiling declared by `cfg.vcpus_max` (ADR-0068 D3) — without it Cloud
/// Hypervisor refuses any `vm.resize` above the boot count, measured
/// (Annex A) as `Requested vCPUs exceed maximum`. With `cpu_affinity`, pins
/// each boot vCPU to the same list of host CPUs (`affinity=0@[list],1@[list],…`
/// — unaffected by `max`, which only reserves slots for vCPUs that do not
/// exist yet). Pure function — tested without hardware.
fn cpus_arg(cfg: &VmConfig) -> String {
    let n = cfg.vcpus.max(1);
    let mut a = format!("boot={n}");
    if let Some(max) = cfg.vcpus_max {
        if max > n {
            a.push_str(&format!(",max={max}"));
        }
    }
    if let Some(list) = &cfg.cpu_affinity {
        // Cloud Hypervisor wants the WHOLE affinity value as one bracketed list,
        // its per-vCPU elements separated by commas — `affinity=[0@[list],1@[list],…]`
        // (measured against cloud-hypervisor v52: the older `0@[list]:1@[list]`
        // colon form is rejected as `ParseCpus(Conversion("affinity", …))`, so a
        // pinned VM never booted). The outer brackets also shield the inner commas
        // from the `--cpus` key=value splitter.
        let aff: Vec<String> = (0..n).map(|v| format!("{v}@[{list}]")).collect();
        a.push_str(&format!(",affinity=[{}]", aff.join(",")));
    }
    a
}

/// Memory the VMM process needs on top of the guest's RAM, in MiB — the margin
/// its leaf's `memory.max` adds to the VM's memory ceiling.
///
/// Measured with Cloud Hypervisor v53.0 (firmware boot, 2 vCPUs, no disk): the
/// VMM's own resident set was ~7 MiB (2.8 MiB anonymous) with 256 MiB and with
/// 2 GiB of guest RAM alike — the overhead does not grow with the guest. Disks
/// and a qcow2 cache add to it; 128 MiB leaves an order of magnitude of room,
/// because a margin too tight is not a softer limit but an OOM kill of a VM that
/// was inside its own size.
pub const VMM_MEMORY_OVERHEAD_MIB: u64 = 128;

/// The name of a VM's process leaf (the directory is `dlx-vm-<name>`).
fn vmm_leaf_name(vm: &str) -> String {
    format!("vm-{vm}")
}

/// Is `leaf` the one this backend created for VM `vm`? A `stop` removes only its
/// own leaf, never whatever cgroup a recycled pid happens to be in.
fn is_vmm_leaf(leaf: &ProcessLeaf, vm: &str) -> bool {
    leaf.dir_name() == Some(leaf_dir_name(&vmm_leaf_name(vm)).as_str())
}

/// The ceilings of a VM's VMM leaf, from the VM's own shape. PURE.
///
/// * `cpu.max` — the vCPU CEILING (`vcpus_max` when hotplug headroom is declared,
///   else `vcpus`) as whole cores: a VM sized for N vCPUs gets N cores of CPU
///   time, its VMM threads included, the same contract a container's `--cpus`
///   gives.
/// * `memory.max` — the memory ceiling (`memory_max_mib` when declared above the
///   boot size, so a hotplug up to it fits) plus [`VMM_MEMORY_OVERHEAD_MIB`]. With
///   `hugepages` the guest RAM is charged to `hugetlb`, not `memory`, so this is
///   then a looser bound; it is never a tighter one.
/// * `pids.max` — the engine's per-container default, or more for a VM whose
///   vCPU threads alone would come close to it (a task is a THREAD here: one per
///   vCPU, plus the VMM's own — 7 measured for 2 vCPUs).
fn vmm_leaf_limits(cfg: &VmConfig) -> LeafLimits {
    let vcpus = cfg.vcpus_max.unwrap_or(cfg.vcpus).max(cfg.vcpus).max(1);
    let boot_mib = mem_mib(&cfg.memory);
    let guest_mib = cfg.memory_max_mib.unwrap_or(boot_mib).max(boot_mib);
    LeafLimits {
        cpu_quota_usec: Some(LeafLimits::quota_for_cores(vcpus)),
        memory_max_bytes: Some((guest_mib + VMM_MEMORY_OVERHEAD_MIB) * 1024 * 1024),
        pids_max: Some(delonix_compute::DEFAULT_PIDS_MAX.max(u64::from(vcpus) * 2 + 64)),
        io_max: None,
    }
}

/// The VMM's leaf for `cfg`, or the refusal. The decision, apart from the global
/// registry and the environment so it is tested with a fake port.
///
/// A VM always declares its CPU and memory, and those ARE the ceilings: before
/// this, the VMM ran in whatever cgroup launched it, so neither was enforced on
/// the process that holds the vCPUs and the guest's RAM. When no leaf can hold
/// them — no provider registered in this process, a host with no delegated
/// cgroup, a ceiling that did not land — the VM is REFUSED
/// ([`Error::LimitsNotEnforceable`]) instead of starting with no ceiling while
/// its record says it has one. `escape_hatch` (`DELONIX_ALLOW_UNENFORCED_LIMITS`,
/// the same switch the container limits use) starts it anyway, with a warning.
fn vmm_leaf(
    leaves: Option<&dyn delonix_compute::ports::ProcessLeaves>,
    cfg: &VmConfig,
    escape_hatch: bool,
) -> Result<Option<ProcessLeaf>> {
    let why = match leaves {
        None => "no cgroup leaf provider is registered in this process".to_string(),
        Some(p) => match p.create(&vmm_leaf_name(&cfg.name), &vmm_leaf_limits(cfg)) {
            Ok(Some(leaf)) => return Ok(Some(leaf)),
            Ok(None) => "this host gives the engine no cgroup to create it in (rootless without \
                         a delegated cgroup)"
                .to_string(),
            Err(e) => e.to_string(),
        },
    };
    if escape_hatch {
        tracing::warn!(
            vm = %cfg.name,
            reason = %why,
            "the VM's CPU and memory ceilings are NOT enforced on its VMM \
             (DELONIX_ALLOW_UNENFORCED_LIMITS is set)"
        );
        return Ok(None);
    }
    Err(Error::LimitsNotEnforceable(format!(
        "VM '{}': its CPU and memory ceilings ({} vCPU, {}) cannot be enforced on the \
         Cloud Hypervisor process — {why}. Run as root, or rootless under a delegated scope \
         (`systemd-run --user --scope -p Delegate=yes -- delonix vm …`); \
         DELONIX_ALLOW_UNENFORCED_LIMITS=1 starts it without them",
        cfg.name, cfg.vcpus, cfg.memory
    )))
}

/// The body of [`CloudHypervisorBackend::boot`] after the refusals: the network,
/// then the VMM — inside `leaf` when there is one.
fn boot_on_network(
    vmdir: &Path,
    cfg: &VmConfig,
    overlay: &str,
    on: &dyn Fn(CreateStage),
    leaf: Option<&ProcessLeaf>,
) -> delonix_model::Result<Boot> {
    // Own private network when named (≠ shared ingress): ensures its
    // isolated bridge + DHCP before the attach. The VMs' SDN lives here.
    on(CreateStage::Network);
    if names_a_declared_network(&cfg.network) {
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
    let pid = match boot_ch(vmdir, cfg, overlay, &tap, &mac, leaf) {
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
        // The VMM's cgroup leaf, with the VM's ceilings, BEFORE any effect: a VM
        // whose ceilings nothing would hold is refused here, not after its tap
        // exists (see `vmm_leaf`).
        let leaf = vmm_leaf(
            delonix_compute::vm_registry::process_leaves(),
            cfg,
            std::env::var_os("DELONIX_ALLOW_UNENFORCED_LIMITS").is_some(),
        )?;
        let booted = boot_on_network(vmdir, cfg, overlay, on, leaf.as_ref());
        if booted.is_err() {
            if let Some(l) = &leaf {
                l.remove();
            }
        }
        booted
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
            // Read BEFORE the VMM goes: `/proc/<pid>/cgroup` leaves with it, and a
            // stop run by another process cannot rebuild the path (the rootless
            // base depends on the cgroup of whoever created it).
            let leaf = ProcessLeaf::of_pid(pid).filter(|l| is_vmm_leaf(l, &vm.name));
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
            if let Some(l) = leaf {
                l.remove();
            }
        }
        // The record's own address if it learned one; otherwise the lease its MAC
        // maps to — so a VM stopped before it ever DHCP'd still gives up its chain.
        let net = delonix_compute::vm_registry::network()?;
        let ip = vm.ip.clone().or_else(|| net.lease_ip(&vm.network, &vm.mac));
        net.detach_tap(&vm.name, ip.as_deref());
        // Each hot-added NIC laid its own tap under a per-NIC owner
        // (`hotplug_add_nic`); tear exactly those down too, recomputed from the
        // recorded MAC — a tap nobody detaches would outlive the VM.
        for nic in &vm.boot.extra_nics {
            if let Some(mac) = &nic.mac {
                let network = nic.source.as_deref().unwrap_or(&vm.network);
                let owner = ch_extra_nic_owner(&vm.name, mac);
                let lease = net.lease_ip(network, mac);
                net.detach_tap(&owner, lease.as_deref());
            }
        }
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

    // ---- hotplug add (ADR-0068, Phase 1) ----------------------------------
    //
    // Both go through `vm.resize`, over the same api-socket `pause`/`resume`
    // already use. Removal is a separate capability (D10) and is not here.

    /// `vm.resize {"desired_vcpus":N}`. Always `Partial` (D4/Q5): Cloud
    /// Hypervisor has no channel today to confirm the GUEST onlined a
    /// hot-added vCPU, only that the VMM spawned its thread — so `wait` is
    /// accepted (trait contract) and intentionally unused rather than spent
    /// polling for a signal that does not exist.
    fn hotplug_add_cpu(
        &self,
        _vmdir: &Path,
        vm: &Vm,
        target_vcpus: u32,
        _wait: Duration,
    ) -> delonix_model::Result<(u32, HotplugOutcome)> {
        let ceiling = vm.vcpus_max.unwrap_or(vm.vcpus);
        if target_vcpus > ceiling {
            return Err(delonix_model::Error::from(Error::InvalidResize(format!(
                "VM '{}' has no headroom for {target_vcpus} vCPUs — the declared maximum is \
                 {ceiling} (see `vm resize --vcpus-max`, ADR-0068 D3)",
                vm.name
            ))));
        }
        let pid = vm.pid.ok_or_else(|| {
            delonix_model::Error::from(Error::Command {
                context: "vm",
                message: format!("VM '{}' has no recorded VMM pid to hotplug", vm.name),
            })
        })?;
        let (status, _) = ch_api_call_with_body(
            &vm.api_socket,
            "PUT",
            "/api/v1/vm.resize",
            Some(&format!("{{\"desired_vcpus\":{target_vcpus}}}")),
            Duration::from_secs(10),
        )?;
        if !http_status_is_2xx(&status) {
            return Err(delonix_model::Error::from(Error::CloudHypervisorApi(
                format!(
                    "cloud-hypervisor vm.resize (vcpus) for VM '{}': {}",
                    vm.name,
                    if status.is_empty() {
                        "no response"
                    } else {
                        &status
                    }
                ),
            )));
        }
        // ADR-0068 D5: the truth is what is read back from the live VM, never
        // the request's own answer — `vcpuN` thread count, not `boot_vcpus`.
        let assigned = ch_live_vcpu_count(pid).ok_or_else(|| {
            delonix_model::Error::from(Error::CloudHypervisorApi(format!(
                "cloud-hypervisor accepted vm.resize for VM '{}' but its VMM threads (pid {pid}) \
                 could not be read back — the live vCPU count is unknown",
                vm.name
            )))
        })?;
        Ok((
            assigned,
            HotplugOutcome::Partial {
                reason: "cloud-hypervisor has no channel to confirm the guest onlined the \
                         hot-added vCPU (ADR-0068 D4/Q5)"
                    .into(),
            },
        ))
    }

    /// `vm.resize {"desired_ram":<bytes>}` over the virtio-mem region `boot`
    /// opens when `vcfg.memory_max_mib` declares headroom (`memory_arg`).
    /// Unlike CPU, memory HAS a channel (D4): `vm.info`'s
    /// `memory_actual_size` only moves once the guest's virtio-mem driver
    /// claims the new blocks, so this polls it for `wait` before settling.
    fn hotplug_add_memory(
        &self,
        _vmdir: &Path,
        vm: &Vm,
        target_mib: u64,
        wait: Duration,
    ) -> delonix_model::Result<(u64, HotplugOutcome)> {
        let ceiling = vm.memory_max_mib.unwrap_or_else(|| mem_mib(&vm.memory));
        if target_mib > ceiling {
            return Err(delonix_model::Error::from(Error::InvalidResize(format!(
                "VM '{}' has no headroom for {target_mib} MiB — the declared maximum is {ceiling} \
                 MiB (see `vm resize --memory-max`, ADR-0068 D3)",
                vm.name
            ))));
        }
        let target_bytes = target_mib.saturating_mul(1024 * 1024);
        let (status, _) = ch_api_call_with_body(
            &vm.api_socket,
            "PUT",
            "/api/v1/vm.resize",
            Some(&format!("{{\"desired_ram\":{target_bytes}}}")),
            Duration::from_secs(10),
        )?;
        if !http_status_is_2xx(&status) {
            return Err(delonix_model::Error::from(Error::CloudHypervisorApi(
                format!(
                    "cloud-hypervisor vm.resize (memory) for VM '{}': {}",
                    vm.name,
                    if status.is_empty() {
                        "no response"
                    } else {
                        &status
                    }
                ),
            )));
        }
        // ADR-0068 D5: read back `memory_actual_size`, never the request's
        // own `desired_ram` — it is the guest's virtio-mem driver, not the
        // hypervisor's acceptance, that moves this number (D4, finding 4).
        let deadline = Instant::now() + wait;
        loop {
            let actual = ch_api_call(
                &vm.api_socket,
                "GET",
                "/api/v1/vm.info",
                Duration::from_secs(2),
            )
            .ok()
            .and_then(|(status, body)| http_status_is_2xx(&status).then_some(body))
            .and_then(|body| vm_info_memory_actual_bytes(&body));
            if let Some(actual) = actual {
                let actual_mib = actual / (1024 * 1024);
                if actual_mib >= target_mib {
                    return Ok((actual_mib, HotplugOutcome::Complete));
                }
                if Instant::now() >= deadline {
                    return Ok((
                        actual_mib,
                        HotplugOutcome::Partial {
                            reason: format!(
                                "cloud-hypervisor accepted the resize, but the guest's \
                                 virtio-mem driver has not plugged the memory within {}s — \
                                 actual {actual_mib} MiB of {target_mib} MiB requested \
                                 (no driver, or not onlined — ADR-0068 D8)",
                                wait.as_secs()
                            ),
                        },
                    ));
                }
            } else if Instant::now() >= deadline {
                return Err(delonix_model::Error::from(Error::CloudHypervisorApi(
                    format!(
                    "cloud-hypervisor accepted vm.resize for VM '{}' but `vm.info` could not be \
                     read back — the live memory size is unknown",
                    vm.name
                ),
                )));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    // ---- hotplug disk / NIC add (ADR-0068, Phases 4/5) --------------------
    //
    // Cloud Hypervisor's own `PUT /api/v1/vm.add-disk` / `vm.add-net`, over the
    // same per-VM api-socket the resize/pause verbs already use. Removal is a
    // separate capability (D10) and is not here.
    //
    // Like CPU add (D4/Q5) this backend has no in-band channel to ask the GUEST
    // whether it onlined the device — Cloud Hypervisor exposes no guest agent —
    // so the settled outcome is always `Partial`: the VMM accepted the device
    // (and the record keeps it, D6), but "the guest sees it" is proved out of
    // band, in the battery (`scripts/vm-native-ch-hotplug.sh`), never in band.

    /// `vm.add-disk {"path":…,"readonly":…}`. Cloud Hypervisor attaches it on
    /// the virtio-blk bus (no SATA/SCSI/IDE), so a disk asking for another bus
    /// is refused rather than silently moved. The target letter is the guest's
    /// to assign (virtio-blk has no stable name the VMM can pin), so it is left
    /// as given — unlike libvirt, which pins a `vdX`.
    fn hotplug_add_disk(
        &self,
        _vmdir: &Path,
        vm: &Vm,
        disk: &ExtraDisk,
        _wait: Duration,
        attached: &mut dyn FnMut(&ExtraDisk) -> delonix_model::Result<()>,
    ) -> delonix_model::Result<(ExtraDisk, HotplugOutcome)> {
        if disk.device == "cdrom" {
            return Err(delonix_model::Error::from(Error::InvalidResize(format!(
                "VM '{}': Cloud Hypervisor does not hot-plug a cdrom",
                vm.name
            ))));
        }
        if !disk.bus.is_empty() && disk.bus != "virtio" {
            return Err(delonix_model::Error::from(Error::InvalidResize(format!(
                "VM '{}': Cloud Hypervisor attaches disks on the virtio bus only, not '{}'",
                vm.name, disk.bus
            ))));
        }
        if !Path::new(&disk.source).exists() {
            return Err(delonix_model::Error::from(Error::InvalidResize(format!(
                "VM '{}': the disk to add '{}' is not on this host",
                vm.name, disk.source
            ))));
        }
        // `image_type` is MANDATORY, exactly as at boot (`boot_ch`): Cloud
        // Hypervisor's autodetect, when it guesses RAW, DISABLES sector-0 writes
        // as a safety opt-in — so `mkfs` on the fresh disk fails `ReadOnly` at
        // the guest (measured live, CH v52). The JSON enum is PascalCase
        // (`Raw`/`Qcow2`), unlike the `--disk image_type=` command line.
        let image_type = match disk.format.as_str() {
            "" | "qcow2" => "Qcow2",
            "raw" => "Raw",
            other => {
                return Err(delonix_model::Error::from(Error::InvalidResize(format!(
                    "VM '{}': Cloud Hypervisor hot-adds a qcow2 or raw disk, not format '{other}'",
                    vm.name
                ))))
            }
        };
        let body = format!(
            "{{\"path\":{},\"readonly\":{},\"image_type\":\"{image_type}\"}}",
            json_string(&disk.source),
            disk.read_only
        );
        let (status, _) = ch_api_call_with_body(
            &vm.api_socket,
            "PUT",
            "/api/v1/vm.add-disk",
            Some(&body),
            Duration::from_secs(10),
        )?;
        if !http_status_is_2xx(&status) {
            return Err(delonix_model::Error::from(Error::CloudHypervisorApi(format!(
                "cloud-hypervisor vm.add-disk for VM '{}': {}",
                vm.name,
                if status.is_empty() {
                    "no response"
                } else {
                    &status
                }
            ))));
        }
        // Recorded now (D6), before we settle: the VMM has the disk, so a
        // command killed from here on must not leave the domain with a device
        // the record does not name.
        let pinned = disk.clone();
        attached(&pinned)?;
        Ok((
            pinned,
            HotplugOutcome::Partial {
                reason: "cloud-hypervisor accepted the disk but has no channel to confirm the \
                         guest onlined it (ADR-0068 D4/Q5)"
                    .into(),
            },
        ))
    }

    /// `vm.add-net {"tap":…,"mac":…}` over a fresh tap this backend lays on the
    /// VM's SDN — one tap per NIC, keyed by a per-NIC owner derived from the
    /// pinned MAC so `stop` can tear exactly it down. `bridge`/`user` NICs have
    /// no home on this backend (that is the libvirt shape) and are refused.
    fn hotplug_add_nic(
        &self,
        _vmdir: &Path,
        vm: &Vm,
        nic: &ExtraNic,
        _wait: Duration,
        attached: &mut dyn FnMut(&ExtraNic) -> delonix_model::Result<()>,
    ) -> delonix_model::Result<(ExtraNic, HotplugOutcome)> {
        let named = match nic.kind.as_str() {
            "" | "network" => nic.source.clone().unwrap_or_else(|| vm.network.clone()),
            other => {
                return Err(delonix_model::Error::from(Error::InvalidResize(format!(
                    "VM '{}': Cloud Hypervisor attaches extra NICs on a declared network only, \
                     not '{other}' — use `--nic-add network:<name>`",
                    vm.name
                ))))
            }
        };
        // The ingress aliases (`""`/`ingress`/`bridge`/`default`) all name the
        // shared ingress the primary NIC sits on; resolve them to its canonical
        // empty name, so the attach resolves that network (`resolve_net` only
        // knows `""`/`ingress`) instead of hunting for a declared one that is
        // not there. A genuinely declared network keeps its name.
        let network = if names_a_declared_network(&named) {
            named
        } else {
            String::new()
        };
        let mac = match &nic.mac {
            Some(m) => m.clone(),
            None => ch_extra_nic_mac(vm),
        };
        let net = delonix_compute::vm_registry::network()?;
        if names_a_declared_network(&network) {
            net.ensure_network(&network)?;
        }
        // The tap's own owner (`<vm>~<mac>`): its own DHCP lease and ref marker,
        // distinct from the primary NIC's `vm-<name>`, so two NICs of one VM are
        // not one owner's two attaches. `stop` recomputes it from the recorded
        // MAC to detach.
        let owner = ch_extra_nic_owner(&vm.name, &mac);
        let tap = net.attach_tap(&owner, &network, &mac, &vm.namespace)?;
        let lease = net.lease_ip(&network, &mac);
        let body = format!(
            "{{\"tap\":{},\"mac\":{}}}",
            json_string(&tap),
            json_string(&mac)
        );
        let (status, _) = match ch_api_call_with_body(
            &vm.api_socket,
            "PUT",
            "/api/v1/vm.add-net",
            Some(&body),
            Duration::from_secs(10),
        ) {
            Ok(r) => r,
            Err(e) => {
                net.detach_tap(&owner, lease.as_deref());
                return Err(e.into());
            }
        };
        if !http_status_is_2xx(&status) {
            net.detach_tap(&owner, lease.as_deref());
            return Err(delonix_model::Error::from(Error::CloudHypervisorApi(format!(
                "cloud-hypervisor vm.add-net for VM '{}': {}",
                vm.name,
                if status.is_empty() {
                    "no response"
                } else {
                    &status
                }
            ))));
        }
        let mut pinned = nic.clone();
        pinned.mac = Some(mac);
        pinned.kind = "network".into();
        pinned.source = Some(network);
        // Recorded now (D6), before we settle.
        attached(&pinned)?;
        Ok((
            pinned,
            HotplugOutcome::Partial {
                reason: "cloud-hypervisor accepted the NIC but has no channel to confirm the \
                         guest onlined it (ADR-0068 D4/Q5)"
                    .into(),
            },
        ))
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
    ch_api_call_with_body(sock, method, path, None, timeout)
}

/// Same framing as [`ch_api_call`], with an optional JSON request body — the
/// only verb that needs one is `vm.resize` (`{"desired_vcpus":N}` /
/// `{"desired_ram":<bytes>}`, ADR-0068 D1), so every other caller keeps
/// sending the exact `Content-Length: 0` request it always did.
fn ch_api_call_with_body(
    sock: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Result<(String, Vec<u8>)> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(sock)
        .map_err(|e| Error::CloudHypervisorApi(format!("cloud-hypervisor api socket: {e}")))?;
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let body = body.unwrap_or("");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
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

/// A JSON string literal (quoted, with `"` and `\` escaped) for the one-field
/// bodies `vm.add-disk`/`vm.add-net` send — a host path or a MAC, never a
/// control character, so this is all the escaping those bodies can need.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A MAC for an extra NIC, derived from the VM name and distinct from the
/// primary MAC and every extra NIC already on the record — the same scheme the
/// libvirt backend uses, so a VM reads the same on both.
fn ch_extra_nic_mac(vm: &Vm) -> String {
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

/// The SDN owner key of an extra NIC's tap: `<vm>~<mac-without-colons>`. Keyed
/// on the PINNED MAC (recorded on the NIC) so `stop` recomputes exactly the
/// owner `hotplug_add_nic` attached, without a tap name of its own on the
/// record.
fn ch_extra_nic_owner(vm_name: &str, mac: &str) -> String {
    format!("{vm_name}~{}", mac.replace(':', ""))
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

fn boot_ch(
    vmdir: &Path,
    cfg: &VmConfig,
    overlay: &str,
    tap: &str,
    mac: &str,
    leaf: Option<&ProcessLeaf>,
) -> Result<i32> {
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

    launch_vmm(&join, &script, &pidfile, &sock, &log, VMM_READY_GRACE, leaf)
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
///
/// With a `leaf`, the launcher's child enters it BEFORE it execs (see
/// [`enter_leaf_at_exec`]), so the VMM — a grandchild through the `join` and the
/// `sh` — is born inside it and never allocates anywhere else.
fn launch_vmm(
    join: &[String],
    script: &str,
    pidfile: &Path,
    sock: &Path,
    log: &Path,
    ready: Duration,
    leaf: Option<&ProcessLeaf>,
) -> Result<i32> {
    let mut cmd = Command::new(&join[0]);
    cmd.args(&join[1..])
        .args(["sh", "-c", script])
        .env("DELONIX_INTERNAL", "1");
    // Held until the launcher has been spawned: the child writes through it.
    let _procs = match leaf {
        Some(l) => Some(enter_leaf_at_exec(&mut cmd, l).map_err(|e| {
            Error::LimitsNotEnforceable(format!(
                "cannot open {} to start the VMM inside its cgroup leaf: {e}",
                l.procs_path().display()
            ))
        })?),
        None => None,
    };
    let st = cmd.status().map_err(|e| match leaf {
        // The `pre_exec` write is the only step that runs between the fork and the
        // exec of a launcher that otherwise always exists; its failure surfaces here.
        Some(l) => Error::LimitsNotEnforceable(format!(
            "the VMM launcher could not enter its cgroup leaf {}: {e}",
            l.path().display()
        )),
        None => Error::Command {
            context: "cloud-hypervisor",
            message: e.to_string(),
        },
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

/// Makes `cmd`'s child move itself into `leaf` between the fork and the exec.
///
/// Opened HERE, in the parent, because the child may only make async-signal-safe
/// calls: it writes `0` (cgroup v2 reads it as «the writer») through a descriptor
/// that already exists, and a failed write aborts the spawn — the launcher never
/// runs outside the leaf. The file is opened with `O_CLOEXEC` (std's default), so
/// the launcher does not inherit it. The returned file must outlive the spawn.
fn enter_leaf_at_exec(cmd: &mut Command, leaf: &ProcessLeaf) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let procs = std::fs::OpenOptions::new()
        .write(true)
        .open(leaf.procs_path())?;
    let fd = procs.as_raw_fd();
    // SAFETY: the closure runs in the forked child before `exec`. It only calls
    // `write(2)` on a descriptor opened by the parent and kept open (`procs` is
    // returned to, and held by, the caller across the spawn), with a static
    // one-byte buffer, and builds its error with `last_os_error`, which reads
    // `errno` without allocating — all async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            if libc::write(fd, b"0".as_ptr().cast(), 1) == 1 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    Ok(procs)
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

/// Pure: the `memory_actual_size` of a `vm.info` body, in bytes — ADR-0068
/// D5's read-back for memory, never `config.memory.*` (measured, Annex A
/// finding 4, to answer "plugged" even when nothing plugged: ACPI reported a
/// removal that did not happen, and virtio-mem's `hotplugged_size` moves
/// before the guest driver claims anything — `memory_actual_size` is what
/// the VMM says the guest is actually using). No JSON parser needed for one
/// integer field; a `None` means the key was not found, never 0.
fn vm_info_memory_actual_bytes(body: &[u8]) -> Option<u64> {
    let compact: String = String::from_utf8_lossy(body)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let key = "\"memory_actual_size\":";
    let start = compact.find(key)? + key.len();
    let digits: String = compact[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Pure: how many of Cloud Hypervisor's own `vcpuN` thread names are present
/// — ADR-0068 D5's vCPU truth, never `config.boot_vcpus` (measured, Annex A
/// finding 3: a `vm.resize` that answers 204 can leave the OLD thread count
/// one request away from matching what was asked for).
fn count_vcpu_threads<'a>(thread_names: impl Iterator<Item = &'a str>) -> u32 {
    thread_names
        .filter(|n| {
            n.strip_prefix("vcpu")
                .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        })
        .count() as u32
}

/// Reads [`count_vcpu_threads`] off the real VMM process's threads
/// (`/proc/<pid>/task/<tid>/comm`). `None` when the pid cannot be read at
/// all (gone, or `/proc` denied) — never a silent 0, which would read as
/// "every vCPU disappeared" instead of "could not look".
fn ch_live_vcpu_count(pid: i32) -> Option<u32> {
    let dir = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    let names: Vec<String> = dir
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("comm")).ok())
        .map(|s| s.trim().to_string())
        .collect();
    Some(count_vcpu_threads(names.iter().map(String::as_str)))
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

    /// The VMM leaf's ceilings are the VM's own shape: vCPUs as whole cores,
    /// guest RAM plus the VMM's margin, and the hotplug ceilings when declared so a
    /// hotplug up to them is not OOM-killed by the leaf.
    #[test]
    fn the_vmm_leaf_ceilings_are_the_vms_shape() {
        let mut cfg = hpc_cfg(); // 4 vCPUs, 2G
        let l = vmm_leaf_limits(&cfg);
        assert_eq!(l.cpu_quota_usec, Some(400_000));
        assert_eq!(
            l.memory_max_bytes,
            Some((2048 + VMM_MEMORY_OVERHEAD_MIB) * 1024 * 1024)
        );
        assert_eq!(l.pids_max, Some(delonix_compute::DEFAULT_PIDS_MAX));
        assert_eq!(l.io_max, None);

        cfg.vcpus_max = Some(8);
        cfg.memory_max_mib = Some(8192);
        let l = vmm_leaf_limits(&cfg);
        assert_eq!(l.cpu_quota_usec, Some(800_000), "the hotplug ceiling");
        assert_eq!(
            l.memory_max_bytes,
            Some((8192 + VMM_MEMORY_OVERHEAD_MIB) * 1024 * 1024)
        );

        // A ceiling BELOW the boot size never shrinks the leaf under the guest.
        cfg.vcpus_max = Some(1);
        cfg.memory_max_mib = Some(512);
        let l = vmm_leaf_limits(&cfg);
        assert_eq!(l.cpu_quota_usec, Some(400_000));
        assert_eq!(
            l.memory_max_bytes,
            Some((2048 + VMM_MEMORY_OVERHEAD_MIB) * 1024 * 1024)
        );

        // A VM wide enough that its threads alone would near the default.
        cfg.vcpus = 300;
        cfg.vcpus_max = None;
        assert_eq!(vmm_leaf_limits(&cfg).pids_max, Some(664));
    }

    /// A [`delonix_compute::ports::ProcessLeaves`] that answers what it is told.
    struct FakeLeaves(std::result::Result<Option<ProcessLeaf>, String>);

    impl delonix_compute::ports::ProcessLeaves for FakeLeaves {
        fn create(
            &self,
            name: &str,
            _limits: &LeafLimits,
        ) -> delonix_model::Result<Option<ProcessLeaf>> {
            assert_eq!(name, "vm-v", "the leaf is named after the VM");
            self.0.clone().map_err(delonix_model::Error::Invalid)
        }
    }

    /// No leaf → the VM is REFUSED with the typed error, whatever the reason; the
    /// escape hatch is the only way past, and it starts the VM with no leaf.
    #[test]
    fn a_vm_whose_ceilings_no_leaf_can_hold_is_refused() {
        let cfg = hpc_cfg();
        let none = FakeLeaves(Ok(None));
        let failed = FakeLeaves(Err("memory.max=1: boom".into()));
        let cases: [(Option<&dyn delonix_compute::ports::ProcessLeaves>, &str); 3] = [
            (None, "no cgroup leaf provider"),
            (Some(&none), "no cgroup to create it in"),
            (Some(&failed), "boom"),
        ];
        for (port, why) in cases {
            let e = vmm_leaf(port, &cfg, false).expect_err("started with no leaf");
            assert_eq!(e.number(), 6515, "{e}");
            let text = e.to_string();
            assert!(text.contains(why), "{text}");
            assert!(text.contains("DELONIX_ALLOW_UNENFORCED_LIMITS"), "{text}");
            assert_eq!(vmm_leaf(port, &cfg, true).unwrap(), None);
        }
        let leaf = ProcessLeaf::at("/sys/fs/cgroup/delonix.slice/dlx-vm-v");
        let held = FakeLeaves(Ok(Some(leaf.clone())));
        assert_eq!(vmm_leaf(Some(&held), &cfg, false).unwrap(), Some(leaf));
    }

    /// The refusal comes BEFORE any effect: `boot` returns DX-6515 without
    /// reaching the network stage (no leaf provider — and no network — is
    /// registered in this test process, so before the leaf existed it failed
    /// later, on the network).
    #[test]
    fn boot_refuses_before_the_network_when_no_leaf_can_exist() {
        if std::env::var_os("DELONIX_ALLOW_UNENFORCED_LIMITS").is_some() {
            return; // the escape hatch is the one way past, by design
        }
        let tmp = tempfile::tempdir().unwrap();
        let stages = std::cell::RefCell::new(Vec::new());
        let on = |s: CreateStage| stages.borrow_mut().push(s);
        let e = CloudHypervisorBackend
            .boot(tmp.path(), &hpc_cfg(), "/nonexistent.qcow2", &on)
            .err()
            .expect("booted with no leaf");
        assert_eq!(e.number(), 6515, "{e}");
        assert!(
            stages.borrow().is_empty(),
            "a stage ran: {:?}",
            stages.borrow()
        );
    }

    /// `stop` removes only the leaf it created for this VM.
    #[test]
    fn a_stop_recognises_only_its_own_leaf() {
        assert!(is_vmm_leaf(
            &ProcessLeaf::at("/sys/fs/cgroup/delonix.slice/dlx-vm-web"),
            "web"
        ));
        assert!(!is_vmm_leaf(
            &ProcessLeaf::at("/sys/fs/cgroup/user.slice/session-2.scope"),
            "web"
        ));
        assert!(!is_vmm_leaf(
            &ProcessLeaf::at("/sys/fs/cgroup/delonix.slice/dlx-vm-web2"),
            "web"
        ));
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
        // each of the 4 vCPUs pinned to the host's 8-15 list, as the one
        // bracketed, comma-separated list Cloud Hypervisor v52 accepts.
        assert_eq!(
            cpus_arg(&c),
            "boot=4,affinity=[0@[8-15],1@[8-15],2@[8-15],3@[8-15]]"
        );
    }

    #[test]
    fn cpus_arg_declares_max_only_above_boot() {
        let mut c = hpc_cfg();
        // ADR-0068 D3's default: no headroom declared, no `max=` — byte for
        // byte what every VM without a ceiling already boots with.
        c.vcpus_max = None;
        assert_eq!(cpus_arg(&c), "boot=4");
        // A ceiling equal to (or below) the boot count is not headroom.
        c.vcpus_max = Some(4);
        assert_eq!(cpus_arg(&c), "boot=4");
        c.vcpus_max = Some(8);
        assert_eq!(cpus_arg(&c), "boot=4,max=8");
    }

    #[test]
    fn memory_arg_declares_virtio_mem_only_above_boot() {
        let mut c = hpc_cfg();
        c.memory_max_mib = None;
        assert_eq!(memory_arg(&c), "size=2048M");
        c.memory_max_mib = Some(2048);
        assert_eq!(memory_arg(&c), "size=2048M");
        c.memory_max_mib = Some(4096);
        assert_eq!(
            memory_arg(&c),
            "size=2048M,hotplug_method=virtio-mem,hotplug_size=2048M"
        );
        // hugepages and the hotplug region are independent CH knobs.
        c.hugepages = true;
        assert_eq!(
            memory_arg(&c),
            "size=2048M,hugepages=on,hotplug_method=virtio-mem,hotplug_size=2048M"
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
        let r = launch_vmm(
            &no_join(),
            &s,
            &pid,
            &sock,
            &log,
            Duration::from_secs(5),
            None,
        );
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
        let pid = launch_vmm(
            &no_join(),
            &s,
            &pidf,
            &sock,
            &log,
            Duration::from_secs(5),
            None,
        )
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
            None,
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

    #[test]
    fn vm_info_memory_actual_bytes_reads_the_real_shapes() {
        assert_eq!(
            vm_info_memory_actual_bytes(
                br#"{"config":{},"state":"Running","memory_actual_size":536870912}"#
            ),
            Some(536_870_912)
        );
        // pretty-printed, and the field appearing before other numeric keys
        // that could be mistaken for it.
        assert_eq!(
            vm_info_memory_actual_bytes(
                b"{\n  \"memory_actual_size\": 268435456,\n  \"boot_vcpus\": 1\n}"
            ),
            Some(268_435_456)
        );
        assert_eq!(vm_info_memory_actual_bytes(br#"{"state":"Running"}"#), None);
        assert_eq!(vm_info_memory_actual_bytes(b""), None);
    }

    #[test]
    fn count_vcpu_threads_matches_only_the_vcpu_shape() {
        assert_eq!(
            count_vcpu_threads(["vcpu0", "vcpu1", "vcpu2"].into_iter()),
            3
        );
        // Cloud Hypervisor's other threads (I/O, event loop, vhost-user) do
        // not start with "vcpu" at all here, but a near-miss must not count.
        assert_eq!(
            count_vcpu_threads(["vcpu0", "vhost_user", "vcpux", "vcpu"].into_iter()),
            1
        );
        assert_eq!(count_vcpu_threads(std::iter::empty()), 0);
    }
}
