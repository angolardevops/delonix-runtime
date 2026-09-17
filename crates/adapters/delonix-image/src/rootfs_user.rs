//! `--user <uid[:gid]|name[:group]>` resolved against an unpacked image rootfs.
//!
//! The user part is a number (used verbatim) or a name looked up in the image's
//! `/etc/passwd`, which gives its uid AND its primary gid; that gid is used when no
//! `:group` is given (like docker and the CRI `RunAsUsername`, where the runtime
//! MUST resolve the user in the image). The optional group part is a number or a
//! name looked up in `/etc/group`. A name the image does not have is an error,
//! never invented.
//!
//! The rootfs is usually NOT mounted when this runs: a rootless container's
//! overlay is mounted by its own init, inside its user namespace, so `merged/` is
//! still an empty directory. Reading `merged/etc/passwd` answered "no such user"
//! for every name on every rootless `run -u <name>` (measured). The files are
//! read through the layers instead, the way the overlay will present them.

use std::path::{Path, PathBuf};

/// Why a `--user` specification could not be resolved. Returned as data so the
/// caller words it (and translates it) for its own audience.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserLookupError {
    /// The user part is empty (`--user ""` or `--user :0`).
    EmptyUser,
    /// A user name the image's `/etc/passwd` does not have.
    NoSuchUser(String),
    /// A group name the image's `/etc/group` does not have.
    NoSuchGroup(String),
}

/// Resolves `spec` into `(uid, Option<gid>)` against `rootfs`.
pub fn resolve_user(rootfs: &Path, spec: &str) -> Result<(u32, Option<u32>), UserLookupError> {
    let (user_part, group_part) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    if user_part.is_empty() {
        return Err(UserLookupError::EmptyUser);
    }
    let (uid, primary_gid) = match user_part.parse::<u32>() {
        Ok(n) => (n, None),
        Err(_) => {
            let (uid, gid) = passwd_lookup(rootfs, user_part)
                .ok_or_else(|| UserLookupError::NoSuchUser(user_part.to_string()))?;
            (uid, Some(gid))
        }
    };
    let gid = match group_part {
        Some(g) if !g.is_empty() => Some(match g.parse::<u32>() {
            Ok(n) => n,
            Err(_) => group_lookup(rootfs, g)
                .ok_or_else(|| UserLookupError::NoSuchGroup(g.to_string()))?,
        }),
        _ => primary_gid,
    };
    Ok((uid, gid))
}

/// `name:passwd:uid:gid:gecos:home:shell` — returns `(uid, gid)`.
fn passwd_lookup(rootfs: &Path, name: &str) -> Option<(u32, u32)> {
    let content = read_image_file(rootfs, "etc", "passwd")?;
    content.lines().find_map(|line| {
        let mut f = line.split(':');
        if f.next() != Some(name) {
            return None;
        }
        let uid = f.nth(1)?.parse().ok()?;
        let gid = f.next()?.parse().ok()?;
        Some((uid, gid))
    })
}

/// `name:passwd:gid:members` — returns the gid.
fn group_lookup(rootfs: &Path, name: &str) -> Option<u32> {
    let content = read_image_file(rootfs, "etc", "group")?;
    content.lines().find_map(|line| {
        let mut f = line.split(':');
        if f.next() != Some(name) {
            return None;
        }
        f.nth(1)?.parse().ok()
    })
}

/// `<dir>/<name>` as the container will see it.
///
/// A regular file under `rootfs` wins: a flat rootfs, or an overlay already
/// mounted by the root-mode engine. Otherwise, if `rootfs` is the `merged/` of an
/// overlay-backed container, the write layer and then each lower layer (highest
/// first) are searched, and a deletion in a higher layer hides the file below it.
///
/// A symlink is never followed: the path belongs to an image, and resolving it on
/// the host would read a HOST file.
fn read_image_file(rootfs: &Path, dir: &str, name: &str) -> Option<String> {
    let direct = rootfs.join(dir).join(name);
    if is_regular_file(&direct) {
        return std::fs::read_to_string(direct).ok();
    }
    for layer in overlay_layers(rootfs)? {
        let d = layer.join(dir);
        let entry = d.join(name);
        if let Ok(meta) = std::fs::symlink_metadata(&entry) {
            // The kernel's whiteout in the write layer is a 0:0 character device;
            // anything but a regular file ends the search without a file.
            return meta
                .file_type()
                .is_file()
                .then(|| std::fs::read_to_string(entry).ok())
                .flatten();
        }
        // The OCI whiteouts of an extracted image layer: the file deleted, or the
        // whole directory made opaque.
        if d.join(format!(".wh.{name}")).exists() || d.join(".wh..wh..opq").exists() {
            return None;
        }
    }
    None
}

/// The layers of an overlay-backed container whose `merged/` is `rootfs`, highest
/// first: the write layer, then the lower stack from the marker file.
fn overlay_layers(rootfs: &Path) -> Option<Vec<PathBuf>> {
    let base = rootfs.parent()?;
    let lowers = std::fs::read_to_string(base.join(crate::ImageStore::LOWERS_FILE)).ok()?;
    let mut layers = vec![base.join("upper")];
    layers.extend(lowers.lines().filter(|l| !l.is_empty()).map(PathBuf::from));
    Some(layers)
}

fn is_regular_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rootfs() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("etc")).unwrap();
        std::fs::write(
            d.path().join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/sh\napp:x:1000:1001::/home/app:/bin/sh\n",
        )
        .unwrap();
        std::fs::write(d.path().join("etc/group"), "root:x:0:\nstaff:x:50:app\n").unwrap();
        d
    }

    #[test]
    fn numbers_are_used_verbatim_without_reading_the_image() {
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(resolve_user(empty.path(), "42"), Ok((42, None)));
        assert_eq!(resolve_user(empty.path(), "42:7"), Ok((42, Some(7))));
    }

    #[test]
    fn a_name_brings_its_primary_gid_unless_a_group_is_given() {
        let r = rootfs();
        assert_eq!(resolve_user(r.path(), "app"), Ok((1000, Some(1001))));
        assert_eq!(resolve_user(r.path(), "app:staff"), Ok((1000, Some(50))));
        assert_eq!(resolve_user(r.path(), "app:0"), Ok((1000, Some(0))));
        assert_eq!(resolve_user(r.path(), "app:"), Ok((1000, Some(1001))));
    }

    #[test]
    fn names_the_image_does_not_have_are_refused_never_invented() {
        let r = rootfs();
        assert_eq!(resolve_user(r.path(), ""), Err(UserLookupError::EmptyUser));
        assert_eq!(
            resolve_user(r.path(), ":0"),
            Err(UserLookupError::EmptyUser)
        );
        assert_eq!(
            resolve_user(r.path(), "ghost"),
            Err(UserLookupError::NoSuchUser("ghost".into()))
        );
        assert_eq!(
            resolve_user(r.path(), "app:ghosts"),
            Err(UserLookupError::NoSuchGroup("ghosts".into()))
        );
    }

    /// `containers/<id>/{merged,upper}` plus two extracted lower layers, the way
    /// a rootless container looks before its init mounts the overlay.
    fn overlay(
        upper: &[(&str, &str)],
        high: &[(&str, &str)],
        low: &[(&str, &str)],
    ) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let put = |layer: &Path, files: &[(&str, &str)]| {
            std::fs::create_dir_all(layer.join("etc")).unwrap();
            for (name, body) in files {
                std::fs::write(layer.join("etc").join(name), body).unwrap();
            }
        };
        let base = d.path().join("containers/c1");
        std::fs::create_dir_all(base.join("merged")).unwrap();
        put(&base.join("upper"), upper);
        put(&d.path().join("layers/high"), high);
        put(&d.path().join("layers/low"), low);
        std::fs::write(
            base.join(crate::ImageStore::LOWERS_FILE),
            format!(
                "{}\n{}\n",
                d.path().join("layers/high").display(),
                d.path().join("layers/low").display()
            ),
        )
        .unwrap();
        d
    }

    fn merged(d: &tempfile::TempDir) -> PathBuf {
        d.path().join("containers/c1/merged")
    }

    #[test]
    fn an_unmounted_overlay_is_read_through_its_layers() {
        let d = overlay(
            &[],
            &[],
            &[
                ("passwd", "app:x:1000:1001::/:/bin/sh\n"),
                ("group", "staff:x:50:\n"),
            ],
        );
        assert_eq!(resolve_user(&merged(&d), "app:staff"), Ok((1000, Some(50))));
    }

    #[test]
    fn a_higher_layer_wins_and_the_write_layer_wins_over_all() {
        let low = [("passwd", "app:x:1:1::/:/bin/sh\n")];
        let high = [("passwd", "app:x:2:2::/:/bin/sh\n")];
        let d = overlay(&[], &high, &low);
        assert_eq!(resolve_user(&merged(&d), "app"), Ok((2, Some(2))));
        let d = overlay(&[("passwd", "app:x:3:3::/:/bin/sh\n")], &high, &low);
        assert_eq!(resolve_user(&merged(&d), "app"), Ok((3, Some(3))));
    }

    #[test]
    fn a_whiteout_in_a_higher_layer_hides_the_file_below() {
        let low = [("passwd", "app:x:1:1::/:/bin/sh\n")];
        let d = overlay(&[], &[(".wh.passwd", "")], &low);
        assert_eq!(
            resolve_user(&merged(&d), "app"),
            Err(UserLookupError::NoSuchUser("app".into()))
        );
        let d = overlay(&[], &[(".wh..wh..opq", "")], &low);
        assert_eq!(
            resolve_user(&merged(&d), "app"),
            Err(UserLookupError::NoSuchUser("app".into()))
        );
    }

    #[test]
    fn a_symlinked_passwd_is_never_followed_to_the_host() {
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("passwd"), "hostuser:x:7:7::/:/bin/sh\n").unwrap();
        let flat = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(flat.path().join("etc")).unwrap();
        std::os::unix::fs::symlink(host.path().join("passwd"), flat.path().join("etc/passwd"))
            .unwrap();
        assert_eq!(
            resolve_user(flat.path(), "hostuser"),
            Err(UserLookupError::NoSuchUser("hostuser".into()))
        );
        let d = overlay(&[], &[], &[]);
        std::fs::remove_dir_all(d.path().join("layers/high/etc")).unwrap();
        std::fs::create_dir_all(d.path().join("layers/high/etc")).unwrap();
        std::os::unix::fs::symlink(
            host.path().join("passwd"),
            d.path().join("layers/high/etc/passwd"),
        )
        .unwrap();
        assert_eq!(
            resolve_user(&merged(&d), "hostuser"),
            Err(UserLookupError::NoSuchUser("hostuser".into()))
        );
    }

    #[test]
    fn a_name_that_prefixes_another_does_not_match_it() {
        let r = rootfs();
        assert_eq!(
            resolve_user(r.path(), "ap"),
            Err(UserLookupError::NoSuchUser("ap".into()))
        );
    }
}
