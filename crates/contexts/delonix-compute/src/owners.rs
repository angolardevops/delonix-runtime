//! The ownership an image records for its files, as data (ADR-0062).
//!
//! Written by the image store when a layer is unpacked and read by the
//! container's init, which gives back exactly the entries the image gave to a
//! user or group other than root. The two sides live in different adapters, so
//! the record and its on-disk form live here, once.

use std::path::{Component, Path, PathBuf};

/// The file, beside a container's `merged/`, that holds its image's owners.
pub const OWNERS_FILE: &str = "overlay-owners";

/// One entry the image gives to a user or group other than root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    /// Relative to the root filesystem: no leading `/`, no `..`.
    pub path: PathBuf,
    pub uid: u32,
    pub gid: u32,
}

/// `path` as a path relative to the rootfs, or `None` when it cannot name
/// something inside it (`..`, or nothing left). A leading `./` or `/` is dropped.
pub fn relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// On-disk form: one `uid gid path` record per entry, NUL-terminated. A path
/// may hold a newline or a space; it cannot hold a NUL.
pub fn encode(owners: &[Owner]) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut out = Vec::new();
    for o in owners {
        out.extend_from_slice(format!("{} {} ", o.uid, o.gid).as_bytes());
        out.extend_from_slice(o.path.as_os_str().as_bytes());
        out.push(0);
    }
    out
}

/// Reads what [`encode`] wrote. A record that does not parse, or whose path is
/// not confined to the rootfs, is dropped: the index is advice about ownership,
/// and a damaged line must not become a path the init acts on.
pub fn decode(bytes: &[u8]) -> Vec<Owner> {
    use std::os::unix::ffi::OsStrExt;
    bytes
        .split(|b| *b == 0)
        .filter_map(|rec| {
            let mut parts = rec.splitn(3, |b| *b == b' ');
            let uid = std::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
            let gid = std::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
            let path = relative(Path::new(std::ffi::OsStr::from_bytes(parts.next()?)))?;
            Some(Owner { path, uid, gid })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(path: &str, uid: u32, gid: u32) -> Owner {
        Owner {
            path: path.into(),
            uid,
            gid,
        }
    }

    /// The on-disk form survives a path with a space and a newline, and a record
    /// that tries to leave the rootfs is dropped on the way in.
    #[test]
    fn the_index_round_trips_and_refuses_escapes() {
        let owners = vec![owner("a b/c\nd", 1000, 1000), owner("x", 0, 42)];
        assert_eq!(decode(&encode(&owners)), owners);
        let hostile = b"99 99 ../../home/user/.ssh\x0099 99 ok\x00garbage\x00";
        assert_eq!(decode(hostile), vec![owner("ok", 99, 99)]);
        assert!(decode(b"").is_empty());
        assert_eq!(
            relative(Path::new("/./etc/passwd")),
            Some("etc/passwd".into())
        );
    }
}
