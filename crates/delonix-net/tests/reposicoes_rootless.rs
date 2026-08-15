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
    let d = std::env::temp_dir().join(format!("dlx-ipt-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let f = d.join("save.txt");
    let mut fh = std::fs::File::create(&f).unwrap();
    writeln!(
        fh,
        "*filter\n:INPUT ACCEPT [0:0]\n:FORWARD DROP [0:0]\n-A INPUT -i lo -j ACCEPT\nCOMMIT"
    )
    .unwrap();

    let r = delonix_net::infra::import_iptables(&f).expect("devia analisar");
    assert!(r.contains("1 tabela"), "contagem de tabelas: {r}");
    assert!(r.contains("2 cadeia"), "contagem de cadeias: {r}");
    assert!(r.contains("1 regra"), "contagem de regras: {r}");

    let _ = std::fs::remove_dir_all(&d);
}

/// Um ficheiro que não existe dá erro NOMEADO — não um relatório de zeros.
///
/// A distinção importa: «não há regras nenhumas» e «não consegui ler o ficheiro»
/// levam quem migra a decisões opostas.
#[test]
fn import_iptables_recusa_ficheiro_ausente() {
    let e = delonix_net::infra::import_iptables(std::path::Path::new("/nao/existe/save.txt"))
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
    let d = std::env::temp_dir().join(format!("dlx-attach-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::env::set_var("DELONIX_ROOT", &d);

    let net = delonix_net::infra::network_create("rede-teste").expect("cria rede");
    let fora = "10.99.0.5";
    assert!(
        !fora.starts_with(&net.prefix),
        "o teste precisa de um IP fora de {}",
        net.prefix
    );

    let e = delonix_net::infra::attach_container_on_ip("abc123", "rede-teste", fora, "")
        .expect_err("devia recusar um IP de outra rede");
    let t = e.to_string();
    assert!(t.contains(fora), "o erro devia nomear o IP: {t}");
    assert!(t.contains("rede-teste"), "e a rede: {t}");

    let _ = std::fs::remove_dir_all(&d);
}
