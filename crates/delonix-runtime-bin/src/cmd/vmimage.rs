//! `delonix image --vm` — golden VM images (Ubuntu + kubeadm/kubelet/
//! kubectl + `delonix-cri`), managed separately from container images (those
//! live in `cmd::image`/`ImageStore`). One standalone `.qcow2` per image (no
//! CAS/layers — there is only one blob per image, nothing to deduplicate) + a
//! `.json` of metadata, both under `<root>/vm-images/`.
//!
//! `build` produces the image from scratch (download of the Ubuntu cloud
//! image plus `virt-customize`); `push`/`pull` publish/fetch it from an OCI
//! registry (a single-blob artifact, see
//! `delonix_image::registry::{push_oci_artifact, pull_oci_artifact}`) — the
//! same protocol as container images, only without the Docker layers/config
//! model.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Subcommand;
use delonix_runtime_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::output::{self, fmt_local, fmt_size};
use super::util::state_root;

const VM_IMAGE_MEDIA_TYPE: &str = "application/vnd.delonix.vmimage.v1.qcow2";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmImage {
    pub name: String,
    pub tag: String,
    pub digest: String,
    pub size: u64,
    pub ubuntu_release: Option<String>,
    pub k8s_version: Option<String>,
    pub created_unix: u64,
    /// The Linux kernel release string (`uname -r` shape, e.g.
    /// `6.8.0-31-generic`) baked into the image — read back via `virt-cat`
    /// right after `virt-customize` (see `cmd_build`), never booted to find
    /// out. `None` for images built before this field existed, or `vm pull`ed
    /// (same known gap as `ubuntu_release`/`k8s_version` — the OCI artifact
    /// only carries the qcow2 blob, not build metadata).
    #[serde(default)]
    pub kernel_version: Option<String>,
    /// `"ubuntu"` | `"debian"` — added in v0.17.0 alongside multi-distro
    /// support. `#[serde(default)]` so pre-existing on-disk metadata (all
    /// Ubuntu, built before this field existed) still loads; `None` there is
    /// treated as `"ubuntu"` for display purposes (`distro_label`), never
    /// re-written on disk. `ubuntu_release` keeps its field NAME for
    /// backward-compat with existing `.json` files (no `#[serde(default)]`
    /// on it — a rename would break loading them) but now holds the release
    /// identifier for WHATEVER distro this is (a Debian codename like
    /// `bookworm`, not just an Ubuntu release number).
    #[serde(default)]
    pub distro: Option<String>,
}

/// Base distro for a golden image build. `Ubuntu` stays the default (no
/// behavior change for existing callers); `Debian` is additive.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Distro {
    Ubuntu,
    Debian,
    Rocky,
}

impl Distro {
    fn as_str(self) -> &'static str {
        match self {
            Distro::Ubuntu => "ubuntu",
            Distro::Debian => "debian",
            Distro::Rocky => "rocky",
        }
    }
}

/// `"ubuntu/24.04"`-style label for `ls`/`describe`. Pre-v0.17.0 metadata has
/// no `distro` field — displayed as just the release, matching what those
/// images actually showed before this column existed (never silently
/// mislabeled as Debian, which `None` would otherwise risk).
fn distro_label(img: &VmImage) -> String {
    match (img.distro.as_deref(), img.ubuntu_release.as_deref()) {
        (Some(d), Some(r)) => format!("{d}/{r}"),
        (Some(d), None) => d.to_string(),
        (None, Some(r)) => r.to_string(),
        (None, None) => "-".to_string(),
    }
}

pub struct VmImageStore {
    root: PathBuf,
}

impl VmImageStore {
    pub fn open(base: impl Into<PathBuf>) -> Result<Self> {
        let root = base.into().join("vm-images");
        std::fs::create_dir_all(root.join("_base"))?;
        Ok(Self { root })
    }

    fn sanitize(name: &str) -> String {
        name.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn meta_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{}.json", Self::sanitize(name)))
    }

    pub fn qcow2_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{}.qcow2", Self::sanitize(name)))
    }

    pub fn base_cache_path(&self, distro: Distro, release: &str) -> PathBuf {
        // `sanitize` (not applied here before — security-audit finding, see
        // AGENTS.md) strips `/` from `release`, preventing
        // `--ubuntu-release '../../../etc/cron.d/x'` from writing outside `_base/`.
        let filename = match distro {
            Distro::Ubuntu => format!(
                "ubuntu-{}-server-cloudimg-amd64.img",
                Self::sanitize(release)
            ),
            Distro::Debian => format!(
                "debian-{}-genericcloud-amd64.qcow2",
                Self::sanitize(release)
            ),
            Distro::Rocky => format!("rocky-{}-genericcloud-amd64.qcow2", Self::sanitize(release)),
        };
        self.root.join("_base").join(filename)
    }

    pub fn save(&self, img: &VmImage) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(img)?;
        std::fs::write(self.meta_path(&img.name), bytes)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<VmImage>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root)?.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(bytes) = std::fs::read(&p) {
                    if let Ok(img) = serde_json::from_slice::<VmImage>(&bytes) {
                        out.push(img);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn get(&self, name: &str) -> Result<VmImage> {
        let bytes = std::fs::read(self.meta_path(name))
            .map_err(|_| Error::NotFound(format!("imagem VM '{name}'")))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[derive(Subcommand)]
pub enum VmImageCmd {
    /// List the local VM images.
    Ls {
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: output::OutputFormat,
    },
    /// Human-readable detail of one or more VM images, `kubectl describe` style.
    Describe { names: Vec<String> },
    /// Publish a local VM image to an OCI registry (single-blob artifact).
    Push { name: String, target: String },
    /// Pull a VM image from an OCI registry — with no argument, the OFFICIAL
    /// Delonix image (ready for `vm create`/`cluster kubeadm`).
    Pull {
        source: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// With no `source`, pull the official NO-Kubernetes golden (just the
        /// `delonix` engine, rootless-ready) instead of the Kubernetes one.
        #[arg(long)]
        no_k8s: bool,
    },
    /// List the tags available in a remote OCI repository — with no
    /// argument, the OFFICIAL Delonix golden image repo (discover which
    /// k8s versions are published before `pull`/`--k8s-version`).
    LsRemote {
        source: Option<String>,
        /// With no `source`, list the official NO-Kubernetes golden's repo
        /// instead of the Kubernetes one.
        #[arg(long)]
        no_k8s: bool,
    },
    /// Build the golden image: Ubuntu cloud image + kubeadm/kubelet/kubectl
    /// + `delonix-cri` (CRI endpoint for the kubelet), via `virt-customize`.
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        /// Base distro for the cloud image.
        #[arg(long, value_enum, default_value = "ubuntu")]
        distro: Distro,
        #[arg(long, default_value = "26.04")]
        ubuntu_release: String,
        /// Debian codename (`bookworm`, `trixie`, ...) — only used with `--distro debian`.
        #[arg(long, default_value = "bookworm")]
        debian_release: String,
        /// Rocky Linux major version (`8`, `9`, `10`) — only used with `--distro
        /// rocky`. Rocky currently only supports `--no-k8s` builds.
        #[arg(long, default_value = "9")]
        rocky_release: String,
        /// Kubernetes version (e.g. `1.31`) — omit to use the latest stable.
        #[arg(long)]
        k8s_version: Option<String>,
        /// Extra apt package, repeatable — extensibility without touching the code.
        #[arg(long = "extra-package")]
        extra_packages: Vec<String>,
        /// Extra command to run inside the guest during the build, repeatable.
        #[arg(long = "extra-run")]
        extra_run: Vec<String>,
        /// Explicit path of the `delonix-cri` binary to install (otherwise:
        /// looks next to the current `delonix`, then tries to build from the
        /// workspace if a `Cargo.toml` is detected from the cwd).
        #[arg(long)]
        cri_bin: Option<PathBuf>,
        /// Do not compress the final qcow2 (larger, but no decompression cost
        /// on backing-file reads at runtime).
        #[arg(long)]
        no_compress: bool,
        /// Fetch the k8s .deb files on the HOST (verified: InRelease signature +
        /// SHA256) and install them with `dpkg` — the appliance runs without
        /// network (`--no-network`). Dispenses with DHCP/DNS in the guest, so it
        /// dispenses with the host workarounds (passt/dhclient) the online mode requires.
        #[arg(long)]
        offline: bool,
        /// Build a golden image with NO Kubernetes at all — just the
        /// `delonix` engine binary, ready for rootless containers (mutually
        /// exclusive with `--k8s-version`/`--offline`, which don't apply).
        #[arg(long)]
        no_k8s: bool,
        /// Explicit path of the `delonix` binary to install when `--no-k8s`
        /// (otherwise: the currently running `delonix`, then a workspace
        /// build, then a verified download from the matching release).
        #[arg(long)]
        delonix_bin: Option<PathBuf>,
    },
}

pub fn run(action: VmImageCmd) -> Result<()> {
    let store = VmImageStore::open(state_root())?;
    match action {
        VmImageCmd::Ls { output } => cmd_ls(&store, output),
        VmImageCmd::Describe { names } => cmd_describe(&store, &names),
        VmImageCmd::Push { name, target } => cmd_push(&store, &name, &target),
        VmImageCmd::Pull {
            source,
            name,
            no_k8s,
        } => {
            // BUG FIXED HERE, found live: this is the shared engine command
            // behind BOTH `image --vm pull` AND `image vm pull` — it never
            // got the "no argument = official image" default that `delonix
            // vm pull` (a separate, sibling CLI definition in `cmd/vm.rs`)
            // already has, despite this exact struct's own doc comment
            // claiming it. A user on a real host hit this: `delonix image vm
            // pull --name delonix-vm-k8s:1.34` (no source) errored "required
            // arguments were not provided: <SOURCE>".
            let src = source.unwrap_or_else(|| default_pull_source(no_k8s).to_string());
            cmd_pull(&store, &src, name)
        }
        VmImageCmd::LsRemote { source, no_k8s } => {
            let src = source.unwrap_or_else(|| default_pull_source(no_k8s).to_string());
            cmd_ls_remote(&src)
        }
        VmImageCmd::Build {
            tag,
            distro,
            ubuntu_release,
            debian_release,
            rocky_release,
            k8s_version,
            extra_packages,
            extra_run,
            cri_bin,
            no_compress,
            offline,
            no_k8s,
            delonix_bin,
        } => cmd_build(
            &store,
            &tag,
            distro,
            &ubuntu_release,
            &debian_release,
            &rocky_release,
            k8s_version,
            extra_packages,
            extra_run,
            cri_bin,
            !no_compress,
            offline,
            no_k8s,
            delonix_bin,
        ),
    }
}

/// `image --vm ls -o json` / `image vm ls -o json` row (ADR-0005): machine-friendly
/// values (`created_unix`/`size_bytes` as numbers; nullable kernel/k8s).
#[derive(serde::Serialize)]
struct VmImageLsRow {
    name: String,
    distro: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kernel_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    k8s_version: Option<String>,
    created_unix: u64,
    size_bytes: u64,
}

fn cmd_ls(store: &VmImageStore, format: output::OutputFormat) -> Result<()> {
    if format == output::OutputFormat::Json {
        let rows: Vec<VmImageLsRow> = store
            .list()?
            .into_iter()
            .map(|img| VmImageLsRow {
                distro: distro_label(&img),
                name: img.name,
                kernel_version: img.kernel_version,
                k8s_version: img.k8s_version,
                created_unix: img.created_unix,
                size_bytes: img.size,
            })
            .collect();
        return output::print_json(&rows);
    }
    let mut t =
        output::Table::new(&["NAME", "DISTRO", "KERNEL", "K8S", "CREATED", "SIZE"]).right_align(5);
    for img in store.list()? {
        let distro = distro_label(&img);
        t.row(vec![
            img.name,
            distro,
            img.kernel_version.as_deref().unwrap_or("-").to_string(),
            img.k8s_version.as_deref().unwrap_or("-").to_string(),
            fmt_local(img.created_unix),
            fmt_size(img.size),
        ]);
    }
    t.print();
    Ok(())
}

/// `image --vm describe` — human-readable detail, `kubectl describe` style.
fn cmd_describe(store: &VmImageStore, names: &[String]) -> Result<()> {
    for (i, name) in names.iter().enumerate() {
        let img = store.get(name)?;
        if i > 0 {
            println!();
        }
        describe_one(store, &img);
    }
    Ok(())
}

fn describe_one(store: &VmImageStore, img: &VmImage) {
    let mut d = output::Describe::new();
    d.field("Name", &img.name);
    d.field("Tag", &img.tag);
    d.field("Digest", &img.digest);
    d.field("Size", fmt_size(img.size));
    d.field("Created", fmt_local(img.created_unix));
    d.field("Age", output::fmt_age(img.created_unix));
    // `pull` does NOT recover this metadata (the OCI artifact only carries the
    // qcow2 blob) — on a pulled image they stay `None`. See the known gap in AGENTS.md.
    let distro = distro_label(img);
    d.field("Distro", if distro == "-" { "<unknown>" } else { &distro });
    d.field(
        "Kernel",
        img.kernel_version.as_deref().unwrap_or("<unknown>"),
    );
    d.field("K8s", img.k8s_version.as_deref().unwrap_or("<unknown>"));
    let qcow2 = store.qcow2_path(&img.name);
    d.field("Path", qcow2.to_string_lossy());
    // The `size` above is the build/pull one; this is what IS on disk now. If
    // they diverge, the artifact was tampered with out-of-band — worth being able to see.
    d.field_opt(
        "On disk",
        std::fs::metadata(&qcow2).ok().map(|m| fmt_size(m.len())),
    );
    d.print();
}

/// Delonix's OFFICIAL golden VM image (Ubuntu 24.04 + kubeadm/kubelet/
/// kubectl + delonix-cri as a systemd service) — published and validated with
/// a byte-identical round-trip; see AGENTS.md, section "Golden VM image".
pub(crate) const OFFICIAL_VM_IMAGE: &str = "ghcr.io/angolardevops/delonix-vm-k8s:1.34";

/// Golden VM image with NO Kubernetes — just the `delonix` engine binary and
/// rootless prerequisites (see `rootless_customization_steps`). Selected by
/// `Pull`/`LsRemote --no-k8s` when no explicit `source` is given.
pub(crate) const OFFICIAL_VM_BASE_IMAGE: &str =
    "ghcr.io/angolardevops/delonix-vm-base:ubuntu-24.04";

/// Picks the default source for `Pull`/`LsRemote` when no explicit `source`
/// is given. BUG FIXED (gap): `OFFICIAL_VM_BASE_IMAGE` existed but had no way
/// to be selected without typing out the full `ghcr.io/...:ubuntu-24.04`
/// reference by hand — a tenant who only wants the no-Kubernetes golden had
/// no discoverable "just give me the official one" path, unlike the
/// Kubernetes golden (bare `pull`/`ls-remote`).
pub(crate) fn default_pull_source(no_k8s: bool) -> &'static str {
    if no_k8s {
        OFFICIAL_VM_BASE_IMAGE
    } else {
        OFFICIAL_VM_IMAGE
    }
}

pub(crate) fn cmd_push(store: &VmImageStore, name: &str, target: &str) -> Result<()> {
    let img = store.get(name)?;
    let data = std::fs::read(store.qcow2_path(name)).map_err(|e| {
        Error::Invalid(format!(
            "{} '{name}': {e}",
            super::po::t("could not read the qcow2 of")
        ))
    })?;
    let digest = delonix_image::registry::push_oci_artifact(
        &state_root(),
        target,
        VM_IMAGE_MEDIA_TYPE,
        &data,
    )?;
    println!("{digest}");
    let _ = img;
    Ok(())
}

pub(crate) fn cmd_pull(store: &VmImageStore, source: &str, name: Option<String>) -> Result<()> {
    // Download progress bar (the golden is hundreds of MB): the engine
    // reports (bytes, total) every 64KB; we redraw at most every ~2MB
    // so as not to hammer the terminal. Only draws on a tty (see `output`).
    let label = format!("[vm pull] {source}");
    let last = std::cell::Cell::new(0u64);
    let on_progress = move |done: u64, total: Option<u64>| {
        let finished = total.map(|t| done >= t).unwrap_or(false);
        if finished || done.wrapping_sub(last.get()) >= 2 * 1024 * 1024 {
            last.set(done);
            super::output::progress_bar(&label, done, total);
        }
    };
    let data = delonix_image::registry::pull_oci_artifact_with_progress(
        &state_root(),
        source,
        Some(&on_progress),
    )?;
    super::output::progress_done();
    let name = name.unwrap_or_else(|| source.rsplit('/').next().unwrap_or(source).to_string());
    let digest = format!("sha256:{}", hex_sha256(&data));
    std::fs::write(store.qcow2_path(&name), &data)?;
    let img = VmImage {
        name: name.clone(),
        tag: source.to_string(),
        digest,
        size: data.len() as u64,
        ubuntu_release: None,
        k8s_version: None,
        created_unix: now_unix(),
        kernel_version: None,
        distro: None,
    };
    store.save(&img)?;
    println!("{name}");
    Ok(())
}

pub(crate) fn cmd_ls_remote(source: &str) -> Result<()> {
    let mut tags = delonix_image::registry::list_remote_tags(&state_root(), source)?;
    tags.sort();
    // BUG FOUND live: printed one bare tag per line with no header — looked
    // uncomparable next to every other `ls`-shaped command in this CLI, which
    // all go through `output::Table` (same convention `image ls`/`vm ls`/
    // `network ls` already use).
    let mut t = output::Table::new(&["TAG"]);
    for tag in tags.iter_mut() {
        t.row(vec![std::mem::take(tag)]);
    }
    t.print();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_build(
    store: &VmImageStore,
    tag: &str,
    distro: Distro,
    ubuntu_release: &str,
    debian_release: &str,
    rocky_release: &str,
    k8s_version: Option<String>,
    extra_packages: Vec<String>,
    extra_run: Vec<String>,
    cri_bin: Option<PathBuf>,
    compress: bool,
    offline: bool,
    no_k8s: bool,
    delonix_bin: Option<PathBuf>,
) -> Result<()> {
    // `--no-k8s` builds a completely different image (no kubeadm/kubelet/
    // kubectl/delonix-cri at all — see `rootless_customization_steps`); the
    // k8s-only flags don't apply and silently ignoring them would be
    // confusing, so reject the combination up front.
    if no_k8s {
        if k8s_version.is_some() {
            return Err(Error::Invalid(
                super::po::t("--no-k8s and --k8s-version are mutually exclusive").into(),
            ));
        }
        if offline {
            return Err(Error::Invalid(
                super::po::t(
                    "--no-k8s does not support --offline (offline mode only knows how to verify k8s .deb)",
                )
                .into(),
            ));
        }
        if cri_bin.is_some() {
            return Err(Error::Invalid(
                super::po::t("--no-k8s and --cri-bin are mutually exclusive (use --delonix-bin)")
                    .into(),
            ));
        }
    } else if delonix_bin.is_some() {
        return Err(Error::Invalid(
            super::po::t("--delonix-bin only applies with --no-k8s").into(),
        ));
    }
    // Rocky's dnf-family customization steps only exist for the `--no-k8s`
    // path (`rootless_customization_steps`) — `k8s_recipes` is apt-only
    // (pkgs.k8s.io's RPM repo has a different URL/GPG scheme, not
    // implemented). Fail closed rather than silently running apt commands
    // against a dnf guest.
    if distro == Distro::Rocky && !no_k8s {
        return Err(Error::Invalid(
            super::po::t(
                "--distro rocky only supports --no-k8s for now (the k8s path needs the \
                 pkgs.k8s.io RPM repository, not implemented yet)",
            )
            .into(),
        ));
    }
    // `k8s_version` goes into a `format!` that becomes a `virt-customize --run-command`
    // (via `k8s_recipes::k8s_host_recipes`) — validating here closes the same security
    // finding as `cmd::cluster::valid_version` (the embedded apt repository must not
    // contain shell metacharacters). Audit finding, see AGENTS.md.
    if let Some(v) = &k8s_version {
        if !super::cluster::valid_version(v) {
            return Err(Error::Invalid(super::po::tf(
                "--k8s-version '{v}' invalid (only digits and dots, e.g.: '1.31')",
                &[("v", v)],
            )));
        }
    }
    let release = match distro {
        Distro::Ubuntu => ubuntu_release,
        Distro::Debian => debian_release,
        Distro::Rocky => rocky_release,
    };
    let base = match distro {
        Distro::Ubuntu => download_ubuntu_base(store, ubuntu_release)?,
        Distro::Debian => download_debian_base(store, debian_release)?,
        Distro::Rocky => download_rocky_base(store, rocky_release)?,
    };

    let work_dir =
        std::env::temp_dir().join(format!("delonix-vmimage-build-{}", std::process::id()));
    std::fs::create_dir_all(&work_dir)?;
    let work_qcow2 = work_dir.join("work.qcow2");

    eprintln!(
        "{}",
        super::po::t("preparing the working image (flattened, no backing file)...")
    );
    run_tool(
        "qemu-img",
        &[
            "convert",
            "-O",
            "qcow2",
            &base.to_string_lossy(),
            &work_qcow2.to_string_lossy(),
        ],
    )?;

    let ops = if no_k8s {
        eprintln!(
            "{}",
            super::po::t(
                "--no-k8s mode: preparing image without Kubernetes (delonix + rootless)..."
            )
        );
        let delonix = resolve_delonix_bin(delonix_bin)?;
        rootless_customization_steps(&extra_run, &delonix, distro)
    } else {
        let cri = resolve_cri_bin(cri_bin)?;
        let service_unit = workspace_dist_file("delonix-cri.service")?;
        if offline {
            // Everything that needs network happens HERE, on the host (verified), so the
            // appliance can run with `--no-network`.
            eprintln!(
                "{}",
                super::po::t("offline mode: getting the k8s .deb on the host...")
            );
            let debs = download_k8s_debs(
                &work_dir,
                &work_dir.join("debs"),
                k8s_version.as_deref(),
                "amd64",
                &extra_packages,
            )?;
            // Best-effort: the golden image ships with kubeadm's own core images
            // already local, so a real `kubeadm init` on a booted VM does not
            // redownload apiserver/etcd/coredns/... from scratch (see
            // `preseed_k8s_images` for the real crash this fixes). Needs the
            // kubeadm `.deb` we just downloaded/verified on the host.
            let preseed_root = debs
                .iter()
                .find(|d| {
                    d.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("kubeadm_"))
                })
                .and_then(|kubeadm_deb| {
                    eprintln!(
                        "{}",
                        super::po::t("pre-seeding the kubeadm images on the host...")
                    );
                    preseed_k8s_images(&work_dir, kubeadm_deb, k8s_version.as_deref())
                });
            k8s_customization_steps_offline(
                &debs,
                &extra_run,
                &cri,
                &service_unit,
                preseed_root.as_deref(),
                distro,
            )
        } else {
            k8s_customization_steps(
                k8s_version.as_deref(),
                &extra_packages,
                &extra_run,
                &cri,
                &service_unit,
                distro,
            )
        }
    };
    let mut args = customize_args(&work_qcow2, &ops);
    if offline {
        // Without this, libguestfs starts passt and the appliance waits for a DHCP
        // lease that never arrives on hosts where passt is broken (see AGENTS.md).
        args.insert(0, "--no-network".to_string());
    }

    eprintln!(
        "{}",
        super::po::tf(
            "running virt-customize ({n} step(s){net})...",
            &[
                ("n", &ops.len().to_string()),
                ("net", if offline { ", no network" } else { "" }),
            ],
        )
    );
    run_tool(
        "virt-customize",
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;

    // Read back the kernel version the customize steps recorded (see the
    // `/etc/delonix-kernel-version` step in `common_customization_steps`) —
    // `virt-cat` pulls a single file out of a disk image without booting it.
    // Best-effort: a missing/unreadable file just leaves the column blank,
    // never fails the build over a "nice to have" metadata field.
    let kernel_version = std::process::Command::new("virt-cat")
        .args([
            "-a",
            &work_qcow2.to_string_lossy(),
            "/etc/delonix-kernel-version",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty() && s != "unknown");

    // Shrink the artifact. Measured on a 24.04 golden (2.38 GiB → 677 MiB, −72%):
    //  1) `virt-sparsify --in-place` — zeroes the blocks already freed (the apt
    //     cleanup above frees ~367 MiB that, without this, still occupy the qcow2).
    //  2) `qemu-img convert -c` — the Ubuntu cloud image COMES compressed and the
    //     initial `convert` (above, without `-c`) decompresses it; without this step
    //     the final artifact is ~4x larger than the base. `zstd` instead of the
    //     default zlib: compresses 5x faster (10s vs 53s), ends up smaller, and above
    //     all DECOMPRESSES much faster — it matters because this image is used as the
    //     read-only backing file of the VMs (`delonix_vm::create` makes an overlay per
    //     VM), so every read of the base OS goes through the decompressor.
    // Sparsify is best-effort: if it fails, we carry on (only some size is lost).
    let final_qcow2 = if compress {
        eprintln!(
            "{}",
            super::po::t("compacting the image (sparsify + zstd compression)...")
        );
        if let Err(e) = run_tool(
            "virt-sparsify",
            &["--in-place", &work_qcow2.to_string_lossy()],
        ) {
            eprintln!(
                "{} {}",
                super::po::t("warning:"),
                super::po::tf(
                    "virt-sparsify failed ({err}); compressing anyway",
                    &[("err", &e.to_string())]
                )
            );
        }
        let compressed = work_dir.join("final.qcow2");
        run_tool(
            "qemu-img",
            &[
                "convert",
                "-c",
                "-O",
                "qcow2",
                "-o",
                "compression_type=zstd",
                &work_qcow2.to_string_lossy(),
                &compressed.to_string_lossy(),
            ],
        )?;
        compressed
    } else {
        work_qcow2
    };

    let data = std::fs::read(&final_qcow2)?;
    let digest = format!("sha256:{}", hex_sha256(&data));
    let size = data.len() as u64;
    std::fs::rename(&final_qcow2, store.qcow2_path(tag))
        .or_else(|_| std::fs::copy(&final_qcow2, store.qcow2_path(tag)).map(|_| ()))?;
    let _ = std::fs::remove_dir_all(&work_dir);

    let img = VmImage {
        name: tag.to_string(),
        tag: tag.to_string(),
        digest,
        size,
        ubuntu_release: Some(release.to_string()),
        k8s_version,
        created_unix: now_unix(),
        kernel_version,
        distro: Some(distro.as_str().to_string()),
    };
    store.save(&img)?;
    println!("{tag}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Download + verification of the Ubuntu cloud image
// ---------------------------------------------------------------------------

fn download_ubuntu_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
    let cached = store.base_cache_path(Distro::Ubuntu, release);
    if cached.exists() {
        return Ok(cached);
    }
    let base_url = format!("https://cloud-images.ubuntu.com/releases/{release}/release");
    let img_name = format!("ubuntu-{release}-server-cloudimg-amd64.img");
    let img_url = format!("{base_url}/{img_name}");
    let sums_url = format!("{base_url}/SHA256SUMS");

    eprintln!(
        "{}",
        super::po::tf("downloading {url}...", &[("url", &img_url)])
    );
    let tmp = cached.with_extension("download");
    stream_download(&img_url, &tmp)?;

    eprintln!("{}", super::po::t("verifying SHA256SUMS..."));
    let sums = http_get_text(&sums_url)?;
    let expected = sums
        .lines()
        .find(|l| l.trim_end().ends_with(&img_name))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| {
            Error::Invalid(format!(
                "{} {img_name}",
                super::po::t("SHA256SUMS has no entry for")
            ))
        })?
        .to_string();
    let got = hex_sha256_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for {img_name}: expected {expected}, got {got} — download discarded",
            &[
                ("img_name", &img_name),
                ("expected", &expected),
                ("got", &got),
            ],
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    Ok(cached)
}

// ---------------------------------------------------------------------------
// Download + verification of the Debian cloud image
// ---------------------------------------------------------------------------

/// Debian's cloud image directory is keyed by CODENAME (`bookworm`), but the
/// filename embeds the MAJOR VERSION NUMBER (`debian-12-...`) — confirmed
/// live against `cloud.debian.org` (no numeric-only directory alias exists,
/// so this mapping can't be derived from the codename string alone). Fails
/// closed on an unknown codename rather than guessing a number.
fn debian_major_version(codename: &str) -> Result<&'static str> {
    match codename {
        "bullseye" => Ok("11"),
        "bookworm" => Ok("12"),
        "trixie" => Ok("13"),
        _ => Err(Error::Invalid(super::po::tf(
            "--debian-release '{codename}' unknown (bullseye|bookworm|trixie)",
            &[("codename", codename)],
        ))),
    }
}

/// Same shape as `download_ubuntu_base`, two confirmed differences (checked
/// live against `cloud.debian.org` before writing this, not assumed): (1) the
/// path is `images/cloud/<codename>/latest/`, filename
/// `debian-<major>-genericcloud-amd64.qcow2` (the `genericcloud` variant —
/// virtio-only kernel, smaller, still has cloud-init — not the `generic`
/// variant, which also ships legacy drivers this project never needs); (2)
/// Debian publishes `SHA512SUMS`, NOT `SHA256SUMS` (no SHA256 checksums file
/// exists at all) — same `<hash>  <filename>` line format, different hash
/// algorithm, hence `hex_sha512_file` below.
fn download_debian_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
    let cached = store.base_cache_path(Distro::Debian, release);
    if cached.exists() {
        return Ok(cached);
    }
    let major = debian_major_version(release)?;
    let base_url = format!("https://cloud.debian.org/images/cloud/{release}/latest");
    let img_name = format!("debian-{major}-genericcloud-amd64.qcow2");
    let img_url = format!("{base_url}/{img_name}");
    let sums_url = format!("{base_url}/SHA512SUMS");

    eprintln!(
        "{}",
        super::po::tf("downloading {url}...", &[("url", &img_url)])
    );
    let tmp = cached.with_extension("download");
    stream_download(&img_url, &tmp)?;

    eprintln!("{}", super::po::t("verifying SHA512SUMS..."));
    let sums = http_get_text(&sums_url)?;
    let expected = sums
        .lines()
        .find(|l| l.trim_end().ends_with(&img_name))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| {
            Error::Invalid(format!(
                "{} {img_name}",
                super::po::t("SHA512SUMS has no entry for")
            ))
        })?
        .to_string();
    let got = hex_sha512_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for {img_name}: expected {expected}, got {got} — download discarded",
            &[
                ("img_name", &img_name),
                ("expected", &expected),
                ("got", &got),
            ],
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    Ok(cached)
}

// ---------------------------------------------------------------------------
// Download + verification of the Rocky Linux cloud image
// ---------------------------------------------------------------------------

/// Rocky's release directory AND filename both use the plain major version
/// number (`9`, not a codename) — simpler than Debian, confirmed live
/// against `dl.rockylinux.org` (8/9/10 all exist today). Whitelisted rather
/// than accepted verbatim purely for a fast, clear error before any network
/// call — an unknown value would otherwise still fail safely (a 404 from
/// `stream_download`), this is just better UX, not a security boundary
/// (unlike Debian's codename→number mapping, which IS load-bearing: there is
/// no way to derive the filename's number from an arbitrary codename).
fn valid_rocky_release(release: &str) -> Result<()> {
    if matches!(release, "8" | "9" | "10") {
        Ok(())
    } else {
        Err(Error::Invalid(super::po::tf(
            "--rocky-release '{release}' unknown (8|9|10)",
            &[("release", release)],
        )))
    }
}

/// Same shape as `download_ubuntu_base`/`download_debian_base`, confirmed
/// live before writing code: the image is `Rocky-<release>-GenericCloud.
/// latest.x86_64.qcow2` under `pub/rocky/<release>/images/x86_64/` (no
/// `images/cloud/` segment — a different tree shape than Debian's). The
/// checksum sidecar is PER-FILE (`<img>.CHECKSUM`, not a directory-wide
/// `SUMS` file) and uses the BSD `SHA256 (<filename>) = <hash>` shape — a
/// THIRD checksum format in this module, after Ubuntu/Debian's GNU
/// `<hash>  <filename>` — hence `parse_bsd_checksum` below. SHA256 (not
/// SHA512 like Debian), so
/// `hex_sha256_file` is reused as-is.
fn download_rocky_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
    valid_rocky_release(release)?;
    let cached = store.base_cache_path(Distro::Rocky, release);
    if cached.exists() {
        return Ok(cached);
    }
    let img_name = format!("Rocky-{release}-GenericCloud.latest.x86_64.qcow2");
    let img_url = format!("https://dl.rockylinux.org/pub/rocky/{release}/images/x86_64/{img_name}");
    let sums_url = format!("{img_url}.CHECKSUM");

    eprintln!(
        "{}",
        super::po::tf("downloading {url}...", &[("url", &img_url)])
    );
    let tmp = cached.with_extension("download");
    stream_download(&img_url, &tmp)?;

    eprintln!("{}", super::po::t("verifying CHECKSUM..."));
    let sums = http_get_text(&sums_url)?;
    let expected = parse_bsd_checksum(&sums, &img_name).ok_or_else(|| {
        Error::Invalid(format!(
            "{} {img_name}",
            super::po::t("CHECKSUM has no entry for")
        ))
    })?;
    let got = hex_sha256_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for {img_name}: expected {expected}, got {got} — download discarded",
            &[
                ("img_name", &img_name),
                ("expected", &expected),
                ("got", &got),
            ],
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    Ok(cached)
}

/// Rocky's `.CHECKSUM` uses the BSD `SHA256 (<filename>) = <hash>` line
/// shape — confirmed live, different from both `<hash>  <filename>` forms
/// used elsewhere in this module. Pure/tested. Matches the exact filename
/// (not just any `SHA256 (...)` line) as defense-in-depth against a sidecar
/// that happens to list more than one file.
fn parse_bsd_checksum(text: &str, filename: &str) -> Option<String> {
    let prefix = format!("SHA256 ({filename}) = ");
    text.lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .map(|s| s.trim().to_string())
}

pub(crate) fn stream_download(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(std::time::Duration::from_secs(3600))
        .build()
        .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("HTTP client"))))?;
    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| Error::Invalid(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Invalid(format!("GET {url}: HTTP {}", resp.status())));
    }
    let mut file = std::fs::File::create(dest)?;
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("reading response"))))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
    }
    Ok(())
}

pub(crate) fn http_get_text(url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("HTTP client"))))?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| Error::Invalid(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Invalid(format!("GET {url}: HTTP {}", resp.status())));
    }
    resp.text().map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::tf("body of {url}", &[("url", url)])
        ))
    })
}

// ---------------------------------------------------------------------------
// OFFLINE build: download+verify the k8s .deb files ON THE HOST
// ---------------------------------------------------------------------------
// This way `virt-customize` runs with `--no-network` and the appliance never
// needs DHCP/DNS — which removes the host workarounds (passt/dhclient) that the
// online path requires. The chain of trust is the SAME as apt's, only done
// here instead of inside the guest:
//   InRelease (clearsigned, verified with the repo's Release.key)
//     → SHA256 of `Packages`  → SHA256 of each `.deb`
// A file is never accepted without the previous step having authenticated it — the
// same principle as CRITICAL finding nº3 of the audit (`pull_oci_artifact` without digest).

/// A `.deb` from the `pkgs.k8s.io` repo, already resolved from an authenticated `Packages`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct K8sDeb {
    pub name: String,
    pub version: String,
    /// Path relative to the repo root (the `Filename` field).
    pub filename: String,
    pub sha256: String,
}

/// Parses a `Packages` index (Debian control, blocks separated by a blank
/// line) and returns, per package in `wanted`, the HIGHEST version available for
/// `arch`. PURE function (testable without network).
///
/// `version_prefix` (e.g. "1.34.") only applies to the `versioned` packages — the
/// components that follow the Kubernetes version (kubeadm/kubelet/kubectl). The
/// rest of the repo has its OWN versioning (`kubernetes-cni` is 1.7.x,
/// `cri-tools` is 1.34.x but independent) and take only "the most recent": filtering
/// them by the k8s prefix returned nothing.
pub(crate) fn parse_packages_index(
    index: &str,
    arch: &str,
    version_prefix: &str,
    wanted: &[&str],
    versioned: &[&str],
) -> Vec<K8sDeb> {
    let mut best: std::collections::BTreeMap<String, K8sDeb> = Default::default();
    for block in index.split("\n\n") {
        let mut f: std::collections::HashMap<&str, &str> = Default::default();
        for line in block.lines() {
            if let Some((k, v)) = line.split_once(": ") {
                f.insert(k.trim(), v.trim());
            }
        }
        let (Some(name), Some(version), Some(filename), Some(sha), Some(a)) = (
            f.get("Package"),
            f.get("Version"),
            f.get("Filename"),
            f.get("SHA256"),
            f.get("Architecture"),
        ) else {
            continue;
        };
        if *a != arch {
            continue;
        }
        if !wanted.is_empty() && !wanted.contains(name) {
            continue;
        }
        // The k8s prefix only applies to those that follow the k8s version.
        if versioned.contains(name) && !version.starts_with(version_prefix) {
            continue;
        }
        let cand = K8sDeb {
            name: name.to_string(),
            version: version.to_string(),
            filename: filename.to_string(),
            sha256: sha.to_string(),
        };
        best.entry(name.to_string())
            .and_modify(|cur| {
                if deb_version_lt(&cur.version, &cand.version) {
                    *cur = cand.clone();
                }
            })
            .or_insert(cand);
    }
    best.into_values().collect()
}

/// Compares two Debian versions well enough for the k8s repo
/// (`1.34.9-1.1`): compares numerically the fields separated by `.`/`-`.
/// It is not dpkg's full algorithm — the repo only uses versions of this form, and a
/// tie/unexpected format degrades to lexicographic comparison.
pub(crate) fn deb_version_lt(a: &str, b: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> {
        s.split(['.', '-'])
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
            })
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (pa, pb) = (parts(a), parts(b));
    match pa.cmp(&pb) {
        std::cmp::Ordering::Equal => a < b,
        o => o == std::cmp::Ordering::Less,
    }
}

/// Extracts from an authenticated `Release` the expected SHA256 of a file
/// (e.g. "Packages"). The indexes come in the `SHA256:` section as
/// `<sha>  <size>  <path>`. PURE function.
pub(crate) fn release_sha256_of(release: &str, want_path: &str) -> Option<String> {
    let mut in_sha = false;
    for line in release.lines() {
        if line.starts_with("SHA256:") {
            in_sha = true;
            continue;
        }
        // another top-level (non-indented) section ends the SHA256 block.
        if in_sha && !line.starts_with(' ') {
            in_sha = false;
        }
        if !in_sha {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if let [sha, _size, path] = cols[..] {
            if path == want_path {
                return Some(sha.to_string());
            }
        }
    }
    None
}

/// Verifies the `InRelease` (clearsigned) with the repo's `Release.key` and returns
/// the ALREADY AUTHENTICATED body. Uses `gpgv` with a temporary keyring — never touches
/// the user's keyring. Fails closed: without a valid signature, there is no build.
fn verify_inrelease(work: &Path, repo_base: &str) -> Result<String> {
    let key_armored = http_get_text(&format!("{repo_base}/Release.key"))?;
    let key_asc = work.join("k8s-release.asc");
    let keyring = work.join("k8s-release.gpg");
    std::fs::write(&key_asc, &key_armored)?;
    // ASCII-armored → binary keyring that gpgv understands.
    run_tool(
        "gpg",
        &[
            "--batch",
            "--yes",
            "--no-default-keyring",
            "--dearmor",
            "-o",
            &keyring.to_string_lossy(),
            &key_asc.to_string_lossy(),
        ],
    )
    .map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::t("preparing the k8s repo keyring")
        ))
    })?;

    let inrelease = work.join("InRelease");
    stream_download(&format!("{repo_base}/InRelease"), &inrelease)?;
    run_tool(
        "gpgv",
        &[
            "--keyring",
            &keyring.to_string_lossy(),
            &inrelease.to_string_lossy(),
        ],
    )
    .map_err(|_| {
        Error::Invalid(
            super::po::t(
                "the k8s repo's InRelease signature does NOT match the Release.key — aborting \
                 (possible compromised repo or MITM)",
            )
            .to_string(),
        )
    })?;
    Ok(std::fs::read_to_string(&inrelease)?)
}

/// Downloads to `dest_dir` the k8s `.deb` files (repo closure: kubeadm/kubelet/
/// kubectl + `kubernetes-cni`), with the full apt chain verified on the host.
/// Returns the local paths. `arch` is the Debian architecture (e.g. "amd64").
fn download_k8s_debs(
    work: &Path,
    dest_dir: &Path,
    k8s_version: Option<&str>,
    arch: &str,
    extra_packages: &[String],
) -> Result<Vec<PathBuf>> {
    let repo = super::k8s_recipes::k8s_repo_version(k8s_version);
    let repo_base = format!("https://pkgs.k8s.io/core:/{repo}/deb");
    std::fs::create_dir_all(dest_dir)?;

    eprintln!(
        "{}",
        super::po::tf(
            "verifying the k8s repo signature ({repo})...",
            &[("repo", &repo)]
        )
    );
    let release = verify_inrelease(work, &repo_base)?;

    // `Packages` authenticated by the SHA256 listed in the signed InRelease.
    let want_sha = release_sha256_of(&release, "Packages").ok_or_else(|| {
        Error::Invalid(
            super::po::t("the k8s repo InRelease does not declare the SHA256 of 'Packages'")
                .to_string(),
        )
    })?;
    let packages_path = work.join("Packages");
    stream_download(&format!("{repo_base}/Packages"), &packages_path)?;
    let got = hex_sha256_file(&packages_path)?;
    if got != want_sha {
        return Err(Error::Invalid(super::po::tf(
            "SHA256 of the Packages index does not match (expected {expected}, got {got}) — aborting",
            &[
                ("expected", &want_sha[..16.min(want_sha.len())]),
                ("got", &got[..16.min(got.len())]),
            ],
        )));
    }
    let index = std::fs::read_to_string(&packages_path)?;

    // Closure: the 3 requested + `kubernetes-cni` (kubelet dep inside the repo).
    // The remaining kubelet deps (iptables/mount/util-linux/libc6) already come in
    // the Ubuntu cloud image — if any is missing, `dpkg -i` fails LOUDLY in the guest,
    // which is what we want (never install half-installed silently).
    // `versioned` follow the k8s version (`--k8s-version 1.34` → `1.34.*`);
    // `kubernetes-cni` has its own versioning → only "the most recent".
    const VERSIONED: [&str; 3] = ["kubeadm", "kubelet", "kubectl"];
    let mut wanted: Vec<&str> = vec!["kubeadm", "kubelet", "kubectl", "kubernetes-cni"];
    for p in extra_packages {
        wanted.push(p.as_str());
    }
    let version_prefix = match k8s_version {
        Some(v) if v != "stable" => format!("{v}."),
        _ => String::new(),
    };
    let debs = parse_packages_index(&index, arch, &version_prefix, &wanted, &VERSIONED);
    for base in ["kubeadm", "kubelet", "kubectl", "kubernetes-cni"] {
        if !debs.iter().any(|d| d.name == base) {
            return Err(Error::Invalid(super::po::tf(
                "the k8s repo ({repo}) does not have '{base}' for {arch} — nonexistent version?",
                &[("repo", &repo), ("base", base), ("arch", arch)],
            )));
        }
    }

    let mut out = Vec::new();
    for d in &debs {
        let file_name = d.filename.rsplit('/').next().unwrap_or(&d.filename);
        let dest = dest_dir.join(file_name);
        eprintln!("  {} {} ({arch})", d.name, d.version);
        stream_download(&format!("{repo_base}/{}", d.filename), &dest)?;
        let got = hex_sha256_file(&dest)?;
        if got != d.sha256 {
            let _ = std::fs::remove_file(&dest);
            return Err(Error::Invalid(super::po::tf(
                "SHA256 of {file_name} does not match (expected {expected}, got {got}) — aborting",
                &[
                    ("file_name", file_name),
                    ("expected", &d.sha256[..16.min(d.sha256.len())]),
                    ("got", &got[..16.min(got.len())]),
                ],
            )));
        }
        out.push(dest);
    }
    Ok(out)
}

/// Pre-seeds the golden image's own `ImageStore` (`/var/lib/delonix`, what
/// `delonix-cri` reads at runtime) with the exact container images
/// `kubeadm init`/`join` need for this k8s version — fetched+verified on the
/// HOST (same offline philosophy as the `.deb` packages: the appliance
/// itself never needs network for this), injected via
/// `virt-customize --copy-in`.
///
/// Real bug this closes (host kaeso-sys-01): `kubeadm init` redownloaded
/// every core image (apiserver/controller-manager/scheduler/etcd/coredns/
/// pause) fresh on EVERY VM boot, slow enough to blow past kubeadm's own
/// internal rate-limiter deadline and crash the bootstrap. This alone is not
/// sufficient — it depends on the CAS-first fix in
/// `delonix_image::registry::pull_from_registry_with_creds` (skip a blob
/// already on disk) to actually pay off at runtime; without that fix,
/// `delonix-cri` would still re-download every blob regardless of what is
/// pre-seeded here.
///
/// Best-effort end to end: returns `None` (never fails the whole build) if
/// `kubeadm config images list` or the pre-seed step does not succeed, and
/// logs (does not fail) a per-image pull error — a slower first boot beats a
/// broken one.
fn preseed_k8s_images(
    work: &Path,
    kubeadm_deb: &Path,
    k8s_version: Option<&str>,
) -> Option<PathBuf> {
    let extract_dir = work.join("kubeadm-host");
    let status = Command::new("dpkg-deb")
        .args([
            "-x",
            &kubeadm_deb.to_string_lossy(),
            &extract_dir.to_string_lossy(),
        ])
        .status()
        .ok()?;
    if !status.success() {
        eprintln!("warning: dpkg-deb -x kubeadm.deb failed — skipping image pre-seed");
        return None;
    }
    let kubeadm_bin = extract_dir.join("usr/bin/kubeadm");
    if !kubeadm_bin.exists() {
        eprintln!("warning: kubeadm binary not found after extraction — skipping image pre-seed");
        return None;
    }

    let mut cmd = Command::new(&kubeadm_bin);
    cmd.arg("config").arg("images").arg("list");
    if let Some(v) = k8s_version {
        cmd.arg(format!("--kubernetes-version=v{v}"));
    }
    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("warning: `kubeadm config images list` failed — skipping image pre-seed");
            return None;
        }
    };
    let images: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if images.is_empty() {
        return None;
    }

    let preseed_root = work.join("preseed-images");
    let store = delonix_image::ImageStore::open(&preseed_root).ok()?;
    for img in &images {
        eprintln!("  pre-seeding {img}...");
        if let Err(e) = delonix_image::registry::pull_from_registry_with_creds(&store, img, None) {
            eprintln!(
                "warning: could not pre-seed {img}: {e} (kubeadm will fetch it at runtime instead)"
            );
        }
    }
    Some(preseed_root)
}

fn hex_sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finalize())
}

pub(crate) fn hex_sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// Debian's cloud images only publish `SHA512SUMS` (no `SHA256SUMS` at all —
/// confirmed live) — same streaming-hash shape as `hex_sha256_file`, `Sha512`
/// instead of `Sha256` (already in the `sha2` crate, no new dependency).
fn hex_sha512_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha512::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Resolution of the `delonix-cri` binary to install in the guest
// ---------------------------------------------------------------------------

pub(crate) fn resolve_cri_bin(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.exists() {
            return Err(Error::Invalid(super::po::tf(
                "--cri-bin '{path}' does not exist",
                &[("path", &p.display().to_string())],
            )));
        }
        return Ok(p);
    }
    // Next to the current `delonix` (normal install, release).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("delonix-cri");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    // Dev convenience: source-code workspace from the cwd.
    if let Some(workspace_root) = find_workspace_root() {
        eprintln!(
            "{}",
            super::po::tf(
                "compiling delonix-cri (release) from {dir}...",
                &[("dir", &workspace_root.display().to_string())],
            )
        );
        let status = Command::new("cargo")
            .args([
                "build",
                "--release",
                "-p",
                "delonix-cri",
                "--bin",
                "delonix-cri",
            ])
            .current_dir(&workspace_root)
            .status()
            .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("running cargo build"))))?;
        if !status.success() {
            return Err(Error::Invalid(
                super::po::t("cargo build of delonix-cri failed").into(),
            ));
        }
        let built = workspace_root.join("target/release/delonix-cri");
        if built.exists() {
            return Ok(built);
        }
    }
    // BUG FIXED HERE, found live: a user who installed via `install.sh`
    // WITHOUT `--with-cri` (the default) and isn't running from a source
    // checkout had no way forward — `cluster kubeadm` needs `delonix-cri` to
    // install on every provisioned host, and none of the checks above ever
    // find one. `delonix-cri` is published as its own release asset
    // alongside `delonix` (same tag, always released together) — download it
    // (verified against the release's own SHA256SUMS, same as `install.sh`
    // would with `--with-cri`) instead of giving up.
    download_cri_bin().map_err(|e| {
        Error::Invalid(format!(
            "{e} — or use --cri-bin <path> / run from a source checkout"
        ))
    })
}

/// `true` when the CPU has AVX2+BMI2+FMA — the same 3-feature check
/// `install.sh` uses to pick the `-v3` release asset (Zen 2+/Haswell+).
pub(crate) fn cpu_has_x86_64_v3() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("avx2")
            && is_x86_feature_detected!("bmi2")
            && is_x86_feature_detected!("fma")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// `<root>/bin/<running-version>/` — where a resolved-without-a-checkout
/// `delonix-cri` (binary and/or service unit) is cached, namespaced by the
/// RUNNING `delonix` binary's own version so an upgrade never serves a
/// stale copy from a previous install.
fn cri_cache_dir() -> PathBuf {
    state_root().join("bin").join(env!("CARGO_PKG_VERSION"))
}

/// Downloads (and caches) `delonix-cri` from the GitHub release matching the
/// RUNNING `delonix` binary's own version — the two are always released
/// together, same tag. Verified against the release's own SHA256SUMS, same
/// non-negotiable as every other download in this codebase (never installs
/// an unverified binary). Cached under `<root>/bin/<version>/delonix-cri`,
/// so this only ever downloads once per installed version.
fn download_cri_bin() -> Result<PathBuf> {
    let version = env!("CARGO_PKG_VERSION");
    let cache_dir = cri_cache_dir();
    let cached = cache_dir.join("delonix-cri");
    if cached.exists() {
        return Ok(cached);
    }
    std::fs::create_dir_all(&cache_dir)?;
    let base_url =
        format!("https://github.com/angolardevops/delonix-runtime/releases/download/v{version}");
    let tmp = cache_dir.join("delonix-cri.download");
    let variant = if cpu_has_x86_64_v3() { "-v3" } else { "" };
    let mut asset = format!("delonix-cri-x86_64{variant}-linux");
    eprintln!(
        "{}",
        super::po::tf(
            "downloading {asset} (v{version})...",
            &[("asset", &asset), ("version", version)],
        )
    );
    if !variant.is_empty() && stream_download(&format!("{base_url}/{asset}"), &tmp).is_err() {
        asset = "delonix-cri-x86_64-linux".to_string();
        eprintln!(
            "{}",
            super::po::tf(
                "{asset} missing from this release — trying the generic binary...",
                &[("asset", &asset)],
            )
        );
        stream_download(&format!("{base_url}/{asset}"), &tmp)?;
    } else if variant.is_empty() {
        stream_download(&format!("{base_url}/{asset}"), &tmp)?;
    }
    let sums = http_get_text(&format!("{base_url}/SHA256SUMS"))?;
    let expected = sums
        .lines()
        .find(|l| l.trim_end().ends_with(&asset))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| Error::Invalid(format!("SHA256SUMS has no entry for {asset}")))?
        .to_string();
    let got = hex_sha256_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for {asset}: expected {expected}, got {got} — download discarded",
            &[("asset", &asset), ("expected", &expected), ("got", &got)],
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cached, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(cached)
}

/// Mirrors `resolve_cri_bin` for the `delonix` engine binary itself (used by
/// `--no-k8s` golden images). Simpler than the CRI case for tier 2: this
/// command is already running AS `delonix`, so `current_exe()` IS a valid
/// `delonix` binary — no "next to the exe" lookup needed.
pub(crate) fn resolve_delonix_bin(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.exists() {
            return Err(Error::Invalid(super::po::tf(
                "--delonix-bin '{path}' does not exist",
                &[("path", &p.display().to_string())],
            )));
        }
        return Ok(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if exe.exists() {
            return Ok(exe);
        }
    }
    // Dev convenience: source-code workspace from the cwd.
    if let Some(workspace_root) = find_workspace_root() {
        eprintln!(
            "{}",
            super::po::tf(
                "compiling delonix (release) from {dir}...",
                &[("dir", &workspace_root.display().to_string())],
            )
        );
        let status = Command::new("cargo")
            .args([
                "build",
                "--release",
                "-p",
                "delonix-runtime-bin",
                "--bin",
                "delonix",
            ])
            .current_dir(&workspace_root)
            .status()
            .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("running cargo build"))))?;
        if !status.success() {
            return Err(Error::Invalid(
                super::po::t("cargo build of delonix failed").into(),
            ));
        }
        let built = workspace_root.join("target/release/delonix");
        if built.exists() {
            return Ok(built);
        }
    }
    download_delonix_bin().map_err(|e| {
        Error::Invalid(format!(
            "{e} — or use --delonix-bin <path> / run from a source checkout"
        ))
    })
}

/// Downloads (and caches) `delonix` itself from the GitHub release matching
/// the RUNNING binary's own version. Same verified-download machinery as
/// `download_cri_bin`, just a different release asset name.
fn download_delonix_bin() -> Result<PathBuf> {
    let version = env!("CARGO_PKG_VERSION");
    let cache_dir = cri_cache_dir();
    let cached = cache_dir.join("delonix");
    if cached.exists() {
        return Ok(cached);
    }
    std::fs::create_dir_all(&cache_dir)?;
    let base_url =
        format!("https://github.com/angolardevops/delonix-runtime/releases/download/v{version}");
    let tmp = cache_dir.join("delonix.download");
    let variant = if cpu_has_x86_64_v3() { "-v3" } else { "" };
    let mut asset = format!("delonix-x86_64{variant}-linux");
    eprintln!(
        "{}",
        super::po::tf(
            "downloading {asset} (v{version})...",
            &[("asset", &asset), ("version", version)],
        )
    );
    if !variant.is_empty() && stream_download(&format!("{base_url}/{asset}"), &tmp).is_err() {
        asset = "delonix-x86_64-linux".to_string();
        eprintln!(
            "{}",
            super::po::tf(
                "{asset} missing from this release — trying the generic binary...",
                &[("asset", &asset)],
            )
        );
        stream_download(&format!("{base_url}/{asset}"), &tmp)?;
    } else if variant.is_empty() {
        stream_download(&format!("{base_url}/{asset}"), &tmp)?;
    }
    let sums = http_get_text(&format!("{base_url}/SHA256SUMS"))?;
    let expected = sums
        .lines()
        .find(|l| l.trim_end().ends_with(&asset))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| Error::Invalid(format!("SHA256SUMS has no entry for {asset}")))?
        .to_string();
    let got = hex_sha256_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for {asset}: expected {expected}, got {got} — download discarded",
            &[("asset", &asset), ("expected", &expected), ("got", &got)],
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cached, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(cached)
}

fn find_workspace_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("Cargo.toml").exists() && dir.join("crates/delonix-cri").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Embedded fallback for `dist/delonix-cri.service` — the ONLY file
/// `workspace_dist_file` is ever asked for. It is small, static, and
/// version-independent (no templating), so there is no staleness risk in
/// baking it into the binary at compile time — same fix class as
/// `download_cri_bin` above (a user outside a source checkout had no way
/// forward for this file either), just without needing a network round-trip
/// since the content already ships inside us.
const DELONIX_CRI_SERVICE_UNIT: &str = include_str!("../../../../dist/delonix-cri.service");

pub(crate) fn workspace_dist_file(name: &str) -> Result<PathBuf> {
    if let Some(root) = find_workspace_root() {
        let p = root.join("dist").join(name);
        if p.exists() {
            return Ok(p);
        }
    }
    if name == "delonix-cri.service" {
        let cached = cri_cache_dir().join(name);
        if !cached.exists() {
            std::fs::create_dir_all(cri_cache_dir())?;
            std::fs::write(&cached, DELONIX_CRI_SERVICE_UNIT)?;
        }
        return Ok(cached);
    }
    Err(Error::Invalid(super::po::tf(
        "could not find dist/{name} — run from the source-code checkout or supply it via --extra-run",
        &[("name", name)],
    )))
}

// ---------------------------------------------------------------------------
// Customization steps (pure function — testable without a real VM/virt-customize)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CustomizeOp {
    RunCommand(String),
    CopyIn(PathBuf, String),
    Password { user: String, password: String },
    RootPassword(String),
}

/// Builds the list of customization steps to apply to the base image — the
/// "100% parameterized" part: `extra_packages`/`extra_run` extend without
/// touching this function. Pure (no I/O), testable in isolation. The
/// technically sensitive recipes (repo/packages/swap/modules/sysctls) come from
/// `k8s_recipes::k8s_host_recipes` — the SAME catalog that `cmd::cluster`
/// uses via SSH, so the golden image and a host prepared by `cluster
/// apply` end up exactly alike.
/// Like [`k8s_customization_steps`], but WITHOUT network in the guest: instead of
/// the apt repository + `apt-get install`, it injects the `.deb` files already
/// downloaded and verified on the HOST (`download_k8s_debs`) and installs them with
/// `dpkg -i`. The remaining recipes (swap/modules/sysctls) are the SAME as the online
/// path (`k8s_recipes::k8s_config_recipes`) — they do not diverge.
///
/// `dpkg -i` instead of `apt-get install ./*.deb`: apt would need to contact
/// the lists to resolve deps; the kubelet deps outside the k8s repo
/// (iptables/mount/util-linux/libc6) already come in the cloud image. If any is
/// missing, `dpkg` fails LOUDLY and the build stops — it never leaves a half-installed guest.
///
/// `preseed_images_root`, when given (see `preseed_k8s_images`), points at a
/// HOST-side `delonix_image::ImageStore` root already populated with
/// kubeadm's core images — copied verbatim into the guest's own
/// `/var/lib/delonix` (what `delonix-cri` reads at runtime) via 4
/// `--copy-in` calls, one per `ImageStore` subdirectory
/// (`images`/`layers`/`containers`/`blobs`).
pub(crate) fn k8s_customization_steps_offline(
    debs: &[PathBuf],
    extra_run: &[String],
    cri_bin: &Path,
    cri_service: &Path,
    preseed_images_root: Option<&Path>,
    distro: Distro,
) -> Vec<CustomizeOp> {
    let mut ops: Vec<CustomizeOp> = Vec::new();
    // `--copy-in` requires the target directory to ALREADY exist in the guest.
    ops.push(CustomizeOp::RunCommand("mkdir -p /tmp/k8s-debs".into()));
    for d in debs {
        ops.push(CustomizeOp::CopyIn(d.clone(), "/tmp/k8s-debs".to_string()));
    }
    ops.push(CustomizeOp::RunCommand(
        "dpkg -i /tmp/k8s-debs/*.deb && apt-mark hold kubeadm kubelet kubectl && rm -rf /tmp/k8s-debs"
            .into(),
    ));
    ops.extend(
        super::k8s_recipes::k8s_config_recipes()
            .into_iter()
            .map(|r| CustomizeOp::RunCommand(r.apply_offline().to_string())),
    );
    ops.extend(install_cri_steps(cri_bin, cri_service));
    ops.extend(shared_account_steps(extra_run, distro));
    if let Some(root) = preseed_images_root {
        ops.push(CustomizeOp::RunCommand("mkdir -p /var/lib/delonix".into()));
        for sub in ["images", "layers", "containers", "blobs"] {
            ops.push(CustomizeOp::CopyIn(
                root.join(sub),
                "/var/lib/delonix".to_string(),
            ));
        }
    }
    ops
}

pub(crate) fn k8s_customization_steps(
    k8s_version: Option<&str>,
    extra_packages: &[String],
    extra_run: &[String],
    cri_bin: &Path,
    cri_service: &Path,
    distro: Distro,
) -> Vec<CustomizeOp> {
    let mut ops: Vec<CustomizeOp> =
        super::k8s_recipes::k8s_host_recipes(k8s_version, extra_packages)
            .into_iter()
            .map(|r| CustomizeOp::RunCommand(r.apply_offline().to_string()))
            .collect();
    ops.extend(install_cri_steps(cri_bin, cri_service));
    ops.extend(shared_account_steps(extra_run, distro));
    ops
}

/// `delonix-cri` install — CRI endpoint for the kubelet (replaces containerd).
/// Split out of the old `common_customization_steps` so the no-k8s golden
/// image path (`rootless_customization_steps`) can skip it entirely: a
/// CRI-to-kubelet shim is meaningless on a VM with no kubelet.
fn install_cri_steps(cri_bin: &Path, cri_service: &Path) -> Vec<CustomizeOp> {
    vec![
        CustomizeOp::CopyIn(cri_bin.to_path_buf(), "/usr/local/bin".to_string()),
        CustomizeOp::RunCommand("chmod +x /usr/local/bin/delonix-cri".into()),
        CustomizeOp::CopyIn(cri_service.to_path_buf(), "/etc/systemd/system".to_string()),
        CustomizeOp::RunCommand("systemctl enable delonix-cri.service".into()),
    ]
}

/// The tail shared by every golden image variant regardless of k8s/no-k8s:
/// accounts + the user's `--extra-run` + package-cache cleanup + machine-id
/// reset. Kept separate from `install_cri_steps` so a non-Kubernetes image
/// gets the same account/UX/cleanup guarantees without carrying the CRI shim.
///
/// The three lines below actually differ by distro FAMILY, all confirmed
/// live (not assumed) before writing this: the sudo-equivalent group is
/// `wheel` on Rocky/RHEL, not `sudo` (which doesn't exist there at all);
/// the system-wide interactive-bash file is `/etc/bashrc` on Rocky, not
/// `/etc/bash.bashrc` (a Debian/Ubuntu-family convention); and the package
/// cache cleanup command is obviously package-manager-specific.
fn shared_account_steps(extra_run: &[String], distro: Distro) -> Vec<CustomizeOp> {
    let sudo_group = match distro {
        Distro::Ubuntu | Distro::Debian => "sudo",
        Distro::Rocky => "wheel",
    };
    let bashrc_path = match distro {
        Distro::Ubuntu | Distro::Debian => "/etc/bash.bashrc",
        Distro::Rocky => "/etc/bashrc",
    };
    let mut ops: Vec<CustomizeOp> = Vec::new();
    ops.extend([
        // Default account: root/delonix and delonix:delonix in sudoers (explicit request).
        CustomizeOp::RootPassword("delonix".to_string()),
        CustomizeOp::RunCommand(format!(
            "useradd -m -s /bin/bash -G {sudo_group} delonix || true"
        )),
        CustomizeOp::Password { user: "delonix".to_string(), password: "delonix".to_string() },
        CustomizeOp::RunCommand(
            "echo 'delonix ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/90-delonix && chmod 440 /etc/sudoers.d/90-delonix"
                .into(),
        ),
        // Shell UX the Kubernetes docs recommend: kubectl/kubeadm bash completion
        // + the `k` alias (with completion wired to it). Written to the
        // system-wide interactive-bash file (sourced for every interactive
        // bash — login AND non-login — so both the serial console and SSH
        // get it), NOT to `/etc/profile.d` (those are sourced by `sh` too,
        // which chokes on the `<(...)` process substitution). Each block is
        // guarded by `command -v` so it is inert if a tool is missing (e.g.
        // every non-k8s image), and evaluated at shell-start (not build
        // time) — order relative to the package install does not matter.
        CustomizeOp::RunCommand(format!(
            "cat >> {bashrc_path} <<'DELONIX_KUBECTL_EOF'\n\
             \n\
             # --- Delonix golden image: kubectl/kubeadm completion + `k` alias (k8s docs) ---\n\
             if command -v kubectl >/dev/null 2>&1; then\n\
             \x20 source <(kubectl completion bash)\n\
             \x20 alias k=kubectl\n\
             \x20 complete -o default -F __start_kubectl k\n\
             fi\n\
             if command -v kubeadm >/dev/null 2>&1; then\n\
             \x20 source <(kubeadm completion bash)\n\
             fi\n\
             if command -v crictl >/dev/null 2>&1; then\n\
             \x20 source <(crictl completion bash) 2>/dev/null || true\n\
             fi\n\
             # --- end Delonix ---\n\
             DELONIX_KUBECTL_EOF"
        )),
    ]);
    ops.extend(extra_run.iter().cloned().map(CustomizeOp::RunCommand));
    // Records the installed kernel's `uname -r` string for `image --vm ls`'s
    // KERNEL column — `virt-customize` never boots the image's own kernel (it
    // chroots via its OWN appliance kernel), so there is no `uname -r` to run
    // here; `/boot/vmlinuz-<release>` is named by the exact release string
    // once booted, so listing it is the reliable proxy. Written to a file
    // (not returned — `virt-customize` has no channel back to the host
    // process) that `cmd_build` reads out with `virt-cat` right after this
    // runs, once for the whole build, not per VM.
    ops.push(CustomizeOp::RunCommand(
        "ls /boot/vmlinuz-* 2>/dev/null | sed 's#.*/vmlinuz-##' | sort -V | tail -1 \
         > /etc/delonix-kernel-version || echo unknown > /etc/delonix-kernel-version"
            .into(),
    ));
    // Package cache cleanup — ALWAYS at the end (after the user's
    // `--extra-run`, which may install more packages). Measured on a 24.04
    // apt golden: `/var/cache/apt` (~181 MiB of already-installed .deb) +
    // `/var/lib/apt/lists` (~186 MiB of indexes) = ~367 MiB of pure garbage,
    // which filled the root to 92%. An `apt-get update`/`dnf makecache`
    // regenerates the indexes if the node needs them.
    //
    // DELIBERATELY here and not in `k8s_recipes`: that catalog is SHARED
    // with `cluster apply`, which prepares LIVE hosts — cleaning the package
    // cache is a concern of the ARTIFACT (shrinking a distributable image),
    // not of host preparation.
    let cleanup_cmd = match distro {
        Distro::Ubuntu | Distro::Debian => "apt-get clean && rm -rf /var/lib/apt/lists/*",
        Distro::Rocky => "dnf clean all",
    };
    ops.push(CustomizeOp::RunCommand(cleanup_cmd.into()));
    // BUG FOUND LIVE (delonix cluster kubeadm, multi-VM libvirt NAT): every VM
    // cloned from this golden qcow2 shares ONE `/etc/machine-id` — installing
    // kubeadm's dependencies during `virt-customize` pulls in a package whose
    // postinst calls `systemd-machine-id-setup`/`dbus-uuidgen`, baking a REAL id
    // into the image (a fresh Ubuntu cloud image ships this file EMPTY on
    // purpose, so systemd generates a fresh one on each VM's actual first boot —
    // `virt-customize` doesn't do that virt-sysprep-style cleanup by itself).
    // systemd-networkd derives its DHCP client-id (DUID) from machine-id, so
    // dnsmasq saw 3 cluster VMs as the SAME client and kept moving the one lease
    // to whichever VM last renewed — evicting the other two, breaking
    // connectivity mid-`kubeadm init`. Confirmed live: `lab-cp1` and `lab-w1`
    // reported the byte-for-byte identical machine-id. MUST be the very last
    // step (after `--extra-run`/apt cleanup) so nothing after it regenerates one.
    ops.push(CustomizeOp::RunCommand(
        "truncate -s 0 /etc/machine-id && rm -f /var/lib/dbus/machine-id && ln -sf /etc/machine-id /var/lib/dbus/machine-id"
            .into(),
    ));
    ops
}

/// Non-Kubernetes golden image: no kubeadm/kubelet/kubectl, no `delonix-cri`
/// (a CRI shim is meaningless without a kubelet to call it) — instead, the
/// `delonix` engine binary itself plus everything `scripts/install.sh`
/// normally configures on a fresh host for rootless containers to work
/// out of the box (rootless deps, subuid/subgid range, AppArmor profile on
/// 23.10+-family hosts). Without this, the appliance boots but `delonix run`
/// fails immediately on a userns error — defeating the point of a golden image.
pub(crate) fn rootless_customization_steps(
    extra_run: &[String],
    delonix_bin: &Path,
    distro: Distro,
) -> Vec<CustomizeOp> {
    // Same package LIST `install.sh` requires (`require_dep`/`optional_dep`),
    // just guest-installed instead of host-detected. Package NAMES confirmed
    // live against Rocky 9's own repo listings before writing this (not
    // assumed): `shadow-utils` (not `uidmap`), `iproute` (not `iproute2`),
    // `conntrack-tools` (not `conntrack`) — all present in Rocky's base
    // BaseOS/AppStream, no EPEL needed. `nftables`/`slirp4netns` share the
    // same package name across both families.
    let pkg_install_cmd = match distro {
        Distro::Ubuntu | Distro::Debian => {
            "apt-get update && apt-get install -y slirp4netns uidmap nftables iproute2 conntrack"
                .to_string()
        }
        Distro::Rocky => {
            "dnf install -y slirp4netns shadow-utils nftables iproute conntrack-tools".to_string()
        }
    };
    let mut ops: Vec<CustomizeOp> = vec![
        CustomizeOp::RunCommand(pkg_install_cmd),
        // `delonix` — the daemonless engine binary itself. No systemd unit: it
        // is CLI-invoked, not a long-running service (unlike `delonix-cri`).
        CustomizeOp::CopyIn(delonix_bin.to_path_buf(), "/usr/local/bin".to_string()),
        CustomizeOp::RunCommand("chmod +x /usr/local/bin/delonix".into()),
    ];
    ops.extend(shared_account_steps(extra_run, distro));
    // Subuid/subgid range for the `delonix` account — mirrors `install.sh`'s
    // `ensure_subid`: without a range, the rootless userns only maps 1 uid and
    // any image with a non-root USER fails. Idempotent (`grep -q` guard).
    // `/etc/subuid`/`/etc/subgid` are `shadow-utils` files, present and in the
    // same location on every distro family here.
    ops.push(CustomizeOp::RunCommand(
        "grep -q '^delonix:' /etc/subuid || echo 'delonix:100000:65536' >> /etc/subuid".into(),
    ));
    ops.push(CustomizeOp::RunCommand(
        "grep -q '^delonix:' /etc/subgid || echo 'delonix:100000:65536' >> /etc/subgid".into(),
    ));
    // AppArmor `unconfined+userns` profile — Ubuntu-ONLY. The kernel sysctl
    // this defends against (`kernel.apparmor_restrict_unprivileged_userns=1`,
    // 23.10+-family hosts) is an Ubuntu-specific LSM hardening patch, not
    // present upstream/Debian and meaningless on Rocky (SELinux, no AppArmor
    // LSM at all). BUG CAUGHT HERE, before it shipped: the write is
    // `printf ... > /etc/apparmor.d/delonix && (apparmor_parser ... || true)`
    // — the `|| true` only guards the parser call, NOT the file write. Rocky
    // cloud images have no `/etc/apparmor.d/` directory at all, so the
    // redirect itself would fail, the `&&` would short-circuit, and the
    // WHOLE `virt-customize` step (hence the whole build) would fail — not a
    // harmless no-op like on a distro that happens to lack AppArmor tooling
    // but still has the directory. Gating to `Ubuntu` is the correct fix, not
    // just a Rocky workaround (also correct for Debian, which never actually
    // needed this step either — kept unconditional there since it degrades
    // harmlessly, but Rocky specifically cannot risk it).
    if distro == Distro::Ubuntu {
        ops.push(CustomizeOp::RunCommand(
            "printf 'abi <abi/4.0>,\\ninclude <tunables/global>\\nprofile delonix /usr/local/bin/delonix flags=(unconfined) {\\n  userns,\\n}\\n' > /etc/apparmor.d/delonix && \
             (apparmor_parser -r /etc/apparmor.d/delonix || true)"
                .into(),
        ));
    }
    // Publishing 80/443 rootless (`-p 80:80`) is refused by the kernel: the
    // host-side bind is done by `slirp4netns` as the unprivileged `delonix`
    // user, and ports below `net.ipv4.ip_unprivileged_port_start` (1024 by
    // default) need CAP_NET_BIND_SERVICE.
    //
    // `install.sh` keeps this OPT-IN (`--low-ports`) because it lowers a
    // privilege boundary for a whole host that may be shared or in production:
    // from then on any local program can bind 80-1023. THIS image is the
    // opposite situation — a disposable, single-tenant VM whose entire purpose
    // is running Delonix rootless — so the trade-off flips and it ships applied.
    // Deliberately NOT in `shared_account_steps`: the k8s golden is a Kubernetes
    // node whose kubelet/kube-proxy already run as root and never needed it.
    //
    // Written as a file, not `sysctl -w`: `virt-customize` runs against an
    // offline guest, so only what lands in /etc/sysctl.d survives to first boot.
    ops.push(CustomizeOp::RunCommand(
        "printf '# Delonix Runtime — publish ports <1024 rootless (see install.sh --low-ports).\\n\
         net.ipv4.ip_unprivileged_port_start = 80\\n' > /etc/sysctl.d/99-delonix-lowports.conf"
            .into(),
    ));
    ops
}

/// Translates the `CustomizeOp`s into the actual `virt-customize` arguments.
pub(crate) fn customize_args(disk: &Path, ops: &[CustomizeOp]) -> Vec<String> {
    let mut args = vec!["-a".to_string(), disk.to_string_lossy().into_owned()];
    for op in ops {
        match op {
            CustomizeOp::RunCommand(cmd) => {
                args.push("--run-command".into());
                args.push(cmd.clone());
            }
            CustomizeOp::CopyIn(src, dst) => {
                args.push("--copy-in".into());
                args.push(format!("{}:{}", src.display(), dst));
            }
            CustomizeOp::Password { user, password } => {
                args.push("--password".into());
                args.push(format!("{user}:password:{password}"));
            }
            CustomizeOp::RootPassword(password) => {
                args.push("--root-password".into());
                args.push(format!("password:{password}"));
            }
        }
    }
    args
}

fn run_tool(bin: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(bin).args(args).status().map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::tf("running {bin}", &[("bin", bin)])
        ))
    })?;
    if !status.success() {
        return Err(Error::Invalid(super::po::tf(
            "{bin} failed (exit {code})",
            &[("bin", bin), ("code", &format!("{:?}", status.code()))],
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gap closed: `OFFICIAL_VM_BASE_IMAGE` existed but had no default-selection
    /// wiring — a bare `pull`/`ls-remote --no-k8s` now resolves to it, same as a
    /// bare `pull`/`ls-remote` already resolved to the Kubernetes golden.
    #[test]
    fn default_pull_source_escolhe_k8s_ou_base_consoante_a_flag() {
        assert_eq!(default_pull_source(false), OFFICIAL_VM_IMAGE);
        assert_eq!(default_pull_source(true), OFFICIAL_VM_BASE_IMAGE);
        assert_ne!(default_pull_source(false), default_pull_source(true));
    }

    #[test]
    fn customization_steps_incluem_pacotes_extra() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops =
            k8s_customization_steps(None, &["htop".to_string()], &[], &cri, &svc, Distro::Ubuntu);
        let install_step = ops
            .iter()
            .find_map(|op| match op {
                CustomizeOp::RunCommand(c) if c.contains("apt-get install") => Some(c),
                _ => None,
            })
            .expect("devia haver um RunCommand de apt-get install");
        assert!(install_step.contains("kubeadm"));
        assert!(install_step.contains("htop"));
    }

    #[test]
    fn fmt_size_legivel_por_escalao() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(1024), "1.0 KiB");
        assert_eq!(fmt_size(1536), "1.5 KiB");
        assert_eq!(fmt_size(1024 * 1024), "1.0 MiB");
        assert_eq!(fmt_size(2_555_576_320), "2.38 GiB");
        assert_eq!(fmt_size(1024_u64.pow(4)), "1.00 TiB");
    }

    #[test]
    fn fmt_local_tem_a_forma_data_hora() {
        // 1784216635 → a local date/time; we validate the SHAPE (the timezone is the host's).
        let s = fmt_local(1_784_216_635);
        let b = s.as_bytes();
        assert_eq!(s.len(), 16, "esperado 'AAAA-MM-DD HH:MM', obtido {s:?}");
        assert!(b[4] == b'-' && b[7] == b'-' && b[10] == b' ' && b[13] == b':');
        assert!(s[..4].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn customization_steps_incluem_extra_run_no_fim() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(
            None,
            &[],
            &["echo oi".to_string()],
            &cri,
            &svc,
            Distro::Ubuntu,
        );
        // `--extra-run` runs after all base steps; only the apt cleanup
        // comes after it (it must be last — the extra-run may install packages).
        let idx_extra = ops
            .iter()
            .position(|op| matches!(op, CustomizeOp::RunCommand(c) if c == "echo oi"))
            .expect("o --extra-run devia estar na lista");
        assert_eq!(
            idx_extra,
            ops.len() - 4,
            "o --extra-run devia vir logo antes da leitura do kernel + limpeza"
        );
        assert!(
            matches!(&ops[ops.len() - 3], CustomizeOp::RunCommand(c) if c.contains("/etc/delonix-kernel-version"))
        );
        assert!(
            matches!(&ops[ops.len() - 2], CustomizeOp::RunCommand(c) if c.contains("apt-get clean"))
        );
        // machine-id reset must be the ABSOLUTE last step (regression: shared
        // machine-id across cloned VMs breaks DHCP client-id, see comment at
        // the push site in `common_customization_steps`).
        assert!(
            matches!(ops.last(), Some(CustomizeOp::RunCommand(c)) if c.contains("truncate -s 0 /etc/machine-id")),
            "o reset do machine-id devia ser o ÚLTIMO passo"
        );
    }

    #[test]
    fn customization_steps_configuram_completion_e_alias_k() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        // Both build paths (online + offline) share `common_customization_steps`,
        // so the kubectl UX must be present in both.
        for ops in [
            k8s_customization_steps(None, &[], &[], &cri, &svc, Distro::Ubuntu),
            k8s_customization_steps_offline(
                &[PathBuf::from("/tmp/x/kubeadm_1.34.9-1.1_amd64.deb")],
                &[],
                &cri,
                &svc,
                None,
                Distro::Ubuntu,
            ),
        ] {
            let bashrc = ops
                .iter()
                .find_map(|op| match op {
                    CustomizeOp::RunCommand(c) if c.contains("/etc/bash.bashrc") => Some(c),
                    _ => None,
                })
                .expect("devia haver um passo a escrever no /etc/bash.bashrc");
            assert!(bashrc.contains("kubectl completion bash"));
            assert!(bashrc.contains("alias k=kubectl"));
            assert!(bashrc.contains("complete -o default -F __start_kubectl k"));
            assert!(bashrc.contains("kubeadm completion bash"));
            // Guarded so it is inert when a tool is absent.
            assert!(bashrc.contains("command -v kubectl"));
        }
    }

    /// A reduced `Packages`, with the same shape as the real one (several architectures and
    /// versions per package) — includes the case that broke the 1st offline build.
    const PACKAGES_FIXTURE: &str = "\
Package: cri-tools
Version: 1.34.0-1.1
Architecture: amd64
Filename: amd64/cri-tools_1.34.0-1.1_amd64.deb
SHA256: aaa1

Package: kubeadm
Version: 1.34.0-1.1
Architecture: amd64
Filename: amd64/kubeadm_1.34.0-1.1_amd64.deb
SHA256: bbb1

Package: kubeadm
Version: 1.34.9-1.1
Architecture: amd64
Filename: amd64/kubeadm_1.34.9-1.1_amd64.deb
SHA256: bbb2

Package: kubeadm
Version: 1.34.9-1.1
Architecture: arm64
Filename: arm64/kubeadm_1.34.9-1.1_arm64.deb
SHA256: bbb3

Package: kubeadm
Version: 1.33.1-1.1
Architecture: amd64
Filename: amd64/kubeadm_1.33.1-1.1_amd64.deb
SHA256: bbb4

Package: kubernetes-cni
Version: 1.7.1-1.1
Architecture: amd64
Filename: amd64/kubernetes-cni_1.7.1-1.1_amd64.deb
SHA256: ccc1
";

    #[test]
    fn parse_packages_escolhe_maior_versao_da_arch_certa() {
        let got = parse_packages_index(
            PACKAGES_FIXTURE,
            "amd64",
            "1.34.",
            &["kubeadm"],
            &["kubeadm"],
        );
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].version, "1.34.9-1.1",
            "devia escolher a maior 1.34.*"
        );
        assert_eq!(got[0].filename, "amd64/kubeadm_1.34.9-1.1_amd64.deb");
        assert_eq!(got[0].sha256, "bbb2");
    }

    #[test]
    fn parse_packages_ignora_versionamento_proprio_no_filtro_de_versao() {
        // REGRESSION: `kubernetes-cni` is 1.7.x — filtering it by "1.34." returned
        // nothing and the offline build aborted with "does not have kubernetes-cni".
        let got = parse_packages_index(
            PACKAGES_FIXTURE,
            "amd64",
            "1.34.",
            &["kubeadm", "kubernetes-cni"],
            &["kubeadm"], // only kubeadm follows the k8s version
        );
        let cni = got
            .iter()
            .find(|d| d.name == "kubernetes-cni")
            .expect("cni tem de vir");
        assert_eq!(cni.version, "1.7.1-1.1");
        assert!(got
            .iter()
            .any(|d| d.name == "kubeadm" && d.version == "1.34.9-1.1"));
    }

    #[test]
    fn deb_version_lt_compara_numericamente() {
        assert!(deb_version_lt("1.34.0-1.1", "1.34.9-1.1"));
        assert!(deb_version_lt("1.33.1-1.1", "1.34.0-1.1"));
        assert!(
            deb_version_lt("1.9.0-1.1", "1.10.0-1.1"),
            "9 < 10 numericamente, não lexicograficamente"
        );
        assert!(!deb_version_lt("1.34.9-1.1", "1.34.0-1.1"));
        assert!(!deb_version_lt("1.34.9-1.1", "1.34.9-1.1"));
    }

    #[test]
    fn release_sha256_of_le_a_seccao_certa() {
        let release = "\
Origin: obs://build.opensuse.org
MD5Sum:
 deadbeef 1234 Packages
SHA256:
 abc123 4567 Packages
 def456 89 Release
Date: Fri, 12 Jun 2026 12:40:56 UTC
";
        assert_eq!(
            release_sha256_of(release, "Packages").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            release_sha256_of(release, "Release").as_deref(),
            Some("def456")
        );
        assert_eq!(release_sha256_of(release, "nao-existe"), None);
    }

    #[test]
    fn steps_offline_instalam_por_dpkg_e_nao_tocam_a_rede() {
        let debs = vec![PathBuf::from("/tmp/x/kubeadm_1.34.9-1.1_amd64.deb")];
        let ops = k8s_customization_steps_offline(
            &debs,
            &[],
            &PathBuf::from("/tmp/delonix-cri"),
            &PathBuf::from("/tmp/delonix-cri.service"),
            None,
            Distro::Ubuntu,
        );
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds
            .iter()
            .any(|c| c.contains("dpkg -i /tmp/k8s-debs/*.deb")));
        assert!(
            cmds.iter().any(|c| c.contains("mkdir -p /tmp/k8s-debs")),
            "o --copy-in exige o dir criado"
        );
        // The central guarantee of offline mode: nothing contacts the network in the guest.
        for c in &cmds {
            assert!(
                !c.contains("curl") && !c.contains("apt-get update") && !c.contains("https://"),
                "passo offline com rede: {c}"
            );
        }
        // And the .deb is injected.
        assert!(ops
            .iter()
            .any(|o| matches!(o, CustomizeOp::CopyIn(_, d) if d == "/tmp/k8s-debs")));
    }

    #[test]
    fn steps_offline_com_preseed_injecta_as_4_subpastas_do_image_store() {
        let debs = vec![PathBuf::from("/tmp/x/kubeadm_1.34.9-1.1_amd64.deb")];
        let preseed_root = PathBuf::from("/tmp/preseed-images");
        let ops = k8s_customization_steps_offline(
            &debs,
            &[],
            &PathBuf::from("/tmp/delonix-cri"),
            &PathBuf::from("/tmp/delonix-cri.service"),
            Some(&preseed_root),
            Distro::Ubuntu,
        );
        for sub in ["images", "layers", "containers", "blobs"] {
            assert!(
                ops.iter().any(|o| matches!(
                    o,
                    CustomizeOp::CopyIn(src, dst)
                        if src == &preseed_root.join(sub) && dst == "/var/lib/delonix"
                )),
                "faltou o --copy-in de {sub}"
            );
        }
        assert!(
            ops.iter().any(
                |o| matches!(o, CustomizeOp::RunCommand(c) if c == "mkdir -p /var/lib/delonix")
            ),
            "/var/lib/delonix tem de existir antes do --copy-in"
        );
    }

    #[test]
    fn customization_steps_limpam_a_cache_apt_no_fim() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(None, &[], &[], &cri, &svc, Distro::Ubuntu);
        // ~367 MiB of .deb + indexes that, without this, filled the golden's root to 92%.
        // Second-to-last: the machine-id reset (below) must run AFTER it.
        let clean = &ops[ops.len() - 2];
        assert!(
            matches!(clean, CustomizeOp::RunCommand(c) if c.contains("apt-get clean") && c.contains("/var/lib/apt/lists")),
            "o penúltimo passo devia limpar a cache apt, obtido: {clean:?}"
        );
    }

    #[test]
    fn customization_steps_configuram_delonix_user_e_root_password() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(None, &[], &[], &cri, &svc, Distro::Ubuntu);
        assert!(ops
            .iter()
            .any(|op| matches!(op, CustomizeOp::RootPassword(p) if p == "delonix")));
        assert!(ops.iter().any(|op| matches!(op, CustomizeOp::Password{user,password} if user=="delonix" && password=="delonix")));
    }

    #[test]
    fn rootless_steps_instalam_dependencias_e_o_binario_delonix_sem_cri() {
        let delonix = PathBuf::from("/tmp/delonix");
        let ops = rootless_customization_steps(&[], &delonix, Distro::Ubuntu);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds.iter().any(|c| c.contains("slirp4netns")
            && c.contains("uidmap")
            && c.contains("nftables")
            && c.contains("iproute2")
            && c.contains("conntrack")));
        assert!(ops
            .iter()
            .any(|o| matches!(o, CustomizeOp::CopyIn(src, dst) if src == &delonix && dst == "/usr/local/bin")));
        // No CRI shim on a no-k8s image — there is no kubelet to serve.
        assert!(!cmds.iter().any(|c| c.contains("delonix-cri")));
        assert!(!ops
            .iter()
            .any(|o| matches!(o, CustomizeOp::CopyIn(_, dst) if dst == "/etc/systemd/system")));
    }

    #[test]
    fn rootless_steps_configuram_subuid_e_subgid_para_delonix() {
        let ops = rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Ubuntu);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds
            .iter()
            .any(|c| c.contains("/etc/subuid") && c.contains("delonix:100000:65536")));
        assert!(cmds
            .iter()
            .any(|c| c.contains("/etc/subgid") && c.contains("delonix:100000:65536")));
    }

    #[test]
    fn rootless_steps_escrevem_o_perfil_apparmor_unconfined_userns() {
        let ops = rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Ubuntu);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds.iter().any(|c| c.contains("/etc/apparmor.d/delonix")
            && c.contains("flags=(unconfined)")
            && c.contains("userns")));
    }

    #[test]
    fn rootless_steps_partilham_a_criacao_de_conta_delonix() {
        let ops = rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Ubuntu);
        assert!(ops
            .iter()
            .any(|op| matches!(op, CustomizeOp::RootPassword(p) if p == "delonix")));
        assert!(ops.iter().any(|op| matches!(op, CustomizeOp::Password{user,password} if user=="delonix" && password=="delonix")));
    }

    #[test]
    fn rootless_steps_rocky_usa_dnf_e_pacotes_rpm() {
        let ops = rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Rocky);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        // Package NAMES confirmed live against Rocky 9's own repo listings —
        // `shadow-utils`/`iproute`/`conntrack-tools`, not the apt names.
        assert!(cmds.iter().any(|c| c.starts_with("dnf install")
            && c.contains("slirp4netns")
            && c.contains("shadow-utils")
            && c.contains("nftables")
            && c.contains("iproute")
            && c.contains("conntrack-tools")));
        assert!(!cmds.iter().any(|c| c.contains("apt-get")));
    }

    #[test]
    fn rootless_steps_rocky_usa_wheel_bashrc_e_dnf_clean() {
        let ops = rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Rocky);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds
            .iter()
            .any(|c| c.contains("useradd") && c.contains("-G wheel")));
        assert!(!cmds.iter().any(|c| c.contains("-G sudo")));
        assert!(cmds.iter().any(|c| c.contains(">> /etc/bashrc")));
        assert!(!cmds.iter().any(|c| c.contains("/etc/bash.bashrc")));
        assert!(cmds.iter().any(|c| c == &"dnf clean all"));
        assert!(!cmds.iter().any(|c| c.contains("apt-get clean")));
    }

    #[test]
    fn rootless_steps_rocky_nunca_escreve_perfil_apparmor() {
        // BUG CAUGHT DURING IMPLEMENTATION: the AppArmor write is
        // `printf ... > /etc/apparmor.d/delonix && (apparmor_parser ... || true)`
        // — only the parser call is guarded. Rocky has no `/etc/apparmor.d/`
        // directory at all, so if this step ran there the redirect itself
        // would fail and take down the whole `virt-customize` build (the
        // `&&` isn't guarded). Ubuntu-only gating is the fix; this test
        // proves Rocky never even sees the command.
        let ops = rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Rocky);
        assert!(!ops
            .iter()
            .any(|o| matches!(o, CustomizeOp::RunCommand(c) if c.contains("apparmor"))));
    }

    /// The rootless golden ships `ip_unprivileged_port_start=80` applied, so
    /// `-p 80:80` works out of the box in it — a disposable single-tenant VM is
    /// exactly the case where the host-wide trade-off is acceptable, unlike the
    /// public `install.sh` (where it stays behind `--low-ports`). Every distro,
    /// since the file is a kernel path, not a packaging convention. The k8s
    /// golden must NOT get it: that node's kubelet/kube-proxy run as root.
    #[test]
    fn so_a_golden_rootless_traz_as_portas_baixas_abertas() {
        let has_lowports = |ops: &[CustomizeOp]| {
            ops.iter().any(
                |o| matches!(o, CustomizeOp::RunCommand(c) if c.contains("ip_unprivileged_port_start")),
            )
        };
        for d in [Distro::Ubuntu, Distro::Debian, Distro::Rocky] {
            let ops = rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), d);
            assert!(
                has_lowports(&ops),
                "rootless golden ({d:?}) needs the sysctl"
            );
        }
        assert!(!has_lowports(&k8s_customization_steps(
            None,
            &[],
            &[],
            &PathBuf::from("/tmp/delonix-cri"),
            &PathBuf::from("/tmp/delonix-cri.service"),
            Distro::Ubuntu,
        )));
    }

    #[test]
    fn rootless_steps_debian_nao_muda_de_comportamento() {
        // v0.17.0 regression guard: Debian's sudo/bashrc/apt-clean output must
        // stay byte-identical to before this Rocky-driven refactor.
        let ops = rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Debian);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds
            .iter()
            .any(|c| c.contains("useradd") && c.contains("-G sudo")));
        assert!(cmds.iter().any(|c| c.contains(">> /etc/bash.bashrc")));
        assert!(cmds.iter().any(|c| c.contains("apt-get clean")));
    }

    #[test]
    fn parse_bsd_checksum_extrai_o_hash_do_ficheiro_pedido() {
        // Real line captured live from `dl.rockylinux.org`.
        let text = "# Rocky-9-GenericCloud.latest.x86_64.qcow2: 645988352 bytes\n\
                     SHA256 (Rocky-9-GenericCloud.latest.x86_64.qcow2) = 92c206cc6f790c61583247eefe87890f8828420662c17cacf247cec78ab4eec8\n";
        assert_eq!(
            parse_bsd_checksum(text, "Rocky-9-GenericCloud.latest.x86_64.qcow2"),
            Some("92c206cc6f790c61583247eefe87890f8828420662c17cacf247cec78ab4eec8".to_string())
        );
        assert_eq!(parse_bsd_checksum(text, "other-file.qcow2"), None);
        assert_eq!(parse_bsd_checksum("", "x"), None);
    }

    #[test]
    fn valid_rocky_release_aceita_so_as_versoes_conhecidas() {
        assert!(valid_rocky_release("8").is_ok());
        assert!(valid_rocky_release("9").is_ok());
        assert!(valid_rocky_release("10").is_ok());
        assert!(valid_rocky_release("7").is_err());
        assert!(valid_rocky_release("latest").is_err());
    }

    #[test]
    fn no_k8s_false_com_distro_rocky_e_rejeitado() {
        let (store, dir) = tmp_store();
        let err = cmd_build(
            &store,
            "t",
            Distro::Rocky,
            "24.04",
            "bookworm",
            "9",
            None,
            vec![],
            vec![],
            None,
            true,
            false,
            false, // no_k8s = false — Rocky doesn't support the k8s path yet
            None,
        );
        assert!(err.is_err());
        assert!(format!("{}", err.unwrap_err()).contains("--no-k8s"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tmp_store() -> (VmImageStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "delonix-vmimage-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (VmImageStore::open(&dir).unwrap(), dir)
    }

    #[test]
    fn hex_sha512_file_bate_com_vector_conhecido() {
        // NIST test vector: SHA-512("abc").
        let dir = std::env::temp_dir().join(format!(
            "delonix-vmimage-sha512-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::write(&dir, b"abc").unwrap();
        let got = hex_sha512_file(&dir).unwrap();
        assert_eq!(got, "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn debian_major_version_conhece_os_codinomes_suportados() {
        assert_eq!(debian_major_version("bullseye").unwrap(), "11");
        assert_eq!(debian_major_version("bookworm").unwrap(), "12");
        assert_eq!(debian_major_version("trixie").unwrap(), "13");
        assert!(debian_major_version("sid").is_err());
        assert!(debian_major_version("").is_err());
    }

    #[test]
    fn distro_label_combina_distro_e_release_ou_degrada_com_gracia() {
        let mut img = VmImage {
            name: "x".to_string(),
            tag: "x".to_string(),
            digest: "sha256:x".to_string(),
            size: 0,
            ubuntu_release: None,
            k8s_version: None,
            created_unix: 0,
            kernel_version: None,
            distro: None,
        };
        // Pulled image, no build metadata at all — pre-existing gap, not new.
        assert_eq!(distro_label(&img), "-");
        // Pre-v0.17.0 on-disk metadata: `distro` missing, `ubuntu_release` set.
        img.ubuntu_release = Some("24.04".to_string());
        assert_eq!(distro_label(&img), "24.04");
        // A build from this version on: both set.
        img.distro = Some("debian".to_string());
        img.ubuntu_release = Some("bookworm".to_string());
        assert_eq!(distro_label(&img), "debian/bookworm");
    }

    #[test]
    fn base_cache_path_distingue_distro_e_continua_a_sanitizar() {
        let (store, dir) = tmp_store();
        let ubuntu = store.base_cache_path(Distro::Ubuntu, "24.04");
        let debian = store.base_cache_path(Distro::Debian, "bookworm");
        let rocky = store.base_cache_path(Distro::Rocky, "9");
        assert_ne!(ubuntu, debian);
        assert_ne!(debian, rocky);
        assert_ne!(ubuntu, rocky);
        assert!(ubuntu
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("ubuntu-24.04-"));
        assert!(debian
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("debian-bookworm-"));
        assert!(rocky
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("rocky-9-"));
        // Same path-traversal defense as the Ubuntu side, for the new Debian arm:
        // `sanitize` strips `/` (dots survive — harmless without a separator,
        // `Path::join` can't treat them as multiple segments), so the result
        // stays confined to a single filename inside `_base/`.
        let evil = store.base_cache_path(Distro::Debian, "../../../etc/cron.d/x");
        assert_eq!(evil.parent().unwrap(), dir.join("vm-images").join("_base"));
        assert!(!evil.file_name().unwrap().to_str().unwrap().contains('/'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_k8s_rejeita_k8s_version_offline_e_cri_bin() {
        let (store, dir) = tmp_store();
        let base = |k8s_version: Option<String>, offline: bool, cri_bin: Option<PathBuf>| {
            cmd_build(
                &store,
                "t",
                Distro::Ubuntu,
                "24.04",
                "bookworm",
                "9",
                k8s_version,
                vec![],
                vec![],
                cri_bin,
                true,
                offline,
                true, // no_k8s
                None,
            )
        };
        assert!(base(Some("1.34".into()), false, None).is_err());
        assert!(base(None, true, None).is_err());
        assert!(base(None, false, Some(PathBuf::from("/tmp/x"))).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delonix_bin_sem_no_k8s_e_rejeitado() {
        let (store, dir) = tmp_store();
        let err = cmd_build(
            &store,
            "t",
            Distro::Ubuntu,
            "24.04",
            "bookworm",
            "9",
            None,
            vec![],
            vec![],
            None,
            true,
            false,
            false, // no_k8s = false
            Some(PathBuf::from("/tmp/delonix")),
        );
        assert!(err.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn customize_args_traduz_run_command_e_copy_in_correctamente() {
        let ops = vec![
            CustomizeOp::RunCommand("apt-get install -y a b".to_string()),
            CustomizeOp::CopyIn(PathBuf::from("/host/bin"), "/usr/local/bin".to_string()),
            CustomizeOp::RootPassword("x".to_string()),
        ];
        let args = customize_args(Path::new("/tmp/disk.qcow2"), &ops);
        assert_eq!(args[0], "-a");
        assert_eq!(args[1], "/tmp/disk.qcow2");
        assert!(args.windows(2).any(|w| w
            == [
                "--run-command".to_string(),
                "apt-get install -y a b".to_string()
            ]));
        assert!(args.windows(2).any(|w| w
            == [
                "--copy-in".to_string(),
                "/host/bin:/usr/local/bin".to_string()
            ]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--root-password".to_string(), "password:x".to_string()]));
    }

    #[test]
    fn hex_sha256_e_consistente() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
