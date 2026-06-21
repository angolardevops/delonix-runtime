//! Montagem do rootfs com **overlay2**: empilha os *layers* (read-only) e
//! acrescenta uma camada de escrita por container.

use crate::cas::strip;
use crate::image::{Image, ImageStore};
use delonix_core::{Error, Result};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use std::path::{Path, PathBuf};

/// `chown` recursivo de um directório (para suporte a user namespace).
fn chown_recursive(path: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::fs::chown;
    let _ = chown(path, Some(uid), Some(gid));
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            chown_recursive(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

/// Extrai um *layer* (tar, opcionalmente gzip ou zstd) para um directório.
/// Detecta a compressão pelos *magic bytes* (gzip `1f 8b`, zstd `28 b5 2f fd`).
fn extract_layer(data: &[u8], dest: &Path) -> Result<()> {
    let is_gzip = data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b;
    let is_zstd =
        data.len() >= 4 && data[0] == 0x28 && data[1] == 0xb5 && data[2] == 0x2f && data[3] == 0xfd;
    let result = if is_gzip {
        let gz = flate2::read::GzDecoder::new(data);
        tar::Archive::new(gz).unpack(dest)
    } else if is_zstd {
        let zd = zstd::stream::read::Decoder::new(data)
            .map_err(|e| Error::Invalid(format!("falha a abrir zstd: {e}")))?;
        tar::Archive::new(zd).unpack(dest)
    } else {
        tar::Archive::new(data).unpack(dest)
    };
    result.map_err(|e| Error::Invalid(format!("falha a extrair layer: {e}")))
}

/// Aplica um *layer* a um destino FLAT (não overlay), tratando os *whiteouts*
/// do OCI: `.wh.<nome>` apaga o alvo; `.wh..wh..opq` esvazia o directório. É o
/// que torna o resultado num rootfs portável (ex.: um *bundle* OCI p/ o `runc`).
fn apply_layer_flat(data: &[u8], dest: &Path) -> Result<()> {
    let reader: Box<dyn std::io::Read> = if data.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(data))
    } else if data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Box::new(
            zstd::stream::read::Decoder::new(data)
                .map_err(|e| Error::Invalid(format!("zstd: {e}")))?,
        )
    } else {
        Box::new(data)
    };
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    ar.set_overwrite(true);
    for entry in ar.entries().map_err(|e| Error::Invalid(format!("tar: {e}")))? {
        let mut entry = entry.map_err(|e| Error::Invalid(format!("tar entry: {e}")))?;
        let path = entry.path().map_err(|e| Error::Invalid(format!("tar path: {e}")))?.into_owned();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if let Some(target) = name.strip_prefix(".wh.") {
            let parent = path.parent().map(|p| dest.join(p)).unwrap_or_else(|| dest.to_path_buf());
            if target == ".wh..opq" {
                if let Ok(rd) = std::fs::read_dir(&parent) {
                    for e in rd.flatten() {
                        let _ = std::fs::remove_dir_all(e.path()).or_else(|_| std::fs::remove_file(e.path()));
                    }
                }
            } else {
                let victim = parent.join(target);
                let _ = std::fs::remove_dir_all(&victim).or_else(|_| std::fs::remove_file(&victim));
            }
            continue;
        }
        // ROOTLESS: muitas imagens têm directórios read-only (ex. `/usr/lib64` 0555).
        // O `unpack_in` cria o dir com esse modo e, sem CAP_DAC_OVERRIDE (não-root),
        // não consegue escrever os ficheiros lá dentro → PermissionDenied silencioso
        // → bash/glibc/coreutils desaparecem do rootfs. Garantimos que o dir-pai está
        // gravável ANTES de extrair, e que cada dir criado fica gravável p/ o dono
        // (como o `Archive::unpack` em bloco faz, deferindo as permissões dos dirs).
        let safe = safe_rel(&path);
        if let Some(rel) = &safe {
            if let Some(parent) = rel.parent() {
                if parent.as_os_str().is_empty() {
                } else {
                    let pj = dest.join(parent);
                    let _ = std::fs::create_dir_all(&pj);
                    ensure_owner_writable(&pj);
                }
            }
        }
        let is_dir = entry.header().entry_type().is_dir();
        let _ = entry.unpack_in(dest); // ignora nós que precisam de privilégio
        if is_dir {
            if let Some(rel) = &safe {
                ensure_owner_writable(&dest.join(rel));
            }
        }
    }
    Ok(())
}

/// Caminho relativo "seguro" (sem componentes absolutos nem `..`), para podermos
/// pré-criar o dir-pai sem risco de escapar do `dest`. `None` se for inseguro
/// (deixamos o `unpack_in` rejeitar/tratar).
fn safe_rel(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return None, // RootDir, ParentDir, Prefix → inseguro
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Garante o bit de escrita do DONO num directório (best-effort), para que os
/// ficheiros/subdirs seguintes possam ser lá escritos em modo rootless. Mantém
/// os bits de leitura/execução e de grupo/outros.
fn ensure_owner_writable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = std::fs::metadata(p) {
        if md.is_dir() {
            let mode = md.permissions().mode();
            if mode & 0o200 == 0 {
                let mut perm = md.permissions();
                perm.set_mode(mode | 0o700);
                let _ = std::fs::set_permissions(p, perm);
            }
        }
    }
}

impl ImageStore {
    /// Extrai uma imagem para um rootfs FLAT em `dest` (aplica todos os layers por
    /// ordem, com *whiteouts*) — base de um *bundle* OCI runtime (C1).
    pub fn export_rootfs(&self, image: &Image, dest: &Path) -> Result<()> {
        std::fs::create_dir_all(dest)?;
        for digest in &image.layers {
            let data = self.cas().read(digest)?;
            apply_layer_flat(&data, dest)?;
        }
        Ok(())
    }

    /// Garante que cada *layer* está extraído em `layers/<hex>/` (cacheado). A
    /// extracção é **atómica**: vai para um directório temporário próprio e só
    /// depois é renomeada para o destino final. Assim, vários `run` da MESMA
    /// imagem em paralelo não se atropelam a escrever os mesmos ficheiros
    /// (robustez sob concorrência — ver `tools/stress.sh`).
    fn ensure_layers(&self, image: &Image) -> Result<Vec<PathBuf>> {
        let mut dirs = Vec::new();
        for digest in &image.layers {
            let hex = strip(digest);
            let dir = self.root().join("layers").join(hex);
            let marker = dir.join(".extracted");
            if !marker.exists() {
                let layers_dir = self.root().join("layers");
                std::fs::create_dir_all(&layers_dir)?;
                // temp exclusivo deste processo (pid + digest).
                let tmp = layers_dir.join(format!(".{hex}.{}.tmp", std::process::id()));
                let _ = std::fs::remove_dir_all(&tmp);
                std::fs::create_dir_all(&tmp)?;
                let data = self.cas().read(digest)?;
                extract_layer(&data, &tmp)?;
                std::fs::write(tmp.join(".extracted"), b"ok")?;
                // publica atomicamente. Se outro processo já publicou (o rename
                // falha porque o destino existe), descartamos o nosso temp.
                if marker.exists() || std::fs::rename(&tmp, &dir).is_err() {
                    let _ = std::fs::remove_dir_all(&tmp);
                }
            }
            dirs.push(dir);
        }
        Ok(dirs)
    }

    /// O directório base de um container no armazém de imagens.
    fn container_dir(&self, container_id: &str) -> PathBuf {
        self.root().join("containers").join(container_id)
    }

    /// Monta o rootfs overlay de um container e devolve o caminho `merged`.
    pub fn mount_rootfs(&self, image: &Image, container_id: &str) -> Result<PathBuf> {
        let lowers = self.ensure_layers(image)?;
        if lowers.is_empty() {
            return Err(Error::Invalid("imagem sem layers".into()));
        }
        let base = self.container_dir(container_id);
        let upper = base.join("upper");
        let work = base.join("work");
        let merged = base.join("merged");
        for d in [&upper, &work, &merged] {
            std::fs::create_dir_all(d)?;
        }

        let lowerdir = lowers
            .iter()
            .rev()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let opts = format!(
            "lowerdir={lowerdir},upperdir={},workdir={}",
            upper.display(),
            work.display()
        );

        mount(
            Some("overlay"),
            &merged,
            Some("overlay"),
            MsFlags::empty(),
            Some(opts.as_str()),
        )
        .map_err(|e| Error::Runtime {
            context: "mount overlay",
            message: e.to_string(),
        })?;

        Ok(merged)
    }

    /// Faz `chown` (recursivo) da camada de escrita do container (upper+work)
    /// para `uid:gid`. Necessário com user namespace: o root do container
    /// (mapeado para `uid` no host) precisa de ser dono da sua camada de escrita.
    pub fn chown_writable(&self, container_id: &str, uid: u32, gid: u32) -> Result<()> {
        let base = self.container_dir(container_id);
        for sub in ["upper", "work"] {
            chown_recursive(&base.join(sub), uid, gid)?;
        }
        Ok(())
    }

    /// Desmonta o overlay de um container e remove a sua camada de escrita.
    pub fn unmount_rootfs(&self, container_id: &str) -> Result<()> {
        let base = self.container_dir(container_id);
        let merged = base.join("merged");
        if merged.exists() {
            let _ = umount2(&merged, MntFlags::MNT_DETACH);
        }
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_layer_flat, extract_layer};

    /// Regressão: uma camada com um directório READ-ONLY (modo 0555, p.ex.
    /// `/usr/lib64`) e ficheiros lá dentro. Em ROOTLESS, o `unpack_in` criava o
    /// dir 0555 e depois NÃO conseguia escrever os filhos (PermissionDenied
    /// silencioso) → bash/glibc desapareciam. O fix garante o dir gravável pelo
    /// dono. Asserção independente do uid: o dir tem de ficar com o bit de escrita
    /// E o ficheiro lá dentro tem de existir.
    #[test]
    fn flat_extract_writes_into_readonly_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            // dir "ro/" com modo 0555 (read+execute, sem escrita)
            let mut dh = tar::Header::new_gnu();
            dh.set_entry_type(tar::EntryType::Directory);
            dh.set_size(0);
            dh.set_mode(0o555);
            dh.set_cksum();
            b.append_data(&mut dh, "ro/", std::io::empty()).unwrap();
            // ficheiro DENTRO do dir read-only (como glibc em /usr/lib64)
            let content = b"glibc";
            let mut fh = tar::Header::new_gnu();
            fh.set_size(content.len() as u64);
            fh.set_mode(0o644);
            fh.set_cksum();
            b.append_data(&mut fh, "ro/libc.so.6", &content[..]).unwrap();
            b.finish().unwrap();
        }
        let dir = std::env::temp_dir().join(format!("delonix-flat-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        apply_layer_flat(&buf, &dir).unwrap();
        assert!(
            dir.join("ro/libc.so.6").exists(),
            "ficheiro dentro de directório read-only tem de ser extraído (bug rootless)"
        );
        let mode = std::fs::metadata(dir.join("ro")).unwrap().permissions().mode();
        assert!(mode & 0o200 != 0, "o directório tem de ficar gravável pelo dono (fix)");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Constrói um tar com um ficheiro e devolve os bytes.
    fn tar_with_file(name: &str, content: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            b.append_data(&mut header, name, content).unwrap();
            b.finish().unwrap();
        }
        buf
    }

    #[test]
    fn extracts_zstd_and_gzip_layers() {
        let tar = tar_with_file("hello.txt", b"camada");
        let zstd_bytes = zstd::encode_all(&tar[..], 0).unwrap();
        assert_eq!(&zstd_bytes[..4], &[0x28, 0xb5, 0x2f, 0xfd]); // magic zstd

        let dir = std::env::temp_dir().join(format!("delonix-zstd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        extract_layer(&zstd_bytes, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("hello.txt")).unwrap(), "camada");

        // o caminho gzip continua a funcionar
        let mut gz = Vec::new();
        {
            use std::io::Write;
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            enc.write_all(&tar).unwrap();
            enc.finish().unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        extract_layer(&gz, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("hello.txt")).unwrap(), "camada");
        std::fs::remove_dir_all(&dir).ok();
    }
}
