//! The read ports the `container run` use case resolves its inputs through
//! (`docs/discovery/54_P2_COMPUTE_RUN.md`, step 2).
//!
//! The names are the ones ADR-0040 gives them. `ImageStore` and `StorageProvider`
//! belong to the artifact and storage contexts, which do not exist yet; they live
//! here until then, and move with a `pub use` left behind so no caller changes.
//!
//! An implementation owns its terminal manners (a progress line, a translated
//! message); the use case only sees data and errors.

use delonix_runtime_core::{Mount, Result};

use crate::Notice;

/// What the use case reads from an image's config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageConfig {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    /// `""` when the image sets none.
    pub working_dir: String,
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
    /// prepared is reused instead of being unpacked again.
    fn prepare_rootfs(&self, image: &Self::Image, id: &str, second_pass: bool) -> Result<String>;

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
}
