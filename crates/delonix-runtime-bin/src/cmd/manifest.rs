//! `delonix-manifest.yaml` — manifesto declarativo multi-documento, ao estilo
//! Kubernetes (`apiVersion`/`kind`/`metadata`/`spec`), para os 5 Kinds já
//! cobertos por grupo de CLI: `Container`/`Image`/`Vm`/`Volume`/`Network`.
//!
//! **Semântica de `apply`: "garante presente", não um reconciliador.** Sem
//! diffing/rollout/drift-detection contínua — isso é trabalho de um
//! orchestrator com controllers (fora de escopo aqui, deliberadamente). Cada
//! `apply` de um recurso verifica se já existe por nome; se sim, salta; se
//! não, cria com a mesma lógica do comando `create`/`run`/`pull` equivalente.
//! Ver `cmd::stack` para a composição de todos os Kinds (`stack apply`).

use std::path::{Path, PathBuf};

use delonix_runtime_core::{Error, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    pub name: String,
}

/// Um documento do manifesto — `spec` fica cru (`serde_yaml::Value`) até o
/// grupo do Kind certo o re-desserializar para o seu tipo tipado (`ContainerSpec`,
/// `VmSpec`, ...). Evita este módulo ter de conhecer os 5 tipos de spec.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestDoc {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    #[serde(default)]
    pub spec: serde_yaml::Value,
}

/// `-f <ficheiro>` explícito, ou `./delonix-manifest.yaml` por omissão.
pub fn resolve_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let default = PathBuf::from("delonix-manifest.yaml");
    if default.exists() {
        Ok(default)
    } else {
        Err(Error::Invalid(
            "sem manifesto: passa -f <ficheiro> ou cria um ./delonix-manifest.yaml".into(),
        ))
    }
}

/// A única `apiVersion` reconhecida hoje — recusa cedo (em vez de avançar
/// silenciosamente) se o manifesto vier de uma versão futura/incompatível.
const SUPPORTED_API_VERSION: &str = "delonix.io/v1";

/// Carrega TODOS os documentos (`---`-separados) de um manifesto.
pub fn load(path: &Path) -> Result<Vec<ManifestDoc>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Invalid(format!("não consegui ler {}: {e}", path.display())))?;
    if text.trim().is_empty() {
        return Err(Error::Invalid(format!("{} está vazio (sem documentos YAML)", path.display())));
    }
    let mut docs = Vec::new();
    for de in serde_yaml::Deserializer::from_str(&text) {
        let doc = ManifestDoc::deserialize(de)
            .map_err(|e| Error::Invalid(format!("manifesto inválido em {}: {e}", path.display())))?;
        if doc.api_version != SUPPORTED_API_VERSION {
            return Err(Error::Invalid(format!(
                "{} '{}': apiVersion '{}' desconhecida (só '{SUPPORTED_API_VERSION}' é suportada)",
                doc.kind, doc.metadata.name, doc.api_version
            )));
        }
        docs.push(doc);
    }
    if docs.is_empty() {
        return Err(Error::Invalid(format!("{} está vazio (sem documentos YAML)", path.display())));
    }
    Ok(docs)
}

/// Filtra os documentos de um `kind` específico (comparação exacta, ex. `"Container"`).
pub fn of_kind<'a>(docs: &'a [ManifestDoc], kind: &str) -> Vec<&'a ManifestDoc> {
    docs.iter().filter(|d| d.kind == kind).collect()
}

/// Re-desserializa o `spec` cru de um documento para o tipo tipado do seu Kind.
pub fn spec_of<T: for<'de> Deserialize<'de>>(doc: &ManifestDoc) -> Result<T> {
    serde_yaml::from_value(doc.spec.clone())
        .map_err(|e| Error::Invalid(format!("{} '{}': spec inválido: {e}", doc.kind, doc.metadata.name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_multi_doc_com_kinds_diferentes() {
        let text = "\
apiVersion: delonix.io/v1
kind: Network
metadata: { name: appnet }
spec: { driver: bridge }
---
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: pgdata }
spec: { driver: local }
---
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: \"alpine:3.19\" }
";
        let p = std::env::temp_dir().join(format!("delonix-manifest-test-{}.yaml", std::process::id()));
        std::fs::write(&p, text).unwrap();
        let docs = load(&p).unwrap();
        assert_eq!(docs.len(), 3);
        assert_eq!(docs[0].kind, "Network");
        assert_eq!(docs[0].metadata.name, "appnet");
        assert_eq!(docs[2].kind, "Container");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn of_kind_filtra_correctamente() {
        let text = "\
apiVersion: delonix.io/v1
kind: Network
metadata: { name: a }
spec: {}
---
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: b }
spec: {}
";
        let p = std::env::temp_dir().join(format!("delonix-manifest-test2-{}.yaml", std::process::id()));
        std::fs::write(&p, text).unwrap();
        let docs = load(&p).unwrap();
        assert_eq!(of_kind(&docs, "Network").len(), 1);
        assert_eq!(of_kind(&docs, "Volume").len(), 1);
        assert_eq!(of_kind(&docs, "Vm").len(), 0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn ficheiro_vazio_e_erro_claro() {
        let p = std::env::temp_dir().join(format!("delonix-manifest-empty-{}.yaml", std::process::id()));
        std::fs::write(&p, "").unwrap();
        let err = load(&p).unwrap_err();
        assert!(format!("{err}").contains("vazio"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn resolve_path_sem_flag_nem_ficheiro_e_erro_claro() {
        let dir = std::env::temp_dir().join(format!("delonix-manifest-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let err = resolve_path(None).unwrap_err();
        assert!(format!("{err}").contains("sem manifesto"));
        std::env::set_current_dir(orig).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
