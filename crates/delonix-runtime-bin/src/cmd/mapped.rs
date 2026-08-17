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

/// Creates an overlayfs **whiteout** — a character device 0:0, which is how
/// overlayfs records "this path was deleted" in the upper layer.
///
/// Works rootless: measured inside `unshare --user --map-root-user --mount`,
/// `mknod c 0 0` succeeds and the mounted overlay honours it (the file is gone
/// from `merged`). No `trusted.overlay.*` xattr needed — that one WOULD have
/// been a problem, since setting it wants CAP_SYS_ADMIN over the filesystem.
/// This is the whole reason the migration has to run inside the mapped userns.
fn mknod_whiteout(p: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes())
        .map_err(|_| Error::Invalid(format!("path with NUL: {}", p.display())))?;
    // SAFETY: a valid C string and a fixed device number; we are root in the
    // mapped user namespace, so CAP_MKNOD is held.
    let rc = unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR, libc::makedev(0, 0)) };
    if rc != 0 {
        return Err(io_err("whiteout")(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Is this flat rootfs a plausible WHOLE tree, or a truncated one?
///
/// # The bug this exists to close, measured in production
///
/// `whiteout_missing` reads "in the lower, absent from the upper" as "the
/// container deleted it", and for a file that is right. For a TOP-LEVEL system
/// directory it is not: no container deletes `/usr`, `/etc` and `/bin`. When the
/// flat tree is empty or truncated, every one of those absences becomes a
/// whiteout and the result is a container that is permanently empty — the rootfs
/// destroyed by the very step meant to shrink it.
///
/// Measured: a `postgres:15-alpine` whose `rootfs/` had been emptied migrated
/// into an upper holding **12 whiteouts and nothing else**, and the next start
/// died on `could not write 'etc/hostname': Not a directory` — `etc` was by then
/// a character device. Nothing recovers that tree; it has to be recreated.
///
/// So the guard is at the TOP LEVEL only, where absence is implausible, and
/// deeper whiteouts keep working (a container purging `/etc/motd` is ordinary,
/// and that deletion must survive — it is the whole reason whiteouts are
/// written). Refusing costs exactly the disk the migration would have saved, and
/// the container goes on running flat. Destroying it costs the container.
///
/// It is the same class this repo already catalogues under «X is not Y»: an
/// empty directory is not "the container deleted everything" — here it is far
/// more likely to be a tree that was never complete.
fn flat_looks_complete(lowers: &[std::path::PathBuf], rootfs: &Path) -> bool {
    for low in lowers {
        let Ok(entries) = std::fs::read_dir(low) else {
            continue; // unreadable lower proves nothing either way
        };
        for e in entries.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue; // only top-level DIRECTORIES carry this signal
            }
            if rootfs.join(e.file_name()).symlink_metadata().is_err() {
                return false;
            }
        }
    }
    true
}

/// For every path present in a LOWER layer and absent from the upper, records a
/// whiteout.
///
/// **This is a correctness requirement, not an optimisation.** A flat rootfs is
/// the image plus the container's writes, already merged — so a file the
/// container DELETED is simply absent. Turn that tree into an upper layer
/// without whiteouts and the overlay happily serves the file back from the
/// lower: a deletion silently undone, which for something like a purged config
/// or a rotated secret is worse than the disk it saves.
///
/// Does not descend past a missing path: once the directory itself is whited
/// out, nothing under it can show through.
fn whiteout_missing(low: &Path, upper: &Path, rel: &Path) -> Result<()> {
    let dir = low.join(rel);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        // An unreadable lower directory is not fatal: the whiteouts we fail to
        // write only cost correctness for paths we could not see anyway, and
        // the caller reverts on error. Skipping keeps a partial read from
        // aborting a migration that is otherwise fine.
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let r = rel.join(&name);
        let up = upper.join(&r);
        match up.symlink_metadata() {
            // Absent from the upper → the container deleted it. Mark and stop.
            Err(_) => mknod_whiteout(&up)?,
            Ok(um) => {
                let is_dir_both =
                    entry.file_type().map(|t| t.is_dir()).unwrap_or(false) && um.is_dir();
                if is_dir_both {
                    whiteout_missing(low, upper, &r)?;
                }
            }
        }
    }
    Ok(())
}

/// Two paths hold the same thing: same type, same mode, same owner, and the same
/// bytes (or the same symlink target).
///
/// Deliberately strict. This decides what gets DELETED from the upper, so a
/// false "equal" loses the container's change — a file whose contents match but
/// whose mode the container chmod'ed is NOT equal.
fn same_entry(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (ma, mb) = match (a.symlink_metadata(), b.symlink_metadata()) {
        (Ok(x), Ok(y)) => (x, y),
        _ => return false,
    };
    if ma.file_type() != mb.file_type() || ma.mode() != mb.mode() {
        return false;
    }
    if ma.uid() != mb.uid() || ma.gid() != mb.gid() {
        return false;
    }
    if ma.file_type().is_symlink() {
        return match (std::fs::read_link(a), std::fs::read_link(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        };
    }
    if !ma.file_type().is_file() {
        return false; // only regular files and symlinks are pruned
    }
    if ma.len() != mb.len() {
        return false;
    }
    same_bytes(a, b).unwrap_or(false)
}

/// Byte comparison in chunks — these are whole container images, so nothing is
/// read into memory at once. Any I/O error reads as "not equal", which keeps the
/// file in the upper: the safe direction.
fn same_bytes(a: &Path, b: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    let (mut ba, mut bb) = ([0u8; 64 * 1024], [0u8; 64 * 1024]);
    loop {
        let na = fa.read(&mut ba)?;
        let nb = fb.read(&mut bb)?;
        if na != nb {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
        if ba[..na] != bb[..nb] {
            return Ok(false);
        }
    }
}

/// Removes from the upper everything that is byte-for-byte what the lower
/// already provides. Returns the bytes freed.
///
/// This is the step that actually saves the disk, and it is the ONLY step that
/// may fail without consequence — it runs after the migration has committed, and
/// a file it declines to remove costs space, never data. That asymmetry is the
/// whole reason the migration is written as "delete what is redundant" rather
/// than the seemingly equivalent "copy across what differs": the latter loses
/// data when the comparison is wrong, this one only loses savings.
fn prune_identical(low: &Path, upper: &Path, rel: &Path) -> u64 {
    let mut freed = 0u64;
    let entries = match std::fs::read_dir(upper.join(rel)) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let r = rel.join(&name);
        let (lp, up) = (low.join(&r), upper.join(&r));
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            if lp.is_dir() {
                freed += prune_identical(low, upper, &r);
                // An emptied directory that the lower also provides, with the
                // same metadata, shows through identically — so it can go. A
                // non-empty one must stay: it is the parent of what remains.
                if same_dir_meta(&lp, &up) {
                    let _ = std::fs::remove_dir(&up); // fails if not empty: fine
                }
            }
            continue;
        }
        if same_entry(&lp, &up) {
            let sz = up.symlink_metadata().map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(&up).is_ok() {
                freed += sz;
            }
        }
    }
    freed
}

/// Same mode and owner on two directories (contents compared separately).
fn same_dir_meta(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (a.symlink_metadata(), b.symlink_metadata()) {
        (Ok(x), Ok(y)) => {
            x.is_dir()
                && y.is_dir()
                && x.mode() == y.mode()
                && x.uid() == y.uid()
                && x.gid() == y.gid()
        }
        _ => false,
    }
}

/// `__ovlmigrate <container-dir>` — converts a legacy FLAT rootfs into a shared
/// overlay, in place, inside the mapped userns.
///
/// Reads the lower stack from `overlay-lowers.pending`, which the parent wrote
/// after making sure the image's layers are extracted. That file is also the
/// COMMIT POINT: renaming it to `overlay-lowers` is the single step that turns
/// this container from flat into overlay, and everything before it is
/// reversible.
///
/// # The order is correctness, not taste
///
/// 1. `rename(rootfs → upper)` — atomic, and the tree is unchanged.
/// 2. **whiteouts** for everything the lower has and the upper does not. Skip
///    this and a file the container deleted comes back from the lower.
/// 3. `rename(pending → overlay-lowers)` — commit. Only now is it an overlay.
/// 4. prune the redundant — pure saving, after the commit, may fail freely.
///
/// A failure before step 3 renames the upper back and the container stays flat.
/// **The migration must never be able to stop a container from starting**: the
/// caller treats any error here as "carry on flat", which is exactly what the
/// tree still is.
pub fn ovlmigrate(dir: &Path) -> Result<()> {
    let pending = dir.join("overlay-lowers.pending");
    let rootfs = dir.join("rootfs");
    let upper = dir.join("upper");
    if !rootfs.exists() || dir.join("overlay-lowers").exists() {
        let _ = std::fs::remove_file(&pending);
        return Ok(()); // nothing to migrate, or already migrated — idempotent
    }
    let body = std::fs::read_to_string(&pending).map_err(io_err("__ovlmigrate lowers"))?;
    let lowers: Vec<std::path::PathBuf> = body
        .lines()
        .filter(|l| !l.is_empty())
        .map(std::path::PathBuf::from)
        .collect();
    if lowers.is_empty() {
        return Err(Error::Invalid("__ovlmigrate: empty lower stack".into()));
    }
    if !flat_looks_complete(&lowers, &rootfs) {
        // Refuse rather than migrate: see `flat_looks_complete`. The container
        // stays flat, which costs disk and nothing else.
        let _ = std::fs::remove_file(&pending);
        return Ok(());
    }

    std::fs::rename(&rootfs, &upper).map_err(io_err("__ovlmigrate rootfs→upper"))?;

    let committed = (|| -> Result<()> {
        for low in &lowers {
            whiteout_missing(low, &upper, Path::new(""))?;
        }
        std::fs::rename(&pending, dir.join("overlay-lowers"))
            .map_err(io_err("__ovlmigrate commit"))?;
        Ok(())
    })();

    if let Err(e) = committed {
        // Back to exactly what we found. The container starts flat, as before.
        let _ = std::fs::rename(&upper, &rootfs);
        return Err(e);
    }

    let mut freed = 0u64;
    for low in &lowers {
        freed += prune_identical(low, &upper, Path::new(""));
    }
    // Said out loud, and only when it saved something: a migration that runs in
    // silence is indistinguishable from one that did not run at all.
    if freed > 0 {
        println!(
            "migrated to shared layers, {} MiB reclaimed",
            freed / (1024 * 1024)
        );
    }
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

#[cfg(test)]
mod migrate_tests {
    use super::*;
    use std::io::Write;

    /// O idioma dos testes vizinhos (`backup.rs`/`cdi.rs`): `temp_dir` + pid,
    /// porque este crate não tem `dev-dependencies` e a regra do repo é não
    /// acrescentar dependências — nem para testes.
    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("delonix-mig-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &std::path::Path, rel: &str, body: &[u8], mode: u32) -> std::path::PathBuf {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    /// `same_entry` decides what gets DELETED from the upper layer, so every
    /// case it gets wrong loses a container's change. These are the four ways
    /// two files can differ while looking alike to a careless comparison.
    #[test]
    fn same_entry_so_diz_igual_quando_tudo_bate() {
        const NAME: &str = "eq";
        let t = scratch(NAME);
        let a = write(t.as_path(), "a/f", b"conteudo", 0o644);
        let b = write(t.as_path(), "b/f", b"conteudo", 0o644);
        assert!(
            same_entry(&a, &b),
            "mesmos bytes, modo e dono deviam ser iguais"
        );

        // Conteúdo diferente do MESMO tamanho — um comparador por len passaria.
        let c = write(t.as_path(), "c/f", b"conteudX", 0o644);
        assert!(
            !same_entry(&a, &c),
            "bytes diferentes do mesmo tamanho não são iguais"
        );

        // Só o modo difere: o container fez chmod e essa mudança é dele.
        let d = write(t.as_path(), "d/f", b"conteudo", 0o755);
        assert!(
            !same_entry(&a, &d),
            "um chmod do container não pode ser apagado"
        );

        // Tamanhos diferentes.
        let e = write(t.as_path(), "e/f", b"conteudo-maior", 0o644);
        assert!(!same_entry(&a, &e));
    }

    /// Um symlink e um ficheiro regular com o mesmo nome NUNCA são a mesma
    /// coisa — apagar o symlink da upper deixaria a lower a servir um ficheiro
    /// onde o container pôs um link, e vice-versa.
    #[test]
    fn same_entry_distingue_symlink_de_ficheiro() {
        const NAME: &str = "link";
        let t = scratch(NAME);
        let f = write(t.as_path(), "a/f", b"alvo", 0o644);
        let l = t.as_path().join("b/f");
        std::fs::create_dir_all(l.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("alvo", &l).unwrap();
        assert!(!same_entry(&f, &l), "symlink e ficheiro não são o mesmo");

        // Dois symlinks para alvos diferentes também não.
        let l2 = t.as_path().join("c/f");
        std::fs::create_dir_all(l2.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("outro", &l2).unwrap();
        assert!(
            !same_entry(&l, &l2),
            "symlinks com alvos diferentes não são iguais"
        );

        // O mesmo alvo, sim.
        let l3 = t.as_path().join("d/f");
        std::fs::create_dir_all(l3.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("alvo", &l3).unwrap();
        assert!(same_entry(&l, &l3));
    }

    /// A guarda que faltava, e que custou um container real: um rootfs flat
    /// VAZIO não é «o container apagou tudo». Sem isto, cada directório de
    /// topo da lower vira um whiteout e a árvore fica destruída para sempre.
    #[test]
    fn um_rootfs_truncado_nao_migra() {
        const NAME: &str = "trunc";
        let t = scratch(NAME);
        let low = t.join("low");
        for d in ["usr", "etc", "bin"] {
            std::fs::create_dir_all(low.join(d)).unwrap();
        }
        let lowers = vec![low.clone()];

        // Vazio: o caso medido em produção.
        let vazio = t.join("vazio");
        std::fs::create_dir_all(&vazio).unwrap();
        assert!(
            !flat_looks_complete(&lowers, &vazio),
            "um rootfs vazio nunca pode migrar"
        );

        // Truncado: tem parte, falta-lhe um directório de sistema.
        let parcial = t.join("parcial");
        std::fs::create_dir_all(parcial.join("usr")).unwrap();
        std::fs::create_dir_all(parcial.join("etc")).unwrap();
        assert!(
            !flat_looks_complete(&lowers, &parcial),
            "faltar um directório de topo da imagem é implausível, logo não migra"
        );

        // Completo: migra, e um ficheiro apagado LÁ DENTRO continua a poder
        // levar whiteout — a guarda é só ao nível de topo.
        let cheio = t.join("cheio");
        for d in ["usr", "etc", "bin"] {
            std::fs::create_dir_all(cheio.join(d)).unwrap();
        }
        assert!(flat_looks_complete(&lowers, &cheio));
        std::fs::remove_file(cheio.join("etc/motd")).ok();
        assert!(
            flat_looks_complete(&lowers, &cheio),
            "apagar um ficheiro dentro de /etc é normal e não bloqueia a migração"
        );
    }

    /// Um caminho que não existe nunca é "igual" — o valor por omissão tem de
    /// ser NÃO apagar, senão um erro de I/O transitório apaga dados.
    #[test]
    fn same_entry_falha_para_o_lado_seguro() {
        const NAME: &str = "safe";
        let t = scratch(NAME);
        let a = write(t.as_path(), "a/f", b"x", 0o644);
        assert!(!same_entry(&a, &t.as_path().join("nao/existe")));
        assert!(!same_entry(&t.as_path().join("nao/existe"), &a));
    }
}
