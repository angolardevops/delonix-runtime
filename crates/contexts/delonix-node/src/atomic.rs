//! Durable file writes shared by every layer that writes a file of its own
//! (P4b.4b of ADR-0044): one copy of the discipline — a temp name unique per
//! writer, the content durable before the rename, the rename durable, the mode
//! set at creation — so a provider crate, which may not depend on the state
//! adapter, does not grow a second copy of it.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A name fragment unique to THIS call, in this process: `<pid>.<sequence>`.
///
/// For a caller that stages its own file under a name it builds from a
/// request/resource identity (not a bare temp name `write_atomic_mode` already
/// handles) — `operations::begin`'s exclusive-creation staging file is the
/// first such caller. Sharing this crate's own counter, rather than each
/// caller keeping a private one, is the same "one copy of the discipline"
/// this module's doc comment already asks for: a second private
/// `AtomicU64` here would be exactly the kind of duplicate this file exists
/// to prevent.
pub fn unique_tmp_suffix() -> String {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}.{}", std::process::id(), seq)
}

/// [`write_atomic`] with an explicit file mode, set **atomically at creation**.
///
/// For anything secret this is the only correct form. The alternative —
/// `fs::write` then `set_permissions` — creates the file under the ambient
/// umask and narrows it afterwards, leaving a window in which another local
/// user can open it. That is exactly the residual TOCTOU an earlier audit found
/// in the kubeconfig path and closed with `OpenOptions::mode`; the secret store
/// had the same shape and had not been converted.
pub fn write_atomic_mode(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".to_string());
    // Unique per WRITER (pid + sequence): a fixed temp name lets two processes —
    // or two threads of the CRI server — interleave their bytes in the same
    // temp, and then `rename` faithfully publishes the corruption.
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{stem}.{}.{seq}.tmp", std::process::id()));

    let write = || -> std::io::Result<()> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        if let Some(m) = mode {
            opts.mode(m); // atomic at creation — never widen-then-narrow
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        // THE ORDER IS THE POINT: the content must be durable BEFORE the
        // directory entry that publishes it exists.
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        // And the rename itself must be durable, or a crash can lose the entry
        // even though the file's blocks are safely on disk.
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    };
    let r = write();
    if r.is_err() {
        let _ = fs::remove_file(&tmp); // never leave junk behind on failure
    }
    r
}
