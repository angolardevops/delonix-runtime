//! Content-Addressable Store (CAS) — blobs identified by the sha256 of their
//! content, in `root/blobs/sha256/<hex>`. The **name** of a blob is the hash of
//! what it contains (the same principle as git and the OCI registry).

use delonix_runtime_core::Result;
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

    /// Verifies integrity: `sha256(content) == digest`.
    pub fn verify(&self, digest: &str) -> Result<bool> {
        let data = self.read(digest)?;
        Ok(sha256_hex(&data) == strip(digest))
    }
}
