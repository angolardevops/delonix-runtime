//! Auto-generated names in the Angolan pattern — kings/queens + places in Angola.
//!
//! It's the naming identity of the whole product: the kind-mode clusters already
//! used it (`random_cluster_name`) and containers join it here, instead of the
//! unreadable `dlx-<hash>`. A name that can be read and spoken shows up in `ls`, in
//! the logs, and in team conversations — `njinga-benguela-07` tells a story,
//! `dlx-a1fef9d5` doesn't.

/// Kings/queens of Angola — Ndongo, Kongo, Matamba, Bailundo.
pub(crate) const REIS: &[&str] = &[
    "njinga",
    "mandume",
    "ekuikui",
    "nzinga",
    "kiluanji",
    "ngola",
    "mbandi",
    "kitamba",
    "katyavala",
    "samakaka",
    "kalandula",
    "mutu",
    "hoolo",
    "soba",
];

/// Provinces, municipalities, and communes of Angola.
pub(crate) const LUGARES: &[&str] = &[
    "luanda",
    "benguela",
    "huambo",
    "huila",
    "bie",
    "malanje",
    "uige",
    "zaire",
    "cunene",
    "namibe",
    "moxico",
    "bengo",
    "cuando",
    "cubango",
    "viana",
    "cacuaco",
    "belas",
    "talatona",
    "kilamba",
    "catumbela",
    "lobito",
    "lubango",
    "chibia",
    "cazenga",
    "sumbe",
    "ndalatando",
    "menongue",
    "saurimo",
    "dundo",
    "ondjiva",
    "caxito",
    "gabela",
    "quibala",
    "camacupa",
    "andulo",
    "chinguar",
];

/// Derives a DETERMINISTIC name `<king>-<place>-<NN>` from `seed`
/// (the container's id), skipping the ones that `taken` reports as occupied.
///
/// Deterministic on purpose, not random: `run --net <network>` re-executes
/// the process (2 passes, see `reexec_into_netns`) and both have to arrive
/// at the SAME name without carrying it externally — the id already travels in
/// `DELONIX_REEXEC_ID`, the name derives from it. FNV-1a seeds, an LCG iterates; ~50k
/// combinations and 50 attempts against the existing names are more than enough.
pub(crate) fn derived_name<F: Fn(&str) -> bool>(seed: &str, taken: F) -> Option<String> {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in seed.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    for _ in 0..50 {
        h = h
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (h >> 33) as usize;
        let name = format!(
            "{}-{}-{:02}",
            REIS[r % REIS.len()],
            LUGARES[(r / REIS.len()) % LUGARES.len()],
            (r / (REIS.len() * LUGARES.len())) % 100
        );
        if !taken(&name) {
            return Some(name);
        }
    }
    None
}

/// Same `<king>-<place>-<NN>` pattern as [`derived_name`], but randomly
/// seeded (time + pid) instead of derived from an existing id — for callers
/// with no natural seed to derive from (a brand-new cluster has no id yet,
/// unlike a container). Used by kind-mode `cluster create` and by `cluster
/// kubeadm` (see `cmd::kindmode::random_cluster_name`/
/// `cmd::cluster::random_kubeadm_cluster_name`) so every kind of
/// auto-generated cluster name reads the same way as an auto-named container.
pub(crate) fn random_name<F: Fn(&str) -> bool>(taken: F) -> Option<String> {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 20;
    for _ in 0..50 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (seed >> 33) as usize;
        let name = format!(
            "{}-{}-{:02}",
            REIS[r % REIS.len()],
            LUGARES[(r / REIS.len()) % LUGARES.len()],
            (r / (REIS.len() * LUGARES.len())) % 100
        );
        if !taken(&name) {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesmo_seed_da_o_mesmo_nome() {
        // The guarantee that the two re-exec passes converge.
        let a = derived_name("abc123", |_| false).unwrap();
        let b = derived_name("abc123", |_| false).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn seeds_diferentes_dao_nomes_diferentes() {
        let a = derived_name("abc123", |_| false).unwrap();
        let b = derived_name("xyz789", |_| false).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn nome_ocupado_avanca_para_o_seguinte() {
        let first = derived_name("abc123", |_| false).unwrap();
        let second = derived_name("abc123", |n| n == first).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn formato_rei_lugar_nn() {
        let n = derived_name("abc123", |_| false).unwrap();
        let parts: Vec<&str> = n.rsplitn(2, '-').collect();
        assert_eq!(parts[0].len(), 2, "sufixo NN: {n}");
        assert!(parts[0].chars().all(|c| c.is_ascii_digit()), "{n}");
    }
}
