//! IPAM with **lease** registry — the `/16` anti-collision allocator.
//!
//! The pure hash ([`crate::derive_ip_in`]) only gives the **preferred** IP of an id.
//! On its own, it collides: it maps 32 bits of the id into 16 bits of host (`a.b`), so by
//! the **birthday** paradox two distinct ids hit the same IP with ~50% probability
//! already at ~300 containers in one `/16` — two containers with the SAME IP =
//! broken network, anti-spoof dropping, and firewall/DNAT rules indexed on the
//! wrong IP.
//!
//! This module guarantees **real uniqueness**: an `id → ip` lease persisted per
//! `/16` (one JSON file per prefix at `<base_root>/ipam/<prefix>.json`),
//! protected by `flock` (the CRI is concurrent). Allocation starts from the preferred IP
//! and, if it is held by ANOTHER id, **linearly probes** the host space of the
//! `/16` until the first free one. Deterministic and stable: the same id always returns
//! the same IP (the cleanup paths — detach/publish/firewall —
//! recompute the IP from the id and rely on this).
//!
//! Responsibility boundary: `allocate` creates the lease (on attach), `release`
//! frees it (on detach), `lookup` only reads (in the cleanup recomputers, never
//! creates a file). Allocation always runs on the HOST side (before talking to the
//! holder), so the registry lives in the host's `base_root`, like the `NetDef`s.

use crate::infra::base_root;
use delonix_runtime_core::{Error, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Lease registry directory (`<base_root>/ipam/`).
fn ipam_dir() -> PathBuf {
    base_root().join("ipam")
}

/// Lease file of a `/16` (one per prefix, e.g.: `10.88.json`). The prefix
/// only has digits and a dot, but we sanitize for safety (it never goes to a path
/// with `/`/`..`).
fn prefix_file(prefix: &str) -> PathBuf {
    let safe: String = prefix
        .chars()
        .map(|c| {
            if c.is_ascii_digit() || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    ipam_dir().join(format!("{safe}.json"))
}

/// Exclusive lock (`flock`) of the IPAM registry — serializes read-modify-write of
/// concurrent `allocate`/`release`. A single global lock suffices (the operations
/// are short and rare compared to the container's lifecycle). `Drop` releases it.
struct IpamLock(i32);
impl IpamLock {
    fn acquire() -> IpamLock {
        let _ = std::fs::create_dir_all(ipam_dir());
        let path = ipam_dir().join("lock");
        let c = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes().to_vec())
            .unwrap_or_else(|_| std::ffi::CString::new("/tmp/dlxipamlock").unwrap());
        // SAFETY: open/flock with a valid path; -1 on failure is handled next.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600) };
        if fd >= 0 {
            unsafe { libc::flock(fd, libc::LOCK_EX) };
        }
        IpamLock(fd)
    }
}
impl Drop for IpamLock {
    fn drop(&mut self) {
        if self.0 >= 0 {
            // SAFETY: own fd, opened in acquire().
            unsafe {
                libc::flock(self.0, libc::LOCK_UN);
                libc::close(self.0);
            }
        }
    }
}

/// Reads the `id → ip` map of a prefix. Returns `None` if the file does not exist
/// (never creates it — important so `lookup` doesn't seed state when recomputing a
/// cleanup IP, and for the pure tests that only derive).
fn load(prefix: &str) -> Option<BTreeMap<String, String>> {
    let bytes = std::fs::read(prefix_file(prefix)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persists the `id → ip` map of a prefix (pretty, like the `NetDef`s).
/// **Atomic write** (temporary file + `rename`): a lockless reader
/// (`lookup`, on the cleanup path) never sees a file truncated in the middle of a
/// concurrent `store` — it would see the OLD map or the NEW one, never garbage. Without this, a
/// torn read returned `None` and cleanup fell back to the DERIVED IP (wrong, if the
/// real one had been probed on top of a collision), leaving orphan rules.
fn store(prefix: &str, map: &BTreeMap<String, String>) -> Result<()> {
    std::fs::create_dir_all(ipam_dir()).map_err(|e| Error::Runtime {
        context: "ipam dir",
        message: e.to_string(),
    })?;
    let json = serde_json::to_vec_pretty(map).map_err(|e| Error::Runtime {
        context: "ipam serialize",
        message: e.to_string(),
    })?;
    let final_path = prefix_file(prefix);
    // The tmp stays in the SAME directory (atomic rename only within the same filesystem);
    // suffixed by the pid so two processes under the flock don't clobber the same tmp.
    // SAFETY: getpid() has no preconditions.
    let tmp = ipam_dir().join(format!(".{prefix}.{}.tmp", unsafe { libc::getpid() }));
    std::fs::write(&tmp, json).map_err(|e| Error::Runtime {
        context: "ipam write tmp",
        message: e.to_string(),
    })?;
    std::fs::rename(&tmp, &final_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::Runtime {
            context: "ipam rename",
            message: e.to_string(),
        }
    })
}

/// Allocates (or returns the existing lease of) a unique IP in `prefix`'s `/16` for
/// `id`. Idempotent: an already-registered id always returns the SAME IP. For a new id,
/// it starts from the preferred hash IP and, if held by another id, linearly probes the
/// rest of the `/16`. Clear error if the `/16` is full (~65k hosts). Under `flock`.
pub fn allocate(prefix: &str, id: &str) -> Result<String> {
    let _lock = IpamLock::acquire();
    let mut map = load(prefix).unwrap_or_default();
    if let Some(ip) = map.get(id) {
        return Ok(ip.clone());
    }
    let used: std::collections::HashSet<&str> = map.values().map(String::as_str).collect();
    let preferred = crate::derive_ip_in(prefix, id);
    let ip = if crate::valid_ip_in_subnet(prefix, &preferred) && !used.contains(preferred.as_str())
    {
        preferred
    } else {
        probe_free(prefix, &preferred, &used).ok_or_else(|| Error::Runtime {
            context: "ipam",
            message: format!("no free IP in the {prefix} /16 (registry full)"),
        })?
    };
    map.insert(id.to_string(), ip.clone());
    store(prefix, &map)?;
    Ok(ip)
}

/// Linear probe over the `/16`'s host space, starting at the preferred IP's host
/// (locality — the IP stays close to the deterministic one), skipping reserved ones
/// (`.0.0`/`.0.1`/`.255.255`) and those already in use. `None` if the `/16` is full.
fn probe_free(
    prefix: &str,
    preferred: &str,
    used: &std::collections::HashSet<&str>,
) -> Option<String> {
    // starting host = the last two octets of the preferred one as u16 (a*256+b).
    let start: u32 = preferred
        .rsplit('.')
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .filter_map(|o| o.parse::<u32>().ok())
        .fold(0u32, |acc, o| (acc << 8) | (o & 0xff));
    for k in 0..0x1_0000u32 {
        let host = (start + k) & 0xffff;
        let cand = format!("{prefix}.{}.{}", (host >> 8) & 0xff, host & 0xff);
        if crate::valid_ip_in_subnet(prefix, &cand) && !used.contains(cand.as_str()) {
            return Some(cand);
        }
    }
    None
}

/// Registers a PINNED `id → ip` lease (IP chosen by the user at attach),
/// so that other containers' probing sees it as occupied and never reassigns it.
/// Idempotent. Under `flock`.
pub fn reserve(prefix: &str, id: &str, ip: &str) {
    let _lock = IpamLock::acquire();
    let mut map = load(prefix).unwrap_or_default();
    if map.get(id).map(String::as_str) == Some(ip) {
        return;
    }
    // WARN if the pinned IP already belongs (by lease) to ANOTHER container: we don't
    // reject it (the user chose it explicitly), but we don't silence it either —
    // two containers would end up with the same IP on the wire.
    if let Some(other) = map
        .iter()
        .find(|(other_id, v)| v.as_str() == ip && other_id.as_str() != id)
    {
        tracing::warn!(
            ip = %ip,
            container_id = %id,
            held_by = %other.0,
            "pinned IP {ip} is already leased to '{}'; '{id}' will collide on the network",
            other.0
        );
    }
    map.insert(id.to_string(), ip.to_string());
    let _ = store(prefix, &map);
}

/// Looks up `id`'s leased IP in `prefix`'s `/16`, creating nothing. `None` if
/// there is no lease (the caller then falls back to the hash-derived IP — compat with a
/// container pre-existing this registry, or not yet attached).
pub fn lookup(prefix: &str, id: &str) -> Option<String> {
    load(prefix)?.get(id).cloned()
}

/// Frees `id`'s lease in `prefix`'s `/16` (on detach). Best-effort and
/// idempotent. Under `flock`.
pub fn release(prefix: &str, id: &str) {
    let _lock = IpamLock::acquire();
    if let Some(mut map) = load(prefix) {
        if map.remove(id).is_some() {
            let _ = store(prefix, &map);
        }
    }
}

/// The `/16` prefix (`a.b`) of an IP `a.b.c.d` — to free the lease on detach
/// from the known IP, without the caller having to pass the prefix.
pub fn prefix_of(ip: &str) -> String {
    let o: Vec<&str> = ip.split('.').collect();
    if o.len() == 4 {
        format!("{}.{}", o[0], o[1])
    } else {
        ip.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Isolates the registry in a tmpdir (via `DELONIX_ROOT`) so as not to touch the
    /// user's real store. Serialized by a process lock — this module's tests
    /// share the global `DELONIX_ROOT` env var.
    fn with_root<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dlx-ipam-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: single-thread test under the Mutex above.
        unsafe { std::env::set_var("DELONIX_ROOT", &dir) };
        let out = f();
        unsafe { std::env::remove_var("DELONIX_ROOT") };
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn ids_que_colidiam_no_hash_recebem_ips_distintos() {
        with_root("collide", || {
            // "deadbeef1234" and "deadbeef9999" derive the SAME preferred IP (they share
            // the first 8 hex) — this was exactly the old allocator's collision.
            let a = allocate("10.88", "deadbeef1234").unwrap();
            let b = allocate("10.88", "deadbeef9999").unwrap();
            assert_eq!(a, crate::derive_ip_in("10.88", "deadbeef1234"));
            assert_ne!(
                a, b,
                "a sondagem tem de dar IPs distintos a ids que colidem no hash"
            );
            assert!(crate::valid_ip_in_subnet("10.88", &b));
        });
    }

    #[test]
    fn allocate_e_idempotente_e_lookup_ve_o_lease() {
        with_root("idem", || {
            let a1 = allocate("10.88", "cafe1234").unwrap();
            let a2 = allocate("10.88", "cafe1234").unwrap();
            assert_eq!(a1, a2, "o mesmo id devolve sempre o mesmo IP");
            assert_eq!(lookup("10.88", "cafe1234").as_deref(), Some(a1.as_str()));
            // looking up an id with no lease creates nothing and returns None.
            assert_eq!(lookup("10.88", "naoexiste"), None);
        });
    }

    #[test]
    fn release_liberta_o_ip_para_reuso() {
        with_root("release", || {
            let ip = allocate("10.88", "deadbeef1234").unwrap();
            // a second colliding id got a probed IP (!= ip).
            let other = allocate("10.88", "deadbeef9999").unwrap();
            assert_ne!(ip, other);
            release("10.88", "deadbeef1234");
            assert_eq!(lookup("10.88", "deadbeef1234"), None);
            // the freed IP goes back to being the preferred one of whoever derived it.
            let reuse = allocate("10.88", "deadbeef1234").unwrap();
            assert_eq!(reuse, ip);
        });
    }

    #[test]
    fn muitos_ids_zero_colisoes() {
        // The original bug: by the birthday paradox, a collision in a /16 became likely at
        // ~300 containers and nearly certain at ~600. We allocate 2000 ids (>3× that
        // threshold) and require ALL IPs distinct and valid — the proof that the
        // registry + probing eliminates collision at scale. (The per-prefix file is
        // rewritten in full on each allocate — O(n) I/O per attach; 2000 is enough
        // for the guarantee without making the test O(n²) slow.)
        with_root("stress", || {
            let mut seen = std::collections::HashSet::new();
            for i in 0..2000u32 {
                let id = format!("{:08x}dead", i.wrapping_mul(2_654_435_761)); // spreads
                let ip = allocate("10.88", &id).unwrap();
                assert!(crate::valid_ip_in_subnet("10.88", &ip), "IP inválido {ip}");
                assert!(seen.insert(ip.clone()), "COLISÃO no IP {ip} (id {id})");
            }
            assert_eq!(seen.len(), 2000);
        });
    }

    #[test]
    fn multi_homing_lease_por_rede_e_release_isolado() {
        // A multi-homed container has a lease in EACH /16 (primary network + extra),
        // in the respective prefix file. Disconnecting the extra network
        // (`detach_extra_container`, which now receives the ip) must free ONLY the
        // extra's lease, without touching the primary's. Regression of the v1 leak.
        with_root("multihoming", || {
            let id = "cafebabe0001";
            let primary = allocate("10.88", id).unwrap(); // primary network
            let extra = allocate("10.204", id).unwrap(); // additional network
            assert_eq!(prefix_of(&primary), "10.88");
            assert_eq!(prefix_of(&extra), "10.204");
            // disconnect the extra: frees only the 10.204 lease (via prefix_of(ip)).
            release(&prefix_of(&extra), id);
            assert_eq!(
                lookup("10.204", id),
                None,
                "lease da rede extra tem de sair"
            );
            assert_eq!(
                lookup("10.88", id).as_deref(),
                Some(primary.as_str()),
                "o lease da rede primária NÃO pode ser afetado"
            );
        });
    }

    #[test]
    fn prefix_of_extrai_o_16() {
        assert_eq!(prefix_of("10.88.3.7"), "10.88");
        assert_eq!(prefix_of("10.200.255.254"), "10.200");
    }
}
