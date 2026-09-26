//! Content-Addressable Store (CAS) — blobs identified by the sha256 of their
//! content, in `root/blobs/sha256/<hex>`. The **name** of a blob is the hash of
//! what it contains (the same principle as git and the OCI registry).

use crate::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

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
            let tmp = self.dir().join(format!(".{hex}.tmp"));
            fs::write(&tmp, data)?;
            fs::rename(&tmp, &dst)?;
        }
        Ok(format!("sha256:{hex}"))
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
