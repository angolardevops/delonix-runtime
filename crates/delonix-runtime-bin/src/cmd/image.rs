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
use super::vmimage::Distro;

/// `spec` of `kind: Image` — either `pull: <ref>` or `build: {...}` (mutually
/// exclusive; clear error if both are missing).
#[derive(Debug, Deserialize, Serialize)]
struct ImageSpec {
    pull: Option<String>,
    build: Option<BuildSpec>,
    /// `kind: Secret` holding `username`/`password` for the registry this image
    /// is pulled from.
    ///
    /// Without it the pull uses the machine's credential vault
    /// (`delonix image login`) — per-MACHINE state that a manifest cannot
    /// carry. A `kind: Image` naming a private registry therefore applied
    /// cleanly on the host where someone had logged in and failed on every
    /// other one, with an authentication error about a registry the manifest
    /// never mentioned a credential for. Naming a Secret makes the document
    /// self-contained, which is the whole point of GitOps.
    ///
    /// Only meaningful with `pull:` — a `build:` produces an image locally and
    /// authenticates nowhere.
    #[serde(
        default,
        rename = "pullSecret",
        skip_serializing_if = "Option::is_none"
    )]
    pull_secret: Option<String>,
}

/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: ImageSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

/// Field names accepted in the `spec` of `kind: Image`, for the unknown-field warning.
pub(crate) const IMAGE_SPEC_FIELDS: &[&str] = &["pull", "build", "pullSecret"];

/// Fields the reconciler compares for a `kind: Image`.
///
/// `ref` converges HOT — «converging» an image means fetching the ref, and
/// nothing is destroyed doing it. That is the opposite of every other Kind's
/// cold field, and it follows from what an image IS: shared, content-addressed
/// cache, not a resource with a lifecycle.
pub(crate) const RECONCILED_IMAGE_FIELDS: &[&str] = &["ref", "digest"];

/// The reference a `kind: Image` document is ABOUT.
///
/// **An image's identity is its ref, never `metadata.name`.** `presence()` used
/// to resolve the document name, so a `kind: Image` named `web-base` with
/// `pull: nginx:alpine` reported as absent even with `nginx:alpine` sitting in
/// the store — measured on this host: `image ls` listed it, the plan said
/// `+ Image/web-base`. Every `stack ls`/`describe`/`plan` was wrong for any
/// document whose name was not literally the tag.
pub(crate) fn image_ref(doc: &ManifestDoc) -> Option<String> {
    let spec: ImageSpec = manifest::spec_of(doc).ok()?;
    spec.pull.or_else(|| spec.build.map(|b| b.tag))
}

/// What the manifest declares, for the reconciler.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: ImageSpec = manifest::spec_of(doc)?;
    let mut f = std::collections::BTreeMap::new();
    // A BUILT image does not converge, and saying so is the honest answer.
    // There is no build cache: `apply` reruns the build and replaces the tag
    // every single time. Reporting `=` would be a lie, and reporting a change on
    // every plan would make `--detailed-exitcode` return 2 forever in any repo
    // that builds an image — breaking the drift gate for everyone else.
    let built = spec.build.is_some();
    if let Some(r) = image_ref(doc) {
        f.insert("ref".into(), r.clone());
        // A ref pinned by digest is the only case where the DESIRED digest is
        // knowable without asking a registry — and asking one would make
        // computing a plan a network round-trip. For a moving tag the plan
        // compares the ref alone; that a tag may have moved upstream is not
        // something a local plan can see, and pretending otherwise would be
        // worse than the gap.
        if let Some((_, digest)) = r.split_once("@sha256:") {
            f.insert("digest".into(), format!("sha256:{digest}"));
        }
    }
    Ok(super::reconcile::Desired {
        kind: "Image".into(),
        name: doc.metadata.name.clone(),
        fields: f,
        converges: !built,
        // Shared content: the same `nginx:alpine` backs every stack on the host.
        ownable: false,
    })
}

/// What is on the machine, for the reconciler — keyed by the DOCUMENT name (that
/// is what the plan matches on) but resolved by the REF.
pub(crate) fn actual(docs: &[ManifestDoc]) -> Result<Vec<super::reconcile::Actual>> {
    let (images, _) = open_stores()?;
    let mut out = Vec::new();
    for doc in manifest::of_kind(docs, "Image") {
        let Some(r) = image_ref(doc) else { continue };
        let Ok(img) = images.resolve(&r) else {
            continue;
        };
        let mut f = std::collections::BTreeMap::new();
        f.insert("ref".into(), r);
        f.insert("digest".into(), img.id.clone());
        out.push(super::reconcile::Actual {
            kind: "Image".into(),
            name: doc.metadata.name.clone(),
            fields: f,
            owner: None,
            last_applied: None,
        });
    }
    Ok(out)
}

/// Converges an image: fetch the ref. Nothing is destroyed — an image is cache.
pub(crate) fn converge(name: &str, diffs: &[super::reconcile::FieldDiff]) -> Result<()> {
    let (images, _) = open_stores()?;
    for d in diffs {
        match d.field.as_str() {
            "ref" | "digest" => {
                if let Some(r) = &d.to {
                    resolve_or_pull(&images, r)?;
                }
            }
            other => {
                return Err(Error::Invalid(format!(
                    "image/{name}: '{other}' does not converge hot — bug in \
                     `reconcile::hot_fields`"
                )))
            }
        }
    }
    Ok(())
}

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
    /// `id=<name>,src=<path>` entries — same as the CLI's repeatable `--secret`.
    #[serde(default)]
    secrets: Vec<String>,
    /// `linux/<arch>` — same as the CLI's `--platform`.
    #[serde(default)]
    platform: Option<String>,
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
        #[arg(long)]
        json: bool,
    },
    /// Pull an image from a registry. With `--vm`, no argument = the
    /// OFFICIAL Delonix golden VM image.
    Pull {
        image: Option<String>,
        /// Verify the cosign signature with this public key (PEM) AFTER the
        /// pull, and fail if it does not match. Without this, a pull is not
        /// authenticated beyond the registry's own digest.
        #[arg(value_hint = clap::ValueHint::FilePath, long, value_name = "PEM")]
        verify: Option<PathBuf>,
        /// (only with `--vm`) With no argument, pull the official
        /// NO-Kubernetes golden instead of the Kubernetes one.
        #[arg(long)]
        no_k8s: bool,
    },
    /// List the tags available in a remote OCI repository (only with `--vm`).
    ///
    /// With no argument, the OFFICIAL Delonix golden image repo.
    LsRemote {
        source: Option<String>,
        /// With no argument, list the official NO-Kubernetes golden's repo
        /// instead of the Kubernetes one.
        #[arg(long)]
        no_k8s: bool,
    },
    /// List local images.
    Ls {
        /// Output format: `table` (default) or `json` (ADR-0005). Works for both
        /// `image ls` and `image --vm ls`.
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: super::output::OutputFormat,
    },
    /// Human-readable detail of one or more images, `kubectl describe`-style.
    ///
    /// Tags/digest/size/layers + the OCI config:
    /// entrypoint/cmd/env/workdir. With `--vm`, describes golden VM images.
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
        #[arg(value_hint = clap::ValueHint::FilePath, value_name = "PEM")]
        key: PathBuf,
    },
    /// SBOM + CVE scan of an image.
    ///
    /// Reads the layers from the CAS, without running anything. Pulls the
    /// image if missing. See `--sbom`, `--fail-on`, `--update`.
    Scan {
        /// Image to scan (optional with `--update`).
        #[arg(add = ArgValueCandidates::new(super::complete::images))]
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
        /// Remove it even if a container still uses it.
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Remove unused images and the CAS blobs nobody references any more.
    ///
    /// By default only the DANGLING ones (no tag); `--all` also drops tagged
    /// images that no container uses. Containers and volumes are untouched —
    /// see `container prune` and `volumes prune`.
    Prune {
        /// Skip the confirmation prompt (REQUIRED when stdin is not a terminal).
        #[arg(short = 'f', long)]
        force: bool,
        /// Also remove unused images that DO have a tag (not just the dangling ones).
        #[arg(short, long)]
        all: bool,
    },
    /// Export an OCI runtime bundle (rootfs + config.json) for `runc`/`crun`.
    Export {
        #[arg(add = ArgValueCandidates::new(super::complete::images))]
        image: String,
        #[arg(value_hint = clap::ValueHint::DirPath)]
        dir: PathBuf,
    },
    /// Save an image to a portable archive (`docker save`'s counterpart).
    ///
    /// The way to move an image to another machine with no registry. The
    /// archive is an OCI layout WITH the legacy `manifest.json`, so `delonix
    /// image load`, `docker load`, `podman load` and `ctr images import` all
    /// read it.
    Save {
        #[arg(add = ArgValueCandidates::new(super::complete::images))]
        image: String,
        /// Destination file. Use `-o /dev/stdout` to pipe (e.g. into `gzip`).
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'o', long = "output", value_name = "FILE")]
        output: PathBuf,
    },
    /// Load an image from an archive (the counterpart of `save`).
    ///
    /// Reads archives produced by `delonix image save`, `docker save` or
    /// `podman save`.
    Load {
        /// Archive to read (`.tar`; a `.tar.gz` must be gunzipped first).
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'i', long = "input", value_name = "FILE")]
        input: PathBuf,
    },
    /// Apply the `kind: Image` documents of a manifest.
    ///
    /// `pull` is idempotent by reference; `build` rebuilds and replaces the
    /// tag on each apply.
    Apply {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Authenticate to an OCI registry.
    ///
    /// Stores the credentials in `<root>/auth.json`, docker/podman format.
    /// The password ALWAYS comes from stdin — never from an argument (it
    /// would end up in the shell history and in /proc).
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
    Logout {
        #[arg(add = ArgValueCandidates::new(super::complete::registries))]
        registry: String,
    },
    /// Golden VM images (`<root>/vm-images/`): ls/pull/push/build.
    /// Equivalent to `image --vm <cmd>` (old form, kept).
    Vm {
        #[command(subcommand)]
        action: VmSub,
    },
    /// Publish a local image to an OCI registry.
    ///
    /// Without `target`, publishes under the image's own reference. With
    /// `--vm`, `target` is required.
    Push {
        #[arg(add = ArgValueCandidates::new(super::complete::images))]
        name: String,
        target: Option<String>,
    },
    /// (only with `--vm`) Register an existing disk image under a name.
    Import(super::vmimage::ImportArgs),
    /// Convert a VM disk to the format another ecosystem imports (only with
    /// `--vm`).
    ///
    /// `qcow2`, `raw`, `vmdk`, `vdi`, `vhdx`, `vhd`.
    Convert {
        #[arg(add = ArgValueCandidates::new(super::complete::vm_images))]
        source: String,
        #[arg(long = "to", value_enum)]
        to: super::vmimage::ConvertFormat,
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'o', long = "output")]
        output: Option<PathBuf>,
        /// Compress the output. Only `qcow2` and `vmdk` can — refused for the
        /// others rather than handed to `qemu-img` to fail on.
        #[arg(long)]
        compress: bool,
    },
    /// (only with `--vm`) Scaffold a `VMfile` for building your own VM image.
    ///
    /// Writes a `VMfile` (and a cloud-init) for building your own image. The
    /// built-in alternative is the golden VM image (Ubuntu +
    /// kubeadm/kubelet/kubectl + `delonix-cri`), built by `image --vm build`
    /// with no `-f`.
    Init {
        /// Name to use in the scaffold (image tag, hostname, account).
        #[arg(default_value = "myimage")]
        name: String,
        /// Where to write it (default: the current directory).
        #[arg(value_hint = clap::ValueHint::DirPath, short = 'd', long)]
        dir: Option<PathBuf>,
        /// Overwrite an existing `VMfile`.
        #[arg(long)]
        force: bool,
    },
    /// (only with `--vm`) Build a VM image.
    ///
    /// The built-in golden recipe (Ubuntu + kubeadm/kubelet/kubectl +
    /// `delonix-cri`), or a `VMfile` of your own with `-f`.
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        /// Build from a `VMfile` instead of the built-in golden recipe.
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Build context — the directory `COPY` reads from.
        #[arg(value_hint = clap::ValueHint::DirPath, default_value = ".")]
        context: PathBuf,
        #[arg(long, value_enum, default_value = "ubuntu")]
        distro: Distro,
        #[arg(long, default_value = "26.04")]
        ubuntu_release: String,
        #[arg(long, default_value = "bookworm")]
        debian_release: String,
        #[arg(long, default_value = "9")]
        rocky_release: String,
        /// Fedora release AND build (e.g. `42-1.1`) — only with `--distro fedora`.
        #[arg(long, default_value = "42-1.1")]
        fedora_release: String,
        #[arg(long)]
        k8s_version: Option<String>,
        #[arg(long = "extra-package")]
        extra_packages: Vec<String>,
        #[arg(long = "extra-run")]
        extra_run: Vec<String>,
        #[arg(value_hint = clap::ValueHint::FilePath, long)]
        cri_bin: Option<PathBuf>,
        /// Do not compress the final qcow2 (larger, but with no decompression
        /// cost on backing-file reads at runtime).
        #[arg(long)]
        no_compress: bool,
        /// Give the guest network access during `RUN` — VMfile builds only.
        /// The golden recipe already decides this with `--offline`.
        #[arg(long)]
        network: bool,
        /// Fetch the k8s .deb packages on the HOST (verified) and install them with `dpkg` —
        /// the appliance runs without network. No DHCP/DNS needed in the guest.
        #[arg(long)]
        offline: bool,
        /// Build a golden image with NO Kubernetes — just `delonix` itself.
        #[arg(long)]
        no_k8s: bool,
        #[arg(value_hint = clap::ValueHint::FilePath, long)]
        delonix_bin: Option<PathBuf>,
    },
}

/// Subcommands of `image vm` — mirror `cmd::vmimage::VmImageCmd` 1:1.
// A CLI enum parsed once per invocation, not a hot path — the same
// justification the sibling command enums already carry.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum VmSub {
    /// Remove a local VM image (its disk and its metadata).
    ///
    /// **Refused while a VM still uses it**: a VM runs on a thin overlay whose
    /// backing file IS the image, so deleting it makes that VM permanently
    /// unreadable rather than freeing anything.
    Rm {
        /// Image name(s), as shown by `image vm ls`.
        #[arg(required = true)]
        names: Vec<String>,
        /// Remove it even while VMs back onto it — **those VMs stop being
        /// readable**.
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// List the local VM images.
    Ls {
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: super::output::OutputFormat,
    },
    /// Human-readable detail of one or more VM images, `kubectl describe`-style.
    Describe {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::vm_images))]
        names: Vec<String>,
    },
    /// Fetch a VM image from an OCI registry (single-blob artifact) — with
    /// no argument, the OFFICIAL Delonix image.
    Pull {
        source: Option<String>,
        /// Local name (default: derived from the reference).
        #[arg(long)]
        name: Option<String>,
        /// With no `source`, pull the official NO-Kubernetes golden instead
        /// of the Kubernetes one.
        #[arg(long)]
        no_k8s: bool,
    },
    /// List the tags available in a remote OCI repository.
    ///
    /// With no argument, the OFFICIAL Delonix golden image repo (discover
    /// which k8s versions are published before `pull`/`--k8s-version`).
    LsRemote {
        source: Option<String>,
        /// With no `source`, list the official NO-Kubernetes golden's repo
        /// instead of the Kubernetes one.
        #[arg(long)]
        no_k8s: bool,
    },
    /// Publish a local VM image to an OCI registry.
    ///
    /// Omit the destination to publish to the OFFICIAL repository the image
    /// belongs in.
    Push {
        #[arg(add = ArgValueCandidates::new(super::complete::vm_images))]
        name: String,
        target: Option<String>,
    },
    /// Register an existing disk image under a name, so `vm create --disk
    /// <name>` and `image vm push` can use it.
    Import(super::vmimage::ImportArgs),
    /// Convert a VM disk to the format another ecosystem imports.
    ///
    /// `qcow2`, `raw`, `vmdk` (VMware), `vdi` (VirtualBox), `vhdx`/`vhd`
    /// (Hyper-V, Azure). Flattened either way: the result is a standalone
    /// file.
    Convert {
        #[arg(add = ArgValueCandidates::new(super::complete::vm_images))]
        source: String,
        #[arg(long = "to", value_enum)]
        to: super::vmimage::ConvertFormat,
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'o', long = "output")]
        output: Option<PathBuf>,
        /// Compress the output. Only `qcow2` and `vmdk` can — refused for the
        /// others rather than handed to `qemu-img` to fail on.
        #[arg(long)]
        compress: bool,
    },
    /// Scaffold a `VMfile` (and a cloud-init) for building your own image.
    ///
    /// Build the golden VM image (Ubuntu + kubeadm/kubelet/kubectl +
    /// `delonix-cri`).
    Init {
        /// Name to use in the scaffold (image tag, hostname, account).
        #[arg(default_value = "myimage")]
        name: String,
        /// Where to write it (default: the current directory).
        #[arg(value_hint = clap::ValueHint::DirPath, short = 'd', long)]
        dir: Option<PathBuf>,
        /// Overwrite an existing `VMfile`.
        #[arg(long)]
        force: bool,
    },
    /// Build a VM image: the built-in golden recipe, or your own `VMfile`.
    ///
    /// The golden recipe is Ubuntu + kubeadm/kubelet/kubectl + `delonix-cri`;
    /// build a `VMfile` of your own with `-f`.
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        /// Build from a `VMfile` instead of the built-in golden recipe.
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Build context — the directory `COPY` reads from.
        #[arg(value_hint = clap::ValueHint::DirPath, default_value = ".")]
        context: PathBuf,
        #[arg(long, value_enum, default_value = "ubuntu")]
        distro: Distro,
        #[arg(long, default_value = "26.04")]
        ubuntu_release: String,
        #[arg(long, default_value = "bookworm")]
        debian_release: String,
        #[arg(long, default_value = "9")]
        rocky_release: String,
        /// Fedora release AND build (e.g. `42-1.1`) — only with `--distro fedora`.
        #[arg(long, default_value = "42-1.1")]
        fedora_release: String,
        #[arg(long)]
        k8s_version: Option<String>,
        #[arg(long = "extra-package")]
        extra_packages: Vec<String>,
        #[arg(long = "extra-run")]
        extra_run: Vec<String>,
        #[arg(value_hint = clap::ValueHint::FilePath, long)]
        cri_bin: Option<PathBuf>,
        /// Do not compress the final qcow2 (larger, but with no decompression
        /// cost on backing-file reads at runtime).
        #[arg(long)]
        no_compress: bool,
        /// Give the guest network access during `RUN` — VMfile builds only.
        /// The golden recipe already decides this with `--offline`.
        #[arg(long)]
        network: bool,
        /// Fetch the k8s .deb packages on the HOST (verified) and install them with `dpkg` —
        /// the appliance runs without network. No DHCP/DNS needed in the guest.
        #[arg(long)]
        offline: bool,
        /// Build a golden image with NO Kubernetes — just `delonix` itself.
        #[arg(long)]
        no_k8s: bool,
        #[arg(value_hint = clap::ValueHint::FilePath, long)]
        delonix_bin: Option<PathBuf>,
    },
}

/// `vm`: enables `--vm` in the `image` group — dispatches `ls`/`pull`/`push`/`build`
/// to `cmd::vmimage` (golden VM images) instead of `ImageStore` (container
/// images). `rm`/`export`/`apply` make no sense for VM images at this
/// stage — clear error instead of silently wrong behavior.
pub fn run(vm: bool, action: ImageCmd) -> Result<()> {
    // login/logout are agnostic to container-vs-VM (same auth.json).
    match &action {
        ImageCmd::Dash { once, json } => {
            return super::dash::run(super::dash::DashScope::Images, *once, *json);
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
            println!(
                "{}",
                super::po::tf(
                    "credentials for {registry} removed",
                    &[("registry", registry)]
                )
            );
            return Ok(());
        }
        _ => {}
    }
    if let ImageCmd::Vm { action } = action {
        use super::vmimage::{self, VmImageCmd};
        return vmimage::run(match action {
            VmSub::Rm { names, force } => VmImageCmd::Rm { names, force },
            VmSub::Ls { output } => VmImageCmd::Ls { output },
            VmSub::Describe { names } => VmImageCmd::Describe { names },
            VmSub::Pull {
                source,
                name,
                no_k8s,
            } => VmImageCmd::Pull {
                source,
                name,
                no_k8s,
            },
            VmSub::LsRemote { source, no_k8s } => VmImageCmd::LsRemote { source, no_k8s },
            VmSub::Push { name, target } => VmImageCmd::Push { name, target },
            VmSub::Import(args) => VmImageCmd::Import(args),
            VmSub::Convert {
                source,
                to,
                output,
                compress,
            } => VmImageCmd::Convert {
                source,
                to,
                output,
                compress,
            },
            VmSub::Init { name, dir, force } => VmImageCmd::Init { name, dir, force },
            VmSub::Build {
                tag,
                file,
                context,
                distro,
                ubuntu_release,
                debian_release,
                rocky_release,
                fedora_release,
                k8s_version,
                extra_packages,
                extra_run,
                cri_bin,
                no_compress,
                network,
                offline,
                no_k8s,
                delonix_bin,
            } => VmImageCmd::Build {
                tag,
                file,
                context,
                distro,
                ubuntu_release,
                debian_release,
                rocky_release,
                fedora_release,
                k8s_version,
                extra_packages,
                extra_run,
                cri_bin,
                no_compress,
                network,
                offline,
                no_k8s,
                delonix_bin,
            },
        });
    }
    if vm {
        return run_vm(action);
    }
    let (images, store) = open_stores()?;
    match action {
        ImageCmd::Dash { .. } => unreachable!("tratado no topo de run"),
        ImageCmd::Pull {
            image,
            verify,
            no_k8s: _,
        } => {
            // Unlike `--vm pull` (defaults to the official golden image), a
            // plain container-image pull has no sensible default — `image`
            // only became `Option<String>` so the SAME struct could serve
            // both paths at the clap level (see `run_vm`'s mapping). `no_k8s`
            // only matters in `--vm` mode too — ignored here.
            let image = image.ok_or_else(|| {
                Error::Invalid(super::po::t("`image pull <reference>`: the reference is required").into())
            })?;
            cmd_pull(&images, &image, verify.as_deref())
        }
        ImageCmd::LsRemote { .. } => Err(Error::Invalid(
            super::po::t(
                "`ls-remote` of container images is not supported yet — use `delonix image --vm ls-remote` for VM images",
            )
            .into(),
        )),
        ImageCmd::Ls { output } => cmd_ls(&images, output),
        ImageCmd::Describe { names } => cmd_describe(&images, &names),
        ImageCmd::Tag { source, target } => cmd_tag(&images, &source, &target),
        ImageCmd::History { image } => cmd_history(&images, &image),
        ImageCmd::Verify { image, key } => cmd_verify(&images, &image, &key),
        ImageCmd::Scan { image, sbom, fail_on, update, feed } => {
            if update {
                super::scan::cmd_scan_update(feed)
            } else {
                let image = image.ok_or_else(|| {
                    Error::Invalid(
                        super::po::t(
                            "specify the image to scan, or use `--update` to sync the feed",
                        )
                        .into(),
                    )
                })?;
                super::scan::cmd_scan(&image, sbom, fail_on.as_deref())
            }
        }
        ImageCmd::Rm { image, force } => cmd_rm(&images, &store, &image, force),
        ImageCmd::Prune { force, all } => cmd_prune(&images, &store, force, all),
        ImageCmd::Export { image, dir } => cmd_export(&images, &image, &dir),
        ImageCmd::Save { image, output } => cmd_save(&images, &image, &output),
        ImageCmd::Load { input } => cmd_load(&images, &input),
        ImageCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
        ImageCmd::Push { name, target } => cmd_push(&images, &name, target.as_deref()),
        ImageCmd::Convert { .. } => Err(Error::Invalid(
            super::po::t(
                "`convert` is only for VM images — use `delonix image --vm convert`",
            )
            .into(),
        )),
        // Container images come from `delonix image pull`/`build`/`load`; a
        // disk image is a different kind of thing entirely.
        ImageCmd::Import(_) => Err(Error::Invalid(
            super::po::t(
                "`import` is only for VM images — use `delonix image --vm import` (for a container image tarball, use `delonix image load`)",
            )
            .into(),
        )),
        // `init` scaffolds a VMfile, which only describes a VM image — the
        // container equivalent is `delonix build`'s Dockerfile/Delonixfile.
        ImageCmd::Init { .. } => Err(Error::Invalid(
            super::po::t(
                "`init` in this group is only for VM images — use `delonix vm init` (or `delonix image --vm init`)",
            )
            .into(),
        )),
        ImageCmd::Build { .. } => Err(Error::Invalid(
            super::po::t(
                "`build` in this group is only for VM images — use `delonix image --vm build`, or `delonix build` for container images",
            )
            .into(),
        )),
        ImageCmd::Login { .. } | ImageCmd::Logout { .. } | ImageCmd::Vm { .. } => unreachable!("tratados acima"),
    }
}

/// `image login` — reads the password from stdin (mandatory: an argument would end up
/// in the shell history and be visible in /proc) and delegates to `delonix_image::auth`.
fn cmd_login(registry: &str, username: &str, password_stdin: bool) -> Result<()> {
    if !password_stdin {
        return Err(Error::Invalid(
            super::po::t(
                "use --password-stdin (e.g.: `gh auth token | delonix image login ghcr.io -u USER --password-stdin`)",
            )
            .into(),
        ));
    }
    let mut pw = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut pw).map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::t("reading the password from stdin")
        ))
    })?;
    let pw = pw.trim();
    if pw.is_empty() {
        return Err(Error::Invalid(
            super::po::t("empty password on stdin").into(),
        ));
    }
    delonix_image::auth::login(&super::util::state_root(), registry, username, pw)?;
    println!(
        "{}",
        super::po::tf(
            "login to {registry} saved (auth.json)",
            &[("registry", registry)],
        )
    );
    Ok(())
}

fn run_vm(action: ImageCmd) -> Result<()> {
    use super::vmimage::{self, VmImageCmd};
    let mapped = match action {
        ImageCmd::Dash { .. } => unreachable!("tratado no topo de run"),
        ImageCmd::Ls { output } => VmImageCmd::Ls { output },
        ImageCmd::Describe { names } => VmImageCmd::Describe { names },
        ImageCmd::Init { name, dir, force } => VmImageCmd::Init { name, dir, force },
        // BUG FIXED HERE, found live on a real host: `delonix image --vm
        // pull` (no argument) should default to the official golden image,
        // same as `delonix vm pull` — this mapping used to pass `image`
        // through even when `None`, and `VmImageCmd::Pull` used to require a
        // `source`, so clap itself rejected the no-arg invocation before this
        // code ever ran. Both are now `Option<String>`; `vmimage::run`
        // applies the actual default.
        ImageCmd::Pull {
            image,
            verify: _,
            no_k8s,
        } => VmImageCmd::Pull {
            source: image,
            name: None,
            no_k8s,
        },
        ImageCmd::LsRemote { source, no_k8s } => VmImageCmd::LsRemote { source, no_k8s },
        ImageCmd::Push { name, target } => VmImageCmd::Push {
            name,
            // A VM image has no repo_tags from which to infer the destination.
            // No longer required: without it the official repository is
            // chosen from the image's own metadata (`official_repo_for`).
            target,
        },
        ImageCmd::Import(args) => VmImageCmd::Import(args),
        ImageCmd::Convert {
            source,
            to,
            output,
            compress,
        } => VmImageCmd::Convert {
            source,
            to,
            output,
            compress,
        },
        ImageCmd::Build {
            tag,
            file,
            context,
            distro,
            ubuntu_release,
            debian_release,
            rocky_release,
            fedora_release,
            k8s_version,
            extra_packages,
            extra_run,
            cri_bin,
            no_compress,
            network,
            offline,
            no_k8s,
            delonix_bin,
        } => VmImageCmd::Build {
            tag,
            file,
            context,
            distro,
            ubuntu_release,
            debian_release,
            rocky_release,
            fedora_release,
            k8s_version,
            extra_packages,
            extra_run,
            cri_bin,
            no_compress,
            network,
            offline,
            no_k8s,
            delonix_bin,
        },
        ImageCmd::Tag { .. }
        | ImageCmd::History { .. }
        | ImageCmd::Verify { .. }
        | ImageCmd::Scan { .. } => return Err(Error::Invalid(
            super::po::t(
                "tag/history/verify are for container images — they do not apply to VM images (--vm)",
            )
            .into(),
        )),
        ImageCmd::Rm { .. }
        | ImageCmd::Prune { .. }
        | ImageCmd::Export { .. }
        | ImageCmd::Save { .. }
        | ImageCmd::Load { .. }
        | ImageCmd::Apply { .. } => {
            return Err(Error::Invalid(
                super::po::t("command not available for VM images (--vm) — use ls/pull/push/build")
                    .into(),
            ))
        }
        ImageCmd::Login { .. } | ImageCmd::Logout { .. } | ImageCmd::Vm { .. } => {
            unreachable!("tratados em run()")
        }
    };
    vmimage::run(mapped)
}

/// `(username, password)` out of a `kind: Secret`, for a registry pull.
///
/// The key names are the ones a registry credential is universally called, and
/// a secret that has neither is refused NAMING what it has instead: the common
/// mistake is a secret built for something else (a `token`, a `password` with
/// no user) being pointed at a registry, and "unauthorized" from the far end
/// would send the reader looking at the registry rather than at the secret.
fn registry_creds_from_secret(image: &str, secret: &str) -> Result<(String, String)> {
    let store = delonix_runtime_core::SecretStore::open(super::util::state_root())?;
    let s = store.load(secret)?;
    let user = s
        .data
        .get("username")
        .or_else(|| s.data.get("user"))
        .cloned();
    let pass = s
        .data
        .get("password")
        .or_else(|| s.data.get("token"))
        .cloned();
    match (user, pass) {
        (Some(u), Some(p)) => Ok((u, p)),
        _ => {
            let mut have: Vec<&str> = s.data.keys().map(String::as_str).collect();
            have.sort_unstable();
            Err(Error::Invalid(super::po::tf(
                "Image '{image}': secret '{secret}' needs `username` and `password` (it has: {have})",
                &[("image", image), ("secret", secret), ("have", &have.join(", "))],
            )))
        }
    }
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (images, _store) = open_stores()?;
    for doc in manifest::of_kind(docs, "Image") {
        let name = &doc.metadata.name;
        manifest::warn_unknown_fields(doc, IMAGE_SPEC_FIELDS);
        let spec: ImageSpec = manifest::spec_of(doc)?;
        // WHERE the field applies is checked BEFORE the secret is resolved. The
        // other order produced the wrong error: a `pullSecret` on a `build:`
        // reported that the secret lacked `username`/`password`, sending the
        // reader to fix a secret when the problem is a field in the wrong
        // place. Refusing beats ignoring — the author believes the build is
        // authenticating somewhere, and it is not.
        if spec.pull_secret.is_some() && spec.pull.is_none() {
            return Err(Error::Invalid(super::po::tf(
                "Image '{name}': pullSecret only applies to `pull:` — a `build:` produces the image locally and authenticates nowhere",
                &[("name", name)],
            )));
        }
        let creds = match &spec.pull_secret {
            Some(sref) => Some(registry_creds_from_secret(name, sref)?),
            None => None,
        };
        match (spec.pull, spec.build) {
            (Some(reference), None) => {
                super::util::resolve_or_pull_with_creds(&images, &reference, creds)?;
                println!(
                    "{}",
                    super::po::tf(
                        "image/{name}: ensured ({reference})",
                        &[("name", name), ("reference", &reference)],
                    )
                );
            }
            (None, Some(b)) => {
                let file = b
                    .file
                    .unwrap_or_else(|| super::build::default_build_file(&b.context));
                let build_args = super::build::parse_build_args(&b.build_args);
                let secrets = super::build::parse_build_secrets(&b.secrets)?;
                let platform = b
                    .platform
                    .as_deref()
                    .map(super::build::parse_platform)
                    .transpose()?;
                let img = super::build::build_from_spec(
                    &b.context,
                    &file,
                    &b.tag,
                    &build_args,
                    !b.no_cache,
                    &secrets,
                    platform.as_deref(),
                )?;
                println!(
                    "image/{name}: {} ({})",
                    super::po::t("built"),
                    img.short_id()
                );
            }
            (Some(_), Some(_)) => {
                return Err(Error::Invalid(super::po::tf(
                    "image/{name}: spec has BOTH `pull` AND `build` — only one of the two",
                    &[("name", name)],
                )))
            }
            (None, None) => {
                return Err(Error::Invalid(super::po::tf(
                    "image/{name}: spec has neither `pull` nor `build`",
                    &[("name", name)],
                )))
            }
        }
    }
    Ok(())
}

fn cmd_pull(images: &ImageStore, reference: &str, verify: Option<&std::path::Path>) -> Result<()> {
    // Per-layer progress bar (`docker pull`-style) — BUG FOUND live: a
    // multi-hundred-MB image gave no feedback at all beyond one log line at
    // the very start, looked hung. `last` tracks (layer, bytes) so a
    // transition to a NEW layer closes the previous bar's line, without
    // depending on the registry sending a Content-Length (chunked transfers
    // may not) to know when one layer ends and the next begins.
    // `Mutex` and not `Cell`: the pull runs the layers in PARALLEL and every
    // worker reports through here, so the callback has to be `Sync`. The lock
    // is not a hot path — it is taken once per progress tick (every 2 MiB), not
    // per byte.
    let last = std::sync::Mutex::new((0usize, 0u64));
    let on_progress = move |layer: usize, layer_total: usize, done: u64, total: Option<u64>| {
        let (last_layer, last_done) = *last.lock().unwrap();
        if last_layer != 0 && last_layer != layer {
            super::output::progress_done();
        }
        let advanced = last_layer != layer || done.saturating_sub(last_done) >= 2 * 1024 * 1024;
        let finished = total.map(|t| done >= t).unwrap_or(false);
        if advanced || finished {
            *last.lock().unwrap() = (layer, done);
            super::output::progress_bar(
                &format!("[pull] layer {layer}/{layer_total}"),
                done,
                total,
            );
        }
    };
    let img = delonix_image::registry::pull_from_registry_with_creds_full(
        images,
        reference,
        None,
        None,
        Some(&on_progress),
    )?;
    super::output::progress_done();
    // Verify AFTER the pull (the cosign signature lives in a tag alongside the
    // image in the registry, so we need it here). If it fails, the command fails —
    // the image stays local, but whoever asked for `--verify` knows it is untrusted.
    if let Some(key) = verify {
        let pem = std::fs::read_to_string(key)?;
        let digest = delonix_image::verify_signature(images, reference, &pem)?;
        println!(
            "{}",
            super::po::tf(
                "valid signature for {reference} ({digest})",
                &[("reference", reference), ("digest", &digest)],
            )
        );
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
    println!(
        "{}",
        super::po::tf(
            "OK: valid signature for {image} ({digest})",
            &[("image", image), ("digest", &digest)],
        )
    );
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
pub(crate) fn image_size(images: &ImageStore, img: &delonix_image::Image) -> Option<u64> {
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

/// `image ls -o json` row (ADR-0005): all tags (not just the first), full id,
/// numeric `created_unix`/`size_bytes` (`size_bytes` null when unmeasurable).
#[derive(serde::Serialize)]
struct ImageLsRow {
    repo_tags: Vec<String>,
    id: String,
    created_unix: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
}

fn cmd_ls(images: &ImageStore, format: super::output::OutputFormat) -> Result<()> {
    let mut imgs = images.list()?;
    // Newest first, as in `docker images`.
    imgs.sort_by_key(|i| std::cmp::Reverse(i.created_unix));
    if format == super::output::OutputFormat::Json {
        let rows: Vec<ImageLsRow> = imgs
            .iter()
            .map(|img| ImageLsRow {
                repo_tags: img.repo_tags.clone(),
                id: img.id.clone(),
                created_unix: img.created_unix,
                size_bytes: image_size(images, img),
            })
            .collect();
        return super::output::print_json(&rows);
    }
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

/// `image prune` — the image half of `system prune`, on its own.
///
/// The reason it exists is the concrete one an SRE hits: wanting the disk back
/// from images alone meant running the global prune, which ALSO removes every
/// stopped container. Same sweep as `system prune` (`prune::sweep_images`), so
/// "unused" cannot come to mean two different things depending on which command
/// you typed.
fn cmd_prune(
    images: &ImageStore,
    store: &delonix_runtime_core::Store,
    force: bool,
    all: bool,
) -> Result<()> {
    // The preview only fires for `--all`, and it earns its place: the default
    // takes only untagged leftovers, while `--all` takes images someone pulled
    // on purpose and merely is not running right now.
    let preview = all.then(|| {
        super::po::t("With --all this ALSO removes TAGGED images that no container uses.")
            .to_string()
    });
    if !super::prune::confirm(
        force,
        super::po::t(
            "`image prune` removes unused images and unreferenced blobs — pass --force to confirm \
             when not on a terminal",
        ),
        preview,
        super::po::t("Removes unused images and the CAS blobs nobody references. Continue? [y/N]"),
    )? {
        return Ok(());
    }
    let i = super::prune::sweep_images(images, store, all)?;
    println!(
        "{}",
        super::po::tf(
            "removed: {i} image(s), {b} blob(s) — {size} freed",
            &[
                ("i", &i.images.to_string()),
                ("b", &i.blobs.to_string()),
                ("size", &i.freed.fmt()),
            ]
        )
    );
    Ok(())
}

/// `image rm` — refuses while a container still references the image.
///
/// Docker refuses this ("image is being used by ... container"); this used to
/// delete it unconditionally and report success. The running container keeps
/// working (its rootfs is already materialized), so nothing looks wrong — but the
/// workload can no longer be recreated or scaled, and on an air-gapped node or
/// after the upstream tag moves, that image is simply gone. A latent outage that
/// only surfaces at the worst moment is exactly the class of silent failure this
/// engine's own invariant forbids.
fn cmd_rm(
    images: &ImageStore,
    store: &delonix_runtime_core::Store,
    reference: &str,
    force: bool,
) -> Result<()> {
    if !force {
        let img = images.resolve(reference)?;
        let mut users: Vec<String> = Vec::new();
        for c in store.list()? {
            // A record points at the image either by id or by any of its tags —
            // `run` stores whatever reference the user typed.
            if c.image == img.id
                || delonix_image::cas::strip(&c.image) == delonix_image::cas::strip(&img.id)
                || img.repo_tags.contains(&c.image)
            {
                let alive = c.pid.map(delonix_runtime::is_alive).unwrap_or(false);
                let state = if alive {
                    super::po::t("running")
                } else {
                    super::po::t("stopped")
                };
                users.push(format!("{} ({state})", c.name));
            }
        }
        if !users.is_empty() {
            return Err(Error::Invalid(super::po::tf(
 "image '{ref}' is in use by container(s): {list} — remove them first, or pass --force (the image can then no longer be used to recreate them)",
                &[("ref", reference), ("list", &users.join(", "))],
            )));
        }
    }
    let removed = images.remove(reference)?;
    delonix_runtime_core::events::emit(
        &super::util::state_root(),
        "image",
        "remove",
        &removed,
        reference,
        if force { Some("force") } else { None },
    );
    println!("{removed}");
    Ok(())
}

/// Writes a minimal OCI runtime bundle (rootfs + config.json) for `runc`/`crun`.
/// `image save` — the counterpart of `docker save`/`podman save`, and what makes
/// a registry-free deploy to ANOTHER machine possible (build here, `save`, copy,
/// `load` there). The archive is deliberately readable by all four consumers that
/// matter: `delonix image load`, `docker load`, `podman load` and `ctr images
/// import` (see [`delonix_image::write_oci_archive`]).
///
/// The reference is written into the archive VERBATIM (not the store's first
/// tag): an image can carry several tags, and loading under a name the caller
/// never asked for is how a deploy ends up pinning the wrong thing.
fn cmd_save(images: &ImageStore, reference: &str, output: &std::path::Path) -> Result<()> {
    let img = images.resolve(reference)?;
    let ref_name = delonix_image::image::normalise_tag(reference);
    delonix_image::write_oci_archive(images, &img, &ref_name, output)?;
    // stdout is a legitimate destination (`-o /dev/stdout | gzip`) — reporting to
    // stdout there would corrupt the archive. All progress goes to stderr.
    eprintln!(
        "{}",
        super::po::tf(
            "{ref}: saved to {path}",
            &[("ref", &ref_name), ("path", &output.display().to_string())],
        )
    );
    Ok(())
}

/// `image load` — imports an archive into the local store. Accepts what
/// `docker save`/`podman save`/`delonix image save` produce (the legacy
/// `manifest.json` layout).
fn cmd_load(images: &ImageStore, input: &std::path::Path) -> Result<()> {
    let img = delonix_image::load_docker_archive(images, input)?;
    let tags = if img.repo_tags.is_empty() {
        img.short_id()
    } else {
        img.repo_tags.join(", ")
    };
    println!("{}", super::po::tf("loaded: {tags}", &[("tags", &tags)]));
    Ok(())
}

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
        .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("serializing OCI spec"))))?;
    std::fs::write(&cfg, json).map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::tf("writing {path}", &[("path", &cfg.display().to_string())])
        ))
    })?;
    println!(
        "{}",
        super::po::tf(
            "OCI bundle at {dir}",
            &[("dir", &dir.display().to_string())],
        )
    );
    println!(
        "{}",
        super::po::tf(
            "run with:  runc run -b {dir} delonix-oci",
            &[("dir", &dir.display().to_string())],
        )
    );
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
