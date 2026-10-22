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

/// Default ceiling on the DECOMPRESSED size of one layer. The download side caps
/// the COMPRESSED blob at 8 GiB (`registry::MAX_BLOB_BYTES`); that number says
/// nothing about the decompressed side, where a few MB of zeros become
/// terabytes. Applied by `overlay::with_layer_archive` through [`LimitReader`],
/// so every real caller (pull, CRI, flat extract) inherits it.
///
/// Why 64 GiB: real layers are far below it (a Kubernetes node image runs to a
/// few GiB in total, the largest public GPU/ML images to a few tens of GiB
/// across many layers, and a single layer is a fraction of that), so no normal
/// image comes near it — while any bomb is orders of magnitude above. A node
/// with a small disk should LOWER it with [`BYTES_ENV`]; a trusted image that
/// really is larger can raise it.
pub(crate) const DEFAULT_MAX_LAYER_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// Default ceiling on the NUMBER of entries in one layer, against the
/// millions-of-tiny-files variant the byte ceiling alone misses: it exhausts
/// inodes, not bytes, so a stream of zero-length entries slips under
/// [`DEFAULT_MAX_LAYER_BYTES`]. A full distro image has a few hundred thousand
/// files, a `node_modules`-heavy layer rarely above a million; 4M leaves room
/// for both and is still a fraction of a filesystem's inode table.
pub(crate) const DEFAULT_MAX_LAYER_ENTRIES: u64 = 4_000_000;

/// Environment variable that overrides [`DEFAULT_MAX_LAYER_BYTES`].
pub(crate) const BYTES_ENV: &str = "DELONIX_LAYER_MAX_BYTES";
/// Environment variable that overrides [`DEFAULT_MAX_LAYER_ENTRIES`].
pub(crate) const ENTRIES_ENV: &str = "DELONIX_LAYER_MAX_ENTRIES";

/// The two ceilings a layer is unpacked under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LayerLimits {
    /// Uncompressed tar-stream bytes (headers and padding included).
    pub max_bytes: u64,
    /// Entries in the tar.
    pub max_entries: u64,
}

impl LayerLimits {
    /// What an unconfigured node enforces.
    pub(crate) const DEFAULT: Self = Self {
        max_bytes: DEFAULT_MAX_LAYER_BYTES,
        max_entries: DEFAULT_MAX_LAYER_ENTRIES,
    };

    /// The limits in force: the defaults, each overridden by its environment
    /// variable. A value that is not a positive size is IGNORED with a warning,
    /// never read as "no limit": a typo must not switch a security bound off.
    pub(crate) fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let pick = |name: &str, default: u64| match get(name) {
            None => default,
            Some(raw) if raw.trim().is_empty() => default,
            Some(raw) => parse_size(&raw).unwrap_or_else(|| {
                tracing::warn!(
                    variable = name,
                    value = %raw,
                    default,
                    "ignoring a layer limit that is not a positive size; using the default"
                );
                default
            }),
        };
        Self {
            max_bytes: pick(BYTES_ENV, Self::DEFAULT.max_bytes),
            max_entries: pick(ENTRIES_ENV, Self::DEFAULT.max_entries),
        }
    }
}

/// A positive integer with an optional binary suffix (`k`, `m`, `g`, `t`, any
/// case; `1g` is 2^30). Zero, empty, negative, fractional or overflowing values
/// are `None`.
fn parse_size(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let digits_end = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (digits, suffix) = raw.split_at(digits_end);
    let unit: u64 = match suffix.to_ascii_lowercase().as_str() {
        "" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => return None,
    };
    let n: u64 = digits.parse().ok()?;
    n.checked_mul(unit).filter(|&v| v > 0)
}

/// Which of the two ceilings a layer went past.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LimitKind {
    /// Too many uncompressed bytes.
    Bytes,
    /// Too many entries.
    Entries,
}

/// A layer went past an unpack ceiling. Carried inside an `io::Error` through
/// the `tar` and decompressor stack, and recovered by [`exceeded`] so the
/// caller can raise the stable, typed [`crate::Error::LayerLimit`] instead of a
/// string.
#[derive(Debug)]
pub(crate) struct LayerLimitExceeded {
    pub kind: LimitKind,
    pub limit: u64,
}

impl std::fmt::Display for LayerLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            LimitKind::Bytes => write!(
                f,
                "layer expands beyond the {} uncompressed limit (possible decompression bomb); \
                 set {BYTES_ENV} to accept a larger one",
                human_bytes(self.limit)
            ),
            LimitKind::Entries => write!(
                f,
                "layer has more than {} entries (possible tar bomb); \
                 set {ENTRIES_ENV} to accept a larger one",
                self.limit
            ),
        }
    }
}

impl std::error::Error for LayerLimitExceeded {}

impl LayerLimitExceeded {
    pub(crate) fn into_io(self) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, self)
    }
}

fn human_bytes(n: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    if n >= GIB && n.is_multiple_of(GIB) {
        format!("{} GiB", n / GIB)
    } else if n >= MIB && n.is_multiple_of(MIB) {
        format!("{} MiB", n / MIB)
    } else {
        format!("{n} bytes")
    }
}

/// The ceiling an error went past, if it is (or wraps) a
/// [`LayerLimitExceeded`]. `tar` wraps the reader's error in its own context
/// (`failed to unpack ...`), and `io::Error::source` skips the error it holds,
/// so the walk looks inside each `io::Error` before following `source()`.
pub(crate) fn exceeded(e: &io::Error) -> Option<&LayerLimitExceeded> {
    fn find<'a>(mut cur: &'a (dyn std::error::Error + 'static)) -> Option<&'a LayerLimitExceeded> {
        loop {
            if let Some(l) = cur.downcast_ref::<LayerLimitExceeded>() {
                return Some(l);
            }
            if let Some(io_err) = cur.downcast_ref::<io::Error>() {
                cur = io_err.get_ref()?;
                continue;
            }
            cur = cur.source()?;
        }
    }
    find(e)
}

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
    limit: u64,
    remaining: u64,
}

impl<R> LimitReader<R> {
    pub(crate) fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            remaining: limit,
        }
    }
}

impl<R: Read> Read for LimitReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            // The whole budget is delivered. A stream that ends exactly here is
            // fine; one more byte would exceed it. Fail closed — a truncated
            // extraction is a clear error, a filled disk is not.
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(LayerLimitExceeded {
                    kind: LimitKind::Bytes,
                    limit: self.limit,
                }
                .into_io()),
            };
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
    unpack_recording_limited(archive, dst, LayerLimits::from_env().max_entries)
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
    // files are owned by whoever runs the engine, so in rootless mode (the
    // normal path) that owner is the unprivileged caller, and a setuid bit
    // grants nothing it lacks; inside the container that owner is root, which is
    // what the image meant.
    //
    // On a REAL-root node (the CRI path) the recorded uid is applied verbatim, so
    // a setuid bit on a file owned by a NON-root uid would be setuid to a named
    // host identity on the node filesystem — something no container needs. That
    // one bit is dropped (`sanitize_special_bits`); setuid-root (the image's
    // `su`/`mount`), setgid and sticky are kept, so nothing the container relies
    // on regresses.
    archive.set_preserve_permissions(true);
    let real_root = !delonix_node::is_rootless();
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
        // OCI whiteouts are an instruction to the layer MERGE, not files of the
        // image. On the per-layer overlay path each layer is its own `lowerdir`,
        // so a deletion cannot reach a lower layer: the whiteout has to be left
        // in the form the kernel overlay driver honours — a 0/0 character device
        // standing where the hidden entry was. Unpacked as the literal
        // `.wh.<name>` regular file (what this loop used to do), the kernel
        // ignored it and the "deleted" lower entry stayed visible — e.g. a Debian
        // image whose build ran `a2dismod mpm_event` booted with the lower
        // layer's `mpm_event.load` still present and died with "More than one MPM
        // loaded". `apply_layer_flat` (the flat-bundle path) handles the same
        // markers by deleting the target instead.
        if let Some(rel) = file.path().ok().and_then(|p| relative(&p)) {
            if let Some(base) = rel.file_name().and_then(|s| s.to_str()) {
                if base == ".wh..wh..opq" {
                    // An opaque directory needs the `trusted.overlay.opaque`
                    // xattr (or its userxattr equivalent), which is privileged
                    // and cannot be set on the rootless path; we at least never
                    // extract the marker as a visible file. Opaque-dir whiteouts
                    // in rootless images stay a known gap (they need a userxattr
                    // mount), but this is the rarer marker.
                    continue;
                }
                if let Some(name) = base.strip_prefix(".wh.") {
                    let at = rel
                        .parent()
                        .map(|p| dst.join(p))
                        .unwrap_or_else(|| dst.to_path_buf())
                        .join(name);
                    write_overlay_whiteout(&at);
                    continue;
                }
            }
        }
        if file.header().entry_type() == tar::EntryType::Directory {
            directories.push(file);
        } else {
            file.unpack_in(dst)?;
            drop_foreign_setuid(&file, dst, real_root);
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
    scan_limited(archive, unpacked, LayerLimits::from_env().max_entries)
}

/// As [`scan`], with the entry ceiling as a parameter (see
/// [`unpack_recording_limited`]).
pub(crate) fn scan_limited<R: Read>(
    archive: &mut tar::Archive<R>,
    unpacked: Option<&Path>,
    max_entries: u64,
) -> io::Result<Vec<Owner>> {
    let real_root = !delonix_node::is_rootless();
    let mut owners = Vec::new();
    let mut count: u64 = 0;
    for entry in archive.entries()? {
        count += 1;
        if count > max_entries {
            return Err(too_many_entries(max_entries));
        }
        let entry = entry?;
        owners.extend(owner_of(&entry));
        if let Some(dir) = unpacked {
            restore_special_mode(&entry, dir, real_root);
        }
    }
    Ok(owners)
}

/// The error both unpack paths raise when a layer has more entries than the
/// ceiling — the inode-exhaustion half of the decompression-bomb defense.
pub(crate) fn too_many_entries(max: u64) -> io::Error {
    LayerLimitExceeded {
        kind: LimitKind::Entries,
        limit: max,
    }
    .into_io()
}

/// The special-bit mode to write on disk, given the entry's recorded `uid` and
/// whether the engine is REAL root (euid 0 in the initial user namespace).
///
/// The only thing dropped is a setuid bit owned by a NON-root uid on a real-root
/// node: that is setuid to a named host identity, which no container needs and
/// which a planted layer uses to borrow a real user on the node. Everything else
/// is returned unchanged — setuid-root (the image's `su`/`mount`), setgid (so
/// `wall`/`crontab`, setgid to a group, keep working), and sticky. In rootless
/// mode nothing is dropped at all, so the ADR-0062 path is byte-for-byte as it
/// was. Pure, so the policy is tested without being root.
pub(crate) fn sanitize_special_bits(mode: u32, uid: u64, real_root: bool) -> u32 {
    if real_root && uid != 0 && mode & 0o4000 != 0 {
        mode & !0o4000
    } else {
        mode
    }
}

/// Drops a foreign setuid bit from a freshly unpacked regular file (the extraction
/// already wrote the full mode under `set_preserve_permissions`). A no-op unless
/// [`sanitize_special_bits`] changes the mode, so only the real-root, non-root-uid
/// setuid case ever touches disk. Never through a symlink, which has no mode.
/// Writes the kernel-overlay whiteout for an OCI `.wh.<name>` entry: a 0/0
/// character device at the hidden entry's path, which the overlay driver reads
/// as "deleted — do not show the lower layers' version". Any entry already at
/// `at` within this freshly-extracted layer is removed first. Best-effort: on a
/// host that denies the caller `mknod` of a device node the whiteout is not
/// created and the lower entry stays visible — exactly the behaviour before this
/// conversion, never worse.
fn write_overlay_whiteout(at: &Path) {
    use nix::sys::stat::{mknod, Mode, SFlag};
    if let Some(parent) = at.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(at);
    let _ = std::fs::remove_dir_all(at);
    // Character device, major 0 minor 0 — the overlay whiteout representation.
    let _ = mknod(at, SFlag::S_IFCHR, Mode::empty(), 0);
}

fn drop_foreign_setuid<R: Read>(file: &tar::Entry<'_, R>, dst: &Path, real_root: bool) {
    use std::os::unix::fs::PermissionsExt;
    if !real_root || !file.header().entry_type().is_file() {
        return;
    }
    let Ok(mode) = file.header().mode() else {
        return;
    };
    let uid = file.header().uid().unwrap_or(0);
    let sanitized = sanitize_special_bits(mode, uid, real_root);
    if sanitized == mode {
        return;
    }
    let Some(rel) = file.path().ok().and_then(|p| relative(&p)) else {
        return;
    };
    let path = dst.join(rel);
    let real = std::fs::symlink_metadata(&path).is_ok_and(|m| !m.file_type().is_symlink());
    if real {
        let _ =
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(sanitized & 0o7777));
    }
}

/// Gives an unpacked entry its setuid/setgid/sticky bits back. Best-effort, and
/// never through a symlink: a link has no mode, and `chmod` would follow it out
/// of the layer. On a real-root node a foreign setuid bit is not restored
/// (`sanitize_special_bits`) — an older engine had already dropped it.
fn restore_special_mode<R: Read>(entry: &tar::Entry<'_, R>, dir: &Path, real_root: bool) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(mode) = entry.header().mode() else {
        return;
    };
    let kind = entry.header().entry_type();
    if mode & 0o7000 == 0 || !(kind.is_file() || kind.is_dir()) {
        return;
    }
    let mode = sanitize_special_bits(mode, entry.header().uid().unwrap_or(0), real_root);
    if mode & 0o7000 == 0 {
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

    /// The byte ceiling is on what is READ, so a stream of exactly the ceiling is
    /// not a bomb: only the byte after it is.
    #[test]
    fn a_stream_of_exactly_the_ceiling_is_accepted() {
        let data = vec![7u8; 4096];
        let mut r = LimitReader::new(&data[..], 4096);
        let mut sink = Vec::new();
        io::copy(&mut r, &mut sink).expect("exactly the ceiling is within it");
        assert_eq!(sink.len(), 4096);

        let mut r = LimitReader::new(&data[..], 4095);
        let err = io::copy(&mut r, &mut Vec::new()).unwrap_err();
        assert!(exceeded(&err).is_some_and(|l| l.kind == LimitKind::Bytes));
    }

    /// The configuration grammar: a positive size with an optional binary suffix;
    /// everything else is rejected, and a rejected value falls back to the
    /// default and NEVER to "no limit".
    #[test]
    fn limits_come_from_the_environment_and_a_bad_value_keeps_the_default() {
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size(" 2k "), Some(2048));
        assert_eq!(parse_size("1G"), Some(1 << 30));
        assert_eq!(parse_size("3t"), Some(3 << 40));
        for bad in [
            "",
            "0",
            "0k",
            "-1",
            "1.5g",
            "10x",
            "g",
            "99999999999999999999",
            "18446744073709551615t",
        ] {
            assert_eq!(parse_size(bad), None, "{bad:?}");
        }

        let none = LayerLimits::from_lookup(|_| None);
        assert_eq!(none, LayerLimits::DEFAULT);

        let set = LayerLimits::from_lookup(|name| match name {
            BYTES_ENV => Some("512m".into()),
            ENTRIES_ENV => Some("1000".into()),
            _ => None,
        });
        assert_eq!(set.max_bytes, 512 << 20);
        assert_eq!(set.max_entries, 1000);

        // 0, junk and blanks do not switch the bound off.
        for bad in ["0", "unlimited", "-1", "", "  "] {
            let got = LayerLimits::from_lookup(|_| Some(bad.to_string()));
            assert_eq!(got, LayerLimits::DEFAULT, "{bad:?}");
        }
    }

    /// The limit error has to be recoverable from where it really surfaces: inside
    /// `tar`'s own `failed to unpack` wrapper, when the byte ceiling fires in the
    /// middle of a file's data. A string match would not survive a reworded
    /// message; the downcast does.
    #[test]
    fn the_limit_is_recognised_through_the_tar_error_wrapper() {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_size(64 * 1024);
        b.append_data(&mut h, "big", io::repeat(0).take(64 * 1024))
            .unwrap();
        let data = b.into_inner().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let err = unpack_recording_limited(
            &mut tar::Archive::new(LimitReader::new(&data[..], 8 * 1024)),
            tmp.path(),
            u64::MAX,
        )
        .unwrap_err();
        let limit = exceeded(&err).unwrap_or_else(|| panic!("not recognised: {err:?}"));
        assert_eq!(limit.kind, LimitKind::Bytes);
        assert_eq!(limit.limit, 8 * 1024);
        // An unrelated I/O error is not mistaken for a limit.
        assert!(exceeded(&io::Error::other("disk on fire")).is_none());
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

    /// A `.wh.<name>` entry is turned into an overlay whiteout (a 0/0 char
    /// device at `<name>`), never unpacked as a literal `.wh.<name>` file the
    /// kernel overlay driver would ignore. That literal-file behaviour was the
    /// bug that let a Debian layer's `a2dismod mpm_event` leave the lower layer's
    /// `mpm_event.load` visible, so Apache loaded two MPMs and refused to start.
    #[test]
    fn a_whiteout_entry_becomes_an_overlay_char_device() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let tmp = tempfile::tempdir().unwrap();
        let data = tar_of(&[
            ("etc/apache2/mods-enabled/", 0, 0, true),
            ("etc/apache2/mods-enabled/.wh.mpm_event.load", 0, 0, false),
        ]);
        unpack_recording(&mut tar::Archive::new(&data[..]), tmp.path()).unwrap();
        // The literal marker is never left behind as a visible file — that leak
        // was exactly what the kernel overlay ignored.
        assert!(
            tmp.path()
                .join("etc/apache2/mods-enabled/.wh.mpm_event.load")
                .symlink_metadata()
                .is_err(),
            "the .wh. marker leaked into the layer as a regular file"
        );
        // Where mknod of a device node is permitted (the normal engine host),
        // the whiteout stands as the 0/0 char device the overlay driver honours.
        // On a host that denies it, the path is simply absent — no worse than the
        // old leak, and the assertion above still holds.
        if let Ok(md) = tmp
            .path()
            .join("etc/apache2/mods-enabled/mpm_event.load")
            .symlink_metadata()
        {
            assert!(
                md.file_type().is_char_device(),
                "overlay whiteout must be a character device"
            );
            assert_eq!(md.rdev(), 0, "overlay whiteout must be device 0:0");
        }
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

    /// Only a foreign setuid on a real-root node is dropped; setuid-root, setgid
    /// (to any gid), sticky and the whole rootless path are left exactly alone.
    #[test]
    fn only_a_foreign_setuid_on_a_real_root_node_is_dropped() {
        // setuid-root: the image's `su`/`mount` — kept, so pods keep working.
        assert_eq!(sanitize_special_bits(0o4755, 0, true), 0o4755);
        // setuid owned by a named host uid: dropped, nothing else touched.
        assert_eq!(sanitize_special_bits(0o4755, 1000, true), 0o0755);
        // setuid + setgid owned by a non-root uid: only setuid goes.
        assert_eq!(sanitize_special_bits(0o6755, 1000, true), 0o2755);
        // setgid to a group (wall/crontab) and sticky (/tmp): never dropped.
        assert_eq!(sanitize_special_bits(0o2755, 1000, true), 0o2755);
        assert_eq!(sanitize_special_bits(0o1777, 0, true), 0o1777);
        // Rootless: the ADR-0062 path, byte-for-byte unchanged whatever the uid.
        assert_eq!(sanitize_special_bits(0o4755, 1000, false), 0o4755);
        assert_eq!(sanitize_special_bits(0o4755, 0, false), 0o4755);
    }
}
