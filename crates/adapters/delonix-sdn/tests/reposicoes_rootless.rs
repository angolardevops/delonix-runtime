//! As funções que o `Net` levou consigo, repostas pelo caminho rootless.
//!
//! Testa o que se pode testar sem holder: as VALIDAÇÕES e o relatório. O que
//! exige netns (o attach em si, a remoção de regras) fica para a validação ao
//! vivo — um teste que precise de holder deixa de correr no CI e passa a
//! decoração.

use std::io::Write;

/// `import_iptables` não toca no host: lê, conta e relata.
#[test]
fn import_iptables_conta_sem_aplicar() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("save.txt");
    let mut fh = std::fs::File::create(&f).unwrap();
    writeln!(
        fh,
        "*filter\n:INPUT ACCEPT [0:0]\n:FORWARD DROP [0:0]\n-A INPUT -i lo -j ACCEPT\nCOMMIT"
    )
    .unwrap();

    let r = delonix_sdn::infra::import_iptables(&f).expect("devia analisar");
    assert!(r.contains("1 tabela"), "contagem de tabelas: {r}");
    assert!(r.contains("2 cadeia"), "contagem de cadeias: {r}");
    assert!(r.contains("1 regra"), "contagem de regras: {r}");
}

/// Um ficheiro que não existe dá erro NOMEADO — não um relatório de zeros.
///
/// A distinção importa: «não há regras nenhumas» e «não consegui ler o ficheiro»
/// levam quem migra a decisões opostas.
#[test]
fn import_iptables_recusa_ficheiro_ausente() {
    let e = delonix_sdn::infra::import_iptables(std::path::Path::new("/nao/existe/save.txt"))
        .expect_err("devia falhar");
    let t = e.to_string();
    assert!(
        t.contains("save.txt"),
        "o erro devia nomear o ficheiro: {t}"
    );
}

/// O IP de um attach com endereço escolhido é validado contra a rede.
///
/// Um endereço de fora do prefixo não é um pedido exótico, é um engano: aplicá-lo
/// daria um container inalcançável com um attach bem-sucedido — o pior dos dois
/// mundos, porque nada assinala o problema.
#[test]
fn attach_on_ip_recusa_endereco_fora_da_rede() {
    // The only test in this binary that reads `DELONIX_ROOT`, so a per-test
    // dir is safe even though the variable is process-wide — re-checked
    // 2026-10-07 against `import_iptables_*`, which touch no state root.
    let d = tempfile::tempdir().unwrap();
    std::env::set_var("DELONIX_ROOT", d.path());

    let net = delonix_sdn::infra::network_create("rede-teste").expect("cria rede");
    let fora = "10.99.0.5";
    assert!(
        !fora.starts_with(&net.prefix),
        "o teste precisa de um IP fora de {}",
        net.prefix
    );

    let e = delonix_sdn::infra::attach_container_on_ip("abc123", "rede-teste", fora, "")
        .expect_err("devia recusar um IP de outra rede");
    let t = e.to_string();
    assert!(t.contains(fora), "o erro devia nomear o IP: {t}");
    assert!(t.contains("rede-teste"), "e a rede: {t}");

    // A removal that fails FAILS the test: `TempDir`'s own `Drop` ignores the
    // error, and a cleanup that reports nothing is how a stray `.tmpXXXXXX`
    // comes to sit in `TMPDIR` (`scripts/tmp_roots_gate.py`).
    //
    // The pin is deliberately NOT removed afterwards — `scripts/arch_fitness.py`
    // ratchets the number of env writes, and a second one here would be new
    // debt for no gain: what made a dead root dangerous was production code
    // RESOLVING it to create state, and since 2026-10-07 the one path that did
    // (`NetworkStore`'s default-octet cache) uses the store's own root
    // (`delonix_sdn::testenv::TempRoot` records the whole failure).
    d.close().expect("temp root removed");
}
