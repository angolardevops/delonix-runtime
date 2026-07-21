//! Helpers shared by several command groups (`container`, `image`,
//! `build`) — state root, opening the stores, image resolution, and the
//! rootless-flat vs root-overlay logic for preparing the rootfs.

use std::path::{Path, PathBuf};

use delonix_image::{Image, ImageStore};
use delonix_runtime::{self as runtime};
use delonix_runtime_core::{Container, Error, Result, Store};

/// The runtime's state root: `$DELONIX_ROOT` or the `ImageStore` default.
pub(crate) fn state_root() -> PathBuf {
    std::env::var_os("DELONIX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(ImageStore::default_root)
}

pub(crate) fn open_stores() -> Result<(ImageStore, Store)> {
    let root = state_root();
    let images = ImageStore::open(&root)?;
    let store = Store::open(root.join("containers"))?;
    Ok((images, store))
}

/// Resolve a local image; if missing, pull it from the registry.
pub(crate) fn resolve_or_pull(images: &ImageStore, reference: &str) -> Result<Image> {
    match images.resolve(reference) {
        Ok(img) => Ok(img),
        Err(_) => {
            eprintln!("a puxar {reference}…");
            delonix_image::pull_from_registry(images, reference)
        }
    }
}

/// Effective command (pure function): ENTRYPOINT + (the user's args, otherwise the
/// image's CMD) — the same semantics as Docker/OCI (`run <cmd>` replaces the CMD, not
/// the ENTRYPOINT).
pub(crate) fn compose_command(
    entrypoint: &[String],
    cmd: &[String],
    user: &[String],
) -> Vec<String> {
    let mut v = entrypoint.to_vec();
    if user.is_empty() {
        v.extend(cmd.iter().cloned());
    } else {
        v.extend(user.iter().cloned());
    }
    v
}

/// Like [`compose_command`], but from the image's config.
pub(crate) fn effective_command(img: &Image, user: &[String]) -> Vec<String> {
    compose_command(&img.config.entrypoint, &img.config.cmd, user)
}

/// `chown -R <uid>:<uid>` of a FLAT rootfs (rootless): without this, the files
/// belong to the host's uid 0, which ends up unmapped inside the user namespace.
/// Delegates to `delonix_runtime::lchown_tree` (uses `lchown`, never follows symlinks —
/// see the security note there; don't reimplement this locally with
/// `std::os::unix::fs::chown`, which follows symlinks).
pub(crate) fn chown_tree(path: &Path, uid: u32) -> Result<()> {
    delonix_runtime::lchown_tree(path, uid, uid);
    Ok(())
}

/// Locates a container by ID prefix or by exact name.
pub(crate) fn find(store: &Store, q: &str) -> Result<Container> {
    let all = store.list()?;
    all.into_iter()
        .find(|c| c.id == q || c.id.starts_with(q) || c.name == q)
        .ok_or_else(|| Error::Invalid(format!("{}: {q}", super::po::t("container not found"))))
}

/// Prepares a new container's rootfs from an image: FLAT (export +
/// chown to the userns uid) in rootless, or a mounted overlay in root mode. Same
/// rule used by `container run` and by `build` (the "work" container).
pub(crate) fn prepare_rootfs(images: &ImageStore, img: &Image, id: &str) -> Result<String> {
    let rootless = runtime::is_rootless();
    if rootless {
        let rfs = images.root().join("containers").join(id).join("rootfs");
        images.export_rootfs(img, &rfs)?;
        chown_tree(&rfs, runtime::USERNS_UID_BASE)?;
        Ok(rfs.to_string_lossy().into_owned())
    } else {
        Ok(images.mount_rootfs(img, id)?.to_string_lossy().into_owned())
    }
}
