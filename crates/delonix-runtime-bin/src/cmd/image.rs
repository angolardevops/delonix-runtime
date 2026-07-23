//! `delonix image` — pull/ls/rm/export.

use std::path::PathBuf;

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_image::ImageStore;
use delonix_runtime_core::{Error, Result};
use oci_spec::runtime::{
    get_default_maskedpaths, get_default_mounts, get_default_namespaces,
    get_default_readonly_paths, Capability, LinuxBuilder, LinuxCapabilitiesBuilder, ProcessBuilder,
    RootBuilder, Spec, SpecBuilder, User,
};
use serde::{Deserialize, Serialize};

use super::manifest::{self, ManifestDoc};
use super::util::{effective_command, open_stores, resolve_or_pull};

/// `spec` of `kind: Image` — either `pull: <ref>` or `build: {...}` (mutually
/// exclusive; clear error if both are missing).
#[derive(Debug, Deserialize, Serialize)]
struct ImageSpec {
    pull: Option<String>,
    build: Option<BuildSpec>,
}

/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: ImageSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

/// Field names accepted in the `spec` of `kind: Image`, for the unknown-field warning.
pub(crate) const IMAGE_SPEC_FIELDS: &[&str] = &["pull", "build"];

#[derive(Debug, Deserialize, Serialize)]
struct BuildSpec {
    #[serde(default = "default_context")]
    context: PathBuf,
    file: Option<PathBuf>,
    tag: String,
    /// `ARG` overrides (`KEY=VALUE`) — same semantics as the CLI's `--build-arg`:
    /// only takes effect for a name the Dockerfile actually declares.
    #[serde(default, rename = "buildArgs")]
    build_args: Vec<String>,
    /// Bypasses the layer cache — same as the CLI's `--no-cache`.
    #[serde(default, rename = "noCache")]
    no_cache: bool,
}

fn default_context() -> PathBuf {
    PathBuf::from(".")
}

#[derive(Subcommand)]
pub enum ImageCmd {
    /// Dashboard (KPIs + table) of images — interactive TUI, or `--once` snapshot.
    Dash {
        #[arg(long)]
        once: bool,
    },
    /// Pull an image from a registry.
    Pull {
        image: String,
        /// Verify the cosign signature with this public key (PEM) AFTER the
        /// pull, and fail if it does not match. Without this, a pull is not
        /// authenticated beyond the registry's own digest.
        #[arg(long, value_name = "PEM")]
        verify: Option<PathBuf>,
    },
    /// List local images.
    Ls,
    /// Human-readable detail of one or more images, `kubectl describe`-style
    /// (tags/digest/size/layers + the OCI config: entrypoint/cmd/env/workdir).
    /// With `--vm`, describes golden VM images.
    Describe {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::images))]
        names: Vec<String>,
    },
    /// Give another name/tag to a local image (copies nothing — it's just a new
    /// name for the same content).
    Tag {
        #[arg(add = ArgValueCandidates::new(super::complete::images))]
        source: String,
        target: String,
    },
    /// Layers of an image (digest + size), from base to top.
    History {
        #[arg(add = ArgValueCandidates::new(super::complete::images))]
        image: String,
    },
    /// Verify the cosign signature of a local image against a public key.
    Verify {
        #[arg(add = ArgValueCandidates::new(super::complete::images))]
        image: String,
        /// Public key in PEM.
        #[arg(value_name = "PEM")]
        key: PathBuf,
    },
    /// SBOM + CVE scan of an image (reads the layers from the CAS, without running anything).
    /// Pulls the image if missing. See `--sbom`, `--fail-on`, `--update`.
    Scan {
        /// Image to scan (optional with `--update`).
        image: Option<String>,
        /// List the SBOM (installed packages) instead of scanning.
        #[arg(long)]
        sbom: bool,
        /// Fail (exit 1) if there are vulnerabilities >= this severity
        /// (low|medium|high|critical) — gate for CI.
        #[arg(long = "fail-on", value_name = "SEV")]
        fail_on: Option<String>,
        /// Sync the CVE feed to the local database (used afterwards by each scan).
        #[arg(long)]
        update: bool,
        /// Feed source for `--update`: URL or file (or $DELONIX_ADVISORY_FEED).
        #[arg(long = "feed", value_name = "URL|FICHEIRO")]
        feed: Option<String>,
    },
    /// Remove a local image.
    Rm {
        #[arg(add = ArgValueCandidates::new(super::complete::images))]
        image: String,
    },
    /// Export an OCI runtime bundle (rootfs + config.json) for `runc`/`crun`.
    Export {
        #[arg(add = ArgValueCandidates::new(super::complete::images))]
        image: String,
        dir: PathBuf,
    },
    /// Apply the `kind: Image` documents of a manifest (`pull` idempotent
    /// by reference; `build` rebuilds and replaces the tag on each apply).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Authenticate to an OCI registry (stores the credentials in `<root>/auth.json`,
    /// docker/podman format). The password ALWAYS comes from stdin — never from an
    /// argument (it would end up in the shell history and in /proc).
    Login {
        /// Registry (e.g. `ghcr.io`, `docker.io`).
        registry: String,
        #[arg(short = 'u', long = "username")]
        username: String,
        /// Read the password/token from stdin (the only supported way).
        #[arg(long = "password-stdin")]
        password_stdin: bool,
    },
    /// Remove the stored credentials of a registry.
    Logout { registry: String },
    /// Golden VM images (`<root>/vm-images/`): ls/pull/push/build.
    /// Equivalent to `image --vm <cmd>` (old form, kept).
    Vm {
        #[command(subcommand)]
        action: VmSub,
    },
    /// Publish a local image to an OCI registry. Without `target`, publishes under
    /// the image's own reference. With `--vm`, `target` is required.
    Push {
        #[arg(add = ArgValueCandidates::new(super::complete::images))]
        name: String,
        target: Option<String>,
    },
    /// (only with `--vm`) Build the golden VM image (Ubuntu + kubeadm/kubelet/
    /// kubectl + `delonix-cri`).
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        #[arg(long, default_value = "26.04")]
        ubuntu_release: String,
        #[arg(long)]
        k8s_version: Option<String>,
        #[arg(long = "extra-package")]
        extra_packages: Vec<String>,
        #[arg(long = "extra-run")]
        extra_run: Vec<String>,
        #[arg(long)]
        cri_bin: Option<PathBuf>,
        /// Do not compress the final qcow2 (larger, but with no decompression
        /// cost on backing-file reads at runtime).
        #[arg(long)]
        no_compress: bool,
        /// Fetch the k8s .deb packages on the HOST (verified) and install them with `dpkg` —
        /// the appliance runs without network. No DHCP/DNS needed in the guest.
        #[arg(long)]
        offline: bool,
    },
}

/// Subcommands of `image vm` — mirror `cmd::vmimage::VmImageCmd` 1:1.
#[derive(Subcommand)]
pub enum VmSub {
    /// List the local VM images.
    Ls,
    /// Human-readable detail of one or more VM images, `kubectl describe`-style.
    Describe {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Fetch a VM image from an OCI registry (single-blob artifact).
    Pull {
        source: String,
        /// Local name (default: derived from the reference).
        #[arg(long)]
        name: Option<String>,
    },
    /// Publish a local VM image to an OCI registry.
    Push { name: String, target: String },
    /// Build the golden VM image (Ubuntu + kubeadm/kubelet/kubectl + `delonix-cri`).
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        #[arg(long, default_value = "26.04")]
        ubuntu_release: String,
        #[arg(long)]
        k8s_version: Option<String>,
        #[arg(long = "extra-package")]
        extra_packages: Vec<String>,
        #[arg(long = "extra-run")]
        extra_run: Vec<String>,
        #[arg(long)]
        cri_bin: Option<PathBuf>,
        /// Do not compress the final qcow2 (larger, but with no decompression
        /// cost on backing-file reads at runtime).
        #[arg(long)]
        no_compress: bool,
        /// Fetch the k8s .deb packages on the HOST (verified) and install them with `dpkg` —
        /// the appliance runs without network. No DHCP/DNS needed in the guest.
        #[arg(long)]
        offline: bool,
    },
}

/// `vm`: enables `--vm` in the `image` group — dispatches `ls`/`pull`/`push`/`build`
/// to `cmd::vmimage` (golden VM images) instead of `ImageStore` (container
/// images). `rm`/`export`/`apply` make no sense for VM images at this
/// stage — clear error instead of silently wrong behavior.
pub fn run(vm: bool, action: ImageCmd) -> Result<()> {
    // login/logout are agnostic to container-vs-VM (same auth.json).
    match &action {
        ImageCmd::Dash { once } => {
            return super::dash::run(super::dash::DashScope::Images, *once);
        }
        ImageCmd::Login {
            registry,
            username,
            password_stdin,
        } => {
            return cmd_login(registry, username, *password_stdin);
        }
        ImageCmd::Logout { registry } => {
            delonix_image::auth::logout(&super::util::state_root(), registry)?;
            println!("credenciais de {registry} removidas");
            return Ok(());
        }
        _ => {}
    }
    if let ImageCmd::Vm { action } = action {
        use super::vmimage::{self, VmImageCmd};
        return vmimage::run(match action {
            VmSub::Ls => VmImageCmd::Ls,
            VmSub::Describe { names } => VmImageCmd::Describe { names },
            VmSub::Pull { source, name } => VmImageCmd::Pull { source, name },
            VmSub::Push { name, target } => VmImageCmd::Push { name, target },
            VmSub::Build {
                tag,
                ubuntu_release,
                k8s_version,
                extra_packages,
                extra_run,
                cri_bin,
                no_compress,
                offline,
            } => VmImageCmd::Build {
                tag,
                ubuntu_release,
                k8s_version,
                extra_packages,
                extra_run,
                cri_bin,
                no_compress,
                offline,
            },
        });
    }
    if vm {
        return run_vm(action);
    }
    let (images, _store) = open_stores()?;
    match action {
        ImageCmd::Dash { .. } => unreachable!("tratado no topo de run"),
        ImageCmd::Pull { image, verify } => cmd_pull(&images, &image, verify.as_deref()),
        ImageCmd::Ls => cmd_ls(&images),
        ImageCmd::Describe { names } => cmd_describe(&images, &names),
        ImageCmd::Tag { source, target } => cmd_tag(&images, &source, &target),
        ImageCmd::History { image } => cmd_history(&images, &image),
        ImageCmd::Verify { image, key } => cmd_verify(&images, &image, &key),
        ImageCmd::Scan { image, sbom, fail_on, update, feed } => {
            if update {
                super::scan::cmd_scan_update(feed)
            } else {
                let image = image.ok_or_else(|| Error::Invalid("indica a imagem a analisar, ou usa `--update` para sincronizar o feed".into()))?;
                super::scan::cmd_scan(&image, sbom, fail_on.as_deref())
            }
        }
        ImageCmd::Rm { image } => cmd_rm(&images, &image),
        ImageCmd::Export { image, dir } => cmd_export(&images, &image, &dir),
        ImageCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
        ImageCmd::Push { name, target } => cmd_push(&images, &name, target.as_deref()),
        ImageCmd::Build { .. } => Err(Error::Invalid(
            "`build` neste grupo é só para imagens VM — usa `delonix image --vm build`, ou `delonix build` para imagens de container".into(),
        )),
        ImageCmd::Login { .. } | ImageCmd::Logout { .. } | ImageCmd::Vm { .. } => unreachable!("tratados acima"),
    }
}

/// `image login` — reads the password from stdin (mandatory: an argument would end up
/// in the shell history and be visible in /proc) and delegates to `delonix_image::auth`.
fn cmd_login(registry: &str, username: &str, password_stdin: bool) -> Result<()> {
    if !password_stdin {
        return Err(Error::Invalid(
            "usa --password-stdin (ex.: `gh auth token | delonix image login ghcr.io -u USER --password-stdin`)".into(),
        ));
    }
    let mut pw = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut pw)
        .map_err(|e| Error::Invalid(format!("a ler a password do stdin: {e}")))?;
    let pw = pw.trim();
    if pw.is_empty() {
        return Err(Error::Invalid("password vazia no stdin".into()));
    }
    delonix_image::auth::login(&super::util::state_root(), registry, username, pw)?;
    println!("login em {registry} guardado (auth.json)");
    Ok(())
}

fn run_vm(action: ImageCmd) -> Result<()> {
    use super::vmimage::{self, VmImageCmd};
    let mapped = match action {
        ImageCmd::Dash { .. } => unreachable!("tratado no topo de run"),
        ImageCmd::Ls => VmImageCmd::Ls,
        ImageCmd::Describe { names } => VmImageCmd::Describe { names },
        ImageCmd::Pull { image, verify: _ } => VmImageCmd::Pull {
            source: image,
            name: None,
        },
        ImageCmd::Push { name, target } => VmImageCmd::Push {
            name,
            // A VM image has no repo_tags from which to infer the destination.
            target: target.ok_or_else(|| {
                Error::Invalid(
                    super::po::t("`image --vm push <name> <dest>`: the destination is required")
                        .into(),
                )
            })?,
        },
        ImageCmd::Build {
            tag,
            ubuntu_release,
            k8s_version,
            extra_packages,
            extra_run,
            cri_bin,
            no_compress,
            offline,
        } => VmImageCmd::Build {
            tag,
            ubuntu_release,
            k8s_version,
            extra_packages,
            extra_run,
            cri_bin,
            no_compress,
            offline,
        },
        ImageCmd::Tag { .. }
        | ImageCmd::History { .. }
        | ImageCmd::Verify { .. }
        | ImageCmd::Scan { .. } => return Err(Error::Invalid(
            "tag/history/verify são de imagens de container — não se aplicam a imagens VM (--vm)"
                .into(),
        )),
        ImageCmd::Rm { .. } | ImageCmd::Export { .. } | ImageCmd::Apply { .. } => {
            return Err(Error::Invalid(
                "comando não disponível para imagens VM (--vm) — usa ls/pull/push/build".into(),
            ))
        }
        ImageCmd::Login { .. } | ImageCmd::Logout { .. } | ImageCmd::Vm { .. } => {
            unreachable!("tratados em run()")
        }
    };
    vmimage::run(mapped)
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (images, _store) = open_stores()?;
    for doc in manifest::of_kind(docs, "Image") {
        let name = &doc.metadata.name;
        manifest::warn_unknown_fields(doc, IMAGE_SPEC_FIELDS);
        let spec: ImageSpec = manifest::spec_of(doc)?;
        match (spec.pull, spec.build) {
            (Some(reference), None) => {
                resolve_or_pull(&images, &reference)?;
                println!("image/{name}: garantida ({reference})");
            }
            (None, Some(b)) => {
                let file = b
                    .file
                    .unwrap_or_else(|| super::build::default_build_file(&b.context));
                let build_args = super::build::parse_build_args(&b.build_args);
                let img = super::build::build_from_spec(
                    &b.context,
                    &file,
                    &b.tag,
                    &build_args,
                    !b.no_cache,
                )?;
                println!(
                    "image/{name}: {} ({})",
                    super::po::t("built"),
                    img.short_id()
                );
            }
            (Some(_), Some(_)) => {
                return Err(Error::Invalid(format!(
                    "image/{name}: spec tem `pull` E `build` — só um dos dois"
                )))
            }
            (None, None) => {
                return Err(Error::Invalid(format!(
                    "image/{name}: spec sem `pull` nem `build`"
                )))
            }
        }
    }
    Ok(())
}

fn cmd_pull(images: &ImageStore, reference: &str, verify: Option<&std::path::Path>) -> Result<()> {
    let img = delonix_image::pull_from_registry(images, reference)?;
    // Verify AFTER the pull (the cosign signature lives in a tag alongside the
    // image in the registry, so we need it here). If it fails, the command fails —
    // the image stays local, but whoever asked for `--verify` knows it is untrusted.
    if let Some(key) = verify {
        let pem = std::fs::read_to_string(key)?;
        let digest = delonix_image::verify_signature(images, reference, &pem)?;
        println!("assinatura válida para {reference} ({digest})");
    }
    // CVE admission policy (scan-on-pull): off by default (no latency),
    // opt-in via `DELONIX_SCAN_ON_PULL`. Closes the "pull without looking inside" —
    // see `scan::admission_scan_on_pull`. Runs AFTER the signature
    // verification: first "is it who it says it is", then "does it bring dangerous stuff?".
    super::scan::admission_scan_on_pull(images, reference, &img)?;
    println!("{}", img.short_id());
    Ok(())
}

/// `image tag` — another name for the same content (does not copy layers).
fn cmd_tag(images: &ImageStore, source: &str, target: &str) -> Result<()> {
    images.tag(source, target)?;
    println!("{source} -> {target}");
    Ok(())
}

/// `image history` — the image's layers, from base to top.
///
/// The `#` is the position in the stack (0 = base), as in `docker history`. The size is
/// that of the COMPRESSED blob in the CAS — see the note in `image_size`.
fn cmd_history(images: &ImageStore, image: &str) -> Result<()> {
    let img = images.resolve(image)?;
    let mut t = super::output::Table::new(&["#", "LAYER", "SIZE"]).right_align(2);
    for (i, dg) in img.layers.iter().enumerate() {
        let size = std::fs::metadata(images.cas().path(dg))
            .map(|m| m.len())
            .unwrap_or(0);
        t.row(vec![
            i.to_string(),
            super::output::truncate(dg, 23),
            super::output::fmt_size(size),
        ]);
    }
    t.print();
    Ok(())
}

/// `image verify` — cosign signature against a public key.
fn cmd_verify(images: &ImageStore, image: &str, key: &std::path::Path) -> Result<()> {
    let pem = std::fs::read_to_string(key)?;
    let digest = delonix_image::verify_signature(images, image, &pem)?;
    println!("OK: assinatura válida para {image} ({digest})");
    Ok(())
}

/// `image push` — publishes a container image to an OCI registry.
fn cmd_push(images: &ImageStore, image: &str, destination: Option<&str>) -> Result<()> {
    // Without a destination, publishes under its own reference (the common case: the image
    // was already built with the destination registry's tag).
    let dest = destination.unwrap_or(image);
    let digest = delonix_image::push_to_registry(images, image, dest)?;
    println!("{dest}  {digest}");
    Ok(())
}

/// Size of an image = sum of its layers' blobs in the CAS.
///
/// **Not the "SIZE" from `docker images`**, which is the UNCOMPRESSED rootfs; here it is
/// what the image actually occupies on disk (compressed layers, shared among
/// images that reuse them). It is the only measure obtainable without decompressing
/// everything, and it is the one that answers the question asked of an `ls` ("how much
/// space does this use?"). A layer missing from the CAS does not count — hence `Option`
/// only when NOTHING is readable, so as not to report "0 B" for an image whose blobs
/// have disappeared.
fn image_size(images: &ImageStore, img: &delonix_image::Image) -> Option<u64> {
    if img.layers.is_empty() {
        return None;
    }
    let mut total = 0u64;
    let mut seen_any = false;
    for l in &img.layers {
        if let Ok(m) = std::fs::metadata(images.cas().path(l)) {
            total += m.len();
            seen_any = true;
        }
    }
    seen_any.then_some(total)
}

fn cmd_ls(images: &ImageStore) -> Result<()> {
    let mut imgs = images.list()?;
    // Newest first, as in `docker images`.
    imgs.sort_by_key(|i| std::cmp::Reverse(i.created_unix));
    let mut t = super::output::Table::new(&["REPOSITORY:TAG", "IMAGE ID", "CREATED", "SIZE"])
        .right_align(3);
    for img in imgs {
        let tag = img
            .repo_tags
            .first()
            .cloned()
            .unwrap_or_else(|| "<none>".into());
        t.row(vec![
            // `display_ref` strips the redundant `@sha256:…` (the tag already identifies it);
            // `truncate` is the safety net for huge repo names.
            super::output::truncate(&super::output::display_ref(&tag), 44),
            img.short_id(),
            // It used to be the raw epoch (`CRIADA(unix)`) — unreadable in a table.
            super::output::fmt_age(img.created_unix),
            image_size(images, &img)
                .map(super::output::fmt_size)
                .unwrap_or_else(|| "-".into()),
        ]);
    }
    t.print();
    Ok(())
}

/// `image describe` — human-readable detail, `kubectl describe`-style.
fn cmd_describe(images: &ImageStore, names: &[String]) -> Result<()> {
    for (i, name) in names.iter().enumerate() {
        // `resolve` (not `resolve_or_pull`): describing is not fetching — a
        // `describe` of a nonexistent image should say so, not spend
        // minutes pulling from the registry by mistake.
        let img = images.resolve(name)?;
        if i > 0 {
            println!();
        }
        describe_one(images, &img);
    }
    Ok(())
}

fn describe_one(images: &ImageStore, img: &delonix_image::Image) {
    let mut d = super::output::Describe::new();
    d.field("ID", &img.id);
    d.field("Short ID", img.short_id());
    d.list("Tags", &img.repo_tags);
    d.field("Created", super::output::fmt_local(img.created_unix));
    d.field("Age", super::output::fmt_age(img.created_unix));
    d.field(
        "Size",
        image_size(images, img)
            .map(super::output::fmt_size)
            .unwrap_or_else(|| "<unknown>".into()),
    );

    // Layers with each blob's size — it's what shows WHERE the weight is.
    if img.layers.is_empty() {
        d.field("Layers", "<none>");
    } else {
        d.section("Layers");
        for l in &img.layers {
            let sz = std::fs::metadata(images.cas().path(l))
                .map(|m| super::output::fmt_size(m.len()))
                .unwrap_or_else(|_| "<missing>".into());
            d.item(format!("{l}  {sz}"));
        }
    }

    let c = &img.config;
    d.section("Config");
    d.sub(
        "Entrypoint",
        if c.entrypoint.is_empty() {
            "<none>".to_string()
        } else {
            c.entrypoint.join(" ")
        },
    );
    d.sub(
        "Cmd",
        if c.cmd.is_empty() {
            "<none>".to_string()
        } else {
            c.cmd.join(" ")
        },
    );
    d.sub(
        "Workdir",
        if c.working_dir.is_empty() {
            "/"
        } else {
            &c.working_dir
        },
    );
    d.sub("User", if c.user.is_empty() { "root" } else { &c.user });
    // Delonix extensions of the Dockerfile/Delonixfile (`CPUS`/`MEMORY`/`SECURITY`/
    // `HEALTHCHECK`) — omitted entirely on images that do not have them.
    d.sub_opt("CPUs", c.cpus.as_deref());
    d.sub_opt("Memory", c.memory.as_deref());
    d.sub_opt("Healthcheck", c.healthcheck.as_deref());
    if !c.security.is_empty() {
        d.sub("Security", c.security.join(", "));
    }
    d.list("Env", &c.env);
    d.print();
}

fn cmd_rm(images: &ImageStore, reference: &str) -> Result<()> {
    let removed = images.remove(reference)?;
    println!("{removed}");
    Ok(())
}

/// Writes a minimal OCI runtime bundle (rootfs + config.json) for `runc`/`crun`.
fn cmd_export(images: &ImageStore, reference: &str, dir: &std::path::Path) -> Result<()> {
    let img = resolve_or_pull(images, reference)?;
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::Invalid(format!("mkdir {}: {e}", dir.display())))?;
    let rootfs = dir.join("rootfs");
    images.export_rootfs(&img, &rootfs)?;
    let args = effective_command(&img, &[]);
    let args = if args.is_empty() {
        vec!["/bin/sh".to_string()]
    } else {
        args
    };
    let cwd = if img.config.working_dir.is_empty() {
        "/".to_string()
    } else {
        img.config.working_dir.clone()
    };
    let spec = build_runtime_spec(args, img.config.env.clone(), cwd)?;
    let cfg = dir.join("config.json");
    let json = serde_json::to_vec_pretty(&spec)
        .map_err(|e| Error::Invalid(format!("serializar spec OCI: {e}")))?;
    std::fs::write(&cfg, json)
        .map_err(|e| Error::Invalid(format!("escrever {}: {e}", cfg.display())))?;
    println!("bundle OCI em {}", dir.display());
    println!("corre com:  runc run -b {} delonix-oci", dir.display());
    Ok(())
}

/// Builds a **conformant OCI-runtime** `config.json` from the `oci-spec`
/// canonical types (instead of the previous hand-written JSON, which was incomplete).
/// PURE — no IO — so it can be validated by a round-trip test against
/// `oci_spec::runtime::Spec` itself.
///
/// It differs from the previous minimal bundle in three points that made it **non-functional**
/// with `runc`/`crun` (not just non-conformant):
/// 1. **`mounts`** — before there were NONE. Without `/proc`, `/sys`, `/dev/pts`,
///    `/dev/shm`, `/dev/mqueue` the container started without `/proc` and most
///    workloads broke. Now uses the `runc spec` standard set.
/// 2. **Capabilities** — before only `bounding` was defined, so the process (uid 0)
///    ended up with an empty EFFECTIVE set (neither `chown` nor bind <1024). Now the
///    same set goes to bounding+effective+permitted; inheritable/ambient empty
///    (least privilege, consistent with `noNewPrivileges`).
/// 3. **`maskedPaths`/`readonlyPaths`** — standard hardening (`/proc/kcore`, …)
///    that the previous bundle omitted entirely.
fn build_runtime_spec(args: Vec<String>, env: Vec<String>, cwd: String) -> Result<Spec> {
    let mkerr = |what: &'static str| {
        move |e: oci_spec::OciSpecError| Error::Invalid(format!("{what}: {e}"))
    };

    // The same capability posture as the previous bundle, but applied to the three
    // sets that make it EFFECTIVE (not just the `bounding` ceiling).
    let caps: std::collections::HashSet<Capability> = [
        Capability::Chown,
        Capability::DacOverride,
        Capability::Fowner,
        Capability::Setgid,
        Capability::Setuid,
        Capability::NetBindService,
    ]
    .into_iter()
    .collect();
    let capabilities = LinuxCapabilitiesBuilder::default()
        .bounding(caps.clone())
        .effective(caps.clone())
        .permitted(caps)
        .inheritable(std::collections::HashSet::new())
        .ambient(std::collections::HashSet::new())
        .build()
        .map_err(mkerr("capabilities"))?;

    let process = ProcessBuilder::default()
        .terminal(false)
        .user(User::default()) // uid 0 / gid 0 — as before
        .args(args)
        .env(env)
        .cwd(cwd)
        .capabilities(capabilities)
        .no_new_privileges(true)
        .build()
        .map_err(mkerr("process"))?;

    let root = RootBuilder::default()
        .path("rootfs")
        .readonly(false)
        .build()
        .map_err(mkerr("root"))?;

    // Standard namespaces/masked/readonly-paths of the `runc spec` — the
    // conformance target. (Includes an isolated network namespace, like the `runc spec`;
    // whoever wants host networking edits the `config.json`.)
    let linux = LinuxBuilder::default()
        .namespaces(get_default_namespaces())
        .masked_paths(get_default_maskedpaths())
        .readonly_paths(get_default_readonly_paths())
        .build()
        .map_err(mkerr("linux"))?;

    SpecBuilder::default()
        .version("1.0.2")
        .hostname("delonix")
        .root(root)
        .process(process)
        .mounts(get_default_mounts())
        .linux(linux)
        .build()
        .map_err(mkerr("spec"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OCI-runtime conformance of the exported bundle: serializes and **deserializes
    /// again** through the canonical `oci_spec::runtime::Spec` — if our JSON
    /// diverged from the schema, the round-trip would fail here.
    #[test]
    fn bundle_exportado_e_conformante_oci_runtime() {
        let spec = build_runtime_spec(
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo hi".to_string(),
            ],
            vec!["PATH=/usr/bin".to_string()],
            "/work".to_string(),
        )
        .expect("build spec");

        let json = serde_json::to_vec(&spec).expect("serializar");
        let parsed: Spec = serde_json::from_slice(&json).expect("round-trip pelo tipo canónico");

        // ociVersion present and semantically valid.
        assert_eq!(parsed.version(), "1.0.2");

        // The central FIX: standard mounts present — in particular `/proc`, without which
        // the container started broken. Before this commit there were no mounts at all.
        let mounts = parsed.mounts().as_ref().expect("mounts");
        assert!(
            mounts
                .iter()
                .any(|m| m.destination() == std::path::Path::new("/proc")),
            "bundle tem de montar /proc (era a lacuna que o tornava não-funcional)"
        );
        assert!(
            mounts.len() >= 5,
            "conjunto de mounts padrão do runc (proc/sys/dev/pts/shm/…)"
        );

        // Process: args/env/cwd propagated and EFFECTIVE capabilities (not just bounding).
        let proc = parsed.process().as_ref().expect("process");
        assert_eq!(proc.args().as_ref().unwrap()[0], "/bin/sh");
        assert_eq!(proc.cwd(), std::path::Path::new("/work"));
        assert_eq!(proc.no_new_privileges(), Some(true));
        let caps = proc.capabilities().as_ref().expect("capabilities");
        let eff = caps.effective().as_ref().expect("effective caps");
        assert!(
            eff.contains(&Capability::NetBindService),
            "as capacidades têm de ir ao conjunto EFETIVO, não só ao bounding"
        );

        // Standard hardening that the previous bundle omitted.
        let linux = parsed.linux().as_ref().expect("linux");
        assert!(!linux.masked_paths().as_ref().expect("masked").is_empty());
        assert!(!linux.namespaces().as_ref().expect("namespaces").is_empty());
    }
}
