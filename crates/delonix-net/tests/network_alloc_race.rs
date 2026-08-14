//! Regressão: a alocação do `/16` em `infra::network_create` era um
//! ler-depois-escrever **sem fechadura**.
//!
//! Duas criações em paralelo liam o mesmo conjunto de prefixos já usados e
//! escolhiam o MESMO. Medido antes da correção: 10 criações concorrentes davam
//! 2 a 4 prefixos distintos; 20 davam 10. As bridges diferem (o nome deriva do
//! da rede), por isso as redes PARECEM separadas — mas os workloads tiram
//! endereços do mesmo `/16` e qualquer regra indexada num IP fica ambígua entre
//! duas redes que o operador julga isoladas.
//!
//! Só filesystem: `network_create` não toca em netlink nem em namespaces, o que
//! torna a corrida testável sem privilégios. Threads (e não processos) chegam —
//! o `flock` é por *open file description*, e cada `acquire()` faz o seu
//! próprio `open`, logo threads do mesmo processo excluem-se de facto.

use std::collections::HashSet;
use std::sync::OnceLock;

/// `DELONIX_ROOT` é lido do ambiente do PROCESSO. Definir por-teste faz os
/// testes paralelos lutarem pela mesma variável, por isso é uma raiz por
/// processo, criada uma vez.
fn raiz() -> &'static std::path::PathBuf {
    static RAIZ: OnceLock<std::path::PathBuf> = OnceLock::new();
    RAIZ.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("delonix-net-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::env::set_var("DELONIX_ROOT", &d);
        d
    })
}

/// Cria `n` redes em paralelo e devolve os `NetDef` resultantes.
fn criar_em_paralelo(prefixo_do_nome: &str, n: usize) -> Vec<delonix_net::infra::NetDef> {
    raiz();
    let barreira = std::sync::Arc::new(std::sync::Barrier::new(n));
    let mut hs = Vec::with_capacity(n);
    for i in 0..n {
        let nome = format!("{prefixo_do_nome}-{i}");
        let b = barreira.clone();
        hs.push(std::thread::spawn(move || {
            // Todas as threads largam ao mesmo tempo — sem isto, arrancarem em
            // fila esconde a corrida que o teste existe para apanhar.
            b.wait();
            delonix_net::infra::network_create(&nome)
        }));
    }
    hs.into_iter()
        .map(|h| {
            h.join()
                .expect("thread em pânico")
                .expect("network_create falhou")
        })
        .collect()
}

#[test]
fn criacoes_concorrentes_nao_partilham_o_mesmo_16() {
    let defs = criar_em_paralelo("corrida", 16);

    let prefixos: HashSet<&str> = defs.iter().map(|d| d.prefix.as_str()).collect();
    assert_eq!(
        prefixos.len(),
        defs.len(),
        "duas redes distintas ficaram no mesmo /16 — prefixos: {:?}",
        defs.iter()
            .map(|d| (&d.name, &d.prefix))
            .collect::<Vec<_>>()
    );

    // E o que ficou em disco tem de concordar com o que foi devolvido: uma
    // escrita a pisar outra dá prefixos únicos na memória e duplicados no disco.
    let em_disco: Vec<_> = delonix_net::infra::network_list()
        .into_iter()
        .filter(|d| d.name.starts_with("corrida-"))
        .collect();
    assert_eq!(em_disco.len(), defs.len(), "redes perdidas no disco");
    let no_disco: HashSet<String> = em_disco.iter().map(|d| d.prefix.clone()).collect();
    assert_eq!(
        no_disco.len(),
        defs.len(),
        "prefixos duplicados no disco: {em_disco:?}"
    );
}

/// NOTA de honestidade: este passa também no código ANTIGO (6/6 corridas). Com
/// o registo vazio, todas as threads lêem o mesmo `used`, escolhem o mesmo
/// prefixo e escrevem `NetDef`s IDÊNTICOS — a escrita a pisar a outra é
/// invisível quando o conteúdo coincide. Fica como guarda do invariante (e da
/// re-verificação dentro da fechadura), não como prova da correção: essa é o
/// `criacoes_concorrentes_nao_partilham_o_mesmo_16`.
#[test]
fn o_mesmo_nome_em_paralelo_converge_numa_so_rede() {
    raiz();
    const N: usize = 12;
    let barreira = std::sync::Arc::new(std::sync::Barrier::new(N));
    let hs: Vec<_> = (0..N)
        .map(|_| {
            let b = barreira.clone();
            std::thread::spawn(move || {
                b.wait();
                delonix_net::infra::network_create("mesmo-nome")
            })
        })
        .collect();
    let defs: Vec<_> = hs
        .into_iter()
        .map(|h| h.join().unwrap().expect("network_create falhou"))
        .collect();

    // A re-verificação DENTRO da fechadura é o que garante isto: sem ela, quem
    // chegasse depois reescrevia o `NetDef` do vencedor com outro prefixo,
    // mudando a bridge por baixo do que já lá estivesse ligado.
    let prefixos: HashSet<&str> = defs.iter().map(|d| d.prefix.as_str()).collect();
    assert_eq!(
        prefixos.len(),
        1,
        "o mesmo nome deu redes diferentes: {prefixos:?}"
    );
    let bridges: HashSet<&str> = defs.iter().map(|d| d.bridge.as_str()).collect();
    assert_eq!(
        bridges.len(),
        1,
        "o mesmo nome deu bridges diferentes: {bridges:?}"
    );

    assert_eq!(
        delonix_net::infra::network_list()
            .iter()
            .filter(|d| d.name == "mesmo-nome")
            .count(),
        1
    );
}

#[test]
fn a_fechadura_nao_entra_no_registo_de_redes() {
    // A fechadura vive ao LADO do registo. Se caísse lá dentro, `network_list`
    // teria de a saltar por acidente (por falhar o parse) em vez de por desenho.
    let _ = criar_em_paralelo("vizinha", 2);
    let dir = raiz().join("ingress").join("networks");
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        assert!(
            n.ends_with(".json"),
            "ficheiro estranho no registo de redes: {n}"
        );
    }
    assert!(raiz().join("ingress").join("networks.lock").exists());
}
