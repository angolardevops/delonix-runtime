//! The ownership an image records, kept beside the layers that lose it (ADR-0062).
//!
//! A layer is unpacked with every entry owned by whoever runs the engine: in
//! rootless mode that uid IS uid 0 inside the container, and a second uid cannot
//! be written to disk without a mapped user namespace. So the owner each tar
//! header names is gone the moment the layer is on disk — `/var/lib/haproxy` is
//! `haproxy:haproxy` in the image and `root:root` in a container.
//!
//! The engine used to repair that by giving the WHOLE root filesystem to the
//! container's user (`chown -R <user> /`), which made a non-root user the owner
//! of `/etc/passwd` and of every binary, and re-owned bind-mounted host files.
//! This module is the other half of removing that walk: the owners are recorded
//! while the layer is unpacked, and the container's init gives back exactly the
//! entries the image gave to someone other than root — nothing else.
//!
//! Only non-root owners are recorded. Root is what the unpacked layer already
//! reads as, and listing every file of an image would make the index as large as
//! the image's file list for no effect.

use std::collections::HashSet;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub use delonix_compute::owners::{decode, encode, relative, Owner};

/// Ceiling on the DECOMPRESSED size of one layer. The download side caps the
/// COMPRESSED blob at 8 GiB (`registry::MAX_BLOB_BYTES`); that number says
/// nothing about the decompressed side, where a few MB of zeros become
/// terabytes. Applied by `overlay::with_layer_archive` through [`LimitReader`],
/// so every real caller (pull, CRI, flat extract) inherits it. Generous for
/// real layers (`kindest/node` runs to low single-digit GiB, AGENTS.md) and far
/// below any bomb.
pub(crate) const MAX_LAYER_UNCOMPRESSED_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// Ceiling on the NUMBER of entries in one layer, against the
/// millions-of-tiny-files variant the byte ceiling alone misses: it exhausts
/// inodes, not bytes, so a stream of zero-length entries slips under
/// [`MAX_LAYER_UNCOMPRESSED_BYTES`]. A full distro image has well under a
/// million files.
pub(crate) const MAX_LAYER_ENTRIES: u64 = 4_000_000;

/// A reader that refuses to yield more than `limit` bytes, so a decompressor
/// behind it cannot be driven to produce an unbounded stream from a small blob.
///
/// Structural, not header-trusting: it counts the bytes the tar parser actually
/// pulls, whatever the entry headers claim. Without it a crafted layer — a few
/// MB of zeros that gzip/zstd expand to terabytes — fills the node's disk and
/// the kubelet evicts every pod on it (a decompression bomb). The sibling cap
/// on entry count lives in [`unpack_recording`]/[`scan`]; the two together are
/// the layer-unpack half of the blob-size cap the download already had.
pub(crate) struct LimitReader<R> {
    inner: R,
    remaining: u64,
}

impl<R> LimitReader<R> {
    pub(crate) fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

impl<R: Read> Read for LimitReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            // We have already delivered the whole budget; one more byte would
            // exceed it. Fail closed — a truncated extraction is a clear error,
            // a filled disk is not.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "layer exceeds the {} GiB uncompressed ceiling (possible decompression bomb)",
                    MAX_LAYER_UNCOMPRESSED_BYTES / (1024 * 1024 * 1024)
                ),
            ));
        }
        let cap = buf
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..cap])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// The owner a header names, when it is worth recording: not root:root, not a
/// whiteout (an instruction to the layer merge, not a file of the image).
fn owner_of<R: Read>(entry: &tar::Entry<'_, R>) -> Option<Owner> {
    let header = entry.header();
    let (uid, gid) = (header.uid().ok()?, header.gid().ok()?);
    if uid == 0 && gid == 0 {
        return None;
    }
    let (uid, gid) = (u32::try_from(uid).ok()?, u32::try_from(gid).ok()?);
    let path = relative(&entry.path().ok()?)?;
    let is_whiteout = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(".wh."));
    (!is_whiteout).then_some(Owner { path, uid, gid })
}

/// Unpacks `archive` into `dst` exactly as `tar::Archive::unpack` does, and
/// returns the non-root owners its headers name.
///
/// The loop is `tar`'s own (`Archive::_unpack`, 0.4.46), kept step for step:
/// non-directories as they come, directories last and deepest first, so a
/// restrictive directory mode never blocks the extraction of its children. It is
/// repeated here only because `unpack` gives no access to the headers, and
/// reading them in a second pass would decompress every layer twice.
pub fn unpack_recording<R: Read>(
    archive: &mut tar::Archive<R>,
    dst: &Path,
) -> io::Result<Vec<Owner>> {
    unpack_recording_limited(archive, dst, MAX_LAYER_ENTRIES)
}

/// As [`unpack_recording`], with the entry ceiling as a parameter so a test can
/// reach the limit cheaply instead of building four million entries.
pub(crate) fn unpack_recording_limited<R: Read>(
    archive: &mut tar::Archive<R>,
    dst: &Path,
    max_entries: u64,
) -> io::Result<Vec<Owner>> {
    // The mode the image recorded, special bits included. `tar` drops setuid,
    // setgid and sticky unless told otherwise: measured, `/tmp` came out `777`
    // instead of `1777` (any user could delete another's files there) and
    // `/usr/bin/su` came out without setuid, so it could not switch user. The
    // files are owned by whoever runs the engine, so on the host a setuid bit
    // grants nothing that user lacks; inside the container that owner is root,
    // which is what the image meant.
    archive.set_preserve_permissions(true);
    if dst.symlink_metadata().is_err() {
        std::fs::create_dir_all(dst)?;
    }
    let dst = &dst.canonicalize().unwrap_or(dst.to_path_buf());
    let mut owners = Vec::new();
    let mut directories = Vec::new();
    let mut count: u64 = 0;
    for entry in archive.entries()? {
        count += 1;
        if count > max_entries {
            return Err(too_many_entries(max_entries));
        }
        let mut file = entry?;
        owners.extend(owner_of(&file));
        if file.header().entry_type() == tar::EntryType::Directory {
            directories.push(file);
        } else {
            file.unpack_in(dst)?;
        }
    }
    directories.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
    for mut dir in directories {
        dir.unpack_in(dst)?;
    }
    Ok(owners)
}

/// The non-root owners of a layer, from its headers alone — for a layer that
/// was unpacked before the engine recorded them.
///
/// Such a layer was also unpacked WITHOUT its setuid, setgid and sticky bits.
/// When `unpacked` is given, the entries that carry one get their mode back in
/// that directory, so an image pulled by an older engine does not need a
/// re-pull to have a working `su` or a sticky `/tmp`.
pub fn scan<R: Read>(
    archive: &mut tar::Archive<R>,
    unpacked: Option<&Path>,
) -> io::Result<Vec<Owner>> {
    let mut owners = Vec::new();
    let mut count: u64 = 0;
    for entry in archive.entries()? {
        count += 1;
        if count > MAX_LAYER_ENTRIES {
            return Err(too_many_entries(MAX_LAYER_ENTRIES));
        }
        let entry = entry?;
        owners.extend(owner_of(&entry));
        if let Some(dir) = unpacked {
            restore_special_mode(&entry, dir);
        }
    }
    Ok(owners)
}

/// The error both unpack paths raise when a layer has more entries than the
/// ceiling — the inode-exhaustion half of the decompression-bomb defense.
fn too_many_entries(max: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("layer has more than {max} entries (possible tar bomb)"),
    )
}

/// Gives an unpacked entry its setuid/setgid/sticky bits back. Best-effort, and
/// never through a symlink: a link has no mode, and `chmod` would follow it out
/// of the layer.
fn restore_special_mode<R: Read>(entry: &tar::Entry<'_, R>, dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(mode) = entry.header().mode() else {
        return;
    };
    let kind = entry.header().entry_type();
    if mode & 0o7000 == 0 || !(kind.is_file() || kind.is_dir()) {
        return;
    }
    let Some(rel) = entry.path().ok().and_then(|p| relative(&p)) else {
        return;
    };
    let path = dir.join(rel);
    let real = std::fs::symlink_metadata(&path).is_ok_and(|m| !m.file_type().is_symlink());
    if real {
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o7777));
    }
}

/// The owners of the image as a container sees it: for each path, the TOPMOST
/// layer that holds it decides.
///
/// `layers` is lowest first, each with its unpacked directory and its own
/// owners. A path a lower layer gives to a user and a higher layer ships again
/// as root is root in the merged view — the higher layer's entry is not in its
/// index (only non-root owners are), so the directory on disk is what says the
/// higher layer has it.
pub fn merge(layers: &[(PathBuf, Vec<Owner>)]) -> Vec<Owner> {
    let mut out = Vec::new();
    let mut decided: HashSet<&Path> = HashSet::new();
    for (i, (_, owners)) in layers.iter().enumerate().rev() {
        // The last record for a path wins within one layer, as the last tar
        // entry does when it is unpacked.
        for o in owners.iter().rev() {
            if !decided.insert(o.path.as_path()) {
                continue;
            }
            let shadowed = layers[i + 1..]
                .iter()
                .any(|(dir, _)| dir.join(&o.path).symlink_metadata().is_ok());
            if !shadowed {
                out.push(o.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tar with the given `(path, uid, gid, is_dir)` entries.
    fn tar_of(entries: &[(&str, u64, u64, bool)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, uid, gid, is_dir) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_uid(*uid);
            h.set_gid(*gid);
            if *is_dir {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(0o755);
                h.set_size(0);
                b.append_data(&mut h, path, &[][..]).unwrap();
            } else {
                h.set_mode(0o644);
                h.set_size(1);
                b.append_data(&mut h, path, &b"x"[..]).unwrap();
            }
        }
        b.into_inner().unwrap()
    }

    fn owner(path: &str, uid: u32, gid: u32) -> Owner {
        Owner {
            path: path.into(),
            uid,
            gid,
        }
    }

    /// The structural cap: a reader never yields past its ceiling, and the
    /// error names the bomb so the caller's "failed to extract layer" is
    /// actionable.
    #[test]
    fn the_limit_reader_refuses_a_stream_past_its_ceiling() {
        let data = vec![0u8; 10 * 1024];
        let mut r = LimitReader::new(&data[..], 1024);
        let mut sink = Vec::new();
        let err = io::copy(&mut r, &mut sink).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            sink.len() <= 1024,
            "delivered {} bytes past the 1 KiB ceiling",
            sink.len()
        );
        assert!(err.to_string().contains("decompression bomb"));
    }

    /// The exact composition `overlay::with_layer_archive` builds — a gzip
    /// decoder behind a `LimitReader` — stops a bomb: ~1 MiB of zeros gzip to a
    /// few hundred bytes, and the ceiling (here 64 KiB) fires long before the
    /// MiB is written. Without the `LimitReader` the copy succeeds and this
    /// fails: that is the regression it guards.
    #[test]
    fn a_gzip_bomb_is_refused_by_the_uncompressed_ceiling() {
        use std::io::Write;
        let payload = vec![0u8; 1024 * 1024];
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&payload).unwrap();
        let gz = enc.finish().unwrap();
        assert!(
            gz.len() < 64 * 1024,
            "the compressed bomb should be tiny, was {}",
            gz.len()
        );
        let dec = flate2::read::GzDecoder::new(&gz[..]);
        let mut r = LimitReader::new(dec, 64 * 1024);
        let mut sink = Vec::new();
        let err = io::copy(&mut r, &mut sink).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(sink.len() <= 64 * 1024);
    }

    /// The entry cap: a layer with more entries than the ceiling is refused
    /// before it can exhaust the node's inodes, and nothing past the limit is
    /// written. Measured against the limit, not four million real entries.
    #[test]
    fn too_many_entries_are_refused() {
        let data = tar_of(&[("a", 0, 0, false), ("b", 0, 0, false), ("c", 0, 0, false)]);
        let tmp = tempfile::tempdir().unwrap();
        let err =
            unpack_recording_limited(&mut tar::Archive::new(&data[..]), tmp.path(), 2).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("entries"));
        // The third entry, past the ceiling, never reached the disk.
        assert!(!tmp.path().join("c").exists());
    }

    /// Unpacking records every non-root owner and nothing else, and the files
    /// land where `Archive::unpack` would put them.
    #[test]
    fn unpacking_records_only_the_non_root_owners() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tar_of(&[
            ("etc/", 0, 0, true),
            ("etc/passwd", 0, 0, false),
            ("./var/lib/app/", 99, 99, true),
            ("var/lib/app/data", 99, 99, false),
            ("usr/share/group-only", 0, 5, false),
            ("var/lib/.wh.gone", 99, 99, false),
        ]);
        let owners = unpack_recording(&mut tar::Archive::new(&data[..]), tmp.path()).unwrap();
        assert_eq!(
            owners,
            vec![
                owner("var/lib/app", 99, 99),
                owner("var/lib/app/data", 99, 99),
                owner("usr/share/group-only", 0, 5),
            ]
        );
        assert!(tmp.path().join("etc/passwd").is_file());
        assert!(tmp.path().join("var/lib/app/data").is_file());
        // The header-only scan of the same layer agrees with the unpack.
        assert_eq!(
            scan(&mut tar::Archive::new(&data[..]), None).unwrap(),
            owners
        );
    }

    /// A tar with one file and one directory, with the given modes.
    fn tar_with_modes(file_mode: u32, dir_mode: u32) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(dir_mode);
        h.set_size(0);
        b.append_data(&mut h, "tmp/", &[][..]).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_mode(file_mode);
        h.set_size(1);
        b.append_data(&mut h, "bin-su", &b"x"[..]).unwrap();
        b.into_inner().unwrap()
    }

    /// Unpacking keeps setuid, setgid and sticky; and a layer unpacked without
    /// them gets them back from its headers.
    #[test]
    fn special_mode_bits_survive_the_unpack_and_are_healed() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let data = tar_with_modes(0o4755, 0o1777);

        let tmp = tempfile::tempdir().unwrap();
        unpack_recording(&mut tar::Archive::new(&data[..]), tmp.path()).unwrap();
        assert_eq!(mode(&tmp.path().join("bin-su")), 0o4755);
        assert_eq!(mode(&tmp.path().join("tmp")), 0o1777);

        // What an older engine left on disk: the bits gone.
        let old = tempfile::tempdir().unwrap();
        tar::Archive::new(&data[..]).unpack(old.path()).unwrap();
        assert_eq!(mode(&old.path().join("bin-su")) & 0o7000, 0);
        scan(&mut tar::Archive::new(&data[..]), Some(old.path())).unwrap();
        assert_eq!(mode(&old.path().join("bin-su")), 0o4755);
        assert_eq!(mode(&old.path().join("tmp")), 0o1777);
    }

    /// The topmost layer that holds a path decides its owner: a higher layer
    /// that ships the path again as root takes it back.
    #[test]
    fn the_topmost_layer_holding_a_path_decides() {
        let tmp = tempfile::tempdir().unwrap();
        let (low, mid, high) = (
            tmp.path().join("low"),
            tmp.path().join("mid"),
            tmp.path().join("high"),
        );
        for (dir, files) in [
            (&low, &["data/kept", "data/retaken", "data/reowned"][..]),
            (&mid, &["data/reowned"][..]),
            (&high, &["data/retaken"][..]),
        ] {
            for f in files {
                let p = dir.join(f);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, b"x").unwrap();
            }
        }
        let merged = merge(&[
            (
                low,
                vec![
                    owner("data/kept", 99, 99),
                    owner("data/retaken", 99, 99),
                    owner("data/reowned", 99, 99),
                ],
            ),
            (mid, vec![owner("data/reowned", 7, 7)]),
            // `high` ships data/retaken as root: on disk, not in its index.
            (high, vec![]),
        ]);
        let mut got = merged;
        got.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            got,
            vec![owner("data/kept", 99, 99), owner("data/reowned", 7, 7)]
        );
    }
}
