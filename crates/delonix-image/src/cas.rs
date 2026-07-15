//! Content-Addressable Store (CAS) — blobs identificados pelo sha256 do seu
//! conteúdo, em `root/blobs/sha256/<hex>`. O **nome** de um blob é o hash do
//! que ele contém (o mesmo princípio do git e do registo OCI).

use delonix_runtime_core::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

/// Calcula o sha256 de `data` em hexadecimal (sem o prefixo `sha256:`).
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Remove o prefixo `sha256:` de um digest.
pub fn strip(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

/// O armazém content-addressed.
pub struct Cas {
    root: PathBuf,
}

impl Cas {
    /// Abre (criando) o CAS enraizado em `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("blobs").join("sha256"))?;
        Ok(Self { root })
    }

    fn dir(&self) -> PathBuf {
        self.root.join("blobs").join("sha256")
    }

    /// O caminho do blob com este digest.
    pub fn path(&self, digest: &str) -> PathBuf {
        self.dir().join(strip(digest))
    }

    /// `true` se o blob já está no armazém (base da dedup e da cache).
    pub fn has(&self, digest: &str) -> bool {
        self.path(digest).exists()
    }

    /// Escreve `data` e devolve `sha256:<hex>`. Deduplica.
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

    /// Lê o conteúdo de um blob pelo digest.
    pub fn read(&self, digest: &str) -> Result<Vec<u8>> {
        Ok(fs::read(self.path(digest))?)
    }

    /// Verifica a integridade: `sha256(conteúdo) == digest`.
    pub fn verify(&self, digest: &str) -> Result<bool> {
        let data = self.read(digest)?;
        Ok(sha256_hex(&data) == strip(digest))
    }
}
