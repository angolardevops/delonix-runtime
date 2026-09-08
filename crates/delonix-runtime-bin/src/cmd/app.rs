//! `kind: App` — connects the existing Cloud Native Buildpacks scaffolding
//! (`delonix_image::{buildpack, detect, internal_registry}`) to a real build
//! path (ADR-0035).
//!
//! The one problem that scaffolding never solved: the CNB `creator` exports
//! to an OCI registry, and `internal_registry` was loopback-only — a builder
//! container, in its own netns, cannot reach the host's loopback. This module
//! puts the registry on a throwaway `kind: Network` instead, reachable by the
//! builder over its SDN IP, and ALSO publishes it to the host (a normal
//! `-p`) so the built image can be pulled back into the local `ImageStore`
//! once the build succeeds — two different reachers, one container, no new
//! dataplane. The builder reaches it by IP and not by this engine's internal
//! DNS name: found only by running a real build, the lifecycle's registry
//! client (go-containerregistry) only auto-selects plain HTTP for an RFC1918
//! address or a literal `localhost:<port>` — any other hostname gets HTTPS
//! and the build fails ("server gave HTTP response to HTTPS client"). This
//! engine's SDN subnets are always RFC1918, so the registry's own address
//! already satisfies that heuristic for free.
//!
//! The build itself runs as an `exec` into a `sleep infinity` placeholder
//! container, not as the container's own detached main process: a plain
//! detached container without a `--restart` supervisor does not reliably
//! surface its real exit code (this engine is not its real parent), while
//! `exec` does — the same reason `container exec`'s own code path always
//! reads a real code back.

use std::collections::BTreeMap;
use std::path::PathBuf;

use delonix_image::{buildpack::CnbPlan, detect, ImageStore};
use delonix_net::NetworkStore;
use delonix_runtime::{self as runtime};
use delonix_runtime_core::{Error, Result, Store};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::kinds as k;
use super::manifest::{self, ManifestDoc, Metadata};
use super::util::{find, open_stores, state_root};

pub(crate) const APP_SPEC_FIELDS: &[&str] = &["source", "builder", "runImage", "image"];
pub(crate) const RECONCILED_APP_FIELDS: &[&str] = &["ref"];

/// The CNB Platform API version this engine speaks to `/cnb/lifecycle/creator`
/// (`CNB_PLATFORM_API`) — REQUIRED, not optional: the lifecycle refuses to run
/// at all without it ("failed to get platform API version"), found only by
/// running a real build. Fixed rather than negotiated per builder: both known
/// families (`auto`/Paketo and `heroku`) embed the SAME lifecycle version
/// (0.21.18, confirmed against their published `io.buildpacks.builder.metadata`
/// labels) and declare this exact value as their own default
/// (`lifecycle.api.platform`), inside a supported range that reaches "0.15" —
/// this is the lifecycle's own preferred choice, not a guess. A custom
/// (`Explicit`) builder with a different lifecycle is this ADR's already-scoped
/// limitation: this engine does not introspect a custom builder's own metadata
/// (same reasoning as not discovering its run image).
const CNB_PLATFORM_API: &str = "0.7";

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub(crate) struct AppSpec {
    /// Directory to build (Compose-style relative-to-CWD default, same
    /// convention `kind: Image`'s `build.context` already uses — not the
    /// manifest file's own directory).
    #[serde(default = "default_source")]
    source: String,
    /// `"auto"` (detect the stack, use the Paketo builder), `"heroku"`, or an
    /// explicit builder image reference — the last one REQUIRES `runImage`
    /// (this engine does not introspect a custom builder's own `builder.toml`
    /// to discover its run image; refusing is the honest answer, not a guess).
    #[serde(default = "default_builder")]
    builder: String,
    #[serde(default, rename = "runImage")]
    run_image: Option<String>,
    /// Tag the built image gets in the LOCAL `ImageStore` once the build
    /// succeeds.
    image: String,
}

fn default_source() -> String {
    ".".to_string()
}

fn default_builder() -> String {
    "auto".to_string()
}

#[derive(Debug)]
enum ResolvedBuilder {
    /// `(builder_image, run_image)` — `builder_images()`'s two known families.
    Known(&'static str, &'static str),
    /// `(builder_image, run_image)` — explicit, `runImage` was required and given.
    Explicit(String, String),
}

impl ResolvedBuilder {
    fn images(&self) -> (String, String) {
        match self {
            ResolvedBuilder::Known(b, r) => (b.to_string(), r.to_string()),
            ResolvedBuilder::Explicit(b, r) => (b.clone(), r.clone()),
        }
    }
}

fn resolve_builder(spec: &AppSpec) -> Result<ResolvedBuilder> {
    match spec.builder.as_str() {
        "auto" => {
            let (b, r) = delonix_image::buildpack::builder_images("auto");
            Ok(ResolvedBuilder::Known(b, r))
        }
        "heroku" => {
            let (b, r) = delonix_image::buildpack::builder_images("heroku");
            Ok(ResolvedBuilder::Known(b, r))
        }
        other => match &spec.run_image {
            Some(run) => Ok(ResolvedBuilder::Explicit(other.to_string(), run.clone())),
            None => Err(Error::Invalid(format!(
                "app: unknown builder '{other}' — use 'auto', 'heroku', or set `runImage` \
                 explicitly to name a custom builder (this engine does not read a custom \
                 builder's own metadata to discover its run image)"
            ))),
        },
    }
}

pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: AppSpec = manifest::spec_of(doc)?;
    let mut f = BTreeMap::new();
    f.insert("ref".into(), spec.image.clone());
    Ok(super::reconcile::Desired {
        kind: k::APP.into(),
        name: doc.metadata.name.clone(),
        fields: f,
        // No build cache, same honesty `kind: Image`'s built case already
        // states: `apply` reruns the build and replaces the tag every time,
        // so reporting drift would make `--detailed-exitcode` return 2
        // forever in any repo that declares an App.
        converges: false,
        // The output is a shared, content-addressed image — same reasoning
        // as `kind: Image`, which is also never ownable.
        ownable: false,
    })
}

pub(crate) fn actual(docs: &[ManifestDoc]) -> Result<Vec<super::reconcile::Actual>> {
    let (images, _) = open_stores()?;
    let mut out = Vec::new();
    for doc in manifest::of_kind(docs, k::APP) {
        let spec: AppSpec = manifest::spec_of(doc)?;
        let Ok(img) = images.resolve(&spec.image) else {
            continue;
        };
        let mut f = BTreeMap::new();
        f.insert("ref".into(), spec.image);
        f.insert("digest".into(), img.id.clone());
        out.push(super::reconcile::Actual {
            kind: k::APP.into(),
            name: doc.metadata.name.clone(),
            fields: f,
            owner: None,
            last_applied: None,
        });
    }
    Ok(out)
}

/// The App's OUTPUT ref — same reasoning as `image::image_ref`: identity for
/// presence/reconciliation purposes is the produced image's tag, not the
/// document name.
pub(crate) fn image_ref(doc: &ManifestDoc) -> Option<String> {
    manifest::spec_of::<AppSpec>(doc).ok().map(|s| s.image)
}

pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: AppSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(e.to_string()))
}

fn simple_network_doc(name: &str) -> ManifestDoc {
    ManifestDoc {
        api_version: "delonix.io/v1".to_string(),
        kind: k::NETWORK.to_string(),
        metadata: Metadata {
            name: name.to_string(),
            namespace: None,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
        },
        spec: serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
    }
}

fn build_net_name(app: &str) -> String {
    format!("app-build-{app}")
}
fn registry_name(app: &str) -> String {
    format!("app-build-{app}-registry")
}
fn builder_name(app: &str) -> String {
    format!("app-build-{app}-builder")
}

/// Idempotent — tears down whatever this app's build resources left behind,
/// whether from a previous failed attempt or the normal end of a successful
/// one. Errors are logged and swallowed: this is cleanup, and a half-removed
/// set of resources should not stop the caller from reporting the build's
/// own real result.
fn teardown_build_resources(images: &ImageStore, store: &Store, app: &str) {
    let (builder, registry, net) = (builder_name(app), registry_name(app), build_net_name(app));
    for name in [&builder, &registry] {
        if let Err(e) = super::container::cmd_rm(images, store, name, true) {
            if find(store, name).is_ok() {
                super::output::warn(&format!("app: could not remove '{name}': {e}"));
            }
        }
    }
    if let Ok(net_store) = NetworkStore::open(state_root()) {
        let _ = super::network::cmd_rm(&net_store, &net);
    }
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (images, store) = open_stores()?;
    for doc in manifest::of_kind(docs, k::APP) {
        apply_one(&images, &store, doc)?;
    }
    Ok(())
}

/// Validates that `source` is a directory and canonicalizes it.
///
/// Canonicalizing here — BEFORE it becomes a bind-mount source string
/// (`CnbPlan::mounts`) — matters because the builder container is created
/// on a custom network, which takes the `--net <custom>` re-exec path into
/// a different process (a different CWD): a relative "." surviving to that
/// point resolves against the wrong directory instead of this one.
fn resolve_source_dir(source: &str, name: &str) -> Result<PathBuf> {
    let source_dir = PathBuf::from(source);
    if !source_dir.is_dir() {
        return Err(Error::Invalid(format!(
            "app '{name}': source '{}' is not a directory",
            source_dir.display()
        )));
    }
    source_dir.canonicalize().map_err(|e| {
        Error::Invalid(format!(
            "app '{name}': failed to resolve source '{source}': {e}"
        ))
    })
}

fn apply_one(images: &ImageStore, store: &Store, doc: &ManifestDoc) -> Result<()> {
    let spec: AppSpec = manifest::spec_of(doc)?;
    let name = &doc.metadata.name;
    let source_dir = resolve_source_dir(&spec.source, name)?;

    let resolved = resolve_builder(&spec)?;
    let (builder_image, run_image) = resolved.images();

    // "auto" only NAMES the family (Paketo already does its own in-image
    // detection across every buildpack it ships) — `detect.rs` is used here
    // only to fail fast with a clear reason before spending minutes on a
    // build the builder itself would also refuse, and to tell the operator
    // what stack was recognized.
    if spec.builder == "auto" {
        match detect::detect(&source_dir) {
            Some(d) => eprintln!(
                "app '{name}': detected {} ({})",
                d.stack,
                d.framework.unwrap_or("no framework")
            ),
            None => {
                return Err(Error::Invalid(format!(
                    "app '{name}': no buildable stack detected in '{}' — none of the \
                     recognized markers (go.mod, Cargo.toml, package.json, requirements.txt, \
                     Gemfile, composer.json, index.html, ...) were found",
                    source_dir.display()
                )));
            }
        }
    }

    // Defensive: a previous attempt for this SAME app may have left its
    // build resources behind (a crash, a killed `apply`). Idempotent by
    // construction — the same reason `container run`'s own auto-heal
    // re-derives state instead of assuming a clean slate.
    teardown_build_resources(images, store, name);

    let net_name = build_net_name(name);
    super::network::apply(&[simple_network_doc(&net_name)])?;

    let host_port = super::compose::free_host_port()?;
    let reg_name = registry_name(name);
    super::container::cmd_run(
        images,
        store,
        super::container::RunOpts {
            detach: true,
            name: Some(reg_name.clone()),
            net: net_name.clone(),
            ports: vec![format!("{host_port}:5000")],
            image: "registry:2".to_string(),
            quiet: true,
            ..Default::default()
        },
    )?;
    // The registry's SDN IP, not its internal-DNS name, is what the CNB
    // lifecycle's registry client (go-containerregistry) needs to see: it
    // only auto-selects plain HTTP for an RFC1918 address or a literal
    // "localhost:<port>" — any other hostname, DNS name included, gets
    // HTTPS and fails ("server gave HTTP response to HTTPS client"), found
    // only by running a real build. This engine's SDN subnets are always
    // RFC1918 (10.x/16 per network), so the registry's own address already
    // satisfies the heuristic without a config file or an extra flag.
    let reg_ip = find(store, &reg_name)?
        .ip
        .ok_or_else(|| Error::Invalid(format!("app '{name}': the build registry has no IP")))?;

    let plan = CnbPlan::new(
        name,
        &source_dir,
        &spec.builder,
        &format!("{reg_ip}:5000/{name}"),
    );
    let cache_volume = plan.cache_volume.clone();
    let mounts: Vec<String> = plan
        .mounts()
        .into_iter()
        .map(|(src, dst)| format!("{src}:{dst}"))
        .collect();
    let bname = builder_name(name);
    let build_result = (|| -> Result<()> {
        super::container::cmd_run(
            images,
            store,
            super::container::RunOpts {
                detach: true,
                name: Some(bname.clone()),
                net: net_name.clone(),
                volumes: mounts,
                image: builder_image.clone(),
                command: vec!["sleep".into(), "infinity".into()],
                env: vec![format!("CNB_PLATFORM_API={CNB_PLATFORM_API}")],
                quiet: true,
                ..Default::default()
            },
        )?;
        let c = find(store, &bname)?;
        // Override the run-image only for a KNOWN family whose default
        // already matches `run_image` — for `Explicit`, `run_image` was
        // supplied by the caller and IS the plan's run image already.
        let mut args = plan.creator_args();
        if run_image != plan.run_image {
            for a in &mut args {
                if a.starts_with("-run-image=") {
                    *a = format!("-run-image={run_image}");
                }
            }
        }
        eprintln!("app '{name}': building ({builder_image})…");
        let code = runtime::exec(&c, &args, false)?;
        if code != 0 {
            return Err(Error::Invalid(format!(
                "app '{name}': build failed (creator exit {code}) — see \
                 `delonix container logs {bname}`; the build's containers were left up for \
                 inspection, clean up with `delonix container rm -f {bname} {reg_name}` and \
                 `delonix network rm {net_name}`"
            )));
        }
        Ok(())
    })();

    build_result?;

    // The image now exists in the ephemeral registry, reachable from the
    // HOST via the port just published — pull it back into the LOCAL store
    // under the name the manifest asked for, the same path `resolve_or_pull`
    // already uses for any other registry.
    let remote_ref = format!("127.0.0.1:{host_port}/{name}");
    let pulled = delonix_image::pull_from_registry(images, &remote_ref);
    teardown_build_resources(images, store, name);
    let _ = std::fs::remove_dir_all(state_root().join("volumes").join(&cache_volume));
    match pulled {
        Ok(_) => {
            images.tag(&remote_ref, &spec.image)?;
            println!("app '{name}': built -> {}", spec.image);
            Ok(())
        }
        Err(e) => Err(Error::Invalid(format!(
            "app '{name}': the build reported success but the image could not be pulled back \
             from the ephemeral registry: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(builder: &str, run_image: Option<&str>) -> AppSpec {
        AppSpec {
            source: ".".to_string(),
            builder: builder.to_string(),
            run_image: run_image.map(str::to_string),
            image: "x:latest".to_string(),
        }
    }

    #[test]
    fn auto_resolves_to_the_paketo_family() {
        let (b, r) = resolve_builder(&spec("auto", None)).unwrap().images();
        assert_eq!(b, "paketobuildpacks/builder-jammy-base");
        assert_eq!(r, "paketobuildpacks/run-jammy-base");
    }

    #[test]
    fn heroku_resolves_to_the_heroku_family() {
        let (b, r) = resolve_builder(&spec("heroku", None)).unwrap().images();
        assert_eq!(b, "heroku/builder:24");
        assert_eq!(r, "heroku/heroku:24");
    }

    #[test]
    fn an_unknown_builder_without_run_image_is_refused() {
        let e = resolve_builder(&spec("paketto-typo", None))
            .unwrap_err()
            .to_string();
        assert!(e.contains("paketto-typo"), "{e}");
        assert!(e.contains("runImage"), "{e}");
    }

    #[test]
    fn an_explicit_builder_with_run_image_is_accepted() {
        let (b, r) = resolve_builder(&spec("myorg/builder:1", Some("myorg/run:1")))
            .unwrap()
            .images();
        assert_eq!(b, "myorg/builder:1");
        assert_eq!(r, "myorg/run:1");
    }

    #[test]
    fn source_dir_is_canonicalized_not_left_relative() {
        // The default `source: "."` must never survive as a relative path:
        // it becomes a bind-mount source string (`CnbPlan::mounts`) built
        // by a container created on a custom network, which re-execs into
        // a different process with a different CWD.
        let resolved = resolve_source_dir(".", "x").unwrap();
        assert!(resolved.is_absolute(), "{}", resolved.display());
    }

    #[test]
    fn source_dir_refuses_a_file_that_is_not_a_directory() {
        let e = resolve_source_dir("Cargo.toml", "x")
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a directory"), "{e}");
    }

    #[test]
    fn app_spec_defaults_source_and_builder() {
        let yaml = "image: x:latest\n";
        let spec: AppSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.source, ".");
        assert_eq!(spec.builder, "auto");
        assert!(spec.run_image.is_none());
    }
}
