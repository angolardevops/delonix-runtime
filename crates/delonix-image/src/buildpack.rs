//! Cloud Native Buildpacks / Paketo — Pilar 1, **Bloco B** (lógica pura + scaffolding).
//!
//! Constrói o **plano** de um build CNB: que imagem-*builder* correr, que *run-image*,
//! que argumentos do `lifecycle/creator` (CNB *platform spec*, fase única detect+build+
//! export) e que *mounts*. Tudo isto é **lógica pura, unit-testada**.
//!
//! A **execução** (correr o builder como container Delonix rootless e exportar a imagem)
//! é conduzida pela CLI e exige **ambiente real** (builder Paketo presente + o registo
//! interno do Bloco E para o *exporter*) — está em scaffolding, **E2E por validar**.

use std::path::{Path, PathBuf};

/// As imagens (builder, run) de uma família de buildpacks.
///
/// `auto`/`paketo` (omissão) → `builder-jammy-base`, que cobre Node/Python/Go/Java/Ruby/
/// .NET/PHP/web num só builder. `heroku` → a família Heroku.
pub fn builder_images(family: &str) -> (&'static str, &'static str) {
    match family {
        "heroku" => ("heroku/builder:24", "heroku/heroku:24"),
        _ => (
            "paketobuildpacks/builder-jammy-base",
            "paketobuildpacks/run-jammy-base",
        ),
    }
}

/// O plano de um build CNB — tudo o que é preciso para correr o lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CnbPlan {
    /// Imagem que se corre (contém o lifecycle + os buildpacks).
    pub builder_image: String,
    /// Imagem-base do runtime, passada ao `creator`.
    pub run_image: String,
    /// Referência OCI de saída (ex.: `<name>:latest` ou `<registo>/<name>:latest`).
    pub output_ref: String,
    /// Pasta do código-fonte (montada em `/workspace`).
    pub source: PathBuf,
    /// Volume nomeado para o cache de layers entre builds (montado em `/cache`).
    pub cache_volume: String,
}

impl CnbPlan {
    /// Constrói o plano para uma app a partir da família de builder e da ref de saída.
    pub fn new(name: &str, source: &Path, family: &str, output_ref: &str) -> CnbPlan {
        let (builder, run) = builder_images(family);
        CnbPlan {
            builder_image: builder.to_string(),
            run_image: run.to_string(),
            output_ref: output_ref.to_string(),
            source: source.to_path_buf(),
            cache_volume: format!("cnb-cache-{name}"),
        }
    }

    /// Argumentos do `lifecycle/creator` (fase única: detect → build → export) conforme a
    /// *CNB platform spec*. O `creator` é o entrypoint do container builder.
    pub fn creator_args(&self) -> Vec<String> {
        vec![
            "/cnb/lifecycle/creator".into(),
            "-app=/workspace".into(),
            "-layers=/layers".into(),
            "-cache-dir=/cache".into(),
            format!("-run-image={}", self.run_image),
            self.output_ref.clone(),
        ]
    }

    /// Mounts do container builder: fonte→`/workspace`, volume de cache→`/cache`.
    /// (O cache reusa a CAS/overlay do engine via um volume nomeado — Bloco F.)
    pub fn mounts(&self) -> Vec<(String, String)> {
        vec![
            (self.source.display().to_string(), "/workspace".to_string()),
            (self.cache_volume.clone(), "/cache".to_string()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn selects_builder_family() {
        assert_eq!(
            builder_images("auto"),
            ("paketobuildpacks/builder-jammy-base", "paketobuildpacks/run-jammy-base")
        );
        assert_eq!(builder_images("paketo").0, "paketobuildpacks/builder-jammy-base");
        assert_eq!(builder_images("heroku"), ("heroku/builder:24", "heroku/heroku:24"));
    }

    #[test]
    fn plan_has_correct_creator_args_and_mounts() {
        let plan = CnbPlan::new("shop", Path::new("/src/shop"), "auto", "shop:latest");
        assert_eq!(plan.builder_image, "paketobuildpacks/builder-jammy-base");
        assert_eq!(plan.run_image, "paketobuildpacks/run-jammy-base");
        assert_eq!(plan.cache_volume, "cnb-cache-shop");

        let args = plan.creator_args();
        assert_eq!(args[0], "/cnb/lifecycle/creator");
        assert!(args.contains(&"-app=/workspace".to_string()));
        assert!(args.contains(&"-run-image=paketobuildpacks/run-jammy-base".to_string()));
        assert_eq!(args.last().unwrap(), "shop:latest"); // output ref é o posicional final

        let mounts = plan.mounts();
        assert_eq!(mounts[0], ("/src/shop".to_string(), "/workspace".to_string()));
        assert_eq!(mounts[1], ("cnb-cache-shop".to_string(), "/cache".to_string()));
    }
}
