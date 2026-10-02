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
    for entry in archive.entries()? {
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
    for entry in archive.entries()? {
        let entry = entry?;
        owners.extend(owner_of(&entry));
        if let Some(dir) = unpacked {
            restore_special_mode(&entry, dir);
        }
    }
    Ok(owners)
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
