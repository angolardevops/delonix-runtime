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

use crate::infra::{base_root, REF_MARKER_GRACE};
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Lease registry directory (`<base_root>/ipam/`).
fn ipam_dir() -> PathBuf {
    base_root().join("ipam")
}

/// The file of a registry KEY (`10.88.json`, `192.168.1.0_24.json`). The key
/// only has digits, dots and a `/`, but we sanitize for safety (it never goes to
/// a path with `/`/`..`): the `/` of a CIDR becomes `_`, which [`key_of_stem`]
/// reverses.
///
/// Takes a KEY, never a raw prefix — the leak this module had came from the
/// two being mixed: `allocate` wrote the file of the raw `10.77.0.0/16` while
/// `release` removed from the file of its key, `10.77`.
fn key_file(key: &str) -> PathBuf {
    ipam_dir().join(format!("{}.json", sanitize_key(key)))
}

fn sanitize_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_digit() || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The registry key a file stem stands for. `None` for what is not a registry
/// (the `lock`, the reaper's side file). A stem written under an old key — the
/// raw `10.77.0.0_16` of the leak — maps to its CANONICAL key, which is how the
/// migration finds it.
fn key_of_stem(stem: &str) -> String {
    registry_key(&stem.replace('_', "/"))
}

/// Exclusive lock (`flock`) of the IPAM registry — serializes read-modify-write of
/// concurrent `allocate`/`release`. A single global lock suffices (the operations
/// are short and rare compared to the container's lifecycle). `Drop` releases it.
struct IpamLock(i32);
impl IpamLock {
    /// Acquires the lock. `None` when it could not be taken — the caller MUST
    /// then refuse the operation.
    ///
    /// BUG FIXED HERE: this used to be infallible. On `open` failure it
    /// returned `IpamLock(-1)` and the callers, which bind it to `let _lock =`,
    /// carried on **with no lock at all** — running exactly the unsynchronized
    /// read-modify-write (`load` → mutate → `store`) that this module exists to
    /// prevent. Two concurrent attaches then both read the same map, both write,
    /// and one lease is lost: two containers on ONE IP, with the firewall and
    /// DNAT rules indexed on the wrong one. Silently failing OPEN on the lock
    /// that guards address uniqueness is the worst possible direction.
    ///
    /// (`Store`'s `FileLock` degrades the same way, but it at least says so in
    /// its doc and the loss there is one overwritten record, not a duplicated
    /// address. Here the failure is not recoverable by a retry of the same
    /// command.)
    fn acquire() -> Option<IpamLock> {
        let _ = std::fs::create_dir_all(ipam_dir());
        let path = ipam_dir().join("lock");
        let c =
            std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes().to_vec()).ok()?;
        // SAFETY: open/flock with a valid NUL-terminated path; -1 is handled.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600) };
        if fd < 0 {
            return None;
        }
        // SAFETY: fd is ours and open; LOCK_EX blocks until granted.
        if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
            // SAFETY: closing our own fd; we are about to drop it on the floor.
            unsafe { libc::close(fd) };
            return None;
        }
        let lock = IpamLock(fd);
        migrate_stray_files();
        Some(lock)
    }

    /// The error every caller reports when the lock cannot be taken. Naming the
    /// consequence matters: "could not lock" alone reads like a transient
    /// annoyance, when what it prevents is a duplicate address.
    fn unavailable() -> Error {
        Error::Command {
            context: "ipam",
            message: format!(
                "could not lock the IPAM registry at {} — refusing to allocate, \
                 since an unsynchronized allocation can hand the same IP to two containers",
                ipam_dir().join("lock").display()
            ),
        }
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
fn load(key: &str) -> Option<BTreeMap<String, String>> {
    load_path(&key_file(key))
}

fn load_path(path: &std::path::Path) -> Option<BTreeMap<String, String>> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Persists the `id → ip` map of a prefix (pretty, like the `NetDef`s).
///
/// **Atomic AND durable** (temp → `fsync` → `rename` → `fsync` the dir, via
/// [`delonix_state::write_atomic`]): a lockless reader (`lookup`, on the
/// cleanup path) never sees a file truncated in the middle of a concurrent
/// `store` — it sees the OLD map or the NEW one, never garbage. Without that, a
/// torn read returned `None` and cleanup fell back to the DERIVED IP (wrong, if
/// the real one had been probed on top of a collision), leaving orphan rules.
///
/// The `fsync` half was added later and matters MOST here: this file is the
/// only thing standing between the allocator and the birthday collision this
/// whole module exists to eliminate (~50 % at ~300 containers). Losing it to a
/// crash is not a degraded metric — it is two containers on one IP, with the
/// firewall and DNAT rules indexed on the wrong one.
fn store(key: &str, map: &BTreeMap<String, String>) -> Result<()> {
    std::fs::create_dir_all(ipam_dir()).map_err(|e| Error::Command {
        context: "ipam dir",
        message: e.to_string(),
    })?;
    let json = serde_json::to_vec_pretty(map).map_err(|e| Error::Command {
        context: "ipam serialize",
        message: e.to_string(),
    })?;
    delonix_state::write_atomic(&key_file(key), &json).map_err(|e| Error::Command {
        context: "ipam write",
        message: e.to_string(),
    })
}

/// Allocates (or returns the existing lease of) a unique IP in `prefix`'s `/16` for
/// `id`. Idempotent: an already-registered id always returns the SAME IP. For a new id,
/// it starts from the preferred hash IP and, if held by another id, linearly probes the
/// rest of the `/16`. Clear error if the `/16` is full (~65k hosts). Under `flock`.
pub fn allocate(prefix: &str, id: &str) -> Result<String> {
    let _lock = IpamLock::acquire().ok_or_else(IpamLock::unavailable)?;
    // The KEY, not the prefix as the caller spelled it: `10.77.0.0/16` and
    // `10.77` are one network and must be one registry. It is also what gives a
    // CIDR `/16` its hash-derived address back — `derive_ip_in` of the raw CIDR
    // produced `10.77.0.0/16.A.B`, never valid, so every address came from the
    // linear probe (`.0.2`, `.0.3`, …) and no id kept a stable preferred one.
    let prefix = &registry_key(prefix);
    let mut map = load(prefix).unwrap_or_default();
    if let Some(ip) = map.get(id) {
        return Ok(ip.clone());
    }
    let used: std::collections::HashSet<&str> = map.values().map(String::as_str).collect();
    let preferred = crate::derive_ip_in(prefix, id);
    let ip = if crate::valid_ip_in_subnet(prefix, &preferred)
        && !crate::in_vm_dhcp_pool(prefix, &preferred)
        && !used.contains(preferred.as_str())
    {
        preferred
    } else {
        probe_free(prefix, &preferred, &used).ok_or_else(|| Error::Command {
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
/// (`.0.0`/`.0.1`/`.255.255`), the VM DHCP pool ([`crate::in_vm_dhcp_pool`]) and
/// those already in use. `None` if the `/16` is full.
fn probe_free(
    prefix: &str,
    preferred: &str,
    used: &std::collections::HashSet<&str>,
) -> Option<String> {
    // Sonda LINEAR sobre o espaço de hosts do prefixo, a partir do preferido —
    // o mesmo que a versão anterior fazia, mas sobre o prefixo real em vez de um
    // /16 assumido. Num /16 percorre exactamente os mesmos 65536 candidatos.
    let net = crate::Cidr::parse(prefix)?;
    let inicio = crate::Cidr::parse_addr(preferred).unwrap_or(net.base);
    let tamanho = net.size();
    // O ciclo é sobre o TAMANHO do prefixo e não sobre um 0x10000 fixo: num /22
    // dar 65536 voltas seria percorrer 64× o mesmo espaço, e num /8 pararia a
    // meio e reportaria «cheio» com endereços livres de sobra — o erro mais
    // caro que este ciclo poderia ter.
    for k in 0..tamanho {
        // Envolve DENTRO do prefixo: sair dele e voltar a entrar produziria
        // candidatos de outra rede, que o `valid_ip_in_subnet` recusaria em
        // silêncio, gastando o ciclo inteiro sem nunca encontrar nada.
        let desloc = (inicio.wrapping_sub(net.base).wrapping_add(k)) % tamanho;
        let cand = crate::Cidr::fmt_u32(net.base + desloc);
        if crate::valid_ip_in_subnet(prefix, &cand)
            && !crate::in_vm_dhcp_pool(prefix, &cand)
            && !used.contains(cand.as_str())
        {
            return Some(cand);
        }
    }
    None
}

/// Registers a PINNED `id → ip` lease (IP chosen by the user at attach),
/// so that other containers' probing sees it as occupied and never reassigns it.
/// Idempotent. Under `flock`.
///
/// **Fail-closed**, in the three ways it used to fail open:
///
/// * without the lock it logged and RETURNED — and the attach went on to wire
///   an address the registry never heard of, free to be handed to the next
///   container;
/// * an address already leased to ANOTHER id was only a warning, and was
///   written anyway: two containers on one IP, with the anti-spoof and every
///   rule indexed on the wrong one;
/// * an address in the VM DHCP pool was accepted, and the DHCP server hands
///   that same address to a VM whose MAC hashes onto it.
///
/// Each of those is now an error, and the attach does not happen.
pub fn reserve(prefix: &str, id: &str, ip: &str) -> Result<()> {
    let key = registry_key(prefix);
    if crate::in_vm_dhcp_pool(&key, ip) {
        return Err(Error::IpInUse(format!(
            "IP {ip} is in the VM DHCP pool of {prefix} (.254.10–.254.249) — the \
             network's DHCP server hands it to a VM; pick an address outside it"
        )));
    }
    reserve_in(&key, id, ip, "pick another address")
}

/// The lease of a VM's DHCP address — the pool's half of [`reserve`].
///
/// The DHCP server computes a VM's address from its MAC, outside this registry,
/// and two MACs can hash onto one address (240 slots per network: by the
/// birthday paradox, ~50 % at 18 VMs). Recording the address here is what turns
/// that collision into a refusal at attach instead of two guests answering ARP
/// for one IP — and what makes a VM's address show in `network ipam ls` beside
/// the containers'.
pub fn reserve_vm_dhcp(prefix: &str, id: &str, ip: &str) -> Result<()> {
    let key = registry_key(prefix);
    if !crate::in_vm_dhcp_pool(&key, ip) {
        return Err(Error::IpNotInSubnet(format!(
            "IP {ip} is not in the VM DHCP pool of {prefix}"
        )));
    }
    // A VM does not choose its address: the DHCP derives it from the MAC, and
    // the MAC from the VM's name. "Pick another address" is advice it cannot
    // follow (measured: that is what the refusal said).
    reserve_in(
        &key,
        id,
        ip,
        "a VM's DHCP address comes from its MAC, which comes from its NAME — create it \
         under another name",
    )
}

/// `remedy` is the one thing the refused caller can actually do about it.
fn reserve_in(key: &str, id: &str, ip: &str, remedy: &str) -> Result<()> {
    if !crate::valid_ip_in_subnet(key, ip) {
        return Err(Error::IpNotInSubnet(format!(
            "IP {ip} is not a usable address of {key}"
        )));
    }
    let _lock = IpamLock::acquire().ok_or_else(IpamLock::unavailable)?;
    let mut map = load(key).unwrap_or_default();
    if map.get(id).map(String::as_str) == Some(ip) {
        return Ok(());
    }
    if let Some((other, _)) = map
        .iter()
        .find(|(other_id, v)| v.as_str() == ip && other_id.as_str() != id)
    {
        return Err(Error::IpInUse(format!(
            "IP {ip} is already leased to '{other}' — two owners of one address \
             would collide on the wire; {remedy}"
        )));
    }
    map.insert(id.to_string(), ip.to_string());
    store(key, &map)
}

/// Looks up `id`'s leased IP in `prefix`'s `/16`, creating nothing. `None` if
/// there is no lease (the caller then falls back to the hash-derived IP — compat with a
/// container pre-existing this registry, or not yet attached).
pub fn lookup(prefix: &str, id: &str) -> Option<String> {
    let key = registry_key(prefix);
    if let Some(ip) = load(&key).and_then(|m| m.get(id).cloned()) {
        return Some(ip);
    }
    // Lockless, so it can run before any locked operation has migrated a stray
    // file: read the file the prefix as SPELLED would have had, too. Without
    // this, the first cleanup after an upgrade would fall back to the derived
    // IP of a container whose real address lives in the old file.
    let raw = ipam_dir().join(format!("{}.json", sanitize_key(prefix)));
    if raw != key_file(&key) {
        return load_path(&raw)?.get(id).cloned();
    }
    None
}

/// Frees `id`'s lease in `prefix`'s `/16` (on detach). Best-effort and
/// idempotent. Under `flock`.
pub fn release(prefix: &str, id: &str) {
    // Idem: um release destravado pode reescrever o mapa por cima de um
    // allocate concorrente e ressuscitar um lease já libertado.
    let Some(_lock) = IpamLock::acquire() else {
        tracing::error!(
            prefix = %prefix, container_id = %id,
            "{}", IpamLock::unavailable()
        );
        return;
    };
    let key = registry_key(prefix);
    if let Some(mut map) = load(&key) {
        if map.remove(id).is_some() {
            let _ = store(&key, &map);
        }
    }
}

/// Frees every lease `id` holds, in every registry — for the caller that no
/// longer knows on which network it was (a VM detached from its orphan
/// cleanup, with no record to read the address from). Best-effort, under `flock`.
pub fn release_everywhere(id: &str) {
    let Some(_lock) = IpamLock::acquire() else {
        tracing::error!(container_id = %id, "{}", IpamLock::unavailable());
        return;
    };
    for key in registry_keys() {
        if let Some(mut map) = load(&key) {
            if map.remove(id).is_some() {
                let _ = store(&key, &map);
            }
        }
    }
}

/// The keys of every registry file present (skips `lock` and side files).
fn registry_keys() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(ipam_dir()) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            e.path()
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
                .map(key_of_stem)
        })
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Moves the leases of every file written under a NON-canonical key into the
/// file of its canonical one, and removes the stray file.
///
/// The stray files are what the leak left behind: a network created with
/// `--subnet 10.X.0.0/16` leased in `10.X.0.0_16.json` and was released in
/// `10.X.json`, so every container ever run on it still holds its lease there.
/// Their entries WIN over the canonical file's for the same id — they are what
/// `allocate`/`lookup` actually handed out and read back, so a live container
/// keeps its address on the next `start`. The dead ones among them are then
/// ordinary orphans, for `network ipam prune` and its grace window.
///
/// Runs under the lock (from [`IpamLock::acquire`]), so it never races an
/// allocation; a file that cannot be rewritten is left in place, never dropped.
fn migrate_stray_files() {
    let Ok(rd) = std::fs::read_dir(ipam_dir()) else {
        return;
    };
    for e in rd.flatten() {
        let path = e.path();
        let Some(stem) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        let key = key_of_stem(stem);
        if key_file(&key) == path {
            continue;
        }
        let Some(stray) = load_path(&path) else {
            continue;
        };
        let mut canon = load(&key).unwrap_or_default();
        for (id, ip) in stray {
            if let Some((other, _)) = canon.iter().find(|(o, v)| **v == ip && **o != id) {
                tracing::warn!(
                    ip = %ip, container_id = %id, held_by = %other,
                    "ipam migration: {ip} is leased to two ids — `network ipam prune` \
                     reclaims whichever of them is dead"
                );
            }
            canon.insert(id, ip);
        }
        if store(&key, &canon).is_ok() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The `/16` prefix (`a.b`) of an IP `a.b.c.d` — to free the lease on detach
/// from the known IP, without the caller having to pass the prefix.
/// A CHAVE do registo de leases para um endereço.
///
/// **Procura, não calcula.** O `prefix_of` deriva `10.210` dos dois primeiros
/// octetos, e isso só funciona enquanto toda a rede for um /16 — num /22 ou num
/// /28 dois octetos não identificam rede nenhuma, e libertar um lease com a
/// chave errada deixa o endereço marcado como usado PARA SEMPRE (o container
/// desaparece, o lease fica, e a rede vai-se enchendo sem nada a explicar).
///
/// Por isso vai à lista de redes e devolve a chave daquela que CONTÉM o
/// endereço. Só quando nenhuma o contém — uma rede já removida, um IP de outra
/// era — cai para os dois octetos, que é o que sempre fez e continua a ser a
/// resposta certa para um registo legado.
pub fn key_for_ip(ip: &str) -> String {
    if let Some(addr) = crate::Cidr::parse_addr(ip) {
        for def in crate::infra::network_list() {
            if let Some(c) = crate::Cidr::parse(&def.prefix) {
                if c.contains(addr) {
                    return registry_key(&def.prefix);
                }
            }
        }
    }
    prefix_of(ip)
}

/// A chave de registo de um prefixo.
///
/// Um `10.x/16` continua a ser `10.x` — **os ficheiros de lease que existem no
/// disco estão indexados assim**, e mudar a chave faria o motor deixar de ver os
/// leases de todas as redes actuais de uma só vez: cada container reiniciado
/// receberia um endereço novo, e os antigos ficariam ocupados por ninguém.
/// Qualquer outro prefixo usa o CIDR, que é a única forma que o descreve.
pub fn registry_key(prefix: &str) -> String {
    match crate::Cidr::parse(prefix) {
        Some(c) if c.len == 16 && (c.base >> 24) == 10 => {
            let b = c.base.to_be_bytes();
            format!("{}.{}", b[0], b[1])
        }
        Some(c) => c.to_string_cidr(),
        None => prefix.to_string(),
    }
}

/// Every lease this node holds, as `(prefix, id, ip)`, sorted.
///
/// READ-ONLY, and deliberately without the `flock`: this exists for `network
/// diagnose` to SHOW the registry, and taking the allocator's lock to look at it
/// would let a report block an attach. A torn read is impossible anyway — `store`
/// writes through `write_atomic`, so a reader sees the old map or the new one.
///
/// It does not reap, and the distinction matters: deciding a lease is dead needs
/// a grace period and the lock, because a container holds its lease BEFORE it has
/// a record. Showing is safe; reclaiming is not, and the reclaim is why this is
/// the command that comes first.
pub fn all_leases() -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(ipam_dir()) else {
        return out;
    };
    for e in rd.flatten() {
        let path = e.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(prefix) = name.strip_suffix(".json") else {
            continue; // the `lock` file, and anything else that is not a registry
        };
        // The canonical key, not the file stem: it is what `network ipam ls
        // --network` filters on (`registry_key`), and a stem (`…_24`) never
        // matched it.
        if let Some(map) = load_path(&path) {
            let key = key_of_stem(prefix);
            for (id, ip) in map {
                out.push((key.clone(), id, ip));
            }
        }
    }
    out.sort();
    out
}

/// Candidate reap markers file (`<base_root>/ipam/reap-candidates`):
/// `"<prefix>/<id>" -> unix seconds of the first time it was seen orphaned`.
///
/// A lease has no per-entry timestamp of its own (`allocate`/`release` only
/// ever touch the `id -> ip` map), so — unlike `infra::refs_dir`, where each
/// marker is its own file with its own `mtime` — this module has to keep the
/// grace clock in a side file instead of reading one off the lease itself.
fn reap_candidates_file() -> PathBuf {
    ipam_dir().join("reap-candidates") // no `.json`: `all_leases`/the reaper skip it like the `lock` file
}

fn load_reap_candidates() -> BTreeMap<String, u64> {
    std::fs::read(reap_candidates_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn store_reap_candidates(candidates: &BTreeMap<String, u64>) {
    if candidates.is_empty() {
        let _ = std::fs::remove_file(reap_candidates_file());
        return;
    }
    if let Ok(json) = serde_json::to_vec_pretty(candidates) {
        let _ = delonix_state::write_atomic(&reap_candidates_file(), &json);
    }
}

/// Reclaims leases whose id is not among the `live` ones — the IPAM's own
/// version of `infra::reap_orphan_refs`, closing the leak this repo has
/// measured (391 leases, 47 with a live container — 88% orphaned).
///
/// **Two-pass, under the SAME [`crate::infra::REF_MARKER_GRACE`] window**: a
/// lease is written on `allocate`, before the container's own Store record is
/// saved (the address has to exist before the container does) — the exact
/// TOCTOU `reap_orphan_refs`'s grace period already exists to survive. A lease
/// first seen orphaned is only a CANDIDATE; it is reclaimed on a LATER call
/// that still finds it orphaned after the grace window, never on the pass
/// that first notices it. Reclaiming on sight would race a container that is
/// mid-creation and hand its just-allocated address to someone else.
///
/// A candidate reappearing in `live` (the container showed up after all) is
/// dropped from the candidate list without being touched — never reclaimed on
/// a stale sighting.
///
/// Under the allocator's own lock, so it can never race an `allocate`/
/// `release`/`reserve` into losing a write. Returns how many leases it freed.
pub fn reap_orphan_leases(live: &std::collections::HashSet<String>) -> usize {
    let Some(_lock) = IpamLock::acquire() else {
        tracing::error!("{}", IpamLock::unavailable());
        return 0;
    };
    let now = std::time::SystemTime::now();
    let now_secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let candidates = load_reap_candidates();
    let mut freed = 0usize;

    // Canonical keys: the lock above already migrated any stray file.
    let prefixes = registry_keys();

    let mut still_candidate: BTreeMap<String, u64> = BTreeMap::new();
    for prefix in prefixes {
        let Some(mut map) = load(&prefix) else {
            continue;
        };
        let mut changed = false;
        for id in map.keys().cloned().collect::<Vec<_>>() {
            let key = format!("{prefix}/{id}");
            if live.contains(&id) {
                continue; // still alive — never a candidate, never reaped
            }
            match candidates.get(&key) {
                None => {
                    // First sighting: start the clock, don't reclaim yet.
                    still_candidate.insert(key, now_secs);
                }
                Some(&first_seen) => {
                    let age = now_secs.saturating_sub(first_seen);
                    if age >= REF_MARKER_GRACE.as_secs() {
                        map.remove(&id);
                        changed = true;
                        freed += 1;
                        // dropped from `still_candidate` — reclaimed, not carried forward
                    } else {
                        still_candidate.insert(key, first_seen);
                    }
                }
            }
        }
        if changed {
            let _ = store(&prefix, &map);
        }
    }
    store_reap_candidates(&still_candidate);
    freed
}

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

    /// **Esgotar um prefixo por inteiro** — a prova anti-colisão que um /16
    /// nunca dá, porque ninguém enche 65 mil endereços num teste.
    ///
    /// A /28 has 16 addresses and 13 usable: 2 of them are the VM DHCP pool
    /// (`crate::vm_dhcp_pool`), so the probe must return the other 11, all
    /// distinct, all inside, none in the pool — and then say there is no more,
    /// instead of repeating one (a silent collision: two containers on one IP)
    /// or handing out one from outside.
    #[test]
    fn esgotar_um_28_da_13_enderecos_distintos_e_depois_nada() {
        let cidr = "192.168.1.0/28";
        let net = crate::Cidr::parse(cidr).unwrap();
        let mut usados: std::collections::HashSet<String> = std::collections::HashSet::new();
        let (_, pool) = crate::vm_dhcp_pool(cidr).unwrap();
        let for_containers = 13 - pool as usize;
        for i in 0..for_containers {
            let refs: std::collections::HashSet<&str> = usados.iter().map(String::as_str).collect();
            let preferido = crate::derive_ip_in(cidr, &format!("{i:08x}"));
            let ip = probe_free(cidr, &preferido, &refs)
                .unwrap_or_else(|| panic!("sem endereço à {i}.ª volta, com {} usados", refs.len()));
            let a = crate::Cidr::parse_addr(&ip).unwrap();
            assert!(net.contains(a), "{ip} fora do prefixo");
            assert_ne!(a, net.base);
            assert_ne!(a, net.base + 1);
            assert_ne!(a, net.last());
            assert!(!crate::in_vm_dhcp_pool(cidr, &ip), "{ip} is in the VM pool");
            assert!(usados.insert(ip.clone()), "REPETIU {ip}");
        }
        assert_eq!(usados.len(), 11);
        // E agora está mesmo cheio.
        let refs: std::collections::HashSet<&str> = usados.iter().map(String::as_str).collect();
        assert_eq!(
            probe_free(cidr, "192.168.1.5", &refs),
            None,
            "devolveu um endereço de um /28 cheio"
        );
    }

    /// A sonda percorre o espaço do PREFIXO, e não 0x10000 fixo.
    ///
    /// Com o ciclo antigo, um /8 parava a meio e reportava «cheio» com milhões
    /// de endereços livres; e num /22 dava 64 voltas ao mesmo espaço. Aqui
    /// verifica-se o que importa: a partir de um preferido perto do FIM, a sonda
    /// envolve para o princípio em vez de desistir.
    #[test]
    fn a_sonda_envolve_dentro_do_prefixo_em_vez_de_sair_ou_desistir() {
        let cidr = "192.168.1.0/28";
        // tudo ocupado excepto o .2 (o primeiro utilizável)
        let ocupados: Vec<String> = (3..=14).map(|i| format!("192.168.1.{i}")).collect();
        let refs: std::collections::HashSet<&str> = ocupados.iter().map(String::as_str).collect();
        // parte do fim: só encontra se envolver.
        assert_eq!(
            probe_free(cidr, "192.168.1.14", &refs).as_deref(),
            Some("192.168.1.2")
        );
    }

    use super::*;

    /// Isolates the registry in a tmpdir (via `DELONIX_ROOT`) so as not to touch the
    /// user's real store. Serialized by a process lock — this module's tests
    /// share the global `DELONIX_ROOT` env var.
    /// `DELONIX_ROOT` é uma variável de ambiente GLOBAL ao processo: todo o
    /// teste que lhe mexa tem de partilhar ESTE mutex.
    ///
    /// BUG apanhado pela suite completa: o teste de fail-closed adicionado na
    /// v0.38.1 trazia um `static LOCK` PRÓPRIO, o que não serializa nada — dois
    /// mutexes distintos deixam os testes correr em paralelo, e o `DELONIX_ROOT`
    /// só-leitura que ele instala vazava para um `allocate` concorrente, que
    /// falhava com ENOENT. Flaky, e por isso passou despercebido na corrida em
    /// que foi introduzido.
    fn with_root<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        // The lock is now crate-wide (`crate::testenv`): `infra`'s tests write
        // the same variable, and a mutex private to this module serialized
        // nothing against them — see the note on `testenv`.
        let mut env = crate::testenv::lock();
        // The PID in the path, not just the tag. `ENV_LOCK` above serializes
        // within the PROCESS; nothing serializes across processes, and this
        // workspace runs several sessions at once (one worktree per task). With
        // a fixed path, two `cargo test -p delonix-sdn` runs delete each other's
        // directory in the entry and exit `remove_dir_all`, and whichever is
        // midway through this test's 2000 allocations dies writing.
        //
        // Measured 2026-08-28: the suite failed the pre-push gate, passed when
        // run alone, and `pgrep` caught ANOTHER session running this very test
        // at that moment, chasing the same failure. The neighbour
        // `dlx-ipam-nolock-{pid}`, 114 lines below, already did this — the fix
        // was written in the same file.
        let dir = std::env::temp_dir().join(format!("dlx-ipam-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        env.set("DELONIX_ROOT", &dir);
        let out = f();
        // No explicit unset: the guard restores what it found when it drops,
        // including «was not set».
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

    /// `all_leases` sees every prefix and skips what is not a registry — the
    /// `lock` file sits in the same directory, and a reader that tried to parse
    /// it would report an empty registry on a node that has leases.
    #[test]
    fn all_leases_sees_every_prefix_and_skips_the_lock() {
        with_root("alllease", || {
            allocate("10.88", "aaaa0001").unwrap();
            allocate("10.88", "bbbb0002").unwrap();
            allocate("10.99", "cccc0003").unwrap();
            // The allocator's own lock file lives beside the registries.
            let _ = IpamLock::acquire();

            let all = all_leases();
            assert_eq!(all.len(), 3, "{all:?}");
            let prefixes: std::collections::BTreeSet<&str> =
                all.iter().map(|(p, _, _)| p.as_str()).collect();
            assert_eq!(
                prefixes,
                ["10.88", "10.99"].into_iter().collect(),
                "both prefixes, and nothing from `lock`"
            );
            // Sorted, so a report does not reorder between runs.
            let mut sorted = all.clone();
            sorted.sort();
            assert_eq!(all, sorted);
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

    /// REGRESSION (fail-closed): if the registry lock cannot be taken,
    /// `allocate` must REFUSE, never allocate unsynchronized.
    ///
    /// `acquire()` used to be infallible — on `open` failure it handed back a
    /// lock holding fd `-1` and the caller ran the whole read-modify-write with
    /// no mutual exclusion at all. That is the exact race this module exists to
    /// prevent, and its outcome is two containers sharing one IP. Restoring the
    /// infallible `acquire()` makes this test fail: `allocate` would return
    /// `Ok(ip)` from an unlocked path.
    #[test]
    fn allocate_recusa_quando_nao_consegue_trancar_o_registo() {
        use std::os::unix::fs::PermissionsExt;
        let mut env = crate::testenv::lock();

        let dir = std::env::temp_dir().join(format!("dlx-ipam-nolock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Read-only root: `ipam/` cannot be created, so the lock file cannot be
        // opened — the same shape as a full disk or a lost mount.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        env.set("DELONIX_ROOT", &dir);
        let got = allocate("10.88", "cafe0001");

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        match got {
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("same IP to two containers"),
                    "o erro tem de nomear a consequência, não só 'falhou': {msg}"
                );
            }
            // Root ignores the mode bits, so the lock opens fine and the
            // allocation legitimately succeeds — declare it instead of letting
            // the test pass for the wrong reason.
            Ok(ip) => {
                assert!(
                    // SAFETY: `geteuid` takes no arguments and has no preconditions.
                    unsafe { libc::geteuid() } == 0,
                    "allocate devolveu {ip} sem conseguir trancar o registo — é este o bug"
                );
                eprintln!("aviso: a correr como root, os bits de permissão não se aplicam — asserção saltada");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefix_of_extrai_o_16() {
        assert_eq!(prefix_of("10.88.3.7"), "10.88");
        assert_eq!(prefix_of("10.200.255.254"), "10.200");
    }

    /// The first sighting of an orphaned lease is NEVER reclaimed on the same
    /// call — it only starts the grace-period clock. Reclaiming on sight
    /// would repeat exactly the race `infra::reap_orphan_refs` already exists
    /// to survive: a container mid-creation has no Store record yet.
    #[test]
    fn primeira_observacao_orfa_nunca_e_reclamada_de_imediato() {
        with_root("reap-first", || {
            let ip = allocate("10.88", "orfao0001").unwrap();
            let live = std::collections::HashSet::new();
            let freed = reap_orphan_leases(&live);
            assert_eq!(freed, 0, "the 1st sighting must not reclaim anything");
            assert_eq!(
                lookup("10.88", "orfao0001").as_deref(),
                Some(ip.as_str()),
                "the lease must still be alive after the 1st sighting"
            );
        });
    }

    /// A lease still orphaned AFTER the grace period is reclaimed — simulated
    /// without a real sleep: the 1st call records the candidate, and its
    /// timestamp is aged past the window by hand before the 2nd call.
    #[test]
    fn lease_orfao_alem_da_graca_e_reclamado_na_segunda_chamada() {
        with_root("reap-second", || {
            allocate("10.88", "orfao0002").unwrap();
            let live = std::collections::HashSet::new();
            assert_eq!(reap_orphan_leases(&live), 0);

            // Age the candidate's clock past the grace window without a real
            // sleep — the same trick the rest of the suite uses to stay fast.
            let mut candidates = load_reap_candidates();
            let key = "10.88/orfao0002".to_string();
            assert!(
                candidates.contains_key(&key),
                "the 1st call should have recorded the candidate: {candidates:?}"
            );
            let aged = candidates[&key].saturating_sub(REF_MARKER_GRACE.as_secs() + 1);
            candidates.insert(key, aged);
            store_reap_candidates(&candidates);

            let freed = reap_orphan_leases(&live);
            assert_eq!(freed, 1, "should have reclaimed the past-grace lease");
            assert_eq!(lookup("10.88", "orfao0002"), None);
        });
    }

    /// A candidate that comes back ALIVE between two calls is never reclaimed
    /// — even having been seen orphaned before, and even if its clock (were
    /// it consulted) had already crossed the window.
    #[test]
    fn candidato_que_reaparece_vivo_nunca_e_reclamado() {
        with_root("reap-revive", || {
            let ip = allocate("10.88", "revive0001").unwrap();
            let empty = std::collections::HashSet::new();
            assert_eq!(reap_orphan_leases(&empty), 0);

            // Age the candidate past grace, same trick as above — if the next
            // call ignored `live`, this would reclaim it.
            let mut candidates = load_reap_candidates();
            let key = "10.88/revive0001".to_string();
            let aged = candidates[&key].saturating_sub(REF_MARKER_GRACE.as_secs() + 1);
            candidates.insert(key, aged);
            store_reap_candidates(&candidates);

            let mut live = std::collections::HashSet::new();
            live.insert("revive0001".to_string());
            let freed = reap_orphan_leases(&live);
            assert_eq!(freed, 0, "a live id can never be reclaimed");
            assert_eq!(lookup("10.88", "revive0001").as_deref(), Some(ip.as_str()));

            // And the candidate cannot have survived hidden: if it comes back
            // orphaned again, its clock restarts from zero instead of already
            // being "old" from an earlier sighting.
            let freed_again = reap_orphan_leases(&empty);
            assert_eq!(
                freed_again, 0,
                "a fresh orphan sighting starts the grace period from zero"
            );
            assert_eq!(lookup("10.88", "revive0001").as_deref(), Some(ip.as_str()));
        });
    }

    /// A live lease never enters the candidate list, call after call.
    #[test]
    fn lease_vivo_nunca_e_tocado() {
        with_root("reap-alive", || {
            let ip = allocate("10.88", "vivo0001").unwrap();
            let mut live = std::collections::HashSet::new();
            live.insert("vivo0001".to_string());
            for _ in 0..3 {
                assert_eq!(reap_orphan_leases(&live), 0);
            }
            assert_eq!(lookup("10.88", "vivo0001").as_deref(), Some(ip.as_str()));
            assert!(
                load_reap_candidates().is_empty(),
                "um id vivo nunca deve aparecer como candidato"
            );
        });
    }
}

/// Transactional IPAM (NaaS audit S2, doc 62 §6 P1): the leases that were left
/// behind, and the two address authorities. Every test here was seen FAILING
/// against the code before this fix.
#[cfg(test)]
mod tests_transactional {
    use super::*;

    /// BOTH roots isolated: `detach_container` reaches the control socket, and
    /// without `DELONIX_NET_RUNTIME_DIR` that path would resolve to this host's
    /// real infra. With no holder, `control_send` fails fast.
    fn with_roots<T>(tag: &str, f: impl FnOnce(&std::path::Path) -> T) -> T {
        let mut env = crate::testenv::lock();
        let dir = std::env::temp_dir().join(format!("dlx-ipam-s2-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run")).unwrap();
        env.set("DELONIX_ROOT", &dir);
        env.set("DELONIX_NET_RUNTIME_DIR", dir.join("run"));
        let out = f(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    fn leases_of(id: &str) -> Vec<(String, String, String)> {
        all_leases()
            .into_iter()
            .filter(|(_, i, _)| i == id)
            .collect()
    }

    /// Finding 1: a network created with `--subnet 10.X.0.0/16` leased in
    /// `10.X.0.0_16.json` (the raw prefix) and the detach released in
    /// `10.X.json` (`key_for_ip` → `registry_key`). The lease never left.
    /// Measured before: `[("10.77.0.0_16", "c1d20000feed0001", "10.77.0.2")]`
    /// still there after the detach.
    #[test]
    fn a_cidr_16_lease_leaves_on_detach() {
        with_roots("cidr16", |_| {
            let def = crate::infra::network_create_with("s2cidr16", "10.77.0.0/16").unwrap();
            let plan = crate::infra::resolve_net(&def.name).unwrap();
            let id = "c1d20000feed0001";
            let ip = allocate(&plan.prefix, id).unwrap();
            assert_eq!(leases_of(id).len(), 1, "the attach must leave ONE lease");
            crate::infra::detach_container(id, &ip);
            assert!(leases_of(id).is_empty(), "left behind: {:?}", leases_of(id));
        });
    }

    /// And a CIDR `/16` gets the id's PREFERRED address back: with the raw
    /// prefix, `derive_ip_in` gave `10.77.0.0/16.A.B` (invalid) and every
    /// address came from the linear probe (`.0.2`, `.0.3`, …).
    #[test]
    fn a_cidr_16_hands_out_the_id_derived_address() {
        with_roots("derived", |_| {
            let id = "0a0b0c0dfeed0002";
            let ip = allocate("10.78.0.0/16", id).unwrap();
            assert_eq!(ip, crate::derive_ip_in("10.78", id));
            // The same address through both spellings of one prefix, one key.
            assert_eq!(lookup("10.78", id).as_deref(), Some(ip.as_str()));
            assert_eq!(lookup("10.78.0.0/16", id).as_deref(), Some(ip.as_str()));
            let keys: Vec<String> = leases_of(id).into_iter().map(|(p, _, _)| p).collect();
            assert_eq!(keys, vec![registry_key("10.78.0.0/16")]);
        });
    }

    /// A non-/16 prefix never leaked — the canonical key must not break it, and
    /// `ipam ls --network` filters by the key `all_leases` returns.
    #[test]
    fn a_slash24_lease_leaves_on_detach_and_lists_under_its_key() {
        with_roots("cidr24", |_| {
            let def = crate::infra::network_create_with("s2cidr24", "172.20.9.0/24").unwrap();
            let id = "c1d20000feed0003";
            let ip = allocate(&def.prefix, id).unwrap();
            assert_eq!(
                leases_of(id)
                    .into_iter()
                    .map(|(p, _, _)| p)
                    .collect::<Vec<_>>(),
                vec![registry_key(&def.prefix)],
                "`ipam ls` showed the file stem (`…_24`), which never matched the filter"
            );
            crate::infra::detach_container(id, &ip);
            assert!(leases_of(id).is_empty(), "{:?}", leases_of(id));
        });
    }

    /// Finding 1, migration: the files the bug left behind move to the canonical
    /// key without losing a lease — the live container keeps the SAME address.
    #[test]
    fn raw_prefix_files_migrate_to_the_canonical_key() {
        with_roots("migrate", |root| {
            let ipam = root.join("ipam");
            std::fs::create_dir_all(&ipam).unwrap();
            std::fs::write(
                ipam.join("10.83.0.0_16.json"),
                r#"{ "live00000000mig1": "10.83.0.7" }"#,
            )
            .unwrap();
            std::fs::write(
                ipam.join("10.83.json"),
                r#"{ "old000000000mig2": "10.83.9.9" }"#,
            )
            .unwrap();
            // Before any locked operation, `lookup` already sees it.
            assert_eq!(
                lookup("10.83.0.0/16", "live00000000mig1").as_deref(),
                Some("10.83.0.7")
            );
            let ip = allocate("10.83.0.0/16", "live00000000mig1").unwrap();
            assert_eq!(ip, "10.83.0.7", "the live container's address changed");
            assert!(
                !ipam.join("10.83.0.0_16.json").exists(),
                "the raw file stayed"
            );
            assert_eq!(
                lookup("10.83", "old000000000mig2").as_deref(),
                Some("10.83.9.9")
            );
            release(&key_for_ip(&ip), "live00000000mig1");
            assert!(leases_of("live00000000mig1").is_empty());
        });
    }

    /// Finding 2: reserving an IP ANOTHER container already holds only warned
    /// and wrote — two containers on one address.
    #[test]
    fn reserving_another_containers_ip_is_refused() {
        with_roots("dup", |_| {
            reserve("10.79", "owner000000000a1", "10.79.3.3").unwrap();
            let e = reserve("10.79", "intruder0000000b", "10.79.3.3").unwrap_err();
            assert!(matches!(e, Error::IpInUse(_)), "{e}");
            assert_eq!(lookup("10.79", "intruder0000000b"), None);
            assert_eq!(
                lookup("10.79", "owner000000000a1").as_deref(),
                Some("10.79.3.3")
            );
            // The owner itself may repeat it (idempotent).
            reserve("10.79", "owner000000000a1", "10.79.3.3").unwrap();
        });
    }

    /// Finding 2: without the lock `reserve` logged and RETURNED, and the attach
    /// went on with an address the registry never heard of.
    #[test]
    fn reserve_refuses_when_it_cannot_lock_the_registry() {
        use std::os::unix::fs::PermissionsExt;
        let mut env = crate::testenv::lock();
        let dir = std::env::temp_dir().join(format!("dlx-ipam-s2-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        env.set("DELONIX_ROOT", &dir);
        let got = reserve("10.88", "fixed00000000c1", "10.88.4.4");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        // As root the mode bits do not apply, and then `Ok` is legitimate.
        // SAFETY: `geteuid` takes no arguments and has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            let e = got.expect_err("reserve without the lock had to refuse");
            assert!(format!("{e}").contains("same IP to two containers"), "{e}");
        }
    }

    /// Finding 4: the VM DHCP hands out `<prefix>.254.10–.249` from the MAC,
    /// outside the IPAM, and the IPAM walked the whole /16 — the pool included.
    /// An id whose preferred address is EXACTLY a VM's got it.
    #[test]
    fn allocate_never_hands_out_a_vm_dhcp_pool_address() {
        with_roots("pool", |_| {
            let vm_ip = crate::vm_dhcp_lease_ip("10.81", "52:54:00:12:34:56").unwrap();
            let host: u32 = vm_ip.rsplit('.').next().unwrap().parse().unwrap();
            let id = format!("{:08x}c0ffee00", 0xfe00 | host);
            assert_eq!(crate::derive_ip_in("10.81", &id), vm_ip);
            let ip = allocate("10.81", &id).unwrap();
            assert_ne!(ip, vm_ip, "the container got a VM's DHCP address");
            assert!(!crate::in_vm_dhcp_pool("10.81", &ip), "{ip}");
        });
    }

    /// And the probe skips the whole pool, not just the preferred address: a
    /// /16 with everything taken but the pool and one address returns that one.
    #[test]
    fn the_probe_skips_the_dhcp_pool() {
        let used: Vec<String> = (0u32..=0xffff)
            .map(|h| format!("10.81.{}.{}", h >> 8, h & 0xff))
            .filter(|ip| !crate::in_vm_dhcp_pool("10.81", ip) && ip != "10.81.254.5")
            .collect();
        let refs: std::collections::HashSet<&str> = used.iter().map(String::as_str).collect();
        assert_eq!(
            probe_free("10.81", "10.81.254.100", &refs).as_deref(),
            Some("10.81.254.5")
        );
    }

    /// A fixed IP asked for a container inside the VM pool is refused.
    #[test]
    fn reserving_inside_the_vm_dhcp_pool_is_refused() {
        with_roots("fixedpool", |_| {
            let e = reserve("10.82", "fixed00000000001", "10.82.254.50").unwrap_err();
            assert!(matches!(e, Error::IpInUse(_)), "{e}");
            assert_eq!(lookup("10.82", "fixed00000000001"), None);
        });
    }

    /// Finding 4, the other half: two VMs whose MACs hash onto one DHCP
    /// address. The second is refused instead of answering ARP for the first
    /// one's IP, and a VM's address shows in the registry.
    #[test]
    fn two_vms_on_one_dhcp_address_are_refused() {
        with_roots("vmvm", |_| {
            reserve_vm_dhcp("10.86", "vm-a", "10.86.254.77").unwrap();
            let e = reserve_vm_dhcp("10.86", "vm-b", "10.86.254.77").unwrap_err();
            assert!(matches!(e, Error::IpInUse(_)), "{e}");
            assert!(format!("{e}").contains("another name"), "{e}");
            assert_eq!(
                leases_of("vm-a"),
                vec![("10.86".into(), "vm-a".into(), "10.86.254.77".into())]
            );
            // Outside the pool it is not a DHCP address.
            assert!(reserve_vm_dhcp("10.86", "vm-c", "10.86.3.3").is_err());
            release_everywhere("vm-a");
            assert!(leases_of("vm-a").is_empty());
        });
    }

    /// CONCURRENCY: `reserve` and `allocate` in parallel, with the ids asking
    /// for the SAME address. Without the lock in `reserve`, or with a duplicate
    /// only warned about, two ids ended up on one IP.
    #[test]
    fn concurrent_reserve_and_allocate_never_duplicate_an_address() {
        with_roots("conc", |_| {
            let target = "10.87.5.5";
            let hs: Vec<_> = (0..16)
                .map(|i| {
                    std::thread::spawn(move || {
                        if i % 2 == 0 {
                            let _ = reserve("10.87", &format!("fix{i:013}"), target);
                        } else {
                            // This id's preferred address is the target (0x0505).
                            let _ = allocate("10.87", &format!("00000505{i:08}"));
                        }
                    })
                })
                .collect();
            for h in hs {
                h.join().unwrap();
            }
            let map = load("10.87").unwrap();
            let mut seen = std::collections::HashSet::new();
            for (id, ip) in &map {
                assert!(
                    seen.insert(ip.clone()),
                    "{ip} duplicated (id {id}): {map:?}"
                );
            }
            assert!(map.values().any(|v| v == target), "nobody got the target");
        });
    }
}
