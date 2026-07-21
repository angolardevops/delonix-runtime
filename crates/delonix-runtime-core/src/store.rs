//! Container state store — one JSON file per container.
//!
//! Reuses the JSON *snapshot* pattern of the `kvstore` (Month 3): each container
//! is persisted in `root/<id>.json`, with atomic writes (temporary file +
//! `rename`).

use crate::{Container, Error, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs;
use std::marker::PhantomData;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Sequence to make the temporary file of [`Store::save`] unique PER
/// WRITER. The pid alone is not enough: the CRI server is multi-threaded
/// (`tokio::spawn_blocking`), so two threads of the SAME process could
/// collide on the same temp.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Exclusive file lock (`flock`) — sequences the **read-modify-write**
/// of a container BETWEEN PROCESSES. Same pattern as `delonix-net::infra`.
///
/// Why it is needed: this runtime is daemonless — N processes (`delonix` on the CLI,
/// the `delonix-cri` server that the kubelet calls, and this one is CONCURRENT by
/// design) mutate the same JSON. The atomic write (temp+`rename`) avoids
/// TORN files, but does not avoid the classic **lost update**: two readers
/// read the same state, both modify, both write — one of the changes
/// disappears silently (e.g.: a `RemoveContainer` undone by a concurrent
/// reconcile that rewrites the old record).
struct FileLock(fs::File);

impl FileLock {
    /// Acquires the lock (blocks until it gets it). `None` if the lock file
    /// cannot even be opened — in that case the caller proceeds without a lock
    /// (graceful degradation: better than refusing the operation).
    fn acquire(path: &Path) -> Option<FileLock> {
        let f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .ok()?;
        // SAFETY: valid, open fd; LOCK_EX blocks until the lock is ours.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return None;
        }
        Some(FileLock(f))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: fd still open (we own the File until here). The File's `close`
        // would also release the flock; explicit so as not to depend on that.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Sanitizes a key/id into a safe file name (`a-z0-9._-`,
/// preserving uppercase). Blocks path traversal (`../`, `/etc/passwd`,
/// separators) by mapping any character outside that allowlist to `-`.
/// Shared by [`Store`] and [`JsonStore`] — **every** id/key coming from outside
/// (e.g.: `Path<String>` of axum handlers in `delonix-api`) must pass through
/// here before entering a `PathBuf::join`.
pub(crate) fn safe_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The state store, rooted in a directory.
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Opens (creating) the store in the `root` directory.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// `$DELONIX_ROOT/containers`, or — **rootless** (without privileges) — the user's
    /// store (`$XDG_DATA_HOME/delonix` or `~/.local/share/delonix`), or
    /// `/var/lib/delonix/containers` when root. Consistent with
    /// `ImageStore::default_root` so rootless `run` works without `sudo`.
    pub fn default_root() -> PathBuf {
        if let Some(root) = std::env::var_os("DELONIX_ROOT") {
            return PathBuf::from(root).join("containers");
        }
        // SAFETY: geteuid() is always safe and does not fail.
        if unsafe { libc::geteuid() } != 0 {
            let base = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
                .unwrap_or_else(|| PathBuf::from("."));
            return base.join("delonix").join("containers");
        }
        PathBuf::from("/var/lib/delonix/containers")
    }

    /// The base directory (`$DELONIX_ROOT`) — the parent of `containers`. Used by
    /// subsystems that live alongside (e.g.: [`crate::SecretStore`] in `<base>/secrets`).
    pub fn base(&self) -> PathBuf {
        self.root
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.root.clone())
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{}.json", safe_key(id)))
    }

    /// Lock file of a container (see [`FileLock`]). It lives alongside the
    /// state and is NEVER deleted — deleting it would open a window in which two
    /// processes lock different inodes and both enter the critical section.
    fn lock_path(&self, id: &str) -> PathBuf {
        self.root.join(format!(".{}.lock", safe_key(id)))
    }

    /// Persists a container (atomic write).
    ///
    /// The temporary is unique **per writer** (pid + sequence): with a
    /// fixed name (`.<id>.tmp`), two processes writing the SAME container would write
    /// over each other in the same file and the `rename` would publish an
    /// interleaved JSON — the atomicity of the `rename` saves nothing if the temp's
    /// content already comes corrupted.
    pub fn save(&self, c: &Container) -> Result<()> {
        let safe = safe_key(&c.id);
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .root
            .join(format!(".{safe}.{}.{seq}.tmp", std::process::id()));
        let write = || -> Result<()> {
            fs::write(&tmp, serde_json::to_vec_pretty(c)?)?;
            fs::rename(&tmp, self.path(&c.id))?;
            Ok(())
        };
        let r = write();
        if r.is_err() {
            let _ = fs::remove_file(&tmp); // do not leave junk if it failed half-way
        }
        r
    }

    /// **Safe read-modify-write** of a container: locks (`flock`), re-reads the
    /// state ALREADY under the lock, applies `f` and writes — all as one critical
    /// section between processes.
    ///
    /// Use this (and not `load` + mutate + `save`) whenever the change depends
    /// on the CURRENT state. The naive pattern loses writes when the CRI (which is
    /// concurrent) and the CLI touch the same container at the same time.
    ///
    /// `f` returns `false` to abort the write (nothing changes). The container
    /// returned is the final state (or the one read, if it aborted).
    pub fn update<F>(&self, id_or_name: &str, f: F) -> Result<Container>
    where
        F: FnOnce(&mut Container) -> bool,
    {
        // Resolve the REAL id first (accepts prefix/name), to always lock
        // the same lock file regardless of how it was referenced.
        let id = self.load(id_or_name)?.id;
        let _lock = FileLock::acquire(&self.lock_path(&id));
        // Re-read UNDER the lock: between the resolve and the `flock` another process may have
        // written; using the value read before would reintroduce the lost update.
        let mut c = self.load(&id)?;
        if !f(&mut c) {
            return Ok(c);
        }
        self.save(&c)?;
        Ok(c)
    }

    /// Loads a container by exact id, id prefix, or name.
    pub fn load(&self, id_or_name: &str) -> Result<Container> {
        let exact = self.path(id_or_name);
        if exact.exists() {
            return Ok(serde_json::from_slice(&fs::read(exact)?)?);
        }
        for c in self.list()? {
            if c.id.starts_with(id_or_name) || c.name == id_or_name {
                return Ok(c);
            }
        }
        Err(Error::NotFound(id_or_name.to_string()))
    }

    /// Lists all containers, from most recent to oldest.
    pub fn list(&self) -> Result<Vec<Container>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(bytes) = fs::read(&path) {
                    if let Ok(c) = serde_json::from_slice::<Container>(&bytes) {
                        out.push(c);
                    }
                }
            }
        }
        out.sort_by_key(|c| std::cmp::Reverse(c.created_unix));
        Ok(out)
    }

    /// Removes the state file of a container.
    pub fn remove(&self, id: &str) -> Result<()> {
        let p = self.path(id);
        if !p.exists() {
            return Err(Error::NotFound(id.to_string()));
        }
        fs::remove_file(p)?;
        Ok(())
    }
}

/// Generic typed store — one JSON file per item, indexed by a key
/// (name). Reuses the same atomic pattern (temp + `rename`) as [`Store`],
/// for types that are not `Container`: VMs ([`crate::Vm`]) and the applied
/// manifests (desired state of the `reconcile` daemon).
pub struct JsonStore<T> {
    root: PathBuf,
    _t: PhantomData<T>,
}

impl<T: Serialize + DeserializeOwned> JsonStore<T> {
    /// Opens (creating) the store in the `root` directory.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            _t: PhantomData,
        })
    }

    fn path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{}.json", safe_key(key)))
    }

    /// Persists an item under `key` (atomic write).
    pub fn save(&self, key: &str, value: &T) -> Result<()> {
        let safe = safe_key(key);
        // Temp unique per writer — see the note in `Store::save`.
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .root
            .join(format!(".{safe}.{}.{seq}.tmp", std::process::id()));
        fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
        fs::rename(&tmp, self.path(key))?;
        Ok(())
    }

    /// Loads an item by exact key.
    pub fn load(&self, key: &str) -> Result<T> {
        let p = self.path(key);
        if !p.exists() {
            return Err(Error::NotFound(key.to_string()));
        }
        Ok(serde_json::from_slice(&fs::read(p)?)?)
    }

    /// `true` if an item with this key exists.
    pub fn exists(&self, key: &str) -> bool {
        self.path(key).exists()
    }

    /// Lists all items (filesystem order).
    pub fn list(&self) -> Result<Vec<T>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(bytes) = fs::read(&path) {
                    if let Ok(v) = serde_json::from_slice::<T>(&bytes) {
                        out.push(v);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Removes the item of a key (idempotent: absence is not an error).
    pub fn remove(&self, key: &str) -> Result<()> {
        let p = self.path(key);
        if p.exists() {
            fs::remove_file(p)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Container;

    fn tmp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "delonix-store-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn safe_key_neutraliza_path_traversal() {
        // `.` is an allowed character (legitimate ids/names have dots), but `/`
        // is ALWAYS replaced — so "../" never survives as a separator:
        // the result is always ONE SINGLE file name component, even if it
        // contains ".." like a substring. `PathBuf::join` only interprets ".."
        // as traversal when it is a whole component (delimited by `/`);
        // within a single component without `/`, it is just harmless text.
        assert_eq!(safe_key("../../etc/passwd"), "..-..-etc-passwd");
        assert_eq!(safe_key("a/../../b"), "a-..-..-b");
        assert!(!safe_key("../../../root/.ssh/authorized_keys").contains('/'));
        // normal ids (hex/uuid) pass through intact — no behavior regression.
        assert_eq!(safe_key("a1b2c3d4e5f6"), "a1b2c3d4e5f6");
        assert_eq!(safe_key("my-container_v1.2"), "my-container_v1.2");
    }

    #[test]
    fn store_path_traversal_nunca_escreve_fora_da_raiz() {
        let root = tmp_dir("store-path");
        let outside = root
            .parent()
            .unwrap()
            .join(format!("delonix-store-test-VICTIM-{}", std::process::id()));
        let store = Store::open(&root).unwrap();

        // a malicious "id" coming from an unvalidated HTTP handler.
        let evil_id = format!(
            "../{}/pwned",
            outside.file_name().unwrap().to_str().unwrap()
        );
        let c = Container::new(
            evil_id.clone(),
            "x".into(),
            "img".into(),
            vec![],
            "256M".into(),
        );
        store.save(&c).unwrap();

        // the file MUST stay inside `root` — never in `outside`.
        assert!(
            !outside.exists(),
            "save com id malicioso escreveu FORA da raiz do Store"
        );
        let entries: Vec<_> = fs::read_dir(&root).unwrap().flatten().collect();
        assert_eq!(
            entries.len(),
            1,
            "devia existir exactamente 1 ficheiro dentro da raiz sanitizada"
        );
        assert!(
            entries[0]
                .path()
                .to_string_lossy()
                .starts_with(root.to_string_lossy().as_ref()),
            "ficheiro escrito fora da raiz esperada"
        );

        // load/remove with the SAME malicious id still resolve to inside
        // the root (consistency: save/load/remove sanitize the same way).
        let loaded = store.load(&evil_id).unwrap();
        assert_eq!(
            loaded.id, evil_id,
            "o conteúdo persistido continua correcto (só o PATH em disco é sanitizado)"
        );
        store.remove(&evil_id).unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().flatten().count(), 0);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn jsonstore_path_traversal_tambem_neutralizado() {
        let root = tmp_dir("jsonstore-path");
        let store: JsonStore<String> = JsonStore::open(&root).unwrap();
        let evil_key = "../../../tmp/pwned-jsonstore";
        store.save(evil_key, &"conteudo".to_string()).unwrap();

        let entries: Vec<_> = fs::read_dir(&root).unwrap().flatten().collect();
        assert_eq!(
            entries.len(),
            1,
            "JsonStore também tem de manter tudo dentro da raiz"
        );
        assert!(store.load(evil_key).is_ok());

        let _ = fs::remove_dir_all(&root);
    }

    /// REGRESSION (concurrency): `update` sequences read-modify-write between
    /// threads. Without the `flock`, N concurrent increments are lost (lost
    /// update) and the final total comes out < N. With the lock, it must be exactly N.
    #[test]
    fn update_concorrente_nao_perde_escritas() {
        let root = tmp_dir("store-update-race");
        let store = Store::open(&root).unwrap();
        let mut c = Container::new(
            "race1".into(),
            "race1".into(),
            "img".into(),
            vec!["x".into()],
            "max".into(),
        );
        c.labels.insert("n".into(), "0".into());
        store.save(&c).unwrap();

        const N: usize = 24;
        std::thread::scope(|sc| {
            for _ in 0..N {
                let root = root.clone();
                sc.spawn(move || {
                    let st = Store::open(&root).unwrap();
                    st.update("race1", |c| {
                        let n: u64 = c.labels.get("n").unwrap().parse().unwrap();
                        // Explicit race window between the read and the write:
                        // without a lock, guarantees the lost update.
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        c.labels.insert("n".into(), (n + 1).to_string());
                        true
                    })
                    .unwrap();
                });
            }
        });

        let got: usize = store
            .load("race1")
            .unwrap()
            .labels
            .get("n")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(got, N, "perderam-se escritas: {got} de {N}");
        let _ = fs::remove_dir_all(&root);
    }

    /// REGRESSION: the `save` temporary must be unique per writer. With a
    /// fixed name (`.<id>.tmp`), concurrent writes of the SAME container
    /// interleaved in the temp and the `rename` published corrupted JSON.
    #[test]
    fn save_concorrente_nunca_publica_json_corrompido() {
        let root = tmp_dir("store-save-race");
        let store = Store::open(&root).unwrap();
        let base = Container::new(
            "race2".into(),
            "race2".into(),
            "img".into(),
            vec!["x".into()],
            "max".into(),
        );
        store.save(&base).unwrap();

        std::thread::scope(|sc| {
            for i in 0..16 {
                let root = root.clone();
                sc.spawn(move || {
                    let st = Store::open(&root).unwrap();
                    let mut c = Container::new(
                        "race2".into(),
                        format!("nome-{}", "a".repeat(i * 7)), // different sizes = visible interleaving
                        "img".into(),
                        vec!["x".into()],
                        "max".into(),
                    );
                    c.labels.insert("k".into(), "v".repeat(i * 11));
                    st.save(&c).unwrap();
                    // Each read must ALWAYS see a valid JSON.
                    st.load("race2")
                        .expect("JSON corrompido publicado pelo rename");
                });
            }
        });

        store
            .load("race2")
            .expect("estado final tem de ser um JSON válido");
        let _ = fs::remove_dir_all(&root);
    }
}
