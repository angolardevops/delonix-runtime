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
    /// Recommended vCPUs (`VCPUS` in the `VMfile` that built this image).
    /// `vm create` uses it as the default when `--vcpus` is not given AND the
    /// disk resolves to this image (by local name) — never overrides an
    /// explicit `--vcpus`. `#[serde(default)]`: absent on images built before
    /// this field existed, or `vm pull`ed (the OCI artifact only carries the
    /// qcow2 blob, same known gap as `ubuntu_release`/`k8s_version`).
    #[serde(default)]
    pub default_vcpus: Option<u32>,
    /// Recommended memory (`MEMORY` in the `VMfile`, e.g. `"2G"`). Same rule
    /// as `default_vcpus`.
    #[serde(default)]
    pub default_memory: Option<String>,
    /// Recommended VM backend (`HYPERVISOR` in the `VMfile`):
    /// `"cloud-hypervisor"` or `"libvirt"`, already canonicalized by
    /// `delonix_vm::valid_backend_name`. Same rule as `default_vcpus` — wins
    /// over the engine's own auto-detection heuristic but never over an
    /// explicit `--backend`/`DELONIX_VM_BACKEND`/persisted default (see
    /// `cmd::vm::resolve_disk_and_defaults`).
    #[serde(default)]
    pub default_backend: Option<String>,
    /// Whether the guest runs cloud-init. `None` (every image built before
    /// this field existed) means *unknown* and is treated as `true` — the
    /// cloud images this engine has always built do run it, so nothing about
    /// them changes.
    ///
    /// `Some(false)` marks an **appliance**: a vendor system that installs and
    /// configures itself (OPNsense, Proxmox, TrueNAS). For those, `vm create`
    /// must NOT attach the NoCloud seed it otherwise always builds — an ISO
    /// nothing reads, on a CD-ROM that changes the guest's device list. And
    /// `--hostname`/`--ssh-key`/`--user-data` are refused outright rather than
    /// accepted and silently dropped, which is the failure mode this repo
    /// names as its worst (see `--security-opt seccomp=`, `-v …:z`,
    /// `--network-alias` in CLAUDE.md).
    #[serde(default)]
    pub cloud_init: Option<bool>,
}

impl VmImage {
    /// True when this image's guest runs cloud-init, so a NoCloud seed is
    /// worth generating. Unknown (pre-existing metadata) counts as yes: that
    /// is what those images have always been.
    pub fn uses_cloud_init(&self) -> bool {
        self.cloud_init.unwrap_or(true)
    }
}

/// Target format for `image vm convert` — **the integration point with every
/// other hypervisor's ecosystem**.
///
/// This engine runs two backends (libvirt/QEMU and Cloud Hypervisor) and will
/// not grow a backend for every product on the market: VirtualBox does not
/// coexist with KVM on one host, vSphere and Proxmox are remote datacenter APIs
/// rather than local hypervisors, and Hyper-V is Windows. But an IMAGE built
/// here can be imported by all of them, and `qemu-img` already writes every one
/// of their formats — so the cheap, honest integration is the artifact, not a
/// driver. Build here, import there.
///
/// The mapping is not decorative; each format is what a specific product's
/// importer expects:
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConvertFormat {
    /// QEMU/KVM, libvirt, Cloud Hypervisor — this engine's own two backends.
    Qcow2,
    /// Raw sectors: Proxmox VE (its default for LVM/ZFS/Ceph storage), and the
    /// universal fallback anything can read. Not sparse on a filesystem that
    /// does not support holes — expect the full virtual size on disk.
    Raw,
    /// VMware — Workstation, Fusion, ESXi/vSphere.
    Vmdk,
    /// VirtualBox.
    Vdi,
    /// Hyper-V (Windows 8/2012 and later), and Azure's modern disk format.
    Vhdx,
    /// Hyper-V's older VHD (`vpc` in qemu-img's own naming). Kept separate from
    /// `vhdx` because they are different formats, not spellings — an importer
    /// that wants one rejects the other.
    Vhd,
}

impl ConvertFormat {
    /// The name `qemu-img -O` knows. **VHD is `vpc` there** — qemu's historical
    /// name for it — which is exactly the kind of detail a user should not have
    /// to know to get a file Hyper-V accepts.
    fn as_str(self) -> &'static str {
        match self {
            ConvertFormat::Qcow2 => "qcow2",
            ConvertFormat::Raw => "raw",
            ConvertFormat::Vmdk => "vmdk",
            ConvertFormat::Vdi => "vdi",
            ConvertFormat::Vhdx => "vhdx",
            ConvertFormat::Vhd => "vpc",
        }
    }

    /// The file extension the target ecosystem expects — NOT always the same as
    /// the qemu format name (`vpc` produces a `.vhd`), which is why the two are
    /// separate functions instead of one string used twice.
    fn extension(self) -> &'static str {
        match self {
            ConvertFormat::Qcow2 => "qcow2",
            ConvertFormat::Raw => "raw",
            ConvertFormat::Vmdk => "vmdk",
            ConvertFormat::Vdi => "vdi",
            ConvertFormat::Vhdx => "vhdx",
            ConvertFormat::Vhd => "vhd",
        }
    }

    /// Whether the format supports `qemu-img`'s `-c` (compressed) output.
    ///
    /// Only qcow2 and vmdk do. Passing `-c` to the others makes `qemu-img` fail
    /// outright, so this is what keeps `--compress` from turning into a
    /// confusing tool error on a flag the user was invited to use.
    fn supports_compression(self) -> bool {
        matches!(self, ConvertFormat::Qcow2 | ConvertFormat::Vmdk)
    }
}

/// Base distro for a golden image build. `Ubuntu` stays the default (no
/// behavior change for existing callers); `Debian` is additive.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Distro {
    Ubuntu,
    Debian,
    Rocky,
    Fedora,
}

impl Distro {
    fn as_str(self) -> &'static str {
        match self {
            Distro::Ubuntu => "ubuntu",
            Distro::Debian => "debian",
            Distro::Rocky => "rocky",
            Distro::Fedora => "fedora",
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
        // CLAUDE.md) strips `/` from `release`, preventing
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
            Distro::Fedora => format!(
                "Fedora-Cloud-Base-Generic-{}.x86_64.qcow2",
                Self::sanitize(release)
            ),
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

// A CLI enum parsed once per invocation, not a hot path — the same
// justification the sibling command enums already carry.
#[allow(clippy::large_enum_variant)]
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
    Push {
        name: String,
        /// Destination. Omit it for an OFFICIAL repository: which one is
        /// decided by what the image says about itself (an appliance goes to
        /// the appliances repo, a Kubernetes golden to the k8s one). Give it
        /// to publish anywhere else.
        target: Option<String>,
    },
    /// Convert a VM disk between `qcow2` (default, per-VM overlay) and `raw`
    /// — flattened (no backing file) either way, ready to boot on either
    /// backend (libvirt/QEMU and Cloud Hypervisor already share this same
    /// pair of formats; there is no separate "per-hypervisor" format here).
    Convert {
        /// A local VM image name (`image vm ls`) or a literal `.qcow2`/`.raw` path.
        source: String,
        /// Target format.
        #[arg(long = "to", value_enum)]
        to: ConvertFormat,
        /// Destination file (default: alongside the source, with the new extension).
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,
        /// Compress the output. Only `qcow2` and `vmdk` can — refused for the
        /// others rather than handed to `qemu-img` to fail on.
        #[arg(long)]
        compress: bool,
    },
    /// Register an existing disk image under a name, so `vm create --disk
    /// <name>` and `image vm push` can use it.
    ///
    /// The counterpart to `build` for a disk this engine did not produce: a
    /// vendor appliance installed from its own ISO, or an image exported from
    /// somewhere else. The file is copied into the store as-is — nothing is
    /// booted, nothing is inspected.
    Import(ImportArgs),
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
    /// Scaffold a `VMfile` (and a cloud-init) for building your own image.
    ///
    /// Writes a recipe that BUILDS AS WRITTEN — a scaffold nobody can run is
    /// documentation that lies. Delete what you do not need.
    Init {
        /// Name to use in the scaffold (image tag, hostname, account).
        #[arg(default_value = "myimage")]
        name: String,
        /// Where to write it (default: the current directory).
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
        /// Overwrite an existing `VMfile`.
        #[arg(long)]
        force: bool,
    },
    /// Build a VM image: from a `VMfile` when there is one, otherwise the
    /// built-in golden recipe (Ubuntu cloud image + kubeadm/kubelet/kubectl +
    /// `delonix-cri`), via `virt-customize`.
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        /// Build from a `VMfile` instead of the built-in golden recipe.
        ///
        /// With no `-f`, a `VMfile` in the context directory is used if there
        /// is one — same rule `delonix build` follows for `Delonixfile`. The
        /// flags below (`--distro`, `--k8s-version`, …) belong to the golden
        /// recipe and are REFUSED with a VMfile, which describes all of that
        /// itself; accepting and ignoring them is the failure this repo names
        /// as its worst.
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Build context — the directory `COPY` reads from (default: `.`).
        #[arg(default_value = ".")]
        context: PathBuf,
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
        /// Fedora release AND build, as shown on Fedora's download page
        /// (e.g. `42-1.1`) — only used with `--distro fedora`. The build
        /// number is not derivable from the release, and Fedora's redirector
        /// offers no listing to look it up, so it is asked for rather than
        /// guessed.
        #[arg(long, default_value = "42-1.1")]
        fedora_release: String,
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
        /// Give the guest network access during `RUN` — VMfile builds only.
        /// The golden recipe already decides this with `--offline`.
        #[arg(long)]
        network: bool,
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
        VmImageCmd::Push { name, target } => cmd_push(&store, &name, target.as_deref()),
        VmImageCmd::Convert {
            source,
            to,
            output,
            compress,
        } => cmd_convert(&store, &source, to, output, compress),
        VmImageCmd::Import(args) => cmd_import(&store, args),
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
            // A reference WITHOUT a registry (`opnsense:26.1`) is looked up in
            // the official catalogue; one with a `/` is used verbatim, so the
            // argument keeps meaning "somewhere of your own".
            let src = match source {
                Some(s) => resolve_official_ref(&s)?,
                None => default_pull_source(no_k8s).to_string(),
            };
            cmd_pull(&store, &src, name)
        }
        VmImageCmd::LsRemote { source, no_k8s } => match source {
            // A bare name resolves against the official catalogue, so
            // `ls-remote appliances` needs no URL.
            Some(s) => cmd_ls_remote(&resolve_official_ref(&s)?),
            // `--no-k8s` predates the catalogue and still means "the base
            // images"; kept working rather than removed.
            None if no_k8s => cmd_ls_remote(
                OFFICIAL_REPOS
                    .iter()
                    .find(|r| r.key == "base")
                    .expect("base is in the table")
                    .repo,
            ),
            None => cmd_ls_remote_official(),
        },
        VmImageCmd::Init { name, dir, force } => cmd_init(&name, dir, force),
        VmImageCmd::Build {
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
        } => {
            // A `VMfile` in the context beats the golden recipe, the same way a
            // `Delonixfile` beats a `Dockerfile` for `delonix build`. Explicit
            // `-f` always wins.
            let vmfile = file.or_else(|| {
                let p = context.join("VMfile");
                p.exists().then_some(p)
            });
            if let Some(path) = vmfile {
                // The golden-recipe flags describe a recipe the VMfile replaces.
                // Silently ignoring them would let someone believe their
                // `--k8s-version` took effect.
                let golden: &[(&str, bool)] = &[
                    ("--k8s-version", k8s_version.is_some()),
                    ("--extra-package", !extra_packages.is_empty()),
                    ("--extra-run", !extra_run.is_empty()),
                    ("--offline", offline),
                    ("--no-k8s", no_k8s),
                    ("--cri-bin", cri_bin.is_some()),
                    ("--delonix-bin", delonix_bin.is_some()),
                ];
                let used: Vec<&str> = golden
                    .iter()
                    .filter(|(_, on)| *on)
                    .map(|(n, _)| *n)
                    .collect();
                if !used.is_empty() {
                    return Err(Error::Invalid(super::po::tf(
                        "{flags} belong to the built-in golden recipe and mean nothing with a VMfile — the VMfile describes all of that itself",
                        &[("flags", &used.join(", "))],
                    ).to_string()));
                }
                return super::vmfile::build(&store, &path, &context, &tag, !no_compress, network);
            }
            if network {
                return Err(Error::Invalid(super::po::t(
                    "`--network` is for VMfile builds; the golden recipe decides it with `--offline`",
                )
                .to_string()));
            }
            cmd_build(
                &store,
                &tag,
                distro,
                &ubuntu_release,
                &debian_release,
                &rocky_release,
                &fedora_release,
                k8s_version,
                extra_packages,
                extra_run,
                cri_bin,
                !no_compress,
                offline,
                no_k8s,
                delonix_bin,
            )
        }
    }
}

/// `vm init` — writes the scaffold.
fn cmd_init(name: &str, dir: Option<PathBuf>, force: bool) -> Result<()> {
    let dir = super::vmfile::init_dir(dir);
    std::fs::create_dir_all(&dir).map_err(|e| Error::Invalid(format!("{}: {e}", dir.display())))?;
    let vmfile = dir.join("VMfile");
    if vmfile.exists() && !force {
        return Err(Error::Invalid(super::po::tf(
            "{path} already exists — use --force to overwrite",
            &[("path", &vmfile.display().to_string())],
        )));
    }
    std::fs::write(&vmfile, super::vmfile::scaffold(name))
        .map_err(|e| Error::Invalid(format!("{}: {e}", vmfile.display())))?;
    let ci_dir = dir.join("cloud-init");
    std::fs::create_dir_all(&ci_dir)
        .map_err(|e| Error::Invalid(format!("{}: {e}", ci_dir.display())))?;
    let ci = ci_dir.join("user-data.yaml");
    if !ci.exists() || force {
        std::fs::write(&ci, super::vmfile::scaffold_cloud_init(name))
            .map_err(|e| Error::Invalid(format!("{}: {e}", ci.display())))?;
    }
    println!("{}", vmfile.display());
    println!("{}", ci.display());
    println!(
        "\n{}",
        super::po::tf(
            "Next: `delonix vm build -t {name}:1.0 {dir}` then `delonix vm create dev --disk-image {name}:1.0`",
            &[("name", name), ("dir", &dir.display().to_string())],
        )
    );
    Ok(())
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
    /// Whether the guest runs cloud-init; `null` for images registered before
    /// the field existed. Automation reads this to know whether passing
    /// `--hostname`/`--ssh-key` to `vm create` would be refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    cloud_init: Option<bool>,
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
                cloud_init: img.cloud_init,
            })
            .collect();
        return output::print_json(&rows);
    }
    // TYPE and DEFAULTS answer what the reader actually needs before running
    // `vm create`: whether this image configures itself (and therefore refuses
    // `--ssh-key`), and what it wants for vCPU/memory. KERNEL/K8S stay — they
    // are filled whenever they are known, which now includes a `vm pull` (the
    // manifest annotations carry them) and an `import --kernel-version`.
    let mut t = output::Table::new(&[
        "NAME", "DISTRO", "TYPE", "KERNEL", "K8S", "DEFAULTS", "CREATED", "SIZE",
    ])
    .right_align(7);
    for img in store.list()? {
        let distro = distro_label(&img);
        let type_label = image_type_label(match img.cloud_init {
            Some(true) => Some("true"),
            Some(false) => Some("false"),
            None => None,
        });
        let defaults = defaults_label(&img);
        t.row(vec![
            img.name,
            distro,
            type_label,
            img.kernel_version.as_deref().unwrap_or("-").to_string(),
            img.k8s_version.as_deref().unwrap_or("-").to_string(),
            defaults,
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
    // qcow2 blob) — on a pulled image they stay `None`. See the known gap in CLAUDE.md.
    let distro = distro_label(img);
    d.field("Distro", if distro == "-" { "<unknown>" } else { &distro });
    d.field(
        "Kernel",
        img.kernel_version.as_deref().unwrap_or("<unknown>"),
    );
    d.field("K8s", img.k8s_version.as_deref().unwrap_or("<unknown>"));
    // Worth a line of its own: this is what makes `vm create` skip the seed
    // and refuse `--hostname`/`--ssh-key`, and without it that refusal reads
    // as arbitrary.
    d.field(
        "Cloud-init",
        match img.cloud_init {
            Some(true) => super::po::t("yes"),
            Some(false) => super::po::t("no (appliance — configure it on first boot)"),
            None => super::po::t("<unknown> (assumed yes)"),
        },
    );
    d.field_opt("Default vCPUs", img.default_vcpus.map(|v| v.to_string()));
    d.field_opt("Default memory", img.default_memory.clone());
    d.field_opt("Default backend", img.default_backend.clone());
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

/// A repository the project itself publishes. Having these as DATA rather than
/// as a pair of constants behind a boolean is what lets `ls-remote`, `pull` and
/// `push` all agree on what "official" means without any of them taking an
/// argument — and what makes adding a fourth one a single row instead of a new
/// flag.
///
/// The old shape was `OFFICIAL_VM_IMAGE`/`OFFICIAL_VM_BASE_IMAGE` selected by
/// `--no-k8s`, which stops working the moment there is a third repository: a
/// boolean cannot name three things.
pub(crate) struct OfficialRepo {
    /// Short name for the CLI and for messages (`k8s`, `base`, `appliances`).
    pub key: &'static str,
    /// `host/namespace/name`, never with a tag.
    pub repo: &'static str,
    /// What a bare `pull` from this repository should fetch. `None` for a
    /// repository with no single sensible default — `appliances` holds
    /// unrelated products, and picking one of them for the user would be a
    /// guess dressed as a default.
    pub default_tag: Option<&'static str>,
    /// One line, for the listing header.
    pub what: &'static str,
}

pub(crate) const OFFICIAL_REPOS: &[OfficialRepo] = &[
    OfficialRepo {
        key: "k8s",
        repo: "ghcr.io/angolardevops/delonix-vm-k8s",
        default_tag: Some("1.34"),
        what: "golden Kubernetes node (kubeadm/kubelet/kubectl + delonix-cri)",
    },
    OfficialRepo {
        key: "base",
        repo: "ghcr.io/angolardevops/delonix-vm-base",
        default_tag: Some("ubuntu-24.04"),
        what: "base OS images (delonix engine, rootless-ready, no Kubernetes)",
    },
    OfficialRepo {
        key: "appliances",
        repo: "ghcr.io/angolardevops/delonix-vm-appliances",
        default_tag: None,
        what: "vendor appliances (OPNsense, Proxmox, TrueNAS) — no cloud-init",
    },
];

/// The official repository a LOCAL image belongs in, decided from what the
/// image says about itself rather than from a flag: an appliance goes with the
/// appliances, an image carrying a Kubernetes version with the golden nodes,
/// anything else with the base images.
///
/// This is what lets `image vm push <name>` work with no destination. `None`
/// when the metadata does not settle it — and then the caller must be told to
/// name a target instead of having one guessed.
pub(crate) fn official_repo_for(img: &VmImage) -> Option<&'static OfficialRepo> {
    let key = if img.cloud_init == Some(false) {
        "appliances"
    } else if img.k8s_version.is_some() {
        "k8s"
    } else if img.distro.is_some() {
        "base"
    } else {
        return None;
    };
    OFFICIAL_REPOS.iter().find(|r| r.key == key)
}

/// Resolves a reference that names no registry (`opnsense:26.1`) against the
/// official repositories, so a pull of something the project publishes needs
/// no URL. A reference WITH a `/` is a real one and is returned untouched —
/// the parameter keeps meaning "a repository of your own".
pub(crate) fn resolve_official_ref_local(reference: &str) -> Option<String> {
    if reference.contains('/') {
        return Some(reference.to_string());
    }
    let name = reference.split(':').next().unwrap_or(reference);
    for r in OFFICIAL_REPOS {
        if r.key == name {
            return Some(match r.default_tag {
                Some(tag) => format!(
                    "{}:{}",
                    r.repo,
                    reference.split_once(':').map_or(tag, |(_, t)| t)
                ),
                None => r.repo.to_string(),
            });
        }
    }
    None
}

/// The tag a bare reference is asking for. `<product>:<version>` is spelled
/// `<product>-<version>` in the official repositories, so both forms name the
/// same artifact; anything else is already a tag.
pub(crate) fn official_tag_candidate(reference: &str) -> String {
    match reference.split_once(':') {
        Some((prod, ver)) => format!("{prod}-{ver}"),
        None => reference.to_string(),
    }
}

/// Resolves a reference to a full repository ref, asking the official
/// repositories when the answer cannot be known locally.
///
/// [`resolve_official_ref_local`] settles what is decidable offline: anything
/// with a `/` already names a repository, and a repository KEY (`k8s`, `base`,
/// `appliances`) names one directly. Everything else is a bare tag, and the
/// only honest way to place it is to look — the tag namespaces are not
/// partitioned by any rule this code can derive. `rocky-9` lives in the base
/// repository and `proxmox-ve-9.1` in the appliances one, and nothing in either
/// string says so.
///
/// This used to fall back to the appliances repository for every bare tag. That
/// is what turned `delonix vm pull rocky-9` into `no such image
/// …/delonix-vm-appliances:rocky-9` — a 404 naming a repository the user had
/// never typed, about an image that was published and public the whole time.
/// The fallback was right when appliances were the only repository with product
/// tags, and started lying the moment the base images shipped.
pub(crate) fn resolve_official_ref(reference: &str) -> Result<String> {
    if let Some(local) = resolve_official_ref_local(reference) {
        return Ok(local);
    }
    let want = official_tag_candidate(reference);
    let root = state_root();
    let mut found: Vec<&str> = Vec::new();
    let mut unreachable: Vec<String> = Vec::new();
    for r in OFFICIAL_REPOS {
        match delonix_image::registry::list_remote_tags(&root, r.repo) {
            Ok(tags) if tags.iter().any(|t| t == &want) => found.push(r.repo),
            Ok(_) => {}
            // A repository we could not read is NOT a repository without the
            // tag: a private package answers 404 exactly like a missing one.
            // Reporting "nowhere" while one of the three never answered would
            // be the same class of lie this function exists to remove.
            Err(e) => unreachable.push(format!("{}: {e}", r.repo)),
        }
    }
    match found.as_slice() {
        [one] => Ok(format!("{one}:{want}")),
        [] => {
            let mut msg = super::po::tf(
                "no official repository has the tag '{tag}' — run `delonix image vm ls-remote` to see what is published",
                &[("tag", &want)],
            );
            for u in &unreachable {
                msg.push_str(&format!("\n  unreachable — {u}"));
            }
            Err(Error::Invalid(msg))
        }
        many => Err(Error::Invalid(super::po::tf(
            "the tag '{tag}' exists in more than one official repository ({repos}) — name the one you mean",
            &[("tag", &want), ("repos", &many.join(", "))],
        ))),
    }
}

/// Delonix's OFFICIAL golden VM image (Ubuntu 24.04 + kubeadm/kubelet/
/// kubectl + delonix-cri as a systemd service) — published and validated with
/// a byte-identical round-trip; see CLAUDE.md, section "Golden VM image".
pub(crate) const OFFICIAL_VM_IMAGE: &str = "ghcr.io/angolardevops/delonix-vm-k8s:1.34";

/// Golden VM image with NO Kubernetes — just the `delonix` engine binary and
/// rootless prerequisites (see `rootless_customization_steps`). Selected by
/// `Pull`/`LsRemote --no-k8s` when no explicit `source` is given.
pub(crate) const OFFICIAL_VM_BASE_IMAGE: &str =
    "ghcr.io/angolardevops/delonix-vm-base:ubuntu-24.04";

/// Downloads a cloud image from an ARBITRARY URL, for `FROM https://…`.
///
/// Verified when the publisher makes it possible: a sibling `<url>.sha256` (the
/// convention almost everyone follows) is fetched and checked. When there is
/// none, the download proceeds over TLS and **says so** — because the
/// alternative is a build that silently trusts whatever answered, and someone
/// pointing `FROM` at their own bucket deserves to know which of the two they
/// got.
pub(crate) fn download_url_base(store: &VmImageStore, url: &str) -> Result<PathBuf> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(Error::Invalid(format!("not an absolute URL: {url}")));
    }
    if url.starts_with("http://") {
        eprintln!(
            "{} {}",
            super::po::t("warning:"),
            super::po::t("plain http — the image is downloaded without any transport protection")
        );
    }
    // Cache key from the URL, not from its last path segment: two different
    // buckets publish `noble-server-cloudimg-amd64.img` and they are not the
    // same file.
    let key = format!("url-{}", hex_sha256(url.as_bytes()));
    let cached = store.base_cache_path(Distro::Ubuntu, &key);
    if cached.exists() {
        return Ok(cached);
    }
    if let Some(parent) = cached.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = cached.with_extension("part");
    eprintln!("{}", super::po::tf("downloading {url}...", &[("url", url)]));
    stream_download(url, &tmp)?;

    let sums_url = format!("{url}.sha256");
    let sums = std::env::temp_dir().join(format!("delonix-url-sha-{}", std::process::id()));
    match stream_download(&sums_url, &sums) {
        Ok(()) => {
            let want = std::fs::read_to_string(&sums)
                .unwrap_or_default()
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let _ = std::fs::remove_file(&sums);
            let got = hex_sha256_file(&tmp)?;
            if want.is_empty() || want != got {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Invalid(super::po::tf(
                    "{url}: checksum mismatch (expected {want}, got {got}) — refusing the image",
                    &[("url", url), ("want", &want), ("got", &got)],
                )));
            }
            eprintln!("{}", super::po::t("checksum verified against <url>.sha256"));
        }
        Err(_) => {
            let _ = std::fs::remove_file(&sums);
            eprintln!(
                "{} {}",
                super::po::t("warning:"),
                super::po::tf(
                    "no {sums} published — the image is trusted on TLS alone. Publish one next to it to have this verified.",
                    &[("sums", &sums_url)],
                )
            );
        }
    }
    std::fs::rename(&tmp, &cached)?;
    Ok(cached)
}

/// Dispatches to the right per-distro downloader.
///
/// Each publisher does checksums differently — Ubuntu ships GNU `SHA256SUMS`,
/// Debian only `SHA512SUMS`, Rocky a per-file BSD-format `.CHECKSUM` — and that
/// knowledge already lives in the three functions below. This is the one place
/// that picks between them.
pub(crate) fn download_base(
    store: &VmImageStore,
    distro: Distro,
    release: &str,
) -> Result<PathBuf> {
    match distro {
        Distro::Ubuntu => download_ubuntu_base(store, release),
        Distro::Debian => download_debian_base(store, release),
        Distro::Rocky => download_rocky_base(store, release),
        Distro::Fedora => download_fedora_base(store, release),
    }
}

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

/// The tag an image takes in its official repository. One repository holds
/// several products, so the local `<product>:<version>` becomes
/// `<product>-<version>` — which is also what `resolve_official_ref` undoes on
/// the way back, so `push` and `pull` agree without either knowing about the
/// other.
pub(crate) fn official_tag_for(img: &VmImage, local_name: &str) -> String {
    // `delonix-vm-base:ubuntu-24.04` publishes as `ubuntu-24.04`: the
    // repository already carries the family, repeating it in the tag would
    // read as `delonix-vm-base:delonix-vm-base-ubuntu-24.04`.
    if let Some(rest) = local_name.strip_prefix("delonix-vm-base:") {
        return rest.to_string();
    }
    if let Some(rest) = local_name.strip_prefix("delonix-vm-k8s:") {
        return rest.to_string();
    }
    match local_name.split_once(':') {
        Some((prod, ver)) => format!("{prod}-{ver}"),
        None => match (&img.distro, &img.ubuntu_release) {
            (Some(d), Some(r)) => format!("{d}-{r}"),
            _ => local_name.to_string(),
        },
    }
}

pub(crate) fn cmd_push(store: &VmImageStore, name: &str, target: Option<&str>) -> Result<()> {
    let img = store.get(name)?;
    // With no destination, publish where this image belongs — decided from the
    // image's own metadata, not from a flag the caller has to remember. A tag
    // that is not a valid OCI reference is rewritten the way the appliances
    // repository names them (`opnsense:26.1` → `opnsense-26.1`), because one
    // repository holds several products.
    let target = match target {
        Some(t) => t.to_string(),
        None => {
            let repo = official_repo_for(&img).ok_or_else(|| {
                Error::Invalid(super::po::tf(
                    "{name}: cannot tell which official repository this belongs in (no distro, \
                     no k8s version, not marked as an appliance) — name a destination explicitly",
                    &[("name", name)],
                ))
            })?;
            let tag = official_tag_for(&img, name);
            let dest = format!("{}:{}", repo.repo, tag);
            eprintln!(
                "{}",
                super::po::tf(
                    "publishing to the official repository: {dest}",
                    &[("dest", &dest)]
                )
            );
            dest
        }
    };
    let target = target.as_str();
    let data = std::fs::read(store.qcow2_path(name)).map_err(|e| {
        Error::Invalid(format!(
            "{} '{name}': {e}",
            super::po::t("could not read the qcow2 of")
        ))
    })?;
    let digest = delonix_image::registry::push_oci_artifact_with_annotations(
        &state_root(),
        target,
        VM_IMAGE_MEDIA_TYPE,
        &data,
        &annotations_of(&img),
    )?;
    println!("{digest}");
    Ok(())
}

/// Arguments of `image vm import`, defined ONCE and `flatten`ed into all
/// three entry points (`vm import`, `image vm import`, `image --vm import`).
/// Spelling them out per entry point is how those three drift apart — a flag
/// that works in one place and not another is worse than no flag.
#[derive(clap::Args, Clone, Debug)]
pub struct ImportArgs {
    /// Path to a `.qcow2` (or any format `qemu-img` reads — it is converted).
    pub source: PathBuf,
    /// Name to register it under (`image vm ls`).
    #[arg(short = 't', long = "tag")]
    pub tag: String,
    /// The guest does NOT run cloud-init: it is a self-configuring appliance
    /// (OPNsense, Proxmox, TrueNAS). `vm create` then skips the NoCloud seed
    /// instead of attaching one nothing in the guest reads.
    #[arg(long)]
    pub appliance: bool,
    /// What this is, for `image vm ls` (e.g. `opnsense`, `proxmox-ve`).
    #[arg(long)]
    pub distro: Option<String>,
    /// Version, for `image vm ls` (e.g. `26.1.2`).
    #[arg(long)]
    pub release: Option<String>,
    /// Recommended vCPUs, applied by `vm create` when `--vcpus` is absent.
    #[arg(long)]
    pub default_vcpus: Option<u32>,
    /// Recommended memory (e.g. `4G`), same rule as `--default-vcpus`.
    #[arg(long)]
    pub default_memory: Option<String>,
    /// Replace an image already registered under this name.
    #[arg(long)]
    pub force: bool,
    /// Store the qcow2 uncompressed (larger, but no decompression cost on
    /// backing-file reads at runtime) — same flag, same trade-off, as `build`.
    #[arg(long)]
    pub no_compress: bool,
    /// Kernel baked into the image (`uname -r` shape), for `image vm ls`.
    /// Omit to let it be probed from the image — which only works where
    /// libguestfs can read `/boot` (not FreeBSD, not root-on-ZFS).
    #[arg(long)]
    pub kernel_version: Option<String>,
}

/// `image vm import` — registers a disk this engine did not build.
///
/// Always goes through `qemu-img convert`, never a plain copy: the source may
/// be raw, or a qcow2 with a backing file (which would leave the store holding
/// a reference to a path outside it — a store entry that silently breaks the
/// day that file moves).
pub(crate) fn cmd_import(store: &VmImageStore, args: ImportArgs) -> Result<()> {
    let ImportArgs {
        source,
        tag,
        appliance,
        distro,
        release,
        default_vcpus,
        default_memory,
        force,
        no_compress,
        kernel_version,
    } = args;
    let (source, tag) = (source.as_path(), tag.as_str());
    if !source.exists() {
        return Err(Error::Invalid(super::po::tf(
            "no such file: {source}",
            &[("source", &source.display().to_string())],
        )));
    }
    let dest = store.qcow2_path(tag);
    if dest.exists() && !force {
        return Err(Error::Invalid(super::po::tf(
            "VM image '{tag}' already exists — pass --force to replace it",
            &[("tag", tag)],
        )));
    }
    eprintln!(
        "{}",
        super::po::tf(
            "importing {source} as '{tag}'...",
            &[("source", &source.display().to_string()), ("tag", tag)],
        )
    );
    let tmp = dest.with_extension("importing");
    let _ = std::fs::remove_file(&tmp);
    let mut argv: Vec<&str> = vec!["convert", "-O", "qcow2"];
    if !no_compress {
        // zstd, and compressed by default, for the same reason the golden
        // recipe settled on it: a store image is the read-only BACKING FILE of
        // every VM created from it, so decompression speed is what matters,
        // and `qemu-img convert` without `-c` would inflate an already
        // compressed source several-fold on the way in.
        argv.extend(["-c", "-o", "compression_type=zstd"]);
    }
    // `--` so a path starting with `-` stays a path.
    argv.push("--");
    let (src_s, tmp_s) = (source.to_string_lossy(), tmp.to_string_lossy());
    argv.push(&src_s);
    argv.push(&tmp_s);
    run_tool("qemu-img", &argv)?;
    // Rename only after the conversion succeeded: a failed import must not
    // leave a truncated image registered under a good name.
    std::fs::rename(&tmp, &dest)?;

    let data = std::fs::read(&dest)?;
    let img = VmImage {
        name: tag.to_string(),
        tag: tag.to_string(),
        digest: format!("sha256:{}", hex_sha256(&data)),
        size: data.len() as u64,
        ubuntu_release: release,
        k8s_version: None,
        created_unix: now_unix(),
        // Explicit wins; otherwise probe the image (best-effort — see
        // `detect_kernel_version`). Never a guess: unknown stays `None`.
        kernel_version: kernel_version.or_else(|| detect_kernel_version(&dest)),
        distro,
        default_vcpus,
        default_memory,
        default_backend: None,
        cloud_init: Some(!appliance),
    };
    store.save(&img)?;
    println!("{tag}");
    Ok(())
}

/// Manifest annotations carrying what the qcow2 blob cannot: the store's own
/// view of the image. Without these a `vm pull` lands an image with every
/// metadata field blank — a documented gap for `ubuntu_release`/`k8s_version`,
/// and a functional bug for `cloud_init`, which decides whether `vm create`
/// attaches a seed the guest may not be able to read.
///
/// Only fields that are actually known are emitted: an absent annotation and
/// an annotation saying "unknown" are different claims, and the pull side
/// leaves what it was not told alone.
pub(crate) fn annotations_of(img: &VmImage) -> std::collections::BTreeMap<String, String> {
    let mut a = std::collections::BTreeMap::new();
    let mut put = |k: &str, v: String| {
        a.insert(format!("io.delonix.vmimage.{k}"), v);
    };
    if let Some(v) = &img.distro {
        put("distro", v.clone());
    }
    if let Some(v) = &img.ubuntu_release {
        put("release", v.clone());
    }
    if let Some(v) = &img.k8s_version {
        put("k8s-version", v.clone());
    }
    if let Some(v) = &img.kernel_version {
        put("kernel-version", v.clone());
    }
    if let Some(v) = img.default_vcpus {
        put("default-vcpus", v.to_string());
    }
    if let Some(v) = &img.default_memory {
        put("default-memory", v.clone());
    }
    if let Some(v) = &img.default_backend {
        put("default-backend", v.clone());
    }
    if let Some(v) = img.cloud_init {
        put("cloud-init", v.to_string());
    }
    a
}

/// Inverse of [`annotations_of`], applied to the metadata a `vm pull` just
/// built from the blob alone.
///
/// A malformed value is IGNORED rather than fatal: a bad `default-vcpus`
/// should not make an otherwise good image unusable, and the field's own
/// absence already has well-defined behaviour. `cloud-init` is the one case
/// where a wrong reading matters, so only the two exact strings count.
pub(crate) fn apply_pulled_annotations(
    mut img: VmImage,
    a: &std::collections::BTreeMap<String, String>,
) -> VmImage {
    let get = |k: &str| {
        a.get(&format!("io.delonix.vmimage.{k}"))
            .map(|s| s.as_str())
    };
    img.distro = get("distro").map(str::to_string).or(img.distro);
    img.ubuntu_release = get("release").map(str::to_string).or(img.ubuntu_release);
    img.k8s_version = get("k8s-version").map(str::to_string).or(img.k8s_version);
    img.kernel_version = get("kernel-version")
        .map(str::to_string)
        .or(img.kernel_version);
    img.default_vcpus = get("default-vcpus")
        .and_then(|v| v.parse().ok())
        .or(img.default_vcpus);
    img.default_memory = get("default-memory")
        .map(str::to_string)
        .or(img.default_memory);
    img.default_backend = get("default-backend")
        .map(str::to_string)
        .or(img.default_backend);
    img.cloud_init = match get("cloud-init") {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => img.cloud_init,
    };
    img
}

/// `image vm convert` — flattens/converts a disk to the format another
/// ecosystem imports (`vmdk` for VMware, `vdi` for VirtualBox, `vhdx`/`vhd`
/// for Hyper-V and Azure, plus this engine's own `qcow2`/`raw`), using
/// `qemu-img convert` (same tool `cmd_build`/`vmfile::build` already use
/// to flatten a base image). `source` is tried as a local VM image name
/// first (so `convert my-image --to raw` works without knowing the qcow2's
/// on-disk path), falling back to a literal path — never an error to pass a
/// path that happens to collide with no store entry.
pub(crate) fn cmd_convert(
    store: &VmImageStore,
    source: &str,
    to: ConvertFormat,
    output: Option<PathBuf>,
    compress: bool,
) -> Result<()> {
    // Refused HERE, not handed to `qemu-img` to reject: the tool's own error for
    // `-c` on a format that cannot compress says nothing about which formats
    // can, and the flag was offered by this CLI in the first place.
    if compress && !to.supports_compression() {
        return Err(Error::Invalid(super::po::tf(
            "{fmt} cannot be compressed — only qcow2 and vmdk can",
            &[("fmt", to.extension())],
        )));
    }
    let src_path = {
        let by_name = store.qcow2_path(source);
        if by_name.exists() {
            by_name
        } else {
            PathBuf::from(source)
        }
    };
    if !src_path.exists() {
        return Err(Error::Invalid(super::po::tf(
            "no such local VM image or file: {source} (see `delonix image vm ls`)",
            &[("source", source)],
        )));
    }
    let dest = output.unwrap_or_else(|| {
        let stem = src_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("image");
        src_path.with_file_name(format!("{stem}.{}", to.extension()))
    });
    if dest == src_path {
        return Err(Error::Invalid(
            super::po::t(
                "destination is the same as the source — pass `-o <file>` to convert in place",
            )
            .to_string(),
        ));
    }
    eprintln!(
        "{}",
        super::po::tf(
            "converting {src} to {fmt} → {dst}...",
            &[
                ("src", &src_path.display().to_string()),
                ("fmt", to.as_str()),
                ("dst", &dest.display().to_string()),
            ],
        )
    );
    let mut args: Vec<&str> = vec!["convert", "-O", to.as_str()];
    if compress {
        args.push("-c");
    }
    let (src, dst) = (src_path.to_string_lossy(), dest.to_string_lossy());
    // `--` so a source path starting with `-` is a path, not an option:
    // `source` here may be a literal path straight from the caller.
    args.extend(["--", &src, &dst]);
    run_tool("qemu-img", &args)?;
    let _unused = ();
    println!("{}", dest.display());
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
    let (data, annotations) = delonix_image::registry::pull_oci_artifact_with_meta(
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
        // The OCI artifact only carries the qcow2 blob, not build metadata —
        // same known gap as `ubuntu_release`/`k8s_version` above.
        default_vcpus: None,
        default_memory: None,
        default_backend: None,
        // Filled in below from the manifest annotations when the publisher
        // set them. Left `None` (= assume cloud-init, the historical
        // behaviour) for an artifact that carries no annotations.
        cloud_init: None,
    };
    let img = apply_pulled_annotations(img, &annotations);
    store.save(&img)?;
    println!("{name}");
    Ok(())
}

/// `ls-remote` with no argument: every official repository, one after the
/// other. Listing only one of them (and not saying which) is what made a user
/// who had just published to another conclude the push had failed.
pub(crate) fn cmd_ls_remote_official() -> Result<()> {
    let mut failed = false;
    for (i, r) in OFFICIAL_REPOS.iter().enumerate() {
        if i > 0 {
            println!();
        }
        eprintln!(
            "{}",
            super::po::tf("{key} — {what}", &[("key", r.key), ("what", r.what)],)
        );
        // One unreachable repository must not hide the others: a brand-new
        // package is private until someone flips it, and 404 is what that
        // looks like from here.
        if let Err(e) = cmd_ls_remote(r.repo) {
            eprintln!("  {e}");
            failed = true;
        }
    }
    if failed {
        eprintln!(
            "{}",
            super::po::t(
                "note: a repository that reports \"not found\" may simply be private to these credentials"
            )
        );
    }
    Ok(())
}

pub(crate) fn cmd_ls_remote(source: &str) -> Result<()> {
    let root = state_root();
    // Say WHICH repository is being listed. With no argument this command
    // lists a default one, and a reader who has just published elsewhere sees
    // a short list of tags that are not theirs and concludes the push failed —
    // which is exactly what happened. The tags alone do not identify their
    // origin, so the header has to.
    // Without the tag: the default source carries one (`…:1.34`), and printing
    // it here would suggest the listing is scoped to that tag when it is the
    // whole repository. Cut only a `:` that comes AFTER the last `/`, so a
    // `host:port/repo` keeps its port.
    let shown = match source.rfind('/') {
        Some(slash) => match source[slash..].find(':') {
            Some(colon) => &source[..slash + colon],
            None => source,
        },
        None => source.split(':').next().unwrap_or(source),
    };
    eprintln!(
        "{}",
        super::po::tf("repository: {source}", &[("source", shown)])
    );
    let mut tags = delonix_image::registry::list_remote_tags(&root, source)?;
    tags.sort();
    // A bare list of tags does not answer the question the reader has, which is
    // "which of these do I want" — so each tag's MANIFEST is read (one GET, no
    // blob transfer) for its size and for the annotations `image vm push`
    // stamps: distro/release, and whether the guest runs cloud-init.
    //
    // A tag whose manifest cannot be read still gets its row, with `-` in the
    // columns: a listing that hides a tag because one HTTP call failed would be
    // worse than one that admits it does not know.
    let mut t = output::Table::new(&["TAG", "DISTRO", "TYPE", "SIZE"]).right_align(3);
    for tag in tags.iter_mut() {
        let tag = std::mem::take(tag);
        match delonix_image::registry::describe_remote_artifact(&root, source, &tag) {
            Ok(a) => {
                let get = |k: &str| {
                    a.annotations
                        .get(&format!("io.delonix.vmimage.{k}"))
                        .map(|s| s.as_str())
                };
                let distro = match (get("distro"), get("release")) {
                    (Some(d), Some(r)) => format!("{d}/{r}"),
                    (Some(d), None) => d.to_string(),
                    (None, Some(r)) => r.to_string(),
                    (None, None) => "-".to_string(),
                };
                t.row(vec![
                    tag,
                    distro,
                    image_type_label(get("cloud-init")),
                    fmt_size(a.size),
                ]);
            }
            Err(_) => t.row(vec![tag, "-".into(), "-".into(), "-".into()]),
        }
    }
    t.print();
    Ok(())
}

/// Kernel baked into a disk image this engine did NOT build, read without
/// booting it.
///
/// `cmd_build` reads `/etc/delonix-kernel-version`, a file it writes itself —
/// useless for an imported image. This lists `/boot` instead and takes the
/// highest `vmlinuz-<version>` it finds, which is what the guest boots by
/// default.
///
/// Best-effort by design, and quiet when it fails: an appliance may be FreeBSD
/// (OPNsense), or root on ZFS (TrueNAS), where libguestfs cannot see `/boot` at
/// all. An empty KERNEL column is honest; failing an import over a display
/// field would not be.
fn detect_kernel_version(qcow2: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("virt-ls")
        .args(["-a", &qcow2.to_string_lossy(), "/boot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().strip_prefix("vmlinuz-"))
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .max()
}

/// `2cpu/2G` — what `vm create` will use when `--vcpus`/`--memory` are absent.
/// `-` when the image recommends nothing (the engine then falls back to 1/1G).
fn defaults_label(img: &VmImage) -> String {
    match (img.default_vcpus, img.default_memory.as_deref()) {
        (Some(c), Some(m)) => format!("{c}cpu/{m}"),
        (Some(c), None) => format!("{c}cpu"),
        (None, Some(m)) => m.to_string(),
        (None, None) => "-".to_string(),
    }
}

/// How an image gets configured on first boot, in one word — the difference
/// that decides whether `vm create` seeds it and whether `--ssh-key` works.
fn image_type_label(cloud_init: Option<&str>) -> String {
    match cloud_init {
        Some("false") => super::po::t("appliance").to_string(),
        Some("true") => "cloud-init".to_string(),
        _ => "-".to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_build(
    store: &VmImageStore,
    tag: &str,
    distro: Distro,
    ubuntu_release: &str,
    debian_release: &str,
    rocky_release: &str,
    fedora_release: &str,
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
    if matches!(distro, Distro::Rocky | Distro::Fedora) && !no_k8s {
        return Err(Error::Invalid(super::po::tf(
            "--distro {distro} only supports --no-k8s for now (the k8s path needs the \
                 pkgs.k8s.io RPM repository, not implemented yet)",
            &[("distro", distro.as_str())],
        )));
    }
    // `k8s_version` goes into a `format!` that becomes a `virt-customize --run-command`
    // (via `k8s_recipes::k8s_host_recipes`) — validating here closes the same security
    // finding as `cmd::cluster::valid_version` (the embedded apt repository must not
    // contain shell metacharacters). Audit finding, see CLAUDE.md.
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
        Distro::Fedora => fedora_release,
    };
    let base = match distro {
        Distro::Ubuntu => download_ubuntu_base(store, ubuntu_release)?,
        Distro::Debian => download_debian_base(store, debian_release)?,
        Distro::Rocky => download_rocky_base(store, rocky_release)?,
        Distro::Fedora => download_fedora_base(store, fedora_release)?,
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
        // lease that never arrives on hosts where passt is broken (see CLAUDE.md).
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
        // The built-in golden recipe has no VMfile to carry a recommendation
        // from — only a `VMfile` build (`vmfile::build`) sets these.
        default_vcpus: None,
        default_memory: None,
        default_backend: None,
        // The golden recipe deliberately leaves cloud-init enabled in the
        // image so each VM's first boot applies its own hostname/SSH keys.
        cloud_init: Some(true),
    };
    store.save(&img)?;
    println!("{tag}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Download + verification of the Ubuntu cloud image
// ---------------------------------------------------------------------------

pub(crate) fn download_ubuntu_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
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
pub(crate) fn download_debian_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
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

/// Fedora Cloud base image.
///
/// Fedora's artifact name carries a BUILD number that the release number does
/// not determine (`42` ships as `42-1.1`), and its download redirector serves
/// no directory listing — so there is nothing to derive it from. Rather than
/// guess, `--fedora-release` takes the full `<release>-<build>` exactly as
/// Fedora's own download page shows it, and says so when given anything else.
/// Same principle as the Proxmox ISO inputs in the appliance workflow: an
/// unverifiable guess presented as a fact is worse than asking.
///
/// The CHECKSUM is BSD-style (`SHA256 (file) = hash`), the same shape Rocky
/// uses — `parse_bsd_checksum` is shared, not duplicated.
pub(crate) fn download_fedora_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
    valid_fedora_release(release)?;
    let cached = store.base_cache_path(Distro::Fedora, release);
    if cached.exists() {
        return Ok(cached);
    }
    let major = release.split('-').next().unwrap_or(release);
    let img_name = format!("Fedora-Cloud-Base-Generic-{release}.x86_64.qcow2");
    let base_url = format!(
        "https://download.fedoraproject.org/pub/fedora/linux/releases/{major}/Cloud/x86_64/images"
    );
    let img_url = format!("{base_url}/{img_name}");
    let sums_url = format!("{base_url}/Fedora-Cloud-{release}-x86_64-CHECKSUM");

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

/// `<release>-<build>`, e.g. `42-1.1`. Both parts are numeric; the build may
/// have several dotted components. Rejecting a bare `42` here is the point:
/// it looks right and produces a 404 several hundred megabytes later.
fn valid_fedora_release(release: &str) -> Result<()> {
    let bad = || {
        Error::Invalid(super::po::tf(
            "--fedora-release '{release}' must be `<release>-<build>` as shown on Fedora's \
             download page (e.g. `42-1.1`) — the build number is not derivable from the release",
            &[("release", release)],
        ))
    };
    let (major, build) = release.split_once('-').ok_or_else(bad)?;
    if major.is_empty() || !major.chars().all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    if build.is_empty()
        || !build.chars().all(|c| c.is_ascii_digit() || c == '.')
        || build.starts_with('.')
        || build.ends_with('.')
    {
        return Err(bad());
    }
    Ok(())
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
pub(crate) fn download_rocky_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
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

/// Downloads `url` to `dest`, RESUMING a partial file and retrying a dropped
/// connection.
///
/// Measured here, and the reason this exists: a Rocky cloud image (646 MiB) died
/// 3.8 MiB in. The old version had no retry at all — one failed `read()` threw
/// away everything and the next run started from zero, so on a slow or flaky
/// link a build was a coin toss. Every caller verifies a checksum afterwards,
/// which is what makes resuming safe: bytes stitched from two ranges either add
/// up to the published hash or the download is discarded.
pub(crate) fn stream_download(url: &str, dest: &Path) -> Result<()> {
    const ATTEMPTS: u32 = 5;
    /// How long a sample must run before its rate means anything.
    const STALL_WINDOW_SECS: f64 = 20.0;
    /// Bounded: a link that is simply slow must not reconnect forever.
    const MAX_STALL_RECONNECTS: u32 = 3;
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(std::time::Duration::from_secs(3600))
        .build()
        .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("HTTP client"))))?;

    let mut last_err = String::new();
    let mut stalls: u32 = 0;
    // A reconnect for slowness is not a failed attempt — it is the same
    // transfer continuing on (hopefully) a better node, so it must not consume
    // the retry budget that exists for genuine errors.
    let mut attempt = 0u32;
    while attempt < ATTEMPTS + stalls {
        attempt += 1;
        // Where a previous attempt stopped. `dest` is the caller's `.download`
        // temp file, never the final cached image, so a leftover here is always
        // ours to continue.
        let have = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        let mut req = client.get(url);
        if have > 0 {
            req = req.header("Range", format!("bytes={have}-"));
        }
        let mut resp = match req.send() {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("{e}");
                continue;
            }
        };
        if !resp.status().is_success() {
            return Err(Error::Invalid(format!("GET {url}: HTTP {}", resp.status())));
        }
        // 206 means the server honoured the range and we append; anything else
        // (a mirror without range support, or a redirect to a fresh object)
        // means it is sending the whole thing again, so start over.
        let resuming = have > 0 && resp.status().as_u16() == 206;
        let mut file = if resuming {
            std::fs::OpenOptions::new().append(true).open(dest)?
        } else {
            std::fs::File::create(dest)?
        };
        let mut written: u64 = if resuming { have } else { 0 };
        if attempt > 1 || resuming {
            eprintln!(
                "{}",
                super::po::tf(
                    "resuming download at {have} bytes (attempt {attempt})...",
                    &[
                        ("have", &written.to_string()),
                        ("attempt", &attempt.to_string())
                    ],
                )
            );
        }
        let mut buf = [0u8; 1 << 20];
        let mut broke = false;
        // Stall detection. A CDN redirect can land the connection on a slow
        // node and keep it there: measured here, a resumed Rocky transfer sat
        // at 242 KiB/s while a FRESH request to the same URL got 1732 KiB/s —
        // 7x, same host, same second. Reconnecting used to mean throwing the
        // whole file away, so it was never worth it; with `Range` resume it
        // costs nothing, which is what makes this worth doing at all.
        //
        // The threshold is RELATIVE to the best window this transfer has seen,
        // not an absolute number: a uniformly slow link (satellite, rural) must
        // not reconnect forever chasing a speed it will never reach.
        let mut window_start = std::time::Instant::now();
        let mut window_bytes: u64 = 0;
        let mut best_rate: f64 = 0.0;
        loop {
            let n = match resp.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    // Keep what is on disk: the next attempt continues from here.
                    last_err = format!("{e}");
                    broke = true;
                    break;
                }
            };
            if n == 0 {
                break;
            }
            written += n as u64;
            // Ceiling on ACTUAL bytes read, not on the advertised
            // `Content-Length`: a hostile or misconfigured server can simply
            // keep streaming, and this writes to disk, so the failure is a full
            // filesystem — on a node that may be running everything else this
            // host serves. Deliberately far above any real cloud image, so it
            // only ever fires on something that is not an image.
            if written > MAX_DOWNLOAD_BYTES {
                let _ = std::fs::remove_file(dest);
                return Err(Error::Invalid(format!(
                    "GET {url}: aborted after {written} bytes (over the {MAX_DOWNLOAD_BYTES}-byte limit) \
                     — the server is streaming more than any expected image"
                )));
            }
            file.write_all(&buf[..n])?;

            window_bytes += n as u64;
            let elapsed = window_start.elapsed().as_secs_f64();
            if elapsed >= STALL_WINDOW_SECS {
                let rate = window_bytes as f64 / elapsed;
                if rate > best_rate {
                    best_rate = rate;
                } else if stalls < MAX_STALL_RECONNECTS && rate < best_rate / 4.0 {
                    stalls += 1;
                    eprintln!(
                        "{}",
                        super::po::tf(
                            "download slowed to {now} KiB/s (peak {peak}) — reconnecting to resume",
                            &[
                                ("now", &format!("{:.0}", rate / 1024.0)),
                                ("peak", &format!("{:.0}", best_rate / 1024.0)),
                            ],
                        )
                    );
                    broke = true;
                    break;
                }
                window_start = std::time::Instant::now();
                window_bytes = 0;
            }
        }
        if !broke {
            return Ok(());
        }
        file.flush()?;
    }
    Err(Error::Invalid(super::po::tf(
        "GET {url}: gave up after {attempts} attempts ({err}) — partial file kept, \
         a later run resumes from it",
        &[
            ("url", url),
            ("attempts", &ATTEMPTS.to_string()),
            ("err", &last_err),
        ],
    )))
}

/// Ceiling for a single downloaded artifact (cloud image, `.deb`, binary).
/// 32 GiB: an order of magnitude above the largest cloud image anyone ships,
/// so a legitimate download never approaches it.
const MAX_DOWNLOAD_BYTES: u64 = 32 * 1024 * 1024 * 1024;

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

pub(crate) fn hex_sha256(data: &[u8]) -> String {
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

pub(crate) fn now_unix() -> u64 {
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
        Distro::Rocky | Distro::Fedora => "wheel",
    };
    let bashrc_path = match distro {
        Distro::Ubuntu | Distro::Debian => "/etc/bash.bashrc",
        Distro::Rocky | Distro::Fedora => "/etc/bashrc",
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
        // Those two passwords are FIXED, PUBLIC (they are right here, in an
        // open-source repo) and the account has passwordless sudo — so anyone
        // who can reach a login prompt on a golden VM is root on it. Password
        // login over SSH is therefore turned OFF in the image: every supported
        // way in gets you in without it (cloud-init injects the SSH keys, and
        // `cluster kubeadm` authenticates with a generated key), while the
        // serial console still takes the password, which is what you want when
        // a VM has lost its network and you need to get in and fix it.
        //
        // Both places, deliberately: `sshd_config.d` is the modern drop-in, but
        // Debian bullseye's stock `sshd_config` has no `Include` line, so there
        // the drop-in alone would be silently ignored. In the main file the
        // existing directives are commented out first rather than appended to —
        // sshd takes the FIRST occurrence of a keyword, so an append lands after
        // the distro's own line and does nothing.
        CustomizeOp::RunCommand(
            "mkdir -p /etc/ssh/sshd_config.d && \
             printf 'PasswordAuthentication no\\nPermitRootLogin prohibit-password\\nKbdInteractiveAuthentication no\\n' \
               > /etc/ssh/sshd_config.d/99-delonix-hardening.conf && \
             chmod 644 /etc/ssh/sshd_config.d/99-delonix-hardening.conf && \
             if [ -f /etc/ssh/sshd_config ]; then \
               sed -i -E 's/^[[:space:]]*(PasswordAuthentication|PermitRootLogin|KbdInteractiveAuthentication|ChallengeResponseAuthentication)[[:space:]]/#&/I' /etc/ssh/sshd_config && \
               printf '\\n# --- Delonix golden image hardening (the built-in password is public) ---\\nPasswordAuthentication no\\nPermitRootLogin prohibit-password\\nKbdInteractiveAuthentication no\\n' \
                 >> /etc/ssh/sshd_config; \
             fi"
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
        Distro::Rocky | Distro::Fedora => "dnf clean all",
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
    //
    // `mkdir -p /var/lib/dbus` FOUND LIVE building Fedora: that directory does
    // not exist there (dbus reads /etc/machine-id directly), so `ln -sf` failed
    // and took the whole `virt-customize` run with it — the same class of trap
    // as the AppArmor step that had to be made Ubuntu-only for Rocky. The
    // symlink is still created where the directory does exist, because that is
    // the case the compat matters for.
    ops.push(CustomizeOp::RunCommand(
        "truncate -s 0 /etc/machine-id && rm -f /var/lib/dbus/machine-id && \
         mkdir -p /var/lib/dbus && ln -sf /etc/machine-id /var/lib/dbus/machine-id"
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
        // Fedora is the same dnf/RPM family as Rocky and uses the same package
        // names — the four that differ from Debian's (`shadow-utils`, `iproute`,
        // `conntrack-tools`, and the shared `nftables`/`slirp4netns`) are named
        // identically in Fedora's repos.
        Distro::Rocky | Distro::Fedora => {
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

/// The package that ships `bin`, for the two families this engine builds for.
///
/// Split out and pure because the message it feeds is the whole value: an
/// absent tool surfaces from `Command::status()` as `ENOENT`, which renders as
/// "No such file or directory" — a sentence that sends the reader looking for
/// a missing *file*. Measured on a host without libguestfs, that is exactly
/// what `vm build` printed after a 600 MB download had already succeeded.
pub(crate) fn tool_package(bin: &str) -> Option<(&'static str, &'static str)> {
    match bin {
        "virt-customize" | "virt-sparsify" | "virt-copy-out" => {
            Some(("libguestfs-tools", "guestfs-tools"))
        }
        "qemu-img" => Some(("qemu-utils", "qemu-img")),
        "cloud-localds" => Some(("cloud-image-utils", "cloud-utils")),
        "virsh" => Some(("libvirt-clients", "libvirt-client")),
        _ => None,
    }
}

pub(crate) fn run_tool(bin: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(bin).args(args).status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            if let Some((deb, rpm)) = tool_package(bin) {
                return Error::Invalid(super::po::tf(
                    "`{bin}` is not installed. Install it with `sudo apt install {deb}` \
                     (Debian/Ubuntu) or `sudo dnf install {rpm}` (Fedora/Rocky).",
                    &[("bin", bin), ("deb", deb), ("rpm", rpm)],
                ));
            }
            return Error::Invalid(super::po::tf(
                "`{bin}` is not installed, and it is needed here.",
                &[("bin", bin)],
            ));
        }
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

    /// Achado de auditoria (MÉDIO): a golden traz `root/delonix` e
    /// `delonix:delonix` com sudo NOPASSWD — credenciais FIXAS e públicas (estão
    /// no código-fonte aberto). Sem desligar o login por password no SSH, uma
    /// golden exposta à rede é root remoto com credenciais conhecidas. Todos os
    /// caminhos suportados entram por chave (cloud-init injecta-a, o
    /// `cluster kubeadm` gera a sua), por isso desligar não custa nada; a
    /// consola série continua a aceitar a password para recuperação.
    #[test]
    fn todas_as_goldens_desligam_o_login_por_password_no_ssh() {
        let hardened = |ops: &[CustomizeOp]| {
            ops.iter().any(
                |o| matches!(o, CustomizeOp::RunCommand(c) if c.contains("PasswordAuthentication no")),
            )
        };
        for d in [Distro::Ubuntu, Distro::Debian, Distro::Rocky] {
            let ops = rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), d);
            assert!(
                hardened(&ops),
                "a golden rootless ({d:?}) tem de desligar o login por password"
            );
            // Tem de mexer nos DOIS sítios: o `sshd_config` do bullseye não tem
            // linha `Include`, logo só o drop-in seria ignorado em silêncio.
            let cmds: Vec<&String> = ops
                .iter()
                .filter_map(|o| match o {
                    CustomizeOp::RunCommand(c) => Some(c),
                    _ => None,
                })
                .collect();
            assert!(cmds
                .iter()
                .any(|c| c.contains("sshd_config.d") && c.contains("/etc/ssh/sshd_config")));
        }
        // A golden k8s partilha os mesmos passos de conta, logo também vem
        // endurecida — é a que corre em nós de produção.
        let k8s = k8s_customization_steps(
            None,
            &[],
            &[],
            &PathBuf::from("/tmp/delonix-cri"),
            &PathBuf::from("/tmp/delonix-cri.service"),
            Distro::Ubuntu,
        );
        assert!(hardened(&k8s), "a golden k8s também tem de vir endurecida");
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
            "42-1.1",
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
            default_vcpus: None,
            default_memory: None,
            default_backend: None,
            cloud_init: None,
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
                "42-1.1",
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
            "42-1.1",
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

    /// An absent tool is not a missing file, and the message has to say so.
    ///
    /// Measured on a host without libguestfs: `vm build` downloaded 600 MB,
    /// verified the checksum, resized the disk, and then said
    /// `running virt-customize: No such file or directory` — which reads as a
    /// broken path, not as «install this package». The mapping is the fix, so
    /// it is the mapping that gets the test.
    #[test]
    fn ferramenta_ausente_nomeia_o_pacote_das_duas_familias() {
        assert_eq!(
            super::tool_package("virt-customize"),
            Some(("libguestfs-tools", "guestfs-tools"))
        );
        assert_eq!(
            super::tool_package("qemu-img"),
            Some(("qemu-utils", "qemu-img"))
        );
        // Unknown tools still get a sentence, just without a package name —
        // never a silent fallthrough to the ENOENT text.
        assert_eq!(super::tool_package("whatever"), None);
    }

    /// Builds a `VmImage` with every field empty — the shape a `vm pull`
    /// produces before annotations are applied.
    fn bare_img() -> VmImage {
        VmImage {
            name: "x".to_string(),
            tag: "x".to_string(),
            digest: "sha256:x".to_string(),
            size: 0,
            ubuntu_release: None,
            k8s_version: None,
            created_unix: 0,
            kernel_version: None,
            distro: None,
            default_vcpus: None,
            default_memory: None,
            default_backend: None,
            cloud_init: None,
        }
    }

    #[test]
    fn metadados_antigos_sem_o_campo_continuam_a_carregar_e_contam_como_cloud_init() {
        // Every `.json` written before `cloud_init` existed. Loading these has
        // to keep working, AND they have to behave exactly as before — those
        // images are cloud images, and `vm create` must still seed them.
        let antigo = r#"{
            "name": "golden", "tag": "golden", "digest": "sha256:a", "size": 1,
            "ubuntu_release": "24.04", "k8s_version": "1.34", "created_unix": 0
        }"#;
        let img: VmImage = serde_json::from_str(antigo).unwrap();
        assert_eq!(img.cloud_init, None);
        assert!(
            img.uses_cloud_init(),
            "unknown tem de contar como cloud-init: e o que estas imagens sempre foram"
        );
    }

    #[test]
    fn so_uma_marca_explicita_de_appliance_desliga_o_seed() {
        let mut img = bare_img();
        assert!(img.uses_cloud_init());
        img.cloud_init = Some(true);
        assert!(img.uses_cloud_init());
        img.cloud_init = Some(false);
        assert!(!img.uses_cloud_init());
    }

    #[test]
    fn annotations_fazem_round_trip_pelo_push_e_pull() {
        // The point of the annotations: what the store knows must survive a
        // push/pull, because the qcow2 blob carries none of it.
        let mut orig = bare_img();
        orig.distro = Some("opnsense".to_string());
        orig.ubuntu_release = Some("26.1.2".to_string());
        orig.default_vcpus = Some(2);
        orig.default_memory = Some("2G".to_string());
        orig.default_backend = Some("libvirt".to_string());
        orig.cloud_init = Some(false);

        let recuperado = apply_pulled_annotations(bare_img(), &annotations_of(&orig));
        assert_eq!(recuperado.distro, orig.distro);
        assert_eq!(recuperado.ubuntu_release, orig.ubuntu_release);
        assert_eq!(recuperado.default_vcpus, orig.default_vcpus);
        assert_eq!(recuperado.default_memory, orig.default_memory);
        assert_eq!(recuperado.default_backend, orig.default_backend);
        assert_eq!(recuperado.cloud_init, Some(false));
        assert!(!recuperado.uses_cloud_init());
    }

    #[test]
    fn artefacto_sem_annotations_nao_muda_nada() {
        // Everything published before this existed, and anything pushed by
        // another tool: the pull must land exactly where it landed before.
        let vazio = std::collections::BTreeMap::new();
        let img = apply_pulled_annotations(bare_img(), &vazio);
        assert_eq!(img.cloud_init, None);
        assert_eq!(img.distro, None);
        assert!(img.uses_cloud_init());
    }

    #[test]
    fn annotation_malformada_e_ignorada_menos_no_campo_que_decide_o_seed() {
        let mut a = std::collections::BTreeMap::new();
        a.insert(
            "io.delonix.vmimage.default-vcpus".to_string(),
            "muitos".to_string(),
        );
        // Anything that is not exactly "true"/"false" leaves cloud-init
        // unknown — which falls back to "yes, seed it", the safe reading for
        // an image we cannot classify.
        a.insert("io.delonix.vmimage.cloud-init".to_string(), "0".to_string());
        let img = apply_pulled_annotations(bare_img(), &a);
        assert_eq!(img.default_vcpus, None);
        assert_eq!(img.cloud_init, None);
        assert!(img.uses_cloud_init());
    }

    #[test]
    fn annotations_so_carregam_o_que_e_sabido() {
        // An absent annotation and one saying "unknown" are different claims;
        // only the first is honest for a field nobody filled in.
        let a = annotations_of(&bare_img());
        assert!(a.is_empty(), "nada sabido, nada anunciado: {a:?}");
    }

    #[test]
    fn valid_fedora_release_exige_release_e_build() {
        // Fedora's artifact name carries a build number the release does not
        // determine, and its redirector has no listing to look it up in — so a
        // bare `42` is rejected here rather than 404-ing several hundred
        // megabytes into the download.
        assert!(valid_fedora_release("42-1.1").is_ok());
        assert!(valid_fedora_release("41-1.4").is_ok());
        assert!(valid_fedora_release("43-1.10.2").is_ok());
        for bad in [
            "42", "42-", "-1.1", "abc-1.1", "42-1.1a", "42-.1", "42-1.", "",
        ] {
            let e = valid_fedora_release(bad)
                .expect_err("devia recusar")
                .to_string();
            assert!(e.contains("<release>-<build>"), "{bad}: {e}");
        }
    }

    #[test]
    fn fedora_partilha_as_convencoes_rpm_do_rocky() {
        // Same dnf/RPM family: `wheel` (not `sudo`), `/etc/bashrc` (not
        // `/etc/bash.bashrc`), `dnf clean all`. Verified against the Rocky
        // steps rather than restated, so the two cannot drift apart.
        let bin = PathBuf::from("/tmp/delonix");
        let fedora = rootless_customization_steps(&[], &bin, Distro::Fedora);
        let rocky = rootless_customization_steps(&[], &bin, Distro::Rocky);
        let cmds = |ops: &[CustomizeOp]| -> Vec<String> {
            ops.iter()
                .filter_map(|o| match o {
                    CustomizeOp::RunCommand(c) => Some(c.clone()),
                    _ => None,
                })
                .collect()
        };
        let (f, r) = (cmds(&fedora), cmds(&rocky));
        assert_eq!(f, r, "Fedora e Rocky partilham os passos da familia RPM");
        assert!(f.iter().any(|c| c.contains("dnf install -y")));
        assert!(f.iter().any(|c| c.contains("wheel")));
        assert!(f.iter().any(|c| c.contains("dnf clean all")));
    }

    #[test]
    fn fedora_nunca_escreve_perfil_apparmor() {
        // AppArmor is an Ubuntu-only workaround (the
        // `kernel.apparmor_restrict_unprivileged_userns` patch); Fedora uses
        // SELinux and has no /etc/apparmor.d, so writing there would fail the
        // whole `virt-customize` run — the same trap already fixed for Rocky.
        let ops = rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Fedora);
        assert!(!ops.iter().any(|o| matches!(
            o,
            CustomizeOp::RunCommand(c) if c.contains("apparmor")
        )));
    }

    #[test]
    fn fedora_sem_no_k8s_e_rejeitado() {
        let (store, dir) = tmp_store();
        let err = cmd_build(
            &store,
            "t",
            Distro::Fedora,
            "24.04",
            "bookworm",
            "9",
            "42-1.1",
            None,
            vec![],
            vec![],
            None,
            true,
            false,
            false,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("fedora"),
            "a recusa tem de nomear a distro: {err}"
        );
        assert!(err.contains("--no-k8s"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn image_type_label_distingue_appliance_de_cloud_init_e_de_desconhecido() {
        // The three states are genuinely different and the column must not
        // collapse them: an appliance REFUSES --ssh-key, a cloud-init image
        // accepts it, and an unknown one (pre-existing metadata, or a tag
        // published before annotations existed) is treated as cloud-init but
        // should not claim to be one.
        assert_eq!(image_type_label(Some("false")), "appliance");
        assert_eq!(image_type_label(Some("true")), "cloud-init");
        assert_eq!(image_type_label(None), "-");
        assert_eq!(image_type_label(Some("sim")), "-");
    }

    #[test]
    fn defaults_label_mostra_so_o_que_a_imagem_recomenda() {
        let mut img = bare_img();
        assert_eq!(defaults_label(&img), "-");
        img.default_vcpus = Some(4);
        assert_eq!(defaults_label(&img), "4cpu");
        img.default_memory = Some("8G".into());
        assert_eq!(defaults_label(&img), "4cpu/8G");
        img.default_vcpus = None;
        assert_eq!(defaults_label(&img), "8G");
    }

    #[test]
    fn o_reset_do_machine_id_cria_o_dir_do_dbus_antes_de_lhe_apontar() {
        // Found live on Fedora: /var/lib/dbus does not exist there, so the
        // `ln -sf` failed and virt-customize aborted the whole build. Every
        // distro must get a command that cannot fail on a missing directory.
        for d in [
            Distro::Ubuntu,
            Distro::Debian,
            Distro::Rocky,
            Distro::Fedora,
        ] {
            let ops = shared_account_steps(&[], d);
            let mid = ops
                .iter()
                .filter_map(|o| match o {
                    CustomizeOp::RunCommand(c) if c.contains("machine-id") => Some(c),
                    _ => None,
                })
                .next_back()
                .expect("o reset de machine-id tem de existir");
            assert!(
                mid.contains("mkdir -p /var/lib/dbus"),
                "{d:?}: sem mkdir, o ln falha onde o dir nao existe: {mid}"
            );
        }
    }

    /// **`vhd` é `vpc` para o qemu-img, e `.vhd` para quem importa.** É a única
    /// combinação em que o nome do formato e a extensão divergem, e é
    /// exactamente o detalhe que um utilizador não devia ter de saber para
    /// obter um ficheiro que o Hyper-V aceita — daí serem duas funções e não
    /// uma string usada duas vezes.
    #[test]
    fn o_formato_do_qemu_e_a_extensao_do_ecossistema_podem_divergir() {
        use super::ConvertFormat as F;
        for (f, qemu, ext) in [
            (F::Qcow2, "qcow2", "qcow2"),
            (F::Raw, "raw", "raw"),
            (F::Vmdk, "vmdk", "vmdk"),
            (F::Vdi, "vdi", "vdi"),
            (F::Vhdx, "vhdx", "vhdx"),
            (F::Vhd, "vpc", "vhd"),
        ] {
            assert_eq!(f.as_str(), qemu, "{f:?}");
            assert_eq!(f.extension(), ext, "{f:?}");
        }
    }

    /// Só o qcow2 e o vmdk aceitam o `-c` do qemu-img; passá-lo aos outros faz
    /// a ferramenta falhar, e o utilizador levaria um erro de tool numa flag
    /// que lhe foi oferecida.
    #[test]
    fn so_dois_formatos_aceitam_compressao() {
        use super::ConvertFormat as F;
        assert!(F::Qcow2.supports_compression() && F::Vmdk.supports_compression());
        for f in [F::Raw, F::Vdi, F::Vhdx, F::Vhd] {
            assert!(!f.supports_compression(), "{f:?}");
        }
    }

    #[test]
    fn o_cabecalho_do_ls_remote_tira_a_tag_mas_nao_a_porta() {
        // The default source carries a tag; showing it would suggest the
        // listing is scoped to it. A `host:port/repo` must keep its port —
        // hence cutting only a colon that comes after the last slash.
        let show = |s: &str| -> String {
            match s.rfind('/') {
                Some(slash) => match s[slash..].find(':') {
                    Some(colon) => s[..slash + colon].to_string(),
                    None => s.to_string(),
                },
                None => s.split(':').next().unwrap_or(s).to_string(),
            }
        };
        assert_eq!(
            show("ghcr.io/ang/delonix-vm-k8s:1.34"),
            "ghcr.io/ang/delonix-vm-k8s"
        );
        assert_eq!(
            show("ghcr.io/ang/delonix-vm-base"),
            "ghcr.io/ang/delonix-vm-base"
        );
        assert_eq!(show("localhost:5000/foo:v1"), "localhost:5000/foo");
        assert_eq!(show("localhost:5000/foo"), "localhost:5000/foo");
        assert_eq!(show("alpine:latest"), "alpine");
    }

    #[test]
    fn o_repo_oficial_sai_do_que_a_imagem_diz_de_si() {
        let mut img = bare_img();
        // Nothing known: must NOT guess a destination.
        assert!(official_repo_for(&img).is_none());
        img.distro = Some("ubuntu".into());
        assert_eq!(official_repo_for(&img).unwrap().key, "base");
        img.k8s_version = Some("1.34".into());
        assert_eq!(official_repo_for(&img).unwrap().key, "k8s");
        // An appliance wins over everything: it is the one that changes how
        // `vm create` behaves.
        img.cloud_init = Some(false);
        assert_eq!(official_repo_for(&img).unwrap().key, "appliances");
    }

    #[test]
    fn resolve_official_ref_so_toca_no_que_nao_tem_registo() {
        // A real reference is left alone — the argument keeps meaning
        // "a repository of your own".
        for as_is in [
            "ghcr.io/outro/coisa:1.0",
            "docker.io/library/alpine:latest",
            "localhost:5000/x:v1",
        ] {
            assert_eq!(resolve_official_ref_local(as_is).as_deref(), Some(as_is));
        }
        // Repository keys.
        assert!(resolve_official_ref_local("appliances")
            .unwrap()
            .ends_with("delonix-vm-appliances"));
        assert_eq!(
            resolve_official_ref_local("k8s:1.35").as_deref(),
            Some("ghcr.io/angolardevops/delonix-vm-k8s:1.35")
        );
    }

    #[test]
    fn um_tag_nu_nao_e_adivinhado_para_um_repositorio() {
        // The bug this replaced: EVERY bare tag was assumed to live in the
        // appliances repository, so `vm pull rocky-9` reported "no such image
        // …/delonix-vm-appliances:rocky-9" for an image that was published,
        // public, and in the base repository all along. `_local` now declines
        // to answer, and the caller goes and looks.
        for bare in ["rocky-9", "ubuntu-24.04", "opnsense:26.1", "proxmox-ve-9.1"] {
            assert_eq!(
                resolve_official_ref_local(bare),
                None,
                "{bare} must be discovered, not guessed"
            );
        }
        // The `<product>:<version>` spelling still names the published tag.
        assert_eq!(official_tag_candidate("opnsense:26.1"), "opnsense-26.1");
        assert_eq!(official_tag_candidate("rocky-9"), "rocky-9");
    }

    #[test]
    fn a_tag_publicada_e_o_inverso_do_que_o_pull_resolve() {
        // push and pull must agree without either knowing about the other.
        let mut img = bare_img();
        img.cloud_init = Some(false);
        img.distro = Some("opnsense".into());
        let tag = official_tag_for(&img, "opnsense:26.1");
        assert_eq!(tag, "opnsense-26.1");
        // The TAG is what the two sides have to agree on without either knowing
        // about the other: push derives it from the image's metadata, pull from
        // the string the user typed. (The repository no longer has to match by
        // construction — pull finds it by looking, which is why a base tag no
        // longer lands in the appliances repo.)
        assert_eq!(official_tag_candidate("opnsense:26.1"), tag);
        assert_eq!(
            official_repo_for(&img).unwrap().key,
            "appliances",
            "an appliance is published to the appliances repository"
        );
        // A base image does not repeat the family in the tag.
        assert_eq!(
            official_tag_for(&bare_img(), "delonix-vm-base:ubuntu-24.04"),
            "ubuntu-24.04"
        );
    }
}
