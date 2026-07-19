//! Armazém de estado dos containers — um ficheiro JSON por container.
//!
//! Reaproveita o padrão de *snapshot* JSON do `kvstore` (Mês 3): cada container
//! é persistido em `root/<id>.json`, com escrita atómica (ficheiro temporário +
//! `rename`).

use crate::{Container, Error, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs;
use std::marker::PhantomData;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Sequência para tornar o ficheiro temporário de [`Store::save`] único POR
/// ESCRITOR. O pid sozinho não chega: o servidor CRI é multi-thread
/// (`tokio::spawn_blocking`), logo duas threads do MESMO processo podiam
/// colidir no mesmo temp.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Trinco exclusivo de ficheiro (`flock`) — sequencia o **read-modify-write**
/// de um container ENTRE PROCESSOS. Mesmo padrão do `delonix-net::infra`.
///
/// Porque é preciso: este runtime é daemonless — N processos (`delonix` na CLI,
/// o servidor `delonix-cri` que o kubelet chama, e este é CONCORRENTE por
/// desenho) mutam o mesmo JSON. A escrita atómica (temp+`rename`) evita
/// ficheiros RASGADOS, mas não evita o **lost update** clássico: dois leitores
/// lêem o mesmo estado, ambos modificam, ambos gravam — uma das mudanças
/// desaparece em silêncio (ex.: um `RemoveContainer` desfeito por um reconcile
/// concorrente que regrava o registo antigo).
struct FileLock(fs::File);

impl FileLock {
    /// Adquire o trinco (bloqueia até o obter). `None` se o ficheiro de trinco
    /// não puder sequer ser aberto — nesse caso o chamador segue sem trinco
    /// (degradação graciosa: melhor que recusar a operação).
    fn acquire(path: &Path) -> Option<FileLock> {
        let f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .ok()?;
        // SAFETY: fd válido e aberto; LOCK_EX bloqueia até o trinco ser nosso.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return None;
        }
        Some(FileLock(f))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: fd ainda aberto (somos donos do File até aqui). O `close` do
        // File também libertaria o flock; explícito para não depender disso.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Sanitiza uma chave/id para um nome de ficheiro seguro (`a-z0-9._-`,
/// preservando maiúsculas). Bloqueia path traversal (`../`, `/etc/passwd`,
/// separadores) mapeando qualquer carácter fora dessa allowlist para `-`.
/// Partilhado por [`Store`] e [`JsonStore`] — **todo** id/chave vindo de fora
/// (ex.: `Path<String>` de handlers axum em `delonix-api`) tem de passar por
/// aqui antes de entrar num `PathBuf::join`.
pub(crate) fn safe_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// O armazém de estado, enraizado num directório.
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Abre (criando) o armazém no directório `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// `$DELONIX_ROOT/containers`, ou — **rootless** (sem privilégios) — o armazém
    /// do utilizador (`$XDG_DATA_HOME/delonix` ou `~/.local/share/delonix`), ou
    /// `/var/lib/delonix/containers` quando root. Consistente com
    /// `ImageStore::default_root` para o `run` rootless funcionar sem `sudo`.
    pub fn default_root() -> PathBuf {
        if let Some(root) = std::env::var_os("DELONIX_ROOT") {
            return PathBuf::from(root).join("containers");
        }
        // SAFETY: geteuid() é sempre seguro e não falha.
        if unsafe { libc::geteuid() } != 0 {
            let base = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
                .unwrap_or_else(|| PathBuf::from("."));
            return base.join("delonix").join("containers");
        }
        PathBuf::from("/var/lib/delonix/containers")
    }

    /// O diretório-base (`$DELONIX_ROOT`) — o pai de `containers`. Usado por
    /// subsistemas que vivem ao lado (ex.: [`crate::SecretStore`] em `<base>/secrets`).
    pub fn base(&self) -> PathBuf {
        self.root
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.root.clone())
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{}.json", safe_key(id)))
    }

    /// Ficheiro de trinco de um container (ver [`FileLock`]). Fica ao lado do
    /// estado e NUNCA é apagado — apagá-lo abriria uma janela em que dois
    /// processos trancam inodes diferentes e ambos entram na secção crítica.
    fn lock_path(&self, id: &str) -> PathBuf {
        self.root.join(format!(".{}.lock", safe_key(id)))
    }

    /// Persiste um container (escrita atómica).
    ///
    /// O temporário é único **por escritor** (pid + sequência): com um nome
    /// fixo (`.<id>.tmp`), dois processos a gravar o MESMO container escreviam
    /// por cima um do outro no mesmo ficheiro e o `rename` publicava um JSON
    /// entrelaçado — a atomicidade do `rename` não salva nada se o conteúdo do
    /// temp já vem corrompido.
    pub fn save(&self, c: &Container) -> Result<()> {
        let safe = safe_key(&c.id);
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .root
            .join(format!(".{safe}.{}.{seq}.tmp", std::process::id()));
        let write = || -> Result<()> {
            fs::write(&tmp, serde_json::to_vec_pretty(c)?)?;
            fs::rename(&tmp, self.path(&c.id))?;
            Ok(())
        };
        let r = write();
        if r.is_err() {
            let _ = fs::remove_file(&tmp); // não deixar lixo se falhou a meio
        }
        r
    }

    /// **Read-modify-write seguro** de um container: tranca (`flock`), relê o
    /// estado JÁ sob o trinco, aplica `f` e grava — tudo como uma secção
    /// crítica entre processos.
    ///
    /// Usar isto (e não `load` + mutar + `save`) sempre que a mudança dependa
    /// do estado ACTUAL. O padrão ingénuo perde escritas quando o CRI (que é
    /// concorrente) e a CLI mexem no mesmo container ao mesmo tempo.
    ///
    /// `f` devolve `false` para abortar a gravação (nada muda). O container
    /// devolvido é o estado final (ou o lido, se abortou).
    pub fn update<F>(&self, id_or_name: &str, f: F) -> Result<Container>
    where
        F: FnOnce(&mut Container) -> bool,
    {
        // Resolve o id REAL primeiro (aceita prefixo/nome), para trancar sempre
        // o mesmo ficheiro de trinco independentemente de como foi referido.
        let id = self.load(id_or_name)?.id;
        let _lock = FileLock::acquire(&self.lock_path(&id));
        // Relê SOB o trinco: entre o resolve e o `flock` outro processo pode ter
        // gravado; usar o valor lido antes reintroduziria o lost update.
        let mut c = self.load(&id)?;
        if !f(&mut c) {
            return Ok(c);
        }
        self.save(&c)?;
        Ok(c)
    }

    /// Carrega um container por id exacto, prefixo de id, ou nome.
    pub fn load(&self, id_or_name: &str) -> Result<Container> {
        let exact = self.path(id_or_name);
        if exact.exists() {
            return Ok(serde_json::from_slice(&fs::read(exact)?)?);
        }
        for c in self.list()? {
            if c.id.starts_with(id_or_name) || c.name == id_or_name {
                return Ok(c);
            }
        }
        Err(Error::NotFound(id_or_name.to_string()))
    }

    /// Lista todos os containers, do mais recente para o mais antigo.
    pub fn list(&self) -> Result<Vec<Container>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(bytes) = fs::read(&path) {
                    if let Ok(c) = serde_json::from_slice::<Container>(&bytes) {
                        out.push(c);
                    }
                }
            }
        }
        out.sort_by_key(|c| std::cmp::Reverse(c.created_unix));
        Ok(out)
    }

    /// Remove o ficheiro de estado de um container.
    pub fn remove(&self, id: &str) -> Result<()> {
        let p = self.path(id);
        if !p.exists() {
            return Err(Error::NotFound(id.to_string()));
        }
        fs::remove_file(p)?;
        Ok(())
    }
}

/// Armazém genérico tipado — um ficheiro JSON por item, indexado por uma chave
/// (nome). Reaproveita o mesmo padrão atómico (temp + `rename`) do [`Store`],
/// para tipos que não são `Container`: VMs ([`crate::Vm`]) e os manifestos
/// aplicados (estado desejado do daemon `reconcile`).
pub struct JsonStore<T> {
    root: PathBuf,
    _t: PhantomData<T>,
}

impl<T: Serialize + DeserializeOwned> JsonStore<T> {
    /// Abre (criando) o armazém no directório `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            _t: PhantomData,
        })
    }

    fn path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{}.json", safe_key(key)))
    }

    /// Persiste um item sob `key` (escrita atómica).
    pub fn save(&self, key: &str, value: &T) -> Result<()> {
        let safe = safe_key(key);
        // Temp único por escritor — ver a nota em `Store::save`.
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .root
            .join(format!(".{safe}.{}.{seq}.tmp", std::process::id()));
        fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
        fs::rename(&tmp, self.path(key))?;
        Ok(())
    }

    /// Carrega um item por chave exacta.
    pub fn load(&self, key: &str) -> Result<T> {
        let p = self.path(key);
        if !p.exists() {
            return Err(Error::NotFound(key.to_string()));
        }
        Ok(serde_json::from_slice(&fs::read(p)?)?)
    }

    /// `true` se existe um item com esta chave.
    pub fn exists(&self, key: &str) -> bool {
        self.path(key).exists()
    }

    /// Lista todos os itens (ordem do sistema de ficheiros).
    pub fn list(&self) -> Result<Vec<T>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(bytes) = fs::read(&path) {
                    if let Ok(v) = serde_json::from_slice::<T>(&bytes) {
                        out.push(v);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Remove o item de uma chave (idempotente: ausência não é erro).
    pub fn remove(&self, key: &str) -> Result<()> {
        let p = self.path(key);
        if p.exists() {
            fs::remove_file(p)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Container;

    fn tmp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "delonix-store-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn safe_key_neutraliza_path_traversal() {
        // `.` é um carácter permitido (ids/nomes legítimos têm pontos), mas `/`
        // é SEMPRE substituído — por isso "../" nunca sobrevive como separador:
        // o resultado é sempre UM SÓ componente de nome de ficheiro, mesmo que
        // contenha ".." like a substring. `PathBuf::join` só interpreta ".."
        // como travessia quando é um componente inteiro (delimitado por `/`);
        // dentro de um único componente sem `/`, é só texto inofensivo.
        assert_eq!(safe_key("../../etc/passwd"), "..-..-etc-passwd");
        assert_eq!(safe_key("a/../../b"), "a-..-..-b");
        assert!(!safe_key("../../../root/.ssh/authorized_keys").contains('/'));
        // ids normais (hex/uuid) passam intactos — sem regressão de comportamento.
        assert_eq!(safe_key("a1b2c3d4e5f6"), "a1b2c3d4e5f6");
        assert_eq!(safe_key("my-container_v1.2"), "my-container_v1.2");
    }

    #[test]
    fn store_path_traversal_nunca_escreve_fora_da_raiz() {
        let root = tmp_dir("store-path");
        let outside = root
            .parent()
            .unwrap()
            .join(format!("delonix-store-test-VICTIM-{}", std::process::id()));
        let store = Store::open(&root).unwrap();

        // um "id" malicioso vindo de um handler HTTP não validado.
        let evil_id = format!(
            "../{}/pwned",
            outside.file_name().unwrap().to_str().unwrap()
        );
        let c = Container::new(
            evil_id.clone(),
            "x".into(),
            "img".into(),
            vec![],
            "256M".into(),
        );
        store.save(&c).unwrap();

        // o ficheiro TEM de ficar dentro de `root` — nunca em `outside`.
        assert!(
            !outside.exists(),
            "save com id malicioso escreveu FORA da raiz do Store"
        );
        let entries: Vec<_> = fs::read_dir(&root).unwrap().flatten().collect();
        assert_eq!(
            entries.len(),
            1,
            "devia existir exactamente 1 ficheiro dentro da raiz sanitizada"
        );
        assert!(
            entries[0]
                .path()
                .to_string_lossy()
                .starts_with(root.to_string_lossy().as_ref()),
            "ficheiro escrito fora da raiz esperada"
        );

        // load/remove com o MESMO id malicioso continuam a resolver para dentro
        // da raiz (consistência: save/load/remove sanitizam da mesma forma).
        let loaded = store.load(&evil_id).unwrap();
        assert_eq!(
            loaded.id, evil_id,
            "o conteúdo persistido continua correcto (só o PATH em disco é sanitizado)"
        );
        store.remove(&evil_id).unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().flatten().count(), 0);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn jsonstore_path_traversal_tambem_neutralizado() {
        let root = tmp_dir("jsonstore-path");
        let store: JsonStore<String> = JsonStore::open(&root).unwrap();
        let evil_key = "../../../tmp/pwned-jsonstore";
        store.save(evil_key, &"conteudo".to_string()).unwrap();

        let entries: Vec<_> = fs::read_dir(&root).unwrap().flatten().collect();
        assert_eq!(
            entries.len(),
            1,
            "JsonStore também tem de manter tudo dentro da raiz"
        );
        assert!(store.load(evil_key).is_ok());

        let _ = fs::remove_dir_all(&root);
    }

    /// REGRESSÃO (concorrência): `update` sequencia read-modify-write entre
    /// threads. Sem o `flock`, N incrementos concorrentes perdem-se (lost
    /// update) e o total final vem < N. Com o trinco, tem de ser exactamente N.
    #[test]
    fn update_concorrente_nao_perde_escritas() {
        let root = tmp_dir("store-update-race");
        let store = Store::open(&root).unwrap();
        let mut c = Container::new(
            "race1".into(),
            "race1".into(),
            "img".into(),
            vec!["x".into()],
            "max".into(),
        );
        c.labels.insert("n".into(), "0".into());
        store.save(&c).unwrap();

        const N: usize = 24;
        std::thread::scope(|sc| {
            for _ in 0..N {
                let root = root.clone();
                sc.spawn(move || {
                    let st = Store::open(&root).unwrap();
                    st.update("race1", |c| {
                        let n: u64 = c.labels.get("n").unwrap().parse().unwrap();
                        // Janela de corrida explícita entre o read e o write:
                        // sem trinco, garante o lost update.
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        c.labels.insert("n".into(), (n + 1).to_string());
                        true
                    })
                    .unwrap();
                });
            }
        });

        let got: usize = store
            .load("race1")
            .unwrap()
            .labels
            .get("n")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(got, N, "perderam-se escritas: {got} de {N}");
        let _ = fs::remove_dir_all(&root);
    }

    /// REGRESSÃO: o temporário do `save` tem de ser único por escritor. Com um
    /// nome fixo (`.<id>.tmp`), escritas concorrentes do MESMO container
    /// entrelaçavam-se no temp e o `rename` publicava JSON corrompido.
    #[test]
    fn save_concorrente_nunca_publica_json_corrompido() {
        let root = tmp_dir("store-save-race");
        let store = Store::open(&root).unwrap();
        let base = Container::new(
            "race2".into(),
            "race2".into(),
            "img".into(),
            vec!["x".into()],
            "max".into(),
        );
        store.save(&base).unwrap();

        std::thread::scope(|sc| {
            for i in 0..16 {
                let root = root.clone();
                sc.spawn(move || {
                    let st = Store::open(&root).unwrap();
                    let mut c = Container::new(
                        "race2".into(),
                        format!("nome-{}", "a".repeat(i * 7)), // tamanhos diferentes = entrelaçado visível
                        "img".into(),
                        vec!["x".into()],
                        "max".into(),
                    );
                    c.labels.insert("k".into(), "v".repeat(i * 11));
                    st.save(&c).unwrap();
                    // Cada leitura tem de ver SEMPRE um JSON válido.
                    st.load("race2")
                        .expect("JSON corrompido publicado pelo rename");
                });
            }
        });

        store
            .load("race2")
            .expect("estado final tem de ser um JSON válido");
        let _ = fs::remove_dir_all(&root);
    }
}
