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
        Some(IpamLock(fd))
    }

    /// The error every caller reports when the lock cannot be taken. Naming the
    /// consequence matters: "could not lock" alone reads like a transient
    /// annoyance, when what it prevents is a duplicate address.
    fn unavailable() -> Error {
        Error::Runtime {
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
fn load(prefix: &str) -> Option<BTreeMap<String, String>> {
    let bytes = std::fs::read(prefix_file(prefix)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persists the `id → ip` map of a prefix (pretty, like the `NetDef`s).
///
/// **Atomic AND durable** (temp → `fsync` → `rename` → `fsync` the dir, via
/// [`delonix_runtime_core::write_atomic`]): a lockless reader (`lookup`, on the
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
fn store(prefix: &str, map: &BTreeMap<String, String>) -> Result<()> {
    std::fs::create_dir_all(ipam_dir()).map_err(|e| Error::Runtime {
        context: "ipam dir",
        message: e.to_string(),
    })?;
    let json = serde_json::to_vec_pretty(map).map_err(|e| Error::Runtime {
        context: "ipam serialize",
        message: e.to_string(),
    })?;
    delonix_runtime_core::write_atomic(&prefix_file(prefix), &json).map_err(|e| Error::Runtime {
        context: "ipam write",
        message: e.to_string(),
    })
}

/// Every `/16` prefix that has a lease registry on disk.
///
/// Exists because until now **nothing could see this file**. `allocate`,
/// `reserve`, `lookup` and `release` are the whole public surface, and every
/// one of them answers about ONE id. There was no way to ask "how many leases
/// are there, and do they still belong to anybody" — which is how the registry
/// on the host this was written against reached **391 leases against 47 live
/// containers** with nobody noticing. A leak nothing can display is a leak
/// nobody fixes.
pub fn prefixes() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(ipam_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_string_lossy()
                .strip_suffix(".json")
                .map(str::to_string)
        })
        .collect();
    out.sort();
    out
}

/// The `id → ip` map of a prefix, for reporting. Empty when the prefix has no
/// registry — never creates one, same contract as [`load`].
pub fn entries(prefix: &str) -> BTreeMap<String, String> {
    load(prefix).unwrap_or_default()
}

/// **PURE** — which leases of a prefix no longer belong to anybody.
///
/// Two independent tests, and neither is enough on its own:
///
/// * `live` — the ids the CALLER knows are alive, assembled by it exactly as
///   [`crate::infra::reap_orphan_refs`] receives its own. Never discovered
///   here: a reaper that decides for itself what is alive is the shape that
///   made published ports die on a host where the caller's list was partial.
/// * `attached` — the ids holding a ref-marker in the ingress. A marker means
///   the id has a wire inside the holder, which is a different question from
///   whether a container record exists, and it is the one that covers the
///   window this reaper has to survive: `attach_container` writes the LEASE
///   first and the marker immediately after, so an id mid-attach has a lease
///   and, a moment later, a marker.
///
/// A lease is a candidate only when BOTH say no. Even then the caller must not
/// act on one reading — see [`crate::infra::confirm_orphan_leases`] for why the
/// verdict is taken twice.
///
/// # The failure mode this leaves, and why the marker covers it
///
/// `Store::list` skips, silently, any container record that fails to
/// deserialize — so a corrupt `containers/<id>.json` makes its owner vanish
/// from `live` while the container keeps running. On the `live` test alone,
/// this reaper would then take a running container's address and hand it to
/// the next one.
///
/// The `attached` test is what stops that: a container on a custom network
/// holds a ref-marker for as long as it has a wire in the holder, and the
/// marker is a plain file whose presence does not depend on any record parsing.
/// The two tests fail in different ways, which is the only reason having both
/// is worth more than having the stricter one twice.
///
/// Found while writing the proof for this, not by reading it: a hand-written
/// test record that did not deserialize made its lease read `unclaimed`, and it
/// took a real container to tell the difference.
pub fn orphan_leases(
    entries: &BTreeMap<String, String>,
    live: &std::collections::HashSet<String>,
    attached: &std::collections::HashSet<String>,
) -> Vec<String> {
    entries
        .keys()
        .filter(|id| !live.contains(*id) && !attached.contains(*id))
        .cloned()
        .collect()
}

/// Drops `ids` from a prefix's registry. Under `flock`, idempotent, and it
/// re-reads under the lock — the list was computed outside it.
///
/// Returns how many leases were actually dropped.
pub fn release_many(prefix: &str, ids: &[String]) -> usize {
    let Some(_lock) = IpamLock::acquire() else {
        return 0;
    };
    let Some(mut map) = load(prefix) else {
        return 0;
    };
    let mut n = 0;
    for id in ids {
        if map.remove(id).is_some() {
            n += 1;
        }
    }
    if n > 0 && store(prefix, &map).is_err() {
        return 0;
    }
    n
}

/// Allocates (or returns the existing lease of) a unique IP in `prefix`'s `/16` for
/// `id`. Idempotent: an already-registered id always returns the SAME IP. For a new id,
/// it starts from the preferred hash IP and, if held by another id, linearly probes the
/// rest of the `/16`. Clear error if the `/16` is full (~65k hosts). Under `flock`.
pub fn allocate(prefix: &str, id: &str) -> Result<String> {
    let _lock = IpamLock::acquire().ok_or_else(IpamLock::unavailable)?;
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
    // Sem lock não se escreve: um read-modify-write destravado aqui apaga o
    // lease de outro container tão bem como no `allocate`.
    let Some(_lock) = IpamLock::acquire() else {
        tracing::error!(
            prefix = %prefix, container_id = %id,
            "{}", IpamLock::unavailable()
        );
        return;
    };
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
    // Idem: um release destravado pode reescrever o mapa por cima de um
    // allocate concorrente e ressuscitar um lease já libertado.
    let Some(_lock) = IpamLock::acquire() else {
        tracing::error!(
            prefix = %prefix, container_id = %id,
            "{}", IpamLock::unavailable()
        );
        return;
    };
    if let Some(mut map) = load(prefix) {
        if map.remove(id).is_some() {
            let _ = store(prefix, &map);
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

    /// As duas condições são independentes, e ambas são precisas.
    ///
    /// A do ref-marker existe por causa de uma janela de duas linhas: o
    /// `attach_container` escreve o LEASE e só depois o marcador, por isso um
    /// id a meio de um attach não tem registo de container nenhum. Ceifá-lo aí
    /// entrega o mesmo endereço a um segundo container enquanto o primeiro
    /// ainda está a ser construído — a colisão que este módulo existe para
    /// eliminar.
    #[test]
    fn um_lease_so_e_orfao_quando_nem_o_container_nem_o_marcador_o_reclamam() {
        let entries: std::collections::BTreeMap<String, String> = [
            ("vivo", "10.210.0.2"),
            ("so-marcador", "10.210.0.3"),
            ("abandonado", "10.210.0.4"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        let live: std::collections::HashSet<String> = ["vivo".to_string()].into_iter().collect();
        let attached: std::collections::HashSet<String> =
            ["so-marcador".to_string()].into_iter().collect();

        assert_eq!(
            super::orphan_leases(&entries, &live, &attached),
            vec!["abandonado".to_string()],
            "um id a meio de um attach tem marcador e nenhum registo — não é órfão"
        );

        // Sem a metade do marcador, o id em voo era condenado.
        let sem_marcador = std::collections::HashSet::new();
        let mut sem = super::orphan_leases(&entries, &live, &sem_marcador);
        sem.sort();
        assert_eq!(
            sem,
            vec!["abandonado".to_string(), "so-marcador".to_string()]
        );

        // E um registo vazio não condena nada.
        assert!(super::orphan_leases(
            &Default::default(),
            &Default::default(),
            &Default::default()
        )
        .is_empty());
    }

    /// O `release_many` é idempotente e só toca no que lhe foi nomeado — a
    /// lista é calculada FORA do lock, por isso a remoção volta a lê-la debaixo
    /// dele e um id que entretanto desapareceu não conta.
    #[test]
    fn a_ceifa_de_leases_e_idempotente_e_cirurgica() {
        // `with_root` e não um tmpdir próprio: o `DELONIX_ROOT` é global ao
        // processo, e um mutex próprio não serializa nada — é o bug que o
        // doc-comment do `ENV_LOCK` descreve, e que este teste apanhou ao ser
        // escrito assim à primeira.
        with_root("gc", || {
            let pfx = "10.251";
            super::reserve(pfx, "a", "10.251.0.2");
            super::reserve(pfx, "b", "10.251.0.3");
            super::reserve(pfx, "c", "10.251.0.4");
            assert_eq!(super::entries(pfx).len(), 3);

            let levados = super::release_many(pfx, &["b".to_string(), "inexistente".to_string()]);
            assert_eq!(levados, 1, "só o que existia conta");
            let restam = super::entries(pfx);
            assert_eq!(restam.len(), 2);
            assert!(restam.contains_key("a") && restam.contains_key("c"));

            // Repetir não muda nada.
            assert_eq!(super::release_many(pfx, &["b".to_string()]), 0);
            assert_eq!(super::entries(pfx).len(), 2);

            // E o prefixo aparece na listagem, que é a razão de ela existir.
            assert!(super::prefixes().iter().any(|p| p == pfx));
        });
    }

    /// **Esgotar um prefixo por inteiro** — a prova anti-colisão que um /16
    /// nunca dá, porque ninguém enche 65 mil endereços num teste.
    ///
    /// Num /28 há 16 endereços e 13 utilizáveis. A sonda tem de devolver os 13,
    /// todos distintos e todos dentro, e depois dizer que não há mais — em vez
    /// de repetir um (colisão silenciosa: dois containers com o mesmo IP, e a
    /// rede a funcionar para um deles) ou de devolver um de fora.
    #[test]
    fn esgotar_um_28_da_13_enderecos_distintos_e_depois_nada() {
        let cidr = "192.168.1.0/28";
        let net = crate::Cidr::parse(cidr).unwrap();
        let mut usados: std::collections::HashSet<String> = std::collections::HashSet::new();
        for i in 0..13 {
            let refs: std::collections::HashSet<&str> = usados.iter().map(String::as_str).collect();
            let preferido = crate::derive_ip_in(cidr, &format!("{i:08x}"));
            let ip = probe_free(cidr, &preferido, &refs)
                .unwrap_or_else(|| panic!("sem endereço à {i}.ª volta, com {} usados", refs.len()));
            let a = crate::Cidr::parse_addr(&ip).unwrap();
            assert!(net.contains(a), "{ip} fora do prefixo");
            assert_ne!(a, net.base);
            assert_ne!(a, net.base + 1);
            assert_ne!(a, net.last());
            assert!(usados.insert(ip.clone()), "REPETIU {ip}");
        }
        assert_eq!(usados.len(), 13);
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
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_root<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let dir = std::env::temp_dir().join(format!("dlx-ipam-nolock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Read-only root: `ipam/` cannot be created, so the lock file cannot be
        // opened — the same shape as a full disk or a lost mount.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        // SAFETY: single-threaded under the mutex above.
        unsafe { std::env::set_var("DELONIX_ROOT", &dir) };
        let got = allocate("10.88", "cafe0001");
        unsafe { std::env::remove_var("DELONIX_ROOT") };

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
}
