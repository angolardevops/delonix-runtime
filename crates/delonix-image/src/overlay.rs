//! Mounting the rootfs with **overlay2**: stacks the *layers* (read-only) and
//! adds a write layer per container.

use crate::cas::strip;
use crate::image::{Image, ImageStore};
use delonix_runtime_core::{Error, Result};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use std::path::{Path, PathBuf};

/// Recursive `chown` of a directory (for user namespace support).
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

/// Extracts a *layer* (tar, optionally gzip or zstd) into a directory.
/// Detects the compression by *magic bytes* (gzip `1f 8b`, zstd `28 b5 2f fd`).
fn extract_layer(data: &[u8], dest: &Path) -> Result<()> {
    let is_gzip = data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b;
    let is_zstd =
        data.len() >= 4 && data[0] == 0x28 && data[1] == 0xb5 && data[2] == 0x2f && data[3] == 0xfd;
    let result = if is_gzip {
        let gz = flate2::read::GzDecoder::new(data);
        tar::Archive::new(gz).unpack(dest)
    } else if is_zstd {
        let zd = zstd::stream::read::Decoder::new(data)
            .map_err(|e| Error::Invalid(format!("failed to open zstd: {e}")))?;
        tar::Archive::new(zd).unpack(dest)
    } else {
        tar::Archive::new(data).unpack(dest)
    };
    result.map_err(|e| Error::Invalid(format!("failed to extract layer: {e}")))
}

/// Applies a *layer* to a FLAT destination (not overlay), handling the OCI
/// *whiteouts*: `.wh.<name>` deletes the target; `.wh..wh..opq` empties the directory. This is
/// what makes the result a portable rootfs (e.g. an OCI *bundle* for `runc`).
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
    for entry in ar
        .entries()
        .map_err(|e| Error::Invalid(format!("tar: {e}")))?
    {
        let mut entry = entry.map_err(|e| Error::Invalid(format!("tar entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| Error::Invalid(format!("tar path: {e}")))?
            .into_owned();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if let Some(target) = name.strip_prefix(".wh.") {
            // SECURITY: unlike the extraction branch below, this used to join the RAW
            // tar path (never run through `safe_rel`) straight into `remove_dir_all`/
            // `remove_file` — a `..`-laden whiteout entry (e.g.
            // `../../../../home/<u>/.wh..wh..opq`) deleted files outside `dest`. Reject
            // any whiteout whose path isn't confined (mirrors the extraction branch).
            let Some(safe) = safe_rel(&path) else {
                continue;
            };
            let parent = safe
                .parent()
                .map(|p| dest.join(p))
                .unwrap_or_else(|| dest.to_path_buf());
            // Defense in depth: an EARLIER layer may have planted a symlink at `parent`
            // pointing outside `dest` (the lexical check above only rejects `..` in the
            // whiteout's OWN path, not a symlink an unrelated prior entry created) —
            // canonicalize and confirm we're still inside `dest` before deleting through it.
            let (Ok(canon_parent), Ok(canon_dest)) = (parent.canonicalize(), dest.canonicalize())
            else {
                continue;
            };
            if !canon_parent.starts_with(&canon_dest) {
                continue;
            }
            if target == ".wh..opq" {
                if let Ok(rd) = std::fs::read_dir(&parent) {
                    for e in rd.flatten() {
                        let _ = std::fs::remove_dir_all(e.path())
                            .or_else(|_| std::fs::remove_file(e.path()));
                    }
                }
            } else {
                let victim = parent.join(target);
                let _ = std::fs::remove_dir_all(&victim).or_else(|_| std::fs::remove_file(&victim));
            }
            continue;
        }
        // ROOTLESS: many images have read-only directories (e.g. `/usr/lib64` 0555).
        // `unpack_in` creates the dir with that mode and, without CAP_DAC_OVERRIDE (non-root),
        // cannot write the files inside it → silent PermissionDenied
        // → bash/glibc/coreutils disappear from the rootfs. We ensure the parent dir is
        // writable BEFORE extracting, and that each created dir is writable for the owner
        // (as `Archive::unpack` in bulk does, deferring the dirs' permissions).
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
        let _ = entry.unpack_in(dest); // ignore nodes that require privilege
        if is_dir {
            if let Some(rel) = &safe {
                ensure_owner_writable(&dest.join(rel));
            }
        }
    }
    Ok(())
}

/// "Safe" relative path (no absolute components nor `..`), so we can
/// pre-create the parent dir without risk of escaping `dest`. `None` if unsafe
/// (we let `unpack_in` reject/handle it).
fn safe_rel(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return None, // RootDir, ParentDir, Prefix → unsafe
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Ensures the OWNER write bit on a directory (best-effort), so that the
/// following files/subdirs can be written there in rootless mode. Keeps
/// the read/execute and group/other bits.
fn ensure_owner_writable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = std::fs::metadata(p) {
        if md.is_dir() {
            let mode = md.permissions().mode();
            // A directory needs owner WRITE **and** EXECUTE to
            // create/access entries. `0o555` (r-x, without w) AND `0o644` (rw, without x —
            // e.g. `/etc/containerd` in some images) block the writing of
            // children in rootless → files disappear. We require both (`0o300`).
            if mode & 0o300 != 0o300 {
                let mut perm = md.permissions();
                perm.set_mode(mode | 0o700);
                let _ = std::fs::set_permissions(p, perm);
            }
        }
    }
}

impl ImageStore {
    /// Extracts an image into a FLAT rootfs at `dest` (applies all layers in
    /// order, with *whiteouts*) — the basis of an OCI runtime *bundle* (C1).
    pub fn export_rootfs(&self, image: &Image, dest: &Path) -> Result<()> {
        std::fs::create_dir_all(dest)?;
        for digest in &image.layers {
            let data = self.cas().read(digest)?;
            apply_layer_flat(&data, dest)?;
        }
        Ok(())
    }

    /// Ensures each *layer* is extracted into `layers/<hex>/` (cached). The
    /// extraction is **atomic**: it goes to its own temporary directory and only
    /// then is renamed to the final destination. This way, several `run`s of the SAME
    /// image in parallel do not trample each other writing the same files
    /// (robustness under concurrency — see `tools/stress.sh`).
    fn ensure_layers(&self, image: &Image) -> Result<Vec<PathBuf>> {
        let mut dirs = Vec::new();
        for digest in &image.layers {
            let hex = strip(digest);
            let dir = self.root().join("layers").join(hex);
            let marker = dir.join(".extracted");
            if !marker.exists() {
                let layers_dir = self.root().join("layers");
                std::fs::create_dir_all(&layers_dir)?;
                // temp exclusive to this process (pid + digest).
                let tmp = layers_dir.join(format!(".{hex}.{}.tmp", std::process::id()));
                let _ = std::fs::remove_dir_all(&tmp);
                std::fs::create_dir_all(&tmp)?;
                let data = self.cas().read(digest)?;
                extract_layer(&data, &tmp)?;
                std::fs::write(tmp.join(".extracted"), b"ok")?;
                // publish atomically. If another process already published (the rename
                // fails because the destination exists), we discard our temp.
                if marker.exists() || std::fs::rename(&tmp, &dir).is_err() {
                    let _ = std::fs::remove_dir_all(&tmp);
                }
            }
            dirs.push(dir);
        }
        Ok(dirs)
    }

    /// The base directory of a container in the image store.
    fn container_dir(&self, container_id: &str) -> PathBuf {
        self.root().join("containers").join(container_id)
    }

    /// Mounts the overlay rootfs of a container and returns the `merged` path.
    pub fn mount_rootfs(&self, image: &Image, container_id: &str) -> Result<PathBuf> {
        let lowers = self.ensure_layers(image)?;
        if lowers.is_empty() {
            return Err(Error::Invalid("image has no layers".into()));
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

    /// Name of the file that marks a container directory as OVERLAY-backed and
    /// carries its lower stack (one absolute path per line, highest layer
    /// first — already in `lowerdir=` order).
    ///
    /// It is an on-disk contract on purpose, not a field threaded through
    /// `RunSpec`: the rootless paths re-exec the binary (`--net <custom>` goes
    /// through `nsenter … ip netns exec`, `--pod` likewise), and anything the
    /// mount needs has to survive that boundary. A sibling file next to
    /// `merged/` does; a struct built before the re-exec does not.
    pub const LOWERS_FILE: &'static str = "overlay-lowers";

    /// The extracted layer directories of an image, highest first — the exact
    /// `lowerdir=` order. Extracts and caches them if needed.
    ///
    /// Public for the flat→overlay migration, which needs the lower stack
    /// WITHOUT creating a write layer: `prepare_overlay` would also write the
    /// marker, and that file is the migration's commit point — writing it early
    /// would declare a container migrated before it is.
    pub fn lower_dirs(&self, image: &Image) -> Result<Vec<PathBuf>> {
        let lowers = self.ensure_layers(image)?;
        Ok(lowers.into_iter().rev().collect())
    }

    /// Prepares an overlay rootfs for a container WITHOUT mounting it, and
    /// returns the `merged` path the mount will land on.
    ///
    /// This is the rootless counterpart of [`Self::mount_rootfs`]: there, the
    /// engine is real root and mounts on the host right away; here it cannot —
    /// an unprivileged `mount(2)` on the host is EPERM, and the overlay can only
    /// be mounted from INSIDE the container's user namespace (where the creator
    /// holds full caps). So this call does the part that must happen outside
    /// (extract/cache the layers, create the write layer) and leaves the mount
    /// to `delonix-runtime`'s `setup_rootfs`, which reads [`Self::LOWERS_FILE`].
    ///
    /// What this replaces is a FULL COPY of the image per container
    /// (`export_rootfs`): measured on this host, 21 containers of the same
    /// `kaeso-odoo:16` held 21 physical copies of the same 2.1 GiB tree, every
    /// file at `nlink == 1` — ~39 GiB of byte-identical duplication, and 13 s of
    /// I/O on every `run`. The layers under `layers/<hex>/` were already shared
    /// and already had the right ownership for the container's uid map; nothing
    /// pointed at them.
    pub fn prepare_overlay(&self, image: &Image, container_id: &str) -> Result<PathBuf> {
        let lowers = self.ensure_layers(image)?;
        if lowers.is_empty() {
            return Err(Error::Invalid("image has no layers".into()));
        }
        // A `:` would split the mount option into two lowerdirs and silently
        // point the overlay at a path that does not exist. Our own layer
        // directories are hex digests and cannot contain one, but the state root
        // is user-supplied (`DELONIX_ROOT`), so refuse instead of assuming.
        if let Some(bad) = lowers.iter().find(|p| p.to_string_lossy().contains(':')) {
            return Err(Error::Invalid(format!(
                "state root path contains ':' and cannot back an overlay: {}",
                bad.display()
            )));
        }
        let base = self.container_dir(container_id);
        for d in ["upper", "work", "merged"] {
            std::fs::create_dir_all(base.join(d))?;
        }
        // Highest layer first, which is what `lowerdir=` expects — the same
        // `.rev()` that `mount_rootfs` applies, done ONCE here so the reader
        // never has to know the store's layer order.
        let body = lowers
            .iter()
            .rev()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(base.join(Self::LOWERS_FILE), body)?;
        Ok(base.join("merged"))
    }

    /// Does a (recursive) `chown` of the container's write layer (upper+work)
    /// to `uid:gid`. Needed with a user namespace: the container's root
    /// (mapped to `uid` on the host) needs to own its write layer.
    pub fn chown_writable(&self, container_id: &str, uid: u32, gid: u32) -> Result<()> {
        let base = self.container_dir(container_id);
        for sub in ["upper", "work"] {
            chown_recursive(&base.join(sub), uid, gid)?;
        }
        Ok(())
    }

    /// Unmounts a container's overlay and removes its write layer.
    pub fn unmount_rootfs(&self, container_id: &str) -> Result<()> {
        let base = self.container_dir(container_id);
        let merged = base.join("merged");
        if merged.exists() {
            let _ = umount2(&merged, MntFlags::MNT_DETACH);
        }
        // ROOTLESS OVERLAY: `upper/` is the container's PERSISTENT state — it holds
        // every write the container made, and is exactly what the flat `rootfs/`
        // used to hold. Deleting it on `stop` would make a restart come back with
        // the bare image, silently losing the container's data. Only the mountpoint
        // and the overlay's own bookkeeping directory are scratch.
        //
        // The check is the LOWERS file and not `upper/`: `mount_rootfs` (root mode)
        // also creates an `upper/`, and there the whole directory is scratch —
        // reading the marker keeps the two models apart instead of guessing from a
        // directory both of them happen to make.
        if base.join(Self::LOWERS_FILE).exists() {
            for d in ["merged", "work"] {
                let _ = std::fs::remove_dir_all(base.join(d));
            }
        }
        // ROOTLESS FLAT (legacy): `rootfs/` is the container's PERSISTENT state (reused on
        // restart, preserves the writes — like Docker). On `stop`/cleanup do NOT
        // delete it; clean only the overlay's scratch directories (root model). The
        // definitive destroy is `remove_container_dir` (called by `rm`).
        else if base.join("rootfs").exists() {
            for d in ["merged", "upper", "work"] {
                let _ = std::fs::remove_dir_all(base.join(d));
            }
        } else {
            let _ = std::fs::remove_dir_all(&base);
        }
        Ok(())
    }

    /// Removes the ENTIRE container directory (incl. the flat `rootfs/`). Use in `rm`
    /// (definitive destroy), unlike `unmount_rootfs` (stop, which preserves).
    /// **Reports failure instead of swallowing it.** This used to be a bare
    /// `let _ = remove_dir_all(...)`, and that silence hid a real leak for a
    /// long time: on the custom-network path a removed container left its whole
    /// flat rootfs behind — measured at 39 MiB for a `redis:7-alpine`, ~1.2 GiB
    /// per 30 containers — while `rm` reported success and the record was gone,
    /// so nothing anywhere pointed at the orphan. It is the same shape as the
    /// disk-pressure incident that once taunted a kubelet with 49 orphan
    /// rootfs directories.
    ///
    /// Returns whether the directory is gone. **Says nothing on failure**, on
    /// purpose: the caller has a mapped-userns fallback that usually succeeds
    /// right after (`cmd::container::purge_container_dir`), and a warning
    /// printed here would announce a leak that is about to be cleaned up — the
    /// dishonest reporting this engine treats as its worst class of bug, just
    /// pointing the other way. Only the caller knows the final outcome, so only
    /// the caller speaks.
    pub fn remove_container_dir(&self, container_id: &str) -> bool {
        let dir = self.container_dir(container_id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => true,
            // Already gone is success, not failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    }

    /// Path of a container's directory (`<root>/containers/<id>`). So `rm`
    /// can remove it in a mapped userns (subuid files) via the runtime.
    pub fn container_path(&self, container_id: &str) -> PathBuf {
        self.container_dir(container_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_layer_flat, extract_layer};

    /// Regression: a layer with a READ-ONLY directory (mode 0555, e.g.
    /// `/usr/lib64`) and files inside it. In ROOTLESS, `unpack_in` created the
    /// dir 0555 and then could NOT write the children (silent
    /// PermissionDenied) → bash/glibc disappeared. The fix ensures the dir is writable by the
    /// owner. Assertion independent of the uid: the dir must end up with the write bit
    /// AND the file inside it must exist.
    #[test]
    fn flat_extract_writes_into_readonly_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            // dir "ro/" with mode 0555 (read+execute, no write)
            let mut dh = tar::Header::new_gnu();
            dh.set_entry_type(tar::EntryType::Directory);
            dh.set_size(0);
            dh.set_mode(0o555);
            dh.set_cksum();
            b.append_data(&mut dh, "ro/", std::io::empty()).unwrap();
            // file INSIDE the read-only dir (like glibc in /usr/lib64)
            let content = b"glibc";
            let mut fh = tar::Header::new_gnu();
            fh.set_size(content.len() as u64);
            fh.set_mode(0o644);
            fh.set_cksum();
            b.append_data(&mut fh, "ro/libc.so.6", &content[..])
                .unwrap();
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
        let mode = std::fs::metadata(dir.join("ro"))
            .unwrap()
            .permissions()
            .mode();
        assert!(
            mode & 0o200 != 0,
            "o directório tem de ficar gravável pelo dono (fix)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression B.3 (Kind): a `0o644` directory (write WITHOUT execute — e.g.
    /// `/etc/containerd` in kindest/node) blocks the creation of files inside it
    /// in rootless. The fix must also ensure the EXECUTE bit.
    #[test]
    fn flat_extract_writes_into_writable_but_nonexec_dir() {
        use std::os::unix::fs::PermissionsExt;
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            let mut dh = tar::Header::new_gnu();
            dh.set_entry_type(tar::EntryType::Directory);
            dh.set_size(0);
            dh.set_mode(0o644); // rw-r--r-- (without x) — the /etc/containerd case
            dh.set_cksum();
            b.append_data(&mut dh, "cfgdir/", std::io::empty()).unwrap();
            let content = b"version = 2\n";
            let mut fh = tar::Header::new_gnu();
            fh.set_size(content.len() as u64);
            fh.set_mode(0o644);
            fh.set_cksum();
            b.append_data(&mut fh, "cfgdir/config.toml", &content[..])
                .unwrap();
            b.finish().unwrap();
        }
        let dir = std::env::temp_dir().join(format!("delonix-flat-nox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        apply_layer_flat(&buf, &dir).unwrap();
        assert!(
            dir.join("cfgdir/config.toml").exists(),
            "ficheiro num dir 0644 (sem x) tem de ser extraído (regressão Kind/containerd)"
        );
        let mode = std::fs::metadata(dir.join("cfgdir"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o300, 0o300, "o dir tem de ficar com w+x do dono");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Builds a tar with one file and returns the bytes.
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
        assert_eq!(&zstd_bytes[..4], &[0x28, 0xb5, 0x2f, 0xfd]); // zstd magic

        let dir = std::env::temp_dir().join(format!("delonix-zstd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        extract_layer(&zstd_bytes, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("hello.txt")).unwrap(),
            "camada"
        );

        // the gzip path still works
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
        assert_eq!(
            std::fs::read_to_string(dir.join("hello.txt")).unwrap(),
            "camada"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
