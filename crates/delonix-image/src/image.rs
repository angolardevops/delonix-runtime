//! O modelo de uma imagem e o seu armazém local.

use crate::cas::{strip, Cas};
use delonix_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// O subconjunto do config OCI que o Delonix usa (Cmd/Env) + extensões Delonix
/// (limites de recursos embebidos na imagem — algo que o Docker não tem).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ImageConfig {
    /// O comando por omissão (Docker: `Cmd`).
    #[serde(default)]
    pub cmd: Vec<String>,
    /// O executável de entrada (Docker: `Entrypoint`). Muitas imagens definem só
    /// isto (sem `Cmd`); o comando final é `entrypoint + cmd` (ou `entrypoint +
    /// args do utilizador`).
    #[serde(default)]
    pub entrypoint: Vec<String>,
    /// As variáveis de ambiente por omissão (Docker: `Env`).
    #[serde(default)]
    pub env: Vec<String>,
    /// Limite de CPU embebido (`CPUS` no Dockerfile Delonix), ex.: "0.5".
    #[serde(default)]
    pub cpus: Option<String>,
    /// Limite de memória embebido (`MEMORY`), ex.: "96M".
    #[serde(default)]
    pub memory: Option<String>,
    /// Postura de segurança embebida (`SECURITY`): ex. `["userns","apparmor"]`.
    #[serde(default)]
    pub security: Vec<String>,
    /// Comando de `HEALTHCHECK` do Dockerfile (a parte após `CMD`), se houver.
    #[serde(default)]
    pub healthcheck: Option<String>,
    /// O utilizador por omissão (Docker/OCI: `User`), ex.: `"elasticsearch"`,
    /// `"1000"` ou `"1000:1000"`. Vazio = root (uid 0). Imagens como o
    /// Elasticsearch recusam correr como root, por isso o runtime troca para este
    /// uid/gid antes do `exec` (em rootless, via mapa de subuid `newuidmap`).
    #[serde(default)]
    pub user: String,
    /// Diretório de trabalho por omissão (Docker/OCI: `WorkingDir`), ex.: `"/data"`,
    /// `"/app"`. Vazio = `/`. O runtime faz `chdir` para aqui antes do `exec` — sem
    /// isto, entrypoints que operam no CWD (ex.: o `chown -R` do redis/postgres) correm
    /// a partir de `/` e tocam `/sys` (RO). [[delonix-rootless-user]]
    #[serde(default)]
    pub working_dir: String,
}

/// Uma imagem registada localmente.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Image {
    /// Id da imagem = digest do blob de config (`sha256:...`).
    pub id: String,
    /// As etiquetas (`name:tag`) que apontam para esta imagem.
    pub repo_tags: Vec<String>,
    /// Digests dos *layers*, da base para o topo.
    pub layers: Vec<String>,
    /// O config resolvido (Cmd/Env).
    pub config: ImageConfig,
    /// Instante de importação/construção (segundos Unix).
    pub created_unix: u64,
}

impl Image {
    /// Os primeiros 12 caracteres do id (hex).
    pub fn short_id(&self) -> String {
        strip(&self.id).chars().take(12).collect()
    }

    /// A pseudo-imagem `scratch` (Docker): uma base VAZIA, sem layers nem config.
    /// NÃO se resolve no store nem se puxa de um registry — é o ponto de partida
    /// vazio para `FROM scratch`. `export_rootfs` sobre ela produz um rootfs vazio.
    pub fn scratch() -> Self {
        Image {
            id: "sha256:scratch".into(), // sentinela; não há blob de config real
            repo_tags: vec!["scratch:latest".into()],
            layers: Vec::new(),
            config: ImageConfig::default(),
            created_unix: 0,
        }
    }
}

/// O armazém de imagens: registos JSON + blobs no CAS.
pub struct ImageStore {
    root: PathBuf,
    cas: Cas,
}

impl ImageStore {
    /// Abre (criando) o armazém. `$DELONIX_ROOT` ou `/var/lib/delonix`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("images"))?;
        fs::create_dir_all(root.join("layers"))?;
        fs::create_dir_all(root.join("containers"))?;
        let cas = Cas::open(&root)?;
        Ok(Self { root, cas })
    }

    /// O directório por omissão do armazém de imagens.
    pub fn default_root() -> PathBuf {
        if let Some(root) = std::env::var_os("DELONIX_ROOT") {
            return PathBuf::from(root);
        }
        // Rootless (A13): sem privilégios, o armazém de root (`/var/lib/delonix`)
        // não é escrevível → usa o do utilizador (`$XDG_DATA_HOME/delonix`).
        if !nix::unistd::geteuid().is_root() {
            let base = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
                .unwrap_or_else(|| PathBuf::from("."));
            return base.join("delonix");
        }
        PathBuf::from("/var/lib/delonix")
    }

    /// Acesso ao CAS subjacente.
    pub fn cas(&self) -> &Cas {
        &self.cas
    }

    /// A raiz do armazém.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.root.join("images").join(format!("{}.json", strip(id)))
    }

    /// Etiquetas a guardar para uma imagem com este `id`: a nova primeiro, e
    /// depois as que JÁ existam para o mesmo `id` (conteúdo idêntico ⇒ mesmo id,
    /// pode ter várias tags — como o Docker). Evita perder a tag anterior quando
    /// dois builds produzem o mesmo config (ex.: cache hit no mesmo segundo).
    pub(crate) fn merged_tags(&self, id: &str, new_tag: &str) -> Vec<String> {
        let mut tags = vec![normalise_tag(new_tag)];
        if let Ok(data) = fs::read(self.record_path(id)) {
            if let Ok(existing) = serde_json::from_slice::<Image>(&data) {
                for t in existing.repo_tags {
                    if !tags.contains(&t) {
                        tags.push(t);
                    }
                }
            }
        }
        tags
    }

    /// Persiste uma imagem (escrita atómica).
    pub fn save(&self, img: &Image) -> Result<()> {
        let p = self.record_path(&img.id);
        let tmp = p.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(img)?)?;
        fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// Lista todas as imagens, da mais recente para a mais antiga.
    pub fn list(&self) -> Result<Vec<Image>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("images"))? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(bytes) = fs::read(&path) {
                    if let Ok(img) = serde_json::from_slice::<Image>(&bytes) {
                        out.push(img);
                    }
                }
            }
        }
        out.sort_by_key(|i| std::cmp::Reverse(i.created_unix));
        Ok(out)
    }

    /// Resolve `name:tag`, `name` (→`:latest`) ou prefixo de id.
    pub fn resolve(&self, name: &str) -> Result<Image> {
        let want = normalise_tag(name);
        for img in self.list()? {
            if img.repo_tags.contains(&want) || strip(&img.id).starts_with(strip(name)) {
                return Ok(img);
            }
        }
        Err(Error::NotFound(format!("image {name}")))
    }

    /// Garante que cada etiqueta de `img` aponta SÓ para `img`: retira-a de
    /// qualquer OUTRO registo que ainda a tenha (e apaga o registo se ficar sem
    /// etiquetas). É o que faz uma re-etiquetagem MOVER a tag (como o Docker), em
    /// vez de a deixar a apontar para duas imagens.
    pub(crate) fn enforce_tag_uniqueness(&self, img: &Image) -> Result<()> {
        let tags: std::collections::HashSet<&String> = img.repo_tags.iter().collect();
        for other in self.list()? {
            if other.id == img.id {
                continue;
            }
            let kept: Vec<String> =
                other.repo_tags.iter().filter(|t| !tags.contains(t)).cloned().collect();
            if kept.len() == other.repo_tags.len() {
                continue; // nada para remover
            }
            if kept.is_empty() {
                let _ = fs::remove_file(self.record_path(&other.id));
            } else {
                let mut moved = other.clone();
                moved.repo_tags = kept;
                self.save(&moved)?;
            }
        }
        Ok(())
    }

    /// Acrescenta uma nova etiqueta a uma imagem existente (movendo-a de outra
    /// imagem se já lá estiver — a tag fica única).
    pub fn tag(&self, source: &str, new_tag: &str) -> Result<()> {
        let mut img = self.resolve(source)?;
        let tag = normalise_tag(new_tag);
        if !img.repo_tags.contains(&tag) {
            img.repo_tags.push(tag);
        }
        self.enforce_tag_uniqueness(&img)?;
        self.save(&img)?;
        Ok(())
    }

    /// Remove uma etiqueta; se for a última, apaga o registo da imagem.
    pub fn remove(&self, name: &str) -> Result<String> {
        let mut img = self.resolve(name)?;
        let want = normalise_tag(name);
        let id = img.short_id();
        if img.repo_tags.len() > 1 && img.repo_tags.contains(&want) {
            img.repo_tags.retain(|t| *t != want);
            self.save(&img)?;
            Ok(format!("untagged: {want}"))
        } else {
            fs::remove_file(self.record_path(&img.id))?;
            Ok(format!("deleted: {id}"))
        }
    }
}

/// Normaliza um nome de imagem: `alpine` → `alpine:latest`.
pub fn normalise_tag(name: &str) -> String {
    if name.contains(':') || name.contains('@') {
        name.to_string()
    } else {
        format!("{name}:latest")
    }
}

/// O instante actual em segundos Unix.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
