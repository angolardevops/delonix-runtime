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

/// The network a Cloud Hypervisor VM is attached through, registered once by the
/// composition root ([`set_network`]).
use delonix_compute::Vm;
#[cfg(test)]
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
    CreateStage, DestroyStage, GuestFilesystem, GuestInfo, HotplugOutcome, MoveOptions,
    ReportFactory, VmBackend, VmConfig,
};

pub mod capabilities;
pub mod cloudinit;
pub mod firewall;
pub mod local_ports;
pub mod provider;

use delonix_compute::ports::VmBackends;
use delonix_compute::vm::VmEngine;
pub use delonix_compute::vm::{console_socket, serial_log_path};
pub use delonix_compute::vm::{valid_vm_name, Destroyed};
use delonix_compute::vm_registry;
pub use delonix_compute::vm_registry::mac_for;
pub use delonix_compute::vm_registry::set_network;
#[cfg(test)]
use delonix_compute::vm_registry::{auto_detect, unmet};
use delonix_provider_libvirt::LibvirtBackend;
pub use delonix_provider_libvirt::{libvirt_domain_xml, libvirt_uri};
pub use local_ports::disk_backing_format;
use local_ports::{CloudLocaldsSeed, QemuImgDisks, RegistryBackends};
// The pure halves of the use cases, read by this crate's own tests by their
// old names.
#[cfg(test)]
use delonix_compute::vm::{
    admission_verdict, boot_spec_of, config_from, resolve_required_capabilities, vm_namespace_of,
};
#[cfg(test)]
use delonix_provider_cloud_hypervisor::CloudHypervisorBackend;

/// The ports the orchestration below calls (see [`local_ports`]).
const VM_BACKENDS: RegistryBackends = RegistryBackends;

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

/// `true` if a VM with this name already exists.
pub fn exists(base: &Path, name: &str) -> bool {
    store(base).map(|s| s.exists(name)).unwrap_or(false)
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

// ===========================================================================
// Backend trait
// ===========================================================================

/// Names this build knows but only registers once configured (the hint an
/// unknown-name error gives).
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

/// Seeds the registry with this crate's two local backends, once, before
/// anything reads it (P4b.4a): the composition root's job, until the
/// application layer (P5) takes it.
fn seeded() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| vm_registry::seed(builtin_backends(), KNOWN_UNREGISTERED));
}

pub fn provider_reports() -> Vec<delonix_compute::capability::ProviderReport> {
    seeded();
    vm_registry::provider_reports()
}

pub fn register_backend(reg: BackendRegistration) -> Result<()> {
    seeded();
    vm_registry::register_backend(reg)
}

pub fn select_backend(want: Option<&str>) -> Result<Box<dyn VmBackend>> {
    seeded();
    vm_registry::select_backend(want)
}

pub fn select_backend_requiring(
    want: Option<&str>,
    required: &[Capability],
) -> Result<Box<dyn VmBackend>> {
    seeded();
    vm_registry::select_backend_requiring(want, required)
}

pub fn require_capabilities(backend_id: &str, required: &[Capability]) -> Result<()> {
    seeded();
    vm_registry::require_capabilities(backend_id, required)
}

pub fn backend_manages_own_storage(want: Option<&str>) -> bool {
    seeded();
    vm_registry::backend_manages_own_storage(want)
}

pub fn valid_backend_name(s: &str) -> Result<&'static str> {
    seeded();
    vm_registry::valid_backend_name(s)
}

fn backend_for(vm: &Vm) -> Result<Box<dyn VmBackend>> {
    seeded();
    vm_registry::backend_for(vm)
}

fn backend_declares(backend_id: &str, cap: Capability) -> bool {
    seeded();
    vm_registry::backend_declares(backend_id, cap)
}

fn canonical_backend_name(s: &str) -> Option<&'static str> {
    seeded();
    vm_registry::canonical_backend_name(s)
}

#[cfg(test)]
fn unknown_backend(name: &str) -> Error {
    seeded();
    vm_registry::unknown_backend(name)
}

#[cfg(test)]
fn with_backends<T>(f: impl FnOnce(&[BackendRegistration]) -> T) -> T {
    seeded();
    vm_registry::with_backends(f)
}

fn builtin_backends() -> Vec<BackendRegistration> {
    vec![
        delonix_provider_cloud_hypervisor::registration(),
        delonix_provider_libvirt::registration(),
    ]
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

/// What a firmware boot (a cloud image, no `--kernel`) asks for when the caller
/// named no backend.
#[derive(Debug, PartialEq, Eq)]
enum FirmwareBootPreference {
    /// libvirt boots cloud images with full UEFI/SeaBIOS, so it is asked for.
    Libvirt,
    /// libvirt is installed but does not meet the caller's requirements
    /// (ADR-0050 D6): the auto-detection picks among the backends that do.
    AnyMeetingRequirements,
    /// libvirt is not installed: Cloud Hypervisor gets the cloud image, with a
    /// warning.
    CloudHypervisorFallback,
}

/// libvirt is a PREFERENCE for a firmware boot, never a requirement. Asking for
/// it by name when it does not meet `--require` turned the preference into the
/// caller's choice: the requirement filter in [`auto_detect`] never ran, and a
/// `--require vm.namespace-isolation` was refused naming libvirt while Cloud
/// Hypervisor, installed and supporting it, was never asked.
fn firmware_boot_preference(
    libvirt_available: bool,
    libvirt_meets_requirements: bool,
) -> FirmwareBootPreference {
    match (libvirt_available, libvirt_meets_requirements) {
        (true, true) => FirmwareBootPreference::Libvirt,
        (true, false) => FirmwareBootPreference::AnyMeetingRequirements,
        (false, _) => FirmwareBootPreference::CloudHypervisorFallback,
    }
}

#[cfg(test)]
mod tests_firmware_boot_preference {
    use super::*;

    #[test]
    fn libvirt_is_preferred_only_when_it_meets_the_requirements() {
        assert_eq!(
            firmware_boot_preference(true, true),
            FirmwareBootPreference::Libvirt
        );
        assert_eq!(
            firmware_boot_preference(true, false),
            FirmwareBootPreference::AnyMeetingRequirements
        );
        assert_eq!(
            firmware_boot_preference(false, false),
            FirmwareBootPreference::CloudHypervisorFallback
        );
    }
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

/// The backend a NEW VM gets — the policy [`VmBackends::select`] answers for
/// this adapter's registry: the name `cfg` asks for, else the operator's
/// standing choice, else volumes ⇒ libvirt and the firmware preference, then
/// auto-detection among the backends that meet `required`.
fn select_for_create(
    base: &Path,
    cfg: &VmConfig,
    required: &[Capability],
) -> Result<Box<dyn VmBackend>> {
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
        None if cfg.kernel.is_none() => {
            let available = LibvirtBackend.available();
            let meets = available && require_capabilities("libvirt", required).is_ok();
            match firmware_boot_preference(available, meets) {
                FirmwareBootPreference::Libvirt => Some("libvirt".to_string()),
                FirmwareBootPreference::AnyMeetingRequirements => None,
                FirmwareBootPreference::CloudHypervisorFallback => {
                    tracing::warn!(
                        vm = %cfg.name,
                        "booting a cloud image on Cloud Hypervisor (libvirt not found) — if it \
                         panics on 'unable to mount root fs', install libvirt+qemu"
                    );
                    None
                }
            }
        }
        None => None,
    };
    select_backend_requiring(want.as_deref(), required)
}

// ===========================================================================
// Lifecycle (generic, delegates to the backend)
// ===========================================================================

/// The VM use cases, assembled over this adapter's ports (P4b.3b).
fn engine(
    base: &Path,
) -> Result<VmEngine<'_, JsonStore<Vm>, RegistryBackends, QemuImgDisks, CloudLocaldsSeed>> {
    Ok(VmEngine {
        root: base,
        repo: store(base)?,
        backends: RegistryBackends,
        disks: QemuImgDisks,
        seed: CloudLocaldsSeed,
        network: vm_registry::network().ok(),
    })
}

pub fn create(base: &Path, cfg: &VmConfig) -> Result<Vm> {
    engine(base)?.create(cfg)
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
pub fn create_with(base: &Path, cfg: &VmConfig, on: &dyn Fn(CreateStage)) -> Result<Vm> {
    engine(base)?.create_with(cfg, on)
}

/// Removes a VM: stops the VMM (via its backend), and deletes overlay/state.
///
/// If the backend cleanup fails (e.g. libvirt refuses the undefine), the local
/// record stays **INTACT** and the error propagates — the old version deleted the record
/// anyway and the VM was orphaned in libvirt, invisible to `vm ls`/`vm stop`. It also covers
/// the reverse: no local record but with an orphaned domain in libvirt (an
/// old interrupted `rm`), `remove` cleans up the domain anyway.
/// Removes a VM: stops the VMM (via its backend), and deletes overlay/state.
///
/// If the backend cleanup fails (e.g. libvirt refuses the undefine), the local
/// record stays **INTACT** and the error propagates — the old version deleted the record
/// anyway and the VM was orphaned in libvirt, invisible to `vm ls`/`vm stop`. It also covers
/// the reverse: no local record but with an orphaned domain in libvirt (an
/// old interrupted `rm`), `remove` cleans up the domain anyway.
pub fn remove(base: &Path, name: &str) -> Result<()> {
    engine(base)?.remove(name)
}

/// Like [`remove`], but deletes the local state EVEN if the backend cleanup
/// fails (the `vm rm --force`) — the user takes on resolving the rest in libvirt.
/// Like [`remove`], but deletes the local state EVEN if the backend cleanup
/// fails (the `vm rm --force`) — the user takes on resolving the rest in libvirt.
pub fn remove_force(base: &Path, name: &str) -> Result<()> {
    engine(base)?.remove_force(name)
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
    engine(base)?.destroy(name, force, purge_disks)
}

/// [`destroy`] that reports each [`DestroyStage`] as it starts.
/// [`destroy`] that reports each [`DestroyStage`] as it starts.
pub fn destroy_with(
    base: &Path,
    name: &str,
    force: bool,
    purge_disks: bool,
    on: &dyn Fn(DestroyStage),
) -> Result<Destroyed> {
    engine(base)?.destroy_with(name, force, purge_disks, on)
}

/// Stops the VM via ITS backend (CH/libvirt) but **preserves** the record and disk
/// (resumable). Unlike `remove`, it deletes nothing. Fixes the case where
/// the CLI's `vm stop` (direct-QEMU scheme) did not know how to stop a declarative
/// libvirt VM (pid null → the domain stayed alive, orphaned).
/// Stops the VM via ITS backend (CH/libvirt) but **preserves** the record and disk
/// (resumable). Unlike `remove`, it deletes nothing. Fixes the case where
/// the CLI's `vm stop` (direct-QEMU scheme) did not know how to stop a declarative
/// libvirt VM (pid null → the domain stayed alive, orphaned).
pub fn stop(base: &Path, name: &str) -> Result<()> {
    engine(base)?.stop(name)
}

/// Suspends a RUNNING VM's vCPUs (see [`VmBackend::pause`]). Refuses a VM
/// that is not currently `Running`, rather than a silent no-op — the caller
/// would otherwise have no way to tell "already paused" from "just paused".
/// Suspends a RUNNING VM's vCPUs (see [`VmBackend::pause`]). Refuses a VM
/// that is not currently `Running`, rather than a silent no-op — the caller
/// would otherwise have no way to tell "already paused" from "just paused".
pub fn pause(base: &Path, name: &str) -> Result<()> {
    engine(base)?.pause(name)
}

/// Resumes a VM suspended with [`pause`]. Refuses a VM that is not currently
/// `Paused`.
/// Resumes a VM suspended with [`pause`]. Refuses a VM that is not currently
/// `Paused`.
pub fn unpause(base: &Path, name: &str) -> Result<()> {
    engine(base)?.unpause(name)
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
    engine(base)?.set_cloud_init(name, hostname, ci_user, keys)
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
/// Changes a STOPPED VM's vCPUs, memory and/or boot disk size for its next
/// boot — the cold resize (`vm.resize.cold`/`vm.disk.resize`, see
/// [`VmBackend::resize_cold`]/[`VmBackend::resize_disk`]).
///
/// Everything that can be refused is refused before the backend is asked:
/// nothing to change, zero vCPUs, zero GiB of disk, a memory value that does
/// not parse (the lenient [`mem_mib`] would read `2GB` as 1 GiB and this
/// would report it done), and a VM that is running or paused — a guest that
/// only sees the change after its next reboot has not been resized yet. The
/// record is rewritten only after the backend returns `Ok`, so a refused or
/// failed resize leaves it saying what the VM actually has.
///
/// Returns the updated record.
pub fn resize(
    base: &Path,
    name: &str,
    vcpus: Option<u32>,
    memory: Option<&str>,
    disk_size_gib: Option<u32>,
) -> Result<Vm> {
    engine(base)?.resize(name, vcpus, memory, disk_size_gib)
}

/// `vm update <name> [--vcpus N] [--memory M]` (ADR-0068 D1): adds vCPUs
/// and/or memory to a RUNNING VM, live, up to its declared ceiling (D3); on
/// a STOPPED VM it is [`resize`]'s cold path. See [`delonix_compute::vm::VmEngine::update`].
///
/// Returns the updated record and, for a running VM, the
/// [`HotplugOutcome`] of each field asked for — `Partial` when the
/// hypervisor assigned the resource but the guest did not confirm it within
/// `wait` (ADR-0068 D4), never silently rounded up to success.
pub fn update(
    base: &Path,
    name: &str,
    vcpus: Option<u32>,
    memory: Option<&str>,
    wait: std::time::Duration,
) -> Result<(Vm, Vec<(&'static str, HotplugOutcome)>)> {
    engine(base)?.update(name, vcpus, memory, wait)
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
    engine(base)?.move_to_node(name, target, opts)
}

/// What the guest of VM `name` reports about itself through its agent
/// (`vm.guest-agent`, see [`VmBackend::guest_info`]). `Ok(None)` when the
/// record says the VM is not running — there is no guest to ask — or the
/// backend has no agent answer to give.
/// What the guest of VM `name` reports about itself through its agent
/// (`vm.guest-agent`, see [`VmBackend::guest_info`]). `Ok(None)` when the
/// record says the VM is not running — there is no guest to ask — or the
/// backend has no agent answer to give.
pub fn guest_info(base: &Path, name: &str) -> Result<Option<GuestInfo>> {
    engine(base)?.guest_info(name)
}

/// Takes a named snapshot of VM `name` (see [`VmBackend::snapshot`]). On libvirt a
/// running VM's snapshot is a system checkpoint (memory + disk).
/// Takes a named snapshot of VM `name` (see [`VmBackend::snapshot`]). On libvirt a
/// running VM's snapshot is a system checkpoint (memory + disk).
pub fn snapshot(base: &Path, name: &str, snap: &str) -> Result<()> {
    engine(base)?.snapshot(name, snap)
}

/// Reverts VM `name` to the named snapshot (see [`VmBackend::restore`]).
/// Reverts VM `name` to the named snapshot (see [`VmBackend::restore`]).
pub fn restore(base: &Path, name: &str, snap: &str) -> Result<()> {
    engine(base)?.restore(name, snap)
}

/// Copies a RUNNING VM's disk to `dest` without stopping it (see
/// [`VmBackend::backup_disk_live`]); a backend that cannot refuses by name.
/// Copies a RUNNING VM's disk to `dest` without stopping it (see
/// [`VmBackend::backup_disk_live`]); a backend that cannot refuses by name.
pub fn backup_disk_live(base: &Path, name: &str, dest: &Path, quiesce: bool) -> Result<()> {
    engine(base)?.backup_disk_live(name, dest, quiesce)
}

/// Lists VM `name`'s snapshot names (see [`VmBackend::snapshots`]).
/// Lists VM `name`'s snapshot names (see [`VmBackend::snapshots`]).
pub fn snapshots(base: &Path, name: &str) -> Result<Vec<String>> {
    engine(base)?.snapshots(name)
}

/// Deletes VM `name`'s snapshot `snap` (see [`VmBackend::delete_snapshot`]).
/// Deletes VM `name`'s snapshot `snap` (see [`VmBackend::delete_snapshot`]).
pub fn delete_snapshot(base: &Path, name: &str, snap: &str) -> Result<()> {
    engine(base)?.delete_snapshot(name, snap)
}

/// Applies one direction of VM `name`'s own firewall (see
/// [`VmBackend::apply_firewall`]).
/// Applies one direction of VM `name`'s own firewall (see
/// [`VmBackend::apply_firewall`]).
pub fn apply_firewall(base: &Path, name: &str, policy: &firewall::Policy) -> Result<()> {
    engine(base)?.apply_firewall(name, policy)
}

/// Reads one direction of VM `name`'s own firewall back (see
/// [`VmBackend::read_firewall`]).
/// Reads one direction of VM `name`'s own firewall back (see
/// [`VmBackend::read_firewall`]).
pub fn read_firewall(
    base: &Path,
    name: &str,
    direction: firewall::Direction,
) -> Result<firewall::Policy> {
    engine(base)?.read_firewall(name, direction)
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
    engine(base)?.start(name)
}

/// Stops (if running) then starts — always a real reboot, unlike `start`
/// (which no-ops when already running). Same recovered-fields caveat as
/// `start`/[`config_from`].
/// Stops (if running) then starts — always a real reboot, unlike `start`
/// (which no-ops when already running). Same recovered-fields caveat as
/// `start`/[`config_from`].
pub fn restart(base: &Path, name: &str) -> Result<Vm> {
    engine(base)?.restart(name)
}

/// Current state of a VM, with `status`/`ip` reconciled by its backend.
/// Current state of a VM, with `status`/`ip` reconciled by its backend.
pub fn status(base: &Path, name: &str) -> Result<Vm> {
    engine(base)?.status(name)
}

/// Lists all VMs, with reconciled state.
/// Lists all VMs, with reconciled state.
pub fn list(base: &Path) -> Result<Vec<Vm>> {
    engine(base)?.list()
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
        && !VM_BACKENDS.declares(backend_id, Capability::VmRestartPolicyNative)
}
/// Does this VM's recorded IP come from a PREDICTION rather than an
/// observation? See [`VmBackend::ip_is_predicted`].
///
/// `false` for a record whose backend this build cannot resolve: the caller is
/// a boot wait, and the useful default there is the one that does not go and
/// probe an address nobody can vouch for.
pub fn ip_is_predicted(vm: &Vm) -> bool {
    VM_BACKENDS
        .for_vm(vm)
        .map(|b| b.ip_is_predicted())
        .unwrap_or(false)
}
#[cfg(test)]
mod tests {
    use super::*;

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

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::create_dir_all(vms_dir(base)).unwrap();
        let st = store(base).unwrap();
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
        assert_eq!(status(base, "p").unwrap().status, Status::Paused);
        assert_eq!(st.load("p").unwrap().status, Status::Paused);

        ALIVE.store(false, Ordering::SeqCst);
        assert_eq!(status(base, "p").unwrap().status, Status::Stopped);
        assert_eq!(
            st.load("p").unwrap().status,
            Status::Stopped,
            "`vm ls` said Stopped while the record on disk stayed Paused"
        );

        vm_registry::deregister("pausavel");
    }

    #[test]
    fn stop_e_remove_de_vm_inexistente_dizem_no_such_vm() {
        // Regression from the bug report: `vm stop dev` without a record answered
        // "no such container: dev" — wrong noun for a VM — and
        // `vm rm` of a non-existent name returned silent success.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        for res in [super::stop(base, "nope"), super::remove(base, "nope")] {
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
            allow_mac_spoofing: false,
            ..Default::default()
        }
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
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp = tmp_dir.path();
        let vmdir = vms_dir(tmp);
        std::fs::create_dir_all(&vmdir).unwrap();
        // direct-QEMU record (raw scheme, WITHOUT `backend`) — as `vm run` writes it.
        std::fs::write(
            vmdir.join("myvm.json"),
            br#"{"name":"myvm","pid":1234,"memory":1024,"cpus":1}"#,
        )
        .unwrap();
        let mut cfg = hpc_cfg();
        cfg.name = "myvm".into();
        let err = create(tmp, &cfg).unwrap_err();
        assert!(
            format!("{err}").contains("vm run"),
            "create should refuse the clobber of a direct-QEMU record: {err}"
        );
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
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // Nothing set yet.
        assert_eq!(get_default_backend(dir), None);

        set_default_backend(dir, "KVM").unwrap();
        assert_eq!(get_default_backend(dir).as_deref(), Some("libvirt"));

        set_default_backend(dir, "ch").unwrap();
        assert_eq!(
            get_default_backend(dir).as_deref(),
            Some("cloud-hypervisor")
        );

        // Unknown name refused, previous value untouched.
        assert!(set_default_backend(dir, "hyperv").is_err());
        assert_eq!(
            get_default_backend(dir).as_deref(),
            Some("cloud-hypervisor")
        );

        clear_default_backend(dir).unwrap();
        assert_eq!(get_default_backend(dir), None);
        // Clearing an already-cleared default is not an error.
        clear_default_backend(dir).unwrap();

        // ADR-0054 §3: a name this process has not registered is kept, not
        // dropped — so selecting it fails instead of falling through to a
        // local hypervisor.
        std::fs::write(default_backend_file(dir), "Nave-Remota\n").unwrap();
        assert_eq!(get_default_backend(dir).as_deref(), Some("nave-remota"));
        assert!(select_backend(get_default_backend(dir).as_deref()).is_err());
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
            allow_mac_spoofing: true,
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
        // ADR-0055: an opt-out lost on restart would bring the NIC back
        // filtered and take the nested guests' network with it.
        assert!(back.allow_mac_spoofing);
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
        vm_registry::deregister("fakeremote");
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

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
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
        let e = create_with(base, &cfg, &|_| {}).unwrap_err();
        assert!(e.to_string().contains("refused"), "{e}");
        assert!(
            vitima.exists(),
            "o boot falhou e o motor apagou um ficheiro que nao criou"
        );

        vm_registry::deregister("falharemoto");
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

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let vmdir = vms_dir(base);
        std::fs::create_dir_all(vmdir.join("d")).unwrap();
        let outside = base.join("outside.qcow2");
        std::fs::write(vmdir.join("d.qcow2"), vec![0u8; 4096]).unwrap();
        std::fs::write(vmdir.join("d").join("seed.iso"), vec![0u8; 1024]).unwrap();
        std::fs::write(vmdir.join("d-data.qcow2"), vec![0u8; 2048]).unwrap();
        std::fs::write(&outside, b"operator image").unwrap();
        let st = store(base).unwrap();
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

        let d = destroy(base, "d", false, false).expect("destroy");
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
        destroy(base, "d", false, true).expect("destroy purge");
        assert!(!outside.exists());
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

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::create_dir_all(vms_dir(base)).unwrap();
        let st = store(base).unwrap();
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

        stop(base, "r").expect("stop");
        assert_eq!(STOPS.load(Ordering::SeqCst), 1);
        assert_eq!(
            DESTROYS.load(Ordering::SeqCst),
            0,
            "`vm stop` destruiu a VM — o disco de um backend remoto vai com ela"
        );

        remove(base, "r").expect("rm");
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

        vm_registry::deregister("contador");
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

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::create_dir_all(vms_dir(base)).unwrap();
        let st = store(base).unwrap();
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

        let out = start(base, "s").expect("start");
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

        vm_registry::deregister("retomavel");
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

    /// `guest_info`: a VM the record says is not running is never asked
    /// about (there is no guest), a running one returns the backend's answer
    /// as it is, and a backend with no guest channel answers `None`.
    #[test]
    fn guest_info_asks_only_a_running_vm_and_returns_the_backends_answer() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static ASKED: AtomicUsize = AtomicUsize::new(0);
        struct Talks;
        impl VmBackend for Talks {
            fn id(&self) -> &'static str {
                "fala"
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
                true
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
                Ok(())
            }
            fn guest_info(&self, _: &Vm) -> delonix_model::Result<Option<GuestInfo>> {
                ASKED.fetch_add(1, Ordering::SeqCst);
                Ok(Some(GuestInfo {
                    hostname: Some("g1".into()),
                    ..Default::default()
                }))
            }
        }
        register_backend(BackendRegistration {
            id: "fala",
            aliases: &[],
            auto_selectable: false,
            report: crate::capabilities::undeclared("fake"),
            new: Box::new(|| Ok(Box::new(Talks))),
        })
        .expect("registar");
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::create_dir_all(vms_dir(base)).unwrap();
        let st = store(base).unwrap();
        for (name, status) in [("parada", Status::Stopped), ("viva", Status::Running)] {
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
            vm.backend = "fala".into();
            vm.status = status;
            st.save(name, &vm).unwrap();
        }
        assert_eq!(guest_info(base, "parada").unwrap(), None);
        assert_eq!(ASKED.load(Ordering::SeqCst), 0, "a stopped VM was asked");
        let g = guest_info(base, "viva").unwrap().expect("an answer");
        assert_eq!(g.hostname.as_deref(), Some("g1"));
        assert_eq!(ASKED.load(Ordering::SeqCst), 1);
        assert!(guest_info(base, "nao-existe").unwrap_err().is_not_found());
        vm_registry::deregister("fala");
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

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::create_dir_all(vms_dir(base)).unwrap();
        let st = store(base).unwrap();
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
            code_of(move_to_node(base, "parada", "b", &storage_only).unwrap_err()),
            1538
        );
        let empty_storage = MoveOptions {
            with_local_disks: true,
            target_storage: Some(" ".into()),
            ..Default::default()
        };
        assert_eq!(
            code_of(move_to_node(base, "parada", "b", &empty_storage).unwrap_err()),
            1538
        );
        assert_eq!(
            code_of(move_to_node(base, "parada", " ", &offline).unwrap_err()),
            1538
        );
        assert_eq!(
            code_of(move_to_node(base, "parada", "b", &online).unwrap_err()),
            5507
        );
        assert_eq!(
            code_of(move_to_node(base, "a-correr", "b", &offline).unwrap_err()),
            5507
        );
        assert_eq!(
            code_of(move_to_node(base, "pausada", "b", &online).unwrap_err()),
            5507
        );
        assert_eq!(
            code_of(move_to_node(base, "pausada", "b", &offline).unwrap_err()),
            5507
        );
        assert!(move_to_node(base, "nao-existe", "b", &offline)
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
        assert!(move_to_node(base, "parada", "b", &offline).is_err());
        assert_eq!(
            handle("parada"),
            "fake:a:7",
            "a failed move rewrote the handle"
        );
        FAIL.store(false, Ordering::SeqCst);

        let vm = move_to_node(base, "parada", "b", &offline).unwrap();
        assert_eq!(vm.api_socket, "fake:b:7");
        assert_eq!(handle("parada"), "fake:b:7");
        let vm = move_to_node(base, "a-correr", "b", &online).unwrap();
        assert_eq!(vm.api_socket, "fake:b:7");
        assert_eq!(
            *CALLS.lock().unwrap(),
            vec![
                ("b".to_string(), false),
                ("b".to_string(), false),
                ("b".to_string(), true)
            ]
        );

        let e = move_to_node(base, "n", "b", &offline).unwrap_err();
        assert_eq!(e.number(), 1501, "{e}");
        let e = e.to_string();
        assert!(e.contains("sem-cluster") && e.contains("vm migrate"), "{e}");
        assert_eq!(handle("n"), "fake:a:7");

        vm_registry::deregister("movivel");
        vm_registry::deregister("sem-cluster");
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

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::create_dir_all(vms_dir(base)).unwrap();
        let st = store(base).unwrap();
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
        assert_eq!(
            code_of(resize(base, "r", None, None, None).unwrap_err()),
            1536
        );
        assert_eq!(
            code_of(resize(base, "r", Some(0), None, None).unwrap_err()),
            1536
        );
        assert_eq!(
            code_of(resize(base, "r", None, Some("2GB"), None).unwrap_err()),
            1536
        );
        assert_eq!(
            code_of(resize(base, "r", None, Some("0"), None).unwrap_err()),
            1536
        );
        assert_eq!(
            code_of(resize(base, "a-correr", Some(2), None, None).unwrap_err()),
            5505
        );
        assert_eq!(
            code_of(resize(base, "pausada", Some(2), None, None).unwrap_err()),
            5505
        );
        assert!(resize(base, "nao-existe", Some(2), None, None)
            .unwrap_err()
            .is_not_found());
        assert!(
            CALLS.lock().unwrap().is_empty(),
            "a refusal reached the backend"
        );
        unchanged("r");
        unchanged("a-correr");

        FAIL.store(true, Ordering::SeqCst);
        assert!(resize(base, "r", Some(4), None, None).is_err());
        unchanged("r");
        FAIL.store(false, Ordering::SeqCst);

        // Only memory: vCPUs keep the record's value, and the backend is told both.
        let vm = resize(base, "r", None, Some("4Gi"), None).unwrap();
        assert_eq!((vm.vcpus, vm.memory.as_str()), (1, "4Gi"));
        let vm = resize(base, "r", Some(3), None, None).unwrap();
        assert_eq!((vm.vcpus, vm.memory.as_str()), (3, "4Gi"));
        assert_eq!(st.load("r").unwrap().vcpus, 3);
        assert_eq!(
            *CALLS.lock().unwrap(),
            vec![(4, 1024), (1, 4096), (3, 4096)]
        );

        let e = resize(base, "n", Some(2), None, None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("resize") && e.contains("sem-resize"), "{e}");
        unchanged("n");

        vm_registry::deregister("redimensionavel");
        vm_registry::deregister("sem-resize");
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

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::create_dir_all(vms_dir(base)).unwrap();
        let st = store(base).unwrap();
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
            code_of(set_cloud_init(base, "c", None, None, None).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(base, "c", Some("-x"), None, None).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(base, "c", Some("a.b"), None, None).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(base, "c", None, Some("Root"), None).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(base, "c", None, None, Some(vec![])).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(base, "c", None, None, key("a\nb")).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(base, "app", Some("h"), None, None).unwrap_err()),
            1537
        );
        assert_eq!(
            code_of(set_cloud_init(base, "viva", Some("h"), None, None).unwrap_err()),
            5506
        );
        assert!(set_cloud_init(base, "nada", Some("h"), None, None)
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

        let vm = set_cloud_init(base, "c", Some("novo"), None, None).unwrap();
        assert_eq!(vm.boot.hostname.as_deref(), Some("novo"));
        assert_eq!(
            vm.boot.ssh_keys,
            vec!["ssh-ed25519 AAAA velha".to_string()],
            "keys kept"
        );
        let vm =
            set_cloud_init(base, "c", None, Some("ops"), key(" ssh-ed25519 AAAA nova ")).unwrap();
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

        let e = set_cloud_init(base, "sem", Some("h"), None, None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("cloud-init") && e.contains("libvirt"), "{e}");
        assert_eq!(
            st.load("sem").unwrap().boot.hostname.as_deref(),
            Some("velho")
        );

        vm_registry::deregister("com-ci");
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
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::create_dir_all(vms_dir(base)).unwrap();
        let st = store(base).unwrap();
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
        assert_eq!(status(base, "m").unwrap().api_socket, "remote:novo:7");
        assert_eq!(
            st.load("m").unwrap().api_socket,
            "remote:novo:7",
            "persisted"
        );
        assert_eq!(status(base, "fica").unwrap().api_socket, "remote:velho:7");
        assert_eq!(st.load("fica").unwrap().api_socket, "remote:velho:7");
        vm_registry::deregister("movido");
    }

    /// O campo é `#[serde(default)]`: um `.json` escrito antes disto existir
    /// tem de continuar a carregar, senão a correcção tranca VMs em disco.
    /// Escrito com o MESMO `JsonStore` que o motor usa, e sem o campo novo.
    #[test]
    fn um_registo_em_disco_sem_o_campo_continua_a_carregar() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let antigo = concat!(
            r#"{"name":"v","disk":"d","overlay":"o","vcpus":1,"#,
            r#""memory":"1G","network":"n","tap":"t","mac":"m","pid":42,"#,
            r#""api_socket":"s","status":"Running","created_unix":0}"#
        );
        std::fs::write(dir.join("v.json"), antigo).unwrap();
        let st: JsonStore<Vm> = JsonStore::open(dir).unwrap();
        let vm = st.load("v").expect("um registo antigo tem de carregar");
        assert_eq!(vm.pid, Some(42));
        assert_eq!(vm.pid_starttime, None);
    }
}
