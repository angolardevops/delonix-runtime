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
        let mut t = super::output::Table::new(&["KIND", "NAME", "PRESENT", "STATUS"]);
        for doc in of {
            let name = &doc.metadata.name;
            let (present, status) = presence(kind, name, &containers);
            t.row(vec![kind.to_string(), name.clone(), present, status]);
        }
        t.print();
    }
    Ok(())
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
    super::network::apply(&docs)?;
    super::volume::apply(&docs)?;
    super::storage::apply(&docs)?;
    super::image::apply(&docs)?;
    super::vm::apply(&docs)?;
    super::container::apply(&docs)?;
    super::firewall::apply(&docs)?;
    Ok(())
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
