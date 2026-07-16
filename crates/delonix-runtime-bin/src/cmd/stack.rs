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
    /// Aplica todos os Kinds do manifesto (Network → Volume → Image → Vm → Container).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

pub fn run(action: StackCmd) -> Result<()> {
    match action {
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
