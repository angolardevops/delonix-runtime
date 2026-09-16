//! Cloud Native Buildpacks / Paketo — Pillar 1, **Block B** (pure logic + scaffolding).
//!
//! Builds the **plan** of a CNB build: which *builder* image to run, which *run-image*,
//! which `lifecycle/creator` arguments (CNB *platform spec*, single detect+build+
//! export phase) and which *mounts*. All of this is **pure, unit-tested logic**.
//!
//! The **execution** (running the builder as a rootless Delonix container and exporting the image)
//! is driven by the CLI and requires a **real environment** (Paketo builder present + the
//! internal registry from Block E for the *exporter*) — it is in scaffolding, **E2E yet to be validated**.

use std::path::{Path, PathBuf};

/// The (builder, run) images of a buildpack family.
///
/// `auto`/`paketo` (default) → `builder-jammy-base`, which covers Node/Python/Go/Java/Ruby/
/// .NET/PHP/web in a single builder. `heroku` → the Heroku family.
pub fn builder_images(family: &str) -> (&'static str, &'static str) {
    match family {
        "heroku" => ("heroku/builder:24", "heroku/heroku:24"),
        _ => (
            "paketobuildpacks/builder-jammy-base",
            "paketobuildpacks/run-jammy-base",
        ),
    }
}

/// The plan of a CNB build — everything needed to run the lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CnbPlan {
    /// Image that is run (contains the lifecycle + the buildpacks).
    pub builder_image: String,
    /// Runtime base image, passed to the `creator`.
    pub run_image: String,
    /// Output OCI reference (e.g. `<name>:latest` or `<registry>/<name>:latest`).
    pub output_ref: String,
    /// Source code folder (mounted at `/workspace`).
    pub source: PathBuf,
    /// Named volume for the layer cache across builds (mounted at `/cache`).
    pub cache_volume: String,
}

impl CnbPlan {
    /// Builds the plan for an app from the builder family and the output ref.
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

    /// `lifecycle/creator` arguments (single phase: detect → build → export) per the
    /// *CNB platform spec*. The `creator` is the entrypoint of the builder container.
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

    /// Mounts of the builder container: source→`/workspace`, cache volume→`/cache`.
    /// (The cache reuses the engine's CAS/overlay via a named volume — Block F.)
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
            (
                "paketobuildpacks/builder-jammy-base",
                "paketobuildpacks/run-jammy-base"
            )
        );
        assert_eq!(
            builder_images("paketo").0,
            "paketobuildpacks/builder-jammy-base"
        );
        assert_eq!(
            builder_images("heroku"),
            ("heroku/builder:24", "heroku/heroku:24")
        );
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
        assert_eq!(args.last().unwrap(), "shop:latest"); // output ref is the final positional

        let mounts = plan.mounts();
        assert_eq!(
            mounts[0],
            ("/src/shop".to_string(), "/workspace".to_string())
        );
        assert_eq!(
            mounts[1],
            ("cnb-cache-shop".to_string(), "/cache".to_string())
        );
    }
}
