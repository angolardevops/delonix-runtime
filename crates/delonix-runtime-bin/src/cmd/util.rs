//! Helpers partilhados por vários grupos de comandos (`container`, `image`,
//! `build`) — raiz de estado, abertura de armazéns, resolução de imagens e a
//! lógica rootless-flat vs root-overlay de preparação de rootfs.

use std::path::{Path, PathBuf};

use delonix_image::{Image, ImageStore};
use delonix_runtime::{self as runtime};
use delonix_runtime_core::{Container, Error, Result, Store};

/// Raiz de estado do runtime: `$DELONIX_ROOT` ou o default do `ImageStore`.
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

/// Resolve uma imagem local; se faltar, puxa-a do registo.
pub(crate) fn resolve_or_pull(images: &ImageStore, reference: &str) -> Result<Image> {
    match images.resolve(reference) {
        Ok(img) => Ok(img),
        Err(_) => {
            eprintln!("a puxar {reference}…");
            delonix_image::pull_from_registry(images, reference)
        }
    }
}

/// Comando efetivo (função pura): ENTRYPOINT + (args do utilizador, senão CMD da
/// imagem) — a mesma semântica do Docker/OCI (o `run <cmd>` substitui o CMD, não o
/// ENTRYPOINT).
pub(crate) fn compose_command(entrypoint: &[String], cmd: &[String], user: &[String]) -> Vec<String> {
    let mut v = entrypoint.to_vec();
    if user.is_empty() {
        v.extend(cmd.iter().cloned());
    } else {
        v.extend(user.iter().cloned());
    }
    v
}

/// Como [`compose_command`], mas a partir da config da imagem.
pub(crate) fn effective_command(img: &Image, user: &[String]) -> Vec<String> {
    compose_command(&img.config.entrypoint, &img.config.cmd, user)
}

/// `chown -R <uid>:<uid>` de um rootfs FLAT (rootless): sem isto, os ficheiros
/// pertencem ao uid 0 do host, que fica não-mapeado dentro do user namespace.
/// Delega em `delonix_runtime::lchown_tree` (usa `lchown`, nunca segue symlinks —
/// ver nota de segurança lá; não reimplementar isto localmente com
/// `std::os::unix::fs::chown`, que segue symlinks).
pub(crate) fn chown_tree(path: &Path, uid: u32) -> Result<()> {
    delonix_runtime::lchown_tree(path, uid, uid);
    Ok(())
}

/// Localiza um container pelo prefixo do ID ou pelo nome exato.
pub(crate) fn find(store: &Store, q: &str) -> Result<Container> {
    let all = store.list()?;
    all.into_iter()
        .find(|c| c.id == q || c.id.starts_with(q) || c.name == q)
        .ok_or_else(|| Error::Invalid(format!("container não encontrado: {q}")))
}

/// Prepara o rootfs de um novo container a partir de uma imagem: FLAT (export +
/// chown ao uid do userns) em rootless, ou overlay montado em modo root. Mesma
/// regra usada por `container run` e por `build` (o container "de trabalho").
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
