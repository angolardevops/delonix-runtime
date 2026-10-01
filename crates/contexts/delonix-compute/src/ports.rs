//! The read ports the `container run` use case resolves its inputs through
//! (`docs/discovery/54_P2_COMPUTE_RUN.md`, step 2).
//!
//! The names are the ones ADR-0040 gives them. `ImageStore` and `StorageProvider`
//! belong to the artifact and storage contexts, which do not exist yet; they live
//! here until then, and move with a `pub use` left behind so no caller changes.
//!
//! An implementation owns its terminal manners (a progress line, a translated
//! message); the use case only sees data and errors.

use crate::{Container, Mount};
use delonix_model::records::ContainerFw;
use delonix_model::Result;

use crate::Notice;

/// What the use case reads from an image's config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageConfig {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    /// `""` when the image sets none.
    pub working_dir: String,
    /// The image's `USER`, verbatim (`odoo`, `101`, `101:101`); `""` when the
    /// image sets none. Read so the run can SAY it is not applied (ADR-0062).
    pub user: String,
}

/// Images: resolve a reference (pulling it when absent), read its config, and
/// prepare the root filesystem a container starts from.
pub trait ImageStore {
    type Image;

    /// The image a reference names, pulled when it is not local.
    fn resolve(&self, reference: &str) -> Result<Self::Image>;

    /// The parts of the image's config the record needs.
    fn config(&self, image: &Self::Image) -> ImageConfig;

    /// The root filesystem for container `id`. `second_pass` is the re-exec into
    /// a custom network's namespace, where a rootfs the first pass already
    /// prepared is reused instead of being unpacked again. `ephemeral` is a
    /// container whose write layer is discarded when it exits (`--rm`), which the
    /// store may prepare so its exit does not wait for the host's writeback
    /// (ADR-0056).
    fn prepare_rootfs(
        &self,
        image: &Self::Image,
        id: &str,
        second_pass: bool,
        ephemeral: bool,
    ) -> Result<String>;

    /// `--user <uid[:gid]|name[:group]>` resolved against the prepared rootfs.
    fn resolve_user(&self, rootfs: &str, spec: &str) -> Result<(u32, Option<u32>)>;
}

/// Volumes: the `-v` specifications resolved into mounts.
pub trait StorageProvider {
    fn resolve_mounts(&self, volumes: &[String], namespace: &str) -> Result<Vec<Mount>>;
}

/// What resolving `--gpus` and `--device` produced.
#[derive(Debug, Clone, Default)]
pub struct DeviceEdits {
    /// The final device list.
    pub devices: Vec<String>,
    /// Mounts a device specification injects.
    pub mounts: Vec<Mount>,
    /// Environment a device specification injects.
    pub env: Vec<String>,
    /// Warnings for the operator.
    pub notices: Vec<Notice>,
}

/// Devices: `--gpus` and `--device`, including vendor device specifications.
pub trait DeviceResolver {
    fn resolve(&self, gpus: Option<&str>, devices: &[String]) -> Result<DeviceEdits>;
}

/// This node: files named on the command line and the defaults it imposes.
pub trait RunHost {
    /// The contents of a file named by the specification (`--env-file`).
    fn read_file(&self, path: &str) -> std::io::Result<String>;
    fn default_memory(&self) -> String;
    fn default_cpus(&self) -> String;
    fn default_masked_paths(&self) -> Vec<String>;
    fn default_readonly_paths(&self) -> Vec<String>;
    fn rootless(&self) -> bool;
    /// A seccomp profile file: its JSON, and the syscalls it names that this
    /// architecture does not have. `Err` is the reason, shown after the flag.
    fn load_seccomp_profile(
        &self,
        path: &str,
    ) -> std::result::Result<(String, Vec<String>), String>;
    /// Makes sure an AppArmor profile is usable on this host (loading the
    /// engine's own profile when that is the one named).
    fn ensure_apparmor(&self, profile: &str) -> Result<()>;
    /// Refuses a `--secret` naming a secret that does not exist.
    fn check_secret(&self, name: &str) -> Result<()>;
    /// Where a detached container's log goes when `--log-file` is not given.
    fn default_log_path(&self, id: &str) -> String;
}

/// The node's network as a Cloud Hypervisor VM sees it: a `tap` on a network's
/// bridge inside the rootless infra, with a DHCP lease computable from the MAC.
/// Its home is the networking context (ADR-0040); it lives here until that
/// context exists.
///
/// `Send + Sync` because the VM engine keeps one process-wide, registered by
/// the composition root, like its backends.
pub trait VmNetwork: Send + Sync {
    /// Ensures a private network's bridge and DHCP exist before an attach.
    fn ensure_network(&self, name: &str) -> Result<()>;
    /// Creates `vm`'s tap on `network`, registered in `namespace`; returns the
    /// tap's name.
    fn attach_tap(&self, vm: &str, network: &str, mac: &str, namespace: &str) -> Result<String>;
    /// The address the network's DHCP gives `mac` — computed, not observed.
    fn lease_ip(&self, network: &str, mac: &str) -> Option<String>;
    /// Removes `vm`'s tap and forgets its lease.
    fn detach_tap(&self, vm: &str, lease: Option<&str>);
    /// The argv prefix that runs a command inside the infra's namespaces, or
    /// `None` when the infra is not up.
    fn join_argv(&self) -> Option<Vec<String>>;
}

/// The VM backends a node has, as the VM use cases ask for them
/// (`docs/discovery/61`, P4b.3). The registry that answers is the composition
/// root's; until the local backends leave `delonix-vm` (P4b.4) that adapter
/// implements it over its own table.
pub trait VmBackends {
    /// The backend a recorded VM runs on.
    fn for_vm(&self, vm: &crate::Vm) -> Result<Box<dyn crate::vm_backend::VmBackend>>;
    /// The backend a NEW VM gets: the one `cfg` names, else the operator's
    /// standing choice under `root`, else the registry's own policy
    /// (auto-detection, the firmware preference) — met against `required`.
    fn select(
        &self,
        root: &std::path::Path,
        cfg: &crate::vm_backend::VmConfig,
        required: &[crate::capability::Capability],
    ) -> Result<Box<dyn crate::vm_backend::VmBackend>>;
    /// Refuses when `backend_id` does not do all of `required`.
    fn require(&self, backend_id: &str, required: &[crate::capability::Capability]) -> Result<()>;
    /// Whether `backend_id` declares `cap` supported in its report.
    fn declares(&self, backend_id: &str, cap: crate::capability::Capability) -> bool;
    /// The refusals a backend makes of a config before anything is built — an
    /// option that backend cannot honour, refused by name instead of accepted
    /// and ignored (today: `allow_mac_spoofing` off a libvirt NIC with a tap).
    fn admit(&self, backend_id: &str, cfg: &crate::vm_backend::VmConfig) -> Result<()>;
    /// The backend that holds a VM named `name` which no record describes —
    /// left behind by an old `rm` — if any.
    fn unrecorded(&self, name: &str) -> Option<&'static str>;
    /// Powers off an unrecorded VM named `name`; `false` when there is none.
    fn stop_unrecorded(&self, name: &str) -> Result<bool>;
    /// Removes whatever a backend still holds for `name` without a record.
    fn remove_unrecorded(&self, name: &str) -> Result<()>;
}

/// The local disk work a VM on THIS host needs before it boots: the thin
/// overlay over its base image. A backend that owns its storage never calls it.
pub trait LocalDiskImages {
    /// Resolves `base` on this filesystem and builds `name`'s overlay in
    /// `vmdir` when it does not exist yet (`on` hears `CreateStage::Disk` only
    /// then), `size_gib` at least the base's. Answers `(base, overlay)`.
    fn overlay(
        &self,
        vmdir: &std::path::Path,
        name: &str,
        base: &str,
        size_gib: Option<u32>,
        on: &dyn Fn(crate::vm_backend::CreateStage),
    ) -> Result<(std::path::PathBuf, std::path::PathBuf)>;
}

/// The cloud-init seed a local VM boots with: the realized form of `cfg`'s
/// hostname, user, keys and volumes, as a file under `root`.
pub trait SeedBuilder {
    fn seed(
        &self,
        root: &std::path::Path,
        cfg: &crate::vm_backend::VmConfig,
    ) -> Result<std::path::PathBuf>;
}

/// The node's network: custom networks, published ports, per-container firewall
/// and shaping, and the L7 proxy's routes. Its home is the networking context
/// (ADR-0040); it lives here until that context exists.
pub trait NetworkProvider {
    /// Refuses a network that does not exist.
    fn check_network(&self, name: &str) -> Result<()>;
    /// Attaches container `id` to `network` in `namespace` — at `fixed_ip` when
    /// given — and returns the named network namespace and the address.
    fn attach(
        &self,
        id: &str,
        network: &str,
        namespace: &str,
        fixed_ip: Option<&str>,
    ) -> Result<(String, String)>;
    /// Undoes an attach; best effort, used on the way out of a failure.
    fn detach(&self, id: &str, ip: &str);
    /// Publishes one `-p` specification on a container's address.
    fn publish(&self, ip: &str, spec: &str) -> Result<()>;
    /// Unpublishes everything the record says is published; best effort.
    fn unpublish(&self, container: &Container);
    fn apply_firewall(&self, id: &str, ip: &str, fw: &ContainerFw) -> Result<()>;
    /// Limits the container's bandwidth (`--net-bps`, `--net-burst`).
    fn shape(&self, id: &str, bps: &str, burst: Option<&str>) -> Result<()>;
    /// Registers a `--expose` route in the L7 proxy.
    fn register_expose(&self, name: &str, namespace: &str, ip: &str, port: u16) -> Result<()>;
}
