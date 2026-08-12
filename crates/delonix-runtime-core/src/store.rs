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

/// Writes `bytes` to `path` so that a reader NEVER sees a half-written file and
/// a **crash** never leaves one behind: temp in the same directory → `fsync` the
/// temp → `rename` → `fsync` the directory.
///
/// BUG FIXED HERE. Every store in this workspace wrote state as temp +
/// `rename` and called it an "atomic write". That is only half true, and the
/// missing half is the one that matters after a power loss: `rename(2)` is
/// atomic with respect to concurrent *readers*, but it publishes a directory
/// entry that may point at a file whose CONTENT the kernel has not written out
/// yet. Nothing in the workspace called `fsync` — `grep -rn 'sync_all|fsync'`
/// over all nine crates returned exactly one hit, and it was the `SYS_fsync`
/// constant in the seccomp allowlist.
///
/// The consequence is not theoretical for a daemonless engine whose entire
/// notion of "what exists" lives in these JSON files. The worst case is
/// `delonix-net`'s IPAM lease registry: lose that file and every `id → ip`
/// lease goes with it, dropping the allocator back to the bare hash — which its
/// own module doc measures as colliding with ~50 % probability at ~300
/// containers, i.e. two containers on one IP, with the firewall and DNAT rules
/// indexed on the wrong one.
///
/// ext4's `data=ordered` heuristic (auto-flush on rename-over) hides this most
/// of the time on the common desktop case. It is a heuristic, not a guarantee,
/// and it is not the whole story on XFS/btrfs or on the network/replicated
/// storage this engine is deployed onto.
///
/// The directory `fsync` is best-effort: some filesystems reject `fsync` on a
/// directory fd, and failing the whole write over that would be worse than the
/// slightly weaker guarantee.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_mode(path, bytes, None)
}

/// Writes `bytes` to a NEW file in the system temp directory that a hostile
/// local user can neither pre-create, redirect, nor read. Returns its path; the
/// caller owns it and should delete it when done.
///
/// `std::env::temp_dir()` is world-writable, so `fs::write` to a name another
/// local user can guess is a real vulnerability, not a style issue:
///
/// * `fs::write` FOLLOWS SYMLINKS, so a pre-planted symlink redirects the write
///   to any file the writing process can reach — and some of these callers run
///   as root.
/// * It creates at the ambient umask (0644 on a default install), so the
///   contents are readable while they exist.
/// * If the attacker creates the file first, THEY own it, and in a sticky `/tmp`
///   we cannot unlink it — they can then rewrite it between our write and
///   whatever reads it back. That last one is why this matters most for
///   `delonix-net`'s BPF object: the file is handed to `bpftool prog loadall`,
///   so winning that race means an unprivileged user gets their own BPF program
///   loaded into the kernel by a privileged process.
///
/// `O_EXCL` (via `create_new`) is what closes all three: it refuses to open an
/// existing path and does not follow symlinks, so a pre-planted anything makes
/// us fail rather than obey. On collision we simply try the next name; the mode
/// is set at creation, never widened-then-narrowed.
///
/// The same shape an earlier audit fixed in `ensure_libvirt_network`; these
/// callers had been left behind, which is exactly why it lives here now instead
/// of being written out a fourth time.
pub fn write_private_temp(prefix: &str, bytes: &[u8]) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = std::env::temp_dir();
    let mut last_err: Option<std::io::Error> = None;
    for _ in 0..64 {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(".{prefix}.{}.{seq}.{nanos}", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true) // O_EXCL: refuses an existing path, ignores symlinks
            .mode(0o600)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(bytes)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue; // taken (or squatted) — just pick another name
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(last_err
        .unwrap_or_else(|| std::io::Error::other("could not create a private temp file"))
        .into())
}

/// [`write_atomic`] with an explicit file mode, set **atomically at creation**.
///
/// For anything secret this is the only correct form. The alternative —
/// `fs::write` then `set_permissions` — creates the file under the ambient
/// umask and narrows it afterwards, leaving a window in which another local
/// user can open it. That is exactly the residual TOCTOU an earlier audit found
/// in the kubeconfig path and closed with `OpenOptions::mode`; the secret store
/// had the same shape and had not been converted.
pub fn write_atomic_mode(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
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

    let write = || -> Result<()> {
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

/// Just the fields a lookup by prefix/name needs to DECIDE. `serde` walks past
/// everything else in the JSON without allocating it.
///
/// A `Container` carries mounts, env, labels, ports, firewall rules and more;
/// building all of that for every record just to compare two strings is most of
/// the cost of a lookup, and it is thrown away for every record but one. See
/// [`Store::load`].
#[derive(serde::Deserialize, Clone)]
struct Ident {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default = "crate::default_namespace")]
    namespace: String,
    #[serde(default)]
    created_unix: u64,
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
        write_atomic(&self.path(&c.id), &serde_json::to_vec_pretty(c)?)
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
    ///
    /// Cost note: an exact id is a single `stat`+read. A prefix or a NAME
    /// cannot be resolved from the filename (files are keyed by id), so those
    /// still scan the directory — but they now parse only [`Ident`] per record
    /// instead of constructing a whole `Container`, and only the winner is
    /// fully deserialized. On a host with the 49 containers this engine has
    /// really run, every name-based command was paying full construction 49
    /// times; `Store::update` paid it twice per call.
    ///
    /// Deliberately NOT fixed here: the directory walk itself. Making a name
    /// lookup O(1) needs a name→id index, i.e. a second piece of persistent
    /// state to keep in sync with the records — in a daemonless engine where N
    /// processes mutate the store concurrently, a stale or divergent index is a
    /// worse failure than a linear scan. The ordering semantics are unchanged:
    /// newest first, first match wins.
    /// Also accepts the qualified `<namespace>/<name>` form, and REFUSES a bare
    /// name that exists in several namespaces.
    ///
    /// Names became unique per (namespace, name) rather than globally
    /// (ADR-0011 §3), which is what lets two tenants both own `db`. That made
    /// the old "newest match wins" tie-break unsafe for NAMES: `ingress deny db`
    /// or an `apply` touching `db` would silently pick a tenant. Prefix-of-id
    /// ambiguity keeps the historical tie-break — it is a typo to fix, not two
    /// legitimate owners.
    pub fn load(&self, id_or_name: &str) -> Result<Container> {
        let exact = self.path(id_or_name);
        if exact.exists() {
            return Ok(serde_json::from_slice(&fs::read(exact)?)?);
        }
        let qualified = id_or_name.split_once('/');
        let mut hits: Vec<Ident> = Vec::new();
        let mut named: Vec<Ident> = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(idt) = serde_json::from_slice::<Ident>(&bytes) else {
                continue;
            };
            match qualified {
                Some((ns, name)) => {
                    if idt.namespace == ns && idt.name == name {
                        named.push(idt);
                    }
                }
                None => {
                    if idt.name == id_or_name {
                        named.push(idt.clone());
                    }
                    if idt.id.starts_with(id_or_name) {
                        hits.push(idt);
                    }
                }
            }
        }
        // An exact name wins over an id prefix, as it always has — but only when
        // it names ONE container.
        if named.len() > 1 {
            let mut opts: Vec<String> = named
                .iter()
                .map(|i| format!("{}/{}", i.namespace, i.name))
                .collect();
            opts.sort();
            return Err(Error::Invalid(format!(
                "container name '{id_or_name}' exists in several namespaces ({}) — qualify it as <namespace>/<name>",
                opts.join(", ")
            )));
        }
        if let Some(hit) = named.first() {
            return Ok(serde_json::from_slice(&fs::read(self.path(&hit.id))?)?);
        }
        if qualified.is_some() {
            return Err(Error::NotFound(format!("container: {id_or_name}")));
        }
        // Same tie-break as before (`list()` sorts newest-first and the old loop
        // returned the first match), so an ambiguous prefix keeps resolving to
        // exactly the container it used to.
        hits.sort_by_key(|i| std::cmp::Reverse(i.created_unix));
        match hits.first() {
            Some(hit) => Ok(serde_json::from_slice(&fs::read(self.path(&hit.id))?)?),
            None => Err(Error::NotFound(format!("container: {id_or_name}"))),
        }
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
            return Err(Error::NotFound(format!("container: {id}")));
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

    /// Lock file of an item (see [`FileLock`]). Same convention as
    /// [`Store::lock_path`] — never deleted, to avoid a window where two
    /// processes lock different inodes and both enter the critical section.
    fn lock_path(&self, key: &str) -> PathBuf {
        self.root.join(format!(".{}.lock", safe_key(key)))
    }

    /// **Safe read-modify-write** of an item: locks (`flock`), re-reads the
    /// state ALREADY under the lock, applies `f` and writes — same pattern as
    /// [`Store::update`], generalized to any `JsonStore<T>`.
    ///
    /// Use this (and not `load` + mutate + `save`) whenever the change depends
    /// on the CURRENT state and more than one process may touch the same key
    /// concurrently — e.g. `delonix-vm`'s `status()` (background metrics
    /// refresh) racing a CLI `vm start`/`stop`/`create` on the same VM.
    ///
    /// `f` returns `false` to abort the write (nothing changes). The item
    /// returned is the final state (or the one read, if it aborted).
    pub fn update<F>(&self, key: &str, f: F) -> Result<T>
    where
        F: FnOnce(&mut T) -> bool,
    {
        let _lock = FileLock::acquire(&self.lock_path(key));
        // Re-read UNDER the lock: between any earlier read and the `flock`
        // another process may have written; using a stale value would
        // reintroduce the lost update this exists to prevent.
        let mut v = self.load(key)?;
        if !f(&mut v) {
            return Ok(v);
        }
        self.save(key, &v)?;
        Ok(v)
    }

    /// Persists an item under `key` (atomic write).
    pub fn save(&self, key: &str, value: &T) -> Result<()> {
        write_atomic(&self.path(key), &serde_json::to_vec_pretty(value)?)
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

    /// REGRESSION: a reader must NEVER observe a partially-written file.
    ///
    /// This is the half of `write_atomic`'s contract a unit test can actually
    /// prove. A bare `fs::write` TRUNCATES the target and then fills it, so a
    /// concurrent reader lands in that window and gets a short/empty file —
    /// which, for every store in this workspace, deserializes to nothing and
    /// makes the resource disappear from `ls`/`inspect` while its data stays on
    /// disk. Replacing `write_atomic`'s body with `fs::write(path, bytes)`
    /// makes this test fail.
    ///
    /// **What this test does NOT prove**: crash durability. The `fsync` of the
    /// temp and of the directory cannot be exercised without power-cycling the
    /// machine — see the live `strace` validation recorded in the session notes
    /// for evidence that `fsync(tmp)` really does precede `rename()`.
    #[test]
    fn write_atomic_nunca_deixa_um_leitor_ver_ficheiro_parcial() {
        let root = tmp_dir("write-atomic-torn");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("state.json");

        // Two payloads of very different sizes: a truncating writer leaves the
        // file at every length in between, so a torn read is easy to catch.
        let big = vec![b'a'; 512 * 1024];
        let small = vec![b'b'; 4 * 1024];
        write_atomic(&target, &big).unwrap();

        let stop = std::sync::atomic::AtomicBool::new(false);
        let torn = std::sync::atomic::AtomicUsize::new(0);

        std::thread::scope(|sc| {
            sc.spawn(|| {
                for i in 0..200 {
                    let payload = if i % 2 == 0 { &big } else { &small };
                    write_atomic(&target, payload).unwrap();
                }
                stop.store(true, Ordering::SeqCst);
            });
            for _ in 0..3 {
                sc.spawn(|| {
                    while !stop.load(Ordering::SeqCst) {
                        if let Ok(seen) = fs::read(&target) {
                            // Every published version is one of the two whole
                            // payloads, all-'a' or all-'b'. Anything else is a
                            // read that caught a truncation in progress.
                            let ok = (seen.len() == big.len() && seen.iter().all(|&b| b == b'a'))
                                || (seen.len() == small.len() && seen.iter().all(|&b| b == b'b'));
                            if !ok {
                                torn.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                });
            }
        });

        assert_eq!(
            torn.load(Ordering::SeqCst),
            0,
            "um leitor apanhou o ficheiro a meio de uma escrita"
        );
        // E não ficou lixo: só o alvo, nenhum `.tmp` sobrevivente.
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "state.json")
            .collect();
        assert!(
            leftovers.is_empty(),
            "ficheiros temporários órfãos: {leftovers:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// `load` por prefixo/nome tem de manter EXACTAMENTE a semântica anterior
    /// depois de deixar de construir um `Container` inteiro por registo: id
    /// exacto, prefixo de id, nome, e — o que é fácil de partir sem dar por
    /// isso — o desempate pelo mais RECENTE quando um prefixo casa com vários.
    #[test]
    fn load_resolve_id_exacto_prefixo_e_nome_com_desempate_pelo_mais_recente() {
        let root = tmp_dir("store-load-resolve");
        let store = Store::open(&root).unwrap();

        let mut old = Container::new(
            "abcdef111111".into(),
            "antigo".into(),
            "img".into(),
            vec!["x".into()],
            "max".into(),
        );
        old.created_unix = 1_000;
        store.save(&old).unwrap();

        let mut new = Container::new(
            "abcdef999999".into(),
            "recente".into(),
            "img".into(),
            vec!["x".into()],
            "max".into(),
        );
        new.created_unix = 2_000;
        store.save(&new).unwrap();

        // id exacto
        assert_eq!(store.load("abcdef111111").unwrap().name, "antigo");
        // nome
        assert_eq!(store.load("recente").unwrap().id, "abcdef999999");
        // prefixo não-ambíguo
        assert_eq!(store.load("abcdef9").unwrap().name, "recente");
        // prefixo AMBÍGUO → o mais recente ganha (comportamento de sempre)
        assert_eq!(
            store.load("abcdef").unwrap().id,
            "abcdef999999",
            "um prefixo ambíguo tem de continuar a resolver para o mais recente"
        );
        // o objecto devolvido vem COMPLETO, não só os campos do índice
        let full = store.load("recente").unwrap();
        assert_eq!(full.image, "img");
        assert_eq!(full.command, vec!["x".to_string()]);
        // inexistente
        assert!(matches!(store.load("nao-existe"), Err(Error::NotFound(_))));

        let _ = fs::remove_dir_all(&root);
    }

    /// REGRESSÃO (auditoria de segurança): um ficheiro temporário em `/tmp` não
    /// pode ser sequestrável por outro utilizador local.
    ///
    /// O caso que motivou isto: `delonix-net::bpf` escrevia o objecto BPF no
    /// caminho FIXO `/tmp/delonix_flow.bpf.o` com `fs::write`, e esse ficheiro é
    /// depois carregado no kernel por um processo com `CAP_BPF`/root. Quem
    /// pré-criasse o caminho ficava DONO dele (num `/tmp` sticky nem sequer o
    /// podemos apagar) e podia trocar-lhe o conteúdo antes do `bpftool` o ler.
    ///
    /// O que este teste prova é a propriedade que fecha isso: com o caminho já
    /// ocupado — inclusive por um SYMLINK, que é o vector de redirecção —, a
    /// escrita nunca lhe toca. Nasce sempre um ficheiro NOVO, e a vítima do
    /// symlink fica intacta.
    #[test]
    fn write_private_temp_nao_escreve_num_caminho_sequestrado() {
        use std::os::unix::fs::PermissionsExt;

        let root = tmp_dir("private-temp");
        fs::create_dir_all(&root).unwrap();
        // O "ficheiro do sistema" que o atacante quer que nós sobrescrevamos.
        let victim = root.join("ficheiro-importante");
        fs::write(&victim, b"conteudo-original").unwrap();

        // Não se consegue plantar um symlink no nome que a função VAI escolher
        // (é único por desenho — e é essa a outra metade da defesa). O que se
        // prova aqui é a primitiva de que ela depende, com o symlink já no
        // lugar: `create_new` recusa um caminho existente e não o segue.
        let squatted = root.join("squatted");
        std::os::unix::fs::symlink(&victim, &squatted).unwrap();
        let r = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&squatted);
        assert!(
            r.is_err(),
            "create_new tem de recusar um caminho já existente (incluindo symlink)"
        );
        assert_eq!(
            fs::read(&victim).unwrap(),
            b"conteudo-original",
            "a vítima do symlink foi escrita — a redirecção não foi bloqueada"
        );

        // E o caminho feliz: ficheiro novo, modo 0600 desde a criação, e cada
        // chamada devolve um caminho DIFERENTE (senão a 2.ª invocação voltaria a
        // ser um alvo previsível).
        let a = write_private_temp("dlx-test-priv", b"aaa").unwrap();
        let b = write_private_temp("dlx-test-priv", b"bbb").unwrap();
        assert_ne!(a, b, "dois stages seguidos não podem partilhar o nome");
        assert_eq!(fs::read(&a).unwrap(), b"aaa");
        assert_eq!(fs::read(&b).unwrap(), b"bbb");
        for p in [&a, &b] {
            let mode = fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "temp privado nasceu com o modo errado: {mode:o}"
            );
            let _ = fs::remove_file(p);
        }
    }

    /// REGRESSION: um ficheiro secreto tem de nascer JÁ com o modo restrito.
    ///
    /// `fs::write` + `set_permissions` cria-o sob o umask e só depois o aperta —
    /// uma janela em que outro utilizador local o consegue abrir. É o mesmo
    /// TOCTOU residual que o caminho do kubeconfig fechou com `OpenOptions::mode`,
    /// e que o cofre de segredos ainda tinha.
    #[test]
    fn write_atomic_mode_cria_o_ficheiro_ja_restrito() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp_dir("write-atomic-mode");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("segredo.json");

        write_atomic_mode(&target, b"selado", Some(0o600)).unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "o segredo nasceu com o modo errado: {mode:o}");
        assert_eq!(fs::read(&target).unwrap(), b"selado");

        // Sem modo explícito, o comportamento antigo (umask) mantém-se — não é
        // uma mudança para quem não pediu nada.
        let plain = root.join("normal.json");
        write_atomic(&plain, b"x").unwrap();
        assert!(plain.exists());

        // E nenhum dos dois deixou temporários para trás.
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporários órfãos: {leftovers:?}");
        let _ = fs::remove_dir_all(&root);
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

    /// REGRESSION (concurrency): [`JsonStore::update`] (added to close the same
    /// gap `delonix-vm`'s `status()` had — see `update_concorrente_nao_perde_
    /// escritas` above for the `Store<Container>` sibling this mirrors) must
    /// sequence read-modify-write between threads exactly the same way — N
    /// concurrent increments through a bare `load`+mutate+`save` would lose
    /// writes; through `update`, the final count must be exactly N.
    #[test]
    fn jsonstore_update_concorrente_nao_perde_escritas() {
        let root = tmp_dir("jsonstore-update-race");
        let store: JsonStore<u64> = JsonStore::open(&root).unwrap();
        store.save("counter", &0u64).unwrap();

        const N: usize = 24;
        std::thread::scope(|sc| {
            for _ in 0..N {
                let root = root.clone();
                sc.spawn(move || {
                    let st: JsonStore<u64> = JsonStore::open(&root).unwrap();
                    st.update("counter", |n| {
                        let cur = *n;
                        // Explicit race window between the read and the write:
                        // without a lock, guarantees the lost update.
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        *n = cur + 1;
                        true
                    })
                    .unwrap();
                });
            }
        });

        let got = store.load("counter").unwrap();
        assert_eq!(got, N as u64, "perderam-se escritas: {got} de {N}");
        let _ = fs::remove_dir_all(&root);
    }

    /// `update` on a key that doesn't exist yet propagates `NotFound`
    /// (it re-reads under the lock via `load`, it doesn't create).
    #[test]
    fn jsonstore_update_de_chave_inexistente_da_not_found() {
        let root = tmp_dir("jsonstore-update-missing");
        let store: JsonStore<u64> = JsonStore::open(&root).unwrap();
        let err = store.update("ghost", |n| {
            *n += 1;
            true
        });
        assert!(matches!(err, Err(Error::NotFound(_))));
        let _ = fs::remove_dir_all(&root);
    }

    /// `f` returning `false` aborts the write — the file on disk must stay
    /// untouched (same contract as `Store::update`).
    #[test]
    fn jsonstore_update_aborta_sem_escrever_quando_f_devolve_false() {
        let root = tmp_dir("jsonstore-update-abort");
        let store: JsonStore<u64> = JsonStore::open(&root).unwrap();
        store.save("k", &10u64).unwrap();
        let v = store
            .update("k", |n| {
                *n = 999;
                false
            })
            .unwrap();
        assert_eq!(v, 999, "o valor devolvido reflecte a mutação em memória");
        assert_eq!(
            store.load("k").unwrap(),
            10,
            "mas nada foi persistido, porque f devolveu false"
        );
        let _ = fs::remove_dir_all(&root);
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

    /// REGRESSION (ADR-0011 §3): once names are unique per NAMESPACE rather
    /// than globally, two tenants may legitimately both own `db`. The old
    /// "newest match wins" tie-break would then hand `ingress deny db` — or any
    /// `apply` naming it — whichever was created last, i.e. silently pick a
    /// tenant. It must refuse and name both instead.
    #[test]
    fn um_nome_em_duas_namespaces_e_recusado_nao_adivinhado() {
        let root = tmp_dir("store-ns-names");
        let store = Store::open(&root).unwrap();
        let mk = |id: &str, ns: &str| {
            let mut c = Container::new(
                id.to_string(),
                "db".to_string(),
                "alpine".into(),
                vec!["sh".into()],
                "0".into(),
            );
            c.namespace = ns.to_string();
            c
        };
        store.save(&mk("aaa1", "teamA")).unwrap();
        store.save(&mk("bbb2", "teamB")).unwrap();

        let err = store.load("db").unwrap_err().to_string();
        assert!(
            err.contains("teamA/db") && err.contains("teamB/db"),
            "{err}"
        );
        // The qualified form is exact, in both directions.
        assert_eq!(store.load("teamA/db").unwrap().id, "aaa1");
        assert_eq!(store.load("teamB/db").unwrap().id, "bbb2");
        // The id keeps working, and a qualified miss is NotFound, not ambiguity.
        assert_eq!(store.load("bbb2").unwrap().namespace, "teamB");
        assert!(matches!(store.load("teamC/db"), Err(Error::NotFound(_))));
        let _ = fs::remove_dir_all(&root);
    }

    /// The other half of the contract: a name that is unique on the node — every
    /// node not using namespaces — must resolve exactly as it always did.
    #[test]
    fn um_nome_unico_continua_a_resolver_como_sempre() {
        let root = tmp_dir("store-ns-unique");
        let store = Store::open(&root).unwrap();
        let mut c = Container::new(
            "aaa1".into(),
            "web".into(),
            "alpine".into(),
            vec!["sh".into()],
            "0".into(),
        );
        c.namespace = "teamA".into();
        store.save(&c).unwrap();
        assert_eq!(store.load("web").unwrap().id, "aaa1");
        assert_eq!(store.load("teamA/web").unwrap().id, "aaa1");
        assert!(matches!(store.load("nope"), Err(Error::NotFound(_))));
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
