//! Handlers for the **mapped** re-execs (`__rmtree`, `__volsnap`) — the halves
//! that were missing from the engine's own contract.
//!
//! # Why this exists
//!
//! In rootless with subuid, the files a container writes belong to **mapped**
//! uids (e.g. the container's uid 0 → 100000 on the host). The real user cannot
//! delete or read them. The solution (the same as `podman unshare`):
//! `delonix-runtime` forks a child in a user namespace, maps its subuid range
//! with `newuidmap`, and the child — now root IN THAT userns, hence the
//! effective owner of the subuids — re-executes `delonix __rmtree <path>` or
//! `delonix __volsnap <mode> <data> <tarball>`.
//!
//! **The contract was half-implemented in the public repo**: the library
//! (`delonix_runtime::{remove_tree_mapped, reexec_mapped}`) did the re-exec, but
//! the subcommands only existed in `delonix-paas`'s PRIVATE CLI. A user of the
//! public `delonix` caught the child dying with "unrecognized subcommand
//! '__rmtree'" (rc=2) — and since `remove_tree_mapped` did not even look at the
//! exit status, the tree was left unremoved **silently**. Verified running:
//! `delonix __rmtree /x` → rc=2.
//!
//! They are not public subcommands: `main` intercepts them before clap (like
//! `netns holder`), and the user never invokes them by hand.

use std::path::Path;

use delonix_runtime_core::{Error, Result};

fn io_err(context: &'static str) -> impl Fn(std::io::Error) -> Error {
    move |e: std::io::Error| Error::Runtime {
        context,
        message: e.to_string(),
    }
}

/// `__rmtree <path>` — deletes an entire tree, including subuid files.
///
/// We already run as root in a mapped userns (the parent used `newuidmap`), so a
/// normal `remove_dir_all` is enough: inside this userns we own the subuids.
pub fn rmtree(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path).or_else(|e| {
        // Already not existing is success — the goal is "not being there".
        if e.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(io_err("__rmtree")(e))
        }
    })
}

/// `__volsnap create <data> <tarball>` — tar.gz of a volume's `_data`.
///
/// Writes to a `.tmp` and does a `rename`: a crash midway does not leave a
/// truncated snapshot pretending to be good.
pub fn volsnap_create(data: &Path, tarball: &Path) -> Result<()> {
    if let Some(dir) = tarball.parent() {
        std::fs::create_dir_all(dir).map_err(io_err("volume snapshot"))?;
    }
    let tmp = tarball.with_extension("tar.gz.tmp");
    let f = std::fs::File::create(&tmp).map_err(io_err("volume snapshot"))?;
    let enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
    let mut b = tar::Builder::new(enc);
    b.follow_symlinks(false); // symlinks go in as symlinks, not the target
    b.append_dir_all(".", data)
        .map_err(io_err("volume snapshot"))?;
    b.into_inner()
        .and_then(|enc| enc.finish())
        .map_err(io_err("volume snapshot"))?;
    std::fs::rename(&tmp, tarball).map_err(io_err("volume snapshot"))?;
    Ok(())
}

/// `__volsnap restore <data> <tarball>` — restores `_data` from the tar.gz.
///
/// Clears the CONTENTS and not `_data` itself (keeps the inode/mountpoint: it
/// may be mounted in a running container). Owners and permissions preserved — in
/// the mapped userns the subuid chown works.
pub fn volsnap_restore(data: &Path, tarball: &Path) -> Result<()> {
    let f = std::fs::File::open(tarball).map_err(io_err("volume restore"))?;
    for e in std::fs::read_dir(data).map_err(io_err("volume restore"))? {
        let p = e.map_err(io_err("volume restore"))?.path();
        if p.is_dir() && !p.is_symlink() {
            std::fs::remove_dir_all(&p).map_err(io_err("volume restore"))?;
        } else {
            std::fs::remove_file(&p).map_err(io_err("volume restore"))?;
        }
    }
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(f));
    a.set_preserve_permissions(true);
    a.set_preserve_ownerships(true);
    a.set_overwrite(true);
    a.unpack(data).map_err(io_err("volume restore"))?;
    Ok(())
}

/// `__buildtar <rootfs> <out>` — packs a FLAT rootfs (rootless build) into an
/// UNCOMPRESSED tar, run INSIDE the mapped userns.
///
/// Why mapped: a `RUN` with `apt-get install` (dpkg) leaves subuid files with
/// restrictive modes (`/var/cache/ldconfig/aux-cache` 0600, `.../partial` dirs
/// 0700). `commit_flat_rootfs` packing as the REAL user cannot read them →
/// `Permission denied` and the whole build fails at the end (after every RUN
/// passes — the worst place to fail). Here we are root in the userns (effective
/// owners of the subuids), so we read everything; and the tar records uid 0, not
/// the subuid number — more correct for an OCI layer.
///
/// UNCOMPRESSED tar on purpose: `commit_flat_rootfs_from_tar` uses this tar's
/// digest as the `diff_id` (OCI requires the digest of the UNcompressed tar).
/// `out` is left world-readable (0644) so the parent — which does not own the
/// subuid — can read it back.
pub fn buildtar(rootfs: &Path, out: &Path) -> Result<()> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(io_err("build tar"))?;
    }
    let f = std::fs::File::create(out).map_err(io_err("build tar"))?;
    let mut b = tar::Builder::new(f);
    b.follow_symlinks(false);
    b.append_dir_all(".", rootfs).map_err(io_err("build tar"))?;
    b.finish().map_err(io_err("build tar"))?;
    Ok(())
}

/// Dispatches `__volsnap <mode> <data> <tarball>`.
pub fn volsnap(mode: &str, data: &Path, tarball: &Path) -> Result<()> {
    match mode {
        "create" => volsnap_create(data, tarball),
        "restore" => volsnap_restore(data, tarball),
        other => Err(Error::Invalid(format!(
            "__volsnap: modo desconhecido '{other}' (create|restore)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(nome: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("delonix-mapped-{nome}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rmtree_apaga_a_arvore() {
        let d = tmpdir("rm");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/b/f"), b"x").unwrap();
        rmtree(&d).unwrap();
        assert!(!d.exists());
    }

    #[test]
    fn rmtree_e_idempotente() {
        // The goal is "not being there" — deleting what no longer exists is
        // success, otherwise a repeated `container rm` would fail for no reason.
        let d = tmpdir("rm-idem");
        std::fs::remove_dir_all(&d).unwrap();
        rmtree(&d).unwrap();
    }

    #[test]
    fn volsnap_round_trip_preserva_conteudo() {
        let base = tmpdir("snap");
        let data = base.join("_data");
        std::fs::create_dir_all(data.join("sub")).unwrap();
        std::fs::write(data.join("sub/ficheiro"), b"conteudo").unwrap();
        let tar = base.join("_snapshots/s1.tar.gz");

        volsnap_create(&data, &tar).unwrap();
        assert!(tar.exists(), "o snapshot devia existir");
        // No .tmp left behind.
        assert!(!tar.with_extension("tar.gz.tmp").exists());

        // Touch _data and restore.
        std::fs::write(data.join("sub/ficheiro"), b"estragado").unwrap();
        std::fs::write(data.join("intruso"), b"a apagar").unwrap();
        volsnap_restore(&data, &tar).unwrap();

        assert_eq!(
            std::fs::read(data.join("sub/ficheiro")).unwrap(),
            b"conteudo"
        );
        assert!(
            !data.join("intruso").exists(),
            "o restore tem de limpar o que não estava no snapshot"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn volsnap_restore_mantem_o_proprio_data() {
        // `_data` may be mounted in a live container: the contents are cleared,
        // never the directory (otherwise the mount would point at a dead inode).
        let base = tmpdir("snap-inode");
        let data = base.join("_data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("f"), b"v1").unwrap();
        let tar = base.join("s.tar.gz");
        volsnap_create(&data, &tar).unwrap();
        let ino_antes = std::fs::metadata(&data).unwrap().rt_ino();
        volsnap_restore(&data, &tar).unwrap();
        assert_eq!(
            ino_antes,
            std::fs::metadata(&data).unwrap().rt_ino(),
            "o inode do _data mudou"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn buildtar_empacota_o_rootfs() {
        let base = tmpdir("buildtar");
        let rootfs = base.join("rootfs");
        std::fs::create_dir_all(rootfs.join("etc")).unwrap();
        std::fs::write(rootfs.join("etc/hostname"), b"delonix").unwrap();
        std::fs::write(rootfs.join("app"), b"bin").unwrap();
        let out = base.join("layer.tar");

        buildtar(&rootfs, &out).unwrap();
        assert!(out.exists(), "o tar devia existir");

        // The tar contains the rootfs entries (verify by re-reading).
        let mut a = tar::Archive::new(std::fs::File::open(&out).unwrap());
        let mut nomes: Vec<String> = a
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        nomes.sort();
        assert!(
            nomes.iter().any(|n| n.ends_with("etc/hostname")),
            "faltou etc/hostname: {nomes:?}"
        );
        assert!(
            nomes.iter().any(|n| n.ends_with("app")),
            "faltou app: {nomes:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn volsnap_modo_invalido_e_erro_claro() {
        let d = tmpdir("snap-modo");
        let err = volsnap("destruir", &d, &d.join("t.tar.gz")).unwrap_err();
        assert!(
            format!("{err}").contains("modo desconhecido"),
            "erro pouco claro: {err}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    trait RtIno {
        fn rt_ino(&self) -> u64;
    }
    impl RtIno for std::fs::Metadata {
        fn rt_ino(&self) -> u64 {
            use std::os::unix::fs::MetadataExt;
            self.ino()
        }
    }
}
