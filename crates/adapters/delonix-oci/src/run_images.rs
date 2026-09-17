//! The compute context's `ImageStore` port, backed by this crate's store.
//!
//! What the terminal owns stays with the caller as hooks: the line printed before
//! a pull, the progress shown while an image is unpacked, and the wording of a
//! `--user` the image does not have.

use crate::rootfs_user::{resolve_user, UserLookupError};
use crate::{Image, ImageStore};
use delonix_compute::ports::ImageConfig;
use delonix_model::{Error, Result};
use std::path::{Path, PathBuf};

/// Unpacks a rootfs, showing whatever progress the caller wants around it.
pub type UnpackFn<'a> = dyn Fn(&mut dyn FnMut() -> Result<PathBuf>) -> Result<PathBuf> + 'a;

pub struct HostImages<'a> {
    pub store: &'a ImageStore,
    /// Called with the reference right before a pull starts.
    pub announce_pull: &'a dyn Fn(&str),
    /// Wraps the unpacking of a new rootfs (e.g. in a progress line).
    pub unpacking: &'a UnpackFn<'a>,
    /// Words a `--user` the image cannot satisfy as an error.
    pub user_error: &'a dyn Fn(UserLookupError) -> Error,
}

impl delonix_compute::ports::ImageStore for HostImages<'_> {
    type Image = Image;

    fn resolve(&self, reference: &str) -> Result<Image> {
        crate::registry::resolve_or_pull(self.store, reference, None, self.announce_pull)
    }

    fn config(&self, img: &Image) -> ImageConfig {
        ImageConfig {
            entrypoint: img.config.entrypoint.clone(),
            cmd: img.config.cmd.clone(),
            env: img.config.env.clone(),
            working_dir: img.config.working_dir.clone(),
        }
    }

    fn prepare_rootfs(&self, img: &Image, id: &str, second_pass: bool) -> Result<String> {
        // The re-exec's second pass reuses the rootfs the first pass prepared: a
        // full extraction again over a populated tree costs full price (measured).
        let prepare = || self.store.prepare_container_rootfs(img, id);
        let path = if second_pass && delonix_node::is_rootless() {
            match self.store.existing_rootfs_path(id) {
                Some(p) => p,
                None => prepare()?,
            }
        } else {
            (self.unpacking)(&mut || prepare())?
        };
        Ok(path.to_string_lossy().into_owned())
    }

    fn resolve_user(&self, rootfs: &str, spec: &str) -> Result<(u32, Option<u32>)> {
        resolve_user(Path::new(rootfs), spec).map_err(self.user_error)
    }
}
