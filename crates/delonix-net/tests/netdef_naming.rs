//! Regressão: o registo de uma rede de ingress era nomeado com `sanitize(name)`,
//! que **corta a 12 caracteres**.
//!
//! Esse limite existe para nomes de DISPOSITIVO (o kernel tem `IFNAMSIZ`); num
//! nome de ficheiro é perda pura. `producao-alpha` e `producao-alpine`
//! partilhavam o ficheiro `producao-alp.json` — e o que se lê num registo é a
//! BRIDGE, por isso os workloads da segunda iam parar à bridge da primeira,
//! com as duas a aparecerem separadas no `network ls`.
//!
//! Só filesystem: `network_create` não toca em netlink nem em namespaces.

use std::sync::OnceLock;

fn raiz() -> &'static std::path::PathBuf {
    static RAIZ: OnceLock<std::path::PathBuf> = OnceLock::new();
    RAIZ.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("delonix-net-naming-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::env::set_var("DELONIX_ROOT", &d);
        d
    })
}

fn dir_de_redes() -> std::path::PathBuf {
    raiz().join("ingress").join("networks")
}

#[test]
fn nomes_com_os_mesmos_12_caracteres_nao_partilham_registo() {
    raiz();
    let a = delonix_net::infra::network_create("producao-alpha").expect("alpha");
    let b = delonix_net::infra::network_create("producao-alpine").expect("alpine");

    assert_ne!(a.prefix, b.prefix, "duas redes no mesmo /16");
    assert_ne!(a.bridge, b.bridge, "duas redes na mesma bridge");

    // E cada nome resolve para o SEU registo — antes, o segundo `network_get`
    // devolvia o primeiro, com a bridge errada.
    let ga = delonix_net::infra::network_get("producao-alpha").expect("get alpha");
    let gb = delonix_net::infra::network_get("producao-alpine").expect("get alpine");
    assert_eq!((ga.name.as_str(), ga.bridge.as_str()), ("producao-alpha", a.bridge.as_str()));
    assert_eq!((gb.name.as_str(), gb.bridge.as_str()), ("producao-alpine", b.bridge.as_str()));

    let listadas: Vec<String> = delonix_net::infra::network_list()
        .into_iter()
        .map(|d| d.name)
        .filter(|n| n.starts_with("producao-"))
        .collect();
    assert_eq!(listadas.len(), 2, "o `ls` devia ver as duas: {listadas:?}");
}

#[test]
fn remover_uma_nao_destroi_a_vizinha_de_nome_parecido() {
    raiz();
    let manter = delonix_net::infra::network_create("contabilidade-a").expect("a");
    delonix_net::infra::network_create("contabilidade-b").expect("b");

    delonix_net::infra::network_remove("contabilidade-b");

    // O `rm` da segunda mandava `netdel` à bridge da PRIMEIRA e apagava-lhe o
    // registo — a rede que ninguém pediu para remover ficava sem bridge e sem
    // ficheiro, e o operador só descobria quando os workloads dela calassem.
    let sobrevivente = delonix_net::infra::network_get("contabilidade-a")
        .expect("a rede que não foi removida desapareceu");
    assert_eq!(sobrevivente.bridge, manter.bridge);
    assert_eq!(sobrevivente.prefix, manter.prefix);
    assert!(delonix_net::infra::network_get("contabilidade-b").is_none());
}

#[test]
fn remover_um_nome_inexistente_nao_toca_em_nada() {
    raiz();
    let antes = delonix_net::infra::network_create("logistica-primaria").expect("cria");
    // Partilha os 12 primeiros caracteres com a de cima, e nunca foi criada.
    delonix_net::infra::network_remove("logistica-primaria-2");
    let depois = delonix_net::infra::network_get("logistica-primaria").expect("continua lá");
    assert_eq!(depois.bridge, antes.bridge);
}

#[test]
fn registo_legado_continua_a_ser_lido_e_migra_a_escrita() {
    raiz();
    // Um registo escrito pela fórmula ANTIGA (truncada a 12), como está hoje no
    // disco de quem já corre isto. Perdê-lo seria perder a bridge e o /16 de uma
    // rede possivelmente com workloads ligados.
    std::fs::create_dir_all(dir_de_redes()).unwrap();
    let legado = dir_de_redes().join("arquivo-anti.json");
    std::fs::write(
        &legado,
        r#"{"name":"arquivo-antigo","bridge":"dlxnaaaabbbb","prefix":"10.249"}"#,
    )
    .unwrap();

    let lido = delonix_net::infra::network_get("arquivo-antigo").expect("o legado devia ser lido");
    assert_eq!(lido.bridge, "dlxnaaaabbbb");
    assert_eq!(lido.prefix, "10.249");

    // Uma rede, não duas, mesmo a meio da migração.
    assert_eq!(
        delonix_net::infra::network_list()
            .iter()
            .filter(|d| d.name == "arquivo-antigo")
            .count(),
        1
    );

    // A escrita seguinte migra-o: `network_create` é idempotente por nome, por
    // isso devolve o registo existente sem lhe mudar o prefixo.
    let de_novo = delonix_net::infra::network_create("arquivo-antigo").expect("idempotente");
    assert_eq!(de_novo.prefix, "10.249", "a idempotência não pode renumerar a rede");

    // O ficheiro legado só desaparece quando algo o reescreve — força-o por um
    // caminho que escreve mesmo (declarar o gateway).
    delonix_net::infra::network_create_with_gateway("arquivo-antigo", "10.249", Some("10.249.0.9"))
        .expect("declara gateway");
    assert!(!legado.exists(), "o registo legado ficou para trás");
    assert_eq!(
        delonix_net::infra::network_get("arquivo-antigo").unwrap().bridge,
        "dlxnaaaabbbb",
        "a migração não pode inventar uma bridge nova"
    );
}

/// NOTA: este passa também no código antigo, por outra razão (o nome do
/// ficheiro não é o que a fórmula antiga procurava). É guarda da fórmula NOVA —
/// que a verificação do campo `name` continue a existir — e não prova da
/// correção; essa são os quatro de cima.
#[test]
fn um_registo_de_outra_rede_nao_e_aceite_como_este() {
    raiz();
    // Defesa em profundidade: mesmo que dois nomes caiam no mesmo ficheiro, o
    // campo `name` do registo é a autoridade. O resultado é «não existe» — uma
    // recusa — e nunca a rede errada.
    std::fs::create_dir_all(dir_de_redes()).unwrap();
    std::fs::write(
        dir_de_redes().join("intruso-0000ffff.json"),
        r#"{"name":"outra-qualquer","bridge":"dlxn00000000","prefix":"10.248"}"#,
    )
    .unwrap();
    assert!(delonix_net::infra::network_get("intruso").is_none());
}
