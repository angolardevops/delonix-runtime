//! Nomes auto-gerados no padrão angolano — reis/rainhas + lugares de Angola.
//!
//! É a identidade de nomeação do produto inteiro: os clusters kind-mode já a
//! usavam (`random_cluster_name`) e os containers juntam-se-lhe aqui, em vez
//! do `dlx-<hash>` ilegível. Um nome que se lê e se diz aparece no `ls`, nos
//! logs e nas conversas de equipa — `njinga-benguela-07` conta uma história,
//! `dlx-a1fef9d5` não.

/// Reis/rainhas de Angola — Ndongo, Kongo, Matamba, Bailundo.
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

/// Províncias, municípios e comunas de Angola.
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

/// Deriva um nome `<rei>-<lugar>-<NN>` DETERMINÍSTICO a partir de `seed`
/// (o id do container), saltando os que `taken` acusar como ocupados.
///
/// Determinístico de propósito, não aleatório: o `run --net <rede>` re-executa
/// o processo (2 passagens, ver `reexec_into_netns`) e as duas têm de chegar
/// ao MESMO nome sem o transportar por fora — o id já viaja no
/// `DELONIX_REEXEC_ID`, o nome deriva dele. FNV-1a semeia, um LCG itera; ~50k
/// combinações e 50 tentativas contra os nomes existentes chegam de sobra.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesmo_seed_da_o_mesmo_nome() {
        // A garantia de que as duas passagens do re-exec convergem.
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
