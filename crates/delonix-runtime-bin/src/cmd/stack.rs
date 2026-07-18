//! `delonix stack` — aplica TODOS os Kinds de um manifesto de uma vez
//! (`Network`/`Volume`/`Image`/`Vm`/`Container`), na ordem certa por
//! dependência de nome (redes/volumes/imagens antes de quem as referencia).
//!
//! **Fail-fast, sem transacionalidade**: pára no primeiro erro; o que já foi
//! aplicado antes do erro FICA aplicado (não há rollback) — mesma semântica
//! de "garante presente" documentada em `cmd::manifest`.

use std::path::PathBuf;

use clap::Subcommand;
use delonix_runtime_core::Result;

use super::manifest;

#[derive(Subcommand)]
pub enum StackCmd {
    /// Inicializa um projecto COMPLETO: Delonixfile + manifesto + cluster + README — ficheiros JÁ PREENCHIDOS (imagens
    /// incluídas), prontos a usar sem editar nada.
    Init {
        /// Directório do projecto (default: o actual).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Nome do projecto (default: o nome do directório).
        #[arg(long)]
        name: Option<String>,
        /// Imagem a usar. Omitir = preenche com a imagem por omissão.
        #[arg(long)]
        image: Option<String>,
        /// Substitui ficheiros já existentes.
        #[arg(long)]
        force: bool,
    },
    /// Aplica todos os Kinds do manifesto (Network → Volume → Image → Vm → Container).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Detalhe do stack ao estilo `kubectl describe`: cada recurso DECLARADO no
    /// manifesto e se está, ou não, presente na máquina.
    ///
    /// **O stack não tem estado próprio** — não há registo de "stacks", só um
    /// manifesto e os recursos que ele cria. Por isso este `describe` parte
    /// sempre do ficheiro e vai confirmar cada recurso ao store respectivo, em
    /// vez de inventar um registo novo a dessincronizar (a mesma razão pela
    /// qual o `cluster ls` deriva o estado das labels dos containers).
    ///
    /// A coluna que interessa é a de PRESENÇA: um `apply` é fail-fast e sem
    /// rollback, logo um stack meio-aplicado é um estado normal e é exactamente
    /// isto que o mostra.
    Describe {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Valida o manifesto SEM tocar em nada (dry-run): resolve as referências
    /// cruzadas (`Container.network`/`.volumes`, `Vm.network`, `Ingress/Egress.
    /// target`) contra o que o manifesto declara MAIS o que já existe nos stores.
    /// Sai com erro se alguma referência ficar por resolver — é a rede de
    /// segurança contra um `apply` que só falharia a meio (fail-fast, sem rollback).
    Validate {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

pub fn run(action: StackCmd) -> Result<()> {
    if let StackCmd::Init { dir, name, image, force } = action {
        return cmd_init(super::scaffold::Target::Stack, dir, name, image, force);
    }
    match action {
        // Tratado no topo de `run` (faz `return`).
        StackCmd::Init { .. } => unreachable!("tratado acima"),
        StackCmd::Apply { file } => apply(file),
        StackCmd::Describe { file } => describe(file),
        StackCmd::Validate { file } => validate(file),
    }
}

/// Os Kinds do stack, na MESMA ordem do `apply` — quem lê o `describe` vê a
/// ordem por que as coisas são criadas, o que é metade do diagnóstico quando um
/// apply pára a meio.
const KINDS: [&str; 8] = ["Network", "Volume", "Storage", "Image", "Vm", "Container", "Ingress", "Egress"];

fn describe(file: Option<PathBuf>) -> Result<()> {
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;

    let mut d = super::output::Describe::new();
    d.field("Manifest", path.display().to_string());
    d.field("Documents", docs.len().to_string());
    d.print();

    // Kinds que o manifesto traz mas o stack não sabe aplicar: melhor dizê-lo do
    // que ignorar em silêncio (o `apply` também os ignoraria, sem avisar).
    let desconhecidos: Vec<&str> = docs.iter().map(|doc| doc.kind.as_str()).filter(|k| !KINDS.contains(k)).collect();
    if !desconhecidos.is_empty() {
        println!();
        println!("AVISO: kinds não suportados pelo stack (ignorados pelo `apply`): {}", desconhecidos.join(", "));
    }

    let (_, cstore) = super::util::open_stores()?;
    let containers = cstore.list().unwrap_or_default();

    for kind in KINDS {
        let of = manifest::of_kind(&docs, kind);
        if of.is_empty() {
            continue;
        }
        println!();
        let mut t = super::output::Table::new(&["KIND", "NAME", "PRESENT", "STATUS", "LABELS"]);
        for doc in of {
            let name = &doc.metadata.name;
            let (present, status) = presence(kind, name, &containers);
            t.row(vec![kind.to_string(), name.clone(), present, status, fmt_labels(&doc.metadata)]);
        }
        t.print();
    }
    Ok(())
}

/// `key=value` dos labels de `metadata` (mais um `+N anno` se houver anotações),
/// ou `-` se não houver nenhum — a coluna organizacional do `describe`.
fn fmt_labels(meta: &manifest::Metadata) -> String {
    let mut parts: Vec<String> = meta.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    if !meta.annotations.is_empty() {
        parts.push(format!("+{} anno", meta.annotations.len()));
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(",")
    }
}

/// `(presente, estado)` de um recurso declarado. **Não é um reconciliador**: só
/// responde "existe algo com este nome?", nunca compara a spec declarada com a
/// real (drift-detection é trabalho de um orchestrator, deliberadamente fora do
/// escopo deste runtime — ver `cmd::manifest`).
fn presence(kind: &str, name: &str, containers: &[delonix_runtime_core::Container]) -> (String, String) {
    let root = super::util::state_root();
    match kind {
        "Container" => match containers.iter().find(|c| c.name == name) {
            Some(c) => {
                let mut c = c.clone();
                delonix_runtime::reconcile_status(&mut c);
                ("yes".into(), c.status.to_string())
            }
            None => ("no".into(), "-".into()),
        },
        // Storage é um volume de rede — vive no mesmo store que os volumes.
        "Volume" | "Storage" => match delonix_volume::VolumeStore::open(&root).and_then(|s| s.list()) {
            Ok(vs) => yes_no(vs.iter().any(|v| v.name == name)),
            Err(e) => ("?".into(), e.to_string()),
        },
        "Network" => match delonix_net::NetworkStore::open(&root).and_then(|s| s.list()) {
            Ok(ns) => yes_no(ns.iter().any(|n| n.name == name)),
            Err(e) => ("?".into(), e.to_string()),
        },
        "Image" => match delonix_image::ImageStore::open(&root) {
            Ok(s) => yes_no(s.resolve(name).is_ok()),
            Err(e) => ("?".into(), e.to_string()),
        },
        // `status` (e não o registo cru) para o estado vir reconciliado com o
        // backend — uma VM que morreu por fora aparece como Stopped, não Running.
        "Vm" => match delonix_vm::status(&root, name) {
            Ok(vm) => ("yes".into(), vm.status.to_string()),
            Err(_) => ("no".into(), "-".into()),
        },
        // Ingress/Egress não têm store próprio — são directivas de firewall
        // aplicadas a um container-alvo, não recursos com estado. O `apply`
        // aplica-as sempre (idempotente); aqui só se assinala a natureza.
        "Ingress" | "Egress" => ("-".into(), "declarative".into()),
        _ => ("?".into(), "kind não suportado".into()),
    }
}

fn yes_no(b: bool) -> (String, String) {
    if b {
        ("yes".into(), "present".into())
    } else {
        ("no".into(), "-".into())
    }
}

fn apply(file: Option<PathBuf>) -> Result<()> {
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;
    // Valida o grafo ANTES de tocar em nada: o `apply` é fail-fast sem rollback,
    // por isso uma referência quebrada (um `Ingress` a apontar para um container
    // que ninguém declara) deve parar tudo ANTES da primeira criação, não a meio
    // com metade do stack já no kernel.
    let issues = validate_graph(&docs);
    if !issues.is_empty() {
        for i in &issues {
            eprintln!("  ✗ {i}");
        }
        return Err(delonix_runtime_core::Error::Invalid(format!(
            "stack apply abortado: {} referência(s) por resolver (corrige o manifesto ou usa `stack validate`)",
            issues.len()
        )));
    }
    super::network::apply(&docs)?;
    super::volume::apply(&docs)?;
    super::storage::apply(&docs)?;
    super::image::apply(&docs)?;
    super::vm::apply(&docs)?;
    super::container::apply(&docs)?;
    super::firewall::apply(&docs)?;
    Ok(())
}

/// `stack validate` — dry-run: só corre `validate_graph` e reporta, sem aplicar.
fn validate(file: Option<PathBuf>) -> Result<()> {
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;
    let issues = validate_graph(&docs);
    if issues.is_empty() {
        println!("stack validate: OK — {} documento(s), todas as referências resolvidas", docs.len());
        Ok(())
    } else {
        for i in &issues {
            println!("  ✗ {i}");
        }
        Err(delonix_runtime_core::Error::Invalid(format!(
            "{} referência(s) por resolver",
            issues.len()
        )))
    }
}

/// Nomes de rede embutidos (não são referências a um `kind: Network`): os
/// containers têm `host`/`none`; as VMs usam `bridge` como default do ingress.
fn is_builtin_net(net: &str, is_vm: bool) -> bool {
    matches!(net, "" | "host" | "none") || (is_vm && net == "bridge")
}

/// Extrai os nomes de VOLUME nomeado de um `spec.volumes` (`["data:/x", ...]`).
/// Bind mounts (`/host:/x`) e entradas vazias não são referências a recursos.
fn volume_refs(doc: &manifest::ManifestDoc) -> Vec<String> {
    let Some(seq) = doc.spec.get("volumes").and_then(|v| v.as_sequence()) else {
        return Vec::new();
    };
    seq.iter()
        .filter_map(|v| v.as_str())
        .filter_map(|s| {
            let name = s.split(':').next().unwrap_or("");
            if name.is_empty() || name.starts_with('/') {
                None // bind mount ou lixo — não é um volume nomeado
            } else {
                Some(name.to_string())
            }
        })
        .collect()
}

/// Resolve todas as referências cruzadas do manifesto contra o que ele DECLARA
/// mais o que já EXISTE nos stores (leitura, best-effort). Devolve a lista de
/// problemas (vazia = grafo íntegro). **Não toca em nada** — é a base partilhada
/// pelo `stack validate` (dry-run) e pelo gate do `apply`.
fn validate_graph(docs: &[manifest::ManifestDoc]) -> Vec<String> {
    let root = super::util::state_root();

    // Recursos já presentes na máquina contam como resolvidos (um manifesto pode
    // referir uma rede criada num apply anterior). Best-effort: se um store não
    // abre, seguimos só com o que o manifesto declara.
    let existing_networks: Vec<String> = delonix_net::NetworkStore::open(&root)
        .and_then(|s| s.list())
        .map(|ns| ns.into_iter().map(|n| n.name).collect())
        .unwrap_or_default();
    let existing_volumes: Vec<String> = delonix_volume::VolumeStore::open(&root)
        .and_then(|s| s.list())
        .map(|vs| vs.into_iter().map(|v| v.name).collect())
        .unwrap_or_default();
    let existing_containers: Vec<String> = super::util::open_stores()
        .and_then(|(_, cstore)| cstore.list())
        .map(|cs| cs.into_iter().map(|c| c.name).collect())
        .unwrap_or_default();

    validate_graph_with(docs, &existing_networks, &existing_volumes, &existing_containers)
}

/// Núcleo PURO de `validate_graph`: recebe o que já existe na máquina como
/// listas explícitas (em vez de ler os stores), para os testes serem
/// determinísticos e não dependerem do estado real da máquina de dev.
fn validate_graph_with(
    docs: &[manifest::ManifestDoc],
    existing_networks: &[String],
    existing_volumes: &[String],
    existing_containers: &[String],
) -> Vec<String> {
    use std::collections::HashSet;

    let declared = |kinds: &[&str]| -> HashSet<String> {
        docs.iter().filter(|d| kinds.contains(&d.kind.as_str())).map(|d| d.metadata.name.clone()).collect()
    };
    let mut networks = declared(&["Network"]);
    let mut volumes = declared(&["Volume", "Storage"]);
    let mut containers = declared(&["Container"]);
    networks.extend(existing_networks.iter().cloned());
    volumes.extend(existing_volumes.iter().cloned());
    containers.extend(existing_containers.iter().cloned());

    let mut issues = Vec::new();

    // Duplicados dentro do manifesto (mesmo Kind + nome) — o `apply` criaria um e
    // saltaria o outro; melhor avisar do que aplicar um dos dois às cegas.
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for doc in docs {
        let key = (doc.kind.clone(), doc.metadata.name.clone());
        if !seen.insert(key) {
            issues.push(format!("{} '{}' declarado mais do que uma vez", doc.kind, doc.metadata.name));
        }
    }

    for doc in docs {
        let name = &doc.metadata.name;
        match doc.kind.as_str() {
            "Container" | "Vm" => {
                let is_vm = doc.kind == "Vm";
                if let Some(net) = doc.spec.get("network").and_then(|v| v.as_str()) {
                    if !is_builtin_net(net, is_vm) && !networks.contains(net) {
                        issues.push(format!("{} '{name}' → network '{net}' não é declarada nem existe", doc.kind));
                    }
                }
                for vref in volume_refs(doc) {
                    if !volumes.contains(&vref) {
                        issues.push(format!("{} '{name}' → volume '{vref}' não é declarado (Volume/Storage) nem existe", doc.kind));
                    }
                }
            }
            "Ingress" | "Egress" => {
                if let Some(target) = doc.spec.get("target").and_then(|v| v.as_str()) {
                    if !containers.contains(target) {
                        issues.push(format!("{} '{name}' → target '{target}' não é um Container declarado nem existente", doc.kind));
                    }
                }
            }
            _ => {}
        }
    }
    issues
}

/// Trata o `init` deste grupo (ver `cmd::scaffold`).
fn cmd_init(target: super::scaffold::Target, dir: PathBuf, name: Option<String>, image: Option<String>, force: bool) -> Result<()> {
    let name = name.unwrap_or_else(|| {
        // Sem `--name`, usa o nome do DIRECTÓRIO. Não se pode usar `canonicalize`:
        // o directório ainda não existe (é o `init` que o cria) e falharia sempre,
        // caindo no fallback — todos os projectos ficavam chamados "app".
        // `.`/vazio resolvem para o cwd; um caminho novo usa o seu basename.
        let p = if dir.as_os_str().is_empty() || dir == std::path::Path::new(".") {
            std::env::current_dir().ok()
        } else {
            Some(dir.clone())
        };
        p.as_deref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".to_string())
    });
    super::scaffold::init(target, &super::scaffold::InitOpts { dir, name, image, force })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parseia YAML multi-doc para `Vec<ManifestDoc>` via o mesmo `load` real
    /// (para as regras de canonicalização/apiVersion valerem nos testes).
    fn docs(yaml: &str) -> Vec<manifest::ManifestDoc> {
        // Nome ÚNICO por chamada: os testes correm em threads do MESMO processo,
        // logo `process::id()` não chega para os distinguir — sem o contador,
        // duas chamadas colidiam no path e uma apagava o ficheiro da outra.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir()
            .join(format!("delonix-stack-test-{}-{n}.yaml", std::process::id()));
        std::fs::write(&p, yaml).unwrap();
        let d = manifest::load(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        d
    }

    fn check(yaml: &str) -> Vec<String> {
        // Nada "existente" na máquina — o teste vê só o que o manifesto declara.
        validate_graph_with(&docs(yaml), &[], &[], &[])
    }

    #[test]
    fn grafo_integro_nao_tem_problemas() {
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Network
metadata: { name: appnet }
spec: { driver: bridge }
---
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: data }
spec: {}
---
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: nginx, network: appnet, volumes: [\"data:/var\", \"/host/x:/y:ro\"] }
---
apiVersion: delonix.io/v1
kind: Ingress
metadata: { name: web-in }
spec: { target: web }
",
        );
        assert!(issues.is_empty(), "esperava grafo íntegro, veio: {issues:?}");
    }

    #[test]
    fn network_por_declarar_e_sinalizada() {
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: nginx, network: fantasma }
",
        );
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("network 'fantasma'"), "{issues:?}");
    }

    #[test]
    fn builtins_de_rede_nao_sao_referencias() {
        // host/none (container) e bridge (vm) não são um kind: Network.
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: c1 }
spec: { image: nginx, network: host }
---
apiVersion: delonix.io/v1
kind: Vm
metadata: { name: v1 }
spec: { disk: d, network: bridge }
",
        );
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn volume_nomeado_por_declarar_e_sinalizado_mas_bind_mount_nao() {
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: nginx, volumes: [\"semvolume:/x\", \"/host/ok:/y\"] }
",
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("volume 'semvolume'"), "{issues:?}");
    }

    #[test]
    fn ingress_target_inexistente_e_sinalizado() {
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Egress
metadata: { name: out }
spec: { target: nao-existe }
",
        );
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("target 'nao-existe'"), "{issues:?}");
    }

    #[test]
    fn duplicado_no_manifesto_e_sinalizado() {
        let issues = check(
            "\
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: data }
spec: {}
---
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: data }
spec: {}
",
        );
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("declarado mais do que uma vez"), "{issues:?}");
    }

    #[test]
    fn recurso_ja_existente_na_maquina_resolve_a_referencia() {
        let d = docs(
            "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: nginx, network: prod-net }
",
        );
        // prod-net não está no manifesto, mas existe na máquina → resolvido.
        let issues = validate_graph_with(&d, &["prod-net".to_string()], &[], &[]);
        assert!(issues.is_empty(), "{issues:?}");
    }
}
