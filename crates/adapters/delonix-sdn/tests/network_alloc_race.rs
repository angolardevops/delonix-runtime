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
use std::sync::{Mutex, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// `DELONIX_ROOT` is read from the PROCESS environment. Setting it per test
/// makes parallel tests fight over the same variable, so there is one root per
/// process and the variable is set once. This only names the path; the
/// directory itself exists while a [`RootLease`] does.
fn raiz() -> &'static std::path::PathBuf {
    static RAIZ: OnceLock<std::path::PathBuf> = OnceLock::new();
    RAIZ.get_or_init(|| {
        let tmp = std::env::temp_dir();
        sweep_dead_roots(&tmp);
        let d = tmp.join(format!("{ROOT_PREFIX}{}", std::process::id()));
        std::env::set_var("DELONIX_ROOT", &d);
        d
    })
}

const ROOT_PREFIX: &str = "delonix-net-race-";

/// libtest has no "after all tests" hook, and it does not tell a test how many
/// others will run (filters, `--test-threads`), so "the last test removes the
/// root" cannot be decided by counting tests. It is decided by counting the
/// tests USING the root right now: the first lease creates it, the last one to
/// be dropped removes it — also when that test panics, because `Drop` runs on
/// unwind. If a later test starts after the count hit zero, it simply gets a
/// fresh root; no test here depends on another's networks. Measured on
/// 2026-09-28 before this: 2400 runs left ~2400 roots in the temp dir.
static LEASES: Mutex<usize> = Mutex::new(0);

struct RootLease(&'static std::path::Path);

fn lease() -> RootLease {
    let root = raiz();
    let mut n = LEASES.lock().unwrap_or_else(|e| e.into_inner());
    if *n == 0 {
        let _ = std::fs::remove_dir_all(root);
        std::fs::create_dir_all(root).unwrap();
    }
    *n += 1;
    RootLease(root)
}

impl std::ops::Deref for RootLease {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        self.0
    }
}

impl Drop for RootLease {
    fn drop(&mut self) {
        let mut n = LEASES.lock().unwrap_or_else(|e| e.into_inner());
        *n -= 1;
        if *n == 0 {
            let _ = std::fs::remove_dir_all(self.0);
        }
    }
}

/// The lease does not run when the process is KILLED (Ctrl-C, a CI timeout,
/// SIGKILL), so each process also removes the roots of runs that died that
/// way: a `delonix-net-race-<pid>` whose pid is no longer alive has no owner
/// left to use it. A live pid is left alone even if it was reused by something
/// else — the cost of being wrong that way is one stale directory. Without
/// `/proc` every pid would look dead, so nothing is swept then.
fn sweep_dead_roots(tmp: &std::path::Path) {
    let proc = std::path::Path::new("/proc");
    if !proc.join("self").exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(ROOT_PREFIX))
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !proc.join(pid.to_string()).exists() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// The three tests share the root and run in parallel. The ones that CREATE
/// networks may overlap each other — that is the race they measure — so they
/// take this lock shared; the one that INSPECTS the registry folder takes it
/// exclusive, to see it quiet. Without it, it caught the
/// `.<record>.json.<pid>.<n>.tmp` another test's `write_atomic` had open at
/// that instant (CI `test (arm64)`, 2026-09-28; 506/800 runs locally with the
/// CPU constrained).
static REGISTRY: RwLock<()> = RwLock::new(());

fn writing() -> RwLockReadGuard<'static, ()> {
    REGISTRY.read().unwrap_or_else(|e| e.into_inner())
}

fn quiet() -> RwLockWriteGuard<'static, ()> {
    REGISTRY.write().unwrap_or_else(|e| e.into_inner())
}

/// Cria `n` redes em paralelo e devolve os `NetDef` resultantes.
/// The caller holds a [`RootLease`] for as long as it reads the results.
fn criar_em_paralelo(prefixo_do_nome: &str, n: usize) -> Vec<delonix_sdn::infra::NetDef> {
    let barreira = std::sync::Arc::new(std::sync::Barrier::new(n));
    let mut hs = Vec::with_capacity(n);
    for i in 0..n {
        let nome = format!("{prefixo_do_nome}-{i}");
        let b = barreira.clone();
        hs.push(std::thread::spawn(move || {
            // Todas as threads largam ao mesmo tempo — sem isto, arrancarem em
            // fila esconde a corrida que o teste existe para apanhar.
            b.wait();
            delonix_sdn::infra::network_create(&nome)
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
    let _g = writing();
    let _root = lease();
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
    let em_disco: Vec<_> = delonix_sdn::infra::network_list()
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
    let _g = writing();
    let _root = lease();
    const N: usize = 12;
    let barreira = std::sync::Arc::new(std::sync::Barrier::new(N));
    let hs: Vec<_> = (0..N)
        .map(|_| {
            let b = barreira.clone();
            std::thread::spawn(move || {
                b.wait();
                delonix_sdn::infra::network_create("mesmo-nome")
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
        delonix_sdn::infra::network_list()
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
    //
    // The assertion stays STRICT — anything that is not `.json` fails, temps
    // included — and it is the quiet folder that makes it deterministic, not an
    // exemption for `*.tmp`. With the writes finished, a temp still there is no
    // longer in flight: it is junk `write_atomic` left behind, and that must
    // break this test too.
    let _g = quiet();
    let root = lease();
    let _ = criar_em_paralelo("vizinha", 2);
    let dir = root.join("ingress").join("networks");
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        assert!(
            n.ends_with(".json"),
            "ficheiro estranho no registo de redes: {n}"
        );
    }
    assert!(root.join("ingress").join("networks.lock").exists());
}
