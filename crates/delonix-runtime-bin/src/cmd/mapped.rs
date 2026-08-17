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

/// `__duusage <path> <outfile>` — measures a tree from INSIDE the mapped userns
/// and writes `<bytes>` to `outfile`.
///
/// Why this exists at all: measuring a volume as the real user is not just
/// imprecise in rootless, it is wrong in the common case. A container in a mapped
/// userns writes `_data` as a subuid, and every managed database `chmod 700`s its
/// data dir — so `read_dir` returns EACCES and the walk reported **0 bytes** for
/// volumes holding real data. That made `volumes describe` print `Usage: 0 B`
/// over 20 MiB, `system df` print `volumes 0 B` on a filling disk, and the
/// rootless quota monitor — the only enforcement rootless has — never fire.
/// Here we are root in the userns, hence the effective owner of the subuids, so
/// the walk sees everything, exactly like `__volsnap` already does for the tar.
///
/// The count goes through a FILE and not stdout because `reexec_mapped` only
/// reports the child's exit status; `outfile` is left world-readable (0644) so
/// the parent — which does not own the subuid — can read it back, the same
/// arrangement `__buildtar` already uses.
///
/// **The line carries `<bytes> <unreadable>`, not just the bytes.** Owning the
/// subuids removes the EACCES this exists for, and it removes a lot more than
/// that — root in the userns holds `CAP_DAC_OVERRIDE` there, so it reads even a
/// `chmod 000` directory owned by a mapped uid (measured: `du` refuses that tree
/// and this walk reports it correctly and in full). What it does NOT cover is a
/// subtree outside the mapping, or a mount this process cannot enter. In those
/// cases `measure` returns a non-zero `unreadable` — and reporting only the byte
/// count made the parent reconstruct `unreadable: 0`, an INCOMPLETE walk
/// asserting it was complete. That is the exact bug the `Usage` type was
/// introduced to kill, reintroduced by the mechanism added to fix it.
///
/// Honest about the evidence: this is fixed from reading the code and covered by
/// `parse_duusage`'s unit test. It was NOT reproduced live — every tree that can
/// be built on this host is inside the mapping, which is precisely why the
/// mapped walk sees all of it.
pub fn duusage(path: &Path, out: &Path) -> Result<()> {
    let u = delonix_volume::measure(path);
    let mut line = format!("{} {}", u.bytes, u.unreadable);
    line.push('\n');
    std::fs::write(out, line.as_bytes()).map_err(io_err("__duusage"))?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o644));
    Ok(())
}

/// Reads the whole snapshot through the gzip+tar decoders WITHOUT writing
/// anything, so a corrupt archive is caught while the live data is still there.
///
/// Deliberately walks every entry and drains each one: a truncated gzip stream
/// only errors when the reader reaches the end, so checking the header alone (or
/// just listing names) would pass an archive that cannot actually be extracted.
fn verify_snapshot(tarball: &Path) -> Result<()> {
    let f = std::fs::File::open(tarball).map_err(io_err("volume restore"))?;
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(f));
    let entries = a.entries().map_err(io_err("volume restore"))?;
    let mut sink = std::io::sink();
    for e in entries {
        let mut e = e.map_err(io_err("volume restore"))?;
        std::io::copy(&mut e, &mut sink).map_err(io_err("volume restore"))?;
    }
    Ok(())
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
    // VALIDATE BEFORE DESTROYING. This used to clear `_data` and only then start
    // unpacking, so a truncated/corrupt archive (an interrupted copy, a snapshot
    // taken when the disk filled, a hand-moved file) destroyed the live data and
    // had nothing to put back — no rollback, total loss. A full decode pass first
    // costs one extra read of a file we are about to read anyway, and turns that
    // into a clean refusal with the data still intact.
    verify_snapshot(tarball)?;
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

/// `__ovlhold <container-dir>` — mounts a STOPPED container's overlay inside a
/// throwaway mount namespace, announces the mountpoint is ready, and then sleeps
/// until the parent kills it.
///
/// # Why a live process instead of a copy
///
/// A stopped overlay container has no readable tree from the outside: `merged/`
/// is an empty directory until something mounts the overlay, and an unprivileged
/// `mount(2)` on the host is EPERM. The obvious fixes are both bad — packing the
/// whole tree to a temp directory costs a full copy of exactly the data this
/// engine stopped duplicating, and re-implementing overlayfs's merge in
/// userspace (upper over lowers, whiteouts and all) is a second implementation of
/// the kernel's semantics that would drift from it.
///
/// So: hold the namespace and let the caller in through `/proc/<pid>/root`, the
/// SAME door a running container already uses (`container_fs_root`). Nothing
/// downstream changes — `cp` and `commit` keep reading a plain path.
///
/// Runs under `reexec_mapped`, so we are root in a user namespace with the
/// subuids mapped: we can both mount and read files the container wrote as a
/// non-root uid. The extra `unshare(CLONE_NEWNS)` is ours to do — being root in
/// the userns is what makes it permitted — and it is what keeps the mount from
/// ever appearing in the host's mount tree.
///
/// Prints `ready` on stdout and flushes BEFORE sleeping: that byte is the
/// parent's only proof the mount succeeded. Without it the parent would race the
/// mount and read an empty directory — the silent-empty-answer failure this
/// whole path exists to avoid.
pub fn ovlhold(dir: &Path) -> Result<()> {
    use std::io::Write;
    // Our own mount namespace, with propagation severed: the overlay must never
    // show up on the host, and must disappear with this process.
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        return Err(io_err("__ovlhold unshare")(std::io::Error::last_os_error()));
    }
    // `libc` and not `nix`: this crate does not depend on `nix`, and a new
    // dependency in a container runtime is not worth one `mount(2)` call.
    // SAFETY: three well-formed C strings and a flag word; `/` is always a valid
    // target, and we are root in our own user namespace with a private mount ns.
    let slash = c"/";
    let rc = unsafe {
        libc::mount(
            std::ptr::null(),
            slash.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(io_err("__ovlhold private root")(
            std::io::Error::last_os_error(),
        ));
    }
    let merged = dir.join("merged");
    delonix_runtime::mount_overlay_if_marked(&merged.to_string_lossy()).map_err(|e| {
        Error::Runtime {
            context: "__ovlhold mount overlay",
            message: e.to_string(),
        }
    })?;
    println!("ready");
    std::io::stdout().flush().map_err(io_err("__ovlhold"))?;
    // The parent reads `/proc/<pid>/root/...` for as long as it needs and then
    // signals us. `pause` returns only on a signal, so this costs nothing while
    // it waits.
    loop {
        unsafe { libc::pause() };
    }
}

/// Dispatches `__volsnap <mode> <data> <tarball>`.
pub fn volsnap(mode: &str, data: &Path, tarball: &Path) -> Result<()> {
    match mode {
        "create" => volsnap_create(data, tarball),
        "restore" => volsnap_restore(data, tarball),
        other => Err(Error::Invalid(super::po::tf(
            "__volsnap: unknown mode '{other}' (create|restore)",
            &[("other", other)],
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
            format!("{err}").contains("unknown mode"),
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
