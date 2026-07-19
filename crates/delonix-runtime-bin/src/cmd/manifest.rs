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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use delonix_runtime_core::{Error, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    pub name: String,
    /// Rótulos livres para agrupar/seleccionar recursos (estilo k8s). Opcional —
    /// o runtime é single-tenant, não há namespaces; isto é só organização.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Anotações livres (notas, prereqs, referências) — nunca interpretadas pelo
    /// runtime, só transportadas para o `describe`.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
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

/// Normaliza o `kind` para a sua forma canónica, aceitando sinónimos comuns —
/// o match de Kind é por string exacta (`of_kind`), por isso um `VirtualMachine`
/// ou `VM` num manifesto tem de resolver para o mesmo `Vm` que o resto do código
/// usa. Devolve a forma canónica se conhecida, senão o `kind` tal e qual (Kinds
/// desconhecidos são tratados a jusante, ver `cmd::stack::describe`).
pub fn canonical_kind(kind: &str) -> &str {
    // Case-insensitive de propósito: `Vm`/`VM`/`vm`/`VirtualMachine`/`virtualMachine`
    // (qualquer casing) resolvem todos para o `Vm` canónico — meia-medida
    // (só alguns casings) seria pior do que nada, deixando um `kind: vm` a ser
    // ignorado em silêncio pelo `stack apply`.
    let lower = kind.to_ascii_lowercase();
    match lower.as_str() {
        "vm" | "virtualmachine" => "Vm",
        // `KnowDepends` é o nome que o utilizador pediu; `Dependency` é o canónico.
        "knowdepends" | "dependency" => "Dependency",
        _ => kind,
    }
}

/// Carrega TODOS os documentos (`---`-separados) de um manifesto.
pub fn load(path: &Path) -> Result<Vec<ManifestDoc>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Invalid(format!("não consegui ler {}: {e}", path.display())))?;
    if text.trim().is_empty() {
        return Err(Error::Invalid(format!("{} está vazio (sem documentos YAML)", path.display())));
    }
    let mut docs = Vec::new();
    for de in serde_yaml::Deserializer::from_str(&text) {
        let mut doc = ManifestDoc::deserialize(de)
            .map_err(|e| Error::Invalid(format!("manifesto inválido em {}: {e}", path.display())))?;
        // Canonicaliza cedo: todo o resto (of_kind, stack::KINDS, describe) fala
        // só a forma canónica, e um `kind: VirtualMachine` passa a ser um `Vm`.
        let canon = canonical_kind(&doc.kind);
        if canon != doc.kind {
            doc.kind = canon.to_string();
        }
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

/// Avisa (stderr, NÃO erro) por cada chave de topo do `spec` que não conste em
/// `known`. Os specs não têm `deny_unknown_fields` de propósito — um manifesto
/// `delonix.io/v1` escrito para um binário mais recente pode trazer campos que
/// este ainda não conhece, e nesse caso queremos ignorá-los e seguir, não
/// abortar. Mas o caso comum de um campo desconhecido é uma GRALHA (`memroy:`),
/// e um IaaS nunca deve aplicar um default em silêncio quando o utilizador
/// claramente quis outra coisa. Daí o aviso claro e accionável.
///
/// `known` deve conter TODOS os nomes aceites (o canónico e cada `alias`) — há
/// um teste por Kind que garante que os `examples/` não disparam nenhum aviso,
/// travando o drift entre esta lista e o struct.
pub fn warn_unknown_fields(doc: &ManifestDoc, known: &[&str]) {
    for key in unknown_fields(doc, known) {
        eprintln!(
            "AVISO: {} '{}': campo desconhecido '{}' no spec — ignorado (verifica a ortografia)",
            doc.kind, doc.metadata.name, key
        );
    }
}

/// Núcleo puro de `warn_unknown_fields`: devolve as chaves de topo do `spec` que
/// não constam em `known`. Separado para os testes de drift (`examples/` nunca
/// deve produzir chaves desconhecidas) poderem afirmar sobre o resultado.
pub fn unknown_fields(doc: &ManifestDoc, known: &[&str]) -> Vec<String> {
    let serde_yaml::Value::Mapping(map) = &doc.spec else { return Vec::new() };
    map.keys()
        .filter_map(|k| k.as_str())
        .filter(|key| !known.contains(key))
        .map(str::to_string)
        .collect()
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
    fn kind_virtualmachine_canonicaliza_para_vm() {
        let text = "\
apiVersion: delonix.io/v1
kind: VirtualMachine
metadata: { name: node1 }
spec: { disk: k8s-golden }
---
apiVersion: delonix.io/v1
kind: VM
metadata: { name: node2 }
spec: { disk: k8s-golden }
";
        let p = std::env::temp_dir().join(format!("delonix-manifest-vm-alias-{}.yaml", std::process::id()));
        std::fs::write(&p, text).unwrap();
        let docs = load(&p).unwrap();
        // Ambos os sinónimos passam a ser o `Vm` canónico, apanhados por `of_kind`.
        assert_eq!(of_kind(&docs, "Vm").len(), 2);
        assert_eq!(docs[0].kind, "Vm");
        assert_eq!(docs[1].kind, "Vm");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn canonical_kind_e_case_insensitive_para_vm() {
        // Qualquer casing plausível de outra ferramenta resolve para `Vm`.
        for k in ["Vm", "VM", "vm", "VirtualMachine", "virtualMachine", "VIRTUALMACHINE"] {
            assert_eq!(canonical_kind(k), "Vm", "kind {k:?} devia canonicalizar para Vm");
        }
        // Kinds não-Vm passam intactos (não inventamos sinónimos).
        assert_eq!(canonical_kind("Container"), "Container");
        assert_eq!(canonical_kind("Storage"), "Storage");
    }

    #[test]
    fn metadata_labels_annotations_opcionais() {
        let text = "\
apiVersion: delonix.io/v1
kind: Container
metadata:
  name: web
  labels: { tier: frontend }
  annotations: { note: exemplo }
spec: { image: alpine }
---
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: sem-labels }
spec: {}
";
        let p = std::env::temp_dir().join(format!("delonix-manifest-meta-{}.yaml", std::process::id()));
        std::fs::write(&p, text).unwrap();
        let docs = load(&p).unwrap();
        assert_eq!(docs[0].metadata.labels.get("tier").map(String::as_str), Some("frontend"));
        assert_eq!(docs[0].metadata.annotations.get("note").map(String::as_str), Some("exemplo"));
        // Sem bloco labels/annotations → mapas vazios, nunca erro.
        assert!(docs[1].metadata.labels.is_empty());
        assert!(docs[1].metadata.annotations.is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unknown_fields_apanha_gralha_e_ignora_conhecidos() {
        let text = "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: alpine, memroy: 2G, restartPolicy: always }
";
        let p = std::env::temp_dir().join(format!("delonix-manifest-unknown-{}.yaml", std::process::id()));
        std::fs::write(&p, text).unwrap();
        let docs = load(&p).unwrap();
        let unknown = unknown_fields(&docs[0], crate::cmd::container::CONTAINER_SPEC_FIELDS);
        // `memroy` (gralha) é sinalizado; `image`/`restartPolicy` (canónico) não.
        assert_eq!(unknown, vec!["memroy".to_string()]);
        let _ = std::fs::remove_file(&p);
    }

    /// Drift-guard: cada ficheiro em `examples/` tem de parsear sem UM campo
    /// desconhecido. Se alguém acrescenta um campo ao exemplo mas esquece a
    /// const `*_SPEC_FIELDS` (ou vice-versa), este teste parte — é o que mantém
    /// as listas de campos conhecidos alinhadas com o schema real e com a doc.
    #[test]
    fn examples_nao_tem_campos_desconhecidos() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let fields_for = |kind: &str| -> Option<&'static [&'static str]> {
            match kind {
                "Container" => Some(crate::cmd::container::CONTAINER_SPEC_FIELDS),
                "Vm" => Some(crate::cmd::vm::VM_SPEC_FIELDS),
                "Volume" => Some(crate::cmd::volume::VOLUME_SPEC_FIELDS),
                "Storage" => Some(crate::cmd::storage::STORAGE_SPEC_FIELDS),
                "Network" => Some(crate::cmd::network::NETWORK_SPEC_FIELDS),
                "Image" => Some(crate::cmd::image::IMAGE_SPEC_FIELDS),
                "Secret" => Some(crate::cmd::secret::SECRET_SPEC_FIELDS),
                "Ingress" | "Egress" | "FirewallPolicy" => Some(crate::cmd::firewall::FW_SPEC_FIELDS),
                "HTTPRoute" => Some(crate::cmd::httproute::HTTP_ROUTE_SPEC_FIELDS),
                "Dependency" => Some(crate::cmd::dependency::DEPENDENCY_SPEC_FIELDS),
                _ => None, // Cluster tem specs aninhados próprios; fora deste guard.
            }
        };
        for entry in std::fs::read_dir(&dir).expect("examples/ existe") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Distinguir "não é um manifesto delonix" (cloud-config, sem
            // apiVersion — saltar) de "é um manifesto e está PARTIDO" (tem o
            // marcador mas o load falha — o teste TEM de falhar, senão um
            // example malformado passa despercebido). Sem esta distinção, o
            // guard ficava vacuamente verde para um example partido.
            if !text.contains(SUPPORTED_API_VERSION) {
                continue;
            }
            let docs = load(&path)
                .unwrap_or_else(|e| panic!("{}: é um manifesto delonix mas não parseia: {e}", path.display()));
            for doc in &docs {
                let Some(known) = fields_for(&doc.kind) else { continue };
                let unknown = unknown_fields(doc, known);
                assert!(
                    unknown.is_empty(),
                    "{}: {} '{}' tem campos desconhecidos {:?} — actualiza a const *_SPEC_FIELDS",
                    path.display(), doc.kind, doc.metadata.name, unknown
                );
            }
        }
    }

    #[test]
    fn manifesto_marcado_mas_partido_falha_load_nao_e_saltado() {
        // Contrato do drift-guard (Fix #1): um ficheiro que TEM o marcador
        // `delonix.io/v1` mas está partido (aqui, falta `metadata.name`) tem de
        // dar Err no load — é isso que distingue um example malformado (o guard
        // FALHA) de um cloud-config sem marcador (o guard salta).
        let text = "\
apiVersion: delonix.io/v1
kind: Container
metadata: {}
spec: { image: alpine }
";
        assert!(text.contains(SUPPORTED_API_VERSION));
        let p = std::env::temp_dir().join(format!("delonix-manifest-partido-{}.yaml", std::process::id()));
        std::fs::write(&p, text).unwrap();
        assert!(load(&p).is_err(), "manifesto marcado mas sem metadata.name devia falhar o load");
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
