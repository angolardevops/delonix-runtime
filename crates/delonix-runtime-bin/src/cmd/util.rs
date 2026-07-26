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
///
/// BUG FOUND: this used to be `.find(...)` on the id-prefix/exact-name
/// predicate, returning the FIRST match from `store.list()` (created-desc
/// order) — an ambiguous short prefix silently resolved to the newest
/// matching container instead of erroring, unlike Docker/Podman ("multiple
/// IDs found"). Destructive verbs (`stop`/`rm`) all resolve through this, so
/// an ambiguous prefix could silently act on the wrong container. Fixed:
/// an exact id or exact name match wins outright (unambiguous by
/// definition); otherwise every prefix match is collected, and more than
/// one is a hard error listing the candidates.
pub(crate) fn find(store: &Store, q: &str) -> Result<Container> {
    let all = store.list()?;
    if let Some(c) = all.iter().find(|c| c.id == q || c.name == q) {
        return Ok(c.clone());
    }
    let mut matches: Vec<Container> = all.into_iter().filter(|c| c.id.starts_with(q)).collect();
    match matches.len() {
        0 => Err(Error::Invalid(format!(
            "{}: {q}",
            super::po::t("container not found")
        ))),
        1 => Ok(matches.remove(0)),
        _ => {
            let ids: Vec<&str> = matches
                .iter()
                .map(|c| super::container::short_id(&c.id))
                .collect();
            Err(Error::Invalid(format!(
                "{}: {q} ({})",
                super::po::t("multiple containers match this prefix"),
                ids.join(", ")
            )))
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "delonix-util-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (Store::open(&dir).unwrap(), dir)
    }

    fn mk(id: &str, name: &str) -> Container {
        Container::new(
            id.to_string(),
            name.to_string(),
            "alpine".to_string(),
            vec!["sh".to_string()],
            "0".to_string(),
        )
    }

    #[test]
    fn find_prefixo_ambiguo_e_erro_nao_o_mais_recente() {
        // BUG regression guard: `find` used to silently return the FIRST
        // (newest-created) match on an ambiguous id prefix instead of
        // erroring — the exact opposite of Docker/Podman semantics.
        let (store, dir) = tmp_store();
        store.save(&mk("a1f3000000000000", "old")).unwrap();
        store.save(&mk("a1f9000000000000", "new")).unwrap();
        let err = find(&store, "a1").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("a1f300000000") || msg.contains("a1f900000000"),
            "{msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_prefixo_unico_resolve_normalmente() {
        let (store, dir) = tmp_store();
        store.save(&mk("a1f3000000000000", "old")).unwrap();
        store.save(&mk("b2000000000000000", "other")).unwrap();
        let c = find(&store, "a1f3").unwrap();
        assert_eq!(c.name, "old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_nome_exacto_ganha_mesmo_com_prefixo_de_id_ambiguo() {
        // An exact id/name match is unambiguous by definition and must win
        // outright, even if OTHER containers' ids happen to share a prefix
        // with the query string.
        let (store, dir) = tmp_store();
        store.save(&mk("a1f3000000000000", "web")).unwrap();
        store.save(&mk("a1f9000000000000", "other")).unwrap();
        let c = find(&store, "web").unwrap();
        assert_eq!(c.id, "a1f3000000000000");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
