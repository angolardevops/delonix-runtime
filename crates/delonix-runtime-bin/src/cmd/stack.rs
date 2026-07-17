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
}

pub fn run(action: StackCmd) -> Result<()> {
    if let StackCmd::Init { dir, name, image, force } = action {
        return cmd_init(super::scaffold::Target::Stack, dir, name, image, force);
    }
    match action {
        // Tratado no topo de `run` (faz `return`).
        StackCmd::Init { .. } => unreachable!("tratado acima"),
        StackCmd::Apply { file } => apply(file),
    }
}

fn apply(file: Option<PathBuf>) -> Result<()> {
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;
    super::network::apply(&docs)?;
    super::volume::apply(&docs)?;
    super::image::apply(&docs)?;
    super::vm::apply(&docs)?;
    super::container::apply(&docs)?;
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
