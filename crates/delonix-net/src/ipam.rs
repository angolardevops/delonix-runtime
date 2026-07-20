//! IPAM com registo de **leases** — o alocador anti-colisão do `/16`.
//!
//! O hash puro ([`crate::derive_ip_in`]) dá apenas o IP **preferido** de um id.
//! Sozinho, colide: mapeia 32 bits do id em 16 bits de host (`a.b`), portanto por
//! **aniversário** dois ids distintos batem no mesmo IP com ~50% de probabilidade
//! já aos ~300 containers num mesmo `/16` — dois containers com o MESMO IP =
//! rede partida, anti-spoof a dropar, e regras de firewall/DNAT indexadas no IP
//! errado.
//!
//! Este módulo garante **unicidade real**: um lease `id → ip` persistido por
//! `/16` (um ficheiro JSON por prefixo em `<base_root>/ipam/<prefixo>.json`),
//! protegido por `flock` (o CRI é concorrente). A alocação parte do IP preferido
//! e, se estiver ocupado por OUTRO id, **sonda linearmente** o espaço de host do
//! `/16` até ao primeiro livre. Determinístico e estável: o mesmo id devolve
//! sempre o mesmo IP (os caminhos de limpeza — detach/publish/firewall —
//! recomputam o IP pelo id e dependem disso).
//!
//! Fronteira de responsabilidade: `allocate` cria o lease (no attach), `release`
//! liberta-o (no detach), `lookup` só lê (nos recomputadores de cleanup, nunca
//! cria ficheiro). A alocação corre sempre do lado do HOST (antes de falar com o
//! holder), logo o registo vive no `base_root` do host, como as `NetDef`.

use crate::infra::base_root;
use delonix_runtime_core::{Error, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Diretório do registo de leases (`<base_root>/ipam/`).
fn ipam_dir() -> PathBuf {
    base_root().join("ipam")
}

/// Ficheiro de leases de um `/16` (um por prefixo, ex.: `10.88.json`). O prefixo
/// só tem dígitos e um ponto, mas saneamos por segurança (nunca vai a um caminho
/// com `/`/`..`).
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

/// Trinco exclusivo (`flock`) do registo IPAM — serializa read-modify-write de
/// `allocate`/`release` concorrentes. Um único trinco global chega (as operações
/// são curtas e raras face ao ciclo de vida do container). O `Drop` liberta.
struct IpamLock(i32);
impl IpamLock {
    fn acquire() -> IpamLock {
        let _ = std::fs::create_dir_all(ipam_dir());
        let path = ipam_dir().join("lock");
        let c = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes().to_vec())
            .unwrap_or_else(|_| std::ffi::CString::new("/tmp/dlxipamlock").unwrap());
        // SAFETY: open/flock com caminho válido; -1 em falha trata-se a seguir.
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
            // SAFETY: fd próprio, aberto em acquire().
            unsafe {
                libc::flock(self.0, libc::LOCK_UN);
                libc::close(self.0);
            }
        }
    }
}

/// Lê o mapa `id → ip` de um prefixo. Devolve `None` se o ficheiro não existir
/// (nunca o cria — importante para `lookup` não semear estado ao recomputar um IP
/// de limpeza, e para os testes puros que só derivam).
fn load(prefix: &str) -> Option<BTreeMap<String, String>> {
    let bytes = std::fs::read(prefix_file(prefix)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persiste o mapa `id → ip` de um prefixo (pretty, como as `NetDef`).
/// **Escrita atómica** (ficheiro temporário + `rename`): um leitor sem trinco
/// (`lookup`, no caminho de limpeza) nunca vê um ficheiro truncado a meio de um
/// `store` concorrente — veria o mapa ANTIGO ou o NOVO, nunca lixo. Sem isto, uma
/// leitura rasgada devolvia `None` e a limpeza caía no IP DERIVADO (errado, se o
/// real tinha sido sondado por cima de uma colisão), deixando regras órfãs.
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
    // O tmp fica no MESMO diretório (rename atómico só dentro do mesmo filesystem);
    // sufixado pelo pid para dois processos sob o flock não pisarem o mesmo tmp.
    // SAFETY: getpid() não tem pré-condições.
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

/// Aloca (ou devolve o lease existente de) um IP único no `/16` de `prefix` para
/// `id`. Idempotente: um id já registado devolve sempre o MESMO IP. Num id novo,
/// parte do IP preferido do hash e, se ocupado por outro id, sonda linearmente o
/// resto do `/16`. Erro claro se o `/16` estiver cheio (~65k hosts). Sob `flock`.
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

/// Sonda linear pelo espaço de host do `/16`, começando no host do IP preferido
/// (localidade — o IP fica perto do determinístico), saltando reservados
/// (`.0.0`/`.0.1`/`.255.255`) e os já em uso. `None` se o `/16` estiver cheio.
fn probe_free(
    prefix: &str,
    preferred: &str,
    used: &std::collections::HashSet<&str>,
) -> Option<String> {
    // host de partida = os dois últimos octetos do preferido como u16 (a*256+b).
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

/// Regista um lease `id → ip` FIXADO (IP escolhido pelo utilizador no attach),
/// para a sondagem de outros containers o ver como ocupado e nunca o reatribuir.
/// Idempotente. Sob `flock`.
pub fn reserve(prefix: &str, id: &str, ip: &str) {
    let _lock = IpamLock::acquire();
    let mut map = load(prefix).unwrap_or_default();
    if map.get(id).map(String::as_str) == Some(ip) {
        return;
    }
    // AVISO se o IP fixado já pertence (por lease) a OUTRO container: não o
    // rejeitamos (o utilizador escolheu-o explicitamente), mas não o silenciamos —
    // ficariam dois containers com o mesmo IP na wire.
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

/// Consulta o IP em lease de `id` no `/16` de `prefix`, sem criar nada. `None` se
/// não houver lease (o chamador cai então no IP derivado do hash — compat com um
/// container pré-existente a este registo, ou ainda não atacado).
pub fn lookup(prefix: &str, id: &str) -> Option<String> {
    load(prefix)?.get(id).cloned()
}

/// Liberta o lease de `id` no `/16` de `prefix` (no detach). Best-effort e
/// idempotente. Sob `flock`.
pub fn release(prefix: &str, id: &str) {
    let _lock = IpamLock::acquire();
    if let Some(mut map) = load(prefix) {
        if map.remove(id).is_some() {
            let _ = store(prefix, &map);
        }
    }
}

/// O prefixo `/16` (`a.b`) de um IP `a.b.c.d` — para libertar o lease no detach a
/// partir do IP conhecido, sem o chamador ter de passar o prefixo.
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

    /// Isola o registo num tmpdir (via `DELONIX_ROOT`) para não tocar no armazém
    /// real do utilizador. Serializado por um trinco de processo — os testes deste
    /// módulo partilham a env var global `DELONIX_ROOT`.
    fn with_root<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dlx-ipam-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: teste single-thread sob o Mutex acima.
        unsafe { std::env::set_var("DELONIX_ROOT", &dir) };
        let out = f();
        unsafe { std::env::remove_var("DELONIX_ROOT") };
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn ids_que_colidiam_no_hash_recebem_ips_distintos() {
        with_root("collide", || {
            // "deadbeef1234" e "deadbeef9999" derivam o MESMO IP preferido (partilham
            // os 8 primeiros hex) — era exatamente a colisão do alocador antigo.
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
            // lookup de um id sem lease não cria nada e devolve None.
            assert_eq!(lookup("10.88", "naoexiste"), None);
        });
    }

    #[test]
    fn release_liberta_o_ip_para_reuso() {
        with_root("release", || {
            let ip = allocate("10.88", "deadbeef1234").unwrap();
            // um segundo id colidente ficou com um IP sondado (!= ip).
            let other = allocate("10.88", "deadbeef9999").unwrap();
            assert_ne!(ip, other);
            release("10.88", "deadbeef1234");
            assert_eq!(lookup("10.88", "deadbeef1234"), None);
            // o IP libertado volta a ser o preferido de quem o derivava.
            let reuse = allocate("10.88", "deadbeef1234").unwrap();
            assert_eq!(reuse, ip);
        });
    }

    #[test]
    fn muitos_ids_zero_colisoes() {
        // O bug original: por aniversário, a colisão num /16 tornava-se provável aos
        // ~300 containers e quase certa aos ~600. Alocamos 2000 ids (>3× esse
        // limiar) e exigimos IPs TODOS distintos e válidos — a prova de que o
        // registo + sondagem elimina a colisão à escala. (O ficheiro por-prefixo é
        // reescrito por inteiro a cada allocate — O(n) I/O por attach; 2000 chega
        // para a garantia sem tornar o teste O(n²) lento.)
        with_root("stress", || {
            let mut seen = std::collections::HashSet::new();
            for i in 0..2000u32 {
                let id = format!("{:08x}dead", i.wrapping_mul(2_654_435_761)); // espalha
                let ip = allocate("10.88", &id).unwrap();
                assert!(crate::valid_ip_in_subnet("10.88", &ip), "IP inválido {ip}");
                assert!(seen.insert(ip.clone()), "COLISÃO no IP {ip} (id {id})");
            }
            assert_eq!(seen.len(), 2000);
        });
    }

    #[test]
    fn multi_homing_lease_por_rede_e_release_isolado() {
        // Um container multi-homed tem um lease em CADA /16 (rede primária + extra),
        // no ficheiro de prefixo respetivo. O disconnect da rede extra
        // (`detach_extra_container`, que agora recebe o ip) tem de libertar SÓ o
        // lease da extra, sem tocar no da primária. Regressão do leak v1.
        with_root("multihoming", || {
            let id = "cafebabe0001";
            let primary = allocate("10.88", id).unwrap(); // rede primária
            let extra = allocate("10.204", id).unwrap(); // rede adicional
            assert_eq!(prefix_of(&primary), "10.88");
            assert_eq!(prefix_of(&extra), "10.204");
            // disconnect da extra: liberta só o lease de 10.204 (via prefix_of(ip)).
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
