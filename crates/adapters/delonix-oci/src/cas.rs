//! Content-Addressable Store (CAS) — blobs identified by the sha256 of their
//! content, in `root/blobs/sha256/<hex>`. The **name** of a blob is the hash of
//! what it contains (the same principle as git and the OCI registry).

use crate::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Computes the sha256 of `data` in hexadecimal (without the `sha256:` prefix).
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Strips the `sha256:` prefix from a digest.
pub fn strip(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

/// The content-addressed store.
pub struct Cas {
    root: PathBuf,
}

impl Cas {
    /// Opens (creating) the CAS rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("blobs").join("sha256"))?;
        Ok(Self { root })
    }

    fn dir(&self) -> PathBuf {
        self.root.join("blobs").join("sha256")
    }

    /// The path of the blob with this digest.
    pub fn path(&self, digest: &str) -> PathBuf {
        self.dir().join(strip(digest))
    }

    /// `true` if the blob is already in the store (basis of dedup and caching).
    pub fn has(&self, digest: &str) -> bool {
        self.path(digest).exists()
    }

    /// Writes `data` and returns `sha256:<hex>`. Deduplicates.
    pub fn write(&self, data: &[u8]) -> Result<String> {
        let hex = sha256_hex(data);
        let dst = self.dir().join(&hex);
        if !dst.exists() {
            let tmp = self.tmp_path();
            fs::write(&tmp, data)?;
            self.adopt(&tmp, &hex)?;
        }
        Ok(format!("sha256:{hex}"))
    }

    /// A fresh, unique scratch path inside the store — on the same
    /// filesystem, so the final `rename` is atomic. The name was `.<hex>.tmp`,
    /// shared by every process: two pulls of the same layer wrote the same
    /// file at once.
    pub fn tmp_path(&self) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        self.dir()
            .join(format!(".dl-{}-{n}.tmp", std::process::id()))
    }

    /// Moves a scratch file written by this process into place as the blob
    /// `hex`. The caller has already checked that the content hashes to
    /// `hex`; if the blob is there already (another writer won the race) the
    /// scratch file is dropped, since the content is identical by definition.
    pub fn adopt(&self, tmp: &Path, hex: &str) -> Result<()> {
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let _ = fs::remove_file(tmp);
            return Err(crate::Error::DigestMismatch(format!(
                "not a sha256 digest: {hex}"
            )));
        }
        let dst = self.dir().join(hex);
        if dst.exists() {
            let _ = fs::remove_file(tmp);
            return Ok(());
        }
        fs::rename(tmp, &dst)?;
        Ok(())
    }

    /// Reads the content of a blob by its digest.
    pub fn read(&self, digest: &str) -> Result<Vec<u8>> {
        Ok(fs::read(self.path(digest))?)
    }

    /// Size of a blob in bytes, without reading it.
    pub fn size(&self, digest: &str) -> Result<u64> {
        Ok(fs::metadata(self.path(digest))?.len())
    }

    /// The first `n` bytes of a blob (fewer if the blob is shorter). Enough to
    /// sniff a compression magic number without pulling a whole layer into
    /// memory — a manifest needs the size and the media type, never the bytes.
    pub fn head(&self, digest: &str, n: usize) -> Result<Vec<u8>> {
        use std::io::Read;
        let mut buf = Vec::with_capacity(n);
        fs::File::open(self.path(digest))?
            .take(n as u64)
            .read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// Verifies integrity: `sha256(content) == digest`.
    pub fn verify(&self, digest: &str) -> Result<bool> {
        let data = self.read(digest)?;
        Ok(sha256_hex(&data) == strip(digest))
    }
}

/// A blob being written to disk as it arrives, hashed on the way through.
/// Each byte is written once and never held whole in memory — the download
/// path buffered every layer in a `Vec` first, so the RSS of a pull was the
/// sum of the layers in flight and nothing reached the disk until the last
/// byte had arrived.
///
/// The disk writes happen on a THREAD of their own. Measured: writing from the
/// download thread made a pull of `node:22` about twice as slow on a disk
/// under I/O pressure — every blocked `write(2)` stalled the socket read that
/// followed it, the TCP window shrank and the sender backed off. The network
/// now only waits for the disk when [`STREAM_QUEUE`] blocks of
/// [`STREAM_BUF`] are already queued, which also caps the memory per blob.
pub struct StreamingBlob {
    tx: Option<std::sync::mpsc::SyncSender<WriteOp>>,
    writer: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    pending: Vec<u8>,
    hasher: Sha256,
    len: u64,
}

/// Bytes a [`StreamingBlob`] gathers before handing one block to its writer.
const STREAM_BUF: usize = 1 << 20;
/// Blocks that may wait for the disk before the download waits too.
const STREAM_QUEUE: usize = 16;

enum WriteOp {
    Data(Vec<u8>),
    /// Start the file over: the server ignored a `Range` request.
    Truncate,
}

impl StreamingBlob {
    /// Creates `path` (it must not exist: a scratch path from
    /// [`Cas::tmp_path`], never a blob's final name).
    pub fn create(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Self::spawn(file, Sha256::new(), 0))
    }

    fn spawn(mut file: fs::File, hasher: Sha256, len: u64) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<WriteOp>(STREAM_QUEUE);
        let writer = std::thread::spawn(move || -> std::io::Result<()> {
            use std::io::Seek;
            for op in rx {
                match op {
                    WriteOp::Data(block) => file.write_all(&block)?,
                    WriteOp::Truncate => {
                        file.set_len(0)?;
                        file.rewind()?;
                    }
                }
            }
            file.flush()
        });
        Self {
            tx: Some(tx),
            writer: Some(writer),
            pending: Vec::with_capacity(STREAM_BUF),
            hasher,
            len,
        }
    }

    /// Hands one operation to the writer. A closed channel means the writer
    /// stopped on an I/O error; that error is what is returned.
    fn send(&mut self, op: WriteOp) -> Result<()> {
        let sent = self.tx.as_ref().is_some_and(|tx| tx.send(op).is_ok());
        if sent {
            Ok(())
        } else {
            Err(self.stop_writer().err().unwrap_or_else(|| {
                std::io::Error::other("blob writer stopped unexpectedly").into()
            }))
        }
    }

    /// Closes the channel and waits for the writer, returning its outcome.
    fn stop_writer(&mut self) -> Result<()> {
        self.tx = None;
        match self.writer.take().map(|h| h.join()) {
            None | Some(Ok(Ok(()))) => Ok(()),
            Some(Ok(Err(e))) => Err(e.into()),
            Some(Err(_)) => Err(std::io::Error::other("blob writer panicked").into()),
        }
    }

    /// Bytes received so far — what a resumed download continues from.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// `true` before the first byte.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Starts over from zero, for when the server ignored a `Range` request
    /// and is sending the whole blob again.
    pub fn reset(&mut self) -> Result<()> {
        // What is still pending belongs to the attempt being thrown away.
        self.pending.clear();
        self.send(WriteOp::Truncate)?;
        self.hasher = Sha256::new();
        self.len = 0;
        Ok(())
    }

    /// Appends the next chunk.
    pub fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.hasher.update(bytes);
        self.len += bytes.len() as u64;
        self.pending.extend_from_slice(bytes);
        if self.pending.len() >= STREAM_BUF {
            let block = std::mem::replace(&mut self.pending, Vec::with_capacity(STREAM_BUF));
            self.send(WriteOp::Data(block))?;
        }
        Ok(())
    }

    /// Writes what is left, waits for the disk, and returns the hex sha256
    /// of everything written.
    pub fn finish(mut self) -> Result<String> {
        if !self.pending.is_empty() {
            let block = std::mem::take(&mut self.pending);
            self.send(WriteOp::Data(block))?;
        }
        self.stop_writer()?;
        let hasher = std::mem::take(&mut self.hasher);
        Ok(format!("{:x}", hasher.finalize()))
    }
}

impl Drop for StreamingBlob {
    /// A download that failed still stops its writer before the caller
    /// removes the scratch file.
    fn drop(&mut self) {
        let _ = self.stop_writer();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_and_head_do_not_need_the_whole_blob() {
        let dir = std::env::temp_dir().join(format!("delonix-cas-head-{}", std::process::id()));
        let cas = Cas::open(&dir).unwrap();
        let dg = cas.write(&[0x1f, 0x8b, 7, 7, 7, 7]).unwrap();
        assert_eq!(cas.size(&dg).unwrap(), 6);
        assert_eq!(cas.head(&dg, 4).unwrap(), vec![0x1f, 0x8b, 7, 7]);
        // Shorter than asked: returns what there is, never an error.
        assert_eq!(cas.head(&dg, 64).unwrap().len(), 6);
        assert!(cas.size("sha256:0000").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
