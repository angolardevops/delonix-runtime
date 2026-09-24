//! `delonix-vm`'s own failures, grouped by what went wrong, and the dictionary
//! number of each group (ADR-0043).
//!
//! **The messages are a contract with what was there before.** Each variant
//! carries the text the call site used to build by hand and converts into the
//! same shared class, wrapped with its number — so the CLI prints byte for
//! byte what it printed and exits with the same code. A handful of sites that
//! were generic `Invalid` before but always meant "this host lacks a
//! capability" (no backend installed, no firmware found) now convert into
//! [`delonix_model::Error::Unavailable`] instead — the same correction the
//! shared type's own doc comment describes for `wg`/`virt-customize`/`ngrok`,
//! extended here to this crate's own equivalents.
//!
//! **Grouped, not enumerated one-to-one.** Several call sites that shell out to
//! `virsh`/`qemu-img`/the Cloud Hypervisor process share one shape — "an
//! external tool this crate depends on failed" — and share one variant
//! ([`Error::Command`]), the same pattern `delonix-volume`'s `Command` variant
//! already uses. The seven `backup_disk_live` pipeline steps that are not the
//! "wrong backend" precondition collapse into [`Error::LiveBackupFailed`] for
//! the same reason: seven near-identical numbers would not tell a caller
//! anything seven near-identical `SystemFailure`s do not already say once.
//!
//! **[`Error::VmNotFound`] is not numbered here.** `delonix_model::Error` has
//! carried a `VmNotFound` variant, with its own dictionary number (`DX-4501`,
//! `vm.not_found`), since before this crate had a local error type — the
//! `NotFound` message says "no such container", which reads wrong for a `vm
//! stop`/`vm rm`. This crate's own `VmNotFound` is the same failure, so it
//! converts straight into the shared variant instead of wrapping it a second
//! time under a second number.

use thiserror::Error;

/// A failure of `delonix-vm`.
#[derive(Debug, Error)]
pub enum Error {
    // ---- invalid argument -------------------------------------------------
    /// A pause/unpause/snapshot verb a backend does not implement.
    #[error("{0}")]
    UnsupportedByBackend(String),

    /// [`super::register_backend`] refused a registration: an empty id, an
    /// `auto_selectable` claim from outside this crate, or a name already
    /// claimed by another backend.
    #[error("{0}")]
    BackendRegistrationRefused(String),

    /// A `--backend`/`DELONIX_VM_BACKEND` value that names nothing registered.
    #[error("{0}")]
    UnknownBackend(String),

    /// A VM record names a backend this process has no registration for.
    #[error("{0}")]
    UnregisteredBackendInRecord(String),

    /// `spec.volumes` on a VM forced onto a backend that cannot mount them
    /// (Cloud Hypervisor has no virtio-9p).
    #[error("{0}")]
    RequiresLibvirtBackend(String),

    /// A UNIX socket path (api-socket or console) that would not fit in
    /// `sun_path`.
    #[error("{0}")]
    SocketPathTooLong(String),

    /// A VM name outside [`super::valid_vm_name`]'s charset.
    #[error("{0}")]
    InvalidName(String),

    /// `--namespace` on a backend whose VMs are not on this engine's SDN.
    #[error("{0}")]
    NamespaceUnsupported(String),

    /// `--disk-size` smaller than the base image it would overlay.
    #[error("{0}")]
    DiskTooSmall(String),

    /// A snapshot name outside [`super::valid_vm_name`]'s charset.
    #[error("{0}")]
    InvalidSnapshotName(String),

    /// `--ip` on a libvirt VM whose net mode is not `nat`/`network`.
    #[error("{0}")]
    StaticIpRequiresNat(String),

    /// A `--ip` value that does not parse as an IPv4 address.
    #[error("{0}")]
    InvalidStaticIp(String),

    /// `virsh net-update` refused a DHCP reservation for a static IP.
    #[error("{0}")]
    StaticIpReservationFailed(String),

    /// Live disk backup asked of a VM that is not on the libvirt backend.
    #[error("{0}")]
    LiveBackupNeedsLibvirt(String),

    /// `pause`/`unpause` asked of a VM that is not, respectively, running or
    /// paused — nothing to do, and answering silently would hide that. Kept as
    /// `InvalidArgument` (not the shared `NotRunning` class) rather than
    /// prefixing the message with the container-flavoured "container is not
    /// running:" — the wording that class's `Display` uses everywhere else.
    #[error("{0}")]
    NotRunningForOp(String),
    /// A `required_capabilities` name the catalog does not have — a typo, or a
    /// name from another catalog version. An INVALID ARGUMENT on purpose: read
    /// as "unsupported" it would send the caller shopping for a provider.
    #[error("{0}")]
    UnknownCapability(String),

    // ---- not found ----------------------------------------------------
    /// There is no VM with the given name. Converts directly into the shared
    /// [`delonix_model::Error::VmNotFound`] — see the module doc comment.
    #[error("no such VM: {0} (see `delonix vm ls`)")]
    VmNotFound(String),

    /// This VM has no snapshot by that name.
    #[error("{0}")]
    SnapshotNotFound(String),

    // ---- conflict -------------------------------------------------------
    /// A VM record already exists under this name, from the other (direct-QEMU)
    /// VM subsystem that shares the same `vms/` folder.
    #[error("{0}")]
    RecordConflict(String),

    /// A snapshot name this VM already has.
    #[error("{0}")]
    SnapshotTaken(String),

    // ---- unavailable ------------------------------------------------------
    /// A named backend this build knows about but has not configured (e.g.
    /// Proxmox without a target).
    #[error("{0}")]
    BackendNotConfigured(String),

    /// Auto-detection found no VM backend installed at all.
    #[error("{0}")]
    NoBackendAvailable(String),

    /// No `kernel`/`firmware` given and no bundled `rust-hypervisor-fw` found.
    #[error("{0}")]
    NoFirmware(String),
    /// The selected (or every auto-selectable) backend does not mark a
    /// required capability usable on this host — the contract's
    /// `FAILED_PRECONDITION` / `CapabilityNotSupported` (ADR-0050 D6), in the
    /// UNAVAILABLE class: the remedy is another provider or this host, never
    /// the argument.
    #[error("{0}")]
    CapabilityNotSupported(String),

    // ---- system failure -----------------------------------------------
    /// An external tool this crate shells out to (`virsh`, `qemu-img`, the
    /// `cloud-hypervisor` process) could not be run or exited badly.
    #[error("system call `{context}` failed: {message}")]
    Command {
        /// What was being done.
        context: &'static str,
        /// The tool's own error.
        message: String,
    },

    /// The Cloud Hypervisor VMM's local HTTP API (over its api-socket) could
    /// not be reached or answered with a non-2xx status.
    #[error("{0}")]
    CloudHypervisorApi(String),

    /// `qemu-img check` found a stopped Cloud Hypervisor VM's disk corrupted
    /// (BUG-VM-001) — a post-condition check, not the stop itself failing.
    #[error("{0}")]
    DiskCorrupted(String),

    /// A snapshot verb refused because the Cloud Hypervisor VM it targets is
    /// still running: the vmm holds the qcow2 exclusively.
    #[error("{0}")]
    SnapshotNeedsStopped(String),

    /// A stopped libvirt VM has no `<name>.xml` recorded, so there is nothing
    /// to redefine for a snapshot/restore verb.
    #[error("{0}")]
    NoStoppedDomainXml(String),

    /// The host-protection admission check refused to boot a VM whose
    /// requested memory does not fit in what the host has available.
    #[error("{0}")]
    AdmissionRefused(String),

    /// A step of the live-disk-backup pipeline (list disks, stage the
    /// overlay, snapshot, copy, pivot) failed.
    #[error("{0}")]
    LiveBackupFailed(String),

    /// A failure of the layers underneath (filesystem, JSON, state), with its
    /// own class.
    #[error(transparent)]
    Engine(#[from] delonix_model::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Engine(e.into())
    }
}

impl From<crate::cloudinit::Error> for Error {
    fn from(e: crate::cloudinit::Error) -> Self {
        Error::Engine(e.into())
    }
}

impl From<delonix_state::Error> for Error {
    fn from(e: delonix_state::Error) -> Self {
        Error::Engine(e.into())
    }
}

/// The shared class each `delonix-vm` failure belongs to.
type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure (ADR-0043). A failure of the
    /// layers underneath keeps its own; [`Error::VmNotFound`] keeps the
    /// number the shared variant it converts into already had.
    pub fn number(&self) -> u16 {
        match self {
            Error::UnsupportedByBackend(_) => 1501,
            Error::BackendRegistrationRefused(_) => 1502,
            Error::UnknownBackend(_) => 1503,
            Error::UnregisteredBackendInRecord(_) => 1504,
            Error::RequiresLibvirtBackend(_) => 1505,
            Error::SocketPathTooLong(_) => 1506,
            Error::InvalidName(_) => 1507,
            Error::NamespaceUnsupported(_) => 1508,
            Error::DiskTooSmall(_) => 1509,
            Error::InvalidSnapshotName(_) => 1510,
            Error::StaticIpRequiresNat(_) => 1511,
            Error::InvalidStaticIp(_) => 1512,
            Error::StaticIpReservationFailed(_) => 1513,
            Error::LiveBackupNeedsLibvirt(_) => 1514,
            Error::NotRunningForOp(_) => 1515,
            Error::UnknownCapability(_) => 1527,
            Error::VmNotFound(_) => 4501,
            Error::SnapshotNotFound(_) => 4502,
            Error::RecordConflict(_) => 5501,
            Error::SnapshotTaken(_) => 5502,
            Error::BackendNotConfigured(_) => 6501,
            Error::NoBackendAvailable(_) => 6502,
            Error::NoFirmware(_) => 6503,
            Error::CapabilityNotSupported(_) => 6507,
            Error::Command { .. } => 9501,
            Error::CloudHypervisorApi(_) => 9502,
            Error::DiskCorrupted(_) => 9503,
            Error::SnapshotNeedsStopped(_) => 9504,
            Error::NoStoppedDomainXml(_) => 9505,
            Error::AdmissionRefused(_) => 9506,
            Error::LiveBackupFailed(_) => 9507,
            Error::Engine(e) => e.number(),
        }
    }

    /// «No such resource» — asked of the class, like the shared error.
    pub fn is_not_found(&self) -> bool {
        self.number() / 1000 == 4
    }

    /// «Already exists».
    pub fn is_conflict(&self) -> bool {
        self.number() / 1000 == 5
    }

    /// The shared class this failure converts into, for a caller that needs a
    /// variant's payload (`match e.into_root() { Error::VmNotFound(n) => … }`).
    pub fn into_root(self) -> Dx {
        Dx::from(self).into_root()
    }
}

impl From<Error> for Dx {
    fn from(e: Error) -> Self {
        // `VmNotFound` is a shared variant with its own number ALREADY, and
        // wrapping it in `Dx::coded` a second time would be a second number
        // for the same failure — see the module doc comment.
        if let Error::VmNotFound(name) = e {
            return Dx::VmNotFound(name);
        }
        let number = e.number();
        let class = match e {
            Error::SnapshotNotFound(text) => Dx::NotFound(text),
            Error::RecordConflict(text) | Error::SnapshotTaken(text) => Dx::Conflict(text),
            Error::BackendNotConfigured(text)
            | Error::NoBackendAvailable(text)
            | Error::NoFirmware(text)
            | Error::CapabilityNotSupported(text) => Dx::Unavailable(text),
            Error::Command { context, message } => Dx::Runtime { context, message },
            Error::CloudHypervisorApi(text)
            | Error::DiskCorrupted(text)
            | Error::SnapshotNeedsStopped(text)
            | Error::NoStoppedDomainXml(text)
            | Error::AdmissionRefused(text)
            | Error::LiveBackupFailed(text) => Dx::Runtime {
                context: "vm",
                message: text,
            },
            Error::VmNotFound(_) => unreachable!("handled above"),
            Error::Engine(e) => return e,
            e => Dx::Invalid(e.to_string()),
        };
        Dx::coded(number, class)
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    fn every_variant() -> Vec<Error> {
        vec![
            Error::UnsupportedByBackend("pause is not supported on the 'proxmox' backend".into()),
            Error::BackendRegistrationRefused("a backend registration needs an id".into()),
            Error::UnknownBackend("unknown VM backend: 'x' (use 'ch', 'libvirt')".into()),
            Error::UnregisteredBackendInRecord(
                "vm 'x': its record names backend 'y', which this process does not have \
                 registered (it has 'cloud-hypervisor', 'libvirt')"
                    .into(),
            ),
            Error::RequiresLibvirtBackend(
                "VM 'x': spec.volumes requires the libvirt backend".into(),
            ),
            Error::SocketPathTooLong("VM 'x': socket path ... is 120 bytes".into()),
            Error::InvalidName("invalid VM name 'x'".into()),
            Error::NamespaceUnsupported(
                "namespace 'a' is not enforceable on the 'x' backend".into(),
            ),
            Error::DiskTooSmall("--disk-size 1G é menor que a imagem base (2 GiB)".into()),
            Error::InvalidSnapshotName("invalid snapshot name: ../x".into()),
            Error::StaticIpRequiresNat("VM 'x': --ip requires the libvirt `nat` mode".into()),
            Error::InvalidStaticIp("VM 'x': invalid static IP 'y'".into()),
            Error::StaticIpReservationFailed(
                "could not reserve static IP 1.2.3.4 on libvirt network 'default': boom".into(),
            ),
            Error::LiveBackupNeedsLibvirt(
                "live disk backup needs the libvirt backend (this VM runs on x)".into(),
            ),
            Error::UnknownCapability("unknown capability 'vm.nope'".into()),
            Error::CapabilityNotSupported(
                "the 'libvirt' backend does not support what this VM requires".into(),
            ),
            Error::NotRunningForOp(
                "VM 'x' is not running (status: Stopped) — nothing to pause".into(),
            ),
            Error::VmNotFound("dev".into()),
            Error::SnapshotNotFound("snapshot of VM 'x': s1".into()),
            Error::RecordConflict(
                "a VM 'x' created by `vm run` (direct-QEMU) already exists".into(),
            ),
            Error::SnapshotTaken("VM 'x' already has a snapshot named 's1'".into()),
            Error::BackendNotConfigured(
                "VM backend 'proxmox' is not available in this build: y".into(),
            ),
            Error::NoBackendAvailable(
                "no VM backend available: install 'cloud-hypervisor' or 'libvirt'+'qemu'".into(),
            ),
            Error::NoFirmware(
                "VM without 'kernel' or 'firmware' and no rust-hypervisor-fw found".into(),
            ),
            Error::Command {
                context: "vm-tool",
                message: "qemu-img: boom".into(),
            },
            Error::CloudHypervisorApi("cloud-hypervisor api socket: connection refused".into()),
            Error::DiskCorrupted(
                "VM 'x' stopped, but `qemu-img check` now finds its disk corrupted".into(),
            ),
            Error::SnapshotNeedsStopped(
                "cloud-hypervisor cannot take a snapshot of a RUNNING VM".into(),
            ),
            Error::NoStoppedDomainXml(
                "VM 'x' is stopped and its libvirt domain description is not on disk".into(),
            ),
            Error::AdmissionRefused(
                "host protection: VM 'x' asks for 4096 MiB but the host only has 2048 MiB \
                 available"
                    .into(),
            ),
            Error::LiveBackupFailed("live backup: cannot list the disks of x: boom".into()),
            Error::Engine(delonix_model::Error::Conflict("x".into())),
        ]
    }

    #[test]
    fn every_failure_keeps_its_number_through_the_conversion() {
        for e in every_variant() {
            let number = e.number();
            let shown = e.to_string();
            let converted = delonix_model::Error::from(e);
            assert_eq!(converted.number(), number, "{shown}");
            assert!(
                delonix_model::codes::lookup(number).is_some()
                    // `VmNotFound` reuses the pre-existing `vm.not_found` entry —
                    // it was published before this crate had a local Error type.
                    || number == 4501,
                "DX-{number:04} ({shown}) has no dictionary entry"
            );
        }
    }

    /// The text the CLI printed before this type existed: the shared class
    /// wraps the local variant's message verbatim.
    #[test]
    fn the_converted_message_is_the_one_printed_before() {
        let cases = [
            (
                Error::InvalidName("invalid VM name 'x'".into()),
                "invalid argument: invalid VM name 'x'",
            ),
            (
                Error::VmNotFound("dev".into()),
                "no such VM: dev (see `delonix vm ls`)",
            ),
            (
                Error::SnapshotNotFound("snapshot of VM 'x': s1".into()),
                "no such snapshot of VM 'x': s1",
            ),
            (
                Error::SnapshotTaken("VM 'x' already has a snapshot named 's1'".into()),
                "conflict: VM 'x' already has a snapshot named 's1'",
            ),
            (
                Error::NoBackendAvailable(
                    "no VM backend available: install 'cloud-hypervisor' or 'libvirt'+'qemu'"
                        .into(),
                ),
                "unavailable: no VM backend available: install 'cloud-hypervisor' or \
                 'libvirt'+'qemu'",
            ),
            (
                Error::NotRunningForOp(
                    "VM 'x' is not running (status: Stopped) — nothing to pause".into(),
                ),
                "invalid argument: VM 'x' is not running (status: Stopped) — nothing to pause",
            ),
            (
                Error::Command {
                    context: "vm-tool",
                    message: "qemu-img: boom".into(),
                },
                "system call `vm-tool` failed: qemu-img: boom",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(delonix_model::Error::from(e).to_string(), want);
        }
    }

    /// `into_root` is what `load_vm`/`start`/`restart`/`status` match on to
    /// retarget a state-layer `NotFound` into this crate's own `VmNotFound` —
    /// preserved exactly as it worked before this crate had its own `Error`.
    #[test]
    fn into_root_unwraps_to_the_shared_class() {
        let e = Error::SnapshotNotFound("snapshot of VM 'x': s1".into());
        assert!(matches!(e.into_root(), delonix_model::Error::NotFound(_)));
    }
}
